# ADR-0004 — Plugin re-simulation safety (making re-sim correct with the shipped `.qdp` plugins)

- **Status:** Accepted (proposed)
- **Date:** 2026-10-11
- **Decision owner:** architecture lead
- **Supersedes:** ADR-0002 §2 (plugin-state enumeration) — refines the *how* of plugin-state capture given what the sources actually show; the engine-range decisions are unchanged.
- **Grounding:** `docs/RECON.md` §6, `docs/decisions/0001..0003`, `crates/cosmo-rollback/src/{rollback,ranges,snapshot,frame_hook}.rs`, and the plugin sources at
  `~/.cargo/git/checkouts/aise-9e85dedfeaa0c87f/da4fed6/` (aise v2.42.1, vendored `bw_dat`) and `/tmp/opencode/samase_plugin/` (`samase_plugin` + `samase_shim`).

---

## Context

### The blocker, restated in one paragraph

The shadow gate (`crates/cosmo-rollback/src/rollback/mod.rs:80-106`) restores an older snapshot and
re-runs the true per-frame game function at **0x004D9670** (`frame_hook.rs:18-20`) forward to the
present, then compares the state hash. Every engine field reproduces **except** `rng_seed`
(0x0051CA14), `Game+0x0` = `minerals[0]` (0x0057F0F0; `bw_dat/src/bw/structs.rs:30` makes `minerals`
the first `Game` field), `Game+0xe608`, and occasionally `first_active_bullet` (0x0064DEC4). All four
of these *are* in the snapshot range table (`ranges.rs:64,56,83`); they diverge because the re-sim
**produces** different values, not because they are missing. The cause is that re-running 0x004D9670
re-runs the shipped plugins' engine hooks from **stale plugin state**: the plugins' internal state is
not in any snapshot range, so the plugins make different decisions the second time, and those
decisions write back into engine state (spend/refund minerals, draw engine RNG, spawn a bullet with a
different spin). The plugins also trip an assertion panic early in a game (`src\lib.rs:8908`,
`assert(left == right) left:0 right:1`, in `gptp.qdp`), which is why the gate currently starts shadow
at frame 90 (`rollback/mod.rs:53-56`).

### A structural fact that changes the calculus (new in this ADR)

Each `.qdp` plugin is a **self-contained `cdylib`** that **statically links** `samase_shim`, which
statically links `samase_plugin` (`aise/Cargo.toml`: `crate-type = ["cdylib"]`;
`samase_shim/Cargo.toml`: `samase_plugin` with `features = ["implementer_helpers"]`; `samase_plugin`
has no `crate-type`, so it is an rlib). Consequently:

- Every plugin owns its **own** `whack::Patcher` and its own hook chain (`samase_shim/src/lib.rs:28`).
- Every plugin owns its **own** save-hook registry: `samase_plugin::save::add_hook`
  (`samase_plugin/src/save.rs:49-56`) pushes into a `static SAVE_HOOKS` (`save.rs:17`) that lives in
  **that plugin's** private module image. `samase_shim::extend_save` → `save::add_hook`
  (`samase_shim/src/lib.rs:2584-2594`). There is **no cross-plugin registry** our DLL can enumerate.
- Each plugin self-initializes: `aise::Initialize` → `samase_shim::init_1161()` →
  `samase::samase_plugin_init(ctx.api())` (`aise/src/lib.rs:189-226`), where `ctx.api()` is the
  plugin's *own* `PLUGIN_API` (`samase_shim/src/lib.rs:1030-1171`).

**Consequence:** we cannot reach `aise`/`gptp`'s save/load from our DLL by linking `samase_plugin`
(we'd get our own empty `SAVE_HOOKS`), and neither plugin exports its `save`/`load` (aise exports
only `GetMPQDraftPlugin`, `Initialize`, `samase_plugin_init` — RECON §6.1 / `aise/src/lib.rs:189`).
This invalidates ADR-0002 §2.2's assumption that `register_state(save,load)` is just a matter of
"call the plugin's existing save hook" — there is nothing to call unless a plugin is modified or a
shared seam is added.

### What `aise`'s state actually is (grounds every mechanism below)

