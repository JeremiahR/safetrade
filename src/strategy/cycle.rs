//! The margin on a whole cycle, not on one leg of it.
//!
//! A CYCLE is how the base asset is acquired cheaply on one book and released
//! on another: acquire PRL with USDC on the thin book, release it for USDT on
//! the deep one, recycle the USDT back to USDC, repeat. Each leg is already
//! scored honestly on its own -- `Db::scoring_mid` values a referenced book's
//! fills against the deep book's mid through the stablecoin cross, so a
//! prlusdc fill is measured against where PRL is actually priced, not against
//! its own stale midpoint. The legs therefore ADD UP, and that is the whole
//! basis of this module: the sum of the legs is the cycle.
//!
//! What it exists to fix is not the accounting, which is right. It is the
//! DECISION. The arms size each arm on that arm's own yield per dollar-hour,
//! which is correct for an arm that stands alone and wrong for a leg that
//! only exists to unlock another one. Measured live over 3.7 days of maker
//! fills, net of fees, every figure against the reference mid five minutes
//! on:
//!
//!   prlusdc  buy    +85 bp   n=90    acquire below fair value
//!   prlusdc  sell  +209 bp   n=80    release above it
//!   prlusdt  sell   -17 bp  n=109    release at fair value
//!   prlusdt  buy    -22 bp  n=164    a one-tick book, swept
//!   usdcusdt buy     -1 bp   n=20    the recycle leg
//!
//! Read as arms, prlusdt's sell loses 17 bp and the arms defund it -- they
//! did, to 0% of size, which is correct in isolation and shuts the cycle's
//! release valve. Read as cycles:
//!
//!   acquire prlusdc, release prlusdc   +85 +209        = +294 bp
//!   acquire prlusdc, release prlusdt   +85 -17 -24     =  +44 bp
//!
//! Both pay. The second pays LESS than half a percent where the first pays
//! nearly three, so the thin book's own ask is always preferred and the deep
//! book is strictly an overflow: the thin book's flow is what it is, and when
//! its bid fills faster than its ask can release, the choice is not between
//! -17 bp and zero, it is between -17 bp and forfeiting the +85 bp that comes
//! with acquiring at all. That is the trade an arm cannot see and this module
//! can.
//!
//! The recycle leg is charged at what it costs to CROSS, not at what a maker
//! would earn, because waiting for a maker fill on it is what stalls the
//! cycle. On usdcusdt that is about 24 bp -- half of a 29 bp spread plus the
//! 10 bp fee -- against $1.05M of ask depth and $2.16M of bid depth, on a
//! pair whose mid has stayed inside 0.9981-1.0041. Paying it is a quarter of
//! the edge it unlocks and it cannot move the price. That is the one place
//! crossing is allowed; see `Quoting::allow_taker`.
//!
//! Thin samples must not open a valve. Every leg carries the standard error
//! of its own mean and a cycle is only acted on when its margin is clear of
//! the floor by `SE_MULTIPLE` standard errors, so a leg measured on four
//! fills cannot vote until it has earned the right to.

use std::collections::HashMap;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::exchange::Side;

/// Standard errors of margin a cycle must clear its floor by before it is
/// acted on.
///
/// The arms use the same idea for the same reason: a cell with one fill
/// reports a number, not evidence. Two is deliberately mild -- this gates
/// whether a leg is ALLOWED to rest, not how large it rests, and the size
/// still comes from the arms.
pub const SE_MULTIPLE: Decimal = dec!(2);

/// The default margin a cycle must clear before it is acted on.
///
/// Not zero. A cycle that pays 5 bp is inside the noise of every figure that
/// goes into it, and acting on it spends real fees and real queue priority to
/// find that out.
pub const DEFAULT_FLOOR_BPS: Decimal = dec!(20);

/// The exchange's fee, in basis points, charged on every fill either side of
/// the book. Fees are otherwise read from the fills themselves; this is only
/// needed to price a crossing that has not happened yet.
pub const FEE_BPS: Decimal = dec!(10);

/// What crossing the spread on the recycle leg costs, in basis points, when
/// the book has not been measured yet. Half of usdcusdt's observed 29 bp
/// spread plus the 10 bp fee.
pub const DEFAULT_RECYCLE_BPS: Decimal = dec!(24);

