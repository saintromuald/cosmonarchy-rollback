# ADR-0007 — The live rollback client

- **Status:** Accepted (implemented; validated in-harness and over UDP between two instances;
  a real CrownLink match is still unvalidated)
- **Date:** 2026-10-19
- **Refines:** ADR-0001 (architecture), ADR-0003 (hook boundary), ADR-0006 (transport)
- **Grounding:** `crates/cosmo-rollback/src/rollback/live.rs`, `turn_hook.rs`, `frame_hook.rs`,
  `session.rs`, `transport.rs`; the stock exe disassembled with `ndisasm`.

---

## Context

ADR-0006 built the transport and the engine-independent netcode core (input table with
prediction, tick plan, session, desync hashes, authority). This ADR is the piece that makes
them run a **match**: the per-frame loop that reads the local turn out of the engine, injects
every slot's turn for every step, and re-simulates on a misprediction.

## Decision

### 1. One game frame, in order (`Live::tick`)

1. **Absorb** the peer's messages: turns into the input table by sequence number, hashes into
   the desync comparison, acknowledgements, leaves.
2. **Send** the local turn. It is read from the engine's turn buffer *before* the frame's flush
   clears it, because at that moment it holds the commands the player issued during the
   previous frame — which are the turn for the step this frame runs. The engine's flush runs at
   the start of the frame, before `StepObjects`, so the engine applies them to the same frame.
   An empty buffer becomes the engine's empty-turn marker (`GameCommand_Sequence`, `0x05`),
   which is what the flush would have sent.
3. **Stall** rather than predict past the limit: the frame is not run at all, exactly like the
   engine's own lockstep stall. The engine's turn scheduler is also forced ready
   (`force_turn`), so it never skips a frame for a *missing* peer turn — the client decides.
4. **Plan** (roll back if the input table has contradicted a prediction), **restore** the
   snapshot at or before the rollback target, then run the steps: for each step, dispatch every
   slot's turn, inject it with `ProcessCommands`, and call the frame function.
5. **Settle**, record our desync hash, and report the hash of the newest **confirmed** frame
   every `CM_ROLLBACK_HASH_EVERY` (default 8, ADR-0001 §1.1).
6. **Adjudicate** (host only): feed the peer's reports to the `Authority`; evict the minority
   or void the game on an even split.
7. **Cadence**: acknowledge the peer's turns and re-send anything unacknowledged.

### 2. The engine's turn path is suppressed, in the least invasive way that is correct

Two patches, both behind `CM_ROLLBACK_LIVE`:

- **Zero the turn buffer** at the flush hook (`turn_hook`'s `FillPipeline` trampoline on
  `0x00485AA0`), after capturing it. The engine's lockstep flush then sends only its empty-turn
  marker, so the real commands never travel over lockstep — they go on the rollback channel.
  The buffer is still reset, and the pipeline, the **game-start loop** (`0x004D9A4A` fills the
  pipeline until `step_network` reports ready) and the acknowledgement plumbing all keep
  working.
- **No-op the turn processor** (`0x00486F50`). It reads the per-player turn store and does
  **not** clear it, so a re-simulation would otherwise re-apply *whatever the store holds now* —
  a different frame's turns — and the live pass and the re-sim would disagree. With it no-oped,
  the live pass and every re-sim apply exactly what the client injects.

**Rejected: NOPing the lockstep send** (`0x00485B25`). It hangs the game: the start loop fills
the turn pipeline until `step_network` reports ready, and with the send gone it never does.
(Kept as `CM_ROLLBACK_LIVE_NOP_SEND` for experiments.)

**Rejected: suppressing `receive_network_messages`.** The local turn loops back through it, so
the pipeline would never fill — the same hang.

### 3. Start only when the game has started

`Live::tick` passes the frame straight through until `g_is_in_game` (`0x006D5BC8`) is set. Two
clients must begin from the same state, and the only state they share is the start of the game;
driving from the menu would start each client at whatever menu state it happened to be in.

### 4. A scripted peer and a UDP carrier, so the loop is testable without a match

