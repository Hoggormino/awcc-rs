//! Core domain types, device database resolution, ACPI prefix matching, and thermal state rules.
//!
//! This crate is intentionally pure: it performs no hardware I/O and has no UI dependencies.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod lighting;

/// Compiled-in fallback device profiles.
pub const DEFAULT_DATABASE_JSON: &str = include_str!("../data/database.json");

/// Errors produced by `awcc-core` domain operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("failed to parse device database JSON from {source_label}: {details}")]
    DatabaseParse {
        source_label: String,
        details: String,
    },
    #[error("thermal mode `{mode}` is not supported by device `{device_name}`")]
    UnsupportedThermalMode {
        device_name: String,
        mode: ThermalMode,
    },
    #[error("fan boost value {0}% is out of valid range 0..=100%")]
    InvalidFanBoost(u16),
    #[error("TCC offset value {0}°C is out of valid range 0..=15°C")]
    InvalidTccOffset(i32),
}

/// Provenance of the resolved `database.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DatabaseSource {
    EnvVar(PathBuf),
    LocalWorkingDir(PathBuf),
    SystemEtc(PathBuf),
    EmbeddedDefault,
}

impl fmt::Display for DatabaseSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvVar(p) => write!(f, "AWCC_DATABASE_PATH ({})", p.display()),
            Self::LocalWorkingDir(p) => write!(f, "local ({})", p.display()),
            Self::SystemEtc(p) => write!(f, "system ({})", p.display()),
            Self::EmbeddedDefault => write!(f, "compiled-in DEFAULT_DATABASE_JSON"),
        }
    }
}

fn parse_bitset_value(val: &serde_json::Value, field: &str) -> Result<u8, String> {
    match val {
        serde_json::Value::String(s) => u8::from_str_radix(s.trim(), 2)
            .map_err(|e| format!("invalid binary bitstring for `{field}` (`{s}`): {e}")),
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| format!("out-of-range numeric bitset for `{field}`: {n}")),
        other => Err(format!("unexpected JSON type for `{field}`: {other}")),
    }
}

const fn default_feature_set() -> u8 {
    0b0111_1111
}

const fn default_thermal_modes() -> u8 {
    0b1111_1111
}

const fn default_lighting_modes() -> u8 {
    0b0011_1111
}

fn parse_zones_value(val: &serde_json::Value) -> Result<Vec<u8>, String> {
    let Some(items) = val.as_array() else {
        return Err("`keyboardZones` must be a JSON array".to_string());
    };
    let mut zones = Vec::with_capacity(items.len());
    for item in items {
        if let serde_json::Value::String(s) = item {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                continue;
            }
            let hex = trimmed
                .strip_prefix("0x")
                .or_else(|| trimmed.strip_prefix("0X"))
                .unwrap_or(trimmed);
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                #[allow(clippy::cast_possible_truncation)]
                let z = (v & 0xFF) as u8;
                if !zones.contains(&z) {
                    zones.push(z);
                }
            }
        }
    }
    Ok(zones)
}

/// Per-model capability entry in `database.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceEntry {
    #[serde(rename = "featureSet")]
    pub feature_set: u8,
    #[serde(rename = "thermalModes")]
    pub thermal_modes: u8,
    #[serde(rename = "lightingModes")]
    pub lighting_modes: u8,
    #[serde(rename = "keyboardZones", default)]
    pub keyboard_zones: Vec<u8>,
}

impl<'de> Deserialize<'de> for DeviceEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let map = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let feature_set = match map.get("featureSet") {
            Some(v) => parse_bitset_value(v, "featureSet").map_err(serde::de::Error::custom)?,
            None => 0,
        };
        let thermal_modes = match map.get("thermalModes") {
            Some(v) => parse_bitset_value(v, "thermalModes").map_err(serde::de::Error::custom)?,
            None => 0,
        };
        let lighting_modes = match map.get("lightingModes") {
            Some(v) => parse_bitset_value(v, "lightingModes").map_err(serde::de::Error::custom)?,
            None => 0,
        };
        let keyboard_zones = match map.get("keyboardZones") {
            Some(v) => parse_zones_value(v).map_err(serde::de::Error::custom)?,
            None => vec![0, 1, 2, 3],
        };

        Ok(Self {
            feature_set,
            thermal_modes,
            lighting_modes,
            keyboard_zones,
        })
    }
}

impl DeviceEntry {
    /// Default capability profile used when a device is unlisted.
    #[must_use]
    pub fn fallback_default() -> Self {
        Self {
            feature_set: default_feature_set(),
            thermal_modes: default_thermal_modes(),
            lighting_modes: default_lighting_modes(),
            keyboard_zones: vec![0, 1, 2, 3],
        }
    }

    #[must_use]
    pub const fn has_feature_fan_boost(&self) -> bool {
        (self.feature_set & 0b0000_0001) != 0
    }

    #[must_use]
    pub const fn has_feature_thermal_modes(&self) -> bool {
        (self.feature_set & 0b0000_0010) != 0
    }

    #[must_use]
    pub const fn has_feature_auto_boost(&self) -> bool {
        (self.feature_set & 0b0000_0100) != 0
    }

    #[must_use]
    pub const fn has_feature_cpu_temp(&self) -> bool {
        (self.feature_set & 0b0000_1000) != 0
    }

    #[must_use]
    pub const fn has_feature_gpu_temp(&self) -> bool {
        (self.feature_set & 0b0001_0000) != 0
    }

    #[must_use]
    pub const fn has_feature_brightness_control(&self) -> bool {
        (self.feature_set & 0b0010_0000) != 0
    }

