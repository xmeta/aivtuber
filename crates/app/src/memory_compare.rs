//! Executable sidecar-comparison verdict for the long-range memory evaluation
//! (issue #105).
//!
//! Section 12 of `docs/memory-adaptation.adoc` states the comparison rule in
//! prose: a sidecar has to beat the runtime on `temporal_order_errors` or
//! `current_fact_correctness` — and not regress `stale_memory_use`,
//! `absent_violations`, `truncated_claims`, or the bounds — before adopting it
//! buys anything. This module is that rule, in code, applied to two
//! `memory-long-range-eval/v1` documents so the same verdict comes out on
//! every machine.
//!
//! A candidate report is *verified against its own records*, never trusted on
//! its declared totals: correctness and stale-memory use are recomputed from
//! the per-case delivered lists, and the totals are recomputed from the case
//! rows. A report that disagrees with its own records, covers a different set
//! of cases, or names another schema is refused as incomparable rather than
//! compared, because "unverifiable" is not a measurement.

use serde::{Deserialize, Serialize};

use crate::memory_eval::{
    CaseReport, EvalReport, MEMORY_EVAL_FIXTURE_SCHEMA_VERSION, MEMORY_EVAL_REPORT_SCHEMA_VERSION,
    Totals,
};

/// The verdict artifact's own version. It is separate from the report's: the
/// report says what was measured, this says how the two were judged.
pub const MEMORY_COMPARE_SCHEMA_VERSION: &str = "memory-eval-compare/v1";

/// Tolerance when re-deriving a declared float from its records. The values
/// are small ratios averaged over a dozen cases, so anything far larger than
/// rounding noise means the report was edited rather than measured.
const FLOAT_TOLERANCE: f64 = 1e-12;

#[derive(Debug)]
pub enum MemoryCompareError {
    Incomparable(String),
}

impl std::fmt::Display for MemoryCompareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomparable(message) => {
                write!(formatter, "reports are not comparable: {message}")
            }
        }
    }
}

impl std::error::Error for MemoryCompareError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateVerdict {
    /// The candidate wins a quality dimension this issue cares about and
    /// regresses nothing: the comparison supports adopting it.
    Improvement,
    /// The candidate is comparable and safe, but wins nothing: adopting it
    /// would buy nothing this issue asks for.
    NoImprovement,
    /// The candidate regresses a zero-tolerance metric or breaks a request
    /// bound. A candidate in this state is worse than not running it.
    Rejected,
}

/// One aggregate metric, with both sides and which direction is better.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDelta {
    pub metric: String,
    pub runtime: f64,
    pub candidate: f64,
    pub lower_is_better: bool,
    pub candidate_better: bool,
    pub candidate_worse: bool,
}

/// One case's side-by-side numbers, so a verdict can be traced to evidence
/// rather than to an aggregate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseDelta {
    pub case: String,
    pub runtime_delivered: Vec<String>,
    pub candidate_delivered: Vec<String>,
    pub runtime_temporal_order_errors: usize,
    pub candidate_temporal_order_errors: usize,
    pub runtime_stale_memory_use: usize,
    pub candidate_stale_memory_use: usize,
    pub runtime_absent_violations: usize,
    pub candidate_absent_violations: usize,
    pub runtime_truncated_claims: usize,
    pub candidate_truncated_claims: usize,
    pub runtime_request_bytes: usize,
    pub candidate_request_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryComparison {
    pub schema_version: String,
    pub fixture_schema_version: String,
    pub verdict: CandidateVerdict,
    /// Every reason behind the verdict, in the order they were found.
    pub reasons: Vec<String>,
    pub deltas: Vec<MetricDelta>,
    pub case_deltas: Vec<CaseDelta>,
    pub runtime_totals: Totals,
    pub candidate_totals: Totals,
}

impl MemoryComparison {
    /// A comparison that gates CI should fail only when the candidate is
    /// worse than nothing.
    pub fn is_regression(&self) -> bool {
        self.verdict == CandidateVerdict::Rejected
    }

    /// Stable label for log lines, so a report parsed by a human and one
    /// grepped in CI name the same verdict.
    pub fn verdict_label(&self) -> &'static str {
        match self.verdict {
            CandidateVerdict::Improvement => "improvement",
            CandidateVerdict::NoImprovement => "no-improvement",
            CandidateVerdict::Rejected => "rejected",
        }
    }
}

