# cosmonarchy-rollback

Porting **rollback netcode** (GGPO-style prediction + re-simulation) into
[Cosmonarchy BW](https://fraudsclub.com/cosmonarchy-bw/) — a StarCraft: Brood War
**1.16.1** total-conversion mod — so that matches played over **CrownLink** use rollback
instead of pure deterministic lockstep.

The rollback engine is modelled on [ShieldBattery](https://github.com/ShieldBattery/ShieldBattery)'s
MIT-licensed `game/src/rollback` + `game/src/netcode_v2` (which target StarCraft: Remastered).
Cosmonarchy is a patched 1.16.1 exe with injected GPTP/QDP plugins, so this is a
reverse-engineering + engine-hook port, **not a drop-in**.

## Cosmonarchy Plus (branch `cosmonarchy-plus`)

The gameplay fork built on top of the rollback client: QoL overlays, placement rules and
its own data/balance identity. Because it changes the simulation, Plus clients only play
other Plus clients of the same build. Plan, feasibility matrix and phased roadmap:
[`docs/COSMONARCHY-PLUS.md`](docs/COSMONARCHY-PLUS.md).

## Layout

```
reference/            upstream code studied for the port (git submodules, read-only)
  ShieldBattery/      rollback + netcode_v2 reference (targets SC:R)
  CrownLink/          Cosmonarchy's SNP provider (P2P/WebRTC transport)
docs/
  RECON.md            grounded map of both references + the Cosmonarchy binary
  PORT-PLAN.md        milestones, architecture, flash/pro work split
  decisions/          architecture decision records (ADRs)
crates/cosmo-rollback/  the Cosmonarchy-side 32-bit Windows QDP plugin (Rust)
tools/                  analysis / harness / packaging scripts (build, install, gate,
                        headless probe, disassembler, log summary, unit-test runner)
```

## Key facts

- **Game client:** `~/Games/cosmonarchy/prefix/drive_c/Cosmonarchy/Release/Cosmonarchy BW.exe`
  (patched 1.16.1, PE32/i386), plugins in `.../Release/plugins/*.qdp`.
- **Base engine:** `.../Starcraft/StarCraft.exe` + `storm.dll` + `standard.snp`,
  `battle.snp`, `CrownLink.snp`.
- **Transport today:** CrownLink (SNP ABI, P2P via libjuice/WebRTC, crow_serve lobby,
  TURN fallback). No state hashes, no desync detection, no rollback.
- **Rollback reference:** ShieldBattery `game/` — snapshots via a fixed `(address,len)`
  range list resolved by static binary analysis; input table with prediction; per-tick
  state hash adjudicated by a relay.

See `docs/RECON.md` and `docs/PORT-PLAN.md`.

## Playing with someone else

`dist/rollback-client.zip` is the player package: release plugin, rollback-capable
`CrownLink.snp`, host/guest `cosmo_rollback.conf` files, checksums and a README with the exact
install and verification steps. Both players need the same files and the same Cosmonarchy
build. If only one side is armed, the armed client detects it after 3 s and falls back to normal
lockstep instead of stalling the match.

## Arming an install

`tools/arm-rollback.sh [Build] [CrownLink.snp]` does the whole setup and verifies it: builds the
release plugin, installs it, installs the rollback-capable `.snp` (refusing one without the
rollback exports) and checks that the `.snp` carries its caps archive — without that archive
Storm never lists CrownLink in the multiplayer menu.

## Running a rollback match

The plugin is a QDP that the Cosmonarchy hotloader loads alongside the others. It is **inert
unless armed**, so an unmodified game behaves exactly as before.

1. **Install a rollback-capable transport.** The stock `CrownLink.snp` has no rollback channel;
   build one from the `build-crownlink` workflow in `saintromuald/CrownLink` and install it:

   ```
   tools/install-crownlink.sh path/to/CrownLink.snp     # keeps a backup; refuses a stock .snp
   ```

   Use the **`dist/CrownLink.snp` the workflow uploads** (it has `caps.mpq` appended). A
   `.snp` without that archive still loads, but Storm never enumerates the provider, so the game
   falls back to the stock `Select Connection` list and CrownLink never appears. The installed
   file is 931,817 B with the caps archive; the bare linker output is 930,304 B.

2. **Arm the client** for a *multiplayer match* — either in the launch environment

   ```
   CM_ROLLBACK_LIVE=1 CM_ROLLBACK_SLOT=0   # 0 = host, 1 = guest
   ```

   or in `cosmo_rollback.conf` next to the game (see `cosmo_rollback.conf.example`). Both
   players must run the same plugin and a rollback-capable `.snp`. **Do not arm it for a
   replay or single-player game** — the client waits for a peer.

3. **Both players' logs** are `cosmo_rollback.log` in the game's directory: look for
   `live: CrownLink rollback channel bound`, `live: transport CrownLink (engine applies turns)`,
   `live: game started`, `live rollback #` and `live: hashes agree`; a `live DESYNC` means the
   two simulations diverged.

**Verifying without a second machine:** `tools/crownlink_match.sh` drives a real CrownLink lobby
with two clients on one machine (host + join + start) and `... report` prints the pass/fail
summary:

```
CL_MAP="Pagan" tools/crownlink_match.sh run a :96 ~/Games/cosmonarchy-agent2 \
    b :95 ~/Games/cosmonarchy-agent3 alpha
tools/crownlink_match.sh report
```

A passing run reports the same carrier on both clients, non-zero `sent`/`recv`, rollbacks, at
least one `agreed` hash and `desyncs=0`. On one machine the RTT is ~1 ms, so nothing ever
mispredicts; set `CL_SEND_DELAY_MS=150` to delay the client's own channel messages and make the
prediction/rollback path run for real (`live: tick … rolls …` in the log). The harness path (lobby + game start) is scripted with
`tools/ui_session.sh` (persistent session) and `tools/ocr_row.py`/`ocr_text.py` (read a list row
or panel with Tesseract, because the public games list reorders under the cursor).

`tools/two_client_test.sh` is the older UDP-carrier soak. It starts each client from a replay,
so it depends on that replay loading cleanly (this install's `LastReplay.rep` crashes `gptp.qdp`
on one of the trees) and reports `INCONCLUSIVE` when the clients start different games.

**The gate** (snapshot/restore completeness) runs headlessly: `tools/gate.sh Release 3 90`.


## Status

- Recon complete (`docs/RECON.md`).
- Architecture: ADR-0001 (host-authority over CrownLink), ADR-0002 (snapshot
  ranges + audit harness), ADR-0003 (QDP/samase plugin integration + hook boundary),
  ADR-0004 (plugin re-sim safety), ADR-0005 (gptp state serialization).
- `crates/cosmo-rollback` builds a loadable 32-bit Windows QDP plugin exporting
  `GetMPQDraftPlugin`/`Initialize`; contains the 1.16.1 range table, snapshot/hash
  core, and the audit harness.
- Shadow gate runs headlessly (`tools/gate.sh`). Object pools are resolved at runtime
  (Cosmonarchy's gptp relocates them with the CCMU extender); the `rng_seed` and
  `Game.elapsed_seconds` divergences are cleared.
- **Frame boundary fixed.** The hook wraps `0x004D9670` (`run_game_tick`); its wall-clock
  catch-up back-edge (`0x004D9823`) is NOPed so one call is exactly one logic frame
  (`docs/analysis/hook-boundary.md`).
- **Netcode core implemented and unit-tested** (run under wine by `tools/test.sh`, 25
  tests): `turns.rs` (input table with prediction), `tick.rs` (tick scheduler),
  `session.rs` (client state machine, incl. a two-client convergence test through real
  mispredictions/rollbacks), `transport.rs` (wire format + loopback link),
  `state_hash.rs` (pointer-free desync hash).
- **Turn path measured** (`docs/analysis/turn-path.md`): in the replay harness one frame
  is one turn boundary and the scheduler never stalls, so `CM_ROLLBACK_FORCE_TURN` is a
  no-op there. Turn capture (`0x00485A40`) + injection (`ProcessCommands` `0x004865D0`) +
  stall suppression (`0x004D9550`) are the next engine-facing step.
- **Open (essentially closed):** the shadow gate ran at **37 divergences**; it is now **~3,
  all AI-only, on ~2 of ~135 frames**. The causes found and fixed, in order: the re-sim not
  re-running `receive_network_messages` (`0x00485F70`), render-written pool fields in the
  hash, the un-snapshotted `UnitPath`/`AiScriptArray`/`AiTownArray`/`GuardAi` pools, and one
  outer-written `Unit+0xCA` byte (now restored but excluded from the hash as `units.render`).
  The AI residual is `aise`'s unsnapshotted heap state (Layer B) — **out of v1 scope**
  (ADR-0001 §4 defers AI; v1 is 1v1 human melee). Every non-AI range reproduces.
  See `docs/analysis/restore-residual.md`.
- Plugin-state serializers: Layer C + Phase 2 are built and version-matched (CI artifact in
  `saintromuald/cosmonarchy-gptp`); Layer B (`aise`) is locally buildable and prepared.
  They are **not** on the critical path for the gate.

