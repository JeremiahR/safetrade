//! Telegram front end.
//!
//! Deliberately thin: it parses a message into exactly the same command that
//! `safetrade ctl` writes, and puts it in the same `commands` table. The engine
//! cannot tell the two apart, so there is one control plane, one audit trail,
//! and no second copy of the trading logic to drift out of sync.
//!
//! Read-only requests (`status`, `help`) are answered here from the database
//! without going near the exchange.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::{Config, TUNABLE};
use crate::db::Db;
use crate::db::models::ProgramKind;
use crate::reporting;

const API: &str = "https://api.telegram.org";
/// Long-poll window. Telegram holds the connection open until something
/// arrives, so this costs nothing and makes replies feel instant.
const POLL_TIMEOUT_SECS: u64 = 30;
/// Minimum gap between alerts of the same kind.
const ALERT_COOLDOWN: Duration = Duration::from_secs(600);

/// Token from the environment. `TELGRAM` is accepted because that is the name
/// already in use in the deployed .env; the correctly spelled names win.
pub fn token_from_env() -> Option<String> {
    ["TELEGRAM_BOT_TOKEN", "TELEGRAM", "TELGRAM"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| s.contains(':'))
}

pub struct Telegram {
    /// Whether pre-existing alerts have been suppressed for this process.
    suppressed: std::sync::atomic::AtomicBool,
    http: reqwest::Client,
    token: String,
    db: Db,
    cfg: Config,
}

#[derive(Debug, Deserialize)]
struct Updates {
    result: Vec<Update>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    callback_query: Option<CallbackQuery>,
}

/// A tapped inline button. `data` carries whatever we put on the button, so a
/// tap becomes the same command a typed message would.
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    from: Option<User>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    text: Option<String>,
    chat: Chat,
    #[serde(default)]
    from: Option<User>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct User {
    #[serde(default)]
    username: Option<String>,
}

impl Telegram {
    pub fn new(token: String, db: Db, cfg: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Must outlast the long poll, or every poll looks like a timeout.
            .timeout(Duration::from_secs(POLL_TIMEOUT_SECS + 15))
            .build()?;
        Ok(Telegram { suppressed: std::sync::atomic::AtomicBool::new(false), http, token, db, cfg })
    }

    /// The chat we talk to.
    ///
    /// Pinned to one chat on purpose: this bot can move money, and a Telegram
    /// bot username is guessable. `TELEGRAM_CHAT_ID` pins it explicitly;
    /// otherwise the first `/start` claims it and everyone else is ignored from
    /// then on.
    async fn chat_id(&self) -> Option<i64> {
        if let Ok(v) = std::env::var("TELEGRAM_CHAT_ID")
            && let Ok(id) = v.trim().parse()
        {
            return Some(id);
        }
        self.db
            .get_setting("telegram_chat_id")
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
    }

    pub async fn send(&self, text: &str) -> Result<()> {
        let Some(chat) = self.chat_id().await else {
            warn!("no Telegram chat bound yet; send /start to the bot");
            return Ok(());
        };
        self.send_to(chat, text).await
    }

    /// How long the bot was down, if that is known and worth mentioning.
    ///
    /// The heartbeat is written every loop, so the gap between the last one and
    /// now is the outage. A deploy takes seconds and is not worth a message.
    pub async fn downtime(&self) -> Option<Duration> {
        let last: i64 = self.db.get_setting("last_seen_ms").await.ok()??.parse().ok()?;
        let gap = crate::db::now_ms().saturating_sub(last);
        (gap > 0).then(|| Duration::from_millis(gap as u64))
    }

    /// The button row shown under every reply.
    ///
    /// Each button's `callback_data` is simply the command it stands for, so a
    /// tap and a typed message travel the identical path -- there is no second
    /// dispatch table to fall out of step with the first.
    /// Six buttons, each answering a different question.
    ///
    /// The old set had two that overlapped -- "Edge" repeated most of "Status"
    /// -- and showed Pause and Resume side by side, so one of them was always a
    /// no-op and neither told you which state you were in. The toggle now shows
    /// the action that is available, which also makes the current state
    /// readable at a glance.
    ///
    /// A stale keyboard on an old message is harmless: tapping re-sends an
    /// idempotent command, and the reply carries a fresh keyboard.
    fn keyboard(paused: bool) -> serde_json::Value {
        let toggle = if paused {
            serde_json::json!({"text": "▶️ Resume", "callback_data": "resume"})
        } else {
            serde_json::json!({"text": "⏸ Pause", "callback_data": "pause"})
        };
        serde_json::json!({
            "inline_keyboard": [
                [
                    {"text": "💰 Earnings", "callback_data": "status"},
                    {"text": "📊 Details", "callback_data": "details 24h"}
                ],
                [
                    {"text": "📅 Week", "callback_data": "details 7d"}
                ],
                [
                    {"text": "📖 Orders", "callback_data": "orders"},
                    {"text": "📋 Report", "callback_data": "report 24h"}
                ],
                [
                    toggle,
                    {"text": "❓ Help", "callback_data": "help"}
                ]
            ]
        })
    }

