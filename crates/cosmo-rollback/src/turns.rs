//! The turn/input table: prediction and confirmation for a simulation that runs ahead
//! of the turns it has received.
//!
//! Ported from ShieldBattery's `game/src/netcode_v2/input_table.rs` (MIT), with the
//! transport and StarCraft-command specifics left to the caller: this module is pure
//! bookkeeping over opaque turn bytes, so it can be reasoned about and tested on its own.
//!
//! The contract it implements (ShieldBattery's own module docs):
//!
//! > Every simulation step dispatches exactly one turn from every slot still in the
//! > game, so a slot's k-th in-game turn is dispatched by step k (the step that starts
//! > from frame k) on every client. A client that has not received a slot's turn for a
//! > step can still run the step with a stand-in that issues no commands,
//! > [`PREDICTED_TURN`], and run it again once the real turn arrives if that turn did
//! > issue any. [`InputTable`] keeps each slot's turns for as long as a rollback may
//! > simulate their steps again, reports the earliest step whose prediction a turn has
//! > since contradicted, and schedules the relay's leaves at the step each belongs to.
//!
//! Two things are deliberately *not* here yet, because they need decisions the port has
//! not made:
//!
//! - The command bytes' internal structure (the sync command id, the per-command
//!   framing). Turns are opaque here; `matches_prediction` only has to recognise "this
//!   turn does nothing", which is `[`SYNC_NOP`]` or empty.
//! - The transport. `push` is called by whoever received a turn (CrownLink's SNP
//!   receive path, or a relay), and `dispatch` by the frame hook.
//!
//! The engine-side integration point is `crate::rollback`: each frame, before running
//! the step, dispatch the turn for that step and inject it; a rollback re-dispatches the
//! turns for the frames it simulates again.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Slots the engine's turn scheduler can have. ShieldBattery uses `MAX_STORM_PLAYERS`;
/// 1.16.1's storm player array is the same shape, and v1 is 1v1, so 8 is ample.
pub const SLOTS: usize = 8;

/// The turn a step runs with for a slot whose turn has not arrived: an empty one, which
/// is what an idle player's turn holds once native sync commands are stripped from it.
pub const PREDICTED_TURN: &[u8] = &[];

/// The sync command that means "no commands this turn". A turn holding only this does
/// exactly what [`PREDICTED_TURN`] does, so it counts as a correct prediction.
///
/// 1.16.1's no-op is `GameCommand_Sequence = 5` (`reference/cosmonarchy-gptp/BW/enumerations.h:1353`):
/// it is a 1-byte command whose only effect is `++*g_sequence` (`hooks/game_hooks.cpp:4422`),
/// and it is what the engine writes to an **empty** turn buffer (`BW/structures.h:1758`
/// defaults `Command_Sequence`; the flush at `0x00485A4E` writes `0x05`). This was
/// previously `0x0F` (ShieldBattery's SC:R `NOP`), which would have made every idle turn
/// look like a misprediction and cost one needless rollback per idle turn.
pub const SYNC_NOP: u8 = 0x05;

/// Whether a turn does exactly what [`PREDICTED_TURN`] does.
pub fn matches_prediction(turn: &[u8]) -> bool {
    turn.is_empty() || turn == [SYNC_NOP]
}

