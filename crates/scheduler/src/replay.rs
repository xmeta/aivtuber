use crate::{
    AppliedVariation, BlendChannel, PlannedPerformance, Priority, Rejection, ScheduledItem,
    Scheduler, SchedulerConfig, SeededRng, Status, VariationSpec, apply_variation,
};
use aivtuber_domain::{EventEnvelope, EventKind, FallbackReason};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayAsset {
    pub asset_id: String,
    pub duration_ms: u64,
    pub interruptible: bool,
    pub interrupt_points_ms: Vec<u64>,
    pub exclusive: bool,
    pub channels: BTreeSet<BlendChannel>,
    pub variation: VariationSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplayDirective {
    Performance { asset: ReplayAsset },
    OperatorStop,
    Fallback { reason: FallbackReason },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayEvent {
    pub at_ms: u64,
    pub event: EventEnvelope,
    pub directive: ReplayDirective,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayOutcome {
    Scheduled,
    Rejected(Rejection),
    OperatorStop,
    Fallback(FallbackReason),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayStep {
    pub at_ms: u64,
    pub event_id: String,
    pub sequence: u64,
    pub outcome: ReplayOutcome,
    pub plan: Option<PlannedPerformance>,
    pub variation: Option<AppliedVariation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkActionKind {
    Start,
    Stop,
    DegradedStart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkAction {
    pub at_ms: u64,
    pub event_id: String,
    pub asset_id: String,
    pub generation: u64,
    pub action: SinkActionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayHarnessConfig {
    pub seed: u64,
    pub scheduler: SchedulerConfig,
    pub audio_available: bool,
    pub avatar_available: bool,
}

impl Default for ReplayHarnessConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            scheduler: SchedulerConfig::default(),
            audio_available: true,
            avatar_available: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayResult {
    pub steps: Vec<ReplayStep>,
    pub scheduled_items: Vec<ScheduledItem>,
    pub audio_actions: Vec<SinkAction>,
    pub avatar_actions: Vec<SinkAction>,
}

impl ReplayResult {
    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ReplayResult contains only JSON-safe values")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayError {
    message: String,
}

impl ReplayError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ReplayError {}

#[derive(Debug, Clone, Copy)]
pub struct ReplayHarness {
    config: ReplayHarnessConfig,
}

impl ReplayHarness {
    pub fn new(config: ReplayHarnessConfig) -> Self {
        Self { config }
    }

    pub fn run(&self, trace: &[ReplayEvent]) -> Result<ReplayResult, ReplayError> {
        let mut ordered = trace.to_vec();
        for input in &ordered {
            input.event.validate().map_err(|error| {
                ReplayError::new(format!(
                    "event {} is invalid: {error}",
                    input.event.event_id
                ))
            })?;
            validate_directive(input)?;
        }

        ordered.sort_by(|left, right| {
            left.at_ms
                .cmp(&right.at_ms)
                .then_with(|| control_rank(left).cmp(&control_rank(right)))
                .then_with(|| left.event.sequence.cmp(&right.event.sequence))
                .then_with(|| left.event.event_id.cmp(&right.event.event_id))
        });

        let mut scheduler = Scheduler::new(self.config.scheduler);
        let mut steps = Vec::with_capacity(ordered.len());

        for input in ordered {
            scheduler.advance_to(input.at_ms);

            match input.directive {
                ReplayDirective::OperatorStop => {
                    scheduler.stop_all(input.at_ms);
                    steps.push(ReplayStep {
                        at_ms: input.at_ms,
                        event_id: input.event.event_id,
                        sequence: input.event.sequence,
                        outcome: ReplayOutcome::OperatorStop,
                        plan: None,
                        variation: None,
                    });
                }
                ReplayDirective::Fallback { reason } => {
                    steps.push(ReplayStep {
                        at_ms: input.at_ms,
                        event_id: input.event.event_id,
                        sequence: input.event.sequence,
                        outcome: ReplayOutcome::Fallback(reason),
                        plan: None,
                        variation: None,
                    });
                }
                ReplayDirective::Performance { asset } => {
                    validate_asset(&asset)?;
                    let mut rng =
                        SeededRng::new(self.config.seed.wrapping_add(input.event.sequence));
                    let variation = apply_variation(asset.variation, &mut rng);
                    let plan = PlannedPerformance {
                        event_id: input.event.event_id.clone(),

                        asset_id: asset.asset_id,
                        priority: priority_for(input.event.kind),
                        interruptible: asset.interruptible,
                        interrupt_points_ms: asset.interrupt_points_ms,
                        start_at_ms: input.at_ms.saturating_add(variation.start_delay_ms),
                        duration_ms: asset.duration_ms,
                        generation: 0,
                        exclusive: asset.exclusive,
                        channels: asset.channels,
                    };

                    match scheduler.schedule_at(input.at_ms, plan) {
                        Ok(plan) => steps.push(ReplayStep {
                            at_ms: input.at_ms,
                            event_id: input.event.event_id,
                            sequence: input.event.sequence,
                            outcome: ReplayOutcome::Scheduled,
                            plan: Some(plan),
                            variation: Some(variation),
                        }),
                        Err(rejection) => steps.push(ReplayStep {
                            at_ms: input.at_ms,
                            event_id: input.event.event_id,
                            sequence: input.event.sequence,
                            outcome: ReplayOutcome::Rejected(rejection),
                            plan: None,
                            variation: Some(variation),
                        }),
                    }
                }
            }
        }

        scheduler.complete_all();

        let scheduled_items = scheduler.items().to_vec();
        let audio_actions = build_sink_actions(
            &scheduled_items,
            self.config.audio_available,
            SinkTarget::Audio,
        );
        let avatar_actions = build_sink_actions(
            &scheduled_items,
            self.config.avatar_available,
            SinkTarget::Avatar,
        );

        Ok(ReplayResult {
            steps,
            scheduled_items,
            audio_actions,
            avatar_actions,
        })
    }
}

fn validate_directive(input: &ReplayEvent) -> Result<(), ReplayError> {
    match (&input.event.kind, &input.directive) {
        (EventKind::OperatorCommand, ReplayDirective::OperatorStop) => Ok(()),
        (EventKind::OperatorCommand, _) => Err(ReplayError::new(format!(
            "operator event {} requires operator_stop directive",
            input.event.event_id
        ))),
        (_, ReplayDirective::OperatorStop) => Err(ReplayError::new(format!(
            "non-operator event {} cannot use operator_stop directive",
            input.event.event_id
        ))),
        _ => Ok(()),
    }
}

fn validate_asset(asset: &ReplayAsset) -> Result<(), ReplayError> {
    if asset.asset_id.trim().is_empty() {
        return Err(ReplayError::new("replay asset id must not be empty"));
    }
    for (field, value) in [
        ("variation.speed_pct", asset.variation.speed_pct),
        ("variation.amplitude_pct", asset.variation.amplitude_pct),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(ReplayError::new(format!(
                "{field} must be finite and non-negative"
            )));
        }
    }
    Ok(())
}

fn priority_for(kind: EventKind) -> Priority {
    match kind {
        EventKind::OperatorCommand => Priority::Operator,
        EventKind::ChatDonation => Priority::HighPriorityInteraction,
        EventKind::GameEvent => Priority::StrongReaction,
        EventKind::ChatMessage | EventKind::SpeechInput => Priority::Conversation,
        EventKind::StreamEvent => Priority::Commentary,
        EventKind::TimerTick | EventKind::SystemHealth => Priority::Background,
    }
}

fn control_rank(input: &ReplayEvent) -> u8 {
    if input.event.kind == EventKind::OperatorCommand {
        0
    } else {
        1
    }
}

#[derive(Debug, Clone, Copy)]
enum SinkTarget {
    Audio,
    Avatar,
}

fn relevant_to_sink(item: &ScheduledItem, target: SinkTarget) -> bool {
    match target {
        SinkTarget::Audio => item.plan.channels.contains(&BlendChannel::Audio),
        SinkTarget::Avatar => item.plan.channels.iter().any(|channel| {
            matches!(
                channel,
                BlendChannel::Face | BlendChannel::Body | BlendChannel::Gaze
            )
        }),
    }
}

fn build_sink_actions(
    items: &[ScheduledItem],
    available: bool,
    target: SinkTarget,
) -> Vec<SinkAction> {
    let mut actions = Vec::new();

    for item in items {
        if !relevant_to_sink(item, target) {
            continue;
        }
        let cancelled_before_start = item
            .cancel_at_ms
            .is_some_and(|cancel| cancel < item.plan.start_at_ms);
        if cancelled_before_start {
            continue;
        }

        actions.push(SinkAction {
            at_ms: item.plan.start_at_ms,
            event_id: item.plan.event_id.clone(),
            asset_id: item.plan.asset_id.clone(),
            generation: item.plan.generation,
            action: if available {
                SinkActionKind::Start
            } else {
                SinkActionKind::DegradedStart
            },
        });

        if available
            && item.status == Status::Cancelled
            && let Some(cancel_at_ms) = item.cancel_at_ms
            && cancel_at_ms >= item.plan.start_at_ms
        {
            actions.push(SinkAction {
                at_ms: cancel_at_ms,
                event_id: item.plan.event_id.clone(),
                asset_id: item.plan.asset_id.clone(),
                generation: item.plan.generation,
                action: SinkActionKind::Stop,
            });
        }
    }

    actions.sort_by_key(|action| {
        (
            action.at_ms,
            action.generation,
            sink_action_rank(action.action),
        )
    });
    actions
}

fn sink_action_rank(action: SinkActionKind) -> u8 {
    match action {
        SinkActionKind::Start | SinkActionKind::DegradedStart => 0,
        SinkActionKind::Stop => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        AuthorizationContext, AuthorizationMethod, Capability, EVENT_SCHEMA_VERSION, SecurityPlane,
        SourceClass, TrustLevel,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn channels(values: &[BlendChannel]) -> BTreeSet<BlendChannel> {
        values.iter().copied().collect()
    }

    fn event(kind: EventKind, event_id: &str, sequence: u64) -> EventEnvelope {
        let (source, source_class, plane, trust_level, authorization) = match kind {
            EventKind::ChatMessage => (
                "chat",
                SourceClass::PublicChat,
                SecurityPlane::Content,
                TrustLevel::Untrusted,
                None,
            ),
            EventKind::ChatDonation => (
                "donation",
                SourceClass::Donation,
                SecurityPlane::Content,
                TrustLevel::SemiTrusted,
                None,
            ),

            EventKind::GameEvent => (
                "game",
                SourceClass::Game,
                SecurityPlane::Content,
                TrustLevel::SemiTrusted,
                None,
            ),
            EventKind::OperatorCommand => (
                "local-operator-hotkey",
                SourceClass::Operator,
                SecurityPlane::Control,
                TrustLevel::Trusted,
                Some(AuthorizationContext {
                    principal: "operator:local".to_owned(),
                    method: AuthorizationMethod::OperatorHotkey,
                    capabilities: BTreeSet::from([Capability::PerformerStop]),
                }),
            ),
            _ => panic!("test helper does not cover {kind:?}"),
        };

        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: event_id.to_owned(),
            correlation_id: "corr-replay".to_owned(),
            sequence,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: source.to_owned(),
            source_class,
            plane,
            trust_level,
            kind,

            actor_id: None,
            priority_hint: None,
            authorization,
            payload: BTreeMap::new(),
        }
    }

    fn asset(id: &str, duration_ms: u64) -> ReplayAsset {
        ReplayAsset {
            asset_id: id.to_owned(),
            duration_ms,
            interruptible: true,
            interrupt_points_ms: vec![0, duration_ms],
            exclusive: false,
            channels: channels(&[BlendChannel::Audio, BlendChannel::Face, BlendChannel::Body]),
            variation: VariationSpec::default(),
        }
    }

    fn harness(seed: u64) -> ReplayHarness {
        ReplayHarness::new(ReplayHarnessConfig {
            seed,
            scheduler: SchedulerConfig {
                min_reaction_spacing_ms: 0,
            },
            audio_available: true,
            avatar_available: true,
        })
    }

    #[test]
    fn same_trace_and_seed_are_byte_equivalent_with_mixed_priorities() {
        let trace = vec![
            ReplayEvent {
                at_ms: 0,
                event: event(EventKind::ChatMessage, "chat-1", 1),
                directive: ReplayDirective::Performance {
                    asset: asset("chat.asset", 600),
                },
            },
            ReplayEvent {
                at_ms: 0,
                event: event(EventKind::GameEvent, "game-2", 2),
                directive: ReplayDirective::Performance {
                    asset: asset("game.asset", 600),
                },
            },
            ReplayEvent {
                at_ms: 0,
                event: event(EventKind::ChatDonation, "donation-3", 3),
                directive: ReplayDirective::Performance {
                    asset: asset("paid.asset", 600),
                },
            },
        ];

        let first = harness(4242).run(&trace).expect("first replay");
        let second = harness(4242).run(&trace).expect("second replay");
        assert_eq!(first.to_json_bytes(), second.to_json_bytes());

        assert_eq!(
            first
                .steps
                .iter()
                .map(|step| step.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["chat-1", "game-2", "donation-3"]
        );
        assert!(
            first
                .steps
                .iter()
                .all(|step| step.outcome == ReplayOutcome::Scheduled)
        );
        assert_eq!(first.scheduled_items.len(), 3);
        assert_eq!(first.scheduled_items[0].status, Status::Cancelled);
        assert_eq!(first.scheduled_items[1].status, Status::Cancelled);
        assert_eq!(first.scheduled_items[2].status, Status::Completed);
    }

    #[test]
    fn later_operator_preserves_causality_and_stops_at_arrival_time() {
        let trace = vec![
            ReplayEvent {
                at_ms: 0,
                event: event(EventKind::ChatMessage, "chat-1", 1),
                directive: ReplayDirective::Performance {
                    asset: asset("chat.asset", 1000),
                },
            },
            ReplayEvent {
                at_ms: 500,
                event: event(EventKind::OperatorCommand, "operator-2", 2),
                directive: ReplayDirective::OperatorStop,
            },
        ];

        let result = harness(7).run(&trace).expect("replay");
        assert_eq!(
            result
                .steps
                .iter()
                .map(|step| step.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["chat-1", "operator-2"]
        );
        assert_eq!(result.scheduled_items[0].cancel_at_ms, Some(500));
        assert_eq!(
            result
                .audio_actions
                .iter()
                .map(|action| (action.at_ms, action.action))
                .collect::<Vec<_>>(),
            vec![(0, SinkActionKind::Start), (500, SinkActionKind::Stop)]
        );
    }

    #[test]
    fn same_timestamp_operator_runs_before_content_without_time_travel() {
        let trace = vec![
            ReplayEvent {
                at_ms: 100,
                event: event(EventKind::ChatMessage, "chat-1", 1),
                directive: ReplayDirective::Performance {
                    asset: asset("chat.asset", 300),
                },
            },
            ReplayEvent {
                at_ms: 100,
                event: event(EventKind::OperatorCommand, "operator-2", 2),
                directive: ReplayDirective::OperatorStop,
            },
        ];

        let result = harness(1).run(&trace).expect("replay");
        assert_eq!(
            result
                .steps
                .iter()
                .map(|step| step.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["operator-2", "chat-1"]
        );
        assert_eq!(result.steps[0].at_ms, 100);
        assert_eq!(result.steps[1].at_ms, 100);
        assert_eq!(result.scheduled_items[0].plan.start_at_ms, 100);
    }

    #[test]
    fn timeout_fallback_does_not_block_later_scheduling() {
        let trace = vec![
            ReplayEvent {
                at_ms: 0,
                event: event(EventKind::ChatMessage, "timeout-1", 1),
                directive: ReplayDirective::Fallback {
                    reason: FallbackReason::Timeout,
                },
            },
            ReplayEvent {
                at_ms: 10,
                event: event(EventKind::ChatMessage, "chat-2", 2),
                directive: ReplayDirective::Performance {
                    asset: asset("chat.asset", 300),
                },
            },
        ];

        let result = harness(9).run(&trace).expect("replay");
        assert_eq!(
            result.steps[0].outcome,
            ReplayOutcome::Fallback(FallbackReason::Timeout)
        );
        assert_eq!(result.steps[1].outcome, ReplayOutcome::Scheduled);
        assert_eq!(result.scheduled_items.len(), 1);
        assert_eq!(result.scheduled_items[0].status, Status::Completed);
    }

    #[test]
    fn unavailable_mock_sinks_degrade_without_blocking() {
        let config = ReplayHarnessConfig {
            seed: 3,
            scheduler: SchedulerConfig {
                min_reaction_spacing_ms: 0,
            },
            audio_available: false,
            avatar_available: false,
        };

        let trace = vec![ReplayEvent {
            at_ms: 0,
            event: event(EventKind::ChatMessage, "chat-1", 1),
            directive: ReplayDirective::Performance {
                asset: asset("chat.asset", 300),
            },
        }];

        let result = ReplayHarness::new(config).run(&trace).expect("replay");
        assert_eq!(result.steps[0].outcome, ReplayOutcome::Scheduled);
        assert_eq!(result.audio_actions.len(), 1);
        assert_eq!(result.avatar_actions.len(), 1);
        assert_eq!(
            result.audio_actions[0].action,
            SinkActionKind::DegradedStart
        );
        assert_eq!(
            result.avatar_actions[0].action,
            SinkActionKind::DegradedStart
        );
    }
}
