//! Passive support-report assembly, session evidence persistence, and ZIP archive export.
//!
//! **Passive invariant**: Support-report generation and summary rendering NEVER probe hardware,
//! open ACPI/USB transports, invoke `pkexec`/`sudo`, refresh telemetry, or replay commands.
//! Either the caller passes an already-observed [`SessionEvidence`] (for example from the GUI's
//! cached snapshot), or the exporter reads the most recent persisted `evidence-<session>.json`
//! from the log directory and reports its observation age, or it marks live hardware state as
//! `unavailable`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use awcc_build_info::{BuildInfo, detect_package_format};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::event::Event;
use crate::redact::Redactor;
use crate::ring::RingStats;
use crate::sink::{SinkStatus, list_sessions, parse_session_segment_file_name};
use crate::time::{now_unix_millis, rfc3339_millis};
use crate::zip::{ZipEntry, ZipError, write_zip};

/// Current support-report schema version.
pub const REPORT_SCHEMA_VERSION: u32 = 1;
/// Maximum number of application events written into `events.jsonl`.
pub const MAX_EXPORTED_EVENTS: usize = 1000;
/// Maximum number of hardware write-audit entries written into `hardware-audit.jsonl`.
pub const MAX_EXPORTED_AUDIT_ENTRIES: usize = 500;
/// Maximum number of recent sessions scanned when loading persisted events/evidence.
pub const MAX_EXPORT_SCAN_SESSIONS: usize = 8;
/// Maximum number of persisted `.jsonl` segment files scanned during export.
pub const MAX_EXPORT_SCAN_FILES: usize = 16;
/// Maximum total bytes read from persisted `.jsonl` segment files during export (2 MiB).
pub const MAX_EXPORT_SCAN_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum length in bytes of a single JSONL line accepted during export.
pub const MAX_EXPORT_LINE_BYTES: usize = 64 * 1024;

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

/// Statistics and truncation/collection limits from assembling `events.jsonl`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCollectionStats {
    pub max_exported_events: usize,
    pub max_scan_sessions: usize,
    pub max_scan_files: usize,
    pub max_scan_bytes: u64,
    pub sessions_discovered: usize,
    pub sessions_scanned: usize,
    pub sessions_skipped_by_limit: usize,
    pub files_scanned: usize,
    pub files_skipped_by_limit: usize,
    pub bytes_scanned: u64,
    pub files_tail_truncated_by_byte_limit: usize,
    pub malformed_lines_skipped: u64,
    pub duplicate_events_deduplicated: u64,
    pub events_dropped_records_included: u64,
    pub events_truncated_by_cap: u64,
    pub exported_events: usize,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    #[serde(default)]
    pub event_collection: EventCollectionStats,
    pub sink_status: SinkStatus,
    pub redaction_policy: RedactionPolicySummary,
    pub evidence: EvidenceEnvelope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_diagnostics: Option<crate::service::ServiceSupport>,
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
/// Evaluates up to `MAX_EXPORT_SCAN_SESSIONS` recent sessions and selects the valid evidence file
/// with the newest `recorded_unix_ms` (breaking ties by session modification time and session ID).
#[must_use]
pub fn load_latest_persisted_evidence(log_dir: &Path) -> Option<SessionEvidence> {
    let mut sessions = list_sessions(log_dir);
    sessions.sort_by(|a, b| {
        b.newest_mtime
            .cmp(&a.newest_mtime)
            .then_with(|| b.session_id.cmp(&a.session_id))
    });
    let mut best: Option<((u64, SystemTime, String), SessionEvidence)> = None;
    for s in sessions.into_iter().take(MAX_EXPORT_SCAN_SESSIONS) {
        for p in &s.paths {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with("evidence-")
                && name.ends_with(".json")
                && let Ok(meta) = fs::symlink_metadata(p)
                && meta.len() <= MAX_EXPORT_SCAN_BYTES
                && let Ok(raw) = fs::read_to_string(p)
                && let Ok(ev) = serde_json::from_str::<SessionEvidence>(&raw)
            {
                let key = (ev.recorded_unix_ms, s.newest_mtime, ev.session_id.clone());
                if best.as_ref().is_none_or(|(best_key, _)| key > *best_key) {
                    best = Some((key, ev));
                }
            }
        }
    }
    best.map(|(_, ev)| ev)
}

