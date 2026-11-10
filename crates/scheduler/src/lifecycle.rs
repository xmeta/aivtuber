//! Bounded speech lifecycle log and state machine (issue #222 S2, ADR-0004 D5).
//!
//! The scheduler owns the public truth of what was prepared, committed,
//! started, passed a safe point, finished or cancelled. This module is the
//! only producer of [`SpeechLifecycleEvent`] and the only writer of `seq`; the
//! composition root and the output adapters are *reporters*: they pass
//! [`OutputObservation`] values back through the scheduler and never emit
//! events themselves. Provider-specific signals (process exit, a vendor
//! "finished" callback) are mapped to [`OutputObservation`] inside adapters and
//! never cross this boundary.
//!
//! Transition rules (everything else is a typed [`LifecycleError`], never a
//! panic):
//!
//! ```text
//! Prepared -> Committed -> Started -> (Boundary)* -> Finished
//!     \            \            \----------------> Cancelled
//!      \            \------------------------------> Cancelled
//!       \------------------------------------------> Cancelled
//! ```
//!
//! `Started` needs `Committed`, `Finished` needs `Started`, nothing follows a
//! terminal state, and a unit's `at_ms` never decreases. `Cancelled.reached` is
//! computed here from the unit's stage, so a reporter cannot claim a state the
//! unit never reached, and a cancelled unit is never reported as finished.
//!
//! Boundedness: live state is capped ([`MAX_LIVE_SPEECH`]); recently terminal
//! ids are remembered in a FIFO ring ([`TERMINAL_MEMORY`]) only to tell "after
//! terminal" from "unknown"; the outbound queue is a ring that drops a
//! `Boundary` first and otherwise the oldest event, counting every drop. A slow
//! consumer detects loss through `seq` gaps and re-synchronizes from
//! [`SpeechLifecycleSnapshot`].

use aivtuber_domain::{
    SPEECH_LIFECYCLE_SCHEMA_VERSION, SpeechCancelReason, SpeechLifecycleEvent, SpeechPhase,
    SpeechReached, SpeechRef, SpeechTimeBasis, clip_lifecycle_label,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

/// Most speech units that may be live (prepared, committed or started) at once.
pub const MAX_LIVE_SPEECH: usize = 256;
/// Terminal unit ids remembered so a stray transition is `AfterTerminal`.
pub const TERMINAL_MEMORY: usize = 256;
/// Default capacity of the outbound event queue.
pub const DEFAULT_LIFECYCLE_QUEUE_CAPACITY: usize = 1024;

/// What an output adapter observed about one command, normalized so no
/// provider type crosses the scheduler boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputObservation {
    /// The adapter accepted the command (a backend-return proxy, not
    /// first-audible time).
    Accepted,
    /// The adapter reported that playback drained.
    Drained,
    /// The command did not reach the speaker.
    Failed(OutputFailureKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFailureKind {
    /// The adapter returned an error.
    AdapterError,
    /// The command was dropped because output is muted.
    Muted,
    /// The command was dropped as late or no longer live.
    StaleOrLate,
}

impl OutputFailureKind {
    fn reason(self) -> SpeechCancelReason {
        match self {
            Self::AdapterError => SpeechCancelReason::OutputFailure,
            Self::Muted => SpeechCancelReason::Muted,
            Self::StaleOrLate => SpeechCancelReason::StaleOrLate,
        }
    }
}

/// Why a lifecycle call was refused. Every refusal is counted; none panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// No live unit has that id (never prepared, or terminal long ago).
    UnknownSpeech,
    /// The unit is already terminal; nothing may follow `Finished` or
    /// `Cancelled`.
    AfterTerminal,
    /// The transition is not allowed from the unit's current stage.
    InvalidTransition { from: SpeechStage, to: &'static str },
    /// The unit's logical time would move backwards.
    NonMonotonicTime,
    /// The bounded live set is full.
    LiveCapacity,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSpeech => f.write_str("unknown speech unit"),
            Self::AfterTerminal => f.write_str("speech unit is already terminal"),
            Self::InvalidTransition { from, to } => {
                write!(f, "cannot go from {from:?} to {to}")
            }
            Self::NonMonotonicTime => f.write_str("logical time moved backwards"),
            Self::LiveCapacity => f.write_str("live speech capacity reached"),
        }
    }
}

impl std::error::Error for LifecycleError {}

