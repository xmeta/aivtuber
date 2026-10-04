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

/// Unique actors a `high_cardinality` scenario has to materialise for its label to
/// mean anything.
///
/// Declared here rather than read from `scripts/validate.mjs`, which holds the
/// same number for the same reason: a scenario that declares a large pool over too
/// short a stream reaches a fraction of it, so both sides judge the workload the
/// trace will contain rather than the rates on paper.
const HIGH_CARDINALITY_MIN_ACTORS: usize = 500;

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
        "2026-00-01T00:00:00Z",
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
fn a_workload_denser_than_the_runtime_can_play_is_a_consumer_refusal_not_a_format_error() {
    // Measured, not assumed: the starter pack completes a mixed workload at 10
    // events/minute and rejects the overlap at 15. The scenario contract must not
    // encode that number, because #70 exists so soak, retention and stress
    // consumers share this format and they do not share this ceiling - so a dense
    // scenario has to be a perfectly good document that only the replay path
    // refuses.
    let mut dense = scenario();
    dense.phases[0].events_per_minute = aivtuber_app::MAX_PLAYABLE_EVENTS_PER_MINUTE + 1.0;

    dense
        .validate()
        .unwrap_or_else(|error| panic!("the format must accept a dense workload: {error}"));
    let trace = generate_scenario_trace(&dense).expect("the generator must accept it too");
    assert!(
        trace.len() > 1,
        "a dense scenario still describes a stream, so a consumer with a higher ceiling can run it"
    );

    let error = dense
        .validate_playability_for_cached_replay()
        .expect_err("the cached replay path cannot play it");
    let message = error.to_string();
    assert!(
        message.contains("the cached replay path can play"),
        "the refusal names the measured limit and which consumer it belongs to, got: {message}"
    );
    assert!(
        message.contains("may still run it"),
        "the refusal must make clear the document is not defective, got: {message}"
    );
}

#[test]
fn a_playable_scenario_passes_the_cached_replay_check() {
    let playable = scenario();
    playable
        .validate_playability_for_cached_replay()
        .unwrap_or_else(|error| panic!("the shipped corpus must be playable: {error}"));
}

#[test]
fn the_cached_replay_check_still_refuses_a_malformed_scenario() {
    // `replay-benchmark` calls the consumer check and nothing else, so it has to
    // run the format contract itself. Otherwise a malformed document would pass
    // the consumer's gate and fail later, in the middle of a benchmark job.
    let mut malformed = scenario();
    malformed.phases[0].kind_mix = mix(&[(EventKind::OperatorCommand, 1.0)]);
    malformed.phases[0].events_per_minute = aivtuber_app::MAX_PLAYABLE_EVENTS_PER_MINUTE + 1.0;

    let error = malformed
        .validate_playability_for_cached_replay()
        .expect_err("the format defect must be reported, not skipped");
    assert!(
        error.to_string().contains("operator.command"),
        "the consumer check must surface the format error rather than the rate, got: {error}"
    );
}

#[test]
fn a_consumer_refused_scenario_is_still_a_valid_scenario_document() {
    // `examples/scenarios/consumer-refused/` exists to pin the boundary between
    // the shared format and one consumer's ceiling. Each case must be schema-clean
    // and field-clean, and refused *only* by the consumer check - otherwise
    // "we separated the concerns" would just mean the corpus moved the problem.
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/scenarios/consumer-refused");
    let mut checked = 0;
    for entry in std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("reading the consumer-refused corpus: {error}"))
    {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a readable fixture");
        let scenario: StreamScenario =
            serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path:?}: {error}"));
        let label = path.file_name().unwrap().to_string_lossy().to_string();

        scenario.validate().unwrap_or_else(|error| {
            panic!("{label}: a consumer refusal is not a format defect, got: {error}")
        });
        generate_scenario_trace(&scenario).unwrap_or_else(|error| {
            panic!("{label}: the generator must accept what the format accepts: {error}")
        });
        let error = scenario
            .validate_playability_for_cached_replay()
            .err()
            .unwrap_or_else(|| {
                panic!("{label}: this corpus exists to be refused by the consumer, but it passed")
            });
        assert!(
            error
                .to_string()
                .contains("the cached replay path can play"),
            "{label}: the refusal must name the consumer's measured limit, got: {error}"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "the consumer-refused corpus must not be empty, or the separation is untested"
    );
}

