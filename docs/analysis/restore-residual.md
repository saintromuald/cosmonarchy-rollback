# Why the shadow gate diverges: the hook boundary, not plugin state

**Status:** analysis + experiment, 2026-10-18.
**Grounding:** the stock exe disassembled with `ndisasm -b 32 -o 0x401000`; the CI-built
gptp artifact; `CM_ROLLBACK_DET`, the shadow gate, and `crates/cosmo-rollback/src/rollback/mod.rs`.

---

## 0. The finding

The shadow gate's divergence — the project's central correctness blocker — is **not** caused
by un-snapshotted plugin state. Two independent results say so:

1. **Re-testing the plugin serializer on the fixed harness changes nothing.** ADR-0005's
   "serializing gptp state changes nothing" was measured on 2026-10-11, *before* the pool
   relocation and frame-boundary fixes, so it was confounded (ADR-0005 Update 2026-10-18).
   Re-run now, on the isolated copy of the game, with the version-matched CI artifact that
   carries Layer C **and** Phase 2 (`g_ext`):

   | gptp installed | serializers resolved | `state_size` | divergences |
   |---|---|---|---|
   | shipped (no exports) | `[]` | — | 104 (one per frame) |
   | CI `a506d0704` (Layer C + Phase 2) | `["gptp"]` | 10,319,476 B | 96 (one per frame) |

   The divergence is per-frame in both; the differing ranges are the same.

2. **A single step *is* reproducible, yet the shadow gate fails.** With the shipped plugin
   and `CM_ROLLBACK_NO_PLUGIN=1`, `CM_ROLLBACK_DET=120` reports `DETERMINISM f120: one step
   is reproducible`, with **no** uncovered exe `.data` bytes. If the engine were
   non-deterministic given the same start state, this probe would fail.

The explanation is structural: **the snapshot/re-sim boundary is not a closed simulation
step.** The hook wraps `0x004D9670`, but the game's outer loop does work *between* calls to
it, and that work mutates the state the snapshot covers.

---

## 1. The evidence, in the disassembly

`0x004D9670` is one logic frame: it calls the replay injector (`0x00487100`), the turn
scheduler (`0x004D9550`), and `StepObjects` (`0x004D94B0`) — plus `0x004DC540`, `0x0048A460`,
`0x004DEED0`. With its loop back-edge (`0x004D9823`) NOPed, one call = one iteration = one
frame.

But it is called from **one** place, the outer main loop at `0x004D9903`, and that loop runs
a great deal of other code every iteration:

```
0x4D986D  loop:
0x4D987C    call 0x4D1BF0        ; pump_messages(3)   (bwhelper.h:43)
0x4D9883    GetTickCount          ; pacing
0x4D98BD    call 0x4D14D0        ; update_cursor      (bwhelper.h:44)
0x4D98D0    call 0x47F0E0        ; scroll_screen_step (bwhelper.h:45)
0x4D98E7    call 0x485F70        ; receive_network_messages
0x4D98EC    call 0x47D160        ; (a `ret` — no-op)
0x4D98F1    call 0x47F590        ; send_bnet_league_query (bwhelper.h:47)
0x4D98F6    tick - g_next_tick_ms
0x4D98FF    js   0x4D990F        ; not this frame's time -> skip the frame
0x4D9903    call 0x4D9670        ; <-- the frame logic (our hook)
0x4D990F    call 0x4D93F0        ; post-frame update: 0x4C38B0, 0x45EBC0, 0x4599A0,
                                 ;   0x458120, 0x48DDC0
0x4D9914    ... 0x41CA00         ; dialog repaint
0x4D9945    jmp  0x4D986D
```

### 1.1 Proof that the between-frame work mutates simulation state

A `CM_ROLLBACK_BOUNDARY=1` diagnostic captures the state at the **exit** of each frame and
compares it with the state at the **entry** of the next one. If the hook boundary were a
closed step these would be identical. Measured on the isolated copy (shipped plugins,
`CM_ROLLBACK_NO_PLUGIN=1`, 145 frames):

```
[cosmo-rollback] BOUNDARY f1:   entry != exit(f-1)
[cosmo-rollback] BOUNDARY f2:   entry != exit(f-1)
...                                   (145/145 frames)
[cosmo-rollback] BOUNDARY f120: entry != exit(f-1)
    range images                       115 bytes differ (image 15743 +0xc)
    range unit_query_marks             6 bytes differ
    range unit_query_results           32 bytes differ
    range image_list_pointers          42 bytes differ
```

So the outer loop — almost certainly the post-frame update `0x4D93F0` (its callees are
render/update) — writes `Image` fields (flags at `+0xC`), the unit-query scratch arrays and
the image/sprite list-pointer arrays **between** two frames. A re-simulation that only
re-runs `0x004D9670` can never reproduce that.

**It is not only render state.** With the scratch arrays excluded from the snapshot, the
boundary diff still shows the outer loop writing **simulation** state:

