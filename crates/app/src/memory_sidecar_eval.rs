//! The long-range memory evaluation, run against an external memory sidecar
//! (issue #105).
//!
//! The runtime's own measurement ([`crate::memory_eval`]) asks what the
//! project's store delivers. This one asks what a memory *service* delivers
//! for the same fixture, through the provider-neutral [`MemorySidecar`] port,
//! and produces the same `memory-long-range-eval/v1` report — so
//! [`compare`](crate::memory_compare::compare) can judge the two documents by
//! one rule.
//!
//! What makes the comparison meaningful is who decides what:
//!
//! * **Admission is still the project's.** Every claim goes through the real
//!   `authorize_memory_write` gate before the sidecar is told about it, and a
//!   claim the gate refuses is never retained. A service cannot buy admission
//!   that the project would not grant.
//! * **Retention policy is still the project's.** The project's retention
//!   bound, its durable TTL, and its supersession evidence decide *what the
//!   service is allowed to keep*: the runner retires claims the store would
//!   have evicted, expired, or replaced, so the candidate is measured holding
//!   the memory the project would have kept, not everything it was ever told.
//! * **Retrieval is the service's.** Ranking, matching, and ordering are what
//!   the service is being compared on; the runner does not re-rank what it
//!   answered.
//! * **Acceptance is still the project's.** Scope, liveness, and identity are
//!   re-checked against the project's own records, and the request budget is
//!   applied by the same function the runtime uses
//!   ([`bounded_prefix`](crate::bounded_prefix)). An item the project refuses
//!   never reaches generation; it is recorded as evidence of what the service
//!   widened or kept.

use aivtuber_adaptation::source_class_is_shared;
use aivtuber_domain::{
    AuthenticatedControl, MAX_SIDECAR_RECALL_ITEMS, MemorySidecar, SidecarClaim, SidecarHandle,
    SidecarRecallQuery, SourceClass,
};
use aivtuber_runtime::{MemoryWriteDecision, SecurityRuntime, SecurityRuntimeConfig};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
use std::collections::{BTreeMap, BTreeSet};

use crate::memory_context::bounded_prefix;
use crate::memory_eval::{
    CaseDelivery, CaseReport, DeliveredClaim, ExpectationPolicy, MEMORY_EVAL_REQUEST_TEXT,
    MEMORY_EVAL_SOURCE_NAMESPACE, MemoryEvalCase, MemoryEvalError, MemoryEvalFixture,
    MemoryEvalOutcome, RefusalReason, RefusedItem, RetentionSnapshot, case_violations,
    eval_pseudonymizer, event, measure_case, memory_admin, outcome_for,
};

/// The source class every claim in the fixture arrives as, so the scope rule
/// applied to a sidecar's answer is the one the store applies to its own
/// entries.
const EVAL_SOURCE_CLASS: SourceClass = SourceClass::PublicChat;

/// A claim the project admitted, as the runner records it.
///
/// The record is what a returned item is judged against: the text the project
/// retained, the scope key it filed the claim under, and when its content
/// happened. Nothing here comes from the service.
struct AdmittedClaim {
    text: String,
    actor: Option<String>,
    source_class: SourceClass,
    recorded_at_ms: u64,
}

/// A recall item the project accepted, before the request budget is applied.
struct AcceptedItem {
    claim_id: String,
    text: String,
    retained_text: String,
}

/// Measure `fixture` against `sidecar`.
///
/// `scope_prefix` namespaces the service scopes the run opens: every case gets
/// `{scope_prefix}-{case}` and is closed again, so one measurement cannot see
/// another's claims. The prefix must be a plain token the service can name a
/// namespace with.
pub fn evaluate_sidecar(
    sidecar: &dyn MemorySidecar,
    fixture: &MemoryEvalFixture,
    scope_prefix: &str,
) -> Result<MemoryEvalOutcome, MemoryEvalError> {
    fixture.validate()?;
    if !is_scope_token(scope_prefix) {
        return Err(MemoryEvalError::InvalidFixture(format!(
            "scope prefix {scope_prefix:?} must be 1..=64 bytes of [A-Za-z0-9_-]"
        )));
    }
    let admin = memory_admin();
    let mut cases = Vec::with_capacity(fixture.cases.len());
    let mut violations = Vec::new();
    for case in &fixture.cases {
        let (report, case_violations) = run_sidecar_case(sidecar, case, scope_prefix, &admin)?;
        violations.extend(case_violations);
        cases.push(report);
    }
    Ok(outcome_for(fixture, cases, violations))
}

