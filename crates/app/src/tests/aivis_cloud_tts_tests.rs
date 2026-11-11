//! `local` profile with the optional Aivis Cloud API backend (issue #327).
//!
//! The whole TTS path is real except the engine: the generative pipeline's
//! `NormalizedHttpTtsAdapter` posts the normalized contract to the in-process
//! bridge, which drives a fake Aivis Cloud API on loopback (no key, no model,
//! no network beyond 127.0.0.1). Thinking is a scripted stub.

use super::local_thinking_fallback_tests::{
    LocalRig, assert_visible_fallback, both_modes, free_text_line, local_rig_with_engines,
    submit_free_text_line,
};
use super::*;
use aivtuber_adapters::speech_tts::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
use aivtuber_adapters::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig};
use std::collections::HashMap;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-327-{name}-{}-{}",
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
fn bridge_for(api_endpoint: &str, timeout_ms: &str, root: &Path) -> AivisBridge {
    let env: HashMap<&str, &str> = HashMap::from([
        ("AIVTUBER_TTS_BACKEND", "aivis-cloud"),
        ("AIVIS_API", "sk-test-SECRETKEY"),
        (
            "AIVTUBER_AIVIS_MODEL_UUID",
            "a59cb814-0083-4369-8542-f51a29e72af7",
        ),
        ("AIVTUBER_AIVIS_CREDIT", "Aivis:test"),
        ("AIVTUBER_AIVIS_ENDPOINT", api_endpoint),
        ("AIVTUBER_AIVIS_TIMEOUT_MS", timeout_ms),
    ]);
    let settings = AivisSettings::resolve(
        &|name| env.get(name).map(|value| (*value).to_owned()),
        CompositionProfile::Local,
        true,
    )
    .expect("settings")
    .expect("aivis selected");
    AivisBridge::start(settings, &root.join("audio"), root).expect("bridge")
}

fn tts_adapter(bridge: &AivisBridge) -> NormalizedHttpTtsAdapter {
    NormalizedHttpTtsAdapter::new(
        NormalizedHttpTtsConfig {
            endpoint: bridge.tts_endpoint().to_owned(),
            backend_name: "aivis-cloud".to_owned(),
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

fn rig(bridge: &AivisBridge, reply: &str, mode: PhraseMode) -> LocalRig {
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
fn spoken_replies_reach_the_engine_as_projected_text_in_both_modes() {
    for mode in both_modes() {
        let phrased = matches!(mode, PhraseMode::Phrased(_));
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let root = scratch("spoken");
        let bridge = bridge_for(&api.endpoint(), "5000", &root);
        let mut rig = rig(&bridge, REPLY, mode);
        submit_free_text_line(&mut rig, 70);
        // A phrased reply keeps synthesizing as earlier phrases start playing,
        // so the logical clock has to move while the worker runs.
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

        // ADR-0004 D-5: URLs and emoji never reach the service.
        let spoken = api.spoken_texts();
        assert!(!spoken.is_empty());
        for text in &spoken {
            assert!(
                !text.contains("http") && !text.contains("example.com"),
                "{text}"
            );
            assert!(!text.contains('😊') && !text.contains('🎉'), "{text}");
        }
        let joined = spoken.join("");
        assert!(joined.contains("ありがとう"), "{joined}");
        assert!(joined.contains("見てね"), "{joined}");
        assert!(joined.contains("また明日も遊ぼうね"), "{joined}");
        if phrased {
            // Phrase mode: one bridge synthesis per phrase.
            assert!(spoken.len() >= 2, "per-phrase synthesis: {spoken:?}");
        } else {
            assert_eq!(
                spoken.len(),
                1,
                "one synthesis for the whole reply: {spoken:?}"
            );
        }

        // The reply is spoken: audio commands name WAV files the bridge wrote.
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

/// Unreachable, rejected (401/402/429/5xx) and too-slow services fail exactly like a TTS sidecar
/// failure: the generative path degrades to the visible `reaction.neutral`
/// fallback (issue #291), counted by reason, with no audio.
#[test]
fn a_service_failure_plays_the_visible_fallback_in_both_modes() {
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        drop(listener);
        endpoint
    };
    let rejected: Vec<(String, FakeAivisCloud)> = [401_u16, 402, 429, 503]
        .into_iter()
        .map(|status| {
            (
                format!("http {status}"),
                FakeAivisCloud::start(FakeAivisBehaviour {
                    status,
                    ..FakeAivisBehaviour::default()
                }),
            )
        })
        .collect();
    let slow = FakeAivisCloud::start(FakeAivisBehaviour {
        delay: Duration::from_millis(1_500),
        ..FakeAivisBehaviour::default()
    });
    let mut cases: Vec<(String, String, &str)> = vec![
        ("unreachable".to_owned(), dead, "5000"),
        ("timeout".to_owned(), slow.endpoint(), "300"),
    ];
    for (label, api) in &rejected {
        cases.push((label.clone(), api.endpoint(), "5000"));
    }
    for mode in both_modes() {
        for (label, endpoint, timeout_ms) in &cases {
            let root = scratch("fail");
            let bridge = bridge_for(endpoint, timeout_ms, &root);
            let mut rig = rig(&bridge, REPLY, mode);
            free_text_line(&mut rig, 71);
            // The bridge answers 5xx; the adapter maps every one to the
            // provider-unavailable class of the fallback vocabulary.
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