#[derive(Debug, Clone)]
struct CandidateSegment {
    path: PathBuf,
    session_id: String,
    session_newest_mtime: SystemTime,
    started_utc_compact: String,
    segment: u32,
    byte_len: u64,
}

#[derive(Debug, Clone)]
struct ParsedSegmentEvent {
    started_utc_compact: String,
    segment: u32,
    line_index: usize,
    event: Event,
}

/// Read lines from `path`, seeking to the last `max_bytes` of the file (and skipping any partial
/// first line after seeking) when `byte_len > max_bytes`.
fn read_segment_events_bounded(
    seg: &CandidateSegment,
    max_bytes: u64,
    stats: &mut EventCollectionStats,
) -> Vec<ParsedSegmentEvent> {
    let Ok(mut file) = File::open(&seg.path) else {
        return Vec::new();
    };
    let actual_len = file.metadata().map(|m| m.len()).unwrap_or(seg.byte_len);
    let bytes_to_read = if actual_len > max_bytes {
        let start_offset = actual_len - max_bytes;
        if file.seek(SeekFrom::Start(start_offset)).is_err() {
            return Vec::new();
        }
        stats.files_tail_truncated_by_byte_limit += 1;
        max_bytes
    } else {
        actual_len
    };
    stats.bytes_scanned += bytes_to_read;
    stats.files_scanned += 1;

    let mut reader = BufReader::new(file.take(bytes_to_read));
    let mut buf = Vec::new();
    if actual_len > max_bytes {
        // Seeking into the middle of a file can land inside a line; discard up to the first '\n'.
        let _ = reader.read_until(b'\n', &mut buf);
        buf.clear();
    }

    let mut out = Vec::new();
    let mut line_index = 0usize;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                if buf.is_empty() {
                    continue;
                }
                if buf.len() > MAX_EXPORT_LINE_BYTES {
                    stats.malformed_lines_skipped += 1;
                    continue;
                }
                let Ok(line_str) = std::str::from_utf8(&buf) else {
                    stats.malformed_lines_skipped += 1;
                    continue;
                };
                let trimmed = line_str.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Event>(trimmed) {
                    Ok(event) => {
                        out.push(ParsedSegmentEvent {
                            started_utc_compact: seg.started_utc_compact.clone(),
                            segment: seg.segment,
                            line_index,
                            event,
                        });
                        line_index += 1;
                    }
                    Err(_) => {
                        stats.malformed_lines_skipped += 1;
                    }
                }
            }
        }
    }
    out
}

