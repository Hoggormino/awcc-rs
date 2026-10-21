//! User-side elevation: relaunch the running binary under `pkexec` and relay its progress.
//!
//! The elevated child (`service-apply`, see [`super::run_apply_command`]) prints one JSON
//! [`ApplyEvent`] per line on stdout. Everything else on stdout/stderr (pkexec or runtime noise)
//! is ignored for the protocol and only used to classify failures.
//!
//! * AppImage: `pkexec <APPIMAGE path> --appimage-extract-and-run --service-apply <action>`. The
//!   user's FUSE mount is not readable by root, so the runtime extracts itself for the elevated
//!   run. The argv is an array, so paths with spaces need no quoting.
//! * Any other `awcc-cli`: `pkexec <absolute exe> service-apply <action>`.
//!
//! `pkexec` exits `126` when the user dismissed the prompt and `127` for authorization or
//! execution errors; the child reserves `0`/`1` for success/failure.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::host::{HostCommand, HostError, HostRunner, HostTool};
use super::{ApplyReport, InstallPaths, Progress, ServiceAction};

/// One line of the elevated child's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ApplyEvent {
    Progress(Progress),
    Done(ApplyReport),
    Failed { kind: String, message: String },
}

/// What to relaunch under `pkexec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTarget {
    /// A running AppImage (`$APPIMAGE`, canonical absolute path).
    AppImage { path: PathBuf },
    /// A plain executable (canonical absolute path).
    Executable { path: PathBuf },
}

impl LaunchTarget {
    /// Choose the launch target. `appimage_env` is the value of `$APPIMAGE`; `current_exe` the
    /// path of the running `awcc-cli`.
    ///
    /// # Errors
    /// [`ElevationError::ExecutableInaccessible`] when the chosen file cannot be resolved.
    pub fn detect(
        appimage_env: Option<&OsStr>,
        current_exe: &Path,
    ) -> Result<Self, ElevationError> {
        let (path, appimage) = match appimage_env.filter(|v| !v.is_empty()) {
            Some(value) => (PathBuf::from(value), true),
            None => (current_exe.to_path_buf(), false),
        };
        let canonical = std::fs::canonicalize(&path)
            .ok()
            .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()));
        let Some(canonical) = canonical else {
            let hint = terminal_hint(&path, appimage, ServiceAction::Install);
            return Err(ElevationError::ExecutableInaccessible {
                path: path.display().to_string(),
                hint,
            });
        };
        Ok(if appimage {
            Self::AppImage { path: canonical }
        } else {
            Self::Executable { path: canonical }
        })
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        match self {
            Self::AppImage { path } | Self::Executable { path } => path,
        }
    }

    const fn is_appimage(&self) -> bool {
        matches!(self, Self::AppImage { .. })
    }

    /// Arguments that follow the program for `action`.
    #[must_use]
    pub fn apply_args(&self, action: ServiceAction) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if self.is_appimage() {
            args.push("--appimage-extract-and-run".into());
            args.push("--service-apply".into());
        } else {
            args.push("service-apply".into());
        }
        args.push(action.name().into());
        if matches!(action, ServiceAction::Uninstall { purge: true }) {
            args.push("--purge".into());
        }
        args
    }

    /// The terminal alternative to show when elevation is impossible.
    #[must_use]
    pub fn terminal_command(&self, action: ServiceAction) -> String {
        terminal_hint(self.program(), self.is_appimage(), action)
    }
}

fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'))
    {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn terminal_hint(program: &Path, appimage: bool, action: ServiceAction) -> String {
    let flag = if appimage {
        "--service-apply"
    } else {
        "service-apply"
    };
    let purge = if matches!(action, ServiceAction::Uninstall { purge: true }) {
        " --purge"
    } else {
        ""
    };
    format!(
        "sudo {} {flag} {}{purge}",
        shell_quote(&program.to_string_lossy()),
        action.name()
    )
}

/// The exact `pkexec` command for `action` (an argv array, no shell).
#[must_use]
pub fn build_pkexec_command(action: ServiceAction, target: &LaunchTarget) -> HostCommand {
    let mut args: Vec<OsString> = vec![target.program().as_os_str().to_owned()];
    args.extend(target.apply_args(action));
    HostCommand::new(HostTool::Pkexec, args)
}

/// Why the unprivileged side could not complete the elevated operation.
#[derive(Debug, Error)]
pub enum ElevationError {
    #[error("systemd is not the running init system; the service installer supports systemd only")]
    UnsupportedInit,
    #[error("`pkexec` (polkit) is not installed. Run in a terminal instead: {hint}")]
    PkexecUnavailable { hint: String },
    #[error(
        "no polkit authentication agent is available to ask for your password. Run in a terminal \
         instead: {hint}"
    )]
    NoAuthAgent { hint: String },
    #[error("authorization was cancelled; nothing was changed")]
    Cancelled,
    #[error("authorization was denied. Run in a terminal instead: {hint}\n{stderr}")]
    NotAuthorized { hint: String, stderr: String },
    #[error("cannot launch {path} as root (inaccessible or not a regular file). Try: {hint}")]
    ExecutableInaccessible { path: String, hint: String },
    #[error("could not run pkexec: {0}")]
    Host(#[from] HostError),
    /// The elevated installer ran and reported a failure.
    #[error("{message}")]
    Apply { kind: String, message: String },
    #[error("the elevated installer exited with status {code} without a result: {stderr}")]
    Failed { code: String, stderr: String },
    #[error("the elevated installer finished without reporting a result")]
    Protocol,
}

