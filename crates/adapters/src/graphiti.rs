//! The Graphiti memory sidecar, behind the project's own port (issue #105).
//!
//! Graphiti is a temporal knowledge graph built from episodes: it extracts
//! entities and facts from what it is told, links them over valid time, and
//! answers a search with facts, not with the input it was handed. The port
//! asks for something narrower: retain the claim the project admitted, answer
//! a scoped recall, and retire a claim that must stop being recalled.
//!
//! So this adapter files each claim as one episode and maps recall back to
//! that episode: the text a recall returns is the episode content the project
//! itself wrote, never the service's extracted fact text. Graphiti is still
//! what ranks the answer — the adapter only resolves the facts it named back
//! to the claims that produced them — and the project still decides admission,
//! retention, and what may reach generation, as it does for Hindsight.
//!
//! Three properties matter more than the wire format:
//!
//! * **The project stays the authority.** Nothing here writes durable memory:
//!   the caller has already passed the write gate, and the adapter only
//!   relays what the project decided to retain or retire.
//! * **A materialized claim, or no answer.** `POST /messages` is asynchronous:
//!   it accepts a message and returns before extraction finishes. A claim that
//!   never appears in the episode list is a failed retain — not an empty
//!   recall — so an extraction failure cannot be scored as a retrieval miss.
//! * **Private claim text never enters an error message.** Failures carry the
//!   status (or the schema gap) alone; the claim travels only in the request.
//!
//! The reference server ships without authentication and without tenant
//! isolation, so a deployment is a loopback-only spike rather than a
//! production dependency. A bearer credential is supported only for a
//! deployment fronted by a proxy that enforces it.

use aivtuber_domain::{
    EngineError, EngineErrorKind, MAX_SIDECAR_RECALL_ITEMS, MemorySidecar, SidecarClaim,
    SidecarHandle, SidecarRecallItem, SidecarRecallQuery,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::SecretString;
use crate::http::{HttpTransportError, JsonHttpRequestTransport, UreqJsonTransport};
use crate::time::iso8601_utc;

/// Prefix of the metadata channel the adapter writes into an episode's
/// `source_description`.
///
/// Graphiti's message wire has no arbitrary metadata map, so the claim's
/// project identity and scope travel as a bounded JSON document in the one
/// free-text field the episode keeps. It carries ids, a topic, and times only
/// — never the claim text, which is the episode content itself.
const DESCRIPTION_PREFIX: &str = "aivtuber-v1:";

/// Maximum bytes of that metadata channel.
///
/// The channel is a label beside the claim, not a place to smuggle a second
/// copy of it; a claim whose identity does not fit is refused rather than
/// truncated, because a truncated description would silently lose the scope a
/// recall must be narrowed by.
const MAX_DESCRIPTION_BYTES: usize = 1024;

/// The role a retained claim is filed under.
///
/// Graphiti's extraction prompt distinguishes speakers; a retained claim is
/// input the project already admitted, and the port carries no speaker role,
/// so one constant is used rather than a guess per source class.
const CLAIM_ROLE: &str = "user";

/// Episodes read back when locating a retained claim or resolving a fact.
const EPISODE_WINDOW: u64 = 64;

/// Default per-request timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default deadline for one claim to materialize as an episode.
///
/// Graphiti extracts entities and facts with a model before an episode is
/// complete, so this is measured in seconds to minutes, not milliseconds.
const DEFAULT_INGEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Default wait between episode-list polls.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Where a Graphiti deployment lives and how it is reached.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphitiConfig {
    /// The service base URL, for example `http://127.0.0.1:8000`.
    pub endpoint: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// How long one retain waits for its episode to materialize.
    pub ingest_timeout: Duration,
    /// How long to wait between episode-list polls.
    pub poll_interval: Duration,
    /// Optional bearer credential for a fronting proxy. The reference server
    /// has no authentication of its own, and the token is never printed.
    pub bearer_token: Option<SecretString>,
}

impl GraphitiConfig {
    /// A configuration for `endpoint` with the default timeouts.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout: DEFAULT_TIMEOUT,
            ingest_timeout: DEFAULT_INGEST_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            bearer_token: None,
        }
    }

    /// Set the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the deadline for one claim to materialize.
    pub fn with_ingest_timeout(mut self, timeout: Duration) -> Self {
        self.ingest_timeout = timeout;
        self
    }

    /// Set the wait between episode-list polls.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Set the bearer credential for a fronting proxy.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(SecretString::new(token));
        self
    }

    /// Refuse a configuration that could not address the service, or whose
    /// poll interval cannot fit inside the deadline it is polling against.
    pub fn validate(&self) -> Result<(), EngineError> {
        let url = url::Url::parse(&self.endpoint).map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("graphiti endpoint is not a URL: {error}"),
            )
        })?;
        if !url.has_host() {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "graphiti endpoint has no host",
            ));
        }
        if !matches!(url.scheme(), "http" | "https") {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!(
                    "graphiti endpoint scheme {:?} is not http or https",
                    url.scheme()
                ),
            ));
        }
        if self.ingest_timeout.is_zero() {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "graphiti ingest timeout must be positive",
            ));
        }
        if self.poll_interval.is_zero() {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "graphiti poll interval must be positive",
            ));
        }
        if self.poll_interval > self.ingest_timeout {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!(
                    "graphiti poll interval {:?} does not fit inside the {:?} ingest timeout",
                    self.poll_interval, self.ingest_timeout
                ),
            ));
        }
        Ok(())
    }
}

