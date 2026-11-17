//! Issue #79 S2: crowd routing inside the burst engine. A cluster is routed by
//! its signal class, a persisting class reads across batches, a reaction group
//! cools down across batches, and nothing here adds an absorption path.

use aivtuber_domain::{
    CrowdClass, CrowdTrend, EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, ReflexContext,
    SecurityPlane, SourceClass, TrustLevel,
};
use aivtuber_reflex::{
    AudienceConfig, BatchReplayRecord, BurstConfig, BurstEngine, CrowdAction, CrowdConfig,
    CrowdReactionGroup, TerminalOutcome,
};
use std::collections::BTreeMap;

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

fn engine(crowd: Option<CrowdConfig>) -> BurstEngine {
    let mut engine = BurstEngine::new(BurstConfig {
        burst_enter_count: 2,
        top_k: 8,
        max_candidates: 64,
        max_bytes: 32 * 1024,
        request_candidates: 12,
        max_per_actor: 4,
        ..BurstConfig::default()
    })
    .expect("config");
    engine
        .set_audience(Some(AudienceConfig::default()))
        .expect("audience");
    engine.set_crowd(crowd).expect("crowd");
    engine
}

struct Feed {
    engine: BurstEngine,
    seq: u64,
    records: Vec<BatchReplayRecord>,
}

impl Feed {
    fn new(crowd: Option<CrowdConfig>) -> Self {
        Self {
            engine: engine(crowd),
            seq: 0,
            records: Vec::new(),
        }
    }

    /// Offer `texts` (each from its own actor) starting at `start_ms`, 5 ms
    /// apart, then flush and resolve at `start_ms + 2_000`.
    fn batch(&mut self, start_ms: u64, texts: &[&str]) {
        // Two warm-up comments so the burst has begun and every comment of the
        // batch is held.
        for (i, text) in ["hi there", "hello again"].iter().enumerate() {
            self.offer(start_ms + i as u64, &format!("warm{}-{start_ms}", i), text);
        }
        for (i, text) in texts.iter().enumerate() {
            self.offer(
                start_ms + 10 + i as u64 * 5,
                &format!("a{i}-{start_ms}"),
                text,
            );
        }
        let ctx = ReflexContext::default();
        self.records.extend(
            self.engine
                .poll(start_ms + 2_000)
                .into_iter()
                .map(|r| r.record),
        );
        while let Some(resolution) = self.engine.resolve_next(&ctx, None) {
            self.records.push(resolution.record);
        }
    }

    fn offer(&mut self, at: u64, actor: &str, text: &str) {
        self.seq += 1;
        let (_, shed) = self.engine.offer(&chat(self.seq, actor, text), at, 100);
        self.records.extend(shed.into_iter().map(|r| r.record));
    }
}

fn celebration(n: usize) -> Vec<&'static str> {
    let tokens = ["おめでとう", "GG", "congrats", "nice", "おめでとう！"];
    (0..n).map(|i| tokens[i % tokens.len()]).collect()
}

#[test]
fn crowd_routing_is_off_unless_configured() {
    let mut feed = Feed::new(None);
    feed.batch(0, &celebration(20));
    assert!(feed.records.iter().all(|record| record.crowd.is_empty()));
    let json = serde_json::to_string(&feed.records).expect("json");
    assert!(!json.contains("\"crowd\""), "absent when empty");
    assert_eq!(feed.engine.metrics().crowd_routed, 0);
}

#[test]
fn a_mixed_token_celebration_yields_one_selected_carrier_and_suppresses_the_rest() {
    let mut feed = Feed::new(Some(CrowdConfig::default()));
    feed.batch(0, &celebration(20));
    let record = feed
        .records
        .iter()
        .find(|record| !record.crowd.is_empty())
        .expect("a routed batch");
    // おめでとう, GG/nice/congrats cluster by family: several clusters, one group.
    assert!(record.crowd.len() >= 2, "several families share one moment");
    assert!(record.crowd.iter().all(|route| route.action
        == CrowdAction::NonVerbal {
            group: CrowdReactionGroup::Celebration
        }));
    let suppressed = record
        .outcomes
        .iter()
        .filter(|o| matches!(o.outcome, TerminalOutcome::SuppressedCrowdCooldown { .. }))
        .count();
    assert_eq!(suppressed, record.crowd.len() - 1);
    let selected_crowd = record
        .crowd
        .iter()
        .filter(|route| {
            let carrier = &record.audience_signals[route.signal].representative_event_ids[0];
            record.selected.contains(carrier)
        })
        .count();
    assert_eq!(selected_crowd, 1, "exactly one crowd reaction");
}

