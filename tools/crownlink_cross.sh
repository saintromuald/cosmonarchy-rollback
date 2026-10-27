#!/usr/bin/env bash
# Two-machine CrownLink match: host on this machine, joiner on the peer over ssh.
#
# Both sides run the same tree (the peer's is rsynced beforehand — see the header of
# two_client_test.sh for the same-tree requirement), the same patched CrownLink.snp
# and the same plugin. The host creates the lobby; the joiner finds the game in the
# public/private games list and joins; the host then starts the match.
#
#   tools/crownlink_cross.sh [delay_ms] [soak_secs]
#
# Env:
#   PEER        ssh host (default saintaquinas)
#   PEER_ROOT   game tree on the peer (default ~/Games/cosmonarchy-netcode)
#   PEER_REPO   cosmonarchy-rollback repo on the peer (default ~/Projects/cosmonarchy-rollback)
#   LOCAL_ROOT  game tree here (default ~/Games/cosmonarchy-agent2)
#   CL_MAP      map for the lobby (see crownlink_match.sh host_steps)
#   CL_PASSWORD lobby password (both sides; default rollback-test)
#
# delay_ms applies CL_SEND_DELAY_MS on both sides: a LAN has ~1 ms RTT, so turns arrive
# before the client needs them and nothing ever mispredicts. The delay is what makes the
# prediction + rollback path run for real on a LAN (it is the ONLY artificial knob; the
# protocol and determinism are exercised identically with or without it).
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
M="$ROOT/tools/crownlink_match.sh"
UI="$ROOT/tools/ui_session.sh"
PEER="${PEER:-saintaquinas}"
PEER_ROOT="${PEER_ROOT:-~/Games/cosmonarchy-netcode}"
PEER_REPO="${PEER_REPO:-~/Projects/cosmonarchy-rollback}"
LOCAL_ROOT="${LOCAL_ROOT:-$HOME/Games/cosmonarchy-agent2}"
DELAY="${1:-${CL_SEND_DELAY_MS:-0}}"
SOAK="${2:-60}"
HOST_S=a
JOIN_S=b

die() { echo "cross: $*" >&2; exit 1; }

# Clean slate on both machines.
pkill -9 -f 'Cosmonarchy BW\.exe$' 2>/dev/null
pkill -9 -f 'Xvfb :96 -screen' 2>/dev/null
ssh "$PEER" "pkill -9 -f 'Cosmonarchy BW\.exe$' 2>/dev/null; pkill -9 -f 'Xvfb :96 -screen' 2>/dev/null; true"
sleep 2

echo "cross: host on $HOST_S (:96, $LOCAL_ROOT), joiner on $PEER ($JOIN_S, $PEER_ROOT)"

# The host creates the lobby first (its creation tears down if the joiner restarts, and
# the joiner needs a game to find).
${DELAY:+CL_SEND_DELAY_MS=$DELAY} "$M" host "$HOST_S" :96 "$LOCAL_ROOT" 0
HOST_RC=$?
echo "cross: host steps done (rc $HOST_RC)"
[[ $HOST_RC -eq 0 ]] || die "host steps failed"

# The joiner (on the peer) boots and joins the lobby.
ssh "$PEER" "cd '$PEER_REPO' && CL_PASSWORD='${CL_PASSWORD:-rollback-test}' \
  ${DELAY:+CL_SEND_DELAY_MS=$DELAY} ./tools/crownlink_match.sh join '$JOIN_S' :96 '$PEER_ROOT' 1 alpha"
JOIN_RC=$?
echo "cross: join steps done (rc $JOIN_RC)"
[[ $JOIN_RC -eq 0 ]] || die "join steps failed"

# Both in the lobby: the host starts the match.
DISPLAY=:96 xdotool mousemove 931 589 click 1
echo "cross: match started; soaking ${SOAK}s"
sleep 45
DISPLAY=:96 import -window root /tmp/opencode/cross-host-ingame.png 2>/dev/null || true
sleep $((SOAK > 45 ? SOAK - 45 : 1))

# Gather both logs and report.
A_LOG="$LOCAL_ROOT/prefix/drive_c/Cosmonarchy/Release/cosmo_rollback.log"
B_LOG=/tmp/opencode/cross-join-cosmo_rollback.log
scp -q "$PEER:$PEER_ROOT/prefix/drive_c/Cosmonarchy/Release/cosmo_rollback.log" "$B_LOG" 2>/dev/null \
  || echo "cross: could not pull the peer log"

echo "== host log =="
"$M" report "$HOST_S" "$JOIN_S" "$A_LOG" "$B_LOG" || true

# Teardown.
"$UI" stop "$HOST_S" >/dev/null 2>&1 || true
ssh "$PEER" "cd '$PEER_REPO' && tools/ui_session.sh stop '$JOIN_S' >/dev/null 2>&1; pkill -9 -f 'Cosmonarchy BW\.exe$' 2>/dev/null; pkill -9 -f 'Xvfb :96 -screen' 2>/dev/null; true"
pkill -9 -f 'Cosmonarchy BW\.exe$' 2>/dev/null
pkill -9 -f 'Xvfb :96 -screen' 2>/dev/null
echo "cross: done"
