#![forbid(unsafe_code)]

//! Telemetry vocabulary shared by benchmarks and runtime instrumentation.

/// Initial metric groups required by the architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricGroup {
    Latency,
    Routing,
    Cache,
    Generation,
    Fallback,
}

#[cfg(test)]
mod tests {
    use super::MetricGroup;

    #[test]
    fn latency_metric_group_exists() {
        assert_eq!(MetricGroup::Latency, MetricGroup::Latency);
    }
}
