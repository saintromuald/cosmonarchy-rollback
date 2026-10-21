#!/usr/bin/env bash
# Summarize a cosmo_rollback.log from a shadow-gate run.
#
# The log is append-only, so a run's output is only meaningful after truncating it;
# tools/gate.sh does that. This script answers the three questions a gate run is
# asked: did anything diverge, what state was it in, and did the re-sim consume the
# RNG the same number of times as normal play.
#
# Usage: log-summary.sh [path-to-cosmo_rollback.log]
set -uo pipefail

LOG="${1:-$HOME/Games/cosmonarchy/prefix/drive_c/Cosmonarchy/Release/cosmo_rollback.log}"
[[ -f "$LOG" ]] || { echo "no log at $LOG" >&2; exit 1; }

echo "== log: $LOG ($(wc -l <"$LOG") lines)"

echo
echo "== startup"
grep -E "loaded:|pool |plugin serializers:|shadow=" "$LOG" | sed 's/^/   /'

echo
echo "== range layout"
grep -cE "^  range " "$LOG" | sed 's/^/   ranges logged: /'

DIV=$(grep -c "DIVERGENCE" "$LOG")
echo
echo "== divergences: $DIV"
if (( DIV > 0 )); then
    echo "   frames:      $(grep -o 'DIVERGENCE frame [0-9]*' "$LOG" | awk '{print $3}' | tr '\n' ' ')"
    echo "   by range:"
    grep -E "^    range " "$LOG" | awk '{print $2}' | sort | uniq -c | sort -rn | sed 's/^/      /'
fi

echo
echo "== re-sim RNG call deltas (0 == the re-sim drew the same number of random numbers)"
if grep -q "rng_calls" "$LOG"; then
    grep -o "delta [-0-9]*" "$LOG" | awk '{print $2}' | sort -n | uniq -c | sed 's/^/   delta /'
else
    echo "   (none logged; the shadow window did not reach a snapshot)"
fi

echo
echo "== self-restore (capture -> restore -> capture must be byte-identical)"
grep "SELF-RESTORE" "$LOG" | sed 's/^/   /' || echo "   (not reached)"

echo
echo "== shadow frames that re-simulated cleanly: $(grep -c 'shadow ok' "$LOG")"
