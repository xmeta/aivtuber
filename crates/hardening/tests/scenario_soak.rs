//! Issue #70 Stage 1 closure: the #40 failure/soak path consumes the same
//! versioned scenario the #58 benchmark runs.
//!
//! The properties under test are the ones that make a shared workload
//! trustworthy for retention measurement:
//!
//! 1. **Reproducibility.** The same scenario and overlay yield an identical
//!    report, or a retained-state plateau cannot be compared against anything.
//! 2. **Identity, not similarity.** An overlay written for another scenario (or
//!    another version) is refused rather than applied to a different workload.
//! 3. **Observable faults only.** A subsystem this path never calls is refused,
//!    because an injected fault nobody can observe is decoration.
//! 4. **Degrade, never freeze.** Injected faults are counted and the
//!    deterministic performer keeps advancing.
//! 5. **Retention over long logical time.** A long scenario plateau at the
//!    configured bounds instead of growing without limit.

use aivtuber_domain::{
    EventKind, PriorityBucket, SCENARIO_SCHEMA_VERSION, ScenarioClass, ScenarioPhase,
    ScenarioProvenance, SemanticBand, StreamScenario,
};
use aivtuber_hardening::{
    FaultKind, FaultOverlay, FaultSpec, FaultSubsystem, SoakConfig, run_scenario_soak,
};
use aivtuber_telemetry::ReproducibilityMetadata;
use std::collections::BTreeMap;

fn mix<T: Ord + Copy>(entries: &[(T, f64)]) -> BTreeMap<T, f64> {
    entries.iter().copied().collect()
}

fn phase(name: &str, duration_ms: u64, events_per_minute: f64, actors: u32) -> ScenarioPhase {
    ScenarioPhase {
        name: name.to_owned(),
        duration_ms,
        events_per_minute,
        kind_mix: mix(&[(EventKind::ChatMessage, 9.0), (EventKind::GameEvent, 1.0)]),
        semantic_mix: mix(&[
            (SemanticBand::Hit, 6.0),
            (SemanticBand::NearMiss, 3.0),
            (SemanticBand::Miss, 4.0),
        ]),
        priority_mix: mix(&[
            (PriorityBucket::Low, 4.0),
            (PriorityBucket::Normal, 2.0),
            (PriorityBucket::High, 1.0),
        ]),
        distinct_actors: actors,
        distinct_topics: 8,
        repeat_viewer_probability: 0.4,
    }
}

fn scenario(id: &str, phases: Vec<ScenarioPhase>) -> StreamScenario {
    let stream_duration_ms = phases.iter().map(|phase| phase.duration_ms).sum();
    StreamScenario {
        schema_version: SCENARIO_SCHEMA_VERSION.to_owned(),
        scenario_id: id.to_owned(),
        scenario_version: 1,
        scenario_class: ScenarioClass::Burst,
        provenance: ScenarioProvenance::SyntheticDesignAssumption,
        provenance_note: "A hardening test workload; not a measurement of any channel.".to_owned(),
        seed: 20261005,
        logical_start: "2026-10-01T00:00:00Z".to_owned(),
        stream_duration_ms,
        phases,
    }
}

fn overlay(id: &str, version: u32, faults: Vec<FaultSpec>) -> FaultOverlay {
    FaultOverlay {
        schema_version: "1".to_owned(),
        scenario_id: id.to_owned(),
        scenario_version: version,
        seed: 7,
        faults,
    }
}

fn metadata() -> ReproducibilityMetadata {
    ReproducibilityMetadata {
        dataset_id: "scenario-soak-test".to_owned(),
        git_commit: "unit-test".to_owned(),
        rust_toolchain: "rustc-test".to_owned(),
        bun_toolchain: None,
        config_version: "scenario-soak-v1".to_owned(),
        asset_version: "generated-dynamic-v1".to_owned(),
        index_version: None,
        jev_model: None,
        thinking_model: None,
        tts_model: None,
        seed: 20261005,
        stream_duration_ms: Some(120_000),
    }
}

fn small_config() -> SoakConfig {
    SoakConfig {
        logical_events: 200_000,
        event_interval_ms: 50,
        ingress_queue_limit: 16,
        scheduler_history_limit: 64,
        audit_limit: 64,
        telemetry_limit: 64,
        working_memory_limit: 32,
        memory_compaction_limit: 16,
        memory_links_limit: 64,
        generated_asset_limit: 16,
        promotion_metadata_limit: 16,
        generated_asset_every: 25,
    }
}

fn burst_scenario(id: &str) -> StreamScenario {
    scenario(
        id,
        vec![
            phase("baseline", 60_000, 6.0, 12),
            phase("burst", 60_000, 40.0, 40),
        ],
    )
}

