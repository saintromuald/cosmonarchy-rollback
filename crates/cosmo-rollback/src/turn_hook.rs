//! Engine-facing turn capture and injection for the 1.16.1 turn path.
//!
//! This is the "engine half" of `crate::turns`/`crate::session` (which are pure
//! bookkeeping over opaque turn bytes). It reads the local turn out of the engine and
//! injects turns back in. See `docs/analysis/turn-path.md` §3 (capture) and §4
//! (injection), and `docs/analysis/turn-capture-and-inject.md` for what was verified
//! empirically and what still needs a live/2-client test.
//!
//! # Capture
//!
//! The local player's commands accumulate in `g_turn_buffer` (`0x00654880`, appended by
//! `send_command` `0x00485BD0`) with length `g_command_queue_size` (`0x00654AA0`). The
//! buffer is flushed — sent via `SNetSendTurn` and its length reset to 0 — in two places:
//!
//! * `0x00485AA0` (the "turn-pipeline fill"), which the turn scheduler reaches through
//!   `step_network` (`0x00487070`) once per turn boundary. This is the **normal**
//!   per-turn flush, and it is *inlined* here (`push eax; push 0x654880; call 0x410202`
//!   at `0x00485B25`, reset at `0x00485B8D`), not a call out.
//! * `0x00485A40` (the standalone `send_turn`), called **only** from `send_command`
//!   (`0x00485C3D`) when the buffer is about to overflow `g_provider_max_buffer` — a
//!   mid-frame overflow flush, not the per-turn one.
//!
//! `docs/analysis/turn-path.md` §3 names `0x00485A40` as the capture point; that captures
//! only the overflow flush. The correct per-turn capture point is `0x00485AA0` on entry
//! (the buffer still holds the local turn there, before the inlined send resets it).
//! `0x00485AA0` has five call sites, but all are network-pipeline sites and the capture
//! is a pure read of `.data` globals, so hooking it is safe; the one that matters for a
//! rollback game is the turn boundary via `step_network`.
//!
//! # Injection
//!
//! `ProcessCommands` (`0x004865D0`, a.k.a. `game_recv`) is the injection primitive. Its
//! calling convention is **`eax = command bytes`, `[esp+4] = size`, `[esp+8] = replay
//! flag`**, stdcall (`ret 8`). Grounded in three independent places:
//!
//! * the callee disassembly: `mov esi,eax` (buffer), `mov eax,[ebp+8]` (size),
//!   `mov edx,[ebp+0xc]` (flag), ending in `ret 0x8`;
//! * gptp's `game_recv_wrapper` (`reference/cosmonarchy-gptp/hooks/game_hooks_inject.cpp:124`),
//!   which unpacks exactly `eax → cur_len`, `[ebp+8] → data_len`, `[ebp+0xc] → as_replay`;
//! * `samase_shim/src/bw.rs:122`: `ProcessCommands(@eax *const c_void, u32, u32)`.
//!
//! To inject one player's turn, set the three player-context globals and call it (this is
//! exactly what gptp's `replay_loop` does, `game_hooks.cpp:5153-5169`):
//!
//! ```text
//! *g_active_nation_id (0x00512678) = player_id
//!  g_active_player_id (0x0051267C) = player_id
//! *g_sequence         (0x006556E8) = 0
//! ProcessCommands(bytes, size, replay_flag)
//! ```
//!
//! The `replay_flag` is `false` for a live game (`g_is_in_replay` `0x006D0F14` is 0, so
//! the command is processed in full regardless); in a *replay* the same flag must be
//! `true` or the "replay watcher" filter drops everything but a handful of multiplayer
//! commands. The no-op / sync command is `GameCommand_Sequence` (`0x05`), which is what
//! the engine writes to an empty turn buffer, and whose processing increments
//! `g_sequence`.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::log::log;

// The injection primitive (see module docs for the convention). `stdcall` because the
// function does `ret 8` (callee cleans its two stack arguments).
whack_funcs!(stdcall, init_process_commands, 0x00400000,
    0x004865D0 => ProcessCommands(@eax *const u8, u32, u32);
);

