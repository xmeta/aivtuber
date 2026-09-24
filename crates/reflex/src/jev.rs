use aivtuber_domain::{
    AttentionTarget, BackendIdentity, DecisionEngine, EngineError, EngineErrorKind, EngineFuture,
    REFLEX_SCHEMA_VERSION, ReflexDecision, ReflexRequest, ResponseRoute, RouteDecision,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const DEFAULT_MODEL: &str = "jev-latest";

pub struct JevApiKey(Vec<u8>);

impl JevApiKey {
    pub fn new(value: impl AsRef<str>) -> Result<Self, EngineError> {
        let value = value.as_ref();
        if value.trim().is_empty() {
            return Err(EngineError::new(
                EngineErrorKind::Authentication,
                "Jev API key must not be empty",
            ));
        }
        Ok(Self(value.as_bytes().to_vec()))
    }

    fn expose(&self) -> &str {
        std::str::from_utf8(&self.0).expect("API key originated from UTF-8")
    }
}
impl fmt::Debug for JevApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JevApiKey([REDACTED])")
    }
}

impl Drop for JevApiKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JevAdapterConfig {
    pub endpoint: String,
    pub model_alias: String,
    pub deadline: Duration,
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_state_bytes: usize,
    pub max_candidates: usize,
}

impl Default for JevAdapterConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            model_alias: DEFAULT_MODEL.to_owned(),
            deadline: Duration::from_millis(250),
            max_attempts: 3,
            initial_backoff: Duration::from_millis(20),
            max_state_bytes: 16 * 1024,
            max_candidates: 8,
        }
    }
}

impl JevAdapterConfig {
    fn validate(&self) -> Result<(), EngineError> {
        if !self.endpoint.starts_with("https://") {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "Jev endpoint must use HTTPS",
            ));
        }
        if self.model_alias.trim().is_empty() || self.deadline.is_zero() {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "Jev model alias and deadline must be non-empty",
            ));
        }
        if self.max_attempts == 0 || self.max_state_bytes == 0 || self.max_candidates == 0 {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "Jev limits must be positive",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    Unavailable(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("transport deadline exceeded"),
            Self::Unavailable(message) => write!(f, "transport unavailable: {message}"),
        }
    }
}

impl Error for TransportError {}

