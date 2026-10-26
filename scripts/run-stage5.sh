#!/bin/bash
# Stage 5 sweep recovery. Usage: run-stage5.sh <min_offline_seconds>
# Scale: vtxoTreeExpiry 600 s (arkd minimum is 512 s) stands for a 7-day batch, so 5400 s offline ~ 63 days (> 2 months).
set -u
R=${R:-/workspace/rgb-arkade}; BIN=$R/demo/target/debug/ark-rgb-demo
D=$R/state/run-$(date +%s)-stage5-recovery; mkdir -p $D
log(){ echo "[$(date +%H:%M:%S)] $*"; }
setexp(){ curl -s -X POST -H 'Content-Type: application/json' localhost:7071/v1/admin/settings -d "{\"settings\":{\"vtxoTreeExpiry\":\"$1\"}}"; echo; }
log "dir $D"; log "set expiry 600"; setexp 600; sleep 45  # let rounds already in progress (old expiry) finish
( while true; do node $R/arkade-regtest/regtest.mjs mine 1 >/dev/null 2>&1; sleep 10; done ) & MINER=$!
$BIN --stage5-setup $D > $D/setup.log 2>&1; RC=$?
log "setup rc=$RC; restore expiry 604672"; setexp 604672
[ $RC -ne 0 ] && { kill $MINER; log "SETUP FAILED"; exit 1; }
T0=$(python3 -c "import json;print(json.load(open('$D/scenario.json'))['t_offline'])")
log "alice offline (process exited); no watchtower"
while [ $(date +%s) -lt $((T0+$1)) ]; do sleep 10; done
V=$(grep -o 'alice_vtxo = [0-9a-f]*:[0-9]*' $D/setup.log | cut -d' ' -f3)
for i in $(seq 60); do curl -s "localhost:7070/v1/indexer/vtxos?outpoints=$V" | grep -q '"isSwept":true' && break; sleep 10; done
log "arkd reports $V swept (after ${i}0 s extra); wait 30 s for the sweep tx to confirm"; sleep 30
log "alice returns"; $BIN --stage5-return $D > $D/return.log 2>&1; log "return rc=$?"
kill $MINER; curl -s -o /dev/null localhost:7070/v1/info || docker unpause arkd
log "DONE"
