//! Installer lifecycle for the privileged `awccd` service of an AppImage (or tarball) deployment.
//!
//! # What this module does
//!
//! * [`inspect`] — passive status: who owns `awccd.service`, whether it runs, whether it is
//!   enabled, which build the installer deployed. It never writes and never prompts.
//! * [`apply`] — the **root side**: `install` and `update` stage hash-verified releases,
//!   atomically switch the `current` symlink, health-check and attempt journaled rollback on
//!   failure. `setup` authorizes the invoking user and enables an existing owned service without
//!   deploying files. `uninstall [--purge]` removes an installer-owned deployment. Setup and
//!   uninstall report failures but may leave earlier steps applied.
//! * [`elevate_and_apply`] — the **user side**: relaunches the running binary under `pkexec` and
//!   replays the child's progress events.
//!
//! # Deployment layout (installer-managed; native packages keep `/usr/bin` + `/usr/lib/systemd`)
//!
//! ```text
//! /opt/awcc-rs/service/releases/<build-id>/{bin/awcc-cli, lib/*, share/awcc/database.json,
//!                                           MANIFEST.sha256}
//! /opt/awcc-rs/service/current -> releases/<build-id>        (atomic symlink switch)
//! /etc/systemd/system/awccd.service                          (generated, ExecStart via current/)
//! /var/lib/awcc/install-owner.json                           (ownership record)
//! /var/lib/awcc/install-journal.json                         (only while an operation runs)
//! /var/lib/awcc/.install.lock                                (flock, serializes operations)
//! ```
//!
//! # Trust model
//!
//! The administrator who authorizes the elevation (polkit prompt, `sudo`) authorizes *this
//! release*. `MANIFEST.sha256` inside the same release detects corruption and tampering of the
//! extracted tree; it does **not** authenticate the publisher. The installer never changes
//! `fuse.conf`, setuid bits, or sudoers, never overwrites a unit it does not own, and never
//! unmasks anything.
//!
//! # Testability
//!
//! [`InstallPaths::with_root`] relocates every fixed destination below a directory and turns the
//! root-ownership checks off, so the whole lifecycle runs unprivileged in a temporary directory
//! with a fake [`HostRunner`] and [`HealthProbe`].

mod apply;
mod elevate;
pub mod host;
pub mod payload;
pub mod unit;

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use awcc_diagnostics::hash::sha256_hex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use apply::{ApplyContext, ApplyOptions, HealthProbe, apply, run_apply_command};
pub use elevate::{
    ApplyEvent, ElevationError, LaunchTarget, build_pkexec_command, elevate_and_apply,
};
pub use host::{HostCommand, HostError, HostOutput, HostRunner, HostTool, SystemRunner};
pub use payload::{PayloadError, PayloadSource, VerifiedPayload};

/// Value of `installer` in the ownership record.
pub const INSTALLER_ID: &str = "awcc-appimage-installer";
/// Name of the managed unit.
pub const UNIT_NAME: &str = "awccd.service";
/// Schema version of the ownership record and journal.
pub const SCHEMA_VERSION: u32 = 1;

/// Requested lifecycle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ServiceAction {
    Install,
    Update,
    /// Authorize the invoking user and enable/start an existing known service.
    Setup,
    Uninstall {
        purge: bool,
    },
}

impl ServiceAction {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Setup => "setup",
            Self::Uninstall { .. } => "uninstall",
        }
    }
}

/// Fixed destinations, relocatable for tests.
#[derive(Debug, Clone)]
pub struct InstallPaths {
    root: PathBuf,
    privileged: bool,
}

