#!/bin/bash
# Start the arkade-regtest stack (bitcoind, electrs/esplora, arkd:rgb, arkd-wallet) plus the offline
# mempool pools mirror, and wait until arkd answers. Usage: scripts/regtest-up.sh [clean]
# REGTEST_PROFILE=fast uses patches/arkade-regtest.fast.env (delays in blocks); switching profile needs clean.
set -eu
source "$(dirname "$0")/lib.sh"
cd "$R/arkade-regtest"
cp "$R/patches/arkade-regtest${REGTEST_PROFILE:+.$REGTEST_PROFILE}.env" .env
[ "${1:-}" = clean ] && node regtest.mjs clean
node regtest.mjs start --profile ark > "$R/logs/regtest-start.log" 2>&1 &
START=$!
# the mempool container needs pools-v2.json without internet: serve pools/ once the network exists
for _ in $(seq 120); do docker network inspect arkade-regtest_default >/dev/null 2>&1 && break; sleep 2; done
docker rm -f rgbarkade-pools >/dev/null 2>&1 || true
docker run -d --name rgbarkade-pools --network arkade-regtest_default --network-alias pools-mirror \
  -v "$R/pools:/usr/share/nginx/html:ro" nginx:alpine >/dev/null
wait $START
curl -s "$ARKD/v1/info" | head -c 200; echo
log "regtest stack up (log: logs/regtest-start.log)"
