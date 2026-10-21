# PORT-PLAN — rollback netcode for Cosmonarchy over CrownLink

Grounding: `docs/RECON.md`. Reference design: `reference/ShieldBattery/docs/rollback.md`.

## Goal

Every multiplayer match launched through **CrownLink** runs rollback netcode:
each client predicts remote turns, re-simulates on misprediction, and a relay/peer
adjudicates per-tick state hashes. Native lockstep sync (0x37) is disabled.

Non-goal (v1): observers, UMS, subfaction RNG edge cases, matchmaking/MMR — those come
after a human 1v1 works.

## Work split

- **flash** (cheap): recon, cloning, scaffolding, tooling, builds, harnesses, running
  analysis, docs, packaging, mechanical refactors.
- **deep** (V4 Pro, expensive): architecture decisions, 1.16.1 memory-layout reasoning,
  snapshot-range strategy, hook design, turn protocol / authority concurrency design,
  correctness-critical code.

## Milestones

| # | Milestone | Lane | State |
|---|-----------|------|-------|
| 0 | Recon both references + target binary | flash | **done** (`docs/RECON.md`) |
| 1 | Integration architecture + relay-vs-P2P authority decision | **deep** | **done** (`docs/decisions/0001-architecture.md`) |
| 2 | Cross-build toolchain for 32-bit Windows DLL | flash | **done** (see below) |
| 3 | 1.16.1 address/struct table + range specs behind the SB `RangeSpec` interface | flash+deep | **third cut**: CCMU pools resolved at runtime, 106 ranges; per-frame **scratch arrays are now excluded** (they are rebuilt, not state — see `docs/analysis/restore-residual.md`) |
| 4 | Plugin-state audit: what sim state the `.qdp` plugins own | **deep** | Layer C + Phase 2 built and re-tested (version-matched CI artifact) and shown **not** to be the gate's cause; Layer B (aise) is locally buildable with the `cosmo_aise_*` exports added |
| 5 | Hook + snapshot + restore + state hash on 1.16.1 | **deep** | **the hook boundary is the open blocker**: `0x004D9670` is not a closed step — the outer main loop advances `rng_seed`/orders/images between frames (`CM_ROLLBACK_BOUNDARY=1`, 145/145 frames). See `docs/analysis/restore-residual.md` |
| 6 | Turn transport over CrownLink (or new relay) | **deep** | **seam built**: fork `saintromuald/CrownLink` + CI + `cl_rollback_*` exports (CI-built, non-breaking); Rust side `transport::CrownLink` `Link`, seq-aware input table, desync-hash exchange, and the host `Authority` are implemented and unit-tested. Wiring into the engine path is pending |
| 7 | Harness / soak under Wine; determinism verification | flash | runnable (`tools/gate.sh`, `tools/headless_probe.sh` with `COSMO_GAME_ROOT` isolation) |
| 8 | Fraud Launcher injection + arming + packaging | flash | plugin loads via the hotloader; **release build verified** (399 KB); packaging into `saintromuald/cosmonarchy-bw-release` pending |

**ADR-0003 (plugin integration):** the rollback code ships as a **QDP samase plugin**
(`plugins/zz-cosmo-rollback.qdp`, exports `GetMPQDraftPlugin`/`Initialize`), uses
`samase_shim` for 1.16.1 address resolution, and for v1 hooks `hook_step_objects`
(before/after) as the per-frame boundary. Open question: is that boundary sufficient,
or does rollback need a real `StepGameLogic` trampoline?

## Current build state

`crates/cosmo-rollback` compiles to a loadable 32-bit Windows `.qdp` plugin with:
- `bw161/` — `bw_dat` v116 struct layer (size-asserted: `Game=0x17700`, `Unit=0x150`).
- `pools.rs` — resolves the live object-pool bases (CCMU-relocated on Cosmonarchy) by
  reading the operands the extender patched, validated against the loaded module list.
- `ranges.rs` — the 1.16.1 range table (88 ranges, ~3.2 MB): pools + pool free lists +
  list heads/tails + trigger timers + visibility-hash scalars; the `game` range is split
  around the viewer-local fields.
- `snapshot.rs` / `state_hash.rs` — capture/restore/hash core.
- `rollback/` — snapshot ring + **restore + re-simulation**; `CM_ROLLBACK_SHADOW=<depth>`
  re-simulates every frame and logs `DIVERGENCE` + the differing ranges and byte counts
  (the kill gate).