#[test]
fn the_same_scenario_and_overlay_reproduce_an_identical_report() {
    let scenario = burst_scenario("repeatable-soak");
    let overlay = overlay(
        "repeatable-soak",
        1,
        vec![
            FaultSpec {
                subsystem: FaultSubsystem::ContentIngress,
                occurrence: 1,
                kind: FaultKind::Flood,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Thinking,
                occurrence: 1,
                kind: FaultKind::Timeout,
            },
        ],
    );

    let first = run_scenario_soak(&scenario, &overlay, small_config(), metadata()).expect("first");
    let second =
        run_scenario_soak(&scenario, &overlay, small_config(), metadata()).expect("second");

    assert_eq!(
        first.to_json_pretty().unwrap(),
        second.to_json_pretty().unwrap(),
        "a recorded soak outcome must reproduce byte for byte"
    );
    assert_eq!(first.trace_events, scenario.total_event_count().unwrap());
    assert_eq!(first.scenario_dataset_id, scenario.dataset_id());
    assert_eq!(
        first.dataset_id,
        format!("{}/faults-{}", scenario.dataset_id(), first.fault_plan_id)
    );
    assert_eq!(
        first.metadata.dataset_id, first.dataset_id,
        "the reproducibility result key must carry the fault-plan identity, not just the seed"
    );
}

#[test]
fn a_different_fault_plan_changes_the_baseline_identity_even_with_the_same_seed() {
    let scenario = burst_scenario("identity-soak");
    let base = overlay(
        "identity-soak",
        1,
        vec![FaultSpec {
            subsystem: FaultSubsystem::ContentIngress,
            occurrence: 1,
            kind: FaultKind::Flood,
        }],
    );

    let run = |overlay: &FaultOverlay| {
        run_scenario_soak(&scenario, overlay, small_config(), metadata()).expect("soak")
    };
    let baseline = run(&base);

    // Same seed, different fault kind.
    let mut other_kind = base.clone();
    other_kind.faults[0].kind = FaultKind::RateLimited;
    let kind_report = run(&other_kind);
    assert_eq!(baseline.fault_seed, kind_report.fault_seed);
    assert_ne!(
        baseline.fault_plan_id, kind_report.fault_plan_id,
        "a different fault kind is a different plan"
    );
    assert_ne!(baseline.dataset_id, kind_report.dataset_id);

    // Same seed, same kind, different occurrence.
    let mut other_occurrence = base.clone();
    other_occurrence.faults[0].occurrence = 2;
    let occurrence_report = run(&other_occurrence);
    assert_ne!(baseline.fault_plan_id, occurrence_report.fault_plan_id);

    // Reordering the same faults must not change the identity.
    let mut reordered = FaultOverlay {
        faults: vec![
            FaultSpec {
                subsystem: FaultSubsystem::ContentIngress,
                occurrence: 1,
                kind: FaultKind::Flood,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Thinking,
                occurrence: 1,
                kind: FaultKind::Timeout,
            },
        ],
        ..base.clone()
    };
    let forward = reordered.clone();
    reordered.faults.reverse();
    assert_eq!(forward.plan_id().unwrap(), reordered.plan_id().unwrap());
}

#[test]
fn an_overlay_bound_to_another_scenario_identity_is_refused() {
    let scenario = burst_scenario("the-real-workload");
    let mismatched = overlay(
        "some-other-workload",
        1,
        vec![FaultSpec {
            subsystem: FaultSubsystem::ContentIngress,
            occurrence: 1,
            kind: FaultKind::Flood,
        }],
    );

    let error = run_scenario_soak(&scenario, &mismatched, small_config(), metadata())
        .expect_err("a fault plan must name the workload it was written for");
    assert!(
        error.to_string().contains("some-other-workload"),
        "the refusal names the mismatched identity, got: {error}"
    );

    let wrong_version = overlay(
        "the-real-workload",
        2,
        vec![FaultSpec {
            subsystem: FaultSubsystem::ContentIngress,
            occurrence: 1,
            kind: FaultKind::Flood,
        }],
    );
    assert!(
        run_scenario_soak(&scenario, &wrong_version, small_config(), metadata()).is_err(),
        "a version bump is a new workload, so an old overlay must not silently apply"
    );
}

#[test]
fn an_unobservable_subsystem_is_refused_rather_than_recorded() {
    let overlay = overlay(
        "the-real-workload",
        1,
        vec![FaultSpec {
            subsystem: FaultSubsystem::Jev,
            occurrence: 1,
            kind: FaultKind::Timeout,
        }],
    );
    let error = overlay
        .validate()
        .expect_err("Jev is not callable here")
        .to_string();
    assert!(
        error.contains("never calls"),
        "the refusal explains that the fault is unobservable, got: {error}"
    );
}

