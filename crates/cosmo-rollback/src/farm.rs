//! Farm driver: unpaced stepping and scripted command streams.
//!
//! The desync farm runs two headless clients per pair at the highest frame rate the
//! CPU allows, and drives both clients with identical per-step command streams so
//! every divergence the hash adjudication reports is a real desync (same inputs,
//! different outputs) rather than an input difference.
//!
//! # Unpaced stepping
//!
//! The outer main loop (`0x004D986D`, see `docs/analysis/hook-boundary.md` §6) gates
//! the frame call on the wall clock:
//!
//! ```text
//! 004D98F6  mov eax,[ebp-0xc]          ; now (GetTickCount)
//! 004D98F9  sub eax,[0x51ce94]         ; now - g_next_tick_ms
//! 004D98FF  js 0x4d990f                ; ahead of schedule -> skip the frame
//! 004D9901  xor ecx,ecx
//! 004D9903  call 0x4d9670              ; run_game_tick (our hooked frame)
//! ...
//! 004D9933  call 0x41ca00              ; DialogRepaint (draw pass)
//! ```
//!
//! `CM_ROLLBACK_UNPACED` NOPs the `js` so the frame always runs: `g_next_tick_ms`
//! keeps advancing by the frame duration inside `run_game_tick`, and the lag clamp
//! at `0x004D98AC` re-syncs it to `now` each pass, so the loop free-runs at CPU
//! speed. `CM_ROLLBACK_NO_RENDER` additionally NOPs the `DialogRepaint` call: the
//! draw pass only rewrites render fields the state hash excludes (see
//! `docs/analysis/boundary-fix.md` §2), which is exactly the model the shadow
//! gate's re-simulation already treats as correct.
//!
//! # Command streams
//!
//! `CM_ROLLBACK_TURN_STREAM=<path>` (player 0) and `CM_ROLLBACK_TURN_STREAM_1=<path>`
//! (player 1) name files whose lines are turns: each line is the turn's bytes as
//! hex (`05` is the no-op `GameCommand_Sequence`), `#` starts a comment, blank
//! lines are no-ops. One line is injected per step via `turn_hook::inject_turn`,
//! wrapping at end of file. Both clients run the same file, so the injected bytes
//! are identical on both sides — the input distribution of the run is the stream,
//! not the network.
//!
//! Streams require lockstep (`CM_ROLLBACK_PREDICT=0`): a rollback re-simulation
//! does not replay the injection, so with prediction enabled a rolled-back frame
//! would diverge by harness design. A run that arms streams with prediction on
//! logs one warning and still injects.

use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::log::log;
use crate::turns::SYNC_NOP;

/// Whether `CM_ROLLBACK_UNPACED` asks to remove the wall-clock pace.
pub fn unpaced_enabled() -> bool {
    std::env::var("CM_ROLLBACK_UNPACED").is_ok()
}

/// Whether `CM_ROLLBACK_NO_RENDER` asks to skip the outer repaint.
pub fn no_render_enabled() -> bool {
    std::env::var("CM_ROLLBACK_NO_RENDER").is_ok()
}

/// Both game-start patches are one-shot.
static GAME_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install-time patches: only the repaint skip, when explicitly asked
/// (`CM_ROLLBACK_NO_RENDER`). The farm does **not** use this — it wants the
/// menus rendered (OCR navigation) and paced (low load).
pub fn install(exe: &mut whack::ModulePatcher) {
    if no_render_enabled() {
        nop_repaint(exe);
    }
}

/// Apply the farm's game-start patches once the match has started
/// (`CM_ROLLBACK_UNPACED`): remove the wall-clock pace gate so the simulation
/// free-runs, and skip the draw pass so a headless instance spends no time
/// presenting frames the farm never looks at. The menus stay paced and
/// rendered, which the OCR navigation needs.
///
/// Called from `live.rs` on the first in-game frame.
pub fn unpace_and_hide_render() {
    if !unpaced_enabled() || GAME_ARMED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // The patches go through the shared patcher the frame hook owns.
    crate::frame_hook::patch_exe(nop_pace_gate);
    crate::frame_hook::patch_exe(nop_repaint);
}

fn nop_pace_gate(exe: &mut whack::ModulePatcher) {
    const GATE: usize = 0x004D_98FF;
    const EXPECTED: [u8; 2] = [0x78, 0x0E]; // js 0x4d990f
    let actual = unsafe { std::slice::from_raw_parts(GATE as *const u8, EXPECTED.len()) };
    if actual == EXPECTED {
        unsafe { exe.nop(GATE, EXPECTED.len()) };
        let after = unsafe { std::slice::from_raw_parts(GATE as *const u8, EXPECTED.len()) };
        log(&format!(
            "[cosmo-rollback] farm: pace gate removed at 0x{GATE:08x} (now {after:02x?})"
        ));
    } else {
        log(&format!(
            "[cosmo-rollback] farm: pace gate NOT removed: 0x{GATE:08x} holds {actual:02x?}, expected {EXPECTED:02x?}"
        ));
    }
}

