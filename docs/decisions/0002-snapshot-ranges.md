# ADR-0002 — Snapshot ranges and the audit harness for 1.16.1 (via `bw_dat` / `aise` / `samase`)

- **Status:** Accepted (proposed)
- **Date:** 2026-10-10
- **Supersedes:** §3 and §4 of `0001-architecture.md` (authority/transport/hook-ownership decisions are unchanged; this revises the snapshot-enumeration and module-plan detail)
- **Grounding:** `docs/RECON.md` §6, `0001-architecture.md` §3/§4, `reference/ShieldBattery/game/src/rollback/ranges.rs`,
  `rollback_harness.rs`, `rollback_probe.rs`, `rollback_soak.rs`, and the `aise` v2.42.1 checkout at
  `~/.cargo/git/checkouts/aise-9e85dedfeaa0c87f/da4fed6/`.

---

## Context and the revised approach in one paragraph

ADR-0001 §3 assumed we would hand-transcribe a 1.16.1 struct/address table. Flash recon (`RECON.md:209-238`)
found that the local Cargo cache already contains **neivv's `aise` v2.42.1 with vendored `bw_dat`**:
`bw_dat/src/bw/structs.rs` is the full `#[repr(C)]` BW struct layer with `#[cfg]`-gated v116-vs-scr fields
(the exact 32-bit 1.16.1 layouts), and `bw_dat/tests/v116.rs` is the conformance gate. `aise/src/bw/mod.rs:267+`
shows `whack` hook/func/var tables keyed to the 1.16.1 module base `0x00400000`, and address resolution goes
through neivv's `samase` plugin API. This is the **same toolkit ShieldBattery uses** (its `bw_dat` dep, with the
`scr-only` feature); for Cosmonarchy we use `bw_dat` **without** the `scr-only` feature.

**What this actually buys us (honest accounting):**

| Layer | Source | Status for 1.16.1 |
|---|---|---|
| Struct definitions, `#[repr(C)]` layouts, sizes/offsets | `bw_dat/src/bw/structs.rs`, conformance-tested at `structs.rs:1200-1251` (`Game=0x17700`, `Unit=0x150`, `Bullet=0x70`, `Sprite=0x28`, `Image=0x40`, `Order=0x14`, `Pathing=0x97a20`, `Region=0x40`, `Player=0x24`, `PlayerAiData=0x4e8`, `AiRegion=0x34`, `AiTown=0x1cc`, `ResourceAreaArray=0x2ee8`, …) | **Reuse verbatim.** These are exactly ShieldBattery's `#[cfg(target_arch="x86")]` sizes (`ranges.rs:64-77`). |
| A *curated* set of globals (~15) | `samase` `VarId` set loaded by `aise` (`aise/src/samase.rs:70-89`): `Game`, `FirstActiveUnit`, `FirstHiddenUnit`, `Players`, `MapTileFlags`, `Pathing`, `AiRegions`, `PlayerAi`, `ActiveAiTowns`, `LocalUniquePlayerId`, `IsMultiplayer`, `FirstAiScript`, `FirstFreeAiScript`, `RngSeed`, `IsPaused` | **Partial.** A handful of the ~80 globals `analyze_ranges` needs. |
| A few hardcoded 1.16.1 addresses | `aise/src/bw/mod.rs:267-296` (`whack_hooks!`/`whack_funcs!`/`whack_vars!` at `0x00400000`, e.g. `client_selection @ 0x00597208`) | **Pattern + a few addresses.** Confirms fixed base `0x00400000` (no ASLR on this PE), which is the key simplification. |
| The `StepGameLoop` hook | `samase` `FuncId::StepGameLoop` (`aise/src/samase.rs:616-618`) plus `hook_step_objects`/`hook_step_order`/`hook_step_order_hidden` (`samase.rs:581-596`) | **Reuse** — this is exactly the game-logic-step seam rollback needs, and it is already the same one GPTP/aise hook. |
| ShieldBattery's exhaustive `analyze_ranges` accessors | `samase_scarf` (`~/.cargo/git/checkouts/samase_scarf-5de00b95646201b1`) | **Does NOT transfer.** It is SCR-only (see `src/x86_64_globals.rs`, `src/eud.rs`, `src/firegraft.rs`, `src/clientside.rs`). Retargeting it to 1.16.1 ≈ the analysis-layer rewrite ADR-0001 already flagged, and we now know it isn't a shortcut. |

