//! App-level tests of phrase-level commit (issue #83 S1). No model, no
//! network: Thinking and TTS are stubs, time is the logical clock the test
//! advances itself.

use super::*;
use aivtuber_generative::{
    Delivery, GenerationDisposition, GenerativePipeline, PhraseConfig, PhraseDelivery, PhraseEnd,
    PhraseUnit, SlotWait,
};
use std::sync::atomic::AtomicBool;

const PHRASE_A: &str = "今日は配信に来てくれてありがとうございます。";
const PHRASE_B: &str = "このあとはゲームを遊びながらお話しします。";
const PHRASE_C: &str = "最後まで楽しんでいってくださいね。";

fn three_phrase_reply() -> String {
    format!("{PHRASE_A}{PHRASE_B}{PHRASE_C}")
}

/// 40 ms of audio per character, so durations are exact and no viseme file
/// has to match them.
fn duration_of(text: &str) -> u64 {
    40 * text.chars().count() as u64
}

#[derive(Clone)]
struct PhraseTts {
    calls: Arc<AtomicUsize>,
    texts: Arc<Mutex<Vec<String>>>,
    fail_on_call: Option<usize>,
    /// Block inside this call until `release` is set.
    block_on_call: Option<usize>,
    release: Arc<AtomicBool>,
    base_delay: Duration,
    delay_per_char: Duration,
}

impl PhraseTts {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            texts: Arc::new(Mutex::new(Vec::new())),
            fail_on_call: None,
            block_on_call: None,
            release: Arc::new(AtomicBool::new(false)),
            base_delay: Duration::ZERO,
            delay_per_char: Duration::ZERO,
        }
    }
}

impl TtsEngine for PhraseTts {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.lock().unwrap().push(request.text.clone());
        let chars = request.text.chars().count();
        std::thread::sleep(self.base_delay + self.delay_per_char * chars as u32);
        if self.block_on_call == Some(call) {
            let started = Instant::now();
            while !self.release.load(Ordering::SeqCst)
                && started.elapsed() < Duration::from_secs(10)
            {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let result = if self.fail_on_call == Some(call) {
            Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "scripted phrase failure",
            ))
        } else {
            Ok(SpeechArtifact {
                audio_ref: format!("audio://generated/phrase-{call}.wav"),
                duration_ms: duration_of(&request.text),
                viseme_ref: None,
            })
        };
        Box::pin(async move { result })
    }

    fn identity(&self) -> TtsBackendIdentity {
        TtsBackendIdentity {
            backend: BackendIdentity {
                name: "phrase-tts".to_owned(),
                model_alias: Some("phrase".to_owned()),
                model_version: Some("1".to_owned()),
            },
            voice_model: Some("example-voice-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
        }
    }
}

struct Rig {
    app: ProductionApp<FixedGenerateRoute>,
    tts_calls: Arc<AtomicUsize>,
    tts_texts: Arc<Mutex<Vec<String>>>,
    release: Arc<AtomicBool>,
    audio: Arc<Mutex<Vec<AudioPlaybackCommand>>>,
}

fn rig_with(mode: PhraseMode, reply: &str, tts: PhraseTts) -> Rig {
    let tts_calls = Arc::clone(&tts.calls);
    let tts_texts = Arc::clone(&tts.texts);
    let release = Arc::clone(&tts.release);
    let audio = RecordingAudio::default();
    let audio_log = Arc::clone(&audio.commands);
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime");
    let runtime = generative_runtime(
        MockThinking {
            reply: reply.to_owned(),
            failure: None,
        },
        tts,
    )
    .with_phrase_mode(mode)
    .expect("phrase mode");
    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        Box::new(audio),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(runtime);
    app.startup().expect("startup");
    Rig {
        app,
        tts_calls,
        tts_texts,
        release,
        audio: audio_log,
    }
}

fn phrased() -> PhraseMode {
    PhraseMode::Phrased(PhraseConfig::default())
}

fn submit(rig: &mut Rig, sequence: u64, now_ms: u64) {
    let raw = serde_json::to_vec(&chat_event(sequence)).expect("event json");
    rig.app
        .process_content_bytes(&raw, now_ms, sequence)
        .expect("generation submission");
}

