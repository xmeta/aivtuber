//! Issue #220 S2b: the TTS sidecar supervisor and the production TTS adapter
//! against the sidecar test double. No model and no engine library is involved;
//! every failure mode must degrade to a typed error without hanging the caller.

use aivtuber_adapters::speech_tts::supervisor::{LivenessGatedTts, TtsSidecar, TtsSidecarConfig};
use aivtuber_adapters::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig};
use aivtuber_domain::{EngineErrorKind, SpeechRequest, TtsEngine};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(future)
}

const DOUBLE: &str = env!("CARGO_BIN_EXE_local-tts-sidecar-double");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aivtuber-tts-sup-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(root: &std::path::Path, extra: &[&str]) -> TtsSidecarConfig {
    let mut args = vec![
        "--listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--audio-root".to_owned(),
        root.to_str().unwrap().to_owned(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let mut config = TtsSidecarConfig::new(DOUBLE, args);
    // The double loads no engine library.
    config.verify_engine_libs = false;
    config.ready_timeout = Duration::from_secs(5);
    config
}

fn adapter(addr: std::net::SocketAddr, timeout: Duration) -> NormalizedHttpTtsAdapter {
    NormalizedHttpTtsAdapter::new(
        NormalizedHttpTtsConfig {
            endpoint: format!("http://{addr}/v1/synthesize"),
            backend_name: "local-tts-sidecar".to_owned(),
            model_alias: Some("supertonic-3-int8".to_owned()),
            model_version: None,
            voice_model: Some("supertonic-3-int8".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            timeout,
        },
        None,
    )
    .unwrap()
}

fn speak(adapter: &NormalizedHttpTtsAdapter) -> Result<(), EngineErrorKind> {
    adapter
        .synthesize_sync(&SpeechRequest {
            text: "こんにちは".to_owned(),
            style: Some("calm".to_owned()),
        })
        .map(|_| ())
        .map_err(|e| e.kind)
}

#[test]
fn a_ready_sidecar_serves_the_production_adapter_and_ends_with_its_owner() {
    let root = scratch("normal");
    let sidecar = TtsSidecar::spawn(&config(&root, &[])).expect("ready");
    let addr = sidecar.ready().listen;
    assert!(addr.ip().is_loopback());
    assert_ne!(addr.port(), 0, "the ready line reports the bound port");
    let adapter = adapter(addr, Duration::from_secs(5));
    let artifact = adapter
        .synthesize_sync(&SpeechRequest {
            text: "こんにちは".to_owned(),
            style: None,
        })
        .expect("speech");
    assert_eq!(artifact.duration_ms, 500);
    let file = artifact.audio_ref.strip_prefix("audio://").unwrap();
    assert!(root.join(file).is_file(), "the WAV is under the audio root");

    // Dropping the owner ends the child: the port stops answering.
    drop(sidecar);
    let deadline = Instant::now() + Duration::from_secs(5);
    while speak(&adapter) != Err(EngineErrorKind::Unavailable) {
        assert!(Instant::now() < deadline, "the sidecar outlived its owner");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_sidecar_that_never_becomes_ready_is_a_start_failure_not_a_hang() {
    let root = scratch("never");
    let mut config = config(&root, &["--mode", "never-ready"]);
    config.ready_timeout = Duration::from_millis(500);
    let started = Instant::now();
    let error = TtsSidecar::spawn(&config).err().expect("not ready");
    assert!(error.contains("not ready in time"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(4));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_sidecar_that_exits_at_start_reports_it() {
    let root = scratch("badargs");
    // A non-loopback listen address makes the double exit 2 before it is ready.
    let mut config = config(&root, &[]);
    config.args[1] = "0.0.0.0:0".to_owned();
    let error = TtsSidecar::spawn(&config).err().expect("exits");
    assert!(error.contains("exited before it was ready"), "{error}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_sidecar_that_dies_mid_run_degrades_requests_to_a_typed_error() {
    let root = scratch("exit");
    let mut sidecar =
        TtsSidecar::spawn(&config(&root, &["--mode", "exit", "--after-ms", "300"])).expect("ready");
    let adapter = adapter(sidecar.ready().listen, Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(5);
    while sidecar.is_alive() {
        assert!(Instant::now() < deadline, "the double should have exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The raw adapter is bounded by its own timeout (a closed loopback port
    // is not always refused quickly on Windows); the gated engine the app
    // composes answers at once.
    assert!(matches!(
        speak(&adapter),
        Err(EngineErrorKind::Timeout | EngineErrorKind::Unavailable)
    ));
    assert!(!sidecar.is_alive());
    let flag_deadline = Instant::now() + Duration::from_secs(3);
    while sidecar.output_open() {
        assert!(Instant::now() < flag_deadline, "stdout should have closed");
        std::thread::sleep(Duration::from_millis(20));
    }
    let gated = LivenessGatedTts::new(Arc::new(adapter), sidecar.liveness());
    let started = Instant::now();
    let error = block_on(gated.synthesize(&SpeechRequest {
        text: "こんにちは".to_owned(),
        style: None,
    }))
    .unwrap_err();
    assert_eq!(error.kind, EngineErrorKind::Unavailable);
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "a dead sidecar is refused at once, not waited on"
    );
    assert_eq!(gated.identity().backend.name, "local-tts-sidecar");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hung_sidecar_costs_one_bounded_timeout_per_request() {
    let root = scratch("hang");
    let sidecar = TtsSidecar::spawn(&config(&root, &["--mode", "hang"])).expect("ready");
    let adapter = adapter(sidecar.ready().listen, Duration::from_millis(400));
    let started = Instant::now();
    let error = speak(&adapter).expect_err("a hung sidecar never answers");
    assert!(
        matches!(
            error,
            EngineErrorKind::Timeout | EngineErrorKind::Unavailable
        ),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_failing_engine_degrades_every_request_without_taking_the_sidecar_down() {
    let root = scratch("fail");
    let mut sidecar = TtsSidecar::spawn(&config(&root, &["--mode", "fail"])).expect("ready");
    let adapter = adapter(sidecar.ready().listen, Duration::from_secs(2));
    for _ in 0..3 {
        assert_eq!(speak(&adapter), Err(EngineErrorKind::Unavailable));
    }
    assert!(sidecar.is_alive());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_busy_sidecar_is_refused_with_overloaded_and_never_queues() {
    let root = scratch("busy");
    let sidecar = TtsSidecar::spawn(&config(&root, &["--synth-delay-ms", "700"])).expect("ready");
    let addr = sidecar.ready().listen;
    let slow = std::thread::spawn(move || {
        let adapter = adapter(addr, Duration::from_secs(5));
        adapter.synthesize_sync(&SpeechRequest {
            text: "ゆっくり".to_owned(),
            style: None,
        })
    });
    std::thread::sleep(Duration::from_millis(250));
    let started = Instant::now();
    let error = adapter(addr, Duration::from_secs(5))
        .synthesize_sync(&SpeechRequest {
            text: "別の文".to_owned(),
            style: None,
        })
        .unwrap_err();
    assert_eq!(error.kind, EngineErrorKind::Overloaded);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(slow.join().unwrap().is_ok());
    let _ = std::fs::remove_dir_all(&root);
}
