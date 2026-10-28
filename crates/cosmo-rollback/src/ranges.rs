//! Snapshot range table for StarCraft 1.16.1.
//!
//! Addresses and lengths are transcribed from `samase_shim/src/bw.rs` (the
//! `whack_vars!` table), which is the authoritative 1.16.1 global layout used by
//! the GPTP plugins Cosmonarchy ships. The main exe is non-ASLR (base
//! `0x00400000`), so these are stable absolute addresses.
//!
//! NOTE (v1, provisional): this is a conservative first cut. Completeness is
//! established by the audit (`crate::audit`), not by this list. The `Game` struct
//! is not snapshotted whole here; its viewer-local fields must be excluded (see
//! `docs/decisions/0001-architecture.md` §3) and its exact 1.16.1 extent still
//! needs confirmation.

#![allow(dead_code)]

/// A resolved piece of mutable simulation memory.
#[derive(Debug, Clone, Copy)]
pub struct RangeSpec {
    pub name: &'static str,
    pub addr: usize,
    pub len: usize,
}

impl RangeSpec {
    pub const fn new(name: &'static str, addr: usize, len: usize) -> Self {
        RangeSpec { name, addr, len }
    }
    pub fn end(&self) -> usize {
        self.addr + self.len
    }
}

/// The set of ranges snapshotted each tick.
#[derive(Debug, Default)]
pub struct RangeList {
    pub ranges: Vec<RangeSpec>,
}

impl RangeList {
    pub fn add(&mut self, r: RangeSpec) {
        self.ranges.push(r);
    }
    pub fn total_bytes(&self) -> usize {
        self.ranges.iter().map(|r| r.len).sum()
    }
}

/// 1.16.1 engine globals, from `samase_shim/src/bw.rs` (`whack_vars!`).
/// The range table, built **once** per process.
///
/// Building it is expensive — it resolves every relocated pool and expands the per-object
/// field ranges into ~100k entries — and it never changes during a session, so every caller
/// shares one instance. Callers hold a `&'static RangeList`, which also lets a `Snapshot`
/// reference it instead of cloning 100k `RangeSpec`s on every capture.
pub fn engine_ranges() -> &'static RangeList {
    static ONCE: once_cell::sync::OnceCell<RangeList> = once_cell::sync::OnceCell::new();
    ONCE.get_or_init(build_ranges)
}

