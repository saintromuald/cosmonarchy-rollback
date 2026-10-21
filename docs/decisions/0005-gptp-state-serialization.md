# ADR-0005 — gptp state serialization (Layer C)

- **Status:** Proposed
- **Date:** 2026-10-11
- **Decision owner:** architecture lead
- **Refines:** ADR-0004 (Layer C), ADR-0002 §2 (plugin-state capture)
- **Grounding:** `reference/cosmonarchy-gptp` (HEAD `b4c07c784`), in particular
  `globals.h`, `effect_field.h`, `globals.cpp`, `systems/triggers/trigger_system.h`,
  `systems/triggers/trigger_system.cpp`, `data_ext.h`/`data_ext.cpp`,
  `bullet_extender.h`, `BW/structures.h`, `BW/structures/UnitLayout.h`,
  `BW/structures/Unit.cpp`, `hooks/game_hooks.cpp`, `hooks/game_hooks_inject.cpp`,
  and `crates/cosmo-rollback/src/{frame_hook,rollback/mod,snapshot}.rs`.

---

## Context

ADR-0004 established that flat `.data` snapshotting of a plugin is unsafe (rewinding
heap pointers and `parking_lot` mutexes corrupts it) and that each plugin needs its own
`save`/`load`. Its "Update" section located gptp's state: two heap objects,
`*g_iquare_globals` (`globals.h:42`) and `*g_globals` (`trigger_system.h:263`), whose
`aise_save`/`aise_load` are stubs (`globals.cpp:12-19`). This ADR designs the Layer C
serializer for those two objects, the pointer-remapping scheme that makes their raw
`Unit*`/`Bullet*` fields survive a rollback, and the C ABI + harness wiring.

Two facts discovered in the sources sharpen the design (both beyond what ADR-0004
knew):

1. **The sim-relevant pointer fields are all in `IquareGlobals`, not `Globals`.** The
   effects' `source`/`origin`, the line-attack `source`/`target`, the multihit bullet's
   `already_hit`, the spine-hit map, and `bounce_bullet` live in `*g_iquare_globals`
   (`globals.h:47-54`, `effect_field.h:55-59`). `*g_globals` is mostly config/UI; its
   only per-frame sim state is the `trigger_exts` wait-timer map
   (`trigger_system.h:225`, `trigger_system.cpp:174-181`) and a few player/game-setup
   fields.
2. **There is a third, larger heap state not in either global: `g_ext`** (`DataExt`,
   `data_ext.cpp:29`), a per-unit/per-bullet/per-tile sidecar table holding
   `UnitExt::timers[]/values[]/units[]/unit_vectors[]` (`data_ext.h:620-634`) and
   `BulletExt` (`data_ext.h:640-663`). Effect ticks write straight into it, e.g.
   `target_ext.timers[TimerSalamanderFire] = 1secs` (`globals.cpp:198`) and
   `target->ext().units[UnitPlagueSource] = effect.origin` (`globals.cpp:333`). It is
   out of the two-object scope this ADR targets but is **flagged as Phase 2** (§6.1);
   the pointer scheme here is designed to extend to it unchanged.

---

## Decision

**Serialize `*g_iquare_globals` and the sim-relevant slice of `*g_globals` with a
hand-written, versioned binary serializer**, using an index/generation-based identity
for every raw `Unit*`/`Bullet*`/`TriggerAction*` pointer, and expose it as four C
exports the rollback DLL invokes at its existing 0x004D9670 frame boundary.

---

## 1. What exactly to serialize

Enum rules: `EffectId`, `SpriteId::Enum` are plain enums; serialize their underlying
numeric value (`u32`) and treat enum reordering as a version bump (the blob version,
§3, is the guard). `EffectRef` is `u64`. `v2u16` is two `u16`. `s32`/`u32`/`u8` are
POD. `SearchRadius` is `std::variant<u32, v2u16>` → write a `u8` tag then the payload.

### 1.1 `IquareGlobals` (`globals.h:42-80`)

