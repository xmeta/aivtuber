//! The opt-in timing artifact of the sidecar memory evaluation (issue #280).
//!
//! [`memory_sidecar_eval`](crate::memory_sidecar_eval) produces a
//! `memory-long-range-eval/v1` report, and that report has to stay
//! byte-identical between two runs of the same fixture: it is what
//! [`compare`](crate::memory_compare::compare) judges a candidate by, so a
//! wall-clock number inside it would make two runs of an unchanged service
//! disagree.
//!
//! Latency and resource use are still the question a service comparison asks
//! first, so they go into a *separate*, opt-in artifact versioned
//! [`SIDECAR_TIMING_SCHEMA_VERSION`]: per-case recall latency, the run's
//! nearest-rank p50/p95 over those samples, and the CPU and peak resident set
//! the measurement consumed. Nothing here is read by the comparison rule, and
//! nothing in the deterministic report changes because a timing run was asked
//! for.
//!
//! The artifact is descriptive, not a gate: latency on a shared machine is
//! noisy, so a number here never makes a run pass or fail.

use crate::memory_eval::{MemoryEvalFixture, fixture_digest};
use crate::memory_sidecar_eval::RecallSample;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// The timing artifact's schema. Deliberately distinct from the measured
/// report's `memory-long-range-eval/v1`: a reader must not mistake latency
/// context for a comparison input.
pub const SIDECAR_TIMING_SCHEMA_VERSION: &str = "memory-sidecar-timing/v1";

/// One case's recall latency, as the runner measured it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseTiming {
    pub case: String,
    /// Wall-clock time the service's `recall` call took, in microseconds.
    pub recall_us: u64,
}

/// The run's recall-latency aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingTotals {
    pub cases: usize,
    pub samples: usize,
    pub total_recall_us: u64,
    pub max_recall_us: u64,
    /// Nearest-rank p50 over the case samples; `None` when the run produced
    /// none, because a zero would be indistinguishable from a real zero-cost
    /// recall.
    pub p50_recall_us: Option<u64>,
    /// Nearest-rank p95 over the case samples; `None` when the run produced
    /// none.
    pub p95_recall_us: Option<u64>,
}

/// What the platform could say about the measurement process.
///
/// Both fields are optional because neither is available everywhere: on Linux
/// they come from `/proc/self`, on Windows from `Get-Process`, and elsewhere
/// they are simply absent rather than invented.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessUsage {
    /// CPU time the process had consumed at the sample, in milliseconds.
    pub cpu_ms: Option<u64>,
    /// The process's peak resident set size in bytes.
    pub peak_rss_bytes: Option<u64>,
}

impl ProcessUsage {
    /// The resources a run consumed: the CPU difference between two samples,
    /// and the peak high-water mark both samples agree on.
    ///
    /// A peak is a high-water mark, so the later sample's is the run's; the
    /// earlier one is only used when the later sample has none.
    pub fn delta(self, later: Self) -> Self {
        Self {
            cpu_ms: self
                .cpu_ms
                .zip(later.cpu_ms)
                .map(|(before, after)| after.saturating_sub(before)),
            peak_rss_bytes: later.peak_rss_bytes.or(self.peak_rss_bytes),
        }
    }
}

/// The measurement's process resources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessTiming {
    /// Wall-clock time of the whole evaluation run.
    pub wall_ms: u64,
    /// CPU time the process consumed while evaluating, when measurable.
    pub cpu_ms: Option<u64>,
    /// Peak resident set size of the process, when measurable.
    pub peak_rss_bytes: Option<u64>,
}

/// The timing artifact: latency and resources, never a quality verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingReport {
    pub schema_version: String,
    /// Canonical digest of the case document this run measured, so a timing
    /// artifact can be tied to the report it belongs beside.
    pub fixture_digest: String,
    pub cases: Vec<CaseTiming>,
    pub totals: TimingTotals,
    pub process: ProcessTiming,
}

/// The nearest-rank percentile of `samples`, or `None` when there are none.
///
/// The rank is `ceil(percentile * n / 100)`, clamped to `1..=n` after sorting
/// ascending: one sample is both its p50 and its p95, and an even count takes
/// the sample at the rank rather than interpolating between two.
pub fn nearest_rank(samples: &[u64], percentile: u8) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = (usize::from(percentile) * sorted.len())
        .div_ceil(100)
        .max(1);
    sorted.get(rank.min(sorted.len()) - 1).copied()
}

