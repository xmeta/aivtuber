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

impl FaultSubsystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Thinking => "thinking",
            Self::Tts => "tts",
            Self::VTubeStudio => "v_tube_studio",
            Self::Obs => "obs",
            Self::Audio => "audio",
            Self::AssetStore => "asset_store",
            Self::SemanticIndex => "semantic_index",
            Self::ContentIngress => "content_ingress",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

impl FaultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::RateLimited => "rate_limited",
            Self::Disconnect => "disconnect",
            Self::Corrupted => "corrupted",
            Self::Incompatible => "incompatible",
            Self::Flood => "flood",
        }
    }
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

    /// Stable identity of the plan's behaviour, independent of `seed`.
    ///
    /// Injection is fully determined by the explicit `faults` list - the seed is
    /// provenance, not a knob - so a comparison baseline must be keyed on the
    /// plan itself. Two overlays that differ in a subsystem, occurrence or kind
    /// produce different ids even with the same seed, and reordering the same
    /// faults does not change it.
    pub fn plan_id(&self) -> String {
        let mut faults: Vec<&FaultSpec> = self.faults.iter().collect();
        faults.sort_by_key(|fault| (fault.subsystem, fault.occurrence, fault.kind));
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for fault in faults {
            for byte in fault.subsystem.as_str().bytes() {
                hash = fnv1a64_step(hash, byte);
            }
            hash = fnv1a64_step(hash, b':');
            for byte in fault.occurrence.to_le_bytes() {
                hash = fnv1a64_step(hash, byte);
            }
            for byte in fault.kind.as_str().bytes() {
                hash = fnv1a64_step(hash, byte);
            }
            hash = fnv1a64_step(hash, b';');
        }
        format!("{hash:016x}")
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

fn fnv1a64_step(hash: u64, byte: u8) -> u64 {
    (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
}
