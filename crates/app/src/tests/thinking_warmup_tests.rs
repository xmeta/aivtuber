//! Thinking warm-up (issue #333).
//!
//! The server is a scripted single-slot loopback fake: it serves one
//! connection at a time and the first one it accepts is slow, like a cold
//! llama-server evaluating its first prompt. Times are scaled down from the
//! measured 19.4 s against a 15 s timeout: 700 ms against a 400 ms timeout.

use super::local_thinking_fallback_tests::{
    assert_visible_fallback, free_text_line, local_rig_with_engines,
};
use super::*;
use crate::thinking_warmup::{
    DEFAULT_WARMUP_TIMEOUT_MS, MAX_WARMUP_TIMEOUT_MS, MIN_WARMUP_TIMEOUT_MS, WarmupDecision,
    WarmupMode, decide, run, start, warmup_timeout,
};
use aivtuber_adapters::{
    OpenAiResponsesAdapter, OpenAiResponsesConfig, ThinkingAuthentication, WARMUP_USER_LINE,
};
use std::io::{Read, Write};
use std::net::TcpListener;

const COLD_DELAY: Duration = Duration::from_millis(700);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(400);
const WARMUP_REPLY: &str = "WARMUP-SECRET-REPLY";
const REAL_REPLY: &str = "配信に来てくれてありがとう。";

fn reply_body(text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": text, "annotations": []}
        ]}],
        "usage": {"input_tokens": 206}
    }))
    .expect("body")
}

/// Serves `connections` requests one at a time (a single slot). The first is
/// slow. Each request body is recorded; a request with `max_output_tokens: 1`
/// is the warm-up and gets the secret reply.
fn cold_server(connections: usize) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for index in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut received = Vec::new();
            let mut buffer = [0_u8; 4096];
            let body = loop {
                let Ok(n) = stream.read(&mut buffer) else {
                    return;
                };
                if n == 0 {
                    return;
                }
                received.extend_from_slice(&buffer[..n]);
                let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .expect("content length");
                if received.len() >= end + 4 + length {
                    break String::from_utf8_lossy(&received[end + 4..end + 4 + length])
                        .into_owned();
                }
            };
            let is_warmup = serde_json::from_str::<serde_json::Value>(&body)
                .map(|v| v["max_output_tokens"] == 1)
                .unwrap_or(false);
            seen.lock().unwrap().push(body);
            if index == 0 {
                std::thread::sleep(COLD_DELAY);
            }
            let payload = reply_body(if is_warmup { WARMUP_REPLY } else { REAL_REPLY });
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            // The client may already have given up (the timeout case).
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&payload);
        }
    });
    (endpoint, bodies)
}

