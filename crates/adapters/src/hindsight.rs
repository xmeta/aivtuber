//! The Hindsight memory sidecar, behind the project's own port (issue #105).
//!
//! Hindsight is an external memory service: it keeps claims, answers a scoped
//! recall over them, and can retire a claim the project no longer wants to be
//! recalled. This adapter is the *only* place that knows its wire format; the
//! project talks to it through [`MemorySidecar`], the provider-neutral port in
//! `aivtuber-domain`.
//!
//! Three properties matter more than the wire format itself:
//!
//! * **The project stays the authority.** The service is told what to keep
//!   and what to retire; it is never asked to decide. Nothing here writes
//!   durable memory on its own: the caller has already passed the project's
//!   write gate before a claim reaches this adapter.
//! * **A scope is a namespace, not a filter.** Each scope is one bank on the
//!   service, so one measurement cannot see another measurement's claims.
//!   Opening a scope clears it first, so a repeated run measures the same
//!   input rather than the input plus the previous run.
//! * **Private claim text never enters an error message.** A failure answer
//!   from the service may echo the request, and the request holds the claim,
//!   so failures carry the status alone.

use aivtuber_domain::{
    EngineError, EngineErrorKind, MemorySidecar, SidecarClaim, SidecarHandle, SidecarRecallItem,
    SidecarRecallQuery,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

use crate::SecretString;
use crate::http::{HttpTransportError, JsonHttpRequestTransport, UreqJsonTransport};
use crate::time::iso8601_utc;

/// The API root a Hindsight deployment exposes.
const HINDSIGHT_API_ROOT: [&str; 2] = ["v1", "default"];

/// Tag prefix that carries the actor scope of a claim.
const ACTOR_TAG_PREFIX: &str = "actor:";

/// Fact types the adapter asks for: the raw facts a claim was retained as.
///
/// A consolidated observation is a model-written summary of several facts, so
/// it is not the claim the project admitted and is not requested. Asking for
/// it would measure the service's prose, not its retrieval.
const RECALLED_FACT_TYPES: [&str; 2] = ["world", "experience"];

/// Token budget for one recall.
///
/// The project's own item and byte bounds decide what reaches generation;
/// this only keeps the service's answer finite.
const RECALL_MAX_TOKENS: u64 = 4096;

/// Items read back when locating a retained claim's handle.
const LIST_LIMIT: u64 = 16;

/// Where a Hindsight deployment lives and how it is reached.
#[derive(Clone, PartialEq, Eq)]
pub struct HindsightConfig {
    /// The service base URL, for example `http://localhost:8888`.
    pub endpoint: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Optional bearer credential for a hosted deployment. It is never
    /// printed: [`SecretString`] redacts it.
    pub bearer_token: Option<SecretString>,
}

impl HindsightConfig {
    /// A configuration for `endpoint` with the default timeout.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout: Duration::from_secs(30),
            bearer_token: None,
        }
    }

    /// Set the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the bearer credential.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(SecretString::new(token));
        self
    }

    /// Refuse a configuration that could not address a service.
    pub fn validate(&self) -> Result<(), EngineError> {
        let url = url::Url::parse(&self.endpoint).map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("hindsight endpoint is not a URL: {error}"),
            )
        })?;
        if !url.has_host() {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "hindsight endpoint has no host",
            ));
        }
        if !matches!(url.scheme(), "http" | "https") {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!(
                    "hindsight endpoint scheme {:?} is not http or https",
                    url.scheme()
                ),
            ));
        }
        Ok(())
    }
}

/// A Hindsight deployment behind the project's memory-sidecar port.
pub struct HindsightSidecar {
    config: HindsightConfig,
    transport: Arc<dyn JsonHttpRequestTransport>,
}

impl HindsightSidecar {
    /// Reach a Hindsight deployment over the network.
    pub fn new(config: HindsightConfig) -> Self {
        Self::with_transport(config, Arc::new(UreqJsonTransport::default()))
    }