// The turn-pipeline fill: the per-turn flush point (capture hook). No register or stack
// arguments; returns eax (callers ignore it).
whack_hooks!(stdcall, 0x00400000,
    0x00485AA0 => FillPipeline() -> u32;
);

/// `g_turn_buffer` — the outgoing command blob (bwdata.h:232).
const G_TURN_BUFFER: usize = 0x0065_4880;
/// `g_command_queue_size` — its length (bwdata.h:231).
const G_COMMAND_QUEUE_SIZE: usize = 0x0065_4AA0;
/// `g_active_nation_id` / `command_user` (bwdata.h:223).
const G_ACTIVE_NATION_ID: usize = 0x0051_2678;
/// `g_active_player_id` / `unique_command_user` (bwdata.h:224).
const G_ACTIVE_PLAYER_ID: usize = 0x0051_267C;
/// `g_sequence` — the per-turn command sequence counter (bwdata.h:1273).
const G_SEQUENCE: usize = 0x0065_56E8;
/// `g_sync_turn_enabled` (bwdata.h:1433) — the switch that gates the turn/replay
/// injector. Zeroing it makes the injector early-return, leaving the simulation's
/// only input to whatever calls [`inject_turn`]. See `rollback/drive.rs`.
const G_SYNC_TURN_ENABLED: usize = 0x0065_56E0;
/// `Game.frame_count` (bwdata.h:218).
const FRAME_COUNT: usize = 0x0057_F23C;

/// Captured local turns, one per flush, as `(frame_count, turn bytes)`.
static CAPTURED: Lazy<Mutex<VecDeque<(u32, Vec<u8>)>>> =
    Lazy::new(|| Mutex::new(VecDeque::new()));

/// Bound the capture log: the first few turns and then a sparse sample.
static CAPTURE_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Bound the injection log the same way.
static INJECT_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Install the turn-path hooks. `exe` is the `ModulePatcher` for `0x00400000` created in
/// [`crate::frame_hook::install`]; the `ProcessCommands` wrapper is always registered
/// (it is just a callable, it patches nothing), and the `FillPipeline` capture hook is
/// installed only when `CM_ROLLBACK_CAPTURE_TURN` is set.
pub fn install(exe: &mut whack::ModulePatcher) {
    unsafe { init_process_commands(exe) };
    log("[cosmo-rollback] ProcessCommands (0x004865D0) callable registered");

    if capture_enabled() {
        unsafe {
            exe.hook_closure(
                FillPipeline,
                move |orig: unsafe extern "C" fn() -> u32| -> u32 {
                    capture_local_turn();
                    // Live mode: the real commands must not leave over lockstep — they travel
                    // on the rollback channel and are injected by the client. Zero the buffer
                    // size so the flush sends the engine's empty-turn marker instead; the
                    // pipeline, the game-start loop and the acknowledgement plumbing all keep
                    // working (see `rollback/live.rs`).
                    // Only in a match, and only while the client is *driving*: zeroing the
                    // buffer in the lobby would also discard the lobby's own turn payloads
                    // (the engine sends the map/lobby state over the same turns), and the
                    // joiner would never receive the map. During the arming pending phase
                    // and after a handoff the engine's own turns flow untouched.
                    if crate::rollback::live::enabled()
                        && crate::rollback::live::in_game()
                        && crate::rollback::live::driving()
                    {
                        *(G_COMMAND_QUEUE_SIZE as *mut u32) = 0;
                    }
                    orig()
                },
            );
        }
        log("[cosmo-rollback] installed FillPipeline (turn capture) trampoline at 0x00485AA0");
    }
}

/// Whether the local turn should be captured: `CM_ROLLBACK_CAPTURE_TURN`, or live mode
/// (which needs it — see `rollback/live.rs`).
pub fn capture_enabled() -> bool {
    std::env::var("CM_ROLLBACK_CAPTURE_TURN").is_ok() || crate::rollback::live::enabled()
}

