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
use std::path::Path;

const FIXTURE: &str = include_str!("../../../../examples/crowd-routing/falsifier.json");
/// Same lane shape, waves spaced beyond the crowd cooldown: seven routes with
/// observable crowd stages (#489 evidence without a live hour run).
const TIMING_FIXTURE: &str =
    include_str!("../../../../examples/crowd-routing/timing-scenario.json");

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
    avatar: Arc<Mutex<Vec<AvatarPlaybackCommand>>>,
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
    let thinking = CaptureThinking::default();
    let requests = Arc::clone(&thinking.requests);
    let mut rig = rig_with_thinking(config, thinking);
    rig.requests = requests;
    rig
}

/// The same rig with the caller's thinking engine, so a test can hold the
/// single generation worker busy (#79 S5).
fn rig_with_thinking<Thinking>(config: ChatBurstConfig, thinking: Thinking) -> Rig
where
    Thinking: ThinkingEngine + 'static,
{
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
    let recording_avatar = RecordingAvatar::default();
    let avatar = Arc::clone(&recording_avatar.commands);
    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        route,
        Box::new(RecordingAudio::default()),
        Box::new(recording_avatar),
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
        requests: Arc::new(Mutex::new(Vec::new())),
        avatar,
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
    replay_fixture(config, FIXTURE, None).0
}

