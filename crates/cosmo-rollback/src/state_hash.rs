//! State hashes over the snapshot.
//!
//! Two hashes, for two jobs:
//!
//! - [`state_hash`] — FNV-1a 64 over every captured byte. This is the *local* kill gate's
//!   hash: it answers "did a re-simulation reproduce exactly what the engine produced?",
//!   where pointer values are fine to include because it is the same process.
//! - [`desync_hash`] — a *pointer-free* view, for comparing one client's state against
//!   another's. Pointer bytes cannot be compared across processes: the object pools live
//!   in `gptp.qdp`'s `.data` and the `.qdp` modules are relocated per process, so every
//!   pointer into a pool has a different value on each client. Hashing them would report
//!   a desync on every frame.

use crate::snapshot::Snapshot;

pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The local gate's hash: every captured byte, plus the auxiliary blobs that are restored
/// with them. The reveal region is deliberately left out — it is rebuilt from a rect list
/// on restore, and the list can legitimately come back in a different order.
///
/// Ranges named `*.render` are also left out: they are the pool fields the outer main loop
/// writes *between* two logic frames, so no re-simulation of the frame can reproduce them
/// (`pools.rs` `UNIT_RENDER`). They are still snapshotted, so a restore puts them back.
pub fn state_hash(snap: &Snapshot) -> u64 {
    state_hash_ignoring(snap, &[])
}

