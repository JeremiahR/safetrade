//! Where the capital should sit, learned from the arms.
//!
//! The inventory target used to be a number typed into a program: hold
//! a fixed quantity of PRL. The arms already measure where the yield is, so the split of
//! the account between PRL, USDT and USDC follows them instead, and the
//! owner deposits capital rather than targets.
//!
//! What a currency is FOR, here, is funding one side of a ladder:
//! a bid on prlusdt spends USDT, a bid on prlusdc spends USDC, an ask on
//! either spends PRL, and usdcusdt trades the two stablecoins against each
//! other. A side whose arms have earned size needs its currency on hand or
//! the ladder cannot rest at that size; a side whose arms have earned nothing
//! rests the exchange minimum and needs almost none. So each currency's
//! DEMAND is the sum, over the sides that spend it, of the size those sides'
//! arms have earned (`spend_weight`, the best share on the side, in [0, 1])
//! times what the side needs on hand for its ladder to rest at full size.
//! That last number is not typed here: it is derived from the settings the
//! owner already typed for the ladder -- the rung as a share of equity, the
//! levels and their growth, the fraction of a side's balance the ladder
//! deploys, the slice reserved for the deep rungs -- by `side_funding`, so
//! it scales with the account and changes when the ladder does.
//!
//! The demands are bounded by construction. With the production settings a
//! full-size prlusdt side needs about a quarter of equity, a prlusdc side
//! 4.5%, a usdcusdt side 2.3%: every side at full size is 64% of equity, and
//! most sides are nowhere near full. The rest of the account -- the SURPLUS
//! -- cannot earn as resting orders, because the caps that keep a maker from
//! being the book bound what can rest, and the yield says nothing about
//! where it should sit. Any split of it is as good as any other for yield,
//! so it stays where the owner put it: an ANCHOR share per currency, read
//! from the account when the learned program starts and again at every
//! settled deposit or withdrawal. That is what makes a deposit capital
//! rather than inventory to work off: the anchor absorbs it the moment it
//! lands, and only the other currencies' unmet demands are drawn from it.
//! It is also the guard against concentration: a strong arm on thin
//! evidence can pull at most its side's funding toward itself, never the
//! surplus, and the t on its posterior has already discounted the thin
//! evidence. The variance the arm measured is the variance of a resting
//! rung; a directional holding is a different bet, and this never makes it.
//!
//! The target moves by DRIFT, never by a jump. The learned program is a
//! balanced program whose ratio is re-derived every hour and whose glide is
//! re-anchored from the current inventory over two evidence half-lives (24
//! hours). Each hour it moves a 24th of the way; over a day 63%. The lean
//! does the moving, through the sizes and the reservation price, so a
//! rebalance never pays the spread to arrive. Hourly because the arms
//! refresh every five minutes and the split should not chase every refresh:
//! in an hour the evidence has decayed by 6%, which is the smallest change
//! worth re-deciding on. Two half-lives because the split should not move
//! faster than the evidence it follows, and because the book turns over
//! 1.33x equity a day (measured 2026-09-25), so a day is the time in which
//! it can shift a quarter of equity by leaning alone, without forcing.

use std::collections::HashMap;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::config::Quoting;
use crate::num;
use crate::strategy::quote::growth_pow;

/// Value of the currency one side of a book must hold, in that book's quote
/// currency, for its ladder to rest at full size.
///
/// The tight ladder fits `ladder_fraction` of the side's balance -- after
/// the deep ladder's slice is reserved -- across its rungs by their growth
/// weights, with the configured rung as a ceiling; it is at full size once
/// the balance covers the rungs divided by that fraction. The deep ladder
/// draws on its own slice directly. The side needs whichever of the two
/// wants more. Mirrors the sizing in `quote::plan`, and is tested against
/// it.
pub fn side_funding(q: &Quoting, equity: Decimal, inventory: Decimal, price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let rung_value = if q.size_pct_of_equity > Decimal::ZERO && equity > Decimal::ZERO {
        equity * q.size_pct_of_equity
    } else {
        q.size_base * price
    };
    let weights: Decimal = (0..q.levels as i64).map(|i| growth_pow(q.level_size_growth, i)).sum();
    let deep_share = if q.deep_levels > 0 {
        num::clamp(q.deep_allocation, Decimal::ZERO, dec!(0.8))
    } else {
        Decimal::ZERO
    };
    let tight = rung_value * weights;
    // With no ladder fraction the rungs are the configured size whatever
    // the balance, and only the balance itself bounds them.
    let fitted = if q.ladder_fraction > Decimal::ZERO { tight / q.ladder_fraction } else { tight };
    let tight_need = if deep_share < Decimal::ONE { fitted / (Decimal::ONE - deep_share) } else { fitted };

    let deep_need = if q.deep_levels > 0 && deep_share > Decimal::ZERO {
        let rung = if q.deep_pct_of_inventory > Decimal::ZERO {
            inventory * q.deep_pct_of_inventory
        } else {
            q.deep_size
        };
        let deep_weights: Decimal =
            (0..q.deep_levels as i64).map(|i| growth_pow(q.deep_size_growth, i)).sum();
        rung * deep_weights * price / deep_share
    } else {
        Decimal::ZERO
    };
    tight_need.max(deep_need).max(Decimal::ZERO)
}

