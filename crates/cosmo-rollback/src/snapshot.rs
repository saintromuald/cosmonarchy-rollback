//! A snapshot is a memcpy of every range in a `RangeList`, plus any plugin-state blob
//! (ADR-0005 Layer C), plus the range table it was taken with so a restore is
//! self-describing. Mirrors ShieldBattery's `rollback/snapshot.rs`.

use crate::creep;
use crate::image;
use crate::plugin_state;
use crate::pools;
use crate::ranges::{RangeList, RangeSpec};
use crate::region;
use crate::replay;
use crate::tiles;

pub struct Snapshot {
    ranges: &'static RangeList,
    data: Vec<u8>,
    blobs: Vec<Vec<u8>>,
    region: Vec<u8>,
    image: Vec<u8>,
    tiles: Vec<u8>,
    creep: Vec<u8>,
    replay: Vec<u8>,
    /// Restore-only scratch state — `Unit.position_search_indices` and `Sprite.position`
    /// — simulation state that is **restored but not hashed**: the render pass rewrites
    /// it between frames, so hashing it would report a false desync, but the re-sim
    /// reads it, so it must be rolled back. See `pools::capture_scratch`.
    scratch: Vec<u8>,
}

impl Snapshot {
    /// Resolve a range's live address. Pointer-read ("dynamic") ranges are re-read from
    /// their `.data` slot and checked against the process memory map: a pool the engine
    /// freed or moved since plugin load is skipped instead of faulting the copy. Returns
    /// `(address, valid)`; an invalid range keeps its snapshot slot zeroed and is not
    /// written back on restore.
    fn resolve(r: &RangeSpec) -> (usize, bool) {
        if crate::pools::is_dynamic(r.name) {
            let fresh = crate::pools::dynamic_base(r.name);
            let addr = if fresh != 0 { fresh } else { r.addr };
            (addr, crate::modules::range_committed(addr, r.len))
        } else {
            (r.addr, true)
        }
    }

    /// Log a skipped range once per range name per process.
    fn note_skipped(name: &'static str, addr: usize, len: usize) {
        use once_cell::sync::Lazy;
        use std::sync::Mutex;
        static SKIPPED: Lazy<Mutex<Vec<&'static str>>> = Lazy::new(|| Mutex::new(Vec::new()));
        let mut seen = SKIPPED.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.contains(&name) {
            crate::log::log(&format!(
                "[cosmo-rollback] snapshot: range {name} @0x{addr:08x} len 0x{len:x} is not \
                 committed process memory; skipping (zero-filled)"
            ));
            seen.push(name);
        }
    }

    /// Copy every range's bytes out of the game's address space, plus plugin state.
    ///
    /// # Safety
    /// Every range address/length must be a valid, readable mapping in this process.
    pub unsafe fn capture(rl: &'static RangeList) -> Self {
        // One-shot diagnostic: the match is running here, so the `.snp` is loaded.
        crate::transport::probe_channel_once();
        let total = rl.total_bytes();
        let mut data = vec![0u8; total];
        let mut off = 0usize;
        for r in &rl.ranges {
            let (addr, valid) = Self::resolve(r);
            if valid {
                let src = std::slice::from_raw_parts(addr as *const u8, r.len);
                data[off..off + r.len].copy_from_slice(src);
            } else {
                Self::note_skipped(r.name, addr, r.len);
            }
            off += r.len;
        }
        Snapshot {
            ranges: rl,
            data,
            blobs: plugin_state::capture_blobs(),
            region: region::capture(),
            image: image::capture(),
            tiles: tiles::capture(),
            creep: creep::capture(),
            replay: replay::capture(),
            scratch: pools::capture_scratch(),
        }
    }