/// A Graphiti deployment behind the project's memory-sidecar port.
pub struct GraphitiSidecar {
    config: GraphitiConfig,
    transport: Arc<dyn JsonHttpRequestTransport>,
}

impl GraphitiSidecar {
    /// Reach a Graphiti deployment over the network.
    pub fn new(config: GraphitiConfig) -> Self {
        Self::with_transport(config, Arc::new(UreqJsonTransport::default()))
    }

    /// Reach a Graphiti deployment through a given transport.
    pub fn with_transport(
        config: GraphitiConfig,
        transport: Arc<dyn JsonHttpRequestTransport>,
    ) -> Self {
        Self { config, transport }
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &GraphitiConfig {
        &self.config
    }

    /// The URL of one path below the configured endpoint.
    fn url_for(&self, segments: &[&str]) -> Result<String, EngineError> {
        let mut url = url::Url::parse(&self.config.endpoint).map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("graphiti endpoint is not a URL: {error}"),
            )
        })?;
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    "graphiti endpoint cannot carry a path",
                )
            })?;
            path.pop_if_empty();
            path.extend(segments);
        }
        Ok(url.to_string())
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
                        format!("graphiti request is not serializable: {error}"),
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
                format!("graphiti {method} {url} answered {}", response.status),
            ));
        }
        if response.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&response.body).map_err(|error| {
            EngineError::new(
                EngineErrorKind::Backend,
                format!("graphiti {method} answered with a body that is not JSON: {error}"),
            )
        })
    }

    /// The group URL a scope is cleared or dropped through.
    fn group_url(&self, scope: &str) -> Result<String, EngineError> {
        self.url_for(&["group", scope])
    }

    /// The episodes a scope holds, newest first within the window.
    ///
    /// The window is bounded and reapplied on every poll: materialization is
    /// asynchronous, so the claim being waited for is among the newest
    /// episodes once it exists.
    fn list_episodes(&self, scope: &str) -> Result<Vec<Value>, EngineError> {
        let base = self.url_for(&["episodes", scope])?;
        let mut url = url::Url::parse(&base).map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("graphiti episode URL is not a URL: {error}"),
            )
        })?;
        url.query_pairs_mut()
            .append_pair("last_n", &EPISODE_WINDOW.to_string());
        episodes_from(self.send("GET", url.as_str(), None)?)
    }
}

