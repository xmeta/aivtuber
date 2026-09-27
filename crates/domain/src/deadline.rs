use crate::DomainValidationError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionDeadlineClass {
    HighPriority,
    StrongReaction,
    Conversation,
    Commentary,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineStage {
    NormalizationSecurity,
    Retrieval,
    Reflex,
    Template,
    GenerationQueue,
    Thinking,
    Tts,
    Commit,
    SchedulerDispatch,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineExhaustionReason {
    Expired,
    InsufficientBudget,
    ProviderTimeout,
    Cancelled,
    StaleCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionDeadline {
    pub class: InteractionDeadlineClass,
    pub started_at_ms: u64,
    pub expires_at_ms: u64,
}

impl InteractionDeadline {
    pub fn new(
        class: InteractionDeadlineClass,
        started_at_ms: u64,
        total_budget_ms: u64,
    ) -> Result<Self, DomainValidationError> {
        if total_budget_ms == 0 {
            return Err(DomainValidationError::new(
                "total_budget_ms",
                "must be greater than zero",
            ));
        }
        Ok(Self {
            class,
            started_at_ms,
            expires_at_ms: started_at_ms.saturating_add(total_budget_ms),
        })
    }

    pub fn total_budget_ms(self) -> u64 {
        self.expires_at_ms.saturating_sub(self.started_at_ms)
    }

    pub fn remaining_ms(self, now_ms: u64) -> u64 {
        self.expires_at_ms.saturating_sub(now_ms)
    }

    pub fn is_expired(self, now_ms: u64) -> bool {
        self.remaining_ms(now_ms) == 0
    }

    pub fn admits(self, now_ms: u64, min_useful_ms: u64) -> bool {
        self.remaining_ms(now_ms) >= min_useful_ms.max(1)
    }

    pub fn effective_timeout(self, now_ms: u64, provider_max: Duration) -> Option<Duration> {
        let remaining = Duration::from_millis(self.remaining_ms(now_ms));
        (!remaining.is_zero()).then(|| remaining.min(provider_max))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_budget_uses_logical_monotonic_time() {
        let deadline = InteractionDeadline::new(InteractionDeadlineClass::Conversation, 100, 500)
            .expect("deadline");
        assert_eq!(deadline.total_budget_ms(), 500);
        assert_eq!(deadline.remaining_ms(100), 500);
        assert_eq!(deadline.remaining_ms(599), 1);
        assert_eq!(deadline.remaining_ms(600), 0);
        assert!(deadline.is_expired(700));
    }

    #[test]
    fn recorded_deadline_replays_identical_logical_admission_decisions() {
        let original = InteractionDeadline::new(InteractionDeadlineClass::Conversation, 100, 500)
            .expect("deadline");
        let recorded = serde_json::to_vec(&original).expect("record deadline");
        let replayed: InteractionDeadline =
            serde_json::from_slice(&recorded).expect("replay deadline");

        for now_ms in [100_u64, 598, 599, 600, 700] {
            assert_eq!(replayed.remaining_ms(now_ms), original.remaining_ms(now_ms));
            assert_eq!(replayed.admits(now_ms, 2), original.admits(now_ms, 2));
            assert_eq!(
                replayed.effective_timeout(now_ms, Duration::from_secs(5)),
                original.effective_timeout(now_ms, Duration::from_secs(5))
            );
        }

        assert!(original.admits(598, 2));
        assert!(!original.admits(599, 2));
        assert!(original.is_expired(600));
    }

    #[test]
    fn effective_timeout_never_exceeds_remaining_budget() {
        let deadline = InteractionDeadline::new(InteractionDeadlineClass::HighPriority, 10, 100)
            .expect("deadline");
        assert_eq!(
            deadline.effective_timeout(60, Duration::from_secs(5)),
            Some(Duration::from_millis(50))
        );
    }
}
