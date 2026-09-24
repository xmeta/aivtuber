use crate::HardeningError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultSubsystem {
    Jev,
    Thinking,
    Tts,
    VTubeStudio,
    Obs,
    Audio,
    AssetStore,
    SemanticIndex,
    ContentIngress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    Timeout,
    Unavailable,
    RateLimited,
    Disconnect,
    Corrupted,
    Incompatible,
    Flood,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultSpec {
    pub subsystem: FaultSubsystem,
    /// One-based call occurrence for this subsystem.
    pub occurrence: u64,
    pub kind: FaultKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultPlan {
    pub seed: u64,
    pub faults: Vec<FaultSpec>,
}

impl FaultPlan {
    pub fn new(seed: u64, mut faults: Vec<FaultSpec>) -> Result<Self, HardeningError> {
        faults.sort_by_key(|fault| (fault.subsystem, fault.occurrence));
        let mut previous = None;
        for fault in &faults {
            if fault.occurrence == 0 {
                return Err(HardeningError::InvalidConfiguration(
                    "fault occurrence must be one-based",
                ));
            }
            let key = (fault.subsystem, fault.occurrence);
            if previous == Some(key) {
                return Err(HardeningError::InvalidConfiguration(
                    "duplicate subsystem occurrence in fault plan",
                ));
            }
            previous = Some(key);
        }
        Ok(Self { seed, faults })
    }

    pub fn injector(&self) -> FaultInjector {
        FaultInjector {
            plan: self.clone(),
            counters: BTreeMap::new(),
            consumed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedFault {
    pub subsystem: FaultSubsystem,
    pub occurrence: u64,
    pub kind: FaultKind,
}

#[derive(Debug, Clone)]
pub struct FaultInjector {
    plan: FaultPlan,
    counters: BTreeMap<FaultSubsystem, u64>,
    consumed: Vec<InjectedFault>,
}

impl FaultInjector {
    pub fn next(&mut self, subsystem: FaultSubsystem) -> Option<FaultKind> {
        let occurrence = self
            .counters
            .entry(subsystem)
            .and_modify(|value| *value = value.saturating_add(1))
            .or_insert(1);
        let occurrence = *occurrence;
        let kind = self
            .plan
            .faults
            .iter()
            .find(|fault| fault.subsystem == subsystem && fault.occurrence == occurrence)
            .map(|fault| fault.kind)?;
        self.consumed.push(InjectedFault {
            subsystem,
            occurrence,
            kind,
        });
        Some(kind)
    }

    pub fn consumed(&self) -> &[InjectedFault] {
        &self.consumed
    }

    pub fn seed(&self) -> u64 {
        self.plan.seed
    }
}