fn adapter(endpoint: String) -> OpenAiResponsesAdapter {
    OpenAiResponsesAdapter::with_authentication(
        OpenAiResponsesConfig {
            endpoint,
            model_alias: "local-test-model".to_owned(),
            timeout: REQUEST_TIMEOUT,
            ..OpenAiResponsesConfig::default()
        },
        ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .expect("loopback adapter")
    .with_backend_name("llama-server")
}

fn rig_for(adapter: OpenAiResponsesAdapter) -> (LocalRig, Arc<Mutex<Vec<String>>>) {
    let tts = MockTts::default();
    let spoken = Arc::clone(&tts.texts);
    (
        local_rig_with_engines(adapter, tts, PhraseMode::Buffered),
        spoken,
    )
}

type LocalRig = super::local_thinking_fallback_tests::LocalRig;

#[test]
fn auto_runs_only_for_the_unauthenticated_loopback_llama_server() {
    let run_it = WarmupDecision::Run;
    assert_eq!(
        decide(WarmupMode::Auto, "unauthenticated-loopback", "llama-server"),
        run_it
    );
    // A remote or paid endpoint, or another backend name, is not warmed.
    for (auth, backend) in [
        ("bearer", "openai-compatible-responses"),
        ("bearer", "llama-server"),
        ("unauthenticated-loopback", "openai-compatible-responses"),
    ] {
        assert!(
            matches!(
                decide(WarmupMode::Auto, auth, backend),
                WarmupDecision::Skip(_)
            ),
            "{auth} {backend}"
        );
    }
    assert_eq!(decide(WarmupMode::On, "bearer", "x"), run_it);
    assert_eq!(
        decide(WarmupMode::Off, "unauthenticated-loopback", "llama-server"),
        WarmupDecision::Skip("disabled")
    );
}

#[test]
fn mode_and_timeout_settings_are_parsed_and_bounded() {
    assert_eq!(WarmupMode::parse(None), Ok(WarmupMode::Auto));
    assert_eq!(WarmupMode::parse(Some("AUTO")), Ok(WarmupMode::Auto));
    assert_eq!(WarmupMode::parse(Some("on")), Ok(WarmupMode::On));
    assert_eq!(WarmupMode::parse(Some("off")), Ok(WarmupMode::Off));
    assert!(WarmupMode::parse(Some("sometimes")).is_err());

    assert_eq!(
        warmup_timeout(DEFAULT_WARMUP_TIMEOUT_MS),
        Ok(Duration::from_secs(120))
    );
    assert!(warmup_timeout(MIN_WARMUP_TIMEOUT_MS - 1).is_err());
    assert!(warmup_timeout(MAX_WARMUP_TIMEOUT_MS + 1).is_err());
    assert!(warmup_timeout(MIN_WARMUP_TIMEOUT_MS).is_ok());
    assert!(warmup_timeout(MAX_WARMUP_TIMEOUT_MS).is_ok());
}

#[test]
fn a_failed_warm_up_only_logs_and_never_blocks_the_caller() {
    // Refused connection: reported as a failure, no panic, no error value.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    drop(listener);
    let lines = Mutex::new(Vec::new());
    let ok = run(&adapter(endpoint), Duration::from_secs(8), &|line| {
        lines.lock().unwrap().push(line.to_owned())
    });
    assert!(!ok);
    let lines = lines.into_inner().unwrap();
    assert!(
        lines[0].starts_with("thinking_warmup: started"),
        "{lines:?}"
    );
    assert_eq!(lines[1], "thinking_warmup: failed reason=unavailable");

    // A hanging server: `start` returns at once; the thread is what waits.
    let (endpoint, _bodies) = cold_server(1);
    let began = Instant::now();
    let handle = start(
        WarmupMode::On,
        &adapter(endpoint),
        "llama-server",
        Duration::from_secs(5),
    )
    .expect("warm-up thread");
    assert!(began.elapsed() < Duration::from_millis(300));
    handle.join().expect("warm-up thread ends without panic");

    // Skipped modes start nothing.
    assert!(
        start(
            WarmupMode::Off,
            &adapter("http://127.0.0.1:9/v1/responses".to_owned()),
            "llama-server",
            Duration::from_secs(5)
        )
        .is_none()
    );
}

#[test]
fn a_successful_warm_up_logs_its_time_and_prompt_tokens() {
    let (endpoint, bodies) = cold_server(1);
    let lines = Mutex::new(Vec::new());
    assert!(run(&adapter(endpoint), Duration::from_secs(5), &|line| {
        lines.lock().unwrap().push(line.to_owned())
    }));
    let lines = lines.into_inner().unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[1].starts_with("thinking_warmup: ok ms=") && lines[1].ends_with(" prompt_tokens=206"),
        "{lines:?}"
    );
    assert!(bodies.lock().unwrap()[0].contains(WARMUP_USER_LINE));
}

#[test]
fn the_first_real_line_succeeds_while_the_cold_warm_up_is_still_running() {
    let (endpoint, bodies) = cold_server(2);
    let adapter = adapter(endpoint);
    let handle = start(
        WarmupMode::On,
        &adapter,
        "llama-server",
        Duration::from_secs(5),
    )
    .expect("warm-up thread");
    let (mut rig, spoken) = rig_for(adapter);
    // The warm-up has reached the server and is in its cold prompt eval.
    let deadline = Instant::now() + Duration::from_secs(5);
    while bodies.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "warm-up never reached the server"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Without the overlap grace this request would wait ~600 ms for a
    // 400 ms timeout and fall back, which is the first-chat defect.
    free_text_line(&mut rig, 61);
    handle.join().expect("warm-up thread");

    let counts = rig.app.generation_fallback_counts();
    assert!(counts.by_reason.is_empty(), "no fallback: {counts:?}");
    // Only the real reply is ever spoken; the warm-up output is discarded.
    assert_eq!(spoken.lock().unwrap().as_slice(), [REAL_REPLY]);
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert!(sent[0].contains(WARMUP_USER_LINE) && !sent[1].contains(WARMUP_USER_LINE));
    assert!(!format!("{:?}", rig.app.telemetry().events()).contains(WARMUP_REPLY));
}

#[test]
fn without_a_warm_up_the_same_cold_first_line_falls_back_on_timeout() {
    // The control for the test above: the cold request is the real one.
    let (endpoint, _bodies) = cold_server(1);
    let (mut rig, spoken) = rig_for(adapter(endpoint));
    free_text_line(&mut rig, 62);
    assert_visible_fallback(&mut rig, "timeout");
    assert!(spoken.lock().unwrap().is_empty());
}
