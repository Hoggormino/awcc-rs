//! Passive support-report assembly, session evidence persistence, and ZIP archive export.
//!
//! **Passive invariant**: Support-report generation and summary rendering NEVER probe hardware,
//! open ACPI/USB transports, invoke `pkexec`/`sudo`, refresh telemetry, or replay commands.
//! Either the caller passes an already-observed [`SessionEvidence`] (for example from the GUI's
//! cached snapshot), or the exporter reads the most recent persisted `evidence-<session>.json`
//! from the log directory and reports its observation age, or it marks live hardware state as
//! `unavailable`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use awcc_build_info::{BuildInfo, detect_package_format};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::event::Event;
use crate::redact::Redactor;
use crate::ring::RingStats;
use crate::sink::{SinkStatus, list_sessions};
use crate::time::{now_unix_millis, rfc3339_millis};
use crate::zip::{ZipEntry, ZipError, write_zip};

/// Current support-report schema version.
pub const REPORT_SCHEMA_VERSION: u32 = 1;
/// Maximum number of application events written into `events.jsonl`.
pub const MAX_EXPORTED_EVENTS: usize = 1000;
/// Maximum number of hardware write-audit entries written into `hardware-audit.jsonl`.
pub const MAX_EXPORTED_AUDIT_ENTRIES: usize = 500;

/// Serializable runtime build identity stored in `build-info.json` and `report.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentityReport {
    pub binary_name: String,
    pub version: String,
    pub commit: String,
    pub commit_short: String,
    pub commit_source: String,
    pub dirty: String,
    pub source_date_epoch: Option<u64>,
    pub target: String,
    pub profile: String,
    pub rustc: String,
    pub package_format: String,
}

impl BuildIdentityReport {
    #[must_use]
    pub fn current(binary_name: &str) -> Self {
        let info = BuildInfo::current();
        let pkg = detect_package_format();
        Self::from_build_info(binary_name, &info, pkg.as_str())
    }

    #[must_use]
    pub fn from_build_info(binary_name: &str, info: &BuildInfo, package_format: &str) -> Self {
        Self {
            binary_name: binary_name.to_string(),
            version: info.version.clone(),
            commit: info.commit.clone().unwrap_or_else(|| "unknown".to_string()),
            commit_short: info.short_commit().unwrap_or("unknown").to_string(),
            commit_source: info.commit_source.as_str().to_string(),
            dirty: info.dirty.as_str().to_string(),
            source_date_epoch: info.source_date_epoch,
            target: info.target.clone(),
            profile: info.profile.clone(),
            rustc: info.rustc.clone(),
            package_format: package_format.to_string(),
        }
    }

    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "{} {} ({}, dirty={}, format={}, target={}, {})",
            self.binary_name,
            self.version,
            self.commit_short,
            self.dirty,
            self.package_format,
            self.target,
            self.rustc
        )
    }
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("failed to serialize support report data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to package support archive: {0}")]
    Zip(#[from] ZipError),
    #[error("failed to write support archive to {path}: {reason}")]
    Io { path: String, reason: String },
}

/// Where the hardware/service evidence in a support report came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOrigin {
    /// Supplied directly from the running process's in-memory cache (e.g. GUI snapshot).
    InMemoryCache,
    /// Loaded passively from a persisted `evidence-<session>.json` file on disk.
    PersistedSessionFile,
    /// No active or persisted session evidence was available.
    Unavailable,
}

impl EvidenceOrigin {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InMemoryCache => "in_memory_cache",
            Self::PersistedSessionFile => "persisted_session_file",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Allowlisted device profile identity recorded in session evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfileEvidence {
    pub model_name: String,
    pub smbios_system_id: String,
    pub acpi_prefix: String,
    pub acpi_wmax_path: String,
    pub usb_vid_pid: String,
    pub profile_source: String,
    pub capabilities: Vec<String>,
}

/// Provenance and fingerprint of the loaded model database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseProvenance {
    /// E.g. `"compiled_in_default"`, `"system_etc_awcc"`, `"env_override"`, `"cwd_override"`.
    pub source_kind: String,
    /// Redacted path if loaded from disk, or `"<embedded>"`.
    pub source_label: String,
    /// SHA-256 hex digest of the database payload or profile definition.
    pub fingerprint_sha256: String,
    pub matched_device_name: Option<String>,
}

