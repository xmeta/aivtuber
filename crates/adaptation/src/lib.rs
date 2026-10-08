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
    /// Internal node identity, assigned when the entry is inserted. Links
    /// reference this and never `source.event_id`: a source event id is not
    /// unique among retained entries (the scheduler may legitimately replay
    /// one) and is not durable identity, so it cannot name a graph node.
    /// `0` is never assigned; it marks an entry that has not been stored yet.
    pub memory_id: u64,
}

/// Versioned edge vocabulary for retained episodic memory (#105). Kept small
/// on purpose: temporal order is derived deterministically at write time and
/// supersession is declared by the caller that owns the update evidence;
/// causal kinds wait for an evidence source that can actually establish them.
pub const MEMORY_LINK_VOCAB_VERSION: &str = "memory-link-v1";

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

    /// The versioned form of the retained edge set. Anything that persists or
    /// replays links serializes this envelope rather than the bare list, so
    /// [`MEMORY_LINK_VOCAB_VERSION`] can never be separated from the data it
    /// versions.
    pub fn link_snapshot(&self) -> MemoryLinkSnapshot {
        MemoryLinkSnapshot {
            vocab_version: MEMORY_LINK_VOCAB_VERSION,
            links: self.links.iter().cloned().collect(),
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
    /// evidence, owned by the caller). Targets are resolved to exactly one
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
    pub fn remember_durable_superseding(
        &mut self,
        permit: &MemoryWritePermit,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[&str],
    ) -> Result<&MemoryEntry, AdaptationError> {
        let declarations_bound = self
            .config
            .max_links_per_memory
            .saturating_mul(SUPERSESSION_DECLARATION_FACTOR);
        if supersedes.len() > declarations_bound {
            return Err(AdaptationError::InvalidInput(
                "one write may not hand more supersession declarations than the bounded input \
                 allows; collapse repeated declarations before calling",
            ));
        }
        let mut unique: Vec<&str> =
            Vec::with_capacity(self.config.max_links_per_memory.min(supersedes.len()));
        for target in supersedes {
            if !unique.contains(target) {
                // Refuse as soon as a new unique target exceeds the budget:
                // at cap+1 uniques the write is already rejected, so
                // buffering beyond that would only waste memory.
                if unique.len() >= self.config.max_links_per_memory {
                    return Err(AdaptationError::InvalidInput(
                        "one write may not declare more supersessions than the per-memory link \
                         budget",
                    ));
                }
                unique.push(target);
            }
        }
        let mut targets = Vec::with_capacity(unique.len());
        for target in &unique {
            targets.push(self.resolve_supersession_target(target, now_ms)?);
        }
        // Explicit supersession evidence is never evicted to make room for
        // more of it: shedding the edge that marks a still-retained target
        // makes that claim current again while the memory that replaced it
        // is alive — the stale fact this layer exists to keep (#105). The
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
                link.kind == MemoryLinkKind::Supersedes
                    && survivors.contains(&link.from)
                    && survivors.contains(&link.to)
            })
            .count();
        let new_evidence_retained = targets.iter().filter(|id| survivors.contains(id)).count();
        if explicit_retained + new_evidence_retained > self.config.max_links {
            return Err(AdaptationError::InvalidInput(
                "the link store cannot hold this write's supersession evidence without \
                 discarding earlier evidence for a still-retained claim; raise max_links or let \
                 retention expire the earlier endpoints first",
            ));
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
            memory_id: 0,
        };
        let topic = entry.topic.clone();
        let stored = self.push_bounded(entry, now_ms)?;
        let new_id = stored.memory_id;
        self.link_new_memory(new_id, topic.as_deref(), now_ms, &targets);
        self.entries.back().ok_or(AdaptationError::Invariant(
            "inserted memory entry disappeared",
        ))
    }

    /// Resolve a declared supersession target to exactly one node, failing
    /// closed at every point identity or liveness is in doubt:
    ///
    /// * unknown or working-only ids are refused — no cross-retention edge;
    /// * an id whose only durable matches are expired *at `now_ms`* is
    ///   refused — an expired memory is already deleted for retrieval, so it
    ///   cannot authorize an update relation just because compaction has not
    ///   physically removed it yet;
    /// * an id retained as more than one live durable memory is refused as
    ///   ambiguous — a source event id is not a node identity, and the link
    ///   must not be guessed onto one of several candidates;
    /// * a target recorded *after* the superseding write is refused — a claim
    ///   cannot replace evidence from its own future, or a backfill would mark
    ///   a logically later claim stale and contradict the record-time ordering
    ///   the temporal spine and retrieval ranking both follow.
    fn resolve_supersession_target(&self, id: &str, now_ms: u64) -> Result<u64, AdaptationError> {
        let retained: Vec<&MemoryEntry> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.source.event_id == id && entry.retention == RetentionClass::Durable
            })
            .collect();
        if retained.is_empty() {
            return Err(AdaptationError::InvalidInput(
                "supersession target must be a retained durable memory",
            ));
        }
        let mut live = retained.iter().filter(|entry| entry.expires_at_ms > now_ms);
        let Some(target) = live.next() else {
            return Err(AdaptationError::InvalidInput(
                "supersession target has expired; an expired memory cannot authorize an update \
                 relation",
            ));
        };
        if live.next().is_some() {
            return Err(AdaptationError::InvalidInput(
                "supersession target is ambiguous: one source event id matches more than one \
                 retained durable memory, and links must not guess which node the caller meant",
            ));
        }
        if target.created_at_ms > now_ms {
            return Err(AdaptationError::InvalidInput(
                "supersession target was recorded after the superseding write; a claim cannot \
                 replace evidence from its own future",
            ));
        }
        Ok(target.memory_id)
    }

    /// Create this write's links, deterministic by construction: explicit
    /// supersessions first (they are the caller-owned evidence), then a
    /// temporal spine to the most recent same-topic durable memories, all
    /// within the per-memory budget. Same claims written twice dedupe to one
    /// edge, and the store total is re-bounded afterwards.
    fn link_new_memory(
        &mut self,
        new_id: u64,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[u64],
    ) {
        let mut budget = self.config.max_links_per_memory;
        for target in supersedes {
            if budget == 0 {
                break;
            }
            self.push_link(MemoryLink {
                from: new_id,
                to: *target,
                kind: MemoryLinkKind::Supersedes,
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
    /// supersession targets are both judged against this set, so evidence
    /// this write itself removes never counts against this write. The
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
    /// only, oldest first. Explicit supersession evidence is never removed
    /// here: [`Self::remember_durable_superseding`] refuses any write whose
    /// supersession edges would not fit alongside the ones already retained,
    /// so once the derived edges are gone the store is provably within its
    /// bound and this loop exits — an explicit-only store cannot be over the
    /// limit. Shedding an explicit edge instead would make the claim it marks
    /// current again while the memory that replaced it is still live, which
    /// is the stale fact #105 exists to keep, so the bound yields to that
    /// correctness rule rather than forgetting evidence.
    fn enforce_link_bound(&mut self) {
        while self.links.len() > self.config.max_links {
            let Some(derived) = self
                .links
                .iter()
                .position(|link| link.kind == MemoryLinkKind::TemporalBefore)
            else {
                // Unreachable: the pre-store supersession check guarantees
                // explicit edges alone fit, so there is always a derived edge
                // left to shed before the bound can be exceeded.
                break;
            };
            self.links.remove(derived);
        }
        debug_assert!(
            self.links.len() <= self.config.max_links,
            "an explicit-only link store exceeded its bound; the pre-store supersession check \
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
    /// bounded link view, and — filling whatever the query limit still allows
    /// — with the live nodes one link away from those results. Direct matches
    /// and the expansion both keep the query's **actor** scope: a direct
    /// match can arrive through the topic alone when the query supplies an
    /// actor *and* a topic, so the scope is reapplied to every result, not
    /// only the hop. `stale` is a
    /// verdict over the **full** retained edge set, never over the capped
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
        let cap = self.config.max_links_per_memory;
        let limit = query.limit;
        // The actor scope `relevant` ranks under, computed before the query is
        // moved: an actor-scoped query must not receive another actor's claim
        // as context, direct or expanded.
        let actor = query
            .source_namespace
            .zip(query.actor_id)
            .map(|(source, actor)| self.pseudonymizer.pseudonymize(source, actor));
        let live: HashSet<u64> = self
            .entries
            .iter()
            .filter(|entry| entry.expires_at_ms > now_ms)
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
                let (stale, links) = self.link_view(entry.memory_id, &live, cap);
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
                    let (stale, node_links) = self.link_view(node.memory_id, &live, cap);
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
            })
            .collect()
    }

    /// One entry's bounded link view, with every edge required to have **both**
    /// endpoints live at `now_ms`: retrieval already treats an expired node as
    /// deleted, so relation metadata about it would expose a dangling edge to
    /// a node that no longer exists. Supersessions lead, creation order breaks
    /// ties within a kind, and the per-memory budget cuts the view.
    fn link_view(
        &self,
        memory_id: u64,
        live: &HashSet<u64>,
        cap: usize,
    ) -> (bool, Vec<MemoryLink>) {
        let incident: Vec<&MemoryLink> = self
            .links
            .iter()
            .filter(|link| {
                (link.from == memory_id || link.to == memory_id)
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
        for kind in [MemoryLinkKind::Supersedes, MemoryLinkKind::TemporalBefore] {
            for link in &incident {
                if links.len() >= cap {
                    break;
                }
                if link.kind == kind {
                    links.push((*link).clone());
                }
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
    fn link_store_bound_must_cover_one_maximum_write() {
        // A store that cannot hold one maximum-size write would drop an
        // explicit supersession the very same write just had accepted, which
        // is not historical edge eviction — it is a lost caller-owned fact.
        let error = WorkingMemory::new(
            WorkingMemoryConfig {
                max_links: 1,
                max_links_per_memory: 2,
                ..WorkingMemoryConfig::default()
            },
            test_pseudonymizer(),
        )
        .expect_err("max_links must cover one maximum-size write");
        assert!(matches!(error, AdaptationError::InvalidConfiguration(_)));
    }

    /// Mint a permit and perform one durable write inside the same call: the
    /// permit is a non-serializable capability for that gate pass, so it
    /// never outlives the write it authorizes.
    fn durable_write(
        memory: &mut WorkingMemory,
        security: &mut SecurityRuntime,
        id: &str,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
    ) {
        durable_write_superseding(memory, security, id, claim, topic, now_ms, &[])
            .expect("durable remember");
    }

    fn durable_write_superseding(
        memory: &mut WorkingMemory,
        security: &mut SecurityRuntime,
        id: &str,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[&str],
    ) -> Result<u64, AdaptationError> {
        let source = event(id, SourceClass::System, TrustLevel::Trusted, None);
        let (_, permit) = security.authorize_memory_write(&source, None);
        let permit = permit.expect("system permit");
        memory
            .remember_durable_superseding(&permit, claim, topic, now_ms, supersedes)
            .map(|entry| entry.memory_id)
    }

    fn unfiltered_query(limit: usize) -> MemoryQuery<'static> {
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: None,
            limit,
        }
    }

    fn retained_node(memory: &WorkingMemory, event_id: &str) -> u64 {
        memory
            .entries()
            .iter()
            .find(|entry| entry.source.event_id == event_id)
            .map(|entry| entry.memory_id)
            .expect("retained node")
    }

    /// A durable write from a system source that still carries an actor, so a
    /// retrieval query can scope by that actor the way a viewer-scoped
    /// lookup does.
    fn durable_write_actor(
        memory: &mut WorkingMemory,
        security: &mut SecurityRuntime,
        id: &str,
        topic: Option<&str>,
        now_ms: u64,
        actor: &str,
        supersedes: &[&str],
    ) -> u64 {
        let source = event(id, SourceClass::System, TrustLevel::Trusted, Some(actor));
        let (_, permit) = security.authorize_memory_write(&source, None);
        let permit = permit.expect("system permit");
        memory
            .remember_durable_superseding(
                &permit,
                &format!("claim from {id}"),
                topic,
                now_ms,
                supersedes,
            )
            .expect("durable remember")
            .memory_id
    }

    #[test]
    fn an_actor_scoped_query_never_expands_into_another_actors_claim() {
        // The derived temporal spine connects every durable entry that shares
        // a topic, so an unscoped hop would hand viewer 1's query a claim
        // viewer 2 wrote — unrelated viewer memory in retrieved context.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig::default());
        durable_write_actor(
            &mut memory,
            &mut security,
            "evt-one",
            Some("game-x"),
            1_000,
            "viewer-1",
            &[],
        );
        durable_write_actor(
            &mut memory,
            &mut security,
            "evt-two",
            Some("game-x"),
            2_000,
            "viewer-2",
            &[],
        );
        durable_write_actor(
            &mut memory,
            &mut security,
            "evt-update",
            Some("game-x"),
            3_000,
            "viewer-2",
            &["evt-one"],
        );
        assert!(
            !memory.links().is_empty(),
            "the fixture produced both a temporal and a cross-actor supersession edge"
        );

        let scoped = memory.relevant_linked(
            MemoryQuery {
                source_namespace: Some("system-health"),
                actor_id: Some("viewer-1"),
                topic: None,
                limit: 10,
            },
            3_500,
        );
        assert_eq!(
            scoped.len(),
            1,
            "only the scoped actor's own claim is retrieved: {:?}",
            scoped
                .iter()
                .map(|item| item.entry.source.event_id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(scoped[0].entry.source.event_id, "evt-one");
        // Scope bounds *content*, not verdicts or edge metadata: the edge
        // carries node ids only, and the staleness verdict still comes from
        // the full retained edge set.
        assert!(
            scoped[0].stale,
            "the cross-actor supersession still marks it"
        );
        assert!(
            !scoped[0].links.is_empty(),
            "the bounded view still names the relations"
        );

        // Without an actor scope the same edges are all reachable: the hop
        // only ever narrows to what the query asked for.
        let unscoped = memory.relevant_linked(unfiltered_query(10), 3_500);
        assert_eq!(unscoped.len(), 3, "every live node is retrievable unscoped");
    }

    #[test]
    fn an_actor_and_topic_query_never_returns_another_actors_direct_match() {
        // With an actor *and* a topic supplied, `relevant` admits entries
        // matching either signal, so another viewer's same-topic claim ranks
        // as a direct match — the hop's actor check never sees it, and only
        // `relevant_linked`'s own re-filter can refuse it (#105 review
        // round 8).
        let mut memory = test_memory(WorkingMemoryConfig::default());
        let own = event(
            "evt-own",
            SourceClass::PublicChat,
            TrustLevel::Untrusted,
            Some("viewer-a"),
        );
        let foreign = event(
            "evt-foreign",
            SourceClass::PublicChat,
            TrustLevel::Untrusted,
            Some("viewer-b"),
        );
        memory
            .remember_working(&own, "likes rust", Some("coding"), 10)
            .expect("own");
        memory
            .remember_working(&foreign, "also likes rust", Some("coding"), 20)
            .expect("foreign");

        let scoped = memory.relevant_linked(
            MemoryQuery {
                source_namespace: Some("public-chat"),
                actor_id: Some("viewer-a"),
                topic: Some("coding"),
                limit: 10,
            },
            30,
        );
        assert_eq!(
            scoped.len(),
            1,
            "the other viewer's same-topic claim must not arrive as context: {:?}",
            scoped
                .iter()
                .map(|item| item.entry.source.event_id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(scoped[0].entry.source.event_id, "evt-own");

        // The same topic without an actor scope still admits both entries:
        // the removal comes from the actor scope, not from narrowing what a
        // topic match means.
        let unscoped = memory.relevant_linked(
            MemoryQuery {
                source_namespace: None,
                actor_id: None,
                topic: Some("coding"),
                limit: 10,
            },
            30,
        );
        assert_eq!(unscoped.len(), 2, "both claims stay topic-retrievable");
    }

    #[test]
    fn a_supersession_target_recorded_after_the_write_is_refused() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig::default());
        durable_write(
            &mut memory,
            &mut security,
            "evt-later",
            "claim recorded later",
            Some("game-x"),
            2_000,
        );

        let error = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-backfill",
            "backfilled claim",
            Some("game-x"),
            1_000,
            &["evt-later"],
        )
        .expect_err("a claim cannot replace evidence from its own future");
        assert!(error.to_string().contains("recorded after"), "{error}");
        assert_eq!(memory.len(), 1, "the refused write stored nothing");
        assert!(
            memory.links().is_empty(),
            "and minted no edge that would mark the later claim stale"
        );

        // The same declaration in logical order is accepted, so the rule is
        // direction and not a blanket ban on backfills.
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-update",
            "an update",
            Some("game-y"),
            3_000,
            &["evt-later"],
        )
        .expect("a later claim may replace an earlier one");
        assert_eq!(
            memory.links().len(),
            1,
            "the accepted supersession is stored"
        );
    }

    #[test]
    fn the_store_bound_sheds_derived_edges_before_explicit_supersession() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_links: 1,
            max_links_per_memory: 1,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b replaces a",
            Some("game-x"),
            2_000,
            &["evt-a"],
        )
        .expect("supersession accepted");
        assert_eq!(memory.links().len(), 1, "the store sits at its bound");

        // The next derived edge must not evict the caller's evidence: doing
        // so would resurrect a replaced claim while both nodes are live.
        durable_write(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c",
            Some("game-x"),
            3_000,
        );
        assert_eq!(
            memory.links().front().map(|link| link.kind),
            Some(MemoryLinkKind::Supersedes),
            "the bound sheds derived context first: {:?}",
            memory.links()
        );

        let results = memory.relevant_linked(unfiltered_query(10), 3_500);
        let a = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-a")
            .expect("claim a is still retained");
        assert!(
            a.stale,
            "the superseded claim stays marked while both endpoints are live"
        );
    }

    #[test]
    fn a_supersession_write_that_would_evict_earlier_evidence_is_refused() {
        // The re-review reproduction: with both limits at 1 the store cannot
        // hold two explicit edges, and evicting `B -> A` made A current again
        // while B — the memory that replaced it — was still alive. The bound
        // now refuses the *later* write instead of forgetting the earlier
        // evidence, and it refuses it before anything is stored, so the
        // caller learns the evidence was not accepted.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_links: 1,
            max_links_per_memory: 1,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b replaces a",
            Some("game-x"),
            2_000,
            &["evt-a"],
        )
        .expect("the first supersession fits the bound");
        assert_eq!(memory.links().len(), 1);

        let error = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c replaces b",
            Some("game-x"),
            3_000,
            &["evt-b"],
        )
        .expect_err("evidence that would displace earlier evidence is refused");
        assert!(
            error.to_string().contains("supersession evidence"),
            "{error}"
        );
        assert_eq!(memory.len(), 2, "the refused write stored nothing");
        assert_eq!(memory.links().len(), 1, "and mangled nothing");

        let results = memory.relevant_linked(unfiltered_query(10), 3_500);
        assert_eq!(results.len(), 2, "claim c was never accepted");
        let a = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-a")
            .expect("claim a is still retained");
        assert!(
            a.stale,
            "A was explicitly superseded and is still retained: {:?}",
            memory.links()
        );
        let b = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-b")
            .expect("claim b is still retained");
        assert!(
            !b.stale,
            "C's evidence was refused, so nothing retained supersedes B"
        );
    }

    #[test]
    fn a_superseding_write_is_not_blocked_by_an_edge_that_its_own_compaction_expires() {
        // Round-3 expiry reproduction: the retained B -> A edge has an
        // endpoint already expired at the write timestamp but not yet
        // physically compacted. It is dead evidence — not a claim held
        // against a still-retained node — so counting it would refuse a
        // supersession that fits once this write's own compaction runs, and
        // a valid update would wait for an unrelated write to free
        // logically dead capacity.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_links: 1,
            max_links_per_memory: 1,
            durable_ttl_ms: 1_000,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            100,
        );
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b replaces a",
            Some("game-x"),
            200,
            &["evt-a"],
        )
        .expect("the first supersession fits the bound");
        assert_eq!(memory.links().len(), 1);

        // A expired at 1_100 while B (created at 200) stays live until
        // 1_200, so at 1_150 the B -> A edge is dead but not compacted.
        let written = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c replaces b",
            Some("game-x"),
            1_150,
            &["evt-b"],
        )
        .expect("an edge that expires with this write's own compaction does not block the write");

        assert_eq!(
            memory.links().len(),
            1,
            "one live edge remains: {:?}",
            memory.links()
        );
        assert_eq!(memory.links()[0].kind, MemoryLinkKind::Supersedes);
        assert_eq!(memory.links()[0].from, written);
        assert_eq!(memory.links()[0].to, retained_node(&memory, "evt-b"));
        assert!(
            memory
                .entries()
                .iter()
                .all(|entry| entry.source.event_id != "evt-a"),
            "this write's compaction physically removed the expired endpoint"
        );

        let results = memory.relevant_linked(unfiltered_query(10), 1_150);
        assert_eq!(results.len(), 2, "A is expired, B and C are retained");
        let b = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-b")
            .expect("claim b is still retained");
        assert!(b.stale, "C -> B marks the replaced claim stale");
        let c = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-c")
            .expect("claim c was accepted");
        assert!(!c.stale, "nothing retained supersedes C");
    }

    #[test]
    fn a_superseding_write_is_not_blocked_by_edges_its_own_insertion_evicts() {
        // Round-3 capacity reproduction: with two node slots the insertion
        // of C evicts A, which prunes the dead B -> A edge within the same
        // write, so C -> B exactly fits the one-edge store. The earlier
        // evidence went with the node retention already removed — no
        // still-retained claim loses its marker — so refusing C would only
        // make a valid update wait for an unrelated write to free the slot.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_entries: 2,
            max_links: 1,
            max_links_per_memory: 1,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b replaces a",
            Some("game-x"),
            2_000,
            &["evt-a"],
        )
        .expect("the first supersession fits the bound");
        assert_eq!(memory.links().len(), 1);

        let written = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c replaces b",
            Some("game-x"),
            3_000,
            &["evt-b"],
        )
        .expect("an edge that this insertion evicts does not block the write");

        assert_eq!(memory.len(), 2, "the insertion evicted A");
        assert!(
            memory
                .entries()
                .iter()
                .all(|entry| entry.source.event_id != "evt-a"),
            "A is capacity-evicted: {:?}",
            memory.entries()
        );
        assert_eq!(
            memory.links().len(),
            1,
            "the one-edge store keeps C -> B: {:?}",
            memory.links()
        );
        assert_eq!(memory.links()[0].kind, MemoryLinkKind::Supersedes);
        assert_eq!(memory.links()[0].from, written);
        assert_eq!(memory.links()[0].to, retained_node(&memory, "evt-b"));

        let results = memory.relevant_linked(unfiltered_query(10), 3_500);
        let b = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-b")
            .expect("claim b is still retained");
        assert!(b.stale, "C -> B marks the replaced claim stale");
        let c = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-c")
            .expect("claim c was accepted");
        assert!(!c.stale, "nothing retained supersedes C");
    }

    #[test]
    fn a_supersession_target_this_insertion_evicts_does_not_consume_link_capacity() {
        // Round-4 capacity reproduction: three node slots hold X, A, B and
        // the one-edge store holds the retained B -> A evidence. C
        // supersedes X — the node this insertion capacity-evicts — so the
        // fresh C -> X edge is pruned by the same write's compaction and
        // the store ends exactly where it started. Refusing C would hold a
        // write hostage to a slot its own insertion frees; the evicted
        // target leaves no retained claim that could resurrect.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_entries: 3,
            max_links: 1,
            max_links_per_memory: 1,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-x",
            "claim x",
            Some("game-x"),
            1_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            2_000,
        );
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b replaces a",
            Some("game-x"),
            3_000,
            &["evt-a"],
        )
        .expect("the first supersession fits the bound");
        assert_eq!(memory.links().len(), 1);

        let written = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c replaces x",
            Some("game-x"),
            4_000,
            &["evt-x"],
        )
        .expect("a target this insertion evicts does not consume link capacity");

        assert_eq!(memory.len(), 3, "the insertion evicted X");
        assert!(
            memory
                .entries()
                .iter()
                .all(|entry| entry.source.event_id != "evt-x"),
            "X is capacity-evicted: {:?}",
            memory.entries()
        );
        assert_eq!(
            memory.links().len(),
            1,
            "C -> X was pruned with X; B -> A remains: {:?}",
            memory.links()
        );
        assert_eq!(memory.links()[0].kind, MemoryLinkKind::Supersedes);
        assert_eq!(memory.links()[0].from, retained_node(&memory, "evt-b"));
        assert_eq!(memory.links()[0].to, retained_node(&memory, "evt-a"));
        assert!(
            memory.links().iter().all(|link| link.from != written),
            "no edge survives from the new node when its only target was evicted"
        );

        let results = memory.relevant_linked(unfiltered_query(10), 4_500);
        let a = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-a")
            .expect("claim a is still retained");
        assert!(a.stale, "B -> A survives the write untouched");
        let c = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-c")
            .expect("claim c was accepted");
        assert!(!c.stale, "nothing retained supersedes C");
    }

    #[test]
    fn an_explicit_supersession_chain_survives_derived_edge_pressure() {
        // The same `A <- B <- C` shape at a bound that can hold the chain:
        // the store is then only ever allowed to shed derived edges, so both
        // explicitly superseded nodes keep their verdict while all three are
        // live — the assertion the re-review asked for under edge pressure.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_entries: 8,
            max_links: 2,
            max_links_per_memory: 1,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b replaces a",
            Some("game-x"),
            2_000,
            &["evt-a"],
        )
        .expect("first link fits");
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c replaces b",
            Some("game-x"),
            3_000,
            &["evt-b"],
        )
        .expect("the chain fits the bound");
        assert_eq!(memory.links().len(), 2, "the chain fills the bound");

        // Derived pressure: every later same-topic write wants a temporal
        // edge the bound must shed instead of touching the chain.
        for (id, at_ms) in [("evt-d", 4_000_u64), ("evt-e", 5_000_u64)] {
            durable_write(
                &mut memory,
                &mut security,
                id,
                "a later claim",
                Some("game-x"),
                at_ms,
            );
        }
        assert_eq!(
            memory
                .links()
                .iter()
                .filter(|link| link.kind == MemoryLinkKind::Supersedes)
                .count(),
            2,
            "both explicit edges survived derived pressure: {:?}",
            memory.links()
        );

        let results = memory.relevant_linked(unfiltered_query(10), 5_500);
        assert_eq!(results.len(), 5, "every node is retained");
        for id in ["evt-a", "evt-b"] {
            let item = results
                .iter()
                .find(|result| result.entry.source.event_id == id)
                .expect("retained");
            assert!(
                item.stale,
                "{id} was explicitly superseded and is still retained: {:?}",
                memory.links()
            );
        }
        let c = results
            .iter()
            .find(|result| result.entry.source.event_id == "evt-c")
            .expect("retained");
        assert!(!c.stale, "nothing retained supersedes C");
    }

    #[test]
    fn repeated_supersession_targets_are_counted_once_against_the_write_budget() {
        for cap in [2_usize, 3] {
            let mut security = security();
            let mut memory = test_memory(WorkingMemoryConfig {
                max_links: cap + 2,
                max_links_per_memory: cap,
                ..WorkingMemoryConfig::default()
            });
            durable_write(
                &mut memory,
                &mut security,
                "evt-a",
                "claim a",
                Some("game-x"),
                1_000,
            );
            durable_write(
                &mut memory,
                &mut security,
                "evt-b",
                "claim b",
                Some("game-x"),
                2_000,
            );

            // Three declarations, two unique targets: the raw count must not
            // refuse the write, and the duplicate must not eat a budget slot
            // the temporal spine could have used.
            let written = durable_write_superseding(
                &mut memory,
                &mut security,
                "evt-d",
                "claim d",
                Some("game-x"),
                3_000,
                &["evt-a", "evt-a", "evt-b"],
            )
            .unwrap_or_else(|error| panic!("cap {cap}: duplicate names are one target: {error}"));
            let supersessions = memory
                .links()
                .iter()
                .filter(|link| link.from == written && link.kind == MemoryLinkKind::Supersedes)
                .count();
            assert_eq!(supersessions, 2, "cap {cap}: one edge per unique target");
            let temporal = memory
                .links()
                .iter()
                .filter(|link| link.to == written && link.kind == MemoryLinkKind::TemporalBefore)
                .count();
            assert_eq!(
                temporal,
                usize::from(cap >= 3),
                "cap {cap}: whatever the duplicates did not spend stays available to the spine"
            );
        }
    }

    #[test]
    fn supersession_declarations_are_bounded_before_deduplication() {
        // Round-6: the dedup pass used to allocate capacity for — and walk —
        // the entire raw slice before applying the per-write budget, so a
        // flood of restated ids could force unbounded allocation and CPU
        // despite producing a single edge. The input is now bounded before
        // any allocation, while ordinary restatement inside that bound keeps
        // its old meaning: duplicates still count once.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_links: 4,
            max_links_per_memory: 2,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        let links_before = memory.links().len();

        // Bound: max_links_per_memory (2) × SUPERSESSION_DECLARATION_FACTOR.
        let flood = vec!["evt-a"; 4_096];
        let error = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b",
            Some("game-x"),
            2_000,
            &flood,
        )
        .expect_err("an oversized declaration list is refused before the dedup scan");
        assert!(
            error.to_string().contains("declarations"),
            "the refusal names the unbounded input: {error}"
        );
        assert_eq!(memory.len(), 1, "the refused write stored nothing");
        assert_eq!(memory.links().len(), links_before, "and mangled nothing");

        // Inside the bound, restatement keeps its old meaning: duplicates
        // count once rather than erroring or spending budget.
        let written = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b replaces a",
            Some("game-x"),
            2_000,
            &["evt-a", "evt-a", "evt-a"],
        )
        .expect("restatement inside the bound is still one target");
        let supersessions = memory
            .links()
            .iter()
            .filter(|link| link.from == written && link.kind == MemoryLinkKind::Supersedes)
            .count();
        assert_eq!(supersessions, 1, "three names, one edge");
    }

    #[test]
    fn a_superseding_claim_brings_its_linked_target_back_within_the_query_limit() {
        // The cross-topic update: the query matches only the new claim, and
        // the single-hop timeline is only consumable if the claim it replaces
        // comes back with it — otherwise the caller must bypass the API and
        // scan the store for the endpoint id the view merely names.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig::default());
        durable_write(
            &mut memory,
            &mut security,
            "evt-old-topic",
            "viewer disliked the game",
            Some("game-x"),
            1_000,
        );
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-new-topic",
            "viewer finished the game",
            Some("game-y"),
            2_000,
            &["evt-old-topic"],
        )
        .expect("supersession accepted");

        let results = memory.relevant_linked(
            MemoryQuery {
                source_namespace: None,
                actor_id: None,
                topic: Some("game-y"),
                limit: 10,
            },
            3_000,
        );
        assert_eq!(
            results.len(),
            2,
            "the matched claim plus the retained claim it supersedes"
        );
        assert_eq!(results[0].entry.source.event_id, "evt-new-topic");
        assert_eq!(results[1].entry.source.event_id, "evt-old-topic");
        assert!(!results[0].stale, "the current claim is not stale");
        assert!(
            results[1].stale,
            "the replaced claim comes back marked stale"
        );

        // The hop is bounded by the caller's limit and never displaces a
        // direct match.
        let bounded = memory.relevant_linked(
            MemoryQuery {
                source_namespace: None,
                actor_id: None,
                topic: Some("game-y"),
                limit: 1,
            },
            3_000,
        );
        assert_eq!(bounded.len(), 1, "the query limit is still the bound");
        assert_eq!(bounded[0].entry.source.event_id, "evt-new-topic");
    }

    #[test]
    fn an_expired_endpoint_never_appears_in_a_returned_link_view() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            durable_ttl_ms: 1_500,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b",
            Some("game-x"),
            2_000,
        );
        assert_eq!(memory.links().len(), 1, "one temporal edge between them");

        // `evt-a` expired at 2_500 while compaction has not run: retrieval
        // already treats it as deleted, so relation metadata about it is a
        // dangling edge to a node that no longer exists.
        let results = memory.relevant_linked(
            MemoryQuery {
                source_namespace: None,
                actor_id: None,
                topic: Some("game-x"),
                limit: 10,
            },
            3_000,
        );
        assert_eq!(results.len(), 1, "only the live claim is retrievable");
        assert!(
            results[0].links.is_empty(),
            "no dangling relation to the expired endpoint"
        );
        assert!(
            !results[0].stale,
            "an expired node cannot keep a claim marked"
        );
        assert_eq!(
            memory.links().len(),
            1,
            "physical removal stays compaction's job"
        );
    }

    #[test]
    fn serialized_link_artifacts_carry_the_vocabulary_version() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig::default());
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b",
            Some("game-x"),
            2_000,
        );

        let snapshot =
            serde_json::to_value(memory.link_snapshot()).expect("serialize the link envelope");
        assert_eq!(
            snapshot["vocab_version"],
            Value::String(MEMORY_LINK_VOCAB_VERSION.to_owned())
        );
        assert_eq!(snapshot["links"].as_array().map(Vec::len), Some(1));

        let results = memory.relevant_linked(unfiltered_query(10), 3_000);
        let view = serde_json::to_value(&results[0]).expect("serialize the retrieval view");
        assert_eq!(
            view["vocab_version"],
            Value::String(MEMORY_LINK_VOCAB_VERSION.to_owned()),
            "a replay can identify the vocabulary instead of inferring it: {view}"
        );
    }

    #[test]
    fn temporal_edges_follow_record_time_not_insertion_order() {
        // Backfill: the second write carries an *earlier* record time than
        // the first. Insertion order would derive `TemporalBefore` from the
        // 2_000 record to the 1_000 one — an edge that contradicts the record
        // times `relevant` ranks by.
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig::default());
        durable_write(
            &mut memory,
            &mut security,
            "evt-2000",
            "claim recorded at 2000",
            Some("game-x"),
            2_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-1000",
            "claim recorded at 1000",
            Some("game-x"),
            1_000,
        );
        assert!(
            memory.links().is_empty(),
            "a later record is not an earlier record's predecessor: {:?}",
            memory.links()
        );

        durable_write(
            &mut memory,
            &mut security,
            "evt-1500",
            "claim recorded at 1500",
            Some("game-x"),
            1_500,
        );
        let older = retained_node(&memory, "evt-1000");
        let middle = retained_node(&memory, "evt-1500");
        let newer = retained_node(&memory, "evt-2000");
        let edges: Vec<(u64, u64)> = memory
            .links()
            .iter()
            .map(|link| (link.from, link.to))
            .collect();
        assert_eq!(
            edges,
            vec![(older, middle)],
            "only the genuinely older record is a predecessor"
        );
        assert!(!edges.contains(&(newer, middle)));
    }

    #[test]
    fn staleness_is_decided_from_the_full_edge_set_not_the_capped_view() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_links_per_memory: 2,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b",
            Some("game-x"),
            2_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c",
            Some("game-x"),
            3_000,
        );
        // evt-a now carries two temporal edges — its whole view budget — so
        // the later explicit supersession is the third incident edge.
        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-d",
            "claim d",
            Some("game-x"),
            4_000,
            &["evt-a"],
        )
        .expect("supersession accepted");

        let results = memory.relevant_linked(
            MemoryQuery {
                source_namespace: None,
                actor_id: None,
                topic: Some("game-x"),
                limit: 10,
            },
            4_500,
        );
        let a = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-a")
            .expect("evt-a retained");
        assert!(
            a.stale,
            "a supersession that fell past the view cap still marks its target stale"
        );
        assert!(a.links.len() <= 2, "the view stays capped");
        assert_eq!(
            a.links.first().map(|link| link.kind),
            Some(MemoryLinkKind::Supersedes),
            "semantically decisive edges lead the bounded view: {:?}",
            a.links
        );
        assert_eq!(a.links[0].to, a.entry.memory_id);
    }

    #[test]
    fn an_ambiguous_supersession_target_is_refused_and_stores_nothing() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig::default());
        durable_write(
            &mut memory,
            &mut security,
            "evt-same",
            "first claim",
            None,
            1_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-same",
            "second unrelated claim",
            None,
            2_000,
        );
        assert_eq!(memory.len(), 2, "duplicate retained ids stay allowed");
        let copies: Vec<u64> = memory
            .entries()
            .iter()
            .map(|entry| entry.memory_id)
            .collect();
        assert_ne!(copies[0], copies[1], "each retained copy is its own node");
        let links_before = memory.links().len();

        let error = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-update",
            "an update",
            None,
            3_000,
            &["evt-same"],
        )
        .expect_err("one source event id retained twice must not be guessed onto a node");
        assert!(matches!(error, AdaptationError::InvalidInput(_)));
        assert_eq!(memory.len(), 2, "a refused write stores nothing");
        assert_eq!(
            memory.links().len(),
            links_before,
            "a refused write mints no links"
        );
        let results = memory.relevant_linked(unfiltered_query(10), 3_500);
        assert!(
            results.iter().all(|item| !item.stale),
            "neither duplicate may be marked stale by a refused write"
        );
    }

    #[test]
    fn an_expired_supersession_target_is_refused_and_stores_nothing() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            durable_ttl_ms: 1_000,
            ..WorkingMemoryConfig::default()
        });
        durable_write(&mut memory, &mut security, "evt-old", "old claim", None, 0);
        assert_eq!(
            memory.len(),
            1,
            "expired but not yet compacted: still physically present"
        );

        let error = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-update",
            "an update",
            None,
            2_500,
            &["evt-old"],
        )
        .expect_err(
            "an expired memory is already deleted for retrieval and cannot authorize an update \
             relation",
        );
        assert!(matches!(error, AdaptationError::InvalidInput(_)));
        assert_eq!(memory.len(), 1, "a refused write stores nothing");
        assert!(memory.links().is_empty(), "a refused write mints no links");
    }

    #[test]
    fn an_accepted_write_keeps_every_explicit_supersession_it_declared() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_links: 2,
            max_links_per_memory: 2,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b",
            Some("game-x"),
            2_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-c",
            "claim c",
            Some("game-x"),
            3_000,
        );
        assert_eq!(
            memory.links().len(),
            2,
            "the store is at its cap from historical edges only"
        );

        let update_id = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-d",
            "claim d",
            Some("game-x"),
            4_000,
            &["evt-a", "evt-b"],
        )
        .expect("write accepted");

        let declared = memory
            .links()
            .iter()
            .filter(|link| link.from == update_id && link.kind == MemoryLinkKind::Supersedes)
            .count();
        assert_eq!(
            declared, 2,
            "every explicit supersession of one accepted write survives it while its endpoints \
             remain retained"
        );
        assert!(
            memory
                .entries()
                .iter()
                .any(|entry| entry.source.event_id == "evt-a")
                && memory
                    .entries()
                    .iter()
                    .any(|entry| entry.source.event_id == "evt-b"),
            "endpoints retained"
        );
        let results = memory.relevant_linked(
            MemoryQuery {
                source_namespace: None,
                actor_id: None,
                topic: Some("game-x"),
                limit: 10,
            },
            4_500,
        );
        for superseded in ["evt-a", "evt-b"] {
            assert!(
                results
                    .iter()
                    .find(|item| item.entry.source.event_id == superseded)
                    .map(|item| item.stale)
                    .unwrap_or_else(|| panic!("{superseded} retained")),
                "{superseded} is reported stale"
            );
        }
    }

    #[test]
    fn deletion_prunes_by_node_identity_not_by_a_shared_source_event_id() {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            durable_ttl_ms: 1_000,
            working_ttl_ms: 10_000_000,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-x",
            "durable copy",
            Some("topic-x"),
            0,
        );
        // The same source event id legitimately exists again as working
        // memory (the scheduler may replay one); it is a different node.
        let working = event(
            "evt-x",
            SourceClass::PublicChat,
            TrustLevel::Untrusted,
            Some("viewer-1"),
        );
        memory
            .remember_working(&working, "working copy", Some("topic-x"), 0)
            .expect("working copy");
        let durable_id = memory
            .entries()
            .iter()
            .find(|entry| {
                entry.source.event_id == "evt-x" && entry.retention == RetentionClass::Durable
            })
            .map(|entry| entry.memory_id)
            .expect("durable copy");
        let working_id = memory
            .entries()
            .iter()
            .find(|entry| entry.retention == RetentionClass::Working)
            .map(|entry| entry.memory_id)
            .expect("working copy");
        assert_ne!(durable_id, working_id);

        durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-update",
            "an update",
            Some("topic-x"),
            100,
            &["evt-x"],
        )
        .expect("unique live durable target");
        assert_eq!(
            memory.links().len(),
            2,
            "one supersession plus one temporal spine edge to the durable node"
        );

        // Expire the *durable* node while the working copy with the same
        // source event id survives: the edges must go with the node they
        // pointed at, not stay alive behind the shared id.
        memory.compact(1_050);
        assert!(
            memory
                .entries()
                .iter()
                .any(|entry| entry.memory_id == working_id),
            "the working copy is still retained"
        );
        assert!(
            memory.links().is_empty(),
            "an edge may not outlive its node through a shared source event id"
        );
        let results = memory.relevant_linked(
            MemoryQuery {
                source_namespace: None,
                actor_id: None,
                topic: Some("topic-x"),
                limit: 10,
            },
            1_050,
        );
        let survivor = results
            .iter()
            .find(|item| item.entry.source.event_id == "evt-x")
            .expect("working copy retained");
        assert!(
            !survivor.stale,
            "the surviving copy is a different node and must not inherit the deleted node's \
             staleness"
        );
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
