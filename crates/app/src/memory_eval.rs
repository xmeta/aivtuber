//! Provider-free long-range memory evaluation (issue #105).
//!
//! The fixture is data, not Rust code, so the same inputs can be handed to an
//! external memory sidecar and both sides can produce a comparable report. The
//! runner here measures *this* runtime: every claim enters through the real
//! `authorize_memory_write` gate and the delivered context comes from
//! [`retained_memory_context`](crate::retained_memory_context), the function
//! the composition root calls for every generated reply.
//!
//! Two artifacts, both versioned:
//!
//! * [`MEMORY_EVAL_FIXTURE_SCHEMA_VERSION`] — the case document
//!   (`examples/memory-long-range/cases.json`): claims, their declared
//!   update and causal evidence, the query, and what the case expects to be
//!   delivered;
//! * [`MEMORY_EVAL_REPORT_SCHEMA_VERSION`] — the measured report: current-fact
//!   correctness, stale-memory use, absent violations, temporal-order errors,
//!   retained causal spans, delivered bytes/items against the domain bounds,
//!   and retention high-water. It is deterministic, so two runs are
//!   byte-identical and two revisions are comparable.
//!
//! A violation never aborts the run: it is recorded in
//! [`MemoryEvalOutcome::violations`] so a breach is reported with its evidence
//! instead of only failing the first case.

use aivtuber_adaptation::{
    ActorPseudonymizer, MemoryEntry, MemoryEvidence, MemoryLinkKind, WorkingMemory,
    WorkingMemoryConfig,
};
use aivtuber_domain::{
    AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION,
    EventEnvelope, EventKind, LocalControlIngress, MAX_THINKING_CONTEXT_BYTES,
    MAX_THINKING_CONTEXT_ITEMS, MAX_THINKING_TEXT_BYTES, OperatorCommandInput, SecurityPlane,
    SourceClass, TrustLevel,
};
use aivtuber_runtime::{MemoryWriteDecision, SecurityRuntime, SecurityRuntimeConfig};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::{SecretRedactor, canonical_json_digest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::retained_memory_context;

pub const MEMORY_EVAL_FIXTURE_SCHEMA_VERSION: &str = "memory-eval-v1";
pub const MEMORY_EVAL_REPORT_SCHEMA_VERSION: &str = "memory-long-range-eval/v1";

/// Fixture source namespace the claims are attributed to. It is part of the
/// actor-pseudonymization key, so it must not vary between compared runs.
pub const MEMORY_EVAL_SOURCE_NAMESPACE: &str = "public-chat";

/// The request the synthetic events make.
///
/// The runtime ranks retained claims by the request's topic, so a topic is
/// the retrieval signal. A request that carries no topic still needs a
/// non-empty retrieval request when an external service is asked, and this is
/// the request the synthetic event made.
pub const MEMORY_EVAL_REQUEST_TEXT: &str = "what happened next?";

/// The pseudonymization key version the evaluation runs under.
///
/// It is part of the actor scope a claim is filed under, so both sides of a
/// comparison must derive the same scope key: a claim filed under one scope
/// and looked up under another would measure nothing.
pub const MEMORY_EVAL_PSEUDONYM_KEY_VERSION: &str = "memory-eval-v1";

const ADMIN_SECRET: [u8; 32] = [0x21_u8; 32];

#[derive(Debug)]
pub enum MemoryEvalError {
    InvalidFixture(String),
    WriteGateDenied {
        case: String,
        claim: String,
    },
    Write(String),
    /// A memory sidecar refused or failed a call the measurement needed.
    Sidecar {
        case: String,
        message: String,
    },
    /// A measurement failure, followed by a failure to finish the run.
    ///
    /// Both are kept: the measurement error says why the run stopped, and the
    /// cleanup error says what the run may have left behind on the service. A
    /// report that named only one of them would hide either the cause or the
    /// state.
    SidecarCleanup {
        case: String,
        measurement: Box<MemoryEvalError>,
        cleanup: String,
    },
}

impl fmt::Display for MemoryEvalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFixture(message) => {
                write!(formatter, "invalid memory-eval fixture: {message}")
            }
            Self::WriteGateDenied { case, claim } => write!(
                formatter,
                "case {case}: claim {claim} did not pass the memory-write gate"
            ),
            Self::Write(message) => write!(formatter, "durable write failed: {message}"),
            Self::Sidecar { case, message } => {
                write!(formatter, "case {case}: memory sidecar failed: {message}")
            }
            Self::SidecarCleanup {
                case,
                measurement,
                cleanup,
            } => write!(
                formatter,
                "case {case}: {measurement}; the run could not be cleaned up: {cleanup}"
            ),
        }
    }
}

impl Error for MemoryEvalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SidecarCleanup { measurement, .. } => Some(measurement.as_ref()),
            _ => None,
        }
    }
}

/// A case document: many synthetic long-range memory cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEvalFixture {
    pub schema_version: String,
    pub cases: Vec<MemoryEvalCase>,
}

