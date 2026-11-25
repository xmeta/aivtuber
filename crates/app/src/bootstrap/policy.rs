use super::env::*;
use aivtuber_app::{
    IntentShadowPolicy, ProfileSummary, RoutePlanner, RuntimeRetentionPolicy,
    SHADOW_ORCHESTRATOR_SCHEMA_VERSION, ShadowOrchestratorConfig, ShadowPolicyIdentity,
    ShadowingRoutePlanner,
};
use aivtuber_asset_store::{AssetStore, RuntimeCompatibility};
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, EventKind, LocalControlIngress,
    OperatorCommandInput,
};
use aivtuber_reflex::SemanticIndex;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

pub(crate) type ShadowWrappedPlanner = (Box<dyn RoutePlanner>, Option<String>);

pub(crate) fn build_runtime_retention_policy() -> Result<RuntimeRetentionPolicy, Box<dyn Error>> {
    let defaults = RuntimeRetentionPolicy::default();
    let policy = RuntimeRetentionPolicy {
        max_telemetry_events: env_usize(
            "AIVTUBER_RETENTION_MAX_TELEMETRY_EVENTS",
            defaults.max_telemetry_events,
        )?,
        max_audit_records: env_usize(
            "AIVTUBER_RETENTION_MAX_AUDIT_RECORDS",
            defaults.max_audit_records,
        )?,
        max_rate_limit_sources: env_usize(
            "AIVTUBER_RETENTION_MAX_RATE_LIMIT_SOURCES",
            defaults.max_rate_limit_sources,
        )?,
        rate_limit_source_ttl_ms: env_u64(
            "AIVTUBER_RETENTION_RATE_LIMIT_SOURCE_TTL_MS",
            defaults.rate_limit_source_ttl_ms,
        )?,
        max_scheduler_history: env_usize(
            "AIVTUBER_RETENTION_MAX_SCHEDULER_HISTORY",
            defaults.max_scheduler_history,
        )?,
        max_hot_generated_assets: env_usize(
            "AIVTUBER_RETENTION_MAX_HOT_GENERATED_ASSETS",
            defaults.max_hot_generated_assets,
        )?,
        max_hot_generated_bytes: env_usize(
            "AIVTUBER_RETENTION_MAX_HOT_GENERATED_BYTES",
            defaults.max_hot_generated_bytes,
        )?,
        max_promotion_metadata: env_usize(
            "AIVTUBER_RETENTION_MAX_PROMOTION_METADATA",
            defaults.max_promotion_metadata,
        )?,
        max_cached_variant_groups: env_usize(
            "AIVTUBER_RETENTION_MAX_CACHED_VARIANT_GROUPS",
            defaults.max_cached_variant_groups,
        )?,
        max_working_memory_entries: env_usize(
            "AIVTUBER_MEMORY_MAX_ENTRIES",
            defaults.max_working_memory_entries,
        )?,
        max_memory_compaction_records: env_usize(
            "AIVTUBER_MEMORY_MAX_COMPACTION_RECORDS",
            defaults.max_memory_compaction_records,
        )?,
        max_memory_links: env_usize("AIVTUBER_MEMORY_MAX_LINKS", defaults.max_memory_links)?,
        max_memory_links_per_write: env_usize(
            "AIVTUBER_MEMORY_MAX_LINKS_PER_WRITE",
            defaults.max_memory_links_per_write,
        )?,
        max_adaptation_feedback_assets: env_usize(
            "AIVTUBER_RETENTION_MAX_ADAPTATION_FEEDBACK_ASSETS",
            defaults.max_adaptation_feedback_assets,
        )?,
        max_adaptation_recent_groups: env_usize(
            "AIVTUBER_RETENTION_MAX_ADAPTATION_RECENT_GROUPS",
            defaults.max_adaptation_recent_groups,
        )?,
        max_adaptation_decisions: env_usize(
            "AIVTUBER_RETENTION_MAX_ADAPTATION_DECISIONS",
            defaults.max_adaptation_decisions,
        )?,
        generation_queue_capacity: env_usize(
            "AIVTUBER_RETENTION_GENERATION_QUEUE_CAPACITY",
            defaults.generation_queue_capacity,
        )?,
        max_causal_traces: env_usize(
            "AIVTUBER_RETENTION_MAX_CAUSAL_TRACES",
            defaults.max_causal_traces,
        )?,
    };
    if policy.max_telemetry_events == 0
        || policy.max_audit_records == 0
        || policy.max_rate_limit_sources == 0
        || policy.rate_limit_source_ttl_ms == 0
        || policy.max_scheduler_history == 0
        || policy.max_hot_generated_assets == 0
        || policy.max_hot_generated_bytes == 0
        || policy.max_promotion_metadata == 0
        || policy.max_cached_variant_groups == 0
        || policy.max_working_memory_entries == 0
        || policy.max_memory_compaction_records == 0
        || policy.max_memory_links == 0
        || policy.max_memory_links_per_write == 0
        || policy.max_adaptation_feedback_assets == 0
        || policy.max_adaptation_recent_groups == 0
        || policy.max_adaptation_decisions == 0
        || policy.generation_queue_capacity == 0
        || policy.max_causal_traces == 0
    {
        return Err(io::Error::other("runtime retention limits must be positive").into());
    }
    // The store must be able to hold one maximum-size write, or the store
    // bound would drop an explicit supersession the same write just had
    // accepted (WorkingMemory::new refuses this shape; fail here with the
    // env-var names instead).
    if policy.max_memory_links < policy.max_memory_links_per_write {
        return Err(io::Error::other(
            "AIVTUBER_MEMORY_MAX_LINKS must cover one maximum-size write \
             (>= AIVTUBER_MEMORY_MAX_LINKS_PER_WRITE)",
        )
        .into());
    }
    Ok(policy)
}

