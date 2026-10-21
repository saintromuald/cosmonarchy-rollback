# Range additions for the Cosmonarchy 1.16.1 snapshot

Grounds the **additional** `(address,length)` ranges the snapshot must cover beyond what
`crates/cosmo-rollback/src/ranges.rs` already lists, so the shadow re-simulation stops diverging.

Authoritative sources, cited as `file:line`:
- `ref` = `reference/cosmonarchy-gptp/BW/bwdata.h`
- `List.h` = `reference/cosmonarchy-gptp/BW/structures/List.h`
- `structs.h` = `reference/cosmonarchy-gptp/BW/structures.h`
- `ccmu.h` / `ccmu_inject.cpp` = `reference/cosmonarchy-gptp/hooks/limits/`
- `samase` = `~/.cargo/git/checkouts/samase_plugin-…/samase_shim/src/bw.rs`
- `aise` = `~/.cargo/git/checkouts/aise-9e85dedfeaa0c87f/da4fed6/src/`
- `bw_dat` = `aise/…/bw_dat/src/bw/structs.rs`
- disasm = the shipped stock `StarCraft.exe` (base `0x00400000`, non-ASLR), decoded with `objdump -d`.

## Why the current divergence is pool free-list state

The divergence frequency is `units 21, images 21, sprites 19, game.after_saved_screen 12,
orders 7, game 6, first_active_bullet 2, bullets 2`. The first differing offsets are all *high*
offsets inside a pool's storage: `units +0x14275c` (unit ≈ 4045 of 4095), `sprites +0x4c8ec`,
`orders +0x13dec`, `images +0xf5f4c`. These are the **free / unused tail** of each pool. The
snapshot already copies the whole pool *storage* (via `pools.rs`), so a byte in the free tail
differs only because the re-simulation **threads the free list differently** — i.e. the free-list
head/tail/count globals beside each pool are not being restored. Restoring the storage without the
heads is exactly the failure mode `pools.rs` documents (`pools.rs:24-27`) and ShieldBattery calls
out (`reference/ShieldBattery/game/src/rollback/ranges.rs:738-742`).

The `game.after_saved_screen +0xe47c` report resolves to absolute `0x0058D6F8` =
`Game.elapsed_seconds` (`bw_dat` `structs.rs:89`, base `0x0057F0F0` → offset `0xe608`; same field
ADR-0004/0005 tracked as "Game+0xe608"). It diverges because the trigger elapsed-time tick timer
`0x006509C0` (§4) is not restored, so the game clock advances differently. The `game` (6) report is
`minerals[]` (base `0x0057F0F0`), already understood as plugin-driven (ADR-0005 §6).

---

## 1. Pool free-list heads, tails and counts

All are 4-byte pointer/count globals (`kind = flat`). Every address below is grounded in `ref` or
disasm; the three that are *not* in `bwdata.h` (unit free-list, order free-list) are grounded in
disassembly.

| name | addr | len | kind | evidence (file:line or disasm) | why sim-relevant | priority |
|---|---|---|---|---|---|---|
| `unit_free_head` (first_free_unit) | `0x00628438` | 4 | flat | disasm `0x49de00` unlinks from `0x628438`; `0x4a06f5` `mov 0x628438,%ecx` in `unit_constructor` | re-sim threads the free list differently → free-tail bytes differ | **P0** |
| `unit_free_tail` (last_free_unit) | `0x0062843C` | 4 | flat | disasm `0x49de11`/`0x49de1b` (`0x62843c`) | same | **P0** |
| `unit_count` (g_unit_count) | `0x006283F0` | 4 | flat | `ref:68`; disasm `0x4a06c3` `cmpl $0x6a4,0x6283f0` | allocation is refused at limit; count must rewind with the list | **P0** |
| `sprite_free_head` (g_free_sprites.head) | `0x0063FE30` | 4 | flat | `ref:1003` `g_free_sprites` = `List<Sprite>*`; `List.h:16-18` head@0,tail@4 | sprite free list | **P0** |
| `sprite_free_tail` (g_free_sprites.tail) | `0x0063FE34` | 4 | flat | `List.h:16-18` (head+4) | sprite free list | **P0** |
| `sprite_count` (g_sprite_count) | `0x006C4A2C` | 4 | flat | `ref:72` | sprite alloc limit | **P0** |
| `image_free_head` (unusedImages.head) | `0x0057EB68` | 4 | flat | `ref:1002` `ListExtern<Image,&Image::link>(0x0057EB68, 0x0057EB70)` | image free list | **P0** |
| `image_free_tail` (unusedImages.tail) | `0x0057EB70` | 4 | flat | `ref:1002` | image free list | **P0** |
| `order_free_head` (first_free_order) | `0x0064B2E0` | 4 | flat | disasm `0x48c5a2`/`0x48c5e8` (`0x64b2e0`) in `InitializeOrderArray` | order free list | **P0** |
| `order_free_tail` (last_free_order) | `0x0064B2E4` | 4 | flat | disasm `0x48c5a8`/`0x48c5c4` (`0x64b2e4`) | order free list | **P0** |
| `order_count` (g_order_count) | `0x00641698` | 4 | flat | `ref:384`; disasm `0x48c70e` `movl $0,0x641698` | allocated-order count (`allocated_order_count` in SB) | **P0** |
| `bullet_free_head` (first_free_bullet) | `0x0064EED8` | 4 | flat | `ref:48`; disasm `0x48a5b0`/`0x48a6ac` | bullet free list | **P0** |
| `bullet_free_tail` (last_free_bullet) | `0x0064EEDC` | 4 | flat | `ref:49`; disasm `0x48a5ba`/`0x48a6a6` | bullet free list | **P0** |
| `bullet_count` | `0x0064DEBC` | 4 | flat | `ref:50`; disasm `0x48a5db` `decl 0x64debc` | bullet count | **P0** |
| `thingy_free_head` (g_first_free_thingy) | `0x00654878` | 4 | flat | `ref:1283` | lone-sprite (thingy) free list | P1 |
| `thingy_free_tail` (g_last_free_thingy) | `0x0065487C` | 4 | flat | `ref:1284` | lone-sprite free list | P1 |
| `fow_free_head` (g_first_free_fow_sprite) | `0x006509D0` | 4 | flat | `ref:1167` | fow-sprite free list | P1 |
| `fow_free_tail` (g_last_free_fow_sprite) | `0x00654870` | 4 | flat | `ref:1168` | fow-sprite free list | P1 |