/// The value each currency should hold, given the account's total value,
/// the anchor share of each currency, and the value each currency's arms
/// have earned. Values are all in one unit of account.
///
/// Every demand is funded first. What is left is split in proportion to how
/// far each currency's anchor sits above its own demand, so the surplus
/// stays where the owner put it and only the unmet demands are drawn from
/// it. Should the demands exceed the account -- not possible with the
/// production ladder, which wants 64% of equity at full size -- they are
/// scaled to fit.
pub fn learned_targets(
    total: Decimal,
    anchor: &HashMap<String, Decimal>,
    demand: &HashMap<String, Decimal>,
) -> HashMap<String, Decimal> {
    let currencies: Vec<&String> = {
        let mut v: Vec<&String> = anchor.keys().chain(demand.keys()).collect();
        v.sort();
        v.dedup();
        v
    };
    let want = |c: &String| demand.get(c).copied().unwrap_or_default().max(Decimal::ZERO);
    let demanded: Decimal = currencies.iter().map(|c| want(c)).sum();
    if total <= Decimal::ZERO {
        return currencies.iter().map(|c| ((*c).clone(), Decimal::ZERO)).collect();
    }
    if demanded >= total {
        return currencies.iter().map(|c| ((*c).clone(), want(c) * total / demanded)).collect();
    }
    let surplus = total - demanded;
    let room = |c: &String| (anchor.get(c).copied().unwrap_or_default() * total - want(c)).max(Decimal::ZERO);
    let rooms: Decimal = currencies.iter().map(|c| room(c)).sum();
    currencies
        .iter()
        .map(|c| {
            let extra = if rooms > Decimal::ZERO { surplus * room(c) / rooms } else { Decimal::ZERO };
            ((*c).clone(), want(c) + extra)
        })
        .collect()
}