pub(crate) fn runtime_avatar_authority()
-> Result<aivtuber_domain::AuthenticatedControl, Box<dyn Error>> {
    let secret = fixed_control_secret("AIVTUBER_CONTROL_SECRET")?;
    let ingress = LocalControlIngress::new(
        "local-runtime",
        "runtime:motor",
        AuthorizationMethod::SignedLocalApi,
        BTreeSet::from([Capability::AvatarControl]),
        ControlSecret::new(secret),
    )?;
    let command = ingress.authenticate(
        OperatorCommandInput {
            event_id: "bootstrap-avatar-authority".to_owned(),
            correlation_id: "bootstrap-runtime".to_owned(),
            sequence: 0,
            observed_at: "1970-01-01T00:00:00Z".to_owned(),
            action: "avatar.control".to_owned(),
            payload: BTreeMap::new(),
        },
        &secret,
    )?;
    Ok(command.authority().clone())
}

pub(crate) fn fixed_hex_key(name: &str) -> Result<[u8; 32], Box<dyn Error>> {
    let value = env::var(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required for actor pseudonymization"),
        )
    })?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain exactly 64 hexadecimal characters"),
        )
        .into());
    }

    let mut key = [0_u8; 32];
    for (index, slot) in key.iter_mut().enumerate() {
        let offset = index * 2;
        *slot = u8::from_str_radix(&value[offset..offset + 2], 16).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} contains invalid hexadecimal key material: {error}"),
            )
        })?;
    }
    Ok(key)
}

pub(crate) fn fill_random(bytes: &mut [u8; 32]) {
    getrandom::fill(bytes).expect("OS entropy source failed");
}

pub(crate) fn fixed_control_secret(name: &str) -> Result<[u8; 32], Box<dyn Error>> {
    let value = env::var(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required when VTube Studio output is enabled"),
        )
    })?;
    let bytes = value.as_bytes();
    if bytes.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain exactly 32 UTF-8 bytes"),
        )
        .into());
    }

    let mut secret = [0_u8; 32];
    secret.copy_from_slice(bytes);
    Ok(secret)
}

