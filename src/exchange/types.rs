//! Wire types for the SafeTrade (OpenWare/Peatio) REST API.
//!
//! The exchange sends every number as a string, which suits us: they go
//! straight into `Decimal` without a float ever existing.

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};

use crate::num;

/// Mirrors the exchange's market object; `state` is carried for completeness.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Market {
    pub id: String,
    pub name: String,
    pub base_unit: String,
    pub quote_unit: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_amount: Decimal,
    pub amount_precision: u32,
    pub price_precision: u32,
    pub state: String,
}

impl Market {
    /// Smallest price increment. On PRL/USDT this is 0.01.
    pub fn tick(&self) -> Decimal {
        num::tick(self.price_precision)
    }

}

#[derive(Debug, Clone, Deserialize)]
pub struct Balance {
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub balance: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub locked: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub const BOTH: [Side; 2] = [Side::Buy, Side::Sell];

    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "buy" | "bid" => Some(Side::Buy),
            "sell" | "ask" => Some(Side::Sell),
            _ => None,
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Mirrors the exchange's order object.
///
/// The wire names are not the ones Peatio's documentation suggests: there is no
/// `remaining_volume` or `origin_volume`, the size fields are `origin_amount`
/// and `filled_amount`, the order type is `type`, and the timestamps are
/// RFC 3339 strings. Aliases cover the documented spellings in case a different
/// deployment uses them, and `remaining` is derived rather than read.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Order {
    pub id: i64,
    pub market: String,
    pub side: String,
    #[serde(default, alias = "type")]
    pub ord_type: Option<String>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub price: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub avg_price: Option<Decimal>,
    pub state: String,
    #[serde(alias = "origin_volume", with = "rust_decimal::serde::str")]
    pub origin_amount: Decimal,
    #[serde(
        default,
        alias = "executed_volume",
        with = "rust_decimal::serde::str"
    )]
    pub filled_amount: Decimal,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl Order {
    /// Size still resting on the book. The engine reads the exchange's own
    /// fields directly; this pins the arithmetic in the wire-format tests.
    #[cfg(test)]
    pub fn remaining(&self) -> Decimal {
        (self.origin_amount - self.filled_amount).max(Decimal::ZERO)
    }

    pub fn side(&self) -> Option<Side> {
        Side::parse(&self.side)
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Trade {
    pub id: i64,
    #[serde(default)]
    pub order_id: Option<i64>,
    pub market: String,
    /// On the PRIVATE endpoint this is the COUNTERPARTY's side, not ours.
    /// On the public endpoint it is absent.
    #[serde(default)]
    pub side: Option<String>,
    /// Our own side, present only on the private endpoint.
    #[serde(default)]
    pub order_side: Option<String>,
    /// Aggressor's side on the public endpoint.
    #[serde(default)]
    pub taker_type: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub fee: Option<Decimal>,
    #[serde(default)]
    pub fee_currency: Option<String>,
    #[serde(deserialize_with = "unix_seconds")]
    pub created_at: i64,
}

/// Accept a timestamp as either unix seconds or an RFC 3339 string.
///
/// The public trades endpoint returns `"created_at": 1790063895` and the
/// private one returns `"created_at": "2026-09-22T07:27:20Z"` -- same field,
/// same type, two encodings. Both decode to unix seconds here so nothing
/// downstream has to care which endpoint a trade came from.
fn unix_seconds<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(i64),
        Str(String),
    }
    match Raw::deserialize(d)? {
        Raw::Int(v) => Ok(v),
        Raw::Str(s) => chrono::DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.timestamp())
            .map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// Captured verbatim from a live POST /trade/market/orders response.
    /// Three wire-format surprises have already cost a deploy each, so the real
    /// payloads live in the tests rather than in anyone's memory.
    const LIVE_ORDER: &str = r#"{"id":600000002,"ask":"prl","bid":"usdt","market":"prlusdt",
        "price":"1.02","max_price":null,"avg_price":"1.02","stop_price":null,
        "filled_amount":"0","origin_amount":"502.6802","side":"sell","type":"limit",
        "state":"wait","maker_fee":"0.001","taker_fee":"0.001",
        "triggered_at":"2026-09-22T09:12:52.337Z","created_at":"2026-09-22T09:12:52.325Z",
        "updated_at":"2026-09-22T09:12:52.337Z"}"#;

    #[test]
    fn order_decodes_the_live_payload() {
        let o: Order = serde_json::from_str(LIVE_ORDER).expect("live order must decode");
        assert_eq!(o.id, 600000002);
        assert_eq!(o.side(), Some(Side::Sell));
        assert_eq!(o.price, Some(dec!(1.02)));
        assert_eq!(o.origin_amount, dec!(502.6802));
        assert_eq!(o.filled_amount, Decimal::ZERO);
        assert_eq!(o.ord_type.as_deref(), Some("limit"));
        assert_eq!(o.state, "wait");
    }

    #[test]
    fn order_remaining_never_goes_negative() {
        let o: Order = serde_json::from_str(
            r#"{"id":1,"market":"prlusdt","side":"sell","state":"done",
                "origin_amount":"100","filled_amount":"150"}"#,
        )
        .unwrap();
        assert_eq!(o.remaining(), Decimal::ZERO);
    }

    #[test]
    fn order_accepts_the_documented_field_names_too() {
        let o: Order = serde_json::from_str(
            r#"{"id":2,"market":"prlusdt","side":"buy","state":"wait","ord_type":"limit",
                "price":"0.98","origin_volume":"250","executed_volume":"50"}"#,
        )
        .expect("documented spellings must still decode");
        assert_eq!(o.remaining(), dec!(200));
    }

    /// The ticker's `volume` is quote currency and `amount` is base. Verified
    /// on PRL/BTC, where the two cannot be confused because the price is far
    /// from 1 -- on PRL/USDT they look identical and either reading "works".
    /// The shape of GET /trade/account/deposits, captured from the live API
    /// (identifiers and amounts replaced).
    ///
    /// Pinned because three wire-format assumptions have each cost a deploy,
    /// and this one governs whether a real deposit is recognised at all. Note
    /// there is no `state` field: deposits carry `credited`, withdrawals carry
    /// `status`.
    const LIVE_DEPOSIT: &str = r#"{"id":10000001,"currency":"prl","fee_currency":"prl",
        "blockchain_key":"pearl-tokens","amount":"1500","fee":"0","fee_paid":true,
        "credited":true,"from_address":"prl1qexample","txid":"0000aaaa",
        "created_at":"2026-09-22T08:14:58.99Z","completed_at":"2026-09-22T08:47:48.88Z",
        "updated_at":"2026-09-22T08:47:48.88Z","status":"collected"}"#;

    /// Copied from `GET /trade/account/withdraws`, not transcribed (identifiers
    /// and amounts replaced).
    ///
    /// The previous version of this constant said `"currency"`, because that is
    /// what deposits send and what a person writing it out by hand would
    /// assume. The exchange sends `"currency_id"` here, so the test passed
    /// against a shape that never existed while all 31 real withdrawals failed
    /// to decode.
    const LIVE_WITHDRAWAL: &str = r#"{"id":800001,"type":"onchain",
        "beneficiary_id":null,"currency_id":"prl","fee_currency_id":"prl",
        "blockchain_key":"pearl-tokens","amount":"2500.1234567","fee":"0.01",
        "txid":"000000000000000000000000000000000000000000000000000000000000beef",
        "status":"succeed","block_number":100000,"sum":"0","total":"0",
        "tid":"TID0000000001","rid":"prl1qexampledestinationaddress",
        "created_at":"2026-09-22T07:28:25.342Z","updated_at":"2026-09-22T07:39:25.535Z",
        "completed_at":"2026-09-22T07:39:25.535Z"}"#;

    #[test]
    fn deposit_decodes_the_live_payload() {
        let d: Transfer = serde_json::from_str(LIVE_DEPOSIT).expect("live deposit must decode");
        assert_eq!(d.id, 10000001);
        assert_eq!(d.currency, "prl");
        assert_eq!(d.amount, dec!(1500));
        assert!(d.is_settled(), "a credited deposit must count as settled");
        assert_eq!(d.net(Decimal::ONE), dec!(1500), "no fee on this one");
        // The timestamp must parse, or the row is silently skipped as too old.
        let ts = chrono::DateTime::parse_from_rfc3339(d.completed_at.as_deref().unwrap());
        assert!(ts.is_ok(), "completed_at must be RFC 3339");
    }

    #[test]
    fn withdrawal_decodes_the_live_payload() {
        let w: Transfer =
            serde_json::from_str(LIVE_WITHDRAWAL).expect("live withdrawal must decode");
        assert_eq!(w.id, 800001);
        // The field the exchange actually names `currency_id`.
        assert_eq!(w.currency, "prl", "a withdrawal with no currency cannot be applied");
        assert_eq!(w.amount, dec!(2500.1234567));
        assert!(w.is_settled(), "status `succeed` must count as settled");
        // A withdrawal costs amount PLUS fee; both leave the balance.
        assert_eq!(w.net(Decimal::NEGATIVE_ONE), dec!(-2500.1334567));
        // The timestamp must parse, or the row is silently skipped as too old.
        let ts = chrono::DateTime::parse_from_rfc3339(w.completed_at.as_deref().unwrap());
        assert!(ts.is_ok(), "completed_at must be RFC 3339");
    }

    /// A failed withdrawal is not in flight. Its money came back.
    ///
    /// The account this ran on carried one, status `failed`.
    /// The first version of the pending-withdrawal logic asked "is it settled?"
    /// and treated everything else as in flight, so it subtracted that amount
    /// from the tradeable USDT balance every loop, for ever. Live for about ten
    /// minutes before its own log line gave it away.
    #[test]
    fn a_failed_withdrawal_is_not_in_flight() {
        let w: Transfer = serde_json::from_str(
            r#"{"id":700001,"currency_id":"usdt","amount":"1000.000001","fee":"1",
                "status":"failed","created_at":"2026-08-25T07:10:13.047Z"}"#,
        )
        .expect("must decode");
        assert_eq!(w.pending_reason(), PendingState::Returned);
        assert!(!w.is_in_flight(), "failed money is back; it must not be subtracted");
        assert!(!w.is_settled(), "and it certainly did not settle");
    }

    /// Every state the exchange uses, sorted by what happened to the money.
    #[test]
    fn withdrawal_states_are_sorted_by_where_the_money_went() {
        let with = |status: &str| -> Transfer {
            serde_json::from_str(&format!(
                r#"{{"id":1,"currency_id":"prl","amount":"100","status":"{status}"}}"#
            ))
            .expect("must decode")
        };
        for s in ["succeed", "done"] {
            assert_eq!(with(s).pending_reason(), PendingState::Settled, "{s}");
            assert!(!with(s).is_in_flight(), "{s} has already gone");
        }
        for s in ["canceled", "cancelled", "rejected", "failed", "errored", "skipped"] {
            assert_eq!(with(s).pending_reason(), PendingState::Returned, "{s}");
            assert!(!with(s).is_in_flight(), "{s} means the money came back");
        }
        for s in ["prepared", "submitted", "accepted", "processing", "confirming"] {
            assert_eq!(with(s).pending_reason(), PendingState::InFlight, "{s}");
            assert!(with(s).is_in_flight(), "{s} means the money is locked and leaving");
        }
    }

    /// An unrecognised status must not be assumed to be in flight.
    ///
    /// Guessing "in flight" shrinks the tradeable balance permanently and
    /// silently, which is the worse of the two failures: guessing the other way
    /// overstates inventory once, loudly, and the caller logs it.
    #[test]
    fn an_unknown_withdrawal_status_is_not_treated_as_in_flight() {
        let w: Transfer = serde_json::from_str(
            r#"{"id":2,"currency_id":"prl","amount":"100","status":"teleported"}"#,
        )
        .expect("must decode");
        assert_eq!(w.pending_reason(), PendingState::Unknown);
        assert!(!w.is_in_flight());
    }

    /// The live one, which is what started all this.
    #[test]
    fn the_cold_storage_withdrawal_reads_as_in_flight() {
        let w: Transfer = serde_json::from_str(
            r#"{"id":841323,"currency_id":"prl","amount":"25000.99","fee":"0.01",
                "status":"confirming","created_at":"2026-09-23T07:25:47.144Z"}"#,
        )
        .expect("must decode");
        assert!(w.is_in_flight(), "confirming means locked and leaving");
        assert!(!w.is_settled(), "but not yet gone, so it must not move net deposits");
        // Amount PLUS fee is what the exchange locks.
        assert_eq!(w.amount + w.fee.unwrap(), dec!(25001));
    }

    /// Deposits and withdrawals disagree about what the currency field is
    /// called, so both spellings must decode to the same place. This is the
    /// assertion that would have caught the transcription error above.
    #[test]
    fn both_spellings_of_the_currency_field_decode() {
        for body in [
            r#"{"id":1,"currency":"prl","amount":"10","status":"succeed"}"#,
            r#"{"id":1,"currency_id":"prl","amount":"10","status":"succeed"}"#,
        ] {
            let t: Transfer = serde_json::from_str(body).expect("must decode");
            assert_eq!(t.currency, "prl", "failed on {body}");
        }
    }

    /// A deposit that has not been credited must not move anything: acting on
    /// it would have us quoting against coins that have not arrived.
    #[test]
    fn a_pending_deposit_is_not_settled() {
        let p: Transfer = serde_json::from_str(
            r#"{"id":1,"currency":"prl","amount":"5000","credited":false,
                "status":"submitted","created_at":"2026-09-23T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(!p.is_settled());
    }

    #[test]
    fn ticker_volume_is_quote_currency() {
        let e: TickerEntry = serde_json::from_str(
            r#"{"ticker":{"buy":"0.00001189","sell":"0.0000118","last":"0.0000119",
                "avg_price":"0.00001098","volume":"0.06079707","amount":"5560.4687"}}"#,
        )
        .expect("live ticker must decode");
        let t = e.ticker;
        let (vol, amt, avg) = (t.volume.unwrap(), t.amount.unwrap(), t.avg_price.unwrap());
        // amount x VWAP reproduces volume. Using `last` instead is off by ~9%
        // here, which is the difference between a closing print and an average.
        let implied = amt * avg;
        assert!(
            (implied - vol).abs() / vol < dec!(0.01),
            "amount {amt} x avg {avg} = {implied} should match volume {vol}"
        );
        // And the two are not interchangeable: base and quote differ by 5 orders
        // of magnitude on this pair, so reading the wrong one is not survivable.
        assert!(amt / vol > dec!(10000));
    }

    #[test]
    fn ticker_tolerates_missing_fields() {
        let e: TickerEntry = serde_json::from_str(r#"{"ticker":{}}"#).unwrap();
        assert!(e.ticker.volume.is_none());
    }

    #[test]
    fn trade_decodes_a_unix_timestamp() {
        let t: Trade = serde_json::from_str(
            r#"{"id":1,"market":"prlusdt","price":"0.98","amount":"10","total":"9.8",
                "taker_type":"sell","created_at":1790063895}"#,
        )
        .expect("public form must decode");
        assert_eq!(t.created_at, 1790063895);
    }

    #[test]
    fn trade_decodes_an_rfc3339_timestamp() {
        let t: Trade = serde_json::from_str(
            r#"{"id":23177915,"market":"prlusdt","price":"1.01","amount":"80.1421",
                "total":"80.943521","side":"buy","fee_currency":"prl","fee":"0.0801421",
                "order_id":600000001,"created_at":"2026-09-22T07:27:20Z"}"#,
        )
        .expect("private form must decode");
        assert_eq!(t.created_at, 1790062040);
        assert_eq!(t.our_side(), Some(Side::Buy));
        assert_eq!(t.order_id, Some(600000001));
    }

    #[test]
    fn public_trade_liquidity_comes_from_taker_type() {
        let mk = |side: &str, taker: &str| -> Trade {
            serde_json::from_str(&format!(
                r#"{{"id":1,"market":"prlusdt","price":"1","amount":"1","total":"1",
                     "side":"{side}","taker_type":"{taker}","created_at":1}}"#
            ))
            .unwrap()
        };
        assert_eq!(mk("sell", "buy").liquidity(), Some("maker"));
        assert_eq!(mk("sell", "sell").liquidity(), Some("taker"));
    }

    /// Regression, captured verbatim from the first live fill.
    ///
    /// This is our SELL being lifted. `side` says "buy" because the
    /// counterparty bought; `order_side` says "sell" because that is us.
    /// Reading `side` booked a 240 PRL sale as a 240 PRL purchase and moved the
    /// recorded position the wrong way by twice the trade size.
    #[test]
    fn private_trade_side_is_the_counterparty_not_us() {
        let t: Trade = serde_json::from_str(
            r#"{"id":23179675,"market":"prlusdt","price":"1.01","amount":"239.7705",
                "total":"242.168205","side":"buy","fee_currency":"usdt",
                "fee":"0.242168205","order_id":600000003,"order_side":"sell",
                "created_at":"2026-09-22T09:49:43Z"}"#,
        )
        .unwrap();
        assert_eq!(t.our_side(), Some(Side::Sell), "order_side is ours");
        // We were resting and got lifted, so we were the maker.
        assert_eq!(t.liquidity(), Some("maker"));
        // Fee paid in the quote asset is the tell that this was a sale.
        assert_eq!(t.fee_currency.as_deref(), Some("usdt"));
    }

    #[test]
    fn private_trade_where_we_were_the_taker() {
        // Our own buy that crossed: both fields agree, fee paid in base.
        let t: Trade = serde_json::from_str(
            r#"{"id":23177915,"market":"prlusdt","price":"1.01","amount":"80.1421",
                "total":"80.943521","side":"buy","fee_currency":"prl","fee":"0.0801421",
                "order_id":600000001,"order_side":"buy","created_at":"2026-09-22T07:27:20Z"}"#,
        )
        .unwrap();
        assert_eq!(t.our_side(), Some(Side::Buy));
        assert_eq!(t.liquidity(), Some("taker"));
    }
}

