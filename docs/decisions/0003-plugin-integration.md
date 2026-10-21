# ADR-0003 — 1.16.1 plugin integration: QDP/samase plugin, `samase_shim`, and the hook boundary

- **Status:** Accepted (proposed)
- **Date:** 2026-10-10
- **Supersedes:** ADR-0001 §2 (where the DLL lives / hook ownership) and refines its §3 (range table) with a concrete, grounded source.
- **Grounding:** `reference/CrownLink`, `reference/ShieldBattery`, and the local checkouts `aise` (`~/.cargo/git/checkouts/aise-*/*/`), `samase_plugin`/`samase_shim` (github.com/neivv/samase_plugin).

## Context

ADR-0001 chose "a new injected Rust DLL alongside the QDP plugins" and a
hand-transcribed 1.16.1 range table. Investigation of how the Cosmonarchy plugins
actually load and resolve memory changed two facts:

1. **`.qdp` plugins are DLLs loaded by the QDP hotloader.** The hotloader scans
   `plugins/*.qdp`, `LoadLibraryA`s each, and requires the export
   **`GetMPQDraftPlugin`** (it logs `Function GetMPQDraftPlugin not found!`; verified
   by disassembling `_qdp-hotloader.qdp`). `aise.qdp` exports `GetMPQDraftPlugin`,
   `Initialize`, and `samase_plugin_init`.
2. **1.16.1 globals/functions are resolved by `samase_shim`**, not by a bespoke
   analysis. `aise`'s `Initialize` (`aise/src/lib.rs:189-226`) calls
   `samase_shim::on_win_main(...)` → `samase_shim::init_1161()` → resolves vars/funcs
   from its own `whack_vars!`/`whack_funcs!` address tables in
   `samase_shim/src/bw.rs` (e.g. `game`=0x0057F0F0, `players`=0x0057EEE0,
   `units`=0x0059CCA8, `rng_seed`=0x0051CA14, `map_tile_flags`=0x006D1260,
   `first_active_unit`=0x00628430). These are the authoritative 1.16.1 addresses the
   GPTP plugins use — so the "hardcoded table" has a maintained, grounded source and
   needs no reverse-engineering.

Also relevant: `samase_shim` does **not** expose a `StepGameLogic` hook for 1.16.1
(`get_func(FuncId::StepGameLogic)` returns 0, `samase_shim/src/lib.rs:2231`) nor
`hook_game_loop_start` (stub, `:1894`). It does expose **`hook_step_objects(before,
after)`** — the once-per-frame object-step boundary `aise` uses for its `frame_hook`
(`aise/src/samase.rs:581-587`).

## Decision

1. **Ship the rollback code as a QDP samase plugin** (`crates/cosmo-rollback` →
   `plugins/zz-cosmo-rollback.qdp`), exporting `GetMPQDraftPlugin` (vtable whose
   `InitializePlugin` runs our init) and `Initialize`. It links `bw_dat` (v116) and
   `samase_shim`, exactly as `aise` does. `mpqdraft.rs` mirrors `aise/src/mpqdraft.rs`.
2. **Resolve 1.16.1 addresses via `samase_shim`** (its `bw.rs` tables) rather than a
   hand table. The range table (`crates/cosmo-rollback/src/ranges.rs`) transcribes
   those addresses with comments pointing at the source.
3. **Frame boundary: our own `whack` trampoline on `StepObjects` (0x004D94B0).**
   `hook_step_objects` is a *notification* hook (no `orig`), so it cannot re-run a
   step; re-simulation needs the original. We therefore install our own `whack`
   hook on `StepObjects` — the same address `samase_shim` hooks (`samase_shim/src/lib.rs:244-257`),
   resolved from `samase_shim/src/bw.rs:116` — which yields `orig`. The open question
   is whether `StepObjects` is the correct (full) rollback boundary or whether a
   higher-level trampoline is required (command processing/AI may sit outside it).
4. **Loading order:** name the QDP `zz-...` so it enumerates after the others, giving
   it the outermost hook among the plugins (the ADR-0001 "outermost trampoline" goal,
   approximated within the plugin mechanism).

## Consequences

### Positive
- No bespoke 1.16.1 address discovery; the addresses are maintained by the GPTP
  ecosystem and already used in the shipped `aise.qdp`.
- Uses the existing Fraud Launcher/hotloader plugin path; no new injection mechanism.
- The plugin is a real, loadable `.qdp` today (exports verified).

### Negative / risks
- **Multiple plugins each link their own `samase_shim` and patch the exe.** Our
  plugin calls `init_1161`/`on_win_main` like `aise`; whether two plugins hooking
  `WinMain`/`InitMpqs` chain cleanly must be verified at runtime (they evidently
  coexist for the existing plugin set).