impl MemoryEvalFixture {
    /// Refuse a fixture that could not be compared meaningfully: unknown or
    /// duplicate node ids, expectations naming a claim that does not exist, or
    /// a case that requires and refuses the same claim.
    pub fn validate(&self) -> Result<(), MemoryEvalError> {
        if self.schema_version != MEMORY_EVAL_FIXTURE_SCHEMA_VERSION {
            return Err(MemoryEvalError::InvalidFixture(format!(
                "expected schema_version {MEMORY_EVAL_FIXTURE_SCHEMA_VERSION}, got {:?}",
                self.schema_version
            )));
        }
        if self.cases.is_empty() {
            return Err(MemoryEvalError::InvalidFixture(
                "a fixture needs at least one case".to_owned(),
            ));
        }
        let mut case_ids = BTreeSet::new();
        for case in &self.cases {
            let fail = |message: String| {
                MemoryEvalError::InvalidFixture(format!("case {:?}: {message}", case.id))
            };
            if case.id.trim().is_empty() {
                return Err(MemoryEvalError::InvalidFixture(
                    "case ids must not be empty".to_owned(),
                ));
            }
            if !case_ids.insert(case.id.as_str()) {
                return Err(fail("duplicate case id".to_owned()));
            }
            if case.claims.is_empty() {
                return Err(fail("a case needs at least one claim".to_owned()));
            }
            let mut claim_ids = BTreeSet::new();
            for claim in &case.claims {
                if claim.event_id.trim().is_empty() || !claim_ids.insert(claim.event_id.as_str()) {
                    return Err(fail(format!(
                        "claim id {:?} is empty or repeated",
                        claim.event_id
                    )));
                }
                if claim.text.is_empty() {
                    return Err(fail(format!("claim {:?} has empty text", claim.event_id)));
                }
                if claim
                    .text_bytes
                    .is_some_and(|bytes| bytes == 0 || !claim.text.is_ascii())
                {
                    return Err(fail(format!(
                        "claim {:?} text_bytes requires a non-empty ASCII seed",
                        claim.event_id
                    )));
                }
                if let Some(target) = claim.supersedes.as_deref()
                    && !case.claims.iter().any(|other| other.event_id == target)
                {
                    return Err(fail(format!(
                        "claim {:?} supersedes unknown claim {target:?}",
                        claim.event_id
                    )));
                }
                if let Some(target) = claim.caused_by.as_deref()
                    && !case.claims.iter().any(|other| other.event_id == target)
                {
                    return Err(fail(format!(
                        "claim {:?} is caused by unknown claim {target:?}",
                        claim.event_id
                    )));
                }
            }
            for id in case
                .must_deliver
                .iter()
                .chain(&case.must_not_deliver)
                .chain(&case.expected_delivery_order)
            {
                if !claim_ids.contains(id.as_str()) {
                    return Err(fail(format!("expectation names unknown claim {id:?}")));
                }
            }
            for id in &case.stale {
                if !case.must_not_deliver.contains(id) {
                    return Err(fail(format!(
                        "stale claim {id:?} must also be in must_not_deliver"
                    )));
                }
            }
            for id in &case.must_deliver {
                if case.must_not_deliver.contains(id) {
                    return Err(fail(format!("claim {id:?} is both required and refused")));
                }
            }
            if let Some(expected) = case.expected_retained_causal_spans {
                let declared = case
                    .claims
                    .iter()
                    .filter(|claim| claim.caused_by.is_some())
                    .count();
                if expected > declared {
                    return Err(fail(format!(
                        "expected_retained_causal_spans {expected} exceeds the {declared} declared \
                         causal edge(s) in the case"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// One synthetic case. `event_at_ms` is ground truth the store deliberately
/// does not keep — the gap is what the temporal-order metric measures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEvalCase {
    pub id: String,
    #[serde(default)]
    pub config: MemoryEvalConfig,
    pub claims: Vec<MemoryEvalClaim>,
    pub query: MemoryEvalQuery,
    /// Claims this query must receive: current, in-scope evidence.
    #[serde(default)]
    pub must_deliver: Vec<String>,
    /// Claims that must never reach generation (superseded, another viewer's,
    /// expired, or cut by the budget).
    #[serde(default)]
    pub must_not_deliver: Vec<String>,
    /// The subset of `must_not_deliver` refused because a retained
    /// supersession replaced it.
    #[serde(default)]
    pub stale: Vec<String>,
    /// Ground-truth inversion count this case exhibits. Declared rather than
    /// derived so a change in ordering semantics is visible.
    #[serde(default)]
    pub expected_temporal_errors: usize,
    /// Exact delivered order, when the case pins it. Empty means "membership
    /// only".
    #[serde(default)]
    pub expected_delivery_order: Vec<String>,
    /// How many retained causal edges must be usable — both endpoints
    /// delivered — for the claims this case declares through `caused_by`
    /// (#105). `None` measures without pinning; `Some` makes a lost edge
    /// visible, because a causal edge never changes which claim is current and
    /// would otherwise leave no trace in these metrics.
    #[serde(default)]
    pub expected_retained_causal_spans: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemoryEvalConfig {
    pub max_entries: Option<usize>,
    pub durable_ttl_ms: Option<u64>,
    pub max_claim_bytes: Option<usize>,
    pub max_links: Option<usize>,
    pub max_links_per_memory: Option<usize>,
}

impl MemoryEvalConfig {
    /// The store configuration a case's bounds describe.
    pub fn apply(self) -> WorkingMemoryConfig {
        let defaults = WorkingMemoryConfig::default();
        WorkingMemoryConfig {
            max_entries: self.max_entries.unwrap_or(defaults.max_entries),
            durable_ttl_ms: self.durable_ttl_ms.unwrap_or(defaults.durable_ttl_ms),
            max_claim_bytes: self.max_claim_bytes.unwrap_or(defaults.max_claim_bytes),
            max_links: self.max_links.unwrap_or(defaults.max_links),
            max_links_per_memory: self
                .max_links_per_memory
                .unwrap_or(defaults.max_links_per_memory),
            ..defaults
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEvalClaim {
    pub event_id: String,
    #[serde(default)]
    pub actor: Option<String>,
    /// Claim text (or an ASCII seed tiled to `text_bytes`).
    pub text: String,
    /// Tile `text` to exactly this many ASCII bytes. Keeps byte-budget cases
    /// readable in the fixture without a multi-kilobyte literal.
    #[serde(default)]
    pub text_bytes: Option<usize>,
    #[serde(default)]
    pub topic: Option<String>,
    pub recorded_at_ms: u64,
    /// Defaults to `recorded_at_ms`, which is the common case.
    #[serde(default)]
    pub event_at_ms: Option<u64>,
    #[serde(default)]
    pub supersedes: Option<String>,
    /// A retained claim that brought this one about (#105). The store resolves
    /// it under the same rules as `supersedes`; the fixture declares it so a
    /// measurement can see whether retrieval delivers the reason with the
    /// effect.
    #[serde(default)]
    pub caused_by: Option<String>,
}

impl MemoryEvalClaim {
    /// When the claim's content happened. The store deliberately keeps no
    /// event time, so this is the ground truth the temporal-order metric
    /// measures against.
    pub fn event_at(&self) -> u64 {
        self.event_at_ms.unwrap_or(self.recorded_at_ms)
    }

    /// The claim text, tiled to `text_bytes` when the case asks for it.
    pub fn claim_text(&self) -> Result<String, MemoryEvalError> {
        let Some(bytes) = self.text_bytes else {
            return Ok(self.text.clone());
        };
        if bytes == 0 || !self.text.is_ascii() {
            return Err(MemoryEvalError::InvalidFixture(format!(
                "claim {:?} text_bytes requires a non-empty ASCII seed",
                self.event_id
            )));
        }
        let mut text = String::with_capacity(bytes);
        while text.len() < bytes {
            let remaining = bytes - text.len();
            let take = self.text.len().min(remaining);
            text.push_str(&self.text[..take]);
        }
        Ok(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEvalQuery {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    pub now_ms: u64,
    /// The request's own event text, charged before any retained claim.
    #[serde(default)]
    pub charged_bytes: usize,
}

/// One claim that reached generation.
///
/// The delivered text and the retained text are kept apart on purpose: a
/// claim that reaches generation with text other than the text the project
/// retained is a *shortened* claim, and the comparison has to see that
/// difference rather than trust a count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveredClaim {
    /// The project's claim id.
    pub claim_id: String,
    /// The text that reached generation.
    pub delivered_text: String,
    /// The text the project retained for the claim.
    pub retained_text: String,
}

/// Why the project refused an item a service answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// The item named a claim of somebody else: the service answered outside
    /// the scope the request asked for.
    OutOfScope,
    /// The item named a claim the project had already retired: the service
    /// kept content the project removed.
    Removed,
    /// The item named no claim the project admitted.
    Unknown,
}

/// One item a service answered with that the project refused.
///
/// The project filters what it accepts, so a refused item never reaches
/// generation. It is still recorded: the service's answer is the evidence
/// for whether the service kept content the project removed or widened the
/// scope the request asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusedItem {
    /// The claim the item named, when it named one.
    #[serde(default)]
    pub claim_id: Option<String>,
    pub reason: RefusalReason,
}

/// A causal edge between two project claims (#105).
///
/// `effect` is the claim that was caused; `cause` is the retained claim that
/// brought it about. The direction is carried explicitly and never inferred
/// from delivery order, so a measurement cannot count a reversed pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CausalEdge {
    pub effect: String,
    pub cause: String,
}

/// The causal pairs a case's claims declare, as claim-id edges.
///
/// This is the fixture's *claim*, not evidence: two claims can declare a pair
/// while the store holds no edge for it — the gate refused the write, the
/// edge was never minted, or it was lost — so a measurement of this runtime
/// never counts a span from this list. It is what a comparison has to work
/// with, because an external service is handed claims and not relations.
pub fn declared_causal_edges(case: &MemoryEvalCase) -> Vec<CausalEdge> {
    case.claims
        .iter()
        .filter_map(|claim| {
            claim.caused_by.as_ref().map(|cause| CausalEdge {
                effect: claim.event_id.clone(),
                cause: cause.clone(),
            })
        })
        .collect()
}

/// Declared causal pairs whose two members both reached generation.
///
/// The comparison's causal metric: rows on both sides are re-derived from the
/// fixture's declarations and the delivered set, never read from a report's
/// own causal count, because a report's number is a claim about a store the
/// comparison was not handed. What it can show is that the answer returned both
/// members of a pair the fixture declares — not that it carried the relation
/// itself, which the port never hands a service; the fixture declarations are
/// the only causal statement either side of the comparison has.
///
/// `delivered` compares against the declared edge direction: an edge counts
/// once, when *both* of its endpoints are in the delivered set.
pub fn declared_causal_pairs_delivered(case: &MemoryEvalCase, delivered: &[String]) -> usize {
    declared_causal_edges(case)
        .into_iter()
        .filter(|edge| delivered.contains(&edge.effect) && delivered.contains(&edge.cause))
        .count()
}

/// The causal edges the store retains and would hand out at `now_ms` (#105).
///
/// Read through [`WorkingMemory::link_snapshot`] — the same versioned view the
/// runtime persists and replays — so this is evidence and not declaration:
/// only `CausedBy` edges whose endpoints are both live at the read timestamp
/// are returned, and an edge that was never retained, whose cause was evicted,
/// or that was recorded after the read cannot be counted. Node ids are mapped
/// back to the source event ids the fixture and the reports name, so an edge
/// is only usable when both of its claims are still identifiable.
pub fn retained_causal_edges(memory: &WorkingMemory, now_ms: u64) -> Vec<CausalEdge> {
    let claim_ids: BTreeMap<u64, &str> = memory
        .entries()
        .iter()
        .map(|entry| (entry.memory_id, entry.source.event_id.as_str()))
        .collect();
    memory
        .link_snapshot(now_ms)
        .links
        .into_iter()
        .filter(|link| link.kind == MemoryLinkKind::CausedBy)
        .filter_map(|link| {
            Some(CausalEdge {
                effect: claim_ids.get(&link.from)?.to_string(),
                cause: claim_ids.get(&link.to)?.to_string(),
            })
        })
        .collect()
}

/// One case's delivery, in the shape both sides of a comparison report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaseDelivery {
    /// Claims that reached generation, in delivery order.
    pub claims: Vec<DeliveredClaim>,
    /// Items a service answered with that the project refused.
    pub refused: Vec<RefusedItem>,
    /// The store's own causal evidence for these claims, read from the
    /// retained links ([`retained_causal_edges`]). The runtime fills it from
    /// the store; a service retains no project edge, so its delivery carries
    /// none — which is the truth about the evidence it kept, not a missing
    /// measurement.
    pub causal_edges: Vec<CausalEdge>,
}

/// The retention numbers a report carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSnapshot {
    pub entries_high_water: usize,
    pub links_high_water: usize,
    pub max_entries: usize,
    pub max_links: usize,
}

/// Which fixture expectations a measurement enforces.
///
/// The runtime enforces every expectation: the fixture pins the runtime's own
/// behavior, so a difference is a defect. A candidate is measured instead of
/// pinned: a different delivery order or a different temporal-error count is
/// the result under test, not a failure of the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectationPolicy {
    /// Whether the delivered order must match the case's pinned order.
    pub enforce_delivery_order: bool,
    /// Whether the temporal-error count must match the case's expectation.
    pub enforce_temporal_count: bool,
    /// Whether the causal-span count must match the case's expectation. The
    /// store's own delivery is pinned; a candidate's retrieval is the result
    /// under test, so it is measured without being declared a failure.
    pub enforce_causal_spans: bool,
}

impl ExpectationPolicy {
    /// The runtime's own expectations: every one of them.
    pub const RUNTIME: Self = Self {
        enforce_delivery_order: true,
        enforce_temporal_count: true,
        enforce_causal_spans: true,
    };
    /// A candidate's expectations: the quality metrics are the result, not a
    /// failure of the candidate.
    pub const CANDIDATE: Self = Self {
        enforce_delivery_order: false,
        enforce_temporal_count: false,
        enforce_causal_spans: false,
    };
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseReport {
    pub case: String,
    pub delivered: Vec<String>,
    pub must_deliver: Vec<String>,
    /// The claims this case refused, carried so a comparison can re-derive
    /// `absent_violations` from what was actually delivered instead of
    /// trusting a candidate's own count.
    pub refused_claims: Vec<String>,
    pub stale_claims: Vec<String>,
    /// Retained causal evidence the request could use: `CausedBy` edges the
    /// store actually holds, with both endpoints delivered. Read from the
    /// store's own link view rather than from the fixture's declaration, so
    /// losing the edge lowers this even while both claims still reach
    /// generation.
    ///
    /// This measures the *store* half — the edge is retained and both of its
    /// claims are in the request. The generation input itself carries claims,
    /// not links (`ThinkingContent` has no edge field), so whether a reply can
    /// state the relation is not measured here and is not claimed. Additive,
    /// so a report written before this field existed still parses.
    #[serde(default)]
    pub retained_causal_spans: usize,
    pub current_fact_correctness: f64,
    pub stale_memory_use: usize,
    pub absent_violations: usize,
    /// Items the service answered with that the project refused: they fell
    /// outside the requested actor scope, they named a claim the project had
    /// already retired, or they named no claim the project admitted. A
    /// service that widens the scope or keeps removed content shows up here;
    /// a report written before this field existed still parses.
    #[serde(default)]
    pub refused_items: Vec<RefusedItem>,
    pub temporal_order_errors: usize,
    pub context_items: usize,
    pub claim_bytes: usize,
    pub request_bytes: usize,
    pub within_item_budget: bool,
    pub within_byte_budget: bool,
    pub truncated_claims: usize,
    pub entries_high_water: usize,
    pub links_high_water: usize,
    pub max_entries: usize,
    pub max_links: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub cases: usize,
    pub current_fact_correctness: f64,
    pub stale_memory_use: usize,
    pub absent_violations: usize,
    /// Retained causal edges the requests could use (#105).
    #[serde(default)]
    pub retained_causal_spans: usize,
    /// Items a service answered with that the project refused.
    #[serde(default)]
    pub refused_items: usize,
    pub temporal_order_errors: usize,
    pub truncated_claims: usize,
    pub max_context_items: usize,
    pub max_claim_bytes: usize,
    pub max_request_bytes: usize,
    pub max_entries_high_water: usize,
    pub max_links_high_water: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReport {
    pub schema_version: String,
    pub fixture_schema_version: String,
    /// Canonical digest of the case document this report measured (#105).
    ///
    /// The version string cannot tell two revisions of the same schema apart,
    /// so a comparison needs the document itself, not its name: a report is
    /// only comparable with a fixture it can prove it measured. The rule is
    /// [`fixture_digest`]'s, and it is the canonical form
    /// [`aivtuber_telemetry::canonical_json_digest`] documents, so a producer
    /// in another process can derive the same value.
    #[serde(default)]
    pub fixture_digest: String,
    pub context_item_budget: usize,
    pub context_byte_budget: usize,
    pub item_byte_budget: usize,
    pub totals: Totals,
    pub cases: Vec<CaseReport>,
}

/// The measured report plus every expectation the run missed.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEvalOutcome {
    pub report: EvalReport,
    pub violations: Vec<String>,
}

/// Run every case in `fixture` and return the report with its violations.
///
/// The fixture must already be validated ([`MemoryEvalFixture::validate`]);
/// evaluation repeats the structural checks it needs so a caller that skips
/// validation still gets an error rather than a panic.
pub fn evaluate(fixture: &MemoryEvalFixture) -> Result<MemoryEvalOutcome, MemoryEvalError> {
    fixture.validate()?;
    let admin = memory_admin();
    let mut cases = Vec::with_capacity(fixture.cases.len());
    let mut violations = Vec::new();
    for case in &fixture.cases {
        let (report, case_violations) = run_case(case, &admin)?;
        violations.extend(case_violations);
        cases.push(report);
    }
    Ok(MemoryEvalOutcome {
        report: report_for(fixture, cases),
        violations,
    })
}
/// The report a set of measured cases produces.
pub fn outcome_for(
    fixture: &MemoryEvalFixture,
    cases: Vec<CaseReport>,
    violations: Vec<String>,
) -> MemoryEvalOutcome {
    MemoryEvalOutcome {
        report: report_for(fixture, cases),
        violations,
    }
}

/// The canonical digest of a case document (#105).
///
/// A fixture is plain JSON by construction — strings, bounded integers, arrays
/// and maps — so canonicalizing it cannot fail; a document that somehow could
/// not be digested would be a programming error and not a measurement result.
/// The digest names the *document*, not its version: two revisions of
/// `memory-eval-v1` that change one `caused_by` are different documents, and a
/// report measured against one of them is not comparable with the other.
pub fn fixture_digest(fixture: &MemoryEvalFixture) -> String {
    canonical_json_digest(&serde_json::to_value(fixture).expect("a fixture serializes to JSON"))
}

/// The report envelope both sides of a comparison emit.
pub fn report_for(fixture: &MemoryEvalFixture, cases: Vec<CaseReport>) -> EvalReport {
    EvalReport {
        schema_version: MEMORY_EVAL_REPORT_SCHEMA_VERSION.to_owned(),
        fixture_schema_version: fixture.schema_version.clone(),
        fixture_digest: fixture_digest(fixture),
        context_item_budget: MAX_THINKING_CONTEXT_ITEMS,
        context_byte_budget: MAX_THINKING_CONTEXT_BYTES,
        item_byte_budget: MAX_THINKING_TEXT_BYTES,
        totals: totals_for(&cases),
        cases,
    }
}

/// The aggregate of a report's case rows.
///
/// Both sides of a comparison derive their totals the same way, so a
/// difference in the aggregate is a difference in the cases and never in the
/// arithmetic.
pub fn totals_for(cases: &[CaseReport]) -> Totals {
    let case_count = cases.len();
    Totals {
        cases: case_count,
        current_fact_correctness: cases
            .iter()
            .map(|case| case.current_fact_correctness)
            .sum::<f64>()
            / case_count as f64,
        stale_memory_use: cases.iter().map(|case| case.stale_memory_use).sum(),
        absent_violations: cases.iter().map(|case| case.absent_violations).sum(),
        retained_causal_spans: cases.iter().map(|case| case.retained_causal_spans).sum(),
        refused_items: cases.iter().map(|case| case.refused_items.len()).sum(),
        temporal_order_errors: cases.iter().map(|case| case.temporal_order_errors).sum(),
        truncated_claims: cases.iter().map(|case| case.truncated_claims).sum(),
        max_context_items: cases
            .iter()
            .map(|case| case.context_items)
            .max()
            .unwrap_or(0),
        max_claim_bytes: cases.iter().map(|case| case.claim_bytes).max().unwrap_or(0),
        max_request_bytes: cases
            .iter()
            .map(|case| case.request_bytes)
            .max()
            .unwrap_or(0),
        max_entries_high_water: cases
            .iter()
            .map(|case| case.entries_high_water)
            .max()
            .unwrap_or(0),
        max_links_high_water: cases
            .iter()
            .map(|case| case.links_high_water)
            .max()
            .unwrap_or(0),
    }
}

/// Measure one case's delivery against the case's expectations.
///
/// This is the one measurement both sides of a comparison run: the runtime's
/// own delivery and a sidecar's answer are measured by the same code, so a
/// difference in the numbers is a difference in the retrieval and never in
/// the metric.
pub fn measure_case(
    case: &MemoryEvalCase,
    delivery: &CaseDelivery,
    retention: &RetentionSnapshot,
) -> CaseReport {
    let event_at: BTreeMap<&str, u64> = case
        .claims
        .iter()
        .map(|claim| (claim.event_id.as_str(), claim.event_at()))
        .collect();
    let delivered: Vec<String> = delivery
        .claims
        .iter()
        .map(|claim| claim.claim_id.clone())
        .collect();
    let truncated_claims = delivery
        .claims
        .iter()
        .filter(|claim| claim.delivered_text != claim.retained_text)
        .count();
    let current_hits = case
        .must_deliver
        .iter()
        .filter(|required| delivered.contains(required))
        .count();
    let current_fact_correctness = if case.must_deliver.is_empty() {
        1.0
    } else {
        current_hits as f64 / case.must_deliver.len() as f64
    };
    let stale_memory_use = delivered
        .iter()
        .filter(|id| case.stale.contains(id))
        .count();
    let absent_violations = delivered
        .iter()
        .filter(|id| case.must_not_deliver.contains(id))
        .count();
    let temporal_order_errors = delivered
        .windows(2)
        .filter(|pair| event_at[pair[1].as_str()] > event_at[pair[0].as_str()])
        .count();
    // A causal span is an edge the *store* retains whose two endpoints both
    // reached generation: a relation to a claim the request leaves behind
    // explains nothing, and a pair that only the fixture declares is not
    // evidence the store ever kept. Reading the retained edges is what makes
    // a lost link visible; measuring the declaration instead would report the
    // fixture's intent back and never notice the loss (#105 review round 13).
    let retained_causal_spans = delivery
        .causal_edges
        .iter()
        .filter(|edge| delivered.contains(&edge.effect) && delivered.contains(&edge.cause))
        .count();

    let claim_bytes: usize = delivery
        .claims
        .iter()
        .map(|claim| claim.delivered_text.len())
        .sum();
    let request_bytes = case.query.charged_bytes.saturating_add(claim_bytes);
    CaseReport {
        case: case.id.clone(),
        delivered,
        must_deliver: case.must_deliver.clone(),
        refused_claims: case.must_not_deliver.clone(),
        stale_claims: case.stale.clone(),
        retained_causal_spans,
        current_fact_correctness,
        stale_memory_use,
        absent_violations,
        refused_items: delivery.refused.clone(),
        temporal_order_errors,
        context_items: delivery.claims.len(),
        claim_bytes,
        request_bytes,
        within_item_budget: delivery.claims.len() <= MAX_THINKING_CONTEXT_ITEMS
            && delivery
                .claims
                .iter()
                .all(|claim| claim.delivered_text.len() <= MAX_THINKING_TEXT_BYTES),
        within_byte_budget: request_bytes <= MAX_THINKING_CONTEXT_BYTES,
        truncated_claims,
        entries_high_water: retention.entries_high_water,
        links_high_water: retention.links_high_water,
        max_entries: retention.max_entries,
        max_links: retention.max_links,
    }
}

/// The expectations a measurement missed.
///
/// A violation never aborts a run: it is recorded so a breach is reported
/// with its evidence instead of only failing the first case.
pub fn case_violations(
    case: &MemoryEvalCase,
    report: &CaseReport,
    policy: ExpectationPolicy,
) -> Vec<String> {
    let mut violations = Vec::new();
    let push = |violations: &mut Vec<String>, message: String| {
        violations.push(format!("case {}: {message}", case.id));
    };
    for required in &case.must_deliver {
        if !report.delivered.contains(required) {
            push(
                &mut violations,
                format!("required current claim {required:?} was not delivered"),
            );
        }
    }
    if report.stale_memory_use > 0 {
        push(
            &mut violations,
            format!(
                "{} superseded claim(s) reached generation",
                report.stale_memory_use
            ),
        );
    }
    if report.absent_violations > 0 {
        push(
            &mut violations,
            format!(
                "{} refused claim(s) reached generation",
                report.absent_violations
            ),
        );
    }
    if report.truncated_claims > 0 {
        push(
            &mut violations,
            format!(
                "{} shortened claim(s) reached generation",
                report.truncated_claims
            ),
        );
    }
    if !report.within_item_budget || !report.within_byte_budget {
        push(
            &mut violations,
            format!(
                "request bounds violated: {} items, {} bytes",
                report.context_items, report.request_bytes
            ),
        );
    }
    if !report.refused_items.is_empty() {
        push(
            &mut violations,
            format!(
                "{} item(s) the service answered with were refused by the project: {}",
                report.refused_items.len(),
                report
                    .refused_items
                    .iter()
                    .map(|item| format!(
                        "{:?} {}",
                        item.reason,
                        item.claim_id.as_deref().unwrap_or("<unnamed>")
                    ))
                    .collect::<Vec<String>>()
                    .join(", ")
            ),
        );
    }
    if policy.enforce_temporal_count
        && report.temporal_order_errors != case.expected_temporal_errors
    {
        push(
            &mut violations,
            format!(
                "temporal_order_errors {}, expected {}",
                report.temporal_order_errors, case.expected_temporal_errors
            ),
        );
    }
    if report.entries_high_water > report.max_entries || report.links_high_water > report.max_links
    {
        push(
            &mut violations,
            format!(
                "retention exceeded its bounds: {} entries / {} links",
                report.entries_high_water, report.links_high_water
            ),
        );
    }
    if policy.enforce_delivery_order
        && !case.expected_delivery_order.is_empty()
        && report.delivered != case.expected_delivery_order
    {
        push(
            &mut violations,
            format!(
                "delivered order {:?}, expected {:?}",
                report.delivered, case.expected_delivery_order
            ),
        );
    }
    if policy.enforce_causal_spans
        && let Some(expected) = case.expected_retained_causal_spans
        && report.retained_causal_spans != expected
    {
        push(
            &mut violations,
            format!(
                "retained causal spans {}, expected {expected}",
                report.retained_causal_spans
            ),
        );
    }
    violations
}

/// Write a case's claims into a fresh store, exactly the way the runtime
/// does: through the real `authorize_memory_write` gate, with both declared
/// relations riding the permit's own path.
///
/// This is the store whose delivery a case measures, and the seam the causal
/// regression tests read the retained edges back from: what the store holds
/// is the evidence, and what the fixture declares is only the request.
fn write_case_claims(
    case: &MemoryEvalCase,
    admin: &AuthenticatedControl,
) -> Result<WorkingMemory, MemoryEvalError> {
    let mut security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig::default(),
        SecretRedactor::default(),
        None,
    )
    .map_err(|error| MemoryEvalError::Write(error.to_string()))?;
    let mut memory = WorkingMemory::new(case.config.apply(), eval_pseudonymizer())
        .map_err(|error| MemoryEvalError::Write(error.to_string()))?;

    for claim in &case.claims {
        let text = claim.claim_text()?;
        let mut envelope = event(&claim.event_id, claim.actor.as_deref());
        envelope
            .payload
            .insert("text".to_owned(), Value::String(text.clone()));
        let (decision, permit) = security.authorize_memory_write(&envelope, Some(admin));
        if decision == MemoryWriteDecision::Denied {
            return Err(MemoryEvalError::WriteGateDenied {
                case: case.id.clone(),
                claim: claim.event_id.clone(),
            });
        }
        let permit = permit.ok_or_else(|| MemoryEvalError::WriteGateDenied {
            case: case.id.clone(),
            claim: claim.event_id.clone(),
        })?;
        // Both declarations ride the same permit-gated path the runtime uses,
        // so a fixture cannot write evidence the gate would have refused.
        let supersedes: Vec<&str> = claim.supersedes.iter().map(String::as_str).collect();
        let caused_by: Vec<&str> = claim.caused_by.iter().map(String::as_str).collect();
        let written = memory.remember_durable_with_evidence(
            &permit,
            &text,
            claim.topic.as_deref(),
            claim.recorded_at_ms,
            MemoryEvidence {
                supersedes: &supersedes,
                caused_by: &caused_by,
            },
        );
        written.map_err(|error| MemoryEvalError::Write(format!("{}: {error}", claim.event_id)))?;
    }
    Ok(memory)
}

fn run_case(
    case: &MemoryEvalCase,
    admin: &AuthenticatedControl,
) -> Result<(CaseReport, Vec<String>), MemoryEvalError> {
    let config = case.config.apply();
    let memory = write_case_claims(case, admin)?;

    let context = retained_memory_context(
        &memory,
        MEMORY_EVAL_SOURCE_NAMESPACE,
        case.query.actor.as_deref(),
        case.query.topic.as_deref(),
        case.query.now_ms,
        case.query.charged_bytes,
    );

    let mut delivery = CaseDelivery::default();
    for item in &context {
        let event_id = item.event_id.as_deref().ok_or_else(|| {
            MemoryEvalError::InvalidFixture(format!(
                "case {:?}: retained-memory content without an event id",
                case.id
            ))
        })?;
        let claim = case
            .claims
            .iter()
            .find(|claim| claim.event_id == event_id)
            .ok_or_else(|| {
                MemoryEvalError::InvalidFixture(format!(
                    "case {:?}: delivered unknown claim {event_id:?}",
                    case.id
                ))
            })?;
        delivery.claims.push(DeliveredClaim {
            claim_id: claim.event_id.clone(),
            delivered_text: item.text.clone(),
            retained_text: stored_claim(&memory, event_id)
                .unwrap_or_default()
                .to_owned(),
        });
    }

    // The store's own causal evidence, read back at the query's timestamp: a
    // claim pair only counts as a delivered span when the store holds the edge
    // and retrieval made both endpoints usable.
    delivery.causal_edges = retained_causal_edges(&memory, case.query.now_ms);

    let metrics = memory.retention_metrics();
    let retention = RetentionSnapshot {
        entries_high_water: metrics.entries_high_water,
        links_high_water: metrics.links_high_water,
        max_entries: config.max_entries,
        max_links: config.max_links,
    };
    let report = measure_case(case, &delivery, &retention);
    let violations = case_violations(case, &report, ExpectationPolicy::RUNTIME);
    Ok((report, violations))
}

/// The pseudonymizer every side of a comparison derives its scope keys with.
///
/// A claim filed under one scope key and looked up under another would make a
/// comparison measure nothing, so the runtime path and the sidecar path share
/// this one derivation.
pub fn eval_pseudonymizer() -> ActorPseudonymizer {
    ActorPseudonymizer::new(MEMORY_EVAL_PSEUDONYM_KEY_VERSION, [0x5a; 32])
        .expect("fixed evaluation key")
}

/// The text the project retained for a claim.
///
/// The retained text is the reference a delivered claim is compared against:
/// a claim that reaches generation with other text than this is a shortened
/// claim, whatever the reason.
pub fn stored_claim<'a>(memory: &'a WorkingMemory, event_id: &str) -> Option<&'a str> {
    memory
        .entries()
        .iter()
        .find(|entry: &&MemoryEntry| entry.source.event_id == event_id)
        .map(|entry| entry.normalized_claim.as_str())
}

/// The synthetic event a case's claim arrives as.
///
/// Every claim in the fixture is a public chat message, so the envelope is
/// the same for all of them: the harness measures the runtime and the
/// sidecar against one shape of evidence.
pub fn event(id: &str, actor: Option<&str>) -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: id.to_owned(),
        correlation_id: format!("corr-{id}"),
        sequence: id.bytes().map(u64::from).sum(),
        observed_at: "2026-10-20T00:00:00Z".to_owned(),
        source: MEMORY_EVAL_SOURCE_NAMESPACE.to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: actor.map(str::to_owned),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([(
            "text".to_owned(),
            Value::String(MEMORY_EVAL_REQUEST_TEXT.to_owned()),
        )]),
    }
}

/// The operator authority the evaluation's durable writes are made under.
///
/// The memory-write gate mints its permit from an authenticated local
/// ingress, and the gate only mints one for an authenticated caller, so the
/// evaluation authenticates exactly like the runtime does.
pub fn memory_admin() -> AuthenticatedControl {
    let ingress = LocalControlIngress::new(
        "memory-eval",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([Capability::MemoryAdmin]),
        ControlSecret::new(ADMIN_SECRET),
    )
    .expect("fixed evaluation ingress");
    ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-memory-eval-admin".to_owned(),
                correlation_id: "corr-memory-eval-admin".to_owned(),
                sequence: 1,
                observed_at: "2026-10-20T00:00:00Z".to_owned(),
                action: "memory.admin".to_owned(),
                payload: BTreeMap::new(),
            },
            &ADMIN_SECRET,
        )
        .expect("authenticated evaluation admin")
        .authority()
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_adaptation::MemoryLink;

    /// The published fixture, read from the file every run measures.
    fn published_fixture() -> MemoryEvalFixture {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/memory-long-range/cases.json");
        let bytes =
            std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

    fn causal_case(fixture: &MemoryEvalFixture) -> &MemoryEvalCase {
        fixture
            .cases
            .iter()
            .find(|case| case.id == "causal_reason_travels_with_its_effect")
            .expect("the published fixture keeps the causal case")
    }

    fn retention_for(memory: &WorkingMemory, case: &MemoryEvalCase) -> RetentionSnapshot {
        let metrics = memory.retention_metrics();
        let config = case.config.apply();
        RetentionSnapshot {
            entries_high_water: metrics.entries_high_water,
            links_high_water: metrics.links_high_water,
            max_entries: config.max_entries,
            max_links: config.max_links,
        }
    }

    /// A delivery of the named claims carrying the causal evidence supplied.
    fn delivery(
        memory: &WorkingMemory,
        delivered: &[&str],
        causal_edges: Vec<CausalEdge>,
    ) -> CaseDelivery {
        CaseDelivery {
            claims: delivered
                .iter()
                .map(|id| DeliveredClaim {
                    claim_id: (*id).to_owned(),
                    delivered_text: stored_claim(memory, id).unwrap_or_default().to_owned(),
                    retained_text: stored_claim(memory, id).unwrap_or_default().to_owned(),
                })
                .collect(),
            refused: Vec::new(),
            causal_edges,
        }
    }

    /// The store retains the declared edge, with the effect on the `from` end,
    /// and hands it out only while both of its endpoints are live.
    #[test]
    fn the_store_retains_the_declared_causal_edge() {
        let fixture = published_fixture();
        let case = causal_case(&fixture);
        let memory = write_case_claims(case, &memory_admin()).expect("the case writes");

        assert_eq!(
            retained_causal_edges(&memory, case.query.now_ms),
            [CausalEdge {
                effect: "evt-boss-retry".to_owned(),
                cause: "evt-boss-bug".to_owned(),
            }],
            "the metric reads the store's own edge, not the fixture's declaration"
        );

        // The edge is a stored link between the two retained claims, with the
        // effect as `from` — the direction the write path defines and the one
        // a reader of the retained evidence sees.
        let effect = memory
            .entries()
            .iter()
            .find(|entry| entry.source.event_id == "evt-boss-retry")
            .expect("the effect is retained");
        let cause = memory
            .entries()
            .iter()
            .find(|entry| entry.source.event_id == "evt-boss-bug")
            .expect("the cause is retained");
        let stored: Vec<&MemoryLink> = memory
            .links()
            .iter()
            .filter(|link| link.kind == MemoryLinkKind::CausedBy)
            .collect();
        assert_eq!(stored.len(), 1, "one declared cause is one edge");
        assert_eq!(
            (stored[0].from, stored[0].to),
            (effect.memory_id, cause.memory_id)
        );

        // Evidence expires with its endpoints: at the timestamp the cause is no
        // longer live, the same read hands out no edge.
        assert!(
            retained_causal_edges(&memory, cause.expires_at_ms).is_empty(),
            "an edge is usable only while both endpoints are live"
        );
    }

    /// The review's falsifier: both claims delivered, the retained edge gone.
    ///
    /// Nothing else in the report moves — the pair still reaches generation —
    /// so the metric and the case's own pin are the only things that can see
    /// the loss, and both must.
    #[test]
    fn a_delivered_pair_without_a_retained_edge_is_not_a_causal_span() {
        let fixture = published_fixture();
        let case = causal_case(&fixture);
        let memory = write_case_claims(case, &memory_admin()).expect("the case writes");
        let retention = retention_for(&memory, case);
        // The order the store delivers in: newest record first.
        let delivered = ["evt-boss-retry", "evt-boss-bug"];

        let kept = delivery(
            &memory,
            &delivered,
            retained_causal_edges(&memory, case.query.now_ms),
        );
        let report = measure_case(case, &kept, &retention);
        assert_eq!(report.retained_causal_spans, 1);
        let violations = case_violations(case, &report, ExpectationPolicy::RUNTIME);
        assert!(violations.is_empty(), "{violations:?}");

        let lost = delivery(&memory, &delivered, Vec::new());
        let report = measure_case(case, &lost, &retention);
        assert_eq!(
            report.delivered, delivered,
            "both claims still reach generation"
        );
        assert_eq!(
            report.retained_causal_spans, 0,
            "a pair the store retains no edge for is not a causal span"
        );
        let violations = case_violations(case, &report, ExpectationPolicy::RUNTIME);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("retained causal spans 0, expected 1")),
            "the lost edge must fail the case: {violations:?}"
        );
    }