    #[must_use]
    pub const fn has_feature_gmode_toggle(&self) -> bool {
        (self.feature_set & 0b0100_0000) != 0
    }

    #[must_use]
    pub const fn supports_thermal_control(&self) -> bool {
        (self.feature_set & 0b011) != 0
    }

    #[must_use]
    pub const fn supports_lighting_control(&self) -> bool {
        (self.feature_set & 0b0010_0000) != 0 || self.lighting_modes != 0
    }

    #[must_use]
    pub const fn supports_fan_control(&self) -> bool {
        (self.feature_set & 0b001) != 0
    }

    #[must_use]
    pub fn supports_thermal_mode(&self, mode: ThermalMode) -> bool {
        match mode.bitmask_bit() {
            Some(bit) => (self.thermal_modes & (1 << bit)) != 0,
            None => false,
        }
    }

    #[must_use]
    pub fn supported_thermal_modes(&self) -> Vec<ThermalMode> {
        ThermalMode::ALL
            .iter()
            .copied()
            .filter(|m| self.supports_thermal_mode(*m))
            .collect()
    }
}

/// Parsed device database and its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub source: DatabaseSource,
    pub devices: BTreeMap<String, DeviceEntry>,
}

impl Database {
    /// Parse a database from raw JSON and a source label.
    pub fn from_json_str(json: &str, source: DatabaseSource) -> Result<Self, CoreError> {
        let devices: BTreeMap<String, DeviceEntry> =
            serde_json::from_str(json).map_err(|err| CoreError::DatabaseParse {
                source_label: source.to_string(),
                details: err.to_string(),
            })?;
        Ok(Self { source, devices })
    }

    /// Load the compiled-in default database.
    pub fn load_embedded() -> Result<Self, CoreError> {
        Self::from_json_str(DEFAULT_DATABASE_JSON, DatabaseSource::EmbeddedDefault)
    }

    /// Resolve the database using the 4-tier precedence contract:
    /// 1. `AWCC_DATABASE_PATH` environment variable
    /// 2. `./database.json` in `working_dir` (or `../database.json`)
    /// 3. `/etc/awcc/database.json`
    /// 4. Compiled-in `DEFAULT_DATABASE_JSON`
    pub fn resolve_with_reader<F>(
        env_path: Option<&Path>,
        working_dir: &Path,
        mut read_file: F,
    ) -> Result<Self, CoreError>
    where
        F: FnMut(&Path) -> Option<String>,
    {
        if let Some(custom_path) = env_path
            && !custom_path.as_os_str().is_empty()
            && let Some(contents) = read_file(custom_path)
            && let Ok(db) =
                Self::from_json_str(&contents, DatabaseSource::EnvVar(custom_path.to_path_buf()))
        {
            return Ok(db);
        }

        let local_path = working_dir.join("database.json");
        if let Some(contents) = read_file(&local_path)
            && let Ok(db) =
                Self::from_json_str(&contents, DatabaseSource::LocalWorkingDir(local_path))
        {
            return Ok(db);
        }

        if let Some(parent_dir) = working_dir.parent() {
            let parent_path = parent_dir.join("database.json");
            if let Some(contents) = read_file(&parent_path)
                && let Ok(db) =
                    Self::from_json_str(&contents, DatabaseSource::LocalWorkingDir(parent_path))
            {
                return Ok(db);
            }
        }

        let etc_path = PathBuf::from("/etc/awcc/database.json");
        if let Some(contents) = read_file(&etc_path)
            && let Ok(db) = Self::from_json_str(&contents, DatabaseSource::SystemEtc(etc_path))
        {
            return Ok(db);
        }

        Self::load_embedded()
    }

    /// Look up a device by exact DMI product name, returning `(DeviceEntry, matched_in_database)`.
    #[must_use]
    pub fn lookup_device(&self, device_name: &str) -> (DeviceEntry, bool) {
        if let Some(entry) = self.devices.get(device_name) {
            (entry.clone(), true)
        } else {
            (DeviceEntry::fallback_default(), false)
        }
    }
}

/// Parse raw `/proc/acpi/call` output into a signed 32-bit response value.
///
/// Strips trailing `\n`, `\r`, `' '`, `\t`, and `\0`, parses `0x`/`0X` hexadecimal or unsigned
/// decimal input, rejects empty strings or trailing characters, and returns `None` on error.
#[must_use]
pub fn parse_acpi_output(raw_output: &str) -> Option<i32> {
    let trimmed = raw_output.trim_end_matches(['\n', '\r', ' ', '\t', '\0']);
    if trimmed.is_empty() || trimmed.starts_with(['+', '-']) {
        return None;
    }
    let parsed: u64 = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(hex, 16).ok()?
    } else {
        if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        trimmed.parse::<u64>().ok()?
    };
    #[allow(clippy::cast_possible_truncation)]
    Some(parsed as u32 as i32)
}

/// CPU vendor used as the fallback selector for ACPI prefix resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CpuVendor {
    Intel,
    Amd,
    Unknown,
}

impl fmt::Display for CpuVendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Intel => write!(f, "GenuineIntel"),
            Self::Amd => write!(f, "AuthenticAMD"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// ACPI object prefix used to form `\_SB.<PREFIX>.WMAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcpiPrefix {
    Amw0,
    Amw1,
    Amw3,
    Amww,
}

impl AcpiPrefix {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Amw0 => "AMW0",
            Self::Amw1 => "AMW1",
            Self::Amw3 => "AMW3",
            Self::Amww => "AMWW",
        }
    }

    #[must_use]
    pub fn wmax_path(self) -> String {
        format!(r"\_SB.{}.WMAX", self.as_str())
    }
}

