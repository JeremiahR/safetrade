//! Configuration.
//!
//! Split deliberately in two:
//!   * `config.toml` — structural things you restart to change (market, db path,
//!     loop cadence, hard risk limits).
//!   * the `settings` table — the knobs you turn on a running bot via `ctl set`.
//!
//! `Params::load` merges them, DB winning, so the file stays the documented
//! default and the DB records every live override with a timestamp.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use crate::db::Db;

/// The whole config file.
///
/// `deny_unknown_fields` alongside `default` is deliberate and was bought the
/// hard way. Every `[quoting]` key for the cycle -- `cycle_release`,
/// `cycle_recycle`, `cycle_floor_bps` and the rest -- had been written at the
/// TOP level of `config.prod.toml`, above the `[quoting]` header. With only
/// `default`, serde discarded all of them without a word and the struct took
/// its defaults: the file said `cycle_release = true` and the running bot read
/// `false`, for as long as the file had existed. A misplaced key is now a
/// start-up error, which is the only way a typo in a config that moves money
/// should ever present itself.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Primary market. Kept for single-market configs and as the default
    /// program key's source.
    pub market: String,
    /// Every market to quote. Empty means just `market`.
    ///
    /// All markets quoting the same base asset share ONE inventory target, so
    /// the bot is running one position across several books rather than
    /// several independent bots that happen to trade the same coin.
    pub markets: Vec<String>,
    /// Per-market overrides, keyed by market id. A thin illiquid book wants
    /// different sizing and a wider spread than a deep one.
    pub overrides: HashMap<String, MarketOverride>,
    pub db_path: PathBuf,
    /// Start selling a deposit of the base asset as soon as it is credited.
    ///
    /// The owner deposits PRL in order to sell it; doing that by hand means
    /// watching a chart. Off by default, because a bot that starts selling
    /// on its own is not something to inherit by surprise.
    pub auto_trim_deposits: bool,
    /// How long a deposit is worked off over. The clock PAUSES whenever price
    /// is under `auto_trim_floor`, so this is a floor on the duration, not a
    /// promise.
    pub auto_trim_secs: i64,
    /// Never sell below this while working off a deposit.
    pub auto_trim_floor: Option<Decimal>,
    /// How much of the base asset to KEEP when working a deposit off.
    ///
    /// A deposit is not necessarily all for sale: this is the size of the
    /// position the bot is meant to make a market around, so the distribution
    /// glides down to it rather than to zero. `None` sells the whole deposit.
    pub auto_trim_keep: Option<Decimal>,
    /// Keep quoting when no program is active, by standing one up at the
    /// current inventory.
    ///
    /// Without it, "no active program" is a halt: every order cancelled and the
    /// book abandoned until a human types something. Market making around what
    /// we already hold changes no position and is what the bot is for.
    pub always_quote: bool,
    /// How much a good price speeds the glide up.
    ///
    /// The multiplier per unit of price above the floor: at 5, a price 10%
    /// over runs the clock at 1.5x, so a strong market front-loads the sale
    /// and a weak one waits it out. 0 disables the speed-up; the pause below
    /// the floor still applies.
    pub trim_speed_gain: Decimal,
    /// Ceiling on that speed-up, so one spike cannot finish the whole
    /// distribution in an afternoon.
    pub trim_speed_max: Decimal,
    /// Seconds between quote refreshes.
    ///
    /// Four API calls go out per loop, so this sets the request rate.
    /// Cloudflare fronts this exchange and starts returning bare 403s when
    /// pushed, costing a whole loop each time; the strategy quotes passively
    /// and its orders rest for minutes, so polling harder buys nothing.
    pub loop_secs: u64,
    pub api_base: Option<String>,
    /// Lighter's market id for the PRL perpetual, to record its book and tape
    /// beside ours (`lighter:<id>`). Read-only; unset records nothing.
    pub lighter_market_id: Option<u32>,
    /// Depth levels to request. Enough to measure the band we care about.
    pub depth_levels: u32,
    /// Days of book snapshots and events to retain.
    pub retain_days: i64,
    /// Seconds between unprompted Telegram status messages.
    pub telegram_status_secs: u64,
    /// Keep trading profit in the base asset rather than splitting it by the
    /// ratio.
    ///
    /// This is the switch between "grow the account" and "grow the coin pile".
    /// Spread capture is the same either way -- a round trip that earns 1%
    /// earns 1% whichever unit it is kept in -- so this changes only where the
    /// winnings land, not how they are made.
    pub bank_profit_in_base: bool,
    /// Smallest amount worth banking, in quote terms. Keeps the target from
    /// twitching on fractions of a cent.
    pub bank_min_chunk: Decimal,
    /// Bank on a clock instead of by chunk: every this many seconds, whatever
    /// has been earned, however small. 0 = bank by `bank_min_chunk`.
    pub bank_every_secs: u64,
    /// Also set banked profit ASIDE, in the base asset: it joins the
    /// never-sell reserve (`reserve:<asset>`), so the book stays the size it
    /// was funded at and the winnings pile up where nothing will sell them.
    pub bank_set_aside: bool,
    pub quoting: Quoting,
    pub risk: Risk,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            market: "prlusdt".into(),
            markets: vec![],
            overrides: HashMap::new(),
            db_path: PathBuf::from("data/safetrade.db"),
            auto_trim_deposits: false,
            auto_trim_secs: 7 * 86_400,
            auto_trim_floor: None,
            auto_trim_keep: None,
            always_quote: false,
            trim_speed_gain: dec!(5),
            trim_speed_max: dec!(3),
            loop_secs: 6,
            api_base: None,
            lighter_market_id: None,
            depth_levels: 50,
            retain_days: 30,
            telegram_status_secs: 3600,
            bank_profit_in_base: false,
            bank_min_chunk: dec!(5),
            bank_every_secs: 0,
            bank_set_aside: false,
            quoting: Quoting::default(),
            risk: Risk::default(),
        }
    }
}

