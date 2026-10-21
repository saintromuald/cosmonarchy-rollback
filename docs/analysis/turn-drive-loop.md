# Turn drive loop: dispatch → inject → re-sim → compare, validated in the harness

**Status:** engineering notes + empirical findings.
**Grounding:** shipped `StarCraft.exe` (base `0x400000`) disassembled with `tools/dis161.sh`;
gptp `hooks/game_hooks.cpp` (`replay_loop`), `hooks/game_hooks_inject.cpp` (`call_patch`,
`game_recv_wrapper`), `BW/bwdata.h` (`g_sync_turn_enabled`); the code in
`crates/cosmo-rollback/src/{rollback/drive.rs,turn_hook.rs,rollback/mod.rs}`.

---

## 0. Summary

`rollback/drive.rs` (`CM_ROLLBACK_DRIVE=1`) exercises the full §7 loop in the replay harness,
which the capture/inject smoke tests could not (see `turn-capture-and-inject.md` §1): it

1. suppresses the replay command injector — by default by **NOPing the `call replay_loop`** gptp
   patched onto `0x004D96C4` (5 bytes), leaving `g_sync_turn_enabled` and the sync-turn
   machinery intact (see §1 for why zeroing the flag is the wrong knob here);
2. drives a 2-slot `Session` (local slot 0, scripted remote slot 1) one turn per step;
3. injects the dispatched turn for each slot before `orig`, **on both the forward and the
   re-sim path** (a rollback re-dispatches and re-injects the same turns);
4. forces a real misprediction → rollback by delivering the remote turn a fixed number of
   steps late, and
5. runs a determinism probe at `CM_ROLLBACK_DRIVE_PROBE` that re-simulates the same frames
   twice from the same snapshot and compares.

The probe separates the turn loop from the known plugin-state divergence: `sequence` and
`command_users` are pure functions of the injected turns, so they **must** reproduce; the full
state hash is expected to differ in the baseline plugin-`.data`-driven ranges (which the
existing shadow gate also shows, with **no** injection — `turn-capture-and-inject.md` §1,
and the `CM_ROLLBACK_NO_PLUGIN` gate).

---

## 1. Grounding: what `g_sync_turn_enabled = 0` actually turns off

The task premise is confirmed at two independent levels:

- **Stock exe.** The frame function `0x004D9670` calls the turn injector at `0x004D96C4`:

  ```
  004D96C4  call 0x487100        ; the sync-turn injector
  004D96C9  lea  ecx,[ebp-0x18]
  004D96CC  call 0x4d9550        ; TurnScheduler (runs the network step)
  ```

  and `0x00487100` begins:

  ```
  00487100  mov  eax,[0x6556e0]   ; g_sync_turn_enabled
  00487105  push ebx
  00487106  xor  ebx,ebx
  00487108  cmp  eax,ebx
  0048710A  jz   0x487249         ; -> pop ebx; ret   (early return when 0)
  ```

  i.e. the stock injector early-returns when `g_sync_turn_enabled == 0`. (The rest of
  `0x00487100` loops the staged per-player turn buffers `0x006552B0`/`0x006554D8`/
  `0x00654AA8`, sets the player context, clears `g_sequence`, and calls `ProcessCommands`
  `0x004865D0` per player — the same shape `turn_hook::inject_turn` uses.)

- **Patched exe.** gptp `call_patch(replay_loop, 0x004D96C4)` (`game_hooks_inject.cpp:344`)
  replaces that `call` with its own, and `replay_loop` (`game_hooks.cpp:5139`) begins
  `if (!g_sync_turn_enabled) return;`. So in the shipped build the replay commands come from
  `replay_loop`, and zeroing the flag makes it return immediately.

`g_sync_turn_enabled` (`bwdata.h:1433`) is read by nothing else in *gptp's own source*, but
the stock exe reads it in **two** places, and zeroing it has a second, benign effect beyond
`replay_loop`:

- **The stock turn injector `0x487100`** (replaced by gptp's `replay_loop` at `0x004D96C4`,
  so dead in the shipped build) — see above.
