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
//! * **Waits for spikes up high.** Asked for on 2026-10-22: "bigger asks
//!   higher up in the book that capitalize on spikes". Apart from the small
//!   joined asks, `spike_rungs` larger asks rest at `spike_step`, twice that,
//!   and so on over the mid, each bigger than the one under it, holding
//!   `spike_share` of the coins between them. They keep their place while the
//!   price rises into them, and come down only when the mid falls so far that
//!   the top one is more than a step past its band.

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
    /// On a small level, an order may be bumped up to this much quote
    /// currency even past `level_share` (owner, 2026-10-15: "for small parts
    /// of the book you can bump ask to 2000 dollars"). 0 = off.
    pub min_notional: Decimal,
    /// The lowest this many stranger levels may take up to `near_share` of
    /// their size instead of `level_share`, sized at that ceiling whatever
    /// the flow (owner, 2026-10-23: "go up to 25% in the first three ticks
    /// above the ask"). 0 = off.
    pub near_levels: usize,
    pub near_share: Decimal,
    /// How many spike asks rest over the mid; 0 = off.
    pub spike_rungs: usize,
    /// Rung `r` (from 0) rests `(r + 1) * spike_step` over the mid.
    pub spike_step: Decimal,
    /// Share of the holding the spike asks hold between them, weighted
    /// 1, 2, 3... from the lowest, so the highest is the biggest.
    pub spike_share: Decimal,
}

/// Spike asks carry `SPIKE_LEVEL + rung` as their level, so a pass can tell
/// them from the joined asks.
pub const SPIKE_LEVEL: i64 = 500;

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
            min_notional: Decimal::ZERO,
            near_levels: 0,
            near_share: dec!(0.25),
            spike_rungs: 0,
            spike_step: dec!(0.07),
            spike_share: dec!(0.4),
        }
    }
}

/// One of our resting asks.
#[derive(Debug, Clone, Copy)]
pub struct Resting {
    pub price: Decimal,
    /// The level it was placed with: `SPIKE_LEVEL` and up for a spike ask.
    pub level: i64,
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
    /// The most new orders may add under the liquidation pace; `None` for no
    /// pace. See `pace_room`.
    pub pace: Option<Decimal>,
    pub params: SellParams,
}

