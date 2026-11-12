#![forbid(unsafe_code)]

//! Production-target security/runtime boundary.
//!
//! Public content enters through byte-aware rate/backpressure gates. Privileged
//! control uses a separate authenticated path and never waits behind content.

mod cached_playback;
pub use cached_playback::*;
mod speech_freshness;
pub use speech_freshness::*;
mod perception;
mod perception_governor;
pub use perception::*;
pub use perception_governor::*;
mod perception_replay;
pub use perception_replay::*;
mod scene_context;
pub use scene_context::*;
mod speech_observation;
pub use speech_observation::*;
mod speech_lifecycle_replay;
pub use speech_lifecycle_replay::*;
mod speech_replay;
pub use speech_replay::*;
mod speech_transcript;
pub use speech_transcript::*;
mod speech_transcript_replay;
pub use speech_transcript_replay::*;

use aivtuber_domain::{
    AuthenticatedControl, AuthenticatedControlCommand, Capability, EventEnvelope, EventKind,
    SecurityPlane, SourceClass, TrustLevel,
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
    pub max_audit_records: usize,
    pub max_rate_limit_sources: usize,
    pub rate_limit_source_ttl_ms: u64,
}

impl Default for SecurityRuntimeConfig {
    fn default() -> Self {
        Self {
            content_rate_per_second: 20.0,
            content_burst: 40,
            max_envelope_bytes: 64 * 1024,
            max_payload_bytes: 16 * 1024,
            content_queue_limit: 1024,
            max_audit_records: 4_096,
            max_rate_limit_sources: 4_096,
            rate_limit_source_ttl_ms: 10 * 60 * 1_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecurityRetentionMetrics {
    pub audit_retained: usize,
    pub audit_high_water: usize,
    pub audit_evicted: u64,
    pub rate_limit_sources: usize,
    pub rate_limit_sources_high_water: usize,
    pub rate_limit_sources_expired: u64,
    pub rate_limit_sources_rejected: u64,
    pub content_queue: usize,
    pub content_queue_high_water: usize,
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
    /// The record declared a trust level above the operator-configured ceiling
    /// or a source class outside the configured allow-set (#257). The record is
    /// rejected and audited, never rewritten to fit.
    RejectedIngressPolicy,
}

/// Result of re-running the content gate for a deferred event just before
/// dispatch (#75).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredRevalidation {
    Permitted,
    /// The envelope no longer validates.
    InvalidEnvelope,
    /// Control/system plane, operator kind or an authorization claim: content
    /// can never carry control authority, however long it waited.
    NonContent,
    /// The declared trust/source class exceeds the ingress ceiling.
    IngressPolicy,
    /// The payload exceeds the admitted payload cap.
    PayloadBytes,
}

/// Operator-configured ceiling for the stdin content ingress (#257).
///
/// A record's `source` is producer-chosen and spoofable, so the ceiling is a
/// property of the ingress channel the operator wired up, not of anything the
/// record says about itself. A record above the ceiling is rejected whole;
/// declared values are never clamped, so a mislabelled producer is visible
/// instead of silently repaired. The default admits everything the envelope
/// validation already admits, so existing deployments are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngressPolicy {
    max_trust: TrustLevel,
    /// One bit per `SourceClass`; see `class_bit`.
    allowed_classes: u8,
}

impl Default for IngressPolicy {
    fn default() -> Self {
        Self {
            max_trust: TrustLevel::Trusted,
            allowed_classes: u8::MAX,
        }
    }
}

fn trust_rank(trust: TrustLevel) -> u8 {
    match trust {
        TrustLevel::Untrusted => 0,
        TrustLevel::SemiTrusted => 1,
        TrustLevel::Trusted => 2,
    }
}

fn class_bit(class: SourceClass) -> u8 {
    1 << match class {
        SourceClass::PublicChat => 0,
        SourceClass::Donation => 1,
        SourceClass::Speech => 2,
        SourceClass::Game => 3,
        SourceClass::Stream => 4,
        SourceClass::Timer => 5,
        SourceClass::Operator => 6,
        SourceClass::System => 7,
    }
}

fn parse_source_class(name: &str) -> Option<SourceClass> {
    Some(match name {
        "public_chat" => SourceClass::PublicChat,
        "donation" => SourceClass::Donation,
        "speech" => SourceClass::Speech,
        "game" => SourceClass::Game,
        "stream" => SourceClass::Stream,
        "timer" => SourceClass::Timer,
        "operator" => SourceClass::Operator,
        "system" => SourceClass::System,
        _ => return None,
    })
}

impl IngressPolicy {
    pub fn new(max_trust: TrustLevel, allowed: &[SourceClass]) -> Self {
        Self {
            max_trust,
            allowed_classes: allowed
                .iter()
                .fold(0, |bits, class| bits | class_bit(*class)),
        }
    }

    /// Parse operator text: `max_trust` is `untrusted|semi_trusted|trusted`,
    /// `classes` a comma-separated list of source classes. `None` keeps the
    /// permissive default for that dimension; anything unrecognised is an
    /// error so a typo cannot silently widen the ceiling.
    pub fn parse(max_trust: Option<&str>, classes: Option<&str>) -> Result<Self, RuntimeError> {
        let mut policy = Self::default();
        if let Some(value) = max_trust {
            policy.max_trust = match value.trim() {
                "untrusted" => TrustLevel::Untrusted,
                "semi_trusted" => TrustLevel::SemiTrusted,
                "trusted" => TrustLevel::Trusted,
                _ => {
                    return Err(RuntimeError::InvalidConfiguration(
                        "ingress max trust must be untrusted, semi_trusted or trusted",
                    ));
                }
            };
        }
        if let Some(value) = classes {
            let mut bits = 0_u8;
            for name in value.split(',') {
                let class = parse_source_class(name.trim()).ok_or(
                    RuntimeError::InvalidConfiguration("unknown ingress source class"),
                )?;
                bits |= class_bit(class);
            }
            policy.allowed_classes = bits;
        }
        Ok(policy)
    }

