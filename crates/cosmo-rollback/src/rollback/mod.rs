//! Rollback core: a snapshot ring plus restore + re-simulation.
//!
//! This is the mechanism ShieldBattery's `rollback/{tick,snapshot}.rs` provide,
//! reduced to what the kill gate needs. Each game-logic step:
//!   1. if due, capture a snapshot into the ring;
//!   2. in shadow mode, restore an older snapshot, re-run the engine forward to
//!      the present, and compare the state hash to the state produced normally —
//!      any difference is state a re-simulation did not reproduce (the desync bug
//!      class the whole project hinges on);
//!   3. run the step for real.
//!
//! Env knobs:
//!   - `CM_ROLLBACK_SNAPSHOT_SPACING=<n>` (default 3)
//!   - `CM_ROLLBACK_SHADOW=<depth>` — enable shadow re-simulation at this depth

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use crate::log::log;
use crate::ranges::{self, RangeList};
use crate::session::Session;
use crate::snapshot::Snapshot;
use crate::state_hash::{state_hash, state_hash_ignoring};
use crate::turns::SLOTS;

mod drive;
pub mod live;

struct Entry {
    frame: u32,
    snap: Snapshot,
}

pub struct Engine {
    ranges: &'static RangeList,
    ring: VecDeque<Entry>,
    spacing: u32,
    frame: u32,
    shadow: Option<u32>,
    shadow_start: u32,
    /// Ring entries kept. Each entry is now every object pool (~3 MB) plus the
    /// plugin blob when enabled, so the history is sized to the deepest re-sim the
    /// shadow mode can ask for instead of a fixed count that would exhaust a 32-bit
    /// address space.
    keep: usize,
    /// The rollback session, present only when `CM_ROLLBACK_CAPTURE_TURN` is set.
    /// See [`Engine::feed_captured_turns`].
    session: Option<Session>,
    /// The state at the exit of the previous frame, for the `CM_ROLLBACK_BOUNDARY`
    /// diagnostic.
    last_exit: Option<Snapshot>,
    /// Drive mode (see `rollback/drive.rs`), present only when `CM_ROLLBACK_DRIVE` is
    /// set. When present, `on_step` delegates entirely to it.
    drive: Option<drive::Drive>,
    /// The live client (see `rollback/live.rs`), present only when `CM_ROLLBACK_LIVE` is
    /// set. When present, `on_step` delegates entirely to it.
    live: Option<live::Live>,
    /// Save/load round-trip gate (M1): `CM_ROLLBACK_SAVE_GATE=<depth>` re-simulates
    /// like the shadow gate, but the restored snapshot first travels through the save
    /// file format (serialize → file → read → deserialize).
    save_gate: Option<u32>,
    /// First frame the save gate runs at (`CM_ROLLBACK_SAVE_GATE_START`, default 90).
    save_gate_start: u32,
    /// One-shot save at this frame (`CM_ROLLBACK_SAVE_AT=<frame>`), harness/testing use.
    save_at: Option<u32>,
    /// Re-read and compare every save written (`CM_ROLLBACK_SAVE_VERIFY=1`).
    save_verify: bool,
    /// Directory saves are written to (`CM_ROLLBACK_SAVE_DIR`, default `cosmo_saves`).
    save_dir: String,
}

static ENGINE: Lazy<Mutex<Engine>> = Lazy::new(|| Mutex::new(Engine::new()));

