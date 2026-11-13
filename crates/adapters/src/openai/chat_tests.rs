//! Chat-completions wire, OpenRouter headers/policy, 402 typing and usage
//! parsing, against mock transports (issue #363).

use super::stream::{StreamLimits, parse_event_usage};
use super::warmup::WARMUP_USER_LINE;
use super::*;
use crate::http::test_support::MockHttpTransport;
use crate::{HttpResponse, HttpStreamResponse};
use aivtuber_domain::{
    DeltaFlow, EventEnvelope, PrivacyClass, ReflexContext, RetrievalSnapshot, ThinkingRequest,
};
use std::io::Cursor;
use std::sync::Mutex;

const SECRET: &str = "openrouter-secret-value";

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

fn openrouter_config() -> OpenAiResponsesConfig {
    OpenAiResponsesConfig {
        endpoint: "https://openrouter.ai/api/v1/chat/completions".to_owned(),
        model_alias: "vendor/some-model".to_owned(),
        wire: ThinkingWire::ChatCompletions,
        extra_headers: ExtraHeaders::new(&[
            ("HTTP-Referer", "https://example.invalid/app"),
            ("X-Title", "aivtuber"),
        ])
        .expect("headers"),
        data_collection: Some(DataCollection::Deny),
        ..OpenAiResponsesConfig::default()
    }
}

fn adapter(config: OpenAiResponsesConfig, transport: &MockHttpTransport) -> OpenAiResponsesAdapter {
    OpenAiResponsesAdapter::with_transport(
        config,
        SecretString::new(SECRET),
        Arc::new(transport.clone()),
    )
    .expect("adapter")
}

fn ok(body: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: serde_json::to_vec(&body).expect("json"),
        content_type: Some("application/json".to_owned()),
    }
}

fn chat_reply(text: &str, usage: Value) -> Value {
    let mut body =
        json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": text}}]});
    if !usage.is_null() {
        body["usage"] = usage;
    }
    body
}

fn status(code: u16) -> HttpResponse {
    HttpResponse {
        status: code,
        body: b"{}".to_vec(),
        content_type: Some("application/json".to_owned()),
    }
}

#[test]
fn the_default_config_still_speaks_responses_with_no_headers_or_provider_field() {
    let config = OpenAiResponsesConfig::default();
    assert_eq!(config.wire, ThinkingWire::Responses);
    assert!(config.extra_headers.is_empty());
    assert_eq!(config.data_collection, None);

    let transport = MockHttpTransport::new(Ok(ok(json!({
        "output": [{"content": [{"type": "output_text", "text": "hi"}]}]
    }))));
    adapter(config, &transport)
        .generate_sync(&request())
        .expect("reply");
    let sent = &transport.requests()[0];
    assert!(sent.headers.is_empty());
    let body: Value = serde_json::from_slice(&sent.body).expect("body");
    assert!(body.get("input").is_some() && body.get("messages").is_none());
    assert!(body.get("provider").is_none() && body.get("stream").is_none());
}