impl SellInput<'_> {
    /// What new orders may still add, under every cap.
    pub fn room(&self) -> Decimal {
        let p = &self.params;
        let day = p.day_rate * self.volume_24h - self.sold_24h;
        let hour = p.hour_rate * self.volume_1h - self.sold_1h;
        let pace = self.pace.unwrap_or(Decimal::MAX);
        day.min(hour).min(pace).max(Decimal::ZERO)
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
    let quote = |level: i64, price: Decimal, amount: Decimal| DesiredQuote {
        side: Side::Sell,
        price,
        amount,
        level,
        bps: if mid > Decimal::ZERO { ((price - mid) / mid * dec!(10000)).round_dp(1) } else { Decimal::ZERO },
        recent_move: Decimal::ZERO,
        width_ticks,
    };

    // Keep what rests at or over the floor and still near the touch. An
    // order of ours under every stranger's ask is the best ask already, and
    // is about to be bought; it stays too.
    let keep_top = levels.get(p.keep_levels.saturating_sub(1)).map(|(px, _)| *px);
    let near_top = levels.get(p.near_levels.wrapping_sub(1)).map(|(px, _)| *px);
    let near = |px: Decimal| near_top.is_some_and(|top| px <= top);
    let share_at = |px: Decimal| if near(px) { p.near_share.max(p.level_share) } else { p.level_share };
    let mut out: Vec<DesiredQuote> = Vec::new();
    let (spikes, joined): (Vec<Resting>, Vec<Resting>) =
        input.resting.iter().partition(|r| p.spike_rungs > 0 && r.level >= SPIKE_LEVEL);

    // The spike asks hang off the book's own mid, not the lowest level over
    // the floor.
    let touch = match (input.best_bid, input.asks.first()) {
        (Some(b), Some((a, _))) => (b + *a) / Decimal::TWO,
        (b, a) => b.or(a.map(|(a, _)| *a)).unwrap_or_default(),
    };
    let spike_on = p.spike_rungs > 0 && touch > Decimal::ZERO && p.spike_step > Decimal::ZERO;
    let spike_top = touch * (Decimal::ONE + p.spike_step * Decimal::from(p.spike_rungs as u64 + 1));
    if spike_on {
        for r in &spikes {
            let rung = r.level - SPIKE_LEVEL;
            let live = rung < p.spike_rungs as i64 && !out.iter().any(|q| q.level == r.level);
            if live && r.price >= input.floor && r.price <= spike_top && r.remaining > Decimal::ZERO {
                out.push(quote(r.level, r.price, r.remaining));
            }
        }
    }

    for r in &joined {
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
        // A bumped order is kept too, or it would be pulled the pass after
        // it was placed.
        let small = r.remaining <= beside * share_at(r.price) * dec!(1.5)
            || r.remaining * r.price.abs() <= p.min_notional;
        if r.price >= input.floor && near && small && r.remaining > Decimal::ZERO {
            out.push(quote(0, r.price, r.remaining));
        }
    }
    let spiked = out.iter().filter(|q| q.level >= SPIKE_LEVEL).count();

    let freed: Decimal = input
        .resting
        .iter()
        .filter(|r| !out.iter().any(|q| q.price == r.price && q.amount == r.remaining))
        .map(|r| r.remaining)
        .sum();
    let mut pool = input.free + freed;

    // Spike asks wait for a rally, so the tape caps do not gate them; only
    // the coins do. A missing rung is placed at its height over the mid, next
    // to a stranger's ask just over it when there is one.
    if spike_on && spiked < p.spike_rungs {
        let held = input.free + input.resting.iter().map(|r| r.remaining).sum::<Decimal>();
        let weights = Decimal::from((p.spike_rungs * (p.spike_rungs + 1) / 2) as u64);
        for rung in 0..p.spike_rungs {
            let level = SPIKE_LEVEL + rung as i64;
            if out.iter().any(|q| q.level == level) {
                continue;
            }
            let step = p.spike_step * Decimal::from(rung as u64 + 1);
            let target = num::ceil_to(touch * (Decimal::ONE + step), market.price_precision).max(input.floor);
            let reach = target * (Decimal::ONE + p.spike_step / Decimal::TWO);
            // A price we already rest at is taken; the rung goes a tick up.
            let taken = |px: &Decimal| out.iter().any(|q| q.price == *px);
            let mut price = input
                .asks
                .iter()
                .map(|(px, _)| *px)
                .find(|px| *px >= target && *px <= reach && !taken(px))
                .unwrap_or(target);
            while taken(&price) && market.tick() > Decimal::ZERO {
                price += market.tick();
            }
            let j = crate::strategy::quote::jitter_unit(input.seed.wrapping_add(rung as u64 * 104_729));
            let want = held * p.spike_share * Decimal::from(rung as u64 + 1) / weights * (Decimal::ONE + dec!(0.1) * j);
            let amount = num::floor_to(want.min(pool), market.amount_precision);
            if amount < p.min_order.max(market.min_amount) {
                continue;
            }
            pool -= amount;
            out.push(quote(level, price, amount));
        }
    }

    // Then a slice at each of the lowest levels we are not already on,
    // bottom first, while the caps and the free coins allow.
    let mut budget = input.room().min(pool);
    let scale = input.flow_scale();
    if scale.is_zero() {
        budget = Decimal::ZERO;
    }
    for (i, (price, size)) in levels.iter().take(p.levels).enumerate() {
        // A near level we already rest on is topped up to its share by a
        // second order, so a raised share takes effect without losing the
        // queue; any other level we are on is left as it is.
        let have: Decimal = out.iter().filter(|q| q.price == *price).map(|q| q.amount).sum();
        if have > Decimal::ZERO && !near(*price) {
            continue;
        }
        // Jitter both the share and the cap, so neither "half the level"
        // nor "1,500" shows up as a pattern on the book.
        let j = |k: u64| crate::strategy::quote::jitter_unit(input.seed.wrapping_add(k).wrapping_add(i as u64 * 7919));
        // Over the boost price the owner wants out faster (2026-10-10).
        let boost = if *price >= p.boost_price { p.boost.max(Decimal::ONE) } else { Decimal::ONE };
        let normal = p.level_share * dec!(0.75);
        let share = if near(*price) {
            share_at(*price) * (dec!(0.925) + dec!(0.075) * j(1))
        } else {
            (normal * boost * scale * (Decimal::ONE + p.size_jitter * j(1))).min(p.level_share)
        };
        let mut want = size * share;
        if p.max_order > Decimal::ZERO {
            want = want.min(p.max_order * boost * (dec!(0.85) + dec!(0.15) * j(2)));
        }
        // Prices are negated on the buy side, hence the abs.
        if p.min_notional > Decimal::ZERO && !price.is_zero() {
            want = want.max(p.min_notional * (dec!(0.85) + dec!(0.15) * j(3)) / price.abs());
        }
        // A top-up smaller than a third of the slice is not worth an order.
        let want = if have > Decimal::ZERO && want - have < want / dec!(3) { Decimal::ZERO } else { want - have };
        let want = want.min(budget);
        let amount = num::floor_to(want, market.amount_precision);
        if amount < p.min_order.max(market.min_amount) {
            continue;
        }
        budget -= amount;
        out.push(quote(i as i64, *price, amount));
    }
    out.sort_by(|a, b| a.price.cmp(&b.price));
    out
}

