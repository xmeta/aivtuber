#![forbid(unsafe_code)]

//! Production-target security/runtime boundary.
//!
//! Public content enters through byte-aware rate/backpressure gates. Privileged
//! control uses a separate authenticated path and never waits behind content.

mod cached_playback;
pub use cached_playback::*;

use aivtuber_domain::{
    AuthenticatedControl, AuthenticatedControlCommand, Capability, EventEnvelope, EventKind,
    SecurityPlane, TrustLevel,
};
use aivtuber_scheduler::{Scheduler, SchedulerConfig};
use aivtuber_telemetry::{AuditCategory, AuditRecord, SecretRedactor};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SecurityRuntimeConfig {
    pub content_rate_per_second: f64,
    pub content_burst: u32,
    pub max_envelope_bytes: usize,
    pub max_payload_bytes: usize,
    pub content_queue_limit: usize,
}

impl Default for SecurityRuntimeConfig {
    fn default() -> Self {
        Self {
            content_rate_per_second: 20.0,
            content_burst: 40,
            max_envelope_bytes: 64 * 1024,
            max_payload_bytes: 16 * 1024,
            content_queue_limit: 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    InvalidConfiguration(&'static str),
    InvalidEnvelope(String),
    UnauthorizedControl(&'static str),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(field) => {
                write!(f, "invalid runtime security configuration: {field}")
            }
            Self::InvalidEnvelope(message) => write!(f, "invalid event envelope: {message}"),
            Self::UnauthorizedControl(reason) => {
                write!(f, "authenticated control rejected: {reason}")
            }
        }
    }
}

impl Error for RuntimeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentAdmitDecision {
    Queued,
    DroppedEnvelopeBytes,
    DroppedPayloadBytes,
    DroppedRate,
    DroppedBackpressure,
    RejectedNonContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOutcome {
    Stopped { cancelled: usize },
    Muted,
    AuthenticatedOther,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryWriteDecision {
    AllowedSystemSource,
    AllowedMemoryAdmin,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputVerdict {
    Allow,
    Redact,
    ReplaceWithCached,
    Suppress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOutput {
    pub verdict: OutputVerdict,
    pub text: Option<String>,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill_ms: u64,
}

#[derive(Debug)]
pub struct SecurityRuntime {
    config: SecurityRuntimeConfig,
    scheduler: Scheduler,
    buckets: BTreeMap<String, Bucket>,
    content_queue: VecDeque<EventEnvelope>,
    muted: bool,
    redactor: SecretRedactor,
    cached_reaction: Option<String>,
    audit: Vec<AuditRecord>,
}

impl SecurityRuntime {
    pub fn new(
        config: SecurityRuntimeConfig,
        scheduler_config: SchedulerConfig,
        redactor: SecretRedactor,
        cached_reaction: Option<String>,
    ) -> Result<Self, RuntimeError> {
        validate_config(config)?;
        Ok(Self {
            config,
            scheduler: Scheduler::new(scheduler_config),
            buckets: BTreeMap::new(),
            content_queue: VecDeque::new(),
            muted: false,
            redactor,
            cached_reaction,
            audit: Vec::new(),
        })
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        &mut self.scheduler
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn content_len(&self) -> usize {
        self.content_queue.len()
    }

    pub fn audit(&self) -> &[AuditRecord] {
        &self.audit
    }

    pub fn pop_content(&mut self) -> Option<EventEnvelope> {
        self.content_queue.pop_front()
    }

    /// Admit untrusted/semi-trusted content from serialized bytes.
    ///
    /// The raw envelope byte cap is checked before parsing. The payload cap is
    /// measured from UTF-8 JSON bytes, never character/string length.
    pub fn admit_content_bytes(
        &mut self,
        raw: &[u8],
        now_ms: u64,
    ) -> Result<ContentAdmitDecision, RuntimeError> {
        if raw.len() > self.config.max_envelope_bytes {
            self.audit.push(self.redactor.record(
                None,
                AuditCategory::Ingress,
                "dropped_envelope_bytes",
                format!("envelope_bytes={}", raw.len()),
            ));
            return Ok(ContentAdmitDecision::DroppedEnvelopeBytes);
        }

        let event: EventEnvelope = serde_json::from_slice(raw)
            .map_err(|error| RuntimeError::InvalidEnvelope(error.to_string()))?;
        event
            .validate()
            .map_err(|error| RuntimeError::InvalidEnvelope(error.to_string()))?;

        if event.plane != SecurityPlane::Content
            || event.kind == EventKind::OperatorCommand
            || event.authorization.is_some()
        {
            self.record_ingress(
                &event,
                "rejected_non_content",
                "control/system authority is not accepted on content ingress",
            );
            return Ok(ContentAdmitDecision::RejectedNonContent);
        }

        let payload_bytes = serde_json::to_vec(&event.payload)
            .map_err(|error| RuntimeError::InvalidEnvelope(error.to_string()))?
            .len();
        if payload_bytes > self.config.max_payload_bytes {
            self.record_ingress(
                &event,
                "dropped_payload_bytes",
                &format!("payload_bytes={payload_bytes}"),
            );
            return Ok(ContentAdmitDecision::DroppedPayloadBytes);
        }

        if !self.consume_token(&event.source, now_ms) {
            self.record_ingress(&event, "dropped_rate", "per-source token bucket exhausted");
            return Ok(ContentAdmitDecision::DroppedRate);
        }

        if self.content_queue.len() >= self.config.content_queue_limit {
            self.record_ingress(&event, "dropped_backpressure", "content queue full");
            return Ok(ContentAdmitDecision::DroppedBackpressure);
        }

        self.record_ingress(&event, "queued", &format!("payload_bytes={payload_bytes}"));
        self.content_queue.push_back(event);
        Ok(ContentAdmitDecision::Queued)
    }

    /// Authenticated emergency/control path. It bypasses content queues and
    /// model services and is therefore not starved by content backpressure.
    pub fn handle_control(
        &mut self,
        command: &AuthenticatedControlCommand,
        at_ms: u64,
    ) -> Result<ControlOutcome, RuntimeError> {
        let event = command.event();
        let authority = command.authority();
        validate_authority_matches_event(authority, event)?;

        let action = event
            .payload
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or(RuntimeError::UnauthorizedControl("missing action"))?;

        let outcome = match action {
            "stop" | "performer.stop" => {
                require_capability(authority, Capability::PerformerStop)?;
                let cancelled = self.scheduler.stop_all(at_ms).len();
                ControlOutcome::Stopped { cancelled }
            }
            "mute" | "performer.mute" => {
                require_capability(authority, Capability::PerformerMute)?;
                self.muted = true;
                ControlOutcome::Muted
            }
            _ => ControlOutcome::AuthenticatedOther,
        };

        self.audit.push(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Authorization,
            "authorized_control",
            format!("principal={} action={action}", authority.principal()),
        ));
        Ok(outcome)
    }

    /// Apply an authenticated control command to a scheduler owned by a
    /// production composition root. This keeps emergency stop on the same
    /// timeline as cached playback instead of maintaining a shadow scheduler.
    pub fn handle_control_with_scheduler(
        &mut self,
        command: &AuthenticatedControlCommand,
        at_ms: u64,
        scheduler: &mut Scheduler,
    ) -> Result<ControlOutcome, RuntimeError> {
        let event = command.event();
        let authority = command.authority();
        validate_authority_matches_event(authority, event)?;

        let action = event
            .payload
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or(RuntimeError::UnauthorizedControl("missing action"))?;

        let outcome = match action {
            "stop" | "performer.stop" => {
                require_capability(authority, Capability::PerformerStop)?;
                let cancelled = scheduler.stop_all(at_ms).len();
                ControlOutcome::Stopped { cancelled }
            }
            "mute" | "performer.mute" => {
                require_capability(authority, Capability::PerformerMute)?;
                self.muted = true;
                ControlOutcome::Muted
            }
            _ => ControlOutcome::AuthenticatedOther,
        };

        self.audit.push(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Authorization,
            "authorized_control",
            format!("principal={} action={action}", authority.principal()),
        ));
        Ok(outcome)
    }

    /// The production public-output surface always applies the deterministic
    /// output gate before returning text that may be spoken/displayed.
    pub fn publish_text(&mut self, text: &str) -> PublicOutput {
        const CONTROL_TERMS: [&str; 5] = [
            "obs.control",
            "tool.grant",
            "memory.admin",
            "performer.stop",
            "performer.mute",
        ];

        let lower = text.to_ascii_lowercase();
        if CONTROL_TERMS.iter().any(|term| lower.contains(term)) {
            let output = if let Some(cached) = &self.cached_reaction {
                PublicOutput {
                    verdict: OutputVerdict::ReplaceWithCached,
                    text: Some(self.redactor.redact(cached)),
                    reason: "control_plane_text",
                }
            } else {
                PublicOutput {
                    verdict: OutputVerdict::Suppress,
                    text: None,
                    reason: "control_plane_text",
                }
            };
            self.record_output(output.reason);
            return output;
        }

        let redacted = self.redactor.redact(text);
        let output = if redacted != text {
            PublicOutput {
                verdict: OutputVerdict::Redact,
                text: Some(redacted),
                reason: "configured_secret_redacted",
            }
        } else {
            PublicOutput {
                verdict: OutputVerdict::Allow,
                text: Some(text.to_owned()),
                reason: "clean",
            }
        };
        self.record_output(output.reason);
        output
    }

    pub fn gate_memory_write(
        &mut self,
        event: &EventEnvelope,
        authority: Option<&AuthenticatedControl>,
    ) -> MemoryWriteDecision {
        let decision = if authority.is_some_and(|auth| auth.has_capability(Capability::MemoryAdmin))
        {
            MemoryWriteDecision::AllowedMemoryAdmin
        } else if event.plane == SecurityPlane::System
            && matches!(
                event.trust_level,
                TrustLevel::Trusted | TrustLevel::SemiTrusted
            )
        {
            MemoryWriteDecision::AllowedSystemSource
        } else {
            MemoryWriteDecision::Denied
        };

        self.audit.push(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Memory,
            format!("{decision:?}"),
            "memory write gate",
        ));
        decision
    }

    fn consume_token(&mut self, source: &str, now_ms: u64) -> bool {
        let bucket = self.buckets.entry(source.to_owned()).or_insert(Bucket {
            tokens: f64::from(self.config.content_burst),
            last_refill_ms: now_ms,
        });
        let elapsed_ms = now_ms.saturating_sub(bucket.last_refill_ms);
        bucket.tokens = (bucket.tokens
            + (elapsed_ms as f64 / 1000.0) * self.config.content_rate_per_second)
            .min(f64::from(self.config.content_burst));
        bucket.last_refill_ms = now_ms;

        if bucket.tokens < 1.0 {
            false
        } else {
            bucket.tokens -= 1.0;
            true
        }
    }

    fn record_ingress(&mut self, event: &EventEnvelope, decision: &str, detail: &str) {
        self.audit.push(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Ingress,
            decision,
            detail,
        ));
    }

    fn record_output(&mut self, reason: &str) {
        self.audit.push(
            self.redactor
                .record(None, AuditCategory::Output, reason, reason),
        );
    }
}

fn validate_config(config: SecurityRuntimeConfig) -> Result<(), RuntimeError> {
    if !config.content_rate_per_second.is_finite() || config.content_rate_per_second <= 0.0 {
        return Err(RuntimeError::InvalidConfiguration(
            "content_rate_per_second",
        ));
    }
    if config.content_burst == 0 {
        return Err(RuntimeError::InvalidConfiguration("content_burst"));
    }
    if config.max_envelope_bytes == 0 {
        return Err(RuntimeError::InvalidConfiguration("max_envelope_bytes"));
    }
    if config.max_payload_bytes == 0 {
        return Err(RuntimeError::InvalidConfiguration("max_payload_bytes"));
    }
    if config.content_queue_limit == 0 {
        return Err(RuntimeError::InvalidConfiguration("content_queue_limit"));
    }
    Ok(())
}

fn require_capability(
    authority: &AuthenticatedControl,
    capability: Capability,
) -> Result<(), RuntimeError> {
    if authority.has_capability(capability) {
        Ok(())
    } else {
        Err(RuntimeError::UnauthorizedControl("capability not granted"))
    }
}

fn validate_authority_matches_event(
    authority: &AuthenticatedControl,
    event: &EventEnvelope,
) -> Result<(), RuntimeError> {
    if event.kind != EventKind::OperatorCommand || event.plane != SecurityPlane::Control {
        return Err(RuntimeError::UnauthorizedControl(
            "authority is not attached to an operator control event",
        ));
    }
    let claim = event
        .authorization
        .as_ref()
        .ok_or(RuntimeError::UnauthorizedControl(
            "missing authorization claim",
        ))?;
    if claim.principal != authority.principal()
        || claim.method != authority.method()
        || claim.capabilities != *authority.capabilities()
    {
        return Err(RuntimeError::UnauthorizedControl(
            "serialized claim does not match authenticated authority",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        AuthorizationMethod, ControlSecret, EVENT_SCHEMA_VERSION, LocalControlIngress,
        OperatorCommandInput, SourceClass,
    };
    use aivtuber_scheduler::{BlendChannel, PlannedPerformance, Priority, Status};
    use std::collections::{BTreeMap, BTreeSet};

    fn chat_event(id: &str, sequence: u64, source: &str, text: &str) -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: id.to_owned(),
            correlation_id: "corr-test".to_owned(),
            sequence,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: source.to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some("viewer:test".to_owned()),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([(
                "text".to_owned(),
                serde_json::Value::String(text.to_owned()),
            )]),
        }
    }

    fn authenticated(capability: Capability, action: &str) -> AuthenticatedControlCommand {
        let secret = [0x42_u8; 32];
        let ingress = LocalControlIngress::new(
            "local-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([capability]),
            ControlSecret::new(secret),
        )
        .expect("ingress");
        ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: format!("evt-{action}"),
                    correlation_id: "corr-control".to_owned(),
                    sequence: 99,
                    observed_at: "2026-09-24T00:00:00Z".to_owned(),
                    action: action.to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated")
    }

    fn runtime_with(config: SecurityRuntimeConfig, secrets: &[&str]) -> SecurityRuntime {
        SecurityRuntime::new(
            config,
            SchedulerConfig {
                min_reaction_spacing_ms: 0,
            },
            SecretRedactor::new(secrets.iter().copied()),
            Some("safe cached reaction".to_owned()),
        )
        .expect("runtime")
    }

    fn audio_plan() -> PlannedPerformance {
        PlannedPerformance {
            event_id: "evt-performance".to_owned(),
            asset_id: "asset.test".to_owned(),
            priority: Priority::Conversation,
            interruptible: false,
            interrupt_points_ms: vec![0, 1_000],
            start_at_ms: 0,
            duration_ms: 1_000,
            generation: 0,
            exclusive: false,
            channels: BTreeSet::from([BlendChannel::Audio]),
        }
    }

    #[test]
    fn payload_limit_uses_utf8_bytes_not_character_count() {
        let event = chat_event("evt-bytes", 1, "chat-a", "日本語日本語");
        let payload_json = serde_json::to_vec(&event.payload).expect("payload json");
        let character_count = std::str::from_utf8(&payload_json)
            .expect("utf8")
            .chars()
            .count();
        assert!(payload_json.len() > character_count);

        let config = SecurityRuntimeConfig {
            max_payload_bytes: character_count,
            ..SecurityRuntimeConfig::default()
        };
        let mut runtime = runtime_with(config, &[]);
        let raw = serde_json::to_vec(&event).expect("event json");

        assert_eq!(
            runtime.admit_content_bytes(&raw, 0).expect("decision"),
            ContentAdmitDecision::DroppedPayloadBytes
        );
        assert_eq!(runtime.content_len(), 0);
    }

    #[test]
    fn serialized_operator_claim_cannot_enter_authenticated_content_path() {
        let command = authenticated(Capability::PerformerStop, "performer.stop");
        let raw = serde_json::to_vec(command.event()).expect("operator event json");
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);

        assert_eq!(
            runtime.admit_content_bytes(&raw, 0).expect("decision"),
            ContentAdmitDecision::RejectedNonContent
        );
        assert_eq!(runtime.content_len(), 0);
    }

