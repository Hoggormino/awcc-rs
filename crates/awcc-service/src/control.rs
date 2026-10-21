//! Daemon-owned thermal control: one long-lived worker that owns the thermal orchestrator, the
//! custom fan-curve control loop and the persisted settings.
//!
//! # Ownership and scheduling
//!
//! * Exactly one [`ControlCore`] owns every thermal hardware write. Complete logical operations
//!   (read, transition, write, confirm, roll back) run to completion on the control thread, so two
//!   clients can never interleave the steps of an operation.
//! * Curve evaluation is driven by a **monotonic daemon schedule** (`tick_interval`, default 2 s),
//!   independent of any client, GUI timer or telemetry consumer. At most one evaluation runs at a
//!   time; when a hardware call overruns the interval the next evaluation is scheduled from *now*
//!   (no catch-up burst of obsolete ticks).
//! * Requests arrive on a bounded queue. A full queue is reported to the client as overload; the
//!   control thread never blocks on a client: replies use a one-slot channel and the latest state
//!   is published in a shared cell that readers copy (obsolete views are simply replaced).
//! * Persistence runs on a separate thread (latest-wins), so filesystem delays cannot stall
//!   tracking. Saving never implies the device applied anything.
//!
//! # Interaction policy between curve, profiles, G-Mode and manual boost
//!
//! A custom curve is superseded by any *accepted* thermal-mode change, G-Mode toggle or manual
//! boost: the curve is dropped without restoring the old profile (the new command defines the
//! mode), the saved *intent* becomes "not running", the saved curve parameters are kept, and later
//! ticks never undo the user's command. A *failed* superseding command leaves the curve running.
//!
//! # Failure policy
//!
//! If the CPU temperature is unavailable or stale for [`SENSOR_LOSS_DEADLINE`] while the curve is
//! active, the daemon restores the verified firmware profile through the normal stop path and
//! enters `Fallback`. The curve resumes automatically once a fresh CPU temperature is observed
//! again and the intent is still "run". If the restore itself fails, the failure is exposed and
//! retried every evaluation; the last manual fan setting is never presented as a safe fallback.
//! The GPU temperature falls back to the CPU temperature (the pre-existing curve behaviour).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use awcc_core::{FanCurveConfig, OperationLifecycle, SensorReading, ThermalMode, ThermalModeState};

use crate::daemon::{DaemonRpcRequest, try_direct_rpc_kernel_io};
use crate::settings::{
    SETTINGS_SCHEMA_VERSION, ServiceSettings, SettingsLoad, SettingsStore, validate_curve,
};
use crate::wire::{
    ControlState, ControlStatus, MAX_VIEW_AUDIT_ENTRIES, ThermalView, WireObservation, WireSensor,
    WireTelemetry,
};
use crate::{ServiceCommand, SystemSnapshot, ThermalOrchestrator};

/// Default evaluation cadence of the custom curve.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(2);
/// A missing/stale CPU temperature this long triggers the verified fallback.
pub const SENSOR_LOSS_DEADLINE: Duration = Duration::from_secs(10);
/// Bound of the request queue between client handlers and the control thread.
pub const REQUEST_QUEUE_CAPACITY: usize = 32;
/// Wall-clock jump (beyond monotonic elapsed time) treated as a suspend/resume.
pub const RESUME_GAP: Duration = Duration::from_secs(5);
/// How long after the last client interest an idle daemon keeps refreshing its view.
pub const CLIENT_INTEREST_WINDOW: Duration = Duration::from_secs(15);
/// Largest number of operation results remembered for duplicate/reconciliation handling.
pub const OPERATION_MEMORY: usize = 64;

/// A thermal operation that a client may request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOp {
    SetThermalMode(ThermalMode),
    ToggleGMode,
    ApplyManualBoost {
        cpu_boost_pct: u8,
        gpu_boost_pct: u8,
    },
    SetCpuTurbo(bool),
    SetTccOffset(u8),
    /// Start (or replace) the custom curve with these parameters.
    ConfigureCurve(FanCurveConfig),
    StopCurve,
    SetRestoreAtStartup(bool),
}

impl ControlOp {
    /// `true` when this operation, once accepted, ends custom-curve control.
    #[must_use]
    pub const fn supersedes_curve(&self) -> bool {
        matches!(
            self,
            Self::SetThermalMode(_) | Self::ToggleGMode | Self::ApplyManualBoost { .. }
        )
    }
}

/// Why an operation was rejected before any hardware work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRejection {
    pub code: &'static str,
    pub message: String,
}

/// Reply to one control request.
#[derive(Debug, Clone)]
pub struct ControlReply {
    pub view: ThermalView,
    /// Raw kernel output of a low-level compatibility request (`RawAcpi` only).
    pub raw_output: Option<String>,
    /// `Err` only when the request was rejected without touching hardware.
    pub result: Result<OperationLifecycle, ControlRejection>,
}

/// Work item for the control thread.
pub enum ControlRequest {
    /// Re-read hardware state and return a fresh view.
    Refresh,
    /// A state-changing operation. `id` is the client's request id: a repeated id never executes
    /// twice, it returns the recorded outcome.
    Op { id: String, op: ControlOp },
    /// Low-level ACPI compatibility request, executed serialized on the control thread.
    RawAcpi {
        id: String,
        request: DaemonRpcRequest,
    },
    /// Graceful stop (restores the firmware profile when a curve is active).
    Shutdown,
}

struct Envelope {
    request: ControlRequest,
    reply: SyncSender<ControlReply>,
}

// ---------------------------------------------------------------------------------------------
// Settings persistence (separate thread, latest-wins)
// ---------------------------------------------------------------------------------------------

/// Where accepted settings go. Implementations must never block the control path for long.
pub trait SettingsSink: Send + Sync {
    fn submit(&self, settings: ServiceSettings);
    /// `(pending, last_error)`.
    fn status(&self) -> (bool, Option<String>);
    /// Wait until nothing is pending (or the deadline passes). Returns whether it is durable.
    fn flush(&self, deadline: Duration) -> bool;
}

/// Discards settings (simulation and tests that do not exercise persistence).
#[derive(Debug, Default)]
pub struct NullSettingsSink;

impl SettingsSink for NullSettingsSink {
    fn submit(&self, _settings: ServiceSettings) {}
    fn status(&self) -> (bool, Option<String>) {
        (false, None)
    }
    fn flush(&self, _deadline: Duration) -> bool {
        true
    }
}

#[derive(Default)]
struct PersistState {
    pending: Option<ServiceSettings>,
    in_flight: bool,
    last_error: Option<String>,
    closing: bool,
}