/// Measure one case against `sidecar`.
///
/// The scope is opened before the measurement and closed after it, whatever
/// the measurement did. A scope left open keeps the project's claims in the
/// service after the run, and the next run would then measure this run's
/// memory as its own answer.
fn run_sidecar_case(
    sidecar: &dyn MemorySidecar,
    case: &MemoryEvalCase,
    scope_prefix: &str,
    admin: &AuthenticatedControl,
) -> Result<(CaseReport, Vec<String>), MemoryEvalError> {
    let scope = scope_token(scope_prefix, &case.id);
    let sidecar_error = |message: String| MemoryEvalError::Sidecar {
        case: case.id.clone(),
        message,
    };
    sidecar
        .open_scope(&scope)
        .map_err(|error| sidecar_error(format!("open scope {scope:?}: {error}")))?;

    let measured = measure_case_in_scope(sidecar, case, &scope, admin);
    let closed = sidecar.close_scope(&scope);
    match (measured, closed) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(close_error)) => Err(sidecar_error(format!(
            "close scope {scope:?}: {close_error}"
        ))),
        (Err(primary), Ok(())) => Err(primary),
        // The measurement is lost either way, so the failure that stopped it
        // is the one reported — with the cleanup failure attached, because an
        // unclosed scope is state the reader of the run has to know about.
        (Err(primary), Err(close_error)) => Err(MemoryEvalError::SidecarCleanup {
            case: case.id.clone(),
            measurement: Box::new(primary),
            cleanup: format!("close scope {scope:?}: {close_error}"),
        }),
    }
}

