#!/bin/sh
# build-macos.sh <git-ref|WORKTREE> <name>: build.sh for Docker Desktop on Apple Silicon.
# Cross-compiles a static aarch64 Linux binary with zig (brew install zig;
# rustup target add aarch64-unknown-linux-musl).
set -e
E=$(cd "$(dirname "$0")" && pwd); W=$E/.target/zig; mkdir -p "$W"
# zig doesn't understand rustc's --target flag; drop it.
printf '%s\n' '#!/bin/sh' 'for a do shift; case "$a" in --target=*|-target) ;; *) set -- "$@" "$a";; esac; done' \
  'exec zig cc -target aarch64-linux-musl "$@"' > "$W/cc"
printf '%s\n' '#!/bin/sh' 'exec zig ar "$@"' > "$W/ar"
chmod +x "$W/cc" "$W/ar"
export E2E_TARGET=aarch64-unknown-linux-musl \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$W/cc" \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-self-contained=no" \
  CC_aarch64_unknown_linux_musl="$W/cc" AR_aarch64_unknown_linux_musl="$W/ar"
exec "$E/build.sh" "$@"
