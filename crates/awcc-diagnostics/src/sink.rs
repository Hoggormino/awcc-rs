//! Rotating private session log files written by a background thread.
//!
//! Emitters never block on file I/O: lines go through a bounded queue with `try_send`. When the
//! queue is full the line is dropped and counted; when the directory is unwritable or the disk is
//! full the writer records the failure and keeps retrying in the background. None of this can
//! change the outcome of a control operation.
//!
//! Layout (under `$XDG_STATE_HOME/awcc-rs/logs`, default `~/.local/state/awcc-rs/logs`):
//! - `session-<utc>-<pid>-<session>.<n>.jsonl`   one JSON event per line, rotated by size
//! - `evidence-<session>.json`                    latest session evidence (atomic replace)
//!
//! Every process writes only its own session files (`create_new`, mode `0600`), so concurrent GUI
//! and CLI processes never truncate, rotate, or delete each other's active files. Cleanup removes
//! only *inactive* sessions, oldest first, to stay within the retained-log budget.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::{compact_utc, now_unix_millis};

/// Default bytes per log segment.
pub const DEFAULT_PER_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// Default maximum segments kept for one session (older segments of the *same* session are
/// removed when exceeded).
pub const DEFAULT_MAX_FILES_PER_SESSION: usize = 4;
/// Default retained-log budget across all sessions.
pub const DEFAULT_TOTAL_BUDGET_BYTES: u64 = 50 * 1024 * 1024;
/// Inactive sessions older than this are removed even when under budget.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// Default writer queue bound (events).
pub const DEFAULT_QUEUE_CAPACITY: usize = 4096;
/// A file modified within this window is considered active regardless of its pid.
const ACTIVE_MTIME_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum SinkError {
    #[error("cannot create or use the log directory {path}: {reason}")]
    Directory { path: String, reason: String },
}

#[derive(Debug, Clone)]
pub struct SinkConfig {
    pub dir: PathBuf,
    pub session_id: String,
    pub pid: u32,
    pub per_file_bytes: u64,
    pub max_files_per_session: usize,
    pub total_budget_bytes: u64,
    pub max_age: Duration,
    pub queue_capacity: usize,
}

impl SinkConfig {
    #[must_use]
    pub fn new(dir: PathBuf, session_id: String, pid: u32) -> Self {
        Self {
            dir,
            session_id,
            pid,
            per_file_bytes: DEFAULT_PER_FILE_BYTES,
            max_files_per_session: DEFAULT_MAX_FILES_PER_SESSION,
            total_budget_bytes: DEFAULT_TOTAL_BUDGET_BYTES,
            max_age: DEFAULT_MAX_AGE,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
        }
    }
}

/// Resolve the log directory: `$XDG_STATE_HOME/awcc-rs/logs`, else `~/.local/state/awcc-rs/logs`.
/// Returns `None` when neither base is an absolute path.
#[must_use]
pub fn default_log_dir() -> Option<PathBuf> {
    state_base().map(|b| b.join("awcc-rs").join("logs"))
}

fn state_base() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && xdg.starts_with('/')
    {
        return Some(PathBuf::from(xdg));
    }
    if let Ok(home) = std::env::var("HOME")
        && home.starts_with('/')
    {
        return Some(PathBuf::from(home).join(".local/state"));
    }
    None
}

/// Live status of the file sink, surfaced to the GUI and to exports (never hidden).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkStatus {
    pub enabled: bool,
    pub directory: Option<String>,
    pub active_file: Option<String>,
    pub bytes_written: u64,
    pub rotations: u64,
    pub files_removed_by_cleanup: u64,
    pub write_failures: u64,
    pub last_error: Option<String>,
    /// Lines dropped because the queue was full.
    pub dropped_queue_full: u64,
    /// Lines lost because the file could not be written.
    pub dropped_write_failed: u64,
}

enum Msg {
    Line(String),
    Flush(SyncSender<()>),
    #[cfg(test)]
    Pause(SyncSender<()>, mpsc::Receiver<()>),
    Shutdown,
}

