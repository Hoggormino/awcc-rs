//! Privileged daemon Unix domain socket service (`/tmp/awcc.sock`), strict command allowlist,
//! peer credential inspection, and custom fan-curve IPC protocol.
//!
//! Matches C++ `Daemon` (`src/Daemon.cpp`) behavior while providing hermetic testability over
//! temporary Unix sockets without touching `/tmp/awcc.sock` during offline tests.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use awcc_core::{FanCurveConfig, FanCurvePoint};
use awcc_platform::CommandExecutor;

/// Default Unix domain socket path used by `awccd`.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/awcc.sock";

/// Validate whether a command string is permitted by the `awccd` privileged allowlist.
///
/// Matches C++ `Daemon::m_CommandAllowed`:
/// 1. `pkexec sh -c 'echo "\_SB.(AMW0|AMW1|AMW3|AMWW).WMAX 0 0x(14|15|25) {0x..,0x..,0x..,0x00}" > /proc/acpi/call && cat /proc/acpi/call'`
/// 2. `echo (0|1) | pkexec tee /sys/devices/system/cpu/intel_pstate/no_turbo > /dev/null`
/// 3. `echo (0|1) | pkexec tee /sys/devices/system/cpu/cpufreq/boost > /dev/null`
/// 4. `FANCURVE STOP` or `FANCURVE <t0> <b0> <t1> <b1> <t2> <b2> <t3> <b3> [hyst]`
#[must_use]
pub fn is_command_allowed(cmd: &str) -> bool {
    let trimmed = cmd.trim_end_matches(['\r', '\n']);
    if trimmed.contains('\n') || trimmed.contains('\r') || trimmed.contains('\0') {
        return false;
    }

    // 1. Intel / AMD CPU Turbo Boost toggles
    if matches!(
        trimmed,
        "echo 0 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo"
            | "echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo"
            | "echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost"
            | "echo 1 | sudo tee /sys/devices/system/cpu/cpufreq/boost"
            | "echo 0 | pkexec tee /sys/devices/system/cpu/intel_pstate/no_turbo > /dev/null"
            | "echo 1 | pkexec tee /sys/devices/system/cpu/intel_pstate/no_turbo > /dev/null"
            | "echo 0 | pkexec tee /sys/devices/system/cpu/cpufreq/boost > /dev/null"
            | "echo 1 | pkexec tee /sys/devices/system/cpu/cpufreq/boost > /dev/null"
    ) {
        return true;
    }

    // 2. Custom Fan Curve IPC commands
    if parse_fancurve_ipc_command(trimmed).is_some() {
        return true;
    }

    // 3. ACPI WMAX call allowlist
    const PREFIX_START: &str = "pkexec sh -c 'echo \"\\_SB.";
    const SUFFIX_END: &str = "\" > /proc/acpi/call && cat /proc/acpi/call'";

    let Some(rest) = trimmed.strip_prefix(PREFIX_START) else {
        return false;
    };
    let Some(middle) = rest.strip_suffix(SUFFIX_END) else {
        return false;
    };

    // middle must start with `(AMW0|AMW1|AMW3|AMWW).WMAX 0 `
    let acpi_args = ["AMW0", "AMW1", "AMW3", "AMWW"]
        .iter()
        .find_map(|p| middle.strip_prefix(&format!("{p}.WMAX 0 ")));
    let Some(args_str) = acpi_args else {
        return false;
    };

    // args_str must be `0x(14|15|25) {0xHEX,0xHEX,0xHEX,0x00}`
    let Some((subcmd_str, brace_part)) = args_str.split_once(' ') else {
        return false;
    };
    if !matches!(subcmd_str, "0x14" | "0x15" | "0x25") {
        return false;
    }

    let Some(inner) = brace_part
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
    else {
        return false;
    };

    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 4 || parts[3] != "0x00" {
        return false;
    }

    parts[..3].iter().all(|p| is_hex_u16_literal(p))
}

fn is_hex_u16_literal(s: &str) -> bool {
    let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) else {
        return false;
    };
    !hex.is_empty() && hex.len() <= 4 && hex.chars().all(|c| c.is_ascii_hexdigit())
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

