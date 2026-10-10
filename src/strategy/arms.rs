//! Edge per arm, and the size each arm has earned.
//!
//! An ARM is one place we can rest capital: a market, a side, how far from
//! fair value the order sits when it is placed, and how fast the market was
//! moving when it was placed. Every resting order is a bet placed on one
//! arm; every scored fill is that bet's outcome. This module estimates what
//! each arm pays and turns that into a size multiplier for the rungs that
//! land on it, so capital drifts toward the arms that earn and away from the
//! ones that do not, without anyone re-tuning a spread or typing a
//! volatility threshold by hand.
//!
//! The objective is DOLLARS PER DOLLAR-HOUR OF RESTING CAPITAL, not basis
//! points per fill. Basis points alone send everything to the far rungs,
//! where the edge per fill is highest and fills almost never happen: the deep
//! ladder filled seven times in 22 hours. What an arm pays is its net markout
//! (fill judged five minutes on, less the fee) divided by the dollar-hours
//! its orders spent resting. Fill rate is inside that number, so an arm that
//! never fills scores zero however good its one fill looked.
//!
//! What 24.5 hours of orders and fills said when this was built (2026-09-23,
//! 17,944 orders, 1,600 fills, every fill re-scored against the reference
//! mid, distance measured AT PLACEMENT because that is the only thing the
//! strategy controls):
//!
//!   prlusdt   <50 bp   64,710 $h   599 fills   -10.6 ± 4.3 ‰/$h   the touch
//!             50-100   18,013      314          -0.9 ± 12.9
//!             100-200 120,113      297          -2.3 ± 2.3
//!             200-400  42,501       53          -1.2 ± 6.1
//!             400+     59,202        3           0.0 ± 0.1
//!   prlusdc   <50 bp      510        79        -161   ± 62         at fair value
//!             50-100      630        21         +20.5 ± 9.1
//!             100-200   2,311       100          +4.9 ± 7.5
//!             200-400   3,673        46         +18.2 ± 9.4
//!             400+        367         7         +89.6 ± 52
//!   usdcusdt  <50 bp    4,504        66          +0.76 ± 0.29
//!
//! And what the same day said when cut by the five-minute range of fair
//! value AT PLACEMENT (2026-09-25; net markout per fill, equal-weighted,
//! prlusdt; the range is the same number the volatility breaker reads):
//!
//!   range     time    touch (0-100 bp)      far (100+ bp)
//!   <2%       73%     +0.7 ±  8.6  n=465    -11.4 ± 13.4  n=150
//!   2-4%      21%    -13.4 ±  8.1  n=304    +27.8 ± 13.2  n=126
//!   >4%        6%    -56.8 ± 13.9  n=144    +31.4 ± 34.1  n= 77
//!
//! The calm day that followed (2026-09-25, 250 fills, +39 bp overall) spent
//! 79% of its time under 2% and never rested the touch in a storm at all --
//! the volatility widening had moved rung 0 out -- so it cannot contradict
//! this and does not.
//!
//! Five things about that shape decide the design:
//!
//!   1. The touch loses and the far rungs are at best break-even on the deep
//!      book; the thin book pays away from the touch and bleeds at it. Where
//!      an order sits is the signal, so distance is a dimension of the arm.
//!   2. Buy and sell never differed beyond noise inside a bucket (the largest
//!      gap was 1.4 standard errors; the method-of-moments estimate of the
//!      between-side variance was zero on both days). Direction stays a
//!      dimension of the arm -- a trend will separate the sides -- but each
//!      side is SHRUNK toward the other side of its bucket, allowed to differ
//!      from it by half the bucket's yield a priori.
//!   3. Buckets differ far beyond noise (prlusdc's between-bucket spread is
//!      88 ‰/$h against standard errors of 9-62), so buckets are NOT pooled
//!      toward the market, and it was tried: a far rung that rests for
//!      thousands of dollar-hours and never fills reports itself a certain
//!      zero, right for that rung and worthless as a prior for its
//!      neighbours, and it crushed a +13.5 ± 10 far arm to +0.9 ± 1.5.
//!   4. Volatility changes what an arm pays, and it changes it differently
//!      by bucket: the touch goes from flat to -57 bp a fill as the range
//!      passes 4% (three and a half standard errors of difference), while
//!      the far rungs go from losing to paying. It is not a level shift, so
//!      it cannot be a multiplier on the market's yield; it is a dimension of
//!      the arm, and each cell is shrunk toward the other regimes of its
//!      bucket, allowed to differ by the whole of the bucket's yield a priori
//!      (the between-regime spread measured 0.5-3x the pooled yield, and 3-4
//!      ‰/$h where the pooled yield was near zero). A regime that has not
//!      been measured therefore inherits its bucket's yield with a WIDER
//!      error bar, which under the sizing rule below means a smaller stake:
//!      the book pulls back in a storm before the storm has cost anything,
//!      by exactly as much as the calm evidence fails to cover it, and then
//!      learns the storm's own number as its fills arrive. Nobody types a
//!      threshold; the circuit breaker in `quote.rs` stays only for the
//!      pathological case, a feed that breaks or a price that gaps 40%.
//!   5. A 12-hour half-life on the evidence changed no bucket's sign and left
//!      about 200 effective episodes per arm. PRL went 0.98 -> 1.70 -> 1.36
//!      in two days; an observation from yesterday's regime is worth a
//!      quarter of one from this morning's.
//!   6. On a tick-quantised book, distance from fair value is not where an
//!      order sits. The same 100-200 bp bucket holds "one tick behind the
//!      touch of a one-tick book" and "the touch of a three-tick book", and
//!      those are opposite bets. The first fills only when the level ahead
//!      of it is cleared -- a sweep, after which the mid has moved through
//!      the order -- and the second fills from ordinary taps into a wide,
//!      thin book. Cut by the width of the stranger book AT PLACEMENT (our
//!      own size stripped), the violent day's prlusdt fills read (net
//!      ‰/$h, and the share of the day the book spent that wide):
//!
//!        width      time   yield            touch of that book    behind it
//!        1 tick      81%   -4.0 ±  1.7      -10.1 ± 4.2 (554)    -5.9 ± 3.1
//!        2 ticks     13%   -6.3 ±  9.8      +12.2 ± 41  (148)   -12 ± 18
//!        3+ ticks     6%  +21.0 ± 13.4      +42.8 ± 17   (60)    -25 ± 70
//!
//!      and inside the 50-100 bp bucket alone, -13.6 ± 11 on a one-tick
//!      book against +146 ± 75 on a three-tick one. The one-tick book is
//!      where prlusdt loses, on both sides alike (-24 bp a fill on the
//!      bids, -23 on the asks, five minutes on), and its fills give the
//!      whole spread back inside thirty seconds: +72 bp against the mid at
//!      the fill, 0 at thirty seconds, -14 at a minute. The fill and the
//!      tick of adverse move are one event. The wide book is the one place
//!      on prlusdt that has paid. So the width of the book at placement, in
//!      ticks, is a dimension of the arm -- one, two, or three and more --
//!      shrunk across widths the way regimes are. On the thin books it
//!      seldom splits: prlusdc's stranger spread is 5-9 ticks most of the
//!      time and usdcusdt's 25-47, so nearly every order there lands in the
//!      wide cell, much as every usdcusdt order lands in `Under50`.
//!
//!      Under the twelve-hour half-life and the shrinkage below, the wide
//!      cell does not yet fund itself: on the violent day's copy it reads
//!      -0.8 ± 8.8 in the 50-100 bp bucket once its own +146 ± 75 is
//!      weighed against the bucket's other cells, and the live copy has one
//!      fill there. That is the rule working as written -- seventeen fills
//!      with an error bar of 75 are not evidence -- and it is what this
//!      dimension is for: the cell now accumulates its own fills instead of
//!      being averaged into the one-tick book's, and funds itself when
//!      they say so.
//!
//! Whether the damage is the LEVEL of volatility or its persistence could not
//! be separated on this data: inside the 1-4% band, fills placed after two or
//! more consecutive fast windows lost 21 bp more than fresh ones (1.6
//! standard errors), and the 60-minute range explained nothing once the
//! 5-minute range was in. The level is what is keyed on. A cell's regime is
//! re-read every time its rung is placed, so a storm that lasts keeps
//! landing orders on the storm's cells, which is where its cost is learned.
//!
//! SIZING. What Kelly says about a resting rung, measured on both days: the
//! per-hour variance of the return on a dollar resting on an arm is small,
//! 1-8% per root-hour on the PRL arms and 0.2% on usdcusdt, and the yields
//! are not, so the full-Kelly stake at the point estimate is 1-30 TIMES
//! EQUITY on every arm with a positive mean (usdcusdt: 280x) against a rung
//! of 3% of equity. Scoring the fills thirty minutes on instead of five
//! raises the variance 3-9x (5.6x on prlusdt, 2.8x on prlusdc, on both days,
//! and less in a storm, not more) and the stake is still 5-50 rungs. No
//! Kelly fraction binds; the caps do, and they should, because a thin book
//! is what limits a market maker, not variance. What is genuinely uncertain
//! is the SIGN of the yield, and the bet a rung makes is the one whose
//! outcome is that sign. So an arm quotes the Kelly stake on that bet: at
//! even odds, the net odds that it pays,
//!
//!     share = P(yield > 0) - P(yield < 0), floored at zero,
//!
//! with the probability from a Student-t on the posterior mean and error,
//! whose degrees of freedom are the fill-episodes the error was estimated
//! from, less one. Nothing is typed in: no evidence is zero, an even chance
//! is zero, one sigma is 68%, two is 95%, and it moves continuously in both
//! directions as fills arrive and old ones decay. The t rather than the
//! normal because the error bar on two fills is itself estimated from those
//! two fills: at one degree of freedom a reading three errors from zero is
//! 80% size, not 99.5%, which is what "two lucky fills are not evidence"
//! means as a formula rather than a threshold. The error bar is not inflated
//! for anything: adjacent five-minute episodes were measured independent
//! (the variance of six-episode sums against six times the variance of one
//! was 0.97 on the violent day and 1.21 on the calm one), so the episodes
//! are the sample they appear to be.
//!
//! An arm the rule sizes to nothing still rests the exchange's MINIMUM ORDER
//! (`quote.rs`). Sizing does not speed learning: fills, not dollars, drive
//! the error bar, and per dollar-hour a fill at any size is the same
//! evidence. So the whole exploration budget is the smallest order that can
//! fill, which is the exchange's number, not ours. This is deliberately not
//! Thompson sampling or UCB: both explore by optimism, and with per-arm
//! errors ten to a hundred times the means, optimism means staking real
//! money on noise.
//!
//! Statistics are in f64. They carry error bars of several per mille, and
//! exact decimal arithmetic would only pretend otherwise; the results cross
//! back to `Decimal` at the boundary.

