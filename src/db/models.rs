use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use sqlx::FromRow;

use crate::exchange::Side;
use crate::num;

/// What the bot is trying to do with its inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProgramKind {
    /// Hold `target_qty` and earn the spread around it. No deadline.
    Neutral,
    /// TWAP buy: ramp inventory from `start_qty` up to `target_qty` by `ends_at`.
    Accumulate,
    /// TWAP sell: ramp inventory down to `target_qty` by `ends_at`.
    Distribute,
    /// Get to `target_qty` as fast as passive quoting allows, no schedule.
    Flatten,
    /// Hold `ratio` of total portfolio *value* in the base asset.
    ///
    /// The only kind whose target is not a fixed quantity. As price rises the
    /// target quantity falls, so the bot sells into strength; as price falls it
    /// rises, so the bot buys weakness. In a ranging market that mechanically
    /// grows both the base and quote pools, which is the point. In a sustained
    /// trend it underperforms simply holding the winning side — that is the
    /// trade being made, and it is worth making deliberately.
    Balanced,
    /// A TWAP buy of everything: put the whole account into the base asset,
    /// over the window, and never sell a unit of it on the way.
    ///
    /// Three differences from `Accumulate`, and each one is a thing that went
    /// wrong when this was expressed as an accumulate by hand:
    ///
    /// * **The target is computed, not typed.** It is what the account can hold
    ///   if every unit of quote currency is spent, worked out once when the
    ///   program starts, so nobody has to price the cash by hand. It then holds
    ///   still: the target does NOT follow the balance around, because a target
    ///   that moves every loop makes what the bot is about to do impossible to
    ///   predict from what it said. New capital is a discrete event instead --
    ///   see the engine's transfer handling, which starts a fresh twap over its
    ///   own window when money lands.
    /// * **It never sells.** A TWAP's inventory error skews sizes both ways,
    ///   which is the catch-up behaviour a two-sided program wants: get ahead
    ///   of the ramp and it sells back down to it. Asked to spend a deposit,
    ///   that is not catch-up, it is the bot undoing the job. So the ask side
    ///   is not quoted at all, at any distance, on any book.
    /// * **It is the only thing quoting.** With `Quoting::twap_only` set, a
    ///   book whose asset is not being worked stands down rather than making a
    ///   market beside it with the same money.
    Twap,
    /// `Balanced`, with the ratio set by the arms rather than by hand.
    ///
    /// Every hour the engine re-derives the split of the account between
    /// the currencies from what each currency's arms have earned
    /// (`strategy::allocation`), writes it here as `ratio`, and re-anchors
    /// the glide from the current inventory over two evidence half-lives.
    /// Between those hours it is exactly a balanced program.
    Learned,
    /// A slow sell of every unit of the base asset, paced by the market.
    ///
    /// No target and no deadline: every coin on the account is for sale, and
    /// a deposit is simply more of it. The pace is a share of the book's own
    /// 24h volume (`sell_rate_pct`, more above `sell_fast_price`), rested as
    /// a ladder of asks from the touch up into where the spikes reach. It
    /// never crosses and never undercuts the best ask, and never sells below
    /// `limit_price`. See `strategy::sell`.
    TwapSell,
    /// A slow buy with every unit of quote currency: the slow sell worked
    /// down the bid side. A deposit of cash is simply more to spend. Joins
    /// only existing bid levels, never above the best bid nor `limit_price`,
    /// which it requires. See `strategy::sell::buy_ladder`.
    TwapBuy,
}

