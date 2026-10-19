//! Platform hardware transport traits, Linux ACPI/hwmon/state-store backends,
//! and deterministic Alienware m15 R6 (`AMWW`) simulation backend.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use awcc_core::{
    AcpiPrefix, CoreError, CpuVendor, Database, DeviceProfile, SensorReading, SimScenario,
    TelemetrySnapshot, ThermalCodeTable, ThermalMode, parse_acpi_output,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod usb_elc;

/// Errors produced at the platform / ACPI transport boundary.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("ACPI call `{call}` returned 0xFFFFFFFF (hardware rejected operation)")]
    HardwareRejected { call: String },
    #[error("hardware transport unavailable: {0}")]
    TransportUnavailable(String),
}

/// Hard cap on per-transport write audit buffers so long-running sessions stay bounded in memory.
pub const MAX_TRANSPORT_AUDIT_ENTRIES: usize = 256;

/// Audit log entry for every hardware/simulated write command issued.
///
/// Passive operations (`query_raw_thermal_mode`, `query_telemetry`, `device-info`) never append
/// to the write audit log, allowing tests and the Diagnostics UI to prove zero unsolicited writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteAuditEntry {
    pub sequence: usize,
    pub command: String,
    /// Result of the *outgoing* write (ACPI return code, sysfs result, or USB OUT transfer).
    pub outcome: String,
    /// Whether the outgoing write itself succeeded. For USB this says nothing about whether the
    /// controller acknowledged it: see `ack`.
    pub succeeded: bool,
    /// Result of the optional device acknowledgement read that follows a USB OUT report,
    /// recorded separately from the outgoing transfer. `NotApplicable` for ACPI/sysfs writes.
    #[serde(default)]
    pub ack: AckStatus,
    /// Correlated operation ID assigned by `ThermalOrchestrator::run_op`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_id: Option<u64>,
    /// Optional 33-byte USB input report hex from the acknowledgement phase (included in support
    /// exports only when `--trace-raw-usb` / `AWCC_TRACE_RAW_USB=1` is opted into).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_usb_response_hex: Option<String>,
}

impl WriteAuditEntry {
    /// An entry for a write that has no acknowledgement phase (ACPI, sysfs).
    #[must_use]
    pub fn new(
        sequence: usize,
        command: impl Into<String>,
        outcome: impl Into<String>,
        succeeded: bool,
    ) -> Self {
        Self {
            sequence,
            command: command.into(),
            outcome: outcome.into(),
            succeeded,
            ack: AckStatus::NotApplicable,
            op_id: None,
            raw_usb_response_hex: None,
        }
    }
}

/// Outcome of the device acknowledgement read that follows an outgoing USB HID report.
///
/// The legacy C++ `LightFX` never reads an acknowledgement after a set command, and no firmware
/// success/failure codes are known for these reports, so the acknowledgement is *optional*
/// evidence: a missing or failed read never fails the transfer, and a received report's content
/// is not interpreted as a success code. It is recorded so that "the bytes were sent" and "the
/// device answered" stay distinct.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckStatus {
    /// No acknowledgement phase applies (ACPI/sysfs writes, query reports).
    #[default]
    NotApplicable,
    /// A full 33-byte input report was read after the transfer (content not interpreted).
    Received,
    /// A read completed but returned fewer bytes than a full report.
    Incomplete { bytes: usize },
    /// The acknowledgement read failed (timeout, I/O error, ...).
    Failed { reason: String },
}

impl AckStatus {
    /// `true` when an acknowledgement was expected but did not arrive intact.
    #[must_use]
    pub const fn is_problem(&self) -> bool {
        matches!(self, Self::Incomplete { .. } | Self::Failed { .. })
    }

    /// Short human description, e.g. `ack received` or `ack read failed: timed out`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NotApplicable => "no ack phase".to_string(),
            Self::Received => "ack received".to_string(),
            Self::Incomplete { bytes } => format!("ack incomplete ({bytes}/33 bytes)"),
            Self::Failed { reason } => format!("ack read failed: {reason}"),
        }
    }
}

/// Persistence boundary for pre-G-Mode thermal state (`/etc/awcc/prev_mode`, `/run/awcc/prev_mode`,
/// or `$XDG_STATE_HOME/awcc/prev_mode` in production).
pub trait StateStorage: Send {
    fn load_pre_gmode(&self) -> Option<ThermalMode>;
    fn save_pre_gmode(&mut self, mode: ThermalMode);
}

/// In-memory `StateStorage` implementation for simulation and offline testing.
#[derive(Debug, Default, Clone)]
pub struct MemoryStateStorage {
    saved_mode: Option<ThermalMode>,
}

impl MemoryStateStorage {
    #[must_use]
    pub const fn new(initial: Option<ThermalMode>) -> Self {
        Self {
            saved_mode: initial,
        }
    }
}

impl StateStorage for MemoryStateStorage {
    fn load_pre_gmode(&self) -> Option<ThermalMode> {
        self.saved_mode
    }

    fn save_pre_gmode(&mut self, mode: ThermalMode) {
        if mode != ThermalMode::GMode && mode != ThermalMode::Manual {
            self.saved_mode = Some(mode);
        }
    }
}

/// POSIX file-backed `StateStorage` matching C++ `Thermals::loadPreviousMode` / `savePreviousMode`.
///
/// Enforces `O_NOFOLLOW | O_CLOEXEC` and verifies `is_file()` (`S_ISREG`) via `fstat` metadata
/// before reading or writing so symlink/FIFO attacks are rejected fail-closed.
#[derive(Debug, Clone)]
pub struct PosixStateStore {
    override_path: Option<PathBuf>,
    cached_mode: Option<ThermalMode>,
}

