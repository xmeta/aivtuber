//! The host side of the idle initiative (issue #87 S1): how the composition
//! root feeds the [`InitiativeDirector`] trusted state, starts the one
//! generation it asks for, and cancels it when chat, speech or the operator
//! takes over. The decision itself lives in [`crate::initiative`].
//!
//! An initiative is an ordinary `Background` performer item: the trusted
//! timer event it is dispatched as ranks below every chat and speech reply in
//! the scheduler and the generation cancellation registry, so the existing
//! paths preempt it. It never reads viewer text.

use std::time::{Duration, Instant};

use aivtuber_domain::EventKind;
use aivtuber_generative::GenerationRoutingReason;
use aivtuber_scheduler::{PhraseLink, Priority, Status};
use aivtuber_telemetry::AuditCategory;
use serde::{Deserialize, Serialize};

use super::*;
use crate::initiative::{
    self, Decision, InitiativeDirector, InitiativeInputs, InitiativeMetrics, Preemption,
};
use crate::topics::{TopicsEditor, TopicsOutcome, TopicsStatus};

/// A pending initiative that has not ended after this long is forced to end:
/// the provider deadline plus a generous playback allowance. A bound, not a
/// timer anyone should hit.
const PENDING_BOUND_MS: u64 = 120_000;
/// Fade when an initiative's audio is stopped for chat or speech.
const PREEMPT_FADE_MS: u32 = 80;

/// The operator-visible initiative state: text-free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeStatus {
    pub enabled: bool,
    pub idle_secs: u64,
    pub pending: bool,
    pub fired: u64,
    pub completed: u64,
    pub failed: u64,
    pub preempted_chat: u64,
    pub preempted_speech: u64,
    pub preempted_stop: u64,
    pub suppressed_total: u64,
    /// `(reason, count)` for every reason that ever suppressed one.
    pub suppressed: Vec<(String, u64)>,
    /// The operator topic list (#87 S2): counts only, never text. Absent
    /// unless live topic editing is composed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topics: Option<TopicsStatus>,
}

impl InitiativeStatus {
    pub(crate) fn from_director(
        director: &InitiativeDirector,
        editor: Option<&TopicsEditor>,
    ) -> Self {
        let metrics: InitiativeMetrics = director.metrics();
        Self {
            enabled: director.enabled(),
            idle_secs: director.config().idle_ms / 1_000,
            pending: director.pending_event_id().is_some(),
            fired: metrics.fired,
            completed: metrics.completed,
            failed: metrics.failed,
            preempted_chat: metrics.preempted_chat,
            preempted_speech: metrics.preempted_speech,
            preempted_stop: metrics.preempted_stop,
            suppressed_total: metrics.suppressed_total(),
            suppressed: initiative::Suppression::ALL
                .into_iter()
                .filter_map(|reason| {
                    let count = metrics.suppressed_by(reason);
                    (count > 0).then(|| (reason.name().to_owned(), count))
                })
                .collect(),
            topics: editor.map(|editor| TopicsStatus {
                count: director.topic_count(),
                persisted: editor.path().is_some(),
                thinking_remote: editor.thinking_remote(),
                used: metrics.topics_used,
                without_topic: metrics.without_topic,
            }),
        }
    }
}

