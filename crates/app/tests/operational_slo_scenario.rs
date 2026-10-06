//! Issue #71 windowing over real #70 scenario evidence.
//!
//! Review finding on PR #218: the windowing contract is only meaningful if the
//! offset that reaches the report is the *workload's* logical timeline. The
//! first version of this test rebuilt observations from
//! `scenario_instant_ms(...)` itself, which proved the arithmetic but never
//! exercised the path a benchmark actually takes — so a scenario replayed
//! through `replay-benchmark --scenario` was still compressed to one virtual
//! second per event, and an 80-minute workload reported as ~13 minutes.
//!
//! These tests therefore go through `scenario_to_benchmark_fixture`, the
//! conversion the runner itself uses, and assert that the offset survives it.

use aivtuber_app::{CachedReplayPlayability, StreamScenario};
use aivtuber_domain::scenario::{generate_scenario_trace, scenario_instant_ms};
use aivtuber_telemetry::{
    BenchmarkReport, ComparisonMode, EventObservation, ReproducibilityMetadata, RouteClass,
    SloEvaluationConfig, SloProvenance, SloReport, SloStatus, SloTargets, SloVerdict, WindowKind,
    evaluate,
};

fn repository_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn load(relative: &str) -> StreamScenario {
    let path = repository_path(relative);
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

/// Run the exact conversion `replay-benchmark --scenario` runs, then read the
/// offsets back out of the produced fixture document.
fn scenario_fixture_offsets(scenario: &StreamScenario) -> (Vec<u64>, u64) {
    let document = aivtuber_domain::scenario::scenario_to_benchmark_fixture(scenario)
        .expect("scenario converts to a benchmark fixture");
    let offsets: Vec<u64> = document
        .get("events")
        .and_then(|events| events.as_array())
        .expect("fixture events")
        .iter()
        .map(|event| {
            event
                .get("stream_offset_ms")
                .and_then(|offset| offset.as_u64())
                .expect("each scenario fixture event must carry stream_offset_ms")
        })
        .collect();
    let declared = document
        .get("stream_duration_ms")
        .and_then(|value| value.as_u64())
        .expect("declared stream duration");
    (offsets, declared)
}

/// Build observations from the offsets the fixture actually carries, which is
/// what the replay runner records once the fix lands.
fn report_from_offsets(
    scenario: &StreamScenario,
    offsets: &[u64],
    stream_duration_ms: u64,
) -> BenchmarkReport {
    let events: Vec<EventObservation> = offsets
        .iter()
        .enumerate()
        .map(|(index, offset)| {
            let mut observation = EventObservation::new(
                format!("{index:06}"),
                ComparisonMode::DeterministicSemantic,
                RouteClass::Deterministic,
            );
            observation.stream_offset_ms = Some(*offset);
            observation.event_to_first_visible_reaction_ms = Some(20);
            observation.event_to_first_audio_ms = Some(60);
            observation
        })
        .collect();
    BenchmarkReport::from_events(
        ReproducibilityMetadata {
            dataset_id: scenario.dataset_id(),
            git_commit: "scenario-test".to_owned(),
            rust_toolchain: "rustc 1.98.0".to_owned(),
            bun_toolchain: None,
            config_version: "bench-v1".to_owned(),
            asset_version: "starter-v1".to_owned(),
            index_version: None,
            jev_model: None,
            thinking_model: None,
            tts_model: None,
            seed: scenario.seed,
            stream_duration_ms: Some(stream_duration_ms.max(1)),
        },
        ComparisonMode::DeterministicSemantic,
        events,
    )
    .expect("benchmark report")
}

fn evaluate_scenario(report: &BenchmarkReport) -> SloReport {
    evaluate(
        report,
        &SloTargets::default(),
        SloEvaluationConfig {
            provenance: SloProvenance::ScenarioReplay,
            rolling_window_hours: 1,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report")
}

/// The fixture conversion must carry the scenario's own timeline, not a
/// per-event cadence.
#[test]
fn scenario_fixture_carries_the_scenario_timeline_not_the_replay_pacer() {
    let scenario = load("examples/scenarios/high-cardinality-actors.json");
    let origin = scenario.logical_start_ms().expect("declared logical start");
    let trace = generate_scenario_trace(&scenario).expect("scenario trace");

    let (offsets, declared) = scenario_fixture_offsets(&scenario);
    assert_eq!(offsets.len(), trace.len());

    for (entry, offset) in trace.iter().zip(&offsets) {
        let expected = scenario_instant_ms(&entry.event.observed_at)
            .expect("scenario instant")
            .saturating_sub(origin);
        assert_eq!(
            *offset, expected,
            "fixture offset must equal the event's own position on the scenario timeline"
        );
    }

    // The whole point of finding #1: the represented span must be the
    // scenario's, not `event_count * 1s`.
    let span = offsets.last().copied().unwrap_or(0);
    assert!(
        span > 3_600_000,
        "the represented span must exceed one stream hour, got {span} ms"
    );
    assert!(
        span * 100 >= declared * 99,
        "the last event sits at {span} ms of a declared {declared} ms run; the fixture lost \
         more than 1% of the timeline"
    );
    let pacer_span = (offsets.len() as u64).saturating_mul(1_000);
    assert!(
        pacer_span * 2 < declared,
        "the pacing clock ({pacer_span} ms) is not a usable stand-in for the scenario \
         timeline ({declared} ms)"
    );
}

/// An 80-minute scenario must produce represented stream-hour windows, and the
/// buckets must partition the session exactly once.
#[test]
fn an_eighty_minute_scenario_yields_stream_hour_windows() {
    let scenario = load("examples/scenarios/high-cardinality-actors.json");
    let (offsets, declared) = scenario_fixture_offsets(&scenario);
    let report = report_from_offsets(&scenario, &offsets, declared);
    let slo_report = evaluate_scenario(&report);

    let hours: Vec<_> = slo_report
        .windows
        .iter()
        .filter(|window| window.kind == WindowKind::StreamHour)
        .collect();
    assert_eq!(
        hours.len(),
        2,
        "4,800,000 ms of logical stream time is exactly two stream hours: {:?}",
        slo_report.windows
    );
    assert!(hours.iter().all(|window| window.admitted_events > 0));

    let total = report.events.len() as u64;
    let bucketed: u64 = hours.iter().map(|window| window.admitted_events).sum();
    assert_eq!(
        bucketed, total,
        "the hour buckets must partition the session exactly once"
    );
    assert_eq!(slo_report.windows[0].admitted_events, total);
    assert_eq!(slo_report.source.provenance, SloProvenance::ScenarioReplay);
    assert_eq!(slo_report.source.dataset_id, scenario.dataset_id());
}

/// A short scenario reports one window and says which of the two cases it hit.
#[test]
fn a_short_scenario_reports_one_window_and_says_why() {
    let scenario = load("examples/scenarios/chat-burst-10x.json");
    let (offsets, declared) = scenario_fixture_offsets(&scenario);
    let slo_report = evaluate_scenario(&report_from_offsets(&scenario, &offsets, declared));

    assert_eq!(slo_report.windows.len(), 1);
    assert!(
        slo_report
            .limitations
            .iter()
            .any(|limitation| limitation.contains("less than one stream hour"))
    );
}

/// Nothing is calibrated until a target file says so.
#[test]
fn nothing_is_calibrated_until_a_target_file_says_so() {
    let scenario = load("examples/scenarios/idle-to-burst.json");
    let (offsets, declared) = scenario_fixture_offsets(&scenario);
    let slo_report = evaluate_scenario(&report_from_offsets(&scenario, &offsets, declared));

    assert_eq!(slo_report.verdict, SloVerdict::Incomplete);
    assert!(slo_report.error_budgets.is_empty());
    assert!(!slo_report.uncalibrated.is_empty());
    assert!(!slo_report.latency_calibration.is_empty());
    assert!(
        slo_report
            .indicators
            .iter()
            .filter(|indicator| indicator.status == SloStatus::Uncalibrated)
            .all(|indicator| indicator.target.is_none())
    );
    // A missing value is only ever paired with an empty denominator or an
    // explicit non-passing status. It is never a silent zero.
    assert!(slo_report.indicators.iter().all(|indicator| {
        indicator.value.is_some()
            || indicator.eligible == 0
            || matches!(
                indicator.status,
                SloStatus::Uncalibrated
                    | SloStatus::NotYetMeasured
                    | SloStatus::NoData
                    | SloStatus::InsufficientSamples
            )
    }));
}

/// The checked-in target file must stay valid against this catalog and stay
/// empty until a target cites measured evidence from a representative series
/// at a sufficient denominator. The first calibration cycle ran on the
/// mock-driven smoke fixture, whose pooled denominators sat below the sample
/// floors — evidence that cannot decide an objective cannot justify a target
/// for it (docs/operational-slo.adoc, PR #228 review).
#[test]
fn the_checked_in_target_file_is_valid_and_still_empty() {
    let bytes =
        std::fs::read(repository_path("examples/slo/slo-targets.json")).expect("targets file");
    let targets = SloTargets::from_json(&bytes).expect("parse targets");
    targets
        .validate(&aivtuber_telemetry::catalog())
        .expect("targets describe this catalog");

    assert_eq!(
        targets.catalog_version,
        aivtuber_telemetry::SLO_CATALOG_VERSION
    );
    assert!(
        targets.targets.is_empty(),
        "a numeric target may only be checked in once it cites measured baseline evidence from a \
         representative series whose pooled denominators reach the sample floor"
    );
    let description = targets
        .description
        .expect("the target file must explain which contract it is not");
    assert!(
        description.contains("regression budgets"),
        "the operational target file must say how it differs from the PR regression budgets: \
         {description}"
    );
}

/// A soak-shaped workload the cached replay path cannot play still produces a
/// correct SLO report: the windowing depends on the fixture timeline, not on
/// the replay consumer's measured playability ceiling.
#[test]
fn a_scenario_the_replay_consumer_refuses_still_yields_an_slo_report() {
    let scenario = load("examples/scenarios/consumer-refused/unplayable-density.json");
    assert!(
        scenario.validate_playability_for_cached_replay().is_err(),
        "fixture must stay refused by the cached replay consumer"
    );
    let (offsets, declared) = scenario_fixture_offsets(&scenario);
    let slo_report = evaluate_scenario(&report_from_offsets(&scenario, &offsets, declared));

    assert_eq!(slo_report.source.dataset_id, scenario.dataset_id());
    assert_eq!(slo_report.verdict, SloVerdict::Incomplete);
    assert!(
        slo_report
            .windows
            .iter()
            .all(|window| window.admitted_events == offsets.len() as u64
                || window.kind == WindowKind::Session),
        "a refused-consumer workload must not lose events from its buckets"
    );
}