    /// A retained edge whose cause the request may not see explains nothing.
    ///
    /// The store keeps the caller's edge (the write path resolves the target,
    /// not the scope), and retrieval holds the scope rule at both endpoints,
    /// so the pair reaches the request un-usable — which is exactly what the
    /// span metric has to say and where "delivered" is decided, not here.
    #[test]
    fn a_cause_outside_the_request_scope_is_not_a_delivered_span() {
        let case: MemoryEvalCase = serde_json::from_value(serde_json::json!({
            "id": "cross_scope_cause",
            "claims": [
                {
                    "event_id": "evt-other",
                    "actor": "viewer-2",
                    "text": "viewer two saw the bug",
                    "topic": "game-x",
                    "recorded_at_ms": 1000
                },
                {
                    "event_id": "evt-mine",
                    "actor": "viewer-1",
                    "text": "viewer one retried the boss",
                    "topic": "game-x",
                    "recorded_at_ms": 2000,
                    "caused_by": "evt-other"
                }
            ],
            "query": { "actor": "viewer-1", "topic": "game-x", "now_ms": 3000, "charged_bytes": 16 },
            "must_deliver": ["evt-mine"]
        }))
        .expect("probe case");
        let admin = memory_admin();
        let memory = write_case_claims(&case, &admin).expect("the case writes");
        assert_eq!(
            retained_causal_edges(&memory, case.query.now_ms).len(),
            1,
            "the store keeps the edge the caller declared"
        );

        let (report, violations) = run_case(&case, &admin).expect("the case measures");
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(
            report.delivered,
            ["evt-mine"],
            "scope keeps the other viewer's claim out of the request"
        );
        assert_eq!(
            report.retained_causal_spans, 0,
            "an edge to a claim the request cannot see explains nothing"
        );
    }

