//! Runtime resolution of the simulation's relocated object pools and arrays.
//!
//! Cosmonarchy's gptp plugin ships the **CCMU** limit extender, which moves every
//! fixed object pool and several large per-object arrays out of the exe's `.data`
//! into a single `CCMU_Extender` static owned by the plugin
//! (`reference/cosmonarchy-gptp/hooks/limits/ccmu.h`):
//!
//! ```text
//! BW_DATA(Unit*,   g_unit_table,   ccmu_extender.unit_table);   // was 0x0059CCA8
//! BW_DATA(Sprite*, g_sprite_table, ccmu_extender.sprite_table); // was 0x00629D98
//! BW_DATA(Order*,  g_order_table,  ccmu_extender.order_table);  // was 0x006416A0
//! BW_DATA(Image*,  g_image_table,  ccmu_extender.image_table);  // was 0x0052F568
//! BW_DATA(Thingy*, g_thingy_table, ccmu_extender.thingy_table); // was 0x006509D8
//! Bulllet new_bullet_table[BulletLimit];                        // was 0x0064B2E8
//! ```
//!
//! The engine reaches the relocated arrays through *absolute operands*, and
//! `inject_*_extender` rewrote those operands at load time
//! (`hooks/limits/ccmu_inject.cpp`: `memory_patch(0x00497388, &ccmu_extender.image_table)`
//! and ~2000 like it). Reading such an operand back therefore yields the exact
//! address the engine uses for this build — no link against gptp, no plugin-relative
//! offset to keep in sync. On a stock 1.16.1 (no extender loaded) the operands still
//! hold their stock values, so the same reads return the stock addresses.
//!
//! A snapshot taken at the hardcoded stock addresses copies arrays the engine no
//! longer uses: the restore is a no-op for the live objects, so a re-simulated frame
//! runs on the un-rewound simulation and consumes the RNG differently. That is the
//! failure this module exists to prevent.
//!
//! Each site below is the address of an operand the extender patches with the array
//! base itself (never an indexed interior address), verified against the shipped
//! 1.16.1 exe: reading the dword there before the extender runs gives the stock base
//! recorded beside it.

#![allow(dead_code)]

use once_cell::sync::OnceCell;

use crate::log::log;
use crate::ranges::{RangeList, RangeSpec};

// Pool limits and element sizes, mirroring `reference/cosmonarchy-gptp/BW/structures.h`
// (the Cosmonarchy build raises these well above stock 1.16.1) and the struct sizes
// the gptp headers assert (`Unit` 0x150, `Sprite` 0x24, `Order` 0x14, `Image` 0x40,
// `Thingy` 0x10, `Bullet` 0x70).
pub const UNIT_LIMIT: usize = 4095;
pub const SPRITE_LIMIT: usize = 8875;
pub const ORDER_LIMIT: usize = 4080;
pub const IMAGE_LIMIT: usize = 16385;
pub const THINGY_LIMIT: usize = 2048;
pub const FOW_SPRITE_LIMIT: usize = 2048;
pub const BULLET_LIMIT: usize = 2048;

pub const UNIT_SIZE: usize = 0x150;
pub const SPRITE_SIZE: usize = 0x24;
pub const ORDER_SIZE: usize = 0x14;
pub const IMAGE_SIZE: usize = 0x40;
pub const THINGY_SIZE: usize = 0x10;
pub const FOW_SPRITE_SIZE: usize = 0x10;
pub const BULLET_SIZE: usize = 0x70;

/// Every pool the rollback snapshot must cover.
#[derive(Debug, Clone, Copy)]
pub struct Pools {
    pub unit_table: usize,
    pub sprite_table: usize,
    pub order_table: usize,
    pub image_table: usize,
    pub thingy_table: usize,
    pub fow_sprite_table: usize,
    pub bullet_table: usize,
}

impl Pools {
    pub fn base(&self, which: &str) -> Option<(usize, usize)> {
        // (address, pool byte length)
        Some(match which {
            "units" => (self.unit_table, UNIT_LIMIT * UNIT_SIZE),
            "sprites" => (self.sprite_table, SPRITE_LIMIT * SPRITE_SIZE),
            "orders" => (self.order_table, ORDER_LIMIT * ORDER_SIZE),
            "images" => (self.image_table, IMAGE_LIMIT * IMAGE_SIZE),
            "thingies" => (self.thingy_table, THINGY_LIMIT * THINGY_SIZE),
            "fow_sprites" => (self.fow_sprite_table, FOW_SPRITE_LIMIT * FOW_SPRITE_SIZE),
            "bullets" => (self.bullet_table, BULLET_LIMIT * BULLET_SIZE),
            _ => return None,
        })
    }
}