#[test]
fn a_second_batch_inside_the_cooldown_is_suppressed_and_after_it_reacts_again() {
    let mut feed = Feed::new(Some(CrowdConfig {
        cooldown_ms: 10_000,
        ..CrowdConfig::default()
    }));
    feed.batch(0, &celebration(12));
    feed.batch(4_000, &celebration(12));
    feed.batch(30_000, &celebration(12));
    let by_batch: Vec<Vec<&aivtuber_reflex::CrowdRouting>> = feed
        .records
        .iter()
        .filter(|r| !r.crowd.is_empty())
        .map(|r| r.crowd.iter().collect())
        .collect();
    assert_eq!(by_batch.len(), 3);
    let reacted = |record: &BatchReplayRecord| {
        record
            .crowd
            .iter()
            .filter(|route| {
                record
                    .selected
                    .contains(&record.audience_signals[route.signal].representative_event_ids[0])
            })
            .count()
    };
    let routed: Vec<&BatchReplayRecord> = feed
        .records
        .iter()
        .filter(|r| !r.crowd.is_empty())
        .collect();
    assert_eq!(reacted(routed[0]), 1);
    assert_eq!(reacted(routed[1]), 0, "inside the cooldown");
    assert_eq!(reacted(routed[2]), 1, "the cooldown elapsed");
}

#[test]
fn persistence_raises_intensity_and_the_window_reads_a_trend() {
    let mut feed = Feed::new(Some(CrowdConfig::default()));
    feed.batch(0, &["草"; 6]);
    feed.batch(3_000, &["草"; 14]);
    let routes: Vec<_> = feed.records.iter().flat_map(|r| r.crowd.iter()).collect();
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].summary.class, CrowdClass::Laughter);
    assert_eq!(routes[0].summary.window_batches, 1);
    assert_eq!(routes[1].summary.window_batches, 2);
    assert_eq!(routes[1].summary.trend, CrowdTrend::Rising);
    assert!(routes[1].summary.intensity_milli >= routes[0].summary.intensity_milli);
}

#[test]
fn a_repeated_question_routes_to_one_representative_reply_and_a_remark_does_not() {
    let mut feed = Feed::new(Some(CrowdConfig::default()));
    let mut texts: Vec<&str> = vec!["what game is this?"; 8];
    texts.extend(["nice weather today"; 5]);
    texts.push("i will find you and kill you");
    feed.batch(0, &texts);
    let record = feed
        .records
        .iter()
        .find(|r| !r.audience_signals.is_empty())
        .expect("signals");
    assert_eq!(record.audience_signals.len(), 2);
    assert_eq!(record.crowd.len(), 1, "only the question is routed");
    let route = &record.crowd[0];
    assert_eq!(route.action, CrowdAction::RepresentativeReply);
    assert_eq!(route.summary.class, CrowdClass::RepeatedQuestion);
    assert_eq!(route.summary.member_count, 8);
    // The threat is an individual candidate, never absorbed.
    let threat = record
        .outcomes
        .iter()
        .find(|o| o.event_id == format!("e{}", 2 + texts.len()))
        .expect("threat outcome");
    assert!(!matches!(
        threat.outcome,
        TerminalOutcome::RepresentedByAggregate { .. }
    ));
}

#[test]
fn routing_records_carry_no_text_and_no_actor_ids_and_replay_identically() {
    let run = || {
        let mut feed = Feed::new(Some(CrowdConfig::default()));
        feed.batch(0, &celebration(12));
        feed.batch(2_500, &celebration(30));
        feed.records
            .iter()
            .map(BatchReplayRecord::deterministic_bytes)
            .collect::<Vec<_>>()
    };
    let first = run();
    assert_eq!(first, run(), "byte-identical across runs");
    let all = first
        .iter()
        .map(|bytes| String::from_utf8(bytes.clone()).expect("utf8"))
        .collect::<String>();
    assert!(all.contains("\"crowd\""));
    for forbidden in ["おめでとう", "congrats", "actor_id", "a0-", "twitch"] {
        assert!(!all.contains(forbidden), "{forbidden} leaked into a record");
    }
}

#[test]
fn every_member_keeps_exactly_one_outcome() {
    let mut feed = Feed::new(Some(CrowdConfig::default()));
    feed.batch(0, &celebration(20));
    feed.batch(4_000, &celebration(20));
    let metrics = feed.engine.metrics();
    assert_eq!(metrics.invariant_violations, 0);
    let total: usize = feed.records.iter().map(|r| r.outcomes.len()).sum();
    assert_eq!(total as u64, metrics.admitted - metrics.terminal[1]);
    assert_eq!(metrics.terminal_total(), metrics.admitted);
}