#[test]
fn an_impossible_calendar_date_is_refused_rather_than_normalised() {
    // `days_from_civil` accepts any day up to 31, so `2026-02-31` used to validate
    // and silently generate a trace starting 2026-03-03. The document would then
    // disagree with the workload it produced, which is the whole reason the start
    // instant is declared rather than read from the clock.
    for impossible in [
        "2026-02-31T00:00:00Z",
        "2026-04-31T00:00:00Z",
        "2026-02-29T00:00:00Z",
        "2026-06-31T00:00:00Z",
        "2025-02-29T00:00:00Z",
    ] {
        let mut sloppy = scenario();
        sloppy.logical_start = impossible.to_owned();
        let error = generate_scenario_trace(&sloppy)
            .expect_err("a date the calendar does not have must not be accepted");
        assert!(
            error.to_string().contains("calendar does not have"),
            "{impossible} must be refused by name, got: {error}"
        );
    }

    // Leap days that really exist still work, so the check is not simply
    // "reject anything interesting".
    // Every one of these is load-bearing: each is asserted against the generated
    // trace, so a check that over-rejects (a leap year refused, or December 31st
    // treated as out of range) fails here rather than quietly narrowing what a
    // scenario is allowed to declare.
    for real in [
        "2024-02-29T00:00:00Z",
        "2000-02-29T00:00:00Z",
        "2026-12-31T00:00:00Z",
        "2026-03-01T00:00:00Z",
    ] {
        let mut fine = scenario();
        fine.logical_start = real.to_owned();
        let trace = generate_scenario_trace(&fine).unwrap_or_else(|error| {
            panic!("{real} is a real instant and must be accepted: {error}")
        });
        assert!(
            trace[0].event.observed_at.starts_with(&real[..10]),
            "{real}: the trace must start on the declared date, got {}",
            trace[0].event.observed_at
        );
        // And the whole stream must stay inside the declared day, so a
        // mis-parsed start cannot drift into the next one either.
        assert!(
            trace[trace.len() - 1]
                .event
                .observed_at
                .starts_with(&real[..10]),
            "{real}: the last event must still be on the declared date, got {}",
            trace[trace.len() - 1].event.observed_at
        );
    }
}

#[test]
fn a_declared_actor_pool_is_not_the_claim_the_class_makes() {
    // The generator draws viewers from the declared pool, so the cardinality a
    // `high_cardinality` workload actually exercises is the number of *distinct*
    // actors on its timeline - bounded by the event count, not by the pool size.
    // A short scenario with a huge pool produces a trace with no crowd in it, and
    // a count measured on that trace would describe nothing.
    let distinct_actors = |scenario: &StreamScenario| {
        let trace = trace_of(scenario);
        let mut actors: Vec<&str> = trace
            .iter()
            .filter_map(|entry| entry.event.actor_id.as_deref())
            .collect();
        actors.sort_unstable();
        actors.dedup();
        assert!(
            actors.len()
                <= scenario
                    .phases
                    .iter()
                    .map(|phase| phase.distinct_actors as usize)
                    .sum::<usize>(),
            "distinct actors can never exceed the sum of the declared pools, because \
             actor ids come from a pool and no id is invented"
        );
        actors.len()
    };

    let crowd = shipped("high-cardinality-actors.json");
    assert!(
        distinct_actors(&crowd) >= HIGH_CARDINALITY_MIN_ACTORS,
        "the shipped high-cardinality workload must actually put {HIGH_CARDINALITY_MIN_ACTORS} distinct actors on its timeline, or it does not test cardinality"
    );

    // The same parameters over a shorter stream reach a fraction of the pool, and
    // the pool alone would have hidden it.
    let mut truncated = shipped("high-cardinality-actors.json");
    truncated.phases.truncate(1);
    truncated.phases[0].duration_ms = 120_000;
    truncated.stream_duration_ms = 120_000;
    assert!(
        distinct_actors(&truncated) < HIGH_CARDINALITY_MIN_ACTORS,
        "two minutes of the same pool cannot reach the cardinality eighty minutes does; this is why the duration is part of the claim"
    );

    // And the mirror image: a long stream drawn from a tiny pool repeats the same
    // dozen actors no matter how many events it produces. Without a case that
    // isolates this, a check on the pool bound could be dropped unnoticed while the
    // event-count check kept passing.
    let mut narrow = shipped("high-cardinality-actors.json");
    for phase in &mut narrow.phases {
        phase.distinct_actors = 40;
        phase.repeat_viewer_probability = 0.0;
    }
    assert!(
        narrow.total_event_count().unwrap() >= HIGH_CARDINALITY_MIN_ACTORS as u64,
        "this case must supply plenty of events, so only the pool bound can reject it"
    );
    assert!(
        distinct_actors(&narrow) < HIGH_CARDINALITY_MIN_ACTORS,
        "a pool of 40 cannot produce 500 distinct viewers however long the stream runs"
    );
}

