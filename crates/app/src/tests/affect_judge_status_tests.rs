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
    std::thread::sleep(Duration::from_millis(50));
    assert!(ticket.resolve().is_err());

    let json = serde_json::to_value(OperatorStatusSnapshot::from_app(&rig.app, 0)).expect("json");
    assert_eq!(json["affect_judge"]["rate_limited"], json!(1));
    assert_eq!(json["affect_judge"]["judged"], json!(0));
    assert!(
        !json.to_string().contains("synthetic phrase"),
        "status never carries text"
    );
}
