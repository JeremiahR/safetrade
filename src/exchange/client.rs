//! SafeTrade REST client.
//!
//! Auth is OpenWare/Barong API-key style: three headers, where the signature is
//! `hex(HMAC_SHA256(secret, nonce || api_key))` and the nonce is unix
//! milliseconds. The server rejects nonces more than a few seconds from its own
//! clock (`authz.nonce_expired`), so the host clock has to be in sync — see
//! `Client::check_clock`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use reqwest::{Method, StatusCode};
use rust_decimal::Decimal;
use sha2::Sha256;
use tokio::sync::Semaphore;

use super::ratelimit::RateLimiter;
use super::types::*;
use crate::num;

const DEFAULT_BASE: &str = "https://safe.trade/api/v2";
/// The API sits behind Cloudflare, which 403s requests without a browser-ish UA.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) safetrade-mm/0.1";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("rate limited")]
    RateLimited,
    /// Cloudflare sat in front of the API and served a challenge page instead of
    /// passing us through. Comes back as HTML where we expect JSON. Treated as
    /// a hard back-off signal: retrying quickly makes it worse.
    #[error("blocked by Cloudflare challenge")]
    Challenged,
    #[error("nonce rejected by server (clock skew?)")]
    NonceRejected,
    #[error("insufficient balance")]
    InsufficientBalance,
    #[error("order not found")]
    OrderNotFound,
    #[error("api error {status}: {body}")]
    Other { status: u16, body: String },
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    key: Option<String>,
    secret: Option<String>,
    /// Serialises private calls. Peatio counts a nonce as used, so two in-flight
    /// requests built in the same millisecond can collide.
    private_gate: Semaphore,
    /// Paces every request. The exchange publishes no limit, so this one is
    /// learned: see `ratelimit`.
    limiter: RateLimiter,
}

impl Client {
    pub fn new(base: Option<String>, key: Option<String>, secret: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .context("building http client")?;
        Ok(Client {
            http,
            base: base.unwrap_or_else(|| DEFAULT_BASE.to_string()),
            key,
            secret,
            private_gate: Semaphore::new(1),
            limiter: RateLimiter::new(1.5),
        })
    }

    pub fn has_credentials(&self) -> bool {
        self.key.is_some() && self.secret.is_some()
    }

