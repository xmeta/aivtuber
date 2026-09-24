use aivtuber_adapters::{
    AdapterClock, AudioPlayRequest, AudioPlayer, HttpResponse as AdapterHttpResponse,
    HttpTransportError, JsonHttpTransport, NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig,
    ObsWebSocketAdapter, ObsWebSocketConfig, OpenAiResponsesAdapter, OpenAiResponsesConfig,
    ProcessAudioConfig, ProcessAudioPlayer, SecretString, TransportError as WsTransportError,
    VTubeStudioAdapter, VTubeStudioConfig, WebSocketConnector, WebSocketTransport,
};
use aivtuber_asset_store::load_asset_file;
use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EngineErrorKind, EventEnvelope, EventKind, PrivacyClass, ReflexContext,
    ReflexRequest, RetrievalCandidateContext, RetrievalSnapshot, SecurityPlane, SourceClass,
    SpeechRequest, ThinkingRequest, TrustLevel,
};
use aivtuber_hardening::{FaultInjector, FaultKind, FaultPlan, FaultSpec, FaultSubsystem};
use aivtuber_reflex::{
    HttpResponse as JevHttpResponse, HttpTransport as JevHttpTransport, IndexedAsset, JevAdapter,
    JevAdapterConfig, JevApiKey, RetrievalMetadata, SemanticIndex, SimilarityMetric,
    TransportError as JevTransportError,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
struct FaultingJsonTransport {
    injector: Arc<Mutex<FaultInjector>>,
    subsystem: FaultSubsystem,
}

impl JsonHttpTransport for FaultingJsonTransport {
    fn post_json(
        &self,
        _endpoint: &str,
        _bearer_token: Option<&str>,
        _body: &[u8],
        _timeout: Duration,
    ) -> Result<AdapterHttpResponse, HttpTransportError> {
        let kind = self
            .injector
            .lock()
            .expect("fault injector")
            .next(self.subsystem);
        match kind {
            Some(FaultKind::Timeout) => Err(HttpTransportError::Timeout),
            Some(FaultKind::RateLimited) => Ok(AdapterHttpResponse {
                status: 429,
                body: b"{}".to_vec(),
                content_type: Some("application/json".to_owned()),
            }),
            Some(_) => Err(HttpTransportError::Unavailable(
                "injected HTTP failure".to_owned(),
            )),
            None => Err(HttpTransportError::Unavailable(
                "unscripted HTTP call".to_owned(),
            )),
        }
    }
}

#[derive(Clone)]
struct FaultingJevTransport {
    injector: Arc<Mutex<FaultInjector>>,
}

impl JevHttpTransport for FaultingJevTransport {
    fn post_json(
        &self,
        _endpoint: &str,
        _bearer_token: &str,
        _body: &[u8],
        _timeout: Duration,
    ) -> Result<JevHttpResponse, JevTransportError> {
        match self
            .injector
            .lock()
            .expect("fault injector")
            .next(FaultSubsystem::Jev)
        {
            Some(FaultKind::Timeout) => Err(JevTransportError::Timeout),
            Some(FaultKind::RateLimited) => Ok(JevHttpResponse {
                status: 429,
                body: Vec::new(),
            }),
            Some(_) => Err(JevTransportError::Unavailable(
                "injected Jev failure".to_owned(),
            )),
            None => Err(JevTransportError::Unavailable(
                "unscripted Jev call".to_owned(),
            )),
        }
    }
}

#[derive(Clone)]
struct FaultingConnector {
    injector: Arc<Mutex<FaultInjector>>,
    subsystem: FaultSubsystem,
}

impl WebSocketConnector for FaultingConnector {
    fn connect(&self, _endpoint: &str) -> Result<Box<dyn WebSocketTransport>, WsTransportError> {
        let kind = self
            .injector
            .lock()
            .expect("fault injector")
            .next(self.subsystem);
        Err(WsTransportError::new(format!(
            "injected websocket fault: {kind:?}"
        )))
    }
}

#[derive(Debug)]
struct FixedClock;

impl AdapterClock for FixedClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

fn event() -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: "evt-hardening-fault".to_owned(),
        correlation_id: "corr-hardening-fault".to_owned(),
        sequence: 1,
        observed_at: "2026-09-25T00:00:00Z".to_owned(),
        source: "hardening-chat".to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: Some("viewer:test".to_owned()),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([("text".to_owned(), Value::String("hello".to_owned()))]),
    }
}