/// `(pool name, operand site, stock 1.16.1 base, pool byte length)`.
const SITES: &[(&str, usize, usize, usize)] = &[
    ("units", 0x0040_1EF0, 0x0059_CCA8, UNIT_LIMIT * UNIT_SIZE),
    ("sprites", 0x0048_73EC, 0x0062_9D98, SPRITE_LIMIT * SPRITE_SIZE),
    ("orders", 0x0048_C59A, 0x0064_16A0, ORDER_LIMIT * ORDER_SIZE),
    ("images", 0x0049_7388, 0x0052_F568, IMAGE_LIMIT * IMAGE_SIZE),
    ("thingies", 0x0048_7336, 0x0065_09D8, THINGY_LIMIT * THINGY_SIZE),
    (
        "fow_sprites",
        0x0048_7316,
        0x0065_2928,
        FOW_SPRITE_LIMIT * FOW_SPRITE_SIZE,
    ),
    ("bullets", 0x0048_A691, 0x0064_B2E8, BULLET_LIMIT * BULLET_SIZE),
];

/// The other large per-object arrays the extender relocates, which are simulation
/// state and are covered by the snapshot:
///
/// - `unit_finder_x`/`unit_finder_y`: the unit position search (ShieldBattery's
///   `unit_position_search_x/y`); each `Unit.finder_index` (0x13C) indexes into them.
/// - `air_splash_list`: the splash-target scratch list (SB `air_splash_candidates`).
/// - `unit_query_marks`/`unit_query_results`: the scratch and result arrays of a
///   position query (SB `unit_query_scratch_marks`/`unit_query_results`).
/// - `sprite_list_pointers`/`image_list_pointers`: the per-object list bookkeeping the
///   CCMU list routines index by object index.
///
/// `(name, operand site, stock 1.16.1 base, byte length)`.
/// Relocated arrays (base resolved at runtime by `resolve_one`). The last field marks
/// **per-frame scratch**: arrays the engine rebuilds during a frame — the unit-position
/// search scratch, the get-all-units-in-bounds scratch, and the render sprite/image list
/// pointers. Nothing reads them across a frame boundary, so restoring them is meaningless
/// and *comparing* them makes the shadow gate differ for reasons that are not desync (they
/// are rebuilt by the render path the re-simulation does not run; see
/// `docs/analysis/restore-residual.md`). They are excluded from the snapshot by default;
/// `CM_ROLLBACK_SCRATCH=1` includes them for diagnostics.
const ARRAYS: &[(&str, usize, usize, usize, bool)] = &[
    ("unit_finder_x", 0x0042_22B9, 0x0066_FF78, UNIT_LIMIT * 2 * 8, true),
    ("unit_finder_y", 0x0042_2195, 0x0067_69B8, UNIT_LIMIT * 2 * 8, true),
    ("air_splash_list", 0x0048_B1B1, 0x0064_EEE0, UNIT_LIMIT * 4, false),
    (
        "unit_query_marks",
        0x0042_FEAC,
        0x006B_B938,
        (UNIT_LIMIT + 1) * 4,
        true,
    ),
    (
        "unit_query_results",
        0x0042_FEEB,
        0x006B_D3D0,
        (UNIT_LIMIT + 1) * 4,
        true,
    ),
    (
        "sprite_list_pointers",
        0x0042_D343,
        0x006C_2318,
        SPRITE_LIMIT * 4,
        true,
    ),
    (
        "image_list_pointers",
        0x0042_D37E,
        0x006C_4A38,
        IMAGE_LIMIT * 4,
        true,
    ),
];

static POOLS: OnceCell<Pools> = OnceCell::new();
static ARRAY_BASES: OnceCell<Vec<(&'static str, usize, usize)>> = OnceCell::new();

/// The resolved pools for this process (resolved on first use, after the extender
/// has patched the operands).
pub fn get() -> &'static Pools {
    POOLS.get_or_init(resolve)
}