#[test]
fn a_fault_occurrence_beyond_the_workload_is_refused() {
    let scenario = burst_scenario("short-soak");
    let overlay = overlay(
        "short-soak",
        1,
        vec![FaultSpec {
            subsystem: FaultSubsystem::ContentIngress,
            occurrence: 9_999,
            kind: FaultKind::Flood,
        }],
    );

    let error = run_scenario_soak(&scenario, &overlay, small_config(), metadata())
        .expect_err("an out-of-range occurrence must be refused, not reported as a clean run");
    assert!(
        error.to_string().contains("only calls that subsystem"),
        "the refusal names the unreachable occurrence, got: {error}"
    );
}

#[test]
fn a_provider_fault_on_a_workload_with_no_misses_is_refused() {
    // A hit-only workload never takes the generative path, so a Thinking fault
    // could never fire. Planning one anyway would make the failure test a false
    // green.
    let mut steady = phase("steady", 60_000, 40.0, 12);
    steady.semantic_mix = mix(&[(SemanticBand::Hit, 1.0)]);
    let scenario = scenario("no-miss-workload", vec![steady]);

    let overlay = overlay(
        "no-miss-workload",
        1,
        vec![FaultSpec {
            subsystem: FaultSubsystem::Thinking,
            occurrence: 1,
            kind: FaultKind::Timeout,
        }],
    );
    let error = run_scenario_soak(&scenario, &overlay, small_config(), metadata())
        .expect_err("there is no generative call to fail");
    assert!(
        error.to_string().contains("0 time(s)"),
        "the refusal states the workload never reaches the subsystem, got: {error}"
    );
}

#[test]
fn a_window_the_core_link_probe_refuses_is_still_a_valid_scenario_run() {
    // Round-6: the core link-probe minimum (three node slots, so a window
    // can retain a link) is not a scenario-retention invariant. The
    // scenario workload only performs working-memory writes and omits the
    // link growth findings, so a two-entry window is a valid bounded
    // experiment and must not be refused by core-only rules.
    let scenario = burst_scenario("two-slot-window");
    let overlay = overlay(
        "two-slot-window",
        1,
        vec![FaultSpec {
            subsystem: FaultSubsystem::ContentIngress,
            occurrence: 1,
            kind: FaultKind::Flood,
        }],
    );
    let config = SoakConfig {
        working_memory_limit: 2,
        ..small_config()
    };
    let report = run_scenario_soak(&scenario, &overlay, config, metadata())
        .expect("a two-entry working window is a valid scenario experiment");
    assert!(
        report.final_state.working_memory_entries <= 2,
        "the small window is enforced: {:?}",
        report.final_state.working_memory_entries
    );
    assert!(
        report.finding("memory_links").is_none(),
        "scenario reports still omit link checks"
    );
}

#[test]
fn leading_and_trailing_idle_are_part_of_the_declared_timeline() {
    // The soak must start at the scenario's declared logical_start and settle to
    // the declared stream end, not normalise away the idle phases around the
    // active one - otherwise the same event list would describe a different
    // temporal workload.
    let scenario = scenario(
        "idle-burst-idle",
        vec![
            phase("lead-idle", 60_000, 0.0, 1),
            phase("active", 60_000, 20.0, 12),
            phase("trail-idle", 60_000, 0.0, 1),
        ],
    );
    let report = run_scenario_soak(
        &scenario,
        &overlay(
            "idle-burst-idle",
            1,
            vec![FaultSpec {
                subsystem: FaultSubsystem::ContentIngress,
                occurrence: 1,
                kind: FaultKind::Flood,
            }],
        ),
        small_config(),
        metadata(),
    )
    .expect("soak");

    assert_eq!(report.stream_duration_ms, 180_000);
    assert_eq!(report.trace_events, 20);
    assert!(
        report.first_event_at_ms >= 60_000,
        "the leading idle phase must be preserved, first event at {}",
        report.first_event_at_ms
    );
    assert!(
        report.last_event_at_ms <= 120_000,
        "the trailing idle phase must not be filled with events, last event at {}",
        report.last_event_at_ms
    );
    assert_eq!(report.faults.observed(), 1);
}