    #[test]
    fn rate_limit_is_per_source_and_enforced_before_queueing() {
        let config = SecurityRuntimeConfig {
            content_rate_per_second: 1.0,
            content_burst: 1,
            content_queue_limit: 10,
            ..SecurityRuntimeConfig::default()
        };
        let mut runtime = runtime_with(config, &[]);
        let first = serde_json::to_vec(&chat_event("evt-1", 1, "chat-a", "one")).unwrap();
        let second = serde_json::to_vec(&chat_event("evt-2", 2, "chat-a", "two")).unwrap();
        let other = serde_json::to_vec(&chat_event("evt-3", 3, "chat-b", "three")).unwrap();

        assert_eq!(
            runtime.admit_content_bytes(&first, 0).unwrap(),
            ContentAdmitDecision::Queued
        );
        assert_eq!(
            runtime.admit_content_bytes(&second, 0).unwrap(),
            ContentAdmitDecision::DroppedRate
        );
        assert_eq!(
            runtime.admit_content_bytes(&other, 0).unwrap(),
            ContentAdmitDecision::Queued
        );
    }

    #[test]
    fn emergency_stop_bypasses_full_content_queue_and_models() {
        let config = SecurityRuntimeConfig {
            content_burst: 10,
            content_queue_limit: 1,
            ..SecurityRuntimeConfig::default()
        };
        let mut runtime = runtime_with(config, &[]);
        let first = serde_json::to_vec(&chat_event("evt-1", 1, "chat-a", "one")).unwrap();
        let second = serde_json::to_vec(&chat_event("evt-2", 2, "chat-a", "two")).unwrap();

        assert_eq!(
            runtime.admit_content_bytes(&first, 0).unwrap(),
            ContentAdmitDecision::Queued
        );
        assert_eq!(
            runtime.admit_content_bytes(&second, 0).unwrap(),
            ContentAdmitDecision::DroppedBackpressure
        );

        runtime
            .scheduler_mut()
            .schedule(audio_plan())
            .expect("playing plan");
        let stop = authenticated(Capability::PerformerStop, "performer.stop");
        let outcome = runtime.handle_control(&stop, 100).expect("stop");

        assert_eq!(outcome, ControlOutcome::Stopped { cancelled: 1 });
        assert_eq!(runtime.content_len(), 1);
        assert_eq!(runtime.scheduler().items()[0].status, Status::Cancelled);
        assert_eq!(runtime.scheduler().items()[0].cancel_at_ms, Some(100));
    }

