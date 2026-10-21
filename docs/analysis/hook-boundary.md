# Hook boundary for a deterministic 1.16.1 logic frame

**Status:** design proposal (supersedes the `0x004D9670`-entry assumption in ADR-0003 open Q1 and
corrects ADR-0005 §4.3).

**Grounding:** the shipped stock `StarCraft.exe` (base `0x00400000`, non-ASLR) disassembled with
`objdump -d`; `reference/cosmonarchy-gptp/BW/bwhelper.h`, `BW/bwdata.h`,
`hooks/game_hooks_inject.cpp`, `hooks/game_hooks.cpp`; and
`crates/cosmo-rollback/src/{frame_hook.rs,rollback/mod.rs}`.

---

## 1. The problem, restated precisely

The rollback hook wraps `0x004D9670` (`bwhelper.h:48`: `run_game_tick(b32 replay_step_only)`),
treating "one `orig()` call" as one logic frame. But `run_game_tick` is a **wall-clock-paced
catch-up loop**, not one frame. One call runs `K` logic frames, where

```
K = min([0x5124D4],  number of iterations before `now - g_next_tick_ms >= 0` fails)
```

(`0x4d969f` loads the budget `[0x5124D4]`; the loop head is `0x4d96a7`; the wall-clock exit is
`0x4d97cb`–`0x4d97fd`.) `K` therefore depends on real time, so a re-simulation that runs
`orig()` `steps = f - g` times advances a **different number of logic frames** than the normal
path. That is the whole remaining symptom: the free-list heads are now correctly restored, but the
re-sim threads the free lists a different number of times, so the free-tail bytes of `units`,
`images`, `sprites`, `orders`, `bullets` still diverge. ADR-0003 open Q1 ("is `hook_step_objects`
sufficient?") is answered **no**, and the `0x004D9670` entry is also wrong as a frame boundary.

---

## 2. Annotated disassembly of `run_game_tick` (`0x004D9670`)

One iteration of the inner loop (loop head `0x4d96a7`, back-edge `0x4d9823`) is the **deterministic
logic frame**. Everything else is scheduling.

| addr | instruction | meaning | sim? |
|---|---|---|---|
| `0x4d9676` | `mov [ebp-0x1c],ecx` | arg `replay_step_only` | — |
| `0x4d9680` | `call *[0x4fe0c4]`; `add eax,0x7d0` | `now` + 2000 ms deadline | wall clock |
| `0x4d9695` | `mov [0x6d11f0],0` | return-code global | no (transient) |
| `0x4d969f` | `mov eax,[0x5124d4]` | per-call frame budget | wall clock |
| `0x4d96a7` | **loop head** | | |
| `0x4d96ad`/`0x4d96b3` | `dec [ebp-0x10]`; `test` | budget--; exit if 0 | wall clock |
| `0x4d96bb`/`0x4d96c4` | `cmp [0x6d0f14],0`; `call 0x487100` | replay branch (gptp patches `0x4d96c4`→`replay_loop`) | replay pacing |
| `0x4d96cc` | `call 0x4d9550` | turn scheduler (see §5) | **yes** |
| `0x4d96d1` | `test eax,eax`; `je` exit | turn not ready → stall | **yes** |
| `0x4d96e4`–`0x4d9725` | game-active / replay-pause checks | | mostly replay pacing |
| `0x4d973e` | `or [0x6cefb5],2` | set "world changed" flag | viewer |
| `0x4d974e` | `call 0x488780` | **gptp patches this → `next_frame_hook`** (see §7) | **yes** |
| `0x4d9753` | `test eax,eax` | branch on `is_game_paused` | **yes** |
| `0x4d9757` | `call 0x4d9460` | paused visual update (see §6) | render-ish |
| `0x4d975e`–`0x4d9766` | `inc [0x57f23c]` | `Game.frame_count++` | **yes** |
| `0x4d977b` | `call 0x4d94b0` | `StepObjects` | **yes** |
| `0x4d9780`–`0x4d9785` | `call 0x4dc540` (`ecx=1`) | `rng_enabled = 1` | **yes** |
| `0x4d978a`–`0x4d9798` | `push [0x5124d8 + 0x6cdfd4*4]`; `call 0x48a460` | `execute_all_game_triggers(ms_per_frame)` | **yes** |
| `0x4d979d`–`0x4d979f` | `call 0x4dc540` (`ecx=0`) | `rng_enabled = 0` | **yes** |
| `0x4d97b2`–`0x4d97c5` | `[0x51ce94] += [0x5124d8 + speed*4]` | `g_next_tick_ms += frame_duration` | wall clock |
| `0x4d97cb`–`0x4d97fd` | `call *[0x4fe0c4]`; compare | catch-up / deadline exits | wall clock |
| `0x4d9823` | `jmp 0x4d96a7` | **back-edge** | — |
| `0x4d9828` | `mov [0x51ce6c],edx`; ret | return frames-done | no |

So the deterministic unit is exactly:

```
turn_scheduler();                        // 0x4d9550, decrements g_turn_ticks_left
if (is_game_paused)  paused_update();    // 0x4d9460  (see §6)
else {  Game.frame_count++;  StepObjects(); }
rng_enabled = 1;  execute_all_game_triggers(ms_per_frame);  rng_enabled = 0;
```

---

## 3. Options evaluated

### (a) Hook `StepObjects` (`0x4D94B0`) and replicate the surrounding glue

Would need to manually reproduce, in the right order and with the right args:
`inc [0x57f23c]` (frame_count), the `is_game_paused` branch, the `rng_enabled` toggle around
`execute_all_game_triggers(ms_per_frame = g_game_speed_replay_modifiers[g_options_list])`. It also
**misses gptp's `next_frame_hook`** at `0x4D974E` (which is *outside* `StepObjects` and runs before
it), so the plugins' per-frame work would not be re-run at the right point.

