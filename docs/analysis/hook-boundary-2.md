# Hook boundary, take 2: the replay command stream is the missing input

**Status:** design proposal (corrects the `hook-boundary.md` conclusion that the pre/post passes
were the culprit — they are not).

**Grounding:** shipped stock `StarCraft.exe` disassembled with `objdump -d`; gptp source
`reference/cosmonarchy-gptp/hooks/{game_hooks.cpp,game_hooks_inject.cpp,game_hooks.h}` and
`BW/bwdata.h`; `bw_dat` (`aise` checkout) `structs.rs` for the `ReplayData` layout.

---

## 0. The falsified hypothesis, and what the new evidence means

NOPing the back-edge made `run_game_tick` run exactly one loop-body iteration, yet the divergence
was byte-for-byte unchanged. That is decisive: the `K`-frames catch-up was **not** the cause, and
neither is the pre/post render pass. The decisive experiment — `CM_ROLLBACK_SHADOW=1
CM_ROLLBACK_SNAPSHOT_SPACING=1` (restore frame `f-1`, run **one** `orig`, compare to frame `f`) —
shows `orig()` itself does not reproduce frame `f` from frame `f-1`, with `image_free_head` and
`order_free_head` differing after a single step. `SELF-RESTORE ok` proves capture/restore is
lossless, so the missing work is **state that `orig()` reads but the snapshot does not capture** —
i.e. **input**, not simulation state.

That input is the **replay command stream**, injected *inside* `run_game_tick` by gptp's
`replay_loop`, which advances a set of replay cursors that are **not in the snapshot**.

---

## 1. Where the replay commands come from (inside `orig`)

`call_patch(replay_loop, 0x004D96C4)` (`game_hooks_inject.cpp:344`) redirects the `call 0x487100`
inside `run_game_tick`'s loop body (`0x4d96bb`–`0x4d96c4`, the `is_replay` branch). `replay_loop`
(`game_hooks.cpp:5139`) is the replay command injector:

```cpp
void replay_loop() {                                   // runs at 0x4D96C4, every logic frame
    if (!g_sync_turn_enabled) return;                  // 0x6556E0
    s32 elapsed_time = g_elapsed_time_frames;          // = Game.frame_count 0x57F23C
    if (elapsed_time < g_replay_end_frame) {           // 0x6D0F31
        s32 next_frame_time = *next_replay_command_frame;   // 0x5136C8
        while (elapsed_time >= next_frame_time) {
            if (elapsed_time <= next_frame_time) {
                // inject the commands currently in g_replay_cmd_buffer (for this frame)
                ... helper::game_recv(&g_replay_cmd_buffer[cur], cmd_size, true); ...
                g_replay_cmd_count = 0;
            }
            if ((*next_replay_command_frame = next_frame_time = get_replay_turn_data()) == -1) break;
        }
    } ...
}
```

`get_replay_turn_data` (`game_hooks.cpp:5083`, `call_patch`ed over `0x4CDDF0` at
`game_hooks_inject.cpp:343`) reads `replay_data->cur_frame` and advances it, copying the next
frame's commands into the flat buffers.

So the replay input stream is read and advanced **inside `orig`**, once per logic frame. The
mutable input state it depends on:

| name | addr | what it is | snapshotted today? |
|---|---|---|---|
| `next_replay_command_frame` | `0x5136C8` (`bwdata.h:303`) | frame number whose commands are staged | **no** |
| `g_replay_data` → `ReplayData.cur_frame`/`cur_command` | `0x596BBC` → `+0x1C` | pointer cursor into the replay buffer (`ReplayData` layout, `bw_dat` `structs.rs:251-263`) | **no** |
| `g_replay_end_frame` | `0x6D0F31` (`bwdata.h:344`) | replay length | **no** (constant, but cheap) |
| `g_replay_cmd_buffer` | `0x6552B0` (`bwdata.h:1429`) | staged command bytes | **no** |
| `g_replay_cmd_players` | `0x6554D8` (`bwdata.h:1430`) | staged command sender ids | **no** |
| `g_replay_cmd_sizes` | `0x654AA8` (`bwdata.h:1431`) | staged command sizes | **no** |
| `g_replay_cmd_count` | `0x6D5BF0` (`bwdata.h:1432`) | staged command count | **no** |
| `g_sync_turn_enabled` | `0x6556E0` (`bwdata.h:1433`) | replay injection gate | **no** (constant) |

