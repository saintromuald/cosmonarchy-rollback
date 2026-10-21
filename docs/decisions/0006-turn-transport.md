# ADR-0006 — Rollback turn transport (implementation of ADR-0001 §2.1)

- **Status:** Accepted (implemented; not yet validated in a live match)
- **Date:** 2026-10-18
- **Refines:** ADR-0001 §1.1 (message model) and §2.1 (the CrownLink channel)
- **Grounding:** `saintromuald/CrownLink` (fork of `impromptu1583/CrownLink`),
  `src/snp/RollbackChannel.h`, `src/snp/{JuiceManager,JuiceAgent,DLLMain}.cpp`,
  `src/snp/CrownLink.def`; `crates/cosmo-rollback/src/{transport,turns,session}.rs`;
  `reference/CrownLink/src/snp/{SNPModule,JuiceManager,JuiceAgent}.cpp`;
  `reference/ShieldBattery/game/src/netcode_v2/`.

---

## Context

ADR-0001 §2.1 designed a small `__stdcall` export surface on `CrownLink.snp` so the rollback
DLL can exchange turns/hashes over CrownLink's existing juice P2P instead of storm's lockstep
turn machinery. This ADR records how it was actually built, the two places the implementation
deviates from the sketch, and what could not be validated.

## Decision

**1. The channel is a CrownLink custom packet with a dedicated sub-type.**
Rollback messages travel as `GamePacketSubTypeRollback` packets, not as game packets, so they
never reach the engine's lockstep receive queue (`spi_receive` would otherwise hand them to
the game as turns). `JuiceAgent::on_recv` intercepts that sub-type and calls the registered
callback instead of enqueuing. The sub-type value is **15**, taken as a raw
`static_cast<GamePacketSubType>(15)` rather than added to storm_sdk's enum: storm_sdk is an
external FetchContent dependency (`gitlab.com/Unveiling1561/storm_sdk`) whose enum ends at
`Messages = 14`, so adding a value would mean forking it. `RollbackChannel.h` documents that
this must move if storm_sdk ever allocates 15.

**2. The exported ABI, with two deviations from the ADR sketch.**

```c
u32  __stdcall cl_rollback_abi(void);                        // 1
void __stdcall cl_rollback_set_receive_cb(void(__stdcall*)(const NetAddress*, const void*, u32));
int  __stdcall cl_rollback_broadcast(const void* data, u32 len);        // fire-and-forget
int  __stdcall cl_rollback_send_redundant(const void* data, u32 len);   // P2P + relay
```

