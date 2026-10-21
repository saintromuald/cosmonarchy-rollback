# RECON — grounded map of the port target

All findings below carry `file:line` anchors. References are git submodules pinned at:

| Reference | Commit | What it is |
|---|---|---|
| `reference/ShieldBattery` | `473477d` | Rollback + netcode_v2 engine (targets **StarCraft: Remastered**, x86 + x86_64) |
| `reference/CrownLink` | `12be58c` | Cosmonarchy's SNP provider (32-bit, MSVC, P2P/WebRTC + crow_serve lobby) |

---

## 1. The Cosmonarchy client (what we must hook)

Path: `~/Games/cosmonarchy/prefix/drive_c/Cosmonarchy/`

- `Release/Cosmonarchy BW.exe` — PE32/i386, 5.1 MB, 4 sections, sha256
  `6a7413a2…7dac`. This is the patched 1.16.1 engine the game runs.
- `Starcraft/StarCraft.exe` — PE32/i386 stock 1.16.1, sha256 `ad6b58b2…c6a46`.
- `Starcraft/storm.dll` — Storm (Battle.net/SysOp/SNP) runtime, PE32.
- `Starcraft/{standard,battle}.snp` — built-in providers; `Starcraft/CrownLink.snp` —
  CrownLink's provider (also PE32/i386).
- `Release/plugins/*.qdp` — GPTP/plugin injection: `gptp.qdp` (4.9 MB), `aise.qdp`,
  `mtl.qdp`, `vx4ex.qdp`, `fastmenus.qdp`, `isdbg.qdp`, `aize_debug.qdp`,
  `_qdp-hotloader.qdp`. These are loaded by the **hotloader** (`hotloader.log`), i.e. the
  exe is patched at runtime, not statically, for plugin code.
- `Release/mpq/` — the mod's own data overlay (`arr/units.datex` etc.), loaded over the
  base `*.mpq` in `Starcraft/`.
- `Release/.git` exists (the Fraud Launcher build clone; `mine` remote → GitHub snapshot).

**Implication:** the "engine" is 1.16.1 (a very well-known binary to the BW modding
community), plus runtime plugin patches whose mutable state is *not* in any published
memory map. Snapshot completeness (§3) is therefore the central problem.

---

## 2. CrownLink (current transport) — from `reference/CrownLink`

CrownLink is a **transport only**. It implements storm's SNP ABI and ships game turns
verbatim; it does **not** touch simulation state, does **not** compute or check state
hashes, and has **no desync detection**.

### 2.1 SNP surface
- Exports 12 symbols (`src/snp/CrownLink.def:1-13`): the real ABI pair `SnpBind`
  (`DLLMain.cpp:24-31`) and `SnpQuery` (`DLLMain.cpp:10-22`); ordinals 3–12 are
  launcher extensions (`version`, `register_status_callback`, `set/get_turns_per_second`,
  `set/get_password`, `get_status`, `set_status_lobby`, `set_map_name_edit`,
  `iterate_lobbies`) consumed by the Cosmonarchy launcher/overlay via `GetProcAddress`.
- SPI table built at `src/snp/SNPModule.cpp:354-383` (must match storm `NetFunctions`,
  112 bytes). `spi_select_game` and external-message family return `false` → **no BnET path**
  (`:338-348`).

### 2.2 Turn model
- CrownLink does not own a turn clock. `TurnsPerSecond` (`include/crownlink/TurnsPerSecond.h:8-26`)
  is the value it reports to storm through `Caps::turns_per_second` (Standard = 8), which
  changes the **engine's** lockstep rate. Baked into an appended signed `caps.mpq`.

### 2.3 Send / receive path and wire format
- Outbound: storm → `spi_send` (`SNPModule.cpp:211-231`) → `JuiceManager::send_p2p`
  (`JuiceManager.cpp:205-210`) → `JuiceAgent::send_message` (`JuiceAgent.cpp:119-142`) over
  libjuice (UDP/DTLS), DSCP EF.