- **Verdict:** rejected. Too much exact re-implementation of engine ordering; fragile against gptp's
  patches and future plugin hooks.

### (b) Mid-function trampoline + patched back-edge (the loop body becomes callable)

Patch the back-edge `0x4D9823` so it no longer loops; keep the hook at `0x4D9670`. One
`orig()` call then executes **exactly one loop iteration** and returns. The frame body
(`next_frame_hook` → `frame_count++` → `StepObjects` → triggers) runs verbatim inside `orig`, in
the engine's own order, with the engine's own args. No replication. gptp's `next_frame_hook` and
`replay_loop` stay inside `orig` and are re-run correctly.

- **Verdict:** **recommended** (see §4).

### (c) Hook `0x4D9550` (the turn scheduler)

`0x4d9550` is only the lockstep turn check (decrements `g_turn_ticks_left`, tests
`0x487070` for a ready turn). Hooking it does not give a logic frame; it is one sub-step of it.

- **Verdict:** rejected as a frame boundary; it is just one callee.

### (d) A cleaner existing function

There is no existing single function that is "one logic frame": `StepObjects` (`0x4D94B0`) excludes
the trigger step and `frame_count`; `execute_all_game_triggers` (`0x48a460`) excludes unit logic;
`run_game_tick` (`0x4D9670`) runs `K` frames. The loop body is the unit, so (b) is the minimal
manufactured boundary.

---

## 4. Recommended implementation

**Patch the back-edge and keep the hook at `0x4D9670`.**

- At `0x004D9823`, the instruction is `E9 7F FE FF FF` (`jmp 0x4d96a7`). Replace the 5 bytes with
  `90 90 90 90 90` (5× NOP). Control then falls through to the epilogue at `0x4d9828`
  (`mov [0x51ce6c],edx; leave; ret`), so one `run_game_tick` call runs **one** logic frame.
- No change to `frame_hook.rs`'s install address (`0x4D9670`). `orig(ecx)` now means "advance one
  logic frame" and the existing re-sim loop `for _ in 0..steps { orig(ecx) }` runs `steps` logic
  frames deterministically.
