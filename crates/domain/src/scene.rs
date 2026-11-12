//! Provider-neutral scene context (issue #88 S1, ADR-0007 D-3 and D-10).
//!
//! A [`SceneContextSnapshot`] is the one small, closed-vocabulary answer to
//! "what is the stream showing right now, and how safe is it to talk": a scene
//! class, at most eight labels, an interruptibility hint, the source rank each
//! field came from, and a monotonic version. It has no free text on purpose, so
//! it is the only scene value a Reflex, Jev or Thinking consumer is handed.
//!
//! Authority rule (enforced by [`SceneContextSnapshot::validate`], not by
//! convention): only a trusted source (rank 1 to 3) can carry an
//! interruptibility other than `unknown`. A perceived scene (rank 4, untrusted
//! model output) can inform a decision but can never make the performer busy,
//! defer a reply or declare a moment uninterruptible. Chat has no path in at
//! all: nothing here is built from an event.
//!
//! Normative wire forms: `schemas/scene-context.schema.json` (the snapshot) and
//! `schemas/obs-scene-map.schema.json` (the operator-owned table that maps an
//! OBS program scene name to a scene class). Source ranking and freshness live
//! in `aivtuber_runtime::SceneContextTracker`.

use crate::perception::PerceptionSceneClass;
use crate::speech::MAX_OBSERVATION_TIME_MS;
use crate::{DomainValidationError, validate_perception_label_token};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const SCENE_CONTEXT_SCHEMA_VERSION: &str = "0.1.0";
pub const OBS_SCENE_MAP_SCHEMA_VERSION: &str = "0.1.0";
/// Most labels one snapshot may carry.
pub const MAX_SCENE_LABELS: usize = 8;
/// Most entries in the operator's OBS scene mapping table.
pub const MAX_OBS_SCENE_MAP_ENTRIES: usize = 32;
/// Longest OBS scene name a table key may have, in UTF-8 bytes.
pub const MAX_OBS_SCENE_NAME_BYTES: usize = 128;
/// Largest serialized mapping table the loader accepts, before parsing.
pub const MAX_OBS_SCENE_MAP_BYTES: usize = 16 * 1024;

/// The final closed label vocabulary (replaces the S1a provisional list; the
/// tokens are unchanged). A label outside it is dropped, never forwarded.
pub const SCENE_LABEL_VOCABULARY: [&str; 12] = [
    "boss_fight",
    "combat",
    "dialogue",
    "exploration",
    "inventory",
    "map",
    "multiplayer_lobby",
    "puzzle",
    "racing",
    "settings",
    "title_screen",
    "victory",
];

pub fn is_scene_label(token: &str) -> bool {
    SCENE_LABEL_VOCABULARY.contains(&token)
}

/// Closed scene and stream-segment vocabulary. Perception can only produce the
/// subset it models ([`PerceptionSceneClass`]); the stream segments
/// (`just_chatting`, `intermission`) come from trusted OBS state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneClass {
    Gameplay,
    Menu,
    Loading,
    Cutscene,
    Result,
    Desktop,
    /// A talking segment with no game in focus.
    JustChatting,
    /// Starting, break, be-right-back or ending screens.
    Intermission,
    /// No source knows. The safe coarse default; consumers must handle it.
    Unknown,
}

impl SceneClass {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gameplay => "gameplay",
            Self::Menu => "menu",
            Self::Loading => "loading",
            Self::Cutscene => "cutscene",
            Self::Result => "result",
            Self::Desktop => "desktop",
            Self::JustChatting => "just_chatting",
            Self::Intermission => "intermission",
            Self::Unknown => "unknown",
        }
    }
}

impl From<PerceptionSceneClass> for SceneClass {
    fn from(class: PerceptionSceneClass) -> Self {
        match class {
            PerceptionSceneClass::Gameplay => Self::Gameplay,
            PerceptionSceneClass::Menu => Self::Menu,
            PerceptionSceneClass::Loading => Self::Loading,
            PerceptionSceneClass::Cutscene => Self::Cutscene,
            PerceptionSceneClass::Result => Self::Result,
            PerceptionSceneClass::Desktop => Self::Desktop,
            PerceptionSceneClass::Unknown => Self::Unknown,
        }
    }
}

