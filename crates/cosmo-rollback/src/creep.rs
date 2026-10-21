//! Creep tile-array snapshot (ADR-0005 addendum; deep disassembly 2026-10-17).
//!
//! `spread_creep` (0x00414680) is called per-frame from the creep-building update
//! (0x4ea880, the `[unit+0x64] ∈ {0x83,0x84,0x85}` Hatchery/Lair/Hive path) and
//! consumes exactly **one** LCG step of `rng_seed` for each owner whose candidate
//! list is non-empty (the LCG is inlined at 0x414850: `seed = seed*0x015a4e35 + 1`).
//!
//! The candidate gate reads **heap-resident per-tile arrays** whose pointers live in
//! `.data` (allocated once at map load, `0x414a30`):
//!
//!   0x006d0e84 -> tileset_indexed_map_tiles   (2 bytes/tile)  "already creep?" check
//!   0x006d0c68 -> creep_original_tiles        (2 bytes/tile)
//!   0x006d0e80 -> creep_tile_borders          (1 byte/tile)
//!
//! `tiles.rs` snapshots `map_tile_flags` (0x006d1260, occupancy) — NOT the array this
//! gate reads. Leaving the creep arrays unrestored makes re-simulation compute a
//! different candidate list, so a building with zero candidates in normal play gets
//! one (or vice versa): one extra/subtracted RNG call, and `rng_seed` diverges,
//! cascading into minerals/bullets/reveal. These are heap buffers, so we dereference
//! the `.data` pointer slots and memcpy the buffer *contents* — never the pointer
//! words (restoring those is what made the whole-`.data` restore crash, Iteration 14).

#![allow(dead_code)]

const MAP_WIDTH: usize = 0x0057_F1D4;
const MAP_HEIGHT: usize = 0x0057_F1D6;

/// (` .data` slot holding the buffer pointer, element size in bytes).
const ARRAYS: &[(usize, usize)] = &[
    (0x006D_0E84, 2), // tileset_indexed_map_tiles
    (0x006D_0C68, 2), // creep_original_tiles
    (0x006D_0E80, 1), // creep_tile_borders
];

const MAX_BYTES: usize = 0x10_0000; // 1 MiB cap per array

fn dims() -> Option<(usize, usize)> {
    unsafe {
        let w = *(MAP_WIDTH as *const u16) as usize;
        let h = *(MAP_HEIGHT as *const u16) as usize;
        if w == 0 || h == 0 {
            return None;
        }
        Some((w, h))
    }
}

fn used() -> Vec<(usize, usize)> {
    // Optional subset selection without a rebuild, e.g. CM_ROLLBACK_CREEP_ONLY=0x6d0e84
    if let Ok(s) = std::env::var("CM_ROLLBACK_CREEP_ONLY") {
        let want: Vec<usize> = s
            .split(',')
            .filter_map(|p| {
                let p = p.trim();
                let h = p.strip_prefix("0x").or_else(|| p.strip_prefix("0X"));
                match h {
                    Some(h) => usize::from_str_radix(h, 16).ok(),
                    None => p.parse().ok(),
                }
            })
            .collect();
        if !want.is_empty() {
            return ARRAYS.iter().copied().filter(|(slot, _)| want.contains(slot)).collect();
        }
    }
    ARRAYS.to_vec()
}

/// Log the capture geometry only the first time (see `tiles::once`).
fn once(msg: &str) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::log::log(msg);
    }
}

pub fn capture() -> Vec<u8> {
    if std::env::var("CM_ROLLBACK_NO_CREEP").is_ok() {
        return Vec::new();
    }
    let (w, h) = match dims() {
        Some(d) => d,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (slot, elem) in used() {
        unsafe {
            let base = *(slot as *const u32) as *const u8;
            let len = w.saturating_mul(h).saturating_mul(elem);
            if base.is_null() || len == 0 || len > MAX_BYTES {
                crate::log::log(&format!(
                    "[cosmo-rollback] creep capture skipped slot=0x{:x} base=0x{:x} len=0x{:x}",
                    slot, base as usize, len
                ));
                return Vec::new();
            }
            out.extend_from_slice(std::slice::from_raw_parts(base, len));
        }
    }
    if !out.is_empty() {
        once(&format!(
            "[cosmo-rollback] creep capture: w={} h={} slots={} bytes=0x{:x}",
            w,
            h,
            used().len(),
            out.len()
        ));
    }
    out
}

pub fn restore(blob: &[u8]) {
    if blob.is_empty() {
        return;
    }
    let (w, h) = match dims() {
        Some(d) => d,
        None => return,
    };
    let slots = used();
    let expected: usize = slots
        .iter()
        .map(|&(_, elem)| w.saturating_mul(h).saturating_mul(elem))
        .sum();
    if expected == 0 || blob.len() < expected {
        return;
    }
    let mut off = 0usize;
    for (slot, elem) in slots {
        unsafe {
            let base = *(slot as *const u32) as *mut u8;
            let len = w.saturating_mul(h).saturating_mul(elem);
            if base.is_null() || len == 0 {
                return;
            }
            std::ptr::copy_nonoverlapping(blob.as_ptr().add(off), base, len);
            off += len;
        }
    }
}
