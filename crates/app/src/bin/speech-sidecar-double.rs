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

use aivtuber_adapters::speech_vad::pump_clock_syncs;
use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;

fn batch(edge: &str, observed_at_ms: u64) -> String {
    format!(
        "{{\"schema_version\":\"0.1.0\",\"provider_id\":\"sidecar-double\",\"provider_version\":\"1\",\"language\":\"ja\",\"observations\":[{{\"kind\":\"speech_activity\",\"speaker_id\":\"double\",\"edge\":\"{edge}\",\"observed_at_ms\":{observed_at_ms}}}]}}\n"
    )
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
            eprintln!("usage: speech-sidecar-double clock|crash|garbage|flood|oversize|hang");
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
