//! セキュリティフィクスチャに対する `aivtuber-app` 合成境界の直接エビデンス (#504, #497)。
//!
//! `crates/runtime/tests/security_fixtures.rs` がランタイム単体境界（正規化・キュー・
//! 公開出力ゲート・メモリ許可証）を検証するのに対し、本モジュールでは `ProductionApp`
//! 合成境界上で以下を直接検証する:
//! 1. `oversized-chat-flood.json`: プロバイダ不可 (`jev`/`llm`/`tts` unavailable) かつ
//!    フラッド飽和時でもモデル呼び出しが 0 回 (`requires_model == false`) であり、
//!    認証済みオペレータ停止 (`performer.stop`) が独立して即時実行されること。
//! 2. `prompt-injection-chat.json` / `model-output-forged-authorization.json`:
//!    攻撃ペイロードを含むモデル出力が公開出力ゲートで TTS・ストリーム出力の前に
//!    封じ込められ、`authorized_control` 監査が発生しないこと。
//! 3. `memory-poisoning-chat.json`: 公開チャットとモデル出力から永続メモリへ
//!    視聴者由来の事実や権限昇格が書き込まれないこと。

use super::*;
use crate::operator_control::{OperatorRequest, apply_operator_request};
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, EpistemicClass, LocalControlIngress,
};
use serde_json::Value;

const OPERATOR_SECRET: [u8; 32] = [0x5B; 32];

fn security_fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/security")
        .join(name);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("failed to parse fixture {}: {err}", path.display()))
}

fn operator_ingress() -> LocalControlIngress {
    LocalControlIngress::new(
        "operator-control",
        "operator:local",
        AuthorizationMethod::SignedLocalApi,
        [Capability::PerformerStop, Capability::PerformerMute]
            .into_iter()
            .collect(),
        ControlSecret::new(OPERATOR_SECRET),
    )
    .expect("operator ingress")
}

#[derive(Clone, Default)]
struct CountingUnavailableThinking {
    calls: Arc<AtomicUsize>,
}

impl ThinkingEngine for CountingUnavailableThinking {
    fn generate<'a>(
        &'a self,
        _request: &'a aivtuber_domain::ThinkingRequest,
    ) -> EngineFuture<'a, GeneratedReply> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "thinking provider unavailable",
            ))
        })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "counting-unavailable-thinking".to_owned(),
            model_alias: Some("unavailable".to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

#[derive(Clone, Default)]
struct CountingUnavailableTts {
    calls: Arc<AtomicUsize>,
}

impl TtsEngine for CountingUnavailableTts {
    fn synthesize<'a>(&'a self, _request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "tts provider unavailable",
            ))
        })
    }

    fn identity(&self) -> TtsBackendIdentity {
        TtsBackendIdentity {
            backend: BackendIdentity {
                name: "counting-unavailable-tts".to_owned(),
                model_alias: Some("unavailable".to_owned()),
                model_version: Some("1".to_owned()),
            },
            voice_model: Some("unavailable".to_owned()),
            viseme_mapping: None,
        }
    }
}

#[derive(Clone, Default)]
struct RecordingStreamOutput {
    actions: Arc<Mutex<Vec<AuthorizedStreamAction>>>,
}

impl StreamOutput for RecordingStreamOutput {
    fn execute(&mut self, action: &AuthorizedStreamAction) -> Result<(), EngineError> {
        self.actions
            .lock()
            .expect("stream actions")
            .push(action.clone());
        Ok(())
    }
}

fn chat_event_from_fixture(fixture: &Value, sequence: u64) -> EventEnvelope {
    let input_event = fixture
        .pointer("/input/event")
        .expect("fixture must contain input.event");
    let mut event = chat_event(sequence);
    if let Some(id) = input_event.get("event_id").and_then(Value::as_str) {
        event.event_id = id.to_owned();
    }
    if let Some(source) = input_event.get("source").and_then(Value::as_str) {
        event.source = source.to_owned();
    }
    if let Some(actor_id) = input_event.get("actor_id").and_then(Value::as_str) {
        event.actor_id = Some(actor_id.to_owned());
    }
    if let Some(payload) = input_event.get("payload").and_then(Value::as_object) {
        event.payload = payload
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    }
    event
}

fn build_flood_event_bytes(sequence: u64, target_bytes: usize) -> Vec<u8> {
    let mut event = chat_event(sequence);
    event.event_id = format!("evt-flood-{sequence:04}");
    event.actor_id = Some(format!("viewer-flood-{}", sequence % 17));
    event.payload.insert("text".to_owned(), json!(""));
    let base_len = serde_json::to_vec(&event).expect("base flood json").len();
    let pad_len = target_bytes.saturating_sub(base_len);
    event
        .payload
        .insert("text".to_owned(), json!("x".repeat(pad_len)));
    let raw = serde_json::to_vec(&event).expect("padded flood json");
    assert_eq!(raw.len(), target_bytes);
    raw
}