| Field | Line | Sim? | Encoding |
|---|---|---|---|
| `output` (`std::string`) | 43 | viewer debug | **exclude** |
| `unitid_extender` (`bool`) | 45 | config toggle (set once) | `u8` (cheap; future-proof) |
| `m_next_effect_field_ref` (`EffectRef`) | 47 | **yes** | `u64` |
| `m_effects` (`vector<EffectField>`) | 48 | **yes** | `u32` count + records (§1.2) |
| `m_effects_to_add` (`vector<EffectField>`) | 49 | **yes** (pending queue) | same |
| `m_line_attack_hits` (`vector<LineAttackHit>`) | 50 | **yes** | count + `{UnitRef,UnitRef}` |
| `m_multihit_bullets` (`vector<MultihitBullet>`) | 51 | **yes** | count + `{bulletRef,s32,already_hit[]}` |
| `m_lakizilisk_spine_hits` (`unordered_map<Unit*,vector<Unit*>>`) | 52 | **yes** | count + `{UnitRef,count,UnitRef[]}` |
| `bounce_bullet` (`Bullet*`) | 54 | **yes** | `bulletRef` |

`m_lakizilisk_spine_hits` **must be serialized in a deterministic order**: iterate the
map keys, sort by their `UnitRef.value`, then emit. `std::unordered_map` iteration order
is unspecified and differs between save and load; leaving it unspecified would make two
identically-stated maps produce different bytes (a self-inflicted desync). Same rule for
any future map (`trigger_exts`, §1.3).

### 1.2 `EffectField` (`effect_field.h:45-68`)

| Field | Line | Sim? | Encoding |
|---|---|---|---|
| `ref` | 46 | yes | `u64` |
| `id` (`EffectId`) | 47 | yes | `u32` |
| `position` (`v2u16`) | 48 | yes | 2×`u16` |
| `radius` (`SearchRadius`) | 49 | yes | tag `u8` + payload |
| `current_time`, `max_time` | 50-51 | yes | 2×`u32` |
| `sprite_id` | 52 | gate for thingy | `u32` |
| `elevation`, `player` | 53-54 | yes | 2×`u32` |
| `thingy` (`Thingy*`) | 55 | **viewer** | **drop → null** (see below) |
| `source` (`Unit*`) | 56 | **yes** | `UnitRef` |
| `origin` (`Unit*`) | 59 | **yes** | `UnitRef` |
| `intervals`, `move_target`, `move_speed` | 60-62 | yes | `u32`, `v2u16`, `u32` |
| `tick` (`s32`) | 65 | yes | `s32` |
| `playing_sound` (`PlayingSound`) | 66 | **audio** | **drop → empty** (see below) |

`thingy` and `playing_sound` are deliberately not serialized:

- `Thingy*` is a sprite handle for the visual field; `tick_effect` re-creates it when
  absent (`if (!effect.thingy || !effect.thingy->sprite) effect.thingy = create_and_init_thingy(…)`,
  `globals.cpp:148-151`). Saving `null` costs nothing and re-establishing it is the
  code's own invariant.
- `PlayingSound` wraps `SoundBuffer*` + a `u64` generation into miniaudio
  (`audio_system.h:104-116`); the loop-sound path re-creates it each tick
  (`globals.cpp:140-146`). Serializing a raw audio handle is meaningless across a
  rollback; store an empty handle and let the next `tick_effect` re-establish it.

### 1.3 `Globals` (`trigger_system.h:208-234`)

Sim-relevant subset only:

| Field | Line | Encoding |
|---|---|---|
| `players[i].faction` | 126 | `u8` |
| `players[i].neutral_hostile` | 128 | `u8` |
| `players[i].no_starting_units` | 127 | `u8` |
| `players[i].disable_liftoff/burrow/…tactical_ai/extended_waves` | 131-135 | `u8` each |
| `players[i].disabled_units`, `hidden_units` (`UnitIdBitset`) | 136-137 | fixed-size bitarray |
| `players[i].game_mode_race` | 139 | `u8` |
| `players[i].build_multiplier` | 140 | `u32` |
| `yields[6]` (`u8`) | 217 | 6×`u8` |
| `trigger_exts` (`unordered_map<TriggerAction*,TriggerExt>`) | 225 | see below |
| `uuid_rand` (`vector<char>`) | 230 | count + bytes |
| `celebration` (`u32`) | 211 | `u32` (cheap; cosmetic) |

