//! Face settle after a reply (issue #447, first decay slice of #86).
//!
//! The renderer keeps one active expression and never decays it, so without
//! this the face of a reply's last phrase would be held until the next reply.
//! `FaceSettle` remembers the last expression the avatar output accepted and,
//! once the performer has been idle long enough, steps it down: the same
//! preset at half intensity, then the baseline expression.
//!
//! # Why this is not a scheduler item
//!
//! A scheduled Face-only low-priority item would delay speech. A playing item
//! that conflicts on the Face channel becomes the new reply's hand-off
//! boundary, the reply then waits `min_reaction_spacing_ms` after it, and its
//! retirement moves `last_terminal_at` and the asset cooldown, so the settle
//! would also be charged against reaction spacing. A host-direct
//! `avatar.execute()` (the precedent is `reset_avatar_neutral`) touches none of
//! that state. The scheduler is only read.
//!
//! The guards that replace `command_is_live`/`command_not_cancelled`, enforced
//! by `ProductionApp::service_face_settle` and this type:
//!
//! * nothing is `Queued` or `Playing`, and no phrased reply that already spoke
//!   may still commit a phrase, so a new reply (committed at the start of the
//!   same tick, before the settle runs) always wins and a settle step can never
//!   follow its start;
//! * the output is not muted;
//! * `Reset` (operator stop, mute, barge-in) calls [`FaceSettle::clear`], so no
//!   step survives it;
//! * a stale half step is skipped once the baseline is due;
//! * all timing is logical (`now_ms` from the caller and the scheduler's last
//!   terminal time), never a wall clock, so a replay emits the same commands.
//!
//! State is one optional face and one stage: bounded by construction.

/// Idle time after the reply's end before the half-intensity step.
pub const FACE_SETTLE_HALF_AFTER_MS: u64 = 1_500;
/// Idle time after the reply's end before the baseline expression.
pub const FACE_SETTLE_BASELINE_AFTER_MS: u64 = 4_000;

/// Intensities this close to the baseline's count as "already baseline".
const BASELINE_EPSILON: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Held,
    Halved,
    Done,
}

#[derive(Debug, Clone, PartialEq)]
struct LastFace {
    preset: String,
    intensity: f64,
    at_ms: u64,
    stage: Stage,
}

/// One settle step to execute: an `Expression` for the avatar output.
#[derive(Debug, Clone, PartialEq)]
pub struct SettleStep {
    pub preset: String,
    pub intensity: f64,
    /// The logical due time of the step (not the tick that noticed it).
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FaceSettle {
    baseline_preset: String,
    baseline_intensity: f64,
    half_after_ms: u64,
    baseline_after_ms: u64,
    last: Option<LastFace>,
}

impl FaceSettle {
    pub fn new(baseline_preset: impl Into<String>, baseline_intensity: f64) -> Self {
        Self {
            baseline_preset: baseline_preset.into(),
            baseline_intensity: baseline_intensity.clamp(0.0, 1.0),
            half_after_ms: FACE_SETTLE_HALF_AFTER_MS,
            baseline_after_ms: FACE_SETTLE_BASELINE_AFTER_MS,
            last: None,
        }
    }

    /// Record an expression the avatar output accepted.
    pub fn note_expression(&mut self, preset: &str, intensity: f64, at_ms: u64) {
        self.last = Some(LastFace {
            preset: preset.to_owned(),
            intensity,
            at_ms,
            stage: Stage::Held,
        });
    }

    /// Forget the face (the avatar was reset to neutral).
    pub fn clear(&mut self) {
        self.last = None;
    }

