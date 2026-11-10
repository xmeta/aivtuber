//! The phrased generative pipeline (issue #83 S1).
//!
//! Same stages and same safety rules as the buffered pipeline, applied to each
//! phrase of a buffered Thinking reply instead of the reply as a whole:
//!
//! ```text
//! Thinking (buffered) -> whole-reply gate -> segment -> plan
//!   per phrase:  gate -> projection (+ gate) -> TTS -> compile -> deliver
//! ```
//!
//! The worker never touches the scheduler. [`PhraseDelivery::deliver`] is the
//! only way out, and it accepts only a [`PhraseUnit`], which exists only for a
//! phrase that is gated, completely synthesized and compiled.

use crate::PHRASE_SEGMENTATION_VERSION;
use crate::phrase::{
    Delivery, PhraseDelivery, PhraseEnd, PhraseFailure, PhraseOptions, PhraseState, PhraseStep,
    PhraseSummary, PhraseUnit, SlotWait, has_content, segment_reply,
};
use crate::{
    CancellationStage, FallbackDirective, GeneratedTextGateDecision, GenerationCancellationToken,
    GenerationDisposition, GenerationError, GenerationRequest, GenerationResult, GenerationTrace,
    GenerativePipeline, LlmCallRecord, cancelled_result, duration_ms_ceil,
    fallback_reason_from_error, fallback_result, project_speech_text, remaining_provider_timeout,
    validate_generation_request,
};
use aivtuber_domain::{FallbackReason, SpeechRequest};
use futures::executor::block_on;
use std::time::{Duration, Instant};

/// Why one phrase stopped short of delivery.
enum PhraseStop {
    Cancelled(CancellationStage),
    Failed(PhraseFailure, FallbackReason),
}

/// Segment the gated reply and make every phrase speakable: a phrase that
/// would project to nothing (only a URL or emoji) joins its neighbour so the
/// plan, and with it the final flag, is fixed before the first phrase is
/// delivered.
pub(crate) fn plan_phrases(
    gated_reply: &str,
    config: &crate::phrase::SegmentationConfig,
) -> Vec<String> {
    let mut plan: Vec<String> = Vec::new();
    let mut carry = String::new();
    for phrase in segment_reply(gated_reply, config) {
        let text = if carry.is_empty() {
            phrase
        } else {
            format!("{carry} {phrase}")
        };
        if !has_content(&project_speech_text(&text).text) {
            carry = text;
        } else {
            carry.clear();
            plan.push(text);
        }
    }
    if !carry.is_empty()
        && let Some(last) = plan.last_mut()
    {
        last.push(' ');
        last.push_str(&carry);
    }
    plan
}

fn advance(state: &mut PhraseState, step: PhraseStep) {
    // The pipeline walks the fixed forward order, so a refused step is a bug in
    // this file, not a runtime condition.
    *state = state
        .apply(step)
        .expect("phrase pipeline steps follow the fixed order");
}

