//! Emits Rust-serialized `InitiativeContext` samples for the Rust -> JSON
//! Schema parity check (`bun run validate:rust-parity`).

use aivtuber_domain::{InitiativeContext, InitiativeKind, ReflexContext};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut contexts = Vec::new();
    for kind in InitiativeKind::ALL {
        let context = InitiativeContext::new(kind);
        context.validate()?;
        contexts.push(context);
    }
    let without = ReflexContext::default();
    let with = ReflexContext {
        initiative: Some(contexts[1].clone()),
        ..ReflexContext::default()
    };
    with.validate()?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "contexts": contexts,
            "reflex_context_without_initiative": without,
            "reflex_context_with_initiative": with,
        }))?
    );
    Ok(())
}
