//! `local` profile with the optional Irodori-TTS backend (issue #411).
//!
//! The whole TTS path is real except the server: the generative pipeline's
//! `NormalizedHttpTtsAdapter` posts the normalized contract to the in-process
//! bridge, which drives a fake Irodori-TTS-Server on loopback (no server, no
//! model, no GPU, no network beyond 127.0.0.1). Thinking is a scripted stub.

use super::local_thinking_fallback_tests::{
    LocalRig, assert_visible_fallback, both_modes, free_text_line, local_rig_with_engines,
    submit_free_text_line,
};
use super::*;
use aivtuber_adapters::speech_tts::irodori_fake::{FakeBehaviour, FakeIrodoriServer};
use aivtuber_adapters::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig};
use std::collections::HashMap;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-411-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Settings exactly as the operator's environment would produce them.
fn bridge_for(server_endpoint: &str, timeout_ms: &str, root: &Path) -> IrodoriBridge {
    let env: HashMap<&str, &str> = HashMap::from([
        ("AIVTUBER_TTS_BACKEND", "irodori"),
        ("AIVTUBER_IRODORI_ENDPOINT", server_endpoint),
        ("AIVTUBER_IRODORI_SEED", "4242"),
        ("AIVTUBER_IRODORI_TIMEOUT_MS", timeout_ms),
    ]);
    let settings = IrodoriSettings::resolve(
        &|name| env.get(name).map(|value| (*value).to_owned()),
        CompositionProfile::Local,
        true,
    )
    .expect("settings")
    .expect("irodori selected");
    IrodoriBridge::start(settings, &root.join("audio"), root).expect("bridge")
}

