//! SafeTrade execution and market-making bot.
//!
//! Two goals, one mechanism. The primary job is to buy or sell size without
//! pushing the price around; the secondary job is to earn the spread while
//! doing it. Both reduce to "here is where inventory should be, get there
//! patiently and get paid for the patience" — see `strategy::quote`.

mod config;
mod db;
mod engine;
mod exchange;
mod num;
mod reporting;
mod strategy;
mod telegram;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;

use crate::config::{Config, Credentials, TUNABLE};
use crate::db::Db;
use crate::db::models::ProgramKind;
use crate::exchange::Client;

#[derive(Parser)]
#[command(name = "safetrade", version, about = "SafeTrade execution / market-making bot")]
struct Cli {
    /// Path to config.toml.
    #[arg(long, short = 'c', default_value = "config.toml", global = true)]
    config: PathBuf,

    /// Override the database path from the config (handy for `report` on a
    /// copy pulled down from the server).
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the bot.
    Run {
        /// Compute and log quotes without sending a single order.
        #[arg(long)]
        dry_run: bool,
        /// Do not start the Telegram front end even if a token is set.
        ///
        /// Telegram hands each update to exactly one long-polling consumer, so
        /// a second instance silently steals commands from the live bot and
        /// applies them to its own database. Dry runs skip Telegram for this
        /// reason already; this flag is for any other second instance.
        #[arg(long)]
        no_telegram: bool,
    },
    /// Write a commented default config.
    InitConfig {
        #[arg(default_value = "config.toml")]
        path: PathBuf,
    },
    /// Show account, market and connectivity status. Touches no orders.
    Status,
    /// Print the local report.
    Report {
        /// Window to report over, e.g. 1h, 24h, 7d.
        #[arg(long, default_value = "24h")]
        window: String,
        /// Render the short form Telegram sends for `status`, so what you see
        /// here is exactly what lands on your phone.
        #[arg(long)]
        compact: bool,
        /// Render the earnings screen Telegram sends hourly and for `status`.
        #[arg(long)]
        earnings: bool,
    },
    /// Re-estimate the arm table from the database and print it.
    ///
    /// Does offline exactly what the running bot does on its refresh: records
    /// the placement distance on older orders, estimates every arm from the
    /// last four days of orders and fills, and stores the table where `report`
    /// reads it. Judged as of the bot's last heartbeat, so a copy pulled down
    /// from the server reads the same as it would have on the box.
    Arms,
    /// Control a running bot by queueing a command in the database.
    #[command(subcommand)]
    Ctl(Ctl),
}

