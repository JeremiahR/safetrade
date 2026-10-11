//! The slow sell: distribute every unit of the base asset without being seen.
//!
//! The brief, 2026-10-10: sell all the PRL, with some haste, above a floor
//! price, "invisible but distributing", under 5-10% of the day's volume, as
//! a professional working an order out of a thin book.
//!
//! So the bot behaves like a small iceberg spread over the ask side:
//!
//! * **Join, never set.** It only rests at prices where strangers already
//!   have asks, at the floor or above. It never prints a new price level,
//!   never undercuts, never crosses. The book's shape does not change.
//! * **Small beside what is there.** Each order is at most `level_share`
//!   (20%) of the strangers' size at its price. A level the size of an
//!   average trade gets a slice smaller than one.
//! * **Bottom first.** The lowest levels are funded first, since that is
//!   where the buying arrives. That is the haste.
//! * **Queue kept.** A resting order is never resized or moved for drift.
//!   It stays until it fills, falls under the floor, or the book has moved
//!   so far down that it is no longer near the touch -- then it is cancelled
//!   and its coins go to the bottom.
//! * **Capped by the tape.** New orders only go out while what we sold in
//!   the last 24h is under `day_rate` of the day's volume, and what we sold
//!   in the last hour under `hour_rate` of the hour's. The caps gate new
//!   orders, not resting ones, so a quiet hour does not cost the queue.
//! * **No pattern.** Each new order's size is jittered and its cap varies, so
//!   no two slices are the same fraction of their level or a round number.
//! * **Reads the flow.** Over the last few minutes of tape: when sellers are
//!   hitting the bids, it adds nothing (resting asks stay, keeping their
//!   place); when buyers are lifting the asks, it shows more. A passive
//!   seller that keeps refilling into a falling market is how a patient
//!   distribution moves the price down anyway.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::exchange::{Market, Side};
use crate::num;
use crate::strategy::DesiredQuote;

/// The knobs, read from settings each pass.
#[derive(Debug, Clone, Copy)]
pub struct SellParams {
    /// Most we sell over 24h, as a share of the book's 24h volume.
    pub day_rate: Decimal,
    /// Most we sell over the last hour, as a share of that hour's volume.
    pub hour_rate: Decimal,
    /// How many stranger price levels, from the lowest at or over the floor,
    /// get a new order.
    pub levels: usize,
    /// A resting order is kept while it is within this many levels of the
    /// lowest; beyond that its coins go back to the bottom.
    pub keep_levels: usize,
    /// One order is never more than this share of the strangers' size at its
    /// price -- a hard ceiling (owner, 2026-10-10: "cap it at 20% of the
    /// level's existing size"). A normal slice is about three quarters of it;
    /// the boost and buyer flow raise it to the ceiling, never past.
    pub level_share: Decimal,
    /// And never more than this many units; 0 for no cap beyond the level's
    /// own size (the owner, 2026-10-10: "look at how much is in the books").
    pub max_order: Decimal,
    /// Nor fewer: a slice smaller than this is not worth the order.
    pub min_order: Decimal,
    /// Each new order's size varies by up to this fraction either way.
    pub size_jitter: Decimal,
    /// Flow at or under this (sellers' share of taker volume over buyers')
    /// adds no new orders.
    pub stand_back: Decimal,
    /// Most the size grows when buyers are lifting the asks.
    pub lean_max: Decimal,
    /// At and above this price, levels get `boost` times the share and cap.
    pub boost_price: Decimal,
    pub boost: Decimal,
}

impl Default for SellParams {
    fn default() -> Self {
        SellParams {
            day_rate: dec!(0.05),
            hour_rate: dec!(0.10),
            levels: 7,
            keep_levels: 10,
            level_share: dec!(0.2),
            max_order: Decimal::ZERO,
            min_order: dec!(20),
            size_jitter: dec!(0.25),
            stand_back: dec!(-0.5),
            lean_max: dec!(1.5),
            boost_price: dec!(1.40),
            boost: dec!(2),
        }
    }
}

/// One of our resting asks.
#[derive(Debug, Clone, Copy)]
pub struct Resting {
    pub price: Decimal,
    pub remaining: Decimal,
}