Bullet-adjacent scalars that change bullet spin/creation without the RNG (SB
`last_bullet_spawner`/`last_bullet_spin_direction`/`active_bullet_count`, `ranges.rs:399-409`):

| name | addr | len | kind | evidence | why | priority |
|---|---|---|---|---|---|---|
| `last_bullet_creator` | `0x0064DEB0` | 4 | flat | `ref:516`; disasm `0x48a670` `movl $0,0x64deb0` | next spinning bullet from the same unit uses the opposite side, no RNG draw | P1 |
| `launch_spin_random` | `0x0064DEC0` | 4 | flat | `ref:517` | spin direction state | P1 |
| `last_bullet_index` | `0x0064DEA8` | 4 | flat | `ref:500`; disasm `0x48a63d` | cyclic missile index cursor | P1 |
| `last_bullet` (active tail) | `0x0064DEAC` | 4 | flat | `ref:47`; disasm `0x48a57c`/`0x48a6b2` | active bullet list tail | P1 |

---

## 2. Other linked-list heads/tails beside the pools

`sim` = snapshot (simulation state); `viewer` = render/UI only (exclude). Evidence is `ref`
line numbers from the §"Linked list nodes" block (`ref:33-83`) plus the scattered `Unit**` list
heads (`ref:482-517, 1225, 1287`).

| name | addr | sim? | evidence | note |
|---|---|---|---|---|
| `first_active_unit` (g_first_visible_unit) | `0x00628430` | sim | `ref:33` | already in `ranges.rs` |
| `last_active_unit` (g_last_visible_unit) | `0x0059CC9C` | sim | `ref:34` | **missing** — add |
| `first_hidden_unit` | `0x006283EC` | sim | `ref:35` | already in `ranges.rs` |
| `last_hidden_unit` | `0x00628428` | sim | `ref:36` | **missing** — add |
| `first_dying_unit` | `0x0062842C` | sim | `ref:497`; disasm `0x49ddb0` | **missing** — add |
| `last_dying_unit` | `0x0059CC98` | sim | `ref:1287`; disasm `0x49ddc1` | **missing** — add |
| `first_scanner_sweep` | `0x006283F4` | sim | `ref:38`; disasm `0x49dd60` | scanner-sweep list head |
| `last_scanner_sweep` | `0x00628434` | sim | `ref:39`; disasm `0x49dd71` | scanner-sweep list tail |
| `first_psi_field_provider` (first_pylon) | `0x0063FF54` | sim | `ref:40`; disasm `0x49312f` | pylon/psi provider list head |
| `first_invisible_unit` | `0x0063FF5C` | sim | `ref:482`; disasm `0x491450`/`0x49158a` | invisible-unit list head (singly linked) |
| `first_active_bullet` (g_first_bullet) | `0x0064DEC4` | sim | `ref:46` | already in `ranges.rs` |
| `first_player_unit` | `0x006283F8` | sim | `ref:55`; 12 × `Unit*` | per-player first-unit heads (`first_player_unit` block) |
| `first_lone_sprite` (g_first_thingy) | `0x00654874` | sim | `ref:82` | already in `ranges.rs` |
| `last_lone_sprite` (g_last_thingy) | `0x0065291C` | sim | `ref:83` | **missing** — add |
| `first_fow_sprite` | `0x00654868` | sim | `ref:1165` | already in `ranges.rs` |
| `last_fow_sprite` | `0x0065486C` | sim | `ref:1166` | **missing** — add |
| `first_ai_script` | `0x0068C100` | sim | `ref:1038`; `samase:227` | already in `ranges.rs` |
| `first_free_ai_script` | `0x0068C0F8` | sim | `ref` + `samase:228` | already in `ranges.rs` |
| `psi_field_grp_width`/`height` | `0x0063FF4C`/`0x0063FF58` | sim | `ref:41-42` | psi-field rectangle dims |
| `g_psi_aura_is_drawn` | `0x0063FF48` | viewer | `ref:44` | drawn-only flag |
| `g_can_update_powered_status` | `0x0063FF44` | sim | `ref:1054`; disasm `0x4932c0` | pylon power refresh gate |
| `g_units_recalled` | `0x0063FF50` | sim | `ref:1409` | recall state flag |

