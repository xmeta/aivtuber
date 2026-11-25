#![forbid(unsafe_code)]

//! Deterministic bounded memory and generated-asset adaptation policy.
//!
//! This crate owns policy/state only. SecurityRuntime owns memory-write authority,
//! while AssetStore owns validated persistence primitives.

mod erasure;
pub use erasure::*;

use aivtuber_asset_store::{AssetStore, AssetStoreError, PerformanceAsset};
use aivtuber_domain::{EventEnvelope, SourceClass, TrustLevel};
use aivtuber_runtime::{MemoryWriteDecision, MemoryWritePermit};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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

pub use aivtuber_domain::EpistemicClass as MemoryEpistemicClass;

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
    /// Epistemic class derived once at write time from trusted source metadata
    /// and whether the claim is the source event's own claim text (#105 §3.1).
    /// Stored, never recomputed: retrieval, ranking, and link structure cannot
    /// change a claim's class.
    pub epistemic_class: MemoryEpistemicClass,
    /// Epistemic vocabulary this entry's class was derived under, stored with
    /// the entry rather than stamped from the current process constant, so a
    /// replay can tell a class derived under an older vocabulary from one
    /// derived under the present one.
    pub epistemic_version: String,
    /// Internal node identity, assigned when the entry is inserted. Links
    /// reference this and never `source.event_id`: a source event id is not
    /// unique among retained entries (the scheduler may legitimately replay
    /// one) and is not durable identity, so it cannot name a graph node.
    /// `0` is never assigned; it marks an entry that has not been stored yet.
    pub memory_id: u64,
}

/// Versioned edge vocabulary for retained episodic memory (#105). Kept small
/// on purpose: temporal order is derived deterministically at write time, and
/// supersession and causation are declared by the caller that owns the
/// evidence — a model may recommend either, but only the permit-gated durable
/// path can turn one into an edge.
///
/// `memory-link-v2` adds the causal kind. The version is pinned so the
/// change is replay-visible: an artifact written under v1 simply carries no
/// causal edges, and a reader never has to infer the vocabulary from which
/// kinds happen to appear.
pub const MEMORY_LINK_VOCAB_VERSION: &str = "memory-link-v2";

/// Versioned epistemic vocabulary for retained memory (#105 §3.1). The
/// retrieval view names it, so a replay can identify which classification
/// shaped a record instead of inferring it from field presence.
pub const MEMORY_EPISTEMIC_VOCAB_VERSION: &str = "memory-epistemic-v1";

/// How many raw supersession declarations one write may hand
/// `remember_durable_superseding`, as a multiple of
/// `max_links_per_memory`. Duplicates count once, but the pass that
/// discovers that stays bounded too: a restate may buy this factor of
/// slack over the edge budget, never an unbounded scan or allocation over
/// an arbitrary slice (#105 review round 6).
const SUPERSESSION_DECLARATION_FACTOR: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLinkKind {
    /// `from` was recorded before `to` within one topic — derived at write
    /// time from the retained timeline, never from model output.
    TemporalBefore,
    /// `from` explicitly supersedes `to` — declared at write time through the
    /// same permit path as the claim itself.
    Supersedes,
    /// `from` was caused by `to` — declared at write time through the same
    /// permit path as the claim itself.
    ///
    /// Only one direction is stored. The other reading of the same evidence —
    /// "`to` resulted in `from`" — is this edge seen from its other endpoint,
    /// so a second kind would spend two units of the per-write budget on one
    /// fact and could disagree with the first after partial eviction.
    CausedBy,
}

/// One edge between retained memory nodes. It carries no claim content —
/// only node ids ([`MemoryEntry::memory_id`]), a kind, and record time — so
/// expiry or deletion can never leave removed content reachable through link
/// metadata (security threat model §8: an edge must not become a covert
/// archive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryLink {
    pub from: u64,
    pub to: u64,
    pub kind: MemoryLinkKind,
    pub created_at_ms: u64,
}

/// One retrieved memory together with its bounded link view. This is the
/// retrieval surface the #105 long-range evaluation measures: `stale` marks a
/// claim a retained memory explicitly supersedes, `links` is capped at the
/// per-memory budget so a neighborhood can never fan out unboundedly, and
/// nodes one link away are resolved into the result within the query limit so
/// a supersession that changed topic or actor is still consumable.
#[derive(Debug, Clone, Serialize)]
pub struct LinkedMemory<'a> {
    pub entry: &'a MemoryEntry,
    pub stale: bool,
    pub links: Vec<MemoryLink>,
    /// Edge vocabulary these links were produced under, serialized with the
    /// view: a replay has to be able to identify which vocabulary shaped a
    /// record instead of inferring it from field presence.
    pub vocab_version: &'static str,
    /// Epistemic vocabulary the carried entry's class was derived under. Read
    /// from the entry itself, not stamped from the current process constant, so
    /// the view reports the version the class was actually derived under
    /// (#105 §3.1).
    pub epistemic_version: &'a str,
}

/// Serialized envelope for the retained edge set. The vocabulary version
/// travels inside the artifact that carries the edges, so an older snapshot
/// can be identified — and refused — rather than reinterpreted under a newer
/// vocabulary.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryLinkSnapshot {
    pub vocab_version: &'static str,
    pub links: Vec<MemoryLink>,
}

