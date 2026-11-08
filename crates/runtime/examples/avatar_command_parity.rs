//! Emits Rust-serialized `AvatarPlaybackCommand` samples for the Rust -> JSON
//! Schema parity check (`bun run validate:rust-parity`).

use aivtuber_domain::Viseme;
use aivtuber_runtime::AvatarPlaybackCommand;
use serde_json::json;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let fixtures = [
        include_str!("../../../examples/avatar-playback-commands/valid/expression.json"),
        include_str!("../../../examples/avatar-playback-commands/valid/gesture.json"),
        include_str!("../../../examples/avatar-playback-commands/valid/gaze.json"),
        include_str!("../../../examples/avatar-playback-commands/valid/viseme-aa.json"),
        include_str!("../../../examples/avatar-playback-commands/valid/viseme-neutral.json"),
        include_str!("../../../examples/avatar-playback-commands/valid/reset.json"),
    ];
    let mut commands = fixtures
        .into_iter()
        .map(serde_json::from_str::<AvatarPlaybackCommand>)
        .collect::<Result<Vec<_>, _>>()?;
    // Every canonical viseme, so the schema enum and the Rust enum cannot drift.
    commands.extend(
        Viseme::ALL
            .into_iter()
            .map(|viseme| AvatarPlaybackCommand::Viseme {
                generation: 1,
                at_ms: 0,
                viseme,
                weight: 1.0,
            }),
    );
    let values = commands
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    println!("{}", serde_json::to_string(&json!({ "commands": values }))?);
    Ok(())
}