**Net:** the struct layer (and therefore every `size_of`/`element_size`/`offset_of` in the range table, plus
typed access for the state hash) transfers from `bw_dat` for free and is conformance-tested. The **addresses**
— the ~80 scalar globals, the object-pool arrays + free-list heads, the tile arrays, the trigger-list heads,
the RNG block, and the AI ranges that ShieldBattery's `analyze_ranges` enumerates — are **not** in `bw_dat` or in
`aise`'s `samase` surface. That enumeration is the remaining work, and because 1.16.1 is a single pinned binary
at fixed base `0x00400000` (`RECON.md:16-18`), it is a **hardcoded address table**, verified by the audit — not a
runtime analysis.

One open question worth settling early: `samase_plugin` (github.com/neivv/samase_plugin, rev
`c01a88f0…`, `aise/Cargo.toml`) is **not** in the local cache, so we cannot yet enumerate its full
`VarId`/`FuncId` surface — `aise` loads only ~15 of whatever it defines. If the shipped `samase` host already
exposes more (it is used by GPTP, which hooks many more functions), the cheapest source for many addresses is
"ask `samase` for them" rather than transcribe from a map. Resolve this **before** transcribing the scalar-global
table (see §5, experiment 0).

---

## 1. Engine range table on 1.16.1

### 1.1 Resolution model

`crates/cosmo-rollback/src/bw161/` defines a `RangeSpec` list structurally identical to ShieldBattery's
(`ranges.rs:209-224`), keeping `RangeKind` verbatim (`ranges.rs:151-176`): `Storage`, `StorageBlock`, `Block`,
`IndirectBlock`, `MapTiles`, `PoolVector`, `AiRegions`, `TriggerLists`, plus `RangeList::make_disjoint`
(`ranges.rs:284-301`). Operands resolve from three sources:

- **`Const(addr)`** — an absolute 1.16.1 virtual address (base `0x00400000` + offset). Used for every scalar
  global and static-array base. No runtime analysis; the address is a `const` in a table, and the audit verifies
  it.
- **`Ptr(addr)`** — read the pointer *at* `addr` to get an array base (e.g. pool vectors if they turn out to be
  heap-allocated, trigger-list heads, per-player AI-region arrays). Mirrors `IndirectBlock`/`AiRegions`.
- **`bw_dat` typed field** — for the few we resolve through a struct we already have a pointer to (e.g. `Game`
  and `Players` come from `samase::game()` / `samase::players()` or a hardcoded `Game` global).

We keep ShieldBattery's `checked_size` pattern (`ranges.rs:805-813`) but point it at `bw_dat`'s own asserts
instead of a runtime analysis: because `bw_dat` is compiled with `target_pointer_width = "32"` and its `test_sizes`
are the conformance gate, a mismatch between our table's constant and `size_of::<bw_dat::Unit>()` is a compile-time
or test-time failure, not a runtime surprise.

### 1.2 Category-by-category mapping

Walking ShieldBattery's `analyze_ranges` (`ranges.rs:338-795`) and stating the 1.16.1 source for each category:

**A. Analysis words — heads/tails/counters (`ranges.rs:356-513`).** ~60 scalar globals: `first_active_unit`,
`last_active_unit`, `first_hidden_unit`, `first_dying_unit`, `first_revealer`, `first_invisible_unit`,
`first_pylon`, `first_free_unit`, `last_free_unit`, `unit_count`, `unit_position_search_entry_count`,
`pylon_refresh`, `pylon_auras_visible`, `order_timer_reset`, `secondary_order_timer_reset`, sprite/lone/fow/image/
bullet/order/path free-list heads and tails, `last_bullet_spawner`, `last_bullet_spin_direction`,
`active_bullet_count`, `allocated_order_count`, `pathing_ignored_unit`, `lurker_hits_frame/pos`,
`game_frame_count`, `trigger_result_check_timer`, `turn_timer_accumulator`, `vision_update_counter`,
`vision_updated`, `is_paused`, `trigger_current_player`, `trigger_execution_timer`, `trigger_elapsed_time_tick_timer`,
`leaderboard_refresh_timer`, `dcreep_next_update`, `dcreep_unit_next_update`, `first_ai_script`,
`ai_military_update`, the three `ai_target_ignore_reset*` counters, `step_ai_regions_player/region`,
`ai_expansion_player_cursor`, `draw_cursor_marker`, `sync_slot_index`, `sync_check_kind_index/count`,
`sync_map_row_index`, `captured_minimap_*`, `current_sync_state_byte`, `current_sync_check_hash`.
**1.16.1 source:** `samase` supplies only `first_active_unit`, `first_hidden_unit`, `is_paused`, `rng_seed`
(`samase.rs:70-89`). The rest are **hardcoded `Const(addr)`** transcribed from a 1.16.1 map (BWAPI/OpenBW/PyMS
symbol tables), each with a name for the audit log. `RangeKind::Storage`.

**B. Fixed-size blocks (`ranges.rs:526-658`).** `first_player_unit` (12 ptrs), `sprite_hlines{,_end}`
(0x100 ptrs each), `trigger_completed_units_cache` / `trigger_all_units_cache` (12×228×u32), `selections`
(8×0xc ptrs), `selection_hotkey_last_used_frames` (8×8×u16), `resource_areas` (`ResourceAreaArray`, 0x2ee8),
`path_array` (0x400×`Path`=0x80), `repulse_state` (0xab×0xab), `dcreep_lookup` (0x400 ptrs),
`dcreep_list_begin/size`, `player_ai` (8×`PlayerAiData`), `player_ai_towns`, `first_guard_ai`, `sync_data`
(0x10×0x10c), `sync_check_kinds` (0x20), `current_sync_vision_bytes` (0x100), `player_trigger_wait_*` /
`player_trigger_victory_states` / `player_trigger_active_flags`. **1.16.1 source:** element sizes and struct
definitions from `bw_dat` (`structs.rs`); the array *bases* are hardcoded `Const(addr)` — except `player_ai`
and `first_guard_ai`, which `samase` exposes (`VarId::PlayerAi`, `VarId::FirstGuardAi`). `RangeKind::Block`.

**C. Tile arrays (`ranges.rs:669-682`).** `map_tile_flags` (4B/tile), `tileset_indexed_map_tiles` (2B),
`vx4_map_tiles` (2B), `creep_original_tiles` (2B), `creep_tile_borders` (1B). **1.16.1 source:**
`map_tile_flags` from `samase` (`VarId::MapTileFlags`); the other four hardcoded. `RangeKind::MapTiles` (length
from `game.map_width_tiles * map_height_tiles`, read off `Game` at build time, `snapshot.rs:287`).

**D. RNG block (`ranges.rs:686-692`).** Six `u32` words: seed + draw-state advance + enable flag.
**1.16.1 source:** `samase` exposes only `RngSeed` (one word, `samase.rs:304-310`); the **full 6-word block**
must be hardcoded (seed word ± its neighbors). `RangeKind::StorageBlock { len: 6*4 }`.

**E. `lurker_hits` (`ranges.rs:696-703`), `ai_region_reachability` (`ranges.rs:712-721`),
`pathing_dynamic_state` (`ranges.rs:726-736`).** Hardcoded `Const`/`IndirectBlock` addresses; `pathing` itself
from `samase` (`VarId::Pathing`), with the dynamic-state pointer offset from a 1.16.1 map. `ai_regions`
(`ranges.rs:704`) from `samase` (`VarId::AiRegions`) → `RangeKind::AiRegions`.

**F. AI pools (`ranges.rs:744-786`).** worker/building/town/script/military/guard/dcreep pool storage +
free-list heads. `bw_dat` gives the struct sizes (`WorkerAi=0x18`, `BuildingAi=0x2c`, `AiTown=0x1cc`,
`AiScript=0x34`, `MilitaryAi=0x14`, `GuardAi=0x20`); the pool *bases* and free-list heads are hardcoded
(guard/town partially via `samase` `VarId::FirstGuardAi`/`ActiveAiTowns`). `RangeKind::Block` (storage) +
`RangeKind::Storage` (free list). In 1.16.1 these pools are fixed-size static arrays, so no `PoolVector` header
read is needed.

