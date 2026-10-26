#!/bin/bash
cd /workspace/rgb-arkade
B=state/bench-stage4
echo "[$(date +%T)] hops 16 (--pause)"; demo/target/debug/ark-rgb-demo --bench $B hops 16 --pause > logs/stage4-bench-hops16.log 2>&1; echo "rc=$?"
docker unpause arkd 2>/dev/null
echo "[$(date +%T)] refresh 8 (--pause)"; demo/target/debug/ark-rgb-demo --bench $B refresh 8 --pause > logs/stage4-bench-refresh8.log 2>&1; echo "rc=$?"
docker unpause arkd 2>/dev/null
echo "[$(date +%T)] done"