impl Engine {
    fn new() -> Self {
        let spacing = std::env::var("CM_ROLLBACK_SNAPSHOT_SPACING")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(3);
        let shadow = std::env::var("CM_ROLLBACK_SHADOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0);
        let shadow_start = std::env::var("CM_ROLLBACK_SHADOW_START")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(90);
        // Enough history for the deepest re-sim this session can request, plus slack.
        let keep = ((shadow.unwrap_or(0) as usize / spacing as usize) + 12).max(16);
        // A capture-enabled run also gets a session, so the captured local turns are fed
        // to the engine-independent half (see `feed_captured_turns`). Single-player: no
        // remote slot, so this exercises capture → input table → dispatch, not rollback.
        let session = crate::turn_hook::capture_enabled().then(|| {
            Session::new(0, &[], 8, 0, spacing, [Duration::ZERO; SLOTS])
        });
        Engine {
            ranges: ranges::engine_ranges(),
            ring: VecDeque::new(),
            spacing,
            frame: 0,
            shadow,
            shadow_start,
            keep,
            session,
            last_exit: None,
            drive: drive::Drive::new(),
            live: live::enabled().then(live::Live::new),
            save_gate: std::env::var("CM_ROLLBACK_SAVE_GATE")
                .ok()
                .and_then(|s| {
                    if s.is_empty() {
                        Some(3)
                    } else {
                        s.parse().ok()
                    }
                })
                .filter(|&n| n > 0),
            save_gate_start: std::env::var("CM_ROLLBACK_SAVE_GATE_START")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(90),
            save_at: std::env::var("CM_ROLLBACK_SAVE_AT")
                .ok()
                .and_then(|s| s.parse().ok()),
            save_verify: std::env::var("CM_ROLLBACK_SAVE_VERIFY").is_ok(),
            save_dir: std::env::var("CM_ROLLBACK_SAVE_DIR")
                .unwrap_or_else(|_| "cosmo_saves".to_string()),
        }
    }

    /// Feed the local turns `turn_hook` captured this frame into the session's input
    /// table, then plan and dispatch the step, checking that the bytes dispatched for a
    /// frame are the bytes captured for it. This is the **capture → input table → dispatch**
    /// plumbing, driven by real engine turn bytes; it does **not** inject (the replay's own
    /// injector already applies these turns, so injecting them again would double-apply —
    /// injection is proven separately by `CM_ROLLBACK_INJECT_TURN`). The real per-step
    /// wiring (which slot is local, which are peers, and injecting on a re-sim) needs the
    /// transport/peer model.
    fn feed_captured_turns(&mut self) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        while let Some((frame, bytes)) = crate::turn_hook::take_captured() {
            s.push_local(bytes.clone(), Instant::now());
            let current = s.current();
            let plan = s.plan();
            let step = plan.start(current);
            let (turns, predicted) = s.dispatch(step);
            let dispatched = turns[0].as_deref().unwrap_or(&[]);
            let ok = dispatched == bytes.as_slice();
            if step < 12 || !ok {
                log(&format!(
                    "[cosmo-rollback] session: captured f{frame} len {} -> dispatched step {step} len {} predicted {predicted} {} (current {} confirmed {})",
                    bytes.len(),
                    dispatched.len(),
                    if ok { "match" } else { "MISMATCH" },
                    s.current(),
                    s.confirmed(),
                ));
            }
            s.settle(plan.present, 0);
        }
    }