/// The local turn as it stands **now**: `g_turn_buffer` of `g_command_queue_size` bytes, or
/// the engine's empty-turn marker (`GameCommand_Sequence`, `0x05`) when nothing is buffered —
/// which is exactly what the flush would send (`0x00485B13`).
///
/// Read it at the **start** of a tick, before the frame's flush clears the buffer: at that
/// moment it holds the commands the player issued during the previous frame, which are the
/// turn for the step this tick runs (the flush happens at the start of the frame, before
/// `StepObjects`, so the engine applies them to the same frame — see `docs/analysis/turn-path.md`).
pub fn local_turn() -> Vec<u8> {
    let (_, mut bytes) = read_local_turn();
    if bytes.is_empty() {
        bytes.push(crate::turns::SYNC_NOP);
    }
    bytes
}

/// Suppress the engine's own lockstep send of the local turn: NOP the 5-byte
/// `call SNetSendTurn` inside the turn-pipeline fill (`0x00485B25`), so the turn never enters
/// CrownLink's lockstep stream. The flush still resets the buffer (`0x00485B8D`), which the
/// rollback client wants. Live mode sends the turn over its own channel instead.
///
/// Returns whether the expected bytes were found (and logs either way), so a mismatched build
/// is reported rather than silently doing nothing.
pub fn suppress_lockstep_send(exe: &mut whack::ModulePatcher) -> bool {
    // 0x00485B25: `call 0x00410202` (SNetSendTurn), immediately after `push 0x654880`.
    const SEND_CALL: usize = 0x0048_5B25;
    const EXPECTED: [u8; 5] = [0xE8, 0xD8, 0xA6, 0xF8, 0xFF];
    let actual = unsafe { std::slice::from_raw_parts(SEND_CALL as *const u8, EXPECTED.len()) };
    if actual != EXPECTED {
        log(&format!(
            "[cosmo-rollback] lockstep send NOT suppressed: 0x{SEND_CALL:08x} holds {actual:02x?}, expected {EXPECTED:02x?}"
        ));
        return false;
    }
    unsafe { exe.nop(SEND_CALL, EXPECTED.len()) };
    let after = unsafe { std::slice::from_raw_parts(SEND_CALL as *const u8, EXPECTED.len()) };
    log(&format!(
        "[cosmo-rollback] lockstep send suppressed: NOP at 0x{SEND_CALL:08x} (now {after:02x?})"
    ));
    true
}

/// Whether `CM_ROLLBACK_INJECT_TURN` asks to run the injection smoke test.
pub fn inject_enabled() -> bool {
    std::env::var("CM_ROLLBACK_INJECT_TURN").is_ok()
}

/// The bytes to inject for the smoke test, from `CM_ROLLBACK_INJECT_HEX` (default the
/// no-op `[0x05]`). A value that fails to parse is logged and yields the default.
pub fn inject_blob() -> Vec<u8> {
    match std::env::var("CM_ROLLBACK_INJECT_HEX") {
        Ok(s) => match decode_hex(&s) {
            Some(v) => v,
            None => {
                log(&format!(
                    "[cosmo-rollback] CM_ROLLBACK_INJECT_HEX={s:?} is not valid hex; using no-op 0x05"
                ));
                vec![0x05]
            }
        },
        Err(_) => vec![0x05],
    }
}

/// The replay flag for [`inject_turn`], from `CM_ROLLBACK_INJECT_FLAG` (default false).
pub fn inject_flag() -> bool {
    std::env::var("CM_ROLLBACK_INJECT_FLAG").is_ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Reads the local turn (buffer + size) at the current moment, without clearing it.
fn read_local_turn() -> (u32, Vec<u8>) {
    unsafe {
        let size = *(G_COMMAND_QUEUE_SIZE as *const u32);
        let frame = *(FRAME_COUNT as *const u32);
        let bytes = if size == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(G_TURN_BUFFER as *const u8, size as usize).to_vec()
        };
        (frame, bytes)
    }
}