    /// The step due at `now_ms`, if any. `idle_since_ms` is the scheduler's
    /// last terminal time; the caller has already checked that nothing is
    /// queued, playing or about to be committed. `graded` is whether the
    /// output can show an intensity (VTS hotkeys cannot), in which case the
    /// half step is skipped.
    pub fn due(&mut self, idle_since_ms: u64, now_ms: u64, graded: bool) -> Option<SettleStep> {
        let (baseline_preset, baseline_intensity) =
            (self.baseline_preset.clone(), self.baseline_intensity);
        let (half_after, baseline_after) = (self.half_after_ms, self.baseline_after_ms);
        let last = self.last.as_mut()?;
        if last.stage == Stage::Done {
            return None;
        }
        if last.preset == baseline_preset && last.intensity <= baseline_intensity + BASELINE_EPSILON
        {
            last.stage = Stage::Done;
            return None;
        }
        let anchor = idle_since_ms.max(last.at_ms);
        let half_due = anchor.saturating_add(half_after);
        let baseline_due = anchor.saturating_add(baseline_after);
        if now_ms >= baseline_due {
            *last = LastFace {
                preset: baseline_preset.clone(),
                intensity: baseline_intensity,
                at_ms: baseline_due,
                stage: Stage::Done,
            };
            return Some(SettleStep {
                preset: baseline_preset,
                intensity: baseline_intensity,
                at_ms: baseline_due,
            });
        }
        if now_ms >= half_due && last.stage == Stage::Held && graded {
            last.stage = Stage::Halved;
            // The mood fades first and the resting face follows, so the half
            // step may sit under the baseline's own intensity.
            return Some(SettleStep {
                preset: last.preset.clone(),
                intensity: last.intensity * 0.5,
                at_ms: half_due,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle() -> FaceSettle {
        FaceSettle::new("speaking.neutral", 0.4)
    }

    fn step(preset: &str, intensity: f64, at_ms: u64) -> Option<SettleStep> {
        Some(SettleStep {
            preset: preset.to_owned(),
            intensity,
            at_ms,
        })
    }

    #[test]
    fn steps_down_to_half_then_baseline() {
        let mut s = settle();
        s.note_expression("surprise_bright", 0.8, 1_000);
        // The reply ended at 3_000.
        assert_eq!(s.due(3_000, 4_499, true), None);
        assert_eq!(
            s.due(3_000, 4_500, true),
            step("surprise_bright", 0.4, 4_500)
        );
        assert_eq!(s.due(3_000, 6_999, true), None);
        assert_eq!(
            s.due(3_000, 7_000, true),
            step("speaking.neutral", 0.4, 7_000)
        );
        assert_eq!(s.due(3_000, 60_000, true), None, "settled for good");
    }

    #[test]
    fn the_half_step_may_sit_under_the_baseline_intensity() {
        let mut s = settle();
        s.note_expression("surprise_medium", 0.26, 0);
        assert_eq!(s.due(0, 1_500, true), step("surprise_medium", 0.13, 1_500));
        assert_eq!(s.due(0, 4_000, true), step("speaking.neutral", 0.4, 4_000));
    }

    #[test]
    fn a_late_tick_sends_only_the_baseline() {
        let mut s = settle();
        s.note_expression("smile_bright", 0.6, 0);
        assert_eq!(s.due(0, 9_000, true), step("speaking.neutral", 0.4, 4_000));
    }

    #[test]
    fn no_settle_when_the_face_is_already_baseline() {
        let mut s = settle();
        s.note_expression("speaking.neutral", 0.4, 0);
        assert_eq!(s.due(0, 10_000, true), None);
        s.note_expression("speaking.neutral", 0.3, 0);
        assert_eq!(s.due(0, 10_000, true), None);
    }

    #[test]
    fn an_output_without_intensity_gets_only_the_baseline() {
        let mut s = settle();
        s.note_expression("smile_bright", 0.6, 0);
        assert_eq!(s.due(0, 2_000, false), None);
        assert_eq!(s.due(0, 4_000, false), step("speaking.neutral", 0.4, 4_000));
    }

    #[test]
    fn a_new_face_restarts_the_sequence_and_clear_cancels_it() {
        let mut s = settle();
        s.note_expression("smile_bright", 0.6, 0);
        assert!(s.due(0, 1_500, true).is_some());
        s.note_expression("surprise_bright", 0.6, 2_000);
        assert_eq!(s.due(0, 3_000, true), None, "anchored on the new face");
        s.clear();
        assert_eq!(s.due(0, 60_000, true), None);
    }

    #[test]
    fn timing_depends_only_on_logical_inputs() {
        let run = |ticks: &[u64]| {
            let mut s = settle();
            s.note_expression("smile_bright", 0.6, 500);
            ticks
                .iter()
                .filter_map(|now| s.due(1_000, *now, true))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(&[2_500, 3_000, 5_000, 6_000]), run(&[2_500, 5_000]));
        let dense: Vec<u64> = (2_500..5_000).step_by(7).chain([5_000]).collect();
        assert_eq!(run(&[2_500, 5_000]), run(&dense));
    }
}