use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::exchange::Side;

/// How far from fair value an order rested when it was placed, in basis
/// points, bucketed.
///
/// The edges follow the PRL tick. At 1.0-1.7 a tick is 60-100 bp, so `<50` is
/// a joined touch on a one-tick book, `50` is one tick behind it (or the touch
/// of a two-tick book), `100` is two ticks, `200` is where the volatility
/// widening puts the ladder in a 2-4% range, and `400+` is the sweep catcher.
/// A 0-25 bp bucket was tried and held nine orders in a day, so the first
/// bucket starts at zero. On usdcusdt (tick 1 bp) every order lands in `<50`
/// and the arm is effectively the side alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Bucket {
    Under50,
    From50,
    From100,
    From200,
    From400,
}

impl Bucket {
    pub const ALL: [Bucket; 5] =
        [Bucket::Under50, Bucket::From50, Bucket::From100, Bucket::From200, Bucket::From400];

    /// Negative distance -- an order placed through fair value -- is the
    /// touch bucket: it is the same bet, made worse.
    pub fn of_bps(bps: f64) -> Bucket {
        if bps < 50.0 {
            Bucket::Under50
        } else if bps < 100.0 {
            Bucket::From50
        } else if bps < 200.0 {
            Bucket::From100
        } else if bps < 400.0 {
            Bucket::From200
        } else {
            Bucket::From400
        }
    }

    /// Never contains `<`.
    ///
    /// Telegram parses the status message as HTML and rejects the WHOLE
    /// message if it cannot: a bucket labelled "<50" reads as an unclosed tag
    /// and returned `can't parse entities: Unsupported start tag "50"`, so
    /// `status` silently stopped answering the moment this table shipped. The
    /// label is data that flows into markup, so it carries no markup
    /// characters of its own rather than relying on every call site to escape
    /// it.
    pub fn label(self) -> &'static str {
        match self {
            Bucket::Under50 => "0-50",
            Bucket::From50 => "50",
            Bucket::From100 => "100",
            Bucket::From200 => "200",
            Bucket::From400 => "400+",
        }
    }
}

/// How fast the market was moving when the order was placed: the largest
/// relative excursion of fair value inside the volatility window (five
/// minutes), the same reading the breaker and the spread widening use.
///
/// The edges are where the touch's markout changed by more than its error
/// on the violent day: flat below a 2% range, -13 bp a fill from 2 to 4%,
/// -57 above (see the module doc). In ticks on PRL a 2% range is three, a
/// 4% range is six. On usdcusdt, which ranges a tenth of a percent, every
/// order lands in `Calm`, as every order lands in `Under50`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum Regime {
    #[default]
    Calm,
    Fast,
    Wild,
}

impl Regime {
    pub const ALL: [Regime; 3] = [Regime::Calm, Regime::Fast, Regime::Wild];

    /// `recent_move` is the five-minute range as a fraction of price.
    pub fn of_move(recent_move: f64) -> Regime {
        if recent_move < 0.02 {
            Regime::Calm
        } else if recent_move < 0.04 {
            Regime::Fast
        } else {
            Regime::Wild
        }
    }

    /// Words, not ranges: "<2%" would be an unclosed tag to Telegram, the
    /// same trap `Bucket::label` documents.
    pub fn label(self) -> &'static str {
        match self {
            Regime::Calm => "calm",
            Regime::Fast => "fast",
            Regime::Wild => "wild",
        }
    }
}

/// How wide the stranger book was when the order was placed, in ticks:
/// the best ask less the best bid with our own size stripped, which is the
/// book `quote::plan` sees.
///
/// Ticks, not basis points, because the tick is what quantises where a rung
/// can rest: on a one-tick book a rung either joins the queue at the touch
/// or sits a whole tick behind it, and neither is a bet on the spread -- it
/// is a bet on the level ahead being swept. On a three-tick book the same
/// rung IS the touch, a tick and a half from the mid, and fills from taps.
/// See point 6 of the module doc for what each paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum Width {
    /// One tick, or a crossed or synthesised touch: nothing to make.
    #[default]
    One,
    Two,
    /// Three ticks and more. Nearly every prlusdc and usdcusdt order lands
    /// here.
    Wide,
}

impl Width {
    pub const ALL: [Width; 3] = [Width::One, Width::Two, Width::Wide];

    /// `ticks` is the spread in ticks; on a real book it is a whole number,
    /// and a synthesised side lands a tick away, so the edges sit between.
    pub fn of_ticks(ticks: f64) -> Width {
        if ticks < 1.5 {
            Width::One
        } else if ticks < 2.5 {
            Width::Two
        } else {
            Width::Wide
        }
    }