/// Tick at `now_ms` until `condition` holds (real time bound, logical time
/// fixed by the caller).
fn wait_until(rig: &mut Rig, now_ms: u64, mut condition: impl FnMut(&mut Rig) -> bool) {
    for _ in 0..5_000 {
        rig.app.tick(now_ms);
        if condition(rig) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("condition not reached at logical {now_ms} ms");
}

fn audio_count(rig: &Rig) -> usize {
    rig.audio.lock().unwrap().len()
}

fn phrase_items(rig: &Rig) -> Vec<aivtuber_scheduler::ScheduledItem> {
    rig.app
        .performer()
        .scheduler()
        .items()
        .iter()
        .filter(|item| item.phrase.is_some())
        .cloned()
        .collect()
}

fn the_record(rig: &Rig) -> PhraseResponseRecord {
    let records = rig.app.recent_phrase_responses();
    assert_eq!(records.len(), 1, "exactly one phrased reply: {records:?}");
    records[0].clone()
}

#[test]
fn a_reply_is_committed_phrase_by_phrase_with_one_phrase_of_lookahead() {
    let tts = PhraseTts::new();
    let mut rig = rig_with(phrased(), &three_phrase_reply(), tts);
    submit(&mut rig, 30, 0);

    // Phrase 1 and phrase 2 are committed; phrase 1 plays at 0, phrase 2 is
    // queued behind it, and phrase 3 is NOT synthesized yet: the worker waits
    // for phrase 2 to start playing.
    wait_until(&mut rig, 0, |rig| phrase_items(rig).len() == 2);
    std::thread::sleep(Duration::from_millis(150));
    rig.app.tick(0);
    assert_eq!(
        rig.tts_calls.load(Ordering::SeqCst),
        2,
        "synthesis of phrase 3 waits until phrase 2 has started"
    );
    assert_eq!(
        audio_count(&rig),
        1,
        "only phrase 1 has been handed to audio"
    );
    assert!(rig.app.recent_phrase_responses().is_empty(), "not over yet");

    // Phrase 2 starts exactly where phrase 1 ends.
    let a_ms = duration_of(PHRASE_A);
    let b_ms = duration_of(PHRASE_B);
    rig.app.tick(a_ms);
    assert_eq!(audio_count(&rig), 2);
    wait_until(&mut rig, a_ms, |rig| {
        rig.tts_calls.load(Ordering::SeqCst) == 3 && phrase_items(rig).len() == 2
    });

    // Phrase 3 is committed behind phrase 2; then the reply is over.
    rig.app.tick(a_ms + b_ms);
    assert_eq!(audio_count(&rig), 3);
    wait_until(&mut rig, a_ms + b_ms, |rig| {
        !rig.app.recent_phrase_responses().is_empty()
    });

    let record = the_record(&rig);
    assert_eq!(record.event_id, "evt-30");
    assert_eq!(record.planned_phrases, 3);
    assert_eq!(record.end, PhraseResponseEnd::Completed);
    assert_eq!(record.commit_horizon(), 3);
    assert_eq!(
        record.segmentation_version,
        aivtuber_generative::PHRASE_SEGMENTATION_VERSION
    );
    let order: Vec<(u32, u64, u64, u64)> = record
        .commits
        .iter()
        .map(|c| (c.seq, c.start_at_ms, c.end_at_ms, c.gap_before_ms))
        .collect();
    let c_ms = duration_of(PHRASE_C);
    assert_eq!(
        order,
        vec![
            (0, 0, a_ms, 0),
            (1, a_ms, a_ms + b_ms, 0),
            (2, a_ms + b_ms, a_ms + b_ms + c_ms, 0)
        ],
        "commit order, and the phrases are gapless"
    );
    assert!(record.commits[2].final_phrase && !record.commits[1].final_phrase);
    assert!(
        record
            .commits
            .iter()
            .all(|c| c.gate == PhraseGateOutcome::Allowed)
    );

    // The shared scheduler executed the phrases as ordinary items: distinct
    // event ids, an interrupt point at the end of every phrase.
    let commands = rig.audio.lock().unwrap().clone();
    assert_eq!(
        commands
            .iter()
            .map(|c| c.event_id.as_str())
            .collect::<Vec<_>>(),
        vec!["evt-30", "evt-30#p1", "evt-30#p2"]
    );
    assert_eq!(
        commands.iter().map(|c| c.at_ms).collect::<Vec<_>>(),
        vec![0, a_ms, a_ms + b_ms]
    );
    let history: Vec<_> = rig
        .app
        .performer()
        .scheduler()
        .history()
        .map(|entry| entry.item.clone())
        .collect();
    assert!(!history.is_empty());
    for item in history.iter().chain(phrase_items(&rig).iter()) {
        assert_eq!(
            item.plan.interrupt_points_ms,
            vec![item.plan.duration_ms],
            "a phrase declares an interrupt point at its end"
        );
    }
    assert_eq!(
        rig.tts_texts.lock().unwrap().as_slice(),
        [PHRASE_A, PHRASE_B, PHRASE_C],
        "TTS got whole phrases"
    );

    let metrics = rig.app.phrase_metrics();
    assert_eq!(metrics.responses, 1);
    assert_eq!(metrics.completed, 1);
    assert_eq!(metrics.phrases_committed, 3);
    assert_eq!(metrics.underruns, 0);
    // The event is observed once, at its first phrase.
    assert_eq!(rig.app.telemetry().events().len(), 1);
    assert!(
        rig.app.telemetry().events()[0]
            .event_to_first_audio_ms
            .is_some()
    );
    // The audit records the commit order.
    let committed: Vec<String> = rig
        .app
        .security()
        .audit()
        .iter()
        .filter(|record| record.decision == "phrase_committed")
        .map(|record| record.detail.clone())
        .collect();
    assert_eq!(committed.len(), 3);
    assert!(committed[0].contains("seq=0") && committed[2].contains("seq=2 final=true"));
}

#[test]
fn phrase_one_plays_while_phrase_two_is_still_synthesizing() {
    let mut tts = PhraseTts::new();
    tts.block_on_call = Some(1);
    let mut rig = rig_with(phrased(), &three_phrase_reply(), tts);
    submit(&mut rig, 30, 0);

    wait_until(&mut rig, 0, |rig| audio_count(rig) == 1);
    wait_until(&mut rig, 0, |rig| rig.tts_calls.load(Ordering::SeqCst) == 2);
    // Phrase 2's synthesis is in progress (blocked); phrase 1 already plays
    // and nothing partial of phrase 2 reached the scheduler.
    std::thread::sleep(Duration::from_millis(50));
    rig.app.tick(0);
    assert_eq!(audio_count(&rig), 1);
    assert_eq!(phrase_items(&rig).len(), 1, "no partial phrase 2 anywhere");
    assert!(rig.app.recent_phrase_responses().is_empty());

    rig.release.store(true, Ordering::SeqCst);
    wait_until(&mut rig, 0, |rig| phrase_items(rig).len() == 2);
}

#[test]
fn a_synthesis_failure_in_phrase_two_ends_the_reply_after_phrase_one() {
    let mut tts = PhraseTts::new();
    tts.fail_on_call = Some(1);
    let mut rig = rig_with(phrased(), &three_phrase_reply(), tts);
    submit(&mut rig, 30, 0);

    wait_until(&mut rig, 0, |rig| {
        !rig.app.recent_phrase_responses().is_empty()
    });
    let record = the_record(&rig);
    assert_eq!(record.commit_horizon(), 1);
    assert_eq!(
        record.end,
        PhraseResponseEnd::Partial {
            cause: PhraseCause::SynthesisFailed(FallbackReason::Unavailable)
        },
        "a partial reply is never recorded as complete"
    );
    assert_eq!(audio_count(&rig), 1, "phrase 1 was spoken once");
    assert_eq!(
        rig.app.performer().scheduler().items().len(),
        1,
        "no fallback reaction and nothing else was scheduled"
    );
    assert_eq!(
        rig.tts_calls.load(Ordering::SeqCst),
        2,
        "one failed attempt, no retry"
    );
    let metrics = rig.app.phrase_metrics();
    assert_eq!(
        (metrics.responses, metrics.partial, metrics.completed),
        (1, 1, 0)
    );
    assert_eq!(metrics.synthesis_failed_after_commit, 1);
}

#[test]
fn a_failure_in_phrase_one_is_the_buffered_fallback_and_nothing_is_spoken() {
    let mut tts = PhraseTts::new();
    tts.fail_on_call = Some(0);
    let mut rig = rig_with(phrased(), &three_phrase_reply(), tts);
    submit(&mut rig, 30, 0);
    wait_until(&mut rig, 0, |rig| {
        rig.app
            .generation_execution_snapshot()
            .is_some_and(|snapshot| snapshot.is_quiescent())
            && rig.app.telemetry().events().len() == 1
    });
    assert_eq!(audio_count(&rig), 0);
    assert!(rig.app.recent_phrase_responses().is_empty());
    assert_eq!(rig.app.telemetry().events().len(), 1, "observed once");
}

#[test]
fn an_operator_stop_while_phrase_two_is_synthesizing_prevents_every_later_commit() {
    let mut tts = PhraseTts::new();
    tts.block_on_call = Some(1);
    let mut rig = rig_with(phrased(), &three_phrase_reply(), tts);
    submit(&mut rig, 30, 0);
    wait_until(&mut rig, 0, |rig| {
        audio_count(rig) == 1 && rig.tts_calls.load(Ordering::SeqCst) == 2
    });

    rig.app
        .handle_control(&stop_command(), 100)
        .expect("operator stop");
    assert!(rig.app.performer().scheduler().items().is_empty());

    // Phrase 2's synthesis completes after the stop: it must go nowhere.
    rig.release.store(true, Ordering::SeqCst);
    wait_until(&mut rig, 100, |rig| {
        !rig.app.recent_phrase_responses().is_empty()
    });
    let record = the_record(&rig);
    assert_eq!(record.commit_horizon(), 1);
    assert_eq!(
        record.end,
        PhraseResponseEnd::Partial {
            cause: PhraseCause::OperatorStop
        }
    );
    assert_eq!(audio_count(&rig), 1, "no audio after the stop");
    assert!(rig.app.performer().scheduler().items().is_empty());
    assert_eq!(
        rig.tts_calls.load(Ordering::SeqCst),
        2,
        "phrase 3 never started"
    );
}

#[test]
fn a_barge_in_style_cancellation_between_phrases_stops_the_reply() {
    let mut rig = rig_with(phrased(), &three_phrase_reply(), PhraseTts::new());
    submit(&mut rig, 30, 0);
    wait_until(&mut rig, 0, |rig| phrase_items(rig).len() == 2);

    // The user starts talking: queued phrases are cancelled by the scheduler
    // and the reply itself is cancelled so no later phrase is committed.
    let stop = rig
        .app
        .performer_mut()
        .scheduler_mut()
        .stop_speaking_for(aivtuber_scheduler::Priority::Conversation, 10);
    assert!(!stop.is_empty());
    assert_eq!(rig.app.cancel_phrased_replies(PhraseCause::BargeIn), 1);
    wait_until(&mut rig, 10, |rig| {
        !rig.app.recent_phrase_responses().is_empty()
    });
    let record = the_record(&rig);
    assert_eq!(
        record.end,
        PhraseResponseEnd::Partial {
            cause: PhraseCause::BargeIn
        }
    );
    assert!(record.commit_horizon() <= 2);
    assert!(rig.app.performer().scheduler().items().is_empty());
    assert_eq!(
        rig.tts_calls.load(Ordering::SeqCst),
        2,
        "phrase 3 never started"
    );
}

/// Collects real units from the phrased pipeline so the commit boundary can be
/// driven with hand-made worker decisions.
#[derive(Default)]
struct VecDelivery {
    units: Vec<PhraseUnit>,
}

impl PhraseDelivery for VecDelivery {
    fn wait_for_slot(&mut self, _seq: u32, _c: &GenerationCancellationToken) -> SlotWait {
        SlotWait::Ready
    }

    fn deliver(&mut self, unit: PhraseUnit, _trace: &GenerationTrace) -> Delivery {
        self.units.push(unit);
        Delivery::Accepted
    }
}

fn make_units(event: &EventEnvelope) -> (Vec<PhraseUnit>, aivtuber_generative::PhraseSummary) {
    let compiler = PerformanceCompiler::new(PerformanceCompilerConfig {
        compiler_version: "0.1.0".to_owned(),
        avatar_profile: Some("example-live2d-v1".to_owned()),
        motion_library: Some("starter-v1".to_owned()),
        expression_preset: Some("speaking.neutral".to_owned()),
        expression_intensity: 0.4,
    })
    .expect("compiler");
    let pipeline = GenerativePipeline::new(
        Arc::new(MockThinking {
            reply: three_phrase_reply(),
            failure: None,
        }),
        Arc::new(PhraseTts::new()),
        compiler,
    );
    let mut router = FixedGenerateRoute {
        reply_context: ReflexContext::default(),
        fallback_variant_group: None,
    };
    let PlaybackRoute::Generate(route) = router.route(event).expect("route") else {
        panic!("generate route");
    };
    let request = GenerationRequest {
        source_event: event.clone(),
        thinking: route.thinking,
        routing_reason: route.routing_reason,
        intent: route.intent,
        style: None,
        fallback_variant_group: None,
        seed: 7,
    };
    let mut delivery = VecDelivery::default();
    let result = pipeline
        .run_phrased(
            &request,
            &GenerationCancellationToken::default(),
            FallbackDirective::NonVerbalReaction {
                reaction_family: "reaction.neutral".to_owned(),
            },
            None,
            &aivtuber_generative::PhraseOptions {
                response_id: 1,
                config: PhraseConfig::default(),
            },
            |text| GeneratedTextGateDecision::Publish(text.to_owned()),
            &mut delivery,
        )
        .expect("pipeline");
    let GenerationDisposition::Phrased { summary } = result.disposition else {
        panic!("phrased");
    };
    assert_eq!(summary.end, PhraseEnd::Completed);
    (delivery.units, summary)
}

fn allow(text: &str) -> Option<PublicOutput> {
    Some(PublicOutput {
        verdict: OutputVerdict::Allow,
        text: Some(text.to_owned()),
        reason: "clean",
    })
}

fn register_run(rig: &mut Rig, event: &EventEnvelope) -> GenerationCancellationToken {
    let token = GenerationCancellationToken::default();
    let generative = rig.app.generative.as_mut().expect("generative runtime");
    generative.cancellation.activate(event, token.clone());
    generative.active_generation_id = Some(1);
    let context = GenerationCompletionContext {
        budget_reservation: None,
        routing_latency_us: 0,
        decision: None,
        template_fallback: None,
        interaction_deadline: InteractionDeadline::new(
            aivtuber_domain::InteractionDeadlineClass::Conversation,
            0,
            30_000,
        )
        .expect("deadline"),
        remaining_at_route_ms: 30_000,
        remaining_at_generation_ms: 30_000,
        correlation_id: event.correlation_id.clone(),
    };
    generative.phrase.runs.push(phrase_commit::PhraseRun::new(
        1,
        event.clone(),
        7,
        token.clone(),
        Arc::new(AtomicU32::new(0)),
        Instant::now() + Duration::from_secs(60),
        context,
    ));
    token
}

fn message(unit: PhraseUnit, decision: Option<PublicOutput>) -> phrase_commit::PhraseMessage {
    phrase_commit::PhraseMessage {
        generation_id: 1,
        unit,
        output_decision: decision,
        trace: GenerationTrace::default(),
        queue_wait_us: 0,
        provider_latency_us: 0,
        generation_latency_us: 0,
        delivered_at: Instant::now(),
    }
}

fn send(rig: &Rig, message: phrase_commit::PhraseMessage) {
    rig.app
        .generative
        .as_ref()
        .expect("generative runtime")
        .phrase_tx
        .try_send(message)
        .ok()
        .expect("mailbox has room");
}

fn finish(rig: &mut Rig, summary: &aivtuber_generative::PhraseSummary) {
    let run = rig
        .app
        .generative
        .as_mut()
        .and_then(|generative| generative.phrase.take_run(1))
        .expect("run");
    rig.app.finish_phrase_run(run, Some(summary));
}

#[test]
fn a_gate_refusal_of_phrase_two_after_phrase_one_played_ends_the_reply_at_the_horizon() {
    let event = chat_event(30);
    let (mut units, summary) = make_units(&event);
    let mut rig = rig_with(phrased(), "unused", PhraseTts::new());
    let token = register_run(&mut rig, &event);

    let third = units.pop().unwrap();
    let second = units.pop().unwrap();
    let first = units.pop().unwrap();
    let first_text = first.display_text().to_owned();
    send(&rig, message(first, allow(&first_text)));
    // The decision the worker saw is not for phrase two's text: the commit
    // boundary must refuse it however the mismatch came about.
    send(&rig, message(second, allow("some other text")));
    rig.app.tick(0);

    assert!(token.is_cancelled(), "the worker is told to stop");
    assert_eq!(
        audio_count(&rig),
        1,
        "phrase one was committed and dispatched"
    );
    assert_eq!(
        phrase_items(&rig).len(),
        1,
        "phrase two never reached the scheduler"
    );

    // Anything the worker still delivers is dropped, never committed.
    send(&rig, message(third, allow(PHRASE_C)));
    rig.app.tick(0);
    assert_eq!(phrase_items(&rig).len(), 1);
    assert_eq!(rig.app.phrase_metrics().late_phrases_dropped, 1);

    finish(&mut rig, &summary);
    let record = the_record(&rig);
    assert_eq!(record.commit_horizon(), 1);
    assert_eq!(
        record.end,
        PhraseResponseEnd::Partial {
            cause: PhraseCause::GateRejected
        }
    );
    let metrics = rig.app.phrase_metrics();
    assert_eq!(metrics.gate_rejected_after_commit, 1);
    assert_eq!(metrics.gate_rejected_before_first_commit, 0);
}

#[test]
fn a_gate_refusal_of_phrase_one_speaks_nothing_and_is_observed_as_a_policy_override() {
    let event = chat_event(30);
    let (mut units, _summary) = make_units(&event);
    let mut rig = rig_with(phrased(), "unused", PhraseTts::new());
    let token = register_run(&mut rig, &event);

    let first = units.remove(0);
    send(&rig, message(first, None));
    rig.app.tick(0);

    assert!(token.is_cancelled());
    assert_eq!(audio_count(&rig), 0);
    assert!(rig.app.performer().scheduler().items().is_empty());
    assert_eq!(
        rig.app.phrase_metrics().gate_rejected_before_first_commit,
        1
    );
    let observations = rig.app.telemetry().events();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].route, RouteClass::NonVerbalFallback);
}

