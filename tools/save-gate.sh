#!/usr/bin/env bash
# Run the save/load round-trip gate end to end (M1, save format).
#
# Like gate.sh, but arms the save gate: at each gate frame the ring snapshot is
# serialized to the save file format, written to disk, read back, deserialized,
# restored and re-simulated. A lossy format, a cross-build range-table mismatch or a
# file-IO problem shows up as a `SAVEGATE DIVERGENCE` / `SAVEGATE ... failed` line.
# A one-shot save + re-read (`CM_ROLLBACK_SAVE_AT`, `CM_ROLLBACK_SAVE_VERIFY`)
# additionally proves the on-disk round trip of a full save (snapshot + session).
#
# Env knobs passed through (see crates/cosmo-rollback/src/*.rs):
#   CM_ROLLBACK_SAVE_GATE=<depth>          re-sim depth through the file round-trip
#   CM_ROLLBACK_SAVE_GATE_START=<frame>    first frame to gate (default 90)
#   CM_ROLLBACK_SAVE_AT=<frame>            one-shot save at this frame (default 100)
#   CM_ROLLBACK_SAVE_VERIFY=1              re-read and compare every save written
#   CM_ROLLBACK_GATE_IGNORE=<range,…>      ranges to exclude from the comparison
#   CM_GATE_BOOT/CM_GATE_POST              headless boot/run seconds
#
# Usage: save-gate.sh [Build] [depth] [start_frame]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="${1:-Release}"
DEPTH="${2:-3}"
START="${3:-90}"
CLICKS="${CM_GATE_CLICKS:-490,182;940,590;320,645;350,238;940,590}"
BOOT="${CM_GATE_BOOT:-55}"
POST="${CM_GATE_POST:-60}"
LOG="$HOME/Games/cosmonarchy/prefix/drive_c/Cosmonarchy/$BUILD/cosmo_rollback.log"

"$ROOT/tools/build.sh"
"$ROOT/tools/install-plugin.sh" "$BUILD"

# The log is append-only; a stale one would mix runs.
: > "$LOG"

CM_ROLLBACK_SAVE_GATE="$DEPTH" \
CM_ROLLBACK_SAVE_GATE_START="$START" \
CM_ROLLBACK_SAVE_AT="${CM_ROLLBACK_SAVE_AT:-100}" \
CM_ROLLBACK_SAVE_VERIFY=1 \
CM_ROLLBACK_NO_PLUGIN="${CM_ROLLBACK_NO_PLUGIN:-1}" \
    bash "$ROOT/tools/headless_probe.sh" "$BUILD" "$CLICKS" "" /tmp/opencode/savegate.png "$BOOT" "$POST"

echo "---- save gate summary ----"
if grep -qE "SAVEGATE|saveverify|save:" "$LOG"; then
    grep -E "SAVEGATE|saveverify|save:" "$LOG"
    if grep -q "SAVEGATE DIVERGENCE\|saveverify .*MISMATCH\|SAVEGATE .*failed" "$LOG"; then
        echo "SAVE GATE: FAIL"
        exit 1
    fi
    echo "SAVE GATE: PASS"
else
    echo "(no save-gate lines in $LOG — the replay may not have reached the gate frames)"
    exit 1
fi
