//! Issue #70: versioned seeded stream scenarios.
//!
//! The properties under test are the ones that make a shared workload
//! trustworthy. If any of them fails, every downstream number computed from a
//! scenario is quietly unfounded:
//!
//! 1. **Reproducibility.** The same scenario must yield a byte-identical ordered
//!    trace, or a recorded baseline cannot be compared against anything.
//! 2. **A version bump reshuffles the trace**, so an incompatible scenario
//!    change starts a new comparison baseline instead of reusing an old one.
//! 3. **The trace is ordered and well-formed.** Strictly increasing timestamps
//!    and contiguous sequence numbers, every event satisfying the domain
//!    contract. A trace that is merely *a* list is not a stream.
//! 4. **The scenario cannot misrepresent itself.** Provenance is required, a
//!    scenario that generates nothing is refused, and nothing synthesizable
//!    carries an authorization claim or viewer prose.

use aivtuber_app::{
    MAX_EVENTS_PER_MINUTE, PriorityBucket, SCENARIO_SCHEMA_VERSION, ScenarioClass, ScenarioEvent,
    ScenarioPhase, ScenarioProvenance, SemanticBand, StreamScenario, generate_scenario_trace,
    scenario_to_benchmark_fixture,
};
use aivtuber_domain::{EventEnvelope, EventKind};
use std::collections::BTreeMap;

fn mix<T: Ord + Copy>(entries: &[(T, f64)]) -> BTreeMap<T, f64> {
    entries.iter().copied().collect()
}

fn steady_phase(name: &str, duration_ms: u64, events_per_minute: f64) -> ScenarioPhase {
    ScenarioPhase {
        name: name.to_owned(),
        duration_ms,
        events_per_minute,
        kind_mix: mix(&[
            (EventKind::ChatMessage, 8.0),
            (EventKind::GameEvent, 1.0),
            (EventKind::TimerTick, 1.0),
        ]),
        semantic_mix: mix(&[
            (SemanticBand::Hit, 0.6),
            (SemanticBand::NearMiss, 0.3),
            (SemanticBand::Miss, 0.1),
        ]),
        priority_mix: mix(&[(PriorityBucket::Low, 1.0), (PriorityBucket::High, 0.2)]),
        distinct_actors: 12,
        distinct_topics: 8,
        repeat_viewer_probability: 0.4,
    }
}

fn scenario() -> StreamScenario {
    StreamScenario {
        schema_version: SCENARIO_SCHEMA_VERSION.to_owned(),
        scenario_id: "test-mixed-stream".to_owned(),
        scenario_version: 1,
        scenario_class: ScenarioClass::NormalMixed,
        provenance: ScenarioProvenance::SyntheticDesignAssumption,
        provenance_note: "A unit-test workload; not a measurement of any channel.".to_owned(),
        seed: 20261004,
        logical_start: "2026-10-01T00:00:00Z".to_owned(),
        stream_duration_ms: 120_000,
        phases: vec![
            steady_phase("steady", 60_000, 6.0),
            steady_phase("burst", 60_000, 10.0),
        ],
    }
}

fn trace_of(scenario: &StreamScenario) -> Vec<ScenarioEvent> {
    generate_scenario_trace(scenario).expect("the scenario is valid")
}

#[test]
fn the_same_scenario_reproduces_an_identical_ordered_trace() {
    let scenario = scenario();

    let first = trace_of(&scenario);
    let second = trace_of(&scenario);

    assert_eq!(first.len() as u64, scenario.total_event_count().unwrap());
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap(),
        "a recorded workload must reproduce byte for byte, or its baseline means nothing"
    );
}

#[test]
fn a_different_seed_changes_the_trace_but_not_its_length() {
    let mut reseeded = scenario();
    let original = trace_of(&scenario());

    reseeded.seed = scenario().seed.wrapping_add(1);
    let reseeded_trace = trace_of(&reseeded);

    assert_eq!(
        original.len(),
        reseeded_trace.len(),
        "the seed chooses the draw, not the size of the stream"
    );
    assert_ne!(
        serde_json::to_value(&original).unwrap(),
        serde_json::to_value(&reseeded_trace).unwrap(),
        "two seeds must not produce the same stream"
    );
}

