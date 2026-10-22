//! Serializable view of the daemon-owned thermal state.
//!
//! The daemon owns the single authoritative thermal state machine. Clients receive this
//! [`ThermalView`] over IPC and rebuild their presentation snapshot from it. The view keeps the
//! project invariants intact: *Unknown is not Manual* and *Unavailable is not zero*, because every
//! reading travels with its explicit availability state rather than a default value.

use awcc_core::{
    FanCurveConfig, Observation, OperationLifecycle, SensorReading, TelemetrySnapshot, ThermalMode,
    ThermalModeState,
};
use awcc_platform::WriteAuditEntry;
use serde::{Deserialize, Serialize};

/// Maximum number of write-audit entries carried by one view.
pub const MAX_VIEW_AUDIT_ENTRIES: usize = 32;

fn static_unit(unit: &str) -> &'static str {
    match unit {
        "°C" => "°C",
        "RPM" => "RPM",
        "%" => "%",
        _ => "",
    }
}

/// Wire form of [`SensorReading`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireSensor<T> {
    Valid {
        value: T,
        unit: String,
    },
    Unavailable {
        reason: String,
    },
    Stale {
        last_value: T,
        unit: String,
        age_secs: u64,
        reason: String,
    },
}

impl<T: Copy> WireSensor<T> {
    #[must_use]
    pub fn from_core(reading: &SensorReading<T>) -> Self {
        match reading {
            SensorReading::Valid { value, unit } => Self::Valid {
                value: *value,
                unit: (*unit).to_string(),
            },
            SensorReading::Unavailable { reason } => Self::Unavailable {
                reason: reason.clone(),
            },
            SensorReading::Stale {
                last_value,
                unit,
                age_secs,
                reason,
            } => Self::Stale {
                last_value: *last_value,
                unit: (*unit).to_string(),
                age_secs: *age_secs,
                reason: reason.clone(),
            },
        }
    }

    #[must_use]
    pub fn to_core(&self) -> SensorReading<T> {
        match self {
            Self::Valid { value, unit } => SensorReading::Valid {
                value: *value,
                unit: static_unit(unit),
            },
            Self::Unavailable { reason } => SensorReading::Unavailable {
                reason: reason.clone(),
            },
            Self::Stale {
                last_value,
                unit,
                age_secs,
                reason,
            } => SensorReading::Stale {
                last_value: *last_value,
                unit: static_unit(unit),
                age_secs: *age_secs,
                reason: reason.clone(),
            },
        }
    }

    /// Re-express this reading as stale because the service connection was lost `elapsed_secs`
    /// ago. A reading that never had a value stays unavailable.
    #[must_use]
    pub fn disconnected(&self, elapsed_secs: u64, reason: &str) -> Self {
        match self {
            Self::Valid { value, unit } => Self::Stale {
                last_value: *value,
                unit: unit.clone(),
                age_secs: elapsed_secs,
                reason: reason.to_string(),
            },
            Self::Stale {
                last_value,
                unit,
                age_secs,
                ..
            } => Self::Stale {
                last_value: *last_value,
                unit: unit.clone(),
                age_secs: age_secs.saturating_add(elapsed_secs),
                reason: reason.to_string(),
            },
            Self::Unavailable { .. } => Self::Unavailable {
                reason: reason.to_string(),
            },
        }
    }
}

/// Wire form of [`Observation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireObservation<T> {
    NotObserved,
    Observed {
        value: T,
    },
    Unavailable {
        reason: String,
    },
    Stale {
        last_value: T,
        age_secs: u64,
        reason: String,
    },
}

impl<T: Clone> WireObservation<T> {
    #[must_use]
    pub fn from_core(obs: &Observation<T>) -> Self {
        match obs {
            Observation::NotObserved => Self::NotObserved,
            Observation::Observed(value) => Self::Observed {
                value: value.clone(),
            },
            Observation::Unavailable { reason } => Self::Unavailable {
                reason: reason.clone(),
            },
            Observation::Stale {
                last_value,
                age_secs,
                reason,
            } => Self::Stale {
                last_value: last_value.clone(),
                age_secs: *age_secs,
                reason: reason.clone(),
            },
        }
    }

