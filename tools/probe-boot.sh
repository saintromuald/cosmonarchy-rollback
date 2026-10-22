#!/usr/bin/env bash
# Boot the game headlessly and print a process/log timeline, for boot diagnostics.
# Usage: probe-boot.sh [build] [total_secs] [display]
set -u

BUILD="${1:-Release}"
TOTAL="${2:-240}"
DISPLAY_NUM="${3:-:97}"
GAME_ROOT="${COSMO_GAME_ROOT:-$HOME/Games/cosmonarchy}"
LOG_DIR="${COSMO_LOG_DIR:-/tmp/opencode}"
BASE="$GAME_ROOT/prefix/drive_c/Cosmonarchy"
PLOG="$BASE/$BUILD/cosmo_plus.log"
RLOG="$BASE/$BUILD/cosmo_rollback.log"
ERRDIR="$BASE/Starcraft/Errors"

export WINEPREFIX="$GAME_ROOT/prefix"
export PROTONPATH=GE-Proton
export GAMEID="umu-cosmonarchy-bootdiag-$BUILD"
export PROTON_VERB=waitforexitandrun

mkdir -p "$LOG_DIR"
pkill -9 -f "Xvfb $DISPLAY_NUM" 2>/dev/null
sleep 1
Xvfb "$DISPLAY_NUM" -screen 0 1280x720x24 -nolisten tcp >"$LOG_DIR/xvfb-boot.log" 2>&1 &
XPID=$!
sleep 3

cd "$BASE/$BUILD" || exit 1
DISPLAY="$DISPLAY_NUM" WAYLAND_DISPLAY= setsid umu-run "C:\\Cosmonarchy\\$BUILD\\Cosmonarchy BW.exe" \
    >"$LOG_DIR/run_boot.log" 2>&1 </dev/null &
GPID=$!

p0=$(wc -c < "$PLOG" 2>/dev/null || echo 0)
r0=$(wc -c < "$RLOG" 2>/dev/null || echo 0)
for i in $(seq 1 $((TOTAL / 15))); do
    sleep 15
    t=$((i * 15))
    game=$(pgrep -cf "Cosmonarchy BW.exe" 2>/dev/null || echo 0)
    star=$(pgrep -cf "Starcraft.exe" 2>/dev/null || echo 0)
    pn=$(wc -c < "$PLOG" 2>/dev/null || echo 0)
    rn=$(wc -c < "$RLOG" 2>/dev/null || echo 0)
    err=$(ls "$ERRDIR"/*.ERR 2>/dev/null | wc -l)
    DISPLAY="$DISPLAY_NUM" import -window root "$LOG_DIR/boot-t$t.png" 2>/dev/null
    sz=$(stat -c %s "$LOG_DIR/boot-t$t.png" 2>/dev/null || echo 0)
    echo "t${t}s launcher=$game starcraft=$star pluslog=$pn(+$((pn-p0))) rollbacklog=$rn(+$((rn-r0))) errs=$err shot=$sz"
    [ "$game" = "0" ] && [ "$star" = "0" ] && echo "t${t}s: all game processes gone" && break
done

# Teardown.
wineserver -k 2>/dev/null
pkill -9 -f 'Cosmonarchy BW.exe' 2>/dev/null
kill -9 "$XPID" 2>/dev/null
echo "done"
