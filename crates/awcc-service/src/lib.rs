//! Non-blocking thermal/fan/lighting orchestration service, daemon IPC, and hotkey state machine.
//!
//! # Operation outcome contract
//!
//! Every state-changing [`ServiceCommand`] produces exactly one operation record, identified by
//! [`SystemSnapshot::operation_id`], whose [`OperationLifecycle`] says how much is known:
//!
//! | state        | meaning                                                                     |
//! |--------------|-----------------------------------------------------------------------------|
//! | `Requested`  | queued by a client, nothing executed (set by the GUI, never by the worker)  |
//! | `Pending`    | the worker started executing; no transport result yet                       |
//! | `Accepted`   | the transport accepted the write; the applied state is **not** confirmed    |
//! | `Confirmed`  | accepted **and** the post-write readback matched the request                |
//! | `RolledBack` | a multi-step operation failed part-way and the earlier step was undone      |
//! | `Failed`     | the write was rejected, or the readback contradicts the request             |
//!
//! `Confirmed` is only reachable where a readback exists and was *already part of the passive
//! end-of-operation snapshot*: no extra hardware query, retry, or replay is ever issued to
//! strengthen a result. Operations without a readback (all lighting writes, since the ELC protocol
//! has none) top out at `Accepted`, and a USB transfer that completed is reported separately from
//! the optional acknowledgement read that follows it.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::control::ControlOp;
use crate::ipc::{ClientError, DaemonClient, DaemonLock};
use crate::wire::{ControlStatus, ThermalView, WireObservation, WireSensor, WireTelemetry};
use awcc_core::{
    DatabaseSource, DeviceProfile, FanCurveConfig, Observation, OperationError, OperationLifecycle,
    OperationSuccess, SensorReading, SimScenario, TelemetrySnapshot, ThermalMode, ThermalModeState,
    interpolate_curve_boost,
    lighting::{LightingEffect, ZoneTarget},
    validate_fan_boost, validate_tcc_offset,
};
use awcc_diagnostics::{
    export::{
        BuildIdentityReport, DatabaseProvenance, DeviceProfileEvidence, SessionEvidence,
        SubsystemHealthEvidence, TelemetryEvidence,
    },
    global_diagnostics,
    hash::sha256_hex,
    ring::{HasSeq, RingStats, SeqRing},
    time::{now_unix_millis, rfc3339_millis},
};
use awcc_platform::{
    HwmonProvider, LinuxAcpiTransport, MemoryStateStorage, PosixStateStore, SimBackend, SimFaults,
    StateStorage, ThermalTransport, TransportError, WriteAuditEntry,
    usb_elc::{
        EffectController, ElcDiagnosticsSummary, LightingStateSource, LinuxUsbElcTransport,
        SimUsbElcTransport, TransactionReceipt, UsbHidTransport,
    },
};
pub use awcc_platform::{resolve_host_device_profile, resolve_simulated_m15_r6_profile};
use thiserror::Error;

pub mod control;
pub mod daemon;
pub mod installer;
pub mod ipc;
pub mod keybinder;
pub mod settings;
pub mod wire;

/// Default bounded capacity of the in-memory hardware write-audit ring.
pub const DEFAULT_AUDIT_RING_CAPACITY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SequencedAudit(WriteAuditEntry);

impl HasSeq for SequencedAudit {
    fn seq(&self) -> u64 {
        self.0.sequence as u64
    }
}

/// Immutable snapshot of the system state published to the GUI or CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSnapshot {
    pub profile: DeviceProfile,
    /// Which backend was *selected* (simulated vs. live). This is not proof that every live
    /// subsystem is currently communicating: see the per-value observations below.
    pub is_simulated: bool,
    pub scenario: SimScenario,
    pub thermal_mode_state: ThermalModeState,
    /// GameShift (G-Mode) latch. A failed query is `Unavailable`/`Stale`, never `false`.
    pub game_shift: Observation<bool>,
    pub pre_gmode_restore_target: Option<ThermalMode>,
    pub telemetry: TelemetrySnapshot,
    /// CPU turbo state. A failed query is `Unavailable`/`Stale`, never "enabled".
    pub cpu_turbo: Observation<bool>,
    pub tcc_offset_c: SensorReading<u8>,
    pub fan_curve: FanCurveConfig,
    /// Remembered lighting brightness. The ELC protocol has no brightness readback: see
    /// `lighting_state_source` for whether this was assumed, restored, or commanded.
    pub lighting_brightness_pct: u8,
    pub lighting_stealth_enabled: bool,
    pub lighting_state_source: LightingStateSource,
    pub lighting_last_effect: LightingEffect,
    pub lighting_last_target: ZoneTarget,
    pub lighting_persist_boot: bool,
    pub lighting_zone_colors: BTreeMap<u8, u32>,
    pub lighting_power_colors: BTreeMap<u16, u32>,
    /// Zones (`u8`) whose colour/effect has been explicitly commanded in this session (making
    /// their `lighting_zone_colors` entry a remembered session value rather than an assumed default).
    pub lighting_commanded_zones: BTreeSet<u8>,
    /// Power-button state slots (`0x005B..=0x0060`) whose colour has been explicitly commanded
    /// in this session (making their `lighting_power_colors` entry a remembered session value
    /// rather than an assumed default).
    pub lighting_commanded_power_slots: BTreeSet<u16>,
    /// ELC firmware/config diagnostics. `NotObserved`/`Unavailable` never fall back to made-up
    /// values; `Stale` carries the age of the last good report and the failure that replaced it.
    pub elc_diagnostics: Observation<ElcDiagnosticsSummary>,
    /// Outcome of the most recent state-changing operation (see [`Self::operation_id`]).
    pub lifecycle: OperationLifecycle,
    /// Id of the operation `lifecycle` belongs to; `0` when no operation has run yet. A passive
    /// refresh never changes it, so a client can tell a reused outcome from a fresh one.
    pub operation_id: u64,
    /// Number of queued commands the worker had processed when this snapshot was produced
    /// (`0` for the initial snapshot). Compare with [`CommandTicket::seq`] to know whether a
    /// specific request has been handled yet.
    pub command_seq: u64,
    pub write_audit_log: Vec<WriteAuditEntry>,
    /// Sequence-aware accounting for `write_audit_log`: distinguishes lifetime write count,
    /// currently retained entries, evicted entries, and entries cleared from the visible view.
    pub audit_stats: RingStats,
    /// Control status reported by the background service, when thermal control is service-owned.
    pub service_control: Option<ControlStatus>,
    /// Why the background service is not reachable, when thermal control is service-owned.
    pub service_error: Option<String>,
}

impl SystemSnapshot {
    /// Lifetime count of mutating hardware/simulated writes executed since startup.
    ///
    /// Survives bounded ring eviction and clearing of the visible audit history.
    #[must_use]
    pub fn write_count(&self) -> usize {
        if self.audit_stats.lifetime_total > 0 || self.write_audit_log.is_empty() {
            self.audit_stats.lifetime_total as usize
        } else {
            self.write_audit_log.len()
        }
    }

    /// `true` only when the latest GameShift query succeeded and reported the latch enabled.
    #[must_use]
    pub fn game_shift_latched(&self) -> bool {
        self.game_shift.fresh() == Some(&true)
    }

    /// Convert this passive snapshot into a serializable [`SessionEvidence`] record without
    /// touching hardware.
    #[must_use]
    pub fn to_session_evidence(
        &self,
        session_id: &str,
        build_info: BuildIdentityReport,
        worker_alive: bool,
    ) -> SessionEvidence {
        let recorded_unix_ms = now_unix_millis();
        let recorded_at_utc = rfc3339_millis(recorded_unix_ms);

        let mut capabilities = Vec::new();
        if self.profile.entry.has_feature_thermal_modes() {
            capabilities.push("thermal_modes".to_string());
        }
        if self.profile.entry.has_feature_fan_boost() {
            capabilities.push("fan_boost".to_string());
        }
        if self.profile.entry.has_feature_auto_boost() {
            capabilities.push("auto_boost".to_string());
        }
        if self.profile.entry.has_feature_cpu_temp() {
            capabilities.push("cpu_temp".to_string());
        }
        if self.profile.entry.has_feature_gpu_temp() {
            capabilities.push("gpu_temp".to_string());
        }
        if self.profile.entry.has_feature_brightness_control() {
            capabilities.push("brightness_control".to_string());
        }
        if self.profile.entry.has_feature_gmode_toggle() {
            capabilities.push("gmode_toggle".to_string());
        }

        let (source_kind, source_label, fingerprint_sha256) = match &self.profile.database_source {
            DatabaseSource::EmbeddedDefault => (
                "compiled_in_default".to_string(),
                "<embedded>".to_string(),
                sha256_hex(awcc_core::DEFAULT_DATABASE_JSON.as_bytes()),
            ),
            DatabaseSource::SystemEtc(path) => (
                "system_etc_awcc".to_string(),
                path.display().to_string(),
                std::fs::read(path).map_or_else(
                    |_| sha256_hex(awcc_core::DEFAULT_DATABASE_JSON.as_bytes()),
                    |bytes| sha256_hex(&bytes),
                ),
            ),
            DatabaseSource::LocalWorkingDir(path) => (
                "cwd_override".to_string(),
                path.display().to_string(),
                std::fs::read(path).map_or_else(
                    |_| sha256_hex(awcc_core::DEFAULT_DATABASE_JSON.as_bytes()),
                    |bytes| sha256_hex(&bytes),
                ),
            ),
            DatabaseSource::EnvVar(path) => (
                "env_override".to_string(),
                path.display().to_string(),
                std::fs::read(path).map_or_else(
                    |_| sha256_hex(awcc_core::DEFAULT_DATABASE_JSON.as_bytes()),
                    |bytes| sha256_hex(&bytes),
                ),
            ),
        };

        let acpi_wmax_available = matches!(self.thermal_mode_state, ThermalModeState::Known(_))
            || self.game_shift.is_observed();
        let usb_elc_available = self.elc_diagnostics.is_observed();
        let hwmon_available = self.telemetry.cpu_temp_c.is_available()
            || self.telemetry.gpu_temp_c.is_available()
            || self.telemetry.cpu_fan_rpm.is_available();

        let (usb_firmware_version, usb_status_summary) = match &self.elc_diagnostics {
            Observation::Observed(diag) => {
                let (maj, min, pat) = diag.firmware_version;
                (
                    Some(format!("{maj}.{min}.{pat}")),
                    Some(format!(
                        "status=0x{:02X}, animations={}",
                        diag.status_byte, diag.animation_count
                    )),
                )
            }
            Observation::Stale {
                last_value,
                age_secs,
                reason,
            } => {
                let (maj, min, pat) = last_value.firmware_version;
                (
                    Some(format!("{maj}.{min}.{pat} (stale {age_secs}s)")),
                    Some(format!(
                        "status=0x{:02X} (stale {age_secs}s: {reason})",
                        last_value.status_byte
                    )),
                )
            }
            Observation::Unavailable { reason } => (None, Some(format!("unavailable ({reason})"))),
            Observation::NotObserved => (None, None),
        };

        let last_error = match &self.lifecycle {
            OperationLifecycle::Failed { error, .. } => Some(error.clone()),
            OperationLifecycle::RolledBack { reason, .. } => Some(reason.clone()),
            _ => self
                .elc_diagnostics
                .problem()
                .or_else(|| self.game_shift.problem())
                .or_else(|| self.cpu_turbo.problem())
                .or_else(|| self.telemetry.cpu_temp_c.problem())
                .map(ToString::to_string),
        };

        let g_mode_state = match &self.game_shift {
            Observation::Observed(true) => "active".to_string(),
            Observation::Observed(false) => "inactive".to_string(),
            Observation::Stale {
                last_value,
                age_secs,
                reason,
            } => format!(
                "{} (stale {age_secs}s: {reason})",
                if *last_value { "active" } else { "inactive" }
            ),
            Observation::Unavailable { reason } => format!("unavailable ({reason})"),
            Observation::NotObserved => "not_observed".to_string(),
        };

        let stealth_source = match self.lighting_state_source {
            LightingStateSource::AssumedDefault => "assumed_default",
            LightingStateSource::PersistedBrightness => "persisted_brightness",
            LightingStateSource::Commanded => "commanded",
        };
        let stealth_lights_off = format!(
            "{} ({stealth_source})",
            if self.lighting_stealth_enabled {
                "enabled"
            } else {
                "disabled"
            }
        );

        #[allow(clippy::cast_possible_truncation)]
        let cpu_temp_c = self.telemetry.cpu_temp_c.fresh_value().map(|v| v as u8);
        #[allow(clippy::cast_possible_truncation)]
        let gpu_temp_c = self.telemetry.gpu_temp_c.fresh_value().map(|v| v as u8);

        let telemetry_stale = matches!(self.telemetry.cpu_temp_c, SensorReading::Stale { .. })
            || matches!(self.telemetry.gpu_temp_c, SensorReading::Stale { .. })
            || self.game_shift.is_stale()
            || self.cpu_turbo.is_stale()
            || self.elc_diagnostics.is_stale();
        let telemetry_age_ms = match &self.telemetry.cpu_temp_c {
            SensorReading::Stale { age_secs, .. } => Some(age_secs.saturating_mul(1000)),
            _ => None,
        };

        let audit_entries = self
            .write_audit_log
            .iter()
            .map(|e| {
                let mut obj = serde_json::Map::new();
                obj.insert("sequence".to_string(), serde_json::json!(e.sequence));
                obj.insert("op_id".to_string(), serde_json::json!(e.op_id));
                obj.insert("command".to_string(), serde_json::json!(e.command));
                obj.insert("outcome".to_string(), serde_json::json!(e.outcome));
                obj.insert("succeeded".to_string(), serde_json::json!(e.succeeded));
                obj.insert("ack".to_string(), serde_json::json!(e.ack.describe()));
                if let Some(raw_hex) = &e.raw_usb_response_hex {
                    obj.insert(
                        "raw_usb_response_hex".to_string(),
                        serde_json::json!(raw_hex),
                    );
                }
                serde_json::Value::Object(obj)
            })
            .collect();

        let thermal_mode_str = match &self.thermal_mode_state {
            ThermalModeState::Known(mode) => mode.to_string(),
            ThermalModeState::Unknown { reason, .. } => format!("unknown ({reason})"),
        };

        SessionEvidence {
            session_id: session_id.to_string(),
            recorded_at_utc,
            recorded_unix_ms,
            build_info,
            device_profile: Some(DeviceProfileEvidence {
                model_name: self.profile.device_name.clone(),
                smbios_system_id: self.profile.smbios_system_id.clone(),
                acpi_prefix: self.profile.acpi_prefix.as_str().to_string(),
                acpi_wmax_path: self.profile.acpi_prefix.wmax_path(),
                usb_vid_pid: "187c:0550".to_string(),
                profile_source: if self.profile.matched_in_database {
                    "database_exact_match".to_string()
                } else {
                    "fallback_default".to_string()
                },
                capabilities,
            }),
            database_provenance: Some(DatabaseProvenance {
                source_kind,
                source_label,
                fingerprint_sha256,
                matched_device_name: self
                    .profile
                    .matched_in_database
                    .then(|| self.profile.device_name.clone()),
            }),
            subsystem_health: SubsystemHealthEvidence {
                backend_selection: if self.is_simulated {
                    format!("simulated ({:?})", self.scenario)
                } else {
                    "hardware".to_string()
                },
                acpi_wmax_available,
                usb_elc_available,
                hwmon_available,
                worker_alive,
                g_mode_recovery_pending: self.pre_gmode_restore_target.is_some()
                    && self.game_shift_latched(),
                last_error,
                usb_firmware_version,
                usb_status_summary,
            },
            telemetry: Some(TelemetryEvidence {
                thermal_mode: thermal_mode_str,
                g_mode_state,
                stealth_lights_off,
                brightness_dim: Some(100_u8.saturating_sub(self.lighting_brightness_pct)),
                cpu_temp_c,
                gpu_temp_c,
                fan_summary: vec![
                    format!(
                        "CPU fan: {} (boost {})",
                        self.telemetry.cpu_fan_rpm.formatted_value(),
                        self.telemetry.cpu_boost_pct.formatted_value()
                    ),
                    format!(
                        "GPU fan: {} (boost {})",
                        self.telemetry.gpu_fan_rpm.formatted_value(),
                        self.telemetry.gpu_boost_pct.formatted_value()
                    ),
                ],
                telemetry_stale,
                telemetry_age_ms,
            }),
            audit_stats: self.audit_stats,
            audit_entries,
        }
    }
}