---

## 3. AI state ShieldBattery snapshots (1v1 melee: deferred, but listed for completeness)

ADR-0001 §4 defers AI ranges for v1; none of these are needed to clear the *current* divergence
(the headless replay has no AI players). Included so the table is honest about what is *not* yet
covered. Only entries with a grounded 1.16.1 address are listed; the rest are in §"unverified".

| name | addr | len | kind | evidence | note |
|---|---|---|---|---|---|
| `guard_ai_pool` (g_guard_array) | `0x0067D400` | `1000×0x20 + 4` | pool-array | `ref:1094`; `GuardAi` `C_ASSERT 0x20` `structs.h:747`; `first_free_guard` at `+0xC800` | guard AI pool + free head |
| `player_guard_list` | `0x00685108` | `8×8` | pool-array | `ref:1095`; `GuardAiList{array,first}` `structs.h:830-833` | already partially in `ranges.rs` as `guard_ais 8*2*4` |
| `ai_town_pool` (g_ai_town_array) | `0x006AA090` | `100×0x1cc + 4` | pool-array | `ref:453`; `AiTown` `C_ASSERT 0x1cc` `structs.h:948`; `first_free` at `+0xB1F0` | AI town pool + free head |
| `ai_town_list` (g_ai_town_list) | `0x006AA050` | `8×8` | pool-array | `ref:454`; `AiTownList` `C_ASSERT 0x8` `structs.h:959` | already in `ranges.rs` as `active_ai_towns 8*8` |
| `ai_script_pool` (g_aiscript_array) | `0x0068C0FC` | `100×0x34` | pool-array | `ref:1039`; `AiScript` `C_ASSERT 0x34` `structs.h:1039` | AI script storage (free head is the separate `first_free_ai_script` global) |
| `player_ai` (g_aiscript_controller) | `0x0068FEE8` | `8×0x4e8` | pool-array | `ref:1035`; `samase:216`; `AI_Main` `structs.h:973-1024` | already in `ranges.rs` as `player_ai` |
| `ai_regions` (g_ai_region_captains) | `0x0069A604` | `8×4` | pointer-global | `ref:1036`; `samase:215` | already in `ranges.rs` as `ai_regions` (deref per player) |
| `resource_areas` (g_base_array) | `0x00692688` | `0x2ee8` | pool-array | `ref:1041`/`ref:573`; `ResourceAreaArray` (`bw_dat` size `0x2ee8`) | resource area array + count + timer |
| `g_region_ids` | `0x0069262C` | — | pool-array | `ref:577` | region id table (len from `g_region_ids_count`) |
| `g_region_ids_count` | `0x0069265C` | 4 | flat | `ref:576` | count for the above |
| `g_ai_supply_reserved` | `0x006CA4BC` | 4 | flat | `ref:1037` | AI supply reservation |
| `g_ai_update_cooldown` | `0x0059CC94` | 4 | flat | `ref:1097` | AI per-frame update cooldown |
| `g_ai_strength_air` | `0x006BB210` | 4 | flat | `ref:1221` | AI air strength base |
| `g_ai_strength_ground` | `0x006BB5A0` | 4 | flat | `ref:1222` | AI ground strength base |

The worker/building/military AI pools and their free-list heads, plus
`ai_spending_player_index`, `ai_military_update`, the three `ai_target_ignore_reset*` counters,
`step_ai_regions_player/_region`, `ai_expansion_player_cursor`, the AI region reachability memo, and
the dcreep lookup/lists **have no address in `bwdata.h`** — they must be located by disassembling
the AI pool allocators (`0x00433DD0` `AddTownUnitAi`, `0x00435770` `AddBuildingAi`,
`0x0043DA20` `AddMilitaryAi`, `0x004A2830` `AddAiToTrainedUnit`) and the AI step/region functions.
Do not guess; see §"unverified".

