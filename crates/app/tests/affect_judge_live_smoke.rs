//! Manual live smoke of the opt-in affect judge (#426). Never runs in CI.
//!
//! Drives the real phrase pipeline with a real local TTS sidecar and the real
//! System One endpoint, once with the rules classifier only and once with the
//! judge in parallel, on the same synthetic single-phrase replies, and reports
//! judge-used vs fallback and the added time-to-first-audio.
//!
//! Run (PowerShell; the key is read from the environment, never printed):
//!   $env:AIVTUBER_SMOKE_SIDECAR = "...\local-tts-sidecar.exe"
//!   $env:AIVTUBER_SMOKE_MODEL_DIR = "...\supertonic\<model dir>"
//!   $env:AIVTUBER_SMOKE_SCRATCH = "I:\temp\affectjudge-live"
//!   $env:OPENROUTER_API_KEY = "..."
//!   cargo test -p aivtuber-app --test affect_judge_live_smoke -- --ignored --nocapture
//!
//! Only synthetic text is sent. Optional: `AIVTUBER_SMOKE_TRIALS` (default 36),
//! `AIVTUBER_SMOKE_JEV_ENDPOINT`, `AIVTUBER_SMOKE_JEV_MODEL`.

use aivtuber_adapters::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig};
use aivtuber_domain::{
    BackendIdentity, EngineFuture, EventEnvelope, GeneratedReply, PrivacyClass, ReflexContext,
    RetrievalSnapshot, ThinkingEngine, ThinkingRequest,
};
use aivtuber_generative::{
    AffectConfig, AffectJudgeRunner, AffectSource, Delivery, FallbackDirective,
    GeneratedTextGateDecision, GenerationCancellationToken, GenerationDisposition,
    GenerationRequest, GenerationRoutingReason, GenerativePipeline, PerformanceCompiler,
    PerformanceCompilerConfig, PhraseConfig, PhraseDelivery, PhraseOptions, PhraseUnit, SlotWait,
};
use aivtuber_reflex::{JevAdapter, JevAdapterConfig, JevAffectJudge, JevApiKey};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LINES: [&str; 11] = [
    "今日は配信に来てくれて本当にありがとう！とても嬉しいよ。",
    "えっ、それ本当に？まさかそんなことがあるなんてびっくりした！",
    "大丈夫だよ、ゆっくり深呼吸してね。ここでのんびりしていこう。",
    "昨日はずっと一人で、ちょっと寂しかったんだ。",
    "もう、何度言ったらわかるの！いい加減にしてよ！",
    "このあとはゲームを遊びながらお話しします。",
    "わあ、すごい！こんなにたくさんの人が来てくれるなんて夢みたい！",
    "ねえ、聞いて。今日はなんだかとても静かで、気持ちが落ち着くね。",
    "ううん、やっぱり会えなくなるのはさみしいな。また来てね。",
    "次の曲は、みんながリクエストしてくれたものです。",
    "ありがとう、でもちょっと疲れちゃったかな。",
];

struct FixedThinking(String);

impl ThinkingEngine for FixedThinking {
    fn generate<'a>(&'a self, _: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply> {
        let text = self.0.clone();
        Box::pin(async move { Ok(GeneratedReply { text, usage: None }) })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "smoke-thinking".to_owned(),
            model_alias: None,
            model_version: None,
        }
    }
}

#[derive(Default)]
struct Delivered {
    first_at: Option<Instant>,
    units: usize,
}

impl PhraseDelivery for Delivered {
    fn wait_for_slot(&mut self, _: u32, _: &GenerationCancellationToken) -> SlotWait {
        SlotWait::Ready
    }

    fn deliver(&mut self, _unit: PhraseUnit, _: &aivtuber_generative::GenerationTrace) -> Delivery {
        self.first_at.get_or_insert_with(Instant::now);
        self.units += 1;
        Delivery::Accepted
    }
}

struct Sidecar(Child);

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

fn request() -> GenerationRequest {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/events/chat-message.json"
    ))
    .expect("chat fixture");
    let mut source_event: EventEnvelope = serde_json::from_str(&raw).expect("fixture");
    source_event.event_id = "evt-smoke-1".to_owned();
    let thinking = ThinkingRequest::from_event(
        &source_event,
        "synthetic",
        PrivacyClass::Pseudonymous,
        ReflexContext::default(),
        RetrievalSnapshot::default(),
        Vec::new(),
    );
    GenerationRequest {
        source_event,
        thinking,
        routing_reason: GenerationRoutingReason::NoCachedMatch,
        intent: "dynamic.reply".to_owned(),
        style: None,
        fallback_variant_group: Some("reaction.surprise".to_owned()),
        seed: 7,
    }
}

