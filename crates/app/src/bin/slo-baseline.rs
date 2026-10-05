//! SLO baseline-selection CLI — issue #71.
//!
//! Pools a history of operational SLO reports (`slo-report` output, #58's
//! measured runs) into per-indicator calibration evidence, so a target can be
//! chosen from repeated measurements instead of invented. It emits
//! [`BaselineProposal`] evidence, never a target: the target ratio stays a
//! product decision made above the measured value.
//!
//! Rules enforced here (see `docs/operational-slo.adoc`):
//!
//! * every input must belong to one #58 compatibility series
//!   (`mode + dataset_id + config_version + seed`);
//! * every input must carry a run identity (`slo-report --run-id`): runs are
//!   counted by identity, never by report content, because two independent
//!   deterministic runs produce identical reports and a retry may not;
//! * inputs repeating a run identity are one run — the last artifact supplied
//!   wins, as a #58 re-run replaces its row;
//! * fewer than [`MIN_BASELINE_RUNS`] distinct runs produce no proposals;
//! * represented stream time is summed exactly (no per-run ceil).
//!
//! Latency calibration is a two-step cycle, so the first latency target is
//! constructible entirely from measured artifacts:
//!
//! 1. `slo-report --probe-latency availability.event_to_first_audio_within_target=250`
//!    over each run, choosing the candidate boundary from the target-free
//!    percentiles;
//! 2. `slo-baseline` over the probed reports, which publishes the *measured*
//!    conforming ratio at that boundary as valid `BaselineEvidence`.
//!
//! Usage:
//! ```text
//! slo-baseline <slo-report.json>... [--out <proposals.json>] [--markdown <summary.md>]
//! ```

#![forbid(unsafe_code)]

use aivtuber_telemetry::{BaselineProposalSet, SloError, SloReport};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str =
    "usage: slo-baseline <slo-report.json>... [--out <proposals.json>] [--markdown <summary.md>]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("slo-baseline: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Arguments {
    reports: Vec<PathBuf>,
    output: Option<PathBuf>,
    markdown: Option<PathBuf>,
}

fn run() -> Result<(), Box<dyn Error>> {
    let Arguments {
        reports,
        output,
        markdown,
    } = parse_args(std::env::args().skip(1).collect())?;

    let mut history = Vec::with_capacity(reports.len());
    for path in &reports {
        history.push(read_report(path)?);
    }

    let proposals = BaselineProposalSet::from_reports(&history)?;

    if let Some(path) = &output {
        write(path, &proposals.to_json_pretty()?)?;
    }
    if let Some(path) = &markdown {
        write(path, proposals.markdown_summary().as_bytes())?;
    }

    print!("{}", proposals.markdown_summary());
    Ok(())
}

fn read_report(path: &Path) -> Result<SloReport, Box<dyn Error>> {
    serde_json::from_slice(&fs::read(path)?).map_err(|error| {
        SloError::new(format!("{} is not an SLO report: {error}", path.display())).into()
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<Arguments, Box<dyn Error>> {
    let mut reports = Vec::new();
    let mut output: Option<PathBuf> = None;
    let mut markdown: Option<PathBuf> = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--out" => {
                output = Some(PathBuf::from(required_value(&args, index, "--out")?));
                index += 2;
            }
            "--markdown" => {
                markdown = Some(PathBuf::from(required_value(&args, index, "--markdown")?));
                index += 2;
            }
            unknown if unknown.starts_with("--") => {
                return Err(format!("unknown argument {unknown:?}\n{USAGE}").into());
            }
            path => {
                reports.push(PathBuf::from(path));
                index += 1;
            }
        }
    }

    if reports.is_empty() {
        return Err(format!("at least one SLO report is required\n{USAGE}").into());
    }
    Ok(Arguments {
        reports,
        output,
        markdown,
    })
}

fn required_value(args: &[String], index: usize, name: &str) -> Result<String, Box<dyn Error>> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("{name} requires a value\n{USAGE}").into())
}