pub struct SellInput<'a> {
    pub market: &'a Market,
    /// The strangers' asks, lowest first, our own size stripped.
    pub asks: &'a [(Decimal, Decimal)],
    pub best_bid: Option<Decimal>,
    /// Never sell below this.
    pub floor: Decimal,
    pub volume_24h: Decimal,
    pub sold_24h: Decimal,
    pub volume_1h: Decimal,
    pub sold_1h: Decimal,
    /// Base not committed to any order.
    pub free: Decimal,
    /// Our own asks resting now.
    pub resting: &'a [Resting],
    /// Who is trading: (buyer-taken - seller-taken) / both, over the last
    /// few minutes, in -1..=1. `None` when the tape is too thin to say.
    pub flow: Option<Decimal>,
    /// Seeds the size jitter. Any value; a new one each pass.
    pub seed: u64,
    pub params: SellParams,
}

impl SellInput<'_> {
    /// What new orders may still add, under both caps.
    pub fn room(&self) -> Decimal {
        let p = &self.params;
        let day = p.day_rate * self.volume_24h - self.sold_24h;
        let hour = p.hour_rate * self.volume_1h - self.sold_1h;
        day.min(hour).max(Decimal::ZERO)
    }

    /// How much of a normal slice to add, from the flow: nothing while
    /// sellers dominate, more while buyers do, normal when it is quiet.
    pub fn flow_scale(&self) -> Decimal {
        let p = &self.params;
        match self.flow {
            Some(f) if f <= p.stand_back => Decimal::ZERO,
            Some(f) => num::clamp(Decimal::ONE + f, dec!(0.5), p.lean_max.max(Decimal::ONE)),
            None => Decimal::ONE,
        }
    }

    /// The stranger levels we may join, lowest first.
    fn levels(&self) -> Vec<(Decimal, Decimal)> {
        self.asks
            .iter()
            .copied()
            .filter(|(p, q)| *p >= self.floor && *q > Decimal::ZERO)
            .filter(|(p, _)| self.best_bid.is_none_or(|b| *p > b))
            .collect()
    }
}