    /// Direct test of whether one `orig()` call is reproducible from the same state.
    ///
    /// `CM_ROLLBACK_DET=<frame>`: at that frame, snapshot the state, run one step,
    /// capture, restore the snapshot, run one step again, capture, and compare. If the
    /// two captures differ, `orig` reads state the snapshot does not carry (wall clock,
    /// input stream, or an un-snapshotted global) and no range table can make the shadow
    /// gate pass — which is a different problem from an incomplete range list.
    ///
    /// The simulation is left at the restored frame, so the normal path continues
    /// unaffected.
    fn probe_determinism(&mut self, f: u32, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) {
        let Ok(want) = std::env::var("CM_ROLLBACK_DET") else {
            return;
        };
        if want.parse::<u32>() != Ok(f) {
            return;
        }
        unsafe {
            let s = Snapshot::capture(&self.ranges);
            orig(ecx);
            let mid1 = Snapshot::capture(&self.ranges);
            s.restore();
            orig(ecx);
            let mid2 = Snapshot::capture(&self.ranges);
            s.restore();
            if state_hash(&mid1) == state_hash(&mid2) {
                log(&format!(
                    "[cosmo-rollback] DETERMINISM f{f}: one step is reproducible"
                ));
                return;
            }
            log(&format!(
                "[cosmo-rollback] DETERMINISM f{f}: one step is NOT reproducible"
            ));
            diff_ranges(&self.ranges, &mid1, &mid2);
            for (name, a, b) in [
                ("image", mid1.aux_image(), mid2.aux_image()),
                ("tiles", mid1.aux_tiles(), mid2.aux_tiles()),
                ("creep", mid1.aux_creep(), mid2.aux_creep()),
                ("replay", mid1.aux_replay(), mid2.aux_replay()),
            ] {
                if a != b {
                    let n = a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();
                    let first = a
                        .iter()
                        .zip(b.iter())
                        .position(|(x, y)| x != y)
                        .unwrap_or(0);
                    log(&format!(
                        "[cosmo-rollback]   AUX {name}: {n} bytes differ (first +0x{first:x} {} -> {})",
                        a.get(first).copied().unwrap_or(0),
                        b.get(first).copied().unwrap_or(0)
                    ));
                }
            }

            // Name the memory that differs outside the snapshot: read the exe's whole
            // `.data` after each step and report the differing bytes the range list does
            // not cover. Those addresses are the state `orig` reads that a restore
            // cannot put back, which is what makes a single step irreproducible.
            let data = || std::slice::from_raw_parts(0x0050_C000 as *const u8, 0x1D_1694).to_vec();
            // Re-run both steps so the .data reads are at the same phase as the capture.
            let s2 = Snapshot::capture(&self.ranges);
            orig(ecx);
            let d1 = data();
            s2.restore();
            orig(ecx);
            let d2 = data();
            s2.restore();
            let mut outside = 0u32;
            let mut shown = 0u32;
            for i in 0..d1.len() {
                if d1[i] == d2[i] {
                    continue;
                }
                let addr = 0x0050_C000 + i;
                if self.covered(addr) {
                    continue;
                }
                outside += 1;
                if shown < 24 {
                    log(&format!(
                        "[cosmo-rollback]   UNCOVERED 0x{addr:08x} {} -> {}",
                        d1[i], d2[i]
                    ));
                    shown += 1;
                }
            }
            log(&format!(
                "[cosmo-rollback]   uncovered differing bytes in exe .data: {outside}"
            ));

            // Same scan over every loaded plugin's writable sections, at the same two
            // phases. gptp's ~19 MB `.data` holds the CCMU extender (already covered) plus
            // the plugin's own simulation state (`g_ext` is heap, but `g_trigger_vars`,
            // `g_ai_layouts` and the mode state are here), and none of it is snapshotted.
            // This is what names the plugin state a restore cannot put back.
            let sections = crate::modules::writable_sections(usize::MAX);
            let read_sections = || -> Vec<Vec<u8>> {
                sections
                    .iter()
                    .map(|r| std::slice::from_raw_parts(r.addr as *const u8, r.len).to_vec())
                    .collect()
            };
            let s3 = Snapshot::capture(&self.ranges);
            orig(ecx);
            let p1 = read_sections();
            s3.restore();
            orig(ecx);
            let p2 = read_sections();
            s3.restore();
            for ((r, a), b) in sections.iter().zip(p1.iter()).zip(p2.iter()) {
                let diffs: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
                if diffs.is_empty() {
                    continue;
                }
                let first = diffs[0];
                let last = *diffs.last().unwrap();
                let covered = self.covered(r.addr + first);
                let sample = diffs
                    .iter()
                    .take(6)
                    .map(|&i| format!("+0x{i:x}:{}->{}", a[i], b[i]))
                    .collect::<Vec<_>>()
                    .join(" ");
                let all = diffs
                    .iter()
                    .take(40)
                    .map(|&i| format!("+0x{i:x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                log(&format!(
                    "[cosmo-rollback]   PLUGIN {:<24} +0x{first:x}..+0x{last:x} (span 0x{:x}, 0x{:08x}) {} bytes{} :: {sample}",
                    r.name,
                    last - first + 1,
                    r.addr + first,
                    diffs.len(),
                    if covered { " (covered)" } else { " UNCOVERED" }
                ));
                log(&format!("[cosmo-rollback]     offsets: {all}"));
            }
        }
    }

    /// Whether any flat range covers `addr`.
    fn covered(&self, addr: usize) -> bool {
        self.ranges
            .ranges
            .iter()
            .any(|r| addr >= r.addr && addr < r.addr + r.len)
    }

