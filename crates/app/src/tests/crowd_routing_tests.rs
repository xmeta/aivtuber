//! Issue #79 S2 falsifier: crowd-level reaction routing through the live burst
//! path, replayed from `examples/crowd-routing/falsifier.json`.
//!
//! * A celebration burst of more than 80 comments yields exactly one crowd
//!   reaction, played from the pack without routing or generating anything for
//!   the carrier's text.
//! * A repeated-question burst yields exactly one representative reply whose
//!   request carries a typed crowd summary (no member text, no actor id).
//! * Outliers stay individual, accounting is exact, and the replay output is
//!   byte-identical across two runs.

use super::*;
use aivtuber_domain::{CrowdClass, CrowdTrend, ThinkingRequest};
use aivtuber_reflex::{AudienceConfig, BurstConfig, CrowdConfig};

const FIXTURE: &str = include_str!("../../../../examples/crowd-routing/falsifier.json");

#[derive(Clone, Default)]
struct ProbeRoute {
    routed: Arc<Mutex<Vec<String>>>,
}

impl RoutePlanner for ProbeRoute {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        self.routed
            .lock()
            .expect("routed")
            .push(event.event_id.clone());
        Ok(PlaybackRoute::Generate(Box::new(GenerationRoute {
            source_event: event.clone(),
            thinking: aivtuber_domain::ThinkingRequest::from_event(
                event,
                event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("event"),
                PrivacyClass::Pseudonymous,
                ReflexContext::default(),
                RetrievalSnapshot::default(),
                Vec::new(),
            ),
            routing_reason: aivtuber_generative::GenerationRoutingReason::ExplicitLlmRoute,
            intent: "generated.reply".to_owned(),
            style: None,
            fallback_variant_group: None,
        })))
    }
}

#[derive(Clone, Default)]
struct CaptureThinking {
    requests: Arc<Mutex<Vec<ThinkingRequest>>>,
}

impl ThinkingEngine for CaptureThinking {
    fn generate<'a>(&'a self, request: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            Ok(GeneratedReply {
                text: "That is a good question.".to_owned(),
                usage: None,
            })
        })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "capture-thinking".to_owned(),
            model_alias: Some("capture".to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

struct Rig {
    app: ProductionApp<ProbeRoute>,
    routed: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<ThinkingRequest>>>,
}

fn lane_config(fixture: &serde_json::Value, crowd: bool) -> ChatBurstConfig {
    let lane = &fixture["lane"];
    let burst = &lane["burst"];
    let get = |key: &str| burst[key].as_u64().expect(key);
    ChatBurstConfig {
        burst: BurstConfig {
            window_ms: get("window_ms"),
            burst_enter_count: get("burst_enter_count") as usize,
            top_k: get("top_k") as usize,
            max_per_actor: get("max_per_actor") as usize,
            max_candidates: get("max_candidates") as usize,
            max_bytes: get("max_bytes") as usize,
            ..BurstConfig::default()
        },
        reply_ttl_ms: lane["reply_ttl_ms"].as_u64().expect("reply_ttl_ms"),
        audience: Some(AudienceConfig::default()),
        crowd: crowd.then(|| CrowdConfig {
            window_ms: lane["crowd"]["window_ms"].as_u64().expect("window_ms"),
            cooldown_ms: lane["crowd"]["cooldown_ms"].as_u64().expect("cooldown_ms"),
            ..CrowdConfig::default()
        }),
        ..ChatBurstConfig::default()
    }
}

fn rig(config: ChatBurstConfig) -> Rig {
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig {
            // The falsifier pushes ~100 comments from one source; the
            // per-source limiter is not what is under test.
            content_rate_per_second: 1_000_000.0,
            content_burst: 1_000_000,
            ..SecurityRuntimeConfig::default()
        },
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        },
        SecretRedactor::default(),
        Some("safe cached reaction".to_owned()),
    )
    .expect("security runtime");
    let route = ProbeRoute::default();
    let routed = Arc::clone(&route.routed);
    let thinking = CaptureThinking::default();
    let requests = Arc::clone(&thinking.requests);
    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        route,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(generative_runtime(thinking, MockTts::default()))
    .with_chat_burst(config)
    .expect("chat burst config");
    app.startup().expect("startup");
    Rig {
        app,
        routed,
        requests,
    }
}