/// The asks that should rest after this pass, lowest first.
///
/// An order kept as it is appears with its exact remaining size, so the
/// reconciler matches it and leaves it in the queue; anything resting that is
/// not in the list is cancelled.
pub fn ladder(input: &SellInput) -> Vec<DesiredQuote> {
    let p = &input.params;
    let market = input.market;
    let levels = input.levels();
    let mid = match (input.best_bid, levels.first()) {
        (Some(b), Some((a, _))) => (b + a) / Decimal::TWO,
        _ => Decimal::ZERO,
    };
    let width_ticks = match (input.best_bid, input.asks.first()) {
        (Some(b), Some((a, _))) if market.tick() > Decimal::ZERO => ((*a - b) / market.tick()).round_dp(2),
        _ => Decimal::ONE,
    };
    let quote = |level: usize, price: Decimal, amount: Decimal| DesiredQuote {
        side: Side::Sell,
        price,
        amount,
        level: level as i64,
        bps: if mid > Decimal::ZERO { ((price - mid) / mid * dec!(10000)).round_dp(1) } else { Decimal::ZERO },
        recent_move: Decimal::ZERO,
        width_ticks,
    };

    // Keep what rests at or over the floor and still near the touch. An
    // order of ours under every stranger's ask is the best ask already, and
    // is about to be bought; it stays too.
    let keep_top = levels.get(p.keep_levels.saturating_sub(1)).map(|(px, _)| *px);
    let mut out: Vec<DesiredQuote> = Vec::new();
    for r in input.resting {
        let near = keep_top.is_some_and(|top| r.price <= top);
        // Never keep an order that has grown past the ceiling against its
        // level -- the strangers pulled size, or it was placed under an older
        // rule. Measured against the strangers at its price, or at the next
        // level up when it rests alone. Half again over the ceiling, so a
        // small wobble in the level does not cost the queue.
        let beside = input
            .asks
            .iter()
            .find(|(px, q)| *px >= r.price && *q > Decimal::ZERO)
            .map(|(_, q)| *q)
            .unwrap_or_default();
        let small = r.remaining <= beside * p.level_share * dec!(1.5);
        if r.price >= input.floor && near && small && r.remaining > Decimal::ZERO {
            out.push(quote(0, r.price, r.remaining));
        }
    }

    // Then a slice at each of the lowest levels we are not already on,
    // bottom first, while the caps and the free coins allow.
    let freed: Decimal = input
        .resting
        .iter()
        .filter(|r| !out.iter().any(|q| q.price == r.price && q.amount == r.remaining))
        .map(|r| r.remaining)
        .sum();
    let mut budget = input.room().min(input.free + freed);
    let scale = input.flow_scale();
    if scale.is_zero() {
        budget = Decimal::ZERO;
    }
    for (i, (price, size)) in levels.iter().take(p.levels).enumerate() {
        if out.iter().any(|q| q.price == *price) {
            continue;
        }
        // Jitter both the share and the cap, so neither "half the level"
        // nor "1,500" shows up as a pattern on the book.
        let j = |k: u64| crate::strategy::quote::jitter_unit(input.seed.wrapping_add(k).wrapping_add(i as u64 * 7919));
        // Over the boost price the owner wants out faster (2026-10-10).
        let boost = if *price >= p.boost_price { p.boost.max(Decimal::ONE) } else { Decimal::ONE };
        let normal = p.level_share * dec!(0.75);
        let share = (normal * boost * scale * (Decimal::ONE + p.size_jitter * j(1))).min(p.level_share);
        let mut want = (size * share).min(budget);
        if p.max_order > Decimal::ZERO {
            want = want.min(p.max_order * boost * (dec!(0.85) + dec!(0.15) * j(2)));
        }
        let amount = num::floor_to(want, market.amount_precision);
        if amount < p.min_order.max(market.min_amount) {
            continue;
        }
        budget -= amount;
        out.push(quote(i, *price, amount));
    }
    out.sort_by(|a, b| a.price.cmp(&b.price));
    out
}