pub trait HttpTransport: Send + Sync {
    fn post_json(
        &self,
        endpoint: &str,
        bearer_token: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, TransportError>;
}

#[derive(Clone)]
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl Default for UreqTransport {
    fn default() -> Self {
        let config = ureq::Agent::config_builder().https_only(true).build();
        Self {
            agent: config.into(),
        }
    }
}

impl fmt::Debug for UreqTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UreqTransport")
    }
}
impl HttpTransport for UreqTransport {
    fn post_json(
        &self,
        endpoint: &str,
        bearer_token: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        let authorization = format!("Bearer {bearer_token}");
        let request = self
            .agent
            .post(endpoint)
            .header("authorization", &authorization)
            .header("content-type", "application/json")
            .config()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();

        let mut response = request.send(body).map_err(map_ureq_error)?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|error| TransportError::Unavailable(error.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

fn map_ureq_error(error: ureq::Error) -> TransportError {
    match error {
        ureq::Error::Timeout(_) => TransportError::Timeout,
        other => TransportError::Unavailable(other.to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JevUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedAnswer {
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Noul {
        probability: f64,
    },
    Score {
        score: f64,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevCallEvidence {
    pub decision: ReflexDecision,
    pub selected_candidate_id: Option<String>,
    pub requested_model: String,
    pub returned_model: String,
    pub usage: JevUsage,
    pub attempts: u32,
    pub latency_ms: f64,
    pub answers: BTreeMap<String, NormalizedAnswer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JevCallFailure {
    pub kind: EngineErrorKind,
    pub message: String,
    pub requested_model: String,
    pub attempts: u32,
    pub latency_ms: f64,
}

impl JevCallFailure {
    fn new(
        kind: EngineErrorKind,
        message: impl Into<String>,
        requested_model: &str,
        attempts: u32,
        started: Instant,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            requested_model: requested_model.to_owned(),
            attempts,
            latency_ms: started.elapsed().as_secs_f64() * 1000.0,
        }
    }

    pub fn as_engine_error(&self) -> EngineError {
        EngineError::new(self.kind, self.message.clone())
    }
}

#[derive(Debug, Clone, Default)]
pub struct JevCancellationToken(Arc<AtomicBool>);

impl JevCancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, AtomicOrdering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire)
    }
}
#[derive(Clone)]
pub struct JevAdapter {
    config: JevAdapterConfig,
    api_key: Arc<JevApiKey>,
    transport: Arc<dyn HttpTransport>,
}

impl fmt::Debug for JevAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JevAdapter")
            .field("config", &self.config)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl JevAdapter {
    pub fn new(config: JevAdapterConfig, api_key: JevApiKey) -> Result<Self, EngineError> {
        Self::with_transport(config, api_key, Arc::new(UreqTransport::default()))
    }

    pub fn with_transport(
        config: JevAdapterConfig,
        api_key: JevApiKey,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, EngineError> {
        config.validate()?;
        Ok(Self {
            config,
            api_key: Arc::new(api_key),
            transport,
        })
    }

    pub fn config(&self) -> &JevAdapterConfig {
        &self.config
    }

    pub fn evaluate_evidence(
        &self,
        request: &ReflexRequest,
    ) -> Result<JevCallEvidence, JevCallFailure> {
        self.evaluate_with_cancellation(request, &JevCancellationToken::default())
    }

    pub fn evaluate_with_cancellation(
        &self,
        request: &ReflexRequest,
        cancellation: &JevCancellationToken,
    ) -> Result<JevCallEvidence, JevCallFailure> {
        let started = Instant::now();
        let deadline = started + self.config.deadline;
        let body = self.build_request_body(request).map_err(|error| {
            JevCallFailure::new(
                error.kind,
                error.message,
                &self.config.model_alias,
                0,
                started,
            )
        })?;
        let mut attempts = 0_u32;

        loop {
            if cancellation.is_cancelled() {
                return Err(JevCallFailure::new(
                    EngineErrorKind::Timeout,
                    "Jev request cancelled",
                    &self.config.model_alias,
                    attempts,
                    started,
                ));
            }

            let Some(remaining) = remaining_until(deadline) else {
                return Err(JevCallFailure::new(
                    EngineErrorKind::Timeout,
                    "Jev reflex deadline exceeded",
                    &self.config.model_alias,
                    attempts,
                    started,
                ));
            };
            attempts += 1;
            let response = match self.transport.post_json(
                &self.config.endpoint,
                self.api_key.expose(),
                &body,
                remaining,
            ) {
                Ok(response) => response,
                Err(TransportError::Timeout) => {
                    return Err(JevCallFailure::new(
                        EngineErrorKind::Timeout,
                        "Jev transport deadline exceeded",
                        &self.config.model_alias,
                        attempts,
                        started,
                    ));
                }
                Err(TransportError::Unavailable(message)) => {
                    return Err(JevCallFailure::new(
                        EngineErrorKind::Unavailable,
                        message,
                        &self.config.model_alias,
                        attempts,
                        started,
                    ));
                }
            };

            if response.status == 200 {
                if remaining_until(deadline).is_none() {
                    return Err(JevCallFailure::new(
                        EngineErrorKind::Timeout,
                        "Jev reflex deadline exceeded before normalization",
                        &self.config.model_alias,
                        attempts,
                        started,
                    ));
                }
                return self.normalize_response(
                    &response.body,
                    request,
                    attempts,
                    started,
                    deadline,
                );
            }

            let retry_kind = match response.status {
                401 | 403 => {
                    return Err(JevCallFailure::new(
                        EngineErrorKind::Authentication,
                        format!("Jev HTTP {}", response.status),
                        &self.config.model_alias,
                        attempts,
                        started,
                    ));
                }
                400 | 422 => {
                    return Err(JevCallFailure::new(
                        EngineErrorKind::InvalidRequest,
                        format!("Jev HTTP {}", response.status),
                        &self.config.model_alias,
                        attempts,
                        started,
                    ));
                }
                429 => Some(EngineErrorKind::RateLimited),
                529 => Some(EngineErrorKind::Overloaded),
                _ => None,
            };

            let Some(kind) = retry_kind else {
                return Err(JevCallFailure::new(
                    EngineErrorKind::Unavailable,
                    format!("Jev HTTP {}", response.status),
                    &self.config.model_alias,
                    attempts,
                    started,
                ));
            };

            if attempts >= self.config.max_attempts {
                return Err(JevCallFailure::new(
                    kind,
                    format!("Jev HTTP {} after {attempts} attempts", response.status),
                    &self.config.model_alias,
                    attempts,
                    started,
                ));
            }

            let backoff = self.backoff_for(attempts);
            if !sleep_with_cancellation(backoff, deadline, cancellation) {
                return Err(JevCallFailure::new(
                    EngineErrorKind::Timeout,
                    "Jev reflex deadline exhausted during retry backoff",
                    &self.config.model_alias,
                    attempts,
                    started,
                ));
            }
        }
    }

    fn backoff_for(&self, attempt: u32) -> Duration {
        let multiplier = 1_u32 << attempt.saturating_sub(1).min(16);
        self.config.initial_backoff.saturating_mul(multiplier)
    }
    fn build_request_body(&self, request: &ReflexRequest) -> Result<Vec<u8>, EngineError> {
        request.validate().map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("invalid reflex request: {error}"),
            )
        })?;
        let candidates = request
            .retrieval
            .candidate_asset_ids()
            .take(self.config.max_candidates)
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let state = compact_state(request, &candidates);
        let state_bytes = serde_json::to_vec(&state).map_err(|error| {
            EngineError::new(EngineErrorKind::InvalidRequest, error.to_string())
        })?;
        if state_bytes.len() > self.config.max_state_bytes {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!(
                    "compact Jev state exceeds {} bytes",
                    self.config.max_state_bytes
                ),
            ));
        }

        let request = SystemOneRequest {
            state,
            model: self.config.model_alias.clone(),
            questions: build_questions(&candidates),
        };
        serde_json::to_vec(&request)
            .map_err(|error| EngineError::new(EngineErrorKind::InvalidRequest, error.to_string()))
    }

    fn normalize_response(
        &self,
        body: &[u8],
        request: &ReflexRequest,
        attempts: u32,
        started: Instant,
        deadline: Instant,
    ) -> Result<JevCallEvidence, JevCallFailure> {
        let response: SystemOneResponse = serde_json::from_slice(body).map_err(|error| {
            JevCallFailure::new(
                EngineErrorKind::InvalidRequest,
                format!("invalid Jev response JSON: {error}"),
                &self.config.model_alias,
                attempts,
                started,
            )
        })?;

        if remaining_until(deadline).is_none() {
            return Err(JevCallFailure::new(
                EngineErrorKind::Timeout,
                "Jev reflex deadline exceeded during response parsing",
                &self.config.model_alias,
                attempts,
                started,
            ));
        }

        let candidates = request
            .retrieval
            .candidate_asset_ids()
            .take(self.config.max_candidates)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let normalized = normalize_answers(
            &response.answers,
            &candidates,
            &self.config.model_alias,
            &response.model,
            started.elapsed().as_secs_f64() * 1000.0,
        )
        .map_err(|message| {
            JevCallFailure::new(
                EngineErrorKind::InvalidRequest,
                message,
                &self.config.model_alias,
                attempts,
                started,
            )
        })?;

        Ok(JevCallEvidence {
            decision: normalized.decision,
            selected_candidate_id: normalized.selected_candidate_id,
            requested_model: self.config.model_alias.clone(),
            returned_model: response.model,
            usage: response.usage,
            attempts,
            latency_ms: started.elapsed().as_secs_f64() * 1000.0,
            answers: normalized.answers,
        })
    }
}

impl DecisionEngine for JevAdapter {
    fn decide<'a>(&'a self, request: &'a ReflexRequest) -> EngineFuture<'a, ReflexDecision> {
        let adapter = self.clone();
        let request = request.clone();
        let token = JevCancellationToken::default();
        let future = WorkerFuture::spawn(token, move |cancellation| {
            adapter
                .evaluate_with_cancellation(&request, &cancellation)
                .map(|evidence| evidence.decision)
                .map_err(|failure| failure.as_engine_error())
        });
        Box::pin(future)
    }
}
#[derive(Debug, Serialize)]
struct SystemOneRequest {
    state: Value,
    model: String,
    questions: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct SystemOneResponse {
    model: String,
    answers: BTreeMap<String, Value>,
    usage: JevUsage,
}

fn compact_state(request: &ReflexRequest, candidates: &[String]) -> Value {
    let retrieval = request
        .retrieval
        .candidates
        .iter()
        .take(candidates.len())
        .collect::<Vec<_>>();

    json!({
        "request_schema_version": request.schema_version,
        "event": {
            "kind": request.event.kind,
            "source_class": request.event.source_class,
            "trust_level": request.event.trust_level,
            "actor_id": request.event.actor_id,
            "payload": request.event.payload,
        },
        "runtime": {
            "schema_version": request.context.schema_version,
            "performer": request.context.performer,
            "stream": request.context.stream,
            "recent": request.context.recent,
            "now_ms": request.context.now_ms,
        },
        "retrieval": {
            "candidates": retrieval,
        }
    })
}

fn build_questions(candidates: &[String]) -> BTreeMap<String, Value> {
    let mut questions = BTreeMap::new();
    questions.insert(
        "response_route".to_owned(),
        choice_question(
            "Choose the response route for this event.",
            &[
                ("silent", "No public response."),
                ("reaction", "Non-verbal or short reaction."),
                ("cached", "Reuse one retrieved performance asset."),
                ("template", "Use a deterministic template."),
                ("llm", "Generate a novel long-form response."),
            ],
        ),
    );
    questions.insert(
        "reaction_family".to_owned(),
        choice_question(
            "Choose a compact semantic reaction family.",
            &[
                ("none", "No reaction."),
                ("agree", "Agreement or affirmation."),
                ("laugh", "Amused or playful laughter."),
                ("surprise", "Surprised reaction."),
                ("confused", "Confusion or uncertainty."),
                ("empathy", "Supportive or empathetic reaction."),
            ],
        ),
    );
    questions.insert(
        "gesture_family".to_owned(),
        choice_question(
            "Choose an allowed gesture family.",
            &[
                ("none", "No gesture."),
                ("nod_small", "Small nod."),
                ("head_shake_small", "Small head shake."),
                ("wave_small", "Small wave."),
                ("shrug_small", "Small shrug."),
            ],
        ),
    );
    questions.insert(
        "attention_target".to_owned(),
        choice_question(
            "Choose the attention target.",
            &[
                ("camera", "Look toward the camera."),
                ("chat", "Attend to chat."),
                ("game", "Attend to the game."),
                ("speaker", "Attend to the speaker."),
                ("away", "No active target."),
            ],
        ),
    );
    questions.insert(
        "interrupt".to_owned(),
        noul_question("Should the current performance be interrupted?"),
    );
    questions.insert(
        "cache_reuse".to_owned(),
        noul_question("Is a retrieved candidate contextually safe to reuse?"),
    );
    questions.insert(
        "importance".to_owned(),
        score_question(
            "How important is this event?",
            &["low importance", "high importance"],
        ),
    );
    questions.insert(
        "emotion_intensity".to_owned(),
        score_question(
            "How intense should the emotional expression be?",
            &["low intensity", "high intensity"],
        ),
    );
    if !candidates.is_empty() {
        let mut criteria = BTreeMap::from([(
            "none".to_owned(),
            "Do not reuse any retrieved candidate.".to_owned(),
        )]);
        for candidate in candidates {
            criteria.insert(
                candidate.clone(),
                format!("Reuse retrieved asset {candidate}."),
            );
        }
        questions.insert(
            "selected_candidate".to_owned(),
            json!({
                "type": "choice",
                "instructions": "Select the best reusable candidate, or none.",
                "criteria": criteria,
            }),
        );
    }
    questions
}

fn choice_question(instructions: &str, criteria: &[(&str, &str)]) -> Value {
    let criteria = criteria
        .iter()
        .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
        .collect::<BTreeMap<_, _>>();
    json!({
        "type": "choice",
        "instructions": instructions,
        "criteria": criteria,
    })
}

fn noul_question(instructions: &str) -> Value {
    json!({
        "type": "noul",
        "instructions": instructions,
    })
}

fn score_question(instructions: &str, criteria: &[&str]) -> Value {
    json!({
        "type": "score",
        "instructions": instructions,
        "criteria": criteria,
    })
}

struct NormalizedResponse {
    decision: ReflexDecision,
    selected_candidate_id: Option<String>,
    answers: BTreeMap<String, NormalizedAnswer>,
}
fn normalize_answers(
    answers: &BTreeMap<String, Value>,
    candidates: &[String],
    requested_model: &str,
    returned_model: &str,
    latency_ms: f64,
) -> Result<NormalizedResponse, String> {
    let (route, route_confidence, route_answer) = parse_choice(answers, "response_route")?;
    let route = match route.as_str() {
        "silent" => ResponseRoute::Silent,
        "reaction" => ResponseRoute::Reaction,
        "cached" => ResponseRoute::Cached,
        "template" => ResponseRoute::Template,
        "llm" => ResponseRoute::Llm,
        other => return Err(format!("unknown response_route choice {other:?}")),
    };

    let (reaction, _, reaction_answer) = parse_choice(answers, "reaction_family")?;
    let (gesture, _, gesture_answer) = parse_choice(answers, "gesture_family")?;
    let (attention, _, attention_answer) = parse_choice(answers, "attention_target")?;
    let attention_target = match attention.as_str() {
        "camera" => AttentionTarget::Camera,
        "chat" => AttentionTarget::Chat,
        "game" => AttentionTarget::Game,
        "speaker" => AttentionTarget::Speaker,
        "away" => AttentionTarget::Away,
        other => return Err(format!("unknown attention_target choice {other:?}")),
    };

    let (interrupt_probability, interrupt_answer) = parse_noul(answers, "interrupt")?;
    let (cache_reuse_probability, cache_answer) = parse_noul(answers, "cache_reuse")?;
    let (importance, importance_answer) = parse_score(answers, "importance")?;
    let (emotion_intensity, emotion_answer) = parse_score(answers, "emotion_intensity")?;

    let mut normalized_answers = BTreeMap::new();
    normalized_answers.insert("response_route".to_owned(), route_answer);
    normalized_answers.insert("reaction_family".to_owned(), reaction_answer);
    normalized_answers.insert("gesture_family".to_owned(), gesture_answer);
    normalized_answers.insert("attention_target".to_owned(), attention_answer);
    normalized_answers.insert("interrupt".to_owned(), interrupt_answer);
    normalized_answers.insert("cache_reuse".to_owned(), cache_answer);
    normalized_answers.insert("importance".to_owned(), importance_answer);
    normalized_answers.insert("emotion_intensity".to_owned(), emotion_answer);
    let selected_candidate_id = if candidates.is_empty() {
        None
    } else {
        let (selected, _, selected_answer) = parse_choice(answers, "selected_candidate")?;
        normalized_answers.insert("selected_candidate".to_owned(), selected_answer);
        if selected == "none" {
            None
        } else if candidates.iter().any(|candidate| candidate == &selected) {
            Some(selected)
        } else {
            return Err(format!(
                "Jev selected candidate {selected:?} that was not in Top-K"
            ));
        }
    };

    let decision = ReflexDecision {
        schema_version: REFLEX_SCHEMA_VERSION.to_owned(),
        route: RouteDecision {
            value: route,
            confidence: Some(route_confidence),
        },
        reaction_family: (reaction != "none").then_some(reaction),
        gesture_family: (gesture != "none").then_some(gesture),
        attention_target,
        interrupt_probability,
        cache_reuse_probability,
        importance,
        emotion_intensity,
        backend: BackendIdentity {
            name: "jev".to_owned(),
            model_alias: Some(requested_model.to_owned()),
            model_version: Some(returned_model.to_owned()),
        },
        latency_ms,
        fallback_reason: aivtuber_domain::FallbackReason::None,
    };
    decision
        .validate()
        .map_err(|error| format!("normalized Jev decision is invalid: {error}"))?;

    Ok(NormalizedResponse {
        decision,
        selected_candidate_id,
        answers: normalized_answers,
    })
}

fn parse_choice(
    answers: &BTreeMap<String, Value>,
    name: &str,
) -> Result<(String, f64, NormalizedAnswer), String> {
    let object = answer_object(answers, name, "choice")?;
    let choice = object
        .get("choice")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name}.choice is missing"))?
        .to_owned();
    let confidence = unit_number(object.get("confidence"), &format!("{name}.confidence"))?;
    let probabilities = probability_map(object.get("probabilities"), name)?;
    Ok((
        choice.clone(),
        confidence,
        NormalizedAnswer::Choice {
            choice,
            confidence,
            probabilities,
        },
    ))
}
fn parse_noul(
    answers: &BTreeMap<String, Value>,
    name: &str,
) -> Result<(f64, NormalizedAnswer), String> {
    let object = answer_object(answers, name, "noul")?;
    let probability = unit_number(object.get("noul"), &format!("{name}.noul"))?;
    Ok((probability, NormalizedAnswer::Noul { probability }))
}

fn parse_score(
    answers: &BTreeMap<String, Value>,
    name: &str,
) -> Result<(f64, NormalizedAnswer), String> {
    let object = answer_object(answers, name, "score")?;
    let score = unit_number(object.get("score"), &format!("{name}.score"))?;
    let confidence = unit_number(object.get("confidence"), &format!("{name}.confidence"))?;
    let probabilities = probability_map(object.get("probabilities"), name)?;
    if !object.get("legend").is_some_and(Value::is_object) {
        return Err(format!("{name}.legend is missing"));
    }
    Ok((
        score,
        NormalizedAnswer::Score {
            score,
            confidence,
            probabilities,
        },
    ))
}

fn answer_object<'a>(
    answers: &'a BTreeMap<String, Value>,
    name: &str,
    expected_type: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = answers
        .get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("answer {name:?} is missing or not an object"))?;
    let answer_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name}.type is missing"))?;
    if answer_type != expected_type {
        return Err(format!(
            "{name}.type expected {expected_type:?}, got {answer_type:?}"
        ));
    }
    Ok(object)
}

