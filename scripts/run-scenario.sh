#!/bin/bash
# Run one scenario against the running regtest stack and print its RESULT line.
# Usage: scripts/run-scenario.sh <name> [args] [--offline-proof]
#   simple scenarios: basic, basic-onchain-genesis, offchain-transfer, refresh, collab-exit,
#                     forfeit-negatives, multi-user, failed-batch, fraud, multihop-exit,
#                     verify-recovery <dir>
#   orchestrated:     exit-watchtower              (short expiry, ~30 min)
#                     long-offline                 (watchtowerd, scaled 2 weeks offline, ~40 min)
#                     sweep-recovery [offline_s]   (offline past the sweep, default 720 s, ~20 min)
#                     delegate-renewal [days]      (profile cponly-seconds: delegate renews ~60 scaled days, ~3 h)
# Logs: logs/scenario-<name>.log, or scenario-<name>-offline-proof.log (+ the run dir under state/).
set -u
source "$(dirname "$0")/lib.sh"
NAME=${1:?usage: run-scenario.sh <name> [args]}; shift
L=$R/logs/scenario-$NAME.log
case " $* " in *" --offline-proof "*) L=$R/logs/scenario-$NAME-offline-proof.log ;; esac
cd "$R/demo"
trap 'stop_miner; unpause_arkd' EXIT

case "$NAME" in exit-watchtower|long-offline|sweep-recovery|delegate-renewal|threshold-delegate)
  grep -q '^ARKD_VTXO_TREE_EXPIRY=[0-9]\{4,\}' "$R/arkade-regtest/.env" ||
    { echo "$NAME needs the default (seconds) profile: scripts/regtest-up.sh clean"; exit 2; } ;;
