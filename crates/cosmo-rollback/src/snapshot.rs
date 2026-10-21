//! A snapshot is a memcpy of every range in a `RangeList`, plus any plugin-state blob
//! (ADR-0005 Layer C), plus the range table it was taken with so a restore is
//! self-describing. Mirrors ShieldBattery's `rollback/snapshot.rs`.

use crate::creep;
use crate::image;
use crate::plugin_state;
use crate::pools;
use crate::ranges::RangeList;
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
            let src = std::slice::from_raw_parts(r.addr as *const u8, r.len);
            data[off..off + r.len].copy_from_slice(src);
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
            let dst = std::slice::from_raw_parts_mut(r.addr as *mut u8, r.len);
            dst.copy_from_slice(&self.data[off..off + r.len]);
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
}