    fn on_step(&mut self, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) -> u32 {
        // Drive mode (see `rollback/drive.rs`) replaces this whole path: suppress the
        // replay injector, drive turns from a 2-slot session, and re-inject on the re-sim.
        if let Some(d) = self.drive.as_mut() {
            return d.tick(ecx, orig);
        }
        // Live mode (see `rollback/live.rs`) owns the whole frame: it drives every slot's
        // turn through the input table and injects it.
        if let Some(l) = self.live.as_mut() {
            return l.tick(ecx, orig);
        }

        // Diagnostic (`CM_ROLLBACK_SUPPRESS_SYNC=1`): zero `g_sync_turn_enabled` at the
        // first frame while leaving the normal path intact, to see whether the game
        // still steps with the replay injector suppressed (see `rollback/drive.rs`).
        if crate::turn_hook::suppress_sync_enabled() {
            crate::turn_hook::suppress_replay_injector();
        }

        let f = self.frame;

        // Boundary diagnostic (`CM_ROLLBACK_BOUNDARY=1`): compare this frame's *entry* state
        // with the previous frame's *exit* state. They should be identical (nothing between
        // two calls to the hooked function advances the simulation); if they are not, the
        // outer main loop mutates state the re-simulation cannot reproduce, which is why the
        // shadow gate cannot pass. See `docs/analysis/restore-residual.md`.
        if std::env::var("CM_ROLLBACK_BOUNDARY").is_ok() {
            if let Some(prev) = self.last_exit.as_ref() {
                let now = unsafe { Snapshot::capture(&self.ranges) };
                if state_hash(prev) != state_hash(&now) {
                    log(&format!("[cosmo-rollback] BOUNDARY f{f}: entry != exit(f-1)"));
                    diff_ranges(&self.ranges, prev, &now);
                } else if f % 60 == 0 {
                    log(&format!("[cosmo-rollback] BOUNDARY f{f}: entry == exit(f-1)"));
                }
            }
        }

        if f % self.spacing == 0 {
            let snap = unsafe { Snapshot::capture(&self.ranges) };
            self.ring.push_back(Entry { frame: f, snap });
            // Keep enough history for the shadow depth plus slack.
            let keep = self.keep;
            while self.ring.len() > keep {
                self.ring.pop_front();
            }
        }

        if let Some(depth) = self.shadow {
            if f >= depth && f >= self.shadow_start {
                let target = f - depth;
                self.probe_determinism(f, ecx, orig);
                if let Some(pos) = self.ring.iter().rposition(|e| e.frame <= target) {
                    let g = self.ring[pos].frame;
                    let steps = f - g;

                    // `CM_ROLLBACK_DET_STEPWISE=<frame>`: step the re-sim one frame at a
                    // time and compare each intermediate state against the live snapshot
                    // the ring recorded for that same frame (needs
                    // `CM_ROLLBACK_SNAPSHOT_SPACING=1`). The first frame that differs is
                    // the frame whose step the re-sim got wrong.
                    if det_frame("CM_ROLLBACK_DET_STEPWISE") == Some(f) {
                        unsafe {
                            self.ring[pos].snap.restore();
                            let mut matched = true;
                            for i in 1..=steps as usize {
                                orig(ecx);
                                let c = Snapshot::capture(&self.ranges);
                                let want = g + i as u32;
                                match self.ring.iter().find(|e| e.frame == want) {
                                    Some(entry) => {
                                        if state_hash(&c) != state_hash(&entry.snap) {
                                            log(&format!(
                                                "[cosmo-rollback] STEPWISE f{f}: re-sim diverges from live at frame {want} (step {i})"
                                            ));
                                            diff_ranges(&self.ranges, &entry.snap, &c);
                                            matched = false;
                                            break;
                                        }
                                    }
                                    None => {
                                        log(&format!(
                                            "[cosmo-rollback] STEPWISE f{f}: ring has no frame {want} (need spacing=1)"
                                        ));
                                        matched = false;
                                        break;
                                    }
                                }
                            }
                            if matched {
                                log(&format!(
                                    "[cosmo-rollback] STEPWISE f{f}: every re-simulated frame matched live"
                                ));
                            }
                            self.ring[pos].snap.restore();
                        }
                    }

                    // `CM_ROLLBACK_DET_RESIM=<frame>`: run the same multi-step re-sim
                    // twice and compare. This separates "the re-sim itself is
                    // non-deterministic" from "the snapshot at g is not a faithful
                    // restore" — the one-step probe only tests the former locally.
                    if det_frame("CM_ROLLBACK_DET_RESIM") == Some(f) {
                        unsafe {
                            let run = || {
                                self.ring[pos].snap.restore();
                                for _ in 0..steps {
                                    orig(ecx);
                                }
                                Snapshot::capture(&self.ranges)
                            };
                            let c1 = run();
                            let c2 = run();
                            self.ring[pos].snap.restore();
                            if state_hash(&c1) == state_hash(&c2) {
                                log(&format!(
                                    "[cosmo-rollback] RESIM2 f{f} g{g} steps{steps}: re-sim is deterministic"
                                ));
                            } else {
                                log(&format!(
                                    "[cosmo-rollback] RESIM2 f{f} g{g} steps{steps}: re-sim is NOT deterministic"
                                ));
                                diff_ranges(&self.ranges, &c1, &c2);
                            }
                        }
                    }
                    let before = unsafe { Snapshot::capture(&self.ranges) };

                    // Whole-.data diff diagnostic (read-only): the differing bytes OUTSIDE our
                    // ranges name the missing state. Only at frame 90 to bound cost.
                    let data_snapshot = |f: u32| -> Vec<u8> {
                        if f != 90 {
                            return Vec::new();
                        }
                        unsafe {
                            std::slice::from_raw_parts(0x0050_C000 as *const u8, 0x1D1694).to_vec()
                        }
                    };
                    let d_before = data_snapshot(f);

                    // Option 2 diagnostic: capture -> restore -> capture must be identical
                    // (a no-op restore). A mismatch means capture/restore itself is lossy.
                    if f == 90 {
                        let s1 = unsafe { Snapshot::capture(&self.ranges) };
                        unsafe { s1.restore() };
                        let s2 = unsafe { Snapshot::capture(&self.ranges) };
                        log(&format!(
                            "[cosmo-rollback] SELF-RESTORE f90: {}",
                            if state_hash(&s1) == state_hash(&s2) { "ok" } else { "MISMATCH" }
                        ));
                    }

                    // Option 1 diagnostic: RNG call counter (0x51CA18) around the re-sim.
                    let cnt_restore = unsafe { *(0x0051_CA18 as *const u32) };

                    unsafe { self.ring[pos].snap.restore() };
                    let rng_restore = unsafe { *(0x0051_CA14 as *const u32) };
                    // Attribute the RNG call sites during the re-sim (Iteration 24).
                    let watch = crate::rng_watch::enabled() && f == 90;
                    if watch {
                        crate::log::log(&format!("[cosmo-rollback] RNGTRACE resim f{}", f));
                        crate::rng_watch::arm();
                    }
                    for _ in 0..steps {
                        unsafe { orig(ecx) };
                        // The outer main loop runs its own work between two frame calls; a
                        // re-sim must do the same or it cannot reach the live state (see
                        // `docs/analysis/restore-residual.md`).
                        if resim_loop() {
                            unsafe { crate::frame_hook::outer_loop_work() };
                        } else if resim_post() {
                            unsafe { crate::frame_hook::post_frame() };
                        } else if !no_resim_recv() {
                            // `receive_network_messages` (0x485F70) is the between-frame
                            // call that processes the next replay/network turn — it moves
                            // units, allocates orders and consumes RNG, all simulation
                            // state. The re-sim must run it (or it cannot reach the live
                            // state); it was omitted from both `CM_ROLLBACK_PROBE_OUTER`
                            // and the earlier `CM_ROLLBACK_RESIM_LOOP` set. The replay
                            // cursor it reads is snapshotted (`replay.rs`), so re-running
                            // it is deterministic. This is the default for the replay
                            // harness; `CM_ROLLBACK_NO_RESIM_RECV=1` disables it.
                            unsafe { crate::frame_hook::receive_network_messages() };
                        }
                    }
                    if watch {
                        crate::rng_watch::disarm();
                    }
                    let rng_resim = unsafe { *(0x0051_CA14 as *const u32) };
                    let cnt_resim = unsafe { *(0x0051_CA18 as *const u32) };
                    if f == 90 || f % 60 == 0 {
                        log(&format!(
                            "[cosmo-rollback] shadow f{} g{}: rng {}->{}, rng_calls {}->{} (delta {})",
                            f,
                            g,
                            rng_restore,
                            rng_resim,
                            cnt_restore,
                            cnt_resim,
                            cnt_resim as i64 - cnt_restore as i64
                        ));
                    }
                    let after = unsafe { Snapshot::capture(&self.ranges) };
                    let d_after = data_snapshot(f);
                    if f == 90 && !d_before.is_empty() && d_before.len() == d_after.len() {
                        let mut total = 0u32;
                        for i in 0..d_before.len() {
                            if d_before[i] != d_after[i] {
                                if total < 2000 {
                                    log(&format!(
                                        "[cosmo-rollback] DATA-DIFF 0x{:08x}",
                                        0x0050_C000 + i
                                    ));
                                }
                                total += 1;
                            }
                        }
                        log(&format!("[cosmo-rollback] DATA-DIFF total {} bytes", total));
                    }
                    let ignore: Vec<String> = std::env::var("CM_ROLLBACK_GATE_IGNORE")
                        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
                        .unwrap_or_default();
                    let ignore: Vec<&str> = ignore.iter().map(|s| s.as_str()).collect();
                    if state_hash_ignoring(&before, &ignore)
                        != state_hash_ignoring(&after, &ignore)
                    {
                        log(&format!(
                            "[cosmo-rollback] DIVERGENCE frame {} re-sim from {} ({} steps)",
                            f, g, steps
                        ));
                        diff_ranges(&self.ranges, &before, &after);
                    } else if f % 120 == 0 {
                        log(&format!(
                            "[cosmo-rollback] shadow ok frame {} (re-sim {} steps from {})",
                            f, steps, g
                        ));
                    }
                    // The re-sim left the simulation at its own end state. Restore this
                    // frame's entry state before running the real frame, or the shadow mode
                    // perturbs the very simulation it is measuring — which was a large,
                    // harness-caused divergence on every frame (units/sprites/orders; see
                    // `docs/analysis/restore-residual.md` §1.2).
                    unsafe { before.restore() };
                }
            }
        }

        // Save/load round-trip gate (`CM_ROLLBACK_SAVE_GATE=<depth>`): like the shadow
        // gate, but the snapshot that gets restored first travels through the save file
        // format (serialize → file → read → deserialize). A lossy format or a
        // cross-build range-table mismatch shows up as a divergence, exactly like a
        // lossy snapshot would (M1; see `docs/analysis/save-format.md`).
        if let Some(depth) = self.save_gate {
            if f >= depth && f >= self.save_gate_start {
                self.save_gate_step(f, depth, ecx, orig);
            }
        }

        // One-shot save (`CM_ROLLBACK_SAVE_AT=<frame>`): write the state at this
        // frame's entry to `<save_dir>/save_<frame>.cosmo` (harness/testing use).
        if self.save_at == Some(f) {
            self.save_at = None;
            self.write_save_now(f);
        }

        let rng_ptr = 0x0051_CA14 as *const u32;
        let cnt_ptr = 0x0051_CA18 as *const u32;
        let rng_in = unsafe { *rng_ptr };
        let cnt_in = unsafe { *cnt_ptr };
        // Attribute the RNG call sites during normal play for the same window the
        // shadow re-sim covers at frame 90 (frames 87..89).
        let watch_normal = crate::rng_watch::enabled() && (87..=89).contains(&f);
        if watch_normal {
            crate::log::log(&format!("[cosmo-rollback] RNGTRACE normal f{}", f));
            crate::rng_watch::arm();
        }

        // Injection smoke test (`CM_ROLLBACK_INJECT_TURN=1`): before running the real
        // step, inject a turn through `ProcessCommands` with the player context set, so
        // the injected command is part of this frame's simulation. This is the §4
        // injection primitive driven from the frame hook; it proves the calling
        // convention live (see `turn_hook::inject_turn`, which logs `g_sequence`
        // before/after). It is a smoke test, not the re-sim loop: the injected bytes are
        // a fixed blob, and the shadow re-sim above does not replay the injection, so a
        // real command will make the shadow gate diverge (expected; see
        // `docs/analysis/turn-capture-and-inject.md`). The default blob is the no-op
        // `0x05`, which only bumps `g_sequence` and leaves the sim state alone.
        if crate::turn_hook::inject_enabled() {
            let blob = crate::turn_hook::inject_blob();
            // v1 is 1v1; the local slot is 0. The player id 0 keeps attribution to the
            // local player without pretending a second client exists.
            crate::turn_hook::inject_turn(0, &blob, crate::turn_hook::inject_flag());
        }
        let r = unsafe { orig(ecx) };
        if watch_normal {
            crate::rng_watch::disarm();
        }
        let rng_out = unsafe { *rng_ptr };
        let cnt_out = unsafe { *cnt_ptr };
        if f < 40 || (84..=96).contains(&f) {
            log(&format!(
                "[cosmo-rollback] frame {} rng {} -> {} calls {} -> {}",
                f, rng_in, rng_out, cnt_in, cnt_out
            ));
        }
        self.frame = f.wrapping_add(1);
        // The turn flush happens inside `orig` (via `0x00485AA0`), so the captures for
        // this frame exist only now. Feed them to the session's input table (no-op unless
        // `CM_ROLLBACK_CAPTURE_TURN` is set).
        self.feed_captured_turns();
        // Remember this frame's exit state for the `CM_ROLLBACK_BOUNDARY` diagnostic.
        if std::env::var("CM_ROLLBACK_BOUNDARY").is_ok() {
            self.last_exit = Some(unsafe { Snapshot::capture(&self.ranges) });
        }
        // `CM_ROLLBACK_PROBE_OUTER=<frame>`: find which between-frame call mutates state.
        probe_outer_calls(self);
        r
    }