impl Default for PosixStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PosixStateStore {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            override_path: None,
            cached_mode: None,
        }
    }

    #[must_use]
    pub fn with_override_path(path: impl Into<PathBuf>) -> Self {
        Self {
            override_path: Some(path.into()),
            cached_mode: None,
        }
    }

    fn user_state_base() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
            && xdg.starts_with('/')
        {
            Some(PathBuf::from(xdg).join("awcc"))
        } else if let Ok(home) = std::env::var("HOME")
            && home.starts_with('/')
        {
            Some(PathBuf::from(home).join(".local/state/awcc"))
        } else {
            None
        }
    }

    /// Resolve the state file path following C++ `Thermals::resolveStateFilePath(for_write)`.
    #[must_use]
    pub fn resolve_path(&self, for_write: bool) -> Option<PathBuf> {
        if let Some(path) = &self.override_path {
            return Some(path.clone());
        }

        let etc_dir = Path::new("/etc/awcc");
        let etc_file = Path::new("/etc/awcc/prev_mode");
        let run_dir = Path::new("/run/awcc");
        let run_file = Path::new("/run/awcc/prev_mode");

        if can_write_target(etc_file, etc_dir) {
            return Some(etc_file.to_path_buf());
        }
        if can_write_target(run_file, run_dir) {
            return Some(run_file.to_path_buf());
        }

        if let Some(base) = Self::user_state_base() {
            let user_file = base.join("prev_mode");
            if for_write {
                if !base.exists() {
                    let _ = fs::create_dir_all(&base);
                }
                return Some(user_file);
            }
            if user_file.exists() && is_readable(&user_file) {
                return Some(user_file);
            }
        }

        if !for_write && etc_file.exists() && is_readable(etc_file) {
            return Some(etc_file.to_path_buf());
        }
        if !for_write && run_file.exists() && is_readable(run_file) {
            return Some(run_file.to_path_buf());
        }

        Self::user_state_base().map(|base| base.join("prev_mode"))
    }
}

fn is_readable(path: &Path) -> bool {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW (0o400000) | O_CLOEXEC (0o2000000) on Linux x86_64
        opts.custom_flags(0o400_000 | 0o2_000_000);
    }
    opts.open(path)
        .and_then(|f| f.metadata().map(|m| m.is_file()))
        .unwrap_or(false)
}

fn can_write_target(file: &Path, dir: &Path) -> bool {
    if file.exists() {
        let mut opts = OpenOptions::new();
        opts.write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(0o400_000 | 0o2_000_000);
        }
        return opts
            .open(file)
            .and_then(|f| f.metadata().map(|m| m.is_file()))
            .unwrap_or(false);
    }
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".awcc_write_probe_{}", std::process::id()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    if opts.open(&probe).is_ok() {
        let _ = fs::remove_file(&probe);
        true
    } else {
        false
    }
}

impl StateStorage for PosixStateStore {
    fn load_pre_gmode(&self) -> Option<ThermalMode> {
        let path = self.resolve_path(false)?;
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(0o400_000 | 0o2_000_000);
        }
        let Ok(mut file) = opts.open(&path) else {
            return self.cached_mode;
        };
        let Ok(meta) = file.metadata() else {
            return self.cached_mode;
        };
        if !meta.is_file() {
            return self.cached_mode;
        }
        let mut buf = [0_u8; 64];
        let Ok(n) = file.read(&mut buf) else {
            return self.cached_mode;
        };
        if n == 0 {
            return self.cached_mode;
        }
        let text = std::str::from_utf8(&buf[..n]).ok()?.trim();
        let raw_discriminant: i64 = text.parse().ok()?;
        let mode = ThermalMode::from_c_enum_discriminant(raw_discriminant)?;
        if mode == ThermalMode::GMode || mode == ThermalMode::Manual {
            return self.cached_mode;
        }
        Some(mode)
    }

    fn save_pre_gmode(&mut self, mode: ThermalMode) {
        if mode == ThermalMode::GMode || mode == ThermalMode::Manual {
            return;
        }
        self.cached_mode = Some(mode);
        let Some(path) = self.resolve_path(true) else {
            return;
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            let _ = fs::create_dir_all(parent);
        }
        // Reject pre-existing symlinks or non-regular files before opening with O_TRUNC.
        if let Ok(sym_meta) = fs::symlink_metadata(&path)
            && !sym_meta.is_file()
        {
            return;
        }
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(0o400_000 | 0o2_000_000);
        }
        let Ok(mut file) = opts.open(&path) else {
            return;
        };
        if let Ok(meta) = file.metadata()
            && !meta.is_file()
        {
            return;
        }
        let payload = format!("{}\n", mode.c_enum_discriminant());
        let _ = file.write_all(payload.as_bytes());
    }
}

