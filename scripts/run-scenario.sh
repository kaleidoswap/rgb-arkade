#!/bin/bash
# Run one scenario against the running regtest stack and print its RESULT line.
# Usage: scripts/run-scenario.sh <name> [args] [--offline-proof]
#   simple scenarios: basic, basic-onchain-genesis, offchain-transfer, refresh, collab-exit,
#                     forfeit-negatives, multi-user, failed-batch, fraud, verify-recovery <dir>
#   orchestrated:     exit-watchtower              (short expiry, ~30 min)
#                     long-offline                 (watchtowerd, scaled 2 weeks offline, ~40 min)
#                     sweep-recovery [offline_s]   (offline past the sweep, default 720 s, ~20 min)
# Logs: logs/scenario-<name>.log (+ the run dir under state/).
set -u
source "$(dirname "$0")/lib.sh"
NAME=${1:?usage: run-scenario.sh <name> [args]}; shift
L=$R/logs/scenario-$NAME.log
cd "$R/demo"
trap 'stop_miner; unpause_arkd' EXIT

case "$NAME" in
  exit-watchtower)
    setexp 1024 wait
    "$BIN" scenario exit-watchtower "$@" > "$L" 2>&1; RC=$?
    setexp "$DEFAULT_EXPIRY" ;;

  long-offline)
    # scale: expiry 2048 s = 28 days, offline window 1024 s = 14 days
    D=$R/state/run-$(date +%s)-long-offline; mkdir -p "$D"
    W=$R/watchtowerd; [ -d "$W" ] && mv "$W" "$W.prev-$(date +%s)"; mkdir -p "$W/A" "$W/B"
    j() { python3 -c "import json;d=json.load(open('$D/scenario.json'));print(d$1)"; }
    { log "dir $D"; setexp 2048 wait; start_miner 10
      nohup "$BIN" watchtowerd "$W/A" > "$D/wtd-A-1.log" 2>&1 & WA=$!
      nohup "$BIN" watchtowerd "$W/B" > "$D/wtd-B.log" 2>&1 & WB=$!
      "$BIN" scenario long-offline-setup "$D" > "$D/setup.log" 2>&1; RC=$?
      setexp "$DEFAULT_EXPIRY"
      if [ $RC -ne 0 ]; then kill $WA $WB; log "SETUP FAILED"; exit 1; fi
      sleep 60; log "kill watchtowerd A (crash test)"; kill $WA
      sleep 60; nohup "$BIN" watchtowerd "$W/A" > "$D/wtd-A-2.log" 2>&1 & WA=$!; log "restarted watchtowerd A"
      sleep 30; log "kill watchtowerd B for good (watchtower-down control)"; kill $WB
      waitts $(( $(j "['alice']['t_offline']") + 1040 ))
      log "alice returns"; "$BIN" scenario long-offline-return alice "$D" > "$D/return-alice.log" 2>&1; RA=$?
      EEXP=$(j "['erin']['expires_at']"); PT=$((EEXP-400-25))
      if [ "$(date +%s)" -lt $PT ]; then waitts $PT; log "pause arkd (indexer down around erin's threshold)"; docker pause arkd; sleep 90; docker unpause arkd; fi
      waitts $((EEXP+30))
      for _ in $(seq 60); do grep -q '"done-onchain"' "$W"/A/status/*.json 2>/dev/null && break; sleep 10; done
      log "erin returns"; "$BIN" scenario long-offline-return erin "$D" > "$D/return-erin.log" 2>&1; RE=$?
      waitts $(( $(j "['frank']['expires_at']") + 150 ))
      RF=1; for i in 1 2 3 4 5 6; do log "frank returns (try $i)"; "$BIN" scenario long-offline-return frank "$D" > "$D/return-frank.log" 2>&1 && { RF=0; break; }; sleep 60; done
      kill $WA 2>/dev/null
      grep -h "RESULT" "$D"/return-*.log
      RC=$(( RA || RE || RF ))
    } > "$L" 2>&1 ;;

  sweep-recovery)
    # scale: expiry 600 s (arkd minimum 512 s) stands for a 7-day batch
    OFF=${1:-720}
    D=$R/state/run-$(date +%s)-sweep-recovery; mkdir -p "$D"
    { log "dir $D"; setexp 600 wait; start_miner 10
      "$BIN" scenario sweep-recovery-setup "$D" > "$D/setup.log" 2>&1; RC=$?
      setexp "$DEFAULT_EXPIRY"
      [ $RC -ne 0 ] && { log "SETUP FAILED"; exit 1; }
      T0=$(python3 -c "import json;print(json.load(open('$D/scenario.json'))['t_offline'])")
      log "alice offline, no watchtower"; waitts $((T0+OFF))
      V=$(grep -o 'alice_vtxo = [0-9a-f]*:[0-9]*' "$D/setup.log" | cut -d' ' -f3)
      for _ in $(seq 60); do curl -s "$ARKD/v1/indexer/vtxos?outpoints=$V" | grep -q '"isSwept":true' && break; sleep 10; done
      log "arkd reports $V swept; wait 30 s for the sweep tx to confirm"; sleep 30
      log "alice returns"; "$BIN" scenario sweep-recovery-return "$D" > "$D/return.log" 2>&1; RC=$?
      grep -h "RESULT" "$D/return.log"
      log "re-verify the recovery proof from chain data"; "$BIN" scenario verify-recovery "$D" > "$D/verify.log" 2>&1 || RC=1
      grep -h "RESULT" "$D/verify.log"
    } > "$L" 2>&1 ;;

  refresh)
    start_miner 15   # CSV needs MTP to advance during the exit
    "$BIN" scenario refresh "$@" > "$L" 2>&1; RC=$? ;;

  *)
    "$BIN" scenario "$NAME" "$@" > "$L" 2>&1; RC=$? ;;
esac

grep -h "RESULT" "$L" | tail -3
log "$NAME rc=$RC (log $L)"
exit $RC
