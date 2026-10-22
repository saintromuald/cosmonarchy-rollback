# Save format, autosave and the save/load gate (M1)

M1 of the save/resume work delivers three things:

1. **A versioned save file format** (`crates/cosmo-rollback/src/savefile.rs`).
2. **Autosaves** during live rollback matches, taken only at **confirmed** frames.
3. **The save/load gate** (`tools/save-gate.sh`): proof on the real exe that a save
   serialized to the format, written to disk, read back and restored re-simulates
   identically.

Deliberately out of M1 scope: the resume protocol (loading a save into a fresh lobby,
M3) and desync rewind (M4). This milestone only proves the save is *faithful*; the
next milestone makes it *usable*.

## File format

```text
0x00  b"CMRB"              magic
0x04  u32                  version (SAVE_VERSION = 1)
0x08  u32                  frame  (frames completed by the saved state)
0x0c  u32                  flags  (FLAG_LIVE=1 has a session section; FLAG_HARNESS=2)
0x10  u32                  snapshot_len
0x14  snapshot bytes       (Snapshot::serialize)
      u32                  session_len (0 = none)
      session bytes        (Session::save_state)
      u32                  checksum: FNV-1a 32 over every byte before it
```

`read_save` verifies magic, version and checksum before returning anything. The
snapshot payload itself carries each range's **name and length** in order; on load,
`Snapshot::deserialize` compares that description against the running build's range
table (`ranges::engine_ranges()`) and refuses a save from a different build. A
corrupted, truncated or foreign file can therefore never be restored into game memory
— it fails with a logged error.

`Snapshot` serializes: flat engine ranges, plugin blobs (ADR-0005 Layers B/C, already
version-matched by the plugins), and the auxiliary captures (region, image, tiles,
creep, replay cursor, scratch). `Session` serializes the input table (per-slot turn
windows including gaps and lateness, frontier, misprediction mark, leaves), the sim
position (`current`/`confirmed`), the desync comparison state (peer/own hashes,
pending reports), the un-acked sent turns and the authority's latest lead/turn
counts. The only state dropped is `usable_at` wall-clock holds (a save is taken at a
confirmed step, so held turns are usable anyway) — held *delays* themselves are kept.

## The save/load gate

`tools/save-gate.sh [Build] [depth] [start]` boots a replay headlessly and, at every
frame ≥ `start`, restores the ring snapshot at or before `f − depth` **through the
file round-trip** (serialize → `cosmo_saves/save_gate.cosmo` → read → deserialize),
re-simulates `depth` frames (with the same between-frame work as the shadow gate)
and compares the state hash against the live pass. `SAVEGATE DIVERGENCE` means the
format lost state; `SAVEGATE ok` means the round trip is faithful. The script also
writes one full save at `CM_ROLLBACK_SAVE_AT` and re-reads it
(`CM_ROLLBACK_SAVE_VERIFY`), printing `saveverify ok`.

Knobs: `CM_ROLLBACK_SAVE_GATE`, `CM_ROLLBACK_SAVE_GATE_START` (default 90),
`CM_ROLLBACK_SAVE_AT`, `CM_ROLLBACK_SAVE_VERIFY`, `CM_ROLLBACK_SAVE_DIR` (default
`cosmo_saves`, relative to the game's working directory),
`CM_ROLLBACK_GATE_IGNORE` (same semantics as the shadow gate).

## Autosave

In a live match the client writes `cosmo_saves/autosave_<frame>.cosmo` every
`CM_ROLLBACK_AUTOSAVE_EVERY` frames (default 1440 ≈ 1 minute at Fastest), keeping the
newest `CM_ROLLBACK_AUTOSAVE_KEEP` (default 4). A save is only taken when the frame
just produced **equals** the confirmed frame: predicted state may still be corrected
by a rollback, and a save from it would not be a "no issue at all" resume point.
Autosaves are skipped when the client is not driving (menu, fallback to lockstep, AI
present).

## Why this is the foundation for desync recovery

A desync is state that diverged. If the cause is deterministic (a missed snapshot
range), rewinding both clients to a pre-desync save re-diverges at the same frame —
a save cannot fix that. What a confirmed-frame save *does* give:

- **Crash/disconnect recovery** ("restart with another client"): resume the match
  from the last agreed state in a fresh lobby (M3).
- **Bisection**: the desync fingerprint (`live fp`) plus a faithful save lets a
  tester load just before the divergence and re-run it.
- **Host-authority resync**: the host's state is truth (ADR-0001), so a desynced
  guest can be re-synced from the host's snapshot at the last agreed frame without
  pausing the host (M4) — strictly better than symmetric rewind.

## Validation status

- `tools/test.sh`: 49 unit tests pass, including writer/reader round trips, file
  checksum/magic rejection, pruning, and a full `Session` save → load → dispatch
  identity test.
- `tools/save-gate.sh`: **PASS on the real game** (build 8421086, 2026-10-22):
  every gate frame reports `SAVEGATE ok` — a snapshot serialized to the format,
  written to disk, read back, deserialized, restored and re-simulated reproduces the
  live state exactly — and the one-shot full save re-reads as `saveverify ok`
  (5.5 MB, hash-identical). The gate run ignores `sprite_count` (the draw pass
  allocates and frees helper sprites between frames; ADR-0007) and was run with
  `aise.qdp` removed, because its unsnapshotted heap state (Layer B, out of v1 scope)
  makes *any* re-simulation — including the plain shadow gate — diverge in
  `ai_script_pool`/`ai_town_pool`/`first_ai_script` on AI replays. Both residuals
  were confirmed to be the pre-existing engine classes, not the save format: the
  plain shadow gate shows the same divergences, and every `SAVEGATE DIVERGENCE`
  names only those ranges.

## Navigating the current menus (harness note)

Build 8421086 with `fastmenus` changed the click path: Single Player → a "Registry"
profile dialog (Enter accepts the default) → Watch Replay → select
`LastReplay.rep` → double-click starts it. `tools/headless_probe.sh` now supports
`key:<name>` and `x,y,2` (double-click) entries in the click list, backward
compatibly; `save-gate.sh` carries the current path.
