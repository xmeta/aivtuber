//! #498 / #504 (#497): shared fixture evidence at the actual Rust security boundary.
//! Runtime-level tests cover event admission, memory permit, synthetic flood expansion,
//! public-output containment and authenticated scheduler control. Full app/provider
//! composition evidence lives separately in `aivtuber-app`.
use aivtuber_domain::{
    AuthenticatedControlCommand, AuthorizationMethod, Capability, ControlSecret, EventEnvelope,
    LocalControlIngress, OperatorCommandInput,
};
use aivtuber_runtime::{
    ContentAdmitDecision, ControlOutcome, MemoryWriteDecision, OutputVerdict, RuntimeError,
    SecurityRuntime, SecurityRuntimeConfig,
};
use aivtuber_scheduler::{BlendChannel, PlannedPerformance, Priority, SchedulerConfig};
use aivtuber_telemetry::SecretRedactor;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn runtime() -> SecurityRuntime {
    SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("valid test runtime")
}

// Credentials and capabilities come from the test's trusted configuration,
// never from a fixture's serialized authorization or model output.
fn authenticated(capability: Capability, action: &str) -> AuthenticatedControlCommand {
    let secret = [0x42_u8; 32];
    let ingress = LocalControlIngress::new(
        "fixture-test-operator",
        "operator:fixture-test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([capability]),
        ControlSecret::new(secret),
    )
    .expect("test ingress");
    ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-authenticated-fixture-test".to_owned(),
                correlation_id: "corr-fixture-test".to_owned(),
                sequence: 1,
                observed_at: "2026-09-24T00:00:00Z".to_owned(),
                action: action.to_owned(),
                payload: BTreeMap::new(),
            },
            &secret,
        )
        .expect("verified test credential")
}