/// Read the DMI product name from `/sys/devices/virtual/dmi/id/product_name` (or an override path).
#[must_use]
pub fn read_dmi_product_name(dmi_path: Option<&Path>) -> Option<String> {
    let path = dmi_path.unwrap_or_else(|| Path::new("/sys/devices/virtual/dmi/id/product_name"));
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Detect CPU vendor from `/proc/cpuinfo` (or an override path), matching `Helper::isIntel()` / `Helper::isAmd()`.
#[must_use]
pub fn read_cpu_vendor(cpuinfo_path: Option<&Path>) -> CpuVendor {
    let path = cpuinfo_path.unwrap_or_else(|| Path::new("/proc/cpuinfo"));
    let Ok(contents) = fs::read_to_string(path) else {
        return CpuVendor::Unknown;
    };
    for line in contents.lines() {
        if line.contains("GenuineIntel") {
            return CpuVendor::Intel;
        }
        if line.contains("AuthenticAMD") {
            return CpuVendor::Amd;
        }
    }
    CpuVendor::Unknown
}

/// Read the SMBIOS system SKU / ID from `/sys/devices/virtual/dmi/id/product_sku` (or an override path).
#[must_use]
pub fn read_dmi_smbios_id(sku_path: Option<&Path>) -> Option<String> {
    let path = sku_path.unwrap_or_else(|| Path::new("/sys/devices/virtual/dmi/id/product_sku"));
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.starts_with("0x")
        && !trimmed.starts_with("0X")
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
    {
        Some(format!("0x{}", trimmed.to_ascii_uppercase()))
    } else {
        Some(trimmed.to_string())
    }
}

fn load_explicit_env_database() -> Option<Result<Database, CoreError>> {
    let raw = std::env::var_os("AWCC_DATABASE_PATH")?;
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let source = awcc_core::DatabaseSource::EnvVar(path.clone());
    match fs::read_to_string(&path) {
        Ok(contents) => Some(Database::from_json_str(&contents, source)),
        Err(err) => Some(Err(CoreError::DatabaseParse {
            source_label: source.to_string(),
            details: err.to_string(),
        })),
    }
}

/// Resolve the live host `DeviceProfile` from DMI (`product_name`, `product_sku`), `/proc/cpuinfo`,
/// and the 4-tier `database.json` lookup order.
pub fn resolve_host_device_profile() -> Result<DeviceProfile, CoreError> {
    let db = if let Some(explicit) = load_explicit_env_database() {
        explicit?
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Database::resolve_with_reader(None, &cwd, |p| fs::read_to_string(p).ok())?
    };
    let device_name = read_dmi_product_name(None).unwrap_or_else(|| "Alienware m15 R6".to_string());
    let smbios_id = read_dmi_smbios_id(None).unwrap_or_else(|| "0x0A86".to_string());
    let cpu_vendor = read_cpu_vendor(None);
    Ok(DeviceProfile::resolve(
        &db,
        device_name,
        smbios_id,
        cpu_vendor,
    ))
}

/// Resolve the simulated `Alienware m15 R6` (`0x0A86`, `AMWW`, `GenuineIntel`) profile,
/// honoring `AWCC_DATABASE_PATH` fail-closed when explicitly configured and otherwise using
/// the compiled-in default database.
pub fn resolve_simulated_m15_r6_profile() -> Result<DeviceProfile, CoreError> {
    if let Some(explicit) = load_explicit_env_database() {
        let db = explicit?;
        Ok(DeviceProfile::resolve(
            &db,
            "Alienware m15 R6",
            "0x0A86",
            CpuVendor::Intel,
        ))
    } else {
        DeviceProfile::alienware_m15_r6()
    }
}

/// Linux `hwmon` sensor provider matching C++ `readHwmonLabeledSensor` in `src/Thermals.cpp`.
///
/// Scans `/sys/class/hwmon/*` (or a test root directory) for `name` in `{"alienware_wmi", "dell_ddv"}`
/// and matches `temp{1..8}_label` / `fan{1..8}_label`.
#[derive(Debug, Clone)]
pub struct HwmonProvider {
    root: PathBuf,
}

impl Default for HwmonProvider {
    fn default() -> Self {
        Self::new("/sys/class/hwmon")
    }
}

impl HwmonProvider {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn read_labeled_sensor(
        &self,
        prefix: &str,
        target_labels: &[&str],
        divisor: i64,
    ) -> Option<u16> {
        let entries = fs::read_dir(&self.root).ok()?;
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|d| d.path())).collect();
        paths.sort();

        for dir in paths {
            let drv_name = fs::read_to_string(dir.join("name"))
                .ok()
                .map(|s| s.lines().next().unwrap_or("").trim().to_string())?;
            if drv_name != "alienware_wmi" && drv_name != "dell_ddv" {
                continue;
            }
            for idx in 1..=8 {
                let label_path = dir.join(format!("{prefix}{idx}_label"));
                let input_path = dir.join(format!("{prefix}{idx}_input"));
                let Ok(label_raw) = fs::read_to_string(&label_path) else {
                    continue;
                };
                let label = label_raw.lines().next().unwrap_or("").trim();
                if target_labels.contains(&label) {
                    let Ok(input_raw) = fs::read_to_string(&input_path) else {
                        continue;
                    };
                    let token = input_raw.split_whitespace().next().unwrap_or("");
                    if let Ok(raw_val) = token.parse::<i64>()
                        && raw_val >= 0
                        && divisor > 0
                    {
                        let scaled = raw_val / divisor;
                        if let Ok(val_u16) = u16::try_from(scaled) {
                            return Some(val_u16);
                        }
                    }
                }
            }
        }
        None
    }

    #[must_use]
    pub fn cpu_temp_c(&self) -> Option<u16> {
        self.read_labeled_sensor("temp", &["CPU"], 1000)
    }

    #[must_use]
    pub fn gpu_temp_c(&self) -> Option<u16> {
        self.read_labeled_sensor("temp", &["GPU", "Video"], 1000)
    }

    #[must_use]
    pub fn cpu_fan_rpm(&self) -> Option<u16> {
        self.read_labeled_sensor("fan", &["CPU Fan"], 1)
    }

    #[must_use]
    pub fn gpu_fan_rpm(&self) -> Option<u16> {
        self.read_labeled_sensor("fan", &["GPU Fan", "Video Fan"], 1)
    }
}

/// Callback or socket executor for raw command strings built by `LinuxAcpiTransport`,
/// matching C++ `AcpiUtils::CommandExecutor` / `Daemon::executeFromDaemon`.
pub type CommandExecutor = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Abstract hardware transport for ACPI WMAX thermal/fan control and CPU turbo sysfs.
pub trait ThermalTransport: Send {
    /// Returns `true` when backed by `SimBackend` rather than physical `/proc/acpi/call`.
    fn is_simulated(&self) -> bool;

    /// Current simulation scenario.
    fn scenario(&self) -> SimScenario;

    /// Switch the active simulation scenario (passive; does not count as a hardware write).
    fn set_scenario(&mut self, scenario: SimScenario);

    /// Install fine-grained simulated faults. Real transports ignore this.
    fn set_sim_faults(&mut self, _faults: SimFaults) {}

    /// Query raw thermal mode byte via `WMAX 0 0x14 {0x0b, 0x00, 0x00, 0x00}`.
    fn query_raw_thermal_mode(&mut self) -> Result<u32, TransportError>;

    /// Query GameShift (`G-Mode`) latch via `WMAX 0 0x25 {0x02, 0x00, 0x00, 0x00}`.
    fn query_game_shift_status(&mut self) -> Result<bool, TransportError>;

    /// Set thermal mode code via `WMAX 0 0x15 {0x01, mode_code, 0x00, 0x00}`.
    /// Accepts `u16` to support legacy `0x196..0x198` set codes on `Aurora R9` / `17 R5`.
    fn set_raw_thermal_mode(&mut self, mode_code: u16) -> Result<(), TransportError>;

    /// Set GameShift (`G-Mode`) latch via `WMAX 0 0x25 {arg1, 0x00, 0x00, 0x00}`.
    fn set_game_shift_status(&mut self, enabled: bool) -> Result<(), TransportError>;

    /// Read CPU & GPU temperatures (`hwmon` -> `0x14 0x04`), fan RPM (`hwmon` -> `0x14 0x05`),
    /// and fan boost (`0x14 0x0c`).
    fn query_telemetry(&mut self) -> TelemetrySnapshot;

    /// Write manual CPU (`0x32`) and GPU (`0x33`, or `0x36`+`0x3a` on `Alienware Aurora R11`)
    /// fan boost (`0..=100`) via `WMAX 0 0x15 {0x02, fan_id, boost, 0x00}`.
    fn set_fan_boost(&mut self, cpu_boost: u8, gpu_boost: u8) -> Result<(), TransportError>;