/// The resolved relocated arrays as `(name, address, length)`.
pub fn arrays() -> &'static [(&'static str, usize, usize)] {
    ARRAY_BASES.get_or_init(|| {
        let scratch = std::env::var("CM_ROLLBACK_SCRATCH").is_ok();
        if !scratch {
            log("[cosmo-rollback] per-frame scratch arrays excluded from snapshot (CM_ROLLBACK_SCRATCH=1 to include)");
        }
        ARRAYS
            .iter()
            .filter(|&&(_, _, _, _, is_scratch)| scratch || !is_scratch)
            .map(|&(name, site, stock, len, _)| (name, resolve_one(name, site, stock, len), len))
            .collect()
    })
}

fn resolve() -> Pools {
    let mut p = Pools {
        unit_table: 0x0059_CCA8,
        sprite_table: 0x0062_9D98,
        order_table: 0x0064_16A0,
        image_table: 0x0052_F568,
        thingy_table: 0x0065_09D8,
        fow_sprite_table: 0x0065_2928,
        bullet_table: 0x0064_B2E8,
    };
    for &(name, site, stock, len) in SITES {
        let addr = resolve_one(name, site, stock, len);
        set(&mut p, name, addr);
    }
    p
}

/// Resolve one relocated base: an env override, else the patched operand if it lands
/// inside a loaded module, else the verified stock address.
fn resolve_one(name: &str, site: usize, stock: usize, len: usize) -> usize {
    let read = read_operand(site);
    match overridden(name) {
        Some(a) => {
            log(&format!(
                "[cosmo-rollback] {name}: override 0x{a:08x} (site read 0x{read:08x}, stock 0x{stock:08x})"
            ));
            a
        }
        None if read == stock => {
            log(&format!(
                "[cosmo-rollback] {name}: stock 0x{stock:08x} (extender not active)"
            ));
            stock
        }
        None => match crate::modules::owner_of(read, len) {
            Some(module) => {
                log(&format!(
                    "[cosmo-rollback] {name}: relocated 0x{read:08x} in {module} (stock 0x{stock:08x})"
                ));
                read
            }
            None => {
                log(&format!(
                    "[cosmo-rollback] {name}: UNRESOLVED read 0x{read:08x} is not inside any \
                     loaded module; falling back to stock 0x{stock:08x}"
                ));
                stock
            }
        },
    }
}

fn set(p: &mut Pools, name: &str, addr: usize) {
    match name {
        "units" => p.unit_table = addr,
        "sprites" => p.sprite_table = addr,
        "orders" => p.order_table = addr,
        "images" => p.image_table = addr,
        "thingies" => p.thingy_table = addr,
        "fow_sprites" => p.fow_sprite_table = addr,
        "bullets" => p.bullet_table = addr,
        _ => {}
    }
}

/// Read the 32-bit absolute address held in the operand at `site`.
///
/// The operand is the immediate of a `mov reg, imm32`-shaped instruction, so `site`
/// is not necessarily 4-byte aligned (the order-table site is at ...59A). Read it
/// unaligned rather than assuming alignment.
///
/// # Safety
/// `site` must be inside the (committed, read-only) mapped image of the game exe.
fn read_operand(site: usize) -> usize {
    unsafe { std::ptr::read_unaligned(site as *const u32) as usize }
}

/// `CM_ROLLBACK_POOL_<NAME>` overrides a base (hex or decimal), for pinning a
/// candidate address without a rebuild.
fn overridden(name: &str) -> Option<usize> {
    let key = format!("CM_ROLLBACK_POOL_{}", name.to_uppercase());
    let v = std::env::var(key).ok()?;
    let v = v.trim();
    let hex = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X"));
    match hex {
        Some(h) => usize::from_str_radix(h, 16).ok(),
        None => v.parse().ok(),
    }
}

