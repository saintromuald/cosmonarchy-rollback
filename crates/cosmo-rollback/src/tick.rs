//! What one game-loop tick should do: roll back as far as needed, re-simulate, and step
//! on to the present, snapshotting on a fixed spacing.
//!
//! Ported from ShieldBattery's `game/src/rollback/tick.rs` (MIT). The reference's tick
//! drives StarCraft through `bw` calls directly; here the plan is pure data so the
//! scheduler can be reasoned about and tested without the engine, and `crate::rollback`
//! executes it against the 1.16.1 frame hook.
//!
//! Frames are frame counts: frame `n` is the simulation state after `n` logic steps, and
//! the step that produces it is the step from `n - 1`.

#![allow(dead_code)]

/// Frames between snapshots unless a knob says otherwise. A rollback re-simulates from
/// the newest snapshot at or before the frame it needs, so a wider spacing snapshots
/// less often and re-simulates more per rollback.
pub const DEFAULT_SNAPSHOT_SPACING: u32 = 3;

/// What one tick should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickPlan {
    /// The earliest frame whose simulation has to be redone: the tick restores the
    /// newest snapshot at or before it and steps forward from there. `None`, or a frame
    /// the simulation has not passed yet, re-simulates nothing.
    pub rollback_to: Option<u32>,
    /// The frame the tick ends on, which is shown next.
    pub present: u32,
    /// The newest frame no later tick will simulate differently. Snapshots from before
    /// the newest one at or before it are dropped, since no rollback will reach back
    /// past it.
    pub confirmed: u32,
    /// Snapshots are taken of every frame that is a multiple of this.
    pub spacing: u32,
}

/// The step about to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepInfo {
    /// The frame the step produces.
    pub frame: u32,
    /// How many steps the tick runs after this one.
    pub steps_after: u32,
    /// Whether the frame it produces is confirmed, so no later tick simulates it again.
    pub is_final: bool,
    /// Whether this is the first step after the tick restored a snapshot, so the
    /// simulation is still exactly as that snapshot holds it.
    pub resumes_from_restore: bool,
    /// Whether the frame it produces was produced before, by the simulation the tick
    /// rolled back.
    pub is_resimulation: bool,
}

impl TickPlan {
    /// Plans a tick that ends on `present`, re-simulating from `rollback_to` when the
    /// input table has contradicted a prediction at or before it.
    ///
    /// `current` is the frame the simulation is on now; `confirmed` is the newest frame
    /// no later tick will simulate differently (the frontier the input table is sure of).
    pub fn new(
        current: u32,
        present: u32,
        confirmed: u32,
        mispredicted: Option<u32>,
        spacing: u32,
    ) -> Self {
        let spacing = spacing.max(1);
        // A rollback only makes sense for a frame the simulation has already passed.
        let rollback_to = mispredicted.filter(|&m| m < present && m < current);
        Self {
            rollback_to,
            present,
            confirmed,
            spacing,
        }
    }

    /// The first frame the tick simulates: the snapshot frame it restores, or the frame
    /// the simulation is already on when it does not roll back.
    ///
    /// This is the *restore* frame, not the rollback target: a snapshot is only taken on
    /// multiples of `spacing`, so the newest one at or before the target is earlier than
    /// the target, and the engine has to re-run every frame between them too.
    pub fn start(&self, current: u32) -> u32 {
        match self.restore_frame(current) {
            Some(f) => f,
            None => current,
        }
    }

    /// Whether the tick restores a snapshot before stepping.
    pub fn restores(&self, current: u32) -> bool {
        matches!(self.rollback_to, Some(f) if f < current)
    }

    /// The snapshot frame to restore for a rollback: the newest multiple of `spacing` at
    /// or before the rollback target. `None` when the tick does not roll back.
    pub fn restore_frame(&self, current: u32) -> Option<u32> {
        if !self.restores(current) {
            return None;
        }
        let target = self.rollback_to.unwrap_or(current);
        Some(target - (target % self.spacing))
    }

    /// The steps the tick runs, in order, each producing the next frame.
    pub fn steps(&self, current: u32) -> impl Iterator<Item = StepInfo> + '_ {
        let start = self.start(current);
        let present = self.present;
        let confirmed = self.confirmed;
        let restores = self.restores(current);
        (start..present).map(move |frame| {
            let produced = frame + 1;
            StepInfo {
                frame: produced,
                steps_after: present.saturating_sub(produced),
                is_final: produced >= confirmed,
                resumes_from_restore: restores && frame == start,
                is_resimulation: produced <= current,
            }
        })
    }

    /// Whether the tick should snapshot the frame it just produced.
    pub fn snapshots(&self, frame: u32) -> bool {
        frame % self.spacing == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_tick_steps_one_frame_and_snapshots_on_spacing() {
        let plan = TickPlan::new(10, 11, 11, None, 3);
        assert!(!plan.restores(10));
        assert_eq!(plan.restore_frame(10), None);
        let steps: Vec<_> = plan.steps(10).collect();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].frame, 11);
        assert!(!steps[0].is_resimulation);
        assert!(steps[0].is_final);
        assert!(!plan.snapshots(11));
        assert!(plan.snapshots(12));
    }

    #[test]
    fn a_misprediction_rolls_back_to_the_snapshot_at_or_before_it() {
        // Simulation is on frame 10; a turn contradicted the prediction for frame 7.
        let plan = TickPlan::new(10, 11, 11, Some(7), 3);
        assert!(plan.restores(10));
        assert_eq!(plan.restore_frame(10), Some(6));
        // Steps run from the restored frame 6 through to frame 11.
        assert_eq!(plan.start(10), 6);
        let steps: Vec<_> = plan.steps(10).collect();
        assert_eq!(steps.len(), 5); // frames 7, 8, 9, 10, 11
        assert_eq!(steps[0].frame, 7);
        assert!(steps[0].resumes_from_restore);
        assert!(steps[0].is_resimulation);
        assert!(!steps[1].resumes_from_restore);
        // Everything up to frame 10 was produced before; frame 11 is new.
        assert!(steps[3].frame == 10 && steps[3].is_resimulation);
        assert_eq!(steps[4].frame, 11);
        assert!(!steps[4].is_resimulation);
    }

    #[test]
    fn a_misprediction_at_or_after_the_present_does_not_roll_back() {
        let plan = TickPlan::new(10, 11, 11, Some(11), 3);
        assert!(!plan.restores(10));
        assert_eq!(plan.restore_frame(10), None);
    }

    #[test]
    fn spacing_zero_is_treated_as_one() {
        let plan = TickPlan::new(4, 5, 5, Some(2), 0);
        assert_eq!(plan.spacing, 1);
        assert_eq!(plan.restore_frame(4), Some(2));
    }
}