    /// Query CPU Turbo Boost state (`/sys/devices/system/cpu/intel_pstate/no_turbo` or `cpufreq/boost`).
    ///
    /// Returns an error when no usable control file exists or its content cannot be parsed: a
    /// failed query is never reported as "enabled" or "disabled".
    fn query_cpu_turbo(&mut self) -> Result<bool, TransportError>;

    /// Toggle CPU Turbo Boost state.
    fn set_cpu_turbo(&mut self, enabled: bool) -> Result<(), TransportError>;

    /// Query CPU TCC Offset (`0..=15°C` reduction from TjMax) via `WMAX 0 0x14 {0x0d, 0x00, 0x00, 0x00}`.
    fn query_tcc_offset(&mut self) -> SensorReading<u8>;

    /// Set CPU TCC Offset (`0..=15°C` reduction from TjMax) via `WMAX 0 0x15 {0x03, offset, 0x00, 0x00}`.
    fn set_tcc_offset(&mut self, offset: u8) -> Result<(), TransportError>;

    /// Inspect all hardware write commands issued since initialization.
    fn write_audit_log(&self) -> &[WriteAuditEntry];
}

const NO_TURBO_PATH: &str = "/sys/devices/system/cpu/intel_pstate/no_turbo";
const BOOST_PATH: &str = "/sys/devices/system/cpu/cpufreq/boost";

/// Read the CPU turbo state from the `intel_pstate/no_turbo` (inverted) or `cpufreq/boost` file.
///
/// Returns an error when neither file is readable and numeric, so that a missing or malformed
/// control file is never reported as "turbo enabled".
pub fn read_cpu_turbo_state(no_turbo: &Path, boost: &Path) -> Result<bool, TransportError> {
    if let Ok(s) = fs::read_to_string(no_turbo)
        && let Ok(val) = s.trim().parse::<u8>()
    {
        return Ok(val == 0);
    }
    if let Ok(s) = fs::read_to_string(boost)
        && let Ok(val) = s.trim().parse::<u8>()
    {
        return Ok(val != 0);
    }
    Err(TransportError::TransportUnavailable(format!(
        "no readable CPU turbo control file ({} or {})",
        no_turbo.display(),
        boost.display()
    )))
}

/// Real / mockable Linux ACPI WMAX transport matching C++ `AcpiUtils::executeAcpiCommand` + `Thermals`.
pub struct LinuxAcpiTransport {
    device_name: String,
    prefix: AcpiPrefix,
    hwmon: HwmonProvider,
    executor: CommandExecutor,
    write_log: Vec<WriteAuditEntry>,
    next_write_seq: usize,
}

impl LinuxAcpiTransport {
    #[must_use]
    pub fn new(
        device_name: impl Into<String>,
        prefix: AcpiPrefix,
        hwmon: HwmonProvider,
        executor: CommandExecutor,
    ) -> Self {
        Self {
            device_name: device_name.into(),
            prefix,
            hwmon,
            executor,
            write_log: Vec::new(),
            next_write_seq: 0,
        }
    }

    /// Format the exact shell command string produced by C++ `AcpiUtils::executeAcpiCommand`.
    #[must_use]
    pub fn format_acpi_shell_command(
        prefix: AcpiPrefix,
        subcmd: u8,
        arg1: u16,
        arg2: u16,
        arg3: u16,
    ) -> String {
        format!(
            "pkexec sh -c 'echo \"{wmax} 0 {subcmd:#x} {{{arg1:#x},{arg2:#x},{arg3:#x},0x00}}\" > /proc/acpi/call && cat /proc/acpi/call'",
            wmax = prefix.wmax_path()
        )
    }

    /// Execute an ACPI WMAX call and parse its hex return code exactly like `AcpiUtils::executeAcpiCommand`.
    pub fn execute_acpi_call(
        &mut self,
        subcmd: u8,
        arg1: u16,
        arg2: u16,
        arg3: u16,
        is_mutating: bool,
    ) -> Result<i32, TransportError> {
        let cmd = Self::format_acpi_shell_command(self.prefix, subcmd, arg1, arg2, arg3);
        let raw_output = (self.executor)(&cmd);
        let Some(parsed) = parse_acpi_output(&raw_output) else {
            if is_mutating {
                self.record_write(cmd.clone(), "parse error", false);
            }
            return Err(TransportError::TransportUnavailable(format!(
                "Failed to parse ACPI output for `{cmd}`: `{raw_output}`"
            )));
        };

        #[allow(clippy::cast_sign_loss)]
        let unsigned = parsed as u32;
        let ok = parsed >= 0 && unsigned < 0xffff_fffd;
        if is_mutating {
            self.record_write(cmd.clone(), format!("0x{unsigned:x}"), ok);
        }
        if !ok {
            return Err(TransportError::HardwareRejected { call: cmd });
        }
        Ok(parsed)
    }

    fn record_write(&mut self, command: String, outcome: impl Into<String>, succeeded: bool) {
        self.next_write_seq += 1;
        let sequence = self.next_write_seq;
        self.write_log
            .push(WriteAuditEntry::new(sequence, command, outcome, succeeded));
        if self.write_log.len() > MAX_TRANSPORT_AUDIT_ENTRIES {
            let excess = self.write_log.len() - MAX_TRANSPORT_AUDIT_ENTRIES;
            self.write_log.drain(..excess);
        }
    }
}

impl ThermalTransport for LinuxAcpiTransport {
    fn is_simulated(&self) -> bool {
        false
    }

    fn scenario(&self) -> SimScenario {
        SimScenario::Normal
    }

    fn set_scenario(&mut self, _scenario: SimScenario) {}

    fn query_raw_thermal_mode(&mut self) -> Result<u32, TransportError> {
        let val = self.execute_acpi_call(0x14, 0x0b, 0x00, 0x00, false)?;
        #[allow(clippy::cast_sign_loss)]
        Ok(val as u32)
    }

    fn query_game_shift_status(&mut self) -> Result<bool, TransportError> {
        let val = self.execute_acpi_call(0x25, 0x02, 0x00, 0x00, false)?;
        Ok(val != 0)
    }

    fn set_raw_thermal_mode(&mut self, mode_code: u16) -> Result<(), TransportError> {
        let res = self.execute_acpi_call(0x15, 0x01, mode_code, 0x00, true)?;
        if res == 0 {
            Ok(())
        } else {
            Err(TransportError::HardwareRejected {
                call: Self::format_acpi_shell_command(self.prefix, 0x15, 0x01, mode_code, 0),
            })
        }
    }

