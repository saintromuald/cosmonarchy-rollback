# The 1.16.1 network-turn path (for rollback capture / injection)

**Status:** design proposal.
**Grounding:** shipped `StarCraft.exe` (base `0x400000`) and `storm.dll` (base `0x15000000`)
disassembled with `objdump -d` / `tools/dis161.sh`; gptp `BW/bwdata.h`, `storm/storm.h`,
`hooks/game_hooks.cpp`; CrownLink `src/snp/SNPModule.cpp`; `crates/cosmo-rollback/src/{turns.rs,tick.rs,replay.rs,rollback/mod.rs}`.

---

## 0. The lockstep model in one paragraph

1.16.1 is classic deterministic lockstep: the local player's commands accumulate in a turn buffer
during a frame; every `g_turn_ticks_left` (0x0051CEA0) frames the engine flushes that buffer as the
local **turn** and hands it to storm (which gives it to CrownLink); peers' turns arrive on the storm
receive queue; when every player's turn for a turn-slot is present, the engine processes all of them
in player order and runs the corresponding logic frames. If a peer's turn has not arrived, the
engine **stalls** (`g_turn_stalled`) instead of running. This stall and the "turn = N frames"
mapping are the two things rollback must override.

---

## 0.1 Empirical update (2026-10-18): the cadence in the replay harness

The `TurnScheduler` trampoline on `0x004D9550` was instrumented (log `ticks_in` =
`g_turn_ticks_left` on entry, the return value, and `g_sequence` 0x006556E8) and the headless
replay probe run with and without `CM_ROLLBACK_FORCE_TURN=1`:

```
CM_ROLLBACK_SHADOW=1 CM_ROLLBACK_SNAPSHOT_SPACING=1 CM_ROLLBACK_SHADOW_START=95 \
CM_ROLLBACK_NO_PLUGIN=1 bash tools/headless_probe.sh Release \
  "490,182;940,590;320,645;350,238;940,590" "" /tmp/opencode/turn.png 55 60
```

Result, **identical in both runs**:

```
turn scheduler #0:   ticks_in=1 stalled=0 ret=1 frame_count=0   sequence=1
turn scheduler #1:   ticks_in=1 stalled=0 ret=1 frame_count=1   sequence=1
...
turn scheduler #256: ticks_in=1 stalled=0 ret=1 frame_count=175 sequence=1
```

So in this replay at its locked speed:

- `g_turn_ticks_left` is **already 1** on entry on every call — the scheduler is at a turn boundary
  every frame, i.e. **one frame = one turn**;
- the scheduler **never returns 0** — the stall path is **not taken at all** in a replay (there are
  no peers to wait for);
- `g_sequence` stays 1 (no command is processed on this path).

Consequences:

- `CM_ROLLBACK_FORCE_TURN` is a **no-op in the replay harness** (it sets `g_turn_ticks_left = 1`,
  which is already true, and clears a stall that never happens). It is only meaningful against a
  real peer, or when the stall branch is forced.
- §7's "single most important open design point" (turn ≠ frame) **does not bite in this
  configuration**: the input table's 1 turn = 1 step assumption holds here. It remains unverified
  for other game speeds (`g_51CE70[speed]`) and for a live networked game.
- The **stall-suppression** half of the confirmatory experiment is **not testable in the replay
  harness**; the **injection** half would double-apply (the replay injector `0x00487100` already
  supplies turns), so a replay is not a valid vehicle for injection either. Validating capture +
  inject + suppress needs either a live/2-client game or a harness change that disables the replay
  injector and drives the replay from the input table.

---

## 1. How a turn is produced and consumed (Q1)

### Production (local → wire)

| step | function | evidence |
|---|---|---|
| accumulate local commands | `0x00485BD0` `send_command(ecx=data, edx=size)` | appends to `g_turn_buffer`; `rep movsl` at `0x485c4f` (`lea 0x654880(%eax),%edi`) |
| turn buffer | `g_turn_buffer` `0x00654880` (`bwdata.h:232`) | the command blob; first byte `0x05` = NOP |
| buffer size | `g_command_queue_size` `0x00654AA0` (`bwdata.h:231`) | incremented at `0x485c60` |
| flush (send) | `0x00485a40` | if empty writes NOP `0x05` at `0x485a4e`; `push size; push 0x654880; call 0x410202`; resets size to 0 at `0x485a8b` |
| SNetSendTurn thunk | `0x00410202` → `*[0x4fe478]` = **storm ordinal 128** | send call site `0x485a60`/`0x485b25` |