impl MemorySidecar for GraphitiSidecar {
    /// Open an isolated scope, empty.
    ///
    /// Graphiti creates a group with the first episode written into it, so
    /// there is nothing to create here. Clearing is still required: a group
    /// left by an earlier run would make this run measure two inputs at once.
    /// Only a 404 — the group that does not exist yet — is tolerated.
    fn open_scope(&self, scope: &str) -> Result<(), EngineError> {
        self.config.validate()?;
        validate_scope(scope)?;
        self.send_allowing("DELETE", &self.group_url(scope)?, None, &[404])?;
        Ok(())
    }

    /// Retain a claim and return the handle that names its episode.
    ///
    /// `POST /messages` is asynchronous, so the handle is read back from the
    /// episode list; returning before the episode materializes would leave a
    /// retirement with nothing to delete and turn an extraction failure into
    /// a retrieval miss.
    fn retain(&self, scope: &str, claim: &SidecarClaim) -> Result<SidecarHandle, EngineError> {
        self.config.validate()?;
        validate_scope(scope)?;
        claim.validate()?;
        let description = description_for(claim)?;
        let body = json!({
            "group_id": scope,
            "messages": [{
                "content": claim.text,
                "role_type": CLAIM_ROLE,
                "name": claim.claim_id,
                // Event time, not record time: the claim's place in the
                // chronology is what the service is being asked to keep.
                "timestamp": iso8601_utc(claim.event_at_ms),
                "source_description": description,
            }],
        });
        self.send("POST", &self.url_for(&["messages"])?, Some(&body))?;

        let deadline = Instant::now() + self.config.ingest_timeout;
        loop {
            let episodes = self.list_episodes(scope)?;
            if let Some(uuid) = episodes.iter().find_map(|episode| {
                let uuid = episode_uuid(episode)?;
                episode_matches(episode, &claim.claim_id).then(|| uuid.to_owned())
            }) {
                return Ok(SidecarHandle::new(uuid));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(EngineError::new(
                    EngineErrorKind::Timeout,
                    format!(
                        "graphiti did not materialize claim {:?} in scope {scope:?} within {:?}",
                        claim.claim_id, self.config.ingest_timeout
                    ),
                ));
            }
            sleep(self.config.poll_interval.min(deadline.duration_since(now)));
        }
    }

    /// Answer a scoped recall.
    ///
    /// Graphiti ranks facts, not claims, so the adapter asks for a bounded
    /// over-fetch and resolves each fact's episodes back to the claim text the
    /// project retained. A fact whose episodes are all gone is skipped: it
    /// can no longer be attributed to a claim the project holds.
    fn recall(
        &self,
        scope: &str,
        query: &SidecarRecallQuery,
    ) -> Result<Vec<SidecarRecallItem>, EngineError> {
        self.config.validate()?;
        validate_scope(scope)?;
        query.validate()?;
        // Actor scope has no service-side filter, so the adapter asks for a
        // bounded over-fetch and narrows what came back, exactly as the
        // Hindsight adapter does: the service decides order, the project
        // decides scope.
        let max_facts = query
            .limit
            .saturating_mul(4)
            .clamp(query.limit, MAX_SIDECAR_RECALL_ITEMS);
        let body = json!({
            "group_ids": [scope],
            "query": query.query,
            "max_facts": max_facts,
        });
        let response = self.send("POST", &self.url_for(&["search"])?, Some(&body))?;
        let facts = response
            .get("facts")
            .and_then(Value::as_array)
            .ok_or_else(|| backend("graphiti search answered without a facts array"))?;
        if facts.is_empty() {
            return Ok(Vec::new());
        }
        let episodes = self.list_episodes(scope)?;
        let mut items: Vec<SidecarRecallItem> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        'facts: for fact in facts {
            for id in episode_ids_of(fact) {
                let Some(episode) = episodes
                    .iter()
                    .find(|episode| episode_uuid(episode) == Some(id.as_str()))
                else {
                    continue;
                };
                let Some(metadata) = episode_metadata(episode) else {
                    continue;
                };
                if !seen.insert(metadata.claim_id.clone()) {
                    // The same claim already answered at a better rank.
                    continue 'facts;
                }
                let text = episode
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| backend("graphiti episode answered without content"))?;
                items.push(SidecarRecallItem {
                    claim_id: metadata.claim_id,
                    text: text.to_owned(),
                    actor: metadata.actor,
                    topic: metadata.topic,
                    event_at_ms: Some(metadata.event_at_ms),
                });
                continue 'facts;
            }
        }
        Ok(narrow_to_scope(items, query))
    }

    /// Retire a retained claim so it stops being recalled.
    ///
    /// Graphiti deletes the episode. Facts other episodes also contributed to
    /// survive, so a retired claim can still shape a surviving fact; that is
    /// measured by the comparison rather than hidden here. A 404 means the
    /// episode is already gone, which is the state retirement asks for.
    fn retire(&self, scope: &str, handle: &SidecarHandle) -> Result<(), EngineError> {
        self.config.validate()?;
        validate_scope(scope)?;
        if handle.as_str().is_empty() {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "graphiti episode handle must not be empty",
            ));
        }
        self.send_allowing(
            "DELETE",
            &self.url_for(&["episode", handle.as_str()])?,
            None,
            &[404],
        )?;
        Ok(())
    }

    /// Drop the group and everything in it. A group that never materialized
    /// is already gone, so a 404 is the requested state.
    fn close_scope(&self, scope: &str) -> Result<(), EngineError> {
        self.config.validate()?;
        validate_scope(scope)?;
        self.send_allowing("DELETE", &self.group_url(scope)?, None, &[404])?;
        Ok(())
    }
}