    fn auth_headers(&self) -> Result<Vec<(&'static str, String)>> {
        let (key, secret) = match (&self.key, &self.secret) {
            (Some(k), Some(s)) => (k, s),
            _ => bail!("no API credentials configured; set SAFETRADE_API_KEY and SAFETRADE_API_SECRET"),
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
            .map_err(|e| anyhow!("bad api secret: {e}"))?;
        mac.update(nonce.as_bytes());
        mac.update(key.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        Ok(vec![
            ("X-Auth-Apikey", key.clone()),
            ("X-Auth-Nonce", nonce),
            ("X-Auth-Signature", sig),
        ])
    }

    /// Retry transient failures, but **only for GETs**.
    ///
    /// Order placement is not idempotent: a POST that times out may still have
    /// reached the matching engine, and retrying it would double the position.
    /// Losing a quote for one loop is cheap; an accidental duplicate order is
    /// not, so writes get exactly one attempt.
    async fn request_retrying<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        private: bool,
    ) -> Result<T> {
        let mut delay = Duration::from_millis(500);
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(delay).await;
                delay *= 4;
            }
            match self.request(method.clone(), path, query, None, private).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let transient = matches!(
                        e.downcast_ref::<ApiError>(),
                        Some(ApiError::RateLimited | ApiError::Challenged)
                    ) || e.downcast_ref::<reqwest::Error>().is_some();
                    if !transient {
                        return Err(e);
                    }
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("request failed")))
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
        private: bool,
    ) -> Result<T> {
        // Held for the whole private request so the nonce cannot be reused.
        let _permit = if private { Some(self.private_gate.acquire().await?) } else { None };

        // Pace before building the request: the nonce is minted below and must
        // not go stale while we wait for a token.
        self.limiter.acquire().await;

        let url = format!("{}{}", self.base, path);
        let mut req = self.http.request(method, &url);
        if !query.is_empty() {
            req = req.query(query);
        }
        if private {
            for (k, v) in self.auth_headers()? {
                req = req.header(k, v);
            }
        }
        if let Some(b) = body {
            req = req.json(&b);
        }

        let resp = req.send().await.with_context(|| format!("request to {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status.is_success() {
            // A 200 carrying HTML is Cloudflare, not the exchange.
            if looks_like_html(&text) {
                self.limiter.on_limited().await;
                return Err(ApiError::Challenged.into());
            }
            self.limiter.on_success().await;
            return serde_json::from_str(&text)
                .with_context(|| format!("decoding {url}: {}", truncate(&text, 400)));
        }

        let err = classify(status, &text);
        match err {
            ApiError::RateLimited | ApiError::Challenged => self.limiter.on_limited().await,
            // Any other error is the exchange answering us, not refusing us.
            _ => self.limiter.on_success().await,
        }
        Err(err.into())
    }

    /// The learned request rate, and how often we have been refused.
    pub async fn pacing(&self) -> (f64, u64) {
        (self.limiter.rate().await, self.limiter.times_limited().await)
    }

    /// Restore a previously learned rate, so a restart does not rediscover the
    /// ceiling by bouncing off it again.
    pub async fn restore_rate(&self, rate: f64) {
        if rate.is_finite() && rate > 0.0 {
            self.limiter.set_rate(rate).await;
        }
    }

    // ---------- public ----------

    pub async fn markets(&self) -> Result<Vec<Market>> {
        self.request_retrying(
            Method::GET,
            "/peatio/public/markets",
            &[("limit", "1000".into())],
            false,
        )
        .await
    }

    pub async fn market(&self, id: &str) -> Result<Market> {
        let all = self.markets().await?;
        all.into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| anyhow!("market {id} not listed on the exchange"))
    }

    pub async fn depth(&self, market: &str, limit: u32) -> Result<OrderBook> {
        let raw: RawDepth = self
            .request_retrying(
                Method::GET,
                &format!("/peatio/public/markets/{market}/depth"),
                &[("limit", limit.to_string())],
                false,
            )
            .await?;
        Ok(raw.into())
    }

    /// Every market's 24-hour volume, in one request.
    ///
    /// Preferred over deriving a rate from the trades endpoint. That page caps
    /// at 1000 rows, which on a busy book spans twenty minutes -- and
    /// extrapolating twenty minutes of a rally to a day overstated PRL/USDT's
    /// volume more than fourfold. Some rows also carry timestamps days in the
    /// future, which corrupts any span computed from them. This number is the
    /// exchange's own and needs no arithmetic.
    pub async fn tickers(&self) -> Result<std::collections::HashMap<String, TickerEntry>> {
        self.request_retrying(Method::GET, "/peatio/public/markets/tickers", &[], false)
            .await
    }

    pub async fn public_trades(&self, market: &str, limit: u32) -> Result<Vec<Trade>> {
        let raw: Vec<serde_json::Value> = self
            .request_retrying(
                Method::GET,
                &format!("/peatio/public/markets/{market}/trades"),
                &[("limit", limit.to_string())],
                false,
            )
            .await?;
        Ok(Self::decode_lenient(&raw, "public trade"))
    }

    // ---------- private ----------

    pub async fn balances(&self) -> Result<Vec<Balance>> {
        // No query parameters. This endpoint 404s if given *any* query string
        // -- not just an out-of-range limit -- so `?limit=100` fails exactly
        // like `?limit=99999` does.
        self.request_retrying(Method::GET, "/trade/account/balances", &[], true).await
    }

    pub async fn open_orders(&self, market: &str) -> Result<Vec<Order>> {
        let raw: Vec<serde_json::Value> = self
            .request_retrying(
                Method::GET,
                "/trade/market/orders",
            &[
                ("market", market.to_string()),
                ("state", "wait".into()),
                    ("limit", "1000".into()),
                ],
                true,
            )
            .await?;
        Ok(Self::decode_lenient(&raw, "open order"))
    }

    /// Decode a list where one malformed element must not lose the rest.
    ///
    /// A trades page decodes all-or-nothing, and `since` is derived from the
    /// last INGESTED fill -- so a single undecodable row (an empty `fee`, say)
    /// stays inside the window forever, ingestion stops permanently, the
    /// position freezes, and every later balance move reads as a deposit.
    fn decode_lenient<T: serde::de::DeserializeOwned>(raw: &[serde_json::Value], what: &str) -> Vec<T> {
        let mut out = Vec::with_capacity(raw.len());
        let mut bad = 0usize;
        for v in raw {
            match serde_json::from_value::<T>(v.clone()) {
                Ok(t) => out.push(t),
                Err(e) => {
                    bad += 1;
                    if bad <= 3 {
                        tracing::warn!(
                            %what, error = %e, row = %truncate(&v.to_string(), 200),
                            "skipping an undecodable row"
                        );
                    }
                }
            }
        }
        if bad > 0 {
            tracing::warn!(%what, skipped = bad, kept = out.len(), "some rows did not decode");
        }
        out
    }

    /// Our own trades, newest first. `time_from` is unix *seconds*.
    pub async fn my_trades(&self, market: &str, time_from: Option<i64>, limit: u32) -> Result<Vec<Trade>> {
        // No `order_by`: this endpoint 500s on it, and it already returns
        // newest-first. `ingest_fills` sorts chronologically anyway, so the
        // server's order is not something we depend on.
        let mut q = vec![
            ("market", market.to_string()),
            ("limit", limit.to_string()),
        ];
        if let Some(t) = time_from {
            q.push(("time_from", t.to_string()));
        }
        let raw: Vec<serde_json::Value> = self
            .request_retrying(Method::GET, "/trade/market/trades", &q, true)
            .await?;
        Ok(Self::decode_lenient(&raw, "own trade"))
    }

    /// Deposits the exchange has credited, newest first.
    pub async fn deposits(&self, limit: u32) -> Result<Vec<Transfer>> {
        let raw: Vec<serde_json::Value> = self
            .request_retrying(
                Method::GET,
                "/trade/account/deposits",
                &[("limit", limit.to_string())],
                true,
            )
            .await?;
        Ok(Self::decode_lenient(&raw, "deposit"))
    }

    /// Withdrawals, newest first. Note the endpoint is `withdraws`.
    pub async fn withdrawals(&self, limit: u32) -> Result<Vec<Transfer>> {
        let raw: Vec<serde_json::Value> = self
            .request_retrying(
                Method::GET,
                "/trade/account/withdraws",
                &[("limit", limit.to_string())],
                true,
            )
            .await?;
        Ok(Self::decode_lenient(&raw, "withdrawal"))
    }

    pub async fn place_limit(
        &self,
        market: &str,
        side: Side,
        amount: Decimal,
        price: Decimal,
    ) -> Result<Order> {
        let body = serde_json::json!({
            "market": market,
            "side": side.as_str(),
            "amount": num::to_str(amount),
            "type": "limit",
            "price": num::to_str(price),
        });
        self.request(Method::POST, "/trade/market/orders", &[], Some(body), true).await
    }

    pub async fn cancel(&self, id: i64) -> Result<Order> {
        self.request(
            Method::POST,
            &format!("/trade/market/orders/{id}/cancel"),
            &[],
            None,
            true,
        )
        .await
    }

    /// Confirms credentials work and measures clock skew against the server.
    /// A skew over a couple of seconds makes every private call fail with
    /// `authz.nonce_expired`, and the error alone does not say why.
    pub async fn check_clock(&self) -> Result<()> {
        match self.balances().await {
            Ok(_) => Ok(()),
            Err(e) => {
                if let Some(ApiError::NonceRejected) = e.downcast_ref::<ApiError>() {
                    bail!(
                        "server rejected our nonce — the host clock is out of sync with the \
                         exchange. Fix with `sudo timedatectl set-ntp true` and retry."
                    );
                }
                Err(e)
            }
        }
    }
}

