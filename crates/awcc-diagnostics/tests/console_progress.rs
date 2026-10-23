#![allow(clippy::unwrap_used, clippy::expect_used)]
use awcc_diagnostics::{DiagnosticsConfig, DiagnosticsRuntime, Severity, resolve_log_level};
use std::{
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[test]
fn worker_logs_while_main_holds_stderr() {
    if std::env::var_os("AWCC_CONSOLE_TEST_CHILD").is_none() {
        return;
    }
    let locked = std::io::stderr().lock();
    let mut cfg = DiagnosticsConfig::from_env_and_cli(
        "console-test",
        resolve_log_level(None, false, None).unwrap(),
        false,
    );
    cfg.enable_file_sink = false;
    cfg.service_console = true;
    let runtime = DiagnosticsRuntime::new(cfg);
    let (done, rx) = mpsc::sync_channel(1);
    let writer = runtime.clone();
    let thread = std::thread::spawn(move || {
        for n in 0..100 {
            writer.record(awcc_diagnostics::NewEvent::new(
                Severity::Error,
                "test",
                format!("worker error {n}"),
            ));
        }
        done.send(()).unwrap();
    });
    let progress = rx.recv_timeout(Duration::from_secs(1));
    drop(locked);
    assert!(progress.is_ok(), "worker stalled behind stderr lock");
    thread.join().unwrap();
    assert!(runtime.events_snapshot().0.len() >= 100);
    runtime.shutdown(Duration::from_secs(1));
}

#[test]
fn child_process_console_regression_finishes_and_delivers_errors() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "worker_logs_while_main_holds_stderr",
            "--nocapture",
        ])
        .env("AWCC_CONSOLE_TEST_CHILD", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("child stalled");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("worker error 99"));
}