#[derive(Subcommand)]
enum Ctl {
    /// Pull all quotes and stop quoting. The bot keeps running.
    Pause,
    /// Resume quoting.
    Resume,
    /// Cancel everything and disable quoting, right now.
    Panic,
    /// Set a runtime parameter. With no arguments, lists what can be set.
    Set { key: Option<String>, value: Option<String> },
    /// Restart the scoreboard: from now on, only what happens next counts
    /// toward the reported P&L, verdict and earnings. The arms keep their
    /// evidence. Drawn automatically when the config changes; run this by
    /// hand after a change the config cannot see.
    Epoch,
    /// Hold a fixed share of portfolio *value* in the base asset, rebalancing
    /// continuously. This is the "grow both pools" stance: it sells into
    /// strength and buys weakness, so a ranging market adds to both sides.
    Balance {
        /// Share of total value to hold in the base asset, 0..1 (or a percent).
        #[arg(default_value = "0.5")]
        ratio: Decimal,
        /// Approach the target gradually over this long, e.g. 24h. Strongly
        /// recommended when starting from a lopsided position.
        #[arg(long)]
        over: Option<String>,
        /// Which asset to balance. Defaults to the primary market's base.
        ///
        /// Programs are filed per asset, so `--asset usdc` steers the USDC/USDT
        /// book independently of the PRL one.
        #[arg(long)]
        asset: Option<String>,
    },
    /// Let the arms set the split of the account between its currencies,
    /// and keep setting it: one learned program per base asset, starting at
    /// what the account holds now and drifting hourly toward the split that
    /// funds the arms that pay. Deposits are absorbed as capital.
    Learn {
        /// One base asset only (e.g. usdc); every base by default.
        #[arg(long)]
        asset: Option<String>,
    },
    /// TWAP the whole account into the base asset: bids only, nothing else
    /// quoting, new capital picked up by itself.
    ///
    /// The target is not a quantity you type. It is everything the account can
    /// hold if every unit of quote currency is spent, recomputed from live
    /// balances every loop, so a deposit mid-run is worked in too — over a
    /// fresh window of its own, from the moment it settles.
    Twap {
        /// Spread the work over this long, e.g. 6h. Also the window a deposit
        /// that lands mid-run gets to itself.
        #[arg(long, default_value = "6h")]
        over: String,
        /// Never pay above this price.
        #[arg(long)]
        limit_price: Option<Decimal>,
        /// Which asset to buy. Defaults to the primary market's base.
        #[arg(long)]
        asset: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Sell every unit of the base asset, slowly: asks only, a share of the
    /// day's volume, never below a floor, nothing else quoting. Deposits are
    /// simply more for sale. Pace via `ctl set sell_day_pct` and friends.
    Twapsell {
        /// Never sell below this.
        #[arg(long, default_value = "1.30")]
        floor: Decimal,
        /// Which asset to sell. Defaults to the primary market's base.
        #[arg(long)]
        asset: Option<String>,
    },
    /// Start an execution program.
    Program {
        /// neutral | accumulate | distribute | flatten
        kind: String,
        /// Target inventory in base currency (PRL).
        target: Decimal,
        /// Spread the work over this long, e.g. 6h, 3d. Omit for no deadline.
        #[arg(long)]
        over: Option<String>,
        /// Never buy above / sell below this price.
        #[arg(long)]
        limit_price: Option<Decimal>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Show queued and recently applied commands.
    Log,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "safetrade=info,warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // `init-config` must work before a config exists.
    if let Cmd::InitConfig { path } = &cli.cmd {
        return init_config(path);
    }

    let cfg = {
        let mut c = Config::load(&cli.config)?;
        if let Some(p) = &cli.db {
            c.db_path = p.clone();
        }
        c
    };

    match cli.cmd {
        Cmd::InitConfig { .. } => unreachable!("handled above"),

        Cmd::Run { dry_run, no_telegram } => {
            let creds = Credentials::from_env();
            if !dry_run && (creds.key.is_none() || creds.secret.is_none()) {
                bail!(
                    "SAFETRADE_API_KEY / SAFETRADE_API_SECRET are not set.\n\
                     Put them in .env or the systemd unit's environment file."
                );
            }
            let db = Db::open(&cfg.db_path).await?;
            let api = Arc::new(Client::new(cfg.api_base.clone(), creds.key, creds.secret)?);

            if !dry_run {
                // Retry rather than exit. An exchange outage, a Cloudflare
                // block or a maintenance window should cost us quoting for its
                // duration, not take the process down -- and a process that
                // exits cannot tell anyone why.
                let mut attempt = 0u32;
                loop {
                    match api.check_clock().await {
                        Ok(()) => break,
                        Err(e) => {
                            attempt += 1;
                            let wait = std::time::Duration::from_secs(
                                (15 * attempt.min(8)) as u64,
                            );
                            tracing::error!(
                                attempt,
                                error = %format!("{e:#}"),
                                retry_in_s = wait.as_secs(),
                                "cannot authenticate with SafeTrade; check the API key, its IP \
                                 allowlist and the host clock"
                            );
                            db.log(
                                "error",
                                "auth_failed",
                                &format!("attempt {attempt}: {e:#}"),
                                None,
                            )
                            .await;
                            if shutdown_requested() {
                                return Ok(());
                            }
                            tokio::time::sleep(wait).await;
                        }
                    }
                }
            }

            let shutdown = Arc::new(AtomicBool::new(false));
            spawn_signal_handler(shutdown.clone(), api.clone(), db.clone(), cfg.market_list(), dry_run);
            SHUTDOWN.store(false, Ordering::Relaxed);

            // The Telegram front end writes into the same command queue the CLI
            // uses, so it needs the database and nothing else. Running it as a
            // sibling task means a Telegram outage cannot stall trading.
            //
            // A dry run never starts it. Telegram delivers each update to
            // exactly one long-polling consumer, so a dry run on a laptop --
            // which skips the credential check and so starts happily anywhere --
            // would quietly take commands meant for the live bot and apply them
            // to its own throwaway database. The operator would see the reply
            // and believe the real bot had acted.
            let telegram_token = telegram::token_from_env()
                .filter(|_| !no_telegram)
                .filter(|_| !dry_run);
            if dry_run && telegram::token_from_env().is_some() {
                tracing::info!("dry run: Telegram disabled so the live bot keeps its updates");
            }
            match telegram_token {
                Some(token) => {
                    let tg = Arc::new(telegram::Telegram::new(token, db.clone(), cfg.clone())?);
                    // Supervised: a panic in any of these restarts the task
                    // rather than silently ending it.
                    tokio::spawn(tg.clone().supervise("poll", shutdown.clone(), |t, s| {
                        t.poll_loop(s)
                    }));
                    tokio::spawn(tg.clone().supervise("alerts", shutdown.clone(), |t, s| {
                        t.alert_loop(s)
                    }));
                    let every = std::time::Duration::from_secs(cfg.telegram_status_secs);
                    tokio::spawn(tg.clone().supervise("status", shutdown.clone(), move |t, s| {
                        t.hourly_loop(s, every)
                    }));
                    // No "started" message on a routine restart. A deploy is
                    // not news, and a chat that pings for non-events is a chat
                    // you learn to ignore. Only an unexpectedly long gap is
                    // worth saying out loud.
                    if let Some(gap) = tg.downtime().await
                        && gap > std::time::Duration::from_secs(900)
                    {
                        let _ = tg
                            .send(&format!(
                                "🟢 Back up after {} minutes down.",
                                gap.as_secs() / 60
                            ))
                            .await;
                    }
                }
                None => tracing::info!("no Telegram token in the environment; Telegram disabled"),
            }

            engine::Engine::new(cfg, db, api, dry_run).await?.run(shutdown).await
        }

        Cmd::Status => {
            let creds = Credentials::from_env();
            let api = Client::new(cfg.api_base.clone(), creds.key, creds.secret)?;
            status(&cfg, &api).await
        }

        Cmd::Report { window, compact, earnings } => {
            let db = Db::open_ro(&cfg.db_path)
                .await
                .with_context(|| format!("opening {} for reading", cfg.db_path.display()))?;
            let r = reporting::build_full(
                &db, &cfg.market, &db.program_key(&cfg.market).await, &db.quote_currencies().await,
                parse_duration(&window)?,
            )
            .await?;
            if compact || earnings {
                // Strip the HTML that Telegram renders, so the shape is legible
                // in a terminal without pretending to be a browser.
                let html = if earnings {
                    reporting::render_earnings(&r)
                } else {
                    reporting::render_compact(&r)
                };
                let text = html
                    .replace("<pre>", "").replace("</pre>", "")
                    .replace("<b>", "").replace("</b>", "")
                    .replace("<i>", "").replace("</i>", "")
                    .replace("<code>", "").replace("</code>", "")
                    .replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">");
                print!("{text}");
            } else {
                print!("{}", reporting::render(&r));
            }
            Ok(())
        }

        Cmd::Arms => {
            let db = Db::open(&cfg.db_path).await?;
            let n = db.resettle_reference_markouts().await?;
            if n > 0 {
                println!("  re-scored {n} markouts against the reference mid");
            }
            let n = db.backfill_order_distance().await?;
            if n > 0 {
                println!("  recorded placement distance on {n} older orders");
            }
            let n = db.backfill_order_move(cfg.risk.vol_window_secs * 1000, engine::ARMS_MAX_AGE_MS).await?;
            if n > 0 {
                println!("  recorded the range at placement on {n} older orders");
            }
            let n = db.backfill_order_width(engine::ARMS_MAX_AGE_MS).await?;
            if n > 0 {
                println!("  recorded the book width at placement on {n} older orders");
            }
            let params = config::Params::load(&cfg, &db, false).await;
            // An offline copy stops where the bot stopped. Judged from the
            // wall clock, a day-old copy would decay to nothing.
            let now = db
                .get_setting("last_seen_ms")
                .await?
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(db::now_ms);
            let mut books = Vec::new();
            for m in cfg.market_list() {
                let base = db
                    .get_setting(&format!("base_unit:{m}"))
                    .await?
                    .with_context(|| format!("{m}: no base_unit recorded; the bot has not run on this database"))?;
                books.push((m, base));
            }
            let sizes = engine::estimate_arms(&db, &books, &params, now).await?;
            db.set_setting("arms", &serde_json::to_string(&sizes)?).await?;
            println!(
                "\n  as of {}\n",
                chrono::DateTime::from_timestamp_millis(now)
                    .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_default()
            );
            print!("{}", reporting::render_arms(&sizes));
            println!();
            Ok(())
        }

        Cmd::Ctl(c) => {
            let db = Db::open(&cfg.db_path).await?;
            ctl(&db, &cfg, c).await
        }
    }
}

/// Load `.env` if present. Deliberately does not override variables already in
/// the environment, so a systemd `EnvironmentFile` wins over a stray file.
fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if std::env::var_os(k.trim()).is_none() {
                unsafe { std::env::set_var(k.trim(), v) };
            }
        }
    }
}