/// The property the `high_cardinality` predicate exists to guarantee, asserted
/// directly against generated traces rather than against a pinned number.
///
/// A hardcoded count only proves the corpus did not change. This proves the
/// *rule* holds: for any document the validator accepts, the trace it generates
/// really does put at least `HIGH_CARDINALITY_MIN_ACTORS` distinct viewers on the
/// timeline. The bound `min(pool, non-repeated events)` is what
/// `SCENARIO_CLASS_PREDICATES.high_cardinality` applies to a document, so the two
/// implementations of the same rule cannot drift apart without this failing.
#[test]
fn an_exhausted_pool_repeats_viewers_instead_of_inventing_new_ones() {
    // Once a phase has walked its whole pool there is nobody left to arrive, so
    // further draws have to fall back to sampling *within* the pool. That branch
    // is the only place actor ids can leave the pool, and it is what keeps a long
    // stream over a small crowd from silently widening the crowd.
    let mut phase = steady_phase("long", 1_200_000, 10.0);
    phase.distinct_actors = 20;
    phase.repeat_viewer_probability = 0.0;
    // A second phase with a *different* pool size, so its fallback draw lands at a
    // non-zero actor offset. With two equal-sized pools, dropping that offset
    // would reuse the first phase's ids and the distinct count would still come
    // out right, which is exactly the bug this shape has to catch.
    let mut later = steady_phase("later", 1_200_000, 10.0);
    later.distinct_actors = 30;
    later.repeat_viewer_probability = 0.0;
    let scenario = StreamScenario {
        phases: vec![phase.clone(), later.clone()],
        stream_duration_ms: phase.duration_ms + later.duration_ms,
        ..shipped("high-cardinality-actors.json")
    };

    let events = scenario.total_event_count().unwrap();
    assert!(
        events > 50,
        "this case must outlast both pools, or the fallback branch is never reached: {events} events"
    );

    let trace = trace_of(&scenario);
    let mut actors: Vec<&str> = trace
        .iter()
        .filter_map(|entry| entry.event.actor_id.as_deref())
        .collect();
    let total = actors.len();
    actors.sort_unstable();
    actors.dedup();

    assert_eq!(
        actors.len(),
        50,
        "every viewer must come from a declared pool, however long the stream runs"
    );
    assert!(
        total > actors.len(),
        "the pools must actually have been exhausted ({total} events, {} distinct), \
         otherwise this case does not exercise the fallback",
        actors.len()
    );
}