/// Where a live unit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SpeechStage {
    Prepared,
    Committed,
    Started,
}

/// What a reporter knows when audio is prepared. The scheduler derives the
/// rest (generation, time, sequence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechPrepared {
    /// The scheduler item's event id (phrase items derive `{root}#p{seq}`).
    pub event_id: String,
    pub asset_id: String,
    pub phrase: u32,
    pub final_phrase: bool,
    /// SHA-256 (lowercase hex) of the text sent to TTS, never the text.
    pub speech_text_digest: Option<String>,
    /// `compatibility.speech_projection` of the asset, when recorded.
    pub projection: Option<String>,
}

#[derive(Debug, Clone)]
struct LiveSpeech {
    reference: SpeechRef,
    stage: SpeechStage,
    last_at_ms: u64,
    started_at_ms: Option<u64>,
    boundaries_emitted: usize,
    /// Strongest output evidence seen for this unit.
    basis: SpeechTimeBasis,
}

/// One live unit as a consumer sees it in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSpeechView {
    pub speech: SpeechRef,
    pub stage: SpeechStage,
    pub started_at_ms: Option<u64>,
}

/// Re-synchronization view for a consumer that saw a `seq` gap: every live
/// unit plus the events still queued (both bounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechLifecycleSnapshot {
    pub next_seq: u64,
    pub dropped: u64,
    pub live: Vec<LiveSpeechView>,
    pub queued: Vec<SpeechLifecycleEvent>,
}

/// Counters for telemetry. Cumulative since process start, except `live` and
/// `queued`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeechLifecycleMetrics {
    pub emitted: u64,
    /// Events dropped from the full outbound queue.
    pub dropped: u64,
    /// Calls refused with a [`LifecycleError`] other than `AfterTerminal`.
    pub rejected: u64,
    /// Scheduler transitions that arrived after the unit was already terminal
    /// (for example `Finished` after an output failure cancelled it).
    pub suppressed_after_terminal: u64,
    /// Output reports that arrived after the unit had already finished or was
    /// unknown (the scheduler had retired it).
    pub late_output_reports: u64,
    pub live: usize,
    pub queued: usize,
    pub prepared: u64,
    pub committed: u64,
    pub started: u64,
    pub finished: u64,
    pub cancelled_before_commit: u64,
    pub cancelled_committed: u64,
    pub cancelled_started: u64,
}

/// The single owner of speech lifecycle truth. See the module documentation.
#[derive(Debug)]
pub struct SpeechLifecycleLog {
    next_seq: u64,
    live: BTreeMap<String, LiveSpeech>,
    terminal: VecDeque<String>,
    terminal_set: BTreeSet<String>,
    queue: VecDeque<SpeechLifecycleEvent>,
    /// Events written while a batch is open, ordered and given their `seq` when
    /// the batch ends (see [`SpeechLifecycleLog::begin_batch`]).
    batch: Option<Vec<SpeechLifecycleEvent>>,
    capacity: usize,
    metrics: SpeechLifecycleMetrics,
}

impl Default for SpeechLifecycleLog {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_LIFECYCLE_QUEUE_CAPACITY)
    }
}