/// Per-subsystem availability and last error context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemHealthEvidence {
    pub backend_selection: String,
    pub acpi_wmax_available: bool,
    pub usb_elc_available: bool,
    pub hwmon_available: bool,
    pub worker_alive: bool,
    pub g_mode_recovery_pending: bool,
    pub last_error: Option<String>,
    pub usb_firmware_version: Option<String>,
    pub usb_status_summary: Option<String>,
}

/// Passive snapshot of last-known telemetry and settings state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryEvidence {
    pub thermal_mode: String,
    pub g_mode_state: String,
    pub stealth_lights_off: String,
    pub brightness_dim: Option<u8>,
    pub cpu_temp_c: Option<u8>,
    pub gpu_temp_c: Option<u8>,
    pub fan_summary: Vec<String>,
    pub telemetry_stale: bool,
    pub telemetry_age_ms: Option<u64>,
}

/// Complete passive session evidence persisted by a running service/GUI or CLI command and
/// consumed by `diagnostics export` / `diagnostics summary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvidence {
    pub session_id: String,
    pub recorded_at_utc: String,
    pub recorded_unix_ms: u64,
    pub build_info: BuildIdentityReport,
    pub device_profile: Option<DeviceProfileEvidence>,
    pub database_provenance: Option<DatabaseProvenance>,
    pub subsystem_health: SubsystemHealthEvidence,
    pub telemetry: Option<TelemetryEvidence>,
    pub audit_stats: RingStats,
    /// Recent mutating hardware audit records (already structured as JSON objects).
    #[serde(default)]
    pub audit_entries: Vec<Value>,
}

/// Envelope in `report.json` that wraps [`SessionEvidence`] with its origin and observation age at
/// export time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEnvelope {
    pub origin: EvidenceOrigin,
    pub observed_at_utc: Option<String>,
    pub observation_age_ms: Option<u64>,
    pub note: String,
    pub data: Option<SessionEvidence>,
}

/// Allowlisted Linux host/session metadata. Never includes hostnames, usernames, raw display
/// socket paths, or arbitrary environment variables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEnvironment {
    pub os_name: String,
    pub os_version: String,
    pub kernel_release: String,
    pub arch: String,
    pub desktop_session: String,
    pub display_protocol: String,
}

impl HostEnvironment {
    /// Collect allowlisted OS/kernel/session metadata from `/etc/os-release`, `/proc`, and safe
    /// session-type indicators.
    #[must_use]
    pub fn collect() -> Self {
        let (os_name, os_version) = read_os_release();
        let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let desktop_session = std::env::var("XDG_CURRENT_DESKTOP")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let display_protocol = std::env::var("XDG_SESSION_TYPE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                    "wayland".to_string()
                } else if std::env::var_os("DISPLAY").is_some() {
                    "x11".to_string()
                } else {
                    "headless".to_string()
                }
            });
        Self {
            os_name,
            os_version,
            kernel_release,
            arch: std::env::consts::ARCH.to_string(),
            desktop_session,
            display_protocol,
        }
    }
}