/// Whether the performer may be interrupted. A hint for trusted scheduling
/// only; see the module authority rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interruptibility {
    Interruptible,
    Uninterruptible,
    #[default]
    Unknown,
}

/// The interruptibility scheduling and the conversational floor may read
/// (#88 S1, ADR-0007 D-10; #83 S3b).
///
/// The inner value is private and the only way to a non-`unknown` one is
/// [`Self::from_snapshot`], which yields `unknown` for any snapshot whose
/// source is not rank 1 to 3. A perceived (rank 4) scene therefore cannot be
/// turned into this type with a scheduling meaning, whatever its fields say,
/// and the floor API accepts nothing else. There is deliberately no
/// `Deserialize`, no `From<Interruptibility>` and no public constructor taking
/// a bare value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct TrustedInterruptibility(Interruptibility);

impl TrustedInterruptibility {
    /// Nothing trusted is known: every consumer keeps its default behaviour.
    pub const fn unknown() -> Self {
        Self(Interruptibility::Unknown)
    }

    /// The snapshot's interruptibility if and only if its source is trusted.
    pub fn from_snapshot(snapshot: &SceneContextSnapshot) -> Self {
        if snapshot.source_rank.is_trusted() {
            Self(snapshot.interruptibility)
        } else {
            Self::unknown()
        }
    }

    pub const fn get(self) -> Interruptibility {
        self.0
    }
}

/// ADR-0007 D-3 source rank. Lower is more authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneSourceRank {
    /// Rank 1: trusted game telemetry. Reserved; no source yet.
    TrustedTelemetry,
    /// Rank 2: known OBS state, mapped through the operator's table.
    ObsState,
    /// Rank 3: trusted timers and state machines. Reserved; no source yet.
    TrustedTimer,
    /// Rank 4: perception. Untrusted, expiring, never authority.
    Perception,
    /// Rank 5: nothing is known.
    None,
}

impl SceneSourceRank {
    pub const fn rank(self) -> u8 {
        match self {
            Self::TrustedTelemetry => 1,
            Self::ObsState => 2,
            Self::TrustedTimer => 3,
            Self::Perception => 4,
            Self::None => 5,
        }
    }

    /// Ranks 1 to 3: the only sources that may affect scheduling.
    pub const fn is_trusted(self) -> bool {
        self.rank() <= 3
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::TrustedTelemetry => "trusted_telemetry",
            Self::ObsState => "obs_state",
            Self::TrustedTimer => "trusted_timer",
            Self::Perception => "perception",
            Self::None => "none",
        }
    }
}

/// What the stream shows now. Bounded, versioned, free-text-free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneContextSnapshot {
    pub schema_version: String,
    /// Monotonic per runtime run, starting at 1. Increases on every change of
    /// any other field, so a recorded decision names exactly the value it used.
    pub version: u64,
    pub scene_class: SceneClass,
    /// Closed-vocabulary labels (at most eight, unique).
    pub labels: Vec<String>,
    /// `unknown` unless `source_rank` is trusted.
    pub interruptibility: Interruptibility,
    /// Rank of the source `scene_class` (and `interruptibility`) came from.
    pub source_rank: SceneSourceRank,
    /// Rank of the source `labels` came from; `none` when `labels` is empty.
    pub labels_source_rank: SceneSourceRank,
    /// Operator alias of the perception source, when any field is perceived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// Self-reported confidence of the perceived part. Absent for trusted
    /// sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Whether `confidence` is a calibrated probability.
    #[serde(default)]
    pub calibrated: bool,
    /// Logical time of the newest observation that contributed.
    pub observed_at_ms: u64,
}

