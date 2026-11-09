//! Emits Rust-serialized `SpeechTranscriptBatch` samples for the Rust -> JSON
//! Schema parity check (`bun run validate:rust-parity`).

use aivtuber_domain::{
    SPEECH_TRANSCRIPT_SCHEMA_VERSION, SpeechTranscript, SpeechTranscriptBatch, TranscriptEngine,
    TranscriptKind,
};
use serde_json::json;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let fixtures = [
        include_str!("../../../examples/speech-transcripts/valid/partial.json"),
        include_str!("../../../examples/speech-transcripts/valid/final.json"),
        include_str!("../../../examples/speech-transcripts/valid/empty-final.json"),
        include_str!("../../../examples/speech-transcripts/valid/revision-run.json"),
        include_str!("../../../examples/speech-transcripts/valid/parakeet-final.json"),
    ];
    let mut batches = fixtures
        .into_iter()
        .map(|text| SpeechTranscriptBatch::from_json_bytes(text.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    // Boundary values the fixtures do not reach: the largest text, revision,
    // covered span and time, a partial and a final in one batch.
    batches.push(SpeechTranscriptBatch {
        schema_version: SPEECH_TRANSCRIPT_SCHEMA_VERSION.to_owned(),
        provider_id: "parity".to_owned(),
        provider_version: "1".to_owned(),
        language: "ja".to_owned(),
        engine: TranscriptEngine {
            model_id: "m:1".to_owned(),
            sha256: "f".repeat(64),
        },
        transcripts: vec![
            SpeechTranscript {
                speaker_id: "s1".to_owned(),
                segment_id: 9_007_199_254_740_991,
                revision: 4_095,
                kind: TranscriptKind::Partial,
                text: "あ".repeat(1_000),
                covered_audio_ms: 60_000,
                observed_at_ms: 9_007_199_254_740_991,
                complete: false,
            },
            SpeechTranscript {
                speaker_id: "s1".to_owned(),
                segment_id: 0,
                revision: 0,
                kind: TranscriptKind::Final,
                text: String::new(),
                covered_audio_ms: 0,
                observed_at_ms: 0,
                complete: true,
            },
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
