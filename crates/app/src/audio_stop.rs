//! The one place the app ends audio that already started (#277, #81 S2).
//!
//! Both the operator emergency path (`handle_control`, hard stop) and the
//! barge-in path (`service_barge_in`, short fade) end started audio through
//! [`AudioChannel::stop`], which calls the single `AudioOutput::stop`
//! primitive. The operator path has no feature flag and no dependency on
//! speech, floor, content or model state.
//!
//! A stop must never wedge the caller. The control handler runs on the
//! runtime thread, so a stuck audio backend (a driver call that never
//! returns) is handled by running the backend `stop` on a short-lived worker
//! thread and waiting a bounded time for its answer. When the bound passes
//! the caller gets [`AudioStopReport::Unconfirmed`], which is reported as
//! *not* confirmed silent; the worker keeps going and finishes the stop
//! whenever the backend returns. While a stop is stuck, dispatch never blocks
//! on the output either (`try_lock`): a command meant for a cancelled
//! generation is dropped anyway.

use crate::AudioOutput;
use aivtuber_adapters::AudioStopOutcome;
use aivtuber_domain::{EngineError, EngineErrorKind};
use aivtuber_runtime::AudioPlaybackCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread;
use std::time::Duration;

/// How long a control handler waits for the audio backend to answer a stop.
/// A process backend answers in about 2 ms; this only bounds a stuck one.
pub const AUDIO_STOP_BOUND: Duration = Duration::from_millis(100);

/// What an app-level audio stop achieved, honest about every non-success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStopReport {
    /// The backend answered; see [`AudioStopOutcome`].
    Outcome(AudioStopOutcome),
    /// The backend reported an error.
    Failed(String),
    /// The backend did not answer within the bound (or a previous stop is
    /// still stuck). Audio may still be audible.
    Unconfirmed,
}

impl AudioStopReport {
    /// Stable token for audit records and operator responses.
    pub fn label(&self) -> String {
        match self {
            Self::Outcome(AudioStopOutcome::NothingPlaying) => "nothing_playing".to_owned(),
            Self::Outcome(AudioStopOutcome::Faded { fade_ms }) => format!("faded_{fade_ms}ms"),
            Self::Outcome(AudioStopOutcome::HardStopped) => "hard_stopped".to_owned(),
            Self::Outcome(AudioStopOutcome::Unsupported) => "unsupported".to_owned(),
            Self::Failed(_) => "failed".to_owned(),
            Self::Unconfirmed => "unconfirmed".to_owned(),
        }
    }

    /// Whether the backend confirmed that no started audio remains (or
    /// remains only for the requested fade). `Unsupported`, errors and
    /// timeouts never count.
    pub fn confirmed_silent(&self) -> bool {
        matches!(
            self,
            Self::Outcome(
                AudioStopOutcome::NothingPlaying
                    | AudioStopOutcome::Faded { .. }
                    | AudioStopOutcome::HardStopped
            )
        )
    }
}

/// What an operator stop/mute did to the already-started outputs, for the
/// audit record and the operator response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorStopReport {
    pub audio: AudioStopReport,
    /// Whether the avatar `Reset` (return to neutral) was delivered.
    pub avatar_reset: bool,
}

impl OperatorStopReport {
    /// Audit/response fragment. Contains tokens only, never backend text.
    pub fn detail(&self) -> String {
        format!(
            "audio_stop={} avatar_reset={}",
            self.audio.label(),
            if self.avatar_reset { "ok" } else { "failed" }
        )
    }

    /// Audit decision label: success is only claimed when the backend
    /// confirmed it.
    pub fn audit_decision(&self) -> &'static str {
        if self.audio.confirmed_silent() {
            "operator_audio_stopped"
        } else {
            "operator_audio_stop_unconfirmed"
        }
    }
}

/// Owns the audio output behind a lock the stop worker can take without the
/// runtime thread ever blocking on it.
pub(crate) struct AudioChannel {
    output: Arc<Mutex<Box<dyn AudioOutput>>>,
    /// Fixed at composition: whether the output queues gaplessly (#83 S3d).
    /// Read without the output lock, which a stop may be holding.
    gapless: bool,
    stop_in_flight: Arc<AtomicBool>,
}

impl AudioChannel {
    pub(crate) fn new(output: Box<dyn AudioOutput>) -> Self {
        Self {
            gapless: output.gapless_queue(),
            output: Arc::new(Mutex::new(output)),
            stop_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn gapless_queue(&self) -> bool {
        self.gapless
    }

    /// Dispatch one playback command. Never blocks: while a stop holds the
    /// output the command is refused as unavailable.
    pub(crate) fn execute(&self, command: &AudioPlaybackCommand) -> Result<(), EngineError> {
        match self.output.try_lock() {
            Ok(mut output) => output.execute(command),
            Err(TryLockError::Poisoned(poison)) => poison.into_inner().execute(command),
            Err(TryLockError::WouldBlock) => Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "audio output busy ending playback",
            )),
        }
    }

    /// Ask the output to stop and wait at most `bound` for its answer.
    pub(crate) fn stop(&self, fade_ms: u32, bound: Duration) -> AudioStopReport {
        // One stop at a time: repeated stops against a stuck backend must not
        // pile up threads.
        if self.stop_in_flight.swap(true, Ordering::AcqRel) {
            return AudioStopReport::Unconfirmed;
        }
        let (answer, receiver) = mpsc::sync_channel(1);
        let output = Arc::clone(&self.output);
        let in_flight = Arc::clone(&self.stop_in_flight);
        let spawned = thread::Builder::new()
            .name("audio-stop".to_owned())
            .spawn(move || {
                // Cleared on every exit, including a panicking backend.
                struct Clear(Arc<AtomicBool>);
                impl Drop for Clear {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _clear = Clear(in_flight);
                let result = output
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .stop(fade_ms);
                let _ = answer.send(result);
            });
        if let Err(error) = spawned {
            self.stop_in_flight.store(false, Ordering::Release);
            return AudioStopReport::Failed(error.to_string());
        }
        match receiver.recv_timeout(bound) {
            Ok(Ok(outcome)) => AudioStopReport::Outcome(outcome),
            Ok(Err(error)) => AudioStopReport::Failed(error.to_string()),
            Err(_) => AudioStopReport::Unconfirmed,
        }
    }
}