#[test]
fn app_flood_fixture_with_unavailable_providers_invokes_zero_models_and_allows_authenticated_stop()
{
    let fixture = security_fixture("oversized-chat-flood.json");
    assert_eq!(
        fixture.pointer("/input/services/jev"),
        Some(&json!("unavailable"))
    );
    assert_eq!(
        fixture.pointer("/input/services/llm"),
        Some(&json!("unavailable"))
    );
    assert_eq!(
        fixture.pointer("/input/services/tts"),
        Some(&json!("unavailable"))
    );
    assert_eq!(
        fixture.pointer("/expected/requires_model"),
        Some(&json!(false))
    );

    let repeat_count = fixture
        .pointer("/input/flood/repeat_count")
        .and_then(Value::as_u64)
        .expect("repeat_count") as usize;
    let window_seconds = fixture
        .pointer("/input/flood/window_seconds")
        .and_then(Value::as_u64)
        .expect("window_seconds");
    let payload_bytes = fixture
        .pointer("/input/flood/payload_bytes")
        .and_then(Value::as_u64)
        .expect("payload_bytes") as usize;
    assert_eq!(repeat_count, 500);
    assert_eq!(window_seconds, 1);
    assert_eq!(payload_bytes, 4096);

    let thinking = CountingUnavailableThinking::default();
    let thinking_calls = Arc::clone(&thinking.calls);
    let tts = CountingUnavailableTts::default();
    let tts_calls = Arc::clone(&tts.calls);
    let stream = RecordingStreamOutput::default();
    let stream_actions = Arc::clone(&stream.actions);

    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..SchedulerConfig::default()
        },
        SecretRedactor::default(),
        Some("reaction.agree.01".to_owned()),
    )
    .expect("security runtime");

    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        FixedAssetRoute("reaction.agree.01"),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(stream),
        250,
    )
    .with_generation(generative_runtime(thinking, tts));
    app.startup().expect("startup");

    // 500 件 / 1 秒 / 4096B の合成フラッドをコンテンツ入口に投入する。
    let window_ms = window_seconds * 1_000;
    let mut queued = 0usize;
    let mut dropped_rate = 0usize;
    let mut dropped_backpressure = 0usize;
    for index in 0..repeat_count {
        let now_ms = (index as u64 * window_ms) / repeat_count as u64;
        let raw = build_flood_event_bytes(index as u64 + 1, payload_bytes);
        match app
            .security
            .admit_content_bytes(&raw, now_ms)
            .expect("admit flood bytes")
        {
            ContentAdmitDecision::Queued => queued += 1,
            ContentAdmitDecision::DroppedRate => dropped_rate += 1,
            ContentAdmitDecision::DroppedBackpressure => dropped_backpressure += 1,
            other => panic!("unexpected decision under 4096B flood: {other:?}"),
        }
    }

    assert!(queued > 0, "一部イベントは受理されるべき");
    assert!(
        dropped_rate + dropped_backpressure >= 400,
        "デフォルト境界でフラッドの大半が遮断されるべき (rate={dropped_rate}, bp={dropped_backpressure})"
    );
    assert!(
        app.security.content_len() <= SecurityRuntimeConfig::default().content_queue_limit,
        "コンテンツキューは有界上限を超えないこと"
    );

    // 受理済みイベントを決定論的アセット経路で処理しても、不可状態のプロバイダは一切呼ばれない。
    // 短時間の連続入力ではスケジューラのクールダウンによる抑制も正常な決定論的挙動である。
    let mut processed = 0usize;
    while let Some(event) = app.security.pop_content() {
        processed += 1;
        let raw = serde_json::to_vec(&event).expect("re-serialize queued event");
        match app.process_content_bytes(&raw, 1_000 + processed as u64, processed as u64) {
            Ok(_) => {}
            Err(AppError::Playback(_)) => {}
            Err(other) => panic!("unexpected error on deterministic asset route: {other:?}"),
        }
    }
    assert_eq!(processed, queued);
    assert_eq!(
        thinking_calls.load(Ordering::SeqCst),
        0,
        "requires_model == false 経路で ThinkingEngine は呼ばれないこと"
    );
    assert_eq!(
        tts_calls.load(Ordering::SeqCst),
        0,
        "requires_model == false 経路で TtsEngine は呼ばれないこと"
    );
    assert!(
        stream_actions.lock().expect("stream actions").is_empty(),
        "チャットフラッドでストリーム制御アクションが実行されないこと"
    );

    // コンテンツキューを再度飽和させた直後でも、認証済みオペレータ停止は独立して即時実行される。
    for index in 0..SecurityRuntimeConfig::default().content_queue_limit + 4 {
        let raw = build_flood_event_bytes(10_000 + index as u64, 512);
        let _ = app.security.admit_content_bytes(&raw, 10_000);
    }
    let ingress = operator_ingress();
    let stop_request = OperatorRequest {
        action: "stop".to_owned(),
        secret: String::from_utf8(OPERATOR_SECRET.to_vec()).expect("utf8 secret"),
        request_id: Some("req-flood-stop".to_owned()),
        argument: None,
    };
    let response = apply_operator_request(&ingress, &mut app, &stop_request, 10_050);
    assert!(
        response.ok,
        "飽和時でも認証済み stop は成功すべき: {}",
        response.detail
    );
    assert!(
        app.security()
            .audit()
            .iter()
            .any(|record| record.decision == "authorized_control"),
        "認証済みオペレータ停止が監査ログに記録されるべき"
    );
}