/// Plugin `.data` simulation state that the snapshot must cover but that cannot be
/// memcpy'd wholesale (ADR-0004: restoring gptp's ~19 MB `.data` rewinds heap pointers
/// and `parking_lot` mutexes and corrupts the plugin). Each entry is
/// `(writable-section name, byte offset within the section, length)`.
///
/// These offsets were found with the reproducibility probe (`CM_ROLLBACK_DET`): with
/// every exe range covered, a step run twice from the same restored state still differed
/// in these bytes, and the probe reported them inside `gptp.qdp..data` / `aise.qdp..data`
/// rather than the exe's `.data`. They are pinned by section + offset because the `.qdp`
/// modules are relocated per process.
///
/// Only regions whose bytes read as scalars (counters/flags that step by one, or flags
/// cleared to zero) are pinned here. The scattered pointer-looking bytes the probe also
/// reports in `aise.qdp..data` are deliberately **not** restored flat: rewinding a plugin
/// heap pointer is exactly the corruption ADR-0004 documented, and that state belongs in
/// the plugin's own serializer (Layer B/C).
/// Plugin `.data` simulation state, pinned by `(writable-section name, byte offset in the
/// section, length)`.
///
/// **Empty on purpose.** The reproducibility probe found real un-snapshotted state inside
/// the plugins, and pinning the small scalar-looking parts of it (`gptp.qdp..data`
/// `+0x365478`, `+0x1167770`, `+0x1190fe8`, `+0x1191470`, `+0x1214f5c`; `aise.qdp..data`
/// `+0x2bc`, `+0x2cc` — aise's 16-byte `Xoshiro128PlusPlus` state, `+0xaec`, `+0xaf8`;
/// `mtl.qdp..data` `+0x0..0x18de`) was tried and **measured**: it did not make a step
/// reproducible, and it added those ranges to the gate's divergence report without
/// changing the outcome.
///
/// That is the expected result, and it is why ADR-0004 rejected flat plugin `.data`
/// restore: a plugin's state is not independent bytes. Rewinding aise's RNG while its heap
/// containers (`Vec`/`HashMap`, ADR-0005 §5) stay advanced leaves the plugin inconsistent,
/// and `mtl.qdp` is a Rust plugin whose statics may be caches rather than state. Plugin
/// state must come from each plugin's own serializer (ADR-0004/0005 Layers B & C), which
/// needs gptp/aise rebuilt with the `cosmo_*_state_*` exports.
///
/// The mechanism is kept because it is the right way to pin plugin state once it is
/// *known* to be a plain value (it resolves by section + offset, since the `.qdp` modules
/// are relocated per process), and the offsets above are the evidence to start from.
const PLUGIN_STATE: &[(&str, usize, usize)] = &[];

/// Resolve [`PLUGIN_STATE`] against the loaded modules and add each region as a range.
pub fn add_plugin_state_ranges(rl: &mut RangeList) {
    if std::env::var("CM_ROLLBACK_NO_PLUGIN_STATE").is_ok() {
        log("[cosmo-rollback] CM_ROLLBACK_NO_PLUGIN_STATE: plugin .data state excluded");
        return;
    }
    for &(section, off, len) in PLUGIN_STATE {
        match crate::modules::section_base(section) {
            Some(base) => {
                let name: &'static str = Box::leak(
                    format!("{}@0x{off:x}", section.split('.').next().unwrap_or(section))
                        .into_boxed_str(),
                );
                log(&format!(
                    "[cosmo-rollback] plugin state {name}: 0x{:08x} ({section} +0x{off:x}, {len} bytes)",
                    base + off
                ));
                rl.add(RangeSpec::new(name, base + off, len));
            }
            None => log(&format!(
                "[cosmo-rollback] plugin state: section {section} not found; skipped"
            )),
        }
    }
}

