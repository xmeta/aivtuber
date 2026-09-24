#![forbid(unsafe_code)]

//! Deterministic bounded memory and generated-asset adaptation policy.
//!
//! This crate owns policy/state only. SecurityRuntime owns memory-write authority,
//! while AssetStore owns validated persistence primitives.

use aivtuber_asset_store::{AssetStore, AssetStoreError, PerformanceAsset};
use aivtuber_domain::{EventEnvelope, SourceClass, TrustLevel};
use aivtuber_runtime::{MemoryWriteDecision, MemoryWritePermit};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    Working,
    Durable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryGateDecision {
    WorkingOnly,
    AllowedSystemSource,
    AllowedMemoryAdmin,
}

impl From<MemoryWriteDecision> for MemoryGateDecision {
    fn from(value: MemoryWriteDecision) -> Self {
        match value {
            MemoryWriteDecision::AllowedSystemSource => Self::AllowedSystemSource,
            MemoryWriteDecision::AllowedMemoryAdmin => Self::AllowedMemoryAdmin,
            MemoryWriteDecision::Denied => Self::WorkingOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySourceMetadata {
    pub event_id: String,
    pub source: String,
    pub source_class: SourceClass,
    pub trust_level: TrustLevel,
    pub pseudonymous_actor_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub source: MemorySourceMetadata,
    pub normalized_claim: String,
    pub topic: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub retention: RetentionClass,
    pub write_decision: MemoryGateDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingMemoryConfig {
    pub max_entries: usize,
    pub working_ttl_ms: u64,
    pub durable_ttl_ms: u64,
    pub max_claim_bytes: usize,
    pub max_topic_bytes: usize,
    pub pseudonym_salt: u64,
}

impl Default for WorkingMemoryConfig {
    fn default() -> Self {
        Self {
            max_entries: 256,
            working_ttl_ms: 15 * 60 * 1_000,
            durable_ttl_ms: 24 * 60 * 60 * 1_000,
            max_claim_bytes: 1_024,
            max_topic_bytes: 128,
            pseudonym_salt: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCompactionRecord {
    pub at_ms: u64,
    pub expired_removed: usize,
    pub capacity_removed: usize,
    pub remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuery<'a> {
    pub actor_id: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct WorkingMemory {
    config: WorkingMemoryConfig,
    entries: VecDeque<MemoryEntry>,
    compactions: Vec<MemoryCompactionRecord>,
}

impl WorkingMemory {
    pub fn new(config: WorkingMemoryConfig) -> Result<Self, AdaptationError> {
        if config.max_entries == 0
            || config.working_ttl_ms == 0
            || config.durable_ttl_ms == 0
            || config.max_claim_bytes == 0
            || config.max_topic_bytes == 0
        {
            return Err(AdaptationError::InvalidConfiguration(
                "working-memory bounds must be positive",
            ));
        }
        Ok(Self {
            config,
            entries: VecDeque::new(),
            compactions: Vec::new(),
        })
    }

    pub fn config(&self) -> WorkingMemoryConfig {
        self.config
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &VecDeque<MemoryEntry> {
        &self.entries
    }

    pub fn compactions(&self) -> &[MemoryCompactionRecord] {
        &self.compactions
    }

    pub fn remember_working(
        &mut self,
        event: &EventEnvelope,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
    ) -> Result<&MemoryEntry, AdaptationError> {
        let entry = self.entry_from_event(
            event,
            claim,
            topic,
            now_ms,
            RetentionClass::Working,
            MemoryGateDecision::WorkingOnly,
        )?;
        self.push_bounded(entry, now_ms)
    }

    pub fn remember_durable(
        &mut self,
        permit: &MemoryWritePermit,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
    ) -> Result<&MemoryEntry, AdaptationError> {
        let retention = RetentionClass::Durable;
        let claim = bounded_nonempty(claim, self.config.max_claim_bytes, "memory claim")?;
        let topic = bounded_optional(topic, self.config.max_topic_bytes, "memory topic")?;
        let actor = permit
            .actor_id()
            .map(|actor| pseudonymous_actor(actor, self.config.pseudonym_salt));
        let ttl = self.config.durable_ttl_ms;
        let entry = MemoryEntry {
            source: MemorySourceMetadata {
                event_id: permit.event_id().to_owned(),
                source: permit.source().to_owned(),
                source_class: permit.source_class(),
                trust_level: permit.trust_level(),
                pseudonymous_actor_id: actor,
            },
            normalized_claim: claim,
            topic,
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl),
            retention,
            write_decision: permit.decision().into(),
        };
        self.push_bounded(entry, now_ms)
    }

    pub fn compact(&mut self, now_ms: u64) -> MemoryCompactionRecord {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.expires_at_ms > now_ms);
        let expired_removed = before.saturating_sub(self.entries.len());
        let mut capacity_removed = 0;
        while self.entries.len() > self.config.max_entries {
            self.entries.pop_front();
            capacity_removed += 1;
        }
        let record = MemoryCompactionRecord {
            at_ms: now_ms,
            expired_removed,
            capacity_removed,
            remaining: self.entries.len(),
        };
        if expired_removed > 0 || capacity_removed > 0 {
            self.compactions.push(record.clone());
        }
        record
    }

    pub fn relevant(&self, query: MemoryQuery<'_>, now_ms: u64) -> Vec<&MemoryEntry> {
        if query.limit == 0 {
            return Vec::new();
        }
        let actor = query
            .actor_id
            .map(|actor| pseudonymous_actor(actor, self.config.pseudonym_salt));
        let normalized_topic = query.topic.map(normalize);

        let mut ranked = self
            .entries
            .iter()
            .filter(|entry| entry.expires_at_ms > now_ms)
            .map(|entry| {
                let actor_match = u8::from(
                    actor.as_ref().is_some()
                        && actor.as_ref() == entry.source.pseudonymous_actor_id.as_ref(),
                );
                let topic_match = u8::from(
                    normalized_topic.as_deref().is_some()
                        && normalized_topic.as_deref()
                            == entry.topic.as_deref().map(normalize).as_deref(),
                );
                (actor_match, topic_match, entry)
            })
            .filter(|(actor_match, topic_match, _)| {
                actor.is_none() && normalized_topic.is_none()
                    || *actor_match > 0
                    || *topic_match > 0
            })
            .collect::<Vec<_>>();

        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| right.2.created_at_ms.cmp(&left.2.created_at_ms))
                .then_with(|| left.2.source.event_id.cmp(&right.2.source.event_id))
        });
        ranked
            .into_iter()
            .take(query.limit)
            .map(|(_, _, entry)| entry)
            .collect()
    }

    fn entry_from_event(
        &self,
        event: &EventEnvelope,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        retention: RetentionClass,
        write_decision: MemoryGateDecision,
    ) -> Result<MemoryEntry, AdaptationError> {
        let claim = bounded_nonempty(claim, self.config.max_claim_bytes, "memory claim")?;
        let topic = bounded_optional(topic, self.config.max_topic_bytes, "memory topic")?;
        let actor = event
            .actor_id
            .as_deref()
            .map(|actor| pseudonymous_actor(actor, self.config.pseudonym_salt));
        let ttl = match retention {
            RetentionClass::Working => self.config.working_ttl_ms,
            RetentionClass::Durable => self.config.durable_ttl_ms,
        };
        Ok(MemoryEntry {
            source: MemorySourceMetadata {
                event_id: event.event_id.clone(),
                source: event.source.clone(),
                source_class: event.source_class,
                trust_level: event.trust_level,
                pseudonymous_actor_id: actor,
            },
            normalized_claim: claim,
            topic,
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl),
            retention,
            write_decision,
        })
    }

    fn push_bounded(
        &mut self,
        entry: MemoryEntry,
        now_ms: u64,
    ) -> Result<&MemoryEntry, AdaptationError> {
        self.compact(now_ms);
        self.entries.push_back(entry);
        self.compact(now_ms);
        self.entries.back().ok_or(AdaptationError::Invariant(
            "inserted memory entry disappeared",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PromotionPolicy {
    pub min_uses: u64,
    pub min_quality_labels: u64,
    pub min_quality_ratio: f64,
    pub invalidate_after_negative_labels: u64,
    pub recent_variant_window: usize,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            min_uses: 3,
            min_quality_labels: 2,
            min_quality_ratio: 0.8,
            invalidate_after_negative_labels: 3,
            recent_variant_window: 2,
        }
    }
}

impl PromotionPolicy {
    fn validate(self) -> Result<Self, AdaptationError> {
        if self.min_uses == 0
            || self.min_quality_labels == 0
            || !self.min_quality_ratio.is_finite()
            || !(0.0..=1.0).contains(&self.min_quality_ratio)
            || self.invalidate_after_negative_labels == 0
        {
            return Err(AdaptationError::InvalidConfiguration(
                "promotion policy thresholds are invalid",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetFeedback {
    pub uses: u64,
    pub positive_quality_labels: u64,
    pub negative_quality_labels: u64,
}

impl AssetFeedback {
    pub fn quality_ratio(self) -> Option<f64> {
        let total = self
            .positive_quality_labels
            .saturating_add(self.negative_quality_labels);
        (total > 0).then(|| self.positive_quality_labels as f64 / total as f64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptationDecisionKind {
    KeepHot,
    Promote,
    Invalidate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptationDecisionRecord {
    pub sequence: u64,
    pub asset_id: String,
    pub asset_identity: String,
    pub decision: AdaptationDecisionKind,
    pub reason: String,
    pub feedback: AssetFeedback,
    pub policy_version: String,
    pub policy: PromotionPolicy,
    pub seed: u64,
}

impl AdaptationDecisionRecord {
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedAdaptation {
    KeptHot,
    Promoted(PathBuf),
    Invalidated,
}

#[derive(Debug, Clone)]
pub struct AdaptationEngine {
    policy: PromotionPolicy,
    policy_version: String,
    seed: u64,
    sequence: u64,
    feedback: BTreeMap<String, AssetFeedback>,
    recent_by_group: BTreeMap<String, VecDeque<String>>,
    decisions: Vec<AdaptationDecisionRecord>,
}

impl AdaptationEngine {
    pub fn new(
        policy: PromotionPolicy,
        policy_version: impl Into<String>,
        seed: u64,
    ) -> Result<Self, AdaptationError> {
        let policy = policy.validate()?;
        let policy_version = policy_version.into();
        if policy_version.trim().is_empty() {
            return Err(AdaptationError::InvalidConfiguration(
                "policy_version must not be empty",
            ));
        }
        Ok(Self {
            policy,
            policy_version,
            seed,
            sequence: 0,
            feedback: BTreeMap::new(),
            recent_by_group: BTreeMap::new(),
            decisions: Vec::new(),
        })
    }

    pub fn policy(&self) -> PromotionPolicy {
        self.policy
    }

    pub fn feedback(&self, asset_id: &str) -> AssetFeedback {
        self.feedback.get(asset_id).copied().unwrap_or_default()
    }

    pub fn decisions(&self) -> &[AdaptationDecisionRecord] {
        &self.decisions
    }

    pub fn record_use(&mut self, asset: &PerformanceAsset) {
        let feedback = self.feedback.entry(asset.id.clone()).or_default();
        feedback.uses = feedback.uses.saturating_add(1);
        let group = asset
            .variant_group
            .as_deref()
            .unwrap_or(asset.intent.as_str())
            .to_owned();
        let recent = self.recent_by_group.entry(group).or_default();
        recent.push_back(asset.id.clone());
        while recent.len() > self.policy.recent_variant_window {
            recent.pop_front();
        }
    }

    pub fn record_quality(&mut self, asset_id: &str, positive: bool) {
        let feedback = self.feedback.entry(asset_id.to_owned()).or_default();
        if positive {
            feedback.positive_quality_labels = feedback.positive_quality_labels.saturating_add(1);
        } else {
            feedback.negative_quality_labels = feedback.negative_quality_labels.saturating_add(1);
        }
    }

    pub fn evaluate(
        &mut self,
        asset: &PerformanceAsset,
    ) -> Result<AdaptationDecisionRecord, AdaptationError> {
        ensure_promotable(asset)?;
        let feedback = self.feedback(&asset.id);
        let label_count = feedback
            .positive_quality_labels
            .saturating_add(feedback.negative_quality_labels);
        let quality = feedback.quality_ratio();

        let (decision, reason) =
            if feedback.negative_quality_labels >= self.policy.invalidate_after_negative_labels {
                (
                    AdaptationDecisionKind::Invalidate,
                    "negative_quality_threshold",
                )
            } else if feedback.uses >= self.policy.min_uses
                && label_count >= self.policy.min_quality_labels
                && quality.is_some_and(|ratio| ratio >= self.policy.min_quality_ratio)
            {
                (
                    AdaptationDecisionKind::Promote,
                    "frequency_and_quality_thresholds",
                )
            } else {
                (AdaptationDecisionKind::KeepHot, "thresholds_not_met")
            };

        self.sequence = self.sequence.saturating_add(1);
        let record = AdaptationDecisionRecord {
            sequence: self.sequence,
            asset_id: asset.id.clone(),
            asset_identity: asset.identity().stable_key(),
            decision,
            reason: reason.to_owned(),
            feedback,
            policy_version: self.policy_version.clone(),
            policy: self.policy,
            seed: self.seed,
        };
        self.decisions.push(record.clone());
        Ok(record)
    }

    pub fn apply(
        &mut self,
        store: &mut AssetStore,
        asset_id: &str,
    ) -> Result<AppliedAdaptation, AdaptationError> {
        let asset = store.hot_get(asset_id).ok_or_else(|| {
            AdaptationError::AssetStore(AssetStoreError::NotFound {
                id: asset_id.to_owned(),
            })
        })?;
        let decision = self.evaluate(&asset)?;
        match decision.decision {
            AdaptationDecisionKind::KeepHot => Ok(AppliedAdaptation::KeptHot),
            AdaptationDecisionKind::Promote => store
                .persist_generated_descriptor(asset_id)
                .map(AppliedAdaptation::Promoted)
                .map_err(AdaptationError::AssetStore),
            AdaptationDecisionKind::Invalidate => {
                store
                    .invalidate_generated_asset(asset_id)
                    .map_err(AdaptationError::AssetStore)?;
                Ok(AppliedAdaptation::Invalidated)
            }
        }
    }

    pub fn select_variant<'a>(
        &self,
        variant_group: &str,
        candidates: &'a [PerformanceAsset],
        event_sequence: u64,
    ) -> Option<&'a PerformanceAsset> {
        if candidates.is_empty() {
            return None;
        }
        let recent = self.recent_by_group.get(variant_group);
        let mut preferred = candidates
            .iter()
            .filter(|asset| {
                asset.variant_group.as_deref() == Some(variant_group)
                    && recent.is_none_or(|recent| !recent.contains(&asset.id))
            })
            .collect::<Vec<_>>();
        if preferred.is_empty() {
            preferred = candidates
                .iter()
                .filter(|asset| asset.variant_group.as_deref() == Some(variant_group))
                .collect();
        }
        preferred.sort_by(|left, right| left.id.cmp(&right.id));
        if preferred.is_empty() {
            return None;
        }
        let index = stable_index(
            self.seed.wrapping_add(event_sequence),
            variant_group,
            preferred.len(),
        );
        preferred.get(index).copied()
    }
}

fn ensure_promotable(asset: &PerformanceAsset) -> Result<(), AdaptationError> {
    asset
        .validate()
        .map_err(|error| AdaptationError::InvalidAsset(error.to_string()))?;
    if !asset
        .provenance
        .as_ref()
        .is_some_and(|provenance| provenance.generated == Some(true))
    {
        return Err(AdaptationError::InvalidAsset(format!(
            "asset {:?} is not generated",
            asset.id
        )));
    }
    Ok(())
}

fn stable_index(seed: u64, key: &str, len: usize) -> usize {
    debug_assert!(len > 0);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in seed.to_le_bytes().into_iter().chain(key.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash % len as u64) as usize
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn bounded_nonempty(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<String, AdaptationError> {
    let normalized = normalize(value);
    if normalized.is_empty() {
        return Err(AdaptationError::InvalidInput(field));
    }
    Ok(truncate_utf8(&normalized, max_bytes))
}

fn bounded_optional(
    value: Option<&str>,
    max_bytes: usize,
    field: &'static str,
) -> Result<Option<String>, AdaptationError> {
    value
        .map(|value| bounded_nonempty(value, max_bytes, field))
        .transpose()
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn pseudonymous_actor(actor_id: &str, salt: u64) -> String {
    let mut hash = 0xcbf29ce484222325_u64 ^ salt;
    for byte in actor_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("actor:{hash:016x}")
}

#[derive(Debug)]
pub enum AdaptationError {
    InvalidConfiguration(&'static str),
    InvalidInput(&'static str),
    InvalidAsset(String),
    Invariant(&'static str),
    AssetStore(AssetStoreError),
}

impl fmt::Display for AdaptationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(f, "invalid adaptation configuration: {message}")
            }
            Self::InvalidInput(field) => write!(f, "invalid adaptation input: {field}"),
            Self::InvalidAsset(message) => write!(f, "invalid adaptation asset: {message}"),
            Self::Invariant(message) => write!(f, "adaptation invariant failed: {message}"),
            Self::AssetStore(error) => write!(f, "{error}"),
        }
    }
}

impl Error for AdaptationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AssetStore(error) => Some(error),
            _ => None,
        }
    }
}

impl From<AssetStoreError> for AdaptationError {
    fn from(value: AssetStoreError) -> Self {
        Self::AssetStore(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_asset_store::{RuntimeCompatibility, load_asset_file};
    use aivtuber_domain::{
        EVENT_SCHEMA_VERSION, EventKind, SecurityPlane, SourceClass, TrustLevel,
    };
    use aivtuber_runtime::{SecurityRuntime, SecurityRuntimeConfig};
    use aivtuber_scheduler::SchedulerConfig;
    use aivtuber_telemetry::SecretRedactor;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn event(
        id: &str,
        source_class: SourceClass,
        trust_level: TrustLevel,
        actor_id: Option<&str>,
    ) -> EventEnvelope {
        let (plane, kind, source) = match source_class {
            SourceClass::System => (
                SecurityPlane::System,
                EventKind::SystemHealth,
                "system-health",
            ),
            _ => (
                SecurityPlane::Content,
                EventKind::ChatMessage,
                "public-chat",
            ),
        };
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: id.to_owned(),
            correlation_id: format!("corr-{id}"),
            sequence: id.bytes().map(u64::from).sum(),
            observed_at: "2026-09-25T00:00:00Z".to_owned(),
            source: source.to_owned(),
            source_class,
            plane,
            trust_level,
            kind,
            actor_id: actor_id.map(str::to_owned),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([(
                "text".to_owned(),
                Value::String("remember this".to_owned()),
            )]),
        }
    }

    fn security() -> SecurityRuntime {
        SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig::default(),
            SecretRedactor::default(),
            None,
        )
        .expect("security runtime")
    }

    fn generated_fixture() -> PerformanceAsset {
        load_asset_file(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/performance-assets/valid/generated-dynamic.json"),
        )
        .expect("generated fixture")
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

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aivtuber-adaptation-{name}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test dir");
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

    #[test]
    fn public_chat_cannot_mint_durable_memory_and_working_memory_is_pseudonymous() {
        let chat = event(
            "evt-chat",
            SourceClass::PublicChat,
            TrustLevel::Untrusted,
            Some("viewer-real-id"),
        );
        let mut security = security();
        let (decision, permit) = security.authorize_memory_write(&chat, None);
        assert_eq!(decision, MemoryWriteDecision::Denied);
        assert!(permit.is_none());

        let mut memory = WorkingMemory::new(WorkingMemoryConfig {
            max_entries: 2,
            working_ttl_ms: 100,
            durable_ttl_ms: 1_000,
            pseudonym_salt: 99,
            ..WorkingMemoryConfig::default()
        })
        .expect("working memory");
        let stored = memory
            .remember_working(&chat, "  Likes   Rust  ", Some(" Coding "), 10)
            .expect("working entry");
        assert_eq!(stored.retention, RetentionClass::Working);
        assert_eq!(stored.write_decision, MemoryGateDecision::WorkingOnly);
        assert_eq!(stored.normalized_claim, "likes rust");
        assert_eq!(stored.topic.as_deref(), Some("coding"));
        assert_ne!(
            stored.source.pseudonymous_actor_id.as_deref(),
            Some("viewer-real-id")
        );
        assert_eq!(stored.expires_at_ms, 110);

        let compacted = memory.compact(110);
        assert_eq!(compacted.expired_removed, 1);
        assert!(memory.is_empty());
    }

    #[test]
    fn durable_memory_requires_gate_permit_and_preserves_gate_metadata() {
        let system = event("evt-system", SourceClass::System, TrustLevel::Trusted, None);
        let mut security = security();
        let (decision, permit) = security.authorize_memory_write(&system, None);
        assert_eq!(decision, MemoryWriteDecision::AllowedSystemSource);
        let permit = permit.expect("system permit");

        let mut memory = WorkingMemory::new(WorkingMemoryConfig {
            max_entries: 4,
            working_ttl_ms: 100,
            durable_ttl_ms: 1_000,
            ..WorkingMemoryConfig::default()
        })
        .expect("working memory");
        let stored = memory
            .remember_durable(&permit, "stream is healthy", Some("health"), 50)
            .expect("durable entry");
        assert_eq!(stored.retention, RetentionClass::Durable);
        assert_eq!(
            stored.write_decision,
            MemoryGateDecision::AllowedSystemSource
        );
        assert_eq!(stored.source.event_id, "evt-system");
        assert_eq!(stored.source.source_class, SourceClass::System);
        assert_eq!(stored.source.trust_level, TrustLevel::Trusted);
        assert_eq!(stored.expires_at_ms, 1_050);
    }

    #[test]
    fn working_memory_compaction_enforces_capacity_deterministically() {
        let config = WorkingMemoryConfig {
            max_entries: 2,
            working_ttl_ms: 10_000,
            durable_ttl_ms: 20_000,
            ..WorkingMemoryConfig::default()
        };
        let mut first = WorkingMemory::new(config).expect("memory");
        let mut second = WorkingMemory::new(config).expect("memory");

        for memory in [&mut first, &mut second] {
            for index in 1..=3 {
                let event = event(
                    &format!("evt-{index}"),
                    SourceClass::PublicChat,
                    TrustLevel::Untrusted,
                    Some("viewer"),
                );
                memory
                    .remember_working(&event, &format!("claim {index}"), Some("topic"), index)
                    .expect("remember");
            }
        }

        assert_eq!(first.len(), 2);
        assert_eq!(first.entries(), second.entries());
        assert_eq!(
            first
                .entries()
                .iter()
                .map(|entry| entry.source.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["evt-2", "evt-3"]
        );
        assert_eq!(
            first
                .compactions()
                .last()
                .expect("capacity compaction")
                .capacity_removed,
            1
        );
    }

    #[test]
    fn viewer_and_topic_relevance_is_deterministic_without_raw_actor_identity() {
        let mut memory = WorkingMemory::new(WorkingMemoryConfig {
            pseudonym_salt: 7,
            ..WorkingMemoryConfig::default()
        })
        .expect("memory");
        let a = event(
            "evt-a",
            SourceClass::PublicChat,
            TrustLevel::Untrusted,
            Some("viewer-a"),
        );
        let b = event(
            "evt-b",
            SourceClass::PublicChat,
            TrustLevel::Untrusted,
            Some("viewer-b"),
        );
        memory
            .remember_working(&a, "likes rust", Some("coding"), 10)
            .expect("a");
        memory
            .remember_working(&b, "likes music", Some("music"), 20)
            .expect("b");

        let result = memory.relevant(
            MemoryQuery {
                actor_id: Some("viewer-a"),
                topic: Some("coding"),
                limit: 2,
            },
            30,
        );
        assert_eq!(result[0].source.event_id, "evt-a");
        assert!(
            memory
                .entries()
                .iter()
                .all(|entry| entry.source.pseudonymous_actor_id.as_deref() != Some("viewer-a"))
        );
    }

    #[test]
    fn promotion_and_invalidation_follow_explicit_frequency_quality_policy() {
        let dir = TestDir::new("promotion");
        let asset = generated_fixture();
        let id = asset.id.clone();
        let provenance = asset.provenance.clone();
        let mut store = AssetStore::new(dir.path(), generated_runtime());
        store
            .insert_hot(asset.clone())
            .expect("hot generated asset");

        let policy = PromotionPolicy {
            min_uses: 2,
            min_quality_labels: 2,
            min_quality_ratio: 0.75,
            invalidate_after_negative_labels: 2,
            recent_variant_window: 1,
        };
        let mut adaptation = AdaptationEngine::new(policy, "promotion-v1", 42).expect("adaptation");

        adaptation.record_use(&asset);
        adaptation.record_quality(&id, true);
        assert_eq!(
            adaptation.apply(&mut store, &id).expect("keep hot"),
            AppliedAdaptation::KeptHot
        );

        adaptation.record_use(&asset);
        adaptation.record_quality(&id, true);
        let promoted = adaptation.apply(&mut store, &id).expect("promote");
        let path = match promoted {
            AppliedAdaptation::Promoted(path) => path,
            other => panic!("expected promotion, got {other:?}"),
        };
        let persisted = load_asset_file(&path).expect("persisted");
        assert_eq!(persisted.provenance, provenance);
        assert!(persisted.compatibility.check(store.runtime()).is_usable());

        adaptation.record_quality(&id, false);
        adaptation.record_quality(&id, false);
        assert_eq!(
            adaptation.apply(&mut store, &id).expect("invalidate"),
            AppliedAdaptation::Invalidated
        );
        assert!(!path.exists());
        assert!(store.hot_get(&id).is_none());

        let decisions = adaptation.decisions();
        assert_eq!(
            decisions
                .iter()
                .map(|record| record.decision)
                .collect::<Vec<_>>(),
            vec![
                AdaptationDecisionKind::KeepHot,
                AdaptationDecisionKind::Promote,
                AdaptationDecisionKind::Invalidate
            ]
        );
        assert!(
            decisions
                .iter()
                .all(|record| record.policy_version == "promotion-v1")
        );
        assert!(decisions.iter().all(|record| record.seed == 42));
    }

    #[test]
    fn adaptation_replay_and_anti_repetition_are_seed_deterministic() {
        let mut first =
            AdaptationEngine::new(PromotionPolicy::default(), "policy-v1", 4242).expect("first");
        let mut second =
            AdaptationEngine::new(PromotionPolicy::default(), "policy-v1", 4242).expect("second");

        let mut a = generated_fixture();
        a.id = "dynamic.variant.a".to_owned();
        a.variant_group = Some("dynamic.reply".to_owned());
        let mut b = a.clone();
        b.id = "dynamic.variant.b".to_owned();
        let candidates = vec![a.clone(), b.clone()];

        first.record_use(&a);
        second.record_use(&a);
        first.record_quality(&a.id, true);
        second.record_quality(&a.id, true);

        let first_decision = first.evaluate(&a).expect("decision");
        let second_decision = second.evaluate(&a).expect("decision");
        assert_eq!(first_decision, second_decision);
        assert_eq!(
            first_decision.to_json_bytes().expect("serialize decision"),
            second_decision.to_json_bytes().expect("serialize decision")
        );
        let selected_first = first
            .select_variant("dynamic.reply", &candidates, 9)
            .expect("variant");
        let selected_second = second
            .select_variant("dynamic.reply", &candidates, 9)
            .expect("variant");
        assert_eq!(selected_first.id, selected_second.id);
        assert_eq!(selected_first.id, "dynamic.variant.b");
    }
}