impl SpeechLifecycleLog {
    /// A log whose outbound queue holds at most `capacity` events (at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            next_seq: 1,
            live: BTreeMap::new(),
            terminal: VecDeque::new(),
            terminal_set: BTreeSet::new(),
            queue: VecDeque::new(),
            batch: None,
            capacity: capacity.max(1),
            metrics: SpeechLifecycleMetrics::default(),
        }
    }

    /// Open a batch: state transitions apply immediately, but the events they
    /// produce are held until [`Self::end_batch`], which emits them in logical
    /// order (time, then terminal before start so a hand-off reads as
    /// finished-then-started, then scheduler generation) while keeping each
    /// unit's own order. The scheduler wraps one logical-time advance in a
    /// batch because its internal slot order is not time order.
    pub fn begin_batch(&mut self) {
        if self.batch.is_none() {
            self.batch = Some(Vec::new());
        }
    }

    /// Close the batch and emit its events.
    pub fn end_batch(&mut self) {
        let Some(held) = self.batch.take() else {
            return;
        };
        let mut per_unit: BTreeMap<String, VecDeque<SpeechLifecycleEvent>> = BTreeMap::new();
        for event in held {
            per_unit
                .entry(event.speech.event_id.clone())
                .or_default()
                .push_back(event);
        }
        loop {
            let next = per_unit
                .iter()
                .filter_map(|(id, queue)| queue.front().map(|event| (batch_key(event), id)))
                .min()
                .map(|(_, id)| id.clone());
            let Some(id) = next else {
                break;
            };
            if let Some(event) = per_unit.get_mut(&id).and_then(VecDeque::pop_front) {
                self.push_event(event);
            }
        }
    }

    pub fn metrics(&self) -> SpeechLifecycleMetrics {
        SpeechLifecycleMetrics {
            live: self.live.len(),
            queued: self.queue.len(),
            ..self.metrics
        }
    }

    /// Whether the unit is live (prepared, committed or started).
    pub fn is_live(&self, event_id: &str) -> bool {
        self.live.contains_key(event_id)
    }

    /// Ids of every live unit, in id order.
    pub fn live_ids(&self) -> impl Iterator<Item = &String> {
        self.live.keys()
    }

    /// Count an output report that arrived for a unit the scheduler had
    /// already retired.
    pub(crate) fn note_late_output_report(&mut self) {
        self.metrics.late_output_reports += 1;
    }

    /// The stage of a live unit.
    pub fn stage(&self, event_id: &str) -> Option<SpeechStage> {
        self.live.get(event_id).map(|speech| speech.stage)
    }

    pub fn snapshot(&self) -> SpeechLifecycleSnapshot {
        SpeechLifecycleSnapshot {
            next_seq: self.next_seq,
            dropped: self.metrics.dropped,
            live: self
                .live
                .values()
                .map(|speech| LiveSpeechView {
                    speech: speech.reference.clone(),
                    stage: speech.stage,
                    started_at_ms: speech.started_at_ms,
                })
                .collect(),
            queued: self.queue.iter().cloned().collect(),
        }
    }

    /// Remove and return up to `max` queued events in `seq` order.
    pub fn drain(&mut self, max: usize) -> Vec<SpeechLifecycleEvent> {
        let take = max.min(self.queue.len());
        self.queue.drain(..take).collect()
    }

    /// Audio for a unit was prepared (TTS finished, or a cached asset was
    /// resolved). This is the only fact a provider "synthesis done" signal may
    /// become.
    pub fn prepared(&mut self, at_ms: u64, input: SpeechPrepared) -> Result<(), LifecycleError> {
        if self.live.contains_key(&input.event_id) {
            return self.refuse(LifecycleError::InvalidTransition {
                from: self.live[&input.event_id].stage,
                to: "prepared",
            });
        }
        if self.live.len() >= MAX_LIVE_SPEECH {
            return self.refuse(LifecycleError::LiveCapacity);
        }
        // The same id may start a new life once its previous one is terminal.
        if self.terminal_set.remove(&input.event_id) {
            self.terminal.retain(|id| id != &input.event_id);
        }
        let reference = SpeechRef {
            event_id: clip_lifecycle_label(&input.event_id),
            generation: None,
            phrase: input.phrase,
            final_phrase: input.final_phrase,
            asset_id: clip_lifecycle_label(&input.asset_id),
            speech_text_digest: input.speech_text_digest,
            projection: input.projection.as_deref().map(clip_lifecycle_label),
        };
        self.live.insert(
            input.event_id.clone(),
            LiveSpeech {
                reference,
                stage: SpeechStage::Prepared,
                last_at_ms: at_ms,
                started_at_ms: None,
                boundaries_emitted: 0,
                basis: SpeechTimeBasis::LogicalTimeline,
            },
        );
        self.metrics.prepared += 1;
        self.emit(
            at_ms,
            SpeechTimeBasis::LogicalTimeline,
            &input.event_id,
            SpeechPhase::Prepared,
        );
        Ok(())
    }

    pub fn committed(
        &mut self,
        at_ms: u64,
        event_id: &str,
        generation: u64,
    ) -> Result<(), LifecycleError> {
        let speech = self.step(event_id, at_ms, "committed", SpeechStage::Prepared)?;
        speech.stage = SpeechStage::Committed;
        speech.reference.generation = Some(generation);
        self.metrics.committed += 1;
        self.emit(
            at_ms,
            SpeechTimeBasis::LogicalTimeline,
            event_id,
            SpeechPhase::Committed,
        );
        Ok(())
    }

    pub fn started(&mut self, at_ms: u64, event_id: &str) -> Result<(), LifecycleError> {
        let speech = self.step(event_id, at_ms, "started", SpeechStage::Committed)?;
        speech.stage = SpeechStage::Started;
        speech.started_at_ms = Some(at_ms);
        self.metrics.started += 1;
        self.emit(
            at_ms,
            SpeechTimeBasis::LogicalTimeline,
            event_id,
            SpeechPhase::Started,
        );
        Ok(())
    }

    /// A declared safe interrupt point `offset_ms` into the unit was passed.
    pub fn boundary(
        &mut self,
        at_ms: u64,
        event_id: &str,
        offset_ms: u64,
    ) -> Result<(), LifecycleError> {
        let speech = self.step(event_id, at_ms, "boundary", SpeechStage::Started)?;
        speech.boundaries_emitted += 1;
        self.emit(
            at_ms,
            SpeechTimeBasis::LogicalTimeline,
            event_id,
            SpeechPhase::Boundary { offset_ms },
        );
        Ok(())
    }

    /// How many boundaries the unit has already emitted (the scheduler emits
    /// the next ones in order).
    pub fn boundaries_emitted(&self, event_id: &str) -> usize {
        self.live
            .get(event_id)
            .map_or(0, |speech| speech.boundaries_emitted)
    }

    pub fn finished(&mut self, at_ms: u64, event_id: &str) -> Result<(), LifecycleError> {
        let speech = self.step(event_id, at_ms, "finished", SpeechStage::Started)?;
        let basis = speech.basis;
        self.metrics.finished += 1;
        self.emit(at_ms, basis, event_id, SpeechPhase::Finished);
        self.retire(event_id);
        Ok(())
    }

    /// Cancel a unit from any live stage. `reached` and `spoken_ms` are
    /// derived from the unit's stage and times.
    pub fn cancelled(
        &mut self,
        at_ms: u64,
        event_id: &str,
        reason: SpeechCancelReason,
    ) -> Result<(), LifecycleError> {
        let (reached, spoken_ms, basis, at_ms) = {
            let speech = self.live.get_mut(event_id).ok_or_else(|| {
                if self.terminal_set.contains(event_id) {
                    LifecycleError::AfterTerminal
                } else {
                    LifecycleError::UnknownSpeech
                }
            });
            let speech = match speech {
                Ok(speech) => speech,
                Err(error) => return self.refuse(error),
            };
            // A cancellation never moves a unit's time backwards.
            let at_ms = at_ms.max(speech.last_at_ms);
            speech.last_at_ms = at_ms;
            match speech.stage {
                SpeechStage::Prepared => (SpeechReached::BeforeCommit, None, speech.basis, at_ms),
                SpeechStage::Committed => (SpeechReached::Committed, None, speech.basis, at_ms),
                SpeechStage::Started => (
                    SpeechReached::Started,
                    Some(at_ms.saturating_sub(speech.started_at_ms.unwrap_or(at_ms))),
                    speech.basis,
                    at_ms,
                ),
            }
        };
        match reached {
            SpeechReached::BeforeCommit => self.metrics.cancelled_before_commit += 1,
            SpeechReached::Committed => self.metrics.cancelled_committed += 1,
            SpeechReached::Started => self.metrics.cancelled_started += 1,
        }
        self.emit(
            at_ms,
            basis,
            event_id,
            SpeechPhase::Cancelled {
                reason,
                reached,
                spoken_ms,
            },
        );
        self.retire(event_id);
        Ok(())
    }

    /// An output adapter reported on a unit's command (see
    /// [`OutputObservation`]). `Accepted` and `Drained` only strengthen the
    /// basis of the unit's eventual `Finished`; a failure cancels the unit.
    pub fn observe_output(
        &mut self,
        at_ms: u64,
        event_id: &str,
        observation: OutputObservation,
    ) -> Result<(), LifecycleError> {
        match observation {
            OutputObservation::Accepted | OutputObservation::Drained => {
                let Some(speech) = self.live.get_mut(event_id) else {
                    return self.late_report(event_id);
                };
                let evidence = if observation == OutputObservation::Drained {
                    SpeechTimeBasis::OutputDrained
                } else {
                    SpeechTimeBasis::OutputAccepted
                };
                // Drained outranks accepted outranks the logical timeline.
                if rank(evidence) > rank(speech.basis) {
                    speech.basis = evidence;
                }
                Ok(())
            }
            OutputObservation::Failed(kind) => {
                if !self.live.contains_key(event_id) {
                    return self.late_report(event_id);
                }
                self.cancelled(at_ms, event_id, kind.reason())
            }
        }
    }

    fn late_report(&mut self, event_id: &str) -> Result<(), LifecycleError> {
        self.metrics.late_output_reports += 1;
        Err(if self.terminal_set.contains(event_id) {
            LifecycleError::AfterTerminal
        } else {
            LifecycleError::UnknownSpeech
        })
    }

    /// Count a refusal and return it.
    fn refuse(&mut self, error: LifecycleError) -> Result<(), LifecycleError> {
        match error {
            LifecycleError::AfterTerminal => self.metrics.suppressed_after_terminal += 1,
            _ => self.metrics.rejected += 1,
        }
        Err(error)
    }

    /// Validate a transition and advance the unit's time. The unit must be
    /// live and at exactly `required` (for `boundary`, `required` is
    /// `Started`).
    fn step(
        &mut self,
        event_id: &str,
        at_ms: u64,
        to: &'static str,
        required: SpeechStage,
    ) -> Result<&mut LiveSpeech, LifecycleError> {
        let error = match self.live.get(event_id) {
            None => Some(if self.terminal_set.contains(event_id) {
                LifecycleError::AfterTerminal
            } else {
                LifecycleError::UnknownSpeech
            }),
            Some(speech) if speech.stage != required => Some(LifecycleError::InvalidTransition {
                from: speech.stage,
                to,
            }),
            Some(speech) if at_ms < speech.last_at_ms => Some(LifecycleError::NonMonotonicTime),
            Some(_) => None,
        };
        if let Some(error) = error {
            let _ = self.refuse(error.clone());
            return Err(error);
        }
        let speech = self.live.get_mut(event_id).expect("checked live above");
        speech.last_at_ms = at_ms;
        Ok(speech)
    }

    /// Move a unit from live to the bounded terminal memory.
    fn retire(&mut self, event_id: &str) {
        self.live.remove(event_id);
        if self.terminal_set.insert(event_id.to_owned()) {
            self.terminal.push_back(event_id.to_owned());
        }
        while self.terminal.len() > TERMINAL_MEMORY {
            if let Some(old) = self.terminal.pop_front() {
                self.terminal_set.remove(&old);
            }
        }
    }

    fn emit(&mut self, at_ms: u64, basis: SpeechTimeBasis, event_id: &str, phase: SpeechPhase) {
        // The reference is read from the live unit when it is still live (the
        // terminal transitions emit before they retire).
        let Some(speech) = self.live.get(event_id) else {
            return;
        };
        let event = SpeechLifecycleEvent {
            schema_version: SPEECH_LIFECYCLE_SCHEMA_VERSION.to_owned(),
            seq: 0,
            at_ms,
            basis,
            speech: speech.reference.clone(),
            phase,
        };
        match self.batch.as_mut() {
            Some(held) => held.push(event),
            None => self.push_event(event),
        }
    }

    /// Assign `seq` and queue one event, dropping under pressure.
    fn push_event(&mut self, mut event: SpeechLifecycleEvent) {
        event.seq = self.next_seq;
        self.next_seq += 1;
        self.metrics.emitted += 1;
        if self.queue.len() >= self.capacity {
            // Prefer losing a boundary; otherwise the oldest event. Either way
            // the `seq` gap is visible to the consumer.
            let victim = self
                .queue
                .iter()
                .position(|queued| matches!(queued.phase, SpeechPhase::Boundary { .. }))
                .unwrap_or(0);
            self.queue.remove(victim);
            self.metrics.dropped += 1;
        }
        self.queue.push_back(event);
    }
}

