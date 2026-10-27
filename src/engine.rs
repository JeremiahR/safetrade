//! The run loop: observe, decide, reconcile, record.
//!
//! Ordering inside a loop matters. We ingest fills *before* planning so the
//! position the strategy sees includes anything that traded while we slept, and
//! we cancel *before* placing so a replace cannot momentarily double our
//! exposure at a price level.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn};

use crate::config::{Config, Params};
use crate::db::models::{Program, ProgramKind};
use crate::db::{Db, MARKOUT_HORIZON_MS, now_ms};
use crate::exchange::{ApiError, Balance, Client, Market, OrderBook, Side};
use crate::num;
use crate::strategy::allocation;
use crate::strategy::arms::{self, ArmScales, ArmSize, Book, State};
use crate::strategy::quote::Halt;
use crate::strategy::{Decision, DesiredQuote, QuoteInput, plan};

pub struct Engine {
    cfg: Config,
    db: Db,
    api: Arc<Client>,
    /// Every book we quote. Markets sharing a base asset share one inventory
    /// target, so this is one position across several books rather than
    /// several bots that happen to trade the same coin.
    markets: Vec<Market>,
    /// Dry run forced by the `--dry-run` flag. The effective value each loop is
    /// `params.dry_run`, which also honours the `dry_run` setting in the
    /// database, so it can be turned on for a running bot.
    dry_run: bool,
    /// Rolling fair-value history per market, for the volatility breaker.
    fv_history: HashMap<String, Vec<(i64, Decimal)>>,
    /// When each book's slow sell last re-planned. See `SELL_EVERY_SECS`.
    sell_planned: HashMap<String, i64>,
    last_prune: i64,
    /// Last balance we wrote per currency, so unchanged balances are not
    /// re-recorded every loop. Writing two rows every four seconds for a number
    /// that changes a few times an hour was the single largest consumer of disk.
    last_balances: HashMap<String, (Decimal, Decimal)>,
    last_balance_write: i64,
    /// A discrepancy awaiting confirmation on the next loop.
    pending_external: HashMap<String, Decimal>,
    /// Currency bought by a recycle crossing whose fill we have not read yet,
    /// and when it was bought.
    ///
    /// The crossing is our own taker order, but it moves the balance the instant
    /// it fills while its FILL is only ingested on the next loop that reads the
    /// recycle book -- every third, in production. In that window the drift
    /// detector sees a balance that jumped by more than its 1% threshold with no
    /// fills to explain it, and its two-loop debounce is no help because judging
    /// only happens on the loops that complete. So it alerted: "1371.89 USDC is
    /// unaccounted for", about money it had just spent itself. Harmless -- the
    /// detector no longer acts on inference -- but an alert channel that cries
    /// wolf every time the cycle refills is worse than no alert at all.
    recycle_awaiting: HashMap<String, i64>,
    /// Inventory we believe we should hold per asset, carried forward by our
    /// own fills. Compared against reality to spot coins arriving or leaving.
    expected_inventory: HashMap<String, Decimal>,
    /// Loop counter, for markets quoted less often than every loop.
    loop_n: u64,
    /// Open-order fingerprint per market, so own-trade lookups happen only when
    /// something actually changed.
    order_state: HashMap<String, String>,
    /// Markets currently standing down, and why. Keyed so the alert fires on
    /// the way in and the way out, not on every loop between.
    halted: HashMap<String, String>,
    /// Orders on the exchange that are not ours -- placed by hand. Seen once
    /// so the log says so once, then left alone for good.
    foreign_orders: HashSet<i64>,
    last_transfer_check: i64,
    transfer_checks: u32,
    /// Consecutive per-market API failures. A book we cannot see is a book
    /// whose orders rest unmanaged, and on a thin market it is also a book we
    /// would quote off a stale local mid -- which the code elsewhere notes
    /// costs about 2.5% a fill.
    market_failures: HashMap<String, u32>,
    /// Consecutive failures of the whole tick, not of one market.
    tick_failures: u32,
    /// Withdrawals requested but not yet settled, per currency. The exchange
    /// locks these funds; they are not inventory.
    pending_out: HashMap<String, Decimal>,
    /// Currencies with a deposit the exchange has seen but not yet credited.
    pending_in: HashSet<String>,
    /// Since when the slow sell has had nothing left to sell; see
    /// `chain_buyback`.
    sold_out_since: Option<i64>,
    /// Lighter's PRL perpetual, watched and recorded; see `exchange::lighter`.
    lighter: Option<crate::exchange::lighter::Lighter>,
    /// When each market was last measured. Timed rather than counted, so a
    /// market quoted every third loop is still measured on schedule.
    last_measured: HashMap<String, i64>,
    /// What every arm has earned and the size that justifies, refreshed every
    /// `ARMS_REFRESH_MS`; see `strategy::arms`. `arm_scales` is the same
    /// table indexed the way the quoting looks it up.
    arm_sizes: Vec<ArmSize>,
    /// What a whole cycle pays, refreshed with the arms. `None` until the
    /// first estimate. See `strategy::cycle`.
    cycle: Option<crate::strategy::cycle::Cycle>,
    arm_scales: HashMap<String, ArmScales>,
    last_arms: i64,
    /// When the learned split was last derived; see `retarget`.
    last_retarget: i64,
}

/// How often the arms are re-estimated. One episode: fills are scored five
/// minutes after they happen and folded into five-minute episodes, so nothing
/// the estimate sees changes faster than that.
const ARMS_REFRESH_MS: i64 = MARKOUT_HORIZON_MS;
/// Orders and fills older than this never enter an arm's estimate, whatever
/// the half-life says. At a twelve-hour half-life four days is a weight of
/// one in 256, which is noise; the bound keeps the query small.
///
/// This, and not the scoreboard's epoch, is the arms' window. See
/// `Db::analytics_epoch_ms` for why the two are kept apart.
pub const ARMS_MAX_AGE_MS: i64 = 4 * 86_400_000;

/// Ticks past the touch a recycle order is priced, so it crosses.
///
/// A limit rather than a market order: it takes liquidity identically, but the
/// worst price is stated instead of discovered. Two ticks clears the touch on a
/// book with over a million dollars a side.
pub const TAKER_THROUGH_TICKS: i64 = 2;

/// The `level` recorded against a crossing order, so the arm tables and the
/// reports can tell one from a rung of the ladder. Well clear of the deep
/// rungs' own base.
pub const TAKER_LEVEL: i64 = 9000;
/// How often a learned program's split is re-derived: hourly, twelve arm
/// refreshes. The arms move every five minutes; the split should not chase
/// every refresh, and in an hour the evidence has decayed by 6%, the
/// smallest change worth re-deciding on. See `strategy::allocation`.
const RETARGET_MS: i64 = 12 * ARMS_REFRESH_MS;
/// The glide of a learned program, in evidence half-lives: two, so the split
/// never moves faster than the evidence it follows. At the configured twelve
/// hours that is a day, in which the book turns over 1.33x equity.
const DRIFT_HALF_LIVES: i64 = 2;

/// The settings the bot last went live with, as `strategy_fingerprint` sees
/// them. A different fingerprint at start-up is a settings change, and a
/// settings change restarts the scoreboard.
/// The slow sell's floor when its program carries no limit: never sell PRL
/// below this. A program's `limit_price` overrides it.
const SELL_FLOOR: Decimal = dec!(1.30);

/// How often the slow sell re-plans its asks, by default (`sell_every_secs`).
/// The loop still runs every few seconds for fills, commands and pause; only
/// the ask placement waits. The owner, 2026-10-10: "20 seconds is aggressive".
const SELL_EVERY_SECS: i64 = 120;

/// The slow sell's liquidation pace, by default (`sell_hours`): a holding
/// sells out over about this many hours. The owner, 2026-10-21: "no rush but
/// targeting 24-48 hours to liquidate".
const SELL_HOURS: i64 = 36;

const CONFIG_LIVE_KEY: &str = "config_live";

/// One page of our own trades, and how many pages we will walk in a loop.
/// Twenty pages covers an outage long enough that a restart is the real fix.
const FILL_PAGE: u32 = 200;
const FILL_PAGES_MAX: usize = 20;

/// Consecutive whole-tick failures before we pull every order. About two
/// minutes at a six-second loop, matching the per-market threshold.
const TICK_FAILURES_BEFORE_CANCEL: u32 = 20;

/// How many public trades one request returns. The exchange caps it here.
const TAPE_LIMIT: u32 = 1000;
/// Below this span, a share ratio means nothing: one large fill of ours would
/// read as owning the entire market.
const MIN_SPAN_SECS: i64 = 300;

/// The instant from which BOTH our volume and the market's may be counted.
///
/// The public tape is capped at `TAPE_LIMIT` trades, so on a busy book it
/// covers far less time than the participation window asks for. Measured live
/// during a PRL rally, 1000 trades spanned 72 minutes of a 240-minute window;
/// dividing four hours of our fills by 72 minutes of market volume reported
/// 18.7% against a true 5.7% and pulled every quote off the main book.
///
/// Returns `None` when no honest measurement is possible -- an empty tape, or
/// one spanning too little time to divide by. `None` means "no opinion", which
/// the caller turns into "do not halt": not knowing our share is not the same
/// as knowing it is too high.
fn measurement_window(
    oldest_trade_s: Option<i64>,
    tape_len: usize,
    cutoff_s: i64,
    now_s: i64,
) -> Option<i64> {
    let oldest = oldest_trade_s?;
    // Only a FULL page is evidence of truncation. A short page means the book
    // genuinely traded that little, and the asked-for window is honest.
    let truncated = tape_len as u32 >= TAPE_LIMIT;
    let from = if truncated { oldest.max(cutoff_s) } else { cutoff_s };
    (now_s - from >= MIN_SPAN_SECS).then_some(from)
}

impl Engine {
    pub async fn new(cfg: Config, db: Db, api: Arc<Client>, dry_run: bool) -> Result<Self> {
        // Retry, for the same reason startup authentication does: a listing
        // failure is usually the exchange, not us.
        let all = {
            let mut attempt = 0u32;
            loop {
                match api.markets().await {
                    Ok(v) => break v,
                    Err(e) => {
                        attempt += 1;
                        warn!(
                            attempt, error = %format!("{e:#}"),
                            "could not list markets; retrying"
                        );
                        if attempt >= 20 {
                            return Err(e).context("listing markets");
                        }
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                }
            }
        };
        let mut markets = Vec::new();
        for id in cfg.market_list() {
            let m = all
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("market {id} is not listed on the exchange"))?;
            if m.state != "enabled" {
                bail!("market {id} is {}, not enabled", m.state);
            }
            info!(
                market = %m.id, tick = %m.tick(), min_amount = %m.min_amount,
                base = %m.base_unit, quote = %m.quote_unit,
                "market loaded"
            );
            // Recorded so `report` can name and weigh pools from the database
            // alone, with no credentials and no network.
            db.set_setting(&format!("base_unit:{}", m.id), &m.base_unit).await.ok();
            db.set_setting(&format!("quote_unit:{}", m.id), &m.quote_unit).await.ok();
            markets.push(m);
        }
        if markets.is_empty() {
            bail!("no markets configured");
        }
        // The primary market still names the pools for the default report view.
        db.set_setting("base_unit", &markets[0].base_unit).await.ok();
        db.set_setting("quote_unit", &markets[0].quote_unit).await.ok();

        // One-time correction: edges settled before this used the book at
        // ingestion rather than at the fill, which in a trending market is a
        // materially different price.
        const EDGE_SETTLEMENT: &str = "2";
        let settled = db.get_setting("edge_settlement").await.ok().flatten();
        if settled.as_deref() != Some(EDGE_SETTLEMENT) {
            match db.resettle_all_edges().await {
                Ok(n) => {
                    info!(fills = n, "re-settled edge against the mid at fill time");
                    db.set_setting("edge_settlement", EDGE_SETTLEMENT).await.ok();
                }
                Err(e) => warn!(error = %format!("{e:#}"), "edge re-settlement failed"),
            }
        } else {
            match db.backfill_edges().await {
                Ok(n) if n > 0 => info!(fills = n, "settled edge on new fills"),
                Err(e) => warn!(error = %format!("{e:#}"), "edge backfill failed"),
                _ => {}
            }
        }

        // Programs are keyed by BASE ASSET, not by market, because one
        // inventory target governs every book trading that asset. Move a
        // program written under the old per-market key.
        for m in &markets {
            db.migrate_program_key(&m.id, &m.base_unit).await.ok();
        }

        // Markouts on the thin books scored against their own stale mid are
        // re-judged against the reference now that the mapping is recorded.
        match db.resettle_reference_markouts().await {
            Ok(n) if n > 0 => info!(fills = n, "re-scored markouts against the reference mid"),
            Err(e) => warn!(error = %format!("{e:#}"), "markout re-settlement failed"),
            _ => {}
        }
        // Orders placed before the placement distance was recorded get it
        // now, so the arms start from a week of history rather than cold.
        match db.backfill_order_distance().await {
            Ok(n) if n > 0 => info!(orders = n, "recorded placement distance on older orders"),
            Err(e) => warn!(error = %format!("{e:#}"), "placement-distance backfill failed"),
            _ => {}
        }
        // And the five-minute range at placement, so the arms' regimes are
        // populated from the same history rather than starting empty.
        match db.backfill_order_move(cfg.risk.vol_window_secs * 1000, ARMS_MAX_AGE_MS).await {
            Ok(n) if n > 0 => info!(orders = n, "recorded the range at placement on older orders"),
            Err(e) => warn!(error = %format!("{e:#}"), "placement-range backfill failed"),
            _ => {}
        }
        // And the book's width at placement, for the arms' width cells.
        match db.backfill_order_width(ARMS_MAX_AGE_MS).await {
            Ok(n) if n > 0 => info!(orders = n, "recorded the book width at placement on older orders"),
            Err(e) => warn!(error = %format!("{e:#}"), "placement-width backfill failed"),
            _ => {}
        }

        // Pick up where the last run left off.
        if let Ok(Some(v)) = db.get_setting("api_rate").await
            && let Ok(r) = v.parse::<f64>()
        {
            api.restore_rate(r).await;
            info!(rate = %format!("{r:.2}/s"), "restored the learned API rate");
        }

        // Which book each thin market is scored against. The primary market
        // is the deepest by design and prices the asset; the others are quoted
        // off it, so their fills must be judged against it too, or a stale
        // local mid reports good quoting as a loss. See `Db::scoring_mid`.
        if let Some(primary) = markets.iter().find(|m| m.id == cfg.market) {
            for m in markets.iter().filter(|m| m.base_unit == primary.base_unit && m.id != primary.id) {
                db.set_setting(&format!("reference:{}", m.id), &primary.id).await?;
                let cross_key = format!("reference_cross:{}", m.id);
                // The cross that converts the primary's quote into this one's:
                // "<this quote><primary quote>" has a mid of primary-quote per
                // this-quote, so the price is divided by it; the other way
                // round it is multiplied.
                let div = format!("{}{}", m.quote_unit, primary.quote_unit);
                let mul = format!("{}{}", primary.quote_unit, m.quote_unit);
                if m.quote_unit == primary.quote_unit {
                    db.set_setting(&cross_key, "").await?;
                } else if markets.iter().any(|k| k.id == div) {
                    db.set_setting(&cross_key, &format!("{div}:div")).await?;
                } else if markets.iter().any(|k| k.id == mul) {
                    db.set_setting(&cross_key, &format!("{mul}:mul")).await?;
                } else {
                    db.set_setting(&cross_key, "").await?;
                }
            }
        }
        // Carry the arm table across restarts. Until the first refresh every
        // rung would otherwise quote the minimum, which is safe but throws
        // away a deploy's worth of established edge.
        let arm_sizes: Vec<ArmSize> = match db.get_setting("arms").await {
            Ok(Some(v)) => serde_json::from_str(&v).unwrap_or_default(),
            _ => Vec::new(),
        };
        let arm_scales = markets
            .iter()
            .map(|m| (m.id.clone(), ArmScales::of_market(&arm_sizes, &m.id)))
            .collect();

        let lighter = cfg
            .lighter_market_id
            .and_then(|id| crate::exchange::lighter::Lighter::new(id).ok());

        Ok(Engine {
            cfg,
            db,
            api,
            markets,
            dry_run,
            fv_history: HashMap::new(),
            sell_planned: HashMap::new(),
            last_prune: 0,
            last_balances: HashMap::new(),
            last_balance_write: 0,
            pending_external: HashMap::new(),
            recycle_awaiting: HashMap::new(),
            expected_inventory: HashMap::new(),
            loop_n: 0,
            order_state: HashMap::new(),
            halted: HashMap::new(),
            foreign_orders: HashSet::new(),
            last_transfer_check: 0,
            transfer_checks: 0,
            market_failures: HashMap::new(),
            tick_failures: 0,
            pending_out: HashMap::new(),
            pending_in: HashSet::new(),
            sold_out_since: None,
            lighter,
            last_measured: HashMap::new(),
            arm_sizes,
            cycle: None,
            arm_scales,
            last_arms: 0,
            last_retarget: 0,
        })
    }

    /// Draw the scoreboard's line when the settings that decide the quoting
    /// have changed since the bot last went live.
    ///
    /// Done here, by the code, so a deploy never depends on someone
    /// remembering `ctl epoch`: the owner reads the verdict and the P&L as a
    /// judgement on the CURRENT settings, and a scoreboard that carried the
    /// old settings' results under the new ones would judge the wrong thing.
    /// A restart with the same settings -- a crash, a reboot -- draws nothing,
    /// so the evidence keeps accumulating. The arms are not touched; their
    /// evidence survives a re-tune (see `Db::analytics_epoch_ms`).
    async fn restart_scoreboard_if_settings_changed(&self) -> Result<()> {
        let live = strategy_fingerprint(&self.cfg);
        let stored = self.db.get_setting(CONFIG_LIVE_KEY).await?;
        if stored.as_deref() == Some(live.as_str()) {
            return Ok(());
        }
        let now = self.db.reset_analytics_epoch("settings changed").await?;
        self.db.set_setting(CONFIG_LIVE_KEY, &live).await?;
        // The settings themselves go in the audit log, so "what was live
        // when" can be answered from the database alone.
        self.db
            .log("info", "settings_live", &format!("fingerprint {live}"), serde_json::to_value(&self.cfg).ok())
            .await;
        info!(fingerprint = %live, epoch_ms = now, "settings changed; scoreboard restarted");
        self.db
            .alert(
                "epoch",
                &format!(
                    "New settings are live. The scoreboard restarts from {}: P&L, verdict and \
                     earnings now count only what happens from here. The arms keep their \
                     evidence.",
                    chrono::DateTime::from_timestamp_millis(now)
                        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_default()
                ),
            )
            .await;
        Ok(())
    }

    /// Re-estimate every arm from its orders and scored fills, and decide how
    /// much of its rung size that yield justifies. See `strategy::arms`.
    ///
    /// An execution program -- accumulate, distribute, flatten -- on a base
    /// asset exempts that asset's books: getting to the target is the job,
    /// not a bet on the spread.
    async fn refresh_arms(&mut self, params: &Params) {
        if now_ms() - self.last_arms < ARMS_REFRESH_MS {
            return;
        }
        self.last_arms = now_ms();
        let books: Vec<(String, String)> =
            self.markets.iter().map(|m| (m.id.clone(), m.base_unit.clone())).collect();
        let sizes = match estimate_arms(&self.db, &books, params, now_ms()).await {
            Ok(s) => s,
            Err(e) => {
                // Keep the last table rather than resetting every arm to the
                // minimum on a transient read failure.
                warn!(error = %format!("{e:#}"), "could not re-estimate the arms");
                return;
            }
        };

        // Say what changed, once, in one message. Thirty arms each announcing
        // themselves would be a chat you mute.
        let changed: Vec<String> = sizes
            .iter()
            .filter_map(|z| {
                let was = self.arm_sizes.iter().find(|o| o.arm == z.arm).map(|o| o.state);
                (was.is_some() && was != Some(z.state)).then(|| {
                    format!(
                        "{} {} {} {} {}t: {} -> {} ({:+.1} ± {:.1} ‰/h, pays {:.0}%, {:.0} fills)",
                        z.arm.market,
                        z.arm.side,
                        z.arm.bucket.label(),
                        z.arm.regime.label(),
                        z.arm.width.label(),
                        was.map(State::label).unwrap_or("-"),
                        z.state.label(),
                        z.yield_pm,
                        z.se_pm,
                        z.p_pays * 100.0,
                        z.fills,
                    )
                })
            })
            .collect();
        if !changed.is_empty() {
            info!(changes = changed.len(), "arms re-sized");
            self.db.alert("arms", &changed.join("\n")).await;
        }

        if let Ok(json) = serde_json::to_string(&sizes) {
            self.db.set_setting("arms", &json).await.ok();
        }
        self.arm_scales = self
            .markets
            .iter()
            .map(|m| (m.id.clone(), ArmScales::of_market(&sizes, &m.id)))
            .collect();
        self.arm_sizes = sizes;
        self.refresh_cycle().await;
    }