**G. Object pools — `pool_specs` (`ranges.rs:901-989`).** `images` (`Image`=0x40), `sprites` (`Sprite`=0x28),
`lone_sprites` (`LoneSprite`=0x10), `units` (`Unit`=0x150), `bullets` (`Bullet`=0x70), `orders` (`Order`=0x14),
`fow_sprites` (`FowSprite`=0x10). **1.16.1 difference:** these are **fixed-size static arrays** in the engine
(not the heap `BwVector`s SCR uses), so `RangeKind::PoolVector`'s vector-header read is replaced by
`RangeKind::Block` over the full fixed array + `RangeKind::Storage` for the free-list head. The array sizes are
1.16.1 constants (unit count 1700, etc.). **Important for Cosmonarchy:** "huge unit counts" (`RECON.md:1`) means
the mod fills the existing array closer to its fixed maximum; it does **not** grow the array, so snapshooting the
whole fixed array is correct and cheap relative to ShieldBattery. The unit pool's auxiliary arrays
(`UNIT_AUXILIARY_ARRAYS`: splash candidates, the two position-search arrays, query scratch — `ranges.rs:859-891`)
and the sprite draw heap/order (`SPRITE_AUXILIARY_ARRAYS`, snapshotted=false — `ranges.rs:841-855`) are the same
in 1.16.1 and keep their `EXCLUDED`/included decisions.

**H. Trigger lists (`ranges.rs:662-666`, `snapshot.rs:33-57`).** `player_trigger_lists` → `RangeKind::TriggerLists`
(copied node-by-node, not as a flat range). **1.16.1 source:** hardcoded head-array address; node size
`TRIGGER_SIZE = 0x960` (`snapshot.rs:24`) is a 1.16.1 constant.

**I. `Game` and `players` (`snapshot.rs:299-313`).** Copied around `VIEWER_LOCAL_GAME_FIELDS`
(`ranges.rs:48-59`: `screen_pos_tiles`, `saved_screen_positions` — both present in `bw_dat::structs::Game` at
the same offsets `structs.rs:37-38,57`). `Game` pointer from `samase::game()` (or the hardcoded global);
`players` from `samase::players()`.

### 1.3 Concretized starting `RangeSpec` list

The deliverable of milestone 3 is a `const` table of `(name, kind, source)` triples. The minimal set to start
coding the **engine-only** snapshot (1v1 melee, no AI, no plugins) is, in dependency order:

1. `game` — `Block` over `size_of::<Game>()` minus `VIEWER_LOCAL_GAME_FIELDS`; source `Const(game_addr)`.
2. `players` — `Block` `0xc × size_of::<Player>()`; source `samase::players()`.
3. `rng` — `StorageBlock { 24 }`; source `Const(rng_block_addr)`.
4. `units` pool + `first_free_unit`/`last_free_unit`/`unit_count`/`first_active_unit`/`last_active_unit`/
   `first_hidden_unit`/`first_dying_unit`/`first_revealer`/`first_invisible_unit`/`first_pylon`/
   `unit_position_search_entry_count` + the unit auxiliary arrays.
5. `bullets` pool + `first_active_bullet`/`last_active_bullet`/`first_free_bullet`/`last_free_bullet`/
   `active_bullet_count`/`last_bullet_spawner`/`last_bullet_spin_direction`.
6. `sprites`/`images`/`orders`/`lone_sprites`/`fow_sprites` pools + their free-list heads + `allocated_order_count`.
7. `map_tile_flags` + the four other tile arrays.
8. `resource_areas` + `path_array` + `repulse_state`.
9. `selections` + `selection_hotkey_last_used_frames` + `first_player_unit`.
10. `player_trigger_lists` (TriggerLists) + `player_trigger_*` flags/timers + `trigger_current_player` +
    `trigger_execution_timer` + `trigger_elapsed_time_tick_timer` + `trigger_result_check_timer`.
11. `sync_data` ring + `sync_*` cursors + `current_sync_state_byte`/`current_sync_check_hash` (needed even with
    native sync off, because the turn-timer accumulator and sync-slot cursors advance each step —
    `ranges.rs:427-435,492-513`).