/// The actor walk must not consume a different number of values from the PRNG.
///
/// Actor assignment sits between the draws that pick an event's kind, semantic
/// band, priority and intent. Consuming a different number of values there
/// reshuffles everything after it, which silently changes an unrelated
/// scenario's trace - and that is how `idle-to-burst`, a fixture that benchmarks
/// end to end, stopped playing after an unrelated change to cardinality. These
/// digests pin the traces so that failure is loud.
#[test]
fn the_trace_is_stable_against_unrelated_generation_changes() {
    let digests = |scenario: &StreamScenario| -> Vec<String> {
        trace_of(scenario)
            .iter()
            .map(|entry| {
                let payload = |key: &str| {
                    entry
                        .event
                        .payload
                        .get(key)
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_owned()
                };
                format!(
                    "{}|{}|{:?}|{}|{}|{}",
                    entry.event.event_id,
                    entry.event.observed_at,
                    entry.event.kind,
                    payload("intent"),
                    payload("topic_id"),
                    entry.event.actor_id.as_deref().unwrap_or_default(),
                )
            })
            .collect()
    };

    // Both fixtures the benchmark is expected to run. A change to any of these
    // means the workload changed and the measured numbers are no longer
    // comparable, so it has to be a deliberate act with a version bump.
    // Pinned field-by-field. Generated, not guessed: run once on a correct tree and
    // keep the values. The point is that any change to the number of values the
    // actor draw takes from the PRNG moves these, because that draw sits between
    // the ones choosing each event's intent and topic.
    for (name, expected_first, expected_last) in [
        (
            "idle-to-burst.json",
            "idle-to-burst:000001|2026-10-01T00:00:06.758Z|ChatMessage|filler.thinking|topic-0000|viewer:0000",
            "idle-to-burst:000011|2026-10-01T00:02:54.943Z|GameEvent|reaction.surprise|topic-0002|viewer:0011",
        ),
        (
            "chat-burst-10x.json",
            "chat-burst-10x:000001|2026-10-01T00:00:25.312Z|ChatMessage|reaction.surprise|topic-0000|viewer:0000",
            "chat-burst-10x:000011|2026-10-01T00:01:57.078Z|GameEvent|filler.thinking|topic-0014|viewer:0017",
        ),
    ] {
        let digest = digests(&shipped(name));
        assert_eq!(digest.len(), 11, "{name}: trace length changed");
        assert_eq!(
            digest[0], expected_first,
            "{name}: the first event moved, so the actor draw is consuming a \
             different number of values from the stream and every later draw shifted"
        );
        assert_eq!(
            *digest.last().unwrap(),
            expected_last,
            "{name}: the last event moved, so the trace no longer matches the \
             recorded baseline"
        );
    }

    // Re-seeding is the one thing that legitimately moves the trace.
    let original = shipped("idle-to-burst.json");
    let mut reseeded = original.clone();
    reseeded.seed = original.seed.wrapping_add(1);
    assert_ne!(
        digests(&original),
        digests(&reseeded),
        "the seed must still decide the trace"
    );
}

#[test]
fn a_repeated_viewer_rate_is_an_exact_quota_not_a_coin_flip() {
    // The boundary that broke the estimate version of this rule: pool 500, 556
    // events, p = 0.1. `floor(556 * 0.9) = 500` accepts the document, but with
    // per-event Bernoulli repeats the realised fresh count varied with the seed
    // and 13 of 40 seeds materialised fewer than 500 distinct viewers.
    //
    // Repeats are now an exact quota, so the count is an identity rather than an
    // expectation - checked across a spread of seeds so a return to a probabilistic
    // decision cannot pass by landing favourably on the seed it happens to use.
    let mut phase = steady_phase("crowd", 3_336_000, 10.0);
    phase.distinct_actors = 500;
    phase.repeat_viewer_probability = 0.1;

    for seed in 0..24u64 {
        let scenario = StreamScenario {
            scenario_id: format!("quota-p{}-seed{seed}", seed),
            seed,
            phases: vec![phase.clone()],
            stream_duration_ms: phase.duration_ms,
            ..shipped("high-cardinality-actors.json")
        };
        let events = scenario.total_event_count().unwrap();
        let repeats = (events as f64 * phase.repeat_viewer_probability).floor() as u64;
        let predicted = (phase.distinct_actors as usize).min((events - repeats) as usize);

        let trace = trace_of(&scenario);
        let mut actors: Vec<&str> = trace
            .iter()
            .filter_map(|entry| entry.event.actor_id.as_deref())
            .collect();
        actors.sort_unstable();
        actors.dedup();

        assert_eq!(
            actors.len(),
            predicted,
            "seed {seed}: distinct viewers must be min(pool, events - repeats) exactly; \
             the validator evaluates the same expression, so any other number means the \
             two disagree about what this document produces"
        );
        assert!(
            actors.len() >= HIGH_CARDINALITY_MIN_ACTORS,
            "seed {seed}: a document accepted as high_cardinality reached only {} viewers",
            actors.len()
        );
    }
}

