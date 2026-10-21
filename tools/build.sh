#!/usr/bin/env bash
# Cross-build the Cosmonarchy rollback DLL for 32-bit Windows from Linux.
#
# Host toolchain is the userspace rustup install (RUSTUP_HOME/CARGO_HOME) because
# Arch's system rustc bakes a vendor suffix that makes prebuilt rust-std metadata
# incompatible. cargo-xwin supplies the MSVC CRT/SDK; XWIN_ARCH=x86 selects the
# 32-bit libraries. LLVM (clang-cl/lld-link/llvm-lib/nasm) must be on PATH.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"
export XWIN_ARCH=x86

TARGET="${TARGET:-i686-pc-windows-msvc}"
PROFILE="${PROFILE:-dev}"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
    exec cargo xwin build --target "$TARGET" --release "$@"
else
    exec cargo xwin build --target "$TARGET" "$@"
fi
