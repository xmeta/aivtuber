//! RAM-only targeted erase with an erasure report (#305, ADR-0008 S1).
//!
//! Revoke visibility first, then delete, then prove. A transaction is a small
//! state machine over the retained store; every step is idempotent and
//! resumable, and until the transaction reaches `Reported` the scope stays
//! unreadable (fail closed). Nothing here reads or writes memory content: an
//! erase only reduces retained data, and it needs a non-serializable
//! [`MemoryErasePermit`] minted by the security runtime.
//!
//! Everything is bounded and ordered by logical time (`now_ms` supplied by the
//! caller) so a replay of the same inputs produces the same reports. No raw
//! actor id and no claim text is retained by the transaction, the rate-limit
//! table or the reports: scopes are keyed by pseudonym and memory id.

use super::{AdaptationError, MemoryEntry, WorkingMemory};
use aivtuber_runtime::{MemoryEraseAuthority, MemoryErasePermit};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};

/// Versioned vocabulary of the report; bump on an incompatible change.
pub const ERASURE_VOCAB_VERSION: &str = "memory-erasure-v1";

/// Minimum logical gap between two viewer self-erase requests from one
/// pseudonym (ADR-0008 D-Q7). Operator erases are not rate-limited here: the
/// authenticated control path has its own limiter.
pub const VIEWER_ERASE_MIN_INTERVAL_MS: u64 = 60_000;

/// Pseudonyms tracked by the viewer rate limiter. When full and nothing has
/// aged out, further viewer erase requests are refused (fail closed) rather
/// than growing the table.
pub const MAX_ERASE_RATE_ENTRIES: usize = 256;

/// Open (not yet reported) transactions. Erase runs to completion inside one
/// call, so more than a few can only exist if a caller steps manually.
pub const MAX_PENDING_ERASURES: usize = 8;

/// Retained reports (most recent last).
pub const MAX_ERASURE_REPORTS: usize = 16;

/// Who asked, for the report. Not an authority claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureRequester {
    Operator,
    Viewer,
}

impl From<MemoryEraseAuthority> for ErasureRequester {
    fn from(value: MemoryEraseAuthority) -> Self {
        match value {
            MemoryEraseAuthority::Operator => Self::Operator,
            MemoryEraseAuthority::Viewer => Self::Viewer,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureScopeKind {
    Actor,
    MemoryIds,
    ActorAndMemoryIds,
}

/// Transaction states, in order (ADR-0008 section 6.2, RAM-only subset: there
/// are no index, source or sidecar stores to purge in S1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureState {
    ReadRevoked,
    CanonicalDeleted,
    EdgesPruned,
    CachesPurged,
    ProposalsPurged,
    Verified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureOutcome {
    /// At least one canonical entry was removed.
    Erased,
    /// The scope matched nothing retained; a reported no-op.
    NoOp,
    /// A viewer request inside the rate-limit window; nothing was changed.
    RateLimited,
}

/// A class of copy the runtime cannot prove erased (ADR-0008 section 6.3).
/// The list is fixed and always reported in full: the runtime cannot see
/// whether a sidecar, provider or backup was ever in play, so it never claims
/// their absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureRemnant {
    /// A memory sidecar that received the claim and pseudonym.
    SidecarCopies,
    /// Hindsight `retire` only invalidates; the service keeps the claim.
    /// Only `close_scope` (bank delete) removes it, and not per claim.
    HindsightRetireOnly,
    /// A generated reply that used the claim may be persisted as an asset;
    /// no provenance links assets to memory ids (D-Q13).
    GeneratedAssets,
    /// Text already spoken or streamed (VODs, clips).
    SpokenOrBroadcastOutput,
    /// A request already handed to the generation executor may still carry
    /// the claim until it completes; bounded by its deadline.
    InFlightGenerationContext,
    /// Remote Thinking-provider request logs and sidecar-derived summaries.
    ModelProviderLogs,
    /// Bounded RAM audit/telemetry; claim text is not designed to be present
    /// but is not re-verified here.
    TelemetryAndAuditLogs,
    /// Operator backups, crash dumps and OS swap (D-Q11: none are created by
    /// the runtime, none are covered).
    ExternalBackupsAndProcessResidue,
}

impl ErasureRemnant {
    /// Every class, in report order.
    pub const ALL: [Self; 8] = [
        Self::SidecarCopies,
        Self::HindsightRetireOnly,
        Self::GeneratedAssets,
        Self::SpokenOrBroadcastOutput,
        Self::InFlightGenerationContext,
        Self::ModelProviderLogs,
        Self::TelemetryAndAuditLogs,
        Self::ExternalBackupsAndProcessResidue,
    ];
}

/// Typed, content-free result of one erase request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasureReport {
    pub vocab_version: String,
    pub request_id: String,
    pub requester: ErasureRequester,
    pub scope_kind: ErasureScopeKind,
    pub outcome: ErasureOutcome,
    /// Logical time the transaction reached `Reported`.
    pub completed_at_ms: u64,
    /// Canonical entries removed (including expired entries not yet compacted).
    pub entries_removed: usize,
    /// Links removed because an endpoint was erased.
    pub links_removed: usize,
    /// Derived caches purged. The store keeps none (context is assembled per
    /// call from the canonical entries), so this is `0` by construction.
    pub caches_purged: usize,
    /// Consolidation proposals purged. None exist in the RAM store (#281 is
    /// not implemented), so this is `0` by construction.
    pub proposals_purged: usize,
    /// Post-condition scan result: no entry or link references the scope.
    /// `false` only for `RateLimited`, where no scan ran.
    pub verified: bool,
    /// Always the full fixed list; see [`ErasureRemnant`].
    pub unprovable_remnants: Vec<ErasureRemnant>,
}

/// Result of starting or stepping a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErasureStatus {
    /// Still open; reads of the scope are revoked.
    Pending(ErasureState),
    Done(ErasureReport),
}