#[test]
fn app_model_output_forged_authorization_and_prompt_injection_are_contained_before_tts_and_stream()
{
    for fixture_name in [
        "prompt-injection-chat.json",
        "model-output-forged-authorization.json",
    ] {
        let fixture = security_fixture(fixture_name);
        let model_output_text = serde_json::to_string(
            fixture
                .get("model_output")
                .expect("fixture must have model_output"),
        )
        .expect("serialize model_output");

        for cached_fallback in [None, Some("reaction.agree.01".to_owned())] {
            let tts = MockTts::default();
            let tts_texts = Arc::clone(&tts.texts);
            let stream = RecordingStreamOutput::default();
            let stream_actions = Arc::clone(&stream.actions);
            let security = SecurityRuntime::new(
                SecurityRuntimeConfig::default(),
                SchedulerConfig {
                    min_reaction_spacing_ms: 0,
                    ..SchedulerConfig::default()
                },
                SecretRedactor::default(),
                cached_fallback.clone(),
            )
            .expect("security runtime");

            let mut app = ProductionApp::new(
                security,
                performer(),
                LocalVisemeStore::new(pack_root()),
                FixedGenerateRoute {
                    reply_context: ReflexContext::default(),
                    fallback_variant_group: None,
                },
                Box::new(RecordingAudio::default()),
                Box::new(RecordingAvatar::default()),
                Box::new(stream),
                250,
            )
            .with_generation(generative_runtime(
                MockThinking {
                    reply: model_output_text.clone(),
                    failure: None,
                },
                tts,
            ));
            app.startup().expect("startup");

            let event = chat_event_from_fixture(&fixture, 101);
            let raw = serde_json::to_vec(&event).expect("event json");
            app.process_content_bytes(&raw, 0, 101)
                .expect("submit generation");
            wait_for_generation_completion(&mut app, 0);

            assert!(
                tts_texts.lock().expect("tts log").is_empty(),
                "{fixture_name} (cached={cached_fallback:?}): 制御語彙を含むモデル出力は TTS 前に遮断されるべき"
            );
            assert!(
                stream_actions.lock().expect("stream actions").is_empty(),
                "{fixture_name} (cached={cached_fallback:?}): モデル出力から OBS/ストリーム制御が実行されないこと"
            );
            let authorized_controls = app
                .security()
                .audit()
                .iter()
                .filter(|record| record.decision == "authorized_control")
                .count();
            assert_eq!(
                authorized_controls, 0,
                "{fixture_name} (cached={cached_fallback:?}): モデル出力で authorized_control が発生しないこと"
            );
        }
    }
}

#[test]
fn app_memory_poisoning_fixture_never_writes_durable_memory() {
    let fixture = security_fixture("memory-poisoning-chat.json");
    let model_output_text = fixture
        .get("model_output")
        .and_then(Value::as_str)
        .expect("memory-poisoning model_output string")
        .to_owned();
    let event = chat_event_from_fixture(&fixture, 202);
    let raw = serde_json::to_vec(&event).expect("event json");

    // 1. デフォルト構成（memory writer OFF）では一切永続メモリへ書き込まれない。
    let mut default_app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        MockThinking {
            reply: model_output_text.clone(),
            failure: None,
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    default_app.startup().expect("startup");

    default_app
        .process_content_bytes(&raw, 0, 202)
        .expect("submit generation");
    wait_for_generation_completion(&mut default_app, 0);

    assert!(
        default_app
            .adaptation()
            .expect("adaptation")
            .memory()
            .entries()
            .is_empty(),
        "デフォルト構成では公開チャットとモデル出力から永続メモリへ一切書き込まれないこと"
    );

    // 2. memory writer を明示的に有効化しても、視聴者属性の主張や VerifiedFact 権限昇格は書き込まれない。
    let mut enabled_app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        MockThinking {
            reply: model_output_text,
            failure: None,
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()))
    .with_memory_writer(MemoryWriterConfig::default())
    .expect("valid writer config");
    enabled_app.startup().expect("startup");

    enabled_app
        .process_content_bytes(&raw, 0, 202)
        .expect("submit generation");
    wait_for_generation_completion(&mut enabled_app, 0);

    for entry in enabled_app
        .adaptation()
        .expect("adaptation")
        .memory()
        .entries()
    {
        assert!(
            entry.source.pseudonymous_actor_id.is_none(),
            "視聴者 (viewer-42) に紐づく記憶が書き込まれないこと"
        );
        assert_ne!(
            entry.epistemic_class,
            EpistemicClass::VerifiedFact,
            "公開チャット/モデル出力から VerifiedFact へ昇格しないこと"
        );
        assert!(
            !entry.normalized_claim.contains("admin")
                && !entry.normalized_claim.contains("trusted-user"),
            "権限昇格主張が記憶へ取り込まれないこと: {}",
            entry.normalized_claim
        );
    }
}
