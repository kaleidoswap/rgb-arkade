#!/bin/sh
# Stage 6 regressions (rgb-lib Stage 6 API, arkd v0.9.15-rgb4 unchanged). Sequential to avoid the electrum
# "Invalid indexer" flake; a miner loop runs during stage2-refresh (CSV needs MTP to advance).
R=${R:-/workspace/rgb-arkade}; cd $R/demo
B=./target/debug/ark-rgb-demo; L=../logs; P=$L/stage6-regress-progress.log
run() { name=$1; shift; echo "[$(date +%T)] start $name" >> $P; $B "$@" > $L/stage6-regress-$name.log 2>&1; rc=$?; echo EXIT $rc >> $L/stage6-regress-$name.log; echo "[$(date +%T)] $name rc=$rc" >> $P; }
echo "[$(date +%T)] start stage5 (sweep recovery, 720 s offline)" >> $P
$R/scripts/run-stage5.sh 720 > $L/stage6-regress-stage5.log 2>&1
D5=$(ls -dt $R/state/run-*-stage5-recovery | head -1)
echo "[$(date +%T)] stage5 finished ($D5): $(grep -h 'RESULT' $D5/return.log 2>/dev/null | tail -1)" >> $P
run stage1 --stage1
run stage0-variantA
run stage0-variantB --onchain-genesis
( while true; do node $R/arkade-regtest/regtest.mjs mine 1 >/dev/null 2>&1; sleep 15; done ) & MINER=$!
run stage2-refresh --stage2-refresh
kill $MINER
run stage2-exit --stage2-exit
run stage2-neg --stage2-neg
run stage3-offline-proof --stage1 --offline-proof
echo "[$(date +%T)] start stage4-offline2w" >> $P
cd $R && scripts/run-stage4-offline2w.sh > logs/stage6-regress-stage4-offline2w.log 2>&1
echo "[$(date +%T)] stage4-offline2w finished" >> $P
echo ALLDONE >> $P