/// Measure one case inside an already-open `scope`.
fn measure_case_in_scope(
    sidecar: &dyn MemorySidecar,
    case: &MemoryEvalCase,
    scope: &str,
    admin: &AuthenticatedControl,
) -> Result<(CaseReport, Vec<String>), MemoryEvalError> {
    let config = case.config.apply();
    let pseudonymizer = eval_pseudonymizer();
    let sidecar_error = |message: String| MemoryEvalError::Sidecar {
        case: case.id.clone(),
        message,
    };

    let mut security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig::default(),
        SecretRedactor::default(),
        None,
    )
    .map_err(|error| MemoryEvalError::Write(error.to_string()))?;

    let mut admitted: BTreeMap<String, AdmittedClaim> = BTreeMap::new();
    // Insertion order, which is the retention order the store evicts from.
    let mut retention_order: Vec<String> = Vec::new();
    let mut handles: BTreeMap<String, SidecarHandle> = BTreeMap::new();
    // Claims the project told the service to stop recalling: replaced,
    // expired, or evicted. A service that answers with one is keeping content
    // the project removed.
    let mut retired: BTreeSet<String> = BTreeSet::new();
    let mut entries_high_water = 0usize;

    for claim in &case.claims {
        let text = claim.claim_text()?;
        let mut envelope = event(&claim.event_id, claim.actor.as_deref());
        envelope
            .payload
            .insert("text".to_owned(), serde_json::Value::String(text.clone()));
        let (decision, _permit) = security.authorize_memory_write(&envelope, Some(admin));
        if decision == MemoryWriteDecision::Denied {
            return Err(MemoryEvalError::WriteGateDenied {
                case: case.id.clone(),
                claim: claim.event_id.clone(),
            });
        }

        // The store compacts before it inserts, so the sidecar is told to
        // forget what the store would already have removed at this record
        // time: expired entries first, then the oldest beyond the bound.
        retire_expired(
            &mut handles,
            &mut retired,
            &admitted,
            &retention_order,
            claim.recorded_at_ms,
            config.durable_ttl_ms,
            sidecar,
            scope,
        )
        .map_err(sidecar_error)?;

        let actor = claim
            .actor
            .as_deref()
            .map(|actor| pseudonymizer.pseudonymize(MEMORY_EVAL_SOURCE_NAMESPACE, actor));
        let retained_claim = SidecarClaim {
            claim_id: claim.event_id.clone(),
            actor: actor.clone(),
            topic: claim.topic.clone(),
            text: text.clone(),
            recorded_at_ms: claim.recorded_at_ms,
            event_at_ms: claim.event_at(),
        };
        retained_claim.validate().map_err(|error| {
            sidecar_error(format!(
                "claim {:?} cannot be handed to the sidecar: {error}",
                claim.event_id
            ))
        })?;
        let handle = sidecar
            .retain(scope, &retained_claim)
            .map_err(|error| sidecar_error(format!("retain {:?}: {error}", claim.event_id)))?;

        admitted.insert(
            claim.event_id.clone(),
            AdmittedClaim {
                text,
                actor,
                source_class: EVAL_SOURCE_CLASS,
                recorded_at_ms: claim.recorded_at_ms,
            },
        );
        handles.insert(claim.event_id.clone(), handle);
        retention_order.push(claim.event_id.clone());

        // Explicit supersession is evidence the project owns: the service is
        // told to stop recalling the claim a later claim replaced, whatever
        // the service's own relevance thinks of it.
        if let Some(target) = claim.supersedes.as_deref() {
            if let Some(handle) = handles.remove(target) {
                sidecar
                    .retire(scope, &handle)
                    .map_err(|error| sidecar_error(format!("retire {target:?}: {error}")))?;
            }
            retired.insert(target.to_owned());
        }

        while handles.len() > config.max_entries {
            let Some(oldest) = retention_order
                .iter()
                .find(|id| handles.contains_key(id.as_str()))
                .cloned()
            else {
                break;
            };
            if let Some(handle) = handles.remove(&oldest) {
                sidecar
                    .retire(scope, &handle)
                    .map_err(|error| sidecar_error(format!("retire {oldest:?}: {error}")))?;
            }
            retired.insert(oldest);
        }
        entries_high_water = entries_high_water.max(handles.len());
    }

    // Liveness at the read timestamp: the store's retrieval only sees entries
    // recorded by then and not yet expired, so the project retires from the
    // service whatever it would no longer show.
    retire_expired(
        &mut handles,
        &mut retired,
        &admitted,
        &retention_order,
        case.query.now_ms,
        config.durable_ttl_ms,
        sidecar,
        scope,
    )
    .map_err(sidecar_error)?;
    // A claim recorded after the read timestamp is not live at that read, so
    // the service is told to stop recalling it too: leaving it in the
    // service's index spends its ranking and answer budget on memory the
    // project would never show, and a service that ranks it highly would
    // crowd out a claim that is live.
    for (claim_id, claim) in &admitted {
        if claim.recorded_at_ms > case.query.now_ms {
            if let Some(handle) = handles.remove(claim_id) {
                sidecar
                    .retire(scope, &handle)
                    .map_err(|error| sidecar_error(format!("retire {claim_id:?}: {error}")))?;
            }
            retired.insert(claim_id.clone());
        }
    }

    let query_scope = case
        .query
        .actor
        .as_deref()
        .map(|actor| pseudonymizer.pseudonymize(MEMORY_EVAL_SOURCE_NAMESPACE, actor));
    let recall_query = SidecarRecallQuery {
        actor: query_scope.clone(),
        // The request's topic is its retrieval signal; the runtime ranks by
        // it, so the service is asked about it. A request that carries no
        // topic still needs a query, and the fixture's synthetic request is
        // the one it made.
        query: case
            .query
            .topic
            .clone()
            .unwrap_or_else(|| MEMORY_EVAL_REQUEST_TEXT.to_owned()),
        limit: MAX_SIDECAR_RECALL_ITEMS,
        now_ms: case.query.now_ms,
    };
    recall_query
        .validate()
        .map_err(|error| sidecar_error(format!("recall query: {error}")))?;
    let items = sidecar
        .recall(scope, &recall_query)
        .map_err(|error| sidecar_error(format!("recall: {error}")))?;

    let mut accepted: Vec<AcceptedItem> = Vec::new();
    let mut refused: Vec<RefusedItem> = Vec::new();
    // A service that repeats a claim is answering the same evidence twice;
    // the first answer is the one the project would take.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for item in items {
        if !seen.insert(item.claim_id.clone()) {
            continue;
        }
        let Some(claim) = admitted.get(&item.claim_id) else {
            refused.push(RefusedItem {
                claim_id: Some(item.claim_id),
                reason: RefusalReason::Unknown,
            });
            continue;
        };
        if retired.contains(&item.claim_id) {
            refused.push(RefusedItem {
                claim_id: Some(item.claim_id),
                reason: RefusalReason::Removed,
            });
            continue;
        }
        if !claim_in_scope(claim, query_scope.as_deref()) {
            refused.push(RefusedItem {
                claim_id: Some(item.claim_id),
                reason: RefusalReason::OutOfScope,
            });
            continue;
        }
        accepted.push(AcceptedItem {
            claim_id: item.claim_id,
            text: item.text,
            retained_text: claim.text.clone(),
        });
    }

    let accepted = bounded_prefix(accepted, case.query.charged_bytes, |item| {
        item.text.as_str()
    });
    let delivery = CaseDelivery {
        claims: accepted
            .into_iter()
            .map(|item| DeliveredClaim {
                claim_id: item.claim_id,
                delivered_text: item.text,
                retained_text: item.retained_text,
            })
            .collect(),
        refused,
        // The port hands a service claims, not the relations between them, so
        // a service retains no project edge and its retained-span count is 0
        // by construction. What a service can be asked is whether both members
        // of a declared pair came back, and the comparison re-derives that
        // from the fixture's declarations and this delivered set.
        causal_edges: Vec::new(),
    };
    let retention = RetentionSnapshot {
        entries_high_water,
        links_high_water: 0,
        max_entries: config.max_entries,
        max_links: config.max_links,
    };
    let report = measure_case(case, &delivery, &retention);
    let violations = case_violations(case, &report, ExpectationPolicy::CANDIDATE);
    Ok((report, violations))
}

