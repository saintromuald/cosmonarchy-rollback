#!/usr/bin/env bash
# Drive a real CrownLink lobby on one machine: one client hosts, the other joins.
#
# The transport (ICE/relay) is CrownLink's; this script only presses the buttons the menus
# need, in order. Coordinates are for 1280x720: the game renders 640x480 centred, so the
# menu occupies x=160..1120.
#
#   tools/crownlink_match.sh run   [host_session] [host_display] [host_root] [join_session] [join_display] [join_root] [game]
#   tools/crownlink_match.sh host  <session> <display> <root> [slot]          (game must already be up)
#   tools/crownlink_match.sh join  <session> <display> <root> [slot] [game]   (game must already be up)
#   tools/crownlink_match.sh boot  <session> <display> <root>
#
# `host`/`join` take an optional slot (0/1): passing one arms the live client
# (CM_ROLLBACK_LIVE + CM_ROLLBACK_SLOT), which is how a two-machine match arms each
# side — see tools/crownlink_cross.sh.
#
# `run` starts both clients first and only then drives them: restarting a session while the
# other is in a lobby tears the lobby down (wine/proton teardown is not prefix-scoped in
# practice), which made the host vanish the moment the joiner booted.
#
# The lobby uses a shared password (`CL_PASSWORD`, default `rollback-test`) so the public
# games list only shows our own game — otherwise the list re-orders as other people
# advertise, and the selection changes under the cursor between click and confirm.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
S="$ROOT/tools/ui_session.sh"
CMD="${1:-}"

die() { echo "$*" >&2; exit 1; }

state() { echo "/tmp/opencode/ui-$1"; }

session_display() { cat "$(state "$1")/display"; }

boot_session() { # name display root slot
    local name="$1" display="$2" root="$3" slot="${4:-}"
    "$S" stop "$name" >/dev/null 2>&1
    local cfg="$root/prefix/drive_c/Cosmonarchy/Starcraft/CrownLink.json"
    if [[ -f "$cfg" ]]; then
        python3 - "$cfg" <<'PY'
import json, os, sys
path = sys.argv[1]
data = json.load(open(path))
data["log_level"] = 6  # trace: the SNP logs every provider send/receive
data["lobby_password"] = os.environ.get("CL_PASSWORD", "rollback-test")
json.dump(data, open(path, "w"), indent=4)
PY
    fi
    # Truncate the plugin log: it survives runs, and a stale "loaded" line makes the wait
    # below return instantly (so the script clicks a game that is still booting).
    : >"$root/prefix/drive_c/Cosmonarchy/Release/cosmo_rollback.log"
    # Arm the live client when a slot is given: the plugin then drives the simulation from
    # its own input table and carries turns over the CrownLink channel.
    # Per-slot carrier override: a two-client UDP soak starts the match through the lobby but
    # sends rollback turns over UDP instead of the CrownLink channel.
    if [[ "$slot" == "0" && -n "${CM_HOST_UDP:-}" ]]; then
        export CM_ROLLBACK_UDP="$CM_HOST_UDP"
    elif [[ "$slot" == "1" && -n "${CM_JOIN_UDP:-}" ]]; then
        export CM_ROLLBACK_UDP="$CM_JOIN_UDP"
    fi
    if [[ -n "$slot" ]]; then
        export CM_ROLLBACK_LIVE=1
        export CM_ROLLBACK_SLOT="$slot"
        # Plugin-state snapshots are excluded by default (the shipped plugins have no
        # serializers); CM_ROLLBACK_NO_PLUGIN=0 runs the shipped-match configuration.
        if [[ "${CM_ROLLBACK_NO_PLUGIN:-}" != "0" ]]; then
            export CM_ROLLBACK_NO_PLUGIN=1
        fi
        # One-way delay on our own channel messages, so a same-machine match still mispredicts
        # and therefore exercises prediction + rollback for real.
        if [[ -n "${CL_SEND_DELAY_MS:-}" ]]; then
            export CM_ROLLBACK_SEND_DELAY_MS="$CL_SEND_DELAY_MS"
        fi
        export CM_ROLLBACK_HASH_EVERY="${CM_ROLLBACK_HASH_EVERY:-4}"
    fi
    "$S" start "$name" "$display" "$root" Release >/dev/null
    for _ in $(seq 1 60); do
        grep -q "v0.1.0 loaded" "$root/prefix/drive_c/Cosmonarchy/Release/cosmo_rollback.log" 2>/dev/null && break
        sleep 3
    done
    sleep 8
}