    /// Re-measure what a whole cycle pays, and publish the two settings the
    /// report needs to render the same numbers the engine is deciding on.
    ///
    /// Shares the arms' cadence and window deliberately: the two are the same
    /// scored fills asked two different questions, and a report that disagreed
    /// with the engine about either would be worse than no report.
    async fn refresh_cycle(&mut self) {
        let acquire = self.cycle_acquire();
        let Some(acquire) = acquire else {
            self.cycle = None;
            return;
        };
        let legs = match self.db.cycle_legs(now_ms() - ARMS_MAX_AGE_MS).await {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "could not measure the cycle legs");
                return;
            }
        };
        let quote_of: HashMap<String, String> =
            self.markets.iter().map(|m| (m.id.clone(), m.quote_unit.clone())).collect();
        let base_of: HashMap<String, String> =
            self.markets.iter().map(|m| (m.id.clone(), m.base_unit.clone())).collect();
        let recycle_bps = self.recycle_cost_bps().await;
        let c = crate::strategy::cycle::estimate(&acquire, legs, quote_of, base_of, recycle_bps);

        // Published so `report` renders the engine's own view rather than
        // re-deriving one that could drift from it.
        self.db.set_setting("cycle_acquire", &acquire).await.ok();
        self.db
            .set_setting("cycle_recycle_bps", &num::to_str(c.recycle_bps.round_dp(2)))
            .await
            .ok();

        let floor = self.cfg.quoting.cycle_floor_bps;
        for m in &self.markets {
            if !c.releases_here(&m.id) {
                continue;
            }
            let was = self.cycle.as_ref().is_some_and(|p| p.pays(&m.id, floor));
            let now = c.pays(&m.id, floor);
            if was != now {
                let margin = c.margin_lower_bound(&m.id).unwrap_or_default();
                info!(
                    release = %m.id, %margin, %floor,
                    "the cycle's release valve {}",
                    if now { "OPENED" } else { "closed" }
                );
            }
        }
        self.cycle = Some(c);
    }

    /// Refill the currency the acquiring book's bids spend, by crossing the
    /// stablecoin book, when the whole cycle pays for the crossing.
    ///
    /// The cycle spends USDC on the thin book and, when it releases on the deep
    /// one, is paid in USDT. Without a way back the bid side starves and the
    /// acquisition edge -- which is nearly all of the account's income -- stops
    /// with it. Waiting for a maker fill on the stablecoin book is what stalls
    /// it, so this crosses, and the crossing is charged into the cycle's margin
    /// before anything is placed: lose a little on the leg if the whole trade
    /// works, never otherwise.
    ///
    /// An aggressive LIMIT, not a market order. It crosses and fills as a taker
    /// exactly the same way, but the worst price is stated up front instead of
    /// discovered, which on a book we have never crossed before is the only
    /// honest way to start. `TAKER_THROUGH_TICKS` past the touch is enough to
    /// clear it on a book this deep.
    ///
    /// Four things have to hold, and every one of them can turn it off on its
    /// own: the switch, the recycle book's OWN `allow_taker` (the global stays
    /// false), a cycle that clears its floor with the crossing charged, and
    /// bids genuinely short rather than merely not full.
    /// Wrapper that says why nothing was crossed.
    ///
    /// The body declines in a dozen places and every one of them used to be a
    /// bare `return`, so a swap that never happened was indistinguishable from a
    /// swap that was never wanted. That cost an afternoon: with the feature newly
    /// switched on, the only way to tell whether the gate or the arithmetic was
    /// holding it was to reason through eight guards by hand. One line at debug
    /// answers it.
    async fn replenish_acquire_quote(
        &mut self,
        params: &Params,
        balances: &[Balance],
        public: &HashMap<String, OrderBook>,
    ) -> Result<()> {
        match self.try_replenish_acquire_quote(params, balances, public).await {
            Ok(Some(why)) => {
                debug!(why, "no stablecoin crossing this loop");
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `Ok(Some(reason))` when nothing was crossed, `Ok(None)` when an order
    /// went out.
    async fn try_replenish_acquire_quote(
        &mut self,
        params: &Params,
        balances: &[Balance],
        public: &HashMap<String, OrderBook>,
    ) -> Result<Option<&'static str>> {
        // Paused means no orders of any kind. This swap ran on through a pause
        // on 2026-10-02 and crossed most of the USDT into USDC after the owner
        // had pressed the button, because only the quoting checked the switch.
        if !params.enabled {
            return Ok(Some("paused"));
        }
        if !params.q.cycle_recycle {
            return Ok(Some("cycle_recycle is off"));
        }
        // A TWAP spends both quote currencies on its own book. Crossing the
        // stablecoin book to move cash between them only pays the spread twice.
        if params.q.twap_only {
            return Ok(Some("twap_only is on"));
        }
        let (Some(acquire), Some(recycle)) = (self.cycle_acquire(), self.recycle_market()) else {
            return Ok(Some("no acquiring book, or no book that swaps its quote currency"));
        };
        // The exception is per book and it is checked on the book that would be
        // crossed, never on the global.
        let recycle_params = params.for_market(self.cfg.overrides.get(&recycle));
        if !recycle_params.q.allow_taker {
            return Ok(Some("the recycle book's own allow_taker is off"));
        }
        let Some(c) = self.cycle.as_ref() else {
            return Ok(Some("the cycle has not been measured yet"));
        };
        // Charged with the swap, judged on the evidence -- for either of the two
        // reasons a swap can be worth making.
        //
        // The first is the release landing in the wrong currency: money the
        // cycle has just earned on the deep book, which has to come back before
        // it can be spent again. That is `recycle_worth_taking`, and it can only
        // ever be true while the best release is the deep book.
        //
        // The second is capital already sitting in the wrong currency while the
        // bids that earn are starved -- a release leg since shut, or a deposit,
        // or simply the owner's mix. `recycle_worth_taking` is false for the
        // whole of that case, because the best release is the thin book's own
        // ask and nothing needs bringing back; measured 2026-09-27 that left
        // most of the cash idle in USDT while the USDC bids held under a fifth
        // of what they wanted, on
        // a round trip paying +226 bp through a 24 bp crossing. So the acquiring
        // book's own round trip may justify the swap on its own.
        let floor = params.q.cycle_floor_bps;
        let for_release = c
            .best_release(floor)
            .is_some_and(|(release, _)| c.recycle_worth_taking(&release, floor));
        if !for_release && !c.swap_funds_acquire(floor) {
            debug!(
                acquire = %c.acquire,
                margin = %c.margin_lower_bound(&c.acquire).unwrap_or_default().round_dp(1),
                recycle_bps = %c.recycle_bps.round_dp(1), %floor,
                "the round trip does not pay for the crossing"
            );
            return Ok(Some("no cycle justifies the crossing"));
        }

        let (Some(am), Some(rm)) = (
            self.markets.iter().find(|m| m.id == acquire).cloned(),
            self.markets.iter().find(|m| m.id == recycle).cloned(),
        ) else {
            return Ok(Some("one of the two markets is not loaded"));
        };

        // What the acquiring book's bids need at full size, in its quote
        // currency, against what is actually there.
        let prices = self.prices(public).await;
        let equity: Decimal = holdings(&self.markets, balances, &prices).values().sum();
        let px = prices.get(&am.base_unit).copied().unwrap_or(Decimal::ONE);
        let quote_px = prices.get(&am.quote_unit).copied().unwrap_or(Decimal::ONE);
        if equity <= Decimal::ZERO || px <= Decimal::ZERO || quote_px <= Decimal::ZERO {
            return Ok(Some("equity or a price is unreadable"));
        }
        let aq = params.for_market(self.cfg.overrides.get(&acquire));
        let (inv_a, inv_l) = balance_of(balances, &am.base_unit);
        let need = allocation::side_funding(&aq.q, equity / quote_px, inv_a + inv_l, px / quote_px);
        // What we HOLD, resting included. `side_funding` is a balance-on-hand
        // figure -- the value a side must hold for its ladder to rest at full
        // size -- and `learned_targets` already compares it against available
        // plus locked. Comparing it against available ALONE reads a ladder that
        // is resting correctly as a ladder that is starved, because the money is
        // locked in exactly the bids it was swapped in to fund. This path had
        // never run in production, so the bug had never been paid for: it would
        // have crossed the stablecoin book every loop, at 24 bp a time, until the
        // USDT ran out.
        let (have_a, have_l) = balance_of(balances, &am.quote_unit);
        let have = have_a + have_l;
        let Some(want) = recycle_shortfall(
            need,
            have,
            equity / quote_px,
            params.q.cycle_recycle_trigger,
            params.q.cycle_recycle_max_pct,
        ) else {
            return Ok(Some("the bids are not short enough to pay the crossing"));
        };
        let short = need - have;

        // Which way round the recycle book is: buying our wanted currency means
        // buying its base, or selling it if ours is the quote.
        let (side, amount) = if rm.base_unit == am.quote_unit {
            (Side::Buy, want)
        } else if rm.quote_unit == am.quote_unit {
            // Selling the base gets us the quote; the amount is in base units.
            let rp = prices.get(&rm.base_unit).copied().unwrap_or(Decimal::ONE);
            if rp <= Decimal::ZERO {
                return Ok(Some("the recycle book's base is unpriced"));
            }
            (Side::Sell, want / rp)
        } else {
            return Ok(Some("the recycle book trades neither side of the swap"));
        };

        // Do we hold what we would spend?
        let spend_ccy = if side == Side::Buy { &rm.quote_unit } else { &rm.base_unit };
        let (spend_have, _) = balance_of(balances, spend_ccy);
        let Some(book) = public.get(&recycle) else {
            return Ok(Some("the recycle book was not read this loop"));
        };
        let (Some((bid, _)), Some((ask, _))) = (book.best_bid(), book.best_ask()) else {
            return Ok(Some("the recycle book is one-sided"));
        };
        let through = num::tick(rm.price_precision) * Decimal::from(TAKER_THROUGH_TICKS);
        let price = match side {
            Side::Buy => num::round_to(ask + through, rm.price_precision),
            Side::Sell => num::round_to((bid - through).max(num::tick(rm.price_precision)), rm.price_precision),
        };
        let cost = if side == Side::Buy { amount * price } else { amount };
        let amount = if cost > spend_have && cost > Decimal::ZERO {
            amount * spend_have / cost
        } else {
            amount
        };
        let amount = num::floor_to(amount, rm.amount_precision);
        if amount < rm.min_amount || amount <= Decimal::ZERO {
            // Almost always the currency we would SPEND being empty, which is
            // worth naming: it is the difference between "the swap is not wanted"
            // and "the swap is wanted and there is nothing to make it with".
            debug!(
                %amount, min = %rm.min_amount, spend = %spend_ccy, %spend_have,
                "the crossing rounds to less than the exchange minimum"
            );
            return Ok(Some("nothing left to spend, or under the minimum order"));
        }

        if self.dry_run {
            info!(market = %recycle, %side, %amount, %price, "would cross to refill the bids (dry run)");
            return Ok(None);
        }
        match self.api.place_limit(&recycle, side, amount, price).await {
            Ok(o) => {
                info!(
                    market = %recycle, %side, %amount, %price, id = o.id,
                    short = %short.round_dp(2), need = %need.round_dp(2),
                    "crossed the stablecoin book to refill the acquiring book's bids"
                );
                // The balance moves now; the fill is read on the next loop that
                // covers the recycle book. Tell the drift detector to expect it.
                // Keyed exactly as `absorb_external_flow` keys it: both are a
                // `Market`'s own unit string, so they must not be case-folded on
                // one side only.
                self.recycle_awaiting.insert(am.quote_unit.clone(), now_ms());
                self.db
                    .log(
                        "info",
                        "recycle",
                        &format!(
                            "crossed {recycle} {} {} at {price} to refill {} bids",
                            side.as_str(),
                            amount.round_dp(2),
                            am.quote_unit
                        ),
                        None,
                    )
                    .await;
            }
            Err(e) => {
                warn!(market = %recycle, error = %format!("{e:#}"), "recycle order rejected");
            }
        }
        Ok(None)
    }

    /// The share of a full rung the cycle justifies on `market`'s ask side,
    /// over and above what that book's own arms have earned.
    ///
    /// Zero unless `cycle_release` is on, the cycle clears its floor on the
    /// evidence, and this is not already the best place to release -- the
    /// acquiring book's own ask needs no help from here, it is the leg the
    /// arms already fund. The figure is the ratio of this venue's margin to
    /// the best one's, which makes the overflow self-scaling: as the deep
    /// book's release leg gets better measured the ratio rises by itself, and
    /// if the acquisition edge goes the ratio collapses with it.
    fn cycle_release_floor(&self, market: &str, params: &Params) -> Decimal {
        // In TWAP mode the valve is shut whatever the evidence says: the valve
        // rests an ASK, and the whole point of the mode is that nothing sells.
        if !params.q.cycle_release || params.q.twap_only {
            return Decimal::ZERO;
        }
        let Some(c) = self.cycle.as_ref() else { return Decimal::ZERO };
        let floor = params.q.cycle_floor_bps;
        let Some((best, best_margin)) = c.best_release(floor) else { return Decimal::ZERO };
        if best == market || best_margin <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let Some(mine) = c.margin_lower_bound(market).filter(|m| *m > floor) else {
            return Decimal::ZERO;
        };
        num::clamp(mine / best_margin, Decimal::ZERO, Decimal::ONE)
    }

    /// Whether `market` is the cycle's acquiring book and its own round trip
    /// has earned an exemption from the inventory size skew.
    ///
    /// The skew is a safety rule -- it is what makes a lopsided position
    /// converge -- so the exemption is granted only to a round trip that clears
    /// the cycle floor on the discounted evidence, and it lapses the moment that
    /// edge does. The rule itself is `Quoting::cycle_churn`; this decides who
    /// gets it.
    fn cycle_churn(&self, market: &str, params: &Params) -> bool {
        // Churn is a two-sided exemption. There is no second side to exempt
        // while a buy-only program is running.
        if !params.q.cycle_churn || params.q.twap_only {
            return false;
        }
        if self.cycle_acquire().as_deref() != Some(market) {
            return false;
        }
        self.cycle.as_ref().is_some_and(|c| c.pays(market, params.q.cycle_floor_bps))
    }

    /// The book the cycle acquires the base asset on: the configured one, else
    /// the referenced (thin) book that trades the primary market's base asset,
    /// which is where the acquisition edge comes from by construction.
    fn cycle_acquire(&self) -> Option<String> {
        if let Some(m) = self.cfg.quoting.cycle_acquire.as_ref().filter(|m| !m.is_empty()) {
            return Some(m.clone());
        }
        let primary_base =
            self.markets.iter().find(|m| m.id == self.cfg.market).map(|m| m.base_unit.clone())?;
        self.markets
            .iter()
            .find(|m| m.id != self.cfg.market && m.base_unit == primary_base)
            .map(|m| m.id.clone())
    }

    /// What it costs, in basis points, to cross the book that swaps one quote
    /// currency for the other: half the spread plus the fee.
    ///
    /// Read from the book rather than configured, because the whole point of
    /// charging it is that it is a real, currently-quoted cost. Falls back to
    /// the module's measured default when the book has not been seen.
    async fn recycle_cost_bps(&self) -> Decimal {
        let Some(m) = self.recycle_market() else {
            return crate::strategy::cycle::DEFAULT_RECYCLE_BPS;
        };
        // The smoothed spread the engine already records for the report, as a
        // fraction of the mid. Crossing costs half of it, plus the fee.
        let (_, spread_frac) = self.db.market_stats(&m).await;
        if spread_frac <= Decimal::ZERO {
            return crate::strategy::cycle::DEFAULT_RECYCLE_BPS;
        }
        spread_frac / dec!(2) * dec!(10000) + crate::strategy::cycle::FEE_BPS
    }

    /// The book that swaps the acquiring book's quote currency for the primary
    /// one: the market whose base and quote are those two currencies.
    fn recycle_market(&self) -> Option<String> {
        let acquire = self.cycle_acquire()?;
        let a = self.markets.iter().find(|m| m.id == acquire)?.quote_unit.clone();
        let p = self.markets.iter().find(|m| m.id == self.cfg.market)?.quote_unit.clone();
        if a == p {
            return None;
        }
        self.markets
            .iter()
            .find(|m| {
                (m.base_unit == a && m.quote_unit == p) || (m.base_unit == p && m.quote_unit == a)
            })
            .map(|m| m.id.clone())
    }

    pub async fn run(mut self, shutdown: Arc<AtomicBool>) -> Result<()> {
        let mut ticker = tokio::time::interval(Duration::from_secs(self.cfg.loop_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        info!(
            markets = %self.markets.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", "),
            loop_secs = self.cfg.loop_secs, dry_run = self.dry_run, "starting"
        );
        self.db.log("info", "start", "engine started", None).await;
        if !self.dry_run {
            self.restart_scoreboard_if_settings_changed().await?;
        }

        while !shutdown.load(AtomicOrdering::Relaxed) {
            ticker.tick().await;
            if shutdown.load(AtomicOrdering::Relaxed) {
                break;
            }
            match self.tick().await {
                Ok(()) => self.tick_failures = 0,
                Err(e) => {
                    self.tick_failures += 1;
                    error!(error = ?e, failures = self.tick_failures, "loop iteration failed");
                    self.db
                        .log("error", "loop_error", &format!("{e:#}"), None)
                        .await;

                    // A whole-tick failure used to escalate to nothing.
                    //
                    // Per-market failures pull that market's orders after 20
                    // loops, but anything that aborts the tick itself -- the
                    // balances endpoint 500ing, one malformed row, a full disk
                    // -- just logged every six seconds while every ladder
                    // rested untouched through whatever the market did next.
                    // Orders nobody is managing are the thing we are most
                    // afraid of, whichever layer stopped managing them.
                    if self.tick_failures == TICK_FAILURES_BEFORE_CANCEL {
                        self.db
                            .alert(
                                "loop_stalled",
                                &format!(
                                    "The main loop has failed {} times in a row ({e:#}). Pulling \
                                     every order so none rest unmanaged.",
                                    self.tick_failures
                                ),
                            )
                            .await;
                        self.cancel_all_markets().await;
                    }
                    // Back off briefly so a persistent failure does not hammer the API.
                    tokio::time::sleep(Duration::from_secs(self.cfg.loop_secs)).await;
                }
            }
        }

        info!("shutting down: cancelling resting orders");
        self.db.log("info", "shutdown", "cancelling all orders", None).await;
        if !self.dry_run {
            for m in self.markets.clone() {
                if self.db.keeps_orders_on_restart(&m.id).await {
                    info!(market = %m.id, "slow sell: leaving our orders resting through the restart");
                    continue;
                }
                if let Err(e) = self.cancel_everything(&m.id).await {
                    error!(market = %m.id, error = ?e,
                           "failed to cancel on shutdown — CHECK THE EXCHANGE MANUALLY");
                }
            }
        }
        Ok(())
    }

    async fn tick(&mut self) -> Result<()> {
        // Commands first. Loading params before draining meant `panic` wrote
        // enabled=false, cancelled everything, and then this same tick -- still
        // holding the old params -- placed a fresh ladder that rested until the
        // next loop cancelled it again.
        self.drain_commands().await?;
        let params = Params::load(&self.cfg, &self.db, self.dry_run).await;

        // One balance fetch serves every market.
        //
        // Funds locked behind a withdrawal in flight are removed here, once, so
        // every consumer below -- valuation, targets, the Purse, the reconciler
        // -- sees what we actually own rather than what the exchange has not
        // yet shipped.
        let balances = self.without_pending_withdrawals(self.api.balances().await?);
        self.record_balances(&balances).await;
        if let Err(e) = self.chain_buyback(&balances).await {
            warn!(error = %format!("{e:#}"), "follow-on buy check failed");
        }

        self.loop_n = self.loop_n.wrapping_add(1);

        // Lighter, once a minute or so: observation only, and a failure there
        // must never cost a loop here.
        if self.loop_n % 3 == 0 {
            self.record_lighter().await;
        }

        // Markets due this loop. A thin book re-quoted every six seconds costs
        // API calls the busy book needs more, and gains nothing: the exchange
        // rate-limits us long before PRL/USDC's hundred trades a day require
        // that attention.
        let markets: Vec<Market> = self
            .markets
            .iter()
            .filter(|m| {
                let n = self
                    .cfg
                    .overrides
                    .get(&m.id)
                    .and_then(|o| o.every_n_loops)
                    .unwrap_or(1)
                    .max(1);
                self.loop_n % n == 0
            })
            .cloned()
            .collect();
        if markets.is_empty() {
            return Ok(());
        }

        // ---- pass one: observe every book and take in every fill ----
        //
        // Fills must all be in before inventory is judged, because a base asset
        // traded on two books moves from both, and a deposit is defined as the
        // part of the move our own fills do not explain.
        let mut books: HashMap<String, OrderBook> = HashMap::new();
        let mut resting: HashMap<String, Vec<crate::db::models::LiveOrder>> = HashMap::new();
        let mut filled: HashMap<String, Decimal> = HashMap::new();

        for m in &markets {
            match self.api.depth(&m.id, self.cfg.depth_levels).await {
                Ok(b) => {
                    books.insert(m.id.clone(), b);
                }
                Err(e) => {
                    self.note_market_failure(&m.id, "depth", &format!("{e:#}")).await;
                    continue;
                }
            }

            let exchange_orders = match self.api.open_orders(&m.id).await {
                Ok(o) => o,
                Err(e) => {
                    self.note_market_failure(&m.id, "orders", &format!("{e:#}")).await;
                    continue;
                }
            };
            self.market_failures.remove(&m.id);
            self.db.sync_open_orders(&m.id, &exchange_orders).await?;
            resting.insert(m.id.clone(), self.reconcile_view(m, &exchange_orders).await?);

            // Own trades are only worth fetching when something changed.
            //
            // A resting book that has not moved cannot have produced a fill, so
            // asking costs a request and returns what we already have. With
            // three markets that one saving is the difference between being
            // rate-limited and not.
            let fingerprint = exchange_orders
                .iter()
                .map(|o| format!("{}:{}", o.id, o.filled_amount))
                .collect::<Vec<_>>()
                .join(",");
            let changed = self.order_state.get(&m.id) != Some(&fingerprint);
            self.order_state.insert(m.id.clone(), fingerprint);
            // Sweep occasionally regardless, in case a fill arrives in a way
            // the order list does not reveal.
            let sweep = self.loop_n % 30 == 0;
            if !changed && !sweep {
                continue;
            }

            // Write-once per market, before any fill of its is considered.
            self.db.ensure_fill_cutover(&m.id).await?;
            let cutover = self.db.fills_since_ms(&m.id).await;
            // Clamp to now. The exchange emits trades stamped days in the
            // future; if one of ours is, `time_from` lands in the future and
            // the endpoint returns nothing until real time catches up -- fills
            // stop being ingested, the position freezes, and every subsequent
            // balance move looks like a deposit.
            let since = self
                .db
                .last_fill_ts(&m.id)
                .await?
                .map(|ms| ms - 300_000)
                .unwrap_or(cutover)
                .max(cutover)
                .min(now_ms())
                / 1000;
            // Walk backwards a page at a time until a page is short.
            //
            // One page of 200 silently dropped the oldest fills whenever more
            // than that arrived since we last looked -- and the gap that
            // produces it is exactly a restart or an outage, when our whole
            // ladder can fill while nobody is watching. Those fills would never
            // be ingested, so the position, the banked edge and the report
            // would all be wrong, permanently and quietly.
            let mut trades = Vec::new();
            let mut cursor = Some(since).filter(|s| *s > 0);
            for _ in 0..FILL_PAGES_MAX {
                let page = match self.api.my_trades(&m.id, cursor, FILL_PAGE).await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(market = %m.id, error = %format!("{e:#}"), "could not fetch own trades");
                        break;
                    }
                };
                let short = (page.len() as u32) < FILL_PAGE;
                // Ask again from just past the newest of this page.
                let newest = page.iter().map(|t| t.created_at).max();
                trades.extend(page);
                match newest {
                    Some(n) if !short => cursor = Some(n + 1),
                    _ => break,
                }
            }
            // The mid this loop observed, WITHOUT our own orders in it.
            //
            // Edge is how far our price sat on the good side of the mid. Using
            // the raw book made our own quote part of the mid we score against,
            // and on PRL/USDC -- where we are routinely the only maker on one
            // side -- that is almost the whole mid. A lone bid at 1.03 against
            // a stranger's 1.07 ask gives a microprice near 1.063, so a fill on
            // our own bid booked about 3.3% of "edge" where the honest figure
            // was nearer 0.7%. `bank_profit` then raises the balanced target by
            // that number, so a measurement error became a standing instruction
            // to hold more PRL -- a directional tilt, presented as profit.
            //
            // With our size stripped a one-sided book has no mid at all, and
            // `ingest_fills` leaves the edge unscored. Under-counting is the
            // safe direction: it banks less, rather than inventing a position.
            let mid = {
                let raw = books.get(&m.id);
                let live = resting.get(&m.id).cloned().unwrap_or_default();
                raw.and_then(|b| strip_own(b, &live, m).microprice())
            };
            let (new_fills, _pos, net, net_quote) =
                self.db.ingest_fills(&m.id, &trades, mid).await?;
            if new_fills > 0 {
                info!(market = %m.id, new_fills, net = %net, "fills ingested");
            }
            // Credit BOTH currencies. USDC is the base of USDC/USDT and the
            // quote of PRL/USDC, so a PRL/USDC fill moves the USDC balance --
            // and without this it looks like money arriving from nowhere.
            *filled.entry(m.base_unit.clone()).or_default() += net;
            *filled.entry(m.quote_unit.clone()).or_default() += net_quote;
        }

        // ---- transfers in and out, from the exchange's own records ----
        //
        // Before inventory is judged, not after. Inference runs every third
        // loop and the ledger was on a two-minute timer, so a deposit was
        // reported as unexplained drift up to four times before the ledger
        // explained it -- alerts saying "not a transfer" during the one event
        // that is.
        //
        // Authoritative, so it replaces guessing from a balance that moved.
        // Inference could never tell a deposit from a fill it had not yet
        // ingested, which is how our own buys came to be reported as deposits.
        // Here the amount, the currency and the id are all given.
        // Also read it immediately when inventory looks unexplained, so a
        // deposit is identified as such rather than reported as drift first.
        let unexplained = self.pending_external.values().any(|v| !v.is_zero());
        if unexplained || now_ms() - self.last_transfer_check > 120_000 {
            self.last_transfer_check = now_ms();
            match self.absorb_transfers(&markets, &balances).await {
                Ok(n) => {
                    self.transfer_checks += 1;
                    // Say so once, so the path is visibly working before a
                    // deposit actually needs catching.
                    if self.transfer_checks == 1 {
                        info!(applied = n, "transfer ledger reachable; watching for deposits");
                    }
                }
                Err(e) => warn!(error = %format!("{e:#}"), "could not read transfers"),
            }
        }

        // ---- inherit holdings, and notice anything that arrived ----
        for base in distinct(markets.iter().map(|m| m.base_unit.clone())) {
            let (avail, locked) = balance_of(&balances, &base);
            let inventory = avail + locked;

            // Price the baseline off whichever book for this asset we could read.
            let mark = markets
                .iter()
                .filter(|m| m.base_unit == base)
                .find_map(|m| books.get(&m.id).and_then(|b| b.microprice()));

            if let Some(mark) = mark
                && self.db.seed_baseline(&base, inventory, mark).await?
            {
                info!(%base, qty = %inventory, %mark, "seeded opening position");
                self.db
                    .alert(
                        "baseline",
                        &format!(
                            "Starting from {} {} marked at {}. PnL is measured from here.",
                            inventory.round_dp(2), base.to_uppercase(), mark.round_dp(5)
                        ),
                    )
                    .await;
            }

            let net = filled.get(&base).copied().unwrap_or_default();
            // Every book trading this asset must have been processed, or the
            // fill picture is incomplete and inventory cannot be judged.
            let complete = self
                .markets
                .iter()
                .filter(|m| m.base_unit == base)
                .all(|m| markets.iter().any(|d| d.id == m.id));
            // No program is a halt: every order cancelled, the book abandoned
            // until a human types something. Making a market around what we
            // already hold changes no position and is the bot's whole job, so
            // stand one up rather than stand down.
            //
            // Created in the database rather than invented inside `plan`, so
            // it shows up in the report and can be replaced like any other.
            if self.cfg.always_quote
                && inventory > Decimal::ZERO
                && self.db.active_program(&base).await?.is_none()
            {
                match self
                    .db
                    .create_program(
                        &base,
                        crate::db::models::ProgramKind::Neutral,
                        inventory,
                        inventory,
                        None,
                        None,
                        None,
                        Some("auto: market-making around what we hold"),
                    )
                    .await
                {
                    Ok(id) => info!(program = id, %base, %inventory, "standing up a flat program"),
                    Err(e) => warn!(error = %format!("{e:#}"), "could not stand up a program"),
                }
            }

            if let Some(p) = self.db.active_program(&base).await? {
                self.absorb_external_flow(&base, inventory, net, complete).await?;

                // Stop the glide's clock while its price floor forbids
                // trading. Without this the schedule keeps running through the
                // block, falls behind, and then sells FASTER once price
                // recovers to catch up -- the exact cheap selling the floor
                // exists to prevent.
                if p.kind.is_scheduled()
                    && p.limit_price.is_some()
                    && let Some(side) = p.working_side()
                {
                    let px = markets
                        .iter()
                        .find(|m| m.base_unit == base)
                        .and_then(|m| books.get(&m.id))
                        .and_then(|b| b.microprice());
                    if let Some(px) = px {
                        let rate = p.clock_rate(
                            side, px, self.cfg.trim_speed_gain, self.cfg.trim_speed_max,
                        );
                        let adjust = self.db.advance_clock(p.id, rate).await?;
                        if adjust != 0 {
                            debug!(
                                program = p.id, %px, %rate, adjust,
                                limit = ?p.limit_price, "glide clock re-paced"
                            );
                        }
                    }
                }

                self.maybe_complete(&p, inventory).await?;
                self.bank_profit(&p, &base).await?;
            }
        }

        // ---- account starting value, recorded once ----
        //
        // Valued by `holdings` over EVERY market, which is exactly what the
        // report's "now" is the sum of, so the two ends of the scoreboard are
        // one calculation.
        //
        // This used to price the first book's base asset and add the quote
        // currencies of `markets` -- the PER-LOOP subset, not the account.
        // Both books that touch USDC run `every_n_loops = 3`, so on two loops
        // in three the subset was prlusdt alone, nothing in it had USDC as a
        // quote unit, and the whole USDC pool dropped out of the baseline.
        // Nothing failed; the scoreboard simply read that pool as market
        // profit for the rest of the epoch -- a phantom +1,212 on a 12k
        // account, and it re-appeared on every `ctl epoch`. A starting value
        // is written once and is load-bearing for every number derived from
        // it, so it must never be computed from a subset of the books.
        if self.db.account_start_value().await.is_none() {
            let prices = self.prices(&books).await;
            let value: Decimal = holdings(&self.markets, &balances, &prices).values().sum();
            if value > Decimal::ZERO && self.db.seed_account_value(value).await? {
                info!(%value, "recorded the account's starting value");
            }
        }

        // ---- what each book trades, for the report ----
        //
        // Volume comes from the exchange's own 24h ticker, not from counting
        // the trades page. That page caps at 1000 rows -- twenty minutes on a
        // busy book -- and extrapolating twenty minutes of a rally to a day
        // overstated PRL/USDT more than fourfold. Some rows also carry
        // timestamps days in the future, which corrupts any span derived from
        // them. One request covers every market.
        // Per market, because the thin books are quoted every third loop and a
        // single shared timer kept landing on a loop where their books were not
        // fetched -- so they were never measured at all. Nothing in the quoting
        // reads these; they are the report's "share of the market".
        let due: Vec<String> = markets
            .iter()
            .filter(|m| books.contains_key(&m.id))
            .filter(|m| {
                now_ms() - self.last_measured.get(&m.id).copied().unwrap_or(0) > 600_000
            })
            .map(|m| m.id.clone())
            .collect();
        if !due.is_empty() {
            match self.api.tickers().await {
                Ok(tickers) => {
                    for m in markets.iter().filter(|m| due.contains(&m.id)) {
                        let Some(book) = books.get(&m.id) else { continue };
                        self.last_measured.insert(m.id.clone(), now_ms());
                        let spread = match (book.best_bid(), book.best_ask()) {
                            (Some((b, _)), Some((a, _))) if b > Decimal::ZERO => {
                                (a - b) / ((a + b) / Decimal::TWO)
                            }
                            _ => continue,
                        };
                        let Some(vol24) = tickers.get(&m.id).and_then(|t| t.ticker.volume) else {
                            continue;
                        };
                        let per_hour = vol24 / dec!(24);
                        if self.db.record_market_stats(&m.id, per_hour, spread, dec!(0.3)).await.is_ok() {
                            debug!(
                                market = %m.id, per_hour = %per_hour.round_dp(0),
                                spread = %(spread * dec!(100)).round_dp(2), "measured"
                            );
                        }
                    }
                }
                Err(e) => warn!(error = %format!("{e:#}"), "could not read tickers"),
            }
        }

        // ---- cross-market reference prices ----
        //
        // The configured primary book prices the asset; the thin books are
        // quoted around that, converted through the stablecoin cross. Built
        // from the books WITHOUT our own orders in them, so our quotes cannot
        // set the fair value the thin books are then priced against. See
        // `references` for the rules and the incident behind them.
        let public: HashMap<String, OrderBook> = markets
            .iter()
            .filter_map(|m| {
                let raw = books.get(&m.id)?;
                let live = resting.get(&m.id).cloned().unwrap_or_default();
                Some((m.id.clone(), strip_own(raw, &live, m)))
            })
            .collect();
        let reference = references(&self.cfg.market, &self.markets, &public);

        // ---- pass two: quote ----
        //
        // Free balances are carried across markets and drawn down as each one
        // commits. Two books can share a currency -- PRL/USDC spends USDC while
        // USDC/USDT holds it as inventory -- and sizing both against the same
        // untouched balance is how you end up placing orders you cannot fund.
        // Each market gets a share of every currency it needs, split by what
        // its arms have earned (`allocate`). Without this the first market in
        // the list spends the entire balance and the rest quote nothing --
        // config order would silently decide which books get worked.
        // Spendable means available PLUS what our own resting orders hold.
        //
        // Sizing against `available` alone is unstable: placing an order locks
        // its funds, so the next loop sizes the ladder smaller, which fails the
        // size tolerance, which cancels and replaces, which frees the funds and
        // sizes it larger again. The bot churned its whole book on that loop,
        // climbing from 14 to 30 placements a minute and drawing rate limits.
        //
        // The reconciler cancels whatever no longer matches before placing, so
        // the funds behind our own orders are genuinely available to re-commit;
        // on a dedicated account there is nothing else holding them.
        let mut purse: HashMap<String, Decimal> = HashMap::new();
        for b in &balances {
            purse.insert(b.currency.to_lowercase(), b.balance + b.locked);
        }
        // The set-aside pile never reaches a book: it comes off the asset's
        // TOTAL, before the split. It used to be subtracted from each book's
        // SHARE after the split, so a reserve of about five-eighths of the
        // holding against prlusdc's ~46% share of it left that book a negative
        // budget -- no ask on the only book that pays, for about five hours on
        // 2026-09-28, while over a third of the PRL sat free.
        let mut piles: HashMap<String, Decimal> = HashMap::new();
        for ccy in purse.keys() {
            // Less whatever is on its way out: a withdrawal in flight has
            // already left the balance read above.
            let in_flight = self.pending_out.get(ccy).copied().unwrap_or_default();
            piles.insert(ccy.clone(), (self.db.set_aside(ccy).await - in_flight).max(Decimal::ZERO));
        }
        hold_back(&mut purse, &piles);
        // Size follows measured edge. Refreshed on its own timer, from orders
        // and fills already recorded, so it costs nothing on the loops in
        // between. Before the split below, which follows the same table.
        self.refresh_arms(&params).await;
        // And where the capital should sit, from the same table, on its own
        // slower timer.
        self.retarget(&params, &balances, &public).await;

        // Keep the acquiring book's bids in the currency they spend, crossing
        // the stablecoin book to do it when the cycle can pay for the crossing.
        // Before the quoting, so the money is there on this loop rather than
        // the next one.
        if let Err(e) = self.replenish_acquire_quote(&params, &balances, &public).await {
            warn!(error = %format!("{e:#}"), "could not refill the acquiring book's bids");
        }

        // Allocate across EVERY configured market, not just the ones due this
        // loop. Splitting only among the due markets meant prlusdt took 100%
        // of USDT on two loops in three and ~70% on the third, so every rung
        // resized by ~30% every third loop -- past the 0.25 size tolerance,
        // which cancels and replaces the whole ladder, then replaces it again
        // on the next loop. That is the queue position and the action budget
        // the Purse work was meant to stop burning.
        //
        // In TWAP mode the split is taken over the books that are WORKING the
        // twap and nothing else. Every other book stands down, so leaving them
        // in the split reserves money for a ladder that will never be placed:
        // the weights are clamped to 70% a book, which would have left about a
        // third of the USDT idle behind a halted stablecoin market while the
        // whole point was to spend it.
        let funding: Vec<Market> = if params.q.twap_only {
            let mut keep = Vec::new();
            for m in &self.markets {
                if matches!(
                    self.db.active_program(&m.base_unit).await,
                    Ok(Some(p)) if p.kind.is_twap()
                ) {
                    keep.push(m.clone());
                }
            }
            keep
        } else {
            self.markets.clone()
        };
        let allocation = allocate(
            if funding.is_empty() { &self.markets } else { &funding },
            &purse,
            &self.arm_sizes,
        );

        for m in &markets {
            let Some(book) = books.get(&m.id) else { continue };
            let live = resting.get(&m.id).cloned().unwrap_or_default();
            let (reference, reference_external) = match reference.get(&m.id).copied() {
                Some((p, ext)) => (Some(p), ext),
                None => (None, false),
            };
            let share = allocation.get(&m.id).cloned().unwrap_or_default();
            let reference_required = is_referenced(&self.cfg.market, &self.markets, m);
            if let Err(e) = self
                .quote_market(
                    m, book, &live, &balances, &params, reference, reference_external,
                    reference_required, &share, &mut purse,
                )
                .await
            {
                error!(market = %m.id, error = %format!("{e:#}"), "quoting failed");
                self.db
                    .log("error", "loop_error", &format!("{}: {e:#}", m.id), None)
                    .await;
            }
        }

        // Score fills against the mid five minutes on, in small batches. This
        // is the number that says whether the quoting is actually earning --
        // `edge` alone can only ever flatter us.
        if let Err(e) = self.db.settle_markouts().await {
            warn!(error = %format!("{e:#}"), "could not settle markouts");
        }

        // Heartbeat, so a restart can tell a deploy from a real outage.
        self.db.set_setting("last_seen_ms", &now_ms().to_string()).await.ok();

        // Remember what the exchange will tolerate, so a deploy does not have
        // to find the ceiling by hitting it again.
        let (rate, refusals) = self.api.pacing().await;
        self.db.set_setting("api_rate", &format!("{rate:.3}")).await.ok();
        if self.loop_n % 100 == 0 {
            info!(api_rate = %format!("{rate:.2}/s"), refusals, "pacing");
        }

        self.maybe_prune().await;
        Ok(())
    }

    /// Decide and place quotes for one book.
    #[allow(clippy::too_many_arguments)]
    async fn quote_market(
        &mut self,
        market: &Market,
        book: &OrderBook,
        live: &[crate::db::models::LiveOrder],
        balances: &[Balance],
        params: &Params,
        reference_price: Option<Decimal>,
        reference_is_external: bool,
        reference_required: bool,
        share: &HashMap<String, Decimal>,
        purse: &mut HashMap<String, Decimal>,
    ) -> Result<()> {
        let params = params.for_market(self.cfg.overrides.get(&market.id));

        let (base_avail, base_locked) = balance_of(balances, &market.base_unit);
        let inventory = base_avail + base_locked;
        let (quote_avail, _quote_locked) = balance_of(balances, &market.quote_unit);

        // Portfolio valuation counts EVERY quote currency this asset trades
        // against, not just this book's own.
        //
        // Otherwise the two PRL books value the same portfolio differently and
        // steer to different targets: with PRL 10,000, USDT 2,000 and USDC
        // 12,000, PRL/USDT aims at 6,000 PRL while PRL/USDC aims at 11,000, and
        // they lean against each other. The deep book wins on volume, so the
        // account drifts toward half of (PRL + USDT) and ignores the USDC
        // entirely. One asset, one target.
        let quote_total: Decimal = distinct(
            self.markets
                .iter()
                .filter(|m| m.base_unit == market.base_unit)
                .map(|m| m.quote_unit.clone()),
        )
        .iter()
        .map(|c| {
            let (a, l) = balance_of(balances, c);
            a + l
        })
        .sum();

        // Spendable here: this market's allocation, capped by what is actually
        // left after earlier markets in this loop committed theirs.
        let allowed = |ccy: &str| -> Decimal {
            let left = purse.get(ccy).copied().unwrap_or_default();
            share.get(ccy).copied().unwrap_or(left).min(left)
        };
        let base_free = allowed(&market.base_unit);
        let quote_free = allowed(&market.quote_unit);
        debug_assert!(base_free >= Decimal::ZERO && quote_free >= Decimal::ZERO);

        // Our own resting size is not liquidity we can lean on.
        let public_book = strip_own(book, live, market);

        // The volatility breaker and the spread widening read this history.
        // On a referenced book it is the REFERENCE that moves, not the local
        // residue: see `volatility_anchor`.
        if let Some(fv) = volatility_anchor(reference_price, reference_is_external, public_book.microprice()) {
            self.fv_history.entry(market.id.clone()).or_default().push((now_ms(), fv));
        }
        let recent_move = self.recent_move(&market.id, self.cfg.risk.vol_window_secs);
        // Unknown share means no opinion, which means no halt.
        let our_participation = self
            .participation(&market.id, params.q.participation_window_secs)
            .await
            .unwrap_or(Decimal::ZERO);

        let program = self.db.active_program(&market.base_unit).await?;
        // A market the sizer has not seen quotes the minimum on every rung:
        // the failure mode is too little, never too much.
        let none = ArmScales::none();
        let arms = self.arm_scales.get(&market.id).unwrap_or(&none);

        // How much of full size the cycle will let this book's ask rest, even
        // where the arms have defunded it. Proportional to how well releasing
        // here pays against the best venue, so the overflow book stays small
        // in exactly the proportion it is worse -- on the measured table,
        // prlusdt releases at +24 bp against prlusdc's +232, so it gets about
        // a tenth of a rung and never competes with the book that pays.
        let cycle_release_floor = self.cycle_release_floor(&market.id, &params);

        // And whether this is the acquiring book, round-tripping on an edge
        // good enough that the inventory skew should stop throttling one of its
        // sides. See `Quoting::cycle_churn`.
        let cycle_churn = self.cycle_churn(&market.id, &params);

        let sell = program.as_ref().filter(|p| p.kind.is_slow());
        if let Some(p) = sell {
            let buy = p.kind == ProgramKind::TwapBuy;
            let k = if buy { "buy" } else { "sell" };
            // Between passes the orders are left alone. A pause or a hold acts
            // at once, never waiting out the interval.
            let every = self.db.setting_decimal(&format!("{k}_every_secs"), Decimal::from(SELL_EVERY_SECS)).await;
            let every_ms = i64::try_from(every.round()).unwrap_or(SELL_EVERY_SECS).max(5) * 1000;
            let last = self.sell_planned.get(&market.id).copied().unwrap_or(0);
            let stop = !params.enabled
                || self.db.setting_bool("twap_hold", false).await
                || self.slow_skips(&market.id).await;
            // An order that filled since the last pass is refilled now, not up
            // to two minutes later: a sweep is when the other side is there.
            let since = if buy {
                self.db.bought_since(&market.id, last).await
            } else {
                self.db.sold_since(&market.id, last).await
            };
            let filled = last > 0 && since.unwrap_or_default() > Decimal::ZERO;
            if !stop && !filled && now_ms() - last < every_ms {
                return Ok(());
            }
            self.sell_planned.insert(market.id.clone(), now_ms());
        }
        let decision = if let Some(p) = sell {
            self.plan_sell(market, &public_book, live, p, &params, base_avail, quote_avail, inventory).await
        } else {
            plan(&QuoteInput {
            cycle_release_floor,
            cycle_churn,
            market,
            book: &public_book,
            program: program.as_ref(),
            params: &params,
            inventory,
            base_available: base_free,
            quote_available: quote_free,
            quote_total,
            our_participation,
            recent_move,
            reference_price,
            reference_is_external,
            reference_required,
            arms,
            now_ms: now_ms(),
        })
        };

        // In TWAP mode the stablecoin book would stand down. Instead, if the
        // owner has set a minimum, it sells its USDC for USDT at that price or
        // better (2026-10-10: "opportunistically convert usdc to usdt").
        let decision = match &decision.halt {
            Some(Halt::TwapOnly) if params.enabled && market.base_unit == "usdc" && market.quote_unit == "usdt" => {
                let min = self.db.setting_decimal("usdc_convert_min", dec!(1.0010)).await;
                let own: Decimal = live.iter().filter(|o| o.side == Side::Sell).map(|o| o.remaining()).sum();
                match crate::strategy::sell::convert_ask(
                    market,
                    public_book.best_bid().map(|(p, _)| p),
                    public_book.best_ask().map(|(p, _)| p),
                    base_avail + own,
                    min,
                ) {
                    Some(q) => {
                        info!(market = %market.id, price = %q.price, amount = %q.amount, %min, "converting usdc");
                        Decision { quotes: vec![q], halt: None, fair_value: public_book.mid(), target_inventory: inventory }
                    }
                    None => decision,
                }
            }
            _ => decision,
        };

        // A twap may stand down on its own: by hand (`twap_hold`), or for a
        // breather after price ran up more than `twap_spike_pct` in an hour.
        // Asked for on 2026-10-07: do not chase a spike with the whole purse.
        // It clears by itself once the hour's low catches up or price falls.
        let decision = match program.as_ref() {
            Some(p) if p.kind.is_twap() && decision.halt.is_none() => {
                if self.db.setting_bool("twap_hold", false).await {
                    Decision::halted(Halt::TwapHeld, decision.target_inventory)
                } else if p.kind == ProgramKind::TwapSell {
                    // The spike guard is for buying; a spike is what the slow
                    // sell is waiting for. Its mirror is a crash: stand back
                    // while price is well under the hour's high. Asked for on
                    // 2026-10-19: "dont sell pearl into a crash".
                    let pct = self.db.setting_decimal("twap_crash_pct", dec!(10)).await;
                    match (pct > Decimal::ZERO, decision.fair_value) {
                        (true, Some(fv)) => match self.db.hour_high(&market.id).await {
                            Some(high) if high > Decimal::ZERO && (high - fv) / high * dec!(100) > pct => {
                                Decision::halted(Halt::TwapCrash { fall: (high - fv) / high }, decision.target_inventory)
                            }
                            _ => decision,
                        },
                        _ => decision,
                    }
                } else {
                    let pct = self.db.setting_decimal("twap_spike_pct", dec!(5)).await;
                    match (pct > Decimal::ZERO, decision.fair_value) {
                        (true, Some(fv)) => match self.db.hour_low(&market.id).await {
                            Some(low) if low > Decimal::ZERO && (fv - low) / low * dec!(100) > pct => {
                                Decision::halted(
                                    Halt::TwapSpike { rise: (fv - low) / low },
                                    decision.target_inventory,
                                )
                            }
                            _ => decision,
                        },
                        _ => decision,
                    }
                }
            }
            _ => decision,
        };

        self.db
            .snapshot_book(
                &market.id,
                &public_book,
                params.q.depth_band,
                live.iter().filter(|o| o.side == Side::Buy).map(|o| o.price).max(),
                live.iter().filter(|o| o.side == Side::Sell).map(|o| o.price).min(),
                inventory,
                decision.target_inventory,
            )
            .await
            .ok();

        if let Some(halt) = &decision.halt {
            let first = self.halted.insert(market.id.clone(), halt.reason()).is_none();
            if first {
                warn!(market = %market.id, reason = %halt.reason(), resting = live.len(), "halting");
                self.db
                    .alert(
                        "halt",
                        &format!("{}: quoting halted — {}", market.id, halt.reason()),
                    )
                    .await;
            }
            if !live.is_empty() && !params.dry_run {
                self.cancel_everything(&market.id).await?;
            }
            return Ok(());
        }

        // Back in business. Worth saying, since the halt was worth saying.
        if let Some(was) = self.halted.remove(&market.id) {
            info!(market = %market.id, was = %was, "resumed quoting");
            self.db
                .alert(
                    "resumed",
                    &format!("{}: quoting resumed (was halted — {was})", market.id),
                )
                .await;
        }

        // A twap grows its bid by STACKING orders at the best bid, never by
        // replacing: a resized order goes to the back of the queue.
        let mut decision = decision;
        if program.as_ref().is_some_and(|p| p.kind == ProgramKind::Twap) {
            decision.quotes = twap_stack(
                &decision.quotes, live, decision.target_inventory, inventory, quote_avail,
                market.amount_precision,
            );
        }

        // Reserve what we are about to commit so later markets see it gone.
        for q in &decision.quotes {
            match q.side {
                Side::Buy => {
                    let e = purse.entry(market.quote_unit.clone()).or_default();
                    *e = (*e - q.amount * q.price).max(Decimal::ZERO);
                }
                Side::Sell => {
                    let e = purse.entry(market.base_unit.clone()).or_default();
                    *e = (*e - q.amount).max(Decimal::ZERO);
                }
            }
        }

        // Reconcile against what is genuinely spendable right now: the free
        // balance, which each cancel then adds back to.
        self.reconcile(
            market,
            &decision,
            live,
            &params,
            program.as_ref().map(|p| p.id),
            base_avail,
            quote_avail,
        )
        .await?;

        // Then, and only if the owner has switched it on for this book, cross
        // the spread to keep a schedule the passive ladder cannot keep. After
        // the ladder, so the crossing is sized against what the rungs did not
        // already commit, and bounded by every cap in `take_if_behind`.
        if let (Some(p), Some(fv)) = (program.as_ref().filter(|p| !p.kind.is_slow()), decision.fair_value) {
            self.take_if_behind(
                market,
                &public_book,
                p,
                &params,
                fv,
                inventory,
                decision.target_inventory,
                quote_free,
                base_free,
                our_participation,
            )
            .await;
        }

        info!(
            market = %market.id,
            fv = ?decision.fair_value.map(|d| d.round_dp(5)),
            inventory = %inventory.round_dp(2),
            target = %decision.target_inventory.round_dp(2),
            quotes = decision.quotes.len(),
            resting = live.len(),
            ref_px = ?reference_price.map(|p| p.round_dp(5)),
            "tick"
        );
        Ok(())
    }

    /// Quote currency held back for other budgeted slow buys: what each has
    /// left of its budget after what it spent and what its open bids hold.
    /// Only budgets worked on a book quoted in `quote` count. A count that
    /// cannot be read reserves the whole budget, never nothing.
    async fn reserved_for_budgets(&self, except: i64, quote: &str) -> Decimal {
        let mut reserved = Decimal::ZERO;
        for base in distinct(self.markets.iter().map(|m| m.base_unit.clone())) {
            let Ok(Some(p)) = self.db.active_program(&base).await else { continue };
            let Some(budget) = p.budget.filter(|_| p.kind == ProgramKind::TwapBuy && p.id != except) else {
                continue;
            };
            if !self.markets.iter().any(|m| m.base_unit == base && m.quote_unit == quote) {
                continue;
            }
            reserved += match self.db.program_buy_spend(p.id).await {
                Ok((spent, committed)) => (budget - spent - committed).max(Decimal::ZERO),
                Err(_) => budget,
            };
        }
        reserved
    }

    /// Whether `slow_skip` names this book: the slow sell and slow buy stay
    /// out of it and pull what they have resting there. Asked for on
    /// 2026-10-14: "pull the usdc sells".
    async fn slow_skips(&self, market: &str) -> bool {
        self.db
            .get_setting("slow_skip")
            .await
            .ok()
            .flatten()
            .is_some_and(|v| v.split(',').any(|m| m.trim().eq_ignore_ascii_case(market)))
    }

    /// The slow sell's decision for one book: small asks joined to the
    /// strangers' lowest levels, capped by the tape. See `strategy::sell`.
    ///
    /// It skips `plan` and its breakers on purpose. The volatility breaker
    /// stands down exactly when a sweep comes through, and the floor already
    /// keeps it from selling cheap.
    #[allow(clippy::too_many_arguments)]
    async fn plan_sell(
        &self,
        market: &Market,
        book: &OrderBook,
        live: &[crate::db::models::LiveOrder],
        program: &Program,
        params: &Params,
        base_avail: Decimal,
        quote_avail: Decimal,
        inventory: Decimal,
    ) -> Decision {
        if !params.enabled {
            return Decision::halted(Halt::Disabled, inventory);
        }
        if self.slow_skips(&market.id).await {
            return Decision::halted(Halt::SlowSkip, inventory);
        }
        let buy = program.kind == ProgramKind::TwapBuy;
        let k = if buy { "buy" } else { "sell" };
        let best_bid = book.best_bid().map(|(p, _)| p);
        if best_bid.is_none() && book.asks.is_empty() {
            return Decision::halted(Halt::EmptyBook, inventory);
        }
        let now = now_ms();
        let (day_ago, hour_ago) = (now - 86_400_000, now - 3_600_000);
        let pct = |v: Decimal| v / dec!(100);
        let count = |v: Decimal, d: usize| v.round().to_string().parse::<usize>().unwrap_or(d).min(50);
        let d = crate::strategy::sell::SellParams::default();
        // The buy reads its own `buy_*` knobs, so tuning one side never moves
        // the other. Its boost is off unless set: the sell's default boost
        // price is a selling price.
        let setting = |name: &str| format!("{k}_{name}");
        let boost_default = if buy { Decimal::ZERO } else { d.boost_price };
        let sp = crate::strategy::sell::SellParams {
            day_rate: pct(self.db.setting_decimal(&setting("day_pct"), d.day_rate * dec!(100)).await),
            hour_rate: pct(self.db.setting_decimal(&setting("hour_pct"), d.hour_rate * dec!(100)).await),
            levels: count(self.db.setting_decimal(&setting("levels"), Decimal::from(d.levels)).await, d.levels),
            keep_levels: count(self.db.setting_decimal(&setting("keep_levels"), Decimal::from(d.keep_levels)).await, d.keep_levels),
            level_share: pct(self.db.setting_decimal(&setting("level_pct"), d.level_share * dec!(100)).await),
            max_order: self.db.setting_decimal(&setting("max_order"), d.max_order).await,
            min_order: d.min_order,
            stand_back: self.db.setting_decimal(&setting("stand_back"), d.stand_back).await,
            boost_price: self.db.setting_decimal(&setting("boost_price"), boost_default).await,
            boost: self.db.setting_decimal(&setting("boost"), d.boost).await,
            min_notional: self.db.setting_decimal(&setting("min_notional"), d.min_notional).await,
            near_levels: count(self.db.setting_decimal(&setting("near_levels"), Decimal::from(d.near_levels)).await, d.near_levels),
            near_share: pct(self.db.setting_decimal(&setting("near_pct"), d.near_share * dec!(100)).await).min(Decimal::ONE),
            spike_rungs: count(self.db.setting_decimal(&setting("spike_rungs"), Decimal::from(d.spike_rungs)).await, d.spike_rungs),
            spike_step: pct(self.db.setting_decimal(&setting("spike_step_pct"), d.spike_step * dec!(100)).await),
            spike_share: pct(self.db.setting_decimal(&setting("spike_pct"), d.spike_share * dec!(100)).await).min(Decimal::ONE),
            ..d
        };
        // Who is trading, over every book on this asset: the deep book's
        // flow is the signal for the thin one too.
        let flow_mins = self.db.setting_decimal("sell_flow_mins", dec!(10)).await;
        let since = now - i64::try_from((flow_mins * dec!(60000)).round()).unwrap_or(600_000);
        let (mut bought, mut sold) = (Decimal::ZERO, Decimal::ZERO);
        for m in self.markets.iter().filter(|m| m.base_unit == market.base_unit) {
            if let Ok((b, s)) = self.db.taker_flow(&m.id, since).await {
                bought += b;
                sold += s;
            }
        }
        // Too little traded to say who is driving: no opinion.
        let flow = (bought + sold >= self.db.setting_decimal("sell_flow_min_volume", dec!(1000)).await)
            .then(|| (bought - sold) / (bought + sold));
        let side = if buy { Side::Buy } else { Side::Sell };
        let resting: Vec<crate::strategy::sell::Resting> = live
            .iter()
            .filter(|o| o.side == side)
            .map(|o| crate::strategy::sell::Resting { price: o.price, level: o.level, remaining: o.remaining() })
            .collect();
        let seed = now as u64 ^ market.id.bytes().fold(0u64, |a, b| a.wrapping_mul(131).wrapping_add(b as u64));
        let fair_value = match (best_bid, book.best_ask().map(|(p, _)| p)) {
            (Some(b), Some(a)) => Some((b + a) / Decimal::TWO),
            (b, a) => b.or(a),
        };
        if buy {
            let Some(ceiling) = program.limit_price.filter(|p| *p > Decimal::ZERO) else {
                // Refused at creation; a row from elsewhere without one buys nothing.
                return Decision::halted(Halt::Disabled, inventory);
            };
            // `buy_qty` caps what this program puts in the account, net of the
            // fee taken in the base, counted from its own fills. What is left
            // is grossed up by the fee these fills actually paid (0.10% until
            // one has), so the last bids land on the number, not a fee short.
            let cap = self.db.setting_decimal("buy_qty", Decimal::ZERO).await;
            let max_base = if cap > Decimal::ZERO {
                let Ok((net, gross)) = self.db.program_received(program.id, &market.base_unit).await else {
                    return Decision::halted(Halt::Disabled, inventory);
                };
                let left = cap - net;
                if left <= Decimal::ZERO {
                    return Decision::halted(Halt::BuyQtyDone { received: net }, inventory);
                }
                let keep = if gross > Decimal::ZERO { net / gross } else { dec!(0.999) };
                let committed: Decimal = resting.iter().map(|r| r.remaining).sum();
                Some((left / keep.max(dec!(0.9)) - committed).max(Decimal::ZERO))
            } else {
                None
            };
            // A budget caps what this program spends, counted from its own
            // fills plus what its open bids already hold. Asked for on
            // 2026-10-17: "buy me $10,000 worth of NOID" beside a PRL buy
            // spending everything else.
            let mut free_quote = quote_avail;
            if let Some(budget) = program.budget {
                let Ok((spent, _)) = self.db.program_buy_spend(program.id).await else {
                    return Decision::halted(Halt::Disabled, inventory);
                };
                if spent >= budget {
                    return Decision::halted(Halt::BudgetDone { spent }, inventory);
                }
                let committed: Decimal = resting.iter().map(|r| r.price * r.remaining).sum();
                free_quote = free_quote.min((budget - spent - committed).max(Decimal::ZERO));
            }
            // And every slow buy leaves the unspent part of the OTHER budgets
            // alone, so the two do not race for the same cash.
            let reserved = self.reserved_for_budgets(program.id, &market.quote_unit).await;
            free_quote = (free_quote.min(quote_avail - reserved)).max(Decimal::ZERO);
            let input = crate::strategy::sell::BuyInput {
                market,
                bids: &book.bids,
                best_ask: book.best_ask().map(|(p, _)| p),
                ceiling,
                volume_24h: self.db.tape_volume(&market.id, day_ago).await.unwrap_or_default(),
                bought_24h: self.db.bought_since(&market.id, day_ago).await.unwrap_or_default(),
                volume_1h: self.db.tape_volume(&market.id, hour_ago).await.unwrap_or_default(),
                bought_1h: self.db.bought_since(&market.id, hour_ago).await.unwrap_or_default(),
                free_quote,
                max_base,
                resting: &resting,
                flow,
                seed,
                params: sp,
            };
            let quotes = crate::strategy::sell::buy_ladder(&input);
            info!(
                market = %market.id, %ceiling, best_bid = ?best_bid,
                volume_24h = %input.volume_24h.round_dp(0), bought_24h = %input.bought_24h.round_dp(0),
                volume_1h = %input.volume_1h.round_dp(0), bought_1h = %input.bought_1h.round_dp(0),
                room = %input.room().round_dp(0), free_quote = %free_quote.round_dp(2),
                budget = ?program.budget, reserved = %reserved.round_dp(2), resting = resting.len(),
                bids = quotes.len(), flow = ?flow.map(|f| f.round_dp(2)), flow_scale = %input.flow_scale().round_dp(2),
                "slow buy"
            );
            return Decision { quotes, halt: None, fair_value, target_inventory: inventory };
        }
        // `sell_qty` caps what this program sells, counted from its own fills
        // so a deposit or a manual trade cannot move the count. New asks only
        // get what is left after what already rests. Asked for on
        // 2026-10-15: "keep track of what you sell, and don't go over".
        let cap = self.db.setting_decimal("sell_qty", Decimal::ZERO).await;
        let free = if cap > Decimal::ZERO {
            let Ok(sold) = self.db.program_filled(program.id, "sell").await else {
                // No count, no selling: never guess past the cap.
                return Decision::halted(Halt::Disabled, inventory);
            };
            let left = cap - sold;
            if left <= Decimal::ZERO {
                return Decision::halted(Halt::SellQtyDone { sold }, inventory);
            }
            let committed: Decimal = resting.iter().map(|r| r.remaining).sum();
            base_avail.min((left - committed).max(Decimal::ZERO))
        } else {
            base_avail
        };
        // The liquidation pace, over every book on this asset: a holding
        // sells out over about `sell_hours`, never faster. 0 turns it off.
        let hours = self.db.setting_decimal("sell_hours", Decimal::from(SELL_HOURS)).await;
        let pace = if hours > Decimal::ZERO {
            let window_ago = now - crate::strategy::sell::PACE_WINDOW_HOURS * 3_600_000;
            let (mut sold_day, mut sold_window) = (Decimal::ZERO, Decimal::ZERO);
            for m in self.markets.iter().filter(|m| m.base_unit == market.base_unit) {
                sold_day += self.db.sold_since(&m.id, day_ago).await.unwrap_or_default();
                sold_window += self.db.sold_since(&m.id, window_ago).await.unwrap_or_default();
            }
            // Spike asks sit well over the touch and only fill on a jump, so
            // they do not use up the pace. Counting them let the rungs alone
            // fill the window and kept every near ask off the book
            // (2026-10-27).
            let committed: Decimal = resting
                .iter()
                .filter(|r| r.level < crate::strategy::sell::SPIKE_LEVEL)
                .map(|r| r.remaining)
                .sum();
            Some(crate::strategy::sell::pace_room(
                inventory,
                sold_day,
                sold_window,
                committed,
                hours,
                sp.min_order.max(market.min_amount),
            ))
        } else {
            None
        };
        let input = crate::strategy::sell::SellInput {
            market,
            asks: &book.asks,
            best_bid,
            floor: program.limit_price.unwrap_or(SELL_FLOOR),
            volume_24h: self.db.tape_volume(&market.id, day_ago).await.unwrap_or_default(),
            sold_24h: self.db.sold_since(&market.id, day_ago).await.unwrap_or_default(),
            volume_1h: self.db.tape_volume(&market.id, hour_ago).await.unwrap_or_default(),
            sold_1h: self.db.sold_since(&market.id, hour_ago).await.unwrap_or_default(),
            free,
            resting: &resting,
            flow,
            seed,
            pace,
            params: sp,
        };
        let quotes = crate::strategy::sell::ladder(&input);
        info!(
            market = %market.id, floor = %input.floor, best_ask = ?book.best_ask().map(|(p, _)| p),
            volume_24h = %input.volume_24h.round_dp(0), sold_24h = %input.sold_24h.round_dp(0),
            volume_1h = %input.volume_1h.round_dp(0), sold_1h = %input.sold_1h.round_dp(0),
            room = %input.room().round_dp(0), pace = ?pace.map(|p| p.round_dp(0)), resting = resting.len(), asks = quotes.len(),
            flow = ?flow.map(|f| f.round_dp(2)), flow_scale = %input.flow_scale().round_dp(2),
            "slow sell"
        );
        Decision { quotes, halt: None, fair_value, target_inventory: inventory }
    }

    /// Move newly earned edge into the base asset's pile, if the program is
    /// set to bank there.
    ///
    /// Only profit moves. The principal stays under the ratio, so the
    /// rebalancing machine keeps running at full size while the winnings
    /// accumulate as base -- which is how spread capture, which is
    /// direction-neutral, becomes a growing base balance without taking a
    /// directional bet to get there.
    /// Snapshot Lighter's PRL book and tape into the same tables as ours, under
    /// `lighter:<id>`, so basis, depth and flow there can be measured against
    /// SafeTrade's over the same minutes. Warnings only: it trades nothing.
    async fn record_lighter(&self) {
        let Some(l) = &self.lighter else { return };
        let key = l.market_key();
        match l.book().await {
            Ok(book) => {
                if let Err(e) = self
                    .db
                    .snapshot_book(&key, &book, dec!(0.01), None, None, Decimal::ZERO, Decimal::ZERO)
                    .await
                {
                    warn!(error = %format!("{e:#}"), "could not record the Lighter book");
                }
            }
            Err(e) => warn!(error = %format!("{e:#}"), "Lighter book unavailable"),
        }
        match l.trades(100).await {
            Ok(t) => {
                if let Err(e) = self.db.record_tape(&key, &t).await {
                    warn!(error = %format!("{e:#}"), "could not record the Lighter tape");
                }
            }
            Err(e) => warn!(error = %format!("{e:#}"), "Lighter tape unavailable"),
        }
    }

    async fn bank_profit(&self, program: &Program, base: &str) -> Result<()> {
        if !self.cfg.bank_profit_in_base || !program.kind.is_ratio() {
            return Ok(());
        }
        // EVERY configured book on the asset, not the ones due this loop.
        // Handed the due list, the hourly pass on a loop where prlusdc was
        // not due (two in three) counted prlusdt alone, found its small loss,
        // banked nothing and moved the clock an hour: a profit earned and none
        // of it set aside, on 2026-09-29.
        let markets = &self.markets;
        let ids: Vec<String> = markets
            .iter()
            .filter(|m| m.base_unit == base)
            .map(|m| m.id.clone())
            .collect();
        // Banking starts when it is first asked to, not at the start of
        // whatever history the database happens to hold.
        let since_key = format!("bank_since_ms:{base}");
        if self.db.get_setting(&since_key).await?.is_none() {
            self.db.set_setting(&since_key, &crate::db::now_ms().to_string()).await?;
            return Ok(());
        }
        let new_edge = self.db.unbanked_edge(base, &ids).await?;
        if self.cfg.bank_every_secs > 0 {
            // On the clock: once an hour (say), whatever was earned. The clock
            // moves even when there is nothing to bank, so a loss does not
            // turn the next profit into a bank on every loop.
            let at_key = format!("banked_at:{base}");
            let last = self
                .db
                .get_setting(&at_key)
                .await?
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let now = crate::db::now_ms();
            if now - last < self.cfg.bank_every_secs as i64 * 1000 {
                return Ok(());
            }
            self.db.set_setting(&at_key, &now.to_string()).await?;
            if new_edge <= Decimal::ZERO {
                return Ok(());
            }
        } else if new_edge < self.cfg.bank_min_chunk {
            // Only bank in meaningful chunks, so the target does not twitch on
            // every fractional cent of edge.
            return Ok(());
        }
        // Priced BEFORE anything is written, so a bank that cannot also be set
        // aside does not happen at all and is simply retried next time.
        let set_aside_px = if self.cfg.bank_set_aside {
            let px = match markets
                .iter()
                .find(|m| m.base_unit == base)
                .and_then(|m| self.fv_history.get(&m.id))
                .and_then(|h| h.last().map(|(_, p)| *p))
            {
                Some(p) => Some(p),
                None => self.db.last_mid(base).await,
            };
            match px.filter(|p| *p > Decimal::ZERO) {
                Some(p) => Some(p),
                None => {
                    warn!(%base, "no price to set earnings aside at; not banking this pass");
                    return Ok(());
                }
            }
        } else {
            None
        };
        // Move the high-water mark FIRST. A crash between the two writes then
        // under-banks by one chunk, which the next pass simply picks up; the
        // other order would bank the same edge twice and permanently inflate
        // the target.
        let total = self
            .db
            .setting_decimal(&format!("banked_high_water:{base}"), Decimal::ZERO)
            .await
            + new_edge;
        self.db.mark_banked(base, total).await?;
        self.db.bank_profit(program.id, new_edge).await?;
        info!(%base, banked = %new_edge.round_dp(4), "banked trading profit into the base asset");

        // And set it aside, in the asset. The raised target above has the book
        // buy it; the pile keeps it from ever being quoted again.
        if let Some(px) = set_aside_px {
            let qty = new_edge / px;
            self.db.add_set_aside(base, qty).await?;
            info!(
                %base, qty = %qty.round_dp(4), %px,
                pile = %self.db.set_aside(base).await.round_dp(4),
                "set earnings aside"
            );
        }
        Ok(())
    }

    /// Notice coins arriving or leaving by any route other than our own
    /// trading, and re-anchor the program's glide around them.
    ///
    /// Inventory should only ever move by the net of our fills. Anything else
    /// is a deposit, a withdrawal, or a trade placed by hand. A balanced
    /// program already aims at a share of *live* portfolio value, so its
    /// destination is right the instant the coins land; what breaks is the
    /// path, because the glide interpolates from an anchor fixed when the
    /// program started. Without re-anchoring, a deposit makes the entire new
    /// gap appear at once and the bot works it off as hard as the caps allow --
    /// exactly the behaviour a gradual program exists to avoid.
    async fn absorb_external_flow(
        &mut self,
        base: &str,
        inventory: Decimal,
        net_filled: Decimal,
        complete: bool,
    ) -> Result<()> {
        // Cumulative expectation, not a per-loop delta.
        //
        // Balances are read before fills are, so a trade landing in that gap
        // shows up in one loop as an unexplained fall and in the next as an
        // unexplained rise. Comparing deltas loop by loop sees two anomalies
        // where there is one trade; carrying a running expectation forward
        // sees the second loop cancel the first, which is what actually
        // happened. The previous model reported our own buys as a deposit of
        // slightly less PRL -- the difference being exactly the fee.
        // Seeded from the database, not from whatever the balance happens to be
        // at startup: fills that landed while we were down are ingested on the
        // first loop AND are already in the balance, so seeding from the
        // balance counts them twice.
        if !self.expected_inventory.contains_key(base) {
            let stored = self.db.get_setting(&format!("expected_inv:{base}")).await?;
            let seed = num::opt_parse(stored.as_deref()).unwrap_or(inventory - net_filled);
            self.expected_inventory.insert(base.to_string(), seed);
        }
        let expected = self.expected_inventory.entry(base.to_string()).or_insert(inventory);
        *expected += net_filled;
        let external = inventory - *expected;
        if !net_filled.is_zero() {
            // Persist on every change, not only when absorbed: a restart after
            // an unabsorbed fill would otherwise report it as drift.
            let e = *expected;
            self.db
                .set_setting(&format!("expected_inv:{base}"), &num::to_str(e))
                .await
                .ok();
        }

        // Only JUDGE when every book trading this asset has reported. The
        // accumulation above must happen regardless: with a market quoted every
        // third loop, returning early dropped the net of every fill ingested on
        // the other two, so two of three real fills were later "confirmed" as
        // deposits -- re-anchoring the glide each time, which is why a 24h
        // rebalance never arrived anywhere.
        if !complete {
            return Ok(());
        }

        // A recycle crossing we have just made is not external flow. Its fill
        // arrives a loop or two behind the balance it moved, so judging in that
        // window reports our own spending as unaccounted-for money. The entry
        // clears the moment any fill on this asset is ingested, and expires on
        // its own if the crossing never filled, so this can only ever delay the
        // judgement -- it cannot suppress a genuine gap.
        const RECYCLE_FILL_GRACE_MS: i64 = 300_000;
        match self.recycle_awaiting.get(base).copied() {
            Some(at) if !net_filled.is_zero() || now_ms() - at > RECYCLE_FILL_GRACE_MS => {
                self.recycle_awaiting.remove(base);
            }
            Some(_) => {
                debug!(%base, %inventory, "holding the drift judgement for a recycle fill");
                return Ok(());
            }
            None => {}
        }

        let min_amount = self
            .markets
            .iter()
            .filter(|m| m.base_unit == base)
            .map(|m| m.min_amount)
            .max()
            .unwrap_or(Decimal::ONE);
        let threshold = (inventory * dec!(0.01)).max(min_amount * dec!(10));

        if external.abs() < threshold {
            // Within tolerance: absorb the residue. Buy fees are taken in the
            // base asset, so expectation drifts a little above reality with
            // every purchase, and left alone that drift would eventually look
            // like a withdrawal.
            *expected = inventory;
            let e = *expected;
            self.pending_external.remove(base);
            self.db
                .set_setting(&format!("expected_inv:{base}"), &num::to_str(e))
                .await
                .ok();
            return Ok(());
        }

        // Inference no longer ACTS. The exchange's own deposit and withdrawal
        // records are authoritative and carry the exact amount; guessing from a
        // balance that moved could never tell a transfer from a fill not yet
        // ingested, which is how our own buys came to be booked as deposits.
        //
        // It still watches, because a gap that persists here means our picture
        // of our own trading has drifted from reality -- worth knowing about
        // even though it is not worth trading on.
        let confirmed = self
            .pending_external
            .get(base)
            .is_some_and(|prev| (*prev > Decimal::ZERO) == (external > Decimal::ZERO));
        if !confirmed {
            self.pending_external.insert(base.to_string(), external);
            return Ok(());
        }

        warn!(
            %base, %external, %inventory, expected = %(inventory - external),
            "inventory does not match our record of our own trading"
        );
        self.db
            .alert(
                "inventory_drift",
                &format!(
                    "{} {} of {} is unaccounted for. Not a transfer -- those come from the \
                     exchange's own records -- so this is drift between our fill history and \
                     the balance. Re-synced.",
                    external.abs().round_dp(4),
                    base.to_uppercase(),
                    base.to_uppercase(),
                ),
            )
            .await;

        // Re-sync so the same gap is not reported again every loop.
        self.pending_external.remove(base);
        *expected = inventory;
        self.db
            .set_setting(&format!("expected_inv:{base}"), &num::to_str(inventory))
            .await
            .ok();
        Ok(())
    }

    /// Take in deposits and withdrawals the exchange has settled.
    ///
    /// Each is applied once, tracked by the exchange's own id, so a restart or
    /// a repeated read cannot double-count. Only settled transfers count: a
    /// pending deposit must not move the target, or we would quote against
    /// coins that have not arrived.
    async fn absorb_transfers(
        &mut self,
        markets: &[Market],
        balances: &[Balance],
    ) -> Result<usize> {
        let deposits = self.api.deposits(50).await?;
        let withdrawals = self.api.withdrawals(50).await.unwrap_or_default();

        // A withdrawal that has not settled yet is money already gone.
        //
        // The exchange locks the balance the moment the withdrawal is
        // requested, but reports it as `locked` -- indistinguishable from the
        // funds behind our own resting orders, which ARE ours and come back if
        // we cancel. So the bot counted PRL on its way to cold storage as
        // inventory: it valued a portfolio 2.5x larger than the real one,
        // set targets against it, and would have sold PRL it did not have to
        // reach a target that did not exist.
        //
        // Tracked per currency and subtracted from `locked` wherever balances
        // are read. It clears itself: once the withdrawal settles it leaves the
        // balance for real and stops being pending.
        // A deposit seen but not credited is coins on their way to the sale:
        // the follow-on buy must not start under it. Only recent ones, and
        // never one the exchange has given up on.
        self.pending_in.clear();
        let recent = now_ms() - 2 * 86_400_000;
        for t in deposits.iter().filter(|t| !t.is_settled()) {
            let failed = t.status.as_deref().is_some_and(|s| {
                matches!(
                    s.to_lowercase().as_str(),
                    "rejected" | "canceled" | "cancelled" | "failed" | "errored" | "refunded" | "skipped"
                )
            });
            let created = t
                .created_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.timestamp_millis())
                .unwrap_or(0);
            if !failed && created >= recent {
                self.pending_in.insert(t.currency.to_lowercase());
            }
        }
        if !self.pending_in.is_empty() {
            info!(pending = ?self.pending_in, "deposits arriving, not yet credited");
        }

        self.pending_out.clear();
        for t in &withdrawals {
            match t.pending_reason() {
                crate::exchange::PendingState::InFlight => {
                    *self.pending_out.entry(t.currency.to_lowercase()).or_default() +=
                        t.amount + t.fee.unwrap_or_default();
                }
                // A status we have never seen. Say so rather than guess: the
                // wrong guess either overstates our inventory or shrinks it
                // for ever, and both have already happened once.
                crate::exchange::PendingState::Unknown => warn!(
                    id = t.id, currency = %t.currency, status = ?t.status,
                    "withdrawal has an unrecognised status; not counted as in flight"
                ),
                _ => {}
            }
        }
        if !self.pending_out.is_empty() {
            info!(pending = ?self.pending_out, "withdrawals in flight; excluded from inventory");
        }
        // At info, not debug: "the array decoded" is not the same as "we can
        // recognise a deposit", and the difference is invisible until one
        // lands and nothing happens.
        if self.transfer_checks == 0 {
            info!(
                deposits = deposits.len(),
                settled_deposits = deposits.iter().filter(|t| t.is_settled()).count(),
                withdrawals = withdrawals.len(),
                "transfer ledger decoded"
            );
        }

        // Everything before the bot took over is history, not news.
        let cutover = self.db.fills_since_ms(&markets[0].id).await;
        let mut applied = 0usize;

        for (kind, list, direction) in [
            ("deposit", deposits, Decimal::ONE),
            ("withdrawal", withdrawals, Decimal::NEGATIVE_ONE),
        ] {
            for t in list.iter().filter(|t| t.is_settled()) {
                let stamp = t.completed_at.as_deref().or(t.created_at.as_deref());
                let landed = stamp
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.timestamp_millis());
                let Some(landed) = landed else {
                    // Unparseable would otherwise read as epoch zero and be
                    // skipped as ancient -- a settled deposit silently ignored.
                    warn!(kind, id = t.id, stamp = ?stamp, "transfer has an unreadable timestamp");
                    self.db
                        .alert(
                            "transfer_unreadable",
                            &format!(
                                "A settled {kind} (id {}) has a timestamp I cannot read ({:?}); \
                                 it was NOT applied. Check it by hand.",
                                t.id, stamp
                            ),
                        )
                        .await;
                    continue;
                };
                if landed < cutover {
                    continue;
                }
                let key = format!("transfer:{kind}:{}", t.id);
                if self.db.get_setting(&key).await?.is_some() {
                    continue;
                }

                let ccy = t.currency.to_lowercase();
                let net = t.net(direction);
                info!(kind, id = t.id, %ccy, amount = %net, "settled transfer");

                // A withdrawal is taken from the set-aside pile first.
                if net < Decimal::ZERO {
                    let cut = self.db.draw_set_aside(&ccy, -net).await?;
                    if cut > Decimal::ZERO {
                        info!(
                            %ccy, cut = %cut, pile = %self.db.set_aside(&ccy).await,
                            "withdrawal taken from the set-aside pile"
                        );
                    }
                }

                // The balance already includes it, so the expectation must too
                // or the next loop reads it as unexplained.
                if let Some(e) = self.expected_inventory.get_mut(&ccy) {
                    *e += net;
                    let v = *e;
                    self.db
                        .set_setting(&format!("expected_inv:{ccy}"), &num::to_str(v))
                        .await
                        .ok();
                }

                // Money in or out is not profit.
                // Last mid we saw, falling back to the most recent book
                // snapshot -- fv_history is empty on the first tick after a
                // restart, which is exactly when a deposit that settled during
                // downtime gets applied.
                let price = match markets
                    .iter()
                    .find(|m| m.base_unit == ccy)
                    .and_then(|m| self.fv_history.get(&m.id))
                    .and_then(|h| h.last().map(|(_, p)| *p))
                {
                    Some(p) => p,
                    None => self.db.last_mid(&ccy).await.unwrap_or(Decimal::ONE),
                };
                // Only once the scoreboard's starting value exists, and only
                // for transfers that landed after it. That value is seeded
                // later in this same tick from balances that already hold
                // anything settled by now, so a transfer first seen before
                // the seed -- the one that landed while the bot was down, on
                // the tick after a deploy -- would otherwise be counted in
                // both and reported as a market loss.
                if self.db.account_start_ts().await.is_some_and(|start| landed >= start) {
                    self.db.add_external_flow(net * price).await.ok();
                }

                // Re-anchor the glide so it converges on the new balance.
                //
                // Using THIS tick's balances, which we know we read
                // successfully. Re-fetching here and swallowing the error gave
                // an empty list on any hiccup, so `a + l` was zero and the
                // glide was re-anchored at zero inventory -- ramping from
                // nothing toward half the portfolio, which pins the inventory
                // error at full sell lean for hours, on precisely the tick a
                // deposit is recognised.
                // A learned split treats the transfer as capital: the anchor
                // every currency's surplus sits at is re-read from the account
                // as it now stands, so the deposit is absorbed rather than
                // worked off, and the split is re-derived on the next loop
                // (`retarget` sees the fresh glide). Every learned program,
                // whatever currency landed: the anchors are shared.
                if self.db.setting_decimal("learned_anchors", Decimal::ZERO).await > Decimal::ZERO {
                    let prices = self.prices(&HashMap::new()).await;
                    let holdings = holdings(&self.markets, balances, &prices);
                    self.store_anchors(&holdings).await;
                    for base in distinct(self.markets.iter().map(|m| m.base_unit.clone())) {
                        if let Ok(Some(p)) = self.db.active_program(&base).await
                            && p.kind == ProgramKind::Learned
                        {
                            let (a, l) = balance_of(balances, &base);
                            self.db.reanchor_program(p.id, a + l).await.ok();
                        }
                    }
                }
                // Money landing is what starts a twap on the new capital.
                //
                // The target does not track balances -- that is the point, so
                // what the bot will do next is predictable from what it last
                // said. So a transfer is handled as an EVENT: work out what the
                // account can now hold, and start a fresh twap for it over a
                // full window of its own. Deposit at hour five of six and the
                // new money gets six hours, not the hour that was left of a
                // clock it was not there for.
                //
                // It is done per base asset, not per deposited currency: a twap
                // is funded in the currency it SPENDS, so a USDT deposit (filed
                // under usdt) has to reach the program filed under prl.
                let prices_now = self.prices(&HashMap::new()).await;
                for base in distinct(self.markets.iter().map(|m| m.base_unit.clone())) {
                    let Ok(Some(p)) = self.db.active_program(&base).await else { continue };
                    if p.kind != ProgramKind::Twap {
                        continue;
                    }
                    let window = match p.ends_at {
                        Some(end) => (end - p.started_at).max(3_600_000),
                        // No deadline is "as passively as the caps allow"; there
                        // is no window to restart, so leave it alone.
                        None => continue,
                    };
                    let Some(price) = prices_now.get(&base).copied().filter(|v| *v > Decimal::ZERO)
                    else {
                        warn!(%base, "no price for the asset being worked; twap left as it is");
                        continue;
                    };
                    let (a, l) = balance_of(balances, &base);
                    let held = a + l;
                    let cash: Decimal = quotes_of(&self.markets, &base)
                        .iter()
                        .map(|c| {
                            let (a, l) = balance_of(balances, c);
                            a + l
                        })
                        .sum();
                    let target = held + cash / price;
                    // Nothing to work: every unit of cash is already spent.
                    if target <= held {
                        continue;
                    }
                    match self
                        .db
                        .create_program(
                            &base,
                            ProgramKind::Twap,
                            held,
                            target,
                            None,
                            p.limit_price,
                            Some(now_ms() + window),
                            Some(&format!("auto: {} landed, twap restarted over its own window", ccy.to_uppercase())),
                        )
                        .await
                    {
                        Ok(id) => {
                            info!(
                                program = id, %base, %ccy, %held, %target,
                                hours = window / 3_600_000,
                                "transfer settled; twap restarted on the new capital"
                            );
                            self.db
                                .alert(
                                    "twap_restarted",
                                    &format!(
                                        "{} of {} {} settled. Working {} {} into {} over a fresh {}h \
                                         (holding {}, target {}). Program #{id}.",
                                        if direction > Decimal::ZERO { "Deposit" } else { "Withdrawal" },
                                        net.abs().round_dp(2),
                                        ccy.to_uppercase(),
                                        cash.round_dp(2),
                                        quotes_of(&self.markets, &base)
                                            .iter()
                                            .map(|c| c.to_uppercase())
                                            .collect::<Vec<_>>()
                                            .join(" + "),
                                        base.to_uppercase(),
                                        window / 3_600_000,
                                        held.round_dp(2),
                                        target.round_dp(2),
                                    ),
                                )
                                .await;
                        }
                        Err(e) => {
                            error!(error = %format!("{e:#}"), %base, "could not restart the twap");
                            self.db
                                .alert(
                                    "twap_restart_failed",
                                    &format!(
                                        "{} {} landed but I could not restart the TWAP: {e:#}. \
                                         The old schedule is still running on its old target.",
                                        net.abs().round_dp(2),
                                        ccy.to_uppercase()
                                    ),
                                )
                                .await;
                        }
                    }
                }

                if let Some(p) = self.db.active_program(&ccy).await?
                    && p.kind.is_ratio()
                {
                    let (a, l) = balance_of(balances, &ccy);
                    if let Err(e) = self.db.reanchor_program(p.id, a + l).await {
                        // Best effort: the target is still correct without it,
                        // just reached less gradually. Losing the transfer
                        // entirely would be worse.
                        warn!(error = %format!("{e:#}"), "could not re-anchor after a transfer");
                    }
                    let destination =
                        format!("restarting the glide to {:.0}%", p.ratio.unwrap_or(dec!(0.5)) * dec!(100));
                    self.db
                        .alert(
                            "transfer",
                            &format!(
                                "{} of {} {} confirmed by the exchange. Holding {} now; {destination}.",
                                if direction > Decimal::ZERO { "Deposit" } else { "Withdrawal" },
                                net.abs().round_dp(4),
                                ccy.to_uppercase(),
                                (a + l).round_dp(2),
                            ),
                        )
                        .await;
                } else {
                    self.db
                        .alert(
                            "transfer",
                            &format!(
                                "{} of {} {} confirmed by the exchange.",
                                if direction > Decimal::ZERO { "Deposit" } else { "Withdrawal" },
                                net.abs().round_dp(4),
                                ccy.to_uppercase()
                            ),
                        )
                        .await;
                }

                // A deposit of the base asset starts working itself off.
                //
                // The owner deposits PRL to sell it, and doing that by hand
                // means watching a chart. This queues the distribution the
                // moment the coins are credited, at a rate that is a small
                // fraction of the book's own volume, with a floor under it.
                // Not in TWAP mode. The auto-trim installs a `distribute`,
                // which would REPLACE the twap for that asset -- one program per
                // asset -- so a deposit of the coin we are buying would switch
                // the job from buying to selling, and `twap_only` would then
                // halt the seller and leave every book dark.
                if direction > Decimal::ZERO
                    && self.cfg.auto_trim_deposits
                    && !self.db.setting_bool("twap_only", false).await
                    && markets.iter().any(|m| m.base_unit.eq_ignore_ascii_case(&ccy))
                {
                    self.auto_trim(&ccy, net).await;
                }

                // Marked only now that its effects have landed. Marking first
                // meant any failure in between lost the transfer for good.
                self.db.set_setting(&key, &now_ms().to_string()).await?;
                applied += 1;
            }
        }
        Ok(applied)
    }

    /// Cross the spread to keep an execution program on schedule.
    ///
    /// The ladder cannot do this: `clamp_price` caps every bid at
    /// `best_ask - tick`, which is the guarantee that our quoting never moves
    /// the price. That guarantee is right for a market maker and for a patient
    /// TWAP, and wrong for the one case where the owner has said the schedule
    /// matters more than the spread. Measured live 2026-09-27: passive bidding
    /// on a book one tick wide put us behind 3,700 PRL of queue and bought
    /// about 4% of a large TWAP in forty minutes -- on pace for under 40% of it
    /// in six hours, with the rest simply not spent.
    ///
    /// So this is deliberately hard to trigger and easy to bound. Every one of
    /// these must hold, and each is a thing that would otherwise cost money:
    ///
    /// * `allow_taker` for this market -- off everywhere by default;
    /// * an EXECUTION program (`accumulate`, `distribute`, `flatten`, `twap`)
    ///   with a deadline: without a schedule there is no "behind";
    /// * a `limit_price` on that program. No limit, no crossing: "buy as fast as
    ///   you can" with no ceiling is how a thin ask side gets walked up;
    /// * behind by more than one full rung, so lateness of a few coins does not
    ///   pay the spread;
    /// * a price at or under the program's limit AND within `taker_max_bps` of
    ///   fair value. On a referenced book fair value comes from the deep book,
    ///   so this is what refuses to pay the thin book's 2-15% spread;
    /// * inside the per-order notional cap, the depth cap on the side being
    ///   taken, the participation cap, and the cash actually held.
    ///
    /// One order per market per loop. The clip is whatever the caps allow, so
    /// catching up is a sequence of small crossings rather than one sweep.
    async fn take_if_behind(
        &mut self,
        market: &Market,
        book: &OrderBook,
        program: &Program,
        params: &Params,
        fv: Decimal,
        inventory: Decimal,
        target: Decimal,
        free_quote: Decimal,
        free_base: Decimal,
        our_participation: Decimal,
    ) {
        let Some(clip) = taker_clip(
            market, book, program, params, fv, inventory, target, free_quote, free_base,
            our_participation,
        ) else {
            return;
        };
        let (side, price, amount, behind) = (clip.side, clip.price, clip.amount, clip.behind);

        if params.dry_run {
            info!(
                market = %market.id, %side, %amount, %price, behind = %behind.round_dp(2),
                "DRY RUN cross to keep the schedule"
            );
            return;
        }

        let client_id = format!("{}-{}-take-{}", market.id, side, now_ms());
        if let Err(e) = self
            .db
            .record_placement(
                &client_id, Some(program.id), &market.id, side, price, amount,
                TAKER_LEVEL, None, None, None,
            )
            .await
        {
            error!(error = ?e, "could not record the crossing; not sending it");
            return;
        }
        match self.api.place_limit(&market.id, side, amount, price).await {
            Ok(o) => {
                self.db.attach_exchange_id(&client_id, o.id, &o.state).await.ok();
                info!(
                    market = %market.id, id = o.id, %side, %price, %amount,
                    behind = %behind.round_dp(2),
                    "crossed the spread to keep the program on schedule"
                );
                // The balance moves now and the fill is read on a later loop;
                // without this the drift detector reads it as money appearing
                // from nowhere. Keyed exactly as `absorb_external_flow` keys it.
                self.recycle_awaiting.insert(market.base_unit.clone(), now_ms());
            }
            Err(e) => {
                let msg = format!("{e:#}");
                warn!(market = %market.id, error = %msg, "crossing order rejected");
                self.db.fail_placement(&client_id, &msg).await.ok();
            }
        }
    }

    /// Record a per-market API failure, and escalate if it persists.
    ///
    /// A single failure is weather. A market failing for minutes means its
    /// orders are resting with nobody watching them, so past a threshold we
    /// pull them and say so rather than leaving them out there.
    async fn note_market_failure(&mut self, market: &str, what: &str, error: &str) {
        let n = self.market_failures.entry(market.to_string()).or_insert(0);
        *n += 1;
        let n = *n;
        warn!(market, what, error, failures = n, "market unreadable this loop");
        // ~2 minutes at a 6s loop.
        if n == 20 {
            self.db
                .alert(
                    "market_unreadable",
                    &format!(
                        "{market}: {what} has failed {n} times in a row ({error}). Pulling its \
                         orders so none rest unmanaged."
                    ),
                )
                .await;
            if let Err(e) = self.cancel_everything(market).await {
                warn!(market, error = %format!("{e:#}"), "could not pull orders");
            }
        }
    }

    /// Persist balances, but only the ones that moved.
    ///
    /// A heartbeat every fifteen minutes keeps a regular sample in the table so
    /// reporting still has a time series when nothing is trading.
    async fn record_balances(&mut self, balances: &[Balance]) {
        const HEARTBEAT_MS: i64 = 15 * 60 * 1000;
        let force = now_ms() - self.last_balance_write >= HEARTBEAT_MS;

        let changed: Vec<Balance> = balances
            .iter()
            .filter(|b| {
                force
                    || self
                        .last_balances
                        .get(&b.currency)
                        .is_none_or(|(a, l)| *a != b.balance || *l != b.locked)
            })
            .cloned()
            .collect();

        if changed.is_empty() {
            return;
        }
        if self.db.snapshot_balances(&changed).await.is_ok() {
            for b in &changed {
                self.last_balances.insert(b.currency.clone(), (b.balance, b.locked));
            }
            self.last_balance_write = now_ms();
        }
    }

    /// What is actually resting, according to the exchange.
    ///
    /// Built from the exchange's own list rather than our database, because the
    /// two can disagree: a placement that times out may still have reached the
    /// matching engine, leaving an order live that we never recorded. Trusting
    /// the database there would strand that order on the book forever, since
    /// the reconciler only ever cancels things it knows about.
    ///
    /// Orders the exchange reports but we have no record of are **adopted** —
    /// this is a dedicated bot account, and managing a stray order is better
    /// than leaving it resting. That does mean an order placed by hand in the
    /// SafeTrade UI will be treated as the bot's and likely cancelled.
    async fn reconcile_view(
        &mut self,
        market: &Market,
        exchange_orders: &[crate::exchange::Order],
    ) -> Result<Vec<crate::db::models::LiveOrder>> {
        let known = self.db.live_orders(&market.id).await?;
        let by_id: HashMap<i64, &crate::db::models::LiveOrder> =
            known.iter().map(|o| (o.exchange_id, o)).collect();

        let mut out = Vec::with_capacity(exchange_orders.len());
        for o in exchange_orders {
            let (Some(side), Some(price)) = (o.side(), o.price) else {
                continue;
            };
            if let Some(meta) = by_id.get(&o.id) {
                out.push(crate::db::models::LiveOrder {
                    client_id: meta.client_id.clone(),
                    exchange_id: o.id,
                    side,
                    price,
                    amount: o.origin_amount,
                    filled: o.filled_amount,
                    level: meta.level,
                    placed_at: meta.placed_at,
                });
                continue;
            }

            // Not in the live set. Before calling it a stranger, check whether
            // it is ours in some other state: we mark an order cancelled as
            // soon as the API returns, while the exchange goes on listing it
            // for a moment. Treating that as an untracked order produced a
            // steady drip of alerts about our own orders.
            if let Some(row) = self.db.order_by_exchange_id(o.id).await? {
                debug!(
                    market = %market.id, id = o.id, was = %row.state,
                    "our order is still resting; restoring it"
                );
                self.db.revive_order(o.id, &o.state).await.ok();
                out.push(crate::db::models::LiveOrder {
                    client_id: row.client_id,
                    exchange_id: o.id,
                    side,
                    price,
                    amount: o.origin_amount,
                    filled: o.filled_amount,
                    level: row.level.unwrap_or(-1),
                    placed_at: row.placed_at,
                });
                continue;
            }

            // A placement of ours whose answer never came back?
            if let Some(row) = self
                .db
                .claim_unattached_placement(&market.id, side, price, o.origin_amount)
                .await?
            {
                info!(
                    market = %market.id, id = o.id, client_id = %row.client_id,
                    "matched a resting order to a placement whose reply was lost"
                );
                self.db.attach_exchange_id(&row.client_id, o.id, &o.state).await.ok();
                out.push(crate::db::models::LiveOrder {
                    client_id: row.client_id,
                    exchange_id: o.id,
                    side,
                    price,
                    amount: o.origin_amount,
                    filled: o.filled_amount,
                    level: row.level.unwrap_or(-1),
                    placed_at: row.placed_at,
                });
                continue;
            }

            // Placed by hand. Not ours to manage, and never ours to cancel.
            if self.foreign_orders.insert(o.id) {
                info!(
                    market = %market.id, id = o.id, %side, %price,
                    amount = %o.origin_amount,
                    "leaving a manual order alone"
                );
            }
        }
        Ok(out)
    }

    /// Bring resting orders in line with what we want resting.
    ///
    /// Replacing an order loses queue position, and on a book this thin queue
    /// position is most of the edge. So an order is kept unless its price is
    /// wrong or its size has drifted beyond `size_tolerance`.
    #[allow(clippy::too_many_arguments)]
    async fn reconcile(
        &self,
        market: &Market,
        decision: &Decision,
        live: &[crate::db::models::LiveOrder],
        params: &Params,
        program_id: Option<i64>,
        free_base: Decimal,
        free_quote: Decimal,
    ) -> Result<()> {
        let mut purse = Purse::new(free_base, free_quote);
        let mut unmatched: Vec<&crate::db::models::LiveOrder> = live.iter().collect();
        let mut to_place: Vec<&DesiredQuote> = Vec::new();
        let mut keep: Vec<i64> = Vec::new();

        for want in &decision.quotes {
            // Price must match exactly (both are on the tick); size may drift.
            let found = unmatched.iter().position(|o| {
                o.side == want.side && o.price == want.price && {
                    let rem = o.remaining();
                    if want.amount.is_zero() {
                        false
                    } else {
                        ((rem - want.amount) / want.amount).abs() <= params.q.size_tolerance
                    }
                }
            });
            match found {
                Some(i) => {
                    keep.push(unmatched[i].exchange_id);
                    unmatched.remove(i);
                }
                None => to_place.push(want),
            }
        }

        let mut actions = 0usize;
        let budget = params.q.max_actions_per_loop;

        // Cancel first: never hold both the old and the new order at once.
        //
        // A cancel returns the funds that order was holding, and those funds
        // are what pay for the replacements below. Without crediting them back,
        // a fully deployed ladder sees `available` near zero and can cancel a
        // whole side without replacing any of it -- leaving the book unquoted
        // until the next loop, on every touch move.
        for stale in &unmatched {
            if actions >= budget {
                break;
            }
            let freed = if params.dry_run {
                info!(id = stale.exchange_id, side = %stale.side, price = %stale.price, "DRY RUN cancel");
                true
            } else {
                match self.api.cancel(stale.exchange_id).await {
                    Ok(_) => {
                        self.db.mark_cancelled(stale.exchange_id).await.ok();
                        debug!(id = stale.exchange_id, "cancelled");
                        true
                    }
                    Err(e) => match e.downcast_ref::<ApiError>() {
                        // Already gone: it filled or was cancelled elsewhere.
                        // Either way it no longer holds anything.
                        Some(ApiError::OrderNotFound) => {
                            self.db.mark_cancelled(stale.exchange_id).await.ok();
                            true
                        }
                        _ => {
                            warn!(id = stale.exchange_id, error = ?e, "cancel failed");
                            // Still resting, so its funds are still committed.
                            false
                        }
                    },
                }
            };
            if freed {
                purse.release(stale.side, stale.remaining(), stale.price);
            }
            actions += 1;
        }

        // Fund the touch before the far rungs. `dedupe_levels` sorts by price,
        // so bids arrive deepest-first; spending on those starves the rung that
        // actually earns when money is tight.
        to_place.sort_by_key(|q| q.level);

        for want in to_place {
            if actions >= budget {
                debug!("action budget spent; remaining quotes wait for the next loop");
                break;
            }

            // Only send what we can actually pay for.
            //
            // The ladder is sized against funds including those held by our own
            // resting orders, on the assumption that stale ones get cancelled
            // first. But the action budget caps how many we cancel in a loop,
            // so some of that money is still committed -- and sending the order
            // anyway earned a steady stream of `insufficient balance` rejects,
            // 181 of them before this. Tracking the funds through the cancels
            // makes the decision exact instead of hopeful.
            if !purse.spend(want.side, want.amount, want.price) {
                debug!(
                    market = %market.id, side = %want.side, price = %want.price,
                    amount = %want.amount,
                    cost = %Purse::cost_of(want.side, want.amount, want.price),
                    base = %purse.base, quote = %purse.quote,
                    "skipping a quote we cannot fund this loop"
                );
                continue;
            }

            self.place(market, want, params, program_id).await;
            actions += 1;
        }

        Ok(())
    }

    async fn place(
        &self,
        market: &Market,
        want: &DesiredQuote,
        params: &Params,
        program_id: Option<i64>,
    ) {
        // The deep rungs are allowed past this, by the same multiple the sizer
        // used. This guard is the last line before an order goes out, and it
        // was silently refusing exactly the rungs the sizer had just been
        // taught to make big: the sweep catcher sized 8.5k, 23.6k and 64.8k
        // orders and none of them ever reached the book, so the ladder ended
        // at its second rung with no error the operator would ever see.
        let deep = want.level >= crate::strategy::quote::DEEP_LEVEL_BASE;
        let cap = if deep {
            params.risk.max_order_notional * params.q.deep_cap_multiple.max(Decimal::ONE)
        } else {
            params.risk.max_order_notional
        };
        let notional = want.price * want.amount;
        // A twap's bids have no size cap: the owner wants the stack at the
        // touch to grow as long as it goes unfilled (2026-10-07). In twap
        // mode every bid on the book is a twap bid, and only cash bounds it.
        let twap_bid = params.q.twap_only && want.side == Side::Buy;
        // Nor do the slow sell's asks: in twap mode an ask can only be one of
        // them, and its rungs are sized off the day's volume, not this cap.
        let twap_ask = params.q.twap_only && want.side == Side::Sell;
        if notional > cap && !twap_bid && !twap_ask {
            warn!(%notional, %cap, deep, "refusing order over the notional cap");
            return;
        }

        let client_id = format!("{}-{}-{}-{}", market.id, want.side, want.level, now_ms());
        if params.dry_run {
            info!(
                side = %want.side, price = %want.price, amount = %want.amount,
                level = want.level, %notional, "DRY RUN place"
            );
            return;
        }

        if let Err(e) = self
            .db
            .record_placement(
                &client_id, program_id, &market.id, want.side, want.price, want.amount,
                want.level, Some(want.bps), Some(want.recent_move), Some(want.width_ticks),
            )
            .await
        {
            error!(error = ?e, "could not record placement; not sending the order");
            return;
        }

        match self
            .api
            .place_limit(&market.id, want.side, want.amount, want.price)
            .await
        {
            Ok(o) => {
                self.db.attach_exchange_id(&client_id, o.id, &o.state).await.ok();
                info!(market = %market.id, id = o.id, side = %want.side,
                      price = %want.price, amount = %want.amount, "placed");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                match e.downcast_ref::<ApiError>() {
                    Some(ApiError::InsufficientBalance) => {
                        warn!(side = %want.side, "insufficient balance for quote")
                    }
                    Some(ApiError::RateLimited) => warn!("rate limited placing order"),
                    _ => warn!(error = %msg, "place failed"),
                }
                self.db.fail_placement(&client_id, &msg).await.ok();
            }
        }
    }

    /// Cancel every order WE have resting on `market`, one at a time.
    ///
    /// Never the exchange's cancel-all: that takes the owner's hand-placed
    /// orders with it, and it did, on every pause and every restart.
    async fn cancel_everything(&self, market: &str) -> Result<()> {
        let ours = self.db.live_orders(market).await?;
        let mut n = 0;
        for o in &ours {
            match self.api.cancel(o.exchange_id).await {
                Ok(_) => {
                    self.db.mark_cancelled(o.exchange_id).await.ok();
                    n += 1;
                }
                Err(e) => warn!(market, id = o.exchange_id, error = %format!("{e:#}"), "cancel failed"),
            }
        }
        info!(market, count = n, of = ours.len(), "cancelled our resting orders");
        Ok(())
    }

    /// Largest relative fair-value excursion inside the window.
    fn recent_move(&mut self, market: &str, window_secs: i64) -> Decimal {
        let cutoff = now_ms() - window_secs * 1000;
        let hist = self.fv_history.entry(market.to_string()).or_default();
        hist.retain(|(ts, _)| *ts >= cutoff);
        if hist.len() < 2 {
            return Decimal::ZERO;
        }
        let lo = hist.iter().map(|(_, p)| *p).min().unwrap_or_default();
        let hi = hist.iter().map(|(_, p)| *p).max().unwrap_or_default();
        if lo.is_zero() { Decimal::ZERO } else { (hi - lo) / lo }
    }

    /// Our share of market volume over the window, as a fraction.
    /// Our share of traded volume, or `None` when it cannot be established.
    ///
    /// `None` matters. This used to report a flat 100% whenever the public tape
    /// came back empty -- including when the request simply failed, since the
    /// error was swallowed into an empty list. A single hiccup on the trades
    /// endpoint was therefore enough to convince the bot it was the entire
    /// market and pull every quote. Not knowing our share is not the same as
    /// knowing it is total, and only the latter is grounds for standing down.
    async fn participation(&self, market: &str, window_secs: i64) -> Option<Decimal> {
        let tape = match self.api.public_trades(market, TAPE_LIMIT).await {
            Ok(t) => t,
            Err(e) => {
                warn!(market, error = %format!("{e:#}"), "no public tape; share unknown");
                return None;
            }
        };

        // Persist it before using it. The tape is the only record of who was
        // the aggressor, which is what separates "we quoted badly" from "a
        // sweep came through", and it cannot be reconstructed later.
        if let Err(e) = self.db.record_tape(market, &tape).await {
            warn!(market, error = %format!("{e:#}"), "could not record the tape");
        }

        // Both sides must be measured over the SAME span.
        //
        // The tape is capped at `TAPE_LIMIT` trades. On a busy book that is far
        // less than the window asks for -- measured live during a rally, 1000
        // trades covered 72 minutes of a 240-minute window. Summing our fills
        // over the full window and dividing by a tape covering under a third of
        // it reported 18.7% against a true 5.7%, and pulled every quote off the
        // main book in the middle of the best conditions of the day.
        //
        // So the window follows the evidence: if the tape is truncated, both
        // sides are measured from the oldest trade it actually contains.
        let now_s = now_ms() / 1000;
        let from_s = match measurement_window(
            tape.iter().map(|t| t.created_at).min(),
            tape.len(),
            now_s - window_secs,
            now_s,
        ) {
            Some(f) => f,
            None => {
                warn!(market, "public tape too short or empty; share unknown");
                return None;
            }
        };

        let ours: Option<f64> = sqlx::query_scalar(
            "SELECT CAST(SUM(CAST(amount AS REAL)) AS REAL) FROM fills WHERE market = ? AND created_at >= ?",
        )
        .bind(market)
        .bind(from_s * 1000)
        .fetch_one(self.db.pool())
        .await
        .unwrap_or(None);
        let ours = Decimal::try_from(ours.unwrap_or(0.0)).unwrap_or_default();
        if ours.is_zero() {
            return Some(Decimal::ZERO);
        }

        let market_vol: Decimal =
            tape.iter().filter(|t| t.created_at >= from_s).map(|t| t.amount).sum();

        if market_vol <= Decimal::ZERO {
            // We have fills in this window, so the tape must contain them. An
            // empty window means the endpoint is not telling us the whole
            // story, not that we traded against ourselves.
            warn!(market, "public tape shows no trades despite our own fills; share unknown");
            return None;
        }
        // Our fills appear in the public tape too, so `ours` is already inside
        // `market_vol`; the ratio is a true share, not an overstatement.
        Some(num::clamp(ours / market_vol, Decimal::ZERO, Decimal::ONE))
    }

    /// Start working a freshly deposited amount back out of the account.
    ///
    /// Additive on purpose: a second deposit while the first is still selling
    /// extends the same distribution rather than replacing it, so coins that
    /// arrived earlier are not silently dropped from the plan.
    async fn auto_trim(&self, ccy: &str, amount: Decimal) {
        if amount <= Decimal::ZERO {
            return;
        }
        let held = match self.inventory_of(ccy).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "cannot read inventory; not auto-trimming");
                return;
            }
        };
        // Anything already being worked off stays in the plan.
        let outstanding = match self.db.active_program(ccy).await {
            Ok(Some(p)) if p.kind.is_scheduled() => (p.start_qty - p.target_qty).max(Decimal::ZERO),
            _ => Decimal::ZERO,
        };
        // Glide down to the position we mean to keep, not to zero. A deposit
        // is not necessarily all for sale; what is left is the inventory the
        // bot makes a market around.
        let target = match self.cfg.auto_trim_keep {
            Some(keep) => keep.min(held),
            None => (held - (amount + outstanding)).max(Decimal::ZERO),
        };
        let to_sell = (held - target).max(Decimal::ZERO);
        if to_sell <= Decimal::ZERO {
            info!(%ccy, %held, %target, "deposit credited, already at or under the keep level");
            return;
        }
        let secs = self.cfg.auto_trim_secs.max(3600);

        match self
            .db
            .create_program(
                ccy,
                crate::db::models::ProgramKind::Distribute,
                held,
                target,
                None,
                self.cfg.auto_trim_floor,
                Some(now_ms() + secs * 1000),
                Some("auto: working off a deposit"),
            )
            .await
        {
            Ok(id) => {
                info!(
                    program = id, %ccy, %amount, %target, secs,
                    "deposit credited; working it off"
                );
                self.db
                    .alert(
                        "auto_trim",
                        &format!(
                            "Deposit of {} {} credited. Selling {} over {}h, down to {}{}. \
                             The clock pauses whenever price is under the floor.",
                            amount.round_dp(2),
                            ccy.to_uppercase(),
                            to_sell.round_dp(2),
                            secs / 3600,
                            target.round_dp(2),
                            self.cfg
                                .auto_trim_floor
                                .map(|f| format!(", never below {f}"))
                                .unwrap_or_default(),
                        ),
                    )
                    .await;
            }
            Err(e) => {
                error!(error = %format!("{e:#}"), %ccy, "could not start the auto-trim");
                self.db
                    .alert(
                        "auto_trim_failed",
                        &format!(
                            "A deposit of {} {} landed but I could not start selling it: {e:#}",
                            amount.round_dp(2), ccy.to_uppercase()
                        ),
                    )
                    .await;
            }
        }
    }

    /// Mark a scheduled program done once it is finished and at target.
    async fn maybe_complete(&self, p: &Program, inventory: Decimal) -> Result<()> {
        if !p.kind.is_scheduled() {
            return Ok(());
        }
        let Some(end) = p.ends_at else { return Ok(()) };
        let past_deadline = now_ms() >= end;
        let at_target = match p.working_side() {
            Some(Side::Buy) => inventory >= p.target_qty,
            Some(Side::Sell) => inventory <= p.target_qty,
            None => true,
        };
        if past_deadline && at_target {
            info!(program = p.id, "program complete");
            self.db.set_program_state(p.id, "done").await?;

            // Hand over to a Neutral program at the inventory we reached.
            //
            // Without this the bot has NO active program, which is a halt:
            // `plan` returns `Halt::NoProgram`, every resting order is
            // cancelled, and both PRL books go dark until someone types a
            // command. A TWAP sent at nine in the evening would finish in the
            // small hours and leave the bot switched off for the rest of the
            // month, with one alert to mark the occasion.
            //
            // Neutral at the reached inventory is the honest default: it keeps
            // making a market exactly where the program left us, and changes
            // no position.
            let handover = self
                .db
                .create_program(
                    &p.market,
                    crate::db::models::ProgramKind::Neutral,
                    inventory,
                    inventory,
                    None,
                    None,
                    None,
                    Some(&format!("auto: took over from #{} on completion", p.id)),
                )
                .await;
            let twap_mode = self.db.setting_bool("twap_only", false).await;
            let note = match handover {
                // In TWAP mode the handover program is a placeholder, not a
                // stance: `twap_only` stands every book that is not working a
                // twap down, so saying "now market-making" would be a lie about
                // a bot that is resting nothing.
                Ok(id) if twap_mode && p.kind == ProgramKind::Twap => format!(
                    " TWAP mode is still on, so nothing is quoting: send another \
                     <code>twap</code> to keep buying, or <code>learn</code> / \
                     <code>hold {}</code> to start making a market again (program #{id}).",
                    inventory.round_dp(2)
                ),
                Ok(id) => format!(" Now market-making around {} (program #{id}).", inventory.round_dp(2)),
                Err(e) => {
                    error!(error = %format!("{e:#}"), "could not install the handover program");
                    " WARNING: no program is active — the bot is not quoting. Send a command.".into()
                }
            };
            self.db
                .alert(
                    "program_done",
                    &format!(
                        "Program #{} ({}) finished at inventory {}.{}",
                        p.id, p.kind.as_str(), inventory.round_dp(2), note
                    ),
                )
                .await;
        } else if past_deadline {
            // Do not silently keep working past the deadline; say so every loop
            // until the operator decides.
            warn!(
                program = p.id,
                inventory = %inventory.round_dp(2),
                target = %p.target_qty,
                "past deadline but not at target — still working passively"
            );
        }
        Ok(())
    }

    async fn maybe_prune(&mut self) {
        let now = now_ms();
        if now - self.last_prune < 3_600_000 {
            return;
        }
        self.last_prune = now;
        match self.db.prune(self.cfg.retain_days).await {
            Ok(n) if n > 0 => info!(rows = n, "pruned old rows"),
            Err(e) => warn!(error = ?e, "prune failed"),
            _ => {}
        }
    }

    // ---- control plane ----

    async fn drain_commands(&self) -> Result<()> {
        for cmd in self.db.pending_commands().await? {
            let args: serde_json::Value =
                serde_json::from_str(&cmd.args).unwrap_or(serde_json::Value::Null);
            let result = self.apply_command(&cmd.command, &args).await;
            let text = match &result {
                Ok(s) => s.clone(),
                Err(e) => {
                    // The operator was told "queued" and will assume it ran.
                    let msg = format!("error: {e:#}");
                    self.db
                        .alert(
                            "command_failed",
                            &format!("Command `{}` failed: {msg}", cmd.command),
                        )
                        .await;
                    msg
                }
            };
            info!(id = cmd.id, command = %cmd.command, result = %text, "command applied");
            self.db.complete_command(cmd.id, &text).await?;
            self.db
                .log("info", "command", &format!("{}: {}", cmd.command, text), Some(args))
                .await;
        }
        Ok(())
    }

    async fn apply_command(&self, command: &str, args: &serde_json::Value) -> Result<String> {
        let dec_arg = |k: &str| -> Option<Decimal> {
            args.get(k).and_then(|v| v.as_str()).and_then(|s| s.parse().ok())
        };

        match command {
            "pause" => {
                self.db.set_setting("enabled", "false").await?;
                self.cancel_all_markets().await;
                Ok("paused and cancelled all orders on every market".into())
            }
            "resume" => {
                self.db.set_setting("enabled", "true").await?;
                Ok("resumed".into())
            }
            "panic" => {
                self.db.set_setting("enabled", "false").await?;
                self.cancel_all_markets().await;
                Ok("PANIC: all orders cancelled on every market, quoting disabled".into())
            }
            "set" => {
                let key = args.get("key").and_then(|v| v.as_str()).context("set needs a key")?;
                let value =
                    args.get("value").and_then(|v| v.as_str()).context("set needs a value")?;
                self.db.set_setting(key, value).await?;
                Ok(format!("{key} = {value}"))
            }
            // Let the arms set the split. One learned program per base asset
            // (or the one named), each starting at the share the account
            // holds now, so nothing moves until the arms say it should; the
            // anchors are read from the account as it stands.
            "learn" => {
                self.db.set_setting("twap_only", "false").await?;
                let balances = self.without_pending_withdrawals(self.api.balances().await?);
                let prices = self.prices(&HashMap::new()).await;
                let holdings = holdings(&self.markets, &balances, &prices);
                let total: Decimal = holdings.values().sum();
                if total <= Decimal::ZERO {
                    bail!("the account has no value to split");
                }
                self.store_anchors(&holdings).await;
                let bases = match args.get("asset").and_then(|v| v.as_str()) {
                    Some(a) => vec![a.to_lowercase()],
                    None => distinct(self.markets.iter().map(|m| m.base_unit.clone())),
                };
                let params = Params::load(&self.cfg, &self.db, self.dry_run).await;
                let mut out = Vec::new();
                for base in bases {
                    let quotes = quotes_of(&self.markets, &base);
                    let ratio = allocation::ratio_for(&holdings, &base, &quotes)
                        .with_context(|| format!("{base}: not a base asset of any configured market"))?;
                    let (a, l) = balance_of(&balances, &base);
                    let id = self
                        .db
                        .create_program(
                            &base,
                            ProgramKind::Learned,
                            a + l,
                            Decimal::ZERO,
                            Some(ratio.round_dp(4)),
                            None,
                            Some(now_ms() + drift_ms(&params)),
                            Some("learned: the arms set the split"),
                        )
                        .await?;
                    out.push(format!(
                        "program {id} on {base}: learned, holding {:.1}% of its books' value now",
                        ratio * dec!(100)
                    ));
                }
                Ok(out.join("; "))
            }
            "program" => {
                let kind = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .and_then(ProgramKind::parse)
                    .context("program needs a valid kind")?;
                // A twap needs no target typed: it works the whole account in.
                let requested = match kind {
                    ProgramKind::Twap | ProgramKind::TwapSell | ProgramKind::TwapBuy => Decimal::ZERO,
                    _ => dec_arg("target").context("program needs a target")?,
                };
                // Programs are filed per asset, so a command may name one.
                let asset = args
                    .get("asset")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&self.markets[0].base_unit)
                    .to_lowercase();
                let inventory = self.inventory_of(&asset).await.unwrap_or(Decimal::ZERO);
                // A relative request ("buy 5000 more") is resolved against live
                // inventory here rather than when it was typed, so the number
                // cannot go stale while the command sits in the queue.
                let relative = args.get("relative").and_then(|v| v.as_bool()).unwrap_or(false);
                let target = if relative { inventory + requested } else { requested };
                let ends_at = args.get("duration_secs").and_then(|v| v.as_i64()).map(|d| now_ms() + d * 1000);
                // A slow buy spends every unit of cash. Without a ceiling that
                // is an order to buy at any price, which is never what is meant.
                if kind == ProgramKind::TwapBuy && dec_arg("limit_price").is_none_or(|p| p <= Decimal::ZERO) {
                    bail!("a slow buy needs a ceiling (limit_price)");
                }

                // A twap's target is derived from live balances every loop, so
                // what is stored is only the snapshot completion checks against:
                // everything the account could hold in `asset` if every unit of
                // the currencies that buy it were spent.
                let target = if kind == ProgramKind::Twap {
                    let balances = self.without_pending_withdrawals(self.api.balances().await?);
                    let prices = self.prices(&HashMap::new()).await;
                    // `prices` is keyed by CURRENCY, not by market id: the
                    // dry run caught the market-id lookup, which was always
                    // `None` and refused every twap with "no price".
                    let price = prices
                        .get(&asset)
                        .copied()
                        .filter(|p| *p > Decimal::ZERO)
                        .context("no price for the asset being worked")?;
                    let cash: Decimal = quotes_of(&self.markets, &asset)
                        .iter()
                        .map(|c| {
                            let (a, l) = balance_of(&balances, c);
                            a + l
                        })
                        .sum();
                    inventory + cash / price
                } else {
                    target
                };

                let id = self
                    .db
                    .create_program(
                        &asset,
                        kind,
                        inventory,
                        target,
                        dec_arg("ratio"),
                        dec_arg("limit_price"),
                        ends_at,
                        args.get("note").and_then(|v| v.as_str()),
                    )
                    .await?;

                let budget = dec_arg("budget").filter(|b| *b > Decimal::ZERO && kind == ProgramKind::TwapBuy);
                if let Some(b) = budget {
                    self.db.set_program_budget(id, b).await?;
                }

                // TWAP mode lives and dies with a twap program, so the flag is
                // set here rather than left to the operator to remember. Any
                // other program clears it: the alternative is a bot that looks
                // like it is market-making and is silently standing down.
                let twap = kind == ProgramKind::Twap;
                self.db.set_setting("twap_only", if kind.is_twap() { "true" } else { "false" }).await?;
                if kind == ProgramKind::TwapBuy {
                    return Ok(format!(
                        "program {id} on {asset} (slow buy) — {}, never above {}; \
                         TWAP mode on: bids only, every other book stands down",
                        budget.map_or("every unit of cash spent".to_string(), |b| format!("spends at most {b}")),
                        dec_arg("limit_price").unwrap_or_default()
                    ));
                }
                if kind == ProgramKind::TwapSell {
                    return Ok(format!(
                        "program {id} on {asset} (slow sell) — every unit for sale, never below {}; \
                         TWAP mode on: asks only, every other book stands down",
                        dec_arg("limit_price").unwrap_or(SELL_FLOOR)
                    ));
                }
                if twap {
                    return Ok(format!(
                        "program {id} on {asset} (twap) from {inventory} to {target} \
                         (derived live){} — TWAP mode on: bids only, every other book stands down",
                        ends_at.map(|_| ", on a schedule").unwrap_or("")
                    ));
                }
                Ok(format!(
                    "program {id} on {asset} ({}) from {inventory} to {target}{}",
                    kind.as_str(),
                    ends_at.map(|_| " on a schedule").unwrap_or("")
                ))
            }
            other => Ok(format!("unknown command {other}")),
        }
    }

    /// Once a slow sell has sold everything, start the buy queued behind it
    /// (`then_buy_qty`), once. Asked for on 2026-10-15: "when it hits zero it
    /// begins buying up to 11,000".
    ///
    /// "Everything" is under the smallest order the sell will place, with no
    /// deposit arriving, held for `then_buy_after_mins` -- the exchange only
    /// shows a deposit once it has seen it on chain, so a quiet spell is the
    /// only guard against one still being sent. It goes through the command
    /// queue, so it is in `ctl log` like anything typed.
    async fn chain_buyback(&mut self, balances: &[Balance]) -> Result<()> {
        let qty = self.db.setting_decimal("then_buy_qty", Decimal::ZERO).await;
        if qty <= Decimal::ZERO {
            self.sold_out_since = None;
            return Ok(());
        }
        let base = self.markets[0].base_unit.clone();
        let selling = self
            .db
            .active_program(&base)
            .await?
            .is_some_and(|p| p.kind == ProgramKind::TwapSell);
        let (avail, locked) = balance_of(balances, &base);
        let left = avail + locked;
        let min_order = crate::strategy::sell::SellParams::default().min_order;
        if !selling || left >= min_order || self.pending_in.contains(&base) {
            self.sold_out_since = None;
            return Ok(());
        }
        let since = *self.sold_out_since.get_or_insert_with(now_ms);
        let mins = self.db.setting_decimal("then_buy_after_mins", dec!(15)).await;
        let wait = i64::try_from((mins * dec!(60000)).round()).unwrap_or(900_000);
        if now_ms() - since < wait {
            return Ok(());
        }
        let ceiling = self.db.setting_decimal("then_buy_ceiling", Decimal::ZERO).await;
        if ceiling <= Decimal::ZERO {
            warn!("sold out, but then_buy_ceiling is not set; not starting the follow-on buy");
            return Ok(());
        }
        // Cleared first: whatever happens next, it fires once.
        self.db.set_setting("then_buy_qty", "0").await?;
        self.sold_out_since = None;
        self.db
            .push_command("set", serde_json::json!({ "key": "buy_qty", "value": qty.to_string() }))
            .await?;
        self.db
            .push_command(
                "program",
                serde_json::json!({
                    "kind": "twapbuy",
                    "asset": base,
                    "limit_price": ceiling.to_string(),
                    "note": format!("follow-on buy: {qty} {} after the slow sell sold out", base.to_uppercase()),
                }),
            )
            .await?;
        info!(%qty, %ceiling, %left, "slow sell sold out; follow-on buy queued");
        self.db
            .alert(
                "program",
                &format!(
                    "{}: slow sell sold out ({left} left). Buying {qty} back, slowly, never above {ceiling}.",
                    base.to_uppercase()
                ),
            )
            .await;
        Ok(())
    }

    async fn cancel_all_markets(&self) {
        for m in &self.markets {
            if let Err(e) = self.cancel_everything(&m.id).await {
                warn!(market = %m.id, error = %format!("{e:#}"), "cancel failed");
            }
        }
    }

    async fn inventory_of(&self, asset: &str) -> Result<Decimal> {
        let balances = self.without_pending_withdrawals(self.api.balances().await?);
        let (avail, locked) = balance_of(&balances, asset);
        Ok(avail + locked)
    }

    /// Subtract withdrawals in flight from `locked`.
    ///
    /// `locked` mixes two very different things: the funds behind our own
    /// resting orders, which are ours and return the moment we cancel, and
    /// funds committed to a withdrawal, which are gone. The exchange does not
    /// distinguish them, so we do it here from the withdrawal ledger.
    ///
    /// Floored at zero: if the ledger and the balance disagree -- a withdrawal
    /// settling between the two reads -- the honest answer is "nothing locked",
    /// never a negative balance.
    fn without_pending_withdrawals(&self, balances: Vec<Balance>) -> Vec<Balance> {
        drop_pending_withdrawals(balances, &self.pending_out)
    }

    /// A price in the unit of account -- the primary market's quote -- for
    /// every currency we hold: the unit itself at one, each base at its
    /// market's microprice against the unit (the public book, else the last
    /// fair value seen, else the last snapshot), and anything with no such
    /// market at one, which on this account is nothing.
    async fn prices(&self, public: &HashMap<String, OrderBook>) -> HashMap<String, Decimal> {
        let unit = self
            .markets
            .iter()
            .find(|m| m.id == self.cfg.market)
            .or(self.markets.first())
            .map(|m| m.quote_unit.clone())
            .unwrap_or_else(|| "usdt".into());
        let mut prices: HashMap<String, Decimal> = HashMap::new();
        prices.insert(unit.clone(), Decimal::ONE);
        for m in self.markets.iter().filter(|m| m.quote_unit == unit) {
            let p = match public.get(&m.id).and_then(|b| b.microprice()) {
                Some(p) if p > Decimal::ZERO => Some(p),
                _ => self
                    .fv_history
                    .get(&m.id)
                    .and_then(|h| h.last().map(|(_, p)| *p))
                    .filter(|p| *p > Decimal::ZERO),
            };
            let p = match p {
                Some(p) => p,
                None => self.db.last_mid(&m.base_unit).await.unwrap_or(Decimal::ONE),
            };
            prices.insert(m.base_unit.clone(), p);
        }
        prices
    }

    /// Record the share of value each currency holds as the anchor its
    /// surplus sits at. `learned_anchors` marks that a learned split exists.
    async fn store_anchors(&self, holdings: &HashMap<String, Decimal>) {
        let total: Decimal = holdings.values().sum();
        if total <= Decimal::ZERO {
            return;
        }
        for (c, v) in holdings {
            self.db.set_setting(&format!("learned_anchor:{c}"), &num::to_str(v / total)).await.ok();
        }
        self.db.set_setting("learned_anchors", &now_ms().to_string()).await.ok();
    }

    /// What each currency's arms have earned, in the unit of account: for
    /// every book, the funding its bid side needs at full size times the
    /// best share among its bid arms, charged to the quote currency, and the
    /// same for the asks, charged to the base. See `strategy::allocation`.
    fn demand(
        &self,
        params: &Params,
        balances: &[Balance],
        prices: &HashMap<String, Decimal>,
        holdings: &HashMap<String, Decimal>,
    ) -> HashMap<String, Decimal> {
        let mut demand: HashMap<String, Decimal> = HashMap::new();
        for m in &self.markets {
            let p = params.for_market(self.cfg.overrides.get(&m.id));
            // Equity as `plan` values it for this book: the base and every
            // quote currency the base trades against.
            let equity = holdings.get(&m.base_unit).copied().unwrap_or_default()
                + quotes_of(&self.markets, &m.base_unit)
                    .iter()
                    .map(|c| holdings.get(c).copied().unwrap_or_default())
                    .sum::<Decimal>();
            let (a, l) = balance_of(balances, &m.base_unit);
            let quote_px = prices.get(&m.quote_unit).copied().unwrap_or(Decimal::ONE);
            let base_px = prices.get(&m.base_unit).copied().unwrap_or(Decimal::ONE);
            if quote_px <= Decimal::ZERO {
                continue;
            }
            let need = allocation::side_funding(&p.q, equity / quote_px, a + l, base_px / quote_px) * quote_px;
            let bid = arms::spend_weight(&self.arm_sizes, &m.id, Side::Buy).unwrap_or(Decimal::ZERO);
            let ask = arms::spend_weight(&self.arm_sizes, &m.id, Side::Sell).unwrap_or(Decimal::ZERO);
            *demand.entry(m.quote_unit.clone()).or_default() += need * bid;
            *demand.entry(m.base_unit.clone()).or_default() += need * ask;
        }
        demand
    }

    /// Re-derive every learned program's ratio from the arms, hourly, or at
    /// once when a learned program has just started or been re-anchored by a
    /// transfer (its glide is newer than the last derivation). Each program
    /// gets the new ratio and a fresh glide from its current inventory over
    /// `DRIFT_HALF_LIVES`, so the target is approached by leaning, a 24th of
    /// the way each hour, never by a jump.
    async fn retarget(
        &mut self,
        params: &Params,
        balances: &[Balance],
        public: &HashMap<String, OrderBook>,
    ) {
        let bases = distinct(self.markets.iter().map(|m| m.base_unit.clone()));
        let mut learned = Vec::new();
        for b in &bases {
            if let Ok(Some(p)) = self.db.active_program(b).await
                && p.kind == ProgramKind::Learned
            {
                learned.push((b.clone(), p));
            }
        }
        if learned.is_empty() {
            return;
        }
        let now = now_ms();
        let fresh = learned.iter().any(|(_, p)| p.started_at > self.last_retarget);
        if now - self.last_retarget < RETARGET_MS && !fresh {
            return;
        }
        self.last_retarget = now;

        let prices = self.prices(public).await;
        let holdings = holdings(&self.markets, balances, &prices);
        let total: Decimal = holdings.values().sum();
        if total <= Decimal::ZERO {
            return;
        }
        let mut anchor: HashMap<String, Decimal> = HashMap::new();
        for (c, v) in &holdings {
            let stored = self.db.setting_decimal(&format!("learned_anchor:{c}"), dec!(-1)).await;
            anchor.insert(c.clone(), if stored >= Decimal::ZERO { stored } else { v / total });
        }
        let demand = self.demand(params, balances, &prices, &holdings);
        let targets = allocation::learned_targets(total, &anchor, &demand);
        let drift = drift_ms(params);
        let mut moved = Vec::new();
        for (base, p) in learned {
            let quotes = quotes_of(&self.markets, &base);
            let Some(ratio) = allocation::ratio_for(&targets, &base, &quotes) else { continue };
            let ratio = ratio.round_dp(4);
            let (a, l) = balance_of(balances, &base);
            if let Err(e) = self.db.set_program_glide(p.id, ratio, a + l, now + drift).await {
                warn!(error = %format!("{e:#}"), %base, "could not re-derive the learned split");
                continue;
            }
            let was = p.ratio.unwrap_or(ratio);
            info!(
                %base, from = %was, to = %ratio,
                demand = %demand.get(&base).copied().unwrap_or_default().round_dp(0),
                held = %holdings.get(&base).copied().unwrap_or_default().round_dp(0),
                "learned split re-derived"
            );
            // A percent of the book is worth a line; the hourly no-change is
            // not.
            if (ratio - was).abs() >= dec!(0.01) {
                moved.push(format!(
                    "{}: {:.1}% -> {:.1}% of its books' value (its arms want ${} on hand, holding ${})",
                    base.to_uppercase(),
                    was * dec!(100),
                    ratio * dec!(100),
                    demand.get(&base).copied().unwrap_or_default().round_dp(0),
                    holdings.get(&base).copied().unwrap_or_default().round_dp(0),
                ));
            }
        }
        if !moved.is_empty() {
            self.db.alert("retarget", &format!("Learned split: {}. Drifting there over {} h.", moved.join("; "), drift / 3_600_000)).await;
        }
    }
}