- With no transport (a replay harness), the peer's turn is **scripted**
  (`CM_ROLLBACK_SCRIPT_REMOTE` / `_DELAY`, as `rollback/drive.rs` does): on time during the
  lockstep start, `delay` ticks late afterwards, which forces a prediction and then a real
  rollback.
- `CM_ROLLBACK_UDP=<local>:<peer>[:delay_ms]` runs the client over a real UDP socket instead of
  CrownLink, carrying the **same** `Msg` encoding, so two clients can be run on one machine
  (`tools/two_client_test.sh`) to exercise the transport, the acknowledgement cadence and the
  hash exchange for real.

## Consequences

### Positive

- The full loop runs end to end in the harness: `CM_ROLLBACK_LIVE=1` with a scripted delayed
  peer gives **0 stalls and real rollbacks** (`live rollback #1: frame 5 -> restore 3 …`).
- Two real game instances over UDP (`tools/two_client_test.sh`) link up, exchange turns/acks/
  hashes and **both roll back**; the host's `Authority` adjudicates and reports agreement; and
  the run ends with **no desync on either client** and **identical per-range fingerprints**
  (`live fp`) on both.
- Six real bugs were found and fixed by these tests, each of which would have broken a match:
  1. `report_hash` was gated on `confirmed`, which lags with any delay, so no hash was ever
     recorded and the desync comparison never ran.
  2. A frame produced twice (a rollback re-simulates it) kept its *stale* pre-rollback hash, so
     the peer's corrected hash was compared against a prediction.
  3. The `Authority` was only fed the peer's reports, never our own, so it could never reach a
     verdict.
  4. Caching the range table (`engine_ranges()`) made it build at plugin **startup**, before the
     `UnitPath`/`AiScript`/`AiTown` pools exist — silently dropping those ranges and bringing
     the movement divergence back. It is now built on the first frame.
  5. The cross-client hash covered the auxiliary blobs (tile flags, creep, image iscript) and
     the `UnitPath` pool, which the outer loop writes *between* frames (the boundary diagnostic
     shows 145 frames where the exit and the next entry differ with **no flat range** differing,
     i.e. only those blobs).
  6. A re-simulated step did not re-run the outer loop's `receive_network_messages`, whose
     command dispatcher mutates simulation state — the same fix the shadow gate needed.
- The last desync was pinned down with a per-range fingerprint logged at the reported frame
  (`live fp`): the clients differed only in **`sprite_count`**, one frame out of phase, because
  the draw pass allocates and frees high-index *helper* sprites outside the frame. It is now
  excluded from the cross-client comparison (still snapshotted).
- The default path is untouched: with `CM_ROLLBACK_LIVE` unset, none of this runs (verified:
  the shadow gate is unchanged at 5 divergences, and no `live:` line appears).
- Startup cost cut: the range table is built once, the per-object field ranges are summarised in
  the log instead of printed one per line (116k log lines → ~230), and the auxiliary captures
  log their geometry once instead of every capture.

### Negative / unverified

- **A real CrownLink match is still unvalidated end to end.** The channel is only loaded when a
  provider is bound; in a replay there is no SNP module at all (ADR-0006). The UDP carrier
  exercises the same `Msg` codec and cadence, not CrownLink itself. Progress and the two
  blockers found while driving a real lobby are in "Delivering a real CrownLink match" below.
- The harness cannot drive the live client far (a few frames): live mode suppresses the replay's
  own stepping, so a replay stalls after a handful of frames. That is a harness limitation, not
  a client one.
- `sprite_count` is excluded from the cross-client comparison rather than fixed at the source:
  the sprite pool's helper entries (allocated by the draw pass) may deserve deeper treatment,
  which is the deep lane's boundary work (`docs/analysis/boundary-fix.md`).

## Delivering a real CrownLink match

Driving an actual lobby (two clients on one machine, `tools/crownlink_match.sh`) found two
problems that never appear in the replay/UDP path, both now fixed in the CrownLink fork:

1. **The provider never appeared in the multiplayer menu.** The `.snp` carries its provider
   caps as a *signed MPQ archive appended after the PE image* (`MPQ\x1a` at the end of the
   file). `scripts/build_caps.py` + `mpqcli` produce it, but the CI artifact collected the bare
   linker output (no appended archive), so Storm never enumerated the provider and the game
   fell back to the stock `Select Connection` list. The stock `.snp` is 932,307 B (1,491 B of
   caps); the broken artifact was 930,304 B. The workflow now uploads `dist/CrownLink.snp` and
   fails if the caps archive is missing.
2. **Every join timed out with "Unable to join game".** The rollback channel used
   `sub_type == 15` as its marker and the receive path diverted *any* packet with that sub-type
   to the rollback callback. Storm's own `Sys` packets also use sub-type 15 — the host sends
   `tp:Sys st:15` throughout the join handshake — so the joiner never received it and gave up
   after ~5 s (`PlayerLeave`), even though CrownLink's own handshake
   (`PlayerJoin` → `PlayerJoinAcceptStart` → `PlayerInfo` → `PlayerJoinAcceptDone`) completed.
   The divert now also requires `type == CrownLink`, which is what `send_custom_message` sets.

Two environment facts worth knowing when reproducing on one host:

- **Private lobbies.** Both clients set the same `lobby_password` in `CrownLink.json`; the games
  list then shows only our game. Without it the public list re-orders as other players
  advertise and the selection changes between click and confirm (StarCraft's selection is by
  row index, not by entry).
- **Some maps crash `gptp.qdp` when a lobby is created** (e.g. `(2)Potivector 1.1`,
  `(4)Exodus 1.0`; `(2)Pagan Imperium 1.0` is stable). The crash is in the plugin, not the
  rollback code, and is recorded in `Starcraft/Errors/*.ERR`.

`tools/ui_session.sh` (persistent scriptable session), `tools/crownlink_match.sh` (host/join
flow) and `tools/ocr_row.py` (read a list row by text) are the harness.

### Status: verified in a real CrownLink match

`tools/crownlink_match.sh run` (host + joiner on one machine, through the CrownLink lobby)
now produces a **passing** run:

```
A (a): carrier='CrownLink (engine applies turns) — injection off' sent=500 recv=500 rollbacks=10 agreed=3 desyncs=0
B (b): carrier='CrownLink (engine applies turns) — injection off' sent=500 recv=500 rollbacks=10 desyncs=0
PASS: rollback turns crossed CrownLink, both clients rolled back and agreed
```

A second run with `CL_SEND_DELAY_MS=150` (a one-way delay applied to the client's own channel
messages, so a same-machine match mispredicts) passes with the rollback path actually working:
`steps 363, rolls 95` by the third tick report, `sent=1000 recv=1000`, `agreed=5`, `desyncs=0`,
and the game still renders at `00:00:09`.

Both clients log the same takeover hash (`0x64d82a89b8b1f33e` at `units 132 map 128x128`),
both run at the game's own rate with real rollbacks, and both render the same match (verified
by screenshot at `00:00:10`). Three separate bugs had to fall out to get here:

1. **The capture hook captured turns but the client injected them too.** In a real match the
   engine receives the peer's turns through the `.snp` and applies them itself (the turn
   processor is only no-op'd for the *lockstep send* path we suppress). Injecting the recorded
   copy applied every command twice and crashed inside `ProcessCommands` — an access violation
   with `EIP=0xFFFFFFFF` on the host, right after the first injected turn. The fix is
   `Link::inject_turns()`: true for the harness carriers (nothing else feeds the engine's turn
   store), **false for CrownLink**.
   The `Box<dyn Link + Send>` impl must forward that method — its default silently re-enabled
   injection and cost a debugging cycle.
2. **Taking over too early replayed the match's init turns.** The engine applies the "start
   game" and per-player init commands before the client takes over; re-injecting them replays
   game init on an initialised world. `CM_ROLLBACK_SKIP_START_TURNS` (default 2) skips those
   live steps. With injection disabled for CrownLink this is belt-and-braces, but it is what
   makes the *harness* path safe if injection is ever re-enabled there.
3. **Suppressing the engine's turn path at plugin load broke the lobby** (the map distribution
   and lobby state travel over the same turn pipeline) — see the `g_is_in_game` gate above.

