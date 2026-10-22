//! Explicit service setup repairs an existing deployment without replacing its files.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod installer_common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use awcc_service::installer::{
    ApplyOptions, ApplyOutcome, HostTool, InstallError, OwnerRecord, ProgressStep, ServiceAction,
};
use installer_common::{ProbeFails, ProbeOk, Sandbox};

fn options() -> ApplyOptions {
    ApplyOptions {
        authorized_users_seed: Some("# Invoking desktop account\n1000\n".into()),
        ..Sandbox::options()
    }
}

fn native(sb: &Sandbox, manager: HostTool, package: &str) {
    let unit = sb.path("/usr/lib/systemd/system/awccd.service");
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "[Service]\nExecStart=/usr/bin/awcc-cli -d\n").unwrap();
    sb.host.with(|state| {
        state.enabled = Some(false);
        state.active = false;
        state.native_owner = Some((manager, package.into()));
        state.fragment_override = Some("/usr/lib/systemd/system/awccd.service".into());
    });
}

#[test]
fn setup_authorizes_and_starts_native_packages_without_redeploying_them() {
    for manager in [HostTool::Pacman, HostTool::Dpkg, HostTool::Rpm] {
        let sb = Sandbox::new("native-setup");
        native(&sb, manager, "awcc-rs");
        fs::create_dir_all(sb.path("/etc/awcc")).unwrap();
        fs::write(sb.paths.authorized_users_path(), "1001\n1002\n").unwrap();
        let unit_before = fs::read(sb.path("/usr/lib/systemd/system/awccd.service")).unwrap();
        let mut steps = Vec::new();
        let report = sb
            .run_with_progress(ServiceAction::Setup, None, &ProbeOk, options(), &mut |p| {
                steps.push(p.step)
            })
            .unwrap();

        assert_eq!(report.outcome, ApplyOutcome::Configured);
        assert_eq!(report.enabled, Some(true));
        assert_eq!(report.active, Some(true));
        assert_eq!(report.build_id, None);
        let users =
            awcc_service::ipc::authorized_users::list(&sb.paths.authorized_users_path()).unwrap();
        assert_eq!(
            users.into_iter().collect::<Vec<_>>(),
            ["1000", "1001", "1002"]
        );
        assert_eq!(
            fs::read(sb.path("/usr/lib/systemd/system/awccd.service")).unwrap(),
            unit_before
        );
        assert!(!sb.path("/etc/systemd/system/awccd.service").exists());
        assert!(!sb.path("/opt/awcc-rs").exists());
        assert!(!sb.paths.record_path().exists());
        assert!(!sb.paths.journal_path().exists());
        assert_eq!(
            sb.host.verbs(),
            [
                "daemon-reload",
                "enable awccd.service",
                "start awccd.service"
            ]
        );
        assert!(steps.contains(&ProgressStep::HealthCheck));
        assert!(!steps.contains(&ProgressStep::Staging));
        assert_eq!(steps.last(), Some(&ProgressStep::Done));
    }
}

#[test]
fn setup_repairs_appimage_authorization_and_tracks_it_for_purge() {
    let sb = Sandbox::new("appimage-setup");
    let payload = sb.payload("0.1.3-setup", false);
    sb.install(&payload).unwrap();
    let unit_before = fs::read(sb.paths.unit_path()).unwrap();
    let link_before = fs::read_link(sb.paths.current_link()).unwrap();
    sb.host.with(|state| {
        state.enabled = Some(false);
        state.active = false;
    });
    sb.host.clear_calls();

    let report = sb
        .run(ServiceAction::Setup, None, &ProbeOk, options())
        .unwrap();
    assert_eq!(report.outcome, ApplyOutcome::Configured);
    assert_eq!(report.build_id.as_deref(), Some("0.1.3-setup"));
    assert_eq!(fs::read(sb.paths.unit_path()).unwrap(), unit_before);
    assert_eq!(fs::read_link(sb.paths.current_link()).unwrap(), link_before);
    let record: OwnerRecord =
        serde_json::from_slice(&fs::read(sb.paths.record_path()).unwrap()).unwrap();
    assert!(record.created_authorized_users);
    assert!(
        fs::read_to_string(sb.paths.authorized_users_path())
            .unwrap()
            .contains("\n1000\n")
    );
    sb.run(
        ServiceAction::Uninstall { purge: true },
        None,
        &ProbeOk,
        Sandbox::options(),
    )
    .unwrap();
    assert!(!sb.paths.authorized_users_path().exists());
}

