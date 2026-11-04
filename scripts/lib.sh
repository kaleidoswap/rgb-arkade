# shellcheck shell=bash
# Common helpers for the scripts. Source it; it sets R (project root), BIN (demo binary), and helpers.
R=${RGB_ARKADE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
export RGB_ARKADE_DIR=$R
BIN=${BIN:-${CARGO_TARGET_DIR:-$R/demo/target}/debug/ark-rgb-demo}
ADMIN=${ARKD_ADMIN:-localhost:7071}
ARKD=${ARKD_URL:-localhost:7070}
DEFAULT_EXPIRY=${DEFAULT_EXPIRY:-604672}   # the regtest .env value (~7 days)
mkdir -p "$R/state" "$R/logs"

log() { echo "[$(date +%H:%M:%S)] $*"; }

# arkd keeps settings in its DB; change the batch expiry through the admin API.
# Rounds already in progress keep the old value, so wait for them to finish.
setexp() {
  curl -s -X POST -H 'Content-Type: application/json' "$ADMIN/v1/admin/settings" \
    -d "{\"settings\":{\"vtxoTreeExpiry\":\"$1\"}}" >/dev/null
  log "vtxoTreeExpiry=$1"
  [ "${2:-}" = "wait" ] && sleep 45
  return 0
}

# Background miner (one block every $1 s); CSV/MTP-based timelocks need the chain to advance.
MINER=""
start_miner() { ( while true; do node "$R/arkade-regtest/regtest.mjs" mine 1 >/dev/null 2>&1; sleep "${1:-10}"; done ) & MINER=$!; }
stop_miner() { [ -n "$MINER" ] && kill "$MINER" 2>/dev/null; MINER=""; }

# Make sure arkd is not left paused by an offline-proof run.
unpause_arkd() { curl -s -m 3 -o /dev/null "$ARKD/v1/info" || docker unpause arkd >/dev/null 2>&1 || true; }

waitts() { while [ "$(date +%s)" -lt "$1" ]; do sleep 5; done; }

# Check out an upstream at its pinned commit with its patch applied: vendor_checkout <dir> <url>
# <commit> <patch>. vendor/.stamps/<dir name> records the commit and patch hash last applied, so a
# changed patch is detected. A checkout with other changes (an older patch, local edits) stops the
# setup unless RESET_VENDOR=1, which discards them. A patch that does not apply is an error.
vendor_checkout() {
  local dir=$R/$1 url=$2 commit=$3 rel=$4 patch=$R/$4 stamp want new f
  stamp=$R/vendor/.stamps/$(basename "$1")
  want="$commit $(sha256sum "$patch" | cut -d' ' -f1)"
  mkdir -p "$R/vendor/.stamps"
  [ -e "$dir/.git" ] || git clone -q "$url" "$dir"
  git -C "$dir" cat-file -e "$commit^{commit}" 2>/dev/null || git -C "$dir" fetch -q origin
  if [ "$(git -C "$dir" rev-parse HEAD)" = "$commit" ]; then
    if [ "$(cat "$stamp" 2>/dev/null)" = "$want" ]; then log "$rel: applied (stamp)"; return 0; fi
    if git -C "$dir" apply --reverse --check "$patch" 2>/dev/null; then
      echo "$want" > "$stamp"; log "$rel: already applied"; return 0
    fi
  fi
  # files the patch creates
  new=$(awk '/^diff --git /{p=$4; sub(/^b\//, "", p)} /^new file mode/{print p}' "$patch")
  local dirty
  dirty=$(git -C "$dir" status --porcelain --untracked-files=no)
  for f in $new; do [ -e "$dir/$f" ] && dirty="$dirty $f"; done
  if [ -n "$dirty" ]; then
    if [ "${RESET_VENDOR:-}" != 1 ]; then
      echo "error: $1 has changes that are not $rel (an older patch or local edits)." >&2
      echo "  To keep local edits, regenerate the patch from them (see README); to discard them, rerun with RESET_VENDOR=1." >&2
      return 1
    fi
    log "$1: discarding changes (RESET_VENDOR=1)"
    git -C "$dir" checkout -q -f "$commit"
    for f in $new; do rm -rf "${dir:?}/$f"; done
  fi
  git -C "$dir" checkout -q "$commit"
  if ! git -C "$dir" apply --check "$patch"; then
    echo "error: $rel does not apply to $1 at $commit" >&2
    return 1
  fi
  git -C "$dir" apply "$patch"
  echo "$want" > "$stamp"
  log "$rel: applied"
}
