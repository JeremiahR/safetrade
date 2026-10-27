//! Local reporting.
//!
//! Reads a database file and nothing else — no API, no credentials — so you can
//! scp the file down from the server and read it on your laptop, or point it at
//! the live file on the box while the bot is still writing (WAL makes that
//! safe).
//!
//! The headline number is **execution vs. the time-weighted mid**: for a TWAP,
//! "did I get a good price?" means "did I beat the average price the market
//! offered while I was working?", not "is my PnL positive". That comparison is
//! what `ExecutionQuality` computes.

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::Row;

use crate::db::Db;
use crate::db::models::ProgramKind;
use crate::exchange::Side;
use crate::num;

pub struct Report {
    pub market: String,
    pub window_secs: i64,
    /// When the scoreboard started; all profit is measured from here, and the
    /// window never reaches back past it. Zero on a database the engine has
    /// never run on.
    pub epoch_ms: i64,
    /// What started it: "settings changed", "ctl epoch".
    pub epoch_reason: Option<String>,
    /// What could be taken out without starving the market making.
    pub spare: Vec<(String, Decimal)>,
    /// The base asset held back from every book (`reserve:<asset>`), and how
    /// much of it banking put there. The whole pile is safe to withdraw: the
    /// book was sized without it.
    pub set_aside: Option<(String, Decimal, Decimal)>,
    /// The pool's yield since the book started, if it has.
    pub yield_: Option<Yield>,
    pub mark: Option<Decimal>,
    pub program: Option<ProgramSummary>,
    pub weights: Option<Weights>,
    pub activity: Activity,
    pub execution: Vec<ExecutionQuality>,
    pub maker: Vec<MakerEdge>,
    /// What every arm has earned and the size that justifies, as the engine
    /// last decided it. See `strategy::arms`.
    pub arms: Vec<crate::strategy::arms::ArmSize>,
    /// What a whole cycle pays, leg by leg: acquire the base asset cheaply on
    /// the thin book, release it there or on the deep one, swap the quote
    /// currency back if the release landed in the wrong one. The arms cannot
    /// see this because they score each leg alone. See `strategy::cycle`.
    pub cycle: Option<crate::strategy::cycle::Cycle>,
    /// The floor a cycle's margin must clear, from the config, so the report
    /// shows the same bar the engine is applying.
    pub cycle_floor_bps: Decimal,
    /// Five-minute episodes since the epoch that produced a scored fill, the
    /// fills in them, and the notional those fills traded: the evidence
    /// behind the verdict.
    pub episodes: usize,
    pub scored_fills: i64,
    pub scored_notional: Decimal,
    pub lifetime: Lifetime,
    pub earnings: Earnings,
    /// The request rate the bot has learned the exchange will tolerate.
    pub api_rate: Option<String>,
    pub impact: Option<Impact>,
    pub open_orders: Vec<(String, Decimal, Decimal, i64)>,
    pub overrides: Vec<(String, String)>,
    pub recent_fills: Vec<(i64, String, Decimal, Decimal)>,
    pub errors: Vec<(i64, String, String)>,
}

/// How the account's value is split between the two pools. For a `balanced`
/// program this is the number that matters; growing *both* pools means this
/// stays near its target while the total goes up.
pub struct Pool {
    pub ccy: String,
    pub qty: Decimal,
    /// Value of one unit in the reporting currency.
    pub price: Decimal,
}

impl Pool {
    pub fn value(&self) -> Decimal {
        self.qty * self.price
    }
}

/// What the book yields, treating the whole pool -- PRL, USDT, USDC, every
/// book -- as one asset.
///
/// Both sides of the ratio are valued in the base asset at the current mark:
/// the pool's capital is what it would come to in PRL right now, and the
/// earnings are the markout of every fill since the book started, net of
/// fees, in PRL. So the percentage is a return on a single holding, the way a
/// share price would read, and a price move shifts both sides together
/// instead of showing up as yield.
///
/// The set-aside pile is NOT capital: it is what the book has already paid
/// out, and counting it would let the payouts inflate the base they are
/// measured against.
///
/// Rates are the average since the start, scaled linearly -- simple, not
/// compounded, because the earnings are set aside rather than reinvested.
pub struct Yield {
    pub base_ccy: String,
    pub since_ms: i64,
    pub days: Decimal,
    /// The pool's trading income since the start, in quote terms.
    pub earned_quote: Decimal,
    /// The same, in the base asset at the mark.
    pub earned_base: Decimal,
    /// The book's capital in the base asset: the pool less the set-aside pile.
    pub capital_base: Decimal,
    /// What the last hour earned, in quote terms and in the base asset. Only
    /// fills old enough to have a markout count, so the newest five minutes
    /// land in the next hour's figure.
    pub last_hour_quote: Decimal,
    pub last_hour_base: Decimal,
}

impl Yield {
    pub fn per_day_base(&self) -> Decimal {
        if self.days <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        self.earned_base / self.days
    }

    /// Daily yield on capital, as a fraction.
    pub fn per_day_frac(&self) -> Decimal {
        if self.capital_base <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        self.per_day_base() / self.capital_base
    }