A first-chance vectored exception handler (`crash.rs`) was added because the engine's crash
report cannot symbolise a fault through a poisoned indirect call; it logs the fault address,
the touched address and the top of the stack on the first access violation.

### One-sided arming falls back to lockstep

Arming is per player, so one side can forget the plugin. A client that takes over and then sees
no rollback turn from the peer for 3 s logs

```
live: no rollback turn from the peer within 3 s — the other player is not running the plugin
      (or has no rollback-capable CrownLink.snp); passing frames through, normal lockstep
```

and stops driving. Verified by removing the plugin from the joiner: the host logged exactly that
and the match ran normally (`00:00:40` on screen), instead of stalling both clients.

### Player package

`dist/rollback-client.zip` (also unzipped in `dist/rollback-client/`) contains the release
plugin, the rollback-capable `CrownLink.snp`, host/guest `cosmo_rollback.conf` files, checksums
and a player-facing README. It was validated as a *fresh install*: a clean copy of the game with
only those files applied played a lobby match against another install and passed
(`sent=1500 recv=1500 rollbacks=11 agreed=3 desyncs=0`).

### AI: the engine's AI is rollback-safe, `aise` is not

The shadow gate's divergences were traced to `aise`, not to the engine's AI:

| configuration | gate result |
|---|---|
| `aise.qdp` loaded | 5 divergences in `ai_script_pool`, `ai_town_pool`, `first_ai_script` |
| `aise.qdp` removed | **0 divergences** with `CM_ROLLBACK_GATE_IGNORE=sprite_count` |
| `aise.qdp` removed | 7 divergences, all `sprite_count` (render count; see below) |

So the engine's own AI keeps its state entirely in the pools the snapshot already covers, and a
match driven by it re-simulates exactly. `aise` keeps its state in its own globals, outside the
engine, which no snapshot can reach without cooperation from that plugin.

This does **not** affect multiplayer: Cosmonarchy's CrownLink lobby is human-only (the slot
control offers only Open/Closed; the UI has a "Claimants"/"Witnesses" split and no computer
option), so an armed match never contains a computer player. AI is a single-player feature,
where rollback is not armed.

`CM_ROLLBACK_GATE_IGNORE=<range,…>` excludes named ranges from the gate's comparison. It exists
for `sprite_count` — the draw pass allocates and frees helper sprites *between* frames, so a
re-sim that skips the draw pass legitimately leaves the count different. The live client already
excludes the same range from its cross-client hash.

### AI matches are refused, not desynced

`live` checks `first_ai_script` (`0x0068C100`) at takeover. A non-zero value means an AI script
is running, i.e. a computer player is in the match; its state lives inside `aise`, outside the
engine snapshot, so a re-simulation would make different decisions. The client then logs
`AI script present … passing frames through` and runs normal lockstep instead of driving.
Melee matches read zero (verified: the guard does not trigger in the passing runs above).

### Negative / unverified

- **AI rollback is still out of scope.** The shadow gate's last 5 divergences are AI state
  (`ai_script_pool`, `ai_town_pool`, `first_ai_script`). They need the shipped `aise.qdp` —
  Cosmonarchy's private "Aise New Game State plugin 3.25.0", built against
  `samase_plugin@1e22fc5` — to expose and restore its own state. Public `neivv/aise` (2.42.1,
  `cai88f`) refuses to run against this build (it aborts the game on load), and its upstream org
  is gone, so no version-matched source exists publicly. v1 is 1v1 human melee (ADR-0001 §4).
- **Some maps crash `gptp.qdp` on map/lobby load** (`(2)Potivector 1.1`, `(4)Exodus 1.0`; the
  `LastReplay.rep` in these trees does it intermittently). It reproduces with our plugin removed
  and is recorded in `Starcraft/Errors/*.ERR`.
- **`tools/two_client_test.sh`** (UDP carrier) depends on that replay path: it reports
  `INCONCLUSIVE` when the two clients start different games, rather than claiming a netcode
  failure. The CrownLink run above is the primary validation.
