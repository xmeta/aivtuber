//! Composition of opt-in live screen perception (issue #90 S1b, ADR-0007).
//!
//! [`PerceptionService`] joins three things that deliberately know nothing of
//! each other: the runtime's [`PerceptionTracker`] (the bounded, untrusted,
//! expiring observation state), the runtime's [`PerceptionGovernor`] (cadence,
//! yield gate, latency probe; pure logic) and a worker that does the slow I/O
//! (OBS screenshot, loopback vision model). The worker runs on its own thread,
//! so the main loop, the operator control drain and the performer never wait
//! on OBS or on a model.
//!
//! Boundaries kept here:
//! * **Off by default.** The service is only composed when the operator opts
//!   in, and even then it starts `off`: [`PerceptionService::enable`] needs the
//!   authenticated `perception.control` capability (the tracker re-checks it).
//! * **One in flight.** The governor dispatches at most one job; the job
//!   channel holds at most one more only while an old, superseded job finishes.
//! * **Disable wins.** The live epoch the worker reads before it touches OBS is
//!   updated before anything else on disable, and a result that arrives after
//!   a disable is discarded by the governor.
//! * **Model output is data.** The worker returns a validated observation
//!   batch; it goes to the tracker, which assigns plane and trust. The service
//!   holds no scheduler, output, tool, memory or OBS control handle.
//! * **Nothing persisted.** The service keeps counters and the tracker's
//!   bounded state only; frames live in the worker's single slot and are gone
//!   after each model call.

use aivtuber_adapters::{ProducedObservation, ProducerError, ScreenObservationProducer};
use aivtuber_domain::{AuthenticatedControl, PerceptionObservationBatch};
use aivtuber_runtime::{
    JobOutcome, PerceptionAvailability, PerceptionConfig, PerceptionGateError, PerceptionGates,
    PerceptionGovernor, PerceptionGovernorConfig, PerceptionGovernorStatus, PerceptionJob,
    PerceptionMetrics, PerceptionStatus, PerceptionTracker, ResultDisposition, RuntimeError,
};
use aivtuber_telemetry::SecretRedactor;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;

/// What a worker reports for one job. `batch` is only ever `Some` for a
/// completed capture.
#[derive(Debug)]
pub struct JobReport {
    pub epoch: u64,
    pub outcome: JobOutcome,
    pub batch: Option<PerceptionObservationBatch>,
}

/// The slow half of perception. Runs on the worker thread, one job at a time.
pub trait PerceptionWorker: Send {
    /// Run `job`. `live_epoch` is the service's current epoch: a worker that
    /// finds it different from `epoch` must not touch OBS or the model and
    /// reports [`JobOutcome::Skipped`].
    fn run(&mut self, job: PerceptionJob, epoch: u64, live_epoch: &AtomicU64) -> JobReport;
}

/// The production worker: OBS screenshot, loopback model, repair, redaction.
pub struct ProducerWorker {
    producer: ScreenObservationProducer,
}