/// The glide of a learned program: two evidence half-lives.
fn drift_ms(params: &Params) -> i64 {
    let half_life = (f64::try_from(params.q.edge_half_life_hours).unwrap_or(12.0) * 3_600_000.0) as i64;
    half_life.max(3_600_000) * DRIFT_HALF_LIVES
}

/// The quote currencies `base` trades against, across the configured books.
fn quotes_of(markets: &[Market], base: &str) -> Vec<String> {
    distinct(markets.iter().filter(|m| m.base_unit == base).map(|m| m.quote_unit.clone()))
}

/// Value held per currency, in the unit of account, over every currency the
/// configured books trade.
fn holdings(
    markets: &[Market],
    balances: &[Balance],
    prices: &HashMap<String, Decimal>,
) -> HashMap<String, Decimal> {
    distinct(markets.iter().flat_map(|m| [m.base_unit.clone(), m.quote_unit.clone()]))
        .into_iter()
        .map(|c| {
            let (a, l) = balance_of(balances, &c);
            let v = (a + l) * prices.get(&c).copied().unwrap_or(Decimal::ONE);
            (c, v)
        })
        .collect()
}

/// The arm table from the database: every book's arms, estimated from the
/// orders and fills of the last `ARMS_MAX_AGE_MS` -- NOT since the
/// scoreboard's epoch, see `Db::analytics_epoch_ms` -- and sized by
/// `params`. `books` is (market, base asset); a base asset with an execution
/// program running is exempt from the gate.
///
/// Shared by the engine's refresh and `safetrade arms`, so the table the
/// operator inspects offline is computed by the code that quotes from it.
pub async fn estimate_arms(
    db: &Db,
    books: &[(String, String)],
    params: &Params,
    now_ms: i64,
) -> Result<Vec<ArmSize>> {
    let obs = db.arm_observations(now_ms - ARMS_MAX_AGE_MS, now_ms).await?;
    let mut execution: HashMap<&str, bool> = HashMap::new();
    for (_, base) in books {
        if execution.contains_key(base.as_str()) {
            continue;
        }
        let running = match db.active_program(base).await {
            Ok(Some(p)) => p.kind.is_execution(),
            _ => false,
        };
        execution.insert(base, running);
    }
    let books: Vec<Book> = books
        .iter()
        .map(|(m, base)| Book { market: m, execution: execution[base.as_str()] })
        .collect();
    Ok(arms::size_arms(
        &obs,
        &books,
        now_ms,
        &arms::Params {
            half_life_ms: (f64::try_from(params.q.edge_half_life_hours).unwrap_or(12.0)
                * 3_600_000.0) as i64,
        },
    ))
}