fn thinking_request() -> ThinkingRequest {
    let event = event();
    ThinkingRequest::from_event(
        &event,
        "curated viewer message",
        PrivacyClass::Pseudonymous,
        ReflexContext::default(),
        RetrievalSnapshot {
            candidates: vec![RetrievalCandidateContext {
                asset_id: "reaction.agree.01".to_owned(),
                rank: 1,
                similarity: 0.91,
            }],
        },
        Vec::new(),
    )
}

fn reflex_request() -> ReflexRequest {
    let mut request = ReflexRequest::new(event(), ReflexContext::default());
    request.retrieval = RetrievalSnapshot {
        candidates: vec![RetrievalCandidateContext {
            asset_id: "reaction.agree.01".to_owned(),
            rank: 1,
            similarity: 0.91,
        }],
    };
    request
}

#[test]
fn model_and_tts_faults_are_independently_injected_and_typed() {
    let plan = FaultPlan::new(
        11,
        vec![
            FaultSpec {
                subsystem: FaultSubsystem::Jev,
                occurrence: 1,
                kind: FaultKind::RateLimited,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Thinking,
                occurrence: 1,
                kind: FaultKind::Timeout,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Tts,
                occurrence: 1,
                kind: FaultKind::Unavailable,
            },
        ],
    )
    .expect("fault plan");
    let injector = Arc::new(Mutex::new(plan.injector()));

    let jev = JevAdapter::with_transport(
        JevAdapterConfig {
            deadline: Duration::from_millis(50),
            initial_backoff: Duration::ZERO,
            max_attempts: 1,
            ..JevAdapterConfig::default()
        },
        JevApiKey::new("hardening-jev-key").expect("jev key"),
        Arc::new(FaultingJevTransport {
            injector: Arc::clone(&injector),
        }),
    )
    .expect("jev adapter");
    let failure = jev
        .evaluate_evidence(&reflex_request())
        .expect_err("Jev rate limit");
    assert_eq!(failure.kind, EngineErrorKind::RateLimited);

    let thinking = OpenAiResponsesAdapter::with_transport(
        OpenAiResponsesConfig::default(),
        SecretString::new("hardening-openai-key"),
        Arc::new(FaultingJsonTransport {
            injector: Arc::clone(&injector),
            subsystem: FaultSubsystem::Thinking,
        }),
    )
    .expect("thinking adapter");
    let error = thinking
        .generate_sync(&thinking_request())
        .expect_err("thinking timeout");
    assert_eq!(error.kind, EngineErrorKind::Timeout);

    let tts = NormalizedHttpTtsAdapter::with_transport(
        NormalizedHttpTtsConfig {
            endpoint: "https://tts.invalid/v1/synthesize".to_owned(),
            backend_name: "hardening-tts".to_owned(),
            model_alias: Some("tts-test".to_owned()),
            model_version: Some("v1".to_owned()),
            voice_model: Some("voice-ja-v2".to_owned()),
            viseme_mapping: Some("ja-5vowel-v2".to_owned()),
            timeout: Duration::from_millis(50),
        },
        None,
        Arc::new(FaultingJsonTransport {
            injector: Arc::clone(&injector),
            subsystem: FaultSubsystem::Tts,
        }),
    )
    .expect("tts adapter");
    let error = tts
        .synthesize_sync(&SpeechRequest {
            text: "hello".to_owned(),
            style: None,
        })
        .expect_err("TTS unavailable");
    assert_eq!(error.kind, EngineErrorKind::Unavailable);

    let consumed = injector.lock().expect("injector").consumed().to_vec();
    assert_eq!(consumed.len(), 3);
    assert_eq!(consumed[0].subsystem, FaultSubsystem::Jev);
    assert_eq!(consumed[1].subsystem, FaultSubsystem::Thinking);
    assert_eq!(consumed[2].subsystem, FaultSubsystem::Tts);
}