fn classify(status: StatusCode, body: &str) -> ApiError {
    let b = body.to_lowercase();
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ApiError::RateLimited;
    }
    // Cloudflare's answer to a challenge is usually an HTML page, but it also
    // returns a bare 403 with an EMPTY body. The exchange's own errors are
    // always JSON like {"errors":[...]}, so a 403 that is not JSON did not come
    // from the exchange at all -- it came from the edge in front of it, and the
    // right response is to back off, not to treat it as a permissions failure.
    if looks_like_html(body) {
        return ApiError::Challenged;
    }
    if status == StatusCode::FORBIDDEN && !body.trim_start().starts_with(['{', '[']) {
        return ApiError::Challenged;
    }
    if b.contains("nonce") {
        return ApiError::NonceRejected;
    }
    if b.contains("insufficient") || b.contains("balance") {
        return ApiError::InsufficientBalance;
    }
    // Only trust an explicit "no such order" marker. Treating every 404 as
    // OrderNotFound once reported a malformed *balances* request as a missing
    // order, which sent the investigation in entirely the wrong direction.
    if b.contains("order_not_found") || b.contains("market.order.invalid_id") {
        return ApiError::OrderNotFound;
    }
    let body = if body.trim().is_empty() {
        "(empty body)".to_string()
    } else {
        truncate(body, 300)
    };
    ApiError::Other { status: status.as_u16(), body }
}

