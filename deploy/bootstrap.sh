#!/usr/bin/env bash
# Bring the trading host up, verify its prerequisites, and deploy.
#
# Safe to re-run: every step checks before it acts. It does NOT start the
# service or place orders -- it gets the machine and the binary ready and tells
# you what is left.
#
# The three checks below are not hygiene. Each one has already broken this
# deployment once:
#   * egress must be IPv4, because the exchange allowlists the v4 address and
#     glibc otherwise prefers v6, so every private call 401s as
#     `authz.apikey_untrusted_ip` -- which reads as a key problem, not a
#     routing one.
#   * the clock must be synced, because auth signs a millisecond nonce and a
#     few seconds of drift fails every private call as `authz.nonce_expired`.
#   * swap must be on, because a small droplet cannot link the release binary
#     without it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

say() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
die() { printf '\n\033[31mERROR: %s\033[0m\n' "$1" >&2; exit 1; }

[ -f "${ROOT}/.env" ] || die "no ${ROOT}/.env"
# shellcheck disable=SC1091
set -a; . "${ROOT}/.env"; set +a
: "${DIGITAL_OCEAN_API_KEY:?DIGITAL_OCEAN_API_KEY missing from .env}"
HOST="${SAFETRADE_HOST:?SAFETRADE_HOST missing from .env}"
DROPLET_ID="${SAFETRADE_DROPLET_ID:?SAFETRADE_DROPLET_ID missing from .env}"
REMOTE="root@${HOST}"

do_api() { curl -sS -H "Authorization: Bearer ${DIGITAL_OCEAN_API_KEY}" "$@"; }
droplet_field() {
  do_api "https://api.digitalocean.com/v2/droplets/${DROPLET_ID}" \
    | python3 -c "import json,sys;print(json.load(sys.stdin)['droplet']['$1'])"
}

say "droplet state"
STATUS="$(droplet_field status)"
printf '  status %s  size %s  memory %sMB  disk %sGB\n' \
  "$STATUS" "$(droplet_field size_slug)" "$(droplet_field memory)" "$(droplet_field disk)"

if [ "$STATUS" != "active" ]; then
  say "powering on"
  do_api -X POST -H "Content-Type: application/json" -d '{"type":"power_on"}' \
    "https://api.digitalocean.com/v2/droplets/${DROPLET_ID}/actions" >/dev/null
  for _ in $(seq 30); do
    STATUS="$(droplet_field status)"
    printf '  %s\n' "$STATUS"
    [ "$STATUS" = "active" ] && break
    sleep 10
  done
  [ "$STATUS" = "active" ] || die "droplet did not come up"
fi

say "waiting for ssh"
SSH_OK=""
for _ in $(seq 24); do
  if ssh -o StrictHostKeyChecking=accept-new -o BatchMode=yes -o ConnectTimeout=8 \
       "$REMOTE" true 2>/dev/null; then
    SSH_OK=1; echo "  up"; break
  fi
  sleep 10
done
[ -n "$SSH_OK" ] || die "ssh never came up on ${HOST}"

say "host prerequisites"
ssh -o BatchMode=yes "$REMOTE" 'bash -s' <<'REMOTE_CHECKS'
set -e

# --- package index ---
# Refreshed once, before anything installs. A stale index on a fresh image
# makes an install fail with a 404 on a package that plainly exists.
apt-get update -qq >/dev/null 2>&1 || true

# --- swap ---
if [ "$(swapon --show --noheadings | wc -l)" -eq 0 ]; then
  echo "  swap: off, enabling"
  [ -f /swapfile ] || { fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap -q /swapfile; }
  swapon /swapfile
fi
echo "  swap: $(free -h | awk '/Swap/{print $2}')"
echo "  ram:  $(free -h | awk '/Mem/{print $2}')"

# --- IPv4 egress ---
# The stock gai.conf ships every rule COMMENTED OUT. A grep for the pattern
# alone matches those comments and concludes the rule is present, which is how
# this check passed on a host that had no rule at all.
if ! grep -Eqs '^[[:space:]]*precedence[[:space:]]+::ffff:0:0/96' /etc/gai.conf; then
  echo "  gai.conf: no ACTIVE IPv4 precedence rule, writing it"
  printf 'precedence ::ffff:0:0/96  100\n' >> /etc/gai.conf
fi
echo "  gai.conf: $(grep -Ec '^[[:space:]]*precedence[[:space:]]+::ffff:0:0/96' /etc/gai.conf) active rule(s)"
EGRESS="$(curl -s -m 20 https://safetrade.com/cdn-cgi/trace | sed -n 's/^ip=//p')"
echo "  egress:  ${EGRESS}"
case "$EGRESS" in
  *:*) echo "  !! egress is IPv6 -- the exchange will reject the API key"; exit 1 ;;
  "")  echo "  !! could not determine egress IP (Cloudflare may be challenging)" ;;
esac

# --- Rust ---
# A fresh droplet has no toolchain, and the deploy builds on the host. Without
# this the deploy fails at `/root/.cargo/env: No such file or directory`, which
# reads as a broken deploy script rather than a missing prerequisite.
if [ ! -x /root/.cargo/bin/cargo ]; then
  echo "  rust: not installed, installing"
  curl -sSf --proto '=https' --tlsv1.2 https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path >/dev/null
fi
# shellcheck disable=SC1091
. /root/.cargo/env
echo "  rust:    $(cargo --version 2>/dev/null || echo MISSING)"

# --- a linker for the release build ---
command -v cc >/dev/null || { echo "  installing build-essential"; apt-get install -y -qq build-essential >/dev/null; }
echo "  cc:      $(cc --version | head -1)"

# --- the service account ---
# The unit runs as `safetrade`, and the deploy chowns its data and config to
# that user. On a fresh droplet neither exists, so the install dies at
# `chown: invalid user`, having already built for thirteen minutes.
if ! id -u safetrade >/dev/null 2>&1; then
  echo "  user: creating safetrade"
  useradd --system --home-dir /opt/safetrade --shell /usr/sbin/nologin safetrade
fi
mkdir -p /opt/safetrade/bin /opt/safetrade/etc /opt/safetrade/data
chown -R safetrade:safetrade /opt/safetrade/data /opt/safetrade/etc
echo "  user:    $(id safetrade)"

# --- the heartbeat monitor's dependency ---
command -v sqlite3 >/dev/null || { echo "  installing sqlite3 (heartbeat monitor needs it)"; apt-get install -y -qq sqlite3 >/dev/null; }
echo "  sqlite3: $(command -v sqlite3)"

# --- clock ---
timedatectl set-ntp true 2>/dev/null || true
# Makes `time-sync.target` mean something: without a unit ordered before it,
# the target is reached immediately and the bot can start on an unsynced clock.
systemctl enable --now systemd-time-wait-sync.service >/dev/null 2>&1 || true
echo "  clock:   $(timedatectl show -p NTPSynchronized --value) (NTPSynchronized)"

echo "  disk:    $(df -h / | awk 'NR==2{print $4" free of "$2}')"
REMOTE_CHECKS

say "deploying"
"${ROOT}/deploy/deploy.sh"

say "ready"
cat <<EOF
  Nothing is running yet and no orders are resting. To go live:

    ssh ${REMOTE} 'systemctl enable --now safetrade'
    ssh ${REMOTE} 'journalctl -u safetrade -f'

  First, confirm the account and market look right:

    ssh ${REMOTE} 'cd /opt/safetrade && set -a && . etc/safetrade.env && set +a && bin/safetrade --config etc/config.toml status'

  Then send /start to the Telegram bot to bind your chat.
EOF
