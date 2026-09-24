use aivtuber_domain::{
    AttentionTarget, BackendIdentity, EVENT_SCHEMA_VERSION, EventEnvelope, EventKind,
    FallbackReason, REFLEX_SCHEMA_VERSION, ReflexDecision, ReflexRequest, ResponseRoute,
    RouteDecision, SecurityPlane, SourceClass, TrustLevel,
};
use aivtuber_reflex::{
    DecisionEvidence, HttpResponse, HttpTransport, IndexedAsset, JevAdapter, JevAdapterConfig,
    JevApiKey, JevUsage, ModelEvidence, PolicyConfig, ReflexPipeline, ReflexPipelineInput,
    RetrievalMetadata, RetrievalResult, SemanticIndex, SimilarityMetric, TransportError,
    apply_policy,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct StaticTransport {
    body: Vec<u8>,
}

impl HttpTransport for StaticTransport {
    fn post_json(
        &self,
        _endpoint: &str,
        _bearer_token: &str,
        _body: &[u8],
        _timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status: 200,
            body: self.body.clone(),
        })
    }
}

fn metadata() -> RetrievalMetadata {
    RetrievalMetadata {
        retriever_version: "bench-semantic-v1".to_owned(),
        embedding_model: "bench-embed-v1".to_owned(),
        index_version: "bench-index-v1".to_owned(),
        similarity_metric: SimilarityMetric::Cosine,
        tie_break_rule: "similarity_desc_then_asset_id_asc".to_owned(),
    }
}

fn request() -> ReflexRequest {
    ReflexRequest::new(
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: "evt-bench".to_owned(),
            correlation_id: "corr-bench".to_owned(),
            sequence: 1,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: "bench-chat".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: None,
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([(
                "text".to_owned(),
                Value::String("surprising play".to_owned()),
            )]),
        },
        aivtuber_domain::ReflexContext::default(),
    )
}

fn success_body() -> Vec<u8> {
    let choice = |value: &str| {
        json!({
            "type": "choice",
            "choice": value,
            "confidence": 0.9,
            "probabilities": {"primary": 0.9, "other": 0.1}
        })
    };
    serde_json::to_vec(&json!({
        "model": "jev-bench-v1",
        "answers": {
            "response_route": choice("cached"),
            "reaction_family": choice("surprise"),
            "gesture_family": choice("nod_small"),
            "attention_target": choice("camera"),
            "interrupt": {"type": "noul", "noul": 0.1},
            "cache_reuse": {"type": "noul", "noul": 0.95},
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
            "selected_candidate": choice("asset.a")
        },
        "usage": {"input_tokens": 80, "output_tokens": 9}
    }))
    .expect("bench response")
}

fn deterministic_evidence() -> DecisionEvidence {
    DecisionEvidence {
        retrieval: RetrievalResult {
            metadata: metadata(),
            candidates: Vec::new(),
        },
        normalized: ReflexDecision {
            schema_version: REFLEX_SCHEMA_VERSION.to_owned(),
            route: RouteDecision {
                value: ResponseRoute::Reaction,
                confidence: Some(0.9),
            },
            reaction_family: Some("surprise".to_owned()),
            gesture_family: Some("nod_small".to_owned()),
            attention_target: AttentionTarget::Camera,
            interrupt_probability: 0.1,
            cache_reuse_probability: 0.0,
            importance: 0.5,
            emotion_intensity: 0.7,
            backend: BackendIdentity {
                name: "deterministic".to_owned(),
                model_alias: None,
                model_version: None,
            },
            latency_ms: 0.0,
            fallback_reason: FallbackReason::None,
        },
        selected_candidate_id: None,
        model: ModelEvidence {
            requested_model: "none".to_owned(),
            returned_model: None,
            usage: Some(JevUsage {
                input_tokens: 0,
                output_tokens: 0,
            }),
            attempts: 0,
            latency_ms: 0.0,
            answers: BTreeMap::new(),
        },
    }
}

fn full_pipeline() -> ReflexPipeline {
    let index = SemanticIndex::new(
        metadata(),
        vec![
            IndexedAsset {
                asset_id: "asset.a".to_owned(),
                embedding: vec![1.0, 0.0],
            },
            IndexedAsset {
                asset_id: "asset.b".to_owned(),
                embedding: vec![0.8, 0.2],
            },
            IndexedAsset {
                asset_id: "asset.c".to_owned(),
                embedding: vec![0.0, 1.0],
            },
        ],
    )
    .expect("bench index");
    let adapter = JevAdapter::with_transport(
        JevAdapterConfig {
            deadline: Duration::from_millis(100),
            initial_backoff: Duration::ZERO,
            ..JevAdapterConfig::default()
        },
        JevApiKey::new("bench-key").expect("bench key"),
        Arc::new(StaticTransport {
            body: success_body(),
        }),
    )
    .expect("bench adapter");
    ReflexPipeline::new(index, adapter, PolicyConfig::default(), 2).expect("bench pipeline")
}

fn benchmark(label: &str, iterations: u64, mut work: impl FnMut()) {
    let started = Instant::now();
    for _ in 0..iterations {
        work();
    }
    let elapsed = started.elapsed();
    let ns_per_op = elapsed.as_nanos() as f64 / iterations as f64;
    println!("{label}: {ns_per_op:.1} ns/op ({iterations} iterations)");
}

fn main() {
    let evidence = deterministic_evidence();
    benchmark("deterministic-only policy", 100_000, || {
        black_box(apply_policy(black_box(&evidence), PolicyConfig::default()));
    });

    let pipeline = full_pipeline();
    let input = ReflexPipelineInput {
        request: request(),
        query_embedding: vec![1.0, 0.0],
    };
    benchmark(
        "semantic retrieval + Jev adapter (mock transport)",
        10_000,
        || {
            black_box(
                pipeline
                    .run(black_box(input.clone()))
                    .expect("bench pipeline run"),
            );
        },
    );
}
