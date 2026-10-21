//! In-context instrumentation of the outer main loop's between-frame calls.
//!
//! `docs/analysis/restore-residual.md` shows the snapshot/re-sim boundary
//! (`0x004D9670`) is not a closed simulation step: between two calls to it the outer
//! main loop (`0x004D986D`..`0x004D9945`) runs a block of work that writes `rng_seed`
//! and the order free list — simulation state a re-sim that only re-runs the frame
//! cannot reproduce.
//!
//! The prior `CM_ROLLBACK_PROBE_OUTER` diagnostic *called* each between-frame call out
//! of its live context (the outer loop sets registers first), and it **omitted
//! `receive_network_messages` (0x485F70) entirely**. This module closes both gaps: it
//! installs a wrapping hook on each between-frame call *target*, so the callback fires
//! at the call site with the live registers the outer loop left, and it calls `orig`
//! (the real function) through the trampoline, hashing a focused set of
//! simulation-relevant globals *and* the full unit/sprite/image pools before and after
//! to name the exact mutator.
//!
//! Off unless `CM_ROLLBACK_TRACE_OUTER` is set, so normal play and the default gate are
//! untouched.

#![allow(dead_code)]

use crate::log::log;
use crate::state_hash::hash_bytes;

// Wrapping hooks on each between-frame call target. Signatures match the call sites in
// the outer loop (see `frame_hook.rs` for the stdcall vs cdecl split): the only call
// with an argument is `pump_messages(3)`; the rest take none.
whack_hooks!(stdcall, 0x00400000,
    0x004D1BF0 => TracePumpMessages(u32) -> u32;
    0x004D93F0 => TracePostFrame() -> u32;
);
whack_hooks!(0x00400000,
    0x004D14D0 => TraceUpdateCursor() -> u32;
    0x0047F0E0 => TraceScrollScreenStep() -> u32;
    0x00485F70 => TraceReceiveNetworkMessages() -> u32;
    0x0047F590 => TraceSendBnetLeagueQuery() -> u32;
    0x0041CA00 => TraceDialogRepaint() -> u32;
);

/// Whether `CM_ROLLBACK_TRACE_OUTER` asks to trace the outer loop's between-frame calls.
pub fn enabled() -> bool {
    std::env::var("CM_ROLLBACK_TRACE_OUTER").is_ok()
}

/// The simulation-relevant globals the boundary diff (`docs/analysis/restore-residual.md`)
/// names as mutating between frames: `rng_seed`, the order free list and count, the unit
/// free list, and the image/sprite counts. Each is `(name, address)`; read as `u32`.
const WATCH: &[(&str, usize)] = &[
    ("rng_seed", 0x0051_CA14),
    ("rng_calls", 0x0051_CA18),
    ("rng_enabled", 0x006D_11C8),
    ("order_free_head", 0x0064_B2E0),
    ("order_free_tail", 0x0064_B2E4),
    ("order_count", 0x0064_1698),
    ("unit_free_head", 0x0062_8438),
    ("unit_count", 0x0062_83F0),
    ("sprite_count", 0x006C_4A2C),
    ("image_free_head", 0x0057_EB68),
];

fn read_watch() -> [u32; 10] {
    let mut out = [0u32; 10];
    for (i, &(_, addr)) in WATCH.iter().enumerate() {
        out[i] = unsafe { *(addr as *const u32) };
    }
    out
}

/// Hash the full unit/sprite/image pools (the same bytes the snapshot covers) so a
/// between-frame call that writes render *fields inside an object* (not a count) is
/// caught. ~3.3 MB per call; bounded by the number of frames a probe reaches.
fn pool_hash() -> u64 {
    let p = crate::pools::get();
    let mut h = 0u64;
    unsafe {
        for (base, len) in [
            (p.unit_table, crate::pools::UNIT_LIMIT * crate::pools::UNIT_SIZE),
            (
                p.sprite_table,
                crate::pools::SPRITE_LIMIT * crate::pools::SPRITE_SIZE,
            ),
            (p.image_table, crate::pools::IMAGE_LIMIT * crate::pools::IMAGE_SIZE),
        ] {
            let bytes = std::slice::from_raw_parts(base as *const u8, len);
            h ^= hash_bytes(bytes);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn report(name: &str, before: &[u32; 10], after: &[u32; 10], ph_before: u64, ph_after: u64) {
    if before == after && ph_before == ph_after {
        return;
    }
    let mut line = String::new();
    for (i, &(n, _)) in WATCH.iter().enumerate() {
        if before[i] != after[i] {
            line.push_str(&format!(" {n} 0x{:08x}->0x{:08x};", before[i], after[i]));
        }
    }
    if ph_before != ph_after {
        line.push_str(&format!(" POOLS 0x{ph_before:016x}->0x{ph_after:016x}"));
    }
    log(&format!("[cosmo-rollback] OUTER-TRACE {name} changed:{line}"));
}

/// Install the tracing hooks. Called from `frame_hook::install` only when
/// [`enabled`] is true, so the default build/behaviour is unchanged.
pub fn install(exe: &mut whack::ModulePatcher) {
    log("[cosmo-rollback] OUTER-TRACE: hooking outer loop between-frame calls");
    unsafe {
        exe.hook_closure(
            TracePumpMessages,
            move |arg: u32, orig: unsafe extern "C" fn(u32) -> u32| -> u32 {
                let before = read_watch();
                let ph = pool_hash();
                let r = orig(arg);
                report("pump_messages(3)", &before, &read_watch(), ph, pool_hash());
                r
            },
        );
        exe.hook_closure(
            TraceUpdateCursor,
            move |orig: unsafe extern "C" fn() -> u32| -> u32 {
                let before = read_watch();
                let ph = pool_hash();
                let r = orig();
                report("update_cursor", &before, &read_watch(), ph, pool_hash());
                r
            },
        );
        exe.hook_closure(
            TraceScrollScreenStep,
            move |orig: unsafe extern "C" fn() -> u32| -> u32 {
                let before = read_watch();
                let ph = pool_hash();
                let r = orig();
                report("scroll_screen_step", &before, &read_watch(), ph, pool_hash());
                r
            },
        );
        exe.hook_closure(
            TraceReceiveNetworkMessages,
            move |orig: unsafe extern "C" fn() -> u32| -> u32 {
                let before = read_watch();
                let ph = pool_hash();
                let r = orig();
                report(
                    "receive_network_messages",
                    &before,
                    &read_watch(),
                    ph,
                    pool_hash(),
                );
                r
            },
        );
        exe.hook_closure(
            TraceSendBnetLeagueQuery,
            move |orig: unsafe extern "C" fn() -> u32| -> u32 {
                let before = read_watch();
                let ph = pool_hash();
                let r = orig();
                report(
                    "send_bnet_league_query",
                    &before,
                    &read_watch(),
                    ph,
                    pool_hash(),
                );
                r
            },
        );
        exe.hook_closure(
            TracePostFrame,
            move |orig: unsafe extern "C" fn() -> u32| -> u32 {
                let before = read_watch();
                let ph = pool_hash();
                let r = orig();
                report("post_frame(0x4D93F0)", &before, &read_watch(), ph, pool_hash());
                r
            },
        );
        exe.hook_closure(
            TraceDialogRepaint,
            move |orig: unsafe extern "C" fn() -> u32| -> u32 {
                let before = read_watch();
                let ph = pool_hash();
                let r = orig();
                report(
                    "dialog_repaint(0x41CA00)",
                    &before,
                    &read_watch(),
                    ph,
                    pool_hash(),
                );
                r
            },
        );
    }
    log("[cosmo-rollback] OUTER-TRACE: installed");
}
