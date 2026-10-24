//! Bug-capture hotkey: press Ctrl+F11 in-game to save a full diagnostic bundle —
//! engine snapshot (all 116k ranges), plugin state (gptp serializer blobs), and a
//! manifest with the sim context — into `~/Games/cosmonarchy/bug-reports/`.
//!
//! Two triggers:
//!
//! 1. **Hotkey** (default Ctrl+F11, `CM_ROLLBACK_BUG_VK` overrides the key VK in hex):
//!    polled from the frame hook, so the capture sees a consistent between-frame state.
//! 2. **Freeze watchdog**: a thread watches `Game.frame_count`; if the game stops
//!    advancing for 3 s while not paused, it requests a capture too. If the frame hook
//!    cannot service the request (the game loop itself is stuck — "the game won't let
//!    me do anything"), the thread performs the capture itself after 2.5 s, so a frozen
//!    game still produces a bundle.
//!
//! `cosmo_rollback_last_capture()` exports the captured frame so the Plus plugin can
//! draw an on-screen confirmation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

const VK_CONTROL: i32 = 0x11;
const VK_F11: i32 = 0x7A;
/// `Game.frame_count` (`frame_hook.rs`).
const FRAME_COUNT: usize = 0x0057_F23C;
/// `is_paused` (`ranges.rs`).
const IS_PAUSED: usize = 0x0065_09C4;
/// `rng_seed` / `rng_calls` (`ranges.rs`).
const RNG_SEED: usize = 0x0051_CA14;
const RNG_CALLS: usize = 0x0051_CA18;
/// `first_free_path` / `g_path_array_start` (`pools.rs`).
const FIRST_FREE_PATH: usize = 0x006B_EE84;
const PATH_ARRAY_START: usize = 0x006B_EE8C;
/// `unit_count` (`ranges.rs`).
const UNIT_COUNT: usize = 0x0062_83F0;

/// 0 = no request, 1 = hotkey, 2 = freeze. Set by the watchdog, serviced by the frame hook.
static REQUEST: AtomicU32 = AtomicU32::new(0);
/// When the request was made (`GetTickCount` ms), for the frozen-loop fallback.
static REQUEST_AT: AtomicU32 = AtomicU32::new(0);
/// The last `frame_count` seen by the watchdog, and when it last advanced.
static WATCH_FRAME: AtomicU32 = AtomicU32::new(0);
static WATCH_AT: AtomicU32 = AtomicU32::new(0);
/// The frame of the most recent capture (exported for the on-screen confirmation).
static LAST_CAPTURE: AtomicU32 = AtomicU32::new(0);
/// The frame value at the last auto-capture, so a permanently frozen game is captured
/// once per freeze episode rather than every watchdog cycle.
static FROZEN_FRAME: AtomicU32 = AtomicU32::new(0);

fn tick_ms() -> u32 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() }
}

fn read_u32(addr: usize) -> u32 {
    unsafe { std::ptr::read_unaligned(addr as *const u32) }
}

fn read_u8(addr: usize) -> u8 {
    unsafe { std::ptr::read_unaligned(addr as *const u8) }
}

/// Where bug bundles are written: `CM_ROLLBACK_BUG_DIR`, else
/// `<USERPROFILE>/Games/cosmonarchy/bug-reports`.
fn bug_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CM_ROLLBACK_BUG_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "Z:\\".to_string());
    PathBuf::from(profile)
        .join("Games")
        .join("cosmonarchy")
        .join("bug-reports")
}