- Inbound: `JuiceAgent::on_recv` (`JuiceAgent.cpp:264-284`) → receive queue →
  `spi_receive` (`SNPModule.cpp:233-265`); packets older than 2 s dropped (`:254-256`).
- Wire unit is storm's `GamePacketData`: 12-byte `GamePacketHeader`
  {checksum,size,sequence,ack_sequence,type,sub_type,player_id,flags} + ≤500-byte payload
  (max 512). **CrownLink does not build or validate the header** — checksum/seq/ack are
  storm's. CrownLink only reads `type`/`sub_type` (its `CrownLink=50` channel for ping) and
  the `ResendRequest` flag as a link-quality hint (`JuiceAgent.cpp:277-278`).

### 2.4 P2P protocol / transport
- Two channels: (1) **game packets** over libjuice/WebRTC (UDP/DTLS); (2) **crow_serve**
  TCP signaling using an `"CSRV"`-framed, **CBOR**-encoded message protocol
  (`CrowServe.h:66-87`, `Cbor.h:5-25`).
- Message catalogues: `CrownLinkProtocol` (client↔server: ConnectionRequest, KeyExchange
  [inert], ClientProfile, StartAdvertising, AdvertisementsRequest/Response, Echo)
  and `P2PProtocol` (ConnectionRequest, JuiceLocalDescription/Candidate/Done as WebRTC
  signaling; `Pong` dead code).
- Two parallel `JuiceAgent`s per peer (direct `P2P` + `RelayFallback`), `AdaptiveRedundant`
  send strategy duplicates on both when the link looks poor (`JuiceManager.cpp:108-142`).
- `crow_serve` is authoritative only for identity, lobby list, and ICE/TURN credentials.
  **No peer is authoritative over simulation** — storm's lockstep decides state.

### 2.5 Lobby
- Central server `crowserve.cosmonarchy.com:33377` (`src/snp/Config.h:30-31`), overridable
  via `CrownLink.json`. Host advertises an `AdFile` (name/map/TPS); server stores/echoes.
- Reconnect loop + 1.5 s heartbeat, disconnect after >5 missed (`CrowServe.h:131-244`).

### 2.6 Build / ABI
- CMake ≥3.25, **C++23, MSVC x86**, static CRT (`src/CMakeLists.txt:35`), output
  `CrownLink.snp` (DLL + appended `caps.mpq`, `cmake/Caps.cmake`). `__stdcall`, x86.
  Deps fetched: storm_sdk v1.1.0, nlohmann/json, simdutf, concurrentqueue, libjuice fork,
  spdlog.

### 2.7 Gaps relevant to rollback
- No state hashes / desync detection anywhere.
- `final_connection_type` is stubbed to `Standard` (`JuiceManager.cpp:273-276`) so the
  `[Relayed]` UI state is currently unreachable.
- `KeyExchange`/`preshared_key`/`Token` declared but inert; no encryption enforced.

---

## 3. ShieldBattery rollback engine — from `reference/ShieldBattery/game`

Design doc: `reference/ShieldBattery/docs/rollback.md` (read it; it is the authoritative
cross-component map). Summary with anchors:

### 3.1 Injection & session setup
- Export `OnInject` (`src/lib.rs:216-293`); `DllMain` stashes module handle (`:333-345`).
- `scr_init` (`:295-328`) runs the binary analysis (`BwScr::new`, `src/bw_scr.rs:1908`),
  patches the game (`bw.patch_game`, `:321`). `patch_game` at `src/bw_scr.rs:2647-3836`.
- Launch args (`src/lib.rs:582-655`) are only `game_id`, `server_port`, user-data path,
  and a rally-point port override.
- **Netcode v2 setup arrives over the app websocket** as `NetcodeV2Setup`
  (`src/app_messages.rs:743-765`): base64 token, PKCS#8 Ed25519 key, home relay, roster,
  `initial_buffer_turns`, `rollback`. Handled at `src/game_state.rs:108,873-877`; session
  established at `:342-492`.
- `rollback_live::arm_for_session(setup.rollback)` (`src/netcode_v2/session.rs:294-296`,
  `src/rollback_live.rs:521-552`) builds the `InputTable`.