> **Two flush sites, only one is per-turn (corrected 2026-10-18).** The **per-turn** flush is
> inlined in the turn-pipeline fill `0x00485AA0` (`0x00485B13` writes the `0x05` empty marker,
> `0x00485B25` calls `SNetSendTurn`); `0x00485A40` is called **only** from `0x00485C3D` inside
> `send_command` — the mid-frame **overflow** flush. See §3.

A **turn** is therefore the byte blob at `g_turn_buffer` of length `g_command_queue_size`: a
sequence of commands, each `id` byte followed by `command_lengths[id]` payload bytes (sizes in
`g_turn_command_size` / `command_lengths` `0x005005F8`, `[u32; 0x60]`, `bwdata.h:299`). The 12-byte
`GamePacketHeader` (`checksum,size,sequence,ack_sequence,type,sub_type,player_id,flags`) is added by
storm/SNP around this payload; CrownLink sees the full `GamePacketData`
(`SNPModule.cpp:222-228`).

### Consumption (wire → simulation)

| step | function | evidence |
|---|---|---|
| receive | `0x00485F70` `receive_network_messages` | outer-loop call `0x4d98e7`; `call 0x4101f0` at `0x485f8c` |
| SNetReceiveTurn thunk | `0x004101f0` → `*[0x4fe46c]` = **storm ordinal 121** | returns (count, data, player); dispatch table `0x50439c` indexed by message type |
| message handlers | type 0 → `0x00485f50`, type 1 → `0x00485f30`, type 2 → `0x0047d170` (ret 0) | `0x485fb0 call *[0x50439c(,%eax,4)]`; 0/1 both tail-call `0x485d10` |
| per-player turn store | `0x00654A80` (ptr) / `0x006554B4` (size), `[player*4]` | read by `0x486f50` at `0x486f99`/`0x486fa0` |
| process all turns | `0x00486f50` | loops players 0..7; for each with flag bit `0x20000` in `0x57f0b4`, sets `command_user`/`unique_command_user`/`storm_command_user`, `g_sequence=0`, then `call 0x4865d0` |
| command dispatcher | `0x004865d0` `ProcessCommands`/`game_recv` | `eax=command data`, `[ebp+8]=size`, `[ebp+0xc]=flag`; looks up `command_lengths` at `0x48661c`; gptp patches it (`game_hooks_inject.cpp:327`) |
| turn-pipeline fill | `0x00485aa0` | `call 0x410274` (SNetGetTurnsInTransit) then loops `SNetSendTurn` to fill `g_provider_turns_in_transit`+`g_additional_turns_in_transit` |
| SNetGetTurnsInTransit thunk | `0x00410274` → `*[0x4fe4c4]` = **storm ordinal 115** | `storm.h:40 SNetGetTurnsInTransit` |

The orchestrator is `0x00487070` (unnamed in `bwhelper.h`; call it `step_network`), called from the
turn scheduler at `0x4d957b`. It runs `0x486580` (timeout/avail check) → `0x485aa0` (fill/send) →
`0x486f50` (process) and returns whether the next turn-slot is ready:

```
0x48707f  call 0x486580    ; timeout check, sets 0x57ee78
0x4870a7  call 0x485aa0    ; fill turn pipeline (sends local turn ahead)
0x4870bd  call 0x486f50    ; process received turns (sync path)
0x4870c4  call 0x486530    ; process received turns (non-sync path)
```

### Turn scheduler (decides run vs stall)

`0x004D9550` (called from `run_game_tick` at `0x4d96cc`):