/// Mirrors the shutdown flag for code that runs before the engine exists.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

fn spawn_signal_handler(
    shutdown: Arc<AtomicBool>,
    api: Arc<Client>,
    db: Db,
    markets: Vec<String>,
    dry_run: bool,
) {
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        tracing::info!("signal received; cancelling orders now");
        shutdown.store(true, Ordering::Relaxed);
        SHUTDOWN.store(true, Ordering::Relaxed);

        // Cancel here rather than waiting for the loop to come back round.
        // A tick already in flight can take tens of seconds when the rate
        // limiter is backing off, and twice that has outrun the guard below and
        // exited with quotes still live on the book. Cancelling is idempotent,
        // so the engine doing it again on its way out costs nothing.
        //
        // Our own orders only, one at a time. The exchange's cancel-all took
        // the owner's hand-placed orders down with every restart.
        if !dry_run {
            for m in &markets {
                if db.keeps_orders_on_restart(m).await {
                    tracing::info!(market = %m, "slow sell: leaving our orders resting through the restart");
                    continue;
                }
                let ours = db.live_orders(m).await.unwrap_or_default();
                let mut n = 0;
                for o in &ours {
                    if api.cancel(o.exchange_id).await.is_ok() {
                        db.mark_cancelled(o.exchange_id).await.ok();
                        n += 1;
                    }
                }
                tracing::info!(market = %m, count = n, of = ours.len(), "cancelled ours on signal");
            }
        }

        // Last resort, comfortably inside systemd's TimeoutStopSec=90.
        tokio::time::sleep(std::time::Duration::from_secs(70)).await;
        tracing::error!("shutdown timed out — exiting hard, CHECK FOR RESTING ORDERS");
        std::process::exit(1);
    });
}