/// Background writer: coalesces to the newest settings and writes them atomically.
pub struct AsyncSettingsSink {
    shared: Arc<(Mutex<PersistState>, Condvar)>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl AsyncSettingsSink {
    /// Start the writer thread for `store`.
    ///
    /// # Errors
    /// Returns the I/O error if the thread cannot be spawned.
    pub fn spawn(store: SettingsStore, block_overwrite: Option<String>) -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(PersistState::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("awcc-settings".to_string())
            .spawn(move || {
                let (lock, cv) = &*worker;
                loop {
                    let next = {
                        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        loop {
                            if let Some(s) = state.pending.take() {
                                state.in_flight = true;
                                break Some(s);
                            }
                            if state.closing {
                                break None;
                            }
                            state = cv.wait(state).unwrap_or_else(PoisonError::into_inner);
                        }
                    };
                    let Some(settings) = next else { return };
                    let result = match &block_overwrite {
                        Some(reason) => Err(std::io::Error::other(reason.clone())),
                        None => store.save(&settings),
                    };
                    let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    state.in_flight = false;
                    state.last_error = result.err().map(|e| {
                        tracing::error!(
                            target: "awcc_service::control",
                            category = "app",
                            action = "persist_settings",
                            outcome = "failed",
                            code = "settings_write_failed",
                            "saving service settings failed: {e}"
                        );
                        e.to_string()
                    });
                    cv.notify_all();
                }
            })?;
        Ok(Self {
            shared,
            thread: Mutex::new(Some(thread)),
        })
    }
}

impl SettingsSink for AsyncSettingsSink {
    fn submit(&self, settings: ServiceSettings) {
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        state.pending = Some(settings);
        cv.notify_all();
    }

    fn status(&self) -> (bool, Option<String>) {
        let state = self.shared.0.lock().unwrap_or_else(PoisonError::into_inner);
        (
            state.pending.is_some() || state.in_flight,
            state.last_error.clone(),
        )
    }

    fn flush(&self, deadline: Duration) -> bool {
        let (lock, cv) = &*self.shared;
        let until = Instant::now() + deadline;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while state.pending.is_some() || state.in_flight {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = cv
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.last_error.is_none()
    }
}

impl Drop for AsyncSettingsSink {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            state.closing = true;
            cv.notify_all();
        }
        if let Some(handle) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Control core (synchronous, deterministic, fully testable)
// ---------------------------------------------------------------------------------------------

/// The synchronous control state machine. [`ControlHandle`] runs it on its own thread.
pub struct ControlCore {
    orch: ThermalOrchestrator,
    settings: ServiceSettings,
    sink: Arc<dyn SettingsSink>,
    settings_problem: Option<String>,
    state: ControlState,
    clock: Box<dyn Fn() -> Instant + Send>,
    wall_clock: Box<dyn Fn() -> SystemTime + Send>,
    started: Instant,
    tick_interval: Duration,
    last_tick_clock: Option<(Instant, SystemTime)>,
    last_eval: Option<Instant>,
    last_outcome: Option<String>,
    failure_reason: Option<String>,
    sensor_missing_since: Option<Instant>,
    cpu_sensor_fresh: bool,
    pending_fallback_restore: Option<ThermalMode>,
    needs_revalidation: bool,
    last_view: Option<ThermalView>,
}

impl ControlCore {
    /// Build the core and perform the startup restore decision.
    ///
    /// `load` is what the persisted settings looked like; the device identity is revalidated by the
    /// caller when loading. A curve is restored only when the saved preference asks for it **and**
    /// the device currently reports a fresh CPU temperature and a known thermal mode.
    #[must_use]
    pub fn new(
        orch: ThermalOrchestrator,
        load: SettingsLoad,
        sink: Arc<dyn SettingsSink>,
        tick_interval: Duration,
    ) -> Self {
        Self::with_clocks(
            orch,
            load,
            sink,
            tick_interval,
            Box::new(Instant::now),
            Box::new(SystemTime::now),
        )
    }