Because none of these are restored, after a restore the cursors sit at frame `f`'s position. Re-run
`orig` for frame `g` (`g < f`): `replay_loop` computes `elapsed_time (= g) >= next_replay_command_frame
(= f)`, which is false, so the `while` body is skipped and **no commands are injected** — while the
normal path at frame `g` *did* inject. `StepObjects` then runs with a different (or empty) command
set, which is exactly "one `orig` from `f-1` does not reproduce `f`", and explains
`image_free_head`/`order_free_head` moving after one step (commands create units/orders/overlay
images). The `units/sprites/images` free-tail byte differences are the downstream symptom.

---

## 2. Q1 — the pre/post passes do not touch the pools

Every function between two `run_game_tick` entries was identified; none allocates or frees an
image/order/sprite from the main pools, and none writes `Unit+0x13C-0x14C` (`finder_index`) or the
free-tail pool bytes:

| addr | name (bwhelper.h) | what it does | pool writes? | evidence |
|---|---|---|---|---|
| `0x4D14D0` | `update_cursor` | picks the cursor GRP from mouse/idle state | no | reads `0x596B70` (`g_current_cursor_id`), `0x596B20` (`g_cursor_grps`); calls `0x4BE120` refresh. No pool operand. |
| `0x485F70` | `receive_network_messages` | Storm `SNetReceive` pump + dispatch table `0x50439C` | no (no-op in replay) | calls `0x4101F0` (storm recv) → returns 0 with no network; never touches pools |
| `0x47D160` | — | `ret` stub | no | single `c3` |
| `0x47F590` | `send_bnet_league_query` | BNET ladder poll (wall-clocked) | no | gates on `is_multiplayer 0x57F0B4` and mode `0x59688C`; reads `0x4FE0C4` (clock) |
| `0x4D93F0` | (UI pass) | dispatches UI updates | no | see callees below |
| `0x4C38B0` | (selection copy) | copies active selection `0x6284B8`→`0x597208` (12 ptrs), prunes dead | no | `rep movsl`, calls `0x49A350` per unit |
| `0x45EBC0` | (portrait update) | refresh unit portrait | no | reads `0x68AC74`/`0x68AC60`/`0x68AC98` |
| `0x4599A0` | (buttonset update) | refresh command buttons | no | reads `0x68C1B0`/`0x68C1C8` |
| `0x458120` | (status screen update) | refresh status panel | no | reads `0x597248`/`0x68C1F8`/`0x68C1F0` |
| `0x48DDC0` | `update_placement_state_for_placement_box` | building placement preview from `g_mouse` | no | reads `0x640880` (`g_is_placing_building`), `0x6CDDC4` (`g_mouse`) |
| `0x41CA00` | `refresh_region` (`H_RenderScreen`) | draws sprites/images to screen | **writes `Image` render fields only (active images)** | see §4 |

**Conclusion for Q1:** the `image_free_head`/`order_free_head`/`Sprite.position`/`finder_index`
movement is **not** from these passes. It is from the replay command injection (§1) that the re-sim
misses.

---

## 3. Q3 — why `image_free_head` / `order_free_head` move

Orders are created only by command processing (`targeted_order` `0x754F0` and the order-queue
functions; the allocator writes `0x64B2E0` at `0x474336`/`0x4744BA`). Rendering never allocates an
order. So a one-step `order_free_head` change can only mean a **command was processed** in the
normal path but not the re-sim. The command source is `replay_loop` (§1). The same holds for the
regular image free list (`unusedImages` head `0x57EB68`, written at `0x4D4E30`/`0x4D6CF4`): units
created by a missed command, and the overlay images they spawn, allocate from it.

This is **simulation state**, not viewer state — it cannot be "excluded". It must be made
reproducible by restoring the input cursor that produced it.

---

## 4. Q2/Q4/Q5 — the boundary is correct; the input is what's missing

- **The hook boundary at `run_game_tick` is already the right place.** The replay injection
  (`replay_loop`) is inside `orig`, and `StepObjects`/triggers are inside `orig`. Moving the hook
  *out* to the outer render function would be wrong: it would force re-simulation to re-run
  `update_cursor`, `receive_network_messages` (which would consume real network/replay input), and
  `H_RenderScreen` (which blits) — none of which are simulation and all of which read wall clock or
  input. Option (i) is rejected.
- **Option (iii) (exclude the fields) is also rejected** for `Sprite.position` and
  `Unit.finder_index`: both are written by the simulation (movement, position-search) and by the
  missed commands, so they are genuine sim state. Excluding them would hide a real divergence.
- **The correct fix is option (ii)/(iv) restated: treat the replay command stream as input and
  capture/restore its position alongside the snapshot.** Concretely, add the replay-input state to
  the snapshot (the minimal change that confirms the diagnosis), and/or re-apply the staged commands
  from the input table in production.

