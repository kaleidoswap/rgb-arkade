#!/bin/bash
# (Re)start the Arkade emulator next to the running regtest arkd (same docker network, port 7073).
# Usage: scripts/emulator-up.sh [image=emulator:rgb] [extra docker run args, e.g. -e EMULATOR_RGB_TEST_CRASH=after]
# The datadir (pending finalize records) is the docker volume emulator-data, kept across restarts.
set -euo pipefail
IMG=${1:-emulator:rgb}; shift || true
NET=$(docker inspect arkd -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' | awk '{print $1}')
docker rm -f emulator >/dev/null 2>&1 || true
docker run -d --name emulator --network "$NET" -p 7073:7073 \
  -e EMULATOR_NO_TLS=true -e EMULATOR_PORT=7073 -e EMULATOR_LOG_LEVEL=5 -e EMULATOR_ARKD_URL=arkd:7070 \
  -v emulator-data:/app/data -e EMULATOR_DATADIR=/app/data \
  -e EMULATOR_SECRET_KEY=b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c4 "$@" "$IMG" >/dev/null
for _ in $(seq 60); do curl -sf http://localhost:7073/v1/info >/dev/null && { echo "emulator $IMG up on $NET"; exit 0; }; sleep 1; done
docker logs --tail 30 emulator; echo "emulator did not come up"; exit 1