/// Per-market quoting overrides. Anything left unset falls back to `[quoting]`.
///
/// Unknown keys are rejected rather than ignored. Serde's default is to drop
/// what it does not recognise, so a setting that exists only in the operator's
/// head -- or a typo -- would sit in the file looking effective while doing
/// nothing at all.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketOverride {
    pub size_base: Option<Decimal>,
    /// Rung ceiling as a share of equity, for this book alone.
    ///
    /// The stablecoin pair was sized with a fixed `size_base = 60`, which was
    /// right for the account it was tuned on and silently wrong the day much
    /// of the PRL went to cold storage. A share of equity is right at every size.
    pub size_pct_of_equity: Option<Decimal>,
    /// Single-order cap as a share of equity, for this book alone.
    pub max_order_pct_of_equity: Option<Decimal>,
    pub levels: Option<u32>,
    pub min_half_spread_ticks: Option<Decimal>,
    pub vol_spread_share: Option<Decimal>,
    pub max_half_spread_pct: Option<Decimal>,
    pub level_step_ticks: Option<Decimal>,
    pub max_order_notional: Option<Decimal>,
    pub max_order_pct_of_depth: Option<Decimal>,
    /// Let THIS book be crossed. Global `allow_taker` stays off.
    ///
    /// Crossing is how a price gets moved, which is why the global is false and
    /// should stay false: on the PRL books one tick is about 1% and taking is
    /// precisely the thing this bot exists not to do. The stablecoin book is
    /// the one exception -- $1.05M of asks and $2.16M of bids against a 29 bp
    /// spread, a mid that has held inside 0.9981-1.0041, and a size we would
    /// cross that is a rounding error against that depth. Nothing we can do there moves a price, and
    /// waiting for a maker fill on it is what stalls the cycle.
    pub allow_taker: Option<bool>,
    /// How far from the touch depth is counted, for this book alone.
    ///
    /// The global 5% was written for a book whose spread is one tick. On
    /// prlusdc the spread runs 2-15% (median 6.5%), so a 5% band around the
    /// touch measures almost none of the book, the depth cap then binds on a
    /// number that is mostly noise, and the side with the thinner quoted
    /// depth -- the asks, $398 against $10,699 of bids -- gets throttled to a
    /// quarter of the other side's size. A market maker round-trips at the
    /// rate of its SMALLER side, so that cap was the ceiling on the one book
    /// that pays.
    pub depth_band: Option<Decimal>,
    pub max_improve_ticks: Option<i64>,
    /// Make this book a cycle LEG only: no standing ladder of its own.
    ///
    /// See `Quoting::cycle_leg_only`. This is where it is meant to be set --
    /// it is a statement about one book's role, never about every book.
    pub cycle_leg_only: Option<bool>,
    /// Our maximum share of this book's volume. 1.0 disables the check.
    ///
    /// The cap exists to keep a large order from becoming the whole tape on a
    /// liquid book. On a book nobody else quotes, being most of the volume is
    /// the business, not a warning -- so it belongs per market.
    pub max_participation: Option<Decimal>,
    /// Quote this market only every Nth loop.
    ///
    /// A book that trades a hundred times a day does not need re-quoting every
    /// six seconds, and every market costs API calls that the busy book needs
    /// more. 1 means every loop.
    pub every_n_loops: Option<u64>,
    /// Far-out rungs on this market. 0 turns them off for a book whose own
    /// spread is already wider than the deep ladder's offsets.
    pub deep_levels: Option<u32>,
    pub deep_size: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quoting {
    /// Rungs per side. Level 0 is at or near the touch.
    pub levels: u32,
    /// Base order size per rung, in base currency. Acts as a CEILING when
    /// `ladder_fraction` is set.
    pub size_base: Decimal,
    /// Ceiling on a rung as a share of portfolio value. 0 uses `size_base`.
    ///
    /// A fixed coin count goes stale the moment the account changes size, and
    /// then silently caps the ladder without saying so.
    pub size_pct_of_equity: Decimal,
    /// Deep rung size as a share of inventory. 0 uses `deep_size`.
    pub deep_pct_of_inventory: Decimal,
    /// Fraction of each side's spendable balance the tight ladder should
    /// deploy. 0 keeps the fixed `size_base`.
    ///
    /// A fixed size cannot be right at both ends of a rebalance. Holding 83%
    /// PRL, the bid ladder is limited by whatever USDT we happen to have and
    /// supports a rung about a third of what the same account supports at the
    /// 50/50 target. Sizing the ladder to the money actually available makes
    /// it track that by itself, and stops deeper rungs being silently starved
    /// when the nominal exceeds the balance.
    pub ladder_fraction: Decimal,
    /// Gap between rungs, in ticks. On PRL/USDT one tick is 1%, so 1 is a wide
    /// ladder already.
    pub level_step_ticks: Decimal,
    /// Size multiplier per rung further out.
    pub level_size_growth: Decimal,
    /// Minimum half-spread from fair value, in ticks. Below this we would be
    /// quoting inside our own edge.
    pub min_half_spread_ticks: Decimal,
    /// Share of the recent high-low range to carry as half-spread.
    ///
    /// A maker is paid the spread for the risk that price runs through a
    /// resting order. When PRL ranges 20% in five minutes and we quote 0.9%
    /// wide, the spread is not paying for that risk -- which is what a negative
    /// markout means. 0 disables the widening.
    pub vol_spread_share: Decimal,
    /// Hard ceiling on the half-spread, as a fraction of fair value, so a
    /// mangled volatility reading cannot park our quotes off the book.
    pub max_half_spread_pct: Decimal,
    /// How hard inventory error pushes the reservation price, in ticks at full
    /// error. Higher = mean-reverts inventory faster, earns less spread.
    pub inventory_skew_ticks: Decimal,
    /// How hard inventory error skews *sizes*. 1.0 means at full error the
    /// wrong-way side is fully suppressed.
    pub inventory_size_skew: Decimal,
    /// Inventory error, in base currency, at which the skews above reach full
    /// strength.
    ///
    /// This is the scale that makes the skews mean anything. Normalising
    /// against `max_position` instead looks principled but is useless in
    /// practice: with a bound several times the target, being a third of the
    /// target out is an error of 0.06, which multiplied by a 1.5-tick skew moves the quote by
    /// a tenth of a tick and rounds away to nothing. The bot would then quote
    /// symmetrically while believing it was leaning.
    ///
    /// 0 means derive it from `inventory_scale_pct` of the target.
    pub inventory_scale: Decimal,
    /// Inventory error, as a share of the target, at which the skews are full.
    /// Used when `inventory_scale` is 0.
    ///
    /// The old derivation -- two ladders' worth of the rung CEILING -- came to
    /// 76% of equity once the ceiling was 8% of equity per rung, so the lean
    /// never reached a tenth of its strength: resting asks were 10% LARGER
    /// than bids while the account sat 30% under target in a rally, and the
    /// bot sold a large share of the PRL it was trying to accumulate. Ten percent
    /// of the target is the size of error a rebalance actually produces.
    pub inventory_scale_pct: Decimal,
    /// Ticks we are willing to post *better* than the current touch.
    ///
    /// This is the single most important "don't move the price" control.
    /// 0 means we only ever join the best bid/ask and never improve it, so our
    /// quoting cannot by itself move the displayed price.
    pub max_improve_ticks: i64,
    /// Cross the spread to stay on schedule. Off by default: taking liquidity
    /// is exactly how you move the price.
    pub allow_taker: bool,
    /// Cap a single order at this fraction of the quote-denominated depth
    /// resting within `depth_band` of the touch.
    pub max_order_pct_of_depth: Decimal,
    /// Band around the touch used for depth and participation measurements.
    pub depth_band: Decimal,
    /// Our share of market volume over `participation_window_secs`. When we
    /// exceed it the bot stands down until the window clears. This is the
    /// TWAP's main defence against being the whole tape.
    pub max_participation: Decimal,
    pub participation_window_secs: i64,
    /// Relative size drift tolerated before we cancel/replace. Replacing costs
    /// queue priority, so this is deliberately loose.
    pub size_tolerance: Decimal,
    /// Randomise sizes by up to this fraction, so the ladder does not look like
    /// a metronome to anyone watching the book.
    pub size_jitter: Decimal,
    /// Cap on cancel/replace pairs per loop, to stay polite to the API.
    pub max_actions_per_loop: usize,

    // ---- sizing from measured edge, per arm ----
    //
    // Every rung is a bet on an ARM -- its market, its side, and how far
    // from fair value it rests -- and each arm is sized by what its fills
    // have paid per dollar-hour of resting capital. See `strategy::arms` for
    // the derivation of each default from the fills.
    /// Half-life of the evidence, in hours. PRL went 0.98 -> 1.70 -> 1.36 in
    /// two days; at twelve hours a fill from yesterday's regime weighs a
    /// quarter of one from this morning's, and each arm still keeps about
    /// 200 effective episodes.
    ///
    /// This is the only setting the sizer has. Each arm quotes the
    /// posterior's net odds that it pays, from nothing (the exchange
    /// minimum order, which is all a fill needs to be evidence) to full
    /// size, with no probe share, no threshold on fills and no target yield
    /// to type: those were round numbers standing in for a confidence the
    /// estimator already computes.
    pub edge_half_life_hours: Decimal,
    /// Rest the exchange minimum on an arm the evidence has sized to nothing.
    ///
    /// On when the cheapest possible fill is worth more than it costs: what a
    /// fill teaches does not depend on its size, so the minimum order is a
    /// whole exploration budget for pennies. Off when the owner would rather
    /// spend nothing at all on a losing arm -- the `tape` table records every
    /// public trade with its aggressor side, so what those arms WOULD have
    /// done is still measurable offline, without placing anything.
    pub probe_dead_arms: bool,

    // ---- the cycle ----
    //
    // See `strategy::cycle`. The arms size each leg on its own yield per
    // dollar-hour, which is right for a leg that stands alone and wrong for one
    // that only exists to unlock another. These govern the second case and
    // nothing else; the SIZE of every rung still comes from the arms.
    /// The book the cycle acquires the base asset on, cheaply. Empty means
    /// "the referenced book that trades the primary market's base asset",
    /// which is where the acquisition edge lives by construction.
    pub cycle_acquire: Option<String>,
    /// Basis points a whole cycle must pay, after every leg's error is
    /// charged, before a leg is rested purely because the cycle wants it.
    ///
    /// Not zero: a cycle that pays 5 bp is inside the noise of the figures
    /// that produced it, and finding that out costs real fees and real queue
    /// priority.
    pub cycle_floor_bps: Decimal,
    /// Let the cycle rest a leg the arms have defunded, when the whole cycle
    /// clears `cycle_floor_bps`. Off is the old behaviour exactly.
    pub cycle_release: bool,
    /// Cross the recycle book to refill the currency the acquiring book's bids
    /// spend, when the cycle pays for the crossing. Needs that book's own
    /// `allow_taker`; the global stays off.
    pub cycle_recycle: bool,
    /// Ceiling on one recycle swap, as a share of equity. A swap is a taker
    /// order and this is the only thing bounding how much can be crossed in
    /// one go, so it is deliberately small.
    pub cycle_recycle_max_pct: Decimal,
    /// Refill only when the acquiring book's bids are short of this share of
    /// what they need at full size. Topping up a nearly-full purse spends the
    /// crossing fee to buy nothing.
    pub cycle_recycle_trigger: Decimal,
    /// Quote the acquiring book's two sides at NEUTRAL size, ignoring how far
    /// inventory sits from the program's target.
    ///
    /// The inventory skew exists to make a lopsided position converge, and it
    /// does that by suppressing the side that would move away from target --
    /// `ask_mult = 1 - skew * err`, zero at full error. That is right for a
    /// book being used to change a position and wrong for the one book whose
    /// job is to round-trip: buying and selling the same coin there is churn,
    /// not drift, and the position it ends the day with is the same either way.
    ///
    /// Measured 2026-09-27, this was the binding constraint on the whole
    /// account. The balanced program wanted 87% PRL against 79% held, so
    /// `err = -0.915` at `inventory_size_skew = 1.0`, and prlusdc's asks rested
    /// at 8.5% of full size -- on the leg measured at +152 bp, the best of the
    /// four. A market maker round-trips at the rate of its smaller side, so the
    /// earning book was running at a twelfth of its capacity to express a
    /// directional view that belongs on the deep book instead.
    ///
    /// Gated on the acquiring book's round trip clearing `cycle_floor_bps` on
    /// the discounted evidence, so an exemption from the safety rule is only
    /// ever granted to a round trip that has earned it. Off is the old
    /// behaviour exactly.
    pub cycle_churn: bool,
    /// The most, in basis points over fair value, a crossing order may pay.
    ///
    /// The ceiling on the taker leg (`Engine::take_if_behind`), on top of the
    /// program's own `limit_price`. Fair value on a referenced book comes from
    /// the DEEP book, so this is what stops a crossing order paying the thin
    /// book's 2-15% spread to fix a schedule: prlusdc's ask can sit 6% over
    /// what PRL is worth, and no schedule is worth 600 bp.
    pub taker_max_bps: Decimal,
    /// TWAP mode: the only thing quoting is a TWAP program.
    ///
    /// A book whose asset has no `ProgramKind::Twap` running stands down
    /// entirely -- no ladder, no release valve, no recycle crossing. Without
    /// it, "just work this size" still left the other books making markets
    /// with the same money, and the stablecoin book crossing the spread to
    /// rebalance a split nobody had asked it to hold.
    ///
    /// Set for you by `ctl twap` and cleared by any other program command, so
    /// the mode cannot outlive the program that wanted it.
    pub twap_only: bool,
    /// This book is a cycle LEG, not a book we make markets on.
    ///
    /// Per-market. The arms size each leg on its own yield and prlusdt's are
    /// long since defunded, but "defunded" still means the exchange minimum on
    /// every rung and a stream of cancel/replace on both sides. Measured over
    /// 3.7 days its buy leg pays -15 to -22 bp and its sell leg -17, so the
    /// standing ladder there is a slow, certain cost whose only purpose was to
    /// keep the arms' estimates fresh.
    ///
    /// With this set the book quotes for two reasons and no others:
    ///   * the cycle's release valve wants its ask (`cycle_release_floor`),
    ///     which is the overflow case `strategy::cycle` exists to fund; and
    ///   * an EXECUTION program is running on the base asset, because a
    ///     quantity the owner asked for is not a bet on the spread and this
    ///     deep book -- 160,000/h against the thin book's 3,300 -- is exactly
    ///     where size should be worked.
    ///
    /// Otherwise it rests nothing, and the currency it would have tied up is
    /// free for the book that pays.
    pub cycle_leg_only: bool,

    // ---- deep rungs ----
    //
    // A second, far-out ladder that rarely fills. The tight ladder earns the
    // spread; these earn the dislocations it is too close to catch -- the
    // wick that trades 8% through the book and retraces. Different income,
    // same machinery.
    /// Extra rungs beyond the main ladder. 0 disables them.
    pub deep_levels: u32,
    /// Distance of the FIRST deep rung from fair value, in ticks, and the gap
    /// between each one after it.
    pub deep_start_ticks: Decimal,
    pub deep_step_ticks: Decimal,
    /// Size of each deep rung, in base currency.
    pub deep_size: Decimal,
    /// Size multiplier per deep rung further out. Deep rungs only fill on a
    /// wick, and a wick that reaches the far rung is a better price, so the
    /// far rung should be the bigger one.
    pub deep_size_growth: Decimal,
    /// Price multiple between deep rungs. Above 1, each rung after the first
    /// sits this much further from fair value than the last -- 1.2 puts them
    /// at 1.2x, 1.44x, 1.73x. At or below 1 the old tick spacing applies.
    pub deep_price_step: Decimal,
    /// How much bigger a deep rung may be than the per-order caps allow.
    ///
    /// The single-order caps stop one order moving the market. A rung resting
    /// far above the touch cannot move it -- it only trades if somebody sweeps
    /// through it -- and capping those rungs to the same NOTIONAL as a
    /// touch order made the ladder shrink in coins as it climbed in price,
    /// which is backwards for a sweep catcher.
    pub deep_cap_multiple: Decimal,
    /// How often the ladder is allowed to change shape, in seconds.
    ///
    /// The jitter is seeded from this window, so quotes are identical within
    /// it and the reconciler leaves them resting; they re-roll together when
    /// it rolls. Without it, fresh jitter every loop changed every size every
    /// loop and cancel-replaced the book to look less robotic.
    pub refresh_secs: i64,
    /// Share of free balance the deep ladder may commit. The tight ladder is
    /// quoted first and keeps the rest.
    pub deep_allocation: Decimal,
}