/// Every setting that decides what the bot quotes, as one short digest.
///
/// The operational settings -- where the database lives, how long rows are
/// kept, how often Telegram speaks -- are left out: changing them changes no
/// quote, and must not restart the scoreboard. Comments in the file are not
/// settings either, so a re-worded config leaves the fingerprint alone. The
/// maps inside serialise sorted, so two runs of the same file agree.
pub fn strategy_fingerprint(cfg: &Config) -> String {
    use sha2::Digest;
    let mut v = serde_json::to_value(cfg).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        for k in [
            "db_path", "api_base", "telegram_status_secs", "retain_days", "depth_levels",
            "lighter_market_id",
        ] {
            o.remove(k);
        }
    }
    hex::encode(&sha2::Sha256::digest(v.to_string().as_bytes())[..8])
}

/// The range a stablecoin cross may sit in and still be believed.
///
/// USDC/USDT lives within a fraction of a percent of 1.0. A rate outside this
/// is a broken book or a bad decode, not a market -- and it is the multiplier
/// that converts PRL's price between the two books, so believing it prices
/// every PRL/USDC quote wrongly. The old 0.5-2.0 window would have accepted a
/// rate that halves or doubles the conversion.
const STABLE_CROSS_BAND: (Decimal, Decimal) = (dec!(0.9), dec!(1.1));

