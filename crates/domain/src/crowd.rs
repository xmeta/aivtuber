//! Crowd summary for a reply context (issue #79 S2).
//!
//! A [`CrowdSummary`] tells a Thinking consumer that one comment stands for a
//! crowd: a closed class, how many viewers, how many distinct viewers, the
//! intensity, the direction and how many recent batches the class has
//! appeared in. It has no free text, no viewer text and no actor identifier on
//! purpose: it is the only crowd value a generation request carries, and the
//! representative comment's own text stays in `input` where it already is.
//!
//! Authority rule: the summary is derived data. It is built by the runtime from
//! its own aggregation, never from a payload field, and it grants nothing: it
//! cannot reach the operator or control plane, a capability, a permit or a gate.
//!
//! Normative wire form: `schemas/crowd-context.schema.json`. It is carried as
//! the optional `crowd` member of the Reflex context (and so of the Thinking
//! request context) and is omitted when absent, so a request without a crowd is
//! byte-identical to one built before this contract existed.

use crate::DomainValidationError;
use serde::{Deserialize, Serialize};

pub const CROWD_CONTEXT_SCHEMA_VERSION: &str = "0.1.0";
/// Most viewers one summary may state. A batch holds at most 64 comments and
/// the rolling window a bounded number of batches; this is a wire ceiling.
pub const MAX_CROWD_MEMBERS: u32 = 100_000;
/// Most recent batches one summary may state.
pub const MAX_CROWD_WINDOW_BATCHES: u32 = 64;

/// Closed crowd vocabulary: the reviewed reaction families plus a repeated
/// question. Adding a class is a reviewed code and schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrowdClass {
    Laughter,
    GoodGame,
    Praise,
    Congratulation,
    Greeting,
    Cheer,
    Surprise,
    Sympathy,
    Thanks,
    RepeatedQuestion,
}

/// Direction of the class between recent batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrowdTrend {
    Rising,
    Steady,
    Falling,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrowdSummary {
    pub schema_version: String,
    pub class: CrowdClass,
    /// Comments the representative stands for, including itself.
    pub member_count: u32,
    /// Distinct viewers among them, as the aggregator counted them.
    pub unique_actors_estimate: u32,
    /// Share of the batch, raised while the class persists; `0..=1000`.
    pub intensity_milli: u16,
    pub trend: CrowdTrend,
    /// Recent batches, within the runtime's rolling window, in which the class
    /// appeared, this one included.
    pub window_batches: u32,
}

impl CrowdSummary {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != CROWD_CONTEXT_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "crowd.schema_version",
                format!("expected {CROWD_CONTEXT_SCHEMA_VERSION}"),
            ));
        }
        if self.member_count < 2 || self.member_count > MAX_CROWD_MEMBERS {
            return Err(DomainValidationError::new(
                "crowd.member_count",
                format!("must be in 2..={MAX_CROWD_MEMBERS}"),
            ));
        }
        if self.unique_actors_estimate == 0 || self.unique_actors_estimate > self.member_count {
            return Err(DomainValidationError::new(
                "crowd.unique_actors_estimate",
                "must be in 1..=member_count",
            ));
        }
        if self.intensity_milli > 1000 {
            return Err(DomainValidationError::new(
                "crowd.intensity_milli",
                "must be in 0..=1000",
            ));
        }
        if self.window_batches == 0 || self.window_batches > MAX_CROWD_WINDOW_BATCHES {
            return Err(DomainValidationError::new(
                "crowd.window_batches",
                format!("must be in 1..={MAX_CROWD_WINDOW_BATCHES}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> CrowdSummary {
        CrowdSummary {
            schema_version: CROWD_CONTEXT_SCHEMA_VERSION.to_owned(),
            class: CrowdClass::Congratulation,
            member_count: 12,
            unique_actors_estimate: 9,
            intensity_milli: 700,
            trend: CrowdTrend::Rising,
            window_batches: 2,
        }
    }

    #[test]
    fn a_well_formed_summary_validates_and_has_no_free_text() {
        let value = serde_json::to_value(summary()).expect("json");
        assert!(summary().validate().is_ok());
        assert!(
            value
                .as_object()
                .expect("object")
                .values()
                .all(|v| !v.is_string() || v == "congratulation" || v == "rising" || v == "0.1.0")
        );
    }

    #[test]
    fn the_schema_fixtures_deserialize_and_validate() {
        for text in [
            include_str!("../../../examples/crowd-context/valid/congratulation-rising.json"),
            include_str!("../../../examples/crowd-context/valid/repeated-question.json"),
        ] {
            let parsed: CrowdSummary = serde_json::from_str(text).expect("valid fixture");
            parsed.validate().expect("valid fixture validates");
        }
        for text in [
            include_str!("../../../examples/crowd-context/invalid/free-text-field.json"),
            include_str!("../../../examples/crowd-context/invalid/actor-id-field.json"),
            include_str!("../../../examples/crowd-context/invalid/class-outside-vocabulary.json"),
            include_str!("../../../examples/crowd-context/invalid/single-member.json"),
            include_str!("../../../examples/crowd-context/invalid/intensity-out-of-range.json"),
            include_str!("../../../examples/crowd-context/invalid/trend-outside-vocabulary.json"),
        ] {
            let outcome = serde_json::from_str::<CrowdSummary>(text)
                .map_err(|e| e.to_string())
                .and_then(|summary| summary.validate().map_err(|e| e.to_string()));
            assert!(outcome.is_err(), "invalid fixture must be refused: {text}");
        }
    }

    #[test]
    fn out_of_range_fields_are_refused() {
        let mut bad = summary();
        bad.unique_actors_estimate = 13;
        assert!(bad.validate().is_err());
        let mut bad = summary();
        bad.member_count = 1;
        assert!(bad.validate().is_err());
        let mut bad = summary();
        bad.intensity_milli = 1001;
        assert!(bad.validate().is_err());
        let mut bad = summary();
        bad.window_batches = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn unknown_members_are_rejected_on_the_wire() {
        let mut value = serde_json::to_value(summary()).expect("json");
        value["text"] = serde_json::json!("hello");
        assert!(serde_json::from_value::<CrowdSummary>(value).is_err());
    }
}