impl fmt::Display for AcpiPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resolve the ACPI WMAX object prefix for a DMI `product_name` and `CpuVendor`.
///
/// Device-specific matches take precedence over the CPU-vendor fallback.
#[must_use]
pub fn resolve_acpi_prefix(device_name: &str, cpu_vendor: CpuVendor) -> Option<AcpiPrefix> {
    const AMW3_SUBSTRINGS: &[&str] = &[
        "Dell G15 5515",
        "Alienware m15 R7 AMD",
        "Alienware m15 Ryzen Ed. R5",
        "Alienware m15 Ryzen Edition R5",
    ];
    const AMWW_SUBSTRINGS: &[&str] = &[
        "Alienware m18 R1 AMD",
        "G5 5505",
        "Alienware m16 R1 AMD",
        "Alienware m15 R6",
        "Alienware m15 R7",
    ];
    const AMW1_SUBSTRINGS: &[&str] = &[
        "Alienware Aurora R9",
        "Alienware 17 R5",
        "G5 5590",
        "Alienware Aurora R16",
        "Alienware Area-51m R2",
        "Alienware Area-51m",
        "Alienware x17 R1",
        "G3 3590",
        "Alienware m15 R3",
        "Alienware Aurora R12",
        "Alienware m17 R2",
        "Alienware Aurora R7",
        "Alienware Aurora R15",
        "Alienware Aurora Ryzen Edition R14",
        "Alienware Aurora Ryzen Edition",
        "Alienware x15 R2",
        "Alienware m15 R4",
        "Alienware m17 R4",
        "Alienware m15",
        "Alienware x14",
        "Alienware Aurora R11",
        "Alienware Aurora R13",
        "Alienware x17 R2",
    ];

    if AMW3_SUBSTRINGS.iter().any(|s| device_name.contains(s)) {
        return Some(AcpiPrefix::Amw3);
    }
    if AMWW_SUBSTRINGS.iter().any(|s| device_name.contains(s)) {
        return Some(AcpiPrefix::Amww);
    }
    if AMW1_SUBSTRINGS.iter().any(|s| device_name.contains(s)) {
        return Some(AcpiPrefix::Amw1);
    }

    match cpu_vendor {
        CpuVendor::Intel => Some(AcpiPrefix::Amww),
        CpuVendor::Amd => Some(AcpiPrefix::Amw3),
        CpuVendor::Unknown => None,
    }
}

/// Hardware thermal modes supported by AWCC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ThermalMode {
    Quiet,
    Balanced,
    Performance,
    BatterySaver,
    Cool,
    FullSpeed,
    GMode,
    Manual,
}

impl ThermalMode {
    pub const ALL: [Self; 8] = [
        Self::Quiet,
        Self::Cool,
        Self::Balanced,
        Self::Performance,
        Self::FullSpeed,
        Self::BatterySaver,
        Self::GMode,
        Self::Manual,
    ];

    /// Bit index in the `database.json` `thermalModes` capability bitmask.
    #[must_use]
    pub const fn bitmask_bit(self) -> Option<u8> {
        match self {
            Self::Quiet => Some(0),
            Self::Balanced => Some(1),
            Self::Performance => Some(2),
            Self::BatterySaver => Some(3),
            Self::Cool => Some(4),
            Self::FullSpeed => Some(5),
            Self::GMode => Some(6),
            Self::Manual => Some(7),
        }
    }

    /// Stable integer encoding for a persisted `prev_mode` state file.
    #[must_use]
    pub const fn state_file_code(self) -> u8 {
        match self {
            Self::Manual => 0,
            Self::Balanced => 1,
            Self::Performance => 2,
            Self::Cool => 3,
            Self::Quiet => 4,
            Self::FullSpeed => 5,
            Self::BatterySaver => 6,
            Self::GMode => 7,
        }
    }

    /// Decode a thermal mode stored in a `prev_mode` state file.
    #[must_use]
    pub const fn from_state_file_code(val: i64) -> Option<Self> {
        match val {
            0 => Some(Self::Manual),
            1 => Some(Self::Balanced),
            2 => Some(Self::Performance),
            3 => Some(Self::Cool),
            4 => Some(Self::Quiet),
            5 => Some(Self::FullSpeed),
            6 => Some(Self::BatterySaver),
            7 => Some(Self::GMode),
            _ => None,
        }
    }

    #[must_use]
    pub const fn cli_code(self) -> &'static str {
        match self {
            Self::Quiet => "q",
            Self::Balanced => "b",
            Self::Performance => "p",
            Self::BatterySaver => "bs",
            Self::Cool => "c",
            Self::FullSpeed => "fs",
            Self::GMode => "g",
            Self::Manual => "m",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
            Self::BatterySaver => "Battery Saver",
            Self::Cool => "Cool",
            Self::FullSpeed => "Full Speed",
            Self::GMode => "G-Mode",
            Self::Manual => "Manual",
        }
    }
}

impl fmt::Display for ThermalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Explicit representation of the queried thermal mode state.
///
/// Guarantees that a failed or unmapped query (`Unknown`) is never conflated with `ThermalMode::Manual`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThermalModeState {
    Known(ThermalMode),
    Unknown {
        raw_code: Option<u32>,
        reason: String,
    },
}

impl ThermalModeState {
    #[must_use]
    pub const fn known_mode(&self) -> Option<ThermalMode> {
        match self {
            Self::Known(mode) => Some(*mode),
            Self::Unknown { .. } => None,
        }
    }