/// Capture the local turn into [`CAPTURED`], called at the `FillPipeline` entry.
///
/// The live client reads the buffer directly (`local_turn`), so its captures are not
/// enqueued — the queue would grow without a consumer for the whole session.
fn capture_local_turn() {
    if crate::rollback::live::enabled() {
        return;
    }
    let (frame, bytes) = read_local_turn();
    if let Ok(mut q) = CAPTURED.lock() {
        q.push_back((frame, bytes.clone()));
    }
    let n = CAPTURE_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 8 || n % 256 == 0 {
        log(&format!(
            "[cosmo-rollback] capture #{n}: frame {frame} turn_len {} bytes {:02x?}",
            bytes.len(),
            bytes
        ));
    }
}

/// Takes the oldest captured turn, if any, in flush order.
///
/// This is the feed point for the engine-independent half: hand each `(frame, bytes)`
/// to `crate::session::Session::push_local` (for the local slot) so the input table has
/// the local turns a re-simulation re-injects. Not called here — `session.rs` is owned by
/// the main agent and wired separately.
pub fn take_captured() -> Option<(u32, Vec<u8>)> {
    CAPTURED.lock().ok()?.pop_front()
}

/// How many captured turns are still queued.
pub fn captured_len() -> usize {
    CAPTURED.lock().map(|q| q.len()).unwrap_or(0)
}

/// Zero `g_sync_turn_enabled` (`0x006556E0`) so the injector early-returns.
///
/// Grounding (both confirmed in the shipped exe and gptp source):
/// - the stock turn injector `0x00487100`, called from the frame function at
///   `0x004D96C4`, starts `mov eax,[0x6556e0]; xor ebx,ebx; cmp eax,ebx; jz ret`,
///   i.e. it returns immediately when `g_sync_turn_enabled == 0`;
/// - gptp `call_patch(replay_loop, 0x004D96C4)` (`game_hooks_inject.cpp:344`) replaces
///   that call, and `replay_loop` (`game_hooks.cpp:5139`) begins `if
///   (!g_sync_turn_enabled) return;`.
///
/// After this the replay supplies no commands, so a rollback game must inject every
/// turn itself via [`inject_turn`]. Logs the value it overwrote. Idempotent: only the
/// first caller actually writes (and logs); later callers are no-ops.
pub fn suppress_replay_injector() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    unsafe {
        let before = *(G_SYNC_TURN_ENABLED as *const u32);
        *(G_SYNC_TURN_ENABLED as *mut u32) = 0;
        log(&format!(
            "[cosmo-rollback] suppress replay injector: g_sync_turn_enabled 0x{before:x} -> 0"
        ));
    }
}

/// Whether `CM_ROLLBACK_SUPPRESS_SYNC` asks to zero `g_sync_turn_enabled` at the first
/// frame *without* the rest of drive mode — a diagnostic to separate "the flag is zeroed"
/// from "the input table drives the simulation" (see `rollback/drive.rs`).
pub fn suppress_sync_enabled() -> bool {
    std::env::var("CM_ROLLBACK_SUPPRESS_SYNC").is_ok()
}

/// Which method drive mode uses to suppress the replay command injector, from
/// `CM_ROLLBACK_DRIVE_SUPPRESS`. `"flag"` zeros `g_sync_turn_enabled` (see
/// [`suppress_replay_injector`]); anything else (default) NOPs the `call replay_loop` gptp
/// patches onto `0x004D96C4`, which `frame_hook::install` performs (writing `.text` needs
/// the whack patcher's `VirtualProtect`).
pub fn drive_suppress_method() -> &'static str {
    static METHOD: once_cell::sync::Lazy<&'static str> = once_cell::sync::Lazy::new(|| {
        let m = std::env::var("CM_ROLLBACK_DRIVE_SUPPRESS").unwrap_or_else(|_| "nop".into());
        Box::leak(m.into_boxed_str())
    });
    *METHOD
}