fn read_os_release() -> (String, String) {
    let Ok(content) = fs::read_to_string("/etc/os-release") else {
        return ("Linux".to_string(), "unknown".to_string());
    };
    let mut name = None;
    let mut version = None;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("NAME=") {
            name = Some(rest.trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(rest) = line.strip_prefix("VERSION_ID=") {
            version = Some(rest.trim_matches('"').trim_matches('\'').to_string());
        }
    }
    (
        name.unwrap_or_else(|| "Linux".to_string()),
        version.unwrap_or_else(|| "unknown".to_string()),
    )
}

/// Allowlisted diagnostic & application settings written to `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSettings {
    pub active_log_level: String,
    pub base_log_level: String,
    pub log_level_source: String,
    pub diagnostic_recording_active: bool,
    pub diagnostic_recording_remaining_secs: Option<u64>,
    pub trace_raw_usb_opt_in: bool,
    pub log_retention_per_file_bytes: u64,
    pub log_retention_max_files_per_session: usize,
    pub log_retention_total_budget_bytes: u64,
    pub database_override_env_set: bool,
    pub extra: BTreeMap<String, String>,
}

/// Documented raw-audit and privacy policy embedded into `report.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionPolicySummary {
    pub mode: &'static str,
    pub raw_usb_opt_in_enabled: bool,
    pub included_audit_fields: Vec<&'static str>,
    pub redacted_patterns: Vec<&'static str>,
    pub raw_payload_policy: &'static str,
}

impl RedactionPolicySummary {
    #[must_use]
    pub fn new(raw_usb_opt_in_enabled: bool) -> Self {
        Self {
            mode: "allowlist_with_identifier_scrubbing",
            raw_usb_opt_in_enabled,
            included_audit_fields: vec![
                "sequence",
                "op_id",
                "timestamp_unix_ms",
                "subsystem",
                "operation",
                "raw_command_or_packet_hex",
                "outcome",
                "ack_status",
            ],
            redacted_patterns: vec![
                "home_paths",
                "usernames",
                "hostnames",
                "service_tags_and_serials",
                "uuids",
                "mac_addresses",
                "email_addresses",
            ],
            raw_payload_policy: if raw_usb_opt_in_enabled {
                "Decoded/hex ELC 9-byte and 12-byte command envelopes and raw USB response bytes included via explicit --trace-raw-usb / AWCC_TRACE_RAW_USB opt-in; any trailing descriptor strings or serial fields are scrubbed."
            } else {
                "Standard decoded/hex command and packet audit included; verbose raw USB response dumps omitted unless --trace-raw-usb or AWCC_TRACE_RAW_USB=1 is enabled."
            },
        }
    }
}

/// Top-level `report.json` structure inside a support bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupportReport {
    pub schema_version: u32,
    pub generated_at_utc: String,
    pub generated_unix_ms: u64,
    pub export_mode: &'static str,
    pub build_info: BuildIdentityReport,
    pub host_environment: HostEnvironment,
    pub session_id: String,
    pub event_ring_stats: RingStats,
    pub sink_status: SinkStatus,
    pub redaction_policy: RedactionPolicySummary,
    pub evidence: EvidenceEnvelope,
}

/// Inputs used to build a passive support bundle or diagnostic summary.
#[derive(Debug, Clone)]
pub struct SupportExportInput {
    pub session_id: String,
    pub build_info: BuildIdentityReport,
    pub host_environment: HostEnvironment,
    pub settings: ExportedSettings,
    pub event_ring_stats: RingStats,
    pub sink_status: SinkStatus,
    pub in_memory_events: Vec<Event>,
    pub live_evidence: Option<SessionEvidence>,
    pub log_dir: Option<PathBuf>,
    pub redactor: Redactor,
}

/// Assembled, redacted files ready to be written into a `.zip` archive or inspected in tests.
#[derive(Debug, Clone)]
pub struct SupportBundle {
    pub summary_txt: String,
    pub report: SupportReport,
    pub report_json: String,
    pub events_jsonl: String,
    pub hardware_audit_jsonl: String,
    pub build_info_json: String,
    pub settings_json: String,
}

