//! Drive-mode: exercise the full §7 loop (dispatch → inject → re-sim → compare) in the
//! replay harness by suppressing the replay injector and supplying every turn from the
//! input table.
//!
//! The replay harness's only input is gptp's `replay_loop` (patched onto `0x004D96C4`),
//! and its scheduler never stalls, so a live run never predicts, never mispredicts and
//! never rolls back. This module removes that input (`g_sync_turn_enabled` = 0) and
//! drives the simulation **entirely from a [`Session`]**, one turn per slot per step, so
//! a scripted late "remote" turn forces a real misprediction → rollback → re-simulation.
//!
//! Env knobs (all off unless `CM_ROLLBACK_DRIVE` is set):
//!   - `CM_ROLLBACK_DRIVE=1`                  enable drive mode
//!   - `CM_ROLLBACK_SCRIPT_LOCAL=<hex>`       local (slot 0) turn, default the no-op `05`
//!   - `CM_ROLLBACK_SCRIPT_REMOTE=<hex>`      remote (slot 1) turn, default `37 00 00 00 00 00 00`
//!                                            (`GameCommand_Hash`, non-no-op, so a late one
//!                                            mispredicts; harmless in a replay)
//!   - `CM_ROLLBACK_SCRIPT_DELAY=<n>`         steps the remote turn arrives late (default 2)
//!   - `CM_ROLLBACK_DRIVE_PROBE=<frame>`      run the re-sim determinism probe at this frame
//!                                            (default 60)
//!
//! The decisive proof is the probe: at `probe_frame` it restores the newest earlier
//! snapshot, re-runs the frames to `probe_frame` twice (dispatching + injecting the same
//! turns both times), and compares the state hash, the `sequence` range and the
//! `command_users` range. `sequence`/`command_users` are pure functions of the injected
//! turns, so they **must** match if the turn loop composes with the snapshot restore; the
//! full state hash is expected to differ in the baseline plugin-`.data`-driven ranges
//! (documented in `docs/analysis/turn-path.md`), which is reported separately.

#![allow(dead_code)]

use std::time::Instant;

use crate::log::log;
use crate::ranges::RangeList;
use crate::session::Session;
use crate::snapshot::Snapshot;
use crate::state_hash::state_hash;
use crate::turns::SLOTS;

pub struct Drive {
    session: Session,
    ranges: &'static RangeList,
    /// Local turn bytes, pushed on time each step (default the no-op `0x05`).
    local: Vec<u8>,
    /// Remote turn bytes, pushed `delay` steps late (default `GameCommand_Hash 0x37`).
    remote: Vec<u8>,
    /// Steps the remote turn arrives late, forcing a misprediction → rollback.
    delay: u32,
    /// Snapshots keyed by session frame, captured at spacing multiples.
    snaps: Vec<(u32, Snapshot)>,
    probe_frame: u32,
    probe_done: bool,
    suppress_done: bool,
    rolls: u32,
    tick: u32,
    step_logged: u32,
}

impl Drive {
    /// `None` when `CM_ROLLBACK_DRIVE` is not set (the default path is untouched).
    pub fn new() -> Option<Self> {
        if std::env::var("CM_ROLLBACK_DRIVE").is_err() {
            return None;
        }
        let spacing = std::env::var("CM_ROLLBACK_SNAPSHOT_SPACING")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(3);
        let local = hex_env("CM_ROLLBACK_SCRIPT_LOCAL", &[0x05]);
        let remote = hex_env("CM_ROLLBACK_SCRIPT_REMOTE", &[0x37, 0, 0, 0, 0, 0, 0]);
        let delay = env_u32("CM_ROLLBACK_SCRIPT_DELAY", 2);
        let probe_frame = env_u32("CM_ROLLBACK_DRIVE_PROBE", 60);
        // Local slot 0, remote slot 1 (1v1). Prediction limit 8, no lockstep start, no
        // artificial hold.
        let session = Session::new(0, &[1], 8, 0, spacing, [std::time::Duration::ZERO; SLOTS]);
        log(&format!(
            "[cosmo-rollback] drive: local {:02x?} remote {:02x?} delay {delay} probe f{probe_frame} spacing {spacing}",
            local, remote
        ));
        Some(Drive {
            session,
            ranges: crate::ranges::engine_ranges(),
            local,
            remote,
            delay,
            snaps: Vec::new(),
            probe_frame,
            probe_done: false,
            suppress_done: false,
            rolls: 0,
            tick: 0,
            step_logged: 0,
        })
    }

