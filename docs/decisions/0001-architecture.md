# ADR-0001 — Rollback netcode architecture for Cosmonarchy BW over CrownLink

- **Status:** Accepted (proposed)
- **Date:** 2026-10-10
- **Decision owner:** architecture lead
- **Supersedes:** none
- **Grounding:** `docs/RECON.md`, `docs/PORT-PLAN.md`, `reference/ShieldBattery/docs/rollback.md`, and the code cited inline.

---

## Context

We are porting GGPO-style rollback netcode (prediction + re-simulation + state-hash adjudication)
into **Cosmonarchy BW**, a total-conversion of StarCraft: Brood War **1.16.1**. Today multiplayer
runs over **CrownLink**, a storm `.snp` provider that ships game turn packets verbatim over
libjuice/WebRTC P2P with a TCP lobby (`crow_serve`), and which has **no** state hashes, **no**
desync detection, **no** session clock, and does **not** own the simulation loop (`RECON.md:36-40`).
The reference implementation is ShieldBattery's `game` DLL (`reference/ShieldBattery/game`), which
already runs rollback on StarCraft: Remastered (`reference/ShieldBattery/docs/rollback.md`).

Three facts shape every decision below:

1. **The target is a patched 32-bit PE (i386) StarCraft 1.16.1.** Its globals/functions/struct
   offsets are *published and stable* to the BW modding community (`RECON.md:192-195`), but the
   Cosmonarchy `.qdp` plugins add mutable simulation state that no published map covers
   (`RECON.md:30-32`, `RECON.md:197-198`).
2. **CrownLink is a byte transport.** It implements storm's SNP ABI (`reference/CrownLink/src/snp/SNPModule.cpp:354-383`)
   and forwards `GamePacketData` without building or validating its header
   (`RECON.md:63-67`); it has no reliability layer of its own beyond storm. Its lobby, `crow_serve`,
   is authoritative only for identity, lobby list, and ICE/TURN credentials — never for simulation
   (`RECON.md:79-80`).
3. **We are not shipping a server farm.** Whatever authority model we pick must be something a
   single hobbyist can actually run — a single lightweight server, or no server at all.

What transfers unchanged from ShieldBattery, and what must be redone, is mapped in `RECON.md:184-204`.

---

## Decision

### 1. Authority / transport model — **host-authority peer model, layered over CrownLink's existing P2P**

We do **not** adopt ShieldBattery's relay-authority model. We make **one peer the authority for the
session** and exchange rollback turns, acknowledgements, and state hashes over CrownLink's existing
juice/WebRTC P2P links. `crow_serve` stays a dumb matchmaker and is **not** modified for the game
phase. No new server is deployed.

**Who is authority:** the lobby host (storm makes the session creator slot 0; `HOST_SLOT = SlotId(0)`
in `reference/ShieldBattery/game/src/netcode_v2/mod.rs:97-99`). CrownLink is already host-centered —
the host advertises the `AdFile` (`reference/CrownLink/src/snp/SNPModule.cpp:171-178`) and owns the
lobby — so "host = authority" introduces no new identity or discovery problem.

**Why not the relay model (argued in full under *Alternatives*):** the relay model's correctness
comes from a relay that owns the session clock and adjudicates hashes while being *no one's* player
(`rollback.md:134-162`). Standing that up means re-implementing something close to ShieldBattery's
`rally-point2` service (QUIC/`noq`, TLS with pinned roots, signed tokens, clock copy-merge, hash
meshing) plus a second client transport in the Rust DLL, and running that service forever. That is
the single largest possible amount of new infrastructure, and it is exactly what this hobby project
is trying to avoid. The host-authority model keeps **every** existing piece of CrownLink working
and adds only a message protocol and a thin export surface.

#### 1.1 Turn / ack / hash message model (concrete)

The rollback timeline is counted in **turns, not frames** (`rollback.md:28-32`): a step consumes
exactly one turn from every slot still in the game, so a slot's k-th in-game turn runs on step k on
every client. We keep that invariant verbatim.

- **Slot assignment:** by the authority (host) at lobby join. Slot id ≡ storm player id; host ≡ slot 0.
  This mirrors ShieldBattery's `storm id ≡ rp2 slot` mapping (`mod.rs:97-99`) and needs no new
  negotiation.
- **Seq assignment:** by the **sender**, per slot, monotonic, starting at 0 for the first in-game
  turn. The authority does not renumber turns; it only relays/echoes them and counts them. This
  matches ShieldBattery, where "seq assigned by driver, slot by relay"
  (`RECON.md:170`).