esac

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

  delegate-renewal)
    # scale: expiry 1024 s = 7 days; the owners stay offline DAYS (default 60) scaled days.
    # Needs arkd master + B only: REGTEST_PROFILE=cponly-seconds scripts/regtest-up.sh clean
    DAYS=${1:-60}
    D=$R/state/run-$(date +%s)-delegate-renewal; mkdir -p "$D"
    W=$R/watchtowerd; [ -d "$W" ] && mv "$W" "$W.prev-$(date +%s)"; mkdir -p "$W/A" "$W/B"
    j() { python3 -c "import json;d=json.load(open('$D/scenario.json'));print(d$1)"; }
    nlines() { [ -f "$1" ] && wc -l < "$1" || echo 0; }
    { log "dir $D"; setexp 1024 wait; start_miner 10
      nohup "$BIN" watchtowerd "$W/A" > "$D/wtd-A.log" 2>&1 & WA=$!
      nohup "$BIN" watchtowerd "$W/B" > "$D/wtd-B.log" 2>&1 & WB=$!
      # Rita's delegate reaches arkd through this proxy; killing it = the operator stops cosigning
      nohup "$BIN" proxy 127.0.0.1:7080 "${ARKD#http://}" > "$D/proxy.log" 2>&1 & PX=$!
      RGB_ARKADE_DELEGATE_PROXY_URL=http://localhost:7080 "$BIN" scenario delegate-renewal-setup "$D" > "$D/setup.log" 2>&1; RS=$?
      if [ $RS -ne 0 ]; then kill $WA $WB $PX; setexp "$DEFAULT_EXPIRY"; log "SETUP FAILED"; grep -h RESULT "$D/setup.log"; exit 1; fi
      nohup "$BIN" delegated "$D/delegate" > "$D/delegated.log" 2>&1 & DG=$!
      log "delegated pid $DG; wait for Rita's first renewal"
      until [ "$(nlines "$D/delegate/out/rita/metrics.jsonl")" -ge 1 ]; do sleep 10; done
      log "operator stops cosigning Rita's delegate (proxy killed)"; kill $PX
      for _ in $(seq 240); do grep -q '"done-onchain"' "$W"/A/status/rita-1.json 2>/dev/null && break; sleep 10; done
      log "rita returns"; "$BIN" scenario delegate-renewal-return rita "$D" > "$D/return-rita.log" 2>&1; RR=$?
      T0=$(j "['olivia']['t_offline']"); waitts $(( T0 + DAYS * 1024 / 7 ))
      N=$(nlines "$D/delegate/out/olivia/metrics.jsonl")
      log "$DAYS days: wait for olivia's next renewal (now $N), then stop the delegate"
      until [ "$(nlines "$D/delegate/out/olivia/metrics.jsonl")" -gt "$N" ]; do sleep 10; done
      kill $DG; sleep 2
      log "olivia returns"; "$BIN" scenario delegate-renewal-return olivia "$D" > "$D/return-olivia.log" 2>&1; RO=$?
      "$BIN" scenario delegate-renewal-report "$D" > "$D/report.log" 2>&1
      kill $WA $WB 2>/dev/null
      setexp "$DEFAULT_EXPIRY"
      cat "$D/report.log"
      grep -h "RESULT" "$D"/setup.log "$D"/return-*.log
      RC=$(( RS || RR || RO ))
    } > "$L" 2>&1 ;;

  threshold-delegate)
    # 2-of-3 delegate: D1 = `delegated` (coordinator), D2/D3 = `cosignd`. Same scale as delegate-renewal.
    # Olivia: D2 goes offline after her 2nd renewal (2 of 3 left). Rita: D2 and D3 go offline after
    # her 1st renewal (1 of 3 left): renewals stop and the towers exit.
    DAYS=${1:-14}
    D=$R/state/run-$(date +%s)-threshold-delegate; mkdir -p "$D"
    W=$R/watchtowerd; [ -d "$W" ] && mv "$W" "$W.prev-$(date +%s)"; mkdir -p "$W/A" "$W/B"
    j() { python3 -c "import json;d=json.load(open('$D/scenario.json'));print(d$1)"; }
    nlines() { [ -f "$1" ] && wc -l < "$1" || echo 0; }
    { log "dir $D"; setexp 1024 wait; start_miner 10
      nohup "$BIN" watchtowerd "$W/A" > "$D/wtd-A.log" 2>&1 & WA=$!
      nohup "$BIN" watchtowerd "$W/B" > "$D/wtd-B.log" 2>&1 & WB=$!
      CS=""
      for i in 2 3; do mkdir -p "$D/cosign-$i/jobs"; echo "127.0.0.1:709$((i-1))" > "$D/cosign-$i/listen.txt"
        nohup "$BIN" cosignd "$D/cosign-$i" > "$D/cosignd-$i.log" 2>&1 & CS="$CS $!"; done
      sleep 3
      "$BIN" scenario threshold-delegate-setup "$D" > "$D/setup.log" 2>&1; RS=$?
      if [ $RS -ne 0 ]; then kill $WA $WB $CS; setexp "$DEFAULT_EXPIRY"; log "SETUP FAILED"; grep -h RESULT "$D/setup.log"; exit 1; fi
      nohup "$BIN" delegated "$D/delegate" > "$D/delegated.log" 2>&1 & DG=$!
      log "delegated pid $DG; wait for Rita's first renewal"
      until [ "$(nlines "$D/delegate/out/rita/metrics.jsonl")" -ge 1 ]; do sleep 10; done
      log "D2 and D3 go offline for Rita (1 of 3 left)"; touch "$D/cosign-2/offline-rita" "$D/cosign-3/offline-rita"
      until [ "$(nlines "$D/delegate/out/olivia/metrics.jsonl")" -ge 2 ]; do sleep 10; done
      log "D2 goes offline for Olivia (2 of 3 left)"; touch "$D/cosign-2/offline-olivia"
      for _ in $(seq 240); do grep -q '"done-onchain"' "$W"/A/status/rita-1.json 2>/dev/null && break; sleep 10; done
      log "rita returns"; "$BIN" scenario threshold-delegate-return rita "$D" > "$D/return-rita.log" 2>&1; RR=$?
      T0=$(j "['olivia']['t_offline']"); waitts $(( T0 + DAYS * 1024 / 7 ))
      N=$(nlines "$D/delegate/out/olivia/metrics.jsonl")
      log "$DAYS days: wait for olivia's next renewal (now $N), then stop the delegate"
      until [ "$(nlines "$D/delegate/out/olivia/metrics.jsonl")" -gt "$N" ]; do sleep 10; done
      kill $DG; sleep 2
      log "olivia returns"; "$BIN" scenario threshold-delegate-return olivia "$D" > "$D/return-olivia.log" 2>&1; RO=$?
      "$BIN" scenario threshold-delegate-report "$D" > "$D/report.log" 2>&1
      kill $WA $WB $CS 2>/dev/null
      setexp "$DEFAULT_EXPIRY"
      cat "$D/report.log"
      grep -h "RESULT" "$D"/setup.log "$D"/return-*.log
      grep -hc "REFUSED" "$D"/cosignd-*.log | paste -sd' ' | sed 's/^/cosignd REFUSED counts (D2 D3): /'
      RC=$(( RS || RR || RO ))
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
