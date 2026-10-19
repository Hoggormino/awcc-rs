//! Non-blocking thermal/fan/lighting orchestration service, daemon IPC, and hotkey state machine.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use awcc_core::{
    DeviceProfile, FanCurveConfig, OperationLifecycle, SensorReading, SimScenario,
    TelemetrySnapshot, ThermalMode, ThermalModeState, interpolate_curve_boost,
    lighting::{LightingEffect, ZoneTarget},
    validate_fan_boost, validate_tcc_offset,
};
pub use awcc_platform::resolve_host_device_profile;
use awcc_platform::{
    HwmonProvider, LinuxAcpiTransport, MemoryStateStorage, PosixStateStore, SimBackend,
    StateStorage, ThermalTransport, WriteAuditEntry,
    usb_elc::{
        EffectController, ElcDiagnosticsSummary, LinuxUsbElcTransport, SimUsbElcTransport,
        UsbHidTransport,
    },
};

pub mod daemon;
pub mod keybinder;

/// Immutable snapshot of the system state published to the GUI or CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSnapshot {
    pub profile: DeviceProfile,
    pub is_simulated: bool,
    pub scenario: SimScenario,
    pub thermal_mode_state: ThermalModeState,
    pub game_shift_enabled: bool,
    pub pre_gmode_restore_target: Option<ThermalMode>,
    pub telemetry: TelemetrySnapshot,
    pub cpu_turbo_enabled: bool,
    pub tcc_offset_c: SensorReading<u8>,
    pub fan_curve: FanCurveConfig,
    pub lighting_brightness_pct: u8,
    pub lighting_stealth_enabled: bool,
    pub lighting_last_effect: LightingEffect,
    pub lighting_last_target: ZoneTarget,
    pub lighting_persist_boot: bool,
    pub lighting_zone_colors: BTreeMap<u8, u32>,
    pub lighting_power_colors: BTreeMap<u16, u32>,
    pub elc_diagnostics: Option<ElcDiagnosticsSummary>,
    pub lifecycle: OperationLifecycle,
    pub write_audit_log: Vec<WriteAuditEntry>,
}

impl SystemSnapshot {
    /// Count of mutating hardware/simulated writes executed since startup.
    #[must_use]
    pub fn write_count(&self) -> usize {
        self.write_audit_log.len()
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
    Shutdown,
}

/// Synchronous thermal & lighting state machine owning the transports and state storage.
pub struct ThermalOrchestrator {
    profile: DeviceProfile,
    transport: Box<dyn ThermalTransport>,
    storage: Box<dyn StateStorage>,
    lighting: EffectController<Box<dyn UsbHidTransport>>,
    cached_elc_diagnostics: Option<ElcDiagnosticsSummary>,
    lighting_last_effect: LightingEffect,
    lighting_last_target: ZoneTarget,
    lighting_persist_boot: bool,
    fan_curve: FanCurveConfig,
    pre_curve_mode: Option<ThermalMode>,
    last_curve_boost: Option<(u8, u8)>,
    lifecycle: OperationLifecycle,
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
        let zones = profile.entry.keyboard_zones.clone();
        let lighting = EffectController::new(usb_transport, zones);
        Self {
            profile,
            transport,
            storage,
            lighting,
            cached_elc_diagnostics: None,
            lighting_last_effect: LightingEffect::Static { color: 0x00F0FF },
            lighting_last_target: ZoneTarget::All,
            lighting_persist_boot: true,
            fan_curve: FanCurveConfig::default(),
            pre_curve_mode: None,
            last_curve_boost: None,
            lifecycle: OperationLifecycle::Idle,
        }
    }

