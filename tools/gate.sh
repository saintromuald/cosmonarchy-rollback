#!/usr/bin/env bash
# Run the rollback kill gate end to end and summarize it.
#
# Builds the plugin, installs it, truncates the log, boots Cosmonarchy headlessly,
# clicks through to a running replay so the per-frame hook fires, tears the session
# down, and prints the divergence summary. This is the loop every snapshot-range
# change is judged by: the gate passes when there are no DIVERGENCE lines.
#
# Env knobs are passed through to the game (see crates/cosmo-rollback/src/*.rs):
#   CM_ROLLBACK_SHADOW=<depth>        re-simulate every frame at this depth (default 3)
#   CM_ROLLBACK_SHADOW_START=<frame>  first frame to gate (default 90)
#   CM_ROLLBACK_NO_PLUGIN=1           skip gptp/aise state blobs (default 1 here, for
#                                     isolating engine ranges from plugin state)
#   CM_ROLLBACK_NO_POOLS=1            exclude the object pools entirely (bisection)
#   CM_ROLLBACK_NO_{IMAGE,TILES,CREEP,REGION}=1  exclude one auxiliary snapshot
#   CM_ROLLBACK_POOL_<NAME>=0x...     pin a pool base instead of resolving it
#   any other CM_ROLLBACK_*           forwarded as-is
#
# Usage: gate.sh [Build] [depth] [start_frame]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="${1:-Release}"
DEPTH="${2:-3}"
START="${3:-90}"
# A fresh boot can land on the profile chooser (trees used by the CrownLink harness
# carry several profiles); Enter picks the highlighted one and proceeds to the main
# menu, after which the fixed clicks apply.
CLICKS="${CM_GATE_CLICKS:-key:Return;490,182;940,590;320,645;350,238;940,590}"
BOOT="${CM_GATE_BOOT:-55}"
POST="${CM_GATE_POST:-60}"
# Isolated game trees (the two-client sandboxes) have their own Release build; the
# main tree's builds live under the launcher's layout now, so the gate must follow
# COSMO_GAME_ROOT like headless_probe.sh does.
GAME_ROOT="${COSMO_GAME_ROOT:-$HOME/Games/cosmonarchy}"
LOG="$GAME_ROOT/prefix/drive_c/Cosmonarchy/$BUILD/cosmo_rollback.log"

"$ROOT/tools/build.sh"
"$ROOT/tools/install-plugin.sh" "$BUILD"

# The log is append-only; a stale one would mix runs.
: > "$LOG"

CM_ROLLBACK_SHADOW="$DEPTH" \
CM_ROLLBACK_SHADOW_START="$START" \
CM_ROLLBACK_NO_PLUGIN="${CM_ROLLBACK_NO_PLUGIN:-1}" \
    bash "$ROOT/tools/headless_probe.sh" "$BUILD" "$CLICKS" "" /tmp/opencode/gate.png "$BOOT" "$POST"

exec "$ROOT/tools/log-summary.sh" "$LOG"