```
0x4d9560  dec [0x51cea0]       ; g_turn_ticks_left--
0x4d956f  je 0x4d957b          ; 0 -> turn boundary
0x4d9571  mov eax,1; ret       ; not a boundary: run frame
0x4d957b  call 0x487070        ; step_network -> eax = next slot ready?
0x4d9582  jne 0x4d95b2         ; ready -> proceed
0x4d9584  mov [0x51cea0],1     ; re-arm
0x4d9599  movb [0x51ce9d],1    ; g_turn_stalled = 1
0x4d95ab  xor eax,eax; ret     ; 0 -> run_game_tick exits, frame NOT run
0x4d95b2  ...                  ; ready: reset g_turn_ticks_left from g_51CE70[speed]
0x4d9645  mov [out],1          ; mark turn-ready (checked at 0x4d97ff)
```

**Globals, with addresses (Q1 inventory):**

| name | addr | touches | sim or transport |
|---|---|---|---|
| `g_turn_buffer` | `0x00654880` | `0x485c4f` write | transport (outgoing turn) |
| `g_command_queue_size` | `0x00654AA0` | `0x485c60`/`0x485a8b` | transport |
| `g_turn_ticks_left` | `0x0051CEA0` | `0x4d9560` | **sim** (turn cadence) |
| `g_turn_stalled` | `0x0051CE9D` | `0x4d9599` | **sim** (stall flag) |
| `g_next_tick_ms` | `0x0051CE94` | `0x4d97b2` | wall-clock pacing (exclude) |
| `g_sync_turn_enabled` | `0x006556E0` | `0x485a84`/`0x487088` | transport/sync gate |
| `g_provider_turns_in_transit` | `0x0057F090` | `0x485ae7` | transport |
| `g_additional_turns_in_transit` | `0x006556E4` | `0x485aef` | transport |
| `g_sequence` | `0x006556E8` | `0x48666d` inc | **sim** (per-turn command seq) |
| `command_lengths` (`g_turn_command_size`) | `0x005005F8` | `0x48661c` read | static (read-only) |
| `command_user` / `unique_command_user` / `storm_command_user` | `0x00512678` / `0x0051267C` / `0x00512680` | `0x486f93`/`0x486f7e` | transport scratch (player context) |
| per-player turn ptr / size | `0x00654A80` / `0x006554B4` | `0x486f99`/`0x486fa0` | transport (received turns) |
| per-player avail flags | `0x0057F0B4` | `0x486f70` test bit `0x20000` | transport |
| per-player cmd ctx | `0x0057EEC0` / `0x0057EE7C` | `0x486f85`/`0x486f8c` | transport |

---

## 2. The transport boundary (Q2)

The engine never talks to CrownLink directly. It calls storm.dll via import thunks at `0x41xxxx`:

| engine thunk | storm ordinal | storm fn | CrownLink SPI slot (`NetFunctions`) |
|---|---|---|---|
| `0x00410202` | **128** | `SNetSendTurn` | `send` (index 14) `SNPModule.cpp:369` |
| `0x004101f0` | **121** | `SNetReceiveTurn` | `receive` (index 11) `SNPModule.cpp:366` |
| `0x00410274` | **115** | `SNetGetTurnsInTransit` | *(storm-local, no SPI call)* |

CrownLink's `spi_send(u32 count, NetAddress** list, char* data, u32 size)` (`SNPModule.cpp:211`)
and `spi_receive(NetAddress** peer, GamePacketData** out, u32* size)` (`SNPModule.cpp:233`) are the
two functions rollback's transport seam must sit *beside* (ADR-0001 §2.1's `cl_rollback_*` exports),
**not** replace — the engine still uses them for its own lockstep, which rollback will bypass.

> **Ordinal caveat (flagged):** the *names* 128/121/115 = SendTurn/ReceiveTurn/GetTurnsInTransit
> are inferred from the call signatures and call sites (send passes `(buffer,size)`, receive returns
> `(count,data,player)`, `0x410274` returns a count into `&out`). The numeric→name mapping is
> otherwise unverified; the numeric ordinals and thunk addresses are ground truth.

---

## 3. Capture point (Q3)

Read the **local turn** at the turn boundary, *before* the flush resets the buffer:

- payload: `g_turn_buffer` `0x00654880`
- length: `g_command_queue_size` `0x00654AA0`

Both are only ever `send_command`-appended and flush-reset, so between two boundaries the blob
is stable for the frame it belongs to.

**The capture point is `0x00485AA0` (the turn-pipeline fill) on entry — not `0x00485A40`.**
*(Corrected 2026-10-18: this section previously named `0x00485A40`.)* The per-turn flush is
**inlined** inside `0x00485AA0`: it reads `g_command_queue_size` (`0x00485AFE`), and if it is
zero writes the empty-turn marker `0x05` to `g_turn_buffer` and sets the size to 1
(`0x00485B13`/`0x00485B1A`), then calls `SNetSendTurn` (`0x00410202`) with
`(0x00654880, size)` at `0x00485B25`. By contrast `0x00485A40` is called from **exactly one
place**, `0x00485C3D` inside `send_command` (`0x00485BD0`), i.e. only the mid-frame
**overflow** flush — hooking it would capture almost nothing. `0x00485AA0` has five call
sites but all are network-pipeline sites, and the capture is a pure read of `.data`, so
hooking it is safe; the one that matters is the turn boundary via `step_network`
(`0x00487070`). See `docs/analysis/turn-capture-and-inject.md`.

*(The empty-turn marker is `GameCommand_Sequence = 0x05`, not ShieldBattery's SC:R `NOP`
`0x0F`: `BW/enumerations.h:1353`, defaulted by `BW/structures.h:1758`.)*

**Snapshot coverage today:** `command_users` (0x512678, len 16) is snapshotted; `command_lengths`
(0x5005F8) is **not**, `g_turn_buffer`/`g_command_queue_size` are **not**, the per-player turn
buffers are **not**. This is *correct for the sim snapshot* — the turn buffer is transport/input
state, held in the input table, not the state snapshot (see §6). The one sim-side gap is
`g_turn_ticks_left`/`g_turn_stalled`, which **are** snapshot-worthy and were added in the prior work.

---

## 4. Injection point (Q4)

The callable injection primitive is the same one gptp's `replay_loop` uses (`game_hooks.cpp:5153-5169`):

```
// per command in the turn, with the player context set:
*g_active_nation_id = player_id;     // 0x00512678
g_active_player_id  = player_id;     // 0x0051267C
*g_sequence = 0;                     // 0x006556E8
helper::game_recv(&buffer[cur], cmd_size, true);   // = 0x004865D0
```

`game_recv` / `ProcessCommands` (`0x004865D0`) signature (from `samase_shim/src/bw.rs:122` +
disasm `0x4865d5` `mov esi,eax` / `0x4865d7` `mov eax,[ebp+8]`):

```
ProcessCommands(eax = command bytes*, [esp+4] = size, [esp+8] = replay_flag)
```

So to inject one player's turn for a frame: set the three player-context globals, then call
`ProcessCommands(turn_bytes, turn_len, false)` for that player's blob. For the local slot during a
re-sim, re-inject the captured local turn the same way. The replay injector `0x00487100` is a
*usable model* but is replay-specific (it reads `ReplayData::cur_frame` via `0x004CDFF0`
`get_replay_turn_data`); for rollback you bypass it and call `ProcessCommands` directly.

> The engine's own receive path (`0x486f50`) also reads the per-player buffers `0x654A80`/`0x6554B4`;
> populating those and letting `0x486f50` run is an alternative injection surface, but calling
> `ProcessCommands` directly is simpler and matches the replay path exactly.

---

## 5. Suppressing the wait (Q5)

The stall is a single `return 0` in the turn scheduler when `step_network` says "next slot not
ready" (`0x4d9584`–`0x4d95ad`, setting `g_turn_stalled=1`). Options:

1. **Hook/trampoline `0x004D9550`** (recommended): when the rollback engine is predicting, force the
   "ready" path (skip the `0x487070` result check, jump to `0x4d95b2`), so `run_game_tick` always
   runs the frame. This is the cleanest single point; it also lets us leave `g_sync_turn_enabled` set
   for the native sync checksum if desired.