- **`hook_step_objects` may be the wrong boundary** for rollback (it may not bracket
  command processing/AI). If so, we need a `StepGameLogic` address or the real samase
  host's `FuncId` table.
- The range table is still provisional (see `ranges.rs`); completeness is the audit's
  job.

## Open questions
1. **ANSWERED (2026-10-17): no — the boundary is not sufficient.** The hook wraps
   `0x004D9670` (gptp's `run_game_tick`), which is a *wall-clock-paced catch-up loop*: it
   runs `K = min([0x5124D4], frames until the wall clock reaches g_next_tick_ms 0x0051CE94)`
   logic frames per call, so one call is a variable number of frames and a re-sim of
   `f - g` calls cannot reproduce the original path. Symptom: units/images/sprites/orders
   diverge on every frame in fields the render/update pass touches (`Image` 0x0C flags,
   0x1C map_pos, 0x20 screen_pos, 0x28 grp_size; `Sprite` 0x14 position). A per-logic-frame
   hook is required; design in `docs/analysis/hook-boundary.md`.

   **SHARPENED (2026-10-18): there are two boundary problems, and the catch-up loop was
   only the first.** The back-edge NOP (`0x004D9823`, 2026-10-17) made one call = one
   logic frame, but `0x004D9670` is still not a **closed simulation step**: its only caller
   is the outer main loop (`0x004D9903`), and that loop runs a block of work *between*
   frames — `0x004D1BF0`, `0x004D14D0` (input), `0x47F0E0`, `0x485F70`
   (`receive_network_messages`), `0x47F590`, and after the frame `0x004D93F0`
   (`0x4C38B0/0x45EBC0/0x4599A0/0x458120/0x48DDC0`) and `0x41CA00`. The shadow gate
   snapshots at *entry* to `0x004D9670` and re-runs `orig` from the previous entry, so it
   compares **exit(frame f-1)** with **entry(frame f)** — which differ by exactly that
   between-frame work. No snapshot completeness can fix that. Evidence:
   `docs/analysis/restore-residual.md` (a single step *is* reproducible under
   `CM_ROLLBACK_DET`, yet the gate fails; the version-matched gptp serializer with Layer C
   + Phase 2 changes nothing; excluding the relocated scratch arrays still leaves
   `rng_seed` and the pools diverging). The fix is a boundary that encloses the sim-mutating
   part of the outer iteration — see that doc §3.
2. Do multiple samase plugins' `whack` patches coexist in the Cosmonarchy process?
   (Yes in practice — the plugin loads, hooks `0x004D9670`, and the gptp serializer
   resolves; see `cosmo_rollback.log`.)
3. Which snapshot ranges are actually allocated/present, and where do the pointer-
   based ranges (`pathing`, `map_tile_flags` tile array) really point? (Resolved for the
   object pools: they are CCMU-relocated and are now resolved at runtime by `pools.rs`.)

## Result (2026-10-17) — pools fixed, boundary identified as the remaining blocker

- **CCMU pool relocation found and fixed.** The stock pool addresses in `ranges.rs` /
  `image.rs` were dead: gptp's CCMU extender moves the unit/sprite/order/image/thingy/
  fow-sprite pools into `ccmu_extender` in `gptp.qdp` and rewrites every referencing
  operand. `pools.rs` now reads those operands back and validates them against the loaded
  module list. Live bases: units `0x02c1eef0`, sprites `0x02d9ad84`, orders `0x02df183c`,
  images `0x02e056fc`, thingies `0x02f15744`, fow `0x02f1d744`, bullets `0x02be6ef0`.
- **Measured effect:** the `rng_seed` divergence (previously on every frame) is gone, and
  `Game.elapsed_seconds` cleared once the trigger per-frame timers were added.
- **Remaining:** pool/render-field divergence every frame, caused by the boundary above.
- **Gate tooling:** `tools/gate.sh` runs the whole headless kill gate and
  `tools/log-summary.sh` reports it.

## Validation / next experiments
1. Install `zz-cosmo-rollback.qdp`, launch, start a game, confirm `cosmo_rollback.log`
   is written with the resolved range layout (proves loading + `init_1161` + hook). **Done.**
2. `CM_ROLLBACK_AUDIT_FRAME=<n>` a snapshot and confirm the bytes/hash (proves the
   ranges are valid mapped memory during play). **Done via `tools/gate.sh`.**
3. Resolve open question 1 on the real exe. **Done — see the Result section; the fix is
   pending implementation.**