# Run a helper against a session's display.
onscreen() { local name="$1"; shift; DISPLAY="$(session_display "$name")" "$@"; }
click() { local name="$1" x="$2" y="$3" wait="${4:-3}"; onscreen "$name" xdotool mousemove "$x" "$y" click 1; sleep "$wait"; }
dblclick() { local name="$1" x="$2" y="$3" wait="${4:-3}"; onscreen "$name" xdotool mousemove "$x" "$y" click --repeat 2 --delay 150 1; sleep "$wait"; }
shot() { "$S" shot "$1" "$(state "$1")/$2.png" >/dev/null; }

pick_row() { # session text [x y w h]
    local name="$1" text="$2"; shift 2
    local img; img="$(state "$name")/pick.png"
    onscreen "$name" import -window root "$img" 2>/dev/null
    python3 "$ROOT/tools/ocr_row.py" "$img" "$text" "$@"
}

pick_map() { pick_row "$1" "$2" 250 190 470 180; }

# Walk the map browser back to the map root. A fresh install opens it wherever it was last
# (the live game's is often `replays`), so a fixed down-navigation would miss.
goto_map_root() { # session
    local s="$1" i label xy
    for i in 1 2 3 4 5 6; do
        shot "$s" "maproot-$i"
        label="$(python3 "$ROOT/tools/ocr_text.py" "$(state "$s")/maproot-$i.png" 240 140 360 60 7)"
        # The pixel font reads "CMBW-maps" as e.g. "CNVBW -maps", so match loosely: the root is
        # the only level whose folder label contains "maps" ("melee", "replays", a class
        # folder, ... do not).
        [[ "${label,,}" == *maps* ]] && return 0
        xy="$(pick_row "$s" "Up One Level" 250 190 470 180)" || { sleep 1; continue; }
        read -r x y <<<"$xy"
        onscreen "$s" xdotool mousemove "$x" "$y" click 1
        sleep 2
    done
    return 1
}

open_multiplayer() { # session [profile]
    local s="$1" name="${2:-alpha}" title chooser
    click "$s" 361 298 4     # Multiplayer
    # A fresh install has no profile: the registry asks for a name first. Create one so the
    # harness works on a clean copy (a real player does this once, by hand).
    shot "$s" registry
    title="$(python3 "$ROOT/tools/ocr_text.py" "$(state "$s")/registry.png" 380 215 540 40 7)"
    if [[ "$title" == *[Ee]nter* || "$title" == *character* ]]; then
        "$S" type "$s" "$name"
        "$S" key "$s" Return
        sleep 2
        echo "  created profile '$name'"
        click "$s" 931 589 6     # Registry -> Ok
    else
        # A tree with several profiles shows the chooser (New/Delete/Ok/Cancel): Enter
        # accepts the highlighted profile (the chooser has focus at this point), which
        # proceeds straight to the games list.
        chooser="$(python3 "$ROOT/tools/ocr_text.py" "$(state "$s")/registry.png" 290 440 700 200 6 2>/dev/null)"
        if [[ "$chooser" == *[Nn]ew* && "$chooser" == *[Dd]elete* ]]; then
            "$S" key "$s" Return
            sleep 4
            echo "  profile chooser accepted"
        fi
    fi
}