/// A distinct fixed-length (3 hiragana) token per spoken text, so the
/// sidecar's audio cache can never turn a repeated line into a free one while
/// both arms of a pair still speak the same number of characters.
fn token(n: usize) -> String {
    const KANA: &str =
        "あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわ";
    let kana: Vec<char> = KANA.chars().collect();
    let base = kana.len();
    [n / (base * base) % base, n / base % base, n % base]
        .iter()
        .map(|i| kana[*i])
        .collect()
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let rank = ((sorted.len() as f64) * q).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[test]
#[ignore = "manual live smoke: real TTS sidecar and real System One endpoint"]
fn judge_in_parallel_with_local_tts() {
    let scratch = PathBuf::from(env("AIVTUBER_SMOKE_SCRATCH"));
    let audio_root = scratch.join("audio");
    // The sidecar's audio cache is content addressed on disk; start empty.
    let _ = std::fs::remove_dir_all(&audio_root);
    std::fs::create_dir_all(&audio_root).expect("audio root");
    let port = 18_770_u16;
    let listen = format!("127.0.0.1:{port}");
    let child = Command::new(env("AIVTUBER_SMOKE_SIDECAR"))
        .args([
            "--model-dir",
            &env("AIVTUBER_SMOKE_MODEL_DIR"),
            "--audio-root",
            audio_root.to_str().expect("utf-8 path"),
            "--listen",
            &listen,
            "--path",
            "/v1/synthesize",
            "--speaker-id",
            "0",
            "--threads",
            "2",
        ])
        // The sidecar exits when its stdin closes (parent liveness); a piped
        // handle kept inside `Child` stays open for the whole test.
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(scratch.join("sidecar.err")).expect("sidecar log"),
        ))
        .spawn()
        .expect("spawn sidecar");
    eprintln!("sidecar pid={}", child.id());
    let mut sidecar = Sidecar(child);
    let address: SocketAddr = listen.parse().unwrap();
    let ready_by = Instant::now() + Duration::from_secs(120);
    while TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err() {
        let status = sidecar.0.try_wait().expect("sidecar status");
        assert!(
            status.is_none(),
            "sidecar exited early: {status:?} (see sidecar.err in the scratch directory)"
        );
        assert!(Instant::now() < ready_by, "sidecar did not become ready");
        std::thread::sleep(Duration::from_millis(250));
    }

    let tts = Arc::new(
        NormalizedHttpTtsAdapter::new(
            NormalizedHttpTtsConfig {
                endpoint: format!("http://{listen}/v1/synthesize"),
                backend_name: "local-tts-sidecar".to_owned(),
                model_alias: Some("supertonic-3-int8".to_owned()),
                model_version: None,
                voice_model: Some("supertonic".to_owned()),
                viseme_mapping: None,
                timeout: Duration::from_secs(30),
            },
            None,
        )
        .expect("tts adapter"),
    );

    {
        let t = Instant::now();
        let probe = tts.synthesize_sync(&aivtuber_domain::SpeechRequest {
            text: "接続の確認です。".to_owned(),
            style: None,
        });
        eprintln!(
            "tts probe {:?} ms={}",
            probe.as_ref().err().map(|e| e.to_string()),
            t.elapsed().as_millis()
        );
    }
    let endpoint = std::env::var("AIVTUBER_SMOKE_JEV_ENDPOINT")
        .unwrap_or_else(|_| "https://openrouter.ai/api/alpha/decisions".to_owned());
    let model = std::env::var("AIVTUBER_SMOKE_JEV_MODEL")
        .unwrap_or_else(|_| "perplexity/pplx-decider-flash-v1.3:free".to_owned());
    let adapter = JevAdapter::new(
        JevAdapterConfig {
            endpoint,
            model_alias: model,
            ..JevAdapterConfig::default()
        },
        JevApiKey::new(env("OPENROUTER_API_KEY")).expect("key"),
    )
    .expect("jev adapter");
    let runner = AffectJudgeRunner::new(
        Arc::new(aivtuber_app::SystemOneAffectJudge::new(
            JevAffectJudge::new(adapter),
        )),
        aivtuber_generative::DEFAULT_JUDGE_HARD_CAP,
    )
    .expect("runner");

    let compiler = |judge: bool| {
        let base = PerformanceCompiler::new(PerformanceCompilerConfig {
            compiler_version: "0.1.0".to_owned(),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
            expression_preset: Some("speaking.neutral".to_owned()),
            expression_intensity: 0.4,
        })
        .expect("compiler")
        .with_affect(AffectConfig::starter_pack());
        if judge {
            base.with_affect_judge(runner.clone())
        } else {
            base
        }
    };
    let counter = std::cell::Cell::new(0_usize);
    let trial = |line: &str, judge: bool| {
        // A fresh token in front of every sentence: every phrase of every arm
        // is a distinct text, so no phrase is served from the audio cache.
        let mut unique = String::new();
        let mut at_start = true;
        for c in line.chars() {
            if at_start {
                counter.set(counter.get() + 1);
                unique.push_str(&token(counter.get()));
            }
            unique.push(c);
            at_start = matches!(c, '。' | '！' | '？' | '!' | '?' | '.');
        }
        let line = unique;
        let pipeline =
            GenerativePipeline::new(Arc::new(FixedThinking(line)), tts.clone(), compiler(judge));
        let mut delivery = Delivered::default();
        let started = Instant::now();
        let result = pipeline
            .run_phrased(
                &request(),
                &GenerationCancellationToken::default(),
                FallbackDirective::NonVerbalReaction {
                    reaction_family: "reaction.surprise".to_owned(),
                },
                None,
                &PhraseOptions {
                    response_id: 1,
                    config: PhraseConfig::default(),
                },
                |text: &str| GeneratedTextGateDecision::Publish(text.to_owned()),
                &mut delivery,
            )
            .expect("pipeline");
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Phrased { .. }
        ));
        let first = delivery.first_at.expect("a phrase was delivered") - started;
        eprintln!("trial judge={judge} first_audio_ms={}", first.as_millis());
        (first.as_secs_f64() * 1_000.0, result.trace)
    };

    // Warm the sidecar (model load, first-call cost) before measuring.
    for line in LINES.iter().take(2) {
        trial(line, false);
    }
    let trials: usize = std::env::var("AIVTUBER_SMOKE_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(36);
    let (mut rules_ms, mut judge_ms, mut diffs) = (Vec::new(), Vec::new(), Vec::new());
    let mut control_diffs: Vec<f64> = Vec::new();
    let (mut judged, mut fallbacks) = (0_usize, std::collections::BTreeMap::new());
    // (judged, total) for the first phrase of a reply and for later phrases.
    let (mut first_phrase, mut later_phrases) = ((0_usize, 0_usize), (0_usize, 0_usize));
    for i in 0..trials {
        let line = LINES[i % LINES.len()];
        // Three arms on the same line: rules, judge, and a second rules arm as
        // the control that measures the run-to-run noise of the TTS sidecar.
        // The order rotates so drift and caching do not favour one arm.
        let mut arms = [(0_usize, None), (1, None), (2, None)];
        for slot in 0..3 {
            let arm = (slot + i) % 3;
            arms[arm].1 = Some(trial(line, arm == 1));
        }
        let [(_, a), (_, j), (_, b)] = arms;
        let ((r, _), (j, trace), (control, _)) = (a.unwrap(), j.unwrap(), b.unwrap());
        rules_ms.push(r);
        judge_ms.push(j);
        diffs.push(j - r);
        control_diffs.push(control - r);
        for decision in &trace.affect_decisions {
            let bucket = if decision.seq == 0 {
                &mut first_phrase
            } else {
                &mut later_phrases
            };
            bucket.1 += 1;
            bucket.0 += usize::from(decision.source == AffectSource::Judge);
            match decision.source {
                AffectSource::Judge => judged += 1,
                AffectSource::RulesFallback(reason) => {
                    *fallbacks.entry(reason.as_str()).or_insert(0_usize) += 1;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    for series in [&mut rules_ms, &mut judge_ms, &mut diffs, &mut control_diffs] {
        series.sort_by(f64::total_cmp);
    }
    let status = runner.stats().snapshot();
    eprintln!(
        "replies={trials} phrases={} judged={judged} fallbacks={fallbacks:?}",
        first_phrase.1 + later_phrases.1
    );
    eprintln!(
        "judged/total: first phrase {}/{}, later phrases {}/{}",
        first_phrase.0, first_phrase.1, later_phrases.0, later_phrases.1
    );
    eprintln!(
        "first_audio_ms rules p50={:.0} p95={:.0} | judge p50={:.0} p95={:.0}",
        percentile(&rules_ms, 0.5),
        percentile(&rules_ms, 0.95),
        percentile(&judge_ms, 0.5),
        percentile(&judge_ms, 0.95)
    );
    eprintln!(
        "added_first_audio_ms (paired judge-rules) p50={:.1} p95={:.1} max={:.1}",
        percentile(&diffs, 0.5),
        percentile(&diffs, 0.95),
        diffs.last().copied().unwrap_or(0.0)
    );
    eprintln!(
        "control (rules2-rules1, the noise floor) p50={:.1} p95={:.1} max={:.1}",
        percentile(&control_diffs, 0.5),
        percentile(&control_diffs, 0.95),
        control_diffs.last().copied().unwrap_or(0.0)
    );
    eprintln!(
        "judge call latency ms p50={:?} p95={:?} samples={}",
        status.latency_p50_ms, status.latency_p95_ms, status.latency_samples
    );
    eprintln!("status={}", serde_json::to_string(&status).unwrap());
    // Let any worker finish so nothing is left running.
    let until = Instant::now() + Duration::from_secs(3);
    while runner.in_flight() > 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(runner.in_flight(), 0);
}
