//! The client's rollback session: the state machine a frame hook drives.
//!
//! Ties together [`crate::turns::InputTable`] (what turns we have and what we predicted),
//! [`crate::tick::TickPlan`] (what this tick must simulate), and the desync comparison
//! ([`crate::state_hash::desync_hash`]). It is deliberately engine-independent: it owns no
//! game pointers, takes turn bytes as opaque blobs, and can be tested on its own. The
//! engine-facing half — reading the local turn out of the engine and injecting turns back
//! — lives in `crate::rollback`.
//!
//! The loop it implements, per simulation frame:
//!
//! 1. `plan(current)` decides whether this tick rolls back (the input table has seen a
//!    turn that contradicts a prediction) and where it ends.
//! 2. The caller restores the snapshot for `plan.restore_frame(current)` if it rolls back,
//!    then runs `plan.steps(current)`, injecting the turn `table.dispatch(frame)` returns
//!    for each frame.
//! 3. `settle(frame, hash)` records how far the simulation is now certain of, and compares
//!    our desync hash against peers' reports.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use crate::tick::TickPlan;
use crate::turns::{InputCounts, InputTable, SLOTS};

/// Why a session refused to advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// A slot's turns are missing and the prediction limit is reached.
    PredictionLimit,
    /// Still in the lockstep start, waiting for every turn.
    LockstepStart,
}

/// A client's rollback session.
pub struct Session {
    table: InputTable,
    /// Which slots' turns each step dispatches. v1 is 1v1, so the local slot plus one
    /// remote; a slot that has left clears its flag.
    required: [bool; SLOTS],
    local_slot: usize,
    spacing: u32,
    /// The frame the simulation is on.
    current: u32,
    /// The newest frame no later tick will simulate differently.
    confirmed: u32,
    /// Peers' latest desync hashes, and ours at the frame they name.
    peer_hash: [Option<(u32, u64)>; SLOTS],
    /// The first frame at which a peer's hash disagreed with ours, if any.
    desync_at: Option<u32>,
    /// Set while the caller is replaying frames it has already simulated.
    resimulating: bool,
    /// The rollback target of the tick currently in flight, if any, so
    /// [`settle`](Self::settle) knows which misprediction the tick answered.
    pending_rollback: Option<u32>,
    /// The authority's most recent lead report, if any (host only; a client uses it to pace).
    authority_lead: Option<(u32, u32, u32)>,
    /// The authority's most recent per-slot turn counts, if any.
    authority_turns: Option<[u32; crate::transport::SLOTS]>,
    /// The flip frame the host announced in the arming handshake, if any.
    arm_frame: Option<u32>,
    /// The announced session fast-forward (0 while the host has not filled it in).
    arm_delta: Option<u32>,
    /// The earliest snapshot frame the ring holds (the flip fast-forward); rollback
    /// restore targets are clamped to it.
    earliest_snapshot: u32,
    /// Our own desync hash for each recently settled frame, so a peer's hash can be compared
    /// the moment it *arrives* — it may well arrive after we settled that frame.
    own_hashes: std::collections::VecDeque<(u32, u64)>,
    /// Peer hash reports, in arrival order, for the host's `Authority` to adjudicate.
    peer_reports: Vec<(usize, u32, u64)>,
    /// Local turns we have sent and not yet had acknowledged, so they can be re-sent (the
    /// channel is plain UDP — see `transport::Msg::Ack`).
    sent: std::collections::VecDeque<(u32, Vec<u8>)>,
    /// The highest local turn sequence number the peer has acknowledged contiguously, or
    /// `None` while it has acknowledged nothing (which is different from acknowledging 0).
    peer_acked: Option<u32>,
}

/// How many of our sent turns are kept for re-sending.
const SENT_WINDOW: usize = 64;

/// How many of our own settled frame hashes are kept for late-arriving peer hashes.
const OWN_HASH_WINDOW: usize = 256;

impl Session {
    /// `limit` is how many steps past the newest fully-known one a step may run on
    /// predictions; `lockstep_steps` how many steps at the start wait for every turn;
    /// `held` the artificial per-slot arrival delay (testing only).
    pub fn new(
        local_slot: usize,
        remote: &[usize],
        limit: u32,
        lockstep_steps: u32,
        spacing: u32,
        held: [Duration; SLOTS],
    ) -> Self {
        let mut required = [false; SLOTS];
        if local_slot < SLOTS {
            required[local_slot] = true;
        }
        for &r in remote {
            if r < SLOTS {
                required[r] = true;
            }
        }
        Self {
            table: InputTable::new(limit, limit, lockstep_steps, held),
            required,
            local_slot,
            spacing: spacing.max(1),
            current: 0,
            confirmed: 0,
            peer_hash: [None; SLOTS],
            desync_at: None,
            resimulating: false,
            pending_rollback: None,
            authority_lead: None,
            authority_turns: None,
            arm_frame: None,
            arm_delta: None,
            earliest_snapshot: 0,
            own_hashes: std::collections::VecDeque::new(),
            peer_reports: Vec::new(),
            sent: std::collections::VecDeque::new(),
            peer_acked: None,
        }
    }

    /// The local player's turn for the next step it will be dispatched by, from the
    /// engine's own command queue.
    pub fn push_local(&mut self, bytes: Vec<u8>, now: Instant) {
        self.table.push(self.local_slot, bytes, now);
    }

    /// A remote slot's turn, from the transport.
    pub fn push_remote(&mut self, slot: usize, bytes: Vec<u8>, now: Instant) {
        self.table.push(slot, bytes, now);
    }

    /// A remote slot's turn, with the sequence number the sender gave it.
    ///
    /// The wire is lossy and unordered (CrownLink custom packets are fire-and-forget), so a
    /// turn must be placed by its sequence number, not by arrival order: see
    /// [`InputTable::push_seq`]. Use this from the transport; [`push_remote`](Self::push_remote)
    /// is the in-order special case for tests and the scripted harness.
    pub fn push_remote_seq(&mut self, slot: usize, seq: u32, bytes: Vec<u8>, now: Instant) {
        self.table.push_seq(slot, seq, bytes, now);
    }

    /// The local player's turn, with the sequence number this client gave it.
    pub fn push_local_seq(&mut self, seq: u32, bytes: Vec<u8>, now: Instant) {
        self.table.push_seq(self.local_slot, seq, bytes, now);
    }