---

## 4. AI timers/cursors, splash, lurker, repulse, sync checksum, trigger state

Grounded scalars/blocks (kind `flat` unless noted):

| name | addr | len | kind | evidence | why sim-relevant |
|---|---|---|---|---|---|
| `vision_update_counter` (g_vision_update_count) | `0x0051CE98` | 4 | flat | `ref:472` | already in `ranges.rs` as `frame_cooldown` — **same address**, keep |
| `vision_updated` | `0x0051CE9C` | 4 | flat | `ref:473` | vision update flag |
| `order_timer_reset` (g_order_timer_reassign_count) | `0x0059CCA4` | 4 | flat | `ref:492` | order timer reassignment counter |
| `secondary_order_timer_reset` (g_secondary_order_timer_reassign_count) | `0x006283E8` | 4 | flat | `ref:493` | secondary order timer reassignment |
| `draw_cursor_marker` (g_can_draw_thingy) | `0x00652920` | 1 | flat | `ref:1226`; `samase:271` | order-confirmation marker draw flag |
| `is_targeting` (g_is_targeting) | `0x00641694` | 1 | flat | `ref:491`; `samase:274` | targeting state |
| `interceptor_focused_this_frame` | `0x006957D0` | 4 | flat | `ref:490` | interceptor focus cursor |
| `active_iscript_entity/bullet/unit` | `0x006D11F4` | 12 | flat | `ref:494-496` | already in `ranges.rs` as `active_iscript` |
| `larva_counter` (g_larva_counter) | `0x0067D3F8` | 2 | flat | `ref:507` | larva spawn counter |
| `building_placement_state` | `0x006408F8` | 1 | flat | `ref:510` | placement state machine |
| `selection_circle_color` | `0x00581D6A` | 1 | flat | `ref:513` | viewer-ish (selection circle); borderline |
| `disabled_unit_array` (g_disabled_unit_array) | `0x00692660` | 4 | flat | `ref:533` | AI disabled-unit pointer |
| `current_visibility_hash` | `0x0063FD30` | 4 | flat | `ref:1237` | visibility checksum accumulator |
| `want_visibility_hash_update` | `0x00629D90` | 4 | flat | `ref:1238` | visibility checksum dirty flag |
| `region_vision_hash` | `0x0063FE38` | 4 | flat | `ref:1239` | vision hash |
| `current_hash_offset` | `0x0063FD28` | 4 | flat | `ref:1240` | hash offset cursor |
| `lakiz_splash_list` | `0x0064DEC8` | `400` | pool-array | `ref:498` ("sizeof 400") | splash candidate list |
| `lakiz_splash_list_index` | `0x0064EEC8` | 4 | flat | `ref:499` | splash list cursor |
| `first_free_path` (g_first_free_path) | `0x006BEE84` | 4 | flat | `ref:1333` | path pool free head |
| `path_array_start` (g_path_array_start) | `0x006BEE8C` | 4 | flat | `ref:1334` | path pool base (0x400 × `Path`=0x80) |
| `unit_ordering_count` (g_unit_ordering_count) | `0x0066FF74` | 4 | flat | `ref:919` | `unit_position_search_entry_count` (SB) |
| `sprite_hlines` (g_horizontal_sprite_lines_begin) | `0x00629688` | `0x100×4` | pool-array | `ref:1206`; `samase:261` | per-row sprite list heads |
| `sprite_hlines_end` (g_horizontal_sprite_lines_end / g_sprites_on_tile_row) | `0x00629288` | `0x100×4` | pool-array | `ref:1207`/`ref:1009`; `samase:260` | per-row sprite list tails |

Trigger runtime state (per-player flags/timers; the trigger *lists* are already handled as
`TriggerLists`). All groundable in one disassembly block, the trigger step at `0x00489CC0`–`0x00489D98`
(writes `elapsed_seconds` at `0x489d58`; decrements/resets the two countdowns at `0x489d10`/`0x489d34`).

| name | addr | len | evidence |
|---|---|---|---|
| `trigger_execution_timer` | `0x0065097C` | 2 | disasm `0x489d19` `dec %ax` on `0x65097c`, reset to `0xf` at `0x489d26`, fires `call 0x4538b0` |
| `trigger_elapsed_time_tick_timer` | `0x006509C0` | 2 | disasm `0x489d42` `mov %ax,0x6509c0`, `inc elapsed_seconds` at `0x489d55`, reset to `0xf` at `0x489d7e` — **this is the global whose absence drives `Game.elapsed_seconds` (0x0058D6F8) divergence** |
| `player_trigger_wait_timers` (g_trigger_wait) | `0x00650980` | `8×4` | `ref:1176`; disasm `0x489cd3` `0x650980(,%eax,4)` with `eax` 0..7 |
| `player_trigger_wait_active_flags` (g_is_executing_wait) | `0x006509B8` | 8 | `ref:1174`; disasm `0x489cc5` `cmp %bl,0x6509b8(%eax)` |
| `current_trigger_player` (g_current_trigger_player_id) | `0x006509B0` | 4 | `ref:570` |
| `trigger_current_node` (g_current_trigger_node) | `0x006509AC` | 4 | `ref:1175` (`TriggerLink*`) |
| `trigger_ran_flag` | `0x006509A0` | 2 | disasm `0x489c1f`/`0x489d70` `movw $1,0x6509a0` |
| `trig_remove_unit_active` / `trig_killing_additional_unit` / `trig_kill_unit_count` | `0x005971DC` / `0x00596CD4` / `0x005971E0` | 4 ea | `ref:1119-1121` |