**Turn message (peer → all other peers), sent every step:**
```
Turn {
  slot: u8,          // assigned by authority (= storm player id)
  seq: u32,          // assigned by sender, per-slot monotonic
  commands: bytes,   // the turn's command buffer; native sync (0x37) already stripped, NOP substituted
  game_frame_count: u32,   // sender's frame when the turn left (for pacing/lead), optional
  state_hash: u64?,  // present on every 8th confirmed step (see below); 0 otherwise
}
```
`commands` is exactly what ShieldBattery submits in `submit_local_turn` after `ProcessGameCommands`
strips `0x37` and stands `NOP` (`0x05`) in for it (`rollback.md:93-96`;
`reference/ShieldBattery/game/src/bw/commands.rs:8,13`). A missing turn is the empty prediction
(`PREDICTED_TURN = &[]`, `input_table.rs:22`).

**Authority → peers (broadcast, lower cadence):**
```
LeadReport {  // every 12 steps, over the last 24, per slot
  median_lateness: u32, p90_lateness: u32, clock_stopped_ms: u32,
}
turns_complete {  // stamped on EVERY authority packet
  per-slot counts of turns the authority holds
}
LeaveDirective {  // finalized leave / desync eviction
  slot: u8, reason: u32, final_turn_count: u32,
}
```

**State-hash adjudication.** Clients hash every 8th confirmed step from step 8 and send the hash on
their next turn (`rollback/hash_reports.rs:16`, `state_hash.rs:18-79`). The authority judges a step
once every required slot has reported, or 5 s after the step became confirmable, and evicts a
diverging slot — or, with no majority (a 1v1 disagreement, an even split), voids the game
(`rollback.md:154-162`). Withholding a hash is treated as divergence: a slot that keeps sending
turns (96 past the step) without a report is named (`rollback.md:157-159`).

**Cadence and loss policy.** Turns are fire-and-forget datagrams: loss is absorbed by prediction and
rollback (the `InputTable` already handles missing/duplicate turns, `input_table.rs:205-264`). The
authority's `LeadReport`/`LeaveDirective`/`turns_complete` are reliability-critical and ride
CrownLink's existing `AdaptiveRedundant` strategy, which duplicates on both the direct P2P and relay
links when the link looks poor (`reference/CrownLink/src/snp/JuiceManager.cpp:108-142`). This is the
cheap, correct use of the redundancy CrownLink already ships.

**Reconnect.** The host is a single point of authority. In v1, a host drop ends the game (we apply a
finalized drop); a guest drop is finalized with a synced leave, exactly as ShieldBattery's finalized
drops (`rollback.md:86-89`). Host migration (copy-merge handoff) is explicitly deferred; it is the
one place the relay model is strictly better and is the first candidate for a future "single relay"
upgrade.

**Cheat resistance — the honest cost.** A guest cannot cheat the hash (the authority adjudicates and
evicts a diverging or withholding slot). The **host can**: it is both a player and the authority, so
it could lie about hashes, reorder or delay turns arbitrarily, or withhold its own report to frame
the guest. We accept this for v1 because it is already CrownLink's trust model (the host controls
the lobby and the P2P mesh) and because the audience is a small community, not an open ladder. The
fix is a relay, and the message model above is deliberately relay-compatible (seq by sender, slot by
authority, hash meshing) so a later relay can be dropped in without changing the client protocol.

**Lag / prediction implications.** The host plays at effectively zero added delay (it *is* the
authority); the guest absorbs its own RTT to the host as rollback + input delay, bounded by the
prediction limit (8). This is the standard P2P fighting-game tradeoff and is strictly better than
CrownLink's current lockstep, where the slowest link sets everyone's delay
(`rollback.md:12-14`). A host hitch pauses everyone, bounded by the clock-stand-still + stall-slack
rule we inherit from ShieldBattery (`rollback.md:136-140`), adapted so the *host* enforces it.

### 2. Where the DLL lives / hook ownership — **a new injected Rust DLL (`cosmo_rollback.dll`) owns the loop; CrownLink gains a thin "rollback channel"; plugins are reconciled by making the DLL the outermost hook**