/// Build the timing artifact for a run that measured `samples`.
pub fn timing_report(
    fixture: &MemoryEvalFixture,
    samples: &[RecallSample],
    before: ProcessUsage,
    later: ProcessUsage,
    wall: Duration,
) -> TimingReport {
    let cases: Vec<CaseTiming> = samples
        .iter()
        .map(|sample| CaseTiming {
            case: sample.case.clone(),
            recall_us: sample.recall_us,
        })
        .collect();
    let latencies: Vec<u64> = cases.iter().map(|case| case.recall_us).collect();
    let consumed = before.delta(later);
    TimingReport {
        schema_version: SIDECAR_TIMING_SCHEMA_VERSION.to_owned(),
        fixture_digest: fixture_digest(fixture),
        totals: TimingTotals {
            cases: fixture.cases.len(),
            samples: latencies.len(),
            total_recall_us: latencies.iter().copied().sum(),
            max_recall_us: latencies.iter().copied().max().unwrap_or(0),
            p50_recall_us: nearest_rank(&latencies, 50),
            p95_recall_us: nearest_rank(&latencies, 95),
        },
        process: ProcessTiming {
            wall_ms: u64::try_from(wall.as_millis()).unwrap_or(u64::MAX),
            cpu_ms: consumed.cpu_ms,
            peak_rss_bytes: consumed.peak_rss_bytes,
        },
        cases,
    }
}

/// Serialize `report` as pretty JSON with a trailing newline and write it to
/// `path`.
pub fn write_timing_report(report: &TimingReport, path: &Path) -> Result<(), String> {
    let document = serde_json::to_string_pretty(report)
        .map_err(|error| format!("serialize timing report: {error}"))?;
    std::fs::write(path, format!("{document}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// Sample the measurement process's CPU time and peak resident set.
///
/// Every platform path is read-only and none of them can fail the run: a
/// platform that cannot answer returns `ProcessUsage::default()`, so the
/// artifact says "not measured" rather than reporting a fabricated zero.
pub fn process_usage() -> ProcessUsage {
    #[cfg(target_os = "linux")]
    {
        ProcessUsage {
            cpu_ms: linux_cpu_ms(),
            peak_rss_bytes: linux_peak_rss_bytes(),
        }
    }
    #[cfg(windows)]
    {
        windows_process_usage()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        ProcessUsage::default()
    }
}

/// Peak resident set size from Linux's `/proc/self/status` (`VmHWM`, KiB).
#[cfg(target_os = "linux")]
fn linux_peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib.saturating_mul(1024));
        }
    }
    None
}

/// User + system CPU time from Linux's `/proc/self/stat`, in milliseconds.
///
/// Fields 1 (`pid`) and 2 (`comm`) come first; `comm` is parenthesised and may
/// itself contain spaces or parentheses, so the remaining fields start after
/// the *last* `)`. `utime` and `stime` are fields 14 and 15 — indexes 11 and
/// 12 once `state` (field 3) is index 0 — and the kernel reports them in
/// `USER_HZ` ticks, whose value is the fixed userspace ABI constant 100, so a
/// tick is 10 ms.
#[cfg(target_os = "linux")]
fn linux_cpu_ms() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime.saturating_add(stime).saturating_mul(10))
}