/// Expand the fixture into `(phase, at_ms, event)` in replay order. Event ids
/// are `evt-<n>` over the whole replay, so the fixture can name outliers.
fn expand(fixture: &serde_json::Value) -> Vec<(String, u64, EventEnvelope)> {
    let mut out = Vec::new();
    let mut sequence = 0_u64;
    for phase in fixture["phases"].as_array().expect("phases") {
        let name = phase["name"].as_str().expect("name").to_owned();
        let start = phase["start_ms"].as_u64().expect("start_ms");
        let step = phase["step_ms"].as_u64().expect("step_ms");
        let mut index = 0_u64;
        let mut push = |actor: String, text: &str, out: &mut Vec<_>| {
            sequence += 1;
            let mut event = chat_event(sequence);
            event.actor_id = Some(actor);
            event
                .payload
                .insert("text".to_owned(), serde_json::json!(text));
            event.payload.remove("intent");
            out.push((name.clone(), start + index * step, event));
            index += 1;
        };
        for item in phase["items"].as_array().expect("items") {
            if let Some(repeat) = item["repeat"].as_u64() {
                let prefix = item["actor_prefix"].as_str().expect("actor_prefix");
                let actors = item["actors"].as_u64().expect("actors");
                let texts: Vec<&str> = item["texts"]
                    .as_array()
                    .expect("texts")
                    .iter()
                    .map(|text| text.as_str().expect("text"))
                    .collect();
                for i in 0..repeat {
                    push(
                        format!("viewer:{prefix}{}", i % actors),
                        texts[i as usize % texts.len()],
                        &mut out,
                    );
                }
            } else {
                push(
                    item["actor"].as_str().expect("actor").to_owned(),
                    item["text"].as_str().expect("text"),
                    &mut out,
                );
            }
        }
    }
    out
}

struct Replay {
    /// Terminal outcomes by event id, in the order they happened.
    terminals: BTreeMap<String, Vec<ChatOutcome>>,
    signals: Vec<String>,
    /// Asset played by each event whose dispatch produced playback.
    played: Vec<(String, String)>,
    /// Events the router saw (a generated or ordinary reply path).
    routed: Vec<String>,
    requests: Vec<ThinkingRequest>,
    metrics: ChatBurstMetrics,
    admitted_ids: Vec<(String, String)>,
}

fn replay(config: ChatBurstConfig) -> Replay {
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture");
    let mut rig = rig(config);
    let mut terminals: BTreeMap<String, Vec<ChatOutcome>> = BTreeMap::new();
    let mut signals = Vec::new();
    let mut played = Vec::new();
    let mut admitted_ids = Vec::new();
    let mut drain = |app: &mut ProductionApp<ProbeRoute>, played: &mut Vec<(String, String)>| {
        for record in app.take_chat_burst_records(usize::MAX) {
            terminals
                .entry(record.event_id)
                .or_default()
                .push(record.outcome);
        }
        signals.extend(
            app.take_audience_signals(usize::MAX)
                .iter()
                .map(|signal| format!("{signal:?}")),
        );
        for report in app.take_deferred_reply_reports(usize::MAX) {
            if let Some(asset) = report.asset_id {
                played.push((report.event_id, asset));
            }
        }
    };
    let events = expand(&fixture);
    let mut last_phase = String::new();
    let mut last_at = 0;
    for (phase, at, event) in &events {
        if *phase != last_phase && !last_phase.is_empty() {
            settle(&mut rig.app, last_at, &mut drain, &mut played);
        }
        last_phase = phase.clone();
        last_at = *at;
        let raw = serde_json::to_vec(event).expect("event json");
        let outcome = rig
            .app
            .process_content_bytes(&raw, *at, *at ^ 0x55)
            .expect("content");
        admitted_ids.push((event.event_id.clone(), phase.clone()));
        if let Some(playback) = outcome.playback {
            played.push((event.event_id.clone(), playback.asset_id));
        }
        rig.app.service_chat_burst(*at, *at);
        wait_for_generation_completion(&mut rig.app, *at);
        drain(&mut rig.app, &mut played);
    }
    settle(&mut rig.app, last_at, &mut drain, &mut played);
    for end in [last_at + 60_000, last_at + 200_000] {
        rig.app.tick(end);
        rig.app.service_chat_burst(end, end);
        drain(&mut rig.app, &mut played);
    }
    rig.app.shutdown(last_at + 200_001);
    drain(&mut rig.app, &mut played);
    let metrics = rig.app.chat_burst_metrics().expect("metrics");
    let routed = rig.routed.lock().expect("routed").clone();
    let requests = rig.requests.lock().expect("requests").clone();
    Replay {
        terminals,
        signals,
        played,
        routed,
        requests,
        metrics,
        admitted_ids,
    }
}

