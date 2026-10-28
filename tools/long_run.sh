#!/usr/bin/env bash
# Long map run to catch the intermittent gptp dialog-callback crash
# (historically at ticks 12161+ mid-game, or tick 0 at load).
# Usage: tools/long_run.sh <root> <channel> <display>
set -u
ROOT="${1:-$HOME/Games/cosmonarchy-agent2}"
CHANNEL="${2:-Release}"
DISP="${3:-:94}"
LOG="$ROOT/prefix/drive_c/Cosmonarchy/$CHANNEL/cosmo_rollback.log"
cd "$HOME/Projects/cosmonarchy-rollback" || exit 1

bash tools/ui_session.sh stop dbg 2>/dev/null
sleep 2
rm -f /tmp/opencode/ui-dbg/xvfb.pid
: > "$LOG"
CM_ROLLBACK_LIVE=1 CM_ROLLBACK_SLOT=0 CM_ROLLBACK_SCRIPT_PEER=1 \
    bash tools/ui_session.sh start dbg "$DISP" "$ROOT" "$CHANNEL"
sleep 75
# main menu -> Single Player -> pick profile -> OK -> Custom -> map browser
bash tools/ui_session.sh key dbg "Return";     sleep 5
bash tools/ui_session.sh click dbg "460,205";  sleep 5
bash tools/ui_session.sh click dbg "350,185";  sleep 3   # profile row
bash tools/ui_session.sh click dbg "900,560";  sleep 6   # profile OK
bash tools/ui_session.sh click dbg "215,600";  sleep 5   # Custom -> map browser
# enter campaign/The Lost Gods
bash tools/ui_session.sh click dbg "350,224";  sleep 2
bash tools/ui_session.sh key dbg "Return";     sleep 5
bash tools/ui_session.sh click dbg "350,350";  sleep 2
bash tools/ui_session.sh key dbg "Return";     sleep 5
# pick Waterwar and start
bash tools/ui_session.sh click dbg "350,275";  sleep 3
bash tools/ui_session.sh click dbg "902,564";  sleep 10
echo "long-run started; game log: $LOG"
grep -m1 "taking over\|UNHANDLED" "$LOG" || echo "(not yet in game)"