- **`step_network` `0x00487070`** (reached from the turn scheduler `0x004D9550` once per turn
  boundary) gates on it at three sites:

  ```
  004870A7  call 0x485aa0        ; FillPipeline (turn-buffer send + capture hook) — unconditional
  004870B4  mov  eax,[0x6556e0]
  004870B9  test eax,eax
  004870BB  jz   0x4870c4         ; sync disabled -> call 0x486530 (local-only)
  004870BD  call 0x486f50         ; sync enabled  -> turn receive (sets rng_enabled etc.)
  ...
  004870DB  mov  eax,[0x6556e0]
  004870E0  inc  ecx              ; turn counter 0x57eebc (still increments either way)
  004870E1  test eax,eax
  004870E9  jz   0x4870f0         ; sync disabled -> skip the sync-hash tail
  004870EB  call 0x47cfc0         ; sync hash (advances 0x65eb2e/0x65eb2c)
  ```

  So zeroing the flag makes `step_network` take its local-only path: the turn-buffer flush
  (and the capture hook at `0x00485AA0`) still runs, the turn counter still increments, but
  the sync turn receive and the sync-hash tail are skipped. That is exactly what drive mode
  wants — the network step must not try to receive/apply real sync turns. Both branches write
  only deterministic globals, and the flag is 0 on *both* the forward and re-sim path, so
  this does not disturb the determinism probe.

**But zeroing the flag hangs the replay.** Confirmed empirically with `CM_ROLLBACK_SUPPRESS_SYNC=1`
(the normal path, no drive): the first frame logs `suppress replay injector: g_sync_turn_enabled
0x1 -> 0` and then the frame function never returns (no `frame 0 rng` line). The replay's frame
loop depends on the sync-turn path `g_sync_turn_enabled` gates, so the flag is the wrong knob to
suppress the replay *command* injector. The `"nop"` method (NOP the `call replay_loop` at
`0x004D96C4`, leaving the flag alone) suppresses only the command injection and works — which is
why it is the default.

---

## 2. What `drive.rs` does, per frame

`rollback/mod.rs`'s `on_step` delegates to `Drive::tick` when `CM_ROLLBACK_DRIVE` is set (the
default path is unchanged; see §4). Each tick:

1. first tick only: suppress the replay command injector and take a frame-0 snapshot. The
   suppression method is `CM_ROLLBACK_DRIVE_SUPPRESS` (`"nop"` default — NOP the
   `call replay_loop` at `0x004D96C4`; `"flag"` — zero `g_sync_turn_enabled`, which hangs the
   replay, see §1);
2. push the local turn (`CM_ROLLBACK_SCRIPT_LOCAL`, default the no-op `0x05`);
3. push the remote turn `CM_ROLLBACK_SCRIPT_DELAY` steps late (`CM_ROLLBACK_SCRIPT_REMOTE`,
   default `GameCommand_Hash 0x37 00 00 00 00 00 00` — a valid, harmless command that is
   *not* a no-op, so a late arrival marks the step mispredicted);
4. `Session::plan()`; if it restores, `restore(restore_frame)`;
5. for each step, `dispatch(f)` → `inject_turn` each non-empty turn → `orig`; snapshot on the
   session's spacing;
6. `settle(present, hash)`.

The local turn is always on time; the remote turn is always `delay` steps behind, so each
late non-no-op remote turn marks its step mispredicted and the next tick rolls back and
re-simulates. That is the §7 loop the smoke tests never reached.

## 3. The determinism probe

At `CM_ROLLBACK_DRIVE_PROBE` (default 60), `run_probe` restores the newest snapshot strictly
before that frame, captures it as `base`, and runs `base → probe_frame` **twice**
(dispatch → inject → `orig` each step), then compares:

- `state_hash` — the full snapshot; **expected MISMATCH** (baseline plugin divergence),
- the `sequence` range (`0x006556E8`) — the per-turn command counter; **expected MATCH**,
- the `command_users` range (`0x00512678`, 16 B) — the player-context globals `inject_turn`
  sets/restores; **expected MATCH**.

`sequence` and `command_users` are written only by `inject_turn` (and, for `sequence`, by
`ProcessCommands` counting the injected commands), so a MATCH proves the injected turns
reproduce byte-for-byte across the restore + re-inject + re-sim — the composition
`turn_hook::inject_turn` + `Session::dispatch` + `Snapshot::restore` the whole loop depends
on. A MISMATCH in either would be a genuine bug in that composition, independent of the
plugin.

