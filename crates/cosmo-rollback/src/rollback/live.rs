//! The live rollback client: what an actual match runs.
//!
//! Per game frame it does what a GGPO-style client does:
//!
//! 1. **Absorb** everything the peer has sent (turns by sequence number, desync hashes,
//!    acknowledgements, leaves).
//! 2. **Send** the local turn — read from the engine's turn buffer *before* the frame's flush
//!    clears it, which is the turn for the step this frame runs (`turn_hook::local_turn`).
//! 3. **Stall** if a slot's turns are missing and the prediction limit is reached; otherwise
//!    **plan** (roll back if a prediction was contradicted), **restore** the snapshot at or
//!    before the rollback target, and run the steps: dispatch every slot's turn for the step,
//!    inject it (`ProcessCommands`), then call the frame function.
//! 4. **Settle**: record the frame, report a desync hash, and (on the host) adjudicate the
//!    peer's reports.
//! 5. **Cadence**: acknowledge the peer's turns and re-send anything unacknowledged — the
//!    channel is plain UDP, so a turn lost in transit would diverge the clients for good.
//!
//! ## Why the engine's turn path is suppressed
//!
//! The engine applies turns itself at the network step (`0x00486F50` → `ProcessCommands`),
//! including the local one, and sends the local turn to CrownLink. A rollback client applies
//! *every* slot's turn for *every* step (including re-simulated ones) through its own input
//! table, so both would double-apply. `install` therefore no-ops the turn processor, and the
//! turn-pipeline fill zeroes the outgoing buffer so only the engine's empty-turn marker
//! travels the lockstep stream; the real commands travel on the rollback channel and are
//! injected by the client (`ProcessCommands`) — the **only** applier of turns in a live
//! match. The turn scheduler is forced ready so the frame is never skipped for a missing
//! peer turn. Everything is behind `CM_ROLLBACK_LIVE`, so the default path is untouched.
//!
//! The match's start turns ("start game" + per-player init) are applied by the engine
//! itself *before* the client takes over (`g_is_in_game` flips after init), so the first
//! two live steps skip injection (`skip_start`, `CM_ROLLBACK_SKIP_START_TURNS`) — injecting
//! them again replays init on an initialised world and crashes inside `ProcessCommands`.
//!
//! ## What is not validated
//!
//! A replay harness has no peer (`CrownLink::connect()` returns `None`, and the engine's turn
//! path is inert), so this exercises the single-slot path only. Real turns, prediction and
//! rollback need two clients in a live match — `tools/two_client_test.sh` (UDP, one machine)
//! and `tools/crownlink_match.sh` (the real channel) drive exactly that.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use crate::log::log;
use crate::ranges::{self, RangeList};
use crate::session::{Authority, Session, Verdict};
use crate::snapshot::Snapshot;
use crate::state_hash::desync_hash;
use crate::transport::{CrownLink, Link, Msg};
use crate::turns::SLOTS;

// The engine's turn processor: it applies every available player's turn. Live mode no-ops it
// (the rollback client injects every turn itself).
whack_hooks!(0x00400000,
    0x00486F50 => ProcessTurns() -> u32;
);

/// Whether `CM_ROLLBACK_LIVE` asks for the live client.
pub fn enabled() -> bool {
    std::env::var("CM_ROLLBACK_LIVE").is_ok()
}

