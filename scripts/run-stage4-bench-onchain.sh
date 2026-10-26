#!/bin/bash
cd /workspace/rgb-arkade
B=state/bench-stage4
for k in 1 16; do echo "[$(date +%T)] merge $k"; demo/target/debug/ark-rgb-demo --bench $B merge $k > logs/stage4-bench-merge$k.log 2>&1; echo "rc=$?"; done
echo "[$(date +%T)] onchain 16"; demo/target/debug/ark-rgb-demo --bench $B onchain 16 > logs/stage4-bench-onchain16.log 2>&1; echo "rc=$?"
echo "[$(date +%T)] done"