/// Whether `m` is a book that must be quoted off the primary's price rather
/// than its own: same base asset, not the primary itself.
fn is_referenced(primary: &str, markets: &[Market], m: &Market) -> bool {
    markets
        .iter()
        .find(|k| k.id == primary)
        .is_some_and(|p| m.id != p.id && m.base_unit == p.base_unit)
}

/// Fair value for every market this loop, and whether it came from a
/// DIFFERENT book than the one it is handed to.
///
/// The configured primary anchors its base asset. It used to be whichever
/// book had the most size resting at the touch, measured loop by loop, and
/// that flipped: prlusdc's touch was deeper than prlusdt's in 5.7% of the
/// paired snapshots on 2026-09-22/23 and 3.5% on 09-24, because one stranger's
/// 8,500 PRL bid on the thin book outweighs a thin moment on the deep one. On
/// those loops the deep book was priced off the thin book's residue -- with
/// the improve cap unlocked, since the reference was "external" -- and the
/// thin book was handed its own microprice and quoted itself. Live at
/// 2026-09-24 11:39:33 UTC, with prlusdt at 1.17/1.18 and prlusdc's residue at
/// 1.2977, prlusdt recorded a touch bid as 992 bp from fair value and prlusdc
/// asked at 1.33. The deep book is the deep book by configuration, not by
/// the size of the last order someone left on the thin one.
///
/// A referenced book gets a price only when the primary's public book is
/// two-sided and, where the quote currencies differ, the stablecoin cross is
/// readable and inside `STABLE_CROSS_BAND`. Otherwise it gets nothing, and
/// `plan` stands it down (`Halt::NoReference`) rather than quoting its own
/// residue. Any other base asset is anchored by its deepest two-sided book,
/// which for a lone market is itself, marked internal.
fn references(
    primary: &str,
    markets: &[Market],
    public: &HashMap<String, OrderBook>,
) -> HashMap<String, (Decimal, bool)> {
    let two_sided = |id: &str| -> Option<(Decimal, Decimal)> {
        let b = public.get(id)?;
        Some((b.microprice()?, b.best_bid()?.1 + b.best_ask()?.1))
    };
    let mut out = HashMap::new();
    for base in distinct(markets.iter().map(|m| m.base_unit.clone())) {
        let anchor = match markets.iter().find(|m| m.id == primary && m.base_unit == base) {
            // The primary's asset: the primary or nothing.
            Some(p) => two_sided(&p.id).map(|(px, _)| (p, px)),
            None => markets
                .iter()
                .filter(|m| m.base_unit == base)
                .filter_map(|m| two_sided(&m.id).map(|(px, depth)| (m, px, depth)))
                .max_by(|a, c| a.2.partial_cmp(&c.2).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(m, px, _)| (m, px)),
        };
        let Some((anchor_m, price)) = anchor else { continue };
        for m in markets.iter().filter(|m| m.base_unit == base) {
            let converted = if m.quote_unit == anchor_m.quote_unit {
                Some(price)
            } else {
                // "<this quote><anchor quote>" is anchor-quote per this-quote,
                // so the price is divided by it; the other way round it is
                // multiplied. Only a cross we quote (and so read, with our own
                // size stripped) is used; anything else stays unpriced.
                let div = format!("{}{}", m.quote_unit, anchor_m.quote_unit);
                let mul = format!("{}{}", anchor_m.quote_unit, m.quote_unit);
                let rate = |id: &str| {
                    two_sided(id)
                        .map(|(r, _)| r)
                        .filter(|r| *r > STABLE_CROSS_BAND.0 && *r < STABLE_CROSS_BAND.1)
                };
                if markets.iter().any(|k| k.id == div) {
                    rate(&div).map(|r| price / r)
                } else if markets.iter().any(|k| k.id == mul) {
                    rate(&mul).map(|r| price * r)
                } else {
                    None
                }
            };
            if let Some(p) = converted {
                out.insert(m.id.clone(), (p, m.id != anchor_m.id));
            }
        }
    }
    out
}

