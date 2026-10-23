//! The game-logic frame hook.
//!
//! We install our own `whack` trampoline on the per-frame game function at
//! **0x004D9670** so that we receive `orig` and can call it zero, one, or many
//! times (re-simulation needs that; `samase_shim`'s `hook_step_objects` is a
//! notification hook with no `orig`).
//!
//! Why 0x004D9670 and not `StepObjects` (0x004D94B0): the main game loop
//! (0x004D98EC–0x004D994D) calls 0x004D9670 once per frame with `ecx=0`, and
//! 0x004D9670 increments `Game.frame_count` (0x0057F23C) before calling
//! `StepObjects`. Hooking `StepObjects` alone can't reproduce `frame_count` (it is
//! advanced outside it), which the shadow gate showed as a divergence on every
//! frame. Hooking 0x004D9670 makes "one call = one frame" true. It also avoids
//! colliding with `aise`, which hooks `StepObjects`.

use crate::log::log;

whack_hooks!(stdcall, 0x00400000,
    0x004D9670 => GameFrame(@ecx u32) -> u32;
    // The turn scheduler. `0x004D9550` decrements `g_turn_ticks_left` (0x0051CEA0) and,
    // when it reaches 0, runs the network step (`0x00487070`); if a peer's turn is missing
    // it sets `g_turn_stalled` (0x0051CE9D) and returns 0, which makes `run_game_tick`
    // skip the frame entirely. Returning non-zero makes the frame run.
    0x004D9550 => TurnScheduler(@ecx u32) -> u32;
);

// The outer main loop's per-iteration work (`0x004D986D`–`0x004D9945`): the calls it makes
// *between* two frame calls. They advance simulation state (RNG, order allocation) as well
// as render state, so a re-simulation that only re-runs `0x004D9670` cannot reach the live
// state — the boundary problem in `docs/analysis/restore-residual.md`. Callable so a re-sim
// can run them too (`CM_ROLLBACK_RESIM_LOOP=1`).
whack_funcs!(stdcall, init_outer_stdcall, 0x00400000,
    0x004D93F0 => PostFrame();
    0x004D1BF0 => PumpMessages(u32);
);
whack_funcs!(init_outer_cdecl, 0x00400000,
    0x004D14D0 => UpdateCursor();
    0x0047F0E0 => ScrollScreenStep();
    0x00485F70 => ReceiveNetworkMessages();
    0x0047F590 => SendBnetLeagueQuery();
    0x0041CA00 => DialogRepaint();
);

static PATCHER: std::sync::Mutex<whack::Patcher> = std::sync::Mutex::new(whack::Patcher::new());

/// Run `f` over the exe's module patcher (the one `install` used). Used by
/// `farm.rs` for its one-shot game-start patches (unpaced stepping + render
/// skip once the match has started).
pub fn patch_exe(f: impl FnOnce(&mut whack::ModulePatcher)) {
    let mut patcher = match PATCHER.lock() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut exe = patcher.patch_exe(0x00400000);
    f(&mut exe);
}