`trigger_exts` → `TriggerExt` is `{ OngoingWait ongoing_waits[8] }`
(`trigger_system.h:172-180`), and `OngoingWait` is `{s32 timer; u32 parser_position;
bool active}` (`trigger_system.h:172-176`). Emit `count`, then per entry `{TriggerActionRef,
8×(s32,u32,u8)}`, key-sorted by `TriggerActionRef` for determinism.

**Excluded (viewer/UI/audio/diagnostic):** `preferred_colors_set`,
`players[].camera/witness_mode/preferred_colors/custom_unit_names/ally_alert_blacklist/
hide_in_team_ui`, `alert`, `current_advisor`, `advisor_state`, `desync_state`,
`forbid_replay_auto_upload`, `disable_transmission_waits`, `sound_replacements`,
`sprite_replacements`, `trigger_buttons`, `eco_measurements`.

---

## 2. Pointer-remapping scheme

The crux: the state holds raw `Unit*`/`Bullet*`/`TriggerAction*`. The engine snapshot
already restores the unit table, the bullet pool, and the trigger lists to the rollback
frame — so a *raw pointer* would often still be valid. But relying on that is fragile
(no validation, breaks the moment the referenced object is a generation-older or the
engine relocates a pool). We instead serialize a **stable, validatable identity** and
rebuild the pointer on load, dropping references that do not resolve.

### 2.1 `Unit*` → `UnitRef` (32-bit)

The engine already defines the canonical pair. `Unit::get_unit_index()` returns
`this - g_unit_table + 1` (1-based, `Unit.cpp:1179-1182`); `UnitLayout::unique_id` is a
`u8` generation counter at offset `0xA5` (`UnitLayout.h:302`); `UnitRef` packs
`{index:12, unique_id:4}` (`UnitLayout.h:190-200`) and `UnitIndex` packs
`{index:12, unique_id:20}` (`structures.h:1323-1346`). Only 8 unique-id bits are ever
set (the field is `u8`), so the wire format is:

```
UnitRef = u32
  bits 0..11 : index        (1-based; 0 == null)
  bits 12..19: unique_id    (u8 generation)
  bits 20..31: reserved = 0
```

Encode: `index = unit->get_unit_index(); unique_id = unit->unique_id`.
Decode/validate (mirror `UnitIndex::get`, `structures.cpp:5-25`, plus a liveness check):

```cpp
Unit* resolve_unit(UnitRef r) {
    if (!r) return nullptr;
    Unit* u = &g_unit_table[((r >> 0) & 0xFFF) - 1];
    if (u->unique_id != ((r >> 12) & 0xFF)) return nullptr;   // wrong generation
    if (!u->sprite || (u->sprite->flags & SpriteFlags::Hidden)) return nullptr;
    if (u->main_order_id == OrderId::Die && u->main_order_state == 1) return nullptr;
    return u;
}
```

### 2.2 `Bullet*` → pool index (16-bit)

Bullets have no generation field (`Bullet` layout, `Bullet.h:8-53`; the only near
counter is `cyclic_missile_index`, a burst counter, not unique). But the bullet pool is a
fixed static array `Bullet new_bullet_table[BulletLimit]`
(`bullet_extender.h:4`, `bullet_extender.cpp:5`), and `get_bullet_ext` already computes
`index = bullet - g_bullet_table` (`data_ext.cpp:190-194`). So the wire identity is the
1-based pool index (`0 == null`), and validity is "slot occupied":

```cpp
Bullet* resolve_bullet(u16 idx) {
    if (!idx || idx > BulletLimit) return nullptr;
    Bullet* b = &new_bullet_table[idx - 1];
    return b->sprite ? b : nullptr;   // alive if it has a sprite
}
```

Because multihit entries are removed when their bullet dies (`remove_multihit_bullet`,
`globals.cpp:590-597`), and the engine snapshot restores the bullet pool to the rollback
frame, a live multihit bullet is always present at its index after restore; a dropped
index means the bullet was dead at the snapshot frame anyway.