    /// Like [`Self::new`] with injectable clocks (deterministic tests).
    #[must_use]
    pub fn with_clocks(
        orch: ThermalOrchestrator,
        load: SettingsLoad,
        sink: Arc<dyn SettingsSink>,
        tick_interval: Duration,
        clock: Box<dyn Fn() -> Instant + Send>,
        wall_clock: Box<dyn Fn() -> SystemTime + Send>,
    ) -> Self {
        let device = orch.device_identity();
        let (settings, settings_problem) = match load {
            SettingsLoad::Loaded(s) => (s, None),
            SettingsLoad::Missing => (ServiceSettings::fresh(&device.0, &device.1), None),
            SettingsLoad::Invalid { reason } => (
                ServiceSettings::fresh(&device.0, &device.1),
                Some(format!("saved settings ignored: {reason}")),
            ),
            SettingsLoad::WrongDevice { reason } => (
                ServiceSettings::fresh(&device.0, &device.1),
                Some(format!("saved settings ignored: {reason}")),
            ),
            SettingsLoad::Incompatible { found_version } => (
                ServiceSettings::fresh(&device.0, &device.1),
                Some(format!(
                    "saved settings use schema {found_version} (this daemon supports {SETTINGS_SCHEMA_VERSION}); they are left untouched"
                )),
            ),
        };
        let started = clock();
        let mut core = Self {
            orch,
            settings,
            sink,
            settings_problem,
            state: ControlState::Idle,
            clock,
            wall_clock,
            started,
            tick_interval,
            last_tick_clock: None,
            last_eval: None,
            last_outcome: None,
            failure_reason: None,
            sensor_missing_since: None,
            cpu_sensor_fresh: false,
            pending_fallback_restore: None,
            needs_revalidation: false,
            last_view: None,
        };
        core.startup_restore();
        core
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn startup_restore(&mut self) {
        if self.settings.restore_at_startup && self.settings.intent_active {
            self.try_activate_curve("startup restore");
        }
        tracing::info!(
            target: "awcc_service::control",
            category = "app",
            action = "startup",
            outcome = ?self.state,
            code = "control_started",
            restore_at_startup = self.settings.restore_at_startup,
            intent_active = self.settings.intent_active,
            "daemon control started"
        );
    }

    /// Eligibility checks, then activate the saved curve. Returns whether control is now active.
    fn try_activate_curve(&mut self, why: &str) -> bool {
        let snap = self.orch.query_snapshot();
        let cpu_fresh = matches!(snap.telemetry.cpu_temp_c, SensorReading::Valid { .. });
        if !cpu_fresh {
            self.enter_restore_failed(format!(
                "{why}: no fresh CPU temperature is available to evaluate the curve"
            ));
            return false;
        }
        if matches!(snap.thermal_mode_state, ThermalModeState::Unknown { .. }) {
            self.enter_restore_failed(format!(
                "{why}: the current thermal mode is unknown, refusing to take control"
            ));
            return false;
        }
        let mut cfg = self.settings.curve.clone();
        cfg.enabled = true;
        let outcome = self.orch.execute(ServiceCommand::SetFanCurve(cfg));
        let lifecycle = outcome.snapshot.lifecycle.clone();
        if matches!(lifecycle, OperationLifecycle::Failed { .. }) {
            self.orch.cancel_curve_without_restore();
            self.enter_restore_failed(format!("{why}: {}", lifecycle.detail_message()));
            return false;
        }
        self.state = ControlState::Active;
        self.failure_reason = None;
        self.sensor_missing_since = None;
        self.last_eval = Some(self.now());
        self.last_outcome = Some(lifecycle.detail_message());
        true
    }

    fn enter_restore_failed(&mut self, reason: String) {
        tracing::warn!(
            target: "awcc_service::control",
            category = "app",
            action = "activate_curve",
            outcome = "failed",
            code = "curve_activation_failed",
            "{reason}"
        );
        self.state = ControlState::RestoreFailed;
        self.failure_reason = Some(reason);
    }

    fn persist(&mut self) {
        self.settings.schema_version = SETTINGS_SCHEMA_VERSION;
        self.sink.submit(self.settings.clone());
    }

    fn build_status(&self) -> ControlStatus {
        let (pending, error) = self.sink.status();
        let now = self.now();
        ControlStatus {
            state: self.state,
            saved_curve: {
                let mut c = self.settings.curve.clone();
                c.enabled = false;
                c
            },
            intent_active: self.settings.intent_active,
            restore_at_startup: self.settings.restore_at_startup,
            last_evaluation_age_secs: self
                .last_eval
                .map(|t| now.saturating_duration_since(t).as_secs()),
            last_outcome: self.last_outcome.clone(),
            failure_reason: self.failure_reason.clone(),
            tick_interval_ms: u64::try_from(self.tick_interval.as_millis()).unwrap_or(u64::MAX),
            uptime_secs: now.saturating_duration_since(self.started).as_secs(),
            cpu_sensor_fresh: self.cpu_sensor_fresh,
            settings_problem: self.settings_problem.clone(),
            settings_save_pending: pending,
            settings_save_error: error,
        }
    }

    fn view_from(&mut self, snap: &SystemSnapshot) -> ThermalView {
        self.cpu_sensor_fresh = matches!(snap.telemetry.cpu_temp_c, SensorReading::Valid { .. });
        let audit_start = snap
            .write_audit_log
            .len()
            .saturating_sub(MAX_VIEW_AUDIT_ENTRIES);
        let view = ThermalView {
            device_name: snap.profile.device_name.clone(),
            acpi_prefix: snap.profile.acpi_prefix.to_string(),
            thermal_mode_state: snap.thermal_mode_state.clone(),
            game_shift: WireObservation::from_core(&snap.game_shift),
            pre_gmode_restore_target: snap.pre_gmode_restore_target,
            telemetry: WireTelemetry::from_core(&snap.telemetry),
            cpu_turbo: WireObservation::from_core(&snap.cpu_turbo),
            tcc_offset_c: WireSensor::from_core(&snap.tcc_offset_c),
            fan_curve: snap.fan_curve.clone(),
            lifecycle: snap.lifecycle.clone(),
            operation_id: snap.operation_id,
            audit: snap.write_audit_log[audit_start..].to_vec(),
            control: self.build_status(),
        };
        self.last_view = Some(view.clone());
        view
    }

    /// Passive refresh: query hardware without any write.
    pub fn refresh(&mut self) -> ThermalView {
        let snap = self.orch.query_snapshot();
        self.view_from(&snap)
    }

    /// The most recent view without touching hardware.
    #[must_use]
    pub fn cached_view(&self) -> Option<&ThermalView> {
        self.last_view.as_ref()
    }

    /// Execute one client operation to completion.
    pub fn handle(&mut self, op: ControlOp) -> ControlReply {
        match op {
            ControlOp::SetRestoreAtStartup(value) => {
                self.settings.restore_at_startup = value;
                self.persist();
                let snap = self.orch.query_snapshot();
                let view = self.view_from(&snap);
                ControlReply {
                    raw_output: None,
                    result: Ok(OperationLifecycle::Accepted {
                        summary: format!(
                            "Restore fan curve at startup: {}",
                            if value { "on" } else { "off" }
                        ),
                        caveat: "saved for the next daemon start; no hardware write was performed"
                            .to_string(),
                    }),
                    view,
                }
            }
            ControlOp::ConfigureCurve(cfg) => self.configure_curve(cfg),
            ControlOp::StopCurve => self.stop_curve(),
            other => self.run_thermal_op(other),
        }
    }

    fn reject(&mut self, code: &'static str, message: String) -> ControlReply {
        let snap = self.orch.query_snapshot();
        ControlReply {
            raw_output: None,
            view: self.view_from(&snap),
            result: Err(ControlRejection { code, message }),
        }
    }

    fn configure_curve(&mut self, mut cfg: FanCurveConfig) -> ControlReply {
        cfg.enabled = true;
        if let Err(message) = validate_curve(&cfg) {
            return self.reject("invalid_curve", message);
        }
        let outcome = self.orch.execute(ServiceCommand::SetFanCurve(cfg.clone()));
        let lifecycle = outcome.snapshot.lifecycle.clone();
        let mut snap = outcome.snapshot;
        if matches!(lifecycle, OperationLifecycle::Failed { .. }) {
            self.orch.cancel_curve_without_restore();
            self.state = ControlState::Idle;
            self.failure_reason = Some(lifecycle.detail_message());
            self.last_outcome = Some(lifecycle.detail_message());
            snap.fan_curve.enabled = false;
        } else {
            cfg.enabled = false;
            self.settings.curve = cfg;
            self.settings.intent_active = true;
            self.persist();
            self.state = ControlState::Active;
            self.failure_reason = None;
            self.sensor_missing_since = None;
            self.pending_fallback_restore = None;
            self.last_eval = Some(self.now());
            self.last_outcome = Some(lifecycle.detail_message());
        }
        ControlReply {
            raw_output: None,
            view: self.view_from(&snap),
            result: Ok(lifecycle),
        }
    }

    fn stop_curve(&mut self) -> ControlReply {
        let was_applying = self.orch.curve_enabled();
        let mut cfg = self.settings.curve.clone();
        cfg.enabled = false;
        let outcome = self.orch.execute(ServiceCommand::SetFanCurve(cfg));
        let lifecycle = outcome.snapshot.lifecycle.clone();
        let snap = outcome.snapshot;
        let restore_failed = matches!(lifecycle, OperationLifecycle::Failed { .. });
        // The user's intent is "stop" regardless; a failed restore is reported, not hidden.
        self.settings.intent_active = false;
        self.persist();
        self.state = ControlState::Idle;
        self.sensor_missing_since = None;
        self.pending_fallback_restore = None;
        self.failure_reason = restore_failed.then(|| lifecycle.detail_message());
        self.last_outcome = Some(if was_applying {
            lifecycle.detail_message()
        } else {
            "No custom fan curve was being applied".to_string()
        });
        ControlReply {
            raw_output: None,
            view: self.view_from(&snap),
            result: Ok(lifecycle),
        }
    }

    fn run_thermal_op(&mut self, op: ControlOp) -> ControlReply {
        let supersedes = op.supersedes_curve();
        let command = match op {
            ControlOp::SetThermalMode(m) => ServiceCommand::SetThermalMode(m),
            ControlOp::ToggleGMode => ServiceCommand::ToggleGMode,
            ControlOp::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            } => ServiceCommand::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            },
            ControlOp::SetCpuTurbo(e) => ServiceCommand::SetCpuTurbo(e),
            ControlOp::SetTccOffset(o) => ServiceCommand::SetTccOffset(o),
            ControlOp::ConfigureCurve(_)
            | ControlOp::StopCurve
            | ControlOp::SetRestoreAtStartup(_) => {
                return self.reject("internal", "operation routed incorrectly".to_string());
            }
        };
        let outcome = self.orch.execute(command);
        let lifecycle = outcome.snapshot.lifecycle.clone();
        let mut snap = outcome.snapshot;
        let accepted = !matches!(lifecycle, OperationLifecycle::Failed { .. });
        if supersedes {
            if accepted && self.state != ControlState::Idle {
                self.orch.cancel_curve_without_restore();
                self.state = ControlState::Idle;
                self.settings.intent_active = false;
                self.persist();
                self.sensor_missing_since = None;
                self.pending_fallback_restore = None;
                self.failure_reason = None;
                self.last_outcome = Some(format!(
                    "custom fan curve ended: superseded by {}",
                    lifecycle.detail_message()
                ));
                snap.fan_curve.enabled = false;
            } else if accepted {
                self.settings.intent_active = false;
                self.persist();
            } else {
                // The command failed: the curve keeps running, but its remembered boost can no
                // longer be assumed to match the hardware.
                self.orch.reset_curve_memory();
            }
        }
        ControlReply {
            raw_output: None,
            view: self.view_from(&snap),
            result: Ok(lifecycle),
        }
    }