/// Commands dispatched from the GUI or CLI to the background service worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceCommand {
    Refresh,
    SetThermalMode(ThermalMode),
    ToggleGMode,
    ApplyManualBoost {
        cpu_boost_pct: u8,
        gpu_boost_pct: u8,
    },
    SetCpuTurbo(bool),
    SetTccOffset(u8),
    SetFanCurve(FanCurveConfig),
    StepFanCurve,
    ApplyLighting {
        effect: LightingEffect,
        target: ZoneTarget,
        persist_boot: bool,
    },
    SetBrightness(u8),
    SetStealth(bool),
    ToggleStealth,
    SetPowerButtonColors {
        ac_color: u32,
        dc_color: u32,
        charging_color: u32,
        critical_color: u32,
    },
    SetSimScenario(SimScenario),
    ClearAuditHistory,
    Shutdown,
}

impl ServiceCommand {
    /// `true` for commands that *request* a device-state change and therefore always produce an
    /// operation outcome. `StepFanCurve` is excluded: it is a control-loop tick that only becomes
    /// an operation when it actually writes.
    #[must_use]
    pub const fn is_state_changing(&self) -> bool {
        !matches!(
            self,
            Self::Refresh
                | Self::StepFanCurve
                | Self::SetSimScenario(_)
                | Self::ClearAuditHistory
                | Self::Shutdown
        )
    }

    /// Short human description used for the `Requested` presentation.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Refresh => "Refresh".to_string(),
            Self::SetThermalMode(mode) => format!("Switch thermal mode to {mode}"),
            Self::ToggleGMode => "Toggle G-Mode".to_string(),
            Self::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            } => format!("Apply manual fan boost CPU {cpu_boost_pct}% / GPU {gpu_boost_pct}%"),
            Self::SetCpuTurbo(enabled) => format!(
                "{} CPU Turbo Boost",
                if *enabled { "Enable" } else { "Disable" }
            ),
            Self::SetTccOffset(offset) => format!("Set CPU TCC offset to {offset}°C"),
            Self::SetFanCurve(cfg) if cfg.enabled => "Apply custom fan curve".to_string(),
            Self::SetFanCurve(_) => "Stop custom fan curve".to_string(),
            Self::StepFanCurve => "Custom fan curve step".to_string(),
            Self::ApplyLighting { effect, target, .. } => {
                format!("Apply {} to {}", effect.name(), target.label())
            }
            Self::SetBrightness(pct) => format!("Set lighting brightness to {pct}%"),
            Self::SetStealth(true) => "Enable Stealth mode".to_string(),
            Self::SetStealth(false) => "Disable Stealth mode".to_string(),
            Self::ToggleStealth => "Toggle Stealth mode".to_string(),
            Self::SetPowerButtonColors { .. } => "Program power-button LED states".to_string(),
            Self::SetSimScenario(_) => "Change simulation scenario".to_string(),
            Self::ClearAuditHistory => "Clear visible hardware audit history".to_string(),
            Self::Shutdown => "Shutdown".to_string(),
        }
    }
}

/// Result of [`ThermalOrchestrator::execute`]: the snapshot plus the outcome of *this* command.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub snapshot: SystemSnapshot,
    /// `Some` for state-changing commands, mapped through the shared
    /// [`OperationLifecycle::into_result`] contract. It always describes the command that was
    /// just executed, never a lifecycle left over from an earlier operation.
    pub result: Option<Result<OperationSuccess, OperationError>>,
}

/// Which readback (already present in the post-write snapshot) can confirm an operation.
enum Check {
    /// No readback exists for this operation; the string explains why.
    None(String),
    ThermalMode(ThermalMode),
    FanBoost {
        cpu: u8,
        gpu: u8,
    },
    TccOffset(u8),
    Turbo(bool),
}

/// Intermediate result of executing the write path of an operation.
enum Settled {
    /// The operation already reached a terminal outcome (rejected, rolled back, nothing to do).
    Terminal(OperationLifecycle),
    /// The write(s) were accepted by the transport; `check` decides `Confirmed` vs `Accepted`.
    Written {
        label: String,
        summary: String,
        caveats: Vec<String>,
        check: Check,
    },
}

enum Verdict {
    Matched,
    Unverified(String),
    Mismatch(String),
}

fn verify(check: &Check, snap: &SystemSnapshot) -> Verdict {
    match check {
        Check::None(why) => Verdict::Unverified(why.clone()),
        Check::ThermalMode(target) => match &snap.thermal_mode_state {
            ThermalModeState::Known(mode) if mode == target => Verdict::Matched,
            ThermalModeState::Known(mode) => Verdict::Mismatch(format!(
                "the firmware accepted the write, but the readback reports `{mode}` instead of `{target}`"
            )),
            ThermalModeState::Unknown { reason, .. } => {
                Verdict::Unverified(format!("thermal-mode readback unavailable: {reason}"))
            }
        },
        Check::FanBoost { cpu, gpu } => {
            match (
                snap.telemetry.cpu_boost_pct.fresh_value(),
                snap.telemetry.gpu_boost_pct.fresh_value(),
            ) {
                (Some(c), Some(g)) if c == *cpu && g == *gpu => Verdict::Matched,
                (Some(c), Some(g)) => Verdict::Mismatch(format!(
                    "the firmware accepted the write, but the readback reports CPU {c}% / GPU {g}% instead of CPU {cpu}% / GPU {gpu}%"
                )),
                _ => {
                    let why = snap
                        .telemetry
                        .cpu_boost_pct
                        .problem()
                        .or_else(|| snap.telemetry.gpu_boost_pct.problem())
                        .unwrap_or("fan boost not reported");
                    Verdict::Unverified(format!("fan boost readback unavailable: {why}"))
                }
            }
        }
        Check::TccOffset(offset) => match snap.tcc_offset_c.fresh_value() {
            Some(read) if read == *offset => Verdict::Matched,
            Some(read) => Verdict::Mismatch(format!(
                "the firmware accepted the write, but the readback reports {read}°C instead of {offset}°C"
            )),
            None => Verdict::Unverified(format!(
                "TCC offset readback unavailable: {}",
                snap.tcc_offset_c.problem().unwrap_or("not reported")
            )),
        },
        Check::Turbo(enabled) => match snap.cpu_turbo.fresh() {
            Some(read) if read == enabled => Verdict::Matched,
            Some(read) => Verdict::Mismatch(format!(
                "the write command succeeded, but the readback reports turbo {} instead of {}",
                if *read { "enabled" } else { "disabled" },
                if *enabled { "enabled" } else { "disabled" },
            )),
            None => Verdict::Unverified(format!(
                "CPU turbo readback unavailable: {}",
                snap.cpu_turbo.problem().unwrap_or("not observed")
            )),
        },
    }
}

fn age_secs(then: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(then).as_secs()
}

/// Remembers the last good value of one observed quantity so a later failure can be reported as
/// an explicitly stale last-known value instead of being silently replaced.
struct Tracked<T> {
    last_good: Option<(T, Instant)>,
    attempted: bool,
}

impl<T> Default for Tracked<T> {
    fn default() -> Self {
        Self {
            last_good: None,
            attempted: false,
        }
    }
}

impl<T: Clone> Tracked<T> {
    fn resolve(&mut self, result: Result<T, String>, now: Instant) -> Observation<T> {
        self.attempted = true;
        match result {
            Ok(value) => {
                self.last_good = Some((value.clone(), now));
                Observation::Observed(value)
            }
            Err(reason) => match &self.last_good {
                Some((value, at)) => Observation::Stale {
                    last_value: value.clone(),
                    age_secs: age_secs(*at, now),
                    reason,
                },
                None => Observation::Unavailable { reason },
            },
        }
    }
}

/// Last good numeric reading with its unit.
struct SensorMemory<T> {
    last_good: Option<(T, &'static str, Instant)>,
}

impl<T> Default for SensorMemory<T> {
    fn default() -> Self {
        Self { last_good: None }
    }
}

impl<T: Copy> SensorMemory<T> {
    fn resolve(&mut self, reading: SensorReading<T>, now: Instant) -> SensorReading<T> {
        match reading {
            SensorReading::Valid { value, unit } => {
                self.last_good = Some((value, unit, now));
                SensorReading::Valid { value, unit }
            }
            SensorReading::Unavailable { reason } => match self.last_good {
                Some((last_value, unit, at)) => SensorReading::Stale {
                    last_value,
                    unit,
                    age_secs: age_secs(at, now),
                    reason,
                },
                None => SensorReading::Unavailable { reason },
            },
            stale @ SensorReading::Stale { .. } => stale,
        }
    }
}

#[derive(Default)]
struct Trackers {
    game_shift: Tracked<bool>,
    cpu_turbo: Tracked<bool>,
    elc: Tracked<ElcDiagnosticsSummary>,
    cpu_temp: SensorMemory<u16>,
    gpu_temp: SensorMemory<u16>,
    cpu_rpm: SensorMemory<u16>,
    gpu_rpm: SensorMemory<u16>,
    cpu_boost: SensorMemory<u8>,
    gpu_boost: SensorMemory<u8>,
    tcc: SensorMemory<u8>,
}

fn join_caveats(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("; ")
}

/// Caveats common to every USB write: the acknowledgement evidence and any cleanup warnings.
fn usb_caveats(receipt: &TransactionReceipt) -> Vec<String> {
    let mut caveats = vec![receipt.ack_summary()];
    caveats.extend(receipt.warnings.iter().cloned());
    caveats
}

const NO_LIGHTING_READBACK: &str = "the ELC protocol has no readback of applied lighting";

/// A fan-curve step that needs a boost write.
struct CurveWrite {
    cpu_temp: u16,
    gpu_temp: u16,
    target_cpu: u8,
    target_gpu: u8,
}

enum CurvePlan {
    NotNeeded(&'static str),
    NoTemperature(String),
    Write(CurveWrite),
}

/// Synchronous thermal & lighting state machine owning the transports and state storage.
pub struct ThermalOrchestrator {
    profile: DeviceProfile,
    transport: Box<dyn ThermalTransport>,
    storage: Box<dyn StateStorage>,
    lighting: EffectController<Box<dyn UsbHidTransport>>,
    lighting_last_effect: LightingEffect,
    lighting_last_target: ZoneTarget,
    lighting_persist_boot: bool,
    fan_curve: FanCurveConfig,
    pre_curve_mode: Option<ThermalMode>,
    last_curve_boost: Option<(u8, u8)>,
    lifecycle: OperationLifecycle,
    operation_id: u64,
    trackers: Trackers,
    elc_observation: Observation<ElcDiagnosticsSummary>,
    clock: Box<dyn Fn() -> Instant + Send>,
    chronological_audit_log: SeqRing<SequencedAudit>,
    next_audit_seq: usize,
    synced_acpi_seq: usize,
    synced_usb_seq: usize,
    trace_raw_usb: bool,
    /// When set, thermal control belongs to the background service and this orchestrator only
    /// presents its state (lighting stays local).
    remote: Option<RemoteState>,
    /// Exclusive daemon-lock held by direct maintenance mode.
    exclusive: Option<DaemonLock>,
}

impl ThermalOrchestrator {
    #[must_use]
    pub fn new(
        profile: DeviceProfile,
        transport: Box<dyn ThermalTransport>,
        storage: Box<dyn StateStorage>,
    ) -> Self {
        Self::with_lighting_transport(
            profile,
            transport,
            storage,
            Box::new(SimUsbElcTransport::new_alienware_m15_r6()),
        )
    }

    #[must_use]
    pub fn with_lighting_transport(
        profile: DeviceProfile,
        transport: Box<dyn ThermalTransport>,
        storage: Box<dyn StateStorage>,
        usb_transport: Box<dyn UsbHidTransport>,
    ) -> Self {
        let is_sim = transport.is_simulated();
        let backend_label = if is_sim { "simulated" } else { "hardware" };
        tracing::info!(
            target: "awcc_service::startup",
            category = "app",
            action = "select_backend",
            outcome = backend_label,
            code = "backend_selected",
            model = %profile.device_name,
            acpi_prefix = %profile.acpi_prefix,
            matched_in_db = profile.matched_in_database,
            db_source = %profile.database_source,
            "initialized orchestrator with {backend_label} backend for `{}` (ACPI prefix `{}`, db `{}`)",
            profile.device_name,
            profile.acpi_prefix,
            profile.database_source
        );
        let zones = profile.entry.keyboard_zones.clone();
        let lighting = EffectController::new(usb_transport, zones);
        Self {
            profile,
            transport,
            storage,
            lighting,
            lighting_last_effect: LightingEffect::Static { color: 0x00F0FF },
            lighting_last_target: ZoneTarget::All,
            lighting_persist_boot: true,
            fan_curve: FanCurveConfig::default(),
            pre_curve_mode: None,
            last_curve_boost: None,
            lifecycle: OperationLifecycle::Idle,
            operation_id: 0,
            trackers: Trackers::default(),
            elc_observation: Observation::NotObserved,
            clock: Box::new(Instant::now),
            chronological_audit_log: SeqRing::new(DEFAULT_AUDIT_RING_CAPACITY),
            next_audit_seq: 0,
            synced_acpi_seq: 0,
            synced_usb_seq: 0,
            trace_raw_usb: false,
            remote: None,
            exclusive: None,
        }
    }

    /// Construct the production orchestrator: thermal control is owned by the background service
    /// (reached over `/run/awcc/awcc.sock`), lighting uses the local USB transport.
    ///
    /// Construction performs no hardware access and no IPC: the service is contacted by the first
    /// refresh, so an absent service shows up as explicit "unavailable" state, never as a failure
    /// to start.
    pub fn new_hardware() -> Result<Self, awcc_core::CoreError> {
        let client = DaemonClient::system("awcc-client", env!("CARGO_PKG_VERSION"));
        Self::new_service_backed(Box::new(client))
    }

    /// Service-backed orchestrator with an injected remote (tests, alternative sockets).
    pub fn new_service_backed(
        remote: Box<dyn RemoteThermal>,
    ) -> Result<Self, awcc_core::CoreError> {
        let profile = resolve_host_device_profile()?;
        let mut orch = Self::with_lighting_transport(
            profile,
            Box::new(InertThermalTransport),
            Box::new(MemoryStateStorage::new(None)),
            Box::new(LinuxUsbElcTransport::new()),
        );
        orch.remote = Some(RemoteState {
            client: remote,
            view: None,
            received: None,
            error: None,
        });
        Ok(orch)
    }

    /// Explicit maintenance mode: drive the hardware directly from this process.
    ///
    /// It takes the same exclusive daemon lock, so it fails clearly (without touching hardware)
    /// while the background service is running, and the service cannot start while this process
    /// holds it. Requires root.
    pub fn new_hardware_direct() -> Result<Self, String> {
        let lock = DaemonLock::acquire(std::path::Path::new(crate::ipc::DEFAULT_LOCK_PATH), true)
            .map_err(|e| e.to_string())?;
        let candidates: Vec<PathBuf> = awcc_platform::TRUSTED_DATABASE_PATHS
            .iter()
            .map(PathBuf::from)
            .collect();
        let profile = awcc_platform::resolve_trusted_host_device_profile(&candidates, true)
            .map_err(|e| e.to_string())?;
        let transport = LinuxAcpiTransport::new(
            profile.device_name.clone(),
            profile.acpi_prefix,
            HwmonProvider::default(),
            daemon::daemon_command_executor(),
        );
        let mut orch = Self::with_lighting_transport(
            profile,
            Box::new(transport),
            Box::new(PosixStateStore::new()),
            Box::new(LinuxUsbElcTransport::new()),
        );
        orch.exclusive = Some(lock);
        Ok(orch)
    }

