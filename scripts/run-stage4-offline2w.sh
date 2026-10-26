#!/bin/bash
# Stage 4 / A: 2-weeks-offline simulation (scaled: expiry 2048 s = 28 days, window 1024 s = 14 days)
set -u
R=${R:-/workspace/rgb-arkade}
BIN=$R/demo/target/debug/ark-rgb-demo
D=$R/state/run-$(date +%s)-stage4-offline2w
mkdir -p $D
cd $R/demo
log(){ echo "[$(date +%H:%M:%S)] $*"; }
setexp(){ curl -s -X POST -H 'Content-Type: application/json' localhost:7071/v1/admin/settings -d "{\"settings\":{\"vtxoTreeExpiry\":\"$1\"}}"; echo; }
j(){ python3 -c "import json,sys;d=json.load(open('$D/scenario.json'));print(d$1)"; }
waitts(){ while [ $(date +%s) -lt $1 ]; do sleep 5; done; }
[ -d $R/watchtowerd ] && mv $R/watchtowerd $R/watchtowerd.prev-$(date +%s)
mkdir -p $R/watchtowerd/A $R/watchtowerd/B
log "dir $D"
log "set expiry 2048"; setexp 2048; sleep 45  # let rounds started under the old expiry finish
( while true; do node $R/arkade-regtest/regtest.mjs mine 1 >/dev/null 2>&1; sleep 10; done ) & MINER=$!
nohup $BIN --watchtowerd $R/watchtowerd/A > $D/wtd-A-1.log 2>&1 & WA=$!
nohup $BIN --watchtowerd $R/watchtowerd/B > $D/wtd-B.log 2>&1 & WB=$!
log "miner $MINER wtdA $WA wtdB $WB"
$BIN --offline2w-setup $D > $D/setup.log 2>&1; RC=$?
log "setup rc=$RC; restore expiry 604672"; setexp 604672
if [ $RC -ne 0 ]; then kill $MINER $WA $WB; log "SETUP FAILED"; exit 1; fi
T0=$(date +%s)
log "users offline. wallets processes exited."
sleep 60; log "kill watchtowerd A (crash test)"; kill $WA
sleep 60; nohup $BIN --watchtowerd $R/watchtowerd/A > $D/wtd-A-2.log 2>&1 & WA=$!; log "restarted wtd A pid $WA"
sleep 30; log "kill watchtowerd B for good (watchtower-down test)"; kill $WB
AOFF=$(j "['alice']['t_offline']"); waitts $((AOFF+1040))
log "alice returns"; $BIN --offline2w-return alice $D > $D/return-alice.log 2>&1; log "alice rc=$?"
EEXP=$(j "['erin']['expires_at']"); PT=$((EEXP-400-25))
if [ $(date +%s) -lt $PT ]; then waitts $PT; log "pause arkd (indexer down around erin threshold)"; docker pause arkd; sleep 90; docker unpause arkd; log "arkd unpaused"; else log "too late for arkd pause test"; fi
waitts $((EEXP+30))
for i in $(seq 1 60); do grep -q '"done-onchain"' $R/watchtowerd/A/status/*.json 2>/dev/null && break; sleep 10; done
log "erin returns (after her batch expiry)"; $BIN --offline2w-return erin $D > $D/return-erin.log 2>&1; log "erin rc=$?"
FEXP=$(j "['frank']['expires_at']"); waitts $((FEXP+150))
for i in 1 2 3 4 5 6; do log "frank returns try $i"; $BIN --offline2w-return frank $D > $D/return-frank.log 2>&1 && { log "frank rc=0"; break; }; sleep 60; done
kill $MINER $WA 2>/dev/null
cp -r $R/watchtowerd $D/watchtowerd-final
log "ALL DONE"