struct Shared {
    status: Mutex<SinkStatus>,
    evidence: Mutex<Option<String>>,
    dropped_queue_full: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct FileSink {
    tx: SyncSender<Msg>,
    shared: Arc<Shared>,
    handle: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    pause_resume_tx: Mutex<Option<SyncSender<()>>>,
}

fn session_file_name(cfg: &SinkConfig, started_ms: u64, segment: u32) -> String {
    format!(
        "session-{}-{}-{}.{segment}.jsonl",
        compact_utc(started_ms),
        cfg.pid,
        cfg.session_id
    )
}

fn open_new_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn ensure_private_dir(dir: &Path) -> Result<(), SinkError> {
    let make = || -> std::io::Result<()> {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir)?;
        if !dir.is_dir() {
            return Err(std::io::Error::other("not a directory"));
        }
        Ok(())
    };
    make().map_err(|e| SinkError::Directory {
        path: dir.display().to_string(),
        reason: e.to_string(),
    })
}

struct Writer {
    cfg: SinkConfig,
    shared: Arc<Shared>,
    started_ms: u64,
    segment: u32,
    file: Option<File>,
    file_bytes: u64,
    own_segments: Vec<PathBuf>,
    retry_not_before: Option<Instant>,
    reported_queue_drops: u64,
    last_evidence_write: Instant,
    last_cleanup: Instant,
}

impl Writer {
    fn set_error(&self, err: &str) {
        let mut st = lock(&self.shared.status);
        st.write_failures += 1;
        st.last_error = Some(err.to_string());
    }

    fn open_segment(&mut self) -> bool {
        if let Some(until) = self.retry_not_before
            && Instant::now() < until
        {
            return false;
        }
        let path = self
            .cfg
            .dir
            .join(session_file_name(&self.cfg, self.started_ms, self.segment));
        let attempt = ensure_private_dir(&self.cfg.dir)
            .map_err(|e| e.to_string())
            .and_then(|()| open_new_private(&path).map_err(|e| e.to_string()));
        match attempt {
            Ok(file) => {
                self.file = Some(file);
                self.file_bytes = 0;
                self.retry_not_before = None;
                self.own_segments.push(path.clone());
                while self.own_segments.len() > self.cfg.max_files_per_session {
                    let old = self.own_segments.remove(0);
                    if fs::remove_file(&old).is_ok() {
                        lock(&self.shared.status).files_removed_by_cleanup += 1;
                    }
                }
                let mut st = lock(&self.shared.status);
                st.active_file = path.file_name().map(|n| n.to_string_lossy().into_owned());
                true
            }
            Err(reason) => {
                self.set_error(&reason);
                self.retry_not_before = Some(Instant::now() + Duration::from_secs(5));
                false
            }
        }
    }

    fn write_line(&mut self, line: &str) {
        if self.file.is_none() && !self.open_segment() {
            lock(&self.shared.status).dropped_write_failed += 1;
            return;
        }
        let bytes = line.len() as u64 + 1;
        if self.file_bytes > 0 && self.file_bytes + bytes > self.cfg.per_file_bytes {
            // Rotate: finish this segment and start the next one of *our* session.
            self.file = None;
            self.segment += 1;
            lock(&self.shared.status).rotations += 1;
            if !self.open_segment() {
                lock(&self.shared.status).dropped_write_failed += 1;
                return;
            }
            self.cleanup();
        }
        let result = self.file.as_mut().map(|f| {
            f.write_all(line.as_bytes())
                .and_then(|()| f.write_all(b"\n"))
        });
        match result {
            Some(Ok(())) => {
                self.file_bytes += bytes;
                let mut st = lock(&self.shared.status);
                st.bytes_written += bytes;
                st.last_error = None;
            }
            Some(Err(e)) => {
                // Disk full / I/O error: drop this line, retry with a fresh segment later.
                self.set_error(&e.to_string());
                lock(&self.shared.status).dropped_write_failed += 1;
                self.file = None;
                self.segment += 1;
                self.retry_not_before = Some(Instant::now() + Duration::from_secs(5));
            }
            None => {}
        }
    }

