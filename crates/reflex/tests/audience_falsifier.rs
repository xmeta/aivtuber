//! Issue #79 S1 falsifier: one repeated-topic cluster is aggregated while an
//! important outlier is preserved, every member keeps exactly one terminal
//! outcome, and replay output is deterministic.

use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, ReflexContext, SecurityPlane, SourceClass,
    StreamScenario, TrustLevel, generate_scenario_trace, scenario_instant_ms,
};
use aivtuber_reflex::{
    AggregationMethod, AudienceConfig, AudienceSignalClass, BatchEngagementRequest,
    BatchRankFailure, BatchRanker, BatchReplayRecord, BurstConfig, BurstEngine, Disposition,
    ReactionFamily, TerminalOutcome,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn chat(seq: u64, actor: &str, text: &str) -> EventEnvelope {
    let mut payload = BTreeMap::new();
    payload.insert("text".to_owned(), serde_json::json!(text));
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: format!("e{seq}"),
        correlation_id: "c".to_owned(),
        sequence: seq,
        observed_at: "2026-10-01T00:00:00Z".to_owned(),
        source: "twitch".to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: Some(actor.to_owned()),
        priority_hint: None,
        authorization: None,
        payload,
    }
}

fn engine(audience: bool) -> BurstEngine {
    let mut engine = BurstEngine::new(BurstConfig {
        burst_enter_count: 2,
        top_k: 8,
        request_candidates: 12,
        max_per_actor: 2,
        ..BurstConfig::default()
    })
    .expect("config");
    if audience {
        engine
            .set_audience(Some(AudienceConfig::default()))
            .expect("audience config");
    }
    engine
}

/// 6 laughter reactions, 4 "gg", one threat, one scam and one real question,
/// all from different actors, inside one window.
fn scenario() -> Vec<(u64, EventEnvelope)> {
    let texts = [
        "hello everyone",
        "草",
        "ww",
        "GG",
        "i will find you and kill you",
        "wwww",
        "gg",
        "free bitcoin giveaway http://scam.example/claim",
        "草草",
        "how did you beat that boss?",
        "ＧＧ",
        "lol",
        "gg",
        "ｗｗｗ",
    ];
    texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            (
                i as u64 * 50,
                chat(i as u64 + 1, &format!("actor{i}"), text),
            )
        })
        .collect()
}

fn run(audience: bool, poll_every_ms: u64) -> (Vec<BatchReplayRecord>, BurstEngine) {
    let mut engine = engine(audience);
    let ctx = ReflexContext::default();
    let mut records = Vec::new();
    let mut now = 0;
    for (at, event) in scenario() {
        while poll_every_ms > 0 && now + poll_every_ms <= at {
            now += poll_every_ms;
            records.extend(engine.poll(now).into_iter().map(|r| r.record));
        }
        let (_, shed) = engine.offer(&event, at, 100);
        records.extend(shed.into_iter().map(|r| r.record));
        now = now.max(at);
    }
    records.extend(engine.poll(10_000).into_iter().map(|r| r.record));
    while let Some(resolution) = engine.resolve_next(&ctx, None) {
        records.push(resolution.record);
    }
    (records, engine)
}

fn outcome_of(records: &[BatchReplayRecord], id: &str) -> TerminalOutcome {
    records
        .iter()
        .flat_map(|record| record.outcomes.iter())
        .find(|outcome| outcome.event_id == id)
        .unwrap_or_else(|| panic!("no outcome for {id}"))
        .outcome
}