    /// (label, days) for each horizon the owner reads.
    pub const HORIZONS: [(&'static str, i64); 4] =
        [("day", 1), ("week", 7), ("month", 30), ("year", 365)];

    fn rows(&self) -> String {
        let mut o = String::new();
        for (label, n) in Self::HORIZONS {
            let d = Decimal::from(n);
            o.push_str(&format!(
                "{:<6}{:>+10.2} {:<4}{:>+8.2}%\n",
                label,
                (self.per_day_base() * d).round_dp(2),
                self.base_ccy.to_uppercase(),
                (self.per_day_frac() * d * dec!(100)).round_dp(2),
            ));
        }
        o
    }
}

pub struct Weights {
    pub pools: Vec<Pool>,
    pub base_ccy: String,
    pub quote_ccy: String,
    pub target_weight: Option<Decimal>,
}

impl Weights {
    fn pool(&self, ccy: &str) -> Option<&Pool> {
        self.pools.iter().find(|p| p.ccy == ccy)
    }
    pub fn base_qty(&self) -> Decimal {
        self.pool(&self.base_ccy).map(|p| p.qty).unwrap_or_default()
    }
    pub fn price(&self) -> Decimal {
        self.pool(&self.base_ccy).map(|p| p.price).unwrap_or_default()
    }
    pub fn base_value(&self) -> Decimal {
        self.pool(&self.base_ccy).map(|p| p.value()).unwrap_or_default()
    }
    pub fn total(&self) -> Decimal {
        self.pools.iter().map(|p| p.value()).sum()
    }
    /// Share of total value held in the base asset. With several quote
    /// currencies this is still the number the balanced program steers.
    pub fn base_weight(&self) -> Decimal {
        let t = self.total();
        if t.is_zero() { Decimal::ZERO } else { self.base_value() / t }
    }
}

pub struct ProgramSummary {
    pub id: i64,
    /// Where the glide is asking us to be right now.
    pub waypoint: Decimal,
    pub kind: String,
    pub state: String,
    pub start_qty: Decimal,
    pub target_qty: Decimal,
    pub current: Decimal,
    pub progress: Decimal,
    pub schedule_progress: Option<Decimal>,
    pub ends_at: Option<i64>,
}

#[derive(Default)]
pub struct Activity {
    pub fills: i64,
    pub bought: Decimal,
    pub sold: Decimal,
    pub buy_notional: Decimal,
    pub sell_notional: Decimal,
    pub fees: Decimal,
}

pub struct ExecutionQuality {
    pub side: Side,
    pub our_vwap: Decimal,
    pub market_twap: Decimal,
    /// Positive means we did better than the average market price on offer.
    pub edge_pct: Decimal,
    pub samples: i64,
    pub volume: Decimal,
}

/// Market-making performance, per book.
///
/// The headline is `edge_quote`: for every fill, how far our price sat on the
/// profitable side of the market's mid at that moment, times the size.
///
/// This is the number that answers "are we good at market making", because it
/// is blind to which way the asset then moved. Realised PnL is not: on
/// average-cost accounting it folds in the price drift of inventory we happened
/// to be holding, so a rally flatters it and a selloff damns it regardless of
/// how well we quoted.
pub struct MakerEdge {
    pub market: String,
    /// The book's own recent traded volume (quote per hour) and spread, as the
    /// engine records them for this report. Nothing in the quoting reads
    /// them; capital follows the arms.
    pub market_volume: Decimal,
    pub market_spread: Decimal,
    pub fills: i64,
    pub volume_base: Decimal,
    pub volume_quote: Decimal,
    /// Sum over fills of (our price − mid at that moment) × size, signed so
    /// that selling above the mid and buying below it both count as positive.
    pub edge_quote: Decimal,
    /// The same fills judged `MARKOUT_HORIZON_MS` later.
    ///
    /// `edge_quote` compares our price to the mid at that instant, so it is
    /// positive by construction -- we always quote outside the mid. It cannot
    /// see the fill that happened precisely because the price was about to run
    /// through it. This can, and the gap between the two is the cost of being
    /// picked off.
    pub markout_quote: Decimal,
    /// How many fills have a markout yet. Recent ones do not.
    pub scored: i64,
    pub fees: Decimal,
    pub maker_fills: i64,
}

impl MakerEdge {
    /// Edge in basis points of the notional we traded. Size-independent, so it
    /// compares books of very different depth on equal terms.
    pub fn edge_bps(&self) -> Decimal {
        if self.volume_quote.is_zero() {
            Decimal::ZERO
        } else {
            self.edge_quote / self.volume_quote * dec!(10000)
        }
    }

    /// What is actually kept: gross edge less the fees paid to earn it.
    pub fn net(&self) -> Decimal {
        self.edge_quote - self.fees
    }

    pub fn net_bps(&self) -> Decimal {
        if self.volume_quote.is_zero() {
            Decimal::ZERO
        } else {
            self.net() / self.volume_quote * dec!(10000)
        }
    }

    /// Our share of everything that traded on this book over `window_secs`.
    ///
    /// `market_volume` is stored per HOUR while ours covers the whole window,
    /// so the market side must be scaled up first. Comparing them raw reported
    /// a 1.9% share as 44%, which reads as breaching the participation cap
    /// rather than sitting nowhere near it.
    pub fn share(&self, window_secs: i64) -> Decimal {
        let hours = Decimal::from(window_secs.max(1)) / dec!(3600);
        let market = self.market_volume * hours;
        if market.is_zero() {
            Decimal::ZERO
        } else {
            self.volume_quote / market * dec!(100)
        }
    }

    /// Markout per unit traded, in basis points. Directly comparable with
    /// `edge_bps`: where it is much lower, we are being picked off.
    #[cfg(test)]
    pub fn markout_bps(&self) -> Decimal {
        if self.volume_quote.is_zero() {
            Decimal::ZERO
        } else {
            self.markout_quote / self.volume_quote * dec!(10000)
        }
    }

    /// The spread, but only where it is a price someone would actually cross.
    ///
    /// On a book whose only real liquidity is ours, the posted spread is a
    /// stale residue -- prlusdc showed 1288 bp with nothing behind it. Treating
    /// that as the edge on offer makes good quoting look like a 8% capture
    /// rate, so anything wider than 500 bp is reported as unmeasured rather
    /// than as an opportunity we missed.
    pub fn sane_spread_bps(&self) -> Decimal {
        num::tradeable_spread(self.market_spread) * dec!(10000)
    }

    /// How much of the available spread we actually kept, as a percentage.
    /// `None` where the spread is not measurable -- see `sane_spread_bps`.
    pub fn capture(&self) -> Option<Decimal> {
        let spread = self.sane_spread_bps();
        (spread > Decimal::ZERO).then(|| self.net_bps() / spread * dec!(100))
    }
}

/// Trading income over time.
///
/// Every figure is what quoting earned net of fees, so it is blind to which way
/// the price went. Periods with no fills are omitted rather than shown as
/// zero: a day the bot did not trade is not a day it earned nothing, and
/// padding the series with zeroes would drag every average toward zero.
#[derive(Default)]
pub struct Earnings {
    /// Net trading income per calendar day, most recent first.
    pub daily: Vec<(String, Decimal)>,
    /// Average net income per day within each week, most recent week first.
    pub weekly: Vec<(String, Decimal, i64)>,
    /// Total since the bot started, and how long that has been.
    pub total: Decimal,
    pub hours: Decimal,
}

impl Earnings {
    /// Income per day since launch. `None` until there is enough to mean
    /// anything -- an extrapolation from twenty minutes is a guess wearing a
    /// number's clothes.
    pub fn per_day_since_start(&self) -> Option<Decimal> {
        (self.hours >= dec!(6)).then(|| self.total / self.hours * dec!(24))
    }
}

/// The account since the scoreboard started, split into the two things that
/// moved it: the market, and our own trading.
///
/// Kept separate on purpose. Folded together, a rally makes any bot look
/// brilliant and a selloff makes the same bot look broken, when in neither case
/// did it have much to do with the outcome.
///
/// Everything here starts at the epoch: the starting value is the account at
/// the line, the deposits are those since it, the trading is the markout of
/// fills after it. Nothing from an earlier regime is carried over.
#[derive(Default)]
pub struct Lifetime {
    pub start_value: Decimal,
    pub now_value: Decimal,
    /// Deposits less withdrawals since the start, valued when they happened.
    /// Not profit.
    pub net_deposits: Decimal,
    /// What quoting earned: distance from the mid on every fill, less fees.
    /// Blind to price direction.
    pub trading: Decimal,
    pub fees: Decimal,
    /// Fees belonging to the fills that have a markout. Total fees would be
    /// charged against a subset of the scoring, which invents a loss.
    pub scored_fees: Decimal,
    pub hours: Decimal,
}

impl Lifetime {
    /// Change in account value that is not explained by money moving in or out.
    pub fn change(&self) -> Decimal {
        self.now_value - self.start_value - self.net_deposits
    }

    /// Everything the trading did not account for: the market moving under us.
    pub fn market(&self) -> Decimal {
        self.change() - self.trading
    }

    /// Trading return annualised against the account. Wildly noisy early on.
    pub fn trading_apr(&self) -> Decimal {
        if self.hours <= Decimal::ZERO || self.now_value.is_zero() {
            return Decimal::ZERO;
        }
        self.trading / self.hours * dec!(8760) / self.now_value * dec!(100)
    }
}

pub struct Impact {
    pub first_mid: Decimal,
    pub last_mid: Decimal,
    pub high: Decimal,
    pub low: Decimal,
}

impl Impact {
    pub fn drift_pct(&self) -> Decimal {
        if self.first_mid.is_zero() {
            return Decimal::ZERO;
        }
        (self.last_mid - self.first_mid) / self.first_mid * dec!(100)
    }

    pub fn range_pct(&self) -> Decimal {
        if self.low.is_zero() {
            return Decimal::ZERO;
        }
        (self.high - self.low) / self.low * dec!(100)
    }
}

#[cfg(test)]
pub async fn build(db: &Db, market: &str, window_secs: i64) -> Result<Report> {
    build_full(db, market, market, &[], window_secs).await
}

/// `program_key` is the base asset the program is filed under; `extra_ccys`
/// are any other currencies worth showing (a second quote coin, say).
pub async fn build_full(
    db: &Db,
    market: &str,
    program_key: &str,
    extra_ccys: &[String],
    window_secs: i64,
) -> Result<Report> {
    // The window never reaches back past the EPOCH.
    //
    // Asking for 24h right after drawing a line returned the old settings'
    // results under a "last 24h" heading, directly beneath a fresh scoreboard
    // reading zero -- one screen, two answers. `window_secs` is narrowed to
    // match so every per-hour and per-window figure below divides by the time
    // actually covered.
    let epoch = db.analytics_epoch_ms().await;
    let since = (crate::db::now_ms() - window_secs * 1000).max(epoch);
    let window_secs = ((crate::db::now_ms() - since) / 1000).max(1);

    let mark: Option<Decimal> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT mid FROM book_snapshots WHERE market = ? ORDER BY ts DESC LIMIT 1",
    )
    .bind(market)
    .fetch_optional(db.pool())
    .await?
    .flatten()
    .and_then(|s| s.parse().ok());

    // ---- pool weights ----
    let base_ccy = db.get_setting("base_unit").await?.unwrap_or_else(|| "base".into());
    let quote_ccy = db.get_setting("quote_unit").await?.unwrap_or_else(|| "quote".into());
    // Latest row PER CURRENCY, not every row at the latest timestamp.
    //
    let active_program = db.active_program(program_key).await?;
    let weights = match mark {
        Some(price) => {
            let mut pools = vec![Pool {
                ccy: base_ccy.clone(),
                qty: held(db, &base_ccy).await,
                price,
            }];
            for c in std::iter::once(quote_ccy.clone()).chain(extra_ccys.iter().cloned()) {
                if pools.iter().any(|p| p.ccy == c) {
                    continue;
                }
                // Quote coins are stablecoins here, so one unit is one unit of
                // the reporting currency. Wrong by the stable's own drift,
                // which is under a percent and not worth a second price feed.
                let qty = held(db, &c).await;
                pools.push(Pool { ccy: c, qty, price: Decimal::ONE });
            }
            Some(Weights {
                pools,
                base_ccy: base_ccy.clone(),
                quote_ccy: quote_ccy.clone(),
                target_weight: active_program.as_ref().and_then(|p| p.ratio),
            })
        }
        None => None,
    };

    // ---- program ----
    let program = match db.active_program(program_key).await? {
        Some(p) => {
            let current: Option<String> = sqlx::query_scalar(
                "SELECT inventory FROM book_snapshots WHERE market = ? ORDER BY ts DESC LIMIT 1",
            )
            .bind(market)
            .fetch_optional(db.pool())
            .await?
            .flatten();
            let current = match num::opt_parse(current.as_deref()) {
                Some(c) => c,
                None => db.position(market).await?.qty,
            };
            // Read the target the engine actually used last loop rather than the
            // stored one: a balanced program's target moves with price, so the
            // stored number would be wrong the moment the market ticks.
            let live_target: Option<String> = sqlx::query_scalar(
                "SELECT target_inv FROM book_snapshots WHERE market = ? ORDER BY ts DESC LIMIT 1",
            )
            .bind(market)
            .fetch_optional(db.pool())
            .await?
            .flatten();
            let live_target = num::opt_parse(live_target.as_deref()).unwrap_or(p.target_qty);

            // Measure progress against where the program ENDS UP, not the point
            // the glide happens to be passing through. Against the waypoint,
            // being on pace always reads 100% complete, which is useless next
            // to a schedule bar showing 5%.
            let final_target = match (p.kind, p.ratio, &weights) {
                (ProgramKind::Balanced | ProgramKind::Learned, Some(ratio), Some(w)) if !w.price().is_zero() => {
                    ratio * w.total() / w.price()
                }
                // A twap's destination is the number it was given, not the
                // waypoint its ramp is passing through. Against the waypoint a
                // twap five minutes into six hours reads "100% filled", which is
                // the least useful thing a status could say.
                (ProgramKind::Twap, _, _) => p.target_qty,
                _ => live_target,
            };
            let span = final_target - p.start_qty;
            let progress = if span.is_zero() {
                Decimal::ONE
            } else {
                num::clamp((current - p.start_qty) / span, Decimal::ZERO, Decimal::ONE)
            };
            Some(ProgramSummary {
                schedule_progress: p.ends_at.map(|_| p.schedule_progress(crate::db::now_ms())),
                id: p.id,
                kind: p.kind.as_str().to_string(),
                state: p.state.clone(),
                start_qty: p.start_qty,
                target_qty: final_target,
                waypoint: live_target,
                ends_at: p.ends_at,
                current,
                progress,
            })
        }
        None => None,
    };

    // ---- activity ----
    let rows = sqlx::query(
        "SELECT side, price, amount, total, fee FROM fills WHERE market = ? AND created_at >= ?",
    )
    .bind(market)
    .bind(since)
    .fetch_all(db.pool())
    .await?;

    let mut act = Activity::default();
    let (mut buy_qty_px, mut sell_qty_px) = (Decimal::ZERO, Decimal::ZERO);
    for r in &rows {
        let side: String = r.get(0);
        let price = num::parse(&r.get::<String, _>(1));
        let amount = num::parse(&r.get::<String, _>(2));
        let total = num::parse(&r.get::<String, _>(3));
        act.fills += 1;
        act.fees += num::parse(&r.get::<String, _>(4));
        if side == "buy" {
            act.bought += amount;
            act.buy_notional += total;
            buy_qty_px += price * amount;
        } else {
            act.sold += amount;
            act.sell_notional += total;
            sell_qty_px += price * amount;
        }
    }

    // ---- execution vs. the market's time-weighted mid ----
    // Every book snapshot is one equally-spaced sample of the price we could
    // have traded at, so the plain mean of `mid` over the window is the TWAP.
    let twap_row = sqlx::query(
        // Each aggregate is cast again on the way out: SQLite types a result
        // by its value, so MIN over prices that happen to be whole returns an
        // INTEGER even though the inputs were cast to REAL.
        "SELECT CAST(AVG(CAST(mid AS REAL)) AS REAL), COUNT(*),
                CAST(MIN(CAST(mid AS REAL)) AS REAL),
                CAST(MAX(CAST(mid AS REAL)) AS REAL)
         FROM book_snapshots WHERE market = ? AND ts >= ? AND mid IS NOT NULL",
    )
    .bind(market)
    .bind(since)
    .fetch_one(db.pool())
    .await?;
    let twap: Option<f64> = twap_row.get(0);
    let samples: i64 = twap_row.get(1);

    // Report every side we actually traded, not just the larger one.
    //
    // A two-sided market maker that bought 242 and sold 240 is not "a buyer",
    // and collapsing it to one side made a profitable round trip read as a
    // losing one. For a one-directional TWAP this still shows exactly the one
    // line that matters.
    let execution: Vec<ExecutionQuality> = match twap.and_then(|t| Decimal::try_from(t).ok()) {
        Some(market_twap) if !market_twap.is_zero() => [
            (Side::Buy, act.bought, buy_qty_px),
            (Side::Sell, act.sold, sell_qty_px),
        ]
        .into_iter()
        .filter(|(_, vol, _)| !vol.is_zero())
        .map(|(side, vol, qty_px)| {
            let our_vwap = qty_px / vol;
            // Buying below the TWAP is good; selling above it is good.
            let edge = match side {
                Side::Buy => (market_twap - our_vwap) / market_twap,
                Side::Sell => (our_vwap - market_twap) / market_twap,
            };
            ExecutionQuality {
                side,
                our_vwap,
                market_twap,
                edge_pct: edge * dec!(100),
                samples,
                volume: vol,
            }
        })
        .collect(),
        _ => vec![],
    };

    // ---- market-making edge, per book ----
    //
    // Each fill is compared with the mid price recorded nearest to it in time.
    // Snapshots land every loop, so the match is within seconds; fills with no
    // snapshot nearby are skipped rather than scored against a stale price.
    // Snapshots first, then fills matched to the nearest one in time.
    //
    // Done in Rust rather than a correlated subquery: SQLite will not resolve
    // an outer column inside a subquery's ORDER BY, and a join on time buckets
    // is harder to read than the loop below.
    let snaps = sqlx::query(
        "SELECT market, ts, mid FROM book_snapshots
         WHERE ts >= ? AND mid IS NOT NULL ORDER BY market, ts",
    )
    .bind(since - 120_000)
    .fetch_all(db.pool())
    .await?;

    let mut mids: std::collections::HashMap<String, Vec<(i64, Decimal)>> = Default::default();
    for row in &snaps {
        mids.entry(row.get::<String, _>(0))
            .or_default()
            .push((row.get::<i64, _>(1), num::parse(&row.get::<String, _>(2))));
    }

    /// Mid recorded nearest in time, if one is close enough to be meaningful.
    fn mid_near(series: Option<&Vec<(i64, Decimal)>>, ts: i64) -> Option<Decimal> {
        let series = series?;
        let i = series.partition_point(|(t, _)| *t < ts);
        [i.checked_sub(1), Some(i)]
            .into_iter()
            .flatten()
            .filter_map(|j| series.get(j))
            .min_by_key(|(t, _)| (t - ts).abs())
            .filter(|(t, _)| (t - ts).abs() < 120_000)
            .map(|(_, m)| *m)
    }

    let edge_rows = sqlx::query(
        "SELECT market, side, price, amount, total, fee, fee_currency, liquidity, created_at,
                edge, markout
         FROM fills WHERE created_at >= ?",
    )
    .bind(since)
    .fetch_all(db.pool())
    .await?;

    let mut by_market: std::collections::BTreeMap<String, MakerEdge> = Default::default();
    for row in &edge_rows {
        let market_id: String = row.get(0);
        let side = Side::parse(&row.get::<String, _>(1));
        let price = num::parse(&row.get::<String, _>(2));
        let amount = num::parse(&row.get::<String, _>(3));
        let total = num::parse(&row.get::<String, _>(4));
        let fee = num::parse(&row.get::<String, _>(5));
        let fee_ccy: Option<String> = row.get(6);
        let liquidity: Option<String> = row.get(7);
        let created_at: i64 = row.get(8);
        // Prefer the edge settled at ingestion; fall back to a snapshot match
        // only for rows that predate it.
        let stored: Option<String> = row.get(9);
        let edge_of_fill = num::opt_parse(stored.as_deref());
        let mid = mid_near(mids.get(&market_id), created_at);

        let e = by_market.entry(market_id.clone()).or_insert_with(|| MakerEdge {
            market: market_id,
            market_volume: Decimal::ZERO,
            market_spread: Decimal::ZERO,
            fills: 0,
            volume_base: Decimal::ZERO,
            volume_quote: Decimal::ZERO,
            edge_quote: Decimal::ZERO,
            markout_quote: Decimal::ZERO,
            scored: 0,
            fees: Decimal::ZERO,
            maker_fills: 0,
        });
        e.fills += 1;
        if let Some(mo) = num::opt_parse(row.get::<Option<String>, _>(10).as_deref()) {
            e.markout_quote += mo;
            e.scored += 1;
        }
        e.volume_base += amount;
        e.volume_quote += total;
        if liquidity.as_deref() == Some("maker") {
            e.maker_fills += 1;
        }
        // Fees are charged in whichever currency was received. Value them in
        // quote terms so the comparison with edge is like for like.
        e.fees += match fee_ccy.as_deref() {
            Some(c) if c.eq_ignore_ascii_case("usdt") || c.eq_ignore_ascii_case("usdc") => fee,
            _ => fee * price,
        };
        match (edge_of_fill, mid, side) {
            (Some(v), _, _) => e.edge_quote += v,
            (None, Some(mid), Some(side)) if !mid.is_zero() => {
                e.edge_quote += match side {
                    Side::Sell => (price - mid) * amount,
                    Side::Buy => (mid - price) * amount,
                }
            }
            _ => {}
        }
    }
    let mut maker: Vec<MakerEdge> = by_market.into_values().collect();
    // Include books we have not filled on, so a quiet market still reports its
    // volume -- that is exactly when you want to know whether it is worth
    // quoting at all.
    for (key, _) in db.all_settings().await?.iter().filter(|(k, _)| k.starts_with("vol:")) {
        let id = key.trim_start_matches("vol:").to_string();
        if !maker.iter().any(|m| m.market == id) {
            maker.push(MakerEdge {
                market: id,
                market_volume: Decimal::ZERO,
                market_spread: Decimal::ZERO,
                fills: 0,
                volume_base: Decimal::ZERO,
                volume_quote: Decimal::ZERO,
                edge_quote: Decimal::ZERO,
                markout_quote: Decimal::ZERO,
                scored: 0,
                fees: Decimal::ZERO,
                maker_fills: 0,
            });
        }
    }
    for m in &mut maker {
        let (v, sp) = db.market_stats(&m.market).await;
        m.market_volume = v;
        m.market_spread = sp;
    }
    maker.sort_by(|a, b| b.market_volume.partial_cmp(&a.market_volume).unwrap_or(std::cmp::Ordering::Equal));

    // ---- the split since the epoch: market versus trading ----
    //
    // Trading is measured since the EPOCH, not over the report window, because
    // the account's change since the scoreboard started has to be explained by
    // everything that happened since then. And it is the epoch, not launch,
    // because a change that fixes the quoting is invisible for a day if it has
    // to drag the losses of the old settings behind it, and a day is far too
    // slow to iterate on. The starting value and the deposits are taken from
    // the same instant (`Db::reset_analytics_epoch`), so the three add up.
    let all_rows = sqlx::query(
        "SELECT price, fee, fee_currency, markout FROM fills WHERE created_at >= ?",
    )
    .bind(epoch)
    .fetch_all(db.pool())
    .await?;
    let now_value = weights.as_ref().map(|w| w.total()).unwrap_or_default();
    let mut life = Lifetime {
        net_deposits: db.net_deposits().await,
        // Between a line being drawn and the engine's next tick there is no
        // starting value yet; the engine will seed the live value, so that is
        // what a report in that gap (or on an offline copy) shows. A zero
        // here read the whole account as market gain.
        start_value: db.account_start_value().await.unwrap_or(now_value),
        now_value,
        ..Default::default()
    };
    for row in &all_rows {
        let price = num::parse(&row.get::<String, _>(0));
        let fee = num::parse(&row.get::<String, _>(1));
        let fee_ccy: Option<String> = row.get(2);
        let stored: Option<String> = row.get(3);
        life.fees += match fee_ccy.as_deref() {
            Some(c) if c.eq_ignore_ascii_case("usdt") || c.eq_ignore_ascii_case("usdc") => fee,
            _ => fee * price,
        };
        // Markout, not edge.
        //
        // `edge` compares our fill to the mid at that instant, which we beat by
        // construction -- it reported a quoting profit across a period the
        // quoting actually lost money. Only a fill judged five minutes
        // later counts here. A fill too recent to judge counts as nothing
        // rather than as zero profit, so `scored_fees` tracks the fees of the
        // fills we are actually scoring; charging all fees against a subset of
        // the markouts would invent a loss.
        if let Some(v) = num::opt_parse(stored.as_deref()) {
            life.trading += v;
            life.scored_fees += match fee_ccy.as_deref() {
                Some(c) if c.eq_ignore_ascii_case("usdt") || c.eq_ignore_ascii_case("usdc") => fee,
                _ => fee * price,
            };
        }
    }
    life.trading -= life.scored_fees;
    if let Ok(Some(started)) = db.get_setting("account_start_ts").await
        && let Ok(ts) = started.parse::<i64>()
    {
        life.hours = Decimal::from((crate::db::now_ms() - ts).max(1)) / dec!(3600000);
    }

    // ---- earnings over time ----
    //
    // Grouped by UTC day from the stored per-fill edge. Fees are valued in
    // quote terms, as everywhere else: they are charged in whichever currency
    // was received.
    // Hourly while the epoch is young. Waiting for a calendar day to close
    // before a change shows anything is not a feedback loop.
    let young = crate::db::now_ms() - epoch < 48 * 3_600_000;
    // The format string is bound, not interpolated -- `sqlx::query` rightly
    // refuses a SQL string built at runtime.
    let bucket = if young { "%Y-%m-%d %H:00" } else { "%Y-%m-%d" };
    let day_rows = sqlx::query(
        "SELECT strftime(?1, created_at/1000, 'unixepoch') AS d,
                CAST(SUM(CAST(markout AS REAL)) AS REAL),
                CAST(SUM(CAST(fee AS REAL) *
                     CASE WHEN fee_currency IN ('usdt','usdc') THEN 1
                          ELSE CAST(price AS REAL) END) AS REAL)
         FROM fills WHERE markout IS NOT NULL AND created_at >= ?2
         GROUP BY d ORDER BY d DESC",
    )
    .bind(bucket)
    .bind(epoch)
    .fetch_all(db.pool())
    .await?;

    let mut earnings = Earnings { hours: life.hours, total: life.trading, ..Default::default() };
    let mut by_day: Vec<(String, Decimal)> = Vec::new();
    for r in &day_rows {
        let day: String = r.get(0);
        let edge = Decimal::try_from(r.get::<Option<f64>, _>(1).unwrap_or(0.0)).unwrap_or_default();
        let fees = Decimal::try_from(r.get::<Option<f64>, _>(2).unwrap_or(0.0)).unwrap_or_default();
        by_day.push((day, edge - fees));
    }
    earnings.daily = by_day.iter().take(7).cloned().collect();

    // Weekly averages, over the days that actually traded in each week.
    let today = chrono::Utc::now().date_naive();
    let mut weeks: std::collections::BTreeMap<i64, (Decimal, i64)> = Default::default();
    for (day, net) in &by_day {
        let Ok(d) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else { continue };
        // Clamped at zero: the exchange stamps some trades days ahead, which
        // would otherwise produce a "-1 weeks ago" bucket and a future date at
        // the head of the daily list.
        let age_weeks = ((today - d).num_days() / 7).max(0);
        if age_weeks < 4 {
            let e = weeks.entry(age_weeks).or_insert((Decimal::ZERO, 0));
            e.0 += *net;
            e.1 += 1;
        }
    }
    earnings.weekly = weeks
        .into_iter()
        .map(|(age, (total, days))| {
            let label = match age {
                0 => "this week".to_string(),
                1 => "last week".to_string(),
                n => format!("{n} weeks ago"),
            };
            (label, total / Decimal::from(days.max(1)), days)
        })
        .collect();

    // ---- price impact over the window ----
    let impact = {
        let first: Option<String> = sqlx::query_scalar(
            "SELECT mid FROM book_snapshots WHERE market = ? AND ts >= ? AND mid IS NOT NULL
             ORDER BY ts ASC LIMIT 1",
        )
        .bind(market).bind(since).fetch_optional(db.pool()).await?.flatten();
        let last: Option<String> = sqlx::query_scalar(
            "SELECT mid FROM book_snapshots WHERE market = ? AND ts >= ? AND mid IS NOT NULL
             ORDER BY ts DESC LIMIT 1",
        )
        .bind(market).bind(since).fetch_optional(db.pool()).await?.flatten();
        match (num::opt_parse(first.as_deref()), num::opt_parse(last.as_deref())) {
            (Some(f), Some(l)) => Some(Impact {
                first_mid: f,
                last_mid: l,
                low: twap_row.get::<Option<f64>, _>(2).and_then(|v| Decimal::try_from(v).ok()).unwrap_or(f),
                high: twap_row.get::<Option<f64>, _>(3).and_then(|v| Decimal::try_from(v).ok()).unwrap_or(f),
            }),
            _ => None,
        }
    };

    // ---- open orders / overrides / tails ----
    let open_orders = sqlx::query(
        "SELECT side, price, amount, filled_amount, placed_at FROM orders
         WHERE market = ? AND state IN ('wait','pending') AND exchange_id IS NOT NULL
         ORDER BY side, CAST(price AS REAL) DESC",
    )
    .bind(market)
    .fetch_all(db.pool())
    .await?
    .iter()
    .map(|r| {
        (
            r.get::<String, _>(0),
            num::opt_parse(r.get::<Option<String>, _>(1).as_deref()).unwrap_or_default(),
            // Subtract in Rust, on Decimals. Letting SQLite do `amount -
            // filled_amount` across TEXT columns makes it pick a result type
            // from the values: "250" - "0" comes back INTEGER, and reading that
            // as f64 panics. Amounts are normalised on write, so whole numbers
            // are the common case, not the rare one.
            (num::parse(&r.get::<String, _>(2)) - num::parse(&r.get::<String, _>(3)))
                .max(Decimal::ZERO),
            r.get::<i64, _>(4),
        )
    })
    .collect();

    let recent_fills = sqlx::query(
        "SELECT created_at, side, price, amount FROM fills WHERE market = ?
         ORDER BY created_at DESC LIMIT 15",
    )
    .bind(market)
    .fetch_all(db.pool())
    .await?
    .iter()
    .map(|r| {
        (
            r.get::<i64, _>(0),
            r.get::<String, _>(1),
            num::parse(&r.get::<String, _>(2)),
            num::parse(&r.get::<String, _>(3)),
        )
    })
    .collect();

    let errors = sqlx::query(
        "SELECT ts, kind, message FROM events WHERE level IN ('error','warn') AND ts >= ?
         ORDER BY ts DESC LIMIT 10",
    )
    .bind(since)
    .fetch_all(db.pool())
    .await?
    .iter()
    .map(|r| (r.get::<i64, _>(0), r.get::<String, _>(1), r.get::<String, _>(2)))
    .collect();

    let arms: Vec<crate::strategy::arms::ArmSize> = db
        .get_setting("arms")
        .await?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();

    // The cycle is measured over the WHOLE arm window, not the report window.
    //
    // A cycle's legs are per unit traded, and the thin book trades ~80 fills a
    // day: a one-hour report window would show a leg as unmeasured and print
    // "shut" on a cycle the engine is running. The engine reads the same
    // window (`ARMS_MAX_AGE_MS`), so the report and the decision agree.
    let cycle_since = crate::db::now_ms() - crate::engine::ARMS_MAX_AGE_MS;
    let cycle = match db.cycle_legs(cycle_since).await {
        Ok(legs) if !legs.is_empty() => {
            let all = db.all_settings().await?;
            let pick = |prefix: &str| -> std::collections::HashMap<String, String> {
                all.iter()
                    .filter_map(|(k, v)| Some((k.strip_prefix(prefix)?.to_string(), v.clone())))
                    .collect()
            };
            let (quote_of, base_of) = (pick("quote_unit:"), pick("base_unit:"));
            let acquire = db.get_setting("cycle_acquire").await?.unwrap_or_default();
            let recycle = db
                .get_setting("cycle_recycle_bps")
                .await?
                .map(|v| crate::num::parse(&v))
                .unwrap_or(Decimal::ZERO);
            Some(crate::strategy::cycle::estimate(&acquire, legs, quote_of, base_of, recycle))
        }
        _ => None,
    };
    let cycle_floor_bps = db
        .get_setting("cycle_floor_bps")
        .await?
        .map(|v| crate::num::parse(&v))
        .unwrap_or(crate::strategy::cycle::DEFAULT_FLOOR_BPS);

    // The evidence behind the verdict: episodes that produced a scored fill
    // since the epoch, and what those fills traded. Fills inside one horizon
    // share a price path and are one bet, so the count is of episodes.
    let (episodes, scored_fills, scored_notional): (i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT COUNT(DISTINCT created_at / ?), COUNT(*), CAST(SUM(CAST(total AS REAL)) AS REAL)
         FROM fills WHERE markout IS NOT NULL AND created_at >= ?",
    )
    .bind(crate::db::MARKOUT_HORIZON_MS)
    .bind(epoch)
    .fetch_one(db.pool())
    .await?;

    // ---- what is genuinely spare ----
    //
    // The number the owner actually wants: how much can leave the account
    // without stopping the bot working. Two different questions per asset:
    //
    //   base   anything above the position the program is steering toward.
    //          Below that the ask ladder has nothing to sell.
    //   quote  anything above what the bid ladder needs to keep buying. A
    //          market maker with no cash is a seller, and one-sided quoting
    //          is how inventory ends up all in one asset at the worst price.
    //
    // Deliberately conservative: it reserves several refills of the bid side,
    // because a number that is right on average and wrong in a drawdown is
    // worse than one that is a little small.
    let mut spare: Vec<(String, Decimal)> = Vec::new();
    if let Some(w) = &weights {
        let target = active_program
            .as_ref()
            .map(|p| p.target_qty)
            .unwrap_or_else(|| w.base_qty());
        let base_spare = (w.base_qty() - target).max(Decimal::ZERO);
        if base_spare > Decimal::ZERO {
            spare.push((base_ccy.to_uppercase(), base_spare));
        }

        // Cash is harder, because the bid ladder refills as orders fill and
        // there is no single "needed" number. Reserved as a share of equity:
        // enough to keep buying through a run of one-sided flow, and small
        // enough that a mostly-base account is not told to withdraw nothing.
        let reserve = w.total() * QUOTING_CASH_RESERVE;
        let cash: Decimal = w.pools.iter().filter(|p| p.ccy != base_ccy).map(|p| p.value()).sum();
        let cash_spare = (cash - reserve).max(Decimal::ZERO);
        if cash_spare > Decimal::ZERO {
            // Split across the quote currencies in the proportions held, so
            // taking the money out does not lopside the mix.
            for pool in w.pools.iter().filter(|p| p.ccy != base_ccy) {
                if pool.value() <= Decimal::ZERO || cash.is_zero() {
                    continue;
                }
                let part = cash_spare * pool.value() / cash;
                if part > Decimal::ZERO {
                    spare.push((pool.ccy.to_uppercase(), (part / pool.price).round_dp(2)));
                }
            }
        }
    }

    let pile = db.set_aside(&base_ccy).await;
    let earned = db.set_aside_earned(&base_ccy).await;
    let set_aside = (pile > Decimal::ZERO || earned > Decimal::ZERO)
        .then(|| (base_ccy.to_uppercase(), pile, earned));

    // ---- yield: the pool as one asset ----
    //
    // From the moment banking started, which is when the book was funded.
    let since = db
        .get_setting(&format!("bank_since_ms:{base_ccy}"))
        .await?
        .and_then(|v| v.parse::<i64>().ok());
    let yield_ = match (since, &weights) {
        (Some(since), Some(w)) if w.price() > Decimal::ZERO => {
            let px = w.price();
            let earned_quote = db.earnings_since(since).await;
            let last_hour_quote =
                db.earnings_since((crate::db::now_ms() - 3_600_000).max(since)).await;
            Some(Yield {
                last_hour_quote,
                last_hour_base: last_hour_quote / px,
                base_ccy: base_ccy.clone(),
                since_ms: since,
                days: Decimal::from((crate::db::now_ms() - since).max(1)) / dec!(86400000),
                earned_quote,
                earned_base: earned_quote / px,
                capital_base: (w.total() / px - pile).max(Decimal::ZERO),
            })
        }
        _ => None,
    };

    Ok(Report {
        market: market.to_string(),
        window_secs,
        epoch_ms: epoch,
        epoch_reason: db.epoch_reason().await,
        spare,
        set_aside,
        yield_,
        mark,
        program,
        weights,
        maker,
        arms,
        cycle,
        cycle_floor_bps,
        episodes: episodes.max(0) as usize,
        scored_fills,
        scored_notional: Decimal::try_from(scored_notional.unwrap_or(0.0)).unwrap_or_default(),
        lifetime: life,
        earnings,
        api_rate: db.get_setting("api_rate").await.ok().flatten(),
        activity: act,
        execution,
        impact,
        open_orders,
        overrides: db.all_settings().await?,
        recent_fills,
        errors,
    })
}

// ---------- rendering ----------

/// The verdict on whether the quoting earns, and how sure we are.
///
/// The whole point of the status message. Everything else on the screen is
/// evidence for this line, and without it the reader is left doing statistics
/// in their head -- which is how 23 episodes of +6 bp gets mistaken for a win.
pub enum Verdict {
    /// Not enough evidence yet, and how many episodes short we are.
    Unproven { episodes: usize, needed: usize, bps: Decimal },
    Earning { bps: Decimal, episodes: usize },
    Losing { bps: Decimal, episodes: usize },
}

/// Episodes needed before a mean of this size can be told from zero.
///
/// Two standard errors, at the ~113 bp per-episode standard deviation measured
/// on prlusdt. A 20 bp edge needs about 128 episodes; a 10 bp edge about 510.
/// Capped, because past a point "collect more data" stops being useful advice.
fn episodes_needed(bps: Decimal) -> usize {
    const SD_BPS: f64 = 113.0;
    // No verdict from a handful of episodes, however large the number looks.
    //
    // Scaling purely with the measured size let an early outlier clear the
    // bar instantly: +276 bp over TWO episodes demanded zero and printed
    // EARNING, which is precisely the coin-flip this exists to refuse. The
    // arithmetic is not wrong -- two episodes at that size really are several
    // standard errors from zero -- but the standard deviation those errors
    // are computed from is itself estimated from the same two points, and a
    // pair of lucky fills is not evidence about a strategy.
    //
    // Thirty is the floor: the Kelly sizer will not act under fifty, and a
    // scoreboard that declares victory well before the sizer believes it is a
    // scoreboard that misleads its reader.
    const FLOOR: usize = 30;
    let m = f64::try_from(bps).unwrap_or(0.0).abs().max(1.0);
    ((2.0 * SD_BPS / m).powi(2) as usize).clamp(FLOOR, 2000)
}

impl Verdict {
    /// `bps` is net markout per unit traded; `episodes` how many five-minute
    /// windows produced a fill.
    pub fn of(bps: Decimal, episodes: usize) -> Verdict {
        let needed = episodes_needed(bps);
        if episodes < needed {
            Verdict::Unproven { episodes, needed, bps }
        } else if bps > Decimal::ZERO {
            Verdict::Earning { bps, episodes }
        } else {
            Verdict::Losing { bps, episodes }
        }
    }

