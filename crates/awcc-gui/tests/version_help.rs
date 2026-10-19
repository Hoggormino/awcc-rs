//! Black-box checks of the real `awcc-gui` binary: `--version` and `--help` must work with no
//! graphical session, no hardware, no writable state directory, and a device database that
//! cannot be loaded. If either created a Slint window, resolved a profile, or started a service
//! worker, the process would fail in this environment.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    let cwd = std::env::temp_dir().join(format!(
        "awcc_gui_hostile_{}_{}",
        std::process::id(),
        args.join("_")
            .replace(|c: char| !c.is_ascii_alphanumeric(), "")
    ));
    std::fs::create_dir_all(&cwd).expect("create scratch cwd");
    let output = Command::new(env!("CARGO_BIN_EXE_awcc-gui"))
        .args(args)
        .env_clear()
        .env("PATH", "")
        .env("HOME", "/nonexistent-awcc-home")
        .env("XDG_STATE_HOME", "/nonexistent-awcc-state")
        .env("AWCC_DATABASE_PATH", "/nonexistent-awcc/database.json")
        .current_dir(&cwd)
        .output()
        .expect("spawn awcc-gui");
    let _ = std::fs::remove_dir_all(cwd);
    output
}

#[test]
fn version_and_help_succeed_without_display_hardware_state_dir_or_database() {
    for args in [
        &["--version"][..],
        &["-V"],
        &["--sim", "--version"],
        &["--help"],
        &["-h"],
    ] {
        let out = run(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "{args:?} wrote to stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.stdout.is_empty(), "{args:?} printed nothing");
    }
}

#[test]
fn version_output_matches_the_cli_format() {
    let out = run(&["--version"]);
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(text.starts_with("awcc-gui "), "{text}");
    assert!(text.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))));
    for key in [
        "commit:",
        "commit-source:",
        "dirty:",
        "source-date-epoch:",
        "target:",
        "profile:",
        "rustc:",
        "package-format:",
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(key)),
            "missing {key}: {text}"
        );
    }
}

#[test]
fn control_case_the_hostile_environment_really_breaks_profile_resolution() {
    // Without this, a passing `--version` above would prove nothing.
    let out = run(&["--sim", "--device-info"]);
    assert!(!out.status.success(), "device-info must fail closed here");
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
    assert!(out.stdout.is_empty());
}
