//! Routing of recognised speech into the conversation input (issue #92, S3c).
//!
//! A *final* transcript becomes one `speech.input` [`EventEnvelope`] that is
//! serialized and handed to [`ProductionApp::process_content_bytes`], the very
//! entry point stdin and Twitch chat records use. There is therefore no second
//! request path: the runtime parses, validates, rejects non-content planes,
//! applies the operator's `IngressPolicy` ceiling, the payload cap, the
//! per-source token bucket and the bounded content queue exactly as for chat,
//! and the router builds the typed Reflex/Thinking request from the admitted
//! envelope.
//!
//! What this module decides, and what it must not:
//! * Source class, plane and trust come from the runtime-assigned
//!   [`SpeechTranscriptProvenance`](aivtuber_runtime::SpeechTranscriptProvenance)
//!   (constant: `speech` / `content` / `untrusted`), never from transcript text.
//! * The payload carries exactly `text` and the declared `language`. Router
//!   inputs such as `intent` or `template_id` are payload keys, so a spoken
//!   sentence can never select a cached asset or a template.
//! * `authorization` is always absent. A spoken final cannot mint a
//!   privileged-control capability or a durable-memory permit, even when it is
//!   the operator speaking: those are non-serializable values minted by
//!   authenticated local ingress, which this path never touches.
//! * Text is passed through unmodified (decision Q9). A final over the length
//!   bound is dropped whole and counted, never truncated: a cut sentence would
//!   be a different utterance.
//! * Ordering is logical. Event identity and the `observed_at` stamp derive
//!   from a counter and the transcript's runtime-timeline millisecond, never
//!   from the wall clock, so a replay yields identical envelopes.
//!
//! Partials never get here: the tracker exposes them only as `FloorHold`.

use aivtuber_domain::{EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, MAX_TRANSCRIPT_TEXT_CHARS};
use aivtuber_runtime::SurfacedFinal;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechConversationConfig {
    /// Finals taken from the tracker per call; the rest wait in the tracker's
    /// own bounded queue (which drops and counts on overflow).
    pub max_finals_per_tick: usize,
    /// A final longer than this many characters is dropped whole.
    pub max_text_chars: usize,
}

impl Default for SpeechConversationConfig {
    fn default() -> Self {
        Self {
            max_finals_per_tick: 4,
            max_text_chars: 400,
        }
    }
}

impl SpeechConversationConfig {
    pub fn validate(self) -> Result<Self, &'static str> {
        if !(1..=16).contains(&self.max_finals_per_tick) {
            return Err("speech conversation max_finals_per_tick must be in 1..=16");
        }
        if !(1..=MAX_TRANSCRIPT_TEXT_CHARS).contains(&self.max_text_chars) {
            return Err("speech conversation max_text_chars must be in 1..=1000");
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeechConversationMetrics {
    /// Finals taken from the tracker.
    pub finals_seen: u64,
    /// Finals the runtime admitted to the content queue.
    pub admitted: u64,
    /// Dropped whole for exceeding `max_text_chars`.
    pub dropped_oversize: u64,
    /// Dropped because the text was only whitespace.
    pub dropped_blank: u64,
    /// Refused by the runtime's ingress policy, rate limit, size caps or queue
    /// bound. The runtime audits the specific reason.
    pub dropped_by_runtime: u64,
    /// Admitted but failed while routing.
    pub route_errors: u64,
}

#[derive(Debug)]
pub(crate) struct SpeechConversation {
    config: SpeechConversationConfig,
    metrics: SpeechConversationMetrics,
    next_sequence: u64,
}

impl SpeechConversation {
    pub(crate) fn new(config: SpeechConversationConfig) -> Self {
        Self {
            config,
            metrics: SpeechConversationMetrics::default(),
            next_sequence: 1,
        }
    }

    pub(crate) fn config(&self) -> SpeechConversationConfig {
        self.config
    }

    pub(crate) fn metrics(&self) -> SpeechConversationMetrics {
        self.metrics
    }

    pub(crate) fn metrics_mut(&mut self) -> &mut SpeechConversationMetrics {
        &mut self.metrics
    }

    /// Build the envelope for one final, or count why it was dropped.
    pub(crate) fn envelope_for(&mut self, spoken: &SurfacedFinal) -> Option<EventEnvelope> {
        self.metrics.finals_seen += 1;
        if spoken.text.trim().is_empty() {
            self.metrics.dropped_blank += 1;
            return None;
        }
        if spoken.text.chars().count() > self.config.max_text_chars {
            self.metrics.dropped_oversize += 1;
            return None;
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let provenance = &spoken.provenance;
        Some(EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: format!("evt-speech-{sequence:010}"),
            correlation_id: format!("corr-speech-{sequence:010}"),
            sequence,
            observed_at: logical_timestamp(spoken.observed_at_ms),
            source: format!("speech-stt:{}", provenance.provider_id),
            source_class: provenance.source_class,
            plane: provenance.plane,
            trust_level: provenance.trust_level,
            kind: EventKind::SpeechInput,
            actor_id: Some(format!("speaker:{}", spoken.speaker_id)),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([
                (
                    "text".to_owned(),
                    serde_json::Value::String(spoken.text.clone()),
                ),
                (
                    "language".to_owned(),
                    serde_json::Value::String(provenance.language.clone()),
                ),
            ]),
        })
    }
}

/// RFC 3339 stamp for a runtime-timeline millisecond, counted from the Unix
/// epoch. It is a logical label, not a wall-clock reading: the same
/// millisecond always renders the same string.
pub(crate) fn logical_timestamp(ms: u64) -> String {
    // Keep the year within four digits (about 100 years of runtime).
    let ms = ms.min(3_153_600_000_000);
    let secs = ms / 1_000;
    let milli = ms % 1_000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{milli:03}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_timestamp_is_deterministic_and_valid_rfc3339_shape() {
        assert_eq!(logical_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            logical_timestamp(86_400_000 + 3_723_004),
            "1970-01-02T01:02:03.004Z"
        );
        // 2000-03-01 exercises the leap-day arithmetic.
        assert_eq!(
            logical_timestamp(951_868_800_000),
            "2000-03-01T00:00:00.000Z"
        );
        assert_eq!(logical_timestamp(u64::MAX).len(), 24);
    }

    #[test]
    fn config_bounds_are_enforced() {
        assert!(SpeechConversationConfig::default().validate().is_ok());
        for bad in [
            SpeechConversationConfig {
                max_finals_per_tick: 0,
                ..Default::default()
            },
            SpeechConversationConfig {
                max_finals_per_tick: 17,
                ..Default::default()
            },
            SpeechConversationConfig {
                max_text_chars: 0,
                ..Default::default()
            },
            SpeechConversationConfig {
                max_text_chars: MAX_TRANSCRIPT_TEXT_CHARS + 1,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