    fn set_game_shift_status(&mut self, enabled: bool) -> Result<(), TransportError> {
        if let Ok(active) = self.query_game_shift_status()
            && active == enabled
        {
            return Ok(());
        }
        // C++ `Thermals::setThermalMode` calls `executeAcpiCommand(0x25, 0x01)` to toggle GameShift.
        // On some Alienware WMAX firmware (including m15 R6), `0x25 0x01` returns `0x1` even when
        // toggling GameShift OFF while `0x25 0x02` (`Get G mode`) reflects the new `0x0` state.
        let res = self.execute_acpi_call(0x25, 0x01, 0x00, 0x00, true)?;
        if (res != 0) == enabled || self.query_game_shift_status().ok() == Some(enabled) {
            Ok(())
        } else {
            Err(TransportError::HardwareRejected {
                call: Self::format_acpi_shell_command(self.prefix, 0x25, 0x01, 0x00, 0x00),
            })
        }
    }

    fn query_telemetry(&mut self) -> TelemetrySnapshot {
        let cpu_temp_c = if let Some(temp) = self.hwmon.cpu_temp_c() {
            SensorReading::Valid {
                value: temp,
                unit: "°C",
            }
        } else {
            match self.execute_acpi_call(0x14, 0x04, 0x01, 0x00, false) {
                Ok(v) if u16::try_from(v).is_ok() => SensorReading::Valid {
                    value: u16::try_from(v).unwrap_or(0),
                    unit: "°C",
                },
                Ok(v) => SensorReading::Unavailable {
                    reason: format!("Out-of-range CPU temp response: {v}"),
                },
                Err(e) => SensorReading::Unavailable {
                    reason: e.to_string(),
                },
            }
        };

        let gpu_temp_c = if let Some(temp) = self.hwmon.gpu_temp_c() {
            SensorReading::Valid {
                value: temp,
                unit: "°C",
            }
        } else {
            match self.execute_acpi_call(0x14, 0x04, 0x06, 0x00, false) {
                Ok(v) if u16::try_from(v).is_ok() => SensorReading::Valid {
                    value: u16::try_from(v).unwrap_or(0),
                    unit: "°C",
                },
                Ok(v) => SensorReading::Unavailable {
                    reason: format!("Out-of-range GPU temp response: {v}"),
                },
                Err(e) => SensorReading::Unavailable {
                    reason: e.to_string(),
                },
            }
        };

        let cpu_fan_rpm = if let Some(rpm) = self.hwmon.cpu_fan_rpm() {
            SensorReading::Valid {
                value: rpm,
                unit: " RPM",
            }
        } else {
            match self.execute_acpi_call(0x14, 0x05, 0x32, 0x00, false) {
                Ok(v) if u16::try_from(v).is_ok() => SensorReading::Valid {
                    value: u16::try_from(v).unwrap_or(0),
                    unit: " RPM",
                },
                Ok(v) => SensorReading::Unavailable {
                    reason: format!("Out-of-range CPU fan RPM response: {v}"),
                },
                Err(e) => SensorReading::Unavailable {
                    reason: e.to_string(),
                },
            }
        };

        let gpu_fan_rpm = if let Some(rpm) = self.hwmon.gpu_fan_rpm() {
            SensorReading::Valid {
                value: rpm,
                unit: " RPM",
            }
        } else {
            match self.execute_acpi_call(0x14, 0x05, 0x33, 0x00, false) {
                Ok(v) if u16::try_from(v).is_ok() => SensorReading::Valid {
                    value: u16::try_from(v).unwrap_or(0),
                    unit: " RPM",
                },
                Ok(v) => SensorReading::Unavailable {
                    reason: format!("Out-of-range GPU fan RPM response: {v}"),
                },
                Err(e) => SensorReading::Unavailable {
                    reason: e.to_string(),
                },
            }
        };

        let cpu_boost_pct = match self.execute_acpi_call(0x14, 0x0c, 0x32, 0x00, false) {
            Ok(v) if (0..=100).contains(&v) => SensorReading::Valid {
                value: u8::try_from(v).unwrap_or(0),
                unit: "%",
            },
            Ok(v) => SensorReading::Unavailable {
                reason: format!("Out-of-range CPU boost response: {v}"),
            },
            Err(e) => SensorReading::Unavailable {
                reason: e.to_string(),
            },
        };

        let gpu_boost_pct = match self.execute_acpi_call(0x14, 0x0c, 0x33, 0x00, false) {
            Ok(v) if (0..=100).contains(&v) => SensorReading::Valid {
                value: u8::try_from(v).unwrap_or(0),
                unit: "%",
            },
            Ok(v) => SensorReading::Unavailable {
                reason: format!("Out-of-range GPU boost response: {v}"),
            },
            Err(e) => SensorReading::Unavailable {
                reason: e.to_string(),
            },
        };

        TelemetrySnapshot {
            cpu_temp_c,
            gpu_temp_c,
            cpu_fan_rpm,
            gpu_fan_rpm,
            cpu_boost_pct,
            gpu_boost_pct,
        }
    }

    fn set_fan_boost(&mut self, cpu_boost: u8, gpu_boost: u8) -> Result<(), TransportError> {
        let cpu_boost = u16::from(cpu_boost.min(100));
        let gpu_boost = u16::from(gpu_boost.min(100));
        self.execute_acpi_call(0x15, 0x02, 0x32, cpu_boost, true)?;
        if self.device_name == "Alienware Aurora R11" {
            self.execute_acpi_call(0x15, 0x02, 0x36, gpu_boost, true)?;
            self.execute_acpi_call(0x15, 0x02, 0x3a, gpu_boost, true)?;
        } else {
            self.execute_acpi_call(0x15, 0x02, 0x33, gpu_boost, true)?;
        }
        Ok(())
    }

    fn query_cpu_turbo(&mut self) -> Result<bool, TransportError> {
        read_cpu_turbo_state(Path::new(NO_TURBO_PATH), Path::new(BOOST_PATH))
    }

    fn set_cpu_turbo(&mut self, enabled: bool) -> Result<(), TransportError> {
        let cmd = if Path::new("/sys/devices/system/cpu/cpufreq/boost").exists()
            && !Path::new("/sys/devices/system/cpu/intel_pstate/no_turbo").exists()
        {
            let write_val = u8::from(enabled);
            format!("echo {write_val} | sudo tee /sys/devices/system/cpu/cpufreq/boost")
        } else {
            let no_turbo = u8::from(!enabled);
            format!("echo {no_turbo} | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo")
        };
        let out = (self.executor)(&cmd);
        let ok = !out.contains("Error") && !out.contains("Rejected");
        self.record_write(cmd.clone(), if ok { "OK" } else { "Error" }, ok);
        if ok {
            Ok(())
        } else {
            Err(TransportError::TransportUnavailable(out))
        }
    }