#[test]
fn the_phrase_record_is_deterministic_across_identical_runs() {
    let run_once = || {
        let mut rig = rig_with(phrased(), &three_phrase_reply(), PhraseTts::new());
        submit(&mut rig, 30, 0);
        wait_until(&mut rig, 0, |rig| phrase_items(rig).len() == 2);
        let a_ms = duration_of(PHRASE_A);
        let b_ms = duration_of(PHRASE_B);
        rig.app.tick(a_ms);
        wait_until(&mut rig, a_ms, |rig| {
            rig.tts_calls.load(Ordering::SeqCst) == 3 && phrase_items(rig).len() == 2
        });
        rig.app.tick(a_ms + b_ms);
        wait_until(&mut rig, a_ms + b_ms, |rig| {
            !rig.app.recent_phrase_responses().is_empty()
        });
        let mut record = the_record(&rig);
        for commit in &mut record.commits {
            commit.synthesis_ms = 0; // wall-clock measurement, not an input
        }
        record
    };
    assert_eq!(run_once(), run_once());
}

#[test]
fn first_audio_comes_far_sooner_than_in_buffered_mode_and_both_modes_stay_available() {
    let slow = || {
        let mut tts = PhraseTts::new();
        tts.base_delay = Duration::from_millis(20);
        tts.delay_per_char = Duration::from_millis(4);
        tts
    };
    let time_to_first_audio = |mode: PhraseMode| {
        let mut rig = rig_with(mode, &three_phrase_reply(), slow());
        let started = Instant::now();
        submit(&mut rig, 30, 0);
        wait_until(&mut rig, 0, |rig| audio_count(rig) >= 1);
        let first_audio = started.elapsed();
        (first_audio, rig)
    };
    let (buffered_first, buffered_rig) = time_to_first_audio(PhraseMode::Buffered);
    let (phrased_first, _phrased_rig) = time_to_first_audio(phrased());
    assert!(
        phrased_first * 2 < buffered_first,
        "first audio: phrased {phrased_first:?} vs buffered {buffered_first:?}"
    );
    // Buffered mode is unchanged: one asset for the whole reply.
    assert_eq!(buffered_rig.app.performer().scheduler().items().len(), 1);
    assert!(buffered_rig.app.recent_phrase_responses().is_empty());
}
