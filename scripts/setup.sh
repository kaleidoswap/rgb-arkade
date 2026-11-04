#!/bin/bash
# Fetch the pinned upstream sources, apply patches/, and build the arkd:rgb image, the arkdb test tool
# and the demo. Idempotent: re-running skips clones and patches already applied; a changed patch is
# detected (vendor/.stamps) and stops the setup if the checkout has other changes (RESET_VENDOR=1).
# Usage: scripts/setup.sh   (from anywhere; everything goes under the repo root, or $RGB_ARKADE_DIR)
set -euo pipefail
source "$(dirname "$0")/lib.sh"
cd "$R"
mkdir -p vendor tools watchtowerd

vendor_checkout vendor/arkd      https://github.com/arkade-os/arkd           f444de6c336c024f5b03e646feb781eec9a24c00 patches/arkd.patch      # tag v0.9.15
vendor_checkout vendor/rust-sdk  https://github.com/arkade-os/rust-sdk       f184a5f1aa77924b7fd40eede7cf2fd43854ae76 patches/rust-sdk.patch  # tag v0.12.0
vendor_checkout vendor/rgb-lib   https://github.com/kaleidoswap/rgb-lib      8852b2e55606ebd568f56bdfd9fa9216fbec3435 patches/rgb-lib.patch
vendor_checkout arkade-regtest   https://github.com/ArkLabsHQ/arkade-regtest 457b9ef6119bea1b0a8f1356204ef2f1044b0c9b patches/arkade-regtest.patch
[ -f arkade-regtest/.env ] || cp patches/arkade-regtest.env arkade-regtest/.env

# arkd:rgb image = upstream v0.9.15 image with the patched binary; both binaries run in linux containers
# of the docker daemon's architecture (also when the host is macOS)
export CGO_ENABLED=0 GOOS=linux GOARCH=$(docker version -f '{{.Server.Arch}}')
# ARKD_TAG overrides the image tag (default arkd:rgb, as in patches/arkade-regtest.env)
( cd vendor/arkd && go build -ldflags="-X 'main.Version=v0.9.15-rgb'" -o "$R/arkd-image/arkd" ./cmd/arkd )
docker build -t "${ARKD_TAG:-arkd:rgb}" arkd-image

# fraud-scenario test tool (simulates a colluding operator editing its own DB); built inside the arkd module for its go.sum
mkdir -p vendor/arkd/cmd/arkdb-sim && cp tools/arkdb-sim/main.go vendor/arkd/cmd/arkdb-sim/
( cd vendor/arkd && go build -o "$R/tools/arkdb" ./cmd/arkdb-sim )

# demo (toolchain pinned in demo/rust-toolchain.toml), native
unset GOOS GOARCH
( cd demo && cargo build )
echo "setup done; next: make regtest-up (see README, Quickstart)"