/// Every slot's turns by step. See the [module docs](self).
pub struct InputTable {
    /// How many steps a step whose turns are not all known may run past the newest one
    /// whose turns are, counting itself: 0 runs only steps with every turn known, which
    /// is lockstep.
    limit: u32,
    /// How many frames of lateness this client means to absorb by rolling back rather
    /// than with input delay, when nothing is going wrong. At most `limit`.
    rollback_target: u32,
    /// How many steps at the start of the game wait for every turn, as lockstep does.
    /// Clients start their game loops at different moments, and running the first steps
    /// in lockstep lines their simulations up before each keeps its own schedule.
    lockstep_steps: u32,
    /// The latency buffer this client acts as if the relay asked for at least.
    min_buffer_turns: u32,
    slots: [SlotTurns; SLOTS],
    /// How long each slot's turns are held back after they arrive before a step may use
    /// them, for testing a slow link with a fast one.
    held: [Duration; SLOTS],
    /// The first step no simulation has run yet.
    frontier: u32,
    /// The earliest step that ran on a prediction a turn has since contradicted.
    mispredicted: Option<u32>,
    /// Counts since the last [`take_counts`](Self::take_counts).
    counts: InputCounts,
    /// Recent per-slot turn lateness samples, for the authority's `LeadReport`.
    lateness: [VecDeque<u32>; SLOTS],
    /// One leave per slot, in the order the relay directed them.
    leaves: Vec<ScheduledLeave>,
}

/// What the table saw over a stretch of steps, for logging.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct InputCounts {
    /// Steps that ran for the first time with at least one turn predicted.
    pub predicted_steps: u32,
    /// Predicted turns a later turn contradicted.
    pub mispredicted_turns: u32,
    /// Predicted turns that turned out right.
    pub confirmed_predictions: u32,
}

#[derive(Default)]
struct SlotTurns {
    /// The step that dispatches `turns[0]`.
    first: u32,
    turns: VecDeque<HeldTurn>,
    /// How many of `turns`, from the front, steps may use: turns arrive in step order,
    /// and one held back also holds back every turn after it.
    usable: usize,
}

struct HeldTurn {
    /// The turn's bytes, or `None` while it has not arrived. A `None` entry is a *gap*: the
    /// step is predicted empty until the turn shows up (possibly late, which is a
    /// misprediction).
    bytes: Option<Vec<u8>>,
    /// When steps may start using the turn, or `None` from the moment it arrived.
    usable_at: Option<Instant>,
    /// How many steps past its own the simulation already was when the turn arrived: 0 for a
    /// turn that arrived in time, >0 for a late one. Feeds the authority's `LeadReport`.
    lateness: u32,
}

/// Lateness samples kept per slot for the `LeadReport` median/p90.
const LATENESS_WINDOW: usize = 24;

/// Largest sequence-number gap [`InputTable::push_seq`] will fill with predicted-empty turns.
/// A turn that claims to be further ahead than this is treated as bogus (a desynced or
/// hostile peer) and dropped, rather than allocating unboundedly.
pub const MAX_SEQ_GAP: u32 = 4096;

struct ScheduledLeave {
    storm: usize,
    reason: u32,
    /// The step that applies the leave instead of dispatching the slot's turn.
    step: u32,
    applied: bool,
}

impl InputTable {
    pub fn new(
        limit: u32,
        rollback_target: u32,
        lockstep_steps: u32,
        held: [Duration; SLOTS],
    ) -> Self {
        Self {
            limit,
            rollback_target: rollback_target.min(limit),
            lockstep_steps,
            min_buffer_turns: 0,
            slots: std::array::from_fn(|_| SlotTurns::default()),
            held,
            frontier: 0,
            mispredicted: None,
            counts: InputCounts::default(),
            lateness: std::array::from_fn(|_| VecDeque::new()),
            leaves: Vec::new(),
        }
    }

    /// Adds the next turn of `storm`'s slot, which the step after that slot's previous
    /// turn's dispatches. `now` is when it arrived. This is the in-order special case of
    /// [`push_seq`](Self::push_seq).
    pub fn push(&mut self, storm: usize, bytes: Vec<u8>, now: Instant) {
        let seq = self
            .slots
            .get(storm)
            .map_or(0, |s| s.first + s.turns.len() as u32);
        self.push_seq(storm, seq, bytes, now);
    }