fn build_ranges() -> RangeList {
    crate::log::log("[cosmo-rollback] ranges: build_ranges begin");
    let mut rl = RangeList::default();
    crate::log::log("[cosmo-rollback] ranges: default list ready");

    // Object pools / linked-list heads.
    //
    // The pools' *storage*, not just the list heads, is simulation state: a restore
    // that rewinds the heads but not the entries they thread hands entries out twice
    // (ShieldBattery `rollback/ranges.rs`). Cosmonarchy's gptp relocates all of them
    // with the CCMU extender, so their live addresses come from `pools.rs` rather
    // than from the stock 1.16.1 constants.
    crate::pools::add_ranges(&mut rl);
    rl.add(RangeSpec::new("first_active_unit", 0x00628430, 4));
    rl.add(RangeSpec::new("first_hidden_unit", 0x006283EC, 4));
    rl.add(RangeSpec::new("first_active_bullet", 0x0064DEC4, 4));
    rl.add(RangeSpec::new("first_lone_sprite", 0x00654874, 4));
    rl.add(RangeSpec::new("first_fow_sprite", 0x00654868, 4));

    // Players / economy.
    rl.add(RangeSpec::new("players", 0x0057EEE0, 8 * 0x24));

    // Deterministic RNG.
    rl.add(RangeSpec::new("rng_seed", 0x0051CA14, 4));
    rl.add(RangeSpec::new("rng_enabled", 0x006D11C8, 4));

    // AI.
    rl.add(RangeSpec::new("ai_regions", 0x0069A604, 8 * 4));
    rl.add(RangeSpec::new("player_ai", 0x0068FEE8, 8 * 0x4e8));
    rl.add(RangeSpec::new("first_ai_script", 0x0068C100, 4));
    rl.add(RangeSpec::new("first_free_ai_script", 0x0068C0F8, 4));

    // Selections (per-player + client). Client selection is viewer-local and will
    // be excluded from the snapshot once the boundary is set; kept here for now so
    // the audit can show it changing.
    rl.add(RangeSpec::new("selections", 0x006284E8, 0xc * 8 * 4));

    // Pathing.
    rl.add(RangeSpec::new("pathing", 0x006D5BFC, 4));

    // The central game struct: frame_count, supplies, scores, tech/upgrade levels,
    // unit counts, locations, timers, etc. (bw_dat `Game`, size asserted 0x17700).
    //
    // Two runs of fields inside it are viewer-local and must stay out of the
    // snapshot: the camera's tile position, which local scrolling writes and only
    // rendering reads (restoring it yanks the view back), and the screen positions
    // saved with Shift+F2..F4, which F2..F4 jump back to (restoring them forgets a
    // position saved since). Splitting the block around them is what ShieldBattery
    // does with `VIEWER_LOCAL_GAME_FIELDS` (its `screen_pos_x_tiles` is at 0xe0 and
    // `saved_screen_positions` at 0x180, the same offsets here).
    const GAME_BASE: usize = 0x0057_F0F0;
    const GAME_LEN: usize = 0x17700;
    const CAMERA_OFF: usize = 0xe0; // screen_pos_x_tiles, screen_pos_y_tiles
    const CAMERA_LEN: usize = 4;
    const SAVED_SCREEN_OFF: usize = 0x180; // saved_screen_positions: [[u16; 2]; 3]
    const SAVED_SCREEN_LEN: usize = 12;
    rl.add(RangeSpec::new("game", GAME_BASE, CAMERA_OFF));
    rl.add(RangeSpec::new(
        "game.after_camera",
        GAME_BASE + CAMERA_OFF + CAMERA_LEN,
        SAVED_SCREEN_OFF - (CAMERA_OFF + CAMERA_LEN),
    ));
    rl.add(RangeSpec::new(
        "game.after_saved_screen",
        GAME_BASE + SAVED_SCREEN_OFF + SAVED_SCREEN_LEN,
        GAME_LEN - (SAVED_SCREEN_OFF + SAVED_SCREEN_LEN),
    ));

    // Scalar sim globals (samase_shim/src/bw.rs `whack_vars!`).
    rl.add(RangeSpec::new("frame_cooldown", 0x0051CE98, 4)); // decremented per step in StepObjects
    rl.add(RangeSpec::new("command_users", 0x00512678, 16)); // command_user/unique/storm/local
    rl.add(RangeSpec::new("is_paused", 0x006509C4, 4));
    rl.add(RangeSpec::new("active_iscript", 0x006D11F4, 12));
    rl.add(RangeSpec::new("unit_strength", 0x006BB210, 0xe4 * 2 * 4));
    rl.add(RangeSpec::new("guard_ais", 0x00685108, 8 * 2 * 4));
    rl.add(RangeSpec::new("active_ai_towns", 0x006AA050, 8 * 8));

    // Pool bookkeeping: the free-list head and tail and the live count beside each
    // object pool. These stay in the exe's `.data` (only the arrays themselves are
    // relocated by CCMU) and hold pointers *into* the pools the snapshot restores.
    //
    // Restoring a pool's storage without its free list is the failure ShieldBattery
    // warns about (`reference/ShieldBattery/game/src/rollback/ranges.rs:738-742`): the
    // list head left pointing past a rolled-back allocation hands the same entry out
    // twice, and one left pointing at an entry the rollback un-freed loses the rest of
    // the list. The divergence this fixes is in the pools' *free tails*, which is
    // exactly what a differently-threaded free list rewrites.
    //
    // Addresses are from `reference/cosmonarchy-gptp/BW/bwdata.h` except the unit and
    // order free lists, which are not named there and were read out of the pool
    // initializers by disassembly (unit_constructor 0x004A06C0, InitializeOrderArray
    // 0x0048C590) — see docs/analysis/ranges-additions.md §1.
    rl.add(RangeSpec::new("unit_free_head", 0x00628438, 4));
    rl.add(RangeSpec::new("unit_free_tail", 0x0062843C, 4));
    rl.add(RangeSpec::new("unit_count", 0x006283F0, 4));
    rl.add(RangeSpec::new("sprite_free_head", 0x0063FE30, 4));
    rl.add(RangeSpec::new("sprite_free_tail", 0x0063FE34, 4));
    rl.add(RangeSpec::new("sprite_count", 0x006C4A2C, 4));
    rl.add(RangeSpec::new("image_free_head", 0x0057EB68, 4));
    rl.add(RangeSpec::new("image_free_tail", 0x0057EB70, 4));
    rl.add(RangeSpec::new("order_free_head", 0x0064B2E0, 4));
    rl.add(RangeSpec::new("order_free_tail", 0x0064B2E4, 4));
    rl.add(RangeSpec::new("order_count", 0x00641698, 4));
    rl.add(RangeSpec::new("bullet_free_head", 0x0064EED8, 4));
    rl.add(RangeSpec::new("bullet_free_tail", 0x0064EEDC, 4));
    rl.add(RangeSpec::new("bullet_count", 0x0064DEBC, 4));
    rl.add(RangeSpec::new("thingy_free_head", 0x00654878, 4));
    rl.add(RangeSpec::new("thingy_free_tail", 0x0065487C, 4));
    rl.add(RangeSpec::new("fow_free_head", 0x006509D0, 4));
    rl.add(RangeSpec::new("fow_free_tail", 0x00654870, 4));

    // The remaining linked-list heads and tails the engine keeps beside the pools.
    // Same reasoning as the free lists: a head left on a later element changes what
    // the re-simulated frame walks.
    rl.add(RangeSpec::new("last_active_unit", 0x0059CC9C, 4));
    rl.add(RangeSpec::new("last_hidden_unit", 0x00628428, 4));
    rl.add(RangeSpec::new("first_dying_unit", 0x0062842C, 4));
    rl.add(RangeSpec::new("last_dying_unit", 0x0059CC98, 4));
    rl.add(RangeSpec::new("first_scanner_sweep", 0x006283F4, 4));
    rl.add(RangeSpec::new("last_scanner_sweep", 0x00628434, 4));
    rl.add(RangeSpec::new("first_psi_field_provider", 0x0063FF54, 4));
    rl.add(RangeSpec::new("first_invisible_unit", 0x0063FF5C, 4));
    rl.add(RangeSpec::new("first_player_unit", 0x006283F8, 12 * 4));
    rl.add(RangeSpec::new("last_lone_sprite", 0x0065291C, 4));
    rl.add(RangeSpec::new("last_fow_sprite", 0x0065486C, 4));
    rl.add(RangeSpec::new("psi_field_grp_width", 0x0063FF4C, 4));
    rl.add(RangeSpec::new("psi_field_grp_height", 0x0063FF58, 4));
    rl.add(RangeSpec::new("can_update_powered_status", 0x0063FF44, 4));
    rl.add(RangeSpec::new("units_recalled", 0x0063FF50, 4));

    // Bullet state that changes creation/spin without drawing from the synced RNG.
    // A bullet from the same unit as `last_bullet_creator` takes the opposite side
    // without an RNG draw, so a re-simulated frame that saw a later spawner draws a
    // different number of times (ShieldBattery `ranges.rs:398-409`).
    rl.add(RangeSpec::new("last_bullet", 0x0064DEAC, 4));
    rl.add(RangeSpec::new("last_bullet_index", 0x0064DEA8, 4));
    rl.add(RangeSpec::new("last_bullet_creator", 0x0064DEB0, 4));
    rl.add(RangeSpec::new("launch_spin_random", 0x0064DEC0, 4));

    // Trigger runtime state. The trigger step counts down `trigger_execution_timer`
    // and `trigger_elapsed_time_tick_timer` every frame; the latter is what advances
    // `Game.elapsed_seconds`, so leaving it out makes every re-simulated frame wind
    // the game clock down again and the clock runs fast. The per-player wait arrays
    // are the other half of the trigger pass state.
    // Grounded by disassembling the trigger step at 0x00489CC0-0x00489D98.
    rl.add(RangeSpec::new("trigger_execution_timer", 0x0065097C, 2));
    rl.add(RangeSpec::new("trigger_elapsed_time_tick_timer", 0x006509C0, 2));
    rl.add(RangeSpec::new("player_trigger_wait_timers", 0x00650980, 8 * 4));
    rl.add(RangeSpec::new("player_trigger_wait_active_flags", 0x006509B8, 8));
    rl.add(RangeSpec::new("current_trigger_player", 0x006509B0, 4));
    rl.add(RangeSpec::new("trigger_ran_flag", 0x006509A0, 2));

    // Other scalar sim globals: timers, counters and the visibility-checksum
    // accumulators that the sync ring folds into its per-tick hash.
    rl.add(RangeSpec::new("vision_updated", 0x0051CE9C, 1));
    rl.add(RangeSpec::new("turn_stalled", 0x0051CE9D, 1));
    rl.add(RangeSpec::new("turn_ticks_left", 0x0051CEA0, 4));
    // `g_sequence`: the per-turn command sequence counter, advanced as turns are
    // processed. Simulation state (it decides command ordering), and it must rewind with
    // a rollback or command attribution leaks across it (docs/analysis/turn-path.md §6).
    rl.add(RangeSpec::new("sequence", 0x006556E8, 4));
    rl.add(RangeSpec::new("order_timer_reset", 0x0059CCA4, 4));
    rl.add(RangeSpec::new("secondary_order_timer_reset", 0x006283E8, 4));
    rl.add(RangeSpec::new("unit_ordering_count", 0x0066FF74, 4));
    rl.add(RangeSpec::new("interceptor_focused_this_frame", 0x006957D0, 4));
    rl.add(RangeSpec::new("larva_counter", 0x0067D3F8, 2));
    rl.add(RangeSpec::new("draw_cursor_marker", 0x00652920, 1));
    rl.add(RangeSpec::new("current_visibility_hash", 0x0063FD30, 4));
    rl.add(RangeSpec::new("want_visibility_hash_update", 0x00629D90, 4));
    rl.add(RangeSpec::new("region_vision_hash", 0x0063FE38, 4));
    rl.add(RangeSpec::new("current_hash_offset", 0x0063FD28, 4));
    rl.add(RangeSpec::new("trig_remove_unit_active", 0x005971DC, 4));
    rl.add(RangeSpec::new("trig_killing_additional_unit", 0x00596CD4, 4));
    rl.add(RangeSpec::new("trig_kill_unit_count", 0x005971E0, 4));

    // Replay input cursors.
    //
    // The per-frame replay command injector (`0x00487100`, called inside the hooked
    // frame function when `is_replay` is set) reads `next_replay_command_frame` and
    // compares it with `Game.frame_count` to decide whether the commands staged for
    // this frame should be injected. `frame_count` is in the snapshot but these
    // cursors were not, so after a restore the cursor still pointed past the restored
    // frame: the re-simulated frame computed "not due yet" and injected **no**
    // commands, while the original frame had. That is the whole of the remaining
    // divergence (missed unit/order/overlay creation moves the pools' free lists and
    // the free-tail bytes).
    //
    // Addresses: `next_replay_command_frame` and `replay_end_frame` are the globals
    // the injector compares and advances (disassembly of 0x00487100-0x00487178);
    // `g_replay_cmd_count` is the staged-command count (bwdata.h:1432).
    //
    // This is replay-harness input. A live rollback game takes its input from the
    // turn/input table instead, so this pair is what makes the shadow gate valid on a
    // replay; it is harmless in a live game (the injector is not called there).
    rl.add(RangeSpec::new("next_replay_command_frame", 0x005136C8, 4));
    rl.add(RangeSpec::new("replay_end_frame", 0x006D0F31, 4));
    rl.add(RangeSpec::new("replay_cmd_count", 0x006D5BF0, 4));

    // State found by the reproducibility probe (`CM_ROLLBACK_DET`, `rollback/mod.rs`):
    // with everything above covered, a step run twice from the same restored state still
    // differed in these bytes, i.e. the engine reads them and they are not derivable
    // from the covered state. Each was confirmed to be a plain word in the exe's `.data`
    // (the probe reports any uncovered difference in `0x0050C000..0x006DE694`), not a
    // pointer to heap.
    //
    // Deliberately NOT snapshotted, because they change for reasons outside the
    // simulation and would make the shadow gate's hash differ forever: the RNG call
    // counters (`g_random_counts` 0x0051C610, the counter at 0x0051CA18), the wall-clock
    // pacing target `g_next_tick_ms` (0x0051CE94), and the dialog repaint region
    // (`g_dialog_refresh_region` 0x006D5E2C).
    rl.add(RangeSpec::new("probe_57eebc", 0x0057EEBC, 4));
    rl.add(RangeSpec::new("probe_59cc8c", 0x0059CC8C, 4));
    rl.add(RangeSpec::new("lakiz_splash_list_index", 0x0064EEC8, 4));
    // 0x006554B4 (between the replay command staging globals) is deliberately left out:
    // the probe shows it differs between two steps from the same restored state, i.e. it
    // is not restorable state (a buffer pointer that moves), and hashing it would make the
    // gate differ forever. Same for `g_turn_buffer` (0x00654880), whose pointer bytes
    // change for the same reason.
    rl.add(RangeSpec::new("mask_hash_current_x", 0x0065EB10, 4));
    rl.add(RangeSpec::new("mask_hash_current_y", 0x0065EB28, 4));
    rl.add(RangeSpec::new("mask_hash_next", 0x0065EB2C, 4));
    rl.add(RangeSpec::new("probe_65fae4", 0x0065FAE4, 4));
    rl.add(RangeSpec::new("probe_68fee4", 0x0068FEE4, 4));
    rl.add(RangeSpec::new("probe_69556c", 0x0069556C, 4));
    rl.add(RangeSpec::new("interceptor_focused_next", 0x006957D4, 4));
    rl.add(RangeSpec::new("probe_6aa04c", 0x006AA04C, 4));

    // NOTE: plugin `.data` sections (ADR-0004 Layer A) are intentionally NOT snapshotted
    // here — restoring them rewinds heap pointers/mutexes and corrupts the plugin. Plugin
    // state is handled by the plugin's own serializer instead (ADR-0005 Layer C).

    // Extra ranges for the kill-gate bisection, e.g. "0x5A8200:0x9B24A;0x640000:0x9B24A".
    if let Ok(s) = std::env::var("CM_ROLLBACK_EXTRA_RANGE") {
        for part in s.split(';') {
            let mut it = part.split(':');
            if let (Some(a), Some(l)) = (it.next(), it.next()) {
                if let (Some(a), Some(l)) = (parse_num(a), parse_num(l)) {
                    let name: &'static str =
                        Box::leak(format!("extra@0x{:x}", a).into_boxed_str());
                    rl.add(RangeSpec { name, addr: a, len: l });
                }
            }
        }
    }

    rl
}

fn parse_num(s: &str) -> Option<usize> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        usize::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}