#[test]
fn openrouter_request_has_the_chat_shape_headers_and_data_collection_deny() {
    let transport = MockHttpTransport::new(Ok(ok(chat_reply("こんにちは", Value::Null))));
    let reply = adapter(openrouter_config(), &transport)
        .generate_sync(&request())
        .expect("reply");
    assert_eq!(reply.text, "こんにちは");
    assert_eq!(reply.usage, None, "no usage object means none");

    let sent = &transport.requests()[0];
    assert_eq!(
        sent.endpoint,
        "https://openrouter.ai/api/v1/chat/completions"
    );
    assert_eq!(sent.bearer_token.as_deref(), Some(SECRET));
    assert_eq!(
        sent.headers,
        [
            (
                "HTTP-Referer".to_owned(),
                "https://example.invalid/app".to_owned()
            ),
            ("X-Title".to_owned(), "aivtuber".to_owned()),
        ]
    );
    let raw = String::from_utf8(sent.body.clone()).expect("utf-8");
    assert!(!raw.contains(SECRET), "the key must never be in the body");
    let body: Value = serde_json::from_str(&raw).expect("body");
    assert_eq!(body["model"], "vendor/some-model");
    assert_eq!(body["max_tokens"], 256);
    assert_eq!(body["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .expect("content")
            .contains("trust/privacy labels")
    );
    assert_eq!(body["provider"], json!({"data_collection": "deny"}));
    assert!(body.get("input").is_none() && body.get("max_output_tokens").is_none());
    assert!(body.get("stream").is_none());
}

#[test]
fn generic_chat_wire_sends_no_provider_field_and_allow_is_explicit() {
    let transport = MockHttpTransport::new(Ok(ok(chat_reply("ok", Value::Null))));
    let generic = OpenAiResponsesConfig {
        wire: ThinkingWire::ChatCompletions,
        ..OpenAiResponsesConfig::default()
    };
    adapter(generic, &transport)
        .generate_sync(&request())
        .expect("reply");
    let body: Value = serde_json::from_slice(&transport.requests()[0].body).expect("body");
    assert!(body.get("provider").is_none());
    assert!(body.get("messages").is_some());

    let allow = OpenAiResponsesConfig {
        data_collection: Some(DataCollection::Allow),
        ..openrouter_config()
    };
    adapter(allow, &transport)
        .generate_sync(&request())
        .expect("reply");
    let body: Value = serde_json::from_slice(&transport.requests()[1].body).expect("body");
    assert_eq!(body["provider"], json!({"data_collection": "allow"}));
}

#[test]
fn data_collection_needs_the_chat_wire_and_config_debug_hides_header_values() {
    let error = OpenAiResponsesAdapter::with_transport(
        OpenAiResponsesConfig {
            data_collection: Some(DataCollection::Deny),
            ..OpenAiResponsesConfig::default()
        },
        SecretString::new(SECRET),
        Arc::new(MockHttpTransport::new(Ok(status(200)))),
    )
    .expect_err("a Responses body has no provider field");
    assert_eq!(error.kind, EngineErrorKind::InvalidRequest);

    let transport = MockHttpTransport::new(Ok(status(200)));
    let debug = format!("{:?}", adapter(openrouter_config(), &transport));
    assert!(debug.contains("X-Title") && !debug.contains("example.invalid"));
    assert!(!debug.contains(SECRET));
}

#[test]
fn http_statuses_are_typed_so_every_one_degrades_to_the_fallback() {
    for (code, kind) in [
        (401, EngineErrorKind::Authentication),
        (402, EngineErrorKind::QuotaExhausted),
        (403, EngineErrorKind::Authentication),
        (404, EngineErrorKind::InvalidRequest),
        (429, EngineErrorKind::RateLimited),
        (500, EngineErrorKind::Unavailable),
        (502, EngineErrorKind::Unavailable),
        (503, EngineErrorKind::Overloaded),
        (529, EngineErrorKind::Overloaded),
    ] {
        let transport = MockHttpTransport::new(Ok(status(code)));
        let error = adapter(openrouter_config(), &transport)
            .generate_sync(&request())
            .expect_err("error status");
        assert_eq!(error.kind, kind, "HTTP {code}");
        assert!(!error.message.contains(SECRET));
    }
}

#[test]
fn malformed_replies_are_typed_errors_not_panics() {
    for (name, response) in [
        (
            "not json",
            HttpResponse {
                status: 200,
                body: b"<html>gateway</html>".to_vec(),
                content_type: None,
            },
        ),
        ("no choices", ok(json!({"choices": []}))),
        (
            "null content",
            ok(json!({"choices": [{"message": {"content": null}}]})),
        ),
        ("blank content", ok(chat_reply("  \n", Value::Null))),
        ("responses shape", ok(json!({"output": []}))),
    ] {
        let transport = MockHttpTransport::new(Ok(response));
        let error = adapter(openrouter_config(), &transport)
            .generate_sync(&request())
            .expect_err(name);
        assert_eq!(error.kind, EngineErrorKind::Backend, "{name}");
    }
    // A gateway error object inside HTTP 200 keeps the status it names.
    let transport = MockHttpTransport::new(Ok(ok(
        json!({"error": {"code": 402, "message": "Insufficient credits"}}),
    )));
    let error = adapter(openrouter_config(), &transport)
        .generate_sync(&request())
        .expect_err("embedded 402");
    assert_eq!(error.kind, EngineErrorKind::QuotaExhausted);
}

#[test]
fn usage_and_cost_are_parsed_into_bounded_integers() {
    let transport = MockHttpTransport::new(Ok(ok(chat_reply(
        "hi",
        json!({"prompt_tokens": 120, "completion_tokens": 18, "total_tokens": 138, "cost": 0.000425}),
    ))));
    let reply = adapter(openrouter_config(), &transport)
        .generate_sync(&request())
        .expect("reply");
    assert_eq!(
        reply.usage,
        Some(ThinkingUsage {
            prompt_tokens: 120,
            completion_tokens: 18,
            cost_micro_usd: Some(425),
        })
    );
}

#[test]
fn usage_edge_cases_never_fail_the_reply_and_never_overflow() {
    let parse = |usage: Value| parse_usage(&json!({ "usage": usage }));
    // Responses-style names are understood too.
    assert_eq!(
        parse(json!({"input_tokens": 5, "output_tokens": 7})),
        Some(ThinkingUsage {
            prompt_tokens: 5,
            completion_tokens: 7,
            cost_micro_usd: None
        })
    );
    // Token counts saturate instead of wrapping.
    let huge = parse(json!({"prompt_tokens": 5_000_000_000_u64, "completion_tokens": u64::MAX}))
        .expect("usage");
    assert_eq!(
        (huge.prompt_tokens, huge.completion_tokens),
        (u32::MAX, u32::MAX)
    );
    // Malformed cost is dropped; the tokens survive.
    for bad_cost in [
        json!(-0.5),
        json!("0.1"),
        json!(1e12),
        json!(null),
        json!(true),
    ] {
        let usage = parse(json!({"prompt_tokens": 1, "completion_tokens": 2, "cost": bad_cost}))
            .expect("tokens survive");
        assert_eq!(usage.cost_micro_usd, None, "cost {bad_cost}");
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (1, 2));
    }
    assert_eq!(
        parse(json!({"cost": 0.0})).and_then(|u| u.cost_micro_usd),
        Some(0)
    );
    assert_eq!(
        parse(json!({"cost": 1.5})).and_then(|u| u.cost_micro_usd),
        Some(1_500_000)
    );
    // Wrong types or nothing usable: no usage, no error.
    for none in [
        json!("lots"),
        json!([1, 2]),
        json!({}),
        json!({"prompt_tokens": -3, "completion_tokens": 1.5}),
        json!({"prompt_tokens": "9"}),
        Value::Null,
    ] {
        assert_eq!(parse(none.clone()), None, "{none}");
    }
    assert_eq!(parse_usage(&json!({})), None);
}