pub fn install() {
    let mut patcher = match PATCHER.lock() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut exe = patcher.patch_exe(0x00400000);
    // Turn capture + injection primitives (see `turn_hook.rs`).
    crate::turn_hook::install(&mut exe);
    // Farm driver patches: unpaced stepping + render skip (see `farm.rs`).
    crate::farm::install(&mut exe);
    // The live client (CM_ROLLBACK_LIVE) additionally suppresses the engine's own turn path.
    crate::rollback::live::install(&mut exe);
    // In-context trace of the outer loop's between-frame calls (`CM_ROLLBACK_TRACE_OUTER`).
    if crate::outer_trace::enabled() {
        crate::outer_trace::install(&mut exe);
    }
    unsafe {
        init_outer_stdcall(&mut exe);
        init_outer_cdecl(&mut exe);
        exe.hook_closure(
            GameFrame,
            move |ecx: u32, orig: unsafe extern "C" fn(u32) -> u32| -> u32 {
                crate::rollback::on_frame(ecx, orig)
            },
        );
        // Make one call to the hooked function advance exactly one logic frame.
        // `0x004D9670` is not a single frame: its body is a loop (back-edge at
        // `0x004D9823`) that runs `K = min([0x5124D4], frames until the wall clock
        // reaches g_next_tick_ms 0x0051CE94)` logic frames per call, so `K` depends on
        // real time. The rollback engine treats one `orig()` call as one frame, so a
        // re-sim of `f - g` calls would advance a different number of frames than the
        // original path did — the residual divergence the shadow gate reports.
        //
        // NOPing the 5-byte back-edge `jmp 0x4D96A7` makes control fall through to the
        // epilogue at `0x004D9828`, so the loop body runs once. Pacing is unaffected:
        // the outer per-rendered-frame loop (`0x004D986D`) only calls this function once
        // the wall clock has reached g_next_tick_ms, and the function advances that
        // itself, so the game still runs one frame per frame duration. What is lost is
        // the catch-up burst after a stall, which is the non-determinism a rollback game
        // must not have. See `docs/analysis/hook-boundary.md`.
        const BACK_EDGE: usize = 0x004D_9823;
        const EXPECTED: [u8; 5] = [0xE9, 0x7F, 0xFE, 0xFF, 0xFF]; // jmp 0x004D96A7
        let actual = std::slice::from_raw_parts(BACK_EDGE as *const u8, EXPECTED.len());
        if actual == EXPECTED {
            exe.nop(BACK_EDGE, EXPECTED.len());
            let after = std::slice::from_raw_parts(BACK_EDGE as *const u8, EXPECTED.len());
            log(&format!(
                "[cosmo-rollback] catch-up loop removed: NOP at 0x{BACK_EDGE:08x} (now {after:02x?}; one call = one logic frame)"
            ));
        } else {
            log(&format!(
                "[cosmo-rollback] catch-up loop NOT patched: 0x{BACK_EDGE:08x} holds {actual:02x?}, expected {EXPECTED:02x?}"
            ));
        }
        // Drive-mode suppression of the replay command injector (default "nop" method):
        // gptp `call_patch(replay_loop, 0x004D96C4)` replaces the stock `call 0x487100`
        // there with a call to its own `replay_loop`, which injects the replay's commands.
        // NOP that 5-byte call so `orig()` runs the frame without any replay commands,
        // leaving `g_sync_turn_enabled` and the sync-turn machinery intact (zeroing the
        // flag instead hangs the replay — see `docs/analysis/turn-drive-loop.md` §1).
        if crate::turn_hook::drive_suppress_nop() {
            const REPLAY_CALL: usize = 0x004D_96C4;
            let actual = std::slice::from_raw_parts(REPLAY_CALL as *const u8, 5);
            if actual[0] == 0xE8 {
                exe.nop(REPLAY_CALL, 5);
                let after = std::slice::from_raw_parts(REPLAY_CALL as *const u8, 5);
                log(&format!(
                    "[cosmo-rollback] replay loop suppressed: NOP at 0x{REPLAY_CALL:08x} (was 0xE8, now {after:02x?})"
                ));
            } else {
                log(&format!(
                    "[cosmo-rollback] replay loop NOT suppressed: 0x{REPLAY_CALL:08x} holds {actual:02x?} (op 0x{:02x}), expected a call",
                    actual[0]
                ));
            }
        }
        // The turn scheduler: optionally force one turn per frame and never stall.
        //
        // `0x004D9550` returns 1 to let `run_game_tick` run the frame and 0 to stall it
        // (when a peer's turn has not arrived). `g_turn_ticks_left` (0x0051CEA0) counts
        // down to decide when the network step runs, so one *turn* spans several frames —
        // but `crate::turns::InputTable` is keyed on one turn per step. `CM_ROLLBACK_FORCE_TURN=1`
        // sets the countdown to 1 before the scheduler runs (so the network step runs
        // every frame = one turn per frame) and turns a stall into a run by clearing
        // `g_turn_stalled` and returning 1. See `docs/analysis/turn-path.md` §5/§7.
        exe.hook_closure(
            TurnScheduler,
            move |ecx: u32, orig: unsafe extern "C" fn(u32) -> u32| -> u32 {
                let ticks_in = unsafe { *(TURN_TICKS_LEFT as *const u32) };
                let stalled_in = unsafe { *(TURN_STALLED as *const u8) };
                if !force_turn() {
                    let r = orig(ecx);
                    // Cadence trace: without FORCE_TURN this shows the native turn
                    // boundary (ticks_in == 1 => the network step runs this frame), which
                    // is what sets the turn-per-frame ratio rollback must key on.
                    trace(ticks_in, stalled_in, r, false);
                    return r;
                }
                // Make the network step run this frame.
                *(TURN_TICKS_LEFT as *mut u32) = 1;
                let r = orig(ecx);
                let stalled_out = unsafe { *(TURN_STALLED as *const u8) };
                trace(ticks_in, stalled_out, r, true);
                if r == 0 {
                    // A peer's turn is missing: pretend it arrived, so the frame runs
                    // instead of being skipped.
                    *(TURN_STALLED as *mut u8) = 0;
                    if FORCE_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                        log(&format!(
                            "[cosmo-rollback] turn scheduler: forced ready at frame_count {} (sequence {})",
                            *(0x0057_F23C as *const u32),
                            *(SEQUENCE as *const u32)
                        ));
                    }
                    return 1;
                }
                r
            },
        );
        log("[cosmo-rollback] installed TurnScheduler trampoline at 0x004D9550");
    }
    log("[cosmo-rollback] installed GameFrame trampoline at 0x004D9670");
}