/// Add every resolved pool and relocated array as a snapshot range.
pub fn add_ranges(rl: &mut RangeList) {
    if std::env::var("CM_ROLLBACK_NO_POOLS").is_ok() {
        log("[cosmo-rollback] CM_ROLLBACK_NO_POOLS: object pools excluded from snapshot");
        return;
    }
    let p = get();
    // The unit, sprite and image pools are snapshotted field-by-field (see the
    // `*_SIM` specs below) so the render/scratch fields the outer main loop's draw
    // pass (`0x0041CA00` H_RenderScreen) rewrites *between* two logic frames are not
    // part of the state hash. Those fields are not simulation state: the draw pass
    // legitimately runs outside the re-simulated frame, and re-running it is not
    // reproducible from a snapshot that excludes the viewer/camera state it depends
    // on (`docs/analysis/restore-residual.md` §3). The order/thingy/fow/bullet pools
    // carry no such render fields and are kept whole.
    let raw = std::env::var("CM_ROLLBACK_POOL_RENDER").is_ok();
    for &(name, _, _, _) in SITES {
        let Some((addr, len)) = p.base(name) else {
            continue;
        };
        let (count, stride, fields) = match name {
            "units" => (UNIT_LIMIT, UNIT_SIZE, UNIT_SIM),
            "sprites" => (SPRITE_LIMIT, SPRITE_SIZE, SPRITE_SIM),
            "images" => (IMAGE_LIMIT, IMAGE_SIZE, IMAGE_SIM),
            _ => (0, 0, &[][..]),
        };
        if raw || fields.is_empty() {
            rl.add(RangeSpec::new(name, addr, len));
        } else {
            let label: &'static str = Box::leak(format!("{name}.sim").into_boxed_str());
            let render_label: &'static str =
                Box::leak(format!("{name}.render").into_boxed_str());
            let render = render_fields(name);
            for i in 0..count {
                for &(off, flen) in fields {
                    rl.add(RangeSpec::new(label, addr + i * stride + off, flen));
                }
                // Fields the outer loop writes *between* frames: they are snapshotted (so a
                // restore still puts them back) but excluded from the state hash, because no
                // re-simulation of the frame can reproduce them — see `UNIT_RENDER`.
                for &(off, flen) in render {
                    rl.add(RangeSpec::new(render_label, addr + i * stride + off, flen));
                }
            }
        }
    }
    // The UnitPath pool. It is **heap-allocated** at game init (`0x0042F4F0`:
    // `SMemAlloc(0x20000)` then a 0x400-entry free list; the base lands in
    // `g_path_array_start` `0x006BEE8C`, the free-list head in `g_first_free_path`
    // `0x006BEE84` — `BW/bwdata.h:1333-1334`), so it is neither in a module nor
    // CCMU-relocated and the base must be read from the pointer at runtime. Its *contents*
    // are simulation state: a restore that rewinds a unit but leaves its path at live
    // values diverges in `units.sim` at `next_movement_waypoint` (0x18), the `path` pointer
    // (0x101) and `contour_bounds` (0x109), with an iscript cascade.
    //
    // `UnitPath` is all values (`start`/`next`/`finish`/`time_started`/`unit_index`/
    // `x_y_speed`/…/`steps[24]`, `BW/structures.h:75-91`), so restoring it has no
    // pointer-rewind risk, and its bytes are pointer-free for the cross-client hash.
    let path_base = unsafe { std::ptr::read_unaligned(PATH_ARRAY_START as *const u32) as usize };
    if path_base != 0 {
        log(&format!(
            "[cosmo-rollback] path_array: base 0x{path_base:08x} ({PATH_LIMIT} paths x {PATH_SIZE:#x} = {PATH_POOL_BYTES:#x} bytes)"
        ));
        rl.add(RangeSpec::new("path_array", path_base, PATH_POOL_BYTES));
    } else {
        log("[cosmo-rollback] path_array: base is null (no game running); skipped");
    }
    rl.add(RangeSpec::new("first_free_path", 0x006B_EE84, 4));

    // The AI script pool. `g_aiscript_array` (`0x0068C0FC`) is a **pointer** to the pool
    // (`BW/bwdata.h:1039`), so the pool itself was never snapshotted — only the list heads
    // (`first_ai_script`/`first_free_ai_script`, `ranges.rs`). A restore rewound the heads and
    // left the entries at live values, which is the residual `first_ai_script` divergence.
    // The pool is `AiScript scripts[100]` + `AiScript* first_free` = 100*0x34 + 4 = 0x1454
    // bytes (`BW/structures.h:1041-1044`); the stock base 0x0068ACA8 plus that size lands
    // exactly on the pointer variable, confirming the size. The base is read at runtime so a
    // relocated pool is followed.
    let ai_base = unsafe { std::ptr::read_unaligned(AI_SCRIPT_ARRAY as *const u32) as usize };
    if ai_base != 0 {
        log(&format!(
            "[cosmo-rollback] ai_script_pool: base 0x{ai_base:08x} ({AI_SCRIPT_POOL_BYTES:#x} bytes)"
        ));
        rl.add(RangeSpec::new("ai_script_pool", ai_base, AI_SCRIPT_POOL_BYTES));
    } else {
        log("[cosmo-rollback] ai_script_pool: base is null; skipped");
    }

    // The AiTown pool. `g_ai_town_array` (`0x006AA090`) is the array itself — the engine does
    // `mov esi, 0x6aa090` and `sub ecx, 0x6aa090`, so it is not a pointer variable:
    // `AiTownArray` = `AiTown towns[100]` + `AiTown* first_free` (`BW/structures.h:950-953`,
    // `AiTown` 0x1cc). Only the list *heads* were snapshotted (`active_ai_towns`), so a restore
    // rewound the heads and left the towns at live values — the shape of the residual
    // `ai_script_pool`/`first_ai_script` divergence.
    rl.add(RangeSpec::new("ai_town_pool", 0x006A_A090, AI_TOWN_POOL_BYTES));

    // The GuardAi pool. `GuardAiListPlayers` (`0x00685108`, already snapshotted as `guard_ais`)
    // holds eight `GuardAiList { GuardAiArray* array; GuardAi* first; }`; the array is
    // `GuardAi[1000]` + `first_free_guard` = 0x7D04 bytes (`BW/structures.h:825-833`). Only the
    // list heads were snapshotted, so a restore rewound them and left the guards at live
    // values. The base is read from the first list's `array` field; the eight lists share the
    // one `g_guard_array`.
    let guard_base = unsafe { std::ptr::read_unaligned(GUARD_LIST_PLAYERS as *const u32) as usize };
    if guard_base != 0 {
        log(&format!(
            "[cosmo-rollback] guard_pool: base 0x{guard_base:08x} ({GUARD_POOL_BYTES:#x} bytes)"
        ));
        rl.add(RangeSpec::new("guard_pool", guard_base, GUARD_POOL_BYTES));
    } else {
        log("[cosmo-rollback] guard_pool: base is null; skipped");
    }

    if std::env::var("CM_ROLLBACK_NO_RELOC_ARRAYS").is_ok() {
        log("[cosmo-rollback] CM_ROLLBACK_NO_RELOC_ARRAYS: relocated arrays excluded");
        return;
    }
    for &(name, addr, len) in arrays() {
        rl.add(RangeSpec::new(name, addr, len));
    }
    add_plugin_state_ranges(rl);
}

