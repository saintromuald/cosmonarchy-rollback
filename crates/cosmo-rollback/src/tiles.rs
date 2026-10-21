//! ActiveTile flags array snapshot (ADR-0005 addendum).
//!
//! `map_tile_flags` is a `*mut u32` at 0x006D1260; the array behind it is one dword
//! per tile (`width × height`), mutated every frame by creep/building/unit occupancy.
//! The AI build/expand-placement routine (0x00414680) scans it and consumes the RNG
//! per player with valid tiles, so leaving it unrestored changes the RNG-call count.
//!
//! It is a pointer, so we dereference at capture and snapshot the buffer it targets
//! (never the 4-byte pointer itself). Width/height live in the `Game` struct
//! (map_width_tiles 0x0057F1D4, map_height_tiles 0x0057F1D6).

#![allow(dead_code)]

const TILE_FLAGS_PTR: usize = 0x006D_1260;
const MAP_WIDTH: usize = 0x0057_F1D4;
const MAP_HEIGHT: usize = 0x0057_F1D6;

const MAX_BYTES: usize = 0x10_0000; // 1 MiB cap

/// Log a capture's geometry only the first time: it never changes, and a line per capture
/// (per frame, twice per frame in live mode) costs real runtime.
fn once(msg: &str) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::log::log(msg);
    }
}

pub fn capture() -> Vec<u8> {
    if std::env::var("CM_ROLLBACK_NO_TILES").is_ok() {
        return Vec::new();
    }
    unsafe {
        let base = *(TILE_FLAGS_PTR as *const u32) as *const u8;
        if base.is_null() {
            return Vec::new();
        }
        let w = *(MAP_WIDTH as *const u16) as usize;
        let h = *(MAP_HEIGHT as *const u16) as usize;
        let len = w.saturating_mul(h).saturating_mul(4);
        if len == 0 || len > MAX_BYTES {
            crate::log::log(&format!(
                "[cosmo-rollback] tiles capture skipped: base=0x{:x} w={} h={} len=0x{:x}",
                base as usize, w, h, len
            ));
            return Vec::new();
        }
        once(&format!(
            "[cosmo-rollback] tiles capture: base=0x{:x} w={} h={} len=0x{:x}",
            base as usize, w, h, len
        ));
        std::slice::from_raw_parts(base, len).to_vec()
    }
}

pub fn restore(blob: &[u8]) {
    if blob.is_empty() {
        return;
    }
    unsafe {
        let base = *(TILE_FLAGS_PTR as *const u32) as *mut u8;
        if base.is_null() {
            return;
        }
        let w = *(MAP_WIDTH as *const u16) as usize;
        let h = *(MAP_HEIGHT as *const u16) as usize;
        let len = w.saturating_mul(h).saturating_mul(4);
        if len == 0 || len > MAX_BYTES || blob.len() < len {
            return;
        }
        std::ptr::copy_nonoverlapping(blob.as_ptr(), base, len);
    }
}
