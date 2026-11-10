//! Issue #78 S1 falsifier: a 100x synthetic chat burst built from the #70
//! `chat-burst-10x` scenario stays bounded, makes no per-comment ranker calls
//! and leaves no admitted comment without exactly one terminal outcome.

use aivtuber_domain::{
    EventEnvelope, EventKind, ReflexContext, SecurityPlane, SourceClass, StreamScenario,
    generate_scenario_trace, scenario_instant_ms,
};
use aivtuber_reflex::{
    BatchEngagementRequest, BatchRankFailure, BatchRanker, BatchResolution, BurstConfig,
    BurstEngine, Disposition, ImmediateReason,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn burst_100x() -> Vec<EventEnvelope> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/scenarios/chat-burst-10x.json");
    let mut scenario: StreamScenario =
        serde_json::from_slice(&std::fs::read(path).expect("scenario fixture")).expect("parse");
    // The fixture is 1 -> 10 events/minute (10x). Scale both phases so the
    // burst is 100x its baseline at a rate where batching actually matters:
    // 60/min (1 per second) -> 6000/min (100 per second).
    scenario.scenario_id = "chat-burst-100x".to_owned();
    scenario.phases[0].events_per_minute = 60.0;
    scenario.phases[1].events_per_minute = 6000.0;
    scenario.phases[1].duration_ms = 20_000;
    scenario.stream_duration_ms = 80_000;
    generate_scenario_trace(&scenario)
        .expect("100x trace")
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

fn at_ms(event: &EventEnvelope) -> u64 {
    scenario_instant_ms(&event.observed_at).expect("scenario instant")
}

fn operator_event(sequence: u64) -> EventEnvelope {
    EventEnvelope {
        schema_version: aivtuber_domain::EVENT_SCHEMA_VERSION.to_owned(),
        event_id: format!("operator:{sequence}"),
        correlation_id: "operator".to_owned(),
        sequence,
        observed_at: "2026-10-01T00:00:00Z".to_owned(),
        source: "local".to_owned(),
        source_class: SourceClass::Operator,
        plane: SecurityPlane::Control,
        trust_level: aivtuber_domain::TrustLevel::Trusted,
        kind: EventKind::OperatorCommand,
        actor_id: None,
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::new(),
    }
}

struct CountingRanker {
    calls: u64,
}

impl BatchRanker for CountingRanker {
    fn rank(&mut self, request: &BatchEngagementRequest) -> Result<Vec<String>, BatchRankFailure> {
        self.calls += 1;
        request.validate().expect("bounded request");
        Ok(request
            .candidates
            .iter()
            .rev()
            .map(|c| c.event_id.clone())
            .collect())
    }
}

#[derive(Default)]
struct Run {
    records: Vec<u8>,
    /// event_id -> number of terminal outcomes observed.
    terminal: BTreeMap<String, u32>,
    admitted_chat: Vec<String>,
    control_immediate: u64,
}

fn record(run: &mut Run, resolution: &BatchResolution) {
    run.records.extend(resolution.record.deterministic_bytes());
    for outcome in &resolution.record.outcomes {
        *run.terminal.entry(outcome.event_id.clone()).or_insert(0) += 1;
    }
}

fn drive(drain_each: bool, use_ranker: bool, poll_every_event: bool) -> (Run, BurstEngine, u64) {
    let trace = burst_100x();
    let mut engine = BurstEngine::new(BurstConfig::default()).expect("config");
    let context = ReflexContext::default();
    let mut ranker = CountingRanker { calls: 0 };
    let mut run = Run::default();
    let mut last_ms = 0;

    for (index, event) in trace.iter().enumerate() {
        let now = at_ms(event);
        last_ms = now;
        if poll_every_event {
            for shed in engine.poll(now) {
                record(&mut run, &shed);
            }
        }
        // Operator control arrives mid-burst and must never wait or queue.
        if index % 250 == 249 {
            let before = engine.pending_len();
            let (disposition, shed) = engine.offer(&operator_event(index as u64), now, 0);
            assert_eq!(
                disposition,
                Disposition::Immediate(ImmediateReason::ControlPlane)
            );
            assert!(shed.is_empty());
            assert_eq!(engine.pending_len(), before, "control must not touch batch");
            run.control_immediate += 1;
        }
        let (disposition, shed) = engine.offer(event, now, ((event.sequence * 7919) % 1000) as u16);
        for resolution in &shed {
            record(&mut run, resolution);
        }
        match disposition {
            Disposition::QuietPassthrough => {
                *run.terminal.entry(event.event_id.clone()).or_insert(0) += 1;
                run.admitted_chat.push(event.event_id.clone());
            }
            Disposition::Batched => run.admitted_chat.push(event.event_id.clone()),
            Disposition::Immediate(ImmediateReason::NonChatKind) => {}
            other => panic!("unexpected disposition {other:?}"),
        }
        assert!(engine.pending_len() <= engine.config().max_candidates);
        assert!(engine.ready_len() <= engine.config().max_ready_batches);
        if drain_each {
            while let Some(resolution) =
                engine.resolve_next(&context, if use_ranker { Some(&mut ranker) } else { None })
            {
                record(&mut run, &resolution);
            }
        }
    }
    for resolution in engine.shutdown(last_ms + 1, &context) {
        record(&mut run, &resolution);
    }
    (run, engine, ranker.calls)
}

fn assert_exactly_once(run: &Run, engine: &BurstEngine) {
    for id in &run.admitted_chat {
        assert_eq!(
            run.terminal.get(id).copied(),
            Some(1),
            "{id} must have exactly one terminal outcome"
        );
    }
    assert_eq!(run.terminal.len(), run.admitted_chat.len());
    let metrics = engine.metrics();
    assert_eq!(metrics.admitted, run.admitted_chat.len() as u64);
    assert_eq!(metrics.admitted, metrics.terminal_total());
    assert_eq!(metrics.invariant_violations, 0);
    assert_eq!(engine.pending_len() + engine.ready_len(), 0);
}

#[test]
fn burst_100x_is_bounded_with_few_ranker_calls_and_full_accounting() {
    let (run, engine, ranker_calls) = drive(true, true, false);
    assert_exactly_once(&run, &engine);
    let metrics = engine.metrics();
    assert!(
        metrics.admitted > 1_500,
        "the trace must be a real burst, got {}",
        metrics.admitted
    );
    assert!(metrics.batches_flushed > 0);
    // No per-comment Jev: calls track batches, not comments.
    assert_eq!(ranker_calls, metrics.ranker_calls);
    assert!(metrics.ranker_calls <= metrics.batches_flushed);
    assert!(
        metrics.ranker_calls_per_comment() < 0.1,
        "ranker calls per comment {}",
        metrics.ranker_calls_per_comment()
    );
    assert!(metrics.pending_candidates_high_water <= engine.config().max_candidates);
    assert!(metrics.pending_bytes_high_water <= engine.config().max_bytes);
    assert!(metrics.wait_ms_max <= engine.config().window_ms);
    assert!(metrics.terminal[0] > 0, "some comments are selected");
    assert!(run.control_immediate > 0);
}

#[test]
fn burst_without_a_ranker_makes_zero_ranker_calls() {
    let (run, engine, calls) = drive(true, false, false);
    assert_exactly_once(&run, &engine);
    assert_eq!(calls, 0);
    assert_eq!(engine.metrics().ranker_calls, 0);
}

#[test]
fn undrained_burst_stays_bounded_and_sheds_with_accounting() {
    let (run, engine, _) = drive(false, false, false);
    assert_exactly_once(&run, &engine);
    assert!(
        engine.metrics().batches_shed > 0,
        "a slow consumer must shed"
    );
    assert!(engine.metrics().ready_batches_high_water <= engine.config().max_ready_batches);
    assert!(engine.metrics().terminal[5] > 0);
}

#[test]
fn replay_is_byte_identical_and_independent_of_poll_cadence() {
    let (first, _, _) = drive(true, true, false);
    let (second, _, _) = drive(true, true, false);
    let (polled, _, _) = drive(true, true, true);
    assert!(!first.records.is_empty());
    assert_eq!(first.records, second.records);
    assert_eq!(first.records, polled.records);
}

#[test]
fn every_flush_reason_is_exercised_by_the_burst() {
    let (_, engine, _) = drive(true, false, false);
    let metrics = engine.metrics();
    assert!(metrics.flushed_count > 0);
    assert!(metrics.flushed_window + metrics.flushed_shutdown > 0);
}
