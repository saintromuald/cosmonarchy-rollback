# The closed simulation step: `receive_network_messages` + the render/scratch split

**Status:** implementation + measurements, 2026-10-18.
**Grounding:** the stock exe disassembled with `ndisasm -b 32 -o 0x401000`; gptp
`BW/structures/{Image.h,UnitLayout.h,structures.h}`, `BW/bwdata.h`; the isolated retest
copy run with `CM_ROLLBACK_SHADOW=1 CM_ROLLBACK_SNAPSHOT_SPACING=1 CM_ROLLBACK_SHADOW_START=95
CM_ROLLBACK_NO_PLUGIN=1`.

---

## 0. TL;DR

The shadow gate's divergence had **two** causes, and neither was "un-snapshotted plugin
state" (ADR-0005 was chasing the wrong thing):

1. **`receive_network_messages` (0x00485F70) was missing from the re-simulation.** It is
   the between-frame call that processes the next replay/network turn — it moves units,
   allocates orders and consumes RNG, all *simulation* state. It was absent from both
   `CM_ROLLBACK_PROBE_OUTER` (which therefore "found" only `0x0041CA00`) and
   `CM_ROLLBACK_RESIM_LOOP` (which therefore "didn't help"). The re-sim now runs it after
   each frame, which removes the `orders`/`rng_seed`/`sprite` divergence.
2. **The snapshot covered render/scratch fields the render pass legitimately rewrites
   between frames.** These are excluded field-by-field (`pools.rs`), and the ones that are
   *also* read by `StepObjects` are kept in a restore-only blob so the re-sim stays correct.

Residual (still open): the **`UnitPath` pool is not snapshotted** (`g_path_array_start`
0x006BEE8C, `struct UnitPath` 0x80 bytes), so a few frames still diverge on unit movement
and the iscript cascade that follows it.

---

## 1. The between-frame mutator, measured in context

`outer_trace.rs` installs a wrapping hook on each between-frame call *target*, so the
callback fires at the call site with the live registers and calls `orig` (the real
function). Two earlier diagnostics got this wrong: `CM_ROLLBACK_PROBE_OUTER` **called the
functions out of context and omitted `receive_network_messages` entirely**, and
`CM_ROLLBACK_RESIM_LOOP` re-ran the *render* calls (`post_frame`, `dialog_repaint`) but not
`receive_network_messages`.

The trace (scalar watch over `rng_seed`, the order/unit free lists and counts) shows:

- **No between-frame call mutates `rng_seed`, the order free list, or the unit free list.**
  Only `dialog_repaint` (0x0041CA00, `H_RenderScreen`) changes `sprite_count`.

That scalar result is *misleading*: it watches counts, not the pools' movement fields. The
whole-pool boundary diff shows units diverging on `next_movement_waypoint` (0x18),
`movement_flags` (0x20), `position` (0x28) and the `path` pointer (0x100) — i.e. the
between-frame work also re-runs unit *pathing/command processing*, which is what
`receive_network_messages` does via `0x00485F70 → 0x485d10 → 0x485980` (a command
dispatcher). Re-running it in the re-sim (now the default; `CM_ROLLBACK_NO_RESIM_RECV=1`
disables it) removes the `orders`/`rng_seed`/`sprite` divergence.

## 2. The render/scratch split

The draw pass (`0x0041CA00`) and the render pass's position queries rewrite these fields
*between* two logic frames; the re-sim deliberately does not run them, so they can never
match. They are therefore removed from the state hash. They fall into two classes:

| field | struct | offset | sim-read? | handling |
|---|---|---|---|---|
| `flags` (Redraw/Clickable/Invisible/UsesSpecialOffset) | Image | 0x0C | no (bits masked) | excluded from hash; sim bits kept |
| `map_position`, `screen_position`, `grp_bounds` | Image | 0x1C/0x20/0x24 | no | excluded |
| `position` | Sprite | 0x14 | **yes** | restore-only blob |
| `position_search_indices` (`finder_index`) | Unit | 0x13C | **yes** | restore-only blob |
| `unit_finder_x`/`unit_finder_y` | — | (arrays) | **yes** | restore-only blob |

The restore-only fields are captured into `Snapshot::scratch` (a blob that `Snapshot`
restores but `state_hash` does not hash — the same pattern as `region`/plugin `blobs`).
They are simulation state the re-sim reads (`StepObjects` uses them for pathing and target
selection), but the render pass also rewrites them, so hashing them would report the
render pass's legitimate rewrite as a desync.

The `flags` mask only drops the four known render bits
(`0x00E1` = Redraw|Clickable|Invisible|UsesSpecialOffset); the simulation bits (`Mirrored`
0x02, `DontUpdate` 0x04, `HasRotationFrames` 0x08, `UseFullIscript` 0x10,
`Omnidirectional` 0x100, `Flag09..11`, and the iscript key in the high nibble) are kept —
an earlier mask that dropped the whole low 12 bits broke the iscript and cascaded.

## 3. Result

| configuration | divergences | ranges |
|---|---|---|
| original (whole-pool, no re-sim of between-frame work) | 37 | images, units, sprites |
| + re-run `receive_network_messages` + render/scratch split | **5** | `units.sim`, `images.sim` |

`rng_seed`, `orders`, `sprites`, `thingies` no longer diverge. The remaining 5 frames
diverge on `units.sim` at `next_movement_waypoint` (0x18), the `path` pointer byte (0x101)
and `contour_bounds` (0x109), plus the iscript `direction`/`frame` cascade that follows.

## 4. What remains: the `UnitPath` pool

`Unit.path` (0x100) points into the fixed `UnitPath` array
(`g_path_array_start` 0x006BEE8C, `g_first_free_path` 0x006BEE84, `struct UnitPath` 0x80
bytes; `BW/bwdata.h:1333-1334`). It is **not in `ranges.rs`**, so a restore leaves the
path data (start/next/finish/speed/steps) at its live values while the unit is rolled back.
That is the source of the remaining movement divergence. Fix: resolve the array base the
way `pools.rs` resolves the other relocated pools and add it as a snapshot range (its
`steps[24]` and `x_y_speed` are values, the rest are coordinates — no pointer-rewind risk).

## 5. Files changed

- `crates/cosmo-rollback/src/pools.rs` — field-level pool ranges (`*_SIM`), the
  `capture_scratch`/`restore_scratch` restore-only blob (finder arrays + indices + sprite
  positions), `CM_ROLLBACK_POOL_RENDER=1` to revert to whole-pool.
- `crates/cosmo-rollback/src/image.rs` — mask only the four render `flags` bits.
- `crates/cosmo-rollback/src/snapshot.rs` — added the `scratch` blob (restore, not hash).
- `crates/cosmo-rollback/src/frame_hook.rs` — `receive_network_messages` callable + added
  to `outer_calls`/`outer_loop_work`.
- `crates/cosmo-rollback/src/rollback/mod.rs` — re-run `receive_network_messages` in the
  re-sim by default (`CM_ROLLBACK_NO_RESIM_RECV=1` to disable).
- `crates/cosmo-rollback/src/outer_trace.rs` — in-context between-frame mutator trace
  (`CM_ROLLBACK_TRACE_OUTER`).
- `crates/cosmo-rollback/src/lib.rs` — `mod outer_trace`.

Default behaviour is unchanged: every new behaviour is behind an env knob, and the
`receive_network_messages` re-run only affects the opt-in shadow gate. `./tools/test.sh`
passes (40 tests).