/// CPU time and peak working set of this process through PowerShell's
/// `Get-Process`, which is how the other Windows benchmark in this workspace
/// reports the same numbers (`audio-backend-benchmark`).
#[cfg(windows)]
fn windows_process_usage() -> ProcessUsage {
    let pid = std::process::id();
    let script = format!(
        "$p=Get-Process -Id {pid}; \
         [pscustomobject]@{{cpu_seconds=$p.CPU; peak_rss_bytes=$p.PeakWorkingSet64}} \
         | ConvertTo-Json -Compress"
    );
    let Ok(output) = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
    else {
        return ProcessUsage::default();
    };
    if !output.status.success() {
        return ProcessUsage::default();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return ProcessUsage::default();
    };
    ProcessUsage {
        cpu_ms: value
            .get("cpu_seconds")
            .and_then(serde_json::Value::as_f64)
            .filter(|seconds| *seconds >= 0.0)
            .map(|seconds| (seconds * 1_000.0).round() as u64),
        peak_rss_bytes: value
            .get("peak_rss_bytes")
            .and_then(serde_json::Value::as_u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_eval::{MEMORY_EVAL_FIXTURE_SCHEMA_VERSION, MemoryEvalCase, MemoryEvalQuery};

    fn case(id: &str) -> MemoryEvalCase {
        MemoryEvalCase {
            id: id.to_owned(),
            config: Default::default(),
            claims: vec![crate::memory_eval::MemoryEvalClaim {
                event_id: format!("{id}-claim"),
                actor: None,
                text: "a claim".to_owned(),
                text_bytes: None,
                topic: None,
                recorded_at_ms: 1_000,
                event_at_ms: None,
                supersedes: None,
                caused_by: None,
            }],
            query: MemoryEvalQuery {
                actor: None,
                topic: None,
                now_ms: 1_000,
                charged_bytes: 0,
            },
            must_deliver: vec![format!("{id}-claim")],
            must_not_deliver: Vec::new(),
            stale: Vec::new(),
            expected_temporal_errors: 0,
            expected_delivery_order: Vec::new(),
            expected_retained_causal_spans: None,
        }
    }

    fn sample(case: &str, recall_us: u64) -> RecallSample {
        RecallSample {
            case: case.to_owned(),
            recall_us,
        }
    }

    #[test]
    fn an_empty_run_has_no_percentile_rather_than_zero() {
        assert_eq!(nearest_rank(&[], 50), None);
        assert_eq!(nearest_rank(&[], 95), None);
    }

    #[test]
    fn one_sample_is_both_its_p50_and_its_p95() {
        assert_eq!(nearest_rank(&[7], 50), Some(7));
        assert_eq!(nearest_rank(&[7], 95), Some(7));
    }

    #[test]
    fn an_even_count_takes_the_sample_at_the_rank_not_a_midpoint() {
        // n = 4: p50 -> rank 2 -> 20, p95 -> rank 4 -> 40. An interpolating
        // percentile would answer 25 and 38 here, which would claim a latency
        // no case ever measured.
        let samples = [40, 10, 30, 20];
        assert_eq!(nearest_rank(&samples, 50), Some(20));
        assert_eq!(nearest_rank(&samples, 95), Some(40));
    }

    #[test]
    fn an_odd_count_rounds_the_rank_up() {
        let samples = [10, 20, 30];
        assert_eq!(nearest_rank(&samples, 50), Some(20));
        assert_eq!(nearest_rank(&samples, 95), Some(30));
        assert_eq!(nearest_rank(&samples, 1), Some(10));
        assert_eq!(nearest_rank(&samples, 100), Some(30));
    }

    #[test]
    fn the_artifact_carries_the_samples_and_the_process_delta() {
        let fixture = MemoryEvalFixture {
            schema_version: MEMORY_EVAL_FIXTURE_SCHEMA_VERSION.to_owned(),
            cases: vec![case("one"), case("two")],
        };
        let before = ProcessUsage {
            cpu_ms: Some(120),
            peak_rss_bytes: Some(8_000),
        };
        let later = ProcessUsage {
            cpu_ms: Some(220),
            peak_rss_bytes: Some(16_000),
        };
        let report = timing_report(
            &fixture,
            &[sample("one", 400), sample("two", 100)],
            before,
            later,
            Duration::from_millis(1_500),
        );
        assert_eq!(report.schema_version, SIDECAR_TIMING_SCHEMA_VERSION);
        assert_eq!(report.fixture_digest, fixture_digest(&fixture));
        assert_eq!(
            report.cases,
            [
                CaseTiming {
                    case: "one".to_owned(),
                    recall_us: 400,
                },
                CaseTiming {
                    case: "two".to_owned(),
                    recall_us: 100,
                },
            ]
        );
        assert_eq!(report.totals.cases, 2);
        assert_eq!(report.totals.samples, 2);
        assert_eq!(report.totals.total_recall_us, 500);
        assert_eq!(report.totals.max_recall_us, 400);
        assert_eq!(report.totals.p50_recall_us, Some(100));
        assert_eq!(report.totals.p95_recall_us, Some(400));
        assert_eq!(report.process.wall_ms, 1_500);
        assert_eq!(report.process.cpu_ms, Some(100));
        assert_eq!(report.process.peak_rss_bytes, Some(16_000));
    }

    #[test]
    fn a_platform_that_cannot_measure_reports_nothing_rather_than_zero() {
        let fixture = MemoryEvalFixture {
            schema_version: MEMORY_EVAL_FIXTURE_SCHEMA_VERSION.to_owned(),
            cases: vec![case("one")],
        };
        let report = timing_report(
            &fixture,
            &[sample("one", 5)],
            ProcessUsage::default(),
            ProcessUsage::default(),
            Duration::from_millis(10),
        );
        assert_eq!(report.process.cpu_ms, None);
        assert_eq!(report.process.peak_rss_bytes, None);
        assert_eq!(report.cases.len(), 1);
    }
}