/// Judge `candidate` against `runtime` under the section-12 rule.
///
/// Both documents must describe the same fixture and carry the same schema;
/// otherwise the numbers are not about the same questions and no verdict is
/// meaningful.
pub fn compare(
    runtime: &EvalReport,
    candidate: &EvalReport,
) -> Result<MemoryComparison, MemoryCompareError> {
    validate_report(runtime, "runtime")?;
    validate_report(candidate, "sidecar")?;
    if runtime.cases.len() != candidate.cases.len() {
        return Err(MemoryCompareError::Incomparable(format!(
            "runtime covers {} case(s), sidecar covers {}",
            runtime.cases.len(),
            candidate.cases.len()
        )));
    }
    let mut deltas = Vec::new();
    let mut case_deltas = Vec::new();
    let mut reasons = Vec::new();

    for (runtime_case, candidate_case) in runtime.cases.iter().zip(&candidate.cases) {
        if runtime_case.case != candidate_case.case {
            return Err(MemoryCompareError::Incomparable(format!(
                "case order differs: {:?} (runtime) vs {:?} (sidecar)",
                runtime_case.case, candidate_case.case
            )));
        }
        if runtime_case.must_deliver != candidate_case.must_deliver {
            return Err(MemoryCompareError::Incomparable(format!(
                "case {:?} requires different claims on each side",
                runtime_case.case
            )));
        }
        case_deltas.push(CaseDelta {
            case: runtime_case.case.clone(),
            runtime_delivered: runtime_case.delivered.clone(),
            candidate_delivered: candidate_case.delivered.clone(),
            runtime_temporal_order_errors: runtime_case.temporal_order_errors,
            candidate_temporal_order_errors: candidate_case.temporal_order_errors,
            runtime_stale_memory_use: runtime_case.stale_memory_use,
            candidate_stale_memory_use: candidate_case.stale_memory_use,
            runtime_absent_violations: runtime_case.absent_violations,
            candidate_absent_violations: candidate_case.absent_violations,
            runtime_truncated_claims: runtime_case.truncated_claims,
            candidate_truncated_claims: candidate_case.truncated_claims,
            runtime_request_bytes: runtime_case.request_bytes,
            candidate_request_bytes: candidate_case.request_bytes,
        });

        // Zero tolerance is decided per case, not on the aggregate: one leak
        // in one case is a leak, whatever the average says.
        for (label, value) in [
            ("stale_memory_use", candidate_case.stale_memory_use),
            ("absent_violations", candidate_case.absent_violations),
            ("truncated_claims", candidate_case.truncated_claims),
        ] {
            if value > 0 {
                reasons.push(format!(
                    "case {:?}: candidate {label} {value}, which must be zero",
                    runtime_case.case
                ));
            }
        }
        if !candidate_case.within_item_budget {
            reasons.push(format!(
                "case {:?}: candidate item bound violated",
                runtime_case.case
            ));
        }
        if !candidate_case.within_byte_budget {
            reasons.push(format!(
                "case {:?}: candidate byte bound violated",
                runtime_case.case
            ));
        }
    }

    let runtime_totals = &runtime.totals;
    let candidate_totals = &candidate.totals;
    for delta in [
        scalar_delta(
            "temporal_order_errors",
            runtime_totals.temporal_order_errors as f64,
            candidate_totals.temporal_order_errors as f64,
            true,
        ),
        scalar_delta(
            "current_fact_correctness",
            runtime_totals.current_fact_correctness,
            candidate_totals.current_fact_correctness,
            false,
        ),
        scalar_delta(
            "stale_memory_use",
            runtime_totals.stale_memory_use as f64,
            candidate_totals.stale_memory_use as f64,
            true,
        ),
        scalar_delta(
            "absent_violations",
            runtime_totals.absent_violations as f64,
            candidate_totals.absent_violations as f64,
            true,
        ),
        scalar_delta(
            "truncated_claims",
            runtime_totals.truncated_claims as f64,
            candidate_totals.truncated_claims as f64,
            true,
        ),
        scalar_delta(
            "max_request_bytes",
            runtime_totals.max_request_bytes as f64,
            candidate_totals.max_request_bytes as f64,
            true,
        ),
    ] {
        deltas.push(delta);
    }

    let regression = !reasons.is_empty();
    let wins_quality = |metric: &MetricDelta| metric.candidate_better;
    let improvement = deltas
        .iter()
        .filter(|delta| {
            matches!(
                delta.metric.as_str(),
                "temporal_order_errors" | "current_fact_correctness"
            )
        })
        .any(wins_quality);

    let verdict = if regression {
        CandidateVerdict::Rejected
    } else if improvement {
        CandidateVerdict::Improvement
    } else {
        CandidateVerdict::NoImprovement
    };
    if verdict == CandidateVerdict::Improvement {
        for delta in deltas.iter().filter(|delta| delta.candidate_better) {
            reasons.push(format!(
                "candidate improves {}: {} -> {}",
                delta.metric, delta.runtime, delta.candidate
            ));
        }
    } else if verdict == CandidateVerdict::NoImprovement {
        reasons.push(
            "candidate wins neither temporal_order_errors nor current_fact_correctness".to_owned(),
        );
    }

    Ok(MemoryComparison {
        schema_version: MEMORY_COMPARE_SCHEMA_VERSION.to_owned(),
        fixture_schema_version: runtime.fixture_schema_version.clone(),
        verdict,
        reasons,
        deltas,
        case_deltas,
        runtime_totals: runtime_totals.clone(),
        candidate_totals: candidate_totals.clone(),
    })
}

