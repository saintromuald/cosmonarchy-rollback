#!/usr/bin/env bash
# Install the freshly built rollback plugin into a Cosmonarchy build's plugins dir.
#
# The QDP hotloader loads plugins/*.qdp (it LoadLibraryA's each and requires the
# `GetMPQDraftPlugin` export). We name ours `zz-...` so it is enumerated after the
# other plugins, making it hold the outermost hook among them.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="${1:-Release}"
PROFILE="${PROFILE:-debug}"

DLL="$ROOT/target/i686-pc-windows-msvc/$PROFILE/cosmo_rollback.dll"
GAME_ROOT="${COSMO_GAME_ROOT:-$HOME/Games/cosmonarchy}"
PLUGDIR="$GAME_ROOT/prefix/drive_c/Cosmonarchy/$BUILD/plugins"

[[ -f "$DLL" ]] || { echo "missing $DLL (run tools/build.sh first)" >&2; exit 1; }
[[ -d "$PLUGDIR" ]] || { echo "missing plugins dir $PLUGDIR" >&2; exit 1; }

cp "$DLL" "$PLUGDIR/zz-cosmo-rollback.qdp"
echo "installed: $PLUGDIR/zz-cosmo-rollback.qdp"

# Uninstall: rm "$PLUGDIR/zz-cosmo-rollback.qdp"
