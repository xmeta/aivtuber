//! Benchmark result contract, comparator, and budget gate (issue #58
//! Phase A + B).
//!
//! The result contract is versioned (`schema_version = "1"`, mirrored by
//! `schemas/benchmark-result.schema.json`) and machine-readable so the CI
//! comparator — not visual inspection — gates pull requests. Metric names
//! carry unambiguous units (`_ms`, `_us`, `_pct`, `_count`); cost metrics
//! keep their pricing-version identity in the result's configuration block.
//!
//! The comparator evaluates base vs head results against a
//! repository-controlled budget file:
//!
//! * performance metrics support lower-is-better and higher-is-better
//!   directions with percentage and absolute warn/fail thresholds plus a
//!   minimum sample count;
//! * each side may be measured by repeated runs (issue #180): metric values
//!   are aggregated by their median across runs, hard invariants by their
//!   maximum, so a single noisy run cannot decide the verdict and the
//!   reported sample count is the pooled sample count;
//! * hard invariants (`stale_dispatch_count == 0`, ...) fail on any single
//!   violation regardless of latency improvements;
//! * incompatible datasets/config versions are detected instead of silently
//!   compared;
//! * statuses are `improved | ok | warning | failed | pass | fail`, so noisy
//!   timing metrics can run warning-only until calibration justifies hard
//!   gates.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Result contract version this module implements.
pub const RESULT_SCHEMA_VERSION: &str = "1";

/// One measured value with optional sample count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricValue {
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<u64>,
    /// Observed spread across repeated runs of the same revision, when the
    /// comparator aggregated more than one run (issue #180). Diagnostic: it
    /// shows the noise band a verdict was taken in and never gates by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_range: Option<MetricRange>,
}

impl MetricValue {
    fn range(&self) -> Option<MetricRange> {
        self.run_range
    }
}

