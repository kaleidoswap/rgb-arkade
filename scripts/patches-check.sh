#!/bin/bash
# Check that every patch (or a+b stack, b applied on top of a) applies cleanly to its pinned upstream commit (uses throwaway git worktrees
# of the checkouts made by setup.sh, so already-patched trees are fine). The arkd master patch is
# checked against a worktree of vendor/arkd (same upstream), fetching the master commit if needed,
# so plain `make setup` is enough.
set -u
source "$(dirname "$0")/lib.sh"
FAIL=0
while read -r dir commit patch; do
  if [ ! -e "$R/$dir/.git" ]; then echo "FAIL $patch (no checkout $dir: run make setup)"; FAIL=1; continue; fi
  git -C "$R/$dir" cat-file -e "$commit^{commit}" 2>/dev/null || git -C "$R/$dir" fetch -q origin "$commit" 2>/dev/null || git -C "$R/$dir" fetch -q origin
  w=$(mktemp -d)/wt
  git -C "$R/$dir" worktree add -q --detach "$w" "$commit" || { echo "FAIL $patch (commit $commit not in $dir)"; FAIL=1; continue; }
  # a+b: b applies on top of a
  ok=1; for p in ${patch//+/ }; do git -C "$w" apply "$R/patches/$p" || { ok=0; break; }; done
  if [ $ok = 1 ]; then echo "OK   $patch"; else echo "FAIL $patch"; FAIL=1; fi
  git -C "$R/$dir" worktree remove --force "$w"
done <<'LIST'
vendor/arkd f444de6c336c024f5b03e646feb781eec9a24c00 arkd.patch
vendor/arkd f863e484719344edbe4a8d10cf5fe994b123f2c0 arkd-checkpoint.patch
vendor/arkd f863e484719344edbe4a8d10cf5fe994b123f2c0 arkd-checkpoint.patch+arkd-master-ac.patch
vendor/rust-sdk f184a5f1aa77924b7fd40eede7cf2fd43854ae76 rust-sdk.patch
vendor/rgb-lib 8852b2e55606ebd568f56bdfd9fa9216fbec3435 rgb-lib.patch
arkade-regtest 457b9ef6119bea1b0a8f1356204ef2f1044b0c9b arkade-regtest.patch
vendor/emulator 7b830f6d7bd3998e1790f3d3e7737f34f40588f1 emulator.patch
vendor/emulator 7b830f6d7bd3998e1790f3d3e7737f34f40588f1 emulator.patch+emulator-crashtest.patch
LIST
exit $FAIL