fn unit_number(value: Option<&Value>, field: &str) -> Result<f64, String> {
    let number = value
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{field} is missing or not a number"))?;
    if number.is_finite() && (0.0..=1.0).contains(&number) {
        Ok(number)
    } else {
        Err(format!("{field} must be in 0..=1"))
    }
}
fn probability_map(value: Option<&Value>, field: &str) -> Result<BTreeMap<String, f64>, String> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{field}.probabilities is missing"))?;
    let mut probabilities = BTreeMap::new();
    for (name, value) in object {
        let probability = unit_number(Some(value), &format!("{field}.probabilities.{name}"))?;
        probabilities.insert(name.clone(), probability);
    }
    if probabilities.is_empty() {
        return Err(format!("{field}.probabilities must not be empty"));
    }
    Ok(probabilities)
}

fn remaining_until(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
}

fn sleep_with_cancellation(
    duration: Duration,
    deadline: Instant,
    cancellation: &JevCancellationToken,
) -> bool {
    if duration.is_zero() {
        return !cancellation.is_cancelled() && remaining_until(deadline).is_some();
    }
    let Some(remaining) = remaining_until(deadline) else {
        return false;
    };
    if duration >= remaining {
        return false;
    }

    let slice = Duration::from_millis(5);
    let sleep_until = Instant::now() + duration;
    while Instant::now() < sleep_until {
        if cancellation.is_cancelled() || remaining_until(deadline).is_none() {
            return false;
        }
        let left = sleep_until.saturating_duration_since(Instant::now());
        thread::sleep(left.min(slice));
    }
    true
}

