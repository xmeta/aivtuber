use crate::{
    JevAdapter, JevUsage, NormalizedAnswer, RetrievalError, RetrievalResult, SemanticIndex,
};
use aivtuber_domain::{
    AttentionTarget, BackendIdentity, EngineError, EngineErrorKind, FallbackReason,
    MAX_RETRIEVAL_CANDIDATES, REFLEX_SCHEMA_VERSION, ReflexDecision, ReflexRequest, ResponseRoute,
    RetrievalCandidateContext, RetrievalSnapshot, RouteDecision,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEvidence {
    pub requested_model: String,
    pub returned_model: Option<String>,
    pub usage: Option<JevUsage>,
    pub attempts: u32,
    pub latency_ms: f64,
    pub answers: BTreeMap<String, NormalizedAnswer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionEvidence {
    pub retrieval: RetrievalResult,
    pub normalized: ReflexDecision,
    pub selected_candidate_id: Option<String>,
    pub model: ModelEvidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub route: ResponseRoute,
    pub selected_candidate_id: Option<String>,
    pub selected_candidate_identity: Option<String>,
    pub fallback_reason: FallbackReason,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutedAction {
    Silent,
    Reaction,
    Cached,
    Template,
    Llm,
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutedDecision {
    pub action: ExecutedAction,
    pub asset_id: Option<String>,
    pub asset_identity: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionReplayRecord {
    pub event_id: String,
    pub request_schema_version: String,
    pub context_schema_version: String,
    pub evidence: DecisionEvidence,
    pub policy: PolicyDecision,
    pub executed: ExecutedDecision,
}

impl DecisionReplayRecord {
    /// Full audit/replay record, including measured observational latency.
    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("decision replay record is JSON-safe")
    }

    /// Stable decision fingerprint. Observational wall-clock latency is
    /// normalized because it is recorded metadata, not decision input.
    pub fn deterministic_bytes(&self) -> Vec<u8> {
        let mut normalized = self.clone();
        normalized.evidence.normalized.latency_ms = 0.0;
        normalized.evidence.model.latency_ms = 0.0;
        serde_json::to_vec(&normalized).expect("decision replay record is JSON-safe")
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolicyConfig {
    pub reuse_threshold: f64,
    pub min_route_confidence: f64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            reuse_threshold: 0.85,
            min_route_confidence: 0.5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReflexPipelineInput {
    pub request: ReflexRequest,
    pub query_embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct ReflexPipeline {
    index: SemanticIndex,
    adapter: JevAdapter,
    policy: PolicyConfig,
    top_k: usize,
}

impl ReflexPipeline {
    pub fn new(
        index: SemanticIndex,
        adapter: JevAdapter,
        policy: PolicyConfig,
        top_k: usize,
    ) -> Result<Self, EngineError> {
        if top_k == 0 || top_k > MAX_RETRIEVAL_CANDIDATES {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("reflex Top-K must be in 1..={MAX_RETRIEVAL_CANDIDATES}"),
            ));
        }
        if !(0.0..=1.0).contains(&policy.reuse_threshold)
            || !(0.0..=1.0).contains(&policy.min_route_confidence)
        {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "reflex policy thresholds must be in 0..=1",
            ));
        }
        Ok(Self {
            index,
            adapter,
            policy,
            top_k,
        })
    }
    /// Event-driven reflex path. This is intentionally not a frame-loop API.
    pub fn run(
        &self,
        mut input: ReflexPipelineInput,
    ) -> Result<DecisionReplayRecord, RetrievalError> {
        let retrieval = self.index.search(&input.query_embedding, self.top_k)?;
        input.request.retrieval = RetrievalSnapshot {
            candidates: retrieval
                .candidates
                .iter()
                .map(|candidate| RetrievalCandidateContext {
                    asset_id: candidate.asset_id.clone(),
                    rank: candidate.rank,
                    similarity: candidate.similarity,
                })
                .collect(),
        };

        let evidence = match self.adapter.evaluate_evidence(&input.request) {
            Ok(model) => DecisionEvidence {
                retrieval,
                selected_candidate_id: model.selected_candidate_id.clone(),
                normalized: model.decision,
                model: ModelEvidence {
                    requested_model: model.requested_model,
                    returned_model: Some(model.returned_model),
                    usage: Some(model.usage),
                    attempts: model.attempts,
                    latency_ms: model.latency_ms,
                    answers: model.answers,
                },
            },
            Err(failure) => {
                let fallback_reason = fallback_reason(failure.kind);
                DecisionEvidence {
                    retrieval,
                    selected_candidate_id: None,
                    normalized: deterministic_fallback(fallback_reason, failure.latency_ms),
                    model: ModelEvidence {
                        requested_model: failure.requested_model,
                        returned_model: None,
                        usage: None,
                        attempts: failure.attempts,
                        latency_ms: failure.latency_ms,
                        answers: BTreeMap::new(),
                    },
                }
            }
        };

        let policy = apply_policy(&evidence, self.policy);
        let executed = execute_policy(&policy);
        Ok(DecisionReplayRecord {
            event_id: input.request.event.event_id,
            request_schema_version: input.request.schema_version,
            context_schema_version: input.request.context.schema_version,
            evidence,
            policy,
            executed,
        })
    }
}

pub fn apply_policy(evidence: &DecisionEvidence, config: PolicyConfig) -> PolicyDecision {
    let decision = &evidence.normalized;
    if decision.fallback_reason != FallbackReason::None {
        return PolicyDecision {
            route: ResponseRoute::Silent,
            selected_candidate_id: None,
            selected_candidate_identity: None,
            fallback_reason: decision.fallback_reason,
            reason: "model_or_transport_fallback".to_owned(),
        };
    }

    if decision
        .route
        .confidence
        .is_some_and(|confidence| confidence < config.min_route_confidence)
    {
        return PolicyDecision {
            route: ResponseRoute::Silent,
            selected_candidate_id: None,
            selected_candidate_identity: None,
            fallback_reason: FallbackReason::LowConfidence,
            reason: "route_confidence_below_threshold".to_owned(),
        };
    }
    if decision.route.value == ResponseRoute::Cached {
        if decision.cache_reuse_probability >= config.reuse_threshold
            && evidence.selected_candidate_id.is_some()
        {
            let selected_candidate_identity =
                evidence
                    .selected_candidate_id
                    .as_ref()
                    .and_then(|selected_id| {
                        evidence
                            .retrieval
                            .candidates
                            .iter()
                            .find(|candidate| &candidate.asset_id == selected_id)
                            .and_then(|candidate| candidate.asset_identity.clone())
                    });
            return PolicyDecision {
                route: ResponseRoute::Cached,
                selected_candidate_id: evidence.selected_candidate_id.clone(),
                selected_candidate_identity,
                fallback_reason: FallbackReason::None,
                reason: "explicit_candidate_passed_reuse_gate".to_owned(),
            };
        }
        return PolicyDecision {
            route: ResponseRoute::Silent,
            selected_candidate_id: None,
            selected_candidate_identity: None,
            fallback_reason: FallbackReason::PolicyOverride,
            reason: "cached_route_failed_explicit_reuse_gate".to_owned(),
        };
    }

    PolicyDecision {
        route: decision.route.value,
        selected_candidate_id: None,
        selected_candidate_identity: None,
        fallback_reason: FallbackReason::None,
        reason: "route_accepted".to_owned(),
    }
}

pub fn execute_policy(policy: &PolicyDecision) -> ExecutedDecision {
    let action = if policy.fallback_reason != FallbackReason::None {
        ExecutedAction::Fallback
    } else {
        match policy.route {
            ResponseRoute::Silent => ExecutedAction::Silent,
            ResponseRoute::Reaction => ExecutedAction::Reaction,
            ResponseRoute::Cached => ExecutedAction::Cached,
            ResponseRoute::Template => ExecutedAction::Template,
            ResponseRoute::Llm => ExecutedAction::Llm,
        }
    };

    ExecutedDecision {
        action,
        asset_id: (action == ExecutedAction::Cached)
            .then(|| policy.selected_candidate_id.clone())
            .flatten(),
        asset_identity: (action == ExecutedAction::Cached)
            .then(|| policy.selected_candidate_identity.clone())
            .flatten(),
    }
}

fn fallback_reason(kind: EngineErrorKind) -> FallbackReason {
    match kind {
        EngineErrorKind::Timeout => FallbackReason::Timeout,
        EngineErrorKind::Unavailable | EngineErrorKind::Backend => FallbackReason::Unavailable,
        EngineErrorKind::RateLimited => FallbackReason::RateLimited,
        EngineErrorKind::Overloaded => FallbackReason::Overloaded,
        EngineErrorKind::Authentication | EngineErrorKind::Unauthorized => {
            FallbackReason::Authentication
        }
        EngineErrorKind::InvalidRequest => FallbackReason::InvalidRequest,
    }
}
fn deterministic_fallback(reason: FallbackReason, latency_ms: f64) -> ReflexDecision {
    ReflexDecision {
        schema_version: REFLEX_SCHEMA_VERSION.to_owned(),
        route: RouteDecision {
            value: ResponseRoute::Silent,
            confidence: None,
        },
        reaction_family: None,
        gesture_family: None,
        attention_target: AttentionTarget::Away,
        interrupt_probability: 0.0,
        cache_reuse_probability: 0.0,
        importance: 0.1,
        emotion_intensity: 0.0,
        backend: BackendIdentity {
            name: "deterministic".to_owned(),
            model_alias: None,
            model_version: None,
        },
        latency_ms,
        fallback_reason: reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HttpResponse, HttpTransport, IndexedAsset, JevAdapterConfig, JevApiKey, RetrievalMetadata,
        SimilarityMetric, TransportError,
    };
    use aivtuber_domain::{
        EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, SecurityPlane, SourceClass, TrustLevel,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Debug)]
    struct QueueTransport {
        responses: Mutex<Vec<Result<HttpResponse, TransportError>>>,
    }

    impl QueueTransport {
        fn new(mut responses: Vec<Result<HttpResponse, TransportError>>) -> Self {
            responses.reverse();
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl HttpTransport for QueueTransport {
        fn post_json(
            &self,
            _endpoint: &str,
            _bearer_token: &str,
            _body: &[u8],
            _timeout: Duration,
        ) -> Result<HttpResponse, TransportError> {
            self.responses
                .lock()
                .expect("responses")
                .pop()
                .unwrap_or_else(|| Err(TransportError::Unavailable("empty script".to_owned())))
        }
    }

    fn metadata() -> RetrievalMetadata {
        RetrievalMetadata {
            retriever_version: "semantic-v1".to_owned(),
            embedding_model: "embed-v1".to_owned(),
            index_version: "index-v7".to_owned(),
            asset_compiler_version: "0.1.0".to_owned(),
            similarity_metric: SimilarityMetric::Cosine,
            tie_break_rule: "similarity_desc_then_asset_id_asc".to_owned(),
        }
    }
    fn index() -> SemanticIndex {
        SemanticIndex::new(
            metadata(),
            vec![
                IndexedAsset {
                    asset_id: "asset.a".to_owned(),
                    asset_identity: Some("identity-a".to_owned()),
                    embedding: vec![1.0, 0.0],
                },
                IndexedAsset {
                    asset_id: "asset.b".to_owned(),
                    asset_identity: None,
                    embedding: vec![0.8, 0.2],
                },
                IndexedAsset {
                    asset_id: "asset.c".to_owned(),
                    asset_identity: None,
                    embedding: vec![0.0, 1.0],
                },
            ],
        )
        .expect("index")
    }

    fn request() -> ReflexRequest {
        ReflexRequest::new(
            EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION.to_owned(),
                event_id: "evt-pipeline".to_owned(),
                correlation_id: "corr-pipeline".to_owned(),
                sequence: 1,
                observed_at: "2026-09-24T00:00:00Z".to_owned(),
                source: "chat".to_owned(),
                source_class: SourceClass::PublicChat,
                plane: SecurityPlane::Content,
                trust_level: TrustLevel::Untrusted,
                kind: EventKind::ChatMessage,
                actor_id: None,
                priority_hint: None,
                authorization: None,
                payload: BTreeMap::from([(
                    "text".to_owned(),
                    Value::String("that was surprising".to_owned()),
                )]),
            },
            aivtuber_domain::ReflexContext::default(),
        )
    }

    fn choice(value: &str) -> Value {
        json!({
            "type": "choice",
            "choice": value,
            "confidence": 0.9,
            "probabilities": {"primary": 0.9, "other": 0.1}
        })
    }
    fn success(cache_reuse: f64, selected: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: serde_json::to_vec(&json!({
                "model": "jev-concrete-v1",
                "answers": {
                    "response_route": choice("cached"),
                    "reaction_family": choice("surprise"),
                    "gesture_family": choice("nod_small"),
                    "attention_target": choice("camera"),
                    "interrupt": {"type": "noul", "noul": 0.1},
                    "cache_reuse": {"type": "noul", "noul": cache_reuse},
                    "importance": {
                        "type": "score", "score": 0.5, "confidence": 0.8,
                        "legend": {"0": "low", "1": "high"},
                        "probabilities": {"0": 0.5, "1": 0.5}
                    },
                    "emotion_intensity": {
                        "type": "score", "score": 0.7, "confidence": 0.8,
                        "legend": {"0": "low", "1": "high"},
                        "probabilities": {"0": 0.3, "1": 0.7}
                    },
                    "selected_candidate": choice(selected)
                },
                "usage": {"input_tokens": 90, "output_tokens": 9}
            }))
            .expect("body"),
        }
    }

    fn pipeline(responses: Vec<Result<HttpResponse, TransportError>>) -> ReflexPipeline {
        let config = JevAdapterConfig {
            deadline: Duration::from_millis(100),
            initial_backoff: Duration::ZERO,
            ..JevAdapterConfig::default()
        };
        let adapter = JevAdapter::with_transport(
            config,
            JevApiKey::new("test-key").expect("key"),
            Arc::new(QueueTransport::new(responses)),
        )
        .expect("adapter");
        ReflexPipeline::new(index(), adapter, PolicyConfig::default(), 2).expect("pipeline")
    }
    #[test]
    fn evidence_policy_and_execution_are_recorded_as_separate_stages() {
        let pipeline = pipeline(vec![Ok(success(0.94, "asset.a"))]);
        let record = pipeline
            .run(ReflexPipelineInput {
                request: request(),
                query_embedding: vec![1.0, 0.0],
            })
            .expect("record");

        assert_eq!(
            record.request_schema_version,
            aivtuber_domain::REFLEX_REQUEST_SCHEMA_VERSION
        );
        assert_eq!(
            record.context_schema_version,
            aivtuber_domain::REFLEX_CONTEXT_SCHEMA_VERSION
        );
        assert_eq!(record.evidence.retrieval.candidates[0].asset_id, "asset.a");
        assert_eq!(
            record.evidence.selected_candidate_id.as_deref(),
            Some("asset.a")
        );
        assert_eq!(record.evidence.model.requested_model, "jev-latest");
        assert_eq!(
            record.evidence.model.returned_model.as_deref(),
            Some("jev-concrete-v1")
        );
        assert_eq!(record.evidence.model.usage.expect("usage").input_tokens, 90);

        assert_eq!(record.policy.route, ResponseRoute::Cached);
        assert_eq!(
            record.policy.selected_candidate_id.as_deref(),
            Some("asset.a")
        );
        assert_eq!(record.policy.fallback_reason, FallbackReason::None);

        assert_eq!(record.executed.action, ExecutedAction::Cached);
        assert_eq!(record.executed.asset_id.as_deref(), Some("asset.a"));
        assert_eq!(
            record.executed.asset_identity.as_deref(),
            Some("identity-a")
        );

        let serialized = String::from_utf8(record.to_json_bytes()).expect("utf8");
        assert!(serialized.contains("\"evidence\""));
        assert!(serialized.contains("\"policy\""));
        assert!(serialized.contains("\"executed\""));
        assert!(serialized.contains("\"index_version\":\"index-v7\""));
    }

    #[test]
    fn cached_reuse_requires_explicit_candidate_and_threshold_gate() {
        let low_reuse = pipeline(vec![Ok(success(0.2, "asset.a"))])
            .run(ReflexPipelineInput {
                request: request(),
                query_embedding: vec![1.0, 0.0],
            })
            .expect("record");
        assert_eq!(
            low_reuse.policy.fallback_reason,
            FallbackReason::PolicyOverride
        );
        assert_eq!(low_reuse.executed.action, ExecutedAction::Fallback);

        let no_candidate = pipeline(vec![Ok(success(0.95, "none"))])
            .run(ReflexPipelineInput {
                request: request(),
                query_embedding: vec![1.0, 0.0],
            })
            .expect("record");
        assert_eq!(
            no_candidate.policy.fallback_reason,
            FallbackReason::PolicyOverride
        );
        assert_eq!(no_candidate.executed.asset_id, None);
    }
    #[test]
    fn transport_timeout_becomes_deterministic_fallback_record() {
        let record = pipeline(vec![Err(TransportError::Timeout)])
            .run(ReflexPipelineInput {
                request: request(),
                query_embedding: vec![1.0, 0.0],
            })
            .expect("retrieval still succeeds");

        assert_eq!(
            record.evidence.normalized.fallback_reason,
            FallbackReason::Timeout
        );
        assert_eq!(record.policy.fallback_reason, FallbackReason::Timeout);
        assert_eq!(record.executed.action, ExecutedAction::Fallback);
        assert!(record.evidence.model.returned_model.is_none());
    }

    #[test]
    fn same_inputs_and_model_evidence_serialize_byte_identically() {
        let input = ReflexPipelineInput {
            request: request(),
            query_embedding: vec![1.0, 0.0],
        };
        let first = pipeline(vec![Ok(success(0.94, "asset.a"))])
            .run(input.clone())
            .expect("first");
        let second = pipeline(vec![Ok(success(0.94, "asset.a"))])
            .run(input)
            .expect("second");

        assert_eq!(first.deterministic_bytes(), second.deterministic_bytes());

        let full = String::from_utf8(first.to_json_bytes()).expect("full replay utf8");
        assert!(full.contains("\"latency_ms\""));
    }
}