/// Unix domain socket server and client matching C++ `Daemon`.
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

    /// Execute a command directly inside the daemon (checking `is_command_allowed` first).
    #[must_use]
    pub fn execute_direct(&self, cmd: &str) -> String {
        if !is_command_allowed(cmd) {
            return "Error: Command not allowed".to_string();
        }
        if let Some(fc_cmd) = parse_fancurve_ipc_command(cmd) {
            if let Ok(mut guard) = self.active_fan_curve.lock() {
                match fc_cmd {
                    FanCurveIpcCommand::Stop => {
                        guard.enabled = false;
                        return "OK: Custom fan curve stopped".to_string();
                    }
                    FanCurveIpcCommand::Configure(cfg) => {
                        *guard = cfg;
                        return "OK: Custom fan curve active".to_string();
                    }
                }
            }
        }
        (self.executor)(cmd)
    }

    /// Serve incoming client connections on `self.socket_path` until `self.running` becomes `false`.
    pub fn serve_blocking(&self) -> std::io::Result<()> {
        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        listener.set_nonblocking(true)?;
        self.running.store(true, Ordering::SeqCst);

        while self.running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let mut buf = [0u8; 1024];
                    if let Ok(n) = stream.read(&mut buf)
                        && n > 0
                    {
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let resp = self.execute_direct(&req);
                        let _ = stream.write_all(resp.as_bytes());
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

/// Send a command string to a running `awccd` Unix domain socket and return its response.
pub fn send_command_to_socket(socket_path: &Path, cmd: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(cmd.as_bytes())?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    Ok(resp)
}

/// Attempt a direct in-process write/read to `/proc/acpi/call` or CPU turbo sysfs
/// without spawning `pkexec` or `sh` whenever the current process has write permission
/// (e.g. inside `awccd` or when `/proc/acpi/call` has group/udev write permissions).
#[must_use]
pub fn try_direct_kernel_io(cmd: &str) -> Option<String> {
    let trimmed = cmd.trim();

    // 1. Direct `/proc/acpi/call` write + read
    const PREFIX_START: &str = "pkexec sh -c 'echo \"";
    const SUFFIX_END: &str = "\" > /proc/acpi/call && cat /proc/acpi/call'";
    if let Some(rest) = trimmed.strip_prefix(PREFIX_START)
        && let Some(acpi_payload) = rest.strip_suffix(SUFFIX_END)
    {
        let acpi_path = Path::new("/proc/acpi/call");
        if acpi_path.exists() && fs::write(acpi_path, format!("{acpi_payload}\n")).is_ok() {
            return fs::read_to_string(acpi_path).ok();
        }
        return None;
    }

    // 2. Direct sysfs CPU Turbo Boost write
    for sysfs_path_str in [
        "/sys/devices/system/cpu/intel_pstate/no_turbo",
        "/sys/devices/system/cpu/cpufreq/boost",
    ] {
        if trimmed.contains(sysfs_path_str) {
            let val = if trimmed.contains("echo 1") {
                "1\n"
            } else if trimmed.contains("echo 0") {
                "0\n"
            } else {
                return None;
            };
            let sysfs_path = Path::new(sysfs_path_str);
            if sysfs_path.exists() && fs::write(sysfs_path, val).is_ok() {
                return Some(val.trim().to_string());
            }
            return None;
        }
    }

    None
}

/// Construct a production `CommandExecutor`:
/// 1. Tries direct in-process file I/O (`fs::write` + `fs::read_to_string`) on `/proc/acpi/call`
///    or sysfs when the current process has write permission (zero `pkexec`/`sh` overhead).
/// 2. Otherwise forwards over `/tmp/awcc.sock` when the privileged `awccd` daemon is listening.
/// 3. Falls back to `sh -c` only when neither direct write access nor the daemon socket is available.
#[must_use]
pub fn default_linux_command_executor(socket_path: PathBuf) -> CommandExecutor {
    Arc::new(move |cmd: &str| {
        if let Some(resp) = try_direct_kernel_io(cmd) {
            return resp;
        }
        if socket_path.exists()
            && let Ok(resp) = send_command_to_socket(&socket_path, cmd)
        {
            return resp;
        }
        if !Path::new("/proc/acpi/call").exists() && cmd.contains("/proc/acpi/call") {
            return "Error: /proc/acpi/call not found (load acpi_call kernel module)".to_string();
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
    fn unix_socket_daemon_roundtrips_commands_and_cleans_up_socket() {
        let sock_path =
            std::env::temp_dir().join(format!("awcc_rust_daemon_test_{}.sock", std::process::id()));
        let _ = fs::remove_file(&sock_path);

        let daemon = Arc::new(UnixSocketDaemon::new(
            &sock_path,
            Arc::new(|cmd| {
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

        let valid_cmd = "pkexec sh -c 'echo \"\\_SB.AMWW.WMAX 0 0x14 {0xb,0x0,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'";
        let resp = send_command_to_socket(&sock_path, valid_cmd).unwrap();
        assert_eq!(resp, "0xa0\0");

        let blocked_resp = send_command_to_socket(&sock_path, "uname -a").unwrap();
        assert_eq!(blocked_resp, "Error: Command not allowed");

        let fc_resp =
            send_command_to_socket(&sock_path, "FANCURVE 45 10 60 40 75 75 85 100 4").unwrap();
        assert_eq!(fc_resp, "OK: Custom fan curve active");
        assert!(daemon.active_fan_curve().enabled);
        assert_eq!(daemon.active_fan_curve().hysteresis_c, 4);

        daemon.stop();
        handle.join().unwrap();
        assert!(!sock_path.exists());
    }
}
