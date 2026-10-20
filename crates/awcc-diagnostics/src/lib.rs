//! Shared structured diagnostics, bounded event rings, rotating session logs, and passive
//! support-report export for `awcc-cli` and `awcc-gui`.

pub mod event;
pub mod export;
pub mod hash;
pub mod redact;
pub mod ring;
pub mod sink;
pub mod time;
pub mod zip;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

pub use event::{Category, Event, Severity};
pub use export::{
    BuildIdentityReport, DatabaseProvenance, DeviceProfileEvidence, EventCollectionStats,
    EvidenceEnvelope, EvidenceOrigin, ExportError, ExportedSettings, HostEnvironment,
    REPORT_SCHEMA_VERSION, RedactionPolicySummary, SessionEvidence, SubsystemHealthEvidence,
    SupportBundle, SupportExportInput, SupportReport, TelemetryEvidence,
    collect_and_merge_events_with_limits, load_latest_persisted_evidence, load_persisted_events,
};
pub use redact::{REDACTED, Redactor};
pub use ring::{Delta, HasSeq, RingStats, SeqRing};
pub use sink::{
    DEFAULT_MAX_FILES_PER_SESSION, DEFAULT_PER_FILE_BYTES, DEFAULT_TOTAL_BUDGET_BYTES, FileSink,
    ParsedSegmentFileName, SinkConfig, SinkError, SinkStatus, cleanup_logs, default_log_dir,
    list_sessions, parse_session_segment_file_name, segment_path_sort_key,
};

/// Default bounded in-memory event ring capacity per process.
pub const DEFAULT_EVENT_RING_CAPACITY: usize = 512;
/// Default bounded hardware write-audit ring capacity.
pub const DEFAULT_AUDIT_RING_CAPACITY: usize = 256;
/// Default duration of temporary `DEBUG` diagnostic recording (5 minutes).
pub const DEFAULT_RECORDING_DURATION: Duration = Duration::from_secs(300);
/// Default rate-limit folding window for repeated identical failure events.
pub const DEFAULT_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LogConfigError {
    #[error("invalid log level '{value}': expected one of error, warn, info, debug, trace")]
    InvalidLevel { value: String },
}

/// Resolved log level and how it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLogLevel {
    pub level: Severity,
    pub source: &'static str,
    /// Populated when `AWCC_LOG` contained an unrecognized value and we fell back to `INFO`.
    pub env_warning: Option<String>,
}

/// Parse a severity name (`error`, `warn`, `info`, `debug`, `trace`, case-insensitive).
pub fn parse_log_level(input: &str) -> Result<Severity, LogConfigError> {
    match input.trim().to_ascii_lowercase().as_str() {
        "error" | "err" => Ok(Severity::Error),
        "warn" | "warning" => Ok(Severity::Warn),
        "info" => Ok(Severity::Info),
        "debug" => Ok(Severity::Debug),
        "trace" => Ok(Severity::Trace),
        other => Err(LogConfigError::InvalidLevel {
            value: other.to_string(),
        }),
    }
}

/// Resolve the base log level using the documented precedence:
/// 1. Explicit CLI `--log-level <level>` (invalid values return an error).
/// 2. Explicit CLI `-v` / `--verbose` flag (`DEBUG`).
/// 3. Environment variable `AWCC_LOG` (invalid values fall back to `INFO` and record a warning).
/// 4. Default (`INFO`).
pub fn resolve_log_level(
    cli_level: Option<&str>,
    cli_verbose: bool,
    env_awcc_log: Option<&str>,
) -> Result<ResolvedLogLevel, LogConfigError> {
    if let Some(raw) = cli_level {
        let level = parse_log_level(raw)?;
        return Ok(ResolvedLogLevel {
            level,
            source: "cli_log_level",
            env_warning: None,
        });
    }
    if cli_verbose {
        return Ok(ResolvedLogLevel {
            level: Severity::Debug,
            source: "cli_verbose",
            env_warning: None,
        });
    }
    if let Some(raw_env) = env_awcc_log {
        let trimmed = raw_env.trim();
        if !trimmed.is_empty() {
            match parse_log_level(trimmed) {
                Ok(level) => {
                    return Ok(ResolvedLogLevel {
                        level,
                        source: "env_awcc_log",
                        env_warning: None,
                    });
                }
                Err(_) => {
                    return Ok(ResolvedLogLevel {
                        level: Severity::Info,
                        source: "default_after_invalid_env",
                        env_warning: Some(format!(
                            "ignoring invalid AWCC_LOG='{trimmed}'; falling back to INFO (valid: error, warn, info, debug, trace)"
                        )),
                    });
                }
            }
        }
    }
    Ok(ResolvedLogLevel {
        level: Severity::Info,
        source: "default",
        env_warning: None,
    })
}