    fn admits(&self, event: &EventEnvelope) -> bool {
        self.admits_declared(event.source_class, event.trust_level)
    }

    /// Whether the ceiling admits an ingress that *the runtime itself* has
    /// classified (not one a record declared). Used by ingress paths that do not
    /// carry an `EventEnvelope`, such as speech observation batches (#92).
    pub fn admits_declared(&self, class: SourceClass, trust: TrustLevel) -> bool {
        trust_rank(trust) <= trust_rank(self.max_trust)
            && self.allowed_classes & class_bit(class) != 0
    }
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

/// Non-serializable proof that the deterministic memory-write gate authorized
/// one concrete source event. Only `SecurityRuntime` can construct permits.
#[derive(Clone, PartialEq, Eq)]
pub struct MemoryWritePermit {
    event_id: String,
    source: String,
    source_class: aivtuber_domain::SourceClass,
    trust_level: TrustLevel,
    actor_id: Option<String>,
    decision: MemoryWriteDecision,
    /// The claim the source event itself carried (its payload `text`). A permit
    /// proves the *event* was authorized, not the content of an arbitrary
    /// sentence a caller supplies alongside it, so a consumer that wants to
    /// treat a retained claim as verified evidence must check it against this.
    source_claim: Option<String>,
}

impl fmt::Debug for MemoryWritePermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The permit carries the source event's own claim text. That is
        // retained content, not diagnostic metadata, and a permit is
        // non-serializable authority: debug/trace output reports only whether
        // the event carried a claim, so `{:?}` is not a disclosure path
        // (#105 review round 3).
        f.debug_struct("MemoryWritePermit")
            .field("event_id", &self.event_id)
            .field("source", &self.source)
            .field("source_class", &self.source_class)
            .field("trust_level", &self.trust_level)
            .field("actor_id", &self.actor_id)
            .field("decision", &self.decision)
            .field(
                "source_claim",
                &self.source_claim.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl MemoryWritePermit {
    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn source_class(&self) -> aivtuber_domain::SourceClass {
        self.source_class
    }

    pub fn trust_level(&self) -> TrustLevel {
        self.trust_level
    }

    pub fn actor_id(&self) -> Option<&str> {
        self.actor_id.as_deref()
    }

    pub fn decision(&self) -> MemoryWriteDecision {
        self.decision
    }

    /// The source event's own claim text, when its payload carried one.
    pub fn source_claim(&self) -> Option<&str> {
        self.source_claim.as_deref()
    }
}

/// Source label of memory the composition root retains from the performer's
/// own committed output (#306).
pub const PERFORMER_OUTPUT_SOURCE: &str = "performer.output";

/// Source label of memory the composition root loads from the operator-owned
/// seed file at startup (#321, ADR-0008 S2).
pub const OPERATOR_SEED_SOURCE: &str = "operator.seed";

/// The only chat text that asks to erase the requester's own memory (#305,
/// ADR-0008 D-Q7). Deliberately an explicit, exact command: a viewer sentence
/// that merely mentions forgetting is ordinary content, never an erase.
pub const VIEWER_ERASE_COMMAND: &str = "!forgetme";

/// Upper bound on explicit memory ids one operator erase may name.
pub const MAX_ERASE_MEMORY_IDS: usize = 32;

/// Who asked for an erasure. Recorded in the report for audit; it is not an
/// authority claim, the permit's construction is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEraseAuthority {
    Operator,
    Viewer,
}

/// Non-serializable proof that the deterministic erase gate authorized one
/// concrete erasure (#305). Only `SecurityRuntime` constructs one:
///
/// * an operator permit requires the authenticated `memory.admin` capability
///   and may name an actor and/or retained memory ids;
/// * a viewer permit is derived only from the platform-asserted `actor_id` and
///   `source` of the requester's own chat envelope, so it can name no other
///   actor and no memory id.
///
/// An erase only reduces retained data; this permit cannot read or write.
#[derive(Clone, PartialEq, Eq)]
pub struct MemoryErasePermit {
    request_id: String,
    authority: MemoryEraseAuthority,
    source_namespace: Option<String>,
    actor_id: Option<String>,
    memory_ids: Vec<u64>,
}

impl fmt::Debug for MemoryErasePermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The raw actor id is identifying; debug output never carries it.
        f.debug_struct("MemoryErasePermit")
            .field("request_id", &self.request_id)
            .field("authority", &self.authority)
            .field("source_namespace", &self.source_namespace)
            .field("actor_id", &self.actor_id.as_ref().map(|_| "[REDACTED]"))
            .field("memory_ids", &self.memory_ids)
            .finish()
    }
}

impl MemoryErasePermit {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn authority(&self) -> MemoryEraseAuthority {
        self.authority
    }

    /// The `(namespace, actor)` pair whose pseudonym is erased, when the
    /// request names an actor.
    pub fn actor(&self) -> Option<(&str, &str)> {
        self.source_namespace
            .as_deref()
            .zip(self.actor_id.as_deref())
    }

    /// Explicit retained memory ids (operator only), sorted and deduplicated.
    pub fn memory_ids(&self) -> &[u64] {
        &self.memory_ids
    }
}