    /// Low-level ACPI compatibility request. It obeys the same ownership rules as everything else:
    /// the prefix must match the detected device, and state-changing calls are refused while a
    /// custom curve owns the fans.
    pub fn raw_acpi(&mut self, request: &DaemonRpcRequest) -> ControlReply {
        let device_prefix = self.orch.device_identity().1;
        let DaemonRpcRequest::AcpiWmax { prefix, subcmd, .. } = request else {
            return self.reject("unsupported", "not a raw ACPI request".to_string());
        };
        if prefix.to_string() != device_prefix {
            return self.reject(
                "wrong_device",
                format!("ACPI prefix {prefix} does not match this device ({device_prefix})"),
            );
        }
        if *subcmd != 0x14 && (self.state != ControlState::Idle || self.orch.curve_enabled()) {
            return self.reject(
                "curve_active",
                "a custom fan curve owns thermal control; stop it before issuing raw writes"
                    .to_string(),
            );
        }
        let Some(output) = try_direct_rpc_kernel_io(request) else {
            return self.reject(
                "hardware_unavailable",
                "the kernel ACPI interface is not available to the daemon".to_string(),
            );
        };
        let snap = self.orch.query_snapshot();
        let mut reply = ControlReply {
            raw_output: Some(output),
            view: self.view_from(&snap),
            result: Ok(OperationLifecycle::Idle),
        };
        reply.raw_output = reply.raw_output.take();
        reply
    }

    /// Tell the core that the machine resumed from suspend: discard remembered hardware state and
    /// require fresh sensors and mode before any further curve write.
    pub fn note_resume(&mut self) {
        tracing::info!(
            target: "awcc_service::control",
            category = "app",
            action = "resume",
            outcome = "revalidating",
            code = "resume_detected",
            "resume detected: discarding stale samples and revalidating control eligibility"
        );
        self.orch.reset_curve_memory();
        self.sensor_missing_since = None;
        self.needs_revalidation = true;
    }

    /// One scheduled evaluation. `interest` says whether a client recently looked at the state.
    pub fn tick(&mut self, interest: bool) {
        let now = self.now();
        let wall = (self.wall_clock)();
        if let Some((prev_now, prev_wall)) = self.last_tick_clock {
            let mono = now.saturating_duration_since(prev_now);
            let walled = wall.duration_since(prev_wall).unwrap_or(Duration::ZERO);
            if walled > mono + RESUME_GAP {
                self.note_resume();
            }
        }
        self.last_tick_clock = Some((now, wall));

        match self.state {
            ControlState::Active => self.tick_active(now),
            ControlState::Fallback => self.tick_fallback(now),
            ControlState::RestoreFailed => {
                if self.settings.intent_active {
                    self.try_activate_curve("retry");
                } else {
                    self.state = ControlState::Idle;
                }
            }
            ControlState::Idle => {
                if interest {
                    self.refresh();
                }
            }
        }
    }

    fn tick_active(&mut self, now: Instant) {
        if self.needs_revalidation {
            let snap = self.orch.query_snapshot();
            let eligible = matches!(snap.telemetry.cpu_temp_c, SensorReading::Valid { .. })
                && !matches!(snap.thermal_mode_state, ThermalModeState::Unknown { .. });
            if !eligible {
                self.view_from(&snap);
                self.sensor_missing_since.get_or_insert(now);
                self.maybe_fallback(now, "no fresh sensors or known mode after resume");
                return;
            }
            self.needs_revalidation = false;
        }
        let snap = self.orch.step_fan_curve();
        let fresh = matches!(snap.telemetry.cpu_temp_c, SensorReading::Valid { .. });
        if fresh {
            self.sensor_missing_since = None;
            self.last_eval = Some(now);
            if let OperationLifecycle::Confirmed { summary }
            | OperationLifecycle::Accepted { summary, .. } = &snap.lifecycle
                && summary.starts_with("Custom fan curve active")
            {
                self.last_outcome = Some(summary.clone());
            }
            if let OperationLifecycle::Failed { summary, error } = &snap.lifecycle {
                self.last_outcome = Some(format!("{summary}: {error}"));
            }
            self.view_from(&snap);
        } else {
            self.view_from(&snap);
            self.sensor_missing_since.get_or_insert(now);
            self.maybe_fallback(now, "the CPU temperature is unavailable or stale");
        }
    }

    fn maybe_fallback(&mut self, now: Instant, why: &str) {
        let Some(since) = self.sensor_missing_since else {
            return;
        };
        if now.saturating_duration_since(since) < SENSOR_LOSS_DEADLINE {
            return;
        }
        let target = self.orch.curve_restore_mode();
        let mut cfg = self.settings.curve.clone();
        cfg.enabled = false;
        let outcome = self.orch.execute(ServiceCommand::SetFanCurve(cfg));
        let lifecycle = outcome.snapshot.lifecycle.clone();
        self.state = ControlState::Fallback;
        if matches!(lifecycle, OperationLifecycle::Failed { .. }) {
            self.pending_fallback_restore = Some(target);
            self.failure_reason = Some(format!(
                "{why} for {}s; restoring the `{target}` firmware profile FAILED: {} (retrying every evaluation)",
                SENSOR_LOSS_DEADLINE.as_secs(),
                lifecycle.detail_message()
            ));
        } else {
            self.pending_fallback_restore = None;
            self.failure_reason = Some(format!(
                "{why} for {}s; control suspended and the `{target}` firmware profile restored",
                SENSOR_LOSS_DEADLINE.as_secs()
            ));
        }
        self.last_outcome = self.failure_reason.clone();
        tracing::error!(
            target: "awcc_service::control",
            category = "app",
            action = "fallback",
            outcome = "entered",
            code = "curve_fallback",
            "{}",
            self.failure_reason.as_deref().unwrap_or("")
        );
        self.view_from(&outcome.snapshot);
    }

    fn tick_fallback(&mut self, now: Instant) {
        if let Some(target) = self.pending_fallback_restore {
            let outcome = self.orch.execute(ServiceCommand::SetThermalMode(target));
            if !matches!(
                outcome.snapshot.lifecycle,
                OperationLifecycle::Failed { .. }
            ) {
                self.pending_fallback_restore = None;
                self.failure_reason = Some(format!(
                    "control suspended; the `{target}` firmware profile was restored on retry"
                ));
            }
            self.view_from(&outcome.snapshot);
            return;
        }
        if !self.settings.intent_active {
            self.state = ControlState::Idle;
            return;
        }
        let snap = self.orch.query_snapshot();
        let eligible = matches!(snap.telemetry.cpu_temp_c, SensorReading::Valid { .. })
            && !matches!(snap.thermal_mode_state, ThermalModeState::Unknown { .. });
        self.view_from(&snap);
        if eligible && self.try_activate_curve("resume after sensor recovery") {
            self.needs_revalidation = false;
            self.last_eval = Some(now);
        }
    }

