//! A chat line is never dropped silently when Thinking stalls past the
//! interaction deadline or yields no speakable phrase (issue #469).
//!
//! Thinking is a stub that answers correctly but late (or answers with only
//! whitespace); TTS must never be reached for the late case's fallback.

use super::local_thinking_fallback_tests::{
    LocalRig, both_modes, local_rig_with_engines, play_out, submit_free_text_line,
    wait_until_quiescent,
};
use super::*;

/// Answers `reply` after blocking `stall`: a contended host, as in the #87 S2
/// run where one request took 22 s.
struct StallThinking {
    reply: String,
    stall: Duration,
}

impl ThinkingEngine for StallThinking {
    fn generate<'a>(
        &'a self,
        _request: &'a aivtuber_domain::ThinkingRequest,
    ) -> EngineFuture<'a, GeneratedReply> {
        Box::pin(async move {
            std::thread::sleep(self.stall);
            Ok(GeneratedReply {
                text: self.reply.clone(),
                usage: None,
            })
        })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "stall-thinking".to_owned(),
            model_alias: Some("stall".to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

fn rig_with(reply: &str, stall: Duration, mode: PhraseMode, deadline_ms: u64) -> LocalRig {
    let mut rig = local_rig_with_engines(
        StallThinking {
            reply: reply.to_owned(),
            stall,
        },
        MockTts::default(),
        mode,
    );
    rig.app = rig
        .app
        .with_interaction_deadline_policy(InteractionDeadlinePolicy {
            conversation_ms: deadline_ms,
            ..InteractionDeadlinePolicy::default()
        })
        .expect("deadline policy");
    rig
}

fn assert_counted_visible_fallback(rig: &mut LocalRig, reason: &str) {
    play_out(&mut rig.app);
    let counts = rig.app.generation_fallback_counts();
    assert_eq!(counts.by_reason.get(reason), Some(&1), "{counts:?}");
    assert_eq!((counts.visible, counts.silent), (1, 0), "{counts:?}");
    let observations = rig.app.telemetry().events();
    assert_eq!(observations.len(), 1, "{observations:?}");
    assert_eq!(observations[0].route, RouteClass::CachedFallback);
    assert_eq!(observations[0].fallback_reason.as_deref(), Some(reason));
    assert!(
        !rig.avatar.lock().expect("avatar log").is_empty(),
        "the viewer sees the acknowledgement"
    );
}

#[test]
fn a_reply_that_arrives_after_the_interaction_deadline_ends_in_a_counted_fallback() {
    for mode in both_modes() {
        let mut rig = rig_with(
            "今日は来てくれてありがとう。",
            Duration::from_millis(700),
            mode,
            300,
        );
        submit_free_text_line(&mut rig, 61);
        wait_until_quiescent(&mut rig.app);
        assert!(
            rig.audio.lock().expect("audio log").is_empty(),
            "a reply past its deadline is not spoken late"
        );
        assert_counted_visible_fallback(&mut rig, "timeout");
    }
}

#[test]
fn a_whitespace_only_reply_ends_in_a_counted_fallback() {
    for mode in both_modes() {
        let mut rig = rig_with("  \n\t ", Duration::ZERO, mode, 30_000);
        submit_free_text_line(&mut rig, 62);
        wait_until_quiescent(&mut rig.app);
        assert_counted_visible_fallback(&mut rig, "policy_override");
    }
}
