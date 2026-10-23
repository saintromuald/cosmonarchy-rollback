# cmscripts — the Cosmonarchy desync farm

Runs headless bot-vs-bot Cosmonarchy pairs continuously at maximum sim speed,
detects state-hash divergence between the two clients the moment it happens,
keeps the artifacts, and clusters the findings by signature.

## How a run works

1. `genstream.py` generates two identical-input command streams (one per
   player, hex turns per frame; `05` = no-op). Both clients receive the *same*
   streams, so any divergence the hash adjudication reports is a real desync
   (same inputs, different outputs) — never an input difference.
2. `worker.sh` boots two headless clients from the same pinned map (the proven
   `tools/two_client_test.sh` path) with the rollback plugin in live lockstep
   mode (`CM_ROLLBACK_PREDICT=0`) and the farm patches armed
   (`CM_ROLLBACK_UNPACED=1` removes the wall-clock pace gate,
   `CM_ROLLBACK_NO_RENDER=1` skips the draw pass — both in
   `crates/cosmo-rollback/src/farm.rs`).
3. The plugin injects the streamed turns every step on both clients and the
   host adjudicates exchanged desync hashes. On disagreement it voids the game
   and both clients log per-range fingerprints at the divergence frame.
4. `worker.sh` aborts on the first `voiding the game` / `live desync` line,
   collects both logs + streams + metadata, and `farmd.sh` records the outcome
   and (for desyncs) moves the run to `artifacts/`.
5. `triage.py` diffs the two clients' per-range fingerprints at the divergence
   frame, clusters by the set of differing ranges, and ranks the clusters.

## Setup (each machine)

```bash
cd ~/Projects/cosmonarchy-rollback
./cmscripts/machine-setup.sh          # builds plugin, clones trees, sizes slots
./cmscripts/farmctl run-once          # one smoke round, foreground
./cmscripts/farmctl report            # triage output
./cmscripts/farmctl install           # systemd timer (FARM_WINDOW, default 22:30-08:30)
./cmscripts/farmctl start             # arm the timer now
```

The farm state lives in `~/Games/cosmo-farm/`:

```
trees/slotN-{a,b}/     per-pair game trees (full copies; plugin installed)
runs/slotN/            in-flight/latest runs (worker.log + run-<seed>/)
artifacts/             desync runs moved here by farmd
streams/               generated command streams (last round per slot)
stats.json             per-slot run counters (farmctl status / report)
stop                   touch to make farmd finish its current run and exit
```

## Tuning (cmscripts/farm.conf, machine overrides in farm.local.conf)

- `FARM_SLOTS` — parallel pairs. Sized by machine-setup.sh (~1 pair per 5
  threads / 4 GB RAM). 16-thread/16 GB box → 2; 8-thread/64 GB → 1 (edit
  `farm.local.conf` and re-run machine-setup.sh to change).
- `FARM_WALL_LIMIT` — wall seconds per run; the run is unpaced, so 240 s ≈ a
  long game's worth of sim frames.
- `FARM_STREAM_DENSITY` — 1 command turn per N frames (600 → realistic spam).
- `FARM_WINDOW` — daily active window (24h); `always` disables.
- `FARM_MAP` — pinned map path; must exist in every tree.

## Interpreting results

- `clean` — the pair ran to the wall limit with every exchanged hash agreeing.
- `desync` — the host voided the game; both per-range fingerprints are in the
  artifact logs; `triage.py` names the differing ranges.
- `inconclusive` — takeover never matched (menu path broke), the game started
  with a computer player (`live` refuses to drive AI state), or a process died.
  Check `runs/slotN/worker.log`.

From a desync artifact, the fix workflow is the repo's usual one
(`docs/analysis/boundary-fix.md`): reproduce the same stream in the replay
harness, whole-pool diff, find the first differing field, fix, add the stream
to the gate corpus.

## Notes

- Both machines run the same repo via `cosmo-sync`; build/install per machine
  with `machine-setup.sh` (the peer needs its own trees and its own
  `farm.local.conf`).
- The farm plugin knobs are inert unless set: an unarmed game behaves exactly
  as before.