#[test]
fn a_well_formed_scenario_never_panics_on_large_actor_pools() {
    // `distinct_actors` is a `u32` from external JSON and actor ids share one
    // namespace across phases, so a scenario whose pools do not fit in that
    // namespace is unrepresentable. It has to be refused with a reason, not wrap
    // or panic - the loader reads untrusted documents.
    let phase = |name: &str, actors: u32| ScenarioPhase {
        distinct_actors: actors,
        ..steady_phase(name, 600_000, 1.0)
    };

    let overflows = StreamScenario {
        scenario_id: "actor-namespace-overflow".to_owned(),
        phases: vec![phase("first", u32::MAX), phase("second", 2)],
        stream_duration_ms: 1_200_000,
        ..shipped("high-cardinality-actors.json")
    };
    let error = generate_scenario_trace(&overflows)
        .expect_err("a namespace that cannot be represented must be refused");
    assert!(
        error.to_string().contains("overflows"),
        "the refusal must name the reason, got: {error}"
    );

    // And the boundary just below it still generates: the guard must not reject
    // a large-but-valid namespace.
    let largest_valid = StreamScenario {
        scenario_id: "actor-namespace-largest".to_owned(),
        phases: vec![phase("only", u32::MAX)],
        stream_duration_ms: 600_000,
        ..shipped("high-cardinality-actors.json")
    };
    assert!(
        generate_scenario_trace(&largest_valid).is_ok(),
        "a single phase declaring u32::MAX actors is representable and must generate"
    );
}

#[test]
fn every_accepted_high_cardinality_document_reaches_its_claimed_cardinality() {
    let distinct_actors = |scenario: &StreamScenario| {
        let trace = trace_of(scenario);
        let mut actors: Vec<&str> = trace
            .iter()
            .filter_map(|entry| entry.event.actor_id.as_deref())
            .collect();
        actors.sort_unstable();
        actors.dedup();
        actors.len()
    };

    // The boundary the review found: a pool exactly at the threshold, exactly
    // threshold-many events, and no repeats. Sampling viewers uniformly with
    // replacement reaches only about 300 distinct actors here (the birthday
    // problem), which is why the generator walks the pool in order instead.
    // Each case is sized from the rate so `min(pool, non-repeated events)` clears
    // the threshold with room to spare: a phase must run long enough for the rule
    // to hold, otherwise the case asserts nothing about accepted documents.
    for (pool, events_per_minute, repeats) in [
        (500u32, 10.0, 0.0),
        (500, 10.0, 0.02),
        (500, 20.0, 0.0),
        (1000, 5.0, 0.0),
        (4000, 10.0, 0.05),
        (500, 10.0, 0.1),
        // A pool far larger than the event count, so the realised count sits well
        // above the threshold. Uniform sampling with replacement would still clear
        // 500 here - which is exactly why this case exists: it is the one the
        // threshold assertion alone cannot catch, and only the exact
        // `min(pool, events)` check can.
        (4000, 20.0, 0.0),
    ] {
        let target_events = (HIGH_CARDINALITY_MIN_ACTORS as f64 / (1.0 - repeats) * 1.5) as u64;
        let duration_ms = (target_events as f64 * 60_000.0 / events_per_minute).ceil() as u64;
        let mut phase = steady_phase("crowd", duration_ms, events_per_minute);
        phase.distinct_actors = pool;
        phase.repeat_viewer_probability = repeats;
        let candidate = StreamScenario {
            scenario_id: format!(
                "cardinality-{pool}-p{}-r{}",
                (events_per_minute * 100.0) as u32,
                (repeats * 100.0) as u32
            ),
            phases: vec![phase.clone()],
            stream_duration_ms: phase.duration_ms,
            ..shipped("high-cardinality-actors.json")
        };

        let events = candidate.total_event_count().unwrap();
        let expected_fresh = (events as f64 * (1.0 - repeats)).floor() as usize;
        let predicted = (pool as usize).min(expected_fresh);
        assert!(
            predicted >= HIGH_CARDINALITY_MIN_ACTORS,
            "this case must satisfy the validator's rule, or it proves nothing: \
             pool={pool} events={events} repeats={repeats} predicts {predicted}"
        );

        let actual = distinct_actors(&candidate);
        if repeats == 0.0 {
            // With no repeats every event draws a fresh viewer, so the count is
            // exactly min(pool, events) - a hard guarantee, and the case that
            // fails if actor assignment ever goes back to sampling with
            // replacement. Uniform sampling over a pool of 500 for 500 draws
            // reaches only about 300.
            assert_eq!(
                actual, predicted,
                "every draw is fresh here, so distinct viewers must be exactly \
                 min(pool, events) = {predicted}, got {actual}"
            );
        }
        assert!(
            actual >= HIGH_CARDINALITY_MIN_ACTORS,
            "a document accepted as high_cardinality must reach {HIGH_CARDINALITY_MIN_ACTORS} \
             distinct viewers; pool={pool} events={events} repeats={repeats} reached only \
             {actual}, so the validator's bound is not a guarantee it can keep"
        );
    }
}