fn tts_adapter(bridge: &IrodoriBridge) -> NormalizedHttpTtsAdapter {
    NormalizedHttpTtsAdapter::new(
        NormalizedHttpTtsConfig {
            endpoint: bridge.tts_endpoint().to_owned(),
            backend_name: "irodori-tts-server".to_owned(),
            model_alias: Some(bridge.settings().model_alias()),
            model_version: None,
            voice_model: Some("example-voice-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            timeout: Duration::from_secs(10),
        },
        None,
    )
    .expect("adapter")
}

fn rig(bridge: &IrodoriBridge, reply: &str, mode: PhraseMode) -> LocalRig {
    local_rig_with_engines(
        MockThinking {
            reply: reply.to_owned(),
            failure: None,
        },
        tts_adapter(bridge),
        mode,
    )
}

const REPLY: &str = "配信に来てくれてありがとう😊。詳しくは https://example.com/live を見てね。また明日も遊ぼうね🎉。";

#[test]
fn spoken_replies_reach_the_server_projected_and_pinned_in_both_modes() {
    for mode in both_modes() {
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let root = scratch("spoken");
        let bridge = bridge_for(&server.endpoint(), "5000", &root);
        let mut rig = rig(&bridge, REPLY, mode);
        submit_free_text_line(&mut rig, 80);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut now_ms = 0;
        loop {
            rig.app.tick(now_ms);
            let snapshot = rig
                .app
                .generation_execution_snapshot()
                .expect("generation execution snapshot");
            if snapshot.is_quiescent() && now_ms >= 3_000 {
                break;
            }
            assert!(Instant::now() < deadline, "no quiescence: {snapshot:?}");
            now_ms += 10;
            std::thread::sleep(Duration::from_millis(1));
        }

        // ADR-0004 D-5: URLs and emoji never reach the server.
        let spoken = server.spoken_texts();
        assert!(!spoken.is_empty());
        for text in &spoken {
            assert!(
                !text.contains("http") && !text.contains("example.com"),
                "{text}"
            );
            assert!(!text.contains('😊') && !text.contains('🎉'), "{text}");
        }
        assert!(spoken.join("").contains("ありがとう"), "{spoken:?}");
        // The voice is pinned on every request of the session.
        for request in server.requests() {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("request JSON");
            assert_eq!(body["irodori"]["seed"], json!(4242));
            assert_eq!(body["irodori"]["no_ref"], json!(true));
        }

        let audio = rig.audio.lock().expect("audio log");
        assert!(!audio.is_empty(), "the reply plays");
        for command in audio.iter() {
            assert!(
                command.audio_ref.starts_with("audio://generated/")
                    && command.audio_ref.ends_with(".wav"),
                "{}",
                command.audio_ref
            );
            let file = root
                .join("audio")
                .join("generated")
                .join(command.audio_ref.trim_start_matches("audio://generated/"));
            assert!(file.is_file(), "{}", file.display());
        }
        drop(audio);
        let counts = rig.app.generation_fallback_counts();
        assert!(counts.by_reason.is_empty(), "no fallback: {counts:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Unreachable, non-2xx, malformed-audio and too-slow servers fail exactly like
/// a TTS sidecar failure: the generative path degrades to the visible
/// `reaction.neutral` fallback (issue #291), counted by reason, with no audio.
#[test]
fn a_server_failure_plays_the_visible_fallback_in_both_modes() {
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        drop(listener);
        endpoint
    };
    let server_error = FakeIrodoriServer::start(FakeBehaviour {
        status: 500,
        ..FakeBehaviour::default()
    });
    let malformed = FakeIrodoriServer::start(FakeBehaviour {
        wav_body: Some(b"not a wav".to_vec()),
        ..FakeBehaviour::default()
    });
    let slow = FakeIrodoriServer::start(FakeBehaviour {
        delay: Duration::from_millis(1_500),
        ..FakeBehaviour::default()
    });
    let cases: [(&str, String, &str); 4] = [
        ("unreachable", dead, "5000"),
        ("non-2xx", server_error.endpoint(), "5000"),
        ("malformed audio", malformed.endpoint(), "5000"),
        ("timeout", slow.endpoint(), "300"),
    ];
    for mode in both_modes() {
        for (label, endpoint, timeout_ms) in &cases {
            let root = scratch("fail");
            let bridge = bridge_for(endpoint, timeout_ms, &root);
            let mut rig = rig(&bridge, REPLY, mode);
            free_text_line(&mut rig, 81);
            assert_visible_fallback(&mut rig, "unavailable");
            let stats = bridge.stats();
            assert!(
                stats.ok == 0 && stats.engine_errors + stats.deadline_exceeded >= 1,
                "{label}: {stats:?}"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}

const LIVE_PHRASES: [&str; 12] = [
    "こんにちは、今日も配信に来てくれてありがとう。",
    "えっと、それはちょっと難しい質問だね。",
    "うん、いい感じだよ。次はどうしようかな。",
    "ごめんね、今のは聞き取れなかったよ。",
    "わあ、すごい！ 初見さんもいらっしゃい。",
    "今日はゲームの続きをやっていくよ。",
    "コメントありがとう、とても嬉しいです。",
    "ちょっと待ってね、考えてみるから。",
    "それじゃあ、そろそろ終わりにしようか。",
    "また明日も遊ぼうね。おやすみなさい。",
    "あはは、それは面白いね。もう一回言って。",
    "水分補給も忘れずにね。無理しないでね。",
];

fn live_bridge(root: &Path) -> IrodoriBridge {
    let endpoint = std::env::var("AIVTUBER_IRODORI_ENDPOINT").expect("AIVTUBER_IRODORI_ENDPOINT");
    bridge_for(&endpoint, "60000", root)
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// Opt-in live smoke through the real adapter and bridge against a running
/// official Irodori-TTS-Server (LUNA, GPU), recorded in the pull request:
/// `AIVTUBER_IRODORI_ENDPOINT=http://127.0.0.1:8088 cargo test --locked -p
/// aivtuber-app --lib irodori_live_synth -- --ignored --nocapture`.
/// Twelve phrases with fresh audio roots (no cache hit), latencies, sample
/// rate, and the same phrase synthesized again into another root: the WAV bytes
/// must be identical (the pinned voice).
#[test]
#[ignore = "needs a running Irodori-TTS-Server on a GPU"]
fn irodori_live_synth_twelve_phrases_and_a_repeat() {
    use aivtuber_domain::TtsEngine;
    let root = scratch("live");
    let bridge = live_bridge(&root);
    let adapter = tts_adapter(&bridge);
    let mut millis = Vec::new();
    let mut files = Vec::new();
    for text in LIVE_PHRASES {
        let started = Instant::now();
        let artifact = block_on(adapter.synthesize(&aivtuber_domain::SpeechRequest {
            text: text.to_owned(),
            style: None,
        }))
        .expect("synthesis through the bridge");
        let elapsed = started.elapsed().as_millis();
        millis.push(elapsed);
        let file = root
            .join("audio")
            .join("generated")
            .join(artifact.audio_ref.trim_start_matches("audio://generated/"));
        println!(
            "phrase latency_ms={elapsed} audio_ms={} {}",
            artifact.duration_ms, artifact.audio_ref
        );
        files.push(file);
    }
    // The same phrase through a second bridge with a fresh audio root, so the
    // server is asked again rather than the cache answering.
    let root2 = scratch("live-repeat");
    let bridge2 = live_bridge(&root2);
    let artifact = block_on(
        tts_adapter(&bridge2).synthesize(&aivtuber_domain::SpeechRequest {
            text: LIVE_PHRASES[0].to_owned(),
            style: None,
        }),
    )
    .expect("repeat synthesis");
    let again = root2
        .join("audio")
        .join("generated")
        .join(artifact.audio_ref.trim_start_matches("audio://generated/"));
    let first = std::fs::read(&files[0]).expect("first wav");
    let second = std::fs::read(&again).expect("second wav");
    println!(
        "repeat identical={} bytes={} sample_rate={}",
        first == second,
        first.len(),
        u32::from_le_bytes([first[24], first[25], first[26], first[27]])
    );
    assert_eq!(first, second, "the same phrase must give the same voice");
    millis.sort_unstable();
    println!(
        "live smoke: n={} median_ms={} p90_ms={} max_ms={}",
        millis.len(),
        millis[millis.len() / 2],
        millis[(millis.len() * 9).div_ceil(10) - 1],
        millis[millis.len() - 1]
    );
    drop((bridge, bridge2));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&root2);
}

/// Opt-in: with the server stopped, a reply through the real adapter, bridge
/// and local rig ends as the visible fallback and the performer keeps going.
#[test]
#[ignore = "run with the Irodori-TTS-Server stopped"]
fn irodori_live_degrades_cleanly_when_the_server_is_stopped() {
    for mode in both_modes() {
        let root = scratch("live-down");
        let endpoint =
            std::env::var("AIVTUBER_IRODORI_ENDPOINT").expect("AIVTUBER_IRODORI_ENDPOINT");
        let bridge = bridge_for(&endpoint, "5000", &root);
        let mut rig = rig(&bridge, REPLY, mode);
        free_text_line(&mut rig, 82);
        assert_visible_fallback(&mut rig, "unavailable");
        println!("degraded cleanly: {:?}", bridge.stats());
        let _ = std::fs::remove_dir_all(&root);
    }
}
