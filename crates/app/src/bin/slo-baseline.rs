//! SLO baseline-selection CLI — issue #71.
//!
//! Pools a history of operational SLO reports (`slo-report` output, #58's
//! measured runs) into per-indicator calibration evidence, so a target can be
//! chosen from repeated measurements instead of invented. It emits
//! [`BaselineProposal`] evidence, never a target: the target ratio stays a
//! product decision made above the measured value.
//!
//! Usage:
//! ```text
//! slo-baseline <slo-report.json>... [--out <proposals.json>] [--markdown <summary.md>]
//! ```

#![forbid(unsafe_code)]

use aivtuber_telemetry::{BaselineProposalSet, SloError, SloReport};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
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
        let report: SloReport = serde_json::from_slice(&fs::read(path)?).map_err(|error| {
            SloError::new(format!("{} is not an SLO report: {error}", path.display()))
        })?;
        history.push(report);
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

fn write(path: &PathBuf, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
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