impl ProgramKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProgramKind::Neutral => "neutral",
            ProgramKind::Accumulate => "accumulate",
            ProgramKind::Distribute => "distribute",
            ProgramKind::Flatten => "flatten",
            ProgramKind::Twap => "twap",
            ProgramKind::Balanced => "balanced",
            ProgramKind::Learned => "learned",
            ProgramKind::TwapSell => "twapsell",
            ProgramKind::TwapBuy => "twapbuy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "neutral" => ProgramKind::Neutral,
            "accumulate" | "buy" | "accum" => ProgramKind::Accumulate,
            "distribute" | "sell" | "dist" => ProgramKind::Distribute,
            "flatten" | "flat" => ProgramKind::Flatten,
            "twap" | "ripitin" | "allin" => ProgramKind::Twap,
            "balanced" | "balance" | "rebalance" | "5050" => ProgramKind::Balanced,
            "learned" | "learn" | "auto" => ProgramKind::Learned,
            "twapsell" | "sellslow" => ProgramKind::TwapSell,
            "twapbuy" | "buyslow" => ProgramKind::TwapBuy,
            _ => return None,
        })
    }

    /// Whether the program finishes when its deadline passes.
    ///
    /// `Balanced` may have a deadline too, but that only governs how gradually
    /// it *reaches* its first target; it never completes.
    pub fn is_scheduled(&self) -> bool {
        matches!(self, ProgramKind::Accumulate | ProgramKind::Distribute | ProgramKind::Twap)
    }

    /// Whether the target is a share of value rather than a quantity.
    pub fn is_ratio(&self) -> bool {
        matches!(self, ProgramKind::Balanced | ProgramKind::Learned)
    }

    /// Whether this program may only ever buy.
    ///
    /// Inventory error skews sizes on both sides, so a program that is ahead of
    /// its ramp quotes asks and sells back down to it. For a TWAP buy of the
    /// whole account that is not catch-up, it is undoing the job: measured
    /// live, a buy that ran ahead on the thin book immediately rested an 811
    /// unit ask against the position it had just acquired.
    pub fn is_buy_only(&self) -> bool {
        matches!(self, ProgramKind::Twap)
    }

    /// Whether this program is EXECUTION: a quantity the owner asked for,
    /// rather than a standing bet on the spread.
    ///
    /// Two rules turn on it, for the same reason. The arms do not gate an
    /// execution program's fills (`arms::State::Execution`) because getting to
    /// the target is the job, not a wager on it. And a book restricted to being
    /// a cycle leg (`Quoting::cycle_leg_only`) still quotes for one of these,
    /// because "do not market-make here" is a statement about earning the
    /// spread, never about refusing to work size the owner asked for.
    pub fn is_execution(&self) -> bool {
        matches!(
            self,
            ProgramKind::Accumulate
                | ProgramKind::Distribute
                | ProgramKind::Flatten
                | ProgramKind::Twap
                | ProgramKind::TwapSell
                | ProgramKind::TwapBuy
        )
    }

    /// Whether this is one of the twap programs, the ones TWAP mode
    /// (`Quoting::twap_only`) lets quote while every other book stands down.
    pub fn is_twap(&self) -> bool {
        matches!(self, ProgramKind::Twap | ProgramKind::TwapSell | ProgramKind::TwapBuy)
    }

    /// Whether this is one of the two slow programs, paced by the tape and
    /// laddered by `strategy::sell` rather than quoted by `plan`.
    pub fn is_slow(&self) -> bool {
        matches!(self, ProgramKind::TwapSell | ProgramKind::TwapBuy)
    }
}

/// Mirrors the `programs` table; sqlx binds every column.
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct ProgramRow {
    pub id: i64,
    pub market: String,
    pub kind: String,
    pub state: String,
    pub start_qty: String,
    pub target_qty: String,
    pub ratio: Option<String>,
    pub banked: Option<String>,
    pub limit_price: Option<String>,
    pub budget: Option<String>,
    pub started_at: i64,
    pub ends_at: Option<i64>,
    pub note: Option<String>,
}