#[test]
fn warm_up_uses_the_chat_shape_and_headers_on_the_chat_wire() {
    let transport = MockHttpTransport::new(Ok(ok(chat_reply(
        "ignored",
        json!({"prompt_tokens": 77, "completion_tokens": 1}),
    ))));
    let report = adapter(openrouter_config(), &transport)
        .warm_up(Duration::from_secs(5))
        .expect("warm-up");
    assert_eq!(report.prompt_tokens, Some(77));
    let sent = &transport.requests()[0];
    assert_eq!(sent.headers.len(), 2);
    let body: Value = serde_json::from_slice(&sent.body).expect("body");
    assert_eq!(body["max_tokens"], 1);
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .expect("content")
            .contains(WARMUP_USER_LINE)
    );
    assert_eq!(body["provider"], json!({"data_collection": "deny"}));
}

// -- streaming ---------------------------------------------------------------

/// Endpoint, extra headers and body of one streamed request.
type SeenStream = (String, Vec<(String, String)>, Vec<u8>);

#[derive(Clone)]
struct MockStream {
    status: u16,
    body: Vec<u8>,
    seen: Arc<Mutex<Vec<SeenStream>>>,
}

impl MockStream {
    fn new(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.as_bytes().to_vec(),
            seen: Arc::default(),
        }
    }
}

impl StreamingHttpTransport for MockStream {
    fn post_json_stream(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
        max_body_bytes: u64,
    ) -> Result<HttpStreamResponse, HttpTransportError> {
        self.post_json_stream_with_headers(
            endpoint,
            bearer_token,
            &ExtraHeaders::empty(),
            body,
            timeout,
            max_body_bytes,
        )
    }