Groundable economy/score globals (inside `Game` or beside it; already covered by the `game` range
except where noted):

- `g_elapsed_time_frames` = `Game.frame_count` `0x0057F23C` (`ref:218`, `bw_dat` `structs.rs:51`) — inside the `game` range.
- `g_elapsed_time_seconds` = `Game.elapsed_seconds` `0x0058D6F8` (`ref:220`, `structs.rs:89`) — inside `game.after_saved_screen`; the *current* divergence field.
- `g_countdown_timer` = `Game.countdown_timer` `0x0058D6F4` (`ref:219`, `structs.rs:88`).
- `countdown_timer_interval` `0x0066FBFC`, `countdown_timer_tick_count` `0x0059CC7C`,
  `countdown_timer_tick_count_2` `0x0059CC88`, `countdown_time_remaining` `0x0059CC78`
  (`ref:440-444`).

---

## 5. State owned by plugins (aise / gptp) — what the shadow divergence implicates

### aise (Layer B, still not implemented)

`aise::Globals` (`aise/globals.rs:677-710`) is **pointer-heavy** (`Vec`, `HashMap`, `Box`,
`BlockAllocSet<Script>`); it is **not** a flat memcpy range. It already round-trips through
`bincode` for save games (`globals.rs:822-852`, registered via `extend_save` `aise/samase.rs:635-640`).
The correct capture is the plugin's own `save`/`load` (ADR-0004 Layer B), **not** a range. Its
inline `rng: Rng` (`rng.rs:13`, `Xoshiro128PlusPlus`) is seeded once from the engine `rng_seed`
and thereafter advances its own state — a necessary-but-not-sufficient field; the heap containers
are the bulk. `region_search` is `#[serde(skip)]` (`globals.rs:708`) — a recomputable search cache.

The `game` (minerals) divergence is consistent with aise spending/refund logic writing engine
resources from unrestored heap state (ADR-0004 §Context, §First experiment). **aise has no address
to snapshot** — it needs the Layer B blob.

### gptp (Layer C)

ADR-0005 §6.1 names the scope gap the shadow divergence implicates: `g_ext` (`data_ext.cpp:29`,
`UnitExt`/`BulletExt` sidecar), `g_trigger_vars` (`trigger_vars.h:101`), `g_ai_layouts`
(`ai_layouts.h:184`), `g_active_game_mode`. ADR-0005's Phase 2 serialized `g_ext` and the residual
was **unchanged**, so per that ADR the remaining suspect is **aise (Layer B)** and/or a frame-boundary
issue, not gptp. gptp state is two heap objects (`*g_iquare_globals` `globals.h:42`, `*g_globals`
`trigger_system.h:263`) serialized by `cosmo_rollback_state.cpp` exports (ADR-0005 §4.1) — no flat
ranges. None of `g_ext`/`g_trigger_vars`/`g_ai_layouts`/`g_active_game_mode` can be addressed as a
static range; they are heap, reached only through the serializer.

**Conclusion for §5:** neither plugin contributes a flat range. The `game`/`minerals` divergence is
plugin-heap-driven and is closed by Layer B (aise) / the ADR-0005 blob, not by this table.

---

## must resolve dynamically

Anything CCMU or aise relocates out of the exe `.data` must be read off its **patched operand**
(exactly the `pools.rs` technique), never a hardcoded stock address. `ccmu.h:4-37` is the layout;
`ccmu_inject.cpp` gives the patch sites whose operand holds the live base. `ccmu_extender` itself
lives in `gptp.qdp`, so a snapshot range for one of these arrays is the **dereferenced operand**.