We choose option **(a): a new injected DLL** (the `crates/cosmo-rollback` cdylib already scaffolded
at `crates/cosmo-rollback/`), injected by the **Fraud Launcher** alongside the existing QDP plugins.
CrownLink `.snp` is extended with a small versioned export surface so the Rust DLL can push/pull
rollback bytes over CrownLink's juice P2P; CrownLink does **not** own the loop.

Rejected options and why are in *Alternatives*. The decisive facts:

- The rollback algorithm exists only in Rust, is heavily tested (`input_table.rs:417-751` has 750
  lines of unit tests; `copier.rs:281-347`; `tick.rs`), and depends on SB's `whack` hook library.
  Extending the C++ `.snp` to own the loop means re-implementing all of that in C++23, and the
  `.snp` has no natural game-loop hook point anyway (it only ever sees the SNP SPI surface).
- The `.snp` is loaded by storm at provider-bind time and torn down at `spi_destroy`
  (`SNPModule.cpp:113-152`); its lifecycle does not line up with the game loop, and it cannot
  conveniently be the "outermost" trampoline around the plugins.
- A separate hotloader plugin would entangle rollback with the plugin patching order and make the
  multi-hook reconciliation *worse*, not better.

#### 2.1 The CrownLink "rollback channel" (transport seam)

Add a small `__stdcall`, 32-bit ABI to `CrownLink.snp`, versioned, so the Rust DLL does not depend
on storm's turn machinery (which is exactly the machinery rollback must replace — native sync hashes
state at send time on a predicted frame, `rollback.md:93-96`):

```c
// CrownLink.snp new exports (all __stdcall, i386):
u32 cl_rollback_abi(void);                              // bumps on ABI change
bool cl_rollback_send(u8 slot, const void* data, u32 len);          // send to one peer (juice P2P)
bool cl_rollback_broadcast(const void* data, u32 len);              // authority → all peers
bool cl_rollback_send_redundant(const void* data, u32 len);         // critical packets (lead/leave)
void cl_rollback_set_receive_cb(void(__stdcall* cb)(u8 slot, const void* data, u32 len));
```

`cl_rollback_send` maps to `JuiceManager::send_p2p` (`JuiceManager.cpp:205-210`);
`cl_rollback_broadcast` maps to `JuiceManager::send_all` (`JuiceManager.cpp:259-265`);
`cl_rollback_send_redundant` reuses the `AdaptiveRedundant`/`Redundant` strategy path
(`JuiceManager.cpp:104-114`). Inbound bytes arrive on `JuiceAgent::on_recv` and are routed to the
registered callback by sub-type, using the same mechanism as the existing custom-packet channel
(`JuiceManager.cpp:124-134`). The Rust DLL finds these via `GetProcAddress` at inject time, exactly
as the Fraud Launcher already consumes CrownLink's launcher extensions (`RECON.md:42-50`).

#### 2.2 Hooking the game-logic step (the multi-hook reconciliation problem)

The rollback engine must intercept the **single game-logic step** so that:

1. it runs on the first simulation of a frame,
2. a re-simulation re-runs it (so the plugins' simulation contribution is applied again),
3. the GPTP/QDP plugins continue to run, their hooks do not double-fire for once-per-frame
   side effects, and their order is deterministic.

ShieldBattery's structure answers (i) and (ii) directly: `StepGameLogic` is hooked to
`step_game_logic_hook` → `step_one_game_logic_step` → `rollback_live::run_game_logic_step`
(`RECON.md:145-147`), and `run_tick` re-invokes the hooked step for each re-simulated frame
(`tick.rs:140-186`). We replicate this structure at 1.16.1 addresses, with one critical ordering
rule:

**The rollback DLL must be the *outermost* trampoline on `StepGameLogic`.** Concretely, it must be
installed *after* the hotloader has patched the plugins in, so that one call into the rollback
trampoline means "simulate one step of BW-with-all-plugins-inside". On re-simulation the rollback
engine calls the same trampoline again, so the plugins re-run — which is *required* for their
simulation contribution to be reproduced. The ordering is what makes the plugins' hooks chain
inside the rollback boundary instead of around it.

**The double-fire problem is real and splits in two, each with its own mechanism:**

- **Simulation state** (plugin globals mutated by the step): solved by snapshot coverage, not by
  hook ordering — see §3. A plugin whose mutable state is fully snapshotted re-simulates correctly
  because a restore rewinds it before the step re-runs. This is the only thing that makes (ii)
  correct.
- **Side effects** (a sound, a network send, a file write, a UI popup the plugin fires from inside
  the step): these fire once *per simulation*, not once *per frame*, so a re-simulation fires them
  again. ShieldBattery already gates the engine's own such effects with ledgers
  (`rollback/{sounds,announcements,observer_ui,ui_writes}.rs`, `rollback.md:61-66`). The plugins need
  the same treatment, which is only possible if we can edit them.

Because the plugins are open source (`gitlab.com/the-no-frauds-club/tools`, per `RECON.md:3` note in
PORT-PLAN §3 and the prompt), the reconciliation is solved by **a light rollback contract baked into
the plugins at rebuild**, not by magic in the DLL:

```
// plugin contract (satisfied by rebuilding the open-source plugins):
1. Register every mutable plugin global/array with cosmo_rollback_register_range(desc) at load,
   so the snapshot covers it (§3).
2. Route every once-per-frame side effect through cosmo_rollback_once_per_frame(key, fn) — the DLL
   deduplicates it per frame across re-simulations (SB's ledger pattern).
3. Re-entrancy: a plugin hook that reads the local player's input must not depend on state that is
   viewer-local (camera/selection), which the snapshot deliberately excludes
   (ranges.rs:48-59).
```

For v1 we do **not** require all plugins to be rebuilt up front: we start with the engine snapshot +
re-simulation correct against a *read-only-in-simulation* plugin set, then use the audit (§3.4) to
find which plugin state must be registered. This keeps the first milestone (engine correctness)
decoupled from the plugin-rebuild work.

**Do the plugins' hooks double-fire or reorder?** With the rollback trampoline outermost, a plugin
hook that sits *inside* `StepGameLogic` fires exactly once per simulation, in the same order every
simulation. A plugin hook that sits *outside* the game-logic step (e.g. on a timer, a render tick,
or a network pump) is not re-run by rollback and is safe as long as it does not write simulation
state. The audit (§3.4) is what proves the boundary is where we think it is.

### 3. Snapshot boundary ownership (design-level) — **hardcoded 1.16.1 table for the engine, plus an instrumented-plugin registration API, plus a ported empirical audit as the verification net**

ShieldBattery snapshots a fixed list of `(address, length)` ranges (`ranges.rs:1-8`), resolved from a
*static binary analysis* (`samase_scarf`) into concrete addresses at `snapshot.rs::build`
(`snapshot.rs:281-432`). For 1.16.1 we do not need the analysis: the layouts are published and
stable, and — critically — ShieldBattery's own **x86** sizes (`Unit = 0x150`, `Player = 0x24`,
`Game = 0x17700`, `Pathing = 0x97a20`, …) are the classic 32-bit Brood War layout (`ranges.rs:31-92`,
`ranges.rs:64-77`). So we **seed** the 1.16.1 table from ShieldBattery's `#[cfg(target_arch = "x86")]`
struct sizes and *verify* every offset against a 1.16.1 memory map (BWAPI/OpenBW/PyMS), behind the
same `RangeSpec`/`RangeKind` interface (`ranges.rs:151-176`) so `snapshot.rs`, `copier.rs`, and
`tick.rs` transfer almost unchanged.

The **open, unsolved** part is plugin state: the `.qdp` plugins add mutable simulation state that no
1.16.1 map knows (`RECON.md:197-198`). Three candidate strategies, in the order we combine them:

1. **Hardcoded 1.16.1 address/struct table** (engine state) — *primary*. This covers everything the
   base engine mutates: the `Game` struct, `players[]`, all object pools (`units`, `bullets`,
   `sprites`, `images`, `orders`, `fow_sprites`, AI pools), the RNG block, terrain/tile arrays, the
   trigger lists, and the dozens of scalar globals ShieldBattery enumerates in
   `analyze_ranges` (`ranges.rs:353-795`). We transcribe that list once, substituting verified
   1.16.1 offsets, keeping the `checked_size` assertion pattern (`ranges.rs:805-813`) and the
   `EXCLUDED`/`VIEWER_LOCAL_GAME_FIELDS` reasoning (`ranges.rs:48-59`, `ranges.rs:181-207`).
2. **Instrument the plugins** (plugin state) — *secondary, since the plugins are open source*. Add a
   tiny registration API to `cosmo_rollback` (`cosmo_rollback_register_range(name, addr, len)` and a
   friend that registers a `PoolVector`-like header) and rebuild the plugins to call it for every
   mutable global/array they own. This is the *only* strategy that guarantees completeness for
   plugin state without scanning memory for the plugin's static allocations.
