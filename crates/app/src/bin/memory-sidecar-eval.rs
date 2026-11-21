//! Long-range memory evaluation against an external memory sidecar
//! (issue #105), with an opt-in latency/resource artifact (issue #280).
//!
//! Usage:
//!
//! ```text
//! memory-sidecar-eval <cases.json> <endpoint> [report.json] [--timing <path>]
//! ```
//!
//! Loads the published fixture (or any compatible case document), runs it
//! through the project's write gate and into a Hindsight deployment over the
//! provider-neutral memory-sidecar port, and prints a
//! `memory-long-range-eval/v1` report: the same shape the runtime's own
//! `memory-long-range-eval` emits, so `memory-eval-compare` judges the two by
//! one rule.
//!
//! `--timing <path>` additionally writes a `memory-sidecar-timing/v1` artifact
//! beside that report: each case's recall latency, the run's nearest-rank
//! p50/p95, and the CPU and peak RSS the measurement consumed. It is a separate
//! document on purpose — latency is context, not a quality verdict, and the
//! deterministic report stays byte-identical whether or not timing was asked
//! for.
//!
//! The bearer credential comes from `AIVTUBER_MEMORY_SIDECAR_TOKEN`, never from
//! the command line, where it would land in a process listing. Exit status is
//! non-zero when a case violates its expectations, so the report is evidence
//! rather than a log.
//!
//! ```text
//! memory-long-range-eval examples/memory-long-range/cases.json target/memory-runtime.json
//! memory-sidecar-eval examples/memory-long-range/cases.json http://localhost:8888 target/memory-sidecar.json --timing target/memory-sidecar-timing.json
//! memory-eval-compare examples/memory-long-range/cases.json target/memory-runtime.json target/memory-sidecar.json --out target/memory-verdict.json
//! ```

use aivtuber_adapters::{HindsightConfig, HindsightSidecar};
use aivtuber_app::{
    MemoryEvalFixture, evaluate_sidecar_measured, process_usage, timing_report, write_timing_report,
};
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::{self, ExitCode};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Fixed part of the namespace the run's service scopes are opened under.
const SCOPE_PREFIX: &str = "memory-sidecar-eval";

const USAGE: &str =
    "usage: memory-sidecar-eval <cases.json> <endpoint> [report.json] [--timing <path>]";

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

/// What the command line asked for.
#[derive(Debug)]
struct Arguments {
    fixture: PathBuf,
    endpoint: String,
    /// Where to write the comparable report, when a path was given.
    report: Option<PathBuf>,
    /// Where to write the timing artifact, when `--timing` was given.
    timing: Option<PathBuf>,
}

/// Parse the positional arguments and the one option this tool has.
///
/// An unknown option is refused instead of being read as a path: a mistyped
/// `--timing` would otherwise silently write the report over the fixture, or
/// measure a deployment named `--timing`.
fn parse_arguments(arguments: Vec<String>) -> Result<Arguments, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut timing: Option<PathBuf> = None;
    let mut iter = arguments.into_iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--timing" => {
                let path = iter
                    .next()
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| format!("--timing needs a path; {USAGE}"))?;
                timing = Some(PathBuf::from(path));
            }
            "--" => positional.extend(iter.by_ref()),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option {other:?}; {USAGE}"));
            }
            _ => positional.push(argument),
        }
    }
    if positional.len() < 2 || positional.len() > 3 {
        return Err(USAGE.to_owned());
    }
    let mut positional = positional.into_iter();
    Ok(Arguments {
        fixture: PathBuf::from(positional.next().expect("two positional arguments")),
        endpoint: positional.next().expect("two positional arguments"),
        report: positional.next().map(PathBuf::from),
        timing,
    })
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
    let arguments = parse_arguments(env::args().skip(1).collect())?;
    let bytes = fs::read(&arguments.fixture)
        .map_err(|error| format!("read {}: {error}", arguments.fixture.display()))?;
    let fixture: MemoryEvalFixture = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", arguments.fixture.display()))?;

    let mut config = HindsightConfig::new(arguments.endpoint.clone());
    if let Ok(token) = env::var("AIVTUBER_MEMORY_SIDECAR_TOKEN")
        && !token.is_empty()
    {
        config = config.with_bearer_token(token);
    }
    config.validate()?;
    let sidecar = HindsightSidecar::new(config);

    // Resource sampling is read-only and opt-in: it costs a platform query, so
    // a run that was not asked for timing does not pay for it.
    let before = arguments.timing.as_ref().map(|_| process_usage());
    let started = Instant::now();
    let measured = evaluate_sidecar_measured(&sidecar, &fixture, &run_scope_prefix())?;
    let wall = started.elapsed();

    let report = serde_json::to_string_pretty(&measured.outcome.report)?;
    println!("{report}");
    if let Some(path) = &arguments.report {
        fs::write(path, format!("{report}\n"))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    if let Some(path) = &arguments.timing {
        let timing = timing_report(
            &fixture,
            &measured.recall,
            before.unwrap_or_default(),
            process_usage(),
            wall,
        );
        write_timing_report(&timing, path).map_err(Box::<dyn Error>::from)?;
        // On stderr, so stdout stays exactly the report a pipeline consumes.
        eprintln!(
            "timing: {} case sample(s), p50 {:?} us, p95 {:?} us -> {}",
            timing.totals.samples,
            timing.totals.p50_recall_us,
            timing.totals.p95_recall_us,
            path.display()
        );
    }
    if measured.outcome.violations.is_empty() {
        Ok(())
    } else {
        for violation in &measured.outcome.violations {
            eprintln!("violation: {violation}");
        }
        Err(format!("{} violation(s)", measured.outcome.violations.len()).into())
    }
}
