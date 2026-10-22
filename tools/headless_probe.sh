#!/usr/bin/env bash
# Headless probe: boot Cosmonarchy under a virtual X display, click a point,
# screenshot the result, then tear the whole wine session down. Used to verify the
# plugin in the real game without touching the user's display.
#
# Usage: headless_probe.sh [Build] [click_x] [click_y] [out.png] [boot_secs]
set -u

BUILD="${1:-Release}"
CX="${2:-490}"
CY="${3:-182}"
OUT="${4:-/tmp/opencode/shot.png}"
BOOT="${5:-45}"
POST="${6:-0}"
# Isolation overrides (defaults preserve the original behaviour): point a probe at a
# copied game tree / a second X display so two probes can run side by side.
GAME_ROOT="${COSMO_GAME_ROOT:-$HOME/Games/cosmonarchy}"
DISPLAY_NUM="${COSMO_DISPLAY:-:99}"
LOG_DIR="${COSMO_LOG_DIR:-/tmp/opencode}"
BASE="$GAME_ROOT/prefix/drive_c/Cosmonarchy"

export WINEPREFIX="$GAME_ROOT/prefix"
export PROTONPATH=GE-Proton
export GAMEID="umu-cosmonarchy-${COSMO_GAMEID_SUFFIX:-}$BUILD"
export PROTON_VERB=waitforexitandrun
export CM_ROLLBACK_SHADOW="${CM_ROLLBACK_SHADOW:-3}"

mkdir -p "$LOG_DIR"
pkill -9 -f "Xvfb $DISPLAY_NUM" 2>/dev/null
sleep 1
Xvfb "$DISPLAY_NUM" -screen 0 1280x720x24 -nolisten tcp >"$LOG_DIR/xvfb.log" 2>&1 &
XPID=$!
sleep 3

cd "$BASE/$BUILD"
DISPLAY="$DISPLAY_NUM" WAYLAND_DISPLAY= setsid umu-run "C:\\Cosmonarchy\\$BUILD\\Cosmonarchy BW.exe" \
    >"$LOG_DIR/run_headless.log" 2>&1 </dev/null &

sleep "$BOOT"
DISPLAY="$DISPLAY_NUM" import -window root "${OUT%.png}.pre.png" 2>"$LOG_DIR/import.log"
if [ "$CX" != "none" ]; then
    IFS=';' read -ra CLICKS <<< "$CX"
    for c in "${CLICKS[@]}"; do
        [ "$c" = "none" ] && continue
        # `key:<name>` sends a key press (e.g. key:Return); `x,y,2` is a double-click.
        # Plain `x,y` stays a single click, exactly as before.
        if [[ "$c" == key:* ]]; then
            DISPLAY="$DISPLAY_NUM" xdotool key "${c#key:}"
            sleep 2
            continue
        fi
        x="${c%%,*}"
        rest="${c#*,}"
        y="${rest%%,*}"
        n="${rest#"$y"}"
        n="${n#,}"
        if [ "$n" = "2" ]; then
            DISPLAY="$DISPLAY_NUM" xdotool mousemove "$x" "$y" click --repeat 2 --delay 60 1
        else
            DISPLAY="$DISPLAY_NUM" xdotool mousemove "$x" "$y" click 1
        fi
        sleep 5
    done
fi
sleep "$POST"
DISPLAY="$DISPLAY_NUM" import -window root "$OUT" 2>"$LOG_DIR/import.log"
echo "screenshot: $OUT"

# Tear down the whole session so no wine process keeps our pipes open.
wineserver -k 2>/dev/null
pkill -9 -f 'Cosmonarchy BW.exe' 2>/dev/null
kill -9 "$XPID" 2>/dev/null
sleep 2
echo "done"
