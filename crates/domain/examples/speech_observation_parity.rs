//! Emits Rust-serialized `SpeechObservationBatch` samples for the Rust -> JSON
//! Schema parity check (`bun run validate:rust-parity`).

use aivtuber_domain::{
    SPEECH_OBSERVATION_SCHEMA_VERSION, SpeechActivityObservation, SpeechEdge, SpeechObservation,
    SpeechObservationBatch, TurnEndScoreObservation,
};
use serde_json::json;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let fixtures = [
        include_str!("../../../examples/speech-observations/valid/activity-edges.json"),
        include_str!("../../../examples/speech-observations/valid/turn-end-score-nowcast.json"),
        include_str!("../../../examples/speech-observations/valid/turn-end-score-predicted.json"),
    ];
    let mut batches = fixtures
        .into_iter()
        .map(|text| SpeechObservationBatch::from_json_bytes(text.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    // A constructed batch that sets every optional field and one that sets none.
    batches.push(SpeechObservationBatch {
        schema_version: SPEECH_OBSERVATION_SCHEMA_VERSION.to_owned(),
        provider_id: "parity".to_owned(),
        provider_version: "1".to_owned(),
        language: "ja".to_owned(),
        observations: vec![
            SpeechObservation::SpeechActivity(SpeechActivityObservation {
                speaker_id: "s1".to_owned(),
                edge: SpeechEdge::End,
                observed_at_ms: 10,
                expires_at_ms: Some(20),
                confidence: Some(0.5),
            }),
            SpeechObservation::SpeechActivity(SpeechActivityObservation {
                speaker_id: "s1".to_owned(),
                edge: SpeechEdge::Start,
                observed_at_ms: 10,
                expires_at_ms: None,
                confidence: None,
            }),
            SpeechObservation::TurnEndScore(TurnEndScoreObservation {
                speaker_id: "s1".to_owned(),
                observed_at_ms: 10,
                expires_at_ms: Some(20),
                provider: "p".to_owned(),
                model_id: "m:1".to_owned(),
                calibrated: true,
                score: 1.0,
                prediction_horizon_ms: Some(250),
            }),
        ],
    });
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