12. `turn_timer_accumulator`, `game_frame_count`, `vision_update_counter`, `vision_updated`, `is_paused`,
    `draw_cursor_marker`, `lurker_hits*`, `dcreep_*`.
13. `EXCLUDED` entries, logged with their reasons (`ranges.rs:181-207`): foliage, sprite draw heap/order,
    pathing, pathing-dynamic-state edges.

AI ranges (items `player_ai`, `player_ai_towns`, `first_guard_ai`, AI pools, `ai_regions`,
`ai_region_reachability`, `step_ai_regions_*`, `ai_*` counters) are **deferred** to the post-1v1 milestone and
kept in a `#[cfg(feature = "ai")]` list so the table stays honest about what is (not) covered.

---

## 2. Plugin-state enumeration

### 2.1 What the plugins actually own

`aise` (shipped as `aise.qdp`) is the AI-scripting engine, and its `Globals` struct (`aise/src/globals.rs:678-710`)
is the concrete, enumerable shape of "plugin sim state": `attack_timeouts`, `idle_orders`, `kills_table`,
`base_layouts`, `lift_lands`, `queues`, `build_max`, `max_workers`, `town_ids`, `bunker_states`, `renamed_units`,
`guards`, `bank`, `unit_replace`, `reveal_states`, `under_attack_mode`, `ai_mode`, `global_ai_mode`,
`region_safety_pos`, `build_at` (a `HashMap`), `towns` (a `Vec`), `rng` (a plugin RNG), `ai_scripts`
(a `BlockAllocSet`), `region_search`. This is **pointer-heavy** — `Vec`s, `HashMap`s, `BlockAllocSet`, boxed
`Town`s — so it is **not a flat memcpy range**. ADR-0001's `register_range(name, addr, len)` contract is
insufficient for it.

The saving grace: `aise` **already implements full serialization for save games** via `bincode` — `save`/`load`
(`globals.rs:822-852`) registered through `samase`'s `extend_save` (`samase.rs:635-661`). The whole `Globals`
round-trips through `bincode::serialize`/`deserialize`. So the rollback snapshot can reuse it as a deep
copy/restore, exactly as ShieldBattery special-cases the trigger lists (heap nodes copied and relinked,
`snapshot.rs:33-57`) — the exception to flat memcpy.

`gptp` (Cosmonarchy-GPTP, the big gameplay/trigger patch) almost certainly owns the same kind of state; it is
open source at `gitlab.com/the-no-frauds-club/tools` and, because Cosmonarchy supports save games, it will have a
save/load hook of its own. This must be confirmed in the sources (see §5, experiment 0).

### 2.2 Revised plugin contract

Two registration forms, both `__stdcall` through `cosmo_rollback`'s C ABI (resolved at load, like the
CrownLink channel):

```c
// Flat state: for a plugin global/array that really is contiguous bytes.
void cosmo_rollback_register_range(const char* name, void* addr, u32 len);

// Deep state: for pointerful state (Vec/HashMap/BlockAllocSet). save()/load() are the
// plugin's own serialize/deserialize (aise reuses its bincode save/load).
void cosmo_rollback_register_state(
    const char* name,
    void(__stdcall* save)(void* out_ctx, void(__stdcall* emit)(const u8* data, u32 len, void* ctx)),
    u32(__stdcall* load)(const u8* data, u32 len));
```

The rollback engine holds, per snapshot slot, a `Vec<PluginBlob>` (name + bytes) alongside the flat ranges —
the generalization of `SnapshotExtras` (`snapshot.rs:199-217`). `take` calls each registered `save`, `restore`
calls each `load`. Blobs are restored **after** the flat ranges and before the trigger lists, and — critically —
plugin `load` must be idempotent and allocation-safe on restore, the same discipline `aise`'s `load` already
obeys for save games.

### 2.3 Ordered plan

