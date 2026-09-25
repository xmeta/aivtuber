#![forbid(unsafe_code)]

//! Deterministic failure-injection and logical-time soak harness.
//!
//! This crate is test/tooling infrastructure. Production crates do not depend on it.

mod error;
mod fault;
mod soak;

pub use error::*;
pub use fault::*;
pub use soak::*;

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_telemetry::ReproducibilityMetadata;

    fn metadata(events: u64, interval_ms: u64) -> ReproducibilityMetadata {
        ReproducibilityMetadata {
            dataset_id: "hardening-unit-soak".to_owned(),
            git_commit: "unit-test".to_owned(),
            rust_toolchain: "rustc-test".to_owned(),
            bun_toolchain: None,
            config_version: "hardening-v1".to_owned(),
            asset_version: "generated-dynamic-v1".to_owned(),
            index_version: None,
            jev_model: None,
            thinking_model: None,
            tts_model: None,
            seed: 7,
            stream_duration_ms: Some(events.saturating_mul(interval_ms)),
        }
    }

    #[test]
    fn fault_plan_is_deterministic_and_subsystems_are_independent() {
        let plan = FaultPlan::new(
            42,
            vec![
                FaultSpec {
                    subsystem: FaultSubsystem::Tts,
                    occurrence: 2,
                    kind: FaultKind::Timeout,
                },
                FaultSpec {
                    subsystem: FaultSubsystem::Jev,
                    occurrence: 1,
                    kind: FaultKind::RateLimited,
                },
                FaultSpec {
                    subsystem: FaultSubsystem::Audio,
                    occurrence: 1,
                    kind: FaultKind::Unavailable,
                },
            ],
        )
        .expect("plan");
        let mut first = plan.injector();
        let mut second = plan.injector();

        for injector in [&mut first, &mut second] {
            assert_eq!(
                injector.next(FaultSubsystem::Jev),
                Some(FaultKind::RateLimited)
            );
            assert_eq!(injector.next(FaultSubsystem::Tts), None);
            assert_eq!(
                injector.next(FaultSubsystem::Audio),
                Some(FaultKind::Unavailable)
            );
            assert_eq!(injector.next(FaultSubsystem::Tts), Some(FaultKind::Timeout));
        }
        assert_eq!(first.consumed(), second.consumed());
        assert_eq!(first.seed(), 42);
    }

    #[test]
    fn content_flood_is_bounded_and_emergency_control_stays_responsive() {
        let report = run_content_flood(8).expect("flood");
        assert_eq!(report.queued, 8);
        assert_eq!(report.dropped_backpressure, 16);
        assert_eq!(report.final_queue_len, 8);
        assert_eq!(report.stop_cancelled, 1);
        assert!(report.mute_succeeded);
    }

    #[test]
    fn logical_soak_detects_lifetime_growth_and_bounded_working_state() {
        let config = SoakConfig {
            logical_events: 2_000,
            event_interval_ms: 50,
            ingress_queue_limit: 16,
            working_memory_limit: 32,
            generated_asset_every: 25,
        };
        let report = run_core_soak(
            config.clone(),
            metadata(config.logical_events, config.event_interval_ms),
        )
        .expect("soak");

        assert_eq!(report.final_state.scheduler_active, 0);
        assert!(report.final_state.scheduler_cancelled >= 200);
        assert!(report.final_state.scheduler_completed >= 1_700);
        assert_eq!(report.final_state.content_queue, 0);
        assert!(report.final_state.working_memory_entries <= config.working_memory_limit);
        // Issue #52: live scheduler items no longer grow with lifetime event
        // count - terminal items retire into bounded history, so the soak
        // probe must find NO lifetime growth in scheduler_items.
        assert!(report.finding("scheduler_items").is_some_and(|finding| {
            !finding.lifetime_growth_detected && finding.related_issue == Some(52)
        }));
        assert!(report.finding("telemetry_events").is_some_and(|finding| {
            finding.lifetime_growth_detected && finding.related_issue == Some(51)
        }));
        assert!(report.finding("hot_assets").is_some_and(
            |finding| finding.lifetime_growth_detected && finding.related_issue == Some(53)
        ));
        assert!(
            report
                .finding("working_memory_entries")
                .is_some_and(|finding| !finding.configured_limit_exceeded)
        );
        assert_eq!(report.metadata.dataset_id, "hardening-unit-soak");
    }
}