    #[must_use]
    pub fn display_label(&self) -> String {
        match self {
            Self::Known(mode) => mode.display_name().to_string(),
            Self::Unknown {
                raw_code: Some(code),
                ..
            } => format!("Unknown (0x{code:02X})"),
            Self::Unknown { raw_code: None, .. } => "Unknown (Query Failed)".to_string(),
        }
    }
}

/// Per-device thermal mode code table mapping `ThermalMode` to its WMAX `0x15` set (`u16`) and `0x14` get (`u16`) codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThermalCodeTable {
    set_codes: BTreeMap<ThermalMode, u16>,
    get_codes: BTreeMap<u16, ThermalMode>,
}

impl ThermalCodeTable {
    /// USTT code table (`0xa0`..`0xab`, `Manual = 0x00`) with device-specific set/query mappings.
    #[must_use]
    pub fn for_device(device_name: &str) -> Self {
        let mut set_codes = BTreeMap::from([
            (ThermalMode::Manual, 0x00),
            (ThermalMode::Balanced, 0xa0),
            (ThermalMode::Performance, 0xa1),
            (ThermalMode::Cool, 0xa2),
            (ThermalMode::Quiet, 0xa3),
            (ThermalMode::FullSpeed, 0xa4),
            (ThermalMode::BatterySaver, 0xa5),
            (ThermalMode::GMode, 0xab),
        ]);

        let mut get_codes = BTreeMap::from([
            (0x00, ThermalMode::Manual),
            (0x96, ThermalMode::Quiet),
            (0x97, ThermalMode::Balanced),
            (0x98, ThermalMode::Performance),
            (0x99, ThermalMode::FullSpeed),
            (0xa0, ThermalMode::Balanced),
            (0xa1, ThermalMode::Performance),
            (0xa2, ThermalMode::Cool),
            (0xa3, ThermalMode::Quiet),
            (0xa4, ThermalMode::FullSpeed),
            (0xa5, ThermalMode::BatterySaver),
            (0xab, ThermalMode::GMode),
        ]);

        // Some devices use different codes when setting and querying a profile.
        if device_name.contains("Alienware Aurora R9") {
            set_codes.insert(ThermalMode::Balanced, 0x197);
            get_codes.insert(0x97, ThermalMode::Balanced);
            set_codes.insert(ThermalMode::Performance, 0x198);
            get_codes.insert(0x98, ThermalMode::Performance);
        }
        if device_name.contains("Alienware 17 R5") {
            set_codes.insert(ThermalMode::Quiet, 0x196);
            get_codes.insert(0x96, ThermalMode::Quiet);
            set_codes.insert(ThermalMode::Balanced, 0x197);
            get_codes.insert(0x97, ThermalMode::Balanced);
            set_codes.insert(ThermalMode::Performance, 0x198);
            get_codes.insert(0x98, ThermalMode::Performance);
        }
        if device_name.contains("Dell G15 5515") {
            set_codes.insert(ThermalMode::Balanced, 0x97);
            get_codes.insert(0x97, ThermalMode::Balanced);
        }

        Self {
            set_codes,
            get_codes,
        }
    }

    /// Return the full 16-bit WMAX `0x15 0x01` set code for `mode` (e.g., `0x196` on Alienware 17 R5).
    #[must_use]
    pub fn set_code_for(&self, mode: ThermalMode) -> u16 {
        self.set_codes.get(&mode).copied().unwrap_or(0xa0)
    }

    /// Return the low byte of the set code for `mode` (used by standard USTT 8-bit callers).
    #[must_use]
    pub fn code_for(&self, mode: ThermalMode) -> u8 {
        #[allow(clippy::cast_possible_truncation)]
        let low = self.set_code_for(mode) as u8;
        low
    }

    /// Decode a raw WMAX `0x14 0x0b` response into a `ThermalModeState`.
    ///
    /// Notice that `>= 0xFFFFFFFD` or `> 0xFF` or unmapped codes (like `0xFE`) produce
    /// `ThermalModeState::Unknown`, never falling through to `ThermalMode::Manual` (`0x00`).
    #[must_use]
    pub fn decode_query_response(&self, raw: u32) -> ThermalModeState {
        if raw >= 0xffff_fffd {
            return ThermalModeState::Unknown {
                raw_code: Some(raw),
                reason: "ACPI WMAX query returned error sentinel (>= 0xFFFFFFFD)".to_string(),
            };
        }

        if raw <= 0xff
            && let Ok(code16) = u16::try_from(raw)
            && let Some(mode) = self.get_codes.get(&code16)
        {
            return ThermalModeState::Known(*mode);
        }

        ThermalModeState::Unknown {
            raw_code: Some(raw),
            reason: format!("Unmapped WMAX thermal mode byte 0x{raw:02X}"),
        }
    }
}

/// Complete resolved profile for the active machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfile {
    pub device_name: String,
    pub smbios_system_id: String,
    pub cpu_vendor: CpuVendor,
    pub acpi_prefix: AcpiPrefix,
    pub matched_in_database: bool,
    pub database_source: DatabaseSource,
    pub entry: DeviceEntry,
    pub code_table: ThermalCodeTable,
}