- **The callback is handed the sender's `NetAddress`, not a storm slot id.** CrownLink's only
  peer identity is `NetAddress`; it has no storm-slot table, so the DLL maps the address to a
  slot itself (it sees the same address on the engine's own receive path).
- **There is no per-slot send.** v1 is 1v1, so a broadcast reaches exactly the one peer. A
  per-peer send would need the slot↔address mapping CrownLink does not have.

Both deviations are recorded in `src/snp/RollbackChannel.h` in the fork.

**3. Send strategy.** `cl_rollback_broadcast` sends one copy over whichever agent the pair
considers best (loss is absorbed by rollback prediction);
`cl_rollback_send_redundant` duplicates over both the direct P2P link and the relay, which is
what the ADR wants for the reliability-critical lead/leave messages.

**4. The client side (`transport.rs`) resolves the ABI lazily** by `GetProcAddress` on the
loaded `CrownLink.snp` — no link-time dependency, exactly like the plugin serializers. It
verifies `cl_rollback_abi` before using anything, registers the receive callback, and decodes
incoming datagrams into a queue (`Msg::decode` is total: short/mis-versioned/oversized input
is dropped, never fatal).

**5. The wire format is version 2** (`Msg::Turn`, `Hash`, `Leave`, `LeadReport`,
`TurnsComplete`). v2 added the reporting `slot` to `Hash` — v1 was ambiguous about which peer
a hash came from — and the authority's two report messages.

**6. The input table is sequence-aware** (`InputTable::push_seq`). CrownLink custom packets are
fire-and-forget, so turns can arrive out of order, twice, late, or not at all. `push_seq`
places a turn by the sender's sequence number, drops duplicates, fills a gap with
predicted-empty turns (corrected when the turn arrives late → a misprediction), and refuses a
gap beyond `MAX_SEQ_GAP` (4096) so a desynced or hostile peer cannot allocate unboundedly.
`push` remains the in-order special case.

**7. Desync hashes are compared on arrival, for confirmed frames only.** A peer's hash normally
arrives after we settled the frame, so `peer_hash` compares against our own recorded hash the
moment it arrives. `report_hash(frame, hash)` records our hash only for a frame that is
`confirmed`: hashing a frame still running on a prediction reports a false desync.

**8. The host adjudicates** (`session::Authority`): reports are collected per step and judged
once every required slot has reported — `Agreed`, `Evict(slot)` for the minority (whichever
side that is), or `Void` on an even split (a 1v1 disagreement).

**9. Turns are acknowledged and re-sent — ADR-0001 §1.1's "loss is absorbed by prediction and
rollback" is wrong on its own.** Prediction and rollback recover a turn that arrives *late*;
they cannot recover one that never arrives, and libjuice is a plain-UDP transport with **no
reliability API** (`juice.h` has no `juice_set_reliable`/`juice_set_ordered`; CrownLink
implements acks/resends itself for the engine's turn stream via `GamePacketFlags`). A
fire-and-forget rollback turn lost in transit would therefore diverge the clients
permanently. So:

- `Msg::Ack { slot, seq }` carries the highest turn sequence number a slot has received
  **contiguously**;
- `Session::ack_peers` sends one per remote slot (`known_until_slot(slot) - 1`);
- `Session::send_local_turn` remembers the local turn, and `Session::resend_unacked` re-sends
  every local turn above the peer's ack (a bounded window of `SENT_WINDOW = 64`). The caller
  runs the re-send on a cadence.

## Consequences

### Positive
- No change to storm or to the engine's turn path; the engine's lockstep is untouched.
- The `.snp` is buildable in CI (`saintromuald/CrownLink`, `build-crownlink` workflow,
  windows-latest, MSVC x86 + Ninja + Python): **verified, 2m16s**, producing a valid
  `CrownLink.snp` that exports all four `cl_rollback_*` functions.
- **Non-breaking, verified**: with the patched `.snp` installed the game boots and plays a
  replay to completion.
- **Resolution verified in-game.** Because storm only loads the `.snp` at provider-bind time,
  a replay has no SNP module at all; loading the patched `.snp` explicitly
  (`CM_ROLLBACK_CHANNEL_LOAD=1`) and probing at the first frame gives:

  ```
  channel probe: LoadLibraryA("C:\Cosmonarchy\Starcraft\CrownLink.snp") -> ok
    module CrownLink.snp            base 0x79aa0000 size 0xe7000
  CrownLink rollback channel: resolved (abi 1)
  ```

  i.e. the DLL finds the module, `cl_rollback_abi` returns the version we expect, and the
  receive callback registers.
- The client stack is unit-tested without the game: out-of-order/duplicate/gap/bogus turns,
  hash exchange over the real codec between two clients, and authority adjudication.

### Negative / unverified
- **The channel is not exercised in a live match.** In the headless replay harness storm never
  binds a provider, so `CrownLink.snp` is not loaded at all (the probe correctly reports
  `not available`). The resolution path is verified as above, but sending/receiving between two
  real clients needs a live multiplayer session.
- The per-slot send and the storm-slot↔address mapping are absent (fine for 1v1; needed for
  >2 players).
- Custom packets are capped at `MAX_PAYLOAD_SIZE` (500 B), which bounds a turn's command blob.
- `CrownLink.snp` must be rebuilt and shipped for the channel to exist; the stock `.snp` has
  no `cl_rollback_*` exports.