impl Trade {
    /// Our side of the trade.
    ///
    /// The private endpoint reports BOTH `order_side` (ours) and `side` (the
    /// counterparty's). Reading `side` inverts every buy and sell: a filled
    /// sell of ours arrives as `{"side":"buy","order_side":"sell"}` because
    /// someone bought from us. The giveaway is `fee_currency` -- a real buy
    /// pays its fee in the base asset, and that fill paid in quote.
    ///
    /// The public endpoint has neither field, so `side` is the fallback there.
    pub fn our_side(&self) -> Option<Side> {
        self.order_side
            .as_deref()
            .or(self.side.as_deref())
            .and_then(Side::parse)
    }

    /// The side that crossed the spread to make this trade happen.
    fn aggressor(&self) -> Option<Side> {
        if self.order_side.is_some() {
            // Private: `side` is the counterparty, who is by definition the taker
            // when our own order was resting.
            self.side.as_deref().and_then(Side::parse)
        } else {
            self.taker_type.as_deref().and_then(Side::parse)
        }
    }

    /// Whether we were the maker: true exactly when the aggressor was someone
    /// on the other side of us.
    pub fn liquidity(&self) -> Option<&'static str> {
        let ours = self.our_side()?;
        let taker = self.aggressor()?;
        Some(if taker == ours { "taker" } else { "maker" })
    }
}