    /// Whether quoting is currently switched off. Best-effort: a keyboard that
    /// guesses wrong is a cosmetic problem, not a trading one.
    async fn is_paused(&self) -> bool {
        self.db.get_setting("enabled").await.ok().flatten().as_deref() == Some("false")
    }

    async fn send_to(&self, chat_id: i64, text: &str) -> Result<()> {
        // Telegram rejects anything over 4096 characters, so long output is
        // split rather than cut. Cutting it mid-message used to sever the
        // closing </pre>, which Telegram then rejected outright -- so the
        // longest and most useful replies were the ones that silently vanished.
        let paused = self.is_paused().await;
        for chunk in split_for_telegram(text) {
            self.send_one(chat_id, &chunk, paused).await?;
        }
        Ok(())
    }

    async fn send_one(&self, chat_id: i64, body: &str, paused: bool) -> Result<()> {
        let resp = self
            .http
            .post(format!("{API}/bot{}/sendMessage", self.token))
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": body,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
                "reply_markup": Self::keyboard(paused),
            }))
            .send()
            .await
            .context("sending Telegram message")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            warn!(%status, body = %detail, "Telegram send failed");
            // An error, not a shrug. `alert_loop` marks an alert delivered on
            // Ok, so returning Ok here threw away every alert that hit flood
            // control or a rejected message.
            anyhow::bail!("telegram send failed: {status} {detail}");
        }
        Ok(())
    }

    /// Run a loop forever, restarting it if it panics.
    ///
    /// A panic inside a spawned task kills only that task, silently. A decode
    /// error in the report did exactly that: the bot went on trading while
    /// Telegram stopped answering, and nothing said so. Trading must never
    /// depend on the chat, but the chat must not die quietly either.
    pub async fn supervise<F, Fut>(
        self: Arc<Self>,
        name: &'static str,
        shutdown: Arc<AtomicBool>,
        f: F,
    ) where
        F: Fn(Arc<Self>, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let mut failures = 0u32;
        while !shutdown.load(Ordering::Relaxed) {
            let task = std::panic::AssertUnwindSafe(f(self.clone(), shutdown.clone()));
            match futures_util::FutureExt::catch_unwind(task).await {
                Ok(()) => return, // finished cleanly, i.e. shutdown
                Err(_) => {
                    failures += 1;
                    warn!(task = name, failures, "task panicked; restarting");
                    self.db
                        .alert(
                            "task_panic",
                            &format!("Telegram task `{name}` panicked and was restarted."),
                        )
                        .await;
                    // Back off a little so a persistent fault does not spin.
                    tokio::time::sleep(Duration::from_secs(5 * failures.min(12) as u64)).await;
                }
            }
        }
    }

    /// Long-poll for commands until shutdown.
    pub async fn poll_loop(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        let mut offset: i64 = self
            .db
            .get_setting("telegram_offset")
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        info!("Telegram listener started");
        while !shutdown.load(Ordering::Relaxed) {
            let fetched = self.fetch(offset).await;
            let updates = match fetched {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "Telegram poll failed");
                    vec![]
                }
            };
            if updates.is_empty() {
                // Either a quiet window or a failed poll; either way, do not spin.
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            for u in updates {
                offset = u.update_id + 1;
                // Record the offset before acting, so a crash mid-command cannot
                // replay it — re-running a trading instruction is worse than
                // dropping it.
                self.db
                    .set_setting("telegram_offset", &offset.to_string())
                    .await
                    .ok();
                if let Some(cb) = u.callback_query {
                    self.handle_callback(cb).await;
                } else if let Some(m) = u.message {
                    self.handle(m).await;
                }
            }
        }
    }

    async fn fetch(&self, offset: i64) -> Result<Vec<Update>> {
        let r: Updates = self
            .http
            .get(format!("{API}/bot{}/getUpdates", self.token))
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", POLL_TIMEOUT_SECS.to_string()),
                ("allowed_updates", "[\"message\",\"callback_query\"]".to_string()),
            ])
            .send()
            .await?
            .json()
            .await?;
        Ok(r.result)
    }

    /// A tapped button.
    ///
    /// Telegram leaves a spinner on the button until `answerCallbackQuery` is
    /// called, so that happens first and unconditionally -- before any work
    /// that might fail and leave the button looking stuck.
    async fn handle_callback(&self, cb: CallbackQuery) {
        let _ = self
            .http
            .post(format!("{API}/bot{}/answerCallbackQuery", self.token))
            .json(&serde_json::json!({ "callback_query_id": cb.id }))
            .send()
            .await;

        let Some(data) = cb.data else { return };
        let chat = cb.message.as_ref().map(|m| m.chat.id);
        let bound = self.chat_id().await;
        if bound.is_none() || chat != bound {
            warn!(?chat, "ignoring a button press from an unbound chat");
            return;
        }

        info!(command = %data, "Telegram button");
        let reply = match self.dispatch(&data).await {
            Ok(r) => r,
            Err(e) => format!("⚠️ {e:#}"),
        };
        let _ = self.send_to(bound.unwrap(), &reply).await;
        let _ = cb.from; // identity already established by the chat binding
    }

    async fn handle(&self, m: Message) {
        let Some(text) = m.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            return;
        };
        let from = m.chat.id;
        let username = m.from.as_ref().and_then(|u| u.username.clone()).unwrap_or_default();

        // First /start claims the bot.
        let bound = self.chat_id().await;
        if bound.is_none() {
            if text.trim_start_matches('/').to_lowercase().starts_with("start") {
                self.db.set_setting("telegram_chat_id", &from.to_string()).await.ok();
                info!(chat_id = from, %username, "Telegram chat bound");
                self.db
                    .log("info", "telegram_bound", &format!("chat {from} ({username})"), None)
                    .await;
                let _ = self.send_to(from, &format!(
                    "Bound to this chat.\n\n{}",
                    help_text(&self.cfg.market)
                )).await;
            } else {
                let _ = self.send_to(from, "Send /start to bind this chat to the bot.").await;
            }
            return;
        }

        if bound != Some(from) {
            warn!(chat_id = from, %username, "ignoring Telegram message from an unbound chat");
            return;
        }

        info!(%text, "Telegram command");
        let reply = match self.dispatch(text).await {
            Ok(r) => r,
            Err(e) => format!("⚠️ {e:#}"),
        };
        let _ = self.send_to(from, &reply).await;
    }

    async fn dispatch(&self, text: &str) -> Result<String> {
        let cleaned = text.trim().trim_start_matches('/');
        // Telegram appends @botname to commands sent in groups.
        let cleaned = cleaned.split('@').next().unwrap_or(cleaned);
        let mut parts = cleaned.split_whitespace();
        let verb = parts.next().unwrap_or("").to_lowercase();
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();

        match verb.as_str() {
            "help" | "start" | "?" => Ok(help_text(&self.cfg.market)),

            // `status` is the glance; `report` is everything. Both take a
            // window, so the Week button is the same code path as the day one.
            // `status 7d` used to be accepted and silently answered for 24h.
            // `status` is earnings and nothing else; the old glance is
            // `details`. See `reporting::render_earnings`.
            "status" | "earnings" => self.earnings().await,
            "details" => self.status(args.first().map(String::as_str).unwrap_or("24h"), true).await,
            "report" => {
                self.status(args.first().map(String::as_str).unwrap_or("24h"), false).await
            }

            "pause" => self.queue("pause", serde_json::json!({}), "⏸ Pausing — resting orders cancelled, nothing new posted. Inventory is untouched.").await,
            "resume" => self.queue("resume", serde_json::json!({}), "▶️ Resuming.").await,
            // No `stop` alias on purpose. Someone who means "pause" types
            // "stop", and PANIC is not what they asked for.
            "panic" => {
                self.queue("panic", serde_json::json!({}), "🛑 PANIC — cancelling everything and disabling quoting.").await
            }

            // Was its own screen; everything it showed now sits in `status`
            // under "Trading" and "Edge per unit traded". Kept as an alias so
            // old muscle memory still lands somewhere sensible.
            "edge" => self.status(args.first().map(String::as_str).unwrap_or("24h"), true).await,

            // Every book, not just the primary one, and named -- the old
            // version listed the primary market's orders with no heading, so
            // three books' worth of quoting looked like one book's.
            "orders" => {
                let r = reporting::build_full(
                    &self.db, &self.cfg.market, &self.db.program_key(&self.cfg.market).await,
                    &self.db.quote_currencies().await, 3600,
                )
                .await?;
                let mut o = format!("<b>Resting orders ({})</b>\n", r.open_orders.len());
                if r.open_orders.is_empty() {
                    o.push_str("<pre>none — the bot has nothing posted</pre>\n");
                } else {
                    o.push_str("<pre>");
                    for (side, price, amount, placed) in &r.open_orders {
                        o.push_str(&format!(
                            "{:<4} {:>9} × {:>9}  {}\n",
                            side.to_uppercase(), price.round_dp(5), amount.round_dp(2),
                            reporting::age(*placed)
                        ));
                    }
                    o.push_str("</pre>\n");
                }
                if !r.recent_fills.is_empty() {
                    o.push_str("<b>Last fills</b>\n<pre>");
                    for (t, side, price, amount) in r.recent_fills.iter().take(5) {
                        o.push_str(&format!(
                            "{:<4} {:>9} × {:>9}  {}\n",
                            side.to_uppercase(), price.round_dp(5), amount.round_dp(2),
                            reporting::age(*t)
                        ));
                    }
                    o.push_str("</pre>");
                }
                Ok(o)
            }

            // Restart the scoreboard. A config change does this by itself
            // when the bot starts; this is for a change the config cannot see,
            // so a fix is not judged on the losses it was meant to stop.
            "epoch" | "reset" => {
                self.db.reset_analytics_epoch("epoch command").await?;
                Ok("✅ Scoreboard restarted. P&L, verdict and earnings now count only what \
                    happens from here. The arms keep their evidence.".to_string())
            }

            "settings" => {
                let current = self.db.all_settings().await.unwrap_or_default();
                // Deliberately NOT inside <pre>. Thirty-three keys with
                // sixty-character descriptions in a monospace block wraps every
                // single line on a phone; as proportional text it reads fine.
                let mut s = String::from("<b>Runtime settings</b>\n\n");
                for (k, desc) in TUNABLE {
                    let now = current
                        .iter()
                        .find(|(ck, _)| ck == k)
                        .map(|(_, v)| format!(" = <b>{}</b>", esc(v)))
                        .unwrap_or_default();
                    s.push_str(&format!(
                        "<code>{k}</code>{now}\n<i>{desc}</i>\n\n", k = esc(k), desc = esc(desc)
                    ));
                }
                Ok(s)
            }

            "set" => {
                let (k, v) = match (args.first(), args.get(1)) {
                    (Some(k), Some(v)) => (k.clone(), v.clone()),
                    _ => anyhow::bail!("usage: set &lt;key&gt; &lt;value&gt;  — send <code>settings</code> for the list"),
                };
                if !TUNABLE.iter().any(|(t, _)| *t == k) {
                    anyhow::bail!("unknown setting <code>{}</code> — send <code>settings</code> for the list", esc(&k));
                }
                self.queue(
                    "set",
                    serde_json::json!({ "key": k, "value": v }),
                    &format!("✅ queued: <code>{} = {}</code>", esc(&k), esc(&v)),
                )
                .await
            }

            // Relative verbs: "buy 5000" means acquire 5000 more, which is how
            // a person says it. The engine resolves that against live inventory
            // when it applies the command, so the number cannot go stale
            // between typing and execution.
            "buy" | "accumulate" => self.program(ProgramKind::Accumulate, &args, true).await,
            "sell" | "distribute" => self.program(ProgramKind::Distribute, &args, true).await,
            // Absolute verbs: "hold 20000" means end up holding 20000.
            "hold" | "neutral" => self.program(ProgramKind::Neutral, &args, false).await,
            // No `target` alias: it reads as "hold this much" and meant
            // "flatten", which on a mostly-PRL account is the opposite trade.
            "flat" | "flatten" => self.program(ProgramKind::Flatten, &args, false).await,

            // TWAP the lot in: "twap 6h", "twap over 6h max 1.60".
            "twap" | "ripitin" => self.twap(&args).await,

            // The steady state: hold a share of value in PRL and rebalance.
            "balance" | "balanced" | "rebalance" => self.balance(&args).await,
            // Or let the arms decide the share, and keep deciding it.
            "learn" | "auto" => {
                self.queue(
                    "learn",
                    serde_json::json!({ "note": "via telegram" }),
                    "✅ queued: the arms set the split between PRL, USDT and USDC from here, \
                     re-derived every hour and reached by drifting over a day. Deposits are \
                     absorbed as capital. Send <code>status</code> to watch the target.",
                )
                .await
            }

            // One line. Replying with the entire help meant a typo scrolled
            // thirty lines past whatever you were actually looking at.
            other => Ok(format!(
                "Unknown command <code>{}</code> — send <code>help</code> for the list.",
                esc(other)
            )),
        }
    }

    /// The base currency of the primary market, as the engine recorded it.
    /// Read, not guessed from the market name -- the suffix list that guessing
    /// needs is exactly the hard-coded rule this bot keeps getting bitten by.
    async fn base_ccy(&self) -> String {
        self.db.get_setting("base_unit").await.ok().flatten().unwrap_or_else(|| "base".into())
    }

    /// What we hold of the base asset right now, for previewing a command
    /// before it is queued. `None` if it cannot be read -- a preview is a
    /// courtesy, so a failure here must not block the command.
    async fn inventory(&self) -> Option<Decimal> {
        match self.base_ccy().await.as_str() {
            "base" => None,
            ccy => Some(reporting::held(&self.db, ccy).await),
        }
    }

    async fn earnings(&self) -> Result<String> {
        let r = reporting::build_full(
            &self.db, &self.cfg.market, &self.db.program_key(&self.cfg.market).await,
            &self.db.quote_currencies().await, 3600,
        )
        .await?;
        Ok(reporting::render_earnings(&r))
    }

    async fn status(&self, window: &str, compact: bool) -> Result<String> {
        let secs = crate::parse_duration(window).unwrap_or(86_400);
        let r = reporting::build_full(
            &self.db, &self.cfg.market, &self.db.program_key(&self.cfg.market).await,
            &self.db.quote_currencies().await, secs,
        )
        .await?;
        Ok(if compact {
            reporting::render_compact(&r)
        } else {
            format!("<pre>{}</pre>", esc(&reporting::render(&r)))
        })
    }

    /// `twap [over] 6h [max PRICE]` — work every spendable unit into the base
    /// asset, bids only, with nothing else quoting.
    ///
    /// No quantity: the target is everything the account can hold, re-derived
    /// live, which is also how a deposit mid-run is picked up.
    async fn twap(&self, args: &[String]) -> Result<String> {
        let mut over: Option<String> = None;
        let mut limit: Option<Decimal> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].to_lowercase().as_str() {
                "over" | "in" => {
                    over = args.get(i + 1).cloned();
                    i += 2;
                }
                "max" | "limit" | "at" => {
                    limit = args.get(i + 1).and_then(|v| v.parse().ok());
                    i += 2;
                }
                // A bare duration, as in `twap 6h`.
                w if crate::parse_duration(w).is_ok() => {
                    over = Some(args[i].clone());
                    i += 1;
                }
                _ => i += 1,
            }
        }
        let over = over.unwrap_or_else(|| "6h".into());
        let duration = crate::parse_duration(&over)?;
        let asset = self.base_ccy().await;
        self.queue(
            "program",
            serde_json::json!({
                "kind": "twap",
                "asset": asset,
                "duration_secs": duration,
                "limit_price": limit.map(|p| p.to_string()),
                "note": "twap: the whole account in, via telegram",
            }),
            &format!(
                "✅ queued: TWAP every spendable unit into {} over {}{}.\n\nBids only — \
                 the ask side is not quoted at all, and every book not working {} stands down. \
                 A deposit that lands mid-run is worked in too, over a fresh {} of its own.",
                asset.to_uppercase(),
                esc(&over),
                limit.map(|p| format!(", never above {p}")).unwrap_or_default(),
                asset.to_uppercase(),
                esc(&over),
            ),
        )
        .await
    }

    async fn program(&self, kind: ProgramKind, args: &[String], relative: bool) -> Result<String> {
        let qty: Decimal = match kind {
            // "flat" with no number means go to zero.
            ProgramKind::Flatten if args.is_empty() => Decimal::ZERO,
            _ => args
                .first()
                .ok_or_else(|| anyhow::anyhow!("how much? e.g. <code>buy 5000 over 6h</code>"))?
                .parse()
                .map_err(|_| anyhow::anyhow!("‘{}’ is not a number", esc(args.first().map(String::as_str).unwrap_or(""))))?,
        };
        if qty < Decimal::ZERO {
            anyhow::bail!("quantity must not be negative");
        }

        // Parse the trailing `over <dur>` / `max <price>` / `min <price>` words.
        let mut over: Option<String> = None;
        let mut limit: Option<Decimal> = None;
        let mut i = 1;
        while i < args.len() {
            match args[i].to_lowercase().as_str() {
                "over" | "in" => {
                    over = args.get(i + 1).cloned();
                    i += 2;
                }
                // The parser used to treat these four as synonyms, so
                // `buy 5000 min 0.90` was accepted and set the very limit the
                // engine reads as "never buy ABOVE 0.90" -- the opposite of
                // what the word means, and silently.
                w @ ("max" | "min" | "limit" | "at") => {
                    let wrong = match kind {
                        ProgramKind::Accumulate => w == "min",
                        ProgramKind::Distribute => w == "max",
                        _ => false,
                    };
                    if wrong {
                        anyhow::bail!(
                            "a {} takes <code>{}</code>, not <code>{w}</code> — \
                             did you mean <code>{} {qty} over 6h {} PRICE</code>?",
                            kind.as_str(),
                            if kind == ProgramKind::Accumulate { "max" } else { "min" },
                            if kind == ProgramKind::Accumulate { "buy" } else { "sell" },
                            if kind == ProgramKind::Accumulate { "max" } else { "min" },
                        );
                    }
                    limit = args.get(i + 1).and_then(|v| v.parse().ok());
                    i += 2;
                }
                _ => i += 1,
            }
        }

        if kind.is_scheduled() && over.is_none() {
            anyhow::bail!(
                "a {} needs a duration — e.g. <code>{} {} over 6h</code>\n\n\
                 Without one it is just a market order with extra steps.",
                kind.as_str(),
                if kind == ProgramKind::Accumulate { "buy" } else { "sell" },
                qty
            );
        }
        let duration = over.as_deref().map(crate::parse_duration).transpose()?;

        let signed = if relative && kind == ProgramKind::Distribute { -qty } else { qty };

        // Show the resolved number, not the one that was typed.
        //
        // Three verb families share this path: buy/sell are RELATIVE, hold/flat
        // are ABSOLUTE, and bare `flat` means sell every last coin. The engine
        // resolves all of them against live inventory when it applies the
        // command, so until now the reply said "queued: distribute 5000" and
        // never showed what that worked out to. On an account that is 90% PRL,
        // the difference between `hold 5000` and `sell 5000` is enormous and
        // nothing on the screen distinguished them.
        let held = self.inventory().await;
        let resolved = match (held, kind) {
            (Some(h), ProgramKind::Accumulate) => Some(h + qty),
            (Some(h), ProgramKind::Distribute) => Some(h - qty),
            (Some(_), _) => Some(qty),
            (None, _) => None,
        };

        // A relative sell bigger than the holding resolves to a negative
        // target, which is not a position this bot can hold.
        if let (Some(h), ProgramKind::Distribute) = (held, kind)
            && qty > h
        {
            anyhow::bail!(
                "you hold {} — selling {} would leave {}.\n\nSend <code>flat</code> to sell all of it.",
                h.round_dp(2), qty.round_dp(2), (h - qty).round_dp(2)
            );
        }

        let preview = match resolved {
            Some(t) => {
                let pct = if held.unwrap_or_default() > Decimal::ZERO {
                    let d = (t - held.unwrap_or_default()) / held.unwrap_or_default() * dec!(100);
                    format!("  ({:+.0}%)", d)
                } else {
                    String::new()
                };
                format!(
                    "\n<pre>{} {} → {}{}</pre>{}",
                    self.base_ccy().await.to_uppercase(),
                    held.unwrap_or_default().round_dp(0),
                    t.round_dp(0),
                    pct,
                    if t.is_zero() { "⚠️ this sells everything\n" } else { "" }
                )
            }
            None => String::new(),
        };

        self.queue(
            "program",
            serde_json::json!({
                "kind": kind.as_str(),
                "target": signed.to_string(),
                "relative": relative,
                "duration_secs": duration,
                "limit_price": limit.map(|p| p.to_string()),
                "note": "via telegram",
            }),
            &format!(
                "✅ queued: <b>{}</b> {}{}{}\n{}\nApplied on the next loop. Send <code>status</code> to watch.",
                kind.as_str(),
                qty,
                over.map(|o| format!(" over {}", esc(&o))).unwrap_or_default(),
                limit.map(|l| format!(", limit {l}")).unwrap_or_default(),
                preview,
            ),
        )
        .await
    }

    async fn balance(&self, args: &[String]) -> Result<String> {
        let raw: Decimal = match args.first() {
            Some(v) => v.trim_end_matches('%').parse().map_err(|_| {
                anyhow::anyhow!("‘{}’ is not a number — try <code>balance 50</code>", esc(v))
            })?,
            None => dec!(0.5),
        };
        let ratio = crate::normalise_ratio(raw)?;

        let mut over: Option<String> = None;
        let mut i = 1;
        while i < args.len() {
            if matches!(args[i].to_lowercase().as_str(), "over" | "in") {
                over = args.get(i + 1).cloned();
                i += 2;
            } else {
                i += 1;
            }
        }
        let duration = over.as_deref().map(crate::parse_duration).transpose()?;

        self.queue(
            "program",
            serde_json::json!({
                "kind": "balanced",
                "target": "0",
                "ratio": ratio.to_string(),
                "duration_secs": duration,
                "note": "via telegram",
            }),
            &format!(
                "✅ queued: hold <b>{}%</b> of value in {}{}\n\n\
                 From here the bot sells into strength and buys weakness, \
                 aiming to grow both pools.",
                (ratio * Decimal::ONE_HUNDRED).round_dp(1),
                esc(&self.cfg.market.to_uppercase()),
                over.map(|o| format!(", approached over {}", esc(&o))).unwrap_or_default(),
            ),
        )
        .await
    }

    async fn queue(&self, cmd: &str, args: serde_json::Value, ok: &str) -> Result<String> {
        self.db.push_command(cmd, args).await?;
        Ok(ok.to_string())
    }

    /// Drain the alert queue.
    ///
    /// Separate from the command poller because that one blocks for 30 seconds
    /// at a time on a long poll; an alert should not wait behind it.
    ///
    /// Alerts are grouped by kind and rate limited per kind. A sustained
    /// exchange outage raises a `loop_error` every few seconds, and a chat that
    /// buzzes every four seconds is a chat you mute — which is exactly when you
    /// would stop seeing the alert that matters.
    pub async fn alert_loop(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        // Once per process, not per restart of this loop: the supervisor writes
        // a `task_panic` alert just before restarting us, and suppressing on
        // every start swallowed exactly the alert that says we panicked.
        if !self.suppressed.swap(true, Ordering::Relaxed) {
            self.db.suppress_old_alerts().await.ok();
        }
        let mut last_sent: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();

        while !shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(15)).await;
            if self.chat_id().await.is_none() {
                continue;
            }
            let pending = self.db.pending_alerts(200).await.unwrap_or_default();
            if pending.is_empty() {
                continue;
            }

            // Group by kind, keeping the newest message as the representative.
            let mut grouped: std::collections::BTreeMap<String, (usize, String, Vec<i64>)> =
                std::collections::BTreeMap::new();
            for (id, kind, message) in pending {
                let e = grouped.entry(kind).or_insert((0, String::new(), Vec::new()));
                e.0 += 1;
                e.1 = message;
                e.2.push(id);
            }

            for (kind, (count, message, ids)) in grouped {
                let fresh = last_sent
                    .get(&kind)
                    .is_none_or(|t| t.elapsed() >= ALERT_COOLDOWN);

                if fresh {
                    let icon = match kind.as_str() {
                        "halt" => "⚠️",
                        "arms" => "📐",
                        "program_done" => "🏁",
                        "loop_error" => "❗",
                        _ => "ℹ️",
                    };
                    let more = if count > 1 {
                        format!("\n<i>(+{} more like this)</i>", count - 1)
                    } else {
                        String::new()
                    };
                    let text =
                        format!("{icon} <b>{}</b>\n{}{}", esc(&kind), esc(&message), more);
                    if self.send(&text).await.is_err() {
                        // Leave them unmarked so the next pass retries.
                        continue;
                    }
                    last_sent.insert(kind, std::time::Instant::now());
                }
                // Mark every one of them, sent or suppressed: a suppressed alert
                // has been accounted for in the summary above.
                for id in ids {
                    self.db.mark_notified(id).await.ok();
                }
            }
        }
    }

    /// Push a status message on a fixed cadence.
    pub async fn hourly_loop(self: Arc<Self>, shutdown: Arc<AtomicBool>, every: Duration) {
        let mut tick = tokio::time::interval(every);
        // The first tick fires immediately; skip it so a restart does not spam.
        tick.tick().await;
        while !shutdown.load(Ordering::Relaxed) {
            tick.tick().await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            if self.chat_id().await.is_none() {
                continue;
            }
            let built = self.earnings().await;
            let body = match built {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "could not build the hourly status");
                    continue;
                }
            };
            let _ = self.send(&body).await;
        }
    }
}