/// Resolve whether verbose raw USB transport response dumps are opted into via `--trace-raw-usb`
/// or `AWCC_TRACE_RAW_USB=1|true|yes|on`.
#[must_use]
pub fn resolve_raw_usb_opt_in(cli_flag: bool, env_val: Option<&str>) -> bool {
    if cli_flag {
        return true;
    }
    env_val.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Configuration for initializing a [`DiagnosticsRuntime`].
#[derive(Debug, Clone)]
pub struct DiagnosticsConfig {
    pub app_name: &'static str,
    pub resolved_level: ResolvedLogLevel,
    pub trace_raw_usb: bool,
    pub log_dir: Option<PathBuf>,
    pub enable_file_sink: bool,
    pub enable_stderr_console: bool,
    pub event_ring_capacity: usize,
    pub rate_limit_window: Duration,
}

impl DiagnosticsConfig {
    #[must_use]
    pub fn from_env_and_cli(
        app_name: &'static str,
        resolved_level: ResolvedLogLevel,
        trace_raw_usb: bool,
    ) -> Self {
        let env_dir = std::env::var("AWCC_LOG_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        Self {
            app_name,
            resolved_level,
            trace_raw_usb,
            log_dir: env_dir.or_else(default_log_dir),
            enable_file_sink: true,
            enable_stderr_console: true,
            event_ring_capacity: DEFAULT_EVENT_RING_CAPACITY,
            rate_limit_window: DEFAULT_RATE_LIMIT_WINDOW,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FailureKey {
    severity: Severity,
    component: String,
    code: Option<String>,
    message: String,
}

struct PendingFold {
    key: FailureKey,
    template: Event,
    suppressed_count: u64,
    first_suppressed_at: Instant,
}

#[derive(Default)]
struct RateLimiter {
    window: Duration,
    last_key_by_component: BTreeMap<String, (FailureKey, Instant)>,
    pending_by_component: BTreeMap<String, PendingFold>,
}

impl RateLimiter {
    fn new(window: Duration) -> Self {
        Self {
            window,
            last_key_by_component: BTreeMap::new(),
            pending_by_component: BTreeMap::new(),
        }
    }
}

/// Builder for emitting a structured [`Event`] directly or from the `tracing` layer.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub severity: Severity,
    pub component: String,
    pub category: Category,
    pub op_id: Option<u64>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    pub code: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

impl NewEvent {
    #[must_use]
    pub fn new(
        severity: Severity,
        component: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            component: component.into(),
            category: Category::App,
            op_id: None,
            action: None,
            outcome: None,
            code: None,
            elapsed_ms: None,
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn generate_session_id(pid: u32, started_ms: u64) -> String {
    let raw = format!("{pid}-{started_ms}-{:?}", std::thread::current().id());
    let digest = hash::sha256_hex(raw.as_bytes());
    digest[..12].to_string()
}

/// Shared runtime state for structured logging, temporary `DEBUG` recording, rate-limiting, and
/// support-report generation.
pub struct DiagnosticsRuntime {
    session_id: String,
    build_info: BuildIdentityReport,
    base_level: Severity,
    level_source: &'static str,
    recording_deadline: Mutex<Option<Instant>>,
    trace_raw_usb: AtomicBool,
    next_seq: AtomicU64,
    event_ring: Mutex<SeqRing<Event>>,
    rate_limiter: Mutex<RateLimiter>,
    sink: Option<FileSink>,
    sink_init_error: Option<String>,
    log_dir: Option<PathBuf>,
    stderr_enabled: bool,
    redactor: Redactor,
}

static GLOBAL_RUNTIME: OnceLock<Arc<DiagnosticsRuntime>> = OnceLock::new();

/// Access the process-wide [`DiagnosticsRuntime`] if initialized.
#[must_use]
pub fn global_diagnostics() -> Option<&'static Arc<DiagnosticsRuntime>> {
    GLOBAL_RUNTIME.get()
}

/// Initialize the process-wide `DiagnosticsRuntime` and register its `tracing` subscriber layer.
pub fn init_global_diagnostics(cfg: DiagnosticsConfig) -> Arc<DiagnosticsRuntime> {
    if let Some(existing) = GLOBAL_RUNTIME.get() {
        return Arc::clone(existing);
    }
    let runtime = DiagnosticsRuntime::new(cfg);
    let layer = DiagnosticsLayer::new(Arc::clone(&runtime));
    let subscriber = tracing_subscriber::registry().with(layer);
    let _ = tracing::subscriber::set_global_default(subscriber);
    let _ = GLOBAL_RUNTIME.set(Arc::clone(&runtime));
    runtime
}

impl DiagnosticsRuntime {
    /// Construct a new diagnostics runtime and emit initial startup/build-identity events.
    #[must_use]
    pub fn new(cfg: DiagnosticsConfig) -> Arc<Self> {
        let pid = std::process::id();
        let started_ms = time::now_unix_millis();
        let session_id = generate_session_id(pid, started_ms);
        let build_info = BuildIdentityReport::current(cfg.app_name);

        let mut sink_init_error = None;
        let sink = if cfg.enable_file_sink {
            if let Some(dir) = &cfg.log_dir {
                let sink_cfg = SinkConfig::new(dir.clone(), session_id.clone(), pid);
                match FileSink::start(sink_cfg) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        sink_init_error = Some(e.to_string());
                        None
                    }
                }
            } else {
                sink_init_error =
                    Some("could not determine XDG_STATE_HOME or HOME for session logs".to_string());
                None
            }
        } else {
            None
        };

        let runtime = Arc::new(Self {
            session_id,
            build_info: build_info.clone(),
            base_level: cfg.resolved_level.level,
            level_source: cfg.resolved_level.source,
            recording_deadline: Mutex::new(None),
            trace_raw_usb: AtomicBool::new(cfg.trace_raw_usb),
            next_seq: AtomicU64::new(1),
            event_ring: Mutex::new(SeqRing::new(cfg.event_ring_capacity)),
            rate_limiter: Mutex::new(RateLimiter::new(cfg.rate_limit_window)),
            sink,
            sink_init_error: sink_init_error.clone(),
            log_dir: cfg.log_dir,
            stderr_enabled: cfg.enable_stderr_console,
            redactor: Redactor::from_environment(),
        });

        // Emit startup & build identity immediately.
        let mut startup = NewEvent::new(
            Severity::Info,
            "awcc::startup",
            format!("starting {}", build_info.summary_line()),
        );
        startup.code = Some("startup".to_string());
        startup
            .fields
            .insert("version".to_string(), build_info.version.clone());
        startup
            .fields
            .insert("git_sha".to_string(), build_info.commit_short.clone());
        startup.fields.insert(
            "package_format".to_string(),
            build_info.package_format.clone(),
        );
        startup.fields.insert(
            "log_level".to_string(),
            cfg.resolved_level.level.as_str().to_string(),
        );
        startup.fields.insert(
            "log_level_source".to_string(),
            cfg.resolved_level.source.to_string(),
        );
        runtime.record(startup);

        if let Some(warn_msg) = cfg.resolved_level.env_warning {
            let mut ev = NewEvent::new(Severity::Warn, "awcc::diagnostics", warn_msg);
            ev.code = Some("invalid_log_env".to_string());
            runtime.record(ev);
        }

        if let Some(err_msg) = sink_init_error {
            let mut ev = NewEvent::new(
                Severity::Warn,
                "awcc::diagnostics",
                format!("file logging unavailable, continuing with in-memory ring only: {err_msg}"),
            );
            ev.code = Some("log_sink_unavailable".to_string());
            runtime.record(ev);
        }

        runtime
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn build_info(&self) -> &BuildIdentityReport {
        &self.build_info
    }

    #[must_use]
    pub fn base_level(&self) -> Severity {
        self.base_level
    }

    #[must_use]
    pub fn level_source(&self) -> &'static str {
        self.level_source
    }

    #[must_use]
    pub fn trace_raw_usb(&self) -> bool {
        self.trace_raw_usb.load(Ordering::Relaxed)
    }

    pub fn set_trace_raw_usb(&self, enabled: bool) {
        self.trace_raw_usb.store(enabled, Ordering::Relaxed);
    }

    /// Check whether temporary `DEBUG` recording is active, expiring it and restoring the base
    /// filter level if the deadline has passed.
    #[must_use]
    pub fn is_recording_active(&self) -> bool {
        let expired = {
            let mut slot = lock(&self.recording_deadline);
            if let Some(deadline) = *slot {
                if Instant::now() >= deadline {
                    *slot = None;
                    true
                } else {
                    return true;
                }
            } else {
                false
            }
        };
        if expired {
            let mut ev = NewEvent::new(
                Severity::Info,
                "awcc::diagnostics",
                format!(
                    "diagnostic DEBUG recording timed out; restored log filter to {}",
                    self.base_level.as_str()
                ),
            );
            ev.code = Some("diagnostic_recording_expired".to_string());
            self.record(ev);
        }
        false
    }

    /// Remaining seconds of temporary `DEBUG` recording, if active.
    #[must_use]
    pub fn recording_remaining_secs(&self) -> Option<u64> {
        if !self.is_recording_active() {
            return None;
        }
        let slot = lock(&self.recording_deadline);
        slot.map(|d| d.saturating_duration_since(Instant::now()).as_secs())
    }

    /// Effective log filter level right now (taking temporary `DEBUG` recording into account).
    #[must_use]
    pub fn active_level(&self) -> Severity {
        if self.is_recording_active() {
            self.base_level.max(Severity::Debug)
        } else {
            self.base_level
        }
    }

    /// Enable temporary `DEBUG` recording for `duration`. Never alters hardware polling or issues
    /// extra commands.
    pub fn start_recording(&self, duration: Duration) {
        let clamped = duration.max(Duration::from_millis(50));
        *lock(&self.recording_deadline) = Some(Instant::now() + clamped);
        let mut ev = NewEvent::new(
            Severity::Info,
            "awcc::diagnostics",
            format!(
                "started temporary DEBUG diagnostic recording for {}s (prior filter: {})",
                clamped.as_secs(),
                self.base_level.as_str()
            ),
        );
        ev.code = Some("diagnostic_recording_started".to_string());
        self.record(ev);
    }

    /// Stop temporary `DEBUG` recording and restore the prior log filter level immediately.
    pub fn stop_recording(&self) {
        let was_active = lock(&self.recording_deadline).take().is_some();
        if was_active {
            let mut ev = NewEvent::new(
                Severity::Info,
                "awcc::diagnostics",
                format!(
                    "stopped temporary DEBUG diagnostic recording; restored log filter to {}",
                    self.base_level.as_str()
                ),
            );
            ev.code = Some("diagnostic_recording_stopped".to_string());
            self.record(ev);
        }
    }

    /// Record a structured event if its severity passes `active_level()`, applying rate-limiting
    /// to repeated identical `WARN`/`ERROR` failures.
    pub fn record(&self, item: NewEvent) -> Option<u64> {
        if item.severity > self.active_level() {
            return None;
        }

        let now = Instant::now();
        let mut to_commit: Vec<Event> = Vec::new();
        {
            let mut rl = lock(&self.rate_limiter);
            let window = rl.window;

            // First flush any expired pending folds across components.
            let expired_components: Vec<String> = rl
                .pending_by_component
                .iter()
                .filter(|(_, p)| now.duration_since(p.first_suppressed_at) >= window)
                .map(|(c, _)| c.clone())
                .collect();
            for comp in expired_components {
                if let Some(pending) = rl.pending_by_component.remove(&comp) {
                    to_commit.push(Self::prepare_folded_event(pending));
                }
            }

            if matches!(item.severity, Severity::Error | Severity::Warn) {
                let key = FailureKey {
                    severity: item.severity,
                    component: item.component.clone(),
                    code: item.code.clone(),
                    message: item.message.clone(),
                };
                if let Some(pending) = rl.pending_by_component.get_mut(&item.component) {
                    if pending.key == key {
                        pending.suppressed_count += 1;
                        drop(rl);
                        let _ = self.publish_events(to_commit);
                        return None;
                    }
                    if let Some(finished) = rl.pending_by_component.remove(&item.component) {
                        to_commit.push(Self::prepare_folded_event(finished));
                    }
                } else if let Some((last_key, last_at)) =
                    rl.last_key_by_component.get(&item.component)
                    && *last_key == key
                    && now.duration_since(*last_at) < window
                {
                    let unix_ms = time::now_unix_millis();
                    let template = Event {
                        session_id: self.session_id.clone(),
                        seq: 0,
                        ts_utc: time::rfc3339_millis(unix_ms),
                        unix_ms,
                        severity: item.severity,
                        component: item.component.clone(),
                        category: item.category,
                        op_id: item.op_id,
                        action: item.action,
                        outcome: item.outcome,
                        code: item.code,
                        elapsed_ms: item.elapsed_ms,
                        repeat_count: None,
                        message: item.message,
                        fields: item.fields,
                    };
                    rl.pending_by_component.insert(
                        template.component.clone(),
                        PendingFold {
                            key,
                            template,
                            suppressed_count: 1,
                            first_suppressed_at: now,
                        },
                    );
                    drop(rl);
                    let _ = self.publish_events(to_commit);
                    return None;
                }
                rl.last_key_by_component
                    .insert(item.component.clone(), (key, now));
            } else if let Some(finished) = rl.pending_by_component.remove(&item.component) {
                to_commit.push(Self::prepare_folded_event(finished));
            }
        }

        let unix_ms = time::now_unix_millis();
        to_commit.push(Event {
            session_id: self.session_id.clone(),
            seq: 0,
            ts_utc: time::rfc3339_millis(unix_ms),
            unix_ms,
            severity: item.severity,
            component: item.component,
            category: item.category,
            op_id: item.op_id,
            action: item.action,
            outcome: item.outcome,
            code: item.code,
            elapsed_ms: item.elapsed_ms,
            repeat_count: None,
            message: item.message,
            fields: item.fields,
        });
        self.publish_events(to_commit)
    }

    fn prepare_folded_event(pending: PendingFold) -> Event {
        let unix_ms = time::now_unix_millis();
        let mut ev = pending.template;
        ev.seq = 0;
        ev.unix_ms = unix_ms;
        ev.ts_utc = time::rfc3339_millis(unix_ms);
        ev.repeat_count = Some(pending.suppressed_count);
        ev
    }

    /// Assign consecutive sequence numbers and push into `event_ring` inside a single critical
    /// section so concurrent producers can never insert out of sequence order, then emit to stderr
    /// and the background file sink after releasing the ring lock.
    fn publish_events(&self, mut events: Vec<Event>) -> Option<u64> {
        if events.is_empty() {
            return None;
        }
        let last_seq = {
            let mut ring = lock(&self.event_ring);
            let mut last = None;
            for ev in &mut events {
                let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
                ev.seq = seq;
                ring.push(ev.clone());
                last = Some(seq);
            }
            last
        };
        for ev in &events {
            if self.stderr_enabled
                && (self.level_source != "default"
                    || ev.severity == Severity::Error
                    || ev.code.as_deref() == Some("invalid_log_env"))
            {
                let _ = writeln!(std::io::stderr(), "{}", ev.render_line());
            }
            if let Some(sink) = &self.sink
                && let Ok(json_line) = serde_json::to_string(ev)
            {
                let _ = sink.try_send_line(json_line);
            }
        }
        last_seq
    }

    /// Flush any pending rate-limited failure folds immediately so snapshots/exports see the
    /// exact count.
    pub fn flush_rate_limits(&self) {
        let pending_list: Vec<Event> = {
            let mut rl = lock(&self.rate_limiter);
            let keys: Vec<String> = rl.pending_by_component.keys().cloned().collect();
            keys.into_iter()
                .filter_map(|k| rl.pending_by_component.remove(&k))
                .map(Self::prepare_folded_event)
                .collect()
        };
        let _ = self.publish_events(pending_list);
    }

    /// Sequence-aware delta of application events since `cursor`.
    #[must_use]
    pub fn events_since(&self, cursor: u64) -> Delta<Event> {
        self.flush_rate_limits();
        lock(&self.event_ring).since(cursor)
    }

    /// Snapshot of currently retained application events and ring statistics.
    #[must_use]
    pub fn events_snapshot(&self) -> (Vec<Event>, RingStats) {
        self.flush_rate_limits();
        let ring = lock(&self.event_ring);
        (ring.as_slice().to_vec(), ring.stats())
    }

    /// Clear currently visible in-memory events while preserving `lifetime_total` and recording
    /// the removal in `cleared`.
    pub fn clear_visible_events(&self) -> RingStats {
        self.flush_rate_limits();
        let mut ring = lock(&self.event_ring);
        ring.clear();
        ring.stats()
    }

    /// Current status of the background file sink.
    #[must_use]
    pub fn sink_status(&self) -> SinkStatus {
        if let Some(sink) = &self.sink {
            sink.status()
        } else {
            SinkStatus {
                enabled: false,
                directory: self.log_dir.as_ref().map(|d| d.display().to_string()),
                last_error: self.sink_init_error.clone(),
                ..SinkStatus::default()
            }
        }
    }

    /// Publish updated [`SessionEvidence`] to the background writer (`evidence-<session>.json`).
    pub fn publish_evidence(&self, evidence: &SessionEvidence) {
        if let Some(sink) = &self.sink
            && let Ok(json) = serde_json::to_string_pretty(evidence)
        {
            sink.publish_evidence(json);
        }
    }

    /// Build the allowlisted [`ExportedSettings`] snapshot.
    #[must_use]
    pub fn exported_settings(&self, extra: BTreeMap<String, String>) -> ExportedSettings {
        ExportedSettings {
            active_log_level: self.active_level().as_str().to_string(),
            base_log_level: self.base_level.as_str().to_string(),
            log_level_source: self.level_source.to_string(),
            diagnostic_recording_active: self.is_recording_active(),
            diagnostic_recording_remaining_secs: self.recording_remaining_secs(),
            trace_raw_usb_opt_in: self.trace_raw_usb(),
            log_retention_per_file_bytes: DEFAULT_PER_FILE_BYTES,
            log_retention_max_files_per_session: DEFAULT_MAX_FILES_PER_SESSION,
            log_retention_total_budget_bytes: DEFAULT_TOTAL_BUDGET_BYTES,
            database_override_env_set: std::env::var_os("AWCC_DATABASE_PATH").is_some(),
            extra,
        }
    }

    /// Assemble a passive [`SupportBundle`] without touching hardware.
    pub fn assemble_support_bundle(
        &self,
        live_evidence: Option<SessionEvidence>,
        extra_settings: BTreeMap<String, String>,
    ) -> Result<SupportBundle, ExportError> {
        if let Some(sink) = &self.sink {
            let _ = sink.flush(Duration::from_millis(250));
        }
        let (in_memory_events, event_ring_stats) = self.events_snapshot();
        let input = SupportExportInput {
            session_id: self.session_id.clone(),
            build_info: self.build_info.clone(),
            host_environment: HostEnvironment::collect(),
            settings: self.exported_settings(extra_settings),
            event_ring_stats,
            sink_status: self.sink_status(),
            in_memory_events,
            live_evidence,
            log_dir: self.log_dir.clone(),
            redactor: self.redactor.clone(),
        };
        SupportBundle::assemble(&input)
    }

    /// Flush and stop the background file writer.
    pub fn shutdown(&self, timeout: Duration) {
        self.flush_rate_limits();
        if let Some(sink) = &self.sink {
            sink.shutdown(timeout);
        }
    }
}

/// Format and record a panic as a redacted [`NewEvent`]. Exposed so tests can verify crash
/// evidence formatting and redaction without aborting the test process.
#[must_use]
pub fn build_panic_event(
    location: Option<(&str, u32, u32)>,
    payload_message: &str,
    backtrace_text: &str,
    build_info: &BuildIdentityReport,
    redactor: &Redactor,
) -> NewEvent {
    let loc_str = location
        .map(|(file, line, col)| redactor.redact_text(&format!("{file}:{line}:{col}")))
        .unwrap_or_else(|| "<unknown>".to_string());
    let clean_msg = redactor.redact_text(payload_message);
    let clean_bt = redactor.redact_text(backtrace_text);

    let mut ev = NewEvent::new(
        Severity::Error,
        "awcc::panic",
        format!("unhandled panic at {loc_str}: {clean_msg}"),
    );
    ev.code = Some("panic".to_string());
    ev.outcome = Some("aborted".to_string());
    ev.fields.insert("location".to_string(), loc_str);
    ev.fields
        .insert("build".to_string(), build_info.summary_line());
    // Bound backtrace length so a single crash event never blows up a JSONL line.
    let bt_trimmed: String = clean_bt.chars().take(4096).collect();
    if !bt_trimmed.trim().is_empty() {
        ev.fields.insert("backtrace".to_string(), bt_trimmed);
    }
    ev
}

/// Install the process panic hook that records redacted crash evidence to stderr and to the
/// session log before terminating.
pub fn install_panic_hook(runtime: Arc<DiagnosticsRuntime>) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|l| (l.file(), l.line(), l.column()));
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic payload".to_string()
        };
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let ev = build_panic_event(
            location,
            &payload,
            &bt,
            runtime.build_info(),
            &runtime.redactor,
        );
        runtime.record(ev);
        if let Some(sink) = &runtime.sink {
            let _ = sink.flush(Duration::from_millis(500));
        }
        prev(info);
    }));
}