| array | operand site (disasm/source) | stock base | note |
|---|---|---|---|
| `unit_table` | `0x00401EF0` (`ccmu_inject.cpp:140`) | `0x0059CCA8` | already resolved by `pools.rs` |
| `sprite_table` | `0x004873EC` (`ccmu_inject.cpp:1158`) | `0x00629D98` | resolved by `pools.rs` |
| `order_table` | `0x0048C59A` (`ccmu_inject.cpp:1278`) | `0x006416A0` | resolved by `pools.rs` |
| `image_table` | `0x00497388` (`ccmu_inject.cpp:1363`) | `0x0052F568` | resolved by `pools.rs` |
| `thingy_table` | `0x00487336` (`ccmu_inject.cpp:1441`) | `0x006509D8` | resolved by `pools.rs` |
| `fow_sprite_table` | `0x00487316` (`ccmu_inject.cpp:1435`) | `0x00652928` | resolved by `pools.rs` |
| `bullet_table` | `0x0048A691` (`pools.rs:96`) | `0x0064B2E8` | `new_bullet_table` (`bullet_extender.h:4`) |
| `air_splash_list` | `0x0048B1B1` (`ccmu_inject.cpp:691`) | — | `ccmu.h:6` `air_splash_list[UnitLimit]`; count is the **unrelocated** scalar `g_air_splash_list_count` `0x0064DEB4` (`ref:448`) |
| `unit_finder_x` (unit_position_search_x) | `0x004222B9` (`ccmu_inject.cpp:243`) | — | `ccmu.h:7`; SB `unit_position_search_x`; count = `unit_ordering_count` `0x0066FF74` |
| `unit_finder_y` (unit_position_search_y) | `0x00422195` (`ccmu_inject.cpp:235`) | — | `ccmu.h:8`; count = `unit_ordering_count` |
| `get_all_units_in_bounds_list` (query scratch) | `0x0042FEAC` (`ccmu_inject.cpp:296`) | — | `ccmu.h:10-11`; SB `unit_query_scratch_marks` |
| `get_all_units_in_bounds_index` (query results) | `0x0042FEEB` (`ccmu_inject.cpp:300`) | — | `ccmu.h:12-13`; SB `unit_query_results` |
| `sprite_list_pointers` | `0x0042D343` (`ccmu_inject.cpp:1136`) | — | `ccmu.h:16` |
| `image_list_pointers` | `0x0042D377` (`ccmu_inject.cpp:1325`) | — | `ccmu.h:21-22` |
| selection arrays (`new_client_selection_group`, `new_local_selection_group`, `new_active_selection`, `new_players_selections`, `new_saved_selections`, `new_selGroupHp`, `new_selGroupType`) | `ccmu.h:28-36`; sites in `ccmu_inject.cpp` | — | viewer-local selection is excluded (§"excluded"); `players_selections` is sim (selection hotkeys) |

For the unit/order/bullet **free-list heads and counts** (§1) the operand technique is **not**
needed: those are plain 4-byte globals that stay in the exe `.data` and hold pointers *into* the
relocated arrays. Only the arrays themselves move.

---

## 6. Probe-derived unnamed ranges (identification, 2026-10-18)

`ranges.rs` carries six empirically-justified ranges the reproducibility probe
(`CM_ROLLBACK_DET`, `rollback/mod.rs`) found but that had no name. Identified here by
their **use site** in the shipped `StarCraft.exe` (full `.text` disassembled with
`ndisasm -b 32 -o 0x401000`), not by a canonical symbol — **none of the six has a name
in gptp's `BW/bwdata.h`**, so these are role descriptions, not symbol matches. Addresses
and call sites are ground truth; the *names* are inferred.