/// `g_path_array_start` (`BW/bwdata.h:1334`): a `UnitPath*` holding the pool base.
const PATH_ARRAY_START: usize = 0x006B_EE8C;
/// The pool it points at: 0x400 entries of 0x80 bytes (the init at `0x0042F4F0` allocates
/// `0x20000` and links `cmp eax,0x400; jng`; `BW/structures.h:75-91`).
const PATH_LIMIT: usize = 0x400;
const PATH_SIZE: usize = 0x80;
const PATH_POOL_BYTES: usize = PATH_LIMIT * PATH_SIZE;

/// `g_aiscript_array` (`BW/bwdata.h:1039`): an `AiScriptArray*` holding the AI script pool.
const AI_SCRIPT_ARRAY: usize = 0x0068_C0FC;
/// `AiScriptArray` = `AiScript scripts[100]` + `AiScript* first_free` (`BW/structures.h:1041-1044`);
/// `AiScript` is 52 (0x34) bytes (`C_ASSERT` at `BW/structures.h:1039`).
const AI_SCRIPT_POOL_BYTES: usize = 100 * 0x34 + 4;

/// `AiTownArray` = `AiTown towns[100]` + `AiTown* first_free` (`BW/structures.h:950-953`),
/// `AiTown` 0x1cc bytes.
const AI_TOWN_POOL_BYTES: usize = 100 * 0x1cc + 4;

/// `GuardAiListPlayers` (`BW/bwdata.h:1095`): eight `GuardAiList`, whose first `array` field
/// holds the `GuardAiArray` base.
const GUARD_LIST_PLAYERS: usize = 0x0068_5108;
/// `GuardAiArray` = `GuardAi guard_ais[1000]` + `GuardAi* first_free_guard`
/// (`BW/structures.h:825-828`), `GuardAi` 0x20 bytes.
const GUARD_POOL_BYTES: usize = 1000 * 0x20 + 4;

/// Ranges whose base lives in a `.data` pointer slot and whose backing allocation the
/// engine owns. The game frees or moves these across game/map boundaries (the path pool
/// at game end, heap-resident pools when a build relocates them), so a base resolved once
/// at plugin load can be dead or wrong by the next capture — copying from it faults the
/// process. `snapshot.rs` re-reads the slot and validates the mapping on every
/// capture/restore for these names.
const DYNAMIC_RANGES: &[(&str, usize)] = &[
    ("path_array", PATH_ARRAY_START),
    ("ai_script_pool", AI_SCRIPT_ARRAY),
    ("guard_pool", GUARD_LIST_PLAYERS),
];

/// Whether `name` is one of the pointer-read ranges above.
pub fn is_dynamic(name: &str) -> bool {
    DYNAMIC_RANGES.iter().any(|&(n, _)| n == name)
}