#[test]
fn a_version_bump_starts_a_new_comparison_baseline() {
    let original = scenario();
    let mut bumped = scenario();
    bumped.scenario_version = 2;

    assert_ne!(
        original.dataset_id(),
        bumped.dataset_id(),
        "the dataset identity carries the version, so the gate refuses a cross-version comparison"
    );
    assert_ne!(
        serde_json::to_value(trace_of(&original)).unwrap(),
        serde_json::to_value(trace_of(&bumped)).unwrap(),
        "a version bump reshuffles the trace instead of silently reusing the old workload"
    );
}

#[test]
fn the_trace_is_ordered_and_every_event_satisfies_the_domain_contract() {
    let entries = trace_of(&scenario());
    assert!(entries.len() > 1, "the fixture must generate a real stream");

    let mut previous_instant: Option<String> = None;
    let mut previous_event_id = String::new();
    for (index, entry) in entries.iter().enumerate() {
        let event: &EventEnvelope = &entry.event;
        event.validate().unwrap_or_else(|error| {
            panic!("generated event {} is invalid: {error}", event.event_id)
        });

        assert_eq!(
            event.sequence,
            index as u64 + 1,
            "sequence numbers must be contiguous from 1, or a consumer cannot detect a dropped event"
        );
        assert_ne!(
            event.event_id, previous_event_id,
            "event ids must be unique"
        );
        assert!(
            event.correlation_id.starts_with("test-mixed-stream"),
            "events from one scenario share a correlation id"
        );
        assert_eq!(
            entry.query_embedding.len(),
            aivtuber_app::SCENARIO_EMBEDDING_DIMENSION,
            "the embedding must be in the starter semantic space the benchmark indexes"
        );

        // Millisecond precision is part of the contract, not decoration: slots
        // are one millisecond apart, so rounding to whole seconds would let two
        // events share a timestamp string and hide a reordering.
        let fraction = event.observed_at.get(19..).unwrap_or_default();
        assert!(
            event.observed_at.ends_with('Z')
                && fraction.len() == 5
                && fraction.starts_with('.')
                && fraction[1..4].bytes().all(|byte| byte.is_ascii_digit()),
            "observed_at must carry exactly three fractional digits: {}",
            event.observed_at
        );

        if let Some(previous) = previous_instant {
            assert!(
                event.observed_at.as_str() > previous.as_str(),
                "observed_at must increase strictly: {previous} then {}",
                event.observed_at
            );
        }
        previous_instant = Some(event.observed_at.clone());
        previous_event_id = event.event_id.clone();
    }
}

#[test]
fn generated_events_cannot_carry_viewer_prose() {
    // A checked-in workload must never require a real transcript, and the way to
    // guarantee that is structurally: the payload has a fixed key set.
    for entry in trace_of(&scenario()) {
        let keys: Vec<&str> = entry.event.payload.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["intent", "scenario_phase", "semantic_band", "topic_id"],
            "a synthetic event carries labels only, so no scenario can smuggle prose"
        );
    }
}

#[test]
fn the_semantic_band_is_carried_by_the_embedding_not_the_intent() {
    // `payload.intent` must always name a real variant group, because cached
    // playback uses it directly. So the band is expressed through the query
    // embedding: a hit is the axis itself, a near-miss keeps most of the signal
    // without being a clean match, and a miss carries none of it.
    let axis_count = |entry: &ScenarioEvent| {
        entry
            .query_embedding
            .iter()
            .filter(|value| **value > 0.5)
            .count()
    };
    let is_uniform = |entry: &ScenarioEvent| {
        entry
            .query_embedding
            .iter()
            .all(|value| (*value - 1.0 / 3.0).abs() < 1e-6)
    };

    let banded = |band: SemanticBand| {
        let mut phase = steady_phase("banded", 60_000, 10.0);
        phase.semantic_mix = mix(&[(band, 1.0)]);
        generate_scenario_trace(&StreamScenario {
            phases: vec![phase.clone()],
            stream_duration_ms: phase.duration_ms,
            ..scenario()
        })
        .expect("valid")
    };

    for entry in banded(SemanticBand::Hit) {
        let strongest = entry
            .query_embedding
            .iter()
            .copied()
            .fold(f32::MIN, f32::max);
        assert!(
            strongest > 0.99,
            "a hit is the axis itself, not a blend: {:?}",
            entry.query_embedding
        );
        assert_eq!(
            axis_count(&entry),
            1,
            "a hit resolves onto exactly one indexed axis"
        );
        assert!(!is_uniform(&entry), "a hit is a direction, not the spread");
    }

    for entry in banded(SemanticBand::Miss) {
        assert_eq!(
            axis_count(&entry),
            0,
            "a miss must not resolve onto an indexed axis"
        );
        assert!(
            is_uniform(&entry),
            "a miss carries no axis signal at all: {:?}",
            entry.query_embedding
        );
    }

    for entry in banded(SemanticBand::NearMiss) {
        // A near-miss keeps most of its axis, so it is still recognisably the
        // same direction, but the spread means it is no longer the clean axis
        // itself. That is the whole difference from a hit, and from a miss.
        assert!(
            axis_count(&entry) >= 1,
            "a near-miss keeps its axis: {:?}",
            entry.query_embedding
        );
        assert!(
            entry.query_embedding.iter().all(|value| *value > 0.0),
            "a near-miss is not the bare axis, so every component carries spread: {:?}",
            entry.query_embedding
        );
        assert!(
            !is_uniform(&entry),
            "a near-miss still carries signal, so it is not a miss: {:?}",
            entry.query_embedding
        );
    }
}

