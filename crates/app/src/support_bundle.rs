//! Privacy-safe, bounded runtime diagnostic/support bundle (issue #68).
//!
//! One call captures enough redacted runtime evidence to turn "the avatar
//! stopped reacting for ~20 seconds" into a comparable, reproducible issue:
//! build/toolchain identity, runtime composition profile, non-secret config
//! fingerprint, asset pack/compiler versions, semantic retriever identity,
//! provider/model identifiers, adapter health, bounded recent #65 causal
//! traces, redacted audit summaries, and the bounded resource/retention
//! counters from #51/#52/#53.
//!
//! Security model:
//! - the exporter is **allowlist-based**: it serializes explicit fields from
//!   typed runtime state; it never dumps configuration, environment
//!   variables, payloads, transcripts, or memory contents;
//! - causal traces enter the bundle only through
//!   `CausalTrace::redacted_timeline` (stage/outcome/reason lines, no
//!   payload text);
//! - audit summaries enter through the shared `SecretRedactor` and as
//!   decision counts plus bounded decision strings only;
//! - correlation ids in the identity section are operator-generated incident
//!   references, not viewer identities;
//! - generation is synchronous local work over already-retained state: no
//!   provider calls, no new retention growth, and no scheduler or control
//!   mutation, so operator emergency control stays responsive.

use aivtuber_telemetry::{AuditCategory, AuditRecord};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{GenerativeBudgetRecord, RoutePlanner};

/// Manifest schema version. Bump on any bundle-shape change so two reports
/// can be compared like-for-like.
pub const SUPPORT_BUNDLE_SCHEMA_VERSION: &str = "0.1.0";

/// Default bound on redacted causal timelines included in a bundle.
pub const DEFAULT_MAX_TIMELINES: usize = 8;

/// Default bound on redacted audit decision summaries included in a bundle.
pub const DEFAULT_MAX_AUDIT_SUMMARIES: usize = 32;

/// Upper bound the builder enforces on caller-supplied bounds so a bundle can
/// never retain unbounded runtime history, even when configured carelessly.
const MAX_ALLOWED_RECORDS: usize = 256;

/// Versioned manifest identifying the bundle so incomplete or incomparable
/// reports are detectable.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SupportBundleManifest {
    pub schema_version: &'static str,
    pub generated_unix_ms: u64,
    pub bundle_sha256: String,
}

/// Runtime/build/config identity. Every field is either a version string, a
/// boolean, or an allowlisted identifier — never secret material.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SupportIdentity {
    /// Build-time git revision hash (best effort; "unknown" when not built
    /// with VCS metadata).
    pub git_revision: String,
    /// Whether the working tree had uncommitted changes at build time.
    pub git_dirty: Option<bool>,
    pub rustc_version: String,
    pub os: String,
    pub arch: String,
    /// Active composition profile name (issue #55).
    pub composition_profile: String,
    /// Deterministic, non-secret configuration fingerprint (issue #55).
    pub config_fingerprint: String,
    /// Asset pack / compiler compatibility versions in use.
    pub compiler_version: String,
    pub voice_model: Option<String>,
    pub avatar_profile: Option<String>,
    pub viseme_mapping: Option<String>,
    pub motion_library: Option<String>,
    /// Semantic retriever identity when the reflex profile is active.
    pub retriever_version: Option<String>,
    pub embedding_model: Option<String>,
    /// Provider/model **identifiers** (names only, never keys or endpoints).
    pub thinking_provider: Option<String>,
    pub thinking_model: Option<String>,
    pub tts_provider: Option<String>,
    pub tts_model: Option<String>,
}

/// Adapter health as captured for the bundle. Error strings are redacted.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SupportAdapterHealth {
    pub audio_error: Option<String>,
    pub avatar_error: Option<String>,
    pub stream_error: Option<String>,
    pub muted: bool,
}

/// Bounded resource/retention counters (issues #51/#52/#53) plus content
/// queue depth and generation activity. Counters only — no contents.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SupportResourceCounters {
    pub content_queue_len: usize,
    pub scheduler_active: usize,
    pub scheduler_terminal_retained: usize,
    pub scheduler_terminal_evicted: usize,
    pub cooldowns: usize,
    pub scheduler_high_water_cooldowns: usize,
    pub scheduler_cooldowns_expired: u64,
    pub telemetry_retained: usize,
    pub telemetry_evicted: u64,
    pub audit_retained: usize,
    pub audit_high_water: usize,
    pub audit_evicted: u64,
    pub rate_limit_sources: usize,
    pub rate_limit_sources_rejected: u64,
    pub causal_traces_retained: usize,
    pub causal_traces_high_water: usize,
    pub causal_traces_evicted: u64,
    pub hot_cache_pinned_resident: usize,
    pub hot_cache_dynamic_resident: usize,
    pub hot_cache_estimated_bytes: usize,
    pub hot_cache_hits: u64,
    pub hot_cache_misses: u64,
    pub hot_cache_evictions: u64,
    pub generation_pending: usize,
    pub generation_in_flight: usize,
    pub generation_saturated: u64,
    pub generation_completed: u64,
    pub generation_failed: u64,
    /// Generative budget accounting (issue #69), bounded to the six fixed
    /// budget types; empty when the budget feature is disabled.
    #[serde(default)]
    pub generative_budget: Vec<GenerativeBudgetRecord>,
}