/// Fresh base for a pointer-read range, re-read from its `.data` slot (0 when null).
pub fn dynamic_base(name: &str) -> usize {
    for &(n, slot) in DYNAMIC_RANGES {
        if n == name {
            return unsafe { std::ptr::read_unaligned(slot as *const u32) as usize };
        }
    }
    0
}

/// Pool fields the outer main loop writes **between** two logic frames, so a frame
/// re-simulation cannot reproduce them and they must not be part of the state hash. They are
/// still snapshotted — a restore puts them back, which keeps rollback correct for the units
/// that *do* use the offset for simulation state.
///
/// `Unit+0xCA` is the case: `CM_ROLLBACK_BOUNDARY`-style comparison of a frame's exit with the
/// next frame's entry shows it written outside the frame (on a high-index, render-helper
/// unit), while the same offset is `larva_timer`/`flag_spawn_frame`/`hangar_link` for real
/// units. Restoring it therefore matters, but hashing it makes the gate differ forever.
const UNIT_RENDER: &[(usize, usize)] = &[(0xCA, 1)];

/// The render-written fields of a pool, by pool name.
fn render_fields(name: &str) -> &'static [(usize, usize)] {
    match name {
        "units" => UNIT_RENDER,
        _ => &[],
    }
}

/// The unit pool's simulation fields. `position_search_indices` (0x13C..0x14C, four
/// `u32` each indexing into the `unit_finder_x`/`unit_finder_y` scratch arrays) is
/// excluded: it is a per-query cache that every unit-position search rebuilds, so it is
/// meaningless across a frame boundary — the same reason those two arrays are already
/// excluded as scratch (see `ARRAYS`). The draw pass runs such queries between frames,
/// which is why a whole-pool snapshot diverges on it.
const UNIT_SIM: &[(usize, usize)] = &[
    (0x00, 0xCA),        // ...up to the outer-written byte (see UNIT_RENDER)
    (0xCB, 0x13C - 0xCB), // ...up to position_search_indices
    (0x14C, 0x150 - 0x14C), // repulse_* tail
];

/// The sprite pool's simulation fields. `position` (0x14..0x18) is excluded: the draw
/// pass repositions render sprites (selection/cursor/high-index helper sprites) between
/// frames, and that write is not part of the re-simulated logic frame.
const SPRITE_SIM: &[(usize, usize)] = &[
    (0x00, 0x14), // prev, next, sprite_id, player, selection_index, visibility_mask,
                  // elevation_level, flags, selection_flash_timer, index, width, height
    (0x18, 0x0C), // main_image, first_image, last_image
];

/// The image pool's simulation fields. The draw pass rewrites `flags` (0x0C, the
/// Redraw/drawn bits), `map_position` (0x1C), `screen_position` (0x20) and `grp_bounds`
/// (0x24) every frame, so those are excluded; the iscript program state (0x10..0x1A)
/// and the `iscript_key` half of `flags` are captured separately, field-by-field, by
/// `image.rs`. `prev`/`next` (the free-list links), `image_id`, `step_frame` and
/// `parent` are the simulation bits the raw pool copy also covered.
const IMAGE_SIM: &[(usize, usize)] = &[
    (0x00, 0x0A), // prev, next, image_id
    (0x0B, 0x01), // direction
    (0x10, 0x0C), // iscript header/pos/return_pos/animation/wait (0x10..0x18), frameset, frame
    (0x38, 0x04), // step_frame
    (0x3C, 0x04), // parent
];

/// `Unit.position_search_indices` (0x13C..0x14C): each unit's slot in the incremental
/// `unit_finder_x`/`unit_finder_y` position-search arrays.
///
/// This field is **simulation state** — `StepObjects` reads it to remove a moving unit
/// from the finder before re-inserting it — but the render pass also runs position
/// queries between frames and rewrites it. Restoring it keeps the re-simulated frame's
/// unit moves consistent with the restored unit positions; hashing it would report the
/// render pass's legitimate rewrite as a false desync. It is therefore snapshotted into
/// a **restore-only** blob (see `Snapshot::scratch`), restored alongside the pools but
/// left out of the state hash.
const FINDER_INDICES_OFF: usize = 0x13C;
const FINDER_INDICES_LEN: usize = 16; // [u32; 4]