1. **Inventory** (no code): from each plugin's sources, list its mutable sim state and whether it already has a
   save/load hook. Expected: `gptp` (big), `aise` (has bincode), `mtl` (rally-points / shared orders — likely
   small or derivable from `Game`), `vx4ex` (map-tile extension — likely writes the tile arrays already covered
   by §1.2-C), `fastmenus`/`isdbg`/`aize_debug` (UI/debug — likely **no** sim state, confirmed by inspection).
2. **Engine-only milestone first** (§4): run the audit with every state-owning plugin present but **not**
   registered. Expect the audit to report diverging words *inside plugin static allocations*. This is the
   measurement that both (a) proves the audit can see plugin state and (b) tells us which plugins matter.
3. **Register `aise`** using its existing bincode save/load; re-run the audit. If clean, `aise`'s state is fully
   covered with near-zero new code.
4. **Register `gptp`** (and `mtl`/`vx4ex` if step 1 flags them). Same procedure.
5. **`supported()` gate** (`rollback_live.rs:508-516` analogue): a rollback session is refused unless every
   state-owning plugin that is loaded has registered. A plugin that ships a future version without registration
   must fail closed, not desync.

### 2.4 Making the audit see plugin state

The audit in §3 diffs the exe `.data` section by default. Plugin state lives in **plugin static allocations**
(the plugin DLL's own `.data`/`.bss`, or heap it allocated via its allocator) — outside the exe `.data`. Two
extensions make the audit reach it:

- **Register plugin ranges/blobs for the audit too:** the `register_range`/`register_state` calls double as audit
  inputs; the audit diffs the *covered union* (exe `.data` + registered plugin ranges) against a saved copy, and
  a plugin blob whose `save()` output differs between two simulations is reported by name.
- **Delta-of-two-runs** (§3.3) catches *anything* the register calls miss, because it compares full state
  fingerprints frame-by-frame regardless of where the bytes live.

---

## 3. The audit harness port (the kill gate)

### 3.1 Minimal hook set to run the harness at all

To reproduce ShieldBattery's `rollback_harness.rs` audit on Cosmonarchy, the DLL needs only:

1. **`StepGameLoop` hook** — install the rollback trampoline *after* the hotloader, using `samase`'s
   `hook_func(FuncId::StepGameLoop, …)` seam (`samase.rs:616-618`) or `whack` at the 1.16.1 address. This is the
   single hook the whole re-simulation loop rides on (`RECON.md:145-147`, `tick.rs:140-186`).
2. **Snapshot build** — `Snapshots::build` against the §1.3 range list (`snapshot.rs:281-432`), so the harness
   has ranges to snapshot/restore and, in debug builds, to name in the audit log (`snapshot.rs:637-640`).
3. **The audit diff itself** — `audit_before_step` (`rollback_harness.rs:553-589`) and `audit_after_step`
   (`:607`) copied nearly verbatim: copy the exe `.data` section before the audited frame's first simulation,
   diff before every later simulation of that frame, report each word that changed and is **not covered** by a
   range (`rollback_harness.rs:569-580`).

No transport, no input table, no state hash, no once-per-frame ledgers are needed for the *engine-only* audit —
it runs a local replay and compares re-simulated frames against plain playback, not against a network peer.

### 3.2 Driving it from a Cosmonarchy replay under Wine/GE-Proton (no network)

The game runs locally via `umu-run` + GE-Proton (`RECON.md:234-236`). The harness is replay-driven
(`rollback.md:228-240`): load a recorded Cosmonarchy replay, force re-simulation every tick with
`SB_ROLLBACK_SHADOW=<depth>` (`rollback_live.rs:28-29`), and compare each re-simulated frame's fingerprint
against the confirmed one (`rollback_harness.rs:1056-1062`). Concretely:

- Reproduce ShieldBattery's replay-seek/harness entry point but against the 1.16.1 replay loader. We need only
  "load replay + step the game loop + snapshot + restore + re-step", all reachable through the `StepGameLoop`
  hook — the replay *player* path SB already uses for the harness (`rollback_harness.rs:914` `run_game_logic_step`).
- A first Cosmonarchy replay can be produced by playing (or scripting) a short 1v1 melee and saving the `.rep`.
  No live network opponent is required.

### 3.3 Env-variable analogues and the success metric

Keep ShieldBattery's knob names (rename the prefix so they don't collide with a real SB DLL in the same
process):

