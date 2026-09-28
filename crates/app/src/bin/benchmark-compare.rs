//! Benchmark comparator CLI — issue #58 Phase B.
//!
//! Loads base/head benchmark results (contract `schemas/benchmark-result.schema.json`)
//! and the repository budget file (`benchmarks/budgets.json`), evaluates
//! metric deltas and hard invariants, prints a Markdown summary for the PR
//! job, writes `gate-report.json` + `gate-summary.md`, and exits non-zero on
//! a configured regression (hard invariants always gate; noisy timing
//! metrics stay warning-only until calibrated).
//!
//! Usage:
//! ```text
//! benchmark-compare <base-result.json> <head-result.json> [budgets.json] [output-dir]
//! ```

#![forbid(unsafe_code)]

use aivtuber_telemetry::{BenchmarkGateError, BenchmarkResult, Budgets, GateVerdict, compare};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_BUDGETS: &str = "benchmarks/budgets.json";
const DEFAULT_OUTPUT_DIR: &str = "target/benchmark-gate";

fn main() -> ExitCode {
    match run() {
        Ok(verdict) if verdict != GateVerdict::Fail => ExitCode::from(0),
        Ok(verdict) => {
            eprintln!("benchmark gate failed with verdict: {verdict:?}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("benchmark-compare: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<GateVerdict, Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "usage: benchmark-compare <base-result.json> <head-result.json> [budgets.json] [output-dir]"
        );
        return Err("expected at least two arguments".into());
    }

    let base_path = PathBuf::from(&args[0]);
    let head_path = PathBuf::from(&args[1]);
    let budgets_path = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_relative(DEFAULT_BUDGETS));
    let output_dir = args
        .get(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_DIR));

    let base = load_result(&base_path)?;
    let head = load_result(&head_path)?;
    let budgets = load_budgets(&budgets_path)?;

    let report = compare(&base, &head, &budgets)?;

    println!(
        "benchmark gate: base={} head={}",
        report.base_commit, report.head_commit
    );
    print!("{}", report.markdown_summary());

    fs::create_dir_all(&output_dir)?;
    fs::write(
        output_dir.join("gate-report.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "verdict": match report.verdict {
                GateVerdict::Pass => "pass",
                GateVerdict::Warning => "warning",
                GateVerdict::Fail => "fail",
            },
            "base_commit": report.base_commit,
            "head_commit": report.head_commit,
            "metrics": report.metrics,
            "invariants": report.invariants,
            "warnings": report.warnings,
        }))?,
    )?;
    fs::write(
        output_dir.join("gate-summary.md"),
        report.markdown_summary(),
    )?;

    Ok(report.verdict)
}

fn load_result(path: &std::path::Path) -> Result<BenchmarkResult, Box<dyn Error>> {
    let bytes = fs::read(path).map_err(|error| {
        BenchmarkGateError::new(format!("cannot read {}: {error}", path.display()))
    })?;
    let result = BenchmarkResult::from_json(&bytes)
        .map_err(|error| BenchmarkGateError::new(format!("{}: {error}", path.display())))?;
    Ok(result)
}

fn load_budgets(path: &std::path::Path) -> Result<Budgets, Box<dyn Error>> {
    let bytes = fs::read(path).map_err(|error| {
        BenchmarkGateError::new(format!("cannot read {}: {error}", path.display()))
    })?;
    let budgets: Budgets = serde_json::from_slice(&bytes)
        .map_err(|error| BenchmarkGateError::new(format!("{}: {error}", path.display())))?;
    Ok(budgets)
}

/// Budgets live in the repository; resolve relative to the manifest dir when
/// invoked from a crate subdirectory.
fn repo_relative(path: &str) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../../").join(path)
}
