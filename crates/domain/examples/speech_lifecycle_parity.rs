//! Emits Rust-serialized `SpeechLifecycleEvent` samples for the Rust -> JSON
//! Schema parity check (`bun run validate:rust-parity`).

use aivtuber_domain::{
    SPEECH_LIFECYCLE_SCHEMA_VERSION, SpeechCancelReason, SpeechLifecycleEvent, SpeechPhase,
    SpeechReached, SpeechRef, SpeechTimeBasis,
};
use serde_json::json;
use std::error::Error;

fn event(
    seq: u64,
    basis: SpeechTimeBasis,
    generation: Option<u64>,
    phase: SpeechPhase,
) -> SpeechLifecycleEvent {
    SpeechLifecycleEvent {
        schema_version: SPEECH_LIFECYCLE_SCHEMA_VERSION.to_owned(),
        seq,
        at_ms: seq * 10,
        basis,
        speech: SpeechRef {
            event_id: "evt-parity#p1".to_owned(),
            generation,
            phrase: 1,
            final_phrase: false,
            asset_id: "gen-parity".to_owned(),
            speech_text_digest: Some("0".repeat(64)),
            projection: Some(format!("rules:1|lexicon:starter@1+{}", "b".repeat(64))),
        },
        phase,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let fixtures = [
        include_str!("../../../examples/speech-lifecycle/valid/prepared.json"),
        include_str!("../../../examples/speech-lifecycle/valid/committed.json"),
        include_str!("../../../examples/speech-lifecycle/valid/started.json"),
        include_str!("../../../examples/speech-lifecycle/valid/boundary.json"),
        include_str!("../../../examples/speech-lifecycle/valid/finished-drained.json"),
        include_str!("../../../examples/speech-lifecycle/valid/finished-nonfinal-phrase.json"),
        include_str!(
            "../../../examples/speech-lifecycle/valid/cancelled-operator-stop-started.json"
        ),
        include_str!(
            "../../../examples/speech-lifecycle/valid/cancelled-superseded-committed.json"
        ),
        include_str!(
            "../../../examples/speech-lifecycle/valid/cancelled-scheduler-rejected-before-commit.json"
        ),
        include_str!("../../../examples/speech-lifecycle/valid/minimal-no-digest.json"),
    ];
    let mut events = fixtures
        .into_iter()
        .map(|text| SpeechLifecycleEvent::from_json_bytes(text.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;

    // Every constructed phase and every cancel reason.
    let logical = SpeechTimeBasis::LogicalTimeline;
    events.push(event(1, logical, None, SpeechPhase::Prepared));
    events.push(event(2, logical, Some(3), SpeechPhase::Committed));
    events.push(event(3, logical, Some(3), SpeechPhase::Started));
    events.push(event(
        4,
        logical,
        Some(3),
        SpeechPhase::Boundary { offset_ms: 250 },
    ));
    events.push(event(
        5,
        SpeechTimeBasis::OutputAccepted,
        Some(3),
        SpeechPhase::Finished,
    ));
    for (index, reason) in [
        SpeechCancelReason::OperatorStop,
        SpeechCancelReason::Muted,
        SpeechCancelReason::Preempted,
        SpeechCancelReason::Superseded,
        SpeechCancelReason::BargeIn,
        SpeechCancelReason::SchedulerRejected,
        SpeechCancelReason::StaleOrLate,
        SpeechCancelReason::GenerationCancelled,
        SpeechCancelReason::OutputFailure,
        SpeechCancelReason::Shutdown,
        SpeechCancelReason::GateRejected,
    ]
    .into_iter()
    .enumerate()
    {
        events.push(event(
            10 + index as u64,
            SpeechTimeBasis::OutputDrained,
            Some(4),
            SpeechPhase::Cancelled {
                reason,
                reached: SpeechReached::Started,
                spoken_ms: Some(40),
            },
        ));
    }
    events.push(event(
        30,
        logical,
        None,
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::GateRejected,
            reached: SpeechReached::BeforeCommit,
            spoken_ms: None,
        },
    ));
    events.push(event(
        31,
        logical,
        Some(5),
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::Superseded,
            reached: SpeechReached::Committed,
            spoken_ms: None,
        },
    ));
    for event in &events {
        event.validate()?;
    }
    let values = events
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    println!("{}", serde_json::to_string(&json!({ "events": values }))?);
    Ok(())
}
