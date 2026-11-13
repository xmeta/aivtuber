//! Issue #78 S1 falsifier: a 100x synthetic chat burst built from the #70
//! `chat-burst-10x` scenario stays bounded, makes no per-comment ranker calls
//! and leaves no admitted comment without exactly one terminal outcome.

use aivtuber_domain::{
    EventEnvelope, EventKind, ReflexContext, SecurityPlane, SourceClass, StreamScenario,
    generate_scenario_trace, scenario_instant_ms,
};
use aivtuber_reflex::{
    BatchEngagementRequest, BatchRankFailure, BatchRanker, BatchResolution, BurstConfig,
    BurstEngine, Disposition, HttpResponse, HttpTransport, ImmediateReason, JevAdapter,
    JevAdapterConfig, JevApiKey, JevBatchRanker, JevBatchRankerConfig, TransportError,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

/// Stands in for the Jev endpoint. It answers every `score` question the
/// request names, scoring in reverse request order, except that every 7th
/// request is an HTTP 503 and every 11th a body with an invented slot, so the
/// burst also exercises typed failure and deterministic fallback.
#[derive(Debug, Default)]
struct FakeJev {
    requests: AtomicU64,
    max_questions: AtomicU64,
}

impl HttpTransport for FakeJev {
    fn post_json(
        &self,
        _endpoint: &str,
        _bearer_token: &str,
        body: &[u8],
        _timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        let n = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
        let request: serde_json::Value = serde_json::from_slice(body).expect("request json");
        let slots: Vec<String> = request["questions"]
            .as_object()
            .expect("questions")
            .keys()
            .cloned()
            .collect();
        self.max_questions
            .fetch_max(slots.len() as u64, Ordering::SeqCst);
        if n.is_multiple_of(7) {
            return Ok(HttpResponse {
                status: 503,
                body: Vec::new(),
            });
        }
        let mut answers = serde_json::Map::new();
        for (i, slot) in slots.iter().enumerate() {
            let score = 1.0 - (i as f64 / slots.len() as f64);
            answers.insert(
                slot.clone(),
                serde_json::json!({
                    "type": "score", "score": score, "confidence": 0.9,
                    "probabilities": {"low": 1.0 - score, "high": score},
                    "legend": {"low": "l", "high": "h"},
                }),
            );
        }
        if n.is_multiple_of(11) {
            answers.insert(
                "evt-injected".to_owned(),
                serde_json::json!({"type": "score", "score": 1.0, "confidence": 1.0,
                    "probabilities": {"a": 1.0}, "legend": {}}),
            );
        }
        Ok(HttpResponse {
            status: 200,
            body: serde_json::to_vec(&serde_json::json!({
                "model": "fake", "answers": answers,
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }))
            .expect("body"),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    NoRanker,
    Counting,
    Jev,
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

fn drive(drain_each: bool, kind: Kind, poll_every_event: bool) -> (Run, BurstEngine, u64) {
    let (run, engine, calls, _) = drive_with(drain_each, kind, poll_every_event);
    (run, engine, calls)
}

fn drive_with(
    drain_each: bool,
    kind: Kind,
    poll_every_event: bool,
) -> (Run, BurstEngine, u64, Arc<FakeJev>) {
    let trace = burst_100x();
    let mut engine = BurstEngine::new(BurstConfig::default()).expect("config");
    let context = ReflexContext::default();
    let mut ranker = CountingRanker { calls: 0 };
    let fake = Arc::new(FakeJev::default());
    let mut jev = JevBatchRanker::new(
        JevAdapter::with_transport(
            JevAdapterConfig::default(),
            JevApiKey::new("test-key").expect("key"),
            fake.clone(),
        )
        .expect("adapter"),
        // A breaker would mask the per-batch call accounting under test.
        JevBatchRankerConfig {
            breaker_threshold: 16,
            ..JevBatchRankerConfig::default()
        },
    )
    .expect("ranker");
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
            loop {
                let chosen: Option<&mut dyn BatchRanker> = match kind {
                    Kind::NoRanker => None,
                    Kind::Counting => Some(&mut ranker),
                    Kind::Jev => Some(&mut jev),
                };
                let Some(resolution) = engine.resolve_next(&context, chosen) else {
                    break;
                };
                record(&mut run, &resolution);
            }
        }
    }
    for resolution in engine.shutdown(last_ms + 1, &context) {
        record(&mut run, &resolution);
    }
    let calls = match kind {
        Kind::Jev => fake.requests.load(Ordering::SeqCst),
        _ => ranker.calls,
    };
    (run, engine, calls, fake)
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
    let (run, engine, ranker_calls) = drive(true, Kind::Counting, false);
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
    let (run, engine, calls) = drive(true, Kind::NoRanker, false);
    assert_exactly_once(&run, &engine);
    assert_eq!(calls, 0);
    assert_eq!(engine.metrics().ranker_calls, 0);
}

#[test]
fn undrained_burst_stays_bounded_and_sheds_with_accounting() {
    let (run, engine, _) = drive(false, Kind::NoRanker, false);
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
    let (first, _, _) = drive(true, Kind::Counting, false);
    let (second, _, _) = drive(true, Kind::Counting, false);
    let (polled, _, _) = drive(true, Kind::Counting, true);
    assert!(!first.records.is_empty());
    assert_eq!(first.records, second.records);
    assert_eq!(first.records, polled.records);
}

#[test]
fn every_flush_reason_is_exercised_by_the_burst() {
    let (_, engine, _) = drive(true, Kind::NoRanker, false);
    let metrics = engine.metrics();
    assert!(metrics.flushed_count > 0);
    assert!(metrics.flushed_window + metrics.flushed_shutdown > 0);
}

#[test]
fn burst_100x_with_the_jev_batch_ranker_is_one_call_per_batch_and_fully_accounted() {
    let (run, engine, calls, fake) = drive_with(true, Kind::Jev, false);
    assert_exactly_once(&run, &engine);
    let metrics = engine.metrics();
    assert!(metrics.admitted > 1_500);
    // At most one HTTP request per ranked batch, never per comment.
    assert_eq!(calls, metrics.ranker_calls);
    assert!(metrics.ranker_calls <= metrics.batches_flushed);
    assert!(
        metrics.ranker_calls_per_comment() < 0.1,
        "ranker calls per comment {}",
        metrics.ranker_calls_per_comment()
    );
    // Input stays inside the existing request ceiling.
    assert!(fake.max_questions.load(Ordering::SeqCst) <= engine.config().request_candidates as u64);
    // Invented slots and HTTP failures were rejected, not trusted.
    assert!(metrics.ranker_failures > 0, "the fake injects failures");
    assert!(metrics.ranker_failures < metrics.ranker_calls);
    assert_eq!(metrics.ranker_unknown_ids_ignored, 0);
    assert!(metrics.terminal[0] > 0);
    assert!(run.control_immediate > 0);
}

#[test]
fn jev_ranked_replay_is_byte_identical_and_records_ranker_and_fallback() {
    let (first, _, _) = drive(true, Kind::Jev, false);
    let (second, _, _) = drive(true, Kind::Jev, false);
    let (polled, _, _) = drive(true, Kind::Jev, true);
    assert!(!first.records.is_empty());
    assert_eq!(first.records, second.records);
    assert_eq!(first.records, polled.records);
    let text = String::from_utf8(first.records.clone()).expect("utf8 records");
    assert!(text.contains("\"ranking_source\":\"ranker\""));
    assert!(text.contains("\"ranking_source\":\"ranker_fallback\""));
}