host_steps() { # session
    local s="$1" xy x y
    open_multiplayer "$s" "${CL_HOST_PROFILE:-alpha}"
    shot "$s" mp
    click "$s" 353 500 6     # Create Game
    shot "$s" create
    goto_map_root "$s" || echo "  warning: map browser not at its root"
    # Walk the folder levels by OCR: a single click selects the row and Enter opens it
    # (the browser's Ok button does not open folders at every level, and the rows have
    # drifted from the old fixed coordinates).
    for folder in "melee" "1v1" "Class 0 - Testing Grounds" "1. Standard-Bearers"; do
        xy="$(pick_map "$s" "$folder")" || { shot "$s" "mapnav-$folder"; die "folder '$folder' not found"; }
        read -r x y <<<"$xy"
        onscreen "$s" xdotool mousemove "$x" "$y" click 1
        sleep 2
        "$S" key "$s" Return
        sleep 4
    done
    shot "$s" maplist
    # The lobby carries only the map's *name*, and this install has maps whose names collide
    # after the version suffix is stripped (two "Pagan Imperium"), which makes the joiner load
    # the wrong file and fail the join. Pick a map whose name is unique.
    local map="${CL_MAP:-Sylphid}"
    local xy mx my
    xy="$(pick_map "$s" "$map")" || { shot "$s" maplist-fail; die "map '$map' not found"; }
    read -r mx my <<<"$xy"
    onscreen "$s" xdotool mousemove "$mx" "$my" click 1
    sleep 2
    "$S" key "$s" Return
    sleep 5
    shot "$s" lobby
}

join_steps() { # session game
    local s="$1" want="${2:-alpha}" title="" attempt xy x y
    open_multiplayer "$s" "${CL_JOIN_PROFILE:-beta}"
    shot "$s" games
    for attempt in 1 2 3 4 5; do
        xy="$(pick_row "$s" "$want")" || { sleep 3; continue; }
        read -r x y <<<"$xy"
        onscreen "$s" xdotool mousemove "$x" "$y" click 1
        shot "$s" "select-$attempt"
        title="$(python3 "$ROOT/tools/ocr_text.py" "$(state "$s")/select-$attempt.png" 840 110 200 30 7)"
        echo "  attempt $attempt: clicked ($x,$y), title reads '$title'"
        [[ "${title,,}" == *"${want,,}"* ]] && break
        sleep 2
    done
    [[ "${title,,}" == *"${want,,}"* ]] || die "could not select '$want' (last title '$title')"
    click "$s" 900 559 12    # Ok -> join
    shot "$s" joined
}

case "$CMD" in
host|join|boot)
    NAME="${2:-}"; DISPLAY_NUM="${3:-}"; GAME_ROOT="${4:-}"; SLOT="${5:-${CM_SLOT:-}}"
    [[ -n "$NAME" && -n "$DISPLAY_NUM" && -n "$GAME_ROOT" ]] || die "usage: $0 $CMD <session> <display> <root> [slot] [game]"
    boot_session "$NAME" "$DISPLAY_NUM" "$GAME_ROOT" "$SLOT"
    case "$CMD" in
        host) host_steps "$NAME" ;;
        join) join_steps "$NAME" "${6:-alpha}" ;;
    esac
    ;;
run)
    # run [host_session] [host_display] [host_root] [join_session] [join_display] [join_root] [game]
    HOST_S="${2:-a}"; HOST_D="${3:-:96}"; HOST_R="${4:-$HOME/Games/cosmonarchy-agent2}"
    JOIN_S="${5:-b}"; JOIN_D="${6:-:95}"; JOIN_R="${7:-$HOME/Games/cosmonarchy-agent3}"
    GAME="${8:-alpha}"

    # Both clients up before either touches a lobby: a later restart would take the host's
    # lobby down with it.
    boot_session "$HOST_S" "$HOST_D" "$HOST_R" "${CM_HOST_SLOT:-0}"
    boot_session "$JOIN_S" "$JOIN_D" "$JOIN_R" "${CM_JOIN_SLOT:-1}"

    host_steps "$HOST_S"
    join_steps "$JOIN_S" "$GAME"
    echo "run: lobby ready (host $HOST_S, joiner $JOIN_S); starting the game"
    click "$HOST_S" 900 559 5   # lobby Ok starts the match
    sleep "${CM_START_WAIT:-45}"
    shot "$HOST_S" ingame
    shot "$JOIN_S" ingame
    echo "run: match started"
    ;;