    pub fn headline(&self) -> String {
        match self {
            Verdict::Unproven { episodes, needed, bps } => format!(
                "🤔 TOO EARLY — {:+.0} bp so far\n{} of ~{} episodes",
                bps, episodes, needed
            ),
            Verdict::Earning { bps, episodes } => {
                format!("📈 EARNING — {:+.0} bp after fees\nover {} episodes", bps, episodes)
            }
            Verdict::Losing { bps, episodes } => {
                format!("📉 LOSING — {:+.0} bp after fees\nover {} episodes", bps, episodes)
            }
        }
    }
}

/// "since 09-24 18:02 · settings changed": the scoreboard's window and what
/// started it, at most 36 characters, for the head of both renderings.
fn scoreboard_since(r: &Report) -> String {
    if r.epoch_ms <= 0 {
        return "since launch".into();
    }
    // 20 characters of stamp leave 16 for the reason, which is exactly the
    // longest one written ("settings changed").
    let reason: String = r.epoch_reason.as_deref().unwrap_or("line drawn").chars().take(16).collect();
    format!("since {} · {}", short_ts(r.epoch_ms), reason)
}

/// Share of account value kept as cash to fund the bid ladder.
///
/// A market maker with no cash is just a seller, and one-sided quoting is how
/// inventory ends up entirely in one asset at the worst possible price. There
/// is no exact figure -- the ladder refills as it fills -- so this is a
/// deliberately conservative share of equity rather than a computed need.
const QUOTING_CASH_RESERVE: Decimal = dec!(0.15);

/// Everything we hold of one currency, free and locked.
///
/// The latest row PER CURRENCY, not every row at the latest timestamp: balances
/// are only written when they change, so the most recent timestamp usually
/// holds just the one currency that moved. Reading "everything at max(ts)"
/// reported the other side of the book as zero -- which looked exactly like the
/// account had been emptied.
pub async fn held(db: &Db, ccy: &str) -> Decimal {
    let v: Option<(String, String)> = sqlx::query_as(
        "SELECT available, locked FROM balances WHERE currency = ? ORDER BY ts DESC LIMIT 1",
    )
    .bind(ccy)
    .fetch_optional(db.pool())
    .await
    .ok()
    .flatten();
    v.map(|(a, l)| num::parse(&a) + num::parse(&l)).unwrap_or_default()
}

/// Every currency held, free plus locked, from the latest row per currency.
/// Rounded to 4 places; dust that rounds to zero is left out. Sorted by currency name.
pub async fn holdings(db: &Db) -> Vec<(String, Decimal)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT b.currency, b.available, b.locked FROM balances b
         JOIN (SELECT currency, MAX(ts) AS ts FROM balances GROUP BY currency) m
           ON b.currency = m.currency AND b.ts = m.ts
         ORDER BY b.currency",
    )
    .fetch_all(db.pool())
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|(c, a, l)| (c, (num::parse(&a) + num::parse(&l)).round_dp(4).normalize()))
        .filter(|(_, v)| !v.is_zero())
        .collect()
}

/// How long ago, in one short word. Shared so that a resting order's age reads
/// the same in the terminal report and in a chat message.
pub fn age(ms: i64) -> String {
    dur(((crate::db::now_ms() - ms) / 1000).max(0))
}

/// A timestamp for a narrow report: 11 characters, not 19. The year and the
/// seconds cost eight columns and answer nothing anyone asks of a log line.
fn short_ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

fn dur(secs: i64) -> String {
    match secs {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// Drop HTML tags, leaving what a reader actually sees. Used to measure line
/// width honestly: markup costs characters in the string and none on screen.
#[cfg(test)]
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The arm table as (market, bucket, regime) rows carrying both sides, in
/// the order the ladder is read: touch first, sweep catcher last, calm
/// before the storms.
///
/// One row per bucket and regime rather than per arm, because on a phone
/// ninety rows is a scroll and the two sides of a cell are shrunk toward one
/// number anyway. The pooled yield shown is the exposure-weighted mean of
/// the two sides' posteriors; the sizes are each side's own. A cell nothing
/// has rested on inside the window is not shown: with the storms keyed, most
/// of the table is empty most of the time, and an empty row says nothing.
struct ArmRow<'a> {
    market: &'a str,
    bucket: crate::strategy::arms::Bucket,
    regime: crate::strategy::arms::Regime,
    width: crate::strategy::arms::Width,
    bid: Option<&'a crate::strategy::arms::ArmSize>,
    ask: Option<&'a crate::strategy::arms::ArmSize>,
}

impl ArmRow<'_> {
    fn both(&self) -> impl Iterator<Item = &crate::strategy::arms::ArmSize> {
        self.bid.into_iter().chain(self.ask)
    }
    fn fills(&self) -> f64 {
        self.both().map(|z| z.fills).sum()
    }
    fn exposure(&self) -> f64 {
        self.both().map(|z| z.exposure_dh).sum()
    }
    /// Exposure-weighted posterior yield and its error across the two sides.
    fn yield_se(&self) -> (f64, f64) {
        let e = self.exposure();
        if e <= 0.0 {
            let z = self.both().next();
            return z.map(|z| (z.yield_pm, z.se_pm)).unwrap_or((0.0, 0.0));
        }
        let y = self.both().map(|z| z.yield_pm * z.exposure_dh).sum::<f64>() / e;
        let se = self.both().map(|z| (z.se_pm * z.exposure_dh).powi(2)).sum::<f64>().sqrt() / e;
        (y, se)
    }
}

fn arm_rows(arms: &[crate::strategy::arms::ArmSize]) -> Vec<ArmRow<'_>> {
    use crate::strategy::arms::{Bucket, Regime, Width};
    let mut markets: Vec<&str> = arms.iter().map(|z| z.arm.market.as_str()).collect();
    markets.sort_unstable();
    markets.dedup();
    let mut rows = Vec::new();
    for m in markets {
        for b in Bucket::ALL {
            for r in Regime::ALL {
                for w in Width::ALL {
                    let find = |side| {
                        arms.iter().find(|z| {
                            z.arm.market == m
                                && z.arm.bucket == b
                                && z.arm.regime == r
                                && z.arm.width == w
                                && z.arm.side == side
                        })
                    };
                    let row = ArmRow { market: m, bucket: b, regime: r, width: w, bid: find(Side::Buy), ask: find(Side::Sell) };
                    if row.exposure() > 0.0 {
                        rows.push(row);
                    }
                }
            }
        }
    }
    rows
}

/// A share as a short percentage, or the state where the number would
/// mislead: "100%" on an execution program is not an earned size.
fn share_cell(z: Option<&crate::strategy::arms::ArmSize>) -> String {
    use crate::strategy::arms::State;
    match z {
        None => "—".into(),
        Some(z) if z.state == State::Execution => "prog".into(),
        Some(z) => format!("{:.0}%", z.share * dec!(100)),
    }
}

/// One row of the arm table, 36 columns, under its market's own line:
///     200 calm  3+ +18.2  9.4  46   0% 100%
fn arm_line(row: &ArmRow<'_>) -> String {
    let (y, se) = row.yield_se();
    format!(
        "{:>5}{:>5}{:>4}{:>6}{:>4}{:>4}{:>4}{:>5}\n",
        row.bucket.label(),
        row.regime.label(),
        row.width.label(),
        signed_cell(y),
        small_cell(se),
        short_num(row.fills()),
        share_cell(row.bid),
        share_cell(row.ask),
    )
}

fn arm_header() -> String {
    format!(
        "{:>5}{:>5}{:>4}{:>6}{:>4}{:>4}{:>4}{:>5}\n",
        "bps", "vol", "wid", "yield", "se", "n", "bid", "ask"
    )
}

/// The rows of the arm table with each market named once, on a line of its
/// own, above its rows. Repeating the market on every row cost eight
/// columns a line, and with the book width keyed there was no longer room
/// for it at phone width.
fn arm_table(arms: &[crate::strategy::arms::ArmSize]) -> String {
    let mut o = arm_header();
    let mut market = "";
    for row in arm_rows(arms) {
        if row.market != market {
            market = row.market;
            o.push_str(market);
            o.push('\n');
        }
        o.push_str(&arm_line(&row));
    }
    o
}

/// The arm table, 40 columns wide, as the report and `safetrade arms` print
/// it.
///
/// What each leg of the cycle earns per unit traded, and what the whole cycle
/// pays end to end.
///
/// Deliberately separate from the ARMS table, which answers a different
/// question -- yield per dollar-hour of RESTING capital, the right number for
/// sizing one arm. This is per unit TRADED, which is the only figure that can
/// be added across legs. A leg can be a loser here and still be worth resting,
/// and the VERDICT line is the one that decides: `open` means the cycle clears
/// its floor after each leg's error is charged, `shut` means the evidence does
/// not support it yet, whatever the point estimate says.
pub fn render_cycle(cycle: &crate::strategy::cycle::Cycle, floor_bps: Decimal) -> String {
    use crate::exchange::Side;
    let mut o = String::new();
    o.push_str("CYCLE · bp per unit traded, after fees\n");

    let mut legs: Vec<_> = cycle.legs.values().collect();
    legs.sort_by(|a, b| (&a.market, a.side.as_str()).cmp(&(&b.market, b.side.as_str())));
    if legs.is_empty() {
        o.push_str("  no scored fills yet\n");
        return o;
    }
    o.push_str(&format!("{:<9}{:>5}{:>8}{:>7}{:>6}\n", "leg", "side", "bp", "±", "n"));
    for l in &legs {
        let mark = if l.market == cycle.acquire && l.side == Side::Buy { "*" } else { " " };
        o.push_str(&format!(
            "{}{:<8}{:>5}{:>+8.0}{:>7.0}{:>6}\n",
            mark,
            l.market,
            l.side.as_str(),
            l.net_bps,
            l.se_bps,
            l.n
        ));
    }
    o.push_str(&format!("\nacquire on {} (*)\n", cycle.acquire));
    o.push_str(&format!("swap back  {:>4.0} bp when needed\n", cycle.recycle_bps));
    o.push_str(&format!("floor      {:>4.0} bp to act\n\n", floor_bps));

    // One line per release venue: the point estimate, the figure with both
    // legs' errors charged, and whether that clears the floor.
    o.push_str(&format!("{:<9}{:>7}{:>9}{:>7}\n", "release", "bp", "on evid", ""));
    let mut venues: Vec<&String> = legs
        .iter()
        // Only somewhere the acquired asset can actually be sold. The
        // stablecoin book has sell fills and a quote currency, so without this
        // it read as a place to release PRL.
        .filter(|l| l.side == Side::Sell && cycle.releases_here(&l.market))
        .map(|l| &l.market)
        .collect();
    venues.sort();
    venues.dedup();
    let mut any = false;
    for m in venues {
        let (Some(pt), Some(lb)) = (cycle.margin_bps(m), cycle.margin_lower_bound(m)) else {
            o.push_str(&format!("{:<9}{:>7}{:>9}  {:<7}\n", m, "—", "—", "no data"));
            continue;
        };
        let open = lb > floor_bps;
        any |= open;
        let swap = if cycle.needs_recycle(m) { " +swap" } else { "" };
        o.push_str(&format!(
            "{:<9}{:>+7.0}{:>+9.0}  {:<5}{}\n",
            m,
            pt,
            lb,
            if open { "OPEN" } else { "shut" },
            swap
        ));
    }
    if !any {
        o.push_str("\nno release venue clears the floor on the evidence yet:\n");
        o.push_str("the cycle is measured, not assumed, and it stays shut\n");
    }
    o
}

