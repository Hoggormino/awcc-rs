//! Privileged daemon Unix domain socket service (`/run/awcc/awcc.sock`), structured JSON RPC
//! protocol (`DaemonRpcRequest` / `DaemonRpcResponse`), direct kernel I/O, and custom
//! fan-curve IPC.
//!
//! Replaces the legacy C++ `pkexec`-string regex allowlist over `/run/awcc/awcc.sock` with a
//! strictly typed, schema-validated JSON RPC protocol (`#[serde(deny_unknown_fields)]`)
//! while preserving hermetic offline testability over temporary Unix sockets.

use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use awcc_core::{AcpiPrefix, FanCurveConfig, FanCurvePoint};
use awcc_platform::{CommandExecutor, LinuxAcpiTransport};
use serde::{Deserialize, Serialize};

/// Default Unix domain socket path used by `awccd`.
pub const DEFAULT_SOCKET_PATH: &str = "/run/awcc/awcc.sock";

/// Environment variable that re-enables the per-call `pkexec` fallback for ordinary processes.
///
/// It is off by default: polling the hardware through `pkexec` raises a polkit prompt on every
/// refresh. A privileged `awccd` daemon is the supported route for unprivileged clients.
pub const ALLOW_PKEXEC_ENV: &str = "AWCC_ALLOW_PKEXEC";

/// Message returned when a privileged operation is needed and no privileged route exists.
pub const DAEMON_UNAVAILABLE_MESSAGE: &str = "Error: privileged service unavailable (start it with `sudo systemctl enable --now awccd.service`, or run as root)";

/// Maximum wire request size in bytes accepted by `UnixSocketDaemon` (`4 KiB`).
pub const MAX_RPC_WIRE_BYTES: usize = 4096;

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

/// Unix domain socket server and client serving structured [`DaemonRpcRequest`] messages.
pub struct UnixSocketDaemon {
    socket_path: PathBuf,
    running: Arc<AtomicBool>,
    executor: CommandExecutor,
    active_fan_curve: Arc<Mutex<FanCurveConfig>>,
}

impl UnixSocketDaemon {
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>, executor: CommandExecutor) -> Self {
        Self {
            socket_path: socket_path.into(),
            running: Arc::new(AtomicBool::new(false)),
            executor,
            active_fan_curve: Arc::new(Mutex::new(FanCurveConfig::default())),
        }
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[must_use]
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.running)
    }

    #[must_use]
    pub fn active_fan_curve(&self) -> FanCurveConfig {
        self.active_fan_curve
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Validate and execute a structured [`DaemonRpcRequest`] directly inside the daemon.
    #[must_use]
    pub fn execute_rpc(&self, req: &DaemonRpcRequest) -> DaemonRpcResponse {
        if let Err(err) = req.validate() {
            return DaemonRpcResponse::err(err);
        }
        match req {
            DaemonRpcRequest::StopFanCurve => {
                if let Ok(mut guard) = self.active_fan_curve.lock() {
                    guard.enabled = false;
                }
                DaemonRpcResponse::ok("OK: Custom fan curve stopped")
            }
            DaemonRpcRequest::ConfigureFanCurve { config } => {
                if let Ok(mut guard) = self.active_fan_curve.lock() {
                    *guard = config.clone();
                }
                DaemonRpcResponse::ok("OK: Custom fan curve active")
            }
            DaemonRpcRequest::AcpiWmax { .. } | DaemonRpcRequest::SetCpuTurbo { .. } => {
                if let Some(direct_out) = try_direct_rpc_kernel_io(req) {
                    return DaemonRpcResponse::ok(direct_out);
                }
                let canonical = req.to_canonical_command();
                let out = (self.executor)(&canonical);
                if out.starts_with("Error:") {
                    DaemonRpcResponse::err(out)
                } else {
                    DaemonRpcResponse::ok(out)
                }
            }
        }
    }

    /// Decode a wire payload as a structured JSON [`DaemonRpcRequest`] and return a serialized
    /// [`DaemonRpcResponse`]. Raw legacy shell strings are rejected over the wire.
    #[must_use]
    pub fn handle_wire_payload(&self, raw_payload: &str) -> DaemonRpcResponse {
        let trimmed = raw_payload.trim();
        let Ok(req) = serde_json::from_str::<DaemonRpcRequest>(trimmed) else {
            return DaemonRpcResponse::err(
                "Error: Command not allowed (structured DaemonRpcRequest JSON required)",
            );
        };
        self.execute_rpc(&req)
    }

    /// Execute a canonical command string or JSON RPC request in-process by first converting
    /// into a structured [`DaemonRpcRequest`].
    #[must_use]
    pub fn execute_direct(&self, cmd: &str) -> String {
        let trimmed = cmd.trim();
        if trimmed.starts_with('{') {
            return self.handle_wire_payload(trimmed).into_executor_string();
        }
        let Some(req) = DaemonRpcRequest::from_legacy_command(cmd) else {
            return "Error: Command not allowed".to_string();
        };
        self.execute_rpc(&req).into_executor_string()
    }

    /// Serve incoming client connections on `self.socket_path` until `self.running` becomes `false`.
    ///
    /// Only structured JSON [`DaemonRpcRequest`] payloads are accepted on the socket; raw shell
    /// command strings are rejected without ever reaching the command executor.
    pub fn serve_blocking(&self) -> std::io::Result<()> {
        if let Some(parent) = self.socket_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        // Any local user may talk to the daemon; safety comes from the typed, validated RPC
        // allowlist (no shell strings cross the socket), matching the legacy daemon's access model.
        fs::set_permissions(&self.socket_path, fs::Permissions::from_mode(0o666))?;
        listener.set_nonblocking(true)?;
        self.running.store(true, Ordering::SeqCst);

        while self.running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let mut buf = [0u8; MAX_RPC_WIRE_BYTES];
                    if let Ok(n) = stream.read(&mut buf)
                        && n > 0
                    {
                        let req_str = String::from_utf8_lossy(&buf[..n]);
                        let resp = self.handle_wire_payload(&req_str);
                        if let Ok(mut encoded) = serde_json::to_vec(&resp) {
                            encoded.push(b'\n');
                            let _ = stream.write_all(&encoded);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(_) => break,
            }
        }

        let _ = fs::remove_file(&self.socket_path);
        Ok(())
    }

    /// Stop the daemon loop and unlink the Unix socket file.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for UnixSocketDaemon {
    fn drop(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            self.stop();
        }
    }
}