report)
    # Summarise a match from the two plugin logs.
    A="${2:-a}"; B="${3:-b}"
    A_LOG="${4:-$HOME/Games/cosmonarchy-agent2/prefix/drive_c/Cosmonarchy/Release/cosmo_rollback.log}"
    B_LOG="${5:-$HOME/Games/cosmonarchy-agent3/prefix/drive_c/Cosmonarchy/Release/cosmo_rollback.log}"
    count() { local n; n=$(grep -c "$2" "$1" 2>/dev/null); echo "${n:-0}"; }
    A_BOUND=$(count "$A_LOG" "live: transport")
    B_BOUND=$(count "$B_LOG" "live: transport")
    A_CARRIER=$(grep -m1 "live: transport" "$A_LOG" 2>/dev/null | sed 's/.*live: transport //')
    B_CARRIER=$(grep -m1 "live: transport" "$B_LOG" 2>/dev/null | sed 's/.*live: transport //')
    A_SEND=$(grep -o "channel send #[0-9]*" "$A_LOG" | tail -1 | grep -o "[0-9]*$"); A_SEND=${A_SEND:-0}
    B_SEND=$(grep -o "channel send #[0-9]*" "$B_LOG" | tail -1 | grep -o "[0-9]*$"); B_SEND=${B_SEND:-0}
    A_RECV=$(grep -o "channel recv #[0-9]*" "$A_LOG" | tail -1 | grep -o "[0-9]*$"); A_RECV=${A_RECV:-0}
    B_RECV=$(grep -o "channel recv #[0-9]*" "$B_LOG" | tail -1 | grep -o "[0-9]*$"); B_RECV=${B_RECV:-0}
    A_ROLLS=$(count "$A_LOG" "live rollback #")
    B_ROLLS=$(count "$B_LOG" "live rollback #")
    A_AGREE=$(count "$A_LOG" "hashes agree")
    A_DESYNC=$(count "$A_LOG" "live DESYNC")
    B_DESYNC=$(count "$B_LOG" "live DESYNC")
    A_STALL=$(grep -o "live stall #[0-9]*" "$A_LOG" | tail -1 | grep -o "[0-9]*$"); A_STALL=${A_STALL:-0}
    B_STALL=$(grep -o "live stall #[0-9]*" "$B_LOG" | tail -1 | grep -o "[0-9]*$"); B_STALL=${B_STALL:-0}

    echo "A ($A): carrier='$A_CARRIER' sent=$A_SEND recv=$A_RECV rollbacks=$A_ROLLS agreed=$A_AGREE desyncs=$A_DESYNC last_stall=$A_STALL"
    echo "B ($B): carrier='$B_CARRIER' sent=$B_SEND recv=$B_RECV rollbacks=$B_ROLLS desyncs=$B_DESYNC last_stall=$B_STALL"
    if [[ "$A_BOUND" -ge 1 && "$B_BOUND" -ge 1 && "$A_RECV" -ge 1 && "$B_RECV" -ge 1 \
          && "$A_DESYNC" -eq 0 && "$B_DESYNC" -eq 0 && "$A_AGREE" -ge 1 ]]; then
        echo "PASS: rollback turns crossed CrownLink, both clients rolled back and agreed"
    else
        echo "FAIL: see the two logs above"
        return 1 2>/dev/null || exit 1
    fi
    ;;
*)
    die "unknown command: ${CMD:-}. See the header of this script."
    ;;
esac