- The snapshot is taken at the top of `on_step`, i.e. at the `0x4D9670` entry, **before** the single
  logic frame. This is quiescent (gptp's `next_frame()` has not run for this frame yet — it runs at
  `0x4D974E`, inside the body). Capture/restore here is correct.

**Consequence — catch-up is removed.** The normal path now advances exactly one logic frame per
`run_game_tick` call, paced by the outer loop's time gate at `0x4D98F6`
(`now - g_next_tick_ms; js skip`). This is what a deterministic lockstep/rollback game *wants*: the
`[0x5124D4]` burst (catching up several frames after a stall) is the very non-determinism we are
removing. After a real stall, the turn scheduler's own resync path (`0x4d95d7`–`0x4d95f4`) re-aligns
`g_next_tick_ms`, so no burst is needed.

**Why keep the hook at `0x4D9670` rather than move it into the loop:** the loop body is entered from
the prologue *and* from the back-edge; hooking the loop head would intercept both and would still
need the back-edge fixed. Patching the back-edge while hooking the entry keeps `orig`'s contract
("one call = one frame") and leaves the prologue (budget/deadline setup) running exactly as the
engine wrote it.

---

## 5. The `0x488780` branch

`0x488780` is a one-instruction getter: `mov eax,[0x6509c4]; ret` — it returns
`g_is_game_paused` (`bwdata.h:1045`, `0x006509C4`). It reads **no wall clock, no network, no input**.
The branch in `run_game_tick` (`0x4d9753`) is therefore deterministic: it depends only on
`g_is_game_paused`, which is snapshotted (`is_paused` in `ranges.rs`).

- **Not paused** (`eax==0`) → `0x4d975e`: `frame_count++`, `StepObjects`. This is the path the
  headless replay takes.
- **Paused** (`eax!=0`) → `0x4d9757`: `0x4d9460`, which walks `first_active_unit` (`0x628430`) and,
  for units whose sprite isn't hidden/flashing, calls `0x4ebbd0` with a selection-flash color. It
  then still runs the trigger step at `0x4d9780`.

**Rollback handling:** because the branch is on snapshotted state, `orig` reproduces it exactly. The
only thing to verify is that `0x4ebbd0` (the paused path) writes only viewer/selection-flash state —
it does not affect the deterministic replay path (not paused). Flag: `0x4ebbd0` not individually
disassembled; irrelevant unless a pause frame is re-simulated.

---

## 6. The render/update pass — does it write the diverging Image/Sprite fields?

The outer render loop (`0x4d986d`–`0x4d9945`) calls, per rendered frame, in order:
`0x4d1bf0` (pump messages) → `0x4d14d0` (update cursor) → `0x47f0e0` (scroll) → `0x485f70`
(receive network/replay) → `0x47d160` (ret stub) → `0x47f590` (bnet league query) →
`run_game_tick` (time-gated) → `0x4d93f0` → `0x41ca00` (`H_RenderScreen`, gated on redraw).

- **`0x4d93f0` is a UI/HUD pass, not a pool pass.** It calls `0x4c38b0` (selection-circle reset),
  `0x45ebc0`, `0x4599a0`, `0x458120`, `0x48ddc0` (status screen / buttonset / portrait update),
  then tail-jumps to `0x48cc70` (screen-text layout). These mutate dialogs/UI, **not** the
  `Image`/`Sprite` pools. It reads wall clock (`0x4d9434`, an autosave timer) and must **not** be in
  the re-simulated step.
- **The diverging Image/Sprite fields are render fields written by the draw phase** (`0x41ca00`
  `H_RenderScreen` and the sprite/image draw functions, which run *after* `run_game_tick`):
  - `Image +0x0C` `flags` (incl. `Redraw`) — cleared by draw, set by iscript/`StepObjects`;
  - `Image +0x1C` `map_position` — cached world position (written by `StepObjects`/image update,
    read by draw);
  - `Image +0x20` `screen_position` — **render-only** (camera-relative projection);
  - `Image +0x28` `grp_bounds` — **render-only** (GRP frame bounds);
  - `Sprite +0x14` `position` — map-space position, written by `StepObjects` (movement).