    /// Copy the snapshot's bytes back into the game's address space. The flat engine
    /// ranges are restored first, then the plugin blob, so the plugin's pointer
    /// resolution sees the rolled-back unit/bullet pools (ADR-0005 §4.2).
    ///
    /// # Safety
    /// Same requirement as [`Snapshot::capture`], and the ranges must not have moved.
    pub unsafe fn restore(&self) {
        let mut off = 0usize;
        for r in &self.ranges.ranges {
            let (addr, valid) = Self::resolve(r);
            if valid {
                let dst = std::slice::from_raw_parts_mut(addr as *mut u8, r.len);
                dst.copy_from_slice(&self.data[off..off + r.len]);
            } else {
                Self::note_skipped(r.name, addr, r.len);
            }
            off += r.len;
        }
        plugin_state::restore_blobs(&self.blobs);
        region::restore(&self.region);
        image::restore(&self.image);
        tiles::restore(&self.tiles);
        creep::restore(&self.creep);
        replay::restore(&self.replay);
        pools::restore_scratch(&self.scratch);
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Each flat range's name and bytes, in snapshot order.
    pub fn range_iter(&self) -> impl Iterator<Item = (&'static str, &[u8])> + '_ {
        let mut off = 0usize;
        self.ranges.ranges.iter().map(move |r| {
            let b = &self.data[off..off + r.len];
            off += r.len;
            (r.name, b)
        })
    }

    /// The auxiliary blobs that are restored alongside the flat ranges and therefore
    /// belong in the state hash (see `state_hash::state_hash`).
    pub fn aux_image(&self) -> &[u8] {
        &self.image
    }

    pub fn aux_tiles(&self) -> &[u8] {
        &self.tiles
    }

    pub fn aux_creep(&self) -> &[u8] {
        &self.creep
    }

    /// The replay input cursor. Hashed too: it is the harness's input source, and a
    /// restore that misses it makes every later re-simulated frame inject the wrong
    /// commands (see `replay.rs`).
    pub fn aux_replay(&self) -> &[u8] {
        &self.replay
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Serialize the snapshot for a mid-match save (see `savefile`).
    ///
    /// The range *table itself* is not serialized — only each range's name and length,
    /// in order, so the loader can verify the save was made by a build with the same
    /// range set. A save from a different build (or a corrupted payload) fails at
    /// [`deserialize`](Self::deserialize) instead of restoring garbage into game memory.
    pub fn serialize(&self) -> Vec<u8> {
        use crate::savefile::Writer;
        let mut w = Writer::new();
        w.u32(self.ranges.ranges.len() as u32);
        for r in &self.ranges.ranges {
            w.bytes(r.name.as_bytes());
            w.u32(r.len as u32);
        }
        w.bytes(&self.data);
        w.u32(self.blobs.len() as u32);
        for b in &self.blobs {
            w.bytes(b);
        }
        w.bytes(&self.region);
        w.bytes(&self.image);
        w.bytes(&self.tiles);
        w.bytes(&self.creep);
        w.bytes(&self.replay);
        w.bytes(&self.scratch);
        w.into_bytes()
    }

    /// Deserialize a snapshot, verifying that the saved range description matches this
    /// build's range table exactly (names and lengths, in order) and that the payload
    /// parses completely.
    ///
    /// # Errors
    /// `"truncated"`/`"bad flag"` (corrupt payload), `"range count mismatch"` or
    /// `"range table mismatch"` (save from a different build), `"trailing bytes"`.
    pub fn deserialize(bytes: &[u8]) -> Result<Snapshot, &'static str> {
        use crate::savefile::Reader;
        let rl = crate::ranges::engine_ranges();
        let mut r = Reader::new(bytes);
        let n = r.u32()? as usize;
        if n != rl.ranges.len() {
            return Err("range count mismatch (different build?)");
        }
        for spec in &rl.ranges {
            let name = r.bytes()?;
            let len = r.u32()? as usize;
            if name != spec.name.as_bytes() || len != spec.len {
                return Err("range table mismatch (different build?)");
            }
        }
        let data = r.bytes()?.to_vec();
        if data.len() != rl.total_bytes() {
            return Err("snapshot data length mismatch");
        }
        let nb = r.u32()? as usize;
        if nb > 64 {
            return Err("blob count out of range");
        }
        let mut blobs = Vec::with_capacity(nb);
        for _ in 0..nb {
            blobs.push(r.bytes()?.to_vec());
        }
        let region = r.bytes()?.to_vec();
        let image = r.bytes()?.to_vec();
        let tiles = r.bytes()?.to_vec();
        let creep = r.bytes()?.to_vec();
        let replay = r.bytes()?.to_vec();
        let scratch = r.bytes()?.to_vec();
        r.finish()?;
        Ok(Snapshot {
            ranges: rl,
            data,
            blobs,
            region,
            image,
            tiles,
            creep,
            replay,
            scratch,
        })
    }
}
