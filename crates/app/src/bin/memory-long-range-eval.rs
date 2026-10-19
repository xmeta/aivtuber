//! Deterministic, provider-free long-range memory evaluation (issue #105).
//!
//! Usage:
//!
//! ```text
//! memory-long-range-eval <cases.json> [report.json]
//! ```
//!
//! Runs the published fixture (`examples/memory-long-range/cases.json`) or any
//! compatible case document through the real memory-write gate and the
//! composition root's delivered-context function, prints a
//! `memory-long-range-eval/v1` report, and writes it when a second path is
//! given. Exit status is non-zero when a case violates its expectations, so the
//! report is evidence rather than a log.
//!
//! The same fixture can be handed to an external memory sidecar; a sidecar
//! adapter that emits this report shape makes the two comparable.

use aivtuber_app::{MemoryEvalFixture, evaluate};
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("memory-long-range-eval: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.is_empty() || arguments.len() > 2 {
        return Err("usage: memory-long-range-eval <cases.json> [report.json]".into());
    }
    let fixture_path = PathBuf::from(&arguments[0]);
    let bytes = fs::read(&fixture_path)
        .map_err(|error| format!("read {}: {error}", fixture_path.display()))?;
    let fixture: MemoryEvalFixture = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", fixture_path.display()))?;
    let outcome = evaluate(&fixture)?;
    let report = serde_json::to_string_pretty(&outcome.report)?;
    println!("{report}");
    if let Some(path) = arguments.get(1) {
        fs::write(path, format!("{report}\n")).map_err(|error| format!("write {path}: {error}"))?;
    }
    if outcome.violations.is_empty() {
        Ok(())
    } else {
        for violation in &outcome.violations {
            eprintln!("violation: {violation}");
        }
        Err(format!("{} violation(s)", outcome.violations.len()).into())
    }
}