`aise::Globals` (`aise/src/globals.rs:677-710`) is the AI-script engine's sim state. It is
pointer-heavy: `idle_orders`, `kills_table` (`KillsTable.kills: Vec`), `base_layouts`, `lift_lands`,
`queues`, `build_max`, `max_workers: Vec`, `town_ids: Vec`, `bunker_states`, `renamed_units`,
`guards`, `bank`, `unit_replace`, `reveal_states: Vec`, `build_at: HashMap`, `towns: Vec<Town>`,
`ai_scripts: BlockAllocSet<Script>` (`block_alloc.rs:14-17` — a `Vec<Block>` of `alloc_bytes` heap
chunks), and `region_search` (`#[serde(skip)]`, `globals.rs:708`). The inline (non-heap) fields are
`attack_timeouts: [..; 8]`, `under_attack_mode: [Option<bool>; 8]`, `ai_mode: [AiMode; 8]`,
`global_ai_mode`, `region_safety_pos`, and — critically — `rng: Rng` (`globals.rs:704`), where
`Rng(Option<Xoshiro128PlusPlus>)` is **inline** (`rng.rs:13`).

`aise` already round-trips the whole `Globals` through `bincode` for save-games
(`save`/`load` at `globals.rs:822-852`), registered via `extend_save("aise", …)`
(`aise/src/samase.rs:635-640`). Its RNG is seeded **once, lazily, from the engine `rng_seed`**
(`rng.rs:20-33`, `bw/mod.rs:48-53`) and thereafter advances its own xoshiro, never the engine RNG.

`gptp.qdp` (Cosmonarchy-GPTP, the trigger/gameplay patch) is not in the local cache; its source is at
`gitlab.com/the-no-frauds-club/tools` behind auth we do not have. Its state shape is unknown but is
almost certainly heap-backed (a trigger VM with per-trigger data, timers, and lists), and it is the
most likely consumer of the **engine** RNG (BW trigger `rand()`), which is the best explanation for
the per-frame `rng_seed` divergence.

---

## Decision

**We restore plugin state; we never suppress it.** Re-simulation must re-run the plugins (their
hooks mutate engine sim state: building placement, orders, resources, trigger effects — see
`aise/src/lib.rs:236-319`, `aiscript.rs:1981-2030`), so a correct re-sim requires the plugins to see
exactly the engine+plugin state they saw the first time. The engine half is already restored by the
snapshot; the plugin half is not, and that is the whole bug.

The mechanism is **two layers, deployed in order, each shipping a correctness gain without waiting
for the next**:

1. **Layer A — module-writable-section snapshot (candidate 1), now, no rebuild.** Enumerate loaded
   modules and snapshot each `.qdp` plugin's `.data`/`.bss` (writable, non-heap) as flat ranges. This
   captures every **flat** plugin static — including aise's inline `Globals` fields (`rng`,
   `ai_mode`, `under_attack_mode`, `region_safety_pos`, the `recurse_checked_mutex::Mutex` header,
   `recurse_checked_mutex.rs:9-14`) and the header/pointer words of its heap containers. It does
   **not** capture heap buffers (the `Vec`/`HashMap`/`BlockAllocSet` contents).
2. **Layer B — plugin save/load blobs (candidate 2), for `aise` first, via a minimal rebuild of the
   plugin whose source we already own locally.** `aise`'s `save`/`load` already serialize the whole
   `Globals`; we expose them as two `__stdcall` exports (or call our own `register_state`) so the
   snapshot ring holds a `Vec<PluginBlob>` restored after the flat ranges. This closes the heap gap
   for `aise` with ~zero new serialization code.
3. **Layer C — `gptp` (candidate 4 fallback).** If `gptp`'s residual divergence survives A+B, its
   source is required: either reuse its save section (if it has an `extend_save` like `aise`) with
   the same blob treatment, or bake in ADR-0001's `register_range`+`register_state` contract.
4. **Gate (always):** a rollback session refuses to start unless every loaded state-owning plugin is
   registered (`rollback_live::supported()` analogue, ADR-0002 §2.3 item 5). Fail closed, never
   desync.

