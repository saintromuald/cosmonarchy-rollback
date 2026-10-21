# Cosmonarchy Plus — game plan

Branch: `cosmonarchy-plus` (this repo). Product name: **Cosmonarchy Plus** (`CM+`).
Base: Cosmonarchy BW release commit `3d8d947` (19 Sep 2026) + the rollback netcode in
this tree. Everything below assumes a match runs through the rollback client
(`docs/PORT-PLAN.md`, ADR-0006/0007).

The point of the name is truth in advertising: **Plus means the simulation differs from
vanilla Cosmonarchy**. Every Plus client must refuse to play a non-Plus client, and the
version must be visible in the lobby. If we ever ship UI-only changes, they still get the
Plus tag, because a Plus replay will not replay on vanilla.

---

## 1. What is easy, what is hard

Grounded in the actual stack: BW 1.16.1 + `gptp.qdp` (C++ hooks) + `aise`/Lua AI +
DatExtend `.datex` data + loose-file overlay in `Release/mpq/`.

| Tier | Change | Why |
|---|---|---|
| **Easy** | Costs, HP, armor, build time, supply, damage, ranges, cooldowns, requirements — **as runtime overrides** | The values are constants applied at data load. A Plus plugin can patch them after `DatExt::extend_data()` from a small text config, so we never touch binary `.datex`. Reviewable, diffable, revertable. |
| **Easy** | UI: saturation numbers, queue overlay, timers, tooltips, range circles | Read-only drawing. `ui::OverlayHud` + `ui::Widget` + `screen_text` already exist; `g_production` already tracks every player's production. Zero sim state. |
| **Easy** | Config toggles (`cosmonarchy.yml` / new `plus.yml`), lobby/version branding | Same pattern as `show-ingame-global-queue`. |
| **Medium** | Placement rules (build next to minerals, depot footprints), adjacency auras, build/morph restrictions | `hooks/placement.h:10-11` already hooks creep and tile-flag placement checks. Input-driven, deterministic, easy to snapshot. |
| **Medium** | Reusing an unused/extended unit slot as a new building (same art), new buttons/requirements | IDs up to ~2775 already exist (`units.txt` lists `Extended Unit #2775`); DatExtend + grafting handle the tables. Art can be borrowed. |
| **Medium-hard** | New per-unit simulation state (e.g. an outpost's mining timer) | Fine mechanically, but it must live in snapped memory *and* be added to the GPTP state serializer (`plugin_state.rs` resolves `cosmo_gptp_state_*`; the shipped `gptp.qdp` has no such exports — our fork does). |
| **Hard** | Brand-new art: GRPs, iscript animations, wireframes, icons, sounds | No writer pipeline in this repo; `.datex`, `.gob`/`.got` and iscript are binary. Placeholder/reuse is cheap, *good* art is human work. |
| **Hard** | Authoring `.datex` from scratch | `units.txt` is only a name list; the real table is binary `units.datex`. There is no exporter here. Either (a) runtime overrides, or (b) write a small subset writer for `units/weapons/images/iscript`. |
| **Hard** | Pathfinding, collision, footprints, order state machines, selection logic | Deep engine surgery, high desync and crash risk, and it breaks AI assumptions everywhere. Avoid unless a feature truly needs it. |
| **Hard** | Anything non-deterministic (unseeded RNG, wall-clock timers, pointer-order iteration) | Rollback re-simulates; one non-deterministic branch = permanent desync. |
| **Out of scope** | A sixth race, new subfaction systems, matchmaking, observers, UMS scripting overhaul | Months of work each; already explicit non-goals in `PORT-PLAN.md`. |

**The honest limit of "me":** I can implement all of the Easy/Medium items, write the
data-override layer, the overlay widgets, the placement hooks, the version handshake and
the tests. I cannot make good *art*, cannot judge whether a number "feels Brood War"
without you playtesting, and cannot safely rewrite pathing/collision. Balance taste and
asset quality are the two things that stay human.

---

## 2. Five rules every Plus change must obey

1. **Sim state is either snapped or derived.** New counters go in `UnitExt`
   (`data_ext.h`) and must be covered by the fork's `cosmo_gptp_state_save/load`.
2. **No gameplay logic in Lua/aise.** That layer is not rolled back (ADR-0001 §4; the
   residual in `docs/analysis/restore-residual.md` is AI-only). Lua is for AI flavour and
   map scripts, never for rules.
3. **UI reads, never writes.** Overlays may read `Unit`, `UnitExt`, `g_production`; they
   may not mutate sim fields, and render-written bytes stay out of the hash.
4. **Same inputs, same result.** Placement/queue logic must depend only on game state and
   commands — no real time, no RNG without the shared seed.
5. **Version-locked.** Both peers must present the same Plus build id (data hash + plugin
   hash + git sha). Mismatch = refuse to start, not "try it and desync".

---

## 3. Features, with options

### 3.1 Saturation numbers on mineral fields / gas (SC2-style)

**What already exists:** the mod tracks exactly how many workers are assigned to each
resource node — `UnitExt.values[ValueAssignedHarvesters]`, incremented in
`hooks/game_hooks.cpp:1022`, consumed by `harvester_is_active` (`hooks/harvest.cpp:3`).
Mining yield comes from `g_globals->get_yield(YieldType::Mineral|Gas…)`, and gas nodes
have four tiers (`get_gas_tier`, `harvest.cpp:22`). So the *numerator is free and exact*.
Only the **optimal denominator** is a design question.

Tier-1 vs tier-2 workers are **separate unit ids** (`Terran Mason/Talos/Ogun`,
`Zerg Droleth/Agrileth/Gosvileth`, `Protoss Scribe/Autocort/Prudence` —
`hooks/economic_units.cpp:6-21`). They do not change `get_harvest_amount` (that is
per-node), so any difference in optimal count comes from trip cadence, not payload. That
makes a single universal "2 per patch" wrong in mixed-worker bases.

Options:

- **A. Static** — `n/2` on minerals, `n/3` on gas, colour-coded. Cheapest, predictable,
  but lies once tier-2 workers are in the mix.
- **B. Config table** — per-node-type optimum, with an optional per-worker-id rate factor
  in `plus.yml`. Honest, tunable, no code changes to retune.
- **C. Measured** — use the headless harness to record trips/second per worker tier on a
  fixed map, bake a small curve into the config, show `assigned / optimal(tier mix)`.
- **Recommended: B now, C later.** Draw `assigned/optimal`, colour: grey = under,
  green = optimal, amber = over. Tooltip explains the denominator. Toggle:
  `show-mineral-saturation` (default on for Plus, off if you prefer purity).
- Draw in the existing per-object overlay pass (same place as HP bars/aura circles), only
  for visible, non-depleted nodes, so it costs nothing in big fights.

### 3.2 Legible build/training queue at the top of the screen

**What already exists:** `ProductionArrays g_production`
(`hooks/unit_stats/supply_hooks.h:8-13`) holds, per player, 16 slots of
`{type, timer, count}`; `StatResource` already renders icons + counts + a progress bar
(`hooks/interface/hud/stat_resource.cpp:330-430`), behind the **Show Global Queue**
option. So the data is there; what is missing is *placement and legibility*.

Options:

- **A. Top-screen overlay (recommended).** New `OverlayHud` bound to `g_production`:
  one row for army, one for economy, grouped **by producer** (icon of the building, then
  its queued units, remaining time, count). Two rows max, hides when idle, hotkey to
  toggle. Reuses the existing icon/font/progress code.
- **B. Improve the resource panel** — same data, stays in the corner, less invasive, but
  it competes with resources/APM and is harder to read mid-fight.
- **C. SC2-style command card row** — 5 icons under the resources for the *selected*
  building only. Most familiar, but duplicates the selection panel and needs layout work
  around existing dialogs.

Do **A** (optionally with C's selected-producer detail). Keep "idle producer" pulsing, and
never show enemy queues. This is a pure render feature: no desync surface.

### 3.3 A mining building that can go right next to the minerals ("Fastest" style)

What "Fastest" actually needs: a drop-off that fits the gap and can be placed without the
usual footprint/spacing fight, so mining starts immediately — not a free economy.

Options, cheapest first:

- **A. Placement rule only (recommended first).** Keep existing town halls; allow a depot
  footprint within N tiles of a mineral field and relax the spacing check only for
  designated depot ids. Uses the existing placement hooks
  (`update_building_placement_state_map_tile_flags`, `update_creep_building_placement_state`).
  No new unit, no new art, balance-neutral outside the opening.
- **B. Dedicated 1×1 / 2×2 "supply cache" drop-off.** `ResourceDepot` flag + small
  footprint + cheap cost, reusing borrowed art. Needs a unit id and button/requirement
  wiring — the first real content change, and the template for everything later.
- **C. Mode-level pre-placement.** At game start, spawn a completed depot beside each
  start location (map/mode flag, no data). Perfect for Fastest/custom maps, zero effect on
  ladder maps, but it is a mode, not a building.
- **D. Auto-mining outpost.** Building harvests on a timer with no workers. Fun, but it
  removes worker micro — that is *not* Brood War. Mode-only, last.

**Recommendation:** A + C for the first release; B as the first content milestone; D only
as an opt-in mode if you still want it after playtesting.

### 3.4 "Hachirosk-like" features

Hachirosk is the Zerg T1 **central node**: Resource Depot, spreads cagra in 8 tiles,
generates a Larva every 12s, morphs into Caphrolosk/Gathtelosk. "Add features like the
Hachirosk" can mean three different things, with very different costs:

- **A. Universal forward node.** Every race gets a cheap structure that is a depot *and*
  extends buildable territory (Terran relay / Protoss pylon-nexus hybrid). New unit +
  aura; medium-hard, high design value.
- **B. Hachirosk placement semantics for all town halls.** Let town halls be planted
  anywhere (drop the creep/spacing requirement for designated ids), keeping the morph
  tree. Small rules change, no new content; pairs naturally with 3.3.
- **C. Port Hachirosk itself** to another race/subfaction. Cheapest mechanically (reuse
  art/behaviour), but lore-wise odd and it blurs race identity.

**Recommendation:** B now (it is the same hook as 3.3A), A in Phase 3 if you want a real
new toy, C no.

### 3.5 Balance and data changes (your cost list)

Do these as **runtime overrides**, not `.datex` edits:

- `plus.yml` maps unit id → `{minerals, gas, build_time, hp, armor, supply, …}`.
- Applied once after data load, before the first frame; hashed into the build id.
- Every override is a reviewable line in git; deleting it restores vanilla.
- Add `--vanilla-data` for testing and for tournaments that want stock numbers.
- Guard rails: refuse absurd values (negative, > u16) and warn on ids that don't exist.

This avoids the hardest plumbing problem (no `.datex` writer) while giving 90% of what a
balance fork needs. New *units* still need the binary path; see Phase 3.

---

## 4. Phases and exit criteria

**Phase 0 — identity and baseline (days).**
Freeze the base commit; branch `cosmonarchy-plus`; add `plus.yml`; add the build id
(git sha + data hash + plugin hash) to the rollback handshake and the lobby version;
refuse mismatched peers. Exit: two Plus clients play; a Plus client refuses vanilla.

**Phase 1 — QoL only, provably neutral (1-2 weeks).**
Top-screen queue overlay; saturation numbers; idle-producer hints; options + config.
Exit: `tools/gate.sh` unchanged (same residual), `tools/crownlink_match.sh` with
`CL_SEND_DELAY_MS=150` shows rollbacks and `desyncs=0`, and the state hash is identical to
the pre-Plus build in a scripted 5-minute match. No sim diff = safe to ship.

**Phase 2 — placement and Fastest (2-4 weeks).**
Placement relaxation + mode-level pre-placed depot + a Fastest test map. Exit: placement
determinism test (same commands → same layout), rollback match on the Fastest map,
no pathing stalls at game start.

**Phase 3 — first content (1-2 months).**
Dedicated drop-off depot (B in 3.3), then the universal forward node (A in 3.4).
Exit: unit id, footprint, flags, button, requirement, wireframe/icon, AI + Lua build order,
state serializer updated, rollback clean, and a "content off" switch that restores vanilla.

**Phase 4 — balance (ongoing).**
Your cost/tier list via `plus.yml`, shipped as a named ruleset (`plus-classic`,
`plus-experimental`). Exit: per-patch diff table + a replay set that still reproduces.

---

## 5. Testing (reuse what exists)

- `tools/gate.sh Release 3 90` — snapshot/restore completeness, headless.
- `tools/crownlink_match.sh run a :96 … b :95 …` + `CL_SEND_DELAY_MS=150` — real
  rollback path on one machine; expect non-zero `sent/recv`, rollbacks, `agreed`, `desyncs=0`.
- Hash-neutral check for UI phases: same replay, same input script, compare state hash
  against the pre-Plus binary.
- Placement tests: scripted build orders on synthetic maps (mineral gap, creep edge,
  cliff), compare resulting unit positions.
- A "vanilla parity" replay list: Plus with all toggles off must reproduce vanilla hashes.

---

## 6. Decisions I need from you

1. Is Plus allowed to change **balance** at all, or is v1 strictly QoL + placement?
2. Which "Hachirosk-like" reading is the real one: universal forward node, town-hall
   placement freedom, or porting Hachirosk?
3. Is the mining building a **real building** (art, cost, tech) or a **mode convenience**
   (pre-placed on Fastest maps)?
4. Saturation display: strict 2-per-patch (readable, slightly wrong) or a tier-aware
   optimum (accurate, needs measurement)?
5. Do you want Plus to keep tracking upstream Cosmonarchy releases, or pin and diverge?
