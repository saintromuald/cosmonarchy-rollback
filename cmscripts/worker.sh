#!/usr/bin/env bash
# worker.sh <slot> <run_dir> — one desync-farm pair run.
#
# Boots two headless Cosmonarchy clients from the same map (single-player menu
# path, as tools/two_client_test.sh), arms the rollback plugin in live lockstep
# mode (CM_ROLLBACK_PREDICT=0) with the farm's scripted command streams and
# unpaced stepping, then watches for hash divergence until the wall clock runs
# out. Artifacts (logs, streams, metadata) land in <run_dir>.
#
# Exit codes: 0 clean run, 1 desync found, 2 inconclusive, 3 error.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SLOT="${1:?slot}"
RUN="${2:?run dir}"
mkdir -p "$RUN"

CONF="$ROOT/cmscripts/farm.conf"
[ -f "$CONF" ] && . "$CONF"
FARMDIR="${FARMDIR:-$HOME/Games/cosmo-farm}"
[ -f "$FARMDIR/farm.local.conf" ] && . "$FARMDIR/farm.local.conf"

TREE_A="${FARM_TREE_A:-$FARMDIR/trees/slot$SLOT-a}"
TREE_B="${FARM_TREE_B:-$FARMDIR/trees/slot$SLOT-b}"
BUILD="${FARM_BUILD:-Release}"
DISPLAY_A=":1$((SLOT * 2))0"
DISPLAY_B=":1$((SLOT * 2 + 1))0"
PORT_A=$((40000 + SLOT * 4))
PORT_B=$((40002 + SLOT * 4))
HASH_EVERY="${FARM_HASH_EVERY:-4}"
WALL_LIMIT="${FARM_WALL_LIMIT:-240}"
STREAM0="${FARM_STREAM0:-$RUN/slot0.txt}"
STREAM1="${FARM_STREAM1:-$RUN/slot1.txt}"
MAP="${FARM_MAP:-C:\Cosmonarchy\Starcraft\maps\CMBW-Root\CMBW-maps\melee\1v1\Class 0 - Testing Grounds\1. Standard-Bearers\(2)Pagan Imperium 1.0.scx}"
START="$(date +%s)"

die() { echo "$*" >&2; exit 3; }
[ -d "$TREE_A/prefix" ] || die "missing tree A: $TREE_A"
[ -d "$TREE_B/prefix" ] || die "missing tree B: $TREE_B"
[ -f "$STREAM0" ] || die "missing stream $STREAM0"

A_LOG="$TREE_A/prefix/drive_c/Cosmonarchy/$BUILD/cosmo_rollback.log"
B_LOG="$TREE_B/prefix/drive_c/Cosmonarchy/$BUILD/cosmo_rollback.log"

teardown() {
    for r in "$TREE_A" "$TREE_B"; do WINEPREFIX="$r/prefix" wineserver -k 2>/dev/null; done
    # Kill only OUR game processes (the user may run other Cosmonarchy instances):
    # match on the exe cmdline, then confirm the tree root is in the process env.
    for pid in $(pgrep -f 'Cosmonarchy BW\.exe$' 2>/dev/null); do
        if tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -qF "$TREE_A\|$TREE_B"; then
            kill -9 "$pid" 2>/dev/null
        fi
    done
    pkill -9 -f "Xvfb $DISPLAY_A" 2>/dev/null
    pkill -9 -f "Xvfb $DISPLAY_B" 2>/dev/null
}

# Pin each prefix's Recent Maps registry entry to the farm map, like
# tools/two_client_test.sh, so the menu path is deterministic.
for r in "$TREE_A" "$TREE_B"; do
    python3 - "$r/prefix/user.reg" "$MAP" <<'PY'
import re, sys
path, value = sys.argv[1], sys.argv[2]
text = open(path, encoding="utf-8", errors="surrogateescape").read()
replacement = '"Recent Maps"=str(7):"' + value.replace("\\", "\\\\") + '\\0"'
text, count = re.subn(r'^"Recent Maps"=str\(7\):.*$', lambda _: replacement, text, count=1, flags=re.M)
open(path, "w", encoding="utf-8", errors="surrogateescape").write(text)
print(f"{path}: {count} Recent Maps entries pinned")
PY
    rm -f "$r/prefix/drive_c/Cosmonarchy/Starcraft/Maps/replays/_LastCrashReplay.rep"
    rm -f "$r/prefix/drive_c/Cosmonarchy/Starcraft/Maps/replays/_LastReplay.rep"
    : > "$r/prefix/drive_c/Cosmonarchy/$BUILD/cosmo_rollback.log"