/// Advance logical time in small steps so the burst window flushes, speech
/// ends and the deferred inbox drains in order before any reply time-to-live.
fn settle(
    app: &mut ProductionApp<ProbeRoute>,
    from_ms: u64,
    drain: &mut impl FnMut(&mut ProductionApp<ProbeRoute>, &mut Vec<(String, String)>),
    played: &mut Vec<(String, String)>,
) {
    for step in 1..=100_u64 {
        let at = from_ms + step * 250;
        app.tick(at);
        app.service_chat_burst(at, at);
        wait_for_generation_completion(app, at);
        drain(app, played);
    }
}

fn phase_ids(replay: &Replay, phase: &str) -> Vec<String> {
    replay
        .admitted_ids
        .iter()
        .filter(|(_, p)| p == phase)
        .map(|(id, _)| id.clone())
        .collect()
}

fn text_of(id: &str) -> String {
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture");
    let event = expand(&fixture)
        .into_iter()
        .find(|(_, _, event)| event.event_id == id)
        .expect("event")
        .2;
    event.payload["text"].as_str().expect("text").to_owned()
}

/// Bytes compared across runs: outcomes, signals, playback, routing and the
/// requests the provider saw. No wall-clock metric is in here.
fn deterministic_bytes(replay: &Replay) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "terminals": replay.terminals,
        "signals": replay.signals,
        "played": replay.played,
        "routed": replay.routed,
        "requests": replay.requests.iter().map(|r| serde_json::to_value(r).expect("request")).collect::<Vec<_>>(),
        "terminal_counts": replay.metrics.terminal,
        "crowd": [
            replay.metrics.crowd_routed,
            replay.metrics.crowd_non_verbal,
            replay.metrics.crowd_replies,
            replay.metrics.crowd_fallback_replies,
        ],
    }))
    .expect("json")
}

fn fixture() -> serde_json::Value {
    serde_json::from_str(FIXTURE).expect("fixture")
}

#[test]
fn a_celebration_burst_yields_one_crowd_reaction_and_outliers_stay_individual() {
    let fixture = fixture();
    let replay = replay(lane_config(&fixture, true));
    let celebration = phase_ids(&replay, "celebration");
    let crowd_comments = celebration
        .iter()
        .filter(|id| {
            let text = text_of(id);
            ["おめでとう", "GG", "congrats", "nice", "おめでとう！"].contains(&text.as_str())
        })
        .count();
    assert_eq!(
        crowd_comments as u64,
        fixture["expected"]["celebration_comments"]
            .as_u64()
            .unwrap()
    );
    assert!(crowd_comments > 80);

    // Exactly one crowd reaction.
    assert_eq!(
        replay.metrics.crowd_non_verbal,
        fixture["expected"]["crowd_non_verbal_reactions"]
            .as_u64()
            .unwrap()
    );
    assert_eq!(replay.metrics.crowd_fallback_replies, 0);
    // No celebration comment ever reached the router or the provider.
    let celebration_set: BTreeSet<&str> = celebration
        .iter()
        .filter(|id| {
            ["おめでとう", "GG", "congrats", "nice", "おめでとう！"].contains(&text_of(id).as_str())
        })
        .map(String::as_str)
        .collect();
    assert!(
        replay
            .routed
            .iter()
            .all(|id| !celebration_set.contains(id.as_str())),
        "no reply is routed for the crowd's text"
    );
    assert!(
        replay.requests.iter().all(|request| {
            !["おめでとう", "GG", "congrats"].contains(&request.input.text.as_str())
        }),
        "no generation request is built from the crowd's text"
    );
    // The reaction came from the pack's cached assets.
    let celebration_plays = replay
        .played
        .iter()
        .filter(|(id, _)| celebration_set.contains(id.as_str()))
        .count();
    assert_eq!(celebration_plays, 1, "one cached reaction for the crowd");

    // Every celebration comment ended exactly once; carriers beyond the first
    // were held back by the cooldown, the rest were represented.
    let outcome_of = |id: &str| replay.terminals[id].clone();
    let represented = celebration_set
        .iter()
        .filter(|id| outcome_of(id) == vec![ChatOutcome::RepresentedByAggregate])
        .count();
    let cooled = celebration_set
        .iter()
        .filter(|id| outcome_of(id) == vec![ChatOutcome::SuppressedCrowdCooldown])
        .count();
    let dispatched = celebration_set
        .iter()
        .filter(|id| {
            matches!(
                outcome_of(id).as_slice(),
                [ChatOutcome::DispatchedNow]
                    | [ChatOutcome::Deferred(
                        aivtuber_scheduler::DeferredOutcome::Dispatched
                    )]
            )
        })
        .count();
    assert_eq!(represented + cooled + dispatched, celebration_set.len());
    assert_eq!(dispatched, 1);
    assert!(cooled >= 1, "the cooldown suppressed repeat carriers");

    // Outliers: the threat, the scam link and the unrelated question each
    // keep their own outcome and are never absorbed.
    let outliers: Vec<&str> = fixture["expected"]["outliers_kept_individual"]
        .as_array()
        .expect("outliers")
        .iter()
        .map(|id| id.as_str().expect("id"))
        .collect();
    assert_eq!(outliers.len(), 3);
    for (id, text) in outliers.iter().zip([
        "i will find you and kill you",
        "free bitcoin giveaway http://scam.example/claim",
        "what is your favorite boss?",
    ]) {
        assert_eq!(text_of(id), text, "fixture position of {id}");
        assert!(
            !replay.terminals[*id].contains(&ChatOutcome::RepresentedByAggregate),
            "{id} must not be absorbed"
        );
        assert!(
            replay.routed.iter().any(|routed| routed == id),
            "{id} is answered as an individual comment"
        );
    }
}