/// A program with its numbers parsed. This is what the strategy sees.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Program {
    pub id: i64,
    pub market: String,
    pub kind: ProgramKind,
    pub state: String,
    pub start_qty: Decimal,
    pub target_qty: Decimal,
    /// `Balanced` only: share of portfolio value to hold in the base asset.
    pub ratio: Option<Decimal>,
    /// Quote-currency value of trading profit already banked into the base
    /// asset, and so held outside the ratio.
    ///
    /// This is what turns spread capture into a base-asset pile. A round trip
    /// that sells 100 at 1.05 and buys back at 1.04 returns 100.96 units: the
    /// profit is the same either way, and this decides which pile it lands in.
    /// Banking is direction-neutral, unlike the rebalance itself, which only
    /// grows both pools through completed price cycles and sheds base in a
    /// one-way rally.
    pub banked: Decimal,
    pub limit_price: Option<Decimal>,
    /// `TwapBuy` only: the most quote currency this program spends, counted
    /// from its own fills plus what its open bids hold. Other slow buys leave
    /// the unspent part alone. `None` is every unit of cash.
    pub budget: Option<Decimal>,
    pub started_at: i64,
    pub ends_at: Option<i64>,
    pub note: Option<String>,
}

impl TryFrom<ProgramRow> for Program {
    type Error = anyhow::Error;

    fn try_from(r: ProgramRow) -> Result<Self, Self::Error> {
        Ok(Program {
            kind: ProgramKind::parse(&r.kind)
                .ok_or_else(|| anyhow::anyhow!("unknown program kind {:?}", r.kind))?,
            id: r.id,
            market: r.market,
            state: r.state,
            start_qty: num::parse(&r.start_qty),
            target_qty: num::parse(&r.target_qty),
            ratio: num::opt_parse(r.ratio.as_deref()),
            banked: num::opt_parse(r.banked.as_deref()).unwrap_or_default(),
            limit_price: num::opt_parse(r.limit_price.as_deref()),
            budget: num::opt_parse(r.budget.as_deref()).filter(|b| *b > Decimal::ZERO),
            started_at: r.started_at,
            ends_at: r.ends_at,
            note: r.note,
        })
    }
}

/// What the account holds right now, for targets that depend on value rather
/// than quantity.
#[derive(Debug, Clone, Copy)]
pub struct Portfolio {
    pub price: Decimal,
    pub base_qty: Decimal,
    pub quote_qty: Decimal,
}

impl Portfolio {
    /// Total value in quote currency.
    pub fn value(&self) -> Decimal {
        self.base_qty * self.price + self.quote_qty
    }
}

impl Program {
    /// Where inventory *should* be at time `now` (unix ms).
    ///
    /// This is the heart of the design: every stance reduces to this one
    /// number, so the quoting logic never needs to know whether it is running a
    /// TWAP, making a market, or rebalancing. One code path, and switching
    /// stance at runtime is just changing what this returns.
    pub fn target_at(&self, now: i64, pf: Portfolio) -> Decimal {
        match self.kind {
            ProgramKind::Neutral | ProgramKind::Flatten => self.target_qty,

            // Paced by the tape, not by inventory: there is no position to
            // steer toward, so the target is wherever inventory already is.
            ProgramKind::TwapSell | ProgramKind::TwapBuy => pf.base_qty,

            ProgramKind::Accumulate | ProgramKind::Distribute => {
                self.ramp(now, self.target_qty)
            }

            // Everything the account could hold in the base asset if every
            // unit of quote currency were spent, ramped over the window.
            //
            // Derived live, so it is correct at any balance the moment coins
            // land and at any price: a deposit raises it, a price move changes
            // what the same cash buys. `target_qty` is only the snapshot taken
            // when the program started, kept so completion has something to
            // check against.
            // A fixed destination, reached on a schedule -- the same shape as
            // `Accumulate`. The only live number is the ANCHOR, and only ever
            // downwards: `start_qty` is the balance when the program started,
            // and inventory can fall below it for reasons that are not fills.
            // Measured live 2026-09-27, a withdrawal of the whole PRL balance
            // went in flight seconds after a twap started; a withdrawal in
            // flight is subtracted from inventory at once, because the coins are
            // already gone, so the ramp was interpolating from the full balance
            // while the bot read inventory as 0 -- an error of the whole
            // balance pins the lean at full and turns a six-hour schedule into "buy as fast as the caps
            // allow", which is the one thing a TWAP exists to avoid.
            ProgramKind::Twap => {
                self.ramp_from(now, self.start_qty.min(pf.base_qty), self.target_qty)
            }

            // The quantity that puts `ratio` of total value in the base asset.
            // Recomputed from live price every loop, so it falls as price rises
            // and rises as price falls.
            ProgramKind::Balanced | ProgramKind::Learned => {
                if pf.price.is_zero() {
                    return self.start_qty;
                }
                let ratio = self.ratio.unwrap_or(dec!(0.5));
                // Banked profit sits outside the ratio and is held entirely in
                // the base asset, so only the working capital is split:
                //     (ratio x (V - B) + B) / P
                // With nothing banked this is the plain ratio; as profit
                // accrues the base target climbs above it, which is how
                // spread capture becomes a growing pile of the base asset
                // rather than a growing pile of quote.
                let banked = self.banked.max(Decimal::ZERO).min(pf.value());
                let want = (ratio * (pf.value() - banked) + banked) / pf.price;
                // A deadline on a balanced program governs only how gradually we
                // approach the first target -- it never completes.
                self.ramp(now, want)
            }
        }
    }