#[test]
fn an_idle_phase_contributes_nothing_and_a_burst_phase_carries_the_stream() {
    let mut idle = steady_phase("idle", 60_000, 0.0);
    idle.kind_mix = mix(&[(EventKind::ChatMessage, 1.0)]);
    let mut burst = steady_phase("burst", 60_000, 10.0);
    burst.kind_mix = mix(&[(EventKind::ChatMessage, 1.0)]);

    let trace = generate_scenario_trace(&StreamScenario {
        phases: vec![idle, burst],
        stream_duration_ms: 120_000,
        ..scenario()
    })
    .expect("valid");

    assert_eq!(trace.len(), 10);
    assert!(
        trace
            .iter()
            .all(|entry| entry.event.payload["scenario_phase"].as_str() == Some("burst")),
        "an idle phase must not manufacture events"
    );
}

#[test]
fn a_scenario_whose_phases_do_not_cover_its_duration_is_refused() {
    let mut mismatched = scenario();
    mismatched.stream_duration_ms = 300_000;

    let error = generate_scenario_trace(&mismatched).expect_err("durations must add up");
    assert!(
        error.to_string().contains("stream_duration_ms"),
        "the refusal names the disagreement, got: {error}"
    );
}

#[test]
fn a_scenario_that_generates_nothing_is_refused() {
    let mut empty = scenario();
    for phase in &mut empty.phases {
        phase.events_per_minute = 0.0;
    }

    let error = generate_scenario_trace(&empty).expect_err("an empty workload benchmarks nothing");
    assert!(
        error.to_string().contains("no events"),
        "the refusal explains why, got: {error}"
    );
}

#[test]
fn operator_commands_are_refused_rather_than_fabricated() {
    let mut privileged = scenario();
    privileged.phases[0].kind_mix = mix(&[(EventKind::OperatorCommand, 1.0)]);

    let error = generate_scenario_trace(&privileged).expect_err("no synthetic authorization");
    assert!(
        error.to_string().contains("operator.command"),
        "the refusal names the kind, got: {error}"
    );
}

#[test]
fn a_scenario_without_a_provenance_note_is_refused() {
    let mut unattributed = scenario();
    unattributed.provenance_note = "   ".to_owned();

    let error = generate_scenario_trace(&unattributed).expect_err("unattributed parameters");
    assert!(
        error.to_string().contains("provenance_note"),
        "a workload must always state what its parameters are, got: {error}"
    );
}

#[test]
fn an_incompatible_schema_version_is_refused_rather_than_reinterpreted() {
    let mut future = scenario();
    future.schema_version = "2".to_owned();

    let error = generate_scenario_trace(&future).expect_err("unknown contract version");
    assert!(
        error.to_string().contains("new comparison baseline"),
        "the refusal explains that this is a baseline change, got: {error}"
    );
}

#[test]
fn a_rate_beyond_the_timeline_ceiling_is_refused_by_name() {
    // The timeline can only hold one event per millisecond, which caps a phase
    // at 60000 events/minute. That ceiling sits far above the *playable* one, so
    // the two guards are distinct and both must hold.
    let mut impossible = scenario();
    impossible.phases[0].events_per_minute = MAX_EVENTS_PER_MINUTE + 1.0;

    let error = generate_scenario_trace(&impossible).expect_err("beyond the timeline");
    assert!(
        error.to_string().contains("events_per_minute"),
        "the refusal names the offending field, got: {error}"
    );
}