/// The slow buy's view of the market: the slow sell's, seen from the bid.
pub struct BuyInput<'a> {
    pub market: &'a Market,
    /// The strangers' bids, highest first, our own size stripped.
    pub bids: &'a [(Decimal, Decimal)],
    pub best_ask: Option<Decimal>,
    /// Never pay above this.
    pub ceiling: Decimal,
    pub volume_24h: Decimal,
    pub bought_24h: Decimal,
    pub volume_1h: Decimal,
    pub bought_1h: Decimal,
    /// Quote currency not committed to any order.
    pub free_quote: Decimal,
    /// The most base new bids may add, whatever the cash would buy; `None`
    /// for no limit (`buy_qty`).
    pub max_base: Option<Decimal>,
    /// Our own bids resting now.
    pub resting: &'a [Resting],
    /// Who is trading, as for the sell: (buyer-taken - seller-taken) / both.
    pub flow: Option<Decimal>,
    pub seed: u64,
    /// The sell's knobs, read the buyer's way round: `boost_price` is the
    /// price at and BELOW which levels get `boost`, and 0 turns it off.
    pub params: SellParams,
}

impl BuyInput<'_> {
    /// The same pass as a sell, in negated prices: bids become asks, the
    /// ceiling a floor, the best ask a best bid.
    fn mirrored<T>(&self, f: impl FnOnce(&SellInput) -> T) -> T {
        let asks: Vec<(Decimal, Decimal)> = self.bids.iter().map(|(p, q)| (-*p, *q)).collect();
        let resting: Vec<Resting> =
            self.resting.iter().map(|r| Resting { price: -r.price, ..*r }).collect();
        // Base the free cash buys, priced at the dearest level we would join,
        // so the ladder can never commit more cash than there is.
        let dearest = self
            .bids
            .iter()
            .map(|(p, _)| *p)
            .find(|p| *p <= self.ceiling)
            .unwrap_or(self.ceiling);
        let free = if dearest > Decimal::ZERO { self.free_quote / dearest } else { Decimal::ZERO };
        let free = self.max_base.map_or(free, |m| free.min(m.max(Decimal::ZERO)));
        let boost_price = if self.params.boost_price > Decimal::ZERO { -self.params.boost_price } else { Decimal::MAX };
        let input = SellInput {
            market: self.market,
            asks: &asks,
            best_bid: self.best_ask.map(|a| -a),
            floor: -self.ceiling,
            volume_24h: self.volume_24h,
            sold_24h: self.bought_24h,
            volume_1h: self.volume_1h,
            sold_1h: self.bought_1h,
            free,
            resting: &resting,
            // Sellers hitting the bids are what fill a buyer, so they show
            // more; buyers lifting the asks is a market to stand back from.
            flow: self.flow.map(|f| -f),
            seed: self.seed,
            pace: None,
            params: SellParams { boost_price, ..self.params.clone() },
        };
        f(&input)
    }

    pub fn room(&self) -> Decimal {
        self.mirrored(|i| i.room())
    }

    pub fn flow_scale(&self) -> Decimal {
        self.mirrored(|i| i.flow_scale())
    }
}