async fn status(cfg: &Config, api: &Client) -> Result<()> {
    let market = api.market(&cfg.market).await?;
    let book = api.depth(&cfg.market, 10).await?;

    println!("\n  MARKET  {}", market.name);
    println!("    tick               {}", market.tick());
    if let Some(mid) = book.mid() {
        let pct = market.tick() / mid * Decimal::ONE_HUNDRED;
        println!("    tick as % of mid   {:.2}%   <- every price step is this big", pct);
    }
    println!("    min amount         {}", market.min_amount);
    println!("    amount precision   {}", market.amount_precision);

    println!("\n  BOOK");
    match (book.best_bid(), book.best_ask()) {
        (Some((bp, bq)), Some((ap, aq))) => {
            println!("    best bid           {bp}  × {bq}");
            println!("    best ask           {ap}  × {aq}");
            println!(
                "    spread             {}  ({} ticks)",
                ap - bp,
                ((ap - bp) / market.tick()).round_dp(1)
            );
            if let Some(m) = book.microprice() {
                println!("    microprice         {}", m.round_dp(5));
            }
        }
        _ => println!("    one-sided or empty"),
    }

    println!("\n  ACCOUNT");
    if !api.has_credentials() {
        println!("    no API credentials in the environment");
        println!();
        return Ok(());
    }
    match api.balances().await {
        Ok(b) => {
            for c in [&market.base_unit, &market.quote_unit] {
                let e = b.iter().find(|x| x.currency.eq_ignore_ascii_case(c));
                match e {
                    Some(e) => println!(
                        "    {:<6}             {} available, {} locked",
                        e.currency.to_uppercase(), e.balance, e.locked
                    ),
                    None => println!("    {:<6}             0", c.to_uppercase()),
                }
            }
            match api.open_orders(&cfg.market).await {
                Ok(o) => println!("    resting orders     {}", o.len()),
                Err(e) => println!("    resting orders     error: {e}"),
            }
        }
        Err(e) => println!("    error: {e:#}"),
    }
    println!();
    Ok(())
}