    /// Linear interpolation from `start_qty` to `to` across the schedule.
    fn ramp(&self, now: i64, to: Decimal) -> Decimal {
        self.ramp_from(now, self.start_qty, to)
    }

    /// `ramp`, from an anchor other than `start_qty`.
    fn ramp_from(&self, now: i64, from: Decimal, to: Decimal) -> Decimal {
        let Some(end) = self.ends_at else { return to };
        if now >= end {
            return to;
        }
        if now <= self.started_at || end <= self.started_at {
            return from;
        }
        let frac = Decimal::from(now - self.started_at) / Decimal::from(end - self.started_at);
        from + (to - from) * frac
    }

    /// Fraction of the schedule elapsed, 0..=1.
    pub fn schedule_progress(&self, now: i64) -> Decimal {
        match self.ends_at {
            Some(end) if end > self.started_at => num::clamp(
                Decimal::from(now - self.started_at) / Decimal::from(end - self.started_at),
                Decimal::ZERO,
                Decimal::ONE,
            ),
            _ => Decimal::ZERO,
        }
    }

    /// The side a fixed-target program is net working. `None` once at target.
    pub fn working_side(&self) -> Option<Side> {
        let delta = self.target_qty - self.start_qty;
        if delta > Decimal::ZERO {
            Some(Side::Buy)
        } else if delta < Decimal::ZERO {
            Some(Side::Sell)
        } else {
            None
        }
    }

    /// True when a fill at `price` on `side` would violate the program's price
    /// limit. A limit of 0.90 on an accumulate means "never pay above 0.90".
    /// How fast this glide's clock should run at `price`, where 1.0 is real
    /// time.
    ///
    /// Zero below the floor: not allowed to trade, so no schedule passes.
    /// Above it, faster the better the price -- `gain` is the multiplier per
    /// unit of relative excess, so a gain of 5 makes a price 10% over the
    /// floor run at 1.5x. Capped, because a spike should not finish the whole
    /// distribution in an afternoon.
    ///
    /// With no floor set there is nothing to measure against and the clock
    /// runs at real time.
    pub fn clock_rate(&self, side: Side, price: Decimal, gain: Decimal, max: Decimal) -> Decimal {
        let Some(limit) = self.limit_price else { return Decimal::ONE };
        if self.violates_limit(side, price) {
            return Decimal::ZERO;
        }
        if limit <= Decimal::ZERO || gain <= Decimal::ZERO {
            return Decimal::ONE;
        }
        let excess = match side {
            Side::Sell => (price - limit) / limit,
            Side::Buy => (limit - price) / limit,
        };
        num::clamp(Decimal::ONE + gain * excess.max(Decimal::ZERO), Decimal::ZERO, max.max(Decimal::ONE))
    }

