#!/usr/bin/env bash
# Persistent, scriptable game session under a virtual X display.
#
# `headless_probe.sh` boots, clicks, screenshots and exits in one shot. Exploring a menu
# (multiplayer, CrownLink provider, game list) needs the other shape: keep the game alive
# and drive it a step at a time.
#
#   tools/ui_session.sh start  <name> <display> [game_root] [build]
#   tools/ui_session.sh shot   <name> <out.png>
#   tools/ui_session.sh click  <name> "x,y;x,y;..."      (5s between clicks)
#   tools/ui_session.sh key    <name> "Return"           (xdotool key)
#   tools/ui_session.sh type   <name> "text"
#   tools/ui_session.sh loaded <name>                    (exit 0 once the plugin is up)
#   tools/ui_session.sh log    <name> [lines]
#   tools/ui_session.sh stop   <name>
#
# Each session gets its own Xvfb display, wine prefix (via the game root) and log dir under
# /tmp/opencode/ui-<name>/. Ctrl-C safe: `stop` kills only this session's processes by PID.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CMD="${1:-}"
NAME="${2:-}"
STATE="/tmp/opencode/ui-$NAME"
mkdir -p "$STATE"

die() { echo "$*" >&2; exit 1; }
[[ -n "$CMD" && -n "$NAME" ]] || die "usage: $0 start|shot|click|key|type|loaded|log|stop <name> ..."

case "$CMD" in
start)
    DISPLAY_NUM="${3:-:98}"
    GAME_ROOT="${4:-$HOME/Games/cosmonarchy-agent2}"
    BUILD="${5:-Release}"
    BASE="$GAME_ROOT/prefix/drive_c/Cosmonarchy"

    [[ -d "$BASE/$BUILD" ]] || die "missing $BASE/$BUILD"
    [[ -f "$STATE/xvfb.pid" ]] && die "session $NAME already started (stop it first)"

    pkill -9 -f "Xvfb $DISPLAY_NUM" 2>/dev/null
    sleep 1
    echo "$GAME_ROOT" >"$STATE/root"
    echo "$BUILD" >"$STATE/build"
    Xvfb "$DISPLAY_NUM" -screen 0 1280x720x24 -nolisten tcp >"$STATE/xvfb.log" 2>&1 &
    echo $! >"$STATE/xvfb.pid"
    echo "$DISPLAY_NUM" >"$STATE/display"
    sleep 3

    (
        cd "$BASE/$BUILD" || exit 1
        # Passthrough for the rollback env knobs (the gate's CM_ROLLBACK_SHADOW, the
        # live client's CM_ROLLBACK_LIVE/SLOT/UDP, etc.) so a scripted session can
        # drive any mode.
        for var in $(env | grep -o '^CM_ROLLBACK_[A-Z0-9_]*'); do
            export "$var"
        done
        WINEPREFIX="$GAME_ROOT/prefix" PROTONPATH=GE-Proton \
        GAMEID="umu-cosmo-$NAME-$BUILD" PROTON_VERB=waitforexitandrun \
        DISPLAY="$DISPLAY_NUM" WAYLAND_DISPLAY= setsid umu-run "C:\\Cosmonarchy\\$BUILD\\Cosmonarchy BW.exe" \
            >"$STATE/run.log" 2>&1 </dev/null
    ) >/dev/null 2>&1 &
    echo $! >"$STATE/game.pid"
    echo "started $NAME on $DISPLAY_NUM ($GAME_ROOT)"
    ;;

shot)
    OUT="${3:-$STATE/shot.png}"
    DISPLAY="$(cat "$STATE/display")" import -window root "$OUT" 2>"$STATE/import.log"
    echo "$OUT"
    ;;

click)
    DISPLAY="$(cat "$STATE/display")"
    IFS=';' read -ra CLICKS <<<"${3:-}"
    for c in "${CLICKS[@]}"; do
        [ -z "$c" ] && continue
        DISPLAY="$DISPLAY" xdotool mousemove "${c%,*}" "${c#*,}" click 1
        sleep 5
    done
    ;;

key)
    DISPLAY="$(cat "$STATE/display")" xdotool key --clearmodifiers "${3:-Return}"
    ;;

type)
    DISPLAY="$(cat "$STATE/display")" xdotool type --delay 80 "${3:-}"
    ;;

loaded)
    # The plugin's own log lives next to the game, not in the session dir, so accept either:
    # a game log passed in via $CM_LOG (absolute path) or the launcher output.
    for _ in $(seq 1 120); do
        if grep -q "v0.1.0 loaded" "${CM_LOG:-$STATE/run.log}" 2>/dev/null; then exit 0; fi
        sleep 3
    done
    exit 1
    ;;

log)
    tail -n "${3:-40}" "$STATE/run.log"
    ;;

stop)
    # Kill only this session: each session owns a distinct wine prefix, so `wineserver -k`
    # reaches just its own processes (never a second client on another prefix).
    #
    # Do NOT kill stored PIDs: the launcher subshell exits as soon as the game is up, and
    # that PID gets reused (once by the *other* session's Xvfb, which this then killed).
    # Match the display name instead.
    if [[ -f "$STATE/root" ]]; then
        WINEPREFIX="$(cat "$STATE/root")/prefix" wineserver -k 2>/dev/null
    fi
    if [[ -f "$STATE/display" ]]; then
        pkill -9 -f "Xvfb $(cat "$STATE/display")" 2>/dev/null
    fi
    rm -f "$STATE/game.pid" "$STATE/xvfb.pid"
    echo "stopped $NAME"
    ;;

*)
    die "unknown command: $CMD"
    ;;
esac
