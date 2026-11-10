//! Tests of the SSE parser, the provider-event mapping and the bounded
//! streaming call against a scripted loopback server (issue #83 S2).

use super::stream::{SseError, SseParser, StreamItem, StreamLimits, parse_stream_event};
use super::{OpenAiResponsesAdapter, OpenAiResponsesConfig};
use crate::ThinkingAuthentication;
use aivtuber_domain::{
    DeltaFlow, EngineErrorKind, EventEnvelope, PrivacyClass, ReflexContext, ReplyDeltaSink,
    RetrievalSnapshot, ThinkingEngine, ThinkingRequest,
};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

fn parse_all(chunks: &[&[u8]], max: usize) -> Result<Vec<String>, SseError> {
    let mut parser = SseParser::new(max);
    let mut out = Vec::new();
    for chunk in chunks {
        parser.feed(chunk, &mut out)?;
    }
    Ok(out)
}

#[test]
fn a_frame_split_at_every_byte_boundary_parses_identically() {
    let wire = b"event: x\ndata: {\"a\":1}\n\n: comment\ndata: two\ndata: lines\n\ndata:[DONE]\n\n";
    let whole = parse_all(&[wire], 1024).unwrap();
    assert_eq!(whole, ["{\"a\":1}", "two\nlines", "[DONE]"]);
    for split in 1..wire.len() {
        let parts = parse_all(&[&wire[..split], &wire[split..]], 1024).unwrap();
        assert_eq!(parts, whole, "split at {split}");
    }
    let singles: Vec<&[u8]> = wire.chunks(1).collect();
    assert_eq!(parse_all(&singles, 1024).unwrap(), whole);
}

#[test]
fn crlf_cr_and_lf_line_ends_all_frame_events() {
    for wire in [
        &b"data: a\r\n\r\ndata: b\r\n\r\n"[..],
        &b"data: a\r\rdata: b\r\r"[..],
        &b"data: a\n\ndata: b\n\n"[..],
    ] {
        assert_eq!(parse_all(&[wire], 64).unwrap(), ["a", "b"]);
    }
    // CRLF split between reads must not produce a phantom blank line.
    assert_eq!(
        parse_all(&[b"data: a\r", b"\ndata: b\r\n\r\n"], 64).unwrap(),
        ["a\nb"]
    );
}

#[test]
fn multibyte_text_split_across_reads_survives() {
    let wire = "data: 今日は\n\n".as_bytes();
    for split in 1..wire.len() {
        let parts = parse_all(&[&wire[..split], &wire[split..]], 64).unwrap();
        assert_eq!(parts, ["今日は"], "split at {split}");
    }
}

#[test]
fn an_oversized_event_is_refused_and_an_unterminated_one_is_dropped() {
    let big = format!("data: {}\n\n", "x".repeat(100));
    assert_eq!(
        parse_all(&[big.as_bytes()], 50),
        Err(SseError::EventTooLarge)
    );
    // Many small data lines of one event also count toward the bound.
    let lines = "data: xxxxxxxxxx\n".repeat(20);
    assert_eq!(
        parse_all(&[lines.as_bytes()], 50),
        Err(SseError::EventTooLarge)
    );
    // Without its blank line the event is never dispatched.
    assert!(parse_all(&[b"data: partial"], 64).unwrap().is_empty());
    assert_eq!(
        parse_all(&[b"data: \xff\xfe\n\n"], 64),
        Err(SseError::InvalidUtf8)
    );
}

