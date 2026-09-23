#![forbid(unsafe_code)]

//! Deterministic performance scheduling primitives.

/// Coarse execution priority.
///
/// Detailed interruption policy is added in the scheduler implementation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Idle,
    Background,
    Conversation,
    StrongReaction,
    HighPriorityInteraction,
    Operator,
}

#[cfg(test)]
mod tests {
    use super::Priority;

    #[test]
    fn operator_has_highest_priority() {
        assert!(Priority::Operator > Priority::HighPriorityInteraction);
    }
}