    /// No markup characters, for the reason `Bucket::label` gives.
    pub fn label(self) -> &'static str {
        match self {
            Width::One => "1",
            Width::Two => "2",
            Width::Wide => "3+",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Arm {
    pub market: String,
    pub side: Side,
    pub bucket: Bucket,
    /// Defaulted on read so a table stored before the regime existed still
    /// loads; its rows are the calm ones, which is where 73-79% of the time
    /// was spent.
    #[serde(default)]
    pub regime: Regime,
    /// Defaulted on read for the same reason: a one-tick book is where
    /// prlusdt spent 81-84% of its time. A stored table only bridges the
    /// gap to the first refresh, which re-reads every order's own width.
    #[serde(default)]
    pub width: Width,
}

/// One five-minute episode of one arm, as the database reports it.
///
/// Episodes rather than fills, for the reason the per-book Kelly layer this
/// replaced found first: fills inside one markout horizon share a price path
/// and are one bet, and treating them as independent understated the
/// variance sixfold.
#[derive(Debug, Clone)]
pub struct Observation {
    pub arm: Arm,
    /// Start of the episode, unix ms.
    pub episode_ms: i64,
    /// Dollar-hours of orders resting on this arm during the episode.
    pub exposure_dh: f64,
    /// Markout less fees of the fills in the episode, in quote currency.
    pub net: f64,
    pub fills: u32,
    /// Notional of the orders counted into this episode, for the size of a
    /// typical bet on the arm.
    pub orders_notional: f64,
    pub orders: u32,
}

/// What an arm's own episodes say it pays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// Net dollars per dollar-hour of resting capital, in per mille.
    pub yield_pm: f64,
    pub se_pm: f64,
    /// Decay-weighted fill count: the evidence behind the estimate.
    pub fills: f64,
    /// Decay-weighted dollar-hours.
    pub exposure_dh: f64,
    /// Effective number of episodes after decay.
    pub episodes: f64,
    /// Decay-weighted count of episodes that held a fill: the sample the
    /// error bar is estimated from, and so the degrees of freedom behind it.
    pub fill_episodes: f64,
    /// Mean order notional: the size of one bet on this arm.
    pub bet: f64,
}

/// Per-fill markout standard deviation on the busiest book, as a fraction of
/// notional, measured 2026-09-23 on prlusdt's fills (~180 bp). One typical
/// fill's worth of uncertainty, for the fill not yet seen, is the floor on
/// every arm's standard error: without it an arm with no fills reports
/// itself as a certain zero, because the ratio estimator's variance is
/// identically zero until the first fill. It fades as fills arrive -- once
/// they exist their own dispersion is the evidence, and at PRL's 180 bp the
/// floor would otherwise have doubled usdcusdt's error bar (its fills move
/// 14 bp, not 180) and cut the one proven book to a quarter of its size.
const SIGMA_FILL: f64 = 0.018;

/// Decay-weighted ratio estimator over episodes.
///
/// `yield = sum(w * net) / sum(w * exposure)`; the variance is the standard
/// ratio-estimator form with episodes as the sampling unit, so a burst of
/// fills inside one five-minute window counts once. The per-episode RATIO
/// (net / exposure, then averaged) was tried and rejected: episodes with a
/// few dollars of exposure and one fill produce yields in the thousands of
/// per mille, and the mean of those is dominated by which tiny episodes
/// happened to fill.
pub fn estimate(obs: &[&Observation], now_ms: i64, half_life_ms: i64) -> Option<Estimate> {
    if obs.is_empty() {
        return None;
    }
    let weight = |ep: i64| -> f64 {
        if half_life_ms <= 0 {
            return 1.0;
        }
        let age = (now_ms - ep).max(0) as f64;
        0.5f64.powf(age / half_life_ms as f64)
    };
    let (mut w_exp, mut w_net, mut w_fills, mut w_sum, mut w_sq) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut notional, mut orders, mut w_fill_eps) = (0.0, 0.0, 0.0);
    for o in obs {
        let w = weight(o.episode_ms);
        w_exp += w * o.exposure_dh;
        w_net += w * o.net;
        w_fills += w * o.fills as f64;
        if o.fills > 0 {
            w_fill_eps += w;
        }
        w_sum += w;
        w_sq += w * w;
        notional += w * o.orders_notional;
        orders += w * o.orders as f64;
    }
    if w_exp <= 0.0 {
        return None;
    }
    let r = w_net / w_exp;
    let var: f64 = obs
        .iter()
        .map(|o| {
            let w = weight(o.episode_ms);
            (w * (o.net - r * o.exposure_dh)).powi(2)
        })
        .sum::<f64>()
        / (w_exp * w_exp);
    let bet = if orders > 0.0 { notional / orders } else { 0.0 };
    // One more typical fill, in yield terms, weighed against the fills seen.
    let floor = bet * SIGMA_FILL / w_exp / (1.0 + w_fills).sqrt();
    Some(Estimate {
        yield_pm: 1000.0 * r,
        se_pm: 1000.0 * (var + floor * floor).sqrt(),
        fills: w_fills,
        exposure_dh: w_exp,
        episodes: if w_sq > 0.0 { w_sum * w_sum / w_sq } else { 0.0 },
        fill_episodes: w_fill_eps,
        bet,
    })
}

/// Why an arm is sized the way it is. Reported, so the operator sees which
/// rule binds on each arm rather than inferring it from the sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Fewer than two fill-episodes behind the posterior: an error bar
    /// cannot be estimated from one point, so there is no opinion. The rung
    /// rests the exchange minimum.
    NoData,
    /// Sized to the posterior's net odds of paying.
    Sized,
    /// An execution program (accumulate, distribute, flatten) is running on
    /// this base asset: the fills are the job, not a bet, and are not gated.
    Execution,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::NoData => "no data",
            State::Sized => "sized",
            State::Execution => "program",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmSize {
    pub arm: Arm,
    /// Multiplier on the rung size, in [0, 1]. Zero means the exchange
    /// minimum, never nothing: see `quote::at_least_min`.
    pub share: Decimal,
    pub state: State,
    /// The arm's own evidence: decayed fills and dollar-hours.
    pub fills: f64,
    pub exposure_dh: f64,
    /// Posterior mean and standard deviation of the yield after shrinkage
    /// toward the other cells of the bucket.
    pub yield_pm: f64,
    pub se_pm: f64,
    /// Posterior probability that the yield is positive; the share is its
    /// net odds, `2p - 1`. One half when there is no opinion.
    #[serde(default = "half")]
    pub p_pays: f64,
    /// Mean notional of one order on this arm, decay-weighted: the size of
    /// one bet, as it has actually been placed.
    #[serde(default)]
    pub bet: f64,
}

fn half() -> f64 {
    0.5
}

#[derive(Debug, Clone)]
pub struct Params {
    pub half_life_ms: i64,
}

/// A market whose arms are to be sized, and whether an execution program
/// exempts it from the gate.
pub struct Book<'a> {
    pub market: &'a str,
    pub execution: bool,
}

/// How far a side may sit from its bucket's pooled yield, a priori, as a
/// share of that yield. Half: the two sides of one bucket are the same
/// distance on the same book, and no side has yet been shown to differ from
/// its bucket at all. A side separates from the pool once its own standard
/// error drops below this, which takes a trend that actually treats the two
/// sides differently.
const TAU_SIDE_SHARE: f64 = 0.5;
/// Floor on that prior width, so a bucket pooled near zero does not pin its
/// sides to exactly zero. Half a per mille is a third of the smallest yield
/// an arm has earned (usdcusdt, +0.76 ± 0.29).
const TAU_SIDE_FLOOR_PM: f64 = 0.5;
/// How far a regime may sit from its bucket's pooled yield, a priori, as a
/// share of that yield. The whole of it: the between-regime spread within a
/// bucket, by method of moments on the violent day, was 0.5-3x the pooled
/// yield wherever it could be measured (prlusdc 100-200 bp: pool +6,
/// spread 18; 200-400: pool +12, spread 48) and 7-11 ‰ on the calm day
/// against pools of 3-11.
const TAU_REGIME_SHARE: f64 = 1.0;
/// Floor on that width: the same measurement on the prlusdt buckets whose
/// pooled yield sat near zero gave spreads of 2.7 and 3.8 ‰/$h.
const TAU_REGIME_FLOOR_PM: f64 = 3.0;
/// How far a book width may sit from its bucket's pooled yield, a priori,
/// as a share of it. The whole of it, like a regime: the between-width
/// spread within a bucket, by method of moments on the violent day, was
/// zero within error in the 100-200 and 200-400 bp buckets (three cells
/// each, errors of 2-20 ‰/$h) and 49 ‰/$h in the 50-100 bp bucket, where
/// the one-tick book lost 14 and the three-tick book paid 146. Nothing
/// there separates it from the regime prior, so it takes the same one.
const TAU_WIDTH_SHARE: f64 = 1.0;
const TAU_WIDTH_FLOOR_PM: f64 = 3.0;

/// The posterior on one arm's yield: mean, standard deviation, and the
/// fill-episodes its error bar was estimated from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Posterior {
    mean: f64,
    sd: f64,
    fill_episodes: f64,
}

impl Posterior {
    /// Degrees of freedom of the t on this posterior: one point pins the
    /// mean, the rest estimate the spread.
    fn dof(&self) -> f64 {
        self.fill_episodes - 1.0
    }

    /// Probability that the yield is positive, or none when the error bar
    /// rests on a single point and says nothing.
    fn p_pays(&self) -> Option<f64> {
        (self.dof() >= 1.0 && self.sd > 0.0).then(|| student_t_cdf(self.mean / self.sd, self.dof()))
    }
}