    fn report_queue_drops(&mut self) {
        let total = self.shared.dropped_queue_full.load(Ordering::Relaxed);
        if total > self.reported_queue_drops {
            let n = total - self.reported_queue_drops;
            self.reported_queue_drops = total;
            lock(&self.shared.status).dropped_queue_full = total;
            let unix_ms = now_unix_millis();
            let record = crate::event::Event {
                session_id: self.cfg.session_id.clone(),
                seq: 0,
                ts_utc: crate::time::rfc3339_millis(unix_ms),
                unix_ms,
                severity: crate::event::Severity::Warn,
                component: "awcc::diagnostics".to_string(),
                category: crate::event::Category::App,
                op_id: None,
                action: None,
                outcome: None,
                code: Some("events_dropped".to_string()),
                elapsed_ms: None,
                repeat_count: Some(n),
                message: format!(
                    "{n} event(s) were not written to the log file: writer queue was full"
                ),
                fields: std::collections::BTreeMap::new(),
            };
            if let Ok(line) = serde_json::to_string(&record) {
                self.write_line(&line);
            }
        }
    }

    fn write_evidence(&mut self) {
        let Some(text) = lock(&self.shared.evidence).take() else {
            return;
        };
        let final_path = self
            .cfg
            .dir
            .join(format!("evidence-{}.json", self.cfg.session_id));
        let tmp = self
            .cfg
            .dir
            .join(format!("evidence-{}.json.tmp", self.cfg.session_id));
        let _ = fs::remove_file(&tmp);
        let result = open_new_private(&tmp)
            .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| fs::rename(&tmp, &final_path));
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            self.set_error(&format!("evidence write failed: {e}"));
        }
        self.last_evidence_write = Instant::now();
    }

    fn cleanup(&mut self) {
        let removed = cleanup_logs(
            &self.cfg.dir,
            self.cfg.total_budget_bytes,
            self.cfg.max_age,
            &self.cfg.session_id,
        );
        lock(&self.shared.status).files_removed_by_cleanup += removed;
        self.last_cleanup = Instant::now();
    }

    fn run(mut self, rx: &mpsc::Receiver<Msg>) {
        self.cleanup();
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Msg::Line(line)) => {
                    self.report_queue_drops();
                    self.write_line(&line);
                }
                Ok(Msg::Flush(ack)) => {
                    self.report_queue_drops();
                    if let Some(f) = self.file.as_mut() {
                        let _ = f.flush();
                    }
                    self.write_evidence();
                    let _ = ack.send(());
                }
                #[cfg(test)]
                Ok(Msg::Pause(ack, resume_rx)) => {
                    let _ = ack.send(());
                    let _ = resume_rx.recv();
                }
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    self.report_queue_drops();
                    self.write_evidence();
                    if let Some(f) = self.file.as_mut() {
                        let _ = f.flush();
                        let _ = f.sync_all();
                    }
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.report_queue_drops();
                }
            }
            if self.last_evidence_write.elapsed() >= Duration::from_secs(2) {
                self.write_evidence();
            }
            if self.last_cleanup.elapsed() >= Duration::from_secs(300) {
                self.cleanup();
            }
        }
    }
}