### 2.3 `Thingy*` → always null

Not serialized; re-created lazily (§1.2). This removes the only non-pool pointer from
`EffectField`.

### 2.4 `TriggerAction*` (map key) → `TriggerActionRef` (16-bit index)

The `trigger_exts` key is a raw `TriggerAction*` into the engine's trigger list
(`TriggerAction` is a POD, `structures.h:1230-1244`). Encode it as its ordinal among the
engine's trigger actions (walk the linked trigger list / fixed action array to a flat
index). On load, resolve to `&action_array[index]` and validate the pointer is within the
action array; if the trigger list was rebuilt and the index is out of range, **drop the
entry**. (If the action base symbol is not convenient to resolve, fall back to a
snapshot-relative `u32` offset; the engine restores the trigger list, so this is stable
within a rollback window. Flag as a build-time decision in §6.1.)

### 2.5 Drop/keep rules (uniform)

On load, for every pointer field:
- **resolves & valid** → write the rebuilt pointer;
- **null already** → keep null;
- **dangling / dead / wrong generation** → **null it** (or erase the map/vector entry).

Dropping is always safe: an effect whose `source`/`origin` died will `destroy()` itself
on the next tick (`globals.cpp:178-182, 291-294`), and `unit_death` is what would have
nulled it anyway (`globals.cpp:21-48`). Keeping a dangling pointer would be UB; dropping
is the correct-and-cheap default. The cost is a bounds check + generation compare per
pointer, i.e. a handful of instructions on top of the existing memcpy.

---

## 3. Mechanism: hand-written binary serializer (recommended) vs RFL

### Recommendation: hand-written, versioned binary

RFL/reflectcpp is already a vcpkg dependency (`vcpkg.json` → `reflectcpp`) and is used
for YAML in `CampaignManager.cpp`. It is **not** the right tool here:

- **It needs annotations.** `IquareGlobals`/`Globals`/`EffectField` are plain structs
  without `rfl::Field`; reflecting them means annotating the structs (changing their
  layout semantics project-wide) or writing a hand `rfl::Reflector<T>` for each — which
  for `unordered_map<Unit*,vector<Unit*>>`, `std::variant`, and the pointer fields is
  **more** code than a hand writer, not less.
- **It is not byte-deterministic for these types.** RFL serializes `unordered_map` in
  iteration order (unspecified); we would have to canonicalize ordering ourselves anyway.
- **It is slow for a per-frame hot path.** YAML/JSON are text, allocate heavily, and are
  orders of magnitude too slow to run every `spacing` frames inside the rollback ring;
  `flexbuf`/binary formats still route through RFL's field-name/type machinery.
- **Custom parsers for pointers still needed.** RFL has no concept of "UnitRef →
  validate → rebuild"; every pointer field still requires a bespoke `Reflector` with the
  §2 logic.

A hand-written serializer is ~300–400 LOC, gives exact control over wire layout (fixed
widths, no padding surprises across MSVC/ABI), is trivially byte-deterministic (write
fields in a fixed order, sort maps), and inlines the §2 remap + validation. It also keeps
the serializer dependency-free (no RFL/yaml-cpp linkage in the hot path).

### Versioned blob format

```
offset 0:  u32 magic  = 'GPTS'
offset 4:  u32 version = 1
offset 8:  u32 total_len  (bytes that follow)
offset 12: section IquareGlobals
offset ..: section Globals
```

Each section is length-prefixed (`u32 len` + payload) so the reader can skip an
unrecognized section on a version mismatch and keep forward-compat. Save writes sections
in order; load checks `magic`, then `version` (mismatch → fail closed, log, return 0).
All integers little-endian (x86 native).

RFL is retained for **offline migration/debug dumps** only (a debug build may emit the
same structs as YAML for diffing), never for the rollback ring.

---

## 4. C exports + harness wiring

### 4.1 gptp exports (new translation unit, `cosmo_rollback_state.cpp`)

