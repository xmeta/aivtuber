//! Local Japanese speech-to-text building blocks (issue #92, slice S3b).
//!
//! The speech sidecar (S2) cuts VAD speech into segments ([`feeder`]) and
//! feeds them, as 16 kHz PCM plus segment control ([`protocol`]), to a
//! separate `speech-stt-worker` child it supervises ([`supervisor`]). The
//! worker ([`worker`]) re-decodes the open segment about every 500 ms of audio
//! for partials, decodes once for the final, and writes one
//! `speech-transcript` batch (schemas/speech-transcript.schema.json) per
//! revision plus a heartbeat on stdout. The sidecar relays the transcript
//! lines to the app on its own stdout and reports a failed worker with one
//! [`SttStatus`] control line. A crash, hang or out-of-memory kill of the
//! recognizer therefore cannot take the VAD edges, or the deterministic floor
//! fallback that depends on them, down with it (decision Q3).
//!
//! Invariants of this module tree:
//! * Audio crosses exactly one local pipe (sidecar to worker), is never
//!   written to disk and never leaves the two processes. `Debug` output of
//!   every audio-bearing type shows lengths only.
//! * Transcript text is the engine's output unmodified (Q9).
//! * Nothing here decides anything: the output is untrusted content the app
//!   tracker classifies (source class speech, plane content, untrusted).
//! * Models and engine libraries must match SHA-256 pins compiled in
//!   ([`pins`]); nothing is downloaded at build or run time.

pub mod feeder;
pub mod pins;
pub mod protocol;
#[cfg(feature = "sherpa-stt")]
pub mod sherpa;
pub mod supervisor;
pub mod worker;

use serde::{Deserialize, Serialize};

/// Provider id on every transcript line.
pub const STT_PROVIDER_ID: &str = "speech-stt-worker";
/// Recorded as `provider_version`: the engine and its version, because both
/// can change the text.
pub const STT_PROVIDER_VERSION: &str = "sherpa-onnx-2.0.1";

/// Why the transcript path is unavailable. The sidecar sends exactly one
/// `unavailable` status per worker, and does not restart the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SttUnavailableReason {
    /// The worker could not be started (missing binary, library pin mismatch).
    WorkerFailedToStart,
    /// The worker process ended.
    WorkerExited,
    /// The worker exited because a decode ran past its deadline.
    DecodeDeadlineExceeded,
    /// The worker closed its output without exiting.
    WorkerClosedOutput,
    /// The worker stopped reading audio.
    WorkerStdinClosed,
    /// 32 consecutive output lines were not valid transcripts.
    InvalidLines,
    /// No heartbeat within the timeout: the worker is hung or never loaded.
    HeartbeatMissing,
}

impl SttUnavailableReason {
    pub const fn name(self) -> &'static str {
        match self {
            Self::WorkerFailedToStart => "worker_failed_to_start",
            Self::WorkerExited => "worker_exited",
            Self::DecodeDeadlineExceeded => "decode_deadline_exceeded",
            Self::WorkerClosedOutput => "worker_closed_output",
            Self::WorkerStdinClosed => "worker_stdin_closed",
            Self::InvalidLines => "invalid_lines",
            Self::HeartbeatMissing => "heartbeat_missing",
        }
    }
}

/// Control line from the sidecar to the app on the shared stdout pipe, next
/// to `speech-observation` and `speech-transcript` batches:
/// `{"stt":{"state":"ready"}}` or
/// `{"stt":{"state":"unavailable","reason":"worker_exited"}}`. Not part of
/// either normative schema, like the stdin `clock_sync` line: it carries no
/// content, only the health of the transcript path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SttStatus {
    Ready,
    Unavailable { reason: SttUnavailableReason },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SttStatusLine {
    stt: SttStatus,
}

pub fn encode_stt_status_line(status: SttStatus) -> String {
    let mut line =
        serde_json::to_string(&SttStatusLine { stt: status }).expect("status is serializable");
    line.push('\n');
    line
}

/// `Some` only for a well-formed status line; anything else is not one.
pub fn parse_stt_status_line(bytes: &[u8]) -> Option<SttStatus> {
    serde_json::from_slice::<SttStatusLine>(bytes)
        .ok()
        .map(|line| line.stt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_round_trip_and_nothing_else_parses() {
        for status in [
            SttStatus::Ready,
            SttStatus::Unavailable {
                reason: SttUnavailableReason::HeartbeatMissing,
            },
        ] {
            let line = encode_stt_status_line(status);
            assert!(line.ends_with('\n'));
            assert_eq!(
                parse_stt_status_line(line.trim_end().as_bytes()),
                Some(status)
            );
        }
        for bad in [
            &br#"{"stt":{"state":"unavailable","reason":"made_up"}}"#[..],
            br#"{"stt":{"state":"ready"},"x":1}"#,
            br#"{"stt":{"state":"restart"}}"#,
            br#"{"observations":[]}"#,
            b"not json",
        ] {
            assert_eq!(parse_stt_status_line(bad), None);
        }
        assert_eq!(
            encode_stt_status_line(SttStatus::Unavailable {
                reason: SttUnavailableReason::DecodeDeadlineExceeded
            }),
            "{\"stt\":{\"state\":\"unavailable\",\"reason\":\"decode_deadline_exceeded\"}}\n"
        );
    }
}