**Do these need to be inside the re-sim step?** No. The observed divergence is at **high
(free-tail) indices** (`images` ≈ index 15733/15734, `sprites` ≈ 8710, `units` ≈ 4045) — free,
un-drawn entries. The draw phase never touches free entries, so those bytes differ only because the
**free list was threaded differently** (the `K`-frames problem), not because the draw pass was
skipped. Fixing the boundary (§4) removes this.

**Separately, the snapshot should stop covering render fields.** Today `pools.rs::add_ranges` adds
the *whole* `Image` and `Sprite` pools as raw ranges, so the snapshot copies `screen_position` and
`grp_bounds` too. `image.rs` already snapshots only the iscript fields (`0x0B`–`0x1B`) and is the
right model: for the live/active image case the render fields must be **excluded** (or the shadow
gate will flag them as a false divergence, since the draw phase legitimately is not re-run). Concrete
exclusions: `Image +0x20/+0x22` (`screen_position`), `Image +0x28/+0x2A` (`grp_bounds`), and the
`Redraw` bit of `Image +0x0C`; keep `Image` prev/next (`0x00/0x04`, free-list links), `map_position`
(`0x1C`), and the iscript fields. `Sprite +0x14` (`position`) is sim state — keep it.

`H_RenderScreen` (`0x41ca00`) must never be re-run for real: it blits. It stays outside `orig`.

---

## 7. Per-frame counters and globals — in or out of the deterministic step

| global | addr | evidence | inside the step? | snapshot? |
|---|---|---|---|---|
| `Game.frame_count` | `0x57F23C` | `0x4d975e`; `bwdata.h:218` | yes (inc at `0x4d9763`) | yes (in `game` range) |
| `g_turn_ticks_left` | `0x51CEA0` | `0x4d9560`; `bwdata.h:349` | yes (dec in `0x4d9550`) | **yes — add if missing** |
| `g_turn_stalled` | `0x51CE9D` | `0x4d9599`; `bwdata.h:348` | yes (set in stall path) | yes (borderline; deterministic given turn schedule) |
| `g_vision_update_count` | `0x51CE98` | `bwdata.h:472` | yes (dec in `StepObjects`) | yes (already `frame_cooldown`) |
| `g_vision_updated` | `0x51CE9C` | `bwdata.h:473` | yes | yes |
| `rng_enabled` | `0x6D11C8` | `0x4dc540` | yes (toggled around triggers) | yes (already) |
| `g_next_tick_ms` | `0x51CE94` | `bwdata.h:347`; `0x4d97b2` | written by pacing | **no** (wall-clock) |
| `g_6D11F0` | `0x6D11F0` | `0x4d9695` | written (return code) | **no** (transient) |
| `g_51CE6C` | `0x51CE6C` | `0x4d9828` | written (frames-done) | **no** (return value) |
| `g_51CE8C` | `0x51CE8C` | `bwdata.h:350`; `0x4d98c2` | no (scroll) | no (viewer) |
| `g_6CEFB5` | `0x6CEFB5` | `0x4d973e` | set flag 0x2 | **verify** — likely "world changed/redraw" (viewer); exclude unless audit says otherwise |
| `g_6D6370`/`g_6D6374` | `0x6D6370`/`0x6D6374` | `0x4d95a6`/`0x4d943a` | wall-clock stall/autosave timestamps | no |

`frame_count`, `g_turn_ticks_left`, `g_vision_*`, `rng_enabled` advance *inside* the single-frame
body and are re-run by `orig`; they must be in the snapshot. `g_next_tick_ms`, `g_6D11F0`,
`g_51CE6C`, and the wall-clock timestamps are scheduling/return values and must be **excluded** —
restoring them would fight the pacing. `g_turn_ticks_left` is the one I specifically flag as likely
**missing** from the current table (it was not in the first-cut `ranges.rs`); add it.