### 3.2 Memory / snapshot model
- **Snapshots are a fixed list of `(address,len)` ranges** (`rollback/ranges.rs`), resolved
  from a static binary analysis — **not hardcoded** — by `samase_scarf` (wrapped thinly by
  `game/scr-analysis`, `src/lib.rs:1-18`; picks `ExecutionStateX86`/`X86_64` per arch).
- `RangeKind` (`ranges.rs:151-176`): `Storage`, `StorageBlock`, `Block`, `IndirectBlock`,
  `MapTiles`, `PoolVector`, `AiRegions`, `TriggerLists`. Specs assembled in
  `analyze_ranges` (`:338-795`) from analysis words, fixed-size blocks (with `size_of`
  asserts, `:31-92`), tile arrays, and pool specs. `RangeList::make_disjoint` (`:284-301`).
- `EXCLUDED` (`ranges.rs:181-207`) documents deliberately omitted state (foliage, sprite
  draw heap/order, pathing graph).
- Concrete addresses computed per game in `snapshot.rs::build` (`:281-432`); one snapshot
  is a memcpy of all ranges (64-byte aligned) + trigger lists (`:42-149`); helper-thread
  copier (`copier.rs`). Snapshot spacing default 3 frames (`tick.rs:18`), forced on
  irreversible steps (`tick.rs:171-173`). Anchors: `Game`=`0x17700`, `Player`=`0x24`
  (32-bit), `Unit`=`0x150` (32-bit) (`ranges.rs:31-92`).
- `rollback_live::supported()` (`:508-516`) → `bw.rollback_missing_analysis()`
  (`src/bw_scr.rs:7179-7217`): refuses rollback if any required accessor/range is missing.
  `establish_session` refuses a rollback session on an unsupporting DLL
  (`netcode_v2/session.rs:178-180`).

### 3.3 State hash
- `rollback/state_hash.rs:18-79`: pointer-free 64-bit FxHash over frame_count, 6 RNG words,
  resources, supply, upgrades/tech, then walks unit/bullet lists (recursive substance
  identification, bounded). Hashed every 8th confirmed step (`rollback/hash_reports.rs:16`).

### 3.4 Hooks (the engine surface we must reproduce for 1.16.1)
Installed in `patch_game` (`src/bw_scr.rs:2647-3836`); signatures at `:9234-9422`.
- **Game logic step:** `StepGameLogic` (`:3403-3411`) → `step_game_logic_hook`
  (`:9479-9570`) → `step_one_game_logic_step` (`:9606-9624`) → [rollback]
  `rollback_live::run_game_logic_step` (`:9612`).
- **Network:** OUT `SendTurnMessage` (`:2995-3007`, strips 0x37, submits local turn),
  IN `ReceiveStormTurns` (**full replacement**, `:3009-3018`), PIPE `FlushLocalTurns`
  (`:3020-3031`), `RecordTurnSyncSlot` (`:3033-3054`), `StormJoinGame` (`:3060-3072`),
  `StepIo`/`StepNetwork` (`:2889-2914`, `:2965-2988`).
- **Command filter:** `ProcessGameCommands` (`:2700-2888`) strips 0x37 (`SYNC=0x37`,
  `NOP=0x05`, `src/bw/commands.rs:8,13`) and substitutes a NOP so BW does not drop the peer.
- **Once-per-frame effects:** `PlaySound` (`:3413-3431`), `ShowGameMessage` (`:3460`),
  `SelectUnits` (`:3481`), `ShowCursorMarkerAt`/`SetSpriteSelectionFlashTimer`
  (`:3519-3543`), `OpenDefeat/VictoryMissionDialog` (`:3549-3569`), observer-UI callbacks
  (`:3574-3628`) — ledgers in `rollback/{sounds,announcements,observer_ui,ui_writes}.rs`.
- Plus many glue hooks (`ScMain`, `GameInit`, `LoadSnpList`, order functions, dialogs, …).

