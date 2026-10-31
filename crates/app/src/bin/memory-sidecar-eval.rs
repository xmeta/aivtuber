//! Long-range memory evaluation against an external memory sidecar
//! (issue #105).
//!
//! Usage:
//!
//! ```text
//! memory-sidecar-eval [--provider hindsight|graphiti]
//!                     [--ingest-timeout-ms <n>]
//!                     <cases.json> <endpoint> [report.json]
//! ```
//!
//! Loads the published fixture (or any compatible case document), runs it
//! through the project's write gate and into the selected memory service over
//! the provider-neutral memory-sidecar port, and prints a
//! `memory-long-range-eval/v1` report: the same shape the runtime's own
//! `memory-long-range-eval` emits, so `memory-eval-compare` judges the two by
//! one rule.
//!
//! Hindsight is the default, so the existing invocation keeps working.
//! `--provider graphiti` selects the Graphiti adapter, whose ingestion is
//! asynchronous and model-bound: `--ingest-timeout-ms` is the deadline for one
//! claim to materialize as an episode (default 180000).
//!
//! The bearer credential comes from `AIVTUBER_MEMORY_SIDECAR_TOKEN`, never from
//! the command line, where it would land in a process listing. It applies to
//! either provider; Graphiti's reference server has no authentication of its
//! own, so a token there is only meaningful behind a proxy that enforces it.
//!
//! ```text
//! memory-long-range-eval examples/memory-long-range/cases.json target/memory-runtime.json
//! memory-sidecar-eval examples/memory-long-range/cases.json http://localhost:8888 target/memory-sidecar.json
//! memory-sidecar-eval --provider graphiti --ingest-timeout-ms 300000 examples/memory-long-range/cases.json http://127.0.0.1:8000 target/memory-graphiti.json
//! memory-eval-compare examples/memory-long-range/cases.json target/memory-runtime.json target/memory-sidecar.json --out target/memory-verdict.json
//! ```

use aivtuber_adapters::{GraphitiConfig, GraphitiSidecar, HindsightConfig, HindsightSidecar};
use aivtuber_app::{MemoryEvalFixture, evaluate_sidecar};
use aivtuber_domain::MemorySidecar;
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::{self, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: memory-sidecar-eval [--provider hindsight|graphiti] [--ingest-timeout-ms <n>] <cases.json> <endpoint> [report.json]";

/// Default deadline for one Graphiti claim to materialize as an episode.
const DEFAULT_GRAPHITI_INGEST_TIMEOUT: Duration = Duration::from_secs(180);

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    Hindsight,
    Graphiti,
}

struct Arguments {
    provider: Provider,
    graphiti_ingest_timeout: Duration,
    fixture: PathBuf,
    endpoint: String,
    report: Option<PathBuf>,
}

fn parse_arguments(arguments: Vec<String>) -> Result<Arguments, String> {
    let mut provider = Provider::Hindsight;
    let mut graphiti_ingest_timeout = DEFAULT_GRAPHITI_INGEST_TIMEOUT;
    let mut positional: Vec<String> = Vec::new();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--provider" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("--provider needs a value\n{USAGE}"))?;
                provider = match value.as_str() {
                    "hindsight" => Provider::Hindsight,
                    "graphiti" => Provider::Graphiti,
                    other => return Err(format!("unknown provider {other:?}\n{USAGE}")),
                };
            }
            "--ingest-timeout-ms" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("--ingest-timeout-ms needs a value\n{USAGE}"))?;
                let millis: u64 = value.parse().map_err(|_| {
                    format!("--ingest-timeout-ms is not a number: {value:?}\n{USAGE}")
                })?;
                if millis == 0 {
                    return Err(format!("--ingest-timeout-ms must be positive\n{USAGE}"));
                }
                graphiti_ingest_timeout = Duration::from_millis(millis);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other:?}\n{USAGE}"));
            }
            _ => positional.push(argument),
        }
    }
    if positional.len() < 2 || positional.len() > 3 {
        return Err(USAGE.to_owned());
    }
    Ok(Arguments {
        provider,
        graphiti_ingest_timeout,
        fixture: PathBuf::from(&positional[0]),
        endpoint: positional[1].clone(),
        report: positional.get(2).map(PathBuf::from),
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

    let token = env::var("AIVTUBER_MEMORY_SIDECAR_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let sidecar: Box<dyn MemorySidecar> = match arguments.provider {
        Provider::Hindsight => {
            let mut config = HindsightConfig::new(arguments.endpoint.clone());
            if let Some(token) = &token {
                config = config.with_bearer_token(token.clone());
            }
            config.validate()?;
            Box::new(HindsightSidecar::new(config))
        }
        Provider::Graphiti => {
            let mut config = GraphitiConfig::new(arguments.endpoint.clone())
                .with_ingest_timeout(arguments.graphiti_ingest_timeout);
            if let Some(token) = &token {
                config = config.with_bearer_token(token.clone());
            }
            config.validate()?;
            Box::new(GraphitiSidecar::new(config))
        }
    };

    let outcome = evaluate_sidecar(sidecar.as_ref(), &fixture, &run_scope_prefix())?;
    let report = serde_json::to_string_pretty(&outcome.report)?;
    println!("{report}");
    if let Some(path) = &arguments.report {
        fs::write(path, format!("{report}\n"))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
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
