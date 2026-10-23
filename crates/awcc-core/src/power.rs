//! Public desktop profiles and their independently saved thermal counterparts.
use crate::{FanCurveConfig, ThermalMode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerProfile {
    PowerSaver,
    Balanced,
    Performance,
}
impl PowerProfile {
    pub const ALL: [Self; 3] = [Self::PowerSaver, Self::Balanced, Self::Performance];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }
    pub fn parse(s: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| format!("unsupported system profile: {s}"))
    }
    pub const fn for_mode(mode: ThermalMode) -> Option<Self> {
        match mode {
            ThermalMode::Quiet | ThermalMode::Cool | ThermalMode::BatterySaver => {
                Some(Self::PowerSaver)
            }
            ThermalMode::Balanced => Some(Self::Balanced),
            ThermalMode::Performance | ThermalMode::FullSpeed | ThermalMode::GMode => {
                Some(Self::Performance)
            }
            ThermalMode::Manual => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThermalPolicy {
    Preset {
        mode: ThermalMode,
    },
    Manual {
        cpu_boost_pct: u8,
        gpu_boost_pct: u8,
        curve: Option<FanCurveConfig>,
    },
}
impl ThermalPolicy {
    pub fn validate(&self, profile: PowerProfile) -> Result<(), String> {
        match self {
            Self::Preset { mode } if PowerProfile::for_mode(*mode) == Some(profile) => Ok(()),
            Self::Manual {
                cpu_boost_pct,
                gpu_boost_pct,
                ..
            } if *cpu_boost_pct <= 100 && *gpu_boost_pct <= 100 => Ok(()),
            _ => Err("thermal counterpart is not valid for this system profile".into()),
        }
    }
}
