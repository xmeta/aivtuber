use aivtuber_asset_store::{
    AssetClass, AssetStore, AssetStoreError, CacheTier, IndexReport, PerformanceAsset,
};
use aivtuber_domain::{EventEnvelope, EventKind};
use aivtuber_scheduler::{
    AppliedVariation, BlendChannel, PlannedPerformance, Rejection, Scheduler, SeededRng,
    VariationSpec as SchedulerVariationSpec, apply_variation, priority_for_event,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const VISEME_TRACK_SCHEMA_VERSION: &str = "0.1.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisemeCue {
    pub at_ms: u64,
    pub viseme: String,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisemeTrack {
    pub schema_version: String,
    pub duration_ms: u64,
    pub cues: Vec<VisemeCue>,
}

impl VisemeTrack {
    pub fn validate(&self) -> Result<(), CachedPlaybackError> {
        if self.schema_version != VISEME_TRACK_SCHEMA_VERSION {
            return Err(CachedPlaybackError::InvalidVisemeTrack(format!(
                "expected schema_version {VISEME_TRACK_SCHEMA_VERSION}, got {}",
                self.schema_version
            )));
        }

        let mut previous = None;
        for (index, cue) in self.cues.iter().enumerate() {
            if cue.viseme.trim().is_empty() {
                return Err(CachedPlaybackError::InvalidVisemeTrack(format!(
                    "cues[{index}].viseme must not be empty"
                )));
            }
            if !cue.weight.is_finite() || !(0.0..=1.0).contains(&cue.weight) {
                return Err(CachedPlaybackError::InvalidVisemeTrack(format!(
                    "cues[{index}].weight must be in 0..=1"
                )));
            }
            if previous.is_some_and(|at_ms| cue.at_ms < at_ms) {
                return Err(CachedPlaybackError::InvalidVisemeTrack(
                    "viseme cues must be monotonic non-decreasing".to_owned(),
                ));
            }
            if cue.at_ms > self.duration_ms {
                return Err(CachedPlaybackError::InvalidVisemeTrack(format!(
                    "cues[{index}].at_ms exceeds duration_ms"
                )));
            }
            previous = Some(cue.at_ms);
        }

        Ok(())
    }
}

pub trait VisemeResolver {
    fn resolve(&self, reference: &str) -> Result<VisemeTrack, CachedPlaybackError>;
}

#[derive(Debug, Clone)]
pub struct LocalVisemeStore {
    root: PathBuf,
}

impl LocalVisemeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl VisemeResolver for LocalVisemeStore {
    fn resolve(&self, reference: &str) -> Result<VisemeTrack, CachedPlaybackError> {
        let reference = reference.trim();
        if reference.is_empty() || reference.contains("://") {
            return Err(CachedPlaybackError::InvalidVisemeReference(
                reference.to_owned(),
            ));
        }

        let relative = Path::new(reference);
        let looks_like_windows_absolute = reference
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':');
        if relative.is_absolute()
            || looks_like_windows_absolute
            || relative
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(CachedPlaybackError::InvalidVisemeReference(
                reference.to_owned(),
            ));
        }

        let path = self.root.join(relative);
        let text = fs::read_to_string(&path).map_err(|source| CachedPlaybackError::Io {
            path: path.clone(),
            source,
        })?;
        let track: VisemeTrack =
            serde_json::from_str(&text).map_err(|source| CachedPlaybackError::Json {
                path: path.clone(),
                message: source.to_string(),
            })?;
        track.validate()?;
        Ok(track)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioPlaybackCommand {
    pub generation: u64,
    pub event_id: String,
    pub asset_id: String,
    pub at_ms: u64,
    pub audio_ref: String,
    pub duration_ms: u64,
    pub speed_factor: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AvatarPlaybackCommand {
    Expression {
        generation: u64,
        at_ms: u64,
        preset: String,
        intensity: f64,
    },
    Gesture {
        generation: u64,
        at_ms: u64,
        preset: String,
        amplitude: f64,
    },
    Gaze {
        generation: u64,
        at_ms: u64,
        target: String,
    },
    Viseme {
        generation: u64,
        at_ms: u64,
        viseme: String,
        weight: f64,
    },
}

impl AvatarPlaybackCommand {
    pub fn at_ms(&self) -> u64 {
        match self {
            Self::Expression { at_ms, .. }
            | Self::Gesture { at_ms, .. }
            | Self::Gaze { at_ms, .. }
            | Self::Viseme { at_ms, .. } => *at_ms,
        }
    }
}
pub trait AudioPlaybackSink {
    fn schedule_audio(&mut self, command: AudioPlaybackCommand);
}

pub trait AvatarPlaybackSink {
    fn schedule_avatar(&mut self, command: AvatarPlaybackCommand);
}

#[derive(Debug, Default)]
pub struct RecordingAudioSink {
    pub commands: Vec<AudioPlaybackCommand>,
}

impl AudioPlaybackSink for RecordingAudioSink {
    fn schedule_audio(&mut self, command: AudioPlaybackCommand) {
        self.commands.push(command);
    }
}
#[derive(Debug, Default)]
pub struct RecordingAvatarSink {
    pub commands: Vec<AvatarPlaybackCommand>,
}

impl AvatarPlaybackSink for RecordingAvatarSink {
    fn schedule_avatar(&mut self, command: AvatarPlaybackCommand) {
        self.commands.push(command);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastPathPreloadReport {
    pub indexed: IndexReport,
    pub preloaded: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedPlaybackConfig {
    pub recent_variant_window: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedPlaybackTiming {
    pub at_ms: u64,
    pub seed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedAssetSelection<'a> {
    pub asset_id: &'a str,
    pub expected_identity: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedPlaybackAsset<'a> {
    asset: &'a PerformanceAsset,
    cache_tier: CacheTier,
}

impl Default for CachedPlaybackConfig {
    fn default() -> Self {
        Self {
            recent_variant_window: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FastPathMetrics {
    pub event_to_first_audio_ms: Option<u64>,
    pub event_to_first_visible_reaction_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CachedPlaybackOutcome {
    pub asset_id: String,
    pub cache_tier: CacheTier,
    pub plan: PlannedPerformance,
    pub variation: AppliedVariation,
    pub metrics: FastPathMetrics,
}

#[derive(Debug)]
pub enum CachedPlaybackError {
    AssetStore(AssetStoreError),
    InvalidEvent(String),
    MissingIntent,
    NoPlayableTracks(String),
    StaleAssetIdentity {
        asset_id: String,
        expected: String,
        actual: String,
    },
    Scheduler(Rejection),
    InvalidVisemeReference(String),
    InvalidVisemeTrack(String),
    VisemeDurationMismatch {
        asset_id: String,
        speech_duration_ms: u64,
        viseme_duration_ms: u64,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Json {
        path: PathBuf,
        message: String,
    },
}
impl fmt::Display for CachedPlaybackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AssetStore(source) => write!(f, "{source}"),
            Self::InvalidEvent(message) => write!(f, "invalid event: {message}"),
            Self::MissingIntent => f.write_str("cached playback requires payload.intent"),
            Self::NoPlayableTracks(asset_id) => {
                write!(f, "asset {asset_id:?} has no playable tracks")
            }
            Self::StaleAssetIdentity {
                asset_id,
                expected,
                actual,
            } => write!(
                f,
                "asset {asset_id:?} identity changed after retrieval: expected {expected:?}, got {actual:?}"
            ),
            Self::Scheduler(reason) => write!(f, "scheduler rejected cached playback: {reason:?}"),
            Self::InvalidVisemeReference(reference) => {
                write!(f, "invalid local viseme reference: {reference:?}")
            }
            Self::InvalidVisemeTrack(message) => {
                write!(f, "invalid viseme track: {message}")
            }
            Self::VisemeDurationMismatch {
                asset_id,
                speech_duration_ms,
                viseme_duration_ms,
            } => write!(
                f,
                "asset {asset_id:?} speech/viseme duration mismatch: {speech_duration_ms}ms vs {viseme_duration_ms}ms"
            ),
            Self::Io { path, source } => {
                write!(f, "I/O error at {}: {source}", path.display())
            }
            Self::Json { path, message } => {
                write!(f, "invalid JSON at {}: {message}", path.display())
            }
        }
    }
}
impl Error for CachedPlaybackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AssetStore(source) => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<AssetStoreError> for CachedPlaybackError {
    fn from(value: AssetStoreError) -> Self {
        Self::AssetStore(value)
    }
}

#[derive(Debug)]
pub struct CachedPerformer {
    assets: AssetStore,
    scheduler: Scheduler,
    config: CachedPlaybackConfig,
    recent_by_group: BTreeMap<String, VecDeque<String>>,
}
impl CachedPerformer {
    pub fn new(assets: AssetStore, scheduler: Scheduler, config: CachedPlaybackConfig) -> Self {
        Self {
            assets,
            scheduler,
            config,
            recent_by_group: BTreeMap::new(),
        }
    }

    pub fn assets(&self) -> &AssetStore {
        &self.assets
    }

    pub fn assets_mut(&mut self) -> &mut AssetStore {
        &mut self.assets
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        &mut self.scheduler
    }
    pub fn index_and_preload_fast_path(
        &mut self,
    ) -> Result<FastPathPreloadReport, CachedPlaybackError> {
        let indexed = self.assets.index_local()?;
        let preloaded = self
            .assets
            .preload_classes(&[AssetClass::Filler, AssetClass::Reaction])?;
        Ok(FastPathPreloadReport { indexed, preloaded })
    }

    pub fn handle_event<R, A, V>(
        &mut self,
        event: &EventEnvelope,
        at_ms: u64,
        seed: u64,
        visemes: &R,
        audio: &mut A,
        avatar: &mut V,
    ) -> Result<CachedPlaybackOutcome, CachedPlaybackError>
    where
        R: VisemeResolver,
        A: AudioPlaybackSink,
        V: AvatarPlaybackSink,
    {
        event
            .validate()
            .map_err(|error| CachedPlaybackError::InvalidEvent(error.to_string()))?;
        if event.kind == EventKind::OperatorCommand {
            return Err(CachedPlaybackError::InvalidEvent(
                "operator commands use the authenticated control path".to_owned(),
            ));
        }

        let intent = event
            .payload
            .get("intent")
            .and_then(serde_json::Value::as_str)
            .filter(|intent| !intent.trim().is_empty())
            .ok_or(CachedPlaybackError::MissingIntent)?;

        let recent_owned: Vec<String> = self
            .recent_by_group
            .get(intent)
            .map(|recent| recent.iter().cloned().collect())
            .unwrap_or_default();
        let recent_refs: Vec<&str> = recent_owned.iter().map(String::as_str).collect();

        let (asset, cache_tier) = self.assets.select_variant_with_tier(
            intent,
            seed.wrapping_add(event.sequence),
            &recent_refs,
        )?;
        let outcome = self.schedule_asset(
            event,
            CachedPlaybackTiming { at_ms, seed },
            ResolvedPlaybackAsset {
                asset: &asset,
                cache_tier,
            },
            visemes,
            audio,
            avatar,
        )?;
        self.record_recent(intent, &asset.id);
        Ok(outcome)
    }
    /// Play an explicitly selected, compatibility-checked asset through the
    /// same scheduler path used by deterministic intent routing. Semantic index
    /// construction remains an upstream responsibility.
    pub fn handle_asset_id<R, A, V>(
        &mut self,
        event: &EventEnvelope,
        asset_id: &str,
        timing: CachedPlaybackTiming,
        visemes: &R,
        audio: &mut A,
        avatar: &mut V,
    ) -> Result<CachedPlaybackOutcome, CachedPlaybackError>
    where
        R: VisemeResolver,
        A: AudioPlaybackSink,
        V: AvatarPlaybackSink,
    {
        event
            .validate()
            .map_err(|error| CachedPlaybackError::InvalidEvent(error.to_string()))?;
        if event.kind == EventKind::OperatorCommand {
            return Err(CachedPlaybackError::InvalidEvent(
                "operator commands use the authenticated control path".to_owned(),
            ));
        }
        if asset_id.trim().is_empty() {
            return Err(CachedPlaybackError::InvalidEvent(
                "selected asset id must not be empty".to_owned(),
            ));
        }

        let (asset, cache_tier) = self.assets.resolve_with_tier(asset_id)?;
        let recent_group = asset
            .variant_group
            .as_deref()
            .unwrap_or(asset.intent.as_str())
            .to_owned();
        let outcome = self.schedule_asset(
            event,
            timing,
            ResolvedPlaybackAsset {
                asset: &asset,
                cache_tier,
            },
            visemes,
            audio,
            avatar,
        )?;
        self.record_recent(&recent_group, &asset.id);
        Ok(outcome)
    }

    /// Play a semantic selection only if the current compatibility-checked
    /// asset still has the exact identity recorded by retrieval.
    pub fn handle_asset_identity<R, A, V>(
        &mut self,
        event: &EventEnvelope,
        selection: CachedAssetSelection<'_>,
        timing: CachedPlaybackTiming,
        visemes: &R,
        audio: &mut A,
        avatar: &mut V,
    ) -> Result<CachedPlaybackOutcome, CachedPlaybackError>
    where
        R: VisemeResolver,
        A: AudioPlaybackSink,
        V: AvatarPlaybackSink,
    {
        event
            .validate()
            .map_err(|error| CachedPlaybackError::InvalidEvent(error.to_string()))?;
        if event.kind == EventKind::OperatorCommand {
            return Err(CachedPlaybackError::InvalidEvent(
                "operator commands use the authenticated control path".to_owned(),
            ));
        }
        if selection.asset_id.trim().is_empty() || selection.expected_identity.trim().is_empty() {
            return Err(CachedPlaybackError::InvalidEvent(
                "selected asset id and identity must not be empty".to_owned(),
            ));
        }

        let (asset, cache_tier) = self.assets.resolve_with_tier(selection.asset_id)?;
        let actual_identity = asset.identity().stable_key();
        if actual_identity != selection.expected_identity {
            return Err(CachedPlaybackError::StaleAssetIdentity {
                asset_id: selection.asset_id.to_owned(),
                expected: selection.expected_identity.to_owned(),
                actual: actual_identity,
            });
        }

        let recent_group = asset
            .variant_group
            .as_deref()
            .unwrap_or(asset.intent.as_str())
            .to_owned();
        let outcome = self.schedule_asset(
            event,
            timing,
            ResolvedPlaybackAsset {
                asset: &asset,
                cache_tier,
            },
            visemes,
            audio,
            avatar,
        )?;
        self.record_recent(&recent_group, &asset.id);
        Ok(outcome)
    }

    fn schedule_asset<R, A, V>(
        &mut self,
        event: &EventEnvelope,
        timing: CachedPlaybackTiming,
        resolved: ResolvedPlaybackAsset<'_>,
        visemes: &R,
        audio: &mut A,
        avatar: &mut V,
    ) -> Result<CachedPlaybackOutcome, CachedPlaybackError>
    where
        R: VisemeResolver,
        A: AudioPlaybackSink,
        V: AvatarPlaybackSink,
    {
        let ResolvedPlaybackAsset { asset, cache_tier } = resolved;
        let viseme_track = resolve_asset_visemes(asset, visemes)?;
        let variation_spec = asset.variation.unwrap_or_default();
        let variation = apply_variation(
            SchedulerVariationSpec {
                speed_pct: variation_spec.speed_pct.unwrap_or(0.0),
                amplitude_pct: variation_spec.amplitude_pct.unwrap_or(0.0),
                start_delay_ms: variation_spec.start_delay_ms.unwrap_or(0),
            },
            &mut SeededRng::new(timing.seed.wrapping_add(event.sequence)),
        );

        let channels = playback_channels(asset, viseme_track.as_ref());
        if channels.is_empty() {
            return Err(CachedPlaybackError::NoPlayableTracks(asset.id.clone()));
        }

        let base_duration_ms = asset_base_duration_ms(asset, viseme_track.as_ref()).max(1);
        let duration_ms = scale_offset(base_duration_ms, variation.speed_factor).max(1);
        let interrupt_points_ms = asset
            .interrupt_points_ms
            .iter()
            .copied()
            .map(|point| scale_offset(point, variation.speed_factor))
            .collect::<Vec<_>>();
        let plan = PlannedPerformance {
            event_id: event.event_id.clone(),
            asset_id: asset.id.clone(),
            priority: priority_for_event(event.kind),
            interruptible: !interrupt_points_ms.is_empty(),
            interrupt_points_ms,
            start_at_ms: timing.at_ms.saturating_add(variation.start_delay_ms),
            duration_ms,
            generation: 0,
            exclusive: false,
            channels,
        };

        let plan = self
            .scheduler
            .schedule_at(timing.at_ms, plan)
            .map_err(CachedPlaybackError::Scheduler)?;
        let metrics = dispatch_asset(
            asset,
            viseme_track.as_ref(),
            &plan,
            variation,
            timing.at_ms,
            audio,
            avatar,
        );
        Ok(CachedPlaybackOutcome {
            asset_id: asset.id.clone(),
            cache_tier,
            plan,
            variation,
            metrics,
        })
    }

    fn record_recent(&mut self, group: &str, asset_id: &str) {
        if self.config.recent_variant_window == 0 {
            return;
        }
        let recent = self.recent_by_group.entry(group.to_owned()).or_default();
        recent.push_back(asset_id.to_owned());
        while recent.len() > self.config.recent_variant_window {
            recent.pop_front();
        }
    }
}
fn resolve_asset_visemes<R: VisemeResolver>(
    asset: &PerformanceAsset,
    resolver: &R,
) -> Result<Option<VisemeTrack>, CachedPlaybackError> {
    let Some(speech) = &asset.speech else {
        return Ok(None);
    };
    let Some(reference) = speech.viseme_ref.as_deref() else {
        return Ok(None);
    };

    let track = resolver.resolve(reference)?;
    if let Some(speech_duration_ms) = speech.duration_ms
        && track.duration_ms != speech_duration_ms
    {
        return Err(CachedPlaybackError::VisemeDurationMismatch {
            asset_id: asset.id.clone(),
            speech_duration_ms,
            viseme_duration_ms: track.duration_ms,
        });
    }
    Ok(Some(track))
}
fn playback_channels(
    asset: &PerformanceAsset,
    visemes: Option<&VisemeTrack>,
) -> BTreeSet<BlendChannel> {
    let mut channels = BTreeSet::new();

    if asset
        .speech
        .as_ref()
        .and_then(|speech| speech.audio_ref.as_ref())
        .is_some()
    {
        channels.insert(BlendChannel::Audio);
    }
    if asset.expression.is_some() || visemes.is_some() {
        channels.insert(BlendChannel::Face);
    }
    if asset.gesture.is_some() {
        channels.insert(BlendChannel::Body);
    }
    if asset.gaze.is_some() {
        channels.insert(BlendChannel::Gaze);
    }

    channels
}
fn asset_base_duration_ms(asset: &PerformanceAsset, visemes: Option<&VisemeTrack>) -> u64 {
    let timeline_end = asset
        .timeline
        .iter()
        .map(|event| event.at_ms)
        .max()
        .unwrap_or(0);
    let interrupt_end = asset.interrupt_points_ms.iter().copied().max().unwrap_or(0);

    let speech_end = asset
        .speech
        .as_ref()
        .and_then(|speech| speech.duration_ms)
        .map(|duration| marker_offset(asset, "speech.start").saturating_add(duration))
        .unwrap_or(0);

    let viseme_end = visemes
        .map(|track| marker_offset(asset, "speech.start").saturating_add(track.duration_ms))
        .unwrap_or(0);

    timeline_end
        .max(interrupt_end)
        .max(speech_end)
        .max(viseme_end)
}
fn marker_offset(asset: &PerformanceAsset, marker: &str) -> u64 {
    asset
        .timeline
        .iter()
        .find(|event| event.event == marker)
        .map(|event| event.at_ms)
        .unwrap_or(0)
}

fn scale_offset(offset_ms: u64, speed_factor: f64) -> u64 {
    if !speed_factor.is_finite() || speed_factor <= 0.0 {
        return offset_ms;
    }
    ((offset_ms as f64) * speed_factor).round() as u64
}

fn dispatch_asset<A, V>(
    asset: &PerformanceAsset,
    visemes: Option<&VisemeTrack>,
    plan: &PlannedPerformance,
    variation: AppliedVariation,
    event_at_ms: u64,
    audio: &mut A,
    avatar: &mut V,
) -> FastPathMetrics
where
    A: AudioPlaybackSink,
    V: AvatarPlaybackSink,
{
    let mut first_audio_at = None;
    let mut avatar_commands = Vec::new();

    if let Some(speech) = &asset.speech
        && let (Some(audio_ref), Some(duration_ms)) =
            (speech.audio_ref.as_deref(), speech.duration_ms)
    {
        let speech_offset =
            scale_offset(marker_offset(asset, "speech.start"), variation.speed_factor);
        let at_ms = plan.start_at_ms.saturating_add(speech_offset);
        let duration_ms = scale_offset(duration_ms, variation.speed_factor);
        audio.schedule_audio(AudioPlaybackCommand {
            generation: plan.generation,
            event_id: plan.event_id.clone(),
            asset_id: plan.asset_id.clone(),
            at_ms,
            audio_ref: audio_ref.to_owned(),
            duration_ms,
            speed_factor: variation.speed_factor,
        });
        first_audio_at = Some(at_ms);

        if let Some(track) = visemes {
            for cue in &track.cues {
                avatar_commands.push(AvatarPlaybackCommand::Viseme {
                    generation: plan.generation,
                    at_ms: at_ms.saturating_add(scale_offset(cue.at_ms, variation.speed_factor)),
                    viseme: cue.viseme.clone(),
                    weight: cue.weight,
                });
            }
        }
    }
    if let Some(expression) = &asset.expression {
        avatar_commands.push(AvatarPlaybackCommand::Expression {
            generation: plan.generation,
            at_ms: plan.start_at_ms.saturating_add(scale_offset(
                marker_offset(asset, "expression.start"),
                variation.speed_factor,
            )),
            preset: expression.preset.clone(),
            intensity: expression.intensity,
        });
    }

    if let Some(gesture) = &asset.gesture {
        avatar_commands.push(AvatarPlaybackCommand::Gesture {
            generation: plan.generation,
            at_ms: plan.start_at_ms.saturating_add(scale_offset(
                marker_offset(asset, "gesture.start"),
                variation.speed_factor,
            )),
            preset: gesture.preset.clone(),
            amplitude: (gesture.amplitude.unwrap_or(1.0) * variation.amplitude_factor)
                .clamp(0.0, 2.0),
        });
    }
    if let Some(gaze) = asset.gaze {
        avatar_commands.push(AvatarPlaybackCommand::Gaze {
            generation: plan.generation,
            at_ms: plan.start_at_ms.saturating_add(scale_offset(
                marker_offset(asset, "gaze.start"),
                variation.speed_factor,
            )),
            target: gaze_name(gaze).to_owned(),
        });
    }

    avatar_commands.sort_by_key(AvatarPlaybackCommand::at_ms);
    let first_visible_at = avatar_commands.first().map(AvatarPlaybackCommand::at_ms);
    for command in avatar_commands {
        avatar.schedule_avatar(command);
    }

    FastPathMetrics {
        event_to_first_audio_ms: first_audio_at.map(|at| at.saturating_sub(event_at_ms)),
        event_to_first_visible_reaction_ms: first_visible_at
            .map(|at| at.saturating_sub(event_at_ms)),
    }
}
fn gaze_name(gaze: aivtuber_asset_store::GazeTarget) -> &'static str {
    match gaze {
        aivtuber_asset_store::GazeTarget::Camera => "camera",
        aivtuber_asset_store::GazeTarget::Chat => "chat",
        aivtuber_asset_store::GazeTarget::Game => "game",
        aivtuber_asset_store::GazeTarget::Speaker => "speaker",
        aivtuber_asset_store::GazeTarget::Away => "away",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_asset_store::{CacheTier, RuntimeCompatibility};
    use aivtuber_domain::{EVENT_SCHEMA_VERSION, SecurityPlane, SourceClass, TrustLevel};
    use aivtuber_scheduler::SchedulerConfig;

    fn pack_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/starter-reaction-pack")
    }

    fn runtime_compatibility() -> RuntimeCompatibility {
        RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some("example-voice-v1".to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        }
    }

    fn performer() -> CachedPerformer {
        CachedPerformer::new(
            AssetStore::new(pack_root().join("descriptors"), runtime_compatibility()),
            Scheduler::new(SchedulerConfig {
                min_reaction_spacing_ms: 0,
                ..aivtuber_scheduler::SchedulerConfig::default()
            }),
            CachedPlaybackConfig {
                recent_variant_window: 1,
            },
        )
    }

    fn chat_event(sequence: u64, intent: &str) -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: format!("evt-{sequence}"),
            correlation_id: "corr-fast-path".to_owned(),
            sequence,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: "chat".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some("viewer:test".to_owned()),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([(
                "intent".to_owned(),
                serde_json::Value::String(intent.to_owned()),
            )]),
        }
    }

    #[test]
    fn starter_pack_preloads_fillers_and_reactions() {
        let mut performer = performer();
        let report = performer
            .index_and_preload_fast_path()
            .expect("index and preload starter pack");

        assert_eq!(report.indexed.indexed, 6);
        assert_eq!(report.indexed.usable, 6);
        assert_eq!(report.preloaded, 6);

        for id in [
            "reaction.surprise.01",
            "reaction.surprise.02",
            "reaction.agree.01",
            "reaction.agree.02",
            "filler.thinking.01",
            "filler.thinking.02",
        ] {
            assert_eq!(performer.assets().tier(id), Some(CacheTier::Memory));
        }
    }

    #[test]
    fn all_starter_pack_viseme_tracks_are_valid() {
        let store = LocalVisemeStore::new(pack_root());
        for slug in [
            "reaction-surprise-01",
            "reaction-surprise-02",
            "reaction-agree-01",
            "reaction-agree-02",
            "filler-thinking-01",
            "filler-thinking-02",
        ] {
            let track = store
                .resolve(&format!("viseme/{slug}.json"))
                .expect("valid starter viseme track");
            assert!(!track.cues.is_empty());
        }
    }

    #[test]
    fn starter_pack_has_multiple_variants_for_three_semantic_families() {
        let mut performer = performer();
        performer
            .index_and_preload_fast_path()
            .expect("preload starter pack");

        for group in ["reaction.surprise", "reaction.agree", "filler.thinking"] {
            let first = performer
                .assets()
                .select_variant_id(group, 42, &[])
                .expect("first variant");
            let second = performer
                .assets()
                .select_variant_id(group, 42, &[first.as_str()])
                .expect("second variant");
            assert_ne!(first, second, "group {group} must have multiple variants");
        }
    }

    #[test]
    fn cached_reaction_runs_end_to_end_without_llm_or_tts() {
        let mut performer = performer();
        performer
            .index_and_preload_fast_path()
            .expect("preload starter pack");

        let visemes = LocalVisemeStore::new(pack_root());
        let mut audio = RecordingAudioSink::default();
        let mut avatar = RecordingAvatarSink::default();
        let event = chat_event(1, "reaction.surprise");

        let outcome = performer
            .handle_event(&event, 1_000, 7, &visemes, &mut audio, &mut avatar)
            .expect("cached playback");

        assert!(outcome.asset_id.starts_with("reaction.surprise."));
        assert_eq!(
            performer.assets().tier(&outcome.asset_id),
            Some(CacheTier::Memory)
        );
        assert_eq!(audio.commands.len(), 1);
        assert!(!avatar.commands.is_empty());
        assert!(outcome.metrics.event_to_first_audio_ms.is_some());
        assert!(outcome.metrics.event_to_first_visible_reaction_ms.is_some());
    }

    #[test]
    fn audio_and_visemes_share_the_scheduled_monotonic_timeline() {
        let mut performer = performer();
        performer
            .index_and_preload_fast_path()
            .expect("preload starter pack");

        let visemes = LocalVisemeStore::new(pack_root());
        let mut audio = RecordingAudioSink::default();
        let mut avatar = RecordingAvatarSink::default();
        let event = chat_event(2, "reaction.agree");

        let outcome = performer
            .handle_event(&event, 2_000, 11, &visemes, &mut audio, &mut avatar)
            .expect("cached playback");

        let audio_command = &audio.commands[0];
        let first_viseme = avatar
            .commands
            .iter()
            .find_map(|command| match command {
                AvatarPlaybackCommand::Viseme {
                    generation, at_ms, ..
                } => Some((*generation, *at_ms)),
                _ => None,
            })
            .expect("viseme command");

        assert_eq!(audio_command.generation, outcome.plan.generation);
        assert_eq!(first_viseme.0, outcome.plan.generation);
        assert_eq!(first_viseme.1, audio_command.at_ms);
        assert!(audio_command.at_ms >= outcome.plan.start_at_ms);
        assert!(
            avatar
                .commands
                .iter()
                .all(|command| command.at_ms() >= outcome.plan.start_at_ms)
        );
    }

    #[test]
    fn cached_filler_runs_on_the_same_fast_path() {
        let mut performer = performer();
        performer
            .index_and_preload_fast_path()
            .expect("preload starter pack");

        let visemes = LocalVisemeStore::new(pack_root());
        let mut audio = RecordingAudioSink::default();
        let mut avatar = RecordingAvatarSink::default();
        let event = chat_event(3, "filler.thinking");

        let outcome = performer
            .handle_event(&event, 3_000, 19, &visemes, &mut audio, &mut avatar)
            .expect("cached filler playback");

        assert!(outcome.asset_id.starts_with("filler.thinking."));
        assert_eq!(audio.commands.len(), 1);
        assert!(!avatar.commands.is_empty());
        assert!(!outcome.plan.interrupt_points_ms.is_empty());
    }

    #[test]
    fn immediate_repetition_is_avoided_across_sequential_events() {
        let mut performer = performer();
        performer
            .index_and_preload_fast_path()
            .expect("preload starter pack");

        let visemes = LocalVisemeStore::new(pack_root());
        let mut audio = RecordingAudioSink::default();
        let mut avatar = RecordingAvatarSink::default();

        let first = performer
            .handle_event(
                &chat_event(10, "reaction.surprise"),
                10_000,
                42,
                &visemes,
                &mut audio,
                &mut avatar,
            )
            .expect("first playback");
        let second = performer
            .handle_event(
                &chat_event(11, "reaction.surprise"),
                12_000,
                42,
                &visemes,
                &mut audio,
                &mut avatar,
            )
            .expect("second playback");

        assert_ne!(first.asset_id, second.asset_id);
    }

    #[test]
    fn fast_path_latency_metrics_match_recorded_sink_timestamps() {
        let mut performer = performer();
        performer
            .index_and_preload_fast_path()
            .expect("preload starter pack");

        let visemes = LocalVisemeStore::new(pack_root());
        let mut audio = RecordingAudioSink::default();
        let mut avatar = RecordingAvatarSink::default();
        let event_at_ms = 20_000;

        let outcome = performer
            .handle_event(
                &chat_event(20, "reaction.agree"),
                event_at_ms,
                5,
                &visemes,
                &mut audio,
                &mut avatar,
            )
            .expect("cached playback");

        let first_audio_at = audio.commands[0].at_ms;
        let first_visible_at = avatar
            .commands
            .iter()
            .map(AvatarPlaybackCommand::at_ms)
            .min()
            .expect("visible command");

        assert_eq!(
            outcome.metrics.event_to_first_audio_ms,
            Some(first_audio_at - event_at_ms)
        );
        assert_eq!(
            outcome.metrics.event_to_first_visible_reaction_ms,
            Some(first_visible_at - event_at_ms)
        );
    }
}