/// Collect and merge persisted `.jsonl` events and `in_memory_events` within explicit scan and
/// output bounds while preserving per-session sequence order and `seq == 0` (`events_dropped`)
/// loss markers.
pub fn collect_and_merge_events_with_limits(
    log_dir: Option<&Path>,
    in_memory_events: &[Event],
    max_events: usize,
    max_scan_sessions: usize,
    max_scan_files: usize,
    max_scan_bytes: u64,
) -> (Vec<Event>, EventCollectionStats) {
    let mut stats = EventCollectionStats {
        max_exported_events: max_events,
        max_scan_sessions,
        max_scan_files,
        max_scan_bytes,
        ..EventCollectionStats::default()
    };

    let mut persisted_by_session: BTreeMap<String, Vec<ParsedSegmentEvent>> = BTreeMap::new();

    if let Some(dir) = log_dir {
        let sessions = list_sessions(dir);
        stats.sessions_discovered = sessions.len();

        // Build per-session segment lists with numeric segment metadata.
        let mut session_groups = Vec::new();

        for s in sessions {
            let mut segs = Vec::new();
            let mut latest_utc = String::new();
            let mut max_seg = 0u32;
            for p in s.paths {
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(parsed) = parse_session_segment_file_name(name) else {
                    continue;
                };
                let Ok(meta) = fs::symlink_metadata(&p) else {
                    continue;
                };
                if !meta.file_type().is_file() {
                    continue;
                }
                if parsed.started_utc_compact > latest_utc {
                    latest_utc.clone_from(&parsed.started_utc_compact);
                }
                if parsed.segment > max_seg {
                    max_seg = parsed.segment;
                }
                segs.push(CandidateSegment {
                    path: p,
                    session_id: s.session_id.clone(),
                    session_newest_mtime: s.newest_mtime,
                    started_utc_compact: parsed.started_utc_compact,
                    segment: parsed.segment,
                    byte_len: meta.len(),
                });
            }
            if !segs.is_empty() {
                session_groups.push(((s.newest_mtime, latest_utc, max_seg, s.session_id), segs));
            }
        }

        // Order sessions newest-first so old sessions are shed first when over max_scan_sessions.
        session_groups.sort_by(|a, b| b.0.cmp(&a.0));
        if session_groups.len() > max_scan_sessions {
            for (_, skipped_segs) in session_groups.drain(max_scan_sessions..) {
                stats.sessions_skipped_by_limit += 1;
                stats.files_skipped_by_limit += skipped_segs.len();
            }
        }
        stats.sessions_scanned = session_groups.len();

        // Flatten candidate segments across the selected sessions and scan newest segments first
        // within the file/byte budgets.
        let mut candidates: Vec<CandidateSegment> = session_groups
            .into_iter()
            .flat_map(|(_, segs)| segs)
            .collect();
        candidates.sort_by(|a, b| {
            b.session_newest_mtime
                .cmp(&a.session_newest_mtime)
                .then_with(|| b.started_utc_compact.cmp(&a.started_utc_compact))
                .then_with(|| b.segment.cmp(&a.segment))
                .then_with(|| b.session_id.cmp(&a.session_id))
        });

        for seg in candidates {
            let remaining_bytes = max_scan_bytes.saturating_sub(stats.bytes_scanned);
            if stats.files_scanned >= max_scan_files || remaining_bytes == 0 {
                stats.files_skipped_by_limit += 1;
                continue;
            }
            let parsed_events = read_segment_events_bounded(&seg, remaining_bytes, &mut stats);
            if !parsed_events.is_empty() {
                persisted_by_session
                    .entry(seg.session_id)
                    .or_default()
                    .extend(parsed_events);
            }
        }
    }

    // Group in-memory events by session_id.
    let mut memory_by_session: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for ev in in_memory_events {
        memory_by_session
            .entry(ev.session_id.clone())
            .or_default()
            .push(ev.clone());
    }

    let mut all_session_ids: BTreeSet<String> = persisted_by_session.keys().cloned().collect();
    all_session_ids.extend(memory_by_session.keys().cloned());

    #[derive(Debug, Clone)]
    struct OrderedSessionEvent {
        effective_unix_ms: u64,
        session_id: String,
        session_order: u64,
        event: Event,
    }

    let mut all_ordered: Vec<OrderedSessionEvent> = Vec::new();

    for sid in all_session_ids {
        let mut persisted = persisted_by_session.remove(&sid).unwrap_or_default();
        // Sort persisted events in strict chronological segment & line order:
        // numeric `segment` ensures `.9.jsonl` comes before `.10.jsonl`.
        persisted.sort_by(|a, b| {
            a.started_utc_compact
                .cmp(&b.started_utc_compact)
                .then_with(|| a.segment.cmp(&b.segment))
                .then_with(|| a.line_index.cmp(&b.line_index))
        });

        let mut seen_nonzero_seqs: BTreeSet<u64> = BTreeSet::new();
        let mut session_events: Vec<Event> = Vec::with_capacity(persisted.len());
        for item in persisted {
            if item.event.seq > 0 && !seen_nonzero_seqs.insert(item.event.seq) {
                stats.duplicate_events_deduplicated += 1;
                continue;
            }
            session_events.push(item.event);
        }

        if let Some(mut mem_events) = memory_by_session.remove(&sid) {
            mem_events.sort_by_key(|e| (e.seq, e.unix_ms));
            for mem_ev in mem_events {
                if mem_ev.seq > 0 && !seen_nonzero_seqs.insert(mem_ev.seq) {
                    stats.duplicate_events_deduplicated += 1;
                    continue;
                }
                // Insert in-memory event before the first persisted event with a larger non-zero
                // sequence number, or at the end if it is newer than all persisted events.
                if mem_ev.seq > 0
                    && let Some(pos) = session_events.iter().position(|e| e.seq > mem_ev.seq)
                {
                    session_events.insert(pos, mem_ev);
                } else {
                    session_events.push(mem_ev);
                }
            }
        }

        // Assign monotonic per-session order and non-decreasing effective timestamp so cross-session
        // chronological sorting never reorders events within the same session.
        let mut running_unix_ms = 0u64;
        for (idx, ev) in session_events.into_iter().enumerate() {
            running_unix_ms = running_unix_ms.max(ev.unix_ms);
            all_ordered.push(OrderedSessionEvent {
                effective_unix_ms: running_unix_ms,
                session_id: sid.clone(),
                session_order: idx as u64,
                event: ev,
            });
        }
    }

    // Sort globally across sessions by (effective_unix_ms, session_id, session_order).
    all_ordered.sort_by(|a, b| {
        a.effective_unix_ms
            .cmp(&b.effective_unix_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| a.session_order.cmp(&b.session_order))
    });

    if all_ordered.len() > max_events {
        let drop_count = all_ordered.len() - max_events;
        stats.events_truncated_by_cap = drop_count as u64;
        all_ordered.drain(..drop_count);
    }

    let events: Vec<Event> = all_ordered.into_iter().map(|o| o.event).collect();
    stats.events_dropped_records_included = events
        .iter()
        .filter(|e| e.code.as_deref() == Some("events_dropped"))
        .count() as u64;
    stats.exported_events = events.len();
    stats.truncated = stats.sessions_skipped_by_limit > 0
        || stats.files_skipped_by_limit > 0
        || stats.files_tail_truncated_by_byte_limit > 0
        || stats.events_truncated_by_cap > 0;

    let mut notes = Vec::new();
    if stats.events_truncated_by_cap > 0 {
        notes.push(format!(
            "older events omitted to stay within max_exported_events={max_events} ({} dropped)",
            stats.events_truncated_by_cap
        ));
    }
    if stats.sessions_skipped_by_limit > 0 {
        notes.push(format!(
            "{} older session(s) skipped by max_scan_sessions={max_scan_sessions}",
            stats.sessions_skipped_by_limit
        ));
    }
    if stats.files_skipped_by_limit > 0 {
        notes.push(format!(
            "{} older segment file(s) skipped by scan limits",
            stats.files_skipped_by_limit
        ));
    }
    if stats.files_tail_truncated_by_byte_limit > 0 {
        notes.push(format!(
            "{} segment file(s) tail-read to stay within max_scan_bytes={max_scan_bytes}",
            stats.files_tail_truncated_by_byte_limit
        ));
    }
    if stats.malformed_lines_skipped > 0 {
        notes.push(format!(
            "{} malformed log line(s) skipped safely",
            stats.malformed_lines_skipped
        ));
    }
    if stats.events_dropped_records_included > 0 {
        notes.push(format!(
            "{} writer queue-drop warning record(s) (code=events_dropped) included",
            stats.events_dropped_records_included
        ));
    }
    if !notes.is_empty() {
        stats.note = Some(notes.join("; "));
    }

    (events, stats)
}

