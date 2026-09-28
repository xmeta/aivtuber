use aivtuber_adapters::{
    HttpResponse as AdapterHttpResponse, HttpTransportError, JsonHttpTransport,
    OpenAiResponsesAdapter, OpenAiResponsesConfig, SecretString,
};
use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EngineErrorKind, EventEnvelope, EventKind, PrivacyClass, ReflexContext,
    ReflexRequest, RetrievalCandidateContext, RetrievalSnapshot, SecurityPlane, SourceClass,
    ThinkingRequest, TrustLevel,
};
use aivtuber_reflex::{
    HttpResponse as JevHttpResponse, HttpTransport as JevHttpTransport, JevAdapter,
    JevAdapterConfig, JevApiKey, TransportError as JevTransportError,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceFixture {
    pub schema_version: String,
    pub fixture_version: String,
    pub provider_id: String,
    pub cases: Vec<ConformanceCase>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceCase {
    pub id: String,
    pub transport: TransportFixture,
    pub expected: ExpectedOutcome,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportFixture {
    pub status: Option<u16>,
    pub body: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedOutcome {
    pub kind: String,
    pub error_kind: Option<String>,
    pub returned_model: Option<String>,
    pub selected_candidate_id: Option<String>,
    pub output_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedOutcome {
    Success {
        returned_model: Option<String>,
        selected_candidate_id: Option<String>,
        output_text: Option<String>,
    },
    Error {
        error_kind: String,
    },
}
pub fn parse_fixture(bytes: &[u8]) -> Result<ConformanceFixture, String> {
    let fixture: ConformanceFixture =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if fixture.schema_version != "1" {
        return Err(format!(
            "unsupported conformance schema_version {:?}",
            fixture.schema_version
        ));
    }
    if fixture.fixture_version.trim().is_empty()
        || fixture.provider_id.trim().is_empty()
        || fixture.cases.is_empty()
    {
        return Err("conformance fixture metadata/cases must be non-empty".to_owned());
    }
    Ok(fixture)
}

pub fn verify_fixture<F>(fixture: &ConformanceFixture, mut runner: F) -> Result<(), String>
where
    F: FnMut(&ConformanceCase) -> ObservedOutcome,
{
    let mut failures = Vec::new();
    for case in &fixture.cases {
        let observed = runner(case);
        if let Err(error) = verify_case(case, &observed) {
            failures.push(format!("{}: {error}", case.id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}
fn verify_case(case: &ConformanceCase, observed: &ObservedOutcome) -> Result<(), String> {
    match (case.expected.kind.as_str(), observed) {
        (
            "success",
            ObservedOutcome::Success {
                returned_model,
                selected_candidate_id,
                output_text,
            },
        ) => {
            compare_optional(
                "returned_model",
                &case.expected.returned_model,
                returned_model,
            )?;
            compare_optional(
                "selected_candidate_id",
                &case.expected.selected_candidate_id,
                selected_candidate_id,
            )?;
            compare_optional("output_text", &case.expected.output_text, output_text)
        }
        ("error", ObservedOutcome::Error { error_kind }) => {
            let expected = case
                .expected
                .error_kind
                .as_deref()
                .ok_or_else(|| "error case is missing expected.error_kind".to_owned())?;
            if expected == error_kind {
                Ok(())
            } else {
                Err(format!(
                    "expected error {expected:?}, observed {error_kind:?}"
                ))
            }
        }
        (expected, observed) => Err(format!(
            "expected outcome kind {expected:?}, observed {observed:?}"
        )),
    }
}

fn compare_optional(
    field: &str,
    expected: &Option<String>,
    observed: &Option<String>,
) -> Result<(), String> {
    if expected
        .as_ref()
        .is_none_or(|expected| Some(expected) == observed.as_ref())
    {
        Ok(())
    } else {
        Err(format!(
            "{field}: expected {expected:?}, observed {observed:?}"
        ))
    }
}
#[derive(Clone)]
struct FixtureJevTransport {
    response: Result<JevHttpResponse, JevTransportError>,
}

impl JevHttpTransport for FixtureJevTransport {
    fn post_json(
        &self,
        _endpoint: &str,
        _bearer_token: &str,
        _body: &[u8],
        _timeout: Duration,
    ) -> Result<JevHttpResponse, JevTransportError> {
        self.response.clone()
    }
}

#[derive(Clone)]
struct FixtureAdapterTransport {
    response: Result<AdapterHttpResponse, HttpTransportError>,
}

impl JsonHttpTransport for FixtureAdapterTransport {
    fn post_json(
        &self,
        _endpoint: &str,
        _bearer_token: Option<&str>,
        _body: &[u8],
        _timeout: Duration,
    ) -> Result<AdapterHttpResponse, HttpTransportError> {
        self.response.clone()
    }
}
pub fn run_jev_case(case: &ConformanceCase) -> ObservedOutcome {
    let transport = Arc::new(FixtureJevTransport {
        response: jev_transport_result(&case.transport),
    });
    let config = JevAdapterConfig {
        max_attempts: 1,
        initial_backoff: Duration::ZERO,
        deadline: Duration::from_millis(250),
        ..JevAdapterConfig::default()
    };
    let adapter = JevAdapter::with_transport(
        config,
        JevApiKey::new("fixture-secret").expect("fixture key"),
        transport,
    )
    .expect("fixture Jev adapter");

    match adapter.evaluate_evidence(&representative_reflex_request()) {
        Ok(evidence) => ObservedOutcome::Success {
            returned_model: Some(evidence.returned_model),
            selected_candidate_id: evidence.selected_candidate_id,
            output_text: None,
        },
        Err(error) => ObservedOutcome::Error {
            error_kind: error_kind_name(error.kind).to_owned(),
        },
    }
}

pub fn run_openai_case(case: &ConformanceCase) -> ObservedOutcome {
    let transport = Arc::new(FixtureAdapterTransport {
        response: adapter_transport_result(&case.transport),
    });
    let config = OpenAiResponsesConfig {
        endpoint: "https://fixture.invalid/v1/responses".to_owned(),
        model_alias: "fixture-model".to_owned(),
        model_version: Some("fixture-version".to_owned()),
        max_output_tokens: 64,
        timeout: Duration::from_millis(250),
    };
    let adapter = OpenAiResponsesAdapter::with_transport(
        config,
        SecretString::new("fixture-secret"),
        transport,
    )
    .expect("fixture OpenAI adapter");

    match adapter.generate_sync(&representative_thinking_request()) {
        Ok(reply) => ObservedOutcome::Success {
            returned_model: None,
            selected_candidate_id: None,
            output_text: Some(reply.text),
        },
        Err(error) => ObservedOutcome::Error {
            error_kind: error_kind_name(error.kind).to_owned(),
        },
    }
}

fn jev_transport_result(fixture: &TransportFixture) -> Result<JevHttpResponse, JevTransportError> {
    if fixture.error.as_deref() == Some("timeout") {
        return Err(JevTransportError::Timeout);
    }
    if let Some(error) = &fixture.error {
        return Err(JevTransportError::Unavailable(error.clone()));
    }
    Ok(JevHttpResponse {
        status: fixture.status.unwrap_or(200),
        body: fixture_body(fixture),
    })
}
fn adapter_transport_result(
    fixture: &TransportFixture,
) -> Result<AdapterHttpResponse, HttpTransportError> {
    if fixture.error.as_deref() == Some("timeout") {
        return Err(HttpTransportError::Timeout);
    }
    if let Some(error) = &fixture.error {
        return Err(HttpTransportError::Unavailable(error.clone()));
    }
    Ok(AdapterHttpResponse {
        status: fixture.status.unwrap_or(200),
        body: fixture_body(fixture),
        content_type: Some("application/json".to_owned()),
    })
}

fn fixture_body(fixture: &TransportFixture) -> Vec<u8> {
    serde_json::to_vec(fixture.body.as_ref().unwrap_or(&Value::Null))
        .expect("fixture JSON serialization")
}

fn error_kind_name(kind: EngineErrorKind) -> &'static str {
    match kind {
        EngineErrorKind::Timeout => "timeout",
        EngineErrorKind::Unavailable => "unavailable",
        EngineErrorKind::RateLimited => "rate_limited",
        EngineErrorKind::Overloaded => "overloaded",
        EngineErrorKind::Authentication => "authentication",
        EngineErrorKind::Unauthorized => "unauthorized",
        EngineErrorKind::InvalidRequest => "invalid_request",
        EngineErrorKind::Backend => "backend",
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityMatrix {
    pub schema_version: String,
    pub matrix_version: String,
    pub providers: Vec<CompatibilityEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityEntry {
    pub provider_id: String,
    pub system: String,
    pub api_protocol_version: String,
    pub adapter: String,
    pub fixture_version: String,
    pub verification_status: String,
    pub required_capabilities: Vec<String>,
    pub optional_capabilities: Vec<String>,
    pub last_live_verified: Option<String>,
    pub known_deviations: Vec<String>,
    pub assumption_ids: Vec<String>,
}

pub fn verify_compatibility_matrix(
    bytes: &[u8],
    fixtures: &[&ConformanceFixture],
) -> Result<CompatibilityMatrix, String> {
    let matrix: CompatibilityMatrix =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if matrix.schema_version != "1" || matrix.matrix_version.trim().is_empty() {
        return Err("compatibility matrix schema/version is invalid".to_owned());
    }
    let fixture_by_provider = fixtures
        .iter()
        .map(|fixture| {
            (
                fixture.provider_id.as_str(),
                fixture.fixture_version.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut provider_ids = BTreeSet::new();
    for entry in &matrix.providers {
        if !provider_ids.insert(entry.provider_id.as_str()) {
            return Err(format!("duplicate provider_id {:?}", entry.provider_id));
        }
        if entry.system.trim().is_empty()
            || entry.api_protocol_version.trim().is_empty()
            || entry.adapter.trim().is_empty()
            || entry.required_capabilities.is_empty()
            || entry.assumption_ids.is_empty()
        {
            return Err(format!(
                "incomplete compatibility entry {:?}",
                entry.provider_id
            ));
        }
        match entry.verification_status.as_str() {
            "fixture_verified_live_unverified" => {
                if entry.last_live_verified.is_some() {
                    return Err(format!(
                        "{} is live-unverified but has last_live_verified",
                        entry.provider_id
                    ));
                }
            }
            "live_verified" => {
                if entry
                    .last_live_verified
                    .as_deref()
                    .is_none_or(str::is_empty)
                {
                    return Err(format!(
                        "{} is live_verified without last_live_verified",
                        entry.provider_id
                    ));
                }
            }
            "unsupported" => {}
            other => {
                return Err(format!(
                    "{} has unknown verification_status {other:?}",
                    entry.provider_id
                ));
            }
        }
        let expected_fixture = fixture_by_provider
            .get(entry.provider_id.as_str())
            .ok_or_else(|| format!("matrix provider {:?} has no fixture", entry.provider_id))?;
        if *expected_fixture != entry.fixture_version {
            return Err(format!(
                "{} matrix fixture_version {:?} does not match fixture {:?}",
                entry.provider_id, entry.fixture_version, expected_fixture
            ));
        }
    }
    if provider_ids.len() != fixtures.len() {
        return Err("fixture/provider matrix coverage is incomplete".to_owned());
    }
    Ok(matrix)
}

fn representative_event() -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: "evt-conformance".to_owned(),
        correlation_id: "corr-conformance".to_owned(),
        sequence: 1,
        observed_at: "2026-09-28T00:00:00Z".to_owned(),
        source: "fixture-chat".to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: Some("fixture-viewer".to_owned()),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([(
            "text".to_owned(),
            Value::String("hello from conformance fixture".to_owned()),
        )]),
    }
}

fn representative_reflex_request() -> ReflexRequest {
    let mut request = ReflexRequest::new(representative_event(), ReflexContext::default());
    request.retrieval = RetrievalSnapshot {
        candidates: vec![
            RetrievalCandidateContext {
                asset_id: "asset.a".to_owned(),
                rank: 1,
                similarity: 0.98,
            },
            RetrievalCandidateContext {
                asset_id: "asset.b".to_owned(),
                rank: 2,
                similarity: 0.82,
            },
        ],
    };
    request
}
fn representative_thinking_request() -> ThinkingRequest {
    ThinkingRequest::from_event(
        &representative_event(),
        "hello from conformance fixture",
        PrivacyClass::Public,
        ReflexContext::default(),
        RetrievalSnapshot::default(),
        Vec::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jev_fixture_matches_adapter_contract() {
        let fixture = parse_fixture(include_bytes!(
            "../../../examples/conformance/jev-system-one.json"
        ))
        .expect("Jev fixture");
        assert_eq!(fixture.provider_id, "jev-system-one");
        verify_fixture(&fixture, run_jev_case).expect("Jev conformance");
    }

    #[test]
    fn openai_fixture_matches_adapter_contract() {
        let fixture = parse_fixture(include_bytes!(
            "../../../examples/conformance/openai-responses.json"
        ))
        .expect("OpenAI fixture");
        assert_eq!(fixture.provider_id, "openai-responses");
        verify_fixture(&fixture, run_openai_case).expect("OpenAI conformance");
    }

    #[test]
    fn compatibility_matrix_matches_versioned_fixtures_without_live_overclaim() {
        let jev = parse_fixture(include_bytes!(
            "../../../examples/conformance/jev-system-one.json"
        ))
        .expect("Jev fixture");
        let openai = parse_fixture(include_bytes!(
            "../../../examples/conformance/openai-responses.json"
        ))
        .expect("OpenAI fixture");
        let matrix = verify_compatibility_matrix(
            include_bytes!("../../../examples/conformance/compatibility-matrix.json"),
            &[&jev, &openai],
        )
        .expect("compatibility matrix");
        assert_eq!(matrix.providers.len(), 2);
        assert!(matrix.providers.iter().all(|provider| {
            provider.verification_status == "fixture_verified_live_unverified"
                && provider.last_live_verified.is_none()
        }));
    }
}
