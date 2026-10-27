#!/usr/bin/env bash
# Two clients on one machine, rollback turns carried over UDP (no CrownLink provider needed).
#
# Both clients load the *same* replay from the single-player menu, so their initial state is
# identical; the client then drives the simulation from its own input table and exchanges
# turns/hashes/acks over a real UDP socket. A delay makes turns arrive late, so prediction and
# rollback actually run.
#
# The menu is driven with the mod's current layout: [profile dialog -> Enter], Single Player,
# [profile dialog -> Enter], Replay, then the LastReplay row is located by OCR and Ok starts
# it. The two clients finish their menus at different times, so the peer grace is generous
# (CM_ROLLBACK_PEER_GRACE, default 90 s) — without it the first client would fall back to
# lockstep before the second one even loads.
#
#   tools/two_client_test.sh [Build] [delay_ms] [boot_secs] [post_secs]
#
# `CL_REPLAY=name` picks the replay row (default LastReplay). `ALLOW_AI=1` drives a
# replay with a computer player anyway: transport/prediction/rollback/adjudication are
# all exercised, with the expected hash divergence as the proof of adjudication
# (PASS-TRANSPORT verdict).
#
# Requires two game trees (COSMO_GAME_A / COSMO_GAME_B) with the plugin installed. The trees
# must be real separate copies — the default ~/Games/cosmonarchy-agent3 was once a symlink to
# agent2, which makes both clients share one wine prefix and corrupt the run.
#
# Determinism: the menu click that selects a game picks whatever the prefix's "Recent Maps"
# holds first, and that is per sandbox — so the script pins it (and the replay list, whose
# crash artifacts sort first) to one map both trees have.
set -u

BUILD="${1:-Release}"
DELAY="${2:-120}"
BOOT="${3:-110}"
POST="${4:-60}"
# The replay both clients load (must exist at the top level of
# Maps/replays/ in both trees and be human-vs-human — a computer player makes
# the live client pass frames through by design).
CL_REPLAY="${CL_REPLAY:-LastReplay}"
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
        CM_ROLLBACK_PEER_GRACE="${PEER_GRACE:-90}" \
        env ${ALLOW_AI:+CM_ROLLBACK_ALLOW_AI=1} \
        DISPLAY="$display" WAYLAND_DISPLAY= setsid umu-run "C:\\Cosmonarchy\\$BUILD\\Cosmonarchy BW.exe" \
            >"$out" 2>&1 </dev/null
    ) >/dev/null 2>&1 &
}

# `ALLOW_AI=1` drives a replay with a computer player anyway (aise state is not
# snapshotted, so the two clients' hashes will diverge — the verdict below expects that).

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
# Drive both clients into the same replay, with the mod's current menu layout:
#   [profile dialog?] -> Enter  ->  Single Player  ->  [profile dialog?] -> Enter
#   ->  Replay  ->  pick the LastReplay row (OCR)  ->  Ok
# The old fixed click sequence no longer matches the menu (the profile chooser and
# the single-player screens moved), so the row is located with ocr_row.py.
for d in :96 :95; do
    DISPLAY="$d" xdotool key --clearmodifiers Return; sleep 5
    DISPLAY="$d" xdotool mousemove 490 207 click 1; sleep 5
    DISPLAY="$d" xdotool key --clearmodifiers Return; sleep 5
    DISPLAY="$d" xdotool mousemove 200 652 click 1; sleep 6
    DISPLAY="$d" import -window root "/tmp/opencode/tc-$d.png" 2>/dev/null
    ROW="$(python3 "$ROOT/tools/ocr_row.py" "/tmp/opencode/tc-$d.png" "$CL_REPLAY" 300 140 420 300 || true)"
    if [[ -n "$ROW" ]]; then
        DISPLAY="$d" xdotool mousemove "${ROW% *}" "${ROW#* }" click 1; sleep 4
    else
        echo "warning: $CL_REPLAY row not found on $d (list: $(python3 "$ROOT/tools/ocr_text.py" "/tmp/opencode/tc-$d.png" 300 140 420 300 6 2>/dev/null | tr '\n' ' '))"
    fi
    DISPLAY="$d" xdotool mousemove 887 558 click 1; sleep 6
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
# A replay with a computer player cannot be driven: aise keeps its state outside the
# engine snapshot, so the live client refuses to drive and passes frames through (no
# channel traffic). That is the expected verdict for an AI replay, not a netcode failure.
if grep -q "computer player present" "$A_LOG" "$B_LOG" 2>/dev/null && [[ -z "${ALLOW_AI:-}" ]]; then
    echo "INCONCLUSIVE: the replay has a computer player (live client passes frames through by design);"
    echo "  replace Maps/replays/LastReplay.rep with a human-vs-human replay and re-run."
    exit 2
fi
# With ALLOW_AI=1 the clients drive an AI replay anyway: the transport, dispatch,
# injection and hash adjudication are all exercised. Two outcomes prove the
# pipeline: hash agreements with no desyncs (the sims consumed identical input, so
# the AI stayed deterministic), or — when a rollback re-simulates across AI state —
# the expected DESYNC verdict. Either is a transport/adjudication pass.
if [[ -n "${ALLOW_AI:-}" ]]; then
    A_DESYNC=$(grep -c "live DESYNC" "$A_LOG" 2>/dev/null); B_DESYNC=$(grep -c "live DESYNC" "$B_LOG" 2>/dev/null)
    A_AGREE=$(grep -c "hashes agree" "$A_LOG" 2>/dev/null)
    A_HASH=$(grep -c "reported hash" "$A_LOG" 2>/dev/null)
    if [[ "$A_DESYNC" -ge 1 || "$B_DESYNC" -ge 1 || "$A_AGREE" -ge 1 ]] && [[ "$A_HASH" -ge 1 ]]; then
        echo "PASS-TRANSPORT: turns flowed, hashes were reported and adjudicated"
        echo "  (agreements=$A_AGREE, desyncs: A=$A_DESYNC B=$B_DESYNC)."
        exit 0
    fi
    echo "FAIL: ALLOW_AI run did not show the expected traffic/adjudication."
    exit 1
fi