/// `Sprite.position` (0x14..0x18), the sprite's map-space position. Like the unit
/// finder indices, it is simulation state (`StepObjects` moves the sprite with its unit
/// and reads it back) that the draw pass also rewrites between frames, so it is restored
/// but not hashed.
const SPRITE_POS_OFF: usize = 0x14;
const SPRITE_POS_LEN: usize = 4; // Point { x: i16, y: i16 }

/// The unit position-search arrays (`unit_finder_x`/`unit_finder_y`) and each unit's
/// `finder_index` into them. The finder is read by the pathfinding and target selection
/// during `StepObjects` (units are looked up by position), so it must be rolled back for
/// the re-sim to reproduce the same paths — but the render pass also runs position
/// queries between frames and rewrites it, so it must not be hashed. The earlier
/// classification of these arrays as per-frame scratch (nothing reads them across a
/// boundary) is wrong: the re-sim diverges in unit movement without them.
const FINDER_X_SITE: usize = 0x0042_22B9;
const FINDER_X_STOCK: usize = 0x0066_FF78;
const FINDER_Y_SITE: usize = 0x0042_2195;
const FINDER_Y_STOCK: usize = 0x0067_69B8;
const FINDER_LEN: usize = UNIT_LIMIT * 2 * 8;

/// Capture the restore-only scratch state (finder arrays + indices + sprite positions).
pub fn capture_scratch() -> Vec<u8> {
    if std::env::var("CM_ROLLBACK_NO_POOLS").is_ok() {
        return Vec::new();
    }
    let p = get();
    let unit_len = UNIT_LIMIT * FINDER_INDICES_LEN;
    let sprite_len = SPRITE_LIMIT * SPRITE_POS_LEN;
    let fx = resolve_one("unit_finder_x", FINDER_X_SITE, FINDER_X_STOCK, FINDER_LEN);
    let fy = resolve_one("unit_finder_y", FINDER_Y_SITE, FINDER_Y_STOCK, FINDER_LEN);
    let mut out = vec![0u8; unit_len + sprite_len + FINDER_LEN * 2];
    let mut pos = 0usize;
    for i in 0..UNIT_LIMIT {
        let src = (p.unit_table + i * UNIT_SIZE + FINDER_INDICES_OFF) as *const u8;
        unsafe {
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(pos), FINDER_INDICES_LEN);
        }
        pos += FINDER_INDICES_LEN;
    }
    for i in 0..SPRITE_LIMIT {
        let src = (p.sprite_table + i * SPRITE_SIZE + SPRITE_POS_OFF) as *const u8;
        unsafe {
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(pos), SPRITE_POS_LEN);
        }
        pos += SPRITE_POS_LEN;
    }
    for (base, _) in [(fx, FINDER_LEN), (fy, FINDER_LEN)] {
        unsafe {
            std::ptr::copy_nonoverlapping(base as *const u8, out.as_mut_ptr().add(pos), FINDER_LEN);
        }
        pos += FINDER_LEN;
    }
    out
}

/// Restore the scratch state captured by [`capture_scratch`].
pub fn restore_scratch(blob: &[u8]) {
    let unit_len = UNIT_LIMIT * FINDER_INDICES_LEN;
    let sprite_len = SPRITE_LIMIT * SPRITE_POS_LEN;
    if blob.len() < unit_len + sprite_len + FINDER_LEN * 2 {
        return;
    }
    let p = get();
    let fx = resolve_one("unit_finder_x", FINDER_X_SITE, FINDER_X_STOCK, FINDER_LEN);
    let fy = resolve_one("unit_finder_y", FINDER_Y_SITE, FINDER_Y_STOCK, FINDER_LEN);
    let mut pos = 0usize;
    for i in 0..UNIT_LIMIT {
        let dst = (p.unit_table + i * UNIT_SIZE + FINDER_INDICES_OFF) as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(blob.as_ptr().add(pos), dst, FINDER_INDICES_LEN);
        }
        pos += FINDER_INDICES_LEN;
    }
    for i in 0..SPRITE_LIMIT {
        let dst = (p.sprite_table + i * SPRITE_SIZE + SPRITE_POS_OFF) as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(blob.as_ptr().add(pos), dst, SPRITE_POS_LEN);
        }
        pos += SPRITE_POS_LEN;
    }
    for (base, _) in [(fx, FINDER_LEN), (fy, FINDER_LEN)] {
        unsafe {
            std::ptr::copy_nonoverlapping(blob.as_ptr().add(pos), base as *mut u8, FINDER_LEN);
        }
        pos += FINDER_LEN;
    }
}