    /// Construct a production Linux hardware orchestrator backed by `LinuxAcpiTransport`
    /// (`/tmp/awcc.sock` + `/proc/acpi/call`), `HwmonProvider`, `PosixStateStore`, and
    /// `LinuxUsbElcTransport` (`187c:0550` / `0x0551`).
    pub fn new_hardware() -> Result<Self, awcc_core::CoreError> {
        let profile = resolve_host_device_profile()?;
        let executor =
            daemon::default_linux_command_executor(PathBuf::from(daemon::DEFAULT_SOCKET_PATH));
        let acpi_transport = LinuxAcpiTransport::new(
            profile.device_name.clone(),
            profile.acpi_prefix,
            HwmonProvider::default(),
            executor,
        );
        let state_store = PosixStateStore::new();
        let usb_transport = LinuxUsbElcTransport::new();
        Ok(Self::with_lighting_transport(
            profile,
            Box::new(acpi_transport),
            Box::new(state_store),
            Box::new(usb_transport),
        ))
    }

    /// Construct a default simulated Alienware m15 R6 orchestrator (`--sim`).
    pub fn new_simulated_m15_r6() -> Result<Self, awcc_core::CoreError> {
        let profile = DeviceProfile::alienware_m15_r6()?;
        Ok(Self::new(
            profile,
            Box::new(SimBackend::new_alienware_m15_r6()),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
        ))
    }

    /// Actively refresh ELC USB diagnostics (`0x20` queries) and update the cached summary.
    pub fn refresh_elc_diagnostics(&mut self) -> Option<ElcDiagnosticsSummary> {
        let diag = self.lighting.query_diagnostics_summary().ok();
        if diag.is_some() {
            self.cached_elc_diagnostics = diag.clone();
        }
        diag
    }

    /// Passively query the current state without issuing any hardware writes.
    pub fn query_snapshot(&mut self) -> SystemSnapshot {
        let game_shift_enabled = self.transport.query_game_shift_status().unwrap_or(false);
        let thermal_mode_state = match self.transport.query_raw_thermal_mode() {
            Ok(raw) => self.profile.code_table.decode_query_response(raw),
            Err(err) => ThermalModeState::Unknown {
                raw_code: None,
                reason: err.to_string(),
            },
        };

        let telemetry = self.transport.query_telemetry();
        let cpu_turbo_enabled = self.transport.query_cpu_turbo();
        let tcc_offset_c = self.transport.query_tcc_offset();
        if self.cached_elc_diagnostics.is_none() {
            self.cached_elc_diagnostics = self.lighting.query_diagnostics_summary().ok();
        }
        let elc_diagnostics = self.cached_elc_diagnostics.clone();

        let mut combined_audit = self.transport.write_audit_log().to_vec();
        for usb_entry in self.lighting.transport().write_audit_log() {
            let seq = combined_audit.len() + 1;
            combined_audit.push(WriteAuditEntry {
                sequence: seq,
                command: usb_entry.command.clone(),
                outcome: usb_entry.outcome.clone(),
                succeeded: usb_entry.succeeded,
            });
        }

        SystemSnapshot {
            profile: self.profile.clone(),
            is_simulated: self.transport.is_simulated(),
            scenario: self.transport.scenario(),
            thermal_mode_state,
            game_shift_enabled,
            pre_gmode_restore_target: self.storage.load_pre_gmode(),
            telemetry,
            cpu_turbo_enabled,
            tcc_offset_c,
            fan_curve: self.fan_curve.clone(),
            lighting_brightness_pct: self.lighting.brightness(),
            lighting_stealth_enabled: self.lighting.is_stealth(),
            lighting_last_effect: self.lighting_last_effect,
            lighting_last_target: self.lighting_last_target.clone(),
            lighting_persist_boot: self.lighting_persist_boot,
            lighting_zone_colors: self.lighting.transport().zone_colors().clone(),
            lighting_power_colors: self.lighting.transport().power_slot_colors().clone(),
            elc_diagnostics,
            lifecycle: self.lifecycle.clone(),
            write_audit_log: combined_audit,
        }
    }

