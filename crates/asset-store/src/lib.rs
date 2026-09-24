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
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const PERFORMANCE_ASSET_SCHEMA_VERSION: &str = "0.1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    #[serde(default)]
    pub generated: Option<bool>,
    #[serde(default)]
    pub generator: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetIndexEntry {
    pub identity: AssetIdentity,
    pub path: PathBuf,
    pub variant_group: Option<String>,
    pub compatibility: CompatibilityStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub indexed: usize,
    pub usable: usize,
    pub incompatible: usize,
}

#[derive(Debug)]
pub struct AssetStore {
    root: PathBuf,
    runtime: RuntimeCompatibility,
    hot: BTreeMap<String, Arc<PerformanceAsset>>,
    local: BTreeMap<String, AssetIndexEntry>,
}

impl AssetStore {
    pub fn new(root: impl Into<PathBuf>, runtime: RuntimeCompatibility) -> Self {
        Self {
            root: root.into(),
            runtime,
            hot: BTreeMap::new(),
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
                variant_group: asset.variant_group.clone(),
                compatibility: compatibility.clone(),
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
        self.hot.insert(id, Arc::clone(&asset));
        Ok(asset)
    }

    /// Pure L0 lookup. This performs no filesystem or network access.
    pub fn hot_get(&self, id: &str) -> Option<Arc<PerformanceAsset>> {
        self.hot.get(id).cloned()
    }

    /// Resolve L0 first, then the indexed local L1 descriptor.
    ///
    /// A compatible L1 hit is promoted into L0. The descriptor is revalidated
    /// and its stable identity is compared with the indexed identity so a file
    /// changed after indexing cannot silently bypass compatibility checks.
    pub fn resolve(&mut self, id: &str) -> Result<Arc<PerformanceAsset>, AssetStoreError> {
        if let Some(asset) = self.hot_get(id) {
            return Ok(asset);
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
        let id = self
            .select_variant_id(variant_group, seed, recently_used)
            .ok_or_else(|| AssetStoreError::NotFound {
                id: format!("variant_group:{variant_group}"),
            })?;
        self.resolve(&id)
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

        let loaded = store.resolve("reaction.surprise.01").expect("resolve L1");
        assert_eq!(loaded.id, "reaction.surprise.01");
        assert_eq!(store.hot_len(), 1);
        assert_eq!(store.tier("reaction.surprise.01"), Some(CacheTier::Memory));

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
}
