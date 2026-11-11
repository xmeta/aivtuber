//! Emits Rust-serialized `SceneContextSnapshot` and `ObsSceneMap` samples for
//! the Rust -> JSON Schema parity check (`bun run validate:rust-parity`).

use aivtuber_domain::{
    Interruptibility, OBS_SCENE_MAP_SCHEMA_VERSION, ObsSceneMap, ObsSceneMapEntry, ReflexContext,
    SCENE_CONTEXT_SCHEMA_VERSION, SceneClass, SceneContextSnapshot, SceneSourceRank,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::error::Error;

fn snapshot(
    class: SceneClass,
    rank: SceneSourceRank,
    labels: &[&str],
    labels_rank: SceneSourceRank,
    interruptibility: Interruptibility,
    perceived: bool,
) -> SceneContextSnapshot {
    SceneContextSnapshot {
        schema_version: SCENE_CONTEXT_SCHEMA_VERSION.to_owned(),
        version: 7,
        scene_class: class,
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        interruptibility,
        source_rank: rank,
        labels_source_rank: labels_rank,
        source_ref: perceived.then(|| "game-main".to_owned()),
        confidence: perceived.then_some(0.5),
        calibrated: perceived,
        observed_at_ms: 1_000,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut snapshots = vec![
        snapshot(
            SceneClass::Gameplay,
            SceneSourceRank::ObsState,
            &["boss_fight"],
            SceneSourceRank::ObsState,
            Interruptibility::Uninterruptible,
            false,
        ),
        snapshot(
            SceneClass::JustChatting,
            SceneSourceRank::ObsState,
            &["settings"],
            SceneSourceRank::Perception,
            Interruptibility::Interruptible,
            true,
        ),
        snapshot(
            SceneClass::Menu,
            SceneSourceRank::Perception,
            &["map", "settings"],
            SceneSourceRank::Perception,
            Interruptibility::Unknown,
            true,
        ),
        snapshot(
            SceneClass::Unknown,
            SceneSourceRank::None,
            &[],
            SceneSourceRank::None,
            Interruptibility::Unknown,
            false,
        ),
        snapshot(
            SceneClass::Intermission,
            SceneSourceRank::TrustedTimer,
            &[],
            SceneSourceRank::None,
            Interruptibility::Interruptible,
            false,
        ),
    ];
    for fixture in [
        include_str!("../../../examples/scene-context/valid/obs-uninterruptible.json"),
        include_str!("../../../examples/scene-context/valid/obs-class-perceived-labels.json"),
        include_str!("../../../examples/scene-context/valid/perceived-gameplay.json"),
        include_str!("../../../examples/scene-context/valid/nothing-known.json"),
    ] {
        snapshots.push(serde_json::from_str(fixture)?);
    }
    for snapshot in &snapshots {
        snapshot.validate()?;
    }

    let mut maps = vec![ObsSceneMap::from_json_bytes(
        include_str!("../../../examples/obs-scene-maps/valid/example.json").as_bytes(),
    )?];
    maps.push(ObsSceneMap {
        schema_version: OBS_SCENE_MAP_SCHEMA_VERSION.to_owned(),
        scenes: BTreeMap::from([
            (
                "Game".to_owned(),
                ObsSceneMapEntry {
                    scene_class: SceneClass::Result,
                    interruptibility: Interruptibility::Interruptible,
                    labels: vec!["victory".to_owned()],
                },
            ),
            (
                "Plain".to_owned(),
                ObsSceneMapEntry {
                    scene_class: SceneClass::Desktop,
                    interruptibility: Interruptibility::Unknown,
                    labels: Vec::new(),
                },
            ),
        ]),
    });
    maps.push(ObsSceneMap::from_json_bytes(
        include_str!("../../../examples/obs-scene-maps/valid/empty.json").as_bytes(),
    )?);
    for map in &maps {
        map.validate()?;
    }

    let without = ReflexContext::default();
    let with = ReflexContext {
        scene: Some(snapshots[0].clone()),
        ..ReflexContext::default()
    };
    with.validate()?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "snapshots": snapshots,
            "obs_scene_maps": maps,
            "reflex_context_without_scene": without,
            "reflex_context_with_scene": with,
        }))?
    );
    Ok(())
}