    /// Reach a Hindsight deployment through a given transport.
    pub fn with_transport(
        config: HindsightConfig,
        transport: Arc<dyn JsonHttpRequestTransport>,
    ) -> Self {
        Self { config, transport }
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &HindsightConfig {
        &self.config
    }

    /// The URL of one path below the configured endpoint.
    fn url_for(&self, segments: &[&str]) -> Result<String, EngineError> {
        let mut url = url::Url::parse(&self.config.endpoint).map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("hindsight endpoint is not a URL: {error}"),
            )
        })?;
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    "hindsight endpoint cannot carry a path",
                )
            })?;
            path.pop_if_empty();
            path.extend(segments);
        }
        Ok(url.to_string())
    }

    /// The URL of one path below a scope's bank.
    fn scope_url(&self, scope: &str, rest: &[&str]) -> Result<String, EngineError> {
        let mut segments = HINDSIGHT_API_ROOT.to_vec();
        segments.extend_from_slice(&["banks", scope]);
        segments.extend_from_slice(rest);
        self.url_for(&segments)
    }

    /// Issue one JSON request and return the parsed answer.
    fn send(&self, method: &str, url: &str, body: Option<&Value>) -> Result<Value, EngineError> {
        self.send_allowing(method, url, body, &[])
    }

    /// Issue one JSON request, treating the listed statuses as an answer
    /// rather than a failure.
    ///
    /// Only a status the caller names is tolerated, and it is tolerated as
    /// "nothing to report": every other failure still stops the caller, so a
    /// request the project depends on is never continued past an unknown
    /// outcome.
    fn send_allowing(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
        allowed: &[u16],
    ) -> Result<Value, EngineError> {
        let payload = body
            .map(|value| {
                serde_json::to_vec(value).map_err(|error| {
                    EngineError::new(
                        EngineErrorKind::InvalidRequest,
                        format!("hindsight request is not serializable: {error}"),
                    )
                })
            })
            .transpose()?;
        let response = self
            .transport
            .request_json(
                method,
                url,
                self.config
                    .bearer_token
                    .as_ref()
                    .map(|token| token.expose()),
                payload.as_deref().unwrap_or(&[]),
                self.config.timeout,
            )
            .map_err(transport_error)?;
        if !(200..300).contains(&response.status) {
            if allowed.contains(&response.status) {
                return Ok(Value::Null);
            }
            // The body of a failure may echo the request, and the request
            // holds the claim, so the error carries the status alone.
            return Err(EngineError::new(
                EngineErrorKind::Unavailable,
                format!("hindsight {method} {url} answered {}", response.status),
            ));
        }
        if response.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&response.body).map_err(|error| {
            EngineError::new(
                EngineErrorKind::Backend,
                format!("hindsight {method} answered with a body that is not JSON: {error}"),
            )
        })
    }

    /// Read the items a scope holds for one document id.
    fn list_for_document(&self, scope: &str, document_id: &str) -> Result<Value, EngineError> {
        let base = self.scope_url(scope, &["memories", "list"])?;
        let mut url = url::Url::parse(&base).map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("hindsight list URL is not a URL: {error}"),
            )
        })?;
        url.query_pairs_mut()
            .append_pair("document_id", document_id)
            .append_pair("limit", &LIST_LIMIT.to_string());
        self.send("GET", url.as_str(), None)
    }
}

impl MemorySidecar for HindsightSidecar {
    /// Open an isolated scope, empty.
    ///
    /// A scope left over from an earlier run would make this run measure two
    /// inputs at once, so opening clears the scope first. Only the scope the
    /// caller names is touched.
    fn open_scope(&self, scope: &str) -> Result<(), EngineError> {
        self.config.validate()?;
        let url = self.scope_url(scope, &[])?;
        // A scope that does not exist yet answers 404 to the clear, and that
        // is the only failure the open can continue past: the goal is an
        // empty scope, and it is about to be created. A 403, a 500, or a
        // timeout means the scope may still hold an earlier run's claims, so
        // the open stops rather than measuring that run's memory as this
        // run's answer.
        self.send_allowing("DELETE", &url, None, &[404])?;
        self.send("PUT", &url, Some(&json!({ "name": scope })))?;
        Ok(())
    }