impl DeviceProfile {
    /// Resolve a `DeviceProfile` from the given `Database`, DMI product name, SMBIOS ID, and CPU vendor.
    #[must_use]
    pub fn resolve(
        database: &Database,
        device_name: impl Into<String>,
        smbios_system_id: impl Into<String>,
        cpu_vendor: CpuVendor,
    ) -> Self {
        let device_name = device_name.into();
        let (entry, matched_in_database) = database.lookup_device(&device_name);
        let acpi_prefix = resolve_acpi_prefix(&device_name, cpu_vendor).unwrap_or(AcpiPrefix::Amww);
        let code_table = ThermalCodeTable::for_device(&device_name);

        Self {
            device_name,
            smbios_system_id: smbios_system_id.into(),
            cpu_vendor,
            acpi_prefix,
            matched_in_database,
            database_source: database.source.clone(),
            entry,
            code_table,
        }
    }

    /// Default validation target profile: `Alienware m15 R6` (`0x0A86`, `AMWW`, `GenuineIntel`).
    pub fn alienware_m15_r6() -> Result<Self, CoreError> {
        let db = Database::load_embedded()?;
        Ok(Self::resolve(
            &db,
            "Alienware m15 R6",
            "0x0A86",
            CpuVendor::Intel,
        ))
    }
}

/// Explicit sensor reading state (`Unavailable != 0`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SensorReading<T> {
    Valid {
        value: T,
        unit: &'static str,
    },
    Unavailable {
        reason: String,
    },
    /// A previous valid reading kept because the latest query failed. `age_secs` is the age of
    /// `last_value` when the snapshot was taken; `reason` is the error from the failed query.
    Stale {
        last_value: T,
        unit: &'static str,
        age_secs: u64,
        reason: String,
    },
}

impl<T: fmt::Display + Copy> SensorReading<T> {
    #[must_use]
    pub fn formatted_value(&self) -> String {
        match self {
            Self::Valid { value, unit } => format!("{value}{unit}"),
            Self::Unavailable { .. } => "--".to_string(),
            Self::Stale {
                last_value,
                unit,
                age_secs,
                ..
            } => format!("{last_value}{unit} (stale {age_secs}s)"),
        }
    }

    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Valid { .. })
    }

    #[must_use]
    pub const fn value_or(&self, fallback: T) -> T {
        match self {
            Self::Valid { value, .. } => *value,
            Self::Stale { last_value, .. } => *last_value,
            Self::Unavailable { .. } => fallback,
        }
    }

    /// The value only when it was read successfully by the latest query (never a stale or
    /// missing one). Use this for decisions and for confirming a write.
    #[must_use]
    pub const fn fresh_value(&self) -> Option<T> {
        match self {
            Self::Valid { value, .. } => Some(*value),
            Self::Stale { .. } | Self::Unavailable { .. } => None,
        }
    }

    /// The error context of a failed query (unavailable or stale), if any.
    #[must_use]
    pub fn problem(&self) -> Option<&str> {
        match self {
            Self::Valid { .. } => None,
            Self::Unavailable { reason } | Self::Stale { reason, .. } => Some(reason),
        }
    }
}

/// State that is not a numeric sensor: a boolean latch, a firmware report, and so on.
///
/// Distinguishes a value that has never been observed, one whose latest query failed, and a
/// last-known value that is explicitly stale. A failed query must never be rendered as a valid
/// `false`/`off`, and a missing diagnostic must never be replaced by a plausible default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Observation<T> {
    /// No query has been attempted yet.
    NotObserved,
    /// The latest query succeeded.
    Observed(T),
    /// The latest query failed and no earlier value exists.
    Unavailable { reason: String },
    /// The latest query failed; `last_value` is from `age_secs` seconds earlier.
    Stale {
        last_value: T,
        age_secs: u64,
        reason: String,
    },
}

impl<T> Observation<T> {
    /// The value only when the latest query succeeded.
    #[must_use]
    pub const fn fresh(&self) -> Option<&T> {
        match self {
            Self::Observed(value) => Some(value),
            Self::NotObserved | Self::Unavailable { .. } | Self::Stale { .. } => None,
        }
    }

    /// The best-known value, including an explicitly stale one.
    #[must_use]
    pub const fn best_known(&self) -> Option<&T> {
        match self {
            Self::Observed(value) => Some(value),
            Self::Stale { last_value, .. } => Some(last_value),
            Self::NotObserved | Self::Unavailable { .. } => None,
        }
    }

    #[must_use]
    pub const fn is_observed(&self) -> bool {
        matches!(self, Self::Observed(_))
    }

    #[must_use]
    pub const fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. })
    }

    /// The error context of a failed query (unavailable or stale), if any.
    #[must_use]
    pub fn problem(&self) -> Option<&str> {
        match self {
            Self::Unavailable { reason } | Self::Stale { reason, .. } => Some(reason),
            Self::Observed(_) | Self::NotObserved => None,
        }
    }
}

impl<T: Copy> Observation<T> {
    /// Label such as `on`, `unavailable (reason)`, or `off (stale 12s)` for a boolean-like value.
    #[must_use]
    pub fn describe_with(&self, render: impl Fn(T) -> String) -> String {
        match self {
            Self::Observed(value) => render(*value),
            Self::NotObserved => "not yet observed".to_string(),
            Self::Unavailable { reason } => format!("unavailable ({reason})"),
            Self::Stale {
                last_value,
                age_secs,
                reason,
            } => format!("{} (stale {age_secs}s: {reason})", render(*last_value)),
        }
    }
}

/// Snapshot of CPU & GPU thermal/fan telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelemetrySnapshot {
    pub cpu_temp_c: SensorReading<u16>,
    pub gpu_temp_c: SensorReading<u16>,
    pub cpu_fan_rpm: SensorReading<u16>,
    pub gpu_fan_rpm: SensorReading<u16>,
    pub cpu_boost_pct: SensorReading<u8>,
    pub gpu_boost_pct: SensorReading<u8>,
}