3. **Empirical discovery** — *verification net, not a source of truth*. Port ShieldBattery's audit
   harness: `SB_ROLLBACK_AUDIT_FRAME` (`rollback_harness.rs:84`, `audit_before_step` at
   `:553-589`) copies the whole executable data section before a chosen frame's first simulation and
   diffs it before every later simulation of that frame, reporting every word that changed *outside*
   the snapshot ranges. `SB_ROLLBACK_DUMP_FRAME` (`:466`) dumps the full ranges + layout to disk for
   bisection, and `SB_ROLLBACK_EXTRA_RANGES` (`ranges.rs:24-25`) lets us add candidate ranges as
   `<exe-offset>+<len>` without recompiling. For heap/plugin state the `.data`-section diff does not
   reach, we add a **delta-of-two-identical-runs** comparison: run the same seeded session twice
   (or once with `SB_ROLLBACK_SHADOW` forcing constant re-simulation, `rollback_live.rs:28-29`) and
   diff the two simulations' full state fingerprints frame by frame, which surfaces any byte a
   re-simulation did not reproduce regardless of where it lives.

**Ordered plan to build and verify the range set:**

1. Transcribe ShieldBattery's `analyze_ranges` list into a 1.16.1 `RangeSpec` table, seeding sizes
   from SB's x86 constants, verifying each against a 1.16.1 map. Add a `size_of`/`offset_of` assert
   per field (the `ranges.rs:31-92` pattern) so a wrong offset fails at load, not mid-game.
2. Get a 1v1 re-simulation runnable *without* any plugin that owns sim state (or with plugins
   marked read-only). Confirm the engine-only snapshot is complete via the audit harness (no
   diverging words outside ranges).
3. For each plugin that owns sim state, add the registration calls and re-enable it; re-run the
   audit + delta-of-two-runs after each plugin until the diff is clean.
4. `rollback_live::supported()` analogue (`rollback_live.rs:508-516`): refuse a rollback session if
   any required range failed to resolve or any required plugin did not register — never fall back to
   lockstep, which would desync against rollback peers (`session.rs:178-180`).

**Detecting a missed range** is exactly step 3's net: a missed range shows up as (a) a non-covered
word in the data-section audit, or (b) a frame where two runs' fingerprints diverge. The state hash
(`state_hash.rs:18-79`) then catches it in live games as a desync, and the authority's eviction names
the divergence.

We deliberately do **not** adopt page-watching / dirty-bit tracking as a primary mechanism
(`rollback.md:268` notes ShieldBattery measured leaving never-used pool entries out via page
watching at ~17% and found it "fragile"). At Cosmonarchy's uncapped unit counts the pool is large
and *mostly* live, so the fragile optimization buys little; we snapshot whole pools like ShieldBattery
does (`snapshot.rs:346-362`).

### 4. Module plan for the new crate

**Fork / adapt from ShieldBattery (algorithm transfers, largely unchanged):**

| Source (SB `game/src/`) | Destination (`crates/cosmo-rollback/src/`) | Notes |
|---|---|---|
| `rollback/copier.rs` | `rollback/copier.rs` | verbatim; `SLOT_ALIGN`/`CHUNK_BYTES`/`MAX_HELPERS` (`copier.rs:15-27`) are arch-agnostic |
| `rollback/tick.rs` | `rollback/tick.rs` | verbatim; `DEFAULT_SNAPSHOT_SPACING`=3, irreversible-step barriers (`tick.rs:18,171-173`) |
| `rollback/snapshot.rs` | `rollback/snapshot.rs` | adapt `build` to the hardcoded table; `TriggerLists`/`ReplayCursor` keep (node size `TRIGGER_SIZE` becomes a 1.16.1 constant) |
| `rollback/state_hash.rs` | `rollback/state_hash.rs` | same FxHash; retarget field names/offsets to the 1.16.1 struct layer |
| `rollback/ranges.rs` | `rollback/ranges.rs` | keep `RangeKind`/`RangeList::make_disjoint` (`:284-301`); replace `analyze_ranges` with the hardcoded 1.16.1 table + plugin registrations |
| `netcode_v2/input_table.rs` | `transport/input_table.rs` | verbatim (transport-agnostic, keyed on storm id); keep the 750-line test suite |
| `rollback_live.rs` | `rollback_live.rs` | adapt the driving loop (`run_game_logic_step`, `:571`) and `arm_for_session` (`:521`); the `with_turn_state` calls become our own `TurnState` fed by the CrownLink channel |
| `rollback/pacing.rs` | `rollback/pacing.rs` | adapt the lead-report *source* (host authority instead of relay); the split law is unchanged |