```cpp
extern "C" __declspec(dllexport) u32  cosmo_gptp_state_size();          // bytes the next save needs (0 = disabled)
extern "C" __declspec(dllexport) u32  cosmo_gptp_save(u8* dst, u32 cap);// -> bytes written, 0 on error/cap-too-small
extern "C" __declspec(dllexport) u32  cosmo_gptp_load(const u8* src, u32 len); // -> 1 ok, 0 on bad version/parse
extern "C" __declspec(dllexport) void cosmo_gptp_rollback_enable(int);  // 0/1; gates the three above
```

- `state_size()` computes (and caches) the current serialized size; returns `0` while
  rollback is disabled, so the harness can skip the plugin entirely.
- `save(dst,cap)` serializes `*g_iquare_globals` + the §1.3 slice of `*g_globals` into
  `dst`; returns bytes written. Never throws; returns 0 on any failure.
- `load(src,len)` deserializes and **mutates in place** (`*g_iquare_globals = …`,
  rebuild `trigger_exts`), running the §2 remap/validation. Idempotent and
  allocation-safe (reuses the vectors).
- These are **separate** from the `aise_save`/`aise_load` stubs (`globals.cpp:12-19`),
  which remain untouched (they are the BW save-game hook path).

### 4.2 Harness wiring (`crates/cosmo-rollback`)

`modules.rs` already enumerates the loaded `.qdp` modules; extend it to resolve the four
symbols from `gptp.qdp` via `GetProcAddress` and cache the function pointers. Extend
`Snapshot` (`snapshot.rs:7-10`) to carry `plugin_blobs: Vec<PluginBlob>`
(`{ name, Vec<u8> }`), and `Engine::on_step` (`rollback/mod.rs:67-111`) to:

1. **save** (at the same boundary as the flat capture, `mod.rs:70-78`): call
   `cosmo_gptp_save` into a scratch buffer (sized by `state_size()`), push the blob into
   the ring entry alongside the flat `Snapshot`.
2. **restore** (in `restore`, `mod.rs:87`): restore the flat engine ranges **first**,
   then call `cosmo_gptp_load` — so the §2 resolver sees the already-rolled-back unit
   table / bullet pool / trigger list.

Ordering matters only on restore (pointers must resolve against restored engine state);
save order is immaterial. If `state_size()==0` or the symbol is absent, skip the plugin
path entirely (no behavior change).

### 4.3 When save/load run relative to the frame boundary

The harness trampoline is at **0x004D9670** (`frame_hook.rs:18-20`), the engine per-frame
function. gptp's own per-frame work runs **after** it: `next_frame_hook` is patched at
`0x004D974E` (`game_hooks_inject.cpp:332`) and `hooks::next_frame()` calls
`update_trigger_system(true)` and `g_iquare_globals->tick()`
(`game_hooks.cpp:836,944`). So at the 0x004D9670 entry point, gptp is **quiescent**
(between its per-frame updates). Snapshotting there captures gptp state that is exactly
consistent with the engine state the harness snapshots at the same point, and it is
never mid-mutation. The re-sim path re-runs `orig` (0x004D9670) but **not**
`next_frame()`, which is correct: gptp's engine-hooks (weapon fire/damage/unit death)
fire *inside* 0x004D9670 and must see restored gptp state; `next_frame()` itself is not
re-run.

---

## 5. Where to call it (minimal gptp edits)

- **New file** `cosmo_rollback_state.cpp` (+ header) implementing §1–§4.1. Add it to
  `GPTP.vcxproj`.
- **No change** to `globals.cpp` tick path, `game_hooks.cpp`/`next_frame()`, or the
  existing `aise_save`/`aise_load`. Save/load are only reachable through the exports.
- **Inert when rollback is off:** `cosmo_gptp_rollback_enable(0)` is the default; until
  the harness calls `enable(1)` (which it only does when it resolved the symbols), the
  exports no-op and `state_size()` returns 0. Normal single-player / lockstep play never
  touches the serializer, so there is zero perf or behavior change.
- **Lifecycle guard:** `g_iquare_globals` is created in `game_init_first_frame`
  (`game_hooks.cpp:661`); `save`/`load` must early-return if either global is null
  (between games / in menus).

---

## 6. Test plan & risks

