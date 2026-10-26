# shellcheck shell=bash
# Common helpers for the scripts. Source it; it sets R (project root), BIN (demo binary), and helpers.
R=${RGB_ARKADE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
export RGB_ARKADE_DIR=$R
BIN=${BIN:-${CARGO_TARGET_DIR:-$R/demo/target}/debug/ark-rgb-demo}
ADMIN=${ARKD_ADMIN:-localhost:7071}
ARKD=${ARKD_URL:-localhost:7070}
DEFAULT_EXPIRY=${DEFAULT_EXPIRY:-604672}   # the regtest .env value (~7 days)
mkdir -p "$R/state" "$R/logs"

log() { echo "[$(date +%H:%M:%S)] $*"; }

# arkd keeps settings in its DB; change the batch expiry through the admin API.
# Rounds already in progress keep the old value, so wait for them to finish.
setexp() {
  curl -s -X POST -H 'Content-Type: application/json' "$ADMIN/v1/admin/settings" \
    -d "{\"settings\":{\"vtxoTreeExpiry\":\"$1\"}}" >/dev/null
  log "vtxoTreeExpiry=$1"
  [ "${2:-}" = "wait" ] && sleep 45
  return 0
}

# Background miner (one block every $1 s); CSV/MTP-based timelocks need the chain to advance.
MINER=""
start_miner() { ( while true; do node "$R/arkade-regtest/regtest.mjs" mine 1 >/dev/null 2>&1; sleep "${1:-10}"; done ) & MINER=$!; }
stop_miner() { [ -n "$MINER" ] && kill "$MINER" 2>/dev/null; MINER=""; }

# Make sure arkd is not left paused by an offline-proof run.
unpause_arkd() { curl -s -m 3 -o /dev/null "$ARKD/v1/info" || docker unpause arkd >/dev/null 2>&1 || true; }

waitts() { while [ "$(date +%s)" -lt "$1" ]; do sleep 5; done; }