#[derive(Debug, Clone)]
pub(crate) struct PendingErasure {
    request_id: String,
    requester: ErasureRequester,
    scope_kind: ErasureScopeKind,
    /// Pseudonym of the erased actor; never the raw id.
    actor: Option<String>,
    memory_ids: Vec<u64>,
    state: ErasureState,
    /// Node ids removed at `CanonicalDeleted`; used to verify edge pruning.
    removed_ids: HashSet<u64>,
    entries_removed: usize,
    links_removed: usize,
}

impl PendingErasure {
    fn covers(&self, entry: &MemoryEntry) -> bool {
        self.memory_ids.binary_search(&entry.memory_id).is_ok()
            || self
                .actor
                .as_ref()
                .is_some_and(|actor| entry.source.pseudonymous_actor_id.as_ref() == Some(actor))
    }
}

/// Bounded erase bookkeeping owned by [`WorkingMemory`].
#[derive(Debug, Clone, Default)]
pub(crate) struct ErasureBook {
    pending: Vec<PendingErasure>,
    reports: VecDeque<ErasureReport>,
    /// Pseudonym -> logical time of the last accepted viewer erase.
    viewer_last: BTreeMap<String, u64>,
    pub(crate) completed: u64,
    pub(crate) entries_removed: u64,
    pub(crate) links_removed: u64,
    pub(crate) rate_limited: u64,
}

impl ErasureBook {
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn revokes(&self, entry: &MemoryEntry) -> bool {
        self.pending.iter().any(|pending| pending.covers(entry))
    }
}

impl WorkingMemory {
    /// Whether an open erase transaction has already revoked read visibility
    /// of this entry. Every retrieval path filters through this first.
    pub(crate) fn is_revoked(&self, entry: &MemoryEntry) -> bool {
        self.erasure.revokes(entry)
    }

    /// Most recent erasure reports, oldest first, bounded by
    /// [`MAX_ERASURE_REPORTS`].
    pub fn erasure_reports(&self) -> &VecDeque<ErasureReport> {
        &self.erasure.reports
    }

    /// Open transactions: scopes that are revoked but not yet reported.
    pub fn pending_erasure_ids(&self) -> Vec<&str> {
        self.erasure
            .pending
            .iter()
            .map(|pending| pending.request_id.as_str())
            .collect()
    }

    /// Run one erase to completion (revoke, delete, prune, verify, report).
    pub fn erase(
        &mut self,
        permit: &MemoryErasePermit,
        now_ms: u64,
    ) -> Result<ErasureReport, AdaptationError> {
        let mut status = self.begin_erase(permit, now_ms)?;
        loop {
            match status {
                ErasureStatus::Done(report) => return Ok(report),
                ErasureStatus::Pending(_) => {
                    status = self.advance_erase(permit.request_id(), now_ms)?;
                }
            }
        }
    }