### 6.1 Scope gap (Phase 2) — the biggest correctness risk

`*g_iquare_globals` + `*g_globals` do **not** cover `g_ext` (`data_ext.cpp:29`),
`g_trigger_vars` (`trigger_vars.h:101`), `g_ai_layouts` (`ai_layouts.h:184`),
`g_active_game_mode` (game-mode state), and `lua::g_script_manager` — all gptp-owned,
all sim-relevant. The effects/timers/`unit_vectors` that live in `g_ext` are the most
likely remaining divergence source after this ADR. **Mitigation:** the shadow gate
(§6.3) is the arbiter; if divergence remains, extend the same blob + §2 scheme to
`g_ext` (serialize `UnitExt`/`BulletExt` arrays with `UnitRef`/bullet-index remap of
their `Unit*`/`unit_vectors`), which is a mechanical extension of this design.

### 6.2 CI build gate

Build `gptp.qdp` via GitHub Actions (`saintromuald/cosmonarchy-gptp`: MSVC + vcpkg).
Assert the four exports exist (`dumpbin /exports`) and that `state_size()` returns 0
before `enable(1)`. Add a debug-only self-test: play N frames, `save` → `load` →
`save` again, assert byte-identical blobs (proves deterministic ordering, catches the
`unordered_map` ordering trap of §1.1).

### 6.3 Headless shadow gate (acceptance)

Run the existing harness (`tools/headless_probe.sh`) with `CM_ROLLBACK_SHADOW=3` and the
gptp blob registered. **Pass = the residual divergence clears**: `rng_seed`
(0x0051CA14), `minerals[0]` (0x0057F0F0), `Game+0xe608`, and `first_active_bullet`
(0x0064DEC4) must no longer appear in `DIVERGENCE` lines (`rollback/mod.rs:92-97`).
Interpretation: if `minerals`/`e608`/`bullet` clear but `rng_seed` remains, the residual
RNG consumer is in the §6.1 scope gap (`g_trigger_vars`/`g_ext`/game-mode), not in the
two globals; if all four clear, Layer C is complete for the two-object scope.

### 6.4 Risks

| # | Risk | Mitigation |
|---|---|---|
| R1 | §6.1 scope gap: `g_ext`/`g_trigger_vars`/`g_ai_layouts`/`g_active_game_mode` still drive divergence. | Shadow gate signature per-field; extend the same scheme (Phase 2). Fail-closed gate refuses to start a rollback session until all state-owning plugins register (ADR-0004 §4). |
| R2 | Enum/id ABI drift (`EffectId`, `SpriteId`, `WeaponId` reorder between gptp builds) makes a blob from one build misparse on another. | Blob `version` bump on any enum reorder; `magic`/`version` check fails closed; only the same-build blob is ever loaded in a session. |
| R3 | `unordered_map` (spine-hits, `trigger_exts`) non-deterministic ordering → two identical states serialize differently. | Sort by key (`UnitRef.value` / `TriggerActionRef`) before writing (§1.1); proven by the §6.2 self-test. |
| R4 | `thingy`/`playing_sound` drop causes a one-frame visual/audio pop after restore. | Accepted: they re-create on the next tick (`globals.cpp:140-151`); pure viewer/audio, no sim effect. |
| R5 | `TriggerAction*` index remap needs a stable action-array base symbol. | Fall back to snapshot-relative `u32` offset (§2.4); validated by the shadow gate (trigger waits affect `rng_seed` via `rand()`). |

---

## Open questions

1. Does the shadow gate clear all four fields with only the two-object scope, or is
   Phase 2 (`g_ext`) immediately required? (First §6.3 run answers this.)
2. Is the `TriggerAction` action-array base address resolvable in gptp's `bwdata`, or
   must §2.4 fall back to a snapshot-relative offset?
3. Does `g_active_game_mode` (TheTurning/Uprising game modes) hold per-frame sim state
   that must join the blob, or is its state derivable from engine state?

---

## Result (2026-10-11) — implemented, built, wired; acceptance blocked by gptp version mismatch