**Rewrite (must be redone for 1.16.1 / CrownLink):**

- **`bw161/`** — the 1.16.1 struct layer (`Game`, `Player`, `Unit`, `Bullet`, …) and the hook
  installers (`StepGameLogic`, `SendTurnMessage`, `ReceiveStormTurns`, `FlushLocalTurns`,
  `ProcessGameCommands` 0x37-strip). Replaces `bw_scr.rs`'s `samase_scarf` analysis with a
  hardcoded table (§3.1) and `whack`-based patches at 1.16.1 addresses.
- **`transport/`** — replaces `netcode_v2/mod.rs`+`session.rs` (QUIC/relay). A `TurnState` +
  `LinkDriver` analogue whose `outbound`/`inbound` ends are the CrownLink channel exports (§2.1)
  instead of Tokio QUIC channels. No Tokio runtime, no TLS, no relay dial.
- **`once_per_frame/`** — the ledger set (`sounds`, `announcements`, `ui_writes`) retargeted at
  1.16.1 effects, plus the plugin `once_per_frame` shim (§2.2).
- **`arming/`** — the Fraud Launcher handoff: parse the launch config (host/guest, slot ids, peer
  list, rollback on/off) from a CrownLink extension (`set_rollback_config`) and the host's
  `AdFile`, replacing SB's app-websocket `NetcodeV2Setup` (`session.rs:173-180`).

**Crate / ABI boundaries:**

- `cosmo_rollback.dll` — `cdylib`, `i686-pc-windows-msvc`, `panic = "abort"`, `lto = "thin"`
  (already configured in `crates/cosmo-rollback/Cargo.toml` / workspace `Cargo.toml`). Entry point
  `OnInject` (SB `lib.rs:216-293` analogue) + `cosmo_rollback_abi()` (scaffolded at
  `crates/cosmo-rollback/src/lib.rs`).
- CrownLink↔DLL seam — the `__stdcall` exports in §2.1, versioned on both sides. The Rust DLL does
  **not** link CrownLink; it resolves the exports at inject time so the two can version independently.
- Everything the engine needs is resolved from the hardcoded 1.16.1 table; there is no dependency on
  SB's D3D build (`PORT-PLAN.md:53-59` documents why SB's `game` crate cannot cross-build, and why
  we don't need to).

**Minimal v1 surface — confirmed: 1v1 human-vs-human melee, no observers, no UMS, no subfactions, no AI.**

Rationale, in increasing cost:
- **1v1** avoids the majority-vs-minority hash-attribution problem entirely (a 1v1 disagreement is
  "no majority → void the game", `rollback.md:154-162`); it also minimizes the input table, pacing,
  and eviction surface.
- **No observers** removes observer turns from the clock (`rollback.md:164-165`) and the
  observer-UI ledger (`observer_ui.rs`) — a meaningful chunk of `bw_scr.rs`.
- **No UMS** avoids arbitrary trigger/EUD memory writes that the snapshot can't cover
  (`rollback.md:170-173`). Melee still needs the *built-in* melee trigger lists, which the snapshot
  already handles (`TriggerLists`, `snapshot.rs:33-57`).
- **No subfactions** avoids the RNG/upgrade edge cases the mod adds; the mod's removal of randomness
  (`RECON.md:1` note, "removes randomness") is a determinism *asset* we lean on.
- **No AI** defers the AI-player ranges (`player_ai`, AI pools, AI region reachability —
  `ranges.rs:601-721`), which is the largest single block of the table we can skip for 1v1 melee.
  Human-only 1v1 still needs `selection`/`hotkeys` ranges for the two humans.

This is the confirmed target; observers, subfactions, and AI are the explicit next milestones.

### 5. Risks and kill criteria