impl ProducerWorker {
    pub fn new(producer: ScreenObservationProducer) -> Self {
        Self { producer }
    }
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl PerceptionWorker for ProducerWorker {
    fn run(&mut self, job: PerceptionJob, epoch: u64, live_epoch: &AtomicU64) -> JobReport {
        let report = |outcome, batch| JobReport {
            epoch,
            outcome,
            batch,
        };
        if live_epoch.load(Ordering::SeqCst) != epoch {
            return report(JobOutcome::Skipped, None);
        }
        match job {
            PerceptionJob::Probe => match self.producer.probe() {
                Ok(latency) => report(
                    JobOutcome::ProbeDone {
                        latency_ms: millis(latency),
                    },
                    None,
                ),
                Err(_) => report(JobOutcome::ProbeFailed, None),
            },
            PerceptionJob::Capture => {
                if self.producer.capture().is_err() {
                    return report(JobOutcome::CaptureFailed, None);
                }
                // Disabled while OBS was answering: drop the frame without
                // spending GPU time on it.
                if live_epoch.load(Ordering::SeqCst) != epoch {
                    self.producer.discard_frame();
                    return report(JobOutcome::Skipped, None);
                }
                match self.producer.describe_held_frame() {
                    Ok(ProducedObservation {
                        batch,
                        model_latency_ms,
                        ..
                    }) => report(JobOutcome::CaptureDone { model_latency_ms }, Some(batch)),
                    Err(ProducerError::NoFrame) => report(JobOutcome::Skipped, None),
                    Err(_) => report(JobOutcome::CaptureFailed, None),
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PerceptionServiceConfig {
    /// The single operator-allowlisted source alias (ADR-0007 D-Q5).
    pub alias: String,
    pub governor: PerceptionGovernorConfig,
    /// Closed label vocabulary; `None` keeps the provisional one.
    pub label_vocabulary: Option<std::collections::BTreeSet<String>>,
}

/// Service-level counters on top of the tracker's and the governor's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PerceptionServiceMetrics {
    pub batches_ingested: u64,
    pub batches_refused: u64,
    pub late_observations_dropped: u64,
    pub jobs_dispatch_deferred: u64,
}

/// Operator-visible, text-free status of the perception path.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PerceptionStatusReport {
    pub governor: PerceptionGovernorStatus,
    pub availability: PerceptionAvailability,
    pub tracker: PerceptionMetrics,
    pub service: PerceptionServiceMetrics,
}

pub struct PerceptionService {
    tracker: PerceptionTracker,
    governor: PerceptionGovernor,
    alias: String,
    live_epoch: Arc<AtomicU64>,
    jobs: Option<SyncSender<(PerceptionJob, u64)>>,
    results: Receiver<JobReport>,
    metrics: PerceptionServiceMetrics,
}

impl std::fmt::Debug for PerceptionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerceptionService")
            .field("alias", &self.alias)
            .field("status", &self.governor.status())
            .finish_non_exhaustive()
    }
}

impl PerceptionService {
    /// Compose the service **disabled**. Spawns the worker thread, which idles
    /// until the first dispatch.
    pub fn start(
        config: PerceptionServiceConfig,
        mut worker: Box<dyn PerceptionWorker>,
        redactor: SecretRedactor,
    ) -> Result<Self, RuntimeError> {
        let mut tracker_config = PerceptionConfig::default();
        if let Some(vocabulary) = config.label_vocabulary {
            tracker_config.label_vocabulary = vocabulary;
        }
        tracker_config.source_privacy_floors = std::collections::BTreeMap::from([(
            config.alias.clone(),
            aivtuber_domain::PrivacyClass::Private,
        )]);
        let tracker = PerceptionTracker::new(tracker_config, redactor)?;
        let governor = PerceptionGovernor::new(config.governor)?;
        let live_epoch = Arc::new(AtomicU64::new(governor.epoch()));

        let (job_tx, job_rx) = mpsc::sync_channel::<(PerceptionJob, u64)>(1);
        let (result_tx, result_rx) = mpsc::sync_channel::<JobReport>(4);
        let worker_epoch = live_epoch.clone();
        thread::Builder::new()
            .name("perception-worker".to_owned())
            .spawn(move || {
                while let Ok((job, epoch)) = job_rx.recv() {
                    let report = worker.run(job, epoch, &worker_epoch);
                    if result_tx.send(report).is_err() {
                        break;
                    }
                }
            })
            .map_err(|_| RuntimeError::InvalidConfiguration("perception worker did not start"))?;
        Ok(Self {
            tracker,
            governor,
            alias: config.alias,
            live_epoch,
            jobs: Some(job_tx),
            results: result_rx,
            metrics: PerceptionServiceMetrics::default(),
        })
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn status(&self) -> PerceptionStatus {
        self.governor.status()
    }

    /// The tracker, for consumers (Reflex snapshot, the single Thinking
    /// caption item). Read-only access to what perception retained.
    pub fn tracker(&self) -> &PerceptionTracker {
        &self.tracker
    }

    pub fn tracker_mut(&mut self) -> &mut PerceptionTracker {
        &mut self.tracker
    }

    pub fn report(&self, now_ms: u64) -> PerceptionStatusReport {
        PerceptionStatusReport {
            governor: self.governor.report(),
            availability: self.tracker.availability(now_ms),
            tracker: self.tracker.metrics(),
            service: self.metrics,
        }
    }

    fn publish_epoch(&self) {
        self.live_epoch
            .store(self.governor.epoch(), Ordering::SeqCst);
    }

    /// Operator opt-in. Needs the authenticated `perception.control`
    /// capability; the tracker checks it and the allowlist. Restarts from the
    /// start latency probe, also after a suspension.
    pub fn enable(
        &mut self,
        authority: &AuthenticatedControl,
        now_ms: u64,
    ) -> Result<(), PerceptionGateError> {
        self.tracker.enable_source(authority, &self.alias, now_ms)?;
        self.governor.enable(now_ms);
        self.publish_epoch();
        Ok(())
    }

    /// Operator disable. Takes effect before the next capture: the live epoch
    /// moves first, then the tracker drops everything it retained.
    pub fn disable(&mut self, authority: &AuthenticatedControl) -> Result<(), PerceptionGateError> {
        // Refuse before changing anything if the authority is wrong.
        if !authority.has_capability(aivtuber_domain::Capability::PerceptionControl) {
            return self.tracker.disable_all(authority);
        }
        self.governor.disable();
        self.publish_epoch();
        self.tracker.disable_all(authority)
    }

    /// Stop everything and release the worker (process shutdown).
    pub fn shutdown(&mut self) {
        self.governor.disable();
        self.publish_epoch();
        self.tracker.suspend_all();
        self.jobs = None;
    }

    /// Drain finished jobs, then start the next one if the governor allows.
    /// Cheap; call it every loop tick. Never blocks.
    pub fn service(&mut self, now_ms: u64, gates: PerceptionGates) {
        while let Ok(report) = self.results.try_recv() {
            let disposition = self
                .governor
                .on_result(report.epoch, report.outcome, now_ms);
            match (disposition, report.batch) {
                (ResultDisposition::Ingest, Some(batch)) => self.ingest(&batch, now_ms),
                (ResultDisposition::DropLate, Some(_)) => {
                    self.metrics.late_observations_dropped += 1;
                }
                _ => {}
            }
        }
        // A suspension by the governor also drops everything retained and
        // closes the tracker's gate, so no stale caption stays consumable.
        if self.governor.status() == PerceptionStatus::Suspended
            && self.tracker.availability(now_ms) != PerceptionAvailability::Disabled
        {
            self.tracker.suspend_all();
        }
        self.publish_epoch();
        if let Some(dispatch) = self.governor.poll(now_ms, gates) {
            let sent = self
                .jobs
                .as_ref()
                .map(|jobs| jobs.try_send((dispatch.job, dispatch.epoch)));
            if !matches!(sent, Some(Ok(()))) {
                // Queue full (a superseded job is still finishing) or the
                // worker is gone: release the slot and retry next tick.
                self.metrics.jobs_dispatch_deferred += 1;
                if matches!(sent, Some(Err(TrySendError::Disconnected(_))) | None) {
                    self.governor
                        .suspend(aivtuber_runtime::SuspendReason::RepeatedFailures);
                    self.tracker.suspend_all();
                    self.publish_epoch();
                } else {
                    self.governor
                        .on_result(dispatch.epoch, JobOutcome::Skipped, now_ms);
                }
            }
        }
    }

    fn ingest(&mut self, batch: &PerceptionObservationBatch, now_ms: u64) {
        // The worker stamps observations on the same monotonic base, but it
        // may have read its clock after the caller read `now_ms`; never let
        // that ordering make a fresh observation look future-dated.
        let stamped = batch
            .observations
            .iter()
            .map(|observation| observation.observed_at_ms)
            .max()
            .unwrap_or(0);
        match self.tracker.ingest_batch(batch, now_ms.max(stamped)) {
            Ok(_) => self.metrics.batches_ingested += 1,
            Err(_) => self.metrics.batches_refused += 1,
        }
    }
}
