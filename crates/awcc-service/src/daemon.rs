//! The privileged daemon (`awccd`): exclusive ownership, daemon-owned thermal control, the framed
//! authorized IPC server, and the low-level ACPI request types.
//!
//! `run_system_daemon` is the single production entry point (`awcc-cli --daemon`). It takes the
//! exclusive lock first, resolves the device from root-controlled resources only, restores
//! persisted settings through the normal eligibility checks, and serves clients until it receives
//! SIGTERM/SIGINT, then leaves custom curve control through the verified path before exiting.
//! See [`crate::control`] for scheduling and failure policy and [`crate::ipc`] for the protocol.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awcc_core::{AcpiPrefix, FanCurveConfig, FanCurvePoint};
use awcc_platform::{
    CommandExecutor, HwmonProvider, LinuxAcpiTransport, PosixStateStore, TRUSTED_DATABASE_PATHS,
    resolve_trusted_host_device_profile,
};
use serde::{Deserialize, Serialize};

use crate::ThermalOrchestrator;
use crate::control::{
    AsyncSettingsSink, ControlCore, ControlHandle, DEFAULT_TICK_INTERVAL, SettingsSink,
};
use crate::ipc::{
    AllowlistAuthorizer, Authorizer, BuildSummary, DEFAULT_AUTHORIZED_USERS_PATH,
    DEFAULT_LOCK_PATH, DaemonLock, IpcServer, OwnershipError, ServerConfig,
};
use crate::settings::{DEFAULT_STATE_DIR, SettingsLoad, SettingsStore};

/// Default Unix domain socket path used by `awccd`.
pub const DEFAULT_SOCKET_PATH: &str = "/run/awcc/awcc.sock";

/// Structured RPC request sent over `/run/awcc/awcc.sock` to the privileged daemon.
///
/// Unknown fields and unenumerated operations are rejected at deserialization time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DaemonRpcRequest {
    /// Execute a bounded ACPI WMAX method call (`\_SB.<PREFIX>.WMAX 0 <subcmd> {<arg1>,<arg2>,<arg3>,0x00}`).
    AcpiWmax {
        prefix: AcpiPrefix,
        subcmd: u8,
        arg1: u16,
        arg2: u16,
        arg3: u16,
    },
    /// Toggle Intel (`intel_pstate/no_turbo`) or AMD (`cpufreq/boost`) CPU Turbo Boost.
    SetCpuTurbo { enabled: bool },
    /// Activate and persist a 4-point custom temperature-to-fan-boost curve.
    ConfigureFanCurve { config: FanCurveConfig },
    /// Stop the active custom fan curve.
    StopFanCurve,
}

/// Structured RPC response returned by `UnixSocketDaemon` over `/run/awcc/awcc.sock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonRpcResponse {
    pub ok: bool,
    pub output: String,
    pub error: Option<String>,
}

impl DaemonRpcResponse {
    #[must_use]
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            error: None,
        }
    }

    #[must_use]
    pub fn err(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            ok: false,
            output: String::new(),
            error: Some(msg),
        }
    }

    /// Convert into the legacy string return convention expected by `CommandExecutor` callers.
    #[must_use]
    pub fn into_executor_string(self) -> String {
        if self.ok {
            self.output
        } else {
            let err = self
                .error
                .unwrap_or_else(|| "Error: Command not allowed".to_string());
            if err.starts_with("Error:") {
                err
            } else {
                format!("Error: {err}")
            }
        }
    }
}