/// One hard-invariant observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantValue {
    pub value: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Versioned benchmark result (see `schemas/benchmark-result.schema.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkResult {
    pub schema_version: String,
    pub benchmark_suite: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    /// History-recording provenance, appended by the Phase E recorder when a
    /// trusted job stores the result on `benchmark-data`. Benchmark producers
    /// never emit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<BenchmarkRecording>,
    pub git: BenchmarkGit,
    pub environment: BenchmarkEnvironment,
    pub configuration: BenchmarkConfiguration,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricValue>,
    #[serde(default)]
    pub invariants: BTreeMap<String, InvariantValue>,
}

/// Provenance of one recorded history row. `run_id` is stable across re-runs
/// of one workflow run, so a re-run replaces its earlier row (idempotent)
/// while distinct runs accumulate for runner-variance calibration
/// (docs/performance-goals.adoc). `attempt` is diagnostic only and never
/// participates in identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkRecording {
    pub run_id: String,
    pub recorded_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkGit {
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkEnvironment {
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    pub rust_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bun_version: Option<String>,
    pub cargo_profile: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkConfiguration {
    pub config_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_profile: Option<String>,
    pub asset_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retriever_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jev_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_model_version: Option<String>,
    pub seed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_duration_ms: Option<u64>,
}

impl BenchmarkResult {
    /// Parse and validate a result document: contract version must match,
    /// required identity metadata must be present.
    pub fn from_json(bytes: &[u8]) -> Result<Self, BenchmarkGateError> {
        let result: Self = serde_json::from_slice(bytes).map_err(|error| {
            BenchmarkGateError::new(format!("invalid benchmark result: {error}"))
        })?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), BenchmarkGateError> {
        if self.schema_version != RESULT_SCHEMA_VERSION {
            return Err(BenchmarkGateError::new(format!(
                "unsupported benchmark result schema_version {:?}; expected {RESULT_SCHEMA_VERSION:?} (results must not be compared across incompatible contracts)",
                self.schema_version
            )));
        }
        if self.git.commit.trim().is_empty() {
            return Err(BenchmarkGateError::new(
                "benchmark result git.commit must be recorded",
            ));
        }
        Ok(())
    }

    /// Compatibility check before comparison: results from different
    /// datasets, suites, config versions, or comparison modes must not be
    /// silently compared.
    fn ensure_comparable(&self, other: &Self) -> Result<(), BenchmarkGateError> {
        for (name, mine, theirs) in [
            (
                "benchmark_suite",
                &self.benchmark_suite,
                &other.benchmark_suite,
            ),
            ("mode", &self.mode, &other.mode),
        ] {
            if mine != theirs {
                return Err(BenchmarkGateError::new(format!(
                    "cannot compare results with different {name}: {mine:?} vs {theirs:?}"
                )));
            }
        }
        match (&self.dataset_id, &other.dataset_id) {
            (Some(mine), Some(theirs)) if mine == theirs => {}
            (mine, theirs) => {
                return Err(BenchmarkGateError::new(format!(
                    "cannot compare results with different dataset_id: {mine:?} vs {theirs:?}"
                )));
            }
        }
        if self.configuration.config_version != other.configuration.config_version {
            return Err(BenchmarkGateError::new(format!(
                "cannot compare results with different config_version: {:?} vs {:?}",
                self.configuration.config_version, other.configuration.config_version
            )));
        }
        if self.configuration.seed != other.configuration.seed {
            return Err(BenchmarkGateError::new(format!(
                "cannot compare results with different seed: {} vs {}",
                self.configuration.seed, other.configuration.seed
            )));
        }
        Ok(())
    }
}

/// Regression direction for a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDirection {
    /// Lower is better (latency, cost, error counts).
    Lower,
    /// Higher is better (throughput, avoidance rates).
    Higher,
}

/// Thresholds for one metric. Percentage and absolute bounds are both
/// possible; a violation of either triggers the respective status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricBudget {
    pub direction: BudgetDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_regression_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_regression_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_absolute_change: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_absolute_change: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_samples: Option<u64>,
}

/// Exact limits for hard invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantBudget {
    pub max: u64,
}

/// Repository-controlled regression policy
/// (`benchmarks/budgets.json`; reviewed like code, issue #58).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Human-readable policy note; not machine-interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricBudget>,
    #[serde(default)]
    pub invariants: BTreeMap<String, InvariantBudget>,
    /// Metrics without a budget entry default to this behavior: report the
    /// delta but never gate (calibrate before making them strict, #58).
    #[serde(default)]
    pub ungated_metrics_warning_only: bool,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            schema_version: Some("1".to_owned()),
            description: None,
            metrics: BTreeMap::new(),
            invariants: BTreeMap::new(),
            ungated_metrics_warning_only: true,
        }
    }
}

/// Status of one compared metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricStatus {
    Improved,
    Ok,
    Warning,
    Failed,
    /// No budget configured for this metric: reported, not gated.
    Ungated,
}

impl MetricRange {
    fn of(values: &[f64]) -> Option<Self> {
        (values.len() > 1).then(|| Self {
            min: values.iter().copied().fold(f64::INFINITY, f64::min),
            max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        })
    }
}

impl MetricStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Improved => "improved",
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Failed => "failed",
            Self::Ungated => "ungated",
        }
    }
}

/// Observed spread of one aggregated metric across the runs of a side.
/// Reported so reviewers can see the measurement noise the verdict was taken
/// in (issue #180) instead of trusting a single run.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricRange {
    pub min: f64,
    pub max: f64,
}

/// One metric row for the PR job summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
    pub name: String,
    pub base: Option<f64>,
    pub head: f64,
    pub delta_percent: Option<f64>,
    pub status: MetricStatus,
    /// Per-run spread behind the aggregated `base`/`head` values; absent for
    /// a single-run comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_range: Option<MetricRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_range: Option<MetricRange>,
}

/// Outcome of an invariant check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantComparison {
    pub name: String,
    pub base: u64,
    pub head: u64,
    pub max: u64,
    pub passed: bool,
}

/// Overall gate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    Pass,
    Warning,
    Fail,
}

/// Full comparison report.
#[derive(Debug, Clone)]
pub struct GateReport {
    pub base_commit: String,
    pub head_commit: String,
    /// Number of runs aggregated per side.
    pub base_runs: usize,
    pub head_runs: usize,
    pub metrics: Vec<MetricComparison>,
    pub invariants: Vec<InvariantComparison>,
    pub warnings: Vec<String>,
    pub verdict: GateVerdict,
}

impl GateReport {
    /// Compact human-readable table for the PR job summary. Reviewers must
    /// not need to download artifacts to see deltas.
    pub fn markdown_summary(&self) -> String {
        let mut out = String::from(
            "| Metric | Base | Head | Delta | Base run range | Head run range | Status |\n|---|---|---|---|---|---|---|\n",
        );
        for metric in &self.metrics {
            let base = metric
                .base
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "-".to_owned());
            let delta = metric
                .delta_percent
                .map(|value| format!("{value:+.1}%"))
                .unwrap_or_else(|| "-".to_owned());
            out.push_str(&format!(
                "| {} | {} | {:.4} | {} | {} | {} | {} |\n",
                metric.name,
                base,
                metric.head,
                delta,
                format_range(metric.base_range),
                format_range(metric.head_range),
                metric.status.as_str()
            ));
        }
        for invariant in &self.invariants {
            out.push_str(&format!(
                "| {} | {} | {} | max={} | - | - | {} |\n",
                invariant.name,
                invariant.base,
                invariant.head,
                invariant.max,
                if invariant.passed { "pass" } else { "fail" }
            ));
        }
        for warning in &self.warnings {
            out.push_str(&format!("\n> [!WARNING]\n> {warning}\n"));
        }
        out.push_str(&format!(
            "\nVerdict: **{}**\n",
            match self.verdict {
                GateVerdict::Pass => "PASS",
                GateVerdict::Warning => "WARNING",
                GateVerdict::Fail => "FAIL",
            }
        ));
        out
    }
}

/// Compare base and head results under the configured budgets.
pub fn compare(
    base: &BenchmarkResult,
    head: &BenchmarkResult,
    budgets: &Budgets,
) -> Result<GateReport, BenchmarkGateError> {
    compare_runs(
        std::slice::from_ref(base),
        std::slice::from_ref(head),
        budgets,
    )
}

/// Compare repeated runs of base and head under the configured budgets
/// (issue #180).
///
/// Each side may contribute one or more results measured under identical
/// fixture/config boundaries. Metric values are aggregated by their median
/// across runs and hard invariants by their maximum, so one noisy run cannot
/// decide a verdict; the reported `sample_count` is the pooled sample count
/// across the side's runs.
pub fn compare_runs(
    base_runs: &[BenchmarkResult],
    head_runs: &[BenchmarkResult],
    budgets: &Budgets,
) -> Result<GateReport, BenchmarkGateError> {
    let base = aggregate_runs("base", base_runs)?;
    let head = aggregate_runs("head", head_runs)?;
    base.ensure_comparable(&head)?;

    let mut metrics = Vec::new();
    let mut warnings = Vec::new();

    for (name, head_metric) in &head.metrics {
        let budget = budgets.metrics.get(name);
        let base_metric = base.metrics.get(name);
        let base_value = base_metric.map(|metric| metric.value);
        let delta_percent = match (base_value, budget.map(|budget| budget.direction)) {
            (Some(base_value), Some(direction)) if base_value != 0.0 => {
                // A reduction of a lower-is-better metric and an increase of
                // a higher-is-better metric are both "improvements"; report
                // the delta in improvement-positive terms.
                Some(match direction {
                    BudgetDirection::Lower => (base_value - head_metric.value) / base_value * 100.0,
                    BudgetDirection::Higher => {
                        (head_metric.value - base_value) / base_value.abs() * 100.0
                    }
                })
            }
            _ => None,
        };

        let status = match (base_value, budget) {
            (Some(base_value), Some(budget)) => {
                let status = evaluate_metric(
                    base_value,
                    head_metric.value,
                    head_metric.sample_count,
                    budget,
                );
                // Say *why* a budgeted metric did not gate, so an ungated row
                // is never mistaken for a passing one (issue #180).
                if status == MetricStatus::Ungated
                    && let Some(min_samples) = budget.min_samples
                {
                    warnings.push(format!(
                        "metric {name:?} pools {} sample(s) over {} head run(s), below its min_samples floor of {min_samples}; reported ungated (raise the repeat count, or lower the floor only with recorded evidence, #180)",
                        head_metric
                            .sample_count
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "an unknown number of".to_owned()),
                        head_runs.len()
                    ));
                }
                status
            }
            (Some(_), None) if budgets.ungated_metrics_warning_only => {
                warnings.push(format!(
                    "metric {name:?} has no budget entry; reported ungated (calibrate before gating, #58)"
                ));
                MetricStatus::Ungated
            }
            (Some(_), None) => MetricStatus::Ungated,
            (None, _) => MetricStatus::Ungated,
        };

        metrics.push(MetricComparison {
            name: name.clone(),
            base: base_value,
            head: head_metric.value,
            delta_percent,
            status,
            base_range: base_metric.and_then(MetricValue::range),
            head_range: head_metric.range(),
        });
    }

    let mut invariants = Vec::new();
    let mut failed = false;
    for (name, budget) in &budgets.invariants {
        let head_value = head
            .invariants
            .get(name)
            .map(|invariant| invariant.value)
            .ok_or_else(|| {
                BenchmarkGateError::new(format!(
                    "head result is missing required invariant {name:?}"
                ))
            })?;
        let base_value = base
            .invariants
            .get(name)
            .map(|invariant| invariant.value)
            .ok_or_else(|| {
                BenchmarkGateError::new(format!(
                    "base result is missing required invariant {name:?}"
                ))
            })?;
        let passed = head_value <= budget.max;
        if !passed {
            failed = true;
        }
        invariants.push(InvariantComparison {
            name: name.clone(),
            base: base_value,
            head: head_value,
            max: budget.max,
            passed,
        });
    }
    // Invariants present in results but not budgeted are surfaced too.
    for (name, invariant) in &head.invariants {
        if !budgets.invariants.contains_key(name) {
            let base_value = base
                .invariants
                .get(name)
                .map(|present| present.value)
                .unwrap_or(0);
            invariants.push(InvariantComparison {
                name: name.clone(),
                base: base_value,
                head: invariant.value,
                max: 0,
                passed: invariant.value == 0,
            });
        }
    }

    metrics.sort_by(|left, right| left.name.cmp(&right.name));
    invariants.sort_by(|left, right| left.name.cmp(&right.name));

    // Ungated metrics are informational (calibrate first, #58): they neither
    // fail the gate nor escalate the verdict above their own status.
    let verdict = if failed
        || metrics
            .iter()
            .any(|metric| metric.status == MetricStatus::Failed)
    {
        GateVerdict::Fail
    } else if metrics
        .iter()
        .any(|metric| metric.status == MetricStatus::Warning)
    {
        GateVerdict::Warning
    } else {
        GateVerdict::Pass
    };

    Ok(GateReport {
        base_commit: base.git.commit.clone(),
        head_commit: head.git.commit.clone(),
        base_runs: base_runs.len(),
        head_runs: head_runs.len(),
        metrics,
        invariants,
        warnings,
        verdict,
    })
}

/// Collapse the runs of one side into a single comparable result.
///
/// * metrics: median value across runs, observed min/max retained as the
///   noise band, sample counts summed (so a pooled measurement satisfies a
///   `min_samples` floor that no single run could);
/// * invariants: maximum value across runs, so a violation observed in any
///   run fails the gate;
/// * identity/config: taken from the first run, which every other run must
///   match (otherwise the runs are not a repetition of one measurement).
///
/// Runs must agree on the metric and invariant key *sets* in both directions
/// and on the revision they claim to measure: dropping a key would silently
/// ungate it, and averaging two revisions together would report one
/// revision's measurements under the other's identity.
fn aggregate_runs(
    side: &str,
    runs: &[BenchmarkResult],
) -> Result<BenchmarkResult, BenchmarkGateError> {
    let Some((first, rest)) = runs.split_first() else {
        return Err(BenchmarkGateError::new(format!(
            "{side} comparison needs at least one benchmark result"
        )));
    };
    first.validate()?;
    for run in rest {
        run.validate()?;
        first.ensure_comparable(run).map_err(|error| {
            BenchmarkGateError::new(format!("{side} runs are not comparable: {error}"))
        })?;
        // `ensure_comparable` deliberately ignores `git.commit` because base
        // and head *must* differ. Within one side they must not: these runs
        // are repeated measurements of one revision (issue #180 review).
        if run.git.commit != first.git.commit {
            return Err(BenchmarkGateError::new(format!(
                "{side} runs disagree on git.commit: {:?} vs {:?}; one side must be repeated measurements of a single revision",
                first.git.commit, run.git.commit
            )));
        }
        for (kind, mismatched) in [
            (
                "metric",
                first.metrics.keys().ne(run.metrics.keys())
                    || first
                        .metrics
                        .keys()
                        .any(|key| !run.metrics.contains_key(key))
                    || run
                        .metrics
                        .keys()
                        .any(|key| !first.metrics.contains_key(key)),
            ),
            (
                "invariant",
                first.invariants.keys().ne(run.invariants.keys())
                    || first
                        .invariants
                        .keys()
                        .any(|key| !run.invariants.contains_key(key))
                    || run
                        .invariants
                        .keys()
                        .any(|key| !first.invariants.contains_key(key)),
            ),
        ] {
            if mismatched {
                return Err(BenchmarkGateError::new(format!(
                    "{side} runs disagree on {kind} keys: the first run and a later run must measure exactly the same {kind} set, so no metric is silently ungated and no invariant violation is silently dropped"
                )));
            }
        }
    }

    let mut aggregated = first.clone();
    aggregated.recording = None;
    aggregated.metrics.clear();
    aggregated.invariants.clear();

    for name in first.metrics.keys() {
        // Key sets are proven identical above, so every lookup is total.
        let values = runs
            .iter()
            .map(|run| run.metrics[name].value)
            .collect::<Vec<_>>();
        // Sample counts pool across runs, so a repeated measurement can meet a
        // `min_samples` floor that no single run reaches. A run without a
        // recorded count makes the pooled count unknown (`None`), which keeps
        // the floor binding instead of inventing one.
        let sample_count = runs
            .iter()
            .map(|run| run.metrics.get(name).and_then(|metric| metric.sample_count))
            .collect::<Option<Vec<_>>>()
            .map(|counts| counts.into_iter().fold(0_u64, u64::saturating_add));
        aggregated.metrics.insert(
            name.clone(),
            MetricValue {
                value: median(&values),
                sample_count,
                run_range: MetricRange::of(&values),
            },
        );
    }

    for (name, invariant) in &first.invariants {
        let value = runs
            .iter()
            .map(|run| run.invariants[name].value)
            .collect::<Vec<_>>();
        aggregated.invariants.insert(
            name.clone(),
            InvariantValue {
                value: value.iter().copied().max().unwrap_or(0),
                detail: invariant.detail.clone(),
            },
        );
    }

    Ok(aggregated)
}

/// Median of a non-empty sample: the middle order statistic, or the mean of
/// the two middle values for an even count. Deterministic and outlier
/// resistant, unlike a mean (issue #180).
fn median(values: &[f64]) -> f64 {
    debug_assert!(!values.is_empty());
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

/// Evaluate one metric against its budget. `delta` is improvement-positive:
/// positive means the head improved the metric in its direction.
fn evaluate_metric(
    base_value: f64,
    head_value: f64,
    sample_count: Option<u64>,
    budget: &MetricBudget,
) -> MetricStatus {
    // An *unknown* pooled count must keep the floor binding: `sample_count`
    // is optional in the schema, so treating "no count" as "enough samples"
    // would let a missing count fail open (issue #180 review).
    if let Some(min_samples) = budget.min_samples
        && sample_count.is_none_or(|count| count < min_samples)
    {
        return MetricStatus::Ungated;
    }

    let delta = head_value - base_value;
    let improved = match budget.direction {
        BudgetDirection::Lower => delta < 0.0,
        BudgetDirection::Higher => delta > 0.0,
    };
    if improved {
        return MetricStatus::Improved;
    }

    let regression = delta.abs();
    let base_reference = base_value.abs();
    let percent_regression = if base_reference > 0.0 {
        regression / base_reference * 100.0
    } else {
        f64::INFINITY
    };

    let fail_percent = budget
        .fail_regression_percent
        .map(|limit| percent_regression > limit)
        .unwrap_or(false);
    let fail_absolute = budget
        .fail_absolute_change
        .map(|limit| regression > limit)
        .unwrap_or(false);
    if fail_percent || fail_absolute {
        return MetricStatus::Failed;
    }

    let warn_percent = budget
        .warn_regression_percent
        .map(|limit| percent_regression > limit)
        .unwrap_or(false);
    let warn_absolute = budget
        .warn_absolute_change
        .map(|limit| regression > limit)
        .unwrap_or(false);
    if warn_percent || warn_absolute {
        return MetricStatus::Warning;
    }

    MetricStatus::Ok
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkGateError {
    message: String,
}

fn format_range(range: Option<MetricRange>) -> String {
    match range {
        Some(range) => format!("{:.4}–{:.4}", range.min, range.max),
        None => "-".to_owned(),
    }
}

impl BenchmarkGateError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BenchmarkGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BenchmarkGateError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric_map(entries: &[(&str, f64, Option<u64>)]) -> BTreeMap<String, MetricValue> {
        entries
            .iter()
            .map(|(name, value, count)| {
                (
                    (*name).to_owned(),
                    MetricValue {
                        value: *value,
                        sample_count: *count,
                        run_range: None,
                    },
                )
            })
            .collect()
    }

    fn invariant_map(entries: &[(&str, u64)]) -> BTreeMap<String, InvariantValue> {
        entries
            .iter()
            .map(|(name, value)| {
                (
                    (*name).to_owned(),
                    InvariantValue {
                        value: *value,
                        detail: None,
                    },
                )
            })
            .collect()
    }

    fn result(commit: &str, metrics: BTreeMap<String, MetricValue>) -> BenchmarkResult {
        BenchmarkResult {
            schema_version: RESULT_SCHEMA_VERSION.to_owned(),
            benchmark_suite: "replay-comparison".to_owned(),
            mode: "deterministic_only".to_owned(),
            dataset_id: Some("starter-replay-comparison-v1".to_owned()),
            recording: None,
            git: BenchmarkGit {
                commit: commit.to_owned(),
                base_commit: None,
            },
            environment: BenchmarkEnvironment {
                os: "windows".to_owned(),
                architecture: "x86_64".to_owned(),
                cpu: None,
                rust_version: "rustc 1.98.1".to_owned(),
                bun_version: None,
                cargo_profile: "release".to_owned(),
            },
            configuration: BenchmarkConfiguration {
                config_version: "replay-benchmark-v1".to_owned(),
                runtime_profile: None,
                asset_version: "starter-v1".to_owned(),
                index_version: None,
                retriever_version: None,
                jev_model: None,
                thinking_model: None,
                tts_model: None,
                cost_model_version: None,
                seed: 4242,
                stream_duration_ms: Some(180_000),
            },
            metrics,
            invariants: invariant_map(&[("reliability.stale_dispatch_count", 0)]),
        }
    }

    fn latency_budget(warn: f64, fail: f64) -> MetricBudget {
        MetricBudget {
            direction: BudgetDirection::Lower,
            warn_regression_percent: Some(warn),
            fail_regression_percent: Some(fail),
            warn_absolute_change: None,
            fail_absolute_change: None,
            min_samples: None,
        }
    }

    #[test]
    fn improved_lower_metric_reports_improvement() {
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "cached.first_audio.p95_ms".to_owned(),
                latency_budget(5.0, 15.0),
            )]),
            ..Budgets::default()
        };
        let base = result(
            "base",
            metric_map(&[("cached.first_audio.p95_ms", 20.0, Some(3))]),
        );
        let head = result(
            "head",
            metric_map(&[("cached.first_audio.p95_ms", 18.0, Some(3))]),
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Improved);
        assert!((report.metrics[0].delta_percent.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(report.verdict, GateVerdict::Pass);
    }

    #[test]
    fn regression_beyond_fail_threshold_fails_and_beyond_warn_warns() {
        let budgets = Budgets {
            metrics: BTreeMap::from([
                (
                    "cached.first_audio.p95_ms".to_owned(),
                    latency_budget(5.0, 15.0),
                ),
                (
                    "scheduler.decision.p99_us".to_owned(),
                    latency_budget(10.0, 20.0),
                ),
            ]),
            ..Budgets::default()
        };
        // +5% on 20ms = 21ms (ok), +18% on 100us = 118us (warning).
        let base = result(
            "base",
            metric_map(&[
                ("cached.first_audio.p95_ms", 20.0, Some(3)),
                ("scheduler.decision.p99_us", 100.0, Some(3)),
            ]),
        );
        let head = result(
            "head",
            metric_map(&[
                ("cached.first_audio.p95_ms", 21.0, Some(3)),
                ("scheduler.decision.p99_us", 118.0, Some(3)),
            ]),
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Ok);
        assert_eq!(report.metrics[1].status, MetricStatus::Warning);
        assert_eq!(report.verdict, GateVerdict::Warning);
    }

    #[test]
    fn hard_regression_fails_the_gate() {
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "cached.first_audio.p95_ms".to_owned(),
                latency_budget(5.0, 15.0),
            )]),
            ..Budgets::default()
        };
        let base = result(
            "base",
            metric_map(&[("cached.first_audio.p95_ms", 20.0, Some(3))]),
        );
        let head = result(
            "head",
            metric_map(&[("cached.first_audio.p95_ms", 24.0, Some(3))]),
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Failed);
        assert_eq!(report.verdict, GateVerdict::Fail);
    }

    #[test]
    fn min_samples_below_threshold_gates_nothing() {
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "cached.first_audio.p95_ms".to_owned(),
                MetricBudget {
                    direction: BudgetDirection::Lower,
                    warn_regression_percent: Some(5.0),
                    fail_regression_percent: Some(15.0),
                    warn_absolute_change: None,
                    fail_absolute_change: None,
                    min_samples: Some(100),
                },
            )]),
            ..Budgets::default()
        };
        let base = result(
            "base",
            metric_map(&[("cached.first_audio.p95_ms", 20.0, Some(3))]),
        );
        let head = result(
            "head",
            metric_map(&[("cached.first_audio.p95_ms", 40.0, Some(3))]),
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Ungated);
        assert_eq!(report.verdict, GateVerdict::Pass);
    }

    #[test]
    fn higher_is_better_metric_improves_when_it_increases() {
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "routing.generative_avoidance_pct".to_owned(),
                MetricBudget {
                    direction: BudgetDirection::Higher,
                    warn_regression_percent: Some(5.0),
                    fail_regression_percent: Some(15.0),
                    warn_absolute_change: None,
                    fail_absolute_change: None,
                    min_samples: None,
                },
            )]),
            ..Budgets::default()
        };
        let base = result(
            "base",
            metric_map(&[("routing.generative_avoidance_pct", 80.0, Some(3))]),
        );
        let head = result(
            "head",
            metric_map(&[("routing.generative_avoidance_pct", 84.0, Some(3))]),
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Improved);
    }

    #[test]
    fn absolute_change_budget_fires_on_small_denominators() {
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "semantic.wrong_reuse_rate_pct".to_owned(),
                MetricBudget {
                    direction: BudgetDirection::Lower,
                    warn_regression_percent: None,
                    fail_regression_percent: None,
                    warn_absolute_change: Some(0.5),
                    fail_absolute_change: Some(2.0),
                    min_samples: None,
                },
            )]),
            ..Budgets::default()
        };
        let base = result(
            "base",
            metric_map(&[("semantic.wrong_reuse_rate_pct", 0.4, Some(3))]),
        );
        let head = result(
            "head",
            metric_map(&[("semantic.wrong_reuse_rate_pct", 1.0, Some(3))]),
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Warning);
    }

    #[test]
    fn any_invariant_violation_fails_despite_improvements() {
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "cached.first_audio.p95_ms".to_owned(),
                latency_budget(5.0, 15.0),
            )]),
            invariants: BTreeMap::from([(
                "reliability.stale_dispatch_count".to_owned(),
                InvariantBudget { max: 0 },
            )]),
            ..Budgets::default()
        };
        let mut base = result(
            "base",
            metric_map(&[("cached.first_audio.p95_ms", 20.0, None)]),
        );
        base.invariants.insert(
            "reliability.stale_dispatch_count".to_owned(),
            InvariantValue {
                value: 0,
                detail: None,
            },
        );
        let mut head = result(
            "head",
            metric_map(&[("cached.first_audio.p95_ms", 1.0, None)]),
        );
        head.invariants.insert(
            "reliability.stale_dispatch_count".to_owned(),
            InvariantValue {
                value: 2,
                detail: None,
            },
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        assert_eq!(report.verdict, GateVerdict::Fail);
        assert!(!report.invariants[0].passed);
        assert_eq!(report.metrics[0].status, MetricStatus::Improved);
    }

    #[test]
    fn incompatible_datasets_are_detected_not_silently_compared() {
        let mut head = result("head", metric_map(&[]));
        head.dataset_id = Some("starter-replay-comparison-v2".to_owned());
        let base = result("base", metric_map(&[]));

        let error = compare(&base, &head, &Budgets::default()).expect_err("dataset mismatch");
        assert!(error.to_string().contains("dataset_id"));
    }

    #[test]
    fn incompatible_schema_versions_are_rejected() {
        let mut head = result("head", metric_map(&[]));
        head.schema_version = "2".to_owned();
        let error = head.validate().expect_err("schema mismatch");
        assert!(error.to_string().contains("schema_version"));
    }

    #[test]
    fn ungated_metric_is_reported_without_gating() {
        let base = result("base", metric_map(&[("some.new.metric_us", 10.0, Some(3))]));
        let head = result(
            "head",
            metric_map(&[("some.new.metric_us", 500.0, Some(3))]),
        );

        let report = compare(&base, &head, &Budgets::default()).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Ungated);
        assert_eq!(report.verdict, GateVerdict::Pass);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("some.new.metric_us"))
        );
    }

    #[test]
    fn repeated_runs_are_aggregated_by_median_and_invariant_maximum() {
        // Issue #180: a single run of a single-digit-microsecond metric must
        // not decide the verdict; the median of repeated runs does.
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "routing.route_decision.p95_us".to_owned(),
                MetricBudget {
                    direction: BudgetDirection::Lower,
                    warn_regression_percent: None,
                    fail_regression_percent: None,
                    warn_absolute_change: Some(3.0),
                    fail_absolute_change: Some(6.0),
                    min_samples: Some(15),
                },
            )]),
            ..Budgets::default()
        };
        let runs = |values: &[f64]| -> Vec<BenchmarkResult> {
            values
                .iter()
                .map(|value| {
                    result(
                        "run",
                        metric_map(&[("routing.route_decision.p95_us", *value, Some(3))]),
                    )
                })
                .collect()
        };
        // One 5µs outlier among quiet 10µs runs must not move the median.
        let base = runs(&[10.0, 10.0, 11.0, 10.0, 12.0, 5.0, 10.0]);
        let head = runs(&[14.0, 14.0, 14.0, 15.0, 14.0, 14.0, 15.0]);

        let report = compare_runs(&base, &head, &budgets).expect("compare");
        let routing = &report.metrics[0];
        assert_eq!(routing.name, "routing.route_decision.p95_us");
        assert_eq!(routing.base, Some(10.0));
        assert_eq!(routing.head, 14.0);
        assert_eq!(routing.status, MetricStatus::Warning);
        assert_eq!(
            routing.base_range,
            Some(MetricRange {
                min: 5.0,
                max: 12.0
            })
        );
        assert_eq!(
            routing.head_range,
            Some(MetricRange {
                min: 14.0,
                max: 15.0
            })
        );
        assert_eq!(report.base_runs, 7);
        assert_eq!(report.head_runs, 7);
        assert_eq!(report.verdict, GateVerdict::Warning);
    }

    #[test]
    fn pooled_sample_counts_satisfy_a_floor_one_run_cannot_meet() {
        // Issue #180: min_samples must bind the *pooled* measurement, and a
        // too-small pooled sample count must report ungated with a reason
        // rather than a silent coin flip.
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "routing.route_decision.p95_us".to_owned(),
                MetricBudget {
                    direction: BudgetDirection::Lower,
                    warn_regression_percent: None,
                    fail_regression_percent: None,
                    warn_absolute_change: Some(3.0),
                    fail_absolute_change: Some(6.0),
                    min_samples: Some(15),
                },
            )]),
            ..Budgets::default()
        };
        let one_run = |value: f64| {
            vec![result(
                "run",
                metric_map(&[("routing.route_decision.p95_us", value, Some(3))]),
            )]
        };

        let report = compare_runs(&one_run(10.0), &one_run(90.0), &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Ungated);
        assert_eq!(report.verdict, GateVerdict::Pass);
        assert!(
            report.warnings.iter().any(
                |warning| warning.contains("min_samples") && warning.contains("route_decision")
            )
        );

        let seven = |value: f64| -> Vec<BenchmarkResult> {
            (0..7)
                .map(|_| {
                    result(
                        "run",
                        metric_map(&[("routing.route_decision.p95_us", value, Some(3))]),
                    )
                })
                .collect()
        };
        let report = compare_runs(&seven(10.0), &seven(90.0), &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Failed);
        assert_eq!(report.verdict, GateVerdict::Fail);
    }

    #[test]
    fn a_clean_repeated_comparison_keeps_the_same_verdict_across_noise() {
        // Issue #180 acceptance: repeating a base-vs-base comparison must not
        // flip between pass and fail on run-to-run noise alone.
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "routing.route_decision.p95_us".to_owned(),
                MetricBudget {
                    direction: BudgetDirection::Lower,
                    warn_regression_percent: None,
                    fail_regression_percent: None,
                    warn_absolute_change: Some(3.0),
                    fail_absolute_change: Some(6.0),
                    min_samples: Some(15),
                },
            )]),
            ..Budgets::default()
        };
        let runs = |values: &[f64]| -> Vec<BenchmarkResult> {
            values
                .iter()
                .map(|value| {
                    result(
                        "run",
                        metric_map(&[("routing.route_decision.p95_us", *value, Some(3))]),
                    )
                })
                .collect()
        };
        // Observed noise for this metric on one runner, one build (#180).
        let base = runs(&[9.0, 10.0, 12.0, 9.0, 15.0, 9.0, 10.0]);
        let head = runs(&[9.0, 10.0, 11.0, 12.0, 14.0, 9.0, 11.0]);

        for _ in 0..8 {
            let report = compare_runs(&base, &head, &budgets).expect("compare");
            assert_eq!(report.verdict, GateVerdict::Pass);
        }
    }

    #[test]
    fn runs_with_inconsistent_metric_sets_are_rejected() {
        let base = vec![result(
            "base",
            metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
        )];
        let head = vec![
            result(
                "head",
                metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
            ),
            result("head", metric_map(&[])),
        ];

        let error = compare_runs(&base, &head, &Budgets::default()).expect_err("metric mismatch");
        assert!(error.to_string().contains("disagree"));
    }

    #[test]
    fn runs_across_different_config_versions_are_rejected() {
        let mut second = result(
            "base",
            metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
        );
        second.configuration.seed = 7;
        let base = vec![
            result(
                "base",
                metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
            ),
            second,
        ];
        let head = vec![result(
            "head",
            metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
        )];

        let error = compare_runs(&base, &head, &Budgets::default()).expect_err("seed mismatch");
        assert!(error.to_string().contains("not comparable"));
    }

    #[test]
    fn empty_run_lists_are_rejected() {
        let error = compare_runs(&[], &[], &Budgets::default()).expect_err("no runs");
        assert!(error.to_string().contains("at least one"));
    }

    #[test]
    fn runs_measuring_different_revisions_are_rejected() {
        // Issue #180 review: `ensure_comparable` ignores `git.commit` because
        // base and head must differ, so one side must enforce it separately —
        // otherwise two revisions get medianed and reported as one.
        let base = vec![
            result(
                "base-rev",
                metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
            ),
            result(
                "other-base-rev",
                metric_map(&[("routing.route_decision.p95_us", 12.0, Some(3))]),
            ),
        ];
        let head = vec![result(
            "head-rev",
            metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
        )];

        let error = compare_runs(&base, &head, &Budgets::default()).expect_err("mixed commits");
        assert!(error.to_string().contains("git.commit"));
    }

    #[test]
    fn an_unknown_pooled_sample_count_keeps_the_floor_binding() {
        // Issue #180 review: `sample_count` is optional in the schema, so a
        // missing count must not fail open through a configured `min_samples`.
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "routing.route_decision.p95_us".to_owned(),
                MetricBudget {
                    direction: BudgetDirection::Lower,
                    warn_regression_percent: None,
                    fail_regression_percent: None,
                    warn_absolute_change: Some(3.0),
                    fail_absolute_change: Some(6.0),
                    min_samples: Some(3),
                },
            )]),
            ..Budgets::default()
        };
        let uncounted = |value: f64| {
            vec![result(
                "rev",
                metric_map(&[("routing.route_decision.p95_us", value, None)]),
            )]
        };

        let report = compare_runs(&uncounted(10.0), &uncounted(90.0), &budgets).expect("compare");
        assert_eq!(report.metrics[0].status, MetricStatus::Ungated);
        assert_eq!(report.verdict, GateVerdict::Pass);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("min_samples")),
            "an ungated row must say why: {:?}",
            report.warnings
        );
    }

    #[test]
    fn keys_present_only_in_a_later_run_are_rejected() {
        // Issue #180 review: comparing only the first run's key sets would
        // silently ignore a later-only metric, and a later-only *invariant*
        // would drop a violation entirely.
        let run_with = |commit: &str, extra_metric: bool, extra_invariant: bool| {
            let mut run = result(
                commit,
                metric_map(&[("routing.route_decision.p95_us", 10.0, Some(3))]),
            );
            if extra_metric {
                run.metrics.insert(
                    "semantic.wrong_reuse_rate_pct".to_owned(),
                    MetricValue {
                        value: 4.0,
                        sample_count: Some(3),
                        run_range: None,
                    },
                );
            }
            if extra_invariant {
                run.invariants.insert(
                    "resource.invalid_route_transition_count".to_owned(),
                    InvariantValue {
                        value: 7,
                        detail: None,
                    },
                );
            }
            run
        };
        let base = vec![run_with("base-rev", false, false)];

        let error = compare_runs(
            &base,
            &[
                run_with("head-rev", false, false),
                run_with("head-rev", true, false),
            ],
            &Budgets::default(),
        )
        .expect_err("later-only metric");
        assert!(error.to_string().contains("metric keys"), "{error}");

        // The same must hold when only an *invariant* appears later: dropping
        // it would hide the violation the gate exists to catch.
        let error = compare_runs(
            &base,
            &[
                run_with("head-rev", false, false),
                run_with("head-rev", false, true),
            ],
            &Budgets::default(),
        )
        .expect_err("later-only invariant");
        assert!(error.to_string().contains("invariant keys"), "{error}");
    }

    #[test]
    fn markdown_summary_is_human_inspectable() {
        let budgets = Budgets {
            metrics: BTreeMap::from([(
                "cached.first_audio.p95_ms".to_owned(),
                latency_budget(5.0, 15.0),
            )]),
            invariants: BTreeMap::from([(
                "reliability.stale_dispatch_count".to_owned(),
                InvariantBudget { max: 0 },
            )]),
            ..Budgets::default()
        };
        let base = result(
            "base",
            metric_map(&[("cached.first_audio.p95_ms", 20.0, Some(3))]),
        );
        let head = result(
            "head",
            metric_map(&[("cached.first_audio.p95_ms", 18.0, Some(3))]),
        );

        let report = compare(&base, &head, &budgets).expect("compare");
        let markdown = report.markdown_summary();
        assert!(markdown.contains("cached.first_audio.p95_ms"));
        assert!(markdown.contains("improved"));
        assert!(markdown.contains("reliability.stale_dispatch_count"));
        assert!(markdown.contains("PASS"));
    }

    #[test]
    fn result_round_trips_through_json() {
        let base = result(
            "base",
            metric_map(&[("cached.first_audio.p95_ms", 20.0, Some(3))]),
        );
        let bytes = serde_json::to_vec(&base).expect("serialize");
        let parsed = BenchmarkResult::from_json(&bytes).expect("parse");
        assert_eq!(parsed, base);
    }
}
