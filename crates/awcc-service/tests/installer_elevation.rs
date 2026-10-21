//! User-side elevation: pkexec argv, error mapping, and event relay. pkexec is always a fake.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod installer_common;

use std::ffi::OsString;
use std::fs;

use awcc_service::installer::{
    ApplyContext, ApplyEvent, ApplyOutcome, ElevationError, HostOutput, HostTool, LaunchTarget,
    ProgressStep, ServiceAction, build_pkexec_command, elevate_and_apply, run_apply_command,
};
use installer_common::{ProbeOk, Sandbox, TempDir};

fn output(code: i32, stderr: &str) -> HostOutput {
    HostOutput {
        code: Some(code),
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

fn target() -> LaunchTarget {
    LaunchTarget::AppImage {
        path: "/home/u/My Apps/AWCC 1.AppImage".into(),
    }
}

fn elevate(sb: &Sandbox, action: ServiceAction) -> Result<(), ElevationError> {
    elevate_and_apply(action, &target(), &sb.paths, &sb.host, &mut |_| {}).map(|_| ())
}

#[test]
fn appimage_command_is_an_argv_array_with_spaces_intact() {
    let command = build_pkexec_command(ServiceAction::Install, &target());
    assert_eq!(command.tool, HostTool::Pkexec);
    let expected: Vec<OsString> = [
        "/home/u/My Apps/AWCC 1.AppImage",
        "--appimage-extract-and-run",
        "--service-apply",
        "install",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    assert_eq!(command.args, expected);
    assert_eq!(command.timeout, None, "interactive: no timeout");
}

#[test]
fn purge_and_plain_executable_variants() {
    let purge = build_pkexec_command(ServiceAction::Uninstall { purge: true }, &target());
    assert_eq!(purge.args.last().and_then(|a| a.to_str()), Some("--purge"));
    assert_eq!(purge.args[3], OsString::from("uninstall"));
    let exe = LaunchTarget::Executable {
        path: "/opt/my tools/awcc-cli".into(),
    };
    let update = build_pkexec_command(ServiceAction::Update, &exe);
    let expected: Vec<OsString> = ["/opt/my tools/awcc-cli", "service-apply", "update"]
        .iter()
        .map(OsString::from)
        .collect();
    assert_eq!(update.args, expected);
}

#[test]
fn terminal_alternative_quotes_awkward_paths() {
    assert_eq!(
        target().terminal_command(ServiceAction::Install),
        "sudo '/home/u/My Apps/AWCC 1.AppImage' --appimage-extract-and-run --service-apply install"
    );
    let exe = LaunchTarget::Executable {
        path: "/usr/bin/awcc-cli".into(),
    };
    assert_eq!(
        exe.terminal_command(ServiceAction::Uninstall { purge: true }),
        "sudo /usr/bin/awcc-cli service-apply uninstall --purge"
    );
    let quote = LaunchTarget::Executable {
        path: "/tmp/it's".into(),
    };
    assert!(
        quote
            .terminal_command(ServiceAction::Update)
            .contains("'/tmp/it'\\''s'")
    );
}

#[test]
fn detect_prefers_appimage_env_and_canonicalizes() {
    let tmp = TempDir::new("detect");
    let image = tmp.path().join("My App.AppImage");
    fs::write(&image, "x").expect("image");
    let exe = tmp.path().join("awcc-cli");
    fs::write(&exe, "x").expect("exe");
    let detected = LaunchTarget::detect(Some(image.as_os_str()), &exe).expect("appimage");
    assert_eq!(
        detected,
        LaunchTarget::AppImage {
            path: fs::canonicalize(&image).expect("c")
        }
    );
    let plain = LaunchTarget::detect(None, &exe).expect("exe");
    assert_eq!(
        plain,
        LaunchTarget::Executable {
            path: fs::canonicalize(&exe).expect("c")
        }
    );
    let empty = LaunchTarget::detect(Some(std::ffi::OsStr::new("")), &exe).expect("empty env");
    assert!(matches!(empty, LaunchTarget::Executable { .. }));
}

#[test]
fn detect_reports_an_inaccessible_file_with_a_terminal_alternative() {
    let tmp = TempDir::new("detect-bad");
    let missing = tmp.path().join("gone.AppImage");
    let error = LaunchTarget::detect(Some(missing.as_os_str()), &tmp.path().join("exe"))
        .expect_err("missing");
    assert!(matches!(
        error,
        ElevationError::ExecutableInaccessible { .. }
    ));
    assert!(error.to_string().contains("sudo"));
}

#[test]
fn cancelling_the_prompt_is_a_distinct_outcome() {
    let sb = Sandbox::new("cancel");
    sb.host.with(|s| s.pkexec = Some(Ok(output(126, ""))));
    let error = elevate(&sb, ServiceAction::Install).expect_err("cancelled");
    assert!(matches!(error, ElevationError::Cancelled), "{error}");
    assert_eq!(error.kind(), "cancelled");
}

#[test]
fn missing_pkexec_is_distinct_and_offers_the_terminal() {
    let sb = Sandbox::new("nopkexec");
    sb.host.with(|s| {
        s.missing_tools.insert("pkexec");
    });
    let error = elevate(&sb, ServiceAction::Install).expect_err("no pkexec");
    assert!(
        matches!(error, ElevationError::PkexecUnavailable { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("--service-apply install"));
}

#[test]
fn missing_auth_agent_is_distinct() {
    let sb = Sandbox::new("noagent");
    sb.host.with(|s| {
        s.pkexec = Some(Ok(output(
            127,
            "Error getting authority: Could not connect",
        )));
    });
    assert!(matches!(
        elevate(&sb, ServiceAction::Install),
        Err(ElevationError::NoAuthAgent { .. })
    ));
    sb.host.with(|s| {
        s.pkexec = Some(Ok(output(127, "No authentication agent found.")));
    });
    assert!(matches!(
        elevate(&sb, ServiceAction::Install),
        Err(ElevationError::NoAuthAgent { .. })
    ));
}

#[test]
fn denied_authorization_and_inaccessible_executable_are_distinct() {
    let sb = Sandbox::new("denied");
    sb.host.with(|s| {
        s.pkexec = Some(Ok(output(
            127,
            "Error executing command as another user: Not authorized",
        )))
    });
    assert!(matches!(
        elevate(&sb, ServiceAction::Install),
        Err(ElevationError::NotAuthorized { .. })
    ));
    sb.host.with(|s| {
        s.pkexec = Some(Ok(output(
            127,
            "Error executing command as another user: No such file or directory",
        )));
    });
    let error = elevate(&sb, ServiceAction::Install).expect_err("inaccessible");
    assert!(
        matches!(error, ElevationError::ExecutableInaccessible { .. }),
        "{error}"
    );
}

#[test]
fn unsupported_init_is_detected_before_pkexec_runs() {
    let sb = Sandbox::new("noinit");
    fs::remove_dir_all(sb.path("/run")).expect("rm");
    let error = elevate(&sb, ServiceAction::Install).expect_err("no systemd");
    assert!(matches!(error, ElevationError::UnsupportedInit));
    assert!(sb.host.calls().is_empty(), "pkexec must not be started");
}

#[test]
fn child_events_are_relayed_and_the_report_returned() {
    let sb = Sandbox::new("relay");
    // Produce genuine events with the real apply entry point, then feed them back as the
    // elevated child's stdout.
    let payload = sb.payload("0.1.2-aaaa", true);
    let source = awcc_service::installer::PayloadSource::new(&payload);
    let ctx = ApplyContext {
        paths: &sb.paths,
        runner: &sb.host,
        probe: &ProbeOk,
        payload: Some(&source),
        options: Sandbox::options(),
    };
    let mut buffer: Vec<u8> = Vec::new();
    let code = run_apply_command(ServiceAction::Install, &ctx, &mut buffer);
    assert_eq!(code, 0);
    let text = String::from_utf8(buffer).expect("utf8");
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    assert!(
        lines
            .iter()
            .all(|l| serde_json::from_str::<ApplyEvent>(l).is_ok())
    );
    assert!(lines.last().expect("last").contains("\"event\":\"done\""));

    let sb2 = Sandbox::new("relay2");
    let mut noisy = vec!["AppRun: starting".to_owned()];
    noisy.extend(lines);
    sb2.host.with(|s| {
        s.pkexec_stdout_lines = noisy;
        s.pkexec = Some(Ok(output(0, "")));
    });
    let mut steps = Vec::new();
    let report = elevate_and_apply(
        ServiceAction::Install,
        &target(),
        &sb2.paths,
        &sb2.host,
        &mut |p| steps.push(p.step),
    )
    .expect("relayed");
    assert_eq!(report.outcome, ApplyOutcome::Installed);
    assert!(steps.contains(&ProgressStep::Switching));
}

#[test]
fn child_failure_event_surfaces_kind_and_message() {
    let sb = Sandbox::new("child-fail");
    let event = serde_json::to_string(&ApplyEvent::Failed {
        kind: "masked".to_owned(),
        message: "awccd.service is masked".to_owned(),
    })
    .expect("json");
    sb.host.with(|s| {
        s.pkexec_stdout_lines = vec![event];
        s.pkexec = Some(Ok(output(1, "")));
    });
    let error = elevate(&sb, ServiceAction::Install).expect_err("failed");
    assert_eq!(error.kind(), "masked");
    assert_eq!(error.to_string(), "awccd.service is masked");
}

#[test]
fn clean_exit_without_a_result_is_a_protocol_error() {
    let sb = Sandbox::new("protocol");
    sb.host.with(|s| s.pkexec = Some(Ok(output(0, ""))));
    assert!(matches!(
        elevate(&sb, ServiceAction::Install),
        Err(ElevationError::Protocol)
    ));
}

#[test]
fn apply_command_reports_failure_as_json_and_exit_one() {
    let sb = Sandbox::new("apply-fail");
    fs::remove_dir_all(sb.path("/run")).expect("rm");
    let ctx = ApplyContext {
        paths: &sb.paths,
        runner: &sb.host,
        probe: &ProbeOk,
        payload: None,
        options: Sandbox::options(),
    };
    let mut buffer = Vec::new();
    assert_eq!(
        run_apply_command(ServiceAction::Install, &ctx, &mut buffer),
        1
    );
    let text = String::from_utf8(buffer).expect("utf8");
    let last: ApplyEvent = serde_json::from_str(text.lines().last().expect("line")).expect("event");
    assert!(matches!(last, ApplyEvent::Failed { kind, .. } if kind == "unsupported_init"));
}
