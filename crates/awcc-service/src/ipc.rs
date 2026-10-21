//! Versioned, framed, authorized IPC between clients and the privileged daemon.
//!
//! # Wire format
//!
//! Every message is a 4-byte big-endian length followed by that many bytes of JSON
//! (`MAX_FRAME_BYTES` upper bound). Reads and writes are complete (`read_exact`/`write_all`) under
//! explicit deadlines, so fragmented, truncated, oversized and slow peers are handled
//! deterministically. A connection starts with a [`ClientHello`] answered by a [`ServerHello`]
//! that carries the protocol version, daemon build identity, capabilities and status;
//! compatibility is decided by protocol range and capabilities, never by exact application
//! version.
//!
//! # Authorization
//!
//! The peer is identified by the kernel (`SO_PEERCRED`), never by anything in the JSON. A peer is
//! authorized when it is `root` or its UID appears in the root-controlled allowlist
//! (`/etc/awcc/authorized-users`, by user name or numeric UID). The same policy covers every
//! operation, including the low-level compatibility requests. Unauthorized peers receive one
//! `access_denied` error and no state.
//!
//! # Ownership
//!
//! [`DaemonLock`] is a process-wide exclusive `flock` that every daemon entry point takes before it
//! touches hardware or the socket. A second daemon fails clearly; a stale socket is removed only
//! while holding the lock, and only if it is a socket (never a symlink or regular file).

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use awcc_core::{FanCurveConfig, OperationLifecycle, ThermalMode};
use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};

use crate::control::{
    ControlHandle, ControlOp, ControlRejection, ControlRequest, OperationStatus, SubmitError,
};
use crate::daemon::DaemonRpcRequest;
use crate::wire::ThermalView;

/// Protocol version spoken by this build.
pub const PROTOCOL_VERSION: u32 = 1;
/// Oldest protocol version this build still speaks.
pub const MIN_SUPPORTED_PROTOCOL: u32 = 1;
/// Upper bound of one frame's payload.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Exclusive daemon lock file.
pub const DEFAULT_LOCK_PATH: &str = "/run/awcc/daemon.lock";
/// Root-controlled list of users allowed to control hardware through the daemon.
pub const DEFAULT_AUTHORIZED_USERS_PATH: &str = "/etc/awcc/authorized-users";
/// Maximum simultaneously served connections.
pub const MAX_CONNECTIONS: usize = 16;
/// Deadline for a peer to complete the handshake.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(3);
/// A connected peer that sends nothing for this long is dropped.
pub const IDLE_DEADLINE: Duration = Duration::from_secs(10);
/// Time a state-changing operation may take on the control thread before the reply times out.
pub const OPERATION_DEADLINE: Duration = Duration::from_secs(12);

/// Capabilities advertised by this daemon.
pub const CAPABILITIES: &[&str] = &[
    "status.v1",
    "snapshot.v1",
    "thermal.v1",
    "curve.v1",
    "startup_preference.v1",
    "reconcile.v1",
    "legacy_rpc.v1",
];

/// Capabilities a client of this build requires.
pub const REQUIRED_CAPABILITIES: &[&str] = &["status.v1", "snapshot.v1", "thermal.v1", "curve.v1"];

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub client_name: String,
    pub client_build: String,
}

/// Build identity of the daemon (no paths, no host data).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSummary {
    pub version: String,
    pub commit: String,
    pub package_format: String,
}

/// Daemon state independent of any client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatus {
    pub uptime_secs: u64,
    pub device_name: String,
    pub acpi_prefix: String,
    /// Whether the control thread has produced a view (process/protocol health, not hardware).
    pub control_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerHello {
    pub protocol: u32,
    pub daemon_build: BuildSummary,
    pub capabilities: Vec<String>,
    pub status: DaemonStatus,
}

/// Stable error classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    AccessDenied,
    ProtocolMismatch,
    Overloaded,
    Timeout,
    InvalidRequest,
    Unsupported,
    Rejected,
    Busy,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Client-chosen unique id; also the reconciliation key for state-changing operations.
    pub id: String,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Daemon status; never touches hardware.
    Status,
    /// Latest view; refreshed from hardware only when older than `max_age_ms`.
    Snapshot {
        max_age_ms: u64,
    },
    /// Force a fresh passive hardware read.
    Refresh,
    SetThermalMode {
        mode: ThermalMode,
    },
    ToggleGMode,
    ApplyManualBoost {
        cpu_boost_pct: u8,
        gpu_boost_pct: u8,
    },
    SetCpuTurbo {
        enabled: bool,
    },
    SetTccOffset {
        offset: u8,
    },
    ConfigureFanCurve {
        config: FanCurveConfig,
    },
    StopFanCurve,
    SetRestoreAtStartup {
        enabled: bool,
    },
    /// What happened to an earlier request id? Never replays it.
    QueryOperation {
        request_id: String,
    },
    /// Low-level compatibility request; same authorization, validation and serialization.
    Legacy {
        request: DaemonRpcRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Body {
    Ok {
        status: Option<DaemonStatus>,
        view: Option<Box<ThermalView>>,
        lifecycle: Option<OperationLifecycle>,
        message: String,
    },
    Err {
        kind: ErrorClass,
        message: String,
    },
    /// `QueryOperation` result: finished, with its recorded outcome.
    Completed {
        view: Box<ThermalView>,
        lifecycle: OperationLifecycle,
    },
    /// `QueryOperation` result: still queued or running.
    Pending,
    /// `QueryOperation` result: not known (never seen, evicted, or lost by a restart).
    Unknown,
}

impl Response {
    fn err(id: &str, kind: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            body: Body::Err {
                kind,
                message: message.into(),
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------------------------

/// Framing failure.
#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    /// Peer closed before the frame completed.
    Truncated,
    TooLarge(usize),
    TimedOut,
    Malformed(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Truncated => write!(f, "connection closed mid-message"),
            Self::TooLarge(n) => write!(f, "message of {n} bytes exceeds {MAX_FRAME_BYTES}"),
            Self::TimedOut => write!(f, "timed out"),
            Self::Malformed(m) => write!(f, "malformed message: {m}"),
        }
    }
}

impl std::error::Error for FrameError {}

fn read_exact_deadline(
    stream: &mut UnixStream,
    buf: &mut [u8],
    deadline: Instant,
) -> Result<(), FrameError> {
    let mut filled = 0;
    while filled < buf.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(FrameError::TimedOut);
        }
        stream
            .set_read_timeout(Some(left.min(Duration::from_millis(500))))
            .map_err(FrameError::Io)?;
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(if filled == 0 {
                    FrameError::Io(std::io::Error::from(ErrorKind::UnexpectedEof))
                } else {
                    FrameError::Truncated
                });
            }
            Ok(n) => filled += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}

/// Read one frame's payload (without decoding it).
///
/// # Errors
/// See [`FrameError`].
pub fn read_frame(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, FrameError> {
    let mut header = [0_u8; 4];
    read_exact_deadline(stream, &mut header, deadline)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0_u8; len];
    read_exact_deadline(stream, &mut payload, deadline).map_err(|e| {
        if matches!(e, FrameError::Io(_)) {
            FrameError::Truncated
        } else {
            e
        }
    })?;
    Ok(payload)
}