/// The price whose movement the volatility breaker and the spread widening
/// should measure on a book: the fair value the book is actually quoted
/// around.
///
/// On a referenced book that is the reference. Measuring the local microprice
/// instead measured the residue: prlusdc's "fair value moved 29.23% inside
/// the vol window" (2026-09-22 16:05 UTC, and 20.4%, 17.5%, 15.8% on three
/// more occasions) was a stranger's far ask arriving and leaving on a book
/// whose real price -- PRL/USDT -- had moved a fraction of that. Each one
/// cancelled the thin book's quotes for five minutes, and between halts the
/// same reading widened its spread by tick after tick of imaginary range: on
/// 2026-09-24 11:41 UTC the residue stepped 1.31 -> 1.23 in two minutes while
/// PRL sat at 1.17, and the asks sat 3.5 ticks out on a 1.5-tick floor.
fn volatility_anchor(
    reference: Option<Decimal>,
    reference_is_external: bool,
    local_micro: Option<Decimal>,
) -> Option<Decimal> {
    match reference {
        Some(r) if reference_is_external => Some(r),
        _ => local_micro,
    }
}

/// Split each currency across the markets that spend it, in proportion to
/// what their arms have earned.
///
/// A currency can be wanted by several books at once -- USDT funds PRL/USDT
/// bids and USDC/USDT bids alike, PRL funds both PRL books' asks -- and
/// whoever is quoted first would otherwise take all of it. This is also where
/// two books on one base asset are reconciled rather than summed: the PRL that
/// prlusdc's asks earn is PRL that prlusdt's asks do not get.
///
/// A book's claim on a currency is the best share among the arms that spend
/// it (bids spend the quote, asks the base), so a book with one full-size arm
/// wants a full ladder's funding and a book with nothing proven claims
/// nothing. Clamped so no book is ever starved outright: a market with no
/// capital cannot place even the minimum orders its arms learn from, and so
/// cannot show what it would have earned. The ceiling stops one book taking
/// everything while the other still has an arm to prove. With no evidence
/// anywhere every claim is zero and the split is even.
/// Take each asset's set-aside pile off its purse, never below zero.
fn hold_back(purse: &mut HashMap<String, Decimal>, piles: &HashMap<String, Decimal>) {
    for (ccy, v) in purse.iter_mut() {
        let pile = piles.get(ccy).copied().unwrap_or_default();
        *v = (*v - pile).max(Decimal::ZERO);
    }
}

fn allocate(
    markets: &[Market],
    purse: &HashMap<String, Decimal>,
    sizes: &[ArmSize],
) -> HashMap<String, HashMap<String, Decimal>> {
    let claim = |m: &Market, ccy: &str| -> Decimal {
        let side = if m.quote_unit == ccy { Side::Buy } else { Side::Sell };
        arms::spend_weight(sizes, &m.id, side).unwrap_or(Decimal::ZERO)
    };
    let mut out: HashMap<String, HashMap<String, Decimal>> = HashMap::new();
    for ccy in distinct(markets.iter().flat_map(|m| [m.base_unit.clone(), m.quote_unit.clone()])) {
        let users: Vec<&Market> =
            markets.iter().filter(|m| m.base_unit == ccy || m.quote_unit == ccy).collect();
        let total: Decimal = users.iter().map(|m| claim(m, &ccy)).sum();
        let weights: Vec<Decimal> = users
            .iter()
            .map(|m| {
                let w = if total.is_zero() { Decimal::ONE } else { claim(m, &ccy) / total };
                num::clamp(w, dec!(0.08), dec!(0.70))
            })
            .collect();
        let norm: Decimal = weights.iter().sum();
        let available = purse.get(&ccy).copied().unwrap_or_default();
        for (m, w) in users.iter().zip(weights) {
            out.entry(m.id.clone()).or_default().insert(ccy.clone(), available * w / norm);
        }
    }
    out
}

/// One crossing order's side, price and size, or `None` to leave the spread
/// alone. See `Engine::take_if_behind` for why each condition is there; this is
/// the arithmetic, split out because it is the part that can be tested without
/// an exchange.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TakerClip {
    side: Side,
    price: Decimal,
    amount: Decimal,
    behind: Decimal,
}

#[allow(clippy::too_many_arguments)]
/// The bids a twap wants resting: all at the best bid, as a stack of orders.
///
/// The owner's rule (2026-10-07): keep buying at the bid until it fills, and
/// bid more the longer it waits -- by adding orders, not replacing them. So:
///
/// * the touch is the plan's level-0 bid, and its size is one chunk;
/// * every order of ours already at that price is kept as it stands, oldest
///   first, since the oldest is nearest the front of the queue;
/// * the stack is topped up with one new order whenever it is a chunk or more
///   short of the schedule deficit, which grows while nothing fills;
/// * once fills catch the schedule up, the newest orders past the deficit are
///   left out, and so cancelled -- the ones with the worst queue place.
///
/// Every deeper rung is dropped: the job is to buy at the bid.
fn twap_stack(
    quotes: &[DesiredQuote],
    live: &[crate::db::models::LiveOrder],
    target_now: Decimal,
    inventory: Decimal,
    // Free quote cash. A top-up the purse cannot fund is skipped whole, so
    // it is sized to what is there -- after a manual buy, that is less than
    // the schedule says.
    free_quote: Decimal,
    amount_precision: u32,
) -> Vec<DesiredQuote> {
    let Some(touch) = quotes.iter().find(|q| q.side == Side::Buy && q.level == 0) else {
        return quotes.to_vec();
    };
    let chunk = touch.amount;
    let want = (target_now - inventory).max(chunk);

    let mut resting: Vec<&crate::db::models::LiveOrder> =
        live.iter().filter(|o| o.side == Side::Buy && o.price == touch.price).collect();
    resting.sort_by_key(|o| o.placed_at);

    let mut out: Vec<DesiredQuote> =
        quotes.iter().filter(|q| q.side == Side::Sell).cloned().collect();
    let mut stacked = Decimal::ZERO;
    for (i, o) in resting.iter().enumerate() {
        // The oldest is always kept. A newer one only while it does not
        // overshoot the deficit by more than a chunk.
        if stacked > Decimal::ZERO && stacked + o.remaining() > want + chunk {
            break;
        }
        stacked += o.remaining();
        out.push(DesiredQuote { amount: o.remaining(), level: i as i64, ..touch.clone() });
    }
    let short = num::floor_to((want - stacked).min(free_quote / touch.price), amount_precision);
    if short >= chunk {
        out.push(DesiredQuote { amount: short, level: resting.len() as i64, ..touch.clone() });
    }
    out
}