    fn query_tcc_offset(&mut self) -> SensorReading<u8> {
        match self.execute_acpi_call(0x14, 0x0d, 0x00, 0x00, false) {
            Ok(v) if (0..=15).contains(&v) => SensorReading::Valid {
                value: u8::try_from(v).unwrap_or(0),
                unit: "°C",
            },
            Ok(v) => SensorReading::Unavailable {
                reason: format!("Out-of-range TCC offset response: {v}"),
            },
            Err(e) => SensorReading::Unavailable {
                reason: e.to_string(),
            },
        }
    }

    fn set_tcc_offset(&mut self, offset: u8) -> Result<(), TransportError> {
        let clamped = u16::from(offset.min(15));
        let res = self.execute_acpi_call(0x15, 0x03, clamped, 0x00, true)?;
        if res == 0 {
            Ok(())
        } else {
            Err(TransportError::HardwareRejected {
                call: Self::format_acpi_shell_command(self.prefix, 0x15, 0x03, clamped, 0),
            })
        }
    }

    fn write_audit_log(&self) -> &[WriteAuditEntry] {
        &self.write_log
    }
}

/// Fine-grained fault injection for [`SimBackend`], used to exercise failure reporting without
/// hardware. Independent of (and combinable with) the coarse [`SimScenario`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimFaults {
    /// `set_cpu_turbo` is rejected (recorded as a failed write).
    pub reject_turbo_write: bool,
    /// `query_cpu_turbo` fails.
    pub turbo_readback_unavailable: bool,
    /// `query_cpu_turbo` keeps returning this value regardless of writes (device ignored them).
    pub turbo_readback_stuck: Option<bool>,
    /// `query_game_shift_status` fails.
    pub game_shift_query_unavailable: bool,
    /// `query_raw_thermal_mode` fails.
    pub thermal_mode_query_unavailable: bool,
    /// Clearing the GameShift latch (`set_game_shift_status(false)`) is rejected.
    pub reject_game_shift_clear: bool,
    /// The reported fan boost stays at the Balanced preset regardless of writes.
    pub fan_boost_readback_stuck: bool,
    /// The reported TCC offset stays at 0 regardless of writes.
    pub tcc_readback_stuck: bool,
}

/// Deterministic simulation of an `Alienware m15 R6` (`\_SB.AMWW.WMAX`) hardware controller.
#[derive(Debug, Clone)]
pub struct SimBackend {
    prefix: AcpiPrefix,
    code_table: ThermalCodeTable,
    scenario: SimScenario,
    raw_mode_code: u16,
    game_shift_enabled: bool,
    cpu_boost_pct: u8,
    gpu_boost_pct: u8,
    cpu_turbo_enabled: bool,
    tcc_offset_c: u8,
    faults: SimFaults,
    write_log: Vec<WriteAuditEntry>,
    next_write_seq: usize,
}

impl SimBackend {
    /// Construct a new simulated Alienware m15 R6 backend initialized passively in `Balanced` (`0xA0`).
    #[must_use]
    pub fn new_alienware_m15_r6() -> Self {
        let code_table = ThermalCodeTable::for_device("Alienware m15 R6");
        let balanced_code = code_table.set_code_for(ThermalMode::Balanced);
        Self {
            prefix: AcpiPrefix::Amww,
            code_table,
            scenario: SimScenario::Normal,
            raw_mode_code: balanced_code,
            game_shift_enabled: false,
            cpu_boost_pct: 28,
            gpu_boost_pct: 24,
            cpu_turbo_enabled: true,
            tcc_offset_c: 0,
            faults: SimFaults::default(),
            write_log: Vec::new(),
            next_write_seq: 0,
        }
    }

    fn record_write(&mut self, command: String, outcome: impl Into<String>, succeeded: bool) {
        self.next_write_seq += 1;
        let sequence = self.next_write_seq;
        self.write_log
            .push(WriteAuditEntry::new(sequence, command, outcome, succeeded));
        if self.write_log.len() > MAX_TRANSPORT_AUDIT_ENTRIES {
            let excess = self.write_log.len() - MAX_TRANSPORT_AUDIT_ENTRIES;
            self.write_log.drain(..excess);
        }
    }

    fn apply_telemetry_preset_for_code(&mut self, mode_code: u16) {
        if mode_code == self.code_table.set_code_for(ThermalMode::Quiet) {
            self.cpu_boost_pct = 15;
            self.gpu_boost_pct = 12;
        } else if mode_code == self.code_table.set_code_for(ThermalMode::Cool) {
            self.cpu_boost_pct = 35;
            self.gpu_boost_pct = 35;
        } else if mode_code == self.code_table.set_code_for(ThermalMode::Balanced) {
            self.cpu_boost_pct = 28;
            self.gpu_boost_pct = 24;
        } else if mode_code == self.code_table.set_code_for(ThermalMode::Performance) {
            self.cpu_boost_pct = 62;
            self.gpu_boost_pct = 58;
        } else if mode_code == self.code_table.set_code_for(ThermalMode::FullSpeed) {
            self.cpu_boost_pct = 90;
            self.gpu_boost_pct = 90;
        } else if mode_code == self.code_table.set_code_for(ThermalMode::GMode) {
            self.cpu_boost_pct = 100;
            self.gpu_boost_pct = 100;
        }
    }
}

impl ThermalTransport for SimBackend {
    fn is_simulated(&self) -> bool {
        true
    }

    fn scenario(&self) -> SimScenario {
        self.scenario
    }

    fn set_scenario(&mut self, scenario: SimScenario) {
        self.scenario = scenario;
    }

    fn set_sim_faults(&mut self, faults: SimFaults) {
        self.faults = faults;
    }

    fn query_raw_thermal_mode(&mut self) -> Result<u32, TransportError> {
        if self.faults.thermal_mode_query_unavailable {
            return Err(TransportError::TransportUnavailable(
                "simulated thermal-mode query failure".to_string(),
            ));
        }
        match self.scenario {
            SimScenario::UnknownModeResponse => Ok(0x0000_00fe),
            SimScenario::Normal
            | SimScenario::AcpiSetFailureRollback
            | SimScenario::SensorUnavailable => Ok(u32::from(self.raw_mode_code & 0xff)),
        }
    }

