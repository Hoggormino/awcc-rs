//! Host command execution for the service installer.
//!
//! Every external program (`systemctl`, the package managers, `pkexec`) goes through
//! [`HostRunner`]. A command is an argv array of a [`HostTool`] enum, never a shell string. The
//! real runner ([`SystemRunner`]) resolves the executable from a fixed list of trusted directories
//! (never from the inherited `PATH`), refuses executables that are not root-owned or that are
//! group/world-writable, and starts the child with a cleared environment containing only
//! [`TRUSTED_ENV`] (so `LD_LIBRARY_PATH`, `LD_PRELOAD`, `APPDIR`, `APPIMAGE`, ... never leak into
//! a privileged tool).

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// The only environment variables a host tool is started with.
pub const TRUSTED_ENV: [(&str, &str); 2] =
    [("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"), ("LC_ALL", "C")];

/// Directories searched (in order) for host tools.
pub const TRUSTED_DIRS: [&str; 4] = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"];

/// Maximum bytes kept from each output stream of a host command.
const OUTPUT_CAP: usize = 1 << 20;

/// External programs the installer may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostTool {
    Systemctl,
    Rpm,
    Dpkg,
    Pacman,
    Pkexec,
}

impl HostTool {
    /// File name looked up in the trusted directories.
    #[must_use]
    pub const fn executable_name(self) -> &'static str {
        match self {
            Self::Systemctl => "systemctl",
            Self::Rpm => "rpm",
            Self::Dpkg => "dpkg",
            Self::Pacman => "pacman",
            Self::Pkexec => "pkexec",
        }
    }
}

/// One host command: a tool, its argv (without `argv[0]`), and an optional time limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCommand {
    pub tool: HostTool,
    pub args: Vec<OsString>,
    pub timeout: Option<Duration>,
}

impl HostCommand {
    #[must_use]
    pub fn new<I, S>(tool: HostTool, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            tool,
            args: args.into_iter().map(Into::into).collect(),
            timeout: None,
        }
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The environment the command is run with. Always [`TRUSTED_ENV`].
    #[must_use]
    pub const fn env(&self) -> &'static [(&'static str, &'static str)] {
        &TRUSTED_ENV
    }

    /// Lossy single-line rendering for logs and error messages.
    #[must_use]
    pub fn display(&self) -> String {
        let mut text = self.tool.executable_name().to_owned();
        for arg in &self.args {
            text.push(' ');
            text.push_str(&arg.to_string_lossy());
        }
        text
    }
}

/// Captured result of a host command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOutput {
    /// Exit code, `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl HostOutput {
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self.code, Some(0))
    }
}

/// Failure to run a host command at all (as opposed to the command exiting non-zero).
#[derive(Debug, Error)]
pub enum HostError {
    #[error("`{}` was not found in the trusted system directories", .0.executable_name())]
    ToolNotFound(HostTool),
    #[error("refusing to run {path}: {reason}")]
    Untrusted { path: String, reason: String },
    #[error("failed to start `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("`{command}` did not finish within {after:?} and was killed")]
    TimedOut { command: String, after: Duration },
}

/// Runs host commands. Tests inject a fake that records argv arrays.
pub trait HostRunner {
    /// Run `command` to completion. When `on_stdout_line` is given, every complete stdout line is
    /// delivered as it arrives (used to stream child progress); the full output is still captured.
    ///
    /// # Errors
    /// Returns [`HostError`] when the tool cannot be resolved, started, or exceeds its timeout.
    fn run(
        &self,
        command: &HostCommand,
        on_stdout_line: Option<&mut dyn FnMut(&str)>,
    ) -> Result<HostOutput, HostError>;
}

/// The real host runner.
#[derive(Debug, Clone)]
pub struct SystemRunner {
    dirs: Vec<PathBuf>,
    enforce_trust: bool,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemRunner {
    /// Production runner: fixed system directories, root-ownership enforced.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dirs: TRUSTED_DIRS.iter().map(PathBuf::from).collect(),
            enforce_trust: true,
        }
    }

    /// Runner over custom directories without ownership enforcement (tests only; reachable only
    /// through this explicit constructor, never through the environment).
    #[must_use]
    pub fn with_search_dirs(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            enforce_trust: false,
        }
    }

    /// Resolve `tool` to an absolute executable path.
    ///
    /// # Errors
    /// [`HostError::ToolNotFound`] or [`HostError::Untrusted`].
    pub fn resolve(&self, tool: HostTool) -> Result<PathBuf, HostError> {
        for dir in &self.dirs {
            let candidate = dir.join(tool.executable_name());
            let Ok(meta) = std::fs::metadata(&candidate) else {
                continue;
            };
            if !meta.is_file() || meta.mode() & 0o111 == 0 {
                continue;
            }
            if self.enforce_trust {
                if meta.uid() != 0 {
                    return Err(HostError::Untrusted {
                        path: candidate.display().to_string(),
                        reason: "not owned by root".to_owned(),
                    });
                }
                if meta.mode() & 0o022 != 0 {
                    return Err(HostError::Untrusted {
                        path: candidate.display().to_string(),
                        reason: "writable by group or others".to_owned(),
                    });
                }
            }
            return Ok(candidate);
        }
        Err(HostError::ToolNotFound(tool))
    }
}

fn drain(mut reader: impl Read) -> String {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = OUTPUT_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

impl HostRunner for SystemRunner {
    fn run(
        &self,
        command: &HostCommand,
        on_stdout_line: Option<&mut dyn FnMut(&str)>,
    ) -> Result<HostOutput, HostError> {
        let program = self.resolve(command.tool)?;
        let display = command.display();
        let mut child = Command::new(&program)
            .args(&command.args)
            .env_clear()
            .envs(command.env().iter().copied())
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| HostError::Spawn {
                command: display.clone(),
                source,
            })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stderr_thread = thread::spawn(move || stderr.map(drain).unwrap_or_default());

        if let Some(callback) = on_stdout_line {
            // Streaming mode: no timeout (used for interactive elevation).
            let mut captured = String::new();
            if let Some(out) = stdout {
                let mut reader = BufReader::new(out);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let text = String::from_utf8_lossy(&line);
                            let trimmed = text.trim_end_matches(['\n', '\r']);
                            callback(trimmed);
                            if captured.len() < OUTPUT_CAP {
                                captured.push_str(&text);
                            }
                        }
                    }
                }
            }
            let status = child.wait().map_err(|source| HostError::Spawn {
                command: display,
                source,
            })?;
            return Ok(HostOutput {
                code: status.code(),
                stdout: captured,
                stderr: stderr_thread.join().unwrap_or_default(),
            });
        }

        let stdout_thread = thread::spawn(move || stdout.map(drain).unwrap_or_default());
        let deadline = command.timeout.map(|limit| (Instant::now() + limit, limit));
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(source) => {
                    return Err(HostError::Spawn {
                        command: display,
                        source,
                    });
                }
            }
            if let Some((at, limit)) = deadline
                && Instant::now() >= at
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HostError::TimedOut {
                    command: display,
                    after: limit,
                });
            }
            thread::sleep(Duration::from_millis(10));
        };
        Ok(HostOutput {
            code: status.code(),
            stdout: stdout_thread.join().unwrap_or_default(),
            stderr: stderr_thread.join().unwrap_or_default(),
        })
    }
}