    fn post_json_stream_with_headers(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        extra_headers: &ExtraHeaders,
        body: &[u8],
        _timeout: Duration,
        _max_body_bytes: u64,
    ) -> Result<HttpStreamResponse, HttpTransportError> {
        assert_eq!(bearer_token, Some(SECRET));
        self.seen.lock().expect("seen").push((
            endpoint.to_owned(),
            extra_headers
                .iter()
                .map(|(n, v)| (n.to_owned(), v.to_owned()))
                .collect(),
            body.to_vec(),
        ));
        Ok(HttpStreamResponse {
            status: self.status,
            content_type: Some("text/event-stream".to_owned()),
            body: Box::new(Cursor::new(self.body.clone())),
        })
    }
}

#[derive(Default)]
struct Collect(String);

impl ReplyDeltaSink for Collect {
    fn push(&mut self, delta: &str) -> DeltaFlow {
        self.0.push_str(delta);
        DeltaFlow::Continue
    }
}

fn streaming(transport: &MockStream) -> OpenAiResponsesAdapter {
    adapter(
        openrouter_config(),
        &MockHttpTransport::new(Ok(status(200))),
    )
    .with_streaming_transport(Arc::new(transport.clone()), StreamLimits::default())
    .expect("streaming")
}

const CHAT_STREAM: &str = concat!(
    ": OPENROUTER PROCESSING\n\n",
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"こん\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"にちは\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":9,\"cost\":0.0001}}\n\n",
    "data: [DONE]\n\n",
);

#[test]
fn chat_stream_sends_the_chat_body_and_takes_usage_from_the_final_chunk() {
    let transport = MockStream::new(200, CHAT_STREAM);
    let adapter = streaming(&transport);
    let mut sink = Collect::default();
    let reply = adapter
        .generate_streaming(&request(), Duration::from_secs(5), &mut sink)
        .expect("streaming enabled")
        .expect("reply");
    assert_eq!(reply.text, "こんにちは");
    assert_eq!(sink.0, "こんにちは");
    assert_eq!(
        reply.usage,
        Some(ThinkingUsage {
            prompt_tokens: 50,
            completion_tokens: 9,
            cost_micro_usd: Some(100),
        })
    );

    let seen = transport.seen.lock().expect("seen");
    let (endpoint, headers, raw) = &seen[0];
    assert_eq!(endpoint, "https://openrouter.ai/api/v1/chat/completions");
    assert_eq!(headers.len(), 2);
    let body: Value = serde_json::from_slice(raw).expect("body");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["max_tokens"], 256);
    assert_eq!(body["provider"], json!({"data_collection": "deny"}));
    assert!(body.get("input").is_none());
}

#[test]
fn a_stream_without_a_usage_chunk_still_completes_with_no_usage() {
    let without = CHAT_STREAM
        .lines()
        .filter(|line| !line.contains("usage"))
        .collect::<Vec<_>>()
        .join("\n");
    let transport = MockStream::new(
        200,
        &format!(
            "{without}
"
        ),
    );
    let reply = streaming(&transport)
        .generate_streaming(&request(), Duration::from_secs(5), &mut Collect::default())
        .expect("streaming enabled")
        .expect("reply");
    assert_eq!(reply.text, "こんにちは");
    assert_eq!(reply.usage, None);
}

#[test]
fn stream_http_errors_are_typed_including_402() {
    for (code, kind) in [
        (402, EngineErrorKind::QuotaExhausted),
        (429, EngineErrorKind::RateLimited),
        (401, EngineErrorKind::Authentication),
        (502, EngineErrorKind::Unavailable),
    ] {
        let transport = MockStream::new(code, "{}");
        let error = streaming(&transport)
            .generate_streaming(&request(), Duration::from_secs(5), &mut Collect::default())
            .expect("streaming enabled")
            .expect_err("error status");
        assert_eq!(error.kind, kind, "HTTP {code}");
    }
}

#[test]
fn usage_extraction_ignores_ordinary_deltas_and_bad_json() {
    assert_eq!(
        parse_event_usage(r#"{"choices":[{"delta":{"content":"usage of words"}}]}"#),
        None
    );
    assert_eq!(parse_event_usage("not json with \"usage\" in it"), None);
    assert_eq!(parse_event_usage("[DONE]"), None);
    assert_eq!(
        parse_event_usage(r#"{"choices":[{"delta":{"content":"x"}}],"usage":{"prompt_tokens":3}}"#),
        Some(ThinkingUsage {
            prompt_tokens: 3,
            completion_tokens: 0,
            cost_micro_usd: None
        })
    );
}