    /// Construct a default simulated Alienware m15 R6 orchestrator (`--sim`).
    pub fn new_simulated_m15_r6() -> Result<Self, awcc_core::CoreError> {
        let profile = resolve_simulated_m15_r6_profile()?;
        Ok(Self::new(
            profile,
            Box::new(SimBackend::new_alienware_m15_r6()),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
        ))
    }

    /// Install fine-grained simulated faults (ignored by real transports).
    pub fn set_sim_faults(&mut self, faults: SimFaults) {
        self.transport.set_sim_faults(faults);
    }

    /// Override the bounded capacity of the hardware write-audit ring (used in unit tests).
    pub fn set_audit_ring_capacity(&mut self, capacity: usize) {
        self.chronological_audit_log = SeqRing::new(capacity);
    }

    /// Explicitly opt in or out of retaining raw USB response dumps on synced audit entries.
    pub fn set_trace_raw_usb(&mut self, enabled: bool) {
        self.trace_raw_usb = enabled;
    }

    /// Clear currently visible hardware write-audit rows while preserving `lifetime_total` and
    /// recording the cleared count in [`RingStats::cleared`].
    pub fn clear_visible_audit_log(&mut self) -> RingStats {
        self.chronological_audit_log.clear();
        let stats = self.chronological_audit_log.stats();
        tracing::info!(
            target: "awcc_service::audit",
            category = "app",
            action = "clear_audit_history",
            outcome = "cleared",
            code = "audit_history_cleared",
            lifetime_total = stats.lifetime_total,
            cleared = stats.cleared,
            "cleared visible hardware write-audit history (lifetime writes={})",
            stats.lifetime_total
        );
        stats
    }

    /// Sequence-aware delta of hardware write-audit entries since `cursor`.
    #[must_use]
    pub fn audit_entries_since(
        &self,
        cursor: u64,
    ) -> awcc_diagnostics::ring::Delta<WriteAuditEntry> {
        let delta = self.chronological_audit_log.since(cursor);
        awcc_diagnostics::ring::Delta {
            items: delta.items.into_iter().map(|s| s.0).collect(),
            missed: delta.missed,
            stats: delta.stats,
        }
    }

    /// Id of the most recent state-changing operation (`0` before the first one).
    #[must_use]
    pub const fn operation_id(&self) -> u64 {
        self.operation_id
    }

    /// Actively query ELC USB diagnostics (`0x20` queries) and update the tracked observation.
    ///
    /// This is an explicit, caller-initiated query (the CLI `raw-status` command). Failure is
    /// returned with its cause instead of being discarded, and a previously good report is kept
    /// as an explicitly stale observation in later snapshots.
    pub fn refresh_elc_diagnostics(&mut self) -> Result<ElcDiagnosticsSummary, TransportError> {
        let result = self.lighting.query_diagnostics_summary();
        self.record_elc_result(&result);
        result
    }

    fn record_elc_result(&mut self, result: &Result<ElcDiagnosticsSummary, TransportError>) {
        let now = (self.clock)();
        if let Err(err) = result {
            tracing::warn!(
                target: "awcc_service::usb_elc",
                category = "diagnostic_read",
                action = "query_elc_diagnostics",
                outcome = "failed",
                code = "elc_query_failed",
                "ELC USB diagnostic query failed: {err}"
            );
        }
        self.elc_observation = self
            .trackers
            .elc
            .resolve(result.clone().map_err(|e| e.to_string()), now);
    }

    fn sync_chronological_audit_log(&mut self) {
        let current_op = (self.operation_id > 0).then_some(self.operation_id);
        let include_raw_usb =
            self.trace_raw_usb || global_diagnostics().is_some_and(|d| d.trace_raw_usb());

        let prev_acpi_seq = self.synced_acpi_seq;
        let acpi_log = self.transport.write_audit_log();
        for entry in acpi_log.iter().filter(|e| e.sequence > prev_acpi_seq) {
            self.next_audit_seq += 1;
            let seq = self.next_audit_seq;
            let op_id = entry.op_id.or(current_op);
            let synced = WriteAuditEntry {
                sequence: seq,
                op_id,
                raw_usb_response_hex: None,
                ..entry.clone()
            };
            tracing::info!(
                target: "awcc_service::audit",
                category = "hardware_write",
                op_id = op_id.unwrap_or(0),
                seq = seq as u64,
                action = %synced.command,
                outcome = %synced.outcome,
                code = if synced.succeeded { "acpi_write_ok" } else { "acpi_write_err" },
                "ACPI write #{seq}: {} -> {}",
                synced.command,
                synced.outcome
            );
            self.chronological_audit_log.push(SequencedAudit(synced));
            self.synced_acpi_seq = entry.sequence;
        }

        let prev_usb_seq = self.synced_usb_seq;
        let usb_log = self.lighting.transport().write_audit_log();
        for entry in usb_log.iter().filter(|e| e.sequence > prev_usb_seq) {
            self.next_audit_seq += 1;
            let seq = self.next_audit_seq;
            let op_id = entry.op_id.or(current_op);
            let raw_usb_response_hex = if include_raw_usb {
                entry.raw_usb_response_hex.clone()
            } else {
                None
            };
            let synced = WriteAuditEntry {
                sequence: seq,
                op_id,
                raw_usb_response_hex,
                ..entry.clone()
            };
            let ack_desc = synced.ack.describe();
            tracing::info!(
                target: "awcc_service::audit",
                category = "hardware_write",
                op_id = op_id.unwrap_or(0),
                seq = seq as u64,
                action = %synced.command,
                outcome = %synced.outcome,
                ack = %ack_desc,
                code = if synced.succeeded { "usb_write_ok" } else { "usb_write_err" },
                "USB write #{seq}: {} -> {} ({ack_desc})",
                synced.command,
                synced.outcome
            );
            self.chronological_audit_log.push(SequencedAudit(synced));
            self.synced_usb_seq = entry.sequence;
        }
    }

    /// Passively query the current state without issuing any hardware writes.
    ///
    /// ELC diagnostics are probed here only the first time (`NotObserved`); a failed probe is
    /// kept as `Unavailable` and is retried only by an explicit `Refresh` command, not after every
    /// operation.
    pub fn query_snapshot(&mut self) -> SystemSnapshot {
        if self.remote.is_some() {
            return self.remote_snapshot();
        }
        let now = (self.clock)();

        let game_shift = {
            let result = self
                .transport
                .query_game_shift_status()
                .map_err(|e| e.to_string());
            if let Err(reason) = &result {
                tracing::warn!(
                    target: "awcc_service::telemetry",
                    category = "diagnostic_read",
                    action = "query_game_shift",
                    outcome = "unavailable",
                    code = "gameshift_query_failed",
                    "GameShift latch query failed: {reason}"
                );
            }
            self.trackers.game_shift.resolve(result, now)
        };
        let thermal_mode_state = match self.transport.query_raw_thermal_mode() {
            Ok(raw) => {
                let decoded = self.profile.code_table.decode_query_response(raw);
                if let ThermalModeState::Unknown { reason, .. } = &decoded {
                    tracing::warn!(
                        target: "awcc_service::telemetry",
                        category = "diagnostic_read",
                        action = "query_thermal_mode",
                        outcome = "unknown",
                        code = "thermal_mode_unknown",
                        "thermal mode query returned unmapped/error code: {reason}"
                    );
                }
                decoded
            }
            Err(err) => {
                tracing::warn!(
                    target: "awcc_service::telemetry",
                    category = "diagnostic_read",
                    action = "query_thermal_mode",
                    outcome = "unavailable",
                    code = "thermal_mode_query_failed",
                    "thermal mode query failed: {err}"
                );
                ThermalModeState::Unknown {
                    raw_code: None,
                    reason: err.to_string(),
                }
            }
        };

        let raw = self.transport.query_telemetry();
        if let Some(problem) = raw
            .cpu_temp_c
            .problem()
            .or_else(|| raw.gpu_temp_c.problem())
            .or_else(|| raw.cpu_fan_rpm.problem())
            .or_else(|| raw.gpu_fan_rpm.problem())
        {
            tracing::warn!(
                target: "awcc_service::telemetry",
                category = "diagnostic_read",
                action = "query_telemetry",
                outcome = "degraded",
                code = "telemetry_degraded",
                "telemetry sample degraded: {problem}"
            );
        } else {
            // Normal telemetry samples are logged only at DEBUG (never at INFO).
            tracing::debug!(
                target: "awcc_service::telemetry",
                category = "diagnostic_read",
                action = "query_telemetry",
                outcome = "ok",
                code = "telemetry_sample",
                cpu_temp = %raw.cpu_temp_c.formatted_value(),
                gpu_temp = %raw.gpu_temp_c.formatted_value(),
                cpu_rpm = %raw.cpu_fan_rpm.formatted_value(),
                gpu_rpm = %raw.gpu_fan_rpm.formatted_value(),
                "telemetry sample collected"
            );
        }
        let telemetry = TelemetrySnapshot {
            cpu_temp_c: self.trackers.cpu_temp.resolve(raw.cpu_temp_c, now),
            gpu_temp_c: self.trackers.gpu_temp.resolve(raw.gpu_temp_c, now),
            cpu_fan_rpm: self.trackers.cpu_rpm.resolve(raw.cpu_fan_rpm, now),
            gpu_fan_rpm: self.trackers.gpu_rpm.resolve(raw.gpu_fan_rpm, now),
            cpu_boost_pct: self.trackers.cpu_boost.resolve(raw.cpu_boost_pct, now),
            gpu_boost_pct: self.trackers.gpu_boost.resolve(raw.gpu_boost_pct, now),
        };
        let cpu_turbo = {
            let result = self.transport.query_cpu_turbo().map_err(|e| e.to_string());
            if let Err(reason) = &result {
                tracing::warn!(
                    target: "awcc_service::telemetry",
                    category = "diagnostic_read",
                    action = "query_cpu_turbo",
                    outcome = "unavailable",
                    code = "turbo_query_failed",
                    "CPU turbo query failed: {reason}"
                );
            }
            self.trackers.cpu_turbo.resolve(result, now)
        };
        let tcc_offset_c = {
            let reading = self.transport.query_tcc_offset();
            self.trackers.tcc.resolve(reading, now)
        };

        if !self.trackers.elc.attempted {
            self.probe_elc_diagnostics();
        }

        self.sync_chronological_audit_log();
        let combined_audit: Vec<WriteAuditEntry> = self
            .chronological_audit_log
            .as_slice()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        let audit_stats = self.chronological_audit_log.stats();

        let snap = SystemSnapshot {
            profile: self.profile.clone(),
            is_simulated: self.transport.is_simulated(),
            scenario: self.transport.scenario(),
            thermal_mode_state,
            game_shift,
            pre_gmode_restore_target: self.storage.load_pre_gmode(),
            telemetry,
            cpu_turbo,
            tcc_offset_c,
            fan_curve: self.fan_curve.clone(),
            lighting_brightness_pct: self.lighting.brightness(),
            lighting_stealth_enabled: self.lighting.is_stealth(),
            lighting_state_source: self.lighting.state_source(),
            lighting_last_effect: self.lighting_last_effect,
            lighting_last_target: self.lighting_last_target.clone(),
            lighting_persist_boot: self.lighting_persist_boot,
            lighting_zone_colors: self.lighting.transport().zone_colors().clone(),
            lighting_power_colors: self.lighting.transport().power_slot_colors().clone(),
            lighting_commanded_zones: self.lighting.commanded_zones().clone(),
            lighting_commanded_power_slots: self.lighting.commanded_power_slots().clone(),
            elc_diagnostics: self.elc_observation.clone(),
            lifecycle: self.lifecycle.clone(),
            operation_id: self.operation_id,
            command_seq: 0,
            write_audit_log: combined_audit,
            audit_stats,
            service_control: None,
            service_error: None,
        };

        if let Some(diag) = global_diagnostics() {
            let evidence =
                snap.to_session_evidence(diag.session_id(), diag.build_info().clone(), true);
            diag.publish_evidence(&evidence);
        }

        snap
    }

    fn probe_elc_diagnostics(&mut self) {
        let result = self.lighting.query_diagnostics_summary();
        self.record_elc_result(&result);
    }

    /// Run one state-changing operation: assign its id, execute the write path, take the passive
    /// end-of-operation snapshot (which doubles as the readback), and settle the lifecycle
    /// against it.
    fn run_op(
        &mut self,
        label: &str,
        execute: impl FnOnce(&mut Self) -> Settled,
    ) -> SystemSnapshot {
        self.operation_id += 1;
        let id = self.operation_id;
        let started = (self.clock)();
        self.lifecycle = OperationLifecycle::Pending {
            description: label.to_string(),
        };
        let settled = execute(self);
        let mut snap = self.query_snapshot();
        let lifecycle = match settled {
            Settled::Terminal(lifecycle) => lifecycle,
            Settled::Written {
                label,
                summary,
                mut caveats,
                check,
            } => match verify(&check, &snap) {
                Verdict::Matched if caveats.is_empty() => OperationLifecycle::Confirmed { summary },
                Verdict::Matched => OperationLifecycle::Confirmed {
                    summary: format!("{summary} (note: {})", join_caveats(&caveats)),
                },
                Verdict::Unverified(why) => {
                    caveats.push(why);
                    OperationLifecycle::Accepted {
                        summary,
                        caveat: join_caveats(&caveats),
                    }
                }
                Verdict::Mismatch(detail) => {
                    caveats.insert(0, detail);
                    OperationLifecycle::Failed {
                        summary: format!("{label} was not confirmed"),
                        error: join_caveats(&caveats),
                    }
                }
            },
        };
        #[allow(clippy::cast_possible_truncation)]
        let elapsed_ms = (self.clock)()
            .saturating_duration_since(started)
            .as_millis() as u64;
        match &lifecycle {
            OperationLifecycle::Confirmed { summary } => {
                tracing::info!(
                    target: "awcc_service::operation",
                    category = "app",
                    op_id = id,
                    action = label,
                    outcome = "confirmed",
                    code = "operation_confirmed",
                    elapsed_ms = elapsed_ms,
                    "{summary}"
                );
            }
            OperationLifecycle::Accepted { summary, caveat } => {
                tracing::info!(
                    target: "awcc_service::operation",
                    category = "app",
                    op_id = id,
                    action = label,
                    outcome = "accepted",
                    code = "operation_accepted",
                    elapsed_ms = elapsed_ms,
                    caveat = %caveat,
                    "{summary} ({caveat})"
                );
            }
            OperationLifecycle::RolledBack { summary, reason } => {
                tracing::warn!(
                    target: "awcc_service::operation",
                    category = "app",
                    op_id = id,
                    action = label,
                    outcome = "rolled_back",
                    code = "operation_rolled_back",
                    elapsed_ms = elapsed_ms,
                    reason = %reason,
                    "{summary}: {reason}"
                );
            }
            OperationLifecycle::Failed { summary, error } => {
                tracing::error!(
                    target: "awcc_service::operation",
                    category = "app",
                    op_id = id,
                    action = label,
                    outcome = "failed",
                    code = "operation_failed",
                    elapsed_ms = elapsed_ms,
                    error = %error,
                    "{summary}: {error}"
                );
            }
            OperationLifecycle::Idle
            | OperationLifecycle::Requested { .. }
            | OperationLifecycle::Pending { .. } => {}
        }
        self.lifecycle = lifecycle.clone();
        snap.lifecycle = lifecycle;
        snap.operation_id = id;
        if let Some(diag) = global_diagnostics() {
            let evidence =
                snap.to_session_evidence(diag.session_id(), diag.build_info().clone(), true);
            diag.publish_evidence(&evidence);
        }
        snap
    }

    // ---------------------------------------------------------------------------------------
    // Thermal mode / G-Mode
    // ---------------------------------------------------------------------------------------

    /// Execute a thermal mode transition with two-step G-Mode rollback protection.
    pub fn set_thermal_mode(&mut self, target: ThermalMode) -> SystemSnapshot {
        self.run_op(&format!("Switch thermal mode to {target}"), |s| {
            s.exec_thermal_mode(target)
        })
    }