/// One measured leg: a market and a side, valued against the reference mid.
#[derive(Debug, Clone, PartialEq)]
pub struct Leg {
    pub market: String,
    pub side: Side,
    /// Net markout per unit traded, in basis points, after fees.
    pub net_bps: Decimal,
    /// Standard error of `net_bps`.
    pub se_bps: Decimal,
    /// Quote-denominated volume behind the estimate.
    pub volume: Decimal,
    /// Fills behind the estimate.
    pub n: u32,
}

impl Leg {
    /// The leg's yield discounted for how little we know about it. A leg with
    /// no fills is worth nothing rather than zero, which matters because zero
    /// would read as "free" on a release leg.
    pub fn lower_bound(&self) -> Decimal {
        if self.n == 0 {
            return Decimal::ZERO;
        }
        self.net_bps - SE_MULTIPLE * self.se_bps
    }

    pub fn measured(&self) -> bool {
        self.n > 0
    }
}

/// Where the base asset is acquired, where it can be released, and what each
/// of those has paid.
#[derive(Debug, Clone, Default)]
pub struct Cycle {
    /// The book the cycle acquires the base asset on.
    pub acquire: String,
    /// Legs by (market, side).
    pub legs: HashMap<(String, Side), Leg>,
    /// Cost in basis points of crossing the recycle book, per cycle.
    pub recycle_bps: Decimal,
    /// The quote currency of each market, so a release that lands in the
    /// wrong currency is charged for the swap back.
    pub quote_of: HashMap<String, String>,
    /// The base asset of each market.
    ///
    /// A release venue has to trade the asset being released. Without this the
    /// stablecoin book qualified as one -- it has sell fills and a quote
    /// currency, so every arithmetic test passed -- and the report offered
    /// usdcusdt as somewhere to sell PRL. Nothing would have executed, because
    /// the engine quotes per market, but a report that suggests an impossible
    /// route is worse than one that says nothing.
    pub base_of: HashMap<String, String>,
}

impl Cycle {
    pub fn leg(&self, market: &str, side: Side) -> Option<&Leg> {
        self.legs.get(&(market.to_string(), side))
    }

    /// Can the asset the cycle acquires actually be released on `market`?
    pub fn releases_here(&self, market: &str) -> bool {
        match (self.base_of.get(market), self.base_of.get(&self.acquire)) {
            (Some(a), Some(b)) => a == b,
            // Unknown metadata: only the acquiring book itself is certain.
            _ => market == self.acquire,
        }
    }

