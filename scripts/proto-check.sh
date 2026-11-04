#!/bin/bash
# Compile proto/*.proto with protoc (python grpcio-tools) and cross-check the Rust prost structs:
# every given .pb file (written by the demo / rgb-lib) must parse with the .proto schema, carry no
# unknown fields and re-serialize to the identical bytes.
# Usage: scripts/proto-check.sh [files...]   (default: the newest *.pb files under state/)
# Needs: PYTHON with grpcio-tools (e.g. python3 -m venv v && v/bin/pip install grpcio-tools).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PY=${PYTHON:-python3}
OUT=$(mktemp -d)
"$PY" -m grpc_tools.protoc -I "$ROOT/proto" --python_out="$OUT" "$ROOT"/proto/*.proto
echo "protoc: $(ls "$ROOT"/proto/*.proto | xargs -n1 basename | tr '\n' ' ')compiled"
if [ $# -eq 0 ]; then
  mapfile -t FILES < <(find "$ROOT/state" -name '*.pb' -newer "$ROOT/proto/ark_proof.proto" 2>/dev/null | grep -v '\.decoded\.pb$' | sort)
else FILES=("$@"); fi
PYTHONPATH="$OUT" "$PY" - "${FILES[@]}" <<'PYEOF'
import sys, os
import ark_proof_pb2 as ap, offers_pb2 as of, watchtower_pb2 as wt
from google.protobuf import unknown_fields
def kind(p):
    b = os.path.basename(p)
    if '.arkproof.pb' in b or '.neg-' in b and b.endswith('.pb') and 'arkproof' in p: return ap.ArkProof
    if b.startswith('fraud-'): return ap.ArkFraudProof
    if b == 'offer.pb': return of.SwapEmuOffer if 'swap-emu' in p else of.SwapNiOffer
    if b.startswith('watchtower-registration') or '/registrations/' in p: return wt.WatchtowerRegistration
    return None
def unknown(m):
    n = len(unknown_fields.UnknownFieldSet(m))
    for fd, v in m.ListFields():
        if fd.message_type is not None:
            for x in (v if fd.is_repeated else [v]): n += unknown(x)
    return n
bad = 0; seen = {}
for p in sys.argv[1:]:
    k = kind(p)
    if k is None: continue
    raw = open(p, 'rb').read()
    m = k(); m.ParseFromString(raw)
    ok = unknown(m) == 0 and m.SerializeToString(deterministic=True) == raw
    seen[k.__name__] = seen.get(k.__name__, 0) + 1
    if not ok:
        bad += 1; print("MISMATCH", k.__name__, p)
print("checked:", seen, "mismatches:", bad)
sys.exit(1 if bad else 0)
PYEOF