    /// The save-gate step: restore the ring snapshot at or before `f - depth` **through
    /// the save file format**, re-simulate to `f`, and compare the state hash against the
    /// live pass. Mirrors the shadow gate's re-sim (`CM_ROLLBACK_RESIM_LOOP` /
    /// `CM_ROLLBACK_RESIM_POST` / `CM_ROLLBACK_NO_RESIM_RECV` apply the same way).
    fn save_gate_step(
        &mut self,
        f: u32,
        depth: u32,
        ecx: u32,
        orig: unsafe extern "C" fn(u32) -> u32,
    ) {
        let target = f - depth;
        let Some(pos) = self.ring.iter().rposition(|e| e.frame <= target) else {
            log(&format!(
                "[cosmo-rollback] SAVEGATE f{f}: no ring snapshot at or before frame {target}"
            ));
            return;
        };
        let g = self.ring[pos].frame;
        let steps = f - g;
        let before = unsafe { Snapshot::capture(&self.ranges) };
        let snap_bytes = self.ring[pos].snap.serialize();
        let path = std::path::PathBuf::from(format!("{}/save_gate.cosmo", self.save_dir));
        if let Err(e) = crate::savefile::write_save(
            &path,
            g,
            crate::savefile::FLAG_HARNESS,
            &snap_bytes,
            &[],
        ) {
            log(&format!("[cosmo-rollback] SAVEGATE f{f}: write failed: {e}"));
            return;
        }
        let (loaded, kind) = match crate::savefile::read_save(&path) {
            Ok((frame, _, sb, _)) if frame == g => (Snapshot::deserialize(&sb), "deserialize"),
            Ok((frame, _, _, _)) => {
                log(&format!(
                    "[cosmo-rollback] SAVEGATE f{f}: file frame {frame} != {g} (stale file?)"
                ));
                return;
            }
            Err(e) => {
                log(&format!("[cosmo-rollback] SAVEGATE f{f}: read failed: {e}"));
                return;
            }
        };
        let snap = match loaded {
            Ok(snap) => snap,
            Err(e) => {
                log(&format!("[cosmo-rollback] SAVEGATE f{f}: {kind} failed: {e}"));
                return;
            }
        };
        unsafe { snap.restore() };
        for _ in 0..steps {
            unsafe { orig(ecx) };
            // The between-frame work a re-sim must also run (same rules as the shadow
            // gate; see `docs/analysis/restore-residual.md`).
            if resim_loop() {
                unsafe { crate::frame_hook::outer_loop_work() };
            } else if resim_post() {
                unsafe { crate::frame_hook::post_frame() };
            } else if !no_resim_recv() {
                unsafe { crate::frame_hook::receive_network_messages() };
            }
        }
        let after = unsafe { Snapshot::capture(&self.ranges) };
        let ignore: Vec<String> = std::env::var("CM_ROLLBACK_GATE_IGNORE")
            .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
            .unwrap_or_default();
        let ignore: Vec<&str> = ignore.iter().map(|s| s.as_str()).collect();
        if state_hash_ignoring(&before, &ignore) != state_hash_ignoring(&after, &ignore) {
            log(&format!(
                "[cosmo-rollback] SAVEGATE DIVERGENCE frame {f} re-sim from {g} through file round-trip ({steps} steps)"
            ));
            diff_ranges(&self.ranges, &before, &after);
        } else if f < 100 || f % 120 == 0 {
            log(&format!(
                "[cosmo-rollback] SAVEGATE ok frame {f} (re-sim {steps} steps from {g} through file round-trip)"
            ));
        }
        // Leave the live simulation exactly where it was.
        unsafe { before.restore() };
    }