/// The caller-owned evidence a durable write may carry (#105).
///
/// Both lists name retained *source event ids*; each is resolved to exactly
/// one live node before anything is stored, and both ride the same
/// non-serializable permit path as the claim, so evidence can never widen the
/// memory-write gate. They also share one per-write budget: the edge bound is
/// a cost bound, and splitting the same number of edges across two lists must
/// not spend it twice.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryEvidence<'a> {
    /// Retained claims this write replaces — the update evidence a caller owns.
    pub supersedes: &'a [&'a str],
    /// Retained claims that brought this one about. What this claim *resulted
    /// in* is the same edge read from the other endpoint, so no inverse list
    /// exists.
    pub caused_by: &'a [&'a str],
}

/// The declared relation a target resolution is serving.
///
/// It exists only so a refusal names which declaration was rejected: both
/// kinds share every rule and the same per-write budget, so the resolution
/// logic is one function, not two copies that could drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclaredRelation {
    Supersession,
    Causal,
}

impl DeclaredRelation {
    fn missing(self) -> &'static str {
        match self {
            Self::Supersession => "supersession target must be a retained durable memory",
            Self::Causal => "causal target must be a retained durable memory",
        }
    }

    fn expired(self) -> &'static str {
        match self {
            Self::Supersession => {
                "supersession target has expired; an expired memory cannot authorize an update \
                 relation"
            }
            Self::Causal => {
                "causal target has expired; an expired memory cannot authorize a causal relation"
            }
        }
    }

    fn ambiguous(self) -> &'static str {
        match self {
            Self::Supersession => {
                "supersession target is ambiguous: one source event id matches more than one \
                 retained durable memory, and links must not guess which node the caller meant"
            }
            Self::Causal => {
                "causal target is ambiguous: one source event id matches more than one retained \
                 durable memory, and links must not guess which node the caller meant"
            }
        }
    }

    fn recorded_in_the_future(self) -> &'static str {
        match self {
            Self::Supersession => {
                "supersession target was recorded after the superseding write; a claim cannot \
                 replace evidence from its own future"
            }
            Self::Causal => {
                "causal target was recorded after the write that declares it; a claim cannot be \
                 caused by evidence from its own future"
            }
        }
    }
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