```
    range images       200 frames
    range units         55 frames
    range sprites       48 frames
    range rng_seed       5 frames     <-- the RNG state advances outside the frame
    range orders         3 frames     <-- order allocation happens outside the frame
    range order_free_head 3 frames
    range order_count    2 frames
```

`rng_seed` (0x0051CA14) and the order free list are not render scratch: if the outer loop
consumes RNG and allocates orders, a re-simulation that skips it runs a **different RNG
stream** and a different order list. That is a genuine obstacle, not a comparison artefact.

**The obvious fix does not work.** Re-running the whole outer-loop between-frame work in the
re-simulation (`CM_ROLLBACK_RESIM_LOOP=1`: `pump_messages(3)`, `update_cursor`,
`scroll_screen_step`, `send_bnet_league_query`, `0x4D93F0`, `0x41CA00`) still leaves a
divergence on essentially every frame (frames 95–129, 131; `units`/`sprites`/`images`). So
the outer work's effect is not reproducible by replaying those calls from the restored
state — plausibly because it depends on viewer/camera state the snapshot deliberately
excludes, and/or because the outer loop runs a **clock-dependent number of iterations**
between two frames (the frame is gated on `GetTickCount` at `0x4D98FF` while the post-frame
update runs every iteration).

**Which call mutates?** A `CM_ROLLBACK_PROBE_OUTER=<frame>|all` diagnostic calls each
between-frame call in turn at a frame boundary and hashes the state after each. Across all
frames it reports exactly one mutator — `0x41CA00` (the call the outer loop makes at
`0x4D9933`, tentatively "dialog repaint") — writing `images` (105 frames) and `sprite_count`
(104 frames); every other call is inert. **Caveat:** the probe calls them out of their
original context (the outer loop sets registers first), so a call may take a different path
than it does live; this identifies the *site*, not a guaranteed live effect.

This also explains why `CM_ROLLBACK_DET` passes: it runs `orig` twice *back to back*, with no
outer-loop work between the two calls, so the boundary mismatch never appears.

**Partial fix tried and rejected:** re-running the post-frame update (`0x4D93F0`) after each
re-simulated frame (`CM_ROLLBACK_RESIM_POST=1`) did **not** remove the divergence — the
`Image` mutation it performs depends on viewer/camera state the snapshot deliberately
excludes, so replaying it out of phase reproduces a different result.

### 1.2 A second cause, and it was the harness itself

The shadow block used to leave the simulation at the **re-simulation's** end state and then
run the real frame from there, instead of restoring the frame's entry state first. Because
the re-sim ends at `exit(f-1)` while the live path continues from `entry(f)` (= `exit(f-1)` +
the between-frame work), every frame ran from the wrong state and the simulation drifted —
the shadow mode was perturbing the very thing it measured.

Restoring `before` after the re-sim is a correctness fix (the diagnostic must not perturb
its subject) and it changes the *character* of the residual: the ranges narrow to the
boundary ones (render/query scratch and `Image` flags), and `orders`/`game` drop out. The
raw divergence count is **not** a reliable measure — it tracks how many frames the replay
reaches, which varies run to run (measured: 104/89/37/21 across runs). The per-frame rate
stays about one, so the restore-back does not by itself close the gate.

| configuration | ranges seen |
|---|---|
| no restore-back | units, sprites, images, orders, game, scratch |
| restore-back | images, unit_query_*, image_list_pointers, sprite_list_pointers, units, sprites |
| restore-back, `CM_ROLLBACK_NO_RELOC_ARRAYS=1` | images, units, sprites |

So the residual is the boundary effect (render/query scratch and `Image` flags) plus a
`units`/`sprites` component the boundary diff does not show, which is still unexplained.

---

## 2. What this means