#[test]
fn websocket_and_audio_faults_are_independent() {
    let plan = FaultPlan::new(
        12,
        vec![
            FaultSpec {
                subsystem: FaultSubsystem::VTubeStudio,
                occurrence: 1,
                kind: FaultKind::Disconnect,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Obs,
                occurrence: 1,
                kind: FaultKind::Disconnect,
            },
            FaultSpec {
                subsystem: FaultSubsystem::Audio,
                occurrence: 1,
                kind: FaultKind::Unavailable,
            },
        ],
    )
    .expect("fault plan");
    let injector = Arc::new(Mutex::new(plan.injector()));
    let clock: Arc<dyn AdapterClock> = Arc::new(FixedClock);

    let vts = VTubeStudioAdapter::with_dependencies(
        VTubeStudioConfig::default(),
        Arc::new(FaultingConnector {
            injector: Arc::clone(&injector),
            subsystem: FaultSubsystem::VTubeStudio,
        }),
        Arc::clone(&clock),
    )
    .expect("VTS adapter");
    assert_eq!(
        vts.connect().expect_err("VTS disconnect").kind,
        EngineErrorKind::Unavailable
    );

    let obs = ObsWebSocketAdapter::with_dependencies(
        ObsWebSocketConfig::default(),
        Arc::new(FaultingConnector {
            injector: Arc::clone(&injector),
            subsystem: FaultSubsystem::Obs,
        }),
        clock,
    )
    .expect("OBS adapter");
    assert_eq!(
        obs.connect().expect_err("OBS disconnect").kind,
        EngineErrorKind::Unavailable
    );

    assert_eq!(
        injector
            .lock()
            .expect("injector")
            .next(FaultSubsystem::Audio),
        Some(FaultKind::Unavailable)
    );
    let audio = ProcessAudioPlayer::new(ProcessAudioConfig {
        root: PathBuf::from("target/hardening-missing-audio-root"),
        ..ProcessAudioConfig::default()
    })
    .expect("audio adapter");
    let error = audio
        .play(&AudioPlayRequest {
            audio_ref: "audio://missing.opus".to_owned(),
            speed_factor: 1.0,
        })
        .expect_err("audio unavailable");
    assert_eq!(error.kind, EngineErrorKind::Unavailable);
}

#[test]
fn corrupted_asset_and_semantic_index_faults_are_detected() {
    let plan = FaultPlan::new(
        13,
        vec![
            FaultSpec {
                subsystem: FaultSubsystem::AssetStore,
                occurrence: 1,
                kind: FaultKind::Corrupted,
            },
            FaultSpec {
                subsystem: FaultSubsystem::SemanticIndex,
                occurrence: 1,
                kind: FaultKind::Incompatible,
            },
        ],
    )
    .expect("fault plan");
    let mut injector = plan.injector();

    assert_eq!(
        injector.next(FaultSubsystem::AssetStore),
        Some(FaultKind::Corrupted)
    );
    let invalid = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/performance-assets/invalid/missing-compiler-version.json");
    assert!(load_asset_file(invalid).is_err());

    assert_eq!(
        injector.next(FaultSubsystem::SemanticIndex),
        Some(FaultKind::Incompatible)
    );
    let result = SemanticIndex::new(
        RetrievalMetadata {
            retriever_version: "hardening-v1".to_owned(),
            embedding_model: "embed@test".to_owned(),
            index_version: "index-hardening".to_owned(),
            asset_compiler_version: "0.1.0".to_owned(),
            similarity_metric: SimilarityMetric::Cosine,
            tie_break_rule: "asset_id".to_owned(),
        },
        vec![
            IndexedAsset {
                asset_id: "asset.a".to_owned(),
                asset_identity: None,
                embedding: vec![1.0, 0.0],
            },
            IndexedAsset {
                asset_id: "asset.b".to_owned(),
                asset_identity: None,
                embedding: vec![1.0, 0.0, 0.0],
            },
        ],
    );
    assert!(result.is_err());
}
