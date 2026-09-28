use crate::{GenerationExecutionConfig, GenerationExecutionSnapshot};
use aivtuber_adaptation::{
    AdaptationRetentionConfig, AdaptationRetentionMetrics, WorkingMemoryConfig,
    WorkingMemoryRetentionMetrics,
};
use aivtuber_asset_store::{HotCacheConfig, HotCacheMetrics};
use aivtuber_runtime::{
    CachedPlaybackConfig, CachedPlaybackRetentionMetrics, SecurityRetentionMetrics,
    SecurityRuntimeConfig,
};
use aivtuber_scheduler::{HistoryPolicy, SchedulerConfig, SchedulerMetrics};
use aivtuber_telemetry::{
    CausalTraceRetentionConfig, CausalTraceRetentionMetrics, TelemetryRetentionConfig,
    TelemetryRetentionMetrics,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRetentionPolicy {
    pub max_telemetry_events: usize,
    pub max_audit_records: usize,
    pub max_rate_limit_sources: usize,
    pub rate_limit_source_ttl_ms: u64,
    pub max_scheduler_history: usize,
    pub max_hot_generated_assets: usize,
    pub max_hot_generated_bytes: usize,
    pub max_promotion_metadata: usize,
    pub max_cached_variant_groups: usize,
    pub max_working_memory_entries: usize,
    pub max_memory_compaction_records: usize,
    pub max_adaptation_feedback_assets: usize,
    pub max_adaptation_recent_groups: usize,
    pub max_adaptation_decisions: usize,
    pub generation_queue_capacity: usize,
    pub max_causal_traces: usize,
}

impl Default for RuntimeRetentionPolicy {
    fn default() -> Self {
        Self {
            max_telemetry_events: 4_096,
            max_audit_records: 4_096,
            max_rate_limit_sources: 4_096,
            rate_limit_source_ttl_ms: 10 * 60 * 1_000,
            max_scheduler_history: 1_024,
            max_hot_generated_assets: 1_024,
            max_hot_generated_bytes: 64 * 1024 * 1024,
            max_promotion_metadata: 4_096,
            max_cached_variant_groups: 256,
            max_working_memory_entries: 256,
            max_memory_compaction_records: 256,
            max_adaptation_feedback_assets: 1_024,
            max_adaptation_recent_groups: 256,
            max_adaptation_decisions: 1_024,
            generation_queue_capacity: 8,
            max_causal_traces: 1_024,
        }
    }
}

impl RuntimeRetentionPolicy {
    pub fn security_config(self, mut config: SecurityRuntimeConfig) -> SecurityRuntimeConfig {
        config.max_audit_records = self.max_audit_records;
        config.max_rate_limit_sources = self.max_rate_limit_sources;
        config.rate_limit_source_ttl_ms = self.rate_limit_source_ttl_ms;
        config
    }

    pub fn scheduler_config(self, mut config: SchedulerConfig) -> SchedulerConfig {
        config.history_capacity = self.max_scheduler_history;
        config.history_policy = HistoryPolicy::Ring;
        config
    }

    pub fn hot_cache_config(self) -> HotCacheConfig {
        HotCacheConfig {
            max_dynamic_assets: self.max_hot_generated_assets,
            max_dynamic_bytes: self.max_hot_generated_bytes,
            max_promotion_metadata: self.max_promotion_metadata,
        }
    }
    pub fn cached_playback_config(self, mut config: CachedPlaybackConfig) -> CachedPlaybackConfig {
        config.max_recent_variant_groups = self.max_cached_variant_groups;
        config
    }

    pub fn working_memory_config(self, mut config: WorkingMemoryConfig) -> WorkingMemoryConfig {
        config.max_entries = self.max_working_memory_entries;
        config.max_compaction_records = self.max_memory_compaction_records;
        config
    }

    pub fn adaptation_config(self) -> AdaptationRetentionConfig {
        AdaptationRetentionConfig {
            max_feedback_assets: self.max_adaptation_feedback_assets,
            max_recent_groups: self.max_adaptation_recent_groups,
            max_decisions: self.max_adaptation_decisions,
        }
    }

    pub fn telemetry_config(self) -> TelemetryRetentionConfig {
        TelemetryRetentionConfig {
            max_events: self.max_telemetry_events,
        }
    }

    /// Causal traces join the unified bounded retention policy (#65, #51):
    /// no separate retention contract is invented for traces.
    pub fn causal_trace_config(self) -> CausalTraceRetentionConfig {
        CausalTraceRetentionConfig {
            max_traces: self.max_causal_traces,
        }
    }

    pub fn generation_execution_config(self) -> GenerationExecutionConfig {
        GenerationExecutionConfig {
            queue_capacity: self.generation_queue_capacity,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRetentionSnapshot {
    pub telemetry: TelemetryRetentionMetrics,
    pub causal_traces: CausalTraceRetentionMetrics,
    pub security: SecurityRetentionMetrics,
    pub scheduler: SchedulerMetrics,
    pub hot_cache: HotCacheMetrics,
    pub cached_playback: CachedPlaybackRetentionMetrics,
    pub working_memory: Option<WorkingMemoryRetentionMetrics>,
    pub adaptation: Option<AdaptationRetentionMetrics>,
    pub generation: Option<GenerationExecutionSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_policy_derives_all_production_retention_configs() {
        let policy = RuntimeRetentionPolicy {
            max_telemetry_events: 11,
            max_audit_records: 12,
            max_rate_limit_sources: 13,
            rate_limit_source_ttl_ms: 14,
            max_scheduler_history: 15,
            max_hot_generated_assets: 16,
            max_hot_generated_bytes: 17,
            max_promotion_metadata: 18,
            max_cached_variant_groups: 19,
            max_working_memory_entries: 20,
            max_memory_compaction_records: 21,
            max_adaptation_feedback_assets: 22,
            max_adaptation_recent_groups: 23,
            max_adaptation_decisions: 24,
            generation_queue_capacity: 25,
            max_causal_traces: 26,
        };

        let security = policy.security_config(SecurityRuntimeConfig::default());
        assert_eq!(security.max_audit_records, 12);
        assert_eq!(security.max_rate_limit_sources, 13);
        assert_eq!(security.rate_limit_source_ttl_ms, 14);

        let scheduler = policy.scheduler_config(SchedulerConfig::replay());
        assert_eq!(scheduler.history_capacity, 15);
        assert_eq!(scheduler.history_policy, HistoryPolicy::Ring);

        let hot = policy.hot_cache_config();
        assert_eq!(hot.max_dynamic_assets, 16);
        assert_eq!(hot.max_dynamic_bytes, 17);
        assert_eq!(hot.max_promotion_metadata, 18);

        let playback = policy.cached_playback_config(CachedPlaybackConfig::default());
        assert_eq!(playback.max_recent_variant_groups, 19);

        let memory = policy.working_memory_config(WorkingMemoryConfig::default());
        assert_eq!(memory.max_entries, 20);
        assert_eq!(memory.max_compaction_records, 21);

        let adaptation = policy.adaptation_config();
        assert_eq!(adaptation.max_feedback_assets, 22);
        assert_eq!(adaptation.max_recent_groups, 23);
        assert_eq!(adaptation.max_decisions, 24);
        assert_eq!(policy.telemetry_config().max_events, 11);
        assert_eq!(policy.causal_trace_config().max_traces, 26);
        assert_eq!(policy.generation_execution_config().queue_capacity, 25);
    }
}