/// The metadata a claim travels with. It carries no claim text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimMetadata {
    claim_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic: Option<String>,
    recorded_at_ms: u64,
    event_at_ms: u64,
}

/// Encode the claim's project identity into the episode's description.
fn description_for(claim: &SidecarClaim) -> Result<String, EngineError> {
    let metadata = ClaimMetadata {
        claim_id: claim.claim_id.clone(),
        actor: claim.actor.clone(),
        topic: claim.topic.clone(),
        recorded_at_ms: claim.recorded_at_ms,
        event_at_ms: claim.event_at_ms,
    };
    let encoded = serde_json::to_string(&metadata).map_err(|error| {
        EngineError::new(
            EngineErrorKind::InvalidRequest,
            format!("graphiti claim metadata is not serializable: {error}"),
        )
    })?;
    let description = format!("{DESCRIPTION_PREFIX}{encoded}");
    if description.len() > MAX_DESCRIPTION_BYTES {
        return Err(EngineError::new(
            EngineErrorKind::InvalidRequest,
            format!(
                "graphiti claim metadata is {} bytes, over the {MAX_DESCRIPTION_BYTES}-byte channel",
                description.len()
            ),
        ));
    }
    Ok(description)
}

/// Decode the metadata channel, refusing a description the adapter did not
/// write.
fn parse_description(value: &str) -> Option<ClaimMetadata> {
    let encoded = value.strip_prefix(DESCRIPTION_PREFIX)?;
    let metadata: ClaimMetadata = serde_json::from_str(encoded).ok()?;
    (!metadata.claim_id.is_empty()).then_some(metadata)
}

/// The episode identity, when the service kept one.
fn episode_uuid(episode: &Value) -> Option<&str> {
    episode.get("uuid").and_then(Value::as_str)
}

/// Whether an episode is the one retained for `claim_id`.
fn episode_matches(episode: &Value, claim_id: &str) -> bool {
    episode_metadata(episode).is_some_and(|metadata| metadata.claim_id == claim_id)
        || episode.get("name").and_then(Value::as_str) == Some(claim_id)
}

/// The claim metadata an episode carries, when the adapter wrote it.
fn episode_metadata(episode: &Value) -> Option<ClaimMetadata> {
    episode
        .get("source_description")
        .and_then(Value::as_str)
        .and_then(parse_description)
}