- **ADR-0003 open question 1 is still open.** The 2026-10-17 note ("answered no — but the fix
  is a back-edge patch, not a different hook target") fixed *one* of two boundary problems
  (the catch-up loop) and missed the other (the outer loop's between-frame work). A per-logic-
  frame hook is required **and** it must be placed so that the snapshot→re-sim cycle covers
  everything that advances the simulation.
- **The plugin serializers are not the blocker** for the shadow gate. They may still matter
  for a real rollback (heap state that a *closed* boundary would need), but they cannot fix
  this divergence, and the effort spent extending them is not on the critical path.
- **`mtl.qdp`'s 5720 bytes and the 8/22-byte gptp/aise `.data` residuals are not the cause**
  either — they are the same class of "state that differs because the two paths do different
  work", not necessarily simulation state.

---

## 3. The fix to try (cheapest first)

The re-sim must reproduce a full *simulation step*, including the outer-loop work that
advances it. Given the measurements above, in order of preference:

1. **Hook the outer loop instead of the frame function.** Hook the loop head (`0x004D986D`)
   so one hook call is one outer iteration, and snapshot/re-sim there. This is the only
   option that *encloses* the between-frame work. Problems to solve: the loop head is
   mid-function (needs an instruction-boundary detour, not a prologue hook); the frame is
   clock-gated at `0x004D98FF`; and the post-frame update runs even on iterations that skip
   the frame.
2. **Determine what the outer loop's sim-mutating calls actually are, and whether they are
   frame-paced or iteration-paced.** `rng_seed` and the order free list advance outside the
   frame. If those advances are frame-paced, a re-sim that runs them per step is correct; if
   they are iteration-paced (clock-dependent), the engine is only deterministic because the
   outer loop runs once per frame in practice — which must be verified. Instrument each
   outer call's state effect (hash before/after) to find the mutator; the `CM_ROLLBACK_BOUNDARY`
   diff already names the ranges, not the call.
3. **Mask render-derived fields only.** A subset of the residual is genuinely render state
   (`Image` flags at `+0xC`, `sprite_list_pointers`). Those may be excluded from the hash.
   This must **not** be applied to `rng_seed`/orders — those are simulation state.

**Experiment 2 is the next one to run**: find which outer call writes `rng_seed`/orders.

---

## 4. Result (2026-10-18, later) — the gate is essentially clean

Deep's boundary lane found the real mutator and I closed the remaining pools. From **37
divergences** the shadow gate is now down to **~3 rare, AI-only divergences** over ~135 frames:

| step | change | effect |
|---|---|---|
| 1 | re-run `receive_network_messages` (`0x00485F70`) in the re-sim (deep) | `rng_seed`, `orders`, `sprites`, `thingies` stop diverging |
| 2 | field-level pool ranges (`UNIT_SIM`/`SPRITE_SIM`/`IMAGE_SIM`) + a restore-only scratch blob (deep) | render-written pool fields leave the hash |
| 3 | **snapshot the `UnitPath` pool** | the movement divergence (`next_movement_waypoint` 0x18, `path` 0x101, `contour_bounds` 0x109) and its iscript cascade go away |
| 4 | **`units.render` for `Unit+0xCA`, excluded from the hash only** | the last outer-written byte leaves the hash; it is still restored |
| 5 | **snapshot the `AiScriptArray` and `AiTownArray` pools** | the AI list divergence is reduced to a few frames |

### 4.1 The pools that were never snapshotted

Both are *pointers* or arrays the earlier range table only covered the *heads* of:

- **`UnitPath`**: `g_path_array_start` (`0x006BEE8C`) holds the base; the pool is `0x400`
  entries of `0x80` = `0x20000` bytes (`0x0042F4F0` allocates it with `SMemAlloc(0x20000)` and
  links `cmp eax,0x400`). Heap-allocated, so the base is read at runtime. All values
  (coordinates/indices/`steps[24]`), so no pointer-rewind risk.
- **`AiScriptArray`**: `g_aiscript_array` (`0x0068C0FC`) is a pointer to it (stock
  `0x0068ACA8`); the pool is `AiScript[100]` + `first_free` = `0x1454` bytes — and the stock
  base plus that size lands exactly on the pointer variable, confirming the size.
- **`AiTownArray`**: `g_ai_town_array` (`0x006AA090`) is the array itself (`mov esi,0x6aa090`);
  `AiTown[100]` + `first_free` = `0x11F04` bytes.

### 4.2 `Unit+0xCA`: restored, but not hashed

The comparison of a frame's **exit** with the next frame's **entry** showed `Unit+0xCA`
written *outside* the frame, on a high-index (3943 of 4095 — a render-helper) unit, on ~27 of
145 frames. No re-simulation of the frame can reproduce that. But the same offset is
`larva_timer`/`flag_spawn_frame`/`hangar_link` for real units, so it must still be **restored**
for rollback correctness. The range table therefore splits it out as `units.render`: it is
snapshotted (so a restore puts it back) and `state_hash` skips any range named `*.render`.

### 4.3 What is left

Three divergences on ~2 of ~135 frames, all in the AI subsystem (`ai_script_pool`,
`first_ai_script`, `first_free_ai_script`, `ai_town_pool`), plus the hash-excluded
`units.render` bytes appearing as diff noise.

Adding the pools did **not** close it, so it is not a missing range: it is the AI's own
behaviour. The AI script interpreter is **`aise`**, a plugin whose state lives in its heap
`Globals` and is not snapshotted — Layer B, which ADR-0005 leaves unimplemented. The
serializer is ready to try (`cosmo_aise_*` exports added and built, uninstalled; see ADR-0006
and the change summary), but a rebuilt `aise.qdp` must match the shipped version before it can
be swapped in.

For v1 this is **not blocking**: ADR-0001 §4 defers AI ranges, and v1 is 1v1 human melee.
Every non-AI range now reproduces.

Also added and verified: the **`GuardAi` pool** (`GuardAiListPlayers` `0x00685108` → its first
`array` field; the base resolves to exactly the documented `0x0067D400`, `GuardAiArray` =
`GuardAi[1000]` + `first_free_guard` = `0x7D04` bytes).