#[test]
fn cluster_is_aggregated_and_outliers_are_preserved_with_exact_accounting() {
    let (records, engine) = run(true, 0);
    let signals: Vec<_> = records
        .iter()
        .flat_map(|record| record.audience_signals.iter())
        .collect();
    assert_eq!(signals.len(), 2, "laughter and good-game clusters");

    let laughter = signals
        .iter()
        .find(|s| {
            s.class
                == AudienceSignalClass::Reaction {
                    family: ReactionFamily::Laughter,
                }
        })
        .expect("laughter signal");
    // 草, ww, wwww, 草草, lol, ｗｗｗ from six actors.
    assert_eq!((laughter.count, laughter.unique_actors_estimate), (6, 6));
    assert_eq!(laughter.method, AggregationMethod::ReactionFamily);
    let good_game = signals
        .iter()
        .find(|s| {
            s.class
                == AudienceSignalClass::Reaction {
                    family: ReactionFamily::GoodGame,
                }
        })
        .expect("good-game signal");
    assert_eq!(good_game.count, 4, "GG, gg, ＧＧ, gg");

    // The important outliers are individual candidates, never absorbed.
    for id in ["e5", "e8", "e10"] {
        // threat, scam, real question
        assert!(
            !matches!(
                outcome_of(&records, id),
                TerminalOutcome::RepresentedByAggregate { .. }
            ),
            "{id} must stay an individual candidate"
        );
        assert!(
            !signals
                .iter()
                .any(|s| s.representative_event_ids.iter().any(|r| r == id)),
            "{id} must not appear in a signal"
        );
    }
    // With top_k 8 every individual candidate (2 carriers + 3 outliers) is
    // selected, so the outliers are really answered, not merely kept.
    for id in ["e5", "e8", "e10"] {
        assert!(matches!(
            outcome_of(&records, id),
            TerminalOutcome::Selected { .. }
        ));
    }

    // Exact accounting: every admitted comment has exactly one outcome.
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    for outcome in records.iter().flat_map(|record| record.outcomes.iter()) {
        *seen.entry(outcome.event_id.as_str()).or_default() += 1;
    }
    let metrics = engine.metrics();
    assert_eq!(metrics.invariant_violations, 0);
    assert_eq!(metrics.admitted, metrics.terminal_total());
    // The first comment was quiet passthrough; the batch has 13 comments.
    let batched: u64 = records.iter().map(|r| r.outcomes.len() as u64).sum();
    assert_eq!(batched, 13);
    assert!(seen.values().all(|&n| n == 1));
    // (6 - 1) + (4 - 1) members are represented; the two carriers are ranked.
    assert_eq!(metrics.terminal[6], 8);
    assert_eq!(metrics.audience_members_represented, 8);
    assert_eq!(metrics.audience_signals, 2);
    // Threat and the scam link were screened out by the guard before grouping.
    use aivtuber_reflex::GuardReason;
    assert_eq!(metrics.audience_guarded[&GuardReason::Threat], 1);
    assert_eq!(metrics.audience_guarded[&GuardReason::Link], 1);
    let carriers: Vec<_> = signals
        .iter()
        .map(|s| s.representative_event_ids[0].as_str())
        .collect();
    for carrier in carriers {
        assert!(matches!(
            outcome_of(&records, carrier),
            TerminalOutcome::Selected { .. }
        ));
    }
}

#[test]
fn replay_is_byte_identical_and_independent_of_poll_cadence() {
    let bytes = |polls: u64| -> Vec<u8> {
        run(true, polls)
            .0
            .iter()
            .flat_map(BatchReplayRecord::deterministic_bytes)
            .collect()
    };
    let baseline = bytes(0);
    assert!(!baseline.is_empty());
    assert_eq!(baseline, bytes(0));
    assert_eq!(baseline, bytes(7));
    assert_eq!(baseline, bytes(333));
}

#[test]
fn without_audience_records_are_unchanged() {
    let (records, engine) = run(false, 0);
    for record in &records {
        assert!(record.audience_signals.is_empty());
        let encoded = String::from_utf8(record.deterministic_bytes()).expect("utf8");
        assert!(!encoded.contains("audience"), "{encoded}");
        assert!(!encoded.contains("represented"), "{encoded}");
    }
    assert_eq!(engine.metrics().terminal[6], 0);
    assert_eq!(engine.metrics().audience_signals, 0);
}

struct Capture {
    requests: Vec<BatchEngagementRequest>,
    answer: Vec<String>,
}

impl BatchRanker for Capture {
    fn rank(&mut self, request: &BatchEngagementRequest) -> Result<Vec<String>, BatchRankFailure> {
        self.requests.push(request.clone());
        Ok(self.answer.clone())
    }
}

#[test]
fn a_ranker_sees_one_crowd_candidate_and_cannot_revive_absorbed_members() {
    let mut engine = engine(true);
    let mut sink = Vec::new();
    for (at, event) in scenario() {
        let (_, shed) = engine.offer(&event, at, 100);
        sink.extend(shed);
    }
    engine.poll(10_000);
    // Ask for an absorbed member first (e3 "ww" is a non-carrier laughter member).
    let mut ranker = Capture {
        requests: Vec::new(),
        answer: vec!["e3".to_owned(), "e4".to_owned(), "nonexistent".to_owned()],
    };
    let resolution = engine
        .resolve_next(&ReflexContext::default(), Some(&mut ranker))
        .expect("resolution");
    let request = &ranker.requests[0];
    request.validate().expect("bounded request");
    // 2 carriers + 3 outliers; the 8 absorbed members were never shown.
    assert_eq!(request.candidates.len(), 5);
    let crowd: Vec<_> = request
        .candidates
        .iter()
        .filter(|c| c.represents.is_some())
        .collect();
    assert_eq!(crowd.len(), 2);
    assert!(crowd.iter().all(|c| c.actor_id.is_none()));
    assert!(matches!(
        resolution
            .record
            .outcomes
            .iter()
            .find(|o| o.event_id == "e3")
            .map(|o| o.outcome),
        Some(TerminalOutcome::RepresentedByAggregate { .. })
    ));
    assert_eq!(engine.metrics().invariant_violations, 0);
}