/// Every place we rest capital, and what it has paid: net markout per
/// dollar-hour of resting orders, in per mille, with its error; the decayed
/// fills behind that; and the size each side is quoting as a share of its
/// rung. One row per bucket and regime, both sides on it.
pub fn render_arms(arms: &[crate::strategy::arms::ArmSize]) -> String {
    let mut o = String::new();
    o.push_str("ARMS · ‰ per $-hour after fees\n");
    o.push_str(&arm_table(arms));
    let mut states: Vec<&str> = Vec::new();
    for z in arms {
        if !states.contains(&z.state.label()) {
            states.push(z.state.label());
        }
    }
    o.push_str(&format!("sizes: {}\n", states.join(", ")));
    o
}

/// A signed yield in at most five characters: +18.2, -161, -1623.
fn signed_cell(v: f64) -> String {
    if v.abs() < 99.95 { format!("{v:+.1}") } else { format!("{v:+.0}") }
}

/// An error bar in at most four characters: 0.3, 9.4, 62, 1.4k.
fn small_cell(v: f64) -> String {
    if v < 9.95 { format!("{v:.1}") } else { short_num(v) }
}

/// A count in at most four characters: 950, 12k, 1.2M.
fn short_num(v: f64) -> String {
    if v < 999.5 {
        format!("{v:.0}")
    } else if v < 9_950.0 {
        format!("{:.1}k", v / 1e3)
    } else if v < 999_500.0 {
        format!("{:.0}k", v / 1e3)
    } else {
        format!("{:.1}M", v / 1e6)
    }
}

fn bar(frac: Decimal, width: usize) -> String {
    let f = num::clamp(frac, Decimal::ZERO, Decimal::ONE);
    let filled = (f * Decimal::from(width as i64)).round().to_string().parse::<usize>().unwrap_or(0);
    format!("{}{}", "█".repeat(filled.min(width)), "░".repeat(width - filled.min(width)))
}