| SB knob (`rollback_live.rs`, `rollback_harness.rs`) | Cosmonarchy analogue | Purpose |
|---|---|---|
| `SB_ROLLBACK_AUDIT_FRAME` (`rollback_harness.rs:84`) | `CM_ROLLBACK_AUDIT_FRAME=<n>` | audit the frame-`n` re-simulation |
| `SB_ROLLBACK_DUMP_FRAME` (`:466`) | `CM_ROLLBACK_DUMP_FRAME=<n>` | dump full ranges + layout for bisection |
| `SB_ROLLBACK_EXTRA_RANGES` (`ranges.rs:24-25`) | `CM_ROLLBACK_EXTRA_RANGES=<exe_off>+<len>,…` | add candidate ranges without recompiling |
| `SB_ROLLBACK_SHADOW` (`rollback_live.rs:28-29`) | `CM_ROLLBACK_SHADOW=<depth>` | force re-simulation every tick |
| `SB_ROLLBACK_PREDICT` / `TARGET` (`:59-71`) | `CM_ROLLBACK_PREDICT` / `TARGET` | prediction limit / rollback target |

**Success metric (kill gate):** for the chosen audit frame(s), `SB_ROLLBACK_AUDIT_FRAME` reports **zero
diverging words outside the snapshot ranges** — i.e. the re-simulation reproduces the first simulation of every
covered byte, and every differing word the audit *does* find is either (a) inside a registered range, or (b)
named in `EXCLUDED` with its reason. Second metric: **delta-of-two-runs clean** — run the same replay twice (or
once with `CM_ROLLBACK_SHADOW`) and confirm the per-frame `probe_fingerprint` (`rollback_probe.rs:586-628`,
the `rng`/`minerals`/`gas`/`trigger_timer` fingerprint) is byte-identical run over run. Both must hold on the
real patched `Cosmonarchy BW.exe`, under Proton, with the state-owning plugins loaded.

### 3.4 Adding candidate ranges without recompiling

`CM_ROLLBACK_EXTRA_RANGES` is the `add_extra_ranges_from_env` port (`ranges.rs:307-331`): parse
`<hex exe offset>+<hex length>` (offsets relative to the exe `.data`/base), append them to the `RangeList`
before `make_disjoint`, and re-run the audit. A diverging word the audit reports gives its offset directly;
feed that offset (+ a guess at length) back in, repeat until the diff is clean. This is how the table is
*completed* empirically after the initial transcription.

---

## 4. First implementation milestone (concrete)

Goal: **audit #1 green on the real engine — engine-only, no plugin registrations, no transport.**

Ordered code to write in `crates/cosmo-rollback`:

1. **Dependency wiring.** Add `bw_dat` (path/vendored, default features — **not** `scr-only`; set
   `target_pointer_width = "32"` via the existing i686 target), `whack`, and `samase_plugin`/`samase_shim`
   (from neivv's repo, the same rev `aise` pins). Add `fxhash` for the state hash (later) and `bincode` for
   plugin blobs (later).
2. **`src/bw161/structs.rs`** — `pub use bw_dat::structs::*;` plus our own `FowSprite`/`ReplayData` if
   `bw_dat` lacks them. Add the `size_of` asserts as a `#[test]` (mirror `structs.rs:1200-1251`) so a layout
   mismatch fails the build test, not the game.
3. **`src/bw161/ranges.rs`** — the §1.3 `RangeSpec` table with `RangeKind`, `RangeList`, `make_disjoint`,
   `checked_size`, `EXCLUDED`, `VIEWER_LOCAL_GAME_FIELDS`, and `add_extra_ranges_from_env` (`CM_*`). This is the
   single file that encodes the transcribed 1.16.1 addresses.
4. **`src/rollback/{copier,tick,snapshot}.rs`** — fork verbatim from SB, adapting `snapshot::build` to resolve
   `Const`/`Ptr`/`bw_dat` operands instead of `resolve_operand` (`snapshot.rs:315-383`).
5. **`src/hooks.rs`** — install the `StepGameLoop` trampoline via `samase` `FuncId::StepGameLoop`
   (`samase.rs:616-618`) **after** the hotloader has loaded the plugins (injection order: `cosmo_rollback.dll`
   injected last, so its `OnInject` runs last). Install the `CM_ROLLBACK_*` env parsing at `OnInject`.
6. **`src/audit.rs`** — port `audit_before_step`/`audit_after_step`/`dump_if_due` (`rollback_harness.rs:504-668`)
   against the §3.3 knobs.
7. **`src/lib.rs`** — `OnInject` + `cosmo_rollback_abi()` (bump to 2), a replay-driven main loop that steps the
   game, snapshots, and re-simulates under `CM_ROLLBACK_SHADOW`, and logs the audit result.

**"Engine-only, no plugins owning sim state" means, concretely:** run the audit with the plugin set reduced to
the *read-only/UI* plugins (`fastmenus`, `isdbg`), **not** `gptp`/`aise`/`mtl`/`vx4ex`, so no plugin mutates
sim state the snapshot doesn't cover. (Alternatively run with them present but confirm via the audit that they
are outside the ranges — but the clean first run is without them.) The audit proves the *engine* range table is
complete; §2 then layers plugin state back in one plugin at a time.

