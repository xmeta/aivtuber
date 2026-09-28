use aivtuber_adapters::{OpenAiResponsesAdapter, OpenAiResponsesConfig, SecretString};
use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, PrivacyClass, ReflexContext, ReflexRequest,
    RetrievalCandidateContext, RetrievalSnapshot, SecurityPlane, SourceClass, ThinkingRequest,
    TrustLevel,
};
use aivtuber_reflex::{JevAdapter, JevAdapterConfig, JevApiKey};
use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the ignored live probe"))
}

fn event() -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: "evt-live-conformance".to_owned(),
        correlation_id: "corr-live-conformance".to_owned(),
        sequence: 1,
        observed_at: "2026-09-28T00:00:00Z".to_owned(),
        source: "conformance-probe".to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: None,
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([(
            "text".to_owned(),
            serde_json::Value::String("reply briefly to a conformance probe".to_owned()),
        )]),
    }
}

fn reflex_request() -> ReflexRequest {
    let mut request = ReflexRequest::new(event(), ReflexContext::default());
    request.retrieval = RetrievalSnapshot {
        candidates: vec![RetrievalCandidateContext {
            asset_id: "conformance.asset".to_owned(),
            rank: 1,
            similarity: 0.9,
        }],
    };
    request
}

fn thinking_request() -> ThinkingRequest {
    ThinkingRequest::from_event(
        &event(),
        "reply briefly to a conformance probe",
        PrivacyClass::Public,
        ReflexContext::default(),
        RetrievalSnapshot::default(),
        Vec::new(),
    )
}

#[test]
#[ignore = "requires explicit Jev credentials; never runs in normal PR CI"]
fn live_jev_conformance() {
    let mut config = JevAdapterConfig {
        endpoint: required_env("AIVTUBER_JEV_ENDPOINT"),
        deadline: Duration::from_secs(5),
        max_attempts: 1,
        initial_backoff: Duration::ZERO,
        ..JevAdapterConfig::default()
    };
    if let Ok(model) = env::var("AIVTUBER_JEV_MODEL")
        && !model.trim().is_empty()
    {
        config.model_alias = model;
    }
    let adapter = JevAdapter::new(
        config,
        JevApiKey::new(required_env("AIVTUBER_JEV_API_KEY")).expect("non-empty Jev key"),
    )
    .expect("valid Jev live probe config");

    let evidence = adapter
        .evaluate_evidence(&reflex_request())
        .expect("Jev live conformance response");
    assert!(!evidence.requested_model.trim().is_empty());
    assert!(!evidence.returned_model.trim().is_empty());
    assert_eq!(evidence.attempts, 1);
}

#[test]
#[ignore = "may be billable and requires explicit OpenAI-compatible credentials"]
fn live_openai_responses_conformance() {
    let mut config = OpenAiResponsesConfig {
        timeout: Duration::from_secs(5),
        max_output_tokens: 32,
        ..OpenAiResponsesConfig::default()
    };
    if let Ok(endpoint) = env::var("AIVTUBER_OPENAI_ENDPOINT")
        && !endpoint.trim().is_empty()
    {
        config.endpoint = endpoint;
    }
    if let Ok(model) = env::var("AIVTUBER_OPENAI_MODEL")
        && !model.trim().is_empty()
    {
        config.model_alias = model;
    }
    config.model_version = env::var("AIVTUBER_OPENAI_MODEL_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let adapter = OpenAiResponsesAdapter::new(
        config,
        SecretString::new(required_env("AIVTUBER_OPENAI_API_KEY")),
    )
    .expect("valid OpenAI-compatible live probe config");
    let identity = aivtuber_domain::ThinkingEngine::identity(&adapter);

    let reply = adapter
        .generate_sync(&thinking_request())
        .expect("OpenAI-compatible live conformance response");
    assert!(!reply.text.trim().is_empty());
    assert_eq!(identity.name, "openai-compatible-responses");
    assert!(
        identity
            .model_alias
            .as_deref()
            .is_some_and(|model| !model.is_empty())
    );
}