#[test]
fn injected_faults_degrade_and_do_not_freeze_the_deterministic_performer() {
    let scenario = burst_scenario("faulted-soak");
    let overlay = overlay(
        "faulted-soak",
        1,
        vec![
            FaultSpec {
                subsystem: FaultSubsystem::ContentIngress,
                occurrence: 1,
                kind: FaultKind::Flood,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Thinking,
                occurrence: 1,
                kind: FaultKind::Timeout,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Tts,
                occurrence: 1,
                kind: FaultKind::Unavailable,
            },
            FaultSpec {
                subsystem: FaultSubsystem::AssetStore,
                occurrence: 1,
                kind: FaultKind::Corrupted,
            },
        ],
    );

    let report = run_scenario_soak(&scenario, &overlay, small_config(), metadata()).expect("soak");

    assert!(report.faults.content_ingress_degraded >= 1);
    assert!(report.faults.generative_degraded >= 1);
    assert!(report.faults.asset_store_degraded >= 1);
    assert_eq!(report.faults.planned, 4);
    // Every planned fault is observable on this workload, so all four fire.
    assert_eq!(
        report.faults.observed(),
        4,
        "consumed={:?}",
        report.faults.consumed
    );

    // Degradation, not a freeze: the deterministic timeline drains and no
    // retained-state bound is exceeded.
    assert_eq!(report.final_state.scheduler_active, 0);
    assert!(report.final_state.scheduler_history <= report.config.scheduler_history_limit);
    for finding in &report.growth {
        assert!(
            !finding.configured_limit_exceeded,
            "{} must stay within its configured bound: {finding:?}",
            finding.metric
        );
    }
}

#[test]
fn a_long_logical_time_scenario_plateaus_at_retained_state_bounds() {
    // Two logical stream hours at a low real-time rate: long logical duration
    // without hours of wall-clock execution, which is the point of logical-time
    // scenarios. The small probe limits below are reached well inside it.
    let scenario = scenario(
        "long-retention-soak",
        vec![
            phase("morning", 60 * 60 * 1_000, 30.0, 40),
            phase("evening", 60 * 60 * 1_000, 30.0, 40),
        ],
    );
    let config = small_config();
    let report = run_scenario_soak(
        &scenario,
        &overlay(
            "long-retention-soak",
            1,
            vec![FaultSpec {
                subsystem: FaultSubsystem::ContentIngress,
                occurrence: 5,
                kind: FaultKind::RateLimited,
            }],
        ),
        config.clone(),
        metadata(),
    )
    .expect("soak");

    assert_eq!(report.trace_events, scenario.total_event_count().unwrap());
    assert_eq!(
        report.final_state.scheduler_history,
        config.scheduler_history_limit
    );
    assert_eq!(report.final_state.audit_records, config.audit_limit);
    assert_eq!(report.final_state.telemetry_events, config.telemetry_limit);
    assert_eq!(
        report.final_state.working_memory_entries,
        config.working_memory_limit
    );
    assert_eq!(
        report.final_state.memory_compaction_records,
        config.memory_compaction_limit
    );
    assert_eq!(report.final_state.hot_assets, config.generated_asset_limit);
    assert_eq!(
        report.final_state.promotion_metadata,
        config.promotion_metadata_limit
    );

    for metric in [
        "scheduler_history",
        "audit_records",
        "working_memory_entries",
        "telemetry_events",
        "hot_assets",
        "promotion_metadata",
    ] {
        let finding = report.finding(metric).expect("growth finding");
        assert!(
            !finding.lifetime_growth_detected,
            "{metric} must plateau after warm-up: {finding:?}"
        );
        assert!(
            !finding.configured_limit_exceeded,
            "{metric} must stay within its configured bound: {finding:?}"
        );
    }
}

#[test]
fn a_scenario_report_carries_no_link_bound_it_never_exercises() {
    // Round-4 finding: the scenario trace only records working memory —
    // links are minted exclusively on the permit-gated durable path, which
    // this workload never drives — so a `memory_links` growth check here
    // would read zero because the subsystem was not exercised, not because
    // it stayed bounded. Such a check cannot fail: it would pass even if
    // link creation or pruning were completely broken. The report therefore
    // omits both link metrics while the core soak still carries them, and
    // the raw counters remain visible in the snapshots as observations.
    let scenario = burst_scenario("linkless-growth-soak");
    let report = run_scenario_soak(
        &scenario,
        &overlay(
            "linkless-growth-soak",
            1,
            vec![FaultSpec {
                subsystem: FaultSubsystem::ContentIngress,
                occurrence: 1,
                kind: FaultKind::Flood,
            }],
        ),
        small_config(),
        metadata(),
    )
    .expect("soak");

    assert_eq!(report.final_state.memory_links, 0);
    assert_eq!(report.final_state.memory_links_high_water, 0);
    assert!(
        report.finding("memory_links").is_none(),
        "a bound the workload cannot move must not be reported as checked"
    );
    assert!(
        report.finding("memory_links_high_water").is_none(),
        "a bound the workload cannot move must not be reported as checked"
    );
    // The metrics this workload does exercise are still reported.
    assert!(report.finding("working_memory_entries").is_some());
}
