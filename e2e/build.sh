#!/bin/sh
# build.sh <git-ref|WORKTREE> <name>: release proxy binary -> bin/<name>.
#   ./build.sh WORKTREE new    # the working tree (builds the UI unless JELLYSWARRM_SKIP_UI=1)
#   ./build.sh 7092b79 old     # an older commit, for the upgrade/backup suites (reuses this tree's UI)
# The binary runs inside Linux containers. On Linux nothing extra is needed. Elsewhere,
# cross-compile: set E2E_TARGET (e.g. aarch64-unknown-linux-musl) plus the matching
# CARGO_TARGET_<TRIPLE>_LINKER; see README.md.
set -e
E=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$E/.." && pwd)
mkdir -p "$E/bin"
if [ "$1" = WORKTREE ]; then
  SRC=$REPO
else
  SRC=$E/.wt-$2; rm -rf "$SRC"; git -C "$REPO" worktree prune
  git -C "$REPO" worktree add -q --detach "$SRC" "$1"
  ln -s "$REPO/crates/jellyswarrm-proxy/static" "$SRC/crates/jellyswarrm-proxy/static"
  export JELLYSWARRM_SKIP_UI=1
fi
(cd "$SRC" && cargo build --release -p jellyswarrm-proxy ${E2E_TARGET:+--target "$E2E_TARGET"} --target-dir "$E/.target")
cp "$E/.target/${E2E_TARGET:+$E2E_TARGET/}release/jellyswarrm-proxy" "$E/bin/$2"
if [ "$1" != WORKTREE ]; then git -C "$REPO" worktree remove --force "$SRC"; fi
echo "built bin/$2 from $1"
