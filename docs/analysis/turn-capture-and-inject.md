# Turn capture and injection: what was verified, and what still needs a live test

**Status:** engineering notes + empirical findings.
**Grounding:** shipped `StarCraft.exe` (base `0x400000`) disassembled with `tools/dis161.sh` /
`objdump -d`; gptp `hooks/game_hooks_inject.cpp` (`game_recv_wrapper`), `hooks/game_hooks.cpp`
(`game_recv`, `replay_loop`), `BW/enumerations.h` (`GameCommand`), `BW/bwdata.h`;
`samase_shim/src/bw.rs`; and the code in `crates/cosmo-rollback/src/{turn_hook.rs,frame_hook.rs,rollback/mod.rs}`.

---

## 0. Summary

Two engine-facing primitives now exist and are wired, each behind an env knob:

- **Capture** — `turn_hook.rs` hooks `0x00485AA0` (the turn-pipeline fill) on entry and copies
  `g_turn_buffer[0 .. g_command_queue_size]` before it is flushed. `CM_ROLLBACK_CAPTURE_TURN=1`.
- **Injection** — `turn_hook.rs::inject_turn` sets the three player-context globals and calls
  `ProcessCommands` (`0x004865D0`) through a `whack_funcs!` wrapper that matches its real
  `@eax` convention. `CM_ROLLBACK_INJECT_TURN=1` drives a smoke test from the frame hook.

Both are *grounded* in the binary (see §2, §3). What they cannot be is *validated end-to-end* in
the replay harness (§5): the harness's input is the replay injector, not the turn buffer, so a
real capture→inject→re-sim loop needs a live or 2-client run.

---

## 1. The test vehicle: what the replay harness can and cannot exercise

The headless gate plays a **replay**. That changes two facts about the turn path:

1. **Input comes from the replay injector, not the turn buffer.** Inside the hooked frame
   function, gptp's `replay_loop` (patched at `0x004D96C4`) reads `ReplayData` and calls
   `ProcessCommands(…, flag=true)` per staged command. The local player issues no commands, so the
   turn buffer holds only the native `GameCommand_Hash` (`0x37`) sync command each boundary —
   **transport sync, not user input** (confirmed empirically, §5). Capture therefore can prove the
   hook fires and reads real bytes, but it can **never observe player commands** in this harness.

2. **The scheduler never stalls.** `g_turn_ticks_left` is 1 on entry every frame, so the stall path
   (`g_turn_stalled`, return 0) is never taken, and there is no missing peer turn to predict or
   contradict. There is no misprediction, so **no rollback is ever scheduled** and the
   input-table/session loop has nothing to do.

Consequences for each piece:

| piece | testable in the replay harness? | why / how |
|---|---|---|
| capture hook fires, reads real bytes | **yes** | `CM_ROLLBACK_CAPTURE_TURN=1` logs the `0x37` sync turn each boundary |
| capture of *player commands* | **no** | a replay has no local player commands, only the native sync hash |
| injection calling convention | **yes** | inject a known blob, watch `g_sequence` advance and no crash |
| injection reproduces a re-sim | **no** | shadow re-sim calls `orig` alone; the replay injector is the input, and injecting on top double-applies |
| full §7 dispatch→inject→orig loop | **no** | needs a real remote turn source |

The honest conclusion is the same as the doc's: capture and injection can be smoke-tested
individually, but the rollback loop needs a live/2-client test (§6).

---

## 2. Capture point: `0x00485A40` is the wrong hook (correction to turn-path.md §3)

`turn-path.md` §3 recommends hooking `0x00485A40` "the flush". Disassembling the call graph shows
that is the **mid-frame overflow flush**, not the per-turn one:

- `0x00485A40` (`send_turn`) is called from **exactly one** place: `0x00485C3D`, inside
  `send_command` (`0x00485BD0`), taken only when `g_command_queue_size + size >
  g_provider_max_buffer` — i.e. the buffer is full and must be flushed before appending.
