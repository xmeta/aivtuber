//! Generative resource budgets and budget-aware graceful degradation
//! (issue #69).
//!
//! This module owns *resource-admission policy*: how much generative work is
//! allowed by call/unit/cost/priority budgets and which deterministic
//! fallback is selected when that policy denies generation. It deliberately
//! does not own the bounded execution mechanism (worker/queue capacity is
//! issue #121's `GenerationExecutor`): budget admission happens *before*
//! submit, so a budget denial never allocates provider work that must then be
//! cancelled merely to enforce the budget.
//!
//! Denial reasons stay distinct from deadline exhaustion (#67) and execution
//! saturation (#121): `resource_budget_exhausted` is never conflated with
//! `deadline_exhausted` or `capacity_saturated`.
//!
//! Accounting uses logical time (`at_ms`) like #67 deadlines, so recorded
//! admission decisions replay identically.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

/// Provider-neutral budget policy (issue #69). All limits are optional;
/// `None` means "not enforced". Zero-valued limits are invalid (they would
/// deny everything and are treated as configuration mistakes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerativeBudgetPolicy {
    /// Master switch. Disabled (default) means the governor admits
    /// everything and records nothing, preserving existing behavior.
    pub enabled: bool,
    /// Maximum concurrently in-flight generation requests.
    pub max_concurrent_generations: Option<u32>,
    /// Maximum LLM calls per rolling interval.
    pub max_llm_calls_per_interval: Option<u32>,
    /// Maximum TTS calls per rolling interval.
    pub max_tts_calls_per_interval: Option<u32>,
    /// Maximum LLM input/output units per rolling interval, where adapters
    /// expose them. Provider-neutral: the unit definition is recorded by the
    /// adapter identity, never invented here.
    pub max_llm_units_per_interval: Option<u64>,
    /// Maximum TTS characters per rolling interval.
    pub max_tts_chars_per_interval: Option<u64>,
    /// Optional estimated-cost ceiling in milliunits (1/1000 of a currency
    /// unit). Estimates only: never a claim about actual provider billing.
    pub max_estimated_cost_milliunits_per_interval: Option<u64>,
    /// Version of the pricing inputs used for currency estimates. Required
    /// (and validated non-empty) when a cost budget is configured.
    pub pricing_input_version: Option<&'static str>,
    /// Length of the rolling interval in milliseconds.
    pub interval_ms: u64,
    /// Share (percent, 0..=100) of each call budget reserved for trusted
    /// high-priority deadline classes (HighPriority/StrongReaction, derived
    /// from the #67 deadline class, never from untrusted content fields).
    pub high_priority_reserve_percent: u32,
    /// Whether shadow evaluation (#64) consumes billable budget. Default is
    /// `false`: shadow usage never silently doubles provider spend.
    pub charge_shadow_to_budget: bool,
}

impl Default for GenerativeBudgetPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent_generations: None,
            max_llm_calls_per_interval: None,
            max_tts_calls_per_interval: None,
            max_llm_units_per_interval: None,
            max_tts_chars_per_interval: None,
            max_estimated_cost_milliunits_per_interval: None,
            pricing_input_version: None,
            interval_ms: 60_000,
            high_priority_reserve_percent: 0,
            charge_shadow_to_budget: false,
        }
    }
}

impl GenerativeBudgetPolicy {
    pub fn validate(self) -> Result<Self, BudgetConfigError> {
        if self.interval_ms == 0 {
            return Err(BudgetConfigError::new(
                "interval_ms",
                "must be greater than zero",
            ));
        }
        for (name, limit) in [
            (
                "max_concurrent_generations",
                self.max_concurrent_generations,
            ),
            (
                "max_llm_calls_per_interval",
                self.max_llm_calls_per_interval,
            ),
            (
                "max_tts_calls_per_interval",
                self.max_tts_calls_per_interval,
            ),
        ] {
            if limit == Some(0) {
                return Err(BudgetConfigError::new(
                    name,
                    "must be greater than zero when enforced",
                ));
            }
        }
        for (name, limit) in [
            (
                "max_llm_units_per_interval",
                self.max_llm_units_per_interval,
            ),
            (
                "max_tts_chars_per_interval",
                self.max_tts_chars_per_interval,
            ),
            (
                "max_estimated_cost_milliunits_per_interval",
                self.max_estimated_cost_milliunits_per_interval,
            ),
        ] {
            if limit == Some(0) {
                return Err(BudgetConfigError::new(
                    name,
                    "must be greater than zero when enforced",
                ));
            }
        }
        if self.max_estimated_cost_milliunits_per_interval.is_some()
            && self
                .pricing_input_version
                .is_none_or(|version| version.trim().is_empty())
        {
            return Err(BudgetConfigError::new(
                "pricing_input_version",
                "is required when a cost budget is configured",
            ));
        }
        if self.high_priority_reserve_percent > 100 {
            return Err(BudgetConfigError::new(
                "high_priority_reserve_percent",
                "must be within 0..=100",
            ));
        }
        Ok(self)
    }

