//! Thinking usage accounting and the optional spend cap (#367) through the
//! real `local` composition: the meter wraps the engine, the generative
//! pipeline records per-turn usage, and a tripped cap degrades to the
//! deterministic fallback on every later turn.

use super::local_thinking_fallback_tests::{LocalRig, free_text_line, local_rig_with_engines};
use super::*;
use aivtuber_generative::{PhraseConfig, StreamPolicy};
use std::sync::atomic::{AtomicUsize, Ordering};

const REPLY: &str = "今日は配信に来てくれてありがとうございます。";

/// Answers every call with one reply carrying the same usage; counts calls.
#[derive(Clone)]
struct CostlyThinking {
    usage: Option<aivtuber_domain::ThinkingUsage>,
    calls: Arc<AtomicUsize>,
}

impl ThinkingEngine for CostlyThinking {
    fn generate<'a>(
        &'a self,
        _: &'a aivtuber_domain::ThinkingRequest,
    ) -> EngineFuture<'a, GeneratedReply> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = self.usage;
        Box::pin(async move {
            Ok(GeneratedReply {
                text: REPLY.to_owned(),
                usage,
            })
        })
    }

    fn streams(&self) -> bool {
        true
    }

    fn generate_streaming(
        &self,
        _: &aivtuber_domain::ThinkingRequest,
        _: Duration,
        sink: &mut dyn aivtuber_domain::ReplyDeltaSink,
    ) -> Option<Result<GeneratedReply, EngineError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = sink.push(REPLY);
        Some(Ok(GeneratedReply {
            text: REPLY.to_owned(),
            usage: self.usage,
        }))
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "costly-test".to_owned(),
            model_alias: None,
            model_version: None,
        }
    }
}

fn usage(cost: Option<u64>) -> Option<aivtuber_domain::ThinkingUsage> {
    Some(aivtuber_domain::ThinkingUsage {
        prompt_tokens: 120,
        completion_tokens: 30,
        cost_micro_usd: cost,
    })
}

fn rig(
    usage: Option<aivtuber_domain::ThinkingUsage>,
    cap_micro_usd: Option<u64>,
    mode: PhraseMode,
) -> (LocalRig, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let meter = ThinkingMeter::new(cap_micro_usd);
    let engine = MeteredThinking::new(
        Arc::new(CostlyThinking {
            usage,
            calls: Arc::clone(&calls),
        }),
        meter.clone(),
    );
    let tts = MockTts::default();
    let tts_texts = Arc::clone(&tts.texts);
    let mut rig = local_rig_with_engines(engine, tts, mode);
    rig.tts_calls = tts_texts;
    rig.app = rig.app.with_thinking_meter(meter);
    (rig, calls)
}

#[test]
fn a_tripped_cap_degrades_every_later_turn_to_the_deterministic_fallback() {
    for mode in [
        PhraseMode::Buffered,
        PhraseMode::Phrased(PhraseConfig::default()),
    ] {
        let (mut rig, calls) = rig(usage(Some(1_000_000)), Some(1_000_000), mode);
        free_text_line(&mut rig, 61);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let status = rig.app.thinking_usage_status().expect("meter composed");
        assert!(status.cap_tripped, "{status:?}");
        assert_eq!(status.cost_micro_usd, Some(1_000_000));

        // Later turns never reach the provider and use the typed fallback.
        for sequence in [62, 63] {
            free_text_line(&mut rig, sequence);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no provider call after the trip"
        );
        let status = rig.app.thinking_usage_status().expect("meter composed");
        assert_eq!(status.suspended_calls, 2);
        assert_eq!(status.turns_with_usage, 1);

        let events = rig.app.telemetry().events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].thinking_cost_micro_usd, Some(1_000_000));
        for event in &events[1..] {
            assert_eq!(event.route, RouteClass::CachedFallback);
            assert_eq!(event.fallback_reason.as_deref(), Some("unavailable"));
            assert_eq!(event.thinking_cost_micro_usd, None);
            assert_eq!(event.thinking_prompt_tokens, None);
        }
        assert_eq!(
            rig.tts_calls.lock().expect("tts log").len(),
            1,
            "only the first turn spoke"
        );
    }
}

#[test]
fn unknown_cost_never_trips_the_cap() {
    let (mut rig, calls) = rig(usage(None), Some(1), PhraseMode::Buffered);
    for sequence in [64, 65] {
        free_text_line(&mut rig, sequence);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let status = rig.app.thinking_usage_status().expect("meter composed");
    assert!(!status.cap_tripped);
    assert_eq!(status.cost_micro_usd, None);
    assert_eq!((status.prompt_tokens, status.completion_tokens), (240, 60));
    let events = rig.app.telemetry().events();
    assert_eq!(events[0].thinking_prompt_tokens, Some(120));
    assert_eq!(events[0].thinking_completion_tokens, Some(30));
    assert_eq!(
        events[0].thinking_cost_micro_usd, None,
        "unknown stays None"
    );
}

#[test]
fn usage_from_the_streamed_path_is_recorded_per_turn_and_in_the_totals() {
    let config = PhraseConfig {
        stream: Some(StreamPolicy::default()),
        ..PhraseConfig::default()
    };
    let (mut rig, calls) = rig(usage(Some(2_500)), None, PhraseMode::Phrased(config));
    free_text_line(&mut rig, 66);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = rig.app.telemetry().events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].thinking_prompt_tokens, Some(120));
    assert_eq!(events[0].thinking_cost_micro_usd, Some(2_500));
    let status = rig.app.thinking_usage_status().expect("meter composed");
    assert_eq!(status.cost_micro_usd, Some(2_500));
    assert_eq!(status.cap_micro_usd, None);
}

#[test]
fn status_snapshot_carries_the_usage_and_omits_it_without_a_meter() {
    let (rig, _) = rig(usage(Some(7)), Some(9_000_000), PhraseMode::Buffered);
    let snapshot = OperatorStatusSnapshot::from_app(&rig.app, 0);
    let usage = snapshot.thinking_usage.clone().expect("composed");
    assert_eq!(usage.cap_micro_usd, Some(9_000_000));
    let json = serde_json::to_value(&snapshot).expect("json");
    assert_eq!(json["thinking_usage"]["cap_tripped"], json!(false));

    let bare = local_rig_with_engines(
        CostlyThinking {
            usage: None,
            calls: Arc::new(AtomicUsize::new(0)),
        },
        MockTts::default(),
        PhraseMode::Buffered,
    );
    let json = serde_json::to_value(OperatorStatusSnapshot::from_app(&bare.app, 0)).expect("json");
    assert!(json.get("thinking_usage").is_none());
}