impl GenerativePipeline {
    /// Run one generation as a phrased reply.
    ///
    /// Failure before phrase 0 is delivered behaves exactly like the buffered
    /// pipeline (a typed fallback directive, nothing spoken). Failure,
    /// cancellation or a gate refusal of a *later* phrase ends the reply after
    /// the last delivered phrase and is reported in the [`PhraseSummary`]; it
    /// never retries, never replays a delivered phrase and never claims the
    /// reply completed.
    #[allow(clippy::too_many_arguments)]
    pub fn run_phrased<F, D>(
        &self,
        request: &GenerationRequest,
        cancellation: &GenerationCancellationToken,
        fallback: FallbackDirective,
        provider_deadline: Option<Instant>,
        options: &PhraseOptions,
        mut output_gate: F,
        delivery: &mut D,
    ) -> Result<GenerationResult, GenerationError>
    where
        F: FnMut(&str) -> GeneratedTextGateDecision,
        D: PhraseDelivery,
    {
        options.config.validate().map_err(GenerationError::new)?;
        validate_generation_request(request)?;

        let mut trace = GenerationTrace::default();
        if cancellation.is_cancelled() {
            return Ok(cancelled_result(trace, CancellationStage::BeforeThinking));
        }

        let thinking_timeout = match remaining_provider_timeout(provider_deadline) {
            Ok(timeout) => timeout,
            Err(()) => {
                trace.fallback_reason = FallbackReason::Timeout;
                return Ok(fallback_result(trace, fallback));
            }
        };
        trace.thinking_timeout_ms = thinking_timeout.map(duration_ms_ceil);

        let thinking_identity = self.thinking.identity();
        trace.llm_calls.push(LlmCallRecord {
            event_id: request.source_event.event_id.clone(),
            routing_reason: request.routing_reason.clone(),
            backend: thinking_identity.clone(),
        });
        let thinking = match thinking_timeout {
            Some(timeout) => self
                .thinking
                .generate_with_timeout(&request.thinking, timeout),
            None => self.thinking.generate(&request.thinking),
        };
        let reply = match block_on(thinking) {
            Ok(reply) => reply,
            Err(error) => {
                trace.fallback_reason = fallback_reason_from_error(&error);
                return Ok(fallback_result(trace, fallback));
            }
        };
        if cancellation.is_cancelled() {
            return Ok(cancelled_result(trace, CancellationStage::AfterThinking));
        }

        // The whole reply is gated first, before anything is spoken: a policy
        // that refuses (or redacts) any part of it is decided while no phrase
        // is irreversible yet. Every phrase is then gated again on its own
        // text, immediately before its own commit.
        let gated_reply = match output_gate(&reply.text) {
            GeneratedTextGateDecision::Publish(text) if !text.trim().is_empty() => text,
            GeneratedTextGateDecision::Publish(_) | GeneratedTextGateDecision::Reject => {
                trace.fallback_reason = FallbackReason::PolicyOverride;
                return Ok(fallback_result(trace, fallback));
            }
        };
        let plan = plan_phrases(&gated_reply, &options.config.segmentation);
        if plan.is_empty() {
            // Nothing speakable (an all-emoji or URL-only reply).
            trace.fallback_reason = FallbackReason::PolicyOverride;
            return Ok(fallback_result(trace, fallback));
        }

        let tts_identity = self.tts.identity();
        trace.tts_attempted = true;
        trace.tts_backend = Some(tts_identity.clone());
        let planned = plan.len() as u32;
        let mut delivered = 0_u32;
        let mut tts_chars = 0_u64;

        let summary = |delivered: u32, tts_chars: u64, end: PhraseEnd| PhraseSummary {
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

        for (index, phrase_text) in plan.iter().enumerate() {
            let seq = index as u32;
            let final_phrase = index + 1 == plan.len();

            // Lookahead bound: synthesis of phrase `seq` overlaps the playback
            // of phrase `seq - 1`, never of the ones before it.
            if seq > 0 {
                match delivery.wait_for_slot(seq, cancellation) {
                    SlotWait::Ready => {}
                    SlotWait::Cancelled => {
                        let end = PhraseEnd::Cancelled {
                            seq,
                            at: CancellationStage::AfterThinking,
                        };
                        return Ok(phrased(trace, summary(delivered, tts_chars, end)));
                    }
                    SlotWait::Stalled => {
                        let end = PhraseEnd::Failed {
                            seq,
                            failure: PhraseFailure::LookaheadStalled,
                        };
                        return Ok(phrased(trace, summary(delivered, tts_chars, end)));
                    }
                }
            }
            if cancellation.is_cancelled() {
                let end = PhraseEnd::Cancelled {
                    seq,
                    at: CancellationStage::AfterThinking,
                };
                if seq == 0 {
                    return Ok(cancelled_result(trace, CancellationStage::AfterThinking));
                }
                return Ok(phrased(trace, summary(delivered, tts_chars, end)));
            }

            let prepared = self.prepare_phrase(
                request,
                options,
                seq,
                final_phrase,
                phrase_text,
                &mut output_gate,
                provider_deadline,
                (&thinking_identity, &tts_identity),
                cancellation,
                &mut trace,
            );
            let (unit, chars) = match prepared {
                Ok(prepared) => prepared,
                Err(PhraseStop::Cancelled(stage)) => {
                    if seq == 0 {
                        return Ok(cancelled_result(trace, stage));
                    }
                    let end = PhraseEnd::Cancelled { seq, at: stage };
                    return Ok(phrased(trace, summary(delivered, tts_chars, end)));
                }
                Err(PhraseStop::Failed(failure, reason)) => {
                    if seq == 0 {
                        // Nothing was spoken: the buffered fallback applies.
                        trace.fallback_reason = reason;
                        return Ok(fallback_result(trace, fallback));
                    }
                    let end = PhraseEnd::Failed { seq, failure };
                    return Ok(phrased(trace, summary(delivered, tts_chars, end)));
                }
            };
            tts_chars = tts_chars.saturating_add(chars as u64);

            match delivery.deliver(unit, &trace) {
                Delivery::Accepted => delivered += 1,
                Delivery::Closed => {
                    let end = PhraseEnd::Cancelled {
                        seq,
                        at: CancellationStage::BeforePublish,
                    };
                    return Ok(phrased(trace, summary(delivered, tts_chars, end)));
                }
            }
        }

        Ok(phrased(
            trace,
            summary(delivered, tts_chars, PhraseEnd::Completed),
        ))
    }

    /// Gate, project, synthesize and compile one phrase. Returns the unit and
    /// the number of characters sent to TTS.
    #[allow(clippy::too_many_arguments)]
    fn prepare_phrase<F>(
        &self,
        request: &GenerationRequest,
        options: &PhraseOptions,
        seq: u32,
        final_phrase: bool,
        phrase_text: &str,
        output_gate: &mut F,
        provider_deadline: Option<Instant>,
        identities: (
            &aivtuber_domain::BackendIdentity,
            &aivtuber_domain::TtsBackendIdentity,
        ),
        cancellation: &GenerationCancellationToken,
        trace: &mut GenerationTrace,
    ) -> Result<(PhraseUnit, usize), PhraseStop>
    where
        F: FnMut(&str) -> GeneratedTextGateDecision,
    {
        let (thinking_identity, tts_identity) = identities;
        let mut state = PhraseState::Planned;

        // Gate on this phrase's display text.
        let approved = match output_gate(phrase_text) {
            GeneratedTextGateDecision::Publish(text) if !text.trim().is_empty() => text,
            GeneratedTextGateDecision::Publish(_) | GeneratedTextGateDecision::Reject => {
                return Err(PhraseStop::Failed(
                    PhraseFailure::GateRejected,
                    FallbackReason::PolicyOverride,
                ));
            }
        };

        // ADR-0004 D-5, per phrase: deletion-only projection, gated again when
        // it changed anything, with the display text through the gate last so
        // the gate's last decision is the one for the text being committed.
        let projection = self.speech_lexicon.project(&approved);
        trace.record_speech_projection(&projection);
        let speech_text = if projection.report.changed() {
            let decision = output_gate(&projection.speech_text);
            let _ = output_gate(&approved);
            match decision {
                GeneratedTextGateDecision::Publish(text) => text,
                GeneratedTextGateDecision::Reject => String::new(),
            }
        } else {
            approved.clone()
        };
        if speech_text.trim().is_empty() {
            return Err(PhraseStop::Failed(
                PhraseFailure::NothingSpeakable,
                FallbackReason::PolicyOverride,
            ));
        }
        advance(&mut state, PhraseStep::Gate);

        // Per-phrase TTS deadline. Phrase 0 is still bounded by the
        // interaction deadline; later phrases are not, because the audience is
        // already hearing the reply: each gets its own bounded budget.
        let timeout = if seq == 0 {
            match remaining_provider_timeout(provider_deadline) {
                Ok(Some(remaining)) => remaining.min(options.config.tts_timeout),
                Ok(None) => options.config.tts_timeout,
                Err(()) => {
                    return Err(PhraseStop::Failed(
                        PhraseFailure::Synthesis(FallbackReason::Timeout),
                        FallbackReason::Timeout,
                    ));
                }
            }
        } else {
            options.config.tts_timeout
        };
        if seq == 0 {
            trace.tts_timeout_ms = Some(duration_ms_ceil(timeout));
        }

        let chars = speech_text.chars().count();
        let speech_request = SpeechRequest {
            text: speech_text,
            style: request.style.clone(),
        };
        let started = Instant::now();
        let speech = self
            .synthesize(&speech_request, Some(timeout), trace)
            .map_err(|error| {
                let reason = fallback_reason_from_error(&error);
                PhraseStop::Failed(PhraseFailure::Synthesis(reason), reason)
            })?;
        let synthesis_ms = duration_ms_ceil(started.elapsed());
        advance(&mut state, PhraseStep::Synthesize);

        if cancellation.is_cancelled() {
            return Err(PhraseStop::Cancelled(CancellationStage::AfterTts));
        }

        let asset = self
            .compiler
            .compile_phrase(
                request,
                seq,
                &approved,
                &speech,
                thinking_identity,
                tts_identity,
            )
            .map(|asset| {
                asset.with_speech_projection(speech_request.text.clone(), projection.identity.key())
            })
            .map_err(|_| {
                PhraseStop::Failed(PhraseFailure::Compile, FallbackReason::InvalidRequest)
            })?;
        advance(&mut state, PhraseStep::Compile);

        if cancellation.is_cancelled() {
            return Err(PhraseStop::Cancelled(CancellationStage::BeforePublish));
        }

        debug_assert_eq!(state, PhraseState::Compiled);
        Ok((
            PhraseUnit::new(
                options.response_id,
                seq,
                final_phrase,
                approved,
                asset,
                synthesis_ms,
            ),
            chars,
        ))
    }
}

/// Bounded wait for a lookahead slot: poll the cancellation token and the
/// started-phrase counter until the previous phrase has begun playing.
///
/// `started` returns how many leading phrases have started (or ended).
pub fn wait_for_started(
    cancellation: &GenerationCancellationToken,
    seq: u32,
    max_wait: Duration,
    mut started: impl FnMut() -> u32,
) -> SlotWait {
    let deadline = Instant::now() + max_wait;
    loop {
        if cancellation.is_cancelled() {
            return SlotWait::Cancelled;
        }
        if started() >= seq {
            return SlotWait::Ready;
        }
        if Instant::now() >= deadline {
            return SlotWait::Stalled;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        GenerationRoutingReason, PHRASE_SEGMENTATION_VERSION, PerformanceCompiler,
        PerformanceCompilerConfig, PhraseConfig,
    };
    use aivtuber_domain::{
        BackendIdentity, EngineError, EngineErrorKind, EngineFuture, EventEnvelope, GeneratedReply,
        PrivacyClass, ReflexContext, RetrievalSnapshot, SpeechArtifact, ThinkingEngine,
        ThinkingRequest, TtsBackendIdentity, TtsEngine,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const PHRASE_A: &str = "今日は配信に来てくれてありがとうございます。";
    const PHRASE_B: &str = "このあとはゲームを遊びながらお話しします。";
    const PHRASE_C: &str = "最後まで楽しんでいってくださいね。";

    fn three_phrase_reply() -> String {
        format!("{PHRASE_A}{PHRASE_B}{PHRASE_C}")
    }

    struct FixedThinking(String);

    impl ThinkingEngine for FixedThinking {
        fn generate<'a>(
            &'a self,
            _request: &'a ThinkingRequest,
        ) -> EngineFuture<'a, GeneratedReply> {
            let text = self.0.clone();
            Box::pin(async move { Ok(GeneratedReply { text }) })
        }

        fn identity(&self) -> BackendIdentity {
            BackendIdentity {
                name: "fixed-thinking".to_owned(),
                model_alias: Some("fixed".to_owned()),
                model_version: Some("1".to_owned()),
            }
        }
    }

    /// TTS stub: per-call scripted failure, optional cancellation during a
    /// call, and a delay proportional to the text so latency can be compared.
    #[derive(Clone)]
    struct ScriptedTts {
        calls: Arc<AtomicUsize>,
        spoken: Arc<Mutex<Vec<String>>>,
        fail_on_call: Option<(usize, EngineErrorKind)>,
        cancel_on_call: Option<(usize, GenerationCancellationToken)>,
        base_delay: Duration,
        delay_per_char: Duration,
    }

    impl ScriptedTts {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                spoken: Arc::new(Mutex::new(Vec::new())),
                fail_on_call: None,
                cancel_on_call: None,
                base_delay: Duration::ZERO,
                delay_per_char: Duration::ZERO,
            }
        }
    }

    impl TtsEngine for ScriptedTts {
        fn synthesize<'a>(
            &'a self,
            request: &'a SpeechRequest,
        ) -> EngineFuture<'a, SpeechArtifact> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.spoken.lock().unwrap().push(request.text.clone());
            let chars = request.text.chars().count();
            std::thread::sleep(self.base_delay + self.delay_per_char * chars as u32);
            if let Some((target, token)) = &self.cancel_on_call
                && *target == call
            {
                token.cancel();
            }
            let result = match self.fail_on_call {
                Some((target, kind)) if target == call => {
                    Err(EngineError::new(kind, "scripted tts failure"))
                }
                _ => Ok(SpeechArtifact {
                    audio_ref: format!("audio://generated/phrase-{call}.wav"),
                    duration_ms: 300 + 60 * chars as u64,
                    viseme_ref: None,
                }),
            };
            Box::pin(async move { result })
        }

        fn identity(&self) -> TtsBackendIdentity {
            TtsBackendIdentity {
                backend: BackendIdentity {
                    name: "scripted-tts".to_owned(),
                    model_alias: Some("scripted".to_owned()),
                    model_version: Some("1".to_owned()),
                },
                voice_model: Some("voice-ja-v2".to_owned()),
                viseme_mapping: Some("ja-5vowel-v2".to_owned()),
            }
        }
    }

    #[derive(Default)]
    struct RecordingDelivery {
        units: Vec<PhraseUnit>,
        slot_requests: Vec<u32>,
        tts_calls_at_slot_request: Vec<usize>,
        delivered_at: Vec<Instant>,
        tts_calls: Option<Arc<AtomicUsize>>,
        stall_at: Option<u32>,
        close_after: Option<usize>,
    }

    impl PhraseDelivery for RecordingDelivery {
        fn wait_for_slot(
            &mut self,
            seq: u32,
            _cancellation: &GenerationCancellationToken,
        ) -> SlotWait {
            self.slot_requests.push(seq);
            if let Some(calls) = &self.tts_calls {
                self.tts_calls_at_slot_request
                    .push(calls.load(Ordering::SeqCst));
            }
            if self.stall_at == Some(seq) {
                SlotWait::Stalled
            } else {
                SlotWait::Ready
            }
        }

        fn deliver(&mut self, unit: PhraseUnit, _trace: &GenerationTrace) -> Delivery {
            if self.close_after == Some(self.units.len()) {
                return Delivery::Closed;
            }
            self.delivered_at.push(Instant::now());
            self.units.push(unit);
            Delivery::Accepted
        }
    }

    fn chat_event() -> EventEnvelope {
        let mut event: EventEnvelope =
            serde_json::from_str(include_str!("../../../examples/events/chat-message.json"))
                .expect("chat fixture");
        event.event_id = "evt-phrase-1".to_owned();
        event.correlation_id = "corr-phrase".to_owned();
        event.sequence = 1;
        event
    }

    fn request() -> GenerationRequest {
        let source_event = chat_event();
        let thinking = ThinkingRequest::from_event(
            &source_event,
            "hello from the current event",
            PrivacyClass::Pseudonymous,
            ReflexContext::default(),
            RetrievalSnapshot::default(),
            Vec::new(),
        );
        GenerationRequest {
            source_event,
            thinking,
            routing_reason: GenerationRoutingReason::NoCachedMatch,
            intent: "dynamic.reply".to_owned(),
            style: None,
            fallback_variant_group: Some("reaction.surprise".to_owned()),
            seed: 7,
        }
    }

    fn pipeline(reply: &str, tts: ScriptedTts) -> GenerativePipeline {
        GenerativePipeline::new(
            Arc::new(FixedThinking(reply.to_owned())),
            Arc::new(tts),
            PerformanceCompiler::new(PerformanceCompilerConfig {
                compiler_version: "0.1.0".to_owned(),
                avatar_profile: Some("example-live2d-v1".to_owned()),
                motion_library: Some("starter-v1".to_owned()),
                expression_preset: Some("speaking.neutral".to_owned()),
                expression_intensity: 0.4,
            })
            .expect("compiler"),
        )
    }

    fn options() -> PhraseOptions {
        PhraseOptions {
            response_id: 41,
            config: PhraseConfig::default(),
        }
    }

    fn fallback() -> FallbackDirective {
        FallbackDirective::NonVerbalReaction {
            reaction_family: "reaction.surprise".to_owned(),
        }
    }

    fn run(
        pipeline: &GenerativePipeline,
        token: &GenerationCancellationToken,
        gate: impl FnMut(&str) -> GeneratedTextGateDecision,
        delivery: &mut RecordingDelivery,
    ) -> GenerationResult {
        pipeline
            .run_phrased(
                &request(),
                token,
                fallback(),
                None,
                &options(),
                gate,
                delivery,
            )
            .expect("pipeline")
    }

    fn publish(text: &str) -> GeneratedTextGateDecision {
        GeneratedTextGateDecision::Publish(text.to_owned())
    }

    fn summary(result: &GenerationResult) -> &PhraseSummary {
        match &result.disposition {
            GenerationDisposition::Phrased { summary } => summary,
            other => panic!("expected a phrased disposition, got {other:?}"),
        }
    }

    #[test]
    fn a_reply_becomes_complete_validated_phrase_units_in_order() {
        let tts = ScriptedTts::new();
        let spoken = Arc::clone(&tts.spoken);
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let mut delivery = RecordingDelivery::default();
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );

        let summary = summary(&result);
        assert!(summary.completed());
        assert_eq!(summary.planned_phrases, 3);
        assert_eq!(summary.delivered_phrases, 3);
        assert_eq!(summary.response_id, 41);
        assert_eq!(summary.segmentation_version, PHRASE_SEGMENTATION_VERSION);
        assert_eq!(
            spoken.lock().unwrap().as_slice(),
            [PHRASE_A, PHRASE_B, PHRASE_C],
            "TTS got each complete phrase, never a fragment or the whole reply"
        );
        assert!(result.trace.tts_attempted);

        for (index, unit) in delivery.units.iter().enumerate() {
            assert_eq!(unit.seq(), index as u32);
            assert_eq!(unit.is_final(), index == 2);
            assert_eq!(unit.response_id(), 41);
            assert_eq!(unit.state(), PhraseState::Compiled);
            unit.asset().validate().expect("each asset validates");
            let speech = unit.asset().speech.as_ref().expect("speech track");
            let duration = speech.duration_ms.expect("complete audio duration");
            assert!(speech.audio_ref.is_some());
            assert_eq!(speech.text.as_deref(), Some(unit.display_text()));
            assert_eq!(
                unit.asset().interrupt_points_ms,
                vec![duration],
                "every phrase declares an interrupt point at its end"
            );
            assert_eq!(unit.interrupt_boundary_ms(), duration);
            assert_eq!(
                unit.asset()
                    .provenance
                    .as_ref()
                    .and_then(|p| p.tts_backend.as_deref()),
                Some("scripted-tts")
            );
        }
        // Only the first phrase starts the expression; later ones continue it.
        assert!(delivery.units[0].asset().expression.is_some());
        assert!(delivery.units[1].asset().expression.is_none());
        assert!(delivery.units[2].asset().expression.is_none());
        assert_eq!(
            delivery.slot_requests,
            vec![1, 2],
            "slot asked before phrases 1 and 2"
        );
    }

    #[test]
    fn a_failure_in_phrase_two_ends_the_reply_after_phrase_one_without_retry_or_fallback() {
        let mut tts = ScriptedTts::new();
        tts.fail_on_call = Some((1, EngineErrorKind::Timeout));
        let calls = Arc::clone(&tts.calls);
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let mut delivery = RecordingDelivery::default();
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );

        let summary = summary(&result);
        assert_eq!(delivery.units.len(), 1, "phrase 1 stays delivered, once");
        assert_eq!(summary.delivered_phrases, 1);
        assert_eq!(
            summary.end,
            PhraseEnd::Failed {
                seq: 1,
                failure: PhraseFailure::Synthesis(FallbackReason::Timeout)
            }
        );
        assert!(!summary.completed(), "a partial reply is never complete");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one failed attempt, no retry storm, phrase 3 never attempted"
        );
        assert_eq!(result.trace.fallback_reason, FallbackReason::None);
    }

    #[test]
    fn a_failure_before_phrase_one_is_the_buffered_fallback() {
        let mut tts = ScriptedTts::new();
        tts.fail_on_call = Some((0, EngineErrorKind::Unavailable));
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let mut delivery = RecordingDelivery::default();
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );
        assert!(delivery.units.is_empty());
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Fallback { .. }
        ));
        assert_eq!(result.trace.fallback_reason, FallbackReason::Unavailable);
    }

    #[test]
    fn phrase_two_cancelled_mid_synthesis_is_never_delivered() {
        let token = GenerationCancellationToken::default();
        let mut tts = ScriptedTts::new();
        tts.cancel_on_call = Some((1, token.clone()));
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let mut delivery = RecordingDelivery::default();
        let result = run(&pipeline, &token, publish, &mut delivery);

        let summary = summary(&result);
        assert_eq!(delivery.units.len(), 1, "phrase 1 was already delivered");
        assert_eq!(delivery.units[0].seq(), 0);
        assert_eq!(
            summary.end,
            PhraseEnd::Cancelled {
                seq: 1,
                at: CancellationStage::AfterTts
            }
        );
        assert_eq!(
            delivery.slot_requests,
            vec![1],
            "phrase 3 never asked for a slot"
        );
    }

    #[test]
    fn a_cancellation_before_the_first_phrase_is_a_plain_cancelled_result() {
        let token = GenerationCancellationToken::default();
        let mut tts = ScriptedTts::new();
        tts.cancel_on_call = Some((0, token.clone()));
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let mut delivery = RecordingDelivery::default();
        let result = run(&pipeline, &token, publish, &mut delivery);
        assert!(delivery.units.is_empty());
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Cancelled {
                stage: CancellationStage::AfterTts
            }
        ));
    }

    #[test]
    fn the_gate_refusing_phrase_two_after_phrase_one_was_delivered_ends_the_reply() {
        let tts = ScriptedTts::new();
        let calls = Arc::clone(&tts.calls);
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let mut delivery = RecordingDelivery::default();
        let mut gate_calls = 0_usize;
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            |text| {
                gate_calls += 1;
                // Call 1 is the whole reply, call 2 phrase 1, call 3 phrase 2.
                if gate_calls == 3 {
                    GeneratedTextGateDecision::Reject
                } else {
                    publish(text)
                }
            },
            &mut delivery,
        );
        let summary = summary(&result);
        assert_eq!(delivery.units.len(), 1);
        assert_eq!(
            summary.end,
            PhraseEnd::Failed {
                seq: 1,
                failure: PhraseFailure::GateRejected
            }
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a refused phrase is never synthesized"
        );
    }

    #[test]
    fn the_whole_reply_is_refused_before_any_phrase_is_irreversible() {
        let tts = ScriptedTts::new();
        let calls = Arc::clone(&tts.calls);
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let mut delivery = RecordingDelivery::default();
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            |_| GeneratedTextGateDecision::Reject,
            &mut delivery,
        );
        assert!(delivery.units.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(result.trace.fallback_reason, FallbackReason::PolicyOverride);
    }

    #[test]
    fn each_phrase_is_gated_on_its_own_text_and_the_display_text_goes_through_last() {
        let reply =
            "ありがとう😊 これからゲームを始めます。このあとも一緒に楽しんでいきましょうね。";
        let tts = ScriptedTts::new();
        let spoken = Arc::clone(&tts.spoken);
        let pipeline = pipeline(reply, tts);
        let mut delivery = RecordingDelivery::default();
        let mut seen: Vec<String> = Vec::new();
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            |text| {
                seen.push(text.to_owned());
                publish(text)
            },
            &mut delivery,
        );
        assert!(summary(&result).completed());
        let first_display = delivery.units[0].display_text().to_owned();
        assert!(first_display.contains('😊'), "display text keeps the emoji");
        assert!(
            !spoken.lock().unwrap()[0].contains('😊'),
            "TTS gets the projection"
        );
        // whole reply, then phrase 1 display, its projection, phrase 1 display
        // again (last), then phrase 2.
        assert_eq!(seen[0], reply);
        assert_eq!(seen[1], first_display);
        assert!(!seen[2].contains('😊'));
        assert_eq!(seen[3], first_display, "the display text is gated last");
    }

    #[test]
    fn every_phrase_is_projected_with_the_lexicon_and_records_the_projection_identity() {
        let reply = "今日はYouTubeでライブをします。AIとVTuberの話をたくさんしますので、最後まで見ていってくださいね。";
        let tts = ScriptedTts::new();
        let spoken = Arc::clone(&tts.spoken);
        let pipeline = pipeline(reply, tts);
        let mut delivery = RecordingDelivery::default();
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );
        assert!(summary(&result).completed());
        assert!(delivery.units.len() >= 2, "{:?}", delivery.units.len());
        let key = pipeline.speech_lexicon().identity().key();
        for (unit, sent) in delivery.units.iter().zip(spoken.lock().unwrap().iter()) {
            let asset = unit.asset();
            asset.validate().expect("valid 0.2.0 asset");
            let speech = asset.speech.as_ref().unwrap();
            assert_eq!(speech.text.as_deref(), Some(unit.display_text()));
            assert_eq!(speech.speech_text.as_deref(), Some(sent.as_str()));
            assert_eq!(
                asset.compatibility.speech_projection.as_deref(),
                Some(key.as_str())
            );
            assert!(
                !sent.contains("YouTube") && !sent.contains("VTuber"),
                "{sent}"
            );
        }
        let all: String = spoken.lock().unwrap().join("");
        assert!(
            all.contains("ユーチューブ")
                && all.contains("エーアイ")
                && all.contains("ブイチューバー")
        );
        assert!(
            delivery.units[0].display_text().contains("YouTube"),
            "display text keeps the Latin brand word"
        );
    }

    #[test]
    fn a_url_only_phrase_joins_its_neighbour_instead_of_being_dropped() {
        let config = crate::phrase::SegmentationConfig::default();
        let plan = plan_phrases(
            "https://example.com/live。今日は配信に来てくれてありがとうございます。",
            &config,
        );
        assert_eq!(plan.len(), 1, "{plan:?}");
        assert!(plan[0].contains("https://example.com/live"));
        assert!(plan[0].contains("ありがとう"));
        assert!(plan_phrases("😊😊😊", &config).is_empty());
    }

    #[test]
    fn identical_phrases_get_distinct_asset_ids() {
        let reply = format!("{PHRASE_A}{PHRASE_A}");
        let pipeline = pipeline(&reply, ScriptedTts::new());
        let mut delivery = RecordingDelivery::default();
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );
        assert!(summary(&result).completed());
        assert_eq!(delivery.units.len(), 2);
        assert_ne!(delivery.units[0].asset().id, delivery.units[1].asset().id);
    }

    #[test]
    fn synthesis_of_phrase_n_is_not_started_until_the_slot_for_n_is_granted() {
        let tts = ScriptedTts::new();
        let mut delivery = RecordingDelivery {
            tts_calls: Some(Arc::clone(&tts.calls)),
            ..RecordingDelivery::default()
        };
        let pipeline = pipeline(&three_phrase_reply(), tts);
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );
        assert!(summary(&result).completed());
        assert_eq!(
            delivery.tts_calls_at_slot_request,
            vec![1, 2],
            "asking for phrase n happens after exactly n phrases were synthesized"
        );
    }

    #[test]
    fn a_stalled_slot_ends_the_reply_with_a_typed_failure() {
        let pipeline = pipeline(&three_phrase_reply(), ScriptedTts::new());
        let mut delivery = RecordingDelivery {
            stall_at: Some(1),
            ..RecordingDelivery::default()
        };
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );
        assert_eq!(
            summary(&result).end,
            PhraseEnd::Failed {
                seq: 1,
                failure: PhraseFailure::LookaheadStalled
            }
        );
        assert_eq!(delivery.units.len(), 1);
    }

    #[test]
    fn a_closed_delivery_stops_the_reply() {
        let pipeline = pipeline(&three_phrase_reply(), ScriptedTts::new());
        let mut delivery = RecordingDelivery {
            close_after: Some(1),
            ..RecordingDelivery::default()
        };
        let result = run(
            &pipeline,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );
        let summary = summary(&result);
        assert_eq!(summary.delivered_phrases, 1);
        assert!(matches!(summary.end, PhraseEnd::Cancelled { seq: 1, .. }));
    }

    #[test]
    fn wait_for_started_polls_until_the_previous_phrase_has_started() {
        let token = GenerationCancellationToken::default();
        let mut polls = 0_u32;
        let outcome = wait_for_started(&token, 1, Duration::from_secs(2), || {
            polls += 1;
            u32::from(polls >= 3)
        });
        assert_eq!(outcome, SlotWait::Ready);
        assert!(polls >= 3);

        assert_eq!(
            wait_for_started(&token, 1, Duration::from_millis(20), || 0),
            SlotWait::Stalled
        );
        token.cancel();
        assert_eq!(
            wait_for_started(&token, 1, Duration::from_secs(2), || 0),
            SlotWait::Cancelled
        );
    }

    /// First-audio latency, measured against the buffered pipeline with a TTS
    /// stub whose cost grows with the text. The first phrase is committable
    /// after one phrase of synthesis; buffered mode synthesizes the whole
    /// reply first.
    #[test]
    fn the_first_phrase_is_ready_far_sooner_than_the_whole_buffered_reply() {
        let reply = three_phrase_reply();
        let make_tts = || {
            let mut tts = ScriptedTts::new();
            tts.base_delay = Duration::from_millis(20);
            tts.delay_per_char = Duration::from_millis(4);
            tts
        };

        let buffered = pipeline(&reply, make_tts());
        let started = Instant::now();
        let result = buffered
            .run(&request(), &GenerationCancellationToken::default(), None)
            .expect("buffered");
        let buffered_first_audio = started.elapsed();
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Generated { .. }
        ));

        let phrased = pipeline(&reply, make_tts());
        let mut delivery = RecordingDelivery::default();
        let started = Instant::now();
        let result = run(
            &phrased,
            &GenerationCancellationToken::default(),
            publish,
            &mut delivery,
        );
        let phrased_completion = started.elapsed();
        let phrased_first_audio = delivery.delivered_at[0] - started;
        assert!(summary(&result).completed());

        assert!(
            phrased_first_audio * 2 < buffered_first_audio,
            "first phrase {phrased_first_audio:?} vs buffered {buffered_first_audio:?}"
        );
        // Per-phrase overhead makes total synthesis a little longer, never
        // dramatically so; the win is first audio, not completion.
        assert!(phrased_completion < buffered_first_audio * 2);
    }
}