impl InstallPaths {
    /// The real system: root `/`, euid 0 required, root-ownership of ancestors enforced.
    #[must_use]
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            privileged: true,
        }
    }

    /// Everything below `root`; ownership/euid checks are off (tests, dry runs in a sandbox).
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            privileged: false,
        }
    }

    #[must_use]
    pub const fn is_privileged(&self) -> bool {
        self.privileged
    }

    /// Map a logical absolute path (`/etc/...`) below the configured root.
    #[must_use]
    pub fn join(&self, logical: &str) -> PathBuf {
        self.root.join(logical.trim_start_matches('/'))
    }

    #[must_use]
    pub fn systemd_runtime_dir(&self) -> PathBuf {
        self.join("/run/systemd/system")
    }
    #[must_use]
    pub fn service_dir(&self) -> PathBuf {
        self.join(unit::SERVICE_ROOT)
    }
    #[must_use]
    pub fn releases_dir(&self) -> PathBuf {
        self.join(&format!("{}/releases", unit::SERVICE_ROOT))
    }
    #[must_use]
    pub fn release_dir(&self, build_id: &str) -> PathBuf {
        self.releases_dir().join(build_id)
    }
    #[must_use]
    pub fn current_link(&self) -> PathBuf {
        self.join(&format!("{}/current", unit::SERVICE_ROOT))
    }
    #[must_use]
    pub fn unit_path(&self) -> PathBuf {
        self.join(&format!("/etc/systemd/system/{UNIT_NAME}"))
    }
    #[must_use]
    pub fn dropin_dir(&self) -> PathBuf {
        self.join(&format!("/etc/systemd/system/{UNIT_NAME}.d"))
    }
    #[must_use]
    pub fn runtime_unit_path(&self) -> PathBuf {
        self.join(&format!("/run/systemd/system/{UNIT_NAME}"))
    }
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        self.join("/var/lib/awcc")
    }
    #[must_use]
    pub fn record_path(&self) -> PathBuf {
        self.state_dir().join("install-owner.json")
    }
    #[must_use]
    pub fn journal_path(&self) -> PathBuf {
        self.state_dir().join("install-journal.json")
    }
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.state_dir().join(".install.lock")
    }
    #[must_use]
    pub fn authorized_users_path(&self) -> PathBuf {
        self.join("/etc/awcc/authorized-users")
    }

    /// Unit paths owned by packages or administrators other than this installer.
    pub(crate) fn foreign_unit_candidates(&self) -> Vec<String> {
        [
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
            "/usr/local/lib/systemd/system",
            "/run/systemd/system",
        ]
        .iter()
        .map(|dir| format!("{dir}/{UNIT_NAME}"))
        .collect()
    }

    /// Logical (root-relative, absolute) form of a path below the configured root.
    pub(crate) fn logical(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).map_or_else(
            |_| path.display().to_string(),
            |rel| format!("/{}", rel.display()),
        )
    }
}

/// Who owns `awccd.service`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ownership {
    /// No unit and no installer record.
    None,
    /// Installed (and recorded) by this installer.
    AppimageInstaller { build_id: String },
    /// Provided by an OS package.
    NativePackage {
        manager: String,
        package: String,
        unit_path: String,
    },
    /// A unit exists that neither a package manager nor this installer claims.
    Unknown { unit_path: String, reason: String },
}

impl Ownership {
    /// Short stable label for UIs and JSON consumers.
    #[must_use]
    pub const fn managed_by(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AppimageInstaller { .. } => "appimage-installer",
            Self::NativePackage { .. } => "native-package",
            Self::Unknown { .. } => "manual",
        }
    }
}

/// Runtime state of the unit, from `systemctl show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    Missing,
    Stopped,
    Running,
    Activating,
    Deactivating,
    Failed,
    Masked,
    Unknown,
}

/// Passive status of the service installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatusReport {
    pub ownership: Ownership,
    /// Same as `ownership.managed_by()`, flattened for simple consumers.
    pub managed_by: String,
    pub unit_state: UnitState,
    /// `Some(true)` enabled, `Some(false)` disabled, `None` unknown/static.
    pub enabled: Option<bool>,
    pub masked: bool,
    pub installed_build_id: Option<String>,
    pub version: Option<String>,
    pub unit_path: Option<String>,
    /// Names of administrator drop-ins in `awccd.service.d` (never touched by the installer).
    pub dropins: Vec<String>,
    /// An earlier install/update was interrupted; the next run recovers it.
    pub interrupted_operation: bool,
    pub problems: Vec<String>,
}