    fn query_game_shift_status(&mut self) -> Result<bool, TransportError> {
        if self.faults.game_shift_query_unavailable {
            return Err(TransportError::TransportUnavailable(
                "simulated GameShift query failure".to_string(),
            ));
        }
        Ok(self.game_shift_enabled)
    }

    fn set_raw_thermal_mode(&mut self, mode_code: u16) -> Result<(), TransportError> {
        let call = format!(
            "{} 0 0x15 {{0x01, 0x{mode_code:02x}, 0x00, 0x00}}",
            self.prefix.wmax_path()
        );

        // In AcpiSetFailureRollback scenario, WMAX 0x15 rejects mode transitions with 0xFFFFFFFF.
        if self.scenario == SimScenario::AcpiSetFailureRollback {
            self.record_write(call.clone(), "0xFFFFFFFF (simulated ACPI rejection)", false);
            return Err(TransportError::HardwareRejected { call });
        }

        self.raw_mode_code = mode_code;
        self.apply_telemetry_preset_for_code(mode_code);
        self.record_write(call, "0x0 (OK)", true);
        Ok(())
    }

    fn set_game_shift_status(&mut self, enabled: bool) -> Result<(), TransportError> {
        let byte = u8::from(enabled);
        let call = format!(
            "{} 0 0x25 {{0x01, 0x{byte:02x}, 0x00, 0x00}}",
            self.prefix.wmax_path()
        );
        if !enabled && self.faults.reject_game_shift_clear {
            self.record_write(call.clone(), "0xFFFFFFFF (simulated ACPI rejection)", false);
            return Err(TransportError::HardwareRejected { call });
        }
        self.game_shift_enabled = enabled;
        self.record_write(call, "0x0 (OK)", true);
        Ok(())
    }

    fn query_telemetry(&mut self) -> TelemetrySnapshot {
        if self.scenario == SimScenario::SensorUnavailable {
            let reason = "ACPI WMAX 0x14 sensor query returned 0xFFFFFFFF".to_string();
            return TelemetrySnapshot {
                cpu_temp_c: SensorReading::Unavailable {
                    reason: reason.clone(),
                },
                gpu_temp_c: SensorReading::Unavailable {
                    reason: reason.clone(),
                },
                cpu_fan_rpm: SensorReading::Unavailable {
                    reason: reason.clone(),
                },
                gpu_fan_rpm: SensorReading::Unavailable {
                    reason: reason.clone(),
                },
                cpu_boost_pct: SensorReading::Unavailable {
                    reason: reason.clone(),
                },
                gpu_boost_pct: SensorReading::Unavailable { reason },
            };
        }

        let (base_cpu_temp, base_gpu_temp): (u16, u16) = if self.game_shift_enabled {
            (61, 56)
        } else if self.raw_mode_code == self.code_table.set_code_for(ThermalMode::Performance) {
            (68, 63)
        } else if self.raw_mode_code == self.code_table.set_code_for(ThermalMode::Quiet) {
            (54, 50)
        } else if self.raw_mode_code == self.code_table.set_code_for(ThermalMode::Cool) {
            (49, 46)
        } else {
            (58, 53)
        };

        let cpu_rpm = 1600_u16 + u16::from(self.cpu_boost_pct) * 34;
        let gpu_rpm = 1500_u16 + u16::from(self.gpu_boost_pct) * 33;

        TelemetrySnapshot {
            cpu_temp_c: SensorReading::Valid {
                value: base_cpu_temp,
                unit: "°C",
            },
            gpu_temp_c: SensorReading::Valid {
                value: base_gpu_temp,
                unit: "°C",
            },
            cpu_fan_rpm: SensorReading::Valid {
                value: cpu_rpm,
                unit: " RPM",
            },
            gpu_fan_rpm: SensorReading::Valid {
                value: gpu_rpm,
                unit: " RPM",
            },
            cpu_boost_pct: SensorReading::Valid {
                value: if self.faults.fan_boost_readback_stuck {
                    28
                } else {
                    self.cpu_boost_pct
                },
                unit: "%",
            },
            gpu_boost_pct: SensorReading::Valid {
                value: if self.faults.fan_boost_readback_stuck {
                    24
                } else {
                    self.gpu_boost_pct
                },
                unit: "%",
            },
        }
    }

    fn set_fan_boost(&mut self, cpu_boost: u8, gpu_boost: u8) -> Result<(), TransportError> {
        let cpu_call = format!(
            "{} 0 0x15 {{0x02, 0x32, 0x{cpu_boost:02x}, 0x00}}",
            self.prefix.wmax_path()
        );
        let gpu_call = format!(
            "{} 0 0x15 {{0x02, 0x33, 0x{gpu_boost:02x}, 0x00}}",
            self.prefix.wmax_path()
        );

        if self.scenario == SimScenario::AcpiSetFailureRollback {
            self.record_write(
                cpu_call.clone(),
                "0xFFFFFFFF (simulated ACPI rejection)",
                false,
            );
            return Err(TransportError::HardwareRejected { call: cpu_call });
        }

        self.cpu_boost_pct = cpu_boost;
        self.gpu_boost_pct = gpu_boost;
        self.record_write(cpu_call, "0x0 (OK)", true);
        self.record_write(gpu_call, "0x0 (OK)", true);
        Ok(())
    }

    fn query_cpu_turbo(&mut self) -> Result<bool, TransportError> {
        if self.scenario == SimScenario::SensorUnavailable || self.faults.turbo_readback_unavailable
        {
            return Err(TransportError::TransportUnavailable(
                "simulated CPU turbo control file unreadable".to_string(),
            ));
        }
        Ok(self
            .faults
            .turbo_readback_stuck
            .unwrap_or(self.cpu_turbo_enabled))
    }

    fn set_cpu_turbo(&mut self, enabled: bool) -> Result<(), TransportError> {
        let no_turbo_val = u8::from(!enabled);
        let call =
            format!("sysfs write /sys/devices/system/cpu/intel_pstate/no_turbo = {no_turbo_val}");
        if self.faults.reject_turbo_write {
            self.record_write(
                call.clone(),
                "Error: simulated sysfs write rejection",
                false,
            );
            return Err(TransportError::HardwareRejected { call });
        }
        self.cpu_turbo_enabled = enabled;
        self.record_write(call, "OK", true);
        Ok(())
    }