/// Replay any fixture of the falsifier's shape. With `log_dir` the lane writes
/// the opt-in session log (#463), so the offline summary can reconstruct the
/// crowd stages (#494); that summary is returned beside the replay.
fn replay_fixture(
    config: ChatBurstConfig,
    fixture_text: &str,
    log_dir: Option<&Path>,
) -> (Replay, Option<crate::session_log::SessionSummary>) {
    let fixture: serde_json::Value = serde_json::from_str(fixture_text).expect("fixture");
    let mut rig = rig(config);
    if let Some(dir) = log_dir {
        rig.app = rig
            .app
            .with_session_log(crate::session_log::SessionLog::new(
                dir.join("logs"),
                None,
                true,
            ));
    }
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
        let declined_before = rig.app.guarded_declined();
        let outcome = rig
            .app
            .process_content_bytes(&raw, *at, *at ^ 0x55)
            .expect("content");
        // A scam or link lure is declined before the lane (#79): it has no
        // lane outcome and is never answered.
        if rig.app.guarded_declined() == declined_before {
            admitted_ids.push((event.event_id.clone(), phase.clone()));
        }
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
    let summary = log_dir.and_then(|dir| {
        rig.app.session_log().expect("log").flush();
        let file = std::fs::read_dir(dir.join("logs"))
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .last()?;
        let bytes = std::fs::read(file).ok()?;
        Some(crate::session_log::summarize(bytes.as_slice()))
    });
    (
        Replay {
            terminals,
            signals,
            played,
            routed,
            requests,
            metrics,
            admitted_ids,
        },
        summary,
    )
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
        if text.contains("http") {
            // Lure: declined before the lane, never selected or routed.
            assert!(!replay.terminals.contains_key(*id), "{id}");
            assert!(!replay.routed.iter().any(|routed| routed == id), "{id}");
            continue;
        }
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
    assert!(!replay.terminals["evt-106"].contains(&ChatOutcome::RepresentedByAggregate));
    assert!(replay.routed.iter().any(|routed| routed == "evt-106"));
    // The scam link (evt-105) is declined before the lane and never answered.
    assert!(!replay.terminals.contains_key("evt-105"));
    assert!(!replay.routed.iter().any(|routed| routed == "evt-105"));
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
fn a_crowd_route_is_written_to_the_session_log_with_counts_only() {
    use crate::session_log::{Record, SessionLog};
    let fixture = fixture();
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-crowd-log-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let mut rig = rig(lane_config(&fixture, true));
    rig.app = rig
        .app
        .with_session_log(SessionLog::new(dir.join("logs"), None, true));
    for (_, at, event) in expand(&fixture)
        .into_iter()
        .take_while(|(phase, _, _)| phase == "celebration")
    {
        let raw = serde_json::to_vec(&event).expect("event json");
        rig.app
            .process_content_bytes(&raw, at, at ^ 0x55)
            .expect("content");
        rig.app.service_chat_burst(at, at);
    }
    for end in [5_000, 30_000] {
        rig.app.tick(end);
        rig.app.service_chat_burst(end, end);
    }
    rig.app.session_log().expect("log").flush();
    let file = std::fs::read_dir(dir.join("logs"))
        .expect("logs")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .last()
        .expect("a session file");
    let raw = std::fs::read_to_string(file).expect("read");
    let records: Vec<Record> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("record"))
        .collect();
    let crowds: Vec<&Record> = records.iter().filter(|r| r.t == "crowd").collect();
    assert!(!crowds.is_empty(), "a routed crowd is logged");
    for crowd in &crowds {
        assert!(crowd.members.unwrap_or(0) >= 3 && crowd.actors.unwrap_or(0) >= 2);
        assert!(crowd.text.is_none() && crowd.viewer.is_none());
        assert!(crowd.turn.is_some());
    }
    let summary = crate::session_log::summarize(raw.as_bytes());
    assert_eq!(summary.crowd_routes.len(), crowds.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crowd_routing_needs_audience_aggregation() {
    let mut config = lane_config(&fixture(), true);
    config.audience = None;
    assert!(config.validate().is_err());
}

fn mute_command() -> AuthenticatedControlCommand {
    let secret = [0x47_u8; 32];
    LocalControlIngress::new(
        "local-test",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([Capability::PerformerMute]),
        ControlSecret::new(secret),
    )
    .expect("control ingress")
    .authenticate(
        OperatorCommandInput {
            event_id: "evt-performer.mute".to_owned(),
            correlation_id: "corr-crowd".to_owned(),
            sequence: 97,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            action: "performer.mute".to_owned(),
            payload: BTreeMap::new(),
        },
        &secret,
    )
    .expect("authenticated command")
}

fn read_session(dir: &std::path::Path) -> Vec<crate::session_log::Record> {
    let file = std::fs::read_dir(dir.join("logs"))
        .expect("logs")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .last()
        .expect("a session file");
    std::fs::read_to_string(file)
        .expect("read")
        .lines()
        .map(|line| serde_json::from_str(line).expect("record"))
        .collect()
}

#[test]
fn a_crowd_route_while_the_performer_is_busy_ends_in_one_recorded_outcome() {
    // #79: the carrier waits behind a busy performer and its reply time-to-live
    // passes. The lane already counted that; the session log must record it
    // too, once, with a reason, and no viewer text.
    use crate::session_log::SessionLog;
    let fixture = fixture();
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-crowd-busy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let mut rig = rig(lane_config(&fixture, true));
    rig.app = rig
        .app
        .with_session_log(SessionLog::new(dir.join("logs"), None, true));
    rig.app
        .handle_control_reporting(&mute_command(), 0)
        .expect("mute");
    for (_, at, event) in expand(&fixture)
        .into_iter()
        .take_while(|(phase, _, _)| phase == "celebration")
    {
        let raw = serde_json::to_vec(&event).expect("event json");
        rig.app
            .process_content_bytes(&raw, at, at ^ 0x55)
            .expect("content");
        rig.app.service_chat_burst(at, at);
    }
    for end in [5_000, 30_000, 200_000] {
        rig.app.tick(end);
        rig.app.service_chat_burst(end, end);
    }
    rig.app.shutdown(200_001);
    rig.app.session_log().expect("log").flush();
    let records = read_session(&dir);
    let crowds: Vec<_> = records.iter().filter(|r| r.t == "crowd").collect();
    assert!(!crowds.is_empty(), "the crowd was routed");
    for crowd in &crowds {
        let turn = crowd.turn.as_ref().expect("turn");
        let ends: Vec<_> = records
            .iter()
            .filter(|r| r.t == "fallback" && r.turn.as_ref() == Some(turn))
            .collect();
        assert_eq!(ends.len(), 1, "one recorded end per routed crowd");
        assert!(
            matches!(
                ends[0].fallback.as_deref(),
                Some("expired_while_busy" | "crowd_cooldown")
            ),
            "got {:?}",
            ends[0].fallback
        );
        assert!(ends[0].text.is_none() && ends[0].viewer.is_none());
    }
    let metrics = rig.app.chat_burst_metrics().expect("metrics");
    assert!(metrics.crowd_unvoiced >= 1);
    assert_eq!(metrics.invariant_violations, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crowd_route_that_is_voiced_has_no_unvoiced_end() {
    // The same burst with an available performer: the carrier turn is
    // answered, so there is no fallback record (only the crowd and its reply).
    use crate::session_log::SessionLog;
    let fixture = fixture();
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-crowd-voiced-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let mut rig = rig(lane_config(&fixture, true));
    rig.app = rig
        .app
        .with_session_log(SessionLog::new(dir.join("logs"), None, true));
    for (_, at, event) in expand(&fixture)
        .into_iter()
        .take_while(|(phase, _, _)| phase == "celebration")
    {
        let raw = serde_json::to_vec(&event).expect("event json");
        rig.app
            .process_content_bytes(&raw, at, at ^ 0x55)
            .expect("content");
        rig.app.service_chat_burst(at, at);
    }
    for end in [5_000, 30_000] {
        rig.app.tick(end);
        rig.app.service_chat_burst(end, end);
    }
    rig.app.session_log().expect("log").flush();
    let records = read_session(&dir);
    let carriers: Vec<_> = records.iter().filter(|r| r.t == "crowd").collect();
    assert!(!carriers.is_empty());
    for crowd in carriers {
        let turn = crowd.turn.as_ref().expect("turn");
        // A carrier suppressed by the crowd cooldown legitimately has its own
        // end; none may have expired or failed.
        assert!(
            records
                .iter()
                .filter(|r| r.t == "fallback" && r.turn.as_ref() == Some(turn))
                .all(|r| r.fallback.as_deref() == Some("crowd_cooldown")),
            "a dispatched crowd carrier is not an unvoiced end"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cached_crowd_reaction_does_not_wait_for_a_generation_in_flight() {
    // #79 S5. Track A (60 min, 2026-11-26) measured the crowd's
    // route-to-dispatch at 3,175 ms of its 4,736 ms route-to-first-phrase while
    // one generation worker was busy. A celebration wave is answered by a
    // reviewed cached line, so that worker is not needed for it at all: the
    // wave's wait is the worker's queue, not the crowd's work.
    let fixture = fixture();
    let gate = Arc::new(BlockingGate::default());
    let mut rig = rig_with_thinking(
        lane_config(&fixture, true),
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "a generated reply".to_owned(),
        },
    );
    let dir = TestDir::new("crowd-generation-in-flight");
    rig.app = rig
        .app
        .with_session_log(crate::session_log::SessionLog::new(
            dir.path().join("logs"),
            None,
            true,
        ));

    // An ordinary reply holds the single generation worker.
    let mut direct = chat_event(90_001);
    direct.payload.remove("intent");
    rig.app
        .dispatch_admitted_event(direct, 1_000, 1, std::time::Instant::now())
        .expect("the generation starts");
    gate.wait_until_entered();
    assert!(
        rig.app
            .generation_cancellation_registry()
            .and_then(|registry| registry.active_event_id())
            .is_some(),
        "a generation is in flight"
    );

    // The wave lands while that worker is busy.
    for (_, at, event) in expand(&fixture)
        .into_iter()
        .take_while(|(phase, _, _)| phase == "celebration")
    {
        let at = at + 2_000;
        let raw = serde_json::to_vec(&event).expect("event json");
        rig.app
            .process_content_bytes(&raw, at, at ^ 0x55)
            .expect("content");
        rig.app.service_chat_burst(at, at);
    }
    rig.app.service_chat_burst(6_000, 6_000);

    // Answered from the pack while the worker is still held: this is the
    // carrier, so it fails if the wave waited for the worker.
    let metrics = rig.app.chat_burst_metrics().expect("metrics");
    assert!(
        metrics.crowd_non_verbal >= 1,
        "the wave was answered from the pack while the worker was busy: {metrics:?}"
    );
    let reports = rig.app.take_deferred_reply_reports(usize::MAX);
    assert!(
        reports
            .iter()
            .any(|report| report.dispatched && report.asset_id.is_some()),
        "the crowd reaction was dispatched from the pack: {reports:?}"
    );

    // Releasing the worker must leave the deterministic performer intact.
    gate.release();
    for step in 1..=40_u64 {
        let at = 6_000 + step * 250;
        rig.app.tick(at);
        rig.app.service_chat_burst(at, at);
    }
    rig.app.shutdown(30_000);
    rig.app.session_log().expect("log").flush();
    assert_eq!(
        rig.app
            .chat_burst_metrics()
            .expect("metrics")
            .invariant_violations,
        0
    );

    // The offline reader sees the wave answered with no route→dispatch wait.
    let file = std::fs::read_dir(dir.path().join("logs"))
        .expect("logs")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .last()
        .expect("a session file");
    let bytes = std::fs::read(file).expect("read");
    let summary = crate::session_log::summarize(bytes.as_slice());
    assert!(!summary.crowd_routes.is_empty(), "the crowd was routed");
    // The first wave of the run is the one Track A measured: it must reach
    // dispatch at the routing instant, not after the worker freed up.
    assert_eq!(
        summary.crowd_routes[0].timing.route_to_dispatch_ms,
        Some(0),
        "the cached reaction was dispatched at the routing instant: {:?}",
        summary.crowd_routes
    );
    let _ = std::fs::remove_dir_all(dir.path());
}

#[test]
fn a_representative_reply_crowd_still_waits_for_the_generation_worker() {
    // #79 S5 keeps the exception narrow. Only a carrier the pack answers skips
    // the queue; a repeated question is answered by generated text, so it must
    // wait for the worker and be answered by it, never be thrown out of the
    // queue as if a cached line existed for it.
    let fixture = fixture();
    let gate = Arc::new(BlockingGate::default());
    let mut rig = rig_with_thinking(
        lane_config(&fixture, true),
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "a farming sim".to_owned(),
        },
    );

    // An ordinary reply holds the single generation worker.
    let mut direct = chat_event(90_002);
    direct.payload.remove("intent");
    rig.app
        .dispatch_admitted_event(direct, 1_000, 1, std::time::Instant::now())
        .expect("the generation starts");
    gate.wait_until_entered();

    // A repeated question is routed as a representative reply, not a pack line.
    for (_, at, event) in expand(&fixture)
        .into_iter()
        .filter(|(phase, _, _)| phase == "question")
    {
        let raw = serde_json::to_vec(&event).expect("event json");
        rig.app
            .process_content_bytes(&raw, at, at ^ 0x55)
            .expect("content");
        rig.app.service_chat_burst(at, at);
    }
    rig.app.service_chat_burst(62_000, 62_000);

    let metrics = rig.app.chat_burst_metrics().expect("metrics");
    assert!(
        metrics.crowd_routed >= 1,
        "the repeated question was routed: {metrics:?}"
    );
    assert_eq!(
        metrics.crowd_non_verbal, 0,
        "a repeated question is not answered from the pack: {metrics:?}"
    );
    assert_eq!(
        metrics.crowd_replies, 0,
        "the generated crowd reply waited for the worker: {metrics:?}"
    );
    assert!(
        metrics.deferred.pending >= 1,
        "the carrier sits in the deferred inbox: {metrics:?}"
    );

    // Once the worker frees up, the representative reply is generated.
    gate.release();
    for step in 1..=40_u64 {
        let at = 62_000 + step * 250;
        rig.app.tick(at);
        rig.app.service_chat_burst(at, at);
        wait_for_generation_completion(&mut rig.app, at);
    }
    let metrics = rig.app.chat_burst_metrics().expect("metrics");
    assert!(
        metrics.crowd_replies >= 1,
        "the representative reply was answered after the worker freed: {metrics:?}"
    );
    assert_eq!(metrics.invariant_violations, 0);
}

#[test]
fn a_crowd_carrier_is_dispatched_before_older_deferred_replies_and_is_voiced() {
    // #79: a crowd lands while a reply is speaking and an older single-viewer
    // reply already waits. When the speech ends, the crowd reaction goes
    // first: it stands for many viewers. Same 20 s TTL, no second queue.
    let fixture = fixture();
    let mut rig = rig(lane_config(&fixture, true));
    let app = &mut rig.app;
    let duration = 6_000;
    let plan = aivtuber_scheduler::PlannedPerformance {
        event_id: "evt-speaking".to_owned(),
        asset_id: "asset.speaking".to_owned(),
        priority: aivtuber_scheduler::Priority::Conversation,
        interruptible: true,
        interrupt_points_ms: vec![duration],
        start_at_ms: 0,
        duration_ms: duration,
        generation: 0,
        exclusive: false,
        channels: BTreeSet::from([
            aivtuber_scheduler::BlendChannel::Audio,
            aivtuber_scheduler::BlendChannel::Face,
        ]),
    };
    let planned = app
        .performer
        .scheduler_mut()
        .schedule(plan)
        .expect("schedule speech");
    app.tick(0);
    let end = planned.end_at_ms();
    let old_text = "what game is this again";
    let mut old = chat_event(9_001);
    old.actor_id = Some("viewer:old".to_owned());
    old.payload
        .insert("text".to_owned(), serde_json::json!(old_text));
    old.payload.remove("intent");
    let raw = serde_json::to_vec(&old).expect("json");
    app.process_content_bytes(&raw, 1, 1 ^ 0x55).expect("old");
    let old_id = old.event_id.clone();
    for (_, at, event) in expand(&fixture)
        .into_iter()
        .take_while(|(phase, _, _)| phase == "celebration")
    {
        let at = at + 2;
        let raw = serde_json::to_vec(&event).expect("event json");
        app.process_content_bytes(&raw, at, at ^ 0x55)
            .expect("content");
        app.service_chat_burst(at, at);
    }
    app.service_chat_burst(3_000, 3_000);
    assert!(
        app.chat_burst_metrics().expect("metrics").deferred.pending >= 2,
        "the old reply and the crowd carrier both wait behind the speech"
    );
    let mut reports = Vec::new();
    for at in [end + 1, end + 250, end + 500, end + 3_000] {
        app.tick(at);
        app.service_chat_burst(at, at);
        reports.extend(app.take_deferred_reply_reports(usize::MAX));
    }
    let first = reports.first().expect("a deferred dispatch");
    assert_ne!(
        first.event_id, old_id,
        "the crowd goes before the old reply"
    );
    assert_ne!(text_of(&first.event_id), old_text);
    assert!(first.dispatched);
    assert!(first.asset_id.is_some(), "the crowd reaction is voiced");
    // #79 S4: the avatar acknowledged the wave at once, while the speech was
    // still playing, through the existing host-direct Gesture command.
    let nods: Vec<_> = rig
        .avatar
        .lock()
        .expect("avatar log")
        .iter()
        .filter_map(|command| match command {
            AvatarPlaybackCommand::Gesture {
                generation: 0,
                at_ms,
                preset,
                amplitude,
            } if preset == "nod_small" => Some((*at_ms, *amplitude)),
            _ => None,
        })
        .collect();
    assert!(
        nods.iter().any(|(at, amp)| *at < end
            && (*amp - deferred_ack::ACK_CROWD_GESTURE_AMPLITUDE).abs() < 1e-9),
        "a crowd nod before the speech ended: {nods:?} {:?}",
        rig.app.chat_burst_metrics().expect("metrics").ack
    );
    assert_eq!(
        rig.app
            .chat_burst_metrics()
            .expect("metrics")
            .ack
            .acked_crowd,
        1
    );
}

/// The reviewed crowd lines, read from the pack descriptors on disk.
fn crowd_lines() -> BTreeMap<String, Vec<String>> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in fs::read_dir(pack_root().join("descriptors")).expect("descriptors") {
        let path = entry.expect("entry").path();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
        let group = value["variant_group"].as_str().unwrap_or_default();
        if group.starts_with("crowd.") {
            groups
                .entry(group.to_owned())
                .or_default()
                .push(value["speech"]["text"].as_str().expect("text").to_owned());
        }
    }
    groups
}

#[test]
fn every_crowd_class_ships_two_to_four_short_lines() {
    let lines = crowd_lines();
    for group in ["celebration", "laughter", "surprise", "greeting"] {
        let texts = &lines[&format!("crowd.{group}")];
        assert!((2..=4).contains(&texts.len()), "{group}: {texts:?}");
        for text in texts {
            assert!(text.chars().count() <= 12, "{text} is too long");
            // Never quotes a viewer or names one: fixed wording, no
            // placeholders, digits, @-mentions or latin text.
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_ascii_alphanumeric() || c == '@' || c == '{'),
                "{text}"
            );
        }
        let unique: BTreeSet<_> = texts.iter().collect();
        assert_eq!(unique.len(), texts.len(), "{group} has a duplicate line");
    }
    // Sympathy has no line: it keeps its speechless fallback.
    assert!(!lines.contains_key("crowd.sympathy"));
}

/// Play `n` crowd reactions of `group` back to back and return the asset ids.
fn play_group(group: aivtuber_reflex::CrowdReactionGroup, n: u64) -> Vec<String> {
    let fixture = fixture();
    let mut rig = rig(lane_config(&fixture, true));
    let mut ids = Vec::new();
    for i in 0..n {
        let mut event = chat_event(100 + i);
        event.payload.remove("intent");
        let at = 10_000 * (i + 1);
        let played = rig
            .app
            .play_crowd_reaction(&event, group, at, 7)
            .expect("the pack has a line for this group")
            .expect("playback")
            .expect("outcome");
        ids.push(played.asset_id);
        rig.app.tick(at + 9_000);
    }
    ids
}

#[test]
fn each_crowd_group_plays_only_a_line_of_its_own_class() {
    use aivtuber_reflex::CrowdReactionGroup as G;
    for (group, prefix) in [
        (G::Celebration, "crowd.celebration."),
        (G::Laughter, "crowd.laughter."),
        (G::Greeting, "crowd.greeting."),
        (G::Surprise, "crowd.surprise."),
    ] {
        let ids = play_group(group, 12);
        assert!(
            ids.iter().all(|id| id.starts_with(prefix)),
            "{prefix}: {ids:?}"
        );
        // 12 plays reach more than one line: it rotates, it is not stuck.
        assert!(ids.iter().collect::<BTreeSet<_>>().len() >= 2, "{ids:?}");
    }
}

#[test]
fn a_crowd_line_never_repeats_back_to_back_and_replays_identically() {
    use aivtuber_reflex::CrowdReactionGroup as G;
    for group in [G::Celebration, G::Laughter, G::Greeting, G::Surprise] {
        let first = play_group(group, 24);
        assert!(
            first.windows(2).all(|pair| pair[0] != pair[1]),
            "{group:?} repeated back to back: {first:?}"
        );
        assert_eq!(first, play_group(group, 24), "{group:?} is replay-stable");
    }
}

#[test]
fn a_celebration_is_never_given_a_mismatched_generic_line() {
    use aivtuber_reflex::CrowdReactionGroup as G;
    for group in [G::Celebration, G::Laughter, G::Greeting] {
        assert!(
            chat_burst::crowd_asset_groups(group)
                .iter()
                .all(|g| g.starts_with("crowd.")),
            "{group:?} must not fall back to a generic reaction line"
        );
    }
    // A pack that has the generic agree lines but no crowd lines: no fitting
    // asset, so the caller takes the generated crowd-level fallback.
    let dir = TestDir::new("crowd-agree-only");
    for name in ["reaction-agree-01.json", "reaction-agree-02.json"] {
        fs::copy(
            pack_root().join("descriptors").join(name),
            dir.path().join(name),
        )
        .expect("copy");
    }
    let fixture = fixture();
    let mut rig = rig(lane_config(&fixture, true));
    rig.app.performer = CachedPerformer::new(
        AssetStore::new(dir.path(), runtime_compatibility()),
        Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        }),
        CachedPlaybackConfig::default(),
    );
    let mut event = chat_event(1);
    event.payload.remove("intent");
    assert!(
        rig.app
            .play_crowd_reaction(&event, G::Celebration, 1_000, 1)
            .is_none()
    );
}

