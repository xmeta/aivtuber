//! Test double for the speech sidecar protocol (#92 S2). It speaks the same
//! pipes as `speech-vad-sidecar` without a microphone or a model, so the app
//! supervisor can be tested against a crashing, flooding or silent child.
//!
//! `speech-sidecar-double <mode>`:
//! * `clock`: for every `{"clock_sync":...}` line on stdin, print one batch
//!   stamped with exactly that runtime time (alternating start/end), until
//!   stdin closes.
//! * `crash`: wait for the first sync, print one batch, exit with status 3.
//! * `garbage`: print junk lines as fast as the pipe accepts them.
//! * `flood`: print valid but long-stale batches as fast as the pipe accepts.
//! * `oversize`: print lines far above the batch size limit.
//! * `hang`: neither read nor write nor exit.
//!
//! Transcript modes (#92 S3b), also clocked by the sync lines: `transcripts`
//! (a ready status, then per segment two partials and a final, each stamped
//! with the sync time), `stt-fail` (edges, a ready status, then an
//! unavailable status, then transcript lines that must be ignored while the
//! edges continue) and `transcript-garbage` (valid edges plus two invalid
//! transcript lines per sync).

use aivtuber_adapters::speech_vad::pump_clock_syncs;
use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;

fn batch(edge: &str, observed_at_ms: u64) -> String {
    format!(
        "{{\"schema_version\":\"0.1.0\",\"provider_id\":\"sidecar-double\",\"provider_version\":\"1\",\"language\":\"ja\",\"observations\":[{{\"kind\":\"speech_activity\",\"speaker_id\":\"double\",\"edge\":\"{edge}\",\"observed_at_ms\":{observed_at_ms}}}]}}\n"
    )
}

fn transcript(segment: u64, revision: u32, kind: &str, text: &str, at: u64) -> String {
    let complete = kind == "final";
    format!(
        "{{\"schema_version\":\"0.1.0\",\"provider_id\":\"stt-double\",\"provider_version\":\"1\",\"language\":\"ja\",\"engine\":{{\"model_id\":\"double:1\",\"sha256\":\"{}\"}},\"transcripts\":[{{\"speaker_id\":\"double\",\"segment_id\":{segment},\"revision\":{revision},\"kind\":\"{kind}\",\"text\":\"{text}\",\"covered_audio_ms\":{},\"observed_at_ms\":{at},\"complete\":{complete}}}]}}\n",
        "0".repeat(64),
        u64::from(revision + 1) * 500
    )
}

fn emit(out: &mut impl Write, line: &str) {
    let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
}

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let mut out = io::stdout().lock();
    match mode.as_str() {
        "clock" => {
            let mut start = true;
            let _ = pump_clock_syncs(BufReader::new(io::stdin().lock()), |runtime_now_ms| {
                let line = batch(if start { "start" } else { "end" }, runtime_now_ms);
                start = !start;
                let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
            });
            ExitCode::SUCCESS
        }
        "transcripts" => {
            let mut n = 0_u64;
            let _ = pump_clock_syncs(BufReader::new(io::stdin().lock()), |at| {
                if n == 0 {
                    emit(&mut out, "{\"stt\":{\"state\":\"ready\"}}\n");
                }
                let (segment, step) = (n / 3, (n % 3) as u32);
                let line = match step {
                    0 => transcript(segment, 0, "partial", "き", at),
                    1 => transcript(segment, 1, "partial", "きょう", at),
                    _ => transcript(segment, 2, "final", "きょうは", at),
                };
                emit(&mut out, &line);
                n += 1;
            });
            ExitCode::SUCCESS
        }
        "stt-fail" => {
            let mut n = 0_u64;
            let _ = pump_clock_syncs(BufReader::new(io::stdin().lock()), |at| {
                emit(
                    &mut out,
                    &batch(if n.is_multiple_of(2) { "start" } else { "end" }, at),
                );
                match n {
                    0 => emit(&mut out, "{\"stt\":{\"state\":\"ready\"}}\n"),
                    2 => emit(
                        &mut out,
                        "{\"stt\":{\"state\":\"unavailable\",\"reason\":\"worker_exited\"}}\n",
                    ),
                    4.. => emit(&mut out, &transcript(0, 0, "partial", "き", at)),
                    _ => {}
                }
                n += 1;
            });
            ExitCode::SUCCESS
        }
        "transcript-garbage" => {
            let mut n = 0_u64;
            let _ = pump_clock_syncs(BufReader::new(io::stdin().lock()), |at| {
                emit(
                    &mut out,
                    &batch(if n.is_multiple_of(2) { "start" } else { "end" }, at),
                );
                emit(&mut out, "{\"transcripts\":[{\"bad\":1}]}\n");
                emit(&mut out, "{\"transcripts\":\"nope\"}\n");
                n += 1;
            });
            ExitCode::SUCCESS
        }
        "crash" => {
            let mut first_line = String::new();
            let _ = io::stdin().lock().read_line(&mut first_line);
            let at = aivtuber_adapters::speech_vad::parse_clock_sync_line(
                first_line.trim_end().as_bytes(),
            );
            let _ = out.write_all(batch("start", at.unwrap_or(0)).as_bytes());
            let _ = out.flush();
            ExitCode::from(3)
        }
        "garbage" => flood(&mut out, b"this is not a speech observation\n"),
        "flood" => flood(&mut out, batch("start", 0).as_bytes()),
        "oversize" => {
            let mut line = vec![b'x'; 256 * 1024];
            line.push(b'\n');
            flood(&mut out, &line)
        }
        "hang" => loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        },
        _ => {
            eprintln!(
                "usage: speech-sidecar-double clock|crash|garbage|flood|oversize|hang|transcripts|stt-fail|transcript-garbage"
            );
            ExitCode::FAILURE
        }
    }
}

/// Write `line` until the reader goes away.
fn flood(out: &mut impl Write, line: &[u8]) -> ExitCode {
    loop {
        if out.write_all(line).is_err() {
            return ExitCode::SUCCESS;
        }
    }
}