The **recommended mechanism for the current blocker is Layer A**, because it is the only one that
ships a correctness gain today with zero plugin cooperation and zero rebuild, and because it is
decisive: it both (a) may clear the residual divergence entirely (if the sim-relevant plugin state is
flat — possible for `gptp` if its trigger state lives largely in the engine's already-snapshotted
trigger lists, ADR-0002 §1.2-H), and (b) if it does not, it gives us the precise residual signature
needed to scope Layer B/C. Layer B for `aise` follows immediately because its source is local.

### Why we do **not** gate `rng_enabled` around re-sim (direct answer to the RNG question)

The `rng_seed` divergence is **not** fixable by gating `rng_enabled` (0x006D11C8,
`samase_shim/src/bw.rs:212`), and it is not caused by a missing RNG range. Three facts:

1. The engine's `random()` is a deterministic LCG over `rng_seed`; it advances **only when
   `rng_enabled` is set**. Setting `rng_enabled = 0` for the whole re-sim would make the engine's own
   legitimate, deterministic `random()` calls (damage/miss rolls, spawn positions, bullet spin)
   return non-advancing constants — the re-sim would then **fail** to reproduce the engine's own RNG
   use. That is the opposite of the goal. The engine's RNG *consumption itself* is deterministic and
   must keep running.
2. The divergence is a **downstream symptom** of stale plugin state: the plugins call the engine
   `random()` (or read `rng_seed`) a *different number of times* on the re-sim because their own
   state was not restored. The proof is that `rng_seed` diverges *together with* `minerals[0]` and
   `Game+0xe608` — fields the plugins write directly — and it is `gptp`'s trigger `rand()` use that
   fits "every frame."
3. `aise`'s own RNG is **inline** and already inside `Globals` (`rng.rs:13`), so it is captured by
   Layer A (as a flat static) and by Layer B (via bincode). Snapshotting it is *necessary* but not
   *sufficient*: it is only one of ~20 state fields, and the real defect is that none of the others
   are restored either.

Note the one secondary hypothesis we must still rule out (cheap, in the first experiment): that our
table snapshots only 2 of the ~6 RNG-adjacent words (`rng_seed` + `rng_enabled`,
`ranges.rs:64-65`) and some engine RNG state lives in a neighbor word (ADR-0002 §1.2-D). If Layer A
clears `rng_seed` but a neighbor word still differs, widen the RNG block before touching `gptp`.

### Ordered implementation plan

1. **Layer A (no rebuild).** In `cosmo_rollback`, add module enumeration (Toolhelp
   `CreateToolhelp32Snapshot`/`MODULEENTRY32`, or `EnumProcessModules` via
   `windows-sys` `Win32_System_Diagnostics_ToolHelp`) and a PE header walk (DOS→`NT_HEADERS`→
   section table). For each module whose name matches the loaded `.qdp`s (`gptp`, `aise`, `mtl`,
   `vx4ex`, …), add every section with `IMAGE_SCN_MEM_WRITE` and without `IMAGE_SCN_MEM_SHARED` as a
   `RangeSpec { name: "<mod>.<section>", addr: base+rva, len: raw/virtual size }`. Add these to the
   `RangeList` in `ranges.rs` *after* the engine ranges. Re-run the shadow gate (see first
   experiment). Measure the residual divergence signature.
2. **RNG-block completeness check.** If `rng_seed` (or any word ±24 bytes of 0x0051CA14) still
   diverges, widen `rng` to the full 6-word block and re-measure, before assuming `gptp`.
3. **Layer B (`aise`).** Rebuild `aise` (source in the Cargo cache) with a two-line shim exporting
   its `globals::save`/`globals::load` (or adding a `cosmo_rollback_register_state("aise",
   save, load)` call). In `cosmo_rollback`, extend `Snapshot` to carry `Vec<PluginBlob>`, call
   `save` at capture and `load` at restore (after flat ranges, before trigger lists — ADR-0002 §2.2).
   Note `load` is allocation-safe and idempotent (`globals.rs:850`); `region_search` is `#[serde(skip)]`
   and is a recomputable search cache, so its omission is acceptable for determinism (verify in the
   gate).