    /// Move a price onto the right side of the limit instead of rejecting it.
    ///
    /// "Never sell below 1.40" should mean "if you were going to quote 1.38,
    /// quote 1.40" -- not "quote nothing". Rejecting outright left a hole
    /// between the floor and wherever the next rung happened to land: live,
    /// with the book at 1.36 and the floor at 1.40, every near ask was dropped
    /// and the cheapest surviving order was 1.64, so the floor silently became
    /// a 1.64 floor. Clamping puts a wall exactly where the limit is, which is
    /// what the limit was asking for.
    ///
    /// Returns `None` only when the clamp would cross the book, which is the
    /// one case where there is no honest price to quote.
    pub fn clamp_to_limit(&self, side: Side, price: Decimal) -> Option<Decimal> {
        let Some(limit) = self.limit_price else { return Some(price) };
        let clamped = match side {
            Side::Sell => price.max(limit),
            Side::Buy => price.min(limit),
        };
        (clamped > Decimal::ZERO).then_some(clamped)
    }

    pub fn violates_limit(&self, side: Side, price: Decimal) -> bool {
        let Some(limit) = self.limit_price else { return false };
        match side {
            Side::Buy => price > limit,
            Side::Sell => price < limit,
        }
    }
}

/// Mirrors the `orders` table; sqlx binds every column.
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct OrderRow {
    pub client_id: String,
    pub exchange_id: Option<i64>,
    pub program_id: Option<i64>,
    pub market: String,
    pub side: String,
    pub price: Option<String>,
    pub amount: String,
    pub filled_amount: String,
    pub state: String,
    pub level: Option<i64>,
    pub placed_at: i64,
}

/// A live order the bot believes is resting on the book.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct LiveOrder {
    pub client_id: String,
    pub exchange_id: i64,
    pub side: Side,
    pub price: Decimal,
    pub amount: Decimal,
    pub filled: Decimal,
    pub level: i64,
    pub placed_at: i64,
}

impl LiveOrder {
    pub fn remaining(&self) -> Decimal {
        (self.amount - self.filled).max(Decimal::ZERO)
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct CommandRow {
    pub id: i64,
    pub created_at: i64,
    pub command: String,
    pub args: String,
}

/// Running position accounting, carried forward fill by fill and stored with
/// each fill (`pos_after`, `cost_after`, `realized`). Kept as a record; nothing
/// the owner reads as profit is derived from it -- that is markout since the
/// epoch, see `reporting`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    /// Net base inventory attributable to bot trading.
    pub qty: Decimal,
    /// Total quote cost of the open position, for average-cost PnL.
    pub cost: Decimal,
    pub realized: Decimal,
    pub fees: Decimal,
}

impl Position {
    #[cfg(test)]
    pub fn avg_price(&self) -> Option<Decimal> {
        if self.qty.is_zero() { None } else { Some(self.cost / self.qty) }
    }

    /// Apply a fill using average-cost accounting. Buys add to the position at
    /// cost; sells release cost pro rata and book the difference as realized.
    /// Fees are tracked separately so spread capture stays legible next to them.
    pub fn apply(&mut self, side: Side, price: Decimal, amount: Decimal, fee: Decimal) {
        self.fees += fee;
        match side {
            Side::Buy => {
                self.qty += amount;
                self.cost += price * amount;
            }
            Side::Sell => {
                let avg = if self.qty > Decimal::ZERO {
                    self.cost / self.qty
                } else {
                    price
                };
                let closing = amount.min(self.qty.max(Decimal::ZERO));
                self.realized += (price - avg) * closing;
                self.cost -= avg * closing;
                self.qty -= amount;
                // Selling past flat opens a short; cost tracks it at the fill price.
                if self.qty < Decimal::ZERO {
                    self.cost = avg * self.qty;
                }
            }
        }
    }

}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn selling(floor: Option<Decimal>) -> Program {
        Program {
            id: 1,
            market: "prl".into(),
            kind: ProgramKind::Distribute,
            state: "active".into(),
            start_qty: dec!(68478),
            target_qty: dec!(0),
            ratio: None,
            limit_price: floor,
            budget: None,
            started_at: 0,
            ends_at: Some(604_800_000),
            banked: Decimal::ZERO,
            note: None,
        }
    }