    fn reserve_share(self, limit: u64) -> u64 {
        limit
            .saturating_mul(u64::from(self.high_priority_reserve_percent))
            .saturating_div(100)
    }

    /// Ordinary (non-high-priority) admissions may consume at most this much
    /// of `limit`; the remainder stays reserved for high priority.
    fn ordinary_share(self, limit: u64) -> u64 {
        limit.saturating_sub(self.reserve_share(limit))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetConfigError {
    field: &'static str,
    message: &'static str,
}

impl BudgetConfigError {
    fn new(field: &'static str, message: &'static str) -> Self {
        Self { field, message }
    }

    pub fn field(&self) -> &'static str {
        self.field
    }
}

impl std::fmt::Display for BudgetConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "budget policy field {}: {}", self.field, self.message)
    }
}

/// Trusted priority classes that may spend the high-priority reserve. Derived
/// exclusively from the validated #67 deadline class, so untrusted content
/// cannot self-escalate into the reserved share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPriorityClass {
    High,
    #[default]
    Ordinary,
    /// Shadow evaluation (#64): never charges billable budget unless
    /// explicitly enabled via `charge_shadow_to_budget`.
    Shadow,
}

/// Why a generative request was denied by the budget governor. Deliberately
/// distinct from #67 `DeadlineExhaustionReason` and #121 `capacity_saturated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDenialReason {
    ConcurrentBudgetExhausted,
    LlmCallBudgetExhausted,
    TtsCallBudgetExhausted,
    LlmUnitBudgetExhausted,
    TtsCharBudgetExhausted,
    CostBudgetExhausted,
}

impl BudgetDenialReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConcurrentBudgetExhausted => "concurrent_budget_exhausted",
            Self::LlmCallBudgetExhausted => "llm_call_budget_exhausted",
            Self::TtsCallBudgetExhausted => "tts_call_budget_exhausted",
            Self::LlmUnitBudgetExhausted => "llm_unit_budget_exhausted",
            Self::TtsCharBudgetExhausted => "tts_char_budget_exhausted",
            Self::CostBudgetExhausted => "cost_budget_exhausted",
        }
    }
}

/// What is being admitted and what it is expected to cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetAdmission {
    pub priority: BudgetPriorityClass,
    /// Estimated character count of the planned TTS utterance, when known.
    pub estimated_tts_chars: u64,
    /// Estimated cost in milliunits, when a pricing estimate exists.
    pub estimated_cost_milliunits: u64,
}

impl BudgetAdmission {
    pub fn ordinary() -> Self {
        Self {
            priority: BudgetPriorityClass::Ordinary,
            ..Self::default()
        }
    }

    pub fn high_priority() -> Self {
        Self {
            priority: BudgetPriorityClass::High,
            ..Self::default()
        }
    }

    pub fn shadow() -> Self {
        Self {
            priority: BudgetPriorityClass::Shadow,
            ..Self::default()
        }
    }

    pub fn with_estimates(mut self, tts_chars: u64, cost_milliunits: u64) -> Self {
        self.estimated_tts_chars = tts_chars;
        self.estimated_cost_milliunits = cost_milliunits;
        self
    }
}