    /// Takes every message the link has for us: turns go into the input table **by sequence
    /// number** (the wire is lossy and unordered), peer hashes into the desync comparison,
    /// and leaves into the leave schedule. The engine-facing caller runs this once per
    /// frame, before [`plan`](Self::plan). Returns how many messages were absorbed.
    pub fn absorb(&mut self, link: &mut impl crate::transport::Link, now: Instant) -> usize {
        use crate::transport::Msg;
        let mut taken = 0usize;
        static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        while let Some(msg) = link.recv() {
            taken += 1;
            if let Msg::Turn { slot, frame, bytes } = &msg {
                if std::env::var("CM_ROLLBACK_LOG_ABSORB").is_ok() {
                    crate::log::log(&format!(
                        "[cosmo-rollback] absorb turn #{taken}: slot {slot} frame {frame} len {}",
                        bytes.len()
                    ));
                }
            }
            match msg {
                Msg::Turn { slot, frame, bytes } => {
                    self.push_remote_seq(slot as usize, frame, bytes, now)
                }
                Msg::Hash { slot, frame, hash } => {
                    self.peer_hash(slot as usize, frame, hash);
                }
                Msg::Leave { slot, reason, step } => self.slot_left(slot as usize, reason, step),
                Msg::Ack { slot, seq } => {
                    // The peer acknowledges *our* turns.
                    if slot as usize == self.local_slot {
                        self.peer_acked = Some(self.peer_acked.map_or(seq, |a| a.max(seq)));
                    }
                }
                Msg::Arm { frame, delta } => {
                    // The arming handshake: the host announces the flip frame and the
                    // session fast-forward. A later announcement supersedes an earlier
                    // one (the host re-sends until the flip, with a possibly updated
                    // frame and the delta filled in at the flip).
                    self.arm_frame = Some(self.arm_frame.map_or(frame, |f| f.max(frame)));
                    if delta != 0 {
                        self.arm_delta = Some(delta);
                    }
                }
                Msg::LeadReport {
                    median_lateness,
                    p90_lateness,
                    clock_stopped_ms,
                } => self.authority_lead = Some((median_lateness, p90_lateness, clock_stopped_ms)),
                Msg::TurnsComplete { counts } => self.authority_turns = Some(counts),
            }
        }
        taken
    }

    /// The authority's most recent `(median_lateness, p90_lateness, clock_stopped_ms)`.
    pub fn authority_lead(&self) -> Option<(u32, u32, u32)> {
        self.authority_lead
    }

    /// The authority's most recent per-slot turn counts.
    pub fn authority_turns(&self) -> Option<[u32; crate::transport::SLOTS]> {
        self.authority_turns
    }

    /// The flip frame the host announced in the arming handshake, if any.
    pub fn arm_frame(&self) -> Option<u32> {
        self.arm_frame
    }

    /// The session fast-forward the host announced alongside the flip frame.
    pub fn arm_delta(&self) -> Option<u32> {
        self.arm_delta
    }

    /// Whether any turn has been received from `slot` (the arming handshake's "the peer
    /// is running the plugin" signal — keyed on traffic, not on the contiguous frontier,
    /// so a lost turn 0 cannot misclassify an armed peer as absent).
    pub fn peer_has_spoken(&self, slot: usize) -> bool {
        self.table.slot_has_any(slot)
    }

    /// `slot`'s recent turn lateness as `(median, p90)` steps, for the authority's
    /// `LeadReport`.
    pub fn lateness(&self, slot: usize) -> Option<(u32, u32)> {
        self.table.lateness_median_p90(slot)
    }

    /// Sends the local player's turn for `seq` to the peers and remembers it, so it can be
    /// re-sent if the acknowledgement does not arrive (the channel is plain UDP; ADR-0006).
    pub fn send_local_turn(
        &mut self,
        link: &mut impl crate::transport::Link,
        seq: u32,
        bytes: Vec<u8>,
        now: Instant,
    ) {
        self.push_local_seq(seq, bytes.clone(), now);
        self.sent.push_back((seq, bytes.clone()));
        while self.sent.len() > SENT_WINDOW {
            self.sent.pop_front();
        }
        link.send(crate::transport::Msg::Turn {
            slot: self.local_slot as u8,
            frame: seq,
            bytes,
        });
    }

    /// Re-sends every local turn the peer has not acknowledged. The caller runs this on a
    /// cadence; without it a turn lost in transit diverges the clients permanently.
    pub fn resend_unacked(&mut self, link: &mut impl crate::transport::Link) {
        for (seq, bytes) in &self.sent {
            if self.peer_acked.map_or(true, |acked| *seq > acked) {
                link.send(crate::transport::Msg::Turn {
                    slot: self.local_slot as u8,
                    frame: *seq,
                    bytes: bytes.clone(),
                });
            }
        }
    }

    /// Acknowledges the newest contiguous turn we hold for each remote slot, so the peer can
    /// stop re-sending it.
    pub fn ack_peers(&mut self, link: &mut impl crate::transport::Link) {
        for slot in 0..SLOTS {
            if slot == self.local_slot || !self.required[slot] {
                continue;
            }
            let known = self.table.known_until_slot(slot);
            if known > 0 {
                link.send(crate::transport::Msg::Ack {
                    slot: slot as u8,
                    seq: known - 1,
                });
            }
        }
    }

    /// Makes turns whose hold has expired usable, and notes mispredictions.
    pub fn release_held(&mut self, now: Instant) {
        self.table.release_held(now);
    }

    /// Whether the next frame may run now, or the session must stall waiting for turns.
    pub fn can_run(&self) -> Result<(), Stall> {
        if self.table.in_lockstep_start() {
            if !self.table.can_run(self.current, &self.required) {
                return Err(Stall::LockstepStart);
            }
            return Ok(());
        }
        match self.table.can_run(self.current, &self.required) {
            true => Ok(()),
            false => Err(Stall::PredictionLimit),
        }
    }

    /// The slots holding the session up, for logging a stall.
    pub fn holding_back(&self) -> Vec<usize> {
        self.table.holding_back(&self.required).collect()
    }

    /// Plans this tick: roll back if a prediction was contradicted, else step forward.
    ///
    /// The misprediction mark is *not* cleared here or when the simulation merely passes
    /// the frame: it is cleared in [`settle`](Self::settle), once the tick that rolled
    /// back to it has finished re-simulating. Clearing it earlier loses the rollback.
    pub fn plan(&mut self) -> TickPlan {
        // The frame the tick ends on: one step on, unless the prediction limit forces a
        // stall (in which case the caller stops after the rollback).
        let present = self.current + 1;
        let plan = TickPlan::new(
            self.current,
            present,
            self.confirmed,
            self.table.mispredicted(),
            self.spacing,
            self.earliest_snapshot,
        );
        self.resimulating = plan.restores(self.current);
        self.pending_rollback = plan.rollback_to;
        plan
    }

    /// The input table's misprediction mark (the earliest step a late turn contradicted).
    pub fn table_mispredicted(&self) -> Option<u32> {
        self.table.mispredicted()
    }

