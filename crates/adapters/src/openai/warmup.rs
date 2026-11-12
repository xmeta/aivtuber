//! One-shot Thinking warm-up for a local inference server (issue #333).
//!
//! A cold llama-server evaluates the first prompt for much longer than the
//! normal request timeout, so the first real chat line would always fall back.
//! The warm-up sends one request built by the same builder as a real one, with
//! a fixed internal user line and a one-token output limit, so the server loads
//! its weights and caches the shared prompt prefix.
//!
//! The reply is discarded by construction: [`warm_up`](OpenAiResponsesAdapter::warm_up)
//! never reads the output text, only an optional token count. Nothing here is
//! authority, memory or speech.
//!
//! State is one bounded value: the instant by which the in-flight warm-up must
//! have ended. A real request that overlaps it is given that much extra
//! transport time (see [`WarmupGate::grace`]); it is never made to wait.

use super::{OpenAiResponsesAdapter, compact_generation_input, map_status, map_transport_error};
use aivtuber_domain::{
    EngineError, EngineErrorKind, EventKind, PrivacyClass, ReflexContext, RetrievalSnapshot,
    SourceClass, ThinkingContent, ThinkingContextSource, ThinkingRequest, TrustLevel,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The fixed, clearly internal user line. Not configurable and not derived
/// from any input.
pub const WARMUP_USER_LINE: &str = "[internal warm-up, no reply needed]";

/// The smallest output limit the API accepts.
const WARMUP_MAX_OUTPUT_TOKENS: u64 = 1;

/// Shared by every clone of one adapter. Holds at most one deadline.
#[derive(Clone, Default)]
pub(super) struct WarmupGate(Arc<Mutex<Option<Instant>>>);

impl WarmupGate {
    /// Extra transport time a real request gets because it may queue behind
    /// the warm-up on a single-slot server: the time the warm-up can still
    /// run. Zero when no warm-up is in flight.
    pub(super) fn grace(&self) -> Duration {
        let slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        slot.map(|end| end.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    }

    pub(super) fn begin(&self, timeout: Duration) -> InFlight<'_> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + timeout);
        InFlight(self)
    }
}

/// Clears the deadline on every exit path, including a panic.
pub(super) struct InFlight<'a>(&'a WarmupGate);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// What a finished warm-up reports. No reply text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmupReport {
    pub elapsed: Duration,
    /// Prompt token count when the server reports usage.
    pub prompt_tokens: Option<u64>,
}

pub(super) fn warmup_request() -> ThinkingRequest {
    ThinkingRequest {
        schema_version: aivtuber_domain::THINKING_REQUEST_SCHEMA_VERSION.to_owned(),
        input: ThinkingContent {
            source: ThinkingContextSource::CurrentEvent,
            event_id: Some("evt-warmup".to_owned()),
            event_kind: Some(EventKind::ChatMessage),
            source_class: Some(SourceClass::PublicChat),
            trust_level: TrustLevel::Untrusted,
            privacy: PrivacyClass::Pseudonymous,
            epistemic_class: None,
            text: WARMUP_USER_LINE.to_owned(),
        },
        context: ReflexContext::default(),
        retrieval: RetrievalSnapshot {
            candidates: Vec::new(),
        },
        curated_context: Vec::new(),
    }
}

fn prompt_tokens(payload: &Value) -> Option<u64> {
    let usage = payload.get("usage")?;
    usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
}

impl OpenAiResponsesAdapter {
    /// Send one warm-up completion and discard its output. Blocks for at most
    /// `timeout`; call it off the performer and off the frame loop.
    pub fn warm_up(&self, timeout: Duration) -> Result<WarmupReport, EngineError> {
        if timeout.is_zero() {
            return Err(EngineError::new(
                EngineErrorKind::Timeout,
                "warm-up timeout must be greater than zero",
            ));
        }
        let started = Instant::now();
        let input = compact_generation_input(&warmup_request())?;
        let body = serde_json::to_vec(&json!({
            "model": self.config.model_alias,
            "input": input,
            "max_output_tokens": WARMUP_MAX_OUTPUT_TOKENS,
        }))
        .map_err(|error| EngineError::new(EngineErrorKind::InvalidRequest, error.to_string()))?;

        let _in_flight = self.warmup.begin(timeout);
        let response = self
            .transport
            .post_json(
                &self.config.endpoint,
                self.api_key.as_ref().map(|key| key.expose()),
                &body,
                timeout,
            )
            .map_err(map_transport_error)?;
        if response.status != 200 {
            return Err(map_status(response.status));
        }
        // The body is read only for the token count; the output is ignored.
        let prompt_tokens = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|payload| prompt_tokens(&payload));
        Ok(WarmupReport {
            elapsed: started.elapsed(),
            prompt_tokens,
        })
    }
}