    /// Retain a claim and return the handle that names it.
    ///
    /// The service does not answer a retain with the id of what it stored, so
    /// the handle is read back from the item list by the claim's document id.
    fn retain(&self, scope: &str, claim: &SidecarClaim) -> Result<SidecarHandle, EngineError> {
        claim.validate()?;
        let mut item = Map::new();
        item.insert("content".to_owned(), json!(claim.text));
        // The service sorts and filters by event time, so the claim's event
        // time travels with it: a claim that was recorded late but happened
        // earlier keeps its place in the chronology.
        item.insert(
            "timestamp".to_owned(),
            json!(iso8601_utc(claim.event_at_ms)),
        );
        item.insert("context".to_owned(), json!(claim.topic));
        item.insert("document_id".to_owned(), json!(claim.claim_id));
        item.insert("metadata".to_owned(), Value::Object(metadata_for(claim)));
        let tags = tags_for(claim);
        if !tags.is_empty() {
            item.insert("tags".to_owned(), json!(tags));
        }
        let body = json!({ "items": [Value::Object(item)], "async": false });
        self.send("POST", &self.scope_url(scope, &["memories"])?, Some(&body))?;

        let listed = self.list_for_document(scope, &claim.claim_id)?;
        let items = listed
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                EngineError::new(
                    EngineErrorKind::Backend,
                    "hindsight item list answered without an items array",
                )
            })?;
        let id = items
            .iter()
            .find(|entry| {
                entry.get("document_id").and_then(Value::as_str) == Some(claim.claim_id.as_str())
            })
            .and_then(|entry| entry.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                EngineError::new(
                    EngineErrorKind::Backend,
                    format!("hindsight did not keep the claim {:?}", claim.claim_id),
                )
            })?;
        Ok(SidecarHandle::new(id))
    }

    /// Answer a scoped recall.
    ///
    /// The actor scope is a tag filter, and the tag match is `any`: a claim
    /// that carries no actor scope is shared with every viewer, which is what
    /// the runtime does with an unscoped claim.
    fn recall(
        &self,
        scope: &str,
        query: &SidecarRecallQuery,
    ) -> Result<Vec<SidecarRecallItem>, EngineError> {
        query.validate()?;
        let mut body = Map::new();
        body.insert("query".to_owned(), json!(query.query));
        body.insert("types".to_owned(), json!(RECALLED_FACT_TYPES));
        body.insert("max_tokens".to_owned(), json!(RECALL_MAX_TOKENS));
        body.insert(
            "query_timestamp".to_owned(),
            json!(iso8601_utc(query.now_ms)),
        );
        if let Some(actor) = &query.actor {
            body.insert(
                "tags".to_owned(),
                json!([format!("{ACTOR_TAG_PREFIX}{actor}")]),
            );
            body.insert("tags_match".to_owned(), json!("any"));
        }
        let response = self.send(
            "POST",
            &self.scope_url(scope, &["memories", "recall"])?,
            Some(&Value::Object(body)),
        )?;
        let results = response
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                EngineError::new(
                    EngineErrorKind::Backend,
                    "hindsight recall answered without a results array",
                )
            })?;
        Ok(narrow_to_scope(
            results.iter().map(item_from_result).collect(),
            query,
        ))
    }

    /// Retire a retained claim so it stops being recalled.
    ///
    /// The service keeps the claim for its own audit; the project only
    /// requires that it is no longer recalled, which is what the state change
    /// does.
    fn retire(&self, scope: &str, handle: &SidecarHandle) -> Result<(), EngineError> {
        let body = json!({
            "state": "invalidated",
            "reason": "retired under the project's retention policy",
        });
        self.send(
            "PATCH",
            &self.scope_url(scope, &["memories", handle.as_str()])?,
            Some(&body),
        )?;
        Ok(())
    }

    /// Drop the scope and everything in it.
    fn close_scope(&self, scope: &str) -> Result<(), EngineError> {
        self.send("DELETE", &self.scope_url(scope, &[])?, None)?;
        Ok(())
    }
}

/// The metadata a claim travels with.
///
/// The claim id is the only key the project depends on: it is what a recall
/// is mapped back to. The rest is provenance the project already knows, kept
/// so the service's own answer can be checked against it.
fn metadata_for(claim: &SidecarClaim) -> Map<String, Value> {
    let mut metadata = Map::new();
    metadata.insert("claim_id".to_owned(), json!(claim.claim_id));
    if let Some(actor) = &claim.actor {
        metadata.insert("actor".to_owned(), json!(actor));
    }
    if let Some(topic) = &claim.topic {
        metadata.insert("topic".to_owned(), json!(topic));
    }
    metadata.insert(
        "recorded_at_ms".to_owned(),
        json!(claim.recorded_at_ms.to_string()),
    );
    metadata.insert(
        "event_at_ms".to_owned(),
        json!(claim.event_at_ms.to_string()),
    );
    metadata
}