---

## 8. Plugin consistency (gptp `next_frame`, and a correction to ADR-0005 §4.3)

`call_patch(next_frame_hook, 0x004D974E, 0)` (`game_hooks_inject.cpp:332`) redirects the
`call 0x488780` at `0x4D974E` — which is **inside the loop body**, immediately before the
`frame_count++`/`StepObjects` path. `next_frame_hook` runs `hooks::next_frame()`
(`game_hooks.cpp:799`: lua `g_game->tick()`, `update_trigger_system(true)`,
`g_ai_layouts.update()`, `update_custom_ai()`, `update_actions()`, `*trigger_cooldown=0`, etc.), then
returns `is_game_paused()` so the branch's meaning is preserved.

**This corrects ADR-0005 §4.3**, which stated "the re-sim re-runs `orig` (0x004D9670) but **not**
`next_frame()`". In fact `next_frame()` is at `0x4D974E`, *inside* `orig`, so it **is** re-run once
per logic frame — which is exactly what correctness requires (gptp's trigger/AI-layout/action state
is re-applied). The same applies to `replay_loop` (`call_patch` at `0x4D96C4`,
`game_hooks_inject.cpp:344`), also inside the loop body.

- With the §4 boundary, `next_frame()` runs once per `orig()` call, at the same point relative to
  `StepObjects`/triggers as the normal path. No plugin change is needed.
- **Snapshot quiescence:** at the `0x4D9670` entry, `next_frame()` has not yet run for this frame, so
  the gptp blob capture (ADR-0005 §4.2) at that point is consistent with the engine ranges.

---

## 9. Risks and verification

Risks:
1. **Losing the burst catch-up** changes normal-play pacing after a stall. Mitigated by the turn
   scheduler's resync path; the rollback engine owns pacing in production anyway. Verify no softlock
   after a forced stall in the headless gate.
2. **`g_turn_ticks_left` / `g_turn_stalled` still missing from the snapshot** would keep the turn
   scheduler divergent across a restore. Add them and re-measure.
3. **The back-edge NOP is permanent** (no conditional re-enable). If the normal render loop needs the
   catch-up for menus/loading, the first-frame-after-load may advance slower than stock. Acceptable
   for rollback; confirm the replay reaches gameplay at the same wall time.
4. **`0x4ebbd0` (paused path) unverified.** Only matters if a paused frame is re-simulated.

**Single confirmatory experiment:** with the `0x4D9823` back-edge NOP applied and
`g_turn_ticks_left` (`0x51CEA0`) + `g_turn_stalled` (`0x51CE9D`) added to `ranges.rs`, run the
existing headless shadow gate (`tools/gate.sh`, `CM_ROLLBACK_SHADOW=3`, frames 90–138). Success =
`DIVERGENCE` falls to zero for `units`, `images`, `sprites`, `orders`, `bullets`, `sprite_count`,
`order_free_head`, `image_free_head`, and `rng_seed`; the only remaining reports, if any, should be
render-field bytes, which are then excluded per §6 rather than "fixed". If `game`/`minerals` still
diverges, that is the known plugin-heap path (Layer B aise / ADR-0005 blob), not a boundary defect.

---

## Unverified / flagged

- `0x4ebbd0` (paused-update callee) — not disassembled; paused path only.
- `g_6CEFB5` (`0x6CEFB5`) — set to `|2` each logic frame at `0x4d973e`; suspected viewer redraw
  flag, but not traced to its reader. Exclude from the snapshot unless the audit names it.
- `0x48a460` (`execute_all_game_triggers`) body — confirmed deterministic at its entry; the interior
  trigger step (`0x489cc0`) was disassembled and reads only snapshotted state (no wall clock).
- Whether the patched `Cosmonarchy BW.exe` runtime copy differs at any of these addresses (the CCMU
  extender relocates *data* operands, not these *code* sites) — the shadow gate is the arbiter.
