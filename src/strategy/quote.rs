//! Quote generation.
//!
//! The whole strategy is one idea: **the program supplies a target inventory,
//! and quoting is the act of closing the gap to it while getting paid.**
//!
//! A TWAP buy is a target that ramps up with time. A neutral market-making
//! stance is a target that sits still. Take-profit is a target that steps down.
//! Because all three are the same number, there is one quoting path, and
//! switching mode at runtime is just changing the target — no separate
//! execution engine, no mode-specific code to get out of sync.
//!
//! Inventory error then does double duty:
//!   * it skews the *price* we quote at (reservation price), and
//!   * it skews the *sizes* on each side,
//! so falling behind a TWAP schedule automatically makes the bot lean into the
//! touch and quote bigger, and getting ahead makes it back off — which is the
//! catch-up behaviour a TWAP needs, without a scheduler.
//!
//! Everything that could move the price is then clamped on the way out:
//! `max_improve_ticks` (default 0) means we may join the best bid but never
//! better it, so our quoting cannot by itself shift the displayed price.

use rust_decimal::{Decimal, MathematicalOps};

/// `growth^level`, saturating instead of panicking.
///
/// `Decimal::powi` panics on overflow, and both the exponent and the base come
/// from settings an operator can change on a running bot. A panic here is on
/// the main loop, every tick, from a value that is persisted -- so it would
/// exhaust the restart budget and stop the bot for good.
pub(crate) fn growth_pow(base: Decimal, level: i64) -> Decimal {
    base.checked_powi(level).unwrap_or(Decimal::ONE)
}
use rust_decimal_macros::dec;

use crate::config::Params;
use crate::db::models::{Portfolio, Program};
use crate::exchange::{Market, OrderBook, Side};
use crate::num;
use crate::strategy::arms::ArmScales;

/// Deep rungs are numbered from here, so a quote's level says which ladder it
/// came from. The placement guard needs that to know which cap applies.
pub const DEEP_LEVEL_BASE: i64 = 100;

/// How far a cross-market reference may sit from the local mid before we stop
/// believing it. Wide enough for a genuinely stale book, tight enough that a
/// mangled price or a wrong conversion is ignored rather than traded on.
const REFERENCE_BAND: Decimal = dec!(0.25);

/// How far past `max_participation` we go from throttling to standing down.
///
/// Below this we quote smaller, which reduces our share by the same mechanism
/// the cap asks for while staying in the market. Above it, our share is so far
/// from the cap that the sensible reading is that something is wrong rather
/// than that we are quoting a little too eagerly.
const PARTICIPATION_HALT_MULTIPLE: Decimal = dec!(3);

#[derive(Debug, Clone, PartialEq)]
pub struct DesiredQuote {
    pub side: Side,
    pub price: Decimal,
    pub amount: Decimal,
    pub level: i64,
    /// How far the order rests on the passive side of fair value, in basis
    /// points, at placement. Recorded with the order so every fill can be
    /// attributed to the ARM it was a bet on -- see `strategy::arms`. This is
    /// the placement distance, not the distance at the fill: only the former
    /// is something the strategy chooses.
    pub bps: Decimal,
    /// The five-minute range of fair value at placement, as a fraction of
    /// price: the other half of the arm's key (`strategy::arms::Regime`).
    /// Recorded with the order for the same reason as `bps`.
    pub recent_move: Decimal,
    /// The stranger book's spread at placement, in ticks, our own size
    /// stripped: the last part of the arm's key (`strategy::arms::Width`).
    /// On a tick-quantised book this, not `bps`, says whether the rung is
    /// the touch of a wide book or a tick behind the touch of a tight one.
    pub width_ticks: Decimal,
}

/// Why the bot is standing down, when it is.
#[derive(Debug, Clone, PartialEq)]
pub enum Halt {
    Disabled,
    NoProgram,
    EmptyBook,
    PriceOutOfBand { price: Decimal },
    Volatile { moved: Decimal },
    ParticipationExceeded { share: Decimal },
    ReferenceDisagrees { reference: Decimal, local: Decimal },
    /// A book priced off a deeper one has no price from it this loop.
    NoReference,
    /// TWAP mode, and this book's asset is not the one being worked.
    TwapOnly,
}

