//! SQLite persistence. The database *is* the bot's state: quotes, fills,
//! position, book history, the control queue and the audit log all live in one
//! file, so a crash loses nothing and `report` can reconstruct any moment.

pub mod models;

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use self::models::*;
use crate::exchange::{OrderBook, Side};
use crate::num;

/// How long after a fill we judge it. Long enough that a tick of noise does
/// not dominate, short enough that it is still about the fill and not about
/// the day's trend.
pub const MARKOUT_HORIZON_MS: i64 = 300_000;

/// The setting that marks when the scoreboard last started: P&L, the verdict
/// and the earnings count only fills after it. See `analytics_epoch_ms`.
pub const EPOCH_KEY: &str = "epoch_ms";
/// Why the line was drawn -- "settings changed", "ctl epoch" -- so the status
/// screen can say which window it is reporting and what started it.
pub const EPOCH_REASON_KEY: &str = "epoch_reason";

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(dir).await.ok();
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .context("sqlite connect options")?
            .create_if_missing(true)
            // WAL lets `report` read the file while the bot is writing it, which
            // is what makes local reporting off a live copy safe.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(10));

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;

        let db = Db { pool };
        db.migrate().await?;
        Ok(db)
    }

    /// Read-only open, for `report` against a database being written elsewhere
    /// (or a copy pulled down from the server).
    pub async fn open_ro(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .read_only(true)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new().max_connections(2).connect_with(opts).await?;
        Ok(Db { pool })
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(include_str!("../../migrations/0001_init.sql"))
            .execute(&self.pool)
            .await
            .context("applying schema")?;

        // CREATE TABLE IF NOT EXISTS cannot add a column to a table that is
        // already there, so new columns are added explicitly and idempotently.
        let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('fills')")
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();
        for (name, ddl) in [
            ("mid_at_fill", "ALTER TABLE fills ADD COLUMN mid_at_fill TEXT"),
            ("edge", "ALTER TABLE fills ADD COLUMN edge TEXT"),
            ("markout", "ALTER TABLE fills ADD COLUMN markout TEXT"),
        ] {
            if !cols.iter().any(|c| c == name) {
                sqlx::query(ddl).execute(&self.pool).await?;
            }
        }
        // How far from fair value an order rested when it was placed, in
        // basis points on the passive side. This is what attributes every
        // fill to the ARM it was a bet on (`strategy::arms`). A statistic,
        // not money, so REAL is fine.
        let ocols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('orders')")
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();
        if !ocols.iter().any(|c| c == "bps") {
            sqlx::query("ALTER TABLE orders ADD COLUMN bps REAL").execute(&self.pool).await?;
        }
        // How fast the market was moving when the order was placed: the
        // five-minute range of fair value as a fraction of price, the same
        // reading the volatility breaker takes. The other half of the arm's
        // key (`strategy::arms::Regime`).
        if !ocols.iter().any(|c| c == "recent_move") {
            sqlx::query("ALTER TABLE orders ADD COLUMN recent_move REAL").execute(&self.pool).await?;
        }
        // How wide the stranger book was when the order was placed, in
        // ticks, our own size stripped. The last part of the arm's key
        // (`strategy::arms::Width`): on a tick-quantised book it is what
        // separates the touch of a wide book from a tick behind the touch of
        // a tight one, which the distance alone cannot.
        if !ocols.iter().any(|c| c == "width_ticks") {
            sqlx::query("ALTER TABLE orders ADD COLUMN width_ticks REAL").execute(&self.pool).await?;
        }
        // The epoch marker was named for the Kelly layer it first gated. The
        // meaning is unchanged: everything before it is another regime.
        if self.get_setting(EPOCH_KEY).await?.is_none()
            && let Some(v) = self.get_setting("kelly_since_ms").await?
        {
            self.set_setting(EPOCH_KEY, &v).await?;
        }
        // Rows nothing reads any more: the per-book Kelly table and its
        // marker, and the banked high-water mark from before it was keyed per
        // asset. A stale number in the settings table reads as a control, and
        // invites someone to act on it.
        sqlx::query(
            "DELETE FROM settings
             WHERE key LIKE 'edge:%' OR key IN ('kelly_since_ms', 'banked_high_water')",
        )
        .execute(&self.pool)
        .await?;
        let pcols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('programs')")
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();
        // The public tape: every trade on the book, ours and everyone else's,
        // with which side was the aggressor. Nothing else we store answers
        // "was this fill ordinary flow, or were we run over", which is the
        // whole question behind a negative markout.
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS tape (
                 trade_id INTEGER NOT NULL,
                 market   TEXT    NOT NULL,
                 ts       INTEGER NOT NULL,
                 price    TEXT    NOT NULL,
                 amount   TEXT    NOT NULL,
                 taker    TEXT,
                 PRIMARY KEY (market, trade_id));
             CREATE INDEX IF NOT EXISTS idx_tape_time ON tape(market, ts);",
        )
        .execute(&self.pool)
        .await
        .context("creating tape")?;

        // The wall-clock instant this program's schedule was last advanced.
        // A glide with a price floor stops its clock while the floor blocks it;
        // see `Db::hold_clock`.
        if !pcols.iter().any(|c| c == "clock_at") {
            sqlx::query("ALTER TABLE programs ADD COLUMN clock_at INTEGER")
                .execute(&self.pool).await?;
        }
        if !pcols.iter().any(|c| c == "banked") {
            sqlx::query("ALTER TABLE programs ADD COLUMN banked TEXT").execute(&self.pool).await?;
        }

        // book_snapshots was keyed on ts alone, so three books sampled in the
        // same millisecond overwrote one another and two of the three were
        // lost. SQLite cannot alter a primary key, so the table is rebuilt --
        // once, guarded by a marker.
        if self.get_setting("snapshot_pk").await?.as_deref() != Some("ts_market") {
            sqlx::raw_sql(
                "ALTER TABLE book_snapshots RENAME TO book_snapshots_old;
                 CREATE TABLE book_snapshots (
                     ts INTEGER NOT NULL, market TEXT NOT NULL,
                     best_bid TEXT, best_ask TEXT, mid TEXT, micro TEXT,
                     bid_qty TEXT, ask_qty TEXT, bid_depth TEXT, ask_depth TEXT,
                     our_bid TEXT, our_ask TEXT, inventory TEXT, target_inv TEXT,
                     PRIMARY KEY (ts, market));
                 INSERT OR IGNORE INTO book_snapshots SELECT
                     ts, market, best_bid, best_ask, mid, micro, bid_qty, ask_qty,
                     bid_depth, ask_depth, our_bid, our_ask, inventory, target_inv
                   FROM book_snapshots_old;
                 DROP TABLE book_snapshots_old;",
            )
            .execute(&self.pool)
            .await
            .context("rebuilding book_snapshots")?;
            self.set_setting("snapshot_pk", "ts_market").await?;
        }
        Ok(())
    }

    /// The mid a fill on `market` at `ts` is scored against.
    ///
    /// Normally the market's own book, nearest in time. A book quoted off a
    /// deeper one is scored against THAT book instead: PRL/USDC's own mid is
    /// whatever stale order was left there last, and judging fills against it
    /// reported -53 bp of edge on quoting that, measured against where PRL was
    /// actually priced, earned +34 bp. The engine records the mapping under
    /// `reference:<market>` and the stablecoin cross under
    /// `reference_cross:<market>` (`<id>:div` or `<id>:mul`); without a cross
    /// snapshot the two stablecoins are taken at par, which is within 30 bp.
    pub async fn scoring_mid(&self, market: &str, ts: i64) -> Option<Decimal> {
        let reference = self.get_setting(&format!("reference:{market}")).await.ok().flatten();
        let Some(reference) = reference else {
            return self.mid_near(market, ts).await;
        };
        let mid = self.mid_near(&reference, ts).await?;
        let cross = self.get_setting(&format!("reference_cross:{market}")).await.ok().flatten();
        let Some(cross) = cross else { return Some(mid) };
        let (id, op) = cross.split_once(':').unwrap_or((cross.as_str(), "div"));
        match self.mid_near(id, ts).await {
            Some(r) if r > Decimal::ZERO && op == "div" => Some(mid / r),
            Some(r) if r > Decimal::ZERO => Some(mid * r),
            _ => Some(mid),
        }
    }

    async fn mid_near(&self, market: &str, ts: i64) -> Option<Decimal> {
        let mid: Option<String> = sqlx::query_scalar(
            "SELECT mid FROM book_snapshots
             WHERE market = ? AND mid IS NOT NULL AND ts BETWEEN ? AND ?
             ORDER BY ABS(ts - ?) LIMIT 1",
        )
        .bind(market)
        .bind(ts - 120_000)
        .bind(ts + 120_000)
        .bind(ts)
        .fetch_optional(&self.pool)
        .await
        .ok()?
        .flatten();
        num::opt_parse(mid.as_deref()).filter(|m| !m.is_zero())
    }

    /// Every arm's five-minute episodes since `since_ms`: how many
    /// dollar-hours rested on it and what the fills in that window paid, net
    /// of fees. The input to `strategy::arms::size_arms`.
    ///
    /// Exposure is the order's full notional for as long as it rested, so a
    /// partly filled order counts its filled part as still resting. That
    /// overstates exposure and understates yield, which is the safe
    /// direction; fills are rare enough (3% of orders were ever touched)
    /// that it is also a small one. Orders without a recorded distance or
    /// range -- adopted strays, or anything placed before the columns
    /// existed and not backfilled -- are not bets on any arm and are left
    /// out.
    pub async fn arm_observations(
        &self,
        since_ms: i64,
        now_ms: i64,
    ) -> Result<Vec<crate::strategy::arms::Observation>> {
        use crate::strategy::arms::{Arm, Bucket, Observation, Regime, Width};
        const H: i64 = MARKOUT_HORIZON_MS;

        let quote_of: std::collections::HashMap<String, String> = self
            .all_settings()
            .await?
            .into_iter()
            .filter_map(|(k, v)| Some((k.strip_prefix("quote_unit:")?.to_string(), v)))
            .collect();

        let mut eps: std::collections::HashMap<(Arm, i64), Observation> = Default::default();
        fn slot<'a>(
            eps: &'a mut std::collections::HashMap<(Arm, i64), Observation>,
            arm: &Arm,
            ep: i64,
        ) -> &'a mut Observation {
            eps.entry((arm.clone(), ep)).or_insert_with(|| Observation {
                arm: arm.clone(),
                episode_ms: ep * H,
                exposure_dh: 0.0,
                net: 0.0,
                fills: 0,
                orders_notional: 0.0,
                orders: 0,
            })
        }

        let orders: Vec<(String, String, f64, f64, f64, String, String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT market, side, bps, recent_move, width_ticks, price, amount, placed_at, closed_at
             FROM orders
             WHERE bps IS NOT NULL AND recent_move IS NOT NULL AND width_ticks IS NOT NULL
               AND exchange_id IS NOT NULL AND state != 'reject'
               AND placed_at <= ? AND (closed_at IS NULL OR closed_at >= ?)",
        )
        .bind(now_ms)
        .bind(since_ms)
        .fetch_all(&self.pool)
        .await?;
        for (market, side, bps, moved, width, price, amount, placed, closed) in orders {
            let Some(side) = Side::parse(&side) else { continue };
            let arm = Arm {
                market,
                side,
                bucket: Bucket::of_bps(bps),
                regime: Regime::of_move(moved),
                width: Width::of_ticks(width),
            };
            let notional = f64::try_from(num::parse(&price) * num::parse(&amount)).unwrap_or(0.0);
            // The window clips both ends: resting before `since` is another
            // regime, and an order still open is counted up to now.
            let start = placed.max(since_ms);
            let end = closed.unwrap_or(now_ms).min(now_ms);
            if end <= start {
                continue;
            }
            let o = slot(&mut eps, &arm, start / H);
            o.orders += 1;
            o.orders_notional += notional;
            let mut t = start;
            while t < end {
                let next = (t / H + 1) * H;
                let seg = next.min(end) - t;
                slot(&mut eps, &arm, t / H).exposure_dh += notional * seg as f64 / 3_600_000.0;
                t = next;
            }
        }

        let fills: Vec<(String, String, f64, f64, f64, String, String, String, Option<String>, i64, String)> =
            sqlx::query_as(
                "SELECT o.market, o.side, o.bps, o.recent_move, o.width_ticks, f.price, f.total, f.fee,
                        f.fee_currency, f.created_at, f.markout
                 FROM fills f JOIN orders o ON o.exchange_id = f.order_id
                 WHERE f.markout IS NOT NULL AND f.created_at >= ? AND f.created_at <= ?
                   AND o.bps IS NOT NULL AND o.recent_move IS NOT NULL AND o.width_ticks IS NOT NULL",
            )
            .bind(since_ms)
            .bind(now_ms)
            .fetch_all(&self.pool)
            .await?;
        for (market, side, bps, moved, width, price, _total, fee, fee_ccy, ts, markout) in fills {
            let Some(side) = Side::parse(&side) else { continue };
            let (price, fee, markout) = (num::parse(&price), num::parse(&fee), num::parse(&markout));
            // Fees are charged in the currency received: quote on a sell,
            // base on a buy. Valued in quote so they subtract from markout.
            let in_quote = match (&fee_ccy, quote_of.get(&market)) {
                (Some(c), Some(q)) => c.eq_ignore_ascii_case(q),
                (Some(c), None) => c.eq_ignore_ascii_case("usdt") || c.eq_ignore_ascii_case("usdc"),
                _ => true,
            };
            let fee_quote = if in_quote { fee } else { fee * price };
            let arm = Arm {
                market,
                side,
                bucket: Bucket::of_bps(bps),
                regime: Regime::of_move(moved),
                width: Width::of_ticks(width),
            };
            let o = slot(&mut eps, &arm, ts / H);
            o.fills += 1;
            o.net += f64::try_from(markout - fee_quote).unwrap_or(0.0);
        }

        let mut out: Vec<Observation> = eps.into_values().collect();
        out.sort_by_key(|o| o.episode_ms);
        Ok(out)
    }

    /// What each (market, side) has paid per unit traded, for `strategy::cycle`.
    ///
    /// The arms already answer "what does this arm yield per dollar-hour of
    /// resting capital", which is the right question for sizing one arm. This
    /// answers a different one: "what does a unit traded on this leg earn",
    /// which is the only figure that can be ADDED across the legs of a cycle.
    /// Both come from the same scored fills; neither replaces the other.
    ///
    /// Every figure is net markout -- the fill judged `MARKOUT_HORIZON_MS`
    /// later against `scoring_mid`, so a referenced book is valued against the
    /// deep book's price through the cross -- less the fee, over the
    /// quote-denominated volume, in basis points.
    ///
    /// The standard error is the one for a sum of `n` independent fills:
    /// `sqrt(n) * sd(per-fill net) / volume`. Fills inside one markout horizon
    /// are NOT independent -- they share a price path, which is why the arms
    /// group them into episodes -- so on a busy leg this understates the error
    /// somewhat. It is used only to keep a thin leg from opening a valve, and
    /// for that an optimistic error bar is the wrong way to be wrong; see the
    /// episode grouping in `arm_observations` for the treatment that does not
    /// have this flaw, and `SE_MULTIPLE` for the margin it is charged at.
    pub async fn cycle_legs(&self, since_ms: i64) -> Result<Vec<crate::strategy::cycle::Leg>> {
        use crate::strategy::cycle::Leg;

        let quote_of: std::collections::HashMap<String, String> = self
            .all_settings()
            .await?
            .into_iter()
            .filter_map(|(k, v)| Some((k.strip_prefix("quote_unit:")?.to_string(), v)))
            .collect();

        let rows: Vec<(String, String, String, String, String, Option<String>, String)> =
            sqlx::query_as(
                "SELECT market, side, price, total, fee, fee_currency, markout FROM fills
                 WHERE markout IS NOT NULL AND liquidity = 'maker' AND created_at >= ?",
            )
            .bind(since_ms)
            .fetch_all(&self.pool)
            .await?;

        // Per leg: the running totals, plus every fill's net so the spread of
        // them can be measured.
        #[allow(clippy::type_complexity)]
        let mut acc: std::collections::HashMap<(String, Side), (Decimal, Vec<(Decimal, Decimal)>)> =
            Default::default();
        for (market, side, price, total, fee, fee_ccy, markout) in rows {
            let Some(side) = Side::parse(&side) else { continue };
            let (price, total, fee, markout) =
                (num::parse(&price), num::parse(&total), num::parse(&fee), num::parse(&markout));
            if total <= Decimal::ZERO {
                continue;
            }
            // Fees are charged in the currency received: quote on a sell, base
            // on a buy. Valued in quote so they subtract from markout.
            let in_quote = match (&fee_ccy, quote_of.get(&market)) {
                (Some(c), Some(q)) => c.eq_ignore_ascii_case(q),
                (Some(c), None) => c.eq_ignore_ascii_case("usdt") || c.eq_ignore_ascii_case("usdc"),
                _ => true,
            };
            let fee_quote = if in_quote { fee } else { fee * price };
            let e = acc.entry((market, side)).or_default();
            e.0 += total;
            e.1.push((markout - fee_quote, total));
        }

        let mut out = Vec::new();
        for ((market, side), (volume, nets)) in acc {
            let n = nets.len();
            if n == 0 || volume <= Decimal::ZERO {
                continue;
            }
            let sum: Decimal = nets.iter().map(|(x, _)| *x).sum();
            let net_bps = sum / volume * Decimal::from(10000);
            // The error on a RATIO, by linearisation, not the error on the
            // numerator alone.
            //
            // `net_bps` is sum(net) / sum(volume), and the two move together --
            // a big fill contributes a big net AND a big volume -- so treating
            // the denominator as fixed and taking sqrt(n)*sd(net)/volume
            // overstates the error badly. Measured live it read 36 bp where a
            // 4,000-sample bootstrap of the same fills read 19. An error bar
            // twice as wide as the truth keeps a cycle shut that the evidence
            // supports, which is the wrong way to be wrong about a valve.
            //
            // So the residual is measured against what each fill's own volume
            // predicted: e_i = net_i - R * volume_i, which is zero for a fill
            // exactly on the ratio however large it is.
            let se_bps = if n > 1 {
                let r = sum / volume;
                let ss: Decimal = nets
                    .iter()
                    .map(|(x, v)| {
                        let e = *x - r * *v;
                        e * e
                    })
                    .sum();
                let scaled = ss * Decimal::from(n as i64) / Decimal::from((n - 1) as i64);
                crate::strategy::cycle::sqrt_for_db(scaled) / volume * Decimal::from(10000)
            } else {
                // One fill is a number, not evidence. An error bar as wide as
                // the estimate keeps it from voting.
                net_bps.abs()
            };
            out.push(Leg { market, side, net_bps, se_bps, volume, n: n as u32 });
        }
        out.sort_by(|a, b| (&a.market, a.side.as_str()).cmp(&(&b.market, b.side.as_str())));
        Ok(out)
    }

    /// Record the placement distance on orders that predate the column, from
    /// the mid nearest their placement, so the arms start with history
    /// rather than cold. Once, guarded by a marker; only the last week, which
    /// is more than the evidence half-life keeps alive.
    pub async fn backfill_order_distance(&self) -> Result<u64> {
        const MARKER: &str = "order_bps_backfill";
        if self.get_setting(MARKER).await?.is_some() {
            return Ok(0);
        }
        let rows: Vec<(String, String, String, String, i64)> = sqlx::query_as(
            "SELECT client_id, market, side, price, placed_at FROM orders
             WHERE bps IS NULL AND price IS NOT NULL AND exchange_id IS NOT NULL
               AND placed_at >= ?",
        )
        .bind(now_ms() - 7 * 86_400_000)
        .fetch_all(&self.pool)
        .await?;
        let mut done = 0u64;
        for (client_id, market, side, price, placed) in rows {
            let (Some(mid), Some(side)) = (self.scoring_mid(&market, placed).await, Side::parse(&side))
            else {
                continue;
            };
            let price = num::parse(&price);
            let away = match side {
                Side::Buy => mid - price,
                Side::Sell => price - mid,
            };
            let bps = f64::try_from(away / mid * Decimal::from(10_000)).unwrap_or(0.0);
            sqlx::query("UPDATE orders SET bps = ? WHERE client_id = ?")
                .bind(bps)
                .bind(client_id)
                .execute(&self.pool)
                .await?;
            done += 1;
        }
        self.set_setting(MARKER, &now_ms().to_string()).await?;
        Ok(done)
    }

    /// Record the five-minute range at placement on orders that predate the
    /// column, from the book snapshots of the market whose price the order
    /// was quoted off, so the arms start with their regimes populated rather
    /// than every cell empty. Once, guarded by a marker; only the last
    /// `max_age_ms`, which is what the arms read.
    ///
    /// The engine reads the range off the reference for a referenced book
    /// and off the local microprice otherwise; here it is the reference's
    /// mid, or the local mid. The stablecoin cross a referenced book divides
    /// by ranges a tenth of a percent and is left out of the range, and the
    /// microprice sits within a fraction of a tick of the mid. Both are well
    /// inside the 2% regime edge.
    pub async fn backfill_order_move(&self, window_ms: i64, max_age_ms: i64) -> Result<u64> {
        const MARKER: &str = "order_move_backfill";
        if self.get_setting(MARKER).await?.is_some() {
            return Ok(0);
        }
        let since = now_ms() - max_age_ms;
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT client_id, market, placed_at FROM orders
             WHERE recent_move IS NULL AND bps IS NOT NULL AND exchange_id IS NOT NULL
               AND placed_at >= ?",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let settings = self.all_settings().await?;
        let anchor_of = |market: &str| -> String {
            settings
                .iter()
                .find(|(k, _)| k.strip_prefix("reference:") == Some(market))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| market.to_string())
        };
        // One series per anchor market, then a binary search per order.
        let mut series: std::collections::HashMap<String, Vec<(i64, f64)>> = Default::default();
        for anchor in rows.iter().map(|(_, m, _)| anchor_of(m)).collect::<std::collections::HashSet<_>>() {
            let mids: Vec<(i64, String)> = sqlx::query_as(
                "SELECT ts, mid FROM book_snapshots
                 WHERE market = ? AND mid IS NOT NULL AND ts >= ? ORDER BY ts",
            )
            .bind(&anchor)
            .bind(since - window_ms)
            .fetch_all(&self.pool)
            .await?;
            series.insert(
                anchor,
                mids.into_iter()
                    .filter_map(|(ts, m)| Some((ts, f64::try_from(num::parse(&m)).ok().filter(|m| *m > 0.0)?)))
                    .collect(),
            );
        }
        let mut done = 0u64;
        for (client_id, market, placed) in rows {
            let Some(s) = series.get(&anchor_of(&market)) else { continue };
            let from = s.partition_point(|(ts, _)| *ts < placed - window_ms);
            let to = s.partition_point(|(ts, _)| *ts <= placed);
            let window = &s[from..to];
            if window.len() < 2 {
                continue;
            }
            let lo = window.iter().map(|(_, m)| *m).fold(f64::INFINITY, f64::min);
            let hi = window.iter().map(|(_, m)| *m).fold(0.0, f64::max);
            let moved = if lo > 0.0 { (hi - lo) / lo } else { 0.0 };
            sqlx::query("UPDATE orders SET recent_move = ? WHERE client_id = ?")
                .bind(moved)
                .bind(client_id)
                .execute(&self.pool)
                .await?;
            done += 1;
        }
        self.set_setting(MARKER, &now_ms().to_string()).await?;
        Ok(done)
    }

    /// Record the stranger book's width at placement on orders that predate
    /// the column, from the snapshot nearest their placement, so the arms'
    /// width cells start from the same history as their regimes. Once,
    /// guarded by a marker; only what the arms still read.
    ///
    /// Snapshots are of the book with our own size stripped, which is what
    /// `quote::plan` measures live. The tick is not stored with them, so it
    /// is read off the prices themselves: the most decimal places any of the
    /// market's snapshot prices carries in the window. Prices are stored
    /// normalised ("1.2", not "1.20"), so one price can understate the tick
    /// and a window of hundreds does not.
    pub async fn backfill_order_width(&self, max_age_ms: i64) -> Result<u64> {
        const MARKER: &str = "order_width_backfill";
        if self.get_setting(MARKER).await?.is_some() {
            return Ok(0);
        }
        const NEAR_MS: i64 = 120_000;
        let since = now_ms() - max_age_ms;
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT client_id, market, placed_at FROM orders
             WHERE width_ticks IS NULL AND bps IS NOT NULL AND exchange_id IS NOT NULL
               AND placed_at >= ?",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let mut series: std::collections::HashMap<String, (Decimal, Vec<(i64, Decimal, Decimal)>)> =
            Default::default();
        for market in rows.iter().map(|(_, m, _)| m.clone()).collect::<std::collections::HashSet<_>>() {
            let snaps: Vec<(i64, String, String)> = sqlx::query_as(
                "SELECT ts, best_bid, best_ask FROM book_snapshots
                 WHERE market = ? AND best_bid IS NOT NULL AND best_ask IS NOT NULL AND ts >= ?
                 ORDER BY ts",
            )
            .bind(&market)
            .bind(since - NEAR_MS)
            .fetch_all(&self.pool)
            .await?;
            let snaps: Vec<(i64, Decimal, Decimal)> =
                snaps.into_iter().map(|(ts, b, a)| (ts, num::parse(&b), num::parse(&a))).collect();
            let scale = snaps.iter().map(|(_, b, a)| b.scale().max(a.scale())).max().unwrap_or(0);
            series.insert(market, (num::tick(scale), snaps));
        }
        let mut done = 0u64;
        for (client_id, market, placed) in rows {
            let Some((tick, s)) = series.get(&market) else { continue };
            let i = s.partition_point(|(ts, _, _)| *ts < placed);
            let nearest = [i.checked_sub(1), Some(i)]
                .into_iter()
                .flatten()
                .filter_map(|j| s.get(j))
                .filter(|(ts, _, _)| (ts - placed).abs() <= NEAR_MS)
                .min_by_key(|(ts, _, _)| (ts - placed).abs());
            let Some((_, bid, ask)) = nearest else { continue };
            let width = f64::try_from(((ask - bid) / tick).round_dp(2)).unwrap_or(1.0);
            sqlx::query("UPDATE orders SET width_ticks = ? WHERE client_id = ?")
                .bind(width)
                .bind(client_id)
                .execute(&self.pool)
                .await?;
            done += 1;
        }
        self.set_setting(MARKER, &now_ms().to_string()).await?;
        Ok(done)
    }

    /// Fill in `edge` for rows written before it was stored, using the nearest
    /// book snapshot. Runs once; afterwards every fill carries its own edge.
    pub async fn backfill_edges(&self) -> Result<u64> {
        self.settle_edges(false).await
    }

    /// Recompute every fill's edge against the snapshot nearest the fill.
    ///
    /// Run once after the mid used for settlement was corrected: earlier rows
    /// were scored against the book at ingestion rather than at the fill, which
    /// in a trending market is a different price entirely.
    pub async fn resettle_all_edges(&self) -> Result<u64> {
        self.settle_edges(true).await
    }

    async fn settle_edges(&self, all: bool) -> Result<u64> {
        let sql = if all {
            "SELECT trade_id, market, side, price, amount, created_at FROM fills"
        } else {
            "SELECT trade_id, market, side, price, amount, created_at FROM fills WHERE edge IS NULL"
        };
        let rows: Vec<(i64, String, String, String, String, i64)> = sqlx::query_as(sql)
        .fetch_all(&self.pool)
        .await?;
        let mut done = 0u64;
        for (id, market, side, price, amount, ts) in rows {
            let (Some(mid), Some(side)) = (self.scoring_mid(&market, ts).await, Side::parse(&side))
            else {
                continue;
            };
            let (p, a) = (num::parse(&price), num::parse(&amount));
            let edge = match side {
                Side::Sell => (p - mid) * a,
                Side::Buy => (mid - p) * a,
            };
            sqlx::query("UPDATE fills SET mid_at_fill = ?, edge = ? WHERE trade_id = ?")
                .bind(num::to_str(mid))
                .bind(num::to_str(edge))
                .bind(id)
                .execute(&self.pool)
                .await?;
            done += 1;
        }
        Ok(done)
    }

    /// Re-score every markout on books that are judged against a reference.
    ///
    /// Markouts settled before `reference:<market>` existed were scored
    /// against the thin book's own stale mid, and unlike `edge` they were
    /// never re-settled: on the copy this was built against, prlusdc's fills
    /// read -1.7 ‰/$h at 200-400 bp on the stored markouts and +18 against
    /// the reference. The arms size from these numbers, so the stored ones
    /// must be the honest ones. Once, guarded by a marker.
    pub async fn resettle_reference_markouts(&self) -> Result<u64> {
        const MARKER: &str = "markout_settlement";
        if self.get_setting(MARKER).await?.as_deref() == Some("2") {
            return Ok(0);
        }
        let referenced: Vec<String> = self
            .all_settings()
            .await?
            .into_iter()
            .filter_map(|(k, _)| k.strip_prefix("reference:").map(str::to_string))
            .collect();
        let mut done = 0u64;
        for market in referenced {
            let rows: Vec<(i64, String, String, String, i64)> = sqlx::query_as(
                "SELECT trade_id, side, price, amount, created_at FROM fills
                 WHERE market = ? AND markout IS NOT NULL",
            )
            .bind(&market)
            .fetch_all(&self.pool)
            .await?;
            for (id, side, price, amount, ts) in rows {
                let (Some(mid), Some(side)) =
                    (self.scoring_mid(&market, ts + MARKOUT_HORIZON_MS).await, Side::parse(&side))
                else {
                    continue;
                };
                let (p, a) = (num::parse(&price), num::parse(&amount));
                let markout = match side {
                    Side::Sell => (p - mid) * a,
                    Side::Buy => (mid - p) * a,
                };
                sqlx::query("UPDATE fills SET markout = ? WHERE trade_id = ?")
                    .bind(num::to_str(markout))
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
                done += 1;
            }
        }
        self.set_setting(MARKER, "2").await?;
        Ok(done)
    }

    /// Score fills against the mid a few minutes AFTER they happened.
    ///
    /// `edge` asks whether our price was better than the mid at that instant.
    /// It always is -- we quote outside the mid -- so it can only ever flatter
    /// us. It cannot see adverse selection: the fill that happens precisely
    /// because the price is about to run through it books a win and then loses
    /// money. Measured live, the same 1,408 fills were a clear profit at the
    /// moment they happened and a loss a minute later.
    ///
    /// Markout is the honest version. Settled lazily, because the answer does
    /// not exist until `MARKOUT_HORIZON_MS` after the fill.
    pub async fn settle_markouts(&self) -> Result<u64> {
        let cutoff = now_ms() - MARKOUT_HORIZON_MS;
        let rows: Vec<(i64, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT trade_id, market, side, price, amount, created_at FROM fills
             WHERE markout IS NULL AND created_at <= ? ORDER BY created_at LIMIT 500",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;

        let mut done = 0u64;
        for (id, market, side, price, amount, ts) in rows {
            let want = ts + MARKOUT_HORIZON_MS;
            let (Some(mid), Some(side)) = (self.scoring_mid(&market, want).await, Side::parse(&side))
            else {
                continue;
            };
            let (p, a) = (num::parse(&price), num::parse(&amount));
            // Sold above where the price went, or bought below it, is a win.
            let markout = match side {
                Side::Sell => (p - mid) * a,
                Side::Buy => (mid - p) * a,
            };
            sqlx::query("UPDATE fills SET markout = ? WHERE trade_id = ?")
                .bind(num::to_str(markout))
                .bind(id)
                .execute(&self.pool)
                .await?;
            done += 1;
        }
        Ok(done)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ---------- settings ----------

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let v: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(v.map(|(s,)| s))
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings(key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn all_settings(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(&self.pool)
            .await?)
    }

    /// A setting parsed as a decimal, falling back to `default` when unset or
    /// unparseable. Overrides from `ctl set` land here.
    pub async fn setting_decimal(&self, key: &str, default: Decimal) -> Decimal {
        match self.get_setting(key).await {
            Ok(Some(v)) => v.parse().unwrap_or(default),
            _ => default,
        }
    }

    pub async fn setting_bool(&self, key: &str, default: bool) -> bool {
        match self.get_setting(key).await {
            Ok(Some(v)) => matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            _ => default,
        }
    }

    // ---------- programs ----------

    pub async fn active_program(&self, market: &str) -> Result<Option<Program>> {
        let row: Option<ProgramRow> = sqlx::query_as(
            "SELECT id, market, kind, state, start_qty, target_qty, ratio, banked, limit_price,
                    started_at, ends_at, note
             FROM programs WHERE market = ? AND state = 'active'
             ORDER BY id DESC LIMIT 1",
        )
        .bind(market)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Program::try_from).transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_program(
        &self,
        market: &str,
        kind: ProgramKind,
        start_qty: Decimal,
        target_qty: Decimal,
        ratio: Option<Decimal>,
        limit_price: Option<Decimal>,
        ends_at: Option<i64>,
        note: Option<&str>,
    ) -> Result<i64> {
        let now = now_ms();
        // Only one program drives the book at a time; supersede the old one.
        sqlx::query("UPDATE programs SET state = 'cancelled', updated_at = ? WHERE market = ? AND state = 'active'")
            .bind(now)
            .bind(market)
            .execute(&self.pool)
            .await?;
        let id = sqlx::query(
            "INSERT INTO programs(market, kind, state, start_qty, target_qty, ratio, limit_price,
                                  started_at, ends_at, created_at, updated_at, note)
             VALUES (?, ?, 'active', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(market)
        .bind(kind.as_str())
        .bind(num::to_str(start_qty))
        .bind(num::to_str(target_qty))
        .bind(ratio.map(num::to_str))
        .bind(limit_price.map(num::to_str))
        .bind(now)
        .bind(ends_at)
        .bind(now)
        .bind(now)
        .bind(note)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();
        Ok(id)
    }

    /// Add trading profit to the pile held in the base asset.
    ///
    /// Called with the edge earned since the last call, in quote terms. Only
    /// profit is banked; the principal stays under the ratio, so the
    /// rebalancing machine keeps running at full size while the winnings
    /// accumulate as base.
    pub async fn bank_profit(&self, program_id: i64, amount: Decimal) -> Result<()> {
        if amount <= Decimal::ZERO {
            return Ok(());
        }
        sqlx::query(
            "UPDATE programs SET banked = CAST(
                 COALESCE(CAST(banked AS REAL), 0) + ? AS TEXT), updated_at = ?
             WHERE id = ?",
        )
        .bind(amount.to_string().parse::<f64>().unwrap_or(0.0))
        .bind(now_ms())
        .bind(program_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Move a program written under a market id to its base-asset key.
    ///
    /// Programs used to be per market. One inventory target now governs every
    /// book trading that asset, so the key is the asset -- and a bot already
    /// running must not lose its program to that rename.
    pub async fn migrate_program_key(&self, from_market: &str, to_base: &str) -> Result<()> {
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM programs WHERE market = ? AND state = 'active'")
                .bind(to_base)
                .fetch_optional(&self.pool)
                .await?;
        if existing.is_some() {
            return Ok(());
        }
        let moved = sqlx::query(
            "UPDATE programs SET market = ?, updated_at = ?
             WHERE market = ? AND state = 'active'",
        )
        .bind(to_base)
        .bind(now_ms())
        .bind(from_market)
        .execute(&self.pool)
        .await?
        .rows_affected();

        // Only carry the legacy un-keyed baseline across if a program actually
        // moved. Copying it for every market handed USDC the PRL baseline and
        // told the bot it had started with its PRL quantity in USDC.
        if moved == 0 {
            return Ok(());
        }
        for k in ["baseline_qty", "baseline_cost"] {
            if let Some(v) = self.get_setting(k).await? {
                if self.get_setting(&format!("{k}:{to_base}")).await?.is_none() {
                    self.set_setting(&format!("{k}:{to_base}"), &v).await?;
                }
            }
        }
        Ok(())
    }

    pub async fn set_program_state(&self, id: i64, state: &str) -> Result<()> {
        sqlx::query("UPDATE programs SET state = ?, updated_at = ? WHERE id = ?")
            .bind(state)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- orders ----------

    /// `bps` is the order's distance from fair value at placement and
    /// `recent_move` the five-minute range at placement; `None` for an
    /// order we did not place ourselves and cannot attribute to an arm.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_placement(
        &self,
        client_id: &str,
        program_id: Option<i64>,
        market: &str,
        side: Side,
        price: Decimal,
        amount: Decimal,
        level: i64,
        bps: Option<Decimal>,
        recent_move: Option<Decimal>,
        width_ticks: Option<Decimal>,
    ) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO orders(client_id, program_id, market, side, ord_type, price, amount,
                                state, level, placed_at, updated_at, bps, recent_move, width_ticks)
             VALUES (?, ?, ?, ?, 'limit', ?, ?, 'pending', ?, ?, ?, ?, ?, ?)
             ON CONFLICT(client_id) DO NOTHING",
        )
        .bind(client_id)
        .bind(program_id)
        .bind(market)
        .bind(side.as_str())
        .bind(num::to_str(price))
        .bind(num::to_str(amount))
        .bind(level)
        .bind(now)
        .bind(now)
        .bind(bps.and_then(|b| f64::try_from(b).ok()))
        .bind(recent_move.and_then(|m| f64::try_from(m).ok()))
        .bind(width_ticks.and_then(|w| f64::try_from(w).ok()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn attach_exchange_id(&self, client_id: &str, exchange_id: i64, state: &str) -> Result<()> {
        sqlx::query(
            "UPDATE orders SET exchange_id = ?, state = ?, updated_at = ? WHERE client_id = ?",
        )
        .bind(exchange_id)
        .bind(state)
        .bind(now_ms())
        .bind(client_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn fail_placement(&self, client_id: &str, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE orders SET state = 'reject', error = ?, updated_at = ?, closed_at = ?
             WHERE client_id = ?",
        )
        .bind(error)
        .bind(now_ms())
        .bind(now_ms())
        .bind(client_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace our view of resting orders with what the exchange just told us.
    /// The exchange is the source of truth; anything we thought was open but is
    /// not in `open` gets closed out locally.
    pub async fn sync_open_orders(
        &self,
        market: &str,
        open: &[crate::exchange::Order],
    ) -> Result<()> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;

        for o in open {
            sqlx::query(
                "UPDATE orders SET state = ?, filled_amount = ?, avg_price = ?, updated_at = ?
                 WHERE exchange_id = ?",
            )
            .bind(&o.state)
            .bind(num::to_str(o.filled_amount))
            .bind(o.avg_price.map(num::to_str))
            .bind(now)
            .bind(o.id)
            .execute(&mut *tx)
            .await?;
        }

        // Anything we still believe is resting but the exchange no longer lists
        // has either filled or been cancelled elsewhere; close it locally.
        //
        // Done as a diff in Rust rather than a generated `NOT IN (...)` clause:
        // the id list is unbounded, and building SQL by string concatenation is
        // a habit worth not having in a process that moves money.
        let known: Vec<i64> = sqlx::query_scalar(
            "SELECT exchange_id FROM orders
             WHERE market = ? AND state IN ('wait','pending') AND exchange_id IS NOT NULL",
        )
        .bind(market)
        .fetch_all(&mut *tx)
        .await?;

        let still_open: std::collections::HashSet<i64> = open.iter().map(|o| o.id).collect();
        for id in known.into_iter().filter(|id| !still_open.contains(id)) {
            sqlx::query(
                "UPDATE orders SET state = 'done', updated_at = ?, closed_at = ?
                 WHERE exchange_id = ?",
            )
            .bind(now)
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn live_orders(&self, market: &str) -> Result<Vec<LiveOrder>> {
        let rows: Vec<OrderRow> = sqlx::query_as(
            "SELECT client_id, exchange_id, program_id, market, side, price, amount,
                    filled_amount, state, level, placed_at
             FROM orders
             WHERE market = ? AND state IN ('wait','pending') AND exchange_id IS NOT NULL",
        )
        .bind(market)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(LiveOrder {
                    exchange_id: r.exchange_id?,
                    side: Side::parse(&r.side)?,
                    price: num::opt_parse(r.price.as_deref())?,
                    amount: num::parse(&r.amount),
                    filled: num::parse(&r.filled_amount),
                    level: r.level.unwrap_or(0),
                    placed_at: r.placed_at,
                    client_id: r.client_id,
                })
            })
            .collect())
    }

    /// Our record of an order, whatever state we last put it in.
    ///
    /// Deliberately not limited to live states. We mark an order cancelled the
    /// moment the API returns, but the exchange keeps listing it for a moment
    /// afterwards -- so a live-only lookup fails to recognise our own order and
    /// reports it as a stranger's.
    pub async fn order_by_exchange_id(&self, exchange_id: i64) -> Result<Option<OrderRow>> {
        Ok(sqlx::query_as(
            "SELECT client_id, exchange_id, program_id, market, side, price, amount,
                    filled_amount, state, level, placed_at
             FROM orders WHERE exchange_id = ?",
        )
        .bind(exchange_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Put an order back on the live books after the exchange showed it is
    /// still resting.
    pub async fn revive_order(&self, exchange_id: i64, state: &str) -> Result<()> {
        sqlx::query(
            "UPDATE orders SET state = ?, closed_at = NULL, updated_at = ? WHERE exchange_id = ?",
        )
        .bind(state)
        .bind(now_ms())
        .bind(exchange_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_cancelled(&self, exchange_id: i64) -> Result<()> {
        let now = now_ms();
        sqlx::query("UPDATE orders SET state = 'cancel', updated_at = ?, closed_at = ? WHERE exchange_id = ?")
            .bind(now)
            .bind(now)
            .bind(exchange_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- fills ----------

    /// Insert fills we have not seen before and carry the position forward.
    ///
    /// Returns how many were new, the resulting position, and the net signed
    /// movement in BOTH currencies.
    ///
    /// Both, because a currency can be one market's base and another's quote:
    /// USDC is the base of USDC/USDT and the quote of PRL/USDC. Reporting only
    /// the base leaves every PRL/USDC fill moving the USDC balance with nothing
    /// to explain it, which then reads as a deposit or withdrawal.
    pub async fn ingest_fills(
        &self,
        market: &str,
        trades: &[crate::exchange::Trade],
        mid: Option<Decimal>,
    ) -> Result<(usize, Position, Decimal, Decimal)> {
        let mut pos = self.position(market).await?;
        let mut inserted = 0usize;
        let mut net_qty = Decimal::ZERO;
        let mut net_quote = Decimal::ZERO;
        let mut tx = self.pool.begin().await?;

        // Oldest first so the running position is built in chronological order.
        let mut sorted: Vec<_> = trades.iter().collect();
        sorted.sort_by_key(|t| (t.created_at, t.id));

        let cutover = self.fills_since_ms(market).await;
        for t in sorted {
            let Some(side) = t.our_side() else { continue };
            if t.created_at * 1000 < cutover {
                continue;
            }
            let exists: Option<(i64,)> = sqlx::query_as("SELECT trade_id FROM fills WHERE trade_id = ?")
                .bind(t.id)
                .fetch_optional(&mut *tx)
                .await?;
            if exists.is_some() {
                continue;
            }

            let fee = t.fee.unwrap_or_default();
            pos.apply(side, t.price, t.amount, fee);
            net_qty += match side {
                Side::Buy => t.amount,
                Side::Sell => -t.amount,
            };
            // A buy spends quote, a sell earns it. Fees charged in the quote
            // currency come out of it either way.
            net_quote += match side {
                Side::Buy => -t.total,
                Side::Sell => t.total,
            };
            // Fees are taken from whatever was received: base on a buy, quote
            // on a sell. Verified against live fills -- a buy paid 0.0801 PRL,
            // a sell paid 0.2422 USDT.
            let fee = t.fee.unwrap_or_default();
            match side {
                Side::Buy => net_qty -= fee,
                Side::Sell => net_quote -= fee,
            }

            let program_id: Option<i64> = sqlx::query_scalar(
                "SELECT program_id FROM orders WHERE exchange_id = ?",
            )
            .bind(t.order_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();

            // Edge is settled here, once, and stored with the fill -- but
            // against the mid from WHEN THE FILL HAPPENED, not from when we
            // noticed it.
            //
            // Those differ by up to a loop, and in a fast market that is the
            // whole measurement. During a 13% rally our resting asks were
            // lifted at 1.11 and scored against a 1.13 mid recorded seconds
            // later, booking -4.37 of "edge" on a trade that did exactly what
            // it was supposed to. We snapshot the book every loop, so a
            // snapshot within a few seconds of any fill exists; the current
            // book is only a fallback.
            let fill_ms = t.created_at * 1000;
            let mid = self.scoring_mid(market, fill_ms).await.or(mid);

            let edge = match (mid, side) {
                (Some(m), Side::Sell) if !m.is_zero() => Some((t.price - m) * t.amount),
                (Some(m), Side::Buy) if !m.is_zero() => Some((m - t.price) * t.amount),
                _ => None,
            };

            sqlx::query(
                "INSERT INTO fills(trade_id, order_id, client_id, program_id, market, side, price,
                                   amount, total, fee, fee_currency, liquidity, created_at,
                                   pos_after, cost_after, realized, mid_at_fill, edge)
                 VALUES (?, ?, (SELECT client_id FROM orders WHERE exchange_id = ?), ?, ?, ?, ?,
                         ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(t.id)
            .bind(t.order_id)
            .bind(t.order_id)
            .bind(program_id)
            .bind(market)
            .bind(side.as_str())
            .bind(num::to_str(t.price))
            .bind(num::to_str(t.amount))
            .bind(num::to_str(t.total))
            .bind(num::to_str(fee))
            .bind(t.fee_currency.as_deref())
            .bind(t.liquidity())
            .bind(t.created_at * 1000)
            .bind(num::to_str(pos.qty))
            .bind(num::to_str(pos.cost))
            .bind(num::to_str(pos.realized))
            .bind(mid.map(num::to_str))
            .bind(edge.map(num::to_str))
            .execute(&mut *tx)
            .await?;
            inserted += 1;
        }

        tx.commit().await?;
        Ok((inserted, pos, net_qty, net_quote))
    }

    /// Restart a program's glide from where inventory is now, keeping its
    /// original duration.
    ///
    /// Used when coins arrive or leave by means other than our own trading.
    /// Re-derive a learned program: a new ratio, and a fresh glide from
    /// `start_qty` to `ends_at`. One write, so a crash between the two
    /// cannot leave a new ratio on an old glide.
    pub async fn set_program_glide(
        &self,
        id: i64,
        ratio: Decimal,
        start_qty: Decimal,
        ends_at: i64,
    ) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE programs SET ratio = ?, start_qty = ?, started_at = ?, ends_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(num::to_str(ratio))
        .bind(num::to_str(start_qty))
        .bind(now)
        .bind(ends_at)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The glide interpolates from a fixed `start_qty`, so without this a
    /// deposit makes the whole new gap appear instantly and the bot works it
    /// off as fast as the caps allow, which is the opposite of what a gradual
    /// program is for.
    pub async fn reanchor_program(&self, id: i64, start_qty: Decimal) -> Result<()> {
        let now = now_ms();
        let row: Option<(i64, Option<i64>)> =
            sqlx::query_as("SELECT started_at, ends_at FROM programs WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((started_at, ends_at)) = row else { return Ok(()) };
        // Preserve the configured duration, so a deposit buys a fresh full
        // glide rather than whatever happened to be left of the old one.
        let new_ends = ends_at.map(|e| now + (e - started_at).max(0));

        sqlx::query(
            "UPDATE programs SET start_qty = ?, started_at = ?, ends_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(num::to_str(start_qty))
        .bind(now)
        .bind(new_ends)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The position the bot inherited when it first started.
    ///
    /// Without this, PnL is nonsense for an account that already held coins:
    /// the first sell would look like opening a short rather than reducing a
    /// long. Seeding qty and cost at the moment the bot took over makes every
    /// number downstream answer the question that actually matters -- "what has
    /// the bot added since it started?" -- rather than mixing in trades a human
    /// made days earlier, whose true cost basis we cannot reconstruct anyway.
    pub async fn baseline(&self, key: &str) -> Position {
        Position {
            qty: self.setting_decimal(&format!("baseline_qty:{key}"), Decimal::ZERO).await,
            cost: self.setting_decimal(&format!("baseline_cost:{key}"), Decimal::ZERO).await,
            realized: Decimal::ZERO,
            fees: Decimal::ZERO,
        }
    }

    /// Record the inherited position, once. Returns true if it was written.
    pub async fn seed_baseline(&self, key: &str, qty: Decimal, mark: Decimal) -> Result<bool> {
        if self.get_setting(&format!("baseline_qty:{key}")).await?.is_some() {
            return Ok(false);
        }
        self.set_setting(&format!("baseline_qty:{key}"), &num::to_str(qty)).await?;
        self.set_setting(&format!("baseline_cost:{key}"), &num::to_str(qty * mark)).await?;
        Ok(true)
    }

    /// Fix the point before which trades are not ours, once per market.
    ///
    /// Per market, and write-once. It used to be a single global setting
    /// written by a per-asset routine, so adding a second asset dragged the
    /// cutover forward for every market at once -- which would silently skip
    /// every fill between the last one recorded and that moment.
    pub async fn ensure_fill_cutover(&self, market: &str) -> Result<()> {
        let key = format!("fills_since_ms:{market}");
        if self.get_setting(&key).await?.is_some() {
            return Ok(());
        }
        // Inherit the legacy global value if there is one, so an upgrade does
        // not suddenly treat old trades as ours.
        let v = match self.get_setting("fills_since_ms").await? {
            Some(v) => v,
            None => now_ms().to_string(),
        };
        self.set_setting(&key, &v).await
    }

    /// Record what the whole account was worth when the scoreboard started,
    /// once per epoch.
    ///
    /// Without a starting value there is nothing to measure the account against,
    /// and "profit" collapses into whichever number is easiest to print.
    /// `reset_analytics_epoch` clears the value, and the engine calls this on
    /// its next tick with the live balances, so a fresh scoreboard measures
    /// the account from the moment it started rather than from launch.
    pub async fn seed_account_value(&self, live: Decimal) -> Result<bool> {
        if self.get_setting("account_start_value").await?.is_some() {
            return Ok(false);
        }
        // With no epoch drawn this is the launch value, reconstructed from
        // what was recorded at the time -- the inherited position's cost plus
        // the earliest balances -- because today's value would silently erase
        // every price move since. Once a line has been drawn the account IS
        // being measured from now, and the live value is the right one.
        let (value, ts) = if self.analytics_epoch_ms().await > 0 {
            (live, now_ms())
        } else {
            self.reconstruct_start().await?.unwrap_or((live, now_ms()))
        };
        self.set_setting("account_start_value", &num::to_str(value)).await?;
        self.set_setting("account_start_ts", &ts.to_string()).await?;
        Ok(true)
    }

    /// Rebuild the account's launch value from the earliest records.
    async fn reconstruct_start(&self) -> Result<Option<(Decimal, i64)>> {
        let first_ts: Option<i64> =
            sqlx::query_scalar("SELECT MIN(ts) FROM balances").fetch_one(&self.pool).await?;
        let Some(first_ts) = first_ts else { return Ok(None) };

        // The base asset's cost at seeding is exactly qty x mark at that moment.
        let mut total = Decimal::ZERO;
        let mut bases: Vec<String> = Vec::new();
        for (k, v) in self.all_settings().await? {
            if let Some(base) = k.strip_prefix("baseline_cost:") {
                total += num::parse(&v);
                bases.push(base.to_string());
            }
        }
        if bases.is_empty() {
            return Ok(None);
        }

        // Everything else held at that first snapshot, valued at par. The quote
        // currencies here are stablecoins, so par is right to under a percent.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT currency, available, locked FROM balances WHERE ts = ?",
        )
        .bind(first_ts)
        .fetch_all(&self.pool)
        .await?;
        for (ccy, avail, locked) in rows {
            if bases.iter().any(|b| b == &ccy) {
                continue; // already counted at its marked cost
            }
            total += num::parse(&avail) + num::parse(&locked);
        }
        Ok(Some((total, first_ts)))
    }

    pub async fn account_start_value(&self) -> Option<Decimal> {
        self.get_setting("account_start_value").await.ok().flatten().and_then(|v| v.parse().ok())
    }

    /// When the account's starting value was taken.
    pub async fn account_start_ts(&self) -> Option<i64> {
        self.get_setting("account_start_ts").await.ok().flatten().and_then(|v| v.parse().ok())
    }

    /// Money put in or taken out is not profit. Accumulated in quote terms so
    /// the account's change can be corrected for it.
    pub async fn add_external_flow(&self, value: Decimal) -> Result<()> {
        let now = self.setting_decimal("account_net_deposits", Decimal::ZERO).await;
        self.set_setting("account_net_deposits", &num::to_str(now + value)).await
    }

    pub async fn net_deposits(&self) -> Decimal {
        self.setting_decimal("account_net_deposits", Decimal::ZERO).await
    }

    /// The base asset programs are filed under, as the engine recorded it.
    ///
    /// Read from what the exchange said, not guessed by chopping a suffix off
    /// the market id. A hard-coded list of quote currencies is wrong the first
    /// time a market uses one that is not on it, and wrong silently.
    pub async fn program_key(&self, fallback: &str) -> String {
        self.get_setting("base_unit").await.ok().flatten().unwrap_or_else(|| fallback.to_string())
    }

    /// Every currency the configured markets quote against, as recorded.
    pub async fn quote_currencies(&self) -> Vec<String> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM settings WHERE key LIKE 'quote_unit:%'")
                .fetch_all(&self.pool)
                .await
                .unwrap_or_default();
        let mut out: Vec<String> = rows.into_iter().map(|(_, v)| v).collect();
        out.sort();
        out.dedup();
        out
    }

    /// The most recent mid recorded for any market with this base asset.
    pub async fn last_mid(&self, base: &str) -> Option<Decimal> {
        let v: Option<String> = sqlx::query_scalar(
            "SELECT mid FROM book_snapshots
             WHERE market LIKE ? AND mid IS NOT NULL ORDER BY ts DESC LIMIT 1",
        )
        .bind(format!("{base}%"))
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();
        num::opt_parse(v.as_deref())
    }

    /// Record what a book traded and how wide it is, for the report's "share
    /// of the market" and "capture" lines. Smoothed, so one quiet window does
    /// not swing them. Nothing in the quoting reads these: capital follows the
    /// arms, not the market's volume.
    pub async fn record_market_stats(
        &self,
        market: &str,
        volume_quote: Decimal,
        spread_frac: Decimal,
        alpha: Decimal,
    ) -> Result<()> {
        let prev_v = self.setting_decimal(&format!("vol:{market}"), volume_quote).await;
        let prev_s = self.setting_decimal(&format!("spread:{market}"), spread_frac).await;
        let v = prev_v + (volume_quote - prev_v) * alpha;
        let sp = prev_s + (spread_frac - prev_s) * alpha;
        self.set_setting(&format!("vol:{market}"), &num::to_str(v)).await?;
        self.set_setting(&format!("spread:{market}"), &num::to_str(sp)).await
    }

    pub async fn market_stats(&self, market: &str) -> (Decimal, Decimal) {
        (
            self.setting_decimal(&format!("vol:{market}"), Decimal::ZERO).await,
            self.setting_decimal(&format!("spread:{market}"), Decimal::ZERO).await,
        )
    }

    /// Profit earned on an asset's books that has not yet been banked.
    ///
    /// Tracked by a high-water mark rather than by flagging rows, so a fill
    /// cannot be banked twice even if it is re-read.
    ///
    /// Scored by MARKOUT, not by edge. Edge is distance from the mid at the
    /// instant of the fill, which a maker beats by construction: on the
    /// first deployment, the first five hours read four times as much edge
    /// as markout. Banking edge set aside profit that was
    /// never made, and every dollar of it came out of the book. A fill too
    /// young to have a markout waits for the next pass.
    ///
    /// Only fills from `bank_since_ms:<base>` on. Without a start, a database
    /// carried across accounts banked the old account's history -- a net loss,
    /// which the new book would have had to earn back before banking a cent.
    pub async fn unbanked_edge(&self, base: &str, markets: &[String]) -> Result<Decimal> {
        let since = self
            .get_setting(&format!("bank_since_ms:{base}"))
            .await?
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let mut total = Decimal::ZERO;
        for m in markets {
            let v: Option<f64> = sqlx::query_scalar(
                "SELECT CAST(SUM(CAST(markout AS REAL)) - SUM(CAST(fee AS REAL) *
                        CASE WHEN fee_currency IN ('usdt','usdc') THEN 1
                             ELSE CAST(price AS REAL) END) AS REAL)
                 FROM fills WHERE market = ? AND markout IS NOT NULL AND created_at >= ?",
            )
            .bind(m)
            .bind(since)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(None);
            total += Decimal::try_from(v.unwrap_or(0.0)).unwrap_or_default();
        }
        // Per asset. One global mark meant whichever program banked first
        // consumed the other's allowance, so the second never banked at all.
        let key = format!("banked_high_water:{base}");
        let already = self.setting_decimal(&key, Decimal::ZERO).await;
        Ok(total - already)
    }

    /// What every book earned from `since` on, in quote terms: markout less
    /// the fees of the fills judged. The pool's whole trading income, for
    /// yield.
    pub async fn earnings_since(&self, since: i64) -> Decimal {
        let v: Option<f64> = sqlx::query_scalar(
            "SELECT CAST(SUM(CAST(markout AS REAL)) - SUM(CAST(fee AS REAL) *
                    CASE WHEN fee_currency IN ('usdt','usdc') THEN 1
                         ELSE CAST(price AS REAL) END) AS REAL)
             FROM fills WHERE markout IS NOT NULL AND created_at >= ?",
        )
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(None);
        Decimal::try_from(v.unwrap_or(0.0)).unwrap_or_default()
    }

    pub async fn mark_banked(&self, base: &str, total: Decimal) -> Result<()> {
        self.set_setting(&format!("banked_high_water:{base}"), &num::to_str(total)).await
    }

    /// The asset held back from every book: never quoted, never sold. The
    /// owner's reserve plus whatever banking has set aside since.
    pub async fn set_aside(&self, asset: &str) -> Decimal {
        self.setting_decimal(&format!("reserve:{asset}"), Decimal::ZERO).await
    }

    /// Of that, how much banking put there: the earnings, in the asset.
    pub async fn set_aside_earned(&self, asset: &str) -> Decimal {
        self.setting_decimal(&format!("banked_qty:{asset}"), Decimal::ZERO).await
    }

    /// Add banked profit to the pile, in the asset.
    pub async fn add_set_aside(&self, asset: &str, qty: Decimal) -> Result<()> {
        let pile = self.set_aside(asset).await + qty;
        let earned = self.set_aside_earned(asset).await + qty;
        self.set_setting(&format!("reserve:{asset}"), &num::to_str(pile)).await?;
        self.set_setting(&format!("banked_qty:{asset}"), &num::to_str(earned)).await
    }

    /// A withdrawal comes out of the pile first: that is what the pile is for,
    /// and it is the only money the owner has been told is safe to take.
    /// Returns what the pile was cut by. Left alone, a withdrawn pile goes on
    /// being held back from coins that are now the book's own, and the ask
    /// side stops.
    pub async fn draw_set_aside(&self, asset: &str, qty: Decimal) -> Result<Decimal> {
        let pile = self.set_aside(asset).await;
        let cut = qty.min(pile).max(Decimal::ZERO);
        if cut > Decimal::ZERO {
            self.set_setting(&format!("reserve:{asset}"), &num::to_str(pile - cut)).await?;
        }
        Ok(cut)
    }

    /// Ignore trades that predate the bot taking over this market.
    pub async fn fills_since_ms(&self, market: &str) -> i64 {
        for key in [format!("fills_since_ms:{market}"), "fills_since_ms".to_string()] {
            if let Ok(Some(v)) = self.get_setting(&key).await
                && let Ok(n) = v.parse()
            {
                return n;
            }
        }
        0
    }

    /// Current position, read off the most recent fill's carried-forward
    /// totals, falling back to the inherited baseline before any fills exist.
    pub async fn position(&self, market: &str) -> Result<Position> {
        let row = sqlx::query(
            // By rowid: `pos_after` is a running total in INGESTION order, and
            // the exchange stamps some trades days in the future. Ordering by
            // created_at would pin the position to such a row until real time
            // caught up, and would drop any late-arriving fill from the total.
            "SELECT pos_after, cost_after, realized FROM fills
             WHERE market = ? ORDER BY rowid DESC LIMIT 1",
        )
        .bind(market)
        .fetch_optional(&self.pool)
        .await?;

        let fees: Option<String> = sqlx::query_scalar(
            "SELECT CAST(SUM(CAST(fee AS REAL)) AS TEXT) FROM fills WHERE market = ?",
        )
        .bind(market)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(None);

        Ok(match row {
            Some(r) => Position {
                qty: num::opt_parse(r.get::<Option<String>, _>(0).as_deref()).unwrap_or_default(),
                cost: num::opt_parse(r.get::<Option<String>, _>(1).as_deref()).unwrap_or_default(),
                realized: num::opt_parse(r.get::<Option<String>, _>(2).as_deref()).unwrap_or_default(),
                fees: num::opt_parse(fees.as_deref()).unwrap_or_default(),
            },
            None => self.baseline(market).await,
        })
    }

    /// Run a glide's clock at `rate`, where 1.0 is real time.
    ///
    /// "Sell over seven days" and "never below 1.40" conflict the moment price
    /// dips under the floor, and one has to give. The owner chose the floor, so
    /// the clock stops rather than falling behind and then selling FASTER to
    /// catch up -- which would be exactly the cheap selling the floor exists to
    /// prevent. And when price is well ABOVE the floor the clock runs fast, so
    /// a good market front-loads the sale and a poor one waits it out.
    ///
    /// Implemented by sliding the deadline instead of accumulating credit, so
    /// the linear ramp is untouched and the state survives a restart in a
    /// column that already persists. A rate of 0 pushes the deadline out by
    /// real elapsed time; a rate of 2 pulls it in by the same amount again.
    /// Returns the milliseconds of adjustment, positive for a delay.
    pub async fn advance_clock(&self, program_id: i64, rate: Decimal) -> Result<i64> {
        let now = now_ms();
        let last: Option<i64> = sqlx::query_scalar("SELECT clock_at FROM programs WHERE id = ?")
            .bind(program_id)
            .fetch_optional(&self.pool)
            .await?
            .flatten();

        // A gap wider than a few minutes is a restart or an outage, not time
        // we watched pass. Crediting it would move the deadline by the length
        // of the downtime.
        let elapsed = match last {
            Some(l) => (now - l).clamp(0, 300_000),
            None => 0,
        };
        let credited = (Decimal::from(elapsed) * rate.max(Decimal::ZERO))
            .round()
            .to_string()
            .parse::<i64>()
            .unwrap_or(elapsed);
        let adjust = elapsed - credited;

        if adjust != 0 {
            // Never pulled before `now`: a deadline in the past would finish
            // the program on the spot and dump the remainder at market.
            sqlx::query(
                "UPDATE programs SET ends_at = MAX(?, ends_at + ?), clock_at = ?, updated_at = ?
                 WHERE id = ? AND ends_at IS NOT NULL",
            )
            .bind(now + 60_000)
            .bind(adjust)
            .bind(now)
            .bind(now)
            .bind(program_id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query("UPDATE programs SET clock_at = ? WHERE id = ?")
                .bind(now)
                .bind(program_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(adjust)
    }

    /// Record the public tape. Ignores trades already stored, so the same
    /// page fetched every loop costs one conflict each rather than a duplicate.
    pub async fn record_tape(&self, market: &str, trades: &[crate::exchange::Trade]) -> Result<u64> {
        let mut n = 0;
        for t in trades {
            let r = sqlx::query(
                "INSERT OR IGNORE INTO tape(trade_id, market, ts, price, amount, taker)
                 VALUES (?,?,?,?,?,?)",
            )
            .bind(t.id)
            .bind(market)
            .bind(t.created_at * 1000)
            .bind(num::to_str(t.price))
            .bind(num::to_str(t.amount))
            .bind(t.taker_type.as_deref())
            .execute(&self.pool)
            .await?;
            n += r.rows_affected();
        }
        Ok(n)
    }

    /// When the scoreboard started. Everything reported as profit -- the
    /// verdict, the P&L split, the earnings -- is measured from here, and the
    /// report window never reaches back past it.
    ///
    /// The ARMS are deliberately not gated by it. What an arm pays per
    /// dollar-hour is scale-free and keyed by where the order rested, so it
    /// survives an account resize and a re-tune; its own half-life discounts
    /// an old regime four-fold a day and a four-day cap drops it entirely. A
    /// scoreboard reset must not blind the sizer: blind, it quotes every arm
    /// at the probe until it has re-learned what it already knew. Evidence
    /// that is genuinely wrong is fixed by re-scoring it (the one-time
    /// migrations above), not by hiding it. Zero means "everything", which is
    /// what a database the engine has never run on has.
    pub async fn analytics_epoch_ms(&self) -> i64 {
        self.get_setting(EPOCH_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .map(|v| v as i64)
            .unwrap_or(0)
    }

    /// Draw a new line: from now on, only what happens next counts on the
    /// scoreboard. `reason` is shown on the status screen.
    ///
    /// The account's starting value goes with it, so "P&L since" is measured
    /// from the balances at the line, not from launch; the engine re-seeds it
    /// on its next tick. Deposits and withdrawals before the new start are
    /// already inside that value, so the running total of them restarts too.
    pub async fn reset_analytics_epoch(&self, reason: &str) -> Result<i64> {
        let now = now_ms();
        self.set_setting(EPOCH_KEY, &now.to_string()).await?;
        self.set_setting(EPOCH_REASON_KEY, reason).await?;
        sqlx::query(
            "DELETE FROM settings
             WHERE key IN ('account_start_value', 'account_start_ts', 'account_net_deposits')",
        )
        .execute(&self.pool)
        .await?;
        Ok(now)
    }

    pub async fn epoch_reason(&self) -> Option<String> {
        self.get_setting(EPOCH_REASON_KEY).await.ok().flatten()
    }

    /// The newest fill we hold that is not stamped in the future.
    ///
    /// The exchange emits trades stamped days ahead. Taking a plain MAX() let
    /// one of those pin the ingestion cursor forever: the caller clamps it to
    /// `now`, so every subsequent request asks for fills from this instant
    /// onward and every real fill -- stamped a second or two ago -- falls just
    /// outside it. Ingestion stops, the position freezes, and every later
    /// balance move reads as a deposit. Excluding future rows from the cursor
    /// keeps them stored and out of the way.
    pub async fn last_fill_ts(&self, market: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT MAX(created_at) FROM fills WHERE market = ? AND created_at <= ?",
        )
        .bind(market)
        .bind(now_ms())
        .fetch_one(&self.pool)
        .await
        .unwrap_or(None))
    }

    // ---------- snapshots ----------

    #[allow(clippy::too_many_arguments)]
    pub async fn snapshot_book(
        &self,
        market: &str,
        book: &OrderBook,
        band: Decimal,
        our_bid: Option<Decimal>,
        our_ask: Option<Decimal>,
        inventory: Decimal,
        target: Decimal,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO book_snapshots
             (ts, market, best_bid, best_ask, mid, micro, bid_qty, ask_qty,
              bid_depth, ask_depth, our_bid, our_ask, inventory, target_inv)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(now_ms())
        .bind(market)
        .bind(book.best_bid().map(|(p, _)| num::to_str(p)))
        .bind(book.best_ask().map(|(p, _)| num::to_str(p)))
        .bind(book.mid().map(num::to_str))
        .bind(book.microprice().map(num::to_str))
        .bind(book.best_bid().map(|(_, q)| num::to_str(q)))
        .bind(book.best_ask().map(|(_, q)| num::to_str(q)))
        .bind(num::to_str(book.depth_within(Side::Buy, band)))
        .bind(num::to_str(book.depth_within(Side::Sell, band)))
        .bind(our_bid.map(num::to_str))
        .bind(our_ask.map(num::to_str))
        .bind(num::to_str(inventory))
        .bind(num::to_str(target))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn snapshot_balances(&self, balances: &[crate::exchange::Balance]) -> Result<()> {
        let ts = now_ms();
        for b in balances {
            sqlx::query(
                "INSERT OR REPLACE INTO balances(ts, currency, available, locked) VALUES (?,?,?,?)",
            )
            .bind(ts)
            .bind(&b.currency)
            .bind(num::to_str(b.balance))
            .bind(num::to_str(b.locked))
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Drop old high-frequency rows so a 10GB droplet disk stays healthy.
    pub async fn prune(&self, keep_days: i64) -> Result<u64> {
        let cutoff = now_ms() - keep_days * 86_400_000;
        let a = sqlx::query("DELETE FROM book_snapshots WHERE ts < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let b = sqlx::query("DELETE FROM events WHERE ts < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let c = sqlx::query(
            "DELETE FROM orders WHERE closed_at IS NOT NULL AND closed_at < ?
               AND client_id NOT IN (SELECT client_id FROM fills WHERE client_id IS NOT NULL)",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(a + b + c)
    }

    // ---------- control plane ----------

    pub async fn push_command(&self, command: &str, args: serde_json::Value) -> Result<i64> {
        Ok(sqlx::query("INSERT INTO commands(created_at, command, args) VALUES (?,?,?)")
            .bind(now_ms())
            .bind(command)
            .bind(args.to_string())
            .execute(&self.pool)
            .await?
            .last_insert_rowid())
    }

    pub async fn pending_commands(&self) -> Result<Vec<CommandRow>> {
        Ok(sqlx::query_as(
            "SELECT id, created_at, command, args FROM commands
             WHERE applied_at IS NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn complete_command(&self, id: i64, result: &str) -> Result<()> {
        sqlx::query("UPDATE commands SET applied_at = ?, result = ? WHERE id = ?")
            .bind(now_ms())
            .bind(result)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- events ----------

    /// Raise something worth waking someone for. Alerts are picked up by the
    /// Telegram task and pushed out; `log` on its own only records.
    pub async fn alert(&self, kind: &str, message: &str) {
        self.log("alert", kind, message, None).await;
    }

    /// Alerts not yet pushed to Telegram, oldest first.
    pub async fn pending_alerts(&self, limit: i64) -> Result<Vec<(i64, String, String)>> {
        Ok(sqlx::query_as(
            "SELECT id, kind, message FROM events
             WHERE notified_at IS NULL AND level IN ('alert','error')
             ORDER BY id LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn mark_notified(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE events SET notified_at = ? WHERE id = ?")
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Suppress alerts that predate this process, so a restart does not replay
    /// yesterday's problems into the chat.
    pub async fn suppress_old_alerts(&self) -> Result<()> {
        sqlx::query("UPDATE events SET notified_at = ? WHERE notified_at IS NULL")
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn log(&self, level: &str, kind: &str, message: &str, data: Option<serde_json::Value>) {
        let _ = sqlx::query("INSERT INTO events(ts, level, kind, message, data) VALUES (?,?,?,?,?)")
            .bind(now_ms())
            .bind(level)
            .bind(kind)
            .bind(message)
            .bind(data.map(|d| d.to_string()))
            .execute(&self.pool)
            .await;
    }
}

#[cfg(test)]
mod tests {

    /// `cycle_legs` must report what a unit traded on each leg earned, with an
    /// error bar that reflects the RATIO it is, and must value a buy's fee --
    /// charged in the base asset -- in quote terms so it subtracts correctly.
    #[tokio::test]
    async fn cycle_legs_report_net_per_unit_traded_with_a_ratio_error() {
        let dir = tempdir::TempDir::new("safetrade-cycle").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("quote_unit:prlusdc", "usdc").await.unwrap();
        let t0 = 1_800_000_000_000i64;

        // Four buys on prlusdc: total 400 of volume, +4.0 of markout, and the
        // fee taken in PRL so it has to be priced into quote.
        //   markout 1.00 each, fee 0.1 PRL at 1.00 = 0.10 quote each
        //   net per fill 0.90, total 3.60 over 400 => 90 bp
        for (id, amount) in [(1i64, "100"), (2, "100"), (3, "100"), (4, "100")] {
            sqlx::query(
                "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                                   liquidity, created_at, markout, pos_after, cost_after, realized)
                 VALUES (?,'prlusdc','buy','1.00',?,?,'0.1','prl','maker',?,'1.0','0','0','0')",
            )
            .bind(id).bind(amount).bind(amount).bind(t0 + id)
            .execute(db.pool()).await.unwrap();
        }
        // A taker fill must be excluded: the legs are a maker's edge.
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, markout, pos_after, cost_after, realized)
             VALUES (9,'prlusdc','buy','1.00','1000','1000','1','prl','taker',?,'-500','0','0','0')",
        )
        .bind(t0 + 9).execute(db.pool()).await.unwrap();
        // And an unsettled fill must be excluded: no markout, no opinion.
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized)
             VALUES (10,'prlusdc','buy','1.00','1000','1000','1','prl','maker',?,'0','0','0')",
        )
        .bind(t0 + 10).execute(db.pool()).await.unwrap();

        let legs = db.cycle_legs(t0).await.unwrap();
        assert_eq!(legs.len(), 1, "one leg: maker buys on prlusdc");
        let l = &legs[0];
        assert_eq!(l.market, "prlusdc");
        assert_eq!(l.side, Side::Buy);
        assert_eq!(l.n, 4, "the taker and the unsettled fill are not evidence");
        assert_eq!(l.volume, dec!(400));
        assert_eq!(l.net_bps.round_dp(2), dec!(90), "1.00 markout less a 0.10 fee, over 100");
        // Every fill sits exactly on the ratio, so the residuals are zero and
        // the error is zero -- which the naive sqrt(n)*sd/volume form would
        // NOT give, because the per-fill nets are all 0.90 and their sd is 0.
        assert_eq!(l.se_bps.round_dp(2), Decimal::ZERO, "identical fills, no spread");

        // Now one fill well off the ratio: the error must open up.
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, markout, pos_after, cost_after, realized)
             VALUES (5,'prlusdc','buy','1.00','100','100','0.1','prl','maker',?,'-9.0','0','0','0')",
        )
        .bind(t0 + 5).execute(db.pool()).await.unwrap();
        let legs = db.cycle_legs(t0).await.unwrap();
        let l = &legs[0];
        assert_eq!(l.n, 5);
        assert!(l.net_bps < dec!(90), "the loser drags the mean down: {}", l.net_bps);
        assert!(l.se_bps > dec!(100), "and widens the error: {}", l.se_bps);

        // A single fill gets an error bar as wide as its own estimate, so it
        // cannot clear any floor on its own.
        let legs2 = db.cycle_legs(t0 + 5).await.unwrap();
        let single = legs2.iter().find(|x| x.n == 1).expect("one fill in the window");
        assert_eq!(single.se_bps, single.net_bps.abs(), "one fill is a number, not evidence");
    }
    use super::*;
    use rust_decimal_macros::dec;

    /// Banking counts markout, not edge, and only from `bank_since_ms`.
    ///
    /// Edge is what a maker beats by construction; banking it sets aside
    /// profit that was never made. And a database carried across accounts
    /// holds the old account's fills, which are not this book's to bank.
    #[tokio::test]
    async fn unbanked_profit_is_markout_since_the_start_less_the_high_water() {
        let dir = tempdir::TempDir::new("safetrade-bank").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let t0 = 1_800_000_000_000i64;
        let fill = |id: i64, at: i64, edge: &'static str, markout: Option<&'static str>| {
            let db = &db;
            async move {
                sqlx::query(
                    "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                                       liquidity, created_at, edge, markout, pos_after, cost_after, realized)
                     VALUES (?,'prlusdc','sell','2.00','100','200','0.2','usdc','maker',?,?,?,'0','0','0')",
                )
                .bind(id).bind(at).bind(edge).bind(markout)
                .execute(db.pool()).await.unwrap();
            }
        };
        fill(1, t0 - 1, "5.0", Some("-50.0")).await; // the old account's history
        fill(2, t0 + 1, "5.0", Some("1.0")).await;
        fill(3, t0 + 2, "5.0", Some("1.0")).await;
        fill(4, t0 + 3, "5.0", None).await; // too young to judge
        db.set_setting("bank_since_ms:prl", &t0.to_string()).await.unwrap();

        let m = vec!["prlusdc".to_string()];
        // Two scored fills since the start: 1.0 markout less a 0.2 fee each.
        assert_eq!(db.unbanked_edge("prl", &m).await.unwrap().round_dp(6), dec!(1.6));
        db.mark_banked("prl", dec!(1.0)).await.unwrap();
        assert_eq!(db.unbanked_edge("prl", &m).await.unwrap().round_dp(6), dec!(0.6));
    }

    /// A withdrawal comes out of the set-aside pile, never below zero, and
    /// leaves the record of what was earned alone.
    #[tokio::test]
    async fn a_withdrawal_draws_down_the_pile_and_not_past_it() {
        let dir = tempdir::TempDir::new("safetrade-pile").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("reserve:prl", "11500").await.unwrap();
        db.add_set_aside("prl", dec!(3)).await.unwrap();
        assert_eq!(db.set_aside("prl").await, dec!(11503));
        assert_eq!(db.set_aside_earned("prl").await, dec!(3));

        assert_eq!(db.draw_set_aside("prl", dec!(11500)).await.unwrap(), dec!(11500));
        assert_eq!(db.set_aside("prl").await, dec!(3));
        // Taking more than is set aside empties it; the rest came from the book.
        assert_eq!(db.draw_set_aside("prl", dec!(10)).await.unwrap(), dec!(3));
        assert_eq!(db.set_aside("prl").await, Decimal::ZERO);
        assert_eq!(db.set_aside_earned("prl").await, dec!(3), "earnings are a record, not the pile");
        // Other assets are untouched.
        assert_eq!(db.draw_set_aside("usdc", dec!(10)).await.unwrap(), Decimal::ZERO);
    }

    /// The fill cutover must be per market and must never move once set.
    ///
    /// It used to be one global setting written by a per-asset routine, so
    /// adding a second asset dragged it forward for every market and would
    /// have skipped every fill since the last one recorded.
    #[tokio::test]
    async fn the_fill_cutover_is_per_market_and_write_once() {
        let dir = tempdir::TempDir::new("safetrade-cutover").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();

        db.ensure_fill_cutover("prlusdt").await.unwrap();
        let first = db.fills_since_ms("prlusdt").await;
        assert!(first > 0);

        // A second market arriving later gets its own, and must not disturb
        // the first.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        db.ensure_fill_cutover("usdcusdt").await.unwrap();
        assert_eq!(db.fills_since_ms("prlusdt").await, first, "must not move");

        // Re-running is a no-op.
        db.ensure_fill_cutover("prlusdt").await.unwrap();
        assert_eq!(db.fills_since_ms("prlusdt").await, first);
    }

    /// Seeding a second asset's baseline must not touch any cutover.
    #[tokio::test]
    async fn seeding_a_baseline_does_not_move_the_cutover() {
        let dir = tempdir::TempDir::new("safetrade-seed").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.ensure_fill_cutover("prlusdt").await.unwrap();
        let before = db.fills_since_ms("prlusdt").await;

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(db.seed_baseline("usdc", dec!(100), dec!(1)).await.unwrap());
        assert_eq!(db.fills_since_ms("prlusdt").await, before);
    }

    /// A thin book is scored against the book that prices the asset.
    ///
    /// PRL/USDC's own mid is whatever stale order was left there last. Scored
    /// against it, 250 fills that earned a profit against PRL/USDT's mid read
    /// as a loss of the same size -- a measurement artifact that made the best book look like the
    /// worst.
    #[tokio::test]
    async fn a_referenced_book_is_scored_against_the_reference_mid() {
        let dir = tempdir::TempDir::new("safetrade-ref").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("reference:prlusdc", "prlusdt").await.unwrap();
        db.set_setting("reference_cross:prlusdc", "usdcusdt:div").await.unwrap();
        let now = now_ms();
        let fill_ts = now - MARKOUT_HORIZON_MS - 60_000;
        let later = fill_ts + MARKOUT_HORIZON_MS;
        for (market, ts, mid) in [
            ("prlusdc", fill_ts, "1.00"), // stale residue
            ("prlusdc", later, "1.00"),
            ("prlusdt", fill_ts, "1.30"), // where PRL is actually priced
            ("prlusdt", later, "1.35"),
            ("usdcusdt", fill_ts, "1.02"), // USDT per USDC
            ("usdcusdt", later, "1.02"),
        ] {
            sqlx::query("INSERT INTO book_snapshots(ts, market, mid) VALUES (?, ?, ?)")
                .bind(ts).bind(market).bind(mid)
                .execute(db.pool()).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO fills(trade_id, market, side, price, amount, total, fee, fee_currency,
                               liquidity, created_at, pos_after, cost_after, realized)
             VALUES (1,'prlusdc','sell','1.32','100','132','0.132','usdc','maker',?,'0','0','0')",
        )
        .bind(fill_ts)
        .execute(db.pool()).await.unwrap();

        assert_eq!(db.backfill_edges().await.unwrap(), 1);
        assert_eq!(db.settle_markouts().await.unwrap(), 1);
        let (edge, markout): (String, String) =
            sqlx::query_as("SELECT edge, markout FROM fills WHERE trade_id = 1")
                .fetch_one(db.pool()).await.unwrap();
        // Reference mid in USDC terms: 1.30 / 1.02 at the fill, 1.35 / 1.02 later.
        let at_fill = dec!(1.30) / dec!(1.02);
        let after = dec!(1.35) / dec!(1.02);
        assert_eq!(num::parse(&edge).round_dp(6), ((dec!(1.32) - at_fill) * dec!(100)).round_dp(6));
        assert_eq!(num::parse(&markout).round_dp(6), ((dec!(1.32) - after) * dec!(100)).round_dp(6));
        assert!(num::parse(&markout) < Decimal::ZERO, "sold under where PRL went: a loss");
        // Against the local mid this would have booked +32 of edge on a losing fill.
        assert!(num::parse(&edge) < dec!(32));
    }

    /// Orders placed before the width was recorded get it from the snapshot
    /// nearest their placement, with the tick read off the snapshot prices;
    /// one with no snapshot near it stays out of the arms. Once.
    #[tokio::test]
    async fn the_width_backfill_reads_the_tick_off_the_snapshots() {
        let dir = tempdir::TempDir::new("safetrade-width").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let now = now_ms();
        // A normalised "1.2" alone would read as a tick of 0.1; the 1.23 ask
        // in the same window fixes the scale at two places for both.
        for (dt, bid, ask) in [(-3_600_000, "1.2", "1.23"), (-1_800_000, "1.2", "1.21")] {
            sqlx::query("INSERT INTO book_snapshots(ts, market, best_bid, best_ask, mid) VALUES (?, 'prlusdt', ?, ?, '1.2')")
                .bind(now + dt)
                .bind(bid)
                .bind(ask)
                .execute(db.pool())
                .await
                .unwrap();
        }
        for (id, oid, dt) in [("wide", 21, -3_600_000 + 5_000), ("tight", 22, -1_800_000 - 5_000), ("alone", 23, -600_000)] {
            db.record_placement(id, None, "prlusdt", Side::Buy, dec!(1.19), dec!(10), 0, Some(dec!(100)), Some(dec!(0.01)), None)
                .await
                .unwrap();
            db.attach_exchange_id(id, oid, "wait").await.unwrap();
            sqlx::query("UPDATE orders SET placed_at = ? WHERE client_id = ?")
                .bind(now + dt)
                .bind(id)
                .execute(db.pool())
                .await
                .unwrap();
        }
        assert_eq!(db.backfill_order_width(86_400_000).await.unwrap(), 2);
        let widths: Vec<(String, Option<f64>)> =
            sqlx::query_as("SELECT client_id, width_ticks FROM orders ORDER BY client_id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(widths, vec![("alone".into(), None), ("tight".into(), Some(1.0)), ("wide".into(), Some(3.0))]);
        assert_eq!(db.backfill_order_width(86_400_000).await.unwrap(), 0, "once; the marker holds");
    }

    /// Orders and fills become per-arm episodes: the order's distance at
    /// placement picks the arm, its resting time is the exposure, and the
    /// fill's markout net of a fee valued in quote terms is the return. A fee
    /// paid in the base asset is valued at the fill price.
    #[tokio::test]
    async fn arm_observations_attribute_exposure_and_net_to_the_placement_arm() {
        use crate::strategy::arms::{Bucket, Width};
        let dir = tempdir::TempDir::new("safetrade-arms").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("quote_unit:prlusdt", "usdt").await.unwrap();
        let t0 = 1_800_000_000_000i64; // an episode boundary
        // A bid 150 bp under fair value on a three-tick book, resting for
        // exactly two episodes.
        db.record_placement("c1", None, "prlusdt", Side::Buy, dec!(2.00), dec!(100), 0, Some(dec!(150)), Some(dec!(0.01)), Some(dec!(3)))
            .await
            .unwrap();
        db.attach_exchange_id("c1", 11, "wait").await.unwrap();
        sqlx::query("UPDATE orders SET placed_at = ?, closed_at = ? WHERE client_id = 'c1'")
            .bind(t0)
            .bind(t0 + 2 * MARKOUT_HORIZON_MS)
            .execute(db.pool()).await.unwrap();
        // An ask at the touch of a one-tick book, still open.
        db.record_placement("c2", None, "prlusdt", Side::Sell, dec!(2.02), dec!(50), 0, Some(dec!(30)), Some(dec!(0.01)), Some(dec!(1)))
            .await
            .unwrap();
        db.attach_exchange_id("c2", 12, "wait").await.unwrap();
        sqlx::query("UPDATE orders SET placed_at = ? WHERE client_id = 'c2'")
            .bind(t0 + MARKOUT_HORIZON_MS)
            .execute(db.pool()).await.unwrap();
        // A stray we adopted: no distance, no arm.
        db.record_placement("c3", None, "prlusdt", Side::Sell, dec!(2.10), dec!(50), -1, None, None, None)
            .await
            .unwrap();
        db.attach_exchange_id("c3", 13, "wait").await.unwrap();
        for (id, oid, side, fee, ccy, mo, ts) in [
            (1, 11, "buy", "0.1", "prl", "3.0", t0 + 60_000),        // fee 0.1 PRL at 2.00 = 0.2
            (2, 11, "buy", "0.1", "prl", "-1.0", t0 + MARKOUT_HORIZON_MS + 1),
            (3, 12, "sell", "0.2", "usdt", "", t0 + MARKOUT_HORIZON_MS + 2), // unscored
            (4, 13, "sell", "0.2", "usdt", "5.0", t0 + 1),           // no arm
        ] {
            sqlx::query(
                "INSERT INTO fills(trade_id, order_id, market, side, price, amount, total, fee,
                                   fee_currency, liquidity, created_at, pos_after, cost_after,
                                   realized, markout)
                 VALUES (?,?,'prlusdt',?,'2.00','100','200',?,?,'maker',?,'0','0','0',?)",
            )
            .bind(id).bind(oid).bind(side).bind(fee).bind(ccy).bind(ts)
            .bind(if mo.is_empty() { None } else { Some(mo) })
            .execute(db.pool()).await.unwrap();
        }

        let now = t0 + 3 * MARKOUT_HORIZON_MS;
        let obs = db.arm_observations(0, now).await.unwrap();
        let bid: Vec<_> = obs.iter().filter(|o| o.arm.side == Side::Buy).collect();
        assert_eq!(bid.len(), 2, "two episodes of resting: {obs:?}");
        assert!(bid.iter().all(|o| o.arm.bucket == Bucket::From100), "150 bp is the 100-200 arm");
        assert!(bid.iter().all(|o| o.arm.width == Width::Wide), "three ticks is the wide cell");
        // $200 resting for five minutes is 16.67 dollar-hours an episode.
        for o in &bid {
            assert!((o.exposure_dh - 200.0 / 12.0).abs() < 1e-6, "{o:?}");
        }
        assert!((bid[0].net - 2.8).abs() < 1e-9, "3.0 markout less 0.2 of fee, got {}", bid[0].net);
        assert!((bid[1].net - -1.2).abs() < 1e-9, "-1.0 less 0.2, got {}", bid[1].net);
        assert_eq!(bid[0].orders, 1, "the order is counted once, in its first episode");
        assert_eq!(bid[1].orders, 0);

        let ask: Vec<_> = obs.iter().filter(|o| o.arm.side == Side::Sell).collect();
        assert_eq!(ask.len(), 2, "open since episode 1, counted to now");
        assert!(ask.iter().all(|o| o.arm.bucket == Bucket::Under50));
        assert!(ask.iter().all(|o| o.arm.width == Width::One));
        assert_eq!(ask.iter().map(|o| o.fills).sum::<u32>(), 0, "unscored fills are not bets yet");
        assert!(obs.iter().all(|o| o.net != 5.0), "the stray's fill belongs to no arm");

        // The window clips: nothing before `since`.
        let late = db.arm_observations(t0 + MARKOUT_HORIZON_MS, now).await.unwrap();
        assert_eq!(late.iter().filter(|o| o.arm.side == Side::Buy).count(), 1);
    }

    /// The epoch marker keeps its meaning across the rename, and the old key
    /// does not linger as a second, dead control.
    #[tokio::test]
    async fn the_old_epoch_key_is_carried_over() {
        let dir = tempdir::TempDir::new("safetrade-epoch").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        db.set_setting("kelly_since_ms", "12345").await.unwrap();
        db.set_setting("banked_high_water", "35").await.unwrap();
        db.set_setting("edge:prlusdt", "{}").await.unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        assert_eq!(db.analytics_epoch_ms().await, 12345);
        for k in ["kelly_since_ms", "banked_high_water", "edge:prlusdt"] {
            assert!(db.get_setting(k).await.unwrap().is_none(), "{k} should be gone");
        }
    }

    /// Drawing a line restarts the account's starting value, and the next
    /// seeding takes the LIVE value rather than reconstructing launch. The
    /// account shrank by over nine-tenths between regimes, mostly by
    /// withdrawal; a scoreboard that still
    /// measured from the launch value would report that as a loss.
    #[tokio::test]
    async fn a_new_epoch_restarts_the_account_baseline_from_the_live_value() {
        let dir = tempdir::TempDir::new("safetrade-epoch-start").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        // Launch: a baseline and an early balance snapshot to reconstruct from.
        db.seed_baseline("prl", dec!(20707), dec!(0.99)).await.unwrap();
        db.snapshot_balances(&[crate::exchange::Balance {
            currency: "usdt".into(), balance: dec!(1000), locked: Decimal::ZERO,
        }])
        .await
        .unwrap();
        assert!(db.seed_account_value(dec!(999_999)).await.unwrap());
        let launch = db.account_start_value().await.unwrap();
        assert_eq!(launch, dec!(20707) * dec!(0.99) + dec!(1000), "reconstructed, not the live value");
        db.add_external_flow(dec!(-4845)).await.unwrap();

        let drawn = db.reset_analytics_epoch("settings changed").await.unwrap();
        assert_eq!(db.analytics_epoch_ms().await, drawn);
        assert_eq!(db.epoch_reason().await.as_deref(), Some("settings changed"));
        assert!(db.account_start_value().await.is_none(), "cleared, to be re-seeded");
        assert_eq!(db.net_deposits().await, Decimal::ZERO, "money moved before the line is inside the new start");

        // The engine's next tick seeds from what the account holds NOW.
        assert!(db.seed_account_value(dec!(7000)).await.unwrap());
        assert_eq!(db.account_start_value().await.unwrap(), dec!(7000));
        assert!(db.account_start_ts().await.unwrap() >= drawn);
        assert!(!db.seed_account_value(dec!(1)).await.unwrap(), "once per epoch");
    }

    #[tokio::test]
    async fn a_baseline_is_seeded_once_per_asset() {
        let dir = tempdir::TempDir::new("safetrade-baseline").unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        assert!(db.seed_baseline("prl", dec!(20707), dec!(0.99)).await.unwrap());
        assert!(!db.seed_baseline("prl", dec!(999), dec!(0.5)).await.unwrap(), "only once");
        assert_eq!(db.baseline("prl").await.qty, dec!(20707));
        // A different asset is independent.
        assert_eq!(db.baseline("usdc").await.qty, Decimal::ZERO);
    }
}
