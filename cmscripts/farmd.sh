#!/usr/bin/env bash
# farmd.sh — the desync-farm daemon: runs pair workers continuously on this machine.
#
# One subshell per slot; each loop generates a fresh command stream (seed from
# wall clock + slot), runs tools/two_client_test-style pair via cmscripts/worker.sh,
# records the outcome, and repeats. Stops on $FARMDIR/stop, exits outside the
# daily window (FARM_WINDOW) so the systemd timer owns the schedule, and holds a
# lock so only one instance runs.
#
#   farmd.sh [--once]      --once runs a single round per slot and exits
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONF="$ROOT/cmscripts/farm.conf"
[ -f "$CONF" ] && . "$CONF"
FARMDIR="${FARMDIR:-$HOME/Games/cosmo-farm}"
[ -f "$FARMDIR/farm.local.conf" ] && . "$FARMDIR/farm.local.conf"
SLOTS="${FARM_SLOTS:-1}"
ONCE=0
[ "${1:-}" = "--once" ] && ONCE=1

mkdir -p "$FARMDIR/runs" "$FARMDIR/streams" "$FARMDIR/artifacts"
exec 9>"$FARMDIR/farm.lock"
flock -n 9 || { echo "farmd: already running (lock held)" >&2; exit 1; }
echo $$ >"$FARMDIR/farmd.pid"

log() { echo "$(date '+%F %T') $*"; }

inside_window() {
    [ "$FARM_WINDOW" = "always" ] && return 0
    local start="${FARM_WINDOW%-*}" end="${FARM_WINDOW#*-}" now hh
    now=$(date +%H:%M)
    if [[ "$start" < "$end" ]]; then
        [[ "$now" > "$start" && "$now" < "$end" ]]
    else
        [[ "$now" > "$start" || "$now" < "$end" ]]
    fi
}

record() { # slot outcome wall frames_a frames_b seed run
    python3 - "$FARMDIR/stats.json" "$FARMDIR/artifacts" "$1" "$2" "$3" "$4" "$5" "$6" "$7" <<'PY'
import json, os, sys, time
stats_path, art_dir, slot, outcome, wall, fa, fb, seed, run = sys.argv[1:10]
stats = json.load(open(stats_path)) if os.path.exists(stats_path) else {}
key = f"slot{slot}"
s = stats.get(key, {"runs": 0, "clean": 0, "desync": 0, "inconclusive": 0, "error": 0, "sim_frames": 0})
s["runs"] += 1
s[outcome] = s.get(outcome, 0) + 1
s["sim_frames"] += max(int(fa), int(fb))
s["last"] = time.strftime("%F %T")
stats[key] = s
stats["updated"] = time.strftime("%F %T")
json.dump(stats, open(stats_path, "w"), indent=2)
if outcome == "desync":
    dst = os.path.join(art_dir, f"{time.strftime('%Y%m%d-%H%M%S')}-slot{slot}-seed{seed}")
    os.rename(run, dst)
    print(f"artifact kept: {dst}")
PY
}

cleanup() {
    log "farmd: stopping (trap)"
    touch "$FARMDIR/stop"
    rm -f "$FARMDIR/farmd.pid"
}
trap cleanup INT TERM

log "farmd: up (slots=$SLOTS once=$ONCE window=$FARM_WINDOW wall_limit=$FARM_WALL_LIMIT)"
pids=()
for s in $(seq 0 $((SLOTS - 1))); do
    (
        exec 9>&-   # do not inherit the farm lock: a slot must not block a restart
        while :; do
            kill -0 "$PPID" 2>/dev/null || exit 0   # farmd died: stop looping
            [ -f "$FARMDIR/stop" ] && exit 0
            if [ "$ONCE" != "1" ] && ! inside_window; then
                log "farmd slot$s: outside window $FARM_WINDOW — exiting"
                exit 0
            fi
            seed=$(( $(date +%s) * 1000 + s + RANDOM ))
            mkdir -p "$FARMDIR/runs/slot$s"
            python3 "$ROOT/cmscripts/genstream.py" fuzz \
                --seed "$seed" --frames "$FARM_STREAM_FRAMES" \
                --density "$FARM_STREAM_DENSITY" --players 2 \
                --out "$FARMDIR/runs/slot$s" || { sleep 30; continue; }
            RUN="$FARMDIR/runs/slot$s/run-$seed"
            mkdir -p "$RUN"
            cp "$FARMDIR/runs/slot$s/slot0.txt" "$FARMDIR/runs/slot$s/slot1.txt" "$RUN/"
            START=$(date +%s)
            FARM_STREAM0="$RUN/slot0.txt" FARM_STREAM1="$RUN/slot1.txt" \
                "$ROOT/cmscripts/worker.sh" "$s" "$RUN" >>"$FARMDIR/runs/slot$s/worker.log" 2>&1
            rc=$?
            case $rc in
                0) outcome="clean" ;;
                1) outcome="desync" ;;
                2) outcome="inconclusive" ;;
                *) outcome="error" ;;
            esac
            wall=$(( $(date +%s) - START ))
            fa=$(grep -o "live: tick [0-9]* frame [0-9]*" "$RUN/slot-a.log" 2>/dev/null | tail -1 | awk '{print $5}')
            fb=$(grep -o "live: tick [0-9]* frame [0-9]*" "$RUN/slot-b.log" 2>/dev/null | tail -1 | awk '{print $5}')
            record "$s" "$outcome" "$wall" "${fa:-0}" "${fb:-0}" "$seed" "$RUN"
            log "farmd slot$s: $outcome (wall ${wall}s, frames a=${fa:-?} b=${fb:-?}, seed=$seed)"
            # Disk hygiene: keep the last 5 clean/inconclusive runs per slot.
            find "$FARMDIR/runs/slot$s" -maxdepth 1 -type d -name 'run-*' \
                | sort | head -n -5 | xargs -r rm -rf
            [ "$ONCE" = "1" ] && exit 0
            # No need to sleep between runs; the worker's boot soak is the cadence.
        done
    ) &
    pids+=($!)
done

if [ "$ONCE" = "1" ]; then
    wait "${pids[@]}"
    rm -f "$FARMDIR/farmd.pid"
    log "farmd: --once finished"
    exit 0
fi

wait "${pids[@]}"
log "farmd: all slots exited"