impl SceneContextSnapshot {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        let bad = |field, message: &str| Err(DomainValidationError::new(field, message));
        if self.schema_version != SCENE_CONTEXT_SCHEMA_VERSION {
            return bad("scene.schema_version", "unsupported schema version");
        }
        if !(1..=MAX_OBSERVATION_TIME_MS).contains(&self.version) {
            return bad("scene.version", "must be in 1..=2^53-1");
        }
        if self.observed_at_ms > MAX_OBSERVATION_TIME_MS {
            return bad(
                "scene.observed_at_ms",
                "exceeds the JSON-safe integer range",
            );
        }
        if self.labels.len() > MAX_SCENE_LABELS {
            return bad("scene.labels", "too many labels");
        }
        let mut seen = BTreeSet::new();
        for label in &self.labels {
            if !is_scene_label(label) {
                return bad("scene.labels", "label is outside the closed vocabulary");
            }
            if !seen.insert(label.as_str()) {
                return bad("scene.labels", "labels must be unique");
            }
        }
        if (self.scene_class == SceneClass::Unknown) != (self.source_rank == SceneSourceRank::None)
        {
            return bad(
                "scene.source_rank",
                "scene_class is unknown exactly when no source supplied it",
            );
        }
        if self.labels.is_empty() != (self.labels_source_rank == SceneSourceRank::None) {
            return bad(
                "scene.labels_source_rank",
                "labels are empty exactly when no source supplied them",
            );
        }
        // The authority rule: an untrusted or absent source can never carry an
        // interruptibility.
        if self.interruptibility != Interruptibility::Unknown && !self.source_rank.is_trusted() {
            return bad(
                "scene.interruptibility",
                "only a trusted source (rank 1 to 3) may set interruptibility",
            );
        }
        let perceived = self.source_rank == SceneSourceRank::Perception
            || self.labels_source_rank == SceneSourceRank::Perception;
        if !perceived && (self.source_ref.is_some() || self.confidence.is_some() || self.calibrated)
        {
            return bad(
                "scene.source_ref",
                "source_ref, confidence and calibrated describe a perceived part",
            );
        }
        if let Some(alias) = &self.source_ref {
            validate_alias("scene.source_ref", alias)?;
        }
        if let Some(confidence) = self.confidence
            && !(confidence.is_finite() && (0.0..=1.0).contains(&confidence))
        {
            return bad("scene.confidence", "must be in 0..=1");
        }
        Ok(())
    }
}

/// A change of scene class, the only thing that is an event. Labels,
/// confidence and refreshed observations never produce one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneTransition {
    pub from: SceneClass,
    pub to: SceneClass,
    /// Version of the snapshot that carries the new class.
    pub version: u64,
    pub at_ms: u64,
    pub source_rank: SceneSourceRank,
}

fn validate_alias(field: &'static str, alias: &str) -> Result<(), DomainValidationError> {
    let ok = !alias.is_empty()
        && alias.len() <= 64
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(DomainValidationError::new(
            field,
            "must be an operator alias",
        ))
    }
}

/// A scene class an operator may map an OBS scene to (`unknown` is not a
/// mapping: it would only be silence).
fn mappable(class: SceneClass) -> bool {
    class != SceneClass::Unknown
}

/// One row of the operator's table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObsSceneMapEntry {
    pub scene_class: SceneClass,
    /// What the operator says about interrupting while this scene is live.
    /// Defaults to `unknown`.
    #[serde(default)]
    pub interruptibility: Interruptibility,
    /// Optional closed-vocabulary labels for this scene.
    #[serde(default)]
    pub labels: Vec<String>,
}

/// The operator-owned table from an exact OBS program scene name to a scene
/// class. It is configuration, trusted because the operator wrote it; the OBS
/// scene name itself is only ever a lookup key and is never copied into a
/// snapshot, a request or a log line.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObsSceneMap {
    pub schema_version: String,
    pub scenes: BTreeMap<String, ObsSceneMapEntry>,
}