/// The episode identities a fact names, in the order it named them.
///
/// Graphiti's fact shape carries its supporting episodes on the edge; the
/// response spells them as ids or as small objects depending on the endpoint.
/// Both are accepted, and a fact that names none simply cannot be mapped.
fn episode_ids_of(fact: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for key in ["episodes", "episode_uuids", "episode_ids"] {
        let Some(items) = fact.get(key).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let id = item
                .as_str()
                .or_else(|| item.get("uuid").and_then(Value::as_str))
                .or_else(|| item.get("episode_uuid").and_then(Value::as_str));
            if let Some(id) = id
                && !ids.iter().any(|seen| seen == id)
            {
                ids.push(id.to_owned());
            }
        }
    }
    ids
}

/// Read an episode list, accepting the array the service documents and the
/// object envelope some versions answer with.
fn episodes_from(value: Value) -> Result<Vec<Value>, EngineError> {
    match value {
        Value::Array(episodes) => Ok(episodes),
        Value::Object(mut object) => match object.remove("episodes") {
            Some(Value::Array(episodes)) => Ok(episodes),
            _ => Err(backend(
                "graphiti episode list answered without an episodes array",
            )),
        },
        _ => Err(backend(
            "graphiti episode list answered with a body that is not a list",
        )),
    }
}

/// Narrow the service's answer to what the query asked for.
///
/// The project's actor scope is not a service-side filter here, so it is
/// enforced on what the service actually answered: `actor: None` is the scope
/// that carries no actor, and an unscoped claim is shared with every viewer.
fn narrow_to_scope(
    items: Vec<SidecarRecallItem>,
    query: &SidecarRecallQuery,
) -> Vec<SidecarRecallItem> {
    let mut answered: Vec<SidecarRecallItem> = items
        .into_iter()
        .filter(|item| match (&query.actor, &item.actor) {
            (Some(_), None) => true,
            (Some(asked), Some(seen)) => asked == seen,
            (None, None) => true,
            (None, Some(_)) => false,
        })
        .collect();
    answered.truncate(query.limit);
    answered
}

/// Refuse a scope Graphiti could not name a group with.
///
/// The service validates group ids against `[A-Za-z0-9_-]`; checking here
/// fails closed at the adapter instead of turning a typo into a 4xx whose
/// meaning depends on the version.
fn validate_scope(scope: &str) -> Result<(), EngineError> {
    let valid = !scope.is_empty()
        && scope.len() <= 256
        && scope
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(EngineError::new(
            EngineErrorKind::InvalidRequest,
            format!("graphiti scope {scope:?} is not a group id ([A-Za-z0-9_-], 1..=256 bytes)"),
        ))
    }
}

fn backend(message: &str) -> EngineError {
    EngineError::new(EngineErrorKind::Backend, message)
}