| # | Risk | Likelihood | Kill criterion / earliest experiment |
|---|---|---|---|
| R1 | A missed snapshot range (engine or plugin) → silent desync every game. | High — this is *the* failure mode of the whole project. | **Snapshot-completeness audit on the real exe** is the first experiment (§3.4). If `SB_ROLLBACK_AUDIT_FRAME`-style diffs can't be driven to zero diverging words on the real patched 1.16.1 exe, the project is dead. |
| R2 | GPTP re-simulation is not deterministic (a plugin reads wall clock, TSC, or a non-snapshotted counter). | Medium-high. | Delta-of-two-runs (§3.3) on a plugin-heavy 1v1. If two identical runs diverge on a plugin frame, isolate the plugin; if it can't be made deterministic, rollback is infeasible for Cosmonarchy. |
| R3 | The outer-trampoline ordering can't be enforced (hotloader re-patches over us, or a plugin wraps `StepGameLogic` in a way we can't sit outside). | Medium. | Prove on the real exe that one injected trampoline sees "1 call = 1 step = all plugins inside", and that re-invoking it reproduces a known state delta. |
| R4 | Snapshot/restore cost at uncapped unit counts exceeds a frame budget (42 ms at Fastest). | Medium. | Bench `copier` on the real exe at Cosmonarchy's max unit count; SB's copier moves megabytes in well under a frame (`rollback.md:56-59`), but verify at Cosmonarchy scale. If a snapshot+restore exceeds ~half a frame, kill. |
| R5 | Host-authority is too laggy/unfair for guests (host at 0 delay, guest eats RTT) or too cheat-prone. | Medium (acceptability, not correctness). | Playtest a real 1v1 at RTT 80–150 ms. If guests find it unplayable, the host-authority model is killed and we pay for the relay. |
| R6 | CrownLink's juice channel reorders/duplicates turns in a way that breaks seq tracking or floods the input table. | Low-medium. | Stress test the channel; the `InputTable` already dedupes by seq and tolerates reorder, but confirm against CrownLink's actual delivery. |
| R7 | Fraud Launcher integration: injection + arming + packaging can't coexist with the existing `CrownLink.snp` drop. | Low. | Early spike: launcher injects `cosmo_rollback.dll` and the DLL resolves `cl_rollback_abi()`. |

The **single earliest kill experiment** is R1: port the audit harness and run it on the real
patched `Cosmonarchy BW.exe`. Every other milestone is contingent on it. If we cannot prove snapshot
completeness on the engine *before* any plugin is involved, we stop before spending any effort on
the transport or the launcher.

---

## Consequences

### Positive

- **No new server.** `crow_serve` and the existing P2P mesh are reused as-is; the whole transport
  delta is a handful of `__stdcall` exports in `CrownLink.snp` and a Rust `transport/` module.
- **The tested algorithm transfers nearly whole.** `copier`, `tick`, `snapshot`, `input_table`,
  `state_hash` come over with their tests; only the analysis layer and transport are rewritten.
- **1.16.1 is easier than SC:R for the table**, because the layouts are published and ShieldBattery's
  own x86 sizes seed the struct layer (`ranges.rs:64-77`).
- **Desync detection now exists** where CrownLink had none — even a 1v1 that can only *detect* (not
  attribute) a desync is a strict improvement over silent divergence.
- **Deterministic upgrade path.** The message model is relay-compatible, so a future single relay
  drops in without protocol changes.

### Negative

- **Host can cheat, and host lag becomes everyone's session clock.** Accepted for a small community;
  this is the direct price of "no server."
- **Host is a single point of failure** — host drop ends the game; no mid-game host migration in v1.
- **Reconnect is weaker than the relay model's copy-merge**; guest reconnect (finalized drop) is the
  only reconnect we ship.
- **Plugin instrumentation is new work the reference never had** — ShieldBattery snapshots only the
  engine. If a plugin proves non-deterministic (R2), the whole approach fails; this is the least
  precedented part of the port.
- **The `.qdp` plugins must be rebuilt** to satisfy the rollback contract, which depends on the
  plugin sources staying available and buildable.

---

## Alternatives considered

### A. Relay-authority (ShieldBattery's model), with a single self-hosted relay

Keep the relay that owns the session clock and adjudicates hashes (`rollback.md:134-162`). **Rejected**
for v1: it is the most infrastructure by a wide margin — a QUIC/`noq` + rustls/ring relay service
(`RECON.md:161-163`), signed-token auth, clock copy-merge, hash meshing, and a second client
transport in Rust replacing CrownLink's P2P for turns. That is a re-implementation of `rally-point2`,
and it must be run forever. It is the correct *end-state* and the upgrade path the message model
preserves, but it is not "least new infrastructure for a hobby project."

### B. Extend `CrownLink.snp` so it also owns the loop

