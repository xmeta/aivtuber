//! Streamed phrased replies (issue #83 S2).
//!
//! The Thinking engine pushes plain text deltas; this module cuts them into
//! phrases with the same `phrase-seg-v1` rules as a buffered reply and runs
//! each phrase through the unchanged per-phrase sequence
//!
//! ```text
//! delta -> segmenter -> [gate -> projection (+ gate) -> TTS -> compile] -> deliver
//! ```
//!
//! A phrase is cut only when it is *complete*: the segmenter needs at least one
//! character after the terminator run (a closing mark, emoji or the second dot
//! of a `..` can still follow) and at least one more phrase after it, so text
//! that is still being written is never spoken and a cut never depends on text
//! that has not arrived. The remainder is flushed when the stream completes.
//!
//! Differences from the buffered path, all deliberate:
//!
//! * There is no whole-reply gate before phrase 0 (the reply does not exist
//!   yet). Every phrase is gated on its own text immediately before its own
//!   commit, exactly as in the buffered path, and a refusal ends the reply
//!   after the last delivered phrase.
//! * The plan is not known up front. A phrase is delivered `final` only when it
//!   is cut at the end of the stream; a phrase cut earlier is delivered
//!   non-final even if the remainder later turns out to project to nothing.
//!   [`PhraseEnd::Completed`] in the summary is the authoritative end.
//! * A provider failure mid-stream ends the reply after the last delivered
//!   phrase (`PhraseFailure::Stream`); phrases already delivered stand and the
//!   incomplete remainder is dropped, never spoken.

use crate::phrase::{
    Delivery, PhraseDelivery, PhraseEnd, PhraseFailure, PhraseOptions, PhraseSummary, SlotWait,
    StreamPolicy, has_content, segment_ranges,
};
use crate::phrase_pipeline::PhraseStop;
use crate::{
    CancellationStage, FallbackDirective, GeneratedTextGateDecision, GenerationCancellationToken,
    GenerationDisposition, GenerationRequest, GenerationResult, GenerationTrace,
    GenerativePipeline, PHRASE_SEGMENTATION_VERSION, SegmentationConfig, cancelled_result,
    duration_ms_ceil, fallback_reason_from_error, fallback_result, project_speech_text,
    remaining_provider_timeout,
};
use aivtuber_domain::{BackendIdentity, DeltaFlow, FallbackReason, ReplyDeltaSink};
use std::cell::Cell;
use std::time::Instant;

/// Cuts a growing reply into phrases without ever cutting on incomplete text.
pub(crate) struct IncrementalSegmenter {
    config: SegmentationConfig,
    pending: Vec<char>,
    emitted: usize,
}

impl IncrementalSegmenter {
    pub(crate) fn new(config: SegmentationConfig) -> Self {
        Self {
            config,
            pending: Vec::new(),
            emitted: 0,
        }
    }

    /// Append a delta; returns the phrases that are now complete.
    pub(crate) fn push(&mut self, delta: &str) -> Vec<String> {
        self.pending.extend(delta.chars());
        let mut out = Vec::new();
        // The last allowed phrase takes the whole tail (the same cap
        // `segment_reply` applies), so stop cutting one short of it.
        while self.emitted + 1 < self.config.max_phrases {
            let ranges = segment_ranges(&self.pending, &self.config);
            if ranges.len() < 2 {
                break;
            }
            let (from, to) = ranges[0];
            let text: String = self.pending[from..to].iter().collect();
            self.pending.drain(..to);
            self.emitted += 1;
            let text = text.trim().to_owned();
            if !text.is_empty() {
                out.push(text);
            }
        }
        out
    }

    /// The stream ended: everything left is cut as in a buffered reply.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        let mut config = self.config;
        config.max_phrases = self.config.max_phrases.saturating_sub(self.emitted).max(1);
        let ranges = segment_ranges(&self.pending, &config);
        let out = ranges
            .into_iter()
            .map(|(from, to)| self.pending[from..to].iter().collect::<String>())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .collect();
        self.pending.clear();
        out
    }
}

/// Why the stream was stopped from our side.
enum Stop {
    Cancelled {
        seq: u32,
        at: CancellationStage,
    },
    Failed {
        seq: u32,
        failure: PhraseFailure,
        reason: FallbackReason,
    },
}