impl FileSink {
    /// Start the writer thread. Fails (without side effects on the process) when the directory
    /// cannot be created or used.
    pub fn start(cfg: SinkConfig) -> Result<Self, SinkError> {
        ensure_private_dir(&cfg.dir)?;
        let shared = Arc::new(Shared {
            status: Mutex::new(SinkStatus {
                enabled: true,
                directory: Some(cfg.dir.display().to_string()),
                ..SinkStatus::default()
            }),
            evidence: Mutex::new(None),
            dropped_queue_full: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::sync_channel::<Msg>(cfg.queue_capacity.max(1));
        let writer = Writer {
            started_ms: now_unix_millis(),
            cfg,
            shared: Arc::clone(&shared),
            segment: 0,
            file: None,
            file_bytes: 0,
            own_segments: Vec::new(),
            retry_not_before: None,
            reported_queue_drops: 0,
            last_evidence_write: Instant::now(),
            last_cleanup: Instant::now(),
        };
        let handle = thread::Builder::new()
            .name("awcc-log-writer".to_string())
            .spawn(move || writer.run(&rx))
            .map_err(|e| SinkError::Directory {
                path: "writer thread".to_string(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            tx,
            shared,
            handle: Mutex::new(Some(handle)),
            #[cfg(test)]
            pause_resume_tx: Mutex::new(None),
        })
    }

    /// Queue one JSON line. Never blocks; returns `false` (and counts the drop) when the queue is
    /// full or the writer has stopped.
    pub fn try_send_line(&self, line: String) -> bool {
        match self.tx.try_send(Msg::Line(line)) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.shared
                    .dropped_queue_full
                    .fetch_add(1, Ordering::Relaxed);
                lock(&self.shared.status).dropped_queue_full =
                    self.shared.dropped_queue_full.load(Ordering::Relaxed);
                false
            }
        }
    }

    /// Replace the pending evidence document (latest wins); written by the writer thread.
    pub fn publish_evidence(&self, json: String) {
        *lock(&self.shared.evidence) = Some(json);
    }

    #[must_use]
    pub fn status(&self) -> SinkStatus {
        let mut st = lock(&self.shared.status).clone();
        st.dropped_queue_full = self.shared.dropped_queue_full.load(Ordering::Relaxed);
        st
    }

    /// Ask the writer to flush and wait up to `timeout`. Returns whether the flush completed.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (ack_tx, ack_rx) = mpsc::sync_channel::<()>(1);
        let mut msg = Msg::Flush(ack_tx);
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    msg = returned;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        ack_rx.recv_timeout(remaining).is_ok()
    }