fn scalar_delta(metric: &str, runtime: f64, candidate: f64, lower_is_better: bool) -> MetricDelta {
    let epsilon = FLOAT_TOLERANCE;
    let better = if lower_is_better {
        candidate + epsilon < runtime
    } else {
        candidate > runtime + epsilon
    };
    let worse = if lower_is_better {
        candidate > runtime + epsilon
    } else {
        candidate + epsilon < runtime
    };
    MetricDelta {
        metric: metric.to_owned(),
        runtime,
        candidate,
        lower_is_better,
        candidate_better: better,
        candidate_worse: worse,
    }
}

/// Refuse a report whose declared numbers disagree with its own records, or
/// which cannot be about the published fixture at all.
///
/// Correctness and stale-memory use are re-derived from each case's delivered
/// list, and every aggregate is re-derived from the case rows, so a report
/// cannot improve on paper while delivering something else.
fn validate_report(report: &EvalReport, label: &str) -> Result<(), MemoryCompareError> {
    if report.schema_version != MEMORY_EVAL_REPORT_SCHEMA_VERSION {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} schema_version {:?}, expected {MEMORY_EVAL_REPORT_SCHEMA_VERSION}",
            report.schema_version
        )));
    }
    if report.fixture_schema_version != MEMORY_EVAL_FIXTURE_SCHEMA_VERSION {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} fixture_schema_version {:?}, expected {MEMORY_EVAL_FIXTURE_SCHEMA_VERSION}",
            report.fixture_schema_version
        )));
    }
    if report.cases.is_empty() {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} report covers no case"
        )));
    }
    if report.totals.cases != report.cases.len() {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} totals declare {} case(s) over {} row(s)",
            report.totals.cases,
            report.cases.len()
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for case in &report.cases {
        if !seen.insert(case.case.as_str()) {
            return Err(MemoryCompareError::Incomparable(format!(
                "{label} repeats case {:?}",
                case.case
            )));
        }
        validate_case(case, label)?;
    }
    validate_totals(&report.totals, &report.cases, label)
}

fn validate_case(case: &CaseReport, label: &str) -> Result<(), MemoryCompareError> {
    if !case.current_fact_correctness.is_finite()
        || !(0.0..=1.0).contains(&case.current_fact_correctness)
    {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} case {:?} declares non-finite/out-of-range correctness {}",
            case.case, case.current_fact_correctness
        )));
    }
    let hits = case
        .must_deliver
        .iter()
        .filter(|required| case.delivered.contains(required))
        .count();
    let derived = if case.must_deliver.is_empty() {
        1.0
    } else {
        hits as f64 / case.must_deliver.len() as f64
    };
    if (derived - case.current_fact_correctness).abs() > FLOAT_TOLERANCE {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} case {:?} declares correctness {} but delivered {:?} of {:?}",
            case.case, case.current_fact_correctness, case.delivered, case.must_deliver
        )));
    }
    let stale = case
        .delivered
        .iter()
        .filter(|id| case.stale_claims.contains(id))
        .count();
    if stale != case.stale_memory_use {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} case {:?} declares stale_memory_use {} but delivered {:?}",
            case.case, case.stale_memory_use, case.stale_claims
        )));
    }
    let refused = case
        .delivered
        .iter()
        .filter(|id| case.refused_claims.contains(id))
        .count();
    if refused != case.absent_violations {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} case {:?} declares absent_violations {} but delivered {:?} of the refused {:?}",
            case.case, case.absent_violations, case.delivered, case.refused_claims
        )));
    }
    Ok(())
}

fn validate_totals(
    totals: &Totals,
    cases: &[CaseReport],
    label: &str,
) -> Result<(), MemoryCompareError> {
    let mean = cases
        .iter()
        .map(|case| case.current_fact_correctness)
        .sum::<f64>()
        / cases.len() as f64;
    let failures = [
        (
            "current_fact_correctness",
            mean - totals.current_fact_correctness,
        ),
        (
            "stale_memory_use",
            cases
                .iter()
                .map(|case| case.stale_memory_use)
                .sum::<usize>() as f64
                - totals.stale_memory_use as f64,
        ),
        (
            "absent_violations",
            cases
                .iter()
                .map(|case| case.absent_violations)
                .sum::<usize>() as f64
                - totals.absent_violations as f64,
        ),
        (
            "temporal_order_errors",
            cases
                .iter()
                .map(|case| case.temporal_order_errors)
                .sum::<usize>() as f64
                - totals.temporal_order_errors as f64,
        ),
        (
            "truncated_claims",
            cases
                .iter()
                .map(|case| case.truncated_claims)
                .sum::<usize>() as f64
                - totals.truncated_claims as f64,
        ),
    ];
    let mismatched: Vec<&str> = failures
        .iter()
        .filter(|(_, difference)| difference.abs() > FLOAT_TOLERANCE)
        .map(|(metric, _)| *metric)
        .collect();
    if !mismatched.is_empty() {
        return Err(MemoryCompareError::Incomparable(format!(
            "{label} totals disagree with its own case rows: {}",
            mismatched.join(", ")
        )));
    }
    Ok(())
}
