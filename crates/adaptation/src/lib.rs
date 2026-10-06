#![forbid(unsafe_code)]

//! Deterministic bounded memory and generated-asset adaptation policy.
//!
//! This crate owns policy/state only. SecurityRuntime owns memory-write authority,
//! while AssetStore owns validated persistence primitives.

use aivtuber_asset_store::{AssetStore, AssetStoreError, PerformanceAsset};
use aivtuber_domain::{EventEnvelope, SourceClass, TrustLevel};
use aivtuber_runtime::{MemoryWriteDecision, MemoryWritePermit};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, HashSet, VecDeque};
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

/// Versioned edge vocabulary for retained episodic memory (#105). Kept small
/// on purpose: temporal order is derived deterministically at write time and
/// supersession is declared by the caller that owns the update evidence;
/// causal kinds wait for an evidence source that can actually establish them.
pub const MEMORY_LINK_VOCAB_VERSION: &str = "memory-link-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLinkKind {
    /// `from` was recorded before `to` within one topic — derived at write
    /// time from the retained timeline, never from model output.
    TemporalBefore,
    /// `from` explicitly supersedes `to` — declared at write time through the
    /// same permit path as the claim itself.
    Supersedes,
}

/// One edge between retained memory ids. It carries no claim content — only
/// ids, a kind, and record time — so expiry or deletion can never leave
/// removed content reachable through link metadata (security threat model
/// §8: an edge must not become a covert archive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryLink {
    pub from: String,
    pub to: String,
    pub kind: MemoryLinkKind,
    pub created_at_ms: u64,
}

/// One retrieved memory together with its bounded link view. This is the
/// retrieval surface the #105 long-range evaluation measures: `stale` marks a
/// claim a retained memory explicitly supersedes, and `links` is capped at
/// the per-memory budget so a neighborhood can never fan out unboundedly.
#[derive(Debug, Clone, Serialize)]
pub struct LinkedMemory<'a> {
    pub entry: &'a MemoryEntry,
    pub stale: bool,
    pub links: Vec<MemoryLink>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ActorPseudonymizer {
    key_version: String,
    key: [u8; 32],
}

impl fmt::Debug for ActorPseudonymizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActorPseudonymizer")
            .field("key_version", &self.key_version)
            .field("key", &"[REDACTED]")
            .finish()
    }
}

impl ActorPseudonymizer {
    pub fn new(key_version: impl Into<String>, key: [u8; 32]) -> Result<Self, AdaptationError> {
        let key_version = key_version.into();
        if key_version.is_empty()
            || key_version.len() > 64
            || !key_version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(AdaptationError::InvalidConfiguration(
                "pseudonym key_version must be 1..=64 bytes of [A-Za-z0-9._-]",
            ));
        }
        if key.iter().all(|byte| *byte == 0) {
            return Err(AdaptationError::InvalidConfiguration(
                "pseudonym key must not be all zeroes",
            ));
        }
        Ok(Self { key_version, key })
    }

    pub fn key_version(&self) -> &str {
        &self.key_version
    }

    pub fn pseudonymize(&self, source_namespace: &str, actor_id: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts 32-byte keys");
        mac.update(b"aivtuber.actor-pseudonym.v1\0");
        update_len_prefixed(&mut mac, self.key_version.as_bytes());
        update_len_prefixed(&mut mac, source_namespace.as_bytes());
        update_len_prefixed(&mut mac, actor_id.as_bytes());
        let digest = mac.finalize().into_bytes();
        let mut encoded = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        format!("actor:{}:{encoded}", self.key_version)
    }
}