/// Whether claims of this source class may be handed to generation as shared
/// context, independent of who is asking.
///
/// The rule lives here, not in the caller, because more than one retrieval
/// path has to apply it: the store's own [`WorkingMemory::context_for`], and
/// the long-range evaluation, which applies the same rule to what an
/// external memory service answers (#105). A second copy of the rule would be
/// a second contract.
///
/// * `System`, `Game`, `Stream`, and `Timer` observations and the
///   performer's own `Speech` are the stream's knowledge, attributable to the
///   runtime rather than to a person;
/// * `PublicChat` and `Donation` are somebody's words, so they are in scope
///   only for that same actor;
/// * `Operator` is the operator's private record. A privacy *label* is not an
///   egress authorization, so it never enters generation; a deliberate
///   declassification policy would have to authorize it first.
pub fn source_class_is_shared(source_class: SourceClass) -> bool {
    match source_class {
        SourceClass::System
        | SourceClass::Speech
        | SourceClass::Game
        | SourceClass::Stream
        | SourceClass::Timer => true,
        SourceClass::PublicChat | SourceClass::Donation | SourceClass::Operator => false,
    }
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
    /// `max_links`. `max_links >= max_links_per_memory` is required, so the
    /// store bound can never drop an explicit supersession the very same
    /// write just had accepted.
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
    /// Completed erase transactions, and what they removed (#305).
    pub erasures_completed: u64,
    pub erasure_entries_removed: u64,
    pub erasure_links_removed: u64,
    /// Open transactions: scopes revoked but not yet reported. Non-zero at
    /// rest means an erase did not finish.
    pub erasures_pending: usize,
    /// Viewer erase requests refused by the per-pseudonym rate limit.
    pub erasures_rate_limited: u64,
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
    /// Next node identity to hand out; starts at 1 so `0` stays the marker
    /// for "not inserted yet".
    next_memory_id: u64,
    /// Targeted-erase transactions, reports and rate limiter (#305).
    erasure: erasure::ErasureBook,
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
        if config.max_links < config.max_links_per_memory {
            return Err(AdaptationError::InvalidConfiguration(
                "max_links must cover one maximum-size write (max_links >= max_links_per_memory); \
                 otherwise the store bound drops explicit supersessions that an accepted write \
                 just declared",
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
            next_memory_id: 1,
            erasure: erasure::ErasureBook::default(),
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

    /// The versioned form of the retained edge set visible at `now_ms`.
    /// Anything that persists or replays links serializes this envelope
    /// rather than the bare list, so [`MEMORY_LINK_VOCAB_VERSION`] can never
    /// be separated from the data it versions. Only edges recorded at or
    /// before the read timestamp whose both endpoints are still visible
    /// there are retained: an edge must not outlive a removed memory as a
    /// covert archive of its content (docs/security-threat-model.adoc §8;
    /// #105 review round 11).
    pub fn link_snapshot(&self, now_ms: u64) -> MemoryLinkSnapshot {
        let visible: HashSet<u64> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.expires_at_ms > now_ms
                    && entry.created_at_ms <= now_ms
                    && !self.is_revoked(entry)
            })
            .map(|entry| entry.memory_id)
            .collect();
        MemoryLinkSnapshot {
            vocab_version: MEMORY_LINK_VOCAB_VERSION,
            links: self
                .links
                .iter()
                .filter(|link| {
                    link.created_at_ms <= now_ms
                        && visible.contains(&link.from)
                        && visible.contains(&link.to)
                })
                .cloned()
                .collect(),
        }
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
            erasures_completed: self.erasure.completed,
            erasure_entries_removed: self.erasure.entries_removed,
            erasure_links_removed: self.erasure.links_removed,
            erasures_pending: self.erasure.pending_len(),
            erasures_rate_limited: self.erasure.rate_limited,
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

    /// A durable write that declares supersession only (#105).
    ///
    /// This is [`Self::remember_durable_with_evidence`] with no causal
    /// evidence — the narrower entry point call sites that only update a claim
    /// already use.
    pub fn remember_durable_superseding(
        &mut self,
        permit: &MemoryWritePermit,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[&str],
    ) -> Result<&MemoryEntry, AdaptationError> {
        self.remember_durable_with_evidence(
            permit,
            claim,
            topic,
            now_ms,
            MemoryEvidence {
                supersedes,
                caused_by: &[],
            },
        )
    }

    /// A durable write that may also declare supersession: `supersedes` names
    /// retained durable memories this claim replaces (issue #105's update
    /// evidence, owned by the caller) and `caused_by` names retained durable
    /// memories that brought it about. Targets are resolved to exactly one
    /// live retained durable node *before* anything is stored — unknown,
    /// working-only, expired, future-dated, or ambiguous targets refuse the
    /// whole write ("stores nothing, mints nothing"), links ride the same
    /// non-serializable permit path as the claim, and
    /// `max_links_per_memory` bounds what one write may create. Repeating one
    /// target declares it once: the budget counts edges the write creates,
    /// not the strings it was handed, so duplicates can neither blow the
    /// budget nor consume it. The raw declaration list itself is bounded by
    /// a fixed multiple of that budget (and the dedup buffer never grows
    /// past the budget), so a flood of restated ids cannot buy an unbounded
    /// scan on a bounded-adaptation API.
    pub fn remember_durable_with_evidence(
        &mut self,
        permit: &MemoryWritePermit,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        evidence: MemoryEvidence<'_>,
    ) -> Result<&MemoryEntry, AdaptationError> {
        // Each list is bounded before it is walked, so a flood of restated ids
        // cannot buy an unbounded scan on a bounded-adaptation API.
        let declarations_bound = self
            .config
            .max_links_per_memory
            .saturating_mul(SUPERSESSION_DECLARATION_FACTOR);
        if evidence.supersedes.len() > declarations_bound
            || evidence.caused_by.len() > declarations_bound
        {
            return Err(AdaptationError::InvalidInput(
                "one write may not hand more explicit relation declarations than the bounded \
                 input allows; collapse repeated declarations before calling",
            ));
        }
        // Both kinds draw on one budget: the edge bound is a cost bound, and
        // splitting the same number of edges across two lists must not spend
        // it twice. Refusing at cap+1 means the dedup buffers never grow past
        // the budget either.
        let mut budget = self.config.max_links_per_memory;
        let mut declared_supersedes: Vec<&str> =
            Vec::with_capacity(budget.min(evidence.supersedes.len()));
        for target in evidence.supersedes {
            if !declared_supersedes.contains(target) {
                if budget == 0 {
                    return Err(AdaptationError::InvalidInput(
                        "one write may not declare more explicit relations than the per-memory \
                         link budget",
                    ));
                }
                declared_supersedes.push(target);
                budget -= 1;
            }
        }
        let mut declared_causes: Vec<&str> =
            Vec::with_capacity(budget.min(evidence.caused_by.len()));
        for target in evidence.caused_by {
            if !declared_causes.contains(target) {
                if budget == 0 {
                    return Err(AdaptationError::InvalidInput(
                        "one write may not declare more explicit relations than the per-memory \
                         link budget",
                    ));
                }
                declared_causes.push(target);
                budget -= 1;
            }
        }
        let mut targets = Vec::with_capacity(declared_supersedes.len());
        for target in &declared_supersedes {
            targets.push(self.resolve_declared_target(
                target,
                now_ms,
                DeclaredRelation::Supersession,
            )?);
        }
        let mut cause_nodes = Vec::with_capacity(declared_causes.len());
        for target in &declared_causes {
            cause_nodes.push(self.resolve_declared_target(
                target,
                now_ms,
                DeclaredRelation::Causal,
            )?);
        }
        // Explicit evidence is never evicted to make room for more of it:
        // shedding the edge that marks a still-retained target makes that
        // claim current again while the memory that replaced it is alive —
        // the stale fact this layer exists to keep (#105) — and shedding a
        // causal edge loses the caller's account of why a claim exists. The
        // bound therefore refuses the *later* write instead of forgetting
        // the earlier evidence, and it refuses it before anything is stored
        // ("stores nothing, mints nothing"), so the caller learns the
        // evidence was not accepted rather than discovering later that a
        // claim resurrected. Derived temporal edges are shed to make room
        // instead — there are always enough of them — so only a write that
        // adds explicit evidence can be refused here. The fit is judged
        // over what this write would *leave retained*: the edges already
        // stored are projected through this write's own compaction first
        // (expiry trim, then the capacity pop), and each new edge counts
        // only if its target survives that same projection — an endpoint
        // already expired at `now_ms`, the node this insertion
        // capacity-evicts, or a fresh target that is itself that evicted
        // node, is dead evidence the write removes anyway rather than a
        // claim held against a still-retained node (round 3: valid updates
        // must not wait for an unrelated write to free logically dead
        // capacity; round 4: the evicted *target* of a new edge must not
        // consume capacity the store will never spend).
        let survivors = self.projected_survivors(now_ms);
        let explicit_retained = self
            .links
            .iter()
            .filter(|link| {
                matches!(
                    link.kind,
                    MemoryLinkKind::Supersedes | MemoryLinkKind::CausedBy
                ) && survivors.contains(&link.from)
                    && survivors.contains(&link.to)
            })
            .count();
        let new_evidence_retained = targets
            .iter()
            .chain(cause_nodes.iter())
            .filter(|id| survivors.contains(id))
            .count();
        if explicit_retained + new_evidence_retained > self.config.max_links {
            return Err(AdaptationError::InvalidInput(
                "the link store cannot hold this write's explicit evidence without discarding \
                 earlier evidence for a still-retained claim; raise max_links or let retention \
                 expire the earlier endpoints first",
            ));
        }
        let retention = RetentionClass::Durable;
        // Verification binds to the claim, not the permit (#105 review round
        // 2): the retained text must be the source event's own claim, or a
        // trusted permit could launder an unrelated sentence into a verified
        // fact. The binding is decided on the *full* text before storage
        // truncation, and on the normalized form of both sides, so appended
        // content cannot hide behind a truncated prefix and the event's own
        // claim is not downgraded just because normalization rewrote its case
        // or whitespace (#105 review round 3).
        let claim_is_bound = permit
            .source_claim()
            .is_some_and(|source_claim| normalize(source_claim) == normalize(claim));
        // A verified fact must be stored complete or not at all (#105 review
        // round 4): truncating it at the byte bound can drop the negation or
        // qualifier that bounded it, so the store would assert a claim whose
        // meaning changed. Weaker classes keep truncation because they assert
        // nothing.
        require_complete_verified_claim(
            claim,
            self.config.max_claim_bytes,
            MemoryEpistemicClass::derive(
                permit.source_class(),
                permit.trust_level(),
                claim_is_bound,
            ),
        )?;
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
            epistemic_class: MemoryEpistemicClass::derive(
                permit.source_class(),
                permit.trust_level(),
                claim_is_bound,
            ),
            epistemic_version: MEMORY_EPISTEMIC_VOCAB_VERSION.to_owned(),
            memory_id: 0,
        };
        let topic = entry.topic.clone();
        let stored = self.push_bounded(entry, now_ms)?;
        let new_id = stored.memory_id;
        self.link_new_memory(new_id, topic.as_deref(), now_ms, &targets, &cause_nodes);
        self.entries.back().ok_or(AdaptationError::Invariant(
            "inserted memory entry disappeared",
        ))
    }

    /// Resolve a declared relation target to exactly one node, failing closed
    /// at every point identity or liveness is in doubt:
    ///
    /// * unknown or working-only ids are refused — no cross-retention edge;
    /// * an id whose only durable matches are expired *at `now_ms`* is
    ///   refused — an expired memory is already deleted for retrieval, so it
    ///   cannot authorize a relation just because compaction has not
    ///   physically removed it yet;
    /// * an id retained as more than one live durable memory is refused as
    ///   ambiguous — a source event id is not a node identity, and the link
    ///   must not be guessed onto one of several candidates;
    /// * a target recorded *after* the write is refused — a claim cannot
    ///   replace or be caused by evidence from its own future, or a backfill
    ///   would contradict the record-time ordering the temporal spine and
    ///   retrieval ranking both follow.
    ///
    /// `relation` only names the kind in the refusal, so an operator learns
    /// which declaration was rejected; both kinds share the rules.
    fn resolve_declared_target(
        &self,
        id: &str,
        now_ms: u64,
        relation: DeclaredRelation,
    ) -> Result<u64, AdaptationError> {
        let retained: Vec<&MemoryEntry> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.source.event_id == id
                    && entry.retention == RetentionClass::Durable
                    && !self.is_revoked(entry)
            })
            .collect();
        if retained.is_empty() {
            return Err(AdaptationError::InvalidInput(relation.missing()));
        }
        let mut live = retained.iter().filter(|entry| entry.expires_at_ms > now_ms);
        let Some(target) = live.next() else {
            return Err(AdaptationError::InvalidInput(relation.expired()));
        };
        if live.next().is_some() {
            return Err(AdaptationError::InvalidInput(relation.ambiguous()));
        }
        if target.created_at_ms > now_ms {
            return Err(AdaptationError::InvalidInput(
                relation.recorded_in_the_future(),
            ));
        }
        Ok(target.memory_id)
    }

    /// Create this write's links, deterministic by construction: explicit
    /// evidence first (supersession, then causation — both caller-owned), then
    /// a temporal spine to the most recent same-topic durable memories, all
    /// within the one per-memory budget. Same claims written twice dedupe to
    /// one edge, and the store total is re-bounded afterwards.
    fn link_new_memory(
        &mut self,
        new_id: u64,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[u64],
        caused_by: &[u64],
    ) {
        let mut budget = self.config.max_links_per_memory;
        for (target, kind) in supersedes
            .iter()
            .map(|target| (target, MemoryLinkKind::Supersedes))
            .chain(
                caused_by
                    .iter()
                    .map(|target| (target, MemoryLinkKind::CausedBy)),
            )
        {
            if budget == 0 {
                break;
            }
            self.push_link(MemoryLink {
                from: new_id,
                to: *target,
                kind,
                created_at_ms: now_ms,
            });
            budget -= 1;
        }
        if let Some(topic) = topic {
            let normalized = normalize(topic);
            // Ordered by logical record time, never by insertion order: a
            // backfilled write may carry an earlier `created_at_ms` than one
            // already stored, and a `TemporalBefore` edge must not contradict
            // the record times `relevant` ranks by. A candidate recorded
            // *after* this write is not its predecessor at all and is
            // skipped; among the rest the newest record leads, with node id
            // breaking ties so two same-tick writes keep insertion order and
            // the budget still cuts deterministically.
            let mut older: Vec<(u64, u64)> = self
                .entries
                .iter()
                .filter(|entry| {
                    entry.retention == RetentionClass::Durable
                        && entry.memory_id != new_id
                        && entry.topic.as_deref().map(normalize).as_deref()
                            == Some(normalized.as_str())
                        && entry.created_at_ms <= now_ms
                })
                .map(|entry| (entry.created_at_ms, entry.memory_id))
                .collect();
            older.sort_by(|left, right| right.cmp(left));
            older.truncate(budget);
            for (_, id) in older {
                self.push_link(MemoryLink {
                    from: id,
                    to: new_id,
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
        let alive: HashSet<u64> = self.entries.iter().map(|entry| entry.memory_id).collect();
        self.links
            .retain(|link| alive.contains(&link.from) && alive.contains(&link.to));
    }

    /// The node ids that would still be retained after this write's own
    /// insertion compaction has run, without mutating the store: mirrors
    /// `push_bounded`'s two `compact` passes (expiry trim, push at the back,
    /// pop from the front while over the node bound). Stored edges and new
    /// explicit targets are both judged against this set, so evidence this
    /// write itself removes never counts against this write. The
    /// inserted node is not in the set yet; it always survives because the
    /// pops come from the front and `max_entries >= 1`.
    fn projected_survivors(&self, now_ms: u64) -> HashSet<u64> {
        let mut survivors: Vec<u64> = self
            .entries
            .iter()
            .filter(|entry| entry.expires_at_ms > now_ms)
            .map(|entry| entry.memory_id)
            .collect();
        let excess = survivors.len().saturating_sub(self.config.max_entries);
        survivors.drain(..excess);
        let excess = survivors
            .len()
            .saturating_add(1)
            .saturating_sub(self.config.max_entries);
        survivors.drain(..excess);
        survivors.into_iter().collect()
    }

    fn push_link(&mut self, link: MemoryLink) {
        let duplicate = self.links.iter().any(|existing| {
            existing.kind == link.kind && existing.from == link.from && existing.to == link.to
        });
        if !duplicate {
            self.links.push_back(link);
        }
    }

    /// Re-enforce the store-wide edge bound by shedding **derived** edges
    /// only, oldest first. Explicit evidence — supersession and causation — is
    /// never removed here: [`Self::remember_durable_with_evidence`] refuses any
    /// write whose explicit edges would not fit alongside the ones already
    /// retained, so once the derived edges are gone the store is provably
    /// within its bound and this loop exits — an explicit-only store cannot be
    /// over the limit. Shedding an explicit supersession edge instead would
    /// make the claim it marks current again while the memory that replaced it
    /// is still live, which is the stale fact #105 exists to keep; shedding a
    /// causal edge would lose the caller's account of why a claim exists, and
    /// neither is re-derivable from record time. The bound therefore yields to
    /// that correctness rule rather than forgetting evidence.
    fn enforce_link_bound(&mut self) {
        while self.links.len() > self.config.max_links {
            let Some(derived) = self
                .links
                .iter()
                .position(|link| link.kind == MemoryLinkKind::TemporalBefore)
            else {
                // Unreachable: the pre-store explicit-evidence check
                // guarantees supersession and causal edges alone fit, so
                // there is always a derived edge left to shed before the
                // bound can be exceeded.
                break;
            };
            self.links.remove(derived);
        }
        debug_assert!(
            self.links.len() <= self.config.max_links,
            "an explicit-only link store exceeded its bound; the pre-store explicit-evidence check \
             must prevent this"
        );
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

    /// Ranked matches visible at `now_ms`: an entry must have been recorded
    /// at or before the query time and not yet expired — `created_at_ms <=
    /// now_ms < expires_at_ms` — so replay-time retrieval can never read
    /// evidence from the future (#105 review round 10).
    pub fn relevant(&self, query: MemoryQuery<'_>, now_ms: u64) -> Vec<&MemoryEntry> {
        if query.limit == 0 {
            return Vec::new();
        }
        // A query that names an actor but not the namespace its pseudonym is
        // hashed in carries an explicit filter that cannot be evaluated: the
        // `zip` below would silently drop it and return every actor's
        // memory. Fail closed with no matches instead of widening the scope
        // (#105 review round 11).
        if query.actor_id.is_some() && query.source_namespace.is_none() {
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
            .filter(|entry| {
                entry.expires_at_ms > now_ms
                    && entry.created_at_ms <= now_ms
                    && !self.is_revoked(entry)
            })
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

        // Equal record times break by insertion order (`memory_id`, newest
        // first): that is the same timeline `TemporalBefore` derives its
        // edges from, so two replay-visible views can never disagree on
        // which same-tick write came later — an arbitrary event id could
        // (#105 review round 11).
        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| right.2.created_at_ms.cmp(&left.2.created_at_ms))
                .then_with(|| right.2.memory_id.cmp(&left.2.memory_id))
        });
        ranked
            .into_iter()
            .take(query.limit)
            .map(|(_, _, entry)| entry)
            .collect()
    }

    /// The same ranking as [`Self::relevant`], each result joined with its
    /// bounded link view, and — filling whatever the query limit still allows
    /// — with the live nodes one link away from those results. Direct matches
    /// and the expansion both keep the query's **actor** scope: a direct
    /// match can arrive through the topic alone when the query supplies an
    /// actor *and* a topic, so the scope is reapplied to every result, not
    /// only the hop. Visibility is time-indexed throughout: nodes and edges
    /// recorded after `now_ms` do not exist for the query, so direct
    /// matches, the staleness verdict, the link view, and the hop all stay
    /// inside the read timestamp and a replay can never act on evidence
    /// from its own future (#105 review round 10). `stale` is a
    /// verdict over the **full** retained edge set visible at `now_ms`, never over the capped
    /// presentation view: a supersession that fell past
    /// `max_links_per_memory` still marks its target stale, and the
    /// superseding node must itself still be live at `now_ms` — an expired
    /// but not yet compacted memory is already deleted for retrieval, so it
    /// cannot keep a claim marked. A view is only shown while *both* endpoints
    /// are live, so an expired node leaves no dangling relation behind, and
    /// the expansion resolves those same endpoints into results: a
    /// supersession that changed topic brings the claim it replaces back with
    /// it, without the caller scanning the store to find it. The hop keeps
    /// that same actor scope — the derived temporal spine connects every
    /// durable entry that shares a topic, so without that scope an
    /// actor-only query would pull another viewer's claim into its context —
    /// while topic is deliberately not required, because following a claim to
    /// what replaced it is the point of the hop; verdicts (`stale`) and edge
    /// metadata stay complete either way, since an edge carries node ids and
    /// never content. The whole result set stays within `query.limit`,
    /// expansion never displaces a direct match, and both the view and the
    /// expansion are deterministic (supersessions before temporal edges,
    /// creation order within a kind), so a neighborhood can never fan out.
    pub fn relevant_linked(&self, query: MemoryQuery<'_>, now_ms: u64) -> Vec<LinkedMemory<'_>> {
        // Fail closed on an actor filter that cannot be evaluated — same
        // rule as `relevant`: without the namespace the pseudonym cannot be
        // computed, and dropping the filter would hand back every actor's
        // memory (#105 review round 11).
        if query.actor_id.is_some() && query.source_namespace.is_none() {
            return Vec::new();
        }
        let cap = self.config.max_links_per_memory;
        let limit = query.limit;
        // The actor scope `relevant` ranks under, computed before the query is
        // moved: an actor-scoped query must not receive another actor's claim
        // as context, direct or expanded.
        let actor = query
            .source_namespace
            .zip(query.actor_id)
            .map(|(source, actor)| self.pseudonymizer.pseudonymize(source, actor));
        // Nodes must be visible at the read timestamp: recorded at or before
        // `now_ms` and not yet expired. A future node is not live for this
        // query, so neither the link view nor the hop can resolve it (#105
        // review round 10).
        let live: HashSet<u64> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.expires_at_ms > now_ms
                    && entry.created_at_ms <= now_ms
                    && !self.is_revoked(entry)
            })
            .map(|entry| entry.memory_id)
            .collect();

        let matched: Vec<(&MemoryEntry, bool, Vec<MemoryLink>)> = self
            .relevant(query, now_ms)
            .into_iter()
            // The actor scope is reapplied to direct matches, not only the
            // hop: with an actor *and* a topic supplied, `relevant` admits
            // entries matching either signal, so another actor's same-topic
            // claim already ranks as a direct match here and no expansion
            // check would ever see it (#105 review round 8). Ranking keeps
            // every actor match above every topic-only match, so this filter
            // only ever drops the tail of the taken window and can never
            // displace an in-scope direct match.
            .filter(|entry| {
                actor
                    .as_ref()
                    .is_none_or(|scope| entry.source.pseudonymous_actor_id.as_ref() == Some(scope))
            })
            .map(|entry| {
                let (stale, links) = self.link_view(entry.memory_id, &live, cap, now_ms);
                (entry, stale, links)
            })
            .collect();

        // One hop outward, ranked by the match that points at it: a linked
        // node that already matched is not added twice, a dead endpoint or an
        // out-of-scope actor is never resolved, and the caller's limit is the
        // bound on how much context the hop may add.
        let mut expanded: Vec<(&MemoryEntry, bool, Vec<MemoryLink>)> = Vec::new();
        if matched.len() < limit {
            let mut selected: HashSet<u64> = matched
                .iter()
                .map(|(entry, _, _)| entry.memory_id)
                .collect();
            'hop: for (entry, _, links) in &matched {
                for link in links {
                    let other = if link.from == entry.memory_id {
                        link.to
                    } else {
                        link.from
                    };
                    if !live.contains(&other) {
                        continue;
                    }
                    let Some(node) = self
                        .entries
                        .iter()
                        .find(|candidate| candidate.memory_id == other)
                    else {
                        continue;
                    };
                    if actor.as_ref().is_some_and(|scope| {
                        node.source.pseudonymous_actor_id.as_ref() != Some(scope)
                    }) {
                        continue;
                    }
                    if !selected.insert(other) {
                        continue;
                    }
                    let (stale, node_links) = self.link_view(node.memory_id, &live, cap, now_ms);
                    expanded.push((node, stale, node_links));
                    if matched.len() + expanded.len() >= limit {
                        break 'hop;
                    }
                }
            }
        }

        matched
            .into_iter()
            .chain(expanded)
            .map(|(entry, stale, links)| LinkedMemory {
                entry,
                stale,
                links,
                vocab_version: MEMORY_LINK_VOCAB_VERSION,
                epistemic_version: entry.epistemic_version.as_str(),
            })
            .collect()
    }

    /// Whether a retained entry may be handed to generation as shared context,
    /// independent of who is asking.
    ///
    /// "Carries no actor" is not evidence of anything: a `ChatMessage` and an
    /// `OperatorCommand` are both valid envelopes without an `actor_id`, so an
    /// unowned public claim and an operator's private record would both look
    /// like shared memory under that test (#105 review). Sharing is therefore
    /// decided per source class:
    ///
    /// * `System`, `Game`, `Stream`, and `Timer` observations and the
    ///   performer's own `Speech` are the stream's knowledge, attributable to
    ///   the runtime rather than to a person;
    /// * `PublicChat` and `Donation` are somebody's words, so they are in scope
    ///   only for that same actor (see [`Self::context_for`]);
    /// * `Operator` is the operator's private record. A privacy *label* is not
    ///   an egress authorization, so it never enters generation; a deliberate
    ///   declassification policy would have to authorize it first.
    fn generation_shared(entry: &MemoryEntry) -> bool {
        source_class_is_shared(entry.source.source_class)
    }

    /// Bounded retrieval for a generation context (#105).
    ///
    /// An entry is in scope when it is *shared* by source class (see
    /// [`Self::generation_shared`]) and not attributed to somebody else, or
    /// when it is the current speaker's own claim — the speaker's own words are
    /// their context even though nobody else may see them.
    ///
    /// Visibility is the same time-indexed rule as `relevant`: a claim recorded
    /// after `now_ms` or already expired at `now_ms` does not exist for the
    /// query, so a replay can never act on its own future.
    ///
    /// Supersession is applied, not ignored: a claim an explicit supersession
    /// replaced is not evidence of the present, so the superseded node is
    /// dropped and its live correction is promoted into the same rank bucket.
    /// A correction that changed topic therefore cannot be cut behind the claim
    /// it replaced — the failure the existing `stale` verdict exists to prevent
    /// (#105 review).
    ///
    /// Ranking is deterministic over the retained state: promoted/topic matches
    /// lead, then logical record time (newest first) with node id breaking ties
    /// — the same timeline `relevant` and `TemporalBefore` use — so the same
    /// retained memory always yields the same prefix, and `limit` bounds both
    /// the result and the work.
    pub fn context_for(
        &self,
        source_namespace: &str,
        actor_id: Option<&str>,
        topic: Option<&str>,
        limit: usize,
        now_ms: u64,
    ) -> Vec<&MemoryEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let actor_scope =
            actor_id.map(|actor| self.pseudonymizer.pseudonymize(source_namespace, actor));
        let normalized_topic = topic.map(normalize);
        let live = |entry: &MemoryEntry| {
            entry.expires_at_ms > now_ms && entry.created_at_ms <= now_ms && !self.is_revoked(entry)
        };
        let matches_topic = |entry: &MemoryEntry| {
            normalized_topic.as_deref().is_some()
                && normalized_topic.as_deref() == entry.topic.as_deref().map(normalize).as_deref()
        };
        // Only a supersession whose superseding node is itself alive at the
        // read timestamp replaces anything: an expired correction is already
        // deleted for retrieval and cannot keep the claim it corrected current
        // either — the same rule `link_view` applies to its `stale` verdict.
        let mut correction_of: HashMap<u64, u64> = HashMap::new();
        for link in &self.links {
            if link.kind != MemoryLinkKind::Supersedes || link.created_at_ms > now_ms {
                continue;
            }
            let superseder_live = self
                .entries
                .iter()
                .any(|entry| entry.memory_id == link.from && live(entry));
            if superseder_live {
                correction_of.entry(link.to).or_insert(link.from);
            }
        }
        let visible: Vec<&MemoryEntry> = self.entries.iter().filter(|entry| live(entry)).collect();
        let mut promoted: HashSet<u64> = HashSet::new();
        for entry in &visible {
            if matches_topic(entry)
                && let Some(correction) = correction_of.get(&entry.memory_id)
            {
                promoted.insert(*correction);
            }
        }
        let in_scope = |entry: &MemoryEntry| {
            let owner = entry.source.pseudonymous_actor_id.as_ref();
            let own_claim = actor_scope
                .as_ref()
                .is_some_and(|scope| owner == Some(scope));
            let shared = Self::generation_shared(entry)
                && owner.is_none_or(|owner| actor_scope.as_ref() == Some(owner));
            own_claim || shared
        };
        let mut ranked = visible
            .into_iter()
            .filter(|entry| !correction_of.contains_key(&entry.memory_id))
            .filter(|entry| in_scope(entry))
            .map(|entry| {
                let priority =
                    u8::from(matches_topic(entry) || promoted.contains(&entry.memory_id));
                (priority, entry)
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.created_at_ms.cmp(&left.1.created_at_ms))
                .then_with(|| right.1.memory_id.cmp(&left.1.memory_id))
        });
        ranked
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect()
    }

    /// One entry's bounded link view, with every edge required to have **both**
    /// endpoints live at `now_ms` and to have been recorded at or before
    /// `now_ms`: retrieval already treats an expired node as deleted, and a
    /// replay-time query must not see a relation from the future — the
    /// staleness verdict is likewise drawn only from edges that already exist
    /// at the read timestamp (#105 review round 10). Caller-owned evidence
    /// leads: supersessions first, then causal edges, both **newest first** by
    /// record time with the higher node id breaking ties — the same
    /// insertion-order tie-break `relevant` and `TemporalBefore` use — so the
    /// per-entry cap can never truncate the latest correction behind an older
    /// one, even for a same-tick pair, and staleness (decided from
    /// supersession) outranks causation when only one edge fits. Temporal
    /// spine edges keep creation order, because the spine is the one kind the
    /// next write in the topic re-derives; and the per-memory budget cuts the
    /// view (#105
    /// review rounds 11-12).
    fn link_view(
        &self,
        memory_id: u64,
        live: &HashSet<u64>,
        cap: usize,
        now_ms: u64,
    ) -> (bool, Vec<MemoryLink>) {
        let incident: Vec<&MemoryLink> = self
            .links
            .iter()
            .filter(|link| {
                (link.from == memory_id || link.to == memory_id)
                    && link.created_at_ms <= now_ms
                    && live.contains(&link.from)
                    && live.contains(&link.to)
            })
            .collect();
        let stale = incident.iter().any(|link| {
            link.kind == MemoryLinkKind::Supersedes
                && link.to == memory_id
                && live.contains(&link.from)
        });
        let mut links: Vec<MemoryLink> = Vec::with_capacity(cap.min(incident.len()));
        // Explicit evidence leads, supersession before causation, then the
        // derived temporal spine: the caller-owned relations are the ones a
        // capped view must not drop, and the spine is the one that can be
        // re-derived on the next write in the topic.
        for kind in [
            MemoryLinkKind::Supersedes,
            MemoryLinkKind::CausedBy,
            MemoryLinkKind::TemporalBefore,
        ] {
            let mut ordered: Vec<&MemoryLink> = incident
                .iter()
                .copied()
                .filter(|link| link.kind == kind)
                .collect();
            if kind != MemoryLinkKind::TemporalBefore {
                // Newest correction first: a caller that reads this view must
                // get the latest supersession rather than the oldest when the
                // cap is one (#105 review round 11). The view is the bounded
                // retrieval surface, not the generation input: §11's request
                // content carries claims, never edges. Equal record
                // times break on the *higher* node id — insertion order,
                // newest first — exactly like `relevant` and the timeline
                // `TemporalBefore` derives from, so a same-tick pair can never
                // disagree about which correction is later (#105 review round
                // 12). The secondary key keeps truncation deterministic.
                ordered.sort_by(|left, right| {
                    right
                        .created_at_ms
                        .cmp(&left.created_at_ms)
                        .then_with(|| right.from.cmp(&left.from))
                        .then_with(|| right.to.cmp(&left.to))
                });
            }
            for link in ordered {
                if links.len() >= cap {
                    break;
                }
                links.push((*link).clone());
            }
        }
        (stale, links)
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
        // A working entry's claim is likewise only the event's own claim when
        // it matches the event text; a derived or rephrased sentence is not the
        // runtime's observation (#105 review round 2). As on the durable path,
        // the comparison uses the full normalized texts, so truncation cannot
        // hide appended content and a case/whitespace rewrite does not
        // downgrade the event's own claim (#105 review round 3).
        let claim_is_bound = event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|event_text| normalize(event_text) == normalize(claim));
        // Same complete-or-reject rule as the durable path: a verified fact
        // is never stored truncated (#105 review round 4).
        require_complete_verified_claim(
            claim,
            self.config.max_claim_bytes,
            MemoryEpistemicClass::derive(event.source_class, event.trust_level, claim_is_bound),
        )?;
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
            epistemic_class: MemoryEpistemicClass::derive(
                event.source_class,
                event.trust_level,
                claim_is_bound,
            ),
            epistemic_version: MEMORY_EPISTEMIC_VOCAB_VERSION.to_owned(),
            memory_id: 0,
        })
    }

    fn push_bounded(
        &mut self,
        mut entry: MemoryEntry,
        now_ms: u64,
    ) -> Result<&MemoryEntry, AdaptationError> {
        // Node identity is assigned at insertion and never derived from the
        // source event id: one event id may legitimately be retained more
        // than once, and links must name exactly this entry.
        let memory_id = self.next_memory_id;
        let next = memory_id
            .checked_add(1)
            .ok_or(AdaptationError::Invariant("memory node id space exhausted"))?;
        entry.memory_id = memory_id;
        self.next_memory_id = next;
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

/// A claim classed `VerifiedFact` must fit the storage budget in full.
///
/// Truncating a verified fact at `max_bytes` can drop the negation or
/// qualifier that bounded it — for example cutting "not verified" off "the
/// game author is not verified" — so the retained copy would assert something
/// the runtime never observed. Fail the write closed with a typed error
/// instead of storing a corrupted fact; this is stricter than the byte bound
/// alone and never applies to weaker classes (+ViewerClaim+,
/// +PerformerExperience+, +Observation+, +PersonaOpinion+), which assert
/// nothing and keep the truncation semantics (#105 review round 4).
fn require_complete_verified_claim(
    value: &str,
    max_bytes: usize,
    epistemic_class: MemoryEpistemicClass,
) -> Result<(), AdaptationError> {
    if epistemic_class == MemoryEpistemicClass::VerifiedFact && normalize(value).len() > max_bytes {
        return Err(AdaptationError::InvalidInput(
            "a verified-fact claim longer than max_claim_bytes would be stored truncated and could lose the negation or qualifier that bounded it; shorten the source event's own claim or raise max_claim_bytes",
        ));
    }
    Ok(())
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
mod tests;