### 3.5 netcode_v2 transport
- `rally-point-client` (git, pinned) over **QUIC/UDP via `noq`** + rustls/ring. Sync
  side: `TurnChannels` try_send/try_recv; async side: `LinkDriver::run_reconnecting`
  (`netcode_v2/session.rs:212,251-267`).
- `Payload {seq, slot, commands, game_frame_count, buffer_directive, sync_generation,
  state_hash}` (`netcode_v2/mod.rs:1099-1109`); seq assigned by driver, slot by relay.
- `InputTable` (`input_table.rs:31-60`): per-step turns; missing remote turn → empty
  `PREDICTED_TURN`; `can_run` up to prediction limit; `take_rollback_target` finds the
  earliest contradicted step.
- Relay keeps the session clock and lead reports (`pacing.rs`), compares state hashes,
  evicts a diverging minority (`docs/rollback.md:154-162`).

### 3.6 Build
- `game/build.bat` (Windows): default `x86_64-pc-windows-msvc`, `x86` →
  `i686-pc-windows-msvc`; copies `shieldbattery.dll` to `dist/`. Crate is a `cdylib`
  named `shieldbattery` (`Cargo.toml:6-8`). Deps include `whack` (hook lib), `bw_dat`
  (aise, `scr-only`), `winapi`, egui. `build.rs` compiles D3D11 shaders from `../package.json`
  version.

---

## 4. Why this is RE, not a port — and what actually transfers

**Transfers (concept + much code):**
- The rollback *algorithm*: input table, prediction, rollback target, snapshot ring,
  restore + re-simulate, once-per-frame effect ledgers, state hash, native-sync disable.
- The range-spec *shape* (`RangeSpec` / `RangeKind` / `resolve_operand`) as an interface to
  retarget at 1.16.1 addresses.
- The "strip 0x37, substitute NOP, hash instead" trick.

**Does NOT transfer (must be redone):**
1. **Analysis layer.** `samase_scarf` analyses SC:R. 1.16.1 is a different binary; but its
   globals/functions/struct offsets are *published and stable* (broodwar memory maps,
   BWAPI, OpenBW, PyMS), so this likely becomes a hardcoded 1.16.1 address table behind the
   same `RangeSpec` interface — arguably **easier** than SC:R.
2. **Plugin state.** Cosmonarchy's `.qdp` plugins add mutable sim state that no 1.16.1
   memory map knows. Snapshot must cover it or rollback desyncs every time. This is the
   hardest and least-documented part.
3. **Transport.** CrownLink is P2P SNP with no session clock / hash adjudication. Either
   merge a relay-style authority into the SNP path, or reimplement the turn table over
   CrownLink sockets with a host peer as authority.
4. **Loop ownership.** Rollback owns the single game-logic step; GPTP plugins also hook the
   engine — these must be reconciled (ordering, re-entrancy on re-simulation).
5. **Launcher integration.** Fraud Launcher must inject and arm the DLL; today it only
   drops `CrownLink.snp` and uses the SNP extensions.

---

## 6. Local building blocks discovered (de-risks the 1.16.1 struct/address layer)

The Cargo git cache already holds neivv's ecosystem, used transitively by ShieldBattery and
by GPTP itself:

- `~/.cargo/git/checkouts/aise-9e85dedfeaa0c87f/da4fed6/` — **`aise` v2.42.1** with the
  vendored **`bw_dat`** subcrate. `bw_dat/src/bw/structs.rs` defines the full `#[repr(C)]`
  BW struct layer (`Game:29`, `Unit:307`, `Player`, `Sprite`, `Image`, `Order`, …) with
  `#[cfg]`-gated fields for **v116 vs scr** — i.e. the exact 32-bit 1.16.1 layouts, behind
  the same crate ShieldBattery uses with the `scr-only` feature. `bw_dat/tests/v116.rs` is
  the 1.16.1 conformance test (gate: `target_pointer_width = "32"`).