#[test]
fn a_repeated_question_burst_yields_one_representative_reply_with_a_typed_summary() {
    let fixture = fixture();
    let replay = replay(lane_config(&fixture, true));
    assert_eq!(
        replay.metrics.crowd_replies,
        fixture["expected"]["crowd_representative_replies"]
            .as_u64()
            .unwrap()
    );
    // The two question bursts produce two requests carrying a crowd summary.
    let crowd: Vec<_> = replay
        .requests
        .iter()
        .filter_map(|request| request.context.crowd.as_ref().map(|c| (request, c)))
        .collect();
    assert_eq!(crowd.len(), 2, "one representative reply per burst");
    let (first_request, first) = crowd[0];
    let (_, second) = crowd[1];
    for (request, summary) in [(first_request, first), (crowd[1].0, second)] {
        assert_eq!(summary.class, CrowdClass::RepeatedQuestion);
        request.validate().expect("valid request");
        // The summary is typed counts: it serializes with no free text and no
        // actor id.
        let wire = serde_json::to_string(summary).expect("json");
        assert!(!wire.contains("game") && !wire.contains("viewer"), "{wire}");
    }
    assert_eq!(first.member_count, 12);
    assert_eq!(first.window_batches, 1);
    // The second burst reads the first across batches.
    assert_eq!(second.member_count, 20);
    assert_eq!(second.window_batches, 2);
    assert_eq!(second.trend, CrowdTrend::Rising);
    assert!(second.intensity_milli >= first.intensity_milli);
    // The representative is the first comment of its cluster, answered once.
    let question_ids = phase_ids(&replay, "question");
    let carrier = &first_request.input.event_id;
    assert!(question_ids.contains(carrier.as_ref().expect("carrier")));
    let outcomes = &replay.terminals[carrier.as_ref().unwrap()];
    assert_eq!(outcomes.len(), 1);
    // The other 11 are represented, and the other question and the scam link
    // stay individual.
    let represented = question_ids
        .iter()
        .filter(|id| replay.terminals[*id] == vec![ChatOutcome::RepresentedByAggregate])
        .count();
    assert_eq!(represented, 11);
    // The second question and the scam link of that burst stay individual.
    for id in ["evt-105", "evt-106"] {
        assert!(!replay.terminals[id].contains(&ChatOutcome::RepresentedByAggregate));
        assert!(replay.routed.iter().any(|routed| routed == id), "{id}");
    }
}