impl Halt {
    pub fn reason(&self) -> String {
        match self {
            Halt::Disabled => "disabled via settings".into(),
            Halt::NoProgram => "no active program".into(),
            Halt::EmptyBook => "order book has no two-sided market".into(),
            Halt::PriceOutOfBand { price } => format!("price {price} outside the configured band"),
            Halt::Volatile { moved } => {
                format!("fair value moved {:.2}% inside the vol window", moved * dec!(100))
            }
            Halt::ReferenceDisagrees { reference, local } => format!(
                "this book says {local}, the deeper book says {reference} — not quoting blind"
            ),
            Halt::ParticipationExceeded { share } => {
                format!("our share of volume is {:.1}%, over the cap", share * dec!(100))
            }
            Halt::TwapOnly => {
                "TWAP mode: this book's asset is not the one being worked".into()
            }
            Halt::NoReference => {
                "no fair value from the deeper book this loop — not quoting this one off its own residue"
                    .into()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub quotes: Vec<DesiredQuote>,
    pub halt: Option<Halt>,
    pub fair_value: Option<Decimal>,
    pub target_inventory: Decimal,
}

impl Decision {
    fn halted(h: Halt, target: Decimal) -> Decision {
        Decision { quotes: vec![], halt: Some(h), fair_value: None, target_inventory: target }
    }
}

pub struct QuoteInput<'a> {
    pub market: &'a Market,
    pub book: &'a OrderBook,
    pub program: Option<&'a Program>,
    pub params: &'a Params,
    /// Base inventory the strategy steers. This is the exchange balance, not
    /// the bot's traded position, so a manual deposit or withdrawal is picked
    /// up on the next loop.
    pub inventory: Decimal,
    /// Free base, for sizing asks.
    pub base_available: Decimal,
    /// Free quote, for sizing bids.
    pub quote_available: Decimal,
    /// Free *plus* locked quote, for valuing the portfolio.
    ///
    /// Valuation has to use the total, or placing a bid lowers the measured
    /// portfolio value (the cash is locked), which lowers a value-weighted
    /// target, which makes the bid no longer wanted, which cancels it and frees
    /// the cash again. The bot oscillated on exactly that loop, replacing its
    /// bid every few seconds and throwing away queue priority each time.
    pub quote_total: Decimal,
    /// Fraction of recent market volume that was ours, for the participation cap.
    pub our_participation: Decimal,
    /// Largest relative fair-value move seen inside the vol window.
    pub recent_move: Decimal,
    /// Fair value taken from a more liquid book, already converted into this
    /// market's quote currency.
    ///
    /// A thin book's own midpoint is not a price, it is the residue of whoever
    /// last left an order there. PRL/USDC sits at 1.00/1.07 with a microprice
    /// near 1.004 while PRL is really worth about 1.037 — quoting off that
    /// would sell the coin two and a half percent under its value on every
    /// fill. So the liquid market sets fair value and the thin market is quoted
    /// around it; that difference IS the edge being harvested.
    pub reference_price: Option<Decimal>,
    /// Whether `reference_price` came from a DIFFERENT book than this one.
    ///
    /// A market is not evidence about itself. The anchor market is handed its
    /// own microprice as a "reference", which is useful for synthesising a
    /// missing side but must never be treated as an outside opinion -- it sits
    /// trivially within `REFERENCE_BAND` of the local mid, so it would unlock
    /// the improve cap on the deepest book we quote, which is the one book
    /// where moving the displayed price costs the most.
    pub reference_is_external: bool,
    /// This book is priced off a deeper one and must not be quoted without
    /// it.
    ///
    /// A thin book's own midpoint is the residue of whoever last left an
    /// order there. The failure this guards against has cost real money once
    /// already: PRL/USDC quoted off its own book sold the coin two and a half
    /// percent under its worth on every fill. When the reference is missing
    /// -- the deeper book unreadable or one-sided, the stablecoin cross
    /// outside its band -- the honest move is to stand down, not to fall back
    /// to the local book as if it were a price.
    pub reference_required: bool,
    /// A floor under the SELL side's arm multiplier, set by the cycle.
    ///
    /// The arms judge a leg alone, which is right for a leg that stands alone
    /// and wrong for one whose only job is to release inventory another leg
    /// acquired cheaply. When the whole cycle clears its floor on the evidence,
    /// this is the share of full size the release leg is allowed even though
    /// its own arm has been defunded -- set to how well it pays RELATIVE to the
    /// best release venue, so the overflow venue stays small exactly in
    /// proportion to how much worse it is. Zero disables it, which is the old
    /// behaviour to the tick. See `strategy::cycle`.
    pub cycle_release_floor: Decimal,
    /// This is the cycle's acquiring book and its round trip has earned the
    /// right to quote both sides at neutral size. See `Quoting::cycle_churn`;
    /// the engine sets it only when the round trip clears the cycle floor.
    pub cycle_churn: bool,
    /// What each rung's ARM has earned, as a multiplier on its size.
    ///
    /// A rung's arm is its side, its distance from fair value at placement
    /// and how fast the market was moving (`recent_move`) when it was
    /// placed; the multiplier is what that arm's measured yield justifies,
    /// from nothing (the exchange minimum) up to full size. See
    /// `strategy::arms`. It is applied to the FINAL size, after the budget
    /// sizing, so it cannot be cancelled the way the inventory skew once
    /// was.
    pub arms: &'a ArmScales,
    pub now_ms: i64,
}

pub fn plan(input: &QuoteInput) -> Decision {
    let QuoteInput { market, book, params, .. } = *input;
    let q = &params.q;
    let tick = market.tick();

    // Fair value is needed to price a value-weighted target, but the book may
    // be one-sided. Fall back to the last trade-able price we can see.
    let pf_price = book
        .microprice()
        .or_else(|| book.best_bid().map(|(p, _)| p))
        .or_else(|| book.best_ask().map(|(p, _)| p))
        .unwrap_or(Decimal::ZERO);
    let portfolio = Portfolio {
        price: pf_price,
        base_qty: input.inventory,
        quote_qty: input.quote_total,
    };
    let target = input
        .program
        .map(|p| p.target_at(input.now_ms, portfolio))
        .unwrap_or(input.inventory);

    if !params.enabled {
        return Decision::halted(Halt::Disabled, target);
    }
    let Some(program) = input.program else {
        return Decision::halted(Halt::NoProgram, target);
    };
    // TWAP mode: nothing quotes except the program being worked.
    //
    // Before the book, the reference and the breakers, because this is not a
    // condition that might clear on the next loop -- it is the operator having
    // said that this book is not to trade. The cycle's valve and the recycle
    // crossing are gated on the same flag at the caller, so the money stays
    // where the TWAP can spend it.
    if params.q.twap_only && !program.kind.is_buy_only() {
        return Decision::halted(Halt::TwapOnly, target);
    }
    // Before the book is even looked at: a book that is priced off a deeper
    // one has nothing to say about price on its own. Only an EXTERNAL
    // reference counts; a market handed its own microprice is not referenced.
    if input.reference_required && !(input.reference_price.is_some() && input.reference_is_external)
    {
        return Decision::halted(Halt::NoReference, target);
    }
    // A one-sided book, once our own size is stripped, is the normal state of
    // a market we are the only maker on. Halting there cancels our remaining
    // side too, which empties the book for real and leaves us out until a
    // stranger posts. With a reference price from a deeper book we can still
    // quote around fair value, so synthesise the missing side a tick away.
    let (best_bid, best_ask) = match (book.best_bid(), book.best_ask()) {
        (Some((b, _)), Some((a, _))) => (b, a),
        (Some((b, _)), None) => match input.reference_price {
            Some(r) if r > b => (b, r + tick),
            _ => return Decision::halted(Halt::EmptyBook, target),
        },
        (None, Some((a, _))) => match input.reference_price {
            Some(r) if r < a => ((r - tick).max(market.min_price), a),
            _ => return Decision::halted(Halt::EmptyBook, target),
        },
        (None, None) => return Decision::halted(Halt::EmptyBook, target),
    };

    // The width of the book the rungs are placed into, for the arm they are
    // a bet on. A synthesised side sits a tick out, so this is never below
    // one on a book we quote.
    let width_ticks = if tick > Decimal::ZERO { ((best_ask - best_bid) / tick).round_dp(2) } else { Decimal::ONE };

    // ---- fair value -------------------------------------------------------
    // Microprice leans toward the thinner side, which is the side about to
    // trade, then clamped inside the touch so a lopsided book cannot drag our
    // quotes through the spread.
    // Fair value.
    //
    // A reference from a deeper book is NOT clamped into the local touch. That
    // clamp is what the thin book is wrong about: PRL/USDC showed a 1.06 ask
    // against a true value of 1.088, and clamping pulled our fair value 2.6%
    // below the asset's worth -- on the very book this exists to exploit.
    // Crossing is already prevented downstream by `clamp_price`, so the clamp
    // here bought no safety and cost real money.
    //
    // It is still bounded: a reference more than `REFERENCE_BAND` from the
    // local mid is treated as bad data and ignored.
    let local_mid = (best_bid + best_ask) / Decimal::TWO;
    let (fv, trust_reference) = match input.reference_price {
        // Only an EXTERNAL reference may override the local touch. A book
        // quoting off its own reflection would improve the touch every loop,
        // with the control that exists to stop that switched off.
        Some(r)
            if input.reference_is_external
                && !local_mid.is_zero()
                && ((r - local_mid) / local_mid).abs() <= REFERENCE_BAND =>
        {
            (r, true)
        }
        // An EXTERNAL reference that disagrees wildly is not noise to ignore.
        //
        // Falling back to the local book assumes the local book is right, and
        // on a thin market it is precisely the thing that is wrong: PRL/USDC
        // holds a stranger's quotes from before a 29% move while PRL/USDT says
        // what PRL is worth now. Quoting the stale book there sells 15% under
        // value to whoever is watching. Standing down loses a spread; quoting
        // blind loses the difference.
        Some(r) if input.reference_is_external && !local_mid.is_zero() => {
            return Decision::halted(
                Halt::ReferenceDisagrees { reference: r, local: local_mid },
                target,
            );
        }
        _ => (
            num::clamp(book.microprice().unwrap_or(local_mid), best_bid, best_ask),
            false,
        ),
    };

    // ---- circuit breakers -------------------------------------------------
    if let Some(floor) = params.risk.price_floor
        && fv < floor
    {
        return Decision::halted(Halt::PriceOutOfBand { price: fv }, target);
    }
    if let Some(ceil) = params.risk.price_ceiling
        && fv > ceil
    {
        return Decision::halted(Halt::PriceOutOfBand { price: fv }, target);
    }
    if input.recent_move > params.risk.max_move {
        return Decision::halted(Halt::Volatile { moved: input.recent_move }, target);
    }
    // There is deliberately no drawdown breaker here.
    //
    // The one that used to live at this line marked the market-keyed position
    // to market and halted below a threshold. Three things were wrong with it,
    // and together they made it a trap rather than a safety net:
    //
    //  * The baseline is seeded per BASE ASSET while the position is keyed per
    //    MARKET, so the position starts at zero and our first sells open a
    //    phantom short. A rally then shows a large unrealized "loss" on PRL we
    //    never owed.
    //  * `Position::fees` sums PRL fees and USDT fees as one number, so the
    //    threshold is compared against a quantity with no single unit.
    //  * Most importantly it inverts the strategy. A balanced program holds
    //    PRL on purpose. Marking that to market and standing down when PRL
    //    falls halts the bot exactly when it should be buying -- and it halts
    //    by cancelling everything, so it also stops the selling that would
    //    reduce the exposure it is worried about.
    //
    // Real protection is already here and is not direction-blind: position
    // limits that scale with the account, the volatility breaker, the
    // participation throttle, and the price band. If an account-level drawdown
    // halt is wanted, it belongs on account value net of deposits -- which
    // `Lifetime::change` already computes -- not on a per-market mark.
    // A cap of 1.0 (or above) is a deliberate opt-out for a book we are the
    // liquidity on, where being most of the volume is the business.
    //
    // Over the cap we quote SMALLER, we do not stand down. Halting cancels
    // every resting order and our own fills only leave the window as it rolls,
    // so a halt lasts until the window expires -- up to four hours, during
    // which we earn nothing and the book loses its liquidity. It also fires
    // hardest exactly when it should not: overnight the market's volume drops
    // roughly tenfold while our passive orders keep filling, so a quiet night
    // reads as dominance. Scaling size down reduces our share by the same
    // mechanism the cap is asking for, and keeps us in the market while it
    // works.
    //
    // A hard halt remains for the genuinely pathological case, where we really
    // are nearly the entire tape and something is wrong.
    let mut participation_throttle = Decimal::ONE;
    if q.max_participation < Decimal::ONE && input.our_participation > q.max_participation {
        if input.our_participation > q.max_participation * PARTICIPATION_HALT_MULTIPLE {
            return Decision::halted(
                Halt::ParticipationExceeded { share: input.our_participation },
                target,
            );
        }
        participation_throttle =
            num::clamp(q.max_participation / input.our_participation, dec!(0.1), Decimal::ONE);
    }

    // ---- limits that scale with the account -------------------------------
    //
    // Fixed numbers here are a trap. They are right for one account size and
    // silently wrong for every other, and the failure is invisible: a stale
    // `max_position` does not warn, it just stops the bot bidding, so it sells
    // one way and harvests none of the volatility it exists to harvest. That
    // happened after a deposit tripled the holding.
    let equity = portfolio.value();
    let mut params = params.clone();

    if params.risk.max_position_factor > Decimal::ZERO {
        // A multiple of what we are actually trying to hold. Always leaves
        // room to buy: the balanced target is at most half the holding, and
        // three times that is above it whatever the account size.
        params.risk.max_position = (target * params.risk.max_position_factor)
            .max(input.inventory * dec!(1.05));
    }
    if params.risk.max_order_pct_of_equity > Decimal::ZERO && equity > Decimal::ZERO {
        params.risk.max_order_notional = equity * params.risk.max_order_pct_of_equity;
    }
    if params.q.size_pct_of_equity > Decimal::ZERO && equity > Decimal::ZERO {
        let px = if pf_price > Decimal::ZERO { pf_price } else { Decimal::ONE };
        params.q.size_base = equity * params.q.size_pct_of_equity / px;
    }
    if params.q.deep_pct_of_inventory > Decimal::ZERO {
        params.q.deep_size = input.inventory * params.q.deep_pct_of_inventory;
    }
    // Over the participation cap we take the same sizes down rather than
    // pulling out of the market. Applied last, so it scales whatever the rules
    // above decided.
    if participation_throttle < Decimal::ONE {
        params.q.size_base *= participation_throttle;
        params.q.deep_size *= participation_throttle;
    }
    let params = &params;
    let q = &params.q;

    // ---- inventory error --------------------------------------------------
    // Normalised to [-1, 1]. The scale it is normalised against is what decides
    // whether the skews below do anything at all.
    //
    // Against the hard position bound alone, they do not: with a 100,000 bound
    // and a 15,000 target, being 5,000 out is an error of 0.06, which times a
    // 1.5-tick skew is a tenth of a tick and rounds away entirely. The bot
    // would quote symmetrically while believing it was leaning — and leaning is
    // the whole mechanism behind both the TWAP catch-up and the rebalance.
    //
    // So the scale is the *tighter* of two things: how much inventory error we
    // consider "full" (a couple of ladders' worth by default), and how much
    // room is left before the hard bound. The first makes the lean responsive
    // in normal conditions; the second makes it slam on near a limit.
    let err_raw = input.inventory - target;
    let room = if err_raw > Decimal::ZERO {
        params.risk.max_position - target
    } else {
        target - params.risk.min_position
    };
    // Derived from the TARGET, not from the ladder. Two ladders' worth of the
    // rung ceiling came to 76% of equity once the ceiling was 8% of equity a
    // rung, so a 30% shortfall against target read as an error of 0.4 and the
    // wrong-way side kept quoting at 70% size through a rally. Ten percent of
    // the target is the size of error a rebalance actually produces, and the
    // lean is meant to be at full strength by then.
    let nominal = if q.inventory_scale > Decimal::ZERO {
        q.inventory_scale
    } else {
        // A program flattening to zero has no target to scale by; the holding
        // being worked off stands in.
        let basis = if target > Decimal::ZERO { target } else { input.inventory.abs() };
        basis * q.inventory_scale_pct.max(dec!(0.001))
    };
    let scale = nominal.min(room.max(Decimal::ZERO)).max(tick);
    let err = num::clamp(err_raw / scale, dec!(-1), dec!(1));

    // ---- reservation price ------------------------------------------------
    // Long (err > 0) pushes the reservation price down: we ask closer and bid
    // further away, so inventory drains back toward target.
    let reservation = fv - err * q.inventory_skew_ticks * tick;
    // ---- half spread, widened by volatility --------------------------------
    //
    // A maker is paid the spread for the risk that price runs straight through
    // a resting order. Measured live, fills that looked profitable at the
    // instant they happened were a loss one minute later: we were being
    // picked off, because a 0.9% spread does not pay for a market ranging 20%
    // in five minutes.
    //
    // So the floor is a floor, not the answer. In calm markets `recent_move` is
    // small and this reverts to `min_half_spread_ticks` on its own; in a
    // dislocation it stands further back rather than subsidising whoever is
    // running the price. The ceiling stops a mangled volatility reading parking
    // our quotes somewhere nobody will ever trade.
    //
    // These controls are not superseded by the arms, and they do not fight
    // them: they decide WHERE a rung rests, and so which arm it is a bet on;
    // the arms decide how much to stake on it. The arms cannot move a rung.
    // Nor does the floor keep a rung off the losing arm: it is a floor on
    // the half-spread from the RESERVATION price, and the inventory lean
    // moves that half a tick toward the wanted side, so on prlusdt under a
    // one-tick floor rung 0 still landed under 50 bp from fair value 35% of
    // the time (315 of 894 placements on 2026-09-23; 34% in 50-100, 27% in
    // 100-200), with rung 1 mostly in 100-200. What keeps the stake on the
    // <50 arm small is that arm sized to what it has earned, not the spread.
    let floor = q.min_half_spread_ticks * tick;
    let ceiling = if q.max_half_spread_pct > Decimal::ZERO {
        (fv * q.max_half_spread_pct).max(floor)
    } else {
        Decimal::MAX
    };
    let half_spread = num::clamp(input.recent_move * q.vol_spread_share * fv, floor, ceiling);

    // ---- sizing base ------------------------------------------------------
    // Each side's multiplier runs from 0 (fully suppressed, wrong way) through
    // 1 (neutral) to 1 + skew (right way), so a TWAP that has fallen behind
    // quotes bigger on the side it needs without any schedule arithmetic.
    //
    // On the cycle's acquiring book the skew is dropped and both sides quote
    // neutral. Buying and selling the same coin on the one book that pays a
    // round trip is CHURN, not drift: it ends the day at the same inventory
    // either way, so suppressing one side of it buys no convergence and costs
    // the leg. The prices still lean -- only the sizes go neutral -- so the
    // rungs stay on the arms they were measured as. See `Quoting::cycle_churn`.
    let skew = if input.cycle_churn { Decimal::ZERO } else { q.inventory_size_skew };
    let bid_mult = num::clamp(Decimal::ONE - skew * err, Decimal::ZERO, Decimal::ONE + skew);
    let ask_mult = num::clamp(Decimal::ONE + skew * err, Decimal::ZERO, Decimal::ONE + skew);

    // ---- a book that is only a cycle leg ----------------------------------
    //
    // No ladder of its own: the bid never rests and the ask rests only as much
    // as the cycle's release valve asks for, which on a shut valve is nothing.
    // An execution program suspends the rule outright -- working a quantity the
    // owner asked for is not making a market, and the deep book is where size
    // belongs. See `Quoting::cycle_leg_only`.
    let leg_only = q.cycle_leg_only && !program.kind.is_execution();
    // A buy-only program quotes no ask, anywhere, at any distance.
    //
    // Not "a small ask", not "an ask far away": none. The inventory lean alone
    // does not achieve this -- run ahead of the ramp and the error flips sign,
    // and the ask side is then the *right-way* side and quotes bigger. Nor does
    // `min_position`: it is a floor under inventory, so every unit bought
    // reopens room to sell one. See `ProgramKind::is_buy_only`.
    let sells_allowed = !program.kind.is_buy_only();
    let leg_ask = (!leg_only || input.cycle_release_floor > Decimal::ZERO) && sells_allowed;
    // The cycle can floor the ask, never cut it: a leg the arms already like
    // keeps the size the arms gave it. On a leg-only book there is no arm to
    // respect, so the valve's own share is the whole of the size.
    let ask_scale = |bps: Decimal, mv: Decimal, w: Decimal| -> Decimal {
        if leg_only {
            input.cycle_release_floor
        } else {
            input.arms.scale(Side::Sell, bps, mv, w).max(input.cycle_release_floor)
        }
    };

    let bid_depth = book.depth_within(Side::Buy, q.depth_band);
    let ask_depth = book.depth_within(Side::Sell, q.depth_band);

    let mut quotes = Vec::new();

    // Balance left to commit, drawn down as the ladder is built.
    //
    // Sizing every rung against the same starting balance is the bug that made
    // the bot re-request a full ladder it could not afford on every single
    // loop: each rung passed the check on its own, but together they needed
    // three times the cash. The exchange rejected the surplus, we logged it,
    // and we did it again four seconds later.
    let quote_budget = (input.quote_available - params.risk.reserve_quote).max(Decimal::ZERO);
    let base_budget = (input.base_available - params.risk.reserve_base).max(Decimal::ZERO);

    // Reserve the deep ladder's share UP FRONT. Taking it from whatever the
    // tight ladder leaves behind would mean it is funded only when the tight
    // ladder happens not to want the money -- which is exactly when the market
    // is quiet and the deep rungs are least likely to be needed.
    // A buy-only program has no deep ladder, and reserves nothing for one.
    //
    // The deep rungs are a market maker's device: far-out bids waiting for a
    // wick to trade through them and retrace. Working a quantity is not that
    // bet, and the reservation is taken UP FRONT -- measured live 2026-09-27,
    // `deep_allocation = 0.45` fenced off 45% of a large TWAP to fund five
    // rungs sized from `deep_pct_of_inventory` of an inventory that was still
    // near zero: a sliver of orders holding nearly half the money the working
    // ladder needed. They also churn, because they sit at fixed offsets from
    // fair value, so every cent of drift cancels and replaces all five.
    let deep_share = if q.deep_levels > 0 && sells_allowed {
        num::clamp(q.deep_allocation, Decimal::ZERO, dec!(0.8))
    } else {
        Decimal::ZERO
    };
    let deep_quote_budget = quote_budget * deep_share;
    let deep_base_budget = base_budget * deep_share;
    let mut quote_left = quote_budget - deep_quote_budget;
    let mut base_left = base_budget - deep_base_budget;

    // Room to the hard position bounds, drawn down as the ladder is built.
    //
    // Capping every rung against the SAME headroom lets the ladder breach the
    // bound collectively: three asks each "within headroom" sell three times
    // it. The bounds are the last line of defence on inventory, so they have to
    // hold for the ladder as a whole, not rung by rung.
    let mut buy_room = (params.risk.max_position - input.inventory).max(Decimal::ZERO);
    let mut sell_room = if sells_allowed {
        (input.inventory - params.risk.min_position).max(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };

    // Size the ladder to the money that is actually there.
    //
    // Each side's rungs have relative weights (growth by level), so dividing
    // the side's budget by the sum of those weights gives the per-rung size
    // that makes a NEUTRAL ladder fit exactly. The configured `size_base`
    // stays as a ceiling.
    //
    // The inventory skew is applied AFTER this, never inside it. It used to be
    // divided out here so that the skewed ladder would still fit the budget,
    // and the multiplication downstream then cancelled it exactly: on the
    // side where the budget bound -- always the cash side, since cash is the
    // scarce asset -- the skew had no effect at all. Measured over 22 hours,
    // resting asks were 10% LARGER than bids while the account was 30% under
    // target, and the bot sold heavily out of the PRL it was trying to
    // accumulate, at a negative five-minute markout. A right-way side that
    // wants more than the budget is simply capped by the balance headroom
    // below, rung by rung, which starves the far rungs first.
    let weights: Decimal = (0..q.levels as i64).map(|i| growth_pow(q.level_size_growth, i)).sum();
    let sized = |budget: Decimal, price: Decimal| -> Decimal {
        if q.ladder_fraction <= Decimal::ZERO || weights <= Decimal::ZERO || price <= Decimal::ZERO
        {
            return q.size_base;
        }
        let per_rung = budget * q.ladder_fraction / (weights * price);
        q.size_base.min(per_rung.max(Decimal::ZERO))
    };
    let bid_size_base = sized(quote_left, fv);
    let ask_size_base = sized(base_left * fv, fv);

    // One seed per market per refresh window. Mixing the market id in stops
    // three books drawing the same jitter and moving in lockstep.
    let window = refresh_window(input.now_ms, q.refresh_secs);
    let market_seed = market.id.bytes().fold(0u64, |a, b| a.wrapping_mul(131).wrapping_add(b as u64));
    let seed_for = |side: Side, level: i64| -> u64 {
        window
            .wrapping_mul(0x100_0001)
            .wrapping_add(market_seed)
            .wrapping_add((level as u64).wrapping_mul(0x9E37))
            .wrapping_add(if side == Side::Buy { 1 } else { 7 })
    };


    for level in 0..q.levels as i64 {
        let step = q.level_step_ticks * Decimal::from(level) * tick;
        let growth = growth_pow(q.level_size_growth, level);

        // --- bid ---
        //
        // Price first, then size. The program's limit MOVES the price rather
        // than dropping the order (see `Program::clamp_to_limit`: dropping it
        // left a hole between the floor and the next surviving rung, so the
        // floor quietly became whatever price happened to land above it), and
        // the arm a rung is a bet on is decided by where it finally rests.
        if let Some(price) = clamp_price(
            Side::Buy,
            num::floor_to(reservation - half_spread - step, market.price_precision),
            best_bid,
            best_ask,
            tick,
            q.max_improve_ticks,
            trust_reference,
            market,
        )
        .and_then(|p| program.clamp_to_limit(Side::Buy, p))
        .map(|p| num::floor_to(p, market.price_precision))
        .filter(|p| *p < best_ask)
        .filter(|_| !leg_only)
        {
            let bps = distance_bps(Side::Buy, price, fv);
            let scale = input.arms.scale(Side::Buy, bps, input.recent_move, width_ticks);
            let sized = |scale| {
                size_for(
                    params, q, market, bid_size_base, bid_mult * growth * scale, price, bid_depth,
                    buy_room, quote_left / price, seed_for(Side::Buy, level), Decimal::ONE,
                )
            };
            let amount = at_least_min(sized(scale), sized(Decimal::ONE), market, q.probe_dead_arms);
            if viable(amount, price, market) {
                quote_left = (quote_left - amount * price).max(Decimal::ZERO);
                buy_room = (buy_room - amount).max(Decimal::ZERO);
                quotes.push(DesiredQuote { side: Side::Buy, price, amount, level, bps, recent_move: input.recent_move, width_ticks });
            }
        }

        // --- ask ---
        if let Some(price) = clamp_price(
            Side::Sell,
            num::ceil_to(reservation + half_spread + step, market.price_precision),
            best_bid,
            best_ask,
            tick,
            q.max_improve_ticks,
            trust_reference,
            market,
        )
        .and_then(|p| program.clamp_to_limit(Side::Sell, p))
        .map(|p| num::ceil_to(p, market.price_precision))
        .filter(|p| *p > best_bid)
        .filter(|_| leg_ask)
        {
            let bps = distance_bps(Side::Sell, price, fv);
            let scale = ask_scale(bps, input.recent_move, width_ticks);
            let sized = |scale| {
                size_for(
                    params, q, market, ask_size_base, ask_mult * growth * scale, price, ask_depth,
                    sell_room, base_left, seed_for(Side::Sell, level), Decimal::ONE,
                )
            };
            let amount = at_least_min(sized(scale), sized(Decimal::ONE), market, q.probe_dead_arms);
            if viable(amount, price, market) {
                base_left = (base_left - amount).max(Decimal::ZERO);
                sell_room = (sell_room - amount).max(Decimal::ZERO);
                quotes.push(DesiredQuote { side: Side::Sell, price, amount, level, bps, recent_move: input.recent_move, width_ticks });
            }
        }
    }

    // ---- deep rungs ----
    //
    // Far from fair value, sized from their own slice of the balance so they
    // cannot crowd out the tight ladder that does the daily earning. These sit
    // and wait: most of the time they do nothing, and occasionally a wick
    // trades through them and retraces.
    if q.deep_levels > 0 && sells_allowed {
        // Its own reserved slice, plus anything the tight ladder did not use.
        let mut deep_quote = deep_quote_budget + quote_left;
        let mut deep_base = deep_base_budget + base_left;
        for level in 0..q.deep_levels as i64 {
            // Rung 0 sits a couple of ticks outside the touch; every rung
            // after it is a MULTIPLE further out, not a fixed number of ticks.
            //
            // Ticks cannot express "20% higher": on PRL a tick is 0.71% of
            // price, so a tick-spaced ladder bunches up near the touch and the
            // spacing drifts as price moves. A sweep catcher wants its rungs at
            // 1.2x, 1.44x, 1.73x of fair value and to stay there.
            let out = if q.deep_price_step > Decimal::ONE && level > 0 {
                let mult = growth_pow(q.deep_price_step, level);
                (reservation * mult - reservation).abs()
            } else {
                (q.deep_start_ticks + q.deep_step_ticks * Decimal::from(level)) * tick
            };
            // Every deep rung used to be exactly `deep_size`, so three rungs
            // meant three orders of identical size sitting on the book -- which
            // reads as a bug to anyone looking at it, and wastes the far rungs:
            // they are the better price, so they should be the larger order.
            let rung = q.deep_size * growth_pow(q.deep_size_growth, level);

            if let Some(price) = clamp_price(
                Side::Buy,
                num::floor_to(reservation - out, market.price_precision),
                best_bid, best_ask, tick, q.max_improve_ticks, trust_reference, market,
            )
            .and_then(|p| program.clamp_to_limit(Side::Buy, p))
            .map(|p| num::floor_to(p, market.price_precision))
            .filter(|p| *p < best_ask)
            .filter(|_| !leg_only)
            {
                let bps = distance_bps(Side::Buy, price, fv);
                let sized = |scale: Decimal| {
                    let rung = rung * scale;
                    size_for(
                        params, q, market, rung, Decimal::ONE, price, bid_depth, buy_room,
                        deep_quote / price, seed_for(Side::Buy, DEEP_LEVEL_BASE + level), q.deep_cap_multiple,
                    )
                    .min(num::floor_to(rung, market.amount_precision))
                };
                let amount = at_least_min(sized(input.arms.scale(Side::Buy, bps, input.recent_move, width_ticks)), sized(Decimal::ONE), market, q.probe_dead_arms);
                if viable(amount, price, market) {
                    deep_quote = (deep_quote - amount * price).max(Decimal::ZERO);
                    buy_room = (buy_room - amount).max(Decimal::ZERO);
                    quotes.push(DesiredQuote {
                        side: Side::Buy, price, amount, level: DEEP_LEVEL_BASE + level, bps,
                        recent_move: input.recent_move,
                        width_ticks,
                    });
                }
            }

            if let Some(price) = clamp_price(
                Side::Sell,
                num::ceil_to(reservation + out, market.price_precision),
                best_bid, best_ask, tick, q.max_improve_ticks, trust_reference, market,
            )
            .and_then(|p| program.clamp_to_limit(Side::Sell, p))
            .map(|p| num::ceil_to(p, market.price_precision))
            .filter(|p| *p > best_bid)
            .filter(|_| leg_ask)
            {
                let bps = distance_bps(Side::Sell, price, fv);
                let sized = |scale: Decimal| {
                    let rung = rung * scale;
                    size_for(
                        params, q, market, rung, Decimal::ONE, price, ask_depth,
                        sell_room, deep_base, seed_for(Side::Sell, DEEP_LEVEL_BASE + level), q.deep_cap_multiple,
                    )
                    .min(num::floor_to(rung, market.amount_precision))
                };
                let amount = at_least_min(sized(ask_scale(bps, input.recent_move, width_ticks)), sized(Decimal::ONE), market, q.probe_dead_arms);
                if viable(amount, price, market) {
                    deep_base = (deep_base - amount).max(Decimal::ZERO);
                    sell_room = (sell_room - amount).max(Decimal::ZERO);
                    quotes.push(DesiredQuote {
                        side: Side::Sell, price, amount, level: DEEP_LEVEL_BASE + level, bps,
                        recent_move: input.recent_move,
                        width_ticks,
                    });
                }
            }
        }
    }

    // Two rungs can round onto the same tick when the ladder step is finer than
    // the tick. Merge rather than send duplicate orders at one price.
    dedupe_levels(&mut quotes, market, params.risk.max_order_notional);

    Decision { quotes, halt: None, fair_value: Some(fv), target_inventory: target }
}

/// How far `price` rests on the passive side of fair value, in basis points.
/// Negative when the order sits through fair value, which the arm layer
/// treats as the touch bucket.
fn distance_bps(side: Side, price: Decimal, fv: Decimal) -> Decimal {
    if fv <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let away = match side {
        Side::Buy => fv - price,
        Side::Sell => price - fv,
    };
    (away / fv * dec!(10000)).round_dp(1)
}

/// Apply every price constraint that keeps us from moving the market.
///
/// Returns `None` when no acceptable price exists on that side this loop.
#[allow(clippy::too_many_arguments)]
fn clamp_price(
    side: Side,
    want: Decimal,
    best_bid: Decimal,
    best_ask: Decimal,
    tick: Decimal,
    max_improve_ticks: i64,
    trust_reference: bool,
    market: &Market,
) -> Option<Decimal> {
    // The improve cap keeps us from pushing a price we believe in. That is
    // right when the local book IS the price, and wrong when it is a stale
    // residue: on PRL/USDC the nearest stranger's ask sat 11% above fair
    // value, so the cap pinned our ask 7% above fair and we captured 8% of the
    // spread instead of quoting where the asset is actually worth.
    //
    // With fair value from a deeper book, quote where we mean to. Not crossing
    // is still enforced below, which is the guarantee that actually matters.
    if trust_reference {
        let price = match side {
            Side::Buy => want.min(best_ask - tick),
            Side::Sell => want.max(best_bid + tick),
        };
        if price <= Decimal::ZERO || price < market.min_price || price > market.max_price {
            return None;
        }
        return match side {
            Side::Buy if price >= best_ask => None,
            Side::Sell if price <= best_bid => None,
            _ => Some(price),
        };
    }

    let improve = Decimal::from(max_improve_ticks) * tick;
    let price = match side {
        // Never post better than the touch plus the allowance. With the default
        // allowance of zero this collapses to "join the bid, never raise it",
        // which is what keeps a large accumulation invisible in the price.
        Side::Buy => {
            let capped = want.min(best_bid + improve);
            // And never cross: a buy at or above the ask is a taker order.
            capped.min(best_ask - tick)
        }
        Side::Sell => {
            let capped = want.max(best_ask - improve);
            capped.max(best_bid + tick)
        }
    };

    if price <= Decimal::ZERO || price < market.min_price || price > market.max_price {
        return None;
    }
    // The clamps can invert on a one-tick-wide book; drop the quote instead of
    // emitting something that would cross.
    match side {
        Side::Buy if price >= best_ask => None,
        Side::Sell if price <= best_bid => None,
        _ => Some(price),
    }
}

/// Size one rung, then take the smallest of every cap that applies.
#[allow(clippy::too_many_arguments)]
/// A deterministic value in [-1, 1] from a seed.
///
/// Deterministic on purpose: the same seed must give the same ladder, so the
/// reconciler sees no change and leaves the orders resting.
fn jitter_unit(seed: u64) -> Decimal {
    // SplitMix64. Cheap, and good enough to stop a ladder looking machine-made.
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Map to [-1, 1] with three decimals, which is finer than any tolerance.
    let n = (z % 2001) as i64 - 1000;
    Decimal::from(n) / dec!(1000)
}

/// Which refresh window `now` falls in. Everything seeded from this is stable
/// until the window rolls, which is what makes the ladder drip.
fn refresh_window(now_ms: i64, secs: i64) -> u64 {
    (now_ms / (secs.max(1) * 1000)) as u64
}

#[allow(clippy::too_many_arguments)]
fn size_for(
    params: &Params,
    q: &crate::config::Quoting,
    market: &Market,
    size_base: Decimal,
    multiplier: Decimal,
    price: Decimal,
    depth: Decimal,
    position_headroom: Decimal,
    balance_headroom: Decimal,
    seed: u64,
    // Multiplier on the per-order caps. Those caps exist so a single order
    // cannot move the market; an order resting 50% above the touch cannot
    // move anything, because it only trades if somebody sweeps through it,
    // so the far rungs are allowed to be big. 1.0 applies them as written.
    cap_scale: Decimal,
) -> Decimal {
    let mut amount = size_base * multiplier;

    // Jitter so the ladder does not read as a bot at a glance.
    //
    // Seeded from the REFRESH WINDOW, not re-rolled per loop. A fresh random
    // number every six seconds changed every size every six seconds, and a
    // size change past the tolerance cancels and replaces the order: jitter
    // alone accounted for 2,381 size-only replacements in a day, burning queue
    // position and the action budget to look less robotic.
    //
    // Seeded this way the whole ladder is identical within a window, so the
    // reconciler leaves it alone, and it re-rolls together at the boundary.
    // That is also what makes the ladder drip rather than twitch.
    if q.size_jitter > Decimal::ZERO {
        let factor = Decimal::ONE + q.size_jitter * jitter_unit(seed);
        amount *= factor.max(dec!(0.1));
    }

    // Never be more than a slice of the visible book: the surest way to move a
    // thin market is to be most of it.
    if depth > Decimal::ZERO {
        amount = amount.min(depth * q.max_order_pct_of_depth * cap_scale / price);
    }
    // From the EFFECTIVE limits. Reading `input.params` here reached back past
    // the proportional adjustments, so a cap scaled to the account was computed
    // and then silently ignored.
    amount = amount.min(params.risk.max_order_notional * cap_scale / price);
    amount = amount.min(position_headroom);
    amount = amount.min(balance_headroom);

    num::floor_to(amount.max(Decimal::ZERO), market.amount_precision)
}

/// The arm's share can take a rung under the exchange minimum. A rung that
/// is not placed earns no fills, so its arm could never learn anything; and
/// what a fill teaches does not depend on its size, because the yield is per
/// dollar-hour. So the smallest order the exchange accepts is the whole
/// exploration budget, and an arm sized to nothing rests exactly that --
/// provided the caps would have allowed at least that much unscaled. Where
/// they would not (a side the lean has zeroed, a budget that is spent, a
/// position bound reached) the rung stays off, as it always did.
fn at_least_min(
    scaled: Decimal,
    unscaled: Decimal,
    market: &Market,
    probe_dead: bool,
) -> Decimal {
    if scaled >= market.min_amount {
        scaled
    } else if probe_dead {
        unscaled.min(market.min_amount)
    } else {
        // The owner would rather spend nothing on an arm the evidence says
        // loses. The tape still records every public trade and its aggressor,
        // so what this arm would have done stays measurable offline.
        Decimal::ZERO
    }
}

fn viable(amount: Decimal, price: Decimal, market: &Market) -> bool {
    amount >= market.min_amount && amount > Decimal::ZERO && price > Decimal::ZERO
}

/// Merge quotes that landed on the same price.
///
/// The merged size must be re-capped. Summing two rungs can carry the result
/// past the single-order notional limit -- which `place` then refuses, so the
/// quote is silently dropped on every loop and that rung never rests at all.
/// A tight rung and a deep rung colliding is enough to trigger it.
fn dedupe_levels(quotes: &mut Vec<DesiredQuote>, market: &Market, max_notional: Decimal) {
    quotes.sort_by(|a, b| {
        (a.side.as_str(), a.price)
            .partial_cmp(&(b.side.as_str(), b.price))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out: Vec<DesiredQuote> = Vec::with_capacity(quotes.len());
    for qt in quotes.drain(..) {
        match out.last_mut() {
            Some(prev) if prev.side == qt.side && prev.price == qt.price => {
                let merged = prev.amount + qt.amount;
                let capped = if prev.price > Decimal::ZERO {
                    merged.min(max_notional / prev.price)
                } else {
                    merged
                };
                prev.amount = num::floor_to(capped, market.amount_precision);
                prev.level = prev.level.min(qt.level);
                prev.bps = prev.bps.min(qt.bps);
            }
            _ => out.push(qt),
        }
    }
    // A merge can round a size below the exchange minimum; drop those rather
    // than emit an order that will be rejected.
    out.retain(|q| q.amount >= market.min_amount);
    *quotes = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic PRNG, so a failure is reproducible from its seed alone.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545F4914F6CDD1D)
        }
        /// A decimal in [lo, hi], quantised to 4dp.
        fn dec_in(&mut self, lo: f64, hi: f64) -> Decimal {
            let f = (self.next() % 1_000_000) as f64 / 1_000_000.0;
            Decimal::try_from(((lo + (hi - lo) * f) * 10_000.0).round() / 10_000.0)
                .unwrap_or(Decimal::ONE)
        }
        fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[(self.next() % xs.len() as u64) as usize]
        }
    }
    use crate::config::{Quoting, Risk};

    fn market() -> Market {
        Market {
            id: "prlusdt".into(),
            name: "PRL/USDT".into(),
            base_unit: "prl".into(),
            quote_unit: "usdt".into(),
            min_price: dec!(0.01),
            max_price: dec!(100000),
            min_amount: dec!(2),
            amount_precision: 4,
            price_precision: 2,
            state: "enabled".into(),
        }
    }

    /// The real PRL/USDT book shape: bid 0.98, ask 1.00, one free tick between.
    fn book() -> OrderBook {
        OrderBook {
            bids: vec![(dec!(0.98), dec!(16000)), (dec!(0.97), dec!(10000)), (dec!(0.96), dec!(57000))],
            asks: vec![(dec!(1.00), dec!(150)), (dec!(1.01), dec!(4000)), (dec!(1.02), dec!(10000))],
        }
    }

    fn params() -> Params {
        Params {
            // Jitter off: these tests compare quoted sizes, and a ±12% random
            // factor would make the comparisons flaky rather than wrong.
            //
            // The proportional limits are off too, so a test that sets a fixed
            // bound is testing that bound. The tests that exercise the
            // proportional ones switch them back on explicitly.
            q: Quoting {
                levels: 2,
                size_base: dec!(200),
                size_jitter: dec!(0),
                size_pct_of_equity: dec!(0),
                deep_pct_of_inventory: dec!(0),
                ..Default::default()
            },
            risk: Risk {
                max_position: dec!(50000),
                min_position: dec!(0),
                max_position_factor: dec!(0),
                max_order_pct_of_equity: dec!(0),
                ..Default::default()
            },
            enabled: true,
            dry_run: false,
        }
    }

    fn program(kind: crate::db::models::ProgramKind, start: Decimal, target: Decimal, dur_ms: i64) -> Program {
        Program {
            id: 1,
            market: "prlusdt".into(),
            kind,
            state: "active".into(),
            start_qty: start,
            target_qty: target,
            ratio: None,
            banked: Decimal::ZERO,
            limit_price: None,
            started_at: 0,
            ends_at: if dur_ms > 0 { Some(dur_ms) } else { None },
            note: None,
        }
    }

    fn input<'a>(
        m: &'a Market, b: &'a OrderBook, p: &'a Params, prog: &'a Program,
        inventory: Decimal, now: i64,
    ) -> QuoteInput<'a> {
        QuoteInput {
            market: m, book: b, program: Some(prog), params: p,
            cycle_release_floor: Decimal::ZERO,
            cycle_churn: false,
            inventory,
            base_available: dec!(100000),
            quote_available: dec!(100000),
            quote_total: dec!(100000),
            our_participation: Decimal::ZERO,
            recent_move: Decimal::ZERO,
            reference_price: None,
            reference_is_external: false,
            reference_required: false,
            arms: &FULL,
            now_ms: now,
        }
    }

    /// Every arm at full size, for tests that are about something else.
    static FULL: std::sync::LazyLock<ArmScales> = std::sync::LazyLock::new(ArmScales::full);

    /// Every safety invariant, over ten thousand random market states.
    ///
    /// These are the properties that must hold whatever the market does,
    /// because violating any of them either loses money outright or breaks a
    /// promise the strategy is built on. A unit test shows the behaviour in one
    /// situation; this shows it is not an accident of that situation.
    #[test]
    fn safety_invariants_hold_across_random_markets() {
        let mut rng = Rng(0x5EED_1234_ABCD_0001);

        for case in 0..20_000u32 {
            let market = Market {
                id: "prlusdt".into(),
                name: "PRL/USDT".into(),
                base_unit: "prl".into(),
                quote_unit: "usdt".into(),
                min_price: dec!(0.01),
                max_price: dec!(100000),
                min_amount: dec!(2),
                amount_precision: 4,
                price_precision: rng.pick(&[2u32, 4]),
                state: "enabled".into(),
            };
            let tick = market.tick();

            // A book that is always valid but otherwise arbitrary: any width,
            // any depth, sometimes crossed-tight, sometimes yawning.
            let bid = num::floor_to(rng.dec_in(0.02, 50.0), market.price_precision).max(tick);
            let width = Decimal::from(rng.next() % 12 + 1) * tick;
            let ask = bid + width;
            let book = OrderBook {
                bids: vec![
                    (bid, rng.dec_in(0.1, 50_000.0)),
                    ((bid - tick).max(tick), rng.dec_in(0.1, 50_000.0)),
                ],
                asks: vec![
                    (ask, rng.dec_in(0.1, 50_000.0)),
                    (ask + tick, rng.dec_in(0.1, 50_000.0)),
                ],
            };

            let mut p = params();
            p.q.levels = (rng.next() % 5) as u32;
            p.q.deep_levels = (rng.next() % 4) as u32;
            p.q.size_base = rng.dec_in(0.0, 5_000.0);
            p.q.deep_size = rng.dec_in(0.0, 5_000.0);
            p.q.max_improve_ticks = (rng.next() % 8) as i64;
            p.q.inventory_skew_ticks = rng.dec_in(0.0, 6.0);
            p.q.inventory_size_skew = rng.dec_in(0.0, 1.0);
            p.q.min_half_spread_ticks = rng.dec_in(0.0, 5.0);
            p.q.level_step_ticks = rng.dec_in(0.0, 5.0);
            p.q.size_jitter = rng.dec_in(0.0, 0.5);
            p.q.max_order_pct_of_depth = rng.dec_in(0.0, 1.0);
            p.q.deep_allocation = rng.dec_in(0.0, 0.9);
            p.risk.min_position = rng.dec_in(0.0, 5_000.0);
            p.risk.max_position = p.risk.min_position + rng.dec_in(0.0, 100_000.0);
            p.risk.max_order_notional = rng.dec_in(0.0, 5_000.0);
            p.risk.reserve_base = rng.dec_in(0.0, 500.0);
            p.risk.reserve_quote = rng.dec_in(0.0, 500.0);

            let inventory = rng.dec_in(0.0, 120_000.0);
            let mut prog = program(
                rng.pick(&[
                    crate::db::models::ProgramKind::Neutral,
                    crate::db::models::ProgramKind::Balanced,
                    crate::db::models::ProgramKind::Accumulate,
                    crate::db::models::ProgramKind::Distribute,
                    crate::db::models::ProgramKind::Flatten,
                    crate::db::models::ProgramKind::Twap,
                ]),
                rng.dec_in(0.0, 100_000.0),
                rng.dec_in(0.0, 100_000.0),
                (rng.next() % 200_000) as i64,
            );
            prog.ratio = Some(rng.dec_in(0.0, 1.0));
            prog.banked = rng.dec_in(0.0, 50_000.0);

            let base_available = rng.dec_in(0.0, 120_000.0);
            let quote_available = rng.dec_in(0.0, 200_000.0);

            // Arbitrary multipliers per arm, some of them nothing.
            let mut arms = ArmScales::uniform(rng.dec_in(0.0, 1.0));
            for side in [Side::Buy, Side::Sell] {
                for b in crate::strategy::arms::Bucket::ALL {
                    if rng.next() % 2 == 0 {
                        arms = arms.with(side, b, rng.dec_in(0.01, 1.0));
                    }
                }
            }

            let input = QuoteInput {
                market: &market,
                book: &book,
                program: Some(&prog),
                params: &p,
                cycle_release_floor: Decimal::ZERO,
                cycle_churn: false,
                inventory,
                base_available,
                quote_available,
                quote_total: quote_available + rng.dec_in(0.0, 5_000.0),
                our_participation: rng.dec_in(0.0, 1.0),
                recent_move: rng.dec_in(0.0, 0.5),
                reference_is_external: rng.next() % 2 == 0,
                reference_price: if rng.next() % 3 == 0 {
                    Some(rng.dec_in(0.01, 60.0))
                } else {
                    None
                },
                reference_required: rng.next() % 4 == 0,
                arms: &arms,
                now_ms: (rng.next() % 300_000) as i64,
            };

            // Must never panic, whatever it is handed.
            let d = plan(&input);
            let ctx = format!(
                "case {case}: bid={bid} ask={ask} tick={tick} inv={inventory} \
                 base_avail={base_available} quote_avail={quote_available}"
            );

            // 0. A referenced book with no outside price is never quoted off
            //    its own residue, whatever else is going on.
            if input.reference_required
                && !(input.reference_price.is_some() && input.reference_is_external)
            {
                assert_eq!(d.halt, Some(Halt::NoReference), "{ctx}");
                assert!(d.quotes.is_empty(), "{ctx}");
            }

            // 0b. A buy-only program never produces a sell, whatever the
            //     market, the balances, the lean or the arms say. This is the
            //     promise `twap` is built on: asked to spend a deposit, the bot
            //     must not be found selling the position it just bought.
            if prog.kind.is_buy_only() {
                assert!(
                    !d.quotes.iter().any(|q| q.side == Side::Sell),
                    "a buy-only program rested an ask -- {ctx}"
                );
            }

            let mut bid_cost = Decimal::ZERO;
            let mut bid_qty = Decimal::ZERO;
            let mut ask_size = Decimal::ZERO;
            for q in &d.quotes {
                // 1. Never cross the spread. Crossing turns a maker into a
                //    taker: it pays the spread instead of earning it.
                match q.side {
                    Side::Buy => assert!(q.price < ask, "bid {} >= ask {ask} -- {ctx}", q.price),
                    Side::Sell => assert!(q.price > bid, "ask {} <= bid {bid} -- {ctx}", q.price),
                }

                // 2. Prices land on the tick, or the exchange rejects them.
                assert_eq!(
                    q.price, num::round_to(q.price, market.price_precision),
                    "price {} off tick -- {ctx}", q.price
                );
                assert!(q.price > Decimal::ZERO, "non-positive price -- {ctx}");
                assert!(q.price >= market.min_price && q.price <= market.max_price, "{ctx}");

                // 3. Sizes are valid and above the exchange minimum.
                assert!(q.amount >= market.min_amount, "amount {} below min -- {ctx}", q.amount);
                assert!(q.amount.scale() <= market.amount_precision, "{ctx}");

                // 4. Never exceed the single-order notional cap.
                assert!(
                    q.price * q.amount <= p.risk.max_order_notional * dec!(1.0001),
                    "notional {} over cap {} -- {ctx}", q.price * q.amount,
                    p.risk.max_order_notional
                );

                match q.side {
                    Side::Buy => {
                        bid_cost += q.amount * q.price;
                        bid_qty += q.amount;
                    }
                    Side::Sell => ask_size += q.amount,
                }
            }

            // 5. The whole ladder must fit inside the money we actually have,
            //    net of reserves. Overdrawing means orders rejected on every
            //    loop, forever.
            let spendable_quote =
                (quote_available - p.risk.reserve_quote).max(Decimal::ZERO);
            let spendable_base = (base_available - p.risk.reserve_base).max(Decimal::ZERO);
            assert!(
                bid_cost <= spendable_quote * dec!(1.0001),
                "bids cost {bid_cost} over spendable {spendable_quote} -- {ctx}"
            );
            assert!(
                ask_size <= spendable_base * dec!(1.0001),
                "asks size {ask_size} over spendable {spendable_base} -- {ctx}"
            );

            // 6. Hard position bounds are never breached, even if every quote
            //    filled at once.
            assert!(
                inventory + bid_qty <= p.risk.max_position + dec!(1) || bid_qty.is_zero(),
                "buying could breach max_position: inv {inventory} + bids {bid_qty} > {} -- {ctx}\n{:?}",
                p.risk.max_position, d.quotes
            );
            assert!(
                inventory - ask_size >= p.risk.min_position - dec!(1) || ask_size.is_zero(),
                "selling could breach min_position -- {ctx}"
            );

            // 7. No duplicate price on a side: two orders at one price is one
            //    order the reconciler will fight with itself over.
            let mut seen = std::collections::HashSet::new();
            for q in &d.quotes {
                assert!(
                    seen.insert((q.side, q.price)),
                    "duplicate quote at {} -- {ctx}", q.price
                );
            }

            // 8. Every quote knows how far from fair value it rests, and on
            //    the passive side unless a reference let it through, and how
            //    wide the book it was placed into was.
            if let Some(fv) = d.fair_value {
                for q in &d.quotes {
                    let want = distance_bps(q.side, q.price, fv);
                    assert_eq!(q.bps, want, "bps mislabelled -- {ctx}");
                    assert!(q.width_ticks >= Decimal::ONE, "width {} under a tick -- {ctx}", q.width_ticks);
                    if let (Some((b, _)), Some((a, _))) = (book.best_bid(), book.best_ask()) {
                        assert_eq!(q.width_ticks, ((a - b) / tick).round_dp(2), "width mislabelled -- {ctx}");
                    }
                }
            }
        }
    }

    /// Regression, found by the property test: merging two rungs that land on
    /// one price summed their sizes without re-checking the notional cap. The
    /// oversized order was then refused at placement on every loop, so that
    /// rung never rested at all.
    /// The ladder must scale with the money available, not sit at a fixed
    /// nominal. A size right for an account holding 83% base is wrong for the
    /// same account at 50/50, and the difference is threefold.
    /// Limits must follow the account, not a number typed once.
    ///
    /// A fixed max_position stopped the bot bidding after a deposit tripled
    /// the holding: it sold one way and harvested no volatility. The failure
    /// was silent, which is the worst part.
    #[test]
    fn limits_scale_with_the_account() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.risk.max_position_factor = dec!(3);
        p.risk.max_order_pct_of_equity = dec!(0.03);
        p.q.size_pct_of_equity = dec!(0.08);
        p.q.deep_pct_of_inventory = dec!(0.03);
        p.q.size_jitter = dec!(0);
        p.q.levels = 2;
        // Deliberately absurd fixed values: the proportional ones must win.
        p.risk.max_position = dec!(1);
        p.risk.max_order_notional = dec!(1);
        p.q.size_base = dec!(1);

        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));

        // Small account, then one four times bigger.
        let sizes: Vec<Decimal> = [dec!(10000), dec!(40000)]
            .iter()
            .map(|inv| {
                let mut i = input(&m, &b, &p, &prog, *inv, 0);
                i.base_available = *inv;
                i.quote_available = *inv;
                i.quote_total = *inv;
                plan(&i).quotes.iter().map(|q| q.amount).sum::<Decimal>()
            })
            .collect();

        assert!(sizes[0] > Decimal::ZERO, "must quote at all despite max_position = 1");
        assert!(
            sizes[1] > sizes[0] * dec!(2),
            "a 4x account should quote much larger, got {} then {}",
            sizes[0], sizes[1]
        );
    }

    /// The bound must never block bidding just because a deposit landed. With
    /// an all-base account the balanced target is half the holding, so three
    /// times it is always above what we hold.
    #[test]
    fn a_deposit_never_stops_the_bot_bidding() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.risk.max_position_factor = dec!(3);
        p.risk.max_position = dec!(1); // stale, as it was in production
        p.q.size_jitter = dec!(0);
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));

        // All base, no quote: the worst case for the bound.
        let mut i = input(&m, &b, &p, &prog, dec!(42000), 0);
        i.base_available = dec!(42000);
        i.quote_available = dec!(5000);
        i.quote_total = dec!(5000);
        let d = plan(&i);
        assert!(
            d.quotes.iter().any(|q| q.side == Side::Buy),
            "must still bid after a deposit; otherwise it sells one way and harvests nothing"
        );
    }

    #[test]
    fn ladder_size_tracks_available_balance() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 3;
        p.q.size_base = dec!(2000); // ceiling, not target
        p.q.ladder_fraction = dec!(0.55);
        p.q.size_jitter = dec!(0);
        p.q.deep_levels = 0;
        p.risk.max_order_notional = dec!(100000);
        p.risk.max_position = dec!(1000000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let bids_for = |quote: Decimal| {
            let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
            i.quote_available = quote;
            i.quote_total = quote;
            i.base_available = dec!(1000000);
            let d = plan(&i);
            d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount * q.price).sum::<Decimal>()
        };

        let poor = bids_for(dec!(3683));   // today: 83% PRL
        let rich = bids_for(dec!(11500));  // at the 50/50 target
        assert!(poor > Decimal::ZERO && rich > poor, "ladder must grow with the balance");
        assert!(
            rich / poor > dec!(2.5),
            "3x the cash should buy roughly 3x the ladder, got {}x", rich / poor
        );
        // And it must still fit inside what is there.
        assert!(poor <= dec!(3683), "ladder {poor} overdrew 3683");
        assert!(rich <= dec!(11500), "ladder {rich} overdrew 11500");
    }

    /// With `ladder_fraction` off, the fixed size is used unchanged.
    #[test]
    fn a_zero_ladder_fraction_keeps_the_fixed_size() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.ladder_fraction = dec!(0);
        p.q.size_base = dec!(200);
        p.q.levels = 1;
        p.q.size_jitter = dec!(0);
        p.q.deep_levels = 0;
        p.risk.max_order_notional = dec!(100000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.quote_available = dec!(1000000);
        i.quote_total = dec!(1000000);
        let d = plan(&i);
        let bid = d.quotes.iter().find(|q| q.side == Side::Buy).expect("a bid");
        assert!(
            (bid.amount - dec!(200)).abs() < dec!(1),
            "expected the fixed 200, got {}", bid.amount
        );
    }

    #[test]
    fn merged_quotes_respect_the_notional_cap() {
        let m = market();
        let mut quotes = vec![
            DesiredQuote { side: Side::Buy, price: dec!(0.98), amount: dec!(400), level: 0, bps: dec!(100), recent_move: Decimal::ZERO, width_ticks: Decimal::ONE },
            DesiredQuote { side: Side::Buy, price: dec!(0.98), amount: dec!(400), level: 100, bps: dec!(100), recent_move: Decimal::ZERO, width_ticks: Decimal::ONE },
        ];
        dedupe_levels(&mut quotes, &m, dec!(500));
        assert_eq!(quotes.len(), 1, "merged into one order");
        assert!(
            quotes[0].price * quotes[0].amount <= dec!(500),
            "merged notional {} over the cap",
            quotes[0].price * quotes[0].amount
        );
    }

    /// And a merge that rounds below the exchange minimum is dropped, not sent.
    #[test]
    fn a_merge_below_the_minimum_is_dropped() {
        let m = market(); // min_amount 2
        let mut quotes = vec![
            DesiredQuote { side: Side::Sell, price: dec!(1.00), amount: dec!(1), level: 0, bps: dec!(100), recent_move: Decimal::ZERO, width_ticks: Decimal::ONE },
            DesiredQuote { side: Side::Sell, price: dec!(1.00), amount: dec!(0.5), level: 1, bps: dec!(100), recent_move: Decimal::ZERO, width_ticks: Decimal::ONE },
        ];
        dedupe_levels(&mut quotes, &m, dec!(1));
        assert!(quotes.is_empty(), "1.0 unit merge is under min_amount 2");
    }

    /// Regression, found by the property test: every rung was capped against
    /// the SAME position headroom, so a three-rung ladder could sell three
    /// times the room to the bound. The bounds are the last defence on
    /// inventory and must hold for the ladder as a whole.
    #[test]
    fn the_ladder_as_a_whole_respects_the_position_bounds() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 3;
        p.q.size_base = dec!(500);
        p.q.size_jitter = dec!(0);
        p.risk.min_position = dec!(9800);
        p.risk.max_position = dec!(10200);
        p.risk.max_order_notional = dec!(100000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.base_available = dec!(100000);
        i.quote_available = dec!(100000);
        i.quote_total = dec!(100000);

        let d = plan(&i);
        let asks: Decimal = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        let bids: Decimal = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount).sum();
        assert!(asks <= dec!(200), "asks {asks} would sell past min_position");
        assert!(bids <= dec!(200), "bids {bids} would buy past max_position");
    }

    #[test]
    fn never_improves_the_touch_by_default() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        // Deeply under target: maximum pressure to buy.
        let d = plan(&input(&m, &b, &p, &prog, dec!(0), 0));
        assert!(d.halt.is_none());
        for qt in &d.quotes {
            match qt.side {
                Side::Buy => assert!(qt.price <= dec!(0.98), "bid {} improved the touch", qt.price),
                Side::Sell => assert!(qt.price >= dec!(1.00), "ask {} improved the touch", qt.price),
            }
        }
    }

    /// A market handed its OWN book as a reference must still respect the
    /// improve cap.
    ///
    /// The engine gives the anchor market its own microprice so a one-sided
    /// book can still be quoted around. That price sits trivially inside
    /// `REFERENCE_BAND` of the local mid, so before `reference_is_external`
    /// existed it unlocked the improve cap on the deepest book we quote --
    /// the one book where moving the displayed price costs the most. The bot
    /// bid a tick inside its own spread, every loop, with the control that
    /// exists to prevent exactly that switched off.
    #[test]
    fn a_self_reference_never_unlocks_the_improve_cap() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(0), 0);
        // Exactly what the engine hands the anchor market: its own microprice.
        i.reference_price = b.microprice();
        i.reference_is_external = false;

        let d = plan(&i);
        assert!(d.halt.is_none());
        for qt in &d.quotes {
            match qt.side {
                Side::Buy => assert!(
                    qt.price <= dec!(0.98),
                    "bid {} improved the touch off our own reflection", qt.price
                ),
                Side::Sell => assert!(
                    qt.price >= dec!(1.00),
                    "ask {} improved the touch off our own reflection", qt.price
                ),
            }
        }
    }

    /// The flag must actually gate the behaviour: the same reference marked
    /// external is allowed to improve, which is what the thin-book fix needs.
    #[test]
    fn an_external_reference_still_frees_us_from_the_touch() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(0), 0);
        i.reference_price = Some(dec!(0.995));
        i.reference_is_external = true;
        let external = plan(&i);

        i.reference_is_external = false;
        let internal = plan(&i);

        let best_bid = |d: &Decision| {
            d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.price).max()
        };
        assert!(
            best_bid(&external) > best_bid(&internal),
            "an external reference must be allowed to improve where an internal one is not"
        );
    }

    /// The spread must widen when the market is moving and revert when it is
    /// not.
    ///
    /// This is the direct answer to the markout: fills that looked profitable
    /// at the instant they happened were a loss a minute later, because a 0.9%
    /// spread does not pay for a book ranging 20% in five minutes.
    #[test]
    fn the_spread_widens_with_volatility_and_reverts_without_it() {
        let (m, b) = (market(), book()); // 0.98 / 1.00, tick 0.01
        let mut p = params();
        p.q.min_half_spread_ticks = dec!(0.5);
        p.q.vol_spread_share = dec!(0.10);
        p.q.max_half_spread_pct = dec!(0.05);
        p.q.max_improve_ticks = 50; // let the spread, not the touch, decide
        p.risk.max_move = dec!(1000); // the breaker is not what is under test
        p.q.levels = 1;
        p.q.deep_levels = 0;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let spread_of = |move_pct: Decimal| {
            let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
            i.recent_move = move_pct;
            let d = plan(&i);
            let bid = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.price).max();
            let ask = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.price).min();
            (bid, ask)
        };

        // Calm: the floor applies, so we quote tight.
        let (calm_bid, calm_ask) = spread_of(dec!(0.001));
        let calm = calm_ask.unwrap() - calm_bid.unwrap();

        // A 20% five-minute range, which is a normal day on PRL.
        let (wild_bid, wild_ask) = spread_of(dec!(0.20));
        let wild = wild_ask.unwrap() - wild_bid.unwrap();

        assert!(wild > calm, "a moving market must widen us: {wild} vs {calm}");
        // 2x rather than the raw ratio of half-spreads: both sides round to
        // the tick, which on a 1% tick coarsens the comparison considerably.
        assert!(
            wild >= calm * dec!(2),
            "20% of range must widen materially, not cosmetically: {wild} vs {calm}"
        );
    }

    /// However wild the reading, the half-spread stops at the ceiling -- a
    /// mangled volatility number must not park our quotes off the book.
    #[test]
    fn the_volatility_widening_is_capped() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.vol_spread_share = dec!(0.10);
        p.q.max_half_spread_pct = dec!(0.05);
        p.q.max_improve_ticks = 500;
        p.risk.max_move = dec!(1000); // the breaker is not what is under test
        p.q.levels = 1;
        p.q.deep_levels = 0;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.recent_move = dec!(50); // 5000%, nonsense

        let d = plan(&i);
        let fv = d.fair_value.expect("a fair value");
        for q in &d.quotes {
            let away = (q.price - fv).abs() / fv;
            assert!(
                away <= dec!(0.06),
                "quote {} is {:.1}% from fair value, past the ceiling", q.price, away * dec!(100)
            );
        }
    }

    /// Setting the share to zero restores the old fixed-floor behaviour.
    #[test]
    fn a_zero_vol_share_keeps_the_fixed_spread() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.min_half_spread_ticks = dec!(2);
        p.q.vol_spread_share = Decimal::ZERO;
        p.q.max_improve_ticks = 50;
        p.risk.max_move = dec!(1000); // the breaker is not what is under test
        p.q.levels = 1;
        p.q.deep_levels = 0;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let mut calm = input(&m, &b, &p, &prog, dec!(10000), 0);
        calm.recent_move = Decimal::ZERO;
        let mut wild = input(&m, &b, &p, &prog, dec!(10000), 0);
        wild.recent_move = dec!(0.30);

        let width = |d: &Decision| {
            let bid = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.price).max();
            let ask = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.price).min();
            ask.unwrap() - bid.unwrap()
        };
        assert_eq!(width(&plan(&calm)), width(&plan(&wild)), "share 0 must ignore volatility");
    }

    /// The deep ladder is geometric in PRICE and doubles in size.
    ///
    /// Ticks cannot express "20% higher": a tick here is 0.71% of price, so a
    /// tick-spaced ladder bunches near the touch and its spacing drifts as
    /// price moves. A sweep catcher wants its rungs at fixed multiples.
    #[test]
    fn deep_rungs_step_by_price_and_double_in_size() {
        let (m, b) = (market(), book()); // 0.98 / 1.00
        let mut p = params();
        p.q.levels = 1;
        p.q.size_base = dec!(10);
        p.q.deep_levels = 4;
        p.q.deep_start_ticks = dec!(2);
        p.q.deep_price_step = dec!(1.2);
        p.q.deep_size = dec!(100);
        p.q.deep_size_growth = dec!(2);
        p.q.deep_allocation = dec!(0.8);
        p.q.size_jitter = dec!(0);
        p.q.max_improve_ticks = 0;
        p.risk.max_order_notional = dec!(100_000_000);
        p.risk.max_position = dec!(100_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(500_000), 0);
        i.quote_available = dec!(10_000_000);
        i.quote_total = dec!(10_000_000);
        i.base_available = dec!(10_000_000);

        let d = plan(&i);
        let mut asks: Vec<_> = d.quotes.iter().filter(|q| q.level >= 100).collect();
        asks.sort_by_key(|q| q.level);
        let asks: Vec<_> = asks.into_iter().filter(|q| q.side == Side::Sell).collect();
        assert_eq!(asks.len(), 4, "four deep asks");

        // Rung 0 is a couple of ticks out; each one after is ~1.2x further
        // from fair value than the last.
        let fv = d.fair_value.expect("a fair value");
        let out: Vec<Decimal> = asks.iter().map(|q| q.price - fv).collect();
        for w in out.windows(2) {
            assert!(w[1] > w[0] * dec!(1.5), "each rung must step out sharply: {out:?}");
        }
        // And each is about twice the size of the one before it.
        for w in asks.windows(2) {
            let ratio = w[1].amount / w[0].amount;
            assert!(
                ratio > dec!(1.7) && ratio < dec!(2.3),
                "sizes must roughly double, got {ratio} in {:?}",
                asks.iter().map(|q| q.amount).collect::<Vec<_>>()
            );
        }
    }

    /// Within a refresh window the ladder must be byte-identical, or the
    /// reconciler cancels and replaces it for no reason.
    #[test]
    fn the_ladder_does_not_change_inside_a_refresh_window() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.size_jitter = dec!(0.08);
        p.q.refresh_secs = 900;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let at = |ms: i64| {
            let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
            i.now_ms = ms;
            plan(&i).quotes
        };
        // Aligned to a window start, so "ten minutes later" is inside the
        // same window rather than a coin flip about where the boundary fell.
        let base = (1_790_000_000_000i64 / 900_000) * 900_000;
        assert_eq!(at(base), at(base + 6_000), "six seconds on: identical");
        assert_eq!(at(base), at(base + 600_000), "ten minutes on: still identical");
        assert_eq!(at(base), at(base + 899_000), "and right up to the boundary");

        // Across the boundary it re-rolls, which is the drip.
        let before = at(base);
        let after = at(base + 900_000);
        assert!(
            before.iter().zip(after.iter()).any(|(x, y)| x.amount != y.amount),
            "a new window must re-roll the sizes"
        );
    }

    /// Zero jitter must stay deterministic regardless of the window.
    #[test]
    fn no_jitter_means_no_drift() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.size_jitter = dec!(0);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let at = |ms: i64| {
            let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
            i.now_ms = ms;
            plan(&i).quotes
        };
        let base = 1_790_000_000_000i64;
        assert_eq!(at(base), at(base + 900_000 * 5), "no jitter, no change");
    }

    /// A price floor must be a WALL at the floor, not a hole below it.
    ///
    /// Live: book 1.35/1.36, floor 1.40. Every near ask was rejected for being
    /// under the floor and the cheapest surviving order was 1.64 -- so a 1.40
    /// floor silently became a 1.64 floor, and the 24 cents in between held
    /// nothing at all.
    #[test]
    fn a_price_floor_stacks_orders_at_the_floor_not_above_the_gap() {
        let (m, b) = (market(), book()); // 0.98 / 1.00
        let mut p = params();
        p.q.levels = 2;
        p.q.deep_levels = 3;
        p.q.deep_start_ticks = dec!(2);
        p.q.deep_price_step = dec!(1.1);
        p.q.size_jitter = dec!(0);
        p.risk.max_order_notional = dec!(1_000_000);
        p.risk.max_position = dec!(1_000_000);

        // Selling, floor at 1.05 -- above the whole book.
        let mut prog =
            program(crate::db::models::ProgramKind::Distribute, dec!(0), dec!(5000), 86_400);
        prog.start_qty = dec!(10000);
        prog.limit_price = Some(dec!(1.05));
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.base_available = dec!(10000);
        i.quote_available = dec!(10000);
        i.quote_total = dec!(10000);

        let d = plan(&i);
        let asks: Vec<Decimal> =
            d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.price).collect();
        assert!(!asks.is_empty(), "a floor must not silence the ask side entirely");
        for px in &asks {
            assert!(*px >= dec!(1.05), "ask {px} is under the floor");
        }
        // The point: something sits AT the floor, not only far above it.
        assert!(
            asks.iter().any(|p| *p == dec!(1.05)),
            "nothing rests at the floor itself, only {asks:?}"
        );
    }

    /// The same for a buy limit: never pay above it, but do still bid at it.
    #[test]
    fn a_buy_limit_bids_at_the_limit() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 2;
        p.q.deep_levels = 0;
        p.q.size_jitter = dec!(0);
        p.risk.max_order_notional = dec!(1_000_000);
        p.risk.max_position = dec!(1_000_000);

        // Buying, willing to pay up to 0.90 -- below the whole book.
        let mut prog =
            program(crate::db::models::ProgramKind::Accumulate, dec!(0), dec!(20000), 86_400);
        prog.start_qty = dec!(10000);
        prog.limit_price = Some(dec!(0.90));
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.quote_available = dec!(100_000);
        i.quote_total = dec!(100_000);
        i.base_available = dec!(10000);

        let d = plan(&i);
        let bids: Vec<Decimal> =
            d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.price).collect();
        assert!(!bids.is_empty(), "a limit must not silence the bid side");
        for px in &bids {
            assert!(*px <= dec!(0.90), "bid {px} is over the limit");
        }
        assert!(bids.iter().any(|p| *p == dec!(0.90)), "nothing rests at the limit: {bids:?}");
    }

    /// The far rungs must hold MORE coins than the near ones, not fewer.
    ///
    /// The single-order notional cap applied to every rung equally, so the
    /// ladder shrank in coins as it climbed in price: live, the rung at 2.82
    /// held about a sixth fewer coins than the one at 1.64, every one of them
    /// the same notional. Exactly backwards for
    /// a ladder whose purpose is to have size waiting where a sweep reaches.
    #[test]
    fn the_far_rungs_carry_more_coins_than_the_near_ones() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 1;
        p.q.deep_levels = 4;
        p.q.deep_start_ticks = dec!(2);
        p.q.deep_price_step = dec!(1.1);
        p.q.deep_size = dec!(200);
        p.q.deep_size_growth = dec!(2.5);
        p.q.deep_allocation = dec!(0.9);
        p.q.deep_cap_multiple = dec!(20);
        p.q.size_jitter = dec!(0);
        // The cap that used to flatten the ladder, left deliberately tight.
        p.risk.max_order_notional = dec!(500);
        p.risk.max_position = dec!(100_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(500_000), 0);
        i.base_available = dec!(500_000);
        i.quote_available = dec!(500_000);
        i.quote_total = dec!(500_000);

        let d = plan(&i);
        let mut asks: Vec<_> =
            d.quotes.iter().filter(|q| q.level >= 100 && q.side == Side::Sell).collect();
        asks.sort_by_key(|q| q.level);
        assert!(asks.len() >= 3, "need several deep asks, got {}", asks.len());

        for w in asks.windows(2) {
            assert!(
                w[1].amount > w[0].amount,
                "rung {} holds {} but rung {} only {} -- the ladder shrinks outward",
                w[0].level, w[0].amount, w[1].level, w[1].amount
            );
        }
        // And the outermost is dramatically bigger, not marginally.
        let (first, last) = (asks[0].amount, asks[asks.len() - 1].amount);
        assert!(last > first * dec!(4), "outermost {last} vs nearest {first}");
    }

    /// The tight ladder keeps the single-order cap exactly as written: those
    /// orders sit at the touch, where size does move the price.
    #[test]
    fn the_cap_still_binds_at_the_touch() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 1;
        p.q.deep_levels = 0;
        p.q.size_base = dec!(100_000);
        p.q.size_pct_of_equity = dec!(0);
        p.q.size_jitter = dec!(0);
        p.q.deep_cap_multiple = dec!(20);
        p.q.max_order_pct_of_depth = dec!(100);
        p.risk.max_order_notional = dec!(500);
        p.risk.max_order_pct_of_equity = dec!(0);
        p.risk.max_position = dec!(100_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(500_000), 0);
        i.base_available = dec!(500_000);
        i.quote_available = dec!(500_000);
        i.quote_total = dec!(500_000);

        for q in plan(&i).quotes.iter().filter(|q| q.level < 100) {
            assert!(
                q.amount * q.price <= dec!(501),
                "touch order {} x {} is past the cap", q.amount, q.price
            );
        }
    }

    #[test]
    fn never_crosses_the_spread() {
        let (m, b) = (market(), book());
        let mut p = params();
        // Even with a large improve allowance, crossing must remain impossible.
        p.q.max_improve_ticks = 10;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let d = plan(&input(&m, &b, &p, &prog, dec!(0), 0));
        for qt in &d.quotes {
            match qt.side {
                Side::Buy => assert!(qt.price < dec!(1.00), "bid {} crossed", qt.price),
                Side::Sell => assert!(qt.price > dec!(0.98), "ask {} crossed", qt.price),
            }
        }
    }

    fn pf(price: Decimal, base: Decimal, quote: Decimal) -> crate::db::models::Portfolio {
        crate::db::models::Portfolio { price, base_qty: base, quote_qty: quote }
    }

    #[test]
    fn twap_target_ramps_linearly() {
        let prog = program(crate::db::models::ProgramKind::Accumulate, dec!(0), dec!(10000), 1000);
        let p = pf(dec!(1), dec!(0), dec!(10000));
        assert_eq!(prog.target_at(0, p), dec!(0));
        assert_eq!(prog.target_at(500, p), dec!(5000));
        assert_eq!(prog.target_at(1000, p), dec!(10000));
        assert_eq!(prog.target_at(9999, p), dec!(10000), "target holds after the deadline");
    }

    /// Regression: locking cash in our own bid must not move the target.
    #[test]
    fn a_resting_bid_does_not_move_a_value_weighted_target() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.risk.max_position = dec!(100000);
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));

        // Same account twice: once with the cash free, once with most of it
        // locked in a resting bid. The valuation, and so the target, must match.
        let mut free = input(&m, &b, &p, &prog, dec!(10000), 0);
        free.quote_available = dec!(10000);
        free.quote_total = dec!(10000);

        let mut locked = input(&m, &b, &p, &prog, dec!(10000), 0);
        locked.quote_available = dec!(1000);
        locked.quote_total = dec!(10000);

        assert_eq!(
            plan(&free).target_inventory,
            plan(&locked).target_inventory,
            "a resting bid changed the target and would oscillate"
        );
    }

    #[test]
    fn balanced_targets_half_the_portfolio_value() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));
        // 10,000 PRL at 1.00 plus 10,000 USDT = 20,000 of value; half of that
        // in PRL at 1.00 is 10,000 PRL, which is what we already hold.
        assert_eq!(prog.target_at(0, pf(dec!(1), dec!(10000), dec!(10000))), dec!(10000));
    }

    /// The destination must be a share of whatever is held, at any size.
    #[test]
    fn balanced_targets_half_of_any_portfolio_size() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));
        // 20k, 40k, 400k of PRL and no quote: the target is always half of it.
        for held in [dec!(20000), dec!(40000), dec!(400000)] {
            assert_eq!(
                prog.target_at(0, pf(dec!(1), held, dec!(0))),
                held / dec!(2),
                "a {held} PRL portfolio should target half of itself"
            );
        }
        // And it accounts for quote already held: 10k PRL + 10k USDT at 1.00 is
        // already balanced, so the target is what we hold.
        assert_eq!(prog.target_at(0, pf(dec!(1), dec!(10000), dec!(10000))), dec!(10000));
    }

    /// Re-anchoring after a deposit must restore a gradual glide rather than
    /// leaving the whole new gap open from the first loop.
    #[test]
    fn a_reanchored_glide_starts_from_the_new_inventory() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(20707), dec!(0), 86_400_000);
        prog.ratio = Some(dec!(0.5));
        let after_deposit = pf(dec!(1), dec!(40707), dec!(0));

        // Stale anchor: the glide still starts at the pre-deposit inventory, so
        // the target is already ~20k below what we hold. That is the bug.
        let stale = prog.target_at(1000, after_deposit);
        assert!(
            (dec!(40707) - stale) > dec!(19000),
            "stale anchor should leave a huge immediate gap, got {stale}"
        );

        // Re-anchored: start_qty becomes current inventory and the clock
        // restarts, so the gap opens gradually from zero.
        prog.start_qty = dec!(40707);
        prog.started_at = 1000;
        prog.ends_at = Some(1000 + 86_400_000);
        let fresh = prog.target_at(1000, after_deposit);
        assert_eq!(fresh, dec!(40707), "right after re-anchoring, target == held");

        let halfway = prog.target_at(1000 + 43_200_000, after_deposit);
        assert_eq!(halfway, dec!(30530.25), "halfway through, half the gap closed");

        let done = prog.target_at(1000 + 86_400_000, after_deposit);
        assert_eq!(done, dec!(20353.5), "at the end, 50/50 of the new balance");
    }

    /// Banking profit into the base asset must raise the base target above the
    /// plain ratio, and only by the banked amount.
    #[test]
    fn banked_profit_lifts_the_base_target_above_the_ratio() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));
        // 10k base at 1.00 plus 10k quote: 20k of value, plain 50/50 wants 10k.
        let pf20 = pf(dec!(1), dec!(10000), dec!(10000));
        assert_eq!(prog.target_at(0, pf20), dec!(10000));

        // Bank 1,000 of profit. Working capital is 19k split evenly (9.5k of
        // base) plus the 1,000 held entirely as base: 10,500.
        prog.banked = dec!(1000);
        assert_eq!(prog.target_at(0, pf20), dec!(10500));

        // Which is 52.5% of value in base rather than 50%.
        let held = prog.target_at(0, pf20);
        assert_eq!(held * dec!(1) / pf20.value() * dec!(100), dec!(52.5));
    }

    /// Banking must never demand more base than the account is worth.
    #[test]
    fn banking_cannot_exceed_the_portfolio() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));
        prog.banked = dec!(999999); // absurd
        let p = pf(dec!(1), dec!(1000), dec!(1000));
        let t = prog.target_at(0, p);
        assert_eq!(t, dec!(2000), "capped at the whole portfolio, not beyond it");
        assert!(t * p.price <= p.value(), "target worth more than we hold");
    }

    /// With nothing banked the behaviour is exactly the old ratio.
    #[test]
    fn zero_banked_is_the_plain_ratio() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));
        prog.banked = Decimal::ZERO;
        assert_eq!(prog.target_at(0, pf(dec!(2), dec!(10000), dec!(10000))), dec!(7500));
    }

    #[test]
    fn balanced_sells_into_strength_and_buys_weakness() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(0), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));
        // Start balanced at 1.00: 10k PRL + 10k USDT.
        let flat = prog.target_at(0, pf(dec!(1), dec!(10000), dec!(10000)));
        // Price doubles. Holdings are now worth 20k PRL-side + 10k quote = 30k;
        // half is 15k of value, which at 2.00 is only 7,500 PRL. Target falls,
        // so the bot sells into the rally.
        let up = prog.target_at(0, pf(dec!(2), dec!(10000), dec!(10000)));
        assert!(up < flat, "target must fall as price rises (sell strength)");
        assert_eq!(up, dec!(7500));
        // Price halves: 5k + 10k = 15k, half is 7.5k of value, at 0.50 that is
        // 15,000 PRL. Target rises, so the bot buys the dip.
        let down = prog.target_at(0, pf(dec!(0.5), dec!(10000), dec!(10000)));
        assert!(down > flat, "target must rise as price falls (buy weakness)");
        assert_eq!(down, dec!(15000));
    }

    #[test]
    fn balanced_ramps_gradually_toward_its_first_target() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(20000), dec!(0), 1000);
        prog.ratio = Some(dec!(0.5));
        // All-in on PRL: 20,000 at 1.00 and no quote. Balanced wants 10,000.
        let p = pf(dec!(1), dec!(20000), dec!(0));
        assert_eq!(prog.target_at(0, p), dec!(20000), "starts where we are");
        assert_eq!(prog.target_at(500, p), dec!(15000), "half way, half sold");
        assert_eq!(prog.target_at(1000, p), dec!(10000), "arrives at the balance");
        assert_eq!(prog.target_at(50_000, p), dec!(10000), "and keeps tracking it");
    }

    #[test]
    fn balanced_survives_a_zero_price() {
        let mut prog = program(crate::db::models::ProgramKind::Balanced, dec!(123), dec!(0), 0);
        prog.ratio = Some(dec!(0.5));
        assert_eq!(prog.target_at(0, pf(dec!(0), dec!(10), dec!(10))), dec!(123));
    }

    #[test]
    fn falling_behind_schedule_grows_the_bid_and_shrinks_the_ask() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Accumulate, dec!(0), dec!(40000), 1000);
        // Halfway through the schedule the target is 20000 but we hold nothing.
        let d = plan(&input(&m, &b, &p, &prog, dec!(0), 500));
        let bid: Decimal = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount).sum();
        let ask: Decimal = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        assert!(bid > ask, "behind schedule should quote more bid ({bid}) than ask ({ask})");
    }

    /// Regression: the skews must still bite when the hard position bound is
    /// far away.
    ///
    /// Normalising inventory error against `max_position` made the error
    /// vanishingly small under realistic settings, so the bot quoted
    /// symmetrically while believing it was leaning. Both the TWAP catch-up and
    /// the rebalance ride on that lean, so this is the mechanism, not a detail.
    #[test]
    fn skew_works_with_a_distant_position_bound() {
        let (m, b) = (market(), book());
        let mut p = params();
        // The shipped default: a bound far above anything we plan to hold.
        p.risk.max_position = dec!(100000);
        p.q.size_base = dec!(200);
        p.q.levels = 2;

        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(15000), 0);
        // 5,000 PRL over a 15,000 target: several ladders out, so the bot should
        // be clearly leaning to sell.
        let d = plan(&input(&m, &b, &p, &prog, dec!(20000), 0));
        let bid: Decimal = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount).sum();
        let ask: Decimal = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        assert!(
            ask > bid * dec!(1.5),
            "being 5,000 over target must lean hard to sell, got bid={bid} ask={ask}"
        );
    }

    /// The size skew must bite on the side whose size the BUDGET decides.
    ///
    /// It used to be divided out of the budget sizing and multiplied back in
    /// afterwards, cancelling exactly. Cash is always the scarce asset on this
    /// account, so the bid side was always budget-bound and the skew never
    /// touched it -- and on the ask side, the same cancellation left asks 10%
    /// larger than bids while the account was 30% under target in a rally.
    /// Measured cost: heavy selling on the wrong side, negative at five minutes.
    #[test]
    fn the_size_skew_survives_budget_bound_sizing() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 2;
        p.q.size_jitter = dec!(0);
        p.q.deep_levels = 0;
        p.q.size_base = dec!(1_000_000); // the ceiling never binds
        p.q.ladder_fraction = dec!(0.55);
        p.q.inventory_size_skew = dec!(0.8);
        p.q.inventory_scale = dec!(1000);
        p.risk.max_order_notional = dec!(1_000_000);
        p.risk.max_position = dec!(1_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        // 3,000 under target: full buy pressure. Both sides budget-bound.
        let mut i = input(&m, &b, &p, &prog, dec!(7000), 0);
        i.quote_available = dec!(2000);
        i.quote_total = dec!(2000);
        i.base_available = dec!(2000);
        let d = plan(&i);
        let bid: Decimal = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount * q.price).sum();
        let ask: Decimal = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount * q.price).sum();
        assert!(bid > Decimal::ZERO && ask > Decimal::ZERO, "both sides quote: bid={bid} ask={ask}");
        assert!(
            ask <= bid * dec!(0.25),
            "at full buy pressure the asks must be a fraction of the bids, got bid={bid} ask={ask}"
        );
        // And the right-way side is not starved by the lean: it still deploys
        // at least the neutral ladder's worth of the budget.
        assert!(bid >= dec!(2000) * dec!(0.55) * dec!(0.99), "bids {bid} must deploy the budget");
    }

    /// An arm's multiplier applies to the final size of the rungs that land
    /// on it, including the ones the budget decides, and ONLY to those: the
    /// touch can sit at 15% while the far rungs quote full size, which
    /// is exactly the shape the fills asked for (prlusdt's touch lost 10.6 ‰
    /// per dollar-hour while its 200-400 bp rungs were break-even).
    #[test]
    fn arm_scales_shrink_only_the_rungs_on_probed_arms() {
        use crate::strategy::arms::Bucket;
        let (m, b) = (market(), book()); // 0.98 / 1.00, tick 0.01, fv ~0.99
        let mut p = params();
        p.q.levels = 2;
        p.q.deep_levels = 2;
        p.q.deep_start_ticks = dec!(3);
        p.q.deep_step_ticks = dec!(3);
        p.q.deep_size = dec!(300);
        p.q.size_jitter = dec!(0);
        p.q.ladder_fraction = dec!(0.55);
        p.risk.max_order_notional = dec!(1_000_000);
        p.risk.max_position = dec!(1_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let quotes_with = |arms: &ArmScales, cash: Decimal| {
            let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
            i.quote_available = cash;
            i.quote_total = cash;
            i.arms = arms;
            let mut q = plan(&i).quotes;
            q.sort_by(|a, c| (a.side.as_str(), a.level).cmp(&(c.side.as_str(), c.level)));
            q
        };
        // Bids: the touch (<50 bp and 50-100 bp) at 15%, the deep rungs full.
        let probed = ArmScales::uniform(dec!(0.15))
            .with(Side::Buy, Bucket::From200, Decimal::ONE)
            .with(Side::Buy, Bucket::From400, Decimal::ONE)
            .with(Side::Sell, Bucket::Under50, Decimal::ONE)
            .with(Side::Sell, Bucket::From50, Decimal::ONE)
            .with(Side::Sell, Bucket::From100, Decimal::ONE)
            .with(Side::Sell, Bucket::From200, Decimal::ONE)
            .with(Side::Sell, Bucket::From400, Decimal::ONE);

        for cash in [dec!(500), dec!(100_000)] {
            let full = quotes_with(&ArmScales::full(), cash);
            let part = quotes_with(&probed, cash);
            let probe_arm = |q: &DesiredQuote| q.side == Side::Buy && q.bps < dec!(200);
            let mut probed_seen = 0;
            let mut full_seen = 0;
            for pr in &part {
                let Some(f) = full.iter().find(|f| (f.side, f.level) == (pr.side, pr.level)) else {
                    // A rung that only exists at the probe: the money the
                    // probed touch no longer takes has funded a far rung. That
                    // is the intended priority, and it can only ever add
                    // rungs on arms that are NOT probed.
                    assert!(!probe_arm(pr), "a probed arm gained a rung: {pr:?}");
                    continue;
                };
                let ratio = pr.amount / f.amount;
                if probe_arm(f) {
                    probed_seen += 1;
                    assert!(
                        (ratio - dec!(0.15)).abs() < dec!(0.02),
                        "{:?} level {} at {} bp should be 15% of full, got {ratio}", f.side, f.level, f.bps
                    );
                } else {
                    full_seen += 1;
                    assert!(
                        ratio >= Decimal::ONE,
                        "{:?} level {} at {} bp must not shrink, got {ratio}", f.side, f.level, f.bps
                    );
                }
            }
            assert!(probed_seen > 0 && full_seen > 0, "both kinds of rung must exist at cash {cash}");
        }
    }

    /// The lean reaches full strength at a share of the TARGET, not at two
    /// ladders' worth of a ceiling that scales with equity. Ten percent over
    /// target must already be a full lean to sell.
    #[test]
    fn the_inventory_scale_follows_the_target() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 2;
        p.q.size_jitter = dec!(0);
        p.q.inventory_scale = dec!(0);
        p.q.inventory_scale_pct = dec!(0.10);
        p.q.inventory_size_skew = dec!(0.8);
        p.risk.max_position = dec!(1_000_000);
        p.risk.max_order_notional = dec!(1_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let d = plan(&input(&m, &b, &p, &prog, dec!(11000), 0)); // 10% over
        let bid: Decimal = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount).sum();
        let ask: Decimal = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        assert!(
            bid <= ask * dec!(0.15),
            "10% over target is a full lean: bids should be ~11% of asks, got bid={bid} ask={ask}"
        );
    }

    #[test]
    fn inventory_scale_can_be_set_explicitly() {
        let (m, b) = (market(), book());
        let mut p = params();
        // Room to the bound must be larger than the scale, or the bound would
        // be what mutes the lean and this would not test the setting at all.
        p.risk.max_position = dec!(10000000);
        p.q.inventory_scale = dec!(1000000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(15000), 0);
        let d = plan(&input(&m, &b, &p, &prog, dec!(20000), 0));
        let bid: Decimal = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount).sum();
        let ask: Decimal = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        assert!(
            (ask - bid).abs() < bid * dec!(0.35),
            "a huge scale should mute the lean, got bid={bid} ask={ask}"
        );
    }

    /// With fair value from a deeper book, our quotes sit around THAT, not
    /// wherever the stale local book happens to be.
    ///
    /// On PRL/USDC the nearest stranger's ask was 11% above fair value and the
    /// improve cap pinned our ask 7% above it, so we earned 8% of the spread
    /// instead of quoting where the asset is worth. Not crossing is still
    /// enforced; that is the guarantee that matters.
    #[test]
    fn a_trusted_reference_frees_us_from_a_stale_touch() {
        let m = market();
        // The real PRL/USDC book with our own orders stripped out.
        let stale = OrderBook {
            bids: vec![(dec!(1.24), dec!(52))],
            asks: vec![(dec!(1.44), dec!(14))],
        };
        let mut p = params();
        p.q.levels = 1;
        p.q.deep_levels = 0;
        p.q.max_improve_ticks = 5; // would pin the ask at 1.39
        p.q.min_half_spread_ticks = dec!(1);
        p.q.size_jitter = dec!(0);
        p.risk.max_order_notional = dec!(100000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let mut i = input(&m, &stale, &p, &prog, dec!(10000), 0);
        i.reference_price = Some(dec!(1.295)); // from PRL/USDT
        i.reference_is_external = true;
        let d = plan(&i);

        let ask = d.quotes.iter().find(|q| q.side == Side::Sell).expect("an ask");
        assert!(
            ask.price < dec!(1.35),
            "ask {} is still anchored to the stale 1.44, not to fair value",
            ask.price
        );
        assert!(ask.price > dec!(1.29), "but still above fair value");

        // Crossing remains impossible whatever the reference says.
        for q in &d.quotes {
            match q.side {
                Side::Buy => assert!(q.price < dec!(1.44), "bid {} crossed", q.price),
                Side::Sell => assert!(q.price > dec!(1.24), "ask {} crossed", q.price),
            }
        }
    }

    #[test]
    fn a_stale_book_is_quoted_around_the_reference_price() {
        let m = market();
        // The real PRL/USDC book: a token bid, a far ask, microprice ~1.004.
        let stale = OrderBook {
            bids: vec![(dec!(1.00), dec!(95))],
            asks: vec![(dec!(1.07), dec!(1598))],
        };
        let mut p = params();
        p.q.levels = 1;
        p.q.max_improve_ticks = 5; // allowed to quote inside a 7-tick spread
        p.q.min_half_spread_ticks = dec!(1);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let mut i = input(&m, &stale, &p, &prog, dec!(10000), 0);
        i.reference_price = Some(dec!(1.037)); // PRL/USDT mid, in USDC terms
        i.reference_is_external = true;
        let d = plan(&i);

        let ask = d.quotes.iter().find(|q| q.side == Side::Sell).expect("an ask");
        assert!(
            ask.price >= dec!(1.04),
            "ask {} must sit above the reference, not near the stale mid",
            ask.price
        );
        assert_eq!(d.fair_value, Some(dec!(1.037)), "reference must not be clamped to the stale touch");
        let bid = d.quotes.iter().find(|q| q.side == Side::Buy).expect("a bid");
        assert!(bid.price <= dec!(1.03), "bid {} must sit below the reference", bid.price);
        assert!(bid.price > dec!(1.00), "bid should improve on the token 1.00 bid");
    }

    /// A reference far from the local mid is bad data, not an opportunity.
    #[test]
    fn an_implausible_reference_is_ignored() {
        let (m, b, p) = (market(), book(), params()); // local 0.98 / 1.00
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        // Not external: nothing outside is claiming anything, so the local
        // book is all we have and we quote it.
        i.reference_price = Some(dec!(5));
        i.reference_is_external = false;
        let d = plan(&i);
        let fv = d.fair_value.expect("a fair value");
        assert!(fv < dec!(1.01), "implausible reference {} was used", fv);
    }

    /// When a DEEPER book disagrees wildly, stand down rather than quote the
    /// stale one.
    ///
    /// Falling back to the local book assumes the local book is right, and on
    /// a thin market that is exactly what is wrong: PRL/USDC held a stranger's
    /// quotes from before a 29% move while PRL/USDT said what PRL was worth.
    /// Quoting the stale book sells well under value to anyone watching.
    #[test]
    fn a_deeper_book_that_disagrees_stops_us_quoting_the_stale_one() {
        let (m, b, p) = (market(), book(), params()); // local 0.98 / 1.00
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        i.reference_price = Some(dec!(1.40)); // the deep book, 41% away
        i.reference_is_external = true;

        let d = plan(&i);
        assert!(
            matches!(d.halt, Some(Halt::ReferenceDisagrees { .. })),
            "must stand down, got {:?}", d.halt
        );
        assert!(d.quotes.is_empty(), "must not quote the stale book");
    }

    /// Inside the band it still quotes around the reference, as before.
    #[test]
    fn a_reference_inside_the_band_is_used_not_refused() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        i.reference_price = Some(dec!(1.05)); // ~6% away, well inside
        i.reference_is_external = true;
        let d = plan(&i);
        assert!(d.halt.is_none(), "a plausible reference must not halt");
        assert_eq!(d.fair_value, Some(dec!(1.05)));
    }

    /// A reference price must never let us cross the local book.
    #[test]
    fn a_wild_reference_price_cannot_cross_the_local_book() {
        let (m, b) = (market(), book()); // 0.98 / 1.00
        let mut p = params();
        p.q.max_improve_ticks = 50;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        for bogus in [dec!(50), dec!(0.01)] {
            let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
            i.reference_price = Some(bogus);
            for q in plan(&i).quotes {
                match q.side {
                    Side::Buy => assert!(q.price < dec!(1.00), "bid {} crossed on ref {bogus}", q.price),
                    Side::Sell => assert!(q.price > dec!(0.98), "ask {} crossed on ref {bogus}", q.price),
                }
            }
        }
    }

    #[test]
    fn being_over_target_flips_the_skew() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let d = plan(&input(&m, &b, &p, &prog, dec!(40000), 0));
        let bid: Decimal = d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.amount).sum();
        let ask: Decimal = d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        assert!(ask > bid, "over target should quote more ask ({ask}) than bid ({bid})");
    }

    #[test]
    fn hard_position_bound_stops_buying() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(60000), 0);
        let d = plan(&input(&m, &b, &p, &prog, dec!(50000), 0));
        assert!(
            d.quotes.iter().all(|q| q.side != Side::Buy),
            "must not bid at the max position"
        );
    }

    #[test]
    fn program_price_limit_suppresses_the_wrong_side() {
        let (m, b, p) = (market(), book(), params());
        let mut prog = program(crate::db::models::ProgramKind::Accumulate, dec!(0), dec!(40000), 1000);
        prog.limit_price = Some(dec!(0.90)); // never pay above 0.90
        let d = plan(&input(&m, &b, &p, &prog, dec!(0), 500));
        assert!(
            d.quotes.iter().all(|q| q.side != Side::Buy || q.price <= dec!(0.90)),
            "bid above the program limit price"
        );
    }

    /// Just over the cap we quote SMALLER, we do not stand down.
    ///
    /// The live halt cancelled every order on the main book and would have
    /// stayed out for up to four hours, because our own fills only leave the
    /// window as it rolls. It also fires hardest overnight, when the market's
    /// volume drops tenfold and our passive orders keep filling -- a quiet
    /// night reads as dominance. Throttling reduces our share by the same
    /// mechanism the cap asks for, while staying in the market.
    #[test]
    fn just_over_the_participation_cap_throttles_instead_of_halting() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.max_participation = dec!(0.15);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        let mut under = input(&m, &b, &p, &prog, dec!(5000), 0);
        under.our_participation = dec!(0.10);
        let base = plan(&under);
        assert!(base.halt.is_none());
        let base_size: Decimal = base.quotes.iter().map(|q| q.amount).sum();

        let mut over = input(&m, &b, &p, &prog, dec!(5000), 0);
        over.our_participation = dec!(0.30); // twice the cap
        let throttled = plan(&over);
        assert!(throttled.halt.is_none(), "twice the cap must not stop trading");
        assert!(!throttled.quotes.is_empty(), "must stay in the market");
        let small: Decimal = throttled.quotes.iter().map(|q| q.amount).sum();
        assert!(
            small < base_size,
            "over the cap must quote smaller: {small} vs {base_size}"
        );
    }

    /// Far past the cap is a different claim -- that we are essentially the
    /// whole tape -- and that is still grounds for standing down.
    #[test]
    fn far_past_the_participation_cap_still_halts() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.max_participation = dec!(0.15);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        i.our_participation = dec!(0.15) * PARTICIPATION_HALT_MULTIPLE + dec!(0.01);
        assert!(matches!(plan(&i).halt, Some(Halt::ParticipationExceeded { .. })));
    }

    #[test]
    fn participation_cap_halts_quoting() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(0), 0);
        i.our_participation = dec!(0.9);
        let d = plan(&i);
        assert!(matches!(d.halt, Some(Halt::ParticipationExceeded { .. })));
    }

    /// A cap of 1.0 opts a book out of the check entirely.
    #[test]
    fn a_full_participation_cap_never_halts() {
        let (m, b, mut p) = (market(), book(), params());
        p.q.max_participation = dec!(1);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        i.our_participation = dec!(1); // the entire tape
        let d = plan(&i);
        assert!(d.halt.is_none(), "a book we are the liquidity on must keep quoting");
        assert!(!d.quotes.is_empty());
    }

    /// And the cap still bites where it is meant to.
    #[test]
    fn a_set_participation_cap_still_halts() {
        let (m, b, mut p) = (market(), book(), params());
        p.q.max_participation = dec!(0.15);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        i.our_participation = dec!(0.9);
        assert!(matches!(plan(&i).halt, Some(Halt::ParticipationExceeded { .. })));
    }

    #[test]
    fn volatility_breaker_halts_quoting() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(0), 0);
        i.recent_move = dec!(0.5);
        assert!(matches!(plan(&i).halt, Some(Halt::Volatile { .. })));
    }

    #[test]
    fn quotes_respect_amount_and_price_precision() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        for qt in plan(&input(&m, &b, &p, &prog, dec!(5000), 0)).quotes {
            assert_eq!(qt.price, num::round_to(qt.price, 2), "price {} not on tick", qt.price);
            assert!(qt.amount >= m.min_amount);
            assert_eq!(qt.amount.scale() <= 4, true, "amount {} too precise", qt.amount);
        }
    }

    /// Regression: a ladder must fit inside the balance as a whole, not rung by
    /// rung. Every rung passing on its own while the ladder overdraws is how
    /// the bot came to fire rejected orders at the exchange on every loop.
    /// Deep rungs must sit far outside the tight ladder and must not eat the
    /// balance it needs.
    /// Deep rungs must not all be the same size.
    ///
    /// Every rung used to be exactly `deep_size`, so three rungs put three
    /// identical orders on the book -- 1236.03 three times over on the live
    /// account. It reads as a bug to anyone looking at the book, and it wastes
    /// the far rungs: they are the better price, so they should be larger.
    #[test]
    fn deep_rungs_grow_as_they_go_further_out() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 1;
        p.q.size_base = dec!(50);
        p.q.deep_levels = 3;
        p.q.deep_start_ticks = dec!(5);
        p.q.deep_step_ticks = dec!(5);
        p.q.deep_size = dec!(300);
        p.q.deep_size_growth = dec!(1.4);
        p.q.deep_allocation = dec!(0.8);
        p.q.max_improve_ticks = 0;
        p.risk.max_order_notional = dec!(1_000_000);
        p.risk.max_position = dec!(1_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.quote_available = dec!(1_000_000);
        i.quote_total = dec!(1_000_000);
        i.base_available = dec!(1_000_000);

        let d = plan(&i);
        for side in [Side::Buy, Side::Sell] {
            let mut rungs: Vec<_> =
                d.quotes.iter().filter(|q| q.level >= 100 && q.side == side).collect();
            rungs.sort_by_key(|q| q.level);
            assert_eq!(rungs.len(), 3, "three deep rungs on the {side} side");
            for w in rungs.windows(2) {
                assert!(
                    w[1].amount > w[0].amount,
                    "{side} rung {} ({}) must be larger than rung {} ({})",
                    w[1].level, w[1].amount, w[0].level, w[0].amount
                );
            }
        }
    }

    /// The same book must never carry two orders of identical size on one
    /// side. Identical sizes are the signature of a flat number applied to
    /// every rung instead of a ladder.
    #[test]
    fn no_side_posts_two_orders_of_the_same_size() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 3;
        p.q.size_base = dec!(200);
        p.q.level_size_growth = dec!(1.5);
        p.q.deep_levels = 3;
        p.q.deep_size = dec!(300);
        p.q.deep_size_growth = dec!(1.4);
        p.q.deep_allocation = dec!(0.5);
        p.q.max_improve_ticks = 0;
        p.risk.max_order_notional = dec!(1_000_000);
        p.risk.max_position = dec!(1_000_000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.quote_available = dec!(1_000_000);
        i.quote_total = dec!(1_000_000);
        i.base_available = dec!(1_000_000);

        let d = plan(&i);
        for side in [Side::Buy, Side::Sell] {
            let sizes: Vec<Decimal> =
                d.quotes.iter().filter(|q| q.side == side).map(|q| q.amount).collect();
            let mut seen = std::collections::HashSet::new();
            for s in &sizes {
                assert!(seen.insert(s.to_string()), "two {side} orders both sized {s}: {sizes:?}");
            }
        }
    }

    #[test]
    fn deep_rungs_sit_far_out_and_stay_inside_their_allocation() {
        let (m, b) = (market(), book()); // 0.98 / 1.00
        let mut p = params();
        p.q.levels = 2;
        p.q.size_base = dec!(200);
        p.q.deep_levels = 2;
        p.q.deep_start_ticks = dec!(5);
        p.q.deep_step_ticks = dec!(5);
        p.q.deep_size = dec!(300);
        p.q.deep_allocation = dec!(0.2);
        p.q.max_improve_ticks = 0;
        p.risk.max_order_notional = dec!(100000);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        i.quote_available = dec!(10000);
        i.quote_total = dec!(10000);
        i.base_available = dec!(10000);

        let d = plan(&i);
        let deep: Vec<_> = d.quotes.iter().filter(|q| q.level >= 100).collect();
        assert_eq!(deep.len(), 4, "two rungs a side");

        // Every deep bid is at least 5 ticks below the touch.
        for q in deep.iter().filter(|q| q.side == Side::Buy) {
            assert!(q.price <= dec!(0.94), "deep bid {} is not deep", q.price);
        }
        for q in deep.iter().filter(|q| q.side == Side::Sell) {
            assert!(q.price >= dec!(1.04), "deep ask {} is not deep", q.price);
        }
        // And they respect their slice: 20% of 10,000.
        let spend: Decimal = deep.iter().filter(|q| q.side == Side::Buy)
            .map(|q| q.amount * q.price).sum();
        assert!(spend <= dec!(10000), "deep bids cannot exceed the balance");

        // The reservation must hold even when the tight ladder is hungry: with
        // almost no cash, the deep rungs still get their slice.
        let mut poor = input(&m, &b, &p, &prog, dec!(10000), 0);
        poor.quote_available = dec!(1000);
        poor.quote_total = dec!(1000);
        poor.base_available = dec!(10000);
        let d2 = plan(&poor);
        assert!(
            d2.quotes.iter().any(|q| q.level >= 100 && q.side == Side::Buy),
            "deep bids must still be funded when the tight ladder wants the cash"
        );
    }

    #[test]
    fn deep_rungs_are_off_by_default() {
        let (m, b, p) = (market(), book(), params());
        assert_eq!(p.q.deep_levels, 0);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let d = plan(&input(&m, &b, &p, &prog, dec!(5000), 0));
        assert!(d.quotes.iter().all(|q| q.level < 100), "no deep rungs unless asked for");
    }

    #[test]
    fn the_whole_ladder_fits_inside_the_available_balance() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 3;
        p.q.size_base = dec!(250);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        // Enough for roughly one rung, not three.
        i.quote_available = dec!(250);
        i.quote_total = dec!(250);
        i.base_available = dec!(250);

        let d = plan(&i);
        let bid_cost: Decimal = d
            .quotes
            .iter()
            .filter(|q| q.side == Side::Buy)
            .map(|q| q.amount * q.price)
            .sum();
        let ask_size: Decimal =
            d.quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        assert!(bid_cost <= dec!(250), "bids cost {bid_cost}, over the 250 available");
        assert!(ask_size <= dec!(250), "asks size {ask_size}, over the 250 available");
    }

    #[test]
    fn insufficient_quote_balance_drops_bids() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(0), 0);
        i.quote_available = dec!(0.5); // under one min_amount at ~1.0
        i.quote_total = dec!(0.5);
        let d = plan(&i);
        assert!(d.quotes.iter().all(|q| q.side != Side::Buy));
    }

    /// A book priced off a deeper one is never quoted off its own residue.
    ///
    /// Live, 2026-09-24 11:39 UTC: the cross-market reference fell through
    /// for one loop and prlusdc quoted its asks around its own microprice of
    /// 1.31 while PRL/USDT said 1.17 -- twelve percent off. Sells that far
    /// above value are harmless; the same fallback on the bid side, or with a
    /// stale book BELOW value, sells the coin under its worth on every fill,
    /// which is the error that cost real money the first time. No reference
    /// means no quotes, and a cancel of whatever rests.
    #[test]
    fn a_referenced_book_without_a_reference_stands_down() {
        let m = market();
        let stale = OrderBook {
            bids: vec![(dec!(1.17), dec!(8500))],
            asks: vec![(dec!(1.30), dec!(153))],
        };
        let mut p = params();
        p.q.max_improve_ticks = 5;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(3488), 0);
        let mut i = input(&m, &stale, &p, &prog, dec!(3488), 0);
        i.reference_required = true;

        // No reference at all: the cross was missing this loop.
        i.reference_price = None;
        let d = plan(&i);
        assert_eq!(d.halt, Some(Halt::NoReference), "{:?}", d.halt);
        assert!(d.quotes.is_empty());

        // Its own microprice handed back as a "reference" is not an outside
        // opinion and must not count either.
        i.reference_price = stale.microprice();
        i.reference_is_external = false;
        assert_eq!(plan(&i).halt, Some(Halt::NoReference));

        // With the deeper book's price it quotes around THAT.
        i.reference_price = Some(dec!(1.1686));
        i.reference_is_external = true;
        let d = plan(&i);
        assert!(d.halt.is_none(), "{:?}", d.halt);
        assert_eq!(d.fair_value, Some(dec!(1.1686)));
        assert!(!d.quotes.is_empty());
    }

    /// The anchor book itself is not referenced, so its one-sided fallback
    /// and its plain local quoting are untouched by the flag.
    #[test]
    fn the_anchor_book_never_needs_a_reference() {
        let (m, b, p) = (market(), book(), params());
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        i.reference_required = false;
        i.reference_price = None;
        let d = plan(&i);
        assert!(d.halt.is_none());
        assert!(!d.quotes.is_empty());
    }

    /// Where the production deep ladder actually lands.
    ///
    /// The ladder was re-cut on 2026-09-24 to sample the 200-400 bp bucket
    /// that had never been measured: 3, 5, 7, 9, 11 ticks, quoted as 219, 365,
    /// 511, 657, 803 bp AT 1.37. A tick is a share of price, so the same
    /// ladder at 1.17 is 256, 427, 598, 769, 940 bp -- one rung in 200-400,
    /// not two -- and rounding to the tick pushes the bids one tick further
    /// still. This pins down both what the bucket coverage is at the two
    /// prices the account has seen this week, and that every one of the five
    /// rungs on each side survives the depth cap, the notional cap, the
    /// ladder fit and the 2 PRL minimum at the tuned account size with half its USDT
    /// allocated to this book (`allocate` splits it with usdcusdt) -- at full
    /// size, and, with nothing known about any arm, as ten minimum orders.
    #[test]
    fn the_production_deep_ladder_reaches_the_200_400_bucket_and_is_not_truncated() {
        use crate::config::Config;
        use crate::strategy::arms::Bucket;
        let cfg: Config =
            toml::from_str(include_str!("../../deploy/config.prod.toml")).expect("prod config");
        let p = Params { q: cfg.quoting.clone(), risk: cfg.risk.clone(), enabled: true, dry_run: false };
        let m = market();
        let arms = ArmScales::full();

        // The account as tuned, and the account after a deposit that leaves
        // three times the equity and 85% PRL. Every size is a share
        // of equity or of inventory, so the ask side must simply be the
        // first case, larger. The bid side is another matter: deep bids are
        // sized from a PRL inventory that has quadrupled and funded from
        // cash that has not, so the ladder truncates on that side -- which
        // is the shape the learned split exists to correct, by funding the
        // bids if their arms earn, and what this test pins is that it
        // truncates cleanly rather than placing rungs it cannot fund.
        for (inventory, cash, bid, ask) in [
            (dec!(3488), dec!(2261), dec!(1.16), dec!(1.17)),
            (dec!(3488), dec!(2261), dec!(1.36), dec!(1.37)),
            (dec!(14000), dec!(2900), dec!(1.17), dec!(1.18)),
        ] {
            let starved_bids = inventory * ask > cash * dec!(4);
            let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), inventory, 0);
            let b = OrderBook {
                bids: vec![(bid, dec!(11450)), (bid - dec!(0.01), dec!(20000))],
                asks: vec![(ask, dec!(34)), (ask + dec!(0.01), dec!(4000))],
            };
            let mut i = input(&m, &b, &p, &prog, inventory, 0);
            i.base_available = inventory / dec!(2); // prlusdc gets the other half
            i.quote_available = cash / dec!(2); // usdcusdt gets the other half
            i.quote_total = cash;
            i.arms = &arms;
            let d = plan(&i);
            assert!(d.halt.is_none(), "{:?}", d.halt);
            for side in [Side::Buy, Side::Sell] {
                let mut deep: Vec<_> = d
                    .quotes
                    .iter()
                    .filter(|q| q.level >= DEEP_LEVEL_BASE && q.side == side)
                    .collect();
                deep.sort_by_key(|q| q.level);
                if starved_bids && side == Side::Buy {
                    assert!(deep.len() >= 3, "{side} at {ask}: the cash funds at least three rungs, got {deep:?}");
                    assert!(deep.iter().all(|q| q.amount >= m.min_amount), "{side} at {ask}: {deep:?}");
                    continue;
                }
                assert_eq!(deep.len(), 5, "{side} at {ask}: five deep rungs, got {deep:?}");
                for q in &deep {
                    assert!(q.amount >= m.min_amount, "{side} rung {} at {ask} is under the minimum", q.level);
                }
                let in_200: usize =
                    deep.iter().filter(|q| Bucket::of_bps(f64::try_from(q.bps).unwrap()) == Bucket::From200).count();
                assert!(in_200 >= 1, "{side} at {ask}: nothing in 200-400 bp: {deep:?}");
                // Each rung further out carries more coins: the 1.3x tilt.
                for w in deep.windows(2) {
                    assert!(w[1].amount > w[0].amount, "{side} at {ask}: {deep:?}");
                }
            }

            // Nothing known: every one of those rungs still rests, at the
            // exchange minimum, so every arm keeps producing fills.
            let none = ArmScales::none();
            i.arms = &none;
            let d = plan(&i);
            for side in [Side::Buy, Side::Sell] {
                let deep: Vec<_> = d.quotes.iter().filter(|q| q.level >= DEEP_LEVEL_BASE && q.side == side).collect();
                if starved_bids && side == Side::Buy {
                    assert!(deep.len() >= 3, "{side} at {ask}: {deep:?}");
                } else {
                    assert_eq!(deep.len(), 5, "{side} at {ask}: five minimum rungs, got {deep:?}");
                }
                assert!(deep.iter().all(|q| q.amount == m.min_amount), "{side} at {ask}: {deep:?}");
            }
        }
    }

    /// `allocation::side_funding` must agree with this planner about what a
    /// side needs on hand: with exactly that much the ladder rests at its
    /// full configured size, and with a third of it the rungs are smaller.
    /// The number feeds the learned split, so a drift between the two would
    /// have the bot drifting capital toward a ladder that then could not use
    /// it, or starving one that could.
    #[test]
    fn the_funding_need_is_what_the_ladder_takes_to_rest_at_full_size() {
        use crate::config::Config;
        use crate::strategy::allocation::side_funding;
        let cfg: Config =
            toml::from_str(include_str!("../../deploy/config.prod.toml")).expect("prod config");
        let mut p = Params { q: cfg.quoting.clone(), risk: cfg.risk.clone(), enabled: true, dry_run: false };
        p.q.size_jitter = dec!(0);
        p.risk.max_order_pct_of_equity = dec!(1); // the cap is not what is under test
        let m = market();
        let b = OrderBook {
            bids: vec![(dec!(1.16), dec!(50000)), (dec!(1.15), dec!(80000))],
            asks: vec![(dec!(1.17), dec!(50000)), (dec!(1.18), dec!(80000))],
        };
        let arms = ArmScales::full();
        for (inventory, cash) in [(dec!(3661), dec!(2069)), (dec!(10983), dec!(6207))] {
            let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), inventory, 0);
            let mut i = input(&m, &b, &p, &prog, inventory, 0);
            i.quote_total = cash;
            i.arms = &arms;
            let fv = b.microprice().unwrap();
            let equity = inventory * fv + cash;
            let need = side_funding(&p.q, equity, inventory, fv);
            let rung = equity * p.q.size_pct_of_equity / fv;

            i.quote_available = need;
            i.base_available = need / fv;
            let d = plan(&i);
            for side in [Side::Buy, Side::Sell] {
                let r0 = d.quotes.iter().find(|q| q.side == side && q.level == 0).expect("rung 0");
                assert!((r0.amount - rung).abs() / rung < dec!(0.02), "{side}: funded, rung 0 is {} of a {rung} rung", r0.amount);
            }

            i.quote_available = need / dec!(3);
            i.base_available = need / fv / dec!(3);
            let d = plan(&i);
            for side in [Side::Buy, Side::Sell] {
                let r0 = d.quotes.iter().find(|q| q.side == side && q.level == 0).expect("rung 0");
                assert!(r0.amount < rung * dec!(0.6), "{side}: at a third of the funding rung 0 is {} of {rung}", r0.amount);
            }
        }
    }

    /// With `probe_dead_arms` off, a losing arm rests nothing at all.
    ///
    /// The owner asked to stop spending money on arms the evidence says lose,
    /// while still measuring them. The tape records every public trade with
    /// its aggressor side, so what an unquoted arm would have done is still
    /// recoverable offline -- the measurement moves from our own fills to the
    /// market's, and costs nothing.
    #[test]
    fn a_dead_arm_rests_nothing_when_probing_is_off() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.probe_dead_arms = false;
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let none = ArmScales::none();
        let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
        // Nothing the evidence will fund.
        i.arms = &none;

        let d = plan(&i);
        assert!(d.quotes.is_empty(), "a losing arm must rest nothing: {:?}", d.quotes);

        // And with probing on, the same arms rest the exchange minimum.
        let mut p2 = p.clone();
        p2.q.probe_dead_arms = true;
        let mut i2 = input(&m, &b, &p2, &prog, dec!(5000), 0);
        i2.arms = &none;
        assert!(!plan(&i2).quotes.is_empty(), "probing on must still explore");
    }

    /// The cycle can put a floor under the ask side that the arms have
    /// defunded, and it can only ever FLOOR it -- never cut a side the arms
    /// already fund.
    ///
    /// This is the release valve. On the measured table prlusdt's sell arm is
    /// -17 bp and the arms correctly size it to nothing; the cycle that leg
    /// completes pays, because the PRL being released was acquired 85 bp under
    /// fair value on the other book, so the leg is worth resting anyway. The
    /// bot must be able to express that without anyone turning off the arms.
    #[test]
    fn the_cycle_can_floor_an_ask_the_arms_defunded_and_never_cuts_one_they_fund() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.probe_dead_arms = false;
        p.q.size_jitter = dec!(0);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let none = ArmScales::none();

        // Arms defunded, no cycle: nothing rests, which is today's behaviour.
        let mut shut = input(&m, &b, &p, &prog, dec!(5000), 0);
        shut.arms = &none;
        assert!(
            !plan(&shut).quotes.iter().any(|q| q.side == Side::Sell),
            "with no cycle a defunded ask rests nothing"
        );

        // Same arms, cycle open: the ask comes back, and only the ask.
        let mut open = input(&m, &b, &p, &prog, dec!(5000), 0);
        open.arms = &none;
        open.cycle_release_floor = dec!(0.10);
        let d = plan(&open);
        let asks: Vec<_> = d.quotes.iter().filter(|q| q.side == Side::Sell).collect();
        assert!(!asks.is_empty(), "the cycle floors the ask: {:?}", d.quotes);
        assert!(
            !d.quotes.iter().any(|q| q.side == Side::Buy),
            "and says nothing about the bid, which the arms still govern"
        );

        // A tenth of a rung is a tenth of a rung: the floor sizes the overflow
        // in proportion to how much worse it pays, it does not open it fully.
        let mut full = input(&m, &b, &p, &prog, dec!(5000), 0);
        full.arms = &none;
        full.cycle_release_floor = Decimal::ONE;
        let big: Decimal = plan(&full).quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        let small: Decimal = asks.iter().map(|q| q.amount).sum();
        assert!(big > small * dec!(3), "a full floor rests far more: {big} vs {small}");

        // And it must never REDUCE a side the arms already fund.
        let mut funded = input(&m, &b, &p, &prog, dec!(5000), 0);
        let with_cycle_amt: Decimal = {
            let mut i = input(&m, &b, &p, &prog, dec!(5000), 0);
            i.cycle_release_floor = dec!(0.10);
            plan(&i).quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum()
        };
        funded.cycle_release_floor = Decimal::ZERO;
        let without: Decimal =
            plan(&funded).quotes.iter().filter(|q| q.side == Side::Sell).map(|q| q.amount).sum();
        assert!(
            with_cycle_amt >= without,
            "a floor must not cut a funded side: {with_cycle_amt} vs {without}"
        );
    }

    /// THE BINDING CONSTRAINT, measured live 2026-09-27: a
    /// directional target throttling the round trip on the one book that pays.
    ///
    /// The balanced program wanted 87% PRL against 79% held, which at
    /// `inventory_size_skew = 1.0` is `err = -0.915` and so `ask_mult = 0.085`.
    /// prlusdc's asks rested at under two-fifths the size of its bids, on a
    /// sell leg measured at +152 bp -- the best of the four. A market maker
    /// round-trips at the rate of its smaller side, so the earner was running at
    /// a twelfth of capacity to express a view that belongs on the deep book.
    ///
    /// `cycle_churn` drops the SIZE skew on the acquiring book and nothing else.
    #[test]
    fn churn_lets_the_acquiring_book_quote_both_sides_against_a_directional_lean() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.size_jitter = dec!(0);
        p.q.inventory_size_skew = dec!(1);
        p.q.inventory_scale_pct = dec!(0.10);
        // Wanted 10,000, holding 9,150: 850 short against a scale of 1,000, so
        // err = -0.85 and the ask side is cut to 15% of full size.
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let side = |d: &Decision, s: Side| -> Decimal {
            d.quotes.iter().filter(|q| q.side == s).map(|q| q.amount).sum()
        };

        let throttled = plan(&input(&m, &b, &p, &prog, dec!(9150), 0));
        let (t_bid, t_ask) = (side(&throttled, Side::Buy), side(&throttled, Side::Sell));
        assert!(t_ask > Decimal::ZERO, "the lean throttles the ask, it does not kill it");
        assert!(t_bid > t_ask * dec!(4), "and the two sides are lopsided: {t_bid} vs {t_ask}");

        let mut churn = input(&m, &b, &p, &prog, dec!(9150), 0);
        churn.cycle_churn = true;
        let d = plan(&churn);
        let (c_bid, c_ask) = (side(&d, Side::Buy), side(&d, Side::Sell));
        assert!(c_ask > t_ask * dec!(4), "the ask is un-throttled: {t_ask} -> {c_ask}");
        assert!(c_bid < t_bid, "and the bid gives back what the lean was adding");
        // Neutral means neutral: the round trip is symmetric, which is the whole
        // point -- it round-trips at the rate of its smaller side.
        assert!(
            (c_bid - c_ask).abs() < c_ask / dec!(10),
            "both sides at neutral size: {c_bid} vs {c_ask}"
        );

        // The PRICES still lean. Only the sizes go neutral, so every rung stays
        // on the arm it was measured as.
        let bid_px = |d: &Decision| d.quotes.iter().filter(|q| q.side == Side::Buy).map(|q| q.price).max();
        assert_eq!(bid_px(&throttled), bid_px(&d), "the price lean is untouched");
    }

    /// A book restricted to being a cycle leg rests no ladder of its own.
    ///
    /// prlusdt, measured over 3.7 days: buy -15 to -22 bp, sell -17. Its arms
    /// are long since defunded, but defunded still means a stream of minimum
    /// rungs on both sides. As a leg it quotes for exactly two reasons -- the
    /// cycle's release valve, and a quantity the owner asked for.
    #[test]
    fn a_cycle_leg_only_book_quotes_for_the_valve_or_a_program_and_nothing_else() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.size_jitter = dec!(0);
        p.q.cycle_leg_only = true;
        let standing = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);

        // Valve shut, no program to work: nothing rests at all. Not a minimum
        // rung, not one side -- nothing.
        let shut = plan(&input(&m, &b, &p, &standing, dec!(5000), 0));
        assert!(shut.quotes.is_empty(), "a leg with nothing to do rests nothing: {:?}", shut.quotes);

        // With probing on it still rests nothing: `probe_dead_arms` is about
        // keeping an arm's estimate alive, and this book is not being sized.
        let mut probing = p.clone();
        probing.q.probe_dead_arms = true;
        let probed = plan(&input(&m, &b, &probing, &standing, dec!(5000), 0));
        assert!(probed.quotes.is_empty(), "and probing is not a way back in: {:?}", probed.quotes);

        // Valve open: the ask comes back, sized by the valve, and the bid stays
        // away -- we never acquire here, that is the other book's job.
        let mut open = input(&m, &b, &p, &standing, dec!(5000), 0);
        open.cycle_release_floor = dec!(0.10);
        let d = plan(&open);
        assert!(d.quotes.iter().any(|q| q.side == Side::Sell), "the valve rests: {:?}", d.quotes);
        assert!(!d.quotes.iter().any(|q| q.side == Side::Buy), "and only the valve: {:?}", d.quotes);

        // An execution program suspends the rule: working a quantity the owner
        // asked for is not making a market, and this is the deep book where size
        // belongs. Both sides are available to the ladder again.
        let working = program(crate::db::models::ProgramKind::Accumulate, dec!(5000), dec!(9000), 0);
        let d = plan(&input(&m, &b, &p, &working, dec!(5000), 0));
        assert!(d.quotes.iter().any(|q| q.side == Side::Buy), "execution quotes: {:?}", d.quotes);

        // And the rule is opt-in: the same book without it is unchanged.
        let mut off = p.clone();
        off.q.cycle_leg_only = false;
        assert!(!plan(&input(&m, &b, &off, &standing, dec!(5000), 0)).quotes.is_empty());
    }

    /// TWAP mode: bids, on every book that can spend the money, and never an
    /// ask -- not even when we have run AHEAD of the ramp.
    ///
    /// This is the live failure of 2026-09-27 pinned down. Asked to buy the
    /// account into PRL, an `accumulate` bought a large clip on the thin book
    /// inside a minute, which put inventory above the ramp; the error flipped
    /// sign, the ask side became the right-way side, and the bot rested an ask
    /// for nearly all of it
    /// against the position it had just paid for. `min_position` does not fix
    /// it: it is a floor under inventory, so every unit bought reopens room to
    /// sell one -- the ask that appeared was exactly `inventory - min_position`.
    #[test]
    fn a_twap_never_quotes_an_ask_even_when_it_is_ahead_of_the_ramp() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.inventory_size_skew = dec!(1);
        p.q.levels = 3;
        // Room to hold everything the cash can buy, so what this test measures
        // is the program and not the position bound.
        p.risk.max_position = dec!(500000);
        // Six hours of window; the ramp is a fifth of the way through.
        let day = 6 * 3_600_000;
        let twap = program(crate::db::models::ProgramKind::Twap, dec!(5000), dec!(100000), day);

        // The harness holds $100,000 of quote, so at ~1 the destination is about
        // 101,000 above the anchor and a fifth of the window is ~28,000 in.
        //
        // Behind the ramp it bids. Ahead of it the lean suppresses the bid --
        // which is right, a TWAP does not buy faster than its schedule -- and
        // the thing that must NOT happen is the freed capacity reappearing as an
        // ask. Ahead of schedule a buy-only program rests nothing and waits.
        for (inventory, where_we_are, bids) in [
            (dec!(5000), "at the anchor", true),
            (dec!(20000), "behind the ramp", true),
            (dec!(45000), "ahead of the ramp", false),
        ] {
            let d = plan(&input(&m, &b, &p, &twap, inventory, day / 5));
            assert!(d.halt.is_none(), "{where_we_are}: {:?}", d.halt);
            assert_eq!(
                d.quotes.iter().any(|q| q.side == Side::Buy), bids,
                "{where_we_are}: bids expected {bids}: {:?}", d.quotes
            );
            assert!(
                !d.quotes.iter().any(|q| q.side == Side::Sell),
                "{where_we_are}: a twap must never rest an ask: {:?}", d.quotes
            );
        }

        // At the production size skew the bid is throttled, not switched off, so
        // being ahead of the ramp still leaves the book covered.
        let mut prod = p.clone();
        prod.q.inventory_size_skew = dec!(0.8);
        let d = plan(&input(&m, &b, &prod, &twap, dec!(45000), day / 5));
        assert!(d.quotes.iter().any(|q| q.side == Side::Buy), "throttled, not off: {:?}", d.quotes);
        assert!(!d.quotes.iter().any(|q| q.side == Side::Sell), "and still no ask: {:?}", d.quotes);

        // Nor by the routes that put an ask back on a leg-only book: the
        // cycle's release valve, and an arm the sizer likes.
        let mut leg = p.clone();
        leg.q.cycle_leg_only = true;
        let mut i = input(&m, &b, &leg, &twap, dec!(20000), day / 5);
        i.cycle_release_floor = dec!(1);
        let d = plan(&i);
        assert!(
            !d.quotes.iter().any(|q| q.side == Side::Sell),
            "not even the release valve reopens the ask side: {:?}", d.quotes
        );
        // And the deep book still works size for it: execution suspends
        // `cycle_leg_only`, which is the whole reason prlusdt is kept.
        assert!(
            d.quotes.iter().any(|q| q.side == Side::Buy),
            "a twap bids on the deep leg book: {:?}", d.quotes
        );

        // A deep ladder is a ladder: its asks are gone too.
        let mut deep = p.clone();
        deep.q.deep_levels = 3;
        deep.q.deep_size = dec!(500);
        deep.q.deep_allocation = dec!(0.5);
        let d = plan(&input(&m, &b, &deep, &twap, dec!(20000), day / 5));
        assert!(
            !d.quotes.iter().any(|q| q.side == Side::Sell),
            "the deep ladder has no ask side either: {:?}", d.quotes
        );
    }

    /// The target is worked out once and then holds still.
    ///
    /// It does NOT follow the balance around: a target that moved every loop
    /// made what the bot was about to do impossible to predict from what it had
    /// just said. New capital is handled by the engine as an event -- a fresh
    /// twap over its own window -- not by the target drifting underneath you.
    #[test]
    fn a_twap_target_is_fixed_once_it_starts() {
        use crate::db::models::Portfolio;
        let six_h = 6 * 3_600_000;
        // Started holding 5,000 with $75,000 of cash at 1.50: 55,000 all in.
        let twap = program(crate::db::models::ProgramKind::Twap, dec!(5000), dec!(55000), six_h);

        let pf = Portfolio { price: dec!(1.5), base_qty: dec!(5000), quote_qty: dec!(75000) };
        assert_eq!(twap.target_at(six_h, pf), dec!(55000), "the destination by the deadline");
        assert_eq!(twap.target_at(six_h / 2, pf), dec!(30000), "half the window, half the way");
        assert_eq!(twap.target_at(0, pf), dec!(5000), "and it starts where we started");

        // A deposit does not move it, and neither does the price. The engine
        // starts a new program for new money; the running one keeps its word.
        let richer = Portfolio { price: dec!(1.5), base_qty: dec!(5000), quote_qty: dec!(105000) };
        let dearer = Portfolio { price: dec!(3), base_qty: dec!(5000), quote_qty: dec!(75000) };
        for pf in [richer, dearer] {
            assert_eq!(twap.target_at(six_h / 2, pf), dec!(30000), "the schedule is unchanged");
            assert_eq!(twap.target_at(six_h, pf), dec!(55000));
        }
    }

    /// A twap has no deep ladder, and nothing is reserved for one.
    ///
    /// Live 2026-09-27: `deep_allocation = 0.45` fenced off 45% of a large
    /// TWAP for five rungs sized from `deep_pct_of_inventory` of an inventory
    /// still near zero -- a sliver of orders holding nearly half the money the
    /// working ladder needed, and churning on every cent of drift because they
    /// sit at fixed offsets from fair value.
    #[test]
    fn a_twap_has_no_deep_ladder_and_reserves_nothing_for_one() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 2;
        p.q.deep_levels = 5;
        p.q.deep_size = dec!(500);
        p.q.deep_allocation = dec!(0.45);
        p.q.ladder_fraction = dec!(0.9);
        let six_h = 6 * 3_600_000;

        // Same market, same money, two programs: one that market-makes and one
        // that is only working size in.
        let making = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(50000), 0);
        let twap = program(crate::db::models::ProgramKind::Twap, dec!(0), dec!(50000), six_h);

        let made = plan(&input(&m, &b, &p, &making, dec!(1000), six_h / 2));
        let worked = plan(&input(&m, &b, &p, &twap, dec!(1000), six_h / 2));

        // The market maker rests its far rungs; the twap rests none.
        assert!(
            made.quotes.iter().any(|q| q.level >= DEEP_LEVEL_BASE),
            "the market maker keeps its deep ladder: {:?}", made.quotes
        );
        assert!(
            !worked.quotes.iter().any(|q| q.level >= DEEP_LEVEL_BASE),
            "a twap rests no deep rung: {:?}", worked.quotes
        );

        // And the money it would have reserved goes to the working rungs. Shown
        // where the BUDGET binds rather than the per-rung ceiling, because that
        // is the case the reservation actually hurt: the ceiling hides it by
        // making both ladders quote the same size.
        let near = |d: &Decision| -> Decimal {
            d.quotes
                .iter()
                .filter(|q| q.side == Side::Buy && q.level < DEEP_LEVEL_BASE)
                .map(|q| q.amount * q.price)
                .sum()
        };
        let mut thin_made = input(&m, &b, &p, &making, dec!(1000), six_h / 2);
        thin_made.quote_available = dec!(500);
        let mut thin_worked = input(&m, &b, &p, &twap, dec!(1000), six_h / 2);
        thin_worked.quote_available = dec!(500);
        let (made_near, worked_near) = (near(&plan(&thin_made)), near(&plan(&thin_worked)));
        assert!(
            worked_near > made_near * dec!(1.5),
            "the deep ladder's 45% funds the working rungs: {worked_near} vs {made_near}"
        );
    }

    /// Coins leaving the account cannot leave the ramp anchored above them.
    ///
    /// The live case, 2026-09-27: a twap started with PRL held, and a
    /// withdrawal of all of it went in flight seconds later. A withdrawal in
    /// flight is subtracted from inventory the moment it is requested -- the
    /// coins are already gone -- so the bot read inventory 0 against a ramp
    /// interpolating down from the whole holding. That is a target the entire
    /// holding above inventory,
    /// which pins the error at full lean: the six-hour schedule becomes "buy as
    /// fast as the caps allow", which is the one thing a TWAP exists to avoid.
    #[test]
    fn a_twap_anchor_never_sits_above_the_coins_we_hold() {
        use crate::db::models::Portfolio;
        let six_h = 6 * 3_600_000;
        // Started from a holding, destination the holding plus the cash priced
        // at the time, then every coin was withdrawn.
        let all_in = dec!(121200);
        let twap = program(crate::db::models::ProgramKind::Twap, dec!(67911.54), all_in, six_h);

        // Nothing held (it is all in flight), the cash untouched.
        let pf = Portfolio { price: dec!(1.489), base_qty: Decimal::ZERO, quote_qty: dec!(79348) };

        // At the start the target is what we hold, not what we used to hold.
        assert_eq!(twap.target_at(0, pf), Decimal::ZERO, "the anchor follows the balance down");
        // And it paces over the window rather than appearing at once.
        let quarter = twap.target_at(six_h / 4, pf);
        assert!(
            quarter > Decimal::ZERO && quarter < all_in / dec!(3),
            "a quarter in, about a quarter of the way: {quarter} of {all_in}"
        );
        assert_eq!(twap.target_at(six_h, pf), all_in, "the destination by the deadline");

        // The anchor only ever moves DOWN to meet the balance. Holding more
        // than we started with (a deposit of the coin itself) leaves the
        // schedule alone -- and cannot produce a sell, because a buy-only
        // program has no ask side whatever the target says.
        for held in [dec!(0), dec!(5000), dec!(67911.54), dec!(200000)] {
            let pf = Portfolio { price: dec!(1.489), base_qty: held, quote_qty: dec!(79348) };
            let anchored = twap.target_at(0, pf);
            assert_eq!(
                anchored, held.min(dec!(67911.54)),
                "held {held}: the anchor is the smaller of the balance and where we started"
            );
        }
    }

    /// TWAP mode means the TWAP and nothing else: a book whose asset is not
    /// being worked stands down rather than making a market beside it with the
    /// same money.
    #[test]
    fn twap_only_stands_every_other_book_down() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.twap_only = true;

        for kind in [
            crate::db::models::ProgramKind::Neutral,
            crate::db::models::ProgramKind::Balanced,
            crate::db::models::ProgramKind::Learned,
            crate::db::models::ProgramKind::Accumulate,
            crate::db::models::ProgramKind::Distribute,
            crate::db::models::ProgramKind::Flatten,
        ] {
            let other = program(kind, dec!(5000), dec!(9000), 0);
            let d = plan(&input(&m, &b, &p, &other, dec!(5000), 0));
            assert_eq!(d.halt, Some(Halt::TwapOnly), "{kind:?} must stand down in twap mode");
            assert!(d.quotes.is_empty(), "{kind:?}: {:?}", d.quotes);
        }

        // The twap itself quotes, and the flag changes nothing for anyone when
        // it is off.
        let twap = program(crate::db::models::ProgramKind::Twap, dec!(5000), dec!(100000), 6 * 3_600_000);
        let d = plan(&input(&m, &b, &p, &twap, dec!(5000), 0));
        assert!(d.halt.is_none(), "{:?}", d.halt);
        assert!(d.quotes.iter().any(|q| q.side == Side::Buy));

        let mut off = params();
        off.q.twap_only = false;
        let neutral = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        assert!(plan(&input(&m, &b, &off, &neutral, dec!(5000), 0)).halt.is_none());
    }

    /// A twap is execution, so the arms do not gate it: getting the size in is
    /// the job, not a bet on the spread.
    #[test]
    fn a_twap_is_not_gated_by_a_dead_arm() {
        let (m, b) = (market(), book());
        let p = params();
        let none = ArmScales::none();
        let twap = program(crate::db::models::ProgramKind::Twap, dec!(5000), dec!(100000), 6 * 3_600_000);
        assert!(twap.kind.is_execution(), "a twap is execution");

        let mut i = input(&m, &b, &p, &twap, dec!(5000), 0);
        i.arms = &none;
        let d = plan(&i);
        assert!(
            d.quotes.iter().any(|q| q.side == Side::Buy && q.amount > m.min_amount),
            "a defunded arm does not shrink a twap to the minimum: {:?}", d.quotes
        );
    }

    /// An arm sized to nothing rests the exchange minimum, not nothing: the
    /// fills are the evidence, and a rung that is never placed can never
    /// show what it would have earned. Every rung the full-size ladder
    /// places is still placed, at the minimum (two rungs that merge onto one
    /// price merge their minimums). A side the lean has zeroed stays off:
    /// the minimum is for arms, not a way around the program.
    #[test]
    fn an_arm_sized_to_nothing_rests_the_minimum() {
        let (m, b) = (market(), book());
        let mut p = params();
        p.q.levels = 2;
        p.q.size_jitter = dec!(0);
        p.q.inventory_size_skew = dec!(1);
        let prog = program(crate::db::models::ProgramKind::Neutral, dec!(0), dec!(10000), 0);
        let none = ArmScales::none();

        let mut i = input(&m, &b, &p, &prog, dec!(10000), 0);
        let full = plan(&i).quotes;
        i.arms = &none;
        let d = plan(&i);
        assert!(d.halt.is_none(), "{:?}", d.halt);
        assert!(!d.quotes.is_empty());
        for side in [Side::Buy, Side::Sell] {
            let at_full: Vec<_> = full.iter().filter(|q| q.side == side).collect();
            let at_min: Vec<_> = d.quotes.iter().filter(|q| q.side == side).collect();
            assert_eq!(at_min.len(), at_full.len(), "{side}: every full-size rung still rests: {at_min:?}");
            for (f, n) in at_full.iter().zip(&at_min) {
                assert_eq!((f.level, f.price), (n.level, n.price), "{side}: same rung, same price");
                assert!(n.amount >= m.min_amount && n.amount <= m.min_amount * Decimal::from(p.q.levels), "{side}: {n:?}");
                assert!(n.amount < f.amount, "{side}: the minimum is smaller than full size: {n:?} vs {f:?}");
            }
        }

        // Far over target with a full size skew: the asks are wanted, the
        // bids are not, and no minimum order revives them.
        let mut i = input(&m, &b, &p, &prog, dec!(30000), 0);
        i.arms = &none;
        let d = plan(&i);
        assert!(d.quotes.iter().all(|q| q.side == Side::Sell), "{:?}", d.quotes);
        assert!(!d.quotes.is_empty());
    }
}
