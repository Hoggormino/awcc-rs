//! Shared CLI/GUI entry points for service lifecycle management.
//!
//! These paths do not construct a hardware backend. Only the narrowly scoped apply helper is
//! elevated; the interactive GUI and normal CLI remain unprivileged.

use std::io::Write;
use std::path::Path;

use thiserror::Error;

use crate::installer::{
    self, ApplyContext, ApplyEvent, ApplyOptions, ApplyReport, ElevationError, InstallError,
    InstallPaths, LaunchTarget, PayloadError, PayloadSource, Progress, ServiceAction, SystemRunner,
};
use crate::ipc::{DaemonClient, ServiceProbe};

/// Failure before or during an installer operation.
#[derive(Debug, Error)]
pub enum ManagementError {
    #[error("cannot resolve the service helper: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error(transparent)]
    Elevation(#[from] ElevationError),
    #[error("{0} must contain the numeric UID of the invoking user")]
    InvalidInvoker(&'static str),
    #[error(
        "this command requires root; use sudo (lifecycle commands can request polkit authorization)"
    )]
    RootRequired,
}

/// Add or revoke a user in the root-owned service allowlist. Existing entries are preserved.
///
/// # Errors
/// Requires root, safe destination directories, a valid entry and writable configuration.
pub fn set_authorized_user_system(user: &str, authorized: bool) -> Result<bool, ManagementError> {
    if !rustix::process::geteuid().is_root() {
        return Err(ManagementError::RootRequired);
    }
    let paths = InstallPaths::system();
    installer::ensure_dir_chain(&paths, "/etc/awcc", 0o755)?;
    let path = paths.authorized_users_path();
    Ok(if authorized {
        crate::ipc::authorized_users::add(&path, user)?
    } else {
        crate::ipc::authorized_users::remove(&path, user)?
    })
}

impl ManagementError {
    fn kind(&self) -> &str {
        match self {
            Self::Install(error) => error.kind(),
            Self::RootRequired => "root_required",
            Self::InvalidInvoker(_) => "invalid_invoker",
            Self::Io(_) => "io",
            Self::Payload(_) => "payload",
            Self::Elevation(_) => "elevation",
        }
    }
}

fn seed_for_invoker(
    pkexec_uid: Option<&str>,
    sudo_uid: Option<&str>,
) -> Result<Option<String>, ManagementError> {
    let Some((name, value)) = pkexec_uid
        .map(|uid| ("PKEXEC_UID", uid))
        .or_else(|| sudo_uid.map(|uid| ("SUDO_UID", uid)))
    else {
        return Ok(None);
    };
    let uid = value
        .parse::<u32>()
        .ok()
        .filter(|uid| *uid != u32::MAX)
        .filter(|_| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(ManagementError::InvalidInvoker(name))?;
    // Root is always authorized. Never guess a desktop user for a direct root invocation.
    Ok((uid != 0)
        .then(|| format!("# User who authorized the initial AWCC service installation.\n{uid}\n")))
}

fn health_check() -> Result<(), String> {
    match DaemonClient::system("awcc-installer", env!("CARGO_PKG_VERSION")).probe() {
        ServiceProbe::Running {
            version, status, ..
        } if version == env!("CARGO_PKG_VERSION") => {
            if status.control_running {
                Ok(())
            } else {
                Err("service control is not ready".to_owned())
            }
        }
        ServiceProbe::Running { version, .. } => Err(format!(
            "service version {version} differs from installer version {}",
            env!("CARGO_PKG_VERSION")
        )),
        ServiceProbe::NotRunning { reason } => Err(reason),
        ServiceProbe::AccessDenied { message } | ServiceProbe::Incompatible { message } => {
            Err(message)
        }
    }
}

fn apply_as_root(
    action: ServiceAction,
    cli_executable: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<ApplyReport, ManagementError> {
    if !rustix::process::geteuid().is_root() {
        return Err(ManagementError::RootRequired);
    }
    let paths = InstallPaths::system();
    let executable = std::fs::canonicalize(cli_executable)?;
    let payload = match action {
        ServiceAction::Install | ServiceAction::Update => {
            Some(PayloadSource::for_executable(&executable)?)
        }
        ServiceAction::Uninstall { .. } => None,
    };
    let seed = if matches!(action, ServiceAction::Uninstall { .. }) {
        None
    } else {
        seed_for_invoker(
            std::env::var("PKEXEC_UID").ok().as_deref(),
            std::env::var("SUDO_UID").ok().as_deref(),
        )?
    };
    let options = ApplyOptions {
        authorized_users_seed: seed,
        ..ApplyOptions::default()
    };
    let mut report = installer::apply(
        action,
        &ApplyContext {
            paths: &paths,
            runner: &SystemRunner::default(),
            probe: &health_check,
            payload: payload.as_ref(),
            options,
        },
        progress,
    )?;
    if !matches!(action, ServiceAction::Uninstall { .. }) && !paths.authorized_users_path().exists()
    {
        report.notes.push(
            "authorize each desktop user with `sudo awcc-cli service authorize <user-or-uid>`"
                .to_owned(),
        );
    }
    Ok(report)
}

/// Apply a lifecycle action, obtaining permission through polkit when the caller is not root.
///
/// `cli_executable` must identify `awcc-cli`, including when called by the GUI. In an AppImage,
/// the original image from `APPIMAGE` is relaunched and extracted by its runtime under root.
///
/// # Errors
/// Returns discovery, authorization, verification or installer failures with their details.
pub fn apply_system(
    action: ServiceAction,
    cli_executable: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<ApplyReport, ManagementError> {
    if rustix::process::geteuid().is_root() {
        return apply_as_root(action, cli_executable, progress);
    }
    let target = LaunchTarget::detect(std::env::var_os("APPIMAGE").as_deref(), cli_executable)?;
    Ok(installer::elevate_and_apply(
        action,
        &target,
        &InstallPaths::system(),
        &SystemRunner::default(),
        progress,
    )?)
}

/// Elevated helper protocol. Every line on stdout is an [`ApplyEvent`], including setup errors.
/// Returns `0` on success and `1` on failure; it never attempts another elevation.
pub fn run_system_apply_command(
    action: ServiceAction,
    cli_executable: &Path,
    out: &mut dyn Write,
) -> u8 {
    fn emit(out: &mut dyn Write, event: &ApplyEvent) {
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
    let result = apply_as_root(action, cli_executable, &mut |progress| {
        emit(out, &ApplyEvent::Progress(progress));
    });
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn first_install_authorizes_only_the_identified_invoker() {
        assert_eq!(seed_for_invoker(None, None).unwrap(), None);
        assert_eq!(seed_for_invoker(Some("0"), Some("1000")).unwrap(), None);
        assert!(
            seed_for_invoker(Some("1001"), Some("1000"))
                .unwrap()
                .unwrap()
                .ends_with("\n1001\n")
        );
        assert!(
            seed_for_invoker(None, Some("1000"))
                .unwrap()
                .unwrap()
                .ends_with("\n1000\n")
        );
        for uid in ["", "user", "-1", "+1", "1000\n1001", "4294967295"] {
            assert!(seed_for_invoker(Some(uid), None).is_err(), "{uid:?}");
        }
    }
}