/// Read the newest persisted `evidence-<session>.json` from `log_dir` without modifying any files.
#[must_use]
pub fn load_latest_persisted_evidence(log_dir: &Path) -> Option<SessionEvidence> {
    let mut sessions = list_sessions(log_dir);
    sessions.sort_by_key(|b| std::cmp::Reverse(b.newest_mtime));
    for s in sessions {
        for p in &s.paths {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with("evidence-")
                && name.ends_with(".json")
                && let Ok(raw) = fs::read_to_string(p)
                && let Ok(ev) = serde_json::from_str::<SessionEvidence>(&raw)
            {
                return Some(ev);
            }
        }
    }
    None
}

/// Read up to `max_events` recent `Event` records from persisted `session-*.jsonl` files in
/// `log_dir`, ordered by `(unix_ms, seq)`. Malformed lines are skipped safely.
#[must_use]
pub fn load_persisted_events(log_dir: &Path, max_events: usize) -> Vec<Event> {
    let mut sessions = list_sessions(log_dir);
    sessions.sort_by_key(|s| s.newest_mtime);
    let mut events = Vec::new();
    for s in sessions {
        let mut jsonl_paths: Vec<PathBuf> = s
            .paths
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        jsonl_paths.sort();
        for p in jsonl_paths {
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            for line in text.lines() {
                if let Ok(ev) = serde_json::from_str::<Event>(line) {
                    events.push(ev);
                }
            }
        }
    }
    if events.len() > max_events {
        events.drain(..events.len() - max_events);
    }
    events
}

fn redact_via_json<T: Serialize + for<'de> Deserialize<'de>>(
    item: &T,
    redactor: &Redactor,
) -> Result<T, serde_json::Error> {
    let mut val = serde_json::to_value(item)?;
    redactor.redact_json(&mut val);
    serde_json::from_value(val)
}