/// The bids that should rest after this pass, highest first: the slow
/// sell's ladder worked down the bid side. Joins only prices where strangers
/// already bid, at or under the ceiling, never above the best bid and never
/// crossing; each order small beside its level; the best bids funded first;
/// the queue kept; capped by the tape; and it holds off while buyers are
/// lifting the asks.
pub fn buy_ladder(input: &BuyInput) -> Vec<DesiredQuote> {
    let mut out: Vec<DesiredQuote> = input
        .mirrored(ladder)
        .into_iter()
        .map(|q| DesiredQuote { side: Side::Buy, price: -q.price, ..q })
        .collect();
    out.sort_by(|a, b| b.price.cmp(&a.price));
    out
}

/// How long the pace looks back: what sold in this window, plus what rests,
/// may not pass the window's share of the schedule.
pub const PACE_WINDOW_HOURS: i64 = 2;

/// The liquidation pace: what new asks may add so a holding sells out over
/// about `hours`, and not faster. Asked for on 2026-10-21: sell any deposit
/// "slowly, into the book, no rush but targeting 24-48 hours".
///
/// The rate is `(holding + sold_24h) / hours` an hour. Over the first day
/// after a deposit that sum is the deposit itself, so the rate is flat; after
/// it the rate tapers as old sales leave the window, and a 36-hour pace is
/// done in about 40. A new deposit raises the holding and so the rate, with
/// no state to keep. What sold in the last `PACE_WINDOW_HOURS` and what rests
/// now both count against it, so a slow hour cannot pile up asks.
///
/// A holding too small for the pace to reach one order a window still gets
/// one order a window, so the tail sells instead of sitting there.
pub fn pace_room(holding: Decimal, sold_24h: Decimal, sold_window: Decimal, resting: Decimal, hours: Decimal, min_order: Decimal) -> Decimal {
    let rate = (holding.max(Decimal::ZERO) + sold_24h.max(Decimal::ZERO)) / hours;
    let budget = (rate * Decimal::from(PACE_WINDOW_HOURS)).max(min_order);
    (budget - sold_window - resting).max(Decimal::ZERO)
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

    fn bids() -> Vec<(Decimal, Decimal)> {
        (0..12).map(|i| (dec!(1.34) - Decimal::from(i) * dec!(0.01), dec!(2000))).collect()
    }

    fn buy_input<'a>(m: &'a Market, bids: &'a [(Decimal, Decimal)], resting: &'a [Resting]) -> BuyInput<'a> {
        BuyInput {
            market: m,
            bids,
            best_ask: Some(dec!(1.35)),
            ceiling: dec!(1.40),
            volume_24h: dec!(2_000_000),
            bought_24h: Decimal::ZERO,
            volume_1h: dec!(50_000),
            bought_1h: Decimal::ZERO,
            free_quote: dec!(40_000),
            max_base: None,
            resting,
            flow: None,
            seed: 7,
            params: SellParams { size_jitter: Decimal::ZERO, levels: 5, keep_levels: 8, max_order: dec!(1500), boost_price: Decimal::ZERO, ..SellParams::default() },
        }
    }

    /// The buy is the sell reflected: bids on the strangers' best levels,
    /// highest first, each sized as the sell sizes an ask beside the same
    /// level.
    #[test]
    fn the_slow_buy_joins_the_best_bids_like_the_sell_joins_the_asks() {
        let m = market();
        let b = bids();
        let q = buy_ladder(&buy_input(&m, &b, &[]));
        assert!(q.iter().all(|o| o.side == Side::Buy));
        assert_eq!(
            q.iter().map(|o| o.price).collect::<Vec<_>>(),
            vec![dec!(1.34), dec!(1.33), dec!(1.32), dec!(1.31), dec!(1.30)],
            "the five best stranger bids, best first"
        );
        let a = book();
        let sell = ladder(&input(&m, &a, &[]));
        assert_eq!(
            q.iter().map(|o| o.amount).collect::<Vec<_>>(),
            sell.iter().map(|o| o.amount).collect::<Vec<_>>(),
            "sized exactly as the mirror-image asks"
        );
    }

    #[test]
    fn the_slow_buy_never_pays_over_the_ceiling_nor_at_the_ask() {
        let m = market();
        let b = bids();
        let mut i = buy_input(&m, &b, &[]);
        i.ceiling = dec!(1.32);
        let q = buy_ladder(&i);
        assert_eq!(q.first().map(|o| o.price), Some(dec!(1.32)), "nothing above the ceiling");
        // A stranger bid at or above the best ask is a crossed book; it is
        // not a level to join.
        let crossed = vec![(dec!(1.36), dec!(2000)), (dec!(1.34), dec!(2000))];
        let i = buy_input(&m, &crossed, &[]);
        assert!(buy_ladder(&i).iter().all(|o| o.price < dec!(1.35)));
    }

    #[test]
    fn the_slow_buy_spends_no_more_cash_than_it_has() {
        let m = market();
        let b = bids();
        let mut i = buy_input(&m, &b, &[]);
        i.free_quote = dec!(1000);
        let spent: Decimal = buy_ladder(&i).iter().map(|o| o.amount * o.price).sum();
        assert!(spent <= dec!(1000), "{spent}");
        i.free_quote = Decimal::ZERO;
        assert!(buy_ladder(&i).is_empty());
    }

    /// Sellers hitting the bids are a buyer's fills: it shows more. Buyers
    /// lifting the asks is a market running away: it adds nothing.
    #[test]
    fn the_slow_buy_reads_the_flow_the_other_way_round() {
        let m = market();
        let b = bids();
        let mut i = buy_input(&m, &b, &[]);
        i.flow = Some(dec!(0.8));
        assert!(buy_ladder(&i).is_empty(), "buyers chasing: stand back");
        i.flow = Some(dec!(-0.6));
        let busy: Decimal = buy_ladder(&i).iter().map(|o| o.amount).sum();
        i.flow = None;
        let quiet: Decimal = buy_ladder(&i).iter().map(|o| o.amount).sum();
        assert!(busy > quiet, "{busy} vs {quiet}");
    }

    #[test]
    fn the_slow_buy_keeps_its_queue_and_its_tape_cap() {
        let m = market();
        let b = bids();
        let resting = [Resting { level: 0, price: dec!(1.33), remaining: dec!(250) }];
        let q = buy_ladder(&buy_input(&m, &b, &resting));
        assert!(q.iter().any(|o| o.price == dec!(1.33) && o.amount == dec!(250)), "kept as it stands");
        let mut i = buy_input(&m, &b, &[]);
        i.bought_1h = dec!(5000); // 10% of the hour's 50k: no room
        assert!(buy_ladder(&i).is_empty());
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
            pace: None,
            params: SellParams { size_jitter: Decimal::ZERO, levels: 5, keep_levels: 8, max_order: dec!(1500), boost_price: dec!(100), ..SellParams::default() },
        }
    }

    /// A deposit sells at a flat rate over the first day and is gone in
    /// about 40 hours on a 36-hour pace: inside the 24-48 asked for.
    #[test]
    fn the_pace_sells_a_deposit_out_in_24_to_48_hours() {
        let hours = dec!(36);
        // Minute steps; everything the pace allows sells at once, the
        // fastest the market could fill it.
        let mut holding = dec!(18000);
        let mut sold: Vec<Decimal> = Vec::new(); // per minute
        let mut done = None;
        for t in 0..(72 * 60) {
            let sum = |mins: usize| sold.iter().rev().take(mins).copied().sum::<Decimal>();
            let room = pace_room(holding, sum(24 * 60), sum(PACE_WINDOW_HOURS as usize * 60), Decimal::ZERO, hours, dec!(20));
            // Fills arrive a little each minute, never more than the room.
            let fill = room.min(holding).min(dec!(20));
            holding -= fill;
            sold.push(fill);
            if holding <= Decimal::ZERO {
                done = Some(t);
                break;
            }
            if t == 12 * 60 {
                assert!(holding > dec!(11000), "no faster than the pace: {holding} left at 12h");
            }
        }
        let done = done.expect("sold out within 72 hours");
        assert!((24 * 60..=48 * 60).contains(&done), "sold out at minute {done}");
    }

    #[test]
    fn the_pace_counts_what_rests_and_never_strands_the_tail() {
        // 3,600 an hour over 2 hours is 7,200, less 5,000 resting.
        assert_eq!(pace_room(dec!(129600), Decimal::ZERO, Decimal::ZERO, dec!(5000), dec!(36), dec!(20)), dec!(2200));
        // A tail too small for one order at the pace still gets one.
        assert_eq!(pace_room(dec!(50), Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, dec!(36), dec!(20)), dec!(20));
        assert_eq!(pace_room(dec!(50), Decimal::ZERO, dec!(20), Decimal::ZERO, dec!(36), dec!(20)), Decimal::ZERO);
        // But only one a window, resting or sold.
        assert_eq!(pace_room(dec!(50), Decimal::ZERO, Decimal::ZERO, dec!(20), dec!(36), dec!(20)), Decimal::ZERO);
    }

    #[test]
    fn the_pace_caps_the_ladder() {
        let m = market();
        let a = book();
        let mut i = input(&m, &a, &[]);
        i.pace = Some(dec!(300));
        let total: Decimal = ladder(&i).iter().map(|o| o.amount).sum();
        assert!(total <= dec!(300) && total > Decimal::ZERO, "{total}");
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
    fn a_buy_never_adds_more_base_than_max_base() {
        let m = market();
        let bids = bids();
        let mut i = buy_input(&m, &bids, &[]);
        i.max_base = Some(dec!(700));
        let q = buy_ladder(&i);
        assert!(!q.is_empty());
        assert!(q.iter().map(|o| o.amount).sum::<Decimal>() <= dec!(700), "{q:?}");
        i.max_base = Some(Decimal::ZERO);
        assert!(buy_ladder(&i).is_empty());
    }

    #[test]
    fn a_small_level_is_bumped_to_the_notional_floor() {
        let m = market();
        let asks = vec![(dec!(1.35), dec!(100)), (dec!(1.36), dec!(9000))];
        let mut i = input(&m, &asks, &[]);
        i.params.level_share = dec!(0.5);
        i.params.max_order = Decimal::ZERO;
        i.params.min_notional = dec!(2000);
        let q = ladder(&i);
        let small = q.iter().find(|o| o.price == dec!(1.35)).unwrap();
        let notional = small.amount * small.price;
        assert!(notional <= dec!(2000) && notional >= dec!(1700), "bumped to about $2,000: {notional}");
        let big = q.iter().find(|o| o.price == dec!(1.36)).unwrap();
        assert!(big.amount <= dec!(4500), "a big level keeps its share: {}", big.amount);

        // And the next pass keeps it rather than pulling it as oversized.
        let resting = [Resting { level: 0, price: small.price, remaining: small.amount }];
        let mut again = input(&m, &asks, &resting);
        again.params = i.params.clone();
        let q2 = ladder(&again);
        assert!(q2.iter().any(|o| o.price == small.price && o.amount == small.amount), "{q2:?}");
    }

    #[test]
    fn the_bump_never_spends_more_than_there_is() {
        let m = market();
        let asks = vec![(dec!(1.35), dec!(100))];
        let mut i = input(&m, &asks, &[]);
        i.params.min_notional = dec!(2000);
        i.free = dec!(300);
        let q = ladder(&i);
        assert_eq!(q.iter().map(|o| o.amount).sum::<Decimal>(), dec!(300));
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
        let resting = [Resting { level: 0, price: dec!(1.36), remaining: dec!(577) }];
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
        let resting = [Resting { level: 0, price: dec!(1.65), remaining: dec!(2752) }];
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
        let resting = [Resting { level: 0, price: dec!(1.34), remaining: dec!(300) }];
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
        let resting = [Resting { level: 0, price: dec!(1.36), remaining: dec!(500) }];
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
            Resting { level: 0, price: dec!(1.36), remaining: dec!(30_000) },
            Resting { level: 0, price: dec!(1.37), remaining: dec!(400) },
        ];
        let q = ladder(&input(&m, &asks, &resting));
        assert!(!q.iter().any(|o| o.amount == dec!(30_000)));
        assert!(q.iter().any(|o| o.price == dec!(1.37) && o.amount == dec!(400)));
        assert!(q.iter().all(|o| o.amount <= dec!(600)));
    }

    fn spiky<'a>(m: &'a Market, asks: &'a [(Decimal, Decimal)], resting: &'a [Resting]) -> SellInput<'a> {
        let mut i = input(m, asks, resting);
        i.params.level_share = dec!(0.1);
        i.params.max_order = Decimal::ZERO;
        i.params.spike_rungs = 4;
        i.free = dec!(50_000);
        i
    }

    /// 2026-10-22: under 10% of each joined level, plus bigger asks up high
    /// for a spike, each bigger than the one under it.
    #[test]
    fn spike_asks_rest_over_the_mid_bigger_as_they_go_up() {
        let m = market();
        let a = book(); // 2,000 a level from 1.35, mid 1.345
        let q = ladder(&spiky(&m, &a, &[]));
        let spikes: Vec<_> = q.iter().filter(|o| o.level >= SPIKE_LEVEL).collect();
        assert_eq!(spikes.len(), 4, "{q:?}");
        assert_eq!(spikes.iter().map(|o| o.price).collect::<Vec<_>>(), vec![dec!(1.44), dec!(1.54), dec!(1.63), dec!(1.73)]);
        assert!(spikes.windows(2).all(|w| w[1].amount > w[0].amount), "{spikes:?}");
        let held: Decimal = spikes.iter().map(|o| o.amount).sum();
        assert!(held > dec!(18_000) && held < dec!(22_000), "about 40% of 50k: {held}");
        assert!(
            q.iter().filter(|o| o.level < SPIKE_LEVEL).all(|o| o.amount <= dec!(200)),
            "joined asks stay under 10% of their level: {q:?}"
        );
        // A rung beside a stranger's ask just over its height joins it.
        assert_eq!(spikes[0].price, dec!(1.44));
    }

    #[test]
    fn spike_asks_keep_their_place_until_the_mid_falls_away() {
        let m = market();
        let a = book();
        let resting = [
            Resting { level: SPIKE_LEVEL, price: dec!(1.44), remaining: dec!(2000) },
            Resting { level: SPIKE_LEVEL + 3, price: dec!(1.73), remaining: dec!(8000) },
        ];
        let q = ladder(&spiky(&m, &a, &resting));
        assert!(q.iter().any(|o| o.price == dec!(1.44) && o.amount == dec!(2000)));
        assert!(q.iter().any(|o| o.price == dec!(1.73) && o.amount == dec!(8000)));
        assert_eq!(q.iter().filter(|o| o.level >= SPIKE_LEVEL).count(), 4, "the missing two are filled in");

        // The price runs up through the low rung: it stays, it is the sale.
        let up: Vec<_> = (0..12).map(|i| (dec!(1.45) + Decimal::from(i) * dec!(0.01), dec!(2000))).collect();
        let mut i = spiky(&m, &up, &resting);
        i.best_bid = Some(dec!(1.43));
        assert!(ladder(&i).iter().any(|o| o.price == dec!(1.44) && o.amount == dec!(2000)), "never pulled into a rally");

        // The price falls well away: the top rung comes down to the new band.
        let down: Vec<_> = (0..12).map(|i| (dec!(1.15) + Decimal::from(i) * dec!(0.01), dec!(2000))).collect();
        let mut i = spiky(&m, &down, &resting);
        i.best_bid = Some(dec!(1.14));
        i.floor = dec!(1.10);
        let q = ladder(&i);
        assert!(!q.iter().any(|o| o.price == dec!(1.73)), "{q:?}");
        assert!(q.iter().any(|o| o.price == dec!(1.44) && o.amount == dec!(2000)), "the near one keeps its place");
    }

    #[test]
    fn spike_asks_never_under_the_floor_and_off_by_default() {
        let m = market();
        let a = book();
        let mut i = spiky(&m, &a, &[]);
        i.floor = dec!(1.60);
        let q = ladder(&i);
        assert!(q.iter().all(|o| o.price >= dec!(1.60)), "{q:?}");
        let mut i = spiky(&m, &a, &[]);
        i.params.spike_rungs = 0;
        assert!(ladder(&i).iter().all(|o| o.level < SPIKE_LEVEL));
    }

    /// With close steps the low rung can land where a joined ask rests: it
    /// moves up to the next free price instead of going missing.
    #[test]
    fn a_spike_rung_on_a_joined_price_moves_up() {
        let m = market();
        let a = book(); // 1.35..1.46
        let mut i = spiky(&m, &a, &[]);
        i.params.levels = 12;
        i.params.spike_step = dec!(0.03);
        let q = ladder(&i);
        assert_eq!(q.iter().filter(|o| o.level >= SPIKE_LEVEL).count(), 4, "{q:?}");
        let mut prices: Vec<_> = q.iter().map(|o| o.price).collect();
        prices.dedup();
        assert_eq!(prices.len(), q.len(), "one order a price: {q:?}");
    }

    #[test]
    fn the_near_levels_take_up_to_their_own_share() {
        let m = market();
        let a = book(); // 2,000 a level
        let mut i = input(&m, &a, &[]);
        i.params.level_share = dec!(0.095);
        i.params.max_order = Decimal::ZERO;
        i.params.near_levels = 3;
        i.flow = Some(dec!(-0.4));
        let q = ladder(&i);
        let at = |px: Decimal| q.iter().find(|o| o.price == px).map(|o| o.amount).unwrap_or_default();
        for px in [dec!(1.35), dec!(1.36), dec!(1.37)] {
            assert!(at(px) > dec!(420) && at(px) <= dec!(500), "{px}: {}", at(px));
        }
        assert!(at(dec!(1.38)) <= dec!(190), "{}", at(dec!(1.38)));
        // And kept at that size on the next pass.
        let resting = [Resting { level: 0, price: dec!(1.35), remaining: at(dec!(1.35)) }];
        let mut again = input(&m, &a, &resting);
        again.params = i.params;
        assert!(ladder(&again).iter().any(|o| o.price == dec!(1.35) && o.amount == at(dec!(1.35))));
    }

    #[test]
    fn a_near_level_we_rest_on_is_topped_up_to_its_share() {
        let m = market();
        let a = book(); // 2,000 a level
        let resting = [Resting { level: 0, price: dec!(1.35), remaining: dec!(150) }];
        let mut i = input(&m, &a, &resting);
        i.params.level_share = dec!(0.095);
        i.params.near_levels = 3;
        let q = ladder(&i);
        let at: Decimal = q.iter().filter(|o| o.price == dec!(1.35)).map(|o| o.amount).sum();
        assert!(q.iter().any(|o| o.price == dec!(1.35) && o.amount == dec!(150)), "the old order keeps its place");
        assert!(at > dec!(420) && at <= dec!(500), "{at}");
        // A level past the near ones is not topped up.
        let resting = [Resting { level: 0, price: dec!(1.38), remaining: dec!(100) }];
        let mut i = input(&m, &a, &resting);
        i.params.level_share = dec!(0.095);
        i.params.near_levels = 3;
        let at: Decimal = ladder(&i).iter().filter(|o| o.price == dec!(1.38)).map(|o| o.amount).sum();
        assert_eq!(at, dec!(100));
    }
}