impl<R> ProductionApp<R>
where
    R: RoutePlanner,
{
    /// Opt in to the idle initiative. `None` (default) composes nothing: the
    /// performer stays silent when nobody writes, exactly as before.
    pub fn with_initiative(mut self, config: initiative::InitiativeConfig) -> Self {
        self.initiative = Some(InitiativeDirector::new(config, 0));
        self
    }

    pub fn initiative_composed(&self) -> bool {
        self.initiative.is_some()
    }

    pub fn initiative_status(&self) -> Option<InitiativeStatus> {
        self.initiative
            .as_ref()
            .map(|director| InitiativeStatus::from_director(director, self.topics_editor.as_ref()))
    }

    /// Opt in to the operator topic list (#87 S2): `list` is what the file
    /// held at start, `editor` says where a live save goes. Only meaningful
    /// next to `with_initiative`; without it nothing is composed.
    pub fn with_topics(mut self, editor: TopicsEditor, list: &aivtuber_domain::TopicList) -> Self {
        if let Some(director) = self.initiative.as_mut() {
            director.set_topics(list.entries());
            self.topics_editor = Some(editor);
        }
        self
    }

    /// Whether the topic list can be edited while running (the operator
    /// endpoint then holds `persona.configure` for `topics.*`).
    pub fn topics_editor_composed(&self) -> bool {
        self.topics_editor.is_some()
    }

    /// Apply one topic-list operator request (#87 S2). `authority` must be the
    /// authenticated local ingress capability (`persona.configure`, the same
    /// closed capability as the character sheet: both are operator-written
    /// text the Thinking request carries as `operator_curated` data).
    /// `action` is `topics.set` (`argument` is the whole new list, one topic
    /// per line; empty clears) or `topics.get`.
    ///
    /// A set is validated by the same kind of parser as the start-up file,
    /// written atomically when a file is configured, and only then becomes the
    /// live list: any refusal, or a failed write, leaves the live list and the
    /// file unchanged. An initiative already being generated keeps its topic.
    /// Details are fixed phrases plus the count, never the text; only
    /// `topics.get` returns it, to the authenticated caller.
    pub fn configure_topics(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        argument: Option<&str>,
    ) -> Result<TopicsOutcome, String> {
        if !authority.has_capability(aivtuber_domain::Capability::PersonaConfigure) {
            return Err("topic list editing is not authorized".to_owned());
        }
        let (Some(editor), Some(director)) =
            (self.topics_editor.as_ref(), self.initiative.as_ref())
        else {
            return Err("topic list editing is not configured".to_owned());
        };
        match action {
            "topics.get" => Ok(TopicsOutcome {
                detail: format!("topics: {}", director.topic_count()),
                text: Some(director.topic_text()),
            }),
            "topics.set" => {
                let Some(text) = argument else {
                    return Err("topics not changed: no text given".to_owned());
                };
                let list = aivtuber_domain::parse_topics(text.as_bytes())
                    .map_err(|error| format!("topics not changed: {}", error.reason()))?;
                let persisted = match editor.path() {
                    Some(path) => {
                        if crate::persona::write_persona_file_atomic(path, &list.to_text()).is_err()
                        {
                            return Err("topics not changed: the topics file could not be written"
                                .to_owned());
                        }
                        true
                    }
                    None => false,
                };
                let count = list.len();
                if let Some(director) = self.initiative.as_mut() {
                    director.set_topics(list.entries());
                }
                let kept = if persisted {
                    "saved to the topics file"
                } else {
                    "applied in memory only (no topics file is configured)"
                };
                Ok(TopicsOutcome {
                    detail: format!("topics updated, {kept}: {count}"),
                    text: None,
                })
            }
            _ => Err("not a topic list action".to_owned()),
        }
    }

    pub fn initiative_director(&self) -> Option<&InitiativeDirector> {
        self.initiative.as_ref()
    }

    /// The operator toggle (`initiative.enable` / `initiative.disable`).
    /// `authority` must be the authenticated local ingress capability. Turning
    /// it off also cancels a pending initiative.
    pub fn configure_initiative(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        at_ms: u64,
    ) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::InitiativeControl) {
            return Err("initiative control is not authorized".to_owned());
        }
        if self.initiative.is_none() {
            return Err("initiative is not configured".to_owned());
        }
        match action {
            "initiative.enable" => {
                if let Some(director) = self.initiative.as_mut() {
                    director.set_enabled(true);
                }
                Ok("initiative on".to_owned())
            }
            "initiative.disable" => {
                self.preempt_initiative(at_ms, Preemption::OperatorStop);
                if let Some(director) = self.initiative.as_mut() {
                    director.set_enabled(false);
                }
                Ok("initiative off".to_owned())
            }
            _ => Err("not an initiative action".to_owned()),
        }
    }

    /// Loop hook for the daemon: close out a finished initiative, then let the
    /// director decide whether the performer should speak now. A no-op unless
    /// composed. Call it after the content services and before `tick`, so
    /// chat that arrived this turn has already restarted the idle clock.
    pub fn service_initiative(&mut self, now_ms: u64, seed: u64) {
        if self.initiative.is_none() {
            return;
        }
        self.settle_initiative(now_ms);
        let inputs = self.initiative_inputs(now_ms);
        // Something took the floor while an initiative was still being made:
        // it yields. (Playing audio is already stopped by barge-in in full
        // duplex; this also covers the generation and the gaps between
        // phrases.)
        if self.initiative_pending() {
            if inputs.muted {
                self.preempt_initiative(now_ms, Preemption::OperatorStop);
            } else if inputs.floor_held {
                self.preempt_initiative(now_ms, Preemption::Speech);
            }
        }
        let decision = match self.initiative.as_mut() {
            Some(director) => director.evaluate(&inputs),
            None => return,
        };
        if let Decision::Fire {
            event_id,
            kind,
            topic,
        } = decision
        {
            self.start_initiative(&event_id, kind, topic, now_ms, seed);
        }
    }

    fn initiative_pending(&self) -> bool {
        self.initiative
            .as_ref()
            .is_some_and(|director| director.pending_event_id().is_some())
    }

    fn initiative_inputs(&self, now_ms: u64) -> InitiativeInputs {
        let speaking = self.performer.scheduler().items().iter().any(|item| {
            matches!(item.status, Status::Queued | Status::Playing)
                && item
                    .plan
                    .channels
                    .contains(&aivtuber_scheduler::BlendChannel::Audio)
        }) || self.performer.scheduler().speech_lifecycle().metrics().live > 0;
        InitiativeInputs {
            now_ms,
            muted: self.security.is_muted(),
            floor_held: self.floor_holds_reply(now_ms),
            speaking,
            generating: self
                .generative
                .as_ref()
                .is_some_and(GenerativeRuntime::is_generating),
            content_queued: self.security.content_len() > 0,
            deferred_pending: self
                .chat_burst
                .as_ref()
                .is_some_and(|lane| lane.pending_deferred() > 0),
        }
    }

    /// Whether the generation or any of its speech is still in flight.
    fn initiative_in_flight(&self, event_id: &str) -> bool {
        let generating = self.generative.as_ref().is_some_and(|generative| {
            generative.cancellation.active_event_id().as_deref() == Some(event_id)
        });
        generating
            || self.performer.scheduler().items().iter().any(|item| {
                matches!(item.status, Status::Queued | Status::Playing)
                    && PhraseLink::root_event_id(&item.plan.event_id) == event_id
            })
    }

    /// End the pending initiative once nothing of it is left in flight.
    fn settle_initiative(&mut self, now_ms: u64) {
        let Some((id, since)) = self.initiative.as_ref().and_then(|director| {
            Some((
                director.pending_event_id()?.to_owned(),
                director.pending_since_ms()?,
            ))
        }) else {
            return;
        };
        let overdue = now_ms.saturating_sub(since) >= PENDING_BOUND_MS;
        if self.initiative_in_flight(&id) && !overdue {
            return;
        }
        // Phrases staged for the refusal check but never heard are forgotten.
        self.repeat_guard.clear_staged();
        let spoke = self
            .recent_replies
            .items()
            .find(|reply| reply.event_id == id)
            .map(|reply| reply.text.clone());
        let fell = std::mem::take(&mut self.initiative_fell);
        let refused = self.initiative_refused_text.take();
        match (spoke, fell) {
            (Some(text), _) => {
                if let Some(director) = self.initiative.as_mut() {
                    director.completed(now_ms, Some(&text));
                }
            }
            (None, true) => {
                // The gesture was played where the reply fell back.
                self.initiative_ended_silent(now_ms, refused.as_deref());
            }
            (None, false) => {
                // Nothing was spoken and nothing said why (stale completion,
                // a refused phrase, an output that never played): the same
                // visual fallback, once.
                self.play_idle_gesture(now_ms);
                self.initiative_ended_silent(now_ms, refused.as_deref());
            }
        }
    }

    /// The initiative ended with nothing spoken. A repeat refusal (#541) is
    /// told apart from a failure: it alone may grant the one early re-fire.
    fn initiative_ended_silent(&mut self, now_ms: u64, refused_text: Option<&str>) {
        let Some(director) = self.initiative.as_mut() else {
            return;
        };
        match refused_text {
            Some(text) => director.repeat_refused(now_ms, Some(text)),
            None => director.failed(now_ms),
        }
    }

    /// Build the one typed request and hand it to the generative path. Any
    /// failure falls back to the idle gesture; nothing is retried (the budget
    /// was charged when the director fired).
    fn start_initiative(
        &mut self,
        event_id: &str,
        kind: aivtuber_domain::InitiativeKind,
        topic: Option<initiative::TopicPick>,
        at_ms: u64,
        seed: u64,
    ) {
        self.initiative_sequence = self.initiative_sequence.saturating_add(1);
        let event = initiative::mint_event(event_id, kind, at_ms, self.initiative_sequence);
        let recent: Vec<String> = self
            .initiative
            .as_ref()
            .map(|director| director.recent_texts().map(str::to_owned).collect())
            .unwrap_or_default();
        let topic_text = topic.as_ref().map(|pick| pick.text.as_str());
        let Ok(thinking) = initiative::build_request(&event, kind, topic_text, &recent) else {
            self.initiative_unavailable(at_ms);
            return;
        };
        let mut route = PlaybackRoute::Generate(Box::new(GenerationRoute {
            source_event: event.clone(),
            thinking,
            routing_reason: GenerationRoutingReason::ExplicitLlmRoute,
            intent: format!("initiative.{}", kind.name()),
            style: None,
            fallback_variant_group: None,
        }));
        // The operator's character sheet, as for every reply.
        self.attach_persona(&mut route);
        let PlaybackRoute::Generate(route) = route else {
            self.initiative_unavailable(at_ms);
            return;
        };
        if self.generative.is_none() {
            self.initiative_unavailable(at_ms);
            return;
        }
        let Ok(Some(deadline)) = self
            .interaction_deadlines
            .deadline_for(EventKind::TimerTick, at_ms)
        else {
            self.initiative_unavailable(at_ms);
            return;
        };
        let Some(provider_deadline) =
            Instant::now().checked_add(Duration::from_millis(deadline.total_budget_ms()))
        else {
            self.initiative_unavailable(at_ms);
            return;
        };
        let context = GenerationCompletionContext {
            budget_reservation: None,
            routing_latency_us: 0,
            decision: None,
            template_fallback: None,
            interaction_deadline: deadline,
            remaining_at_route_ms: deadline.remaining_ms(at_ms),
            remaining_at_generation_ms: remaining_instant(provider_deadline)
                .map(|remaining| remaining.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0),
            correlation_id: event.correlation_id.clone(),
        };
        let record = self.security.redactor_record(
            Some(event_id),
            AuditCategory::Generation,
            "initiative_fired",
            // The topic is the operator's text: the audit names its position.
            match &topic {
                Some(pick) => format!("kind={} topic=#{}", kind.name(), pick.index + 1),
                None => format!("kind={} topic=none", kind.name()),
            },
        );
        self.security.push_external_audit(record);
        self.session_log_initiative(event_id, kind.name(), at_ms);
        match self.submit_generation(event, *route, at_ms, seed, provider_deadline, context) {
            Ok(GenerationSubmission::Deferred) => {}
            Ok(GenerationSubmission::Immediate { handled, .. }) => {
                // Refused before any provider work (budget, saturation,
                // deadline): the fallback already ran and played the gesture.
                let HandledGeneration {
                    playback,
                    route,
                    trace,
                    cache_level,
                } = *handled;
                let handled = HandledEvent {
                    playback,
                    route,
                    routing_latency_us: 0,
                    generation_latency_us: None,
                    generation_trace: Some(trace),
                    decision: None,
                    template_fallback: None,
                    cache_level,
                    deadline: None,
                    deferred_generation: false,
                    admitted_bytes: None,
                    correlation_id: None,
                };
                self.record_handled_event(event_id, &handled, at_ms);
            }
            Err(_) => self.initiative_unavailable(at_ms),
        }
    }

    /// Generation is not available: the visual fallback, counted as failed.
    fn initiative_unavailable(&mut self, at_ms: u64) {
        self.play_idle_gesture(at_ms);
        if let Some(director) = self.initiative.as_mut() {
            director.failed(at_ms);
        }
    }

    /// The code-owned idle gesture, executed directly on the avatar output
    /// like the deferred ack: no scheduler item, so it can neither delay a
    /// reply nor move `last_terminal_at`. Skipped while muted; a refusal is
    /// not an output-health error.
    pub(crate) fn play_idle_gesture(&mut self, at_ms: u64) {
        if self.security.is_muted() {
            return;
        }
        let _ = self.avatar.execute(&AvatarPlaybackCommand::Gesture {
            generation: 0,
            at_ms,
            preset: initiative::IDLE_GESTURE_PRESET.to_owned(),
            amplitude: initiative::IDLE_GESTURE_AMPLITUDE,
        });
    }

    /// Whether `event` is the pending initiative's own event.
    pub(crate) fn is_initiative_event(&self, event: &EventEnvelope) -> bool {
        event.kind == EventKind::TimerTick
            && event.event_id.starts_with(initiative::EVENT_ID_PREFIX)
            && self
                .initiative
                .as_ref()
                .is_some_and(|director| director.pending_event_id() == Some(&event.event_id))
    }

    /// Called where a generated initiative reply could not be used: the idle
    /// gesture replaces speech and the end is remembered for [`Self::settle_initiative`].
    pub(crate) fn initiative_fell_back(&mut self, at_ms: u64) {
        self.play_idle_gesture(at_ms);
        self.initiative_fell = true;
    }

    /// Chat, speech or the operator took over: drop the pending initiative
    /// through the existing paths. Its generation is cancelled and released
    /// (so a chat reply is not held behind it), and any of its speech that was
    /// queued or playing is stopped like a barge-in would. A no-op unless an
    /// initiative is pending, so it never touches anything else.
    pub(crate) fn preempt_initiative(&mut self, at_ms: u64, cause: Preemption) {
        let Some(id) = self
            .initiative
            .as_ref()
            .and_then(|director| director.pending_event_id().map(str::to_owned))
        else {
            return;
        };
        if let Some(generative) = self.generative.as_mut() {
            generative.abandon_active(&id);
        }
        let stop = self
            .performer
            .scheduler_mut()
            .stop_speaking_for(Priority::Background, at_ms);
        if !stop.is_empty() {
            self.purge_cancelled_pending();
            let _ = self.stop_started_audio(PREEMPT_FADE_MS);
            if let Some(generation) = stop.playing.first().map(|plan| plan.generation) {
                let _ = self.reset_avatar_neutral(generation, at_ms);
            }
        }
        self.initiative_fell = false;
        self.initiative_refused_text = None;
        self.repeat_guard.clear_staged();
        if let Some(director) = self.initiative.as_mut() {
            director.preempted(at_ms, cause);
        }
        let record = self.security.redactor_record(
            Some(&id),
            AuditCategory::Generation,
            "initiative_preempted",
            format!("cause={}", cause.name()),
        );
        self.security.push_external_audit(record);
    }

    /// Chat or speech was admitted: restart the idle clock and yield.
    pub(crate) fn note_content_arrival(&mut self, kind: EventKind, at_ms: u64) {
        let Some(director) = self.initiative.as_mut() else {
            return;
        };
        director.note_chat(at_ms);
        let cause = if kind == EventKind::SpeechInput {
            Preemption::Speech
        } else {
            Preemption::Chat
        };
        self.preempt_initiative(at_ms, cause);
    }

    /// The operator stopped or muted the performer.
    pub(crate) fn note_operator_override(&mut self, at_ms: u64) {
        if let Some(director) = self.initiative.as_mut() {
            director.note_operator_stop(at_ms);
        }
        self.preempt_initiative(at_ms, Preemption::OperatorStop);
    }
}
