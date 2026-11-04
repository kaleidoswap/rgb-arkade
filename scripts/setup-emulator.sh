#!/bin/bash
# Build emulator:rgb: arkade-os/emulator v0.0.9 + patches/emulator.patch (accepts the checkpoint-opret
# layout [OP_RETURN <32B>, checkpoint, anchor] of arkd:rgb-cponly; durable finalize of submitted txs
# under $EMULATOR_DATADIR/pending). Used by scenario swap-emu.
set -euo pipefail
ROOT=${RGB_ARKADE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}; cd "$ROOT"
D=vendor/emulator
[ -e "$D/.git" ] || git clone -q https://github.com/arkade-os/emulator "$D"
git -C "$D" checkout -q 7b830f6d7bd3998e1790f3d3e7737f34f40588f1
if git -C "$D" apply --check "$ROOT/patches/emulator.patch" 2>/dev/null; then git -C "$D" apply "$ROOT/patches/emulator.patch"; else echo "emulator.patch already applied or does not apply cleanly; check $D"; fi
( cd "$D/pkg/emulator" && go test -count=1 ./... )
( cd "$D" && go test -count=1 ./internal/application/ )
export CGO_ENABLED=0 GOOS=linux GOARCH=$(docker version -f '{{.Server.Arch}}')
( cd "$D" && go build -ldflags="-X 'main.Version=v0.0.9-rgb'" -o "$ROOT/emulator-image/emulator" ./cmd/emulator.go )
docker build -t emulator:rgb emulator-image
( cd tools/emucov && go build -o "$ROOT/tools/emucov/emucov" . )
# test-only image emulator:rgb-crashtest: + patches/emulator-crashtest.patch (on top of emulator.patch;
# exits right before/after arkd SubmitTx when EMULATOR_RGB_TEST_CRASH=before|after); NOT shipped
if [ "${CRASHTEST:-1}" = 1 ]; then
  git -C "$D" apply "$ROOT/patches/emulator-crashtest.patch"
  ( cd "$D" && go build -ldflags="-X 'main.Version=v0.0.9-rgb-crashtest'" -o "$ROOT/emulator-image/emulator" ./cmd/emulator.go )
  git -C "$D" apply -R "$ROOT/patches/emulator-crashtest.patch"
  docker build -t emulator:rgb-crashtest emulator-image
  ( cd "$D" && go build -ldflags="-X 'main.Version=v0.0.9-rgb'" -o "$ROOT/emulator-image/emulator" ./cmd/emulator.go )
fi
echo "emulator:rgb built (and tools/emucov)"
