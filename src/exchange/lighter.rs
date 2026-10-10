//! Lighter (lighter.xyz), read-only: the PRL perpetual's book and tape.
//!
//! Recorded beside SafeTrade's own books so the case for trading there -- a
//! hedge for the spot inventory, or a second book to quote on -- is made on
//! measured basis, depth and flow rather than on a guess. Nothing here signs
//! or sends an order, and nothing the strategy decides reads from it yet.
//!
//! Why this venue: PRL perp, zero maker and taker fees, a 0.0001 tick (SafeTrade
//! quotes PRL in 0.01, ~0.73% of price). Why only watching for now: the market
//! was listed on 2026-09-29 and its book is thin and one-sided -- $4.9k of
//! bids and $6 of asks within 1% of mid on the day it was first read.

use std::time::Duration;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::types::{Levels, OrderBook, Trade};

const DEFAULT_BASE: &str = "https://mainnet.zklighter.elliot.ai/api/v1";

pub struct Lighter {
    http: reqwest::Client,
    base: String,
    market_id: u32,
}

#[derive(Deserialize)]
struct RawOrder {
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    remaining_base_amount: Decimal,
}

#[derive(Deserialize)]
struct RawBook {
    #[serde(default)]
    bids: Vec<RawOrder>,
    #[serde(default)]
    asks: Vec<RawOrder>,
}

#[derive(Deserialize)]
struct RawTrade {
    trade_id: i64,
    #[serde(with = "rust_decimal::serde::str")]
    size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
    /// Milliseconds.
    timestamp: i64,
    /// The resting order was the ask, so the aggressor bought.
    is_maker_ask: bool,
}

#[derive(Deserialize)]
struct RawTrades {
    #[serde(default)]
    trades: Vec<RawTrade>,
}

/// Individual orders into price levels, best first. Lighter returns orders,
/// not levels: two resting at 1.308 are two rows.
fn levels(orders: Vec<RawOrder>, descending: bool) -> Levels {
    let mut out: Levels = Vec::new();
    for o in orders {
        if o.remaining_base_amount <= Decimal::ZERO {
            continue;
        }
        match out.iter_mut().find(|(p, _)| *p == o.price) {
            Some((_, q)) => *q += o.remaining_base_amount,
            None => out.push((o.price, o.remaining_base_amount)),
        }
    }
    out.sort_by(|a, b| if descending { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) });
    out
}

fn to_trade(t: RawTrade, market: &str) -> Trade {
    Trade {
        id: t.trade_id,
        order_id: None,
        market: market.to_string(),
        side: None,
        order_side: None,
        taker_type: Some(if t.is_maker_ask { "buy" } else { "sell" }.to_string()),
        price: t.price,
        amount: t.size,
        total: t.price * t.size,
        fee: None,
        fee_currency: None,
        created_at: t.timestamp / 1000,
    }
}

impl Lighter {
    pub fn new(market_id: u32) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("building Lighter http client")?;
        Ok(Lighter { http, base: DEFAULT_BASE.to_string(), market_id })
    }

    /// The market's name in our tables: kept apart from every SafeTrade book.
    pub fn market_key(&self) -> String {
        format!("lighter:{}", self.market_id)
    }

    pub async fn book(&self) -> Result<OrderBook> {
        let raw: RawBook = self
            .http
            .get(format!("{}/orderBookOrders", self.base))
            .query(&[("market_id", self.market_id.to_string()), ("limit", "100".to_string())])
            .send()
            .await
            .context("Lighter book")?
            .error_for_status()?
            .json()
            .await
            .context("decoding Lighter book")?;
        Ok(OrderBook { bids: levels(raw.bids, true), asks: levels(raw.asks, false) })
    }

    pub async fn trades(&self, limit: u32) -> Result<Vec<Trade>> {
        let raw: RawTrades = self
            .http
            .get(format!("{}/recentTrades", self.base))
            .query(&[("market_id", self.market_id.to_string()), ("limit", limit.to_string())])
            .send()
            .await
            .context("Lighter trades")?
            .error_for_status()?
            .json()
            .await
            .context("decoding Lighter trades")?;
        let key = self.market_key();
        Ok(raw.trades.into_iter().map(|t| to_trade(t, &key)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn orders_at_one_price_are_one_level_best_first() {
        let raw: RawBook = serde_json::from_str(
            r#"{"bids":[{"price":"1.308","remaining_base_amount":"491.53"},
                        {"price":"1.325","remaining_base_amount":"1116.41"},
                        {"price":"1.308","remaining_base_amount":"870.52"},
                        {"price":"1.30","remaining_base_amount":"0"}],
                "asks":[{"price":"1.3552","remaining_base_amount":"1844.75"},
                        {"price":"1.3291","remaining_base_amount":"4.54"}]}"#,
        )
        .unwrap();
        let bids = levels(raw.bids, true);
        assert_eq!(bids, vec![(dec!(1.325), dec!(1116.41)), (dec!(1.308), dec!(1362.05))]);
        let asks = levels(raw.asks, false);
        assert_eq!(asks[0], (dec!(1.3291), dec!(4.54)), "best ask first");
    }

    #[test]
    fn a_trade_reads_its_aggressor_from_the_resting_side() {
        let raw: RawTrades = serde_json::from_str(
            r#"{"trades":[{"trade_id":32613764243,"size":"93.70","price":"1.3291",
                "timestamp":1790731103664,"is_maker_ask":true}]}"#,
        )
        .unwrap();
        let t = to_trade(raw.trades.into_iter().next().unwrap(), "lighter:4097");
        assert_eq!(t.taker_type.as_deref(), Some("buy"), "the ask rested, so a buyer took it");
        assert_eq!(t.created_at, 1790731103, "seconds, as the tape expects");
        assert_eq!(t.amount, dec!(93.70));
    }
}