/// Escape for Telegram's HTML parse mode.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The manual.
///
/// Written so the RELATIVE verbs and the ABSOLUTE verbs are distinguishable on
/// every single line. `sell 5000` and `hold 5000` used to sit in different
/// sections with no word marking which was which, and on an account that is 90%
/// one asset they are wildly different trades.
fn help_text(market: &str) -> String {
    format!(
        "<b>SafeTrade bot — {}</b>\n\n\
         <b>Look</b>\n\
         <code>status</code> — earnings: last hour, so far, and projected\n\
         <code>details</code> — holdings, P&amp;L, edge, what it is doing\n\
         <code>details 7d</code> — the same over a window (1h, 24h, 7d)\n\
         <code>report</code> — every number, including warnings\n\
         <code>orders</code> — what is resting, and the last fills\n\n\
         <b>Trade a quantity</b> — relative to what you hold\n\
         <code>buy 5000 over 6h</code> — 5,000 <b>more</b>, over 6 hours\n\
         <code>buy 5000 over 2d max 1.02</code> — …never paying above 1.02\n\
         <code>sell 5000 over 12h</code> — 5,000 <b>fewer</b>, over 12 hours\n\
         <code>sell 5000 over 1d min 1.10</code> — …never below 1.10\n\n\
         <b>Put everything in</b> — no quantity, no market making\n\
         <code>twap 6h</code> — every spendable unit into {base} over 6 hours, <b>bids only</b>\n\
         <code>twap 6h max 1.60</code> — …never paying above 1.60\n\
         New capital that lands mid-run is worked in too, over a fresh window of its own.\n\n\
         <b>Set a stance</b> — an end state, not an amount to trade\n\
         <code>balance 50</code> — hold 50% of <i>value</i> in {base}, forever\n\
         <code>balance 50 over 24h</code> — …easing in over a day\n\
         <code>learn</code> — let the arms set the split, and keep setting it\n\
         <code>hold 20000</code> — end up <b>holding 20,000</b> and quote around it\n\
         <code>flat</code> — end up holding <b>zero</b> ⚠️ sells everything\n\
         <code>flat 5000</code> — end up holding 5,000\n\n\
         Every one of these replies with the resolved number before it runs.\n\n\
         <b>Control</b>\n\
         <code>pause</code> — cancel resting orders, stop quoting, keep running\n\
         <code>resume</code> — start quoting again\n\
         <code>panic</code> — the same as pause, but never from a button\n\n\
         <b>Tuning</b>\n\
         <code>settings</code> — every runtime setting and its value\n\
         <code>set size_base 400</code> — change one\n\
         <code>epoch</code> — restart the scoreboard from now (the arms keep their evidence)\n\n\
         Quantities are in {base}. Orders rest on the book and do not cross the \
         spread unless <code>allow_taker</code> is switched on.",
        esc(&market.to_uppercase()),
        base = esc(&market.to_uppercase().replace("USDT", "").replace("USDC", "")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_messages_split_with_balanced_tags() {
        let body = format!("<b>head</b>\n<pre>{}</pre>\n", "line of report\n".repeat(600));
        let parts = split_for_telegram(&body);
        assert!(parts.len() > 1, "a long report must be split, not truncated");
        for (i, p) in parts.iter().enumerate() {
            assert!(p.chars().count() <= 4096, "part {i} is over Telegram's limit");
            assert_eq!(
                p.matches("<pre>").count(),
                p.matches("</pre>").count(),
                "part {i} has unbalanced <pre>:\n{p}"
            );
        }
        // Nothing is dropped.
        let rejoined: String = parts.join("").replace("<pre>", "").replace("</pre>", "");
        let original = body.replace("<pre>", "").replace("</pre>", "");
        assert_eq!(rejoined, original, "splitting must not lose content");
    }

    #[test]
    fn short_messages_are_left_alone() {
        assert_eq!(split_for_telegram("hello"), vec!["hello".to_string()]);
    }

    #[test]
    fn html_escaping_covers_telegram_specials() {
        assert_eq!(esc("a<b>&c"), "a&lt;b&gt;&amp;c");
        // & must be escaped first or the others double-escape.
        assert_eq!(esc("&lt;"), "&amp;lt;");
    }

    #[test]
    fn help_mentions_every_user_facing_verb() {
        let h = help_text("prlusdt");
        for verb in ["status", "details", "report", "edge", "orders", "buy", "sell", "hold", "flat",
                     "twap", "balance", "learn", "pause", "resume", "panic", "settings", "set", "epoch"] {
            assert!(h.contains(verb), "help is missing {verb}");
        }
    }

    /// Every button must carry a command the dispatcher already understands.
    /// A tap and a typed message take the same path, so a button that names a
    /// command that does not exist would be a silent dead end.
    #[test]
    fn every_button_maps_to_a_real_command() {
        let known = [
            "status", "details", "report", "edge", "orders", "pause", "resume", "panic",
            "help", "settings", "set", "buy", "sell", "hold", "flat", "twap", "balance",
        ];
        for paused in [false, true] {
            let kb = Telegram::keyboard(paused);
            let rows = kb["inline_keyboard"].as_array().expect("rows");
            let mut count = 0;
            for row in rows {
                for b in row.as_array().expect("buttons") {
                    let data = b["callback_data"].as_str().expect("callback_data");
                    let verb = data.split_whitespace().next().unwrap_or("");
                    assert!(known.contains(&verb), "button {data:?} has no command behind it");
                    assert!(
                        !b["text"].as_str().unwrap_or("").is_empty(),
                        "button {data:?} has no label"
                    );
                    // Telegram rejects callback_data over 64 bytes.
                    assert!(data.len() <= 64, "callback_data {data:?} is too long");
                    count += 1;
                }
            }
            assert!(count >= 6, "expected a useful set of buttons, found {count}");
        }
    }

    /// Pause and Resume must never be offered at the same time. Showing both
    /// meant one was always a no-op, and neither told you which state you were
    /// in -- so "why is nothing filling" had no answer on the screen.
    #[test]
    fn the_pause_toggle_offers_exactly_one_action() {
        for (paused, want, unwanted) in [(false, "pause", "resume"), (true, "resume", "pause")] {
            let kb = Telegram::keyboard(paused).to_string();
            assert!(kb.contains(&format!("\"{want}\"")), "paused={paused} should offer {want}");
            assert!(
                !kb.contains(&format!("\"{unwanted}\"")),
                "paused={paused} must not also offer {unwanted}"
            );
        }
    }

    /// No button may fire an irreversible or destructive command. Every one of
    /// them is one tap away with no confirmation, so `panic` and the program
    /// verbs stay typed-only on purpose.
    #[test]
    fn no_button_can_move_the_book_in_one_tap() {
        for paused in [false, true] {
            let kb = Telegram::keyboard(paused).to_string();
            for dangerous in ["panic", "flat", "buy", "sell", "hold", "twap", "balance", "learn", "set "] {
                assert!(
                    !kb.contains(&format!("\"{dangerous}")),
                    "button fires {dangerous:?} with no confirmation step"
                );
            }
        }
    }

    /// Telegram rejects a message whose HTML does not balance, and rejects it
    /// wholesale -- so a stray tag does not garble the reply, it deletes it.
    #[test]
    fn help_html_is_balanced() {
        let h = help_text("prlusdt");
        for tag in ["b", "i", "code", "pre"] {
            assert_eq!(
                h.matches(&format!("<{tag}>")).count(),
                h.matches(&format!("</{tag}>")).count(),
                "unbalanced <{tag}> in help"
            );
        }
        // Escaped string continuations leave runs of indentation in the text.
        assert!(!h.contains("   "), "help has a run of literal spaces from a continuation");
    }

    /// The dangerous verbs must be distinguishable from the harmless ones on
    /// the line that describes them. `sell 5000` and `hold 5000` are wildly
    /// different trades on a one-sided account.
    #[test]
    fn help_marks_relative_and_absolute_verbs() {
        let h = help_text("prlusdt");
        assert!(h.contains("relative to what you hold"), "relative family unlabelled");
        assert!(h.contains("an end state"), "absolute family unlabelled");
        assert!(h.contains("sells everything"), "bare `flat` must say what it does");
        assert!(!h.contains("<code>target"), "`target` was ambiguous and is gone");
        assert!(!h.contains("<code>stop"), "`stop` aliased PANIC and is gone");
    }

    #[test]
    fn token_needs_a_colon_to_be_a_token() {
        unsafe {
            std::env::set_var("TELEGRAM_BOT_TOKEN", "not-a-token");
            assert_eq!(token_from_env(), None);
            std::env::set_var("TELEGRAM_BOT_TOKEN", "123:abc");
            assert_eq!(token_from_env().as_deref(), Some("123:abc"));
            std::env::remove_var("TELEGRAM_BOT_TOKEN");
        }
    }
}

/// Split a message into Telegram-sized pieces, keeping `<pre>` blocks closed.
///
/// Telegram's limit is 4096 characters and it rejects unbalanced tags, so a
/// naive truncation of a long report severed the closing `</pre>` and the whole
/// message was refused -- losing precisely the replies worth reading.
fn split_for_telegram(text: &str) -> Vec<String> {
    const LIMIT: usize = 3500;
    if text.chars().count() <= LIMIT {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_pre = false;
    for line in text.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > LIMIT && !cur.is_empty() {
            if in_pre {
                cur.push_str("</pre>");
            }
            out.push(std::mem::take(&mut cur));
            if in_pre {
                cur.push_str("<pre>");
            }
        }
        // Track whether we are inside a block, so each piece closes its own.
        for _ in line.matches("<pre>") {
            in_pre = true;
        }
        for _ in line.matches("</pre>") {
            in_pre = false;
        }
        cur.push_str(line);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}