done

teardown
pkill -9 -f "Xvfb $DISPLAY_A" 2>/dev/null
pkill -9 -f "Xvfb $DISPLAY_B" 2>/dev/null
sleep 2
Xvfb "$DISPLAY_A" -screen 0 1280x720x24 -nolisten tcp >"$RUN/xvfb-a.log" 2>&1 &
Xvfb "$DISPLAY_B" -screen 0 1280x720x24 -nolisten tcp >"$RUN/xvfb-b.log" 2>&1 &
sleep 3

launch() { # root display local_port peer_port slot gameid out stream
    local root="$1" display="$2" local_port="$3" peer_port="$4" slot="$5" gameid="$6" out="$7" stream="$8"
    (
        cd "$root/prefix/drive_c/Cosmonarchy/$BUILD" || exit 1
        WINEPREFIX="$root/prefix" PROTONPATH=GE-Proton GAMEID="$gameid" PROTON_VERB=waitforexitandrun \
        CM_ROLLBACK_LIVE=1 \
        CM_ROLLBACK_SLOT="$slot" \
        CM_ROLLBACK_UDP="$local_port:$peer_port:120" \
        CM_ROLLBACK_NO_PLUGIN=1 \
        CM_ROLLBACK_HASH_EVERY="$HASH_EVERY" \
        CM_ROLLBACK_UNPACED=1 \
        CM_ROLLBACK_PEER_GRACE=60 \
        CM_ROLLBACK_TURN_STREAM="$stream" \
        CM_ROLLBACK_TURN_STREAM_1="$STREAM1" \
        DISPLAY="$display" WAYLAND_DISPLAY= setsid umu-run "C:\\Cosmonarchy\\$BUILD\\Cosmonarchy BW.exe" \
            >"$out" 2>&1 </dev/null
    ) >/dev/null 2>&1 &
}

wait_loaded() { # log timeout_secs
    local log="$1" timeout="${2:-150}" waited=0
    while (( waited < timeout )); do
        grep -q "v0.1.0 loaded" "$log" 2>/dev/null && return 0
        sleep 3
        waited=$((waited + 3))
    done
    return 1
}

echo "[worker slot=$SLOT] launching pair (streams $STREAM0 / $STREAM1)"
launch "$TREE_A" "$DISPLAY_A" "$PORT_A" "$PORT_B" 0 "umu-cosmo-farm-$SLOT-a" "$RUN/run-a.log" "$STREAM0"
wait_loaded "$A_LOG" 150 || { echo "[worker slot=$SLOT] client A did not load"; teardown; exit 3; }
launch "$TREE_B" "$DISPLAY_B" "$PORT_B" "$PORT_A" 1 "umu-cosmo-farm-$SLOT-b" "$RUN/run-b.log" "$STREAM1"
wait_loaded "$B_LOG" 150 || { echo "[worker slot=$SLOT] client B did not load"; teardown; exit 3; }

