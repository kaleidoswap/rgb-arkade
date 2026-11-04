#!/bin/bash
# Build arkd:rgb-cponly: arkd master with ONE change, an optional data OP_RETURN in the checkpoint tx
# (patches/arkd-checkpoint.patch). Used by REGTEST_PROFILE=cponly and the checkpoint-* scenarios.
# Run after scripts/setup.sh (the demo and the other upstreams come from there).
set -euo pipefail
source "$(dirname "$0")/lib.sh"
cd "$R"
vendor_checkout vendor/arkd-master https://github.com/arkade-os/arkd f863e484719344edbe4a8d10cf5fe994b123f2c0 patches/arkd-checkpoint.patch
D=vendor/arkd-master
export CGO_ENABLED=0 GOOS=linux GOARCH=$(docker version -f '{{.Server.Arch}}')
# ARKD_CPONLY_TAG overrides the image tag (default arkd:rgb-cponly, as in patches/arkade-regtest.cponly.env)
( cd "$D" && go build -ldflags="-X 'main.Version=v0.9.16-master-f863e48-rgb-cponly'" -o "$R/arkd-image-cponly/arkd" ./cmd/arkd )
docker build -t "${ARKD_CPONLY_TAG:-arkd:rgb-cponly}" arkd-image-cponly
echo "${ARKD_CPONLY_TAG:-arkd:rgb-cponly} built; next: make regtest-up-cponly, then make scenario SCENARIO=checkpoint-renew"