    /// Below the floor the clock stops dead.
    ///
    /// "Sell over seven days" and "never below 1.40" conflict the moment price
    /// dips under, and the owner chose the floor. A clock that kept running
    /// would fall behind and then sell faster to catch up -- the cheap selling
    /// the floor exists to prevent.
    #[test]
    fn below_the_floor_the_clock_stops() {
        let p = selling(Some(dec!(1.40)));
        assert_eq!(p.clock_rate(Side::Sell, dec!(1.39), dec!(5), dec!(3)), Decimal::ZERO);
        assert_eq!(p.clock_rate(Side::Sell, dec!(0.90), dec!(5), dec!(3)), Decimal::ZERO);
    }

    /// At the floor it runs at real time; above it, faster.
    #[test]
    fn a_better_price_runs_the_clock_faster() {
        let p = selling(Some(dec!(1.40)));
        let rate = |px| p.clock_rate(Side::Sell, px, dec!(5), dec!(3));
        assert_eq!(rate(dec!(1.40)), Decimal::ONE, "at the floor, real time");
        // 1.54 is 10% over: 1 + 5 x 0.10.
        assert_eq!(rate(dec!(1.54)), dec!(1.5));
        assert_eq!(rate(dec!(1.68)), dec!(2.0), "20% over");
        assert!(rate(dec!(1.54)) < rate(dec!(1.68)), "better price, faster");
    }

    /// One spike must not finish the whole distribution in an afternoon.
    #[test]
    fn the_speed_up_is_capped() {
        let p = selling(Some(dec!(1.40)));
        assert_eq!(p.clock_rate(Side::Sell, dec!(14), dec!(5), dec!(3)), dec!(3), "10x price");
        assert_eq!(p.clock_rate(Side::Sell, dec!(1000), dec!(5), dec!(3)), dec!(3));
    }

    /// With no floor there is nothing to measure against, so real time.
    #[test]
    fn no_floor_means_no_repacing() {
        let p = selling(None);
        assert_eq!(p.clock_rate(Side::Sell, dec!(0.10), dec!(5), dec!(3)), Decimal::ONE);
        assert_eq!(p.clock_rate(Side::Sell, dec!(99), dec!(5), dec!(3)), Decimal::ONE);
    }

    /// A gain of zero keeps the pause but drops the acceleration.
    #[test]
    fn zero_gain_still_pauses_below_the_floor() {
        let p = selling(Some(dec!(1.40)));
        assert_eq!(p.clock_rate(Side::Sell, dec!(1.39), Decimal::ZERO, dec!(3)), Decimal::ZERO);
        assert_eq!(p.clock_rate(Side::Sell, dec!(2.00), Decimal::ZERO, dec!(3)), Decimal::ONE);
    }

    #[test]
    fn the_slow_programs_round_trip_and_are_twaps() {
        for (k, name) in [(ProgramKind::TwapSell, "twapsell"), (ProgramKind::TwapBuy, "twapbuy")] {
            assert_eq!(k.as_str(), name);
            assert_eq!(ProgramKind::parse(name), Some(k));
            assert!(k.is_slow() && k.is_twap() && k.is_execution());
            assert!(!k.is_scheduled(), "paced by the tape, never by a clock");
        }
        assert_eq!(ProgramKind::parse("buyslow"), Some(ProgramKind::TwapBuy));
        assert!(!ProgramKind::Twap.is_slow(), "the scheduled twap is planned by `plan`");
    }
}
