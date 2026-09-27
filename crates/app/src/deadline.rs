use crate::AppError;
use aivtuber_domain::{EventKind, InteractionDeadline, InteractionDeadlineClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionDeadlinePolicy {
    pub high_priority_ms: u64,
    pub strong_reaction_ms: u64,
    pub conversation_ms: u64,
    pub commentary_ms: u64,
    pub background_ms: u64,
    pub min_generation_start_ms: u64,
}

impl Default for InteractionDeadlinePolicy {
    fn default() -> Self {
        // Provisional safety ceilings, not latency SLO targets. Keep enough
        // headroom to preserve the pre-#67 two-provider path until #58
        // calibrates measured per-class budgets.
        Self {
            high_priority_ms: 30_000,
            strong_reaction_ms: 30_000,
            conversation_ms: 30_000,
            commentary_ms: 30_000,
            background_ms: 30_000,
            min_generation_start_ms: 1,
        }
    }
}
impl InteractionDeadlinePolicy {
    pub fn validate(self) -> Result<Self, AppError> {
        if [
            self.high_priority_ms,
            self.strong_reaction_ms,
            self.conversation_ms,
            self.commentary_ms,
            self.background_ms,
            self.min_generation_start_ms,
        ]
        .into_iter()
        .any(|value| value == 0)
        {
            return Err(AppError::Routing(
                "interaction deadline budgets must be greater than zero".to_owned(),
            ));
        }
        Ok(self)
    }

    pub fn class_for(self, kind: EventKind) -> Option<InteractionDeadlineClass> {
        match kind {
            EventKind::OperatorCommand => None,
            EventKind::ChatDonation => Some(InteractionDeadlineClass::HighPriority),
            EventKind::GameEvent => Some(InteractionDeadlineClass::StrongReaction),
            EventKind::ChatMessage | EventKind::SpeechInput => {
                Some(InteractionDeadlineClass::Conversation)
            }
            EventKind::StreamEvent => Some(InteractionDeadlineClass::Commentary),
            EventKind::TimerTick | EventKind::SystemHealth => {
                Some(InteractionDeadlineClass::Background)
            }
        }
    }
    pub fn total_budget_ms(self, class: InteractionDeadlineClass) -> u64 {
        match class {
            InteractionDeadlineClass::HighPriority => self.high_priority_ms,
            InteractionDeadlineClass::StrongReaction => self.strong_reaction_ms,
            InteractionDeadlineClass::Conversation => self.conversation_ms,
            InteractionDeadlineClass::Commentary => self.commentary_ms,
            InteractionDeadlineClass::Background => self.background_ms,
        }
    }

    pub fn deadline_for(
        self,
        kind: EventKind,
        started_at_ms: u64,
    ) -> Result<Option<InteractionDeadline>, AppError> {
        let Some(class) = self.class_for(kind) else {
            return Ok(None);
        };
        InteractionDeadline::new(class, started_at_ms, self.total_budget_ms(class))
            .map(Some)
            .map_err(|error| AppError::Routing(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_class_comes_only_from_validated_event_kind() {
        let policy = InteractionDeadlinePolicy::default();
        assert_eq!(
            policy.class_for(EventKind::ChatDonation),
            Some(InteractionDeadlineClass::HighPriority)
        );
        assert_eq!(
            policy.class_for(EventKind::ChatMessage),
            Some(InteractionDeadlineClass::Conversation)
        );
        assert_eq!(policy.class_for(EventKind::OperatorCommand), None);
    }

    #[test]
    fn custom_class_budgets_are_deterministic() {
        let policy = InteractionDeadlinePolicy {
            high_priority_ms: 900,
            strong_reaction_ms: 800,
            conversation_ms: 700,
            commentary_ms: 600,
            background_ms: 500,
            min_generation_start_ms: 100,
        };
        let deadline = policy
            .deadline_for(EventKind::ChatMessage, 1_000)
            .expect("policy")
            .expect("content deadline");
        assert_eq!(deadline.started_at_ms, 1_000);
        assert_eq!(deadline.expires_at_ms, 1_700);
        assert_eq!(deadline.class, InteractionDeadlineClass::Conversation);
    }
}