impl SupportBundle {
    /// Assemble a complete, redacted support bundle passively from in-memory state and/or
    /// persisted session files in `input.log_dir`.
    pub fn assemble(input: &SupportExportInput) -> Result<Self, ExportError> {
        let now_ms = now_unix_millis();
        let now_utc = rfc3339_millis(now_ms);

        // 1. Resolve evidence envelope passively (live cache -> persisted session file -> unavailable).
        let raw_envelope = if let Some(ev) = &input.live_evidence {
            let age = now_ms.saturating_sub(ev.recorded_unix_ms);
            EvidenceEnvelope {
                origin: EvidenceOrigin::InMemoryCache,
                observed_at_utc: Some(ev.recorded_at_utc.clone()),
                observation_age_ms: Some(age),
                note: "Captured passively from in-memory service snapshot cache (no hardware commands issued during export).".to_string(),
                data: Some(ev.clone()),
            }
        } else if let Some(dir) = &input.log_dir
            && let Some(ev) = load_latest_persisted_evidence(dir)
        {
            let age = now_ms.saturating_sub(ev.recorded_unix_ms);
            EvidenceEnvelope {
                origin: EvidenceOrigin::PersistedSessionFile,
                observed_at_utc: Some(ev.recorded_at_utc.clone()),
                observation_age_ms: Some(age),
                note: format!(
                    "Loaded passively from persisted session {} (age {} ms; no hardware commands issued during export).",
                    ev.session_id, age
                ),
                data: Some(ev),
            }
        } else {
            EvidenceEnvelope {
                origin: EvidenceOrigin::Unavailable,
                observed_at_utc: None,
                observation_age_ms: None,
                note: "No live snapshot cache or persisted session evidence was found. Passive export did not probe hardware.".to_string(),
                data: None,
            }
        };

        let envelope = redact_via_json(&raw_envelope, &input.redactor)?;
        let host_env = redact_via_json(&input.host_environment, &input.redactor)?;
        let sink_status = redact_via_json(&input.sink_status, &input.redactor)?;

        // 2. Collect bounded events (persisted + in-memory, deduplicated by `(session_id, seq)`).
        let mut merged_events: Vec<Event> = Vec::new();
        if let Some(dir) = &input.log_dir {
            merged_events.extend(load_persisted_events(dir, MAX_EXPORTED_EVENTS));
        }
        for ev in &input.in_memory_events {
            if !merged_events
                .iter()
                .any(|e| e.session_id == ev.session_id && e.seq == ev.seq && ev.seq != 0)
            {
                merged_events.push(ev.clone());
            }
        }
        if merged_events.len() > MAX_EXPORTED_EVENTS {
            merged_events.drain(..merged_events.len() - MAX_EXPORTED_EVENTS);
        }

        let mut events_jsonl = String::new();
        for ev in &merged_events {
            let mut val = serde_json::to_value(ev)?;
            input.redactor.redact_json(&mut val);
            events_jsonl.push_str(&serde_json::to_string(&val)?);
            events_jsonl.push('\n');
        }

        // 3. Collect bounded hardware write-audit entries from the resolved evidence.
        let mut hardware_audit_jsonl = String::new();
        if let Some(ev_data) = &envelope.data {
            let entries = &ev_data.audit_entries;
            let start = entries.len().saturating_sub(MAX_EXPORTED_AUDIT_ENTRIES);
            for entry in &entries[start..] {
                let mut val = entry.clone();
                if !input.settings.trace_raw_usb_opt_in
                    && let Some(obj) = val.as_object_mut()
                {
                    obj.remove("raw_usb_response_hex");
                }
                input.redactor.redact_json(&mut val);
                hardware_audit_jsonl.push_str(&serde_json::to_string(&val)?);
                hardware_audit_jsonl.push('\n');
            }
        }

        // 4. Build `report.json`.
        let mut build_val = serde_json::to_value(&input.build_info)?;
        input.redactor.redact_json(&mut build_val);
        let redacted_build: BuildIdentityReport = serde_json::from_value(build_val)?;

        let report = SupportReport {
            schema_version: REPORT_SCHEMA_VERSION,
            generated_at_utc: now_utc,
            generated_unix_ms: now_ms,
            export_mode: "passive",
            build_info: redacted_build.clone(),
            host_environment: host_env,
            session_id: input.session_id.clone(),
            event_ring_stats: input.event_ring_stats,
            sink_status,
            redaction_policy: RedactionPolicySummary::new(input.settings.trace_raw_usb_opt_in),
            evidence: envelope,
        };
        let report_json = serde_json::to_string_pretty(&report)?;
        let build_info_json = serde_json::to_string_pretty(&redacted_build)?;

        let mut settings_val = serde_json::to_value(&input.settings)?;
        input.redactor.redact_json(&mut settings_val);
        let settings_json = serde_json::to_string_pretty(&settings_val)?;

        let summary_txt = input.redactor.redact_text(&format_summary_text(
            &report,
            &input.settings,
            merged_events.len(),
        ));

        Ok(Self {
            summary_txt,
            report,
            report_json,
            events_jsonl,
            hardware_audit_jsonl,
            build_info_json,
            settings_json,
        })
    }

    /// Encode this bundle as a bounded ZIP archive in memory.
    pub fn to_zip_bytes(&self) -> Result<Vec<u8>, ExportError> {
        let entries = [
            ZipEntry {
                name: "summary.txt".to_string(),
                data: self.summary_txt.as_bytes().to_vec(),
            },
            ZipEntry {
                name: "report.json".to_string(),
                data: self.report_json.as_bytes().to_vec(),
            },
            ZipEntry {
                name: "events.jsonl".to_string(),
                data: self.events_jsonl.as_bytes().to_vec(),
            },
            ZipEntry {
                name: "hardware-audit.jsonl".to_string(),
                data: self.hardware_audit_jsonl.as_bytes().to_vec(),
            },
            ZipEntry {
                name: "build-info.json".to_string(),
                data: self.build_info_json.as_bytes().to_vec(),
            },
            ZipEntry {
                name: "settings.json".to_string(),
                data: self.settings_json.as_bytes().to_vec(),
            },
        ];
        Ok(write_zip(&entries, self.report.generated_unix_ms)?)
    }