/// Send a structured [`DaemonRpcRequest`] over a Unix domain socket and decode the
/// [`DaemonRpcResponse`].
pub fn send_rpc_to_socket(
    socket_path: &Path,
    req: &DaemonRpcRequest,
) -> std::io::Result<DaemonRpcResponse> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut payload = serde_json::to_vec(req)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    let _ = stream.shutdown(Shutdown::Write);
    let mut resp_str = String::new();
    stream.read_to_string(&mut resp_str)?;
    serde_json::from_str::<DaemonRpcResponse>(resp_str.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Convert a canonical command into a structured [`DaemonRpcRequest`], send it over the
/// Unix domain socket as JSON RPC, and return the executor response string.
///
/// Unallowed or arbitrary shell command strings are rejected client-side before touching the socket.
pub fn send_command_to_socket(socket_path: &Path, cmd: &str) -> std::io::Result<String> {
    let Some(req) = DaemonRpcRequest::from_legacy_command(cmd) else {
        return Ok("Error: Command not allowed".to_string());
    };
    let resp = send_rpc_to_socket(socket_path, &req)?;
    Ok(resp.into_executor_string())
}

/// Construct a production `CommandExecutor`:
/// 1. Tries direct in-process file I/O (`fs::write` + `fs::read_to_string`) on `/proc/acpi/call`
///    or sysfs when the current process has write permission (zero `pkexec`/`sh` overhead).
/// 2. Otherwise translates the operation into a structured [`DaemonRpcRequest`] and sends it over
///    `/run/awcc/awcc.sock` when the privileged `awccd` daemon is listening.
/// 3. Falls back to a per-call `pkexec` only when `AWCC_ALLOW_PKEXEC=1`; otherwise reports that the
///    privileged service is unavailable instead of raising a polkit prompt on every call.
#[must_use]
pub fn default_linux_command_executor(socket_path: PathBuf) -> CommandExecutor {
    Arc::new(move |cmd: &str| {
        if let Some(resp) = try_direct_kernel_io(cmd) {
            return resp;
        }
        if socket_path.exists()
            && let Some(req) = DaemonRpcRequest::from_legacy_command(cmd)
            && let Ok(resp) = send_rpc_to_socket(&socket_path, &req)
        {
            return resp.into_executor_string();
        }
        if !Path::new("/proc/acpi/call").exists() && cmd.contains("/proc/acpi/call") {
            return "Error: /proc/acpi/call not found (load acpi_call kernel module)".to_string();
        }
        if cmd.contains("pkexec") && !pkexec_fallback_allowed() {
            return DAEMON_UNAVAILABLE_MESSAGE.to_string();
        }
        match std::process::Command::new("sh").arg("-c").arg(cmd).output() {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            Ok(out) => format!(
                "Error: command exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => format!("Error: failed to spawn shell command: {e}"),
        }
    })
}

fn pkexec_fallback_allowed() -> bool {
    std::env::var(ALLOW_PKEXEC_ENV).is_ok_and(|v| v == "1")
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

/// Run the privileged daemon until its running flag is cleared. Blocks the calling thread.
///
/// # Errors
/// Returns the I/O error if the socket directory or socket cannot be created.
pub fn run_daemon(socket_path: impl Into<PathBuf>) -> std::io::Result<()> {
    UnixSocketDaemon::new(socket_path, daemon_command_executor()).serve_blocking()
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
    fn daemon_creates_its_socket_directory_and_makes_the_socket_reachable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("awcc_rust_run_dir_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sock_path = dir.join("nested").join("awcc.sock");
        let daemon = Arc::new(UnixSocketDaemon::new(
            &sock_path,
            Arc::new(|_| "0x0\0".to_string()),
        ));
        let running = daemon.running_flag();
        let worker = Arc::clone(&daemon);
        let handle = std::thread::spawn(move || worker.serve_blocking());
        for _ in 0..100 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mode = fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o666);
        running.store(false, Ordering::SeqCst);
        handle.join().unwrap().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_executor_never_shells_out_for_a_refused_write() {
        // A write the daemon cannot perform must surface as an error string, never run `sh`.
        let out =
            daemon_command_executor()("echo 1 | pkexec tee /nonexistent/awcc-test > /dev/null");
        assert!(out.starts_with("Error:"), "{out}");
    }

    #[test]
    fn client_executor_does_not_raise_polkit_prompts_without_a_daemon() {
        if pkexec_fallback_allowed() {
            return;
        }
        let missing = std::env::temp_dir().join("awcc_rust_no_such_daemon.sock");
        let _ = fs::remove_file(&missing);
        // A turbo write is not served by direct I/O in an unprivileged test process and does not
        // mention /proc/acpi/call, so it reaches the former `pkexec` fallback.
        let out = default_linux_command_executor(missing)(
            "echo 1 | pkexec tee /sys/devices/system/cpu/cpufreq/boost > /dev/null",
        );
        let privileged = std::fs::OpenOptions::new()
            .write(true)
            .open("/sys/devices/system/cpu/cpufreq/boost")
            .is_ok();
        if !privileged {
            assert_eq!(out, DAEMON_UNAVAILABLE_MESSAGE);
        }
    }

    #[test]
    fn unix_socket_daemon_roundtrips_commands_and_cleans_up_socket() {
        let sock_path =
            std::env::temp_dir().join(format!("awcc_rust_daemon_test_{}.sock", std::process::id()));
        let _ = fs::remove_file(&sock_path);

        let executor_calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let executor_calls_clone = Arc::clone(&executor_calls);

        let daemon = Arc::new(UnixSocketDaemon::new(
            &sock_path,
            Arc::new(move |cmd| {
                executor_calls_clone.lock().unwrap().push(cmd.to_string());
                if cmd.contains("0x14 {0xb") {
                    "0xa0\0".to_string()
                } else {
                    "0x0\0".to_string()
                }
            }),
        ));

        let d_thread = Arc::clone(&daemon);
        let handle = std::thread::spawn(move || {
            d_thread.serve_blocking().unwrap();
        });

        // Wait until socket appears
        for _ in 0..40 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(sock_path.exists());

        // 1. Direct structured JSON RPC call over the socket
        let rpc_resp = send_rpc_to_socket(
            &sock_path,
            &DaemonRpcRequest::AcpiWmax {
                prefix: AcpiPrefix::Amww,
                subcmd: 0x14,
                arg1: 0x0b,
                arg2: 0,
                arg3: 0,
            },
        )
        .unwrap();
        assert!(rpc_resp.ok);
        assert_eq!(rpc_resp.output, "0xa0\0");

        // 2. Legacy command helper converts to structured RPC before sending over socket
        let valid_cmd = "pkexec sh -c 'echo \"\\_SB.AMWW.WMAX 0 0x14 {0xb,0x0,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'";
        let resp = send_command_to_socket(&sock_path, valid_cmd).unwrap();
        assert_eq!(resp, "0xa0\0");

        let blocked_resp = send_command_to_socket(&sock_path, "uname -a").unwrap();
        assert_eq!(blocked_resp, "Error: Command not allowed");

        // 3. Raw legacy shell string sent directly over the Unix socket wire is rejected!
        let calls_before_raw = executor_calls.lock().unwrap().len();
        {
            let mut raw_stream = UnixStream::connect(&sock_path).unwrap();
            raw_stream.write_all(valid_cmd.as_bytes()).unwrap();
            let _ = raw_stream.shutdown(Shutdown::Write);
            let mut wire_reply = String::new();
            raw_stream.read_to_string(&mut wire_reply).unwrap();
            let decoded: DaemonRpcResponse = serde_json::from_str(wire_reply.trim()).unwrap();
            assert!(!decoded.ok);
            assert!(
                decoded
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("structured DaemonRpcRequest JSON required")
            );
        }
        assert_eq!(
            executor_calls.lock().unwrap().len(),
            calls_before_raw,
            "raw shell string on socket wire must never reach the executor"
        );

        // 4. Fan curve configuration via structured RPC
        let fc_resp =
            send_command_to_socket(&sock_path, "FANCURVE 45 10 60 40 75 75 85 100 4").unwrap();
        assert_eq!(fc_resp, "OK: Custom fan curve active");
        assert!(daemon.active_fan_curve().enabled);
        assert_eq!(daemon.active_fan_curve().hysteresis_c, 4);

        let stop_resp = send_rpc_to_socket(&sock_path, &DaemonRpcRequest::StopFanCurve).unwrap();
        assert!(stop_resp.ok);
        assert!(!daemon.active_fan_curve().enabled);

        daemon.stop();
        handle.join().unwrap();
        assert!(!sock_path.exists());
    }
}