- `src/bw/mod.rs:267+` — `aise`'s own `whack_hooks!`/`whack_funcs!`/`whack_vars!` tables keyed
  to the 1.16.1 module base `0x00400000` (e.g. `client_selection` at `0x00597208`).
  Address **resolution** is done by `samase` (neivv's static analysis of the 1.16.1 exe) —
  the same role `samase_scarf` plays for SC:R in ShieldBattery — plus `whack` for patching.
- `~/.cargo/git/checkouts/`: `whack-*`, `samase_scarf-*`, `scarf-*`, `lde-*`,
  `rally-point2-*`.

**Consequence for the plan:** milestone 3 does **not** have to be a hand-transcribed
hardcoded table (ADR §3, RECON §4.2). We can depend on `bw_dat` (v116 mode) + `aise`/`samase`
for the engine struct/address layer, exactly as ShieldBattery does for SC:R — collapsing the
"analysis layer" rewrite into "reuse neivv's libs in v116 mode". The SYMBOL/struct layer
(and thus the `RangeSpec` operands) should then port with far less bespoke work than the ADR
assumed. (The plugin-state half of §3 is unaffected by this.)

**Running/instrumenting the game:** it launches via `umu-run` + GE-Proton
(`~/.local/bin/omarchy-launch-cosmonarchy-build`, `WINEPREFIX=~/Games/cosmonarchy/prefix`),
so the real engine is runnable locally under Proton; a display is required (desktop machine).
ShieldBattery's replay-driven harness (`rollback_harness.rs`) is the model for driving it
without a network opponent.

### 6.1 `samase_plugin` surface (resolves ADR-0002 risk R3 / "experiment 0")

Cloned `github.com/neivv/samase_plugin` (HEAD, 2026-10-10) to inspect the 1.16.1 plugin API:

- `src/lib.rs:343` — **`enum FuncId` (~80 variants)**: hookable 1.16.1 engine functions with
  typed calling conventions (e.g. `UnitCanRally`, `DoMissileDamage`, `HitUnit`, `KillUnit`,
  `TransformUnit`, `PlaceCreepRect`, `AddBuildingAi`, `AddMilitaryAi`, …).
- `src/lib.rs:525` — **`enum VarId` (~95 variants)**: resolvable 1.16.1 globals, including
  almost exactly the `analyze_ranges` categories: `Game`, `RngSeed`, `RngEnable`, `AiRegions`,
  `PlayerAi`, `FirstActiveUnit`, `FirstHiddenUnit`, `Units`, `UnitsVector`, `Pathing`,
  `Selections`, `ClientSelection`, `MapTileFlags`, `Players`, `FirstActiveBullet`,
  `FirstLoneSprite`, `FirstFowSprite`, `Allocator`, `ActiveAiTowns`, `FirstAiScript`, …
- `src/save.rs` — a **save/load section API** (`save::add_hook(name, save, load, init)`),
  i.e. each plugin already serializes its own state for save-games. This is the natural
  basis for ADR-0002's `register_state(save,load)` plugin contract.
- `samase_shim/` — a shim crate; `tests/v1161.rs` — the 1.16.1 conformance test
  (`tests/idk.snx`, exercises mtl/aise/aice save sections).
- `MAX_FUNC_ID`/`MAX_VAR_ID` at `src/lib.rs:27-28`.

**Consequence:** many range operands come "for free" via `samase` `VarId`s rather than a hand
table, and the plugin save/load contract already exists in the plugin framework — collapsing
a large part of ADR-0002 §1/§2 from bespoke work into wiring. This should be folded into the
first implementation milestone.

## 5. Open questions for the architecture decision (see `docs/PORT-PLAN.md`)

1. Relay-authority (keep ShieldBattery's model; needs a new relay + client transport) vs
   host-authority peer model layered on CrownLink (no server, but lag/prediction
   implications for the host)?
2. Snapshot input: instrument the plugins (rebuild them / add a state-registration API) vs
   empirically discover ranges (page-watching / dirty-bit / delta of two runs)?
3. How to hook: our own injected DLL alongside the QDP plugins (order), or a new
   CrownLink-style `.snp` that also owns the loop?
4. Target scope v1: 1v1 melee human-vs-human only, or include observers/subfactions/UMS?
