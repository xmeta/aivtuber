//! Emits Rust-serialized `CrowdSummary` samples for the Rust -> JSON Schema
//! parity check (`bun run validate:rust-parity`).

use aivtuber_domain::{
    CROWD_CONTEXT_SCHEMA_VERSION, CrowdClass, CrowdSummary, CrowdTrend, ReflexContext,
};
use serde_json::json;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let classes = [
        CrowdClass::Laughter,
        CrowdClass::GoodGame,
        CrowdClass::Praise,
        CrowdClass::Congratulation,
        CrowdClass::Greeting,
        CrowdClass::Cheer,
        CrowdClass::Surprise,
        CrowdClass::Sympathy,
        CrowdClass::Thanks,
        CrowdClass::RepeatedQuestion,
    ];
    let trends = [
        CrowdTrend::Rising,
        CrowdTrend::Steady,
        CrowdTrend::Falling,
        CrowdTrend::Unknown,
    ];
    let mut summaries = Vec::new();
    for (index, class) in classes.into_iter().enumerate() {
        let summary = CrowdSummary {
            schema_version: CROWD_CONTEXT_SCHEMA_VERSION.to_owned(),
            class,
            member_count: 2 + index as u32 * 7,
            unique_actors_estimate: 1 + index as u32,
            intensity_milli: (index as u16 * 111).min(1000),
            trend: trends[index % trends.len()],
            window_batches: 1 + index as u32 % 3,
        };
        summary.validate()?;
        summaries.push(summary);
    }
    let without = ReflexContext::default();
    let with = ReflexContext {
        crowd: Some(summaries[3].clone()),
        ..ReflexContext::default()
    };
    with.validate()?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "summaries": summaries,
            "reflex_context_without_crowd": without,
            "reflex_context_with_crowd": with,
        }))?
    );
    Ok(())
}