/// Ownership record written after a successful install/update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub schema_version: u32,
    pub installer: String,
    pub build_id: String,
    pub version: String,
    pub installed_at_unix: u64,
    pub unit_path: String,
    pub unit_sha256: String,
    pub current_link: String,
    pub release_dir: String,
    pub installed_files: Vec<String>,
    /// `/etc/awcc/authorized-users` was created by the installer (removed on `--purge`).
    pub created_authorized_users: bool,
    /// Service intent before the apply that wrote this record.
    pub previous_state: Option<RecordedIntent>,
}

/// Enablement and active state captured before a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedIntent {
    pub enabled: Option<bool>,
    pub active: bool,
}

/// Progress milestones, emitted when a step *starts*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStep {
    Inspecting,
    Recovering,
    VerifyingPayload,
    Staging,
    VerifyingStaged,
    StoppingService,
    Switching,
    ReloadingSystemd,
    StartingService,
    HealthCheck,
    RollingBack,
    Cleanup,
    Removing,
    Done,
}

/// One progress notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub step: ProgressStep,
    pub message: String,
}

/// Result of a successful [`apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub action: String,
    pub outcome: ApplyOutcome,
    pub build_id: Option<String>,
    pub previous_build_id: Option<String>,
    pub enabled: Option<bool>,
    pub active: Option<bool>,
    /// An interrupted earlier operation was rolled back/finished first.
    pub recovered_interrupted: bool,
    pub purged: bool,
    pub notes: Vec<String>,
}

/// What an [`apply`] ended up doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyOutcome {
    Installed,
    Updated,
    /// An existing service was authorized, enabled, started and health-checked.
    Configured,
    /// Same build already installed and intact; nothing was touched.
    Unchanged,
    Uninstalled,
    /// Uninstall requested, nothing installed by this installer.
    NothingToUninstall,
}

/// How a rollback ended, embedded in failure errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSummary {
    pub restored: bool,
    pub problems: Vec<String>,
}

impl std::fmt::Display for RollbackSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.restored {
            write!(f, "The previous state was restored.")
        } else {
            write!(
                f,
                "Rollback was incomplete ({}); rerun the command to retry recovery.",
                self.problems.join("; ")
            )
        }
    }
}

/// Errors of the root-side lifecycle.
#[derive(Debug, Error)]
pub enum InstallError {
    #[error(
        "installing the service needs root (effective uid is {0}); use `service install` (it asks \
         for authorization) or run the AppImage as root with `--service-apply install`"
    )]
    NotRoot(u32),
    #[error(
        "systemd is not the running init system (/run/systemd/system is missing); the service \
         installer supports systemd only. Run `awcc-cli -d` as root by other means"
    )]
    UnsupportedInit,
    #[error(
        "awccd.service is masked ({path}); the installer never unmasks units. Run \
         `systemctl unmask awccd.service` yourself if you want it, then retry"
    )]
    Masked { path: String },
    #[error(
        "awccd.service is provided by the {manager} package `{package}` ({unit_path}); the \
         AppImage installer will not shadow it. Manage it through {manager}, or remove that \
         package first"
    )]
    NativePackage {
        manager: String,
        package: String,
        unit_path: String,
    },
    #[error(
        "an awccd.service at {unit_path} exists that this installer did not create ({reason}); \
         refusing to overwrite it. Remove or move it manually, then retry"
    )]
    UnknownUnit { unit_path: String, reason: String },
    #[error(
        "awccd.service is not installed; install a native package or run `service install` first"
    )]
    NotInstalled,
    #[error("service setup did not finish: {reason}")]
    SetupFailed { reason: String },
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error("unsafe path {path}: {reason}")]
    UnsafePath { path: String, reason: String },
    #[error("another AWCC service install/update/uninstall is already running (lock {path})")]
    LockHeld { path: String },
    #[error("release directory {path} already exists with different contents; uninstall first")]
    ReleaseCollision { path: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("`{command}` failed ({status}): {stderr}")]
    CommandFailed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("record {path} is invalid: {reason}")]
    BadRecord { path: String, reason: String },
    #[error("release {build_id} failed its health check: {reason}. {rollback}")]
    HealthCheckFailed {
        build_id: String,
        reason: String,
        rollback: RollbackSummary,
    },
    #[error("{step} failed: {reason}. {rollback}")]
    StepFailed {
        step: String,
        reason: String,
        rollback: RollbackSummary,
    },
}