fn update_len_prefixed(mac: &mut Hmac<Sha256>, value: &[u8]) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingMemoryConfig {
    pub max_entries: usize,
    pub working_ttl_ms: u64,
    pub durable_ttl_ms: u64,
    pub max_claim_bytes: usize,
    pub max_topic_bytes: usize,
    pub max_compaction_records: usize,
    /// Total retained memory links. The unified retention policy (#51) owns
    /// this bound exactly as it owns the node bound.
    pub max_links: usize,
    /// Per-write creation budget: how many links one durable write may
    /// create (explicit supersessions plus the temporal neighborhood), which
    /// keeps per-memory degree creation-bounded; the store total is
    /// `max_links`.
    pub max_links_per_memory: usize,
}

impl Default for WorkingMemoryConfig {
    fn default() -> Self {
        Self {
            max_entries: 256,
            working_ttl_ms: 15 * 60 * 1_000,
            durable_ttl_ms: 24 * 60 * 60 * 1_000,
            max_claim_bytes: 1_024,
            max_topic_bytes: 128,
            max_compaction_records: 256,
            max_links: 2_048,
            max_links_per_memory: 8,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingMemoryRetentionMetrics {
    pub entries: usize,
    pub entries_high_water: usize,
    pub compactions: usize,
    pub compactions_high_water: usize,
    pub compactions_evicted: u64,
    pub links: usize,
    pub links_high_water: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuery<'a> {
    pub source_namespace: Option<&'a str>,
    pub actor_id: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct WorkingMemory {
    config: WorkingMemoryConfig,
    pseudonymizer: ActorPseudonymizer,
    entries: VecDeque<MemoryEntry>,
    links: VecDeque<MemoryLink>,
    compactions: Vec<MemoryCompactionRecord>,
    entries_high_water: usize,
    links_high_water: usize,
    compactions_high_water: usize,
    compactions_evicted: u64,
}

impl WorkingMemory {
    pub fn new(
        config: WorkingMemoryConfig,
        pseudonymizer: ActorPseudonymizer,
    ) -> Result<Self, AdaptationError> {
        if config.max_entries == 0
            || config.working_ttl_ms == 0
            || config.durable_ttl_ms == 0
            || config.max_claim_bytes == 0
            || config.max_topic_bytes == 0
            || config.max_compaction_records == 0
            || config.max_links == 0
            || config.max_links_per_memory == 0
        {
            return Err(AdaptationError::InvalidConfiguration(
                "working-memory bounds must be positive",
            ));
        }
        Ok(Self {
            config,
            pseudonymizer,
            entries: VecDeque::new(),
            links: VecDeque::new(),
            compactions: Vec::new(),
            entries_high_water: 0,
            links_high_water: 0,
            compactions_high_water: 0,
            compactions_evicted: 0,
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

    pub fn links(&self) -> &VecDeque<MemoryLink> {
        &self.links
    }

    pub fn compactions(&self) -> &[MemoryCompactionRecord] {
        &self.compactions
    }

    pub fn retention_metrics(&self) -> WorkingMemoryRetentionMetrics {
        WorkingMemoryRetentionMetrics {
            entries: self.entries.len(),
            entries_high_water: self.entries_high_water,
            compactions: self.compactions.len(),
            compactions_high_water: self.compactions_high_water,
            compactions_evicted: self.compactions_evicted,
            links: self.links.len(),
            links_high_water: self.links_high_water,
        }
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
        self.remember_durable_superseding(permit, claim, topic, now_ms, &[])
    }

    /// A durable write that may also declare supersession: `supersedes` names
    /// retained durable memories this claim replaces (issue #105's update
    /// evidence, owned by the caller). Targets are validated *before* anything
    /// is stored, links ride the same non-serializable permit path as the
    /// claim, and `max_links_per_memory` bounds what one write may create.
    /// A denial therefore stores nothing and mints nothing.
    pub fn remember_durable_superseding(
        &mut self,
        permit: &MemoryWritePermit,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[&str],
    ) -> Result<&MemoryEntry, AdaptationError> {
        if supersedes.len() > self.config.max_links_per_memory {
            return Err(AdaptationError::InvalidInput(
                "one write may not declare more supersessions than the per-memory link budget",
            ));
        }
        for target in supersedes {
            self.require_durable_memory(target)?;
        }
        let retention = RetentionClass::Durable;
        let claim = bounded_nonempty(claim, self.config.max_claim_bytes, "memory claim")?;
        let topic = bounded_optional(topic, self.config.max_topic_bytes, "memory topic")?;
        let actor = permit
            .actor_id()
            .map(|actor| self.pseudonymizer.pseudonymize(permit.source(), actor));
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
        let event_id = entry.source.event_id.clone();
        let topic = entry.topic.clone();
        self.push_bounded(entry, now_ms)?;
        self.link_new_memory(&event_id, topic.as_deref(), now_ms, supersedes);
        self.entries.back().ok_or(AdaptationError::Invariant(
            "inserted memory entry disappeared",
        ))
    }

    /// Fail closed: a supersession edge may only point at a memory that is
    /// actually retained here as durable. Working-only or unknown ids are
    /// refused, so no phantom or cross-retention edge can exist.
    fn require_durable_memory(&self, id: &str) -> Result<(), AdaptationError> {
        let found = self
            .entries
            .iter()
            .any(|entry| entry.source.event_id == id && entry.retention == RetentionClass::Durable);
        found.then_some(()).ok_or(AdaptationError::InvalidInput(
            "supersession target must be a retained durable memory",
        ))
    }

    /// Create this write's links, deterministic by construction: explicit
    /// supersessions first (they are the caller-owned evidence), then a
    /// temporal spine to the most recent same-topic durable memories, all
    /// within the per-memory budget. Same claims written twice dedupe to one
    /// edge, and the store total is re-bounded afterwards.
    fn link_new_memory(
        &mut self,
        new_id: &str,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[&str],
    ) {
        let mut budget = self.config.max_links_per_memory;
        for target in supersedes {
            if budget == 0 {
                break;
            }
            self.push_link(MemoryLink {
                from: new_id.to_owned(),
                to: (*target).to_owned(),
                kind: MemoryLinkKind::Supersedes,
                created_at_ms: now_ms,
            });
            budget -= 1;
        }
        if let Some(topic) = topic {
            let normalized = normalize(topic);
            // Newest first (the new entry sits at the back and is filtered by
            // id), so the spine links to the most recent neighbors first and
            // the budget cuts deterministically.
            let older: Vec<String> = self
                .entries
                .iter()
                .rev()
                .filter(|entry| {
                    entry.retention == RetentionClass::Durable
                        && entry.source.event_id != new_id
                        && entry.topic.as_deref().map(normalize).as_deref()
                            == Some(normalized.as_str())
                })
                .take(budget)
                .map(|entry| entry.source.event_id.clone())
                .collect();
            for id in older {
                self.push_link(MemoryLink {
                    from: id,
                    to: new_id.to_owned(),
                    kind: MemoryLinkKind::TemporalBefore,
                    created_at_ms: now_ms,
                });
            }
        }
        // The insert above may have compacted a linking target away (capacity
        // or TTL), so edges are pruned against the surviving store before the
        // bound is re-enforced: no link ever outlives either endpoint.
        self.prune_links();
        self.enforce_link_bound();
    }

    fn prune_links(&mut self) {
        let alive: HashSet<&str> = self
            .entries
            .iter()
            .map(|entry| entry.source.event_id.as_str())
            .collect();
        self.links
            .retain(|link| alive.contains(link.from.as_str()) && alive.contains(link.to.as_str()));
    }

    fn push_link(&mut self, link: MemoryLink) {
        let duplicate = self.links.iter().any(|existing| {
            existing.kind == link.kind && existing.from == link.from && existing.to == link.to
        });
        if !duplicate {
            self.links.push_back(link);
        }
    }

    fn enforce_link_bound(&mut self) {
        while self.links.len() > self.config.max_links {
            self.links.pop_front();
        }
        self.links_high_water = self.links_high_water.max(self.links.len());
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
        // Deletion/expiry applies to relation metadata too: an edge may never
        // outlive either endpoint, so no removed memory id — and, structurally,
        // no removed content — stays reachable through links.
        self.prune_links();
        self.entries_high_water = self.entries_high_water.max(self.entries.len());
        let record = MemoryCompactionRecord {
            at_ms: now_ms,
            expired_removed,
            capacity_removed,
            remaining: self.entries.len(),
        };
        if expired_removed > 0 || capacity_removed > 0 {
            self.compactions.push(record.clone());
            if self.compactions.len() > self.config.max_compaction_records {
                let remove = self.compactions.len() - self.config.max_compaction_records;
                self.compactions.drain(..remove);
                self.compactions_evicted = self.compactions_evicted.saturating_add(remove as u64);
            }
            self.compactions_high_water = self.compactions_high_water.max(self.compactions.len());
        }
        record
    }

    pub fn relevant(&self, query: MemoryQuery<'_>, now_ms: u64) -> Vec<&MemoryEntry> {
        if query.limit == 0 {
            return Vec::new();
        }
        let actor = query
            .source_namespace
            .zip(query.actor_id)
            .map(|(source, actor)| self.pseudonymizer.pseudonymize(source, actor));
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

    /// The same ranking as [`Self::relevant`], each result joined with its
    /// bounded link view: links touching the entry (creation-order capped at
    /// `max_links_per_memory`, so a neighborhood can never fan out) and
    /// whether a *still-retained* memory explicitly supersedes it. Pruning
    /// guarantees a superseder that expired or was evicted takes its edge
    /// with it, so `stale` never cites a memory that is gone.
    pub fn relevant_linked(&self, query: MemoryQuery<'_>, now_ms: u64) -> Vec<LinkedMemory<'_>> {
        self.relevant(query, now_ms)
            .into_iter()
            .map(|entry| {
                let id = entry.source.event_id.as_str();
                let links: Vec<MemoryLink> = self
                    .links
                    .iter()
                    .filter(|link| link.from == id || link.to == id)
                    .take(self.config.max_links_per_memory)
                    .cloned()
                    .collect();
                let stale = links
                    .iter()
                    .any(|link| link.kind == MemoryLinkKind::Supersedes && link.to == id);
                LinkedMemory {
                    entry,
                    stale,
                    links,
                }
            })
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
            .map(|actor| self.pseudonymizer.pseudonymize(&event.source, actor));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptationRetentionConfig {
    pub max_feedback_assets: usize,
    pub max_recent_groups: usize,
    pub max_decisions: usize,
}

impl Default for AdaptationRetentionConfig {
    fn default() -> Self {
        Self {
            max_feedback_assets: 1_024,
            max_recent_groups: 256,
            max_decisions: 1_024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptationRetentionMetrics {
    pub feedback_assets: usize,
    pub feedback_assets_high_water: usize,
    pub feedback_assets_evicted: u64,
    pub recent_groups: usize,
    pub recent_groups_high_water: usize,
    pub recent_groups_evicted: u64,
    pub decisions: usize,
    pub decisions_high_water: usize,
    pub decisions_evicted: u64,
}

#[derive(Debug, Clone)]
pub struct AdaptationEngine {
    policy: PromotionPolicy,
    policy_version: String,
    seed: u64,
    sequence: u64,
    retention: AdaptationRetentionConfig,
    feedback: BTreeMap<String, AssetFeedback>,
    feedback_touch: BTreeMap<String, u64>,
    recent_by_group: BTreeMap<String, VecDeque<String>>,
    recent_group_touch: BTreeMap<String, u64>,
    decisions: Vec<AdaptationDecisionRecord>,
    next_touch: u64,
    feedback_high_water: usize,
    feedback_evicted: u64,
    recent_groups_high_water: usize,
    recent_groups_evicted: u64,
    decisions_high_water: usize,
    decisions_evicted: u64,
}

impl AdaptationEngine {
    pub fn new(
        policy: PromotionPolicy,
        policy_version: impl Into<String>,
        seed: u64,
    ) -> Result<Self, AdaptationError> {
        Self::with_retention(
            policy,
            policy_version,
            seed,
            AdaptationRetentionConfig::default(),
        )
    }

    pub fn with_retention(
        policy: PromotionPolicy,
        policy_version: impl Into<String>,
        seed: u64,
        retention: AdaptationRetentionConfig,
    ) -> Result<Self, AdaptationError> {
        let policy = policy.validate()?;
        let policy_version = policy_version.into();
        if policy_version.trim().is_empty() {
            return Err(AdaptationError::InvalidConfiguration(
                "policy_version must not be empty",
            ));
        }
        if retention.max_feedback_assets == 0
            || retention.max_recent_groups == 0
            || retention.max_decisions == 0
        {
            return Err(AdaptationError::InvalidConfiguration(
                "adaptation retention bounds must be positive",
            ));
        }
        Ok(Self {
            policy,
            policy_version,
            seed,
            sequence: 0,
            retention,
            feedback: BTreeMap::new(),
            feedback_touch: BTreeMap::new(),
            recent_by_group: BTreeMap::new(),
            recent_group_touch: BTreeMap::new(),
            decisions: Vec::new(),
            next_touch: 0,
            feedback_high_water: 0,
            feedback_evicted: 0,
            recent_groups_high_water: 0,
            recent_groups_evicted: 0,
            decisions_high_water: 0,
            decisions_evicted: 0,
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

    pub fn retention_metrics(&self) -> AdaptationRetentionMetrics {
        AdaptationRetentionMetrics {
            feedback_assets: self.feedback.len(),
            feedback_assets_high_water: self.feedback_high_water,
            feedback_assets_evicted: self.feedback_evicted,
            recent_groups: self.recent_by_group.len(),
            recent_groups_high_water: self.recent_groups_high_water,
            recent_groups_evicted: self.recent_groups_evicted,
            decisions: self.decisions.len(),
            decisions_high_water: self.decisions_high_water,
            decisions_evicted: self.decisions_evicted,
        }
    }

    pub fn record_use(&mut self, asset: &PerformanceAsset) {
        self.ensure_feedback_asset(&asset.id);
        let feedback = self
            .feedback
            .get_mut(&asset.id)
            .expect("feedback asset inserted before use");
        feedback.uses = feedback.uses.saturating_add(1);
        let group = asset
            .variant_group
            .as_deref()
            .unwrap_or(asset.intent.as_str())
            .to_owned();
        self.ensure_recent_group(&group);
        let recent = self
            .recent_by_group
            .get_mut(&group)
            .expect("recent group inserted before use");
        recent.push_back(asset.id.clone());
        while recent.len() > self.policy.recent_variant_window {
            recent.pop_front();
        }
    }

    pub fn record_quality(&mut self, asset_id: &str, positive: bool) {
        self.ensure_feedback_asset(asset_id);
        let feedback = self
            .feedback
            .get_mut(asset_id)
            .expect("feedback asset inserted before quality label");
        if positive {
            feedback.positive_quality_labels = feedback.positive_quality_labels.saturating_add(1);
        } else {
            feedback.negative_quality_labels = feedback.negative_quality_labels.saturating_add(1);
        }
    }

    fn next_touch_tick(&mut self) -> u64 {
        let tick = self.next_touch;
        self.next_touch = self.next_touch.saturating_add(1);
        tick
    }

    fn ensure_feedback_asset(&mut self, asset_id: &str) {
        let tick = self.next_touch_tick();
        if !self.feedback.contains_key(asset_id)
            && self.feedback.len() >= self.retention.max_feedback_assets
        {
            let victim = self
                .feedback_touch
                .iter()
                .min_by(|(left_id, left_tick), (right_id, right_tick)| {
                    left_tick
                        .cmp(right_tick)
                        .then_with(|| left_id.cmp(right_id))
                })
                .map(|(id, _)| id.clone())
                .expect("bounded feedback map has a touch record");
            self.feedback.remove(&victim);
            self.feedback_touch.remove(&victim);
            self.feedback_evicted = self.feedback_evicted.saturating_add(1);

            let mut empty_groups = Vec::new();
            for (group, recent) in &mut self.recent_by_group {
                recent.retain(|id| id != &victim);
                if recent.is_empty() {
                    empty_groups.push(group.clone());
                }
            }
            for group in empty_groups {
                self.recent_by_group.remove(&group);
                self.recent_group_touch.remove(&group);
                self.recent_groups_evicted = self.recent_groups_evicted.saturating_add(1);
            }
        }

        self.feedback.entry(asset_id.to_owned()).or_default();
        self.feedback_touch.insert(asset_id.to_owned(), tick);
        self.feedback_high_water = self.feedback_high_water.max(self.feedback.len());
    }

    fn ensure_recent_group(&mut self, group: &str) {
        let tick = self.next_touch_tick();
        if !self.recent_by_group.contains_key(group)
            && self.recent_by_group.len() >= self.retention.max_recent_groups
        {
            let victim = self
                .recent_group_touch
                .iter()
                .min_by(|(left_id, left_tick), (right_id, right_tick)| {
                    left_tick
                        .cmp(right_tick)
                        .then_with(|| left_id.cmp(right_id))
                })
                .map(|(id, _)| id.clone())
                .expect("bounded recent-group map has a touch record");
            self.recent_by_group.remove(&victim);
            self.recent_group_touch.remove(&victim);
            self.recent_groups_evicted = self.recent_groups_evicted.saturating_add(1);
        }

        self.recent_by_group.entry(group.to_owned()).or_default();
        self.recent_group_touch.insert(group.to_owned(), tick);
        self.recent_groups_high_water = self
            .recent_groups_high_water
            .max(self.recent_by_group.len());
    }

    fn forget_asset(&mut self, asset_id: &str) {
        self.feedback.remove(asset_id);
        self.feedback_touch.remove(asset_id);
        let mut empty_groups = Vec::new();
        for (group, recent) in &mut self.recent_by_group {
            recent.retain(|id| id != asset_id);
            if recent.is_empty() {
                empty_groups.push(group.clone());
            }
        }
        for group in empty_groups {
            self.recent_by_group.remove(&group);
            self.recent_group_touch.remove(&group);
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
        if self.decisions.len() > self.retention.max_decisions {
            let remove = self.decisions.len() - self.retention.max_decisions;
            self.decisions.drain(..remove);
            self.decisions_evicted = self.decisions_evicted.saturating_add(remove as u64);
        }
        self.decisions_high_water = self.decisions_high_water.max(self.decisions.len());
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
                self.forget_asset(asset_id);
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

    fn test_pseudonymizer() -> ActorPseudonymizer {
        ActorPseudonymizer::new("test-v1", [0x42; 32]).expect("test pseudonymizer")
    }

    fn test_memory(config: WorkingMemoryConfig) -> WorkingMemory {
        WorkingMemory::new(config, test_pseudonymizer()).expect("working memory")
    }

    #[test]
    fn link_bounds_must_be_positive() {
        let error = WorkingMemory::new(
            WorkingMemoryConfig {
                max_links: 0,
                ..WorkingMemoryConfig::default()
            },
            test_pseudonymizer(),
        )
        .expect_err("a zero link cap is no bound at all");
        assert!(matches!(error, AdaptationError::InvalidConfiguration(_)));
        let error = WorkingMemory::new(
            WorkingMemoryConfig {
                max_links_per_memory: 0,
                ..WorkingMemoryConfig::default()
            },
            test_pseudonymizer(),
        )
        .expect_err("a zero per-write budget is no bound at all");
        assert!(matches!(error, AdaptationError::InvalidConfiguration(_)));
    }

    #[test]
    fn keyed_actor_pseudonyms_are_deterministic_domain_separated_and_full_length() {
        let pseudonymizer = ActorPseudonymizer::new("v7", [0x11; 32]).expect("pseudonymizer");
        let first = pseudonymizer.pseudonymize("youtube.chat", "viewer-123");
        let repeated = pseudonymizer.pseudonymize("youtube.chat", "viewer-123");
        let other_source = pseudonymizer.pseudonymize("twitch.chat", "viewer-123");

        assert_eq!(first, repeated);
        assert_ne!(first, other_source);
        assert!(first.starts_with("actor:v7:"));
        assert_eq!(first.len(), "actor:v7:".len() + 64);
    }

    #[test]
    fn pseudonym_key_and_version_rotation_break_historical_linkability() {
        let v1 = ActorPseudonymizer::new("v1", [0x21; 32]).expect("v1");
        let rotated_key = ActorPseudonymizer::new("v1", [0x22; 32]).expect("rotated key");
        let v2 = ActorPseudonymizer::new("v2", [0x21; 32]).expect("v2");
        let baseline = v1.pseudonymize("public-chat", "viewer-a");

        assert_ne!(
            baseline,
            rotated_key.pseudonymize("public-chat", "viewer-a")
        );
        assert_ne!(baseline, v2.pseudonymize("public-chat", "viewer-a"));
        assert!(
            v2.pseudonymize("public-chat", "viewer-a")
                .starts_with("actor:v2:")
        );
    }

    #[test]
    fn pseudonym_key_is_required_nonzero_and_redacted_from_debug() {
        assert!(ActorPseudonymizer::new("v1", [0; 32]).is_err());
        let pseudonymizer = ActorPseudonymizer::new("v1", [0xab; 32]).expect("pseudonymizer");
        let debug = format!("{pseudonymizer:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("abababab"));
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
    fn retained_adaptation_state_plateaus_under_logical_churn() {
        let mut memory = test_memory(WorkingMemoryConfig {
            max_entries: 2,
            working_ttl_ms: 10_000,
            durable_ttl_ms: 10_000,
            max_compaction_records: 3,
            ..WorkingMemoryConfig::default()
        });
        let chat = event(
            "evt-retention",
            SourceClass::PublicChat,
            TrustLevel::Untrusted,
            Some("viewer:retention"),
        );
        for index in 0_u64..20 {
            memory
                .remember_working(&chat, &format!("claim-{index}"), Some("retention"), index)
                .expect("remember churn");
        }
        let memory_metrics = memory.retention_metrics();
        assert_eq!(memory_metrics.entries, 2);
        assert_eq!(memory_metrics.entries_high_water, 2);
        assert_eq!(memory_metrics.compactions, 3);
        assert_eq!(memory_metrics.compactions_high_water, 3);
        assert!(memory_metrics.compactions_evicted > 0);

        let mut engine = AdaptationEngine::with_retention(
            PromotionPolicy::default(),
            "bounded-retention-v1",
            42,
            AdaptationRetentionConfig {
                max_feedback_assets: 3,
                max_recent_groups: 2,
                max_decisions: 2,
            },
        )
        .expect("bounded adaptation engine");
        for index in 0_u64..10 {
            let mut asset = generated_fixture();
            asset.id = format!("dynamic.retention.{index}");
            asset.intent = format!("retention.intent.{index}");
            asset.variant_group = Some(format!("retention.group.{index}"));
            engine.record_use(&asset);
            engine.record_quality(&asset.id, true);
            engine.evaluate(&asset).expect("evaluate churn asset");
        }

        let metrics = engine.retention_metrics();
        assert_eq!(metrics.feedback_assets, 3);
        assert_eq!(metrics.feedback_assets_high_water, 3);
        assert!(metrics.feedback_assets_evicted > 0);
        assert_eq!(metrics.recent_groups, 2);
        assert_eq!(metrics.recent_groups_high_water, 2);
        assert!(metrics.recent_groups_evicted > 0);
        assert_eq!(metrics.decisions, 2);
        assert_eq!(metrics.decisions_high_water, 2);
        assert_eq!(metrics.decisions_evicted, 8);
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

        let mut memory = test_memory(WorkingMemoryConfig {
            max_entries: 2,
            working_ttl_ms: 100,
            durable_ttl_ms: 1_000,
            ..WorkingMemoryConfig::default()
        });
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
        assert!(
            stored
                .source
                .pseudonymous_actor_id
                .as_deref()
                .is_some_and(|value| value.starts_with("actor:test-v1:"))
        );
        let serialized = serde_json::to_string(stored).expect("serialize memory entry");
        assert!(!serialized.contains("viewer-real-id"));
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

        let mut memory = test_memory(WorkingMemoryConfig {
            max_entries: 4,
            working_ttl_ms: 100,
            durable_ttl_ms: 1_000,
            ..WorkingMemoryConfig::default()
        });
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
        let mut first = test_memory(config);
        let mut second = test_memory(config);

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
        let mut memory = test_memory(WorkingMemoryConfig {
            ..WorkingMemoryConfig::default()
        });
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
                source_namespace: Some("public-chat"),
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

    // Issue #146: the adaptation-side byte-cap truncation applied to memory
    // claims and topics, which come from untrusted input. This is a separate
    // implementation from `crates/app/src/routing.rs::truncate_utf8` (it
    // clamps with `.min(value.len())` and guards `end > 0`), so it gets its own
    // property coverage rather than assuming the other one behaves the same.
    #[test]
    fn truncate_utf8_respects_the_byte_cap_across_unicode_widths() {
        for value in [
            "abcdef",
            "aあいう",
            "a\u{0301}bc",
            "a🎉🎊bc",
            "a\u{10FFFF}b",
        ] {
            for cap in 0..=(value.len() + 3) {
                let truncated = truncate_utf8(value, cap);
                assert!(
                    truncated.len() <= cap,
                    "cap {cap} produced {} bytes from a {}-byte input",
                    truncated.len(),
                    value.len()
                );
                assert!(
                    std::str::from_utf8(truncated.as_bytes()).is_ok(),
                    "cap {cap} produced invalid UTF-8 for {value:?}"
                );
                assert!(
                    value.starts_with(&truncated),
                    "cap {cap} returned {truncated:?}, which is not a prefix of {value:?}"
                );
            }
        }
    }

    #[test]
    fn truncate_utf8_returns_short_inputs_verbatim() {
        for value in ["", "a", "hello", "こんにちは", "🎉"] {
            // Only caps that admit the whole input; a cap below the byte length
            // must still truncate, including cap 0.
            for cap in value.len()..=(value.len() + 2) {
                assert_eq!(truncate_utf8(value, cap), value, "cap {cap} for {value:?}");
            }
        }
    }

    /// Keeps the longest valid prefix rather than merely *a* valid prefix, and
    /// pins the `cap == 0` case that the `end > 0` guard exists to handle.
    #[test]
    fn truncate_utf8_keeps_the_longest_valid_prefix() {
        let value = "aあい";
        assert_eq!(value.len(), 7);
        assert_eq!(truncate_utf8(value, 0), "");
        assert_eq!(truncate_utf8(value, 1), "a");
        assert_eq!(truncate_utf8(value, 2), "a");
        assert_eq!(truncate_utf8(value, 3), "a");
        assert_eq!(truncate_utf8(value, 4), "aあ");
        assert_eq!(truncate_utf8(value, 5), "aあ");
        assert_eq!(truncate_utf8(value, 6), "aあ");
        assert_eq!(truncate_utf8(value, 7), value);
        assert_eq!(truncate_utf8(value, 99), value);
    }
}