    /// A case that declares nothing retains no edge, so the metric cannot be
    /// satisfied by the fixture alone.
    #[test]
    fn an_undeclared_pair_leaves_no_edge_to_count() {
        let mut fixture = published_fixture();
        let index = fixture
            .cases
            .iter()
            .position(|case| case.id == "causal_reason_travels_with_its_effect")
            .expect("the published fixture keeps the causal case");
        fixture.cases[index].claims[1].caused_by = None;

        // The pin is unsatisfiable without a declaration, so the fixture
        // contract refuses it before a run could measure zero and be told the
        // number was wrong.
        assert_eq!(fixture.cases[index].expected_retained_causal_spans, Some(1));
        let error = fixture
            .validate()
            .expect_err("an unsatisfiable pin is refused");
        assert!(
            error.to_string().contains("expected_retained_causal_spans"),
            "{error}"
        );

        let case = &mut fixture.cases[index];
        case.expected_retained_causal_spans = None;
        let memory = write_case_claims(case, &memory_admin()).expect("the case writes");
        assert!(
            stored_claim(&memory, "evt-boss-bug").is_some()
                && stored_claim(&memory, "evt-boss-retry").is_some(),
            "both claims are retained"
        );
        assert!(
            retained_causal_edges(&memory, case.query.now_ms).is_empty(),
            "no declaration, no retained edge, no span"
        );
    }
}