/// The share of value a program on `base` should hold in it, among the
/// currencies that program's books trade: `base` and the quote currencies
/// of every market on it. This is the number `Program::target_at` steers.
pub fn ratio_for(targets: &HashMap<String, Decimal>, base: &str, quotes: &[String]) -> Option<Decimal> {
    let b = targets.get(base).copied()?;
    let pool: Decimal = b + quotes.iter().filter_map(|q| targets.get(q)).sum::<Decimal>();
    if pool <= Decimal::ZERO {
        return None;
    }
    Some(num::clamp(b / pool, Decimal::ZERO, Decimal::ONE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn prod_cfg() -> crate::config::Config {
        toml::from_str(include_str!("../../deploy/config.prod.toml")).unwrap()
    }

    fn prod() -> Quoting {
        prod_cfg().quoting
    }

    /// The production settings as one book sees them.
    fn prod_for(market: &str) -> Quoting {
        let cfg = prod_cfg();
        let p = crate::config::Params { q: cfg.quoting.clone(), risk: cfg.risk.clone(), enabled: true, dry_run: false };
        p.for_market(cfg.overrides.get(market)).q
    }

    fn map(pairs: &[(&str, Decimal)]) -> HashMap<String, Decimal> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// prlusdc's depth rule must not be the ceiling on its ask side.
    ///
    /// Measured 2026-09-26: within 5% of the touch this book showed $10,699 of
    /// bids and $398 of asks, so one 50% rule allowed a $290 bid and a $63
    /// ask -- 4.6x more buying power than selling power on the only book that
    /// pays. A market maker round-trips at the rate of its smaller side, so
    /// that asymmetry, not the edge or the capital, was the cap on the whole
    /// round trip. The band is what was wrong: 5% of a book whose spread is
    /// 6.5% measures almost none of it.
    #[test]
    fn the_thin_book_s_asks_are_not_capped_by_a_band_narrower_than_its_spread() {
        let q = prod_for("prlusdc");
        assert!(
            q.depth_band >= dec!(0.10),
            "the band must cover this book's ~6.5% spread, got {}",
            q.depth_band
        );
        // The rung, not the depth rule, has to be what sizes the order. At a
        // $12k equity the rung is 2% = $240; the measured ask depth was $398,
        // so the cap has to clear $240 on that depth to stop binding.
        let ask_depth = dec!(398);
        assert!(
            ask_depth * q.max_order_pct_of_depth > dec!(12000) * q.size_pct_of_equity,
            "depth cap {} x ${ask_depth} must not bind under the rung",
            q.max_order_pct_of_depth
        );
        // And the global default must stay conservative: this is an exception
        // earned by being the only maker, not a new baseline.
        let g = prod();
        assert!(g.max_order_pct_of_depth <= dec!(0.25), "{}", g.max_order_pct_of_depth);
        assert!(g.depth_band <= dec!(0.05), "{}", g.depth_band);
    }

    /// The production prlusdt side wants about a quarter of equity at full
    /// size: two rungs of 3% at 1.5x growth, fitted at 55% of the balance
    /// left after the deep ladder's 45% slice. The deep ladder itself wants
    /// 24% of the PRL held. Every side at full size is under two thirds of
    /// the account, which is what leaves a surplus for the anchor to hold.
    #[test]
    fn the_production_sides_need_a_bounded_share_of_equity() {
        let q = prod();
        let (equity, price) = (dec!(6480), dec!(1.18));
        let inventory = dec!(3661);
        let prl = side_funding(&q, equity, inventory, price);
        assert!(prl > equity * dec!(0.20) && prl < equity * dec!(0.30), "{prl}");
        // prlusdc wants ~18% of equity since 2026-09-30: a 4% rung on a book
        // that had shrunk keeps its orders in the size zone where its fills
        // pay. The percentage follows the dollar rung, not the other way
        // round; see the config.
        let thin = prod_for("prlusdc");
        let usdc = side_funding(&thin, equity, inventory, price);
        assert!(usdc > equity * dec!(0.17) && usdc < equity * dec!(0.19), "{usdc}");
        let stable = prod_for("usdcusdt");
        let s = side_funding(&stable, dec!(2069), dec!(374), Decimal::ONE);
        assert!(s > dec!(2069) * dec!(0.02) && s < dec!(2069) * dec!(0.03), "{s}");
        // Both sides of every book that rests a ladder, at full size, in one
        // account's worth of value. prlusdt is not one: it is a cycle leg
        // (`cycle_leg_only`) and funds no ladder, so its `prl` figure above
        // is what it WOULD need, not a claim.
        assert!(prod_for("prlusdt").cycle_leg_only, "prlusdt must stay a leg for this bound to hold");
        assert!(dec!(2) * (usdc + s) < equity * dec!(0.70));
    }

    /// The funding need is a share of equity: three times the account is
    /// three times the number, so a model that is right at one size is right
    /// at three times it without anyone retyping it. The deep ladder's share
    /// follows the inventory the same way.
    #[test]
    fn funding_scales_with_the_account() {
        let q = prod();
        let small = side_funding(&q, dec!(6480), dec!(3661), dec!(1.18));
        let big = side_funding(&q, dec!(19440), dec!(10983), dec!(1.18));
        assert!((big - small * dec!(3)).abs() < dec!(0.01), "{small} -> {big}");
        // Skewed to 90% PRL at the same equity: the deep ladder, sized from
        // inventory, wants more; the tight ladder, sized from equity, does
        // not.
        let skewed = side_funding(&q, dec!(6480), dec!(4942), dec!(1.18));
        assert!(skewed >= small, "{small} -> {skewed}");
        assert!(skewed < small * dec!(1.5), "{small} -> {skewed}");
    }

    /// Nothing earning, nothing moves: the targets are the anchor. This is
    /// day one of the learned program, where every demand was already funded.
    #[test]
    fn with_every_demand_funded_the_split_is_the_anchor() {
        let anchor = map(&[("prl", dec!(0.67)), ("usdt", dec!(0.26)), ("usdc", dec!(0.07))]);
        let demand = map(&[("prl", dec!(1300)), ("usdt", dec!(160)), ("usdc", dec!(290))]);
        let t = learned_targets(dec!(6480), &anchor, &demand);
        for (c, share) in &anchor {
            assert!((t[c] - share * dec!(6480)).abs() < dec!(0.01), "{c}: {} vs anchor", t[c]);
        }
        let none = learned_targets(dec!(6480), &anchor, &HashMap::new());
        assert!((none["prl"] - dec!(0.67) * dec!(6480)).abs() < dec!(0.01));
    }

    /// A deposit is capital, not inventory to sell. The owner sends 5,000
    /// PRL to a 67%-PRL account: the anchor re-reads as 87% PRL, and the
    /// only value that moves is what the other currencies' unmet demands
    /// need -- here USDC's ladder, short by the amount the larger account
    /// now wants there -- drawn from every funded currency's surplus in
    /// proportion, so the owner's mix is kept, not just the PRL.
    #[test]
    fn a_deposit_is_absorbed_and_only_unmet_demand_is_drawn_from_it() {
        let price = dec!(1.18);
        let prl_value = (dec!(3661) + dec!(5000)) * price;
        let total = prl_value + dec!(1695) + dec!(374); // about 12,300
        let anchor = map(&[("prl", prl_value / total), ("usdt", dec!(1695) / total), ("usdc", dec!(374) / total)]);
        // USDC's ladder at 99% share on the larger account wants 4.5%.
        let demand = map(&[("prl", total * dec!(0.20)), ("usdt", total * dec!(0.025)), ("usdc", total * dec!(0.045))]);
        let t = learned_targets(total, &anchor, &demand);
        let usdc_short = total * dec!(0.045) - dec!(374);
        assert!(usdc_short > Decimal::ZERO);
        assert!((t["usdc"] - total * dec!(0.045)).abs() < dec!(0.01), "USDC is funded: {}", t["usdc"]);
        let sold_prl = prl_value - t["prl"];
        let sold_usdt = dec!(1695) - t["usdt"];
        assert!(sold_prl > Decimal::ZERO && sold_usdt > Decimal::ZERO, "{t:?}");
        assert!((sold_prl + sold_usdt - usdc_short).abs() < dec!(0.01), "together exactly the shortfall: {sold_prl} + {sold_usdt}");
        // In proportion to each one's surplus over its own demand.
        let room_prl = prl_value - total * dec!(0.20);
        let room_usdt = dec!(1695) - total * dec!(0.025);
        assert!((sold_prl / sold_usdt - room_prl / room_usdt).abs() < dec!(0.001), "{sold_prl} : {sold_usdt}");
        assert!(sold_prl < dec!(200), "of a $5,900 deposit: {sold_prl}");
        let sum: Decimal = t.values().sum();
        assert!((sum - total).abs() < dec!(0.01));
    }

    /// An earning side draws the currency it needs, and no more than its
    /// ladder needs. prlusdt's bids proving themselves on a 90%-PRL account
    /// pull a quarter of equity into USDT, over the drift, and the anchor
    /// keeps the rest in PRL.
    #[test]
    fn an_earning_side_is_funded_up_to_its_ladder_and_no_further() {
        let total = dec!(10000);
        let anchor = map(&[("prl", dec!(0.90)), ("usdt", dec!(0.07)), ("usdc", dec!(0.03))]);
        let demand = map(&[("prl", dec!(2000)), ("usdt", dec!(2500)), ("usdc", dec!(200))]);
        let t = learned_targets(total, &anchor, &demand);
        assert_eq!(t["usdt"], dec!(2500), "{t:?}");
        // The $1,800 comes from the two funded currencies' surplus in
        // proportion: PRL has $7,000 over its demand, USDC $100.
        assert!((t["prl"] - dec!(9000) + dec!(1800) * dec!(70) / dec!(71)).abs() < dec!(0.01), "{t:?}");
        assert!((t["usdc"] - dec!(300) + dec!(1800) / dec!(71)).abs() < dec!(0.01), "{t:?}");
        assert!(t["prl"] > dec!(7000), "the anchor keeps the rest in PRL: {t:?}");
        let sum: Decimal = t.values().sum();
        assert!((sum - total).abs() < dec!(0.01));
        // Twice the evidence for the same arm changes nothing: the demand is
        // the ladder, not the conviction.
        assert_eq!(learned_targets(total, &anchor, &demand)["usdt"], dec!(2500));
    }

    /// Demands past the account are scaled to fit rather than invented.
    #[test]
    fn demands_beyond_the_account_are_scaled_to_fit() {
        let anchor = map(&[("prl", dec!(0.5)), ("usdt", dec!(0.5))]);
        let demand = map(&[("prl", dec!(900)), ("usdt", dec!(300))]);
        let t = learned_targets(dec!(600), &anchor, &demand);
        assert_eq!(t["prl"], dec!(450));
        assert_eq!(t["usdt"], dec!(150));
    }

    /// The ratio a program steers is its base's share among the currencies
    /// its books trade: the PRL program over the whole account, the USDC one
    /// over the two stablecoins.
    #[test]
    fn the_ratio_is_the_base_share_among_its_own_books_currencies() {
        let t = map(&[("prl", dec!(6000)), ("usdt", dec!(3000)), ("usdc", dec!(1000))]);
        assert_eq!(ratio_for(&t, "prl", &["usdt".into(), "usdc".into()]), Some(dec!(0.6)));
        assert_eq!(ratio_for(&t, "usdc", &["usdt".into()]), Some(dec!(0.25)));
        assert_eq!(ratio_for(&t, "eth", &["usdt".into()]), None);
    }
}
