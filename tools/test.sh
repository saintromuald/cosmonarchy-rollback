#!/usr/bin/env bash
# Build and run the crate's unit tests for the 32-bit Windows target, under wine.
#
# The crate is a Windows-only cdylib (whack/samase_shim/windows-sys), so `cargo test`
# cannot run on the Linux host. The engine-independent modules (`turns.rs` and anything
# else that does not touch the game) can still be executed: build the test harness for
# the Windows target with cargo-xwin and run the resulting PE under wine.
#
# Usage: test.sh [test-name-filter]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"
export XWIN_ARCH=x86
TARGET="${TARGET:-i686-pc-windows-msvc}"

cd "$ROOT"
cargo xwin test --target "$TARGET" --no-run "$@"

EXE="$(ls -t target/"$TARGET"/debug/deps/cosmo_rollback-*.exe | head -1)"
echo "running $EXE under wine"
WINEPREFIX="${WINEPREFIX:-$HOME/.wine}" wine "$EXE" --test-threads=1