/// The full report, 36 columns wide.
///
/// Narrow on purpose. A 36-wide report in an 80-column terminal loses nothing;
/// a 72-wide one on a phone loses everything, because every single line wraps
/// two or three times and the columns stop lining up. One renderer serves both,
/// so the Telegram `report` button and the CLI can never drift apart.
pub fn render(r: &Report) -> String {
    let mut o = String::new();
    let line = "─".repeat(36);

    o.push_str(&format!("\nSafeTrade · {} · last {}\n", r.market.to_uppercase(), dur(r.window_secs)));
    o.push_str(&format!("{line}\n"));

    // ---- pools ----
    // The point of a balanced program is that BOTH of these grow. Total value
    // rising while one pool shrinks is a directional bet that happened to work,
    // not spread capture.
    if let Some(w) = &r.weights {
        let bw = w.base_weight();
        let total = w.total();
        o.push_str("POOLS\n");
        for pool in &w.pools {
            let share = if total.is_zero() { Decimal::ZERO } else { pool.value() / total };
            o.push_str(&format!(
                "{:<5} {:>12} {:>9} {:>5.1}%\n",
                pool.ccy.to_uppercase(), pool.qty.round_dp(4),
                pool.value().round_dp(1), share * dec!(100)
            ));
        }
        o.push_str(&format!(
            "{:<5} {:>12} {:>9} {}\n",
            "total", "", total.round_dp(1), w.quote_ccy.to_uppercase()
        ));
        o.push_str(&format!("split  {} {:>5.1}%\n", bar(bw, 20), bw * dec!(100)));
        if let Some(t) = w.target_weight {
            o.push_str(&format!(
                "target {:.1}%  drift {:+.1} pts\n",
                t * dec!(100), (bw - t) * dec!(100)
            ));
        }
        o.push_str(&format!("{line}\n"));
    }

    // ---- program ----
    match &r.program {
        Some(p) => {
            o.push_str(&format!("PROGRAM #{} {} ({})\n", p.id, p.kind.to_uppercase(), p.state));
            o.push_str(&format!(
                "inventory {} → {}\n",
                p.start_qty.round_dp(2), p.current.round_dp(2)
            ));
            o.push_str(&format!(
                "final     {}  glide {}\n",
                p.target_qty.round_dp(2), p.waypoint.round_dp(2)
            ));
            o.push_str(&format!("filled   {} {:>5.1}%\n", bar(p.progress, 20), p.progress * dec!(100)));
            if let Some(sp) = p.schedule_progress {
                o.push_str(&format!("schedule {} {:>5.1}%\n", bar(sp, 20), sp * dec!(100)));
                if let Some(e) = p.ends_at {
                    o.push_str(&format!("ends     {}\n", short_ts(e)));
                }
                // The gap between the two bars is the thing to look at: ahead of
                // schedule means we are paying up, behind means we may not finish.
                let gap = p.progress - sp;
                o.push_str(&format!(
                    "pace     {}\n",
                    if gap.abs() < dec!(0.05) {
                        "ON SCHEDULE"
                    } else if gap > Decimal::ZERO {
                        "AHEAD — filling faster"
                    } else {
                        "BEHIND — may not finish"
                    }
                ));
            }
        }
        None => o.push_str("PROGRAM  none active — idle\n"),
    }
    o.push_str(&format!("{line}\n"));

    // ---- the split ----
    // Directly after the program, because "did the account grow" is the second
    // question after "where is my money", and the market/trading split is the
    // only honest way to answer it.
    let l = &r.lifetime;
    o.push_str(&format!("P&L {}\n", scoreboard_since(r)));
    o.push_str(&format!("{:<10}{:>14}\n", "start", l.start_value.round_dp(2)));
    if !l.net_deposits.is_zero() {
        o.push_str(&format!("{:<10}{:>+14.2}  in/out\n", "deposits", l.net_deposits.round_dp(2)));
    }
    o.push_str(&format!("{:<10}{:>14}\n", "now", l.now_value.round_dp(2)));
    o.push_str(&format!("{:<10}{:>+14.2}\n", "change", l.change().round_dp(2)));
    o.push_str(&format!("  {:<8}{:>+14.2}  price\n", "market", l.market().round_dp(2)));
    o.push_str(&format!("  {:<8}{:>+14.2}  quoting\n", "trading", l.trading.round_dp(2)));
    if l.hours > dec!(1) {
        o.push_str(&format!("  {:<8}{:>+13.1}%  APR\n", "trading", l.trading_apr()));
    }

    // ---- earnings over time ----
    //
    // Every line here is conditional. A section that prints "0.00" for a period
    // the bot was not running reads as a loss, not as an absence.
    let e = &r.earnings;
    if e.per_day_since_start().is_some() || e.daily.len() > 1 || e.weekly.len() > 1 {
        o.push_str(&format!("{line}\nEARNINGS\n"));
        if let Some(rate) = e.per_day_since_start() {
            o.push_str(&format!("{:<12}{:>+10.2} /day\n", "average", rate.round_dp(2)));
            o.push_str(&format!(
                "{:<12}{:>+10.2} over {}\n",
                "total",
                e.total.round_dp(2),
                dur((e.hours * dec!(3600)).to_string().parse::<f64>().unwrap_or(0.0) as i64)
            ));
        }
        if e.daily.len() > 1 {
            o.push_str("by day\n");
            for (day, net) in &e.daily {
                o.push_str(&format!("  {:<10}{:>+10.2}\n", &day[5..], net.round_dp(2)));
            }
        }
        if e.weekly.len() > 1 {
            o.push_str("weekly avg per trading day\n");
            for (label, avg, days) in &e.weekly {
                o.push_str(&format!("  {:<10}{:>+10.2}  {days}d\n", label, avg.round_dp(2)));
            }
        }
    }

    // ---- market making ----
    if !r.maker.is_empty() {
        let hours = Decimal::from(r.window_secs.max(1)) / dec!(3600);
        let total_edge: Decimal = r.maker.iter().map(|m| m.edge_quote).sum();
        let total_fees: Decimal = r.maker.iter().map(|m| m.fees).sum();
        let total_vol: Decimal = r.maker.iter().map(|m| m.volume_quote).sum();
        let total_fills: i64 = r.maker.iter().map(|m| m.fills).sum();
        let maker_fills: i64 = r.maker.iter().map(|m| m.maker_fills).sum();
        let net = total_edge - total_fees;
        let net_bps = if total_vol.is_zero() { Decimal::ZERO } else { net / total_vol * dec!(10000) };
        let gross_bps =
            if total_vol.is_zero() { Decimal::ZERO } else { total_edge / total_vol * dec!(10000) };
        let mkt_vol: Decimal = r.maker.iter().map(|m| m.market_volume * hours).sum();

        o.push_str(&format!("{line}\nMARKET MAKING · last {}\n", dur(r.window_secs)));
        o.push_str(&format!("{:<10}{:>14}\n", "volume", total_vol.round_dp(2)));
        o.push_str(&format!(
            "{:<10}{:>13.2}%  of market\n",
            "share",
            if mkt_vol.is_zero() { Decimal::ZERO } else { total_vol / mkt_vol * dec!(100) }
        ));
        o.push_str(&format!("{:<10}{:>+14.4}\n", "gross", total_edge.round_dp(4)));
        o.push_str(&format!("{:<10}{:>+14.4}\n", "fees", (-total_fees).round_dp(4)));
        o.push_str(&format!("{:<10}{:>+14.4}\n", "net", net.round_dp(4)));
        o.push_str(&format!(
            "{:<10}{:>10}  {:.0}/h {}% mkr\n",
            "fills",
            total_fills,
            Decimal::from(total_fills) / hours,
            if total_fills == 0 { 0 } else { maker_fills * 100 / total_fills }
        ));
        if let Some(w) = &r.weights
            && !w.total().is_zero()
        {
            // Annualised on deployed capital, from this window alone. A short
            // window makes this wildly noisy; it is a scale check, not a forecast.
            o.push_str(&format!(
                "{:<10}{:>13.1}%  on {}\n",
                "annualised",
                net / hours * dec!(8760) / w.total() * dec!(100),
                w.total().round_dp(0)
            ));
        }

        // Edge per unit traded: the size-independent view, and the only one
        // that separates a quiet market from bad quoting.
        let spread = weighted(r, |m| m.sane_spread_bps());
        o.push_str("\nEDGE PER UNIT TRADED\n");
        if spread > Decimal::ZERO {
            o.push_str(&format!("{:<10}{:>8.0} bp  on offer\n", "spread", spread));
        }
        o.push_str(&format!("{:<10}{:>8.0} bp  we quoted\n", "our edge", gross_bps));
        o.push_str(&format!("{:<10}{:>8.0} bp  after fees\n", "kept", net_bps));
        if spread > Decimal::ZERO {
            o.push_str(&format!(
                "{:<10}{:>8.0} %   of spread\n",
                "capture",
                net_bps / spread * dec!(100)
            ));
        }

        o.push_str(&format!("\n{:<9}{:>8}{:>6}{:>6}\n", "by book", "vol", "net", "bps"));
        for m in &r.maker {
            o.push_str(&format!(
                "{:<9}{:>8}{:>6}{:>6.1}\n",
                m.market, m.volume_quote.round_dp(0), m.net().round_dp(1), m.net_bps()
            ));
        }
        o.push_str(&format!(
            "\n{:<9}{:>9}{:>6}{:>6}{:>6}\n", "the books", "vol/h", "sprd", "share", "keep"
        ));
        for m in &r.maker {
            o.push_str(&format!(
                "{:<9}{:>9}{:>5.1}%{:>5.1}%{:>6}\n",
                m.market,
                m.market_volume.round_dp(0),
                m.market_spread * dec!(100),
                m.share(r.window_secs),
                // A spread nobody would cross is not an opportunity we missed,
                // so it reads as unmeasured rather than as a miserable score.
                m.capture().map(|c| format!("{c:.0}%")).unwrap_or_else(|| "—".into())
            ));
        }
    }

    // ---- the arms ----
    if !r.arms.is_empty() {
        o.push_str(&format!("{line}\n"));
        o.push_str(&render_arms(&r.arms));
    }

    // ---- the cycle ----
    //
    // After the arms, because it answers the question the arms cannot: an arm
    // is judged alone, a cycle is judged whole, and a leg the arms defund can
    // still be one the cycle wants rested.
    if let Some(c) = &r.cycle {
        o.push_str(&format!("{line}\n"));
        o.push_str(&render_cycle(c, r.cycle_floor_bps));
    }

    // ---- yield ----
    if let Some(y) = &r.yield_ {
        o.push_str(&format!(
            "{line}\nYIELD · pool as one asset · last {}\n",
            dur((crate::db::now_ms() - y.since_ms) / 1000)
        ));
        o.push_str(&format!(
            "capital {:>10.0} {}\nearned  {:>+10.2} {}  ({:+.2} {})\n",
            y.capital_base, y.base_ccy.to_uppercase(),
            y.earned_base.round_dp(2), y.base_ccy.to_uppercase(),
            y.earned_quote.round_dp(2), r.weights.as_ref().map(|w| w.quote_ccy.to_uppercase()).unwrap_or_default(),
        ));
        o.push_str(&y.rows());
        o.push_str("simple rate, from the average so far\n");
    }

    // ---- what is set aside ----
    if let Some((ccy, pile, earned)) = &r.set_aside {
        o.push_str(&format!("{line}\nSET ASIDE · never sold, safe to withdraw\n"));
        o.push_str(&format!("{:<8}{:>14}\n", ccy, pile.round_dp(4)));
        o.push_str(&format!("{:<8}{:>14}\n", "earned", earned.round_dp(4)));
    }

    // ---- what is spare ----
    if !r.spare.is_empty() {
        o.push_str(&format!("{line}\nSPARE TO WITHDRAW\n"));
        for (ccy, qty) in &r.spare {
            o.push_str(&format!("{:<8}{:>14}\n", ccy, qty.round_dp(4)));
        }
        o.push_str("above the position being steered,\nand a 15% cash reserve for bids\n");
    }

    // ---- price impact ----
    if let Some(i) = &r.impact {
        o.push_str(&format!("{line}\nPRICE\n"));
        o.push_str(&format!(
            "drift  {:>+8.3}%  {} → {}\n",
            i.drift_pct(), i.first_mid.round_dp(5), i.last_mid.round_dp(5)
        ));
        o.push_str(&format!(
            "range  {:>8.3}%  {} … {}\n",
            i.range_pct(), i.low.round_dp(5), i.high.round_dp(5)
        ));
    }

    // ---- execution quality: the headline for a TWAP ----
    o.push_str(&format!("{line}\nEXECUTION vs TWAP\n"));
    if r.execution.is_empty() {
        o.push_str("no fills in this window\n");
    }
    for ex in &r.execution {
        let verb = if ex.side == Side::Buy { "bought" } else { "sold" };
        o.push_str(&format!("{:<7}{:>11} @ {}\n", verb, ex.volume.round_dp(4), ex.our_vwap.round_dp(5)));
        o.push_str(&format!(
            "  twap {}  {:+.3}% {}\n",
            ex.market_twap.round_dp(5),
            ex.edge_pct,
            if ex.edge_pct >= Decimal::ZERO { "✓" } else { "✗" }
        ));
    }
    if let Some(ex) = r.execution.first() {
        o.push_str(&format!("({} book samples)\n", ex.samples));
    }
    o.push_str(&format!("{line}\n"));

    // There is deliberately no average-cost POSITION block here. The one that
    // used to sit at this line carried realised and unrealised P&L forward
    // from launch, per market, against a baseline seeded per asset -- so it
    // started from zero, opened a phantom short on the first sell, and marked
    // the drift of inventory it happened to hold as the bot's doing. Every
    // number the owner reads as profit is markout since the epoch, above.

    // ---- activity ----
    let a = &r.activity;
    o.push_str(&format!("ACTIVITY · last {}\n", dur(r.window_secs)));
    o.push_str(&format!("{:<7}{}\n", "fills", a.fills));
    o.push_str(&format!("{:<7}{} for {}\n", "bought", a.bought.round_dp(4), a.buy_notional.round_dp(4)));
    o.push_str(&format!("{:<7}{} for {}\n", "sold", a.sold.round_dp(4), a.sell_notional.round_dp(4)));
    o.push_str(&format!("{:<7}{}\n", "fees", a.fees.round_dp(6)));
    o.push_str(&format!("{line}\n"));

    // ---- resting orders ----
    o.push_str(&format!("RESTING ORDERS ({})\n", r.open_orders.len()));
    if r.open_orders.is_empty() {
        o.push_str("none\n");
    }
    for (side, price, amount, placed) in &r.open_orders {
        o.push_str(&format!(
            "{:<4} {:>9} × {:>9}  {}\n",
            side.to_uppercase(), price.round_dp(5), amount.round_dp(2), age(*placed)
        ));
    }

    if !r.recent_fills.is_empty() {
        o.push_str(&format!("{line}\nRECENT FILLS\n"));
        for (t, side, price, amount) in &r.recent_fills {
            o.push_str(&format!(
                "{} {:<4} {:>9} × {:>7}\n",
                short_ts(*t), side.to_uppercase(), price.round_dp(5), amount.round_dp(2)
            ));
        }
    }

    // Only real knobs. The settings table also holds internal bookkeeping --
    // the inherited baseline, the fill cutover, the currency names -- and
    // listing those as "overrides" invites someone to change one.
    let live: Vec<_> = r
        .overrides
        .iter()
        .filter(|(k, _)| crate::config::TUNABLE.iter().any(|(t, _)| t == k))
        .collect();
    if !live.is_empty() {
        o.push_str(&format!("{line}\nSETTINGS OVERRIDES\n"));
        for (k, v) in live {
            o.push_str(&format!("{k:<24} {v}\n"));
        }
    }

    if let Some(rate) = &r.api_rate {
        o.push_str(&format!("{line}\nPACING  learned rate {rate}/s\n"));
    }

    if !r.errors.is_empty() {
        o.push_str(&format!("{line}\nWARNINGS\n"));
        for (t, kind, msg) in &r.errors {
            // Message on its own line: prefixing it with a timestamp pushed
            // every warning past the width, and the wrap hid the message.
            let m: String = msg.chars().take(34).collect();
            o.push_str(&format!("{}  [{}]\n  {}\n", short_ts(*t), kind, m));
        }
    }

    o.push_str(&format!("{line}\n\n"));
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::Balance;

    /// Two days, 10 PRL earned on a 1,000 PRL book: 5 a day, 0.5% a day, and
    /// the horizons are that scaled linearly -- simple, because the earnings
    /// are set aside and never compound.
    #[test]
    fn yield_is_the_average_so_far_scaled_simply() {
        let y = Yield {
            base_ccy: "prl".into(),
            since_ms: 0,
            days: dec!(2),
            earned_quote: dec!(13),
            earned_base: dec!(10),
            capital_base: dec!(1000),
            last_hour_quote: Decimal::ZERO,
            last_hour_base: Decimal::ZERO,
        };
        assert_eq!(y.per_day_base(), dec!(5));
        assert_eq!(y.per_day_frac(), dec!(0.005));
        let rows = y.rows();
        assert!(rows.contains("+1825.00 PRL"), "{rows}");
        assert!(rows.contains("+182.50%"), "{rows}");
        assert!(rows.contains("+35.00 PRL") && rows.contains("+3.50%"), "a week: {rows}");

        // No capital, no percentage -- not a division by zero.
        let empty = Yield { capital_base: Decimal::ZERO, ..y };
        assert_eq!(empty.per_day_frac(), Decimal::ZERO);
    }

    /// Build a database in a temp dir with a known set of balances.
    async fn db_with_balances(rows: &[(&str, &str, &str)]) -> (Db, tempdir::TempDir) {
        let dir = tempdir::TempDir::new("safetrade-test").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("base_unit", "prl").await.unwrap();
        db.set_setting("quote_unit", "usdt").await.unwrap();
        for (ccy, avail, locked) in rows {
            db.snapshot_balances(&[Balance {
                currency: ccy.to_string(),
                balance: avail.parse().unwrap(),
                locked: locked.parse().unwrap(),
            }])
            .await
            .unwrap();
            // Distinct timestamps, as successive loops would produce.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        (db, dir)
    }

    /// Regression for the bug that reported an emptied account.
    ///
    /// Balances are written only when they change, so the newest timestamp
    /// normally holds just the currency that moved. Reading "everything at
    /// max(ts)" reported the untouched side as zero -- with USDT written last,
    /// a large PRL holding displayed as 0 PRL.
    #[tokio::test]
    async fn pools_report_each_currency_from_its_own_latest_row() {
        // PRL written first, then USDT alone -- exactly the real sequence.
        let (db, _dir) = db_with_balances(&[
            ("prl", "18917.5866725", "1257.441"),
            ("usdt", "542.26708213872", "0"),
        ])
        .await;
        db.snapshot_book(
            "prlusdt",
            &crate::exchange::OrderBook {
                bids: vec![(dec!(1.04), dec!(100))],
                asks: vec![(dec!(1.05), dec!(100))],
            },
            dec!(0.05), None, None, dec!(20175), dec!(20000),
        )
        .await
        .unwrap();

        let r = build(&db, "prlusdt", 3600).await.unwrap();
        let w = r.weights.expect("weights must be present");
        assert_eq!(w.base_qty(), dec!(20175.0276725), "PRL must include locked size");
        let usdt = w.pools.iter().find(|p| p.ccy == "usdt").expect("usdt pool");
        assert_eq!(usdt.qty, dec!(542.26708213872));
        assert!(w.base_weight() > dec!(0.9), "PRL should dominate, got {}", w.base_weight());
    }

    /// The reverse order must work too: PRL written last, USDT stale.
    #[tokio::test]
    async fn pools_are_correct_whichever_currency_moved_last() {
        let (db, _dir) = db_with_balances(&[
            ("usdt", "542.26", "0"),
            ("prl", "18917.58", "1257.44"),
        ])
        .await;
        let r = build(&db, "prlusdt", 3600).await.unwrap();
        let w = r.weights.or_else(|| None);
        // No book snapshot means no mark, so weights are absent by design --
        // assert the balances themselves rather than silently passing.
        assert!(w.is_none(), "without a mark price there is nothing to weigh");
        let bal: (String, String) = sqlx::query_as(
            "SELECT available, locked FROM balances WHERE currency = 'prl' ORDER BY ts DESC LIMIT 1",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(num::parse(&bal.0) + num::parse(&bal.1), dec!(20175.02));
    }

    /// A currency the account has never held must read zero, not explode.
    #[tokio::test]
    async fn a_currency_never_recorded_reads_zero() {
        let (db, _dir) = db_with_balances(&[("usdt", "100", "0")]).await;
        let bal: Option<(String, String)> = sqlx::query_as(
            "SELECT available, locked FROM balances WHERE currency = 'prl' ORDER BY ts DESC LIMIT 1",
        )
        .fetch_optional(db.pool())
        .await
        .unwrap();
        assert!(bal.is_none());
    }

    /// Selling above the mid and buying below it must both score positive,
    /// and the score must not depend on where the price went afterwards.
    /// The two attributions must add up to the account's change, and a deposit
    /// must not be counted as profit.
    #[test]
    fn the_pnl_split_adds_up_and_ignores_deposits() {
        let l = Lifetime {
            start_value: dec!(20000),
            now_value: dec!(31500),
            net_deposits: dec!(10000), // paid in, not earned
            trading: dec!(120),
            fees: dec!(5),
            scored_fees: dec!(5),
            hours: dec!(24),
        };
        // 31,500 − 20,000 − 10,000 = 1,500 of real change.
        assert_eq!(l.change(), dec!(1500));
        // Of which 120 was our quoting, so 1,380 was the market.
        assert_eq!(l.market(), dec!(1380));
        assert_eq!(l.market() + l.trading, l.change(), "the split must reconcile");
    }

    #[test]
    fn a_withdrawal_is_not_a_loss() {
        let l = Lifetime {
            start_value: dec!(20000),
            now_value: dec!(15000),
            net_deposits: dec!(-5000), // taken out
            trading: dec!(50),
            ..Default::default()
        };
        assert_eq!(l.change(), dec!(0), "taking money out is not a loss");
        assert_eq!(l.market(), dec!(-50));
    }

    #[test]
    fn trading_apr_is_zero_before_any_time_passes() {
        let l = Lifetime { now_value: dec!(1000), trading: dec!(10), ..Default::default() };
        assert_eq!(l.trading_apr(), Decimal::ZERO, "must not divide by zero hours");
    }

    /// Nothing about earnings prints until there is something to say. A
    /// section showing 0.00 for a period the bot was not running reads as a
    /// loss rather than an absence.
    /// Our share must compare like with like. Market volume is stored per
    /// hour; ours covers the report window. Dividing one by the other reported
    /// a 1.9% share as 44% -- the difference between "nowhere near the cap" and
    /// "apparently breaching it".
    #[test]
    fn participation_share_matches_the_window() {
        let our_24h = dec!(70727);
        let market_per_hour = dec!(158636);
        let window_hours = dec!(24);

        let wrong = our_24h / market_per_hour * dec!(100);
        let right = our_24h / (market_per_hour * window_hours) * dec!(100);
        assert!(wrong > dec!(40), "the old arithmetic really did say {wrong}%");
        assert!(right < dec!(2), "the true share is {right}%");
    }

    #[test]
    fn earnings_stay_silent_without_data() {
        let empty = Earnings::default();
        assert!(empty.per_day_since_start().is_none(), "no hours, no rate");
        assert!(empty.daily.is_empty());
        assert!(empty.weekly.is_empty());

        // Too young to extrapolate: twenty minutes says nothing about a day.
        let young = Earnings { total: dec!(5), hours: dec!(0.33), ..Default::default() };
        assert!(young.per_day_since_start().is_none());

        // Past the threshold it reports a rate.
        let grown = Earnings { total: dec!(48), hours: dec!(12), ..Default::default() };
        assert_eq!(grown.per_day_since_start(), Some(dec!(96)), "48 in 12h is 96/day");
    }

    /// Weekly averages divide by days that TRADED, not by seven. A bot started
    /// on a Friday has not had a bad week, it has had two days.
    #[tokio::test]
    async fn weekly_average_divides_by_trading_days() {
        let dir = tempdir::TempDir::new("safetrade-earn").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let now = crate::db::now_ms();
        db.seed_account_value(dec!(1000)).await.unwrap();

        // Three fills on three separate days, 10 of MARKOUT each, no fees.
        // Earnings are scored five minutes on, not at the instant of the fill.
        for (i, day_ago) in [0i64, 1, 2].iter().enumerate() {
            sqlx::query(
                "INSERT INTO fills(trade_id, market, side, price, amount, total, fee,
                                   fee_currency, liquidity, created_at, pos_after, cost_after,
                                   realized, mid_at_fill, edge, markout)
                 VALUES (?, 'prlusdt','sell','1.1','10','11','0','usdt','maker',?,
                         '0','0','0','1.0','10','10')",
            )
            .bind(i as i64 + 1)
            .bind(now - day_ago * 86_400_000)
            .execute(db.pool()).await.unwrap();
        }

        let r = build(&db, "prlusdt", 7 * 86_400).await.unwrap();
        assert_eq!(r.earnings.daily.len(), 3, "one entry per day that traded");
        for (_, net) in &r.earnings.daily {
            assert_eq!(*net, dec!(10));
        }
        let (_, avg, days) = r.earnings.weekly.first().expect("a week");
        assert_eq!(*days, 3, "three trading days, not seven");
        assert_eq!(*avg, dec!(10), "average per trading day, not per calendar day");
    }

    #[tokio::test]
    async fn maker_edge_measures_distance_from_the_mid_both_ways() {
        let dir = tempdir::TempDir::new("safetrade-edge").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("base_unit", "prl").await.unwrap();
        db.set_setting("quote_unit", "usdt").await.unwrap();

        let now = crate::db::now_ms();
        // Mid sits at 1.00 throughout.
        for i in 0..4 {
            sqlx::query(
                "INSERT INTO book_snapshots(ts, market, mid) VALUES (?, 'prlusdt', '1.00')",
            )
            .bind(now - 60_000 + i * 10_000)
            .execute(db.pool())
            .await
            .unwrap();
        }
        // Sold 100 at 1.02 (2c above mid) and bought 100 at 0.98 (2c below).
        // Each earns 2.00, so the edge is 4.00 regardless of direction.
        for (id, side, price) in [(1i64, "sell", "1.02"), (2, "buy", "0.98")] {
            sqlx::query(
                "INSERT INTO fills(trade_id, market, side, price, amount, total, fee,
                                   fee_currency, liquidity, created_at, pos_after, cost_after, realized)
                 VALUES (?, 'prlusdt', ?, ?, '100', '100', '0.1', 'usdt', 'maker', ?, '0','0','0')",
            )
            .bind(id).bind(side).bind(price).bind(now - 30_000)
            .execute(db.pool()).await.unwrap();
        }

        let r = build(&db, "prlusdt", 3600).await.unwrap();
        let m = r.maker.first().expect("one book");
        assert_eq!(m.fills, 2);
        assert_eq!(m.edge_quote, dec!(4.00), "2c above mid selling + 2c below buying");
        assert_eq!(m.maker_fills, 2);
        assert_eq!(m.fees, dec!(0.2));
        assert_eq!(m.net(), dec!(3.80));
        assert_eq!(m.edge_bps(), dec!(200), "4.00 on 200 of volume is 200bps");
    }

    /// A fill with no nearby snapshot must be counted but not scored, rather
    /// than measured against a price from another hour.
    /// An edge stored at ingestion must be used verbatim, never recomputed.
    /// Recomputing is how the same trade came to score differently depending
    /// on which snapshots still existed.
    /// Edge must be scored against the mid WHEN THE FILL HAPPENED, not when we
    /// noticed it. In a trending market those differ by the trend, and a
    /// resting maker order looks terrible measured against a price that only
    /// existed after it traded.
    #[tokio::test]
    async fn edge_uses_the_mid_at_fill_time_not_at_ingestion() {
        let dir = tempdir::TempDir::new("safetrade-attime").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let now = crate::db::now_ms();

        // A rally: 1.11 at the moment of the fill, 1.13 a minute later.
        for (ago, mid) in [(90_000i64, "1.11"), (0, "1.13")] {
            sqlx::query("INSERT INTO book_snapshots(ts, market, mid) VALUES (?, 'prlusdt', ?)")
                .bind(now - ago)
                .bind(mid)
                .execute(db.pool()).await.unwrap();
        }

        // Our ask rested at 1.11 and was lifted 90 seconds ago.
        let t = crate::exchange::Trade {
            id: 1,
            order_id: None,
            market: "prlusdt".into(),
            side: Some("sell".into()),
            order_side: Some("sell".into()),
            taker_type: Some("buy".into()),
            price: dec!(1.11),
            amount: dec!(100),
            total: dec!(111),
            fee: Some(dec!(0.111)),
            fee_currency: Some("usdt".into()),
            created_at: (now - 90_000) / 1000,
        };
        // Pass the LATER mid as the fallback, as the live loop would.
        db.ingest_fills("prlusdt", std::slice::from_ref(&t), Some(dec!(1.13)))
            .await
            .unwrap();

        let stored: String = sqlx::query_scalar("SELECT mid_at_fill FROM fills WHERE trade_id = 1")
            .fetch_one(db.pool()).await.unwrap();
        assert_eq!(num::parse(&stored), dec!(1.11), "must use the mid at the fill");

        let edge: String = sqlx::query_scalar("SELECT edge FROM fills WHERE trade_id = 1")
            .fetch_one(db.pool()).await.unwrap();
        assert_eq!(num::parse(&edge), dec!(0), "sold at the mid: no edge, not a loss");
        // Against the later mid it would have scored -2.00, which is the trend,
        // not the trade.
    }

    #[tokio::test]
    async fn a_stored_edge_wins_over_any_later_recomputation() {
        let dir = tempdir::TempDir::new("safetrade-stored").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("base_unit", "prl").await.unwrap();
        db.set_setting("quote_unit", "usdt").await.unwrap();
        let now = crate::db::now_ms();

        // A snapshot that would imply a completely different edge.
        sqlx::query("INSERT INTO book_snapshots(ts, market, mid) VALUES (?, 'prlusdt', '9.99')")
            .bind(now - 30_000)
            .execute(db.pool()).await.unwrap();
        // The fill carries the edge settled when it happened.
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized,
                               mid_at_fill, edge, markout)
             VALUES (1,'prlusdt','sell','1.02','100','102','0.1','usdt','maker',?,
                     '0','0','0','1.00','2.00','1.50')",
        )
        .bind(now - 30_000)
        .execute(db.pool()).await.unwrap();

        let r = build(&db, "prlusdt", 3600).await.unwrap();
        assert_eq!(r.maker[0].edge_quote, dec!(2.00), "stored edge must be authoritative");
        // Lifetime reports the MARKOUT, not the edge: 1.50 judged five minutes
        // on, less the 0.10 fee. The edge of 2.00 is the flattering number and
        // no longer drives anything the owner reads as profit.
        assert_eq!(r.lifetime.trading, dec!(1.40), "lifetime uses markout, less fees");
    }

    /// Backfill must score old rows once and leave settled ones alone.
    #[tokio::test]
    async fn backfill_fills_in_missing_edges_only() {
        let dir = tempdir::TempDir::new("safetrade-backfill").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let now = crate::db::now_ms();
        sqlx::query("INSERT INTO book_snapshots(ts, market, mid) VALUES (?, 'prlusdt', '1.00')")
            .bind(now - 30_000).execute(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized)
             VALUES (1,'prlusdt','sell','1.05','100','105','0.1','usdt','maker',?,'0','0','0')",
        ).bind(now - 30_000).execute(db.pool()).await.unwrap();

        assert_eq!(db.backfill_edges().await.unwrap(), 1);
        let edge: String = sqlx::query_scalar("SELECT edge FROM fills WHERE trade_id = 1")
            .fetch_one(db.pool()).await.unwrap();
        assert_eq!(num::parse(&edge), dec!(5.00), "(1.05 - 1.00) x 100");
        assert_eq!(db.backfill_edges().await.unwrap(), 0, "already settled, left alone");
    }

    #[tokio::test]
    async fn a_fill_with_no_nearby_mid_is_not_scored() {
        let dir = tempdir::TempDir::new("safetrade-nomid").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("base_unit", "prl").await.unwrap();
        db.set_setting("quote_unit", "usdt").await.unwrap();
        let now = crate::db::now_ms();
        sqlx::query("INSERT INTO book_snapshots(ts, market, mid) VALUES (?, 'prlusdt', '1.00')")
            .bind(now - 3_000_000) // ~50 minutes before the fill
            .execute(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized)
             VALUES (1,'prlusdt','sell','1.50','100','150','0.15','usdt','maker',?,'0','0','0')",
        )
        .bind(now - 30_000)
        .execute(db.pool()).await.unwrap();

        let r = build(&db, "prlusdt", 3600).await.unwrap();
        let m = r.maker.first().expect("one book");
        assert_eq!(m.fills, 1, "still counted");
        assert_eq!(m.edge_quote, Decimal::ZERO, "but not scored against a stale mid");
    }

    #[tokio::test]
    async fn compact_status_names_both_pools_and_the_bot_pnl() {
        let (db, _dir) = db_with_balances(&[
            ("prl", "18917.58", "1257.44"),
            ("usdt", "542.26", "0"),
        ])
        .await;
        db.snapshot_book(
            "prlusdt",
            &crate::exchange::OrderBook {
                bids: vec![(dec!(1.04), dec!(100))],
                asks: vec![(dec!(1.05), dec!(100))],
            },
            dec!(0.05), None, None, dec!(20175), dec!(20000),
        )
        .await
        .unwrap();

        let r = build(&db, "prlusdt", 3600).await.unwrap();
        let out = render_compact(&r);
        assert!(out.contains("PRL"), "must name the base pool");
        assert!(out.contains("USDT"), "must name the quote pool");
        assert!(out.contains("20175"), "must show the real PRL holding, got:\n{out}");
        // The headline is what the QUOTING earned, judged five minutes on.
        // The market's own move is not ours and is not shown here.
        assert!(out.contains("Quoting"), "must report the quoting:\n{out}");
        assert!(out.contains("after 5 min"), "and say how it is judged:\n{out}");
        assert!(!out.contains("PRL price"), "market returns do not belong here:\n{out}");
        assert!(!out.contains("<pre><pre>"), "no nested pre blocks");
        // Nothing traded in this window, so there is no edge to express. A
        // block reading "0 bp" for a quiet hour looks like terrible quoting
        // rather than like no quoting, which is the whole reason it is
        // conditional.
        assert!(!out.contains("Edge per unit"), "no volume, so no edge block:\n{out}");
    }

    /// The four numbers the owner reads first: how much we traded, what share
    /// of the market that was, the edge we quoted, and how much of the spread
    /// on offer we actually kept.
    #[tokio::test]
    async fn compact_status_reports_volume_share_edge_and_capture() {
        let (db, _dir) = db_with_balances(&[("prl", "18917.58", "1257.44"), ("usdt", "542.26", "0")]).await;
        let now = crate::db::now_ms();
        db.snapshot_book(
            "prlusdt",
            &crate::exchange::OrderBook {
                bids: vec![(dec!(1.04), dec!(100))],
                asks: vec![(dec!(1.05), dec!(100))],
            },
            dec!(0.05), None, None, dec!(20175), dec!(20000),
        )
        .await
        .unwrap();
        // The book itself: 500,000 of quote volume an hour, 96 bp wide. These
        // are what `share` and `capture` measure us against.
        db.set_setting("vol:prlusdt", "500000").await.unwrap();
        db.set_setting("spread:prlusdt", "0.0096").await.unwrap();
        // Sold 100 at 1.06 against a 1.045 mid: 1.50 of gross edge on 106 of
        // notional, about 142 bp, against a book offering 96 bp.
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized, edge)
             VALUES (1,'prlusdt','sell','1.06','100','106','0.106','usdt','maker',?,'0','0','0','1.50')",
        )
        .bind(now - 30_000)
        .execute(db.pool()).await.unwrap();

        let r = build(&db, "prlusdt", 3600).await.unwrap();
        let out = render_compact(&r);
        assert!(out.contains("volume"), "must report what we traded:\n{out}");
        assert!(out.contains("of mkt"), "must report our share of the market:\n{out}");
        assert!(out.contains("our edge"), "must report the edge we quoted:\n{out}");
        assert!(out.contains("capture"), "must report how much of it we kept:\n{out}");

        let m = r.maker.first().expect("one book");
        assert!(m.share(3600) > Decimal::ZERO, "share must be measured, got {}", m.share(3600));
        assert!(m.edge_bps() > m.net_bps(), "fees must come out between gross and net");
        assert!(m.capture().is_some(), "a 96 bp spread is a real one");
    }

    /// A fill that looks good and then goes bad must read as a loss.
    ///
    /// This is the whole point of markout. We sold at 1.06 into a 1.045 mid --
    /// `edge` books that as a win and always will, because we quote outside the
    /// mid. Five minutes later the price is 1.12, so we sold 6 cents under
    /// where it went: that fill lost money and only markout can say so.
    #[tokio::test]
    async fn markout_sees_the_adverse_selection_that_edge_cannot() {
        let (db, _dir) = db_with_balances(&[("prl", "1000", "0"), ("usdt", "1000", "0")]).await;
        let now = crate::db::now_ms();
        let fill_ts = now - crate::db::MARKOUT_HORIZON_MS - 60_000;

        // The book at the fill, and again five minutes later, higher.
        for (ts, mid) in [(fill_ts, "1.045"), (fill_ts + crate::db::MARKOUT_HORIZON_MS, "1.12")] {
            sqlx::query("INSERT INTO book_snapshots(ts, market, mid) VALUES (?, 'prlusdt', ?)")
                .bind(ts)
                .bind(mid)
                .execute(db.pool()).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized, edge)
             VALUES (1,'prlusdt','sell','1.06','100','106','0.106','usdt','maker',?,'0','0','0','1.50')",
        )
        .bind(fill_ts)
        .execute(db.pool()).await.unwrap();

        assert_eq!(db.settle_markouts().await.unwrap(), 1, "the fill is old enough to score");

        let r = build(&db, "prlusdt", 86_400).await.unwrap();
        let m = r.maker.first().expect("one book");
        assert_eq!(m.scored, 1);
        assert!(m.edge_quote > Decimal::ZERO, "edge says we won: {}", m.edge_quote);
        // (1.06 - 1.12) x 100 = -6
        assert_eq!(m.markout_quote, dec!(-6), "markout must say we lost");
        assert!(
            m.markout_bps() < m.edge_bps(),
            "markout must be the harsher of the two: {} vs {}", m.markout_bps(), m.edge_bps()
        );
    }

    /// The arm table the engine decided from the orders and fills must be
    /// visible, with each side's size and its regime, in both renderings. An
    /// arm quietly quoting the minimum is exactly the state an operator
    /// needs to see, and so is the storm cell that is sized below its calm
    /// sibling.
    #[tokio::test]
    async fn the_report_shows_the_arms_and_what_each_is_sized_at() {
        use crate::strategy::arms::{Arm, ArmSize, Bucket, Regime, State, Width};
        let (db, _dir) = db_with_balances(&[("prl", "1000", "0"), ("usdt", "1000", "0")]).await;
        let mk = |market: &str, side: Side, bucket, regime, width, share: Decimal, y: f64, se: f64| ArmSize {
            arm: Arm { market: market.into(), side, bucket, regime, width },
            share,
            state: State::Sized,
            fills: 120.0,
            exposure_dh: 30_000.0,
            yield_pm: y,
            se_pm: se,
            p_pays: 0.5 + f64::try_from(share).unwrap() / 2.0,
            bet: 250.0,
        };
        // The live shape: prlusdt's touch losing on a one-tick book and
        // paying on a wide one, prlusdc's far rungs paying, and the storm
        // cell sized below the calm one.
        let arms = vec![
            mk("prlusdt", Side::Buy, Bucket::Under50, Regime::Calm, Width::One, dec!(0), -13.4, 7.9),
            mk("prlusdt", Side::Sell, Bucket::Under50, Regime::Calm, Width::One, dec!(0), -7.1, 3.7),
            mk("prlusdt", Side::Buy, Bucket::From50, Regime::Calm, Width::Wide, dec!(0.88), 146.0, 75.0),
            mk("prlusdc", Side::Buy, Bucket::From200, Regime::Calm, Width::Wide, dec!(0.42), 10.8, 12.0),
            mk("prlusdc", Side::Sell, Bucket::From200, Regime::Calm, Width::Wide, dec!(0.97), 15.5, 10.9),
            mk("prlusdc", Side::Sell, Bucket::From200, Regime::Wild, Width::Wide, dec!(0.31), 15.5, 23.0),
        ];
        db.set_setting("arms", &serde_json::to_string(&arms).unwrap()).await.unwrap();

        let r = build(&db, "prlusdt", 3600).await.unwrap();
        assert_eq!(r.arms, arms);
        let full = render(&r);
        assert!(full.contains("ARMS"), "{full}");
        // Each market is named once, above its rows.
        let rows_under = |text: &str, market: &str| -> Vec<String> {
            text.lines()
                .skip_while(|l| l.trim() != market)
                .skip(1)
                .take_while(|l| l.starts_with(' '))
                .map(str::to_string)
                .collect()
        };
        let usdt = rows_under(&full, "prlusdt");
        let touch = usdt.iter().find(|l| l.contains("0-50")).unwrap_or_else(|| panic!("the touch row:\n{full}"));
        assert!(touch.contains("calm") && touch.contains(" 1 ") && touch.contains("0%"), "{touch}");
        let wide = usdt.iter().find(|l| l.contains("3+")).unwrap_or_else(|| panic!("the wide row:\n{full}"));
        assert!(wide.contains("+146") && wide.contains("88%") && wide.contains("—"), "{wide}");
        assert!(touch.contains("-10.") || touch.contains("-11."), "pooled touch yield: {touch}");
        assert!(full.contains("97%"), "the nearly full side's size:\n{full}");
        let usdc = rows_under(&full, "prlusdc");
        let wild = usdc.iter().find(|l| l.contains("wild")).unwrap_or_else(|| panic!("the storm row:\n{full}"));
        assert!(wild.contains("31%") && wild.contains("—"), "{wild}");
        assert!(!usdc.iter().any(|l| l.contains("0-50")), "prlusdc's rows are its own:\n{full}");
        assert!(full.contains("sized"), "the states:\n{full}");

        // The compact view carries the same table at phone width. Measured
        // on what RENDERS, not on the markup: <b> and <pre> cost characters
        // here and nothing on the screen.
        let compact = render_compact(&r);
        assert!(compact.contains("Arms"), "{compact}");
        assert!(rows_under(&strip_tags(&compact), "prlusdc").iter().any(|l| l.contains("200")), "{compact}");
        for line in compact.lines() {
            let visible = strip_tags(line);
            assert!(
                visible.chars().count() <= 40,
                "compact line renders {} chars, past the phone width: {visible}",
                visible.chars().count()
            );
        }
    }

    /// The arm labels are data that flows into markup, so they carry none of
    /// their own. Checked at the source rather than only in the rendering,
    /// because the next renderer will not remember to escape them either.
    #[test]
    fn arm_labels_carry_no_markup() {
        use crate::strategy::arms::{Bucket, Regime};
        for l in Bucket::ALL.iter().map(|b| b.label()).chain(Regime::ALL.iter().map(|r| r.label())) {
            assert!(!l.contains('<') && !l.contains('>') && !l.contains('&'), "label {l:?}");
        }
    }

    /// The verdict must not call a coin flip.
    ///
    /// The whole purpose of leading with it: 23 episodes of +6 bp is noise,
    /// and a screen that says EARNING on that is worse than one that says
    /// nothing. The threshold is two standard errors at the measured
    /// per-episode spread of ~113 bp.
    /// A huge early number must not shortcut the wait.
    ///
    /// Live on the first day: +276 bp over TWO episodes printed EARNING,
    /// because the episodes needed scaled with the size of the reading and a
    /// big enough reading demanded none. Two lucky fills are not evidence.
    #[test]
    fn a_wild_early_reading_does_not_declare_victory() {
        for bps in [dec!(276), dec!(500), dec!(2000), dec!(-400)] {
            assert!(
                matches!(Verdict::of(bps, 2), Verdict::Unproven { .. }),
                "{bps} bp over 2 episodes must stay unproven"
            );
            assert!(matches!(Verdict::of(bps, 29), Verdict::Unproven { .. }), "{bps} at 29");
        }
        // Past the floor a large, consistent edge is allowed to be called.
        assert!(matches!(Verdict::of(dec!(276), 31), Verdict::Earning { .. }));
    }

    #[test]
    fn the_verdict_waits_for_enough_evidence() {
        // A small edge needs a lot of episodes before it means anything.
        assert!(matches!(
            Verdict::of(dec!(6), 23),
            Verdict::Unproven { needed, .. } if needed > 1000
        ));
        assert!(matches!(Verdict::of(dec!(20), 50), Verdict::Unproven { .. }));

        // A 20 bp edge over ~130 episodes clears it.
        assert!(matches!(Verdict::of(dec!(20), 200), Verdict::Earning { .. }));
        assert!(matches!(Verdict::of(dec!(-20), 200), Verdict::Losing { .. }));

        // A big edge is obvious quickly: 50 bp needs about 20 episodes.
        assert!(matches!(Verdict::of(dec!(50), 30), Verdict::Earning { .. }));
    }

    /// The headline says which of the three states we are in, in words.
    #[test]
    fn the_verdict_says_how_far_off_it_is() {
        let text = Verdict::of(dec!(6), 23).headline();
        assert!(text.contains("TOO EARLY"), "{text}");
        assert!(text.contains("23 of"), "must say how many we have: {text}");
        assert!(Verdict::of(dec!(30), 100).headline().contains("EARNING"));
        assert!(Verdict::of(dec!(-30), 100).headline().contains("LOSING"));
    }

    /// Only what is above the steered position, and above a cash reserve, is
    /// safe to take out.
    ///
    /// The owner asked what he can withdraw. Base above the program's target
    /// is genuinely spare: below it the ask ladder has nothing to sell. Cash
    /// is different -- the bid ladder refills as it fills, so a reserve is
    /// held rather than a need computed.
    #[tokio::test]
    async fn spare_is_what_is_above_the_target_and_the_cash_reserve() {
        let (db, _dir) = db_with_balances(&[("prl", "30000", "0"), ("usdt", "20000", "0")]).await;
        db.snapshot_book(
            "prlusdt",
            &crate::exchange::OrderBook {
                bids: vec![(dec!(1.00), dec!(100))],
                asks: vec![(dec!(1.00), dec!(100))],
            },
            dec!(0.01), None, None, dec!(30000), dec!(25000),
        )
        .await
        .unwrap();
        db.create_program(
            "prl", crate::db::models::ProgramKind::Neutral,
            dec!(30000), dec!(25000), None, None, None, None,
        )
        .await
        .unwrap();

        let r = build_full(&db, "prlusdt", "prl", &[], 3600).await.unwrap();
        let spare = |c: &str| {
            r.spare.iter().find(|(k, _)| k == c).map(|(_, v)| *v).unwrap_or_default()
        };
        // Holding 30,000 against a 25,000 target: 5,000 is genuinely spare.
        assert_eq!(spare("PRL"), dec!(5000));
        // Equity is 30,000 x 1.00 + 20,000 = 50,000; the reserve is 15% of
        // that, 7,500, so 12,500 of the 20,000 USDT is spare.
        assert_eq!(spare("USDT"), dec!(12500));

        let out = render_compact(&r);
        assert!(out.contains("Spare to withdraw"), "{out}");
    }

    /// A twap's progress is measured against where it is GOING, not the
    /// waypoint its ramp is passing through.
    ///
    /// Against the waypoint, a twap five minutes into six hours reads "100%
    /// filled" -- on pace looks finished, which is the least useful thing a
    /// status can say while nearly all of it is still unspent.
    #[tokio::test]
    async fn a_twap_reports_progress_against_its_destination() {
        let (db, _dir) = db_with_balances(&[("prl", "300", "0"), ("usdt", "79000", "0")]).await;
        db.snapshot_book(
            "prlusdt",
            &crate::exchange::OrderBook {
                bids: vec![(dec!(1.50), dec!(100))],
                asks: vec![(dec!(1.50), dec!(100))],
            },
            // The waypoint the ramp is passing through: 320 of 53,000.
            dec!(0.01), None, None, dec!(300), dec!(320),
        )
        .await
        .unwrap();
        db.create_program(
            "prl", crate::db::models::ProgramKind::Twap,
            Decimal::ZERO, dec!(53000), None, None,
            Some(crate::db::now_ms() + 6 * 3_600_000), None,
        )
        .await
        .unwrap();

        let r = build_full(&db, "prlusdt", "prl", &[], 3600).await.unwrap();
        let pr = r.program.expect("a program");
        assert_eq!(pr.kind, "twap");
        assert_eq!(pr.target_qty, dec!(53000), "the destination, not the waypoint");
        assert_eq!(pr.waypoint, dec!(320), "the waypoint is still reported");
        assert!(pr.progress < dec!(0.02), "300 of 53,000 is barely started: {}", pr.progress);
    }

    /// Nothing is spare when we are at or under the target and thin on cash.
    #[tokio::test]
    async fn nothing_is_spare_when_the_ladder_needs_it_all() {
        let (db, _dir) = db_with_balances(&[("prl", "20000", "0"), ("usdt", "1000", "0")]).await;
        db.snapshot_book(
            "prlusdt",
            &crate::exchange::OrderBook {
                bids: vec![(dec!(1.00), dec!(100))],
                asks: vec![(dec!(1.00), dec!(100))],
            },
            dec!(0.01), None, None, dec!(20000), dec!(25000),
        )
        .await
        .unwrap();
        db.create_program(
            "prl", crate::db::models::ProgramKind::Neutral,
            dec!(20000), dec!(25000), None, None, None, None,
        )
        .await
        .unwrap();

        let r = build_full(&db, "prlusdt", "prl", &[], 3600).await.unwrap();
        assert!(r.spare.is_empty(), "under target and thin on cash: {:?}", r.spare);
        assert!(!render_compact(&r).contains("Spare to withdraw"));
    }

    /// Results before the epoch must not be counted.
    ///
    /// A change that fixes the quoting is invisible for a day if it has to drag
    /// the old settings' losses behind it, and a day is far too slow to iterate
    /// on. The line is drawn with `ctl epoch`.
    #[tokio::test]
    async fn the_epoch_hides_everything_that_came_before_it() {
        let (db, _dir) = db_with_balances(&[("prl", "1000", "0"), ("usdt", "1000", "0")]).await;
        let now = crate::db::now_ms();
        // One bad fill two hours ago, one good fill ten minutes ago.
        for (id, ts, markout) in [(1i64, now - 7_200_000, "-50"), (2, now - 600_000, "8")] {
            sqlx::query(
                "INSERT INTO fills(trade_id, market, side, price, amount, total, fee,
                                   fee_currency, liquidity, created_at, pos_after, cost_after,
                                   realized, markout)
                 VALUES (?,'prlusdt','sell','1.05','100','105','0','usdt','maker',?,'0','0','0',?)",
            )
            .bind(id).bind(ts).bind(markout)
            .execute(db.pool()).await.unwrap();
        }

        // With no line drawn, both count and the total is the bad one.
        let r = build(&db, "prlusdt", 86_400).await.unwrap();
        assert_eq!(r.lifetime.trading, dec!(-42), "-50 and +8");

        // Draw the line an hour ago: only the good fill survives.
        db.set_setting(crate::db::EPOCH_KEY, &(now - 3_600_000).to_string()).await.unwrap();
        db.set_setting(crate::db::EPOCH_REASON_KEY, "settings changed").await.unwrap();
        let r = build(&db, "prlusdt", 86_400).await.unwrap();
        assert_eq!(r.lifetime.trading, dec!(8), "only what happened after the line");
        assert_eq!(r.epoch_ms, now - 3_600_000);

        // And both screens say which window they are talking about, and why.
        let stamp = format!("since {} · settings changed", short_ts(now - 3_600_000));
        let out = render_compact(&r);
        assert!(out.contains("Quoting"), "{out}");
        assert!(out.contains(&stamp), "the compact status must name the window:\n{out}");
        assert!(!out.contains("since launch"), "must not claim to be lifetime:\n{out}");
        let full = render(&r);
        assert!(full.contains(&format!("P&L {stamp}")), "the report must name the window:\n{full}");
        assert!(!full.contains("SINCE LAUNCH") && !full.contains("POSITION\n"), "{full}");
    }

    /// A database the engine has never run on has no line, and says so rather
    /// than inventing a date.
    #[tokio::test]
    async fn without_a_line_the_scoreboard_is_since_launch() {
        let (db, _dir) = db_with_balances(&[("prl", "1000", "0"), ("usdt", "1000", "0")]).await;
        let r = build(&db, "prlusdt", 3600).await.unwrap();
        assert_eq!(r.epoch_ms, 0);
        assert!(render_compact(&r).contains("since launch"));
        assert!(render(&r).contains("P&L since launch"));
    }

    /// Until the engine has seeded the new start, the account is measured
    /// from its live value: a fresh scoreboard shows no change, not the whole
    /// account as market gain.
    #[tokio::test]
    async fn a_fresh_line_measures_the_account_from_its_live_value() {
        let (db, _dir) = db_with_balances(&[("prl", "3488", "0"), ("usdt", "2261", "0")]).await;
        db.snapshot_book(
            "prlusdt",
            &crate::exchange::OrderBook {
                bids: vec![(dec!(1.37), dec!(100))],
                asks: vec![(dec!(1.37), dec!(100))],
            },
            dec!(0.01), None, None, dec!(3488), dec!(3488),
        )
        .await
        .unwrap();
        db.reset_analytics_epoch("settings changed").await.unwrap();
        let r = build(&db, "prlusdt", 3600).await.unwrap();
        let l = &r.lifetime;
        assert_eq!(l.start_value, l.now_value, "{} vs {}", l.start_value, l.now_value);
        assert_eq!(l.change(), Decimal::ZERO);
        assert_eq!(l.market(), Decimal::ZERO);
    }

    /// A young epoch buckets by hour, because waiting for a calendar day to
    /// close before a change shows anything is not a feedback loop.
    #[tokio::test]
    async fn a_young_epoch_reports_by_hour() {
        let (db, _dir) = db_with_balances(&[("prl", "1000", "0"), ("usdt", "1000", "0")]).await;
        let now = crate::db::now_ms();
        db.set_setting(crate::db::EPOCH_KEY, &(now - 3 * 3_600_000).to_string()).await.unwrap();
        for (id, hours_ago) in [(1i64, 0i64), (2, 1), (3, 2)] {
            sqlx::query(
                "INSERT INTO fills(trade_id, market, side, price, amount, total, fee,
                                   fee_currency, liquidity, created_at, pos_after, cost_after,
                                   realized, markout)
                 VALUES (?,'prlusdt','sell','1.05','100','105','0','usdt','maker',?,'0','0','0','5')",
            )
            .bind(id).bind(now - hours_ago * 3_600_000 - 600_000)
            .execute(db.pool()).await.unwrap();
        }
        let r = build(&db, "prlusdt", 86_400).await.unwrap();
        assert_eq!(r.earnings.daily.len(), 3, "three hours, not one day");
        assert!(
            r.earnings.daily[0].0.len() > 10,
            "the bucket key carries an hour: {}", r.earnings.daily[0].0
        );
        let out = render_compact(&r);
        assert!(out.contains("By hour"), "{out}");
    }

    /// Fills too recent to judge are left unscored rather than scored as zero.
    /// Counting them as zero would dilute the average toward nothing and make
    /// a bad hour look merely quiet.
    #[tokio::test]
    async fn a_fill_too_recent_to_judge_is_not_scored_yet() {
        let (db, _dir) = db_with_balances(&[("prl", "1000", "0"), ("usdt", "1000", "0")]).await;
        let now = crate::db::now_ms();
        sqlx::query("INSERT INTO book_snapshots(ts, market, mid) VALUES (?, 'prlusdt', '1.05')")
            .bind(now)
            .execute(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized)
             VALUES (1,'prlusdt','sell','1.06','100','106','0.106','usdt','maker',?,'0','0','0')",
        )
        .bind(now - 30_000)
        .execute(db.pool()).await.unwrap();

        assert_eq!(db.settle_markouts().await.unwrap(), 0, "30 seconds old cannot be judged");
        let r = build(&db, "prlusdt", 86_400).await.unwrap();
        assert_eq!(r.maker.first().expect("one book").scored, 0);
    }

    /// A book whose posted spread is a stale phantom must not be scored
    /// against it. prlusdc showed 1288 bp with nothing behind it, which made
    /// good quoting read as an 8% capture rate.
    #[test]
    fn an_absurd_spread_is_reported_as_unmeasured_not_as_a_miss() {
        let mut m = MakerEdge {
            market: "prlusdc".into(),
            market_volume: dec!(2390),
            market_spread: dec!(0.1288),
            fills: 10,
            volume_base: dec!(1000),
            volume_quote: dec!(1290),
            edge_quote: dec!(13),
            markout_quote: dec!(2),
            scored: 10,
            fees: dec!(1.29),
            maker_fills: 10,
        };
        assert_eq!(m.sane_spread_bps(), Decimal::ZERO, "1288 bp is not a crossable spread");
        assert!(m.capture().is_none(), "must not score against a phantom spread");

        m.market_spread = dec!(0.013);
        assert_eq!(m.sane_spread_bps(), dec!(130));
        assert!(m.capture().expect("a real spread scores").round_dp(0) > Decimal::ZERO);
    }

    #[test]
    fn bar_renders_proportionally() {
        assert_eq!(bar(dec!(0), 4), "░░░░");
        assert_eq!(bar(dec!(1), 4), "████");
        assert_eq!(bar(dec!(0.5), 4), "██░░");
        assert_eq!(bar(dec!(5), 4), "████", "over 100% must not overflow the bar");
        assert_eq!(bar(dec!(-1), 4), "░░░░", "negative must not underflow");
    }

    #[test]
    fn impact_percentages() {
        let i = Impact { first_mid: dec!(1.0), last_mid: dec!(1.1), low: dec!(0.9), high: dec!(1.2) };
        assert_eq!(i.drift_pct().round_dp(2), dec!(10.00));
        assert_eq!(i.range_pct().round_dp(2), dec!(33.33));
    }

    /// The CYCLE section must say which routes are open, which are shut, and
    /// never offer a route that cannot be traded.
    #[test]
    fn the_cycle_section_reports_open_shut_and_no_impossible_route() {
        use crate::exchange::Side;
        use crate::strategy::cycle;
        let leg = |m: &str, side, net, se, n| cycle::Leg {
            market: m.into(),
            side,
            net_bps: net,
            se_bps: se,
            volume: dec!(1000),
            n,
        };
        let pair = |v: &[(&str, &str)]| -> std::collections::HashMap<String, String> {
            v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
        };
        let c = cycle::estimate(
            "prlusdc",
            vec![
                leg("prlusdc", Side::Buy, dec!(85), dec!(24), 90),
                leg("prlusdc", Side::Sell, dec!(209), dec!(19), 80),
                leg("prlusdt", Side::Sell, dec!(-17), dec!(38), 109),
                // A sell leg on the stablecoin book: it has fills and a quote
                // currency, so only the base-asset check keeps it from being
                // offered as somewhere to release PRL.
                leg("usdcusdt", Side::Sell, dec!(5), dec!(6), 4),
            ],
            pair(&[("prlusdc", "usdc"), ("prlusdt", "usdt"), ("usdcusdt", "usdt")]),
            pair(&[("prlusdc", "prl"), ("prlusdt", "prl"), ("usdcusdt", "usdc")]),
            dec!(24),
        );
        let out = render_cycle(&c, dec!(20));

        // Every leg is listed, with the acquiring one marked.
        assert!(out.contains("*prlusdc"), "the acquire leg is marked:\n{out}");
        assert!(out.contains("+209"), "{out}");
        assert!(out.contains("-17"), "{out}");

        // The thin book's round trip is open; the deep book's is shut.
        let open_line = out.lines().find(|l| l.starts_with("prlusdc ") && l.contains("OPEN"));
        assert!(open_line.is_some(), "prlusdc must read OPEN:\n{out}");
        let shut_line = out.lines().find(|l| l.starts_with("prlusdt ")).unwrap_or("");
        assert!(shut_line.contains("shut"), "prlusdt must read shut: {shut_line}");
        assert!(shut_line.contains("+swap"), "and be charged the swap: {shut_line}");

        // And the stablecoin book is never offered as a release venue: it
        // cannot trade PRL, whatever its own numbers look like.
        let release_part = out.split("release").nth(1).unwrap_or("");
        assert!(
            !release_part.contains("usdcusdt"),
            "a route that cannot be traded must not be offered:\n{out}"
        );

        // The floor the engine applies is shown, so the two cannot drift.
        assert!(out.contains("floor"), "{out}");
        assert!(out.contains("20 bp to act"), "{out}");

        // With nothing measured the section says so rather than implying zero.
        let empty = cycle::estimate("prlusdc", vec![], pair(&[]), pair(&[]), dec!(24));
        assert!(render_cycle(&empty, dec!(20)).contains("no scored fills yet"));
    }

    #[test]
    fn impact_handles_zero_baseline() {
        let i = Impact { first_mid: dec!(0), last_mid: dec!(1), low: dec!(0), high: dec!(1) };
        assert_eq!(i.drift_pct(), dec!(0));
        assert_eq!(i.range_pct(), dec!(0));
    }
}