    /// Write the current state as `<save_dir>/save_<frame>.cosmo`, and (with
    /// `CM_ROLLBACK_SAVE_VERIFY`) read it back and compare. The state saved is this
    /// frame's **entry** state, i.e. `frame` frames completed.
    fn write_save_now(&mut self, f: u32) {
        let snap = unsafe { Snapshot::capture(&self.ranges) };
        let snap_bytes = snap.serialize();
        let session = self
            .session
            .as_ref()
            .map(|s| s.save_state())
            .unwrap_or_default();
        let path = std::path::PathBuf::from(format!("{}/save_{}.cosmo", self.save_dir, f));
        match crate::savefile::write_save(
            &path,
            f,
            crate::savefile::FLAG_HARNESS,
            &snap_bytes,
            &session,
        ) {
            Ok(n) => {
                log(&format!(
                    "[cosmo-rollback] save: wrote {} ({} bytes; snapshot {}, session {})",
                    path.display(),
                    n,
                    snap_bytes.len(),
                    session.len()
                ));
                if self.save_verify {
                    self.verify_save(&path, f, &snap);
                }
            }
            Err(e) => log(&format!("[cosmo-rollback] save: write failed: {e}")),
        }
    }

    /// Read a save back and compare its snapshot hash against the in-memory snapshot it
    /// was written from. Proves the file round-trip is lossless on the real game.
    fn verify_save(&self, path: &std::path::Path, frame: u32, want: &Snapshot) {
        let verdict = match crate::savefile::read_save(path) {
            Ok((f, _, sb, _)) if f == frame => match Snapshot::deserialize(&sb) {
                Ok(got) => {
                    if state_hash(&got) == state_hash(want) {
                        "ok".to_string()
                    } else {
                        "MISMATCH (hash)".to_string()
                    }
                }
                Err(e) => format!("MISMATCH (deserialize: {e})"),
            },
            Ok((f, _, _, _)) => format!("MISMATCH (frame {f})"),
            Err(e) => format!("MISMATCH (read: {e})"),
        };
        log(&format!(
            "[cosmo-rollback] saveverify f{frame}: {verdict}"
        ));
    }
}

