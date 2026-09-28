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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricValue {
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<u64>,
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
    pub git: BenchmarkGit,
    pub environment: BenchmarkEnvironment,
    pub configuration: BenchmarkConfiguration,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricValue>,
    #[serde(default)]
    pub invariants: BTreeMap<String, InvariantValue>,
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

/// One metric row for the PR job summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
    pub name: String,
    pub base: Option<f64>,
    pub head: f64,
    pub delta_percent: Option<f64>,
    pub status: MetricStatus,
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
    pub metrics: Vec<MetricComparison>,
    pub invariants: Vec<InvariantComparison>,
    pub warnings: Vec<String>,
    pub verdict: GateVerdict,
}

impl GateReport {
    /// Compact human-readable table for the PR job summary. Reviewers must
    /// not need to download artifacts to see deltas.
    pub fn markdown_summary(&self) -> String {
        let mut out =
            String::from("| Metric | Base | Head | Delta | Status |\n|---|---|---|---|---|\n");
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
                "| {} | {} | {:.4} | {} | {} |\n",
                metric.name,
                base,
                metric.head,
                delta,
                metric.status.as_str()
            ));
        }
        for invariant in &self.invariants {
            out.push_str(&format!(
                "| {} | {} | {} | max={} | {} |\n",
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
    base.validate()?;
    head.validate()?;
    base.ensure_comparable(head)?;

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
            (Some(base_value), Some(budget)) => evaluate_metric(
                base_value,
                head_metric.value,
                head_metric.sample_count,
                budget,
            ),
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
        metrics,
        invariants,
        warnings,
        verdict,
    })
}

/// Evaluate one metric against its budget. `delta` is improvement-positive:
/// positive means the head improved the metric in its direction.
fn evaluate_metric(
    base_value: f64,
    head_value: f64,
    sample_count: Option<u64>,
    budget: &MetricBudget,
) -> MetricStatus {
    if let Some(min_samples) = budget.min_samples
        && sample_count.is_some_and(|count| count < min_samples)
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
