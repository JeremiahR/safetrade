#!/usr/bin/env bash
# Pull the live database down for local reporting.
#
# SQLite in WAL mode keeps recent writes in a -wal sidecar, so copying only the
# .db file would silently lose the newest rows -- exactly the ones you want to
# look at. `.backup` asks SQLite for a consistent snapshot instead.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -z "${SAFETRADE_HOST:-}" ] && [ -f "${ROOT}/.env" ] && { set -a; . "${ROOT}/.env"; set +a; }
HOST="${SAFETRADE_HOST:?set SAFETRADE_HOST in .env or the environment}"
OUT="${1:-./data/remote.db}"

mkdir -p "$(dirname "$OUT")"
echo "==> snapshotting remote database"
ssh "root@${HOST}" "sqlite3 /opt/safetrade/data/safetrade.db \".backup '/tmp/snap.db'\""
scp -q "root@${HOST}:/tmp/snap.db" "$OUT"
ssh "root@${HOST}" "rm -f /tmp/snap.db"

echo "==> wrote ${OUT}"
echo "     safetrade report --db ${OUT}"