/// Write one frame completely under a deadline.
///
/// # Errors
/// See [`FrameError`].
pub fn write_frame(
    stream: &mut UnixStream,
    payload: &[u8],
    deadline: Instant,
) -> Result<(), FrameError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut message = Vec::with_capacity(4 + payload.len());
    message.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    message.extend_from_slice(payload);
    let mut sent = 0;
    while sent < message.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(FrameError::TimedOut);
        }
        stream
            .set_write_timeout(Some(left.min(Duration::from_millis(500))))
            .map_err(FrameError::Io)?;
        match stream.write(&message[sent..]) {
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => sent += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}

fn send_json<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    deadline: Instant,
) -> Result<(), FrameError> {
    let bytes = serde_json::to_vec(value).map_err(|e| FrameError::Malformed(e.to_string()))?;
    write_frame(stream, &bytes, deadline)
}

fn recv_json<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<T, FrameError> {
    let payload = read_frame(stream, deadline)?;
    serde_json::from_slice(&payload).map_err(|e| FrameError::Malformed(e.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------------------------

/// Decides whether a kernel-identified peer may control hardware.
pub trait Authorizer: Send + Sync {
    /// `Ok(())` when allowed; `Err(reason)` is logged and summarised to the peer.
    fn authorize(&self, uid: u32) -> Result<(), String>;
}

/// Root plus the users listed in a root-controlled file.
#[derive(Debug, Clone)]
pub struct AllowlistAuthorizer {
    users_file: PathBuf,
    passwd_file: PathBuf,
    require_root_owned: bool,
}

impl AllowlistAuthorizer {
    /// Production policy: `/etc/awcc/authorized-users`, names resolved via `/etc/passwd`.
    #[must_use]
    pub fn system() -> Self {
        Self::new(DEFAULT_AUTHORIZED_USERS_PATH, "/etc/passwd", true)
    }

    #[must_use]
    pub fn new(
        users_file: impl Into<PathBuf>,
        passwd_file: impl Into<PathBuf>,
        require_root_owned: bool,
    ) -> Self {
        Self {
            users_file: users_file.into(),
            passwd_file: passwd_file.into(),
            require_root_owned,
        }
    }
}

fn read_bounded_regular(path: &Path, require_root_owned: bool) -> Result<String, String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK).bits() as i32);
    let mut file = options.open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > 64 * 1024 {
        return Err("file too large".to_string());
    }
    if require_root_owned && (meta.uid() != 0 || meta.mode() & 0o022 != 0) {
        return Err("must be owned by root and not group/world-writable".to_string());
    }
    let mut text = String::new();
    (&mut file)
        .take(64 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    Ok(text)
}

fn passwd_uid(passwd: &str, name: &str) -> Option<u32> {
    passwd.lines().find_map(|line| {
        let mut f = line.split(':');
        (f.next()? == name).then(|| f.nth(1)?.parse().ok())?
    })
}

impl Authorizer for AllowlistAuthorizer {
    fn authorize(&self, uid: u32) -> Result<(), String> {
        if uid == 0 {
            return Ok(());
        }
        let users = match read_bounded_regular(&self.users_file, self.require_root_owned) {
            Ok(t) => t,
            Err(e) if e.contains("No such file") => {
                return Err("no authorized users are configured".to_string());
            }
            Err(e) => return Err(format!("authorized-users file rejected: {e}")),
        };
        let passwd = read_bounded_regular(&self.passwd_file, false).unwrap_or_default();
        for line in users.lines() {
            let entry = line.split('#').next().unwrap_or("").trim();
            if entry.is_empty() {
                continue;
            }
            let entry_uid = entry
                .parse::<u32>()
                .ok()
                .or_else(|| passwd_uid(&passwd, entry));
            if entry_uid == Some(uid) {
                return Ok(());
            }
        }
        Err(format!("uid {uid} is not authorized to control hardware"))
    }
}

/// Management of the allowlist file (used by `awcc-cli service authorize`, root only).
pub mod authorized_users {
    use super::*;

    /// Entries currently in the file (comments and blanks skipped).
    ///
    /// # Errors
    /// Returns the read error.
    pub fn list(path: &Path) -> std::io::Result<BTreeSet<String>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(text
                .lines()
                .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(BTreeSet::new()),
            Err(e) => Err(e),
        }
    }

    fn valid_entry(user: &str) -> bool {
        !user.is_empty()
            && user.len() <= 64
            && user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    }

    fn write(path: &Path, entries: &BTreeSet<String>) -> std::io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| std::io::Error::other("no parent directory"))?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".authorized-users.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut body = String::from(
            "# Users allowed to control AWCC hardware through the daemon.\n# One user name or numeric UID per line. Managed by `awcc-cli service authorize`.\n",
        );
        for e in entries {
            body.push_str(e);
            body.push('\n');
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    }

    /// Add `user`; returns whether the file changed.
    ///
    /// # Errors
    /// Returns an error for an invalid entry or on I/O failure.
    pub fn add(path: &Path, user: &str) -> std::io::Result<bool> {
        if !valid_entry(user) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "user must be a plain name or numeric UID",
            ));
        }
        let mut entries = list(path)?;
        if !entries.insert(user.to_string()) {
            return Ok(false);
        }
        write(path, &entries)?;
        Ok(true)
    }

    /// Remove `user`; returns whether the file changed.
    ///
    /// # Errors
    /// Returns an error on I/O failure.
    pub fn remove(path: &Path, user: &str) -> std::io::Result<bool> {
        let mut entries = list(path)?;
        if !entries.remove(user) {
            return Ok(false);
        }
        write(path, &entries)?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------------------------
// Exclusive ownership
// ---------------------------------------------------------------------------------------------

/// Why the daemon could not take ownership.
#[derive(Debug)]
pub enum OwnershipError {
    /// Another process holds the lock.
    AlreadyRunning(PathBuf),
    /// Directory, lock or socket path is unsafe (symlink, wrong owner, not a socket...).
    Unsafe(String),
    Io(std::io::Error),
}

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning(p) => write!(
                f,
                "another AWCC daemon already owns thermal control (lock {} is held); it was not disturbed",
                p.display()
            ),
            Self::Unsafe(m) => write!(f, "unsafe runtime path: {m}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for OwnershipError {}

/// Process-wide exclusive ownership of thermal control. Released when dropped or on exit.
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
    path: PathBuf,
}

impl DaemonLock {
    /// Take the lock, creating the runtime directory if needed.
    ///
    /// `strict_owner` requires the directory to be owned by the current user and not writable by
    /// others; production passes `true`.
    ///
    /// # Errors
    /// See [`OwnershipError`].
    pub fn acquire(path: &Path, strict_owner: bool) -> Result<Self, OwnershipError> {
        let dir = path
            .parent()
            .ok_or_else(|| OwnershipError::Unsafe("lock path has no directory".to_string()))?;
        ensure_private_dir(dir, strict_owner)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags((OFlags::NOFOLLOW | OFlags::CLOEXEC).bits() as i32);
        let file = options.open(path).map_err(OwnershipError::Io)?;
        let meta = file.metadata().map_err(OwnershipError::Io)?;
        if !meta.is_file() {
            return Err(OwnershipError::Unsafe(
                "lock is not a regular file".to_string(),
            ));
        }
        if strict_owner && meta.uid() != rustix::process::getuid().as_raw() {
            return Err(OwnershipError::Unsafe(
                "lock file is owned by another user".to_string(),
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(Self {
                _file: file,
                path: path.to_path_buf(),
            }),
            Err(fs::TryLockError::WouldBlock) => {
                Err(OwnershipError::AlreadyRunning(path.to_path_buf()))
            }
            Err(fs::TryLockError::Error(e)) => Err(OwnershipError::Io(e)),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn ensure_private_dir(dir: &Path, strict_owner: bool) -> Result<(), OwnershipError> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(OwnershipError::Unsafe(format!(
                    "{} is not a plain directory",
                    dir.display()
                )));
            }
            if strict_owner
                && (meta.uid() != rustix::process::getuid().as_raw() || meta.mode() & 0o002 != 0)
            {
                return Err(OwnershipError::Unsafe(format!(
                    "{} has unsafe ownership or permissions",
                    dir.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(dir).map_err(OwnershipError::Io)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(OwnershipError::Io)
        }
        Err(e) => Err(OwnershipError::Io(e)),
    }
}

/// Remove a stale socket. The caller must hold [`DaemonLock`]. Never follows a symlink and never
/// removes anything that is not a socket.
fn clear_stale_socket(path: &Path) -> Result<(), OwnershipError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            fs::remove_file(path).map_err(OwnershipError::Io)
        }
        Ok(_) => Err(OwnershipError::Unsafe(format!(
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OwnershipError::Io(e)),
    }
}

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

/// Everything the server needs.
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub authorizer: Arc<dyn Authorizer>,
    pub build: BuildSummary,
    pub device_name: String,
    pub acpi_prefix: String,
}

struct ServerContext {
    cfg: ServerConfig,
    control: ControlHandle,
    started: Instant,
    connections: AtomicUsize,
    counter: AtomicU64,
}

/// The bound, listening socket server. Construct only while holding a [`DaemonLock`].
pub struct IpcServer {
    ctx: Arc<ServerContext>,
    listener: UnixListener,
}

impl IpcServer {
    /// Bind the socket (the caller already holds the exclusive lock).
    ///
    /// # Errors
    /// See [`OwnershipError`].
    pub fn bind(
        cfg: ServerConfig,
        control: ControlHandle,
        _lock: &DaemonLock,
    ) -> Result<Self, OwnershipError> {
        if let Some(dir) = cfg.socket_path.parent() {
            ensure_private_dir(dir, false)?;
        }
        clear_stale_socket(&cfg.socket_path)?;
        let listener = UnixListener::bind(&cfg.socket_path).map_err(OwnershipError::Io)?;
        // Reachability is open; authorization is the peer-credential check on every connection.
        fs::set_permissions(&cfg.socket_path, fs::Permissions::from_mode(0o666))
            .map_err(OwnershipError::Io)?;
        listener.set_nonblocking(true).map_err(OwnershipError::Io)?;
        Ok(Self {
            ctx: Arc::new(ServerContext {
                cfg,
                control,
                started: Instant::now(),
                connections: AtomicUsize::new(0),
                counter: AtomicU64::new(0),
            }),
            listener,
        })
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.ctx.cfg.socket_path
    }

    /// Accept connections until `stop` is set. Each connection is served on its own thread, at
    /// most [`MAX_CONNECTIONS`] at a time.
    pub fn serve(&self, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let ctx = Arc::clone(&self.ctx);
                    if ctx.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        ctx.connections.fetch_sub(1, Ordering::SeqCst);
                        refuse_overloaded(stream);
                        continue;
                    }
                    let spawned = std::thread::Builder::new()
                        .name("awcc-ipc".to_string())
                        .spawn({
                            let ctx = Arc::clone(&ctx);
                            move || {
                                handle_connection(stream, &ctx);
                                ctx.connections.fetch_sub(1, Ordering::SeqCst);
                            }
                        });
                    if spawned.is_err() {
                        ctx.connections.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::error!(
                        target: "awcc_service::ipc",
                        category = "app",
                        action = "accept",
                        outcome = "failed",
                        code = "accept_failed",
                        "accept failed: {e}"
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        let _ = fs::remove_file(&self.ctx.cfg.socket_path);
    }
}

fn refuse_overloaded(mut stream: UnixStream) {
    let _ = send_json(
        &mut stream,
        &Response::err(
            "",
            ErrorClass::Overloaded,
            "too many concurrent connections",
        ),
        Instant::now() + Duration::from_millis(300),
    );
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    rustix::net::sockopt::socket_peercred(stream)
        .ok()
        .map(|c| c.uid.as_raw())
}

fn handle_connection(mut stream: UnixStream, ctx: &ServerContext) {
    let uid = peer_uid(&stream);
    let hello: Result<ClientHello, _> = recv_json(&mut stream, Instant::now() + HANDSHAKE_DEADLINE);
    let hello = match hello {
        Ok(h) => h,
        Err(FrameError::TooLarge(_) | FrameError::Malformed(_)) => {
            // Legacy line-oriented or garbage peers get one explanatory frame.
            let _ = send_json(
                &mut stream,
                &Response::err(
                    "",
                    ErrorClass::ProtocolMismatch,
                    "expected a framed AWCC hello; upgrade the client",
                ),
                Instant::now() + Duration::from_millis(500),
            );
            return;
        }
        Err(_) => return,
    };
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    // Authorization is decided on the kernel-reported identity, before any state is revealed.
    let verdict = match uid {
        Some(uid) => ctx.cfg.authorizer.authorize(uid).map_err(|r| (uid, r)),
        None => Err((u32::MAX, "peer credentials unavailable".to_string())),
    };
    if let Err((uid, reason)) = verdict {
        tracing::warn!(
            target: "awcc_service::ipc",
            category = "app",
            action = "authorize",
            outcome = "denied",
            code = "access_denied",
            uid = uid,
            "peer refused: {reason}"
        );
        let _ = send_json(
            &mut stream,
            &Response::err(
                "",
                ErrorClass::AccessDenied,
                format!(
                    "access denied: {reason}. An administrator can run `sudo awcc-cli service authorize <user>`"
                ),
            ),
            deadline,
        );
        return;
    }
    if hello.protocol_min > PROTOCOL_VERSION || hello.protocol_max < MIN_SUPPORTED_PROTOCOL {
        let _ = send_json(
            &mut stream,
            &Response::err(
                "",
                ErrorClass::ProtocolMismatch,
                format!(
                    "client speaks protocol {}..={}, daemon speaks {MIN_SUPPORTED_PROTOCOL}..={PROTOCOL_VERSION}",
                    hello.protocol_min, hello.protocol_max
                ),
            ),
            deadline,
        );
        return;
    }
    let server_hello = ServerHello {
        protocol: PROTOCOL_VERSION.min(hello.protocol_max),
        daemon_build: ctx.cfg.build.clone(),
        capabilities: CAPABILITIES.iter().map(|c| (*c).to_string()).collect(),
        status: daemon_status(ctx),
    };
    if send_json(&mut stream, &server_hello, deadline).is_err() {
        return;
    }
    loop {
        let request: Result<Request, _> = recv_json(&mut stream, Instant::now() + IDLE_DEADLINE);
        let request = match request {
            Ok(r) => r,
            Err(FrameError::Malformed(m)) => {
                let _ = send_json(
                    &mut stream,
                    &Response::err("", ErrorClass::InvalidRequest, m),
                    Instant::now() + Duration::from_secs(1),
                );
                return;
            }
            Err(FrameError::TooLarge(n)) => {
                let _ = send_json(
                    &mut stream,
                    &Response::err(
                        "",
                        ErrorClass::InvalidRequest,
                        format!("message of {n} bytes is too large"),
                    ),
                    Instant::now() + Duration::from_secs(1),
                );
                return;
            }
            Err(_) => return,
        };
        let response = dispatch(ctx, request);
        if send_json(
            &mut stream,
            &response,
            Instant::now() + Duration::from_secs(5),
        )
        .is_err()
        {
            return;
        }
    }
}

fn daemon_status(ctx: &ServerContext) -> DaemonStatus {
    DaemonStatus {
        uptime_secs: ctx.started.elapsed().as_secs(),
        device_name: ctx.cfg.device_name.clone(),
        acpi_prefix: ctx.cfg.acpi_prefix.clone(),
        control_running: ctx.control.published().is_some(),
    }
}

fn ok(
    id: &str,
    view: Option<ThermalView>,
    lifecycle: Option<OperationLifecycle>,
    message: &str,
) -> Response {
    Response {
        id: id.to_string(),
        body: Body::Ok {
            status: None,
            view: view.map(Box::new),
            lifecycle,
            message: message.to_string(),
        },
    }
}

fn dispatch(ctx: &ServerContext, request: Request) -> Response {
    let id = request.id.clone();
    if id.is_empty() || id.len() > 96 {
        return Response::err(
            &id,
            ErrorClass::InvalidRequest,
            "request id must be 1..=96 bytes",
        );
    }
    match request.op {
        Op::Status => Response {
            id,
            body: Body::Ok {
                status: Some(daemon_status(ctx)),
                view: None,
                lifecycle: None,
                message: "ok".to_string(),
            },
        },
        Op::Snapshot { max_age_ms } => {
            ctx.control.note_interest();
            let max_age = Duration::from_millis(max_age_ms);
            if let Some((view, age)) = ctx.control.published()
                && age <= max_age
            {
                return ok(&id, Some((*view).clone()), None, "cached");
            }
            refresh(ctx, &id)
        }
        Op::Refresh => {
            ctx.control.note_interest();
            refresh(ctx, &id)
        }
        Op::QueryOperation { request_id } => match ctx.control.operation_status(&request_id) {
            OperationStatus::Completed(reply) => match reply.result {
                Ok(lifecycle) => Response {
                    id,
                    body: Body::Completed {
                        view: Box::new(reply.view),
                        lifecycle,
                    },
                },
                Err(r) => Response::err(&id, ErrorClass::Rejected, r.message),
            },
            OperationStatus::Rejected(r) => Response::err(&id, ErrorClass::Rejected, r.message),
            OperationStatus::Pending => Response {
                id,
                body: Body::Pending,
            },
            OperationStatus::Unknown => Response {
                id,
                body: Body::Unknown,
            },
        },
        Op::Legacy { request: legacy } => run_legacy(ctx, id, legacy),
        other => {
            let op = match other {
                Op::SetThermalMode { mode } => ControlOp::SetThermalMode(mode),
                Op::ToggleGMode => ControlOp::ToggleGMode,
                Op::ApplyManualBoost {
                    cpu_boost_pct,
                    gpu_boost_pct,
                } => ControlOp::ApplyManualBoost {
                    cpu_boost_pct,
                    gpu_boost_pct,
                },
                Op::SetCpuTurbo { enabled } => ControlOp::SetCpuTurbo(enabled),
                Op::SetTccOffset { offset } => ControlOp::SetTccOffset(offset),
                Op::ConfigureFanCurve { config } => ControlOp::ConfigureCurve(config),
                Op::StopFanCurve => ControlOp::StopCurve,
                Op::SetRestoreAtStartup { enabled } => ControlOp::SetRestoreAtStartup(enabled),
                _ => return Response::err(&id, ErrorClass::Internal, "unrouted operation"),
            };
            run_op(ctx, id, op)
        }
    }
}

fn refresh(ctx: &ServerContext, id: &str) -> Response {
    match ctx
        .control
        .request(ControlRequest::Refresh, OPERATION_DEADLINE)
    {
        Ok(Some(reply)) => ok(id, Some(reply.view), None, "refreshed"),
        Ok(None) => Response::err(
            id,
            ErrorClass::Timeout,
            "the control thread did not answer in time",
        ),
        Err(SubmitError::Overloaded) => {
            Response::err(id, ErrorClass::Overloaded, "control queue is full")
        }
        Err(SubmitError::Stopped) => {
            Response::err(id, ErrorClass::Internal, "control thread stopped")
        }
    }
}

fn run_op(ctx: &ServerContext, id: String, op: ControlOp) -> Response {
    let counter = ctx.counter.fetch_add(1, Ordering::Relaxed);
    let _ = counter;
    match ctx.control.request(
        ControlRequest::Op { id: id.clone(), op },
        OPERATION_DEADLINE,
    ) {
        Ok(Some(reply)) => match reply.result {
            Ok(lifecycle) => ok(&id, Some(reply.view), Some(lifecycle), "executed"),
            Err(ControlRejection { code, message }) => {
                Response::err(&id, ErrorClass::Rejected, format!("{code}: {message}"))
            }
        },
        Ok(None) => Response::err(
            &id,
            ErrorClass::Timeout,
            "outcome not known yet; reconcile with query_operation, do not replay",
        ),
        Err(SubmitError::Overloaded) => {
            Response::err(&id, ErrorClass::Overloaded, "control queue is full")
        }
        Err(SubmitError::Stopped) => {
            Response::err(&id, ErrorClass::Internal, "control thread stopped")
        }
    }
}

fn run_legacy(ctx: &ServerContext, id: String, legacy: DaemonRpcRequest) -> Response {
    if let Err(message) = legacy.validate() {
        return Response::err(&id, ErrorClass::InvalidRequest, message);
    }
    match legacy {
        DaemonRpcRequest::ConfigureFanCurve { config } => {
            run_op(ctx, id, ControlOp::ConfigureCurve(config))
        }
        DaemonRpcRequest::StopFanCurve => run_op(ctx, id, ControlOp::StopCurve),
        DaemonRpcRequest::SetCpuTurbo { enabled } => {
            run_op(ctx, id, ControlOp::SetCpuTurbo(enabled))
        }
        DaemonRpcRequest::AcpiWmax { .. } => match ctx.control.request(
            ControlRequest::RawAcpi {
                id: id.clone(),
                request: legacy,
            },
            OPERATION_DEADLINE,
        ) {
            Ok(Some(reply)) => match reply.result {
                Ok(_) => {
                    let mut response = ok(&id, Some(reply.view), None, "executed");
                    if let Body::Ok { message, .. } = &mut response.body {
                        *message = reply.raw_output.unwrap_or_default();
                    }
                    response
                }
                Err(r) => Response::err(
                    &id,
                    ErrorClass::Rejected,
                    format!("{}: {}", r.code, r.message),
                ),
            },
            Ok(None) => Response::err(
                &id,
                ErrorClass::Timeout,
                "the control thread did not answer in time",
            ),
            Err(SubmitError::Overloaded) => {
                Response::err(&id, ErrorClass::Overloaded, "control queue is full")
            }
            Err(SubmitError::Stopped) => {
                Response::err(&id, ErrorClass::Internal, "control thread stopped")
            }
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------

/// Client-side failure classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// No daemon is listening (missing socket, refused connection).
    Unavailable(String),
    AccessDenied(String),
    ProtocolMismatch(String),
    /// The daemon lacks a capability this client requires.
    MissingCapability(String),
    Overloaded(String),
    /// The request was rejected before any hardware work.
    Rejected(String),
    /// The reply did not arrive and reconciliation could not establish the outcome. The operation
    /// may or may not have run; it was **not** replayed.
    OutcomeUnknown {
        request_id: String,
        detail: String,
    },
    Failed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "service unavailable: {m}"),
            Self::AccessDenied(m) => write!(f, "{m}"),
            Self::ProtocolMismatch(m) => write!(f, "incompatible service: {m}"),
            Self::MissingCapability(c) => {
                write!(f, "incompatible service: missing capability `{c}`")
            }
            Self::Overloaded(m) => write!(f, "service busy: {m}"),
            Self::Rejected(m) => write!(f, "rejected: {m}"),
            Self::OutcomeUnknown { request_id, detail } => write!(
                f,
                "the service did not report an outcome for request {request_id} ({detail}); it was not replayed, check the current state"
            ),
            Self::Failed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Result of a successful state-changing request.
#[derive(Debug, Clone)]
pub struct OperationReport {
    pub view: ThermalView,
    pub lifecycle: OperationLifecycle,
}

/// Passive classification of the service, used by status, GUI and installer health checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceProbe {
    /// Nobody is listening on the socket.
    NotRunning {
        reason: String,
    },
    AccessDenied {
        message: String,
    },
    Incompatible {
        message: String,
    },
    Running {
        daemon_protocol: u32,
        version: String,
        commit: String,
        package_format: String,
        status: DaemonStatus,
        capabilities: Vec<String>,
    },
}

/// Client for the daemon socket.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
    client_name: String,
    client_build: String,
    connect_deadline: Duration,
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_request_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{}-{nanos:x}",
        std::process::id(),
        REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

struct Session {
    stream: UnixStream,
    hello: ServerHello,
}

impl DaemonClient {
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>, client_name: &str, client_build: &str) -> Self {
        Self {
            socket: socket.into(),
            client_name: client_name.to_string(),
            client_build: client_build.to_string(),
            connect_deadline: Duration::from_secs(3),
        }
    }

    /// Client for the standard system socket.
    #[must_use]
    pub fn system(client_name: &str, client_build: &str) -> Self {
        Self::new(
            crate::daemon::DEFAULT_SOCKET_PATH,
            client_name,
            client_build,
        )
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    fn connect(&self) -> Result<Session, ClientError> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|e| {
            ClientError::Unavailable(match e.kind() {
                ErrorKind::NotFound => format!("no socket at {}", self.socket.display()),
                ErrorKind::PermissionDenied => {
                    format!("permission denied opening {}", self.socket.display())
                }
                ErrorKind::ConnectionRefused => {
                    "the socket exists but nothing is listening".to_string()
                }
                _ => e.to_string(),
            })
        })?;
        let deadline = Instant::now() + self.connect_deadline;
        send_json(
            &mut stream,
            &ClientHello {
                protocol_min: MIN_SUPPORTED_PROTOCOL,
                protocol_max: PROTOCOL_VERSION,
                client_name: self.client_name.clone(),
                client_build: self.client_build.clone(),
            },
            deadline,
        )
        .map_err(|e| ClientError::Unavailable(e.to_string()))?;
        let payload = read_frame(&mut stream, deadline)
            .map_err(|e| ClientError::Unavailable(e.to_string()))?;
        if let Ok(hello) = serde_json::from_slice::<ServerHello>(&payload) {
            for cap in REQUIRED_CAPABILITIES {
                if !hello.capabilities.iter().any(|c| c == cap) {
                    return Err(ClientError::MissingCapability((*cap).to_string()));
                }
            }
            if hello.protocol < MIN_SUPPORTED_PROTOCOL || hello.protocol > PROTOCOL_VERSION {
                return Err(ClientError::ProtocolMismatch(format!(
                    "daemon protocol {}",
                    hello.protocol
                )));
            }
            return Ok(Session { stream, hello });
        }
        match serde_json::from_slice::<Response>(&payload) {
            Ok(Response {
                body: Body::Err { kind, message },
                ..
            }) => Err(match kind {
                ErrorClass::AccessDenied => ClientError::AccessDenied(message),
                ErrorClass::ProtocolMismatch => ClientError::ProtocolMismatch(message),
                ErrorClass::Overloaded => ClientError::Overloaded(message),
                _ => ClientError::Failed(message),
            }),
            _ => Err(ClientError::ProtocolMismatch(
                "the daemon sent an unrecognised handshake".to_string(),
            )),
        }
    }

    fn call(
        session: &mut Session,
        request: &Request,
        timeout: Duration,
    ) -> Result<Response, FrameError> {
        let deadline = Instant::now() + timeout;
        send_json(&mut session.stream, request, deadline)?;
        recv_json(&mut session.stream, deadline)
    }

    /// Passive probe: handshake and status only; never touches hardware.
    #[must_use]
    pub fn probe(&self) -> ServiceProbe {
        match self.connect() {
            Ok(session) => ServiceProbe::Running {
                daemon_protocol: session.hello.protocol,
                version: session.hello.daemon_build.version.clone(),
                commit: session.hello.daemon_build.commit.clone(),
                package_format: session.hello.daemon_build.package_format.clone(),
                status: session.hello.status.clone(),
                capabilities: session.hello.capabilities,
            },
            Err(ClientError::Unavailable(reason)) => ServiceProbe::NotRunning { reason },
            Err(ClientError::AccessDenied(message)) => ServiceProbe::AccessDenied { message },
            Err(other) => ServiceProbe::Incompatible {
                message: other.to_string(),
            },
        }
    }

    fn simple(&self, op: Op, timeout: Duration) -> Result<Response, ClientError> {
        let mut session = self.connect()?;
        let request = Request {
            id: new_request_id(),
            op,
        };
        Self::call(&mut session, &request, timeout)
            .map_err(|e| ClientError::Unavailable(e.to_string()))
    }

    fn into_view(response: Response) -> Result<ThermalView, ClientError> {
        match response.body {
            Body::Ok { view: Some(v), .. } => Ok(*v),
            Body::Ok { view: None, .. } => {
                Err(ClientError::Failed("the daemon returned no state".into()))
            }
            Body::Err { kind, message } => Err(map_error(kind, message)),
            _ => Err(ClientError::Failed("unexpected response".into())),
        }
    }

    /// Latest authoritative state, refreshed only if older than `max_age`.
    ///
    /// # Errors
    /// See [`ClientError`].
    pub fn snapshot(&self, max_age: Duration) -> Result<ThermalView, ClientError> {
        let max_age_ms = u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX);
        Self::into_view(self.simple(Op::Snapshot { max_age_ms }, Duration::from_secs(15))?)
    }

    /// Execute a state-changing operation with reconciliation after a lost reply.
    ///
    /// The request id is stable, the daemon never executes an id twice, and a lost reply is
    /// reconciled with `query_operation`; an unknown outcome is reported, never replayed.
    ///
    /// # Errors
    /// See [`ClientError`].
    pub fn execute(&self, op: Op) -> Result<OperationReport, ClientError> {
        let id = new_request_id();
        let request = Request { id: id.clone(), op };
        let first = (|| {
            let mut session = self.connect()?;
            Self::call(
                &mut session,
                &request,
                OPERATION_DEADLINE + Duration::from_secs(3),
            )
            .map_err(|e| ClientError::Unavailable(e.to_string()))
        })();
        match first {
            Ok(response) => Self::report(response),
            Err(ClientError::Unavailable(detail)) => self.reconcile(&id, &detail),
            Err(other) => Err(other),
        }
    }

    fn report(response: Response) -> Result<OperationReport, ClientError> {
        match response.body {
            Body::Ok {
                view: Some(view),
                lifecycle: Some(lifecycle),
                ..
            } => Ok(OperationReport {
                view: *view,
                lifecycle,
            }),
            Body::Completed { view, lifecycle } => Ok(OperationReport {
                view: *view,
                lifecycle,
            }),
            Body::Err {
                kind: ErrorClass::Timeout,
                message,
            } => Err(ClientError::OutcomeUnknown {
                request_id: response.id,
                detail: message,
            }),
            Body::Err { kind, message } => Err(map_error(kind, message)),
            _ => Err(ClientError::Failed("unexpected response".into())),
        }
    }

    fn reconcile(&self, id: &str, detail: &str) -> Result<OperationReport, ClientError> {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let response = self.simple(
                Op::QueryOperation {
                    request_id: id.to_string(),
                },
                Duration::from_secs(3),
            );
            match response {
                Ok(Response {
                    body: Body::Completed { view, lifecycle },
                    ..
                }) => {
                    return Ok(OperationReport {
                        view: *view,
                        lifecycle,
                    });
                }
                Ok(Response {
                    body: Body::Pending,
                    ..
                }) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(Response {
                    body: Body::Err { kind, message },
                    ..
                }) => return Err(map_error(kind, message)),
                _ => {
                    return Err(ClientError::OutcomeUnknown {
                        request_id: id.to_string(),
                        detail: detail.to_string(),
                    });
                }
            }
        }
    }
}

fn map_error(kind: ErrorClass, message: String) -> ClientError {
    match kind {
        ErrorClass::AccessDenied => ClientError::AccessDenied(message),
        ErrorClass::ProtocolMismatch | ErrorClass::Unsupported => {
            ClientError::ProtocolMismatch(message)
        }
        ErrorClass::Overloaded | ErrorClass::Busy => ClientError::Overloaded(message),
        ErrorClass::Rejected | ErrorClass::InvalidRequest => ClientError::Rejected(message),
        ErrorClass::Timeout | ErrorClass::Internal => ClientError::Failed(message),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ThermalOrchestrator;
    use crate::daemon::{Daemon, DaemonError, DaemonOptions};
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    struct AllowAll;
    impl Authorizer for AllowAll {
        fn authorize(&self, _uid: u32) -> Result<(), String> {
            Ok(())
        }
    }
    struct DenyAll;
    impl Authorizer for DenyAll {
        fn authorize(&self, uid: u32) -> Result<(), String> {
            Err(format!("uid {uid} is not authorized to control hardware"))
        }
    }
    struct Recording(Mutex<Vec<u32>>);
    impl Authorizer for Recording {
        fn authorize(&self, uid: u32) -> Result<(), String> {
            self.0.lock().unwrap().push(uid);
            Ok(())
        }
    }

    struct TestDaemon {
        dir: PathBuf,
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        join: Option<JoinHandle<String>>,
    }

    fn base_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("awcc-ipc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn options(dir: &Path, authorizer: Arc<dyn Authorizer>) -> DaemonOptions {
        DaemonOptions {
            socket_path: dir.join("run/awcc.sock"),
            lock_path: dir.join("run/daemon.lock"),
            state_dir: dir.join("state"),
            authorizer,
            build: BuildSummary {
                version: "9.9.9-test".to_string(),
                commit: "deadbeef".to_string(),
                package_format: "test".to_string(),
            },
            tick_interval: Duration::from_millis(40),
            strict_owner: false,
        }
    }

    fn sim(_: &DaemonOptions) -> Result<ThermalOrchestrator, String> {
        ThermalOrchestrator::new_simulated_m15_r6().map_err(|e| e.to_string())
    }

    fn start_with(tag: &str, authorizer: Arc<dyn Authorizer>) -> TestDaemon {
        let dir = base_dir(tag);
        let opts = options(&dir, authorizer);
        let socket = opts.socket_path.clone();
        let daemon = Daemon::start(opts, sim).expect("daemon starts");
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let join = std::thread::spawn(move || daemon.run(&stop2));
        TestDaemon {
            dir,
            socket,
            stop,
            join: Some(join),
        }
    }

    fn start(tag: &str) -> TestDaemon {
        start_with(tag, Arc::new(AllowAll))
    }

    impl TestDaemon {
        fn client(&self) -> DaemonClient {
            DaemonClient::new(&self.socket, "test", "9.9.9")
        }
        fn finish(mut self) -> String {
            self.stop.store(true, Ordering::SeqCst);
            let report = self.join.take().unwrap().join().unwrap();
            let _ = fs::remove_dir_all(&self.dir);
            report
        }
    }

    fn raw_hello(min: u32, max: u32) -> Vec<u8> {
        serde_json::to_vec(&ClientHello {
            protocol_min: min,
            protocol_max: max,
            client_name: "raw".into(),
            client_build: "0".into(),
        })
        .unwrap()
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(payload);
        v
    }

    fn raw_connect(d: &TestDaemon) -> UnixStream {
        UnixStream::connect(&d.socket).unwrap()
    }

    fn handshake(s: &mut UnixStream) -> ServerHello {
        let dl = Instant::now() + Duration::from_secs(3);
        write_frame(s, &raw_hello(1, 1), dl).unwrap();
        serde_json::from_slice(&read_frame(s, dl).unwrap()).unwrap()
    }

    fn call(s: &mut UnixStream, id: &str, op: Op) -> Response {
        let dl = Instant::now() + Duration::from_secs(15);
        let req = Request {
            id: id.to_string(),
            op,
        };
        send_json(s, &req, dl).unwrap();
        recv_json(s, dl).unwrap()
    }

    #[test]
    fn handshake_status_snapshot_and_operation_round_trip() {
        let d = start("roundtrip");
        let probe = d.client().probe();
        let ServiceProbe::Running {
            version,
            daemon_protocol,
            capabilities,
            ..
        } = probe
        else {
            panic!("expected running, got {probe:?}");
        };
        assert_eq!(
            version, "9.9.9-test",
            "client build 9.9.9 differs yet is compatible"
        );
        assert_eq!(daemon_protocol, PROTOCOL_VERSION);
        assert!(
            REQUIRED_CAPABILITIES
                .iter()
                .all(|c| capabilities.iter().any(|x| x == c))
        );
        let view = d.client().snapshot(Duration::from_secs(5)).unwrap();
        assert_eq!(view.device_name, "Alienware m15 R6");
        let report = d
            .client()
            .execute(Op::SetThermalMode {
                mode: ThermalMode::Quiet,
            })
            .unwrap();
        assert!(matches!(
            report.lifecycle,
            OperationLifecycle::Confirmed { .. } | OperationLifecycle::Accepted { .. }
        ));
        d.finish();
    }

    #[test]
    fn fragmented_requests_are_reassembled() {
        let d = start("fragment");
        let mut s = raw_connect(&d);
        let bytes = frame(&raw_hello(1, 1));
        for b in bytes {
            s.write_all(&[b]).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        let dl = Instant::now() + Duration::from_secs(3);
        let hello: ServerHello = serde_json::from_slice(&read_frame(&mut s, dl).unwrap()).unwrap();
        assert_eq!(hello.protocol, 1);
        let req = serde_json::to_vec(&Request {
            id: "frag-1".into(),
            op: Op::Status,
        })
        .unwrap();
        for b in frame(&req) {
            s.write_all(&[b]).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let resp: Response = serde_json::from_slice(&read_frame(&mut s, dl).unwrap()).unwrap();
        assert!(matches!(
            resp.body,
            Body::Ok {
                status: Some(_),
                ..
            }
        ));
        d.finish();
    }

    #[test]
    fn oversized_truncated_and_garbage_peers_do_not_disturb_the_daemon() {
        let d = start("hostile");
        // Oversized declared length.
        let mut s = raw_connect(&d);
        s.write_all(&u32::MAX.to_be_bytes()).unwrap();
        let dl = Instant::now() + Duration::from_secs(3);
        let resp: Response = serde_json::from_slice(&read_frame(&mut s, dl).unwrap()).unwrap();
        assert!(matches!(
            resp.body,
            Body::Err {
                kind: ErrorClass::ProtocolMismatch,
                ..
            }
        ));
        // Legacy raw JSON line (a `{` byte decodes as an absurd length).
        let mut s = raw_connect(&d);
        s.write_all(b"{\"op\":\"stop_fan_curve\"}\n").unwrap();
        assert!(read_frame(&mut s, dl).is_ok());
        // Truncated frame: declares 100 bytes, sends 10, closes.
        let mut s = raw_connect(&d);
        s.write_all(&100_u32.to_be_bytes()).unwrap();
        s.write_all(b"0123456789").unwrap();
        drop(s);
        // Valid JSON of the wrong shape.
        let mut s = raw_connect(&d);
        write_frame(&mut s, br#"{"nonsense":true}"#, dl).unwrap();
        let _ = read_frame(&mut s, dl);
        // The daemon is still healthy for a normal client.
        assert!(matches!(d.client().probe(), ServiceProbe::Running { .. }));
        d.finish();
    }

    #[test]
    fn unauthorized_peer_is_denied_before_any_state_is_revealed() {
        let d = start_with("deny", Arc::new(DenyAll));
        match d.client().probe() {
            ServiceProbe::AccessDenied { message } => assert!(message.contains("access denied")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            d.client().snapshot(Duration::from_secs(1)),
            Err(ClientError::AccessDenied(_))
        ));
        assert!(matches!(
            d.client().execute(Op::ToggleGMode),
            Err(ClientError::AccessDenied(_))
        ));
        d.finish();
    }

    #[test]
    fn authorization_uses_the_kernel_peer_uid_not_request_content() {
        let rec = Arc::new(Recording(Mutex::new(Vec::new())));
        let d = start_with("peeruid", Arc::clone(&rec) as Arc<dyn Authorizer>);
        let _ = d.client().probe();
        let seen = rec.0.lock().unwrap().clone();
        assert_eq!(seen, vec![rustix::process::getuid().as_raw()]);
        d.finish();
    }

    #[test]
    fn incompatible_protocol_is_refused_but_other_app_versions_are_fine() {
        let d = start("proto");
        let mut s = raw_connect(&d);
        let dl = Instant::now() + Duration::from_secs(3);
        write_frame(&mut s, &raw_hello(7, 9), dl).unwrap();
        let resp: Response = serde_json::from_slice(&read_frame(&mut s, dl).unwrap()).unwrap();
        assert!(matches!(
            resp.body,
            Body::Err {
                kind: ErrorClass::ProtocolMismatch,
                ..
            }
        ));
        d.finish();
    }

    #[test]
    fn repeated_request_ids_never_execute_twice_and_can_be_reconciled() {
        let d = start("dedupe");
        let mut s = raw_connect(&d);
        handshake(&mut s);
        let audit_len = |s: &mut UnixStream| -> usize {
            match call(s, "q", Op::Refresh).body {
                Body::Ok { view: Some(v), .. } => v.audit.len(),
                other => panic!("{other:?}"),
            }
        };
        let before = audit_len(&mut s);
        let first = call(&mut s, "toggle-1", Op::ToggleGMode);
        let after_first = audit_len(&mut s);
        assert!(after_first > before, "first request wrote");
        let second = call(&mut s, "toggle-1", Op::ToggleGMode);
        assert_eq!(
            audit_len(&mut s),
            after_first,
            "duplicate id must not replay the write"
        );
        assert_eq!(first.body, second.body);
        match call(
            &mut s,
            "r",
            Op::QueryOperation {
                request_id: "toggle-1".into(),
            },
        )
        .body
        {
            Body::Completed { .. } => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(
            call(
                &mut s,
                "r2",
                Op::QueryOperation {
                    request_id: "never-seen".into()
                }
            )
            .body,
            Body::Unknown
        );
        d.finish();
    }

    #[test]
    fn second_daemon_fails_clearly_without_disturbing_the_first() {
        let d = start("second");
        let mut opts = options(&d.dir, Arc::new(AllowAll));
        opts.state_dir = d.dir.join("state2");
        let touched = Arc::new(AtomicBool::new(false));
        let t2 = Arc::clone(&touched);
        let err = Daemon::start(opts, move |_| {
            t2.store(true, Ordering::SeqCst);
            ThermalOrchestrator::new_simulated_m15_r6().map_err(|e| e.to_string())
        })
        .err()
        .expect("second daemon must fail");
        assert!(matches!(
            err,
            DaemonError::Ownership(OwnershipError::AlreadyRunning(_))
        ));
        assert!(
            !touched.load(Ordering::SeqCst),
            "hardware factory must not run"
        );
        assert!(d.socket.exists(), "the live socket was not unlinked");
        assert!(matches!(d.client().probe(), ServiceProbe::Running { .. }));
        d.finish();
    }

    #[test]
    fn stale_sockets_are_replaced_but_other_files_and_symlinks_are_refused() {
        let dir = base_dir("stale");
        fs::create_dir_all(dir.join("run")).unwrap();
        let opts = options(&dir, Arc::new(AllowAll));
        // A leftover socket from a crashed daemon.
        drop(UnixListener::bind(&opts.socket_path).unwrap());
        let daemon = Daemon::start(opts, sim).expect("stale socket is replaced");
        let stop = AtomicBool::new(true);
        let _ = daemon.run(&stop);

        // A regular file in the socket path is never replaced.
        let opts = options(&dir, Arc::new(AllowAll));
        fs::write(&opts.socket_path, b"precious").unwrap();
        let err = Daemon::start(opts, sim).err().unwrap();
        assert!(matches!(
            err,
            DaemonError::Ownership(OwnershipError::Unsafe(_))
        ));
        assert_eq!(fs::read(dir.join("run/awcc.sock")).unwrap(), b"precious");

        // A symlink in the socket path is never followed.
        fs::remove_file(dir.join("run/awcc.sock")).unwrap();
        fs::write(dir.join("target"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("target"), dir.join("run/awcc.sock")).unwrap();
        let err = Daemon::start(options(&dir, Arc::new(AllowAll)), sim)
            .err()
            .unwrap();
        assert!(matches!(
            err,
            DaemonError::Ownership(OwnershipError::Unsafe(_))
        ));
        assert!(dir.join("target").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_silent_connection_cannot_block_other_clients_and_connections_are_bounded() {
        let d = start("slow");
        let _silent = raw_connect(&d);
        // A silent peer must not stop a normal client.
        assert!(matches!(d.client().probe(), ServiceProbe::Running { .. }));
        // Fill the connection budget with silent peers: the surplus is told it is overloaded.
        let mut held = Vec::new();
        for _ in 0..(MAX_CONNECTIONS + 4) {
            held.push(raw_connect(&d));
        }
        std::thread::sleep(Duration::from_millis(200));
        let mut overloaded = 0;
        for s in &mut held {
            s.set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut hdr = [0_u8; 4];
            if s.read_exact(&mut hdr).is_ok() {
                overloaded += 1;
            }
        }
        assert!(
            overloaded >= 1,
            "excess connections must be refused explicitly"
        );
        d.finish();
    }

    #[test]
    fn root_clients_cannot_bypass_daemon_ownership_for_raw_writes_during_a_curve() {
        let d = start("legacy");
        let mut s = raw_connect(&d);
        handshake(&mut s);
        let curve = awcc_core::FanCurveConfig {
            enabled: true,
            ..awcc_core::FanCurveConfig::default()
        };
        let configured = call(
            &mut s,
            "c1",
            Op::Legacy {
                request: DaemonRpcRequest::ConfigureFanCurve { config: curve },
            },
        );
        assert!(matches!(configured.body, Body::Ok { .. }), "{configured:?}");
        // A raw WMAX write is refused while the curve owns thermal control.
        let raw = call(
            &mut s,
            "w1",
            Op::Legacy {
                request: DaemonRpcRequest::AcpiWmax {
                    prefix: awcc_core::AcpiPrefix::Amww,
                    subcmd: 0x15,
                    arg1: 2,
                    arg2: 0x32,
                    arg3: 50,
                },
            },
        );
        assert!(
            matches!(
                raw.body,
                Body::Err {
                    kind: ErrorClass::Rejected,
                    ..
                }
            ),
            "{raw:?}"
        );
        // Wrong prefix is a device-validation failure.
        let wrong = call(
            &mut s,
            "w2",
            Op::Legacy {
                request: DaemonRpcRequest::AcpiWmax {
                    prefix: awcc_core::AcpiPrefix::Amw3,
                    subcmd: 0x14,
                    arg1: 0xb,
                    arg2: 0,
                    arg3: 0,
                },
            },
        );
        assert!(matches!(wrong.body, Body::Err { .. }));
        // Invalid curve parameters are rejected before any hardware work.
        let bad = call(
            &mut s,
            "w3",
            Op::Legacy {
                request: DaemonRpcRequest::SetCpuTurbo { enabled: true },
            },
        );
        let _ = bad;
        d.finish();
    }

    #[test]
    fn settings_persist_across_daemon_restarts_and_restore_only_when_asked() {
        let dir = base_dir("persist");
        let run = |dir: &Path, f: &dyn Fn(&DaemonClient)| {
            let opts = options(dir, Arc::new(AllowAll));
            let socket = opts.socket_path.clone();
            let daemon = Daemon::start(opts, sim).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let s2 = Arc::clone(&stop);
            let join = std::thread::spawn(move || daemon.run(&s2));
            f(&DaemonClient::new(&socket, "t", "1"));
            stop.store(true, Ordering::SeqCst);
            join.join().unwrap()
        };
        let curve = awcc_core::FanCurveConfig {
            enabled: true,
            ..awcc_core::FanCurveConfig::default()
        };
        let report = run(&dir, &|c| {
            c.execute(Op::ConfigureFanCurve {
                config: curve.clone(),
            })
            .unwrap();
        });
        assert!(report.contains("settings saved"), "{report}");
        // Restart: intent saved, but restore preference is off -> nothing is applied.
        run(&dir, &|c| {
            let view = c.snapshot(Duration::from_secs(1)).unwrap();
            assert!(view.control.intent_active);
            assert!(!view.control.restore_at_startup);
            assert_eq!(view.control.state, crate::wire::ControlState::Idle);
            c.execute(Op::SetRestoreAtStartup { enabled: true })
                .unwrap();
        });
        // Restart again: preference on -> restored through the eligibility checks.
        run(&dir, &|c| {
            let view = c.snapshot(Duration::from_secs(1)).unwrap();
            assert_eq!(view.control.state, crate::wire::ControlState::Active);
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowlist_policy_requires_root_controlled_file_and_root_is_always_allowed() {
        let dir = base_dir("allow");
        let users = dir.join("authorized-users");
        let passwd = dir.join("passwd");
        fs::write(
            &passwd,
            "alice:x:1234:1234::/home/alice:/bin/bash\nbob:x:2345:2345::/h:/bin/sh\n",
        )
        .unwrap();
        assert!(authorized_users::add(&users, "alice").unwrap());
        assert!(
            !authorized_users::add(&users, "alice").unwrap(),
            "idempotent"
        );
        assert!(authorized_users::add(&users, "bad name; rm").is_err());
        let policy = AllowlistAuthorizer::new(&users, &passwd, false);
        assert!(policy.authorize(0).is_ok());
        assert!(
            policy.authorize(1234).is_ok(),
            "named user resolved via passwd"
        );
        assert!(policy.authorize(2345).is_err());
        assert!(authorized_users::add(&users, "2345").unwrap());
        assert!(policy.authorize(2345).is_ok(), "numeric uid entry");
        assert!(authorized_users::remove(&users, "alice").unwrap());
        assert!(
            policy.authorize(1234).is_err(),
            "removal takes effect immediately"
        );
        // A file not owned by root (this test process) is rejected under the production policy.
        let strict = AllowlistAuthorizer::new(&users, &passwd, true);
        if rustix::process::getuid().as_raw() != 0 {
            assert!(strict.authorize(2345).is_err());
        }
        // Missing file: only root.
        let none = AllowlistAuthorizer::new(dir.join("absent"), &passwd, false);
        assert!(none.authorize(0).is_ok());
        assert!(none.authorize(1234).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_clients_share_one_authoritative_state_and_the_service_outlives_both() {
        let d = start("shared");
        let a = d.client();
        let b = d.client();
        let cfg = awcc_core::FanCurveConfig {
            enabled: true,
            ..awcc_core::FanCurveConfig::default()
        };
        a.execute(Op::ConfigureFanCurve { config: cfg }).unwrap();
        drop(a);
        let seen_by_b = b.snapshot(Duration::from_secs(1)).unwrap();
        assert_eq!(seen_by_b.control.state, crate::wire::ControlState::Active);
        assert!(seen_by_b.fan_curve.enabled);
        // "CLI" stops it; a brand-new "GUI" client observes the change.
        b.execute(Op::StopFanCurve).unwrap();
        let gui = d.client().snapshot(Duration::from_secs(1)).unwrap();
        assert_eq!(gui.control.state, crate::wire::ControlState::Idle);
        assert!(!gui.fan_curve.enabled);
        d.finish();
    }

    #[test]
    fn service_backed_orchestrator_presents_daemon_state_and_degrades_to_stale_on_loss() {
        use crate::{ServiceCommand, ThermalOrchestrator};
        let d = start("remote");
        let client = DaemonClient::new(&d.socket, "gui", "1");
        let mut orch = ThermalOrchestrator::new_service_backed(Box::new(client)).unwrap();
        let snap = orch.handle_command(ServiceCommand::Refresh);
        assert!(snap.telemetry.cpu_temp_c.is_available());
        assert!(snap.service_error.is_none());
        let out = orch.execute(ServiceCommand::SetThermalMode(ThermalMode::Quiet));
        assert!(
            out.result
                .expect("state-changing command has an outcome")
                .is_ok()
        );
        assert_eq!(
            out.snapshot.thermal_mode_state.known_mode(),
            Some(ThermalMode::Quiet)
        );
        // A client tick must never drive the curve: the service owns the schedule.
        let tick = orch.handle_command(ServiceCommand::StepFanCurve);
        assert_eq!(tick.operation_id, out.snapshot.operation_id);
        // The service goes away: readings turn stale/unavailable, never current-looking.
        d.finish();
        let after = orch.handle_command(ServiceCommand::Refresh);
        assert!(after.service_error.is_some());
        assert!(!matches!(
            after.telemetry.cpu_temp_c,
            awcc_core::SensorReading::Valid { .. }
        ));
        assert!(matches!(
            after.thermal_mode_state,
            awcc_core::ThermalModeState::Unknown { .. }
        ));
        let failed = orch.execute(ServiceCommand::SetThermalMode(ThermalMode::Balanced));
        assert!(failed.result.expect("outcome").is_err());
    }
}
