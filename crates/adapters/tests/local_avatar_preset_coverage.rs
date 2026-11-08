//! Coverage guard for the adapter-private preset mapping (#219 S4): every
//! expression/gesture preset in a checked-in pack, asset, fixture or built-in
//! default must be either mapped to the local renderer or explicitly
//! unsupported with a reason. A new pack preset the table has never heard of
//! fails here instead of silently never animating.

use aivtuber_adapters::{PresetKind, PresetMapping, map_preset};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

type Found = BTreeSet<(PresetKind, String)>;

/// Built-in defaults that are not in `examples/`:
/// * `speaking.neutral`: `PerformanceCompilerConfig` default expression
///   (`AIVTUBER_GENERATED_EXPRESSION_PRESET` may override it at deployment).
/// * the Jev/System One `gesture_family` allow-list in `crates/reflex/src/jev.rs`.
const BUILT_IN: &[(PresetKind, &str)] = &[
    (PresetKind::Expression, "speaking.neutral"),
    (PresetKind::Gesture, "nod_small"),
    (PresetKind::Gesture, "head_shake_small"),
    (PresetKind::Gesture, "wave_small"),
    (PresetKind::Gesture, "shrug_small"),
];

fn json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read examples dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            json_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

fn preset_of(value: &Value) -> Option<&str> {
    value.get("preset").and_then(Value::as_str)
}

fn collect(value: &Value, found: &mut Found) {
    let mut add = |kind, name: &str| {
        found.insert((kind, name.to_owned()));
    };
    match value {
        Value::Object(map) => {
            // Performance asset / reflex decision shapes.
            if let Some(name) = map.get("expression").and_then(preset_of) {
                add(PresetKind::Expression, name);
            }
            if let Some(name) = map.get("gesture").and_then(preset_of) {
                add(PresetKind::Gesture, name);
            }
            if let Some(name) = map.get("gesture_family").and_then(Value::as_str)
                && name != "none"
            {
                add(PresetKind::Gesture, name);
            }
            // Avatar playback command shape.
            if let (Some(kind), Some(name)) =
                (map.get("kind").and_then(Value::as_str), preset_of(value))
            {
                match kind {
                    "expression" => add(PresetKind::Expression, name),
                    "gesture" => add(PresetKind::Gesture, name),
                    _ => {}
                }
            }
            // Timeline events.
            if let Some(event) = map.get("event").and_then(Value::as_str)
                && let Some(name) = map.get("payload").and_then(preset_of)
            {
                match event {
                    "expression.start" => add(PresetKind::Expression, name),
                    "gesture.start" => add(PresetKind::Gesture, name),
                    _ => {}
                }
            }
            for child in map.values() {
                collect(child, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect(item, found)),
        _ => {}
    }
}

#[test]
fn every_checked_in_preset_is_mapped_or_explicitly_unsupported() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut files = Vec::new();
    json_files(&root, &mut files);
    assert!(!files.is_empty(), "no example JSON found under {root:?}");

    let mut found = Found::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read example");
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file:?}: {e}"));
        collect(&value, &mut found);
    }
    for (kind, name) in BUILT_IN {
        found.insert((*kind, (*name).to_owned()));
    }

    // The scan itself must not rot: the starter pack's presets have to show up.
    for (kind, name) in [
        (PresetKind::Expression, "surprise_medium"),
        (PresetKind::Gesture, "recoil_small"),
        (PresetKind::Expression, "smile_bright"),
        (PresetKind::Gesture, "nod_double"),
    ] {
        assert!(
            found.contains(&(kind, name.to_owned())),
            "scan missed {name}"
        );
    }

    let unknown: Vec<_> = found
        .iter()
        .filter(|(kind, name)| map_preset(*kind, name) == PresetMapping::Unknown)
        .collect();
    assert!(
        unknown.is_empty(),
        "presets with neither a mapping nor an explicit unsupported entry \
         (add them to crates/adapters/src/local_avatar/presets.rs): {unknown:?}"
    );

    // Unsupported entries always carry a reason.
    for (kind, name) in &found {
        if let PresetMapping::Unsupported { reason } = map_preset(*kind, name) {
            assert!(!reason.is_empty(), "{name} has an empty reason");
        }
    }
}