#[test]
fn crowd_candidate_does_not_consume_actor_fairness() {
    // One actor is both a laughter-crowd member and has an individual remark;
    // the crowd carrier is actor-less, so the remark is still selectable.
    let mut engine = BurstEngine::new(BurstConfig {
        burst_enter_count: 2,
        top_k: 3,
        max_per_actor: 1,
        ..BurstConfig::default()
    })
    .expect("config");
    engine
        .set_audience(Some(AudienceConfig::default()))
        .expect("audience");
    let events = [
        chat(1, "x", "warm up"),
        chat(2, "x", "草"),
        chat(3, "b", "草"),
        chat(4, "c", "草"),
        chat(5, "x", "what was that move?"),
    ];
    for (i, event) in events.iter().enumerate() {
        engine.offer(event, i as u64, 100);
    }
    engine.poll(10_000);
    let record = engine
        .resolve_next(&ReflexContext::default(), None)
        .expect("resolution")
        .record;
    let outcome = |id: &str| {
        record
            .outcomes
            .iter()
            .find(|o| o.event_id == id)
            .expect("outcome")
            .outcome
    };
    assert!(matches!(outcome("e2"), TerminalOutcome::Selected { .. }));
    assert!(matches!(
        outcome("e3"),
        TerminalOutcome::RepresentedByAggregate { .. }
    ));
    assert!(matches!(outcome("e5"), TerminalOutcome::Selected { .. }));
}

#[test]
fn control_claims_still_never_reach_aggregation() {
    let mut engine = engine(true);
    let mut claim = chat(1, "a", "草");
    claim.source_class = SourceClass::Operator;
    let (disposition, _) = engine.offer(&claim, 0, 100);
    assert!(matches!(disposition, Disposition::Refused(_)));
    assert_eq!(engine.metrics().audience_signals, 0);
}

fn burst_100x() -> Vec<EventEnvelope> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/scenarios/chat-burst-10x.json");
    let mut scenario: StreamScenario =
        serde_json::from_slice(&std::fs::read(path).expect("fixture")).expect("parse");
    scenario.scenario_id = "chat-burst-100x".to_owned();
    scenario.phases[0].events_per_minute = 60.0;
    scenario.phases[1].events_per_minute = 6000.0;
    scenario.phases[1].duration_ms = 20_000;
    scenario.stream_duration_ms = 80_000;
    generate_scenario_trace(&scenario)
        .expect("trace")
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

#[test]
fn aggregation_over_the_100x_burst_stays_bounded_and_fully_accounted() {
    let trace = burst_100x();
    let mut engine = BurstEngine::new(BurstConfig::default()).expect("config");
    engine
        .set_audience(Some(AudienceConfig::default()))
        .expect("audience");
    let ctx = ReflexContext::default();
    let mut terminal: BTreeMap<String, u32> = BTreeMap::new();
    let mut count = |record: BatchReplayRecord, signals: &mut usize| {
        assert!(record.audience_signals.len() <= 64);
        *signals += record.audience_signals.len();
        for outcome in record.outcomes {
            *terminal.entry(outcome.event_id).or_default() += 1;
        }
    };
    let mut signals = 0;
    let mut quiet = 0_u64;
    for event in &trace {
        let at = scenario_instant_ms(&event.observed_at).expect("instant");
        let (disposition, shed) = engine.offer(event, at, 100);
        if disposition == Disposition::QuietPassthrough {
            quiet += 1;
        }
        for resolution in shed {
            count(resolution.record, &mut signals);
        }
        while let Some(resolution) = engine.resolve_next(&ctx, None) {
            count(resolution.record, &mut signals);
        }
    }
    for resolution in engine.shutdown(10_000_000, &ctx) {
        count(resolution.record, &mut signals);
    }
    let metrics = engine.metrics();
    assert_eq!(metrics.invariant_violations, 0);
    assert_eq!(metrics.admitted, metrics.terminal_total());
    assert_eq!(
        terminal.len() as u64 + quiet,
        metrics.admitted,
        "every admitted comment has an outcome"
    );
    assert!(terminal.values().all(|&n| n == 1));
    assert_eq!(metrics.audience_signals as usize, signals);
}