    fn exec_thermal_mode(&mut self, target: ThermalMode) -> Settled {
        if !self.profile.entry.supports_thermal_mode(target) {
            return Settled::Terminal(OperationLifecycle::Failed {
                summary: format!("Switch to {target} rejected"),
                error: format!(
                    "Mode `{target}` is not supported on {}",
                    self.profile.device_name
                ),
            });
        }

        // Switching to a fixed profile disables custom fan curve if one was running
        if target != ThermalMode::Manual && self.fan_curve.enabled {
            self.fan_curve.enabled = false;
            self.last_curve_boost = None;
        }

        if target == ThermalMode::GMode {
            return self.exec_enter_gmode();
        }

        // If currently in G-Mode or GameShift is latched, clear GameShift first.
        // Matching C++ `Thermals::setThermalMode`, when `target != GMode` we still proceed to
        // `WMAX 0x15 0x01` even if `0x25 0x01` returns a non-zero status code, and an unreadable
        // latch is treated as "not latched" (no write is added on an unknown state).
        let mut caveats = Vec::new();
        match self.transport.query_game_shift_status() {
            Ok(true) => {
                if let Err(err) = self.transport.set_game_shift_status(false) {
                    caveats.push(format!(
                        "clearing the GameShift latch failed ({err}); continued as the legacy implementation does"
                    ));
                }
            }
            Ok(false) => {}
            Err(err) => caveats.push(format!(
                "GameShift latch unreadable ({err}); a latched G-Mode was not cleared"
            )),
        }

        let code = self.profile.code_table.set_code_for(target);
        match self.transport.set_raw_thermal_mode(code) {
            Ok(()) => {
                if target != ThermalMode::GMode && target != ThermalMode::Manual {
                    self.storage.save_pre_gmode(target);
                }
                Settled::Written {
                    label: format!("Switch to {target}"),
                    summary: format!("Switched thermal mode to {target}"),
                    caveats,
                    check: Check::ThermalMode(target),
                }
            }
            Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                summary: format!("Failed to set thermal mode to {target}"),
                error: err.to_string(),
            }),
        }
    }

    fn exec_enter_gmode(&mut self) -> Settled {
        if let Ok(raw) = self.transport.query_raw_thermal_mode()
            && let ThermalModeState::Known(current) =
                self.profile.code_table.decode_query_response(raw)
            && current != ThermalMode::GMode
            && current != ThermalMode::Manual
        {
            self.storage.save_pre_gmode(current);
        }

        // Step 1: Enable GameShift latch (`WMAX 0x25 0x01 0x01`).
        if let Err(err) = self.transport.set_game_shift_status(true) {
            return Settled::Terminal(OperationLifecycle::Failed {
                summary: "G-Mode activation failed at Step 1".to_string(),
                error: err.to_string(),
            });
        }

        // Step 2: Set G-Mode thermal profile (`WMAX 0x15 0x01 0xAB`).
        let gmode_code = self.profile.code_table.set_code_for(ThermalMode::GMode);
        match self.transport.set_raw_thermal_mode(gmode_code) {
            Ok(()) => Settled::Written {
                label: "Engage G-Mode".to_string(),
                summary: "G-Mode (GameShift) engaged — maximum cooling active".to_string(),
                caveats: Vec::new(),
                check: Check::ThermalMode(ThermalMode::GMode),
            },
            Err(err) => match self.transport.set_game_shift_status(false) {
                Ok(()) => Settled::Terminal(OperationLifecycle::RolledBack {
                    summary: "G-Mode transition aborted & rolled back".to_string(),
                    reason: err.to_string(),
                }),
                // A rollback that itself failed must never be reported as a rollback.
                Err(rollback_err) => Settled::Terminal(OperationLifecycle::Failed {
                    summary: "G-Mode activation failed and the rollback also failed — the GameShift latch may still be enabled".to_string(),
                    error: format!("{err}; rollback: {rollback_err}"),
                }),
            },
        }
    }

    /// Toggle G-Mode on or off, restoring the prior saved thermal mode when exiting G-Mode.
    pub fn toggle_gmode(&mut self) -> SystemSnapshot {
        self.run_op("Toggle G-Mode", Self::exec_toggle_gmode)
    }

    fn exec_toggle_gmode(&mut self) -> Settled {
        let latch = self.transport.query_game_shift_status();
        let mode = self
            .transport
            .query_raw_thermal_mode()
            .map(|raw| self.profile.code_table.decode_query_response(raw));

        let in_gmode = if latch == Ok(true)
            || matches!(mode, Ok(ThermalModeState::Known(ThermalMode::GMode)))
        {
            Some(true)
        } else if latch == Ok(false) || matches!(mode, Ok(ThermalModeState::Known(_))) {
            Some(false)
        } else {
            None
        };

        match in_gmode {
            Some(true) => {
                let restore_target = self
                    .storage
                    .load_pre_gmode()
                    .unwrap_or(ThermalMode::Balanced);
                match self.exec_thermal_mode(restore_target) {
                    Settled::Written {
                        label,
                        caveats,
                        check,
                        ..
                    } => Settled::Written {
                        label,
                        summary: format!(
                            "G-Mode disengaged — restored prior thermal mode `{restore_target}`"
                        ),
                        caveats,
                        check,
                    },
                    other => other,
                }
            }
            Some(false) => self.exec_enter_gmode(),
            None => {
                let latch_err = latch
                    .err()
                    .map_or_else(|| "unknown".to_string(), |e| e.to_string());
                let mode_err = match mode {
                    Ok(ThermalModeState::Unknown { reason, .. }) => reason,
                    Err(e) => e.to_string(),
                    Ok(ThermalModeState::Known(_)) => "unknown".to_string(),
                };
                Settled::Terminal(OperationLifecycle::Failed {
                    summary: "G-Mode toggle aborted".to_string(),
                    error: format!(
                        "the current G-Mode state could not be read (latch: {latch_err}; thermal mode: {mode_err}); no write was attempted — choose the target mode explicitly"
                    ),
                })
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Fan boost / turbo / TCC
    // ---------------------------------------------------------------------------------------

    /// Explicitly apply staged manual CPU & GPU fan boost percentages (`0..=100%`).
    pub fn apply_manual_boost(&mut self, cpu_boost_pct: u8, gpu_boost_pct: u8) -> SystemSnapshot {
        let label = ServiceCommand::ApplyManualBoost {
            cpu_boost_pct,
            gpu_boost_pct,
        }
        .label();
        self.run_op(&label, |s| {
            s.exec_manual_boost(cpu_boost_pct, gpu_boost_pct)
        })
    }

    fn exec_manual_boost(&mut self, cpu_boost_pct: u8, gpu_boost_pct: u8) -> Settled {
        if let Err(err) = validate_fan_boost(u16::from(cpu_boost_pct))
            .and_then(|_| validate_fan_boost(u16::from(gpu_boost_pct)))
        {
            return Settled::Terminal(OperationLifecycle::Failed {
                summary: "Invalid manual fan boost".to_string(),
                error: err.to_string(),
            });
        }

        let mut caveats = Vec::new();
        let manual_code = self.profile.code_table.set_code_for(ThermalMode::Manual);
        match self.transport.query_game_shift_status() {
            Ok(true) => {
                if let Err(err) = self.transport.set_game_shift_status(false) {
                    caveats.push(format!(
                        "clearing the GameShift latch failed ({err}); continued as the legacy implementation does"
                    ));
                }
            }
            Ok(false) => {}
            Err(err) => caveats.push(format!(
                "GameShift latch unreadable ({err}); a latched G-Mode was not cleared"
            )),
        }
        if let Err(err) = self.transport.set_raw_thermal_mode(manual_code) {
            return Settled::Terminal(OperationLifecycle::Failed {
                summary: "Failed to switch to Manual mode before applying fan boost".to_string(),
                error: err.to_string(),
            });
        }

        match self.transport.set_fan_boost(cpu_boost_pct, gpu_boost_pct) {
            Ok(()) => Settled::Written {
                label: "Manual fan boost".to_string(),
                summary: format!(
                    "Applied Manual fan boost: CPU {cpu_boost_pct}% / GPU {gpu_boost_pct}%"
                ),
                caveats,
                check: Check::FanBoost {
                    cpu: cpu_boost_pct,
                    gpu: gpu_boost_pct,
                },
            },
            Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                summary: "Failed to apply manual fan boost".to_string(),
                error: format!("{err}; the thermal profile had already been switched to Manual"),
            }),
        }
    }

    /// Toggle CPU Turbo Boost state.
    pub fn set_cpu_turbo(&mut self, enabled: bool) -> SystemSnapshot {
        let label = ServiceCommand::SetCpuTurbo(enabled).label();
        self.run_op(&label, |s| match s.transport.set_cpu_turbo(enabled) {
            Ok(()) => Settled::Written {
                label: if enabled {
                    "Enable CPU Turbo Boost".to_string()
                } else {
                    "Disable CPU Turbo Boost".to_string()
                },
                summary: format!(
                    "CPU Turbo Boost {}",
                    if enabled { "enabled" } else { "disabled" }
                ),
                caveats: Vec::new(),
                check: Check::Turbo(enabled),
            },
            Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                summary: "Failed to toggle CPU Turbo Boost".to_string(),
                error: err.to_string(),
            }),
        })
    }

    /// Set CPU TCC Offset (`0..=15°C` reduction from TjMax) via `WMAX 0x15 0x03`.
    pub fn set_tcc_offset(&mut self, offset: u8) -> SystemSnapshot {
        let label = ServiceCommand::SetTccOffset(offset).label();
        self.run_op(&label, |s| {
            if let Err(err) = validate_tcc_offset(i32::from(offset)) {
                return Settled::Terminal(OperationLifecycle::Failed {
                    summary: "Invalid CPU TCC Offset".to_string(),
                    error: err.to_string(),
                });
            }
            match s.transport.set_tcc_offset(offset) {
                Ok(()) => {
                    let max_temp = 100_u16.saturating_sub(u16::from(offset));
                    Settled::Written {
                        label: format!("Set CPU TCC Offset to {offset}°C"),
                        summary: format!(
                            "Set CPU TCC Offset to {offset}°C (TjMax cap ~{max_temp}°C)"
                        ),
                        caveats: Vec::new(),
                        check: Check::TccOffset(offset),
                    }
                }
                Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                    summary: format!("Failed to set CPU TCC Offset to {offset}°C"),
                    error: err.to_string(),
                }),
            }
        })
    }

    // ---------------------------------------------------------------------------------------
    // Custom fan curve
    // ---------------------------------------------------------------------------------------

    /// Configure or stop the custom temperature-to-fan-boost curve.
    pub fn set_fan_curve(&mut self, config: FanCurveConfig) -> SystemSnapshot {
        let label = ServiceCommand::SetFanCurve(config.clone()).label();
        self.run_op(&label, |s| s.exec_fan_curve_config(config))
    }

    fn exec_fan_curve_config(&mut self, config: FanCurveConfig) -> Settled {
        if config.enabled {
            // Save prior non-Manual mode so we can restore it when the curve is disabled
            if self.pre_curve_mode.is_none()
                && let Ok(raw) = self.transport.query_raw_thermal_mode()
                && let ThermalModeState::Known(current) =
                    self.profile.code_table.decode_query_response(raw)
                && current != ThermalMode::Manual
                && current != ThermalMode::GMode
            {
                self.pre_curve_mode = Some(current);
            }
            self.fan_curve = config;
            self.last_curve_boost = None;
            return match self.plan_fan_curve_step() {
                CurvePlan::Write(plan) => self.exec_fan_curve_write(&plan),
                CurvePlan::NoTemperature(reason) => {
                    Settled::Terminal(OperationLifecycle::Accepted {
                        summary: "Custom fan curve configured: no fan boost has been written yet"
                            .to_string(),
                        caveat: format!(
                            "no CPU temperature reading is available to evaluate the curve ({reason})"
                        ),
                    })
                }
                CurvePlan::NotNeeded(why) => Settled::Terminal(OperationLifecycle::Accepted {
                    summary: "Custom fan curve configured: no fan boost has been written yet"
                        .to_string(),
                    caveat: why.to_string(),
                }),
            };
        }

        let was_enabled = self.fan_curve.enabled;
        self.fan_curve = config;
        self.last_curve_boost = None;
        if !was_enabled {
            return Settled::Terminal(OperationLifecycle::Accepted {
                summary: "No custom fan curve is active in this process — nothing was stopped"
                    .to_string(),
                caveat: "no hardware write was performed; a curve driven by another process (for example the GUI) is not affected by this command".to_string(),
            });
        }

        let restore = self
            .pre_curve_mode
            .take()
            .or_else(|| self.storage.load_pre_gmode())
            .unwrap_or(ThermalMode::Balanced);
        match self.exec_thermal_mode(restore) {
            Settled::Written {
                label,
                caveats,
                check,
                ..
            } => Settled::Written {
                label,
                summary: format!("Custom fan curve stopped — restored thermal profile `{restore}`"),
                caveats,
                check,
            },
            Settled::Terminal(failure) => Settled::Terminal(OperationLifecycle::Failed {
                summary: format!(
                    "Custom fan curve stopped, but restoring thermal profile `{restore}` failed"
                ),
                error: failure.detail_message(),
            }),
        }
    }

    /// `(device name, ACPI prefix)` this orchestrator controls.
    #[must_use]
    pub fn device_identity(&self) -> (String, String) {
        (
            self.profile.device_name.clone(),
            self.profile.acpi_prefix.to_string(),
        )
    }

    /// Drop the active curve *without* any hardware write or mode restoration.
    ///
    /// Used when another accepted command supersedes the curve (that command defines the mode now)
    /// or when a failed activation must not leave a half-configured curve behind.
    pub fn cancel_curve_without_restore(&mut self) {
        self.fan_curve.enabled = false;
        self.last_curve_boost = None;
        self.pre_curve_mode = None;
    }

    /// Forget the last written curve boost so the next evaluation writes unconditionally
    /// (used after resume, when the remembered hardware state can no longer be trusted).
    pub fn reset_curve_memory(&mut self) {
        self.last_curve_boost = None;
    }

    /// `true` while the orchestrator itself is applying a custom curve.
    #[must_use]
    pub const fn curve_enabled(&self) -> bool {
        self.fan_curve.enabled
    }

    /// The verified thermal profile that stopping the curve restores.
    #[must_use]
    pub fn curve_restore_mode(&self) -> ThermalMode {
        self.pre_curve_mode
            .or_else(|| self.storage.load_pre_gmode())
            .unwrap_or(ThermalMode::Balanced)
    }

    /// Evaluate the active custom fan curve against current CPU/GPU temperatures with hysteresis.
    ///
    /// A tick that needs no write is passive (no operation is recorded); a tick that writes is a
    /// normal operation with its own outcome.
    pub fn step_fan_curve(&mut self) -> SystemSnapshot {
        match self.plan_fan_curve_step() {
            CurvePlan::Write(plan) => {
                self.run_op("Custom fan curve step", |s| s.exec_fan_curve_write(&plan))
            }
            CurvePlan::NotNeeded(_) | CurvePlan::NoTemperature(_) => self.query_snapshot(),
        }
    }

    fn plan_fan_curve_step(&mut self) -> CurvePlan {
        if !self.fan_curve.enabled {
            return CurvePlan::NotNeeded("the custom fan curve is not active");
        }
        let telemetry = self.transport.query_telemetry();
        let cpu_temp = match telemetry.cpu_temp_c {
            SensorReading::Valid { value, .. } => value,
            SensorReading::Stale { reason, .. } | SensorReading::Unavailable { reason } => {
                return CurvePlan::NoTemperature(reason);
            }
        };
        let gpu_temp = match telemetry.gpu_temp_c {
            SensorReading::Valid { value, .. } => value,
            SensorReading::Stale { .. } | SensorReading::Unavailable { .. } => cpu_temp,
        };

        let target_cpu = interpolate_curve_boost(&self.fan_curve.cpu_points, cpu_temp);
        let target_gpu = interpolate_curve_boost(&self.fan_curve.gpu_points, gpu_temp);

        let min_boost_step = (self.fan_curve.hysteresis_c as u8).max(2);
        if let Some((prev_cpu, prev_gpu)) = self.last_curve_boost {
            let d_cpu = target_cpu.abs_diff(prev_cpu);
            let d_gpu = target_gpu.abs_diff(prev_gpu);
            if d_cpu < min_boost_step && d_gpu < min_boost_step {
                return CurvePlan::NotNeeded("the fan boost is within the curve hysteresis");
            }
        }
        CurvePlan::Write(CurveWrite {
            cpu_temp,
            gpu_temp,
            target_cpu,
            target_gpu,
        })
    }

    fn exec_fan_curve_write(&mut self, plan: &CurveWrite) -> Settled {
        match self.exec_manual_boost(plan.target_cpu, plan.target_gpu) {
            Settled::Written { caveats, check, .. } => {
                self.last_curve_boost = Some((plan.target_cpu, plan.target_gpu));
                Settled::Written {
                    label: "Custom fan curve step".to_string(),
                    summary: format!(
                        "Custom fan curve active: CPU {}°C→{}%, GPU {}°C→{}%",
                        plan.cpu_temp, plan.target_cpu, plan.gpu_temp, plan.target_gpu
                    ),
                    caveats,
                    check,
                }
            }
            other => other,
        }
    }

    // ---------------------------------------------------------------------------------------
    // Lighting
    // ---------------------------------------------------------------------------------------

    /// Apply a lighting effect (`0x21`/`0x23`/`0x24`/`0x27`) across `target` zones.
    pub fn apply_lighting(
        &mut self,
        effect: LightingEffect,
        target: ZoneTarget,
        persist_boot: bool,
    ) -> SystemSnapshot {
        let label = format!("Apply {} to {}", effect.name(), target.label());
        self.run_op(&label, |s| {
            s.exec_apply_lighting(effect, &target, persist_boot)
        })
    }

    fn exec_apply_lighting(
        &mut self,
        effect: LightingEffect,
        target: &ZoneTarget,
        persist_boot: bool,
    ) -> Settled {
        match self.lighting.apply_effect(effect, target, persist_boot) {
            Ok(receipt) if receipt.reports_sent == 0 => {
                Settled::Terminal(OperationLifecycle::Failed {
                    summary: format!("Failed to apply {} lighting", effect.name()),
                    error: format!(
                        "target `{}` resolved to no configured zones; nothing was sent",
                        target.label()
                    ),
                })
            }
            Ok(receipt) => {
                self.lighting_last_effect = effect;
                self.lighting_last_target = target.clone();
                self.lighting_persist_boot = persist_boot;
                let mode_str = if persist_boot {
                    "boot default"
                } else {
                    "volatile session"
                };
                Settled::Written {
                    label: format!("Apply {} to {}", effect.name(), target.label()),
                    summary: format!(
                        "Applied {} to {} ({mode_str}, {} USB reports sent)",
                        effect.name(),
                        target.label(),
                        receipt.reports_sent
                    ),
                    caveats: usb_caveats(&receipt),
                    check: Check::None(NO_LIGHTING_READBACK.to_string()),
                }
            }
            Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                summary: format!("Failed to apply {} lighting", effect.name()),
                error: err.to_string(),
            }),
        }
    }

    /// Set ELC hardware brightness (`0..=100%`) and persist to `/etc/awcc/brightness`.
    pub fn set_brightness(&mut self, brightness_pct: u8) -> SystemSnapshot {
        let label = ServiceCommand::SetBrightness(brightness_pct).label();
        self.run_op(&label, |s| {
            match s.lighting.set_brightness(brightness_pct) {
                Ok(receipt) => Settled::Written {
                    label: "Set lighting brightness".to_string(),
                    summary: format!(
                        "Set lighting brightness to {}% ({} USB reports sent)",
                        s.lighting.brightness(),
                        receipt.reports_sent
                    ),
                    caveats: usb_caveats(&receipt),
                    check: Check::None(NO_LIGHTING_READBACK.to_string()),
                },
                Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                    summary: "Failed to set lighting brightness".to_string(),
                    error: err.to_string(),
                }),
            }
        })
    }

    /// Enable or disable Stealth (`Go Dark`) mode.
    pub fn set_stealth(&mut self, enable: bool) -> SystemSnapshot {
        let label = ServiceCommand::SetStealth(enable).label();
        self.run_op(&label, |s| s.exec_stealth(enable))
    }

    fn exec_stealth(&mut self, enable: bool) -> Settled {
        match self.lighting.set_stealth(enable) {
            Ok(receipt) => {
                let summary = if enable {
                    "Stealth mode enabled — all LEDs dimmed to 0%".to_string()
                } else {
                    format!(
                        "Stealth mode disabled — brightness restored to {}%",
                        self.lighting.brightness()
                    )
                };
                Settled::Written {
                    label: "Toggle Stealth mode".to_string(),
                    summary,
                    caveats: usb_caveats(&receipt),
                    check: Check::None(NO_LIGHTING_READBACK.to_string()),
                }
            }
            Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                summary: "Failed to toggle Stealth mode".to_string(),
                error: err.to_string(),
            }),
        }
    }

    /// Program all 6 power-button state slots (`0x005B..=0x0060`) on LED `0x02`.
    pub fn set_power_button_colors(
        &mut self,
        ac_color: u32,
        dc_color: u32,
        charging_color: u32,
        critical_color: u32,
    ) -> SystemSnapshot {
        self.run_op("Program power-button LED states", |s| {
            match s.lighting.set_all_power_button_states(
                ac_color,
                dc_color,
                charging_color,
                critical_color,
            ) {
                Ok(receipt) => Settled::Written {
                    label: "Program power-button LED states".to_string(),
                    summary: format!(
                        "Programmed Power Button LED states ({} USB reports sent)",
                        receipt.reports_sent
                    ),
                    caveats: usb_caveats(&receipt),
                    check: Check::None(NO_LIGHTING_READBACK.to_string()),
                },
                Err(err) => Settled::Terminal(OperationLifecycle::Failed {
                    summary: "Failed to program power-button state slots".to_string(),
                    error: err.to_string(),
                }),
            }
        })
    }

    /// Change the active simulation scenario passively (zero hardware writes).
    pub fn set_sim_scenario(&mut self, scenario: SimScenario) -> SystemSnapshot {
        self.transport.set_scenario(scenario);
        self.lifecycle = OperationLifecycle::Idle;
        self.query_snapshot()
    }

    /// Handle a single `ServiceCommand` and return the resulting `SystemSnapshot`.
    pub fn handle_command(&mut self, cmd: ServiceCommand) -> SystemSnapshot {
        if self.remote.is_some() {
            match self.remote_command(&cmd) {
                RemoteRouting::Handled(snap) => return *snap,
                RemoteRouting::Local => {}
            }
        }
        match cmd {
            ServiceCommand::Refresh => {
                // An explicit refresh is the only passive path allowed to re-probe ELC
                // diagnostics that are not currently observed.
                if !self.elc_observation.is_observed() && self.trackers.elc.attempted {
                    self.probe_elc_diagnostics();
                }
                self.query_snapshot()
            }
            ServiceCommand::SetThermalMode(mode) => self.set_thermal_mode(mode),
            ServiceCommand::ToggleGMode => self.toggle_gmode(),
            ServiceCommand::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            } => self.apply_manual_boost(cpu_boost_pct, gpu_boost_pct),
            ServiceCommand::SetCpuTurbo(enabled) => self.set_cpu_turbo(enabled),
            ServiceCommand::SetTccOffset(offset) => self.set_tcc_offset(offset),
            ServiceCommand::SetFanCurve(cfg) => self.set_fan_curve(cfg),
            ServiceCommand::StepFanCurve => self.step_fan_curve(),
            ServiceCommand::ApplyLighting {
                effect,
                target,
                persist_boot,
            } => self.apply_lighting(effect, target, persist_boot),
            ServiceCommand::SetBrightness(pct) => self.set_brightness(pct),
            ServiceCommand::SetStealth(enable) => self.set_stealth(enable),
            ServiceCommand::ToggleStealth => {
                let next = !self.lighting.is_stealth();
                self.set_stealth(next)
            }
            ServiceCommand::SetPowerButtonColors {
                ac_color,
                dc_color,
                charging_color,
                critical_color,
            } => self.set_power_button_colors(ac_color, dc_color, charging_color, critical_color),
            ServiceCommand::SetSimScenario(scenario) => self.set_sim_scenario(scenario),
            ServiceCommand::ClearAuditHistory => {
                self.clear_visible_audit_log();
                self.query_snapshot()
            }
            ServiceCommand::Shutdown => self.query_snapshot(),
        }
    }

    /// Execute `cmd` and map its own outcome through the shared result contract.
    ///
    /// For state-changing commands `result` is derived from an operation recorded *by this call*
    /// (the operation id must have advanced); a lifecycle left over from an earlier command can
    /// never be reported as this command's success.
    pub fn execute(&mut self, cmd: ServiceCommand) -> CommandOutcome {
        let state_changing = cmd.is_state_changing();
        let before = self.operation_id;
        let snapshot = self.handle_command(cmd);
        let result = state_changing.then(|| {
            if snapshot.operation_id > before {
                snapshot.lifecycle.clone().into_result()
            } else {
                Err(OperationError::NoOutcome {
                    state: snapshot.lifecycle.badge_label(),
                })
            }
        });
        CommandOutcome { snapshot, result }
    }
}