/// One source of evidence about a cell: an estimate, widened by the
/// difference the cell is allowed from it a priori.
fn source(est: Option<&Estimate>, widen_pm: f64) -> Option<(f64, f64, f64)> {
    est.filter(|e| e.se_pm > 0.0)
        .map(|e| (e.yield_pm, (e.se_pm * e.se_pm + widen_pm * widen_pm).sqrt(), e.fill_episodes))
}

/// Precision-weighted combination of a cell's own estimate with the other
/// cells of its bucket, each of those widened by the difference it is allowed
/// a priori: the sibling side by `tau_side`, the other regimes by
/// `tau_regime`, the other widths by `tau_width`, and a cell that differs on
/// several of those by all of them in quadrature. Every observation is in
/// exactly one source, so nothing counts twice: shrinking a side toward a
/// pool that contained it tightened the posterior below the side's own error
/// once, and reported a lower bound of +13 on an arm whose own error bar
/// reached below zero. The fill-episodes add: all of those residuals went
/// into the error bar.
fn combine(sources: &[Option<(f64, f64, f64)>]) -> Option<Posterior> {
    let (mut p_sum, mut py_sum, mut fe) = (0.0, 0.0, 0.0);
    for (y, sd, n) in sources.iter().flatten() {
        let p = 1.0 / (sd * sd);
        p_sum += p;
        py_sum += p * y;
        fe += n;
    }
    (p_sum > 0.0).then(|| Posterior { mean: py_sum / p_sum, sd: (1.0 / p_sum).sqrt(), fill_episodes: fe })
}

/// The share an arm quotes for a given probability of paying: the net odds,
/// floored at zero.
fn net_odds(p: f64) -> Decimal {
    Decimal::try_from((2.0 * p - 1.0).clamp(0.0, 1.0)).unwrap_or(Decimal::ZERO).round_dp(4)
}

