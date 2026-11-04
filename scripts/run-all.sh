#!/bin/bash
# Regression suite, sequentially (parallel runs trip electrum's "Invalid indexer").
# Usage: scripts/run-all.sh [quick|all|cponly|stock|full]
#   quick   full patch set, no time-scaled scenarios (fast profile: ~20 min; seconds profile: ~1 h 30)
#   all     quick + the time-scaled scenarios (needs the seconds profile: scripts/regtest-up.sh clean)
#   cponly  checkpoint-only lane (needs make regtest-up-cponly)
#   stock   unmodified arkd (needs REGTEST_PROFILE=stock scripts/regtest-up.sh clean); stock-lane
#           restarts arkd as its last step
#   full    every lane in turn, restarting the stack CLEAN with each profile: fast -> quick,
#           cponly, stock, then the seconds profile -> time-scaled (several hours)
# The default is all. Summary in logs/run-all.log; one log per scenario in logs/scenario-<name>.log.
set -u
source "$(dirname "$0")/lib.sh"
S=$(dirname "$0")/run-scenario.sh
P=$R/logs/run-all.log
MODE=${1:-all}
FAILED=0
run() {
  echo "[$(date +%T)] start $*" >> "$P"
  if "$S" "$@" >> "$P" 2>&1; then echo "[$(date +%T)] PASS $*" >> "$P"; else echo "[$(date +%T)] FAIL $*" >> "$P"; FAILED=$((FAILED+1)); fi
}
image() { sed -n 's/^ARKD_IMAGE=//p' "$R/arkade-regtest/.env"; }
need_image() { # the running profile must use this arkd image
  if [ "$(image)" != "$1" ]; then echo "[$(date +%T)] SKIP $2: the stack runs $(image), not $1" >> "$P"; FAILED=$((FAILED+1)); return 1; fi
}
stack() { # restart the stack clean on a profile (empty = seconds profile)
  echo "[$(date +%T)] regtest-up clean, profile ${1:-default}" >> "$P"
  REGTEST_PROFILE=$1 "$R/scripts/regtest-up.sh" clean >> "$P" 2>&1 || { echo "[$(date +%T)] FAIL regtest-up $1" >> "$P"; exit 1; }
}

quick() {
  run basic
  run basic --offline-proof
  run basic-onchain-genesis
  run basic-onchain-genesis --offline-proof
  run offchain-transfer
  run offchain-transfer --offline-proof
  run refresh
  run refresh --offline-proof
  run collab-exit
  run collab-exit --offline-proof
  run forfeit-negatives
  run multi-user
  run failed-batch
  run fraud                 # needs tools/arkdb (scripts/setup.sh builds it)
  run multihop-exit         # app flows: multi-hop asset, unilateral exit, on-chain spend
  run swap
}
timescaled() {
  run sweep-recovery 720
  run long-offline
  run exit-watchtower
}
cponly() {
  need_image arkd:rgb-cponly cponly || return 0
  run checkpoint-renew
  run checkpoint-swap
  run checkpoint-offline
}
stock() {
  need_image ghcr.io/arkade-os/arkd:v0.9.15 stock || return 0
  run basic
  run opret-probe
  run stock-lane            # last: it restarts arkd
}

echo "[$(date +%T)] run-all ($MODE) root=$R" >> "$P"
case "$MODE" in
  quick) quick ;;
  all) quick; timescaled ;;
  cponly) cponly ;;
  stock) stock ;;
  full)
    stack fast; quick
    stack cponly; cponly
    stack stock; stock
    stack ""; timescaled ;;
  *) echo "usage: run-all.sh [quick|all|cponly|stock|full]"; exit 2 ;;
esac
echo "[$(date +%T)] ALL DONE ($MODE), failed=$FAILED" >> "$P"
exit $FAILED