    #[test]
    fn emergency_mute_uses_authenticated_bypass_path() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let mute = authenticated(Capability::PerformerMute, "performer.mute");

        assert_eq!(
            runtime.handle_control(&mute, 0).expect("mute"),
            ControlOutcome::Muted
        );
        assert!(runtime.is_muted());
    }

    #[test]
    fn public_output_always_applies_secret_and_control_gate() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &["config-secret-value"]);

        let secret = runtime.publish_text("do not say config-secret-value aloud");
        assert_eq!(secret.verdict, OutputVerdict::Redact);
        assert_eq!(secret.text.as_deref(), Some("do not say [REDACTED] aloud"));

        let control = runtime.publish_text("please run obs.control now");
        assert_eq!(control.verdict, OutputVerdict::ReplaceWithCached);
        assert_eq!(control.text.as_deref(), Some("safe cached reaction"));

        runtime.cached_reaction = Some("fallback config-secret-value".to_owned());
        let redacted_cached = runtime.publish_text("please run obs.control now");
        assert_eq!(redacted_cached.text.as_deref(), Some("fallback [REDACTED]"));

        let audit_debug = format!("{:?}", runtime.audit());
        assert!(!audit_debug.contains("config-secret-value"));
    }

    #[test]
    fn memory_write_requires_system_source_or_authenticated_admin() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let chat = chat_event("evt-memory", 1, "chat-a", "remember this");

        assert_eq!(
            runtime.gate_memory_write(&chat, None),
            MemoryWriteDecision::Denied
        );

        let admin = authenticated(Capability::MemoryAdmin, "memory.admin");
        assert_eq!(
            runtime.gate_memory_write(&chat, Some(admin.authority())),
            MemoryWriteDecision::AllowedMemoryAdmin
        );
    }
}