    /// Graceful stop: leave custom control through the verified path and report what happened.
    /// The saved intent and startup preference are preserved.
    pub fn shutdown(&mut self) -> String {
        let report = if self.orch.curve_enabled() {
            let mut cfg = self.settings.curve.clone();
            cfg.enabled = false;
            let outcome = self.orch.execute(ServiceCommand::SetFanCurve(cfg));
            let lifecycle = outcome.snapshot.lifecycle;
            match lifecycle {
                OperationLifecycle::Failed { .. } => format!(
                    "shutdown: leaving custom curve control FAILED: {}",
                    lifecycle.detail_message()
                ),
                other => format!(
                    "shutdown: left custom curve control: {}",
                    other.detail_message()
                ),
            }
        } else {
            "shutdown: no custom curve was being applied".to_string()
        };
        self.state = ControlState::Idle;
        tracing::info!(
            target: "awcc_service::control",
            category = "app",
            action = "shutdown",
            outcome = "done",
            code = "control_shutdown",
            "{report}"
        );
        self.last_outcome = Some(report.clone());
        report
    }

    #[must_use]
    pub const fn state(&self) -> ControlState {
        self.state
    }

    #[must_use]
    pub const fn settings(&self) -> &ServiceSettings {
        &self.settings
    }

    #[must_use]
    pub const fn tick_interval(&self) -> Duration {
        self.tick_interval
    }

    /// Mutable access for tests that need to adjust the orchestrator.
    pub fn orchestrator_mut(&mut self) -> &mut ThermalOrchestrator {
        &mut self.orch
    }
}

// ---------------------------------------------------------------------------------------------
// Control thread
// ---------------------------------------------------------------------------------------------

/// Failure to submit a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// The bounded queue is full.
    Overloaded,
    /// The control thread has stopped.
    Stopped,
}

/// Shared handle to the running control thread. Cloning is cheap.
#[derive(Clone)]
pub struct ControlHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    tx: SyncSender<Envelope>,
    published: Mutex<Option<(Instant, Arc<ThermalView>)>>,
    interest: Mutex<Instant>,
    thread: Mutex<Option<JoinHandle<String>>>,
    stopped: Mutex<Option<String>>,
    memory: Mutex<OperationMemory>,
    pending: Mutex<std::collections::HashSet<String>>,
}

/// What is known about a request id (reconciliation after a lost connection or timeout).
#[derive(Debug, Clone)]
pub enum OperationStatus {
    /// Finished; carries the recorded reply.
    Completed(Box<ControlReply>),
    /// Rejected before any hardware work.
    Rejected(ControlRejection),
    /// Accepted into the queue (or running) but not finished yet.
    Pending,
    /// Never seen, or evicted from the bounded memory / lost by a daemon restart. The caller must
    /// not assume it did or did not execute.
    Unknown,
}

impl ControlHandle {
    /// Spawn the control thread owning `core`.
    ///
    /// # Errors
    /// Returns the I/O error if the thread cannot be spawned.
    pub fn spawn(core: ControlCore) -> std::io::Result<Self> {
        Self::spawn_with_capacity(core, REQUEST_QUEUE_CAPACITY)
    }

