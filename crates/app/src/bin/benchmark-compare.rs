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
//! benchmark-compare --base <base.json>... --head <head.json>... [budgets.json] [output-dir]
//! ```
//!
//! The `--base`/`--head` form takes repeated runs of each side (issue #180):
//! metric values are aggregated by their median and invariants by their
//! maximum, so a single noisy run cannot decide the verdict.

#![forbid(unsafe_code)]

use aivtuber_telemetry::{BenchmarkGateError, BenchmarkResult, Budgets, GateVerdict, compare_runs};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DEFAULT_BUDGETS: &str = "benchmarks/budgets.json";
const DEFAULT_OUTPUT_DIR: &str = "target/benchmark-gate";

const USAGE: &str = "usage: benchmark-compare <base-result.json> <head-result.json> [budgets.json] [output-dir]\n       or: benchmark-compare --base <base.json> [--base <base.json>...] --head <head.json> [--head <head.json>...] [budgets.json] [output-dir]";

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

/// One side of the comparison: the runs to aggregate and where they came from.
struct Side {
    label: &'static str,
    runs: Vec<PathBuf>,
}

fn run() -> Result<GateVerdict, Box<dyn Error>> {
    let (base, head, budgets_path, output_dir) = parse_args(std::env::args().skip(1).collect())?;

    let base_runs = load_results(&base)?;
    let head_runs = load_results(&head)?;
    let budgets = load_budgets(&budgets_path)?;

    let report = compare_runs(&base_runs, &head_runs, &budgets)?;

    println!(
        "benchmark gate: base={} head={} ({} base run(s), {} head run(s))",
        report.base_commit, report.head_commit, report.base_runs, report.head_runs
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
            "base_runs": report.base_runs,
            "head_runs": report.head_runs,
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

/// Accept both the original two-positional form and the repeated
/// `--base`/`--head` form (issue #180), keeping the reviewed CLI stable.
fn parse_args(args: Vec<String>) -> Result<(Side, Side, PathBuf, PathBuf), Box<dyn Error>> {
    let mut base = Vec::new();
    let mut head = Vec::new();
    let mut positional = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--base" | "--head" => {
                let path = args.get(index + 1).ok_or_else(|| {
                    format!("{} requires a result file path\n{USAGE}", args[index])
                })?;
                if args[index] == "--base" {
                    base.push(PathBuf::from(path));
                } else {
                    head.push(PathBuf::from(path));
                }
                index += 2;
            }
            unknown if unknown.starts_with("--") => {
                return Err(format!("unknown argument {unknown:?}\n{USAGE}").into());
            }
            value => {
                positional.push(PathBuf::from(value));
                index += 1;
            }
        }
    }

    if !base.is_empty() || !head.is_empty() {
        if base.is_empty() || head.is_empty() {
            return Err(format!("both --base and --head are required\n{USAGE}").into());
        }
        if positional.len() > 2 {
            return Err(format!(
                "unexpected extra arguments {:?} after --base/--head\n{USAGE}",
                positional[2..].to_vec()
            )
            .into());
        }
        let budgets_path = positional
            .first()
            .cloned()
            .unwrap_or_else(|| repo_relative(DEFAULT_BUDGETS));
        let output_dir = positional
            .get(1)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_DIR));
        return Ok((
            Side {
                label: "base",
                runs: base,
            },
            Side {
                label: "head",
                runs: head,
            },
            budgets_path,
            output_dir,
        ));
    }

    if positional.len() < 2 {
        return Err(format!("expected at least two arguments\n{USAGE}").into());
    }
    if positional.len() > 4 {
        return Err(format!(
            "unexpected extra arguments {:?}\n{USAGE}",
            positional[4..].to_vec()
        )
        .into());
    }
    let budgets_path = positional
        .get(2)
        .cloned()
        .unwrap_or_else(|| repo_relative(DEFAULT_BUDGETS));
    let output_dir = positional
        .get(3)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_DIR));
    Ok((
        Side {
            label: "base",
            runs: vec![positional[0].clone()],
        },
        Side {
            label: "head",
            runs: vec![positional[1].clone()],
        },
        budgets_path,
        output_dir,
    ))
}

fn load_results(side: &Side) -> Result<Vec<BenchmarkResult>, Box<dyn Error>> {
    side.runs
        .iter()
        .map(|path| load_result(path, side.label))
        .collect()
}

fn load_result(path: &Path, side: &str) -> Result<BenchmarkResult, Box<dyn Error>> {
    let bytes = fs::read(path).map_err(|error| {
        BenchmarkGateError::new(format!(
            "cannot read {side} result {}: {error}",
            path.display()
        ))
    })?;
    BenchmarkResult::from_json(&bytes)
        .map_err(|error| BenchmarkGateError::new(format!("{}: {error}", path.display())).into())
}

fn load_budgets(path: &Path) -> Result<Budgets, Box<dyn Error>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<(usize, usize, PathBuf, PathBuf), Box<dyn Error>> {
        let (base, head, budgets, output) =
            parse_args(args.iter().map(|arg| (*arg).to_owned()).collect())?;
        Ok((base.runs.len(), head.runs.len(), budgets, output))
    }

    #[test]
    fn original_positional_form_is_unchanged() {
        let (base_runs, head_runs, budgets, output) =
            parse(&["base.json", "head.json", "b.json", "out"]).expect("parse");
        assert_eq!((base_runs, head_runs), (1, 1));
        assert_eq!(budgets, PathBuf::from("b.json"));
        assert_eq!(output, PathBuf::from("out"));
    }

    #[test]
    fn positional_form_defaults_budgets_and_output() {
        let (_, _, budgets, output) = parse(&["base.json", "head.json"]).expect("parse");
        assert!(budgets.ends_with("benchmarks/budgets.json"));
        assert_eq!(output, PathBuf::from(DEFAULT_OUTPUT_DIR));
    }

    #[test]
    fn repeated_run_form_collects_every_side() {
        let (base_runs, head_runs, budgets, output) = parse(&[
            "--base",
            "b1.json",
            "--base",
            "b2.json",
            "--base",
            "b3.json",
            "--head",
            "h1.json",
            "--head",
            "h2.json",
            "--head",
            "h3.json",
            "budgets.json",
            "gate",
        ])
        .expect("parse");
        assert_eq!((base_runs, head_runs), (3, 3));
        assert_eq!(budgets, PathBuf::from("budgets.json"));
        assert_eq!(output, PathBuf::from("gate"));
    }

    #[test]
    fn a_side_without_runs_is_rejected() {
        assert!(parse(&["--base", "b1.json"]).is_err());
        assert!(parse(&["--head", "h1.json"]).is_err());
        assert!(parse(&["--base"]).is_err());
        assert!(parse(&["--nope", "b1.json"]).is_err());
        assert!(parse(&["base.json"]).is_err());
    }
}
