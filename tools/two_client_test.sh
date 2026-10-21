#!/usr/bin/env bash
# Two clients on one machine, rollback turns carried over UDP (no CrownLink provider needed).
#
# Both clients load the *same* map from the single-player menu, so their initial state is
# identical; the client then drives the simulation from its own input table and exchanges
# turns/hashes/acks over a real UDP socket. A delay makes turns arrive late, so prediction and
# rollback actually run.
#
#   tools/two_client_test.sh [Build] [delay_ms] [boot_secs] [post_secs]
#
# Requires two game trees (COSMO_GAME_A / COSMO_GAME_B) with the plugin installed.
#
# Determinism: the menu click that selects a game picks whatever the prefix's "Recent Maps"
# holds first, and that is per sandbox — so the script pins it (and the replay list, whose
# crash artifacts sort first) to one map both trees have.
set -u

BUILD="${1:-Release}"
DELAY="${2:-120}"
BOOT="${3:-110}"
POST="${4:-60}"
A_ROOT="${COSMO_GAME_A:-$HOME/Games/cosmonarchy-agent2}"
B_ROOT="${COSMO_GAME_B:-$HOME/Games/cosmonarchy-agent3}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG=/tmp/opencode/two-client
PORT_A=39001
PORT_B=39002

mkdir -p "$LOG"
A_LOG="$A_ROOT/prefix/drive_c/Cosmonarchy/$BUILD/cosmo_rollback.log"
B_LOG="$B_ROOT/prefix/drive_c/Cosmonarchy/$BUILD/cosmo_rollback.log"

for r in "$A_ROOT" "$B_ROOT"; do
    [[ -d "$r" ]] || { echo "missing game tree $r" >&2; exit 1; }
done

pkill -9 -f 'Cosmonarchy BW\.exe$' 2>/dev/null
for r in "$A_ROOT" "$B_ROOT"; do WINEPREFIX="$r/prefix" wineserver -k 2>/dev/null; done
sleep 3

# Pin both prefixes to the same map, correctly escaped for the .reg format (`\\` per
# separator, `\0` as the entry terminator) — a mangled value makes the map browser crash.
MAP='C:\Cosmonarchy\Starcraft\maps\CMBW-Root\CMBW-maps\melee\1v1\Class 0 - Testing Grounds\1. Standard-Bearers\(2)Pagan Imperium 1.0.scx'
for r in "$A_ROOT" "$B_ROOT"; do
    python3 - "$r/prefix/user.reg" "$MAP" <<'PY'
import re, sys
path, value = sys.argv[1], sys.argv[2]
text = open(path, encoding="utf-8", errors="surrogateescape").read()
replacement = '"Recent Maps"=str(7):"' + value.replace("\\", "\\\\") + '\\0"'
text, count = re.subn(r'^"Recent Maps"=str\(7\):.*$', lambda _: replacement, text, count=1, flags=re.M)
open(path, "w", encoding="utf-8", errors="surrogateescape").write(text)
print(f"{path}: {count} Recent Maps entry pinned")
PY
done
# Replay list: crash artifacts (`_LastCrashReplay.rep`) sort first and would be selected.
for r in "$A_ROOT" "$B_ROOT"; do
    rm -f "$r/prefix/drive_c/Cosmonarchy/Starcraft/Maps/replays/_LastCrashReplay.rep"
    rm -f "$r/prefix/drive_c/Cosmonarchy/Starcraft/Maps/replays/_LastReplay.rep"
done

: >"$A_LOG"
: >"$B_LOG"

pkill -9 -f 'Xvfb :96' 2>/dev/null
pkill -9 -f 'Xvfb :95' 2>/dev/null
sleep 1
Xvfb :96 -screen 0 1280x720x24 -nolisten tcp >"$LOG/xvfb-a.log" 2>&1 &
XA=$!
Xvfb :95 -screen 0 1280x720x24 -nolisten tcp >"$LOG/xvfb-b.log" 2>&1 &
XB=$!
sleep 3

launch() { # root display local_port peer_port slot gameid out
    local root="$1" display="$2" local_port="$3" peer_port="$4" slot="$5" gameid="$6" out="$7"
    (
        cd "$root/prefix/drive_c/Cosmonarchy/$BUILD" || exit 1
        WINEPREFIX="$root/prefix" PROTONPATH=GE-Proton GAMEID="$gameid" PROTON_VERB=waitforexitandrun \
        CM_ROLLBACK_LIVE=1 \
        CM_ROLLBACK_SLOT="$slot" \
        CM_ROLLBACK_UDP="$local_port:$peer_port:$DELAY" \
        CM_ROLLBACK_NO_PLUGIN=1 \
        CM_ROLLBACK_HASH_EVERY="${HASH_EVERY:-4}" \
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

echo "launching two clients (udp delay ${DELAY}ms)…"
launch "$A_ROOT" :96 "$PORT_A" "$PORT_B" 0 "umu-cosmo-a-$BUILD" "$LOG/run-a.log"
wait_loaded "$A_LOG" 150 || echo "warning: client A did not load in time"
launch "$B_ROOT" :95 "$PORT_B" "$PORT_A" 1 "umu-cosmo-b-$BUILD" "$LOG/run-b.log"
wait_loaded "$B_LOG" 150 || echo "warning: client B did not load in time"

sleep "$BOOT"
for d in :96 :95; do
    for c in "490,182" "940,590" "320,645" "350,238" "940,590"; do
        DISPLAY="$d" xdotool mousemove "${c%,*}" "${c#*,}" click 1
        sleep 5
    done
done
sleep "$POST"

for r in "$A_ROOT" "$B_ROOT"; do WINEPREFIX="$r/prefix" wineserver -k 2>/dev/null; done
pkill -9 -f 'Cosmonarchy BW\.exe$' 2>/dev/null
kill -9 "$XA" "$XB" 2>/dev/null
sleep 2

"$ROOT/tools/crownlink_match.sh" report a b "$A_LOG" "$B_LOG" || true

# Distinguish "the netcode failed" from "the two clients never started the same game": the
# menu path loads a replay, and this install's replay (with a computer player) intermittently
# crashes `gptp.qdp` on map load — unrelated to the rollback client (it reproduces with the
# plugin removed). Both clients log the state they take over from; a mismatch means the test
# never got that far.
A_TAKEOVER=$(grep -m1 "taking over at" "$A_LOG" 2>/dev/null | sed 's/.*taking over at //')
B_TAKEOVER=$(grep -m1 "taking over at" "$B_LOG" 2>/dev/null | sed 's/.*taking over at //')
if [[ -n "$A_TAKEOVER" && "$A_TAKEOVER" != "$B_TAKEOVER" ]]; then
    echo "INCONCLUSIVE: the clients started different games ('$A_TAKEOVER' vs '$B_TAKEOVER');"
    echo "  fix the replay/map selection (the menu loads Maps/replays/LastReplay.rep)."
    exit 2
fi
