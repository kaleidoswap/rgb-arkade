#!/bin/bash
# Start the prototype web app: bring up the regtest stack if arkd does not answer, build the demo,
# then serve the app on http://127.0.0.1:${PORT:-8080}. Usage: scripts/app.sh [port]
set -eu
source "$(dirname "$0")/lib.sh"
PORT=${1:-${PORT:-8080}}
unpause_arkd
if ! curl -s -m 3 -o /dev/null "$ARKD/v1/info"; then
  log "arkd not reachable: starting the regtest stack"
  "$R/scripts/regtest-up.sh"
fi
grep -q '^ARKD_VTXO_TREE_EXPIRY=[0-9]\{4,\}' "$R/arkade-regtest/.env" ||
  log "WARNING: fast profile. arkd reports VTXO expiry in seconds even when it is in blocks, so after ~8 min VTXOs look expired: use scripts/regtest-up.sh clean for the app"
(cd "$R/demo" && cargo build)
trap unpause_arkd EXIT
log "app: http://127.0.0.1:$PORT  (Ctrl-C to stop)"
"$BIN" app "$PORT"
