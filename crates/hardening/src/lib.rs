#![forbid(unsafe_code)]

//! Deterministic failure-injection and logical-time soak harness.
//!
//! This crate is test/tooling infrastructure. Production crates do not depend on it.

mod conformance;
mod error;
mod fault;
mod scenario_soak;
mod soak;

pub use conformance::*;
pub use error::*;
pub use fault::*;
pub use scenario_soak::*;
pub use soak::*;

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_telemetry::{ComparisonMode, ReproducibilityMetadata};

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
    fn logical_soak_retained_state_plateaus_at_configured_limits() {
        let config = SoakConfig {
            logical_events: 2_000,
            event_interval_ms: 50,
            ingress_queue_limit: 16,
            scheduler_history_limit: 64,
            audit_limit: 64,
            telemetry_limit: 64,
            working_memory_limit: 32,
            memory_compaction_limit: 16,
            generated_asset_limit: 16,
            promotion_metadata_limit: 16,
            generated_asset_every: 25,
        };
        let report = run_core_soak(
            config.clone(),
            metadata(config.logical_events, config.event_interval_ms),
        )
        .expect("soak");

        assert_eq!(report.final_state.scheduler_active, 0);
        assert_eq!(
            report.final_state.scheduler_history,
            config.scheduler_history_limit
        );
        assert_eq!(report.final_state.scheduler_cooldowns, 0);
        assert_eq!(report.final_state.content_queue, 0);
        assert_eq!(report.final_state.audit_records, config.audit_limit);
        assert_eq!(report.final_state.rate_limit_sources, 1);
        assert_eq!(
            report.final_state.working_memory_entries,
            config.working_memory_limit
        );
        assert_eq!(
            report.final_state.memory_compaction_records,
            config.memory_compaction_limit
        );
        assert_eq!(report.final_state.telemetry_events, config.telemetry_limit);
        assert_eq!(report.final_state.hot_assets, config.generated_asset_limit);
        assert_eq!(
            report.final_state.promotion_metadata,
            config.promotion_metadata_limit
        );

        for metric in [
            "scheduler_items",
            "scheduler_history",
            "scheduler_cooldowns",
            "content_queue",
            "audit_records",
            "rate_limit_sources",
            "working_memory_entries",
            "memory_compaction_records",
            "telemetry_events",
            "hot_assets",
            "promotion_metadata",
        ] {
            let finding = report.finding(metric).expect("soak finding");
            assert!(
                !finding.lifetime_growth_detected,
                "{metric} must plateau after warm-up: {finding:?}"
            );
            assert!(
                !finding.configured_limit_exceeded,
                "{metric} must stay within its configured bound: {finding:?}"
            );
        }
        assert_eq!(report.metadata.dataset_id, "hardening-unit-soak");
    }

    fn resource_environment() -> aivtuber_telemetry::BenchmarkEnvironment {
        aivtuber_telemetry::BenchmarkEnvironment {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            cpu: None,
            rust_version: "rustc-test".to_owned(),
            bun_version: None,
            cargo_profile: "debug".to_owned(),
        }
    }

    #[test]
    fn resource_bench_result_maps_clean_soak_to_zero_invariants() {
        let config = SoakConfig {
            logical_events: 2_000,
            event_interval_ms: 50,
            ..SoakConfig::default()
        };
        let report = run_core_soak(
            config.clone(),
            metadata(config.logical_events, config.event_interval_ms),
        )
        .expect("soak");

        let result = resource_bench_result(
            &report,
            ResourceBenchTiming {
                wall_clock_ms: 1_000,
                peak_rss_kib: Some(12_345),
            },
            resource_environment(),
        )
        .expect("contract result");

        assert_eq!(
            result.schema_version,
            aivtuber_telemetry::RESULT_SCHEMA_VERSION
        );
        assert_eq!(result.benchmark_suite, RESOURCE_BENCH_SUITE);
        assert_eq!(result.mode, RESOURCE_BENCH_MODE);
        assert_eq!(result.dataset_id.as_deref(), Some(RESOURCE_BENCH_DATASET));
        for invariant in [
            "resource.retention_bound_violation_count",
            "resource.non_plateau_state_count",
        ] {
            let value = result
                .invariants
                .get(invariant)
                .unwrap_or_else(|| panic!("missing invariant {invariant}"));
            assert_eq!(value.value, 0, "{invariant} must be clean: {value:?}");
        }
        // Retained-state metrics expose the configured plateau levels.
        assert_eq!(
            result.metrics["resource.scheduler_history_retained_count"].value,
            config.scheduler_history_limit as f64
        );
        assert_eq!(
            result.metrics["resource.hot_assets_resident_count"].value,
            config.generated_asset_limit as f64
        );
        assert!(result.metrics.contains_key("resource.peak_rss_kib"));
        assert!(
            result
                .metrics
                .contains_key("resource.throughput_events_per_s")
        );
    }

    #[test]
    fn resource_bench_result_maps_growth_findings_to_invariants() {
        // Directly exercise the invariant mapping rather than manufacturing a
        // growing runtime: the mapping is a pure function of GrowthFinding.
        let report = SoakReport {
            metadata: metadata(100, 50),
            config: SoakConfig::default(),
            midpoint: StateSnapshot::default(),
            final_state: StateSnapshot::default(),
            telemetry_summary: {
                let empty = aivtuber_telemetry::BenchmarkReport::from_events(
                    metadata(1, 50),
                    ComparisonMode::FullGenerative,
                    Vec::new(),
                )
                .expect("empty report");
                empty.summary
            },
            growth: vec![
                GrowthFinding {
                    metric: "scheduler_history".to_owned(),
                    midpoint: 400,
                    final_count: 600,
                    configured_limit: Some(512),
                    lifetime_growth_detected: true,
                    configured_limit_exceeded: true,
                    related_issue: Some(52),
                },
                GrowthFinding {
                    metric: "audit_records".to_owned(),
                    midpoint: 500,
                    final_count: 700,
                    configured_limit: None,
                    lifetime_growth_detected: true,
                    configured_limit_exceeded: false,
                    related_issue: Some(51),
                },
            ],
            flood: FloodReport {
                attempted: 1,
                queued: 1,
                dropped_backpressure: 0,
                final_queue_len: 0,
                stop_cancelled: 1,
                mute_succeeded: true,
            },
        };

        let result = resource_bench_result(
            &report,
            ResourceBenchTiming {
                wall_clock_ms: 0,
                peak_rss_kib: None,
            },
            resource_environment(),
        )
        .expect("contract result");

        assert_eq!(
            result.invariants["resource.retention_bound_violation_count"].value,
            1
        );
        assert_eq!(
            result.invariants["resource.non_plateau_state_count"].value,
            2
        );
        assert!(!result.metrics.contains_key("resource.peak_rss_kib"));
        // Throughput falls back to 0 rather than dividing by zero.
        assert_eq!(
            result.metrics["resource.throughput_events_per_s"].value,
            0.0
        );
        // Round-trips through the #58 comparator's result type.
        let bytes = serde_json::to_vec(&result).expect("serialize");
        let parsed =
            aivtuber_telemetry::BenchmarkResult::from_json(&bytes).expect("contract valid");
        assert_eq!(parsed, result);
    }
}