/// A credited deposit or a completed withdrawal, as the exchange records it.
///
/// Authoritative, unlike inferring a transfer from a balance that moved: it
/// carries the exact amount, the currency and an id we can remember, so there
/// is nothing to guess and nothing to confuse with our own trading.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Transfer {
    pub id: i64,
    /// Deposits send `currency`; withdrawals send `currency_id`. Not a
    /// nicety -- without the alias every withdrawal fails to decode, and a
    /// withdrawal the ledger never sees is a balance drop with no explanation,
    /// which reports as a loss the size of the withdrawal.
    #[serde(alias = "currency_id")]
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub fee: Option<Decimal>,
    /// Deposits only; withdrawals use `status`.
    #[serde(default)]
    pub credited: Option<bool>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

/// What the exchange has done with the money behind a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// Left the account for good.
    Settled,
    /// Came back, or never left. Not pending, however old.
    Returned,
    /// Requested and locked, neither gone nor returned.
    InFlight,
    /// A status we do not recognise. Treated as not in flight, and logged.
    Unknown,
}

impl Transfer {
    /// Whether this has actually landed in the balance.
    ///
    /// A pending deposit must not move the target: acting on it would have us
    /// selling against coins that have not arrived.
    pub fn is_settled(&self) -> bool {
        self.credited.unwrap_or(false)
            || self
                .status
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("succeed") || s.eq_ignore_ascii_case("done"))
    }

    /// Whether this withdrawal is in flight: requested, funds locked by the
    /// exchange, not yet gone and not yet returned.
    ///
    /// Deliberately an ALLOWLIST rather than "anything not settled". A failed
    /// withdrawal from a month ago -- there is one on this account, 12,986 USDT
    /// from 25 August, status `failed` -- is not in flight. Its money came
    /// back. Counting it as pending would subtract it from our tradeable
    /// balance every loop, for ever, silently shrinking the bid ladder by the
    /// size of an old mistake.
    ///
    /// An unrecognised status returns false, so an unknown state can only cost
    /// us the 2.5x overstatement once, loudly, rather than shrinking us
    /// permanently and quietly. `pending_reason` says which case a status fell
    /// into so a new one is visible rather than guessed at.
    #[cfg(test)]
    pub fn is_in_flight(&self) -> bool {
        matches!(self.pending_reason(), PendingState::InFlight)
    }

    pub fn pending_reason(&self) -> PendingState {
        let Some(s) = self.status.as_deref() else {
            return PendingState::Unknown;
        };
        // Peatio's withdrawal states, grouped by what happened to the money.
        const GONE: [&str; 2] = ["succeed", "done"];
        const RETURNED: [&str; 6] =
            ["canceled", "cancelled", "rejected", "failed", "errored", "skipped"];
        const LOCKED: [&str; 7] = [
            "prepared", "submitted", "accepted", "processing", "confirming", "under_review",
            "suspected",
        ];
        let eq = |list: &[&str]| list.iter().any(|w| s.eq_ignore_ascii_case(w));
        if eq(&GONE) {
            PendingState::Settled
        } else if eq(&RETURNED) {
            PendingState::Returned
        } else if eq(&LOCKED) {
            PendingState::InFlight
        } else {
            PendingState::Unknown
        }
    }

    /// Net change to the balance, fee included.
    ///
    /// A deposit credits `amount` and the fee is already deducted from it; a
    /// withdrawal debits `amount` PLUS the fee. Subtracting the fee in both
    /// directions made a withdrawal short by twice it.
    pub fn net(&self, direction: Decimal) -> Decimal {
        let fee = self.fee.unwrap_or_default();
        if direction >= Decimal::ZERO {
            self.amount
        } else {
            -(self.amount + fee)
        }
    }
}