    /// Flush and stop the writer, waiting up to `timeout`. Buffered events are written on a
    /// best-effort basis; abrupt termination (SIGKILL, power loss) can still lose them.
    pub fn shutdown(&self, timeout: Duration) {
        #[cfg(test)]
        {
            // Ensure a paused writer is unparked so shutdown does not wait on a test pause.
            let _ = lock(&self.pause_resume_tx).take();
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.tx.try_send(Msg::Shutdown) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => break,
                Err(TrySendError::Full(_)) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
        let handle = lock(&self.handle).take();
        if let Some(handle) = handle {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_paused(&self, paused: bool) {
        if paused {
            let mut slot = lock(&self.pause_resume_tx);
            if slot.is_some() {
                return;
            }
            let (ack_tx, ack_rx) = mpsc::sync_channel::<()>(1);
            let (resume_tx, resume_rx) = mpsc::sync_channel::<()>(1);
            let mut msg = Msg::Pause(ack_tx, resume_rx);
            loop {
                match self.tx.try_send(msg) {
                    Ok(()) => break,
                    Err(TrySendError::Full(returned)) => {
                        msg = returned;
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            let _ = ack_rx.recv_timeout(Duration::from_secs(5));
            *slot = Some(resume_tx);
        } else if let Some(resume_tx) = lock(&self.pause_resume_tx).take() {
            let _ = resume_tx.send(());
        }
    }
}

/// One session's files on disk.
#[derive(Debug, Clone)]
pub struct SessionFiles {
    pub session_id: String,
    pub pid: Option<u32>,
    pub paths: Vec<PathBuf>,
    pub total_bytes: u64,
    pub newest_mtime: SystemTime,
}

/// Parsed metadata from a `session-<utc>-<pid>-<session>.<segment>.jsonl` filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSegmentFileName {
    pub started_utc_compact: String,
    pub pid: u32,
    pub session_id: String,
    pub segment: u32,
}

/// Parse `session-<utc>-<pid>-<session>.<segment>.jsonl` with a numeric segment index.
#[must_use]
pub fn parse_session_segment_file_name(name: &str) -> Option<ParsedSegmentFileName> {
    let rest = name.strip_prefix("session-")?;
    let stem = rest.strip_suffix(".jsonl")?;
    let (stem, segment_str) = stem.rsplit_once('.')?;
    if segment_str.is_empty() || !segment_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let segment = segment_str.parse::<u32>().ok()?;
    let mut parts = stem.splitn(3, '-');
    let started_utc_compact = parts.next()?;
    if started_utc_compact.is_empty()
        || !started_utc_compact
            .chars()
            .all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let pid = parts.next()?.parse::<u32>().ok()?;
    let sid = parts.next()?;
    if sid.is_empty() || !sid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(ParsedSegmentFileName {
        started_utc_compact: started_utc_compact.to_string(),
        pid,
        session_id: sid.to_string(),
        segment,
    })
}

/// Parse `session-<utc>-<pid>-<session>.<n>.jsonl` / `evidence-<session>.json`.
#[must_use]
pub fn parse_log_file_name(name: &str) -> Option<(String, Option<u32>)> {
    if let Some(seg) = parse_session_segment_file_name(name) {
        return Some((seg.session_id, Some(seg.pid)));
    }
    if let Some(rest) = name.strip_prefix("evidence-")
        && let Some(sid) = rest.strip_suffix(".json")
        && !sid.is_empty()
        && sid.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Some((sid.to_string(), None));
    }
    None
}

/// Sort key for paths within a session so numeric `.jsonl` segments (`0, 1, ..., 9, 10`) are
/// ordered chronologically rather than lexicographically.
#[must_use]
pub fn segment_path_sort_key(path: &Path) -> (u8, String, u32, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(seg) = parse_session_segment_file_name(&name) {
        (0, seg.started_utc_compact, seg.segment, name)
    } else {
        (1, String::new(), u32::MAX, name)
    }
}

/// List log/evidence files grouped by session. Only regular files with recognised names are
/// returned; symlinks and everything else in the directory are ignored.
#[must_use]
pub fn list_sessions(dir: &Path) -> Vec<SessionFiles> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut by_session: std::collections::BTreeMap<String, SessionFiles> =
        std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((sid, pid)) = parse_log_file_name(&name) else {
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let slot = by_session
            .entry(sid.clone())
            .or_insert_with(|| SessionFiles {
                session_id: sid,
                pid: None,
                paths: Vec::new(),
                total_bytes: 0,
                newest_mtime: SystemTime::UNIX_EPOCH,
            });
        if pid.is_some() {
            slot.pid = pid;
        }
        slot.paths.push(entry.path());
        slot.total_bytes += meta.len();
        if mtime > slot.newest_mtime {
            slot.newest_mtime = mtime;
        }
    }
    for session in by_session.values_mut() {
        session.paths.sort_by_key(|p| segment_path_sort_key(p));
    }
    by_session.into_values().collect()
}

/// `true` when another live AWCC process (or a very recent writer) may still own the session.
#[must_use]
pub fn session_is_active(files: &SessionFiles, own_session: &str) -> bool {
    if files.session_id == own_session {
        return true;
    }
    if let Some(pid) = files.pid {
        let proc_dir = Path::new("/proc").join(pid.to_string());
        if proc_dir.exists()
            && fs::read_to_string(proc_dir.join("comm")).is_ok_and(|c| c.trim().starts_with("awcc"))
        {
            return true;
        }
    }
    files
        .newest_mtime
        .elapsed()
        .is_ok_and(|age| age < ACTIVE_MTIME_WINDOW)
}

/// Remove inactive sessions (never `own_session`, never an active writer) that are older than
/// `max_age`, then oldest-first until the directory fits `budget_bytes`. Returns files removed.
pub fn cleanup_logs(dir: &Path, budget_bytes: u64, max_age: Duration, own_session: &str) -> u64 {
    let mut sessions = list_sessions(dir);
    let mut removed = 0;
    let remove_session = |s: &SessionFiles, removed: &mut u64| {
        for p in &s.paths {
            if fs::remove_file(p).is_ok() {
                *removed += 1;
            }
        }
    };

    sessions.retain(|s| {
        let expired = !session_is_active(s, own_session)
            && s.newest_mtime.elapsed().is_ok_and(|age| age > max_age);
        if expired {
            remove_session(s, &mut removed);
        }
        !expired
    });

    let mut total: u64 = sessions.iter().map(|s| s.total_bytes).sum();
    let mut inactive: Vec<SessionFiles> = sessions
        .into_iter()
        .filter(|s| !session_is_active(s, own_session))
        .collect();
    inactive.sort_by_key(|s| s.newest_mtime);
    for s in inactive {
        if total <= budget_bytes {
            break;
        }
        remove_session(&s, &mut removed);
        total = total.saturating_sub(s.total_bytes);
    }
    removed
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "awcc_sink_{label}_{}_{}",
            std::process::id(),
            now_unix_millis()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn cfg(dir: &Path, sid: &str) -> SinkConfig {
        SinkConfig::new(dir.to_path_buf(), sid.to_string(), std::process::id())
    }

    fn read_all_lines(dir: &Path, sid: &str) -> Vec<String> {
        let mut files: Vec<PathBuf> = list_sessions(dir)
            .into_iter()
            .filter(|s| s.session_id == sid)
            .flat_map(|s| s.paths)
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort_by_key(|p| segment_path_sort_key(p));
        files
            .iter()
            .flat_map(|p| {
                fs::read_to_string(p)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn writes_private_files_and_flushes_on_shutdown() {
        let dir = tmp("basic");
        let sink = FileSink::start(cfg(&dir, "aaaa0001")).expect("sink");
        for i in 0..20 {
            assert!(sink.try_send_line(format!("{{\"n\":{i}}}")));
        }
        sink.shutdown(Duration::from_secs(5));

        let lines = read_all_lines(&dir, "aaaa0001");
        assert_eq!(lines.len(), 20);
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "log directory must be private");
        for s in list_sessions(&dir) {
            for p in s.paths {
                let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{} must be private", p.display());
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotates_by_size_and_keeps_only_a_bounded_number_of_own_segments() {
        let dir = tmp("rotate");
        let mut c = cfg(&dir, "bbbb0002");
        c.per_file_bytes = 200;
        c.max_files_per_session = 3;
        let sink = FileSink::start(c).expect("sink");
        for i in 0..200 {
            sink.try_send_line(format!("{{\"n\":{i},\"pad\":\"xxxxxxxxxxxxxxxxxxxx\"}}"));
        }
        assert!(sink.flush(Duration::from_secs(10)));
        let status = sink.status();
        sink.shutdown(Duration::from_secs(5));

        let files = list_sessions(&dir)
            .into_iter()
            .find(|s| s.session_id == "bbbb0002")
            .unwrap();
        let segments = files
            .paths
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .count();
        assert!(segments <= 3, "{segments} segments");
        assert!(status.rotations > 3, "{status:?}");
        for p in &files.paths {
            if p.extension().is_some_and(|e| e == "jsonl") {
                assert!(fs::metadata(p).unwrap().len() <= 260, "segment bound");
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_saturation_drops_and_counts_without_blocking_and_reports_the_gap() {
        let dir = tmp("saturate");
        let mut c = cfg(&dir, "cccc0003");
        c.queue_capacity = 8;
        let sink = FileSink::start(c).expect("sink");
        sink.set_paused(true);

        let started = Instant::now();
        let accepted = (0..500)
            .filter(|i| sink.try_send_line(format!("{{\"n\":{i}}}")))
            .count();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "emitters must never block on a stalled writer"
        );
        assert_eq!(
            accepted, 8,
            "acknowledged pause parks writer before queue fills"
        );
        let expected_dropped = (500 - accepted) as u64;
        assert_eq!(sink.status().dropped_queue_full, expected_dropped);

        sink.set_paused(false);
        assert!(sink.flush(Duration::from_secs(10)));

        // Trigger a second saturation burst so incremental drop reporting across multiple windows
        // is also verified.
        sink.set_paused(true);
        let accepted_second = (500..600)
            .filter(|i| sink.try_send_line(format!("{{\"n\":{i}}}")))
            .count();
        assert_eq!(accepted_second, 8);
        let total_expected_dropped = expected_dropped + (100 - accepted_second) as u64;
        assert_eq!(sink.status().dropped_queue_full, total_expected_dropped);

        sink.set_paused(false);
        assert!(sink.flush(Duration::from_secs(10)));
        sink.shutdown(Duration::from_secs(5));

        let lines = read_all_lines(&dir, "cccc0003");
        let mut payload_count = 0usize;
        let mut drop_events: Vec<crate::event::Event> = Vec::new();
        for line in &lines {
            let val: serde_json::Value =
                serde_json::from_str(line).expect("every persisted line must be valid JSON");
            if val.get("code").and_then(serde_json::Value::as_str) == Some("events_dropped") {
                let ev: crate::event::Event = serde_json::from_str(line)
                    .expect("events_dropped line must deserialize as a full Event");
                assert_eq!(ev.session_id, "cccc0003");
                assert_eq!(ev.seq, 0);
                assert_eq!(ev.severity, crate::event::Severity::Warn);
                drop_events.push(ev);
            } else if val.get("n").is_some() {
                payload_count += 1;
            }
        }
        assert_eq!(payload_count, accepted + accepted_second);
        assert!(
            !drop_events.is_empty(),
            "at least one events_dropped Event must be persisted: {lines:?}"
        );
        let reported_sum: u64 = drop_events
            .iter()
            .map(|ev| {
                ev.repeat_count
                    .expect("events_dropped must carry repeat_count")
            })
            .sum();
        assert_eq!(
            reported_sum, total_expected_dropped,
            "sum of incremental events_dropped repeat_count values must equal total dropped lines: {drop_events:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_directory_is_reported_not_fatal() {
        let result = FileSink::start(cfg(Path::new("/proc/awcc-cannot-create/logs"), "dddd0004"));
        match result {
            Err(SinkError::Directory { path, reason }) => {
                assert!(path.contains("awcc-cannot-create"));
                assert!(!reason.is_empty());
            }
            Ok(_) => panic!("must not start in an unwritable directory"),
        }
    }

    #[test]
    fn write_failures_never_block_emitters_and_are_counted() {
        let dir = tmp("failing");
        let sink = FileSink::start(cfg(&dir, "eeee0005")).expect("sink");
        // Remove the directory under the writer: every segment open now fails.
        fs::remove_dir_all(&dir).unwrap();
        fs::write(&dir, b"now a file, not a directory").unwrap();
        let started = Instant::now();
        for i in 0..50 {
            sink.try_send_line(format!("{{\"n\":{i}}}"));
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(sink.flush(Duration::from_secs(10)));
        let status = sink.status();
        assert!(status.write_failures > 0, "{status:?}");
        assert!(status.dropped_write_failed > 0, "{status:?}");
        assert!(status.last_error.is_some());
        sink.shutdown(Duration::from_secs(5));
        let _ = fs::remove_file(&dir);
    }

    #[test]
    fn cleanup_removes_only_inactive_sessions_oldest_first_and_never_touches_others() {
        let dir = tmp("cleanup");
        ensure_private_dir(&dir).unwrap();
        // pid 4_000_000_000 cannot exist: these sessions are inactive once old enough.
        let write = |sid: &str, size: usize| {
            let p = dir.join(format!("session-20260101T000000Z-4000000000-{sid}.0.jsonl"));
            fs::write(&p, vec![b'x'; size]).unwrap();
            p
        };
        let old1 = write("a1", 1000);
        std::thread::sleep(Duration::from_millis(20));
        let old2 = write("a2", 1000);
        let own = write("ff", 5000);
        let foreign_active = {
            // A "foreign" session whose pid is this live test process and whose comm is not
            // `awcc*` but which was written just now: protected by the recent-mtime window.
            let p = dir.join(format!(
                "session-20260101T000000Z-{}-b2.0.jsonl",
                std::process::id()
            ));
            fs::write(&p, vec![b'y'; 5000]).unwrap();
            p
        };
        // Make the two old sessions look old enough to be inactive, with old1 older than old2.
        let older_ago = SystemTime::now() - Duration::from_secs(7200);
        let long_ago = SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&old1)
            .unwrap()
            .set_modified(older_ago)
            .unwrap();
        File::options()
            .write(true)
            .open(&old2)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let before_own = fs::read(&own).unwrap();

        // Total is 12_000 (own 5000 + foreign 5000 + old1 1000 + old2 1000). Budget 11_500 fits
        // own + foreign + old2 (11_000) once the oldest inactive session (old1) is removed.
        let removed = cleanup_logs(&dir, 11_500, DEFAULT_MAX_AGE, "ff");
        assert_eq!(removed, 1, "oldest inactive session removed first");
        assert!(!old1.exists());
        assert!(old2.exists(), "second-oldest kept once under budget");
        assert!(own.exists() && foreign_active.exists());
        assert_eq!(fs::read(&own).unwrap(), before_own, "own file untouched");

        // Even with a zero budget, active sessions are never deleted.
        let removed = cleanup_logs(&dir, 0, DEFAULT_MAX_AGE, "ff");
        assert_eq!(removed, 1);
        assert!(own.exists() && foreign_active.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_sessions_write_separate_files_and_never_touch_each_other() {
        let dir = tmp("concurrent");
        let a = FileSink::start(cfg(&dir, "a0a0a0a0")).expect("sink a");
        let b = FileSink::start(cfg(&dir, "b0b0b0b0")).expect("sink b");
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..300 {
                    a.try_send_line(format!("{{\"who\":\"a\",\"n\":{i}}}"));
                }
            });
            s.spawn(|| {
                for i in 0..300 {
                    b.try_send_line(format!("{{\"who\":\"b\",\"n\":{i}}}"));
                }
            });
        });
        a.shutdown(Duration::from_secs(5));
        let a_before = read_all_lines(&dir, "a0a0a0a0");
        // `b` keeps running (an active writer) while `a`'s process starts cleanup with a zero
        // budget: b's files must survive and keep growing.
        let removed = cleanup_logs(&dir, 0, DEFAULT_MAX_AGE, "a0a0a0a0");
        assert_eq!(removed, 0, "nothing of an active session may be deleted");
        b.try_send_line("{\"who\":\"b\",\"n\":300}".to_string());
        b.shutdown(Duration::from_secs(5));

        let a_lines = read_all_lines(&dir, "a0a0a0a0");
        let b_lines = read_all_lines(&dir, "b0b0b0b0");
        assert_eq!(a_lines.len(), 300);
        assert_eq!(a_lines, a_before, "a's file was not modified by b");
        assert_eq!(b_lines.len(), 301);
        assert!(a_lines.iter().all(|l| l.contains("\"who\":\"a\"")));
        assert!(b_lines.iter().all(|l| l.contains("\"who\":\"b\"")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_is_replaced_atomically_and_privately() {
        let dir = tmp("evidence");
        let sink = FileSink::start(cfg(&dir, "abcd1234")).expect("sink");
        sink.publish_evidence("{\"v\":1}".to_string());
        assert!(sink.flush(Duration::from_secs(5)));
        sink.publish_evidence("{\"v\":2}".to_string());
        sink.shutdown(Duration::from_secs(5));
        let path = dir.join("evidence-abcd1234.json");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"v\":2}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!dir.join("evidence-abcd1234.json.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_file_names_parse_and_foreign_files_are_ignored() {
        assert_eq!(
            parse_log_file_name("session-20261019T155614Z-1234-abc123.0.jsonl"),
            Some(("abc123".to_string(), Some(1234)))
        );
        assert_eq!(
            parse_log_file_name("evidence-abc123.json"),
            Some(("abc123".to_string(), None))
        );
        for bad in [
            "session-1-2.jsonl",
            "notes.txt",
            "../session-x-1-abc.0.jsonl",
            "session-20261019T155614Z-12-zzzz.0.jsonl",
            "evidence-.json",
        ] {
            assert_eq!(parse_log_file_name(bad), None, "{bad}");
        }
    }
}
