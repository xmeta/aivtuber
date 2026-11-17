//! Opt-in System One affect judgement of reply phrases (#426).
//!
//! `AIVTUBER_AFFECT_JUDGE=system-one` makes each generated phrase's gated
//! display text a closed six-class System One question that runs in parallel
//! with the phrase's TTS synthesis (see `aivtuber_generative::affect_judge`
//! for the timing rules). The default, `rules`, composes nothing and keeps the
//! S1 rules classifier exactly as it was.
//!
//! This is a new egress role: reply text leaves the machine to the System One
//! provider, per phrase. It is therefore refused under the `local` and
//! `hybrid` profiles, whose non-Thinking roles are loopback-only by policy
//! (#220, #364), and is allowed only where Jev is already a composed provider
//! (`full`).

use aivtuber_generative::{AffectClass, AffectJudge, AffectJudgeFailure};
use aivtuber_reflex::{AffectJudgeError, JevAffectJudge};
use std::time::Duration;

pub const AFFECT_JUDGE_VAR: &str = "AIVTUBER_AFFECT_JUDGE";
pub const AFFECT_JUDGE_DEADLINE_VAR: &str = "AIVTUBER_AFFECT_JUDGE_DEADLINE_MS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffectJudgeMode {
    /// S1 rules classifier only (default).
    Rules,
    /// Rules classifier plus the System One judge in parallel with TTS.
    SystemOne,
}

impl AffectJudgeMode {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("rules") => Ok(Self::Rules),
            Some("system-one") => Ok(Self::SystemOne),
            Some(other) => Err(format!(
                "{AFFECT_JUDGE_VAR} must be \"rules\" or \"system-one\", got {other:?}"
            )),
        }
    }

    /// Refuse a configuration that would send text where policy forbids it or
    /// where the judge could not work. Called before anything is composed.
    pub fn validate(
        self,
        profile_name: &str,
        profile_allows_jev_egress: bool,
        affect_expression_enabled: bool,
        jev_api_key_present: bool,
    ) -> Result<(), String> {
        if self == Self::Rules {
            return Ok(());
        }
        if !profile_allows_jev_egress {
            return Err(format!(
                "{AFFECT_JUDGE_VAR}=system-one sends reply text to the System One provider, which profile {profile_name:?} does not allow (its non-Thinking roles are loopback-only); use the \"full\" profile or leave it at \"rules\""
            ));
        }
        if !affect_expression_enabled {
            return Err(format!(
                "{AFFECT_JUDGE_VAR}=system-one needs AIVTUBER_AFFECT_EXPRESSION=true (the judge only picks among the expressions the affect mapping offers)"
            ));
        }
        if !jev_api_key_present {
            return Err(format!(
                "{AFFECT_JUDGE_VAR}=system-one needs System One credentials (AIVTUBER_JEV_API_KEY)"
            ));
        }
        Ok(())
    }
}

/// Bilingual start-up notice for the opt-in; names what leaves the machine.
pub fn startup_notice(hard_cap: Duration) -> String {
    format!(
        "affect judge: system-one ON, the text of each reply phrase is sent to the System One provider (AIVTUBER_JEV_ENDPOINT) to pick the expression; deadline {} ms, falls back to the local rules; text is never logged / \
         表情判定: system-one 有効。返答の各フレーズの本文が System One プロバイダ (AIVTUBER_JEV_ENDPOINT) に送信されます。期限 {} ms、間に合わない・失敗時はローカルのルール判定を使います。本文はログに残しません",
        hard_cap.as_millis(),
        hard_cap.as_millis()
    )
}

/// [`AffectJudge`] over the existing Jev client.
pub struct SystemOneAffectJudge(JevAffectJudge);

impl SystemOneAffectJudge {
    pub fn new(judge: JevAffectJudge) -> Self {
        Self(judge)
    }
}

impl AffectJudge for SystemOneAffectJudge {
    fn judge(&self, text: &str, deadline: Duration) -> Result<AffectClass, AffectJudgeFailure> {
        match self.0.judge(text, deadline) {
            Ok(name) => AffectClass::parse(name).ok_or(AffectJudgeFailure::Malformed),
            Err(AffectJudgeError::Timeout) => Err(AffectJudgeFailure::Timeout),
            Err(AffectJudgeError::RateLimited) => Err(AffectJudgeFailure::RateLimited),
            Err(AffectJudgeError::Unavailable) => Err(AffectJudgeFailure::Unavailable),
            Err(AffectJudgeError::Malformed) => Err(AffectJudgeFailure::Malformed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_rules_compose_nothing() {
        assert_eq!(AffectJudgeMode::parse(None), Ok(AffectJudgeMode::Rules));
        assert_eq!(AffectJudgeMode::parse(Some("")), Ok(AffectJudgeMode::Rules));
        assert_eq!(
            AffectJudgeMode::parse(Some(" Rules ")),
            Ok(AffectJudgeMode::Rules)
        );
        assert_eq!(
            AffectJudgeMode::parse(Some("system-one")),
            Ok(AffectJudgeMode::SystemOne)
        );
        assert!(AffectJudgeMode::parse(Some("on")).is_err());
        // `rules` never refuses, whatever else is configured.
        assert!(
            AffectJudgeMode::Rules
                .validate("local", false, false, false)
                .is_ok()
        );
    }

    #[test]
    fn system_one_is_refused_where_text_may_not_leave_or_nothing_could_use_it() {
        let sys = AffectJudgeMode::SystemOne;
        assert!(sys.validate("full", true, true, true).is_ok());
        for profile in ["local", "hybrid"] {
            let error = sys.validate(profile, false, true, true).unwrap_err();
            assert!(error.contains("loopback-only"), "{error}");
        }
        assert!(sys.validate("full", true, false, true).is_err());
        assert!(sys.validate("full", true, true, false).is_err());
    }

    #[test]
    fn the_notice_is_bilingual_and_names_the_egress() {
        let notice = startup_notice(Duration::from_millis(1_200));
        assert!(notice.contains("sent to the System One provider"));
        assert!(notice.contains("送信されます"));
        assert!(notice.contains("1200"));
    }

    #[test]
    fn the_class_names_of_the_wire_match_the_domain_classes() {
        for name in aivtuber_reflex::AFFECT_CLASS_NAMES {
            assert!(AffectClass::parse(name).is_some(), "{name}");
        }
        assert_eq!(
            aivtuber_reflex::AFFECT_CLASS_NAMES.len(),
            AffectClass::ALL.len()
        );
    }
}