/// `g_turn_ticks_left` — frames until the next network step.
const TURN_TICKS_LEFT: usize = 0x0051_CEA0;
/// `g_turn_stalled` — set while a peer's turn is missing.
const TURN_STALLED: usize = 0x0051_CE9D;
/// `g_sequence` — the per-turn command sequence.
const SEQUENCE: usize = 0x0065_56E8;

static FORCE_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Calls to the turn scheduler logged by [`trace`] before it goes quiet.
static TRACE_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Log the turn-scheduler cadence for the first few calls.
///
/// `ticks_in` is `g_turn_ticks_left` on entry; a value of 1 means `orig` took the
/// turn boundary this frame (the network step runs), any other value means it did not.
/// `r == 0` is the stall (the frame was skipped). The ratio of boundaries to calls is
/// the turn↔frame mapping (`docs/analysis/turn-path.md` §0/§7) — the number the input
/// table must key on.
fn trace(ticks_in: u32, stalled: u8, r: u32, forced: bool) {
    let n = TRACE_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 6 || n % 64 == 0 {
        log(&format!(
            "[cosmo-rollback] turn scheduler #{}: ticks_in={ticks_in} stalled={stalled} ret={r} forced={} frame_count={} sequence={}",
            n,
            forced as u8,
            unsafe { *(0x0057_F23C as *const u32) },
            unsafe { *(SEQUENCE as *const u32) },
        ));
    }
}

/// Whether `CM_ROLLBACK_FORCE_TURN` asks for one turn per frame with no stalling.
fn force_turn() -> bool {
    // The live client must never let the engine skip a frame for a missing peer turn: it
    // supplies every turn itself.
    std::env::var("CM_ROLLBACK_FORCE_TURN").is_ok() || crate::rollback::live::enabled()
}

/// Runs the outer main loop's post-frame update (`0x004D93F0`) once.
pub unsafe fn post_frame() {
    (*PostFrame)();
}

/// The outer main loop's between-frame calls, individually, so a diagnostic can find which
/// of them advances the simulation (`CM_ROLLBACK_PROBE_OUTER`).
pub unsafe fn pump_messages() {
    (*PumpMessages)(3);
}
pub unsafe fn update_cursor() {
    (*UpdateCursor)();
}
pub unsafe fn scroll_screen_step() {
    (*ScrollScreenStep)();
}
pub unsafe fn receive_network_messages() {
    (*ReceiveNetworkMessages)();
}
pub unsafe fn send_bnet_league_query() {
    (*SendBnetLeagueQuery)();
}
pub unsafe fn dialog_repaint() {
    (*DialogRepaint)();
}

/// Every between-frame call, in the outer loop's order, by name.
pub fn outer_calls() -> [(&'static str, unsafe fn()); 7] {
    [
        ("pump_messages", pump_messages),
        ("update_cursor", update_cursor),
        ("scroll_screen_step", scroll_screen_step),
        ("receive_network_messages", receive_network_messages),
        ("send_bnet_league_query", send_bnet_league_query),
        ("post_frame", post_frame),
        ("dialog_repaint", dialog_repaint),
    ]
}

/// Runs the whole outer-loop iteration's work between two frame calls, once, for a
/// re-simulation (`CM_ROLLBACK_RESIM_LOOP=1`).
pub unsafe fn outer_loop_work() {
    for (_, call) in outer_calls() {
        unsafe { call() };
    }
}