/// A granted budget reservation. Settle it with observed usage when the
/// generation completes, or release it when the request is rejected or
/// cancelled, so enforcing the budget never strands allocated provider work.
///
/// Dropping an unfinished reservation still returns its concurrency slot
/// (via a shared atomic), so a forgotten reservation can only ever leak a
/// call-ledger entry that expires with the rolling window — never a
/// permanently stuck in-flight charge.
#[derive(Debug, Clone)]
pub struct BudgetReservation {
    granted_at_ms: u64,
    charged_tts_chars: u64,
    charged_cost_milliunits: u64,
    in_flight_slot: Option<Arc<AtomicU32>>,
    finished: bool,
}

impl BudgetReservation {
    pub fn granted_at_ms(&self) -> u64 {
        self.granted_at_ms
    }

    /// Record actual consumption (TTS characters / estimated cost observed
    /// after generation). Unit and cost ledgers are topped up to actual
    /// observed usage so estimates never under-count.
    pub fn settle(
        &mut self,
        governor: &mut GenerativeBudgetGovernor,
        tts_chars: u64,
        estimated_cost_milliunits: u64,
    ) {
        if self.finish() {
            governor.settle_units(
                tts_chars,
                estimated_cost_milliunits,
                self.charged_tts_chars,
                self.charged_cost_milliunits,
                self.in_flight_slot.take(),
            );
        }
    }

    /// Return the reservation without charging unit/cost ledgers, recording
    /// the slot as rejected/degraded (call ledger entries expire via the
    /// rolling window itself).
    pub fn release(&mut self, governor: &mut GenerativeBudgetGovernor) {
        if self.finish() {
            governor.release_slot(self.in_flight_slot.take());
        }
    }

    fn finish(&mut self) -> bool {
        let first = !self.finished;
        self.finished = true;
        first
    }
}