/// One side of the order book, best price first.
pub type Levels = Vec<(Decimal, Decimal)>;

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bids: Levels,
    pub asks: Levels,
}

#[derive(Debug, Deserialize)]
pub struct RawDepth {
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
}

impl From<RawDepth> for OrderBook {
    fn from(r: RawDepth) -> Self {
        let conv = |v: Vec<[String; 2]>| -> Levels {
            v.into_iter()
                .map(|[p, q]| (num::parse(&p), num::parse(&q)))
                .filter(|(p, q)| !p.is_zero() && !q.is_zero())
                .collect()
        };
        OrderBook { bids: conv(r.bids), asks: conv(r.asks) }
    }
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<(Decimal, Decimal)> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<(Decimal, Decimal)> {
        self.asks.first().copied()
    }

    pub fn mid(&self) -> Option<Decimal> {
        let (b, _) = self.best_bid()?;
        let (a, _) = self.best_ask()?;
        Some((b + a) / Decimal::TWO)
    }

    /// Size-weighted top-of-book price. Leans toward the side with less size,
    /// which is the side likely to trade through next, so it is a better
    /// short-horizon fair value than the plain mid.
    pub fn microprice(&self) -> Option<Decimal> {
        let (bp, bq) = self.best_bid()?;
        let (ap, aq) = self.best_ask()?;
        let total = bq + aq;
        if total.is_zero() {
            return Some((bp + ap) / Decimal::TWO);
        }
        Some((bp * aq + ap * bq) / total)
    }