    /// Execute a thermal mode transition with two-step G-Mode rollback protection.
    pub fn set_thermal_mode(&mut self, target: ThermalMode) -> SystemSnapshot {
        if !self.profile.entry.supports_thermal_mode(target) {
            self.lifecycle = OperationLifecycle::Failed {
                summary: format!("Switch to {target} rejected"),
                error: format!(
                    "Mode `{target}` is not supported on {}",
                    self.profile.device_name
                ),
            };
            return self.query_snapshot();
        }

        // Switching to a fixed profile disables custom fan curve if one was running
        if target != ThermalMode::Manual && self.fan_curve.enabled {
            self.fan_curve.enabled = false;
            self.last_curve_boost = None;
        }

        if target == ThermalMode::GMode {
            return self.enter_gmode();
        }

        // If currently in G-Mode or GameShift is latched, clear GameShift first.
        // Matching C++ `Thermals::setThermalMode`, when `target != GMode` we still proceed to
        // `WMAX 0x15 0x01` even if `0x25 0x01` returns a non-zero status code.
        let currently_gmode = self.transport.query_game_shift_status().unwrap_or(false);
        if currently_gmode {
            let _ = self.transport.set_game_shift_status(false);
        }

        let code = self.profile.code_table.set_code_for(target);
        match self.transport.set_raw_thermal_mode(code) {
            Ok(()) => {
                if target != ThermalMode::GMode && target != ThermalMode::Manual {
                    self.storage.save_pre_gmode(target);
                }
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!("Switched thermal mode to {target} (0x{code:02X})"),
                };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: format!("Failed to set thermal mode to {target} (0x{code:02X})"),
                    error: err.to_string(),
                };
            }
        }

        self.query_snapshot()
    }

    fn enter_gmode(&mut self) -> SystemSnapshot {
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
            self.lifecycle = OperationLifecycle::Failed {
                summary: "G-Mode activation failed at Step 1 (WMAX 0x25)".to_string(),
                error: err.to_string(),
            };
            return self.query_snapshot();
        }

        // Step 2: Set G-Mode thermal profile (`WMAX 0x15 0x01 0xAB`).
        let gmode_code = self.profile.code_table.set_code_for(ThermalMode::GMode);
        match self.transport.set_raw_thermal_mode(gmode_code) {
            Ok(()) => {
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!(
                        "G-Mode (GameShift) engaged — WMAX 0x25=0x01 + 0x15=0x{gmode_code:02X}"
                    ),
                };
            }
            Err(err) => {
                let _ = self.transport.set_game_shift_status(false);
                self.lifecycle = OperationLifecycle::RolledBack {
                    summary: "G-Mode transition aborted & rolled back (WMAX 0x25 restored to 0x00)"
                        .to_string(),
                    reason: err.to_string(),
                };
            }
        }

        self.query_snapshot()
    }

    /// Toggle G-Mode on or off, restoring the prior saved thermal mode when exiting G-Mode.
    pub fn toggle_gmode(&mut self) -> SystemSnapshot {
        let in_gmode = self.transport.query_game_shift_status().unwrap_or(false)
            || matches!(
                self.transport
                    .query_raw_thermal_mode()
                    .map(|r| self.profile.code_table.decode_query_response(r)),
                Ok(ThermalModeState::Known(ThermalMode::GMode))
            );

        if in_gmode {
            let restore_target = self
                .storage
                .load_pre_gmode()
                .unwrap_or(ThermalMode::Balanced);
            let snap = self.set_thermal_mode(restore_target);
            if matches!(self.lifecycle, OperationLifecycle::Confirmed { .. }) {
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!(
                        "G-Mode disengaged — restored prior thermal mode `{restore_target}`"
                    ),
                };
                return self.query_snapshot();
            }
            snap
        } else {
            self.enter_gmode()
        }
    }

    /// Explicitly apply staged manual CPU & GPU fan boost percentages (`0..=100%`).
    pub fn apply_manual_boost(&mut self, cpu_boost_pct: u8, gpu_boost_pct: u8) -> SystemSnapshot {
        if let Err(err) = validate_fan_boost(u16::from(cpu_boost_pct))
            .and_then(|_| validate_fan_boost(u16::from(gpu_boost_pct)))
        {
            self.lifecycle = OperationLifecycle::Failed {
                summary: "Invalid manual fan boost".to_string(),
                error: err.to_string(),
            };
            return self.query_snapshot();
        }

        let manual_code = self.profile.code_table.set_code_for(ThermalMode::Manual);
        if self.transport.query_game_shift_status().unwrap_or(false) {
            let _ = self.transport.set_game_shift_status(false);
        }
        if let Err(err) = self.transport.set_raw_thermal_mode(manual_code) {
            self.lifecycle = OperationLifecycle::Failed {
                summary: "Failed to switch to Manual mode before applying fan boost".to_string(),
                error: err.to_string(),
            };
            return self.query_snapshot();
        }

        match self.transport.set_fan_boost(cpu_boost_pct, gpu_boost_pct) {
            Ok(()) => {
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!(
                        "Applied Manual fan boost: CPU {cpu_boost_pct}% / GPU {gpu_boost_pct}%"
                    ),
                };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: "Failed to apply manual fan boost".to_string(),
                    error: err.to_string(),
                };
            }
        }

        self.query_snapshot()
    }

    /// Toggle CPU Turbo Boost state.
    pub fn set_cpu_turbo(&mut self, enabled: bool) -> SystemSnapshot {
        match self.transport.set_cpu_turbo(enabled) {
            Ok(()) => {
                let state_str = if enabled { "enabled" } else { "disabled" };
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!("CPU Turbo Boost {state_str}"),
                };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: "Failed to toggle CPU Turbo Boost".to_string(),
                    error: err.to_string(),
                };
            }
        }
        self.query_snapshot()
    }

    /// Set CPU TCC Offset (`0..=15°C` reduction from TjMax) via `WMAX 0x15 0x03`.
    pub fn set_tcc_offset(&mut self, offset: u8) -> SystemSnapshot {
        if let Err(err) = validate_tcc_offset(i32::from(offset)) {
            self.lifecycle = OperationLifecycle::Failed {
                summary: "Invalid CPU TCC Offset".to_string(),
                error: err.to_string(),
            };
            return self.query_snapshot();
        }

        match self.transport.set_tcc_offset(offset) {
            Ok(()) => {
                let max_temp = 100_u16.saturating_sub(u16::from(offset));
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!("Set CPU TCC Offset to {offset}°C (TjMax cap ~{max_temp}°C)"),
                };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: format!("Failed to set CPU TCC Offset to {offset}°C"),
                    error: err.to_string(),
                };
            }
        }
        self.query_snapshot()
    }

    /// Configure or stop the custom temperature-to-fan-boost curve.
    pub fn set_fan_curve(&mut self, config: FanCurveConfig) -> SystemSnapshot {
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
            self.step_fan_curve()
        } else {
            let was_enabled = self.fan_curve.enabled;
            self.fan_curve = config;
            self.last_curve_boost = None;
            if was_enabled {
                let restore = self
                    .pre_curve_mode
                    .take()
                    .or_else(|| self.storage.load_pre_gmode())
                    .unwrap_or(ThermalMode::Balanced);
                let _ = self.set_thermal_mode(restore);
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!(
                        "Custom fan curve stopped — restored thermal profile `{restore}`"
                    ),
                };
            }
            self.query_snapshot()
        }
    }

    /// Evaluate the active custom fan curve against current CPU/GPU temperatures with hysteresis.
    pub fn step_fan_curve(&mut self) -> SystemSnapshot {
        if !self.fan_curve.enabled {
            return self.query_snapshot();
        }
        let telemetry = self.transport.query_telemetry();
        let cpu_temp = match telemetry.cpu_temp_c {
            SensorReading::Valid { value, .. } => value,
            SensorReading::Stale { .. } | SensorReading::Unavailable { .. } => {
                return self.query_snapshot();
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
                return self.query_snapshot();
            }
        }

        let snap = self.apply_manual_boost(target_cpu, target_gpu);
        if matches!(self.lifecycle, OperationLifecycle::Confirmed { .. }) {
            self.last_curve_boost = Some((target_cpu, target_gpu));
            self.lifecycle = OperationLifecycle::Confirmed {
                summary: format!(
                    "Custom fan curve active: CPU {cpu_temp}°C→{target_cpu}%, GPU {gpu_temp}°C→{target_gpu}%"
                ),
            };
            return self.query_snapshot();
        }
        snap
    }

    /// Apply a lighting effect (`0x21`/`0x23`/`0x24`/`0x27`) across `target` zones.
    pub fn apply_lighting(
        &mut self,
        effect: LightingEffect,
        target: ZoneTarget,
        persist_boot: bool,
    ) -> SystemSnapshot {
        match self.lighting.apply_effect(effect, &target, persist_boot) {
            Ok(reports_sent) => {
                self.lighting_last_effect = effect;
                self.lighting_last_target = target.clone();
                self.lighting_persist_boot = persist_boot;
                let mode_str = if persist_boot {
                    "boot default slot 0x0061"
                } else {
                    "volatile session"
                };
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!(
                        "Applied {} to {} ({mode_str}, {reports_sent} USB HID reports)",
                        effect.name(),
                        target.label()
                    ),
                };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: format!("Failed to apply {} lighting", effect.name()),
                    error: err.to_string(),
                };
            }
        }
        self.query_snapshot()
    }

    /// Set ELC hardware brightness (`0..=100%`) and persist to `/etc/awcc/brightness`.
    pub fn set_brightness(&mut self, brightness_pct: u8) -> SystemSnapshot {
        match self.lighting.set_brightness(brightness_pct) {
            Ok(reports_sent) => {
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!(
                        "Set ELC lighting brightness to {}% ({reports_sent} USB HID reports)",
                        self.lighting.brightness()
                    ),
                };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: "Failed to set lighting brightness".to_string(),
                    error: err.to_string(),
                };
            }
        }
        self.query_snapshot()
    }

    /// Enable or disable Stealth (`Go Dark`) mode.
    pub fn set_stealth(&mut self, enable: bool) -> SystemSnapshot {
        match self.lighting.set_stealth(enable) {
            Ok(active) => {
                let msg = if active {
                    "Stealth mode (Go Dark) enabled — all LEDs dimmed to 0%".to_string()
                } else {
                    format!(
                        "Stealth mode disabled — restored brightness to {}%",
                        self.lighting.brightness()
                    )
                };
                self.lifecycle = OperationLifecycle::Confirmed { summary: msg };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: "Failed to toggle Stealth mode".to_string(),
                    error: err.to_string(),
                };
            }
        }
        self.query_snapshot()
    }

    /// Program all 6 power-button state slots (`0x005B..=0x0060`) on LED `0x02`.
    pub fn set_power_button_colors(
        &mut self,
        ac_color: u32,
        dc_color: u32,
        charging_color: u32,
        critical_color: u32,
    ) -> SystemSnapshot {
        match self.lighting.set_all_power_button_states(
            ac_color,
            dc_color,
            charging_color,
            critical_color,
        ) {
            Ok(reports_sent) => {
                self.lifecycle = OperationLifecycle::Confirmed {
                    summary: format!(
                        "Programmed 6 power-button state slots (0x005B..0x0060) on LED 0x02 ({reports_sent} USB HID reports)"
                    ),
                };
            }
            Err(err) => {
                self.lifecycle = OperationLifecycle::Failed {
                    summary: "Failed to program power-button state slots".to_string(),
                    error: err.to_string(),
                };
            }
        }
        self.query_snapshot()
    }

    /// Change the active simulation scenario passively (zero hardware writes).
    pub fn set_sim_scenario(&mut self, scenario: SimScenario) -> SystemSnapshot {
        self.transport.set_scenario(scenario);
        self.lifecycle = OperationLifecycle::Idle;
        self.query_snapshot()
    }

    /// Handle a single `ServiceCommand` and return the resulting `SystemSnapshot`.
    pub fn handle_command(&mut self, cmd: ServiceCommand) -> SystemSnapshot {
        match cmd {
            ServiceCommand::Refresh => self.query_snapshot(),
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
            ServiceCommand::Shutdown => self.query_snapshot(),
        }
    }
}