4. **Layer C (`gptp`).** Re-measure. If residual remains, this is the hard dependency: obtain
   `gptp` source (ask the Fraud Launcher/community; it may have a mirror), add its save/load (or
   `register_range`+`register_state`) as in Layer B, and re-run the gate.
5. **`supported()` gate.** Refuse a rollback session unless every state-owning plugin loaded is
   registered. A future plugin version that does not register fails closed.

---

## First experiment (runnable headless today)

**Goal:** validate Layer A on the real binary and, in the same run, settle the "flat vs heap" and
"RNG-neighbor" questions that decide whether Layers B/C are needed at all.

**Steps:**

1. Implement module-enumeration + PE-section walk (Layer A) in `cosmo_rollback`; log each added
   `<module>.<section>` range from `audit::on_start` (`audit.rs:5-20`).
2. Build with `tools/build.sh` (32-bit `cargo-xwin`), install via `tools/install-plugin.sh`, and run
   the existing headless replay harness (`tools/headless_probe.sh` / `cosmo-audit.sh`) with
   `CM_ROLLBACK_SHADOW=3` **and `CM_ROLLBACK_SHADOW_START` unset or 0** (so re-sim begins at the
   earliest snapshot, where the plugin assertion panic would otherwise fire).
3. Read `cosmo_rollback.log` for `DIVERGENCE` lines (`rollback/mod.rs:92-97`).

**Interpretation matrix:**

| Outcome | Conclusion | Next action |
|---|---|---|
| Zero `DIVERGENCE` (no `rng_seed`, no `minerals`, no `e608`, no `first_active_bullet`) | The plugins' sim-relevant state is flat/static; the engine table is complete. **Rollback is correct on the shipped, unmodified plugins.** | Ship Layer A; keep Layers B/C as insurance for future plugin versions. |
| `rng_seed` (or a ±24-byte neighbor) alone still diverges | Engine RNG block is wider than 2 words (ADR-0002 §1.2-D); **not** a plugin-state issue. | Widen the `rng` range to the full 6-word block; re-run. |
| `minerals`/`e608`/`first_active_bullet` still diverge | `aise`'s heap `Globals` (Vec/HashMap/`BlockAllocSet`) drives the residual; flat statics were not enough. | Proceed to Layer B (`aise` blob); expect these to clear. |
| Only `rng_seed` + a downstream field remain after A **and** B | `gptp` consumes the engine RNG from stale heap state. | Layer C — obtain `gptp` source (candidate 4). |

**Expected result (our hypothesis):** Layer A clears `aise`'s inline contribution but **not** the
`rng_seed` divergence (that is `gptp` trigger `rand()`, whose state is heap-backed), and it may or
may not clear `minerals[0]` (aise's build spending lives in heap `Vec`s). Either way the experiment
turns the current "rng + minerals + e608 + bullet" blob into a per-plugin, per-field map, which is
exactly what Layers B/C need.

A follow-up experiment (after Layer A confirms the harness sees plugin bytes) is the **aise-blob
run**: rebuild `aise` with an exported `save`/`load`, register it, and confirm the `minerals[0]`/
`Game+0xe608` divergence clears while only `rng_seed` (→ `gptp`) remains.

---

## Consequences

### Positive

- **Layer A is a correctness gain with zero plugin cooperation and zero rebuild**, and it is
  strictly additive to the existing engine ranges (more coverage, never less).
- It converts a binary "desync or not" into a **per-field divergence signature**, which is the
  precise input the remaining layers and the audit need.
- Reuses the plugins' already-shipped, save-tested serialization (`aise`'s bincode) rather than
  inventing one, so Layer B is low-risk and small.
- The `supported()` gate guarantees we fail closed rather than ship a desyncing game.

### Negative / risks

