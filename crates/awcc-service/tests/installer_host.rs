//! The real `SystemRunner`: trusted resolution, clean environment, timeouts.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod installer_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use awcc_service::installer::unit::render_unit;
use awcc_service::installer::{HostCommand, HostError, HostRunner, HostTool, SystemRunner};
use installer_common::TempDir;

fn script(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[test]
fn host_tools_run_with_only_the_trusted_environment() {
    let tmp = TempDir::new("host-env");
    script(tmp.path(), "systemctl", "/usr/bin/env");
    let runner = SystemRunner::with_search_dirs(vec![tmp.path().to_path_buf()]);
    let output = runner
        .run(&HostCommand::new(HostTool::Systemctl, ["show"]), None)
        .expect("run");
    assert!(output.success(), "{output:?}");
    // The `/bin/sh` shebang itself adds PWD/SHLVL/_; everything else would be inherited leakage.
    let mut vars: Vec<&str> = output
        .stdout
        .lines()
        .filter(|l| !l.starts_with("PWD=") && !l.starts_with("SHLVL=") && !l.starts_with("_="))
        .collect();
    vars.sort_unstable();
    assert_eq!(vars, ["LC_ALL=C", "PATH=/usr/sbin:/usr/bin:/sbin:/bin"]);
}

#[test]
fn argv_is_passed_verbatim_without_a_shell() {
    let tmp = TempDir::new("host-argv");
    script(
        tmp.path(),
        "pkexec",
        "for a in \"$@\"; do printf '[%s]\\n' \"$a\"; done",
    );
    let runner = SystemRunner::with_search_dirs(vec![tmp.path().to_path_buf()]);
    let mut lines = Vec::new();
    let mut on_line = |l: &str| lines.push(l.to_owned());
    let output = runner
        .run(
            &HostCommand::new(
                HostTool::Pkexec,
                ["/a b/App.AppImage", "$(touch pwned)", "; rm -rf /"],
            ),
            Some(&mut on_line),
        )
        .expect("run");
    assert!(output.success());
    assert_eq!(
        lines,
        ["[/a b/App.AppImage]", "[$(touch pwned)]", "[; rm -rf /]"]
    );
    assert!(!std::path::Path::new("pwned").exists());
}

#[test]
fn unresolvable_tool_and_timeouts_are_typed_errors() {
    let tmp = TempDir::new("host-missing");
    let runner = SystemRunner::with_search_dirs(vec![tmp.path().to_path_buf()]);
    let error = runner
        .run(&HostCommand::new(HostTool::Rpm, ["-q"]), None)
        .expect_err("missing");
    assert!(matches!(error, HostError::ToolNotFound(HostTool::Rpm)));

    script(tmp.path(), "dpkg", "sleep 5");
    let slow = HostCommand::new(HostTool::Dpkg, ["-S"]).with_timeout(Duration::from_millis(100));
    let error = runner.run(&slow, None).expect_err("timeout");
    assert!(matches!(error, HostError::TimedOut { .. }), "{error}");
}

#[test]
fn production_runner_only_searches_trusted_system_directories() {
    // A tool that does not exist in the trusted directories is never found via $PATH.
    let runner = SystemRunner::new();
    for tool in [HostTool::Systemctl, HostTool::Pkexec, HostTool::Rpm] {
        if let Ok(path) = runner.resolve(tool) {
            assert!(
                ["/usr/sbin", "/usr/bin", "/sbin", "/bin"]
                    .iter()
                    .any(|d| path.starts_with(d)),
                "{path:?}"
            );
        }
    }
}

#[test]
fn rendered_unit_passes_systemd_analyze_verify_when_available() {
    if std::process::Command::new("systemd-analyze")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("systemd-analyze not available; skipping");
        return;
    }
    for has_lib in [true, false] {
        let tmp = TempDir::new("analyze");
        let service_root = tmp.path().join("opt/awcc-rs/service");
        fs::create_dir_all(service_root.join("current")).expect("mkdir");
        // `current` is a plain directory here only so ExecStart resolves; production uses a symlink.
        fs::create_dir_all(service_root.join("current/bin")).expect("mkdir");
        let exe = service_root.join("current/bin/awcc-cli");
        fs::write(&exe, "#!/bin/sh\nexit 0\n").expect("exe");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod");
        let unit = tmp.path().join("awccd.service");
        fs::write(
            &unit,
            render_unit(&service_root.display().to_string(), has_lib),
        )
        .expect("unit");
        let output = std::process::Command::new("systemd-analyze")
            .arg("verify")
            .arg(&unit)
            .output()
            .expect("systemd-analyze");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "has_lib={has_lib}: {stderr}");
        assert!(
            !stderr.contains("Unknown key") && !stderr.contains("Unknown section"),
            "has_lib={has_lib}: {stderr}"
        );
    }
}