/// Opportunistic conversion: one ask selling all of `free` base at `min` or
/// better, for a stablecoin pair where any price over `min` is a good one.
///
/// Asked for on 2026-10-10: turn USDC into USDT when the price is good. The
/// fee is 10 bp a side, so 1.0010 is par after it. If a bid already pays the
/// minimum, it sells into that bid; otherwise it rests at the best ask, or at
/// the minimum when the best ask is under it. `None` with too little to sell.
pub fn convert_ask(market: &Market, best_bid: Option<Decimal>, best_ask: Option<Decimal>, free: Decimal, min: Decimal) -> Option<DesiredQuote> {
    if min <= Decimal::ZERO {
        return None;
    }
    let amount = num::floor_to(free, market.amount_precision);
    if amount < market.min_amount || amount <= Decimal::ZERO {
        return None;
    }
    let price = match (best_bid, best_ask) {
        (Some(b), _) if b >= min => b,
        (_, Some(a)) => a.max(min),
        _ => min,
    };
    Some(DesiredQuote {
        side: Side::Sell,
        price: num::ceil_to(price, market.price_precision),
        amount,
        level: 0,
        bps: Decimal::ZERO,
        recent_move: Decimal::ZERO,
        width_ticks: Decimal::ONE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> Market {
        Market {
            id: "prlusdt".into(),
            name: "PRL/USDT".into(),
            base_unit: "prl".into(),
            quote_unit: "usdt".into(),
            min_price: dec!(0.01),
            max_price: dec!(100),
            min_amount: dec!(1),
            amount_precision: 2,
            price_precision: 2,
            state: "enabled".into(),
        }
    }

    fn book() -> Vec<(Decimal, Decimal)> {
        (0..12).map(|i| (dec!(1.35) + Decimal::from(i) * dec!(0.01), dec!(2000))).collect()
    }

    fn input<'a>(m: &'a Market, asks: &'a [(Decimal, Decimal)], resting: &'a [Resting]) -> SellInput<'a> {
        SellInput {
            market: m,
            asks,
            best_bid: Some(dec!(1.34)),
            floor: dec!(1.30),
            volume_24h: dec!(2_000_000),
            sold_24h: Decimal::ZERO,
            volume_1h: dec!(50_000),
            sold_1h: Decimal::ZERO,
            free: dec!(30_000),
            resting,
            flow: None,
            seed: 7,
            params: SellParams { size_jitter: Decimal::ZERO, levels: 5, keep_levels: 8, max_order: dec!(1500), boost_price: dec!(100), ..SellParams::default() },
        }
    }

    #[test]
    fn joins_the_lowest_levels_small() {
        let m = market();
        let asks = book();
        let q = ladder(&input(&m, &asks, &[]));
        assert_eq!(q.len(), 5);
        assert_eq!(q[0].price, dec!(1.35), "joins the best ask");
        assert!(q.iter().all(|o| asks.iter().any(|(p, _)| *p == o.price)), "never a new price level");
        assert!(q.iter().all(|o| o.amount <= dec!(400)), "at most 20% of the level's size");
    }

    #[test]
    fn never_bigger_than_max_order_or_under_the_floor() {
        let m = market();
        let asks = vec![(dec!(1.28), dec!(9000)), (dec!(1.29), dec!(9000)), (dec!(1.30), dec!(9000)), (dec!(1.31), dec!(9000))];
        let mut i = input(&m, &asks, &[]);
        i.best_bid = Some(dec!(1.27));
        let q = ladder(&i);
        assert_eq!(q.iter().map(|o| o.price).collect::<Vec<_>>(), vec![dec!(1.30), dec!(1.31)]);
        assert!(q.iter().all(|o| o.amount <= dec!(1500) && o.amount >= dec!(1050)), "{q:?}");
    }

    #[test]
    fn the_hour_cap_spends_bottom_first() {
        let m = market();
        let asks = book();
        let mut i = input(&m, &asks, &[]);
        i.sold_1h = dec!(3500); // 10% of 50k is 5k: 1,500 left
        let q = ladder(&i);
        assert_eq!(q[0].price, dec!(1.35));
        let all: Decimal = q.iter().map(|o| o.amount).sum();
        assert!(all <= dec!(1500), "{all}");
    }

    #[test]
    fn resting_orders_keep_their_place_even_over_the_cap() {
        let m = market();
        let asks = book();
        let resting = [Resting { price: dec!(1.36), remaining: dec!(577) }];
        let mut i = input(&m, &asks, &resting);
        i.sold_24h = dec!(100_000); // the day is spent
        let q = ladder(&i);
        assert_eq!(q.len(), 1);
        assert_eq!((q[0].price, q[0].amount), (dec!(1.36), dec!(577)), "kept exactly, so it is not replaced");
    }

    #[test]
    fn far_orders_come_back_to_the_bottom() {
        let m = market();
        let asks = book();
        let resting = [Resting { price: dec!(1.65), remaining: dec!(2752) }];
        let mut i = input(&m, &asks, &resting);
        i.free = Decimal::ZERO;
        let q = ladder(&i);
        assert!(q.iter().all(|o| o.price < dec!(1.65)), "the far order is let go");
        assert_eq!(q[0].price, dec!(1.35), "and its coins fund the bottom");
    }

    #[test]
    fn own_order_under_the_strangers_stays() {
        let m = market();
        let asks = book();
        let resting = [Resting { price: dec!(1.34), remaining: dec!(300) }];
        let mut i = input(&m, &asks, &resting);
        i.best_bid = Some(dec!(1.33));
        let q = ladder(&i);
        assert!(q.iter().any(|o| o.price == dec!(1.34) && o.amount == dec!(300)));
    }

    #[test]
    fn nothing_without_coins() {
        let m = market();
        let asks = book();
        let mut i = input(&m, &asks, &[]);
        i.free = Decimal::ZERO;
        assert!(ladder(&i).is_empty());
    }

    #[test]
    fn sizes_vary_and_stay_under_the_level() {
        let m = market();
        let asks = book();
        let mut i = input(&m, &asks, &[]);
        i.params.size_jitter = dec!(0.25);
        let q = ladder(&i);
        let mut sizes: Vec<_> = q.iter().map(|o| o.amount).collect();
        sizes.dedup();
        assert!(sizes.len() >= 4, "{sizes:?}");
        assert!(q.iter().all(|o| o.amount >= dec!(225) && o.amount <= dec!(400)), "{q:?}");
        i.seed = 8;
        assert_ne!(ladder(&i)[0].amount, q[0].amount, "a new pass draws new sizes");
    }

    #[test]
    fn sellers_hitting_bids_adds_nothing_but_keeps_the_queue() {
        let m = market();
        let asks = book();
        let resting = [Resting { price: dec!(1.36), remaining: dec!(500) }];
        let mut i = input(&m, &asks, &resting);
        i.flow = Some(dec!(-0.5));
        let q = ladder(&i);
        assert_eq!(q.len(), 1);
        assert_eq!((q[0].price, q[0].amount), (dec!(1.36), dec!(500)));
    }

    #[test]
    fn buyers_lifting_shows_more() {
        let m = market();
        let asks = book();
        let mut i = input(&m, &asks, &[]);
        i.params.max_order = dec!(100_000);
        i.volume_1h = dec!(1_000_000);
        let quiet: Decimal = ladder(&i).iter().map(|o| o.amount).sum();
        i.flow = Some(dec!(0.6));
        let busy: Decimal = ladder(&i).iter().map(|o| o.amount).sum();
        assert!(busy > quiet * dec!(1.3), "{busy} vs {quiet}");
        assert!(busy <= dec!(2000), "never over 20% of any level");
        i.flow = Some(dec!(-0.1));
        let soft: Decimal = ladder(&i).iter().map(|o| o.amount).sum();
        assert!(soft < quiet);
    }

    #[test]
    fn converts_only_at_a_good_price() {
        let mut m = market();
        m.price_precision = 4;
        let q = convert_ask(&m, Some(dec!(0.9996)), Some(dec!(1.0000)), dec!(500), dec!(1.0010)).unwrap();
        assert_eq!((q.side, q.price, q.amount), (Side::Sell, dec!(1.0010), dec!(500)), "rests at the minimum");
        let q = convert_ask(&m, Some(dec!(0.9996)), Some(dec!(1.0015)), dec!(500), dec!(1.0010)).unwrap();
        assert_eq!(q.price, dec!(1.0015), "joins a better ask");
        let q = convert_ask(&m, Some(dec!(1.0012)), Some(dec!(1.0015)), dec!(500), dec!(1.0010)).unwrap();
        assert_eq!(q.price, dec!(1.0012), "takes a bid that pays the minimum");
        assert!(convert_ask(&m, None, None, dec!(0.5), dec!(1.0010)).is_none());
        assert!(convert_ask(&m, None, None, dec!(500), Decimal::ZERO).is_none(), "0 is off");
    }

    #[test]
    fn more_over_the_boost_price() {
        let m = market();
        let asks: Vec<_> = (0..6).map(|i| (dec!(1.38) + Decimal::from(i) * dec!(0.01), dec!(1000))).collect();
        let mut i = input(&m, &asks, &[]);
        i.best_bid = Some(dec!(1.37));
        i.params.boost_price = dec!(1.40);
        let q = ladder(&i);
        let at = |px: Decimal| q.iter().find(|o| o.price == px).unwrap().amount;
        assert_eq!(at(dec!(1.39)), dec!(150));
        assert_eq!(at(dec!(1.40)), dec!(200), "boosted, but never over 20% of the level");
    }

    #[test]
    fn an_order_too_big_for_its_level_is_pulled() {
        let m = market();
        let asks = book(); // 2,000 a level
        let resting = [
            Resting { price: dec!(1.36), remaining: dec!(30_000) },
            Resting { price: dec!(1.37), remaining: dec!(400) },
        ];
        let q = ladder(&input(&m, &asks, &resting));
        assert!(!q.iter().any(|o| o.amount == dec!(30_000)));
        assert!(q.iter().any(|o| o.price == dec!(1.37) && o.amount == dec!(400)));
        assert!(q.iter().all(|o| o.amount <= dec!(600)));
    }
}