/// Lifecycle state of the most recent user-initiated control action.
///
/// The states are ordered by how much is actually known about the device:
///
/// * `Requested` — the request was queued by a client; nothing has executed yet.
/// * `Pending` — the service started executing the request; no transport result yet.
/// * `Accepted` — the transport reported that the write was delivered/accepted (ACPI return code
///   success, or a USB OUT transfer that completed in full), but the applied device state is
///   **not** confirmed. `caveat` says why (no readback exists, readback unavailable, device
///   acknowledgement missing, ...).
/// * `Confirmed` — accepted **and** an independent readback taken after the write matched the
///   requested value.
/// * `RolledBack` — a multi-step operation failed part-way and the earlier step was undone.
/// * `Failed` — the operation did not take effect, or the readback contradicts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationLifecycle {
    Idle,
    Requested { description: String },
    Pending { description: String },
    Accepted { summary: String, caveat: String },
    Confirmed { summary: String },
    RolledBack { summary: String, reason: String },
    Failed { summary: String, error: String },
}

impl OperationLifecycle {
    #[must_use]
    pub const fn badge_label(&self) -> &'static str {
        match self {
            Self::Idle => "READY",
            Self::Requested { .. } => "REQUESTED",
            Self::Pending { .. } => "APPLYING",
            Self::Accepted { .. } => "ACCEPTED",
            Self::Confirmed { .. } => "CONFIRMED",
            Self::RolledBack { .. } => "ROLLED BACK",
            Self::Failed { .. } => "FAILED",
        }
    }

    #[must_use]
    pub fn detail_message(&self) -> String {
        match self {
            Self::Idle => "Passive observation active — 0 unsolicited hardware writes.".to_string(),
            Self::Requested { description } | Self::Pending { description } => description.clone(),
            Self::Accepted { summary, caveat } => format!("{summary} — {caveat}"),
            Self::Confirmed { summary } => summary.clone(),
            Self::RolledBack { summary, reason } => format!("{summary} — {reason}"),
            Self::Failed { summary, error } => format!("{summary}: {error}"),
        }
    }

    #[must_use]
    pub const fn is_error_or_rollback(&self) -> bool {
        matches!(self, Self::RolledBack { .. } | Self::Failed { .. })
    }

    /// Map a terminal lifecycle onto the shared result contract used by the CLI and GUI.
    ///
    /// `Accepted` and `Confirmed` are successes (with different strength); `Failed` and
    /// `RolledBack` are errors. A lifecycle that is still `Idle`/`Requested`/`Pending` is *not* a
    /// success: it means no outcome was recorded for the operation, so reporting success would be
    /// an unsupported claim.
    pub fn into_result(self) -> Result<OperationSuccess, OperationError> {
        match self {
            Self::Confirmed { summary } => Ok(OperationSuccess {
                summary,
                strength: SuccessStrength::Confirmed,
            }),
            Self::Accepted { summary, caveat } => Ok(OperationSuccess {
                summary,
                strength: SuccessStrength::Accepted { caveat },
            }),
            Self::Failed { summary, error } => Err(OperationError::Failed { summary, error }),
            Self::RolledBack { summary, reason } => {
                Err(OperationError::RolledBack { summary, reason })
            }
            other @ (Self::Idle | Self::Requested { .. } | Self::Pending { .. }) => {
                Err(OperationError::NoOutcome {
                    state: other.badge_label(),
                })
            }
        }
    }
}

/// How strongly a successful operation is established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuccessStrength {
    /// Accepted and the post-write readback matched the request.
    Confirmed,
    /// Accepted by the transport; the applied state is not confirmed (see `caveat`).
    Accepted { caveat: String },
}

/// A successful operation outcome (`Accepted` or `Confirmed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationSuccess {
    pub summary: String,
    pub strength: SuccessStrength,
}

impl OperationSuccess {
    /// Caveat to show alongside the summary when the result is not device-confirmed.
    #[must_use]
    pub fn caveat(&self) -> Option<&str> {
        match &self.strength {
            SuccessStrength::Confirmed => None,
            SuccessStrength::Accepted { caveat } => Some(caveat),
        }
    }
}

/// A failed operation outcome. Always maps to a non-zero CLI exit status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OperationError {
    #[error("{summary}: {error}")]
    Failed { summary: String, error: String },
    #[error("{summary}: {reason}")]
    RolledBack { summary: String, reason: String },
    #[error("no outcome was recorded for the requested operation (lifecycle state: {state})")]
    NoOutcome { state: &'static str },
}

/// Deterministic simulation scenario selectable in the prototype UI and CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimScenario {
    /// Standard healthy Alienware m15 R6 (`AMWW`) responses.
    Normal,
    /// `WMAX 0x25 0x01 0x01` succeeds, then `WMAX 0x15 0x01 0xAB` returns `0xFFFFFFFF`,
    /// triggering automatic rollback `WMAX 0x25 0x01 0x00`.
    AcpiSetFailureRollback,
    /// `WMAX 0x14 0x0b` returns unmapped byte `0xFE`, proving `Unknown != Manual`.
    UnknownModeResponse,
    /// Sensor queries return `0xFFFFFFFF`, proving `Unavailable` renders `--` instead of `0`.
    SensorUnavailable,
}