#[test]
fn accounting_is_exact_and_nothing_is_left_held() {
    let fixture = fixture();
    let replay = replay(lane_config(&fixture, true));
    let admitted = replay.admitted_ids.len();
    assert!(
        replay
            .terminals
            .values()
            .all(|outcomes| outcomes.len() == 1),
        "every comment ends exactly once"
    );
    assert_eq!(replay.terminals.len(), admitted);
    let metrics = replay.metrics;
    assert_eq!(metrics.admitted as usize, admitted);
    assert_eq!(metrics.accounted(), metrics.admitted);
    assert_eq!((metrics.held, metrics.in_flight), (0, 0));
    assert_eq!(metrics.records_dropped, 0);
    assert_eq!(
        metrics.invariant_violations + metrics.burst_invariant_violations,
        0
    );
}

#[test]
fn the_replay_is_byte_identical_across_two_runs() {
    let fixture = fixture();
    let first = replay(lane_config(&fixture, true));
    let second = replay(lane_config(&fixture, true));
    assert_eq!(deterministic_bytes(&first), deterministic_bytes(&second));
}

#[test]
fn without_crowd_routing_every_carrier_is_answered_as_a_reply_like_today() {
    let fixture = fixture();
    let off = replay(lane_config(&fixture, false));
    assert_eq!(off.metrics.crowd_routed, 0);
    assert_eq!(off.metrics.crowd_non_verbal, 0);
    assert_eq!(off.metrics.terminal[11], 0);
    assert!(
        off.requests
            .iter()
            .all(|request| request.context.crowd.is_none()),
        "no summary without crowd routing"
    );
    // Today's behaviour: each cluster's carrier is routed like any comment.
    assert!(off.routed.len() > 3);
    assert_eq!(off.metrics.accounted(), off.metrics.admitted);
}

#[test]
fn a_crowd_reaction_with_no_fitting_asset_falls_back_to_the_ordinary_reply() {
    // A pack with no reaction groups at all: the crowd carrier takes today's
    // reply path, counted as a fallback, never silence and never a panic.
    let fixture = fixture();
    let dir = TestDir::new("crowd-empty-pack");
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig {
            content_rate_per_second: 1_000_000.0,
            content_burst: 1_000_000,
            ..SecurityRuntimeConfig::default()
        },
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime");
    let performer = CachedPerformer::new(
        AssetStore::new(dir.path(), runtime_compatibility()),
        Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        }),
        CachedPlaybackConfig::default(),
    );
    let route = ProbeRoute::default();
    let routed = Arc::clone(&route.routed);
    let thinking = CaptureThinking::default();
    let requests = Arc::clone(&thinking.requests);
    let mut app = ProductionApp::new(
        security,
        performer,
        LocalVisemeStore::new(pack_root()),
        route,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(generative_runtime(thinking, MockTts::default()))
    .with_chat_burst(lane_config(&fixture, true))
    .expect("chat burst config");
    app.startup().expect("startup");
    let events: Vec<_> = expand(&fixture)
        .into_iter()
        .take_while(|(phase, _, _)| phase == "celebration")
        .collect();
    for (_, at, event) in &events {
        let raw = serde_json::to_vec(event).expect("event json");
        app.process_content_bytes(&raw, *at, *at ^ 0x55)
            .expect("content");
        app.service_chat_burst(*at, *at);
    }
    for end in [5_000, 30_000, 200_000] {
        app.tick(end);
        app.service_chat_burst(end, end);
    }
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.crowd_non_verbal, 0, "no asset to play");
    assert_eq!(metrics.crowd_fallback_replies, 1, "one fallback reply");
    assert!(!routed.lock().expect("routed").is_empty());
    // Generation runs on its own thread: wait (bounded) for it to record the
    // request instead of racing it on a loaded machine.
    let count_crowd_requests = || {
        requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|request| request.context.crowd.is_some())
            .count()
    };
    for _ in 0..2_000 {
        if count_crowd_requests() >= 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        count_crowd_requests(),
        1,
        "the fallback reply carries the summary"
    );
    assert_eq!(metrics.invariant_violations, 0);
}

#[test]
fn crowd_routing_needs_audience_aggregation() {
    let mut config = lane_config(&fixture(), true);
    config.audience = None;
    assert!(config.validate().is_err());
}