#[test]
fn provider_events_map_to_plain_deltas() {
    let delta = |json: &str| parse_stream_event(json).unwrap();
    assert_eq!(
        delta(r#"{"type":"response.output_text.delta","delta":"こん"}"#),
        StreamItem::Delta("こん".to_owned())
    );
    assert_eq!(
        delta(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#),
        StreamItem::Delta("hi".to_owned())
    );
    assert_eq!(
        delta(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
        StreamItem::Ignore
    );
    assert_eq!(
        delta(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        StreamItem::Ignore
    );
    assert_eq!(delta(r#"{"type":"response.created"}"#), StreamItem::Ignore);
    assert_eq!(delta("[DONE]"), StreamItem::Done);
    assert_eq!(delta(r#"{"type":"response.completed"}"#), StreamItem::Done);
    assert_eq!(delta(r#"{"type":"response.incomplete"}"#), StreamItem::Done);
    for failure in [
        r#"{"type":"response.failed"}"#,
        r#"{"type":"error","message":"boom"}"#,
        r#"{"error":{"message":"boom"}}"#,
        "not json",
        "",
    ] {
        assert!(parse_stream_event(failure).is_err(), "{failure}");
    }
}

// -- against a loopback stub server ------------------------------------------

type Script = Vec<(Duration, Vec<u8>)>;

/// One scripted connection: after the request is read, each step writes its
/// bytes after its delay; the connection then closes.
fn serve(steps: Script) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut seen = Vec::new();
        let mut buffer = [0_u8; 1024];
        let mut body_start = None;
        let mut content_length = 0_usize;
        loop {
            let n = stream.read(&mut buffer).unwrap_or(0);
            if n == 0 {
                return;
            }
            seen.extend_from_slice(&buffer[..n]);
            if body_start.is_none()
                && let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n")
            {
                body_start = Some(at + 4);
                let head = String::from_utf8_lossy(&seen[..at]).to_ascii_lowercase();
                content_length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
            }
            if let Some(start) = body_start
                && seen.len() >= start + content_length
            {
                break;
            }
        }
        let request_text = String::from_utf8_lossy(&seen).to_string();
        assert!(request_text.contains("\"stream\":true"), "{request_text}");
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        );
        for (delay, bytes) in steps {
            std::thread::sleep(delay);
            if stream.write_all(&bytes).is_err() || stream.flush().is_err() {
                return;
            }
        }
    });
    format!("http://{address}/v1/responses")
}

fn adapter(endpoint: String, limits: StreamLimits) -> OpenAiResponsesAdapter {
    OpenAiResponsesAdapter::with_authentication(
        OpenAiResponsesConfig {
            endpoint,
            model_alias: "local".to_owned(),
            ..OpenAiResponsesConfig::default()
        },
        ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .expect("loopback adapter")
    .with_streaming(limits)
    .expect("limits")
}

fn request() -> ThinkingRequest {
    let event: EventEnvelope = serde_json::from_str(include_str!(
        "../../../../examples/events/chat-message.json"
    ))
    .expect("chat fixture");
    ThinkingRequest::from_event(
        &event,
        "curated viewer message",
        PrivacyClass::Pseudonymous,
        ReflexContext::default(),
        RetrievalSnapshot::default(),
        Vec::new(),
    )
}

#[derive(Default)]
struct Collect {
    deltas: Vec<String>,
    cancel_after: Option<usize>,
}

impl ReplyDeltaSink for Collect {
    fn push(&mut self, delta: &str) -> DeltaFlow {
        self.deltas.push(delta.to_owned());
        DeltaFlow::Continue
    }
    fn is_cancelled(&self) -> bool {
        self.cancel_after.is_some_and(|n| self.deltas.len() >= n)
    }
}

fn chat(text: &str) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"content": text}}]})
    )
    .into_bytes()
}

fn step(ms: u64, bytes: Vec<u8>) -> (Duration, Vec<u8>) {
    (Duration::from_millis(ms), bytes)
}

#[test]
fn deltas_arrive_incrementally_and_the_reply_is_complete_at_done() {
    let endpoint = serve(vec![
        step(0, chat("こんにちは、")),
        step(80, chat("今日は")),
        step(80, b"data: [DONE]\n\n".to_vec()),
    ]);
    let adapter = adapter(endpoint, StreamLimits::default());
    assert!(adapter.streams());
    let mut sink = Collect::default();
    let reply = adapter
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .expect("streaming enabled")
        .expect("stream completes");
    assert_eq!(reply.text, "こんにちは、今日は");
    assert_eq!(sink.deltas, ["こんにちは、", "今日は"]);
}

#[test]
fn responses_api_events_and_a_frame_split_across_writes_work_too() {
    let frame = format!(
        "event: response.output_text.delta\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"やあ"})
    )
    .into_bytes();
    let (head, tail) = frame.split_at(20);
    let endpoint = serve(vec![
        step(0, head.to_vec()),
        step(60, tail.to_vec()),
        step(10, b"data: {\"type\":\"response.completed\"}\n\n".to_vec()),
    ]);
    let adapter = adapter(endpoint, StreamLimits::default());
    let mut sink = Collect::default();
    let reply = adapter
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .unwrap()
        .unwrap();
    assert_eq!(reply.text, "やあ");
}