    /// Run one session tick: arrive turns, plan, restore on rollback, then dispatch →
    /// inject → `orig` for each step, snapshotting on the session's spacing.
    pub fn tick(&mut self, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) -> u32 {
        if !self.suppress_done {
            // `"nop"` (default): the `call replay_loop` was already NOP'd at install time
            // (frame_hook::install, which has the whack patcher). `"flag"`: zero the flag
            // here at the first frame.
            if crate::turn_hook::drive_suppress_method() == "flag" {
                crate::turn_hook::suppress_replay_injector();
            }
            self.suppress_done = true;
            // The initial state is the frame-0 snapshot a rollback to 0 restores.
            self.snapshot(0);
        }

        let current = self.session.current();

        // Local turn arrives on time.
        self.session.push_local(self.local.clone(), Instant::now());
        // Remote turn arrives `delay` steps late.
        if current >= self.delay {
            self.session.push_remote(1, self.remote.clone(), Instant::now());
        }

        let plan = self.session.plan();
        let restores = plan.restores(current);
        let mut ret = 0u32;

        if let Some(rf) = plan.restore_frame(current) {
            self.restore(rf);
            self.rolls += 1;
            if self.rolls <= 12 || self.rolls % 64 == 0 {
                log(&format!(
                    "[cosmo-rollback] drive rollback #{}: current {current} -> restore f{rf} (rollback_to {:?})",
                    self.rolls, plan.rollback_to
                ));
            }
        }

        for f in plan.start(current)..plan.present {
            let (turns, predicted) = self.session.dispatch(f);
            let mut injected = 0u32;
            for (slot, t) in turns.iter().enumerate() {
                if let Some(b) = t {
                    if !b.is_empty() {
                        crate::turn_hook::inject_turn(slot as u32, b, false);
                        injected += 1;
                    }
                }
            }
            ret = unsafe { orig(ecx) };
            let produced = f + 1;
            if plan.snapshots(produced) {
                self.snapshot(produced);
            }
            let n = self.step_logged;
            self.step_logged += 1;
            if n < 24 || n % 128 == 0 {
                log(&format!(
                    "[cosmo-rollback] drive step {f}{} predicted {predicted} injected {injected} -> seq {}",
                    if restores && f < current { " RESIM" } else { "" },
                    crate::turn_hook::read_g_sequence()
                ));
            }
        }

        let snap = unsafe { Snapshot::capture(self.ranges) };
        let hash = state_hash(&snap);
        let _ = self.session.settle(plan.present, hash);

        if !self.probe_done && plan.present >= self.probe_frame {
            self.probe_done = true;
            self.run_probe(ecx, orig);
        }

        self.tick += 1;
        ret
    }

    fn snapshot(&mut self, frame: u32) {
        let snap = unsafe { Snapshot::capture(self.ranges) };
        if let Some(e) = self.snaps.iter_mut().find(|(f, _)| *f == frame) {
            e.1 = snap;
        } else {
            self.snaps.push((frame, snap));
            self.snaps.sort_by_key(|(f, _)| *f);
        }
    }

    fn restore(&mut self, frame: u32) {
        let idx = self.snaps.iter().rposition(|(f, _)| *f <= frame);
        match idx {
            Some(i) => {
                let (g, snap) = &self.snaps[i];
                unsafe { snap.restore() };
                if *g != frame {
                    log(&format!(
                        "[cosmo-rollback] drive restore f{frame}: using snapshot f{g}"
                    ));
                }
            }
            None => log(&format!(
                "[cosmo-rollback] drive restore f{frame}: NO snapshot available"
            )),
        }
    }

