-- SafeTrade bot storage.
--
-- Money columns are TEXT holding exact decimal strings. SQLite REAL is f64 and
-- would silently round prices and amounts; every read parses back to Decimal.
-- Timestamps are INTEGER unix milliseconds, UTC.

PRAGMA journal_mode = WAL;

-- Runtime-tunable settings. The bot re-reads this table every loop, so `ctl set`
-- takes effect without a restart. Values that live here are the ones you want to
-- change on a running bot; structural settings stay in config.toml.
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- An execution program: the thing that decides where inventory *should* be.
-- kind = 'neutral'     hold target_qty, harvest spread around it
--        'accumulate'  ramp inventory up to target_qty by ends_at (TWAP buy)
--        'distribute'  ramp inventory down to target_qty by ends_at (TWAP sell)
--        'flatten'     go to target_qty as fast as passive quoting allows
--        'balanced'    hold `ratio` of total portfolio VALUE in the base asset,
--                      recomputed every loop. Unlike the others, its target is
--                      a quantity that moves with price: up when price falls,
--                      down when price rises. That is what makes it sell
--                      strength and buy weakness, growing both pools in a
--                      ranging market.
CREATE TABLE IF NOT EXISTS programs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    market        TEXT    NOT NULL,
    kind          TEXT    NOT NULL,
    state         TEXT    NOT NULL DEFAULT 'active',  -- active|paused|done|cancelled
    start_qty     TEXT    NOT NULL,   -- base inventory when the program began
    target_qty    TEXT    NOT NULL,   -- base inventory we want at ends_at
    ratio         TEXT,               -- 'balanced' only: share of value in base
    banked        TEXT,               -- profit banked into the base asset, quote terms
    limit_price   TEXT,               -- never buy above / sell below this (optional)
    started_at    INTEGER NOT NULL,
    ends_at       INTEGER,            -- NULL for neutral: no deadline
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    note          TEXT
);
CREATE INDEX IF NOT EXISTS idx_programs_active ON programs(market, state);

-- Every order we ask the exchange for, including ones that failed to place.
-- client_id is ours and is the join key that survives a restart before we have
-- learned the exchange id.
CREATE TABLE IF NOT EXISTS orders (
    client_id     TEXT PRIMARY KEY,
    exchange_id   INTEGER UNIQUE,
    program_id    INTEGER REFERENCES programs(id),
    market        TEXT    NOT NULL,
    side          TEXT    NOT NULL,   -- buy|sell
    ord_type      TEXT    NOT NULL,   -- limit|market
    price         TEXT,
    amount        TEXT    NOT NULL,
    filled_amount TEXT    NOT NULL DEFAULT '0',
    avg_price     TEXT,
    state         TEXT    NOT NULL,   -- pending|wait|done|cancel|reject
    level         INTEGER,            -- quote ladder rung, 0 = touch
    placed_at     INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    closed_at     INTEGER,
    error         TEXT
);
CREATE INDEX IF NOT EXISTS idx_orders_open  ON orders(market, state);
CREATE INDEX IF NOT EXISTS idx_orders_time  ON orders(placed_at);

-- Fills as reported by the exchange. `trade_id` is the exchange's, so the
-- UNIQUE constraint makes fill ingestion idempotent across restarts.
CREATE TABLE IF NOT EXISTS fills (
    trade_id    INTEGER PRIMARY KEY,
    order_id    INTEGER,
    client_id   TEXT REFERENCES orders(client_id),
    program_id  INTEGER REFERENCES programs(id),
    market      TEXT    NOT NULL,
    side        TEXT    NOT NULL,
    price       TEXT    NOT NULL,
    amount      TEXT    NOT NULL,
    total       TEXT    NOT NULL,
    fee         TEXT    NOT NULL DEFAULT '0',
    fee_currency TEXT,
    liquidity   TEXT,                 -- maker|taker
    created_at  INTEGER NOT NULL,
    -- The market mid at the moment of the fill, and the edge that implies,
    -- both settled at ingestion. Recomputing edge later from snapshots made it
    -- depend on snapshot retention and timestamp matching, so the same trade
    -- could score differently tomorrow.
    mid_at_fill TEXT,
    edge        TEXT,
    -- running position after this fill, for PnL without a full replay
    pos_after   TEXT,
    cost_after  TEXT,
    realized    TEXT
);
CREATE INDEX IF NOT EXISTS idx_fills_time ON fills(market, created_at);

-- Top-of-book samples, one per loop. Drives the reporting charts and lets you
-- check after the fact whether the bot moved the price.
CREATE TABLE IF NOT EXISTS book_snapshots (
    -- (ts, market), not ts alone: three books are sampled in the same loop and
    -- often land in the same millisecond, so a bare ts made them overwrite one
    -- another and two of the three vanished.
    ts          INTEGER NOT NULL,
    market      TEXT NOT NULL,
    best_bid    TEXT,
    best_ask    TEXT,
    mid         TEXT,
    micro       TEXT,                 -- size-weighted microprice
    bid_qty     TEXT,
    ask_qty     TEXT,
    bid_depth   TEXT,                 -- quote-currency depth within band
    ask_depth   TEXT,
    our_bid     TEXT,                 -- best price we are showing, if any
    our_ask     TEXT,
    inventory   TEXT,
    target_inv  TEXT,
    PRIMARY KEY (ts, market)
);

CREATE TABLE IF NOT EXISTS balances (
    ts        INTEGER NOT NULL,
    currency  TEXT    NOT NULL,
    available TEXT    NOT NULL,
    locked    TEXT    NOT NULL,
    PRIMARY KEY (ts, currency)
);

-- Control plane. `ctl` writes here, the bot drains it each loop. Keeping
-- commands in the DB means the audit trail of "who told it to do what, when"
-- is the same file as the trading history.
CREATE TABLE IF NOT EXISTS commands (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL,
    command    TEXT    NOT NULL,
    args       TEXT    NOT NULL DEFAULT '{}',
    applied_at INTEGER,
    result     TEXT
);
CREATE INDEX IF NOT EXISTS idx_commands_pending ON commands(applied_at);

CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         INTEGER NOT NULL,
    level      TEXT    NOT NULL,      -- info|warn|error|alert
    kind       TEXT    NOT NULL,
    message    TEXT    NOT NULL,
    data       TEXT,
    -- Alerts are pushed to Telegram by a separate task. Using the events table
    -- as the queue means an alert raised while Telegram is down is delivered
    -- when it comes back, instead of being lost in a channel.
    notified_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_events_time ON events(ts);
CREATE INDEX IF NOT EXISTS idx_events_undelivered ON events(notified_at, level);