/// #489 evidence before an hour-long live run: the checked-in timing scenario
/// routes several crowds deterministically (waves spaced beyond the crowd
/// cooldown), so the offline summary reports the crowd stage distributions
/// from a replay of the production lane instead of a live session. The numbers
/// are logical-time intervals between existing log records, not acoustic or
/// visible onset, and they do not replace a representative hour.
#[test]
fn deterministic_timing_scenario_reports_crowd_stage_distributions() {
    let fixture: serde_json::Value = serde_json::from_str(TIMING_FIXTURE).expect("fixture");
    let expected = fixture["expected"]["crowd_routes"]
        .as_u64()
        .expect("crowd_routes");
    let dir = TestDir::new("crowd-timing");
    let (first, summary) = replay_fixture(
        lane_config(&fixture, true),
        TIMING_FIXTURE,
        Some(dir.path()),
    );
    let summary = summary.expect("session summary");
    assert_eq!(summary.malformed, 0, "every log line parses");
    assert!(!summary.capped && !summary.truncated_read, "a complete log");
    assert_eq!(
        summary.crowd_routes.len() as u64,
        expected,
        "every wave routes exactly once: {:?}",
        summary.crowd_routes
    );
    assert_eq!(first.metrics.invariant_violations, 0);

    // The scenario's shape is part of the evidence: both reaction classes and
    // one representative reply, and the overlapping wave ends unvoiced.
    let class_count = |class: &str| {
        summary
            .crowd_routes
            .iter()
            .filter(|route| route.class == class)
            .count()
    };
    assert_eq!(class_count("congratulation"), 6);
    assert_eq!(class_count("laughter"), 2);
    assert_eq!(class_count("repeated_question"), 1);
    assert!(
        summary
            .crowd_routes
            .iter()
            .any(|route| route.timing.route_to_phrase_ms.is_some())
            && summary
                .crowd_routes
                .iter()
                .any(|route| route.timing.route_to_phrase_ms.is_none()),
        "the scenario covers a voiced and an unvoiced route: {:?}",
        summary.crowd_routes
    );

    let report = crate::session_log::render_summary(&summary);
    println!("{report}");
    let stage_line = |stage: &str| {
        let prefix = format!("crowd_timing_ms stage={stage} ");
        report
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("stage {stage} missing from the summary"))
            .to_owned()
    };
    let dispatch = stage_line("route_observation_to_dispatch");
    assert!(
        dispatch.contains(&format!("count={expected} routes={expected} ")),
        "every route has both endpoints for dispatch: {dispatch}"
    );
    // #79 S5: a pack-answered carrier is dispatched at the routing instant, and
    // this scenario holds no generation worker, so no route waits to dispatch.
    assert!(
        dispatch.contains("p50=0 p95=0 max=0"),
        "no crowd route waits between routing and dispatch: {dispatch}"
    );
    let phrase = stage_line("route_observation_to_first_phrase");
    assert!(
        phrase.contains(&format!("routes={expected} ")) && !phrase.contains("count=0 "),
        "the unvoiced route is excluded from the phrase distribution: {phrase}"
    );

    // Determinism: the same fixture replays to the same distributions.
    let again_dir = TestDir::new("crowd-timing-again");
    let (_, again) = replay_fixture(
        lane_config(&fixture, true),
        TIMING_FIXTURE,
        Some(again_dir.path()),
    );
    assert_eq!(
        report,
        crate::session_log::render_summary(&again.expect("summary")),
        "the replay is deterministic"
    );

    // Evidence artifact for #489 (target/ is not committed): the stages this
    // replay measured, per route, plus the summary the reader produced.
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/crowd-routing-timing");
    std::fs::create_dir_all(&out).expect("out dir");
    let id = fixture["fixture_id"].as_str().expect("fixture_id");
    let artifact = serde_json::json!({
        "fixture_id": id,
        "issue": "#79 / #489",
        "method": "deterministic replay of the production lane; logical time; session-log summary; not a live run",
        "routes": summary.crowd_routes.len(),
        "crowd_routes": summary
            .crowd_routes
            .iter()
            .map(|route| serde_json::json!({
                "turn": route.turn,
                "class": route.class,
                "members": route.members,
                "actors": route.actors,
                "phrases": route.phrases,
                "cached_phrases": route.cached_phrases,
                "first_audio_ms": route.first_audio_ms,
                "input_to_route_ms": route.timing.input_to_route_ms,
                "route_to_dispatch_ms": route.timing.route_to_dispatch_ms,
                "dispatch_to_phrase_ms": route.timing.dispatch_to_phrase_ms,
                "route_to_phrase_ms": route.timing.route_to_phrase_ms,
            }))
            .collect::<Vec<_>>(),
        "summary": report,
    });
    std::fs::write(
        out.join(format!("{id}.json")),
        serde_json::to_vec_pretty(&artifact).expect("artifact json"),
    )
    .expect("write artifact");
}