    /// The decisive check: re-simulate `g..probe_frame` twice from the same snapshot and
    /// compare. See the module docs for what a match proves.
    fn run_probe(&mut self, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) {
        let f = self.probe_frame;
        let Some(g) = self
            .snaps
            .iter()
            .rev()
            .find(|(gg, _)| *gg < f)
            .map(|(gg, _)| *gg)
        else {
            log(&format!(
                "[cosmo-rollback] drive probe f{f}: no earlier snapshot; skipped"
            ));
            return;
        };
        // Restore frame g, then capture a fresh base snapshot of it to re-sim from.
        self.restore(g);
        let base = unsafe { Snapshot::capture(self.ranges) };

        let (h1, s1, c1) = self.resim(&base, g, f, ecx, orig);
        let (h2, s2, c2) = self.resim(&base, g, f, ecx, orig);
        log(&format!(
            "[cosmo-rollback] drive probe f{f} g{g} steps{}: state_hash {} sequence {} command_users {}",
            f - g,
            if h1 == h2 { "MATCH" } else { "MISMATCH" },
            if s1 == s2 { "MATCH" } else { "MISMATCH" },
            if c1 == c2 { "MATCH" } else { "MISMATCH" },
        ));

        if h1 != h2 {
            // Name the ranges that differ across two identical re-sims. Expected: the
            // baseline plugin-`.data`-driven pool/query ranges, not turn state.
            let a = self.resim_snapshot(&base, g, f, ecx, orig);
            let b = self.resim_snapshot(&base, g, f, ecx, orig);
            diff_ranges(&a, &b);
        }

        // Leave the engine at the live frame-f state so the session and engine stay aligned.
        if let Some((_, live)) = self.snaps.iter().find(|(g2, _)| *g2 == f) {
            unsafe { live.restore() };
        }
    }

    /// Restore `base`, run `g..f` (dispatch → inject → `orig`), and return the hash plus
    /// the `sequence` and `command_users` range bytes of the result.
    fn resim(
        &mut self,
        base: &Snapshot,
        g: u32,
        f: u32,
        ecx: u32,
        orig: unsafe extern "C" fn(u32) -> u32,
    ) -> (u64, Vec<u8>, Vec<u8>) {
        let snap = self.resim_snapshot(base, g, f, ecx, orig);
        (
            state_hash(&snap),
            range_bytes(&snap, "sequence").unwrap_or(&[]).to_vec(),
            range_bytes(&snap, "command_users").unwrap_or(&[]).to_vec(),
        )
    }

    fn resim_snapshot(
        &mut self,
        base: &Snapshot,
        g: u32,
        f: u32,
        ecx: u32,
        orig: unsafe extern "C" fn(u32) -> u32,
    ) -> Snapshot {
        unsafe { base.restore() };
        for s in g..f {
            let (turns, _) = self.session.dispatch(s);
            for (slot, t) in turns.iter().enumerate() {
                if let Some(b) = t {
                    if !b.is_empty() {
                        crate::turn_hook::inject_turn(slot as u32, b, false);
                    }
                }
            }
            let _ = unsafe { orig(ecx) };
        }
        unsafe { Snapshot::capture(self.ranges) }
    }
}

fn range_bytes<'a>(snap: &'a Snapshot, name: &str) -> Option<&'a [u8]> {
    snap.range_iter().find(|(n, _)| *n == name).map(|(_, b)| b)
}

fn diff_ranges(a: &Snapshot, b: &Snapshot) {
    for ((name, x), (_, y)) in a.range_iter().zip(b.range_iter()) {
        if x != y {
            let n = x.iter().zip(y).filter(|(p, q)| p != q).count();
            log(&format!("    range {name:<24} {n} bytes differ"));
        }
    }
}

fn hex_env(var: &str, default: &[u8]) -> Vec<u8> {
    match std::env::var(var) {
        Ok(s) => match decode_hex(&s) {
            Some(v) if !v.is_empty() => v,
            _ => {
                log(&format!(
                    "[cosmo-rollback] drive: {var}={s:?} not valid hex; using default {default:02x?}"
                ));
                default.to_vec()
            }
        },
        Err(_) => default.to_vec(),
    }
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

fn env_u32(var: &str, default: u32) -> u32 {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