    #[must_use]
    pub fn to_core(&self) -> Observation<T> {
        match self {
            Self::NotObserved => Observation::NotObserved,
            Self::Observed { value } => Observation::Observed(value.clone()),
            Self::Unavailable { reason } => Observation::Unavailable {
                reason: reason.clone(),
            },
            Self::Stale {
                last_value,
                age_secs,
                reason,
            } => Observation::Stale {
                last_value: last_value.clone(),
                age_secs: *age_secs,
                reason: reason.clone(),
            },
        }
    }

    #[must_use]
    pub fn disconnected(&self, elapsed_secs: u64, reason: &str) -> Self {
        match self {
            Self::Observed { value } => Self::Stale {
                last_value: value.clone(),
                age_secs: elapsed_secs,
                reason: reason.to_string(),
            },
            Self::Stale {
                last_value,
                age_secs,
                ..
            } => Self::Stale {
                last_value: last_value.clone(),
                age_secs: age_secs.saturating_add(elapsed_secs),
                reason: reason.to_string(),
            },
            Self::NotObserved | Self::Unavailable { .. } => Self::Unavailable {
                reason: reason.to_string(),
            },
        }
    }
}

/// Wire form of [`TelemetrySnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireTelemetry {
    pub cpu_temp_c: WireSensor<u16>,
    pub gpu_temp_c: WireSensor<u16>,
    pub cpu_fan_rpm: WireSensor<u16>,
    pub gpu_fan_rpm: WireSensor<u16>,
    pub cpu_boost_pct: WireSensor<u8>,
    pub gpu_boost_pct: WireSensor<u8>,
}

impl WireTelemetry {
    #[must_use]
    pub fn from_core(t: &TelemetrySnapshot) -> Self {
        Self {
            cpu_temp_c: WireSensor::from_core(&t.cpu_temp_c),
            gpu_temp_c: WireSensor::from_core(&t.gpu_temp_c),
            cpu_fan_rpm: WireSensor::from_core(&t.cpu_fan_rpm),
            gpu_fan_rpm: WireSensor::from_core(&t.gpu_fan_rpm),
            cpu_boost_pct: WireSensor::from_core(&t.cpu_boost_pct),
            gpu_boost_pct: WireSensor::from_core(&t.gpu_boost_pct),
        }
    }

    #[must_use]
    pub fn to_core(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            cpu_temp_c: self.cpu_temp_c.to_core(),
            gpu_temp_c: self.gpu_temp_c.to_core(),
            cpu_fan_rpm: self.cpu_fan_rpm.to_core(),
            gpu_fan_rpm: self.gpu_fan_rpm.to_core(),
            cpu_boost_pct: self.cpu_boost_pct.to_core(),
            gpu_boost_pct: self.gpu_boost_pct.to_core(),
        }
    }

    #[must_use]
    pub fn disconnected(&self, elapsed_secs: u64, reason: &str) -> Self {
        Self {
            cpu_temp_c: self.cpu_temp_c.disconnected(elapsed_secs, reason),
            gpu_temp_c: self.gpu_temp_c.disconnected(elapsed_secs, reason),
            cpu_fan_rpm: self.cpu_fan_rpm.disconnected(elapsed_secs, reason),
            gpu_fan_rpm: self.gpu_fan_rpm.disconnected(elapsed_secs, reason),
            cpu_boost_pct: self.cpu_boost_pct.disconnected(elapsed_secs, reason),
            gpu_boost_pct: self.gpu_boost_pct.disconnected(elapsed_secs, reason),
        }
    }

    /// Every reading unavailable for one reason (no service view has ever been received).
    #[must_use]
    pub fn unavailable(reason: &str) -> Self {
        let u8s = || WireSensor::<u8>::Unavailable {
            reason: reason.to_string(),
        };
        let u16s = || WireSensor::<u16>::Unavailable {
            reason: reason.to_string(),
        };
        Self {
            cpu_temp_c: u16s(),
            gpu_temp_c: u16s(),
            cpu_fan_rpm: u16s(),
            gpu_fan_rpm: u16s(),
            cpu_boost_pct: u8s(),
            gpu_boost_pct: u8s(),
        }
    }
}

/// What the daemon's control loop is doing with the custom fan curve right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlState {
    /// No custom curve is being applied.
    Idle,
    /// The daemon is following the curve.
    Active,
    /// The curve is wanted but suspended: a required sensor is missing or the verified fallback
    /// was entered. Control resumes only after fresh sensors are observed again.
    Fallback,
    /// The saved curve was wanted at startup but could not be restored.
    RestoreFailed,
}