/// Run one game-logic frame under the rollback engine.
pub fn on_frame(ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) -> u32 {
    // The range table is built here, on the first frame, not at plugin startup: it resolves
    // pools the game only allocates when it starts.
    crate::audit::log_layout();
    match ENGINE.lock() {
        Ok(mut e) => e.on_step(ecx, orig),
        Err(_) => unsafe { orig(ecx) },
    }
}

/// The frame named by `CM_ROLLBACK_<var>`, if set and parseable.
fn det_frame(var: &str) -> Option<u32> {
    std::env::var(var).ok()?.parse().ok()
}

/// Whether `CM_ROLLBACK_RESIM_POST` asks a re-simulation to run the outer main loop's
/// post-frame update (`0x004D93F0`) after each re-simulated frame. The live path runs it
/// itself; a re-sim that does not cannot reach the live state (see
/// `docs/analysis/restore-residual.md`).
fn resim_post() -> bool {
    std::env::var("CM_ROLLBACK_RESIM_POST").is_ok()
}

/// Whether `CM_ROLLBACK_RESIM_LOOP` asks a re-simulation to run the outer main loop's whole
/// between-frame work (`frame_hook::outer_loop_work`). See `docs/analysis/restore-residual.md`.
fn resim_loop() -> bool {
    std::env::var("CM_ROLLBACK_RESIM_LOOP").is_ok()
}