/// Non-blocking handle used by the Slint GUI thread to send commands and poll snapshots.
pub struct ThermalServiceHandle {
    cmd_tx: SyncSender<ServiceCommand>,
    snap_rx: Receiver<SystemSnapshot>,
    latest_snapshot: SystemSnapshot,
    worker_join: Option<JoinHandle<()>>,
}

impl ThermalServiceHandle {
    /// Spawn a dedicated background worker thread owning `orchestrator`.
    #[must_use]
    pub fn spawn(mut orchestrator: ThermalOrchestrator) -> Self {
        let initial_snapshot = orchestrator.query_snapshot();
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<ServiceCommand>(32);
        let (snap_tx, snap_rx) = mpsc::sync_channel::<SystemSnapshot>(32);

        let join = thread::Builder::new()
            .name("awcc-thermal-worker".to_string())
            .spawn(move || {
                while let Ok(cmd) = cmd_rx.recv() {
                    if cmd == ServiceCommand::Shutdown {
                        break;
                    }
                    let snapshot = orchestrator.handle_command(cmd);
                    if snap_tx.send(snapshot).is_err() {
                        break;
                    }
                }
            })
            .ok();

        Self {
            cmd_tx,
            snap_rx,
            latest_snapshot: initial_snapshot,
            worker_join: join,
        }
    }