// -------------------------------------------------------------------------------------------
// Service-backed thermal control
// -------------------------------------------------------------------------------------------

/// Client side of the background service as seen by the orchestrator.
pub trait RemoteThermal: Send {
    /// Latest authoritative state (the service decides whether to re-read hardware).
    fn refresh(&mut self) -> Result<ThermalView, ClientError>;
    /// Execute one state-changing operation to completion on the service.
    fn execute(&mut self, op: ControlOp) -> Result<(ThermalView, OperationLifecycle), ClientError>;
}

impl RemoteThermal for DaemonClient {
    fn refresh(&mut self) -> Result<ThermalView, ClientError> {
        self.snapshot(Duration::from_millis(1500))
    }

    fn execute(&mut self, op: ControlOp) -> Result<(ThermalView, OperationLifecycle), ClientError> {
        use crate::ipc::Op;
        let wire = match op {
            ControlOp::SetThermalMode(mode) => Op::SetThermalMode { mode },
            ControlOp::ToggleGMode => Op::ToggleGMode,
            ControlOp::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            } => Op::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            },
            ControlOp::SetCpuTurbo(enabled) => Op::SetCpuTurbo { enabled },
            ControlOp::SetTccOffset(offset) => Op::SetTccOffset { offset },
            ControlOp::ConfigureCurve(config) => Op::ConfigureFanCurve { config },
            ControlOp::StopCurve => Op::StopFanCurve,
            ControlOp::SetRestoreAtStartup(enabled) => Op::SetRestoreAtStartup { enabled },
        };
        DaemonClient::execute(self, wire).map(|r| (r.view, r.lifecycle))
    }
}

struct RemoteState {
    client: Box<dyn RemoteThermal>,
    view: Option<ThermalView>,
    received: Option<Instant>,
    error: Option<String>,
}

enum RemoteRouting {
    Handled(Box<SystemSnapshot>),
    Local,
}

/// Thermal transport of a service-backed orchestrator: it can never reach hardware, so every
/// read is an explicit failure and every write is rejected.
struct InertThermalTransport;

impl InertThermalTransport {
    const WHY: &'static str = "thermal control is owned by the background service";
    fn err() -> TransportError {
        TransportError::TransportUnavailable(Self::WHY.to_string())
    }
}

impl ThermalTransport for InertThermalTransport {
    fn is_simulated(&self) -> bool {
        false
    }
    fn scenario(&self) -> SimScenario {
        SimScenario::Normal
    }
    fn set_scenario(&mut self, _scenario: SimScenario) {}
    fn query_raw_thermal_mode(&mut self) -> Result<u32, TransportError> {
        Err(Self::err())
    }
    fn query_game_shift_status(&mut self) -> Result<bool, TransportError> {
        Err(Self::err())
    }
    fn set_raw_thermal_mode(&mut self, _mode_code: u16) -> Result<(), TransportError> {
        Err(Self::err())
    }
    fn set_game_shift_status(&mut self, _enabled: bool) -> Result<(), TransportError> {
        Err(Self::err())
    }
    fn query_telemetry(&mut self) -> TelemetrySnapshot {
        WireTelemetry::unavailable(Self::WHY).to_core()
    }
    fn set_fan_boost(&mut self, _cpu: u8, _gpu: u8) -> Result<(), TransportError> {
        Err(Self::err())
    }
    fn query_cpu_turbo(&mut self) -> Result<bool, TransportError> {
        Err(Self::err())
    }
    fn set_cpu_turbo(&mut self, _enabled: bool) -> Result<(), TransportError> {
        Err(Self::err())
    }
    fn query_tcc_offset(&mut self) -> SensorReading<u8> {
        SensorReading::Unavailable {
            reason: Self::WHY.to_string(),
        }
    }
    fn set_tcc_offset(&mut self, _offset: u8) -> Result<(), TransportError> {
        Err(Self::err())
    }
    fn write_audit_log(&self) -> &[WriteAuditEntry] {
        &[]
    }
}

