//! #498: shared fixture evidence at the actual Rust security boundary.
//! This is not a model runner, a flood workload runner or an audio test.
use aivtuber_domain::{
    AuthenticatedControlCommand, AuthorizationMethod, Capability, ControlSecret, EventEnvelope,
    LocalControlIngress, OperatorCommandInput,
};
use aivtuber_runtime::{
    ContentAdmitDecision, ControlOutcome, MemoryWriteDecision, RuntimeError, SecurityRuntime,
    SecurityRuntimeConfig,
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