/// Separately reported desired settings and actual control state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlStatus {
    pub state: ControlState,
    /// Saved curve parameters (the `enabled` flag inside is not the intent: see `intent_active`).
    pub saved_curve: FanCurveConfig,
    /// The user's intent: should the curve be running?
    pub intent_active: bool,
    /// Restore the saved curve when the daemon starts (default off).
    pub restore_at_startup: bool,
    /// Whole seconds since the last successful curve evaluation, if any ever succeeded.
    pub last_evaluation_age_secs: Option<u64>,
    /// Outcome text of the last curve write or control transition.
    pub last_outcome: Option<String>,
    /// Why restoration or activation failed, or why the curve is suspended.
    pub failure_reason: Option<String>,
    /// Evaluation cadence in milliseconds.
    pub tick_interval_ms: u64,
    /// Seconds the control worker has been running.
    pub uptime_secs: u64,
    /// Whether a fresh CPU temperature was available at the last evaluation.
    pub cpu_sensor_fresh: bool,
    /// Why saved settings were not applied (corrupt, wrong device, newer schema), if so.
    pub settings_problem: Option<String>,
    /// A settings write has been accepted but is not yet durable.
    pub settings_save_pending: bool,
    /// The last settings write failed.
    pub settings_save_error: Option<String>,
}

/// The authoritative thermal state published by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThermalView {
    pub device_name: String,
    pub acpi_prefix: String,
    pub thermal_mode_state: ThermalModeState,
    pub game_shift: WireObservation<bool>,
    pub pre_gmode_restore_target: Option<ThermalMode>,
    pub telemetry: WireTelemetry,
    pub cpu_turbo: WireObservation<bool>,
    pub tcc_offset_c: WireSensor<u8>,
    /// The curve currently being applied (`enabled` is true only while control is active).
    pub fan_curve: FanCurveConfig,
    pub lifecycle: OperationLifecycle,
    pub operation_id: u64,
    pub audit: Vec<WriteAuditEntry>,
    pub control: ControlStatus,
}

impl ThermalView {
    /// Build a passive "service unreachable" view used before any real view was received.
    #[must_use]
    pub fn placeholder_unavailable(reason: &str) -> ThermalModeState {
        ThermalModeState::Unknown {
            raw_code: None,
            reason: reason.to_string(),
        }
    }

    /// Copy of this view re-expressed as stale because the connection was lost.
    #[must_use]
    pub fn disconnected(&self, elapsed_secs: u64, reason: &str) -> Self {
        let mut view = self.clone();
        view.thermal_mode_state = ThermalModeState::Unknown {
            raw_code: None,
            reason: reason.to_string(),
        };
        view.game_shift = self.game_shift.disconnected(elapsed_secs, reason);
        view.telemetry = self.telemetry.disconnected(elapsed_secs, reason);
        view.cpu_turbo = self.cpu_turbo.disconnected(elapsed_secs, reason);
        view.tcc_offset_c = self.tcc_offset_c.disconnected(elapsed_secs, reason);
        view
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sensor_round_trips_and_disconnect_never_invents_values() {
        let valid = WireSensor::Valid {
            value: 70_u16,
            unit: "°C".to_string(),
        };
        let json = serde_json::to_string(&valid).unwrap();
        assert_eq!(
            serde_json::from_str::<WireSensor<u16>>(&json).unwrap(),
            valid
        );
        let core = valid.to_core();
        assert_eq!(WireSensor::from_core(&core), valid);

        let stale = valid.disconnected(5, "lost");
        assert!(matches!(stale, WireSensor::Stale { age_secs: 5, .. }));
        let missing: WireSensor<u16> = WireSensor::Unavailable {
            reason: "none".into(),
        };
        assert!(matches!(
            missing.disconnected(5, "lost"),
            WireSensor::Unavailable { .. }
        ));
    }

    #[test]
    fn observation_disconnect_is_stale_not_false() {
        let obs = WireObservation::Observed { value: true };
        let stale = obs.disconnected(3, "gone");
        assert!(matches!(
            stale,
            WireObservation::Stale {
                last_value: true,
                age_secs: 3,
                ..
            }
        ));
        assert!(matches!(
            WireObservation::<bool>::NotObserved.disconnected(3, "gone"),
            WireObservation::Unavailable { .. }
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bad = r#"{"state":"valid","value":1,"unit":"%","extra":1}"#;
        assert!(serde_json::from_str::<WireSensor<u8>>(bad).is_err());
    }
}