Rejected: the rollback algorithm is Rust and heavily tested (`input_table.rs:417-751`,
`copier.rs:281-347`); the `.snp` is C++23 with no game-loop hook point (it only sees SNP SPI calls,
`SNPModule.cpp:354-383`) and a lifecycle tied to storm's provider init/destroy
(`SNPModule.cpp:113-152`). Owning the loop from the `.snp` means re-writing the engine in C++ *and*
inventing a hooking/audit harness from scratch. We keep the `.snp` as transport (§2.1) but not as
the loop owner.

### C. A separate hotloader plugin (QDP) owning the loop

Rejected: it entangles rollback with the plugin patching order, making the outer-trampoline
guarantee (§2.2) harder to state and enforce, and couples rollback's lifecycle to the hotloader's.
A separately-injected DLL whose injection the launcher controls is strictly cleaner and mirrors
ShieldBattery's `game` DLL.

### D. Route rollback turns through storm's own turn path (no new CrownLink exports)

Rejected: storm's turn machinery is exactly what rollback must replace — native sync (0x37) hashes
state at send time on a predicted frame and drops peers that lack it (`rollback.md:93-96`), and the
512-byte `GamePacketHeader`/latency-buffer framing is storm's, not ours (`RECON.md:63-67`).
Piggybacking would reintroduce the prediction-vs-sync contradiction. The thin channel (§2.1) is the
minimal alternative that keeps CrownLink's transport while giving rollback its own wire.

### E. Snapshot via page-watching / dirty-bit only (no hardcoded table)

Rejected as a *primary* mechanism: ShieldBattery measured it and called it fragile
(`rollback.md:268`); whole-pool snapshots are simpler and correct, and at Cosmonarchy's uncapped
unit counts the "skip never-used entries" win is small. It survives only as a diagnostic idea inside
the audit.

### F. Broad v1 scope (observers, UMS, subfactions, AI)

Rejected in favor of 1v1 human melee (§4): each addition pulls in a large block of table/ledger work
(observer turns + observer-UI ledger; EUD-trigger coverage; subfaction RNG; AI-player ranges
`ranges.rs:601-721`) for no path to the first working game.

---

## Open questions

1. **Plugin determinism (R2) is unproven.** Which GPTP/QDP plugins read the wall clock, TSC, or a
   non-snapshotted counter, and can they be made deterministic? This is the top open risk; the
   delta-of-two-runs experiment is the arbiter.
2. **Host-authority fairness/cheat ceiling.** At what RTT does the guest's input delay become
   unacceptable, and how much of the cheat surface can a cheap non-authority mitigation (e.g. both
   peers hashing, host audited by the guest's own hash log) close before we need a relay?
3. **Plugin rebuild / arming path.** Can we actually rebuild the shipped `.qdp`s (open source, but
   compiled artifacts ship), and how does the Fraud Launcher pass the rollback config through
   `CrownLink.snp`'s extensions without breaking the existing launcher? This is the R7 spike.

---

## Validation / next experiments

In dependency order; the first is the kill gate.

1. **Snapshot-completeness audit on the real exe (R1).** Port `SB_ROLLBACK_AUDIT_FRAME` +
   `SB_ROLLBACK_DUMP_FRAME` (`rollback_harness.rs:84,466,553`) onto the patched 1.16.1 binary; drive
   the engine-only diverging-word count to zero. If it cannot reach zero, stop.
2. **Determinism delta (R2).** Run the same seeded 1v1 twice (or once with `SB_ROLLBACK_SHADOW`,
   `rollback_live.rs:28-29`); diff per-frame fingerprints. A divergence isolates the offending plugin.
3. **Outer-trampoline proof (R3).** On the real exe, confirm one injected call = one step with all
   plugin hooks inside, and that re-invoking it reproduces a known state delta.
4. **Copier bench at Cosmonarchy scale (R4).** Snapshot+restore cost at the mod's max unit count
   vs the 42 ms frame budget.
5. **CrownLink channel stress (R6).** Turn-stream reorder/duplicate/loss behavior against the
   `InputTable`'s seq handling.
6. **Playtest at RTT 80–150 ms (R5).** Judge host-authority playability and set the accept/kill bar
   on the relay question.
7. **Launcher spike (R7).** Fraud Launcher injects `cosmo_rollback.dll`; the DLL resolves
   `cl_rollback_abi()`; `set_rollback_config` flows host/guest/peer-list without disturbing the
   existing `CrownLink.snp` drop.