fn nop_repaint(exe: &mut whack::ModulePatcher) {
    const REPAINT: usize = 0x004D_9933;
    const EXPECTED: [u8; 5] = [0xE8, 0xC8, 0x30, 0xF4, 0xFF]; // call 0x41ca00
    let actual = unsafe { std::slice::from_raw_parts(REPAINT as *const u8, EXPECTED.len()) };
    if actual == EXPECTED {
        unsafe { exe.nop(REPAINT, EXPECTED.len()) };
        let after = unsafe { std::slice::from_raw_parts(REPAINT as *const u8, EXPECTED.len()) };
        log(&format!(
            "[cosmo-rollback] farm: repaint call removed at 0x{REPAINT:08x} (now {after:02x?})"
        ));
    } else {
        log(&format!(
            "[cosmo-rollback] farm: repaint call NOT removed: 0x{REPAINT:08x} holds {actual:02x?}, expected {EXPECTED:02x?}"
        ));
    }
}

/// One player's scripted turn stream.
struct StreamFile {
    slot: u32,
    path: String,
    turns: Vec<Vec<u8>>,
}

struct Streams {
    streams: Vec<StreamFile>,
    predict_warned: bool,
    inject_logged: u32,
}

static STREAMS: Lazy<Mutex<Streams>> = Lazy::new(|| {
    Mutex::new(Streams {
        streams: Vec::new(),
        predict_warned: false,
        inject_logged: 0,
    })
});

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Load the stream files named by the env knobs (once; called lazily from
/// [`inject_stream_turns`], so config-file knobs are already applied).
fn load_locked(s: &mut Streams) {
    for (slot, key) in [
        (0u32, "CM_ROLLBACK_TURN_STREAM"),
        (1u32, "CM_ROLLBACK_TURN_STREAM_1"),
    ] {
        let Ok(path) = std::env::var(key) else {
            continue;
        };
        if s.streams.iter().any(|f| f.slot == slot) {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut turns: Vec<Vec<u8>> = Vec::new();
                for line in text.lines() {
                    let line = line.split('#').next().unwrap_or("").trim();
                    let turn = match decode_hex(line) {
                        Some(b) if !b.is_empty() => b,
                        _ => vec![SYNC_NOP],
                    };
                    turns.push(turn);
                }
                log(&format!(
                    "[cosmo-rollback] farm: stream slot {slot} '{path}': {} turns",
                    turns.len()
                ));
                s.streams.push(StreamFile {
                    slot,
                    path,
                    turns,
                });
            }
            Err(e) => {
                log(&format!(
                    "[cosmo-rollback] farm: stream '{path}' unreadable: {e}"
                ));
            }
        }
    }
}

/// Inject this step's stream turn for every armed slot. `predict` is the live
/// client's prediction limit: streams are only sound at 0 (lockstep), so a
/// non-zero limit with streams armed logs one warning.
pub fn inject_stream_turns(step: u32, predict: u32) {
    let mut s = match STREAMS.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    load_locked(&mut s);
    if s.streams.is_empty() {
        return;
    }
    if predict != 0 && !s.predict_warned {
        s.predict_warned = true;
        log(&format!(
            "[cosmo-rollback] farm: streams armed with CM_ROLLBACK_PREDICT={predict}; \
             a rollback re-sim does not replay injected turns — run with predict 0"
        ));
    }
    let count = s.streams.len();
    for i in 0..count {
        let (slot, turn) = {
            let sf = &s.streams[i];
            (
                sf.slot,
                sf.turns[(step as usize) % sf.turns.len()].clone(),
            )
        };
        crate::turn_hook::inject_turn(slot, &turn, false);
        let n = s.inject_logged;
        s.inject_logged += 1;
        if n < 6 || n % 4096 == 0 {
            log(&format!(
                "[cosmo-rollback] farm: step {step} slot {slot} turn {turn:02x?} ({} bytes)",
                turn.len()
            ));
        }
    }
}

/// Whether any stream file is armed (for farm scripts to check cheaply).
pub fn streams_armed() -> bool {
    let mut s = match STREAMS.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    load_locked(&mut s);
    !s.streams.is_empty()
}