/// The tags a claim travels with. An unscoped claim carries none, which is
/// how it stays shared.
fn tags_for(claim: &SidecarClaim) -> Vec<String> {
    claim
        .actor
        .as_ref()
        .map(|actor| vec![format!("{ACTOR_TAG_PREFIX}{actor}")])
        .unwrap_or_default()
}

/// One recalled item, mapped back to the project claim it carries.
fn item_from_result(value: &Value) -> SidecarRecallItem {
    let metadata = value.get("metadata").and_then(Value::as_object);
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let claim_id = metadata
        .and_then(|metadata| metadata.get("claim_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let actor = metadata
        .and_then(|metadata| metadata.get("actor"))
        .and_then(Value::as_str)
        .filter(|actor| !actor.is_empty())
        .map(str::to_owned);
    let topic = metadata
        .and_then(|metadata| metadata.get("topic"))
        .and_then(Value::as_str)
        .filter(|topic| !topic.is_empty())
        .map(str::to_owned);
    let event_at_ms = metadata
        .and_then(|metadata| metadata.get("event_at_ms"))
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok());
    SidecarRecallItem {
        claim_id,
        text,
        actor,
        topic,
        event_at_ms,
    }
}

/// Narrow the service's answer to what the query asked for.
///
/// The tag filter the adapter sends is a hint to the service, not a guarantee:
/// a match of `any` also answers unscoped claims, and a service that ignores
/// the filter answers everything. The port says a recall is restricted to the
/// scope it named — `actor: None` asks for the scope that carries no actor —
/// and that `limit` is how many items the caller will use, so both are
/// enforced here, on what the service actually answered.
fn narrow_to_scope(
    items: Vec<SidecarRecallItem>,
    query: &SidecarRecallQuery,
) -> Vec<SidecarRecallItem> {
    let mut answered: Vec<SidecarRecallItem> = items
        .into_iter()
        .filter(|item| match (&query.actor, &item.actor) {
            // An unscoped claim is shared with every viewer: that is how the
            // project files a claim that carries no actor.
            (Some(_), None) => true,
            (Some(asked), Some(seen)) => asked == seen,
            (None, None) => true,
            // A recall with no actor is the scope that carries no actor, not
            // every scope the deployment happens to hold.
            (None, Some(_)) => false,
        })
        .collect();
    answered.truncate(query.limit);
    answered
}

fn transport_error(error: HttpTransportError) -> EngineError {
    match error {
        HttpTransportError::Timeout => {
            EngineError::new(EngineErrorKind::Timeout, "hindsight request timed out")
        }
        HttpTransportError::Unavailable(message) => EngineError::new(
            EngineErrorKind::Unavailable,
            format!("hindsight request failed: {message}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Captured {
        method: String,
        url: String,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    struct MockTransport {
        responses: Arc<Mutex<VecDeque<Result<HttpResponse, HttpTransportError>>>>,
        requests: Arc<Mutex<Vec<Captured>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<HttpResponse, HttpTransportError>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<Captured> {
            self.requests.lock().expect("request lock").clone()
        }
    }

    impl JsonHttpRequestTransport for MockTransport {
        fn request_json(
            &self,
            method: &str,
            endpoint: &str,
            _bearer_token: Option<&str>,
            body: &[u8],
            _timeout: Duration,
        ) -> Result<HttpResponse, HttpTransportError> {
            self.requests.lock().expect("request lock").push(Captured {
                method: method.to_owned(),
                url: endpoint.to_owned(),
                body: body.to_vec(),
            });
            self.responses
                .lock()
                .expect("response lock")
                .pop_front()
                .unwrap_or_else(|| {
                    Err(HttpTransportError::Unavailable(
                        "no queued response".to_owned(),
                    ))
                })
        }
    }

    fn ok(body: Value) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: 200,
            body: serde_json::to_vec(&body).expect("serialize response"),
            content_type: Some("application/json".to_owned()),
        })
    }

    fn sidecar(
        responses: Vec<Result<HttpResponse, HttpTransportError>>,
    ) -> (HindsightSidecar, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(responses));
        let sidecar = HindsightSidecar::with_transport(
            HindsightConfig::new("http://localhost:8888"),
            transport.clone(),
        );
        (sidecar, transport)
    }

    fn claim() -> SidecarClaim {
        SidecarClaim {
            claim_id: "evt-enjoy".to_owned(),
            actor: Some("actor-key".to_owned()),
            topic: Some("game-x".to_owned()),
            text: "viewer enjoyed the game x ending".to_owned(),
            recorded_at_ms: 2000,
            event_at_ms: 2000,
        }
    }

    #[test]
    fn retain_writes_the_claim_and_reads_back_its_handle() {
        let (sidecar, transport) = sidecar(vec![
            ok(json!({"success": true, "items_count": 1})),
            ok(json!({
                "items": [{
                    "id": "mem-1",
                    "document_id": "evt-enjoy",
                    "text": "viewer enjoyed the game x ending"
                }],
                "total": 1
            })),
        ]);
        let handle = sidecar.retain("scope-1", &claim()).expect("retain");
        assert_eq!(handle.as_str(), "mem-1");

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert!(
            requests[0]
                .url
                .ends_with("/v1/default/banks/scope-1/memories")
        );
        let body: Value = serde_json::from_slice(&requests[0].body).expect("request body");
        let item = &body["items"][0];
        assert_eq!(item["content"], "viewer enjoyed the game x ending");
        assert_eq!(item["document_id"], "evt-enjoy");
        assert_eq!(item["timestamp"], "1970-01-01T00:00:02Z");
        assert_eq!(item["context"], "game-x");
        assert_eq!(item["metadata"]["claim_id"], "evt-enjoy");
        assert_eq!(item["metadata"]["actor"], "actor-key");
        assert_eq!(item["metadata"]["event_at_ms"], "2000");
        assert_eq!(item["tags"][0], "actor:actor-key");

        assert_eq!(requests[1].method, "GET");
        assert!(requests[1].url.contains("document_id=evt-enjoy"));
    }

    #[test]
    fn retain_without_an_actor_carries_no_tag() {
        let (sidecar, transport) = sidecar(vec![
            ok(json!({"success": true})),
            ok(json!({"items": [{"id": "mem-2", "document_id": "evt-enjoy"}]})),
        ]);
        let mut unscoped = claim();
        unscoped.actor = None;
        sidecar.retain("scope-1", &unscoped).expect("retain");
        let body: Value =
            serde_json::from_slice(&transport.requests()[0].body).expect("request body");
        assert!(body["items"][0].get("tags").is_none());
        assert!(body["items"][0]["metadata"].get("actor").is_none());
    }

    #[test]
    fn recall_scopes_by_actor_tag_and_maps_metadata_back() {
        let (sidecar, transport) = sidecar(vec![ok(json!({
            "results": [
                {
                    "id": "mem-1",
                    "text": "viewer enjoyed the game x ending",
                    "metadata": {
                        "claim_id": "evt-enjoy",
                        "actor": "actor-key",
                        "topic": "game-x",
                        "event_at_ms": "2000"
                    }
                },
                {
                    "id": "mem-2",
                    "text": "a fact the project never admitted",
                    "metadata": {}
                }
            ]
        }))]);
        let items = sidecar
            .recall(
                "scope-1",
                &SidecarRecallQuery {
                    actor: Some("actor-key".to_owned()),
                    query: "game x".to_owned(),
                    limit: 12,
                    now_ms: 3000,
                },
            )
            .expect("recall");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].claim_id, "evt-enjoy");
        assert_eq!(items[0].actor.as_deref(), Some("actor-key"));
        assert_eq!(items[0].topic.as_deref(), Some("game-x"));
        assert_eq!(items[0].event_at_ms, Some(2000));
        // An item the project never admitted carries no claim id: the
        // project can see that it is not one of its own claims.
        assert!(items[1].claim_id.is_empty());

        let request = &transport.requests()[0];
        assert_eq!(request.method, "POST");
        assert!(
            request
                .url
                .ends_with("/v1/default/banks/scope-1/memories/recall")
        );
        let body: Value = serde_json::from_slice(&request.body).expect("request body");
        assert_eq!(body["query"], "game x");
        assert_eq!(body["tags"][0], "actor:actor-key");
        assert_eq!(body["tags_match"], "any");
        assert_eq!(body["query_timestamp"], "1970-01-01T00:00:03Z");
        assert_eq!(body["types"][0], "world");
    }

    #[test]
    fn recall_without_an_actor_places_no_tag_filter() {
        let (sidecar, transport) = sidecar(vec![ok(json!({"results": []}))]);
        sidecar
            .recall(
                "scope-1",
                &SidecarRecallQuery {
                    actor: None,
                    query: "game x".to_owned(),
                    limit: 12,
                    now_ms: 3000,
                },
            )
            .expect("recall");
        let body: Value =
            serde_json::from_slice(&transport.requests()[0].body).expect("request body");
        assert!(body.get("tags").is_none());
    }

    #[test]
    fn retire_invalidates_the_named_memory() {
        let (sidecar, transport) = sidecar(vec![ok(json!({"state": "invalidated"}))]);
        sidecar
            .retire("scope-1", &SidecarHandle::new("mem-1"))
            .expect("retire");
        let request = &transport.requests()[0];
        assert_eq!(request.method, "PATCH");
        assert!(
            request
                .url
                .ends_with("/v1/default/banks/scope-1/memories/mem-1")
        );
        let body: Value = serde_json::from_slice(&request.body).expect("request body");
        assert_eq!(body["state"], "invalidated");
    }

    #[test]
    fn open_scope_clears_the_scope_before_it_is_created() {
        let (sidecar, transport) = sidecar(vec![
            Ok(HttpResponse {
                status: 404,
                body: Vec::new(),
                content_type: None,
            }),
            ok(json!({"bank_id": "scope-1"})),
        ]);
        sidecar.open_scope("scope-1").expect("open scope");
        let requests = transport.requests();
        assert_eq!(requests[0].method, "DELETE");
        assert_eq!(requests[1].method, "PUT");
        assert!(requests[1].url.ends_with("/v1/default/banks/scope-1"));
    }

    #[test]
    fn a_failed_answer_fails_closed_without_echoing_the_claim() {
        let (sidecar, _transport) = sidecar(vec![Ok(HttpResponse {
            status: 500,
            body: b"viewer enjoyed the game x ending".to_vec(),
            content_type: None,
        })]);
        let error = sidecar
            .retain("scope-1", &claim())
            .expect_err("a failed retain is an error");
        assert_eq!(error.kind, EngineErrorKind::Unavailable);
        assert!(
            !error.message.contains("viewer enjoyed"),
            "the failure must not carry the claim: {error}"
        );
    }

    #[test]
    fn a_claim_the_service_dropped_is_an_error() {
        let (sidecar, _transport) =
            sidecar(vec![ok(json!({"success": true})), ok(json!({"items": []}))]);
        let error = sidecar
            .retain("scope-1", &claim())
            .expect_err("a dropped claim is an error");
        assert!(error.message.contains("evt-enjoy"), "{error}");
    }

    #[test]
    fn open_scope_stops_when_the_scope_cannot_be_cleared() {
        for status in [403_u16, 500] {
            let (sidecar, transport) = sidecar(vec![Ok(HttpResponse {
                status,
                body: Vec::new(),
                content_type: None,
            })]);
            let error = sidecar
                .open_scope("scope-1")
                .expect_err("a scope that cannot be cleared is not opened");
            assert_eq!(error.kind, EngineErrorKind::Unavailable, "status {status}");
            let requests = transport.requests();
            assert_eq!(
                requests.len(),
                1,
                "status {status}: the open must stop at the clear, got {requests:?}"
            );
            assert_eq!(requests[0].method, "DELETE");
        }
    }

    #[test]
    fn open_scope_stops_when_the_clear_times_out() {
        let (sidecar, transport) = sidecar(vec![Err(HttpTransportError::Timeout)]);
        let error = sidecar
            .open_scope("scope-1")
            .expect_err("a timed out clear stops the open");
        assert_eq!(error.kind, EngineErrorKind::Timeout);
        assert_eq!(transport.requests().len(), 1);
    }

    #[test]
    fn recall_applies_the_callers_item_limit() {
        let (sidecar, _transport) = sidecar(vec![ok(json!({
            "results": [
                {
                    "id": "mem-1",
                    "text": "one",
                    "metadata": {"claim_id": "evt-1", "actor": "actor-key"}
                },
                {
                    "id": "mem-2",
                    "text": "two",
                    "metadata": {"claim_id": "evt-2", "actor": "actor-key"}
                }
            ]
        }))]);
        let items = sidecar
            .recall(
                "scope-1",
                &SidecarRecallQuery {
                    actor: Some("actor-key".to_owned()),
                    query: "game x".to_owned(),
                    limit: 1,
                    now_ms: 3000,
                },
            )
            .expect("recall");
        assert_eq!(items.len(), 1, "the caller's limit bounds the answer");
        assert_eq!(items[0].claim_id, "evt-1");
    }

    #[test]
    fn recall_without_an_actor_answers_only_unscoped_claims() {
        let (sidecar, _transport) = sidecar(vec![ok(json!({
            "results": [
                {
                    "id": "mem-1",
                    "text": "another view's claim",
                    "metadata": {"claim_id": "evt-other", "actor": "other-key"}
                },
                {
                    "id": "mem-2",
                    "text": "a claim shared with every viewer",
                    "metadata": {"claim_id": "evt-shared"}
                }
            ]
        }))]);
        let items = sidecar
            .recall(
                "scope-1",
                &SidecarRecallQuery {
                    actor: None,
                    query: "game x".to_owned(),
                    limit: 12,
                    now_ms: 3000,
                },
            )
            .expect("recall");
        assert_eq!(items.len(), 1, "no actor asks for the scope that has none");
        assert_eq!(items[0].claim_id, "evt-shared");
    }

    #[test]
    fn recall_drops_another_actors_claims_but_keeps_shared_ones() {
        let (sidecar, _transport) = sidecar(vec![ok(json!({
            "results": [
                {
                    "id": "mem-1",
                    "text": "the asked viewer's claim",
                    "metadata": {"claim_id": "evt-own", "actor": "actor-key"}
                },
                {
                    "id": "mem-2",
                    "text": "another view's claim",
                    "metadata": {"claim_id": "evt-other", "actor": "other-key"}
                },
                {
                    "id": "mem-3",
                    "text": "a claim shared with every viewer",
                    "metadata": {"claim_id": "evt-shared"}
                }
            ]
        }))]);
        let items = sidecar
            .recall(
                "scope-1",
                &SidecarRecallQuery {
                    actor: Some("actor-key".to_owned()),
                    query: "game x".to_owned(),
                    limit: 12,
                    now_ms: 3000,
                },
            )
            .expect("recall");
        let claim_ids: Vec<&str> = items.iter().map(|item| item.claim_id.as_str()).collect();
        assert_eq!(claim_ids, ["evt-own", "evt-shared"]);
    }

    #[test]
    fn iso8601_conversion_is_exact() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_000), "1970-01-01T00:00:01Z");
        assert_eq!(iso8601_utc(86_399_000), "1970-01-01T23:59:59Z");
        assert_eq!(iso8601_utc(86_400_000), "1970-01-02T00:00:00Z");
        // A leap day, a century boundary, and the epoch of the fixture's
        // base: the three places a hand-rolled conversion usually breaks.
        assert_eq!(iso8601_utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(1_767_225_600_000), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn a_configuration_without_a_host_is_refused() {
        let mut config = HindsightConfig::new("not a url");
        assert_eq!(
            config.validate().expect_err("a broken endpoint").kind,
            EngineErrorKind::InvalidRequest
        );
        config = HindsightConfig::new("ftp://example.test");
        assert_eq!(
            config.validate().expect_err("a wrong scheme").kind,
            EngineErrorKind::InvalidRequest
        );
        HindsightConfig::new("http://localhost:8888")
            .validate()
            .expect("a loopback endpoint");
        HindsightConfig::new("https://hindsight.example")
            .validate()
            .expect("a tls endpoint");
    }
}