- `frame_hook.rs` — a `whack` trampoline on `0x004D9670` (`run_game_tick`). **Known wrong
  boundary:** that function is a wall-clock-paced catch-up loop, so one call is a variable
  number of logic frames; see `docs/decisions/0003-plugin-integration.md` (open Q1) and
  `docs/analysis/hook-boundary.md`.
- `audit.rs` — logs the resolved range layout and pool bases.
- `mpqdraft.rs` / `plugin.rs` — hotloader entry.

Run the gate: `tools/gate.sh Release 3 90` (builds, installs, boots headlessly into a
replay, prints the summary) or `tools/log-summary.sh` on an existing
`<build dir>/cosmo_rollback.log`. For the kill gate, launch with `CM_ROLLBACK_SHADOW=3`
and look for any `DIVERGENCE` lines.

**ADR-0001 decisions:** host-authority peer model over CrownLink (no new server); a new
injected Rust DLL owns the game-logic step as the **outermost** hook; engine range table
+ plugin `register_range` contract + ported audit as the verification net; v1 = 1v1 human
melee. **ADR-0002 revision:** ADR-0001 §3/§4 superseded — reuse neivv's `bw_dat` (v116, no
`scr-only`) for the struct layer, keep the exhaustive arrays as a hardcoded table verified
by the audit, and make the plugin contract a `register_state(save,load)` (reusing each
plugin's own save hook) plus `register_range` for flat globals. The **kill gate** is the
snapshot-completeness audit (ADR-0002 §3) driven to zero on the real exe.

## Milestone 2 — cross-build (flash) — DONE

Host is Linux; target is 32-bit Windows. What was set up:

- **Userspace rustup** (`~/.rustup`, shims in `~/.cargo/bin`, NOT on PATH) with the
  official **1.99.0** toolchain + `i686-`/`x86_64-pc-windows-msvc`. Needed because
  Arch's system `rustc` appends `(Arch Linux rust 1:1.99.0-1)` to its version string,
  which changes the crate metadata hash, so the official prebuilt `rust-std` is
  rejected with `E0514`. (The target std was first hand-installed under
  `/usr/lib/rustlib/` and did not work for this reason; it is otherwise inert.)
- **`cargo-xwin`** (wraps `clang-cl`/`lld-link`/`llvm-lib`, downloads the MSVC CRT/SDK).
  **`XWIN_ARCH=x86` is required** — without it xwin fetches only x86_64/aarch64 libs and
  linking fails on `kernel32.lib`.
- **Reproducible wrapper:** `tools/build.sh` (+ `rust-toolchain.toml`). It builds
  `crates/cosmo-rollback` and outputs `target/i686-pc-windows-msvc/debug/cosmo_rollback.dll`
  (verified: `PE32 … Intel i386 … DLL`).

**Finding:** ShieldBattery's own `game` crate **cannot be cross-built from Linux** — its
build script compiles D3D11 HLSL via the `compile-shaders` crate, which unconditionally
uses Windows-only `ID3DInclude`/`D3DCompile2`. Build scripts always run on the host, so
this needs a Windows host (their CI). This does not block the port: we reuse the rollback
*algorithm*, not SB's renderer/overlay/forge, so the Cosmonarchy DLL is a new crate
(`crates/cosmo-rollback`) with no D3D build step. (Everything else in SB's crate got
through dependency compilation before that failure.)

## Milestone 1 — the decision `deep` must make (hand-off)

Deliverable: `docs/decisions/0001-architecture.md` answering:

1. **Authority model** — relay-authority (ShieldBattery) vs host-authority over CrownLink.
   Must address: session clock ownership, hash adjudication, reconnect, who may cheat.
   Recommend one and justify against Cosmonarchy's constraint (crow_serve exists but is a
   dumb matchmaker; CrownLink is P2P).
2. **Snapshot strategy** — instrument plugins vs empirical range discovery. Must produce a
   concrete, ordered plan for how to enumerate every mutable sim byte on the Cosmonarchy
   exe + QDP plugins, and how to detect a missed range (SB's `SB_ROLLBACK_AUDIT_FRAME`
   analogue).
3. **Hook ownership** — own DLL vs `.snp`; ordering relative to hotloader/GPTP; how the
   game-logic step is intercepted so plugins still run and re-simulation is correct.
4. **Module plan** — what to fork from SB, what to rewrite; crate/ABI boundaries; the
   minimal v1 surface (1v1 human).
5. **Risks + kill criteria.**

## Known risks

- Plugin state may be unenumerable without source; may force an empirical dirty-page
  approach with perf cost at Cosmonarchy's uncapped unit counts.
- 1.16.1 + GPTP re-simulation correctness is the crux; a single missed range = desync.
- No relay today; host-authority has its own lag/prediction problem.