impl ThermalOrchestrator {
    fn remote_op_for(cmd: &ServiceCommand) -> Option<ControlOp> {
        Some(match cmd {
            ServiceCommand::SetThermalMode(m) => ControlOp::SetThermalMode(*m),
            ServiceCommand::ToggleGMode => ControlOp::ToggleGMode,
            ServiceCommand::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            } => ControlOp::ApplyManualBoost {
                cpu_boost_pct: *cpu_boost_pct,
                gpu_boost_pct: *gpu_boost_pct,
            },
            ServiceCommand::SetCpuTurbo(e) => ControlOp::SetCpuTurbo(*e),
            ServiceCommand::SetTccOffset(o) => ControlOp::SetTccOffset(*o),
            ServiceCommand::SetFanCurve(cfg) if cfg.enabled => {
                ControlOp::ConfigureCurve(cfg.clone())
            }
            ServiceCommand::SetFanCurve(_) => ControlOp::StopCurve,
            _ => return None,
        })
    }

    fn remote_command(&mut self, cmd: &ServiceCommand) -> RemoteRouting {
        match cmd {
            ServiceCommand::Refresh => {
                self.remote_pull();
                RemoteRouting::Local
            }
            // The service owns the schedule; a client tick must never drive control.
            ServiceCommand::StepFanCurve => {
                RemoteRouting::Handled(Box::new(self.remote_snapshot()))
            }
            other => {
                let Some(op) = Self::remote_op_for(other) else {
                    return RemoteRouting::Local;
                };
                let label = other.label();
                self.operation_id += 1;
                let id = self.operation_id;
                let started = (self.clock)();
                let outcome = self.remote.as_mut().map(|r| r.client.execute(op));
                let lifecycle = match outcome {
                    Some(Ok((view, lifecycle))) => {
                        if let Some(r) = self.remote.as_mut() {
                            r.view = Some(view);
                            r.received = Some((self.clock)());
                            r.error = None;
                        }
                        lifecycle
                    }
                    Some(Err(err)) => {
                        let text = err.to_string();
                        if matches!(
                            err,
                            ClientError::Unavailable(_)
                                | ClientError::AccessDenied(_)
                                | ClientError::ProtocolMismatch(_)
                                | ClientError::MissingCapability(_)
                        ) && let Some(r) = self.remote.as_mut()
                        {
                            r.error = Some(text.clone());
                        }
                        OperationLifecycle::Failed {
                            summary: label.clone(),
                            error: text,
                        }
                    }
                    None => OperationLifecycle::Failed {
                        summary: label.clone(),
                        error: "no service connection".to_string(),
                    },
                };
                tracing::info!(
                    target: "awcc_service::operation",
                    category = "app",
                    op_id = id,
                    action = label,
                    outcome = lifecycle.badge_label(),
                    code = "service_operation",
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "{}",
                    lifecycle.detail_message()
                );
                self.lifecycle = lifecycle;
                RemoteRouting::Handled(Box::new(self.remote_snapshot()))
            }
        }
    }

    fn remote_pull(&mut self) {
        let now = (self.clock)();
        if let Some(r) = self.remote.as_mut() {
            match r.client.refresh() {
                Ok(view) => {
                    r.view = Some(view);
                    r.received = Some(now);
                    r.error = None;
                }
                Err(e) => r.error = Some(e.to_string()),
            }
        }
    }

    fn remote_snapshot(&mut self) -> SystemSnapshot {
        if self
            .remote
            .as_ref()
            .is_some_and(|r| r.view.is_none() && r.error.is_none())
        {
            self.remote_pull();
        }
        let now = (self.clock)();
        self.sync_chronological_audit_log();
        let mut combined_audit: Vec<WriteAuditEntry> = self
            .chronological_audit_log
            .as_slice()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        let audit_stats = self.chronological_audit_log.stats();
        let (view, error, elapsed) = match self.remote.as_ref() {
            Some(r) => (
                r.view.clone(),
                r.error.clone(),
                r.received
                    .map_or(0, |t| now.saturating_duration_since(t).as_secs()),
            ),
            None => (None, None, 0),
        };
        let waiting = "waiting for the background service";
        let reason = error.clone().unwrap_or_else(|| waiting.to_string());
        let shown = match (&view, &error) {
            (Some(v), None) => Some(v.clone()),
            (Some(v), Some(e)) => Some(v.disconnected(elapsed, e)),
            (None, _) => None,
        };
        let unavailable_obs = || Observation::Unavailable {
            reason: reason.clone(),
        };
        let (
            thermal_mode_state,
            game_shift,
            pre_gmode,
            telemetry,
            cpu_turbo,
            tcc,
            fan_curve,
            control,
        ) = match &shown {
            Some(v) => (
                v.thermal_mode_state.clone(),
                v.game_shift.to_core(),
                v.pre_gmode_restore_target,
                v.telemetry.to_core(),
                v.cpu_turbo.to_core(),
                v.tcc_offset_c.to_core(),
                v.fan_curve.clone(),
                Some(v.control.clone()),
            ),
            None => (
                ThermalView::placeholder_unavailable(&reason),
                unavailable_obs(),
                None,
                WireTelemetry::unavailable(&reason).to_core(),
                unavailable_obs(),
                WireSensor::<u8>::Unavailable {
                    reason: reason.clone(),
                }
                .to_core(),
                FanCurveConfig::default(),
                None,
            ),
        };
        if let Some(v) = &shown {
            combined_audit.extend(v.audit.iter().cloned());
        }
        let _ = WireObservation::<bool>::NotObserved;
        SystemSnapshot {
            profile: self.profile.clone(),
            is_simulated: false,
            scenario: SimScenario::Normal,
            thermal_mode_state,
            game_shift,
            pre_gmode_restore_target: pre_gmode,
            telemetry,
            cpu_turbo,
            tcc_offset_c: tcc,
            fan_curve,
            lighting_brightness_pct: self.lighting.brightness(),
            lighting_stealth_enabled: self.lighting.is_stealth(),
            lighting_state_source: self.lighting.state_source(),
            lighting_last_effect: self.lighting_last_effect,
            lighting_last_target: self.lighting_last_target.clone(),
            lighting_persist_boot: self.lighting_persist_boot,
            lighting_zone_colors: self.lighting.transport().zone_colors().clone(),
            lighting_power_colors: self.lighting.transport().power_slot_colors().clone(),
            lighting_commanded_zones: self.lighting.commanded_zones().clone(),
            lighting_commanded_power_slots: self.lighting.commanded_power_slots().clone(),
            elc_diagnostics: self.elc_observation.clone(),
            lifecycle: self.lifecycle.clone(),
            operation_id: self.operation_id,
            command_seq: 0,
            write_audit_log: combined_audit,
            audit_stats,
            service_control: control,
            service_error: error,
        }
    }
}

// -------------------------------------------------------------------------------------------
// Worker handle
// -------------------------------------------------------------------------------------------

/// Default bound of the command queue between the UI thread and the worker.
pub const DEFAULT_QUEUE_CAPACITY: usize = 32;

/// How long dropping the handle waits for the worker to finish before detaching it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Failure to start the service.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("failed to start the service worker thread: {0}")]
    ThreadStart(#[source] io::Error),
}

/// A command that was **not** queued. It is never retried automatically: the caller decides.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SendError {
    #[error("the command queue is full ({capacity} commands pending); the command was not sent")]
    QueueFull { capacity: usize },
    #[error("the service worker is not running ({0}); the command was not sent")]
    WorkerDisconnected(String),
}

/// Why the worker thread is no longer running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerFailure {
    Panicked(String),
    ExitedUnexpectedly,
}

impl std::fmt::Display for WorkerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked(message) => write!(f, "worker panicked: {message}"),
            Self::ExitedUnexpectedly => f.write_str("worker exited unexpectedly"),
        }
    }
}

/// Receipt for an accepted command: compare `seq` with [`SystemSnapshot::command_seq`] to learn
/// whether the worker has processed it. Accepted means *queued*, nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandTicket {
    pub seq: u64,
}

/// The worker's run loop, handed to the launcher given to [`ThermalServiceHandle::spawn_with`].
pub type WorkerBody = Box<dyn FnOnce() + Send + 'static>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// Non-blocking handle used by the Slint GUI thread to send commands and poll snapshots.
pub struct ThermalServiceHandle {
    cmd_tx: Option<SyncSender<ServiceCommand>>,
    snap_rx: Option<Receiver<SystemSnapshot>>,
    latest_snapshot: SystemSnapshot,
    capacity: usize,
    submitted: Mutex<u64>,
    worker_join: Mutex<Option<JoinHandle<()>>>,
    failure: Mutex<Option<WorkerFailure>>,
}

impl ThermalServiceHandle {
    /// Spawn a dedicated background worker thread owning `orchestrator`.
    ///
    /// A thread-start failure is returned with its cause instead of being discarded.
    pub fn spawn(orchestrator: ThermalOrchestrator) -> Result<Self, ServiceError> {
        Self::spawn_with(orchestrator, DEFAULT_QUEUE_CAPACITY, |body| {
            thread::Builder::new()
                .name("awcc-thermal-worker".to_string())
                .spawn(body)
        })
    }

    /// Like [`Self::spawn`] but with an explicit queue bound and thread spawner.
    ///
    /// This is the seam used to exercise start-up failure, queue saturation, disconnection, and
    /// worker panics deterministically; `spawner` must run `body` on some thread and return its
    /// handle (or the start-up error).
    pub fn spawn_with<S>(
        mut orchestrator: ThermalOrchestrator,
        capacity: usize,
        spawner: S,
    ) -> Result<Self, ServiceError>
    where
        S: FnOnce(WorkerBody) -> io::Result<JoinHandle<()>>,
    {
        let initial_snapshot = orchestrator.query_snapshot();
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<ServiceCommand>(capacity);
        let (snap_tx, snap_rx) = mpsc::sync_channel::<SystemSnapshot>(DEFAULT_QUEUE_CAPACITY);

        let body: WorkerBody = Box::new(move || {
            let mut processed = 0_u64;
            while let Ok(cmd) = cmd_rx.recv() {
                if cmd == ServiceCommand::Shutdown {
                    tracing::info!(
                        target: "awcc_service::worker",
                        category = "app",
                        action = "worker_shutdown",
                        outcome = "stopped",
                        code = "worker_shutdown",
                        "service worker stopped cleanly"
                    );
                    break;
                }
                processed += 1;
                let mut snapshot = orchestrator.handle_command(cmd);
                snapshot.command_seq = processed;
                if snap_tx.send(snapshot).is_err() {
                    break;
                }
            }
        });
        let join = spawner(body).map_err(|err| {
            tracing::error!(
                target: "awcc_service::worker",
                category = "app",
                action = "worker_spawn",
                outcome = "failed",
                code = "worker_spawn_failed",
                "failed to start service worker thread: {err}"
            );
            ServiceError::ThreadStart(err)
        })?;

        Ok(Self {
            cmd_tx: Some(cmd_tx),
            snap_rx: Some(snap_rx),
            latest_snapshot: initial_snapshot,
            capacity,
            submitted: Mutex::new(0),
            worker_join: Mutex::new(Some(join)),
            failure: Mutex::new(None),
        })
    }

    /// Collect the worker's exit reason if the thread has finished.
    fn reap_worker(&self) -> Option<WorkerFailure> {
        let mut failure = lock(&self.failure);
        if failure.is_some() {
            return failure.clone();
        }
        let mut slot = lock(&self.worker_join);
        if slot.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = slot.take()
        {
            let detected = match handle.join() {
                Err(payload) => WorkerFailure::Panicked(panic_message(payload.as_ref())),
                Ok(()) => WorkerFailure::ExitedUnexpectedly,
            };
            tracing::error!(
                target: "awcc_service::worker",
                category = "app",
                action = "worker_exit",
                outcome = "terminated",
                code = "worker_terminated",
                "service worker terminated: {detected}"
            );
            *failure = Some(detected);
        }
        failure.clone()
    }

    /// Why the worker stopped, if it is no longer running.
    #[must_use]
    pub fn worker_failure(&self) -> Option<WorkerFailure> {
        self.reap_worker()
    }