/// A short status for Telegram.
///
/// Deliberately not the terminal report. The question someone asks on a phone
/// is "what do I hold, is the bot making money, and what is it doing" -- three
/// answers, not thirty numbers. `report` still prints everything.
///
/// The one distinction worth keeping is between what the BOT earned and what
/// the MARKET did. Folding PRL's price move into "profit" would flatter the bot
/// enormously in a rally and damn it in a selloff, when in neither case did it
/// have much to do with it.
/// The glance: what the book earned, and what that projects to. Nothing else.
///
/// This is the hourly message and the `status` reply. Holdings, the quoting
/// breakdown, orders and the full report sit behind their own buttons; a
/// status screen that answered every question answered the one that matters
/// -- "is it making money, and how much" -- a screen and a half down.
pub fn render_earnings(r: &Report) -> String {
    let setting = |k: &str| r.overrides.iter().find(|(ck, _)| ck == k).map(|(_, v)| v.as_str());
    let mut o = String::new();
    if setting("enabled") == Some("false") {
        o.push_str("<b>⏸ PAUSED — no quotes resting</b>\n\n");
    }
    if setting("dry_run") == Some("true") {
        o.push_str("<b>🧪 DRY RUN — nothing is sent</b>\n\n");
    }
    let Some(y) = &r.yield_ else {
        o.push_str("<b>Earnings</b>\n<pre>no book start recorded yet</pre>\n");
        return o;
    };
    let ccy = y.base_ccy.to_uppercase();
    let quote = r.weights.as_ref().map(|w| w.quote_ccy.to_uppercase()).unwrap_or_default();
    o.push_str(&format!(
        "<b>Earnings</b> · book {}\n<pre>",
        dur((crate::db::now_ms() - y.since_ms) / 1000)
    ));
    o.push_str(&format!(
        "last hour {:>+9.2} {ccy} {:>+8.2} {quote}\n",
        y.last_hour_base.round_dp(2), y.last_hour_quote.round_dp(2),
    ));
    o.push_str(&format!(
        "so far    {:>+9.2} {ccy} {:>+8.2} {quote}\n",
        y.earned_base.round_dp(2), y.earned_quote.round_dp(2),
    ));
    o.push_str(&format!("capital   {:>9.0} {ccy}\n\n", y.capital_base));
    o.push_str("projected, all data so far\n");
    o.push_str(&y.rows());
    if let Some((_, pile, _)) = &r.set_aside {
        o.push_str(&format!("\nset aside {:>9.2} {ccy}\n", pile.round_dp(2)));
    }
    o.push_str("</pre>");
    o
}

