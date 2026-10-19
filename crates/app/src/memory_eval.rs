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
//!   update evidence, the query, and what the case expects to be delivered;
//! * [`MEMORY_EVAL_REPORT_SCHEMA_VERSION`] — the measured report: current-fact
//!   correctness, stale-memory use, absent violations, temporal-order errors,
//!   delivered bytes/items against the domain bounds, and retention
//!   high-water. It is deterministic, so two runs are byte-identical and two
//!   revisions are comparable.
//!
//! A violation never aborts the run: it is recorded in
//! [`MemoryEvalOutcome::violations`] so a breach is reported with its evidence
//! instead of only failing the first case.

use aivtuber_adaptation::{ActorPseudonymizer, MemoryEntry, WorkingMemory, WorkingMemoryConfig};
use aivtuber_domain::{
    AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION,
    EventEnvelope, EventKind, LocalControlIngress, MAX_THINKING_CONTEXT_BYTES,
    MAX_THINKING_CONTEXT_ITEMS, MAX_THINKING_TEXT_BYTES, OperatorCommandInput, SecurityPlane,
    SourceClass, TrustLevel,
};
use aivtuber_runtime::{MemoryWriteDecision, SecurityRuntime, SecurityRuntimeConfig};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
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

const ADMIN_SECRET: [u8; 32] = [0x21_u8; 32];

#[derive(Debug)]
pub enum MemoryEvalError {
    InvalidFixture(String),
    WriteGateDenied { case: String, claim: String },
    Write(String),
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
        }
    }
}

impl Error for MemoryEvalError {}

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
    fn apply(self) -> WorkingMemoryConfig {
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
}

impl MemoryEvalClaim {
    fn event_at(&self) -> u64 {
        self.event_at_ms.unwrap_or(self.recorded_at_ms)
    }

    fn claim_text(&self) -> Result<String, MemoryEvalError> {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseReport {
    pub case: String,
    pub delivered: Vec<String>,
    pub must_deliver: Vec<String>,
    pub stale_claims: Vec<String>,
    pub current_fact_correctness: f64,
    pub stale_memory_use: usize,
    pub absent_violations: usize,
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
    let case_count = cases.len();
    let totals = Totals {
        cases: case_count,
        current_fact_correctness: cases
            .iter()
            .map(|case| case.current_fact_correctness)
            .sum::<f64>()
            / case_count as f64,
        stale_memory_use: cases.iter().map(|case| case.stale_memory_use).sum(),
        absent_violations: cases.iter().map(|case| case.absent_violations).sum(),
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
    };
    Ok(MemoryEvalOutcome {
        report: EvalReport {
            schema_version: MEMORY_EVAL_REPORT_SCHEMA_VERSION.to_owned(),
            fixture_schema_version: fixture.schema_version.clone(),
            context_item_budget: MAX_THINKING_CONTEXT_ITEMS,
            context_byte_budget: MAX_THINKING_CONTEXT_BYTES,
            item_byte_budget: MAX_THINKING_TEXT_BYTES,
            totals,
            cases,
        },
        violations,
    })
}

fn run_case(
    case: &MemoryEvalCase,
    admin: &AuthenticatedControl,
) -> Result<(CaseReport, Vec<String>), MemoryEvalError> {
    let mut security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig::default(),
        SecretRedactor::default(),
        None,
    )
    .map_err(|error| MemoryEvalError::Write(error.to_string()))?;
    let config = case.config.apply();
    let pseudonymizer =
        ActorPseudonymizer::new("memory-eval-v1", [0x5a; 32]).expect("fixed evaluation key");
    let mut memory = WorkingMemory::new(config, pseudonymizer)
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
        let written = match claim.supersedes.as_deref() {
            Some(target) => memory.remember_durable_superseding(
                &permit,
                &text,
                claim.topic.as_deref(),
                claim.recorded_at_ms,
                &[target],
            ),
            None => memory.remember_durable(
                &permit,
                &text,
                claim.topic.as_deref(),
                claim.recorded_at_ms,
            ),
        };
        written.map_err(|error| MemoryEvalError::Write(format!("{}: {error}", claim.event_id)))?;
    }