    /// Enqueue a command without blocking the UI thread.
    pub fn send(&self, cmd: ServiceCommand) -> bool {
        self.cmd_tx.try_send(cmd).is_ok()
    }

    /// Drain any newly published snapshots from the worker thread and return the newest one.
    pub fn poll_latest(&mut self) -> Option<&SystemSnapshot> {
        let mut updated = false;
        while let Ok(snap) = self.snap_rx.try_recv() {
            self.latest_snapshot = snap;
            updated = true;
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
        let _ = self.cmd_tx.try_send(ServiceCommand::Shutdown);
        if let Some(handle) = self.worker_join.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn startup_and_passive_refresh_perform_zero_hardware_writes() {
        let mut orch =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");
        let snap1 = orch.query_snapshot();
        assert_eq!(snap1.write_count(), 0);
        assert_eq!(
            snap1.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Balanced)
        );

        let snap2 = orch.handle_command(ServiceCommand::Refresh);
        assert_eq!(snap2.write_count(), 0);
    }

    #[test]
    fn gmode_toggle_engages_and_restores_saved_prior_profile() {
        let mut orch =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");
        let snap = orch.set_thermal_mode(ThermalMode::Performance);
        assert_eq!(
            snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Performance)
        );

        let gmode_snap = orch.toggle_gmode();
        assert!(gmode_snap.game_shift_enabled);
        assert_eq!(
            gmode_snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::GMode)
        );
        assert_eq!(
            gmode_snap.pre_gmode_restore_target,
            Some(ThermalMode::Performance)
        );

