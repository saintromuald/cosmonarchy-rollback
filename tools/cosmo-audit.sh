#!/usr/bin/env bash
# Launch a Cosmonarchy build with the rollback plugin installed and the audit
# harness armed. Requires a display; start a game (or load a replay) so the
# per-frame hook fires, then inspect cosmo_rollback.log in the build dir.
#
# Usage: tools/cosmo-audit.sh [Release|Prerelease] [audit_frame]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="${1:-Release}"
FRAME="${2:-120}"

case "$BUILD" in
  release|Release)          BUILD=Release ;;
  prerelease|Prerelease|pre) BUILD=Prerelease ;;
  *) echo "usage: $0 [Release|Prerelease] [audit_frame]" >&2; exit 1 ;;
esac

BASE="$HOME/Games/cosmonarchy/prefix/drive_c/Cosmonarchy"
BUILD_DIR="$BASE/$BUILD"
[[ -d "$BUILD_DIR" ]] || { echo "no build at $BUILD_DIR" >&2; exit 1; }

"$ROOT/tools/install-plugin.sh" "$BUILD"

export WINEPREFIX="$HOME/Games/cosmonarchy/prefix"
export PROTONPATH=GE-Proton
export GAMEID="umu-cosmonarchy-$BUILD"
export PROTON_VERB=waitforexitandrun
export CM_ROLLBACK_AUDIT_FRAME="$FRAME"

echo "Log will be written to: $BUILD_DIR/cosmo_rollback.log"
echo "Arming CM_ROLLBACK_AUDIT_FRAME=$FRAME"
cd "$BUILD_DIR"
exec umu-run "C:\\Cosmonarchy\\$BUILD\\Cosmonarchy BW.exe"