async fn ctl(db: &Db, cfg: &Config, c: Ctl) -> Result<()> {
    match c {
        Ctl::Pause => queue(db, "pause", serde_json::json!({})).await,
        Ctl::Resume => queue(db, "resume", serde_json::json!({})).await,
        Ctl::Panic => queue(db, "panic", serde_json::json!({})).await,

        Ctl::Epoch => {
            let now = db.reset_analytics_epoch("ctl epoch").await?;
            println!(
                "\n  scoreboard restarted at {}\n  P&L, verdict and earnings now count only \
                 fills after this moment. The arms keep their evidence.\n",
                chrono::DateTime::from_timestamp_millis(now)
                    .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_default()
            );
            Ok(())
        }

        Ctl::Set { key, value } => match (key, value) {
            (Some(k), Some(v)) => {
                if !TUNABLE.iter().any(|(t, _)| *t == k) {
                    bail!(
                        "unknown setting {k:?}. Run `safetrade ctl set` with no arguments to \
                         list what can be set."
                    );
                }
                queue(db, "set", serde_json::json!({ "key": k, "value": v })).await
            }
            _ => {
                println!("\n  Runtime settings (safetrade ctl set <key> <value>):\n");
                let current = db.all_settings().await.unwrap_or_default();
                for (k, desc) in TUNABLE {
                    let now = current
                        .iter()
                        .find(|(ck, _)| ck == k)
                        .map(|(_, v)| format!("  [now: {v}]"))
                        .unwrap_or_default();
                    println!("    {k:<24} {desc}{now}");
                }
                println!();
                Ok(())
            }
        },

        Ctl::Balance { ratio, over, asset } => {
            let ratio = normalise_ratio(ratio)?;
            let duration = over.as_deref().map(parse_duration).transpose()?;
            let asset = match asset {
                Some(a) => a.to_lowercase(),
                None => db.program_key(&cfg.market).await.to_lowercase(),
            };
            queue(
                db,
                "program",
                serde_json::json!({
                    "kind": "balanced",
                    "target": "0",
                    "ratio": ratio.to_string(),
                    "duration_secs": duration,
                    "asset": asset,
                }),
            )
            .await?;
            println!(
                "  queued: hold {}% of value in {}{}",
                (ratio * Decimal::ONE_HUNDRED).round_dp(1),
                asset.to_uppercase(),
                over.map(|o| format!(", approached over {o}")).unwrap_or_default()
            );
            Ok(())
        }

        Ctl::Learn { asset } => {
            queue(db, "learn", serde_json::json!({ "asset": asset.map(|a| a.to_lowercase()) })).await?;
            println!("  queued: the arms set the split from here, re-derived hourly");
            Ok(())
        }

        Ctl::Twap { over, limit_price, asset, note } => {
            let duration = parse_duration(&over)?;
            let asset = match asset {
                Some(a) => a.to_lowercase(),
                None => db.program_key(&cfg.market).await.to_lowercase(),
            };
            queue(
                db,
                "program",
                serde_json::json!({
                    "kind": "twap",
                    "asset": asset,
                    "duration_secs": duration,
                    "limit_price": limit_price.map(|p| p.to_string()),
                    "note": note.unwrap_or_else(|| format!("twap: the whole account into {} over {over}", asset.to_uppercase())),
                }),
            )
            .await?;
            println!(
                "  queued: TWAP every spendable unit into {} over {over}{}",
                asset.to_uppercase(),
                limit_price.map(|p| format!(", never above {p}")).unwrap_or_default()
            );
            println!("  bids only; every book that is not working this asset stands down");
            Ok(())
        }

        Ctl::Twapsell { floor, asset } => {
            let asset = match asset {
                Some(a) => a.to_lowercase(),
                None => db.program_key(&cfg.market).await.to_lowercase(),
            };
            queue(
                db,
                "program",
                serde_json::json!({
                    "kind": "twapsell",
                    "asset": asset,
                    "limit_price": floor.to_string(),
                    "note": format!("twapsell: every {} for sale, slowly", asset.to_uppercase()),
                }),
            )
            .await?;
            println!("  queued: slow sell of every {} on the account, never below {floor}", asset.to_uppercase());
            println!("  asks only; every book that is not working this asset stands down");
            Ok(())
        }

        Ctl::Program { kind, target, over, limit_price, note } => {
            let k = ProgramKind::parse(&kind).ok_or_else(|| {
                anyhow::anyhow!("kind must be one of: neutral, accumulate, distribute, flatten")
            })?;
            if k == ProgramKind::Balanced {
                bail!("use `safetrade ctl balance <ratio>` for a balanced program");
            }
            if k == ProgramKind::Learned {
                bail!("use `safetrade ctl learn` for a learned program");
            }
            if k.is_scheduled() && over.is_none() {
                bail!(
                    "{} needs --over (e.g. --over 6h): a TWAP without a duration is just a \
                     market order with extra steps",
                    k.as_str()
                );
            }
            let duration = over.as_deref().map(parse_duration).transpose()?;
            queue(
                db,
                "program",
                serde_json::json!({
                    "kind": k.as_str(),
                    "target": target.to_string(),
                    "duration_secs": duration,
                    "limit_price": limit_price.map(|p| p.to_string()),
                    "note": note,
                }),
            )
            .await?;
            println!(
                "  queued: {} to {} {}{}",
                k.as_str(), target, cfg.market.to_uppercase(),
                over.map(|o| format!(" over {o}")).unwrap_or_default()
            );
            Ok(())
        }

        Ctl::Log => {
            let rows = sqlx::query_as::<_, (i64, i64, String, String, Option<i64>, Option<String>)>(
                "SELECT id, created_at, command, args, applied_at, result FROM commands
                 ORDER BY id DESC LIMIT 20",
            )
            .fetch_all(db.pool())
            .await?;
            println!();
            if rows.is_empty() {
                println!("  no commands yet\n");
            }
            for (id, created, cmd, args, applied, result) in rows.iter().rev() {
                let when = chrono::DateTime::from_timestamp_millis(*created)
                    .map(|d| d.format("%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let state = if applied.is_some() { "applied" } else { "PENDING" };
                println!("  [{id:>3}] {when}  {state:<8} {cmd} {args}");
                if let Some(r) = result {
                    println!("        → {r}");
                }
            }
            println!();
            Ok(())
        }
    }
}

async fn queue(db: &Db, cmd: &str, args: serde_json::Value) -> Result<()> {
    let id = db.push_command(cmd, args).await?;
    println!("  queued command #{id} ({cmd}) — the bot applies it on its next loop");
    Ok(())
}

/// Accept a weight either as a fraction (0.5) or a percentage (50).
///
/// Both readings of "50" are plausible and one of them silently builds a 50x
/// position, so the ambiguous middle is rejected rather than guessed at.
pub(crate) fn normalise_ratio(v: Decimal) -> Result<Decimal> {
    let r = if v > Decimal::ONE { v / Decimal::ONE_HUNDRED } else { v };
    if r <= Decimal::ZERO || r > Decimal::ONE {
        bail!("ratio must be between 0 and 1 (or 0 and 100 as a percentage), got {v}");
    }
    Ok(r)
}

/// Parse `90s`, `15m`, `6h`, `3d` into seconds.
pub(crate) fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => bail!("bad duration {s:?}; use forms like 90s, 15m, 6h, 3d"),
    };
    let n: i64 = num.trim().parse().with_context(|| format!("bad duration {s:?}"))?;
    if n <= 0 {
        bail!("duration must be positive");
    }
    Ok(n * mult)
}