    /// Adds the turn the sender numbered `seq` for `storm`'s slot; that turn dispatches
    /// step `seq`.
    ///
    /// Turns travel over a lossy, unordered channel, so they may arrive out of order,
    /// twice, **late** (for a step already dispatched — a misprediction) or **not at all**
    /// (the steps in between are predicted empty and corrected when the turn finally
    /// arrives). A late real turn marks the earliest contradicted step, exactly as a held
    /// turn released after its step ran does.
    pub fn push_seq(&mut self, storm: usize, seq: u32, bytes: Vec<u8>, now: Instant) {
        let frontier = self.frontier;
        let held = self.held.get(storm).copied().unwrap_or_default();
        let usable_at = (!held.is_zero()).then(|| now + held);
        let is_noop = matches_prediction(&bytes);
        let lateness = frontier.saturating_sub(seq);
        let Some(slot) = self.slots.get_mut(storm) else {
            return;
        };
        if seq < slot.first {
            return; // older than this slot's window
        }
        let index = (seq - slot.first) as usize;
        if index > MAX_SEQ_GAP as usize {
            // A turn claiming to be far in the future is bogus (or the peer is desynced);
            // do not allocate for it.
            return;
        }
        let mut late_mispredict = false;
        if index < slot.turns.len() {
            if slot.turns[index].bytes.as_deref() == Some(bytes.as_slice()) {
                return; // duplicate
            }
            // A real turn where an empty one was predicted, for a step that already ran.
            late_mispredict = index < slot.usable && seq < frontier && !is_noop;
            slot.turns[index] = HeldTurn {
                bytes: Some(bytes),
                usable_at,
                lateness,
            };
        } else {
            // A gap: every missing step is predicted empty (an idle turn).
            while slot.turns.len() < index {
                slot.turns.push_back(HeldTurn {
                    bytes: None,
                    usable_at,
                    lateness,
                });
            }
            slot.turns.push_back(HeldTurn {
                bytes: Some(bytes),
                usable_at,
                lateness,
            });
        }
        if late_mispredict {
            self.mispredicted = Some(self.mispredicted.map_or(seq, |x| x.min(seq)));
            self.counts.mispredicted_turns += 1;
        }
        // Record the lateness sample for the authority's lead report.
        if let Some(window) = self.lateness.get_mut(storm) {
            window.push_back(lateness);
            while window.len() > LATENESS_WINDOW {
                window.pop_front();
            }
        }
        self.release_held(now);
    }

    /// `storm`'s slot's recent turn lateness as `(median, p90)`, or `None` with no samples.
    /// The authority broadcasts this as its `LeadReport` (ADR-0001 §1.1).
    pub fn lateness_median_p90(&self, storm: usize) -> Option<(u32, u32)> {
        let window = self.lateness.get(storm)?;
        if window.is_empty() {
            return None;
        }
        let mut sorted: Vec<u32> = window.iter().copied().collect();
        sorted.sort_unstable();
        // Nearest-rank percentile.
        let at = |p: usize| sorted[(p * sorted.len()).div_ceil(100).max(1) - 1];
        Some((at(50), at(90)))
    }

    /// Makes turns whose hold has run out by `now` usable, noting any whose step already
    /// ran on a prediction that the turn contradicts.
    pub fn release_held(&mut self, now: Instant) {
        // Read before borrowing `slots` mutably.
        let frontier = self.frontier;
        let mispredicted = &mut self.mispredicted;
        let counts = &mut self.counts;
        for slot in &mut self.slots {
            while let Some(turn) = slot.turns.get(slot.usable) {
                if turn.usable_at.is_some_and(|x| x > now) {
                    break;
                }
                let step = slot.first + slot.usable as u32;
                // A gap (no turn yet) is a prediction that nothing confirms yet, so it is
                // not counted either way here.
                if let Some(bytes) = &turn.bytes {
                    if step < frontier {
                        if !matches_prediction(bytes) {
                            *mispredicted = Some(mispredicted.map_or(step, |x| x.min(step)));
                            counts.mispredicted_turns += 1;
                        } else {
                            counts.confirmed_predictions += 1;
                        }
                    }
                }
                slot.usable += 1;
            }
        }
    }