    /// Does releasing on `market` need the quote currency swapped back to the
    /// one the acquiring book spends?
    pub fn needs_recycle(&self, market: &str) -> bool {
        match (self.quote_of.get(market), self.quote_of.get(&self.acquire)) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        }
    }

    /// Basis points the whole cycle pays, releasing on `market`: what the
    /// acquisition earned, plus what the release earned or cost, less the
    /// swap back if the release lands in the wrong currency.
    ///
    /// `None` when either leg has never been measured -- an unmeasured
    /// release leg is exactly the case where guessing zero would open a valve
    /// on no evidence.
    pub fn margin_bps(&self, market: &str) -> Option<Decimal> {
        if !self.releases_here(market) {
            return None;
        }
        let acquire = self.leg(&self.acquire, Side::Buy)?;
        let release = self.leg(market, Side::Sell)?;
        if !acquire.measured() || !release.measured() {
            return None;
        }
        let recycle = if self.needs_recycle(market) { self.recycle_bps } else { Decimal::ZERO };
        Some(acquire.net_bps + release.net_bps - recycle)
    }

    /// The same figure, discounted by each leg's standard error. This is what
    /// a decision is taken on.
    pub fn margin_lower_bound(&self, market: &str) -> Option<Decimal> {
        if !self.releases_here(market) {
            return None;
        }
        let acquire = self.leg(&self.acquire, Side::Buy)?;
        let release = self.leg(market, Side::Sell)?;
        if !acquire.measured() || !release.measured() {
            return None;
        }
        let recycle = if self.needs_recycle(market) { self.recycle_bps } else { Decimal::ZERO };
        // Errors on independent legs add in quadrature, not linearly: charging
        // the sum of them would keep a two-leg cycle shut on evidence that is
        // plenty for either leg alone.
        let var = acquire.se_bps * acquire.se_bps + release.se_bps * release.se_bps;
        Some(acquire.net_bps + release.net_bps - recycle - SE_MULTIPLE * sqrt(var))
    }

    /// Whether the cycle clears `floor_bps` on the evidence, releasing on
    /// `market`.
    pub fn pays(&self, market: &str, floor_bps: Decimal) -> bool {
        self.margin_lower_bound(market).is_some_and(|m| m > floor_bps)
    }

    /// The release venue that pays most, among those that clear the floor.
    /// The acquiring book's own ask is included and normally wins by a wide
    /// margin, which is the point: the deep book is an overflow, never a
    /// preference.
    pub fn best_release(&self, floor_bps: Decimal) -> Option<(String, Decimal)> {
        let mut best: Option<(String, Decimal)> = None;
        for (market, side) in self.legs.keys() {
            if *side != Side::Sell {
                continue;
            }
            let Some(m) = self.margin_lower_bound(market) else { continue };
            if m <= floor_bps {
                continue;
            }
            if best.as_ref().is_none_or(|(_, b)| m > *b) {
                best = Some((market.clone(), m));
            }
        }
        best
    }

    /// Whether it is worth crossing the recycle book to refill the currency
    /// the acquiring book spends. True only when the cycle still pays after
    /// the crossing is charged, which is the whole of the owner's rule: lose
    /// a little on a leg if the whole trade works.
    pub fn recycle_worth_taking(&self, release_market: &str, floor_bps: Decimal) -> bool {
        self.needs_recycle(release_market) && self.pays(release_market, floor_bps)
    }

    /// Whether swapping INTO the currency the acquiring book spends pays for
    /// itself out of that book's own round trip.
    ///
    /// `recycle_worth_taking` answers a narrower question: we were paid in the
    /// wrong currency by a release on the deep book, is it worth swapping back.
    /// That question can only ever be asked about money the cycle itself just
    /// earned, so while the best release is the thin book's own ask -- which
    /// lands in the right currency already -- it is always false and nothing is
    /// ever swapped. Meanwhile the account can be holding the wrong stablecoin
    /// for an entirely different reason (a release leg that has since been shut,
    /// or a deposit) while the bids that earn the acquisition edge sit starved.
    ///
    /// This is that case. The crossing is charged against the acquiring book's
    /// OWN round trip, at the same floor and on the same discounted evidence:
    /// pay 24 bp once to move idle capital onto a leg whose round trip clears
    /// its floor by more than that, or do not move it. Measured 2026-09-27,
    /// prlusdc's round trip is +226 bp on the evidence against a 24 bp crossing
    /// and a 20 bp floor, so the swap is worth roughly ten times its cost --
    /// and if that edge ever goes, this goes with it and the capital stays put.
    pub fn swap_funds_acquire(&self, floor_bps: Decimal) -> bool {
        self.margin_lower_bound(&self.acquire)
            .is_some_and(|m| m > floor_bps + self.recycle_bps)
    }
}

/// The same square root, for `Db::cycle_legs`, which needs it to turn a
/// variance into a standard error while building the legs this module reads.
pub fn sqrt_for_db(v: Decimal) -> Decimal {
    sqrt(v)
}

/// Integer-ish square root for Decimal, by Newton's method.
///
/// `Decimal` has no sqrt and the values here are variances in basis points,
/// so a handful of iterations from a scaled start is far more precision than
/// a standard error needs.
fn sqrt(v: Decimal) -> Decimal {
    if v <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let mut x = if v > Decimal::ONE { v / dec!(2) + dec!(0.5) } else { Decimal::ONE };
    for _ in 0..40 {
        if x <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let next = (x + v / x) / dec!(2);
        if (next - x).abs() < dec!(0.000001) {
            return next;
        }
        x = next;
    }
    x
}