impl Default for Quoting {
    fn default() -> Self {
        Quoting {
            levels: 3,
            size_base: dec!(2000),
            ladder_fraction: dec!(0.55),
            size_pct_of_equity: dec!(0.08),
            deep_pct_of_inventory: dec!(0.03),
            level_step_ticks: dec!(1),
            level_size_growth: dec!(1.5),
            min_half_spread_ticks: dec!(0.5),
            vol_spread_share: dec!(0.10),
            max_half_spread_pct: dec!(0.05),
            inventory_skew_ticks: dec!(1.5),
            inventory_size_skew: dec!(0.8),
            inventory_scale: dec!(0),
            inventory_scale_pct: dec!(0.10),
            max_improve_ticks: 0,
            allow_taker: false,
            max_order_pct_of_depth: dec!(0.15),
            depth_band: dec!(0.05),
            max_participation: dec!(0.15),
            participation_window_secs: 900,
            size_tolerance: dec!(0.25),
            size_jitter: dec!(0.12),
            max_actions_per_loop: 8,
            edge_half_life_hours: dec!(12),
            probe_dead_arms: true,
            cycle_acquire: None,
            cycle_floor_bps: crate::strategy::cycle::DEFAULT_FLOOR_BPS,
            cycle_release: false,
            cycle_recycle: false,
            cycle_recycle_max_pct: dec!(0.05),
            cycle_recycle_trigger: dec!(0.6),
            cycle_churn: false,
            twap_only: false,
            taker_max_bps: dec!(100),
            cycle_leg_only: false,
            deep_levels: 0,
            deep_start_ticks: dec!(5),
            deep_step_ticks: dec!(5),
            deep_size: dec!(400),
            deep_size_growth: dec!(1.4),
            deep_price_step: dec!(1),
            deep_cap_multiple: dec!(1),
            refresh_secs: 900,
            deep_allocation: dec!(0.20),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Risk {
    /// Hard inventory bounds in base currency, used only when the dynamic
    /// bounds below are switched off.
    pub max_position: Decimal,
    pub min_position: Decimal,
    /// Upper inventory bound as a multiple of the program's target. 0 falls
    /// back to the fixed `max_position`.
    ///
    /// A fixed bound is the wrong shape: it has to be re-set by hand after
    /// every deposit, and if it is not, it silently stops the bot BIDDING --
    /// so the bot sells one way and harvests no volatility at all. A multiple
    /// of what we are actually trying to hold scales by itself and always
    /// leaves room to buy, since the balanced target is at most half the
    /// holding and three times that is always above it.
    pub max_position_factor: Decimal,
    /// Single-order cap as a share of portfolio value. 0 uses the fixed one.
    pub max_order_pct_of_equity: Decimal,
    /// Refuse to quote outside this absolute price band. A blunt but effective
    /// guard against quoting into a flash move or a bad data read.
    pub price_floor: Option<Decimal>,
    pub price_ceiling: Option<Decimal>,
    /// Pause quoting if fair value moves more than this fraction within
    /// `vol_window_secs`.
    pub max_move: Decimal,
    pub vol_window_secs: i64,
    /// Never let a single order exceed this notional in quote currency.
    pub max_order_notional: Decimal,
    /// Leave this much quote/base untouched as a buffer.
    pub reserve_quote: Decimal,
    pub reserve_base: Decimal,
}

impl Default for Risk {
    fn default() -> Self {
        Risk {
            max_position: dec!(100000),
            min_position: dec!(0),
            max_position_factor: dec!(3),
            max_order_pct_of_equity: dec!(0.03),
            price_floor: None,
            price_ceiling: None,
            max_move: dec!(0.15),
            vol_window_secs: 300,
            max_order_notional: dec!(500),
            reserve_quote: dec!(0),
            reserve_base: dec!(0),
        }
    }
}

impl Config {
    /// Every market to quote, primary first.
    pub fn market_list(&self) -> Vec<String> {
        if self.markets.is_empty() {
            vec![self.market.clone()]
        } else {
            self.markets.clone()
        }
    }

    pub fn load(path: &std::path::Path) -> Result<Config> {
        if !path.exists() {
            anyhow::bail!(
                "no config at {} — run `safetrade init-config {}` to write a commented default",
                path.display(),
                path.display()
            );
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Credentials come from the environment only, never the config file, so the
/// file can be committed and the secrets cannot.
pub struct Credentials {
    pub key: Option<String>,
    pub secret: Option<String>,
}

impl Credentials {
    pub fn from_env() -> Credentials {
        Credentials {
            key: std::env::var("SAFETRADE_API_KEY").ok().filter(|s| !s.is_empty()),
            secret: std::env::var("SAFETRADE_API_SECRET").ok().filter(|s| !s.is_empty()),
        }
    }
}

impl Params {
    /// Apply a market's overrides on top of the shared settings.
    ///
    /// Returns a `Params` for that book alone, so a wide illiquid market can be
    /// quoted differently from a tight busy one without either affecting the
    /// other's risk limits.
    pub fn for_market(&self, ov: Option<&MarketOverride>) -> Params {
        let mut p = self.clone();
        let Some(ov) = ov else { return p };
        if let Some(v) = ov.size_base {
            p.q.size_base = v;
            // An explicit per-market size is a decision, so stop the
            // account-proportional rule from overwriting it downstream. Without
            // this, usdcusdt's size_base of 60 was replaced by 8% of equity --
            // an order dozens of times that on a stablecoin pair -- and only the notional
            // cap stood between that and the book.
            p.q.size_pct_of_equity = Decimal::ZERO;
        }
        if let Some(v) = ov.size_pct_of_equity {
            p.q.size_pct_of_equity = v;
        }
        if let Some(v) = ov.max_order_pct_of_equity {
            p.risk.max_order_pct_of_equity = v;
        }
        if let Some(v) = ov.levels {
            p.q.levels = v;
        }
        if let Some(v) = ov.min_half_spread_ticks {
            p.q.min_half_spread_ticks = v;
        }
        if let Some(v) = ov.vol_spread_share {
            p.q.vol_spread_share = v;
        }
        if let Some(v) = ov.max_half_spread_pct {
            p.q.max_half_spread_pct = v;
        }
        if let Some(v) = ov.level_step_ticks {
            p.q.level_step_ticks = v;
        }
        if let Some(v) = ov.max_order_notional {
            p.risk.max_order_notional = v;
            // An explicit per-market cap means exactly that, so the share-based
            // one must not quietly override it.
            p.risk.max_order_pct_of_equity = Decimal::ZERO;
        }
        if let Some(v) = ov.max_order_pct_of_depth {
            p.q.max_order_pct_of_depth = v;
        }
        if let Some(v) = ov.allow_taker {
            p.q.allow_taker = v;
        }
        if let Some(v) = ov.depth_band {
            p.q.depth_band = v;
        }
        if let Some(v) = ov.max_improve_ticks {
            p.q.max_improve_ticks = v;
        }
        if let Some(v) = ov.max_participation {
            p.q.max_participation = v;
        }
        if let Some(v) = ov.cycle_leg_only {
            p.q.cycle_leg_only = v;
        }
        if let Some(v) = ov.deep_levels {
            p.q.deep_levels = v;
        }
        if let Some(v) = ov.deep_size {
            p.q.deep_size = v;
        }
        p
    }
}

/// The quoting knobs as the strategy sees them this loop, after DB overrides.
#[derive(Debug, Clone)]
pub struct Params {
    pub q: Quoting,
    pub risk: Risk,
    /// Master switch. `ctl pause` sets this false; the bot keeps running and
    /// reporting but pulls all quotes.
    pub enabled: bool,
    /// Place no orders, log what would have happened.
    pub dry_run: bool,
}

macro_rules! ov {
    ($db:expr, $key:expr, $field:expr) => {
        $field = $db.setting_decimal($key, $field).await;
    };
}

impl Params {
    pub async fn load(cfg: &Config, db: &Db, dry_run_flag: bool) -> Params {
        let mut q = cfg.quoting.clone();
        let mut risk = cfg.risk.clone();

        ov!(db, "size_base", q.size_base);
        ov!(db, "size_pct_of_equity", q.size_pct_of_equity);
        ov!(db, "deep_pct_of_inventory", q.deep_pct_of_inventory);
        ov!(db, "ladder_fraction", q.ladder_fraction);
        ov!(db, "level_step_ticks", q.level_step_ticks);
        ov!(db, "level_size_growth", q.level_size_growth);
        ov!(db, "vol_spread_share", q.vol_spread_share);
        ov!(db, "max_half_spread_pct", q.max_half_spread_pct);
        ov!(db, "min_half_spread_ticks", q.min_half_spread_ticks);
        ov!(db, "inventory_skew_ticks", q.inventory_skew_ticks);
        ov!(db, "inventory_size_skew", q.inventory_size_skew);
        ov!(db, "inventory_scale", q.inventory_scale);
        ov!(db, "inventory_scale_pct", q.inventory_scale_pct);
        ov!(db, "edge_half_life_hours", q.edge_half_life_hours);
        q.probe_dead_arms = db.setting_bool("probe_dead_arms", q.probe_dead_arms).await;
        q.cycle_release = db.setting_bool("cycle_release", q.cycle_release).await;
        q.cycle_recycle = db.setting_bool("cycle_recycle", q.cycle_recycle).await;
        q.cycle_churn = db.setting_bool("cycle_churn", q.cycle_churn).await;
        q.twap_only = db.setting_bool("twap_only", q.twap_only).await;
        ov!(db, "taker_max_bps", q.taker_max_bps);
        ov!(db, "cycle_recycle_max_pct", q.cycle_recycle_max_pct);
        ov!(db, "cycle_recycle_trigger", q.cycle_recycle_trigger);
        ov!(db, "cycle_floor_bps", q.cycle_floor_bps);
        ov!(db, "max_order_pct_of_depth", q.max_order_pct_of_depth);
        ov!(db, "max_participation", q.max_participation);
        ov!(db, "size_tolerance", q.size_tolerance);
        ov!(db, "size_jitter", q.size_jitter);
        ov!(db, "deep_size", q.deep_size);
        ov!(db, "deep_size_growth", q.deep_size_growth);
        ov!(db, "deep_price_step", q.deep_price_step);
        ov!(db, "deep_cap_multiple", q.deep_cap_multiple);
        ov!(db, "deep_allocation", q.deep_allocation);
        ov!(db, "max_position_factor", risk.max_position_factor);
        ov!(db, "max_order_pct_of_equity", risk.max_order_pct_of_equity);
        ov!(db, "max_position", risk.max_position);
        ov!(db, "min_position", risk.min_position);
        ov!(db, "max_move", risk.max_move);
        ov!(db, "max_order_notional", risk.max_order_notional);
        ov!(db, "reserve_quote", risk.reserve_quote);
        ov!(db, "reserve_base", risk.reserve_base);

        // Bounded: these come from `ctl set` on a running bot, are persisted,
        // and an absurd value would otherwise be reapplied on every tick.
        if let Ok(Some(v)) = db.get_setting("levels").await
            && let Ok(n) = v.parse::<u32>()
        {
            q.levels = n.min(20);
        }
        q.level_size_growth = q.level_size_growth.clamp(dec!(0.1), dec!(5));
        q.deep_size_growth = q.deep_size_growth.clamp(dec!(0.1), dec!(5));
        q.levels = q.levels.min(20);
        q.deep_levels = q.deep_levels.min(20);
        if let Ok(Some(v)) = db.get_setting("deep_levels").await
            && let Ok(n) = v.parse::<u32>()
        {
            // Clamped HERE, not above: the general clamp runs before this read,
            // so a `ctl set deep_levels 500` sailed straight past it.
            q.deep_levels = n.min(20);
        }
        if let Ok(Some(v)) = db.get_setting("max_improve_ticks").await
            && let Ok(n) = v.parse()
        {
            q.max_improve_ticks = n;
        }
        q.allow_taker = db.setting_bool("allow_taker", q.allow_taker).await;
        if let Ok(Some(v)) = db.get_setting("price_floor").await {
            risk.price_floor = v.parse().ok();
        }
        if let Ok(Some(v)) = db.get_setting("price_ceiling").await {
            risk.price_ceiling = v.parse().ok();
        }

        Params {
            enabled: db.setting_bool("enabled", true).await,
            dry_run: dry_run_flag || db.setting_bool("dry_run", false).await,
            q,
            risk,
        }
    }
}

/// Keys accepted by `ctl set`, with a one-line description each. Kept beside
/// the overrides above so the two cannot drift apart.
pub const TUNABLE: &[(&str, &str)] = &[
    ("enabled", "master switch; false pulls all quotes but keeps the bot running"),
    ("dry_run", "compute and log quotes without sending orders"),
    ("size_base", "ceiling on order size per rung, base currency"),
    ("ladder_fraction", "share of spendable balance the ladder deploys; 0 = fixed size"),
    ("levels", "rungs per side"),
    ("level_step_ticks", "gap between rungs, in ticks"),
    ("level_size_growth", "size multiplier per rung further out"),
    ("vol_spread_share", "share of the recent range carried as half-spread; 0 = off"),
    ("max_half_spread_pct", "ceiling on the half-spread, as a fraction of price"),
    ("min_half_spread_ticks", "minimum half-spread from fair value, in ticks"),
    ("inventory_skew_ticks", "price skew per unit of inventory error, in ticks"),
    ("inventory_size_skew", "size skew per unit of inventory error, 0..1"),
    ("inventory_scale", "inventory error at which the skews are full; 0 = auto"),
    ("inventory_scale_pct", "share of the target at which the skews are full, when auto"),
    ("edge_half_life_hours", "half-life of the evidence behind each arm's yield"),
    ("probe_dead_arms", "rest the exchange minimum on arms sized to nothing"),
    ("cycle_release", "rest a leg the arms defunded when the whole cycle pays"),
    ("cycle_churn", "quote the acquiring book's two sides at neutral size"),
    ("twap_only", "TWAP mode: only a twap program quotes, every other book stands down"),
    ("taker_max_bps", "most a crossing order may pay over fair value, in basis points"),
    ("cycle_recycle", "cross the stablecoin book to refill the acquiring book's bids"),
    ("cycle_recycle_max_pct", "ceiling on one recycle swap, as a share of equity"),
    ("cycle_recycle_trigger", "refill below this share of the bids' full-size need"),
    ("cycle_floor_bps", "basis points a whole cycle must pay, after its errors"),
    ("epoch_ms", "the scoreboard's start: P&L, verdict and earnings count only fills after it (drawn automatically when the config changes, or by `ctl epoch`; the arms are not reset)"),
    ("max_improve_ticks", "ticks we may post better than the touch; 0 = join only"),
    ("allow_taker", "allow crossing the spread to stay on schedule"),
    ("twap_spike_pct", "a twap stops buying while price is this % above the last hour's low; 0 = off"),
    ("twap_hold", "true puts a running twap on hold; it keeps its schedule"),
    ("sell_day_pct", "slow sell: most it sells in 24h, as % of the book's 24h volume"),
    ("sell_hour_pct", "slow sell: most it sells in an hour, as % of that hour's volume"),
    ("sell_levels", "slow sell: how many of the strangers' lowest ask levels get a new order"),
    ("sell_keep_levels", "slow sell: a resting ask is kept while within this many levels of the lowest"),
    ("sell_level_pct", "slow sell: one order is at most this % of the strangers' size at its price"),
    ("sell_max_order", "slow sell: most units in one order; 0 = no cap beyond the level's own size"),
    ("sell_every_secs", "slow sell: seconds between re-planning the asks"),
    ("usdc_convert_min", "in twap mode, sell USDC for USDT at this price or better (1.0010 is par after the fee); 0 = off"),
    ("sell_stand_back", "slow sell: no new asks while the flow (buyers minus sellers, over both) is at or under this"),
    ("sell_boost_price", "slow sell: at and above this price, levels get sell_boost times the size"),
    ("sell_boost", "slow sell: size multiple at and above sell_boost_price"),
    ("sell_flow_mins", "slow sell: minutes of tape read for who is trading"),
    ("sell_flow_min_volume", "slow sell: under this much traded in that window, the flow is ignored"),
    ("max_order_pct_of_depth", "cap one order at this fraction of book depth"),
    ("max_participation", "our max share of market volume in the window"),
    ("size_tolerance", "relative size drift before cancel/replace"),
    ("size_jitter", "randomise sizes by up to this fraction"),
    ("deep_levels", "far-out rungs beyond the main ladder; 0 disables"),
    ("deep_size", "size of the first deep rung, base currency"),
    ("deep_size_growth", "size multiplier per deep rung further out"),
    ("deep_price_step", "price multiple between deep rungs; 1 = fixed tick spacing"),
    ("deep_cap_multiple", "how far deep rungs may exceed the single-order caps"),
    ("deep_allocation", "share of free balance the deep ladder may commit"),
    ("max_position", "fixed upper inventory bound; used only if the factor is 0"),
    ("max_position_factor", "upper bound as a multiple of the target; 0 = use the fixed one"),
    ("max_order_pct_of_equity", "single-order cap as a share of equity; 0 = use the fixed one"),
    ("size_pct_of_equity", "rung ceiling as a share of equity; 0 = use size_base"),
    ("deep_pct_of_inventory", "deep rung size as a share of inventory; 0 = use deep_size"),
    ("min_position", "hard lower inventory bound, base currency"),
    ("max_move", "pause if fair value moves more than this in the vol window"),
    ("max_order_notional", "cap one order at this quote-currency notional"),
    ("price_floor", "refuse to quote below this price"),
    ("price_ceiling", "refuse to quote above this price"),
    ("reserve_quote", "quote currency held back from quoting"),
    ("reserve_base", "base currency held back from quoting"),
];

#[cfg(test)]
mod tests {

    /// An explicit per-market size must not be overwritten by the
    /// account-proportional rule.
    ///
    /// `usdcusdt` sets `size_base = 60`; `size_pct_of_equity = 0.08` then
    /// replaced it with 8% of equity -- an order dozens of times that on a
    /// stablecoin pair, with only the notional cap between that and the book.
    #[test]
    fn an_explicit_market_size_survives_the_equity_rule() {
        let mut base = Params {
            q: Quoting::default(),
            risk: Risk::default(),
            enabled: true,
            dry_run: false,
        };
        base.q.size_base = dec!(250);
        base.q.size_pct_of_equity = dec!(0.08);

        let ov = MarketOverride { size_base: Some(dec!(60)), ..Default::default() };
        let p = base.for_market(Some(&ov));
        assert_eq!(p.q.size_base, dec!(60));
        assert_eq!(
            p.q.size_pct_of_equity, Decimal::ZERO,
            "an explicit size must switch the proportional rule off"
        );

        // With no override the proportional rule still applies.
        let p = base.for_market(None);
        assert_eq!(p.q.size_pct_of_equity, dec!(0.08));
    }

    use super::*;

    #[test]
    fn market_list_falls_back_to_the_single_market() {
        let c = Config::default();
        assert_eq!(c.market_list(), vec!["prlusdt"]);
    }

    /// A typo in an override must fail loudly at startup, not be dropped.
    #[test]
    fn unknown_override_keys_are_rejected() {
        let bad = r#"
            market = "prlusdt"
            [overrides.prlusdt]
            sizebase = 100
        "#;
        let err = toml::from_str::<Config>(bad).unwrap_err().to_string();
        assert!(err.contains("sizebase") || err.contains("unknown"), "got: {err}");
    }

    /// A book's size and order cap can be set as shares of equity, so the
    /// stablecoin pair no longer carries a coin count typed for an account
    /// twice the size. A per-market share wins over a per-market fixed size.
    #[test]
    fn per_market_sizes_can_follow_equity() {
        let base = Params {
            q: Quoting { size_pct_of_equity: dec!(0.03), ..Default::default() },
            risk: Risk { max_order_pct_of_equity: dec!(0.04), ..Default::default() },
            enabled: true,
            dry_run: false,
        };
        let p = base.for_market(Some(&MarketOverride {
            size_pct_of_equity: Some(dec!(0.005)),
            max_order_pct_of_equity: Some(dec!(0.006)),
            ..Default::default()
        }));
        assert_eq!(p.q.size_pct_of_equity, dec!(0.005));
        assert_eq!(p.risk.max_order_pct_of_equity, dec!(0.006));

        let both = base.for_market(Some(&MarketOverride {
            size_base: Some(dec!(60)),
            size_pct_of_equity: Some(dec!(0.005)),
            ..Default::default()
        }));
        assert_eq!(both.q.size_pct_of_equity, dec!(0.005), "the share is the decision");
    }

    /// The arm-sizing knobs parse from the file.
    #[test]
    fn arm_settings_parse() {
        let c: Config = toml::from_str(
            r#"
            [quoting]
            edge_half_life_hours = 6
            inventory_scale_pct = 0.08
            "#,
        )
        .unwrap();
        assert_eq!(c.quoting.edge_half_life_hours, dec!(6));
        assert_eq!(c.quoting.inventory_scale_pct, dec!(0.08));
    }

    /// The checked-in production config must parse, and every key it sets
    /// under [quoting] and [risk] must be a real field. Those two tables do
    /// not reject unknown keys, so a typo there would sit in the file looking
    /// effective while the default quietly applied.
    #[test]
    fn the_production_config_is_valid_and_has_no_stray_keys() {
        let text = include_str!("../deploy/config.prod.toml");
        let cfg: Config = toml::from_str(text).expect("deploy/config.prod.toml parses");
        let raw: toml::Value = toml::from_str(text).unwrap();
        for (table, known) in [
            ("quoting", serde_json::to_value(Quoting::default()).unwrap()),
            ("risk", serde_json::to_value(Risk::default()).unwrap()),
        ] {
            let keys = raw.get(table).and_then(|t| t.as_table()).expect(table);
            for k in keys.keys() {
                assert!(known.get(k).is_some(), "[{table}] sets unknown key {k:?}");
            }
        }
        // A key the strategy never reads is a control that is not one. The two
        // deep spacings are mutually exclusive: `deep_step_ticks` is read only
        // while `deep_price_step` is 1 or below, and vice versa. Exactly one of
        // them must be the live one.
        let quoting = raw.get("quoting").and_then(|t| t.as_table()).unwrap();
        if cfg.quoting.deep_price_step > Decimal::ONE {
            assert!(quoting.get("deep_step_ticks").is_none(), "dead while deep_price_step > 1");
        } else {
            assert!(quoting.get("deep_step_ticks").is_some(), "tick spacing needs a step");
            assert!(cfg.quoting.deep_start_ticks > Decimal::ONE, "rung 0 must clear the touch");
        }
        // And a default that binds must be in the file, where it can be seen:
        // the ladder fit binds on the cash side.
        assert!(quoting.get("ladder_fraction").is_some(), "ladder_fraction binds; say so in the file");
        // The numbers this tuning turns on.
        assert_eq!(cfg.quoting.levels, 2);
        // On, sized by the 400+ arm: the minimum order until it proves itself.
        assert_eq!(cfg.quoting.deep_levels, 5);
        // Nearly flat while measuring: yield is per dollar-hour, so equal
        // exposure means every arm learns at the same rate. Steep growth (it
        // was 2.5, for the sweep catcher) starves the near rungs of the data
        // this run exists to collect.
        assert!(cfg.quoting.deep_size_growth > Decimal::ONE, "a further rung is a better price");
        assert!(cfg.quoting.deep_size_growth < dec!(2), "not while we are measuring");
        assert_eq!(cfg.quoting.min_half_spread_ticks, dec!(1.0));
        assert_eq!(cfg.quoting.vol_spread_share, dec!(0.5));
        assert_eq!(cfg.quoting.inventory_size_skew, dec!(1.0));
        assert_eq!(cfg.quoting.edge_half_life_hours, dec!(12));
        assert!(cfg.quoting.size_pct_of_equity < cfg.risk.max_order_pct_of_equity, "the cap is a backstop");
        let stable = cfg.overrides.get("usdcusdt").expect("usdcusdt override");
        assert_eq!(stable.vol_spread_share, Some(Decimal::ZERO));
        assert!(stable.size_base.is_none(), "no fixed coin counts left");
        assert!(stable.max_order_notional.is_none());
        let thin = cfg.overrides.get("prlusdc").expect("prlusdc override");
        assert!(thin.max_order_notional.is_none() && thin.size_pct_of_equity.is_some());
        // prlusdc is priced off prlusdt THROUGH usdcusdt's book, and a book is
        // only read on the loops its market is due. A referenced book with no
        // cross this loop stands down (`Halt::NoReference`), so the two
        // cadences must coincide or prlusdc would cancel and re-place its
        // ladder on every loop the cross was not fetched.
        assert_eq!(
            thin.every_n_loops.unwrap_or(1),
            stable.every_n_loops.unwrap_or(1),
            "prlusdc and usdcusdt must be due on the same loops"
        );
    }

    #[test]
    fn deep_rungs_can_be_disabled_per_market() {
        let base = Params {
            q: Quoting { deep_levels: 3, ..Default::default() },
            risk: Risk::default(),
            enabled: true,
            dry_run: false,
        };
        let off = base.for_market(Some(&MarketOverride {
            deep_levels: Some(0),
            ..Default::default()
        }));
        assert_eq!(off.q.deep_levels, 0);
        assert_eq!(base.q.deep_levels, 3, "the shared setting is untouched");
    }

    /// The `weight` override that scaled a book's share of the ladder is gone:
    /// capital is split between books by what their arms have earned, and a
    /// fixed weight beside that would be a second, silent opinion.
    #[test]
    fn a_ladder_weight_is_no_longer_a_setting() {
        let err = toml::from_str::<Config>(
            r#"
            market = "prlusdt"
            [overrides.prlusdc]
            weight = 0.35
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("weight") || err.contains("unknown"), "got: {err}");
    }

    #[test]
    fn per_market_overrides_apply_in_isolation() {
        let base = Params {
            q: Quoting { size_base: dec!(250), levels: 3, ..Default::default() },
            risk: Risk::default(),
            enabled: true,
            dry_run: false,
        };
        let thin = base.for_market(Some(&MarketOverride {
            size_base: Some(dec!(100)),
            min_half_spread_ticks: Some(dec!(20)),
            levels: Some(2),
            ..Default::default()
        }));
        assert_eq!(thin.q.size_base, dec!(100));
        assert_eq!(thin.q.levels, 2);
        assert_eq!(thin.q.min_half_spread_ticks, dec!(20));
        // The original is untouched.
        assert_eq!(base.q.size_base, dec!(250));
        assert_eq!(base.q.levels, 3);
        // An absent override changes nothing.
        assert_eq!(base.for_market(None).q.size_base, dec!(250));
    }

    /// usdcusdt has a 0.0001 tick against PRL's 0.01 -- a hundred times finer.
    /// Sharing one spread setting across both would quote the stable pair at a
    /// hundredth of the intended width, which is why overrides exist.
    /// Crossing is allowed on the stablecoin book and NOWHERE else.
    ///
    /// The global stays false because on the PRL books one tick is about 1% and
    /// taking is precisely how a price gets moved -- the rule this bot was
    /// founded on. usdcusdt is the exception: $1.05M of asks, $2.16M of bids, a
    /// 29 bp spread and a mid that has held inside 0.9981-1.0041, crossed in
    /// sizes that are a rounding error against that depth. This test is the scope of that exception,
    /// and it fails if anyone widens it.
    #[test]
    fn crossing_is_allowed_on_the_stablecoin_book_and_nowhere_else() {
        let cfg: Config = toml::from_str(include_str!("../deploy/config.prod.toml")).unwrap();
        assert!(!cfg.quoting.allow_taker, "the global must stay off");
        let p = |m: &str| {
            Params { q: cfg.quoting.clone(), risk: cfg.risk.clone(), enabled: true, dry_run: false }
                .for_market(cfg.overrides.get(m))
        };
        for m in ["prlusdt", "prlusdc"] {
            assert!(!p(m).q.allow_taker, "{m} must never be crossed: one tick is a percent");
        }
        // And every OTHER market in the config, whatever it is called, unless it
        // is the stablecoin pair.
        for (id, _) in cfg.overrides.iter() {
            if id == "usdcusdt" {
                continue;
            }
            assert!(!p(id).q.allow_taker, "{id} must never be crossed");
        }
    }

    #[test]
    fn a_tick_means_different_things_on_different_markets() {
        let prl_tick = crate::num::tick(2);
        let stable_tick = crate::num::tick(4);
        assert_eq!(prl_tick, dec!(0.01));
        assert_eq!(stable_tick, dec!(0.0001));
        assert_eq!(prl_tick / stable_tick, dec!(100));
    }
}