fn init_config(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; move it aside first", path.display());
    }
    std::fs::write(path, DEFAULT_CONFIG)?;
    println!("  wrote {}", path.display());
    println!("  put SAFETRADE_API_KEY and SAFETRADE_API_SECRET in .env, then run:");
    println!("    safetrade status");
    Ok(())
}

const DEFAULT_CONFIG: &str = include_str!("../config.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("6h").unwrap(), 21600);
        assert_eq!(parse_duration("3d").unwrap(), 259200);
        assert_eq!(parse_duration("45").unwrap(), 45);
    }

    #[test]
    fn bad_durations_rejected() {
        assert!(parse_duration("0h").is_err());
        assert!(parse_duration("-1h").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("6w").is_err());
    }

    #[test]
    fn example_config_parses_and_matches_defaults() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("example config must parse");
        assert_eq!(cfg.market, "prlusdt");
        // The shipped example must not quote through the touch by default.
        assert_eq!(cfg.quoting.max_improve_ticks, 0);
        assert!(!cfg.quoting.allow_taker);
    }

    /// The production config's switches must actually REACH the struct.
    ///
    /// Every cycle key lived above the `[quoting]` header for as long as the
    /// file existed, and serde dropped all of them in silence: the file said
    /// `cycle_release = true` while the running bot read `false`. Parsing
    /// successfully is not the same as being read, so this asserts the values,
    /// not the absence of an error. `deny_unknown_fields` now catches the
    /// misplacement itself; this catches a key being moved back out, or renamed
    /// on one side only.
    #[test]
    fn the_production_cycle_switches_reach_the_quoting_struct() {
        let cfg: Config = toml::from_str(include_str!("../deploy/config.prod.toml"))
            .expect("production config must parse");
        let q = &cfg.quoting;
        assert!(q.cycle_release, "the release valve must be armed");
        assert!(q.cycle_recycle, "the stablecoin crossing must be armed");
        assert!(q.cycle_churn, "the acquiring book must quote both sides neutral");
        assert_eq!(q.cycle_floor_bps, rust_decimal_macros::dec!(20));
        assert!(q.cycle_recycle_max_pct > rust_decimal::Decimal::ZERO);
        assert!(q.cycle_recycle_trigger > rust_decimal::Decimal::ZERO);
        // And prlusdt is a leg, not a book we make markets on.
        assert_eq!(
            cfg.overrides.get("prlusdt").and_then(|o| o.cycle_leg_only),
            Some(true),
            "prlusdt must be cycle-leg-only"
        );
        // The one book that may be crossed, and only that one.
        assert!(!q.allow_taker, "the global must stay false");
        assert_eq!(cfg.overrides.get("usdcusdt").and_then(|o| o.allow_taker), Some(true));
        assert_eq!(cfg.overrides.get("prlusdc").and_then(|o| o.allow_taker), None);
    }

    #[test]
    fn every_tunable_key_is_unique() {
        let mut keys: Vec<&str> = TUNABLE.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "duplicate key in TUNABLE");
    }
}