    /// The turn `frame` dispatches for every slot, to be injected before stepping it.
    /// Call for each frame of `plan.steps()`; the returned `predicted` count is how many
    /// slots' turns were stand-ins.
    pub fn dispatch(&mut self, frame: u32) -> ([Option<Vec<u8>>; SLOTS], u32) {
        self.table.dispatch(frame, &self.required)
    }

    /// Records that the simulation reached `frame`, with `hash` our desync hash of it.
    /// Returns `Some(frame)` the first time a peer's hash for that frame disagreed.
    pub fn settle(&mut self, frame: u32, hash: u64) -> Option<u32> {
        self.current = self.current.max(frame);
        // A rollback that reached past the frame it was scheduled for has answered the
        // misprediction, so the mark can go.
        if let Some(p) = self.pending_rollback.take() {
            if frame > p {
                self.table.clear_mispredicted();
            } else {
                self.pending_rollback = Some(p);
            }
        }
        // Frames whose turns are all known can no longer change.
        let known = self.table.known_until_for(&self.required);
        self.confirmed = self
            .confirmed
            .max(known.min(self.current))
            .min(self.current);
        self.resimulating = false;
        for slot in 0..SLOTS {
            if let Some((peer_frame, peer_hash)) = self.peer_hash[slot] {
                if peer_frame == frame && peer_hash != hash {
                    self.desync_at = Some(self.desync_at.map_or(frame, |f| f.min(frame)));
                    return self.desync_at;
                }
            }
        }
        None
    }

    /// Records **our own** desync hash for a confirmed frame, so a peer's report for it can be
    /// compared whenever it arrives.
    ///
    /// The caller must only pass frames that are [`confirmed`](Self::confirmed): a frame still
    /// running on a prediction may change when the prediction is corrected, and hashing it
    /// would report a false desync (ADR-0001 §1.1 hashes confirmed steps).
    pub fn report_hash(&mut self, frame: u32, hash: u64) {
        // A frame can be produced more than once (a rollback re-simulates it), so replace any
        // earlier hash for it: keeping the stale one would compare the peer's corrected hash
        // against our pre-rollback prediction and report a desync that is not one.
        if let Some(slot) = self.own_hashes.iter_mut().find(|(f, _)| *f == frame) {
            slot.1 = hash;
            return;
        }
        self.own_hashes.push_back((frame, hash));
        while self.own_hashes.len() > OWN_HASH_WINDOW {
            self.own_hashes.pop_front();
        }
    }

    /// Records a peer's reported desync hash for `frame`.
    ///
    /// If we have already hashed that frame ourselves, the comparison happens here — a
    /// peer's hash normally arrives *after* we settled the frame, so waiting until the next
    /// `settle` would never compare it. Otherwise it is stored for `settle` to check.
    pub fn peer_hash(&mut self, slot: usize, frame: u32, hash: u64) -> Option<u32> {
        if slot >= SLOTS {
            return None;
        }
        self.peer_hash[slot] = Some((frame, hash));
        // Also queue it for the host's authority (see `take_peer_hash_reports`).
        self.peer_reports.push((slot, frame, hash));
        // Only judge a frame we are certain of: a peer can be ahead of us, and a frame we are
        // still predicting legitimately differs.
        if frame > self.confirmed {
            return None;
        }
        if let Some((_, ours)) = self.own_hashes.iter().find(|(f, _)| *f == frame) {
            if *ours != hash {
                self.desync_at = Some(self.desync_at.map_or(frame, |f| f.min(frame)));
                return self.desync_at;
            }
        }
        None
    }