/// Set when the live client has fallen back to normal lockstep (the peer never spoke
/// the channel, so the other player is not running the plugin). While set, the engine's
/// own turn path is left intact so the fallback match still plays: the turn processor is
/// not no-op'd, the turn-pipeline fill does not zero the outgoing buffer, and the turn
/// scheduler is not forced ready.
static FALLBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the live client has fallen back to normal lockstep.
pub fn fallen_back() -> bool {
    FALLBACK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Arm (or disarm) the lockstep fallback. Cleared when the client stops driving, so a
/// subsequent match starts fresh.
pub(crate) fn set_fallback(on: bool) {
    FALLBACK.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Suppress the engine's own turn path so it applies no turns at all.
///
/// Called from `frame_hook::install` with the exe patcher. Each patch verifies the bytes it
/// expects first and logs either way, so a mismatched build is reported rather than silently
/// doing nothing.
pub fn install(exe: &mut whack::ModulePatcher) {
    if !enabled() {
        return;
    }
    crate::crash::install();
    // The engine must apply **no** turns: the live client dispatches and injects every slot's
    // turn for every step, including re-simulated ones. `0x00486F50` reads the per-player turn
    // store and does not clear it, so a re-simulation would otherwise re-apply *whatever the
    // store happens to hold now* — a different frame's turns — and the two passes would
    // disagree. No-oping it makes the live pass and a re-sim apply exactly what the client
    // injects.
    //
    // No-oping the *send* instead (`CM_ROLLBACK_LIVE_NOP_SEND=1`) hangs the game: the start
    // loop (`0x004D9A4A`) fills the turn pipeline until `step_network` reports ready.
    unsafe {
        exe.hook_closure(ProcessTurns, move |orig: unsafe extern "C" fn() -> u32| -> u32 {
            // Only once the match is running, and only while the client is driving. In
            // the lobby the engine's own turn processing carries the lobby state and the
            // map distribution; no-oping it there leaves the joiner with an empty slot
            // list and the match never starts. After a lockstep fallback the engine must
            // apply its own turns again or the fallback match plays on empty input.
            if !in_game() || fallen_back() {
                return orig();
            }
            0
        });
    }
    log("[cosmo-rollback] live: engine turn processing suppressed (0x00486F50 no-op)");

    if std::env::var("CM_ROLLBACK_LIVE_NOP_SEND").is_ok() {
        crate::turn_hook::suppress_lockstep_send(exe);
    }
}

/// Whether a re-simulated step should re-run the outer loop's `receive_network_messages`.
/// On by default (`CM_ROLLBACK_NO_RESIM_RECV=1` disables, as in the shadow gate).
fn resim_receive() -> bool {
    std::env::var("CM_ROLLBACK_NO_RESIM_RECV").is_err()
}

/// Parses a whitespace-tolerant hex string, e.g. `"37 00 00"` or `"370000"`.
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

/// How many snapshots the ring keeps. Sized to the deepest rollback the prediction limit can
/// ask for, plus slack: a rollback restores the newest snapshot at or before its target, so it
/// needs `limit / spacing + 1` entries.
fn ring_depth(limit: u32, spacing: u32) -> usize {
    ((limit / spacing.max(1)) as usize + 4).max(6)
}

pub struct Live {
    session: Session,
    /// The transport: CrownLink's channel when the loaded `.snp` carries it, a UDP socket
    /// when `CM_ROLLBACK_UDP` names one, or `None` in a replay harness.
    link: Option<Box<dyn Link + Send>>,
    /// Whether the link is the CrownLink channel, resolved lazily at match takeover
    /// (storm loads the `.snp` only when the lobby binds the provider, long after plugin
    /// startup — see `ensure_link`).
    want_crownlink: bool,
    /// The host's hash adjudicator (`None` on a guest).
    authority: Option<Authority>,
    ranges: &'static RangeList,
    /// Snapshots by frame (spacing multiples), oldest first.
    ring: Vec<(u32, Snapshot)>,
    spacing: u32,
    limit: u32,
    /// Steps since the last acknowledge/re-send cadence.
    since_cadence: u32,
    /// The scripted remote turn used when there is no peer (a replay harness): the loop's
    /// prediction/rollback path needs a second slot. `CM_ROLLBACK_SCRIPT_REMOTE` /
    /// `CM_ROLLBACK_SCRIPT_DELAY` mirror `rollback/drive.rs`.
    script_remote: Vec<u8>,
    script_delay: u32,
    /// Whether a scripted peer was explicitly asked for. Without a transport *and* without
    /// this, live mode refuses to drive: it would stall forever waiting for a peer, which
    /// would freeze a replay or a single-player game.
    scripted: bool,
    /// The next scripted remote sequence number to push, and the tick counter that drives it
    /// (the peer sends its turn for step `s` at its tick `s`, and it reaches us `delay` ticks
    /// later — so the script must advance with ticks, not with our stalled `current`).
    script_issued: u32,
    ticks: u32,
    /// Steps at the start that wait for every turn (`Session`'s lockstep start).
    lockstep_steps: u32,
    /// Frames the session refused to advance on (waiting for a turn).
    stalls: u32,
    /// Live steps whose turns the engine already applied before takeover (the lockstep start);
    /// their injection is skipped. `CM_ROLLBACK_SKIP_START_TURNS`, default 0.
    skip_start: u32,
    /// Live steps taken so far (the counter `skip_start` applies to).
    live_steps: u32,
    /// Set when the match has an AI player; live driving is refused (see `FIRST_AI_SCRIPT`).
    ai_present: bool,
    /// When the client took over (used to detect a peer that never speaks the channel).
    started_at: Option<Instant>,
    /// How long after takeover a client waits for the peer's first rollback turn
    /// before falling back to normal lockstep (`CM_ROLLBACK_PEER_GRACE`, seconds).
    /// The farm starts both clients from separate menu navigations that finish at
    /// slightly different times, so it sets a generous value.
    peer_grace: Duration,
    /// Whether a peer turn has ever arrived.
    peer_seen: bool,
    /// Set when the peer never spoke the channel: the other player is not running the plugin,
    /// so the client falls back to normal lockstep instead of stalling the match.
    no_peer: bool,
    /// Whether the client injects turns itself. Always true while driving: the engine's
    /// turn processor is no-op'd in live mode and the rollback channel never enters the
    /// engine's lockstep queue, so the client is the only applier of turns on every
    /// carrier. `CM_ROLLBACK_NO_INJECT=1` forces it off (diagnostic).
    inject: bool,
    /// Rollbacks performed, and steps run.
    rolls: u32,
    steps: u32,
    logged_stalls: u32,
    logged_rolls: u32,
    /// Hashes we have sent, and adjudications that agreed (both for bounded logging).
    hashes_sent: u32,
    agreements: u32,
    /// Whether the game has started (before that the frame is passed straight through).
    started: bool,
    /// Whether the no-transport refusal has been logged (it would otherwise spam once
    /// per frame while the `.snp` loads).
    refused_logged: bool,
    /// The newest confirmed frame whose hash we have sent.
    hash_reported: u32,
    /// Report a desync hash every this many confirmed frames (ADR-0001 §1.1 says 8; a smaller
    /// value converges a test run faster).
    hash_every: u32,
    /// The frame whose desync fingerprint has been logged, so it is logged once.
    desync_logged: Option<u32>,
}

/// `g_is_in_game` (`BW/bwdata.h`): 0 in the menus, 1 once the match is running.
pub(crate) const G_IS_IN_GAME: usize = 0x006D_5BC8;

/// The players array (`bwdata.h`): 8 × `Player` (0x24 bytes), `Player.player_type` at
/// +0x08 (`bw_dat::structs::Player`; `PlayerType::Computer = 1`, `Human = 2`).
const PLAYERS: usize = 0x0057_EEE0;
const PLAYER_STRIDE: usize = 0x24;
const PLAYER_TYPE_OFF: usize = 0x08;
const PLAYER_TYPE_COMPUTER: u8 = 1;

/// Whether any game slot is a computer player. This is the precise test: the earlier
/// `first_ai_script != 0` signal is also set by neutral critters and mod script units,
/// which would wrongly refuse to drive every ordinary human game. A computer player
/// cannot be rolled back (aise keeps its state outside the engine snapshot, and its
/// decisions diverge on a re-simulation), so such a match passes frames through.
fn computer_player_present() -> bool {
    let mut types = [0u8; 8];
    let mut any = false;
    for i in 0..8usize {
        let ty = unsafe { *((PLAYERS + i * PLAYER_STRIDE + PLAYER_TYPE_OFF) as *const u8) };
        types[i] = ty;
        if ty == PLAYER_TYPE_COMPUTER {
            any = true;
        }
    }
    // Diagnostic: dump the first four players' records (id, type, race, force, name)
    // so the slot mapping can be verified against the lobby.
    unsafe {
        for i in 0..4usize {
            let base = (PLAYERS + i * PLAYER_STRIDE) as *const u8;
            let id = std::ptr::read_unaligned(base as *const u32);
            let ty = *base.add(0x08);
            let race = *base.add(0x09);
            let force = *base.add(0x0A);
            let name = std::slice::from_raw_parts(base.add(0x0B), 25);
            let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
            log(&format!(
                "[cosmo-rollback] live: player[{i}] id {id} type {ty} race {race} force {force} name {:?}",
                String::from_utf8_lossy(name)
            ));
        }
    }
    if any {
        log(&format!(
            "[cosmo-rollback] live: player types {:02x?} (computer slot present)",
            types
        ));
    }
    any
}

/// Whether the engine is actually in a match (as opposed to a lobby or menu).
pub(crate) fn in_game() -> bool {
    unsafe { *(G_IS_IN_GAME as *const u32) != 0 }
}

impl Live {
    pub fn new() -> Self {
        let spacing = std::env::var("CM_ROLLBACK_SNAPSHOT_SPACING")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(crate::tick::DEFAULT_SNAPSHOT_SPACING);
        // How many steps past the newest fully-known one a step may run on predictions.
        let limit = std::env::var("CM_ROLLBACK_PREDICT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        // v1 is 1v1: slot 0 is the session creator (the host, ADR-0001 §1) and slot 1 the
        // guest. The local slot is the host's.
        let local_slot = std::env::var("CM_ROLLBACK_SLOT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0usize);
        let remote = if local_slot == 0 { 1 } else { 0 };
        // The first few steps wait for every turn, as lockstep does, so both clients line
        // their simulations up before either keeps its own schedule.
        let lockstep_steps = limit.min(4);
        let session = Session::new(
            local_slot,
            &[remote],
            limit,
            lockstep_steps,
            spacing,
            [Duration::ZERO; SLOTS],
        );
        // `CM_ROLLBACK_UDP=<local_port>:<peer_port>[:delay_ms]` picks a UDP carrier instead,
        // so two clients can be run on one machine to exercise the transport for real.
        //
        // The CrownLink channel is resolved LAZILY (at match takeover), not here: storm
        // loads `CrownLink.snp` only when the lobby binds the provider, which is long
        // after plugin startup — resolving at init would always find no module and the
        // live client would refuse to drive a real match. See `ensure_link` in `tick`.
        let link: Option<Box<dyn Link + Send>> = match std::env::var("CM_ROLLBACK_UDP") {
            Ok(spec) => {
                let parts: Vec<&str> = spec.split(':').collect();
                let local = parts.first().and_then(|p| p.parse().ok());
                let peer = parts.get(1).and_then(|p| p.parse().ok());
                let delay = parts
                    .get(2)
                    .and_then(|p| p.parse().ok())
                    .map(Duration::from_millis)
                    .unwrap_or_default();
                match (local, peer) {
                    (Some(local), Some(peer)) => match crate::transport::UdpLink::new(local, peer, delay) {
                        Some(l) => {
                            log(&format!(
                                "[cosmo-rollback] live: UDP link on {local} -> {peer} (delay {delay:?})"
                            ));
                            Some(Box::new(l))
                        }
                        None => {
                            log(&format!("[cosmo-rollback] live: cannot bind UDP port {local}"));
                            None
                        }
                    },
                    _ => {
                        log(&format!("[cosmo-rollback] live: bad CM_ROLLBACK_UDP {spec:?}"));
                        None
                    }
                }
            }
            // A real match: resolved at takeover (see `ensure_link`).
            Err(_) => None,
        };
        let want_crownlink = std::env::var("CM_ROLLBACK_UDP").is_err();
        let inject = link.as_ref().map(|l| l.inject_turns()).unwrap_or(true);
        log(&format!(
            "[cosmo-rollback] live: transport {} — injection {}",
            match &link {
                Some(_) => if inject { "live carrier (client injects every turn)" } else { "CM_ROLLBACK_NO_INJECT (diagnostic)" },
                None if want_crownlink => "CrownLink channel (resolved at match takeover)",
                None => "none",
            },
            if inject { "on" } else { "off" }
        ));
        let authority = (local_slot == 0).then(|| Authority::new(2));
        // The match's start turns ("start game" + per-player init) are applied by the
        // engine itself before the client takes over, so the first live steps must not
        // inject them again — that replays init on an initialised world and crashes
        // inside `ProcessCommands`. A real match (CrownLink carrier) defaults to 2; the
        // UDP/replay harness has no engine-applied start turns, so its default is 0.
        // `CM_ROLLBACK_SKIP_START_TURNS` overrides either way.
        let over_udp = std::env::var("CM_ROLLBACK_UDP").is_ok();
        let skip_start = std::env::var("CM_ROLLBACK_SKIP_START_TURNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if over_udp { 0 } else { 2 });
        // With no peer the harness still needs a second slot for the loop to do anything, so
        // the remote turn is scripted: the no-op on time by default (no misprediction), or
        // `CM_ROLLBACK_SCRIPT_REMOTE` delayed by `CM_ROLLBACK_SCRIPT_DELAY` steps to force
        // real rollbacks.
        let script_remote = std::env::var("CM_ROLLBACK_SCRIPT_REMOTE")
            .ok()
            .and_then(|h| decode_hex(&h))
            .unwrap_or_else(|| vec![crate::turns::SYNC_NOP]);
        let script_delay = std::env::var("CM_ROLLBACK_SCRIPT_DELAY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let scripted = std::env::var("CM_ROLLBACK_SCRIPT_REMOTE").is_ok()
            || std::env::var("CM_ROLLBACK_SCRIPT_PEER").is_ok();
        let peer_grace = std::env::var("CM_ROLLBACK_PEER_GRACE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(3));
        if link.is_none() && !want_crownlink {
            log(
                "[cosmo-rollback] live: no transport (replay harness or stock .snp); running \
                 the scripted single-peer path",
            );
        }
        Self {
            session,
            link,
            want_crownlink,
            authority,
            ranges: ranges::engine_ranges(),
            ring: Vec::new(),
            spacing,
            limit,
            script_remote,
            script_delay,
            scripted,
            script_issued: 0,
            ticks: 0,
            lockstep_steps,
            since_cadence: 0,
            stalls: 0,
            inject,
            ai_present: false,
            started_at: None,
            peer_grace,
            peer_seen: false,
            no_peer: false,
            skip_start,
            live_steps: 0,
            rolls: 0,
            steps: 0,
            logged_stalls: 0,
            logged_rolls: 0,
            hashes_sent: 0,
            agreements: 0,
            started: false,
            refused_logged: false,
            hash_reported: 0,
            hash_every: std::env::var("CM_ROLLBACK_HASH_EVERY")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(8),
            desync_logged: None,
        }
    }

    pub fn current(&self) -> u32 {
        self.session.current()
    }

    /// Resolve the CrownLink channel, once its module is loadable. Called from `tick`:
    /// storm only loads `CrownLink.snp` when the lobby binds the provider, so resolving
    /// at plugin startup would always fail. Idempotent: once bound, it stays bound.
    fn ensure_link(&mut self) {
        if !self.want_crownlink || self.link.is_some() {
            return;
        }
        match CrownLink::connect() {
            Some(c) => {
                log("[cosmo-rollback] live: CrownLink rollback channel bound at takeover");
                self.link = Some(Box::new(c));
            }
            None => {
                // Not loaded yet (still in the lobby) — or genuinely absent (stock
                // .snp). Either way the grace logic in `tick` decides the outcome.
            }
        }
    }

    /// Our own recorded desync hash for `frame`, if we produced it.
    fn hash_of(&self, frame: u32) -> Option<u64> {
        self.session.hash_of(frame)
    }

    /// One game frame. `orig` is the engine's frame function through the trampoline; it is
    /// called once per simulated step (more than once when a rollback re-simulates).
    pub fn tick(&mut self, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) -> u32 {
        // Do not drive the simulation from the menu: the two clients must start from the same
        // state, and the only state they share is the start of the game (a real match starts
        // from the map init; this harness starts both clients from the same replay's frame 0).
        // Until `g_is_in_game` is set, pass the frame straight through.
        if unsafe { *(G_IS_IN_GAME as *const u32) } == 0 {
            // A new match starts fresh: a previous match's fallback/AI verdict does not
            // carry into the next one.
            if self.started {
                self.started = false;
                self.no_peer = false;
                self.ai_present = false;
                self.peer_seen = false;
                self.started_at = None;
                set_fallback(false);
            }
            return unsafe { orig(ecx) };
        }
        // A real match resolves its CrownLink channel now (the .snp was loaded by the
        // lobby's provider bind) — retried every tick until it succeeds or the grace
        // logic below falls back to lockstep.
        self.ensure_link();
        if self.ai_present || self.no_peer {
            return unsafe { orig(ecx) };
        }
        if self.link.is_none() && !self.scripted {
            if !self.refused_logged {
                self.refused_logged = true;
                log(
                    "[cosmo-rollback] live: no transport and no scripted peer \
                     (CM_ROLLBACK_SCRIPT_PEER); passing frames through rather than freezing \
                     on a peer that cannot exist",
                );
            }
            // `started` stays false: if the channel resolves on a later tick (the .snp
            // is still loading), the takeover block still runs its checks.
            return unsafe { orig(ecx) };
        }
        if !self.started {
            self.started = true;
            // A computer player cannot be rolled back: aise keeps its state outside the
            // engine, so a re-simulation would make different decisions. Refuse to drive
            // rather than desync. `CM_ROLLBACK_ALLOW_AI=1` drives anyway (diagnostic /
            // farm soak): expect hash divergences while its script runs.
            if computer_player_present() {
                if std::env::var("CM_ROLLBACK_ALLOW_AI").is_err() {
                    log(
                        "[cosmo-rollback] live: computer player present — its state is not \
                         rolled back; passing frames through (normal lockstep, no rollback)",
                    );
                    self.ai_present = true;
                } else {
                    log(
                        "[cosmo-rollback] live: computer player present but CM_ROLLBACK_ALLOW_AI=1 — \
                         driving anyway; desync hashes will diverge while the AI runs",
                    );
                }
            }
            self.started_at = Some(Instant::now());
            // Farm: once the match has started, free-run the simulation and stop
            // presenting frames (the menus stay paced + rendered for the OCR
            // navigation). See `farm.rs`.
            crate::farm::unpace_and_hide_render();
            // Identify the state we take over from: if two clients disagree here, their
            // divergence is the setup, not the netcode.
            unsafe {
                log(&format!(
                    "[cosmo-rollback] live: slot {} — game started; taking over at units {} map {}x{} elapsed {} hash {:#x}",
                    self.session.local_slot(),
                    *(0x0062_83F0 as *const u32),
                    *(0x0057_F1D4 as *const u16),
                    *(0x0057_F1D6 as *const u16),
                    *(0x0058_D6F8 as *const u32),
                    desync_hash(&Snapshot::capture(self.ranges)),
                ));
            }
        }

        let now = Instant::now();
        self.ticks = self.ticks.wrapping_add(1);
        let current = self.session.current();

        // Farm command streams: inject this step's scripted turns for every armed slot
        // (identical files on both clients -> identical inputs; see `farm.rs`).
        crate::farm::inject_stream_turns(self.steps, self.limit);

        // 1. Absorb the peer's messages. Turns go into the input table by sequence number.
        if let Some(link) = self.link.as_mut() {
            self.session.absorb(link, now);
        }
        // A desync the peer's hash just revealed: dump a per-range fingerprint of our own state
        // so the two clients' logs can be diffed to find the range that differs.
        if let Some(f) = self.session.desync_at() {
            if self.desync_logged != Some(f) {
                self.desync_logged = Some(f);
                let snap = unsafe { Snapshot::capture(self.ranges) };
                for (name, bytes) in snap.range_iter() {
                    if name.ends_with(".render") || !crate::state_hash::is_compared(name) {
                        continue;
                    }
                    log(&format!(
                        "[cosmo-rollback] live desync f{f} range {name} {:016x}",
                        crate::state_hash::hash_bytes(bytes)
                    ));
                }
            }
        }

        // 2. The local turn for the step this frame runs: read it now, before the frame's
        //    flush clears the buffer. The engine's own send is suppressed, so this is the only
        //    way it reaches the peer.
        let local = crate::turn_hook::local_turn();
        match self.link.as_mut() {
            Some(link) => self.session.send_local_turn(link, current, local, now),
            None => {
                self.session.push_local_seq(current, local, now);
                // Harness only: the scripted peer sends its turn for step `s` at its tick
                // `s`. During the lockstep start both clients wait for every turn, so those
                // arrive on time; after it, they arrive `script_delay` ticks late — which is
                // what forces a prediction and then a rollback. Driving it off `ticks` (not off
                // our `current`, which is stalled while we wait) is what lets the start complete
                // at all.
                loop {
                    let seq = self.script_issued;
                    let lateness = if seq < self.lockstep_steps {
                        0
                    } else {
                        self.script_delay
                    };
                    if seq + lateness > self.ticks {
                        break;
                    }
                    self.session
                        .push_remote_seq(1, seq, self.script_remote.clone(), now);
                    self.script_issued += 1;
                }
            }
        }

        // A peer that never speaks the channel is not running the plugin (or the .snp carries
        // no channel). Arming is per player, so this happens whenever only one side installed
        // it — fall back to normal lockstep instead of stalling the match.
        if !self.peer_seen && self.session.confirmed() > 0 {
            self.peer_seen = true;
        }
        let waited = self.started_at.map(|t| t.elapsed()).unwrap_or_default();
        if !self.peer_seen && waited >= self.peer_grace {
            self.peer_seen = true;
            if self.session.confirmed() == 0 {
                log(
                    "[cosmo-rollback] live: no rollback turn from the peer within 3 s — the \
                     other player is not running the plugin (or has no rollback-capable \
                     CrownLink.snp); passing frames through, normal lockstep",
                );
                self.no_peer = true;
                // Give the engine its own turn path back (turn processor + outgoing
                // buffer + scheduler), or the fallback match would play on empty input.
                set_fallback(true);
                return unsafe { orig(ecx) };
            }
        }

        // 3. Stall rather than predict past the limit. The engine's frame is not run, so the
        //    simulation does not advance — the same behaviour as its own lockstep stall.
        if self.session.can_run().is_err() {
            self.stalls += 1;
            if self.logged_stalls < 8 || self.stalls % 120 == 0 {
                self.logged_stalls += 1;
                log(&format!(
                    "[cosmo-rollback] live stall #{} at frame {current}: waiting on {:?} (current {} confirmed {})",
                    self.stalls,
                    self.session.holding_back(),
                    self.session.current(),
                    self.session.confirmed(),
                ));
            }
            return 0;
        }

        // 4. Plan, and roll back if the input table has contradicted a prediction.
        let plan = self.session.plan();
        if let Some(rf) = plan.restore_frame(current) {
            match self.ring.iter().find(|(f, _)| *f == rf) {
                Some((_, snap)) => unsafe { snap.restore() },
                None => log(&format!(
                    "[cosmo-rollback] live: no snapshot for restore frame {rf}; cannot roll back"
                )),
            }
            self.rolls += 1;
            if self.logged_rolls < 8 || self.rolls % 60 == 0 {
                self.logged_rolls += 1;
                log(&format!(
                    "[cosmo-rollback] live rollback #{}: frame {current} -> restore {rf} (target {:?}, {} steps)",
                    self.rolls,
                    plan.rollback_to,
                    plan.present - plan.start(current),
                ));
            }
        }

        // 5. Run the steps. Every slot's turn for the step is injected first, so the frame
        //    sees exactly the commands both clients agreed on.
        // A real match starts with the engine's own lockstep turns: the "start game" and
        // per-player init commands are applied by the engine *before* this client takes over,
        // so injecting them again replays init on an initialised world (which crashes inside
        // `ProcessCommands`). Those first turns are already in the input table, so re-sims can
        // still replay them; the live pass simply must not apply them twice.
        // `CM_ROLLBACK_SKIP_START_TURNS=<n>` skips the first `n` live steps' injection.
        let mut r = 0;
        // Phase tracing: the engine's crash report cannot symbolise a call through a bad
        // pointer, so the last marker printed is what localises a crash.
        let trace = self.ticks <= 64 || self.ticks % 600 == 0;
        for step in plan.start(current)..plan.present {
            let (turns, _predicted) = self.session.dispatch(step);
            if trace {
                let shown: Vec<String> = turns
                    .iter()
                    .map(|t| match t {
                        Some(b) => format!("{}:{:02x?}", b.len(), &b[..b.len().min(48)]),
                        None => "-".to_string(),
                    })
                    .collect();
                log(&format!(
                    "[cosmo-rollback] live: t{} step {step} turns {shown:?}",
                    self.ticks
                ));
            }
            // The client injects every slot's turn on every carrier: the engine's turn
            // processor is no-op'd and the rollback channel never enters the engine's
            // lockstep queue, so this is the only applier of turns in a live match.
            // `CM_ROLLBACK_NO_INJECT=1` forces it off (diagnostic).
            let no_inject = std::env::var("CM_ROLLBACK_NO_INJECT").is_ok();
            let skip_inject = no_inject || !self.inject || self.live_steps < self.skip_start;
            self.live_steps += 1;
            if skip_inject && self.live_steps <= 8 {
                log(&format!(
                    "[cosmo-rollback] live: step {step} keeps the engine's own turn (start turn {}/{})",
                    self.live_steps, self.skip_start
                ));
            }
            for (slot, bytes) in turns.iter().enumerate() {
                let Some(bytes) = bytes else {
                    continue;
                };
                if skip_inject {
                    continue;
                }
                // Both slots are injected: the engine's own turn processor is no-op'd, so
                // nothing else applies them. `CM_ROLLBACK_SKIP_LOCAL=1` omits our own slot
                // (diagnostic).
                if slot == self.session.local_slot()
                    && std::env::var("CM_ROLLBACK_SKIP_LOCAL").is_ok()
                {
                    continue;
                }
                crate::turn_hook::inject_turn(slot as u32, bytes, false);
            }
            if trace {
                log(&format!("[cosmo-rollback] live: t{} step {step} -> engine frame", self.ticks));
            }
            r = unsafe { orig(ecx) };
            if trace {
                log(&format!("[cosmo-rollback] live: t{} step {step} <- engine frame ret {r}", self.ticks));
            }
            self.steps += 1;

            // The outer main loop runs `receive_network_messages` (`0x00485F70`) *between*
            // frames, and it mutates simulation state (its command dispatcher processes the
            // next turn). The live pass gets that from the outer loop itself; a re-simulated
            // step must run it too, or the two passes disagree — the same fix the shadow gate
            // needed (`docs/analysis/boundary-fix.md`). Only re-simulated frames: the final
            // step is this frame, and the outer loop will run it for real.
            let produced = step + 1;
            if produced <= current && resim_receive() {
                unsafe { crate::frame_hook::receive_network_messages() };
            }

            // Snapshot the frame just produced, on the spacing.
            if plan.snapshots(produced) {
                if trace {
                    log(&format!("[cosmo-rollback] live: t{} capture frame {produced}", self.ticks));
                }
                let snap = unsafe { Snapshot::capture(&self.ranges) };
                if trace {
                    log(&format!("[cosmo-rollback] live: t{} captured frame {produced}", self.ticks));
                }
                self.ring.retain(|(f, _)| *f != produced);
                self.ring.push((produced, snap));
                self.ring.sort_by_key(|(f, _)| *f);
                let keep = ring_depth(self.limit, self.spacing);
                while self.ring.len() > keep {
                    self.ring.remove(0);
                }
            }
        }

        // 6. Settle: our hash of the frame we just produced. Compared against the peer's the
        //    moment it arrives (`Session::peer_hash`), and sent on the ADR's cadence.
        let present = plan.present;
        if trace {
            log(&format!("[cosmo-rollback] live: t{} settle frame {present}", self.ticks));
        }
        let snap = unsafe { Snapshot::capture(self.ranges) };
        let hash = desync_hash(&snap);
        if trace {
            log(&format!("[cosmo-rollback] live: t{} settled frame {present} hash {hash:#x}", self.ticks));
        }
        if let Some(f) = self.session.settle(present, hash) {
            log(&format!(
                "[cosmo-rollback] live DESYNC at frame {f} (ours {hash:#x})"
            ));
        }
        self.session.report_hash(present, hash);
        // Report the newest *confirmed* multiple of 8 (ADR-0001 §1.1 hashes confirmed steps;
        // an unconfirmed frame may still be corrected by a rollback, so hashing it would
        // report a false desync).
        let confirmed = self.session.confirmed();
        let reportable = confirmed - (confirmed % self.hash_every);
        if reportable > self.hash_reported && reportable % self.hash_every == 0 {
            let Some(ours) = self.hash_of(reportable) else {
                return r;
            };
            self.hash_reported = reportable;
            if let Some(link) = self.link.as_mut() {
                link.send(Msg::Hash {
                    slot: self.session.local_slot() as u8,
                    frame: reportable,
                    hash: ours,
                });
            }
            if self.hashes_sent < 8 || self.hashes_sent % 16 == 0 {
                log(&format!(
                    "[cosmo-rollback] live: reported hash {ours:#x} for confirmed frame {reportable}"
                ));
            }
            // Fingerprint the compared ranges of the *reported* frame's state, so two clients'
            // logs can be diffed to find what disagreed (bounded to the first few reports).
            if self.hashes_sent < 3 {
                for (name, bytes) in snap.range_iter() {
                    if name.ends_with(".render") || !crate::state_hash::is_compared(name) {
                        continue;
                    }
                    log(&format!(
                        "[cosmo-rollback] live fp f{reportable} {name} {:016x}",
                        crate::state_hash::hash_bytes(bytes)
                    ));
                }
            }
            self.hashes_sent += 1;
            // The authority needs our own report too, or it can never reach a verdict.
            if let Some(authority) = self.authority.as_mut() {
                if let Verdict::Agreed = authority.report(
                    reportable,
                    self.session.local_slot(),
                    ours,
                ) {
                    self.agreements += 1;
                    if self.agreements < 6 || self.agreements % 16 == 0 {
                        log(&format!(
                            "[cosmo-rollback] live: hashes agree at frame {reportable} ({} agreed)",
                            self.agreements
                        ));
                    }
                }
            }
        }

        // 7. The host adjudicates the reports it has: evict the minority, or void the game on
        //    an even split (a 1v1 disagreement).
        if let Some(authority) = self.authority.as_mut() {
            for (slot, frame, peer_hash) in self.session.take_peer_hash_reports() {
                match authority.report(frame, slot, peer_hash) {
                    Verdict::Evict(slot) => {
                        log(&format!(
                            "[cosmo-rollback] live: evicting slot {slot} (hash divergence at frame {frame})"
                        ));
                        // Stop waiting on the slot from the frame it diverged at.
                        self.session.slot_left(slot, 0, frame);
                    }
                    Verdict::Void => {
                        log(&format!(
                            "[cosmo-rollback] live: voiding the game (no hash majority at frame {frame})"
                        ));
                        // Farm triage: log our own per-range fingerprint at the voided
                        // frame so it can be diffed against the peer's `live desync`
                        // lines (the guest logs the same block from its side).
                        let snap = unsafe { Snapshot::capture(self.ranges) };
                        for (name, bytes) in snap.range_iter() {
                            if name.ends_with(".render") || !crate::state_hash::is_compared(name) {
                                continue;
                            }
                            log(&format!(
                                "[cosmo-rollback] live desync f{frame} range {name} {:016x}",
                                crate::state_hash::hash_bytes(bytes)
                            ));
                        }
                    }
                    Verdict::Agreed => {
                        self.agreements += 1;
                        if self.agreements < 6 || self.agreements % 16 == 0 {
                            log(&format!(
                                "[cosmo-rollback] live: hashes agree at frame {frame} ({} agreed)",
                                self.agreements
                            ));
                        }
                    }
                    Verdict::Pending => {}
                }
            }
        }

        // Bounded progress trace: ticks vs the frame the simulation is on.
        if self.ticks % 60 == 0 {
            log(&format!(
                "[cosmo-rollback] live: tick {} frame {present} confirmed {} steps {} rolls {} stalls {}",
                self.ticks,
                self.session.confirmed(),
                self.steps,
                self.rolls,
                self.stalls
            ));
        }

        // 8. Cadence: acknowledge the peer's turns and re-send anything unacknowledged.
        self.since_cadence += 1;
        if self.since_cadence >= 2 {
            self.since_cadence = 0;
            if let Some(link) = self.link.as_mut() {
                self.session.ack_peers(link);
                self.session.resend_unacked(link);
            }
        }

        r
    }
}
