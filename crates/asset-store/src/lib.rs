#![forbid(unsafe_code)]

//! Executable Performance Asset loader, compatibility gate, and local cache.
//!
//! L0 is validated in-memory state. L1 is a local filesystem descriptor index.
//! This crate performs no network I/O and keeps binary media as external refs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const PERFORMANCE_ASSET_SCHEMA_VERSION: &str = "0.1.0";
pub const MAX_SEMANTIC_EMBEDDING_DIMENSIONS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTier {
    Memory,
    LocalStorage,
    Generated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetClass {
    Filler,
    Reaction,
    Phrase,
    Template,
    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GazeTarget {
    Camera,
    Chat,
    Game,
    Speaker,
    Away,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTrack {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub audio_ref: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub viseme_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpressionTrack {
    pub preset: String,
    pub intensity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GestureTrack {
    pub preset: String,
    #[serde(default)]
    pub amplitude: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineEvent {
    pub at_ms: u64,
    pub event: String,
    #[serde(default)]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariationSpec {
    #[serde(default)]
    pub speed_pct: Option<f64>,
    #[serde(default)]
    pub amplitude_pct: Option<f64>,
    #[serde(default)]
    pub start_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetCompatibility {
    pub compiler_version: String,
    #[serde(default)]
    pub voice_model: Option<String>,
    #[serde(default)]
    pub avatar_profile: Option<String>,
    #[serde(default)]
    pub viseme_mapping: Option<String>,
    #[serde(default)]
    pub motion_library: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticEmbedding {
    pub model: String,
    pub model_version: String,
    pub vector: Vec<f32>,
}

impl SemanticEmbedding {
    pub fn model_key(&self) -> String {
        format!("{}@{}", self.model, self.model_version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    #[serde(default)]
    pub generated: Option<bool>,
    #[serde(default)]
    pub generator: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub thinking_backend: Option<String>,
    #[serde(default)]
    pub thinking_model_alias: Option<String>,
    #[serde(default)]
    pub thinking_model_version: Option<String>,
    #[serde(default)]
    pub tts_backend: Option<String>,
    #[serde(default)]
    pub tts_model_alias: Option<String>,
    #[serde(default)]
    pub tts_model_version: Option<String>,
    #[serde(default)]
    pub routing_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceAsset {
    pub schema_version: String,
    pub id: String,
    pub intent: String,
    pub class: AssetClass,
    #[serde(default)]
    pub variant_group: Option<String>,
    #[serde(default)]
    pub speech: Option<SpeechTrack>,
    #[serde(default)]
    pub expression: Option<ExpressionTrack>,
    #[serde(default)]
    pub gesture: Option<GestureTrack>,
    #[serde(default)]
    pub gaze: Option<GazeTarget>,
    pub timeline: Vec<TimelineEvent>,
    #[serde(default)]
    pub interrupt_points_ms: Vec<u64>,
    #[serde(default)]
    pub variation: Option<VariationSpec>,
    pub compatibility: AssetCompatibility,
    #[serde(default)]
    pub semantic_embedding: Option<SemanticEmbedding>,
    #[serde(default)]
    pub provenance: Option<Provenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
}

impl ValidationIssue {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetValidationError {
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for AssetValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.issues.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{}: {}", issue.field, issue.message)?;
        }
        Ok(())
    }
}

impl Error for AssetValidationError {}

impl PerformanceAsset {
    pub fn validate(&self) -> Result<(), AssetValidationError> {
        let mut issues = Vec::new();

        if self.schema_version != PERFORMANCE_ASSET_SCHEMA_VERSION {
            issues.push(ValidationIssue::new(
                "schema_version",
                format!(
                    "expected {PERFORMANCE_ASSET_SCHEMA_VERSION}, got {}",
                    self.schema_version
                ),
            ));
        }
        if !is_valid_asset_id(&self.id) {
            issues.push(ValidationIssue::new(
                "id",
                "must match ^[a-z0-9][a-z0-9._-]{2,127}$",
            ));
        }
        if self.intent.trim().is_empty() {
            issues.push(ValidationIssue::new("intent", "must not be empty"));
        }
        validate_optional_nonempty("variant_group", self.variant_group.as_deref(), &mut issues);

        if let Some(speech) = &self.speech {
            validate_optional_nonempty("speech.text", speech.text.as_deref(), &mut issues);
            if let Some(audio_ref) = speech.audio_ref.as_deref() {
                validate_media_ref("speech.audio_ref", audio_ref, &mut issues);
                if speech.duration_ms.is_none() {
                    issues.push(ValidationIssue::new(
                        "speech.duration_ms",
                        "is required when speech.audio_ref is present",
                    ));
                }
            }
            if let Some(viseme_ref) = speech.viseme_ref.as_deref() {
                validate_media_ref("speech.viseme_ref", viseme_ref, &mut issues);
                if speech.audio_ref.is_none() {
                    issues.push(ValidationIssue::new(
                        "speech.viseme_ref",
                        "requires speech.audio_ref",
                    ));
                }
            }
        }

        if let Some(expression) = &self.expression {
            if expression.preset.trim().is_empty() {
                issues.push(ValidationIssue::new(
                    "expression.preset",
                    "must not be empty",
                ));
            }
            validate_number_range(
                "expression.intensity",
                expression.intensity,
                0.0,
                1.0,
                &mut issues,
            );
        }

        if let Some(gesture) = &self.gesture {
            if gesture.preset.trim().is_empty() {
                issues.push(ValidationIssue::new("gesture.preset", "must not be empty"));
            }
            if let Some(amplitude) = gesture.amplitude {
                validate_number_range("gesture.amplitude", amplitude, 0.0, 2.0, &mut issues);
            }
        }

        let mut previous_timeline_ms = None;
        for (index, event) in self.timeline.iter().enumerate() {
            if event.event.trim().is_empty() {
                issues.push(ValidationIssue::new(
                    format!("timeline[{index}].event"),
                    "must not be empty",
                ));
            }
            if previous_timeline_ms.is_some_and(|previous| event.at_ms < previous) {
                issues.push(ValidationIssue::new(
                    format!("timeline[{index}].at_ms"),
                    "timeline must be monotonic non-decreasing",
                ));
            }
            previous_timeline_ms = Some(event.at_ms);
        }

        let mut seen_interrupts = BTreeSet::new();
        let mut previous_interrupt = None;
        let speech_duration = self.speech.as_ref().and_then(|speech| speech.duration_ms);
        for (index, point) in self.interrupt_points_ms.iter().copied().enumerate() {
            if !seen_interrupts.insert(point) {
                issues.push(ValidationIssue::new(
                    format!("interrupt_points_ms[{index}]"),
                    "duplicate interrupt point",
                ));
            }
            if previous_interrupt.is_some_and(|previous| point <= previous) {
                issues.push(ValidationIssue::new(
                    format!("interrupt_points_ms[{index}]"),
                    "interrupt points must be strictly increasing",
                ));
            }
            if speech_duration.is_some_and(|duration| point > duration) {
                issues.push(ValidationIssue::new(
                    format!("interrupt_points_ms[{index}]"),
                    format!(
                        "interrupt point {point}ms exceeds speech duration {}ms",
                        speech_duration.unwrap_or_default()
                    ),
                ));
            }
            previous_interrupt = Some(point);
        }

        if let Some(variation) = self.variation {
            if let Some(value) = variation.speed_pct {
                validate_number_range("variation.speed_pct", value, 0.0, 25.0, &mut issues);
            }
            if let Some(value) = variation.amplitude_pct {
                validate_number_range("variation.amplitude_pct", value, 0.0, 25.0, &mut issues);
            }
            if variation.start_delay_ms.is_some_and(|value| value > 500) {
                issues.push(ValidationIssue::new(
                    "variation.start_delay_ms",
                    "must be between 0 and 500",
                ));
            }
        }

        if self.compatibility.compiler_version.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "compatibility.compiler_version",
                "must not be empty",
            ));
        }
        validate_optional_nonempty(
            "compatibility.voice_model",
            self.compatibility.voice_model.as_deref(),
            &mut issues,
        );
        validate_optional_nonempty(
            "compatibility.avatar_profile",
            self.compatibility.avatar_profile.as_deref(),
            &mut issues,
        );
        validate_optional_nonempty(
            "compatibility.viseme_mapping",
            self.compatibility.viseme_mapping.as_deref(),
            &mut issues,
        );
        validate_optional_nonempty(
            "compatibility.motion_library",
            self.compatibility.motion_library.as_deref(),
            &mut issues,
        );

        if let Some(provenance) = &self.provenance {
            validate_optional_nonempty(
                "provenance.generator",
                provenance.generator.as_deref(),
                &mut issues,
            );
            validate_optional_nonempty(
                "provenance.thinking_backend",
                provenance.thinking_backend.as_deref(),
                &mut issues,
            );
            validate_optional_nonempty(
                "provenance.tts_backend",
                provenance.tts_backend.as_deref(),
                &mut issues,
            );
            validate_optional_nonempty(
                "provenance.routing_reason",
                provenance.routing_reason.as_deref(),
                &mut issues,
            );

            if provenance.generated == Some(true) {
                for (field, value) in [
                    ("provenance.generator", provenance.generator.as_deref()),
                    ("provenance.created_at", provenance.created_at.as_deref()),
                    (
                        "provenance.thinking_backend",
                        provenance.thinking_backend.as_deref(),
                    ),
                    ("provenance.tts_backend", provenance.tts_backend.as_deref()),
                    (
                        "provenance.routing_reason",
                        provenance.routing_reason.as_deref(),
                    ),
                ] {
                    if value.is_none() {
                        issues.push(ValidationIssue::new(
                            field,
                            "is required when provenance.generated is true",
                        ));
                    }
                }
            }
        }

        if let Some(embedding) = &self.semantic_embedding {
            if embedding.model.trim().is_empty() {
                issues.push(ValidationIssue::new(
                    "semantic_embedding.model",
                    "must not be empty",
                ));
            }
            if embedding.model_version.trim().is_empty() {
                issues.push(ValidationIssue::new(
                    "semantic_embedding.model_version",
                    "must not be empty",
                ));
            }
            if embedding.vector.is_empty()
                || embedding.vector.len() > MAX_SEMANTIC_EMBEDDING_DIMENSIONS
            {
                issues.push(ValidationIssue::new(
                    "semantic_embedding.vector",
                    format!("must contain 1..={MAX_SEMANTIC_EMBEDDING_DIMENSIONS} dimensions"),
                ));
            } else if embedding.vector.iter().any(|value| !value.is_finite()) {
                issues.push(ValidationIssue::new(
                    "semantic_embedding.vector",
                    "must contain only finite values",
                ));
            } else {
                let norm_squared: f64 = embedding
                    .vector
                    .iter()
                    .map(|value| f64::from(*value) * f64::from(*value))
                    .sum();
                if norm_squared == 0.0 {
                    issues.push(ValidationIssue::new(
                        "semantic_embedding.vector",
                        "must have non-zero norm",
                    ));
                }
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(AssetValidationError { issues })
        }
    }

    pub fn identity(&self) -> AssetIdentity {
        AssetIdentity::from(self)
    }
}

fn is_valid_asset_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if !(3..=128).contains(&bytes.len()) {
        return false;
    }
    let Some(first) = bytes.first().copied() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes.iter().copied().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    })
}

fn validate_number_range(
    field: &str,
    value: f64,
    minimum: f64,
    maximum: f64,
    issues: &mut Vec<ValidationIssue>,
) {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        issues.push(ValidationIssue::new(
            field,
            format!("must be a finite number between {minimum} and {maximum}"),
        ));
    }
}

fn validate_optional_nonempty(field: &str, value: Option<&str>, issues: &mut Vec<ValidationIssue>) {
    if value.is_some_and(|item| item.trim().is_empty()) {
        issues.push(ValidationIssue::new(
            field,
            "must not be empty when present",
        ));
    }
}

fn validate_media_ref(field: &str, value: &str, issues: &mut Vec<ValidationIssue>) {
    let value = value.trim();
    if value.is_empty() {
        issues.push(ValidationIssue::new(field, "must not be empty"));
        return;
    }
    if value.to_ascii_lowercase().starts_with("data:") {
        issues.push(ValidationIssue::new(
            field,
            "embedded data URIs are forbidden; binary media must remain external",
        ));
        return;
    }

    if value.contains("://") {
        return;
    }

    let path = Path::new(value);
    let looks_like_windows_absolute = value
        .as_bytes()
        .get(1)
        .is_some_and(|separator| *separator == b':');
    if path.is_absolute() || looks_like_windows_absolute {
        issues.push(ValidationIssue::new(
            field,
            "local media references must be relative paths",
        ));
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        issues.push(ValidationIssue::new(
            field,
            "local media references must not escape the asset root",
        ));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCompatibility {
    pub compiler_version: String,
    pub voice_model: Option<String>,
    pub avatar_profile: Option<String>,
    pub viseme_mapping: Option<String>,
    pub motion_library: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityMismatch {
    pub field: &'static str,
    pub asset_requires: String,
    pub runtime_has: Option<String>,
}

impl fmt::Display for CompatibilityMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} requires {:?}, runtime has {:?}",
            self.field, self.asset_requires, self.runtime_has
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatibilityStatus {
    Usable,
    Incompatible(Vec<CompatibilityMismatch>),
}

impl CompatibilityStatus {
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Usable)
    }
}

impl AssetCompatibility {
    pub fn check(&self, runtime: &RuntimeCompatibility) -> CompatibilityStatus {
        let mut mismatches = Vec::new();

        if self.compiler_version != runtime.compiler_version {
            mismatches.push(CompatibilityMismatch {
                field: "compiler_version",
                asset_requires: self.compiler_version.clone(),
                runtime_has: Some(runtime.compiler_version.clone()),
            });
        }
        check_optional_compatibility(
            "voice_model",
            self.voice_model.as_deref(),
            runtime.voice_model.as_deref(),
            &mut mismatches,
        );
        check_optional_compatibility(
            "avatar_profile",
            self.avatar_profile.as_deref(),
            runtime.avatar_profile.as_deref(),
            &mut mismatches,
        );
        check_optional_compatibility(
            "viseme_mapping",
            self.viseme_mapping.as_deref(),
            runtime.viseme_mapping.as_deref(),
            &mut mismatches,
        );
        check_optional_compatibility(
            "motion_library",
            self.motion_library.as_deref(),
            runtime.motion_library.as_deref(),
            &mut mismatches,
        );

        if mismatches.is_empty() {
            CompatibilityStatus::Usable
        } else {
            CompatibilityStatus::Incompatible(mismatches)
        }
    }
}

fn check_optional_compatibility(
    field: &'static str,
    asset_requires: Option<&str>,
    runtime_has: Option<&str>,
    mismatches: &mut Vec<CompatibilityMismatch>,
) {
    if let Some(required) = asset_requires
        && runtime_has != Some(required)
    {
        mismatches.push(CompatibilityMismatch {
            field,
            asset_requires: required.to_owned(),
            runtime_has: runtime_has.map(str::to_owned),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetIdentity {
    pub id: String,
    pub schema_version: String,
    pub compiler_version: String,
    pub voice_model: Option<String>,
    pub avatar_profile: Option<String>,
    pub viseme_mapping: Option<String>,
    pub motion_library: Option<String>,
}

impl From<&PerformanceAsset> for AssetIdentity {
    fn from(asset: &PerformanceAsset) -> Self {
        Self {
            id: asset.id.clone(),
            schema_version: asset.schema_version.clone(),
            compiler_version: asset.compatibility.compiler_version.clone(),
            voice_model: asset.compatibility.voice_model.clone(),
            avatar_profile: asset.compatibility.avatar_profile.clone(),
            viseme_mapping: asset.compatibility.viseme_mapping.clone(),
            motion_library: asset.compatibility.motion_library.clone(),
        }
    }
}

impl AssetIdentity {
    pub fn stable_key(&self) -> String {
        let mut output = String::new();
        append_identity_component(&mut output, "id", Some(&self.id));
        append_identity_component(&mut output, "schema", Some(&self.schema_version));
        append_identity_component(&mut output, "compiler", Some(&self.compiler_version));
        append_identity_component(&mut output, "voice", self.voice_model.as_deref());
        append_identity_component(&mut output, "avatar", self.avatar_profile.as_deref());
        append_identity_component(&mut output, "viseme", self.viseme_mapping.as_deref());
        append_identity_component(&mut output, "motion", self.motion_library.as_deref());
        output
    }
}

fn append_identity_component(output: &mut String, name: &str, value: Option<&str>) {
    output.push_str(name);
    output.push('=');
    match value {
        Some(value) => {
            output.push_str(&value.len().to_string());
            output.push(':');
            output.push_str(value);
        }
        None => output.push('-'),
    }
    output.push('|');
}

#[derive(Debug)]
pub enum AssetStoreError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Json {
        path: PathBuf,
        message: String,
    },
    InvalidAsset {
        path: Option<PathBuf>,
        source: AssetValidationError,
    },
    DuplicateId {
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
    NotFound {
        id: String,
    },
    Incompatible {
        id: String,
        mismatches: Vec<CompatibilityMismatch>,
    },
    NotGenerated {
        id: String,
    },
    AlreadyPersisted {
        id: String,
        path: PathBuf,
    },
}

impl fmt::Display for AssetStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "I/O error at {}: {source}", path.display())
            }
            Self::Json { path, message } => {
                write!(f, "invalid JSON asset {}: {message}", path.display())
            }
            Self::InvalidAsset { path, source } => {
                if let Some(path) = path {
                    write!(f, "invalid Performance Asset {}: {source}", path.display())
                } else {
                    write!(f, "invalid Performance Asset: {source}")
                }
            }
            Self::DuplicateId { id, first, second } => write!(
                f,
                "duplicate asset id {id:?}: {} and {}",
                first.display(),
                second.display()
            ),
            Self::NotFound { id } => write!(f, "asset {id:?} was not found in L0 or L1"),
            Self::Incompatible { id, mismatches } => {
                write!(f, "asset {id:?} is incompatible")?;
                for mismatch in mismatches {
                    write!(f, "; {mismatch}")?;
                }
                Ok(())
            }
            Self::NotGenerated { id } => {
                write!(f, "asset {id:?} is not an eligible generated asset")
            }
            Self::AlreadyPersisted { id, path } => write!(
                f,
                "asset {id:?} already has a persisted descriptor at {}",
                path.display()
            ),
        }
    }
}

impl Error for AssetStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidAsset { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn ensure_generated(asset: &PerformanceAsset) -> Result<(), AssetStoreError> {
    if asset
        .provenance
        .as_ref()
        .is_some_and(|provenance| provenance.generated == Some(true))
    {
        Ok(())
    } else {
        Err(AssetStoreError::NotGenerated {
            id: asset.id.clone(),
        })
    }
}

pub fn load_asset_file(path: impl AsRef<Path>) -> Result<PerformanceAsset, AssetStoreError> {
    let path = path.as_ref();
    let text = fs::read_to_string(path).map_err(|source| AssetStoreError::Io {
        path: path.to_owned(),
        source,
    })?;
    let asset: PerformanceAsset =
        serde_json::from_str(&text).map_err(|source| AssetStoreError::Json {
            path: path.to_owned(),
            message: format!(
                "{} at line {}, column {}",
                source,
                source.line(),
                source.column()
            ),
        })?;
    asset
        .validate()
        .map_err(|source| AssetStoreError::InvalidAsset {
            path: Some(path.to_owned()),
            source,
        })?;
    Ok(asset)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssetIndexEntry {
    pub identity: AssetIdentity,
    pub path: PathBuf,
    pub class: AssetClass,
    pub variant_group: Option<String>,
    pub compatibility: CompatibilityStatus,
    pub semantic_embedding: Option<SemanticEmbedding>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub indexed: usize,
    pub usable: usize,
    pub incompatible: usize,
}

/// Bounded-retention policy for generated/dynamic L0 assets (issue #53).
///
/// Static/preloaded assets are pinned and never evicted by dynamic churn.
/// Dynamic eviction is deterministic LRU with a stable asset-id tie-break so
/// identical recorded inputs/config always produce identical survivor sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotCacheConfig {
    /// Maximum number of generated/dynamic assets resident in L0.
    pub max_dynamic_assets: usize,
    /// Optional estimated-byte budget for generated/dynamic assets. A single
    /// asset larger than the budget still resides (capacity of one floor).
    pub max_dynamic_bytes: usize,
}

impl Default for HotCacheConfig {
    fn default() -> Self {
        Self {
            max_dynamic_assets: 1024,
            max_dynamic_bytes: usize::MAX,
        }
    }
}

/// Cache occupancy and hit/miss counters (issue #53).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HotCacheMetrics {
    pub pinned_resident: usize,
    pub dynamic_resident: usize,
    pub estimated_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

/// Lightweight promotion/adaptation metadata that survives full-asset
/// eviction so #39 promotion scoring keeps its inputs without retaining all
/// media/descriptor state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromotionMetadata {
    pub id: String,
    pub use_count: u64,
    pub last_used_at_ms: Option<u64>,
}

/// Monotonic logical-use counter for deterministic LRU ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HotUsage {
    use_tick: u64,
    last_used_at_ms: Option<u64>,
    note_count: u64,
}

fn estimate_asset_bytes(asset: &PerformanceAsset) -> usize {
    // Estimated residency cost: the descriptor's serialized footprint.
    serde_json::to_vec(asset)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

#[derive(Debug)]
pub struct AssetStore {
    root: PathBuf,
    runtime: RuntimeCompatibility,
    hot: BTreeMap<String, Arc<PerformanceAsset>>,
    /// Which resident assets are pinned (static/preloaded) vs dynamic.
    pinned_ids: BTreeSet<String>,
    hot_config: HotCacheConfig,
    hot_usage: BTreeMap<String, HotUsage>,
    /// Promotion metadata that survives eviction (#39/#53).
    promotion_metadata: BTreeMap<String, PromotionMetadata>,
    hot_hits: u64,
    hot_misses: u64,
    hot_evictions: u64,
    next_use_tick: u64,
    local: BTreeMap<String, AssetIndexEntry>,
}

impl AssetStore {
    pub fn new(root: impl Into<PathBuf>, runtime: RuntimeCompatibility) -> Self {
        Self {
            root: root.into(),
            runtime,
            hot: BTreeMap::new(),
            pinned_ids: BTreeSet::new(),
            hot_config: HotCacheConfig::default(),
            hot_usage: BTreeMap::new(),
            promotion_metadata: BTreeMap::new(),
            hot_hits: 0,
            hot_misses: 0,
            hot_evictions: 0,
            next_use_tick: 0,
            local: BTreeMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runtime(&self) -> &RuntimeCompatibility {
        &self.runtime
    }

    pub fn hot_len(&self) -> usize {
        self.hot.len()
    }

    /// Configure the bounded-retention policy for generated/dynamic L0
    /// assets (issue #53). Applying the config trims residency immediately.
    pub fn set_hot_cache_config(&mut self, config: HotCacheConfig) {
        self.hot_config = config;
        self.evict_dynamic_to_capacity();
    }

    /// Cache occupancy and hit/miss/eviction counters.
    pub fn hot_cache_metrics(&self) -> HotCacheMetrics {
        let estimated_bytes: usize = self
            .hot
            .values()
            .map(|asset| estimate_asset_bytes(asset))
            .sum();
        HotCacheMetrics {
            pinned_resident: self
                .hot
                .keys()
                .filter(|id| self.pinned_ids.contains(*id))
                .count(),
            dynamic_resident: self
                .hot
                .keys()
                .filter(|id| !self.pinned_ids.contains(*id))
                .count(),
            estimated_bytes,
            hits: self.hot_hits,
            misses: self.hot_misses,
            evictions: self.hot_evictions,
        }
    }

    /// Record a use of a hot asset at a logical time, feeding both the
    /// deterministic LRU clock and the lightweight promotion metadata.
    pub fn note_hot_use(&mut self, id: &str, at_ms: u64) {
        let tick = self.next_use_tick;
        self.next_use_tick = self.next_use_tick.saturating_add(1);
        let usage = self.hot_usage.entry(id.to_owned()).or_default();
        usage.use_tick = tick;
        usage.note_count = usage.note_count.saturating_add(1);
        usage.last_used_at_ms = Some(at_ms);
        let metadata = self.promotion_metadata.entry(id.to_owned()).or_default();
        metadata.id = id.to_owned();
        metadata.use_count = metadata.use_count.saturating_add(1);
        metadata.last_used_at_ms = Some(at_ms);
    }

    /// Lightweight promotion/adaptation metadata, independent of full hot
    /// asset retention (survives eviction; issue #39/#53).
    pub fn promotion_metadata(&self, id: &str) -> Option<&PromotionMetadata> {
        self.promotion_metadata.get(id)
    }

    /// Deterministically evict the least-recently-used dynamic assets until
    /// both the count capacity and the byte budget are respected. Ties on
    /// recency break by stable asset id (ascending evicts first).
    fn evict_dynamic_to_capacity(&mut self) {
        loop {
            let dynamic_ids: Vec<String> = self
                .hot
                .keys()
                .filter(|id| !self.pinned_ids.contains(*id))
                .cloned()
                .collect();

            if dynamic_ids.len() <= self.hot_config.max_dynamic_assets {
                let mut estimated_bytes: usize = self
                    .hot
                    .values()
                    .map(|asset| estimate_asset_bytes(asset))
                    .sum();
                if estimated_bytes <= self.hot_config.max_dynamic_bytes || dynamic_ids.is_empty() {
                    return;
                }
                // Byte budget exceeded: evict LRU dynamic assets until within
                // budget, always keeping at least one dynamic asset.
                let mut evict_candidates: Vec<(String, HotUsage)> = dynamic_ids
                    .iter()
                    .map(|id| {
                        (
                            id.clone(),
                            self.hot_usage.get(id).copied().unwrap_or_default(),
                        )
                    })
                    .collect();
                evict_candidates
                    .sort_by(|a, b| a.1.use_tick.cmp(&b.1.use_tick).then_with(|| a.0.cmp(&b.0)));
                while estimated_bytes > self.hot_config.max_dynamic_bytes
                    && evict_candidates.len() > 1
                {
                    let (victim_id, _) = evict_candidates.remove(0);
                    if let Some(asset) = self.hot.remove(&victim_id) {
                        estimated_bytes =
                            estimated_bytes.saturating_sub(estimate_asset_bytes(&asset));
                        self.hot_usage.remove(&victim_id);
                        self.hot_evictions = self.hot_evictions.saturating_add(1);
                    }
                }
                return;
            }

            // Count capacity exceeded: evict LRU with stable id tie-break.
            let mut evict_candidates: Vec<(String, HotUsage)> = dynamic_ids
                .iter()
                .map(|id| {
                    (
                        id.clone(),
                        self.hot_usage.get(id).copied().unwrap_or_default(),
                    )
                })
                .collect();
            evict_candidates
                .sort_by(|a, b| a.1.use_tick.cmp(&b.1.use_tick).then_with(|| a.0.cmp(&b.0)));
            let (victim_id, _) = evict_candidates.remove(0);
            self.hot.remove(&victim_id);
            self.hot_usage.remove(&victim_id);
            self.hot_evictions = self.hot_evictions.saturating_add(1);
        }
    }

    pub fn local_len(&self) -> usize {
        self.local.len()
    }

    pub fn tier(&self, id: &str) -> Option<CacheTier> {
        if self.hot.contains_key(id) {
            Some(CacheTier::Memory)
        } else if self.local.contains_key(id) {
            Some(CacheTier::LocalStorage)
        } else {
            None
        }
    }

    pub fn local_entry(&self, id: &str) -> Option<&AssetIndexEntry> {
        self.local.get(id)
    }

    /// Stable asset-id ordered view of the current validated L1 snapshot.
    ///
    /// Consumers such as semantic-index builders must use this snapshot rather
    /// than walking descriptor files independently, so compatibility and
    /// transactional reindexing stay owned by AssetStore.
    pub fn indexed_entries(&self) -> impl Iterator<Item = (&str, &AssetIndexEntry)> {
        self.local.iter().map(|(id, entry)| (id.as_str(), entry))
    }

    /// Validate and index descriptor JSON files directly under the L1 root.
    ///
    /// The update is transactional: invalid or duplicate descriptors leave
    /// the previous index untouched. Subdirectories are deliberately ignored
    /// because they may contain viseme or other non-asset JSON media.
    pub fn index_local(&mut self) -> Result<IndexReport, AssetStoreError> {
        let mut paths = fs::read_dir(&self.root)
            .map_err(|source| AssetStoreError::Io {
                path: self.root.clone(),
                source,
            })?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|source| AssetStoreError::Io {
                        path: self.root.clone(),
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        paths.retain(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        });
        paths.sort();

        let mut next: BTreeMap<String, AssetIndexEntry> = BTreeMap::new();
        let mut report = IndexReport::default();

        for path in paths {
            let asset = load_asset_file(&path)?;
            let compatibility = asset.compatibility.check(&self.runtime);
            let entry = AssetIndexEntry {
                identity: asset.identity(),
                path: path.clone(),
                class: asset.class,
                variant_group: asset.variant_group.clone(),
                compatibility: compatibility.clone(),
                semantic_embedding: asset.semantic_embedding.clone(),
            };

            if let Some(previous) = next.get(&asset.id) {
                return Err(AssetStoreError::DuplicateId {
                    id: asset.id,
                    first: previous.path.clone(),
                    second: path,
                });
            }

            report.indexed += 1;
            if compatibility.is_usable() {
                report.usable += 1;
            } else {
                report.incompatible += 1;
            }
            next.insert(asset.id, entry);
        }

        self.local = next;
        self.hot.retain(|id, asset| {
            self.local
                .get(id)
                .is_none_or(|entry| entry.identity == asset.identity())
        });
        Ok(report)
    }

    pub fn insert_hot(
        &mut self,
        asset: PerformanceAsset,
    ) -> Result<Arc<PerformanceAsset>, AssetStoreError> {
        asset
            .validate()
            .map_err(|source| AssetStoreError::InvalidAsset { path: None, source })?;

        if let CompatibilityStatus::Incompatible(mismatches) =
            asset.compatibility.check(&self.runtime)
        {
            return Err(AssetStoreError::Incompatible {
                id: asset.id,
                mismatches,
            });
        }

        let id = asset.id.clone();
        let asset = Arc::new(asset);
        let is_dynamic = asset
            .provenance
            .as_ref()
            .is_some_and(|provenance| provenance.generated == Some(true));
        // A fresh insert (or overwrite) counts as a use for recency ordering.
        let tick = self.next_use_tick;
        self.next_use_tick = self.next_use_tick.saturating_add(1);
        let usage = self.hot_usage.entry(id.clone()).or_default();
        usage.use_tick = tick;
        if !is_dynamic {
            self.pinned_ids.insert(id.clone());
        }
        self.hot.insert(id, Arc::clone(&asset));
        self.evict_dynamic_to_capacity();
        Ok(asset)
    }

    /// Persist an already validated generated L0 asset into the local L1 descriptor store.
    ///
    /// This method deliberately contains no promotion policy. Callers must make
    /// that deterministic decision before invoking this storage primitive.
    pub fn persist_generated_descriptor(&mut self, id: &str) -> Result<PathBuf, AssetStoreError> {
        let asset = self
            .hot_get(id)
            .ok_or_else(|| AssetStoreError::NotFound { id: id.to_owned() })?;
        ensure_generated(&asset)?;
        if let CompatibilityStatus::Incompatible(mismatches) =
            asset.compatibility.check(&self.runtime)
        {
            return Err(AssetStoreError::Incompatible {
                id: id.to_owned(),
                mismatches,
            });
        }
        if let Some(existing) = self.local.get(id) {
            return Err(AssetStoreError::AlreadyPersisted {
                id: id.to_owned(),
                path: existing.path.clone(),
            });
        }

        fs::create_dir_all(&self.root).map_err(|source| AssetStoreError::Io {
            path: self.root.clone(),
            source,
        })?;
        let destination = self.root.join(format!("{id}.json"));
        if destination.exists() {
            return Err(AssetStoreError::AlreadyPersisted {
                id: id.to_owned(),
                path: destination,
            });
        }
        let temporary = self
            .root
            .join(format!(".{id}.promote-{}.tmp", std::process::id()));
        let bytes =
            serde_json::to_vec_pretty(asset.as_ref()).map_err(|source| AssetStoreError::Json {
                path: destination.clone(),
                message: source.to_string(),
            })?;
        let mut file = fs::File::create(&temporary).map_err(|source| AssetStoreError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(&bytes)
            .map_err(|source| AssetStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| AssetStoreError::Io {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &destination).map_err(|source| AssetStoreError::Io {
            path: destination.clone(),
            source,
        })?;

        self.local.insert(
            id.to_owned(),
            AssetIndexEntry {
                identity: asset.identity(),
                path: destination.clone(),
                class: asset.class,
                variant_group: asset.variant_group.clone(),
                compatibility: CompatibilityStatus::Usable,
                semantic_embedding: asset.semantic_embedding.clone(),
            },
        );
        Ok(destination)
    }

    /// Remove a generated asset from both L1 and L0 after provenance validation.
    pub fn invalidate_generated_asset(&mut self, id: &str) -> Result<(), AssetStoreError> {
        let local_path = self.local.get(id).map(|entry| entry.path.clone());
        if let Some(path) = local_path.as_ref() {
            let persisted = load_asset_file(path)?;
            ensure_generated(&persisted)?;
        } else if let Some(asset) = self.hot_get(id) {
            ensure_generated(&asset)?;
        } else {
            return Err(AssetStoreError::NotFound { id: id.to_owned() });
        }

        if let Some(path) = local_path {
            fs::remove_file(&path).map_err(|source| AssetStoreError::Io { path, source })?;
            self.local.remove(id);
        }
        self.hot.remove(id);
        Ok(())
    }

    /// Pure L0 lookup. This performs no filesystem or network access.
    /// Records a cache hit/miss and touches recency for deterministic LRU.
    pub fn hot_get(&mut self, id: &str) -> Option<Arc<PerformanceAsset>> {
        if let Some(asset) = self.hot.get(id).cloned() {
            self.hot_hits = self.hot_hits.saturating_add(1);
            let tick = self.next_use_tick;
            self.next_use_tick = self.next_use_tick.saturating_add(1);
            let usage = self.hot_usage.entry(id.to_owned()).or_default();
            usage.use_tick = tick;
            return Some(asset);
        }
        self.hot_misses = self.hot_misses.saturating_add(1);
        None
    }

    /// Resolve L0 first, then the indexed local L1 descriptor.
    ///
    /// A compatible L1 hit is promoted into L0. The descriptor is revalidated
    /// and its stable identity is compared with the indexed identity so a file
    /// changed after indexing cannot silently bypass compatibility checks.
    pub fn resolve(&mut self, id: &str) -> Result<Arc<PerformanceAsset>, AssetStoreError> {
        self.resolve_with_tier(id).map(|(asset, _)| asset)
    }

    /// Resolve an asset and report the cache tier that satisfied this lookup.
    ///
    /// L1 hits are promoted to L0 after the returned tier is captured, so
    /// telemetry can distinguish a true memory hit from local descriptor I/O.
    pub fn resolve_with_tier(
        &mut self,
        id: &str,
    ) -> Result<(Arc<PerformanceAsset>, CacheTier), AssetStoreError> {
        if let Some(asset) = self.hot_get(id) {
            return Ok((asset, CacheTier::Memory));
        }

        let entry = self
            .local
            .get(id)
            .cloned()
            .ok_or_else(|| AssetStoreError::NotFound { id: id.to_owned() })?;

        if let CompatibilityStatus::Incompatible(mismatches) = entry.compatibility {
            return Err(AssetStoreError::Incompatible {
                id: id.to_owned(),
                mismatches,
            });
        }

        let asset = load_asset_file(&entry.path)?;
        if asset.id != id {
            return Err(AssetStoreError::Json {
                path: entry.path,
                message: format!(
                    "indexed id {id:?} changed to {:?}; rebuild the L1 index",
                    asset.id
                ),
            });
        }
        if asset.identity() != entry.identity {
            return Err(AssetStoreError::Json {
                path: entry.path,
                message: "compatibility identity changed; rebuild the L1 index".to_owned(),
            });
        }

        self.insert_hot(asset)
            .map(|asset| (asset, CacheTier::LocalStorage))
    }

    pub fn preload_compatible(&mut self) -> Result<usize, AssetStoreError> {
        let ids: Vec<String> = self
            .local
            .iter()
            .filter(|(_, entry)| entry.compatibility.is_usable())
            .map(|(id, _)| id.clone())
            .collect();

        for id in &ids {
            self.resolve(id)?;
        }
        Ok(ids.len())
    }

    /// Preload compatible descriptors from selected semantic classes into L0.
    pub fn preload_classes(&mut self, classes: &[AssetClass]) -> Result<usize, AssetStoreError> {
        let ids: Vec<String> = self
            .local
            .iter()
            .filter(|(_, entry)| entry.compatibility.is_usable() && classes.contains(&entry.class))
            .map(|(id, _)| id.clone())
            .collect();

        for id in &ids {
            self.resolve(id)?;
        }
        Ok(ids.len())
    }

    /// Deterministically select a compatible asset id from a variant group.
    ///
    /// Recently used ids are excluded while alternatives exist. If every
    /// candidate is recent, the full stable candidate set is used so playback
    /// cannot starve.
    pub fn select_variant_id(
        &self,
        variant_group: &str,
        seed: u64,
        recently_used: &[&str],
    ) -> Option<String> {
        let mut all = BTreeSet::new();

        for (id, entry) in &self.local {
            if entry.compatibility.is_usable()
                && entry.variant_group.as_deref() == Some(variant_group)
            {
                all.insert(id.clone());
            }
        }
        for (id, asset) in &self.hot {
            if asset.variant_group.as_deref() == Some(variant_group)
                && asset.compatibility.check(&self.runtime).is_usable()
            {
                all.insert(id.clone());
            }
        }

        if all.is_empty() {
            return None;
        }

        let recent: BTreeSet<&str> = recently_used.iter().copied().collect();
        let preferred: Vec<String> = all
            .iter()
            .filter(|id| !recent.contains(id.as_str()))
            .cloned()
            .collect();
        let candidates: Vec<String> = if preferred.is_empty() {
            all.into_iter().collect()
        } else {
            preferred
        };

        let index = stable_variant_index(seed, variant_group, candidates.len());
        candidates.get(index).cloned()
    }

    pub fn select_variant(
        &mut self,
        variant_group: &str,
        seed: u64,
        recently_used: &[&str],
    ) -> Result<Arc<PerformanceAsset>, AssetStoreError> {
        self.select_variant_with_tier(variant_group, seed, recently_used)
            .map(|(asset, _)| asset)
    }

    pub fn select_variant_with_tier(
        &mut self,
        variant_group: &str,
        seed: u64,
        recently_used: &[&str],
    ) -> Result<(Arc<PerformanceAsset>, CacheTier), AssetStoreError> {
        let id = self
            .select_variant_id(variant_group, seed, recently_used)
            .ok_or_else(|| AssetStoreError::NotFound {
                id: format!("variant_group:{variant_group}"),
            })?;
        self.resolve_with_tier(&id)
    }
}

fn stable_variant_index(seed: u64, variant_group: &str, len: usize) -> usize {
    debug_assert!(len > 0);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in seed.to_le_bytes().into_iter().chain(variant_group.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash % len as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/performance-assets")
            .join(name)
    }

    fn runtime() -> RuntimeCompatibility {
        RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some("example-voice-v1".to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        }
    }

    fn generated_runtime() -> RuntimeCompatibility {
        RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some("voice-ja-v2".to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v2".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        }
    }

    fn valid_asset() -> PerformanceAsset {
        load_asset_file(fixture("valid/reaction-surprise.json")).expect("valid fixture")
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aivtuber-asset-store-{name}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_asset(path: &Path, asset: &PerformanceAsset) {
        let text = serde_json::to_string_pretty(asset).expect("serialize asset");
        fs::write(path, text).expect("write asset");
    }

    #[test]
    fn valid_fixture_loads_and_identity_is_stable() {
        let asset = valid_asset();
        let key_a = asset.identity().stable_key();
        let key_b = asset.identity().stable_key();

        assert_eq!(key_a, key_b);
        assert!(key_a.contains("reaction.surprise.01"));
        assert!(key_a.contains("example-voice-v1"));
    }

    #[test]
    fn semantic_embedding_metadata_is_validated() {
        let mut asset = valid_asset();
        asset.semantic_embedding = Some(SemanticEmbedding {
            model: "starter-semantic".to_owned(),
            model_version: "1".to_owned(),
            vector: vec![0.0, 0.0, 0.0],
        });
        let error = asset.validate().expect_err("zero vector must fail");
        assert!(
            error
                .to_string()
                .contains("semantic_embedding.vector: must have non-zero norm")
        );

        asset.semantic_embedding = Some(SemanticEmbedding {
            model: "starter-semantic".to_owned(),
            model_version: "1".to_owned(),
            vector: vec![1.0, 0.0, 0.0],
        });
        asset.validate().expect("versioned embedding is valid");
    }

    #[test]
    fn invalid_fixture_fails_with_actionable_json_error() {
        let error = load_asset_file(fixture("invalid/missing-compiler-version.json"))
            .expect_err("fixture must fail");
        let message = error.to_string();

        assert!(message.contains("compiler_version"), "{message}");
        assert!(
            message.contains("missing-compiler-version.json"),
            "{message}"
        );
    }

    #[test]
    fn interrupt_points_must_be_sorted_unique_and_within_speech() {
        let mut asset = valid_asset();
        asset.interrupt_points_ms = vec![680, 680, 1_300];

        let error = asset.validate().expect_err("must reject interrupt points");
        let message = error.to_string();

        assert!(message.contains("duplicate interrupt point"), "{message}");
        assert!(message.contains("strictly increasing"), "{message}");
        assert!(message.contains("exceeds speech duration"), "{message}");
    }

    #[test]
    fn embedded_binary_data_is_rejected() {
        let mut asset = valid_asset();
        asset.speech.as_mut().expect("speech").audio_ref =
            Some("data:audio/opus;base64,AAAA".to_owned());

        let error = asset.validate().expect_err("must reject embedded media");
        assert!(
            error
                .to_string()
                .contains("binary media must remain external")
        );
    }

    #[test]
    fn generated_descriptor_promotion_and_invalidation_preserve_provenance() {
        let dir = TestDir::new("generated-promotion");
        let generated =
            load_asset_file(fixture("valid/generated-dynamic.json")).expect("generated fixture");
        let expected_provenance = generated.provenance.clone();
        let id = generated.id.clone();
        let mut store = AssetStore::new(dir.path(), generated_runtime());
        store
            .insert_hot(generated)
            .expect("insert generated hot asset");

        let path = store
            .persist_generated_descriptor(&id)
            .expect("persist generated descriptor");
        assert!(path.exists());
        let persisted = load_asset_file(&path).expect("persisted descriptor");
        assert_eq!(persisted.provenance, expected_provenance);
        assert_eq!(
            persisted.compatibility.check(store.runtime()),
            CompatibilityStatus::Usable
        );
        assert!(matches!(
            store.persist_generated_descriptor(&id),
            Err(AssetStoreError::AlreadyPersisted { .. })
        ));

        store
            .invalidate_generated_asset(&id)
            .expect("invalidate generated asset");
        assert!(!path.exists());
        assert!(store.hot_get(&id).is_none());
        assert!(store.local_entry(&id).is_none());
    }

    #[test]
    fn static_asset_cannot_use_generated_promotion_or_invalidation_path() {
        let dir = TestDir::new("static-promotion");
        let asset = valid_asset();
        let id = asset.id.clone();
        let mut store = AssetStore::new(dir.path(), runtime());
        store.insert_hot(asset).expect("insert static hot asset");

        assert!(matches!(
            store.persist_generated_descriptor(&id),
            Err(AssetStoreError::NotGenerated { .. })
        ));
        assert!(matches!(
            store.invalidate_generated_asset(&id),
            Err(AssetStoreError::NotGenerated { .. })
        ));
        assert!(store.hot_get(&id).is_some());
    }

    #[test]
    fn generated_asset_requires_replayable_provenance() {
        let mut asset = valid_asset();
        asset.class = AssetClass::Dynamic;
        asset.provenance = Some(Provenance {
            generated: Some(true),
            generator: Some("aivtuber-generative".to_owned()),
            created_at: Some("2026-09-24T00:00:00Z".to_owned()),
            thinking_backend: Some("openai-compatible-responses".to_owned()),
            thinking_model_alias: Some("reasoner".to_owned()),
            thinking_model_version: Some("2026-09".to_owned()),
            tts_backend: Some("example-tts".to_owned()),
            tts_model_alias: Some("tts-fast".to_owned()),
            tts_model_version: Some("2026-09".to_owned()),
            routing_reason: None,
        });

        let error = asset
            .validate()
            .expect_err("generated provenance without routing reason must fail");
        assert!(
            error.to_string().contains("provenance.routing_reason"),
            "{error}"
        );
    }

    #[test]
    fn strict_deserialization_rejects_unknown_fields() {
        let dir = TestDir::new("unknown-field");
        let path = dir.path().join("asset.json");
        let mut value = serde_json::to_value(valid_asset()).expect("serialize asset as JSON value");
        value
            .as_object_mut()
            .expect("asset object")
            .insert("unexpected".to_owned(), Value::Bool(true));
        fs::write(
            &path,
            serde_json::to_string_pretty(&value).expect("serialize JSON"),
        )
        .expect("write JSON");

        let error = load_asset_file(path).expect_err("unknown field must fail");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn incompatible_assets_are_indexed_but_not_resolved() {
        let dir = TestDir::new("incompatible");
        let path = dir.path().join("asset.json");
        write_asset(&path, &valid_asset());

        let mut incompatible_runtime = runtime();
        incompatible_runtime.voice_model = Some("different-voice".to_owned());
        let mut store = AssetStore::new(dir.path(), incompatible_runtime);

        let report = store.index_local().expect("index");
        assert_eq!(report.indexed, 1);
        assert_eq!(report.usable, 0);
        assert_eq!(report.incompatible, 1);
        assert_eq!(
            store.tier("reaction.surprise.01"),
            Some(CacheTier::LocalStorage)
        );

        let error = store
            .resolve("reaction.surprise.01")
            .expect_err("must reject incompatible asset");
        assert!(matches!(error, AssetStoreError::Incompatible { .. }));
        assert!(error.to_string().contains("voice_model"));
    }

    #[test]
    fn l1_resolution_promotes_to_l0_and_hot_lookup_survives_file_removal() {
        let dir = TestDir::new("l0-l1");
        let path = dir.path().join("asset.json");
        write_asset(&path, &valid_asset());

        let mut store = AssetStore::new(dir.path(), runtime());
        store.index_local().expect("index");
        assert_eq!(store.hot_len(), 0);
        assert_eq!(
            store.tier("reaction.surprise.01"),
            Some(CacheTier::LocalStorage)
        );

        let (loaded, source_tier) = store
            .resolve_with_tier("reaction.surprise.01")
            .expect("resolve L1");
        assert_eq!(loaded.id, "reaction.surprise.01");
        assert_eq!(source_tier, CacheTier::LocalStorage);
        assert_eq!(store.hot_len(), 1);
        assert_eq!(store.tier("reaction.surprise.01"), Some(CacheTier::Memory));
        let (_, hot_tier) = store
            .resolve_with_tier("reaction.surprise.01")
            .expect("resolve L0");
        assert_eq!(hot_tier, CacheTier::Memory);

        fs::remove_file(path).expect("remove L1 descriptor");
        let hot = store.hot_get("reaction.surprise.01").expect("L0 hit");
        assert_eq!(hot.id, "reaction.surprise.01");
    }

    #[test]
    fn preload_promotes_only_compatible_assets() {
        let dir = TestDir::new("preload");
        let compatible = valid_asset();
        write_asset(&dir.path().join("a.json"), &compatible);

        let mut incompatible = compatible.clone();
        incompatible.id = "reaction.surprise.incompatible".to_owned();
        incompatible.compatibility.voice_model = Some("other-voice".to_owned());
        write_asset(&dir.path().join("b.json"), &incompatible);

        let mut store = AssetStore::new(dir.path(), runtime());
        let report = store.index_local().expect("index");
        assert_eq!(report.usable, 1);
        assert_eq!(report.incompatible, 1);

        assert_eq!(store.preload_compatible().expect("preload"), 1);
        assert_eq!(store.hot_len(), 1);
        assert!(store.hot_get("reaction.surprise.01").is_some());
        assert!(store.hot_get("reaction.surprise.incompatible").is_none());
    }

    #[test]
    fn variant_selection_is_deterministic_and_avoids_recent_when_possible() {
        let dir = TestDir::new("variants");
        let mut store = AssetStore::new(dir.path(), runtime());

        for suffix in ["01", "02", "03"] {
            let mut asset = valid_asset();
            asset.id = format!("reaction.surprise.{suffix}");
            store.insert_hot(asset).expect("insert variant");
        }

        let first = store
            .select_variant_id("reaction.surprise", 42, &[])
            .expect("variant");
        let replay = store
            .select_variant_id("reaction.surprise", 42, &[])
            .expect("variant");
        assert_eq!(first, replay);

        let next = store
            .select_variant_id("reaction.surprise", 42, &[first.as_str()])
            .expect("alternative");
        assert_ne!(first, next);

        let all_recent = [
            "reaction.surprise.01",
            "reaction.surprise.02",
            "reaction.surprise.03",
        ];
        assert!(
            store
                .select_variant_id("reaction.surprise", 42, &all_recent)
                .is_some()
        );
    }

    #[test]
    fn compatibility_versions_are_part_of_stable_identity() {
        let asset = valid_asset();
        let first = asset.identity();

        let mut changed = asset;
        changed.compatibility.voice_model = Some("example-voice-v2".to_owned());
        let second = changed.identity();

        assert_ne!(first, second);
        assert_ne!(first.stable_key(), second.stable_key());
    }

    #[test]
    fn duplicate_ids_fail_without_replacing_previous_index() {
        let dir = TestDir::new("duplicates");
        let asset = valid_asset();
        write_asset(&dir.path().join("a.json"), &asset);
        write_asset(&dir.path().join("b.json"), &asset);

        let mut store = AssetStore::new(dir.path(), runtime());
        let error = store.index_local().expect_err("must reject duplicate ids");

        assert!(matches!(error, AssetStoreError::DuplicateId { .. }));
        assert_eq!(store.local_len(), 0);
    }

    #[test]
    fn reindex_failure_is_transactional() {
        let dir = TestDir::new("transactional");
        write_asset(&dir.path().join("a.json"), &valid_asset());

        let mut store = AssetStore::new(dir.path(), runtime());
        store.index_local().expect("initial index");
        assert_eq!(store.local_len(), 1);

        fs::write(dir.path().join("bad.json"), r#"{"schema_version":"0.1.0"}"#)
            .expect("write invalid descriptor");

        assert!(store.index_local().is_err());
        assert_eq!(store.local_len(), 1);
        assert!(store.local_entry("reaction.surprise.01").is_some());
    }

    #[test]
    fn descriptor_subdirectories_are_not_parsed_as_assets() {
        let dir = TestDir::new("non-recursive");
        write_asset(&dir.path().join("asset.json"), &valid_asset());
        let media = dir.path().join("viseme");
        fs::create_dir_all(&media).expect("create media directory");
        fs::write(media.join("track.json"), r#"{"frames":[]}"#).expect("write viseme json");

        let mut store = AssetStore::new(dir.path(), runtime());
        let report = store.index_local().expect("index descriptor root");

        assert_eq!(report.indexed, 1);
        assert_eq!(store.local_len(), 1);
    }

    fn generated_asset_with_id(id: &str) -> PerformanceAsset {
        let mut asset =
            load_asset_file(fixture("valid/generated-dynamic.json")).expect("generated fixture");
        asset.id = id.to_owned();
        asset
    }

    #[test]
    fn generated_churn_bounded_dynamic_capacity_keeps_pinned_static_assets() {
        let dir = TestDir::new("bounded-churn");
        // Copy the static pack descriptors beside the generated fixture's
        // compatibility requirements so one runtime accepts both, letting the
        // pinned-vs-dynamic split be exercised in a single store.
        let combined_runtime = RuntimeCompatibility {
            voice_model: Some("voice-ja-v2".to_owned()),
            viseme_mapping: Some("ja-5vowel-v2".to_owned()),
            ..runtime()
        };
        // Static pack uses the v1 voice/viseme; patch descriptors into the
        // temp dir with the v2 runtime identity for this test only.
        let pack_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/starter-reaction-pack/descriptors");
        for entry in fs::read_dir(&pack_dir).expect("read pack") {
            let entry = entry.expect("pack entry");
            let mut asset = load_asset_file(entry.path()).expect("pack asset");
            asset.compatibility.voice_model = Some("voice-ja-v2".to_owned());
            asset.compatibility.viseme_mapping = Some("ja-5vowel-v2".to_owned());
            write_asset(&dir.path().join(entry.file_name()), &asset);
        }
        let mut store = AssetStore::new(dir.path(), combined_runtime);
        store.index_local().expect("index");
        store
            .preload_compatible()
            .expect("preload static pinned assets");
        let pinned_before = store.hot_len();
        assert!(pinned_before > 0, "precondition: static assets resident");

        store.set_hot_cache_config(HotCacheConfig {
            max_dynamic_assets: 3,
            ..HotCacheConfig::default()
        });

        // Generate far more unique dynamic assets than capacity.
        for index in 0..10 {
            let asset = generated_asset_with_id(&format!("dynamic.churn.{index:02}"));
            store.insert_hot(asset).expect("insert generated asset");
        }

        // Bounded plateau: dynamic residency must not exceed capacity.
        let metrics = store.hot_cache_metrics();
        assert_eq!(
            metrics.dynamic_resident, 3,
            "dynamic residency must plateau at capacity"
        );
        assert_eq!(
            metrics.pinned_resident, pinned_before,
            "static assets must not be evicted by dynamic churn"
        );
        assert_eq!(metrics.evictions, 7, "oldest generated assets evicted");
        // The most recently inserted survivors remain resident.
        assert!(store.hot_get("dynamic.churn.07").is_some());
        assert!(store.hot_get("dynamic.churn.08").is_some());
        assert!(store.hot_get("dynamic.churn.09").is_some());
        // The oldest dynamic assets were evicted first.
        assert!(store.hot_get("dynamic.churn.00").is_none());
        assert!(store.hot_get("dynamic.churn.06").is_none());
    }

    #[test]
    fn hot_get_touches_recency_so_recently_used_assets_survive() {
        let dir = TestDir::new("recency");
        let mut store = AssetStore::new(dir.path(), generated_runtime());
        store.set_hot_cache_config(HotCacheConfig {
            max_dynamic_assets: 3,
            ..HotCacheConfig::default()
        });

        for index in 0..3 {
            let asset = generated_asset_with_id(&format!("dynamic.recency.{index}"));
            store.insert_hot(asset).expect("insert");
        }
        // Touch dynamic.recency.0 so its recency tick becomes the newest.
        let _ = store.hot_get("dynamic.recency.0");

        let asset = generated_asset_with_id("dynamic.recency.3");
        store.insert_hot(asset).expect("insert overflow");

        // Without the touch, .0 would have been the LRU victim; the touch
        // demotes .1 to LRU instead.
        assert!(
            store.hot_get("dynamic.recency.0").is_some(),
            "recently used survivor"
        );
        assert!(store.hot_get("dynamic.recency.2").is_some());
        assert!(store.hot_get("dynamic.recency.3").is_some());
        assert!(store.hot_get("dynamic.recency.1").is_none(), "LRU evicted");
    }

    #[test]
    fn eviction_is_deterministic_with_stable_asset_id_tie_break() {
        // Two identical recorded input sequences run in separate stores.
        let survivors = |store_tag: &str| {
            let dir = TestDir::new(store_tag);
            let mut store = AssetStore::new(dir.path(), generated_runtime());
            store.set_hot_cache_config(HotCacheConfig {
                max_dynamic_assets: 2,
                ..HotCacheConfig::default()
            });
            for suffix in ["b", "a", "c", "d"] {
                let asset = generated_asset_with_id(&format!("dynamic.tie.{suffix}"));
                store.insert_hot(asset).expect("insert");
            }
            let mut ids: Vec<String> = Vec::new();
            for suffix in ["a", "b", "c", "d"] {
                let id = format!("dynamic.tie.{suffix}");
                if store.hot_get(&id).is_some() {
                    ids.push(id);
                }
            }
            ids.sort();
            ids
        };

        assert_eq!(
            survivors("tie-a"),
            survivors("tie-b"),
            "identical recorded inputs must produce identical survivors"
        );
    }

    #[test]
    fn arc_handle_remains_valid_after_store_eviction() {
        let dir = TestDir::new("arc-safety");
        let mut store = AssetStore::new(dir.path(), generated_runtime());
        store.set_hot_cache_config(HotCacheConfig {
            max_dynamic_assets: 1,
            ..HotCacheConfig::default()
        });

        let first = store
            .insert_hot(generated_asset_with_id("dynamic.arc.first"))
            .expect("insert first");
        let arc_weak = Arc::downgrade(&first);

        let _second = store
            .insert_hot(generated_asset_with_id("dynamic.arc.second"))
            .expect("insert second evicts first");

        assert!(store.hot_get("dynamic.arc.first").is_none());
        // The in-flight consumer keeps the asset alive safely.
        let still_alive = arc_weak
            .upgrade()
            .expect("Arc must stay alive for in-flight playback");
        assert_eq!(still_alive.id, "dynamic.arc.first");
    }

    #[test]
    fn estimated_byte_budget_enforced_on_dynamic_assets() {
        let dir = TestDir::new("byte-budget");
        let mut store = AssetStore::new(dir.path(), generated_runtime());
        store.set_hot_cache_config(HotCacheConfig {
            max_dynamic_assets: usize::MAX,
            max_dynamic_bytes: 1,
        });

        store
            .insert_hot(generated_asset_with_id("dynamic.bytes.first"))
            .expect("first insert always fits");
        store
            .insert_hot(generated_asset_with_id("dynamic.bytes.second"))
            .expect("second insert evicts first to respect budget");

        assert!(store.hot_get("dynamic.bytes.first").is_none());
        assert!(store.hot_get("dynamic.bytes.second").is_some());
    }

    #[test]
    fn cache_metrics_expose_residency_hits_and_misses() {
        let dir = TestDir::new("metrics");
        let mut store = AssetStore::new(dir.path(), generated_runtime());
        store.set_hot_cache_config(HotCacheConfig {
            max_dynamic_assets: 2,
            ..HotCacheConfig::default()
        });

        let before = store.hot_cache_metrics();
        store
            .insert_hot(generated_asset_with_id("dynamic.metrics.a"))
            .expect("insert a");
        store
            .insert_hot(generated_asset_with_id("dynamic.metrics.b"))
            .expect("insert b");
        let _ = store.hot_get("dynamic.metrics.a"); // hit
        let _ = store.hot_get("missing.asset"); // miss

        let metrics = store.hot_cache_metrics();
        assert_eq!(metrics.dynamic_resident, 2);
        assert_eq!(metrics.pinned_resident, 0);
        assert_eq!(metrics.hits, before.hits + 1);
        assert_eq!(metrics.misses, before.misses + 1);
        assert_eq!(metrics.evictions, before.evictions);
        assert!(metrics.estimated_bytes > 0);
    }

    #[test]
    fn promotion_metadata_survives_full_asset_eviction() {
        let dir = TestDir::new("promo-metadata");
        let mut store = AssetStore::new(dir.path(), generated_runtime());
        store.set_hot_cache_config(HotCacheConfig {
            max_dynamic_assets: 1,
            ..HotCacheConfig::default()
        });

        let first = store
            .insert_hot(generated_asset_with_id("dynamic.promo.first"))
            .expect("insert first");
        store.note_hot_use("dynamic.promo.first", 100);
        let first_snapshot = store
            .promotion_metadata("dynamic.promo.first")
            .expect("metadata");
        assert_eq!(first_snapshot.use_count, 1);
        assert_eq!(first_snapshot.last_used_at_ms, Some(100));
        drop(first);

        // Evict via churn.
        store
            .insert_hot(generated_asset_with_id("dynamic.promo.second"))
            .expect("insert second");
        assert!(store.hot_get("dynamic.promo.first").is_none());

        // Lightweight metadata must outlive full-asset eviction (#39).
        let survivor = store
            .promotion_metadata("dynamic.promo.first")
            .expect("metadata survives");
        assert_eq!(survivor.use_count, 1);
        assert_eq!(survivor.last_used_at_ms, Some(100));
        assert_eq!(survivor.id, "dynamic.promo.first");
    }
}