/// How a running twap is doing, for the 15-minute Telegram push: cash left,
/// what we bought and at what average, what rests now, and the last fills.
///
/// `None` when the asset has no active twap. Counts every twap in the current
/// run, not just the active one: a deposit restarts the twap as a new program,
/// and "bought" should not drop to zero when it does.
pub async fn twap_progress(db: &Db, base: &str, markets: &[String]) -> Result<Option<String>> {
    let Some(p) = db.active_program(base).await? else { return Ok(None) };
    if p.kind != ProgramKind::Twap {
        return Ok(None);
    }

    // The unbroken run of twaps on this asset, newest first.
    let recent: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, kind FROM programs WHERE market = ? ORDER BY id DESC LIMIT 50")
            .bind(base)
            .fetch_all(db.pool())
            .await?;
    let first = recent
        .iter()
        .take_while(|(_, k)| k == "twap")
        .last()
        .map(|(id, _)| *id)
        .unwrap_or(p.id);

    // The books that trade this asset, as the engine recorded them.
    let mut books = Vec::new();
    for m in markets {
        if db.get_setting(&format!("base_unit:{m}")).await?.as_deref() == Some(base) {
            books.push(m.clone());
        }
    }

    let fills: Vec<(i64, String, String, String, String, String)> = sqlx::query_as(
        "SELECT created_at, market, side, price, amount, total FROM fills
         WHERE program_id IN (SELECT id FROM programs WHERE market = ? AND kind = 'twap' AND id >= ?)
         ORDER BY created_at DESC",
    )
    .bind(base)
    .bind(first)
    .fetch_all(db.pool())
    .await?;
    let (mut qty, mut cost) = (Decimal::ZERO, Decimal::ZERO);
    for (_, _, side, _, amount, total) in &fills {
        if side == "buy" {
            qty += num::parse(amount);
            cost += num::parse(total);
        }
    }

    let usdt = held(db, "usdt").await;
    let usdc = held(db, "usdc").await;
    let b = base.to_uppercase();

    let mut o = format!("⏱ <b>TWAP #{} — {b}</b>\n", p.id);
    o.push_str(&format!("USDT left: <b>{}</b>", usdt.round_dp(2)));
    if usdc >= dec!(1) {
        o.push_str(&format!("  (USDC {})", usdc.round_dp(2)));
    }
    o.push('\n');
    o.push_str(&format!("Bought: <b>{} {b}</b> for {}\n", qty.round_dp(2), cost.round_dp(2)));
    if qty > Decimal::ZERO {
        o.push_str(&format!("Avg price: <b>{}</b>\n", (cost / qty).round_dp(4)));
    }
    let left = p.ends_at.map(|e| dur(((e - crate::db::now_ms()) / 1000).max(0)));
    o.push_str(&format!(
        "Target {} {b} · {} left · max {}\n",
        p.target_qty.round_dp(0),
        left.unwrap_or_else(|| "no deadline".into()),
        p.limit_price.map(|v| v.to_string()).unwrap_or_else(|| "none (bids only)".into()),
    ));

    let mut resting = Vec::new();
    for m in &books {
        for r in db.live_orders(m).await? {
            resting.push((m.clone(), r));
        }
    }
    o.push_str(&format!("\n<b>Outstanding orders ({})</b>\n<pre>", resting.len()));
    if resting.is_empty() {
        o.push_str("none\n");
    }
    for (m, r) in &resting {
        o.push_str(&format!(
            "{:<8} {:<4} {:>8} × {:>9}  {}\n",
            m, r.side.as_str().to_uppercase(), r.price.round_dp(4),
            (r.amount - r.filled).max(Decimal::ZERO).round_dp(2), age(r.placed_at)
        ));
    }
    o.push_str("</pre>\n");

    o.push_str("<b>Recent fills</b>\n<pre>");
    if fills.is_empty() {
        o.push_str("none yet\n");
    }
    for (t, m, side, price, amount, _) in fills.iter().take(8) {
        o.push_str(&format!(
            "{:<8} {:<4} {:>8} × {:>9}  {}\n",
            m, side.to_uppercase(), num::parse(price).round_dp(4),
            num::parse(amount).round_dp(2), age(*t)
        ));
    }
    o.push_str("</pre>");
    Ok(Some(o))
}

/// How the slow sell is doing, for `progress` and the hourly push: what is
/// left, what sold and at what average against the market's, our share of
/// the day's volume, and what rests now.
///
/// `None` when the asset has no slow sell running. Counts every sell fill on
/// the asset's books since the run started, hand-placed ones included.
pub async fn sell_progress(db: &Db, base: &str, markets: &[String]) -> Result<Option<String>> {
    let Some(p) = db.active_program(base).await? else { return Ok(None) };
    if p.kind != ProgramKind::TwapSell {
        return Ok(None);
    }
    let mut books = Vec::new();
    for m in markets {
        if db.get_setting(&format!("base_unit:{m}")).await?.as_deref() == Some(base) {
            books.push(m.clone());
        }
    }
    let now = crate::db::now_ms();
    let day_ago = now - 86_400_000;

    let (mut qty, mut proceeds, mut day_qty) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    let (mut mkt_qty, mut mkt_value, mut day_vol) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    let mut fills: Vec<(i64, String, String, String)> = Vec::new();
    for m in &books {
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT created_at, price, amount, total FROM fills
             WHERE market = ? AND side = 'sell' AND created_at >= ? ORDER BY created_at DESC",
        )
        .bind(m)
        .bind(p.started_at.min(day_ago))
        .fetch_all(db.pool())
        .await?;
        for (t, price, amount, total) in rows {
            let a = num::parse(&amount);
            if t >= p.started_at {
                qty += a;
                proceeds += num::parse(&total);
                fills.push((t, m.clone(), price, amount));
            }
            if t >= day_ago {
                day_qty += a;
            }
        }
        let tape: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT ts, price, amount FROM tape WHERE market = ? AND ts >= ?")
                .bind(m)
                .bind(p.started_at.min(day_ago))
                .fetch_all(db.pool())
                .await?;
        for (t, price, amount) in tape {
            let a = num::parse(&amount);
            if t >= p.started_at {
                mkt_qty += a;
                mkt_value += a * num::parse(&price);
            }
            if t >= day_ago {
                day_vol += a;
            }
        }
    }
    fills.sort_by(|a, b| b.0.cmp(&a.0));

    let b = base.to_uppercase();
    let mut o = format!("💸 <b>Slow sell #{} — {b}</b>\n", p.id);
    // The status line goes here, once the resting asks are known.
    let head = o.len();
    o.push_str(&format!("{b} left: <b>{}</b>\n", held(db, base).await.round_dp(0)));
    o.push_str(&format!("Sold: <b>{} {b}</b> for {}", qty.round_dp(0), proceeds.round_dp(2)));
    o.push_str(&format!(" · running {}\n", dur(((now - p.started_at) / 1000).max(0))));
    if qty > Decimal::ZERO {
        let ours = proceeds / qty;
        o.push_str(&format!("Avg price: <b>{}</b>", ours.round_dp(4)));
        if mkt_qty > Decimal::ZERO {
            let theirs = mkt_value / mkt_qty;
            o.push_str(&format!(
                " vs market {} (<b>{:+.2}%</b>)",
                theirs.round_dp(4),
                (ours - theirs) / theirs * dec!(100)
            ));
        }
        o.push('\n');
    }
    if day_vol > Decimal::ZERO {
        o.push_str(&format!(
            "Last 24h: {} of {} traded (<b>{:.2}%</b>)\n",
            day_qty.round_dp(0),
            day_vol.round_dp(0),
            day_qty / day_vol * dec!(100)
        ));
    }
    // How long what is here lasts at the cap, so the owner tops up about a
    // day at a time and never has much more than that on the exchange.
    let day_pct = db.setting_decimal("sell_day_pct", dec!(5)).await;
    let left = held(db, base).await;
    let pace = day_pct / dec!(100) * day_vol;
    let hours = db.setting_decimal("sell_hours", Decimal::ZERO).await;
    o.push_str(&format!(
        "Cap {day_pct}% of 24h volume (≈{} {b}/day){} · never below {}\n",
        pace.round_dp(0),
        if hours > Decimal::ZERO { format!(" · spread over ≥{}h", hours.normalize()) } else { String::new() },
        p.limit_price.map(|v| v.to_string()).unwrap_or_else(|| "1.30".into()),
    ));
    if pace > Decimal::ZERO {
        let hours = left / pace * dec!(24);
        o.push_str(&format!("{b} here lasts ≈{}h at the cap", hours.round_dp(0)));
        if hours < dec!(12) {
            o.push_str(&format!(" — <b>top up ≈{} {b}</b>", ((pace - left).max(Decimal::ZERO) / dec!(1000)).round() * dec!(1000)));
        }
        o.push('\n');
    }

    let mut resting = Vec::new();
    for m in &books {
        for r in db.live_orders(m).await? {
            resting.push((m.clone(), r));
        }
    }
    resting.sort_by(|a, b| a.1.price.cmp(&b.1.price));
    let total: Decimal = resting.iter().map(|(_, r)| (r.amount - r.filled).max(Decimal::ZERO)).sum();
    let status = sell_status(db, &books, &resting, left).await;
    o.insert_str(head, &format!("Now: {status}\n"));
    o.push_str(&format!("\n<b>Asks resting ({}, {} {b})</b>\n<pre>", resting.len(), total.round_dp(0)));
    if resting.is_empty() {
        o.push_str("none\n");
    }
    for (m, r) in &resting {
        o.push_str(&format!(
            "{:<8} {:<4} {:>8} × {:>9}  {}\n",
            m, r.side.as_str().to_uppercase(), r.price.round_dp(4),
            (r.amount - r.filled).max(Decimal::ZERO).round_dp(0), age(r.placed_at)
        ));
    }
    o.push_str("</pre>\n<b>Recent sells</b>\n<pre>");
    if fills.is_empty() {
        o.push_str("none yet\n");
    }
    for (t, m, price, amount) in fills.iter().take(8) {
        o.push_str(&format!(
            "{:<8} {:>8} × {:>9}  {}\n",
            m, num::parse(price).round_dp(4), num::parse(amount).round_dp(0), age(*t)
        ));
    }
    o.push_str("</pre>");
    Ok(Some(o))
}

