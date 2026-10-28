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
//! ## The arming handshake
//!
//! A match where only one side is armed must be indistinguishable from stock lockstep:
//! suppressing the engine's turn path before the peer is confirmed would zero the
//! outgoing buffer and no-op the turn processor on a live lockstep stream — the two
//! sims would diverge in the first seconds. The client therefore runs a **pending
//! phase** from takeover: the engine's own turn path runs the match untouched while
//! the channel carries the early traffic. The host, once it has seen the guest's
//! traffic, announces a flip frame (`Msg::Arm`, re-sent until the flip); both clients
//! flip at that exact frame, fast-forward the session past the steps the engine
//! already ran itself (derived from the engine's own turn counter — simulation state,
//! so identical on both sides), and only then suppress the engine path and drive. A
//! peer that never speaks (not armed, or no rollback-capable CrownLink) keeps the
//! match in pure lockstep forever — the suppression never engages, so nothing can
//! diverge.
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
use crate::state_hash::{desync_hash, state_hash};
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

/// Set while the live client is **driving** a match (both sides confirmed armed and the
/// host's announced flip frame has arrived). While it is clear, the engine's own turn
/// path runs the match untouched — pure lockstep: the lobby phase, the arming pending
/// phase, and any handoff after a desync or a peer loss. The suppression hooks (the
/// turn-processor no-op, the outgoing-buffer zeroing, the forced-ready scheduler) all
/// gate on this.
static DRIVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the live client is driving the match (see [`DRIVING`]).
pub fn driving() -> bool {
    DRIVING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Arm (or disarm) the engine-path suppression.
pub(crate) fn set_driving(on: bool) {
    DRIVING.store(on, std::sync::atomic::Ordering::Relaxed);
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
            // Only while the client is *driving*. In the lobby and during the arming
            // pending phase the engine's own turn processing carries the lobby state,
            // the map distribution and the start turns; no-oping it there leaves the
            // joiner with an empty slot list and the match never starts. After a
            // desync/peer-loss handoff the engine applies its own turns again.
            if !in_game() || !driving() {
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
    /// Set when the match has an AI player; live driving is refused (the flip never
    /// happens and the match stays pure lockstep).
    ai_present: bool,
    /// When the client took over (for the "peer not armed" notice).
    started_at: Option<Instant>,
    /// Set when the client has handed the match back to the engine (a desync, or a peer
    /// that stopped responding) — it never drives again for this match.
    no_peer: bool,
    /// The engine's per-network-step turn counter at takeover, so the driving phase can
    /// fast-forward the session past the steps the engine already ran itself.
    takeover_turn: u32,
    /// The sim frame at which the client starts driving (the host's `Arm` announcement,
    /// or the host's own choice). `None` while pending.
    arm_frame: Option<u32>,
    /// The announced session fast-forward (the host computes it at its flip).
    arm_delta: Option<u32>,
    /// The last sequence number sent over the channel while pending. The pending turn
    /// must be sent **once per sequence**, not once per content change: the engine's
    /// no-op turn ([0x05]) is identical every step, and content-dedup left the peer's
    /// table with gaps at the flip — the guest's pre-flip turns for the gap steps never
    /// arrived, the host stalled at the prediction limit forever, and the match froze.
    pending_last_seq: u32,
    /// Whether any pending turn has been sent at all, and when the last one went out
    /// (drives the pending keepalive re-send).
    pending_sent_any: bool,
    pending_last_at: Instant,
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
    /// The session's construction parameters, kept so a new match can rebuild the
    /// session from scratch (frame numbers, the input table and the confirmed frontier
    /// must not leak into a rematch).
    local_slot: usize,
    remote_slot: usize,
    session_limit: u32,
    /// Wall time the current run of stalls started, for the peer-loss handoff.
    stall_since: Option<Instant>,
    /// The last time any channel message arrived (the peer-loss handoff requires a
    /// stall run with *no* traffic — stalls with live traffic are normal pacing).
    last_traffic: Option<Instant>,
    /// The newest confirmed frame whose hash we have sent.
    hash_reported: u32,
    /// Report a desync hash every this many confirmed frames (ADR-0001 §1.1 says 8; a smaller
    /// value converges a test run faster).
    hash_every: u32,
    /// The frame whose desync fingerprint has been logged, so it is logged once.
    desync_logged: Option<u32>,
    /// Autosave cadence (M1): write a full save every this many **confirmed** frames
    /// (`CM_ROLLBACK_AUTOSAVE_EVERY`, default 1440 ≈ 1 minute at Fastest). Only when
    /// the client is driving a real match.
    autosave_every: Option<u32>,
    /// How many autosave files to keep (`CM_ROLLBACK_AUTOSAVE_KEEP`, default 4).
    autosave_keep: usize,
    /// Re-read and compare every save written (`CM_ROLLBACK_SAVE_VERIFY=1`).
    save_verify: bool,
    /// Directory saves are written to (`CM_ROLLBACK_SAVE_DIR`, default `cosmo_saves`).
    save_dir: String,
}

/// `g_is_in_game` (`BW/bwdata.h`): 0 in the menus, 1 once the match is running.
pub(crate) const G_IS_IN_GAME: usize = 0x006D_5BC8;

/// How long a run of consecutive stalls may last (with a peer that once spoke) before
/// the match is handed back to the engine's own turn path. A lost peer would otherwise
/// stall the simulation forever.
const PEER_LOSS_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the pending phase re-sends its last turn as a keepalive, so a slow guest
/// keeps the host's peer-loss grace from expiring while the guest is still in the
/// lockstep-pending phase (see `pending_last_at`).
const PENDING_KEEPALIVE: Duration = Duration::from_secs(2);

/// `Game.frame_count` (`bwdata.h:218`): the simulation frame, identical on both clients.
const FRAME_COUNT: usize = 0x0057_F23C;

/// `0x0057EEBC` — the engine's per-network-step turn counter (snapshotted as
/// `probe_57eebc`; verified to step once per network step by the reproducibility probe).
/// Simulation state, so identical on both clients at the same sim frame — the arming
/// handshake derives its session fast-forward from it.
const TURN_COUNTER: usize = 0x0057_EEBC;

/// How many frames ahead of the announcement the host places the flip, so the `Arm`
/// message reaches the guest before its sim passes the flip frame (covers the channel
/// round-trip). `CM_ROLLBACK_ARM_MARGIN` overrides.
fn arm_margin() -> u32 {
    std::env::var("CM_ROLLBACK_ARM_MARGIN")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(8)
}

/// The current simulation frame (both clients see the same value at the same state).
fn frame_count() -> u32 {
    unsafe { *(FRAME_COUNT as *const u32) }
}

/// The engine's per-network-step turn counter (simulation state).
fn turn_count() -> u32 {
    unsafe { *(TURN_COUNTER as *const u32) }
}

/// Strip the engine's transport-level sync markers (`GameCommand_Hash`, 0x37, id + 6
/// payload bytes) from the **front** of a turn. The engine seeds the hash command into
/// the buffer before the player's commands are appended (the capture evidence shows
/// `[37, hash…6B][commands…]`), so a turn may legitimately carry commands after the
/// marker — those are simulation input and must be kept. Only a turn with nothing left
/// after stripping is replaced with the empty-turn marker (0x05).
fn strip_sync_markers(mut turn: Vec<u8>) -> Vec<u8> {
    while turn.len() >= 7 && turn[0] == 0x37 {
        turn.drain(..7);
    }
    if turn.is_empty() {
        turn.push(crate::turns::SYNC_NOP);
    }
    turn
}

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
        // The engine applied every pre-takeover step itself; the driving phase derives
        // its start from the engine's own turn counter (see `begin_driving`), so the old
        // fixed skip is no longer needed. `CM_ROLLBACK_SKIP_START_TURNS` remains as an
        // explicit extra diagnostic skip.
        let skip_start = std::env::var("CM_ROLLBACK_SKIP_START_TURNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
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
            local_slot,
            remote_slot: remote,
            session_limit: limit,
            stall_since: None,
            last_traffic: None,
            started_at: None,
            no_peer: false,
            takeover_turn: 0,
            arm_frame: None,
            arm_delta: None,
            pending_last_seq: 0,
            pending_sent_any: false,
            pending_last_at: Instant::now(),
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
            autosave_every: std::env::var("CM_ROLLBACK_AUTOSAVE_EVERY")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&n| n > 0),
            autosave_keep: std::env::var("CM_ROLLBACK_AUTOSAVE_KEEP")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(4),
            save_verify: std::env::var("CM_ROLLBACK_SAVE_VERIFY").is_ok(),
            save_dir: std::env::var("CM_ROLLBACK_SAVE_DIR")
                .unwrap_or_else(|_| "cosmo_saves".to_string()),
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

    /// Write the autosave for `frame` (the just-settled, confirmed state) and prune.
    fn autosave(&mut self, frame: u32) {
        let snap = unsafe { Snapshot::capture(self.ranges) };
        let snap_bytes = snap.serialize();
        let session = self.session.save_state();
        let path = std::path::PathBuf::from(format!("{}/autosave_{}.cosmo", self.save_dir, frame));
        match crate::savefile::write_save(
            &path,
            frame,
            crate::savefile::FLAG_LIVE,
            &snap_bytes,
            &session,
        ) {
            Ok(n) => {
                log(&format!(
                    "[cosmo-rollback] autosave f{frame}: wrote {} ({} bytes; snapshot {}, session {})",
                    path.display(),
                    n,
                    snap_bytes.len(),
                    session.len()
                ));
                if self.save_verify {
                    self.verify_autosave(&path, frame, &snap);
                }
                crate::savefile::prune_autosaves(&self.save_dir, self.autosave_keep);
            }
            Err(e) => log(&format!("[cosmo-rollback] autosave f{frame}: write failed: {e}")),
        }
    }

    /// Read an autosave back and compare its snapshot hash against the in-memory
    /// snapshot it was written from, proving the file round-trip is lossless.
    fn verify_autosave(&self, path: &std::path::Path, frame: u32, want: &Snapshot) {
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
        log(&format!("[cosmo-rollback] saveverify f{frame}: {verdict}"));
    }

    /// Reset everything that belongs to one match, called when the game leaves the match
    /// (the `g_is_in_game` falling edge). A rematch runs in the same process, and the
    /// session's frame numbers, input table and confirmed frontier, the snapshot ring
    /// (whose frames restart at 0), the authority's step reports, the `live_steps`
    /// skip-start counter and the per-match verdicts must all start over.
    fn reset_match(&mut self) {
        self.session = Session::new(
            self.local_slot,
            &[self.remote_slot],
            self.session_limit,
            self.lockstep_steps,
            self.spacing,
            [Duration::ZERO; SLOTS],
        );
        self.authority = (self.local_slot == 0).then(|| Authority::new(2));
        self.ring.clear();
        self.started = false;
        self.refused_logged = false;
        self.no_peer = false;
        self.ai_present = false;
        self.started_at = None;
        self.stall_since = None;
        self.last_traffic = None;
        self.takeover_turn = 0;
        self.arm_frame = None;
        self.arm_delta = None;
        self.pending_last_seq = 0;
        self.pending_sent_any = false;
        self.pending_last_at = Instant::now();
        self.live_steps = 0;
        self.hash_reported = 0;
        self.desync_logged = None;
        self.script_issued = 0;
        self.ticks = 0;
        set_driving(false);
    }

    /// One game frame. `orig` is the engine's frame function through the trampoline; it is
    /// called once per simulated step (more than once when a rollback re-simulates).
    pub fn tick(&mut self, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) -> u32 {
        // Do not drive the simulation from the menu: the two clients must start from the same
        // state, and the only state they share is the start of the game (a real match starts
        // from the map init; this harness starts both clients from the same replay's frame 0).
        // Until `g_is_in_game` is set, pass the frame straight through.
        if unsafe { *(G_IS_IN_GAME as *const u32) } == 0 {
            // A new match starts fresh: the session (frame numbers, input table, confirmed
            // frontier), the snapshot ring, the per-match counters and every verdict
            // (fallback/AI/peer) must not carry into a rematch — stale ring frames would
            // restore a previous match's state, and a stale `live_steps` counter would
            // re-inject the new match's start turns.
            if self.started {
                self.reset_match();
            }
            return unsafe { orig(ecx) };
        }
        // A real match resolves its CrownLink channel now (the .snp was loaded by the
        // lobby's provider bind) — retried every tick until it succeeds.
        self.ensure_link();
        if !self.started {
            self.started = true;
            self.started_at = Some(Instant::now());
            self.takeover_turn = unsafe { *(TURN_COUNTER as *const u32) };
            // `g_sync_turn_enabled` (0x006556E0) gates the engine's turn/replay
            // injector. If a plugin zeroed it, the engine's un-no-op'd 0x486530 path
            // would process stored turns with timing-dependent counts — a divergence
            // source the snapshot cannot see. Refuse loudly rather than drive on it.
            let sync_on = unsafe { *(0x0065_56E0 as *const u32) };
            if sync_on == 0 {
                log(
                    "[cosmo-rollback] live: g_sync_turn_enabled is 0 (a plugin suppressed \
                     the engine's sync path); the match stays in normal lockstep",
                );
                self.no_peer = true;
                return unsafe { orig(ecx) };
            }
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

        // The arming pending phase: the engine's own turn path runs the match untouched
        // (pure lockstep) until both sides have confirmed and the host's announced flip
        // frame arrives. Nothing is suppressed before the flip, so a match where either
        // side is not running the plugin is indistinguishable from stock lockstep — no
        // divergence, ever.
        if !driving() {
            return self.pending_tick(ecx, orig);
        }
        self.drive(ecx, orig)
    }

    /// The arming pending phase (see [`tick`](Self::tick)): run the engine's own turn
    /// path, exchange the early channel traffic, and flip to driving at the host's
    /// announced frame. Returns the engine's frame result, or the driving body's result
    /// on the tick that flips.
    fn pending_tick(&mut self, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) -> u32 {
        let now = Instant::now();
        // Absorb the peer's early traffic: turns mark the peer as armed, hashes queue
        // up, and the host's `Arm` announcement names the flip frame.
        if let Some(link) = self.link.as_mut() {
            self.session.absorb(link, now);
        }
        // The scripted harness peer is present by construction: drive immediately.
        if self.scripted {
            if !self.no_peer && !self.ai_present {
                self.begin_driving(0);
            } else {
                return unsafe { orig(ecx) };
            }
        } else {
            // Send the local turn so the peer can see we are armed. (The engine's own
            // lockstep stream also carries it; this channel copy is what the handshake
            // keys on, and the flip's `advance_to` drops these pre-flip turns.) The
            // turn is sent once per sequence, plus a periodic keepalive re-send of the
            // last sequence: a slow guest can stay pending for tens of seconds, and
            // without the keepalive the host's peer-loss grace sees "no traffic" and
            // hands the match back to lockstep before the guest ever flips.
            let local = strip_sync_markers(crate::turn_hook::local_turn());
            let seq = self.session.current();
            let keepalive = self.pending_sent_any
                && seq == self.pending_last_seq
                && self.pending_last_at.elapsed() >= PENDING_KEEPALIVE;
            if !self.pending_sent_any || seq > self.pending_last_seq || keepalive {
                self.pending_sent_any = true;
                self.pending_last_seq = seq;
                self.pending_last_at = now;
                if let Some(link) = self.link.as_mut() {
                    self.session.send_local_turn(link, seq, local, now);
                }
            }
            if self.no_peer || self.ai_present {
                return unsafe { orig(ecx) };
            }
            if self.session.local_slot() == 0 {
                // The host announces the flip once it has seen the guest, and re-sends
                // the announcement every tick until the flip (the channel is plain UDP).
                if self.session.peer_has_spoken(self.remote_slot) && self.arm_frame.is_none() {
                    let k = frame_count().wrapping_add(arm_margin());
                    self.arm_frame = Some(k);
                }
                if let (Some(k), Some(link)) = (self.arm_frame, self.link.as_mut()) {
                    link.send(crate::transport::Msg::Arm {
                        frame: k,
                        delta: self.arm_delta.unwrap_or(0),
                    });
                }
            } else if let Some(k) = self.session.arm_frame() {
                // The guest honors the host's announcement (a later one supersedes).
                self.arm_frame = Some(k);
            }
            if let Some(k) = self.arm_frame {
                if frame_count() >= k {
                    // The flip. The session fast-forward is the number of engine steps
                    // the lockstep already ran: the two clients' takeover detections can
                    // differ by a few network steps (the replay load completes at
                    // slightly different frames), so the HOST's delta is the single
                    // source of truth — it computes it here, sends it once more, and the
                    // guest uses the announced value. A guest that somehow flipped
                    // without a delta (a lost final Arm) falls back to its own count
                    // rather than stall forever.
                    let delta = if self.session.local_slot() == 0 {
                        let d = turn_count().wrapping_sub(self.takeover_turn);
                        self.arm_delta = Some(d);
                        if let Some(link) = self.link.as_mut() {
                            link.send(crate::transport::Msg::Arm { frame: k, delta: d });
                        }
                        d
                    } else {
                        self.session
                            .arm_delta()
                            .unwrap_or_else(|| turn_count().wrapping_sub(self.takeover_turn))
                    };
                    // A computer player cannot be rolled back: refuse to flip (the match
                    // stays pure lockstep). CM_ROLLBACK_ALLOW_AI overrides.
                    if computer_player_present()
                        && std::env::var("CM_ROLLBACK_ALLOW_AI").is_err()
                    {
                        self.ai_present = true;
                        self.arm_frame = None;
                        log(
                            "[cosmo-rollback] live: computer player present — the match \
                             stays in normal lockstep (no rollback)",
                        );
                        return unsafe { orig(ecx) };
                    }
                    self.begin_driving(delta);
                }
            } else if self
                .started_at
                .map(|t| t.elapsed() >= Duration::from_secs(3))
                .unwrap_or(false)
                && !self.refused_logged
            {
                // The peer never spoke the channel: it is not running the plugin. The
                // match continues as plain lockstep — nothing was ever suppressed.
                self.refused_logged = true;
                log(
                    "[cosmo-rollback] live: no rollback turn from the peer within 3 s — the \
                     other player is not running the plugin (or has no rollback-capable \
                     CrownLink.snp); playing normal lockstep",
                );
            }
        }
        if driving() {
            return self.drive(ecx, orig);
        }
        unsafe { orig(ecx) }
    }

    /// Take over driving: suppress the engine's turn path (the suppression hooks gate on
    /// [`driving`]) and fast-forward the session past the `delta` steps the engine
    /// already ran itself during the pending phase.
    fn begin_driving(&mut self, delta: u32) {
        set_driving(true);
        self.session.advance_to(delta);
        // Seed the snapshot ring with the flip frame: the first driving steps frequently
        // mispredict (the peer's early turns arrive a step or two late), and the rollback
        // target is exactly this frame. Without a snapshot here the restore fails and the
        // match hands back to lockstep on the very first correction.
        let snap = unsafe { Snapshot::capture(self.ranges) };
        self.ring.retain(|(f, _)| *f != delta);
        self.ring.push((delta, snap));
        self.ring.sort_by_key(|(f, _)| *f);
        log(&format!(
            "[cosmo-rollback] live: both sides armed — driving from step {delta} \
             (the engine ran {delta} steps itself before the flip)"
        ));
    }

    /// The driving body: one game frame of the rollback loop.
    fn drive(&mut self, ecx: u32, orig: unsafe extern "C" fn(u32) -> u32) -> u32 {
        let now = Instant::now();
        self.ticks = self.ticks.wrapping_add(1);
        let current = self.session.current();

        // Farm command streams: inject this step's scripted turns for every armed slot
        // (identical files on both clients -> identical inputs; see `farm.rs`).
        crate::farm::inject_stream_turns(self.steps, self.limit);

        // 1. Absorb the peer's messages. Turns go into the input table by sequence number.
        if let Some(link) = self.link.as_mut() {
            if self.session.absorb(link, now) > 0 {
                self.last_traffic = Some(now);
            }
        }
        // A desync the peer's hash just revealed: dump a per-range fingerprint of our own state
        // so the two clients' logs can be diffed to find the range that differs, then hand the
        // match back to the engine: its own turn path resumes and its native sync detector
        // (the 0x37 hash commands) flags the divergence with the game's own desync dialog,
        // instead of two silently-diverged sims playing on.
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
                log(&format!(
                    "[cosmo-rollback] live DESYNC at frame {f}: the simulations diverged — \
                     handing the match back to the engine's turn path (its native desync \
                     detection takes over)"
                ));
                self.no_peer = true;
                set_driving(false);
                return unsafe { orig(ecx) };
            }
        }

        // 2. The local turn for the step this frame runs: read it now, before the frame's
        //    flush clears the buffer. The engine's own send is suppressed, so this is the only
        //    way it reaches the peer.
        //    The engine seeds `GameCommand_Hash` (0x37) into the buffer *before* the player's
        //    commands, so the sync markers are stripped from the front — they are transport
        //    state, never simulation input, and `command_lengths[0x37]` holds the table's
        //    "no length" sentinel (the dispatcher special-cases the hash command).
        let local = strip_sync_markers(crate::turn_hook::local_turn());
        // CrownLink's custom packets carry at most a 500-byte payload (storm_sdk's
        // `MAX_PAYLOAD_SIZE`; the provider advertises a 512-byte storm packet) and our wire
        // framing adds 9 bytes, so a turn above ~490 bytes would be dropped by the SNP
        // layer and could never be re-sent into the channel. The engine's own lockstep
        // path has the same cap (its overflow flush fires at the provider max), so this is
        // a shared, documented limit — but if a turn ever approaches it, say so loudly
        // instead of dropping silently.
        if local.len() > 480 {
            log(&format!(
                "[cosmo-rollback] live: WARNING local turn is {} bytes — near the \
                 CrownLink 500-byte custom-packet cap (wire framing adds 9); the turn may \
                 be dropped by the SNP layer",
                local.len()
            ));
        }
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
            // A peer that once spoke and then went **silent** (mid-match disconnect, crash,
            // network drop) would stall the simulation forever: the prediction limit can
            // never absorb a turn that will not come. After a grace of continuous stalls
            // *with no channel traffic at all*, the match is handed back to the engine —
            // its own turn path resumes and its native disconnect handling shows. (The
            // scripted harness peer never goes silent, and a not-yet-seen peer is the
            // pending phase's business.) Stalls with live traffic — turns merely late
            // beyond the prediction window — are normal pacing and never hand off.
            if !self.scripted {
                let stalled_since = *self.stall_since.get_or_insert_with(Instant::now);
                let alive = self.last_traffic.is_some_and(|t| t >= stalled_since);
                if !alive && stalled_since.elapsed() >= PEER_LOSS_TIMEOUT {
                    log(
                        "[cosmo-rollback] live: no traffic from the peer for over \
                         {PEER_LOSS_TIMEOUT:?}; handing the match back to the engine's turn \
                         path (its native disconnect handling takes over)",
                    );
                    self.no_peer = true;
                    set_driving(false);
                    return unsafe { orig(ecx) };
                }
            }
            return 0;
        }
        self.stall_since = None;

        // 4. Plan, and roll back if the input table has contradicted a prediction.
        let plan = self.session.plan();
        if let Some(rf) = plan.restore_frame(current) {
            match self.ring.iter().find(|(f, _)| *f == rf) {
                Some((_, snap)) => unsafe { snap.restore() },
                None => {
                    // A rollback with no snapshot to restore from is a hard invariant
                    // violation (the ring is sized for the prediction limit): the re-sim
                    // cannot reproduce the live path. Hand the match back to the engine
                    // rather than run diverged silently.
                    log(&format!(
                        "[cosmo-rollback] live: no snapshot for restore frame {rf}; handing \
                         the match back to the engine's turn path (cannot roll back)"
                    ));
                    // Ring forensics: which frames the ring holds, the rollback target and
                    // the table's misprediction mark, so a ring-vs-target gap is diagnosed
                    // from the log alone.
                    log(&format!(
                        "[cosmo-rollback] live: ring frames {:?} rollback_to {:?} mispredicted {:?} \
                         spacing {} current {current}",
                        self.ring.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
                        plan.rollback_to,
                        self.session.table_mispredicted(),
                        plan.spacing,
                    ));
                    self.no_peer = true;
                    set_driving(false);
                    return unsafe { orig(ecx) };
                }
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

        // 6b. Autosave (M1): every `autosave_every` **confirmed** frames, write the full
        //     snapshot + session to `<save_dir>/autosave_<frame>.cosmo` and prune to the
        //     newest `autosave_keep`. A save is only taken when the frame just produced is
        //     confirmed — predicted state may still be corrected by a rollback and is not
        //     a safe resume point (see `docs/analysis/save-format.md`).
        if let Some(every) = self.autosave_every {
            if self.started && present > 0 && present % every == 0 {
                if present == self.session.confirmed() {
                    self.autosave(present);
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