    /// Spawn with an explicit queue bound.
    ///
    /// # Errors
    /// Returns the I/O error if the thread cannot be spawned.
    pub fn spawn_with_capacity(mut core: ControlCore, capacity: usize) -> std::io::Result<Self> {
        let (tx, rx): (SyncSender<Envelope>, Receiver<Envelope>) = mpsc::sync_channel(capacity);
        let inner = Arc::new(HandleInner {
            tx,
            published: Mutex::new(None),
            interest: Mutex::new(Instant::now()),
            thread: Mutex::new(None),
            stopped: Mutex::new(None),
            memory: Mutex::new(OperationMemory::default()),
            pending: Mutex::new(std::collections::HashSet::new()),
        });
        let worker_inner = Arc::clone(&inner);
        let interval = core.tick_interval();
        let handle = std::thread::Builder::new()
            .name("awcc-control".to_string())
            .spawn(move || {
                let first = core.refresh();
                publish(&worker_inner, &first);
                let mut next_tick = Instant::now() + interval;
                loop {
                    let wait = next_tick.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(wait) {
                        Ok(env) => {
                            if serve(&mut core, &worker_inner, env) {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    let now = Instant::now();
                    if now >= next_tick {
                        let interested = {
                            let last = worker_inner
                                .interest
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            now.saturating_duration_since(*last) < CLIENT_INTEREST_WINDOW
                        };
                        core.tick(interested);
                        if let Some(view) = core.cached_view().cloned() {
                            publish(&worker_inner, &view);
                        }
                        // No catch-up: schedule from now, never from the missed deadline.
                        next_tick = Instant::now() + interval;
                    }
                }
                let report = core.shutdown();
                if let Some(view) = core.cached_view().cloned() {
                    publish(&worker_inner, &view);
                }
                report
            })?;
        *inner.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        Ok(Self { inner })
    }

    /// Mark that a client recently looked at the state (keeps an idle daemon's view fresh).
    pub fn note_interest(&self) {
        *self
            .inner
            .interest
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// The latest published view and how old it is.
    #[must_use]
    pub fn published(&self) -> Option<(Arc<ThermalView>, Duration)> {
        self.inner
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(at, view)| (Arc::clone(view), at.elapsed()))
    }

    /// Queue a request and wait up to `deadline` for its reply. Never blocks on a full queue.
    ///
    /// # Errors
    /// [`SubmitError::Overloaded`] when the queue is full, [`SubmitError::Stopped`] when the
    /// control thread is gone. A reply that does not arrive in time yields `Ok(None)`: the
    /// operation may still complete, so the caller must reconcile rather than replay.
    pub fn request(
        &self,
        request: ControlRequest,
        deadline: Duration,
    ) -> Result<Option<ControlReply>, SubmitError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let op_id = match &request {
            ControlRequest::Op { id, .. } => Some(id.clone()),
            _ => None,
        };
        if let Some(id) = &op_id {
            self.inner
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id.clone());
        }
        match self.inner.tx.try_send(Envelope {
            request,
            reply: reply_tx,
        }) {
            Ok(()) => {}
            Err(e) => {
                if let Some(id) = &op_id {
                    self.inner
                        .pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(id);
                }
                return Err(match e {
                    TrySendError::Full(_) => SubmitError::Overloaded,
                    TrySendError::Disconnected(_) => SubmitError::Stopped,
                });
            }
        }
        match reply_rx.recv_timeout(deadline) {
            Ok(reply) => Ok(Some(reply)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(SubmitError::Stopped),
        }
    }

    /// Reconcile a request id after a timeout or connection loss. Never replays anything.
    #[must_use]
    pub fn operation_status(&self, id: &str) -> OperationStatus {
        if let Some(done) = self
            .inner
            .memory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
        {
            return match done {
                StoredOutcome::Completed(reply) => OperationStatus::Completed(reply),
                StoredOutcome::Rejected(r) => OperationStatus::Rejected(r),
            };
        }
        if self
            .inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(id)
        {
            return OperationStatus::Pending;
        }
        OperationStatus::Unknown
    }

    /// Stop the control thread (graceful curve exit included) within `deadline`.
    /// Returns the shutdown report, or `None` if the thread did not finish in time.
    #[must_use]
    pub fn stop(&self, deadline: Duration) -> Option<String> {
        let _ = self.request(ControlRequest::Shutdown, Duration::from_millis(1));
        let handle = self
            .inner
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let until = Instant::now() + deadline;
        if let Some(h) = handle {
            while !h.is_finished() {
                if Instant::now() >= until {
                    // Detach: systemd will escalate; do not hang the caller.
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let report = h.join().ok();
            *self
                .inner
                .stopped
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = report.clone();
            return report;
        }
        self.inner
            .stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn publish(inner: &HandleInner, view: &ThermalView) {
    *inner
        .published
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), Arc::new(view.clone())));
}

/// Serve one envelope; returns `true` when the worker should stop.
fn serve(core: &mut ControlCore, inner: &HandleInner, env: Envelope) -> bool {
    let (reply, stop) = match env.request {
        ControlRequest::Refresh => {
            let view = core.refresh();
            (
                Some(ControlReply {
                    raw_output: None,
                    view,
                    result: Ok(OperationLifecycle::Idle),
                }),
                false,
            )
        }
        ControlRequest::RawAcpi { request, .. } => (Some(core.raw_acpi(&request)), false),
        ControlRequest::Op { id, op } => {
            let recorded = inner
                .memory
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&id);
            let reply = match recorded {
                // A repeated id returns the original outcome; the operation never runs twice.
                Some(StoredOutcome::Completed(prev)) => *prev,
                Some(StoredOutcome::Rejected(r)) => ControlReply {
                    raw_output: None,
                    view: core.refresh(),
                    result: Err(r),
                },
                None => {
                    let reply = core.handle(op);
                    let stored = match &reply.result {
                        Ok(_) => StoredOutcome::Completed(Box::new(reply.clone())),
                        Err(r) => StoredOutcome::Rejected(r.clone()),
                    };
                    inner
                        .memory
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .put(id.clone(), stored);
                    reply
                }
            };
            inner
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            (Some(reply), false)
        }
        ControlRequest::Shutdown => (None, true),
    };
    if let Some(reply) = reply {
        publish(inner, &reply.view);
        // One-slot channel: never blocks even if the requester gave up.
        let _ = env.reply.try_send(reply);
    }
    stop
}

/// Remembered completed operations so a retried request id never replays a non-idempotent write.
#[derive(Default)]
pub struct OperationMemory {
    order: VecDeque<String>,
    done: std::collections::HashMap<String, StoredOutcome>,
}

/// What was recorded for a finished request id.
#[derive(Debug, Clone)]
pub enum StoredOutcome {
    Completed(Box<ControlReply>),
    Rejected(ControlRejection),
}

impl OperationMemory {
    #[must_use]
    pub fn get(&self, id: &str) -> Option<StoredOutcome> {
        self.done.get(id).cloned()
    }

    pub fn put(&mut self, id: String, outcome: StoredOutcome) {
        if self.done.insert(id.clone(), outcome).is_none() {
            self.order.push_back(id);
            while self.order.len() > OPERATION_MEMORY {
                if let Some(old) = self.order.pop_front() {
                    self.done.remove(&old);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use awcc_core::{DeviceProfile, FanCurvePoint, SimScenario, TelemetrySnapshot};
    use awcc_platform::{
        MemoryStateStorage, SimBackend, SimFaults, ThermalTransport, TransportError,
        WriteAuditEntry,
    };

    /// Sim backend with test-controlled CPU/GPU temperatures.
    struct TempTransport {
        inner: SimBackend,
        cpu: Arc<Mutex<Option<u16>>>,
        fail_writes: Arc<Mutex<bool>>,
    }

    impl ThermalTransport for TempTransport {
        fn is_simulated(&self) -> bool {
            true
        }
        fn scenario(&self) -> SimScenario {
            self.inner.scenario()
        }
        fn set_scenario(&mut self, s: SimScenario) {
            self.inner.set_scenario(s);
        }
        fn set_sim_faults(&mut self, f: SimFaults) {
            self.inner.set_sim_faults(f);
        }
        fn query_raw_thermal_mode(&mut self) -> Result<u32, TransportError> {
            self.inner.query_raw_thermal_mode()
        }
        fn query_game_shift_status(&mut self) -> Result<bool, TransportError> {
            self.inner.query_game_shift_status()
        }
        fn set_raw_thermal_mode(&mut self, c: u16) -> Result<(), TransportError> {
            self.inner.set_raw_thermal_mode(c)
        }
        fn set_game_shift_status(&mut self, e: bool) -> Result<(), TransportError> {
            self.inner.set_game_shift_status(e)
        }
        fn query_telemetry(&mut self) -> TelemetrySnapshot {
            let mut t = self.inner.query_telemetry();
            let cpu = *self.cpu.lock().unwrap();
            t.cpu_temp_c = cpu.map_or(
                SensorReading::Unavailable {
                    reason: "test: sensor removed".to_string(),
                },
                |value| SensorReading::Valid { value, unit: "°C" },
            );
            t.gpu_temp_c = t.cpu_temp_c.clone();
            t
        }
        fn set_fan_boost(&mut self, c: u8, g: u8) -> Result<(), TransportError> {
            if *self.fail_writes.lock().unwrap() {
                return Err(TransportError::TransportUnavailable(
                    "test write failure".into(),
                ));
            }
            self.inner.set_fan_boost(c, g)
        }
        fn query_cpu_turbo(&mut self) -> Result<bool, TransportError> {
            self.inner.query_cpu_turbo()
        }
        fn set_cpu_turbo(&mut self, e: bool) -> Result<(), TransportError> {
            self.inner.set_cpu_turbo(e)
        }
        fn query_tcc_offset(&mut self) -> SensorReading<u8> {
            self.inner.query_tcc_offset()
        }
        fn set_tcc_offset(&mut self, o: u8) -> Result<(), TransportError> {
            self.inner.set_tcc_offset(o)
        }
        fn write_audit_log(&self) -> &[WriteAuditEntry] {
            self.inner.write_audit_log()
        }
    }

    struct Rig {
        cpu: Arc<Mutex<Option<u16>>>,
        fail_writes: Arc<Mutex<bool>>,
        now: Arc<Mutex<Instant>>,
        wall: Arc<Mutex<SystemTime>>,
        core: ControlCore,
    }

    #[derive(Default)]
    struct RecordingSink {
        saved: Mutex<Vec<ServiceSettings>>,
    }
    impl SettingsSink for RecordingSink {
        fn submit(&self, s: ServiceSettings) {
            self.saved.lock().unwrap().push(s);
        }
        fn status(&self) -> (bool, Option<String>) {
            (false, None)
        }
        fn flush(&self, _d: Duration) -> bool {
            true
        }
    }

    fn rig_with(load: impl FnOnce(&str, &str) -> SettingsLoad, sink: Arc<dyn SettingsSink>) -> Rig {
        let cpu = Arc::new(Mutex::new(Some(40_u16)));
        let fail_writes = Arc::new(Mutex::new(false));
        let profile = DeviceProfile::alienware_m15_r6().unwrap();
        let (name, prefix) = (profile.device_name.clone(), profile.acpi_prefix.to_string());
        let transport = TempTransport {
            inner: SimBackend::new_alienware_m15_r6(),
            cpu: Arc::clone(&cpu),
            fail_writes: Arc::clone(&fail_writes),
        };
        let orch = ThermalOrchestrator::new(
            profile,
            Box::new(transport),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
        );
        let now = Arc::new(Mutex::new(Instant::now()));
        let wall = Arc::new(Mutex::new(SystemTime::now()));
        let (n2, w2) = (Arc::clone(&now), Arc::clone(&wall));
        let core = ControlCore::with_clocks(
            orch,
            load(&name, &prefix),
            sink,
            Duration::from_secs(2),
            Box::new(move || *n2.lock().unwrap()),
            Box::new(move || *w2.lock().unwrap()),
        );
        Rig {
            cpu,
            fail_writes,
            now,
            wall,
            core,
        }
    }

    fn rig() -> Rig {
        rig_with(
            |n, p| SettingsLoad::Loaded(ServiceSettings::fresh(n, p)),
            Arc::new(NullSettingsSink),
        )
    }

    impl Rig {
        fn advance(&mut self, secs: u64) {
            *self.now.lock().unwrap() += Duration::from_secs(secs);
            *self.wall.lock().unwrap() += Duration::from_secs(secs);
        }
        fn boost(&mut self) -> Option<u8> {
            match self.core.refresh().telemetry.cpu_boost_pct {
                WireSensor::Valid { value, .. } => Some(value),
                _ => None,
            }
        }
    }

    fn curve() -> FanCurveConfig {
        let pts = |v: [(u16, u8); 4]| {
            v.iter()
                .map(|&(temp_c, boost_pct)| FanCurvePoint { temp_c, boost_pct })
                .collect::<Vec<_>>()
        };
        FanCurveConfig {
            enabled: true,
            hysteresis_c: 3,
            cpu_points: pts([(45, 0), (60, 35), (75, 70), (85, 100)]),
            gpu_points: pts([(45, 0), (60, 35), (75, 70), (85, 100)]),
        }
    }

    #[test]
    fn curve_follows_temperature_on_ticks_without_any_client() {
        let mut r = rig();
        *r.cpu.lock().unwrap() = Some(60);
        let reply = r.core.handle(ControlOp::ConfigureCurve(curve()));
        assert!(reply.result.is_ok());
        assert_eq!(r.core.state(), ControlState::Active);
        assert_eq!(r.boost(), Some(35));
        // No client calls from here on: only scheduled ticks.
        *r.cpu.lock().unwrap() = Some(80);
        r.advance(2);
        r.core.tick(false);
        assert_eq!(r.boost(), Some(85));
        *r.cpu.lock().unwrap() = Some(46);
        r.advance(2);
        r.core.tick(false);
        assert_eq!(r.boost(), Some(2));
    }

    #[test]
    fn fresh_settings_do_not_activate_a_curve() {
        let mut r = rig();
        r.core.tick(false);
        assert_eq!(r.core.state(), ControlState::Idle);
        assert!(!r.core.settings().intent_active);
        assert!(!r.core.settings().restore_at_startup);
    }

    #[test]
    fn startup_restore_requires_preference_intent_and_fresh_sensors() {
        let saved = |intent, restore| {
            move |n: &str, p: &str| {
                let mut s = ServiceSettings::fresh(n, p);
                s.intent_active = intent;
                s.restore_at_startup = restore;
                SettingsLoad::Loaded(s)
            }
        };
        let r = rig_with(saved(true, true), Arc::new(NullSettingsSink));
        assert_eq!(r.core.state(), ControlState::Active);
        let r = rig_with(saved(true, false), Arc::new(NullSettingsSink));
        assert_eq!(r.core.state(), ControlState::Idle);
        let r = rig_with(saved(false, true), Arc::new(NullSettingsSink));
        assert_eq!(r.core.state(), ControlState::Idle);
    }

    #[test]
    fn startup_restore_failure_is_reported_and_retried() {
        let cpu_gone = |n: &str, p: &str| {
            let mut s = ServiceSettings::fresh(n, p);
            s.intent_active = true;
            s.restore_at_startup = true;
            SettingsLoad::Loaded(s)
        };
        let cpu = Arc::new(Mutex::new(None::<u16>));
        let profile = DeviceProfile::alienware_m15_r6().unwrap();
        let (name, prefix) = (profile.device_name.clone(), profile.acpi_prefix.to_string());
        let orch = ThermalOrchestrator::new(
            profile,
            Box::new(TempTransport {
                inner: SimBackend::new_alienware_m15_r6(),
                cpu: Arc::clone(&cpu),
                fail_writes: Arc::new(Mutex::new(false)),
            }),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
        );
        let mut core = ControlCore::new(
            orch,
            cpu_gone(&name, &prefix),
            Arc::new(NullSettingsSink),
            Duration::from_secs(2),
        );
        assert_eq!(core.state(), ControlState::RestoreFailed);
        let status = core.refresh().control;
        assert!(
            status
                .failure_reason
                .unwrap()
                .contains("no fresh CPU temperature")
        );
        assert!(status.intent_active && status.restore_at_startup);
        *cpu.lock().unwrap() = Some(70);
        core.tick(false);
        assert_eq!(core.state(), ControlState::Active);
    }

    #[test]
    fn wrong_device_and_corrupt_settings_are_not_applied() {
        for load in [
            SettingsLoad::WrongDevice {
                reason: "other machine".into(),
            },
            SettingsLoad::Invalid {
                reason: "truncated".into(),
            },
            SettingsLoad::Incompatible { found_version: 9 },
        ] {
            let mut r = rig_with(move |_, _| load, Arc::new(NullSettingsSink));
            assert_eq!(r.core.state(), ControlState::Idle);
            let status = r.core.refresh().control;
            assert!(status.settings_problem.is_some());
            assert!(!status.intent_active);
        }
    }

    #[test]
    fn superseding_command_ends_curve_and_later_ticks_do_not_undo_it() {
        let sink = Arc::new(RecordingSink::default());
        let mut r = rig_with(
            |n, p| SettingsLoad::Loaded(ServiceSettings::fresh(n, p)),
            sink.clone(),
        );
        *r.cpu.lock().unwrap() = Some(70);
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        let reply = r.core.handle(ControlOp::SetThermalMode(ThermalMode::Quiet));
        assert!(matches!(
            reply.result,
            Ok(OperationLifecycle::Confirmed { .. } | OperationLifecycle::Accepted { .. })
        ));
        assert_eq!(r.core.state(), ControlState::Idle);
        let writes_after_command = r.core.refresh().audit.len();
        *r.cpu.lock().unwrap() = Some(90);
        for _ in 0..3 {
            r.advance(2);
            r.core.tick(true);
        }
        assert_eq!(r.core.refresh().audit.len(), writes_after_command);
        let last = sink.saved.lock().unwrap().last().cloned().unwrap();
        assert!(!last.intent_active, "intent saved as stopped");
        assert_eq!(last.curve.cpu_points, curve().cpu_points, "parameters kept");
    }

    #[test]
    fn failed_superseding_command_keeps_the_curve_running() {
        let mut r = rig();
        *r.cpu.lock().unwrap() = Some(60);
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        r.core
            .orchestrator_mut()
            .set_sim_faults(SimFaults::default());
        *r.fail_writes.lock().unwrap() = true;
        let reply = r.core.handle(ControlOp::ApplyManualBoost {
            cpu_boost_pct: 90,
            gpu_boost_pct: 90,
        });
        assert!(matches!(
            reply.result,
            Ok(OperationLifecycle::Failed { .. })
        ));
        assert_eq!(r.core.state(), ControlState::Active);
        assert!(r.core.settings().intent_active || r.core.state() == ControlState::Active);
    }

    #[test]
    fn sensor_loss_triggers_verified_fallback_after_deadline_and_resumes() {
        let mut r = rig();
        *r.cpu.lock().unwrap() = Some(70);
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        *r.cpu.lock().unwrap() = None;
        // Below the deadline: still active (no premature fallback, no catch-up burst).
        r.advance(2);
        r.core.tick(false);
        assert_eq!(r.core.state(), ControlState::Active);
        r.advance(SENSOR_LOSS_DEADLINE.as_secs());
        r.core.tick(false);
        assert_eq!(r.core.state(), ControlState::Fallback);
        let status = r.core.refresh().control;
        assert!(status.failure_reason.unwrap().contains("firmware profile"));
        assert!(status.intent_active, "intent survives the fallback");
        // Sensor returns: control resumes only on fresh data.
        *r.cpu.lock().unwrap() = Some(75);
        r.advance(2);
        r.core.tick(false);
        assert_eq!(r.core.state(), ControlState::Active);
    }

    #[test]
    fn resume_discards_memory_and_rewrites_on_fresh_state() {
        let mut r = rig();
        *r.cpu.lock().unwrap() = Some(60);
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        r.advance(2);
        r.core.tick(false); // establishes the clock baseline
        let before = r.core.refresh().audit.len();
        // Wall clock jumps far beyond monotonic time: suspend/resume.
        *r.wall.lock().unwrap() += Duration::from_secs(3600);
        r.advance(2);
        r.core.tick(false);
        assert!(
            r.core.refresh().audit.len() > before,
            "boost is rewritten after resume instead of trusting stale memory"
        );
    }

    #[test]
    fn graceful_shutdown_leaves_curve_and_keeps_intent() {
        let sink = Arc::new(RecordingSink::default());
        let mut r = rig_with(
            |n, p| SettingsLoad::Loaded(ServiceSettings::fresh(n, p)),
            sink.clone(),
        );
        *r.cpu.lock().unwrap() = Some(70);
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        let report = r.core.shutdown();
        assert!(report.contains("left custom curve control"), "{report}");
        assert!(
            r.core.settings().intent_active,
            "maintenance stop keeps intent"
        );
        assert!(!r.core.orchestrator_mut().curve_enabled());
    }

    #[test]
    fn invalid_curve_is_rejected_before_hardware() {
        let mut r = rig();
        let mut bad = curve();
        bad.cpu_points[0].boost_pct = 200;
        let writes = r.core.refresh().audit.len();
        let reply = r.core.handle(ControlOp::ConfigureCurve(bad));
        assert!(reply.result.is_err());
        assert_eq!(reply.view.audit.len(), writes);
    }

    #[test]
    fn restore_preference_is_persisted_without_hardware_writes() {
        let sink = Arc::new(RecordingSink::default());
        let mut r = rig_with(
            |n, p| SettingsLoad::Loaded(ServiceSettings::fresh(n, p)),
            sink.clone(),
        );
        let writes = r.core.refresh().audit.len();
        let reply = r.core.handle(ControlOp::SetRestoreAtStartup(true));
        assert!(reply.result.is_ok());
        assert_eq!(reply.view.audit.len(), writes);
        assert!(
            sink.saved
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .restore_at_startup
        );
    }

    #[test]
    fn threaded_control_keeps_tracking_after_every_client_is_gone_and_never_blocks() {
        let cpu = Arc::new(Mutex::new(Some(70_u16)));
        let profile = DeviceProfile::alienware_m15_r6().unwrap();
        let (name, prefix) = (profile.device_name.clone(), profile.acpi_prefix.to_string());
        let orch = ThermalOrchestrator::new(
            profile,
            Box::new(TempTransport {
                inner: SimBackend::new_alienware_m15_r6(),
                cpu: Arc::clone(&cpu),
                fail_writes: Arc::new(Mutex::new(false)),
            }),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
        );
        let core = ControlCore::new(
            orch,
            SettingsLoad::Loaded(ServiceSettings::fresh(&name, &prefix)),
            Arc::new(NullSettingsSink),
            Duration::from_millis(25),
        );
        let handle = ControlHandle::spawn(core).unwrap();
        let reply = handle
            .request(
                ControlRequest::Op {
                    id: "t-1".to_string(),
                    op: ControlOp::ConfigureCurve(curve()),
                },
                Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        assert!(reply.result.is_ok());
        // Every client "disconnects": nothing reads or requests from here. Only the schedule runs.
        *cpu.lock().unwrap() = Some(46);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (view, _) = handle.published().unwrap();
            if matches!(
                view.telemetry.cpu_boost_pct,
                WireSensor::Valid { value: 2, .. }
            ) {
                break;
            }
            assert!(Instant::now() < deadline, "curve did not keep tracking");
            std::thread::sleep(Duration::from_millis(10));
        }
        let report = handle.stop(Duration::from_secs(5)).expect("graceful stop");
        assert!(report.contains("curve"), "{report}");
    }

    #[test]
    fn full_queue_reports_overload_instead_of_blocking() {
        let r = rig();
        let handle = ControlHandle::spawn_with_capacity(r.core, 1).unwrap();
        // Saturate: the worker is fast, so hammer from several threads and require that no call
        // ever blocks past its bounded wait and that overload is a distinct, typed result.
        let mut saw_ok = false;
        for _ in 0..200 {
            match handle.request(ControlRequest::Refresh, Duration::from_secs(2)) {
                Ok(Some(_)) => saw_ok = true,
                Ok(None) | Err(SubmitError::Overloaded) => {}
                Err(SubmitError::Stopped) => panic!("stopped"),
            }
        }
        assert!(saw_ok);
        let _ = handle.stop(Duration::from_secs(5));
        assert!(matches!(
            handle.request(ControlRequest::Refresh, Duration::from_millis(50)),
            Err(SubmitError::Stopped)
        ));
    }

    #[test]
    fn operation_memory_is_bounded() {
        let mut m = OperationMemory::default();
        for i in 0..(OPERATION_MEMORY + 10) {
            m.put(
                format!("id-{i}"),
                StoredOutcome::Rejected(ControlRejection {
                    code: "x",
                    message: String::new(),
                }),
            );
        }
        assert!(m.get("id-0").is_none());
        assert!(m.get(&format!("id-{}", OPERATION_MEMORY + 9)).is_some());
    }
}