#[test]
fn the_benchmark_fixture_carries_the_scenario_identity_and_the_same_events() {
    let scenario = scenario();

    let fixture = scenario_to_benchmark_fixture(&scenario).expect("fixture");
    let trace = trace_of(&scenario);

    assert_eq!(
        fixture["dataset_id"].as_str().unwrap(),
        scenario.dataset_id(),
        "the #58 result must name the scenario and its version"
    );
    assert_eq!(fixture["seed"].as_u64().unwrap(), scenario.seed);
    assert_eq!(
        fixture["stream_duration_ms"].as_u64().unwrap(),
        scenario.stream_duration_ms
    );
    assert_eq!(
        fixture["events"].as_array().unwrap().len(),
        trace.len(),
        "the benchmark consumes the generated stream, not a copy of it"
    );
    assert_eq!(
        fixture["events"][0]["event"],
        serde_json::to_value(&trace[0].event).unwrap()
    );
    assert_eq!(
        fixture["events"][0]["query_embedding"],
        serde_json::to_value(&trace[0].query_embedding).unwrap()
    );
}

#[test]
fn an_injection_style_start_time_is_refused() {
    // A scenario's start is part of its identity, so a document that smuggles a
    // zone offset past a lenient parser must fail loudly instead of generating
    // a trace under a different instant than it declares.
    for start in [
        "2026-10-01T00:00:00.500Z",
        "2026-10-01T00:00:00+09:00",
        "2026-10-01 00:00:00Z",
        "2026-13-01T00:00:00Z",
    ] {
        let mut sloppy = scenario();
        sloppy.logical_start = start.to_owned();
        assert!(
            generate_scenario_trace(&sloppy).is_err(),
            "{start} must not be accepted as a logical start"
        );
    }
}
fn shipped(name: &str) -> StreamScenario {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/scenarios")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {name}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parsing {name}: {error}"))
}

#[test]
fn every_shipped_scenario_loads_validates_and_generates() {
    // The checked-in corpus is the shared workload for #58, soak, retention and
    // quality. A scenario that does not validate, or that generates a trace the
    // runtime cannot represent, must fail here rather than in a benchmark job.
    for name in [
        "normal-mixed-stream.json",
        "chat-burst-10x.json",
        "idle-to-burst.json",
        "high-cardinality-actors.json",
        "high-generative-miss.json",
    ] {
        let scenario = shipped(name);
        scenario
            .validate()
            .unwrap_or_else(|error| panic!("{name} must validate: {error}"));
        assert!(
            scenario.total_event_count().expect("event count") > 0,
            "{name} must generate something"
        );
        let trace = trace_of(&scenario);
        assert_eq!(trace.len() as u64, scenario.total_event_count().unwrap());
        assert!(
            trace.len() > 1,
            "{name} must describe a stream, not a single event"
        );
        assert!(
            scenario.dataset_id().starts_with("scenario-"),
            "{name} must carry its identity into the benchmark dataset id"
        );
    }
}

#[test]
fn a_workload_denser_than_the_runtime_can_play_is_refused() {
    // Measured, not assumed: the starter pack completes a mixed workload at 10
    // events/minute and rejects the overlap at 15. Shipping a denser scenario
    // would mean a benchmark job that aborts instead of a scenario that says so.
    let mut dense = scenario();
    dense.phases[0].events_per_minute = aivtuber_app::MAX_PLAYABLE_EVENTS_PER_MINUTE + 1.0;

    let error = generate_scenario_trace(&dense).expect_err("unplayable density");
    assert!(
        error.to_string().contains("the starter pack can play"),
        "the refusal names the measured limit, got: {error}"
    );
}

#[test]
fn a_workload_that_collapses_onto_one_asset_is_visible_rather_than_hidden() {
    // A high-reuse workload resolves many events to the same variant group. That
    // is exactly the shape the cached path rejects on cooldown, so the scenario
    // has to keep its reuse fraction visible instead of quietly absorbing it.
    let mut reuse = scenario();
    for phase in &mut reuse.phases {
        phase.semantic_mix = mix(&[(SemanticBand::Hit, 1.0)]);
        phase.events_per_minute = aivtuber_app::MAX_PLAYABLE_EVENTS_PER_MINUTE;
    }

    let trace = trace_of(&reuse);
    let hits = trace
        .iter()
        .filter(|entry| entry.event.payload["semantic_band"] == "hit")
        .count();
    assert_eq!(
        hits,
        trace.len(),
        "every event is a hit, so the reuse pressure is the whole workload"
    );
}