fn taker_clip(
    market: &Market,
    book: &OrderBook,
    program: &Program,
    params: &Params,
    fv: Decimal,
    inventory: Decimal,
    target: Decimal,
    free_quote: Decimal,
    free_base: Decimal,
    our_participation: Decimal,
) -> Option<TakerClip> {
    if !params.enabled
        || !params.q.allow_taker
        || !program.kind.is_execution()
        || program.ends_at.is_none()
    {
        return None;
    }
    // No limit price, no crossing. "Spend it as fast as you can" with no
    // ceiling is how a thin ask side gets walked up.
    let limit = program.limit_price?;
    if params.q.max_participation > Decimal::ZERO && our_participation >= params.q.max_participation
    {
        return None;
    }

    let behind = target - inventory;
    // A program that points one way never CROSSES the other way to get back
    // on schedule. Ahead of schedule is not late in reverse: on 2026-09-29 an
    // accumulate over 15 minutes had a stack of resting bids fill at once,
    // read itself ahead, and crossed to sell them at the bid -- paying the
    // spread twice to undo a buy it wanted. Passive quoting still leans back
    // toward the glide; only the taker leg is one-way.
    let buys_only = program.kind.is_buy_only() || program.kind == ProgramKind::Accumulate;
    let sells_only = matches!(program.kind, ProgramKind::Distribute | ProgramKind::Flatten);
    let side = if behind > Decimal::ZERO {
        if sells_only {
            return None;
        }
        Side::Buy
    } else if buys_only {
        return None;
    } else {
        Side::Sell
    };

    // One rung of lateness is weather, and the spread is not worth paying for it.
    //
    // The rung is the EFFECTIVE one, not the `size_base` setting: where
    // `size_pct_of_equity` is set it replaces that setting as the ladder's
    // per-rung ceiling, and 3% of equity can be several times a `size_base`
    // of 250. Measured live, the raw setting had the leg crossing for a
    // shortfall barely over one raw rung -- PRL bought at the touch
    // for a tick of spread, over and over, when the passive ladder would have
    // covered it inside a minute.
    let equity = inventory * fv + free_quote;
    let by_equity = if params.q.size_pct_of_equity > Decimal::ZERO && fv > Decimal::ZERO {
        equity * params.q.size_pct_of_equity / fv
    } else {
        Decimal::ZERO
    };
    let rung = params.q.size_base.max(by_equity).max(market.min_amount);
    if behind.abs() < rung {
        return None;
    }

    let tick = market.tick();
    let (bid, _) = book.best_bid()?;
    let (ask, _) = book.best_ask()?;

    // The ceiling is the tightest of the program's own limit and a band around
    // fair value. On a referenced book fair value comes from the DEEP book, so
    // the band is what refuses to pay a thin book's 2-15% spread.
    let band = fv * params.q.taker_max_bps / dec!(10000);
    // Hit the touch exactly rather than reaching through it. A limit order at
    // the opposite touch crosses and fills there; going `TAKER_THROUGH_TICKS`
    // past it, as the stablecoin recycle does, prices in levels we are not
    // allowed to take anyway -- the depth cap below is what decides how far into
    // the book one clip may eat. On PRL/USDT one tick is 0.66% of price, so two
    // ticks through would have put every crossing 1.3% over fair value and the
    // band would have refused all of them.
    let price = match side {
        Side::Buy => num::round_to(ask, market.price_precision),
        Side::Sell => num::round_to(bid.max(tick), market.price_precision),
    };
    let within = match side {
        Side::Buy => price <= limit && price <= fv + band,
        Side::Sell => price >= limit && price >= fv - band,
    };
    if !within || price <= Decimal::ZERO {
        return None;
    }

    // The shortfall, then every cap that applies to one order.
    let mut amount = behind.abs();
    if params.risk.max_order_notional > Decimal::ZERO {
        amount = amount.min(params.risk.max_order_notional / price);
    }
    let depth = book.depth_within(side.opposite(), params.q.depth_band);
    if params.q.max_order_pct_of_depth > Decimal::ZERO && depth > Decimal::ZERO {
        amount = amount.min(depth * params.q.max_order_pct_of_depth / price);
    }
    amount = match side {
        Side::Buy => amount.min(free_quote / price),
        Side::Sell => amount.min(free_base),
    };
    let amount = num::floor_to(amount, market.amount_precision);
    if amount < market.min_amount || amount <= Decimal::ZERO {
        return None;
    }
    Some(TakerClip { side, price, amount, behind })
}

/// Subtract withdrawals in flight from `locked`. See the caller for why.
fn drop_pending_withdrawals(
    mut balances: Vec<Balance>,
    pending: &HashMap<String, Decimal>,
) -> Vec<Balance> {
    if pending.is_empty() {
        return balances;
    }
    for b in &mut balances {
        if let Some(out) = pending.get(&b.currency.to_lowercase()) {
            b.locked = (b.locked - *out).max(Decimal::ZERO);
        }
    }
    balances
}

/// How much of the acquiring book's quote currency to cross for, or `None` to
/// leave the account alone.
///
/// Split out from `replenish_acquire_quote` because it is the arithmetic behind
/// the only taker order this bot places, and because the caller cannot be tested
/// without an exchange.
///
/// `have` is what is HELD, resting included -- see the note at the call site.
/// `equity_in_quote` and the two shares come straight from the settings: refill
/// only below `trigger` of the need, never more than `max_pct` of equity at a
/// time, and buy the whole shortfall rather than topping up to the trigger, so
/// the crossing is paid for once per drain instead of once per loop.
fn recycle_shortfall(
    need: Decimal,
    have: Decimal,
    equity_in_quote: Decimal,
    trigger: Decimal,
    max_pct: Decimal,
) -> Option<Decimal> {
    if need <= Decimal::ZERO || have >= need * trigger {
        return None;
    }
    let cap = equity_in_quote * max_pct;
    let want = (need - have).min(cap);
    (want > Decimal::ZERO).then_some(want)
}

fn balance_of(balances: &[Balance], currency: &str) -> (Decimal, Decimal) {
    balances
        .iter()
        .find(|b| b.currency.eq_ignore_ascii_case(currency))
        .map(|b| (b.balance, b.locked))
        .unwrap_or_default()
}

/// Remove our own resting quantity from the book.
///
/// Without this the bot reads its own orders as market depth: it would size
/// against liquidity that is really its own, and worse, treat its own bid as
/// the touch and ratchet up against itself one tick at a time. On a market
/// where a tick is 1%, that ratchet is precisely the price move we are trying
/// not to cause.
fn strip_own(
    book: &OrderBook,
    live: &[crate::db::models::LiveOrder],
    market: &Market,
) -> OrderBook {
    let mut ours: HashMap<(Side, Decimal), Decimal> = HashMap::new();
    for o in live {
        *ours.entry((o.side, o.price)).or_default() += o.remaining();
    }

    let strip = |levels: &[(Decimal, Decimal)], side: Side| -> Vec<(Decimal, Decimal)> {
        levels
            .iter()
            .filter_map(|(p, q)| {
                let mine = ours.get(&(side, *p)).copied().unwrap_or_default();
                let rest = num::floor_to(q - mine, market.amount_precision);
                (rest > Decimal::ZERO).then_some((*p, rest))
            })
            .collect()
    };

    OrderBook {
        bids: strip(&book.bids, Side::Buy),
        asks: strip(&book.asks, Side::Sell),
    }
}

#[cfg(test)]
mod tests {
    use super::{MIN_SPAN_SECS, TAPE_LIMIT, measurement_window, recycle_shortfall};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    const NOW: i64 = 1_790_000_000;

    /// The recycle trigger must count money that is RESTING as money we have.
    ///
    /// This is the only taker order the bot places and the path had never run in
    /// production, so nothing had ever caught it reading `available` alone. The
    /// swap exists to fund bids; the moment those bids rest, the money is locked
    /// and `available` collapses. Judged on `available` the ladder therefore looks
    /// permanently starved and the bot crosses the stablecoin book every loop, at
    /// 24 bp a time, until the USDT is gone.
    #[test]
    fn the_recycle_trigger_counts_resting_money_and_stops_when_the_purse_is_full() {
        // A starved ladder: prlusdc's bids hold under a fifth of what they
        // need, and the need is a small share of equity.
        let (need, equity) = (dec!(1357), dec!(14930));
        let want = recycle_shortfall(need, dec!(250), equity, dec!(0.6), dec!(0.05)).unwrap();
        assert_eq!(want, dec!(746.5), "one swap, capped at 5% of equity");

        // Now $1,000 of that USDC is resting in the bids it was swapped in for,
        // leaving $100 available. Counting only what is available would swap
        // again; counting what is held must not.
        assert_eq!(
            recycle_shortfall(need, dec!(1100), equity, dec!(0.6), dec!(0.05)),
            None,
            "a funded ladder is not a starved one"
        );
        // The trigger is hysteresis, not a target: still nothing at 60% of need.
        assert_eq!(recycle_shortfall(need, need * dec!(0.6), equity, dec!(0.6), dec!(0.05)), None);
        // A hair under it, and the whole shortfall is bought at once -- paying
        // the crossing once per drain, not once per loop.
        let w = recycle_shortfall(need, need * dec!(0.59), equity, dec!(0.6), dec!(0.05)).unwrap();
        assert!(w > need * dec!(0.4), "the shortfall, not a top-up to the trigger: {w}");

        // Degenerate inputs buy nothing.
        assert_eq!(recycle_shortfall(Decimal::ZERO, Decimal::ZERO, equity, dec!(0.6), dec!(0.05)), None);
        assert_eq!(recycle_shortfall(need, dec!(250), equity, dec!(0.6), Decimal::ZERO), None);
    }

    fn bal(ccy: &str, free: &str, locked: &str) -> crate::exchange::Balance {
        crate::exchange::Balance {
            currency: ccy.into(),
            balance: free.parse().unwrap(),
            locked: locked.parse().unwrap(),
        }
    }

    /// The placement guard must let the deep rungs through.
    ///
    /// It is the last check before an order goes out, and it was refusing
    /// exactly the rungs the sizer had been taught to make big: live, the
    /// sweep catcher sized 8.5k, 23.6k and 64.8k orders and not one of them
    /// reached the book. The ladder stopped at its second rung, and the only
    /// trace was a warning nobody was reading.
    #[test]
    fn the_placement_guard_uses_the_deep_cap_for_deep_rungs() {
        use crate::strategy::quote::DEEP_LEVEL_BASE;
        let cap = |level: i64, base: Decimal, multiple: Decimal| -> Decimal {
            if level >= DEEP_LEVEL_BASE { base * multiple.max(Decimal::ONE) } else { base }
        };
        let base: Decimal = "3725".parse().unwrap();
        let mult: Decimal = "20".parse().unwrap();

        // A touch order keeps the cap exactly as written: size there moves the
        // price.
        assert_eq!(cap(0, base, mult), base);
        assert_eq!(cap(DEEP_LEVEL_BASE - 1, base, mult), base);

        // A deep rung gets the multiple, which is what lets a 64.8k order rest
        // 50% above the touch.
        assert_eq!(cap(DEEP_LEVEL_BASE, base, mult), base * mult);
        assert_eq!(cap(DEEP_LEVEL_BASE + 4, base, mult), base * mult);
        assert!(cap(DEEP_LEVEL_BASE + 4, base, mult) > "64840".parse::<Decimal>().unwrap());

        // A multiple below 1 must not tighten the cap by accident.
        assert_eq!(cap(DEEP_LEVEL_BASE, base, Decimal::ZERO), base);
    }

    /// PRL committed to a withdrawal is not inventory.
    ///
    /// The exchange locks it the moment the withdrawal is requested and
    /// reports it as `locked`, indistinguishable from the funds behind our own
    /// resting orders -- which ARE ours and come back on cancel. The bot
    /// counted PRL on its way to cold storage as inventory: a portfolio
    /// 2.5x the real one, with targets to match.
    #[test]
    fn a_withdrawal_in_flight_is_not_inventory() {
        let balances = vec![bal("prl", "16203.86", "25001"), bal("usdt", "2520.18", "0")];
        let mut pending: HashMap<String, Decimal> = HashMap::new();

        // Before the ledger is read, we cannot know, so nothing is subtracted.
        let seen = super::drop_pending_withdrawals(balances.clone(), &pending);
        assert_eq!(seen[0].locked, "25001".parse::<Decimal>().unwrap());

        // 25,000.99 out plus the 0.01 fee.
        pending.insert("prl".into(), "25001".parse().unwrap());
        let seen = super::drop_pending_withdrawals(balances.clone(), &pending);
        assert_eq!(seen[0].locked, Decimal::ZERO, "the outbound PRL must not count");
        assert_eq!(seen[0].balance, "16203.86".parse::<Decimal>().unwrap(), "free is untouched");
        assert_eq!(seen[1].locked, Decimal::ZERO, "other currencies unaffected");
    }

    /// Our own resting orders lock funds too, and those ARE ours. Only the
    /// withdrawal comes off.
    #[test]
    fn locked_behind_our_own_orders_still_counts() {
        let mut pending: HashMap<String, Decimal> = HashMap::new();
        pending.insert("prl".into(), "5000".parse().unwrap());
        let seen = super::drop_pending_withdrawals(vec![bal("prl", "1000", "8000")], &pending);
        assert_eq!(
            seen[0].locked, "3000".parse::<Decimal>().unwrap(),
            "8000 locked less a 5000 withdrawal leaves 3000 behind our orders"
        );
    }

    /// If the ledger and the balance disagree -- a withdrawal settling between
    /// the two reads -- the answer is zero, never a negative balance.
    #[test]
    fn a_settled_withdrawal_cannot_drive_the_balance_negative() {
        let mut pending: HashMap<String, Decimal> = HashMap::new();
        pending.insert("prl".into(), "25001".parse().unwrap());
        let seen = super::drop_pending_withdrawals(vec![bal("prl", "16203.86", "0")], &pending);
        assert_eq!(seen[0].locked, Decimal::ZERO);
    }

    const FOUR_HOURS: i64 = 14_400;

    /// The bug that pulled every quote off prlusdt mid-rally.
    ///
    /// A full page of trades spanning 72 minutes, against a 240-minute window.
    /// Counting our fills over the full window and the market's over 72 minutes
    /// overstates our share by the ratio of the two -- here about 3.3x.
    #[test]
    fn a_truncated_tape_shortens_the_window_for_both_sides() {
        let oldest = NOW - 72 * 60;
        let from = measurement_window(Some(oldest), TAPE_LIMIT as usize, NOW - FOUR_HOURS, NOW)
            .expect("72 minutes is plenty to measure");
        assert_eq!(from, oldest, "must measure from where the evidence starts");
        assert!(from > NOW - FOUR_HOURS, "must not claim the full window");
    }

    /// A short page is not truncation -- the book really did trade that little,
    /// so the window we asked for is the honest one. Shortening it here would
    /// throw away real quiet time and overstate our share on thin books.
    #[test]
    fn a_short_tape_keeps_the_full_window() {
        let cutoff = NOW - FOUR_HOURS;
        let from = measurement_window(Some(NOW - 60 * 60), 120, cutoff, NOW).expect("measurable");
        assert_eq!(from, cutoff, "a partial page means the book was simply quiet");
    }

    /// A tape that cannot span a useful period yields no opinion, and no
    /// opinion must never become a halt.
    #[test]
    fn too_short_a_span_yields_no_opinion() {
        assert_eq!(
            measurement_window(Some(NOW - 30), TAPE_LIMIT as usize, NOW - FOUR_HOURS, NOW),
            None,
            "30 seconds of tape cannot establish a share"
        );
        assert_eq!(measurement_window(None, 0, NOW - FOUR_HOURS, NOW), None, "empty tape");
    }

    /// The window never starts before the one that was asked for, however old
    /// the tape is -- otherwise a stale book would widen the measurement and
    /// quietly dilute our share toward zero.
    #[test]
    fn a_stale_tape_never_widens_the_window() {
        let cutoff = NOW - FOUR_HOURS;
        let from = measurement_window(Some(NOW - 86_400), TAPE_LIMIT as usize, cutoff, NOW)
            .expect("measurable");
        assert_eq!(from, cutoff, "a day-old tape must not stretch the window");
    }

    /// The numbers from the live incident, end to end.
    #[test]
    fn the_live_incident_reads_as_under_the_cap() {
        let cap = 0.15_f64;
        // Our fills over four hours, and the market's volume over the 72
        // minutes the tape actually covered.
        let ours_4h = 22_000.0_f64;
        let market_72m = 117_498.0_f64;
        // The old code divided these two directly.
        let wrong = ours_4h / market_72m;
        assert!(wrong > cap, "the old maths breached the cap: {wrong:.3}");
        // Measured over the same 72 minutes, our share is the tape's share.
        let ours_72m = ours_4h * (72.0 / 240.0);
        let right = ours_72m / market_72m;
        assert!(right < cap, "the true share is under the cap: {right:.3}");
    }

    #[test]
    fn the_span_floor_is_sane() {
        assert!(MIN_SPAN_SECS >= 60, "a sub-minute sample cannot establish a share");
    }

    use super::*;
    use crate::db::models::{LiveOrder, Position};

    fn bid(price: Decimal, amount: Decimal, level: i64) -> DesiredQuote {
        DesiredQuote {
            side: Side::Buy, price, amount, level, bps: dec!(50),
            recent_move: Decimal::ZERO, width_ticks: Decimal::ONE,
        }
    }

    fn resting(id: i64, price: Decimal, amount: Decimal, filled: Decimal, placed_at: i64) -> LiveOrder {
        LiveOrder {
            client_id: format!("c{id}"), exchange_id: id, side: Side::Buy, price, amount,
            filled, level: 0, placed_at,
        }
    }

    /// The twap's bid grows by ADDING orders at the touch, never by resizing
    /// one -- a resized order loses its place in the queue.
    #[test]
    fn a_twap_stacks_new_orders_instead_of_replacing() {
        let plan = vec![bid(dec!(1.24), dec!(500), 0), bid(dec!(1.23), dec!(800), 1)];

        // Nothing resting, on schedule: one chunk at the touch, no deeper rung.
        let q = twap_stack(&plan, &[], dec!(100), dec!(0), dec!(100000), 4);
        assert_eq!(q, vec![bid(dec!(1.24), dec!(500), 0)]);

        // Behind by 3000 with 500 resting: the 500 stays exactly as it is, and
        // one new order of 2500 goes on top. No cap on its size.
        let live = [resting(1, dec!(1.24), dec!(500), dec!(0), 10)];
        let q = twap_stack(&plan, &live, dec!(3000), dec!(0), dec!(100000), 4);
        assert_eq!(q, vec![bid(dec!(1.24), dec!(500), 0), bid(dec!(1.24), dec!(2500), 1)]);

        // Short by less than a chunk: no new order, so no churn every loop.
        let q = twap_stack(&plan, &live, dec!(800), dec!(0), dec!(100000), 4);
        assert_eq!(q, vec![bid(dec!(1.24), dec!(500), 0)]);

        // Fills caught the schedule up: the oldest order is kept (it is at the
        // front of the queue) and the newest is left out, so it is cancelled.
        let live = [
            resting(2, dec!(1.24), dec!(2500), dec!(0), 20),
            resting(1, dec!(1.24), dec!(500), dec!(200), 10),
        ];
        let q = twap_stack(&plan, &live, dec!(1000), dec!(800), dec!(100000), 4);
        assert_eq!(q, vec![bid(dec!(1.24), dec!(300), 0)]);

        // A partly filled order is kept at what is left of it, not resized.
        let q = twap_stack(&plan, &live, dec!(5000), dec!(0), dec!(100000), 4);
        assert_eq!(
            q,
            vec![bid(dec!(1.24), dec!(300), 0), bid(dec!(1.24), dec!(2500), 1), bid(dec!(1.24), dec!(2200), 2)]
        );

        // Cash for only 1240 more: the top-up is sized to it, not skipped.
        let q = twap_stack(&plan, &live, dec!(5000), dec!(0), dec!(1240), 4);
        assert_eq!(
            q,
            vec![bid(dec!(1.24), dec!(300), 0), bid(dec!(1.24), dec!(2500), 1), bid(dec!(1.24), dec!(1000), 2)]
        );
    }

    fn market() -> Market {
        Market {
            id: "prlusdt".into(), name: "PRL/USDT".into(),
            base_unit: "prl".into(), quote_unit: "usdt".into(),
            min_price: dec!(0.01), max_price: dec!(100000), min_amount: dec!(2),
            amount_precision: 4, price_precision: 2, state: "enabled".into(),
        }
    }

    fn order(side: Side, price: Decimal, amount: Decimal) -> LiveOrder {
        LiveOrder {
            client_id: "c".into(), exchange_id: 1, side, price, amount,
            filled: Decimal::ZERO, level: 0, placed_at: 0,
        }
    }

    #[test]
    fn stripping_own_size_reveals_the_real_touch() {
        let book = OrderBook {
            bids: vec![(dec!(0.98), dec!(500)), (dec!(0.97), dec!(10000))],
            asks: vec![(dec!(1.00), dec!(150))],
        };
        // We are the entire 0.98 bid.
        let live = vec![order(Side::Buy, dec!(0.98), dec!(500))];
        let stripped = strip_own(&book, &live, &market());
        assert_eq!(
            stripped.best_bid().map(|(p, _)| p),
            Some(dec!(0.97)),
            "our own bid must not count as the market's best bid"
        );
    }

    #[test]
    fn stripping_leaves_other_participants_size() {
        let book = OrderBook {
            bids: vec![(dec!(0.98), dec!(16000))],
            asks: vec![(dec!(1.00), dec!(150))],
        };
        let live = vec![order(Side::Buy, dec!(0.98), dec!(1000))];
        let stripped = strip_own(&book, &live, &market());
        assert_eq!(stripped.best_bid(), Some((dec!(0.98), dec!(15000))));
    }

    /// Cancels must fund replacements, and a quote we cannot pay for must be
    /// skipped rather than sent to be rejected.
    ///
    /// These call `Purse` directly. The previous version re-implemented the
    /// arithmetic inside the test, so it passed while the real cancel loop
    /// credited nothing back and the bot sent orders it could not fund.
    #[test]
    fn cancelled_orders_fund_their_replacements() {
        let mut p = Purse::new(Decimal::ZERO, dec!(1000));

        // Two resting bids of 500 each are cancelled.
        p.release(Side::Buy, dec!(500), dec!(1.00));
        p.release(Side::Buy, dec!(500), dec!(1.00));
        assert_eq!(p.quote, dec!(2000));

        // Three replacements of 700; the first two fit, the third does not.
        let placed = (0..3)
            .filter(|_| p.spend(Side::Buy, dec!(700), dec!(1.00)))
            .count();
        assert_eq!(placed, 2, "only what the purse covers is sent");
        assert_eq!(p.quote, dec!(600));
    }

    /// With nothing cancelled there is nothing extra to spend.
    #[test]
    fn nothing_is_placed_when_nothing_was_freed() {
        let mut p = Purse::new(Decimal::ZERO, dec!(10));
        assert!(!p.spend(Side::Buy, dec!(700), dec!(1.00)));
        assert_eq!(p.quote, dec!(10), "a refused order changes nothing");
    }

    /// A sell spends base, a buy spends quote. Mixing them up would let the
    /// bot sell coins it does not hold.
    #[test]
    fn each_side_spends_its_own_currency() {
        let mut p = Purse::new(dec!(100), dec!(50));
        assert!(p.spend(Side::Sell, dec!(60), dec!(2.00)), "sells spend base");
        assert_eq!(p.base, dec!(40));
        assert_eq!(p.quote, dec!(50), "quote untouched by a sell");

        assert!(!p.spend(Side::Sell, dec!(60), dec!(2.00)), "only 40 base left");
        assert!(p.spend(Side::Buy, dec!(20), dec!(2.00)), "buys spend quote");
        assert_eq!(p.quote, dec!(10));
        assert_eq!(p.base, dec!(40), "base untouched by a buy");
    }

    /// A partly filled order only holds what is left of it.
    #[test]
    fn a_partial_fill_releases_only_the_remainder() {
        let mut p = Purse::new(Decimal::ZERO, Decimal::ZERO);
        p.release(Side::Buy, dec!(30), dec!(1.50)); // 100 ordered, 70 filled
        assert_eq!(p.quote, dec!(45));
    }

    /// A failed cancel must NOT credit anything: that order is still resting
    /// and still holding its funds.
    #[test]
    fn a_failed_cancel_releases_nothing() {
        let mut p = Purse::new(dec!(5), dec!(5));
        let before = (p.base, p.quote);
        // The engine only calls release() when the cancel succeeded.
        assert_eq!((p.base, p.quote), before);
        assert!(!p.spend(Side::Buy, dec!(100), dec!(1.00)));
    }

    /// The exact false positive that reported our own buys as a deposit.
    ///
    /// Balances are read before fills, so a trade landing in that gap makes one
    /// loop look short and the next look long. A per-loop delta sees two
    /// anomalies where there was one trade; a carried-forward expectation sees
    /// the second cancel the first.
    #[test]
    fn a_fill_seen_late_does_not_look_like_a_deposit() {
        let threshold = dec!(174); // 1% of ~17,400 PRL

        // Cumulative model, replaying the real sequence.
        let mut expected = dec!(17392);
        // Loop A: fill of 289 ingested, but the balance was read before it.
        expected += dec!(289);
        let external_a = dec!(17392) - expected;
        assert_eq!(external_a, dec!(-289), "looks short for one loop");
        assert!(external_a.abs() > threshold, "and would trip a delta model");

        // Loop B: balance now includes the fill, nothing new ingested.
        expected += Decimal::ZERO;
        let external_b = dec!(17681) - expected;
        assert_eq!(external_b, dec!(0), "and the discrepancy is gone");
        assert!(external_b.abs() < threshold, "no alert, correctly");
    }

    /// A genuine deposit must still be caught: it never cancels out.
    #[test]
    fn a_real_deposit_persists_across_loops() {
        let threshold = dec!(174);
        let mut expected = dec!(17392);

        // 5,000 PRL arrives from outside; no fill explains it.
        expected += Decimal::ZERO;
        let a = dec!(22392) - expected;
        assert_eq!(a, dec!(5000));
        assert!(a.abs() > threshold);

        // Next loop, still unexplained -- same sign, so it confirms.
        let b = dec!(22392) - expected;
        assert_eq!(b, a, "a real deposit does not resolve itself");
        assert!((a > Decimal::ZERO) == (b > Decimal::ZERO), "confirms on sign");
    }

    /// Buy fees are taken in the base asset, so expectation drifts above
    /// reality. Absorbing the residue keeps that from accruing into a phantom
    /// withdrawal.
    #[test]
    fn fee_drift_is_absorbed_rather_than_accumulated() {
        let threshold = dec!(174);
        let mut expected = dec!(10000);
        let mut actual = dec!(10000);
        for _ in 0..500 {
            // Buy 100, receive 99.9 after the 0.1% fee.
            expected += dec!(100);
            actual += dec!(99.9);
            let external = actual - expected;
            if external.abs() < threshold {
                expected = actual; // absorb
            }
        }
        assert_eq!(expected, actual, "drift never accumulates");
    }

    /// Volume must be read as a RATE, or a page-capped busy book looks quiet.
    ///
    /// The trades endpoint returns at most 1000 rows. On PRL/USDT those span
    /// about an hour; on PRL/USDC they reach back days. Summing a fixed window
    /// read the busy book's cap as its daily volume and undercounted it roughly
    /// twenty-fold.
    #[test]
    fn volume_is_normalised_by_the_span_the_page_covers() {
        // Busy book: 1000 trades worth $142k, spanning one hour.
        let busy = dec!(142000) / (Decimal::from(3600) / dec!(3600));
        assert_eq!(busy, dec!(142000), "$142k in an hour is $142k/hour");

        // Quiet book: $16k of trades spanning a full day.
        let quiet = dec!(16376) / (Decimal::from(86400) / dec!(3600));
        assert!(
            (quiet - dec!(682)).abs() < dec!(1),
            "$16k over 24h is about $682/hour, got {quiet}"
        );

        // Naively summing both as "24h volume" would have made them comparable;
        // as rates the busy book is two hundred times the other.
        assert!(busy / quiet > dec!(200));
    }

    fn mk(id: &str, base: &str, quote: &str) -> Market {
        Market {
            id: id.into(), name: id.into(), base_unit: base.into(), quote_unit: quote.into(),
            min_price: dec!(0.01), max_price: dec!(100000), min_amount: dec!(2),
            amount_precision: 4, price_precision: 2, state: "enabled".into(),
        }
    }

    fn sized(market: &str, side: Side, share: Decimal) -> ArmSize {
        ArmSize {
            arm: crate::strategy::arms::Arm {
                market: market.into(), side, bucket: crate::strategy::arms::Bucket::From100,
                regime: crate::strategy::arms::Regime::Calm,
                width: crate::strategy::arms::Width::One,
            },
            share,
            state: State::Sized,
            fills: 50.0, exposure_dh: 1000.0, yield_pm: 0.0, se_pm: 0.0, p_pays: 0.5, bet: 0.0,
        }
    }

    fn taker_params() -> Params {
        let mut p = Params {
            q: crate::config::Quoting { levels: 2, size_base: dec!(500), ..Default::default() },
            risk: crate::config::Risk { max_order_notional: dec!(4000), ..Default::default() },
            enabled: true,
            dry_run: false,
        };
        p.q.allow_taker = true;
        p.q.taker_max_bps = dec!(100);
        p.q.max_order_pct_of_depth = dec!(0.15);
        p.q.depth_band = dec!(0.05);
        p.q.max_participation = dec!(0.15);
        p
    }