/// `tracing_subscriber::Layer` that translates `tracing` events into structured [`NewEvent`]
/// records on a [`DiagnosticsRuntime`].
pub struct DiagnosticsLayer {
    runtime: Arc<DiagnosticsRuntime>,
}

impl DiagnosticsLayer {
    #[must_use]
    pub fn new(runtime: Arc<DiagnosticsRuntime>) -> Self {
        Self { runtime }
    }
}

#[derive(Default)]
struct EventVisitor {
    message: String,
    component: Option<String>,
    category: Category,
    op_id: Option<u64>,
    action: Option<String>,
    outcome: Option<String>,
    code: Option<String>,
    elapsed_ms: Option<u64>,
    fields: BTreeMap<String, String>,
}

impl Visit for EventVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "op_id" => self.op_id = Some(value),
            "elapsed_ms" => self.elapsed_ms = Some(value),
            other => {
                self.fields.insert(other.to_string(), value.to_string());
            }
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if value >= 0 {
            self.record_u64(field, value as u64);
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "component" => self.component = Some(value.to_string()),
            "category" => self.category = Category::parse(value),
            "action" => self.action = Some(value.to_string()),
            "outcome" => self.outcome = Some(value.to_string()),
            "code" => self.code = Some(value.to_string()),
            other => {
                self.fields.insert(other.to_string(), value.to_string());
            }
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let formatted = format!("{value:?}");
        let unquoted = formatted
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&formatted);
        self.record_str(field, unquoted);
    }
}