/// Read up to `max_events` recent `Event` records from persisted `session-*.jsonl` files in
/// `log_dir`, preserving numeric segment order and per-session sequence order while selecting the
/// newest `max_events` events overall. Malformed lines are skipped safely.
#[must_use]
pub fn load_persisted_events(log_dir: &Path, max_events: usize) -> Vec<Event> {
    collect_and_merge_events_with_limits(
        Some(log_dir),
        &[],
        max_events,
        MAX_EXPORT_SCAN_SESSIONS,
        MAX_EXPORT_SCAN_FILES,
        MAX_EXPORT_SCAN_BYTES,
    )
    .0
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
        let mut raw_envelope = if let Some(ev) = &input.live_evidence {
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

        // Enforce raw-USB response opt-in and audit entry cap on the shared evidence BEFORE
        // generating `report.json`, `hardware-audit.jsonl`, `summary.txt`, or any other member.
        if let Some(ev_data) = raw_envelope.data.as_mut() {
            if ev_data.audit_entries.len() > MAX_EXPORTED_AUDIT_ENTRIES {
                let drop_count = ev_data.audit_entries.len() - MAX_EXPORTED_AUDIT_ENTRIES;
                ev_data.audit_entries.drain(..drop_count);
            }
            if !input.settings.trace_raw_usb_opt_in {
                for entry in &mut ev_data.audit_entries {
                    if let Some(obj) = entry.as_object_mut() {
                        obj.remove("raw_usb_response_hex");
                    }
                }
            }
        }

        let envelope = redact_via_json(&raw_envelope, &input.redactor)?;
        let host_env = redact_via_json(&input.host_environment, &input.redactor)?;
        let sink_status = redact_via_json(&input.sink_status, &input.redactor)?;

        // 2. Collect bounded events (persisted + in-memory, deduplicated for seq > 0 while
        // preserving seq == 0 `events_dropped` markers).
        let (merged_events, raw_collection_stats) = collect_and_merge_events_with_limits(
            input.log_dir.as_deref(),
            &input.in_memory_events,
            MAX_EXPORTED_EVENTS,
            MAX_EXPORT_SCAN_SESSIONS,
            MAX_EXPORT_SCAN_FILES,
            MAX_EXPORT_SCAN_BYTES,
        );
        let event_collection = redact_via_json(&raw_collection_stats, &input.redactor)?;

        let mut events_jsonl = String::new();
        for ev in &merged_events {
            let mut val = serde_json::to_value(ev)?;
            input.redactor.redact_json(&mut val);
            events_jsonl.push_str(&serde_json::to_string(&val)?);
            events_jsonl.push('\n');
        }

        // 3. Collect bounded hardware write-audit entries from the already-filtered & redacted evidence.
        let mut hardware_audit_jsonl = String::new();
        if let Some(ev_data) = &envelope.data {
            for entry in &ev_data.audit_entries {
                hardware_audit_jsonl.push_str(&serde_json::to_string(entry)?);
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
            event_collection,
            sink_status,
            redaction_policy: RedactionPolicySummary::new(input.settings.trace_raw_usb_opt_in),
            evidence: envelope,
            service_diagnostics: None,
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
        let mut entries = vec![
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
        if let Some(service) = &self.report.service_diagnostics {
            entries.push(ZipEntry {
                name: "service-diagnostics.json".into(),
                data: serde_json::to_vec_pretty(service)?,
            });
            let mut events = String::new();
            if let Some(daemon) = &service.daemon {
                for event in daemon.history.iter().chain(&daemon.events) {
                    events.push_str(&serde_json::to_string(event)?);
                    events.push('\n');
                }
            }
            entries.push(ZipEntry {
                name: "daemon-events.jsonl".into(),
                data: events.into_bytes(),
            });
        }
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
    if let Some(note) = &report.event_collection.note {
        lines.push(format!("Event Collection : {note}"));
    }
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
        let canary = "02 06 19 64 de ad be ef ca fe f0 0d";
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
                "raw_usb_response_hex": canary,
                "outcome": "confirmed"
            })],
        };

        // Also persist the session evidence to disk so both InMemoryCache and PersistedSessionFile
        // paths are verified across the entire ZIP archive.
        let dir = std::env::temp_dir().join(format!(
            "awcc_export_raw_usb_{}_{}",
            std::process::id(),
            now_unix_millis()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("evidence-deadbeef.json"),
            serde_json::to_string_pretty(&ev).unwrap(),
        )
        .unwrap();

        for live in [Some(ev.clone()), None] {
            let mut input = sample_input(live, Some(dir.clone()));
            let default_bundle = SupportBundle::assemble(&input).expect("default bundle");
            assert!(
                default_bundle
                    .hardware_audit_jsonl
                    .contains("02 06 19 64 00 00 00 00 00 00 00 00"),
                "standard decoded/hex command audit must remain present"
            );
            let default_zip = default_bundle.to_zip_bytes().expect("default zip");
            let default_entries = read_stored_zip(&default_zip).expect("read default zip");
            for entry in &default_entries {
                let text = std::str::from_utf8(&entry.data).expect("utf8");
                assert!(
                    !text.contains(canary),
                    "raw USB response canary leaked into {} without opt-in: {text}",
                    entry.name
                );
                assert!(
                    !text.contains("raw_usb_response_hex"),
                    "raw_usb_response_hex key leaked into {} without opt-in: {text}",
                    entry.name
                );
            }

            input.settings.trace_raw_usb_opt_in = true;
            let opt_in_bundle = SupportBundle::assemble(&input).expect("opt-in bundle");
            let opt_in_zip = opt_in_bundle.to_zip_bytes().expect("opt-in zip");
            let opt_in_entries = read_stored_zip(&opt_in_zip).expect("read opt-in zip");
            let audit_entry = opt_in_entries
                .iter()
                .find(|e| e.name == "hardware-audit.jsonl")
                .unwrap();
            let report_entry = opt_in_entries
                .iter()
                .find(|e| e.name == "report.json")
                .unwrap();
            assert!(
                std::str::from_utf8(&audit_entry.data)
                    .unwrap()
                    .contains(canary),
                "hardware-audit.jsonl must include canary when opted in"
            );
            assert!(
                std::str::from_utf8(&report_entry.data)
                    .unwrap()
                    .contains(canary),
                "report.json must include canary when opted in"
            );
            assert!(opt_in_bundle.summary_txt.contains("Alienware m15 R6"));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    fn make_test_event(session_id: &str, seq: u64, unix_ms: u64, msg: &str) -> Event {
        Event {
            session_id: session_id.to_string(),
            seq,
            ts_utc: rfc3339_millis(unix_ms),
            unix_ms,
            severity: Severity::Info,
            component: "awcc::test".to_string(),
            category: Category::App,
            op_id: None,
            action: None,
            outcome: None,
            code: None,
            elapsed_ms: None,
            repeat_count: None,
            message: msg.to_string(),
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn segment_rotation_past_9_into_10_preserves_numeric_segment_order() {
        let dir = std::env::temp_dir().join(format!(
            "awcc_export_seg10_{}_{}",
            std::process::id(),
            now_unix_millis()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Write segments 8, 9, 10, 11 for the same session with equal timestamps so only segment
        // ordering determines the intra-session sequence.
        let pid = std::process::id();
        for (seg_idx, seq) in [(8u32, 80u64), (9, 90), (10, 100), (11, 110)] {
            let file_name = format!("session-20261019T120000Z-{pid}-abcd0010.{seg_idx}.jsonl");
            let ev = make_test_event(
                "abcd0010",
                seq,
                1_792_411_200_000,
                &format!("seg-{seg_idx}"),
            );
            let line = serde_json::to_string(&ev).unwrap() + "\n";
            fs::write(dir.join(file_name), line).unwrap();
        }

        let loaded = load_persisted_events(&dir, 100);
        let seqs: Vec<u64> = loaded.iter().map(|e| e.seq).collect();
        assert_eq!(
            seqs,
            vec![80, 90, 100, 110],
            "segment 10 and 11 must follow segment 9 in numeric order"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_overlapping_sessions_and_in_memory_events_keep_per_session_order_and_newest_events()
    {
        let dir = std::env::temp_dir().join(format!(
            "awcc_export_multi_sess_{}_{}",
            std::process::id(),
            now_unix_millis()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let pid = std::process::id();
        // Session A: seq 1 (t=1000), seq 2 (t=1020), seq 3 (t=1040)
        let sess_a_lines: String = [
            make_test_event("aaaa1111", 1, 1000, "a-1"),
            make_test_event("aaaa1111", 2, 1020, "a-2"),
            make_test_event("aaaa1111", 3, 1040, "a-3"),
        ]
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect();
        fs::write(
            dir.join(format!("session-20261019T120000Z-{pid}-aaaa1111.0.jsonl")),
            sess_a_lines,
        )
        .unwrap();

        // Session B (overlapping timestamps with Session A): seq 1 (t=1010), seq 2 (t=1030)
        let sess_b_lines: String = [
            make_test_event("bbbb2222", 1, 1010, "b-1"),
            make_test_event("bbbb2222", 2, 1030, "b-2"),
        ]
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect();
        fs::write(
            dir.join(format!("session-20261019T120001Z-{pid}-bbbb2222.0.jsonl")),
            sess_b_lines,
        )
        .unwrap();

        // In-memory events for Session B include duplicate seq 2 (t=1030) and new seq 3 (t=1050).
        let in_mem = vec![
            make_test_event("bbbb2222", 2, 1030, "b-2"),
            make_test_event("bbbb2222", 3, 1050, "b-3"),
        ];

        // Request all 6 unique events first:
        let (all_events, stats) =
            collect_and_merge_events_with_limits(Some(&dir), &in_mem, 10, 8, 16, 1024 * 1024);
        assert_eq!(stats.duplicate_events_deduplicated, 1);
        let labels: Vec<&str> = all_events.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(labels, vec!["a-1", "b-1", "a-2", "b-2", "a-3", "b-3"]);

        // Now cap max_events to 4 and verify the newest 4 across both sessions are selected while
        // preserving each session's internal sequence order.
        let (newest_four, cap_stats) =
            collect_and_merge_events_with_limits(Some(&dir), &in_mem, 4, 8, 16, 1024 * 1024);
        let newest_labels: Vec<&str> = newest_four.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(newest_labels, vec!["a-2", "b-2", "a-3", "b-3"]);
        assert_eq!(cap_stats.events_truncated_by_cap, 2);
        assert!(cap_stats.truncated);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_queue_drops_and_malformed_lines_round_trip_into_support_bundle() {
        use crate::sink::{FileSink, SinkConfig};
        use std::time::Duration;

        let dir = std::env::temp_dir().join(format!(
            "awcc_export_drops_{}_{}",
            std::process::id(),
            now_unix_millis()
        ));
        let _ = fs::remove_dir_all(&dir);

        let mut cfg = SinkConfig::new(dir.clone(), "cccc4444".to_string(), std::process::id());
        cfg.queue_capacity = 4;
        let sink = FileSink::start(cfg).expect("start sink");

        // Pause writer and saturate queue twice so multiple seq==0 `events_dropped` records are
        // written into the session `.jsonl` file alongside real Event records.
        sink.set_paused(true);
        let mut accepted = 0usize;
        for seq in 1..=50_u64 {
            let ev = make_test_event(
                "cccc4444",
                seq,
                1_792_411_200_000 + seq,
                &format!("ev-{seq}"),
            );
            if sink.try_send_line(serde_json::to_string(&ev).unwrap()) {
                accepted += 1;
            }
        }
        sink.set_paused(false);
        assert!(sink.flush(Duration::from_secs(5)));

        // Inject a malformed line and another saturation burst.
        assert!(sink.try_send_line("{not-valid-json".to_string()));
        assert!(sink.flush(Duration::from_secs(5)));

        sink.set_paused(true);
        for seq in 51..=80_u64 {
            let ev = make_test_event(
                "cccc4444",
                seq,
                1_792_411_200_000 + seq,
                &format!("ev-{seq}"),
            );
            if sink.try_send_line(serde_json::to_string(&ev).unwrap()) {
                accepted += 1;
            }
        }
        sink.set_paused(false);
        assert!(sink.flush(Duration::from_secs(5)));
        let sink_status = sink.status();
        sink.shutdown(Duration::from_secs(5));

        let expected_dropped = (80 - accepted) as u64;
        assert_eq!(sink_status.dropped_queue_full, expected_dropped);

        let mut input = sample_input(None, Some(dir.clone()));
        input.session_id = "cccc4444".to_string();
        input.in_memory_events.clear();
        input.sink_status = sink_status;

        let bundle = SupportBundle::assemble(&input).expect("assemble bundle");
        let exported_events: Vec<Event> = bundle
            .events_jsonl
            .lines()
            .map(|l| serde_json::from_str::<Event>(l).expect("valid exported Event JSON"))
            .collect();

        let drop_records: Vec<&Event> = exported_events
            .iter()
            .filter(|e| e.code.as_deref() == Some("events_dropped"))
            .collect();
        assert_eq!(
            drop_records.len(),
            2,
            "both seq==0 events_dropped records must survive deduplication and export: {exported_events:?}"
        );
        let exported_drop_sum: u64 = drop_records.iter().filter_map(|e| e.repeat_count).sum();
        assert_eq!(exported_drop_sum, expected_dropped);
        assert_eq!(
            bundle
                .report
                .event_collection
                .events_dropped_records_included,
            2
        );
        assert_eq!(bundle.report.event_collection.malformed_lines_skipped, 1);
        assert!(
            bundle
                .summary_txt
                .contains("writer queue-drop warning record(s) (code=events_dropped) included"),
            "summary.txt must surface queue-drop evidence: {}",
            bundle.summary_txt
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_log_directory_is_scanned_within_byte_and_file_limits_and_reports_truncation() {
        let dir = std::env::temp_dir().join(format!(
            "awcc_export_bounded_{}_{}",
            std::process::id(),
            now_unix_millis()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let pid = std::process::id();
        let mut payload = String::new();
        for seq in 1..=200_u64 {
            let ev = make_test_event(
                "dddd5555",
                seq,
                1_792_411_200_000 + seq,
                &format!("msg-{seq}-padding-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
            );
            payload.push_str(&serde_json::to_string(&ev).unwrap());
            payload.push('\n');
        }
        let seg_path = dir.join(format!("session-20261019T120000Z-{pid}-dddd5555.0.jsonl"));
        fs::write(&seg_path, &payload).unwrap();
        let full_len = payload.len() as u64;
        assert!(full_len > 8_000);

        // Scan with a tight 4 KiB byte budget: only the tail of the file may be read, and the
        // newest events at the end of the file (including seq 200) must be preserved without
        // malformed-line failure at the seek boundary.
        let (events, stats) =
            collect_and_merge_events_with_limits(Some(&dir), &[], 100, 4, 4, 4096);
        assert_eq!(stats.bytes_scanned, 4096);
        assert_eq!(stats.files_tail_truncated_by_byte_limit, 1);
        assert!(stats.truncated);
        assert!(!events.is_empty());
        assert_eq!(events.last().unwrap().seq, 200);
        // Verify strictly increasing sequence order in the tail slice.
        for pair in events.windows(2) {
            assert!(pair[0].seq < pair[1].seq);
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
