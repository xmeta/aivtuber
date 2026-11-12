use super::warmup::{WARMUP_USER_LINE, warmup_request};
use super::*;
use crate::http::test_support::MockHttpTransport;
use crate::{HttpResponse, HttpTransportError};

fn adapter(transport: &MockHttpTransport) -> OpenAiResponsesAdapter {
    OpenAiResponsesAdapter::with_authentication_and_transport(
        OpenAiResponsesConfig {
            endpoint: "http://127.0.0.1:18080/v1/responses".to_owned(),
            timeout: Duration::from_secs(15),
            ..OpenAiResponsesConfig::default()
        },
        ThinkingAuthentication::UnauthenticatedLoopback,
        Arc::new(SystemHostResolver),
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

#[test]
fn warm_up_uses_the_real_prompt_builder_with_a_one_token_limit() {
    let transport = MockHttpTransport::new(Ok(ok(json!({
        "output": [{"content": [{"type": "output_text", "text": "SECRET-REPLY"}]}],
        "usage": {"input_tokens": 206}
    }))));
    let adapter = adapter(&transport);
    let report = adapter.warm_up(Duration::from_secs(120)).expect("warm-up");
    assert_eq!(report.prompt_tokens, Some(206));
    // The report type has no text field, so the reply cannot leak.
    assert!(!format!("{report:?}").contains("SECRET-REPLY"));

    let sent = transport.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].timeout, Duration::from_secs(120));
    assert_eq!(sent[0].bearer_token, None);
    let body: Value = serde_json::from_slice(&sent[0].body).expect("body");
    assert_eq!(body["max_output_tokens"], 1);
    let input = body["input"].as_str().expect("input");
    assert!(input.contains(WARMUP_USER_LINE));
    // Exactly what the shared builder makes of the synthetic request, so the
    // instruction prefix matches a real request's.
    assert_eq!(
        input,
        compact_generation_input(&warmup_request()).expect("input")
    );
    assert!(input.starts_with("Generate one concise AI VTuber reply"));
}

#[test]
fn warm_up_failures_are_typed_and_clear_the_in_flight_marker() {
    let timeout = MockHttpTransport::new(Err(HttpTransportError::Timeout));
    let failing = adapter(&timeout);
    let error = failing.warm_up(Duration::from_secs(1)).expect_err("fails");
    assert_eq!(error.kind, EngineErrorKind::Timeout);
    assert_eq!(failing.warmup.grace(), Duration::ZERO);

    let server_error = MockHttpTransport::new(Ok(HttpResponse {
        status: 500,
        body: Vec::new(),
        content_type: None,
    }));
    let error = adapter(&server_error)
        .warm_up(Duration::from_secs(1))
        .expect_err("fails");
    assert_eq!(error.kind, EngineErrorKind::Unavailable);

    assert!(failing.warm_up(Duration::ZERO).is_err());
}

#[test]
fn unusable_body_is_still_a_successful_warm_up() {
    let transport = MockHttpTransport::new(Ok(HttpResponse {
        status: 200,
        body: b"not json".to_vec(),
        content_type: None,
    }));
    let report = adapter(&transport)
        .warm_up(Duration::from_secs(1))
        .expect("warm-up");
    assert_eq!(report.prompt_tokens, None);
}

#[test]
fn in_flight_warm_up_extends_only_a_real_request_timeout() {
    let transport = MockHttpTransport::new(Ok(ok(json!({
        "output": [{"content": [{"type": "output_text", "text": "hi"}]}]
    }))));
    let adapter = adapter(&transport);
    let request = warmup_request();

    adapter
        .generate_sync_with_timeout(&request, Duration::from_secs(30))
        .expect("reply");
    assert_eq!(transport.requests()[0].timeout, Duration::from_secs(15));

    let in_flight = adapter.warmup.begin(Duration::from_secs(20));
    adapter
        .generate_sync_with_timeout(&request, Duration::from_secs(30))
        .expect("reply");
    assert_eq!(transport.requests()[1].timeout, Duration::from_secs(30));
    // The interaction budget still caps the total.
    adapter
        .generate_sync_with_timeout(&request, Duration::from_secs(16))
        .expect("reply");
    assert_eq!(transport.requests()[2].timeout, Duration::from_secs(16));

    drop(in_flight);
    adapter
        .generate_sync_with_timeout(&request, Duration::from_secs(30))
        .expect("reply");
    assert_eq!(transport.requests()[3].timeout, Duration::from_secs(15));
}