impl ObsSceneMap {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        let bad = |field, message: &str| Err(DomainValidationError::new(field, message));
        if self.schema_version != OBS_SCENE_MAP_SCHEMA_VERSION {
            return bad("obs_scene_map.schema_version", "unsupported schema version");
        }
        if self.scenes.len() > MAX_OBS_SCENE_MAP_ENTRIES {
            return bad("obs_scene_map.scenes", "too many entries");
        }
        for (name, entry) in &self.scenes {
            if name.is_empty()
                || name.len() > MAX_OBS_SCENE_NAME_BYTES
                || name.chars().any(char::is_control)
            {
                return bad(
                    "obs_scene_map.scenes",
                    "scene names must be 1..=128 bytes without control characters",
                );
            }
            if !mappable(entry.scene_class) {
                return bad("obs_scene_map.scene_class", "unknown is not a mapping");
            }
            if entry.labels.len() > MAX_SCENE_LABELS {
                return bad("obs_scene_map.labels", "too many labels");
            }
            let mut seen = BTreeSet::new();
            for label in &entry.labels {
                validate_perception_label_token("obs_scene_map.labels", label)?;
                if !is_scene_label(label) {
                    return bad(
                        "obs_scene_map.labels",
                        "label is outside the closed vocabulary",
                    );
                }
                if !seen.insert(label.as_str()) {
                    return bad("obs_scene_map.labels", "labels must be unique");
                }
            }
        }
        Ok(())
    }

    /// Parse and validate one table. The caller bounds the byte length first
    /// ([`MAX_OBS_SCENE_MAP_BYTES`]); this refuses an oversize slice anyway.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, DomainValidationError> {
        if bytes.len() > MAX_OBS_SCENE_MAP_BYTES {
            return Err(DomainValidationError::new(
                "obs_scene_map",
                "table exceeds the size bound",
            ));
        }
        let map: Self = serde_json::from_slice(bytes)
            .map_err(|error| DomainValidationError::new("obs_scene_map", error.to_string()))?;
        map.validate()?;
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> SceneContextSnapshot {
        SceneContextSnapshot {
            schema_version: SCENE_CONTEXT_SCHEMA_VERSION.to_owned(),
            version: 1,
            scene_class: SceneClass::Gameplay,
            labels: vec!["combat".to_owned()],
            interruptibility: Interruptibility::Unknown,
            source_rank: SceneSourceRank::Perception,
            labels_source_rank: SceneSourceRank::Perception,
            source_ref: Some("game-main".to_owned()),
            confidence: Some(0.6),
            calibrated: false,
            observed_at_ms: 100,
        }
    }

    #[test]
    fn perceived_snapshot_is_valid_and_cannot_carry_interruptibility() {
        assert!(snapshot().validate().is_ok());
        for value in [
            Interruptibility::Interruptible,
            Interruptibility::Uninterruptible,
        ] {
            let mut perceived = snapshot();
            perceived.interruptibility = value;
            assert_eq!(
                perceived.validate().unwrap_err().field(),
                "scene.interruptibility"
            );
        }
        let mut obs = snapshot();
        obs.source_rank = SceneSourceRank::ObsState;
        obs.interruptibility = Interruptibility::Uninterruptible;
        assert!(obs.validate().is_ok());
    }

    #[test]
    fn rank_and_emptiness_agree() {
        let mut unknown_with_rank = snapshot();
        unknown_with_rank.scene_class = SceneClass::Unknown;
        assert!(unknown_with_rank.validate().is_err());
        let mut labels_without_rank = snapshot();
        labels_without_rank.labels_source_rank = SceneSourceRank::None;
        assert!(labels_without_rank.validate().is_err());
        let mut outside = snapshot();
        outside.labels = vec!["not_a_label".to_owned()];
        assert!(outside.validate().is_err());
        let mut duplicate = snapshot();
        duplicate.labels = vec!["combat".to_owned(), "combat".to_owned()];
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn trusted_snapshot_carries_no_perception_provenance() {
        let mut trusted = snapshot();
        trusted.source_rank = SceneSourceRank::ObsState;
        trusted.labels = Vec::new();
        trusted.labels_source_rank = SceneSourceRank::None;
        assert!(trusted.validate().is_err(), "alias and confidence remain");
        trusted.source_ref = None;
        trusted.confidence = None;
        assert!(trusted.validate().is_ok());
    }

    #[test]
    fn unknown_field_and_wire_shape_are_refused() {
        let mut value = serde_json::to_value(snapshot()).unwrap();
        value["authority"] = serde_json::json!("operator");
        assert!(serde_json::from_value::<SceneContextSnapshot>(value).is_err());
    }

    #[test]
    fn obs_scene_map_rejects_unknown_class_and_foreign_labels() {
        let ok = br#"{"schema_version":"0.1.0","scenes":{"Game":{"scene_class":"gameplay","labels":["combat"]}}}"#;
        assert!(ObsSceneMap::from_json_bytes(ok).is_ok());
        for bad in [
            &br#"{"schema_version":"0.1.0","scenes":{"Game":{"scene_class":"unknown"}}}"#[..],
            br#"{"schema_version":"0.1.0","scenes":{"Game":{"scene_class":"gameplay","labels":["x"]}}}"#,
            br#"{"schema_version":"0.1.0","scenes":{"":{"scene_class":"gameplay"}}}"#,
            br#"{"schema_version":"0.1.0","scenes":{"G":{"scene_class":"gameplay","authority":1}}}"#,
        ] {
            assert!(ObsSceneMap::from_json_bytes(bad).is_err());
        }
    }

    #[test]
    fn perception_classes_map_into_the_closed_vocabulary() {
        for class in [
            PerceptionSceneClass::Gameplay,
            PerceptionSceneClass::Menu,
            PerceptionSceneClass::Loading,
            PerceptionSceneClass::Cutscene,
            PerceptionSceneClass::Result,
            PerceptionSceneClass::Desktop,
            PerceptionSceneClass::Unknown,
        ] {
            assert_eq!(SceneClass::from(class).name(), class.name());
        }
    }

    fn parse_snapshot(bytes: &[u8]) -> Result<SceneContextSnapshot, DomainValidationError> {
        let snapshot: SceneContextSnapshot = serde_json::from_slice(bytes)
            .map_err(|error| DomainValidationError::new("scene", error.to_string()))?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Fixture -> Rust parity: the files `bun run validate` checks against the
    /// schemas are accepted and rejected identically by the Rust validators.
    #[test]
    fn schema_fixtures_agree_with_the_rust_validators() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        for (dir, should_pass) in [("valid", true), ("invalid", false), ("inconsistent", false)] {
            let mut count = 0;
            for entry in std::fs::read_dir(root.join("scene-context").join(dir)).expect("dir") {
                let path = entry.expect("entry").path();
                let bytes = std::fs::read(&path).expect("read");
                assert_eq!(
                    parse_snapshot(&bytes).is_ok(),
                    should_pass,
                    "{}",
                    path.display()
                );
                count += 1;
            }
            assert!(count > 0, "scene-context/{dir} has fixtures");
            let mut count = 0;
            for entry in std::fs::read_dir(root.join("obs-scene-maps").join(dir)).expect("dir") {
                let path = entry.expect("entry").path();
                let bytes = std::fs::read(&path).expect("read");
                assert_eq!(
                    ObsSceneMap::from_json_bytes(&bytes).is_ok(),
                    should_pass,
                    "{}",
                    path.display()
                );
                count += 1;
            }
            assert!(count > 0, "obs-scene-maps/{dir} has fixtures");
        }
    }

    #[test]
    fn a_perceived_snapshot_cannot_become_a_trusted_interruptibility() {
        // Even a hand-built, invalid rank-4 snapshot that claims a value.
        let mut perceived = snapshot();
        perceived.interruptibility = Interruptibility::Uninterruptible;
        assert!(perceived.validate().is_err());
        assert_eq!(
            TrustedInterruptibility::from_snapshot(&perceived).get(),
            Interruptibility::Unknown
        );
        let mut trusted = perceived.clone();
        trusted.source_rank = SceneSourceRank::ObsState;
        assert_eq!(
            TrustedInterruptibility::from_snapshot(&trusted).get(),
            Interruptibility::Uninterruptible
        );
        assert_eq!(
            TrustedInterruptibility::default().get(),
            Interruptibility::Unknown
        );
    }
}