impl SimScenario {
    pub const ALL: [Self; 4] = [
        Self::Normal,
        Self::AcpiSetFailureRollback,
        Self::UnknownModeResponse,
        Self::SensorUnavailable,
    ];

    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Normal => 0,
            Self::AcpiSetFailureRollback => 1,
            Self::UnknownModeResponse => 2,
            Self::SensorUnavailable => 3,
        }
    }

    #[must_use]
    pub const fn from_index(index: i32) -> Self {
        match index {
            1 => Self::AcpiSetFailureRollback,
            2 => Self::UnknownModeResponse,
            3 => Self::SensorUnavailable,
            _ => Self::Normal,
        }
    }

    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Normal => "Normal (m15 R6)",
            Self::AcpiSetFailureRollback => "Fault: G-Mode Rollback",
            Self::UnknownModeResponse => "Fault: Unknown (0xFE)",
            Self::SensorUnavailable => "Fault: Sensors Offline",
        }
    }
}

/// Validate a fan boost percentage (`0..=100`).
pub const fn validate_fan_boost(pct: u16) -> Result<u8, CoreError> {
    if pct <= 100 {
        #[allow(clippy::cast_possible_truncation)]
        Ok(pct as u8)
    } else {
        Err(CoreError::InvalidFanBoost(pct))
    }
}

/// Validate a CPU TCC Offset (`0..=15°C` reduction from TjMax).
pub const fn validate_tcc_offset(offset: i32) -> Result<u8, CoreError> {
    if offset >= 0 && offset <= 15 {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(offset as u8)
    } else {
        Err(CoreError::InvalidTccOffset(offset))
    }
}

/// Single control point on a custom temperature-to-fan-boost curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanCurvePoint {
    pub temp_c: u16,
    pub boost_pct: u8,
}

/// Custom temperature-to-fan-boost curve configuration with hysteresis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanCurveConfig {
    pub enabled: bool,
    pub hysteresis_c: u16,
    pub cpu_points: Vec<FanCurvePoint>,
    pub gpu_points: Vec<FanCurvePoint>,
}

impl Default for FanCurveConfig {
    fn default() -> Self {
        let default_pts = vec![
            FanCurvePoint {
                temp_c: 45,
                boost_pct: 0,
            },
            FanCurvePoint {
                temp_c: 60,
                boost_pct: 35,
            },
            FanCurvePoint {
                temp_c: 75,
                boost_pct: 70,
            },
            FanCurvePoint {
                temp_c: 85,
                boost_pct: 100,
            },
        ];
        Self {
            enabled: false,
            hysteresis_c: 3,
            cpu_points: default_pts.clone(),
            gpu_points: default_pts,
        }
    }
}

