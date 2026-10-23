//! Root-side lifecycle: install, update, uninstall, rollback, journal recovery.

use std::fmt::Display;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use awcc_diagnostics::hash::sha256_hex;
use serde::{Deserialize, Serialize};

use super::elevate::ApplyEvent;
use super::payload::{self, PayloadSource, VerifiedPayload};
use super::unit::{SERVICE_ROOT, render_unit};
use super::{
    ApplyOutcome, ApplyReport, Detection, INSTALLER_ID, InstallError, InstallPaths, OwnerRecord,
    Ownership, Progress, ProgressStep, RecordedIntent, RollbackSummary, SCHEMA_VERSION,
    ServiceAction, Systemd, UNIT_NAME, UnitProps, check_existing_dir_chain, detect,
    ensure_dir_chain, io_context, write_atomic,
};
use super::{HostRunner, PayloadError};

const NOFOLLOW: i32 = rustix::fs::OFlags::NOFOLLOW.bits() as i32;
const STAGING_PREFIX: &str = ".staging-";

/// Decides whether a freshly started service is healthy (process alive is checked separately by
/// the installer; this checks the IPC protocol/version). Must not require real hardware.
pub trait HealthProbe {
    /// `Ok` when healthy; `Err(reason)` when not (yet) healthy. Called repeatedly until the
    /// health timeout expires.
    ///
    /// # Errors
    /// The reason the service is not healthy yet.
    fn check(&self) -> Result<(), String>;
}

impl<F: Fn() -> Result<(), String>> HealthProbe for F {
    fn check(&self) -> Result<(), String> {
        self()
    }
}

/// Tunables of [`apply`].
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// Total time allowed for the service to become healthy after a start.
    pub health_timeout: Duration,
    /// Pause between health attempts.
    pub health_interval: Duration,
    /// Time limit for `systemctl` calls (covers a graceful stop; the unit uses
    /// `TimeoutStopSec=15`).
    pub command_timeout: Duration,
    /// Fresh install creates a missing allowlist with this content. Explicit setup instead
    /// requires exactly one numeric UID (comments allowed) and adds it to the existing policy.
    pub authorized_users_seed: Option<String>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            health_timeout: Duration::from_secs(20),
            health_interval: Duration::from_millis(500),
            command_timeout: Duration::from_secs(60),
            authorized_users_seed: None,
        }
    }
}

/// Everything [`apply`] needs besides the action.
pub struct ApplyContext<'a> {
    pub paths: &'a InstallPaths,
    pub runner: &'a dyn HostRunner,
    pub probe: &'a dyn HealthProbe,
    /// Required for install/update; ignored for setup/uninstall.
    pub payload: Option<&'a PayloadSource>,
    pub options: ApplyOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    /// Only files below `releases/` exist; no system state was changed.
    Staged,
    Stopped,
    Switched,
    Started,
    /// Health check passed; only the ownership record write remains.
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreviousState {
    current_target: Option<String>,
    unit_text: Option<String>,
    record_json: Option<String>,
    unit_existed: bool,
    enabled: Option<bool>,
    active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    operation: String,
    phase: Phase,
    new_build_id: String,
    new_release_created: bool,
    created_authorized_users: bool,
    previous: PreviousState,
    new_record_json: Option<String>,
}

struct Run<'a, 'p> {
    ctx: &'a ApplyContext<'a>,
    systemd: Systemd<'a>,
    progress: &'p mut dyn FnMut(Progress),
}

impl Run<'_, '_> {
    fn emit(&mut self, step: ProgressStep, message: impl Into<String>) {
        (self.progress)(Progress {
            step,
            message: message.into(),
        });
    }

    fn paths(&self) -> &InstallPaths {
        self.ctx.paths
    }
}

struct StepFailure {
    step: &'static str,
    reason: String,
    health: bool,
}

fn fail(step: &'static str, error: impl Display) -> StepFailure {
    StepFailure {
        step,
        reason: error.to_string(),
        health: false,
    }
}

struct Lock {
    _file: File,
}

fn acquire_lock(paths: &InstallPaths) -> Result<Lock, InstallError> {
    let path = paths.lock_path();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(&path)
        .map_err(io_context(format!("opening lock {}", path.display())))?;
    let meta = file
        .metadata()
        .map_err(io_context(format!("inspecting lock {}", path.display())))?;
    if !meta.is_file() {
        return Err(InstallError::UnsafePath {
            path: path.display().to_string(),
            reason: "lock is not a regular file".to_owned(),
        });
    }
    if paths.is_privileged() && std::os::unix::fs::MetadataExt::uid(&meta) != 0 {
        return Err(InstallError::UnsafePath {
            path: path.display().to_string(),
            reason: "lock is not owned by root".to_owned(),
        });
    }
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Lock { _file: file }),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(InstallError::LockHeld {
            path: path.display().to_string(),
        }),
        Err(errno) => Err(InstallError::Io {
            context: format!("locking {}", path.display()),
            source: errno.into(),
        }),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn save_journal(paths: &InstallPaths, journal: &Journal) -> Result<(), InstallError> {
    let bytes = serde_json::to_vec_pretty(journal).map_err(|e| InstallError::BadRecord {
        path: paths.journal_path().display().to_string(),
        reason: e.to_string(),
    })?;
    write_atomic(&paths.journal_path(), &bytes, 0o600)
}