# Drive both clients through the menu into the pinned map. The Oct 2026
# widescreen build moved the menus, so navigation is OCR-driven (labels, not
# coordinates): title -> Single Player -> profile dialog (entry + Ok) ->
# Custom -> [melee] -> [1v1] -> Class 0 Testing Grounds -> 1. Standard-Bearers
# -> <map> -> Ok. Every step retries until its label appears or it times out.
NAV="$ROOT/cmscripts/ocrnav.py"
drive_menus() { # display image-prefix phase(nav|start)
    local D="$1" IMG="$2" PHASE="$3"
    local SN=0
    local shot
    shot() {
        DISPLAY="$D" import -window root "$IMG" 2>/dev/null
        SN=$((SN + 1))
        cp "$IMG" "$IMG.step$SN.png" 2>/dev/null
    }
    local step
    step() { echo "[$(date +%T)] [$D] $*"; }
    local has
    has() { python3 "$NAV" find "$IMG" "$1" ${2:-} 2>/dev/null | grep -qv "no match"; }
    # wait_label <label> <region args> <timeout> -> waits until the label is on screen
    wait_label() {
        local label="$1" region="$2" timeout="$3" waited=0
        while :; do
            shot
            if has "$label" "$region"; then return 0; fi
            waited=$((waited + 4)); [ "$waited" -ge "$timeout" ] && return 1
            sleep 4
        done
    }
    local clickit
    clickit() { # kind label [region args] — always acts on a fresh screenshot
        shot
        python3 "$NAV" "$1" "$D" "$IMG" "$2" ${3:-} >/dev/null 2>&1
    }
    # The profile dialog's "entry + Ok" step, then the campaign menu's Custom.
    handle_dialog_then_custom() {
        step "dialog: first list entry"
        python3 "$NAV" firstlist "$D" "$IMG" >/dev/null 2>&1
        sleep 2
        clickit click "Ok" "--x0 800 --y0 480 --x1 1150 --y1 640"
        wait_label "custom" "" 30 || { step "no campaign menu"; return 1; }
        step "campaign menu — clicking Custom"
        clickit click "custom"
        return 0
    }

    if [ "$PHASE" = "start" ]; then
        # Barrier phase: the map is already selected; both clients click Ok as
        # close together as possible so the live channel connects on time.
        wait_label "Ok" "--x0 800 --y0 480 --x1 1150 --y1 640" 60 \
            || { step "no Ok button"; return 1; }
        step "clicking Ok (game starts)"
        clickit click "Ok" "--x0 800 --y0 480 --x1 1150 --y1 640"
        return 0
    fi

    # --- nav phase -----------------------------------------------------------
    # 1. Title screen: click Single Player.
    wait_label "single" "" 240 || { step "no title screen"; return 1; }
    step "title found — clicking Single Player"
    clickit click "single"
    sleep 6
    # 2. Profile/registry dialog may appear (entry + Ok), then Custom.
    shot
    if has "custom"; then
        step "campaign menu direct"
        clickit click "custom"
    elif has "New" "--x0 200 --y0 400 --x1 700 --y1 620"; then
        step "profile dialog — entry + Ok"
        handle_dialog_then_custom || return 1
    else
        local waited=0
        while :; do
            sleep 4; waited=$((waited + 4)); shot
            if has "custom"; then
                step "campaign menu after wait"
                clickit click "custom"; break
            fi
            if has "New" "--x0 200 --y0 400 --x1 700 --y1 620"; then
                step "profile dialog after wait"
                handle_dialog_then_custom || return 1; break
            fi
            if [ "$waited" -ge 40 ]; then
                step "stuck after Single Player — Escape and retry"
                DISPLAY="$D" xdotool key Escape; sleep 4
                clickit click "single"
                waited=0
                if [ "$SN" -gt 40 ]; then
                    step "still stuck (gave up)"
                    return 1
                fi
            fi
        done
    fi
    # 3. Map browser: drill into melee/1v1/Class 0/Standard-Bearers and select
    #    the map row (do NOT start the game: the Ok click is the barrier phase).
    wait_label "melee" "" 40 || { step "no map browser"; return 1; }
    step "drilling: melee"
    clickit dclick "melee"
    wait_label "1v1" "" 30 || { step "no 1v1 folder"; return 1; }
    clickit dclick "1v1"
    wait_label "testing" "" 30 || { step "no Class 0 folder"; return 1; }
    clickit dclick "testing"
    wait_label "standard" "" 30 || { step "no Standard-Bearers folder"; return 1; }
    clickit dclick "standard"
    # The map row lives in the left list; the description panel also names the
    # map, so restrict the search to the list region.
    wait_label "$FARM_MAP_LABEL" "--x0 280 --y0 180 --x1 680 --y1 420" 60 \
        || { step "no map '$FARM_MAP_LABEL' in list"; return 1; }
    step "map row found — selecting"
    clickit click "$FARM_MAP_LABEL" "--x0 280 --y0 180 --x1 680 --y1 420"
    sleep 2
    : > "$IMG.navdone"
    step "nav complete — waiting at the map screen"
    return 0
}

MENU_PIDS=()
for D in "$DISPLAY_A" "$DISPLAY_B"; do
    if [ "$D" = "$DISPLAY_A" ]; then
        drive_menus "$D" "$RUN/menu-a.png" nav &
        MENU_PIDS+=($!)
    else
        drive_menus "$D" "$RUN/menu-b.png" nav &
        MENU_PIDS+=($!)
    fi
done
wait "${MENU_PIDS[@]}" 2>/dev/null || true

