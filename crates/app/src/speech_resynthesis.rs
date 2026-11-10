//! Lazy re-synthesis of stale generated speech (issue #222 S2, ADR-0004 D-4).
//!
//! The composition thread never blocks on TTS: one dedicated worker takes one
//! job at a time (the queue in front of it is the bounded, deduplicated
//! `SpeechFreshness` queue), runs `GenerativePipeline::resynthesize` with the
//! public-output policy snapshot taken when the job was submitted, and hands the
//! result back. A failure of any kind only marks the request failed; the stale
//! asset stays playable and the performer is never waiting on this worker.

use crate::gate_generated_text;
use aivtuber_asset_store::PerformanceAsset;
use aivtuber_generative::{GenerativePipeline, ResynthesisError};
use aivtuber_runtime::PublicOutputPolicy;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One synthesis, not the whole reply: bounds a hung provider.
const RESYNTHESIS_TTS_TIMEOUT: Duration = Duration::from_secs(12);

struct Job {
    asset: PerformanceAsset,
    policy: PublicOutputPolicy,
}

/// A finished (or failed) re-synthesis.
pub(crate) struct Done {
    pub asset_id: String,
    pub result: Result<PerformanceAsset, ResynthesisError>,
}

pub(crate) struct ResynthesisWorker {
    tx: Option<SyncSender<Job>>,
    rx: Receiver<Done>,
    in_flight: bool,
    handle: Option<JoinHandle<()>>,
}

impl ResynthesisWorker {
    pub(crate) fn spawn(pipeline: Arc<GenerativePipeline>) -> Self {
        let (tx, job_rx) = mpsc::sync_channel::<Job>(1);
        let (done_tx, rx) = mpsc::sync_channel::<Done>(1);
        let handle = thread::Builder::new()
            .name("aivtuber-resynthesis-worker".to_owned())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let asset_id = job.asset.id.clone();
                    let result =
                        pipeline.resynthesize(&job.asset, Some(RESYNTHESIS_TTS_TIMEOUT), |text| {
                            gate_generated_text(&job.policy, text).0
                        });
                    if done_tx.send(Done { asset_id, result }).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Self {
            tx: Some(tx),
            rx,
            in_flight: false,
            handle,
        }
    }

    /// Whether a job is out (the single slot is taken).
    pub(crate) fn is_busy(&self) -> bool {
        self.in_flight
    }

    /// Hand one job over. `false` when the worker is busy, shut down or gone;
    /// the caller then marks the request failed.
    pub(crate) fn submit(&mut self, asset: PerformanceAsset, policy: PublicOutputPolicy) -> bool {
        if self.in_flight || self.handle.is_none() {
            return false;
        }
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        match tx.try_send(Job { asset, policy }) {
            Ok(()) => {
                self.in_flight = true;
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    /// The finished job, if any.
    pub(crate) fn poll(&mut self) -> Option<Done> {
        let done = self.rx.try_recv().ok()?;
        self.in_flight = false;
        Some(done)
    }

    /// Stop accepting work. A job already inside a provider call finishes
    /// detached; its result is never applied.
    pub(crate) fn shutdown(&mut self) {
        self.tx.take();
    }
}

impl Drop for ResynthesisWorker {
    fn drop(&mut self) {
        self.shutdown();
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
    }
}