    /// Enqueue a command without blocking the UI thread.
    ///
    /// `Ok` means *queued*. A rejected command is returned as an error and is never replayed.
    pub fn send(&self, cmd: ServiceCommand) -> Result<CommandTicket, SendError> {
        let label = cmd.label();
        let Some(tx) = &self.cmd_tx else {
            return Err(SendError::WorkerDisconnected(
                "the service handle is closed".to_string(),
            ));
        };
        let mut submitted = lock(&self.submitted);
        match tx.try_send(cmd) {
            Ok(()) => {
                *submitted += 1;
                Ok(CommandTicket { seq: *submitted })
            }
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    target: "awcc_service::queue",
                    category = "app",
                    action = %label,
                    outcome = "rejected_queue_full",
                    code = "queue_full",
                    capacity = self.capacity as u64,
                    "command `{label}` rejected: queue is full ({} commands pending)",
                    self.capacity
                );
                Err(SendError::QueueFull {
                    capacity: self.capacity,
                })
            }
            Err(TrySendError::Disconnected(_)) => {
                drop(submitted);
                let reason = self
                    .reap_worker()
                    .map_or_else(|| "its queue is closed".to_string(), |f| f.to_string());
                tracing::error!(
                    target: "awcc_service::queue",
                    category = "app",
                    action = %label,
                    outcome = "rejected_worker_disconnected",
                    code = "worker_disconnected",
                    reason = %reason,
                    "command `{label}` rejected: service worker is not running ({reason})"
                );
                Err(SendError::WorkerDisconnected(reason))
            }
        }
    }

    /// Drain any newly published snapshots from the worker thread and return the newest one.
    pub fn poll_latest(&mut self) -> Option<&SystemSnapshot> {
        let mut updated = false;
        if let Some(rx) = &self.snap_rx {
            while let Ok(snap) = rx.try_recv() {
                self.latest_snapshot = snap;
                updated = true;
            }
        }
        if updated {
            Some(&self.latest_snapshot)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn current_snapshot(&self) -> &SystemSnapshot {
        &self.latest_snapshot
    }
}

impl Drop for ThermalServiceHandle {
    fn drop(&mut self) {
        // Ask the worker to stop after the commands it already accepted, then close the queue so
        // it also exits when `Shutdown` could not be enqueued because the queue was full.
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.try_send(ServiceCommand::Shutdown);
            drop(tx);
        }
        let Some(handle) = lock(&self.worker_join).take() else {
            return;
        };
        // The worker may be blocked publishing a snapshot nobody reads any more: keep draining.
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            if let Some(rx) = &self.snap_rx {
                while rx.try_recv().is_ok() {}
            }
            thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            if let Err(payload) = handle.join() {
                eprintln!(
                    "awcc: service worker panicked before shutdown: {}",
                    panic_message(payload.as_ref())
                );
            }
        } else {
            eprintln!("awcc: service worker did not stop within {SHUTDOWN_GRACE:?}; detaching it");
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use awcc_platform::AckStatus;

    use super::*;

    fn sim() -> ThermalOrchestrator {
        ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init")
    }

    fn sim_with_usb(usb: SimUsbElcTransport) -> ThermalOrchestrator {
        ThermalOrchestrator::with_lighting_transport(
            DeviceProfile::alienware_m15_r6().unwrap(),
            Box::new(SimBackend::new_alienware_m15_r6()),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
            Box::new(usb),
        )
    }

    #[test]
    fn startup_and_passive_refresh_perform_zero_hardware_writes() {
        let mut orch = sim();
        let snap1 = orch.query_snapshot();
        assert_eq!(snap1.write_count(), 0);
        assert_eq!(
            snap1.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Balanced)
        );
        assert_eq!(snap1.operation_id, 0);
        assert_eq!(snap1.lifecycle, OperationLifecycle::Idle);

        let snap2 = orch.handle_command(ServiceCommand::Refresh);
        assert_eq!(snap2.write_count(), 0);
    }

    #[test]
    fn gmode_toggle_engages_and_restores_saved_prior_profile() {
        let mut orch = sim();
        let snap = orch.set_thermal_mode(ThermalMode::Performance);
        assert_eq!(
            snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Performance)
        );
        assert!(matches!(
            snap.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));

        let gmode_snap = orch.toggle_gmode();
        assert!(gmode_snap.game_shift_latched());
        assert_eq!(
            gmode_snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::GMode)
        );
        assert_eq!(
            gmode_snap.pre_gmode_restore_target,
            Some(ThermalMode::Performance)
        );

        let restored_snap = orch.toggle_gmode();
        assert!(!restored_snap.game_shift_latched());
        assert_eq!(
            restored_snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Performance)
        );
        match &restored_snap.lifecycle {
            OperationLifecycle::Confirmed { summary } => {
                assert!(summary.contains("G-Mode disengaged"), "{summary}");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn gmode_step2_failure_rolls_back_gameshift_latch_deterministically() {
        let mut orch = sim();
        orch.set_sim_scenario(SimScenario::AcpiSetFailureRollback);

        let snap = orch.toggle_gmode();
        assert!(!snap.game_shift_latched());
        assert!(matches!(
            snap.lifecycle,
            OperationLifecycle::RolledBack { .. }
        ));
        assert_eq!(
            snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Balanced)
        );
        assert_eq!(snap.write_audit_log.len(), 3);
        assert!(snap.write_audit_log[0].command.contains("0x25 {0x01, 0x01"));
        assert!(snap.write_audit_log[1].command.contains("0x15 {0x01, 0xab"));
        assert!(!snap.write_audit_log[1].succeeded);
        assert!(snap.write_audit_log[2].command.contains("0x25 {0x01, 0x00"));
        assert!(snap.write_audit_log[2].succeeded);
    }

    #[test]
    fn failed_rollback_is_reported_as_failure_not_as_rolled_back() {
        let mut orch = sim();
        orch.set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        orch.set_sim_faults(SimFaults {
            reject_game_shift_clear: true,
            ..SimFaults::default()
        });

        let snap = orch.toggle_gmode();
        match &snap.lifecycle {
            OperationLifecycle::Failed { summary, error } => {
                assert!(summary.contains("rollback also failed"), "{summary}");
                assert!(error.contains("rollback:"), "{error}");
            }
            other => panic!("a failed rollback must not look like a clean one: {other:?}"),
        }
        assert!(snap.game_shift_latched(), "the latch really is still on");
    }

    #[test]
    fn gmode_toggle_with_unreadable_state_refuses_to_guess_a_direction() {
        let mut orch = sim();
        orch.set_sim_faults(SimFaults {
            game_shift_query_unavailable: true,
            thermal_mode_query_unavailable: true,
            ..SimFaults::default()
        });
        let outcome = orch.execute(ServiceCommand::ToggleGMode);
        let err = outcome.result.unwrap().expect_err("must not succeed");
        assert!(err.to_string().contains("no write was attempted"), "{err}");
        assert_eq!(outcome.snapshot.write_count(), 0);
    }

    #[test]
    fn manual_boost_switches_to_manual_mode_and_writes_both_fans() {
        let mut orch = sim();
        let snap = orch.apply_manual_boost(75, 65);
        assert_eq!(
            snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Manual)
        );
        assert_eq!(snap.telemetry.cpu_boost_pct.formatted_value(), "75%");
        assert_eq!(snap.telemetry.gpu_boost_pct.formatted_value(), "65%");
        assert_eq!(snap.write_count(), 3);
        assert!(matches!(
            snap.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));
    }

    #[test]
    fn fan_boost_readback_mismatch_is_a_failure_and_unreadable_is_only_accepted() {
        let mut orch = sim();
        orch.set_sim_faults(SimFaults {
            fan_boost_readback_stuck: true,
            ..SimFaults::default()
        });
        let snap = orch.apply_manual_boost(75, 65);
        match &snap.lifecycle {
            OperationLifecycle::Failed { summary, error } => {
                assert!(summary.contains("not confirmed"), "{summary}");
                assert!(error.contains("CPU 28%"), "{error}");
            }
            other => panic!("expected Failed on readback mismatch, got {other:?}"),
        }

        let mut orch = sim();
        orch.set_sim_scenario(SimScenario::SensorUnavailable);
        let snap = orch.apply_manual_boost(40, 40);
        match &snap.lifecycle {
            OperationLifecycle::Accepted { caveat, .. } => {
                assert!(
                    caveat.contains("fan boost readback unavailable"),
                    "{caveat}"
                );
            }
            other => panic!("expected Accepted without readback, got {other:?}"),
        }
    }

    #[test]
    fn tcc_offset_and_custom_fan_curve_hysteresis_and_restoration_work() {
        let mut orch = sim();

        // Set TCC offset to 8°C
        let snap_tcc = orch.set_tcc_offset(8);
        assert_eq!(snap_tcc.tcc_offset_c.formatted_value(), "8°C");
        assert!(matches!(
            snap_tcc.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));

        // Enable custom fan curve from Balanced mode
        let mut cfg = FanCurveConfig {
            enabled: true,
            ..Default::default()
        };
        let snap_curve = orch.set_fan_curve(cfg.clone());
        assert_eq!(
            snap_curve.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Manual)
        );
        let writes_after_enable = snap_curve.write_count();

        // Stepping curve again at the same temperature must be a no-op (hysteresis)
        let ops_before = orch.operation_id();
        let snap_step = orch.step_fan_curve();
        assert_eq!(snap_step.write_count(), writes_after_enable);
        assert_eq!(
            orch.operation_id(),
            ops_before,
            "a no-op tick is not an operation"
        );

        // Disabling curve restores Balanced mode
        cfg.enabled = false;
        let snap_stop = orch.set_fan_curve(cfg);
        assert_eq!(
            snap_stop.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Balanced)
        );
        assert!(matches!(
            snap_stop.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));
    }

    #[test]
    fn stopping_a_curve_that_is_not_active_here_claims_nothing_and_writes_nothing() {
        let mut orch = sim();
        // A previous, unrelated success must not be mistaken for this command's outcome.
        let first = orch.set_thermal_mode(ThermalMode::Performance);
        assert!(matches!(
            first.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));
        let writes = first.write_count();

        let outcome = orch.execute(ServiceCommand::SetFanCurve(FanCurveConfig::default()));
        let success = outcome.result.unwrap().expect("accepted, not failed");
        assert!(
            success.summary.contains("nothing was stopped"),
            "{}",
            success.summary
        );
        assert!(
            success.caveat().is_some(),
            "must not be reported as confirmed"
        );
        assert_eq!(outcome.snapshot.write_count(), writes);
        assert!(outcome.snapshot.operation_id > first.operation_id);
    }

    #[test]
    fn failed_restore_when_stopping_a_curve_is_not_reported_as_stopped() {
        let mut orch = sim();
        orch.set_fan_curve(FanCurveConfig {
            enabled: true,
            ..Default::default()
        });
        orch.set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        let snap = orch.set_fan_curve(FanCurveConfig::default());
        match &snap.lifecycle {
            OperationLifecycle::Failed { summary, .. } => {
                assert!(summary.contains("restoring thermal profile"), "{summary}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn interleaved_usb_and_acpi_writes_preserve_chronological_order() {
        let mut orch = sim();

        // 1. First perform a USB lighting write (generates multiple USB HID entries)
        let snap1 = orch.set_brightness(80);
        let usb_count = snap1.write_audit_log.len();
        assert!(usb_count >= 1);
        assert!(
            snap1
                .write_audit_log
                .last()
                .unwrap()
                .command
                .contains("USB HID")
        );

        // 2. Next perform an ACPI thermal profile write
        let snap2 = orch.set_thermal_mode(ThermalMode::Performance);
        assert_eq!(snap2.write_audit_log.len(), usb_count + 1);
        let last_after_acpi = snap2.write_audit_log.last().unwrap();
        assert_eq!(last_after_acpi.sequence, usb_count + 1);
        assert!(last_after_acpi.command.contains("WMAX 0 0x15"));

        // 3. Next perform another USB write, then a TCC offset ACPI write
        let snap3 = orch.set_brightness(60);
        assert!(
            snap3
                .write_audit_log
                .last()
                .unwrap()
                .command
                .contains("USB HID")
        );
        let snap4 = orch.set_tcc_offset(5);
        let newest = snap4.write_audit_log.last().unwrap();
        assert_eq!(newest.sequence, snap4.write_audit_log.len());
        assert!(newest.command.contains("0x15 {0x03, 0x05"));
    }

    // --- CPU turbo (regression: the CLI used to print success after a failed write) -----------

    #[test]
    fn rejected_turbo_write_is_a_failure_with_a_nonzero_result() {
        let mut orch = sim();
        orch.set_sim_faults(SimFaults {
            reject_turbo_write: true,
            ..SimFaults::default()
        });
        let outcome = orch.execute(ServiceCommand::SetCpuTurbo(false));
        let err = outcome.result.unwrap().expect_err("write was rejected");
        assert!(
            err.to_string().contains("Failed to toggle CPU Turbo Boost"),
            "{err}"
        );
        assert!(matches!(
            outcome.snapshot.lifecycle,
            OperationLifecycle::Failed { .. }
        ));
        assert!(!outcome.snapshot.write_audit_log.last().unwrap().succeeded);
    }

    #[test]
    fn turbo_is_confirmed_only_by_a_matching_readback() {
        // Healthy: write accepted and readback matches.
        let mut orch = sim();
        let outcome = orch.execute(ServiceCommand::SetCpuTurbo(false));
        let success = outcome.result.unwrap().unwrap();
        assert!(success.caveat().is_none(), "matching readback confirms");
        assert!(matches!(
            outcome.snapshot.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));

        // The device ignored the write: readback contradicts it.
        let mut orch = sim();
        orch.set_sim_faults(SimFaults {
            turbo_readback_stuck: Some(true),
            ..SimFaults::default()
        });
        let err = orch
            .execute(ServiceCommand::SetCpuTurbo(false))
            .result
            .unwrap()
            .expect_err("contradicting readback is a failure");
        assert!(
            err.to_string()
                .contains("reports turbo enabled instead of disabled"),
            "{err}"
        );

        // The readback itself failed: accepted, explicitly unconfirmed.
        let mut orch = sim();
        orch.set_sim_faults(SimFaults {
            turbo_readback_unavailable: true,
            ..SimFaults::default()
        });
        let success = orch
            .execute(ServiceCommand::SetCpuTurbo(false))
            .result
            .unwrap()
            .unwrap();
        assert!(
            success
                .caveat()
                .unwrap()
                .contains("CPU turbo readback unavailable"),
            "{:?}",
            success.caveat()
        );
    }

    // --- Stale lifecycle reuse ----------------------------------------------------------------

    #[test]
    fn passive_refresh_never_manufactures_or_advances_an_operation_outcome() {
        let mut orch = sim();
        let done = orch.set_thermal_mode(ThermalMode::Cool);
        assert!(matches!(
            done.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));

        let outcome = orch.execute(ServiceCommand::Refresh);
        assert!(outcome.result.is_none(), "refresh has no operation result");
        // The old outcome is still visible, but its id proves it is not new.
        assert_eq!(outcome.snapshot.operation_id, done.operation_id);

        let next = orch.execute(ServiceCommand::SetThermalMode(ThermalMode::Quiet));
        assert!(next.snapshot.operation_id > done.operation_id);
    }

    #[test]
    fn every_state_changing_command_gets_its_own_outcome() {
        let mut orch = sim();
        let commands = [
            ServiceCommand::SetThermalMode(ThermalMode::Performance),
            ServiceCommand::ToggleGMode,
            ServiceCommand::ApplyManualBoost {
                cpu_boost_pct: 50,
                gpu_boost_pct: 50,
            },
            ServiceCommand::SetCpuTurbo(true),
            ServiceCommand::SetTccOffset(3),
            ServiceCommand::SetFanCurve(FanCurveConfig::default()),
            ServiceCommand::SetBrightness(70),
            ServiceCommand::SetStealth(true),
            ServiceCommand::ToggleStealth,
        ];
        let mut last_id = 0;
        for cmd in commands {
            assert!(cmd.is_state_changing());
            let outcome = orch.execute(cmd.clone());
            assert!(outcome.result.is_some(), "{cmd:?}");
            assert!(outcome.snapshot.operation_id > last_id, "{cmd:?}");
            last_id = outcome.snapshot.operation_id;
        }
        assert!(!ServiceCommand::Refresh.is_state_changing());
        assert!(!ServiceCommand::StepFanCurve.is_state_changing());
    }

    // --- Unavailable / stale diagnostics -----------------------------------------------------

    fn orch_with_clock(secs: &Arc<AtomicU64>) -> ThermalOrchestrator {
        let mut orch = sim();
        let base = Instant::now();
        let secs = Arc::clone(secs);
        orch.clock = Box::new(move || base + Duration::from_secs(secs.load(Ordering::SeqCst)));
        orch
    }

    #[test]
    fn failed_queries_are_unavailable_when_never_seen_and_never_become_false_or_enabled() {
        let mut orch = sim();
        orch.set_sim_faults(SimFaults {
            game_shift_query_unavailable: true,
            turbo_readback_unavailable: true,
            ..SimFaults::default()
        });
        let snap = orch.query_snapshot();
        assert!(matches!(snap.game_shift, Observation::Unavailable { .. }));
        assert!(!snap.game_shift_latched());
        assert!(matches!(snap.cpu_turbo, Observation::Unavailable { .. }));
        assert_eq!(snap.cpu_turbo.fresh(), None);
        assert_eq!(snap.cpu_turbo.best_known(), None);
    }

    #[test]
    fn last_known_values_become_explicitly_stale_with_age_and_error_context() {
        let secs = Arc::new(AtomicU64::new(0));
        let mut orch = orch_with_clock(&secs);
        let healthy = orch.query_snapshot();
        assert_eq!(healthy.cpu_turbo, Observation::Observed(true));
        assert_eq!(healthy.game_shift, Observation::Observed(false));
        assert!(healthy.telemetry.cpu_temp_c.is_available());

        secs.store(42, Ordering::SeqCst);
        orch.set_sim_faults(SimFaults {
            game_shift_query_unavailable: true,
            turbo_readback_unavailable: true,
            ..SimFaults::default()
        });
        orch.set_sim_scenario(SimScenario::SensorUnavailable);
        let degraded = orch.query_snapshot();

        match &degraded.cpu_turbo {
            Observation::Stale {
                last_value,
                age_secs,
                reason,
            } => {
                assert!(*last_value);
                assert_eq!(*age_secs, 42);
                assert!(reason.contains("turbo"), "{reason}");
            }
            other => panic!("expected Stale turbo, got {other:?}"),
        }
        assert!(degraded.game_shift.is_stale());
        assert!(
            !degraded.game_shift_latched(),
            "stale is not fresh evidence"
        );
        match &degraded.telemetry.cpu_temp_c {
            SensorReading::Stale {
                last_value,
                age_secs,
                reason,
                ..
            } => {
                assert_eq!(*last_value, 58);
                assert_eq!(*age_secs, 42);
                assert!(reason.contains("0xFFFFFFFF"), "{reason}");
            }
            other => panic!("expected Stale cpu temp, got {other:?}"),
        }
        assert_eq!(
            degraded.telemetry.cpu_temp_c.formatted_value(),
            "58°C (stale 42s)"
        );
        assert!(!degraded.telemetry.cpu_temp_c.is_available());
    }

    #[test]
    fn stale_sensors_never_drive_the_fan_curve() {
        let mut orch = sim();
        orch.set_fan_curve(FanCurveConfig {
            enabled: true,
            ..Default::default()
        });
        let writes = orch.query_snapshot().write_count();
        orch.set_sim_scenario(SimScenario::SensorUnavailable);
        let snap = orch.step_fan_curve();
        assert_eq!(
            snap.write_count(),
            writes,
            "no write from stale/unavailable temperatures"
        );
    }

    #[test]
    fn elc_diagnostics_failure_is_unavailable_never_a_fabricated_firmware_reading() {
        let mut usb = SimUsbElcTransport::new_alienware_m15_r6();
        usb.inject_receive_failure(Some("simulated USB read timeout".to_string()));
        let mut orch = sim_with_usb(usb);

        let snap = orch.query_snapshot();
        match &snap.elc_diagnostics {
            Observation::Unavailable { reason } => {
                assert!(reason.contains("simulated USB read timeout"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert_eq!(snap.elc_diagnostics.best_known(), None);

        // A later passive snapshot must not silently retry; an explicit Refresh does.
        let again = orch.query_snapshot();
        assert!(matches!(
            again.elc_diagnostics,
            Observation::Unavailable { .. }
        ));
        let err = orch.refresh_elc_diagnostics().expect_err("still failing");
        assert!(err.to_string().contains("simulated USB read timeout"));
    }

    /// Simulated ELC whose diagnostic reads can be made to fail and heal while it is owned by the
    /// orchestrator.
    struct HealableUsb {
        inner: SimUsbElcTransport,
        failing: Arc<std::sync::atomic::AtomicBool>,
    }

    impl UsbHidTransport for HealableUsb {
        fn is_simulated(&self) -> bool {
            true
        }
        fn device_id(&self) -> (u16, u16) {
            self.inner.device_id()
        }
        fn open(&mut self) -> Result<(), TransportError> {
            self.inner.open()
        }
        fn acquire(&mut self) -> Result<(), TransportError> {
            self.inner.acquire()
        }
        fn release(&mut self) -> Result<(), TransportError> {
            self.inner.release()
        }
        fn close(&mut self) -> Result<(), TransportError> {
            self.inner.close()
        }
        fn send_report(
            &mut self,
            report: &awcc_core::lighting::HidReport33,
        ) -> Result<awcc_platform::usb_elc::SendReceipt, TransportError> {
            self.inner.send_report(report)
        }
        fn receive_report(&mut self) -> Result<awcc_core::lighting::HidReport33, TransportError> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(TransportError::TransportUnavailable(
                    "simulated USB read timeout".to_string(),
                ));
            }
            self.inner.receive_report()
        }
        fn sent_reports(&self) -> &[awcc_core::lighting::HidReport33] {
            self.inner.sent_reports()
        }
        fn write_audit_log(&self) -> &[WriteAuditEntry] {
            self.inner.write_audit_log()
        }
        fn zone_colors(&self) -> &BTreeMap<u8, u32> {
            UsbHidTransport::zone_colors(&self.inner)
        }
        fn power_slot_colors(&self) -> &BTreeMap<u16, u32> {
            UsbHidTransport::power_slot_colors(&self.inner)
        }
    }

    #[test]
    fn explicit_refresh_recovers_elc_diagnostics_and_a_later_failure_is_stale() {
        let secs = Arc::new(AtomicU64::new(0));
        let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut orch = ThermalOrchestrator::with_lighting_transport(
            DeviceProfile::alienware_m15_r6().unwrap(),
            Box::new(SimBackend::new_alienware_m15_r6()),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
            Box::new(HealableUsb {
                inner: SimUsbElcTransport::new_alienware_m15_r6(),
                failing: Arc::clone(&failing),
            }),
        );
        let base = Instant::now();
        let clock_secs = Arc::clone(&secs);
        orch.clock =
            Box::new(move || base + Duration::from_secs(clock_secs.load(Ordering::SeqCst)));

        // First probe fails: unavailable, with context, and no fabricated firmware version.
        assert!(matches!(
            orch.query_snapshot().elc_diagnostics,
            Observation::Unavailable { .. }
        ));

        // The device recovers; only an explicit Refresh re-probes.
        failing.store(false, Ordering::SeqCst);
        assert!(matches!(
            orch.query_snapshot().elc_diagnostics,
            Observation::Unavailable { .. }
        ));
        let refreshed = orch.handle_command(ServiceCommand::Refresh);
        match &refreshed.elc_diagnostics {
            Observation::Observed(diag) => assert_eq!(diag.firmware_version, (2, 0, 5)),
            other => panic!("expected Observed after explicit refresh, got {other:?}"),
        }
        assert_eq!(refreshed.operation_id, 0, "a refresh is not an operation");

        // It fails again later: the last good report is kept, explicitly stale and aged.
        failing.store(true, Ordering::SeqCst);
        secs.store(30, Ordering::SeqCst);
        assert!(orch.refresh_elc_diagnostics().is_err());
        match orch.query_snapshot().elc_diagnostics {
            Observation::Stale {
                last_value,
                age_secs,
                reason,
            } => {
                assert_eq!(last_value.firmware_version, (2, 0, 5));
                assert_eq!(age_secs, 30);
                assert!(reason.contains("simulated USB read timeout"), "{reason}");
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    // --- USB acknowledgement vs transfer ------------------------------------------------------

    #[test]
    fn lighting_never_claims_device_confirmation_even_when_every_ack_arrives() {
        let mut orch = sim();
        let snap = orch.apply_lighting(
            LightingEffect::Static { color: 0x00F0FF },
            ZoneTarget::Keyboard,
            false,
        );
        match &snap.lifecycle {
            OperationLifecycle::Accepted { summary, caveat } => {
                assert!(summary.contains("reports sent"), "{summary}");
                assert!(
                    caveat.contains("reports acknowledged by the controller"),
                    "{caveat}"
                );
                assert!(
                    caveat.contains("no readback of applied lighting"),
                    "{caveat}"
                );
            }
            other => panic!("lighting can only be Accepted, got {other:?}"),
        }
    }

    #[test]
    fn completed_transfer_with_missing_ack_is_accepted_with_the_ack_gap_stated() {
        let mut usb = SimUsbElcTransport::new_alienware_m15_r6();
        usb.inject_ack_fault(Some(AckStatus::Failed {
            reason: "timed out".to_string(),
        }));
        let mut orch = sim_with_usb(usb);
        let outcome = orch.execute(ServiceCommand::SetBrightness(40));
        let success = outcome
            .result
            .unwrap()
            .expect("a completed transfer is not a failure");
        let caveat = success.caveat().expect("unconfirmed");
        assert!(caveat.contains("0 of"), "{caveat}");
        assert!(caveat.contains("timed out"), "{caveat}");
        assert!(!matches!(
            outcome.snapshot.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));
        // The audit trail keeps the successful transfer and the failed ack apart.
        let usb_entries: Vec<_> = outcome
            .snapshot
            .write_audit_log
            .iter()
            .filter(|e| e.command.starts_with("USB HID"))
            .collect();
        assert!(!usb_entries.is_empty());
        assert!(usb_entries.iter().all(|e| e.succeeded));
        assert!(
            usb_entries
                .iter()
                .all(|e| matches!(e.ack, AckStatus::Failed { .. }))
        );
    }

    #[test]
    fn failed_usb_transfer_is_a_failure_and_partial_progress_is_stated() {
        let mut usb = SimUsbElcTransport::new_alienware_m15_r6();
        usb.inject_send_failure_from(Some(3));
        let mut orch = sim_with_usb(usb);
        let outcome = orch.execute(ServiceCommand::ApplyLighting {
            effect: LightingEffect::Static { color: 0xFF0040 },
            target: ZoneTarget::Keyboard,
            persist_boot: false,
        });
        let err = outcome.result.unwrap().expect_err("transfer failed");
        assert!(err.to_string().contains("partially updated"), "{err}");
        assert_ne!(
            outcome.snapshot.lighting_state_source,
            LightingStateSource::Commanded,
            "a failed command must not become remembered state"
        );
    }

    #[test]
    fn lighting_target_with_no_zones_is_not_reported_as_applied() {
        let mut orch = sim();
        let outcome = orch.execute(ServiceCommand::ApplyLighting {
            effect: LightingEffect::Static { color: 0xFF0040 },
            target: ZoneTarget::Custom(Vec::new()),
            persist_boot: false,
        });
        let err = outcome.result.unwrap().expect_err("nothing was sent");
        assert!(err.to_string().contains("nothing was sent"), "{err}");
    }

    #[test]
    fn lighting_provenance_moves_from_assumed_to_commanded() {
        let mut orch = sim();
        let first = orch.query_snapshot();
        assert!(matches!(
            first.lighting_state_source,
            LightingStateSource::AssumedDefault | LightingStateSource::PersistedBrightness
        ));
        assert!(first.lighting_commanded_zones.is_empty());
        assert!(first.lighting_commanded_power_slots.is_empty());

        let after_dim = orch.set_brightness(55);
        assert_eq!(
            after_dim.lighting_state_source,
            LightingStateSource::Commanded
        );
        assert!(
            after_dim.lighting_commanded_zones.is_empty(),
            "brightness alone must not mark uncommanded zone colour swatches as commanded"
        );
        assert!(after_dim.lighting_commanded_power_slots.is_empty());

        let after_kbd = orch.apply_lighting(
            LightingEffect::Static { color: 0xA855F7 },
            ZoneTarget::Keyboard,
            false,
        );
        for z in 0x04_u8..=0x07 {
            assert!(after_kbd.lighting_commanded_zones.contains(&z));
        }
        assert!(!after_kbd.lighting_commanded_zones.contains(&0x03));
        assert!(!after_kbd.lighting_commanded_zones.contains(&0x00));
        assert!(!after_kbd.lighting_commanded_zones.contains(&0x02));
        assert!(after_kbd.lighting_commanded_power_slots.is_empty());

        let after_pwr = orch.set_power_button_colors(0x00F0FF, 0x00C8FF, 0xFFB400, 0xFF2828);
        assert!(after_pwr.lighting_commanded_zones.contains(&0x02));
        assert!(
            after_pwr
                .lighting_commanded_power_slots
                .contains(&awcc_core::lighting::PowerStateSlot::AcOn.slot_id())
        );
    }

    // --- Worker handle ------------------------------------------------------------------------

    fn spawn_gated(capacity: usize) -> (ThermalServiceHandle, mpsc::Sender<()>) {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = ThermalServiceHandle::spawn_with(sim(), capacity, move |body| {
            thread::Builder::new().spawn(move || {
                // Hold the queue without draining it until released, then run the real worker.
                let _ = release_rx.recv();
                body();
            })
        })
        .expect("gated worker spawns");
        (handle, release_tx)
    }

    #[test]
    fn worker_thread_start_failure_is_returned_with_its_cause() {
        let result = ThermalServiceHandle::spawn_with(sim(), 4, |_body| {
            Err(io::Error::other(
                "simulated: resource temporarily unavailable",
            ))
        });
        match result {
            Err(ServiceError::ThreadStart(cause)) => {
                assert!(
                    cause
                        .to_string()
                        .contains("resource temporarily unavailable")
                );
            }
            Ok(_) => panic!("startup must fail"),
        }
        let message = ServiceError::ThreadStart(io::Error::other("boom")).to_string();
        assert!(
            message.contains("failed to start the service worker thread"),
            "{message}"
        );
    }

    #[test]
    fn queue_saturation_is_reported_as_queue_full_not_disconnection_and_never_replayed() {
        let (handle, release) = spawn_gated(2);
        assert_eq!(
            handle.send(ServiceCommand::Refresh),
            Ok(CommandTicket { seq: 1 })
        );
        assert_eq!(
            handle.send(ServiceCommand::Refresh),
            Ok(CommandTicket { seq: 2 })
        );

        let rejected = handle.send(ServiceCommand::SetCpuTurbo(false));
        assert_eq!(rejected, Err(SendError::QueueFull { capacity: 2 }));
        assert!(
            rejected.unwrap_err().to_string().contains("queue is full"),
            "message must say the queue is full"
        );

        // Rejected sends do not consume a ticket and must never be replayed later.
        let (mut handle, release2) = (handle, release);
        release2.send(()).unwrap();
        let mut latest = None;
        for _ in 0..400 {
            if let Some(snap) = handle.poll_latest() {
                latest = Some(snap.clone());
                if snap.command_seq == 2 {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        let latest = latest.expect("the two accepted refreshes were processed");
        assert_eq!(latest.command_seq, 2);
        assert_eq!(
            latest.write_count(),
            0,
            "the rejected write was not replayed"
        );
        assert_eq!(
            handle.send(ServiceCommand::Refresh),
            Ok(CommandTicket { seq: 3 })
        );
    }

    #[test]
    fn disconnected_worker_is_reported_as_such() {
        let handle = ThermalServiceHandle::spawn_with(sim(), 4, |body| {
            drop(body);
            thread::Builder::new().spawn(|| {})
        })
        .unwrap();
        let err = handle.send(ServiceCommand::Refresh).unwrap_err();
        assert!(matches!(err, SendError::WorkerDisconnected(_)), "{err:?}");
        assert!(err.to_string().contains("not running"));
    }

    #[test]
    fn worker_panic_is_detected_with_its_message() {
        let handle = ThermalServiceHandle::spawn_with(sim(), 4, |body| {
            thread::Builder::new().spawn(move || {
                let _held = body;
                panic!("simulated worker crash");
            })
        })
        .unwrap();

        let mut failure = None;
        for _ in 0..400 {
            failure = handle.worker_failure();
            if failure.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        match failure {
            Some(WorkerFailure::Panicked(message)) => {
                assert!(message.contains("simulated worker crash"), "{message}");
            }
            other => panic!("expected a panic failure, got {other:?}"),
        }
        let err = handle.send(ServiceCommand::Refresh).unwrap_err();
        assert!(err.to_string().contains("simulated worker crash"), "{err}");
    }

    #[test]
    fn dropping_the_handle_with_a_full_queue_does_not_deadlock() {
        let (handle, release) = spawn_gated(1);
        handle.send(ServiceCommand::Refresh).unwrap();
        assert!(matches!(
            handle.send(ServiceCommand::Refresh),
            Err(SendError::QueueFull { .. })
        ));

        let (done_tx, done_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(handle);
            let _ = done_tx.send(());
        });
        release.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("drop must finish even when Shutdown could not be queued");
        dropper.join().unwrap();
    }

    #[test]
    fn worker_stamps_snapshots_with_the_processed_command_sequence() {
        let mut handle = ThermalServiceHandle::spawn(sim()).expect("worker starts");
        assert_eq!(handle.current_snapshot().command_seq, 0);
        let ticket = handle
            .send(ServiceCommand::SetThermalMode(ThermalMode::Performance))
            .unwrap();
        assert_eq!(ticket.seq, 1);

        let mut seen = None;
        for _ in 0..400 {
            if let Some(snap) = handle.poll_latest()
                && snap.command_seq >= ticket.seq
            {
                seen = Some(snap.clone());
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let snap = seen.expect("command processed");
        assert!(matches!(
            snap.lifecycle,
            OperationLifecycle::Confirmed { .. }
        ));
        assert_eq!(snap.operation_id, 1);
    }

    #[test]
    fn full_audit_ring_continues_delivering_new_writes_at_constant_capacity_and_across_transport_eviction()
     {
        let mut orch = sim();
        orch.set_audit_ring_capacity(4);
        let mut cursor = 0_u64;

        // Execute 300 TCC offset writes so we exceed both the 4-entry orchestrator ring capacity
        // AND the 256-entry transport audit buffer (`MAX_TRANSPORT_AUDIT_ENTRIES`).
        for i in 1..=300_usize {
            let offset = u8::try_from(i % 15).unwrap();
            let snap = orch.set_tcc_offset(offset);
            assert!(
                snap.write_audit_log.len() <= 4,
                "retained ring stays bounded at capacity 4"
            );
            assert_eq!(
                snap.write_count(),
                i,
                "lifetime write_count grows monotonically"
            );
            assert_eq!(snap.audit_stats.lifetime_total, i as u64);
            assert_eq!(snap.audit_stats.last_seq, i as u64);

            let delta = orch.audit_entries_since(cursor);
            assert_eq!(
                delta.items.len(),
                1,
                "sequence cursor receives every new write at step {i}"
            );
            assert_eq!(delta.items[0].sequence, i);
            assert_eq!(delta.missed, 0);
            cursor = delta.items[0].sequence as u64;
        }
    }

    #[test]
    fn clear_visible_audit_log_preserves_lifetime_write_count_and_tracks_cleared_entries() {
        let mut orch = sim();
        orch.set_tcc_offset(4);
        orch.set_tcc_offset(6);
        let before = orch.set_tcc_offset(8);
        assert_eq!(before.write_audit_log.len(), 3);
        assert_eq!(before.write_count(), 3);

        let cleared = orch.handle_command(ServiceCommand::ClearAuditHistory);
        assert!(cleared.write_audit_log.is_empty());
        assert_eq!(
            cleared.write_count(),
            3,
            "lifetime write_count survives clearing visible history"
        );
        assert_eq!(cleared.audit_stats.lifetime_total, 3);
        assert_eq!(cleared.audit_stats.retained, 0);
        assert_eq!(cleared.audit_stats.cleared, 3);

        // Next write gets sequence 4, never resetting to 1
        let after = orch.set_tcc_offset(10);
        assert_eq!(after.write_audit_log.len(), 1);
        assert_eq!(after.write_audit_log[0].sequence, 4);
        assert_eq!(after.write_count(), 4);
    }

    #[test]
    fn operation_id_propagates_to_hardware_write_audit_entries_including_gmode_rollback() {
        let mut orch = sim();
        let snap1 = orch.set_thermal_mode(ThermalMode::Performance);
        assert_eq!(snap1.operation_id, 1);
        assert_eq!(snap1.write_audit_log.last().unwrap().op_id, Some(1));

        orch.set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        let snap2 = orch.toggle_gmode();
        assert_eq!(snap2.operation_id, 2);
        assert!(matches!(
            snap2.lifecycle,
            OperationLifecycle::RolledBack { .. }
        ));
        let op2_entries: Vec<_> = snap2
            .write_audit_log
            .iter()
            .filter(|e| e.op_id == Some(2))
            .collect();
        assert_eq!(
            op2_entries.len(),
            3,
            "Step 1, failed Step 2, and rollback Step 3 all share op_id=2"
        );
    }

    #[test]
    fn raw_usb_response_hex_is_omitted_by_default_and_included_when_opted_in() {
        let mut orch = sim();
        let snap_default = orch.set_brightness(70);
        assert!(
            snap_default
                .write_audit_log
                .iter()
                .all(|e| e.raw_usb_response_hex.is_none()),
            "verbose raw USB response dumps must be omitted by default"
        );

        let mut orch_opt_in = sim();
        orch_opt_in.set_trace_raw_usb(true);
        let snap_opt_in = orch_opt_in.set_brightness(70);
        assert!(
            snap_opt_in
                .write_audit_log
                .iter()
                .any(|e| e.raw_usb_response_hex.is_some()),
            "explicit opt-in includes raw USB response hex"
        );
    }

    #[test]
    fn to_session_evidence_records_database_fingerprint_and_subsystem_health() {
        let mut orch = sim();
        let snap = orch.set_thermal_mode(ThermalMode::Performance);
        let build = BuildIdentityReport::current("awcc-test");
        let evidence = snap.to_session_evidence("s-test", build, true);
        assert_eq!(evidence.session_id, "s-test");
        let prof = evidence.device_profile.expect("device profile present");
        assert_eq!(prof.model_name, "Alienware m15 R6");
        assert_eq!(prof.acpi_prefix, "AMWW");
        let db = evidence
            .database_provenance
            .expect("database provenance present");
        assert_eq!(db.source_kind, "compiled_in_default");
        assert_eq!(db.fingerprint_sha256.len(), 64);
        assert!(evidence.subsystem_health.acpi_wmax_available);
        assert!(evidence.subsystem_health.usb_elc_available);
        assert!(!evidence.audit_entries.is_empty());
    }
}