/// One line on what the slow sell is doing right now, so "why is nothing
/// selling" has an answer on the screen (owner, 2026-10-27: "add a custom
/// button so i can see what you are doing").
async fn sell_status(db: &Db, books: &[String], resting: &[(String, crate::db::models::LiveOrder)], left: Decimal) -> String {
    if !db.setting_bool("enabled", true).await {
        return "⏸ <b>paused</b> — no orders".into();
    }
    if db.setting_bool("twap_hold", false).await {
        return "✋ <b>on hold</b> — asks pulled".into();
    }
    // The same test the engine halts on: mid this far under the hour's high.
    let crash = db.setting_decimal("twap_crash_pct", dec!(10)).await;
    if crash > Decimal::ZERO {
        for m in books {
            let mid: Option<Decimal> = sqlx::query_scalar::<_, Option<String>>(
                "SELECT mid FROM book_snapshots WHERE market = ? ORDER BY ts DESC LIMIT 1",
            )
            .bind(m)
            .fetch_optional(db.pool())
            .await
            .ok()
            .flatten()
            .flatten()
            .and_then(|s| s.parse().ok());
            if let (Some(mid), Some(high)) = (mid, db.hour_high(m).await)
                && high > Decimal::ZERO
                && (high - mid) / high * dec!(100) > crash
            {
                return format!(
                    "🛡 <b>crash guard</b> on {m} — {:.1}% under the hour's high, asks pulled until it recovers",
                    (high - mid) / high * dec!(100)
                );
            }
        }
    }
    if resting.is_empty() {
        return if left < dec!(20) {
            "waiting for a deposit".into()
        } else {
            "no asks — waiting for room under the caps, or for price over the floor".into()
        };
    }
    let lo = resting.iter().map(|(_, r)| r.price).min().unwrap_or_default();
    let hi = resting.iter().map(|(_, r)| r.price).max().unwrap_or_default();
    format!("🟢 selling — {} asks from {} to {}, waiting for buyers", resting.len(), lo.round_dp(4), hi.round_dp(4))
}

pub fn render_compact(r: &Report) -> String {
    let mut o = String::new();
    let mark = r.mark.unwrap_or_default();
    let setting = |k: &str| r.overrides.iter().find(|(ck, _)| ck == k).map(|(_, v)| v.as_str());

    // The verdict first, before anything else on the screen. The question the
    // owner is actually asking is "is the book making money", and every other
    // number here is evidence for it. Judged on everything since the epoch:
    // net markout of the scored fills over what they traded, against how
    // many episodes produced them.
    if r.episodes > 0 && r.scored_notional > Decimal::ZERO {
        o.push_str(&format!(
            "<b>{}</b>\n",
            Verdict::of(r.lifetime.trading / r.scored_notional * dec!(10000), r.episodes).headline()
        ));
    }
    // Which window the verdict and every P&L figure below are judged on, and
    // what started it. Shown even before there is a verdict: a scoreboard
    // reading nothing is a young one, and this is what says so.
    o.push_str(&format!("<i>{}</i>\n\n", scoreboard_since(r)));

    o.push_str(&format!("<b>{}</b>", r.market.to_uppercase()));
    if !mark.is_zero() {
        o.push_str(&format!("  {}", mark.round_dp(4)));
    }
    if let Some(i) = &r.impact {
        o.push_str(&format!("  ·  {:+.1}% {}", i.drift_pct(), dur(r.window_secs)));
    }
    o.push('\n');

    // Whether the bot is actually quoting is the first thing worth knowing and
    // the compact view used to omit it entirely -- so a paused bot and a
    // working one produced identical screens, and "why no fills" had no answer
    // on the screen that was meant to answer it.
    if setting("enabled") == Some("false") {
        o.push_str("<b>⏸ PAUSED — no quotes resting</b>\n");
    }
    if setting("dry_run") == Some("true") {
        o.push_str("<b>🧪 DRY RUN — nothing is sent</b>\n");
    }
    o.push('\n');

    if let Some(w) = &r.weights {
        let total = w.total();
        o.push_str("<b>Holdings</b>\n<pre>");
        for pool in &w.pools {
            let share = if total.is_zero() {
                Decimal::ZERO
            } else {
                pool.value() / total * dec!(100)
            };
            o.push_str(&format!(
                "{:<5} {:>10} {:>7} {:>3.0}%\n",
                pool.ccy.to_uppercase(), pool.qty.round_dp(2),
                pool.value().round_dp(0), share
            ));
        }
        o.push_str(&format!(
            "{:<5} {:>10} {:>7} {}\n",
            "total", "", total.round_dp(0), w.quote_ccy.to_uppercase()
        ));
        // Target beside actual. Held apart -- target in one block, holdings in
        // another -- answering "is it heading where I want it" meant reading
        // two blocks and doing the subtraction yourself.
        let now = w.base_weight() * dec!(100);
        match w.target_weight {
            Some(t) => o.push_str(&format!(
                "target {:.0}% {} · now {:.0}% ({:+.0})\n",
                t * dec!(100), w.base_ccy.to_uppercase(), now, now - t * dec!(100)
            )),
            None => o.push_str(&format!(
                "now    {:.0}% {} · no target\n",
                now, w.base_ccy.to_uppercase()
            )),
        }
        o.push_str("</pre>\n");
    }

    // The yield, in the base asset and as a return on the book: the number the
    // owner reads this message for. See `Yield`.
    if let Some(y) = &r.yield_ {
        let ccy = y.base_ccy.to_uppercase();
        o.push_str(&format!(
            "<b>Yield · last {}</b>\n<pre>",
            dur((crate::db::now_ms() - y.since_ms) / 1000)
        ));
        o.push_str(&format!(
            "earned  {:>+9.2} {ccy}\ncapital {:>9.0} {ccy}\n",
            y.earned_base.round_dp(2), y.capital_base,
        ));
        o.push_str(&y.rows());
        if let Some((_, pile, _)) = &r.set_aside {
            o.push_str(&format!("set aside {:>7.2} {ccy}\n", pile.round_dp(2)));
        }
        o.push_str("</pre>\n");
    }

    // What the QUOTING earned, and nothing else.
    //
    // The market's move is not ours and knowing it here only confuses the one
    // question this screen answers. It is still in the full report, where the
    // account's whole change has to add up.
    //
    // Every figure below is markout: the fill judged five minutes later, net of
    // the fees on the fills actually judged. The instant-of-fill number flatters
    // by construction and once showed a profit across a stretch that lost money.
    let l = &r.lifetime;
    let since = if r.epoch_ms > 0 {
        format!("last {}", dur((crate::db::now_ms() - r.epoch_ms) / 1000))
    } else {
        "since launch".into()
    };
    o.push_str(&format!("<b>Quoting · after 5 min · {since}</b>\n<pre>"));
    o.push_str(&format!("{:<10}{:>+9.2}  net\n", "", l.trading.round_dp(2)));

    // Earnings, shown only where there is something to show. A line reading
    // 0.00 for a period the bot was not running looks like a loss, not an
    // absence, so each part appears only once it means something.
    let e = &r.earnings;
    if let Some(rate) = e.per_day_since_start() {
        o.push_str(&format!("{:<10}{:>+9.2}  per day\n", "", rate.round_dp(2)));
        // Since the epoch, like the two lines above it. The window's count
        // sat here once and read "0 fills judged" under a since-epoch loss.
        o.push_str(&format!("{:<10}{:>9}  fills judged\n", "", r.scored_fills));
    }
    o.push_str("</pre>\n");
    if e.daily.len() > 1 {
        let hourly = e.daily.first().is_some_and(|(d, _)| d.len() > 10);
        o.push_str(&format!(
            "<b>By {} · after 5 min</b>\n<pre>", if hourly { "hour" } else { "day" }
        ));
        for (day, net) in e.daily.iter().take(8) {
            // Hourly rows carry the date too; it is the same date every time
            // and costs five columns on a phone.
            let label = if hourly { &day[11..] } else { &day[5..] };
            o.push_str(&format!("{:<10}{:>+9.2}\n", label, net.round_dp(2)));
        }
        o.push_str("</pre>\n");
    }

    // ---- what the quoting itself did ----
    let vol: Decimal = r.maker.iter().map(|m| m.volume_quote).sum();
    let fills: i64 = r.maker.iter().map(|m| m.fills).sum();
    let makers: i64 = r.maker.iter().map(|m| m.maker_fills).sum();
    let gross: Decimal = r.maker.iter().map(|m| m.edge_quote).sum();
    let fees: Decimal = r.maker.iter().map(|m| m.fees).sum();
    let net = gross - fees;
    let hours = Decimal::from(r.window_secs.max(1)) / dec!(3600);
    let mkt_vol: Decimal = r.maker.iter().map(|m| m.market_volume * hours).sum();

    o.push_str(&format!("<b>Trading · last {}</b>\n<pre>", dur(r.window_secs)));
    o.push_str(&format!(
        "{:<9}{:>9}  {}\n",
        "volume",
        vol.round_dp(0),
        if mkt_vol.is_zero() {
            "of market".to_string()
        } else {
            format!("{:.1}% of mkt", vol / mkt_vol * dec!(100))
        }
    ));
    o.push_str(&format!(
        "{:<9}{:>9}  {:.0}/h {}% mkr\n",
        "fills",
        fills,
        Decimal::from(fills) / hours,
        if fills == 0 { 0 } else { makers * 100 / fills }
    ));
    o.push_str(&format!("{:<9}{:>+9.2}\n", "fees", (-fees).round_dp(2)));
    // The window's P&L, judged five minutes on and net of the fees of the
    // fills actually judged. `net` from `edge` sat on this line until it was
    // showing +3,258 directly above a markout of -1,141 -- two numbers on one
    // screen disagreeing about whether the day made money.
    let scored: i64 = r.maker.iter().map(|m| m.scored).sum();
    if scored > 0 {
        let mo: Decimal = r.maker.iter().map(|m| m.markout_quote).sum();
        let mo_fees: Decimal = r
            .maker
            .iter()
            .filter(|m| m.fills > 0)
            .map(|m| m.fees * Decimal::from(m.scored) / Decimal::from(m.fills))
            .sum();
        o.push_str(&format!(
            "{:<9}{:>+9.2}  after 5 min\n", "net", (mo - mo_fees).round_dp(2)
        ));
    } else {
        o.push_str(&format!("{:<9}{:>9}  not judged yet\n", "net", "—"));
    }
    o.push_str("</pre>\n");

    // Edge per unit traded: the size-independent view. Absolute profit says
    // how much we did; this says how well, and it is the only pair of numbers
    // that shows whether a quiet day was bad quoting or just a quiet market.
    if !vol.is_zero() {
        let spread: Decimal = weighted(r, |m| m.sane_spread_bps());
        let net_bps = net / vol * dec!(10000);
        let gross_bps = gross / vol * dec!(10000);
        o.push_str("<b>Edge per unit traded</b>\n<pre>");
        if spread > Decimal::ZERO {
            o.push_str(&format!("{:<10}{:>7.0} bp  on offer\n", "spread", spread));
        }
        o.push_str(&format!("{:<10}{:>7.0} bp  at the fill\n", "our edge", gross_bps));
        o.push_str(&format!("{:<10}{:>7.0} bp  after fees\n", "kept", net_bps));
        if spread > Decimal::ZERO {
            o.push_str(&format!(
                "{:<10}{:>7.0} %   of spread\n",
                "capture", net_bps / spread * dec!(100)
            ));
        }

        // The number that actually says whether the quoting earns.
        //
        // Everything above compares our price to the mid at the instant of the
        // fill, which we always beat -- we quote outside the mid. It says
        // nothing about the fill that happened BECAUSE the price was about to
        // run through it. This does, and where it is far below "our edge" we
        // are being picked off rather than paid.
        let scored: i64 = r.maker.iter().map(|m| m.scored).sum();
        if scored > 0 {
            let mo: Decimal = r.maker.iter().map(|m| m.markout_quote).sum();
            let scored_vol: Decimal = r
                .maker
                .iter()
                .filter(|m| m.scored > 0)
                .map(|m| m.volume_quote * Decimal::from(m.scored) / Decimal::from(m.fills.max(1)))
                .sum();
            let mo_bps = if scored_vol.is_zero() {
                Decimal::ZERO
            } else {
                mo / scored_vol * dec!(10000)
            };
            o.push_str(&format!("{:<10}{:>7.0} bp  where it went\n", "after 5m", mo_bps));
            o.push_str(&format!(
                "{:<10}{:>+7.0}     {scored} fills\n", "= actual", mo.round_dp(0)
            ));
        }
        if r.maker.len() > 1 {
            o.push_str(&format!(
                "\n{:<9}{:>6}{:>6}{:>6}\n", "book", "share", "edge", "kept"
            ));
            for m in &r.maker {
                o.push_str(&format!(
                    "{:<9}{:>5.1}%{:>5.0}b{:>5.0}b\n",
                    m.market, m.share(r.window_secs), m.edge_bps(), m.net_bps()
                ));
            }
        }
        o.push_str("</pre>\n");
    }

    // What every arm has paid and the size each side is quoting because of
    // it. Shown on its own, not inside the trading block: an arm at the
    // minimum may have no fills in the window at all, and that is exactly
    // when it must be seen. One row per bucket, regime and width, under the
    // market's own line, 36 columns:
    //     200 calm  3+ +18.2  9.4  46   0% 100%
    if !r.arms.is_empty() {
        o.push_str("<b>Arms · ‰ per $-hour</b>\n<pre>");
        o.push_str(&arm_table(&r.arms));
        o.push_str("</pre>\n");
    }

    if !r.spare.is_empty() {
        o.push_str("<b>Spare to withdraw</b>\n<pre>");
        for (ccy, qty) in &r.spare {
            o.push_str(&format!("{:<6}{:>12}\n", ccy, qty.round_dp(2)));
        }
        o.push_str("keeps the ladder funded\n</pre>\n");
    }

    match &r.program {
        Some(pr) => {
            o.push_str(&format!("<b>Now · {}</b>", pr.kind));
            if let Some(w) = &r.weights
                && let Some(t) = w.target_weight
            {
                o.push_str(&format!(" {:.0}%", t * dec!(100)));
            }
            o.push_str("\n<pre>");
            o.push_str(&format!(
                "{:<9}{} → {}\n",
                "holding", pr.current.round_dp(0), pr.target_qty.round_dp(0)
            ));
            o.push_str(&format!(
                "{:<9}{}  {:.0}%\n", "filled", bar(pr.progress, 8), pr.progress * dec!(100)
            ));
            if let Some(sp) = pr.schedule_progress {
                let gap = pr.progress - sp;
                let pace = if gap.abs() < dec!(0.05) {
                    "on pace"
                } else if gap > Decimal::ZERO {
                    "ahead"
                } else {
                    "behind"
                };
                o.push_str(&format!(
                    "{:<9}{}  {:.0}%  {}\n", "time", bar(sp, 8), sp * dec!(100), pace
                ));
            }
            o.push_str(&format!("{:<9}{} resting\n", "orders", r.open_orders.len()));
            if let Some((t, side, price, amount)) = r.recent_fills.first() {
                o.push_str(&format!(
                    "{:<9}{} {} @{} {} ago\n",
                    "last", side, amount.round_dp(2), price.round_dp(4), age(*t)
                ));
            }
            o.push_str("</pre>");
        }
        None => o.push_str("<b>Now · idle</b>  no program running\n"),
    }

    // Only warnings from the last hour. Counting the whole window meant a
    // problem fixed hours ago kept flashing a warning badge, which trains you
    // to ignore the badge -- precisely when you need to notice it.
    let recent = crate::db::now_ms() - 3_600_000;
    let fresh = r.errors.iter().filter(|(ts, _, _)| *ts >= recent).count();
    if fresh > 0 {
        o.push_str(&format!(
            "\n⚠️ {fresh} warning(s) in the last hour — send <code>report</code>\n"
        ));
    }
    o
}

/// Average a per-book measure across books, weighted by what we traded on each.
///
/// A plain mean would let a book we barely touch drag the headline number
/// around; only books we actually quoted into should move it. Books with no
/// measurement (a zero) are left out rather than averaged in as zero.
fn weighted(r: &Report, f: impl Fn(&MakerEdge) -> Decimal) -> Decimal {
    let mut num = Decimal::ZERO;
    let mut den = Decimal::ZERO;
    for m in &r.maker {
        let v = f(m);
        if v > Decimal::ZERO && m.volume_quote > Decimal::ZERO {
            num += v * m.volume_quote;
            den += m.volume_quote;
        }
    }
    if den.is_zero() { Decimal::ZERO } else { num / den }
}
