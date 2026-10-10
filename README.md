# safetrade

An execution and market-making bot for [SafeTrade](https://safe.trade), built for
the **PRL/USDT** pair.

Two goals, in priority order:

1. **Work size without moving the price.** Buy or sell a target quantity slowly
   and passively, like a TWAP.
2. **Earn the spread while doing it.** Quote both sides and harvest volatility
   when there is nothing to work.

> **This software trades real money.** It is published as-is, under the MIT
> licence, with no warranty. It was built for one account on one exchange; the
> defaults and the findings below are specific to that market and may lose
> money anywhere else. Nothing here is financial advice. Run `--dry-run` first.

## The one idea

Both goals are the same mechanism. A **program** supplies a *target inventory*,
and quoting is the act of closing the gap to that target while getting paid for
the patience.

| Program      | Target inventory                        | Behaves like          |
| ------------ | --------------------------------------- | --------------------- |
| `accumulate` | ramps **up** over the duration          | passive TWAP buy      |
| `distribute` | ramps **down** over the duration        | passive TWAP sell     |
| `neutral`    | sits still                              | classic market making |
| `flatten`    | steps to a value, no schedule           | take profit / exit    |
| `balanced`   | **a share of portfolio value**, live    | constant-mix rebalance |
| `learned`    | `balanced`, with the share **set by the arms** hourly | the split follows the yield |
| `twap`       | **everything the cash can buy**, live   | buy-only TWAP, nothing else quoting |

Because all of them are just a number, there is one quoting path and no
separate execution engine. Switching stance at runtime is changing the target,
nothing more.

`balanced` is the odd one out, and it is the one to use if the goal is *grow
both pools*. Its target is not a quantity but a **share of total value**:

```
target_qty = ratio × (base × price + quote) ÷ price
```

So when the price rises the target quantity falls and the bot sells into
strength; when it falls the target rises and the bot buys weakness. In a ranging
market that mechanically adds to both the base and the quote pile. The honest
caveat: in a sustained trend it underperforms simply holding the winning side.
That is the trade, and it is worth making on purpose rather than by accident.

Give it a duration and the approach to the first target is spread over that
window, which is how you unwind a lopsided starting position without dumping:

```sh
safetrade ctl balance 50 --over 24h     # ease to 50/50 over a day, then track it
```

**Deposits and withdrawals are handled.** The target is a share of live
portfolio value, so it is correct at any balance the moment coins land. The
*glide* needs help though: it interpolates from an anchor fixed when the
program started, so without intervention a deposit would make the whole new
gap appear at once and the bot would work it off as hard as the caps allow.

So the bot watches for inventory moving by anything other than the net of its
own fills, and re-anchors the glide from the new balance over the same
configured window. Deposit any amount, at any time, and it restarts a gradual
convergence to your ratio and tells you on Telegram. Buy fees are taken in the
base asset, so the detector ignores drift below 1% of inventory.

`twap` is re-anchored too, and by the currency it *spends*: a USDT deposit is
filed under USDT while the program is filed under PRL, so the re-anchor is done
for every running TWAP rather than only for the currency that landed. See
**TWAP mode** below.

Fixed-target programs (`accumulate`, `distribute`) are deliberately **not**
re-anchored — "end up holding N" means N, and silently rewriting it after a
deposit would be the bot deciding something you did not ask for. You get an
alert instead.

Inventory error — how far actual inventory is from target — then does double
duty. It skews the **prices** we quote at, and it skews the **sizes** on each
side. So a TWAP that falls behind schedule automatically leans toward the touch
and quotes bigger, and one that gets ahead backs off. That is the catch-up
behaviour a TWAP needs, with no scheduler.

## Why this market shapes the design

`PRL/USDT` has `price_precision = 2`. The coin trades near `1.00`, so:

> **One tick is `0.01`, which is about 1% of the price.**

That single fact drives most of the defaults. Improving the best bid by one tick
moves the displayed price a full percent. So:

- `max_improve_ticks = 0` — the bot **joins** the touch and never betters it.
  Its quoting cannot by itself move the price.
- `allow_taker = false` — crossing the spread is exactly how you move a price.
- `max_order_pct_of_depth` — never be more than a slice of a thin book.
- `max_participation` — stand down after taking too large a share of volume.

Fees are **0.10%** per side, charged in the received currency. Against a ~1%
tick, one tick of captured spread nets roughly 0.8%.

## Setup

```sh
cargo build --release
cp config.example.toml config.toml     # edit to taste
cp .env.example .env                   # fill in credentials
./target/release/safetrade status
```

### Environment (`.env`)

Secrets and machine-specific values live in `.env`, which is gitignored.
`.env.example` lists every variable with a note on each. The bot reads `.env`
from its working directory at start-up; anything already set in the
environment wins, so a systemd `EnvironmentFile` overrides a stray file.

| Variable | Used by | Required |
| --- | --- | --- |
| `SAFETRADE_API_KEY`, `SAFETRADE_API_SECRET` | the bot | for `status`, `run`, and anything that touches the exchange |
| `TELEGRAM_BOT_TOKEN` | the bot, `heartbeat-check.sh` | no; without it the bot is silent (`TELEGRAM`, `TELGRAM` accepted as aliases) |
| `TELEGRAM_CHAT_ID` | the bot | no; otherwise the first `/start` binds the chat |
| `SAFETRADE_HOST` | `deploy/*.sh` | to deploy; the host's IPv4 address |
| `SAFETRADE_DROPLET_ID`, `DIGITAL_OCEAN_API_KEY` | `deploy/bootstrap.sh` | only for bootstrapping a DigitalOcean droplet |

`report` and `arms` read a database file and need none of them.

`deploy.sh` copies only the exchange and Telegram lines to the host, as
`/opt/safetrade/etc/safetrade.env` (mode 0600, piped over ssh so they never
appear in argv). The deployment variables never leave your machine.

### Deployed configuration

Put your production config at `deploy/config.prod.toml`. It is gitignored --
it is your tuning and your account's sizes, not the project's -- but
`deploy.sh` syncs it up and installs it as the host's config on every deploy,
so the box is reproducible from your checkout. Without it a fresh host gets
`config.example.toml`. Runtime tuning that should survive a deploy belongs in
`safetrade ctl set`, which lives in the database.

### Things that will bite you

**The API key is IP-allowlisted.** Add the trading host's address in the
SafeTrade UI. If a host prefers IPv6 the exchange sees the v6 address and
rejects it as `authz.apikey_untrusted_ip`, which reads like a key problem but
is not. Force IPv4:

```sh
printf 'precedence ::ffff:0:0/96  100\n' > /etc/gai.conf
```

**Auth signs a millisecond nonce**, so a host clock more than a few seconds off
fails *every* private call with `authz.nonce_expired`. Keep `chrony` running.

**The API's field names and error codes mislead.** Every one of these cost a
deploy, and all are pinned in tests now rather than trusted from memory:

| Surprise | What it looks like |
| --- | --- |
| `GET /account/balances` 404s on **any** query string | looks like a bad `limit` value |
| `GET /market/trades` 500s on `order_by=desc` | looks like a server fault |
| `created_at` is a unix int on public, RFC 3339 on private | decode error on one endpoint only |
| Orders have `origin_amount`/`filled_amount`/`type`, no `remaining_volume` | order is placed, then fails to decode |
| On private trades `side` is the **counterparty**, `order_side` is **ours** | every fill books backwards |

That last one is the dangerous one, because it is silently wrong rather than
loud. The giveaway is `fee_currency`: a genuine buy pays its fee in the base
asset, a sell pays in quote.

## Running

```sh
safetrade status                  # market, book, balances. Touches nothing.
safetrade run --dry-run           # compute and log quotes, send no orders
safetrade run                     # live
```

The bot cancels every resting order on `SIGTERM`, so `systemctl stop` is safe.

## Controlling a running bot

Commands go into the database and the bot picks them up on its next loop, so the
audit trail of who asked for what lives in the same file as the trading history.

```sh
# Put the whole account into PRL over six hours, bids only
safetrade ctl twap --over 6h

# Accumulate 20,000 PRL over two days, never paying above 1.02
safetrade ctl program accumulate 20000 --over 2d --limit-price 1.02

# Take profit: work back down to 5,000 over six hours
safetrade ctl program distribute 5000 --over 6h

# Neutral: hold ~10,000 and harvest the spread around it
safetrade ctl program neutral 10000

# Let the arms set the PRL / USDT / USDC split, and keep setting it
safetrade ctl learn

safetrade ctl pause               # pull all quotes, keep running
safetrade ctl resume
safetrade ctl panic               # cancel everything, disable quoting

safetrade ctl set                 # list every runtime-tunable setting
safetrade ctl set size_base 400
safetrade ctl log                 # what was asked, and what happened
```

## Rate limiting

SafeTrade publishes no rate limit. Peatio answers `429` when one is breached,
but the thresholds are set per deployment by the operator, so the ceiling can
only be found by looking for it.

The client paces itself with AIMD — the rule TCP uses for congestion. It creeps
upward a few percent after a long run of successes and **halves on the first
refusal**. The asymmetry is deliberate: probing upward slowly costs a little
throughput, while probing downward slowly costs a stream of rejected requests
and an annoyed exchange. The learned rate is written to the database each loop
and restored on start, so a deploy does not rediscover the ceiling by bouncing
off it again. It is capped at 4/s regardless — a bot quoting passively on three
thin books has no use for more.

`report` shows the current learned rate under `PACING`.

## Telegram

Set `TELEGRAM_BOT_TOKEN` in the environment and the bot will talk to you. Send
it `/start` once: the first `/start` claims the bot and every other chat is
ignored from then on, because a bot username is guessable and this one can move
money. Pin it explicitly with `TELEGRAM_CHAT_ID` if you prefer.

**Telegram runs on the trading host, not your laptop.** The bot process holds
the long-poll connection, so it keeps answering while your machine is closed.

Only ever run **one** instance with the token. Telegram delivers each update to
exactly one long-polling consumer, so a second instance silently takes commands
meant for the live bot and applies them to its own database — you would see a
reply and believe the real bot had acted. `run --dry-run` therefore never starts
Telegram, and `run --no-telegram` disables it explicitly.

It posts a status message every hour (`telegram_status_secs`) and answers:

| Message | Does |
| --- | --- |
| `status` | position, program progress, execution quality |
| `report 7d` | the same over a window |
| `buy 5000 over 6h` | acquire 5,000 more, worked passively over six hours |
| `buy 5000 over 2d max 1.02` | …and never pay above 1.02 |
| `sell 5000 over 12h min 1.10` | offload 5,000, never below 1.10 |
| `twap 6h` | put every spendable unit into PRL over six hours, bids only |
| `twap 6h max 1.60` | …never paying above 1.60 |
| `balance 50` | hold 50% of value in PRL, rebalancing forever |
| `balance 50 over 24h` | …easing into it over a day |
| `hold 20000` | market-make around an absolute quantity |
| `flat` | work back to zero |
| `pause` / `resume` / `panic` | as the CLI |
| `settings`, `set size_base 400` | inspect and change runtime settings |
| `help` | all of the above |

`buy` and `sell` are **relative** to current inventory, which is how people say
it; `hold` is an absolute target. A relative quantity is resolved against live
inventory when the engine applies it, not when you typed it, so it cannot go
stale sitting in the queue.

Telegram messages land in the same `commands` table as `safetrade ctl`, so the
engine cannot tell the two apart and both show up in `ctl log`.

## Reporting

Reporting reads a database file and nothing else — no API, no credentials — so
it runs on a laptop against a copy pulled down from the server.

```sh
./deploy/pull-db.sh                      # snapshot the live DB locally
safetrade report --db data/remote.db --window 24h
```

The headline is the **verdict**: net five-minute markout of every fill since
the scoreboard started, against how many episodes produced them, and whether
that is enough evidence to call. Everything under it -- the P&L split into
market and trading, the earnings by hour or day, the arm table -- is judged on
the same window, and both the compact status and the full report name it
(`since 09-24 18:02 · settings changed`).

The scoreboard restarts by itself when the settings in the config change: the
engine fingerprints the strategy settings at start-up and draws the line when
they differ from what last went live, so a deploy of a re-tune is judged from
the moment it went live and nobody has to remember a step. `safetrade ctl
epoch` (or `epoch` on Telegram) draws one by hand for a change the config
cannot see. The arms are never reset by either: see below.

For a TWAP the report also shows execution against the market's time-weighted
mid -- "did I beat the average price on offer while I was working?" -- and how
far the mid drifted over the window.

`safetrade arms --db data/remote.db` re-estimates the arm table from a copy
exactly as the running bot does on its refresh, judged as of the bot's last
heartbeat, and prints it. Run it before un-pausing after a change to the arm
settings, so what the bot is about to size from is on the screen first.

## Deployment

```sh
./deploy/deploy.sh                       # rsync, build on the host, install
ssh root@HOST 'systemctl enable --now safetrade'
ssh root@HOST 'journalctl -u safetrade -f'
```

The build happens on the server. The host runs Ubuntu 24.04 (glibc 2.39) while
a typical dev box runs something newer, so a locally built binary would not run
there.

## What is stored

One SQLite file holds everything: `orders`, `fills` (with position and realized
PnL carried forward so PnL never needs a full replay), `book_snapshots` for the
price-impact analysis, `balances`, the `commands` control queue, `programs`, and
an `events` audit log. WAL mode means you can read it while the bot writes.

## Sizing from measured edge, per arm

Every fill is re-scored five minutes later against where the mid went
(`fills.markout`). That is the only honest measure of whether quoting earns:
`edge` compares our price to the mid at the instant of the fill, which we beat
by construction.

Every resting order is a bet on an **arm**: its market, its side, how far
from fair value it sat when it was placed (`orders.bps`, bucketed at 50, 100,
200 and 400 bp), how fast the market was moving when it was placed
(`orders.recent_move`, the five-minute range: calm under 2%, fast to 4%, wild
above), and how wide the stranger book was when it was placed
(`orders.width_ticks`: one tick, two, or three and more). The width is there
because on a tick-quantised book the distance alone cannot tell "a tick
behind the touch of a one-tick book" from "the touch of a three-tick book",
and those are opposite bets: on the violent day prlusdt's one-tick book lost
4 ‰ per dollar-hour on both sides and its three-tick book paid 21, inside the
same distance buckets. What an arm pays is the net markout of its fills divided by the
**dollar-hours its orders spent resting** -- dollars per unit of capital per
unit time, so an arm that never fills scores nothing however good its one
fill looked, and the far rungs cannot win on basis points alone.

Per arm, the orders and fills are folded into five-minute episodes and a
decay-weighted ratio estimator gives the yield and its standard error. Each
cell is shrunk toward the other cells of its bucket -- the other side by half
the bucket's yield, the other regimes and the other widths by the whole of
it, because on the violent day the touch went from flat in a calm market to
-57 bp a fill in a wild one while the far rungs went from losing to paying,
and the one-tick book lost where the three-tick book paid. Buckets are never
pooled with one another (they differ by an order of magnitude more than their
errors). Each arm then quotes the **net odds that it pays**, from a Student-t
on its posterior:

* an arm the evidence cannot separate from zero rests the exchange minimum
  order -- enough to keep producing fills, so the estimate can recover, and
  no more; there is no probe share and no fill threshold;
* one standard error from zero is 68% of the rung, two is 95%; it moves
  continuously in both directions as fills arrive and old ones decay;
* a storm nobody has measured yet borrows its bucket's calm yield with a
  wider error bar, and so quotes less than the calm before the storm has
  cost anything -- the volatility pull-back is learned, not typed;
* accumulate / distribute / flatten programs are never gated -- they are
  execution, not a bet on the spread.

A literal Kelly fraction does not appear because it does not bind: at the
measured variance the full-Kelly stake is 1-30 times equity on every arm
with a positive mean, and scoring thirty minutes on instead of five raises
the variance 3-9x and the stake is still 5-50 rungs. The caps are what limit
a market maker on a thin book, and the stake within them is the Kelly bet on
the one thing that is uncertain, the sign.

The same table splits each currency between the books that spend it: the PRL
balance goes to whichever book's asks have earned it, the USDT to whichever
book's bids have. There is no fixed stablecoin reserve; USDC is acquired when
the arms that acquire it pay.

The table is stored under `arms` and printed in the report's ARMS section and
the compact status, one line per market with its cells beneath it.

**Why prlusdt loses**, measured on 1,233 maker fills of the violent day and
203 of a calmer stretch: its fills are the sweeps. Against the
mid at the fill they look worth +72 bp; thirty seconds later 0; a minute
later -14. The book is one tick wide 81-84% of the time, so a rung either
joins the back of the queue at the touch or sits a tick behind it, and either
way it fills only when the level ahead is cleared -- after which the mid has
moved a tick through it, and a half-tick of spread cannot pay for a tick of
move plus the fee. The loss is symmetric by side (-24 bp on bids, -23 on
asks), it is not staleness (orders under thirty seconds old lose most), it is
not the reference (the microprice predicts the next move, beta 0.4-0.6), and
it is concentrated: the worst 5% of fills are the whole loss and more, and
they are the full fills of the largest rungs (-32 bp on them; everything
else netted slightly positive). The public tape says the same from the other side: 94% of
aggressor clusters are single-level taps after which the mid does not move,
worth +25-42 bp gross to whoever is at the head of the queue, and we are
never at the head. Where the book was three or more ticks wide our rung was
the touch, filled from taps, and paid +43 ± 17 ‰ per dollar-hour -- 6% of
that day and 2% of a calm one. That is the condition the width dimension
lets the arms fund when it recurs.

The arms are **not** reset by the scoreboard's epoch. What an arm pays per
dollar-hour is scale-free and keyed by where the order rested, so it survives
an account resize and a re-tune; its own 12-hour half-life discounts an old
regime four-fold a day and a four-day cap drops it entirely. A scoreboard
reset that blinded the sizer would have every arm quoting the minimum until
it had re-learned what it already knew. Evidence that is genuinely wrong is fixed
by re-scoring it (the one-time migrations in `db`), not by hiding it. See
`src/strategy/arms.rs` for the derivation of every default from the data.

## Learning the split

`safetrade ctl learn` (Telegram: `learn`) replaces the typed inventory target
with one the arms set. Each currency is for funding one side of a ladder --
USDT funds prlusdt's bids, USDC prlusdc's, PRL both books' asks -- so each
currency's **demand** is, over the sides that spend it, the best share its
arms have earned times what that side needs on hand for its ladder to rest at
full size (derived from the ladder settings: about a quarter of equity for a
prlusdt side, 4.5% for prlusdc, 2.3% for usdcusdt). Every demand is funded;
the rest of the account stays where the owner put it, at an **anchor** share
per currency that is re-read at every settled deposit or withdrawal. So a
deposit is absorbed as capital, not worked off, and the split can never move
more than the ladders' funding needs -- 64% of equity with every side at full
size, usually far less -- which is the bound on concentration, derived rather
than typed.

The split is re-derived every hour and reached by **drift**: the learned
program is a balanced one whose glide is re-anchored from the current
inventory over two evidence half-lives (a day), so the lean does the moving
and nothing ever crosses the spread to arrive. `status` shows the current
target; a change of a percent or more is announced. See
`src/strategy/allocation.rs`.

## The cycle: one book acquires, another releases

The arms size each arm on its own yield, which is right for an arm that stands
alone and wrong for a **leg that only exists to unlock another one**. Measured
over 3.7 days of scored maker fills, net of fees, against the reference mid five
minutes on:

| leg | net | on |
| --- | --- | --- |
| `prlusdc` buy | **+74 bp** | acquire below fair value |
| `prlusdc` sell | **+152 bp** | release above it |
| `prlusdt` sell | −17 bp | release at fair value |
| `prlusdt` buy | −22 bp | a one-tick book, swept |
| `usdcusdt` buy | −1 bp | the recycle leg |

Read as arms, `prlusdt`'s legs lose and the arms defund them — correct in
isolation, and it shuts the cycle's release valve. Read as cycles, both routes
pay: acquiring and releasing on the thin book is **+226 bp on the evidence**,
while acquiring on the thin book and releasing on the deep one is +44 bp before
its errors. So the thin book's own ask is always preferred and the deep book is
strictly an overflow, opened only when the whole round trip clears
`cycle_floor_bps` after every leg's standard error is charged. See
`src/strategy/cycle.rs`.

Three rules follow, and each one is gated on that same measured margin so it
lapses by itself if the edge goes:

- **`cycle_release`** floors the ask on an overflow venue the arms defunded, in
  proportion to how much worse it pays than the best one.
- **`cycle_churn`** quotes the acquiring book's two sides at **neutral size**,
  ignoring how far inventory sits from the program's target. A round trip on
  that book is churn, not drift: it ends the day at the same inventory either
  way, so throttling one side of it buys no convergence and forfeits the leg.
  Sizes only — the prices still lean, so every rung stays on the arm it was
  measured as.
- **`cycle_recycle`** crosses the stablecoin book to put the acquiring book's
  bids back in funds, charging the crossing against the round trip before
  anything is placed. It fires for either of two reasons: money the cycle just
  earned landed in the wrong currency, or capital is *already* in the wrong
  currency while the bids that earn sit starved.

`max_improve_ticks` and the spread floor still decide **where** a rung rests, and
so which arm it is a bet on. The cycle only decides whether the leg may rest at
all, and the arms still decide how much to stake on it.

### Why a directional target must not be expressed on the earning book

Inventory error does double duty — it skews prices *and* sizes, which is the
catch-up behaviour a TWAP needs. On the book that round-trips it is a trap.
Measured live 2026-09-27: a balanced program wanted 87% PRL
against 79% held, so at `inventory_size_skew = 1.0` the normalised error was
−0.915 and `ask_mult` was **0.085**. `prlusdc`'s asks rested at well under half the size
of its bids — on the sell leg measured at +152 bp, the best of the four.

> A market maker round-trips at the rate of its **smaller** side.

So the one book that pays was running at a twelfth of its capacity in order to
express a view about PRL. `cycle_churn` is the fix, and the view belongs
somewhere else:

**`cycle_leg_only`** (per market) says a book is a cycle leg, not a book we make
markets on. It then quotes for exactly two reasons — the release valve, and an
**execution** program (`accumulate`, `distribute`, `flatten`), because working a
quantity the owner asked for is not a bet on the spread. `prlusdt` is set this
way: 160,000/h of volume against the thin book's 3,300 makes it the right place
to move size and the wrong place to earn a spread.

```sh
# how a directional view is expressed from here on
safetrade ctl program accumulate 12000 --over 5h --limit-price 1.50
```

Without that flag "defunded" still meant the exchange minimum on every rung, on
both sides, cancelled and replaced for ever — 212 bids and 282 asks in two hours,
and the USDT tied up was the USDT `prlusdc`'s bids were starved of.

## TWAP mode: put it all in, and nothing else

```sh
safetrade ctl twap --over 6h              # or `twap 6h` on Telegram
```

One instruction — *buy everything, over six hours, and do not sell any of it* —
needs three things no existing program expressed together. Each of the three is a
live failure from 2026-09-27, when that instruction was assembled by hand out of
`accumulate` and `ctl set`.

**The target is derived, not typed.** `accumulate N` is a snapshot of one
moment's balances and price: a deposit lands, the price moves, and the number is
wrong with nothing to say so. A `twap` targets *everything the account could hold
if every unit of quote currency were spent*, recomputed from live balances every
loop, so a deposit, a withdrawal or a price move changes it by itself.
`target_qty` is kept only as the snapshot completion is judged against.

**It never sells.** Inventory error skews prices *and* sizes, which is the
catch-up behaviour a two-sided program wants — and on a buy-only job it is the bot
undoing the work. Live: a buy that ran ahead of its ramp on the thin book immediately rested an
ask for nearly all of the excess against the position it had just paid for.
`min_position` does not fix that, because it is a floor *under* inventory, so
every unit bought reopens room to sell one — the ask that appeared was exactly
`inventory - min_position`. So a buy-only program quotes no ask at all: not the
tight ladder's, not the deep ladder's, not the cycle's release valve. Ahead of
its ramp it rests nothing and waits, which is what a schedule is for.

**It is the only thing quoting.** `twap_only` stands every other book down
(`Halt::TwapOnly`) and shuts the cycle's valve, its churn exemption and its
stablecoin crossing. Without it the other books went on making markets with the
same money, and the stablecoin book crossed the spread to rebalance a split
nobody had asked it to hold. The flag is set by `ctl twap` and cleared by any
other program command, so the mode cannot outlive the program that wanted it.

Deposits are the point of the mode, so they are handled from the funding side: a
settled transfer of any currency **restarts** the running TWAP — a new program,
priced off the balances as they now stand, over a full window of its own. Deposit
at hour five of six and the new money gets six hours, not the hour that was left
of a clock it was not there for. It is done per base asset rather than per
deposited currency, because a twap is funded in the currency it spends: a USDT
deposit is filed under USDT and the program under PRL. Both PRL books work it —
`prlusdt` is a cycle leg, and an execution program suspends that, which is the
reason the deep book is kept.

The depth and participation caps and the volatility breaker all still apply.

### When the touch is not enough: the crossing leg

Passive bidding has a ceiling, and on this book it is low. Measured live
2026-09-27: the book is one tick wide, so joining the best bid puts us behind
whatever is already queued there, and we fill only when a seller comes through
the lot. That bought about 4% of the TWAP in forty minutes -- on pace for under
40% of it in six hours, with the rest simply not spent.

`max_improve_ticks` cannot fix it either: one tick better than the bid IS the
ask on a one-tick book, and `clamp_price` refuses to cross, so the improvement
has nowhere to go. And `allow_taker` did not apply to the ladder at all -- only
the stablecoin recycle ever read it.

So an execution program may cross, under conditions that are deliberately hard
to satisfy and each of which is a thing that would otherwise cost money
(`Engine::take_if_behind`, arithmetic in `taker_clip`):

- `allow_taker` for that market. Off everywhere by default.
- An **execution** program with a deadline. Without a schedule there is no
  "behind".
- A **`limit_price` on the program**. No limit, no crossing: "spend it as fast
  as you can" with no ceiling is how a thin ask side gets walked up.
- Behind by more than one full rung, so lateness of a few coins does not pay the
  spread.
- A price at or under the limit *and* within **`taker_max_bps`** of fair value
  (default 100). On a referenced book fair value comes from the deep book, so
  this is what refuses to pay prlusdc's 2-15% spread to fix a schedule.
- Inside the per-order notional cap, `max_order_pct_of_depth` of the side being
  taken, the participation cap, and the cash actually held.

It hits the touch exactly rather than reaching through it: a limit order at the
opposite touch crosses and fills there, and the depth cap is what decides how
far into the book one clip may eat. One clip per market per loop, so catching up
is a sequence of small crossings rather than one sweep. A TWAP that still cannot
keep up says so every loop once past its deadline.

## Risk controls

| Control                  | What it stops                                    |
| ------------------------ | ------------------------------------------------ |
| `max_improve_ticks`      | quoting through the touch and moving the price   |
| `allow_taker`            | crossing the spread                              |
| `max_position` / `min_position` | inventory leaving its bounds              |
| `max_order_notional`     | one oversized order                              |
| `max_order_pct_of_depth` | being most of a thin book                        |
| `max_participation`      | being most of the tape                           |
| `max_move`               | quoting into a flash move                        |
| `price_floor` / `price_ceiling` | quoting on bad data                       |
| `limit_price` on a program | paying above / selling below your number       |
| `cycle_floor_bps`        | opening a valve on a margin inside its noise     |
| `cycle_recycle_max_pct`  | one oversized crossing of the stablecoin book    |
| `cycle_recycle_trigger`  | paying the crossing to top up a nearly-full purse |
| `taker_max_bps`          | a crossing order paying a thin book's spread      |

The bot also strips its **own** resting size out of the book before reading it.
Without that it would treat its own bid as the market's best bid and ratchet up
against itself, one 1% tick at a time — precisely the price move it exists to
avoid.

## License

MIT. See [LICENSE](LICENSE).