/// `state_hash` with named ranges left out, for the shadow gate.
///
/// The gate compares the *whole* state, so a range that the render pass legitimately rewrites
/// between frames (`sprite_count`: the draw pass allocates and frees helper sprites) shows up
/// as a divergence that cannot be fixed by restoring more state. The live client already
/// excludes it from the cross-client hash; this lets the gate exclude the same range
/// explicitly (from `CM_ROLLBACK_GATE_IGNORE`) instead of hiding it globally.
pub fn state_hash_ignoring(snap: &Snapshot, ignore: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, bytes) in snap.range_iter() {
        if name.ends_with(".render") || ignore.contains(&name) {
            continue;
        }
        h ^= hash_bytes(bytes);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for extra in [
        snap.aux_image(),
        snap.aux_tiles(),
        snap.aux_creep(),
        snap.aux_replay(),
    ] {
        h ^= hash_bytes(extra);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Mix one labelled blob into a hash. The label is mixed in so that two ranges swapping
/// contents cannot collide, and the mixing is order-sensitive.
fn mix(h: u64, label: &str, bytes: &[u8]) -> u64 {
    let mut h = h ^ hash_bytes(label.as_bytes());
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    h ^= hash_bytes(bytes);
    h.wrapping_mul(0x0000_0100_0000_01b3)
}

/// Whether a range is part of the cross-client comparison (i.e. listed in
/// [`POINTER_FREE`]), for diagnostics.
pub fn is_compared(name: &str) -> bool {
    POINTER_FREE.contains(&name)
}

/// A hash that can be compared **between clients**, for desync detection.
///
/// Covers only the state known to be pointer-free: scalars, index arrays, and the
/// auxiliary blobs (tile flags, creep, and the image iscript fields — all plain values).
/// Pointer-bearing structs are not covered; where they matter they are compared through
/// scalar projections (a unit's hitpoints and position, not its `sprite` pointer).
///
/// The list is deliberately conservative: a range is included only where its bytes are
/// certainly values. A name that is missing costs coverage, never a false desync.
pub fn desync_hash(snap: &Snapshot) -> u64 {
    // The range table has ~100k per-object entries, so membership must not be a linear scan
    // of the name list (that is millions of string compares per frame).
    use std::collections::HashSet;
    static FREE: once_cell::sync::Lazy<HashSet<&'static str>> =
        once_cell::sync::Lazy::new(|| POINTER_FREE.iter().copied().collect());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, bytes) in snap.range_iter() {
        if FREE.contains(name) {
            h = mix(h, name, bytes);
        }
    }
    // The auxiliary blobs (tile flags, creep, image iscript) are deliberately **not** hashed
    // here. They are still snapshotted — a restore puts them back, which rollback needs — but
    // the outer main loop writes them *between* two logic frames (`CM_ROLLBACK_BOUNDARY` shows
    // entry != exit on most frames with no flat range differing, i.e. only these blobs), and
    // how many times the outer loop iterates between frames is wall-clock dependent. Hashing
    // them therefore reports a desync between two clients that are in fact in step.
    h
}

/// Flat ranges whose bytes are values, not addresses, so they can be compared between
/// processes. See [`desync_hash`].
const POINTER_FREE: &[&str] = &[
    // Engine scalars, counters and timers.
    "rng_seed",
    "rng_enabled",
    "frame_cooldown",
    "is_paused",
    "vision_updated",
    "turn_stalled",
    "turn_ticks_left",
    "unit_count",
    // `sprite_count` is deliberately **not** compared: the draw pass allocates and frees
    // high-index *helper* sprites outside the frame (see `pools.rs` `SPRITE_SIM`), so the count
    // is outer-written and wall-clock dependent. Two clients a frame out of phase on it were
    // the source of the last cross-client desync report (found with the `live fp` fingerprint).
    "order_count",
    "bullet_count",
    "order_timer_reset",
    "secondary_order_timer_reset",
    "unit_ordering_count",
    "interceptor_focused_this_frame",
    "interceptor_focused_next",
    "larva_counter",
    "draw_cursor_marker",
    "current_visibility_hash",
    "want_visibility_hash_update",
    "region_vision_hash",
    "current_hash_offset",
    "trig_remove_unit_active",
    "trig_killing_additional_unit",
    "trig_kill_unit_count",
    // The replay cursors (`next_replay_command_frame`, `replay_end_frame`, `replay_cmd_count`)
    // are deliberately not listed: they are harness-specific input, not simulation state
    // (ADR-0005), so two clients in a replay sit at different cursors and hashing them reports
    // a desync that is not one. In a live match they are inert.
    "lakiz_splash_list_index",
    "mask_hash_current_x",
    "mask_hash_current_y",
    "mask_hash_next",
    // Probe-derived scalars (see `ranges.rs`); they step like counters. Verified by
    // their use sites (see `docs/analysis/ranges-additions.md` §6): `57eebc` is a turn
    // counter (`inc` per network step), `59cc8c`/`69556c` are countdowns, `68fee4`/
    // `6aa04c` are round-robin cursors 0..7 — all values, safe across processes.
    "probe_57eebc",
    "probe_59cc8c",
    "probe_68fee4",
    "probe_69556c",
    "probe_6aa04c",
    // `probe_65fae4` is deliberately NOT here: it is written through a computed address
    // (it has no direct reference in the exe) and whether its bytes are values or
    // addresses is unproven, so including it risks a false desync on every frame. A
    // missing name costs coverage, never a false desync.
    // Trigger per-frame timers and per-player trigger state (flags and timers).
    "trigger_execution_timer",
    "trigger_elapsed_time_tick_timer",
    "player_trigger_wait_timers",
    "player_trigger_wait_active_flags",
    "current_trigger_player",
    "trigger_ran_flag",
    // Per-player scalar arrays (strength values, not addresses).
    "unit_strength",
    // Index arrays: entries are pool indices, not addresses. (The unit-search and unit-query
    // scratch arrays are no longer snapshotted at all — they are rebuilt every frame; see
    // `pools.rs`.)
    "air_splash_list",
    // The UnitPath pool is snapshotted and restored (the movement divergence needs it), but it
    // is **not** compared across clients: it holds per-frame path *working* state
    // (`is_calculating`, the `steps` being computed, `cur_segments`), which depends on the
    // moment the hash is taken and heals again — a fingerprint dump showed both clients'
    // ranges identical at the moment a desync was reported, while the recorded frame-1 hashes
    // differed. `first_free_path` is likewise a pointer.
];

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: u64 = 0xcbf2_9ce4_8422_2325;

    #[test]
    fn mixing_is_order_and_label_sensitive() {
        let a = mix(mix(SEED, "x", b"ab"), "y", b"cd");
        let b = mix(mix(SEED, "y", b"cd"), "x", b"ab");
        assert_ne!(a, b, "the order of ranges must matter");
        let c = mix(mix(SEED, "x", b"ab"), "z", b"cd");
        assert_ne!(a, c, "the range name must matter");
    }

    #[test]
    fn identical_state_hashes_identically() {
        let rows: [(&str, &[u8]); 2] = [
            ("rng_seed", b"1234"),
            ("unit_count", b"\x05\x00\x00\x00"),
        ];
        let h = rows.iter().fold(SEED, |h, (n, b)| mix(h, n, b));
        let g = rows.iter().fold(SEED, |h, (n, b)| mix(h, n, b));
        assert_eq!(h, g);
    }

    #[test]
    fn changed_state_hashes_differently() {
        let a = mix(SEED, "rng_seed", b"1234");
        let b = mix(SEED, "rng_seed", b"1235");
        assert_ne!(a, b);
    }

    #[test]
    fn the_pointer_free_list_is_sane() {
        assert!(POINTER_FREE.iter().all(|n| !n.is_empty()));
        // No duplicates: a repeated name would mix the same range twice.
        let mut sorted = POINTER_FREE.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "duplicate entries in POINTER_FREE");
    }
}
