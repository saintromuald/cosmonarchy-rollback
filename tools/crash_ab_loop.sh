#!/usr/bin/env bash
# Repeated "load LastReplay via UI" runs to measure whether the gptp.qdp
# dialog-callback crash (0x025f012d) reproduces. Usage:
#   tools/crash_ab_loop.sh <n-runs> [label]
set -u
RUNS="${1:-3}"
LABEL="${2:-run}"
ROOT="${AB_ROOT:-$HOME/Games/cosmonarchy-agent2}"
CHANNEL="${AB_CHANNEL:-Release}"
LOG="$ROOT/prefix/drive_c/Cosmonarchy/$CHANNEL/cosmo_rollback.log"
OUT=/tmp/opencode/ab-results.txt

cd "$HOME/Projects/cosmonarchy-rollback" || exit 1
: > "$OUT"

for i in $(seq 1 "$RUNS"); do
    bash tools/ui_session.sh stop dbg 2>/dev/null
    pkill -9 -f "^Xvfb :94" 2>/dev/null
    sleep 2
    rm -f /tmp/opencode/ui-dbg/xvfb.pid
    : > "$LOG"
    CM_ROLLBACK_LIVE=1 CM_ROLLBACK_SLOT=0 CM_ROLLBACK_SCRIPT_PEER=1 ${AB_ALLOW_AI:+CM_ROLLBACK_ALLOW_AI=1} \
        bash tools/ui_session.sh start dbg :94 "$ROOT" "$CHANNEL"
    sleep 75
    # menu -> Single Player -> campaign screen -> Play Custom Replay -> pick LastReplay -> OK
    bash tools/ui_session.sh key dbg "Return";   sleep 4
    bash tools/ui_session.sh click dbg "490,207"; sleep 4
    bash tools/ui_session.sh key dbg "Return";   sleep 4
    bash tools/ui_session.sh click dbg "200,652"; sleep 4
    bash tools/ui_session.sh click dbg "195,652"; sleep 4
    bash tools/ui_session.sh click dbg "360,277"; sleep 3
    bash tools/ui_session.sh click dbg "935,565"; sleep 45
    U=$(grep -c UNHANDLED "$LOG" 2>/dev/null); U=${U:-0}
    D=$(grep -c "driving from" "$LOG" 2>/dev/null); D=${D:-0}
    F=$(grep -m1 "read at" "$LOG" 2>/dev/null || echo "-")
    echo "[$LABEL #$i] unhandled=$U driving=$D fault=$F" | tee -a "$OUT"
    cp "$LOG" "/tmp/opencode/ab-$LABEL-$i.log" 2>/dev/null
    if [ "$U" -gt 0 ]; then
        grep -A6 "UNHANDLED" "$LOG" | head -40 >> "$OUT"
    fi
done
bash tools/ui_session.sh stop dbg 2>/dev/null
echo "results: $OUT"