#[test]
fn shared_security_fixtures_exercise_rust_event_and_permit_boundaries() {
    // Inventory is deliberately explicit: a newly added or renamed fixture
    // requires a coverage decision rather than silently being skipped.
    let expected = BTreeMap::from([
        (
            "chat-claims-initiative-control.json",
            "security.authorization_forgery.chat_initiative_control",
        ),
        (
            "chat-claims-persona-configure.json",
            "security.authorization_forgery.chat_persona_configure",
        ),
        (
            "chat-claims-session-log-control.json",
            "security.authorization_forgery.chat_session_log_control",
        ),
        (
            "chat-claims-speech-input-control.json",
            "security.authorization_forgery.chat_speech_input_control",
        ),
        (
            "emergency-stop-offline.json",
            "security.emergency_control.stop_offline",
        ),
        (
            "memory-poisoning-chat.json",
            "security.memory_poisoning.viewer_claim",
        ),
        (
            "model-output-forged-authorization.json",
            "security.authorization_forgery.model_claim",
        ),
        (
            "oversized-chat-flood.json",
            "security.resource_abuse.oversized_chat_flood",
        ),
        (
            "prompt-injection-chat.json",
            "security.prompt_injection.chat_obs",
        ),
    ]);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/security");
    let mut seen = BTreeSet::new();
    for entry in std::fs::read_dir(root).expect("security fixture directory") {
        let path = entry.expect("fixture entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap();
        let id = expected.get(name).expect("unclassified security fixture");
        assert!(seen.insert(name.to_owned()));
        let case: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(case["schema_version"], "0.1.0", "{name}");
        assert_eq!(case["id"], *id, "{name}");
        let raw = serde_json::to_vec(&case["input"]["event"]).unwrap();
        let event: EventEnvelope = serde_json::from_slice(&raw).unwrap();
        event.validate().expect("fixture event validates");
        let mut runtime = runtime();

        // No fixture's serialized claim alone is sufficient to mint a permit.
        let (decision, permit) = runtime.authorize_memory_write(&event, None);
        assert_eq!(decision, MemoryWriteDecision::Denied, "{name}");
        assert!(permit.is_none(), "{name}");
        assert_eq!(case["expected"]["memory_write_allowed"], false, "{name}");

        if name == "emergency-stop-offline.json" {
            assert_eq!(
                runtime.admit_content_bytes(&raw, 0).unwrap(),
                ContentAdmitDecision::RejectedNonContent
            );
            assert_eq!(runtime.content_len(), 0);
            assert_eq!(case["input"]["event"]["payload"]["action"], "stop");
            assert_eq!(case["expected"]["control_authorized"], true);
            assert_eq!(case["expected"]["required_action"], "performer.stop");
            runtime
                .scheduler_mut()
                .schedule(PlannedPerformance {
                    event_id: "fixture-audio".to_owned(),
                    asset_id: "fixture-asset".to_owned(),
                    priority: Priority::Conversation,
                    interruptible: false,
                    interrupt_points_ms: vec![0, 1_000],
                    start_at_ms: 0,
                    duration_ms: 1_000,
                    generation: 0,
                    exclusive: false,
                    channels: BTreeSet::from([BlendChannel::Audio]),
                })
                .expect("scheduled audio");
            let stop = authenticated(Capability::PerformerStop, "performer.stop");
            assert_eq!(
                runtime.handle_control(&stop, 100).unwrap(),
                ControlOutcome::Stopped { cancelled: 1 }
            );
            assert!(runtime.scheduler().items().is_empty());
            continue;
        }

        // Even the flood fixture contains a single ordinary-sized event.
        // Do not claim this exercises its repeat_count or payload_bytes metadata.
        assert_eq!(case["expected"]["control_authorized"], false, "{name}");
        assert!(case["expected"]["required_action"].is_null(), "{name}");
        assert_eq!(
            runtime.admit_content_bytes(&raw, 0).unwrap(),
            ContentAdmitDecision::Queued,
            "{name}"
        );
        let admitted = runtime.pop_content().expect("admitted fixture");
        assert!(admitted.authorization.is_none(), "{name}");
        assert!(!runtime.is_muted(), "{name}");
        assert!(runtime.scheduler().items().is_empty(), "{name}");
        assert!(
            !runtime
                .audit()
                .iter()
                .any(|record| record.decision == "authorized_control"),
            "{name}"
        );

        if !case["model_output"]["authorization"].is_null() {
            // Deliberately attack the serialized content ingress; this is NOT
            // a simulation of the real provider/model-output pipeline.
            let mut forged = case["input"]["event"].clone();
            forged["authorization"] = case["model_output"]["authorization"].clone();
            let bytes = serde_json::to_vec(&forged).unwrap();
            assert!(
                matches!(
                    runtime.admit_content_bytes(&bytes, 1),
                    Err(RuntimeError::InvalidEnvelope(_))
                        | Ok(ContentAdmitDecision::RejectedNonContent)
                ),
                "{name}: forged authorization must be rejected"
            );
            assert_eq!(runtime.content_len(), 0, "{name}");
        }
    }
    assert_eq!(
        seen,
        expected.keys().map(|name| (*name).to_owned()).collect()
    );
}

#[test]
fn serialized_memory_admin_does_not_mint_a_permit_but_authenticated_admin_can() {
    let mut runtime = runtime();
    let admin = authenticated(Capability::MemoryAdmin, "memory.admin");
    // A valid serialized control event still carries no authenticated authority.
    let bytes = serde_json::to_vec(admin.event()).unwrap();
    let claim: EventEnvelope = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        runtime.admit_content_bytes(&bytes, 0).unwrap(),
        ContentAdmitDecision::RejectedNonContent
    );
    let (decision, permit) = runtime.authorize_memory_write(&claim, None);
    assert_eq!(decision, MemoryWriteDecision::Denied);
    assert!(permit.is_none());

    let (decision, permit) = runtime.authorize_memory_write(&claim, Some(admin.authority()));
    assert_eq!(decision, MemoryWriteDecision::AllowedMemoryAdmin);
    assert_eq!(
        permit.expect("authenticated permit").event_id(),
        claim.event_id
    );
}

