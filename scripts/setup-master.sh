#!/bin/bash
# Build arkd:rgb-master: arkd master f863e48 + patches/arkd-checkpoint.patch (B) + patches/arkd-master-ac.patch
# (C forfeit-opret, A opret-data-output). Used by REGTEST_PROFILE=master. Run after scripts/setup.sh.
set -euo pipefail
source "$(dirname "$0")/lib.sh"
cd "$R"
D=vendor/arkd-master-ac
vendor_checkout "$D" https://github.com/arkade-os/arkd f863e484719344edbe4a8d10cf5fe994b123f2c0 patches/arkd-checkpoint.patch
if git -C "$D" apply --check "$R/patches/arkd-master-ac.patch" 2>/dev/null; then
  git -C "$D" apply "$R/patches/arkd-master-ac.patch"
else
  git -C "$D" apply --check -R "$R/patches/arkd-master-ac.patch" || { echo "patches/arkd-master-ac.patch does not apply to $D"; exit 1; }
fi
export CGO_ENABLED=0 GOOS=linux GOARCH=$(docker version -f '{{.Server.Arch}}')
( cd "$D" && go build -ldflags="-X 'main.Version=v0.9.16-master-f863e48-rgb'" -o "$R/arkd-image-master/arkd" ./cmd/arkd )
docker build -t arkd:rgb-master arkd-image-master
echo "arkd:rgb-master built; next: REGTEST_PROFILE=master scripts/regtest-up.sh clean"
