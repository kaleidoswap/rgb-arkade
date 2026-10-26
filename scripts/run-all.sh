#!/bin/bash
# Regression suite: every scenario, sequentially (parallel runs trip electrum's "Invalid indexer").
# Usage: scripts/run-all.sh [quick]   quick = skip the long, time-scaled scenarios (~1 h 30 instead of ~3 h)
# Summary in logs/run-all.log; one log per scenario in logs/scenario-<name>.log.
set -u
source "$(dirname "$0")/lib.sh"
S=$(dirname "$0")/run-scenario.sh
P=$R/logs/run-all.log
QUICK=$([ "${1:-}" = quick ] && echo 1 || echo 0)
FAILED=0
run() {
  echo "[$(date +%T)] start $*" >> "$P"
  if "$S" "$@" >> "$P" 2>&1; then echo "[$(date +%T)] PASS $1" >> "$P"; else echo "[$(date +%T)] FAIL $1" >> "$P"; FAILED=$((FAILED+1)); fi
}
echo "[$(date +%T)] run-all (quick=$QUICK) root=$R" >> "$P"
run basic
run basic-onchain-genesis
run offchain-transfer
run offchain-transfer --offline-proof
run refresh
run collab-exit
run forfeit-negatives
run multi-user
run failed-batch
run fraud                 # needs tools/arkdb (scripts/setup.sh builds it)
run multihop-exit          # app flows: multi-hop asset, unilateral exit, on-chain spend
if [ "$QUICK" = 0 ]; then
  run sweep-recovery 720
  run long-offline
  run exit-watchtower
fi
echo "[$(date +%T)] ALL DONE, failed=$FAILED" >> "$P"
exit $FAILED
