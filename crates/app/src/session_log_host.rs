//! The host side of the opt-in session conversation log (issue #463): how the
//! composition root feeds [`SessionLog`] and how the operator switch reaches
//! it. Everything here is a no-op unless the log is composed *and* the
//! operator turned it on, and none of it can fail a reply: the log only
//! formats a bounded line and queues it for its own writer thread.

use aivtuber_domain::EventKind;
use aivtuber_telemetry::RouteClass;

use super::*;
use crate::session_log::{SessionLog, SessionLogStatus};

impl<R> ProductionApp<R>
where
    R: RoutePlanner,
{
    /// Compose the session log. Without this call nothing is ever recorded and
    /// the operator endpoint does not hold `session_log.control`.
    pub fn with_session_log(mut self, log: SessionLog) -> Self {
        self.session_log = Some(log);
        self
    }

    pub fn session_log_composed(&self) -> bool {
        self.session_log.is_some()
    }

    pub fn session_log(&self) -> Option<&SessionLog> {
        self.session_log.as_ref()
    }

    /// Counters and location (the operator status). `None` unless composed.
    pub fn session_log_status(&self) -> Option<SessionLogStatus> {
        self.session_log.as_ref().map(SessionLog::status)
    }

    /// Counters only, for the diagnose bundle: no location, no text.
    pub fn session_log_counters(&self) -> Option<SessionLogStatus> {
        self.session_log_status().map(|mut status| {
            status.location = None;
            status
        })
    }

    /// The operator switch (`session_log.enable`, `session_log.disable`,
    /// `session_log.delete`). `authority` must be the authenticated local
    /// ingress capability. Details are fixed phrases, never a path.
    pub fn configure_session_log(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
    ) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::SessionLogControl) {
            return Err("session log control is not authorized".to_owned());
        }
        let Some(log) = self.session_log.as_mut() else {
            return Err("session log is not configured".to_owned());
        };
        match action {
            "session_log.enable" => match log.set_enabled(true) {
                Ok(()) => Ok("session log on".to_owned()),
                Err(why) => Ok(format!("session log on ({why})")),
            },
            "session_log.disable" => match log.set_enabled(false) {
                Ok(()) => Ok("session log off".to_owned()),
                Err(why) => Ok(format!("session log off ({why})")),
            },
            "session_log.delete" => {
                if log.delete_all() {
                    Ok("session logs deleted".to_owned())
                } else {
                    Err("session logs could not be deleted right now".to_owned())
                }
            }
            _ => Err("not a session log action".to_owned()),
        }
    }

    fn session_log_on(&self) -> bool {
        self.session_log.as_ref().is_some_and(SessionLog::enabled)
    }

    /// An admitted chat line or transcript opens a turn.
    pub(crate) fn session_log_input(&mut self, event: &EventEnvelope, at_ms: u64) {
        if !self.session_log_on() {
            return;
        }
        let origin = match event.kind {
            EventKind::SpeechInput => "voice",
            _ if event.event_id.starts_with("panel-chat-") => "operator",
            _ => "chat",
        };
        let text = event.payload.get("text").and_then(|value| value.as_str());
        let persona = self.persona_status().and_then(|status| status.sha256);
        let seed = self
            .memory_seed_editor_status()
            .map(|status| status.content_hash);
        let redactor = self.security.redactor();
        if let Some(log) = self.session_log.as_mut() {
            log.note_hashes(persona.as_deref(), seed.as_deref());
            log.input(
                redactor,
                &event.event_id,
                origin,
                event.actor_id.as_deref(),
                text,
                None,
                at_ms,
            );
        }
    }

    /// An idle initiative opens a turn (no text, no viewer).
    pub(crate) fn session_log_initiative(&mut self, event_id: &str, kind: &str, at_ms: u64) {
        if !self.session_log_on() {
            return;
        }
        let redactor = self.security.redactor();
        if let Some(log) = self.session_log.as_mut() {
            log.input(
                redactor,
                event_id,
                "initiative",
                None,
                None,
                Some(kind),
                at_ms,
            );
        }
    }

    /// One phrase the output accepted, with the #89 verdict.
    pub(crate) fn session_log_phrase(
        &mut self,
        event_id: &str,
        generated: bool,
        text: &str,
        repeat: repeat_guard::Repeat,
        at_ms: u64,
    ) {
        let redactor = self.security.redactor();
        if let Some(log) = self.session_log.as_mut() {
            let verdict = match repeat {
                repeat_guard::Repeat::None => "none",
                repeat_guard::Repeat::Exact => "exact",
                repeat_guard::Repeat::Near => "near",
            };
            log.phrase(redactor, event_id, generated, text, verdict, at_ms);
        }
    }

    /// How a turn was answered and why it fell back, when it did.
    pub(crate) fn session_log_route(
        &mut self,
        event_id: &str,
        route: RouteClass,
        cache: Option<CacheLevel>,
        fallback: Option<&str>,
        at_ms: u64,
    ) {
        let Some(log) = self.session_log.as_mut() else {
            return;
        };
        let route = match route {
            RouteClass::Silent => "silent",
            RouteClass::Deterministic => "deterministic",
            RouteClass::SemanticReuse => "semantic_reuse",
            RouteClass::JevReaction => "jev_reaction",
            RouteClass::Generated => "generated",
            RouteClass::CachedFallback => "cached_fallback",
            RouteClass::NonVerbalFallback => "non_verbal_fallback",
        };
        let cache = cache.map(|level| match level {
            CacheLevel::Memory => "memory",
            CacheLevel::LocalStorage => "local_storage",
            CacheLevel::Generated => "generated",
        });
        log.route(event_id, route, cache, fallback, at_ms);
    }

    /// A turn answered by a typed fallback (#469).
    pub(crate) fn session_log_fallback(
        &mut self,
        event_id: &str,
        reason: &str,
        visible: bool,
        at_ms: u64,
    ) {
        if let Some(log) = self.session_log.as_mut() {
            log.fallback(event_id, reason, visible, at_ms);
        }
    }

    /// Write the crowd routes the lane produced since the last call (#79).
    pub(crate) fn session_log_crowd_routes(&mut self, at_ms: u64) {
        let (notes, ends) = self
            .chat_burst
            .as_mut()
            .map(|lane| (lane.take_crowd_notes(), lane.take_crowd_ends()))
            .unwrap_or_default();
        if let Some(log) = self.session_log.as_mut() {
            for note in notes {
                let class = serde_json::to_value(note.class)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                log.crowd(&note.carrier, &class, note.members, note.actors, at_ms);
            }
            // The one recorded end of a routed crowd that was never voiced.
            for (carrier, reason) in ends {
                log.fallback(&carrier, reason, false, at_ms);
            }
        }
    }

    /// A fixed-vocabulary event on a turn.
    /// An initiative repeat refusal with the matched phrase's origin, logical
    /// age and shown flag (#547).
    pub(crate) fn session_log_repeat_refusal(
        &mut self,
        event_id: &str,
        repeat: &str,
        matched: (&str, u64, bool),
        at_ms: u64,
    ) {
        if let Some(log) = self.session_log.as_mut() {
            log.note_repeat_refusal(event_id, repeat, matched.0, matched.1, matched.2, at_ms);
        }
    }

    pub(crate) fn session_log_note(
        &mut self,
        event_id: &str,
        kind: &'static str,
        detail: &str,
        at_ms: u64,
    ) {
        if let Some(log) = self.session_log.as_mut() {
            log.note(event_id, kind, detail, at_ms);
        }
    }
}