pub(crate) fn wrap_with_shadow_orchestration(
    active: Box<dyn RoutePlanner>,
    startup_summary: &ProfileSummary,
) -> Result<ShadowWrappedPlanner, Box<dyn Error>> {
    let enabled = env_bool("AIVTUBER_SHADOW_ENABLED", false)?;
    if !enabled {
        return Ok((active, None));
    }

    let defaults = ShadowOrchestratorConfig::default();
    let config = ShadowOrchestratorConfig {
        schema_version: SHADOW_ORCHESTRATOR_SCHEMA_VERSION.to_owned(),
        enabled: true,
        sample_rate_per_10k: env_u32("AIVTUBER_SHADOW_SAMPLE_RATE", defaults.sample_rate_per_10k)?,
        seed: env_u64("AIVTUBER_SHADOW_SEED", 0)?,
        event_kinds: parse_shadow_event_kinds()?,
        max_concurrent: env_usize("AIVTUBER_SHADOW_MAX_CONCURRENT", defaults.max_concurrent)?,
        queue_capacity: env_usize("AIVTUBER_SHADOW_QUEUE_CAPACITY", defaults.queue_capacity)?,
        deadline_ms: env_u64("AIVTUBER_SHADOW_DEADLINE_MS", defaults.deadline_ms)?,
        allow_provider_calls: env_bool("AIVTUBER_SHADOW_ALLOW_PROVIDER", false)?,
        charge_shadow_to_budget: env_bool("AIVTUBER_SHADOW_CHARGE_BUDGET", false)?,
    };
    config.validate().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("AIVTUBER_SHADOW_* configuration invalid: {error}"),
        )
    })?;

    let policy_id = env_string("AIVTUBER_SHADOW_POLICY_ID", "intent-shadow");
    let active_identity = ShadowPolicyIdentity {
        policy_id: "active-production".to_owned(),
        policy_version: env_string("AIVTUBER_SHADOW_ACTIVE_VERSION", "v1"),
        config_fingerprint: startup_summary.config_fingerprint.clone(),
        runtime_profile: startup_summary.profile.clone(),
        dataset_id: None,
    };
    let shadow_identity = ShadowPolicyIdentity {
        policy_id: policy_id.clone(),
        policy_version: env_string("AIVTUBER_SHADOW_POLICY_VERSION", "v1"),
        config_fingerprint: env_string(
            "AIVTUBER_SHADOW_CONFIG_FINGERPRINT",
            &startup_summary.config_fingerprint,
        ),
        runtime_profile: startup_summary.profile.clone(),
        dataset_id: env_optional("AIVTUBER_SHADOW_DATASET_ID"),
    };

    // No production provider bridge is constructed here. Live provider
    // evaluation stays disabled by default and is not reachable from the
    // daemon; enabling the flag alone still performs no network call because
    // `provider` is `None`.
    let planner = ShadowingRoutePlanner::with_orchestrator(
        active,
        IntentShadowPolicy,
        active_identity,
        shadow_identity,
        None,
        config.clone(),
    )?;

    let summary = format!(
        "shadow_orchestration: enabled=true policy={policy_id} schema={} sample_rate_per_10k={} max_concurrent={} queue_capacity={} deadline_ms={} provider_calls={} charge_budget={}",
        config.schema_version,
        config.sample_rate_per_10k,
        config.max_concurrent,
        config.queue_capacity,
        config.deadline_ms,
        config.allow_provider_calls,
        config.charge_shadow_to_budget,
    );
    Ok((Box::new(planner) as Box<dyn RoutePlanner>, Some(summary)))
}

/// Parse the optional `AIVTUBER_SHADOW_EVENT_KINDS` comma-separated filter.
/// An empty or unset value means every kind is eligible.
pub(crate) fn parse_shadow_event_kinds() -> Result<Vec<EventKind>, Box<dyn Error>> {
    let Some(raw) = env_optional("AIVTUBER_SHADOW_EVENT_KINDS") else {
        return Ok(Vec::new());
    };
    let mut kinds = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let kind: EventKind = serde_json::from_value(serde_json::Value::String(token.to_owned()))
            .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("AIVTUBER_SHADOW_EVENT_KINDS contains unknown event kind: {token}"),
            )
        })?;
        kinds.push(kind);
    }
    Ok(kinds)
}

/// Starter-semantic index over the pack's usable Performance Assets, shared by
/// the reflex profiles' planner and the chat-burst retrieval score.
pub(crate) fn build_starter_semantic_index(
    compatibility: &RuntimeCompatibility,
) -> Result<SemanticIndex, Box<dyn Error>> {
    let semantic_pack_root = env_path("AIVTUBER_PACK_ROOT")
        .unwrap_or_else(|| PathBuf::from("examples/starter-reaction-pack"));
    let mut semantic_assets = AssetStore::new(
        semantic_pack_root.join("descriptors"),
        compatibility.clone(),
    );
    semantic_assets.index_local()?;
    Ok(aivtuber_app::build_semantic_index_from_asset_store(
        &semantic_assets,
        &aivtuber_app::AssetSemanticIndexConfig {
            retriever_version: "production-v1".to_owned(),
            embedding_model: "starter-semantic".to_owned(),
            embedding_model_version: "1".to_owned(),
        },
    )?)
}
