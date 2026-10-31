//! Long-range memory evaluation against an external memory sidecar
//! (issue #105).
//!
//! Usage:
//!
//! ```text
//! memory-sidecar-eval <cases.json> <endpoint> [report.json]
//! ```
//!
//! Loads the published fixture (or any compatible case document), runs it
//! through the project's write gate and into a Hindsight deployment over the
//! provider-neutral memory-sidecar port, and prints a
//! `memory-long-range-eval/v1` report: the same shape the runtime's own
//! `memory-long-range-eval` emits, so `memory-eval-compare` judges the two by
//! one rule.
//!
//! The bearer credential comes from `AIVTUBER_MEMORY_SIDECAR_TOKEN`, never from
//! the command line, where it would land in a process listing. Exit status is
//! non-zero when a case violates its expectations, so the report is evidence
//! rather than a log.
//!
//! ```text
//! memory-long-range-eval examples/memory-long-range/cases.json target/memory-runtime.json
//! memory-sidecar-eval examples/memory-long-range/cases.json http://localhost:8888 target/memory-sidecar.json
//! memory-eval-compare examples/memory-long-range/cases.json target/memory-runtime.json target/memory-sidecar.json --out target/memory-verdict.json
//! ```

use aivtuber_adapters::{HindsightConfig, HindsightSidecar};
use aivtuber_app::{MemoryEvalFixture, evaluate_sidecar};
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::{self, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed part of the namespace the run's service scopes are opened under.
const SCOPE_PREFIX: &str = "memory-sidecar-eval";

/// The namespace this run's service scopes are opened under.
///
/// A fixed prefix would be shared by every run, and since opening a scope
/// clears it first, two runs against one deployment would each clear and then
/// measure the other's claims. The process id and start time make the run's
/// namespace its own.
fn run_scope_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{SCOPE_PREFIX}-{}-{nanos:x}", process::id())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("memory-sidecar-eval: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.len() < 2 || arguments.len() > 3 {
        return Err("usage: memory-sidecar-eval <cases.json> <endpoint> [report.json]".into());
    }
    let fixture_path = PathBuf::from(&arguments[0]);
    let bytes = fs::read(&fixture_path)
        .map_err(|error| format!("read {}: {error}", fixture_path.display()))?;
    let fixture: MemoryEvalFixture = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", fixture_path.display()))?;

    let mut config = HindsightConfig::new(arguments[1].clone());
    if let Ok(token) = env::var("AIVTUBER_MEMORY_SIDECAR_TOKEN")
        && !token.is_empty()
    {
        config = config.with_bearer_token(token);
    }
    config.validate()?;
    let sidecar = HindsightSidecar::new(config);

    let outcome = evaluate_sidecar(&sidecar, &fixture, &run_scope_prefix())?;
    let report = serde_json::to_string_pretty(&outcome.report)?;
    println!("{report}");
    if let Some(path) = arguments.get(2) {
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