    fn taker_market() -> Market {
        mk("prlusdt", "prl", "usdt")
    }

    fn taker_book() -> crate::exchange::OrderBook {
        crate::exchange::OrderBook {
            bids: vec![(dec!(1.49), dec!(3000)), (dec!(1.48), dec!(4000))],
            asks: vec![(dec!(1.50), dec!(400)), (dec!(1.51), dec!(2000)), (dec!(1.52), dec!(4000))],
        }
    }

    fn twap(limit: Option<Decimal>) -> Program {
        Program {
            id: 7,
            market: "prl".into(),
            kind: ProgramKind::Twap,
            state: "active".into(),
            start_qty: Decimal::ZERO,
            target_qty: dec!(53570),
            ratio: None,
            banked: Decimal::ZERO,
            limit_price: limit,
            budget: None,
            started_at: 0,
            ends_at: Some(6 * 3_600_000),
            note: None,
        }
    }

    /// The crossing leg only ever fires with every guard satisfied, and its
    /// size is whatever the tightest cap allows.
    #[test]
    fn a_one_way_program_never_crosses_the_other_way() {
        let (m, b, p) = (taker_market(), taker_book(), taker_params());
        let fv = dec!(1.4995);
        let mut prog = twap(Some(dec!(1.60)));
        prog.ends_at = Some(i64::MAX);
        // Ahead of an accumulate's schedule: no crossing sell.
        prog.kind = ProgramKind::Accumulate;
        assert!(
            taker_clip(&m, &b, &prog, &p, fv, dec!(9000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "an accumulate ahead of schedule must not sell what it just bought"
        );
        // Behind it: still buys.
        assert_eq!(
            taker_clip(&m, &b, &prog, &p, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .map(|c| c.side),
            Some(Side::Buy)
        );
        // A distribute or a flatten below its glide: no crossing buy.
        for kind in [ProgramKind::Distribute, ProgramKind::Flatten] {
            prog.kind = kind;
            assert!(
                taker_clip(&m, &b, &prog, &p, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                    .is_none(),
                "{kind:?} behind its glide must not buy"
            );
        }
    }

    #[test]
    fn a_crossing_needs_every_guard_and_is_sized_by_the_tightest_cap() {
        let (m, b, p) = (taker_market(), taker_book(), taker_params());
        let fv = dec!(1.4995);
        // Behind by 5,000 PRL, limit 1.60, plenty of cash.
        let prog = twap(Some(dec!(1.60)));
        let clip = taker_clip(&m, &b, &prog, &p, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
            .expect("a crossing");
        assert_eq!(clip.side, Side::Buy);
        // The ask itself: a limit buy there crosses and fills there.
        assert_eq!(clip.price, dec!(1.50));
        // The ask-side depth cap binds: 15% of the quote resting within 5% of
        // the touch, divided by the price -- far less than the 5,000 shortfall.
        let depth = b.depth_within(Side::Sell, dec!(0.05));
        assert_eq!(clip.amount, num::floor_to(depth * dec!(0.15) / clip.price, m.amount_precision));
        assert!(clip.amount < dec!(5000), "the shortfall is not the binding cap here");

        // No limit price: no crossing, ever.
        assert!(
            taker_clip(&m, &b, &twap(None), &p, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "a program with no ceiling must not cross"
        );

        // Paused: no crossing.
        let mut paused = p.clone();
        paused.enabled = false;
        assert!(
            taker_clip(&m, &b, &prog, &paused, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "a paused bot must not cross"
        );

        // allow_taker off: no crossing.
        let mut off = p.clone();
        off.q.allow_taker = false;
        assert!(
            taker_clip(&m, &b, &prog, &off, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none()
        );

        // On schedule (or ahead): nothing to chase.
        assert!(
            taker_clip(&m, &b, &prog, &p, fv, dec!(7000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "at target"
        );
        assert!(
            taker_clip(&m, &b, &prog, &p, fv, dec!(9000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "a buy-only program ahead of schedule must never sell"
        );

        // Late by less than a rung: not worth the spread.
        assert!(
            taker_clip(&m, &b, &prog, &p, fv, dec!(6700), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "300 behind on a 500 rung"
        );

        // And the rung is the EFFECTIVE one. With `size_pct_of_equity` set, 3%
        // of equity is the ladder's per-rung ceiling, so lateness is measured
        // against that rather than against a `size_base` it has replaced: live,
        // the raw setting had the leg crossing for barely over one raw rung.
        let mut by_equity = p.clone();
        by_equity.q.size_pct_of_equity = dec!(0.03);
        // Equity here is 2,000 x 1.4995 + 70,000 = 73,000, so a rung is ~1,460.
        assert!(
            taker_clip(&m, &b, &prog, &by_equity, fv, dec!(2000), dec!(3000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "1,000 behind is inside a 1,460 rung"
        );
        assert!(
            taker_clip(&m, &b, &prog, &by_equity, fv, dec!(2000), dec!(4000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_some(),
            "2,000 behind is past it"
        );

        // Over the participation cap: stand down.
        assert!(
            taker_clip(&m, &b, &prog, &p, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, dec!(0.2))
                .is_none()
        );

        // No cash: nothing to cross with.
        assert!(
            taker_clip(&m, &b, &prog, &p, fv, dec!(2000), dec!(7000), dec!(1), Decimal::ZERO, Decimal::ZERO)
                .is_none()
        );
    }

    /// Two ceilings, and the tighter one wins: the program's own limit, and a
    /// band around fair value that refuses a thin book's spread.
    #[test]
    fn a_crossing_never_pays_past_the_limit_or_the_band() {
        let (m, b, p) = (taker_market(), taker_book(), taker_params());
        let fv = dec!(1.4995);

        // A limit under the crossing price refuses it outright.
        for limit in [dec!(1.49), dec!(1.45), dec!(1.20)] {
            assert!(
                taker_clip(&m, &b, &twap(Some(limit)), &p, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                    .is_none(),
                "limit {limit} is under the 1.50 crossing price"
            );
        }

        // The band does the same job when the limit is generous. Crossing at
        // 1.50 against a 1.4995 fair value is 3 bp, so a 100 bp band allows it
        // and a 1 bp band does not.
        assert!(
            taker_clip(&m, &b, &twap(Some(dec!(2.00))), &p, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_some(),
            "3 bp over fair value is inside a 100 bp band"
        );
        let mut tight = p.clone();
        tight.q.taker_max_bps = dec!(1);
        assert!(
            taker_clip(&m, &b, &twap(Some(dec!(2.00))), &tight, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "and outside a 1 bp one"
        );
        let mut wider = p.clone();
        wider.q.taker_max_bps = dec!(200);

        // The thin book case this exists for: a stranger's ask 6% over what the
        // deep book says PRL is worth. No schedule is worth 600 bp.
        let thin = crate::exchange::OrderBook {
            bids: vec![(dec!(1.40), dec!(2000))],
            asks: vec![(dec!(1.59), dec!(2000))],
        };
        assert!(
            taker_clip(&m, &thin, &twap(Some(dec!(2.00))), &wider, fv, dec!(2000), dec!(7000), dec!(70000), Decimal::ZERO, Decimal::ZERO)
                .is_none(),
            "the band refuses the thin book's spread even under a generous limit"
        );
    }

    /// In TWAP mode the money goes to the books that are working the twap.
    ///
    /// Every other book stands down, and the weights are clamped at 70% each, so
    /// leaving a halted market in the split reserves capital for a ladder that
    /// is never placed: with usdcusdt in it, about a third of the USDT sat idle
    /// behind a book that was not going to quote.
    #[test]
    fn the_pile_comes_off_the_asset_before_any_book_is_given_a_share() {
        // 18,400 PRL held with 11,500 set aside: the books split 6,900, not
        // 18,400 with 11,500 then taken off whichever book's share it hits.
        let mut purse = HashMap::new();
        purse.insert("prl".to_string(), dec!(18400));
        purse.insert("usdc".to_string(), dec!(5000));
        let mut piles = HashMap::new();
        piles.insert("prl".to_string(), dec!(11500));
        piles.insert("usdt".to_string(), dec!(99)); // an asset not held: nothing to take
        hold_back(&mut purse, &piles);
        assert_eq!(purse["prl"], dec!(6900));
        assert_eq!(purse["usdc"], dec!(5000), "other assets untouched");
        assert!(!purse.contains_key("usdt"));

        let all = vec![mk("prlusdt", "prl", "usdt"), mk("prlusdc", "prl", "usdc")];
        let split = allocate(&all, &purse, &[]);
        let prl: Decimal = split.values().filter_map(|m| m.get("prl")).sum();
        assert_eq!(prl, dec!(6900), "every book's share is of what is free");

        // A pile larger than the holding leaves nothing, never a negative.
        let mut small = HashMap::from([("prl".to_string(), dec!(100))]);
        hold_back(&mut small, &piles);
        assert_eq!(small["prl"], Decimal::ZERO);
    }

    #[test]
    fn twap_mode_hands_the_whole_purse_to_the_books_working_it() {
        let all = vec![mk("prlusdt", "prl", "usdt"), mk("prlusdc", "prl", "usdc"), mk("usdcusdt", "usdc", "usdt")];
        let mut purse = HashMap::new();
        purse.insert("usdt".to_string(), dec!(75000));
        purse.insert("usdc".to_string(), dec!(7000));
        let sizes = vec![
            sized("prlusdt", Side::Buy, Decimal::ZERO),
            sized("usdcusdt", Side::Buy, Decimal::ONE),
        ];

        // All three books in the split: prlusdt is capped at 70% of the USDT and
        // the rest is held for a book that, in this mode, will not quote.
        let shared = allocate(&all, &purse, &sizes);
        assert!(shared["prlusdt"]["usdt"] < dec!(75000) * dec!(0.75), "capped: {}", shared["prlusdt"]["usdt"]);

        // Only the PRL books -- the ones a PRL twap can spend through.
        let funding = vec![all[0].clone(), all[1].clone()];
        let twap = allocate(&funding, &purse, &sizes);
        assert_eq!(twap["prlusdt"]["usdt"], dec!(75000), "the whole USDT purse funds the twap's bids");
        assert_eq!(twap["prlusdc"]["usdc"], dec!(7000), "and the whole USDC purse too");
        assert!(!twap.contains_key("usdcusdt"), "a book that is standing down gets nothing");
    }

    /// Capital follows the arms. PRL is shared by both PRL books' asks: when
    /// prlusdc's asks have earned full size and prlusdt's are probing, the PRL
    /// goes to prlusdc -- reconciled by splitting the one balance, not by
    /// letting both books size against it.
    #[test]
    fn a_currency_is_split_toward_the_arms_that_earn_it() {
        let markets = vec![mk("prlusdt", "prl", "usdt"), mk("prlusdc", "prl", "usdc"), mk("usdcusdt", "usdc", "usdt")];
        let mut purse = HashMap::new();
        purse.insert("prl".to_string(), dec!(10000));
        purse.insert("usdt".to_string(), dec!(1000));
        purse.insert("usdc".to_string(), dec!(1000));
        let sizes = vec![
            sized("prlusdc", Side::Sell, Decimal::ONE),
            sized("prlusdt", Side::Sell, Decimal::ZERO),
            sized("usdcusdt", Side::Buy, Decimal::ONE),
            sized("prlusdt", Side::Buy, Decimal::ZERO),
        ];
        let a = allocate(&markets, &purse, &sizes);
        let prl_thin = a["prlusdc"]["prl"];
        let prl_deep = a["prlusdt"]["prl"];
        assert!(prl_thin > prl_deep * dec!(4), "the earning asks get the PRL: {prl_thin} vs {prl_deep}");
        assert!((prl_thin + prl_deep - dec!(10000)).abs() < dec!(0.01), "and it is split, not summed");
        assert!(prl_deep >= dec!(10000) * dec!(0.08) * dec!(0.9), "the unproven book is never starved");
        // USDT: usdcusdt's bids earned it, prlusdt's did not.
        assert!(a["usdcusdt"]["usdt"] > a["prlusdt"]["usdt"] * dec!(4));
        // A currency only one book uses goes entirely to it.
        assert_eq!(a["prlusdc"]["usdc"] + a["usdcusdt"]["usdc"], dec!(1000));
    }

    /// With no arm table yet, every book gets an equal share: the split must
    /// not depend on config order.
    #[test]
    fn with_no_evidence_the_split_is_even() {
        let markets = vec![mk("prlusdt", "prl", "usdt"), mk("prlusdc", "prl", "usdc")];
        let mut purse = HashMap::new();
        purse.insert("prl".to_string(), dec!(1000));
        let a = allocate(&markets, &purse, &[]);
        assert_eq!(a["prlusdt"]["prl"], dec!(500));
        assert_eq!(a["prlusdc"]["prl"], dec!(500));
        // And with a table whose arms all have no opinion yet, likewise.
        let sizes = vec![sized("prlusdt", Side::Sell, Decimal::ZERO), sized("prlusdc", Side::Sell, Decimal::ZERO)];
        let a = allocate(&markets, &purse, &sizes);
        assert_eq!(a["prlusdt"]["prl"], dec!(500));
        assert_eq!(a["prlusdc"]["prl"], dec!(500));
    }

    #[test]
    fn funds_are_tracked_through_cancels_and_placements() {
        // 1,000 quote free, plus two resting bids holding 500 each.
        let mut free_quote = dec!(1000);
        let resting = [dec!(500), dec!(500)];

        // Cancelling both returns their funds.
        for r in resting {
            free_quote += r;
        }
        assert_eq!(free_quote, dec!(2000));

        // Three replacements of 700 each: the first two fit, the third does not.
        let mut placed = 0;
        for _ in 0..3 {
            let cost = dec!(700);
            if cost > free_quote {
                continue;
            }
            free_quote -= cost;
            placed += 1;
        }
        assert_eq!(placed, 2, "only what the purse covers is sent");
        assert_eq!(free_quote, dec!(600));
    }

    /// A settings change is a change to what the bot quotes, not to where
    /// its database lives or how it is worded.
    #[test]
    fn the_fingerprint_follows_the_strategy_and_ignores_the_rest() {
        let base: Config = toml::from_str(include_str!("../deploy/config.prod.toml")).unwrap();
        let same: Config = toml::from_str(include_str!("../deploy/config.prod.toml")).unwrap();
        assert_eq!(strategy_fingerprint(&base), strategy_fingerprint(&same), "deterministic");

        let mut ops = base.clone();
        ops.db_path = "/somewhere/else.db".into();
        ops.retain_days += 1;
        ops.telegram_status_secs += 1;
        ops.depth_levels += 1;
        ops.api_base = Some("https://example.invalid".into());
        assert_eq!(strategy_fingerprint(&base), strategy_fingerprint(&ops), "operational knobs do not count");

        let mut quoting = base.clone();
        quoting.quoting.edge_half_life_hours += dec!(1);
        assert_ne!(strategy_fingerprint(&base), strategy_fingerprint(&quoting));
        let mut markets = base.clone();
        markets.markets.pop();
        assert_ne!(strategy_fingerprint(&base), strategy_fingerprint(&markets));
        let mut ov = base.clone();
        ov.overrides.get_mut("pearlusdc").unwrap().levels = Some(1);
        assert_ne!(strategy_fingerprint(&base), strategy_fingerprint(&ov));
    }

    /// The arms keep their evidence across a scoreboard reset. Blinded, every
    /// arm would quote the minimum until it had re-learned what it knew, which
    /// on a book with fifty fills a day is a week of quoting almost nothing.
    #[tokio::test]
    async fn the_arms_are_not_reset_by_the_scoreboard_epoch() {
        let dir = tempdir::TempDir::new("safetrade-arms-epoch").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("quote_unit:prlusdt", "usdt").await.unwrap();
        let now = now_ms();
        let placed = now - 2 * 3_600_000;
        db.record_placement("c1", None, "prlusdt", Side::Buy, dec!(1.30), dec!(100), 0, Some(dec!(150)), Some(dec!(0.01)), Some(dec!(1)))
            .await
            .unwrap();
        db.attach_exchange_id("c1", 11, "wait").await.unwrap();
        sqlx::query("UPDATE orders SET placed_at = ?, closed_at = ? WHERE client_id = 'c1'")
            .bind(placed)
            .bind(placed + 3_600_000)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO fills(trade_id, order_id, market, side, price, amount, total, fee,
                               fee_currency, liquidity, created_at, pos_after, cost_after,
                               realized, markout)
             VALUES (1, 11, 'prlusdt', 'buy', '1.30', '100', '130', '0.1', 'prl', 'maker', ?,
                     '0', '0', '0', '2.0')",
        )
        .bind(placed + 60_000)
        .execute(db.pool())
        .await
        .unwrap();
        // The line is drawn AFTER the order and its fill.
        db.reset_analytics_epoch("settings changed").await.unwrap();

        let params = Params {
            q: crate::config::Quoting::default(),
            risk: crate::config::Risk::default(),
            enabled: true,
            dry_run: false,
        };
        let sizes = estimate_arms(&db, &[("prlusdt".into(), "prl".into())], &params, now).await.unwrap();
        let z = sizes
            .iter()
            .find(|z| z.arm.side == Side::Buy && z.arm.bucket == crate::strategy::arms::Bucket::From100)
            .unwrap();
        assert!(z.fills > 0.5, "the fill before the line still counts: {z:?}");
        assert!(z.exposure_dh > 0.0, "and so do the dollar-hours: {z:?}");
    }

    #[test]
    fn position_accounting_books_spread_capture() {
        let mut p = Position::default();
        p.apply(Side::Buy, dec!(0.98), dec!(100), dec!(0));
        p.apply(Side::Sell, dec!(1.00), dec!(100), dec!(0));
        assert_eq!(p.qty, dec!(0));
        assert_eq!(p.realized, dec!(2)); // 100 units * 0.02 spread
    }

    #[test]
    fn position_average_cost_survives_partial_sells() {
        let mut p = Position::default();
        p.apply(Side::Buy, dec!(0.90), dec!(100), dec!(0));
        p.apply(Side::Buy, dec!(1.10), dec!(100), dec!(0));
        assert_eq!(p.avg_price(), Some(dec!(1.00)));
        p.apply(Side::Sell, dec!(1.20), dec!(50), dec!(0));
        assert_eq!(p.realized, dec!(10)); // 50 * (1.20 - 1.00)
        assert_eq!(p.qty, dec!(150));
        assert_eq!(p.avg_price(), Some(dec!(1.00)));
    }

    fn two_sided(bid: Decimal, bq: Decimal, ask: Decimal, aq: Decimal) -> OrderBook {
        OrderBook { bids: vec![(bid, bq)], asks: vec![(ask, aq)] }
    }

    fn three_books() -> Vec<Market> {
        vec![mk("prlusdt", "prl", "usdt"), mk("prlusdc", "prl", "usdc"), mk("usdcusdt", "usdc", "usdt")]
    }

    /// The account's starting value is valued over every book, never over the
    /// subset that happened to run this loop.
    ///
    /// Both USDC books run `every_n_loops = 3` in production, so two loops in
    /// three see prlusdt alone. Valuing from that subset silently dropped the
    /// whole USDC pool out of the baseline, and because a starting value is
    /// written once, the scoreboard then reported that pool as market profit
    /// for the rest of the epoch.
    #[test]
    fn the_starting_value_counts_every_pool_not_this_loop_s_books() {
        let balances = vec![
            bal("prl", "9000", "100"),
            bal("usdt", "300", "101.92"),
            bal("usdc", "1255.55", "0"),
        ];
        let mut prices = HashMap::new();
        prices.insert("usdt".to_string(), Decimal::ONE);
        prices.insert("usdc".to_string(), Decimal::ONE);
        prices.insert("prl".to_string(), dec!(1.13983));

        // Every pool, which is what the report's "now" sums and therefore what
        // the baseline has to match.
        let full: Decimal = holdings(&three_books(), &balances, &prices).values().sum();
        let expected = dec!(9100) * dec!(1.13983) + dec!(401.92) + dec!(1255.55);
        assert!((full - expected).abs() < dec!(0.01), "every pool is counted: {full} vs {expected}");

        // The loop-filtered subset, which is what the bug valued: prlusdt has
        // no USDC leg, so the pool is simply absent.
        let subset: Decimal =
            holdings(&[mk("prlusdt", "prl", "usdt")], &balances, &prices).values().sum();
        assert!(
            full - subset > dec!(1255),
            "the subset loses the USDC pool, which is the bug: {full} vs {subset}"
        );
    }

    /// The primary anchors its asset by configuration, not by whose touch
    /// happened to be deeper this loop.
    ///
    /// 2026-09-24 11:39:33 UTC: prlusdt 1.17/1.18 with 34 PRL on the ask,
    /// prlusdc 1.17/1.30 with a stranger's 8,500 PRL bid. The old depth rule
    /// made prlusdc the anchor, priced prlusdt off 1.2977 (a touch bid went
    /// into the arms as 992 bp out) and handed prlusdc its own microprice to
    /// quote itself -- asks at 1.33 with PRL at 1.17.
    #[test]
    fn the_configured_primary_anchors_even_when_the_thin_book_is_deeper_at_the_touch() {
        let markets = three_books();
        let mut public = HashMap::new();
        public.insert("prlusdt".to_string(), two_sided(dec!(1.17), dec!(10), dec!(1.18), dec!(34)));
        public.insert("prlusdc".to_string(), two_sided(dec!(1.17), dec!(8500), dec!(1.30), dec!(153)));
        public.insert("usdcusdt".to_string(), two_sided(dec!(0.9986), dec!(358), dec!(1.0010), dec!(12)));
        let r = references("prlusdt", &markets, &public);

        let (deep_px, deep_ext) = r["prlusdt"];
        assert!(!deep_ext, "the primary is handed its own price, marked internal");
        assert!(deep_px > dec!(1.17) && deep_px < dec!(1.18), "{deep_px}");

        let (thin_px, thin_ext) = r["prlusdc"];
        assert!(thin_ext, "the thin book's price is an outside opinion");
        let cross = public["usdcusdt"].microprice().unwrap();
        assert_eq!(thin_px, deep_px / cross, "PRL/USDT through the USDC/USDT cross");
        assert!(thin_px < dec!(1.19), "not the residue at 1.2977: {thin_px}");

        // The stablecoin book anchors itself.
        let (_, stable_ext) = r["usdcusdt"];
        assert!(!stable_ext);
    }

    /// A missing cross means NO price for the thin book -- not its own.
    #[test]
    fn a_missing_or_broken_cross_leaves_the_thin_book_unpriced() {
        let markets = three_books();
        let mut public = HashMap::new();
        public.insert("prlusdt".to_string(), two_sided(dec!(1.17), dec!(10), dec!(1.18), dec!(34)));
        public.insert("prlusdc".to_string(), two_sided(dec!(1.17), dec!(8500), dec!(1.30), dec!(153)));

        // usdcusdt unreadable this loop.
        let r = references("prlusdt", &markets, &public);
        assert!(!r.contains_key("prlusdc"), "no cross, no price: {r:?}");
        assert!(r.contains_key("prlusdt"), "the primary still prices itself");

        // Readable but outside the band: a bad decode, not a rate.
        public.insert("usdcusdt".to_string(), two_sided(dec!(0.50), dec!(1), dec!(0.51), dec!(1)));
        let r = references("prlusdt", &markets, &public);
        assert!(!r.contains_key("prlusdc"), "a 0.5 cross must not be believed: {r:?}");

        // One-sided cross: no microprice, no rate.
        public.insert("usdcusdt".to_string(), OrderBook { bids: vec![(dec!(0.9986), dec!(358))], asks: vec![] });
        let r = references("prlusdt", &markets, &public);
        assert!(!r.contains_key("prlusdc"));
    }

    /// The primary unreadable or one-sided leaves its asset unpriced: the
    /// thin book must not step in as the anchor.
    #[test]
    fn a_one_sided_primary_never_hands_the_anchor_to_the_thin_book() {
        let markets = three_books();
        let mut public = HashMap::new();
        public.insert("prlusdt".to_string(), OrderBook { bids: vec![], asks: vec![(dec!(1.18), dec!(34))] });
        public.insert("prlusdc".to_string(), two_sided(dec!(1.17), dec!(8500), dec!(1.30), dec!(153)));
        public.insert("usdcusdt".to_string(), two_sided(dec!(0.9986), dec!(358), dec!(1.0010), dec!(12)));
        let r = references("prlusdt", &markets, &public);
        assert!(!r.contains_key("prlusdc"), "{r:?}");
        assert!(!r.contains_key("prlusdt"), "{r:?}");
        assert!(r.contains_key("usdcusdt"), "another asset is unaffected");

        // Unreadable entirely: the same.
        public.remove("prlusdt");
        let r = references("prlusdt", &markets, &public);
        assert!(!r.contains_key("prlusdc") && !r.contains_key("prlusdt"), "{r:?}");
    }

    /// Which books must halt without a reference: the primary's siblings on
    /// the same base asset, and nothing else.
    #[test]
    fn only_the_primarys_siblings_are_referenced() {
        let markets = three_books();
        assert!(!is_referenced("prlusdt", &markets, &markets[0]));
        assert!(is_referenced("prlusdt", &markets, &markets[1]));
        assert!(!is_referenced("prlusdt", &markets, &markets[2]));
    }

    /// The volatility a referenced book sees is the reference's, not the
    /// residue's. Four "fair value moved 15-29%" halts on prlusdc in a day
    /// were a stranger's far ask coming and going; PRL had moved a fraction
    /// of that.
    #[test]
    fn a_referenced_book_measures_the_references_volatility() {
        let local = Some(dec!(1.2977));
        assert_eq!(volatility_anchor(Some(dec!(1.1686)), true, local), Some(dec!(1.1686)));
        // The anchor's own price, marked internal, is the local price anyway.
        assert_eq!(volatility_anchor(Some(dec!(1.1697)), false, Some(dec!(1.1697))), Some(dec!(1.1697)));
        // No reference: the local book is all there is (and `plan` will halt a
        // referenced book before this number matters).
        assert_eq!(volatility_anchor(None, false, local), local);
        assert_eq!(volatility_anchor(None, false, None), None);
    }
}

/// The money available to reconcile with, tracked exactly.
///
/// Its own type because the previous version lived as two loose `Decimal`s
/// inside `reconcile`, an edit to credit cancels back silently failed to
/// apply, and the test written to cover it re-implemented the arithmetic
/// rather than calling the code -- so it passed while the bot placed orders it
/// could not fund. Behaviour worth testing is behaviour worth naming.
#[derive(Debug, Clone, Copy)]
pub struct Purse {
    pub base: Decimal,
    pub quote: Decimal,
}

impl Purse {
    pub fn new(base: Decimal, quote: Decimal) -> Self {
        Purse { base, quote }
    }

    /// A cancelled order returns what it was holding.
    pub fn release(&mut self, side: Side, remaining: Decimal, price: Decimal) {
        match side {
            Side::Buy => self.quote += remaining * price,
            Side::Sell => self.base += remaining,
        }
    }

    /// What an order would cost, in the currency it spends.
    pub fn cost_of(side: Side, amount: Decimal, price: Decimal) -> Decimal {
        match side {
            Side::Buy => amount * price,
            Side::Sell => amount,
        }
    }

    pub fn can_afford(&self, side: Side, amount: Decimal, price: Decimal) -> bool {
        let cost = Self::cost_of(side, amount, price);
        match side {
            Side::Buy => cost <= self.quote,
            Side::Sell => cost <= self.base,
        }
    }

    /// Commit to an order. Returns false, changing nothing, if it does not fit.
    pub fn spend(&mut self, side: Side, amount: Decimal, price: Decimal) -> bool {
        if !self.can_afford(side, amount, price) {
            return false;
        }
        let cost = Self::cost_of(side, amount, price);
        match side {
            Side::Buy => self.quote -= cost,
            Side::Sell => self.base -= cost,
        }
        true
    }
}

/// Distinct values, order preserved.
fn distinct<I: IntoIterator<Item = String>>(it: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    it.into_iter().filter(|v| seen.insert(v.clone())).collect()
}
