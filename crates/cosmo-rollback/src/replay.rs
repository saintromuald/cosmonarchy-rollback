//! Replay input cursor (shadow-gate harness state).
//!
//! The per-frame replay command injector (`0x00487100`) runs **inside** the hooked
//! frame function, so re-simulation re-runs it. It decides what to inject from
//! `next_replay_command_frame` (0x005136C8) against `Game.frame_count`, and reads the
//! next command block out of the replay stream at `ReplayData::cur_frame`/`cur_command`
//! (`struct ReplayData`, `BW/structures.h:251`, reached through the pointer global at
//! 0x00596BBC).
//!
//! `frame_count` is already in the snapshot but these cursors were not: after a restore
//! they still sat at the *later* frame's position, so the re-simulated frame computed
//! "the next command is not due yet" and injected nothing, while the original frame had
//! injected its commands. Missed command injection is what moved the pools' free lists
//! and the free-tail bytes in the shadow gate.
//!
//! Only the cursor is needed. The replay *data* itself (`ReplayData::start`, the loaded
//! replay buffer) is immutable for the session and `cur_frame` points into it, so
//! restoring the pointer restores the position without copying the stream. The decoded
//! staging buffers (`g_replay_cmd_buffer` 0x6552B0 and friends) are rewritten from the
//! stream on each read, so they follow from the cursor.
//!
//! This is replay-harness input. A live rollback game takes its input from the turn /
//! input table, so this module is what makes the shadow gate valid when it runs on a
//! replay, and is inert in a live game.

#![allow(dead_code)]

/// The global holding the `ReplayData*` (`BW/bwdata.h:298`).
const REPLAY_DATA_PTR: usize = 0x0059_6BBC;
/// `sizeof(ReplayData)`: eight `u32`/pointer words (`BW/structures.h:251`).
const REPLAY_DATA_SIZE: usize = 0x20;

pub fn capture() -> Vec<u8> {
    if std::env::var("CM_ROLLBACK_NO_REPLAY").is_ok() {
        return Vec::new();
    }
    unsafe {
        let p = *(REPLAY_DATA_PTR as *const u32) as *const u8;
        if p.is_null() {
            return Vec::new();
        }
        std::slice::from_raw_parts(p, REPLAY_DATA_SIZE).to_vec()
    }
}

pub fn restore(blob: &[u8]) {
    if blob.len() < REPLAY_DATA_SIZE {
        return;
    }
    unsafe {
        let p = *(REPLAY_DATA_PTR as *const u32) as *mut u8;
        if p.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(blob.as_ptr(), p, REPLAY_DATA_SIZE);
    }
}