---

## 4. Empirical results

Headless probe, `CM_ROLLBACK_DRIVE=1 CM_ROLLBACK_SHADOW=0 CM_ROLLBACK_NO_PLUGIN=1` (drive mode,
default "nop" suppression):

```
[cosmo-rollback] replay loop suppressed: NOP at 0x004d96c4 (was 0xE8, now [90, 90, 90, 90, 90])
[cosmo-rollback] drive: local [05] remote [37, 00, 00, 00, 00, 00, 00] delay 2 probe f60 spacing 3
```

**The injector is suppressed and the input table drives the simulation.** The first two steps
run on a predicted remote turn, then the late remote `GameCommand_Hash` arrives and the session
rolls back — and the re-sim re-injects both slots' turns (the `RESIM` step shows `injected 2`,
i.e. the local `[0x05]` **and** the now-known remote `[0x37 …]`):

```
[cosmo-rollback] drive step 0 predicted 1 injected 1 -> seq 1
[cosmo-rollback] drive step 1 predicted 1 injected 1 -> seq 1
[cosmo-rollback] drive rollback #1: current 2 -> restore f0 (rollback_to Some(0))
[cosmo-rollback] inject #2: player 0 len 1 flag 0 bytes [05] -> g_sequence 0 -> 1
[cosmo-rollback] inject #3: player 1 len 7 flag 0 bytes [37, 00, 00, 00, 00, 00, 00] -> g_sequence 0 -> 1
[cosmo-rollback] drive step 0 RESIM predicted 0 injected 2 -> seq 1
[cosmo-rollback] drive step 1 RESIM predicted 1 injected 1 -> seq 1
[cosmo-rollback] drive step 2 predicted 1 injected 1 -> seq 1
```

This repeats every step (320+ rollbacks by the time the probe runs — one per late remote turn),
each restoring the newest snapshot at or before the mispredicted frame and re-simulating forward
with the corrected turns. That is the §7 loop the smoke tests never reached.

**The determinism probe is the decisive result.** At frame 60 it restores the frame-57 snapshot,
re-simulates frames 57→60 **twice** (dispatching + injecting the same turns both times), and
compares:

```
[cosmo-rollback] drive probe f60 g57 steps3: state_hash MISMATCH sequence MATCH command_users MATCH
    range units                    1 bytes differ
```

- `sequence` and `command_users` — the turn-attribution state written only by `inject_turn` /
  `ProcessCommands` — **reproduce byte-for-byte** across restore → re-inject → re-sim. This is
  the missing proof: `turn_hook::inject_turn` composes with `Session::dispatch` and
  `Snapshot::restore`.
- `state_hash` differs by exactly **1 byte** in the `units` pool, which is the re-sim drawing a
  different RNG than a clean replay would — the plugin's flat `.data` statics (ADR-0004 Layer A)
  are not snapshotted, the same divergence the shadow gate reports with **no** turn injection at
  all. The turn loop contributes no new non-reproducibility.

(Zeroing `g_sync_turn_enabled` instead — the `"flag"` method — hangs the replay on the first
frame, because that flag also gates `step_network`/`FillPipeline`'s sync-turn path, §1. The
`"nop"` method above is the one that works.)

## 5. What remains unverified / the cheapest real vehicle

- **Full-state reproducibility** cannot be shown in this harness: the re-sim consumes a
  different number of RNG draws (the shadow gate logs `rng_calls` delta ≠ 0) because the
  plugin's flat `.data` statics (`g_trigger_vars`, mode state, etc. — ADR-0004 Layer A) are
  not snapshotted, and restoring them wholesale corrupts the plugin. This is the same
  divergence the gate shows with no injection at all, so it is not introduced by the turn
  loop. The fix is to extend gptp's `cosmo_gptp_state_size/save/load` serializer (Layer C) to
  cover those statics, which needs a gptp CI rebuild (blocked).
- **Real peer turns / 2 clients** still need a live or loopback run (see
  `turn-capture-and-inject.md` §6, option 2). The scripted remote source here is a stand-in.