fn transport_error(error: HttpTransportError) -> EngineError {
    match error {
        HttpTransportError::Timeout => {
            EngineError::new(EngineErrorKind::Timeout, "graphiti request timed out")
        }
        HttpTransportError::Unavailable(message) => EngineError::new(
            EngineErrorKind::Unavailable,
            format!("graphiti request failed: {message}"),
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

    fn response(status: u16, body: Value) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status,
            body: serde_json::to_vec(&body).expect("serialize response"),
            content_type: Some("application/json".to_owned()),
        })
    }

    fn empty(status: u16) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status,
            body: Vec::new(),
            content_type: None,
        })
    }

    fn sidecar(
        responses: Vec<Result<HttpResponse, HttpTransportError>>,
    ) -> (GraphitiSidecar, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(responses));
        let config = GraphitiConfig::new("http://localhost:8000")
            .with_ingest_timeout(Duration::from_millis(50))
            .with_poll_interval(Duration::from_millis(5));
        let sidecar = GraphitiSidecar::with_transport(config, transport.clone());
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

    fn query(actor: Option<&str>, limit: usize) -> SidecarRecallQuery {
        SidecarRecallQuery {
            actor: actor.map(str::to_owned),
            query: "game x".to_owned(),
            limit,
            now_ms: 3000,
        }
    }

    fn episode_for(uuid: &str, claim: &SidecarClaim) -> Value {
        json!({
            "uuid": uuid,
            "name": claim.claim_id,
            "content": claim.text,
            "source_description": description_for(claim).expect("claim metadata"),
        })
    }

    #[test]
    fn retain_writes_the_claim_and_reads_back_its_handle() {
        let (sidecar, transport) = sidecar(vec![
            response(202, json!({"success": true})),
            response(200, json!({"episodes": [episode_for("ep-1", &claim())]})),
        ]);
        let handle = sidecar.retain("scope-1", &claim()).expect("retain");
        assert_eq!(handle.as_str(), "ep-1");

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert!(requests[0].url.ends_with("/messages"));
        let body: Value = serde_json::from_slice(&requests[0].body).expect("request body");
        assert_eq!(body["group_id"], "scope-1");
        let message = &body["messages"][0];
        assert_eq!(message["content"], "viewer enjoyed the game x ending");
        assert_eq!(message["role_type"], "user");
        assert_eq!(message["name"], "evt-enjoy");
        // Event time, not record time.
        assert_eq!(message["timestamp"], "1970-01-01T00:00:02Z");
        let description = message["source_description"].as_str().expect("description");
        let metadata = parse_description(description).expect("metadata");
        assert_eq!(metadata.claim_id, "evt-enjoy");
        assert_eq!(metadata.actor.as_deref(), Some("actor-key"));
        assert_eq!(metadata.topic.as_deref(), Some("game-x"));
        assert_eq!(metadata.event_at_ms, 2000);
        assert!(!description.contains("viewer enjoyed"));

        assert_eq!(requests[1].method, "GET");
        assert!(requests[1].url.contains("/episodes/scope-1"));
        assert!(requests[1].url.contains("last_n="));
    }

    #[test]
    fn retain_polls_until_the_episode_materializes() {
        let (sidecar, transport) = sidecar(vec![
            response(202, json!({"success": true})),
            response(200, json!([])),
            response(200, json!([episode_for("ep-1", &claim())])),
        ]);
        let handle = sidecar.retain("scope-1", &claim()).expect("retain");
        assert_eq!(handle.as_str(), "ep-1");
        assert_eq!(transport.requests().len(), 3);
    }

    #[test]
    fn retain_fails_closed_when_the_episode_never_materializes() {
        let mut responses = vec![response(202, json!({"success": true}))];
        responses.extend(std::iter::repeat_with(|| response(200, json!([]))).take(40));
        let (sidecar, _transport) = sidecar(responses);
        let error = sidecar
            .retain("scope-1", &claim())
            .expect_err("a claim that never materialized is a failure");
        assert_eq!(error.kind, EngineErrorKind::Timeout);
        assert!(error.message.contains("evt-enjoy"), "{error}");
        assert!(
            !error.message.contains("viewer enjoyed"),
            "the failure must not carry the claim: {error}"
        );
    }

    #[test]
    fn retain_refuses_metadata_that_does_not_fit_the_channel() {
        let mut wide = claim();
        wide.topic = Some("t".repeat(2_000));
        let (sidecar, transport) = sidecar(Vec::new());
        let error = sidecar
            .retain("scope-1", &wide)
            .expect_err("oversized metadata is refused");
        assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn recall_returns_the_retained_claim_text_not_the_fact_text() {
        let (sidecar, transport) = sidecar(vec![
            response(
                200,
                json!({
                    "facts": [{
                        "uuid": "fact-1",
                        "fact": "the service's own wording",
                        "episodes": ["ep-1"]
                    }]
                }),
            ),
            response(200, json!([episode_for("ep-1", &claim())])),
        ]);
        let items = sidecar
            .recall("scope-1", &query(Some("actor-key"), 12))
            .expect("recall");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "viewer enjoyed the game x ending");
        assert_eq!(items[0].claim_id, "evt-enjoy");
        assert_eq!(items[0].actor.as_deref(), Some("actor-key"));
        assert_eq!(items[0].topic.as_deref(), Some("game-x"));
        assert_eq!(items[0].event_at_ms, Some(2000));

        let request = &transport.requests()[0];
        assert_eq!(request.method, "POST");
        assert!(request.url.ends_with("/search"));
        let body: Value = serde_json::from_slice(&request.body).expect("request body");
        assert_eq!(body["group_ids"][0], "scope-1");
        assert_eq!(body["query"], "game x");
        assert_eq!(body["max_facts"], 48);
    }

    #[test]
    fn recall_drops_another_actors_claims_but_keeps_shared_ones() {
        let mut other = claim();
        other.claim_id = "evt-other".to_owned();
        other.actor = Some("someone-else".to_owned());
        let mut shared = claim();
        shared.claim_id = "evt-shared".to_owned();
        shared.actor = None;
        let (sidecar, _transport) = sidecar(vec![
            response(
                200,
                json!({
                    "facts": [
                        {"uuid": "f1", "episodes": ["ep-a"]},
                        {"uuid": "f2", "episodes": ["ep-b"]},
                        {"uuid": "f3", "episodes": ["ep-c"]},
                    ]
                }),
            ),
            response(
                200,
                json!([
                    episode_for("ep-a", &claim()),
                    episode_for("ep-b", &other),
                    episode_for("ep-c", &shared),
                ]),
            ),
        ]);
        let items = sidecar
            .recall("scope-1", &query(Some("actor-key"), 12))
            .expect("recall");
        assert_eq!(
            items
                .iter()
                .map(|item| item.claim_id.as_str())
                .collect::<Vec<_>>(),
            ["evt-enjoy", "evt-shared"]
        );
    }

    #[test]
    fn recall_without_an_actor_answers_only_unscoped_claims() {
        let mut shared = claim();
        shared.claim_id = "evt-shared".to_owned();
        shared.actor = None;
        let (sidecar, _transport) = sidecar(vec![
            response(
                200,
                json!({
                    "facts": [
                        {"uuid": "f1", "episodes": ["ep-a"]},
                        {"uuid": "f2", "episodes": ["ep-b"]},
                    ]
                }),
            ),
            response(
                200,
                json!([episode_for("ep-a", &claim()), episode_for("ep-b", &shared)]),
            ),
        ]);
        let items = sidecar.recall("scope-1", &query(None, 12)).expect("recall");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].claim_id, "evt-shared");
    }

    #[test]
    fn recall_applies_the_callers_item_limit() {
        let mut second = claim();
        second.claim_id = "evt-second".to_owned();
        let (sidecar, _transport) = sidecar(vec![
            response(
                200,
                json!({
                    "facts": [
                        {"uuid": "f1", "episodes": ["ep-a"]},
                        {"uuid": "f2", "episodes": ["ep-b"]},
                    ]
                }),
            ),
            response(
                200,
                json!([episode_for("ep-a", &claim()), episode_for("ep-b", &second)]),
            ),
        ]);
        let items = sidecar
            .recall("scope-1", &query(Some("actor-key"), 1))
            .expect("recall");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].claim_id, "evt-enjoy");
    }

    #[test]
    fn recall_skips_facts_without_a_retained_episode() {
        let (sidecar, _transport) = sidecar(vec![
            response(
                200,
                json!({
                    "facts": [
                        {"uuid": "f1", "episodes": ["ep-gone"]},
                        {"uuid": "f2"}
                    ]
                }),
            ),
            // A retired episode is deleted, and the fact that named it may
            // survive because another episode also contributed to it.
            response(200, json!([])),
        ]);
        let items = sidecar
            .recall("scope-1", &query(Some("actor-key"), 12))
            .expect("recall");
        assert!(items.is_empty());
    }

    #[test]
    fn recall_refuses_an_answer_without_facts() {
        let (sidecar, _transport) = sidecar(vec![response(200, json!({"unexpected": true}))]);
        let error = sidecar
            .recall("scope-1", &query(Some("actor-key"), 12))
            .expect_err("a search without facts is not an answer");
        assert_eq!(error.kind, EngineErrorKind::Backend);
        assert!(error.message.contains("facts"), "{error}");
    }

    #[test]
    fn retire_deletes_the_episode_and_tolerates_an_absent_one() {
        let (first, transport) = sidecar(vec![empty(200)]);
        first
            .retire("scope-1", &SidecarHandle::new("ep-1"))
            .expect("retire");
        let request = &transport.requests()[0];
        assert_eq!(request.method, "DELETE");
        assert!(request.url.ends_with("/episode/ep-1"));

        let (second, _transport) = sidecar(vec![empty(404)]);
        second
            .retire("scope-1", &SidecarHandle::new("ep-1"))
            .expect("an already deleted episode is retired");
    }

    #[test]
    fn retire_refuses_an_empty_handle() {
        let (sidecar, transport) = sidecar(Vec::new());
        let error = sidecar
            .retire("scope-1", &SidecarHandle::new(""))
            .expect_err("an empty handle is not an episode");
        assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn open_scope_clears_the_group_first() {
        let (sidecar, transport) = sidecar(vec![empty(404)]);
        sidecar.open_scope("scope-1").expect("open scope");
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "DELETE");
        assert!(requests[0].url.ends_with("/group/scope-1"));
    }

    #[test]
    fn open_scope_stops_when_the_group_cannot_be_cleared() {
        for status in [403_u16, 500] {
            let (sidecar, transport) = sidecar(vec![empty(status)]);
            let error = sidecar
                .open_scope("scope-1")
                .expect_err("a group that cannot be cleared stops the open");
            assert_eq!(error.kind, EngineErrorKind::Unavailable);
            assert!(error.message.contains(&status.to_string()), "{error}");
            assert_eq!(transport.requests().len(), 1);
        }
    }

    #[test]
    fn close_scope_drops_the_group_and_tolerates_absence() {
        let (sidecar, transport) = sidecar(vec![empty(404)]);
        sidecar.close_scope("scope-1").expect("close scope");
        let request = &transport.requests()[0];
        assert_eq!(request.method, "DELETE");
        assert!(request.url.ends_with("/group/scope-1"));
    }

    #[test]
    fn a_scope_that_is_not_a_group_id_is_refused() {
        let (sidecar, transport) = sidecar(Vec::new());
        for scope in ["case:1", "case/1", ""] {
            let error = sidecar
                .open_scope(scope)
                .expect_err("an invalid group id is refused");
            assert_eq!(error.kind, EngineErrorKind::InvalidRequest, "{error}");
        }
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn an_endpoint_without_a_host_is_refused() {
        for endpoint in ["not a url", "ftp://localhost:8000", "http://"] {
            let error = GraphitiConfig::new(endpoint)
                .validate()
                .expect_err("an endpoint that is not http(s) with a host is refused");
            assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
        }
        GraphitiConfig::new("http://127.0.0.1:8000")
            .validate()
            .expect("a loopback endpoint");
    }

    #[test]
    fn a_poll_interval_that_does_not_fit_the_deadline_is_refused() {
        let error = GraphitiConfig::new("http://127.0.0.1:8000")
            .with_ingest_timeout(Duration::from_secs(1))
            .with_poll_interval(Duration::from_secs(2))
            .validate()
            .expect_err("a poll that cannot run inside the deadline is refused");
        assert_eq!(error.kind, EngineErrorKind::InvalidRequest);

        let error = GraphitiConfig::new("http://127.0.0.1:8000")
            .with_ingest_timeout(Duration::ZERO)
            .validate()
            .expect_err("a zero deadline is refused");
        assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
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
}
