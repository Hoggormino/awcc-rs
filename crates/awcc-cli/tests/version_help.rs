//! Black-box checks of the real `awcc-cli` binary: `--version` and `--help` must work in an
//! environment that makes any hardware, profile, state-directory, or privilege access fail.
//!
//! The environment is hostile on purpose: an empty `PATH` (no `pkexec`/`sudo`), no `HOME`
//! state directory, no display variables, a working directory that cannot be written, and
//! `AWCC_DATABASE_PATH` pointing at a file that does not exist. The last one is a tripwire: any
//! attempt to resolve a device profile fails closed, which the control case below proves.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

fn hostile_command(args: &[&str]) -> (Command, PathBuf) {
    let cwd = std::env::temp_dir().join(format!(
        "awcc_cli_hostile_{}_{}_{}",
        std::process::id(),
        NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed),
        args.join("_")
            .replace(|c: char| !c.is_ascii_alphanumeric(), "")
    ));
    std::fs::create_dir_all(&cwd).expect("create scratch cwd");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_awcc-cli"));
    cmd.args(args)
        .env_clear()
        .env("PATH", "")
        .env("HOME", "/nonexistent-awcc-home")
        .env("XDG_STATE_HOME", "/nonexistent-awcc-state")
        .env("AWCC_DATABASE_PATH", "/nonexistent-awcc/database.json")
        .current_dir(&cwd);
    (cmd, cwd)
}

fn run(args: &[&str]) -> Output {
    let (mut cmd, cwd) = hostile_command(args);
    let output = cmd.output().expect("spawn awcc-cli");
    let _ = std::fs::remove_dir_all(cwd);
    output
}

#[test]
fn version_and_help_succeed_without_hardware_state_dir_or_database() {
    for args in [
        &["--version"][..],
        &["-V"],
        &["--sim", "--version"],
        &["--help"],
        &["-h"],
        &[],
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
fn version_output_has_the_shared_stable_fields() {
    let out = run(&["--version"]);
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(text.starts_with("awcc-cli "), "{text}");
    assert!(text.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))));
    for key in ["commit:", "dirty:", "target:", "rustc:", "package-format:"] {
        assert!(
            text.lines().any(|l| l.starts_with(key)),
            "missing {key}: {text}"
        );
    }
}

#[test]
fn control_case_the_hostile_environment_really_breaks_profile_resolution() {
    // If this passed, the tripwire above would prove nothing about `--version`.
    let out = run(&["--sim", "device-info"]);
    assert!(!out.status.success(), "device-info must fail closed here");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn backend_start_failure_exits_nonzero_and_keeps_stdout_clean() {
    let out = run(&["--sim", "st", "0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
}

#[test]
fn service_status_is_passive_and_reports_availability_as_json() {
    let out = run(&["service", "status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report["installation"]["managed_by"].is_string());
    assert!(report["daemon"]["state"].is_string());
}

#[test]
fn elevated_helper_setup_errors_follow_the_jsonl_protocol() {
    // This test binary has no bundled service payload. It must fail before an installation
    // can start, including under a root test runner, and still speak the elevation protocol.
    let out = run(&["service-apply", "install"]);
    assert_eq!(out.status.code(), Some(1));
    let event: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(event["event"], "failed");
    assert!(event["message"].is_string());
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