#[test]
fn oversized_chat_flood_fixture_exercises_synthetic_flood_and_payload_bounds() {
    // #504 (#497): oversized-chat-flood.json の flood メタデータ
    // (repeat_count=500, window_seconds=1, payload_bytes=4096) を明示的に合成展開して検証する。
    // 注意: 4096 バイト自体はデフォルト max_payload_bytes (16 KiB) 未満であり、
    // デフォルト構成ではレート制限・キュー上限で有界に保護される。
    // ペイロード上限超過は明示的な上限設定 (4095) と 16 KiB 超過の双方で個別に検証する。
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/security/oversized-chat-flood.json");
    let case: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let repeat_count = case["input"]["flood"]["repeat_count"].as_u64().unwrap() as usize;
    let window_seconds = case["input"]["flood"]["window_seconds"].as_u64().unwrap();
    let payload_bytes = case["input"]["flood"]["payload_bytes"].as_u64().unwrap() as usize;
    assert_eq!(repeat_count, 500);
    assert_eq!(window_seconds, 1);
    assert_eq!(payload_bytes, 4096);

    // 1. デフォルト設定下での 500 件 / 1 秒フラッド展開: レート制限で有界にドロップされる
    let mut rt = runtime();
    let default_cfg = SecurityRuntimeConfig::default();
    assert!(
        payload_bytes < default_cfg.max_payload_bytes,
        "4096 bytes is below the 16 KiB default payload cap"
    );
    let mut queued = 0usize;
    let mut dropped_rate = 0usize;
    let window_ms = window_seconds * 1_000;
    for i in 0..repeat_count {
        let mut ev = case["input"]["event"].clone();
        ev["event_id"] = Value::String(format!("evt-flood-{i:04}"));
        ev["sequence"] = Value::from(100 + i as u64);
        ev["payload"]["text"] = Value::String("A".repeat(payload_bytes - 32));
        let raw = serde_json::to_vec(&ev).unwrap();
        let now_ms = (i as u64 * window_ms) / repeat_count as u64;
        match rt.admit_content_bytes(&raw, now_ms).unwrap() {
            ContentAdmitDecision::Queued => queued += 1,
            ContentAdmitDecision::DroppedRate => dropped_rate += 1,
            other => panic!("unexpected decision during default flood expansion: {other:?}"),
        }
    }
    assert_eq!(queued + dropped_rate, repeat_count);
    assert!(
        queued
            <= default_cfg.content_burst as usize
                + default_cfg.content_rate_per_second as usize
                + 1,
        "queued={queued} must stay bounded by burst + rate"
    );
    assert!(
        dropped_rate >= 400,
        "most of the 500/s flood must be shed by rate limiting (dropped_rate={dropped_rate})"
    );
    assert!(rt.content_len() <= default_cfg.content_queue_limit);

    // 2. ペイロードバイト上限: 4096 バイト級ペイロードは 4095 上限で拒否、16 KiB+1 はデフォルト上限で拒否
    let mut tight_rt = SecurityRuntime::new(
        SecurityRuntimeConfig {
            max_payload_bytes: payload_bytes - 1,
            ..SecurityRuntimeConfig::default()
        },
        SchedulerConfig::default(),
        SecretRedactor::default(),
        None,
    )
    .expect("tight payload runtime");
    let mut ev_4096 = case["input"]["event"].clone();
    ev_4096["payload"]["text"] = Value::String("A".repeat(payload_bytes));
    let raw_4096 = serde_json::to_vec(&ev_4096).unwrap();
    assert_eq!(
        tight_rt.admit_content_bytes(&raw_4096, 0).unwrap(),
        ContentAdmitDecision::DroppedPayloadBytes
    );

    let mut ev_over_default = case["input"]["event"].clone();
    ev_over_default["payload"]["text"] =
        Value::String("A".repeat(default_cfg.max_payload_bytes + 1));
    let raw_over_default = serde_json::to_vec(&ev_over_default).unwrap();
    let mut fresh_default = runtime();
    assert_eq!(
        fresh_default
            .admit_content_bytes(&raw_over_default, 0)
            .unwrap(),
        ContentAdmitDecision::DroppedPayloadBytes
    );

    // 3. キュー飽和（バックプレッシャー）状態でも認証済み緊急停止はブロックされない
    let mut saturated_rt = SecurityRuntime::new(
        SecurityRuntimeConfig {
            content_rate_per_second: 10_000.0,
            content_burst: 1_000,
            content_queue_limit: 4,
            ..SecurityRuntimeConfig::default()
        },
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("saturated runtime");
    let mut backpressure_drops = 0usize;
    for i in 0..10 {
        let mut ev = case["input"]["event"].clone();
        ev["event_id"] = Value::String(format!("evt-sat-{i}"));
        ev["sequence"] = Value::from(1_000 + i as u64);
        let raw = serde_json::to_vec(&ev).unwrap();
        if saturated_rt.admit_content_bytes(&raw, i as u64).unwrap()
            == ContentAdmitDecision::DroppedBackpressure
        {
            backpressure_drops += 1;
        }
    }
    assert_eq!(saturated_rt.content_len(), 4);
    assert_eq!(backpressure_drops, 6);

    saturated_rt
        .scheduler_mut()
        .schedule(PlannedPerformance {
            event_id: "flood-audio".to_owned(),
            asset_id: "flood-asset".to_owned(),
            priority: Priority::Conversation,
            interruptible: false,
            interrupt_points_ms: vec![0, 1_000],
            start_at_ms: 0,
            duration_ms: 1_000,
            generation: 0,
            exclusive: false,
            channels: BTreeSet::from([BlendChannel::Audio]),
        })
        .expect("scheduled audio");
    let stop = authenticated(Capability::PerformerStop, "performer.stop");
    assert_eq!(
        saturated_rt.handle_control(&stop, 50).unwrap(),
        ControlOutcome::Stopped { cancelled: 1 }
    );
    assert!(saturated_rt.scheduler().items().is_empty());
}

#[test]
fn fixture_model_outputs_are_contained_at_public_output_gate() {
    // #504 (#497): 全共有フィクスチャのうち構造化 model_output (requested_actions / authorization)
    // を持つ 6 ケースについて、モデル出力が Rust の公開出力封じ込め境界
    // (SecurityRuntime::publish_text) で必ず遮断・置換されることを検証する。
    // 一方、memory-poisoning-chat.json の文字列 model_output は制御語彙を含まないため
    // 公開テキストとしては Allow となり、永続メモリ書き込みは authorize_memory_write の
    // 非シリアライズ許可境界で拒否されることを明示的に区別して検証する。
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/security");
    let mut contained_control_cases = 0usize;
    let mut non_control_text_cases = 0usize;
    for entry in std::fs::read_dir(root).expect("security fixture directory") {
        let path = entry.expect("fixture entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap();
        let case: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let model_output = &case["model_output"];
        if model_output.is_null() {
            continue;
        }
        if let Some(plain_text) = model_output.as_str() {
            // memory-poisoning-chat.json: 制御語彙を含まない通常文
            assert_eq!(name, "memory-poisoning-chat.json");
            non_control_text_cases += 1;
            let mut rt = runtime();
            let out = rt.publish_text(plain_text);
            assert_eq!(out.verdict, OutputVerdict::Allow, "{name}");
            // 公開テキストが許可されても、イベントから永続メモリ書き込み許可は一切発行されない
            let event: EventEnvelope =
                serde_json::from_value(case["input"]["event"].clone()).unwrap();
            let (decision, permit) = rt.authorize_memory_write(&event, None);
            assert_eq!(decision, MemoryWriteDecision::Denied, "{name}");
            assert!(permit.is_none(), "{name}");
            continue;
        }
        assert!(
            model_output.is_object(),
            "{name}: unexpected model_output shape"
        );
        contained_control_cases += 1;
        let serialized_model_output = serde_json::to_string(model_output).unwrap();

        // フォールバックなし -> Suppress
        let mut rt_suppress = runtime();
        let out_suppress = rt_suppress.publish_text(&serialized_model_output);
        assert_eq!(
            out_suppress.verdict,
            OutputVerdict::Suppress,
            "{name}: model_output must be suppressed when no fallback is configured"
        );
        assert_eq!(out_suppress.reason, "control_plane_text", "{name}");
        assert!(out_suppress.text.is_none(), "{name}");
        assert!(
            !rt_suppress
                .audit()
                .iter()
                .any(|record| record.decision == "authorized_control"),
            "{name}"
        );
        assert!(!rt_suppress.is_muted(), "{name}");

        // フォールバックあり -> ReplaceWithCached
        let mut rt_fallback = SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig::default(),
            SecretRedactor::default(),
            Some("safe cached reaction".to_owned()),
        )
        .expect("fallback runtime");
        let out_fallback = rt_fallback.publish_text(&serialized_model_output);
        assert_eq!(
            out_fallback.verdict,
            OutputVerdict::ReplaceWithCached,
            "{name}: model_output must be replaced with cached fallback"
        );
        assert_eq!(out_fallback.reason, "control_plane_text", "{name}");
        assert_eq!(
            out_fallback.text.as_deref(),
            Some("safe cached reaction"),
            "{name}"
        );
    }
    // prompt-injection-chat, model-output-forged-authorization, 4x chat-claims-* の計 6 ケース
    assert_eq!(contained_control_cases, 6);
    // memory-poisoning-chat の 1 ケース
    assert_eq!(non_control_text_cases, 1);
}