    /// A slot left; its turns stop being waited on.
    pub fn slot_left(&mut self, slot: usize, reason: u32, step: u32) {
        if slot < SLOTS {
            self.table.schedule_leave(slot, reason, step);
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    pub fn confirmed(&self) -> u32 {
        self.confirmed
    }

    pub fn desync_at(&self) -> Option<u32> {
        self.desync_at
    }

    pub fn resimulating(&self) -> bool {
        self.resimulating
    }

    pub fn spacing(&self) -> u32 {
        self.spacing
    }

    /// Whether the frame just settled should have a snapshot taken.
    pub fn snapshots(&self, frame: u32) -> bool {
        frame % self.spacing == 0
    }

    pub fn take_counts(&mut self) -> InputCounts {
        self.table.take_counts()
    }

    /// Our own recorded desync hash for `frame`, if we produced it.
    pub fn hash_of(&self, frame: u32) -> Option<u64> {
        self.own_hashes
            .iter()
            .find(|(f, _)| *f == frame)
            .map(|(_, h)| *h)
    }

    /// The local player's slot.
    pub fn local_slot(&self) -> usize {
        self.local_slot
    }

    /// Fast-forwards the session past steps the engine already ran itself while the
    /// client was waiting for the peer's first rollback turn (the arming handshake's
    /// pending phase): `steps` is the engine's per-network-step turn count. Turns for
    /// those steps were applied by the engine, so the session's input table drops them
    /// and the confirmed frontier starts at `steps`.
    pub fn advance_to(&mut self, steps: u32) {
        self.table.advance_to(steps);
        self.current = self.current.max(steps);
        self.confirmed = self.confirmed.max(steps).min(self.current);
        self.earliest_snapshot = steps;
    }

    /// Drains the peer hash reports that arrived since the last call, as `(slot, frame, hash)`
    /// — the host's [`Authority`] adjudicates them.
    pub fn take_peer_hash_reports(&mut self) -> Vec<(usize, u32, u64)> {
        std::mem::take(&mut self.peer_reports)
    }

    /// Serialize the session for a mid-match save (see `savefile`).
    ///
    /// Saves everything a resumed match needs to continue from the saved frame: the
    /// input table (turn windows, frontier, mispredictions, leaves), the sim position
    /// (`current`/`confirmed`), the desync comparison state (peer hashes, own hashes,
    /// pending reports), the in-flight un-acked local turns and the authority's latest
    /// lead/turn counts.
    pub fn save_state(&self) -> Vec<u8> {
        use crate::savefile::Writer;
        let mut w = Writer::new();
        for &req in &self.required {
            w.flag(req);
        }
        w.u32(self.local_slot as u32);
        w.u32(self.spacing);
        w.u32(self.current);
        w.u32(self.confirmed);
        for &p in &self.peer_hash {
            w.flag(p.is_some());
            if let Some((f, h)) = p {
                w.u32(f);
                w.u64(h);
            }
        }
        w.opt_u32(self.desync_at);
        w.flag(self.resimulating);
        w.opt_u32(self.pending_rollback);
        w.flag(self.authority_lead.is_some());
        if let Some((m, p, c)) = self.authority_lead {
            w.u32(m);
            w.u32(p);
            w.u32(c);
        }
        w.flag(self.authority_turns.is_some());
        if let Some(counts) = self.authority_turns {
            for c in counts {
                w.u32(c);
            }
        }
        w.u32(self.own_hashes.len() as u32);
        for (f, h) in &self.own_hashes {
            w.u32(*f);
            w.u64(*h);
        }
        w.u32(self.peer_reports.len() as u32);
        for (slot, frame, hash) in &self.peer_reports {
            w.u32(*slot as u32);
            w.u32(*frame);
            w.u64(*hash);
        }
        w.u32(self.sent.len() as u32);
        for (seq, bytes) in &self.sent {
            w.u32(*seq);
            w.bytes(bytes);
        }
        w.opt_u32(self.peer_acked);
        w.bytes(&self.table.save_state());
        w.into_bytes()
    }

    /// Restore the session from [`save_state`](Self::save_state) bytes. The loader is
    /// bounds-checked end to end; a corrupt save fails with an `Err` instead of
    /// silently changing the session.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        use crate::savefile::Reader;
        let mut r = Reader::new(bytes);
        for req in self.required.iter_mut() {
            *req = r.flag()?;
        }
        self.local_slot = r.u32()? as usize;
        self.spacing = r.u32()?.max(1);
        self.current = r.u32()?;
        self.confirmed = r.u32()?;
        for p in self.peer_hash.iter_mut() {
            *p = if r.flag()? {
                Some((r.u32()?, r.u64()?))
            } else {
                None
            };
        }
        self.desync_at = r.opt_u32()?;
        self.resimulating = r.flag()?;
        self.pending_rollback = r.opt_u32()?;
        self.authority_lead = if r.flag()? {
            Some((r.u32()?, r.u32()?, r.u32()?))
        } else {
            None
        };
        self.authority_turns = if r.flag()? {
            let mut counts = [0u32; crate::transport::SLOTS];
            for c in counts.iter_mut() {
                *c = r.u32()?;
            }
            Some(counts)
        } else {
            None
        };
        self.own_hashes.clear();
        let nh = r.u32()? as usize;
        if nh > OWN_HASH_WINDOW {
            return Err("own hash window out of range");
        }
        for _ in 0..nh {
            self.own_hashes.push_back((r.u32()?, r.u64()?));
        }
        self.peer_reports.clear();
        let nr = r.u32()? as usize;
        if nr > 256 {
            return Err("peer report count out of range");
        }
        for _ in 0..nr {
            self.peer_reports
                .push((r.u32()? as usize, r.u32()?, r.u64()?));
        }
        self.sent.clear();
        let ns = r.u32()? as usize;
        if ns > SENT_WINDOW {
            return Err("sent window out of range");
        }
        for _ in 0..ns {
            let seq = r.u32()?;
            let bytes = r.bytes()?.to_vec();
            self.sent.push_back((seq, bytes));
        }
        self.peer_acked = r.opt_u32()?;
        let table = r.bytes()?;
        self.table.load_state(table)?;
        r.finish()
    }
}

/// What the authority decided about one step's desync hashes (ADR-0001 §1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Fewer reports than the step requires.
    Pending,
    /// Every reporting slot agreed.
    Agreed,
    /// `slot` disagreed with the majority; it should be evicted.
    Evict(usize),
    /// The reports split with no majority (a 1v1 disagreement): the game is void.
    Void,
}

/// The host's side of hash adjudication (ADR-0001 §1.1).
///
/// The host is a player *and* the authority: it collects each slot's desync hash for a step
/// and judges it once every required slot has reported — evicting a diverging slot, or
/// voiding the game when the split has no majority. A slot that keeps sending turns without
/// ever reporting is treated as diverging by the caller (the ADR's withholding rule); this
/// type only adjudicates the reports it is given.
pub struct Authority {
    /// How many slots must report before a step can be judged.
    required: usize,
    /// Reports per step, kept until judged and then dropped.
    steps: std::collections::BTreeMap<u32, StepReports>,
}

struct StepReports {
    /// The first hash seen for the step: the majority candidate.
    hash: u64,
    /// The slot that gave it.
    first_slot: usize,
    agreed: Vec<usize>,
    disagreed: Vec<usize>,
}

impl Authority {
    /// `required` is how many slots must report for a step (the slots still in the game).
    pub fn new(required: usize) -> Self {
        Self {
            required: required.max(1),
            steps: std::collections::BTreeMap::new(),
        }
    }

    /// Records `slot`'s hash for `step` and returns the verdict once enough reports are in.
    /// A second report from the same slot is ignored.
    pub fn report(&mut self, step: u32, slot: usize, hash: u64) -> Verdict {
        let entry = self.steps.entry(step).or_insert_with(|| StepReports {
            hash,
            first_slot: slot,
            agreed: Vec::new(),
            disagreed: Vec::new(),
        });
        if entry.agreed.contains(&slot) || entry.disagreed.contains(&slot) {
            return Verdict::Pending;
        }
        if entry.hash == hash {
            entry.agreed.push(slot);
        } else {
            entry.disagreed.push(slot);
        }
        if entry.agreed.len() + entry.disagreed.len() < self.required {
            return Verdict::Pending;
        }
        let verdict = if entry.disagreed.is_empty() {
            Verdict::Agreed
        } else {
            match entry.disagreed.len().cmp(&entry.agreed.len()) {
                std::cmp::Ordering::Less => Verdict::Evict(entry.disagreed[0]),
                std::cmp::Ordering::Greater => Verdict::Evict(entry.first_slot),
                std::cmp::Ordering::Equal => Verdict::Void,
            }
        };
        self.steps.remove(&step);
        verdict
    }

