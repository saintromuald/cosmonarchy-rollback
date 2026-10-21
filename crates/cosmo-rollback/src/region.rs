//! Fog-of-war / reveal-region snapshot (ADR-0005 addendum).
//!
//! `0x006D5E2C` holds an opaque Storm region handle (SRgn) — the accumulated reveal
//! (explored/visible) map. It is consulted by unit targeting/attack/mining, so it is
//! simulation state, but it cannot be memcpy-snapshotted (opaque storm.dll object).
//!
//! Instead we dump the region's rect list with `SRgn529i` and rebuild the region on
//! restore (`SRgnDeleteRegion` → `SRgnCreateRegion` → `SRgn523` OR per rect). All four
//! are reachable through the engine's own `__stdcall` thunks at fixed 1.16.1 addresses.

#![allow(dead_code)]

const CREATE_REGION: usize = 0x0041_1E66; // SRgnCreateRegion(HRGN* phRgn, const RECT* lprc)
const COMBINE_RECT: usize = 0x0041_1E5A; // SRgn523(HRGN, const RECT*, int mode, int combine) [RGN_OR=2]
const DELETE_REGION: usize = 0x0041_1E6C; // SRgnDeleteRegion(HRGN)
const GET_RECTS: usize = 0x0041_1E60; // SRgn529i(HRGN, u32* count_inout, RECT* out)

const REGION_HANDLE: usize = 0x006D_5E2C;

const MAX_RECTS: usize = 255;
const RECT_BYTES: usize = 16;
const RGN_OR: i32 = 2;

type CreateRegionFn = unsafe extern "stdcall" fn(*mut u32, *const u8) -> i32;
type DeleteRegionFn = unsafe extern "stdcall" fn(u32) -> i32;
type CombineRectFn = unsafe extern "stdcall" fn(u32, *const u8, i32, i32) -> i32;
type GetRectsFn = unsafe extern "stdcall" fn(u32, *mut u32, *mut u8) -> i32;

/// Dump the reveal region as `u32 count` + `count` 16-byte rects. Empty if no region.
pub fn capture() -> Vec<u8> {
    if std::env::var("CM_ROLLBACK_NO_REGION").is_ok() {
        return Vec::new();
    }
    unsafe {
        let handle = *(REGION_HANDLE as *const u32);
        if handle == 0 {
            return Vec::new();
        }
        let get_rects: GetRectsFn = std::mem::transmute(GET_RECTS);
        let mut count: u32 = MAX_RECTS as u32;
        let mut buf = vec![0u8; MAX_RECTS * RECT_BYTES];
        get_rects(handle, &mut count, buf.as_mut_ptr());
        let n = (count as usize).min(MAX_RECTS);
        if n >= MAX_RECTS || std::env::var("CM_ROLLBACK_LOG_REGION").is_ok() {
            crate::log::log(&format!(
                "[cosmo-rollback] region capture: handle=0x{handle:x} rects={} (cap {MAX_RECTS}){}",
                count,
                if n >= MAX_RECTS { " TRUNCATED" } else { "" }
            ));
        }
        let mut out = Vec::with_capacity(4 + n * RECT_BYTES);
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(&buf[..n * RECT_BYTES]);
        out
    }
}

/// Rebuild the reveal region from a blob (see [`capture`]).
pub fn restore(blob: &[u8]) {
    unsafe {
        if blob.len() < 4 {
            return;
        }
        let count = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
        let rects = &blob[4..];
        let n = count.min(MAX_RECTS).min(rects.len() / RECT_BYTES);

        let delete: DeleteRegionFn = std::mem::transmute(DELETE_REGION);
        let create: CreateRegionFn = std::mem::transmute(CREATE_REGION);
        let combine: CombineRectFn = std::mem::transmute(COMBINE_RECT);

        let old = *(REGION_HANDLE as *const u32);
        if old != 0 {
            delete(old);
        }
        let mut new: u32 = 0;
        create(&mut new, std::ptr::null());
        for i in 0..n {
            let rect = rects.as_ptr().add(i * RECT_BYTES);
            combine(new, rect, 0, RGN_OR);
        }
        *(REGION_HANDLE as *mut u32) = new;
    }
}