- **Layer A does not capture heap**, so it cannot by itself make `aise`/`gptp` deterministic if
  their sim-relevant state is pointer-backed (it almost certainly is, for `aise`'s `Globals`).
- **TLS is not captured by a module-section walk.** `samase_shim` and `samase_plugin` use
  `thread_local`/`ThreadLocal` (`samase_shim/src/lib.rs:33-36`, `samase_plugin/src/save.rs:18`) for
  init/save temporaries (`CONTEXT`, `CURRENT_HOOK`, `LAST_FILE_POINTER`). These are not persistent
  sim state, but the audit must confirm none leaks into determinism.
- **Snapshotting a `Mutex<T>` static is only safe at a quiescent point.** We snapshot at the frame
  boundary (0x004D9670, before `StepObjects`), where `aise`'s `Globals` mutex is unlocked
  (`locking_thread == 0`, `recurse_checked_mutex.rs:42-50`), so the copied lock word is "unlocked."
  A future plugin that holds a lock across the frame boundary would break this; the audit is the
  guard.
- **Layer B requires rebuilding `aise`** (soft: source is local) and **Layer C requires `gptp`
  source** (hard: currently unavailable). Until C, `gptp`'s state is a documented correctness gap.
- Rebuilding a shipped `.qdp` means shipping a forked artifact; the fork must be tracked and kept in
  sync with upstream (RECON §6).

---

## Alternatives considered (candidates 1–5, weighed)

### Candidate 1 — snapshot plugin DLL writable sections (adopted as Layer A)

Enumeration + PE-section walk, as in the plan above.

- **Pros:** no rebuild, no source, no plugin cooperation; captures all flat plugin statics
  (including aise's inline `rng`, `ai_mode`, `region_safety_pos`, and the container headers);
  implementable and testable today; additive.
- **Cons:** misses heap (`Vec`/`HashMap`/`BlockAllocSet` contents, boxed `Town`s — the bulk of
  `aise`'s `Globals`); misses TLS; the mutex-quiescence caveat above. **Insufficient alone for
  `aise`/`gptp`.**
- **Verdict:** necessary first step and decisive diagnostic; not the end state for pointer-heavy
  plugins.

### Candidate 2 — snapshot plugin heap via their own save/load (adopted as Layer B, for `aise`)

Invoke each plugin's `save`/`load` (which already serialize `Globals`) to snapshot/restore per
rollback.

- **Pros:** the only mechanism that guarantees completeness for pointer-heavy state; zero new
  serialization logic (reuses bincode); `load` is idempotent and allocation-safe
  (`globals.rs:837-852`).
- **Cons / obstacles:**
  - **No shared registry** (see Context): each plugin's `SAVE_HOOKS` is process-private
    (`samase_plugin/src/save.rs:17,49`), and neither plugin exports its `save`/`load`. To reach them
    we must either (a) rebuild the plugin to export them (what we propose for `aise`), or (b) add a
    shared seam in the samase host — which is itself a shipped binary we can't rebuild.
  - **File/section ordering:** `call_save_hooks` (`save.rs:92-146`) targets the save-*file* format
    (deflate + chunk headers + a "samase chunk offset" trailer for 1.16.1, `save.rs:165-167`), not a
    raw ring buffer. We bypass it and call the plugin's `save`/`load` directly, which are pure
    in-memory.
  - **`before_save` side effects:** `aise`'s save path runs `claim_bw_allocated_scripts` +
    `set_first_ai_script(null)` with a `defer` restore (`globals.rs:778-804`), a temporary mutation
    of the engine AI-script list designed for save points. We snapshot at the frame boundary (the
    same quiescence), but the blob path must not trigger that dance for a *snapshot* (we snapshot the
    `Globals` directly via `save`/`load`, not via the full save command).
  - **Skipped field:** `region_search` is `#[serde(skip)]` (`globals.rs:708`); acceptable only if it
    is a recomputable cache — the gate must prove it.
- **Verdict:** the correct end-state for `aise`; blocked for `gptp` by source access, not by design.

### Candidate 3 — suppress plugin hooks during re-simulation (rejected as a mechanism; kept as a tool)

Temporarily restore original bytes at the plugins' hook targets, re-run, re-patch.

- **Pros:** avoids the `gptp` assertion panic; isolates engine-only determinism.
- **Cons:**
  - **Removes a sim contribution, so it is wrong for correctness.** `aise`/`gptp` mutate engine sim
    state from inside their hooks (`aise/src/lib.rs:236-319`); suppressing them means the re-sim
    cannot reproduce the original frame. The shadow gate would diverge *more*, not less.
  - **Enumeration is unbounded and fragile.** The plugins hook through both the samase host
    (`StepObjects` 0x004D94B0, `StepOrder` 0x004EC4D0, `StepOrder_Hidden` 0x004EBED0,
    `StepSecondaryOrder` 0x004EC170, `ProcessCommands` 0x004865D0, and ~60 `FuncId`s,
    `samase_shim/src/bw.rs:115-138`) **and** each plugin's own `whack` table (aise's seven hooks,
    `aise/src/bw/mod.rs:267-275`), forming a chain we'd have to walk and re-patch safely.
- **Verdict:** useful only as a diagnostic to attribute divergence; never as the shipping mechanism.

### Candidate 4 — rebuild plugins with a rollback contract (Layer C fallback)

`register_range(addr,len)` + `register_state(save,load)` + `once_per_frame(key,fn)` (ADR-0001 §2.2,
ADR-0002 §2.2).

- **Pros:** the clean, maintainable end-state; gives the once-per-frame side-effect ledgers too.
- **Cons:** requires `gptp` source, which is the exact thing we do not have (gitlab auth). Cannot be
  scheduled; it is a dependency to obtain.
- **Verdict:** the end-state for `gptp`; pending source access.

### Candidate 5 — a better mechanism derived from the sources

The single most promising *derivation* is not a new capture technique but a **reframing**: the
plugins' sim state is only a problem insofar as it is **not** already reachable through engine
state. Because `gptp` is a trigger system and the engine already snapshots its trigger lists
(ADR-0002 §1.2-H), a portion of `gptp`'s state may live in engine ranges we already cover. The
first experiment is designed to reveal exactly this: **the residual divergence signature is the
inventory of plugin state that is *not* derivable from engine state**, and it tells us, per field,
whether Layer A (flat), Layer B (serializable, `aise`), or Layer C (unreachable, `gptp`) applies.

We explicitly **reject** two other derivations:

- **Whole-process-heap snapshot/checkpoint:** rolling back the entire process heap would also
  rewind our own allocator, ring buffers, and log state; it is unsafe and impractical without
  allocator-level control.
- **Engine save/load replay (in-memory save at snapshot, load at restore):** it would indeed pull in
  every plugin's save hook automatically, but it is heavyweight (full engine+plugin serialization
  every `N` frames), it has the `claim_bw_allocated_scripts`/`loaded_save` side effects, and a
  mid-game "load" is not a clean restore. Rejected as too heavy and too stateful for a per-frame
  ring.

---

## Risks

| # | Risk | Mitigation / arbiter |
|---|---|---|
| R1 | Layer A doesn't capture the heap that drives divergence → residual `minerals`/`e608`/`rng_seed`. | First experiment; Layer B (`aise`), then Layer C (`gptp`). |
| R2 | `gptp` source cannot be obtained → Layer C blocked → rollback stays incorrect on `gptp`. | `supported()` gate fails closed; escalate source acquisition (mirror, community, or binary RE of `gptp`'s save section). |
| R3 | `rng_seed` divergence is an engine RNG-block completeness issue, not a plugin issue → we chase plugins needlessly. | Widen `rng` to the 6-word block in the first experiment and re-measure before Layers B/C. |
| R4 | TLS or a held mutex breaks the flat-static snapshot's determinism. | Audit at the frame boundary; confirm `locking_thread == 0` and no TLS sim state. |
| R5 | `region_search` (`#[serde(skip)]`) is not a pure cache → aise blob restore leaves stale search state. | Prove recomputability in the Layer B gate; if not, extend aise's save to include it (source is local). |

---

## Open questions

1. Is the `rng_seed` divergence per-frame `gptp` trigger `rand()`, or an engine RNG-block neighbor
   word we don't snapshot? (First experiment, step 2.)
2. Does `gptp.qdp` register a save section (an `extend_save` equivalent) we could reuse once its
   source is available, mirroring `aise`'s `globals::save`/`load`?
3. Are there plugins beyond `gptp`/`aise` (`mtl`, `vx4ex`) that own sim state, and is it flat
   (Layer A covers it) or heap-backed (Layer B needed)? `vx4ex` likely writes the already-covered
   tile arrays (ADR-0002 §2.3 item 1); confirm.
4. Can the shipped `samase` host be induced to expose a shared save-hook registry (a no-rebuild
   path to Layer B for `gptp`), or is `gptp` self-contained like `aise`?

---

## Result (2026-10-11) — Layer A is unsafe; Layers B/C required

Implemented Layer A (`crates/cosmo-rollback/src/modules.rs`: Toolhelp module walk + PE section
walk, writable non-shared `.qdp` sections added to `ranges.rs`). Ran the headless harness with
`CM_ROLLBACK_SHADOW=3 CM_ROLLBACK_SHADOW_START=0`:

- Enumeration worked: it found `aise.qdp`, `mtl.qdp`, `vx4ex.qdp`, `fastmenus.qdp`, `isdbg.qdp`,
  `aize_debug.qdp`, and `gptp.qdp` sections. `gptp.qdp..data` alone is **~18.9 MB**.
- **With `gptp`'s huge section included** (20 MB snapshot): blank screen.
- **Capping sections at 1 MB** (excludes `gptp`) and excluding our own module: still a **hang**
  (no crash message; `ntsync: up and running` then silence).

**Conclusion:** blindly snapshotting/restoring a plugin's flat `.data` is unsafe — it contains
heap pointers (`aise`'s `Globals` Vec/HashMap) and synchronization primitives (`parking_lot`
mutexes); rewinding them to an older state corrupts the plugin (deadlock/crash). Flat statics are
**not** sufficient; the plugin's own `save`/`load` (Layer B) is required, and `gptp` (Layer C)
must be handled via its own save section or a rebuild. This confirms the ADR's hypothesis.

**Path forward:** Layer B — rebuild `aise` (source is in the Cargo cache) to expose its
`globals::save`/`load` as a rollback blob and register it; obtain/rebuild `gptp` for Layer C.
`gptp`'s source is not available to us (GitLab needs auth), so Layer C is the open dependency.

---

## Update (2026-10-11) — `gptp` source obtained and characterized

The `Cosmonarchy-GPTP` source **is public**: `https://gitlab.com/the-no-frauds-club/tools/cosmonarchy-gptp.git`
(the earlier failure was a wrong URL — `the-no-frauds-club/tools` is a *group*, not the repo). It is
now a submodule at `reference/cosmonarchy-gptp` (HEAD `b4c07c784`, 25 MB, C++).

Findings that decide Layer C:

- **State container:** the plugin's mutable state is a single heap object, **`*g_globals`**
  (`std::unique_ptr<Globals>`, re-created per game at `hooks/game_hooks.cpp:4315`; declared
  `inline auto g_globals = std::make_unique<Globals>();` at `systems/triggers/trigger_system.h:263`).
  `IquareGlobals` (`globals.h:40`) holds heap containers: `std::vector<EffectField>`,
  `std::vector<LineAttackHit>`, `std::vector<MultihitBullet>`,
  `std::unordered_map<Unit*, std::vector<Unit*>>`, plus scalars.
- **Serialization already exists as a dependency:** gptp vendors RFL/reflectcpp (`utils/rfl.h`,
  `rfl::yaml` used in `CampaignManager.cpp`). So `*g_globals` can be serialized with RFL.
- **But gptp's `aise_save`/`aise_load` are stubs** (`globals.cpp:12-19`: *"Saving doesn't work
  properly in Cosmonarchy. Any plugin specific data will be lost."*). There is no existing
  plugin-state serialization to reuse — it must be written.
- **Build:** MSVC solution (`GPTP.sln`/`GPTP.vcxproj`) + vcpkg (`lua`, `sol2`, `reflectcpp`,
  `nlohmann-json`, `magic-enum`, `miniaudio`, …). Rebuilding on Linux is a substantial port;
  the project is Windows/MSVC-oriented.

**Layer C plan:** add an RFL `save`/`load` (or `snapshot`/`restore`) for `*g_globals`, expose it
via a C export the rollback DLL can call (like the CrownLink `cl_rollback_*` channel), and build
`gptp`. The state is well-defined (one heap object + RFL); the **build environment is the main
obstacle**, not the serialization design.