2. **Set `g_sync_turn_enabled` (`0x006556E0`) = 0**: disables the sync checksum path (`0x4870b4`
   branches to `0x486530` instead of `0x486f50`), but does **not** by itself suppress the stall — the
   stall is in `0x4d9550`, upstream of that flag. It must be combined with (1).
3. **Pre-inject empty turns** for the missing slot before the scheduler runs, so `step_network` finds
   them "ready" naturally. More stateful; not recommended for v1.

Recommendation: **(1)**, with `g_sync_turn_enabled` optionally zeroed (ShieldBattery strips the
native 0x37 sync and substitutes NOP; here the analogue is zeroing the sync checksum path).

---

## 6. What must be snapshotted (Q6)

Split transport (exclude) from simulation (include):

| range | addr | len | include? | reason |
|---|---|---|---|---|
| `g_turn_buffer` | `0x654880` | `0x220` | **no** | outgoing turn = input, held in the input table |
| `g_command_queue_size` | `0x654AA0` | 4 | **no** | transport scratch |
| per-player turn ptr/size | `0x654A80` / `0x6554B4` | 8×4 each | **no** | received turns = input |
| per-player avail flags | `0x57F0B4` | 8×4 | **no** | transport |
| per-player cmd ctx | `0x57EEC0` / `0x57EE7C` | 8×4 each | **no** | transport |
| `g_provider_turns_in_transit` | `0x57F090` | 4 | **no** | transport |
| `g_additional_turns_in_transit` | `0x6556E4` | 4 | **no** | transport |
| `g_sync_turn_enabled` | `0x6556E0` | 4 | **no** | gate (constant per session) |
| `g_next_tick_ms` | `0x51CE94` | 4 | **no** | wall clock |
| `g_turn_ticks_left` | `0x51CEA0` | 4 | **yes** | turn cadence — already added |
| `g_turn_stalled` | `0x51CE9D` | 1 | **yes** | stall state — already added |
| `g_sequence` | `0x6556E8` | 4 | **yes** | per-turn command sequence — **add if missing** |
| `command_lengths` | `0x5005F8` | `0x60×4` | **no** (static) | read-only dat table |

**Uncertain sizes (flagged):** `g_turn_buffer` capacity (bounded by `g_provider_max_buffer`
`0x57F0D8`, and the `0x485bd0`/`0x485a5b` send sites cap at `0x220`); the per-player turn buffers
`0x654A80`/`0x6554B4` are 8 entries each (8×4). These are transport-only, so their sizes only
matter for the *input table's* capture, not the state snapshot.

---

## 7. Minimal v1 design (1v1, one remote peer) (Q7)

Hook points (additions to the existing `run_game_tick` hook at `0x4D9670`):

1. **`0x004D9550` trampoline (stall-suppress).** On entry, if `rollback.predicting` is set, jump
   straight to the "ready" branch `0x4d95b2` (skip `step_network`), so the frame always runs.
2. **`0x00485a40` hook (local-turn capture).** On entry, read `g_turn_buffer`[0..`g_command_queue_size`]
   and push it into `InputTable::push(local_slot, bytes, now)`; let `orig` run (the turn still sends,
   or we NOP the `SNetSendTurn` call to stop it leaving — see below).
3. **Inject remote turn before the step.** In the `run_game_tick` hook, after restore (and on every
   re-sim step), for the remote slot call `ProcessCommands(remote_bytes, remote_len, false)` with the
   player context set; for the local slot re-inject the captured local turn identically.

Concrete per-step sequence (inside the `run_game_tick` closure, one call = one logic frame):

```
dispatch = input_table.dispatch(step, required)   // turns.rs
for slot in 0..1:
    if slot == local: inject captured local turn (ProcessCommands)  // re-sim only
    else:             inject remote turn from dispatch[slot] (ProcessCommands)
orig(ecx)                                          // run the frame (with 0x4d9550 forced-ready)
```