| probe range | grounded role | evidence |
|---|---|---|
| `0x0057EEBC` | a **turn/sequence counter**, incremented by 1 on every network step (`step_network` `0x00487070`: `mov ecx,[0x57EEBC]; inc ecx; mov [0x57EEBC],ecx`), reset to 0 by every network re-init. Not a pointer (the `lea [ecx+edx*4]` at `0x0044F860` packs it with `g_provider_turns_in_transit` `0x0057F090` into a turn-id field, which is why it reads like an address). | `0x004870D5`–`0x004870E3` (`inc`); resets at `0x00485871`, `0x004D32B1`, `0x004D9A40`; pack at `0x0044F86D` |
| `0x0059CC8C` | **countdown timer** (initialised `0xF`, reload `0x12C`=300) for the per-frame *clear unit flag `0x80000000`* step. Same function also drives `0x0059CC90` (init `6`) and `g_ai_update_cooldown` `0x0059CC94` (init `0x3C`=60) and `g_ignore_unit_flag80_clear_subcount` `0x006D5BD0`. | init at `0x004A1ECF` (`=0xF`); countdown at `0x004A2A40`–`0x004A2A5A` (dec / reload `0x12C`); the loop at `0x004A2AA0` clears bit `0x80000000` on `[unit+0xDC]` over `first_active_unit` `0x00628430` |
| `0x0068FEE4` | **round-robin AI player cursor 0..7**, used to index `g_aiscript_controller` (`AI_Main[8]` at `0x0068FEE8`, stride `0x4E8`). Init to 0 in the AI-controller init; incremented per AI step. | init `0x00447690`; `inc eax; cmp eax,8; …` at `0x00448920`; indexed read `imul esi,esi,0x4E8; add esi,0x68FEE8` at `0x004488A0`, `0x00448948` |
| `0x0069556C` | **countdown (reload `0x1C2`=450) gating the resource-area / AI-expansion update** at `0x00445610`, itself gated by the enable word `0x00695568`; the step walks `g_resource_areas` (`0x00692688`, cursor at `0x006926BC`). | `0x00445626` read/dec; `0x00445646` `mov dword [0x69556c],0x1C2`; `0x00445654` `mov eax,0x6926BC` |
| `0x006AA04C` | **round-robin cursor 0..7 over the eight `g_ai_town_list` entries** (`AiTownList[8]` at `0x006AA050`, stride 8). Init to 0 by the AI-town init. | init `0x0043268E`; `inc eax; cmp eax,8; …` and `mov esi,[eax*8+0x6AA054]` at `0x00435BD0`–`0x00435BEE` |
| `0x0065FAE4` | **partially resolved.** It has **no direct reference anywhere in the exe's `.text`** (searched the raw image for the little-endian address and the full disassembly), so it is written through a **computed address**: it is at **+0xFD4 inside the 0x1000-byte block based at `0x0065EB10`** (the mask-hash block, whose first fields are `mask_hash_current_x/y/next` `0x0065EB10/28/2C`) — i.e. element 1013 of a 1024 × 4 table. The block is used as a structure (`mov edi,0x65EB30` at `0x0047D09D`; fields at `+0x4/+0x8/+0x14`). Note it is **above** the index→pointer fixup routine at `0x0047DA1D` (which rewrites slots over `0x659B10..0x65EB10`), so it is not one of those pointer slots. Whether its bytes are values or addresses is still unresolved. | no `0x65fae4` / `E4 FA 65 00` in the image; block base used at `0x0047D09D`, `0x0047DA1D`, `0x0047DB3E` |

**Conclusion:** the open item "identify the probe ranges in `bwdata.h`" resolves to
*"gptp's `bwdata.h` does not name them"* — five are grounded by role above, and
`0x0065FAE4` needs a focused pass on the `0x65EB10` block (its base/stride) before it can
be named. A canonical name would need BWAPI/IDA 1.16.1 symbol data, which the reference
checkouts do not contain.

**Cross-check for the desync hash.** These six also appear in `state_hash.rs`'s
`POINTER_FREE` list, which is compared **between processes**; a pointer in that list would
report a desync every frame. Checking each against the disassembly: `0x0057EEBC`
(turn counter), `0x0059CC8C` and `0x0069556C` (countdowns), `0x0068FEE4` and `0x006AA04C`
(round-robin cursors 0..7) are all **values — safe**. `0x0065FAE4` is **unresolved** and
therefore the one entry whose safety is unproven: if it turns out to be an address (like
the excluded buffer pointers `0x006554B4`/`g_turn_buffer`), it must come out of
`POINTER_FREE`. It is not known to be an address, only not known to be a value.

---

## excluded, with reason

| name | reason |
|---|---|
| `sprite_draw_heap`, `sprite_draw_order` | vision step queues sprites for the next draw; only rendering reads them (SB `ranges.rs:187-191`). The per-row `sprite_hlines` are snapshotted (§4) but the draw heap/order are not. |
| `foliage_state` | sim marks footprints, renderer reads (SB `ranges.rs:182-185`). |
| `pathing` (region graph) | built at map init, read-only during play; searches restore their marks (SB `ranges.rs:192-198`). |
| `pathing_dynamic_state_edges` | collision edge arrays built at map init (SB `ranges.rs:199-206`). |
| `client_selection` / `local_selection` / `current_unit_selection` / `active_player_selection` | what the *person* has selected; sim never reads it (SB `ranges.rs:557-560`). `players_selections` (selection *hotkey groups*) is sim and is CCMU-relocated. |
| `g_screen_x/y`, `g_mouse*`, `g_cursor*`, `g_move_to_*`, `g_minimap_*`, `g_ping_*`, font/dialog/palette/theme globals | all input/render/UI (`ref:136-207`, `ref:316-654`). |
| `g_music_*`, `g_sfx_volume`, sound channels, `g_sound_*` | audio (`ref:97-100`, `ref:607-627`). |
| `g_psi_aura_is_drawn` (`0x0063FF48`) | draw-only flag. |
| `g_selection_sound_counter`, `g_last_random_sfx_id`, `g_last_response_unit` | audio/response (`ref:328-329`, `ref:1202`). |
| `g_screen_layers`, `g_graphics_*`, `g_storm_*`, `g_dialog_refresh_region` | graphics/dialog repaint (`ref:936-954`). |
| storm.dll globals (`0x15000000+`) | storm runtime; no sim state. |
| aise/gptp `.data`/`.bss` flat sections | rewinding heap pointers and `parking_lot` mutexes corrupts the plugin (ADR-0004 §Result) — use their serializers, not ranges. |