- The **normal** per-turn flush is **inlined** in `0x00485AA0` (the turn-pipeline fill): at
  `0x00485B1F` it does `push eax; push 0x654880; call 0x410202` (`SNetSendTurn`) and at
  `0x00485B8D` resets `g_command_queue_size` to 0. `0x00485AA0` is reached from the turn scheduler
  through `step_network` (`0x00487070`) at `0x004870A7`, once per turn boundary.

`0x00485AA0` has five call sites (`objdump`): `0x46D439`, `0x4D42CA`, `0x4D9A4A`, `0x4F4138`, and
`0x4870A7`. The last is the turn boundary; the others are network init/end sites. All are
network-pipeline sites, and the capture is a pure read of `.data` globals before the buffer is
reset, so hooking `0x00485AA0` on entry is safe and correct for capture. `turn_hook.rs` therefore
hooks `0x00485AA0` (`FillPipeline`), not `0x00485A40`.

(If capture needs to be restricted to the turn boundary *only*, the caller `0x4870A7` can be
checked, or the read can move into the existing `TurnScheduler` trampoline where
`g_turn_ticks_left == 1` marks the boundary — see turn-path.md §3's "alternative".)

---

## 3. Injection: the `ProcessCommands` convention, confirmed three ways

`0x004865D0` is `ProcessCommands` / `game_recv`. Its convention is `eax = command bytes*`,
`[esp+4] = size`, `[esp+8] = replay_flag`, stdcall (`ret 8`). Three independent confirmations:

1. **Callee disassembly** (`dis161.sh 0x4865D0`): `mov esi,eax` (buffer), `mov eax,[ebp+8]`
   (size), `mov edx,[ebp+0xc]` (flag); the function ends `pop …; mov esp,ebp; pop ebp; ret 0x8`.
2. **gptp's `game_recv_wrapper`** (`game_hooks_inject.cpp:124`), which is jump-patched onto
   `0x004865D0` (`:327`) and unpacks exactly `eax → cur_len`, `[ebp+8] → data_len`,
   `[ebp+0xc] → as_replay`, then `RETN 0x08`.
3. **`samase_shim/src/bw.rs:122`**: `0x004865D0 => ProcessCommands(@eax *const c_void, u32, u32);`.

`turn_hook.rs` declares it as `whack_funcs!(stdcall, …, 0x004865D0 => ProcessCommands(@eax *const u8, u32, u32);)`
and `inject_turn` sets `*g_active_nation_id` (`0x00512678`), `g_active_player_id` (`0x0051267C`),
`*g_sequence` (`0x006556E8`), calls it, then restores the two player-context globals (leaving
`g_sequence` — its post-call value is simulation state, and it is snapshotted).

The `replay_flag` (`[esp+8]`) is `false` in a live game (`g_is_in_replay` `0x006D0F14` is 0, so the
command is fully processed either way). In a replay the same flag must be `true`, or the "replay
watcher" filter (`game_hooks.cpp` game_recv) drops everything but a handful of multiplayer commands.
`turn_hook.rs` exposes the flag via `CM_ROLLBACK_INJECT_FLAG`.

The no-op / sync command is `GameCommand_Sequence = 5` (`0x05`, `enumerations.h:1353`) — the byte
the engine writes to an empty turn buffer — and processing it increments `g_sequence`. This is a
**live, reportable bug** in `turns.rs`: its `SYNC_NOP` is `0x0f`, so a turn holding the real
`0x05` no-op is treated as a misprediction (`matches_prediction` returns false). `turns.rs` is
owned by the main agent; this is flagged here, not edited.

---

## 4. Env knobs added

| knob | effect |
|---|---|
| `CM_ROLLBACK_CAPTURE_TURN=1` | install the `0x00485AA0` capture hook; log each captured turn (first 8, then every 256) |
| `CM_ROLLBACK_INJECT_TURN=1` | inject a turn through `ProcessCommands` before each real step (smoke test) |
| `CM_ROLLBACK_INJECT_HEX=<hex>` | the blob to inject (default `05` = the sync no-op) |
| `CM_ROLLBACK_INJECT_FLAG=1` | pass `replay_flag=true` (needed for non-trivial commands under a replay) |

Default behaviour with none of these set is unchanged: the capture hook is not installed and no
injection runs, so the existing shadow gate path is byte-for-byte the same.

---

## 5. Empirical results

Headless probe, `CM_ROLLBACK_CAPTURE_TURN=1 CM_ROLLBACK_INJECT_TURN=1 CM_ROLLBACK_SHADOW=0
CM_ROLLBACK_NO_PLUGIN=1` (no shadow, to isolate the turn path). Both knobs armed at load:

```
[cosmo-rollback] ProcessCommands (0x004865D0) callable registered
[cosmo-rollback] installed FillPipeline (turn capture) trampoline at 0x00485AA0
[cosmo-rollback] shadow=0
```

**Capture fires, and captures non-empty turns** — the turn buffer in a replay holds the native
`GameCommand_Hash` (`0x37`, `enumerations.h:1362`) sync command, not an empty/NOP turn:

```
capture #0: frame 0 turn_len 7  bytes [37, 10, e2, c2, 00, 00, 00]
capture #1: frame 0 turn_len 7  bytes [37, 10, e2, c2, 00, 00, 00]
capture #2: frame 0 turn_len 7  bytes [37, 10, e2, c2, 00, 00, 00]
capture #3: frame 1 turn_len 19 bytes [37, 20, d1, 18, 00, 00, 00, 63, 02, ...]
capture #4: frame 2 turn_len 55 bytes [37, 30, f8, 90, 00, 00, 00, 63, 02, ...]
capture #5: frame 3 turn_len 19 bytes [37, 40, 12, 04, 00, 00, 00, ...]
```

Two things this confirms: (a) the capture hook reads the real buffer at the right moment (the
`0x37` + hash payload is exactly what `turn-path.md` §5 calls "the native 0x37 sync"); (b) the
`0x485AA0` multi-caller caveat is real — three captures land on frame 0 (the network init sites
`0x4D42CA`/`0x4D9A4A`/`0x4F4138`), then one per frame once the game loop runs. For a rollback game
only the once-per-boundary capture matters; the init-time captures are empty-of-commands in
practice and are filtered by the consumer.

**Injection fires, and the convention is right** — the no-op is processed (`g_sequence` 0 → 1) and
nothing crashes, so `eax = bytes, [esp+4] = size, [esp+8] = flag, ret 8` is confirmed live:

```
inject #0: player 0 len 1 flag 0 bytes [05] -> g_sequence 0 -> 1
inject #1: player 0 len 1 flag 0 bytes [05] -> g_sequence 0 -> 1
...
```

(The `flag 0` / replay-watcher path still processes `GameCommand_Sequence`, as expected from the
`game_recv` switch; a non-trivial command under a replay would need `CM_ROLLBACK_INJECT_FLAG=1`.)

**What this still does not prove**: the full §7 dispatch→inject→orig re-sim loop, and capture of
*player commands* (a replay's `0x37` hash is transport sync, not user input). Both need a live or
2-client run (§6).

---

## 6. Cheapest way to a live / 2-client test

The replay harness cannot drive the rollback loop (§1). The cheapest ways forward, in order:

1. **Synthetic remote turn source in the replay harness.** Disable the replay injector by zeroing
   `g_sync_turn_enabled` (`0x006556E0`) — `replay_loop` early-returns on `!g_sync_turn_enabled`
   (`game_hooks.cpp:5140`) — and inject a scripted "remote" turn via `inject_turn` at chosen
   frames. This exercises dispatch→inject→orig without a second machine, at the cost of the replay
   no longer supplying its own commands (so the sim is whatever the script injects). The
   no-op-only path is then trivially deterministic; a real command (e.g. a fixed `GameCommand_Build`
   `0x0c`) would exercise command attribution.
2. **One machine, two wine processes.** CrownLink's SNP loopback already exists (ADR-0001); two
   instances over it, one `inject_turn` per player, is the smallest real 2-client test.
3. **The doc's "What to try first" experiment**, completed: with a fixed blob injected before
   `orig` and `g_sequence`/`g_turn_ticks_left` logged, confirm the injected command reproduces
   byte-for-byte across a restore. The shadow gate's existing plugin-state divergences are a known
   separate issue and must not be attributed to injection.