    fn query_tcc_offset(&mut self) -> SensorReading<u8> {
        if self.scenario == SimScenario::SensorUnavailable {
            SensorReading::Unavailable {
                reason: "ACPI WMAX 0x14 0x0d returned 0xFFFFFFFF".to_string(),
            }
        } else {
            SensorReading::Valid {
                value: if self.faults.tcc_readback_stuck {
                    0
                } else {
                    self.tcc_offset_c
                },
                unit: "°C",
            }
        }
    }

    fn set_tcc_offset(&mut self, offset: u8) -> Result<(), TransportError> {
        let clamped = offset.min(15);
        let call = format!(
            "{} 0 0x15 {{0x03, 0x{clamped:02x}, 0x00, 0x00}}",
            self.prefix.wmax_path()
        );
        if self.scenario == SimScenario::AcpiSetFailureRollback {
            self.record_write(call.clone(), "0xFFFFFFFF (simulated ACPI rejection)", false);
            return Err(TransportError::HardwareRejected { call });
        }
        self.tcc_offset_c = clamped;
        self.record_write(call, "0x0 (OK)", true);
        Ok(())
    }

    fn write_audit_log(&self) -> &[WriteAuditEntry] {
        &self.write_log
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn passive_queries_produce_zero_write_audit_entries() {
        let mut sim = SimBackend::new_alienware_m15_r6();
        assert_eq!(sim.query_raw_thermal_mode().expect("query mode"), 0xa0);
        assert!(!sim.query_game_shift_status().expect("query gmode"));
        let telemetry = sim.query_telemetry();
        assert!(telemetry.cpu_temp_c.is_available());
        assert!(sim.write_audit_log().is_empty());
    }

    #[test]
    fn failure_scenario_rejects_wmax_0x15_and_logs_failed_write() {
        let mut sim = SimBackend::new_alienware_m15_r6();
        sim.set_scenario(SimScenario::AcpiSetFailureRollback);
        let err = sim
            .set_raw_thermal_mode(0xab)
            .expect_err("should reject 0x15 under AcpiSetFailureRollback");
        assert!(matches!(err, TransportError::HardwareRejected { .. }));
        assert_eq!(sim.write_audit_log().len(), 1);
        assert!(!sim.write_audit_log()[0].succeeded);
    }

    #[test]
    fn posix_state_store_roundtrips_and_rejects_symlinks_or_gmode() {
        let tmp_dir =
            std::env::temp_dir().join(format!("awcc_rust_state_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let state_file = tmp_dir.join("prev_mode");
        let mut store = PosixStateStore::with_override_path(&state_file);

        store.save_pre_gmode(ThermalMode::Performance);
        assert_eq!(store.load_pre_gmode(), Some(ThermalMode::Performance));

        // Saving GMode or Manual must be ignored, preserving Performance.
        store.save_pre_gmode(ThermalMode::GMode);
        assert_eq!(store.load_pre_gmode(), Some(ThermalMode::Performance));
        store.save_pre_gmode(ThermalMode::Manual);
        assert_eq!(store.load_pre_gmode(), Some(ThermalMode::Performance));

        // Symlink attack must be rejected by O_NOFOLLOW.
        #[cfg(unix)]
        {
            let target_file = tmp_dir.join("target_file");
            fs::write(&target_file, "0\n").unwrap();
            let symlink_file = tmp_dir.join("symlink_prev_mode");
            std::os::unix::fs::symlink(&target_file, &symlink_file).unwrap();
            let mut sym_store = PosixStateStore::with_override_path(&symlink_file);
            assert_eq!(sym_store.load_pre_gmode(), None);
            sym_store.save_pre_gmode(ThermalMode::Cool);
            assert_eq!(fs::read_to_string(&target_file).unwrap(), "0\n");
        }

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn hwmon_provider_reads_alienware_wmi_labeled_sensors() {
        let tmp_dir =
            std::env::temp_dir().join(format!("awcc_rust_hwmon_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        let hwmon0 = tmp_dir.join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "alienware_wmi\n").unwrap();
        fs::write(hwmon0.join("temp1_label"), "CPU\n").unwrap();
        fs::write(hwmon0.join("temp1_input"), "64000\n").unwrap();
        fs::write(hwmon0.join("temp2_label"), "GPU\n").unwrap();
        fs::write(hwmon0.join("temp2_input"), "59000\n").unwrap();
        fs::write(hwmon0.join("fan1_label"), "CPU Fan\n").unwrap();
        fs::write(hwmon0.join("fan1_input"), "2850\n").unwrap();
        fs::write(hwmon0.join("fan2_label"), "Video Fan\n").unwrap();
        fs::write(hwmon0.join("fan2_input"), "2710\n").unwrap();

        let provider = HwmonProvider::new(&tmp_dir);
        assert_eq!(provider.cpu_temp_c(), Some(64));
        assert_eq!(provider.gpu_temp_c(), Some(59));
        assert_eq!(provider.cpu_fan_rpm(), Some(2850));
        assert_eq!(provider.gpu_fan_rpm(), Some(2710));

        let _ = fs::remove_dir_all(&tmp_dir);
    }
    #[test]
    fn cpu_turbo_query_reports_failure_instead_of_defaulting_to_enabled() {
        let tmp_dir =
            std::env::temp_dir().join(format!("awcc_rust_turbo_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();
        let no_turbo = tmp_dir.join("no_turbo");
        let boost = tmp_dir.join("boost");

        // Neither control file exists: unknown, never "enabled".
        let err = read_cpu_turbo_state(&no_turbo, &boost).expect_err("missing files");
        assert!(matches!(err, TransportError::TransportUnavailable(_)));

        // Garbage content is also an error.
        fs::write(&no_turbo, "enabled?\n").unwrap();
        assert!(read_cpu_turbo_state(&no_turbo, &boost).is_err());

        // `no_turbo` is inverted; `boost` is not.
        fs::write(&no_turbo, "0\n").unwrap();
        assert!(read_cpu_turbo_state(&no_turbo, &boost).unwrap());
        fs::write(&no_turbo, "1\n").unwrap();
        assert!(!read_cpu_turbo_state(&no_turbo, &boost).unwrap());
        fs::remove_file(&no_turbo).unwrap();
        fs::write(&boost, "1\n").unwrap();
        assert!(read_cpu_turbo_state(&no_turbo, &boost).unwrap());
        fs::write(&boost, "0\n").unwrap();
        assert!(!read_cpu_turbo_state(&no_turbo, &boost).unwrap());

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