fn looks_like_html(body: &str) -> bool {
    let head = body.trim_start().get(..200).unwrap_or(body).to_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html") || head.contains("<title>")
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..s.floor_char_boundary(n)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_html_challenge_is_recognised() {
        let e = classify(StatusCode::FORBIDDEN, "<!DOCTYPE html><html><title>Just a moment</title>");
        assert!(matches!(e, ApiError::Challenged));
    }

    /// Cloudflare also rejects with a 403 and no body at all. Without this the
    /// bot read it as a permissions failure and dropped the loop instead of
    /// backing off and retrying.
    #[test]
    fn cloudflare_empty_403_is_recognised() {
        assert!(matches!(classify(StatusCode::FORBIDDEN, ""), ApiError::Challenged));
        assert!(matches!(classify(StatusCode::FORBIDDEN, "   \n"), ApiError::Challenged));
    }

    /// A real 403 from the exchange is JSON and must NOT be mistaken for the
    /// edge, or a genuine permissions problem would be retried forever.
    #[test]
    fn a_json_403_from_the_exchange_is_not_a_challenge() {
        let e = classify(StatusCode::FORBIDDEN, r#"{"errors":["authz.invalid_permission"]}"#);
        assert!(matches!(e, ApiError::Other { status: 403, .. }), "got {e:?}");
    }

    #[test]
    fn untrusted_ip_stays_a_plain_error() {
        let e = classify(StatusCode::UNAUTHORIZED, r#"{"errors":["authz.apikey_untrusted_ip"]}"#);
        assert!(matches!(e, ApiError::Other { status: 401, .. }), "got {e:?}");
    }

    #[test]
    fn nonce_rejection_is_its_own_error() {
        let e = classify(StatusCode::UNAUTHORIZED, r#"{"errors":["authz.nonce_expired"]}"#);
        assert!(matches!(e, ApiError::NonceRejected));
    }

    #[test]
    fn an_empty_body_is_reported_as_such() {
        match classify(StatusCode::BAD_GATEWAY, "") {
            ApiError::Other { body, .. } => assert_eq!(body, "(empty body)"),
            e => panic!("unexpected {e:?}"),
        }
    }
}
