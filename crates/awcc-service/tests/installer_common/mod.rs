//! Shared fixtures for the offline installer tests: temp roots, payload builder, and a stateful
//! fake `systemctl`/package-manager/`pkexec` host.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use awcc_diagnostics::hash::sha256_hex;
use awcc_service::installer::{
    ApplyContext, ApplyOptions, ApplyReport, HealthProbe, HostCommand, HostError, HostOutput,
    HostRunner, HostTool, InstallError, InstallPaths, PayloadSource, Progress, ServiceAction,
    apply,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Temporary directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "awcc-installer-{label}-{}-{n}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Write `files` plus a matching `MANIFEST.sha256` below `dir`.
pub fn write_payload(dir: &Path, build_id: &str, files: &[(&str, &[u8])]) {
    let mut lines = vec![format!("build-id: {build_id}"), "version: 0.1.2".to_owned()];
    let mut sorted: Vec<_> = files.to_vec();
    sorted.sort();
    for (path, bytes) in &sorted {
        let dest = dir.join(path);
        fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
        fs::write(&dest, bytes).expect("write payload file");
        let mode = if path.starts_with("bin/") {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(&dest, fs::Permissions::from_mode(mode)).expect("chmod");
        lines.push(format!("{}  {path}", sha256_hex(bytes)));
    }
    fs::create_dir_all(dir).expect("mkdir");
    fs::write(dir.join("MANIFEST.sha256"), lines.join("\n") + "\n").expect("manifest");
    fs::set_permissions(
        dir.join("MANIFEST.sha256"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("chmod manifest");
}

/// A normal payload; `with_lib` adds `lib/libusb-1.0.so.0`.
pub fn standard_payload(dir: &Path, build_id: &str, with_lib: bool) {
    let binary = format!("#!/bin/sh\n# awcc-cli {build_id}\n");
    let mut files: Vec<(&str, Vec<u8>)> = vec![
        ("bin/awcc-cli", binary.into_bytes()),
        ("share/awcc/database.json", b"{}".to_vec()),
    ];
    if with_lib {
        files.push(("lib/libusb-1.0.so.0", b"not a real library".to_vec()));
    }
    let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    write_payload(dir, build_id, &borrowed);
}

type Call = (HostTool, Vec<String>, Vec<(String, String)>);

#[derive(Default)]
pub struct FakeState {
    pub enabled: Option<bool>,
    pub active: bool,
    /// `systemctl show` reports the unit as masked.
    pub masked: bool,
    /// Verbs that exit non-zero (e.g. "start").
    pub failing_verbs: HashSet<String>,
    /// After `start`, report `failed` instead of `active`.
    pub start_crashes: bool,
    pub native_owner: Option<(HostTool, String)>,
    pub missing_tools: HashSet<&'static str>,
    pub fragment_override: Option<String>,
    pub pkexec: Option<Result<HostOutput, String>>,
    pub pkexec_stdout_lines: Vec<String>,
    pub calls: Vec<Call>,
}

pub struct FakeHost {
    root: PathBuf,
    pub state: Mutex<FakeState>,
}

fn out(code: i32, stdout: &str, stderr: &str) -> HostOutput {
    HostOutput {
        code: Some(code),
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

impl FakeHost {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            state: Mutex::new(FakeState::default()),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        f(&mut self.state.lock().expect("fake state"))
    }

    pub fn calls(&self) -> Vec<(HostTool, Vec<String>)> {
        self.with(|s| s.calls.iter().map(|(t, a, _)| (*t, a.clone())).collect())
    }

    /// systemctl verbs (without the leading global flags) in call order.
    pub fn verbs(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|(t, _)| *t == HostTool::Systemctl)
            .map(|(_, a)| a[2..].join(" "))
            .filter(|v| !v.starts_with("show"))
            .collect()
    }

    pub fn clear_calls(&self) {
        self.with(|s| s.calls.clear());
    }
}

impl HostRunner for FakeHost {
    fn run(
        &self,
        command: &HostCommand,
        on_stdout_line: Option<&mut dyn FnMut(&str)>,
    ) -> Result<HostOutput, HostError> {
        let args: Vec<String> = command
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut state = self.state.lock().expect("fake state");
        let env: Vec<(String, String)> = command
            .env()
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        state.calls.push((command.tool, args.clone(), env));
        if state.missing_tools.contains(command.tool.executable_name()) {
            return Err(HostError::ToolNotFound(command.tool));
        }
        match command.tool {
            HostTool::Busctl => Ok(HostOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }),
            HostTool::Systemctl => {
                let verb = args.get(2).map_or("", String::as_str).to_owned();
                if state.failing_verbs.contains(&verb) {
                    return Ok(out(1, "", &format!("Failed to {verb}: injected failure")));
                }
                match verb.as_str() {
                    "show" => {
                        let unit = self.root.join("etc/systemd/system/awccd.service");
                        let exists = fs::symlink_metadata(&unit).is_ok();
                        let load = if state.masked {
                            "masked"
                        } else if exists
                            || state.fragment_override.is_some()
                            || self
                                .root
                                .join("usr/lib/systemd/system/awccd.service")
                                .exists()
                        {
                            "loaded"
                        } else {
                            "not-found"
                        };
                        let loaded = load == "loaded";
                        let active = if loaded && state.active {
                            "active"
                        } else {
                            "inactive"
                        };
                        let file_state = match (loaded, state.enabled) {
                            (true, Some(true)) => "enabled",
                            (true, Some(false)) => "disabled",
                            _ => "",
                        };
                        let fragment = state.fragment_override.clone().unwrap_or_else(|| {
                            if exists && loaded {
                                "/etc/systemd/system/awccd.service".to_owned()
                            } else {
                                String::new()
                            }
                        });
                        let active = if state.start_crashes && state.active {
                            "failed"
                        } else {
                            active
                        };
                        Ok(out(
                            0,
                            &format!(
                                "LoadState={load}\nActiveState={active}\nSubState=x\n\
                                 UnitFileState={file_state}\nFragmentPath={fragment}\n"
                            ),
                            "",
                        ))
                    }
                    "stop" => {
                        state.active = false;
                        Ok(out(0, "", ""))
                    }
                    "start" => {
                        state.active = true;
                        Ok(out(0, "", ""))
                    }
                    "enable" => {
                        state.enabled = Some(true);
                        Ok(out(0, "", ""))
                    }
                    "disable" => {
                        state.enabled = Some(false);
                        Ok(out(0, "", ""))
                    }
                    "daemon-reload" => Ok(out(0, "", "")),
                    other => Ok(out(1, "", &format!("unexpected verb {other}"))),
                }
            }
            HostTool::Rpm | HostTool::Dpkg | HostTool::Pacman => match &state.native_owner {
                Some((tool, package)) if *tool == command.tool => {
                    let stdout = match tool {
                        HostTool::Dpkg => format!("{package}:amd64: /x\n"),
                        _ => format!("{package}\n"),
                    };
                    Ok(out(0, &stdout, ""))
                }
                _ => Ok(out(1, "", "not owned")),
            },
            HostTool::Journalctl
            | HostTool::DbusBroker
            | HostTool::DbusDaemon
            | HostTool::PlasmaShell
            | HostTool::GnomeShell => panic!("unexpected diagnostic tool during installer test"),
            HostTool::Pkexec => {
                let lines = state.pkexec_stdout_lines.clone();
                let response = state.pkexec.clone();
                drop(state);
                if let Some(callback) = on_stdout_line {
                    for line in &lines {
                        callback(line);
                    }
                }
                match response {
                    Some(Ok(output)) => Ok(output),
                    Some(Err(text)) => Err(HostError::Untrusted {
                        path: "pkexec".to_owned(),
                        reason: text,
                    }),
                    None => Ok(out(0, "", "")),
                }
            }
        }
    }
}

pub struct ProbeOk;
impl HealthProbe for ProbeOk {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

pub struct ProbeFails(pub &'static str);
impl HealthProbe for ProbeFails {
    fn check(&self) -> Result<(), String> {
        Err(self.0.to_owned())
    }
}

/// A fully relocated installation sandbox.
pub struct Sandbox {
    pub tmp: TempDir,
    pub root: PathBuf,
    pub paths: InstallPaths,
    pub host: FakeHost,
}

impl Sandbox {
    pub fn new(label: &str) -> Self {
        let tmp = TempDir::new(label);
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("run/systemd/system")).expect("systemd dir");
        let paths = InstallPaths::with_root(&root);
        let host = FakeHost::new(&root);
        Self {
            tmp,
            root,
            paths,
            host,
        }
    }

    pub fn payload(&self, build_id: &str, with_lib: bool) -> PathBuf {
        let dir = self.tmp.path().join(format!("payload-{build_id}"));
        let _ = fs::remove_dir_all(&dir);
        standard_payload(&dir, build_id, with_lib);
        dir
    }

    pub fn options() -> ApplyOptions {
        ApplyOptions {
            health_timeout: Duration::from_millis(200),
            health_interval: Duration::from_millis(5),
            command_timeout: Duration::from_secs(5),
            authorized_users_seed: None,
        }
    }

    pub fn run(
        &self,
        action: ServiceAction,
        payload: Option<&Path>,
        probe: &dyn HealthProbe,
        options: ApplyOptions,
    ) -> Result<ApplyReport, InstallError> {
        self.run_with_progress(action, payload, probe, options, &mut |_| {})
    }

    pub fn run_with_progress(
        &self,
        action: ServiceAction,
        payload: Option<&Path>,
        probe: &dyn HealthProbe,
        options: ApplyOptions,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<ApplyReport, InstallError> {
        let source = payload.map(PayloadSource::new);
        let ctx = ApplyContext {
            paths: &self.paths,
            runner: &self.host,
            probe,
            payload: source.as_ref(),
            options,
        };
        apply(action, &ctx, progress)
    }

    pub fn install(&self, payload: &Path) -> Result<ApplyReport, InstallError> {
        self.run(
            ServiceAction::Install,
            Some(payload),
            &ProbeOk,
            Self::options(),
        )
    }

    pub fn path(&self, logical: &str) -> PathBuf {
        self.paths.join(logical)
    }

    /// Every file and symlink below the root, relative, for "nothing was written" assertions.
    pub fn tree(&self) -> BTreeSet<String> {
        fn walk(dir: &Path, base: &Path, out: &mut BTreeSet<String>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let rel = path
                        .strip_prefix(base)
                        .expect("prefix")
                        .display()
                        .to_string();
                    out.insert(rel);
                    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                        walk(&path, base, out);
                    }
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&self.root, &self.root, &mut out);
        out
    }
}