/// Order within a batch: time, then terminal (0) before start (1) before
/// boundary (2), then generation, so a hand-off at one instant is read as the
/// first unit finishing before the next one starts.
fn batch_key(event: &SpeechLifecycleEvent) -> (u64, u8, u64) {
    let class = match event.phase {
        SpeechPhase::Finished | SpeechPhase::Cancelled { .. } => 0,
        SpeechPhase::Prepared | SpeechPhase::Committed | SpeechPhase::Started => 1,
        SpeechPhase::Boundary { .. } => 2,
    };
    (
        event.at_ms,
        class,
        event.speech.generation.unwrap_or(u64::MAX),
    )
}

fn rank(basis: SpeechTimeBasis) -> u8 {
    match basis {
        SpeechTimeBasis::LogicalTimeline => 0,
        SpeechTimeBasis::OutputAccepted => 1,
        SpeechTimeBasis::OutputDrained => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(id: &str) -> SpeechPrepared {
        SpeechPrepared {
            event_id: id.to_owned(),
            asset_id: "asset".to_owned(),
            phrase: 0,
            final_phrase: true,
            speech_text_digest: Some("a".repeat(64)),
            projection: Some("rules:1|lexicon:none".to_owned()),
        }
    }

    #[test]
    fn happy_path_in_order_and_every_event_validates() {
        let mut log = SpeechLifecycleLog::default();
        log.prepared(0, prepared("a")).unwrap();
        log.committed(0, "a", 1).unwrap();
        log.started(10, "a").unwrap();
        log.boundary(40, "a", 30).unwrap();
        log.finished(100, "a").unwrap();
        let events = log.drain(usize::MAX);
        let kinds: Vec<_> = events.iter().map(|e| e.phase).collect();
        assert_eq!(
            kinds,
            vec![
                SpeechPhase::Prepared,
                SpeechPhase::Committed,
                SpeechPhase::Started,
                SpeechPhase::Boundary { offset_ms: 30 },
                SpeechPhase::Finished,
            ]
        );
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        for event in &events {
            event.validate().unwrap();
        }
        assert_eq!(events[0].speech.generation, None);
        assert_eq!(events[1].speech.generation, Some(1));
        assert!(!log.is_live("a"));
        let metrics = log.metrics();
        assert_eq!(
            (metrics.prepared, metrics.finished, metrics.live),
            (1, 1, 0)
        );
    }

    #[test]
    fn invalid_transitions_are_typed_errors() {
        let mut log = SpeechLifecycleLog::default();
        assert_eq!(log.started(0, "x"), Err(LifecycleError::UnknownSpeech));
        log.prepared(0, prepared("a")).unwrap();
        // Started before Committed; Finished before Started; double Prepared.
        assert!(matches!(
            log.started(0, "a"),
            Err(LifecycleError::InvalidTransition {
                from: SpeechStage::Prepared,
                to: "started"
            })
        ));
        assert!(matches!(
            log.finished(0, "a"),
            Err(LifecycleError::InvalidTransition { .. })
        ));
        assert!(matches!(
            log.prepared(0, prepared("a")),
            Err(LifecycleError::InvalidTransition { .. })
        ));
        log.committed(5, "a", 1).unwrap();
        assert!(matches!(
            log.committed(5, "a", 2),
            Err(LifecycleError::InvalidTransition { .. })
        ));
        assert!(matches!(
            log.boundary(5, "a", 0),
            Err(LifecycleError::InvalidTransition { .. })
        ));
        // Time never goes backwards for a unit.
        assert_eq!(log.started(4, "a"), Err(LifecycleError::NonMonotonicTime));
        log.started(5, "a").unwrap();
        assert!(log.metrics().rejected >= 5);
    }

    #[test]
    fn nothing_follows_a_terminal_state() {
        let mut log = SpeechLifecycleLog::default();
        for (id, finish) in [("fin", true), ("can", false)] {
            log.prepared(0, prepared(id)).unwrap();
            log.committed(0, id, 1).unwrap();
            log.started(0, id).unwrap();
            if finish {
                log.finished(10, id).unwrap();
            } else {
                log.cancelled(10, id, SpeechCancelReason::OperatorStop)
                    .unwrap();
            }
            assert_eq!(log.started(11, id), Err(LifecycleError::AfterTerminal));
            assert_eq!(log.finished(11, id), Err(LifecycleError::AfterTerminal));
            assert_eq!(log.boundary(11, id, 1), Err(LifecycleError::AfterTerminal));
            assert_eq!(
                log.cancelled(11, id, SpeechCancelReason::Muted),
                Err(LifecycleError::AfterTerminal)
            );
            assert_eq!(
                log.observe_output(
                    11,
                    id,
                    OutputObservation::Failed(OutputFailureKind::AdapterError)
                ),
                Err(LifecycleError::AfterTerminal)
            );
        }
        // Not counted as rejected: they are suppressed consequences.
        assert_eq!(log.metrics().rejected, 0);
        assert!(log.metrics().suppressed_after_terminal >= 8);
        // The same id may be prepared again once terminal.
        log.prepared(20, prepared("fin")).unwrap();
        assert!(log.is_live("fin"));
    }

    #[test]
    fn cancelled_reached_and_spoken_are_computed_not_supplied() {
        let mut log = SpeechLifecycleLog::default();
        log.prepared(0, prepared("before")).unwrap();
        log.cancelled(3, "before", SpeechCancelReason::GateRejected)
            .unwrap();
        log.prepared(0, prepared("committed")).unwrap();
        log.committed(0, "committed", 1).unwrap();
        log.cancelled(3, "committed", SpeechCancelReason::Superseded)
            .unwrap();
        log.prepared(0, prepared("started")).unwrap();
        log.committed(0, "started", 2).unwrap();
        log.started(10, "started").unwrap();
        log.cancelled(75, "started", SpeechCancelReason::OperatorStop)
            .unwrap();
        let cancelled: Vec<_> = log
            .drain(usize::MAX)
            .into_iter()
            .filter(|e| matches!(e.phase, SpeechPhase::Cancelled { .. }))
            .collect();
        assert_eq!(cancelled.len(), 3);
        assert_eq!(
            cancelled[0].phase,
            SpeechPhase::Cancelled {
                reason: SpeechCancelReason::GateRejected,
                reached: SpeechReached::BeforeCommit,
                spoken_ms: None
            }
        );
        assert_eq!(
            cancelled[1].phase,
            SpeechPhase::Cancelled {
                reason: SpeechCancelReason::Superseded,
                reached: SpeechReached::Committed,
                spoken_ms: None
            }
        );
        assert_eq!(
            cancelled[2].phase,
            SpeechPhase::Cancelled {
                reason: SpeechCancelReason::OperatorStop,
                reached: SpeechReached::Started,
                spoken_ms: Some(65)
            }
        );
        for event in &cancelled {
            event.validate().unwrap();
        }
        let metrics = log.metrics();
        assert_eq!(
            (
                metrics.cancelled_before_commit,
                metrics.cancelled_committed,
                metrics.cancelled_started
            ),
            (1, 1, 1)
        );
    }

    #[test]
    fn output_evidence_sets_the_finished_basis_and_failures_cancel() {
        let mut log = SpeechLifecycleLog::default();
        for id in ["plain", "accepted", "drained"] {
            log.prepared(0, prepared(id)).unwrap();
            log.committed(0, id, 1).unwrap();
            log.started(0, id).unwrap();
        }
        log.observe_output(1, "accepted", OutputObservation::Accepted)
            .unwrap();
        log.observe_output(1, "drained", OutputObservation::Accepted)
            .unwrap();
        log.observe_output(2, "drained", OutputObservation::Drained)
            .unwrap();
        // A later, weaker report never lowers the basis.
        log.observe_output(3, "drained", OutputObservation::Accepted)
            .unwrap();
        for id in ["plain", "accepted", "drained"] {
            log.finished(10, id).unwrap();
        }
        let finished: Vec<_> = log
            .drain(usize::MAX)
            .into_iter()
            .filter(|e| e.phase == SpeechPhase::Finished)
            .map(|e| (e.speech.event_id, e.basis))
            .collect();
        assert_eq!(
            finished,
            vec![
                ("plain".to_owned(), SpeechTimeBasis::LogicalTimeline),
                ("accepted".to_owned(), SpeechTimeBasis::OutputAccepted),
                ("drained".to_owned(), SpeechTimeBasis::OutputDrained),
            ]
        );

        for (id, kind, reason) in [
            (
                "fail",
                OutputFailureKind::AdapterError,
                SpeechCancelReason::OutputFailure,
            ),
            ("mute", OutputFailureKind::Muted, SpeechCancelReason::Muted),
            (
                "late",
                OutputFailureKind::StaleOrLate,
                SpeechCancelReason::StaleOrLate,
            ),
        ] {
            log.prepared(0, prepared(id)).unwrap();
            log.committed(0, id, 1).unwrap();
            log.started(0, id).unwrap();
            log.observe_output(5, id, OutputObservation::Failed(kind))
                .unwrap();
            // The scheduler's own Finished afterwards is suppressed, never
            // reported: a failed command is not a finished one.
            assert_eq!(log.finished(9, id), Err(LifecycleError::AfterTerminal));
            let events = log.drain(usize::MAX);
            assert!(!events.iter().any(|e| e.phase == SpeechPhase::Finished));
            assert!(matches!(
                events.last().unwrap().phase,
                SpeechPhase::Cancelled { reason: r, .. } if r == reason
            ));
        }
        // Reports for an unknown unit are counted as late, not panics.
        assert_eq!(
            log.observe_output(9, "nope", OutputObservation::Accepted),
            Err(LifecycleError::UnknownSpeech)
        );
        assert_eq!(log.metrics().late_output_reports, 1);
    }

    #[test]
    fn queue_is_bounded_prefers_dropping_boundaries_and_counts_every_drop() {
        let mut log = SpeechLifecycleLog::with_capacity(4);
        log.prepared(0, prepared("a")).unwrap();
        log.committed(0, "a", 1).unwrap();
        log.started(0, "a").unwrap();
        log.boundary(1, "a", 1).unwrap();
        // Queue is full (4). The next event drops the boundary, not the oldest.
        log.boundary(2, "a", 2).unwrap();
        let events = log.drain(usize::MAX);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].phase, SpeechPhase::Prepared);
        assert_eq!(events[3].phase, SpeechPhase::Boundary { offset_ms: 2 });
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
        );
        assert_eq!(log.metrics().dropped, 1);

        // With no boundary to lose, the oldest goes; seq gaps stay visible.
        let mut log = SpeechLifecycleLog::with_capacity(2);
        for id in ["a", "b", "c"] {
            log.prepared(0, prepared(id)).unwrap();
        }
        let events = log.drain(usize::MAX);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 2);
        assert_eq!(log.metrics().dropped, 1);
        assert_eq!(log.snapshot().dropped, 1);
    }

    #[test]
    fn live_set_and_terminal_memory_are_bounded() {
        let mut log = SpeechLifecycleLog::default();
        for index in 0..MAX_LIVE_SPEECH {
            log.prepared(0, prepared(&format!("u{index}"))).unwrap();
        }
        assert_eq!(
            log.prepared(0, prepared("overflow")),
            Err(LifecycleError::LiveCapacity)
        );
        assert_eq!(log.metrics().live, MAX_LIVE_SPEECH);
        assert!(log.snapshot().live.len() <= MAX_LIVE_SPEECH);
        for index in 0..MAX_LIVE_SPEECH {
            log.cancelled(0, &format!("u{index}"), SpeechCancelReason::Shutdown)
                .unwrap();
        }
        // More terminals than the memory holds: the oldest ids are forgotten.
        for index in 0..(TERMINAL_MEMORY + 10) {
            let id = format!("t{index}");
            log.prepared(0, prepared(&id)).unwrap();
            log.cancelled(0, &id, SpeechCancelReason::Shutdown).unwrap();
        }
        assert!(log.terminal.len() <= TERMINAL_MEMORY);
        assert_eq!(log.terminal.len(), log.terminal_set.len());
        assert_eq!(log.finished(1, "u0"), Err(LifecycleError::UnknownSpeech));
    }

    #[test]
    fn over_long_or_control_event_ids_still_produce_valid_events() {
        let mut log = SpeechLifecycleLog::default();
        let id = format!("{}\nforged", "x".repeat(400));
        log.prepared(0, prepared(&id)).unwrap();
        log.committed(0, &id, 1).unwrap();
        for event in log.drain(usize::MAX) {
            event.validate().unwrap();
        }
    }

    #[test]
    fn snapshot_lets_a_consumer_resynchronize_after_a_gap() {
        let mut log = SpeechLifecycleLog::with_capacity(2);
        log.prepared(0, prepared("a")).unwrap();
        log.committed(0, "a", 1).unwrap();
        log.started(5, "a").unwrap();
        let snapshot = log.snapshot();
        assert_eq!(snapshot.live.len(), 1);
        assert_eq!(snapshot.live[0].stage, SpeechStage::Started);
        assert_eq!(snapshot.live[0].started_at_ms, Some(5));
        assert_eq!(snapshot.live[0].speech.generation, Some(1));
        // seq 1 was dropped: the first queued event is seq 2.
        assert_eq!(snapshot.queued[0].seq, 2);
        assert_eq!(snapshot.next_seq, 4);
    }
}
