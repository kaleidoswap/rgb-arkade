#!/bin/bash
# Out-of-band payload size and cold validation time (consignment + Ark proof), see docs/test-results.md.
# Usage: scripts/bench.sh [out_dir]   (default state/bench). Rows go to <out_dir>/<phase>.jsonl,
# then bench/analyze.py fits them and bench/usdt_extrapolate.py extrapolates (estimates).
set -u
source "$(dirname "$0")/lib.sh"
B=${1:-$R/state/bench}; mkdir -p "$B"
cd "$R/demo"
trap unpause_arkd EXIT
for spec in "hops 16 --pause" "refresh 8 --pause" "merge 1" "merge 16" "onchain 16"; do
  set -- $spec
  log "bench $*"; "$BIN" bench "$B" "$@" > "$R/logs/bench-$1-$2.log" 2>&1; log "rc=$?"
  unpause_arkd
done
BENCH_DIR=$B python3 "$R/bench/analyze.py" && BENCH_DIR=$B python3 "$R/bench/usdt_extrapolate.py"