    /// The first step `storm`'s slot has no usable turn for, exposed so the transport can
    /// acknowledge it (the peer may stop re-sending everything before it).
    pub fn known_until_slot(&self, storm: usize) -> u32 {
        self.known_until(storm)
    }

    /// The first step `storm`'s slot has no usable turn for.
    ///
    /// A gap counts as unknown: the step would be predicted empty, and the prediction limit
    /// must bound how far past it the simulation may run.
    fn known_until(&self, storm: usize) -> u32 {
        let slot = &self.slots[storm];
        let present = slot
            .turns
            .iter()
            .take(slot.usable)
            .take_while(|t| t.bytes.is_some())
            .count();
        slot.first + present as u32
    }

    /// The first step whose turns are not all known: every step before it dispatches the
    /// turns every other client dispatches for it. `required` holds the slots whose
    /// steps still dispatch.
    pub fn known_until_for(&self, required: &[bool; SLOTS]) -> u32 {
        (0..SLOTS)
            .filter(|&storm| required[storm])
            .map(|storm| self.holds_back_from(storm))
            .min()
            .unwrap_or(u32::MAX)
    }

    /// The slots of `required` that [`known_until_for`](Self::known_until_for) waits on:
    /// the ones with no turn for the first step whose turns are not all known. A stall at
    /// the prediction limit is waiting on exactly these, while a slot that is merely a
    /// few steps behind isn't holding anything up.
    pub fn holding_back(&self, required: &[bool; SLOTS]) -> impl Iterator<Item = usize> + '_ {
        let known_until = self.known_until_for(required);
        let required = *required;
        (0..SLOTS).filter(move |&storm| {
            required[storm] && known_until != u32::MAX && self.holds_back_from(storm) == known_until
        })
    }

    /// The first step `storm`'s slot keeps from having all its turns known, or
    /// `u32::MAX` once its leave is scheduled and every turn it has before that leave is
    /// in.
    fn holds_back_from(&self, storm: usize) -> u32 {
        let known = self.known_until(storm);
        let leaves_after_known = self
            .leaves
            .iter()
            .any(|x| x.storm == storm && x.step <= known);
        match leaves_after_known {
            true => u32::MAX,
            false => known,
        }
    }

    /// Whether a step that has never run may run now, predicting whatever turns it
    /// lacks. The game's first `lockstep_steps` steps wait for every turn.
    pub fn can_run(&self, step: u32, required: &[bool; SLOTS]) -> bool {
        let limit = match step < self.lockstep_steps {
            true => 0,
            false => self.limit,
        };
        step < self.known_until_for(required).saturating_add(limit)
    }

    /// How many frames of lateness this client means to absorb by rolling back rather
    /// than with input delay.
    pub fn rollback_target(&self) -> u32 {
        self.rollback_target
    }

    /// Whether the game is still in the steps at its start that wait for every turn.
    pub fn in_lockstep_start(&self) -> bool {
        self.frontier < self.lockstep_steps
    }

    /// Makes this client act as if the relay asked for a latency buffer of at least
    /// `turns`.
    pub fn set_min_buffer_turns(&mut self, turns: u32) {
        self.min_buffer_turns = turns;
    }

    /// The latency buffer this client acts as if the relay asked for at least.
    pub fn min_buffer_turns(&self) -> u32 {
        self.min_buffer_turns
    }

    /// The first step no simulation has run yet.
    pub fn frontier(&self) -> u32 {
        self.frontier
    }

    /// Whether any turn (contiguous or not) has arrived for `storm`.
    pub fn slot_has_any(&self, storm: usize) -> bool {
        self.slots
            .get(storm)
            .map(|s| s.turns.iter().any(|t| t.bytes.is_some()))
            .unwrap_or(false)
    }

    /// Fast-forwards the table past `step` steps the engine already ran itself (the
    /// lockstep-pending phase before the client took over driving): every step before
    /// `step` is treated as known and dispatched-as-run, and turns for them are dropped.
    /// The table's step numbers are turn numbers, so the caller must pass the engine's
    /// per-network-step turn count, not a frame count.
    pub fn advance_to(&mut self, step: u32) {
        if step <= self.frontier {
            return;
        }
        for s in &mut self.slots {
            let keep_from = s.first.max(step);
            let drop = (keep_from - s.first) as usize;
            s.turns.drain(..drop.min(s.turns.len()));
            s.first = keep_from;
            s.usable = s.usable.saturating_sub(drop);
        }
        if let Some(m) = self.mispredicted {
            if m < step {
                self.mispredicted = None;
            }
        }
        self.frontier = self.frontier.max(step);
    }

    /// The earliest step that ran on a prediction a turn has since contradicted, if any.
    pub fn mispredicted(&self) -> Option<u32> {
        self.mispredicted
    }

    /// Clears the mispredicted mark once the rollback that answers it has been scheduled.
    pub fn clear_mispredicted(&mut self) {
        self.mispredicted = None;
    }

    /// The turns `step` dispatches for each slot of `required`, the known turn or the
    /// prediction, and how many were predicted. Notes the step as run.
    pub fn dispatch(
        &mut self,
        step: u32,
        required: &[bool; SLOTS],
    ) -> ([Option<Vec<u8>>; SLOTS], u32) {
        let mut predicted = 0;
        let turns = std::array::from_fn(|storm| {
            if !required[storm] {
                return None;
            }
            let slot = &self.slots[storm];
            let known = step
                .checked_sub(slot.first)
                .map(|x| x as usize)
                .filter(|&x| x < slot.usable)
                .and_then(|x| slot.turns[x].bytes.clone());
            if known.is_none() {
                predicted += 1;
            }
            Some(known.unwrap_or_else(|| PREDICTED_TURN.to_vec()))
        });
        if step >= self.frontier {
            if predicted != 0 {
                self.counts.predicted_steps += 1;
            }
            self.frontier = step + 1;
        }
        (turns, predicted)
    }

    /// Schedules a leave the relay directed for `storm`'s slot at `step`, unless one has
    /// been scheduled for the slot already: a slot leaves once, and the relay directs
    /// every copy of a slot's leave the same way.
    pub fn schedule_leave(&mut self, storm: usize, reason: u32, step: u32) {
        if self.leaves.iter().any(|x| x.storm == storm) {
            return;
        }
        self.leaves.push(ScheduledLeave {
            storm,
            reason,
            step,
            applied: false,
        });
    }

    /// The step that applies `storm`'s slot's leave when no count of its turns came with
    /// it: the first step its turns have not been received for.
    pub fn first_step_without_turn(&self, storm: usize) -> u32 {
        self.slots.get(storm).map_or(0, |slot| {
            let present = slot.turns.iter().take_while(|t| t.bytes.is_some()).count();
            slot.first + present as u32
        })
    }

    /// The leaves `step` applies, as `(slot, native leave reason)` pairs, noting them
    /// applied. A leave applies at its step instead of dispatching that slot's turn.
    pub fn take_leaves(&mut self, step: u32) -> Vec<(usize, u32)> {
        let mut out = Vec::new();
        for leave in &mut self.leaves {
            if !leave.applied && leave.step == step {
                leave.applied = true;
                out.push((leave.storm, leave.reason));
            }
        }
        out
    }

    /// Counts since the last call, resetting them.
    pub fn take_counts(&mut self) -> InputCounts {
        std::mem::take(&mut self.counts)
    }

    /// Serialize the table for a mid-match save (see `savefile`).
    ///
    /// Everything that decides what later steps dispatch is saved: the tuning knobs,
    /// the frontier and misprediction mark, the per-slot turn windows (including gaps
    /// and each turn's lateness), the held delays and the leave schedule. The only thing
    /// dropped is each held turn's `usable_at` wall-clock: a save is taken at a
    /// *confirmed* step, so every turn up to it is already usable, and on load a still-
    /// held turn is treated as usable at once (held delays are a testing knob, not match
    /// state).
    pub fn save_state(&self) -> Vec<u8> {
        use crate::savefile::Writer;
        let mut w = Writer::new();
        w.u32(self.limit);
        w.u32(self.rollback_target);
        w.u32(self.lockstep_steps);
        w.u32(self.min_buffer_turns);
        w.u32(self.frontier);
        w.opt_u32(self.mispredicted);
        w.u32(self.counts.predicted_steps);
        w.u32(self.counts.mispredicted_turns);
        w.u32(self.counts.confirmed_predictions);
        for held in &self.held {
            w.u64(held.as_nanos() as u64);
        }
        for slot in &self.slots {
            w.u32(slot.first);
            w.u32(slot.usable as u32);
            w.u32(slot.turns.len() as u32);
            for t in &slot.turns {
                w.flag(t.bytes.is_some());
                if let Some(b) = &t.bytes {
                    w.bytes(b);
                }
                w.u32(t.lateness);
            }
        }
        for window in &self.lateness {
            w.u32(window.len() as u32);
            for &x in window {
                w.u32(x);
            }
        }
        w.u32(self.leaves.len() as u32);
        for l in &self.leaves {
            w.u32(l.storm as u32);
            w.u32(l.reason);
            w.u32(l.step);
            w.flag(l.applied);
        }
        w.into_bytes()
    }

    /// Restore the table from [`save_state`](Self::save_state) bytes.
    ///
    /// The loader is bounds-checked and refuses nonsense (`MAX_SEQ_GAP` per-slot turns,
    /// `SLOTS` leaves, capped lateness windows), so a corrupt or foreign save fails
    /// loudly instead of poisoning the session.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        use crate::savefile::Reader;
        let mut r = Reader::new(bytes);
        self.limit = r.u32()?;
        self.rollback_target = r.u32()?;
        self.lockstep_steps = r.u32()?;
        self.min_buffer_turns = r.u32()?;
        self.frontier = r.u32()?;
        self.mispredicted = r.opt_u32()?;
        self.counts = InputCounts {
            predicted_steps: r.u32()?,
            mispredicted_turns: r.u32()?,
            confirmed_predictions: r.u32()?,
        };
        let mut held = [Duration::ZERO; SLOTS];
        for h in held.iter_mut() {
            let nanos = r.u64()?;
            if nanos > 86_400_000_000_000 {
                return Err("held delay out of range");
            }
            *h = Duration::from_nanos(nanos);
        }
        self.held = held;
        for slot in self.slots.iter_mut() {
            let first = r.u32()?;
            let usable = r.u32()?;
            let n = r.u32()? as usize;
            if n > MAX_SEQ_GAP as usize {
                return Err("turn window out of range");
            }
            // `usable` indexes into the window; anything past it is corrupt.
            if usable as usize > n {
                return Err("usable index out of range");
            }
            let mut turns = VecDeque::with_capacity(n);
            for _ in 0..n {
                let has = r.flag()?;
                let bytes = if has { Some(r.bytes()?.to_vec()) } else { None };
                let lateness = r.u32()?;
                turns.push_back(HeldTurn {
                    bytes,
                    usable_at: None,
                    lateness,
                });
            }
            slot.first = first;
            slot.turns = turns;
            slot.usable = usable as usize;
        }
        for window in self.lateness.iter_mut() {
            window.clear();
            let n = r.u32()? as usize;
            if n > LATENESS_WINDOW {
                return Err("lateness window out of range");
            }
            for _ in 0..n {
                window.push_back(r.u32()?);
            }
        }
        self.leaves.clear();
        let nl = r.u32()? as usize;
        if nl > SLOTS {
            return Err("leave count out of range");
        }
        for _ in 0..nl {
            self.leaves.push(ScheduledLeave {
                storm: r.u32()? as usize,
                reason: r.u32()?,
                step: r.u32()?,
                applied: r.flag()?,
            });
        }
        r.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots() -> [bool; SLOTS] {
        let mut r = [false; SLOTS];
        r[0] = true;
        r[1] = true;
        r
    }

    fn table(limit: u32) -> InputTable {
        InputTable::new(limit, limit, 0, [Duration::ZERO; SLOTS])
    }

    #[test]
    fn a_step_whose_turns_are_all_known_dispatches_them() {
        let mut t = table(2);
        let now = Instant::now();
        t.push(0, vec![1], now);
        t.push(1, vec![2], now);
        assert_eq!(t.known_until_for(&slots()), 1);
        assert!(t.can_run(0, &slots()));
        let (turns, predicted) = t.dispatch(0, &slots());
        assert_eq!(predicted, 0);
        assert_eq!(turns[0].as_deref(), Some(&[1u8][..]));
        assert_eq!(turns[1].as_deref(), Some(&[2u8][..]));
    }

    #[test]
    fn a_missing_turn_is_predicted_up_to_the_limit() {
        let mut t = table(2);
        let now = Instant::now();
        t.push(0, vec![1], now);
        // Slot 1 has no turn yet; the limit lets steps 0 and 1 run.
        assert!(t.can_run(0, &slots()));
        assert!(t.can_run(1, &slots()));
        assert!(!t.can_run(2, &slots()));
        let (turns, predicted) = t.dispatch(0, &slots());
        assert_eq!(predicted, 1);
        assert_eq!(turns[1].as_deref(), Some(PREDICTED_TURN));
    }

    #[test]
    fn a_late_turn_that_issued_commands_marks_the_step_mispredicted() {
        let mut t = table(3);
        let now = Instant::now();
        t.push(0, vec![1], now);
        // Predict step 0 for slot 1, then the real turn arrives with a command.
        let (_, predicted) = t.dispatch(0, &slots());
        assert_eq!(predicted, 1);
        t.push(1, vec![9], now);
        assert_eq!(t.mispredicted(), Some(0));
        assert_eq!(t.take_counts().mispredicted_turns, 1);
    }

    #[test]
    fn a_late_turn_that_issued_nothing_confirms_the_prediction() {
        let mut t = table(3);
        let now = Instant::now();
        t.push(0, vec![1], now);
        t.dispatch(0, &slots());
        t.push(1, vec![SYNC_NOP], now);
        assert_eq!(t.mispredicted(), None);
        assert_eq!(t.take_counts().confirmed_predictions, 1);
    }

    #[test]
    fn a_scheduled_leave_stops_holding_the_step_back() {
        let mut t = table(4);
        let now = Instant::now();
        // Slot 0 is three turns ahead; slot 1 stopped after step 0.
        t.push(0, vec![1], now);
        t.push(0, vec![2], now);
        t.push(0, vec![3], now);
        t.push(1, vec![4], now);
        // Slot 1 still has a turn for step 0, but none for 1 or 2, so it blocks step 1.
        assert_eq!(t.known_until_for(&slots()), 1);
        assert_eq!(t.holding_back(&slots()).collect::<Vec<_>>(), vec![1]);
        // Its leave is scheduled at step 1, so from step 1 it is no longer waited on;
        // the frontier now waits on slot 0's turn for step 3 instead.
        t.schedule_leave(1, 0, 1);
        assert_eq!(t.known_until_for(&slots()), 3);
        assert_eq!(t.holding_back(&slots()).collect::<Vec<_>>(), vec![0]);
        assert_eq!(t.take_leaves(1), vec![(1, 0)]);
        assert!(t.take_leaves(1).is_empty());
    }

    // ---- the wire is lossy and unordered ------------------------------------------
    //
    // Turns arrive as datagrams, so `push_seq` must place a turn by the sequence number the
    // sender gave it, not by arrival order.

    #[test]
    fn out_of_order_turns_are_placed_by_sequence_number() {
        let mut t = table(8);
        let now = Instant::now();
        t.push(0, vec![0xa0], now);
        t.push(0, vec![0xb0], now);
        // Slot 1's turns 1 and 0 arrive in the wrong order.
        t.push_seq(1, 1, vec![0xb1], now);
        t.push_seq(1, 0, vec![0xa1], now);
        assert_eq!(t.known_until_for(&slots()), 2);
        let (turns, predicted) = t.dispatch(0, &slots());
        assert_eq!(predicted, 0);
        assert_eq!(turns[1].as_deref(), Some(&[0xa1u8][..]));
        let (turns, _) = t.dispatch(1, &slots());
        assert_eq!(turns[1].as_deref(), Some(&[0xb1u8][..]));
    }

    #[test]
    fn a_duplicate_turn_is_ignored() {
        let mut t = table(8);
        let now = Instant::now();
        t.push(0, vec![0xa0], now);
        t.push_seq(1, 0, vec![7], now);
        t.push_seq(1, 0, vec![7], now);
        assert_eq!(t.known_until_for(&slots()), 1);
        let (turns, _) = t.dispatch(0, &slots());
        assert_eq!(turns[1].as_deref(), Some(&[7u8][..]));
    }

    #[test]
    fn a_gap_is_predicted_empty_and_corrected_when_the_turn_arrives_late() {
        let mut t = table(8);
        let now = Instant::now();
        t.push(0, vec![1], now);
        // Slot 1's turn 0 was lost; turn 1 arrives. Step 0 must be predicted empty, and
        // step 1 must still be dispatchable from the turn we have.
        t.push_seq(1, 1, vec![0xbb], now);
        assert_eq!(t.known_until_for(&slots()), 0, "the gap blocks step 0");
        let (turns, predicted) = t.dispatch(0, &slots());
        assert_eq!(predicted, 1);
        assert_eq!(turns[1].as_deref(), Some(PREDICTED_TURN));
        // The lost turn finally arrives with a command: step 0 was mispredicted.
        t.push_seq(1, 0, vec![0xaa], now);
        assert_eq!(t.mispredicted(), Some(0));
        let (turns, predicted) = t.dispatch(0, &slots());
        assert_eq!(predicted, 0);
        assert_eq!(turns[1].as_deref(), Some(&[0xaau8][..]));
        let (turns, _) = t.dispatch(1, &slots());
        assert_eq!(turns[1].as_deref(), Some(&[0xbbu8][..]));
    }

    #[test]
    fn a_bogus_sequence_number_is_dropped_rather_than_allocated_for() {
        let mut t = table(8);
        let now = Instant::now();
        t.push(0, vec![0xa0], now);
        t.push_seq(1, 0, vec![1], now);
        // A peer claiming to be four billion turns ahead must not allocate for it.
        t.push_seq(1, u32::MAX, vec![2], now);
        assert_eq!(t.known_until_for(&slots()), 1);
    }

    #[test]
    fn lateness_is_measured_against_the_step_the_simulation_had_reached() {
        let mut t = table(8);
        let now = Instant::now();
        t.push(0, vec![1], now);
        t.dispatch(0, &slots());
        t.dispatch(1, &slots());
        // Slot 1's turn 0 arrives two steps late.
        t.push_seq(1, 0, vec![9], now);
        assert_eq!(t.lateness_median_p90(1), Some((2, 2)));
        // Turn 1 arrives one step late (the frontier is already 2).
        t.push_seq(1, 1, vec![9], now);
        assert_eq!(t.lateness_median_p90(1), Some((1, 2)));
        assert_eq!(t.lateness_median_p90(7), None);
    }
}
