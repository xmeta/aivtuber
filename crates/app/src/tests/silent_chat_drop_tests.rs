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

/// #469 (reopened): three chat lines arrive while the first generation is
/// still stalled. The first two are superseded by newer lines, the last one
/// misses its deadline; none may end with only a `route` record.
#[test]
fn chat_queued_behind_a_stalled_generation_always_ends_in_a_reply_a_fallback_or_a_logged_supersession()
 {
    use crate::session_log::{Record, SessionLog};
    for mode in both_modes() {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-469b-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let mut rig = rig_with(
            "今日は来てくれてありがとう。",
            Duration::from_millis(600),
            mode,
            300,
        );
        rig.app = rig
            .app
            .with_session_log(SessionLog::new(dir.join("logs"), None, true));
        for sequence in 71..=73 {
            submit_free_text_line(&mut rig, sequence);
        }
        wait_until_quiescent(&mut rig.app);
        play_out(&mut rig.app);
        rig.app.session_log().expect("log").flush();

        let file = std::fs::read_dir(dir.join("logs"))
            .expect("logs")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .max()
            .expect("a session file");
        let records: Vec<Record> = std::fs::read_to_string(file)
            .expect("read")
            .lines()
            .map(|line| serde_json::from_str(line).expect("record"))
            .collect();
        let turns: Vec<String> = records
            .iter()
            .filter(|r| r.t == "input")
            .filter_map(|r| r.turn.clone())
            .collect();
        assert_eq!(turns.len(), 3, "{records:?}");
        for turn in &turns {
            let ends = records
                .iter()
                .filter(|r| r.turn.as_ref() == Some(turn) && (r.t == "phrase" || r.t == "fallback"))
                .count();
            assert_eq!(ends, 1, "turn {turn} must end exactly once: {records:?}");
        }
        let counts = rig.app.generation_fallback_counts();
        let counted: u64 = counts.by_reason.values().sum::<u64>() + counts.superseded;
        assert_eq!(counted, 3, "{counts:?}");
        assert!(counts.superseded >= 1, "{counts:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