/// Piecewise-linear interpolation of target fan boost (`0..=100%`) for a given temperature (`°C`).
#[must_use]
pub fn interpolate_curve_boost(points: &[FanCurvePoint], temp_c: u16) -> u8 {
    if points.is_empty() {
        return 0;
    }
    let mut sorted = points.to_vec();
    sorted.sort_by_key(|p| p.temp_c);

    if temp_c <= sorted[0].temp_c {
        return sorted[0].boost_pct.min(100);
    }
    let last = sorted[sorted.len() - 1];
    if temp_c >= last.temp_c {
        return last.boost_pct.min(100);
    }

    for pair in sorted.windows(2) {
        let p0 = pair[0];
        let p1 = pair[1];
        if temp_c >= p0.temp_c && temp_c <= p1.temp_c {
            let d_temp = i32::from(p1.temp_c) - i32::from(p0.temp_c);
            if d_temp <= 0 {
                return p1.boost_pct.min(100);
            }
            let b0 = i32::from(p0.boost_pct.min(100));
            let b1 = i32::from(p1.boost_pct.min(100));
            let interp = b0 + ((i32::from(temp_c) - i32::from(p0.temp_c)) * (b1 - b0)) / d_temp;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            return interp.clamp(0, 100) as u8;
        }
    }
    last.boost_pct.min(100)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn embedded_database_parses_and_contains_alienware_m15_r6() {
        let profile = DeviceProfile::alienware_m15_r6().expect("embedded database should parse");
        assert!(profile.matched_in_database);
        assert_eq!(profile.device_name, "Alienware m15 R6");
        assert_eq!(profile.acpi_prefix, AcpiPrefix::Amww);
        assert_eq!(profile.acpi_prefix.wmax_path(), r"\_SB.AMWW.WMAX");
        assert_eq!(profile.entry.feature_set, 0b0111_1111);
        assert_eq!(profile.entry.thermal_modes, 0b1111_0111);
        assert_eq!(profile.entry.keyboard_zones, vec![1, 3, 4, 5, 6, 7, 0]);
        assert!(profile.entry.supports_thermal_mode(ThermalMode::Quiet));
        assert!(profile.entry.supports_thermal_mode(ThermalMode::Balanced));
        assert!(
            profile
                .entry
                .supports_thermal_mode(ThermalMode::Performance)
        );
        assert!(profile.entry.supports_thermal_mode(ThermalMode::Cool));
        assert!(profile.entry.supports_thermal_mode(ThermalMode::FullSpeed));
        assert!(profile.entry.supports_thermal_mode(ThermalMode::GMode));
        assert!(profile.entry.supports_thermal_mode(ThermalMode::Manual));
        assert!(
            !profile
                .entry
                .supports_thermal_mode(ThermalMode::BatterySaver)
        );
    }

    #[test]
    fn acpi_prefix_precedence_prevents_broad_m15_from_shadowing_m15_r6_or_r5_ryzen() {
        assert_eq!(
            resolve_acpi_prefix("Alienware m15 R6", CpuVendor::Intel),
            Some(AcpiPrefix::Amww)
        );
        assert_eq!(
            resolve_acpi_prefix("Alienware m15 Ryzen Ed. R5", CpuVendor::Amd),
            Some(AcpiPrefix::Amw3)
        );
        assert_eq!(
            resolve_acpi_prefix("Alienware m15 R2", CpuVendor::Intel),
            Some(AcpiPrefix::Amw1)
        );
        assert_eq!(
            resolve_acpi_prefix("G5 5590", CpuVendor::Intel),
            Some(AcpiPrefix::Amw1)
        );
        assert_eq!(
            resolve_acpi_prefix("Dell G15 5525", CpuVendor::Amd),
            Some(AcpiPrefix::Amw3)
        );
    }

    #[test]
    fn parse_acpi_output_handles_hex_and_invalid_input() {
        assert_eq!(parse_acpi_output("0xa0\n\0"), Some(0xa0));
        assert_eq!(parse_acpi_output("0x000000ab\0"), Some(0xab));
        assert_eq!(parse_acpi_output("0xffffffff\0"), Some(-1));
        assert_eq!(parse_acpi_output(""), None);
        assert_eq!(parse_acpi_output("not_hex"), None);
    }

    #[test]
    fn thermal_code_tables_keep_set_and_query_mappings_separate() {
        let r5 = ThermalCodeTable::for_device("Alienware 17 R5");
        assert_eq!(r5.set_code_for(ThermalMode::Quiet), 0x196);
        assert_eq!(r5.set_code_for(ThermalMode::Balanced), 0x197);
        assert_eq!(r5.set_code_for(ThermalMode::Performance), 0x198);
        assert_eq!(
            r5.decode_query_response(0x96),
            ThermalModeState::Known(ThermalMode::Quiet)
        );
        assert_eq!(
            r5.decode_query_response(0x97),
            ThermalModeState::Known(ThermalMode::Balanced)
        );

        let r9 = ThermalCodeTable::for_device("Alienware Aurora R9");
        assert_eq!(r9.set_code_for(ThermalMode::Balanced), 0x197);
        assert_eq!(r9.set_code_for(ThermalMode::Performance), 0x198);

        let g15 = ThermalCodeTable::for_device("Dell G15 5515");
        assert_eq!(g15.set_code_for(ThermalMode::Balanced), 0x97);
    }

    #[test]
    fn database_precedence_respects_four_tier_order() {
        let custom_json = r#"{
            "Alienware m15 R6": {
                "featureSet": "0000111",
                "thermalModes": "00000011",
                "lightingModes": "000001",
                "keyboardZones": ["0x0", "0x1", "0x200"]
            }
        }"#;

        let db = Database::resolve_with_reader(
            Some(Path::new("/tmp/custom_db.json")),
            Path::new("/work"),
            |path| {
                if path == Path::new("/tmp/custom_db.json") {
                    Some(custom_json.to_string())
                } else {
                    None
                }
            },
        )
        .expect("custom env db should parse");

        assert_eq!(
            db.source,
            DatabaseSource::EnvVar(PathBuf::from("/tmp/custom_db.json"))
        );
        let (entry, matched) = db.lookup_device("Alienware m15 R6");
        assert!(matched);
        assert_eq!(entry.thermal_modes, 3);
        assert_eq!(entry.keyboard_zones, vec![0, 1]);
    }

    #[test]
    fn unknown_thermal_query_never_aliases_manual_mode() {
        let table = ThermalCodeTable::for_device("Alienware m15 R6");
        assert_eq!(
            table.decode_query_response(0xa0),
            ThermalModeState::Known(ThermalMode::Balanced)
        );
        assert_eq!(
            table.decode_query_response(0x00),
            ThermalModeState::Known(ThermalMode::Manual)
        );

        let failed = table.decode_query_response(0xffff_ffff);
        assert!(matches!(
            failed,
            ThermalModeState::Unknown {
                raw_code: Some(0xffff_ffff),
                ..
            }
        ));
        assert_ne!(failed.known_mode(), Some(ThermalMode::Manual));

        let unmapped = table.decode_query_response(0xfe);
        assert!(matches!(
            unmapped,
            ThermalModeState::Unknown {
                raw_code: Some(0xfe),
                ..
            }
        ));
        assert_ne!(unmapped.known_mode(), Some(ThermalMode::Manual));
    }

    #[test]
    fn unavailable_sensor_reading_never_formats_as_zero() {
        let reading: SensorReading<u16> = SensorReading::Unavailable {
            reason: "ACPI 0x14 0x04 returned 0xFFFFFFFF".to_string(),
        };
        assert_eq!(reading.formatted_value(), "--");
        assert!(!reading.is_available());
    }

    #[test]
    fn tcc_offset_and_fan_curve_interpolation_work_as_expected() {
        assert_eq!(validate_tcc_offset(0), Ok(0));
        assert_eq!(validate_tcc_offset(15), Ok(15));
        assert_eq!(
            validate_tcc_offset(-1),
            Err(CoreError::InvalidTccOffset(-1))
        );
        assert_eq!(
            validate_tcc_offset(16),
            Err(CoreError::InvalidTccOffset(16))
        );

        let curve = FanCurveConfig::default();
        assert_eq!(interpolate_curve_boost(&curve.cpu_points, 35), 0);
        assert_eq!(interpolate_curve_boost(&curve.cpu_points, 60), 35);
        assert_eq!(interpolate_curve_boost(&curve.cpu_points, 75), 70);
        assert_eq!(interpolate_curve_boost(&curve.cpu_points, 80), 85);
        assert_eq!(interpolate_curve_boost(&curve.cpu_points, 92), 100);
    }
}
