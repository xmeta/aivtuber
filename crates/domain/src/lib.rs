#![forbid(unsafe_code)]

//! Provider-neutral domain types for the AI VTuber runtime.

/// Runtime time-scale layers.
///
/// Provider-specific types do not belong in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionLayer {
    /// Deterministic frame/timeline execution.
    Motor,
    /// Event-driven structured decisions.
    Reflex,
    /// Expensive generative reasoning.
    Thinking,
}

#[cfg(test)]
mod tests {
    use super::ExecutionLayer;

    #[test]
    fn layers_are_distinct() {
        assert_ne!(ExecutionLayer::Motor, ExecutionLayer::Reflex);
        assert_ne!(ExecutionLayer::Reflex, ExecutionLayer::Thinking);
    }
}