- **Implemented** `cosmo_rollback_state.cpp` in `reference`/the mirror: hand-written versioned
  serializer for `*g_iquare_globals` (effects, multihit bullets, line-attack hits, spine-hit map,
  bounce bullet) with `UnitIndex`/bullet-pool-index remapping + drop-on-invalid, and the four C
  exports. Added to `GPTP.vcxproj`. (First cut: `IquareGlobals`; the `Globals` slice and `g_ext`
  are Phase 2 per §6.1.)
- **Built** via CI (`saintromuald/cosmonarchy-gptp`): succeeds; the produced `CM-GPTP-Release.qdp`
  (5.14 MB) exports `GetMPQDraftPlugin`, `cosmo_gptp_state_size`, `cosmo_gptp_save`,
  `cosmo_gptp_load`, `cosmo_gptp_rollback_enable`.
- **Wired the harness**: `crates/cosmo-rollback/src/plugin_state.rs` resolves the exports from
  `gptp.qdp` via `GetProcAddress`; `Snapshot` carries a plugin blob captured at the frame boundary
  and restored after the flat ranges. `cosmo-rollback` builds; at load it logs
  `gptp serializer resolved: true` and calls `rollback_enable(1)`.
- **Acceptance test blocked:** with our built gptp installed, the replay **never reaches
  gameplay** (blank screen; no frame hook fires) even after 150 s. The shipped gptp reaches
  gameplay in <45 s. Cause: our build is gptp **HEAD `b4c07c784` (latest master)**, but the
  current Cosmonarchy build is `3d8d947`, which uses a *specific, older* gptp revision — so the
  replay/map won't load. This is a **version-matching problem, not a serializer defect**.

**Next:** build gptp at the revision matching the current Cosmonarchy build (find the commit the
Fraud Launcher pins for `3d8d947`), then re-run the §6.3 shadow gate. The serializer itself
compiles, exports, and is wired end-to-end.

### Update (2026-10-11, later) — matched revision built; Layer C active; residual unchanged

- **Matching revision found:** the gptp `release` branch is stale (2024); the current build uses
  master, whose tip at the build date (2026-09-19) is **`9c4c436b`** (2026-09-16). Built it via CI:
  the artifact is **4,912,640 B** vs the shipped **4,938,240 B** (within 0.5%) and plays the replay
  to completion — so `9c4c436b` is the match.
- **Two harness bugs fixed to reach the test:** (1) the plugin-state exports are `__cdecl`; the Rust
  fn pointers were `extern "system"` (= `__stdcall` on i686) → stack corruption → crash; changed to
  `extern "C"`. (2) The unsafe Layer A (`modules::plugin_ranges()`, plugin `.data` ranges) was still
  wired into `ranges.rs` → restore corrupts plugins; removed (superseded by Layer C).
- **Result:** with the matched gptp + Layer C, the harness runs to frame 115 with **no crash**, the
  serializer is active (`gptp serializer resolved: true`; `state_size`/`save` return 13→44 bytes),
  but the **shadow divergences are unchanged** (`rng_seed` ×26, `game` ×23, `first_active_bullet` ×8).
- **Interpretation:** `*g_iquare_globals` is nearly empty during this replay (44-byte blob) and is
  **not** the residual source. Per §6.1, the residual is the **scope gap** — `g_ext`
  (`UnitExt`/`BulletExt` timers/`unit_vectors`), `g_trigger_vars`, `g_ai_layouts`,
  `g_active_game_mode` — **and/or `aise`** (Layer B, never implemented; its `Globals` is
  unsnapshotted and its engine hooks consume RNG). So Layer C is correct but incomplete: **Phase 2
  (`g_ext`) + Layer B (`aise`) are required** to clear the residual.

### Phase 2 result (2026-10-11) — `g_ext` serialized; residual STILL unchanged

Implemented Phase 2 in `cosmo_rollback_state.cpp` (v2 blob): `UnitExt` via POD-prefix memcpy
(`offsetof(UnitExt, value_vectors)`) + the three `std::vector` arrays (`value_vectors`,
`unit_vectors` with `UnitRef` remap, `timer_vectors`) serialized separately, and `BulletExt` as raw
POD. Built via CI (compiles) and ran the gate: the blob grows to **~10.3 MB** per snapshot, but the
shadow divergences are **byte-for-byte the same** (`rng_seed` ×26, `game` ×23,
`first_active_bullet` ×8).

