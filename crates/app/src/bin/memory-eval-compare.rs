//! Sidecar-comparison verdict CLI for the long-range memory evaluation
//! (issue #105).
//!
//! Usage:
//!
//! ```text
//! memory-eval-compare <runtime-report.json> <sidecar-report.json> [--out <verdict.json>]
//! ```
//!
//! Applies `docs/memory-adaptation.adoc` section 12's rule to two
//! `memory-long-range-eval/v1` documents: a candidate wins only by beating the
//! runtime on `temporal_order_errors` or `current_fact_correctness`, and
//! regresses if it leaks a superseded, refused, or shortened claim, answers
//! with an item the project has to refuse, or breaks a request bound. A report
//! that disagrees with its own case rows, covers a different set of cases, or
//! carries another schema is refused as incomparable rather than compared.
//!
//! Exit status is non-zero for an *incomparable* pair or for a regressed
//! candidate — the same gate `benchmark-compare` gives the benchmark budget —
//! and zero for a clean comparison, including one that found no improvement.
//!
//! The baseline side is meant to be the runtime's own artifact:
//!
//! ```text
//! memory-long-range-eval examples/memory-long-range/cases.json target/memory-runtime.json
//! memory-eval-compare target/memory-runtime.json target/memory-sidecar.json --out target/memory-verdict.json
//! ```

use aivtuber_app::{EvalReport, compare};
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(regression) if regression => ExitCode::FAILURE,
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("memory-eval-compare: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool, Box<dyn Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let mut paths: Vec<String> = Vec::new();
    let mut output: Option<PathBuf> = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--out" => {
                output = Some(PathBuf::from(arguments.get(index + 1).ok_or(
                    "usage: memory-eval-compare <runtime-report.json> <sidecar-report.json> [--out <verdict.json>]",
                )?));
                index += 2;
            }
            _ => {
                paths.push(arguments[index].clone());
                index += 1;
            }
        }
    }
    if paths.len() != 2 {
        return Err(
            "usage: memory-eval-compare <runtime-report.json> <sidecar-report.json> [--out <verdict.json>]".into(),
        );
    }
    let runtime = load_report(&paths[0])?;
    let candidate = load_report(&paths[1])?;
    let comparison = compare(&runtime, &candidate)?;
    let verdict = serde_json::to_string_pretty(&comparison)?;
    println!("{verdict}");
    if let Some(path) = output {
        fs::write(&path, format!("{verdict}\n"))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    for reason in &comparison.reasons {
        eprintln!("{}: {reason}", comparison.verdict_label());
    }
    Ok(comparison.is_regression())
}

fn load_report(path: &str) -> Result<EvalReport, Box<dyn Error>> {
    let bytes = fs::read(path).map_err(|error| format!("read {path}: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {path}: {error}").into())
}