/// Actor ids share one namespace across the whole stream, so cardinality has to
/// accumulate across phases rather than restart in each one.
#[test]
fn distinct_viewers_accumulate_across_phases() {
    let mut first = steady_phase("first", 1_200_000, 10.0);
    first.distinct_actors = 300;
    first.repeat_viewer_probability = 0.0;
    let mut second = steady_phase("second", 1_200_000, 10.0);
    second.distinct_actors = 300;
    second.repeat_viewer_probability = 0.0;

    let trace = generate_scenario_trace(&StreamScenario {
        phases: vec![first, second],
        stream_duration_ms: 2_400_000,
        ..shipped("high-cardinality-actors.json")
    })
    .expect("valid");
    let mut actors: Vec<&str> = trace
        .iter()
        .filter_map(|entry| entry.event.actor_id.as_deref())
        .collect();
    actors.sort_unstable();
    actors.dedup();

    // 200 events per phase, 300-viewer pools. If the second phase restarted its
    // walk it would reissue viewer:0000..viewer:0199 and the stream would show 200
    // distinct viewers instead of 400.
    assert_eq!(
        actors.len(),
        400,
        "each phase must continue into fresh ids; a shared namespace that restarts per \
         phase would quietly halve the cardinality the stream actually carries"
    );
}

#[test]
fn every_shipped_scenario_class_means_what_it_claims() {
    // A class label that the parameters do not support is decoration, so each
    // shipped scenario is held to its own class and to the corpus-level contract
    // that a consumer refusal stays a valid document.
    for name in [
        "normal-mixed-stream.json",
        "chat-burst-10x.json",
        "idle-to-burst.json",
        "high-cardinality-actors.json",
        "high-generative-miss.json",
    ] {
        let scenario = shipped(name);
        match scenario.scenario_class {
            ScenarioClass::IdleToBurst => assert!(
                scenario
                    .phases
                    .iter()
                    .any(|phase| phase.events_per_minute == 0.0),
                "{name} claims an idle phase and declares none"
            ),
            ScenarioClass::HighCardinality => assert!(
                scenario
                    .phases
                    .iter()
                    .any(|phase| phase.distinct_actors as usize >= HIGH_CARDINALITY_MIN_ACTORS),
                "{name} claims cardinality and declares no pool big enough to hold it"
            ),
            _ => {}
        }
    }

    // Every shipped scenario must be one the cached replay path can run, because
    // these are the ones the benchmark is expected to consume.
    for name in [
        "normal-mixed-stream.json",
        "chat-burst-10x.json",
        "idle-to-burst.json",
        "high-cardinality-actors.json",
        "high-generative-miss.json",
    ] {
        shipped(name)
            .validate_playability_for_cached_replay()
            .unwrap_or_else(|error| panic!("{name} must be playable by the benchmark: {error}"));
    }
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
