#!/usr/bin/env bash
# Alert if the bot stops ticking.
#
# Deliberately OUTSIDE the bot process. The two failures that matter most are
# the ones the bot cannot report itself: a unit that has died, and a process
# that is alive but wedged. Both look identical from in here -- the heartbeat
# simply stops advancing -- and both are invisible to anything the bot runs.
set -uo pipefail

# The bot links SQLite in, so the CLI is not implied by the bot working. If it
# is missing this script would read an empty heartbeat, alert once about that,
# and then stay silent for the rest of the month -- failing at its one job in
# the way least likely to be noticed.
if ! command -v sqlite3 >/dev/null; then
  logger -t safetrade-heartbeat "sqlite3 is not installed; heartbeat monitoring is DISABLED"
  exit 1
fi

DB=/opt/safetrade/data/safetrade.db
ENV=/opt/safetrade/etc/safetrade.env
STALE_SECONDS=${STALE_SECONDS:-600}
STATE=/var/lib/safetrade-heartbeat.state

now=$(date +%s)
last=$(sqlite3 "$DB" "SELECT value FROM settings WHERE key='last_seen_ms';" 2>/dev/null || echo "")
active=$(systemctl is-active safetrade 2>/dev/null || true)

if [ -z "$last" ]; then
  problem="no heartbeat recorded at all"
else
  age=$(( now - last/1000 ))
  if [ "$age" -gt "$STALE_SECONDS" ]; then
    problem="last tick was ${age}s ago (service is ${active})"
  fi
fi

if [ -z "${problem:-}" ]; then
  # Recovered: say so once, then stay quiet.
  if [ -f "$STATE" ]; then
    rm -f "$STATE"
    msg="✅ SafeTrade bot is ticking again."
  else
    exit 0
  fi
else
  # Only alert once per outage, not every five minutes.
  [ -f "$STATE" ] && exit 0
  touch "$STATE"
  msg="🔴 SafeTrade bot is not ticking: ${problem}. Check: journalctl -u safetrade -n 50"
fi

# shellcheck disable=SC1090
set -a; . "$ENV"; set +a
TOKEN="${TELEGRAM_BOT_TOKEN:-${TELEGRAM:-${TELGRAM:-}}}"
CHAT=$(sqlite3 "$DB" "SELECT value FROM settings WHERE key='telegram_chat_id';" 2>/dev/null || echo "")
[ -n "$TOKEN" ] && [ -n "$CHAT" ] && \
  curl -sS -m 20 -X POST "https://api.telegram.org/bot${TOKEN}/sendMessage" \
       -d "chat_id=${CHAT}" --data-urlencode "text=${msg}" >/dev/null || true
logger -t safetrade-heartbeat "$msg"
