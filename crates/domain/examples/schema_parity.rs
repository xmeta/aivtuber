use aivtuber_domain::{EventEnvelope, ReflexDecision};
use serde_json::{Value, json};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let event_sources = [
        include_str!("../../../examples/events/chat-message.json"),
        include_str!("../../../examples/events/chat-donation.json"),
        include_str!("../../../examples/events/speech-input.json"),
        include_str!("../../../examples/events/game-event.json"),
        include_str!("../../../examples/events/stream-event.json"),
        include_str!("../../../examples/events/timer-tick.json"),
        include_str!("../../../examples/events/operator-stop.json"),
        include_str!("../../../examples/events/system-health.json"),
    ];
    let reflex_sources = [
        include_str!("../../../examples/reflex-decisions/cached-reaction.json"),
        include_str!("../../../examples/reflex-decisions/timeout-fallback.json"),
    ];

    let events = event_sources
        .into_iter()
        .map(|source| -> Result<Value, Box<dyn Error>> {
            let event: EventEnvelope = serde_json::from_str(source)?;
            event.validate()?;
            Ok(serde_json::to_value(event)?)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let reflex_decisions = reflex_sources
        .into_iter()
        .map(|source| -> Result<Value, Box<dyn Error>> {
            let decision: ReflexDecision = serde_json::from_str(source)?;
            decision.validate()?;
            Ok(serde_json::to_value(decision)?)
        })
        .collect::<Result<Vec<_>, _>>()?;

    println!(
        "{}",
        serde_json::to_string(&json!({
            "events": events,
            "reflex_decisions": reflex_decisions
        }))?
    );
    Ok(())
}
