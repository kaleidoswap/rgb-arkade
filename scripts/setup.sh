#!/bin/bash
# Fetch the exact upstream sources used by the MVP, apply the patches, build arkd:rgb, the arkdb test tool and the demo.
# Run from the repo root. Paths in demo/src and scripts/ assume the root is /workspace/rgb-arkade
# (override scripts with R=...; demo/src has the path hardcoded: symlink the repo there or edit it).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd); cd "$ROOT"
mkdir -p vendor state logs tools watchtowerd

clone() { # dir url commit patch
  [ -d "$1/.git" ] || git clone -q "$2" "$1"
  git -C "$1" checkout -q "$3"
  if [ -n "${4:-}" ]; then
    if git -C "$1" apply --check "$ROOT/$4" 2>/dev/null; then git -C "$1" apply "$ROOT/$4"; else echo "patch $4 already applied or does not apply cleanly; check $1"; fi
  fi
}
clone vendor/arkd      https://github.com/arkade-os/arkd           f444de6c336c024f5b03e646feb781eec9a24c00 patches/arkd.patch      # tag v0.9.15
clone vendor/rust-sdk  https://github.com/arkade-os/rust-sdk       f184a5f1aa77924b7fd40eede7cf2fd43854ae76 patches/rust-sdk.patch  # tag v0.12.0
clone vendor/rgb-lib   https://github.com/kaleidoswap/rgb-lib      8852b2e55606ebd568f56bdfd9fa9216fbec3435 patches/rgb-lib.patch
clone arkade-regtest   https://github.com/ArkLabsHQ/arkade-regtest 457b9ef6119bea1b0a8f1356204ef2f1044b0c9b patches/arkade-regtest.patch
[ -f arkade-regtest/.env ] || cp patches/arkade-regtest.env arkade-regtest/.env

# arkd:rgb image = upstream v0.9.15 image with the patched binary
( cd vendor/arkd && CGO_ENABLED=0 go build -o "$ROOT/arkd-image/arkd" ./cmd/arkd )
docker build -t arkd:rgb arkd-image

# Stage 6 test tool (simulates a colluding operator editing its own DB); built inside the arkd module for its go.sum
mkdir -p vendor/arkd/cmd/arkdb-sim && cp tools/arkdb-sim/main.go vendor/arkd/cmd/arkdb-sim/
( cd vendor/arkd && CGO_ENABLED=0 go build -o "$ROOT/tools/arkdb" ./cmd/arkdb-sim )

# demo (toolchain pinned in demo/rust-toolchain.toml)
( cd demo && cargo build )
echo "setup done; start the regtest stack as described in README 'Reproduce from scratch'"