For Q4, the outer per-rendered-frame function begins at `0x004D986D` (prologue `0f b6 05 ec 11 6d 00`
reading `g_game_active 0x6D11EC`), loops with the back-edge `0x4D9945` (`e9 23 ff ff ff` →
`0x4D986D`). It is **not** the boundary we want, and its back-edge NOP is **not** recommended.

---

## 5. Recommended fix (for the shadow gate, now)

Add the replay-input cursors to the snapshot, so `replay_loop` re-injects the same commands after a
restore:

| range | addr | len | kind | evidence |
|---|---|---|---|---|
| `next_replay_command_frame` | `0x5136C8` | 4 | flat | `bwdata.h:303` |
| `replay_end_frame` | `0x6D0F31` | 4 | flat | `bwdata.h:344` |
| `replay_data` (→ `ReplayData` struct) | deref `0x596BBC` | 0x20 | pointer-global (deref like `tiles.rs`) | `bwdata.h:298`; struct `bw_dat` `structs.rs:251-263`, cursor at `+0x1C` |
| `replay_cmd_buffer` | `0x6552B0` | see note | flat | `bwdata.h:1429` |
| `replay_cmd_players` | `0x6554D8` | see note | flat | `bwdata.h:1430` |
| `replay_cmd_sizes` | `0x654AA8` | see note | flat | `bwdata.h:1431` |
| `replay_cmd_count` | `0x6D5BF0` | 4 | flat | `bwdata.h:1432` |

**Buffer sizes (unverified — flag):** `g_replay_cmd_buffer` extends to `g_replay_cmd_players`
(`0x654AA8`→`0x6552B0` is the sizes array, `0x6552B0`→`0x6554D8` = 0x228 bytes buffer,
`0x6554D8`→`0x6556E0` = 0x208 bytes players). The exact live lengths need confirmation from the
buffer-capacity guard in `get_replay_turn_data` (`game_hooks.cpp:5099`, `0x006554B0` is the buffer
end). Snapshot conservatively to `0x6554D8` (0x228 bytes) for the buffer and to `0x6556E0` for the
players/sizes span, or bisect with `CM_ROLLBACK_EXTRA_RANGE` if a narrower bound is wanted.

The minimal *confirmatory* change is just the two cursors — `next_replay_command_frame`
(`0x5136C8`) and the `ReplayData.cur_frame` field via `0x596BBC` (+0x1C) — because restoring them
re-points `replay_loop` at the right frame; the staged buffers are then refilled deterministically.
Add the buffers too if the two-cursor fix leaves residual divergence.

**Production note:** this is a replay-harness-specific fix. A live rollback game has no replay
cursor — its input is the network turn stream held in the rollback engine's input table and
re-applied from there (ShieldBattery's model, `reference/ShieldBattery/game/src/rollback`). The
snapshot of the replay cursor is therefore the *test harness*'s analogue of "capture the input", and
it does not need to ship in the live path.

---

## 6. Q6 — verification

```
tools/gate.sh Release 144
CM_ROLLBACK_SHADOW=1 CM_ROLLBACK_SNAPSHOT_SPACING=1 CM_ROLLBACK_SHADOW_START=95
```

with the replay-input ranges above added. **Pass =** the depth-1/spacing-1 shadow run reports zero
`DIVERGENCE` for `units`, `sprites`, `images`, `orders`, `game`, `rng_seed`, `image_free_head`,
`order_free_head` across frames 95–144 (one `orig` per restored frame reproduces it exactly). If
`rng_seed`/`game` alone remain, that is the already-known plugin-heap path (Layer B aise / ADR-0005
gptp blob); if a pool still diverges, add the staged buffers (§5) and re-run.

A secondary check: temporarily set `g_sync_turn_enabled` (0x6556E0) = 0 so `replay_loop` no-ops;
the shadow run should then diverge on `rng_seed`/`game` only (plugin heap), with the pool ranges
quiet — this isolates the replay-injection mechanism as the source.

---

## Unverified / flagged

- Exact live lengths of `g_replay_cmd_buffer`/`_players`/`_sizes` (§5).
- Whether `ReplayData.cur_frame` is the *only* cursor `get_replay_turn_data` advances, or whether it
  also mutates `last_frame_time`/`end` (`bw_dat` `structs.rs:258-262`); snapshotting the whole
  0x20-byte `ReplayData` covers it either way.
- `0x41CA00` (`H_RenderScreen`) interior was not fully walked; it is assumed (and is strongly
  indicated by the `order_free_head` evidence) to write only render fields on *active* images. Any
  residual active-image render-field divergence after the replay fix is a separate, excludable
  concern (§4 of the previous doc).