---

## recommended first addition

Smallest set most likely to clear the units/sprites/images/orders (+bullet) divergence, in order:

1. **Unit free list + count** — `0x00628438` (4), `0x0062843C` (4), `0x006283F0` (4). Clears `units`.
2. **Sprite free list + count** — `0x0063FE30` (4), `0x0063FE34` (4), `0x006C4A2C` (4). Clears `sprites`.
3. **Image free list** — `0x0057EB68` (4), `0x0057EB70` (4). Clears `images`.
4. **Order free list + count** — `0x0064B2E0` (4), `0x0064B2E4` (4), `0x00641698` (4). Clears `orders`.
5. **Bullet list + count + spawn scalars** — `0x0064EED8`, `0x0064EEDC`, `0x0064DEBC`,
   `0x0064DEAC`, `0x0064DEA8`, `0x0064DEB0`, `0x0064DEC0` (4 ea). Clears `first_active_bullet`/`bullets`.
6. **Thingy + fow free lists** — `0x00654878`/`0x0065487C`, `0x006509D0`/`0x00654870` (4 ea). Completeness.
7. **Active-list tails** — `0x0059CC9C` (last_active_unit), `0x00628428` (last_hidden_unit),
   `0x0059CC98` (last_dying_unit), `0x0065291C` (last_thingy), `0x0065486C` (last_fow_sprite).
8. **Trigger timers** — `0x0065097C` (2), `0x006509C0` (2), `0x00650980` (8×4),
   `0x006509B8` (8) — to clear `game.after_saved_screen`/`Game.elapsed_seconds`.

The `game` (minerals) divergence is **not** a range gap; it is plugin-heap-driven and is closed by
Layer B (aise) / the ADR-0005 gptp blob, not by adding ranges.

---

## unverified (flag — do not guess an address)

These are sim-relevant but I could not ground an address in `bwdata.h`, `samase`, or a disassembly
I completed. Each needs a disassembly pass before it is added (a wrong restore corrupts the game):

- **Trigger per-frame countdowns that are *not yet* grounded**: `trigger_result_check_timer`,
  `leaderboard_refresh_timer`, `turn_timer_accumulator` (SB `ranges.rs:423-456`). The two main
  per-frame timers (`trigger_execution_timer`, `trigger_elapsed_time_tick_timer`) and the
  per-player wait arrays are now grounded above; these three remain. Candidate globals already
  named in `ref`: `g_countdown_timer_interval` `0x0066FBFC`, `g_countdown_timer_tick_count`
  `0x0059CC7C`, `g_countdown_timer_tick_count_2` `0x0059CC88`, `g_countdown_time_remaining`
  `0x0059CC78` (`ref:440-444`).
- **Sync checksum ring + cursors**: `sync_data` (`0x10 × 0x10c`), `sync_check_kinds` (`0x20`),
  `current_sync_vision_bytes` (`0x100`), `sync_slot_index`, `sync_check_kind_index/count`,
  `sync_map_row_index`, `captured_minimap_*`, `current_sync_state_byte`, `current_sync_check_hash`
  (SB `ranges.rs:492-513, 621-635`). No 1.16.1 address found in the sources read.
- **AI counters/cursors**: `ai_spending_player_index`, `ai_military_update`,
  `ai_target_ignore_reset` ×3, `step_ai_regions_player/_region`, `ai_expansion_player_cursor`,
  the AI region reachability memo, `dcreep_next_update`, `dcreep_unit_next_update`, the dcreep
  lookup (`0x400` ptrs) and dcreep lists (`0xa × {ptr,u16}`). Disassemble the AI pool allocators
  and `AiStepRegion` (`0x0043FC60`, `samase_shim/src/bw.rs:77`).
- **worker/building/military AI pools + free heads**: not in `bwdata.h`; disassemble
  `0x00433DD0`/`0x00435770`/`0x0043DA20`.
- **`unit_availability` block** (`0x0057F27C`, `ref:247`) — inside the `game` range
  (`Game.unit_availability`, `bw_dat` `structs.rs:58`), so covered, but verify it is not split by a
  viewer-local field.
- **`trigger_completed_units_cache` / `trigger_all_units_cache`** (12×228×u32, SB
  `ranges.rs:540-556`) — the trigger unit-count caches; addresses not in the sources read.

Verification procedure for any of the above: locate the function that reads/writes the candidate
global in `objdump -d`, confirm the operand is a `mov`/`cmp` against a fixed `[disp32]`, and that
the address is a list head/cursor (as done for `0x628438`, `0x64b2e0`, `0x64eed8` above). Add it
as a named `RangeSpec` only after that confirmation.
