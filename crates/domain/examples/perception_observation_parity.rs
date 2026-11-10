//! Emits Rust-serialized `PerceptionObservationBatch` samples for the Rust ->
//! JSON Schema parity check (`bun run validate:rust-parity`).

use aivtuber_domain::{
    PERCEPTION_OBSERVATION_SCHEMA_VERSION, PerceptionCaptionPayload, PerceptionFrame,
    PerceptionKind, PerceptionObservation, PerceptionObservationBatch, PerceptionPayload,
    PerceptionProducer, PerceptionRedaction, PerceptionSceneClass, PerceptionScenePayload,
    PerceptionSource, PrivacyClass,
};
use serde_json::json;
use std::error::Error;

fn observation(
    id: &str,
    kind: PerceptionKind,
    payload: PerceptionPayload,
    optional: bool,
) -> PerceptionObservation {
    PerceptionObservation {
        kind,
        observation_id: id.to_owned(),
        source: PerceptionSource {
            alias: "game-main".to_owned(),
        },
        frame: PerceptionFrame {
            capture_seq: 7,
            captured_at_ms: 90,
            width: 768,
            height: 432,
        },
        observed_at_ms: 100,
        expires_at_ms: 100 + 30_000,
        confidence: optional.then_some(0.5),
        calibrated: optional,
        producer: PerceptionProducer {
            model_id: "parity:1".to_owned(),
            config_digest: "ab".repeat(32),
            prompt_version: "p1".to_owned(),
        },
        privacy: if optional {
            PrivacyClass::Public
        } else {
            PrivacyClass::Private
        },
        redaction: PerceptionRedaction {
            policy_version: "r1".to_owned(),
            secret_hits: if optional { 3 } else { 0 },
            truncated: optional,
        },
        payload,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let fixtures = [
        include_str!("../../../examples/perception-observations/valid/scene-gameplay.json"),
        include_str!("../../../examples/perception-observations/valid/caption.json"),
        include_str!(
            "../../../examples/perception-observations/valid/scene-and-caption-one-frame.json"
        ),
        include_str!("../../../examples/perception-observations/valid/caption-at-byte-bound.json"),
        include_str!(
            "../../../examples/perception-observations/valid/caption-hostile-text-is-still-data.json"
        ),
    ];
    let mut batches = fixtures
        .into_iter()
        .map(|text| PerceptionObservationBatch::from_json_bytes(text.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    // A constructed batch that sets every optional field and one that sets none.
    for optional in [true, false] {
        batches.push(PerceptionObservationBatch {
            schema_version: PERCEPTION_OBSERVATION_SCHEMA_VERSION.to_owned(),
            provider_id: "parity".to_owned(),
            provider_version: "1".to_owned(),
            observations: vec![
                observation(
                    "obs-scene",
                    PerceptionKind::ScreenScene,
                    PerceptionPayload::Scene(PerceptionScenePayload {
                        scene_class: PerceptionSceneClass::Cutscene,
                        labels: vec!["dialogue".to_owned()],
                    }),
                    optional,
                ),
                observation(
                    "obs-caption",
                    PerceptionKind::ScreenCaption,
                    PerceptionPayload::Caption(PerceptionCaptionPayload {
                        caption: "a cutscene".to_owned(),
                    }),
                    optional,
                ),
            ],
        });
    }
    for batch in &batches {
        batch.validate()?;
    }
    let values = batches
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    println!("{}", serde_json::to_string(&json!({ "batches": values }))?);
    Ok(())
}