/// Last `n` lines of `path` relative to the process cwd (the game dir), joined.
fn log_tail(path: &str, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("(cannot read {path})");
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The capture itself. Safe to call from the frame hook (consistent state) or from the
/// watchdog thread (racy bytes are fine for diagnosis). Never panics: a diagnostic path
/// must not take the game down with it.
fn capture(reason: &str) {
    let result = std::panic::catch_unwind(|| capture_inner(reason));
    crate::log::log(&format!(
        "[cosmo-rollback] BUG CAPTURE ({reason}): {}",
        match result {
            Ok(Some(path)) => format!("saved {path}"),
            Ok(None) => "nothing to capture (no game running?)".to_string(),
            Err(_) => "capture panicked (see log)".to_string(),
        }
    ));
}

fn capture_inner(reason: &str) -> Option<String> {
    let frame = read_u32(FRAME_COUNT);
    if frame == 0 {
        return None;
    }

    let dir = bug_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        crate::log::log(&format!("[cosmo-rollback] bug capture: mkdir failed: {e}"));
        return None;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("bug-{stamp}-f{frame}");
    let cosmo = dir.join(format!("{name}.cosmo"));
    let manifest = dir.join(format!("{name}.txt"));

    // The snapshot's own serialization already includes the plugin-state blobs, so the
    // save file is complete on its own (session section empty).
    let snapshot = unsafe { crate::snapshot::Snapshot::capture(crate::ranges::engine_ranges()) };
    let snap_bytes = snapshot.serialize();

    let path = cosmo.as_os_str().to_string_lossy().replace('\\', "/");
    if let Err(e) = crate::savefile::write_save(
        &cosmo,
        frame,
        crate::savefile::FLAG_HARNESS,
        &snap_bytes,
        &[],
    ) {
        crate::log::log(&format!("[cosmo-rollback] bug capture: write failed: {e}"));
        return None;
    }

    let first_free = read_u32(FIRST_FREE_PATH);
    let path_base = read_u32(PATH_ARRAY_START);
    let text = format!(
        "cosmonarchy bug capture\n\
         reason:     {reason}\n\
         timestamp:  {stamp} (unix)\n\
         tick:       {}\n\
         frame:      {frame}\n\
         paused:     {}\n\
         rng_seed:   {}\n\
         rng_calls:  {}\n\
         unit_count: {}\n\
         first_free_path: 0x{first_free:08x}\n\
         path_array_base: 0x{path_base:08x}\n\
         rollback_abi: {}\n\
         snapshot:   {} ({snap_len} bytes)\n\
         --- cosmo_plus.log (tail) ---\n\
         {plus_tail}\n\
         --- cosmo_rollback.log (tail) ---\n\
         {rollback_tail}\n",
        tick_ms(),
        read_u8(IS_PAUSED),
        read_u32(RNG_SEED),
        read_u32(RNG_CALLS),
        read_u32(UNIT_COUNT),
        crate::cosmo_rollback_abi(),
        &name,
        snap_len = snap_bytes.len(),
        plus_tail = log_tail("cosmo_plus.log", 30),
        rollback_tail = log_tail("cosmo_rollback.log", 30),
    );
    if let Err(e) = std::fs::write(&manifest, &text) {
        crate::log::log(&format!(
            "[cosmo-rollback] bug capture: manifest write failed: {e}"
        ));
    }

    LAST_CAPTURE.store(frame, Ordering::SeqCst);
    if reason.contains("freeze") || reason.contains("fallback") {
        FROZEN_FRAME.store(frame, Ordering::SeqCst);
    }
    Some(path)
}

/// Frame-hook service point: perform the capture in a consistent state when requested.
pub(crate) fn maybe_capture() {
    let req = REQUEST.swap(0, Ordering::SeqCst);
    match req {
        1 => capture("hotkey"),
        2 => capture("freeze"),
        _ => {}
    }
}

/// The most recent capture's frame (0 = none). Exported; the Plus plugin polls it to
/// draw an on-screen confirmation.
#[no_mangle]
pub extern "system" fn cosmo_rollback_last_capture() -> u32 {
    LAST_CAPTURE.load(Ordering::SeqCst)
}

/// Watchdog thread: hotkey polling + freeze detection + frozen-loop fallback.
pub(crate) fn install() {
    let bug_vk = std::env::var("CM_ROLLBACK_BUG_VK")
        .ok()
        .and_then(|v| {
            let v = v.trim();
            let hex = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X"));
            match hex {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => v.parse().ok(),
            }
        })
        .map(|v| v as i32)
        .unwrap_or(VK_F11);
    crate::log::log(&format!(
        "[cosmo-rollback] bug capture: Ctrl+VK 0x{bug_vk:02X} saves a diagnostic bundle; \
         a frozen game is captured automatically"
    ));

    std::thread::Builder::new()
        .name("cosmo-bug-watch".to_string())
        .spawn(move || watch_loop(bug_vk))
        .expect("spawn bug-capture watchdog");
}

fn watch_loop(bug_vk: i32) {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(40));
        let now = tick_ms();

        // Hotkey: Ctrl held + fresh press of the bug key.
        let ctrl_down = (unsafe { GetAsyncKeyState(VK_CONTROL) } as u16 & 0x8000) != 0;
        let key_state = unsafe { GetAsyncKeyState(bug_vk) } as u16;
        let fresh_press = key_state & 0x0001 != 0;
        if ctrl_down && fresh_press {
            REQUEST.store(1, Ordering::SeqCst);
            REQUEST_AT.store(now, Ordering::SeqCst);
        }

        // Freeze detection: the sim stopped advancing while unpaused, in a game, and
        // this frame value has not already been captured.
        let frame = read_u32(FRAME_COUNT);
        let last = WATCH_FRAME.load(Ordering::SeqCst);
        let last_at = WATCH_AT.load(Ordering::SeqCst);
        if frame != last {
            WATCH_FRAME.store(frame, Ordering::SeqCst);
            WATCH_AT.store(now, Ordering::SeqCst);
        } else if frame != 0
            && read_u8(IS_PAUSED) == 0
            && now.wrapping_sub(last_at) >= 3000
            && REQUEST.load(Ordering::SeqCst) == 0
            && FROZEN_FRAME.load(Ordering::SeqCst) != frame
        {
            crate::log::log(&format!(
                "[cosmo-rollback] bug capture: frame {frame} has not advanced for >=3s; requesting capture"
            ));
            REQUEST.store(2, Ordering::SeqCst);
            REQUEST_AT.store(now, Ordering::SeqCst);
        }

        // Fallback: the frame hook could not service the request (game loop stuck) —
        // capture from here after a grace period, at most once per request.
        let req = REQUEST.load(Ordering::SeqCst);
        if req != 0 && now.wrapping_sub(REQUEST_AT.load(Ordering::SeqCst)) >= 2500 {
            if WATCH_FRAME.load(Ordering::SeqCst) == frame && frame == read_u32(FRAME_COUNT) {
                REQUEST.store(0, Ordering::SeqCst);
                let reason = if req == 2 { "freeze-fallback" } else { "hotkey-fallback" };
                capture(reason);
            }
        }
    }
}