impl InstallError {
    /// Stable machine-readable code (used in the elevated child's JSON events).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::NotRoot(_) => "not_root",
            Self::UnsupportedInit => "unsupported_init",
            Self::Masked { .. } => "masked",
            Self::NativePackage { .. } => "native_package",
            Self::UnknownUnit { .. } => "unknown_unit",
            Self::NotInstalled => "not_installed",
            Self::SetupFailed { .. } => "setup_failed",
            Self::Payload(_) => "payload",
            Self::UnsafePath { .. } => "unsafe_path",
            Self::LockHeld { .. } => "lock_held",
            Self::ReleaseCollision { .. } => "release_collision",
            Self::Io { .. } => "io",
            Self::Host(_) => "host",
            Self::CommandFailed { .. } => "command_failed",
            Self::BadRecord { .. } => "bad_record",
            Self::HealthCheckFailed { .. } => "health_check_failed",
            Self::StepFailed { .. } => "step_failed",
        }
    }
}

pub(crate) fn io_context(context: impl Into<String>) -> impl FnOnce(io::Error) -> InstallError {
    let context = context.into();
    move |source| InstallError::Io { context, source }
}

// ---------------------------------------------------------------------------------------------
// Filesystem helpers shared by inspect and apply.
// ---------------------------------------------------------------------------------------------

/// Create (if needed) every directory of `logical` below the root, rejecting symlinks, non-dirs,
/// and (when privileged) directories that are not root-owned or are group/world-writable.
pub(crate) fn ensure_dir_chain(
    paths: &InstallPaths,
    logical: &str,
    mode: u32,
) -> Result<PathBuf, InstallError> {
    directory_chain(paths, logical, Some(mode))
}

/// Validate existing destination ancestors without creating absent directories. Recovery and
/// cleanup must call this before following a previously used installation path.
pub(crate) fn check_existing_dir_chain(
    paths: &InstallPaths,
    logical: &str,
) -> Result<(), InstallError> {
    directory_chain(paths, logical, None).map(|_| ())
}

fn directory_chain(
    paths: &InstallPaths,
    logical: &str,
    create_mode: Option<u32>,
) -> Result<PathBuf, InstallError> {
    let mut current = paths.root.clone();
    for component in logical.split('/').filter(|c| !c.is_empty()) {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(InstallError::UnsafePath {
                        path: current.display().to_string(),
                        reason: "must be a real directory, not a symlink or file".to_owned(),
                    });
                }
                if paths.privileged && (meta.uid() != 0 || meta.mode() & 0o022 != 0) {
                    return Err(InstallError::UnsafePath {
                        path: current.display().to_string(),
                        reason: "must be owned by root and not writable by group or others"
                            .to_owned(),
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(mode) = create_mode else {
                    return Ok(current);
                };
                DirBuilder::new()
                    .mode(mode)
                    .create(&current)
                    .map_err(io_context(format!("creating {}", current.display())))?;
                // Honor the requested mode independent of the umask.
                fs::set_permissions(&current, fs::Permissions::from_mode(mode))
                    .map_err(io_context(format!("chmod {}", current.display())))?;
            }
            Err(error) => {
                return Err(InstallError::Io {
                    context: format!("inspecting {}", current.display()),
                    source: error,
                });
            }
        }
    }
    Ok(current)
}