/// Whether a returned claim is in the scope the request asked for, under the
/// same rule the store applies to its own entries.
fn claim_in_scope(claim: &AdmittedClaim, query_scope: Option<&str>) -> bool {
    let own_claim = query_scope.is_some() && claim.actor.as_deref() == query_scope;
    let shared = source_class_is_shared(claim.source_class)
        && claim
            .actor
            .as_deref()
            .is_none_or(|owner| Some(owner) == query_scope);
    own_claim || shared
}

/// Retire every retained claim that would not be live at `now_ms`.
///
/// The store's retention is a function of record time and its durable TTL; the
/// project never depends on the service to enforce either, so it tells the
/// service what to forget.
#[allow(clippy::too_many_arguments)]
fn retire_expired(
    handles: &mut BTreeMap<String, SidecarHandle>,
    retired: &mut BTreeSet<String>,
    admitted: &BTreeMap<String, AdmittedClaim>,
    retention_order: &[String],
    now_ms: u64,
    ttl_ms: u64,
    sidecar: &dyn MemorySidecar,
    scope: &str,
) -> Result<(), String> {
    for claim_id in retention_order {
        let Some(claim) = admitted.get(claim_id) else {
            continue;
        };
        let live = claim.recorded_at_ms.saturating_add(ttl_ms) > now_ms;
        if live {
            continue;
        }
        if let Some(handle) = handles.remove(claim_id) {
            sidecar
                .retire(scope, &handle)
                .map_err(|error| format!("retire {claim_id:?}: {error}"))?;
        }
        retired.insert(claim_id.clone());
    }
    Ok(())
}

/// A scope name assembled from the run's prefix and a case id.
///
/// The case id is hex-encoded rather than sanitised. Sanitising maps every
/// non-alphanumeric character to `-` and lowercases the rest, so `case_a` and
/// `case-a` — and `Case_a` and `case_a` — would name one scope, and one case's
/// run would open by clearing the other's claims.
fn scope_token(prefix: &str, case_id: &str) -> String {
    use std::fmt::Write as _;

    let mut scope = String::with_capacity(prefix.len() + 1 + case_id.len() * 2);
    scope.push_str(prefix);
    scope.push('-');
    for byte in case_id.bytes() {
        let _ = write!(scope, "{byte:02x}");
    }
    scope
}

/// Whether a string can address a scope on the service.
fn is_scope_token(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 64
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_ids_that_share_a_spelling_get_distinct_scopes() {
        // The three ways a sanitising scheme collapses two ids into one scope.
        assert_ne!(scope_token("run", "case_a"), scope_token("run", "case-a"));
        assert_ne!(scope_token("run", "Case_A"), scope_token("run", "case_a"));
        assert_ne!(
            scope_token("run", "ケース-1"),
            scope_token("run", "ケース_1")
        );
    }

    #[test]
    fn every_scope_the_runner_builds_is_a_token() {
        for case_id in ["case_a", "case-a", "Case A", "ケース-1", "", ".."] {
            let scope = scope_token("memory-sidecar-eval-abc123", case_id);
            assert!(
                scope
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
                "scope {scope:?} for {case_id:?} is not a token"
            );
        }
    }
}
