#!/bin/bash
# Build arkd:rgb-cponly: arkd master with ONE change, an optional data OP_RETURN in the checkpoint tx
# (patches/arkd-checkpoint.patch). Used by REGTEST_PROFILE=cponly and the checkpoint-* scenarios.
# Run after scripts/setup.sh (the demo and the other upstreams come from there).
set -euo pipefail
ROOT=${RGB_ARKADE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}; cd "$ROOT"
D=vendor/arkd-master
[ -e "$D/.git" ] || git clone -q https://github.com/arkade-os/arkd "$D"
git -C "$D" checkout -q f863e484719344edbe4a8d10cf5fe994b123f2c0
if git -C "$D" apply --check "$ROOT/patches/arkd-checkpoint.patch" 2>/dev/null; then git -C "$D" apply "$ROOT/patches/arkd-checkpoint.patch"; else echo "arkd-checkpoint.patch already applied or does not apply cleanly; check $D"; fi
export CGO_ENABLED=0 GOOS=linux GOARCH=$(docker version -f '{{.Server.Arch}}')
( cd "$D" && go build -o "$ROOT/arkd-image-cponly/arkd" ./cmd/arkd )
docker build -t arkd:rgb-cponly arkd-image-cponly
echo "arkd:rgb-cponly built; next: make regtest-up-cponly, then make scenario SCENARIO=checkpoint-renew"