    /// Start a transaction and revoke read visibility of its scope. This is
    /// the commit point of the viewer-visible guarantee: from here on no read
    /// path returns the scope's entries, even if a later step never runs.
    ///
    /// Idempotent: a request id that is already open returns its current
    /// state, and one already reported returns the retained report. A viewer
    /// request inside the rate-limit window returns a `RateLimited` report
    /// and changes nothing.
    pub fn begin_erase(
        &mut self,
        permit: &MemoryErasePermit,
        now_ms: u64,
    ) -> Result<ErasureStatus, AdaptationError> {
        let request_id = permit.request_id();
        if let Some(open) = self
            .erasure
            .pending
            .iter()
            .find(|pending| pending.request_id == request_id)
        {
            return Ok(ErasureStatus::Pending(open.state));
        }
        if let Some(done) = self
            .erasure
            .reports
            .iter()
            .find(|report| report.request_id == request_id)
        {
            return Ok(ErasureStatus::Done(done.clone()));
        }

        let actor = permit
            .actor()
            .map(|(namespace, id)| self.pseudonymizer.pseudonymize(namespace, id));
        let requester = ErasureRequester::from(permit.authority());
        let scope_kind = match (actor.is_some(), permit.memory_ids().is_empty()) {
            (true, true) => ErasureScopeKind::Actor,
            (false, false) => ErasureScopeKind::MemoryIds,
            (true, false) => ErasureScopeKind::ActorAndMemoryIds,
            (false, true) => {
                return Err(AdaptationError::InvalidInput("erase scope is empty"));
            }
        };
        if permit.authority() == MemoryEraseAuthority::Viewer
            && (scope_kind != ErasureScopeKind::Actor || actor.is_none())
        {
            // Defence in depth: the runtime never mints such a permit.
            return Err(AdaptationError::InvalidInput(
                "viewer erase is limited to the requester's own pseudonym",
            ));
        }

        if requester == ErasureRequester::Viewer
            && let Some(pseudonym) = actor.as_ref()
            && !self.admit_viewer_erase(pseudonym, now_ms)
        {
            self.erasure.rate_limited = self.erasure.rate_limited.saturating_add(1);
            let report = ErasureReport {
                vocab_version: ERASURE_VOCAB_VERSION.to_owned(),
                request_id: request_id.to_owned(),
                requester,
                scope_kind,
                outcome: ErasureOutcome::RateLimited,
                completed_at_ms: now_ms,
                entries_removed: 0,
                links_removed: 0,
                caches_purged: 0,
                proposals_purged: 0,
                verified: false,
                unprovable_remnants: ErasureRemnant::ALL.to_vec(),
            };
            self.push_erasure_report(report.clone());
            return Ok(ErasureStatus::Done(report));
        }

        if self.erasure.pending.len() >= MAX_PENDING_ERASURES {
            return Err(AdaptationError::Invariant(
                "too many open erasure transactions; finish one before starting another",
            ));
        }
        self.erasure.pending.push(PendingErasure {
            request_id: request_id.to_owned(),
            requester,
            scope_kind,
            actor,
            memory_ids: permit.memory_ids().to_vec(),
            state: ErasureState::ReadRevoked,
            removed_ids: HashSet::new(),
            entries_removed: 0,
            links_removed: 0,
        });
        Ok(ErasureStatus::Pending(ErasureState::ReadRevoked))
    }