**Transport seam:** replace the engine's `SNetSendTurn` at `0x485a40` with a NOP + `cl_rollback_send`
(ADR-0001 §2.1), so the local turn goes into the rollback channel instead of CrownLink's lockstep
pipeline; remote turns arrive on `cl_rollback_set_receive_cb` and are `InputTable::push`ed. Native
lockstep (the `0x485aa0` pipeline-fill and `0x487070` stall) is bypassed by the `0x4d9550` trampoline.

**What could break (call out explicitly):**
- **Turn ≠ frame.** `g_turn_ticks_left` makes one turn span `g_51CE70[speed]` frames. The input table
  (`turns.rs`) assumes 1 turn = 1 step. For v1 either force `g_turn_ticks_left` to 1
  (`g_51CE70[speed] = 1`, and CrownLink `TurnsPerSecond` to the frame rate), or key the input table on
  *turn slots* and re-simulate in slot-sized chunks. **This is the single most important open design
  point** — flag as unverified/undecided. *(Measured 1 turn = 1 frame in the replay at its locked
  speed; see §0.1. Still unverified at other speeds and in a live game.)*
- **Double application of the local turn.** The engine applies the local turn itself (via its own
  send/receive path) and our injection must not apply it twice on the non-re-sim first pass.
- **`g_sequence` and the per-player context globals** must be restored with the snapshot (or reset by
  our injection), else command attribution leaks across a rollback.
- **`0x4d9550` is shared** with gptp's hooks; our trampoline must chain (call their `orig`) on the
  non-predicting path.
- **Empty frames** don't reach `0x485a40`; capture must tolerate "no turn this frame" (predicted
  empty turn).

---

## What to try first (cheapest confirmatory experiment)

> **Update 2026-10-18 — retargeted after measurement (§0.1).** Step 1 below was implemented
> (`CM_ROLLBACK_FORCE_TURN` + a `0x4D9550` trampoline with cadence tracing). It confirmed the hook is
> live but proved the replay harness takes **no** stall and already runs **1 turn per frame**, so
> steps 1–3 as written cannot distinguish anything here: suppression is never exercised and injection
> would double-apply the replay's own turns. The experiment now needs a live/2-client vehicle, or a
> harness variant that disables the replay injector (`0x00487100`) and drives the replay from the
> input table. Steps 2–3 (capture + inject) remain the goal.

The single cheapest experiment that confirms capture+inject+suppress on the existing headless
harness (`tools/gate.sh`), with **no transport**:

1. Add a `0x4d9550` trampoline that, when `CM_ROLLBACK_FORCE_TURN=1`, always jumps to `0x4d95b2`
   (never stalls), and log `g_turn_ticks_left`/`g_turn_stalled` around it.
2. In the `run_game_tick` hook, before `orig`, call `ProcessCommands` with a fixed 1-command blob
   (e.g. a NOP `[0x05]`) and log the before/after `g_sequence` (0x6556E8).
3. Run `tools/gate.sh Release 144` with `CM_ROLLBACK_SHADOW=1 CM_ROLLBACK_SNAPSHOT_SPACING=1`.

**Pass =** the shadow gate's `units`/`orders`/`images` ranges stay quiet (the injected command
reproduces byte-for-byte across a restore), and `g_sequence`/`g_turn_ticks_left` advance identically.
If `ProcessCommands` injection alone diverges, the turn↔frame mapping (§7) is the culprit and must be
resolved before the transport is wired.

---

## Unverified / flagged

- Storm ordinal **names** (128/121/115 = SendTurn/ReceiveTurn/GetTurnsInTransit) — numerics and thunk
  addresses are ground truth; names inferred from signatures.
- `g_51CE70` (per-speed turn-tick table, read at `0x4d962b`) values and the exact turn↔frame ratio at
  the replay's locked speed 6 — not dumped (runtime data, not in the file image).
- `0x486530` (the non-sync turn-processing path) — only its call site was traced, not its body.
- `g_turn_buffer` live capacity and the per-player buffer sizes (`0x654A80`/`0x6554B4`) — inferred as
  8×4 and ~0x220; transport-only, so low risk, but measure before depending on them.
- `0x486580`'s timeout threshold `0x9c4` (`0x4865bd`) — the "turn timed out" cutoff; not needed for
  v1 but relevant to the prediction limit.