    /// Write the ZIP archive to `output_path` atomically (via `.tmp` + rename when in a normal
    /// directory).
    pub fn write_to_path(&self, output_path: &Path) -> Result<u64, ExportError> {
        let bytes = self.to_zip_bytes()?;
        if let Some(parent) = output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| ExportError::Io {
                path: parent.display().to_string(),
                reason: e.to_string(),
            })?;
        }
        fs::write(output_path, &bytes).map_err(|e| ExportError::Io {
            path: output_path.display().to_string(),
            reason: e.to_string(),
        })?;
        Ok(bytes.len() as u64)
    }
}

fn format_summary_text(
    report: &SupportReport,
    settings: &ExportedSettings,
    exported_event_count: usize,
) -> String {
    let mut lines = Vec::new();
    lines.push("AWCC Diagnostic Summary".to_string());
    lines.push("=======================".to_string());
    lines.push(format!("Schema Version   : {}", report.schema_version));
    lines.push(format!("Generated (UTC)  : {}", report.generated_at_utc));
    lines.push(format!(
        "Export Mode      : {} (zero hardware probes)",
        report.export_mode
    ));
    lines.push(format!(
        "Build Identity   : {}",
        report.build_info.summary_line()
    ));
    lines.push(format!(
        "Host Environment : {} {} · kernel {} · {} · {} ({})",
        report.host_environment.os_name,
        report.host_environment.os_version,
        report.host_environment.kernel_release,
        report.host_environment.arch,
        report.host_environment.desktop_session,
        report.host_environment.display_protocol,
    ));
    lines.push(format!(
        "Diagnostics      : session={} · level={} (base={}, source={}) · recording={} · raw_usb_opt_in={}",
        report.session_id,
        settings.active_log_level,
        settings.base_log_level,
        settings.log_level_source,
        if settings.diagnostic_recording_active { "active" } else { "off" },
        settings.trace_raw_usb_opt_in,
    ));
    lines.push(format!(
        "Event History    : lifetime={} · retained={} · evicted={} · cleared={} · exported={}",
        report.event_ring_stats.lifetime_total,
        report.event_ring_stats.retained,
        report.event_ring_stats.evicted,
        report.event_ring_stats.cleared,
        exported_event_count,
    ));
    lines.push(format!(
        "Log Sink         : enabled={} · bytes_written={} · rotations={} · dropped_queue={} · dropped_io={}",
        report.sink_status.enabled,
        report.sink_status.bytes_written,
        report.sink_status.rotations,
        report.sink_status.dropped_queue_full,
        report.sink_status.dropped_write_failed,
    ));
    if let Some(err) = &report.sink_status.last_error {
        lines.push(format!("Log Sink Warning : {err}"));
    }

    lines.push(String::new());
    lines.push(format!(
        "Hardware Evidence: origin={}",
        report.evidence.origin.as_str()
    ));
    if let (Some(ts), Some(age)) = (
        &report.evidence.observed_at_utc,
        report.evidence.observation_age_ms,
    ) {
        lines.push(format!("Observed At (UTC): {ts} (age {age} ms)"));
    }
    lines.push(format!("Evidence Note    : {}", report.evidence.note));

    if let Some(ev) = &report.evidence.data {
        if let Some(prof) = &ev.device_profile {
            lines.push(format!(
                "Device Profile   : {} · SMBIOS {} · ACPI {} ({}) · USB {} · source={}",
                prof.model_name,
                prof.smbios_system_id,
                prof.acpi_prefix,
                prof.acpi_wmax_path,
                prof.usb_vid_pid,
                prof.profile_source,
            ));
        } else {
            lines.push("Device Profile   : unavailable".to_string());
        }
        if let Some(db) = &ev.database_provenance {
            lines.push(format!(
                "Database         : kind={} · source={} · sha256={}",
                db.source_kind, db.source_label, db.fingerprint_sha256
            ));
        }
        let h = &ev.subsystem_health;
        lines.push(format!(
            "Subsystems       : backend={} · acpi={} · usb_elc={} · hwmon={} · worker_alive={} · gmode_recovery_pending={}",
            h.backend_selection,
            h.acpi_wmax_available,
            h.usb_elc_available,
            h.hwmon_available,
            h.worker_alive,
            h.g_mode_recovery_pending,
        ));
        if let Some(fw) = &h.usb_firmware_version {
            lines.push(format!("USB Firmware     : {fw}"));
        }
        if let Some(err) = &h.last_error {
            lines.push(format!("Last Service Err : {err}"));
        }
        if let Some(t) = &ev.telemetry {
            lines.push(format!(
                "Telemetry State  : mode={} · g_mode={} · stealth={} · brightness={:?} · stale={}",
                t.thermal_mode,
                t.g_mode_state,
                t.stealth_lights_off,
                t.brightness_dim,
                t.telemetry_stale,
            ));
        }
        lines.push(format!(
            "Hardware Audit   : lifetime={} · retained={} · evicted={} · cleared={}",
            ev.audit_stats.lifetime_total,
            ev.audit_stats.retained,
            ev.audit_stats.evicted,
            ev.audit_stats.cleared,
        ));
    }

    lines.join("\n") + "\n"
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::event::{Category, Severity};
    use crate::zip::read_stored_zip;
    use serde_json::json;

    fn sample_input(live: Option<SessionEvidence>, log_dir: Option<PathBuf>) -> SupportExportInput {
        SupportExportInput {
            session_id: "deadbeef".to_string(),
            build_info: BuildIdentityReport::current("awcc-test"),
            host_environment: HostEnvironment {
                os_name: "Ubuntu".to_string(),
                os_version: "24.04".to_string(),
                kernel_release: "6.8.0-generic".to_string(),
                arch: "x86_64".to_string(),
                desktop_session: "GNOME".to_string(),
                display_protocol: "wayland".to_string(),
            },
            settings: ExportedSettings {
                active_log_level: "INFO".to_string(),
                base_log_level: "INFO".to_string(),
                log_level_source: "default".to_string(),
                diagnostic_recording_active: false,
                diagnostic_recording_remaining_secs: None,
                trace_raw_usb_opt_in: false,
                log_retention_per_file_bytes: 5 * 1024 * 1024,
                log_retention_max_files_per_session: 4,
                log_retention_total_budget_bytes: 50 * 1024 * 1024,
                database_override_env_set: false,
                extra: BTreeMap::new(),
            },
            event_ring_stats: RingStats {
                capacity: 500,
                lifetime_total: 2,
                retained: 2,
                evicted: 0,
                cleared: 0,
                first_retained_seq: Some(1),
                last_seq: 2,
            },
            sink_status: SinkStatus {
                enabled: true,
                directory: Some("/home/alice/.local/state/awcc-rs/logs".to_string()),
                active_file: Some("session-1.jsonl".to_string()),
                bytes_written: 256,
                rotations: 0,
                files_removed_by_cleanup: 0,
                write_failures: 0,
                last_error: None,
                dropped_queue_full: 0,
                dropped_write_failed: 0,
            },
            in_memory_events: vec![Event {
                session_id: "deadbeef".to_string(),
                seq: 1,
                ts_utc: "2026-10-19T12:00:00.000Z".to_string(),
                unix_ms: 1_792_411_200_000,
                severity: Severity::Info,
                component: "awcc::service".to_string(),
                category: Category::App,
                op_id: Some(42),
                action: Some("SetThermalMode(Balanced)".to_string()),
                outcome: Some("confirmed".to_string()),
                code: None,
                elapsed_ms: Some(14),
                repeat_count: None,
                message: "loaded config from /home/alice/db.json on host alienbox".to_string(),
                fields: BTreeMap::from([("serial".to_string(), "SECRET123".to_string())]),
            }],
            live_evidence: live,
            log_dir,
            redactor: Redactor::new(
                Some("alice".to_string()),
                Some("/home/alice".to_string()),
                Some("alienbox".to_string()),
            ),
        }
    }

    #[test]
    fn passive_export_without_evidence_reports_unavailable_and_builds_valid_zip() {
        let input = sample_input(None, None);
        let bundle = SupportBundle::assemble(&input).expect("assemble");
        assert_eq!(bundle.report.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(bundle.report.export_mode, "passive");
        assert_eq!(bundle.report.evidence.origin, EvidenceOrigin::Unavailable);
        assert!(bundle.summary_txt.contains("origin=unavailable"));

        // Privacy check across every generated file:
        let zip_bytes = bundle.to_zip_bytes().expect("zip");
        let files = read_stored_zip(&zip_bytes).expect("valid zip");
        let names: Vec<&str> = files.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "summary.txt",
                "report.json",
                "events.jsonl",
                "hardware-audit.jsonl",
                "build-info.json",
                "settings.json"
            ]
        );
        for entry in &files {
            let text = std::str::from_utf8(&entry.data).expect("utf8");
            for secret in ["alice", "alienbox", "SECRET123"] {
                assert!(
                    !text.contains(secret),
                    "{secret} leaked in {}: {text}",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn raw_usb_response_bytes_require_explicit_opt_in() {
        let ev = SessionEvidence {
            session_id: "deadbeef".to_string(),
            recorded_at_utc: "2026-10-19T12:00:00.000Z".to_string(),
            recorded_unix_ms: 1_792_411_200_000,
            build_info: BuildIdentityReport::current("awcc-test"),
            device_profile: Some(DeviceProfileEvidence {
                model_name: "Alienware m15 R6".to_string(),
                smbios_system_id: "0x0A86".to_string(),
                acpi_prefix: "AMWW".to_string(),
                acpi_wmax_path: "\\_SB.AMWW.WMAX".to_string(),
                usb_vid_pid: "187c:0550".to_string(),
                profile_source: "database".to_string(),
                capabilities: vec!["g_mode".to_string()],
            }),
            database_provenance: None,
            subsystem_health: SubsystemHealthEvidence {
                backend_selection: "simulated".to_string(),
                acpi_wmax_available: true,
                usb_elc_available: true,
                hwmon_available: true,
                worker_alive: true,
                g_mode_recovery_pending: false,
                last_error: None,
                usb_firmware_version: Some("0.2.3".to_string()),
                usb_status_summary: None,
            },
            telemetry: None,
            audit_stats: RingStats::default(),
            audit_entries: vec![json!({
                "sequence": 1,
                "op_id": 7,
                "subsystem": "usb_elc",
                "operation": "dim_lighting",
                "raw_command_or_packet_hex": "02 06 19 64 00 00 00 00 00 00 00 00",
                "raw_usb_response_hex": "02 06 19 64 aa bb cc dd ee",
                "outcome": "confirmed"
            })],
        };

        let mut input = sample_input(Some(ev), None);
        let default_bundle = SupportBundle::assemble(&input).expect("default bundle");
        assert!(
            default_bundle
                .hardware_audit_jsonl
                .contains("02 06 19 64 00 00 00 00 00 00 00 00")
        );
        assert!(
            !default_bundle
                .hardware_audit_jsonl
                .contains("aa bb cc dd ee"),
            "verbose raw USB response must be omitted without opt-in"
        );

        input.settings.trace_raw_usb_opt_in = true;
        let opt_in_bundle = SupportBundle::assemble(&input).expect("opt-in bundle");
        assert!(
            opt_in_bundle
                .hardware_audit_jsonl
                .contains("aa bb cc dd ee"),
            "verbose raw USB response is included when explicitly opted in"
        );
        assert!(opt_in_bundle.summary_txt.contains("Alienware m15 R6"));
    }
}
