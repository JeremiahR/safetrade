//! Decimal helpers. Every price and amount in this bot is a `Decimal`; f64
//! rounding on a market with a 2dp price precision is the difference between an
//! order resting and an order rejected.

use rust_decimal::{Decimal, RoundingStrategy};
use rust_decimal_macros::dec;

/// Round `v` down to `dp` decimal places. Used for sizes, where rounding up
/// risks exceeding a balance, and for bid prices, where rounding up risks
/// paying a tick more than intended.
pub fn floor_to(v: Decimal, dp: u32) -> Decimal {
    v.round_dp_with_strategy(dp, RoundingStrategy::ToZero)
}

/// Round `v` up to `dp` decimal places. Used for ask prices.
pub fn ceil_to(v: Decimal, dp: u32) -> Decimal {
    v.round_dp_with_strategy(dp, RoundingStrategy::AwayFromZero)
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn round_to(v: Decimal, dp: u32) -> Decimal {
    v.round_dp_with_strategy(dp, RoundingStrategy::MidpointAwayFromZero)
}

/// The smallest price increment for a market with `dp` price decimals.
/// On PRL/USDT (`dp = 2`) this is 0.01, which is about 1% of the price — the
/// single most important number in this bot.
pub fn tick(dp: u32) -> Decimal {
    Decimal::new(1, dp)
}

pub fn clamp(v: Decimal, lo: Decimal, hi: Decimal) -> Decimal {
    if v < lo { lo } else if v > hi { hi } else { v }
}

/// Parse a decimal string from the DB or the exchange, tolerating empty/null.
pub fn parse(s: &str) -> Decimal {
    s.trim().parse::<Decimal>().unwrap_or_default()
}

pub fn opt_parse(s: Option<&str>) -> Option<Decimal> {
    s.and_then(|s| s.trim().parse::<Decimal>().ok())
}

/// Canonical string form for storage: no exponent, no trailing-zero surprises.
pub fn to_str(v: Decimal) -> String {
    v.normalize().to_string()
}

/// The widest spread we will believe is really on offer, as a fraction.
///
/// Beyond this, the quotes are a stale residue rather than a price anyone would
/// cross. prlusdc posted 12.38% while we were its only real liquidity.
pub const MAX_CREDIBLE_SPREAD: Decimal = dec!(0.05);

/// A spread we could actually trade against. Zero where the book is posting a
/// number nobody would ever cross.
///
/// Only the report reads this now, for its "capture" column: measured against
/// a 12.38% phantom, good quoting on prlusdc read as an 8% capture rate. No
/// capital is allocated from it -- that follows the arms.
pub fn tradeable_spread(spread_frac: Decimal) -> Decimal {
    if spread_frac > Decimal::ZERO && spread_frac <= MAX_CREDIBLE_SPREAD {
        spread_frac
    } else {
        Decimal::ZERO
    }
}

#[cfg(test)]
mod spread_tests {
    use super::*;

    /// A spread nobody would cross is not a spread we are measured against.
    ///
    /// prlusdc posted 12.38% while we were its only real liquidity; scored
    /// against it, the report called good quoting an 8% capture.
    #[test]
    fn a_phantom_spread_scores_as_no_opportunity() {
        assert_eq!(tradeable_spread(dec!(0.1238)), Decimal::ZERO, "the prlusdc phantom");
        assert_eq!(tradeable_spread(dec!(0.0151)), dec!(0.0151), "prlusdt, real");
        assert_eq!(tradeable_spread(dec!(0.0033)), dec!(0.0033), "usdcusdt, real");
        assert_eq!(tradeable_spread(MAX_CREDIBLE_SPREAD), MAX_CREDIBLE_SPREAD, "the edge counts");
        assert_eq!(tradeable_spread(Decimal::ZERO), Decimal::ZERO, "unmeasured");
        assert_eq!(tradeable_spread(dec!(-0.01)), Decimal::ZERO, "a crossed book is not a spread");
    }
}