#[test]
fn a_stream_that_ends_without_a_completion_marker_is_a_failure() {
    let endpoint = serve(vec![step(0, chat("途中まで"))]);
    let adapter = adapter(endpoint, StreamLimits::default());
    let mut sink = Collect::default();
    let error = adapter
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind, EngineErrorKind::Unavailable);
    assert_eq!(
        sink.deltas,
        ["途中まで"],
        "the delta before the cut was delivered"
    );
}

#[test]
fn malformed_and_oversized_events_fail_the_stream() {
    let endpoint = serve(vec![step(0, b"data: {not json\n\n".to_vec())]);
    let mut sink = Collect::default();
    let error = adapter(endpoint, StreamLimits::default())
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind, EngineErrorKind::Backend);

    let endpoint = serve(vec![step(0, chat(&"x".repeat(400)))]);
    let limits = StreamLimits {
        max_event_bytes: 128,
        ..StreamLimits::default()
    };
    let mut sink = Collect::default();
    let error = adapter(endpoint, limits)
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .unwrap()
        .unwrap_err();
    assert!(error.message.contains("byte limit"), "{error}");
    assert!(sink.deltas.is_empty());
}

#[test]
fn the_reply_character_bound_stops_the_stream() {
    let endpoint = serve(vec![
        step(0, chat("12345678")),
        step(30, chat("12345678")),
        step(30, b"data: [DONE]\n\n".to_vec()),
    ]);
    let limits = StreamLimits {
        max_reply_chars: 10,
        ..StreamLimits::default()
    };
    let mut sink = Collect::default();
    let error = adapter(endpoint, limits)
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .unwrap()
        .unwrap_err();
    assert!(error.message.contains("character limit"), "{error}");
    assert_eq!(sink.deltas.len(), 1);
}

#[test]
fn silence_between_chunks_hits_the_idle_timeout_not_the_deadline() {
    let endpoint = serve(vec![
        step(0, chat("はい。")),
        step(3_000, b"data: [DONE]\n\n".to_vec()),
    ]);
    let limits = StreamLimits {
        idle_timeout: Duration::from_millis(150),
        ..StreamLimits::default()
    };
    let mut sink = Collect::default();
    let started = Instant::now();
    let error = adapter(endpoint, limits)
        .generate_streaming(&request(), Duration::from_secs(30), &mut sink)
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind, EngineErrorKind::Timeout);
    assert!(error.message.contains("idle"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(sink.deltas, ["はい。"]);
}

#[test]
fn the_overall_deadline_bounds_a_stream_that_keeps_talking() {
    let steps: Script = (0..100).map(|_| step(40, chat("あ"))).collect();
    let endpoint = serve(steps);
    let mut sink = Collect::default();
    let started = Instant::now();
    let error = adapter(endpoint, StreamLimits::default())
        .generate_streaming(&request(), Duration::from_millis(300), &mut sink)
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind, EngineErrorKind::Timeout);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!sink.deltas.is_empty());
}

#[test]
fn cancellation_returns_promptly_even_while_the_provider_is_silent() {
    let endpoint = serve(vec![
        step(0, chat("はい。")),
        step(5_000, b"data: [DONE]\n\n".to_vec()),
    ]);
    let mut sink = Collect {
        cancel_after: Some(1),
        ..Collect::default()
    };
    let started = Instant::now();
    let error = adapter(endpoint, StreamLimits::default())
        .generate_streaming(&request(), Duration::from_secs(30), &mut sink)
        .unwrap()
        .unwrap_err();
    assert_eq!(error.message, "stream stopped by the consumer");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_non_success_status_maps_like_the_buffered_call() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        }
    });
    let mut sink = Collect::default();
    let error = adapter(endpoint, StreamLimits::default())
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind, EngineErrorKind::RateLimited);
}

#[test]
fn streaming_is_off_unless_enabled() {
    let adapter = OpenAiResponsesAdapter::with_authentication(
        OpenAiResponsesConfig {
            endpoint: "http://127.0.0.1:1/v1/responses".to_owned(),
            ..OpenAiResponsesConfig::default()
        },
        ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .unwrap();
    assert!(!adapter.streams());
    let mut sink = Collect::default();
    assert!(
        adapter
            .generate_streaming(&request(), Duration::from_secs(1), &mut sink)
            .is_none()
    );
    let bad = StreamLimits {
        idle_timeout: Duration::ZERO,
        ..StreamLimits::default()
    };
    assert!(adapter.with_streaming(bad).is_err());
}
