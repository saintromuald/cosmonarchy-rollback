//! Image-pool Iscript state snapshot (ADR-0005 addendum).
//!
//! The engine's image pool is `ccmu_extender.image_table` on Cosmonarchy (the CCMU
//! limit extender relocated it from the stock `0x0052F568`); `pools::get()` resolves
//! the live address. `ImageLimit` is 16385 on this build, not the stock 5000.
//!
//! The iscript VM state that steers per-frame RNG consumption is restored field by
//! field rather than by a raw copy of the 0x40-byte `Image`. The image pool threads a
//! free list through `link` and points at sprites (`parent_sprite`), GRP data
//! (`grp_file`) and render functions; restoring those wholesale is the kind of
//! cross-object pointer rewinding that has already corrupted the game once. The
//! fields below are exactly the per-image iscript program counter and frame state
//! (`BW/structures/Image.h`):
//!
//! ```text
//! 0x0B direction          0x10 iscript_header_offset  0x14 return_address
//! 0x0C flags + iscript_key 0x12 iscript_offset          0x16 animation  0x17 wait
//! 0x18 frameset           0x1A frame_index
//! ```
//!
//! `CM_ROLLBACK_IMAGE_RAW=1` switches to a raw copy of the whole struct, to test
//! whether the field list is still missing something.

#![allow(dead_code)]

use crate::pools;

/// (offset within the image, length) for the iscript/program state fields.
const SIM_FIELDS: &[(usize, usize)] = &[
    (0x0B, 1), // direction
    (0x0C, 2), // flags : 12 | iscript_key : 4  (masked to iscript_key below)
    (0x10, 8), // iscript_header_offset, iscript_offset, return_address, animation, wait
    (0x18, 2), // frameset
    (0x1A, 2), // frame_index
];

/// The `flags` word (0x0C) holds render flags in its low bits (`Redraw` 0x0001, cleared
/// by the draw pass every frame; `Clickable` 0x0020; `Invisible` 0x0040;
/// `UsesSpecialOffset` 0x0080) and simulation flags in the rest (`Mirrored` 0x0002,
/// `DontUpdate` 0x0004, `HasRotationFrames` 0x0008, `UseFullIscript` 0x0010,
/// `Omnidirectional` 0x0100, Flag09..11, and the iscript key in the high nibble). Only the
/// render bits are masked out of the captured/hashed value; the simulation bits are
/// restored normally (see `pools.rs::IMAGE_SIM`, which excludes the whole word from the
/// flat range).
const FLAGS_OFF: usize = 0x0C;
const FLAGS_MASK: u16 = 0x00E1; // Redraw | Clickable | Invisible | UsesSpecialOffset

fn sim_bytes() -> usize {
    SIM_FIELDS.iter().map(|&(_, len)| len).sum()
}

pub fn capture() -> Vec<u8> {
    if std::env::var("CM_ROLLBACK_NO_IMAGE").is_ok() {
        return Vec::new();
    }
    let base = pools::get().image_table;
    let count = pools::IMAGE_LIMIT;
    let stride = pools::IMAGE_SIZE;
    unsafe {
        if raw() {
            return std::slice::from_raw_parts(base as *const u8, count * stride).to_vec();
        }
        let mut out = vec![0u8; count * sim_bytes()];
        let mut pos = 0;
        for i in 0..count {
            let img = base + i * stride;
            for &(off, len) in SIM_FIELDS {
                let src = (img + off) as *const u8;
                if off == FLAGS_OFF {
                    let masked = (std::ptr::read_unaligned(src as *const u16) & !FLAGS_MASK)
                        .to_le_bytes();
                    out[pos..pos + len].copy_from_slice(&masked);
                } else {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(pos), len);
                }
                pos += len;
            }
        }
        out
    }
}

pub fn restore(blob: &[u8]) {
    if blob.is_empty() {
        return;
    }
    let base = pools::get().image_table;
    let count = pools::IMAGE_LIMIT;
    let stride = pools::IMAGE_SIZE;
    unsafe {
        if raw() {
            if blob.len() < count * stride {
                return;
            }
            std::ptr::copy_nonoverlapping(blob.as_ptr(), base as *mut u8, count * stride);
            return;
        }
        if blob.len() < count * sim_bytes() {
            return;
        }
        let mut pos = 0;
        for i in 0..count {
            let img = base + i * stride;
            for &(off, len) in SIM_FIELDS {
                let dst = (img + off) as *mut u8;
                if off == FLAGS_OFF {
                    // Restore only the simulation flag bits, preserving the live render
                    // flags (which the draw pass owns and the re-sim does not reproduce).
                    let live = std::ptr::read_unaligned(dst as *const u16);
                    let sim = u16::from_le_bytes([blob[pos], blob[pos + 1]]);
                    std::ptr::write_unaligned(
                        dst as *mut u16,
                        (live & FLAGS_MASK) | (sim & !FLAGS_MASK),
                    );
                } else {
                    std::ptr::copy_nonoverlapping(blob.as_ptr().add(pos), dst, len);
                }
                pos += len;
            }
        }
    }
}

fn raw() -> bool {
    std::env::var("CM_ROLLBACK_IMAGE_RAW").is_ok()
}