**Conclusion:** neither `*g_iquare_globals` nor `g_ext` is the residual source. Serializing gptp
state changes nothing, so the divergence is driven by something outside gptp — most likely **`aise`**
(Layer B, its `Globals`/RNG still unsnapshotted), or a boundary issue (RNG consumed outside
0x004D9670). A cheap diagnostic (disable `aise.qdp`) was inconclusive: the replay won't load without
aise. **Next: Layer B (`aise`).** If that also fails to move the divergence, the boundary/architecture
(ADR-0003 open Q1) is the remaining suspect, not plugin state.

### Update (2026-10-18) — the Phase 2 negative result is **confounded**; re-test pending

The conclusion above was reached on 2026-10-11, **before** the 2026-10-17 work that (a) resolved the
object pools through the CCMU extender (`pools.rs`) — until then the snapshot copied arrays the
engine no longer used — and (b) fixed the frame boundary (the `0x004D9670` catch-up back-edge NOP).
Both are exactly the failure modes that make a re-simulation diverge, so **"serializing gptp state
changes nothing" cannot be trusted**: the harness it was measured on was copying the wrong arrays and
running a variable number of logic frames per hook call.

What *is* still true and was re-measured on 2026-10-17:

- With the pools resolved and the boundary fixed, a **single step is reproducible in every flat
  range** (`CM_ROLLBACK_DET=1`), and only **5 bytes** of the exe's `.data` remain uncovered (all
  non-simulation).
- The shadow gate still diverges every frame, and the reproducibility probe reports the
  un-snapshotted bytes per **plugin writable section**: `gptp.qdp..data` 8 B, `aise.qdp..data` 22 B,
  `mtl.qdp..data` 5720 B. Pinning the small scalar-looking parts of those by section+offset did not
  help and was reverted (ADR-0004's flat-`.data` warning).

**The artifact needed to re-test Layer C + Phase 2 is available and version-matched.** The mirror
`saintromuald/cosmonarchy-gptp` has:

- `fe493cfa2` "Cosmonarchy-GPTP @ `9c4c436b` (2026-09-16) + rollback serializer" — the
  version-matched base + Layer C;
- `a506d0704` "Phase 2: serialize `g_ext`" — the current HEAD;
- CI run `38178530308` (2026-10-11, success) → artifact `CM-GPTP-Release`,
  `CM-GPTP-Release.qdp` = 4,914,176 B, exporting `GetMPQDraftPlugin`, `cosmo_gptp_load`,
  `cosmo_gptp_rollback_enable`, `cosmo_gptp_save`, `cosmo_gptp_state_size`.

The shipped `gptp.qdp` is **4,938,240 B**, the same size as when this ADR matched the revision, so
the artifact should fit (the ADR's "plays the replay to completion" test is the gate).
`crates/cosmo-rollback/src/plugin_state.rs` already resolves `cosmo_gptp_*`/`cosmo_aise_*` and
captures/restores one blob per plugin, so the re-test needs only: install the artifact as
`gptp.qdp`, run `CM_ROLLBACK_SHADOW=<depth>` with plugin state enabled, and compare the residual to
the current one. **This is the next experiment; the shipped plugin is backed up at
`gptp.qdp.shipped-backup`.**

Layer B (`aise`) is also feasible **without CI**: aise is a Rust `cdylib` (edition 2024) whose deps
(`samase_plugin`/`whack` at the same revs cosmo-rollback uses) are already in the local cargo cache,
and `cosmo-rollback` cross-builds with the same `i686-pc-windows-msvc` + `xwin` toolchain. aise
already has a bincode `save`/`load` for `Globals` (`src/globals.rs:822,837`, registered via
`extend_save`, `src/samase.rs:635`) but exports only `GetMPQDraftPlugin`/`Initialize`/
`samase_plugin_init`/`rust_eh_personality` — so it needs a fork adding `cosmo_aise_*` exports and a
local build.