    /// Steps still awaiting reports.
    pub fn pending(&self) -> usize {
        self.steps.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(limit: u32) -> Session {
        Session::new(0, &[1], limit, 0, 3, [Duration::ZERO; SLOTS])
    }

    #[test]
    fn with_both_turns_a_frame_runs_with_no_prediction() {
        let mut s = session(2);
        let now = Instant::now();
        s.push_local(vec![1], now);
        s.push_remote(1, vec![2], now);
        assert_eq!(s.can_run(), Ok(()));
        let plan = s.plan();
        assert!(!plan.restores(0));
        let (turns, predicted) = s.dispatch(0);
        assert_eq!(predicted, 0);
        assert_eq!(turns[0].as_deref(), Some(&[1u8][..]));
        assert_eq!(turns[1].as_deref(), Some(&[2u8][..]));
        assert_eq!(s.settle(1, 0xabc), None);
        assert_eq!(s.current(), 1);
        assert_eq!(s.confirmed(), 1);
    }

    #[test]
    fn a_missing_remote_turn_is_predicted_then_rolls_back_when_it_arrives() {
        let mut s = session(2);
        let now = Instant::now();
        s.push_local(vec![1], now);
        // Frame 0 runs with slot 1 predicted.
        assert_eq!(s.can_run(), Ok(()));
        let (_, predicted) = s.dispatch(0);
        assert_eq!(predicted, 1);
        s.settle(1, 0);
        assert_eq!(s.confirmed(), 0, "a predicted frame is not confirmed");
        // The real turn arrives with a command: the next tick must roll back to frame 0.
        s.push_remote(1, vec![9], now);
        let plan = s.plan();
        assert_eq!(plan.rollback_to, Some(0));
        assert_eq!(plan.restore_frame(s.current()), Some(0));
        // Re-simulating frame 0's step (producing frame 1) settles it.
        let (turns, predicted) = s.dispatch(0);
        assert_eq!(predicted, 0);
        assert_eq!(turns[1].as_deref(), Some(&[9u8][..]));
        assert_eq!(s.settle(1, 0), None);
        assert_eq!(s.confirmed(), 1, "the real turn confirms the frame");
    }

    #[test]
    fn the_prediction_limit_stalls_instead_of_running_forever() {
        let mut s = session(1);
        let now = Instant::now();
        s.push_local(vec![1], now);
        // Slot 1 has sent nothing, so `known_until_for` is 0 and limit 1 allows only
        // step 0 (the step that produces frame 1).
        assert_eq!(s.can_run(), Ok(()));
        let (_, predicted) = s.dispatch(0);
        assert_eq!(predicted, 1);
        s.settle(1, 0);
        assert_eq!(s.can_run(), Err(Stall::PredictionLimit));
        assert_eq!(s.holding_back(), vec![1]);
    }

    #[test]
    fn a_peer_hash_mismatch_is_reported_once_at_the_earliest_frame() {
        let mut s = session(2);
        let now = Instant::now();
        s.push_local(vec![1], now);
        s.push_remote(1, vec![2], now);
        s.dispatch(0);
        s.peer_hash(1, 1, 0xdead);
        assert_eq!(s.settle(1, 0xbeef), Some(1));
        assert_eq!(s.desync_at(), Some(1));
        // A later mismatch does not move the first report.
        s.push_local(vec![1], now);
        s.push_remote(1, vec![2], now);
        s.dispatch(1);
        s.peer_hash(1, 2, 0x1234);
        assert_eq!(s.settle(2, 0x5678), Some(1));
    }

    #[test]
    fn a_matching_peer_hash_is_not_a_desync() {
        let mut s = session(2);
        let now = Instant::now();
        s.push_local(vec![1], now);
        s.push_remote(1, vec![2], now);
        s.dispatch(0);
        s.peer_hash(1, 1, 0xaaaa);
        assert_eq!(s.settle(1, 0xaaaa), None);
        assert_eq!(s.desync_at(), None);
    }

    // ---- two-client convergence over a delayed link ------------------------------
    //
    // The strongest check of the rollback logic that does not need the game: two clients
    // whose turns reach each other `delay` frames late, so each predicts and then rolls
    // back. A toy "engine" makes the state a pure function of the turn sets applied per
    // frame, so identical turn sets must give identical state — if the rollback logic is
    // right, the two clients converge frame for frame, and a wrong one shows up as a
    // state mismatch or a reported desync.

    /// A toy engine state: mix every applied turn of one frame.
    fn apply(state: u64, turns: &[Option<Vec<u8>>; SLOTS]) -> u64 {
        let mut h = state;
        for t in turns.iter().flatten() {
            for &b in t {
                h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h
    }

    /// The turn slot `slot` issues for the step that produces frame `frame + 1`.
    fn turn(frame: u32, slot: usize) -> Vec<u8> {
        vec![frame as u8, (frame >> 8) as u8, slot as u8]
    }

    struct Client {
        s: Session,
        slot: usize,
        /// states[i] is the state after i frames.
        states: Vec<u64>,
        /// The first frame whose local turn has not been issued yet.
        issued: u32,
        /// `sigs[i]` is the turn set applied to produce frame `i + 1`, as raw bytes, so a
        /// divergence can be attributed to the turns rather than only the state.
        sigs: Vec<Vec<u8>>,
    }

    impl Client {
        fn new(slot: usize, remote: usize) -> Self {
            Self {
                s: Session::new(slot, &[remote], 4, 0, 3, [Duration::ZERO; SLOTS]),
                slot,
                states: vec![0xcbf2_9ce4_8422_2325],
                issued: 0,
                sigs: Vec::new(),
            }
        }

        /// Runs one tick and returns the turns it issued this tick, as
        /// `(frame, bytes)` pairs for the link to deliver late.
        ///
        /// Arrivals are handed to the input table **before** the tick is planned. That
        /// order is the whole point: a turn that arrives for a frame the simulation has
        /// already run is what makes the table report a misprediction, and only then can
        /// the plan roll back to it. Delivering arrivals after planning would mean the
        /// rollback could never be scheduled.
        fn tick(
            &mut self,
            now: Instant,
            arrivals: &mut Vec<(u32, Vec<u8>)>,
        ) -> Vec<(u32, Vec<u8>)> {
            let mut i = 0;
            while i < arrivals.len() {
                if arrivals[i].0 <= self.s.current() {
                    let (_, bytes) = arrivals.remove(i);
                    self.s.push_remote(1 - self.slot, bytes, now);
                } else {
                    i += 1;
                }
            }
            let current = self.s.current();
            let plan = self.s.plan();
            if let Some(rf) = plan.restore_frame(current) {
                self.states.truncate(rf as usize + 1);
                self.sigs.truncate(rf as usize + 1);
            }
            let mut issued = Vec::new();
            for f in plan.start(current)..plan.present {
                if f >= self.issued {
                    let t = turn(f, self.slot);
                    self.s.push_local(t.clone(), now);
                    issued.push((f, t));
                    self.issued = f + 1;
                }
                let (turns, _) = self.s.dispatch(f);
                let h = apply(self.states[f as usize], &turns);
                let sig: Vec<u8> = turns
                    .iter()
                    .flatten()
                    .flat_map(|t| t.iter().copied())
                    .collect();
                let at = f as usize + 1;
                if self.states.len() > at {
                    self.states.truncate(at);
                    self.sigs.truncate(at);
                }
                self.states.push(h);
                self.sigs.push(sig);
            }
            self.s.settle(plan.present, self.states[plan.present as usize]);
            issued
        }
    }

    #[test]
    fn two_clients_converge_through_mispredictions_and_rollbacks() {
        const DELAY: u32 = 3;
        let now = Instant::now();
        let mut a = Client::new(0, 1);
        let mut b = Client::new(1, 0);
        // (arrival frame, bytes) queues in each direction.
        let mut to_a: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut to_b: Vec<(u32, Vec<u8>)> = Vec::new();

        for _ in 0..20 {
            for (f, t) in a.tick(now, &mut to_a) {
                to_b.push((f + DELAY, t));
            }
            for (f, t) in b.tick(now, &mut to_b) {
                to_a.push((f + DELAY, t));
            }
        }

        // Nothing that has arrived for a frame already simulated may still be queued:
        // that would be a prediction the other client never got to correct. Turns whose
        // arrival frame is beyond the last simulated frame are legitimately still in
        // flight.
        // A turn for a frame the client has already moved past must have been handed to
        // its input table (which is what forces the rollback). A turn for the frame it is
        // on now, or a later one, is legitimately still in flight.
        assert!(
            to_a.iter().all(|(f, _)| *f >= a.s.current())
                && to_b.iter().all(|(f, _)| *f >= b.s.current()),
            "a turn for a simulated frame is still queued"
        );

        // Both clients must have reached the same frame with the same state, and neither
        // may have reported a desync (they compared no hashes here, but a rollback that
        // re-simulated wrongly would show as a state mismatch).
        assert_eq!(a.s.current(), b.s.current(), "clients disagree on the frame");
        assert_eq!(a.s.desync_at(), None);
        assert_eq!(b.s.desync_at(), None);
        // A frame is only settled once every slot's turn for it is in; frames still
        // running on a prediction may legitimately differ, which is the whole point of
        // predicting. So compare up to the frame both clients are certain of.
        let settled = a.s.confirmed().min(b.s.confirmed());
        assert_eq!(
            a.s.confirmed(),
            b.s.confirmed(),
            "clients are certain of different frames"
        );
        assert!(
            settled > DELAY,
            "only {settled} frames settled; the test did not exercise a rollback"
        );
        for i in 1..=settled as usize {
            assert_eq!(
                a.states[i], b.states[i],
                "state differs at settled frame {i} (a={}, b={})\n  a applied: {:02x?}\n  b applied: {:02x?}",
                a.states[i], b.states[i], a.sigs.get(i), b.sigs.get(i)
            );
        }
        // And both must have actually mispredicted at some point, or the test proved
        // nothing. `mispredicted_turns` counts predicted turns a later turn contradicted,
        // which is exactly what forced each rollback above.
        let ca = a.s.take_counts();
        let cb = b.s.take_counts();
        assert!(
            ca.mispredicted_turns > 0 || cb.mispredicted_turns > 0,
            "no prediction was contradicted; the test did not exercise a rollback (a={ca:?}, b={cb:?})"
        );
    }

    // ---- the same convergence, over the real wire format --------------------------
    //
    // The test above drives two `Session`s directly with ad-hoc arrival queues. This one
    // puts the same logic on the `transport` layer: turns are `Msg::Turn` datagrams that
    // are encoded, carried by a `Loopback` link with a fixed delay, decoded, and pushed
    // into the session. It proves the wire format and the session state machine agree end
    // to end — a decoder bug, a version mismatch, or a wrong field order would surface
    // here as a state mismatch or a reported desync.

    #[test]
    fn two_clients_converge_over_the_wire_format() {
        use crate::transport::{Link, Loopback, Msg};

        const DELAY: u32 = 3;
        let now = Instant::now();
        let mut a = Client::new(0, 1);
        let mut b = Client::new(1, 0);
        // One link per direction; each delivers exactly DELAY frames late.
        let mut a_to_b = Loopback::new(DELAY);
        let mut b_to_a = Loopback::new(DELAY);
        // Turns received but not yet delivered to the session (their frame has not
        // arrived yet), kept across ticks exactly as a real client would buffer them.
        let mut in_a: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut in_b: Vec<(u32, Vec<u8>)> = Vec::new();

        for _ in 0..24 {
            a_to_b.tick();
            b_to_a.tick();
            // Drain the links into each client's arrival queue.
            while let Some(m) = b_to_a.recv() {
                if let Msg::Turn { frame, bytes, .. } = m {
                    in_a.push((frame, bytes));
                }
            }
            while let Some(m) = a_to_b.recv() {
                if let Msg::Turn { frame, bytes, .. } = m {
                    in_b.push((frame, bytes));
                }
            }
            // Run one tick, then send everything it issued over the wire.
            for (frame, bytes) in a.tick(now, &mut in_a) {
                a_to_b.send(Msg::Turn { slot: 0, frame, bytes });
            }
            for (frame, bytes) in b.tick(now, &mut in_b) {
                b_to_a.send(Msg::Turn { slot: 1, frame, bytes });
            }
        }

        // Both clients must have reached the same frame with the same settled state.
        assert_eq!(a.s.current(), b.s.current(), "clients disagree on the frame");
        assert_eq!(a.s.desync_at(), None);
        assert_eq!(b.s.desync_at(), None);
        assert_eq!(
            a.s.confirmed(),
            b.s.confirmed(),
            "clients are certain of different frames"
        );
        let settled = a.s.confirmed().min(b.s.confirmed());
        assert!(
            settled > DELAY,
            "only {settled} frames settled; the test did not exercise a rollback"
        );
        for i in 1..=settled as usize {
            assert_eq!(
                a.states[i], b.states[i],
                "state differs at settled frame {i} (a={}, b={})",
                a.states[i], b.states[i]
            );
        }
        // The wire path must actually have been used and decoded, not silently dropped.
        // A turn still buffered may only be one for a frame the client has not reached
        // yet: a turn for a frame it has already simulated would be a prediction that
        // never got corrected.
        assert!(
            in_a.iter().all(|(f, _)| *f > a.s.current())
                && in_b.iter().all(|(f, _)| *f > b.s.current()),
            "a turn for an already-simulated frame is still undelivered"
        );
    }

    #[test]
    fn absorb_places_link_turns_by_sequence_number() {
        use crate::transport::{Link, Loopback, Msg};
        let mut s = Session::new(0, &[1], 4, 0, 3, [Duration::ZERO; SLOTS]);
        let now = Instant::now();
        s.push_local_seq(0, vec![0xa0], now);
        let mut link = Loopback::new(0);
        // The peer's turn 1 arrives before its turn 0 (the wire is unordered).
        link.send(Msg::Turn {
            slot: 1,
            frame: 1,
            bytes: vec![0xb1],
        });
        link.send(Msg::Turn {
            slot: 1,
            frame: 0,
            bytes: vec![0xa1],
        });
        link.send(Msg::Hash {
            slot: 1,
            frame: 0,
            hash: 0xdead,
        });
        link.tick();
        s.absorb(&mut link, now);
        let (turns, predicted) = s.dispatch(0);
        assert_eq!(predicted, 0);
        assert_eq!(turns[1].as_deref(), Some(&[0xa1u8][..]));
        let (turns, _) = s.dispatch(1);
        assert_eq!(turns[1].as_deref(), Some(&[0xb1u8][..]));
    }

    #[test]
    fn two_clients_exchange_hashes_over_the_wire() {
        use crate::transport::{Link, Loopback, Msg};
        const DELAY: u32 = 3;
        let now = Instant::now();
        let mut a = Client::new(0, 1);
        let mut b = Client::new(1, 0);
        let mut a_to_b = Loopback::new(DELAY);
        let mut b_to_a = Loopback::new(DELAY);
        let mut in_a: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut in_b: Vec<(u32, Vec<u8>)> = Vec::new();
        // The newest frame each client has reported a hash for.
        let mut last_a = 0u32;
        let mut last_b = 0u32;

        for _ in 0..24 {
            a_to_b.tick();
            b_to_a.tick();
            while let Some(m) = b_to_a.recv() {
                match m {
                    Msg::Turn { frame, bytes, .. } => in_a.push((frame, bytes)),
                    Msg::Hash { slot, frame, hash } => {
                        a.s.peer_hash(slot as usize, frame, hash);
                    }
                    _ => {}
                }
            }
            while let Some(m) = a_to_b.recv() {
                match m {
                    Msg::Turn { frame, bytes, .. } => in_b.push((frame, bytes)),
                    Msg::Hash { slot, frame, hash } => {
                        b.s.peer_hash(slot as usize, frame, hash);
                    }
                    _ => {}
                }
            }
            for (frame, bytes) in a.tick(now, &mut in_a) {
                a_to_b.send(Msg::Turn {
                    slot: 0,
                    frame,
                    bytes,
                });
            }
            for (frame, bytes) in b.tick(now, &mut in_b) {
                b_to_a.send(Msg::Turn {
                    slot: 1,
                    frame,
                    bytes,
                });
            }
            // Each reports the hash of the newest frame it is *certain* of. A frame still
            // running on a prediction legitimately differs between clients, so hashing it
            // would report a false desync (ADR-0001 §1.1: hash confirmed steps only).
            let fa = a.s.confirmed();
            if fa > last_a {
                last_a = fa;
                a.s.report_hash(fa, a.states[fa as usize]);
                a_to_b.send(Msg::Hash {
                    slot: 0,
                    frame: fa,
                    hash: a.states[fa as usize],
                });
            }
            let fb = b.s.confirmed();
            if fb > last_b {
                last_b = fb;
                b.s.report_hash(fb, b.states[fb as usize]);
                b_to_a.send(Msg::Hash {
                    slot: 1,
                    frame: fb,
                    hash: b.states[fb as usize],
                });
            }
        }

        // A hash that arrives after the frame was settled must still be compared — that is
        // the normal case, and it is what `peer_hash` now does.
        assert_eq!(a.s.desync_at(), None, "client a reported a desync");
        assert_eq!(b.s.desync_at(), None, "client b reported a desync");

        // The authority agrees on a frame both clients are certain of.
        let f = a.s.confirmed().min(b.s.confirmed());
        assert!(f > DELAY);
        let mut auth = Authority::new(2);
        assert_eq!(auth.report(f, 0, a.states[f as usize]), Verdict::Pending);
        assert_eq!(auth.report(f, 1, b.states[f as usize]), Verdict::Agreed);
    }

    // ---- the authority (host) side ------------------------------------------------

    #[test]
    fn the_authority_agrees_when_every_slot_reports_the_same_hash() {
        let mut a = Authority::new(2);
        assert_eq!(a.report(8, 0, 0xaa), Verdict::Pending);
        assert_eq!(a.report(8, 1, 0xaa), Verdict::Agreed);
        assert_eq!(a.pending(), 0);
    }

    #[test]
    fn the_authority_evicts_the_slot_that_disagrees_with_the_majority() {
        let mut a = Authority::new(3);
        assert_eq!(a.report(8, 0, 0xaa), Verdict::Pending);
        assert_eq!(a.report(8, 1, 0xaa), Verdict::Pending);
        assert_eq!(a.report(8, 2, 0xbb), Verdict::Evict(2));
    }

    #[test]
    fn the_authority_evicts_the_first_reporter_when_the_majority_arrives_after_it() {
        let mut a = Authority::new(3);
        assert_eq!(a.report(8, 0, 0xaa), Verdict::Pending);
        assert_eq!(a.report(8, 1, 0xbb), Verdict::Pending);
        assert_eq!(a.report(8, 2, 0xbb), Verdict::Evict(0));
    }

    #[test]
    fn a_split_with_no_majority_voids_the_game() {
        // A 1v1 disagreement: the two hashes split evenly, so there is no majority.
        let mut a = Authority::new(2);
        assert_eq!(a.report(8, 0, 0xaa), Verdict::Pending);
        assert_eq!(a.report(8, 1, 0xbb), Verdict::Void);
    }

    #[test]
    fn a_duplicate_report_from_a_slot_is_ignored() {
        let mut a = Authority::new(2);
        a.report(8, 0, 0xaa);
        assert_eq!(a.report(8, 0, 0xbb), Verdict::Pending);
        assert_eq!(a.report(8, 1, 0xaa), Verdict::Agreed);
    }

    // ---- reliability over a plain-UDP channel ---------------------------------------

    #[test]
    fn both_clients_hold_every_turn_after_a_lossy_exchange() {
        use crate::transport::{Link, Msg};

        #[derive(Default)]
        struct Queue {
            out: Vec<Msg>,
            inbox: std::collections::VecDeque<Msg>,
        }
        impl Link for Queue {
            fn send(&mut self, m: Msg) {
                self.out.push(m);
            }
            fn recv(&mut self) -> Option<Msg> {
                self.inbox.pop_front()
            }
        }

        const STEPS: u32 = 40;
        let now = Instant::now();
        let mut a = Session::new(0, &[1], 8, 0, 3, [Duration::ZERO; SLOTS]);
        let mut b = Session::new(1, &[0], 8, 0, 3, [Duration::ZERO; SLOTS]);
        let mut ab = Queue::default();
        let mut ba = Queue::default();
        let mut n = 0u32;

        for step in 0..STEPS {
            a.send_local_turn(&mut ab, step, vec![0xa0, step as u8], now);
            b.send_local_turn(&mut ba, step, vec![0xb0, step as u8], now);
            // Deliver, dropping every third datagram — a 33% loss rate, far above anything a
            // real link shows.
            for msg in ab.out.drain(..) {
                n += 1;
                if n % 3 != 2 {
                    ba.inbox.push_back(msg);
                }
            }
            for msg in ba.out.drain(..) {
                n += 1;
                if n % 3 != 2 {
                    ab.inbox.push_back(msg);
                }
            }
            a.absorb(&mut ab, now);
            b.absorb(&mut ba, now);
            a.ack_peers(&mut ab);
            b.ack_peers(&mut ba);
            a.resend_unacked(&mut ab);
            b.resend_unacked(&mut ba);
        }

        // Let the in-flight re-sends settle (the last turns had no following iteration).
        for _ in 0..6 {
            for msg in ab.out.drain(..) {
                n += 1;
                if n % 3 != 2 {
                    ba.inbox.push_back(msg);
                }
            }
            for msg in ba.out.drain(..) {
                n += 1;
                if n % 3 != 2 {
                    ab.inbox.push_back(msg);
                }
            }
            a.absorb(&mut ab, now);
            b.absorb(&mut ba, now);
            a.ack_peers(&mut ab);
            b.ack_peers(&mut ba);
            a.resend_unacked(&mut ab);
            b.resend_unacked(&mut ba);
        }

        // Despite the loss, both must end up holding every turn the other sent, in order —
        // this is what the ack/resend is for (prediction alone cannot recover a lost turn).
        assert_eq!(a.table.known_until_slot(1), STEPS, "a is missing turns");
        assert_eq!(b.table.known_until_slot(0), STEPS, "b is missing turns");
        for step in [0u32, 1, STEPS / 2, STEPS - 1] {
            let (turns, _) = a.dispatch(step);
            assert_eq!(
                turns[1].as_deref(),
                Some(&[0xb0u8, step as u8][..]),
                "a has the wrong turn for step {step}"
            );
            let (turns, _) = b.dispatch(step);
            assert_eq!(
                turns[0].as_deref(),
                Some(&[0xa0u8, step as u8][..]),
                "b has the wrong turn for step {step}"
            );
        }
    }

    #[test]
    fn a_lost_turn_is_recovered_by_the_ack_and_resend() {
        use crate::transport::{Link, Msg};

        /// A one-directional link the test drives by hand, so it can drop datagrams.
        #[derive(Default)]
        struct Queue {
            out: Vec<Msg>,
            inbox: std::collections::VecDeque<Msg>,
        }
        impl Link for Queue {
            fn send(&mut self, msg: Msg) {
                self.out.push(msg);
            }
            fn recv(&mut self) -> Option<Msg> {
                self.inbox.pop_front()
            }
        }

        let now = Instant::now();
        let mut a = Session::new(0, &[1], 4, 0, 3, [Duration::ZERO; SLOTS]);
        let mut b = Session::new(1, &[0], 4, 0, 3, [Duration::ZERO; SLOTS]);
        let mut ab = Queue::default();
        let mut ba = Queue::default();

        // a sends its turn 0 and the datagram is lost.
        a.send_local_turn(&mut ab, 0, vec![7], now);
        assert_eq!(ab.out.len(), 1);
        ab.out.clear();

        // No acknowledgement, so the turn is re-sent; this time it arrives.
        a.resend_unacked(&mut ab);
        assert_eq!(ab.out.len(), 1, "the unacked turn must be re-sent");
        for msg in ab.out.drain(..) {
            ba.inbox.push_back(msg);
        }
        b.absorb(&mut ba, now);
        let (turns, _) = b.dispatch(0);
        assert_eq!(
            turns[0].as_deref(),
            Some(&[7u8][..]),
            "the re-sent turn arrived in time"
        );

        // b acknowledges, and a stops re-sending.
        b.ack_peers(&mut ba);
        let ack: Vec<Msg> = ba.out.drain(..).collect();
        assert!(
            matches!(ack.as_slice(), [Msg::Ack { slot: 0, seq: 0 }]),
            "b must acknowledge a's turn 0, got {ack:?}"
        );
        for msg in ack {
            ab.inbox.push_back(msg);
        }
        a.absorb(&mut ab, now);
        a.resend_unacked(&mut ab);
        assert!(
            ab.out.is_empty(),
            "an acknowledged turn must not be re-sent: {:?}",
            ab.out
        );
    }

    // ---- save/load round trip (M1) -------------------------------------------------

    #[test]
    fn session_state_round_trips_through_save() {
        use crate::transport::Loopback;
        let now = Instant::now();
        let mut s = Session::new(0, &[1], 8, 2, 3, [Duration::ZERO; SLOTS]);
        // Exercise every saveable facet: turns in the table (with a gap in slot 1's
        // window), a settled frame, our own hash, an agreeing peer hash, an un-acked
        // sent turn, and a scheduled leave.
        s.push_local_seq(0, vec![0xa0], now);
        s.push_remote_seq(1, 0, vec![0xb0], now);
        s.push_local_seq(1, vec![0xa1], now);
        // seq 2 for slot 1 has not arrived yet: a gap in the window.
        s.push_local_seq(2, vec![0xa2], now);
        let mut link = Loopback::new(0);
        s.send_local_turn(&mut link, 2, vec![0xa2], now);
        s.dispatch(0);
        s.settle(1, 0xaa);
        s.report_hash(1, 0xaa);
        s.peer_hash(1, 1, 0xaa); // agrees
        s.slot_left(1, 1, 3);

        let bytes = s.save_state();
        let mut t = Session::new(0, &[1], 8, 2, 3, [Duration::ZERO; SLOTS]);
        t.load_state(&bytes).expect("session load");

        assert_eq!(t.local_slot(), 0);
        assert_eq!(t.spacing(), 3);
        assert_eq!(t.current(), s.current());
        assert_eq!(t.confirmed(), s.confirmed());
        assert_eq!(t.desync_at(), None, "an agreeing peer hash is not a desync");
        assert_eq!(t.hash_of(1), Some(0xaa));
        // The restored session dispatches the same turns the original does.
        for step in [0u32, 1, 2] {
            let (a, pa) = s.dispatch(step);
            let (b, pb) = t.dispatch(step);
            assert_eq!(a, b, "step {step} dispatches differently after load");
            assert_eq!(pa, pb, "step {step} prediction counts differ after load");
        }
        // Both were mutated identically, so re-saving both must give identical bytes —
        // this covers the leaves, the sent queue, the hashes and the table in one check.
        assert_eq!(
            s.save_state(),
            t.save_state(),
            "re-saved state differs after an identical load+dispatch"
        );
    }

    #[test]
    fn session_load_rejects_a_corrupt_save() {
        let now = Instant::now();
        let mut s = Session::new(0, &[1], 8, 0, 3, [Duration::ZERO; SLOTS]);
        s.push_local(vec![1], now);
        let mut bytes = s.save_state();
        bytes.truncate(bytes.len() - 1);
        let mut t = Session::new(0, &[1], 8, 0, 3, [Duration::ZERO; SLOTS]);
        assert!(t.load_state(&bytes).is_err());
    }
}