        let restored_snap = orch.toggle_gmode();
        assert!(!restored_snap.game_shift_enabled);
        assert_eq!(
            restored_snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Performance)
        );
    }

    #[test]
    fn gmode_step2_failure_rolls_back_gameshift_latch_deterministically() {
        let mut orch =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");
        orch.set_sim_scenario(SimScenario::AcpiSetFailureRollback);

        let snap = orch.toggle_gmode();
        assert!(!snap.game_shift_enabled);
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
    fn manual_boost_switches_to_manual_mode_and_writes_both_fans() {
        let mut orch =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");
        let snap = orch.apply_manual_boost(75, 65);
        assert_eq!(
            snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Manual)
        );
        assert_eq!(snap.telemetry.cpu_boost_pct.formatted_value(), "75%");
        assert_eq!(snap.telemetry.gpu_boost_pct.formatted_value(), "65%");
        assert_eq!(snap.write_count(), 3);
    }

    #[test]
    fn tcc_offset_and_custom_fan_curve_hysteresis_and_restoration_work() {
        let mut orch =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");

        // Set TCC offset to 8°C
        let snap_tcc = orch.set_tcc_offset(8);
        assert_eq!(snap_tcc.tcc_offset_c.formatted_value(), "8°C");

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
        let snap_step = orch.step_fan_curve();
        assert_eq!(snap_step.write_count(), writes_after_enable);

        // Disabling curve restores Balanced mode
        cfg.enabled = false;
        let snap_stop = orch.set_fan_curve(cfg);
        assert_eq!(
            snap_stop.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Balanced)
        );
    }
}