/// Write `bytes` to `path` atomically (temp file in the same directory, fsync, rename).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), InstallError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        fs::rename(&temp, path)
    })();
    result.map_err(|source| {
        let _ = fs::remove_file(&temp);
        InstallError::Io {
            context: format!("writing {}", path.display()),
            source,
        }
    })
}

pub(crate) fn read_record(paths: &InstallPaths) -> Result<Option<OwnerRecord>, InstallError> {
    let path = paths.record_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(InstallError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    let record: OwnerRecord =
        serde_json::from_slice(&bytes).map_err(|error| InstallError::BadRecord {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    if record.installer != INSTALLER_ID || record.schema_version != SCHEMA_VERSION {
        return Err(InstallError::BadRecord {
            path: path.display().to_string(),
            reason: "written by a different installer or schema version".to_owned(),
        });
    }
    Ok(Some(record))
}

// ---------------------------------------------------------------------------------------------
// systemctl wrapper.
// ---------------------------------------------------------------------------------------------

/// Properties of the unit from `systemctl show`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UnitProps {
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub unit_file_state: String,
    pub fragment_path: String,
}

impl UnitProps {
    pub(crate) fn enabled(&self) -> Option<bool> {
        match self.unit_file_state.as_str() {
            "enabled" | "enabled-runtime" => Some(true),
            "disabled" => Some(false),
            _ => None,
        }
    }

    pub(crate) fn active(&self) -> bool {
        matches!(
            self.active_state.as_str(),
            "active" | "activating" | "reloading"
        )
    }

    pub(crate) fn state(&self) -> UnitState {
        if self.load_state == "masked" {
            return UnitState::Masked;
        }
        match self.active_state.as_str() {
            "active" | "reloading" => UnitState::Running,
            "activating" => UnitState::Activating,
            "deactivating" => UnitState::Deactivating,
            "failed" => UnitState::Failed,
            "inactive" if self.load_state == "not-found" => UnitState::Missing,
            "inactive" => UnitState::Stopped,
            _ if self.load_state == "not-found" => UnitState::Missing,
            _ => UnitState::Unknown,
        }
    }
}

pub(crate) struct Systemd<'a> {
    pub runner: &'a dyn HostRunner,
    pub timeout: Duration,
}

impl Systemd<'_> {
    fn command(&self, verb_and_args: &[&str]) -> HostCommand {
        let mut args = vec!["--no-ask-password", "--no-pager"];
        args.extend_from_slice(verb_and_args);
        HostCommand::new(HostTool::Systemctl, args).with_timeout(self.timeout)
    }

    pub(crate) fn show(&self) -> Result<UnitProps, InstallError> {
        let command = self.command(&[
            "show",
            UNIT_NAME,
            "--property=LoadState,ActiveState,SubState,UnitFileState,FragmentPath",
        ]);
        let output = self.runner.run(&command, None)?;
        if !output.success() {
            return Err(InstallError::CommandFailed {
                command: command.display(),
                status: output
                    .code
                    .map_or_else(|| "killed by signal".to_owned(), |c| format!("exit {c}")),
                stderr: output.stderr.trim().to_owned(),
            });
        }
        let mut props = UnitProps::default();
        for line in output.stdout.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let slot = match key {
                "LoadState" => &mut props.load_state,
                "ActiveState" => &mut props.active_state,
                "SubState" => &mut props.sub_state,
                "UnitFileState" => &mut props.unit_file_state,
                "FragmentPath" => &mut props.fragment_path,
                _ => continue,
            };
            value.trim().clone_into(slot);
        }
        if props.load_state.is_empty() || props.active_state.is_empty() {
            return Err(InstallError::CommandFailed {
                command: command.display(),
                status: "incomplete response".to_owned(),
                stderr: "systemd did not return LoadState and ActiveState".to_owned(),
            });
        }
        Ok(props)
    }

    /// Run `systemctl <verb> [awccd.service]`, failing on a non-zero exit.
    pub(crate) fn act(&self, verb: &str, with_unit: bool) -> Result<(), InstallError> {
        let mut args = vec![verb];
        if with_unit {
            args.push(UNIT_NAME);
        }
        let command = self.command(&args);
        let output = self.runner.run(&command, None)?;
        if output.success() {
            Ok(())
        } else {
            Err(InstallError::CommandFailed {
                command: command.display(),
                status: output
                    .code
                    .map_or_else(|| "killed by signal".to_owned(), |c| format!("exit {c}")),
                stderr: output.stderr.trim().to_owned(),
            })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Detection and passive inspection.
// ---------------------------------------------------------------------------------------------

pub(crate) struct Detection {
    pub ownership: Ownership,
    pub record: Option<OwnerRecord>,
    /// The record and on-disk unit agree (or the unit is gone): this installer may modify it.
    pub ours: bool,
    pub masked: bool,
    pub props: Option<UnitProps>,
    pub unit_exists: bool,
    pub dropins: Vec<String>,
    pub problems: Vec<String>,
}

fn is_mask_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        && fs::read_link(path).is_ok_and(|t| t == Path::new("/dev/null"))
}

/// Ask the package managers who owns `logical`. First hit wins.
fn query_native(
    runner: &dyn HostRunner,
    logical: &str,
    problems: &mut Vec<String>,
) -> Option<(String, String)> {
    let queries = [
        (
            "rpm",
            HostCommand::new(
                HostTool::Rpm,
                ["-qf", "--queryformat", "%{NAME}\\n", "--", logical],
            ),
        ),
        (
            "dpkg",
            HostCommand::new(HostTool::Dpkg, ["-S", "--", logical]),
        ),
        (
            "pacman",
            HostCommand::new(HostTool::Pacman, ["-Qo", "-q", "--", logical]),
        ),
    ];
    for (manager, command) in queries {
        match runner.run(&command.with_timeout(Duration::from_secs(20)), None) {
            Ok(output) if output.success() => {
                let first = output.stdout.lines().next().unwrap_or_default().trim();
                let package = if manager == "dpkg" {
                    first.split(':').next().unwrap_or_default().trim()
                } else {
                    first
                };
                if !package.is_empty() {
                    return Some((manager.to_owned(), package.to_owned()));
                }
            }
            Ok(_) | Err(HostError::ToolNotFound(_)) => {}
            Err(error) => problems.push(format!("{manager} ownership query failed: {error}")),
        }
    }
    None
}

pub(crate) fn detect(
    paths: &InstallPaths,
    runner: &dyn HostRunner,
    timeout: Duration,
) -> Detection {
    let mut problems = Vec::new();
    let props = match (Systemd { runner, timeout }).show() {
        Ok(props) => Some(props),
        Err(error) => {
            problems.push(format!("cannot query systemd: {error}"));
            None
        }
    };

    let unit_path = paths.unit_path();
    let unit_meta = fs::symlink_metadata(&unit_path).ok();
    let unit_exists = unit_meta.is_some();
    let mask_link = is_mask_link(&unit_path) || is_mask_link(&paths.runtime_unit_path());
    let masked = mask_link || props.as_ref().is_some_and(|p| p.load_state == "masked");

    let record = match read_record(paths) {
        Ok(record) => record,
        Err(error) => {
            problems.push(error.to_string());
            None
        }
    };
    let unit_sha = unit_meta
        .as_ref()
        .filter(|m| m.is_file())
        .and_then(|_| fs::read(&unit_path).ok())
        .map(|bytes| sha256_hex(&bytes));
    let unit_matches = matches!((&record, &unit_sha), (Some(r), Some(s)) if &r.unit_sha256 == s);
    let ours = record.is_some() && (unit_matches || !unit_exists);
    if record.is_some() && !unit_exists {
        problems.push("installer record exists but the unit file is missing".to_owned());
    }
    if record.is_some() && unit_exists && !unit_matches {
        problems.push("the unit file was modified after the installer wrote it".to_owned());
    }

    // Units that belong to someone else.
    let our_logical = paths.logical(&unit_path);
    let mut foreign_paths: Vec<String> = paths
        .foreign_unit_candidates()
        .into_iter()
        .filter(|logical| {
            let on_disk = paths.join(logical);
            fs::metadata(&on_disk).is_ok_and(|m| m.is_file()) && !is_mask_link(&on_disk)
        })
        .collect();
    if let Some(fragment) = props.as_ref().map(|p| p.fragment_path.clone())
        && !fragment.is_empty()
        && fragment != our_logical
        && !foreign_paths.contains(&fragment)
    {
        foreign_paths.push(fragment);
    }
    let mut foreign = None;
    for logical in &foreign_paths {
        foreign = Some(query_native(runner, logical, &mut problems).map_or_else(
            || Ownership::Unknown {
                unit_path: logical.clone(),
                reason: "not owned by any package and not created by this installer".to_owned(),
            },
            |(manager, package)| Ownership::NativePackage {
                manager,
                package,
                unit_path: logical.clone(),
            },
        ));
        if matches!(foreign, Some(Ownership::NativePackage { .. })) {
            break;
        }
    }

    // A surviving installer record does not authorize us to stop, replace or disable a
    // subsequently installed package/manual service with the same unit name.
    let ours = ours && foreign.is_none();
    let ownership = if let Some(foreign) = foreign {
        foreign
    } else if ours {
        Ownership::AppimageInstaller {
            build_id: record
                .as_ref()
                .map(|r| r.build_id.clone())
                .unwrap_or_default(),
        }
    } else if unit_exists {
        let reason = if mask_link {
            "the unit is a mask link"
        } else if record.is_some() {
            "modified after installation (use drop-ins for local changes)"
        } else {
            "no installer record"
        };
        Ownership::Unknown {
            unit_path: our_logical,
            reason: reason.to_owned(),
        }
    } else {
        Ownership::None
    };

    let mut dropins: Vec<String> = fs::read_dir(paths.dropin_dir())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    dropins.sort();

    Detection {
        ownership,
        record,
        ours,
        masked,
        props,
        unit_exists,
        dropins,
        problems,
    }
}

/// Passive status: reads files and runs read-only queries, never writes or prompts.
#[must_use]
pub fn inspect(paths: &InstallPaths, runner: &dyn HostRunner) -> ServiceStatusReport {
    let detection = detect(paths, runner, Duration::from_secs(15));
    let unit_state = if detection.masked {
        UnitState::Masked
    } else {
        detection
            .props
            .as_ref()
            .map_or(UnitState::Unknown, UnitProps::state)
    };
    let enabled = detection.props.as_ref().and_then(UnitProps::enabled);
    let interrupted = paths.journal_path().exists();
    let mut problems = detection.problems;
    if interrupted {
        problems.push(
            "an earlier install/update was interrupted; the next service command recovers it"
                .to_owned(),
        );
    }
    ServiceStatusReport {
        managed_by: detection.ownership.managed_by().to_owned(),
        installed_build_id: detection.record.as_ref().map(|r| r.build_id.clone()),
        version: detection.record.as_ref().map(|r| r.version.clone()),
        unit_path: detection
            .unit_exists
            .then(|| paths.logical(&paths.unit_path())),
        ownership: detection.ownership,
        unit_state,
        enabled,
        masked: detection.masked,
        dropins: detection.dropins,
        interrupted_operation: interrupted,
        problems,
    }
}
