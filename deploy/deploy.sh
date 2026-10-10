#!/usr/bin/env bash
# Build and deploy the bot to the trading host.
#
# Builds on the server rather than cross-compiling: the box is small, but the
# local machine runs a newer glibc than Ubuntu 24.04, so a natively built
# binary would not run there. The first build takes a while; later ones are
# incremental.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -z "${SAFETRADE_HOST:-}" ] && [ -f "${ROOT}/.env" ] && { set -a; . "${ROOT}/.env"; set +a; }
HOST="${SAFETRADE_HOST:?set SAFETRADE_HOST in .env or the environment}"
REMOTE="root@${HOST}"
SRC="/opt/safetrade/src"

echo "==> syncing source to ${HOST}"
ssh "$REMOTE" "mkdir -p ${SRC} /opt/safetrade/bin /opt/safetrade/etc /opt/safetrade/data"
rsync -az --delete \
  --exclude target --exclude .git --exclude data --exclude '.env' --exclude '*.db*' \
  "${ROOT}/" "${REMOTE}:${SRC}/"

echo "==> building (release)"
# -j1 keeps peak memory inside what a 512MB box plus swap can take.
#
# Serialised under a lock, and the whole deploy shares it. Two deploys
# overlapping is not hypothetical: it has happened twice, and the second one
# sat on "Blocking waiting for file lock" while the first installed and
# restarted underneath it -- so the binary that ended up running was not the
# one the second deploy had just built, and nothing said so. The lock makes
# the second deploy wait for the first to finish rather than interleave with
# it.
ssh "$REMOTE" "
  flock /var/lock/safetrade-deploy.lock bash -s <<'BUILD'
set -e
cd ${SRC}
. \$HOME/.cargo/env
cargo build --release -j1
BUILD
"

echo "==> installing"
ssh "$REMOTE" "
  set -e
  install -m 0755 ${SRC}/target/release/safetrade /opt/safetrade/bin/safetrade
  install -m 0644 ${SRC}/deploy/safetrade.service /etc/systemd/system/safetrade.service
  install -m 0755 ${SRC}/deploy/heartbeat-check.sh /opt/safetrade/bin/heartbeat-check.sh
  install -m 0644 ${SRC}/deploy/safetrade-heartbeat.service /etc/systemd/system/safetrade-heartbeat.service
  install -m 0644 ${SRC}/deploy/safetrade-heartbeat.timer /etc/systemd/system/safetrade-heartbeat.timer
  # deploy/config.prod.toml, when present, is authoritative. It is gitignored
  # (it is your tuning, not the project's) but rsync carries it up anyway.
  #
  # This used to install config.example.toml only when nothing was there, to
  # avoid clobbering a config hand-tuned on the box. That left a fresh droplet
  # running the EXAMPLE -- one market, none of the tuning -- and meant a change
  # made to deploy/config.prod.toml never reached a host that already had
  # a file. Live tuning belongs in 'ctl set', which lives in the database and
  # survives this, so the file can safely be the local one.
  if [ -f ${SRC}/deploy/config.prod.toml ]; then
    install -m 0644 ${SRC}/deploy/config.prod.toml /opt/safetrade/etc/config.toml
  else
    [ -f /opt/safetrade/etc/config.toml ] || install -m 0644 ${SRC}/config.example.toml /opt/safetrade/etc/config.toml
  fi
  chown -R safetrade:safetrade /opt/safetrade/data /opt/safetrade/etc
  systemctl daemon-reload
  systemctl enable --now safetrade-heartbeat.timer
"

# Credentials are pushed over stdin so they never appear in argv or shell
# history on either machine.
if [ -f "${ROOT}/.env" ]; then
  echo "==> updating credentials"
  grep -E '^(SAFETRADE_API_KEY|SAFETRADE_API_SECRET|TELEGRAM_BOT_TOKEN|TELEGRAM|TELGRAM|TELEGRAM_CHAT_ID)=' "${ROOT}/.env" \
    | ssh "$REMOTE" "cat > /opt/safetrade/etc/safetrade.env && chmod 0600 /opt/safetrade/etc/safetrade.env && chown safetrade:safetrade /opt/safetrade/etc/safetrade.env"
fi

echo "==> done. Next:"
echo "     ssh ${REMOTE} 'systemctl enable --now safetrade'"
echo "     ssh ${REMOTE} 'journalctl -u safetrade -f'"