/// Whether the replay-loop NOP (the default suppress method) should be installed: drive
/// mode is on and the method is not the flag zeroing.
pub fn drive_suppress_nop() -> bool {
    // Live mode suppresses the same call (`0x004D96C4`): in a match it is the engine's turn
    // injector, and the live client injects every turn itself.
    (std::env::var("CM_ROLLBACK_DRIVE").is_ok() || crate::rollback::live::enabled())
        && drive_suppress_method() != "flag"
}

/// The current `g_sequence` (`0x006556E8`), the per-turn command sequence counter.
pub fn read_g_sequence() -> u32 {
    unsafe { *(G_SEQUENCE as *const u32) }
}

/// Inject one player's turn by setting the player-context globals and calling
/// `ProcessCommands` (see the module docs for the convention).
///
/// The player-context globals are restored afterwards so the surrounding engine sees the
/// local player again; `g_sequence` is deliberately *not* restored — its post-injection
/// value is part of the simulation (and it is snapshotted, so a rollback rewinds it).
/// Walks a turn's command stream using the engine's `command_lengths` table
/// (`0x005005F8`, `[u32; 0x60]`) and returns `(commands, complete)`.
///
/// The stream is `id` followed by `command_lengths[id]` payload bytes. A stream that does
/// not land exactly on its end is malformed, and handing it to `ProcessCommands` is how the
/// engine crashes (an access violation on a bogus opcode).
pub fn walk_turn(bytes: &[u8]) -> (Vec<(u8, u32)>, bool) {
    const LENGTHS: usize = 0x0050_05F8;
    const COUNT: u8 = 0x60;
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < bytes.len() {
        let id = bytes[p];
        if id >= COUNT {
            return (out, false);
        }
        let len = unsafe { *(LENGTHS as *const u32).add(id as usize) };
        // The table has no entry for the special sync commands (e.g.
        // `GameCommand_Hash` 0x37 reads 0xFFFFFFFF here — the engine handles it by a
        // dedicated path, not the generic walker), and a length that would run past
        // the end is a malformed turn. Both are "incomplete", never an overflow.
        if len as usize > bytes.len() - p - 1 {
            return (out, false);
        }
        out.push((id, len));
        p += 1 + len as usize;
    }
    (out, p == bytes.len())
}

pub fn inject_turn(player_id: u32, bytes: &[u8], replay_flag: bool) {
    // Log the payload *before* the call: the engine's crash report cannot symbolise a fault
    // through a bad pointer, so the last line printed is what localises it.
    let n_log = INJECT_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n_log < 16 || n_log % 256 == 0 {
        let (cmds, complete) = walk_turn(bytes);
        log(&format!(
            "[cosmo-rollback] inject #{n_log} (pre): player {player_id} len {} complete {complete} cmds {cmds:02x?} bytes {:02x?}",
            bytes.len(),
            bytes
        ));
    }
    unsafe {
        let saved_nation = *(G_ACTIVE_NATION_ID as *const u32);
        let saved_player = *(G_ACTIVE_PLAYER_ID as *const u32);
        *(G_ACTIVE_NATION_ID as *mut u32) = player_id;
        *(G_ACTIVE_PLAYER_ID as *mut u32) = player_id;
        *(G_SEQUENCE as *mut u32) = 0;
        let seq_before = *(G_SEQUENCE as *const u32);
        (*ProcessCommands)(bytes.as_ptr(), bytes.len() as u32, replay_flag as u32);
        let seq_after = *(G_SEQUENCE as *const u32);
        *(G_ACTIVE_NATION_ID as *mut u32) = saved_nation;
        *(G_ACTIVE_PLAYER_ID as *mut u32) = saved_player;
        if n_log < 16 || n_log % 256 == 0 {
            log(&format!(
                "[cosmo-rollback] inject #{n_log} (post): flag {} g_sequence {seq_before} -> {seq_after}",
                replay_flag as u8
            ));
        }
    }
}