impl Drop for BudgetReservation {
    fn drop(&mut self) {
        // Unfinished drops return the concurrency slot without any
        // rejected/degraded accounting: the slot must never leak. Only an
        // explicit `release` counts as a rejected/degraded attempt.
        if let Some(slot) = self.in_flight_slot.take() {
            slot.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Bounded per-budget-type accounting record (issue #69 accounting section).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetType {
    Concurrent,
    LlmCalls,
    TtsCalls,
    LlmUnits,
    TtsChars,
    EstimatedCost,
}

impl BudgetType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Concurrent => "concurrent",
            Self::LlmCalls => "llm_calls",
            Self::TtsCalls => "tts_calls",
            Self::LlmUnits => "llm_units",
            Self::TtsChars => "tts_chars",
            Self::EstimatedCost => "estimated_cost",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerativeBudgetRecord {
    pub budget_type: BudgetType,
    pub limit: Option<u64>,
    pub consumed: u64,
    pub remaining: Option<u64>,
    pub rejected_or_degraded: u64,
}

/// Rolling-window budget governor driven by logical time.
#[derive(Debug, Default)]
pub struct GenerativeBudgetGovernor {
    policy: GenerativeBudgetPolicy,
    llm_calls: VecDeque<u64>,
    tts_calls: VecDeque<u64>,
    llm_units: VecDeque<u64>,
    tts_chars: VecDeque<u64>,
    cost_milliunits: VecDeque<u64>,
    /// Shared with granted reservations so drops always return slots.
    in_flight: Arc<AtomicU32>,
    in_flight_high_water: u32,
    denied_total: u64,
    rejected_degraded: u64,
    denials_by_reason: Vec<(BudgetDenialReason, u64)>,
}

impl GenerativeBudgetGovernor {
    pub fn new(policy: GenerativeBudgetPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    pub fn policy(&self) -> &GenerativeBudgetPolicy {
        &self.policy
    }

    /// True when the governor enforces limits; when disabled, admission
    /// always succeeds and no accounting state changes (pre-#69 behavior).
    pub fn is_enforced(&self) -> bool {
        self.policy.enabled
    }

    /// Try to admit one generation request before any provider work happens.
    /// On success returns a reservation that must later be `settle`d with
    /// observed usage or `release`d.
    pub fn try_admit(
        &mut self,
        admission: BudgetAdmission,
        now_ms: u64,
    ) -> Result<BudgetReservation, BudgetDenialReason> {
        // Disabled governors admit without accounting.
        if !self.is_enforced() {
            return Ok(Self::uncharged_reservation(now_ms));
        }
        // Shadow evaluation never consumes billable budget by default; when
        // explicitly charged it flows through the same ledger as ordinary
        // traffic so it cannot escape accounting.
        if admission.priority == BudgetPriorityClass::Shadow && !self.policy.charge_shadow_to_budget
        {
            return Ok(Self::uncharged_reservation(now_ms));
        }

        self.trim_windows(now_ms);

        let high = admission.priority == BudgetPriorityClass::High;

        if let Some(limit) = self.policy.max_concurrent_generations
            && self.in_flight.load(Ordering::Relaxed) >= limit
        {
            return Err(self.deny(BudgetDenialReason::ConcurrentBudgetExhausted, now_ms));
        }

        if let Some(limit) = self.policy.max_llm_calls_per_interval {
            let effective_limit = if high {
                u64::from(limit)
            } else {
                self.policy.ordinary_share(u64::from(limit))
            };
            if self.llm_calls.len() as u64 >= effective_limit {
                return Err(self.deny(BudgetDenialReason::LlmCallBudgetExhausted, now_ms));
            }
        }

        if let Some(limit) = self.policy.max_tts_calls_per_interval {
            let effective_limit = if high {
                u64::from(limit)
            } else {
                self.policy.ordinary_share(u64::from(limit))
            };
            if self.tts_calls.len() as u64 >= effective_limit {
                return Err(self.deny(BudgetDenialReason::TtsCallBudgetExhausted, now_ms));
            }
        }

        if let Some(limit) = self.policy.max_llm_units_per_interval {
            let consumed: u64 = self.llm_units.iter().sum();
            if consumed >= limit {
                return Err(self.deny(BudgetDenialReason::LlmUnitBudgetExhausted, now_ms));
            }
        }

        if let Some(limit) = self.policy.max_tts_chars_per_interval {
            let effective_limit = if high {
                limit
            } else {
                self.policy.ordinary_share(limit)
            };
            let consumed: u64 = self.tts_chars.iter().sum();
            if consumed.saturating_add(admission.estimated_tts_chars) > effective_limit {
                return Err(self.deny(BudgetDenialReason::TtsCharBudgetExhausted, now_ms));
            }
        }

        if let Some(limit) = self.policy.max_estimated_cost_milliunits_per_interval {
            let consumed: u64 = self.cost_milliunits.iter().sum();
            if consumed.saturating_add(admission.estimated_cost_milliunits) > limit {
                return Err(self.deny(BudgetDenialReason::CostBudgetExhausted, now_ms));
            }
        }

        // Grant: charge call ledgers, concurrency, and up-front estimates.
        let charged_tts_chars = if self.policy.max_tts_chars_per_interval.is_some() {
            admission.estimated_tts_chars
        } else {
            0
        };
        let charged_cost_milliunits = if self
            .policy
            .max_estimated_cost_milliunits_per_interval
            .is_some()
        {
            admission.estimated_cost_milliunits
        } else {
            0
        };
        if self.policy.max_llm_calls_per_interval.is_some() {
            self.llm_calls.push_back(now_ms);
        }
        if self.policy.max_tts_calls_per_interval.is_some() {
            self.tts_calls.push_back(now_ms);
        }
        if charged_tts_chars > 0 {
            self.tts_chars.push_back(charged_tts_chars);
        }
        if charged_cost_milliunits > 0 {
            self.cost_milliunits.push_back(charged_cost_milliunits);
        }
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        let in_flight_now = self.in_flight.load(Ordering::Relaxed);
        self.in_flight_high_water = self.in_flight_high_water.max(in_flight_now);

        Ok(BudgetReservation {
            granted_at_ms: now_ms,
            charged_tts_chars,
            charged_cost_milliunits,
            in_flight_slot: Some(Arc::clone(&self.in_flight)),
            finished: false,
        })
    }

    fn uncharged_reservation(now_ms: u64) -> BudgetReservation {
        BudgetReservation {
            granted_at_ms: now_ms,
            charged_tts_chars: 0,
            charged_cost_milliunits: 0,
            in_flight_slot: None,
            finished: false,
        }
    }

    fn settle_units(
        &mut self,
        tts_chars: u64,
        estimated_cost_milliunits: u64,
        already_charged_tts_chars: u64,
        already_charged_cost_milliunits: u64,
        in_flight_slot: Option<Arc<AtomicU32>>,
    ) {
        if let Some(slot) = in_flight_slot {
            slot.fetch_sub(1, Ordering::Relaxed);
        }
        let extra_tts_chars = tts_chars.saturating_sub(already_charged_tts_chars);
        let extra_cost = estimated_cost_milliunits.saturating_sub(already_charged_cost_milliunits);
        if extra_tts_chars > 0 && self.policy.max_tts_chars_per_interval.is_some() {
            self.tts_chars.push_back(extra_tts_chars);
        }
        if extra_cost > 0
            && self
                .policy
                .max_estimated_cost_milliunits_per_interval
                .is_some()
        {
            self.cost_milliunits.push_back(extra_cost);
        }
    }

    fn release_slot(&mut self, in_flight_slot: Option<Arc<AtomicU32>>) {
        if let Some(slot) = in_flight_slot {
            slot.fetch_sub(1, Ordering::Relaxed);
        }
        self.rejected_degraded = self.rejected_degraded.saturating_add(1);
    }

    /// Explicit release of a never-started reservation (denied downstream,
    /// cancelled, or stale): the slot returns to the budget without charging
    /// unit/cost ledgers.
    pub fn release_reservation(&mut self, reservation: &mut BudgetReservation) {
        reservation.release(self);
    }

    fn deny(&mut self, reason: BudgetDenialReason, _now_ms: u64) -> BudgetDenialReason {
        self.denied_total = self.denied_total.saturating_add(1);
        match self
            .denials_by_reason
            .iter_mut()
            .find(|(known, _)| *known == reason)
        {
            Some((_, count)) => *count = count.saturating_add(1),
            None => self.denials_by_reason.push((reason, 1)),
        }
        reason
    }

    fn trim_windows(&mut self, now_ms: u64) {
        let horizon = now_ms.saturating_sub(self.policy.interval_ms);
        for ledger in [
            &mut self.llm_calls,
            &mut self.tts_calls,
            &mut self.llm_units,
            &mut self.tts_chars,
            &mut self.cost_milliunits,
        ] {
            // An entry survives while `at >= horizon`; with saturating
            // arithmetic (now < interval) the horizon is 0 and nothing
            // expires, so early timestamps cannot be dropped spuriously.
            while ledger.front().is_some_and(|at| *at < horizon) {
                ledger.pop_front();
            }
        }
    }

    /// Snapshot of per-budget-type accounting state, bounded by construction
    /// (at most six fixed records). Does not mutate window state.
    pub fn snapshot(&self, now_ms: u64) -> Vec<GenerativeBudgetRecord> {
        if !self.is_enforced() {
            return Vec::new();
        }
        let horizon = now_ms.saturating_sub(self.policy.interval_ms);
        let window_consumed =
            |ledger: &VecDeque<u64>| -> u64 { ledger.iter().filter(|at| **at >= horizon).sum() };
        let rejected = self.rejected_degraded;
        let mut records = Vec::with_capacity(6);
        if let Some(limit) = self.policy.max_concurrent_generations {
            let in_flight = u64::from(self.in_flight.load(Ordering::Relaxed));
            records.push(GenerativeBudgetRecord {
                budget_type: BudgetType::Concurrent,
                limit: Some(u64::from(limit)),
                consumed: in_flight,
                remaining: Some(u64::from(limit).saturating_sub(in_flight)),
                rejected_or_degraded: rejected,
            });
        }
        if let Some(limit) = self.policy.max_llm_calls_per_interval {
            let consumed = self.llm_calls.len() as u64;
            records.push(GenerativeBudgetRecord {
                budget_type: BudgetType::LlmCalls,
                limit: Some(u64::from(limit)),
                consumed,
                remaining: Some(u64::from(limit).saturating_sub(consumed)),
                rejected_or_degraded: rejected,
            });
        }
        if let Some(limit) = self.policy.max_tts_calls_per_interval {
            let consumed = self.tts_calls.len() as u64;
            records.push(GenerativeBudgetRecord {
                budget_type: BudgetType::TtsCalls,
                limit: Some(u64::from(limit)),
                consumed,
                remaining: Some(u64::from(limit).saturating_sub(consumed)),
                rejected_or_degraded: rejected,
            });
        }
        if let Some(limit) = self.policy.max_llm_units_per_interval {
            let consumed = window_consumed(&self.llm_units);
            records.push(GenerativeBudgetRecord {
                budget_type: BudgetType::LlmUnits,
                limit: Some(limit),
                consumed,
                remaining: Some(limit.saturating_sub(consumed)),
                rejected_or_degraded: rejected,
            });
        }
        if let Some(limit) = self.policy.max_tts_chars_per_interval {
            let consumed = window_consumed(&self.tts_chars);
            records.push(GenerativeBudgetRecord {
                budget_type: BudgetType::TtsChars,
                limit: Some(limit),
                consumed,
                remaining: Some(limit.saturating_sub(consumed)),
                rejected_or_degraded: rejected,
            });
        }
        if let Some(limit) = self.policy.max_estimated_cost_milliunits_per_interval {
            let consumed = window_consumed(&self.cost_milliunits);
            records.push(GenerativeBudgetRecord {
                budget_type: BudgetType::EstimatedCost,
                limit: Some(limit),
                consumed,
                remaining: Some(limit.saturating_sub(consumed)),
                rejected_or_degraded: rejected,
            });
        }
        records
    }

    pub fn denied_total(&self) -> u64 {
        self.denied_total
    }

    pub fn rejected_degraded_total(&self) -> u64 {
        self.rejected_degraded
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn in_flight_high_water(&self) -> u32 {
        self.in_flight_high_water
    }

    /// Count of denials per typed reason, bounded by the six variants.
    pub fn denial_counts(&self) -> Vec<(BudgetDenialReason, u64)> {
        self.denials_by_reason.clone()
    }

    /// Record that an admitted-then-denied-elsewhere or degraded generation
    /// fell back deterministically (observable accounting requirement).
    pub fn note_degraded(&mut self) {
        self.rejected_degraded = self.rejected_degraded.saturating_add(1);
    }

    /// Evict window entries older than the retention horizon so a long-idle
    /// governor does not retain stale ledger entries. Called from the same
    /// tick that drives other bounded retention.
    pub fn tick(&mut self, now_ms: u64) {
        self.trim_windows(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> GenerativeBudgetPolicy {
        GenerativeBudgetPolicy {
            enabled: true,
            max_concurrent_generations: Some(2),
            max_llm_calls_per_interval: Some(3),
            max_tts_calls_per_interval: Some(3),
            interval_ms: 1_000,
            ..GenerativeBudgetPolicy::default()
        }
    }

    #[test]
    fn disabled_governor_admits_everything_without_accounting() {
        let mut governor = GenerativeBudgetGovernor::new(GenerativeBudgetPolicy::default());
        for _ in 0..10 {
            let mut reservation = governor
                .try_admit(BudgetAdmission::ordinary(), 0)
                .expect("admit");
            reservation.settle(&mut governor, 500, 0);
        }
        assert!(governor.snapshot(0).is_empty());
        assert_eq!(governor.denied_total(), 0);
        assert_eq!(governor.in_flight(), 0);
    }

    #[test]
    fn llm_call_budget_exhaustion_denies_with_typed_reason() {
        let mut governor = GenerativeBudgetGovernor::new(policy());
        for _ in 0..3 {
            governor
                .try_admit(BudgetAdmission::ordinary(), 0)
                .expect("admit");
        }
        let denial = governor
            .try_admit(BudgetAdmission::ordinary(), 100)
            .expect_err("budget must deny the fourth call");
        assert_eq!(denial, BudgetDenialReason::LlmCallBudgetExhausted);
    }

    #[test]
    fn rolling_window_resets_after_interval_boundary() {
        let mut governor = GenerativeBudgetGovernor::new(policy());
        for at in 0..3 {
            governor
                .try_admit(BudgetAdmission::ordinary(), at)
                .expect("admit");
        }
        assert!(
            governor
                .try_admit(BudgetAdmission::ordinary(), 500)
                .is_err()
        );
        // Window is [now - interval, now]: entries at 0 survive until now == 1001.
        governor
            .try_admit(BudgetAdmission::ordinary(), 1_001)
            .expect("window reset admits again");
    }

    #[test]
    fn window_boundary_expires_entries_at_exactly_the_horizon() {
        let mut governor = GenerativeBudgetGovernor::new(policy());
        for at in 0..3 {
            governor
                .try_admit(BudgetAdmission::ordinary(), at)
                .expect("admit");
        }
        // At exactly 1000 the horizon is 0, so entry at 0 survives; all
        // three entries remain and the budget still denies.
        assert!(
            governor
                .try_admit(BudgetAdmission::ordinary(), 1_000)
                .is_err()
        );
    }

    #[test]
    fn concurrency_budget_denies_before_provider_work_and_releases_slots() {
        let mut governor = GenerativeBudgetGovernor::new(GenerativeBudgetPolicy {
            enabled: true,
            max_concurrent_generations: Some(1),
            ..GenerativeBudgetPolicy::default()
        });
        let mut first = governor
            .try_admit(BudgetAdmission::ordinary(), 0)
            .expect("first");
        let denial = governor
            .try_admit(BudgetAdmission::ordinary(), 1)
            .expect_err("second concurrent admission must deny");
        assert_eq!(denial, BudgetDenialReason::ConcurrentBudgetExhausted);

        first.release(&mut governor);
        assert_eq!(governor.in_flight(), 0);
        governor
            .try_admit(BudgetAdmission::ordinary(), 2)
            .expect("released slot is reusable");
    }

    #[test]
    fn high_priority_can_spend_the_reserved_share() {
        let policy = GenerativeBudgetPolicy {
            high_priority_reserve_percent: 50,
            ..policy()
        };
        let mut governor = GenerativeBudgetGovernor::new(policy);
        // Ordinary share = 3 - (3 * 50 / 100) = 2; one slot stays reserved
        // for high priority.
        governor
            .try_admit(BudgetAdmission::ordinary(), 0)
            .expect("ordinary");
        governor
            .try_admit(BudgetAdmission::ordinary(), 1)
            .expect("ordinary fills its share");
        assert!(governor.try_admit(BudgetAdmission::ordinary(), 2).is_err());
        governor
            .try_admit(BudgetAdmission::high_priority(), 3)
            .expect("high priority spends the reserve");
    }

    #[test]
    fn zero_percent_reserve_lets_ordinary_traffic_fill_the_budget() {
        let mut governor = GenerativeBudgetGovernor::new(policy());
        for at in 0..3 {
            governor
                .try_admit(BudgetAdmission::ordinary(), at)
                .expect("ordinary fills full budget");
        }
        assert!(
            governor
                .try_admit(BudgetAdmission::high_priority(), 100)
                .is_err()
        );
    }

    #[test]
    fn tts_char_budget_charges_estimates_at_admission_and_actuals_at_settle() {
        let policy = GenerativeBudgetPolicy {
            enabled: true,
            max_tts_chars_per_interval: Some(100),
            interval_ms: 10_000,
            ..GenerativeBudgetPolicy::default()
        };
        let mut governor = GenerativeBudgetGovernor::new(policy);
        let mut reservation = governor
            .try_admit(BudgetAdmission::ordinary().with_estimates(40, 0), 0)
            .expect("first estimate fits");
        assert!(
            governor
                .try_admit(BudgetAdmission::ordinary().with_estimates(70, 0), 1)
                .is_err(),
            "40 + 70 > 100 must deny"
        );
        // Actual usage comes back larger than the estimate.
        reservation.settle(&mut governor, 60, 0);
        // 60 charged; a new 45-char request exceeds 100 -> deny.
        assert!(
            governor
                .try_admit(BudgetAdmission::ordinary().with_estimates(45, 0), 2)
                .is_err()
        );
        // A small request still fits (60 + 40 == 100).
        governor
            .try_admit(BudgetAdmission::ordinary().with_estimates(40, 0), 3)
            .expect("60 + 40 == 100 fits");
    }

    #[test]
    fn cost_budget_requires_a_pricing_input_version() {
        let missing = GenerativeBudgetPolicy {
            enabled: true,
            max_estimated_cost_milliunits_per_interval: Some(1_000),
            ..GenerativeBudgetPolicy::default()
        };
        assert!(missing.validate().is_err());
        let present = GenerativeBudgetPolicy {
            pricing_input_version: Some("pricing-2026-09"),
            ..missing
        };
        assert!(present.validate().is_ok());
    }

    #[test]
    fn cost_budget_keeps_pricing_version_and_denies_overrun() {
        let policy = GenerativeBudgetPolicy {
            enabled: true,
            max_estimated_cost_milliunits_per_interval: Some(500),
            pricing_input_version: Some("pricing-2026-09"),
            interval_ms: 10_000,
            ..GenerativeBudgetPolicy::default()
        };
        let mut governor = GenerativeBudgetGovernor::new(policy);
        assert_eq!(
            governor.policy().pricing_input_version,
            Some("pricing-2026-09")
        );
        governor
            .try_admit(BudgetAdmission::ordinary().with_estimates(0, 400), 0)
            .expect("fits");
        assert!(
            governor
                .try_admit(BudgetAdmission::ordinary().with_estimates(0, 200), 1)
                .is_err()
        );
        governor
            .try_admit(BudgetAdmission::ordinary().with_estimates(0, 100), 2)
            .expect("400 + 100 == 500 fits");
    }

    #[test]
    fn shadow_admission_does_not_consume_budget_by_default() {
        let mut governor = GenerativeBudgetGovernor::new(policy());
        for _ in 0..3 {
            governor
                .try_admit(BudgetAdmission::shadow(), 0)
                .expect("shadow is free by default");
        }
        // The billable budget is untouched.
        let mut ordinary = governor
            .try_admit(BudgetAdmission::ordinary(), 1)
            .expect("ordinary");
        assert_eq!(
            governor.in_flight(),
            1,
            "shadow leaves no in-flight charge; only the ordinary reservation counts"
        );
        ordinary.settle(&mut governor, 0, 0);
    }

    #[test]
    fn shadow_evaluation_charges_budget_only_when_explicitly_enabled() {
        let policy = GenerativeBudgetPolicy {
            charge_shadow_to_budget: true,
            ..policy()
        };
        let mut governor = GenerativeBudgetGovernor::new(policy);
        for _ in 0..3 {
            governor
                .try_admit(BudgetAdmission::shadow(), 0)
                .expect("explicitly charged shadow");
        }
        assert!(
            governor.try_admit(BudgetAdmission::ordinary(), 1).is_err(),
            "shadow spend must be visible in the same ledger, never a silent second spend"
        );
    }

    #[test]
    fn snapshot_reports_limit_consumed_remaining_and_denials() {
        let mut governor = GenerativeBudgetGovernor::new(policy());
        governor
            .try_admit(BudgetAdmission::ordinary(), 0)
            .expect("admit");
        governor
            .try_admit(BudgetAdmission::ordinary(), 0)
            .expect("admit");
        governor
            .try_admit(BudgetAdmission::ordinary(), 0)
            .expect("admit");
        assert!(governor.try_admit(BudgetAdmission::ordinary(), 0).is_err());

        let snapshot = governor.snapshot(0);
        let llm = snapshot
            .iter()
            .find(|record| record.budget_type == BudgetType::LlmCalls)
            .expect("llm record");
        assert_eq!(llm.limit, Some(3));
        assert_eq!(llm.consumed, 3);
        assert_eq!(llm.remaining, Some(0));
        assert_eq!(llm.rejected_or_degraded, 0);
        assert_eq!(
            governor.denial_counts(),
            vec![(BudgetDenialReason::LlmCallBudgetExhausted, 1)]
        );
    }

    #[test]
    fn validation_rejects_zero_limits_and_bad_reserve() {
        let zero_interval = GenerativeBudgetPolicy {
            interval_ms: 0,
            ..GenerativeBudgetPolicy::default()
        };
        let error = zero_interval.validate().expect_err("zero interval");
        assert_eq!(error.field(), "interval_ms");
        let zero_calls = GenerativeBudgetPolicy {
            enabled: true,
            max_llm_calls_per_interval: Some(0),
            ..GenerativeBudgetPolicy::default()
        };
        assert!(zero_calls.validate().is_err());
        let bad_reserve = GenerativeBudgetPolicy {
            high_priority_reserve_percent: 101,
            ..GenerativeBudgetPolicy::default()
        };
        assert!(bad_reserve.validate().is_err());
    }
}