impl<S: Subscriber> Layer<S> for DiagnosticsLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let severity = Severity::from_tracing(*meta.level());
        if severity > self.runtime.active_level() {
            return;
        }
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let component = visitor
            .component
            .unwrap_or_else(|| meta.target().to_string());
        let message = if visitor.message.is_empty() {
            meta.name().to_string()
        } else {
            visitor.message
        };
        self.runtime.record(NewEvent {
            severity,
            component,
            category: visitor.category,
            op_id: visitor.op_id,
            action: visitor.action,
            outcome: visitor.outcome,
            code: visitor.code,
            elapsed_ms: visitor.elapsed_ms,
            message,
            fields: visitor.fields,
        });
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn test_runtime(base: Severity) -> Arc<DiagnosticsRuntime> {
        DiagnosticsRuntime::new(DiagnosticsConfig {
            app_name: "awcc-test",
            resolved_level: ResolvedLogLevel {
                level: base,
                source: "test",
                env_warning: None,
            },
            trace_raw_usb: false,
            log_dir: None,
            enable_file_sink: false,
            enable_stderr_console: false,
            event_ring_capacity: 64,
            rate_limit_window: Duration::from_secs(10),
        })
    }

    #[test]
    fn log_level_precedence_cli_beats_verbose_beats_env_beats_default() {
        // 1. Default is INFO.
        let def = resolve_log_level(None, false, None).unwrap();
        assert_eq!(def.level, Severity::Info);
        assert_eq!(def.source, "default");

        // 2. AWCC_LOG overrides default.
        let env = resolve_log_level(None, false, Some("warn")).unwrap();
        assert_eq!(env.level, Severity::Warn);
        assert_eq!(env.source, "env_awcc_log");

        // 3. Invalid AWCC_LOG falls back to INFO with an explicit warning.
        let bad_env = resolve_log_level(None, false, Some("loud")).unwrap();
        assert_eq!(bad_env.level, Severity::Info);
        assert!(bad_env.env_warning.is_some());

        // 4. -v / --verbose sets DEBUG and beats AWCC_LOG.
        let verbose = resolve_log_level(None, true, Some("error")).unwrap();
        assert_eq!(verbose.level, Severity::Debug);
        assert_eq!(verbose.source, "cli_verbose");

        // 5. Explicit --log-level beats -v and AWCC_LOG, and errors on invalid values.
        let explicit = resolve_log_level(Some("trace"), true, Some("error")).unwrap();
        assert_eq!(explicit.level, Severity::Trace);
        assert_eq!(explicit.source, "cli_log_level");
        assert!(resolve_log_level(Some("bogus"), false, None).is_err());
    }

    #[test]
    fn temporary_debug_recording_restores_prior_filter_on_stop_or_timeout() {
        let rt = test_runtime(Severity::Info);
        let subscriber =
            tracing_subscriber::registry().with(DiagnosticsLayer::new(Arc::clone(&rt)));

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(code = "hidden_before", "should be filtered out at INFO");
            assert_eq!(rt.events_snapshot().0.len(), 1, "only startup event");

            rt.start_recording(Duration::from_millis(80));
            assert!(rt.is_recording_active());
            tracing::debug!(
                op_id = 99_u64,
                code = "captured_during",
                "debug event captured during recording"
            );

            // Wait for auto-timeout.
            std::thread::sleep(Duration::from_millis(100));
            assert!(!rt.is_recording_active());
            assert_eq!(rt.active_level(), Severity::Info);

            tracing::debug!(code = "hidden_after", "filtered out again after timeout");
        });

        let (events, _) = rt.events_snapshot();
        assert!(
            events
                .iter()
                .any(|e| e.code.as_deref() == Some("captured_during") && e.op_id == Some(99))
        );
        assert!(
            events
                .iter()
                .any(|e| e.code.as_deref() == Some("diagnostic_recording_expired"))
        );
        assert!(
            !events
                .iter()
                .any(|e| e.code.as_deref() == Some("hidden_before"))
        );
        assert!(
            !events
                .iter()
                .any(|e| e.code.as_deref() == Some("hidden_after"))
        );
    }

    #[test]
    fn repeated_failures_are_rate_limited_with_repeat_count() {
        let rt = test_runtime(Severity::Info);
        for _ in 0..6 {
            let mut ev = NewEvent::new(Severity::Warn, "awcc::service", "fan rpm read timed out");
            ev.code = Some("telemetry_read_failed".to_string());
            rt.record(ev);
        }
        let (events, _) = rt.events_snapshot();
        let matching: Vec<&Event> = events
            .iter()
            .filter(|e| e.code.as_deref() == Some("telemetry_read_failed"))
            .collect();
        assert_eq!(
            matching.len(),
            2,
            "first immediate + one folded summary: {matching:?}"
        );
        assert_eq!(matching[0].repeat_count, None);
        assert_eq!(matching[1].repeat_count, Some(5));
    }

    #[test]
    fn panic_event_builder_redacts_paths_and_records_build_identity() {
        let info = BuildIdentityReport::current("awcc-test");
        let red = Redactor::new(
            Some("alice".to_string()),
            Some("/home/alice".to_string()),
            Some("alienbox".to_string()),
        );
        let ev = build_panic_event(
            Some(("/home/alice/src/main.rs", 42, 9)),
            "failed opening /home/alice/secret on alienbox",
            "0: awcc::main at /home/alice/src/main.rs:42",
            &info,
            &red,
        );
        assert_eq!(ev.code.as_deref(), Some("panic"));
        assert!(!ev.message.contains("alice") && !ev.message.contains("alienbox"));
        assert_eq!(ev.fields.get("location").unwrap(), "~/src/main.rs:42:9");
        assert!(!ev.fields.get("backtrace").unwrap().contains("alice"));
    }

    #[test]
    fn concurrent_producers_and_folded_events_publish_strictly_increasing_sequences_without_gaps() {
        let rt = DiagnosticsRuntime::new(DiagnosticsConfig {
            app_name: "awcc-test",
            resolved_level: ResolvedLogLevel {
                level: Severity::Debug,
                source: "test",
                env_warning: None,
            },
            trace_raw_usb: false,
            log_dir: None,
            enable_file_sink: false,
            enable_stderr_console: false,
            event_ring_capacity: 4096,
            rate_limit_window: Duration::from_secs(60),
        });

        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for worker_idx in 0..8usize {
                let rt = &rt;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    let comp = format!("awcc::worker_{}", worker_idx % 4);
                    for i in 0..120usize {
                        // Interleave repeated WARN failures (triggering rate-limit folding and
                        // fold-flush on subsequent INFO events) with INFO events and concurrent
                        // `events_since` reads.
                        let mut warn_ev =
                            NewEvent::new(Severity::Warn, comp.clone(), "transient bus timeout");
                        warn_ev.code = Some("bus_timeout".to_string());
                        let _ = rt.record(warn_ev);

                        if i % 3 == 0 {
                            let mut info_ev = NewEvent::new(
                                Severity::Info,
                                comp.clone(),
                                format!("recovered step {i}"),
                            );
                            info_ev.op_id = Some((worker_idx * 1000 + i) as u64);
                            let _ = rt.record(info_ev);
                        }
                        if i % 15 == 0 {
                            let delta = rt.events_since(0);
                            assert_eq!(
                                delta.missed, 0,
                                "no events may be reported missed while under ring capacity"
                            );
                        }
                    }
                });
            }
        });

        let delta = rt.events_since(0);
        assert_eq!(delta.missed, 0);
        assert_eq!(delta.items.len() as u64, delta.stats.lifetime_total);
        assert_eq!(delta.stats.last_seq, delta.stats.lifetime_total);
        for (idx, ev) in delta.items.iter().enumerate() {
            let expected_seq = (idx + 1) as u64;
            assert_eq!(
                ev.seq, expected_seq,
                "event ring sequences must be strictly consecutive 1..=N"
            );
        }
    }
}