struct WorkerState<T> {
    result: Option<T>,
    waker: Option<Waker>,
}

struct WorkerFuture<T> {
    shared: Arc<Mutex<WorkerState<T>>>,
    cancellation: JevCancellationToken,
}
impl<T: Send + 'static> WorkerFuture<T> {
    fn spawn(
        cancellation: JevCancellationToken,
        work: impl FnOnce(JevCancellationToken) -> T + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Mutex::new(WorkerState {
            result: None,
            waker: None,
        }));
        let thread_shared = Arc::clone(&shared);
        let thread_cancellation = cancellation.clone();

        thread::spawn(move || {
            let result = work(thread_cancellation);
            let waker = {
                let mut state = thread_shared.lock().expect("Jev worker state poisoned");
                state.result = Some(result);
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        });

        Self {
            shared,
            cancellation,
        }
    }
}

impl<T> Future for WorkerFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.shared.lock().expect("Jev worker state poisoned");
        if let Some(result) = state.result.take() {
            Poll::Ready(result)
        } else {
            state.waker = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

impl<T> Drop for WorkerFuture<T> {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, PerformerSnapshot,
        RecentInteractionSnapshot, ReflexContext, RetrievalCandidateContext, RetrievalSnapshot,
        SecurityPlane, SourceClass, TrustLevel,
    };
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        endpoint: String,
        bearer: String,
        body: Vec<u8>,
        timeout: Duration,
    }

    #[derive(Debug, Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    impl MockTransport {
        fn with_responses(responses: Vec<Result<HttpResponse, TransportError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().expect("requests").clone()
        }
    }

    impl HttpTransport for MockTransport {
        fn post_json(
            &self,
            endpoint: &str,
            bearer_token: &str,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, TransportError> {
            self.requests
                .lock()
                .expect("requests")
                .push(RecordedRequest {
                    endpoint: endpoint.to_owned(),
                    bearer: bearer_token.to_owned(),
                    body: body.to_vec(),
                    timeout,
                });
            self.responses
                .lock()
                .expect("responses")
                .pop_front()
                .unwrap_or_else(|| {
                    Err(TransportError::Unavailable(
                        "no scripted response".to_owned(),
                    ))
                })
        }
    }

    fn request() -> ReflexRequest {
        let event = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: "evt-jev-1".to_owned(),
            correlation_id: "corr-jev-1".to_owned(),
            sequence: 7,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: "example-chat".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some("viewer:test".to_owned()),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([("text".to_owned(), Value::String("hello".to_owned()))]),
        };

        let context = ReflexContext {
            performer: PerformerSnapshot {
                currently_speaking: true,
                current_interruptible: true,
                ..PerformerSnapshot::default()
            },
            recent: RecentInteractionSnapshot {
                reaction_families: vec!["laugh".to_owned()],
                ..RecentInteractionSnapshot::default()
            },
            ..ReflexContext::default()
        };
        let mut request = ReflexRequest::new(event, context);
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

    fn choice(choice: &str) -> Value {
        json!({
            "type": "choice",
            "choice": choice,
            "confidence": 0.9,
            "probabilities": {choice: 0.9, "other": 0.1}
        })
    }

    fn success_body(selected: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "jev-2026-09-15",
            "answers": {
                "response_route": choice("cached"),
                "reaction_family": choice("laugh"),
                "gesture_family": choice("nod_small"),
                "attention_target": choice("camera"),
                "interrupt": {"type": "noul", "noul": 0.2},
                "cache_reuse": {"type": "noul", "noul": 0.94},
                "importance": {
                    "type": "score", "score": 0.4, "confidence": 0.8,
                    "legend": {"0": "low", "1": "high"},
                    "probabilities": {"0": 0.6, "1": 0.4}
                },
                "emotion_intensity": {
                    "type": "score", "score": 0.7, "confidence": 0.85,
                    "legend": {"0": "low", "1": "high"},
                    "probabilities": {"0": 0.3, "1": 0.7}
                },
                "selected_candidate": choice(selected)
            },
            "usage": {"input_tokens": 120, "output_tokens": 9}
        }))
        .expect("success JSON")
    }

    fn config() -> JevAdapterConfig {
        JevAdapterConfig {
            deadline: Duration::from_millis(200),
            initial_backoff: Duration::ZERO,
            ..JevAdapterConfig::default()
        }
    }

    fn adapter(transport: Arc<dyn HttpTransport>) -> JevAdapter {
        JevAdapter::with_transport(
            config(),
            JevApiKey::new("secret-value").expect("key"),
            transport,
        )
        .expect("adapter")
    }

    #[test]
    fn system_one_wire_contract_uses_bearer_model_state_and_named_questions() {
        let transport = Arc::new(MockTransport::with_responses(vec![Ok(HttpResponse {
            status: 200,
            body: success_body("asset.a"),
        })]));
        let adapter = adapter(transport.clone());

        let evidence = adapter.evaluate_evidence(&request()).expect("evidence");
        assert_eq!(evidence.requested_model, "jev-latest");
        assert_eq!(evidence.returned_model, "jev-2026-09-15");
        assert_eq!(evidence.usage.input_tokens, 120);
        assert_eq!(evidence.selected_candidate_id.as_deref(), Some("asset.a"));

        let recorded = transport.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].endpoint, DEFAULT_ENDPOINT);
        assert_eq!(recorded[0].bearer, "secret-value");
        assert!(recorded[0].timeout <= Duration::from_millis(200));

        let body: Value = serde_json::from_slice(&recorded[0].body).expect("request JSON");
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["state"]["request_schema_version"], "0.1.0");
        assert_eq!(body["state"]["runtime"]["schema_version"], "0.1.0");
        assert_eq!(
            body["state"]["runtime"]["performer"]["currently_speaking"],
            true
        );
        assert_eq!(
            body["state"]["retrieval"]["candidates"][0]["asset_id"],
            "asset.a"
        );
        assert_eq!(body["state"]["retrieval"]["candidates"][0]["rank"], 1);
        assert!(body["state"]["retrieval"]["candidates"][0]["similarity"].is_number());
        assert!(body["state"].get("transcript").is_none());
        assert!(body["state"]["runtime"].get("transcript").is_none());
        assert!(body["questions"].is_object());
        assert_eq!(body["questions"]["interrupt"]["type"], "noul");
        assert_eq!(body["questions"]["importance"]["type"], "score");
        assert_eq!(body["questions"]["response_route"]["type"], "choice");
        assert_eq!(body["questions"]["selected_candidate"]["type"], "choice");

        let serialized = String::from_utf8(recorded[0].body.clone()).expect("utf8");
        assert!(!serialized.contains("secret-value"));
        assert!(!serialized.contains("\"transcript\""));
    }
    #[test]
    fn authentication_and_validation_fail_fast_without_retry() {
        for (status, kind) in [
            (401_u16, EngineErrorKind::Authentication),
            (422_u16, EngineErrorKind::InvalidRequest),
        ] {
            let transport = Arc::new(MockTransport::with_responses(vec![
                Ok(HttpResponse {
                    status,
                    body: Vec::new(),
                }),
                Ok(HttpResponse {
                    status: 200,
                    body: success_body("asset.a"),
                }),
            ]));
            let adapter = adapter(transport.clone());
            let failure = adapter
                .evaluate_evidence(&request())
                .expect_err("must fail fast");
            assert_eq!(failure.kind, kind);
            assert_eq!(failure.attempts, 1);
            assert_eq!(transport.requests().len(), 1);
        }
    }

    #[test]
    fn rate_limit_and_overload_retry_within_one_deadline() {
        let transport = Arc::new(MockTransport::with_responses(vec![
            Ok(HttpResponse {
                status: 429,
                body: Vec::new(),
            }),
            Ok(HttpResponse {
                status: 529,
                body: Vec::new(),
            }),
            Ok(HttpResponse {
                status: 200,
                body: success_body("asset.b"),
            }),
        ]));
        let adapter = adapter(transport.clone());

        let evidence = adapter
            .evaluate_evidence(&request())
            .expect("retry succeeds");
        assert_eq!(evidence.attempts, 3);
        assert_eq!(evidence.selected_candidate_id.as_deref(), Some("asset.b"));
        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|recorded| recorded.timeout <= Duration::from_millis(200))
        );
    }

    #[test]
    fn timeout_and_cancellation_are_typed_and_bounded() {
        let timeout_transport = Arc::new(MockTransport::with_responses(vec![Err(
            TransportError::Timeout,
        )]));
        let timeout_adapter = adapter(timeout_transport);
        let failure = timeout_adapter
            .evaluate_evidence(&request())
            .expect_err("timeout");
        assert_eq!(failure.kind, EngineErrorKind::Timeout);

        let cancelled_transport = Arc::new(MockTransport::with_responses(vec![Ok(HttpResponse {
            status: 200,
            body: success_body("asset.a"),
        })]));
        let cancelled_adapter = adapter(cancelled_transport.clone());
        let token = JevCancellationToken::default();
        token.cancel();
        let failure = cancelled_adapter
            .evaluate_with_cancellation(&request(), &token)
            .expect_err("cancelled");
        assert_eq!(failure.kind, EngineErrorKind::Timeout);
        assert_eq!(cancelled_transport.requests().len(), 0);
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let key = JevApiKey::new("super-secret-key").expect("key");
        assert_eq!(format!("{key:?}"), "JevApiKey([REDACTED])");
        assert!(!format!("{key:?}").contains("super-secret-key"));
    }
}