/// Size every arm of every book from the observations.
pub fn size_arms(obs: &[Observation], books: &[Book], now_ms: i64, p: &Params) -> Vec<ArmSize> {
    let mut by_arm: HashMap<&Arm, Vec<&Observation>> = HashMap::new();
    for o in obs {
        by_arm.entry(&o.arm).or_default().push(o);
    }
    let est = |arms: &[Arm]| -> Option<Estimate> {
        let pooled: Vec<&Observation> =
            arms.iter().filter_map(|a| by_arm.get(a)).flatten().copied().collect();
        estimate(&pooled, now_ms, p.half_life_ms)
    };
    let mut out = Vec::new();
    for book in books {
        for bucket in Bucket::ALL {
            let arm_of =
                |side, regime, width| Arm { market: book.market.to_string(), side, bucket, regime, width };
            let all: Vec<Arm> = Side::BOTH
                .iter()
                .flat_map(|&s| Regime::ALL.iter().map(move |&r| (s, r)))
                .flat_map(|(s, r)| Width::ALL.iter().map(move |&w| arm_of(s, r, w)))
                .collect();
            let pool = est(&all);
            let tau = |share: f64, floor: f64| {
                pool.as_ref().map(|e| (share * e.yield_pm.abs()).max(floor)).unwrap_or(floor)
            };
            let taus = [
                tau(TAU_SIDE_SHARE, TAU_SIDE_FLOOR_PM),
                tau(TAU_REGIME_SHARE, TAU_REGIME_FLOOR_PM),
                tau(TAU_WIDTH_SHARE, TAU_WIDTH_FLOOR_PM),
            ];

            for &side in &Side::BOTH {
                for regime in Regime::ALL {
                    for width in Width::ALL {
                        let arm = arm_of(side, regime, width);
                        let own = est(std::slice::from_ref(&arm));
                        // Every other cell of the bucket, grouped by which of
                        // the three dimensions it differs on, and widened by
                        // the prior on each of those differences.
                        let sources: Vec<Option<(f64, f64, f64)>> = (0u8..8)
                            .map(|mask| {
                                let differs = [mask & 1 != 0, mask & 2 != 0, mask & 4 != 0];
                                let sides: Vec<Side> =
                                    if differs[0] { vec![side.opposite()] } else { vec![side] };
                                let regimes: Vec<Regime> = if differs[1] {
                                    Regime::ALL.into_iter().filter(|&r| r != regime).collect()
                                } else {
                                    vec![regime]
                                };
                                let widths: Vec<Width> = if differs[2] {
                                    Width::ALL.into_iter().filter(|&w| w != width).collect()
                                } else {
                                    vec![width]
                                };
                                let arms: Vec<Arm> = sides
                                    .iter()
                                    .flat_map(|&s| regimes.iter().map(move |&r| (s, r)))
                                    .flat_map(|(s, r)| widths.iter().map(move |&w| arm_of(s, r, w)))
                                    .collect();
                                let widen = differs
                                    .iter()
                                    .zip(taus)
                                    .filter(|(d, _)| **d)
                                    .map(|(_, t)| t * t)
                                    .sum::<f64>()
                                    .sqrt();
                                if mask == 0 { source(own.as_ref(), 0.0) } else { source(est(&arms).as_ref(), widen) }
                            })
                            .collect();
                        let post = combine(&sources);
                        let p_pays = post.and_then(|q| q.p_pays());
                        let (share, state) = match (book.execution, p_pays) {
                            (true, _) => (Decimal::ONE, State::Execution),
                            (false, Some(p)) => (net_odds(p), State::Sized),
                            (false, None) => (Decimal::ZERO, State::NoData),
                        };
                        let (fills, exposure, bet) =
                            own.as_ref().map(|e| (e.fills, e.exposure_dh, e.bet)).unwrap_or((0.0, 0.0, 0.0));
                        out.push(ArmSize {
                            arm,
                            share,
                            state,
                            fills,
                            exposure_dh: exposure,
                            yield_pm: post.map(|q| q.mean).unwrap_or(0.0),
                            se_pm: post.map(|q| q.sd).unwrap_or(0.0),
                            p_pays: p_pays.unwrap_or(0.5),
                            bet,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Cumulative distribution of Student's t with `dof` degrees of freedom,
/// through the regularized incomplete beta function.
pub fn student_t_cdf(t: f64, dof: f64) -> f64 {
    if !t.is_finite() {
        return if t > 0.0 { 1.0 } else { 0.0 };
    }
    let x = dof / (dof + t * t);
    let tail = 0.5 * betai(dof / 2.0, 0.5, x);
    if t >= 0.0 { 1.0 - tail } else { tail }
}

/// Regularized incomplete beta I_x(a, b), by Lentz's continued fraction
/// (Numerical Recipes `betai`/`betacf`).
fn betai(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let bt = (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        bt * betacf(a, b, x) / a
    } else {
        1.0 - bt * betacf(b, a, 1.0 - x) / b
    }
}

fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const TINY: f64 = 1e-300;
    const EPS: f64 = 3e-14;
    let (qab, qap, qam) = (a + b, a + 1.0, a - 1.0);
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < TINY {
        d = TINY;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=300 {
        let m = m as f64;
        let m2 = 2.0 * m;
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + aa / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        h *= d * c;
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + aa / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// Lanczos approximation to ln Γ(x), x > 0.
fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection.
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = C[0];
    let t = x + G + 0.5;
    for (i, c) in C.iter().enumerate().skip(1) {
        a += c / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// The multipliers one market's rungs look up, by side, distance, regime and
/// book width.
#[derive(Debug, Clone)]
pub struct ArmScales {
    by: HashMap<(Side, Bucket, Regime, Width), Decimal>,
    /// For a rung whose arm has no entry -- before the first refresh, or a
    /// market the sizer has not seen. The safe direction is small.
    fallback: Decimal,
}

impl ArmScales {
    /// Every rung at full size, for a test that is about something else.
    #[cfg(test)]
    pub fn full() -> Self {
        ArmScales::uniform(Decimal::ONE)
    }

    /// Every rung at `share`.
    pub fn uniform(share: Decimal) -> Self {
        ArmScales { by: HashMap::new(), fallback: share.max(Decimal::ZERO).min(Decimal::ONE) }
    }

    /// Nothing known: every rung at the exchange minimum.
    pub fn none() -> Self {
        ArmScales::uniform(Decimal::ZERO)
    }

    pub fn of_market(sizes: &[ArmSize], market: &str) -> Self {
        let mut s = ArmScales::none();
        for z in sizes.iter().filter(|z| z.arm.market == market) {
            s.by.insert((z.arm.side, z.arm.bucket, z.arm.regime, z.arm.width), z.share);
        }
        s
    }

    /// `bps` is the rung's distance from fair value at placement, positive on
    /// the passive side; `recent_move` the five-minute range at placement;
    /// `width_ticks` the stranger book's spread in ticks at placement.
    pub fn scale(&self, side: Side, bps: Decimal, recent_move: Decimal, width_ticks: Decimal) -> Decimal {
        let b = Bucket::of_bps(f64::try_from(bps).unwrap_or(0.0));
        let r = Regime::of_move(f64::try_from(recent_move).unwrap_or(0.0));
        let w = Width::of_ticks(f64::try_from(width_ticks).unwrap_or(1.0));
        self.by.get(&(side, b, r, w)).copied().unwrap_or(self.fallback)
    }

    /// Set one bucket's share in every regime and width.
    #[cfg(test)]
    pub fn with(mut self, side: Side, bucket: Bucket, share: Decimal) -> Self {
        for r in Regime::ALL {
            for w in Width::ALL {
                self.by.insert((side, bucket, r, w), share);
            }
        }
        self
    }

    /// Set one regime's share in every width.
    #[cfg(test)]
    pub fn with_regime(mut self, side: Side, bucket: Bucket, regime: Regime, share: Decimal) -> Self {
        for w in Width::ALL {
            self.by.insert((side, bucket, regime, w), share);
        }
        self
    }

    /// Set one width's share in every regime.
    #[cfg(test)]
    pub fn with_width(mut self, side: Side, bucket: Bucket, width: Width, share: Decimal) -> Self {
        for r in Regime::ALL {
            self.by.insert((side, bucket, r, width), share);
        }
        self
    }
}

/// How much of a currency a market's ladder on `side` has earned, for
/// splitting that currency between the books that spend it.
///
/// The largest share among the side's arms, in any regime: one arm at full
/// size wants the funding a full ladder needs, whatever its neighbours are
/// doing, and funding is not a stake -- money allotted to a book that is
/// quoting the minimum in a storm simply waits. Two books on one base asset
/// are reconciled here rather than summed -- the currency is split, so the
/// PRL that prlusdc's asks earn is PRL that prlusdt's asks do not get.
pub fn spend_weight(sizes: &[ArmSize], market: &str, side: Side) -> Option<Decimal> {
    sizes
        .iter()
        .filter(|z| z.arm.market == market && z.arm.side == side)
        .map(|z| z.share)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const H: i64 = 300_000;

    fn arm(market: &str, side: Side, bucket: Bucket) -> Arm {
        arm_in(market, side, bucket, Regime::Calm)
    }

    fn arm_in(market: &str, side: Side, bucket: Bucket, regime: Regime) -> Arm {
        Arm { market: market.into(), side, bucket, regime, width: Width::One }
    }

    fn arm_on(market: &str, side: Side, bucket: Bucket, width: Width) -> Arm {
        Arm { market: market.into(), side, bucket, regime: Regime::Calm, width }
    }

    /// `n` episodes, one per five minutes ending at `now`, each resting
    /// `exposure` dollar-hours and, every `every`-th one, filling once for
    /// `net` dollars.
    fn episodes(a: &Arm, n: usize, exposure: f64, net: f64, every: usize) -> Vec<Observation> {
        (0..n)
            .map(|i| {
                let filled = every > 0 && i % every == 0;
                Observation {
                    arm: a.clone(),
                    episode_ms: (i as i64) * H,
                    exposure_dh: exposure,
                    net: if filled { net } else { 0.0 },
                    fills: filled as u32,
                    orders_notional: 1000.0,
                    orders: 1,
                }
            })
            .collect()
    }

    fn params() -> Params {
        Params { half_life_ms: 0 }
    }

    fn book(market: &str) -> Book<'_> {
        Book { market, execution: false }
    }

    fn find<'a>(zs: &'a [ArmSize], market: &str, side: Side, bucket: Bucket) -> &'a ArmSize {
        find_in(zs, market, side, bucket, Regime::Calm)
    }

    fn find_in<'a>(zs: &'a [ArmSize], market: &str, side: Side, bucket: Bucket, regime: Regime) -> &'a ArmSize {
        find_cell(zs, market, side, bucket, regime, Width::One)
    }

    fn find_on<'a>(zs: &'a [ArmSize], market: &str, side: Side, bucket: Bucket, width: Width) -> &'a ArmSize {
        find_cell(zs, market, side, bucket, Regime::Calm, width)
    }

    fn find_cell<'a>(
        zs: &'a [ArmSize],
        market: &str,
        side: Side,
        bucket: Bucket,
        regime: Regime,
        width: Width,
    ) -> &'a ArmSize {
        zs.iter()
            .find(|z| {
                z.arm.market == market
                    && z.arm.side == side
                    && z.arm.bucket == bucket
                    && z.arm.regime == regime
                    && z.arm.width == width
            })
            .unwrap()
    }

    /// The one that matters. The prlusdt touch measured -10.6 ± 4.3 ‰ per
    /// dollar-hour over 599 fills: an arm like that must quote nothing but
    /// the minimum, whatever the rest of the book is doing.
    #[test]
    fn a_losing_arm_sizes_down_to_the_minimum() {
        let a = arm("prlusdt", Side::Buy, Bucket::Under50);
        let obs = episodes(&a, 240, 250.0, -8.0, 2);
        let zs = size_arms(&obs, &[book("prlusdt")], 240 * H, &params());
        let z = find(&zs, "prlusdt", Side::Buy, Bucket::Under50);
        assert!(z.yield_pm < 0.0, "yield {}", z.yield_pm);
        assert_eq!(z.state, State::Sized);
        assert!(z.p_pays < 0.1, "{z:?}");
        assert_eq!(z.share, Decimal::ZERO, "the minimum order keeps data flowing, nothing more");
    }

    /// A positive mean whose error bar reaches well below zero is mostly
    /// not an edge yet, and is sized like one: some, not much.
    #[test]
    fn an_uncertain_edge_is_sized_to_its_odds() {
        let a = arm("prlusdc", Side::Sell, Bucket::From100);
        // Alternating wins and losses that net slightly positive: mean small,
        // standard error larger.
        let mut obs = episodes(&a, 200, 10.0, 6.0, 2);
        for (i, o) in obs.iter_mut().enumerate() {
            if i % 4 == 2 {
                o.net = -5.0;
                o.fills = 1;
            }
        }
        let zs = size_arms(&obs, &[book("prlusdc")], 200 * H, &params());
        let z = find(&zs, "prlusdc", Side::Sell, Bucket::From100);
        assert!(z.yield_pm > 0.0, "mean {} should be positive", z.yield_pm);
        assert!(z.yield_pm < z.se_pm, "mean {} inside one error {}", z.yield_pm, z.se_pm);
        assert_eq!(z.state, State::Sized);
        assert!(z.p_pays > 0.5 && z.p_pays < 0.84, "{z:?}");
        assert!(z.share > Decimal::ZERO && z.share < dec!(0.68), "{z:?}");
    }

    /// An established edge grows with the evidence rather than jumping to
    /// full size on the first good hour.
    #[test]
    fn an_established_edge_grows_with_the_evidence() {
        let a = arm("usdcusdt", Side::Buy, Bucket::Under50);
        // +0.8 ‰/h in expectation: 20 $h an episode, a $0.08 fill every 5th.
        let few = episodes(&a, 60, 20.0, 0.08, 5);
        let many = episodes(&a, 600, 20.0, 0.08, 5);
        let zf = find(&size_arms(&few, &[book("usdcusdt")], 60 * H, &params()), "usdcusdt", Side::Buy, Bucket::Under50).clone();
        let zm = find(&size_arms(&many, &[book("usdcusdt")], 600 * H, &params()), "usdcusdt", Side::Buy, Bucket::Under50).clone();
        assert!(zf.se_pm > zm.se_pm, "more episodes, tighter error");
        assert!(zm.share > zf.share, "evidence must grow the stake: {} then {}", zf.share, zm.share);
        assert!(zm.share > dec!(0.5) && zm.share <= Decimal::ONE, "{zm:?}");
    }

    /// prlusdc's 200-400 bp rungs earned +18 ± 9: two errors from zero over
    /// eighty fills is a 97% chance of paying, and the arm quotes nearly
    /// everything the caps allow.
    #[test]
    fn a_strong_edge_is_nearly_full_size() {
        let a = arm("prlusdc", Side::Sell, Bucket::From200);
        let obs = episodes(&a, 240, 15.0, 4.0, 3);
        let zs = size_arms(&obs, &[book("prlusdc")], 240 * H, &params());
        let z = find(&zs, "prlusdc", Side::Sell, Bucket::From200);
        assert_eq!(z.state, State::Sized, "{z:?}");
        assert!(z.p_pays > 0.97, "{z:?}");
        assert!(z.share > dec!(0.95), "{z:?}");
    }

    /// An error bar cannot be estimated from one point. One fill-episode is
    /// no opinion, and an arm never quoted at all is none either; both rest
    /// the minimum.
    #[test]
    fn one_fill_is_no_opinion() {
        let a = arm("prlusdt", Side::Sell, Bucket::From400);
        let obs = episodes(&a, 240, 250.0, 300.0, 240); // one huge win
        let zs = size_arms(&obs, &[book("prlusdt")], 240 * H, &params());
        let z = find(&zs, "prlusdt", Side::Sell, Bucket::From400);
        assert_eq!(z.state, State::NoData, "{z:?}");
        assert_eq!(z.share, Decimal::ZERO);
        assert_eq!(z.p_pays, 0.5);
        assert!(z.yield_pm > 0.0, "the reading is still reported");
        let z = find(&zs, "prlusdt", Side::Buy, Bucket::From100);
        assert_eq!(z.state, State::NoData);
    }

    /// Three glorious fills are a 94% chance under a normal and 89% under a
    /// t with two degrees of freedom. The difference is the whole point: the
    /// error bar on three fills is estimated from those three fills.
    #[test]
    fn a_few_lucky_fills_are_discounted_by_the_t() {
        let a = arm("prlusdt", Side::Sell, Bucket::From400);
        let obs = episodes(&a, 240, 250.0, 300.0, 80); // three huge wins
        let zs = size_arms(&obs, &[book("prlusdt")], 240 * H, &params());
        let z = find(&zs, "prlusdt", Side::Sell, Bucket::From400);
        assert_eq!(z.state, State::Sized);
        let t = z.yield_pm / z.se_pm;
        assert!(t > 1.5, "{z:?}");
        let normal = student_t_cdf(t, 1e9);
        assert!(z.p_pays < normal - 0.03, "t {} vs normal {}: {z:?}", z.p_pays, normal);
        assert!(z.share < dec!(0.85), "{z:?}");
    }

    /// An arm with exposure and no fills is not a certain zero. The ratio
    /// estimator's variance is identically zero until the first fill, which
    /// would otherwise let a rung nobody has ever hit report itself as
    /// exactly break-even with no error bar.
    #[test]
    fn no_fills_is_not_zero_uncertainty() {
        let a = arm("prlusdt", Side::Buy, Bucket::From400);
        let obs = episodes(&a, 100, 100.0, 0.0, 0);
        let e = estimate(&obs.iter().collect::<Vec<_>>(), 100 * H, 0).unwrap();
        assert_eq!(e.fills, 0.0);
        assert_eq!(e.fill_episodes, 0.0);
        assert_eq!(e.yield_pm, 0.0);
        assert!(e.se_pm > 0.0, "one typical fill's worth of doubt must remain");
    }

    /// Fills inside one horizon are one bet. A burst of two hundred fills in
    /// one five-minute window must not tighten the error as if it were two
    /// hundred observations, and it is one fill-episode, not two hundred.
    #[test]
    fn a_burst_inside_one_episode_is_one_observation() {
        let a = arm("prlusdt", Side::Buy, Bucket::Under50);
        let mut spread = episodes(&a, 200, 10.0, 1.0, 1);
        let mut burst = episodes(&a, 200, 10.0, 0.0, 0);
        burst[0].net = 200.0;
        burst[0].fills = 200;
        spread.truncate(200);
        let es = estimate(&spread.iter().collect::<Vec<_>>(), 200 * H, 0).unwrap();
        let eb = estimate(&burst.iter().collect::<Vec<_>>(), 200 * H, 0).unwrap();
        assert!((es.yield_pm - eb.yield_pm).abs() < 1e-9, "same total, same yield");
        assert!(eb.se_pm > es.se_pm * 5.0, "the burst must be far less certain: {} vs {}", eb.se_pm, es.se_pm);
        assert_eq!(eb.fill_episodes, 1.0);
        assert_eq!(es.fill_episodes, 200.0);
        let zs = size_arms(&burst, &[book("prlusdt")], 200 * H, &params());
        assert_eq!(find(&zs, "prlusdt", Side::Buy, Bucket::Under50).state, State::NoData, "one episode is no opinion");
    }

    /// The evidence decays: an arm that was bad and is now good must be
    /// judged on recent episodes, or it could never recover. PRL went from
    /// 0.98 to 1.70 to 1.36 in two days.
    #[test]
    fn old_episodes_decay_so_an_arm_can_recover() {
        let a = arm("prlusdt", Side::Buy, Bucket::From100);
        // Two days of losing, then twelve hours of winning.
        let mut obs = episodes(&a, 576, 100.0, -3.0, 2);
        let recent = episodes(&a, 144, 100.0, 3.0, 2);
        for (i, mut o) in recent.into_iter().enumerate() {
            o.episode_ms = (576 + i as i64) * H;
            obs.push(o);
        }
        let now = 720 * H;
        let flat = size_arms(&obs, &[book("prlusdt")], now, &params());
        assert_eq!(find(&flat, "prlusdt", Side::Buy, Bucket::From100).share, Decimal::ZERO, "undecayed, still bad");
        let decayed = size_arms(&obs, &[book("prlusdt")], now, &Params { half_life_ms: 6 * 3_600_000 });
        let z = find(&decayed, "prlusdt", Side::Buy, Bucket::From100);
        assert!(z.yield_pm > 0.0, "a six-hour half-life sees the recovery: {z:?}");
        assert!(z.share > dec!(0.5), "{z:?}");
    }

    /// A side with almost no fills inherits its bucket. On the live data no
    /// side ever differed from its bucket beyond noise, so this is where a
    /// thin side gets its estimate from.
    #[test]
    fn a_thin_side_is_shrunk_to_its_bucket() {
        let buys = arm("prlusdc", Side::Buy, Bucket::From50);
        let sells = arm("prlusdc", Side::Sell, Bucket::From50);
        let mut obs = episodes(&buys, 240, 10.0, 3.0, 4); // clearly positive
        obs.extend(episodes(&sells, 240, 10.0, 0.0, 0)); // resting, never filled
        let zs = size_arms(&obs, &[book("prlusdc")], 240 * H, &params());
        let b = find(&zs, "prlusdc", Side::Buy, Bucket::From50);
        let s = find(&zs, "prlusdc", Side::Sell, Bucket::From50);
        assert!(b.yield_pm > 0.0);
        assert!(s.yield_pm > 0.0, "the thin side borrows its sibling's yield: {s:?}");
        assert!(s.yield_pm < b.yield_pm, "but its own empty hours still count: {s:?}");
        assert!(s.se_pm > 0.0);
        assert!(s.share > Decimal::ZERO && s.share < b.share, "and is sized from it, below it: {s:?} vs {b:?}");

        // A side never quoted at all takes the sibling's estimate outright,
        // widened by the difference the two are allowed.
        let only_buys = episodes(&buys, 240, 10.0, 3.0, 4);
        let zs = size_arms(&only_buys, &[book("prlusdc")], 240 * H, &params());
        let b = find(&zs, "prlusdc", Side::Buy, Bucket::From50);
        let s = find(&zs, "prlusdc", Side::Sell, Bucket::From50);
        assert_eq!(s.yield_pm, b.yield_pm);
        assert!(s.se_pm > b.se_pm, "sibling {} must be less certain than the source {}", s.se_pm, b.se_pm);
        assert_eq!(s.state, State::Sized, "the sibling's fills are its degrees of freedom");
    }

    /// One side's evidence must count once. Shrinking a side toward a pool
    /// that contains it tightened the posterior below the side's own error
    /// -- the first version of this reported a lower bound of +13 on an
    /// arm whose own error bar reached below zero.
    #[test]
    fn a_lone_side_is_not_more_certain_than_its_own_estimate() {
        let a = arm("prlusdc", Side::Sell, Bucket::From100);
        let mut obs = episodes(&a, 200, 10.0, 6.0, 2);
        for (i, o) in obs.iter_mut().enumerate() {
            if i % 4 == 2 {
                o.net = -5.0;
                o.fills = 1;
            }
        }
        let own = estimate(&obs.iter().collect::<Vec<_>>(), 200 * H, 0).unwrap();
        let zs = size_arms(&obs, &[book("prlusdc")], 200 * H, &params());
        let z = find(&zs, "prlusdc", Side::Sell, Bucket::From100);
        assert!((z.yield_pm - own.yield_pm).abs() < 1e-9);
        assert!((z.se_pm - own.se_pm).abs() < 1e-9, "posterior {} vs own {}", z.se_pm, own.se_pm);
    }

    /// And a side with strong evidence of its own keeps it: in a trend the
    /// lifted asks lose while the bids do not, and the arm must be able to
    /// say so.
    #[test]
    fn a_well_measured_side_can_separate_from_its_bucket() {
        let buys = arm("prlusdt", Side::Buy, Bucket::From100);
        let sells = arm("prlusdt", Side::Sell, Bucket::From100);
        let mut obs = episodes(&buys, 480, 50.0, 1.0, 1);
        obs.extend(episodes(&sells, 480, 50.0, -1.0, 1));
        let zs = size_arms(&obs, &[book("prlusdt")], 480 * H, &params());
        let b = find(&zs, "prlusdt", Side::Buy, Bucket::From100);
        let s = find(&zs, "prlusdt", Side::Sell, Bucket::From100);
        assert!(b.yield_pm > 0.0 && s.yield_pm < 0.0, "buy {} sell {}", b.yield_pm, s.yield_pm);
        assert!(b.share > dec!(0.5), "{b:?}");
        assert_eq!(s.share, Decimal::ZERO, "{s:?}");
    }

    /// Buckets are never pooled: a losing touch must not drag a paying far
    /// rung down, and a paying far rung must not lift the touch.
    #[test]
    fn buckets_are_judged_on_their_own() {
        let touch = arm("prlusdc", Side::Sell, Bucket::Under50);
        let far = arm("prlusdc", Side::Sell, Bucket::From200);
        let mut obs = episodes(&touch, 240, 5.0, -40.0, 2);
        obs.extend(episodes(&far, 240, 15.0, 4.0, 3));
        let zs = size_arms(&obs, &[book("prlusdc")], 240 * H, &params());
        assert_eq!(find(&zs, "prlusdc", Side::Sell, Bucket::Under50).share, Decimal::ZERO);
        assert!(find(&zs, "prlusdc", Side::Sell, Bucket::From200).share > dec!(0.95));
    }

    /// A storm nobody has measured borrows the calm's yield with a wider
    /// error bar, and so quotes LESS than the calm -- before the storm has
    /// cost anything. On the violent day the touch went from flat under a 2%
    /// range to -57 bp a fill above 4%: what the calm evidence says about a
    /// storm is exactly as uncertain as that.
    #[test]
    fn an_unmeasured_storm_quotes_less_than_the_calm_it_borrows_from() {
        let calm = arm_in("prlusdc", Side::Sell, Bucket::From100, Regime::Calm);
        let obs = episodes(&calm, 480, 10.0, 2.0, 2); // +100 ‰/h, well measured
        let zs = size_arms(&obs, &[book("prlusdc")], 480 * H, &params());
        let c = find_in(&zs, "prlusdc", Side::Sell, Bucket::From100, Regime::Calm);
        let w = find_in(&zs, "prlusdc", Side::Sell, Bucket::From100, Regime::Wild);
        assert!(c.share > dec!(0.95), "{c:?}");
        assert!((w.yield_pm - c.yield_pm).abs() < 1e-9, "the storm borrows the calm's number: {w:?} vs {c:?}");
        assert!(w.se_pm > c.se_pm * 1.5, "with a wider bar: {} vs {}", w.se_pm, c.se_pm);
        assert!(w.share > Decimal::ZERO && w.share < c.share, "and stakes less on it: {w:?}");
        assert_eq!(w.state, State::Sized);
    }

    /// Once the storm has its own fills they decide, and the calm keeps its
    /// size: the touch losing 57 bp a fill above a 4% range must not pull
    /// its calm cell, which was flat, down to nothing.
    #[test]
    fn a_storm_that_has_lost_is_sized_to_nothing_while_the_calm_keeps_its_size() {
        let calm = arm_in("prlusdt", Side::Buy, Bucket::Under50, Regime::Calm);
        let wild = arm_in("prlusdt", Side::Buy, Bucket::Under50, Regime::Wild);
        let mut obs = episodes(&calm, 480, 250.0, 2.0, 2); // +8 ‰/h
        obs.extend(episodes(&wild, 480, 250.0, -15.0, 2)); // -60 ‰/h
        let zs = size_arms(&obs, &[book("prlusdt")], 480 * H, &params());
        let c = find_in(&zs, "prlusdt", Side::Buy, Bucket::Under50, Regime::Calm);
        let w = find_in(&zs, "prlusdt", Side::Buy, Bucket::Under50, Regime::Wild);
        assert!(w.yield_pm < 0.0 && w.share == Decimal::ZERO, "{w:?}");
        assert!(c.yield_pm > 0.0 && c.share > dec!(0.9), "{c:?}");
        // And the regime in between, never measured, sits between them: it
        // borrows both, and the two disagree.
        let f = find_in(&zs, "prlusdt", Side::Buy, Bucket::Under50, Regime::Fast);
        assert!(f.yield_pm < c.yield_pm && f.yield_pm > w.yield_pm, "{f:?}");
        assert!(f.share < c.share, "{f:?}");
    }

    /// The regime edges are where the touch's markout changed on the violent
    /// day: 2% and 4% of price over five minutes.
    #[test]
    fn regime_edges_follow_the_measured_breaks() {
        assert_eq!(Regime::of_move(0.0), Regime::Calm);
        assert_eq!(Regime::of_move(0.0199), Regime::Calm);
        assert_eq!(Regime::of_move(0.02), Regime::Fast);
        assert_eq!(Regime::of_move(0.0399), Regime::Fast);
        assert_eq!(Regime::of_move(0.04), Regime::Wild);
        assert_eq!(Regime::of_move(0.7), Regime::Wild);
        for r in Regime::ALL {
            let l = r.label();
            assert!(!l.contains('<') && !l.contains('>') && !l.contains('&'), "label {l:?}");
        }
    }

    /// A table stored before the regime or the width existed still loads,
    /// as calm one-tick rows.
    #[test]
    fn an_arm_without_a_regime_or_width_reads_as_calm_and_one_tick() {
        let a: Arm = serde_json::from_str(r#"{"market":"prlusdt","side":"buy","bucket":"From100"}"#).unwrap();
        assert_eq!(a.regime, Regime::Calm);
        assert_eq!(a.width, Width::One);
    }

    /// The width edges sit between whole ticks, and a synthesised or
    /// crossed touch is the one-tick cell: nothing to make there either.
    #[test]
    fn width_edges_sit_between_whole_ticks() {
        assert_eq!(Width::of_ticks(0.0), Width::One);
        assert_eq!(Width::of_ticks(1.0), Width::One);
        assert_eq!(Width::of_ticks(1.49), Width::One);
        assert_eq!(Width::of_ticks(2.0), Width::Two);
        assert_eq!(Width::of_ticks(3.0), Width::Wide);
        assert_eq!(Width::of_ticks(40.0), Width::Wide);
        for w in Width::ALL {
            let l = w.label();
            assert!(!l.contains('<') && !l.contains('>') && !l.contains('&'), "label {l:?}");
        }
    }

    /// The finding this dimension exists for: prlusdt's 50-100 bp bucket
    /// lost 14 ‰/$h on a one-tick book and paid 146 on a three-tick one.
    /// The wide cell must be funded on its own fills while the one-tick
    /// cell, which has far more of them, rests the minimum -- and the
    /// bucket's other cells must not average the two into nothing.
    #[test]
    fn a_wide_book_that_pays_is_funded_while_the_one_tick_book_rests_the_minimum() {
        let tight = arm_on("prlusdt", Side::Buy, Bucket::From50, Width::One);
        let wide = arm_on("prlusdt", Side::Buy, Bucket::From50, Width::Wide);
        let mut obs = episodes(&tight, 480, 250.0, -4.0, 2); // -16 ‰/h, well measured
        obs.extend(episodes(&wide, 480, 20.0, 1.2, 2)); // +120 ‰/h on a tenth of the money
        let zs = size_arms(&obs, &[book("prlusdt")], 480 * H, &params());
        let t = find_on(&zs, "prlusdt", Side::Buy, Bucket::From50, Width::One);
        let w = find_on(&zs, "prlusdt", Side::Buy, Bucket::From50, Width::Wide);
        assert!(t.yield_pm < 0.0 && t.share == Decimal::ZERO, "{t:?}");
        assert!(w.yield_pm > 0.0 && w.share > dec!(0.9), "{w:?}");
        // The two-tick book, never measured, borrows both and sits between.
        let two = find_on(&zs, "prlusdt", Side::Buy, Bucket::From50, Width::Two);
        assert!(two.yield_pm > t.yield_pm && two.yield_pm < w.yield_pm, "{two:?}");
        assert!(two.share < w.share, "{two:?}");
    }

    /// A width nobody has measured borrows its bucket with a wider error
    /// bar, and so quotes less than the cell it borrows from: the same rule
    /// as an unmeasured storm.
    #[test]
    fn an_unmeasured_width_quotes_less_than_the_cell_it_borrows_from() {
        let one = arm_on("prlusdc", Side::Sell, Bucket::From200, Width::Wide);
        let obs = episodes(&one, 480, 10.0, 2.0, 2); // +100 ‰/h, well measured
        let zs = size_arms(&obs, &[book("prlusdc")], 480 * H, &params());
        let w = find_on(&zs, "prlusdc", Side::Sell, Bucket::From200, Width::Wide);
        let t = find_on(&zs, "prlusdc", Side::Sell, Bucket::From200, Width::One);
        assert!(w.share > dec!(0.95), "{w:?}");
        assert!((t.yield_pm - w.yield_pm).abs() < 1e-9, "{t:?} vs {w:?}");
        assert!(t.se_pm > w.se_pm * 1.5, "with a wider bar: {} vs {}", t.se_pm, w.se_pm);
        assert!(t.share > Decimal::ZERO && t.share < w.share, "{t:?}");
        // Differing on two dimensions widens more than differing on one.
        let far = find_cell(&zs, "prlusdc", Side::Sell, Bucket::From200, Regime::Wild, Width::One);
        assert!(far.se_pm > t.se_pm, "{far:?} vs {t:?}");
        assert!(far.share < t.share, "{far:?}");
    }

    /// Execution programs are not gated: getting to the target is the job.
    #[test]
    fn an_execution_program_is_never_gated() {
        let a = arm("prlusdt", Side::Sell, Bucket::Under50);
        let obs = episodes(&a, 240, 250.0, -8.0, 2);
        let zs = size_arms(&obs, &[Book { market: "prlusdt", execution: true }], 240 * H, &params());
        let z = find(&zs, "prlusdt", Side::Sell, Bucket::Under50);
        assert_eq!(z.state, State::Execution);
        assert_eq!(z.share, Decimal::ONE);
        assert!(z.yield_pm < 0.0, "the measurement is still reported");
    }

    /// With nothing observed, every arm has no opinion. The minimum order is
    /// the quote layer's to add; here the share is exactly zero.
    #[test]
    fn nothing_observed_is_no_opinion_everywhere() {
        let zs = size_arms(&[], &[book("prlusdt")], 0, &params());
        assert_eq!(zs.len(), 90, "two sides, five buckets, three regimes, three widths");
        assert!(zs.iter().all(|z| z.share == Decimal::ZERO && z.state == State::NoData && z.p_pays == 0.5));
    }

    /// The share is the net odds: nothing at an even chance, 68% at one
    /// sigma, 95% at two, and never a step in between.
    #[test]
    fn the_share_is_the_net_odds_of_paying() {
        assert_eq!(net_odds(0.5), Decimal::ZERO);
        assert_eq!(net_odds(0.2), Decimal::ZERO);
        assert_eq!(net_odds(1.0), Decimal::ONE);
        assert!((net_odds(0.8413) - dec!(0.6826)).abs() < dec!(0.001));
        assert!((net_odds(0.9772) - dec!(0.9544)).abs() < dec!(0.001));
        let mut last = Decimal::ZERO;
        for i in 0..=100 {
            let s = net_odds(0.5 + 0.005 * i as f64);
            assert!(s >= last, "monotone: {s} after {last}");
            assert!(s - last <= dec!(0.011), "no jumps: {s} after {last}");
            last = s;
        }
    }

    /// The t distribution, against tabulated values.
    #[test]
    fn the_student_t_matches_the_tables() {
        let close = |a: f64, b: f64| (a - b).abs() < 2e-4;
        assert!(close(student_t_cdf(0.0, 1.0), 0.5));
        assert!(close(student_t_cdf(1.0, 1.0), 0.75));
        assert!(close(student_t_cdf(6.314, 1.0), 0.95));
        assert!(close(student_t_cdf(2.920, 2.0), 0.95));
        assert!(close(student_t_cdf(1.812, 10.0), 0.95));
        assert!(close(student_t_cdf(-1.812, 10.0), 0.05));
        assert!(close(student_t_cdf(1.96, 1e6), 0.975));
        assert_eq!(student_t_cdf(f64::INFINITY, 3.0), 1.0);
    }

    /// The rung lookup: a rung's distance, the market's range and the
    /// book's width pick its arm, and a rung whose arm has no entry gets the
    /// fallback rather than full size.
    #[test]
    fn scales_follow_distance_regime_and_width_and_default_small() {
        let s = ArmScales::none()
            .with(Side::Buy, Bucket::From200, Decimal::ONE)
            .with_regime(Side::Buy, Bucket::From200, Regime::Wild, dec!(0.2))
            .with_width(Side::Buy, Bucket::From200, Width::Wide, dec!(0.7))
            .with(Side::Sell, Bucket::From200, dec!(0.4));
        let one = Decimal::ONE;
        assert_eq!(s.scale(Side::Buy, dec!(250), dec!(0.01), one), Decimal::ONE);
        assert_eq!(s.scale(Side::Buy, dec!(250), dec!(0.03), one), Decimal::ONE, "fast: set by `with`");
        assert_eq!(s.scale(Side::Buy, dec!(250), dec!(0.05), one), dec!(0.2), "wild: its own cell");
        assert_eq!(s.scale(Side::Buy, dec!(250), dec!(0.05), dec!(3)), dec!(0.7), "wide: its own cell");
        assert_eq!(s.scale(Side::Buy, dec!(250), dec!(0.01), dec!(2)), Decimal::ONE, "two ticks: set by `with`");
        assert_eq!(s.scale(Side::Sell, dec!(399), Decimal::ZERO, one), dec!(0.4));
        assert_eq!(s.scale(Side::Buy, dec!(30), Decimal::ZERO, one), Decimal::ZERO, "no entry: the minimum");
        assert_eq!(s.scale(Side::Buy, dec!(-20), Decimal::ZERO, one), Decimal::ZERO, "through the mid is the touch bucket");
        assert_eq!(ArmScales::full().scale(Side::Sell, dec!(1000), dec!(0.5), dec!(40)), Decimal::ONE);
    }

    #[test]
    fn bucket_edges_follow_the_tick() {
        assert_eq!(Bucket::of_bps(0.0), Bucket::Under50);
        assert_eq!(Bucket::of_bps(49.9), Bucket::Under50);
        assert_eq!(Bucket::of_bps(50.0), Bucket::From50);
        assert_eq!(Bucket::of_bps(199.0), Bucket::From100);
        assert_eq!(Bucket::of_bps(200.0), Bucket::From200);
        assert_eq!(Bucket::of_bps(5000.0), Bucket::From400);
    }

    /// Funding follows the best arm on the side: a book with one nearly
    /// full-size arm wants a full ladder's money, and a book with nothing
    /// proven wants none.
    #[test]
    fn the_spend_weight_is_the_best_arm_on_the_side() {
        let a = arm("prlusdc", Side::Sell, Bucket::From200);
        let mut obs = episodes(&a, 240, 15.0, 4.0, 3);
        obs.extend(episodes(&arm("prlusdc", Side::Sell, Bucket::Under50), 240, 5.0, -40.0, 2));
        let zs = size_arms(&obs, &[book("prlusdc"), book("prlusdt")], 240 * H, &params());
        assert!(spend_weight(&zs, "prlusdc", Side::Sell).unwrap() > dec!(0.95), "the far rung, not the touch");
        assert_eq!(spend_weight(&zs, "prlusdt", Side::Sell), Some(Decimal::ZERO));
        assert_eq!(spend_weight(&zs, "prlusdt", Side::Buy), Some(Decimal::ZERO));
        assert_eq!(spend_weight(&zs, "nope", Side::Sell), None);
    }
}