if [ -f "$RUN/menu-a.png.navdone" ] && [ -f "$RUN/menu-b.png.navdone" ]; then
    # Both clients are at the map screen: start the games in quick succession so
    # the rollback channel handshake has a peer on both ends.
    START_PIDS=()
    drive_menus "$DISPLAY_A" "$RUN/menu-a.png" start & START_PIDS+=($!)
    sleep 1
    drive_menus "$DISPLAY_B" "$RUN/menu-b.png" start & START_PIDS+=($!)
    wait "${START_PIDS[@]}" 2>/dev/null || true
else
    echo "[worker slot=$SLOT] menu nav failed on one or both clients"
    teardown
    exit 2
fi

# Wait for both clients to take over the simulation.
outcome="clean"
deadline=$((START + 480))
while :; do
    A_TO=$(grep -m1 "taking over at" "$A_LOG" 2>/dev/null | sed 's/.*taking over at //')
    B_TO=$(grep -m1 "taking over at" "$B_LOG" 2>/dev/null | sed 's/.*taking over at //')
    [ -n "$A_TO" ] && [ -n "$B_TO" ] && break
    if [ "$(date +%s)" -gt "$deadline" ]; then
        outcome="inconclusive"
        echo "[worker slot=$SLOT] inconclusive: no takeover (A='${A_TO:-}', B='${B_TO:-}')"
        break
    fi
    sleep 3
done

# The two clients start *separate* single-player games; their starting states
# must match exactly or every hash comparison is meaningless (e.g. the lobby's
# "Random" race rolled differently). Mismatch -> inconclusive, farmd retries.
if [ "$outcome" = "clean" ] && [ "$A_TO" != "$B_TO" ]; then
    outcome="inconclusive"
    echo "[worker slot=$SLOT] inconclusive: mismatched starts (A='$A_TO' B='$B_TO')"
fi

if [ "$outcome" = "clean" ]; then
    if grep -q "AI script present" "$A_LOG" "$B_LOG"; then
        outcome="inconclusive"
        echo "[worker slot=$SLOT] inconclusive: AI script present (match has a computer player)"
    else
        end=$(( $(date +%s) + WALL_LIMIT ))
        echo "[worker slot=$SLOT] games started — watching for desyncs for ${WALL_LIMIT}s"
        while [ "$(date +%s)" -lt "$end" ]; do
            if grep -q "voiding the game\|live desync" "$A_LOG" "$B_LOG"; then
                outcome="desync"
                echo "[worker slot=$SLOT] DESYNC detected"
                break
            fi
            alive=$(pgrep -f 'Cosmonarchy BW\.exe$' | wc -l)
            if [ "$alive" -lt 2 ]; then
                outcome="inconclusive"
                echo "[worker slot=$SLOT] inconclusive: game processes gone ($alive alive)"
                break
            fi
            sleep 2
        done
        if [ "$outcome" = "clean" ]; then
            if grep -q "hashes agree" "$A_LOG" "$B_LOG"; then
                echo "[worker slot=$SLOT] clean: hashes agreed"
            else
                outcome="inconclusive"
                echo "[worker slot=$SLOT] inconclusive: no hash agreement seen"
            fi
        fi
    fi
fi

FRAMES_A=$(grep -o "live: tick [0-9]* frame [0-9]*" "$A_LOG" | tail -1 | awk '{print $5}')
FRAMES_B=$(grep -o "live: tick [0-9]* frame [0-9]*" "$B_LOG" | tail -1 | awk '{print $5}')
AGREED=$(grep -c "hashes agree" "$A_LOG" "$B_LOG" | awk -F: '{s+=$2} END {print s}')
WAIT=$(( $(date +%s) - START ))

teardown
sleep 2

cp "$A_LOG" "$RUN/slot-a.log" 2>/dev/null || true
cp "$B_LOG" "$RUN/slot-b.log" 2>/dev/null || true
python3 - "$RUN/meta.json" "$SLOT" "$outcome" "$WAIT" "${FRAMES_A:-0}" "${FRAMES_B:-0}" "${AGREED:-0}" <<'PY'
import json, sys
path, slot, outcome, wait, fa, fb, agreed = sys.argv[1:8]
meta = {
    "slot": int(slot),
    "outcome": outcome,
    "wall_seconds": int(wait),
    "frames_a": int(fa),
    "frames_b": int(fb),
    "hash_agreements": int(agreed),
    "streams": ["slot0.txt", "slot1.txt"],
}
json.dump(meta, open(path, "w"), indent=2)
print("meta:", json.dumps(meta))
PY

case "$outcome" in
    clean) exit 0 ;;
    desync) exit 1 ;;
    inconclusive) exit 2 ;;
    *) exit 3 ;;
esac
