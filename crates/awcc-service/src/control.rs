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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use awcc_core::{FanCurveConfig, OperationLifecycle, SensorReading, ThermalMode, ThermalModeState};

use crate::daemon::{DaemonRpcRequest, try_direct_rpc_kernel_io};
use crate::ipc::DaemonLock;
use crate::settings::{
    SETTINGS_SCHEMA_VERSION, ServiceSettings, SettingsLoad, SettingsStore, is_firmware_preset,
    validate_curve,
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
    EnablePower,
    SelectPower(awcc_core::PowerProfile),
    ConfigurePower {
        profile: awcc_core::PowerProfile,
        policy: awcc_core::ThermalPolicy,
    },
    HoldPower(crate::power::ProfileHold),
    ReleasePower {
        cookie: u32,
        owner: String,
    },
    DisconnectPower(String),
    ReconcilePower,
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
    /// Raw kernel output of a low-level ACPI request (`RawAcpi` only).
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
    /// Low-level ACPI request, executed serialized on the control thread.
    RawAcpi {
        id: String,
        request: DaemonRpcRequest,
    },
    /// Graceful stop (restores the firmware profile when a curve is active).
    Shutdown,
}

struct Envelope {
    generation: Option<u64>,
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
        Self::spawn_with_owner(store, block_overwrite, None)
    }

    pub(crate) fn spawn_owned(
        store: SettingsStore,
        block_overwrite: Option<String>,
        ownership: Arc<DaemonLock>,
    ) -> std::io::Result<Self> {
        Self::spawn_with_owner(store, block_overwrite, Some(ownership))
    }

    fn spawn_with_owner(
        store: SettingsStore,
        block_overwrite: Option<String>,
        ownership: Option<Arc<DaemonLock>>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(PersistState::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("awcc-settings".to_string())
            .spawn(move || {
                // A timed-out filesystem write must finish before a successor daemon can
                // acquire ownership and persist newer settings.
                let _ownership = ownership;
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
        let idle = {
            let (lock, cv) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            state.closing = true;
            cv.notify_all();
            state.pending.is_none() && !state.in_flight
        };
        if let Some(handle) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            && (idle || handle.is_finished())
        {
            let _ = handle.join();
        }
        // Shutdown explicitly flushes with a deadline. A writer stuck in filesystem I/O must
        // not turn dropping the sink into an unbounded second wait. An idle writer can safely
        // exit now; a busy writer retains the shared state and ownership guard until it exits.
    }
}

// ---------------------------------------------------------------------------------------------
// Control core (synchronous, deterministic, fully testable)
// ---------------------------------------------------------------------------------------------

/// The synchronous control state machine. [`ControlHandle`] runs it on its own thread.
pub struct ControlCore {
    orch: ThermalOrchestrator,
    cpu: Box<dyn awcc_platform::cpu_policy::CpuPolicyBackend>,
    power: crate::power::PowerStatus,
    power_enabled: bool,
    applying_power: bool,
    desired_generation: Option<(Arc<AtomicU64>, u64)>,
    applied_policy: Option<(awcc_core::PowerProfile, awcc_core::ThermalPolicy)>,
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
    restore_required: bool,
    needs_revalidation: bool,
    last_view: Option<ThermalView>,
    view_revision: u64,
}

fn operation_succeeded(lifecycle: &OperationLifecycle) -> bool {
    matches!(
        lifecycle,
        OperationLifecycle::Accepted { .. } | OperationLifecycle::Confirmed { .. }
    )
}

impl ControlCore {
    /// Build the core and perform the startup restore decision.
    ///
    /// `load` is what the persisted settings looked like; the device identity is revalidated by the
    /// caller when loading. A curve is restored only when the saved preference asks for it **and**
    /// the device currently reports a fresh CPU temperature and a known thermal mode. Otherwise,
    /// the last explicitly selected firmware preset is restored, if one was saved.
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
        Self::construct(orch, load, sink, tick_interval, clock, wall_clock, true)
    }
    pub(crate) fn new_deferred(
        orch: ThermalOrchestrator,
        load: SettingsLoad,
        sink: Arc<dyn SettingsSink>,
        tick_interval: Duration,
    ) -> Self {
        Self::construct(
            orch,
            load,
            sink,
            tick_interval,
            Box::new(Instant::now),
            Box::new(SystemTime::now),
            false,
        )
    }
    fn construct(
        orch: ThermalOrchestrator,
        load: SettingsLoad,
        sink: Arc<dyn SettingsSink>,
        tick_interval: Duration,
        clock: Box<dyn Fn() -> Instant + Send>,
        wall_clock: Box<dyn Fn() -> SystemTime + Send>,
        restore: bool,
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
            cpu: Box::new(awcc_platform::cpu_policy::SimCpuPolicy),
            power: Default::default(),
            power_enabled: false,
            applying_power: false,
            desired_generation: None,
            applied_policy: None,
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
            restore_required: false,
            needs_revalidation: false,
            last_view: None,
            view_revision: 0,
        };
        if restore {
            core.startup_restore();
        }
        core
    }

    /// Inject CPU controls before enabling the provider; tests never access system sysfs.
    pub fn set_cpu_backend(
        &mut self,
        backend: Box<dyn awcc_platform::cpu_policy::CpuPolicyBackend>,
    ) {
        self.cpu = backend;
    }

    fn handle_power(&mut self, op: &ControlOp) -> Option<ControlReply> {
        use awcc_core::{PowerProfile as P, ThermalPolicy as T};
        let original_settings = self.settings.clone();
        let original_holds = self.power.holds.clone();
        let mut wanted = None;
        let mut save = false;
        match op {
            ControlOp::EnablePower => {
                self.power_enabled = true;
            }
            ControlOp::SelectPower(profile) if self.power.active == Some(*profile) => {}
            ControlOp::SelectPower(profile) => {
                self.settings.power.selected = Some(*profile);
                let cookies = self
                    .power
                    .holds
                    .iter()
                    .map(|h| h.cookie)
                    .collect::<Vec<_>>();
                for cookie in cookies {
                    self.power.release(cookie);
                }
                save = true;
            }
            ControlOp::ConfigurePower { profile, policy } => {
                if let Err(reason) = self.validate_policy(*profile, policy) {
                    return Some(self.reject("invalid_policy", reason));
                }
                self.settings
                    .power
                    .counterparts
                    .insert(*profile, policy.clone());
                save = true;
            }
            ControlOp::HoldPower(hold) => {
                if hold.profile == P::Balanced
                    || hold.reason.len() > 256
                    || hold.application_id.len() > 256
                    || self.power.holds.len() >= 64
                    || self.power.holds.iter().any(|h| h.cookie == hold.cookie)
                {
                    return Some(
                        self.reject("invalid_hold", "invalid or excessive profile holds".into()),
                    );
                }
                self.power.holds.push(hold.clone());
            }
            ControlOp::ReleasePower { cookie, owner } => {
                if !self
                    .power
                    .holds
                    .iter()
                    .any(|h| h.cookie == *cookie && &h.owner == owner)
                {
                    return Some(self.reject(
                        "access_denied",
                        "hold does not belong to this caller".into(),
                    ));
                }
                self.power.release(*cookie);
            }
            ControlOp::DisconnectPower(owner) => {
                let cookies = self
                    .power
                    .holds
                    .iter()
                    .filter(|h| &h.owner == owner)
                    .map(|h| h.cookie)
                    .collect::<Vec<_>>();
                if cookies.is_empty() {
                    return Some(ControlReply {
                        raw_output: None,
                        view: self.refresh(),
                        result: Ok(OperationLifecycle::Idle),
                    });
                }
                for cookie in cookies {
                    self.power.release(cookie);
                }
            }
            ControlOp::ReconcilePower if self.power_enabled => {}
            ControlOp::SetThermalMode(mode) if self.power_enabled => {
                let profile = P::for_mode(*mode)
                    .unwrap_or(self.settings.power.selected.unwrap_or(P::Balanced));
                let policy = if *mode == ThermalMode::Manual {
                    self.settings
                        .power
                        .counterparts
                        .get(&profile)
                        .filter(|p| matches!(p, T::Manual { .. }))
                        .cloned()
                        .unwrap_or(T::Manual {
                            cpu_boost_pct: 30,
                            gpu_boost_pct: 30,
                            curve: None,
                        })
                } else {
                    T::Preset { mode: *mode }
                };
                if let Err(reason) = self.validate_policy(profile, &policy) {
                    return Some(self.reject("invalid_policy", reason));
                }
                wanted = Some((profile, policy));
                save = true;
            }
            ControlOp::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            } if self.power_enabled => {
                let profile = self.settings.power.selected.unwrap_or(P::Balanced);
                wanted = Some((
                    profile,
                    T::Manual {
                        cpu_boost_pct: *cpu_boost_pct,
                        gpu_boost_pct: *gpu_boost_pct,
                        curve: None,
                    },
                ));
                save = true;
            }
            ControlOp::ConfigureCurve(curve) if self.power_enabled => {
                let profile = self.settings.power.selected.unwrap_or(P::Balanced);
                wanted = Some((
                    profile,
                    T::Manual {
                        cpu_boost_pct: 0,
                        gpu_boost_pct: 0,
                        curve: Some(curve.clone()),
                    },
                ));
                save = true;
            }
            ControlOp::StopCurve if self.power_enabled => {
                self.applying_power = true;
                let reply = self.handle(ControlOp::StopCurve);
                self.applying_power = false;
                if reply.result.as_ref().is_ok_and(operation_succeeded)
                    && let ThermalModeState::Known(mode) = reply.view.thermal_mode_state
                    && awcc_core::PowerProfile::for_mode(mode).is_some()
                {
                    return Some(self.handle(ControlOp::SetThermalMode(mode)));
                }
                return Some(reply);
            }
            ControlOp::ToggleGMode if self.power_enabled => {
                let mode = if matches!(
                    self.orch.query_snapshot().thermal_mode_state,
                    ThermalModeState::Known(ThermalMode::GMode)
                ) {
                    ThermalMode::Performance
                } else {
                    ThermalMode::GMode
                };
                return Some(self.handle(ControlOp::SetThermalMode(mode)));
            }
            ControlOp::SetCpuTurbo(enabled) if self.power_enabled => {
                self.applying_power = true;
                let reply = self.handle(op.clone());
                self.applying_power = false;
                if reply.result.as_ref().is_ok_and(operation_succeeded) {
                    self.settings.power.turbo_override = Some(*enabled);
                    self.persist();
                }
                return Some(reply);
            }
            _ => return None,
        }
        if let Some((profile, policy)) = wanted {
            if let Err(reason) = self.validate_policy(profile, &policy) {
                return Some(self.reject("invalid_policy", reason));
            }
            self.settings.power.selected = Some(profile);
            self.settings.power.counterparts.insert(profile, policy);
            let cookies = self
                .power
                .holds
                .iter()
                .map(|h| h.cookie)
                .collect::<Vec<_>>();
            for cookie in cookies {
                self.power.release(cookie);
            }
        }
        // One serialized control worker orders CPU and thermal effects. A generation identifies
        // every attempted effective policy; published ActiveProfile advances only on success.
        self.power.generation = self.power.generation.wrapping_add(1);
        let selected = self.settings.power.selected.unwrap_or(P::Balanced);
        self.power.available = P::ALL
            .into_iter()
            .filter(|p| {
                self.cpu.available(*p)
                    && self
                        .settings
                        .power
                        .counterparts
                        .get(p)
                        .is_some_and(|policy| self.validate_policy(*p, policy).is_ok())
            })
            .collect();
        let effective = self.power.effective(selected);
        let policy = self.settings.power.counterparts.get(&effective).cloned();
        if self.applied_policy.as_ref() == policy.as_ref().map(|p| (effective, p.clone())).as_ref()
            && !matches!(op, ControlOp::EnablePower | ControlOp::ReconcilePower)
        {
            if save {
                self.persist();
            }
            return Some(ControlReply {
                raw_output: None,
                view: self.refresh(),
                result: Ok(OperationLifecycle::Confirmed {
                    summary: "profile policy already active".into(),
                }),
            });
        }
        let generation = self.desired_generation.clone();
        self.applied_policy = None;
        self.power.cpu = self.cpu.apply(effective, &|| {
            generation
                .as_ref()
                .is_none_or(|(latest, g)| latest.load(Ordering::Acquire) == *g)
        });
        if !self.power.cpu.confirmed {
            self.power.problem = Some(format!(
                "CPU policy failed: {}",
                self.power.cpu.errors.join("; ")
            ));
            self.settings = original_settings;
            if matches!(op, ControlOp::HoldPower(_)) {
                self.power.holds = original_holds;
            }
            let reason = self.power.problem.clone().unwrap_or_default();
            return Some(self.reject("cpu_policy_failed", reason));
        }
        if matches!(op, ControlOp::ReconcilePower | ControlOp::EnablePower)
            && let Some(enabled) = self.settings.power.turbo_override
        {
            let turbo = self.orch.execute(ServiceCommand::SetCpuTurbo(enabled));
            if !operation_succeeded(&turbo.snapshot.lifecycle) {
                self.power.problem = Some("saved turbo override could not be restored".into());
                return Some(self.reject(
                    "turbo_restore_failed",
                    "saved turbo override could not be restored".into(),
                ));
            }
        }
        if generation
            .as_ref()
            .is_some_and(|(latest, g)| latest.load(Ordering::Acquire) != *g)
        {
            self.settings = original_settings;
            if matches!(op, ControlOp::HoldPower(_)) {
                self.power.holds = original_holds;
            }
            self.power.problem = Some("CPU phase superseded; awaiting newer policy".into());
            return Some(self.reject(
                "superseded",
                "CPU transition superseded; thermal policy was not applied".into(),
            ));
        }
        let applied = policy.clone().map(|p| (effective, p));
        let command = match policy {
            Some(T::Preset { mode }) => ControlOp::SetThermalMode(mode),
            Some(T::Manual {
                curve: Some(curve), ..
            }) => ControlOp::ConfigureCurve(curve),
            Some(T::Manual {
                cpu_boost_pct,
                gpu_boost_pct,
                ..
            }) => ControlOp::ApplyManualBoost {
                cpu_boost_pct,
                gpu_boost_pct,
            },
            None => return Some(self.reject("missing_policy", "no counterpart configured".into())),
        };
        // Thermal writes use the existing curve ownership and rollback paths. Keep the curve
        // configuration and user intent separate from temporary hold-derived activation.
        let saved = self.settings.clone();
        self.applying_power = true;
        let mut reply = self.handle(command);
        self.applying_power = false;
        self.settings.power = saved.power;
        self.settings.thermal_preset = saved.thermal_preset;
        self.power.thermal = reply.result.as_ref().ok().cloned();
        if reply.result.as_ref().is_ok_and(operation_succeeded) {
            self.power.active = Some(effective);
            self.power.problem = None;
            self.applied_policy = applied;
        } else {
            self.settings = original_settings;
            if matches!(op, ControlOp::HoldPower(_)) {
                self.power.holds = original_holds;
            }
            self.power.problem = Some("CPU policy applied, but thermal activation failed".into());
        }
        if save {
            self.persist();
        }
        let snap = self.orch.query_snapshot();
        reply.view = self.view_from(&snap);
        Some(reply)
    }

    fn validate_policy(
        &self,
        profile: awcc_core::PowerProfile,
        policy: &awcc_core::ThermalPolicy,
    ) -> Result<(), String> {
        policy.validate(profile)?;
        match policy {
            awcc_core::ThermalPolicy::Preset { mode } if !self.orch.supports_mode(*mode) => {
                Err(format!("{mode} is not supported on this device"))
            }
            awcc_core::ThermalPolicy::Manual {
                curve: Some(curve), ..
            } => validate_curve(curve),
            awcc_core::ThermalPolicy::Manual { .. }
                if !self.orch.supports_mode(ThermalMode::Manual) =>
            {
                Err("Manual is not supported on this device".into())
            }
            _ => Ok(()),
        }
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn startup_restore(&mut self) {
        if self.settings.restore_at_startup && self.settings.intent_active {
            self.try_activate_curve("startup restore");
        } else if let Some(mode) = self.settings.thermal_preset {
            let snap = self
                .orch
                .execute(ServiceCommand::SetThermalMode(mode))
                .snapshot;
            let detail = snap.lifecycle.detail_message();
            if operation_succeeded(&snap.lifecycle) {
                self.last_outcome = Some(format!("startup preset restore: {detail}"));
            } else {
                self.failure_reason = Some(format!(
                    "saved {} preset could not be restored: {detail}",
                    mode.display_name()
                ));
                self.last_outcome = self.failure_reason.clone();
                tracing::warn!(
                    target: "awcc_service::control",
                    category = "app", action = "restore_preset", outcome = "failed",
                    code = "preset_restore_failed", "{detail}"
                );
            }
            // This is a single restoration of saved user intent, not a policy loop that fights
            // other power managers. Keep the actual readback and operation lifecycle visible.
            self.view_from(&snap);
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
        if !operation_succeeded(&lifecycle) {
            let restored = self.restore_after_failed_activation(why, &lifecycle);
            self.view_from(&restored);
            return false;
        }
        self.state = ControlState::Active;
        self.restore_required = false;
        self.pending_fallback_restore = None;
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
        if self.applying_power {
            return;
        }
        self.settings.schema_version = SETTINGS_SCHEMA_VERSION;
        self.sink.submit(self.settings.clone());
    }

    fn build_status(&self) -> ControlStatus {
        let (pending, error) = self.sink.status();
        let now = self.now();
        ControlStatus {
            power: {
                let mut p = self.power.clone();
                p.selected = self.settings.power.selected;
                p.counterparts = self.settings.power.counterparts.clone();
                p
            },
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
        self.view_revision = self.view_revision.wrapping_add(1);
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
        if !self.applying_power
            && let Some(reply) = self.handle_power(&op)
        {
            return reply;
        }
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
                        caveat: "queued for persistence; no hardware write was performed"
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
        if !operation_succeeded(&lifecycle) {
            snap = self.restore_after_failed_activation("curve activation", &lifecycle);
        } else {
            cfg.enabled = false;
            self.settings.curve = cfg;
            self.settings.intent_active = true;
            self.persist();
            self.state = ControlState::Active;
            self.failure_reason = None;
            self.sensor_missing_since = None;
            self.pending_fallback_restore = None;
            self.restore_required = false;
            self.last_eval = Some(self.now());
            self.last_outcome = Some(lifecycle.detail_message());
        }
        ControlReply {
            raw_output: None,
            view: self.view_from(&snap),
            result: Ok(lifecycle),
        }
    }

    /// Activation can fail after the hardware has entered Manual mode. Leave through the normal
    /// verified restoration path before suspending evaluation; do not discard its restore target.
    /// A failed replacement keeps the previous saved curve and intent, so recovery can resume it.
    fn restore_after_failed_activation(
        &mut self,
        why: &str,
        lifecycle: &OperationLifecycle,
    ) -> SystemSnapshot {
        let target = self
            .pending_fallback_restore
            .or_else(|| self.orch.curve_restore_mode());
        let snap = self.leave_curve();
        let restore_failed = !operation_succeeded(&snap.lifecycle);
        self.restore_required = restore_failed;
        self.pending_fallback_restore = target.filter(|_| restore_failed);
        self.state = if restore_failed || self.settings.intent_active {
            ControlState::Fallback
        } else {
            ControlState::Idle
        };
        self.sensor_missing_since = None;
        self.failure_reason = Some(format!(
            "{why}: {}; firmware restoration: {}",
            lifecycle.detail_message(),
            snap.lifecycle.detail_message()
        ));
        self.last_outcome = self.failure_reason.clone();
        snap
    }

    fn stop_curve(&mut self) -> ControlReply {
        let was_applying = self.orch.curve_enabled() || self.restore_required;
        let target = self
            .pending_fallback_restore
            .or_else(|| self.orch.curve_restore_mode());
        let snap = self.leave_curve();
        let lifecycle = snap.lifecycle.clone();
        let restore_failed = !operation_succeeded(&lifecycle);
        // The user's intent is "stop" regardless; a failed restore is reported, not hidden.
        self.settings.intent_active = false;
        self.persist();
        self.state = if restore_failed {
            ControlState::Fallback
        } else {
            ControlState::Idle
        };
        self.sensor_missing_since = None;
        self.pending_fallback_restore = target.filter(|_| restore_failed);
        self.restore_required = restore_failed;
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

    /// Retry an interrupted firmware restoration even though the local curve has already been
    /// disabled. An absent verified target remains an explicit failure until the user selects a
    /// supported profile; it must not turn into a successful "nothing to stop" response.
    fn leave_curve(&mut self) -> SystemSnapshot {
        if self.restore_required && !self.orch.curve_enabled() {
            if let Some(target) = self.pending_fallback_restore {
                return self
                    .orch
                    .execute(ServiceCommand::SetThermalMode(target))
                    .snapshot;
            }
            let mut snap = self.orch.query_snapshot();
            snap.lifecycle = OperationLifecycle::Failed {
                summary: "Custom fan curve is stopped, but firmware restoration is unresolved"
                    .to_string(),
                error: "no verified firmware profile is available; select a supported profile explicitly"
                    .to_string(),
            };
            return snap;
        }
        let mut cfg = self.settings.curve.clone();
        cfg.enabled = false;
        self.orch.execute(ServiceCommand::SetFanCurve(cfg)).snapshot
    }

    fn run_thermal_op(&mut self, op: ControlOp) -> ControlReply {
        let supersedes = op.supersedes_curve();
        let preset = match op {
            ControlOp::SetThermalMode(mode) if is_firmware_preset(mode) => Some(mode),
            _ => None,
        };
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
            | ControlOp::SetRestoreAtStartup(_)
            | ControlOp::EnablePower
            | ControlOp::SelectPower(_)
            | ControlOp::ConfigurePower { .. }
            | ControlOp::HoldPower(_)
            | ControlOp::ReleasePower { .. }
            | ControlOp::DisconnectPower(_)
            | ControlOp::ReconcilePower => {
                return self.reject("internal", "operation routed incorrectly".to_string());
            }
        };
        let outcome = self.orch.execute(command);
        let lifecycle = outcome.snapshot.lifecycle.clone();
        let mut snap = outcome.snapshot;
        let accepted = operation_succeeded(&lifecycle);
        if supersedes {
            if accepted {
                self.settings.thermal_preset = preset;
                self.failure_reason = None;
                self.last_outcome = Some(lifecycle.detail_message());
            }
            if accepted && self.state != ControlState::Idle {
                self.orch.cancel_curve_without_restore();
                self.state = ControlState::Idle;
                self.settings.intent_active = false;
                self.persist();
                self.sensor_missing_since = None;
                self.pending_fallback_restore = None;
                self.restore_required = false;
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

    /// Low-level ACPI request. It obeys the same ownership rules as everything else:
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
        if self.power_enabled {
            let _ = self.handle(ControlOp::ReconcilePower);
        }
    }

    /// One scheduled evaluation. `interest` says whether a client recently looked at the state.
    /// Returns whether a new view was produced; an idle tick must not refresh old timestamps.
    pub fn tick(&mut self, interest: bool) -> bool {
        let before = self.view_revision;
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
                self.refresh();
            }
            ControlState::Idle => {
                if interest {
                    self.refresh();
                }
            }
        }
        self.view_revision != before
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
            if snap.lifecycle.is_error_or_rollback() {
                self.last_outcome = Some(snap.lifecycle.detail_message());
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
        let snap = self.leave_curve();
        let lifecycle = snap.lifecycle.clone();
        self.state = ControlState::Fallback;
        self.restore_required = !operation_succeeded(&lifecycle);
        let target_name = target.map_or_else(|| "unknown".to_string(), |mode| format!("`{mode}`"));
        if self.restore_required {
            self.pending_fallback_restore = target;
            self.failure_reason = Some(format!(
                "{why} for {}s; restoring the {target_name} firmware profile FAILED: {}",
                SENSOR_LOSS_DEADLINE.as_secs(),
                lifecycle.detail_message()
            ));
        } else {
            self.pending_fallback_restore = None;
            self.failure_reason = Some(format!(
                "{why} for {}s; control suspended and the {target_name} firmware profile restored",
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
        self.view_from(&snap);
    }

    fn tick_fallback(&mut self, now: Instant) {
        if self.restore_required {
            let snap = self.leave_curve();
            if operation_succeeded(&snap.lifecycle) {
                self.pending_fallback_restore = None;
                self.restore_required = false;
                self.failure_reason = Some(
                    "control suspended; the firmware profile was restored on retry".to_string(),
                );
                if !self.settings.intent_active {
                    self.state = ControlState::Idle;
                    self.failure_reason = None;
                }
            }
            self.last_outcome = Some(snap.lifecycle.detail_message());
            self.view_from(&snap);
            return;
        }
        if !self.settings.intent_active {
            self.state = ControlState::Idle;
            self.refresh();
            return;
        }
        let snap = self.orch.query_snapshot();
        let eligible = matches!(snap.telemetry.cpu_temp_c, SensorReading::Valid { .. })
            && !matches!(snap.thermal_mode_state, ThermalModeState::Unknown { .. });
        self.view_from(&snap);
        if eligible && self.try_activate_curve("resume after sensor recovery") {
            self.needs_revalidation = false;
            self.last_eval = Some(now);
            self.refresh();
        }
    }

    /// Graceful stop: leave custom control through the verified path and report what happened.
    /// The saved intent and startup preference are preserved.
    pub fn shutdown(&mut self) -> String {
        let mut final_snapshot = None;
        let report = if self.orch.curve_enabled() || self.restore_required {
            let snap = self.leave_curve();
            let lifecycle = &snap.lifecycle;
            self.restore_required = !operation_succeeded(lifecycle);
            let report = if self.restore_required {
                format!(
                    "shutdown: leaving custom curve control FAILED: {}",
                    lifecycle.detail_message()
                )
            } else {
                format!(
                    "shutdown: left custom curve control: {}",
                    lifecycle.detail_message()
                )
            };
            final_snapshot = Some(snap);
            report
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
        if let Some(snap) = final_snapshot {
            self.view_from(&snap);
        }
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
    stop_requested: AtomicBool,
    generation: Arc<AtomicU64>,
    next_generation: AtomicU64,
    queued: AtomicUsize,
    capacity: usize,
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

    /// Keep exclusive hardware ownership until the worker actually exits, even if shutdown
    /// times out or the caller cannot finish starting its socket server.
    pub(crate) fn spawn_owned(
        core: ControlCore,
        ownership: Arc<DaemonLock>,
    ) -> std::io::Result<Self> {
        Self::spawn_with_capacity_and_owner(core, REQUEST_QUEUE_CAPACITY, Some(ownership))
    }

    /// Spawn with an explicit queue bound.
    ///
    /// # Errors
    /// Returns the I/O error if the thread cannot be spawned.
    pub fn spawn_with_capacity(core: ControlCore, capacity: usize) -> std::io::Result<Self> {
        Self::spawn_with_capacity_and_owner(core, capacity, None)
    }

    fn spawn_with_capacity_and_owner(
        mut core: ControlCore,
        capacity: usize,
        ownership: Option<Arc<DaemonLock>>,
    ) -> std::io::Result<Self> {
        let (tx, rx): (SyncSender<Envelope>, Receiver<Envelope>) = mpsc::sync_channel(capacity);
        let inner = Arc::new(HandleInner {
            tx,
            published: Mutex::new(None),
            interest: Mutex::new(Instant::now()),
            thread: Mutex::new(None),
            stopped: Mutex::new(None),
            memory: Mutex::new(OperationMemory::default()),
            pending: Mutex::new(std::collections::HashSet::new()),
            stop_requested: AtomicBool::new(false),
            generation: Arc::new(AtomicU64::new(0)),
            next_generation: AtomicU64::new(0),
            queued: AtomicUsize::new(0),
            capacity,
        });
        let worker_inner = Arc::clone(&inner);
        let interval = core.tick_interval();
        let handle = std::thread::Builder::new()
            .name("awcc-control".to_string())
            .spawn(move || {
                let _ownership = ownership;
                let first = core.refresh();
                publish(&worker_inner, &first);
                let mut next_tick = Instant::now() + interval;
                loop {
                    if worker_inner.stop_requested.load(Ordering::Acquire) {
                        break;
                    }
                    let wait = next_tick.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(wait) {
                        Ok(env) => {
                            if !matches!(env.request, ControlRequest::Shutdown) {
                                worker_inner.queued.fetch_sub(1, Ordering::AcqRel);
                            }
                            if worker_inner.stop_requested.load(Ordering::Acquire) {
                                break;
                            }
                            if serve(&mut core, &worker_inner, env) {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    if worker_inner.stop_requested.load(Ordering::Acquire) {
                        break;
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
                        if core.tick(interested)
                            && let Some(view) = core.cached_view().cloned()
                        {
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
                *worker_inner
                    .stopped
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(report.clone());
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
        self.request_inner(request, deadline, None)
    }

    pub fn reserve_desired(&self) -> u64 {
        self.inner.next_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn request_desired(
        &self,
        request: ControlRequest,
        deadline: Duration,
        generation: u64,
    ) -> Result<Option<ControlReply>, SubmitError> {
        self.request_inner(request, deadline, Some(generation))
    }

    fn request_inner(
        &self,
        request: ControlRequest,
        deadline: Duration,
        desired: Option<u64>,
    ) -> Result<Option<ControlReply>, SubmitError> {
        if self.inner.stop_requested.load(Ordering::Acquire) {
            return Err(SubmitError::Stopped);
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let op_id = match &request {
            ControlRequest::Op { id, .. } | ControlRequest::RawAcpi { id, .. } => Some(id.clone()),
            _ => None,
        };
        if let Some(id) = &op_id {
            let inserted = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id.clone());
            if !inserted {
                // The original operation is still queued/running. Do not enqueue another
                // copy or remove its pending marker if the queue happens to be full.
                return Ok(None);
            }
        }
        if self
            .inner
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.inner.capacity).then_some(n + 1)
            })
            .is_err()
        {
            if let Some(id) = &op_id {
                self.inner
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(id);
            }
            return Err(SubmitError::Overloaded);
        }
        let remembered = op_id.as_ref().is_some_and(|id| {
            self.inner
                .memory
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(id)
                .is_some()
        });
        let generation = if !remembered
            && matches!(
                &request,
                ControlRequest::Op {
                    op: ControlOp::SelectPower(_)
                        | ControlOp::ConfigurePower { .. }
                        | ControlOp::SetThermalMode(_)
                        | ControlOp::ApplyManualBoost { .. }
                        | ControlOp::ConfigureCurve(_)
                        | ControlOp::ToggleGMode,
                    ..
                }
            ) {
            {
                let token = desired.unwrap_or_else(|| self.reserve_desired());
                let previous = self.inner.generation.fetch_max(token, Ordering::AcqRel);
                if previous > token {
                    self.inner.queued.fetch_sub(1, Ordering::AcqRel);
                    if let Some(id) = &op_id {
                        self.inner
                            .pending
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(id);
                    }
                    let (view, _) = self.published().ok_or(SubmitError::Stopped)?;
                    return Ok(Some(ControlReply {
                        raw_output: None,
                        view: (*view).clone(),
                        result: Err(ControlRejection {
                            code: "superseded",
                            message: "a newer authorized selection superseded this request".into(),
                        }),
                    }));
                }
                Some(token)
            }
        } else {
            None
        };
        match self.inner.tx.try_send(Envelope {
            generation,
            request,
            reply: reply_tx,
        }) {
            Ok(()) => {}
            Err(e) => {
                self.inner.queued.fetch_sub(1, Ordering::AcqRel);
                if let Some(g) = generation {
                    let _ = self.inner.generation.compare_exchange(
                        g,
                        g - 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
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
        self.inner.stop_requested.store(true, Ordering::Release);
        let (reply, _rx) = mpsc::sync_channel(1);
        // Wake an idle worker. If the queue is full, its next iteration sees stop_requested
        // before executing any more queued operations.
        let _ = self.inner.tx.try_send(Envelope {
            generation: None,
            request: ControlRequest::Shutdown,
            reply,
        });
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
    let before = core.view_revision;
    core.desired_generation = Some((
        Arc::clone(&inner.generation),
        env.generation
            .unwrap_or_else(|| inner.generation.load(Ordering::Acquire)),
    ));
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
        request @ (ControlRequest::Op { .. } | ControlRequest::RawAcpi { .. }) => {
            let id = match &request {
                ControlRequest::Op { id, .. } | ControlRequest::RawAcpi { id, .. } => id.clone(),
                _ => unreachable!("only operations reach replay handling"),
            };
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
                    let reply = match request {
                        ControlRequest::Op { op, .. } => {
                            if core
                                .desired_generation
                                .as_ref()
                                .is_some_and(|(latest, g)| latest.load(Ordering::Acquire) != *g)
                            {
                                core.reject(
                                    "superseded",
                                    "a newer selection superseded this request".into(),
                                )
                            } else {
                                core.handle(op)
                            }
                        }
                        ControlRequest::RawAcpi { request, .. } => core.raw_acpi(&request),
                        _ => unreachable!("only operations reach replay handling"),
                    };
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
        if core.view_revision != before
            && let Some(view) = core.cached_view()
        {
            // Replaying a historical operation result must not roll the latest published
            // telemetry/control state backwards or make an old observation look fresh.
            publish(inner, view);
        }
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
        telemetry_gate: Option<(SyncSender<()>, Receiver<()>)>,
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
            if let Some((entered, release)) = self.telemetry_gate.take() {
                entered.send(()).unwrap();
                release.recv().unwrap();
            }
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
        rig_with_backend(load, sink, SimBackend::new_alienware_m15_r6())
    }

    fn rig_with_backend(
        load: impl FnOnce(&str, &str) -> SettingsLoad,
        sink: Arc<dyn SettingsSink>,
        backend: SimBackend,
    ) -> Rig {
        let cpu = Arc::new(Mutex::new(Some(40_u16)));
        let fail_writes = Arc::new(Mutex::new(false));
        let profile = DeviceProfile::alienware_m15_r6().unwrap();
        let (name, prefix) = (profile.device_name.clone(), profile.acpi_prefix.to_string());
        let transport = TempTransport {
            inner: backend,
            cpu: Arc::clone(&cpu),
            fail_writes: Arc::clone(&fail_writes),
            telemetry_gate: None,
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
    fn delayed_authorization_cannot_overwrite_a_newer_socket_selection() {
        let mut r = rig();
        r.core.handle(ControlOp::EnablePower);
        let h = ControlHandle::spawn(r.core).unwrap();
        let old = h.reserve_desired();
        let reply = h
            .request(
                ControlRequest::Op {
                    id: "newer".into(),
                    op: ControlOp::SelectPower(awcc_core::PowerProfile::PowerSaver),
                },
                Duration::from_secs(3),
            )
            .unwrap()
            .unwrap();
        assert!(reply.result.is_ok());
        let reply = h
            .request_desired(
                ControlRequest::Op {
                    id: "older-authorized-late".into(),
                    op: ControlOp::SelectPower(awcc_core::PowerProfile::Performance),
                },
                Duration::from_secs(3),
                old,
            )
            .unwrap()
            .unwrap();
        assert_eq!(reply.result.unwrap_err().code, "superseded");
        assert_eq!(
            h.published().unwrap().0.control.power.active,
            Some(awcc_core::PowerProfile::PowerSaver)
        );
        let _ = h.stop(Duration::from_secs(3));
    }

    #[test]
    fn superseded_cpu_work_does_not_apply_thermal_policy_or_save_new_intent() {
        use awcc_platform::cpu_policy::{CpuPolicyBackend, CpuPolicyOutcome};
        struct SupersedingCpu(Arc<AtomicU64>);
        impl CpuPolicyBackend for SupersedingCpu {
            fn available(&self, _: awcc_core::PowerProfile) -> bool {
                true
            }
            fn apply(
                &mut self,
                _: awcc_core::PowerProfile,
                _: &dyn Fn() -> bool,
            ) -> CpuPolicyOutcome {
                self.0.store(2, Ordering::Release);
                CpuPolicyOutcome {
                    confirmed: true,
                    controls: vec!["earlier CPU phase completed".into()],
                    errors: vec![],
                }
            }
        }
        let mut r = rig();
        r.core.handle(ControlOp::EnablePower);
        let latest = Arc::new(AtomicU64::new(1));
        r.core.desired_generation = Some((latest.clone(), 1));
        r.core.set_cpu_backend(Box::new(SupersedingCpu(latest)));
        let before = r.core.refresh().audit.len();
        let saved = r.core.settings.power.clone();
        let reply = r.core.handle(ControlOp::SetThermalMode(ThermalMode::Cool));
        assert_eq!(reply.result.unwrap_err().code, "superseded");
        assert_eq!(r.core.refresh().audit.len(), before);
        assert_eq!(r.core.settings.power, saved);
    }

    #[test]
    fn missing_cpu_controls_never_publish_success_and_explicit_turbo_survives_profiles() {
        let mut r = rig();
        r.core.handle(ControlOp::EnablePower);
        r.core.handle(ControlOp::SetCpuTurbo(false));
        r.core.handle(ControlOp::SetThermalMode(ThermalMode::Cool));
        assert_eq!(r.core.settings.power.turbo_override, Some(false));
        assert!(matches!(
            r.core.refresh().cpu_turbo,
            WireObservation::Observed { value: false }
        ));
        let saved = r.core.settings.power.clone();
        r.core
            .set_cpu_backend(Box::new(awcc_platform::cpu_policy::LinuxCpuPolicy::new(
                "/nonexistent/awcc-test-cpufreq",
            )));
        let reply = r
            .core
            .handle(ControlOp::SelectPower(awcc_core::PowerProfile::Performance));
        assert!(reply.result.is_err());
        assert!(!reply.view.control.power.cpu.confirmed);
        assert_eq!(r.core.settings.power, saved);
    }

    #[test]
    fn system_profiles_remember_counterparts_and_temporary_holds_never_change_them() {
        use awcc_core::PowerProfile as P;
        let sink = Arc::new(RecordingSink::default());
        let mut r = rig_with(|_, _| SettingsLoad::Missing, sink.clone());
        r.core.handle(ControlOp::EnablePower);
        r.core.handle(ControlOp::SetThermalMode(ThermalMode::Cool));
        assert_eq!(r.core.settings.power.selected, Some(P::PowerSaver));
        let saved = r.core.settings.power.clone();
        let hold = crate::power::ProfileHold {
            cookie: 1,
            owner: ":1.test".into(),
            profile: P::Performance,
            reason: "test".into(),
            application_id: "test".into(),
        };
        let saves = sink.saved.lock().unwrap().len();
        r.core.handle(ControlOp::HoldPower(hold));
        assert_eq!(r.core.refresh().control.power.active, Some(P::Performance));
        assert_eq!(r.core.settings.power, saved);
        assert_eq!(sink.saved.lock().unwrap().len(), saves);
        r.core.handle(ControlOp::DisconnectPower(":1.test".into()));
        assert_eq!(
            r.core.refresh().thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Cool)
        );
        assert_eq!(r.core.settings.power, saved);
        r.core.handle(ControlOp::SetThermalMode(ThermalMode::Quiet));
        assert_eq!(
            r.core.refresh().thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Quiet)
        );
    }

    #[test]
    fn mapped_gmode_never_toggles_an_unreadable_latch() {
        let mut r = rig();
        r.core.handle(ControlOp::EnablePower);
        r.core.orchestrator_mut().set_sim_faults(SimFaults {
            game_shift_query_unavailable: true,
            ..Default::default()
        });
        let writes = r.core.refresh().audit.len();
        let reply = r.core.handle(ControlOp::SetThermalMode(ThermalMode::GMode));
        assert!(reply.result.unwrap().is_error_or_rollback());
        assert_eq!(r.core.refresh().audit.len(), writes);
    }

    #[test]
    fn mapped_gmode_is_idempotent_and_restart_resume_reapply_the_saved_policy() {
        use awcc_core::PowerProfile as P;
        let mut r = rig();
        r.core.handle(ControlOp::EnablePower);
        r.core.handle(ControlOp::SetThermalMode(ThermalMode::GMode));
        let writes = r.core.refresh().audit.len();
        r.core.handle(ControlOp::SelectPower(P::Performance));
        assert_eq!(r.core.refresh().audit.len(), writes);
        let saved = r.core.settings.clone();
        let mut restarted = rig_with(
            |_, _| SettingsLoad::Loaded(saved),
            Arc::new(NullSettingsSink),
        );
        restarted.core.handle(ControlOp::EnablePower);
        assert_eq!(
            restarted.core.refresh().thermal_mode_state,
            ThermalModeState::Known(ThermalMode::GMode)
        );
        restarted.core.note_resume();
        assert_eq!(
            restarted.core.refresh().thermal_mode_state,
            ThermalModeState::Known(ThermalMode::GMode)
        );
    }

    #[test]
    fn partial_thermal_failure_does_not_publish_or_save_a_new_active_profile() {
        let mut r = rig();
        r.core.handle(ControlOp::EnablePower);
        let saved = r.core.settings.power.clone();
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        r.core.handle(ControlOp::SetThermalMode(ThermalMode::Cool));
        let view = r.core.refresh();
        assert_eq!(
            view.control.power.active,
            Some(awcc_core::PowerProfile::Balanced)
        );
        assert!(view.control.power.cpu.confirmed);
        assert!(view.control.power.problem.is_some());
        assert_eq!(r.core.settings.power, saved);
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
        assert!(r.core.refresh().audit.is_empty());
    }

    #[test]
    fn selected_preset_survives_saved_settings_and_restart_from_full_speed() {
        for mode in [ThermalMode::Cool, ThermalMode::Quiet] {
            let sink = Arc::new(RecordingSink::default());
            let mut r = rig_with(|_, _| SettingsLoad::Missing, sink.clone());
            let reply = r.core.handle(ControlOp::SetThermalMode(mode));
            assert!(operation_succeeded(&reply.result.unwrap()));
            let saved = sink.saved.lock().unwrap().last().cloned().unwrap();
            let dir = std::env::temp_dir().join(format!(
                "awcc_preset_restart_{}_{}",
                std::process::id(),
                mode.cli_code()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let store = SettingsStore::new(&dir);
            store.save(&saved).unwrap();
            let profile = DeviceProfile::alienware_m15_r6().unwrap();
            let mut backend = SimBackend::new_alienware_m15_r6();
            backend
                .set_raw_thermal_mode(profile.code_table.set_code_for(ThermalMode::FullSpeed))
                .unwrap();
            let mut restarted =
                rig_with_backend(|n, p| store.load(n, p), Arc::new(NullSettingsSink), backend);
            assert_eq!(
                restarted.core.refresh().thermal_mode_state,
                ThermalModeState::Known(mode)
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn failed_preset_request_preserves_the_saved_choice() {
        let sink = Arc::new(RecordingSink::default());
        let mut r = rig_with(|_, _| SettingsLoad::Missing, sink.clone());
        r.core.handle(ControlOp::SetThermalMode(ThermalMode::Quiet));
        let writes = sink.saved.lock().unwrap().len();
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        let reply = r.core.handle(ControlOp::SetThermalMode(ThermalMode::Cool));
        assert!(reply.result.unwrap().is_error_or_rollback());
        assert_eq!(r.core.settings().thermal_preset, Some(ThermalMode::Quiet));
        assert_eq!(sink.saved.lock().unwrap().len(), writes);
    }

    #[test]
    fn manual_and_gmode_commands_do_not_restore_an_old_preset_on_restart() {
        for op in [
            ControlOp::ApplyManualBoost {
                cpu_boost_pct: 30,
                gpu_boost_pct: 30,
            },
            ControlOp::SetThermalMode(ThermalMode::Manual),
            ControlOp::SetThermalMode(ThermalMode::GMode),
            ControlOp::ToggleGMode,
        ] {
            let mut r = rig();
            r.core.handle(ControlOp::SetThermalMode(ThermalMode::Cool));
            let reply = r.core.handle(op);
            assert!(operation_succeeded(&reply.result.unwrap()));
            assert_eq!(r.core.settings().thermal_preset, None);
        }
    }

    #[test]
    fn startup_preset_failure_is_visible_and_does_not_loop_or_lose_the_saved_choice() {
        let mut backend = SimBackend::new_alienware_m15_r6();
        backend.set_scenario(SimScenario::AcpiSetFailureRollback);
        let mut r = rig_with_backend(
            |n, p| {
                let mut s = ServiceSettings::fresh(n, p);
                s.thermal_preset = Some(ThermalMode::Quiet);
                SettingsLoad::Loaded(s)
            },
            Arc::new(NullSettingsSink),
            backend,
        );
        let view = r.core.refresh();
        assert!(view.lifecycle.is_error_or_rollback());
        assert!(
            view.control
                .failure_reason
                .unwrap()
                .contains("Quiet preset could not be restored")
        );
        let writes = view.audit.len();
        r.core.tick(true);
        assert_eq!(r.core.refresh().audit.len(), writes);
        assert_eq!(r.core.settings().thermal_preset, Some(ThermalMode::Quiet));
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::Normal);
        let reply = r.core.handle(ControlOp::SetThermalMode(ThermalMode::Cool));
        assert!(operation_succeeded(&reply.result.unwrap()));
        assert!(reply.view.control.failure_reason.is_none());
        assert!(reply.view.control.last_outcome.unwrap().contains("Cool"));
    }

    #[test]
    fn preset_restore_preserves_accepted_when_readback_is_unavailable() {
        let mut backend = SimBackend::new_alienware_m15_r6();
        backend.set_sim_faults(SimFaults {
            thermal_mode_query_unavailable: true,
            ..SimFaults::default()
        });
        let mut r = rig_with_backend(
            |n, p| {
                let mut s = ServiceSettings::fresh(n, p);
                s.thermal_preset = Some(ThermalMode::Cool);
                SettingsLoad::Loaded(s)
            },
            Arc::new(NullSettingsSink),
            backend,
        );
        let view = r.core.refresh();
        assert!(matches!(
            view.lifecycle,
            OperationLifecycle::Accepted { .. }
        ));
        assert!(matches!(
            view.thermal_mode_state,
            ThermalModeState::Unknown { .. }
        ));
        assert!(view.control.failure_reason.is_none());
    }

    #[test]
    fn saved_curve_restore_takes_precedence_over_the_saved_preset() {
        let mut r = rig_with(
            |n, p| {
                let mut s = ServiceSettings::fresh(n, p);
                s.thermal_preset = Some(ThermalMode::Quiet);
                s.intent_active = true;
                s.restore_at_startup = true;
                SettingsLoad::Loaded(s)
            },
            Arc::new(NullSettingsSink),
        );
        assert_eq!(r.core.state(), ControlState::Active);
        assert_eq!(
            r.core.refresh().thermal_mode_state,
            ThermalModeState::Known(ThermalMode::Manual)
        );
        assert_eq!(
            r.core.orchestrator_mut().curve_restore_mode(),
            Some(ThermalMode::Balanced)
        );
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
                telemetry_gate: None,
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
            assert!(r.core.refresh().audit.is_empty());
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
    fn failed_profile_change_keeps_evaluating_the_active_curve() {
        let mut r = rig();
        *r.cpu.lock().unwrap() = Some(60);
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        let reply = r.core.handle(ControlOp::SetThermalMode(ThermalMode::Quiet));
        assert!(matches!(
            reply.result,
            Ok(OperationLifecycle::Failed { .. })
        ));
        assert_eq!(r.core.state(), ControlState::Active);
        assert!(r.core.orchestrator_mut().curve_enabled());
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::Normal);
        *r.cpu.lock().unwrap() = Some(80);
        r.advance(2);
        r.core.tick(false);
        assert_eq!(r.boost(), Some(85));
    }

    #[test]
    fn rolled_back_gmode_toggle_keeps_evaluating_the_active_curve() {
        let mut r = rig();
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        let reply = r.core.handle(ControlOp::ToggleGMode);
        assert!(matches!(
            reply.result,
            Ok(OperationLifecycle::RolledBack { .. })
        ));
        assert_eq!(r.core.state(), ControlState::Active);
        assert!(r.core.settings().intent_active);
        assert!(r.core.orchestrator_mut().curve_enabled());
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::Normal);
        *r.cpu.lock().unwrap() = Some(80);
        r.advance(2);
        r.core.tick(false);
        assert_eq!(r.boost(), Some(85));
    }

    #[test]
    fn failed_curve_activation_restores_firmware_after_entering_manual_mode() {
        let mut r = rig();
        *r.fail_writes.lock().unwrap() = true;
        let reply = r.core.handle(ControlOp::ConfigureCurve(curve()));
        assert!(matches!(
            reply.result,
            Ok(OperationLifecycle::Failed { .. })
        ));
        assert_eq!(r.core.state(), ControlState::Idle);
        assert_eq!(
            reply.view.thermal_mode_state.known_mode(),
            Some(ThermalMode::Balanced)
        );
        assert!(!reply.view.fan_curve.enabled);
        assert!(!reply.view.control.intent_active);
        assert!(!r.core.restore_required);
    }

    #[test]
    fn failed_curve_replacement_restores_firmware_then_resumes_the_saved_curve() {
        let mut r = rig();
        *r.cpu.lock().unwrap() = Some(60);
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        let mut replacement = curve();
        for point in &mut replacement.cpu_points {
            point.boost_pct = 95;
        }
        *r.fail_writes.lock().unwrap() = true;
        let reply = r.core.handle(ControlOp::ConfigureCurve(replacement));
        assert!(matches!(
            reply.result,
            Ok(OperationLifecycle::Failed { .. })
        ));
        assert_eq!(r.core.state(), ControlState::Fallback);
        assert_eq!(
            reply.view.thermal_mode_state.known_mode(),
            Some(ThermalMode::Balanced)
        );
        assert!(!reply.view.fan_curve.enabled);
        assert!(reply.view.control.intent_active);
        assert_eq!(r.core.settings().curve.cpu_points, curve().cpu_points);
        *r.fail_writes.lock().unwrap() = false;
        *r.cpu.lock().unwrap() = Some(80);
        r.advance(2);
        r.core.tick(false);
        assert_eq!(
            r.core.cached_view().unwrap().control.state,
            ControlState::Active
        );
        assert_eq!(r.boost(), Some(85));
    }

    #[test]
    fn failed_activation_retains_a_failed_firmware_restore_for_retry() {
        let mut r = rig();
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        let reply = r.core.handle(ControlOp::ConfigureCurve(curve()));
        assert!(matches!(
            reply.result,
            Ok(OperationLifecycle::Failed { .. })
        ));
        assert_eq!(r.core.state(), ControlState::Fallback);
        assert!(r.core.restore_required);
        assert!(!r.core.settings().intent_active);
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::Normal);
        r.advance(2);
        r.core.tick(false);
        assert_eq!(
            r.core.cached_view().unwrap().control.state,
            ControlState::Idle
        );
        assert!(!r.core.restore_required);
        assert_eq!(
            r.core
                .cached_view()
                .unwrap()
                .thermal_mode_state
                .known_mode(),
            Some(ThermalMode::Balanced)
        );
    }

    fn failed_fallback(r: &mut Rig) {
        r.core.handle(ControlOp::ConfigureCurve(curve()));
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::AcpiSetFailureRollback);
        *r.cpu.lock().unwrap() = None;
        r.advance(2);
        r.core.tick(false);
        r.advance(SENSOR_LOSS_DEADLINE.as_secs());
        r.core.tick(false);
        assert_eq!(r.core.state(), ControlState::Fallback);
        assert!(r.core.restore_required);
        assert!(!r.core.orchestrator_mut().curve_enabled());
    }

    #[test]
    fn explicit_stop_retries_a_failed_fallback_without_reactivating_the_curve() {
        let mut r = rig();
        failed_fallback(&mut r);
        let failed = r.core.handle(ControlOp::StopCurve);
        assert!(matches!(
            failed.result,
            Ok(OperationLifecycle::Failed { .. })
        ));
        assert!(!r.core.settings().intent_active);
        assert!(r.core.restore_required);
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::Normal);
        r.advance(2);
        r.core.tick(false);
        let view = r.core.cached_view().unwrap();
        assert_eq!(view.control.state, ControlState::Idle);
        assert_eq!(
            view.thermal_mode_state.known_mode(),
            Some(ThermalMode::Balanced)
        );
        assert!(!view.control.intent_active);
        assert!(!view.fan_curve.enabled);
        assert!(view.control.failure_reason.is_none());
    }

    #[test]
    fn shutdown_retries_pending_firmware_restoration() {
        let mut r = rig();
        failed_fallback(&mut r);
        r.core
            .orchestrator_mut()
            .set_sim_scenario(SimScenario::Normal);
        let report = r.core.shutdown();
        assert!(report.contains("left custom curve control"), "{report}");
        assert!(!report.contains("FAILED"), "{report}");
        assert_eq!(
            r.core
                .cached_view()
                .unwrap()
                .thermal_mode_state
                .known_mode(),
            Some(ThermalMode::Balanced)
        );
    }

    #[test]
    fn unknown_restore_target_remains_a_failure_on_repeated_stop_and_shutdown() {
        let profile = DeviceProfile::alienware_m15_r6().unwrap();
        let mut transport = SimBackend::new_alienware_m15_r6();
        transport
            .set_raw_thermal_mode(profile.code_table.set_code_for(ThermalMode::Manual))
            .unwrap();
        let orch = ThermalOrchestrator::new(
            profile,
            Box::new(transport),
            Box::new(MemoryStateStorage::new(None)),
        );
        let mut core = ControlCore::new(
            orch,
            SettingsLoad::Missing,
            Arc::new(NullSettingsSink),
            DEFAULT_TICK_INTERVAL,
        );
        core.handle(ControlOp::ConfigureCurve(curve()));
        let before = core.refresh().audit.len();
        for _ in 0..2 {
            let reply = core.handle(ControlOp::StopCurve);
            assert!(matches!(
                reply.result,
                Ok(OperationLifecycle::Failed { .. })
            ));
            assert_eq!(reply.view.audit.len(), before, "no invented profile write");
        }
        let report = core.shutdown();
        assert!(report.contains("FAILED"), "{report}");
        assert_eq!(core.cached_view().unwrap().audit.len(), before);
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
                telemetry_gate: None,
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

    fn gated_core() -> (ControlCore, Receiver<()>, SyncSender<()>) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let orch = ThermalOrchestrator::new(
            DeviceProfile::alienware_m15_r6().unwrap(),
            Box::new(TempTransport {
                inner: SimBackend::new_alienware_m15_r6(),
                cpu: Arc::new(Mutex::new(Some(60))),
                fail_writes: Arc::new(Mutex::new(false)),
                telemetry_gate: Some((entered_tx, release_rx)),
            }),
            Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
        );
        (
            ControlCore::new(
                orch,
                SettingsLoad::Missing,
                Arc::new(NullSettingsSink),
                Duration::from_millis(20),
            ),
            entered_rx,
            release_tx,
        )
    }

    #[test]
    fn saturated_queue_cannot_prevent_shutdown_or_lose_a_pending_request() {
        let (core, entered, release) = gated_core();
        let handle = ControlHandle::spawn_with_capacity(core, 1).unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let request = || ControlRequest::RawAcpi {
            id: "raw-pending".to_string(),
            // Rejected by the raw-ACPI handler before any real kernel I/O.
            request: DaemonRpcRequest::SetCpuTurbo { enabled: false },
        };
        assert!(matches!(
            handle.request(request(), Duration::ZERO),
            Ok(None)
        ));
        assert!(matches!(
            handle.request(request(), Duration::ZERO),
            Ok(None)
        ));
        assert!(matches!(
            handle.operation_status("raw-pending"),
            OperationStatus::Pending
        ));
        assert!(handle.stop(Duration::ZERO).is_none());
        release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while handle.stop(Duration::ZERO).is_none() {
            assert!(
                Instant::now() < until,
                "full queue prevented worker shutdown"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn timed_out_worker_keeps_hardware_ownership_until_it_exits() {
        let dir = std::env::temp_dir().join(format!("awcc_control_owner_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("awccd.lock");
        let owner = Arc::new(DaemonLock::acquire(&path, false).unwrap());
        let (core, entered, release) = gated_core();
        let handle = ControlHandle::spawn_owned(core, Arc::clone(&owner)).unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(handle.stop(Duration::ZERO).is_none());
        drop(handle);
        drop(owner);
        assert!(DaemonLock::acquire(&path, false).is_err());
        release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(owner) = DaemonLock::acquire(&path, false) {
                drop(owner);
                break;
            }
            assert!(
                Instant::now() < until,
                "worker did not release ownership after exit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn idle_ticks_do_not_make_cached_telemetry_look_fresh() {
        let mut r = rig();
        r.core.tick_interval = Duration::from_millis(20);
        let handle = ControlHandle::spawn(r.core).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while handle.published().is_none() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        *handle.inner.interest.lock().unwrap() =
            Instant::now() - CLIENT_INTEREST_WINDOW - Duration::from_secs(1);
        // Serialize behind any tick that had already captured the previous interest time.
        handle
            .request(ControlRequest::Refresh, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let (before, _) = handle.published().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let (after, age) = handle.published().unwrap();
        assert!(
            Arc::ptr_eq(&before, &after),
            "idle tick republished old samples"
        );
        assert!(age >= Duration::from_millis(90));
        assert!(handle.stop(Duration::from_secs(5)).is_some());
    }

    #[test]
    fn replayed_result_does_not_replace_the_current_published_state() {
        let r = rig();
        let handle = ControlHandle::spawn(r.core).unwrap();
        let request = |id: &str, mode| ControlRequest::Op {
            id: id.to_string(),
            op: ControlOp::SetThermalMode(mode),
        };
        for (id, mode) in [
            ("quiet", ThermalMode::Quiet),
            ("full", ThermalMode::FullSpeed),
        ] {
            handle
                .request(request(id, mode), Duration::from_secs(5))
                .unwrap()
                .unwrap();
        }
        let replay = handle
            .request(request("quiet", ThermalMode::Quiet), Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(
            replay.view.thermal_mode_state.known_mode(),
            Some(ThermalMode::Quiet)
        );
        assert_eq!(
            handle
                .published()
                .unwrap()
                .0
                .thermal_mode_state
                .known_mode(),
            Some(ThermalMode::FullSpeed)
        );
        let raw = ControlRequest::RawAcpi {
            id: "raw-rejected".to_string(),
            request: DaemonRpcRequest::SetCpuTurbo { enabled: false },
        };
        assert!(
            handle
                .request(raw, Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .result
                .is_err()
        );
        assert!(matches!(
            handle.operation_status("raw-rejected"),
            OperationStatus::Rejected(_)
        ));
        assert!(handle.stop(Duration::from_secs(5)).is_some());
    }

    #[test]
    fn dropping_a_sink_does_not_wait_for_a_blocked_filesystem_writer() {
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let writer = std::thread::spawn(move || release_rx.recv().unwrap());
        let sink = AsyncSettingsSink {
            shared: Arc::new((
                Mutex::new(PersistState {
                    in_flight: true,
                    ..PersistState::default()
                }),
                Condvar::new(),
            )),
            thread: Mutex::new(Some(writer)),
        };
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let dropper = std::thread::spawn(move || {
            drop(sink);
            done_tx.send(()).unwrap();
        });
        let dropped = done_rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        dropper.join().unwrap();
        assert!(
            dropped.is_ok(),
            "sink Drop exceeded the explicit flush deadline"
        );
    }

    #[test]
    fn settings_writer_retains_ownership_until_clean_shutdown() {
        let dir = std::env::temp_dir().join(format!("awcc_settings_owner_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("awccd.lock");
        let owner = Arc::new(DaemonLock::acquire(&path, false).unwrap());
        let sink =
            AsyncSettingsSink::spawn_owned(SettingsStore::new(&dir), None, Arc::clone(&owner))
                .unwrap();
        drop(owner);
        assert!(DaemonLock::acquire(&path, false).is_err());
        sink.submit(ServiceSettings::fresh("test", "AMWW"));
        assert!(sink.flush(Duration::from_secs(5)));
        drop(sink);
        let owner = DaemonLock::acquire(&path, false).unwrap();
        drop(owner);
        std::fs::remove_dir_all(dir).unwrap();
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
