//! `local` profile with provider token streaming (issue #83 S2).
//!
//! Thinking is the real OpenAI-compatible adapter talking to a scripted SSE
//! server on 127.0.0.1 (no model, no network beyond loopback); TTS is a stub
//! with a fixed latency; the clock the scheduler sees is real elapsed time so
//! the lookahead (phrase n + 1 waits for phrase n to start) behaves as live.
//!
//! The first-audio test doubles as the measurement recorded in the pull
//! request: `cargo test -p aivtuber-app --lib streaming_thinking -- --nocapture`.

use super::local_thinking_fallback_tests::{
    LocalRig, assert_visible_fallback, free_text_line, local_rig_with_engines_and_secrets,
    submit_free_text_line,
};
use super::*;
use aivtuber_adapters::{OpenAiResponsesAdapter, OpenAiResponsesConfig, StreamLimits};
use aivtuber_generative::{PhraseConfig, StreamPolicy};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;

const PHRASE_A: &str = "今日は配信に来てくれてありがとうございます。";
const PHRASE_B: &str = "このあとはゲームを遊びながらお話しします。";
const PHRASE_C: &str = "最後まで楽しんでいってくださいね。";

/// How the scripted server ends a stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    Completed,
    /// The connection drops with no completion marker.
    Cut,
}

#[derive(Clone)]
struct Reply {
    text: String,
    /// Delay before each 3-character delta (a token cadence).
    token_gap: Duration,
    ending: Ending,
}

fn frame(delta: &str) -> Vec<u8> {
    format!(
        "event: response.output_text.delta\ndata: {}\n\n",
        json!({"type": "response.output_text.delta", "delta": delta})
    )
    .into_bytes()
}

/// Serves `connections` requests. A request asking for `"stream":true` gets
/// the reply as SSE at the token cadence; any other request gets the same
/// reply as one JSON document after the same total generation time, so the two
/// modes see an identical model.
fn serve(reply: Reply, connections: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let reply = reply.clone();
            std::thread::spawn(move || {
                let mut seen = Vec::new();
                let mut buffer = [0_u8; 4096];
                let (start, length) = loop {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buffer[..n]);
                    if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&seen[..at]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (at + 4, length);
                    }
                };
                while seen.len() < start + length {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buffer[..n]);
                }
                let wants_stream =
                    String::from_utf8_lossy(&seen[start..]).contains("\"stream\":true");
                let chars: Vec<char> = reply.text.chars().collect();
                let deltas: Vec<String> = chars
                    .chunks(3)
                    .map(|part| part.iter().collect::<String>())
                    .collect();
                if wants_stream {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                    );
                    for delta in &deltas {
                        std::thread::sleep(reply.token_gap);
                        if stream.write_all(&frame(delta)).is_err() || stream.flush().is_err() {
                            return;
                        }
                    }
                    if reply.ending == Ending::Completed {
                        let _ = stream.write_all(b"data: {\"type\":\"response.completed\"}\n\n");
                    }
                } else {
                    std::thread::sleep(reply.token_gap * deltas.len() as u32);
                    let body = serde_json::to_vec(&json!({"output": [{
                    "type": "message", "role": "assistant", "content": [
                        {"type": "output_text", "text": reply.text, "annotations": []}
                    ]}]}))
                    .unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
        }
    });
    endpoint
}

/// TTS stub with a fixed latency: 40 ms plus 4 ms per character, and 40 ms of
/// audio per character.
struct DelayTts {
    texts: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

impl TtsEngine for DelayTts {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.texts.lock().unwrap().push(request.text.clone());
        let chars = request.text.chars().count() as u64;
        std::thread::sleep(Duration::from_millis(40 + 4 * chars));
        Box::pin(async move {
            Ok(SpeechArtifact {
                audio_ref: format!("audio://generated/stream-{call}.wav"),
                duration_ms: 40 * chars,
                viseme_ref: None,
            })
        })
    }