impl ElevationError {
    /// Stable machine-readable code for UIs.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::UnsupportedInit => "unsupported_init",
            Self::PkexecUnavailable { .. } => "pkexec_unavailable",
            Self::NoAuthAgent { .. } => "no_auth_agent",
            Self::Cancelled => "cancelled",
            Self::NotAuthorized { .. } => "not_authorized",
            Self::ExecutableInaccessible { .. } => "executable_inaccessible",
            Self::Host(_) => "host",
            Self::Apply { kind, .. } => kind,
            Self::Failed { .. } => "failed",
            Self::Protocol => "protocol",
        }
    }
}

/// Classify a `pkexec` exit that is not a clean child result.
fn classify_pkexec_failure(
    code: Option<i32>,
    stderr: &str,
    target: &LaunchTarget,
    action: ServiceAction,
) -> ElevationError {
    let hint = target.terminal_command(action);
    let lower = stderr.to_ascii_lowercase();
    match code {
        Some(126) => ElevationError::Cancelled,
        Some(127) => {
            if lower.contains("no authentication agent")
                || lower.contains("error getting authority")
                || lower.contains("could not connect")
            {
                ElevationError::NoAuthAgent { hint }
            } else if lower.contains("not authorized") {
                ElevationError::NotAuthorized {
                    hint,
                    stderr: stderr.trim().to_owned(),
                }
            } else if lower.contains("error executing")
                || lower.contains("no such file")
                || lower.contains("permission denied")
            {
                ElevationError::ExecutableInaccessible {
                    path: target.program().display().to_string(),
                    hint,
                }
            } else {
                ElevationError::NotAuthorized {
                    hint,
                    stderr: stderr.trim().to_owned(),
                }
            }
        }
        other => ElevationError::Failed {
            code: other.map_or_else(|| "signal".to_owned(), |c| c.to_string()),
            stderr: stderr.trim().to_owned(),
        },
    }
}

/// Relaunch `target` under `pkexec` for `action` and relay progress. Run this only when the
/// current process is **not** root; as root call the apply entry point directly.
///
/// # Errors
/// [`ElevationError`], with cancellation, missing `pkexec`, missing authentication agent,
/// inaccessible executable, and unsupported init each reported as a distinct variant.
pub fn elevate_and_apply(
    action: ServiceAction,
    target: &LaunchTarget,
    paths: &InstallPaths,
    runner: &dyn HostRunner,
    progress: &mut dyn FnMut(Progress),
) -> Result<ApplyReport, ElevationError> {
    if !paths.systemd_runtime_dir().is_dir() {
        return Err(ElevationError::UnsupportedInit);
    }
    let command = build_pkexec_command(action, target);
    let mut done: Option<ApplyReport> = None;
    let mut failed: Option<(String, String)> = None;
    let output = {
        let mut on_line = |line: &str| match serde_json::from_str::<ApplyEvent>(line) {
            Ok(ApplyEvent::Progress(p)) => progress(p),
            Ok(ApplyEvent::Done(report)) => done = Some(report),
            Ok(ApplyEvent::Failed { kind, message }) => failed = Some((kind, message)),
            Err(_) => {}
        };
        runner.run(&command, Some(&mut on_line))
    };
    let output = match output {
        Ok(output) => output,
        Err(HostError::ToolNotFound(HostTool::Pkexec)) => {
            return Err(ElevationError::PkexecUnavailable {
                hint: target.terminal_command(action),
            });
        }
        Err(error) => return Err(ElevationError::Host(error)),
    };
    if let Some((kind, message)) = failed {
        return Err(ElevationError::Apply { kind, message });
    }
    match (output.code, done) {
        (Some(0), Some(report)) => Ok(report),
        (Some(0), None) => Err(ElevationError::Protocol),
        (code, _) => Err(classify_pkexec_failure(
            code,
            &output.stderr,
            target,
            action,
        )),
    }
}