fn load_journal(paths: &InstallPaths) -> Result<Option<Journal>, InstallError> {
    let path = paths.journal_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(InstallError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| InstallError::BadRecord {
        path: path.display().to_string(),
        reason: format!("{e}; inspect and delete the file to proceed"),
    })?;
    if journal.schema_version != SCHEMA_VERSION || !payload::valid_token(&journal.new_build_id) {
        return Err(InstallError::BadRecord {
            path: path.display().to_string(),
            reason: "unsupported schema or invalid build id".to_owned(),
        });
    }
    Ok(Some(journal))
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn switch_current(paths: &InstallPaths, target: &str) -> Result<(), InstallError> {
    let link = paths.current_link();
    let temp = link.with_file_name(format!(".current.tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    symlink(target, &temp).map_err(io_context(format!("creating {}", temp.display())))?;
    fs::rename(&temp, &link).map_err(|source| {
        let _ = fs::remove_file(&temp);
        InstallError::Io {
            context: format!("switching {}", link.display()),
            source,
        }
    })
}

fn read_current_target(paths: &InstallPaths) -> Result<Option<String>, InstallError> {
    let link = paths.current_link();
    match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => fs::read_link(&link)
            .map(|t| Some(t.to_string_lossy().into_owned()))
            .map_err(io_context(format!("reading {}", link.display()))),
        Ok(_) => Err(InstallError::UnsafePath {
            path: link.display().to_string(),
            reason: "`current` exists but is not a symlink".to_owned(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(InstallError::Io {
            context: format!("inspecting {}", link.display()),
            source,
        }),
    }
}

fn remove_staging_dirs(paths: &InstallPaths) {
    if let Ok(entries) = fs::read_dir(paths.releases_dir()) {
        for entry in entries.filter_map(Result::ok) {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGING_PREFIX)
            {
                let _ = remove_if_exists(&entry.path());
            }
        }
    }
}

fn has_lib(verified: &VerifiedPayload) -> bool {
    verified.files.iter().any(|f| f.path.starts_with("lib/"))
}

/// Run the requested lifecycle operation as root.
///
/// `progress` is called when each step starts, so a UI can show truthful progress.
///
/// # Errors
/// [`InstallError`]. Install/update failures after deployment starts attempt rollback and report
/// its result. Setup/uninstall failures report the problem and may leave earlier steps applied.
pub fn apply(
    action: ServiceAction,
    ctx: &ApplyContext<'_>,
    progress: &mut dyn FnMut(Progress),
) -> Result<ApplyReport, InstallError> {
    let mut run = Run {
        ctx,
        systemd: Systemd {
            runner: ctx.runner,
            timeout: ctx.options.command_timeout,
        },
        progress,
    };
    let paths = ctx.paths;
    run.emit(
        ProgressStep::Inspecting,
        "inspecting the current installation",
    );

    if paths.is_privileged() {
        let euid = rustix::process::geteuid();
        if !euid.is_root() {
            return Err(InstallError::NotRoot(euid.as_raw()));
        }
    }
    if !paths.systemd_runtime_dir().is_dir() {
        return Err(InstallError::UnsupportedInit);
    }

    let state_exists = fs::symlink_metadata(paths.state_dir()).is_ok();
    if matches!(action, ServiceAction::Uninstall { .. }) && !state_exists {
        let det = detect(paths, ctx.runner, ctx.options.command_timeout);
        return uninstall_gate_nothing(action, &det);
    }
    ensure_dir_chain(paths, "/var/lib/awcc", 0o755)?;
    let _lock = acquire_lock(paths)?;

    // Setup must never run deployment recovery or staging cleanup: a package-owned service
    // may now occupy the same unit name as an interrupted AppImage installation.
    if matches!(action, ServiceAction::Setup) {
        return setup(&mut run);
    }

    // Recovery and stale staging cleanup run before deploy creates its directories. Validate
    // those paths first so cleanup cannot follow an administrator's symlink elsewhere.
    for logical in [
        format!("{SERVICE_ROOT}/releases"),
        "/etc/systemd/system".to_owned(),
        "/etc/awcc".to_owned(),
    ] {
        check_existing_dir_chain(paths, &logical)?;
    }

    // Recovery can replace files and restart the service too. Do not begin it while the
    // manager is unavailable or the current running state cannot be established.
    run.systemd.show()?;
    let recovered = recover(&mut run)?;
    let det = detect(paths, ctx.runner, ctx.options.command_timeout);
    match action {
        ServiceAction::Install | ServiceAction::Update => deploy(&mut run, action, &det, recovered),
        ServiceAction::Uninstall { purge } => uninstall(&mut run, &det, purge, recovered),
        ServiceAction::Setup => unreachable!("setup returns before deployment recovery"),
    }
}

// ---------------------------------------------------------------------------------------------
// Explicit setup of an existing native/package-managed service.
// ---------------------------------------------------------------------------------------------

fn setup_failed(reason: impl Into<String>) -> InstallError {
    InstallError::SetupFailed {
        reason: reason.into(),
    }
}

fn setup_invoker(seed: Option<&str>) -> Result<String, InstallError> {
    let mut entries = seed.unwrap_or_default().lines().filter_map(|line| {
        let entry = line.split('#').next().unwrap_or_default().trim();
        (!entry.is_empty()).then_some(entry)
    });
    let entry = entries.next().ok_or_else(|| {
        setup_failed(
            "no invoking desktop user was identified; run `awcc-cli service setup` from your \
             desktop account (with polkit or sudo)",
        )
    })?;
    let uid = entry
        .parse::<u32>()
        .ok()
        .filter(|uid| *uid != 0 && *uid != u32::MAX)
        .filter(|_| entry.bytes().all(|b| b.is_ascii_digit()));
    if entries.next().is_some() || uid.is_none() {
        return Err(setup_failed(
            "setup requires exactly one valid invoking desktop UID",
        ));
    }
    Ok(entry.to_owned())
}

fn validate_setup_unit(paths: &InstallPaths, det: &Detection) -> Result<(), InstallError> {
    if det.masked {
        return Err(InstallError::Masked {
            path: paths.logical(&paths.unit_path()),
        });
    }
    let logical = match &det.ownership {
        Ownership::AppimageInstaller { .. } if det.ours && det.unit_exists => {
            paths.logical(&paths.unit_path())
        }
        Ownership::NativePackage {
            package, unit_path, ..
        } if package == "awcc-rs" => unit_path.clone(),
        Ownership::None | Ownership::AppimageInstaller { .. } => {
            return Err(InstallError::NotInstalled);
        }
        Ownership::Unknown { unit_path, reason } => {
            return Err(InstallError::UnknownUnit {
                unit_path: unit_path.clone(),
                reason: reason.clone(),
            });
        }
        Ownership::NativePackage { package, .. } => {
            return Err(setup_failed(format!(
                "the service belongs to package `{package}`, not the AWCC package"
            )));
        }
    };
    let props = det.props.as_ref().ok_or_else(|| {
        setup_failed(format!(
            "cannot establish the service state: {}",
            det.problems.join("; ")
        ))
    })?;
    if props.load_state != "loaded" || props.fragment_path.is_empty() {
        return Err(setup_failed(
            "systemd has not loaded the owned service unit",
        ));
    }
    let expected = fs::canonicalize(paths.join(&logical))
        .map_err(io_context("resolving the owned service unit"))?;
    let root =
        fs::canonicalize(paths.join("/")).map_err(io_context("resolving the installation root"))?;
    if !expected.starts_with(&root) {
        return Err(setup_failed(
            "the owned unit resolves outside the installation root",
        ));
    }
    let parent = expected
        .parent()
        .ok_or_else(|| setup_failed("unit has no parent"))?;
    check_existing_dir_chain(paths, &paths.logical(parent))?;
    let metadata =
        fs::symlink_metadata(&expected).map_err(io_context("checking the service unit"))?;
    if !metadata.is_file()
        || paths.is_privileged() && (metadata.uid() != 0 || metadata.mode() & 0o022 != 0)
    {
        return Err(setup_failed(
            "the service unit has unsafe ownership or permissions",
        ));
    }
    let selected = fs::canonicalize(paths.join(&props.fragment_path))
        .map_err(io_context("resolving the loaded service unit"))?;
    if selected != expected {
        return Err(setup_failed(
            "systemd loaded a different service unit than its recorded owner",
        ));
    }
    // Reload must not activate an unrecognized local/runtime override that was written since
    // systemd last loaded the package unit. A link to the same unit remains harmless.
    for override_path in [paths.unit_path(), paths.runtime_unit_path()] {
        if fs::symlink_metadata(&override_path).is_ok()
            && fs::canonicalize(&override_path).ok().as_ref() != Some(&expected)
        {
            return Err(setup_failed(format!(
                "{} overrides the owned service unit; review that file before setup",
                override_path.display()
            )));
        }
    }
    Ok(())
}

fn setup_start_error(paths: &InstallPaths, error: impl Display) -> InstallError {
    let mut reason = format!(
        "{error}. Authorization or enablement may already have been applied; fix the reported \
         problem and retry setup"
    );
    if paths.is_privileged() && !paths.join("/proc/acpi/call").exists() {
        reason.push_str(
            ". The acpi_call driver is unavailable (/proc/acpi/call is missing); install the \
             acpi_call package for your running kernel and load its module before retrying",
        );
    }
    setup_failed(reason)
}

fn setup(run: &mut Run<'_, '_>) -> Result<ApplyReport, InstallError> {
    let paths = run.ctx.paths;
    let invoker = setup_invoker(run.ctx.options.authorized_users_seed.as_deref())?;
    check_existing_dir_chain(paths, "/etc/awcc")?;
    if fs::symlink_metadata(paths.journal_path()).is_ok() {
        return Err(setup_failed(
            "an interrupted AppImage deployment must be resolved before setup; no recovery or \
             service-file replacement was attempted",
        ));
    }
    let det = detect(paths, run.ctx.runner, run.ctx.options.command_timeout);
    validate_setup_unit(paths, &det)?;
    let existing_users = fs::symlink_metadata(paths.authorized_users_path()).is_ok();
    // Record ownership before creating an allowlist so AppImage purge also cleans up a policy
    // first created by this explicit setup action.
    if !existing_users
        && let Some(mut record) = det.record.clone().filter(|_| det.ours)
        && !record.created_authorized_users
    {
        record.created_authorized_users = true;
        let json = serde_json::to_vec_pretty(&record)
            .map_err(|error| setup_failed(format!("cannot update ownership record: {error}")))?;
        write_atomic(&paths.record_path(), &json, 0o644)?;
    }
    ensure_dir_chain(paths, "/etc/awcc", 0o755)?;
    crate::ipc::authorized_users::add(&paths.authorized_users_path(), &invoker)
        .map_err(io_context("authorizing the invoking user"))?;

    run.emit(
        ProgressStep::ReloadingSystemd,
        "loading the installed service unit",
    );
    run.systemd
        .act("daemon-reload", false)
        .map_err(|error| setup_start_error(paths, error))?;
    // Inspect again after reload before any start: the loaded unit must still match ownership.
    let reloaded = detect(paths, run.ctx.runner, run.ctx.options.command_timeout);
    validate_setup_unit(paths, &reloaded)?;
    if paths.is_privileged() {
        crate::provider_install::install(
            paths,
            run.ctx.runner,
            if det.ours {
                "/opt/awcc-rs/service/current/bin/awcc-cli"
            } else {
                "/usr/bin/awcc-cli"
            },
        )
        .map_err(setup_failed)?;
    }
    run.emit(
        ProgressStep::StartingService,
        "enabling and starting the installed service",
    );
    run.systemd
        .act("enable", true)
        .map_err(|error| setup_start_error(paths, error))?;
    run.systemd
        .act("start", true)
        .map_err(|error| setup_start_error(paths, error))?;
    run.emit(
        ProgressStep::HealthCheck,
        "checking that the service is ready",
    );
    health_check(run).map_err(|error| setup_start_error(paths, error))?;
    let after = run
        .systemd
        .show()
        .map_err(|error| setup_start_error(paths, error))?;
    if after.enabled() != Some(true) || !after.active() {
        return Err(setup_start_error(
            paths,
            "the service is not enabled and running",
        ));
    }
    run.emit(
        ProgressStep::Done,
        "the service is configured for this account",
    );
    Ok(ApplyReport {
        action: ServiceAction::Setup.name().to_owned(),
        outcome: ApplyOutcome::Configured,
        build_id: det
            .record
            .as_ref()
            .filter(|_| det.ours)
            .map(|r| r.build_id.clone()),
        previous_build_id: None,
        enabled: after.enabled(),
        active: Some(after.active()),
        recovered_interrupted: false,
        purged: false,
        notes: vec![format!(
            "Authorized desktop UID {invoker}; the service is enabled at boot."
        )],
    })
}

fn uninstall_gate_nothing(
    action: ServiceAction,
    det: &Detection,
) -> Result<ApplyReport, InstallError> {
    refuse_foreign(det)?;
    Ok(ApplyReport {
        action: action.name().to_owned(),
        outcome: ApplyOutcome::NothingToUninstall,
        build_id: None,
        previous_build_id: None,
        enabled: None,
        active: None,
        recovered_interrupted: false,
        purged: false,
        notes: vec!["no installation by this installer was found".to_owned()],
    })
}

fn refuse_foreign(det: &Detection) -> Result<(), InstallError> {
    match &det.ownership {
        Ownership::NativePackage {
            manager,
            package,
            unit_path,
        } if !det.ours => Err(InstallError::NativePackage {
            manager: manager.clone(),
            package: package.clone(),
            unit_path: unit_path.clone(),
        }),
        Ownership::Unknown { unit_path, reason } if !det.ours => Err(InstallError::UnknownUnit {
            unit_path: unit_path.clone(),
            reason: reason.clone(),
        }),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Journal recovery.
// ---------------------------------------------------------------------------------------------

fn recover(run: &mut Run<'_, '_>) -> Result<bool, InstallError> {
    let paths = run.paths();
    let Some(journal) = load_journal(paths)? else {
        remove_staging_dirs(paths);
        return Ok(false);
    };
    run.emit(
        ProgressStep::Recovering,
        format!(
            "recovering an interrupted {} of build {}",
            journal.operation, journal.new_build_id
        ),
    );
    if journal.phase == Phase::Committed {
        if let Some(record) = &journal.new_record_json {
            write_atomic(&run.paths().record_path(), record.as_bytes(), 0o644)?;
        }
        remove_if_exists(&run.paths().journal_path())
            .map_err(io_context("removing the finished journal"))?;
        return Ok(true);
    }
    let summary = rollback(run, &journal);
    if summary.restored {
        Ok(true)
    } else {
        Err(InstallError::StepFailed {
            step: "recovery of an interrupted operation".to_owned(),
            reason: "the earlier operation could not be rolled back".to_owned(),
            rollback: summary,
        })
    }
}

fn rollback(run: &mut Run<'_, '_>, journal: &Journal) -> RollbackSummary {
    run.emit(ProgressStep::RollingBack, "restoring the previous state");
    let paths = run.ctx.paths;
    let mut problems: Vec<String> = Vec::new();
    let mut note = |what: &str, result: Result<(), String>| {
        if let Err(reason) = result {
            problems.push(format!("{what}: {reason}"));
        }
    };

    if journal.phase != Phase::Staged {
        if let Ok(props) = run.systemd.show()
            && props.active()
        {
            note(
                "stopping the new release",
                run.systemd.act("stop", true).map_err(|e| e.to_string()),
            );
        }
        if paths.is_privileged() && !journal.previous.unit_existed {
            note(
                "restoring previous PowerProfiles provider",
                crate::provider_install::remove(paths, run.ctx.runner),
            );
        }
        let prev = &journal.previous;
        if !prev.unit_existed {
            // Best effort: a failed disable of a unit we are about to remove is harmless.
            let _ = run.systemd.act("disable", true);
        }
        match &prev.current_target {
            Some(target) => note(
                "restoring the `current` symlink",
                switch_current(paths, target).map_err(|e| e.to_string()),
            ),
            None => note(
                "removing the new `current` symlink",
                remove_if_exists(&paths.current_link()).map_err(|e| e.to_string()),
            ),
        }
        match &prev.unit_text {
            Some(text) => note(
                "restoring the previous unit file",
                write_atomic(&paths.unit_path(), text.as_bytes(), 0o644).map_err(|e| e.to_string()),
            ),
            None => note(
                "removing the new unit file",
                remove_if_exists(&paths.unit_path()).map_err(|e| e.to_string()),
            ),
        }
        note(
            "reloading systemd",
            run.systemd
                .act("daemon-reload", false)
                .map_err(|e| e.to_string()),
        );
        if prev.unit_existed {
            match run.systemd.show() {
                Ok(props) => match (prev.enabled, props.enabled()) {
                    (Some(true), current) if current != Some(true) => note(
                        "restoring the enabled state",
                        run.systemd.act("enable", true).map_err(|e| e.to_string()),
                    ),
                    (Some(false), Some(true)) => note(
                        "restoring the disabled state",
                        run.systemd.act("disable", true).map_err(|e| e.to_string()),
                    ),
                    _ => {}
                },
                Err(e) => note("reading the unit state", Err(e.to_string())),
            }
        }
        if journal.created_authorized_users {
            note(
                "removing the authorized-users file this run created",
                remove_if_exists(&paths.authorized_users_path()).map_err(|e| e.to_string()),
            );
        }
        match &prev.record_json {
            Some(json) => note(
                "restoring the ownership record",
                write_atomic(&paths.record_path(), json.as_bytes(), 0o644)
                    .map_err(|e| e.to_string()),
            ),
            None => note(
                "removing the ownership record",
                remove_if_exists(&paths.record_path()).map_err(|e| e.to_string()),
            ),
        }
        if prev.active {
            note(
                "restarting the previous release",
                run.systemd.act("start", true).map_err(|e| e.to_string()),
            );
        }
    }
    if journal.new_release_created {
        let target_is_new = read_current_target(paths)
            .ok()
            .flatten()
            .is_some_and(|t| t == format!("releases/{}", journal.new_build_id));
        if !target_is_new {
            note(
                "removing the new release directory",
                remove_if_exists(&paths.release_dir(&journal.new_build_id))
                    .map_err(|e| e.to_string()),
            );
        }
    }
    remove_staging_dirs(paths);
    if problems.is_empty() {
        let _ = remove_if_exists(&paths.journal_path());
    }
    RollbackSummary {
        restored: problems.is_empty(),
        problems,
    }
}

// ---------------------------------------------------------------------------------------------
// Install / update.
// ---------------------------------------------------------------------------------------------

fn stage_release(
    paths: &InstallPaths,
    verified: &VerifiedPayload,
) -> Result<PathBuf, InstallError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let staging = paths.releases_dir().join(format!(
        "{STAGING_PREFIX}{}-{}-{nanos}",
        verified.build_id,
        std::process::id()
    ));
    DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(io_context(format!("creating {}", staging.display())))?;
    let result = (|| -> Result<(), InstallError> {
        let manifest = payload::parse_manifest(
            std::str::from_utf8(&verified.manifest_bytes)
                .map_err(|_| PayloadError::BadManifest("not valid UTF-8".to_owned()))?,
        )?;
        let mut dirs: Vec<PathBuf> = Vec::new();
        for file in &verified.files {
            // Hash the exact bytes that are written; the copy is re-hashed below.
            let bytes = payload::read_verified(
                &verified.dir,
                &file.path,
                &file.sha256,
                paths.is_privileged(),
            )?;
            let dest = staging.join(&file.path);
            if let Some(parent) = dest.parent() {
                DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(io_context(format!("creating {}", parent.display())))?;
                dirs.push(parent.to_path_buf());
            }
            write_new(&dest, &bytes)?;
        }
        write_new(
            &staging.join(payload::MANIFEST_NAME),
            &verified.manifest_bytes,
        )?;
        for file in &verified.files {
            let mode = if file.executable { 0o755 } else { 0o644 };
            fs::set_permissions(staging.join(&file.path), fs::Permissions::from_mode(mode))
                .map_err(io_context(format!("chmod {}", file.path)))?;
        }
        fs::set_permissions(
            staging.join(payload::MANIFEST_NAME),
            fs::Permissions::from_mode(0o644),
        )
        .map_err(io_context("chmod MANIFEST.sha256"))?;
        dirs.sort();
        dirs.dedup();
        for dir in dirs.iter().rev().chain(std::iter::once(&staging)) {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
                .map_err(io_context(format!("chmod {}", dir.display())))?;
        }
        payload::verify_tree(
            &staging,
            &verified.manifest_bytes,
            &manifest,
            paths.is_privileged(),
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(staging),
        Err(error) => {
            let _ = remove_if_exists(&staging);
            Err(error)
        }
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(path)
        .map_err(io_context(format!("creating {}", path.display())))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(io_context(format!("writing {}", path.display())))
}

fn deploy(
    run: &mut Run<'_, '_>,
    action: ServiceAction,
    det: &Detection,
    recovered: bool,
) -> Result<ApplyReport, InstallError> {
    let ctx = run.ctx;
    let paths = ctx.paths;
    if det.masked {
        return Err(InstallError::Masked {
            path: paths.logical(&paths.unit_path()),
        });
    }
    refuse_foreign(det)?;
    if !det.ours && !matches!(det.ownership, Ownership::None) {
        return Err(InstallError::UnknownUnit {
            unit_path: paths.logical(&paths.unit_path()),
            reason: "ownership could not be established".to_owned(),
        });
    }
    let fresh = !det.ours;
    if fresh && matches!(action, ServiceAction::Update) {
        return Err(InstallError::NotInstalled);
    }
    // Query failure must not be mistaken for an intentionally stopped service.
    let props = run.systemd.show()?;
    let source = ctx
        .payload
        .ok_or_else(|| PayloadError::Missing("no payload source configured".to_owned()))?;

    run.emit(
        ProgressStep::VerifyingPayload,
        "verifying the service payload",
    );
    let verified = payload::verify_payload(source, paths.is_privileged())?;
    let build_id = verified.build_id.clone();
    let library = has_lib(&verified);
    let unit_text = render_unit(SERVICE_ROOT, library);
    let unit_sha = sha256_hex(unit_text.as_bytes());

    ensure_dir_chain(paths, &format!("{SERVICE_ROOT}/releases"), 0o755)?;
    ensure_dir_chain(paths, "/etc/systemd/system", 0o755)?;
    remove_staging_dirs(paths);

    let release_dir = paths.release_dir(&build_id);
    let new_target = format!("releases/{build_id}");
    let prev_target = read_current_target(paths)?;
    let prev_build_id = det.record.as_ref().map(|r| r.build_id.clone());

    let release_intact = fs::symlink_metadata(&release_dir).is_ok()
        && payload::parse_manifest(&String::from_utf8_lossy(&verified.manifest_bytes))
            .and_then(|m| {
                payload::verify_tree(
                    &release_dir,
                    &verified.manifest_bytes,
                    &m,
                    paths.is_privileged(),
                )
            })
            .is_ok();

    // Idempotent reinstall of the very same, intact build.
    if let (Some(record), false) = (&det.record, fresh)
        && record.build_id == build_id
        && record.unit_sha256 == unit_sha
        && prev_target.as_deref() == Some(new_target.as_str())
        && release_intact
    {
        return Ok(ApplyReport {
            action: action.name().to_owned(),
            outcome: ApplyOutcome::Unchanged,
            build_id: Some(build_id),
            previous_build_id: prev_build_id,
            enabled: props.enabled(),
            active: Some(props.active()),
            recovered_interrupted: recovered,
            purged: false,
            notes: vec!["this build is already installed and intact".to_owned()],
        });
    }

    // Capture everything a rollback needs.
    let unit_path = paths.unit_path();
    let previous = PreviousState {
        current_target: prev_target.clone(),
        unit_text: if det.unit_exists {
            Some(
                fs::read_to_string(&unit_path)
                    .map_err(io_context(format!("reading {}", unit_path.display())))?,
            )
        } else {
            None
        },
        record_json: match fs::read_to_string(paths.record_path()) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(InstallError::Io {
                    context: "reading the ownership record".to_owned(),
                    source,
                });
            }
        },
        unit_existed: det.unit_exists,
        enabled: props.enabled(),
        active: props.active(),
    };

    // Stage and verify the new release in a private directory.
    let reuse = release_intact;
    if !reuse && fs::symlink_metadata(&release_dir).is_ok() {
        if prev_target.as_deref() == Some(new_target.as_str()) {
            return Err(InstallError::ReleaseCollision {
                path: paths.logical(&release_dir),
            });
        }
        remove_if_exists(&release_dir).map_err(io_context("removing a damaged stale release"))?;
    }
    let staging = if reuse {
        None
    } else {
        run.emit(
            ProgressStep::Staging,
            format!("staging release {build_id} in a private directory"),
        );
        let staging = stage_release(paths, &verified)?;
        run.emit(ProgressStep::VerifyingStaged, "re-verified the staged copy");
        Some(staging)
    };

    let mut journal = Journal {
        schema_version: SCHEMA_VERSION,
        operation: if fresh { "install" } else { "update" }.to_owned(),
        phase: Phase::Staged,
        new_build_id: build_id.clone(),
        new_release_created: staging.is_some(),
        created_authorized_users: false,
        previous: previous.clone(),
        new_record_json: None,
    };
    save_journal(paths, &journal)?;

    let result = deploy_steps(
        run,
        &mut journal,
        &verified,
        &unit_text,
        &unit_sha,
        staging.as_deref(),
        fresh,
        det.record.as_ref(),
    );
    if let Err(failure) = result {
        let summary = rollback(run, &journal);
        return Err(if failure.health {
            InstallError::HealthCheckFailed {
                build_id,
                reason: failure.reason,
                rollback: summary,
            }
        } else {
            InstallError::StepFailed {
                step: failure.step.to_owned(),
                reason: failure.reason,
                rollback: summary,
            }
        });
    }

    run.emit(ProgressStep::Cleanup, "removing superseded releases");
    let mut notes = Vec::new();
    if let Ok(entries) = fs::read_dir(paths.releases_dir()) {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name().to_string_lossy() != build_id
                && let Err(e) = remove_if_exists(&entry.path())
            {
                notes.push(format!("could not remove {}: {e}", entry.path().display()));
            }
        }
    }
    let after = run.systemd.show().unwrap_or_default();
    run.emit(ProgressStep::Done, format!("build {build_id} is deployed"));
    Ok(ApplyReport {
        action: action.name().to_owned(),
        outcome: if fresh {
            ApplyOutcome::Installed
        } else {
            ApplyOutcome::Updated
        },
        build_id: Some(build_id),
        previous_build_id: prev_build_id,
        enabled: after.enabled(),
        active: Some(after.active()),
        recovered_interrupted: recovered,
        purged: false,
        notes,
    })
}

#[allow(clippy::too_many_arguments)]
fn deploy_steps(
    run: &mut Run<'_, '_>,
    journal: &mut Journal,
    verified: &VerifiedPayload,
    unit_text: &str,
    unit_sha: &str,
    staging: Option<&Path>,
    fresh: bool,
    previous_record: Option<&OwnerRecord>,
) -> Result<(), StepFailure> {
    let ctx = run.ctx;
    let paths = ctx.paths;
    let release_dir = paths.release_dir(&verified.build_id);

    if let Some(staging) = staging {
        fs::rename(staging, &release_dir).map_err(|e| fail("activating the staged release", e))?;
    }

    if journal.previous.active {
        journal.phase = Phase::Stopped;
        save_journal(paths, journal).map_err(|e| fail("journal", e))?;
        run.emit(
            ProgressStep::StoppingService,
            "stopping the service gracefully",
        );
        run.systemd
            .act("stop", true)
            .map_err(|e| fail("stopping the running service", e))?;
    }

    journal.phase = Phase::Switched;
    if fresh && ctx.options.authorized_users_seed.is_some() {
        journal.created_authorized_users = match fs::symlink_metadata(paths.authorized_users_path())
        {
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(fail("inspecting authorized-users", error)),
        };
    }
    save_journal(paths, journal).map_err(|e| fail("journal", e))?;
    run.emit(
        ProgressStep::Switching,
        "switching `current` and writing the unit",
    );
    if journal.created_authorized_users
        && let Some(seed) = &ctx.options.authorized_users_seed
    {
        ensure_dir_chain(paths, "/etc/awcc", 0o755).map_err(|e| fail("authorized-users", e))?;
        let mut text = seed.clone();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        write_atomic(&paths.authorized_users_path(), text.as_bytes(), 0o644)
            .map_err(|e| fail("authorized-users", e))?;
    }
    switch_current(paths, &format!("releases/{}", verified.build_id))
        .map_err(|e| fail("switching the `current` symlink", e))?;
    write_atomic(&paths.unit_path(), unit_text.as_bytes(), 0o644)
        .map_err(|e| fail("writing the unit file", e))?;
    run.emit(ProgressStep::ReloadingSystemd, "reloading systemd");
    run.systemd
        .act("daemon-reload", false)
        .map_err(|e| fail("reloading systemd", e))?;

    if paths.is_privileged() {
        crate::provider_install::install(
            paths,
            run.ctx.runner,
            "/opt/awcc-rs/service/current/bin/awcc-cli",
        )
        .map_err(|e| fail("provider handover", e))?;
    }
    journal.phase = Phase::Started;
    save_journal(paths, journal).map_err(|e| fail("journal", e))?;
    let start = fresh || journal.previous.active;
    if start {
        run.emit(ProgressStep::StartingService, "starting the service");
        if fresh {
            run.systemd
                .act("enable", true)
                .map_err(|e| fail("enabling the service", e))?;
        }
        run.systemd
            .act("start", true)
            .map_err(|e| fail("starting the service", e))?;
        run.emit(
            ProgressStep::HealthCheck,
            "checking that the service is healthy",
        );
        health_check(run).map_err(|reason| StepFailure {
            step: "health check",
            reason,
            health: true,
        })?;
    } else {
        run.emit(
            ProgressStep::HealthCheck,
            "checking that the unit loads (service stays stopped)",
        );
        let props = run
            .systemd
            .show()
            .map_err(|e| fail("verifying the unit", e))?;
        if props.load_state != "loaded" {
            return Err(StepFailure {
                step: "health check",
                reason: format!("systemd reports the unit as `{}`", props.load_state),
                health: true,
            });
        }
    }

    let mut installed_files: Vec<String> = verified
        .files
        .iter()
        .map(|f| format!("{SERVICE_ROOT}/releases/{}/{}", verified.build_id, f.path))
        .collect();
    installed_files.push(format!(
        "{SERVICE_ROOT}/releases/{}/{}",
        verified.build_id,
        payload::MANIFEST_NAME
    ));
    installed_files.sort();
    let record = OwnerRecord {
        schema_version: SCHEMA_VERSION,
        installer: INSTALLER_ID.to_owned(),
        build_id: verified.build_id.clone(),
        version: verified.version.clone(),
        installed_at_unix: now_unix(),
        unit_path: format!("/etc/systemd/system/{UNIT_NAME}"),
        unit_sha256: unit_sha.to_owned(),
        current_link: format!("{SERVICE_ROOT}/current"),
        release_dir: format!("{SERVICE_ROOT}/releases/{}", verified.build_id),
        installed_files,
        created_authorized_users: journal.created_authorized_users
            || previous_record.is_some_and(|r| r.created_authorized_users),
        previous_state: Some(RecordedIntent {
            enabled: journal.previous.enabled,
            active: journal.previous.active,
        }),
    };
    let json = serde_json::to_string_pretty(&record).map_err(|e| fail("ownership record", e))?;
    journal.phase = Phase::Committed;
    journal.new_record_json = Some(json.clone());
    save_journal(paths, journal).map_err(|e| fail("journal", e))?;
    write_atomic(&paths.record_path(), json.as_bytes(), 0o644)
        .map_err(|e| fail("writing the ownership record", e))?;
    remove_if_exists(&paths.journal_path()).map_err(|e| fail("removing the journal", e))?;
    Ok(())
}

fn health_check(run: &Run<'_, '_>) -> Result<(), String> {
    let options = &run.ctx.options;
    let deadline = Instant::now() + options.health_timeout;
    let mut last: String;
    loop {
        match run.systemd.show() {
            Ok(props) => match props.active_state.as_str() {
                "active" => match run.ctx.probe.check() {
                    Ok(()) => return Ok(()),
                    Err(reason) => {
                        last = format!("the service is running but not healthy: {reason}")
                    }
                },
                "failed" | "inactive" => {
                    return Err(format!(
                        "the service stopped right after starting (state `{}`/`{}`)",
                        props.active_state, props.sub_state
                    ));
                }
                other => last = format!("the service is still `{other}`"),
            },
            Err(e) => last = e.to_string(),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(last);
        }
        std::thread::sleep(options.health_interval.min(deadline - now));
    }
}

// ---------------------------------------------------------------------------------------------
// Uninstall.
// ---------------------------------------------------------------------------------------------

fn recorded_path_ok(logical: &str) -> bool {
    let prefix = format!("{SERVICE_ROOT}/releases/");
    logical.starts_with(&prefix)
        && payload::validate_relative_path(logical.trim_start_matches('/')).is_ok()
}

fn uninstall(
    run: &mut Run<'_, '_>,
    det: &Detection,
    purge: bool,
    recovered: bool,
) -> Result<ApplyReport, InstallError> {
    let ctx = run.ctx;
    let paths = ctx.paths;
    if !det.ours {
        refuse_foreign(det)?;
        return Ok(ApplyReport {
            action: "uninstall".to_owned(),
            outcome: ApplyOutcome::NothingToUninstall,
            build_id: None,
            previous_build_id: None,
            enabled: None,
            active: None,
            recovered_interrupted: recovered,
            purged: false,
            notes: vec!["no installation by this installer was found".to_owned()],
        });
    }
    let Some(record) = det.record.clone() else {
        return Err(InstallError::NotInstalled);
    };
    let mut notes = Vec::new();
    let props: UnitProps = run.systemd.show()?;

    if props.active() {
        run.emit(
            ProgressStep::StoppingService,
            "stopping the service gracefully",
        );
        run.systemd.act("stop", true)?;
    }
    run.emit(
        ProgressStep::Removing,
        "removing the unit and release files",
    );
    if props.enabled() == Some(true) {
        run.systemd.act("disable", true)?;
    }
    let unit_path = paths.unit_path();
    if det.unit_exists {
        let matches = fs::symlink_metadata(&unit_path).is_ok_and(|m| m.is_file())
            && fs::read(&unit_path).is_ok_and(|b| sha256_hex(&b) == record.unit_sha256);
        if matches {
            fs::remove_file(&unit_path).map_err(io_context("removing the unit file"))?;
        } else {
            notes.push(format!(
                "left {} in place because it is not the unit this installer wrote",
                unit_path.display()
            ));
        }
    }
    if paths.is_privileged() {
        crate::provider_install::remove(paths, run.ctx.runner).map_err(setup_failed)?;
    }
    run.systemd.act("daemon-reload", false)?;

    let link = paths.current_link();
    if fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(&link).map_err(io_context("removing the `current` symlink"))?;
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for logical in &record.installed_files {
        if !recorded_path_ok(logical) {
            notes.push(format!("ignored suspicious recorded path {logical}"));
            continue;
        }
        let path = paths.join(logical);
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_dir() => {
                fs::remove_file(&path).map_err(io_context(format!("removing {logical}")))?;
            }
            Ok(_) | Err(_) => {}
        }
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if dir == paths.releases_dir() || !dir.starts_with(paths.releases_dir()) {
                break;
            }
            dirs.push(dir.to_path_buf());
            parent = dir.parent();
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for dir in dirs {
        let _ = fs::remove_dir(&dir);
    }
    remove_staging_dirs(paths);
    for leftover in [paths.releases_dir(), paths.service_dir()] {
        if fs::remove_dir(&leftover).is_err() && leftover.exists() {
            notes.push(format!(
                "left {} (it still contains files this installer did not record)",
                leftover.display()
            ));
        }
    }
    if let Some(parent) = paths.service_dir().parent() {
        let _ = fs::remove_dir(parent);
    }
    if !det.dropins.is_empty() {
        notes.push(format!(
            "kept your drop-ins in {}",
            paths.dropin_dir().display()
        ));
    }

    if purge && record.created_authorized_users {
        let path = paths.authorized_users_path();
        remove_if_exists(&path).map_err(io_context("removing authorized-users"))?;
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    let _ = fs::remove_file(paths.journal_path());
    if purge {
        let state = paths.state_dir();
        if fs::symlink_metadata(&state).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(&state).map_err(io_context("purging /var/lib/awcc"))?;
        }
    } else {
        fs::remove_file(paths.record_path())
            .map_err(io_context("removing the ownership record"))?;
        notes
            .push("kept service settings in /var/lib/awcc (use --purge to delete them)".to_owned());
    }
    run.emit(ProgressStep::Done, "uninstalled");
    Ok(ApplyReport {
        action: "uninstall".to_owned(),
        outcome: ApplyOutcome::Uninstalled,
        build_id: None,
        previous_build_id: Some(record.build_id),
        enabled: None,
        active: Some(false),
        recovered_interrupted: recovered,
        purged: purge,
        notes,
    })
}

/// Entry point for the elevated child (`service-apply`): runs [`apply`], writes one JSON
/// [`ApplyEvent`] per line to `out`, and returns the process exit code (`0` success, `1`
/// failure; `126`/`127` are reserved by `pkexec`).
pub fn run_apply_command(
    action: ServiceAction,
    ctx: &ApplyContext<'_>,
    out: &mut dyn Write,
) -> i32 {
    fn emit(out: &mut dyn Write, event: &ApplyEvent) {
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
    let result = {
        let mut on_progress = |p: Progress| emit(out, &ApplyEvent::Progress(p));
        apply(action, ctx, &mut on_progress)
    };
    match result {
        Ok(report) => {
            emit(out, &ApplyEvent::Done(report));
            0
        }
        Err(error) => {
            emit(
                out,
                &ApplyEvent::Failed {
                    kind: error.kind().to_owned(),
                    message: error.to_string(),
                },
            );
            1
        }
    }
}