/// Result of classifying one content event as a viewer self-erase request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerEraseDecision {
    /// Ordinary content; route it normally.
    NotACommand,
    /// The event used the command but cannot be honoured (no platform-asserted
    /// actor). It is consumed, never routed as content, and audited.
    Denied(&'static str),
    Permitted(MemoryErasePermit),
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

/// Cloneable, side-effect-free publication-containment policy.
///
/// The current production checks are deliberately narrow: block known control-plane
/// command terms from becoming public text and redact explicitly configured secrets.
/// This is not a general toxicity/harassment/sexual/brand/content-moderation policy,
/// and passing it does not make model output broadly "safe". Authorization safety is
/// instead enforced by typed, non-serializable capability boundaries.
///
/// Provider workers may evaluate generated text with this snapshot before TTS without
/// gaining access to scheduler/control authority or mutating the security audit. The
/// composition thread revalidates completed work before publication and records the
/// resulting audit decision.
#[derive(Debug, Clone)]
pub struct PublicOutputPolicy {
    redactor: SecretRedactor,
    cached_reaction: Option<String>,
}

impl PublicOutputPolicy {
    pub fn evaluate(&self, text: &str) -> PublicOutput {
        const CONTROL_TERMS: [&str; 7] = [
            "obs.control",
            "perception.control",
            "model.select",
            "tool.grant",
            "memory.admin",
            "performer.stop",
            "performer.mute",
        ];

        let lower = text.to_ascii_lowercase();
        if CONTROL_TERMS.iter().any(|term| lower.contains(term)) {
            return if let Some(cached) = &self.cached_reaction {
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
        }

        let redacted = self.redactor.redact(text);
        if redacted != text {
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
        }
    }
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
    audit_high_water: usize,
    audit_evicted: u64,
    rate_limit_sources_high_water: usize,
    rate_limit_sources_expired: u64,
    rate_limit_sources_rejected: u64,
    content_queue_high_water: usize,
    ingress_policy: IngressPolicy,
    ingress_policy_rejected: u64,
    /// Bounded allowlist feed for the #68 support bundle: the last K admitted
    /// content events' correlation ids, mapped to their event id and sequence.
    /// Correlation ids are operator-generated incident references (never raw
    /// viewer identifiers), so they are safe to expose; payloads never enter
    /// this registry.
    correlations: BTreeMap<String, CorrelationRef>,
    /// Insertion-ordered correlation ids backing `correlations` so "recent"
    /// means recency of admission rather than lexicographic key order.
    correlation_order: Vec<String>,
}

#[derive(Debug, Clone)]
struct CorrelationRef {
    event_id: String,
    sequence: u64,
}

/// Borrowed view of one registered correlation for the #68 support bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorrelationEntry<'a> {
    pub correlation_id: &'a str,
    pub event_id: &'a str,
    pub sequence: u64,
}

/// Upper bound of the #68 correlation registry. Small, fixed, and independent
/// of the content queue so bundle generation can never retain unbounded
/// history.
const MAX_CORRELATION_REFS: usize = 128;

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
            audit_high_water: 0,
            audit_evicted: 0,
            rate_limit_sources_high_water: 0,
            rate_limit_sources_expired: 0,
            rate_limit_sources_rejected: 0,
            content_queue_high_water: 0,
            ingress_policy: IngressPolicy::default(),
            ingress_policy_rejected: 0,
            correlations: BTreeMap::new(),
            correlation_order: Vec::new(),
        })
    }

    /// Install the stdin content-ingress ceiling (#257). Applies to every
    /// later `admit_content_bytes`; the default policy changes nothing.
    pub fn set_ingress_policy(&mut self, policy: IngressPolicy) {
        self.ingress_policy = policy;
    }

    /// Records rejected for exceeding the ingress ceiling since startup.
    pub fn ingress_policy_rejected(&self) -> u64 {
        self.ingress_policy_rejected
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

    /// Shared secret redactor used for every record stored here (issue #68
    /// support-bundle export path applies it again at capture time).
    pub fn redactor(&self) -> &SecretRedactor {
        &self.redactor
    }

    pub fn retention_metrics(&self) -> SecurityRetentionMetrics {
        SecurityRetentionMetrics {
            audit_retained: self.audit.len(),
            audit_high_water: self.audit_high_water,
            audit_evicted: self.audit_evicted,
            rate_limit_sources: self.buckets.len(),
            rate_limit_sources_high_water: self.rate_limit_sources_high_water,
            rate_limit_sources_expired: self.rate_limit_sources_expired,
            rate_limit_sources_rejected: self.rate_limit_sources_rejected,
            content_queue: self.content_queue.len(),
            content_queue_high_water: self.content_queue_high_water,
        }
    }

    /// Redact and record an externally-produced audit record (issue #56
    /// operator control). Keeps the same retention policy as internal audit
    /// records while letting the app crate append operator decisions.
    pub fn redactor_record(
        &self,
        event_id: Option<&str>,
        category: AuditCategory,
        decision: impl Into<String>,
        detail: impl AsRef<str>,
    ) -> AuditRecord {
        self.redactor.record(event_id, category, decision, detail)
    }

    pub fn push_external_audit(&mut self, record: AuditRecord) {
        self.push_audit(record);
    }

    fn push_audit(&mut self, record: AuditRecord) {
        self.audit.push(record);
        if self.audit.len() > self.config.max_audit_records {
            let remove = self.audit.len() - self.config.max_audit_records;
            self.audit.drain(..remove);
            self.audit_evicted = self.audit_evicted.saturating_add(remove as u64);
        }
        self.audit_high_water = self.audit_high_water.max(self.audit.len());
    }

    /// Persist a generation-provider invocation without storing prompt or reply text.
    ///
    /// The source event id and deterministic routing reason are retained for audit,
    /// while configured secrets are redacted from provider identity fields.
    pub fn record_generation_call(
        &mut self,
        event_id: &str,
        routing_reason: &str,
        backend_name: &str,
        model_alias: Option<&str>,
        model_version: Option<&str>,
    ) {
        let detail = format!(
            "routing_reason={routing_reason} backend={backend_name} model_alias={} model_version={}",
            model_alias.unwrap_or("-"),
            model_version.unwrap_or("-")
        );
        self.push_audit(self.redactor.record(
            Some(event_id),
            AuditCategory::Generation,
            "llm_call",
            detail,
        ));
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
            self.push_audit(self.redactor.record(
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

        if !self.ingress_policy.admits(&event) {
            self.ingress_policy_rejected = self.ingress_policy_rejected.saturating_add(1);
            self.record_ingress(
                &event,
                "rejected_ingress_policy",
                "declared trust/source class exceeds the configured ingress ceiling",
            );
            return Ok(ContentAdmitDecision::RejectedIngressPolicy);
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
        self.record_correlation(&event);
        self.content_queue.push_back(event);
        self.content_queue_high_water = self.content_queue_high_water.max(self.content_queue.len());
        Ok(ContentAdmitDecision::Queued)
    }

    /// Re-run the structural content gate for an event that was admitted
    /// earlier and held (#75). It consumes no rate token and no queue slot (it
    /// was already charged at admission) and never admits anything new: it can
    /// only refuse. A refusal is audited without payload content.
    pub fn revalidate_deferred_content(&mut self, event: &EventEnvelope) -> DeferredRevalidation {
        let refusal = if event.validate().is_err() {
            Some((
                DeferredRevalidation::InvalidEnvelope,
                "deferred_revalidation_invalid",
            ))
        } else if event.plane != SecurityPlane::Content
            || event.kind == EventKind::OperatorCommand
            || event.authorization.is_some()
        {
            Some((
                DeferredRevalidation::NonContent,
                "deferred_revalidation_non_content",
            ))
        } else if !self.ingress_policy.admits(event) {
            Some((
                DeferredRevalidation::IngressPolicy,
                "deferred_revalidation_ingress_policy",
            ))
        } else if serde_json::to_vec(&event.payload)
            .map(|bytes| bytes.len() > self.config.max_payload_bytes)
            .unwrap_or(true)
        {
            Some((
                DeferredRevalidation::PayloadBytes,
                "deferred_revalidation_payload_bytes",
            ))
        } else {
            None
        };
        match refusal {
            None => DeferredRevalidation::Permitted,
            Some((verdict, decision)) => {
                self.record_ingress(event, decision, "refused before deferred dispatch");
                verdict
            }
        }
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
                self.scheduler
                    .cancel_all_speech(at_ms, aivtuber_domain::SpeechCancelReason::Muted);
                ControlOutcome::Muted
            }
            _ => ControlOutcome::AuthenticatedOther,
        };

        self.push_audit(self.redactor.record(
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
                scheduler.cancel_all_speech(at_ms, aivtuber_domain::SpeechCancelReason::Muted);
                ControlOutcome::Muted
            }
            _ => ControlOutcome::AuthenticatedOther,
        };

        self.push_audit(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Authorization,
            "authorized_control",
            format!("principal={} action={action}", authority.principal()),
        ));
        Ok(outcome)
    }

    /// Capture the immutable portion of the public-output gate for provider
    /// workers. The snapshot has no control/scheduler authority and does not
    /// write audit records.
    pub fn public_output_policy(&self) -> PublicOutputPolicy {
        PublicOutputPolicy {
            redactor: self.redactor.clone(),
            cached_reaction: self.cached_reaction.clone(),
        }
    }

    /// Record a previously evaluated public-output decision on the composition
    /// thread. Worker-side policy evaluation deliberately does not audit.
    pub fn record_public_output_decision(&mut self, output: &PublicOutput) {
        self.record_output(output.reason);
    }

    /// The production public-output surface always applies the deterministic
    /// output gate before returning text that may be spoken/displayed.
    pub fn publish_text(&mut self, text: &str) -> PublicOutput {
        let output = self.public_output_policy().evaluate(text);
        self.record_public_output_decision(&output);
        output
    }

    pub fn gate_memory_write(
        &mut self,
        event: &EventEnvelope,
        authority: Option<&AuthenticatedControl>,
    ) -> MemoryWriteDecision {
        self.authorize_memory_write(event, authority).0
    }

    pub fn authorize_memory_write(
        &mut self,
        event: &EventEnvelope,
        authority: Option<&AuthenticatedControl>,
    ) -> (MemoryWriteDecision, Option<MemoryWritePermit>) {
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

        self.push_audit(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Memory,
            format!("{decision:?}"),
            "memory write gate",
        ));
        let permit =
            (!matches!(decision, MemoryWriteDecision::Denied)).then(|| MemoryWritePermit {
                event_id: event.event_id.clone(),
                source: event.source.clone(),
                source_class: event.source_class,
                trust_level: event.trust_level,
                actor_id: event.actor_id.clone(),
                decision,
                source_claim: event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            });
        (decision, permit)
    }

    /// Permit for the app's post-turn consolidation step to retain one
    /// stream-level fact the performer itself already published (#306,
    /// ADR-0008 Q1).
    ///
    /// The permit is not derived from the text alone: the claim must pass the
    /// current deterministic public-output gate unchanged (`Allow` or `Redact`
    /// with the identical text), so nothing the gate would have withheld or
    /// rewritten can be retained. Source class `speech`, plane `system`,
    /// trust `trusted`, no actor: it can never carry a viewer identity, and the
    /// epistemic class derives to a performer experience, never a verified
    /// fact. Public chat has no path here; only code holding the
    /// `SecurityRuntime` (the composition root) can call it.
    pub fn authorize_performer_output_memory_write(
        &mut self,
        event_id: &str,
        claim: &str,
    ) -> Option<MemoryWritePermit> {
        self.authorize_published_claim_write(
            event_id,
            claim,
            PERFORMER_OUTPUT_SOURCE,
            "performer_output",
            "performer output memory write gate",
        )
    }

    /// Permit for the composition root to load one entry of the operator-owned
    /// seed file at startup (#321, ADR-0008 S2).
    ///
    /// The file is operator configuration, not an input channel: nothing that
    /// arrives over chat, a model or the content queue can reach this. The
    /// claim must still pass the public-output gate unchanged, because a seeded
    /// claim is later spoken; a secret pasted into the file, or text the gate
    /// would withhold, is refused rather than retained. The permit is a
    /// performer-experience claim with no actor, so a seed entry can never be
    /// viewer-attributed and never derives to a verified fact.
    pub fn authorize_operator_seed_memory_write(
        &mut self,
        event_id: &str,
        claim: &str,
    ) -> Option<MemoryWritePermit> {
        self.authorize_published_claim_write(
            event_id,
            claim,
            OPERATOR_SEED_SOURCE,
            "operator_seed",
            "operator seed memory write gate",
        )
    }

    fn authorize_published_claim_write(
        &mut self,
        event_id: &str,
        claim: &str,
        source: &str,
        audit_prefix: &str,
        audit_detail: &str,
    ) -> Option<MemoryWritePermit> {
        let output = self.public_output_policy().evaluate(claim);
        let published_unchanged =
            matches!(output.verdict, OutputVerdict::Allow) && output.text.as_deref() == Some(claim);
        let decision = if published_unchanged {
            MemoryWriteDecision::AllowedSystemSource
        } else {
            MemoryWriteDecision::Denied
        };
        self.push_audit(self.redactor.record(
            Some(event_id),
            AuditCategory::Memory,
            format!("{audit_prefix}_{decision:?}"),
            audit_detail,
        ));
        published_unchanged.then(|| MemoryWritePermit {
            event_id: event_id.to_owned(),
            source: source.to_owned(),
            source_class: aivtuber_domain::SourceClass::Speech,
            trust_level: TrustLevel::Trusted,
            actor_id: None,
            decision,
            source_claim: Some(claim.to_owned()),
        })
    }

    /// Operator erase through the authenticated control capability (#305).
    /// Requires `memory.admin`; the scope must name an actor and/or at least
    /// one memory id, within [`MAX_ERASE_MEMORY_IDS`]. The decision is audited
    /// without the raw actor id.
    pub fn authorize_operator_memory_erase(
        &mut self,
        request_id: &str,
        authority: &AuthenticatedControl,
        actor: Option<(&str, &str)>,
        memory_ids: &[u64],
    ) -> Result<MemoryErasePermit, RuntimeError> {
        if !authority.has_capability(Capability::MemoryAdmin) {
            self.push_audit(self.redactor.record(
                Some(request_id),
                AuditCategory::Memory,
                "erase_denied",
                "operator memory erase requires memory.admin",
            ));
            return Err(RuntimeError::UnauthorizedControl(
                "memory erase requires memory.admin",
            ));
        }
        let mut ids: Vec<u64> = memory_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let invalid = request_id.is_empty()
            || ids.len() > MAX_ERASE_MEMORY_IDS
            || ids.contains(&0)
            || (actor.is_none() && ids.is_empty())
            || actor.is_some_and(|(namespace, id)| namespace.is_empty() || id.is_empty());
        if invalid {
            return Err(RuntimeError::InvalidConfiguration(
                "memory erase needs a request id and an actor and/or 1..=32 non-zero memory ids",
            ));
        }
        self.push_audit(self.redactor.record(
            Some(request_id),
            AuditCategory::Memory,
            "erase_authorized_operator",
            format!("actor_scope={} memory_ids={}", actor.is_some(), ids.len()),
        ));
        Ok(MemoryErasePermit {
            request_id: request_id.to_owned(),
            authority: MemoryEraseAuthority::Operator,
            source_namespace: actor.map(|(namespace, _)| namespace.to_owned()),
            actor_id: actor.map(|(_, id)| id.to_owned()),
            memory_ids: ids,
        })
    }

    /// Classify an admitted content event as a viewer self-erase request
    /// (#305, ADR-0008 D-Q7). The scope is taken only from the envelope's own
    /// `source` and platform-asserted `actor_id`, never from the text, so a
    /// viewer can erase only their own pseudonym; the text cannot name anyone
    /// else. Only `public_chat` on the content plane with no embedded
    /// authorization qualifies, and the text must be exactly
    /// [`VIEWER_ERASE_COMMAND`] (trimmed, ASCII case-insensitive). The
    /// per-actor rate limit is applied by the store, which owns the pseudonym
    /// (the runtime never keeps raw actor ids for it).
    pub fn authorize_viewer_memory_erase(&mut self, event: &EventEnvelope) -> ViewerEraseDecision {
        if event.plane != SecurityPlane::Content
            || event.source_class != SourceClass::PublicChat
            || event.authorization.is_some()
        {
            return ViewerEraseDecision::NotACommand;
        }
        let is_command = event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.trim().eq_ignore_ascii_case(VIEWER_ERASE_COMMAND));
        if !is_command {
            return ViewerEraseDecision::NotACommand;
        }
        let Some(actor) = event.actor_id.as_deref().filter(|actor| !actor.is_empty()) else {
            self.push_audit(self.redactor.record(
                Some(&event.event_id),
                AuditCategory::Memory,
                "erase_denied",
                "viewer erase needs a platform-asserted actor id",
            ));
            return ViewerEraseDecision::Denied("viewer erase needs a platform-asserted actor id");
        };
        self.push_audit(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Memory,
            "erase_authorized_viewer",
            "own pseudonym only",
        ));
        ViewerEraseDecision::Permitted(MemoryErasePermit {
            request_id: event.event_id.clone(),
            authority: MemoryEraseAuthority::Viewer,
            source_namespace: Some(event.source.clone()),
            actor_id: Some(actor.to_owned()),
            memory_ids: Vec::new(),
        })
    }

    fn consume_token(&mut self, source: &str, now_ms: u64) -> bool {
        if !self.buckets.contains_key(source) {
            self.expire_rate_limit_sources(now_ms);
            if self.buckets.len() >= self.config.max_rate_limit_sources {
                self.rate_limit_sources_rejected =
                    self.rate_limit_sources_rejected.saturating_add(1);
                return false;
            }
            self.buckets.insert(
                source.to_owned(),
                Bucket {
                    tokens: f64::from(self.config.content_burst),
                    last_refill_ms: now_ms,
                },
            );
            self.rate_limit_sources_high_water =
                self.rate_limit_sources_high_water.max(self.buckets.len());
        }

        let bucket = self
            .buckets
            .get_mut(source)
            .expect("rate-limit bucket inserted or already present");
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

    fn expire_rate_limit_sources(&mut self, now_ms: u64) {
        let before = self.buckets.len();
        let ttl_ms = self.config.rate_limit_source_ttl_ms;
        self.buckets
            .retain(|_, bucket| now_ms.saturating_sub(bucket.last_refill_ms) < ttl_ms);
        let expired = before.saturating_sub(self.buckets.len());
        self.rate_limit_sources_expired = self
            .rate_limit_sources_expired
            .saturating_add(expired as u64);
    }

    fn record_ingress(&mut self, event: &EventEnvelope, decision: &str, detail: &str) {
        self.push_audit(self.redactor.record(
            Some(&event.event_id),
            AuditCategory::Ingress,
            decision,
            detail,
        ));
    }

    /// Record an admitted content event's correlation id for #68 bundle
    /// assembly. Called only after admission succeeds. Insertion order is
    /// tracked separately so "recent" means recency, not key order.
    fn record_correlation(&mut self, event: &EventEnvelope) {
        if self.correlations.contains_key(&event.correlation_id) {
            self.correlation_order
                .retain(|id| id != &event.correlation_id);
        }
        self.correlation_order.push(event.correlation_id.clone());
        self.correlations.insert(
            event.correlation_id.clone(),
            CorrelationRef {
                event_id: event.event_id.clone(),
                sequence: event.sequence,
            },
        );
        while self.correlations.len() > MAX_CORRELATION_REFS {
            let oldest = self
                .correlation_order
                .first()
                .cloned()
                .expect("order track is non-empty while map is non-empty");
            self.correlation_order.remove(0);
            self.correlations.remove(&oldest);
        }
    }

    /// Bounded allowlist feed for the #68 support bundle: the most recently
    /// admitted correlation ids with their event id and sequence, newest
    /// first. No payload text, actor identifier, or authorization material is
    /// exposed — correlation ids are operator-generated incident references.
    pub fn recent_correlations(&self, limit: usize) -> Vec<CorrelationEntry<'_>> {
        self.correlation_order
            .iter()
            .rev()
            .take(limit)
            .filter_map(|id| {
                self.correlations.get(id).map(|entry| CorrelationEntry {
                    correlation_id: id,
                    event_id: &entry.event_id,
                    sequence: entry.sequence,
                })
            })
            .collect()
    }

    fn record_output(&mut self, reason: &str) {
        self.push_audit(
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
    if config.max_audit_records == 0 {
        return Err(RuntimeError::InvalidConfiguration("max_audit_records"));
    }
    if config.max_rate_limit_sources == 0 {
        return Err(RuntimeError::InvalidConfiguration("max_rate_limit_sources"));
    }
    if config.rate_limit_source_ttl_ms == 0 {
        return Err(RuntimeError::InvalidConfiguration(
            "rate_limit_source_ttl_ms",
        ));
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
                ..aivtuber_scheduler::SchedulerConfig::default()
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
    fn ingress_ceiling_rejects_and_counts_semi_trusted_and_other_classes() {
        let policy = IngressPolicy::parse(Some("untrusted"), Some("public_chat")).expect("policy");
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        runtime.set_ingress_policy(policy);

        let mut semi = chat_event("evt-semi", 1, "twitch:a", "hi");
        semi.trust_level = TrustLevel::SemiTrusted;
        assert_eq!(
            runtime
                .admit_content_bytes(&serde_json::to_vec(&semi).unwrap(), 0)
                .unwrap(),
            ContentAdmitDecision::RejectedIngressPolicy
        );

        let mut donation = chat_event("evt-don", 2, "twitch:a", "hi");
        donation.kind = EventKind::ChatDonation;
        donation.source_class = SourceClass::Donation;
        assert_eq!(
            runtime
                .admit_content_bytes(&serde_json::to_vec(&donation).unwrap(), 0)
                .unwrap(),
            ContentAdmitDecision::RejectedIngressPolicy
        );
        assert_eq!(runtime.ingress_policy_rejected(), 2);
        assert_eq!(runtime.content_len(), 0);

        let ok = chat_event("evt-ok", 3, "twitch:a", "hi");
        assert_eq!(
            runtime
                .admit_content_bytes(&serde_json::to_vec(&ok).unwrap(), 0)
                .unwrap(),
            ContentAdmitDecision::Queued
        );
        assert_eq!(runtime.ingress_policy_rejected(), 2);
    }

    #[test]
    fn default_ingress_policy_keeps_existing_admission() {
        assert_eq!(
            IngressPolicy::parse(None, None).unwrap(),
            IngressPolicy::default()
        );
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let mut semi = chat_event("evt-semi", 1, "chat-a", "hi");
        semi.trust_level = TrustLevel::SemiTrusted;
        assert_eq!(
            runtime
                .admit_content_bytes(&serde_json::to_vec(&semi).unwrap(), 0)
                .unwrap(),
            ContentAdmitDecision::Queued
        );
        assert_eq!(runtime.ingress_policy_rejected(), 0);
    }

    #[test]
    fn ingress_policy_parse_rejects_typos() {
        assert!(IngressPolicy::parse(Some("untrusts"), None).is_err());
        assert!(IngressPolicy::parse(None, Some("public_chat,chat")).is_err());
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
    fn security_retention_bounds_audit_and_source_cardinality_with_logical_ttl() {
        let config = SecurityRuntimeConfig {
            content_rate_per_second: 1_000.0,
            content_burst: 100,
            content_queue_limit: 4,
            max_audit_records: 5,
            max_rate_limit_sources: 2,
            rate_limit_source_ttl_ms: 10,
            ..SecurityRuntimeConfig::default()
        };
        let mut runtime = runtime_with(config, &[]);

        for (id, source) in [("evt-a", "source-a"), ("evt-b", "source-b")] {
            let raw = serde_json::to_vec(&chat_event(id, 1, source, "hello")).unwrap();
            assert_eq!(
                runtime.admit_content_bytes(&raw, 0).unwrap(),
                ContentAdmitDecision::Queued
            );
            runtime.pop_content().expect("queued event");
        }

        let blocked = serde_json::to_vec(&chat_event("evt-c0", 2, "source-c", "blocked")).unwrap();
        assert_eq!(
            runtime.admit_content_bytes(&blocked, 1).unwrap(),
            ContentAdmitDecision::DroppedRate
        );
        let full = runtime.retention_metrics();
        assert_eq!(full.rate_limit_sources, 2);
        assert_eq!(full.rate_limit_sources_high_water, 2);
        assert_eq!(full.rate_limit_sources_rejected, 1);

        let admitted =
            serde_json::to_vec(&chat_event("evt-c1", 3, "source-c", "admitted")).unwrap();
        assert_eq!(
            runtime.admit_content_bytes(&admitted, 11).unwrap(),
            ContentAdmitDecision::Queued
        );
        runtime.pop_content().expect("queued after ttl");

        for index in 0_u64..20 {
            let raw = serde_json::to_vec(&chat_event(
                &format!("evt-audit-{index}"),
                10 + index,
                "source-c",
                "audit churn",
            ))
            .unwrap();
            runtime.admit_content_bytes(&raw, 11 + index).unwrap();
            let _ = runtime.pop_content();
        }

        let metrics = runtime.retention_metrics();
        assert_eq!(metrics.rate_limit_sources, 1);
        assert_eq!(metrics.rate_limit_sources_high_water, 2);
        assert_eq!(metrics.rate_limit_sources_expired, 2);
        assert_eq!(metrics.audit_retained, 5);
        assert_eq!(metrics.audit_high_water, 5);
        assert!(metrics.audit_evicted > 0);
        assert!(runtime.audit().len() <= config.max_audit_records);
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
        // Bounded-state separation (#52): the stopped item retired into
        // bounded history instead of staying in the live collection.
        assert_eq!(runtime.scheduler().items().len(), 0);
        let stopped = runtime
            .scheduler()
            .history()
            .next()
            .expect("stopped item in history");
        assert_eq!(stopped.item.status, Status::Cancelled);
        assert_eq!(stopped.item.cancel_at_ms, Some(100));
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
    fn publication_containment_pins_control_terms_secret_redaction_and_fail_closed_scope() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &["config-secret-value"]);

        let ordinary = runtime.publish_text("ordinary public text outside the containment rules");
        assert_eq!(ordinary.verdict, OutputVerdict::Allow);
        assert_eq!(
            ordinary.text.as_deref(),
            Some("ordinary public text outside the containment rules")
        );

        let secret = runtime.publish_text("do not say config-secret-value aloud");
        assert_eq!(secret.verdict, OutputVerdict::Redact);
        assert_eq!(secret.text.as_deref(), Some("do not say [REDACTED] aloud"));

        for term in [
            "obs.control",
            "model.select",
            "tool.grant",
            "memory.admin",
            "performer.stop",
            "performer.mute",
        ] {
            let control = runtime.publish_text(&format!("please run {term} now"));
            assert_eq!(control.verdict, OutputVerdict::ReplaceWithCached);
            assert_eq!(control.text.as_deref(), Some("safe cached reaction"));
        }

        runtime.cached_reaction = None;
        let suppressed = runtime.publish_text("please run OBS.CONTROL now");
        assert_eq!(suppressed.verdict, OutputVerdict::Suppress);
        assert!(suppressed.text.is_none());

        runtime.cached_reaction = Some("fallback config-secret-value".to_owned());
        let redacted_cached = runtime.publish_text("please run obs.control now");
        assert_eq!(redacted_cached.text.as_deref(), Some("fallback [REDACTED]"));

        let audit_debug = format!("{:?}", runtime.audit());
        assert!(!audit_debug.contains("config-secret-value"));
    }

    #[test]
    fn generation_call_audit_records_source_route_and_redacts_provider_identity() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &["secret-model"]);

        runtime.record_generation_call(
            "evt-generation",
            "explicit_llm_route",
            "openai",
            Some("secret-model"),
            Some("2026-09-01"),
        );

        let record = runtime.audit().last().expect("generation audit");
        assert_eq!(record.event_id.as_deref(), Some("evt-generation"));
        assert_eq!(record.category, AuditCategory::Generation);
        assert_eq!(record.decision, "llm_call");
        assert!(record.detail.contains("routing_reason=explicit_llm_route"));
        assert!(record.detail.contains("backend=openai"));
        assert!(record.detail.contains("model_alias=[REDACTED]"));
        assert!(!record.detail.contains("secret-model"));
    }

    #[test]
    fn operator_seed_write_permit_needs_unchanged_gate_admission_and_carries_no_actor() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &["config-secret-value"]);

        let permit = runtime
            .authorize_operator_seed_memory_write("memory-seed.1", "Hana is a cat themed VTuber")
            .expect("ordinary operator text is admitted");
        assert_eq!(permit.source(), OPERATOR_SEED_SOURCE);
        assert_eq!(permit.source_class(), aivtuber_domain::SourceClass::Speech);
        assert_eq!(permit.trust_level(), TrustLevel::Trusted);
        assert_eq!(permit.actor_id(), None);
        assert_eq!(permit.decision(), MemoryWriteDecision::AllowedSystemSource);

        assert!(
            runtime
                .authorize_operator_seed_memory_write("memory-seed.2", "key config-secret-value")
                .is_none()
        );
        assert!(
            runtime
                .authorize_operator_seed_memory_write("memory-seed.3", "please run memory.admin")
                .is_none()
        );
    }

    #[test]
    fn performer_output_write_permit_needs_unchanged_gate_admission_and_carries_no_actor() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &["config-secret-value"]);

        let permit = runtime
            .authorize_performer_output_memory_write(
                "memory-writer.1",
                "I promise a rematch next stream",
            )
            .expect("ordinary published text is admitted");
        assert_eq!(permit.source(), PERFORMER_OUTPUT_SOURCE);
        assert_eq!(permit.source_class(), aivtuber_domain::SourceClass::Speech);
        assert_eq!(permit.actor_id(), None);
        assert_eq!(permit.decision(), MemoryWriteDecision::AllowedSystemSource);

        // Redaction would rewrite the text, and a control term is replaced:
        // neither may be retained.
        assert!(
            runtime
                .authorize_performer_output_memory_write(
                    "memory-writer.2",
                    "say config-secret-value next time"
                )
                .is_none()
        );
        assert!(
            runtime
                .authorize_performer_output_memory_write(
                    "memory-writer.3",
                    "please run memory.admin next time"
                )
                .is_none()
        );
    }

    #[test]
    fn memory_write_requires_system_source_or_authenticated_admin() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let chat = chat_event("evt-memory", 1, "chat-a", "remember this");

        let (denied, denied_permit) = runtime.authorize_memory_write(&chat, None);
        assert_eq!(denied, MemoryWriteDecision::Denied);
        assert!(denied_permit.is_none());

        let admin = authenticated(Capability::MemoryAdmin, "memory.admin");
        let (allowed, permit) = runtime.authorize_memory_write(&chat, Some(admin.authority()));
        assert_eq!(allowed, MemoryWriteDecision::AllowedMemoryAdmin);
        let permit = permit.expect("memory permit");
        assert_eq!(permit.event_id(), chat.event_id);
        assert_eq!(permit.source(), chat.source);
        assert_eq!(permit.source_class(), chat.source_class);
        assert_eq!(permit.trust_level(), chat.trust_level);
        assert_eq!(permit.actor_id(), chat.actor_id.as_deref());
        assert_eq!(permit.decision(), MemoryWriteDecision::AllowedMemoryAdmin);
    }

    #[test]
    fn memory_permit_debug_does_not_disclose_the_retained_claim() {
        // #105 review round 3: the permit now also carries the source event's
        // own payload text, so the derived `Debug` would print retained content
        // verbatim. Debug/tracing output must not become a disclosure path for
        // it, while the non-content metadata stays diagnosable.
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let chat = chat_event("evt-memory", 1, "chat-a", "sentinel-claim-text");
        let admin = authenticated(Capability::MemoryAdmin, "memory.admin");
        let (_, permit) = runtime.authorize_memory_write(&chat, Some(admin.authority()));
        let permit = permit.expect("memory permit");
        assert_eq!(permit.source_claim(), Some("sentinel-claim-text"));

        let debug = format!("{permit:?}");
        assert!(
            !debug.contains("sentinel-claim-text"),
            "the permit's Debug must not print the source claim: {debug}"
        );
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(debug.contains("evt-memory"), "{debug}");
    }

    #[test]
    fn viewer_erase_scope_comes_from_the_envelope_never_the_text() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let mut event = chat_event("evt-erase", 1, "chat-a", "  !ForgetMe ");
        event.actor_id = Some("viewer-a".to_owned());
        let ViewerEraseDecision::Permitted(permit) = runtime.authorize_viewer_memory_erase(&event)
        else {
            panic!("exact command must be permitted");
        };
        assert_eq!(permit.authority(), MemoryEraseAuthority::Viewer);
        assert_eq!(permit.actor(), Some((event.source.as_str(), "viewer-a")));
        assert!(permit.memory_ids().is_empty());
        assert!(!format!("{permit:?}").contains("viewer-a"));

        // Text naming someone else is not the command at all.
        let mut other = chat_event("evt-erase-2", 2, "chat-a", "!forgetme viewer-b");
        other.actor_id = Some("viewer-a".to_owned());
        assert_eq!(
            runtime.authorize_viewer_memory_erase(&other),
            ViewerEraseDecision::NotACommand
        );
    }

    #[test]
    fn viewer_erase_is_refused_without_an_asserted_actor_or_outside_public_chat() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let mut anonymous = chat_event("evt-erase", 1, "chat-a", "!forgetme");
        anonymous.actor_id = None;
        assert!(matches!(
            runtime.authorize_viewer_memory_erase(&anonymous),
            ViewerEraseDecision::Denied(_)
        ));

        let mut donation = chat_event("evt-erase-2", 2, "chat-a", "!forgetme");
        donation.actor_id = Some("viewer-a".to_owned());
        donation.source_class = SourceClass::Donation;
        assert_eq!(
            runtime.authorize_viewer_memory_erase(&donation),
            ViewerEraseDecision::NotACommand
        );
    }

    #[test]
    fn operator_erase_requires_memory_admin_and_a_bounded_scope() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let stop = authenticated(Capability::PerformerStop, "performer.stop");
        assert!(matches!(
            runtime.authorize_operator_memory_erase(
                "req-1",
                stop.authority(),
                Some(("public-chat", "viewer-a")),
                &[]
            ),
            Err(RuntimeError::UnauthorizedControl(_))
        ));

        let admin = authenticated(Capability::MemoryAdmin, "memory.admin");
        assert!(
            runtime
                .authorize_operator_memory_erase("req-2", admin.authority(), None, &[])
                .is_err(),
            "an empty scope is refused"
        );
        let too_many: Vec<u64> = (1..=(MAX_ERASE_MEMORY_IDS as u64 + 1)).collect();
        assert!(
            runtime
                .authorize_operator_memory_erase("req-3", admin.authority(), None, &too_many)
                .is_err()
        );
        let permit = runtime
            .authorize_operator_memory_erase("req-4", admin.authority(), None, &[5, 3, 5])
            .expect("operator permit");
        assert_eq!(permit.memory_ids(), &[3, 5]);
        assert_eq!(permit.authority(), MemoryEraseAuthority::Operator);
    }

    #[test]
    fn deferred_revalidation_only_refuses_and_charges_nothing() {
        let mut runtime = runtime_with(SecurityRuntimeConfig::default(), &[]);
        let event = chat_event("evt-held", 1, "chat-a", "hello");
        assert_eq!(
            runtime.revalidate_deferred_content(&event),
            DeferredRevalidation::Permitted
        );
        assert_eq!(runtime.content_len(), 0, "no queue slot is taken");

        let mut control = event.clone();
        control.plane = SecurityPlane::Control;
        assert_ne!(
            runtime.revalidate_deferred_content(&control),
            DeferredRevalidation::Permitted
        );
        let mut claimed = event.clone();
        claimed.kind = EventKind::OperatorCommand;
        assert_ne!(
            runtime.revalidate_deferred_content(&claimed),
            DeferredRevalidation::Permitted
        );

        let tight = SecurityRuntimeConfig {
            max_payload_bytes: 8,
            ..SecurityRuntimeConfig::default()
        };
        let mut small = runtime_with(tight, &[]);
        assert_eq!(
            small.revalidate_deferred_content(&chat_event("evt-big", 2, "chat-a", "far too long")),
            DeferredRevalidation::PayloadBytes
        );
        assert!(
            small
                .audit()
                .iter()
                .any(|record| record.decision == "deferred_revalidation_payload_bytes"),
            "a refusal is audited"
        );
    }
}