    let context = retained_memory_context(
        &memory,
        MEMORY_EVAL_SOURCE_NAMESPACE,
        case.query.actor.as_deref(),
        case.query.topic.as_deref(),
        case.query.now_ms,
        case.query.charged_bytes,
    );

    let event_at: BTreeMap<&str, u64> = case
        .claims
        .iter()
        .map(|claim| (claim.event_id.as_str(), claim.event_at()))
        .collect();
    let mut delivered: Vec<String> = Vec::with_capacity(context.len());
    let mut truncated_claims = 0usize;
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
        if stored_claim(&memory, event_id) != Some(item.text.as_str()) {
            truncated_claims += 1;
        }
        delivered.push(claim.event_id.clone());
    }

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

    let claim_bytes: usize = context.iter().map(|item| item.text.len()).sum();
    let request_bytes = case.query.charged_bytes.saturating_add(claim_bytes);
    let metrics = memory.retention_metrics();
    let report = CaseReport {
        case: case.id.clone(),
        delivered: delivered.clone(),
        must_deliver: case.must_deliver.clone(),
        stale_claims: case.stale.clone(),
        current_fact_correctness,
        stale_memory_use,
        absent_violations,
        temporal_order_errors,
        context_items: context.len(),
        claim_bytes,
        request_bytes,
        within_item_budget: context.len() <= MAX_THINKING_CONTEXT_ITEMS
            && context
                .iter()
                .all(|item| item.text.len() <= MAX_THINKING_TEXT_BYTES),
        within_byte_budget: request_bytes <= MAX_THINKING_CONTEXT_BYTES,
        truncated_claims,
        entries_high_water: metrics.entries_high_water,
        links_high_water: metrics.links_high_water,
        max_entries: config.max_entries,
        max_links: config.max_links,
    };

    let mut violations = Vec::new();
    let push = |violations: &mut Vec<String>, message: String| {
        violations.push(format!("case {}: {message}", case.id));
    };
    for required in &case.must_deliver {
        if !delivered.contains(required) {
            push(
                &mut violations,
                format!("required current claim {required:?} was not delivered"),
            );
        }
    }
    if stale_memory_use > 0 {
        push(
            &mut violations,
            format!("{stale_memory_use} superseded claim(s) reached generation"),
        );
    }
    if absent_violations > 0 {
        push(
            &mut violations,
            format!("{absent_violations} refused claim(s) reached generation"),
        );
    }
    if truncated_claims > 0 {
        push(
            &mut violations,
            format!("{truncated_claims} shortened claim(s) reached generation"),
        );
    }
    if !report.within_item_budget || !report.within_byte_budget {
        push(
            &mut violations,
            format!(
                "request bounds violated: {} items, {request_bytes} bytes",
                report.context_items
            ),
        );
    }
    if temporal_order_errors != case.expected_temporal_errors {
        push(
            &mut violations,
            format!(
                "temporal_order_errors {temporal_order_errors}, expected {}",
                case.expected_temporal_errors
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
    if !case.expected_delivery_order.is_empty() && delivered != case.expected_delivery_order {
        push(
            &mut violations,
            format!(
                "delivered order {delivered:?}, expected {:?}",
                case.expected_delivery_order
            ),
        );
    }
    Ok((report, violations))
}

fn stored_claim<'a>(memory: &'a WorkingMemory, event_id: &str) -> Option<&'a str> {
    memory
        .entries()
        .iter()
        .find(|entry: &&MemoryEntry| entry.source.event_id == event_id)
        .map(|entry| entry.normalized_claim.as_str())
}

fn event(id: &str, actor: Option<&str>) -> EventEnvelope {
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
            Value::String("what happened next?".to_owned()),
        )]),
    }
}

fn memory_admin() -> AuthenticatedControl {
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