#[test]
fn setup_refuses_missing_manual_masked_and_foreign_package_units() {
    for case in ["missing", "manual", "masked", "foreign"] {
        let sb = Sandbox::new(case);
        match case {
            "manual" => {
                let unit = sb.paths.unit_path();
                fs::create_dir_all(unit.parent().unwrap()).unwrap();
                fs::write(unit, "[Service]\nExecStart=/bin/false\n").unwrap();
            }
            "masked" => {
                native(&sb, HostTool::Pacman, "awcc-rs");
                sb.host.with(|state| state.masked = true);
            }
            "foreign" => native(&sb, HostTool::Pacman, "another-service"),
            _ => {}
        }
        let error = sb
            .run(ServiceAction::Setup, None, &ProbeOk, options())
            .unwrap_err();
        assert!(
            matches!(
                error,
                InstallError::NotInstalled
                    | InstallError::UnknownUnit { .. }
                    | InstallError::Masked { .. }
                    | InstallError::SetupFailed { .. }
            ),
            "{case}: {error}"
        );
        assert!(!sb.paths.authorized_users_path().exists());
        assert!(sb.host.verbs().is_empty(), "{case}: {:?}", sb.host.verbs());
    }
}

#[test]
fn setup_refuses_unloaded_local_overrides_of_a_package_unit() {
    let sb = Sandbox::new("setup-local-override");
    native(&sb, HostTool::Rpm, "awcc-rs");
    fs::create_dir_all(sb.paths.unit_path().parent().unwrap()).unwrap();
    fs::write(sb.paths.unit_path(), "[Service]\nExecStart=/bin/false\n").unwrap();
    assert!(
        sb.run(ServiceAction::Setup, None, &ProbeOk, options())
            .is_err()
    );
    assert!(sb.host.verbs().is_empty());
    assert!(!sb.paths.authorized_users_path().exists());
}

#[test]
fn setup_never_recovers_or_deletes_an_interrupted_appimage_journal() {
    let sb = Sandbox::new("setup-journal");
    native(&sb, HostTool::Dpkg, "awcc-rs");
    fs::create_dir_all(sb.paths.state_dir()).unwrap();
    fs::write(sb.paths.journal_path(), b"interrupted journal").unwrap();
    assert!(
        sb.run(ServiceAction::Setup, None, &ProbeOk, options())
            .is_err()
    );
    assert_eq!(
        fs::read(sb.paths.journal_path()).unwrap(),
        b"interrupted journal"
    );
    assert!(sb.host.verbs().is_empty());
    assert!(!sb.paths.authorized_users_path().exists());
}

#[test]
fn setup_requires_a_single_identified_nonroot_invoker() {
    for seed in [
        None,
        Some(""),
        Some("0"),
        Some("4294967295"),
        Some("alice"),
        Some("+1000"),
        Some("1000\n1001"),
    ] {
        let sb = Sandbox::new("setup-invoker");
        native(&sb, HostTool::Pacman, "awcc-rs");
        let options = ApplyOptions {
            authorized_users_seed: seed.map(str::to_owned),
            ..Sandbox::options()
        };
        assert!(matches!(
            sb.run(ServiceAction::Setup, None, &ProbeOk, options),
            Err(InstallError::SetupFailed { .. })
        ));
        assert!(sb.host.verbs().is_empty());
        assert!(!sb.paths.authorized_users_path().exists());
    }
}

#[test]
fn setup_does_not_follow_an_authorization_symlink() {
    let sb = Sandbox::new("setup-symlink");
    native(&sb, HostTool::Pacman, "awcc-rs");
    fs::create_dir_all(sb.path("/etc/awcc")).unwrap();
    let other = sb.tmp.path().join("unrelated");
    fs::write(&other, "keep me").unwrap();
    symlink(&other, sb.paths.authorized_users_path()).unwrap();
    assert!(
        sb.run(ServiceAction::Setup, None, &ProbeOk, options())
            .is_err()
    );
    assert_eq!(fs::read_to_string(other).unwrap(), "keep me");
    assert!(sb.host.verbs().is_empty());
}

#[test]
fn setup_never_reports_ready_after_a_start_or_health_failure() {
    for fail_start in [true, false] {
        let sb = Sandbox::new("setup-unhealthy");
        native(&sb, HostTool::Pacman, "awcc-rs");
        if fail_start {
            sb.host.with(|state| {
                state.failing_verbs.insert("start".into());
            });
        }
        let mut steps = Vec::new();
        let error = sb
            .run_with_progress(
                ServiceAction::Setup,
                None,
                &ProbeFails("protocol incompatible"),
                options(),
                &mut |p| steps.push(p.step),
            )
            .unwrap_err();
        assert!(matches!(error, InstallError::SetupFailed { .. }));
        assert!(error.to_string().contains("may already have been applied"));
        assert!(!steps.contains(&ProgressStep::Done));
    }
}

#[test]
fn setup_rejects_unsafe_authorization_permissions() {
    let sb = Sandbox::new("setup-mode");
    native(&sb, HostTool::Pacman, "awcc-rs");
    fs::create_dir_all(sb.path("/etc/awcc")).unwrap();
    fs::write(sb.paths.authorized_users_path(), "1001\n").unwrap();
    fs::set_permissions(
        sb.paths.authorized_users_path(),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    assert!(
        sb.run(ServiceAction::Setup, None, &ProbeOk, options())
            .is_err()
    );
    assert!(sb.host.verbs().is_empty());
    assert_eq!(
        fs::read_to_string(sb.paths.authorized_users_path()).unwrap(),
        "1001\n"
    );
}
