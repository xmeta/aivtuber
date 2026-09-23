#![forbid(unsafe_code)]

//! Provider-neutral reflex/decision-engine boundary.

/// Availability state used by deterministic fallback policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionAvailability {
    Available,
    TimedOut,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::DecisionAvailability;

    #[test]
    fn timeout_is_not_available() {
        assert_ne!(
            DecisionAvailability::TimedOut,
            DecisionAvailability::Available
        );
    }
}