    fn identity(&self) -> TtsBackendIdentity {
        TtsBackendIdentity {
            backend: BackendIdentity {
                name: "delay-tts".to_owned(),
                model_alias: Some("delay".to_owned()),
                model_version: Some("1".to_owned()),
            },
            voice_model: Some("example-voice-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
        }
    }
}

struct Run {
    rig: LocalRig,
    texts: Arc<Mutex<Vec<String>>>,
    started: Instant,
}

fn rig(endpoint: String, streaming: bool) -> Run {
    rig_with_secrets(endpoint, streaming, &[])
}

fn rig_with_secrets(endpoint: String, streaming: bool, secrets: &[&str]) -> Run {
    let mut thinking = OpenAiResponsesAdapter::with_authentication(
        OpenAiResponsesConfig {
            endpoint,
            model_alias: "local-test-model".to_owned(),
            timeout: Duration::from_secs(10),
            ..OpenAiResponsesConfig::default()
        },
        aivtuber_adapters::ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .expect("loopback adapter")
    .with_backend_name("llama-server");
    let mut config = PhraseConfig::default();
    if streaming {
        thinking = thinking
            .with_streaming(StreamLimits::default())
            .expect("stream limits");
        config.stream = Some(StreamPolicy::default());
    }
    let texts = Arc::new(Mutex::new(Vec::new()));
    let tts = DelayTts {
        texts: Arc::clone(&texts),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let rig =
        local_rig_with_engines_and_secrets(thinking, tts, PhraseMode::Phrased(config), secrets);
    Run {
        rig,
        texts,
        started: Instant::now(),
    }
}

/// Tick on real elapsed time until `done` or the bound.
fn drive(run: &mut Run, limit: Duration, mut done: impl FnMut(&LocalRig) -> bool) -> bool {
    let bound = Instant::now() + limit;
    while Instant::now() < bound {
        run.rig.app.tick(run.started.elapsed().as_millis() as u64);
        if done(&run.rig) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

fn full_reply() -> String {
    format!("{PHRASE_A}{PHRASE_B}{PHRASE_C}")
}

fn first_audio(streaming: bool, token_gap: Duration) -> Duration {
    let endpoint = serve(
        Reply {
            text: full_reply(),
            token_gap,
            ending: Ending::Completed,
        },
        1,
    );
    let mut run = rig(endpoint, streaming);
    run.started = Instant::now();
    submit_free_text_line(&mut run.rig, 70);
    assert!(
        drive(&mut run, Duration::from_secs(15), |rig| !rig
            .audio
            .lock()
            .unwrap()
            .is_empty()),
        "no audio reached the output"
    );
    run.started.elapsed()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

#[test]
fn streamed_phrase_mode_reaches_first_audio_sooner_than_buffered_phrase_mode() {
    const RUNS: usize = 5;
    // 25 ms per 3-character delta is roughly 40 tokens/s of Japanese text.
    let gap = Duration::from_millis(25);
    let buffered: Vec<Duration> = (0..RUNS).map(|_| first_audio(false, gap)).collect();
    let streamed: Vec<Duration> = (0..RUNS).map(|_| first_audio(true, gap)).collect();
    let (buffered_p50, streamed_p50) = (median(buffered.clone()), median(streamed.clone()));
    eprintln!(
        "MEASUREMENT time-to-first-audio, stub SSE server ({} ms per 3-char delta, {}-char reply, \
         stub TTS 40 ms + 4 ms/char), {RUNS} runs each\n  buffered phrase mode: p50 {} ms {:?}\n  \
         streamed phrase mode: p50 {} ms {:?}",
        gap.as_millis(),
        full_reply().chars().count(),
        buffered_p50.as_millis(),
        buffered.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        streamed_p50.as_millis(),
        streamed.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
    );
    assert!(
        streamed_p50 + Duration::from_millis(150) < buffered_p50,
        "streamed {streamed_p50:?} vs buffered {buffered_p50:?}"
    );
}

#[test]
fn a_streamed_reply_is_committed_phrase_by_phrase_and_completes() {
    let endpoint = serve(
        Reply {
            text: full_reply(),
            token_gap: Duration::from_millis(10),
            ending: Ending::Completed,
        },
        1,
    );
    let mut run = rig(endpoint, true);
    run.started = Instant::now();
    submit_free_text_line(&mut run.rig, 71);
    assert!(
        drive(&mut run, Duration::from_secs(20), |rig| !rig
            .app
            .recent_phrase_responses()
            .is_empty()),
        "the reply never finished"
    );
    let record = run.rig.app.recent_phrase_responses().remove(0);
    assert_eq!(record.commits.len(), 3);
    assert_eq!(record.end, PhraseResponseEnd::Completed);
    assert!(record.commits[2].final_phrase && !record.commits[1].final_phrase);
    assert_eq!(
        run.texts.lock().unwrap().as_slice(),
        [PHRASE_A, PHRASE_B, PHRASE_C],
        "TTS got each complete phrase, in order"
    );
    assert_eq!(run.rig.app.recent_phrase_responses().len(), 1);
}

#[test]
fn a_stream_cut_mid_reply_keeps_the_spoken_phrases_and_records_a_typed_partial_end() {
    let endpoint = serve(
        Reply {
            text: format!("{PHRASE_A}{PHRASE_B}このあとは"),
            token_gap: Duration::from_millis(10),
            ending: Ending::Cut,
        },
        1,
    );
    let mut run = rig(endpoint, true);
    run.started = Instant::now();
    submit_free_text_line(&mut run.rig, 72);
    assert!(
        drive(&mut run, Duration::from_secs(20), |rig| !rig
            .app
            .recent_phrase_responses()
            .is_empty()),
        "the reply never ended"
    );
    let record = run.rig.app.recent_phrase_responses().remove(0);
    assert!(!record.commits.is_empty(), "phrase 0 stands");
    assert_eq!(
        record.end,
        PhraseResponseEnd::Partial {
            cause: PhraseCause::StreamFailed(FallbackReason::Unavailable)
        }
    );
    for spoken in run.texts.lock().unwrap().iter() {
        assert!(
            spoken == PHRASE_A || spoken == PHRASE_B,
            "an incomplete tail must never be spoken: {spoken}"
        );
    }
    // Something was spoken, so no fallback reaction replaces it.
    assert_eq!(run.rig.app.generation_fallback_counts().by_reason.len(), 0);
}

#[test]
fn a_stream_that_fails_before_any_phrase_plays_the_visible_fallback() {
    let endpoint = serve(
        Reply {
            text: "今日は".to_owned(),
            token_gap: Duration::from_millis(5),
            ending: Ending::Cut,
        },
        1,
    );
    let mut run = rig(endpoint, true);
    free_text_line(&mut run.rig, 73);
    assert_visible_fallback(&mut run.rig, "unavailable");
}

/// A secret that only exists across phrases A and B: neither phrase contains
/// it, so the per-phrase gate passes both, but the reply as a whole would be
/// redacted.
const CROSS_PHRASE_SECRET: &str = "ございます。このあと";

#[test]
fn a_secret_spanning_two_streamed_phrases_is_stopped_before_the_second_is_spoken() {
    assert!(!PHRASE_A.contains(CROSS_PHRASE_SECRET) && !PHRASE_B.contains(CROSS_PHRASE_SECRET));
    let reply = Reply {
        text: full_reply(),
        token_gap: Duration::from_millis(5),
        ending: Ending::Completed,
    };

    // Streaming: phrase A is committed and stands, B is refused, C is never
    // gated or spoken, and the partial end carries the gate-rejected cause.
    let mut run = rig_with_secrets(serve(reply.clone(), 1), true, &[CROSS_PHRASE_SECRET]);
    run.rig.app = run.rig.app.with_speech_lifecycle_recorder(64);
    run.started = Instant::now();
    submit_free_text_line(&mut run.rig, 73);
    assert!(
        drive(&mut run, Duration::from_secs(20), |rig| !rig
            .app
            .recent_phrase_responses()
            .is_empty()),
        "the reply never ended"
    );
    let record = run.rig.app.recent_phrase_responses().remove(0);
    assert_eq!(record.commits.len(), 1, "phrase 0 stands");
    assert_eq!(
        record.end,
        PhraseResponseEnd::Partial {
            cause: PhraseCause::GateRejected
        }
    );
    assert_eq!(
        run.texts.lock().unwrap().as_slice(),
        [PHRASE_A],
        "the offending phrase and the rest were never spoken"
    );
    // Lifecycle (#308/#347): the refused phrase was never prepared or
    // committed, so it has no events at all; the committed phrase's unit is
    // the only speech and nothing is cancelled (it stands).
    let events = run.rig.app.speech_lifecycle_records();
    assert!(!events.is_empty());
    assert!(
        events.iter().all(|event| event.speech.phrase == 0
            && !matches!(event.phase, aivtuber_domain::SpeechPhase::Cancelled { .. })),
        "{events:?}"
    );
    let metrics = run.rig.app.phrase_metrics();
    assert_eq!(metrics.stream_cumulative_gate_rejected, 1);
    assert_eq!(metrics.stream_whole_reply_gate_audits, 0);
    assert_eq!(
        run.rig.app.generation_fallback_counts().by_reason.len(),
        0,
        "something was spoken, so no fallback reaction replaces it"
    );

    // Buffered phrase mode: the whole reply is gated before phrase 0, and the
    // gate redacts the secret in place, so what is spoken never contains it.
    // (Streaming cannot rewrite text that is already spoken, so it refuses.)
    let mut buffered = rig_with_secrets(serve(reply, 1), false, &[CROSS_PHRASE_SECRET]);
    buffered.started = Instant::now();
    submit_free_text_line(&mut buffered.rig, 74);
    assert!(drive(&mut buffered, Duration::from_secs(20), |rig| !rig
        .app
        .recent_phrase_responses()
        .is_empty()));
    let spoken = buffered.texts.lock().unwrap().clone();
    assert!(!spoken.is_empty());
    assert!(
        spoken
            .iter()
            .all(|text| !text.contains(CROSS_PHRASE_SECRET))
    );
}

#[test]
fn a_clean_streamed_reply_gets_one_agreeing_whole_reply_audit() {
    let endpoint = serve(
        Reply {
            text: full_reply(),
            token_gap: Duration::from_millis(5),
            ending: Ending::Completed,
        },
        1,
    );
    let mut run = rig(endpoint, true);
    run.started = Instant::now();
    submit_free_text_line(&mut run.rig, 75);
    assert!(drive(&mut run, Duration::from_secs(20), |rig| !rig
        .app
        .recent_phrase_responses()
        .is_empty()));
    let metrics = run.rig.app.phrase_metrics();
    assert_eq!(metrics.stream_whole_reply_gate_audits, 1);
    assert_eq!(metrics.stream_whole_reply_gate_rejected_after_commit, 0);
    assert_eq!(metrics.stream_cumulative_gate_rejected, 0);
}