struct StreamSink<'a, F, D> {
    pipeline: &'a GenerativePipeline,
    request: &'a GenerationRequest,
    options: &'a PhraseOptions,
    cancellation: &'a GenerationCancellationToken,
    provider_deadline: Option<Instant>,
    gate: &'a mut F,
    delivery: &'a mut D,
    trace: &'a mut GenerationTrace,
    identities: (&'a BackendIdentity, &'a aivtuber_domain::TtsBackendIdentity),
    segmenter: IncrementalSegmenter,
    carry: String,
    next_seq: u32,
    tts_chars: u64,
    stop: Option<Stop>,
    deadline_hit: Cell<bool>,
}

impl<F, D> StreamSink<'_, F, D>
where
    F: FnMut(&str) -> GeneratedTextGateDecision,
    D: PhraseDelivery,
{
    fn phase0_deadline_passed(&self) -> bool {
        self.next_seq == 0
            && self
                .provider_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Run one complete phrase through the stages. Returns false when the
    /// reply must stop (the reason is recorded in `self.stop`).
    fn process(&mut self, text: String, final_phrase: bool) -> bool {
        let text = if self.carry.is_empty() {
            text
        } else {
            format!("{} {text}", std::mem::take(&mut self.carry))
        };
        // A phrase that would project to nothing (only a URL or emoji) joins
        // the next one, as in the buffered plan.
        if !has_content(&project_speech_text(&text).text) {
            self.carry = text;
            return true;
        }
        let seq = self.next_seq;
        if seq > 0 {
            match self.delivery.wait_for_slot(seq, self.cancellation) {
                SlotWait::Ready => {}
                SlotWait::Cancelled => {
                    self.stop = Some(Stop::Cancelled {
                        seq,
                        at: CancellationStage::AfterThinking,
                    });
                    return false;
                }
                SlotWait::Stalled => {
                    self.stop = Some(Stop::Failed {
                        seq,
                        failure: PhraseFailure::LookaheadStalled,
                        reason: FallbackReason::Timeout,
                    });
                    return false;
                }
            }
        }
        if self.cancellation.is_cancelled() {
            self.stop = Some(Stop::Cancelled {
                seq,
                at: CancellationStage::AfterThinking,
            });
            return false;
        }
        if !self.trace.tts_attempted {
            self.trace.tts_attempted = true;
            self.trace.tts_backend = Some(self.identities.1.clone());
        }
        let prepared = self.pipeline.prepare_phrase(
            self.request,
            self.options,
            seq,
            final_phrase,
            &text,
            self.gate,
            self.provider_deadline,
            self.identities,
            self.cancellation,
            self.trace,
        );
        let (unit, chars) = match prepared {
            Ok(prepared) => prepared,
            Err(PhraseStop::Cancelled(at)) => {
                self.stop = Some(Stop::Cancelled { seq, at });
                return false;
            }
            Err(PhraseStop::Failed(failure, reason)) => {
                self.stop = Some(Stop::Failed {
                    seq,
                    failure,
                    reason,
                });
                return false;
            }
        };
        self.tts_chars = self.tts_chars.saturating_add(chars as u64);
        match self.delivery.deliver(unit, self.trace) {
            Delivery::Accepted => {
                self.next_seq += 1;
                true
            }
            Delivery::Closed => {
                self.stop = Some(Stop::Cancelled {
                    seq,
                    at: CancellationStage::BeforePublish,
                });
                false
            }
        }
    }
}

impl<F, D> ReplyDeltaSink for StreamSink<'_, F, D>
where
    F: FnMut(&str) -> GeneratedTextGateDecision,
    D: PhraseDelivery,
{
    fn push(&mut self, delta: &str) -> DeltaFlow {
        if self.stop.is_some() || self.is_cancelled() {
            return DeltaFlow::Stop;
        }
        for phrase in self.segmenter.push(delta) {
            if !self.process(phrase, false) {
                return DeltaFlow::Stop;
            }
        }
        DeltaFlow::Continue
    }

    fn is_cancelled(&self) -> bool {
        if self.cancellation.is_cancelled() {
            return true;
        }
        if self.phase0_deadline_passed() {
            self.deadline_hit.set(true);
            return true;
        }
        false
    }
}

impl GenerativePipeline {
    /// Streamed variant of [`GenerativePipeline::run_phrased`]; called from it
    /// once the engine is known to stream. Never returns an error: every
    /// failure is a typed fallback, cancellation or partial summary.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_streamed<F, D>(
        &self,
        request: &GenerationRequest,
        cancellation: &GenerationCancellationToken,
        fallback: FallbackDirective,
        provider_deadline: Option<Instant>,
        options: &PhraseOptions,
        stream: StreamPolicy,
        output_gate: &mut F,
        delivery: &mut D,
        mut trace: GenerationTrace,
        thinking_identity: &BackendIdentity,
    ) -> GenerationResult
    where
        F: FnMut(&str) -> GeneratedTextGateDecision,
        D: PhraseDelivery,
    {
        let tts_identity = self.tts.identity();
        if let Ok(Some(remaining)) = remaining_provider_timeout(provider_deadline) {
            trace.thinking_timeout_ms = Some(duration_ms_ceil(remaining));
        }

        let (outcome, stop, delivered, tts_chars, deadline_hit) = {
            let mut sink = StreamSink {
                pipeline: self,
                request,
                options,
                cancellation,
                provider_deadline,
                gate: output_gate,
                delivery,
                trace: &mut trace,
                identities: (thinking_identity, &tts_identity),
                segmenter: IncrementalSegmenter::new(options.config.segmentation),
                carry: String::new(),
                next_seq: 0,
                tts_chars: 0,
                stop: None,
                deadline_hit: Cell::new(false),
            };
            let outcome =
                self.thinking
                    .generate_streaming(&request.thinking, stream.max_duration, &mut sink);
            let mut outcome = outcome.map(|result| result.map(|_| ()));
            if matches!(outcome, Some(Ok(()))) && sink.stop.is_none() {
                let tail = sink.segmenter.finish();
                let last = tail.len().saturating_sub(1);
                for (index, phrase) in tail.into_iter().enumerate() {
                    if !sink.process(phrase, index == last) {
                        break;
                    }
                }
                outcome = Some(Ok(()));
            }
            (
                outcome,
                sink.stop.take(),
                sink.next_seq,
                sink.tts_chars,
                sink.deadline_hit.get(),
            )
        };

        let summary = |planned: u32, end: PhraseEnd| PhraseSummary {
            response_id: options.response_id,
            segmentation_version: PHRASE_SEGMENTATION_VERSION.to_owned(),
            planned_phrases: planned,
            delivered_phrases: delivered,
            end,
            tts_chars,
        };
        let phrased = |trace: GenerationTrace, summary: PhraseSummary| GenerationResult {
            trace,
            disposition: GenerationDisposition::Phrased { summary },
        };

        // Our own stop reason is authoritative over the engine's error.
        if let Some(stop) = stop {
            return match stop {
                Stop::Cancelled { at, .. } if delivered == 0 => cancelled_result(trace, at),
                Stop::Cancelled { seq, at } => phrased(
                    trace,
                    summary(delivered + 1, PhraseEnd::Cancelled { seq, at }),
                ),
                Stop::Failed { reason, .. } if delivered == 0 => {
                    trace.fallback_reason = reason;
                    fallback_result(trace, fallback)
                }
                Stop::Failed { seq, failure, .. } => phrased(
                    trace,
                    summary(delivered + 1, PhraseEnd::Failed { seq, failure }),
                ),
            };
        }
        match outcome {
            Some(Ok(())) => {
                if delivered == 0 {
                    // Nothing speakable (an all-emoji or URL-only reply).
                    trace.fallback_reason = FallbackReason::PolicyOverride;
                    return fallback_result(trace, fallback);
                }
                phrased(trace, summary(delivered, PhraseEnd::Completed))
            }
            Some(Err(error)) => {
                if cancellation.is_cancelled() {
                    return if delivered == 0 {
                        cancelled_result(trace, CancellationStage::AfterThinking)
                    } else {
                        phrased(
                            trace,
                            summary(
                                delivered + 1,
                                PhraseEnd::Cancelled {
                                    seq: delivered,
                                    at: CancellationStage::AfterThinking,
                                },
                            ),
                        )
                    };
                }
                let reason = if deadline_hit {
                    FallbackReason::Timeout
                } else {
                    fallback_reason_from_error(&error)
                };
                if delivered == 0 {
                    trace.fallback_reason = reason;
                    return fallback_result(trace, fallback);
                }
                phrased(
                    trace,
                    summary(
                        delivered + 1,
                        PhraseEnd::Failed {
                            seq: delivered,
                            failure: PhraseFailure::Stream(reason),
                        },
                    ),
                )
            }
            None => {
                trace.fallback_reason = FallbackReason::Unavailable;
                fallback_result(trace, fallback)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment_reply;

    fn config() -> SegmentationConfig {
        SegmentationConfig::default()
    }

    fn stream_all(text: &str, chunk: usize) -> Vec<String> {
        let mut segmenter = IncrementalSegmenter::new(config());
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        for part in chars.chunks(chunk) {
            out.extend(segmenter.push(&part.iter().collect::<String>()));
        }
        out.extend(segmenter.finish());
        out
    }

    const REPLY: &str = "こんにちは、今日は配信に来てくれてありがとうございます。\
        今日はゆっくりおしゃべりしながら、みんなの質問にも答えていくよ。\
        Version 3.14 is out! Mr. Smith said so... 本当に楽しみだね😊 \
        最後まで見ていってね。";

    #[test]
    fn streamed_cuts_equal_buffered_cuts_for_every_chunk_size() {
        let expected = segment_reply(REPLY, &config());
        assert!(expected.len() >= 3, "fixture must span several phrases");
        for chunk in [1, 2, 3, 5, 7, 16, 64, 1000] {
            assert_eq!(stream_all(REPLY, chunk), expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn nothing_is_cut_before_the_terminator_run_is_closed() {
        let mut segmenter = IncrementalSegmenter::new(config());
        // A complete-looking sentence with nothing after it: it could still be
        // followed by a closing mark or emoji, so it must wait.
        assert!(
            segmenter
                .push("今日は配信に来てくれてありがとうございます。")
                .is_empty()
        );
        assert!(segmenter.push("次の").is_empty());
        let tail = segmenter.finish();
        assert_eq!(
            tail.len(),
            1,
            "short tail merges back like a buffered reply"
        );
    }

    #[test]
    fn phrase_count_is_capped_like_a_buffered_reply() {
        let mut config = config();
        config.max_phrases = 2;
        let text = "これは長い一つ目の文章であるよ、確かにね。".repeat(8);
        let mut segmenter = IncrementalSegmenter::new(config);
        let mut out = segmenter.push(&text);
        out.extend(segmenter.finish());
        assert_eq!(out, segment_reply(&text, &config));
        assert_eq!(out.len(), 2);
    }

    // -- pipeline-level tests ------------------------------------------------

    use crate::{
        GenerationRoutingReason, PerformanceCompiler, PerformanceCompilerConfig, PhraseConfig,
    };
    use aivtuber_domain::{
        EngineError, EngineErrorKind, EngineFuture, EventEnvelope, GeneratedReply, PrivacyClass,
        ReflexContext, RetrievalSnapshot, SpeechArtifact, SpeechRequest, ThinkingEngine,
        ThinkingRequest, TtsBackendIdentity, TtsEngine,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const A: &str = "今日は配信に来てくれてありがとうございます。";
    const B: &str = "このあとはゲームを遊びながらお話しします。";
    const C: &str = "最後まで楽しんでいってくださいね。";

    enum Step {
        Delta(String),
        /// Wait this long while polling cancellation (a silent provider).
        Silence(Duration),
        Fail(EngineErrorKind),
        /// Cancel the generation token from "outside", as a barge-in would.
        CancelToken,
        Done,
    }

    struct ScriptedStream {
        steps: Mutex<Vec<Step>>,
        streams: bool,
        full_text: String,
        token: GenerationCancellationToken,
        ended_at: Arc<Mutex<Option<Instant>>>,
        generate_calls: Arc<AtomicUsize>,
    }

    impl ThinkingEngine for ScriptedStream {
        fn generate<'a>(
            &'a self,
            _request: &'a ThinkingRequest,
        ) -> EngineFuture<'a, GeneratedReply> {
            self.generate_calls.fetch_add(1, Ordering::SeqCst);
            let text = self.full_text.clone();
            Box::pin(async move { Ok(GeneratedReply { text }) })
        }

        fn streams(&self) -> bool {
            self.streams
        }

        fn generate_streaming(
            &self,
            _request: &ThinkingRequest,
            _timeout: Duration,
            sink: &mut dyn ReplyDeltaSink,
        ) -> Option<Result<GeneratedReply, EngineError>> {
            let steps = std::mem::take(&mut *self.steps.lock().unwrap());
            let mut text = String::new();
            let stopped = || Some(Err(EngineError::new(EngineErrorKind::Backend, "stopped")));
            for step in steps {
                match step {
                    Step::Delta(delta) => {
                        text.push_str(&delta);
                        if sink.push(&delta) == DeltaFlow::Stop {
                            return stopped();
                        }
                    }
                    Step::Silence(wait) => {
                        let until = Instant::now() + wait;
                        while Instant::now() < until {
                            if sink.is_cancelled() {
                                return stopped();
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Step::Fail(kind) => return Some(Err(EngineError::new(kind, "scripted"))),
                    Step::CancelToken => self.token.cancel(),
                    Step::Done => {
                        *self.ended_at.lock().unwrap() = Some(Instant::now());
                        return Some(Ok(GeneratedReply { text }));
                    }
                }
            }
            Some(Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "no end",
            )))
        }

        fn identity(&self) -> aivtuber_domain::BackendIdentity {
            aivtuber_domain::BackendIdentity {
                name: "scripted-stream".to_owned(),
                model_alias: None,
                model_version: None,
            }
        }
    }

    struct Tts {
        spoken: Arc<Mutex<Vec<String>>>,
    }

    impl TtsEngine for Tts {
        fn synthesize<'a>(
            &'a self,
            request: &'a SpeechRequest,
        ) -> EngineFuture<'a, SpeechArtifact> {
            let call = {
                let mut spoken = self.spoken.lock().unwrap();
                spoken.push(request.text.clone());
                spoken.len()
            };
            let chars = request.text.chars().count() as u64;
            Box::pin(async move {
                Ok(SpeechArtifact {
                    audio_ref: format!("audio://generated/p{call}.wav"),
                    duration_ms: 300 + 60 * chars,
                    viseme_ref: None,
                })
            })
        }

        fn identity(&self) -> TtsBackendIdentity {
            TtsBackendIdentity {
                backend: aivtuber_domain::BackendIdentity {
                    name: "stub-tts".to_owned(),
                    model_alias: None,
                    model_version: None,
                },
                voice_model: Some("v".to_owned()),
                viseme_mapping: Some("m".to_owned()),
            }
        }
    }

    #[derive(Default)]
    struct Recorder {
        delivered: Vec<(u32, bool, Instant)>,
    }

    impl PhraseDelivery for Recorder {
        fn wait_for_slot(&mut self, _: u32, _: &GenerationCancellationToken) -> SlotWait {
            SlotWait::Ready
        }
        fn deliver(&mut self, unit: crate::PhraseUnit, _: &GenerationTrace) -> Delivery {
            self.delivered
                .push((unit.seq(), unit.is_final(), Instant::now()));
            Delivery::Accepted
        }
    }

    struct Rig {
        pipeline: GenerativePipeline,
        spoken: Arc<Mutex<Vec<String>>>,
        ended_at: Arc<Mutex<Option<Instant>>>,
        generate_calls: Arc<AtomicUsize>,
        token: GenerationCancellationToken,
    }

    fn rig(steps: Vec<Step>, streams: bool, full_text: &str) -> Rig {
        let token = GenerationCancellationToken::default();
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let ended_at = Arc::new(Mutex::new(None));
        let generate_calls = Arc::new(AtomicUsize::new(0));
        let thinking = ScriptedStream {
            steps: Mutex::new(steps),
            streams,
            full_text: full_text.to_owned(),
            token: token.clone(),
            ended_at: Arc::clone(&ended_at),
            generate_calls: Arc::clone(&generate_calls),
        };
        let pipeline = GenerativePipeline::new(
            Arc::new(thinking),
            Arc::new(Tts {
                spoken: Arc::clone(&spoken),
            }),
            PerformanceCompiler::new(PerformanceCompilerConfig {
                compiler_version: "0.1.0".to_owned(),
                avatar_profile: Some("example-live2d-v1".to_owned()),
                motion_library: Some("starter-v1".to_owned()),
                expression_preset: Some("speaking.neutral".to_owned()),
                expression_intensity: 0.4,
            })
            .expect("compiler"),
        );
        Rig {
            pipeline,
            spoken,
            ended_at,
            generate_calls,
            token,
        }
    }

    fn request() -> GenerationRequest {
        let mut event: EventEnvelope =
            serde_json::from_str(include_str!("../../../examples/events/chat-message.json"))
                .expect("chat fixture");
        event.event_id = "evt-stream-1".to_owned();
        let thinking = ThinkingRequest::from_event(
            &event,
            "hello",
            PrivacyClass::Pseudonymous,
            ReflexContext::default(),
            RetrievalSnapshot::default(),
            Vec::new(),
        );
        GenerationRequest {
            source_event: event,
            thinking,
            routing_reason: GenerationRoutingReason::NoCachedMatch,
            intent: "dynamic.reply".to_owned(),
            style: None,
            fallback_variant_group: None,
            seed: 7,
        }
    }

    fn run_stream(
        rig: &Rig,
        gate: impl FnMut(&str) -> GeneratedTextGateDecision,
        recorder: &mut Recorder,
    ) -> GenerationResult {
        let options = PhraseOptions {
            response_id: 9,
            config: PhraseConfig {
                stream: Some(StreamPolicy::default()),
                ..PhraseConfig::default()
            },
        };
        rig.pipeline
            .run_phrased(
                &request(),
                &rig.token,
                FallbackDirective::NonVerbalReaction {
                    reaction_family: "reaction.surprise".to_owned(),
                },
                None,
                &options,
                gate,
                recorder,
            )
            .expect("pipeline")
    }

    fn publish(text: &str) -> GeneratedTextGateDecision {
        GeneratedTextGateDecision::Publish(text.to_owned())
    }

    fn delta(text: &str) -> Step {
        Step::Delta(text.to_owned())
    }

    /// Deltas of a few characters each, like tokens.
    fn tokens(text: &str) -> Vec<Step> {
        let chars: Vec<char> = text.chars().collect();
        chars
            .chunks(3)
            .map(|part| Step::Delta(part.iter().collect()))
            .collect()
    }

    fn summary_of(result: &GenerationResult) -> &PhraseSummary {
        match &result.disposition {
            GenerationDisposition::Phrased { summary } => summary,
            other => panic!("expected phrased, got {other:?}"),
        }
    }

    #[test]
    fn a_phrase_is_committed_while_the_provider_is_still_streaming() {
        let mut steps = tokens(&format!("{A}{B}"));
        steps.push(Step::Silence(Duration::from_millis(150)));
        steps.extend(tokens(C));
        steps.push(Step::Done);
        let rig = rig(steps, true, "");
        let mut recorder = Recorder::default();
        let result = run_stream(&rig, publish, &mut recorder);

        let summary = summary_of(&result);
        assert!(summary.completed());
        assert_eq!((summary.planned_phrases, summary.delivered_phrases), (3, 3));
        assert_eq!(
            rig.spoken.lock().unwrap().as_slice(),
            [A, B, C],
            "each complete phrase, in order, never a fragment"
        );
        let ended = rig.ended_at.lock().unwrap().expect("stream ended");
        assert!(
            recorder.delivered[0].2 < ended,
            "phrase 0 was delivered before the stream finished"
        );
        let finals: Vec<bool> = recorder.delivered.iter().map(|d| d.1).collect();
        assert_eq!(finals, [false, false, true]);
        assert_eq!(rig.generate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_stream_failure_mid_phrase_keeps_delivered_phrases_and_drops_the_remainder() {
        let mut steps = tokens(A);
        steps.extend(tokens(B));
        steps.extend(tokens("このあとは"));
        steps.push(Step::Fail(EngineErrorKind::Unavailable));
        let rig = rig(steps, true, "");
        let mut recorder = Recorder::default();
        let result = run_stream(&rig, publish, &mut recorder);

        let summary = summary_of(&result);
        assert_eq!(summary.delivered_phrases as usize, recorder.delivered.len());
        assert!(!summary.completed());
        assert!(matches!(
            summary.end,
            PhraseEnd::Failed {
                failure: PhraseFailure::Stream(FallbackReason::Unavailable),
                ..
            }
        ));
        // Whatever was cut was complete; the partial tail was never spoken.
        let spoken = rig.spoken.lock().unwrap();
        assert!(!spoken.is_empty() && spoken[0] == A);
        assert!(spoken.iter().all(|text| text == A || text == B));
    }

    #[test]
    fn a_failure_before_the_first_phrase_is_the_typed_fallback() {
        let rig = rig(
            vec![delta("今日は"), Step::Fail(EngineErrorKind::Timeout)],
            true,
            "",
        );
        let mut recorder = Recorder::default();
        let result = run_stream(&rig, publish, &mut recorder);
        assert!(recorder.delivered.is_empty());
        assert!(rig.spoken.lock().unwrap().is_empty());
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Fallback { .. }
        ));
        assert_eq!(result.trace.fallback_reason, FallbackReason::Timeout);
    }

    #[test]
    fn cancellation_during_a_silent_stream_stops_it_promptly() {
        let mut steps = tokens(&format!("{A}{B}"));
        steps.push(Step::CancelToken);
        steps.push(Step::Silence(Duration::from_secs(10)));
        steps.push(Step::Done);
        let rig = rig(steps, true, "");
        let mut recorder = Recorder::default();
        let started = Instant::now();
        let result = run_stream(&rig, publish, &mut recorder);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the silent provider was not waited for"
        );
        assert!(rig.ended_at.lock().unwrap().is_none());
        assert!(matches!(
            summary_of(&result).end,
            PhraseEnd::Cancelled { .. }
        ));
        assert_eq!(
            recorder.delivered.len(),
            1,
            "phrase 0 was already committed and stands; nothing after the cancel"
        );
    }

    #[test]
    fn cancellation_before_any_phrase_is_a_cancelled_result() {
        let rig = rig(
            vec![Step::CancelToken, delta("今日は配信に"), Step::Done],
            true,
            "",
        );
        let mut recorder = Recorder::default();
        let result = run_stream(&rig, publish, &mut recorder);
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Cancelled { .. }
        ));
        assert!(rig.spoken.lock().unwrap().is_empty());
    }

    #[test]
    fn each_streamed_phrase_is_gated_and_a_refusal_ends_the_reply_after_the_last_good_phrase() {
        let mut steps = tokens(&format!("{A}{B}{C}"));
        steps.push(Step::Done);
        let rig = rig(steps, true, "");
        let mut recorder = Recorder::default();
        let result = run_stream(
            &rig,
            |text| {
                if text.contains("ゲーム") {
                    GeneratedTextGateDecision::Reject
                } else {
                    publish(text)
                }
            },
            &mut recorder,
        );
        assert_eq!(recorder.delivered.len(), 1);
        assert!(matches!(
            summary_of(&result).end,
            PhraseEnd::Failed {
                seq: 1,
                failure: PhraseFailure::GateRejected
            }
        ));
        assert_eq!(rig.spoken.lock().unwrap().as_slice(), [A]);
    }

    #[test]
    fn an_engine_that_does_not_stream_keeps_the_buffered_call() {
        let rig = rig(Vec::new(), false, &format!("{A}{B}"));
        let mut recorder = Recorder::default();
        let result = run_stream(&rig, publish, &mut recorder);
        assert_eq!(rig.generate_calls.load(Ordering::SeqCst), 1);
        assert!(summary_of(&result).completed());
        assert_eq!(recorder.delivered.len(), 2);
    }

    #[test]
    fn a_stream_with_nothing_speakable_falls_back() {
        let rig = rig(vec![delta("😊😊"), Step::Done], true, "");
        let mut recorder = Recorder::default();
        let result = run_stream(&rig, publish, &mut recorder);
        assert!(recorder.delivered.is_empty());
        assert_eq!(result.trace.fallback_reason, FallbackReason::PolicyOverride);
    }
}
