//! Scalar projections of the object pools, for the **cross-client** desync hash.
//!
//! The flat range hash ([`crate::state_hash::desync_hash`]) only covers ranges whose
//! bytes are certainly values — the pools are excluded because their entries thread
//! per-process pointers. But a divergence that changes unit positions, hitpoints or
//! orders while leaving every counter equal would produce identical flat hashes on both
//! clients: a silent desync. This module closes that hole the way ShieldBattery's
//! `state_hash` does — per-object projections of scalar fields, keyed by the object's
//! **index** (computed per process as `(ptr - pool_base) / stride`, which is
//! address-independent), over the objects the engine itself considers live (the active
//! lists). The list walk order and the field values are simulation state, so two
//! clients in sync produce the same hash.
//!
//! Only fields known to be values and known to be simulation-owned are projected:
//! render-written bytes (`Unit+0xCA`, sprite positions, image draw state) are excluded
//! for the same reason the flat hash excludes `*.render` ranges.

#![allow(dead_code)]

use core::mem::offset_of;

use bw_dat::structs::{Bullet, Unit};

use crate::log::log;
use crate::pools;

/// Mix one byte into an FNV-1a accumulator.
fn mix_byte(h: u64, b: u8) -> u64 {
    (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
}

/// The fields of one unit projected into the hash. All are values (no pointers, no
/// render-written bytes); the offsets are pinned with `offset_of!` against the
/// size-asserted 32-bit layout.
const UNIT_PROJECTION: &[(usize, usize)] = &[
    (offset_of!(Unit, flingy) + offset_of!(bw_dat::structs::Flingy, hitpoints), 4),
    (offset_of!(Unit, flingy) + offset_of!(bw_dat::structs::Flingy, position), 4),
    (offset_of!(Unit, flingy) + offset_of!(bw_dat::structs::Flingy, flingy_id), 2),
    (offset_of!(Unit, player), 1),
    (offset_of!(Unit, order), 1),
    (offset_of!(Unit, order_state), 1),
    (offset_of!(Unit, order_signal), 1),
    (offset_of!(Unit, order_timer), 1),
    (offset_of!(Unit, shields), 4),
    (offset_of!(Unit, unit_id), 2),
    (offset_of!(Unit, build_queue), 5 * 2),
    (offset_of!(Unit, energy), 2),
    (offset_of!(Unit, remaining_build_time), 2),
    (offset_of!(Unit, previous_hp), 2),
    (offset_of!(Unit, secondary_order), 1),
    (offset_of!(Unit, secondary_order_state), 1),
    (offset_of!(Unit, move_target_update_timer), 1),
    (offset_of!(Unit, kills), 1),
    (offset_of!(Unit, rank), 1),
];

/// The fields of one bullet projected into the hash.
const BULLET_PROJECTION: &[(usize, usize)] = &[
    (offset_of!(Bullet, flingy) + offset_of!(bw_dat::structs::Flingy, hitpoints), 4),
    (offset_of!(Bullet, flingy) + offset_of!(bw_dat::structs::Flingy, position), 4),
    (offset_of!(Bullet, player), 1),
    (offset_of!(Bullet, state), 1),
    (offset_of!(Bullet, order_state), 1),
    (offset_of!(Bullet, weapon_id), 1),
    (offset_of!(Bullet, bounces_remaining), 1),
    (offset_of!(Bullet, spread_seed), 1),
];

/// The first active unit / bullet list heads (bwdata.h) — the lists the engine walks.
const FIRST_ACTIVE_UNIT: usize = 0x0062_8430;
const FIRST_ACTIVE_BULLET: usize = 0x0064_DEC4;

/// Walk a pool's active list (prev/next links at offset 0/4) and project each element.
///
/// `next`/`prev` are the linked-list pointers (per-process); the element index is
/// derived from the pool base, so the hash is address-independent.
unsafe fn project_list(
    h: &mut u64,
    head: usize,
    base: usize,
    stride: usize,
    fields: &[(usize, usize)],
    label: &[u8],
    max: usize,
) {
    let mut node = unsafe { *(head as *const u32) as usize };
    let mut visited = 0usize;
    while node != 0 && visited < max {
        let Some(rel) = node.checked_sub(base) else {
            break;
        };
        if rel % stride != 0 {
            break;
        }
        let index = (rel / stride) as u32;
        for b in label {
            *h = mix_byte(*h, *b);
        }
        for b in index.to_le_bytes() {
            *h = mix_byte(*h, b);
        }
        for &(off, len) in fields {
            let p = (node + off) as *const u8;
            for b in unsafe { core::slice::from_raw_parts(p, len) } {
                *h = mix_byte(*h, *b);
            }
        }
        visited += 1;
        // The `next` link is at offset 4 for Unit (prev at 0) — every bw_dat pool
        // entry begins with `prev`/`next` pointers.
        node = unsafe { *((node + 4) as *const u32) as usize };
    }
    if visited == max {
        log("[cosmo-rollback] projection: active list longer than the cap; hash truncated");
    }
}

/// The cross-client pool projection hash: units and bullets, over the engine's own
/// active lists. `0` when the pools are excluded (`CM_ROLLBACK_NO_POOLS`).
pub fn pool_projection_hash() -> u64 {
    if std::env::var("CM_ROLLBACK_NO_POOLS").is_ok() {
        return 0;
    }
    let p = pools::get();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    unsafe {
        project_list(
            &mut h,
            FIRST_ACTIVE_UNIT,
            p.unit_table,
            pools::UNIT_SIZE,
            UNIT_PROJECTION,
            b"u",
            pools::UNIT_LIMIT,
        );
        project_list(
            &mut h,
            FIRST_ACTIVE_BULLET,
            p.bullet_table,
            pools::BULLET_SIZE,
            BULLET_PROJECTION,
            b"b",
            pools::BULLET_LIMIT,
        );
    }
    h
}