impl DaemonRpcRequest {
    /// Validate domain bounds on the structured RPC request before execution.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::AcpiWmax { subcmd, .. } => {
                if !matches!(subcmd, 0x14 | 0x15 | 0x25) {
                    return Err(format!(
                        "Error: Disallowed ACPI WMAX subcommand 0x{subcmd:02x} (allowed: 0x14, 0x15, 0x25)"
                    ));
                }
                Ok(())
            }
            Self::SetCpuTurbo { .. } | Self::StopFanCurve => Ok(()),
            Self::ConfigureFanCurve { config } => {
                if !config.enabled {
                    return Err(
                        "Error: ConfigureFanCurve requires config.enabled == true (use StopFanCurve to disable)"
                            .to_string(),
                    );
                }
                if config.cpu_points.len() != 4 || config.gpu_points.len() != 4 {
                    return Err(
                        "Error: ConfigureFanCurve requires exactly 4 CPU and 4 GPU control points"
                            .to_string(),
                    );
                }
                if !(1..=15).contains(&config.hysteresis_c) {
                    return Err(format!(
                        "Error: Fan curve hysteresis {}°C is outside allowed 1..=15°C range",
                        config.hysteresis_c
                    ));
                }
                for pt in config.cpu_points.iter().chain(config.gpu_points.iter()) {
                    if !(20..=105).contains(&pt.temp_c) || pt.boost_pct > 100 {
                        return Err(format!(
                            "Error: Fan curve point ({}°C, {}%) is outside allowed (20..=105°C, 0..=100%) range",
                            pt.temp_c, pt.boost_pct
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    /// Parse a canonical command string produced by `LinuxAcpiTransport` or `FANCURVE` CLI into
    /// a structured [`DaemonRpcRequest`]. Returns `None` if the string does not match the strict
    /// schema.
    #[must_use]
    pub fn from_legacy_command(cmd: &str) -> Option<Self> {
        let trimmed = cmd.trim_end_matches(['\r', '\n']);
        if trimmed.contains('\n') || trimmed.contains('\r') || trimmed.contains('\0') {
            return None;
        }

        // 1. Intel / AMD CPU Turbo Boost toggles
        match trimmed {
            "echo 0 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo"
            | "echo 0 | pkexec tee /sys/devices/system/cpu/intel_pstate/no_turbo > /dev/null"
            | "echo 1 | sudo tee /sys/devices/system/cpu/cpufreq/boost"
            | "echo 1 | pkexec tee /sys/devices/system/cpu/cpufreq/boost > /dev/null" => {
                return Some(Self::SetCpuTurbo { enabled: true });
            }
            "echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo"
            | "echo 1 | pkexec tee /sys/devices/system/cpu/intel_pstate/no_turbo > /dev/null"
            | "echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost"
            | "echo 0 | pkexec tee /sys/devices/system/cpu/cpufreq/boost > /dev/null" => {
                return Some(Self::SetCpuTurbo { enabled: false });
            }
            _ => {}
        }

        // 2. Custom Fan Curve IPC commands
        if let Some(fc_cmd) = parse_fancurve_ipc_command(trimmed) {
            return Some(match fc_cmd {
                FanCurveIpcCommand::Stop => Self::StopFanCurve,
                FanCurveIpcCommand::Configure(config) => Self::ConfigureFanCurve { config },
            });
        }

        // 3. ACPI WMAX call
        const PREFIX_START: &str = "pkexec sh -c 'echo \"\\_SB.";
        const SUFFIX_END: &str = "\" > /proc/acpi/call && cat /proc/acpi/call'";

        let rest = trimmed.strip_prefix(PREFIX_START)?;
        let middle = rest.strip_suffix(SUFFIX_END)?;

        let (prefix, args_str) = [
            ("AMW0", AcpiPrefix::Amw0),
            ("AMW1", AcpiPrefix::Amw1),
            ("AMW3", AcpiPrefix::Amw3),
            ("AMWW", AcpiPrefix::Amww),
        ]
        .into_iter()
        .find_map(|(name, p)| {
            middle
                .strip_prefix(&format!("{name}.WMAX 0 "))
                .map(|s| (p, s))
        })?;

        let (subcmd_str, brace_part) = args_str.split_once(' ')?;
        let subcmd = match subcmd_str {
            "0x14" => 0x14,
            "0x15" => 0x15,
            "0x25" => 0x25,
            _ => return None,
        };

        let inner = brace_part.strip_prefix('{')?.strip_suffix('}')?;
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() != 4 || parts[3] != "0x00" {
            return None;
        }

        let arg1 = parse_hex_u16_literal(parts[0])?;
        let arg2 = parse_hex_u16_literal(parts[1])?;
        let arg3 = parse_hex_u16_literal(parts[2])?;

        Some(Self::AcpiWmax {
            prefix,
            subcmd,
            arg1,
            arg2,
            arg3,
        })
    }

    /// Format this structured request back into the canonical command representation used by
    /// `CommandExecutor` backends and audit logs.
    #[must_use]
    pub fn to_canonical_command(&self) -> String {
        match self {
            Self::AcpiWmax {
                prefix,
                subcmd,
                arg1,
                arg2,
                arg3,
            } => {
                LinuxAcpiTransport::format_acpi_shell_command(*prefix, *subcmd, *arg1, *arg2, *arg3)
            }
            Self::SetCpuTurbo { enabled } => {
                if Path::new("/sys/devices/system/cpu/cpufreq/boost").exists()
                    && !Path::new("/sys/devices/system/cpu/intel_pstate/no_turbo").exists()
                {
                    let write_val = u8::from(*enabled);
                    format!("echo {write_val} | sudo tee /sys/devices/system/cpu/cpufreq/boost")
                } else {
                    let no_turbo = u8::from(!*enabled);
                    format!(
                        "echo {no_turbo} | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo"
                    )
                }
            }
            Self::ConfigureFanCurve { config } => format_fancurve_ipc_command(config),
            Self::StopFanCurve => "FANCURVE STOP".to_string(),
        }
    }
}

/// Validate whether a command string maps to a valid [`DaemonRpcRequest`].
#[must_use]
pub fn is_command_allowed(cmd: &str) -> bool {
    DaemonRpcRequest::from_legacy_command(cmd)
        .and_then(|req| req.validate().ok())
        .is_some()
}

fn parse_hex_u16_literal(s: &str) -> Option<u16> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() > 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(hex, 16).ok()
}

/// Parsed representation of a `FANCURVE` daemon IPC command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanCurveIpcCommand {
    Stop,
    Configure(FanCurveConfig),
}

/// Parse a `FANCURVE STOP` or `FANCURVE <t0> <b0> <t1> <b1> <t2> <b2> <t3> <b3> [hyst]` command.
#[must_use]
pub fn parse_fancurve_ipc_command(cmd: &str) -> Option<FanCurveIpcCommand> {
    let trimmed = cmd.trim();
    if trimmed.eq_ignore_ascii_case("FANCURVE STOP") {
        return Some(FanCurveIpcCommand::Stop);
    }
    let rest = trimmed.strip_prefix("FANCURVE ")?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    if tokens.len() != 8 && tokens.len() != 9 {
        return None;
    }
    let mut vals = Vec::with_capacity(tokens.len());
    for tok in tokens {
        vals.push(tok.parse::<u16>().ok()?);
    }
    for i in 0..4 {
        let temp = vals[i * 2];
        let boost = vals[i * 2 + 1];
        if !(20..=105).contains(&temp) || boost > 100 {
            return None;
        }
    }
    let hysteresis_c = if vals.len() == 9 {
        vals[8].clamp(1, 15)
    } else {
        3
    };
    let mut pts = Vec::with_capacity(4);
    for i in 0..4 {
        #[allow(clippy::cast_possible_truncation)]
        pts.push(FanCurvePoint {
            temp_c: vals[i * 2],
            boost_pct: vals[i * 2 + 1] as u8,
        });
    }
    Some(FanCurveIpcCommand::Configure(FanCurveConfig {
        enabled: true,
        hysteresis_c,
        cpu_points: pts.clone(),
        gpu_points: pts,
    }))
}

/// Format a `FanCurveConfig` into the daemon IPC command string.
#[must_use]
pub fn format_fancurve_ipc_command(cfg: &FanCurveConfig) -> String {
    if !cfg.enabled || cfg.cpu_points.len() < 4 {
        return "FANCURVE STOP".to_string();
    }
    let p = &cfg.cpu_points;
    format!(
        "FANCURVE {} {} {} {} {} {} {} {} {}",
        p[0].temp_c,
        p[0].boost_pct,
        p[1].temp_c,
        p[1].boost_pct,
        p[2].temp_c,
        p[2].boost_pct,
        p[3].temp_c,
        p[3].boost_pct,
        cfg.hysteresis_c
    )
}

/// Attempt direct in-process kernel I/O for a structured [`DaemonRpcRequest`] without spawning
/// `pkexec` or `sh` whenever the current process has write permission to `/proc/acpi/call` or sysfs.
#[must_use]
pub fn try_direct_rpc_kernel_io(req: &DaemonRpcRequest) -> Option<String> {
    if req.validate().is_err() {
        return None;
    }
    match req {
        DaemonRpcRequest::AcpiWmax {
            prefix,
            subcmd,
            arg1,
            arg2,
            arg3,
        } => {
            let acpi_path = Path::new("/proc/acpi/call");
            if !acpi_path.exists() {
                return None;
            }
            let payload = format!(
                "{wmax} 0 {subcmd:#x} {{{arg1:#x},{arg2:#x},{arg3:#x},0x00}}\n",
                wmax = prefix.wmax_path()
            );
            if fs::write(acpi_path, payload).is_ok() {
                return fs::read_to_string(acpi_path).ok();
            }
            None
        }
        DaemonRpcRequest::SetCpuTurbo { enabled } => {
            let boost_path = Path::new("/sys/devices/system/cpu/cpufreq/boost");
            let no_turbo_path = Path::new("/sys/devices/system/cpu/intel_pstate/no_turbo");
            if boost_path.exists() && !no_turbo_path.exists() {
                let val = if *enabled { "1\n" } else { "0\n" };
                if fs::write(boost_path, val).is_ok() {
                    return Some(val.trim().to_string());
                }
            } else if no_turbo_path.exists() {
                let val = if *enabled { "0\n" } else { "1\n" };
                if fs::write(no_turbo_path, val).is_ok() {
                    return Some(val.trim().to_string());
                }
            }
            None
        }
        DaemonRpcRequest::ConfigureFanCurve { .. } | DaemonRpcRequest::StopFanCurve => None,
    }
}

/// Attempt a direct in-process write/read to `/proc/acpi/call` or CPU turbo sysfs
/// without spawning `pkexec` or `sh` whenever the current process has write permission.
#[must_use]
pub fn try_direct_kernel_io(cmd: &str) -> Option<String> {
    let req = DaemonRpcRequest::from_legacy_command(cmd)?;
    try_direct_rpc_kernel_io(&req)
}

/// Executor used inside the privileged daemon: direct kernel I/O only, never `pkexec` or a shell.
#[must_use]
pub fn daemon_command_executor() -> CommandExecutor {
    Arc::new(|cmd: &str| {
        try_direct_kernel_io(cmd).unwrap_or_else(|| {
            if Path::new("/proc/acpi/call").exists() {
                "Error: kernel write was refused (the daemon must run as root)".to_string()
            } else {
                "Error: /proc/acpi/call not found (load acpi_call kernel module)".to_string()
            }
        })
    })
}

/// How the daemon is wired (tests inject temporary paths and a permissive authorizer).
pub struct DaemonOptions {
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub state_dir: PathBuf,
    pub authorizer: Arc<dyn Authorizer>,
    pub build: BuildSummary,
    pub tick_interval: Duration,
    /// Require runtime directories/files to be owned by the current user (production).
    pub strict_owner: bool,
}

impl DaemonOptions {
    /// Production wiring under `/run/awcc`, `/var/lib/awcc` and `/etc/awcc`.
    #[must_use]
    pub fn system(build: BuildSummary) -> Self {
        Self {
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            lock_path: PathBuf::from(DEFAULT_LOCK_PATH),
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            authorizer: Arc::new(AllowlistAuthorizer::new(
                DEFAULT_AUTHORIZED_USERS_PATH,
                "/etc/passwd",
                true,
            )),
            build,
            tick_interval: DEFAULT_TICK_INTERVAL,
            strict_owner: true,
        }
    }
}

/// Why the daemon could not start.
#[derive(Debug)]
pub enum DaemonError {
    Ownership(OwnershipError),
    Device(String),
    Io(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ownership(e) => write!(f, "{e}"),
            Self::Device(m) => write!(f, "cannot resolve the device: {m}"),
            Self::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<OwnershipError> for DaemonError {
    fn from(e: OwnershipError) -> Self {
        Self::Ownership(e)
    }
}

/// A running daemon: exclusive lock, control thread, settings writer and IPC server.
pub struct Daemon {
    control: ControlHandle,
    server: IpcServer,
    sink: Arc<AsyncSettingsSink>,
    _lock: Arc<DaemonLock>,
}

impl Daemon {
    /// Take exclusive ownership, build the orchestrator with `make_orchestrator` (called only after
    /// the lock is held, so a second daemon never touches hardware), restore persisted settings
    /// and bind the socket.
    ///
    /// # Errors
    /// See [`DaemonError`].
    pub fn start(
        options: DaemonOptions,
        make_orchestrator: impl FnOnce(&DaemonOptions) -> Result<ThermalOrchestrator, String>,
    ) -> Result<Self, DaemonError> {
        let lock = Arc::new(DaemonLock::acquire(
            &options.lock_path,
            options.strict_owner,
        )?);
        let orchestrator = make_orchestrator(&options).map_err(DaemonError::Device)?;
        let (device_name, acpi_prefix) = orchestrator.device_identity();
        let store = SettingsStore::new(&options.state_dir);
        let load = store.load(&device_name, &acpi_prefix);
        let block_overwrite = matches!(load, SettingsLoad::Incompatible { .. })
            .then(|| "saved settings use a newer schema and are never overwritten".to_string());
        let sink = Arc::new(
            AsyncSettingsSink::spawn_owned(store, block_overwrite, Arc::clone(&lock))
                .map_err(|e| DaemonError::Io(format!("cannot start settings writer: {e}")))?,
        );
        let core = ControlCore::new(
            orchestrator,
            load,
            Arc::clone(&sink) as Arc<dyn SettingsSink>,
            options.tick_interval,
        );
        let control = ControlHandle::spawn_owned(core, Arc::clone(&lock))
            .map_err(|e| DaemonError::Io(format!("cannot start control thread: {e}")))?;
        let server = match IpcServer::bind(
            ServerConfig {
                socket_path: options.socket_path.clone(),
                authorizer: Arc::clone(&options.authorizer),
                build: options.build.clone(),
                device_name,
                acpi_prefix,
            },
            control.clone(),
            &lock,
        ) {
            Ok(server) => server,
            Err(error) => {
                // Startup may already have restored a saved curve. Stop that worker even when
                // the socket cannot be bound; its lock guard survives a shutdown timeout.
                let _ = control.stop(Duration::from_secs(8));
                return Err(error.into());
            }
        };
        tracing::info!(
            target: "awcc_service::daemon",
            category = "app",
            action = "start",
            outcome = "serving",
            code = "daemon_started",
            "daemon serving on {}",
            options.socket_path.display()
        );
        Ok(Self {
            control,
            server,
            sink,
            _lock: lock,
        })
    }

    /// Serve until `stop` is set, then stop gracefully. Returns the shutdown report.
    #[must_use]
    pub fn run(self, stop: &AtomicBool) -> String {
        self.server.serve(stop);
        let report = self
            .control
            .stop(Duration::from_secs(8))
            .unwrap_or_else(|| "shutdown: control thread did not finish within 8s".to_string());
        let durable = self.sink.flush(Duration::from_secs(2));
        format!(
            "{report}; settings {}",
            if durable {
                "saved"
            } else {
                "not confirmed saved"
            }
        )
    }

    #[must_use]
    pub fn control(&self) -> &ControlHandle {
        &self.control
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Also cover callers that construct a daemon without entering run(). The worker owns a
        // lock reference until its graceful shutdown completes, so this need not block Drop.
        let _ = self.control.stop(Duration::ZERO);
    }
}

/// Production orchestrator for the daemon: root-controlled device database, direct kernel I/O,
/// state under `/var/lib/awcc`.
fn system_orchestrator(options: &DaemonOptions) -> Result<ThermalOrchestrator, String> {
    let candidates: Vec<PathBuf> = TRUSTED_DATABASE_PATHS.iter().map(PathBuf::from).collect();
    let profile =
        resolve_trusted_host_device_profile(&candidates, true).map_err(|e| e.to_string())?;
    fs::create_dir_all(&options.state_dir).map_err(|e| e.to_string())?;
    let transport = LinuxAcpiTransport::new(
        profile.device_name.clone(),
        profile.acpi_prefix,
        HwmonProvider::default(),
        daemon_command_executor(),
    );
    let state = PosixStateStore::with_override_path(options.state_dir.join("prev_mode"));
    Ok(ThermalOrchestrator::new(
        profile,
        Box::new(transport),
        Box::new(state),
    ))
}

/// Run the privileged daemon until SIGTERM/SIGINT. The single production entry point.
///
/// # Errors
/// Returns the reason the daemon could not start (another daemon owns the lock, unsafe runtime
/// paths, device resolution failure, I/O errors).
pub fn run_system_daemon(build: BuildSummary) -> Result<String, DaemonError> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|e| DaemonError::Io(format!("cannot install signal handler: {e}")))?;
    }
    let daemon = Daemon::start(DaemonOptions::system(build), system_orchestrator)?;
    let report = daemon.run(&stop);
    tracing::info!(
        target: "awcc_service::daemon",
        category = "app",
        action = "stop",
        outcome = "done",
        code = "daemon_stopped",
        "{report}"
    );
    let _ = Ordering::SeqCst;
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_valid_wmax_and_turbo_and_fancurve_and_rejects_injections() {
        assert!(is_command_allowed(
            "pkexec sh -c 'echo \"\\_SB.AMWW.WMAX 0 0x14 {0xb,0x0,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'"
        ));
        assert!(is_command_allowed(
            "pkexec sh -c 'echo \"\\_SB.AMW3.WMAX 0 0x15 {0x3,0xa,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'"
        ));
        assert!(is_command_allowed(
            "echo 1 | pkexec tee /sys/devices/system/cpu/intel_pstate/no_turbo > /dev/null"
        ));
        assert!(is_command_allowed("FANCURVE 45 0 60 35 75 70 85 100 3"));
        assert!(is_command_allowed("FANCURVE STOP"));

        // Reject shell injection attempts
        assert!(!is_command_allowed("rm -rf /"));
        assert!(!is_command_allowed(
            "pkexec sh -c 'echo \"\\_SB.AMWW.WMAX 0 0x14 {0xb,0x0,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'; id"
        ));
        assert!(!is_command_allowed(
            "pkexec sh -c 'echo \"\\_SB.EVIL.WMAX 0 0x14 {0xb,0x0,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'"
        ));
    }

    #[test]
    fn structured_rpc_validates_bounds_and_rejects_unknown_fields_or_subcommands() {
        let valid_wmax = DaemonRpcRequest::AcpiWmax {
            prefix: AcpiPrefix::Amww,
            subcmd: 0x14,
            arg1: 0x0b,
            arg2: 0,
            arg3: 0,
        };
        assert!(valid_wmax.validate().is_ok());

        let invalid_subcmd = DaemonRpcRequest::AcpiWmax {
            prefix: AcpiPrefix::Amww,
            subcmd: 0x99,
            arg1: 0x0b,
            arg2: 0,
            arg3: 0,
        };
        assert!(invalid_subcmd.validate().is_err());

        let mut bad_curve = FanCurveConfig {
            enabled: true,
            ..FanCurveConfig::default()
        };
        bad_curve.cpu_points[0].temp_c = 120; // > 105°C
        let invalid_curve_req = DaemonRpcRequest::ConfigureFanCurve { config: bad_curve };
        assert!(invalid_curve_req.validate().is_err());

        // Unknown JSON fields are rejected by #[serde(deny_unknown_fields)]
        let unknown_field_json = r#"{"op":"acpi_wmax","prefix":"Amww","subcmd":20,"arg1":11,"arg2":0,"arg3":0,"shell":"id"}"#;
        assert!(serde_json::from_str::<DaemonRpcRequest>(unknown_field_json).is_err());
    }

    #[test]
    fn daemon_executor_never_shells_out_for_a_refused_write() {
        // A write the daemon cannot perform must surface as an error string, never run `sh`.
        let out =
            daemon_command_executor()("echo 1 | pkexec tee /nonexistent/awcc-test > /dev/null");
        assert!(out.starts_with("Error:"), "{out}");
    }
}