/// Build the cycle from measured legs.
///
/// `legs` is whatever the database reports for the window; `acquire` is the
/// book the cycle buys the base asset on, `quote_of` maps each market to its
/// quote currency, and `recycle_bps` is what crossing the recycle book costs
/// right now.
pub fn estimate(
    acquire: &str,
    legs: Vec<Leg>,
    quote_of: HashMap<String, String>,
    base_of: HashMap<String, String>,
    recycle_bps: Decimal,
) -> Cycle {
    Cycle {
        acquire: acquire.to_string(),
        legs: legs.into_iter().map(|l| ((l.market.clone(), l.side), l)).collect(),
        recycle_bps: if recycle_bps > Decimal::ZERO { recycle_bps } else { DEFAULT_RECYCLE_BPS },
        quote_of,
        base_of,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(market: &str, side: Side, net: Decimal, se: Decimal, n: u32) -> Leg {
        Leg { market: market.into(), side, net_bps: net, se_bps: se, volume: dec!(1000), n }
    }

    fn quotes() -> HashMap<String, String> {
        [("prlusdc", "usdc"), ("prlusdt", "usdt"), ("usdcusdt", "usdt")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn bases() -> HashMap<String, String> {
        [("prlusdc", "prl"), ("prlusdt", "prl"), ("usdcusdt", "usdc")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// The account as measured on 2026-09-26: 3.7 days of maker fills, every
    /// one scored against the reference mid five minutes on, net of fees,
    /// with the standard error of each leg's volume-weighted mean bootstrapped
    /// over its own fills.
    ///
    ///   prlusdc buy    +85 +/- 24   n=90
    ///   prlusdc sell  +209 +/- 19   n=80
    ///   prlusdt sell   -17 +/- 39  n=109
    fn measured() -> Cycle {
        estimate(
            "prlusdc",
            vec![
                leg("prlusdc", Side::Buy, dec!(85), dec!(24), 90),
                leg("prlusdc", Side::Sell, dec!(209), dec!(19), 80),
                leg("prlusdt", Side::Sell, dec!(-17), dec!(39), 109),
            ],
            quotes(),
            bases(),
            dec!(24),
        )
    }

    /// Both routes pay on the point estimate, and the thin book's own ask pays
    /// nearly seven times more, so the deep book can only ever be an overflow.
    #[test]
    fn the_thin_book_s_own_ask_wins_and_the_deep_one_still_pays_on_the_estimate() {
        let c = measured();
        assert_eq!(c.margin_bps("prlusdc"), Some(dec!(294)), "85 + 209, no swap back");
        assert_eq!(c.margin_bps("prlusdt"), Some(dec!(44)), "85 - 17 - 24 to swap back");
        assert!(!c.needs_recycle("prlusdc"), "same quote currency, nothing to swap");
        assert!(c.needs_recycle("prlusdt"), "lands in USDT, the thin book spends USDC");
        // And the deep book's leg, judged alone, is the loser the arms
        // correctly see and correctly defund.
        assert!(c.leg("prlusdt", Side::Sell).unwrap().net_bps < Decimal::ZERO);
    }

    /// THE POINT OF THE MODULE: the thin book's round trip is established far
    /// beyond noise, so it opens wide.
    #[test]
    fn the_thin_book_s_round_trip_is_established_beyond_any_sane_floor() {
        let c = measured();
        // sqrt(24^2 + 19^2) = 30.6, so 294 - 2*30.6 = 232.8.
        let lb = c.margin_lower_bound("prlusdc").unwrap();
        assert!(lb > dec!(230) && lb < dec!(234), "{lb}");
        assert!(c.pays("prlusdc", dec!(100)), "clears even a 100 bp floor");
        let (best, margin) = c.best_release(dec!(20)).unwrap();
        assert_eq!(best, "prlusdc", "the thin book's own ask is always preferred");
        assert!(margin > dec!(230), "{margin}");
    }

    /// AND THE HONEST OTHER HALF: the deep-book release is NOT established on
    /// today's evidence, and must stay shut.
    ///
    /// The point estimate is +44 bp, which is worth having. But prlusdt's sell
    /// leg is -17 +/- 39 on 109 fills: sqrt(24^2 + 39^2) = 45.8, so the lower
    /// bound is 44 - 91.6 = -47.6. The valve is built and it stays closed
    /// until that leg is measured better, which is the whole difference
    /// between this and a hunch.
    #[test]
    fn the_deep_book_release_stays_shut_until_its_leg_is_actually_measured() {
        let c = measured();
        let lb = c.margin_lower_bound("prlusdt").unwrap();
        assert!(lb < Decimal::ZERO, "not established on today's evidence: {lb}");
        assert!(!c.pays("prlusdt", Decimal::ZERO), "so it must not rest");
        assert!(!c.recycle_worth_taking("prlusdt", Decimal::ZERO), "and nothing is swapped for it");

        // Note WHICH leg is short of evidence: 2 SE of the ACQUIRE leg alone
        // is 48 bp, already more than the +44 bp the cycle pays. So the deep
        // release cannot be established by measuring prlusdt better; both legs
        // have to tighten, which is a few more days of fills on each and not a
        // setting anyone can type. Give them that and the valve opens itself.
        let mut better = measured();
        better.legs.insert(
            ("prlusdc".into(), Side::Buy),
            leg("prlusdc", Side::Buy, dec!(85), dec!(8), 800),
        );
        better.legs.insert(
            ("prlusdt".into(), Side::Sell),
            leg("prlusdt", Side::Sell, dec!(-17), dec!(6), 900),
        );
        // sqrt(8^2 + 6^2) = 10, so 44 - 20 = +24.
        let lb = better.margin_lower_bound("prlusdt").unwrap();
        assert!((lb - dec!(24)).abs() < dec!(0.01), "{lb}");
        assert!(better.pays("prlusdt", dec!(20)), "evidence, not a new setting, opens it");
        assert!(better.recycle_worth_taking("prlusdt", dec!(20)));
    }

    /// The valve opens on the CYCLE, not on the leg: a leg that loses money is
    /// worth resting when the acquisition it unlocks pays for it, and stops
    /// being worth it the moment that acquisition edge goes.
    #[test]
    fn a_losing_leg_is_rested_only_while_the_cycle_it_completes_pays() {
        let mut c = measured();
        // Both legs measured well enough to be allowed to vote.
        c.legs
            .insert(("prlusdc".into(), Side::Buy), leg("prlusdc", Side::Buy, dec!(85), dec!(8), 800));
        c.legs.insert(
            ("prlusdt".into(), Side::Sell),
            leg("prlusdt", Side::Sell, dec!(-17), dec!(6), 900),
        );
        assert!(c.pays("prlusdt", dec!(20)), "-17 bp leg rests, because +85 unlocks it");

        // Take the acquisition edge away, keeping the evidence, and the
        // identical leg must shut: then it really is just a losing leg.
        c.legs.insert(("prlusdc".into(), Side::Buy), leg("prlusdc", Side::Buy, dec!(5), dec!(2), 800));
        assert!(!c.pays("prlusdt", dec!(20)), "5 - 17 - 24 pays nobody");
        assert_eq!(c.margin_bps("prlusdt"), Some(dec!(-36)));
    }

    /// A leg nobody has measured must not open a valve, and must not be
    /// treated as free.
    #[test]
    fn an_unmeasured_leg_cannot_vote() {
        let mut c = measured();
        c.legs
            .insert(("prlusdt".into(), Side::Sell), leg("prlusdt", Side::Sell, dec!(500), dec!(0), 0));
        assert_eq!(c.margin_bps("prlusdt"), None, "no fills, no opinion");
        assert!(!c.pays("prlusdt", Decimal::ZERO));
        assert_eq!(leg("x", Side::Sell, dec!(500), dec!(0), 0).lower_bound(), Decimal::ZERO);
        // A release venue we have never sold on is simply absent.
        assert_eq!(c.margin_bps("nosuchmarket"), None);
        // As is a cycle whose acquiring book has never bought.
        let no_acquire = estimate("prlusdc", vec![leg("prlusdt", Side::Sell, dec!(10), dec!(1), 50)], quotes(), bases(), dec!(24));
        assert_eq!(no_acquire.margin_bps("prlusdt"), None);
        assert_eq!(no_acquire.best_release(Decimal::ZERO), None);
    }

    /// Two legs' errors add in quadrature, not linearly: charging the sum
    /// would keep a two-leg cycle shut on evidence that is plenty for either
    /// leg alone.
    #[test]
    fn the_legs_errors_add_in_quadrature() {
        let thin = estimate(
            "prlusdc",
            vec![
                leg("prlusdc", Side::Buy, dec!(85), dec!(60), 3),
                leg("prlusdt", Side::Sell, dec!(-17), dec!(50), 2),
            ],
            quotes(),
            bases(),
            dec!(24),
        );
        assert_eq!(thin.margin_bps("prlusdt"), Some(dec!(44)), "same point estimate");
        assert!(!thin.pays("prlusdt", dec!(20)), "but nothing like enough evidence");
        let lb = thin.margin_lower_bound("prlusdt").unwrap();
        let linear = dec!(44) - SE_MULTIPLE * dec!(110);
        assert!(lb > linear, "quadrature, not a linear sum: {lb} vs {linear}");
        assert!((lb - (dec!(44) - SE_MULTIPLE * dec!(78.1025))).abs() < dec!(0.01), "{lb}");
    }

    /// Crossing the recycle book is only worth it when the cycle still pays
    /// with the crossing charged -- and it is never needed when the release
    /// already lands in the currency the acquiring book spends.
    #[test]
    fn the_swap_back_is_paid_for_only_when_the_whole_trade_works() {
        let mut c = measured();
        c.legs
            .insert(("prlusdc".into(), Side::Buy), leg("prlusdc", Side::Buy, dec!(85), dec!(8), 800));
        c.legs.insert(
            ("prlusdt".into(), Side::Sell),
            leg("prlusdt", Side::Sell, dec!(-17), dec!(6), 900),
        );
        assert!(c.recycle_worth_taking("prlusdt", dec!(20)), "lose 24 bp, the trade still works");
        assert!(!c.recycle_worth_taking("prlusdc", dec!(20)), "nothing to swap");

        // A recycle cost that eats the margin shuts it, with no threshold
        // retyped anywhere.
        let dear = estimate(
            "prlusdc",
            vec![
                leg("prlusdc", Side::Buy, dec!(85), dec!(10), 90),
                leg("prlusdt", Side::Sell, dec!(-17), dec!(8), 109),
            ],
            quotes(),
            bases(),
            dec!(90),
        );
        assert_eq!(dear.margin_bps("prlusdt"), Some(dec!(-22)));
        assert!(!dear.recycle_worth_taking("prlusdt", Decimal::ZERO));
    }

    /// The starved-bids case: the thin book's own round trip pays for the
    /// crossing that funds it, even though no release ever lands in the wrong
    /// currency and so `recycle_worth_taking` is false everywhere.
    ///
    /// This is what was measured on 2026-09-27: USDT sitting idle, USDC
    /// about a fifth of what the bids wanted, and the only route between them
    /// a 24 bp crossing.
    #[test]
    fn funding_the_starved_bids_justifies_the_swap_on_its_own() {
        let c = measured();
        // Nothing is worth swapping BACK: the best release is the thin book's
        // own ask, which is already paid in the currency its bids spend.
        assert_eq!(c.best_release(dec!(20)).map(|(m, _)| m), Some("prlusdc".to_string()));
        assert!(!c.recycle_worth_taking("prlusdc", dec!(20)), "nothing to swap back");
        assert!(!c.recycle_worth_taking("prlusdt", dec!(20)), "and that valve is shut");
        // But the round trip that the USDC would fund pays +232 bp on the
        // evidence, against a 20 bp floor plus a 24 bp crossing.
        assert!(c.swap_funds_acquire(dec!(20)), "so the swap pays for itself");

        // It is charged, not assumed: a crossing dearer than the round trip
        // clears shuts it, with no threshold retyped.
        let dear = estimate(
            "prlusdc",
            vec![
                leg("prlusdc", Side::Buy, dec!(85), dec!(24), 90),
                leg("prlusdc", Side::Sell, dec!(209), dec!(19), 80),
            ],
            quotes(),
            bases(),
            dec!(300),
        );
        assert!(!dear.swap_funds_acquire(dec!(20)), "232 bp does not pay a 300 bp crossing");

        // And the acquisition edge going takes it with it.
        let mut gone = measured();
        gone.legs
            .insert(("prlusdc".into(), Side::Sell), leg("prlusdc", Side::Sell, dec!(5), dec!(2), 80));
        assert!(!gone.swap_funds_acquire(dec!(20)), "no edge, no reason to move capital");

        // An unmeasured round trip cannot open it either.
        let unmeasured = estimate("prlusdc", vec![], quotes(), bases(), dec!(24));
        assert!(!unmeasured.swap_funds_acquire(Decimal::ZERO));
    }

    /// An unset recycle cost falls back to the measured spread, never to free.
    #[test]
    fn an_unpriced_swap_back_is_not_free() {
        let c = estimate("prlusdc", vec![], quotes(), bases(), Decimal::ZERO);
        assert_eq!(c.recycle_bps, DEFAULT_RECYCLE_BPS);
    }

    #[test]
    fn sqrt_is_close_enough_for_a_standard_error() {
        for v in [dec!(0), dec!(1), dec!(4), dec!(100), dec!(6100), dec!(0.25)] {
            let r = sqrt(v);
            assert!((r * r - v).abs() < dec!(0.0001), "sqrt({v}) = {r}");
        }
        assert_eq!(sqrt(dec!(-5)), Decimal::ZERO);
    }
}
