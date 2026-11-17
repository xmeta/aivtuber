//! The opt-in affect judge's counters in the operator status (#426).

use super::local_thinking_fallback_tests::local_rig_with_engines;
use super::*;
use aivtuber_generative::{AffectClass, AffectJudge, AffectJudgeFailure, AffectJudgeRunner};
use std::time::Duration;

struct Fixed(Result<AffectClass, AffectJudgeFailure>);

impl AffectJudge for Fixed {
    fn judge(&self, _: &str, _: Duration) -> Result<AffectClass, AffectJudgeFailure> {
        self.0
    }
}

#[derive(Clone)]
struct Silent;

impl ThinkingEngine for Silent {
    fn generate<'a>(
        &'a self,
        _: &'a aivtuber_domain::ThinkingRequest,
    ) -> EngineFuture<'a, GeneratedReply> {
        Box::pin(async {
            Ok(GeneratedReply {
                text: "x".to_owned(),
                usage: None,
            })
        })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "silent".to_owned(),
            model_alias: None,
            model_version: None,
        }
    }
}

#[test]
fn status_carries_the_judge_counters_and_omits_them_by_default() {
    let mut rig = local_rig_with_engines(Silent, MockTts::default(), PhraseMode::Buffered);
    let json = serde_json::to_value(OperatorStatusSnapshot::from_app(&rig.app, 0)).expect("json");
    assert!(json.get("affect_judge").is_none(), "default is unchanged");
    assert!(rig.app.affect_judge_status().is_none());

    let runner = AffectJudgeRunner::new(
        Arc::new(Fixed(Err(AffectJudgeFailure::RateLimited))),
        Duration::from_millis(300),
    )
    .expect("runner");
    rig.app = rig.app.with_affect_judge_stats(runner.stats());
    let ticket = runner.start("synthetic phrase");
    let until = std::time::Instant::now() + Duration::from_secs(3);
    while runner.in_flight() > 0 && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ticket.resolve().is_err());

    let json = serde_json::to_value(OperatorStatusSnapshot::from_app(&rig.app, 0)).expect("json");
    assert_eq!(json["affect_judge"]["rate_limited"], json!(1));
    assert_eq!(json["affect_judge"]["judged"], json!(0));
    assert!(
        !json.to_string().contains("synthetic phrase"),
        "status never carries text"
    );
}

#[test]
fn the_commit_record_takes_the_phrase_s_own_affect_decision() {
    use aivtuber_generative::{AffectDecision, AffectFallbackReason, AffectSource};
    let mut trace = GenerationTrace::default();
    assert_eq!(crate::phrase_commit::affect_decision_for(&trace, 0), None);
    let first = AffectDecision {
        seq: 0,
        source: AffectSource::Judge,
        class: AffectClass::Joy,
    };
    let second = AffectDecision {
        seq: 1,
        source: AffectSource::RulesFallback(AffectFallbackReason::Late),
        class: AffectClass::Neutral,
    };
    trace.affect_decisions = vec![first, second];
    assert_eq!(
        crate::phrase_commit::affect_decision_for(&trace, 1),
        Some(second)
    );
    assert_eq!(
        crate::phrase_commit::affect_decision_for(&trace, 0),
        Some(first)
    );
    assert_eq!(crate::phrase_commit::affect_decision_for(&trace, 2), None);
}