**Acceptance:** `CM_ROLLBACK_AUDIT_FRAME=<n>` (a few frames across a short melee replay, including a combat
frame and a trigger-fire frame) reports zero uncovered diverging words, and the delta-of-two-runs fingerprint is
clean.

---

## 5. Revised risks / kill criteria

ADR-0001's risk list stands except where `bw_dat` reuse changes it:

- **R1 (missed range) — unchanged, now cheaper to close:** `bw_dat` removes the struct-layout error class
  entirely (wrong offset = test failure), leaving *wrong/missing address* as the only remaining error, which the
  audit catches directly. Kill gate unchanged (audit #1 green).
- **R1a (new) — the 1.16.1 addresses don't match the patched exe.** The mod's runtime patches or a hotloader
  rebase could move the engine's globals off stock addresses. **Mitigation/kill:** audit #1 on the real
  `Cosmonarchy BW.exe` (not `StarCraft.exe`) is the arbiter; if stock addresses are wrong, the audit diverges
  everywhere and we fall back to per-address discovery (or `samase` if its full `VarId` surface covers them).
- **R1b (new) — `samase_plugin`'s full `VarId`/`FuncId` surface is unknown** (not in local cache). If it's richer
  than the 15 `aise` loads, many addresses come from it for free; if not, we transcribe. **Resolve first**
  (experiment 0 below).
- **R2 (plugin determinism) — sharpened:** `aise` uses its own RNG (`rand_xoshiro`, `globals.rs:704`) and
  `Instant`-based elapsed-time tracking (`globals.rs:30-34,855-882`). The `Instant` state is **not** sim state
  (it feeds autosave only) but proves the class of risk is real; the plugin RNG *is* sim state and must be in the
  blob. The delta-of-two-runs experiment is unchanged and now specifically targets `aise`'s `rng` and `gptp`'s
  equivalents.
- **R4 (snapshot cost at uncapped counts) — slightly reduced:** pools are fixed arrays, so cost is a constant
  full-array copy (no vector-header walking, no `MAX_POOL_CAPACITY` guard). Still bench at Cosmonarchy's max
  unit count before trusting it.

**Experiment 0 (new, first thing to do before transcribing addresses):** inspect the `samase_plugin` crate
(github.com/neivv/samase_plugin) for the full `VarId`/`FuncId` list and the GPTP sources at
`gitlab.com/the-no-frauds-club/tools` for (a) its sim state + save/load hook and (b) confirmation that the
shipped `samase` host resolves engine globals at the addresses we need. This single investigation determines
how much of §1.2 is "ask `samase`" vs "transcribe from a map".

---

## Open questions (carried forward + new)

1. Full `samase_plugin` `VarId`/`FuncId` surface, and whether the shipped `samase` host can hand us the ~80
   globals directly.
2. Does `gptp`/`mtl`/`vx4ex` ship a save/load hook we can reuse (like `aise`'s `extend_save` bincode), or must
   we add serialization to them?
3. Whether the patched `Cosmonarchy BW.exe` keeps stock 1.16.1 `.data` addresses (R1a), settled by audit #1.