    /// Quote-currency notional resting within `band` of the best price on a
    /// side. This is the number that says how much size the book can absorb,
    /// and it gates how large our own orders are allowed to be.
    pub fn depth_within(&self, side: Side, band: Decimal) -> Decimal {
        let (levels, best) = match side {
            Side::Buy => (&self.bids, self.best_bid()),
            Side::Sell => (&self.asks, self.best_ask()),
        };
        let Some((best, _)) = best else { return Decimal::ZERO };
        let limit = match side {
            Side::Buy => best * (Decimal::ONE - band),
            Side::Sell => best * (Decimal::ONE + band),
        };
        levels
            .iter()
            .filter(|(p, _)| match side {
                Side::Buy => *p >= limit,
                Side::Sell => *p <= limit,
            })
            .map(|(p, q)| p * q)
            .sum()
    }
}

/// A market's rolling 24-hour summary, as the exchange publishes it.
///
/// `volume` is quote-currency turnover and `amount` is base-currency. Verified
/// against PRL/BTC, where the two differ by the price and cannot be confused:
/// amount 5560.47 x price 1.09e-5 = volume 0.0608.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Ticker {
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub volume: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub amount: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub last: Option<Decimal>,
    /// Volume-weighted average over the window. `volume` reconciles against
    /// `amount` through THIS, not through `last`.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub avg_price: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TickerEntry {
    pub ticker: Ticker,
}