    /// Advance an open transaction by exactly one state. A step that cannot
    /// be verified returns an error and leaves the state (and the revocation)
    /// where it was; it never advances past an unverified step and never
    /// re-enables reads.
    pub fn advance_erase(
        &mut self,
        request_id: &str,
        now_ms: u64,
    ) -> Result<ErasureStatus, AdaptationError> {
        let Some(index) = self
            .erasure
            .pending
            .iter()
            .position(|pending| pending.request_id == request_id)
        else {
            return match self
                .erasure
                .reports
                .iter()
                .find(|report| report.request_id == request_id)
            {
                Some(done) => Ok(ErasureStatus::Done(done.clone())),
                None => Err(AdaptationError::InvalidInput("unknown erase request id")),
            };
        };

        match self.erasure.pending[index].state {
            ErasureState::ReadRevoked => {
                let pending = self.erasure.pending[index].clone();
                let removed: Vec<u64> = self
                    .entries
                    .iter()
                    .filter(|entry| pending.covers(entry))
                    .map(|entry| entry.memory_id)
                    .collect();
                self.entries.retain(|entry| !pending.covers(entry));
                let slot = &mut self.erasure.pending[index];
                slot.entries_removed = removed.len();
                slot.removed_ids = removed.into_iter().collect();
                slot.state = ErasureState::CanonicalDeleted;
            }
            ErasureState::CanonicalDeleted => {
                let before = self.links.len();
                self.prune_links();
                let removed = before.saturating_sub(self.links.len());
                let slot = &mut self.erasure.pending[index];
                slot.links_removed = removed;
                slot.state = ErasureState::EdgesPruned;
            }
            // The store derives context per call and keeps no cache or
            // proposal queue, so these two states have nothing to purge; they
            // stay explicit so a future cache or proposal store must hook in
            // here before `Verified` can be reached.
            ErasureState::EdgesPruned => {
                self.erasure.pending[index].state = ErasureState::CachesPurged;
            }
            ErasureState::CachesPurged => {
                self.erasure.pending[index].state = ErasureState::ProposalsPurged;
            }
            ErasureState::ProposalsPurged => {
                let pending = &self.erasure.pending[index];
                let entry_left = self.entries.iter().any(|entry| pending.covers(entry));
                let link_left = self.links.iter().any(|link| {
                    pending.removed_ids.contains(&link.from)
                        || pending.removed_ids.contains(&link.to)
                });
                if entry_left || link_left {
                    return Err(AdaptationError::Invariant(
                        "erase post-condition failed; scope stays revoked",
                    ));
                }
                self.erasure.pending[index].state = ErasureState::Verified;
            }
            ErasureState::Verified => {
                let done = self.erasure.pending.remove(index);
                let report = ErasureReport {
                    vocab_version: ERASURE_VOCAB_VERSION.to_owned(),
                    request_id: done.request_id,
                    requester: done.requester,
                    scope_kind: done.scope_kind,
                    outcome: if done.entries_removed > 0 {
                        ErasureOutcome::Erased
                    } else {
                        ErasureOutcome::NoOp
                    },
                    completed_at_ms: now_ms,
                    entries_removed: done.entries_removed,
                    links_removed: done.links_removed,
                    caches_purged: 0,
                    proposals_purged: 0,
                    verified: true,
                    unprovable_remnants: ErasureRemnant::ALL.to_vec(),
                };
                self.erasure.completed = self.erasure.completed.saturating_add(1);
                self.erasure.entries_removed = self
                    .erasure
                    .entries_removed
                    .saturating_add(report.entries_removed as u64);
                self.erasure.links_removed = self
                    .erasure
                    .links_removed
                    .saturating_add(report.links_removed as u64);
                self.push_erasure_report(report.clone());
                return Ok(ErasureStatus::Done(report));
            }
        }
        Ok(ErasureStatus::Pending(self.erasure.pending[index].state))
    }

    fn push_erasure_report(&mut self, report: ErasureReport) {
        self.erasure.reports.push_back(report);
        while self.erasure.reports.len() > MAX_ERASURE_REPORTS {
            self.erasure.reports.pop_front();
        }
    }

    /// Per-pseudonym viewer rate limit on logical time. Bounded: expired
    /// records are dropped first, and a full table refuses rather than grows.
    fn admit_viewer_erase(&mut self, pseudonym: &str, now_ms: u64) -> bool {
        if let Some(last) = self.erasure.viewer_last.get(pseudonym)
            && now_ms.saturating_sub(*last) < VIEWER_ERASE_MIN_INTERVAL_MS
        {
            return false;
        }
        if !self.erasure.viewer_last.contains_key(pseudonym)
            && self.erasure.viewer_last.len() >= MAX_ERASE_RATE_ENTRIES
        {
            self.erasure
                .viewer_last
                .retain(|_, last| now_ms.saturating_sub(*last) < VIEWER_ERASE_MIN_INTERVAL_MS);
            if self.erasure.viewer_last.len() >= MAX_ERASE_RATE_ENTRIES {
                return false;
            }
        }
        self.erasure
            .viewer_last
            .insert(pseudonym.to_owned(), now_ms);
        true
    }
}