/// Whether `CM_ROLLBACK_NO_RESIM_RECV` disables the default re-simulation of
/// `receive_network_messages` (see the re-sim loop).
fn no_resim_recv() -> bool {
    std::env::var("CM_ROLLBACK_NO_RESIM_RECV").is_ok()
}

/// `CM_ROLLBACK_PROBE_OUTER=<frame>`: at that frame, run each outer-loop between-frame call
/// in turn and log which of them changes the snapshot. Identifies what advances the
/// simulation outside the hooked frame function (see `docs/analysis/restore-residual.md`).
fn probe_outer_calls(engine: &mut Engine) {
    let Ok(want) = std::env::var("CM_ROLLBACK_PROBE_OUTER") else {
        return;
    };
    let all = want == "all";
    let at = if all {
        true
    } else {
        want.parse::<u32>() == Ok(engine.frame)
    };
    if !at {
        return;
    }
    if !all {
        log(&format!(
            "[cosmo-rollback] OUTER probe at f{}: running each between-frame call in turn",
            engine.frame
        ));
    }
    let f = engine.frame;
    let mut prev = unsafe { Snapshot::capture(&engine.ranges) };
    for (name, call) in crate::frame_hook::outer_calls() {
        unsafe { call() };
        let now = unsafe { Snapshot::capture(&engine.ranges) };
        if state_hash(&prev) != state_hash(&now) {
            if !all || f < 130 {
                log(&format!("[cosmo-rollback] OUTER f{f} {name}: STATE CHANGED"));
                diff_ranges(&engine.ranges, &prev, &now);
            }
        }
        prev = now;
    }
}

fn diff_ranges(ranges: &RangeList, before: &Snapshot, after: &Snapshot) {
    let mut off = 0usize;
    for r in &ranges.ranges {
        let a = &before.bytes()[off..off + r.len];
        let b = &after.bytes()[off..off + r.len];
        // Report how much of the range differs, not just the first byte: one stray byte
        // is a different bug class from a whole rewound entry, and the first offset
        // alone cannot tell them apart.
        let mut count = 0usize;
        let mut first: [usize; 8] = [0; 8];
        let mut shown = 0usize;
        for i in 0..a.len() {
            if a[i] != b[i] {
                count += 1;
                if shown < first.len() {
                    first[shown] = i;
                    shown += 1;
                }
            }
        }
        if count > 0 {
            let list = first[..shown]
                .iter()
                .map(|i| format!("+0x{:x}", i))
                .collect::<Vec<_>>()
                .join(" ");
            log(&format!(
                "    range {:<28} {} bytes differ of 0x{:x} (abs 0x{:08x}; first {}){}",
                r.name,
                count,
                r.len,
                r.addr + first[0],
                list,
                element_of(r.name, first[0]),
            ));
        }
        off += r.len;
    }
}

/// For a pool range, name the element and in-struct offset the first differing byte
/// falls in. A pool offset on its own ("+0x14275c") says nothing about which object
/// and field changed; the index and field offset do.
fn element_of(name: &str, off: usize) -> String {
    use crate::pools as p;
    let (stride, what) = match name {
        "units" => (p::UNIT_SIZE, "unit"),
        "sprites" => (p::SPRITE_SIZE, "sprite"),
        "orders" => (p::ORDER_SIZE, "order"),
        "images" => (p::IMAGE_SIZE, "image"),
        "thingies" => (p::THINGY_SIZE, "thingy"),
        "fow_sprites" => (p::FOW_SPRITE_SIZE, "fow_sprite"),
        "bullets" => (p::BULLET_SIZE, "bullet"),
        _ => return String::new(),
    };
    format!(" => {what} {} +0x{:x}", off / stride, off % stride)
}
