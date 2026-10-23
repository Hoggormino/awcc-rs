//! Desktop policy state, separate from both CPU and firmware observations.
use awcc_core::{OperationLifecycle, PowerProfile, ThermalMode, ThermalPolicy};
use awcc_platform::cpu_policy::CpuPolicyOutcome;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerSettings {
    pub selected: Option<PowerProfile>,
    pub counterparts: BTreeMap<PowerProfile, ThermalPolicy>,
    /// Explicit turbo choice wins over profile policy; profiles never toggle turbo implicitly.
    pub turbo_override: Option<bool>,
}
impl Default for PowerSettings {
    fn default() -> Self {
        Self {
            selected: None,
            counterparts: [
                (
                    PowerProfile::PowerSaver,
                    ThermalPolicy::Preset {
                        mode: ThermalMode::Quiet,
                    },
                ),
                (
                    PowerProfile::Balanced,
                    ThermalPolicy::Preset {
                        mode: ThermalMode::Balanced,
                    },
                ),
                (
                    PowerProfile::Performance,
                    ThermalPolicy::Preset {
                        mode: ThermalMode::Performance,
                    },
                ),
            ]
            .into(),
            turbo_override: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileHold {
    pub cookie: u32,
    pub owner: String,
    pub profile: PowerProfile,
    pub reason: String,
    pub application_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PowerStatus {
    pub selected: Option<PowerProfile>,
    pub active: Option<PowerProfile>,
    pub available: Vec<PowerProfile>,
    pub counterparts: BTreeMap<PowerProfile, ThermalPolicy>,
    pub generation: u64,
    pub cpu: CpuPolicyOutcome,
    pub thermal: Option<OperationLifecycle>,
    pub problem: Option<String>,
    pub holds: Vec<ProfileHold>,
    /// Bounded, monotonically sequenced release notifications, including explicit selection.
    pub releases: Vec<(u64, u32, String)>,
}
impl PowerStatus {
    pub fn effective(&self, selected: PowerProfile) -> PowerProfile {
        if self
            .holds
            .iter()
            .any(|h| h.profile == PowerProfile::PowerSaver)
        {
            PowerProfile::PowerSaver
        } else if self
            .holds
            .iter()
            .any(|h| h.profile == PowerProfile::Performance)
        {
            PowerProfile::Performance
        } else {
            selected
        }
    }
    pub fn release(&mut self, cookie: u32) {
        if let Some(index) = self.holds.iter().position(|h| h.cookie == cookie) {
            let h = self.holds.remove(index);
            let seq = self.releases.last().map_or(1, |r| r.0 + 1);
            self.releases.push((seq, cookie, h.owner));
            if self.releases.len() > 128 {
                self.releases.remove(0);
            }
        }
    }
}
