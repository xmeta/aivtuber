//! Issue #71 against #70 evidence: the operational SLO windowing must work on a
//! real versioned scenario timeline, and the checked-in target file must stay
//! honest about having nothing calibrated yet.

use aivtuber_app::{CachedReplayPlayability, StreamScenario};
use aivtuber_domain::scenario::{generate_scenario_trace, scenario_instant_ms};
use aivtuber_telemetry::{
    BenchmarkReport, ComparisonMode, EventObservation, ReproducibilityMetadata, RouteClass,
    SloProvenance, SloReport, SloStatus, SloTargets, SloVerdict, WindowKind, evaluate,
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

/// Turn a generated #70 trace into the bounded observation set a replay
/// benchmark produces, using the scenario's own logical timeline.
///
/// The offsets are the scenario's `observed_at` instants relative to the first
/// event, which is exactly what the soak consumer records today. The replay
/// benchmark packs events on its own 1-per-virtual-second clock instead, so
/// this test is the check that the windowing contract holds for the scenario's
/// real phase timing and not only for the benchmark's pacing.
fn observations(scenario: &StreamScenario) -> (Vec<EventObservation>, u64) {
    let trace = generate_scenario_trace(scenario).expect("scenario trace");
    let origin = scenario.logical_start_ms().expect("declared logical start");
    let events: Vec<EventObservation> = trace
        .iter()
        .map(|entry| {
            let at_ms = scenario_instant_ms(&entry.event.observed_at).expect("scenario instant");
            let mut observation = EventObservation::new(
                entry.event.event_id.clone(),
                ComparisonMode::DeterministicSemantic,
                RouteClass::Deterministic,
            );
            observation.stream_offset_ms = Some(at_ms.saturating_sub(origin));
            observation.event_to_first_visible_reaction_ms = Some(20);
            observation.event_to_first_audio_ms = Some(60);
            observation
        })
        .collect();
    let stream_duration_ms = trace
        .iter()
        .filter_map(|entry| scenario_instant_ms(&entry.event.observed_at).ok())
        .max()
        .unwrap_or(origin)
        .saturating_sub(origin);
    (events, stream_duration_ms)
}

fn report(scenario: &StreamScenario) -> BenchmarkReport {
    let (events, stream_duration_ms) = observations(scenario);
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

#[test]
fn a_real_scenario_timeline_produces_stream_hour_windows() {
    let scenario = load("examples/scenarios/high-cardinality-actors.json");
    let report = report(&scenario);
    let slo_report = evaluate(
        &report,
        &SloTargets::default(),
        aivtuber_telemetry::SloEvaluationConfig {
            provenance: SloProvenance::ScenarioReplay,
            rolling_window_hours: 1,
            ..Default::default()
        },
    )
    .expect("SLO report");

    let hours: Vec<_> = slo_report
        .windows
        .iter()
        .filter(|window| window.kind == WindowKind::StreamHour)
        .collect();
    assert!(
        hours.len() >= 2,
        "an 80-minute scenario must produce more than one stream hour: {:?}",
        slo_report.windows
    );

    let total = report.events.len() as u64;
    let bucketed: u64 = hours.iter().map(|window| window.admitted_events).sum();
    assert_eq!(
        bucketed, total,
        "the hour buckets must partition the session exactly once"
    );
    assert_eq!(slo_report.windows[0].admitted_events, total);
    assert!(hours.iter().all(|window| window.admitted_events > 0));
    assert_eq!(slo_report.source.provenance, SloProvenance::ScenarioReplay);
    assert_eq!(slo_report.source.dataset_id, scenario.dataset_id());
}

#[test]
fn a_short_scenario_reports_one_window_and_says_why() {
    let scenario = load("examples/scenarios/chat-burst-10x.json");
    let slo_report = evaluate(
        &report(&scenario),
        &SloTargets::default(),
        aivtuber_telemetry::SloEvaluationConfig {
            provenance: SloProvenance::ScenarioReplay,
            ..Default::default()
        },
    )
    .expect("SLO report");

    assert_eq!(slo_report.windows.len(), 1);
    assert!(
        slo_report
            .limitations
            .iter()
            .any(|limitation| limitation.contains("less than one stream hour"))
    );
}

#[test]
fn nothing_is_calibrated_until_a_target_file_says_so() {
    let scenario = load("examples/scenarios/idle-to-burst.json");
    let slo_report: SloReport = evaluate(
        &report(&scenario),
        &SloTargets::default(),
        aivtuber_telemetry::SloEvaluationConfig {
            provenance: SloProvenance::ScenarioReplay,
            ..Default::default()
        },
    )
    .expect("SLO report");

    // #71's own non-goal: no aspirational number may appear because an
    // indicator happens to be measurable.
    assert_eq!(slo_report.verdict, SloVerdict::Uncalibrated);
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
    // explicit uncalibrated/unmeasured status. It is never a silent zero.
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
    // A latency objective with samples but no calibrated threshold reports its
    // eligible set and its percentiles, and no ratio.
    let audio = slo_report
        .indicators
        .iter()
        .find(|indicator| indicator.id == "availability.event_to_first_audio_within_target")
        .expect("audio indicator");
    if audio.eligible > 0 {
        assert_eq!(audio.status, SloStatus::Uncalibrated);
        assert_eq!(audio.value, None);
    }
}

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
        "a numeric target may only be checked in once it cites measured baseline evidence"
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

#[test]
fn a_scenario_the_replay_consumer_refuses_still_yields_an_slo_report() {
    // The soak consumer plays workloads the cached replay path cannot, and an
    // SLO report over a soak-shaped timeline must not depend on the replay
    // path's measured ceiling.
    let scenario = load("examples/scenarios/consumer-refused/unplayable-density.json");
    assert!(
        scenario.validate_playability_for_cached_replay().is_err(),
        "fixture must stay refused by the cached replay consumer"
    );
    let slo_report = evaluate(
        &report(&scenario),
        &SloTargets::default(),
        aivtuber_telemetry::SloEvaluationConfig {
            provenance: SloProvenance::ScenarioReplay,
            ..Default::default()
        },
    )
    .expect("SLO report");
    assert_eq!(slo_report.source.dataset_id, scenario.dataset_id());
    assert_eq!(slo_report.verdict, SloVerdict::Uncalibrated);
}

/// The playability check is a trait in this crate; keep the import honest.
fn _playability_bound_is_a_separate_contract(scenario: &StreamScenario) -> bool {
    scenario.validate_playability_for_cached_replay().is_err()
}