/// One redacted audit summary line (bounded decision text; never a payload).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SupportAuditSummary {
    pub category: String,
    pub decision: String,
}

/// The generated bundle: a versioned manifest plus the allowlisted sections.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SupportBundle {
    pub manifest: SupportBundleManifest,
    pub identity: SupportIdentity,
    pub adapter_health: SupportAdapterHealth,
    pub resources: SupportResourceCounters,
    /// Recent correlation-id references for incident expansion (bounded).
    pub recent_correlations: Vec<String>,
    /// Redacted one-line-per-stage causal timelines (bounded).
    pub causal_timelines: Vec<String>,
    /// Redacted audit decision summaries, newest last (bounded).
    pub audit_summaries: Vec<SupportAuditSummary>,
}

/// Serializes the identity and sections canonically and returns the SHA-256
/// digest used for the manifest integrity hash.
fn bundle_body_sha256(identity: &SupportIdentity, bundle: &SupportBundle) -> String {
    let body = serde_json::to_vec(&(
        identity,
        &bundle.adapter_health,
        &bundle.resources,
        &bundle.recent_correlations,
        &bundle.causal_timelines,
        &bundle.audit_summaries,
    ))
    .unwrap_or_default();
    let digest = Sha256::digest(&body);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A redacted, bounded decision summary extracted from one audit record.
fn audit_summary(record: &AuditRecord) -> SupportAuditSummary {
    let category = match record.category {
        AuditCategory::Ingress => "ingress",
        AuditCategory::Authorization => "authorization",
        AuditCategory::Output => "output",
        AuditCategory::Generation => "generation",
        AuditCategory::Memory => "memory",
    };
    SupportAuditSummary {
        category: category.to_owned(),
        // Audit decisions are structured reason codes, not free text.
        decision: record.decision.clone(),
    }
}

/// Provenance captured at build/bundle time by the embedding binary.
#[derive(Debug, Clone, Default)]
pub struct SupportBundleProvenance {
    pub git_revision: String,
    pub git_dirty: Option<bool>,
    pub rustc_version: String,
    pub os: String,
    pub arch: String,
    /// Semantic index identity when the reflex profile is active.
    pub retriever_version: Option<String>,
    pub embedding_model: Option<String>,
    /// Provider/model **names** for the configured backends (no keys, no
    /// endpoints — endpoints can embed private infrastructure names).
    pub thinking_provider: Option<String>,
    pub thinking_model: Option<String>,
    pub tts_provider: Option<String>,
    pub tts_model: Option<String>,
}

impl SupportBundleProvenance {
    /// Best-effort build identity: git revision from `env!` (set by the
    /// embedding binary through `option_env!`-style build configuration) and
    /// the running rustc/OS/arch of this build.
    pub fn from_build_env() -> Self {
        let git_revision = option_env!("AIVTUBER_GIT_REVISION")
            .unwrap_or("unknown")
            .to_owned();
        Self {
            git_revision,
            git_dirty: None,
            rustc_version: option_env!("AIVTUBER_RUSTC_VERSION")
                .unwrap_or("unknown")
                .to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            retriever_version: None,
            embedding_model: None,
            thinking_provider: None,
            thinking_model: None,
            tts_provider: None,
            tts_model: None,
        }
    }
}

/// Assembles the bounded support bundle from runtime state. Construction is
/// cheap; [`SupportBundleBuilder::generate`] performs the capture.
pub struct SupportBundleBuilder {
    identity: SupportIdentity,
    max_timelines: usize,
    max_audit_summaries: usize,
}

impl SupportBundleBuilder {
    /// Start from the live composition identity. `config_fingerprint` is the
    /// deterministic non-secret fingerprint from issue #55.
    pub fn new(
        profile_name: &str,
        config_fingerprint: &str,
        provenance: SupportBundleProvenance,
    ) -> Self {
        Self {
            identity: SupportIdentity {
                git_revision: provenance.git_revision,
                git_dirty: provenance.git_dirty,
                rustc_version: provenance.rustc_version,
                os: provenance.os,
                arch: provenance.arch,
                composition_profile: profile_name.to_owned(),
                config_fingerprint: config_fingerprint.to_owned(),
                compiler_version: String::new(),
                voice_model: None,
                avatar_profile: None,
                viseme_mapping: None,
                motion_library: None,
                retriever_version: provenance.retriever_version,
                embedding_model: provenance.embedding_model,
                thinking_provider: provenance.thinking_provider,
                thinking_model: provenance.thinking_model,
                tts_provider: provenance.tts_provider,
                tts_model: provenance.tts_model,
            },
            max_timelines: DEFAULT_MAX_TIMELINES,
            max_audit_summaries: DEFAULT_MAX_AUDIT_SUMMARIES,
        }
    }

    /// Bound the number of redacted causal timelines. Clamped to
    /// [`MAX_ALLOWED_RECORDS`].
    pub fn with_max_timelines(mut self, max_timelines: usize) -> Self {
        self.max_timelines = max_timelines.min(MAX_ALLOWED_RECORDS);
        self
    }

    /// Bound the number of redacted audit summaries. Clamped to
    /// [`MAX_ALLOWED_RECORDS`].
    pub fn with_max_audit_summaries(mut self, max_audit_summaries: usize) -> Self {
        self.max_audit_summaries = max_audit_summaries.min(MAX_ALLOWED_RECORDS);
        self
    }

    /// Internal inspection used by tests to assert clamping.
    #[cfg(test)]
    pub(super) fn bounds(&self) -> (usize, usize) {
        (self.max_timelines, self.max_audit_summaries)
    }

    /// Capture the bundle. Reads only already-retained runtime state: no
    /// provider calls, no scheduler/control mutation, no retention growth.
    /// `generated_unix_ms` is supplied by the caller so the capture stays
    /// deterministic under the runtime's monotonic-clock discipline.
    pub fn generate<R: RoutePlanner>(
        mut self,
        app: &crate::ProductionApp<R>,
        generated_unix_ms: u64,
    ) -> SupportBundle {
        // Asset-pack/compiler versions come from the live compatibility
        // snapshot the performer's AssetStore was built with (issue #68:
        // runtime/build/config identity). Names only — no paths.
        let compatibility = app.performer().assets().runtime().clone();
        self.identity.compiler_version = compatibility.compiler_version.clone();
        self.identity.voice_model = compatibility.voice_model.clone();
        self.identity.avatar_profile = compatibility.avatar_profile.clone();
        self.identity.viseme_mapping = compatibility.viseme_mapping.clone();
        self.identity.motion_library = compatibility.motion_library.clone();

        let health: &crate::AdapterHealth = app.health();
        let redactor = app.security().redactor();
        let adapter_health = SupportAdapterHealth {
            audio_error: health
                .audio_error
                .as_deref()
                .map(|text| redact_app_text(&redactor.redact(text))),
            avatar_error: health
                .avatar_error
                .as_deref()
                .map(|text| redact_app_text(&redactor.redact(text))),
            stream_error: health
                .stream_error
                .as_deref()
                .map(|text| redact_app_text(&redactor.redact(text))),
            muted: app.is_muted(),
        };

        let retention = app.retention_snapshot();
        let scheduler = &retention.scheduler;
        let generation = retention.generation.unwrap_or_default();
        let resources = SupportResourceCounters {
            content_queue_len: app.content_queue_len(),
            scheduler_active: scheduler.active,
            scheduler_terminal_retained: scheduler.terminal_retained,
            scheduler_terminal_evicted: scheduler.terminal_evicted,
            cooldowns: scheduler.cooldowns,
            scheduler_high_water_cooldowns: scheduler.cooldowns_high_water,
            scheduler_cooldowns_expired: scheduler.cooldowns_expired,
            telemetry_retained: retention.telemetry.retained,
            telemetry_evicted: retention.telemetry.evicted,
            audit_retained: retention.security.audit_retained,
            audit_high_water: retention.security.audit_high_water,
            audit_evicted: retention.security.audit_evicted,
            rate_limit_sources: retention.security.rate_limit_sources,
            rate_limit_sources_rejected: retention.security.rate_limit_sources_rejected,
            causal_traces_retained: retention.causal_traces.retained,
            causal_traces_high_water: retention.causal_traces.high_water,
            causal_traces_evicted: retention.causal_traces.evicted,
            hot_cache_pinned_resident: retention.hot_cache.pinned_resident,
            hot_cache_dynamic_resident: retention.hot_cache.dynamic_resident,
            hot_cache_estimated_bytes: retention.hot_cache.estimated_bytes,
            hot_cache_hits: retention.hot_cache.hits,
            hot_cache_misses: retention.hot_cache.misses,
            hot_cache_evictions: retention.hot_cache.evictions,
            generation_pending: generation.pending,
            generation_in_flight: generation.in_flight,
            generation_saturated: generation.saturated,
            generation_completed: generation.completed,
            generation_failed: generation.failed,
            generative_budget: app.generative_budget_snapshot(generated_unix_ms),
        };

        // Recent incident references from the security runtime's bounded
        // correlation registry (issue #68 feed) — ids only.
        let recent_correlations: Vec<String> = app
            .security()
            .recent_correlations(self.max_timelines.max(1))
            .iter()
            .map(|entry| entry.correlation_id.to_owned())
            .collect();

        // Redacted timelines: only the collector's redacted_timeline text
        // enters the bundle; payloads never do.
        let causal_timelines: Vec<String> = app
            .causal_traces()
            .recent_correlation_ids(self.max_timelines)
            .into_iter()
            .filter_map(|correlation_id| {
                app.causal_traces()
                    .trace_for_correlation(correlation_id)
                    .map(|trace| trace.redacted_timeline())
            })
            .collect();

        // Audit summaries: the newest retained records, as category + decision
        // pairs. Detail strings are intentionally not exported; the shared
        // redactor has already applied to every stored record.
        let audit_summaries: Vec<SupportAuditSummary> = app
            .security()
            .audit()
            .iter()
            .rev()
            .take(self.max_audit_summaries)
            .rev()
            .map(audit_summary)
            .collect();

        // The shared redactor has already been applied to every stored
        // record and health string at write time; the URL mask above is a
        // second, export-time pass for infrastructure names.
        let identity = self.identity;
        let manifest_probe = SupportBundle {
            manifest: SupportBundleManifest {
                schema_version: SUPPORT_BUNDLE_SCHEMA_VERSION,
                generated_unix_ms,
                bundle_sha256: String::new(),
            },
            identity: identity.clone(),
            adapter_health: adapter_health.clone(),
            resources: resources.clone(),
            recent_correlations: recent_correlations.clone(),
            causal_timelines: causal_timelines.clone(),
            audit_summaries: audit_summaries.clone(),
        };
        let bundle_sha256 = bundle_body_sha256(&identity, &manifest_probe);

        SupportBundle {
            manifest: SupportBundleManifest {
                schema_version: SUPPORT_BUNDLE_SCHEMA_VERSION,
                generated_unix_ms,
                bundle_sha256,
            },
            identity,
            adapter_health,
            resources,
            recent_correlations,
            causal_timelines,
            audit_summaries,
        }
    }
}

/// Adapter error strings can embed provider endpoint text or configured
/// secrets. The shared `SecretRedactor` runs first (record-time secrets), and
/// this helper additionally masks anything that looks like an endpoint URL,
/// which can name private infrastructure.
fn redact_app_text(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            if word.starts_with("http://")
                || word.starts_with("https://")
                || word.starts_with("ws://")
                || word.starts_with("wss://")
            {
                "[ENDPOINT]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Convenience wrapper that captures the provenance available to the daemon
/// binary and generates the bundle in one call.
pub fn generate_support_bundle<R: RoutePlanner>(
    app: &crate::ProductionApp<R>,
    profile_name: &str,
    config_fingerprint: &str,
    generated_unix_ms: u64,
) -> SupportBundle {
    let provenance = SupportBundleProvenance::from_build_env();
    SupportBundleBuilder::new(profile_name, config_fingerprint, provenance)
        .generate(app, generated_unix_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_schema_version_is_stable() {
        assert_eq!(SUPPORT_BUNDLE_SCHEMA_VERSION, "0.1.0");
    }

    #[test]
    fn redact_app_text_masks_endpoints() {
        let text = "connect failed to https://tts.internal.example/v1 after 3 tries";
        let redacted = redact_app_text(text);
        assert!(!redacted.contains("tts.internal.example"));
        assert!(redacted.contains("[ENDPOINT]"));
        assert!(redacted.contains("after 3 tries"));
    }

    #[test]
    fn builder_bounds_are_clamped() {
        let provenance = SupportBundleProvenance::default();
        let builder = SupportBundleBuilder::new("cached", "fp-test", provenance)
            .with_max_timelines(10_000)
            .with_max_audit_summaries(10_000);
        assert_eq!(builder.bounds(), (MAX_ALLOWED_RECORDS, MAX_ALLOWED_RECORDS));
    }

    #[test]
    fn provenance_defaults_are_not_secret_shaped() {
        let provenance = SupportBundleProvenance::from_build_env();
        assert_eq!(provenance.git_revision, "unknown");
        assert!(provenance.thinking_model.is_none());
        assert!(provenance.tts_provider.is_none());
    }
}
