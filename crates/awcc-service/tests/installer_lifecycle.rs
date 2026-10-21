//! Offline lifecycle tests: a relocated root, a stateful fake `systemctl`, no privileges.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod installer_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use awcc_service::installer::unit::{SERVICE_ROOT, render_unit};
use awcc_service::installer::{
    ApplyOutcome, InstallError, OwnerRecord, ProgressStep, ServiceAction, UnitState, inspect,
};
use installer_common::{ProbeFails, ProbeOk, Sandbox};

fn record(sb: &Sandbox) -> OwnerRecord {
    serde_json::from_slice(&fs::read(sb.path("/var/lib/awcc/install-owner.json")).expect("record"))
        .expect("record json")
}

#[test]
fn fresh_install_deploys_enables_and_starts() {
    let sb = Sandbox::new("fresh");
    let payload = sb.payload("0.1.2-aaaa", true);
    let mut steps = Vec::new();
    let report = sb
        .run_with_progress(
            ServiceAction::Install,
            Some(&payload),
            &ProbeOk,
            Sandbox::options(),
            &mut |p| steps.push(p.step),
        )
        .expect("install");
    assert_eq!(report.outcome, ApplyOutcome::Installed);
    assert_eq!(report.build_id.as_deref(), Some("0.1.2-aaaa"));
    assert_eq!(report.enabled, Some(true));
    assert_eq!(report.active, Some(true));

    assert_eq!(
        fs::read_link(sb.path("/opt/awcc-rs/service/current")).expect("symlink"),
        std::path::Path::new("releases/0.1.2-aaaa")
    );
    let release = sb.path("/opt/awcc-rs/service/releases/0.1.2-aaaa");
    for file in [
        "bin/awcc-cli",
        "lib/libusb-1.0.so.0",
        "share/awcc/database.json",
        "MANIFEST.sha256",
    ] {
        assert!(release.join(file).is_file(), "{file}");
    }
    let mode = |p: &str| {
        fs::metadata(release.join(p))
            .expect("meta")
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("bin/awcc-cli"), 0o755);
    assert_eq!(mode("share/awcc/database.json"), 0o644);
    assert_eq!(
        fs::metadata(&release).expect("dir").permissions().mode() & 0o7777,
        0o755
    );

    let unit = fs::read_to_string(sb.path("/etc/systemd/system/awccd.service")).expect("unit");
    assert_eq!(unit, render_unit(SERVICE_ROOT, true));
    assert!(unit.contains("ExecStart=/opt/awcc-rs/service/current/bin/awcc-cli -d"));
    assert!(unit.contains("Environment=LD_LIBRARY_PATH=/opt/awcc-rs/service/current/lib"));
    assert!(!unit.contains("AWCC_DATABASE_PATH"));

    let rec = record(&sb);
    assert_eq!(rec.build_id, "0.1.2-aaaa");
    assert!(
        rec.installed_files
            .iter()
            .any(|f| f.ends_with("bin/awcc-cli"))
    );
    assert!(!sb.path("/var/lib/awcc/install-journal.json").exists());
    assert_eq!(
        sb.host.verbs(),
        [
            "daemon-reload",
            "enable awccd.service",
            "start awccd.service"
        ]
    );
    assert!(steps.contains(&ProgressStep::Staging));
    assert!(steps.contains(&ProgressStep::HealthCheck));
    assert_eq!(steps.last(), Some(&ProgressStep::Done));

    let status = inspect(&sb.paths, &sb.host);
    assert_eq!(status.managed_by, "appimage-installer");
    assert_eq!(status.unit_state, UnitState::Running);
    assert_eq!(status.enabled, Some(true));
    assert_eq!(status.installed_build_id.as_deref(), Some("0.1.2-aaaa"));
}

#[test]
fn unit_without_private_libs_has_no_library_path() {
    let sb = Sandbox::new("nolib");
    let payload = sb.payload("0.1.2-nolib", false);
    sb.install(&payload).expect("install");
    let unit = fs::read_to_string(sb.path("/etc/systemd/system/awccd.service")).expect("unit");
    assert!(!unit.contains("LD_LIBRARY_PATH"));
    assert_eq!(unit, render_unit(SERVICE_ROOT, false));
}

#[test]
fn unit_text_matches_golden_snapshot() {
    let golden = include_str!("golden/awccd-appimage.service");
    assert_eq!(render_unit(SERVICE_ROOT, true), golden);
}

#[test]
fn same_version_reinstall_is_idempotent() {
    let sb = Sandbox::new("idem");
    let payload = sb.payload("0.1.2-aaaa", true);
    sb.install(&payload).expect("install");
    sb.host.clear_calls();
    let tree_before = sb.tree();
    let again = sb.install(&payload).expect("reinstall");
    assert_eq!(again.outcome, ApplyOutcome::Unchanged);
    assert!(
        sb.host.verbs().is_empty(),
        "no stop/start/reload: {:?}",
        sb.host.verbs()
    );
    assert_eq!(sb.tree(), tree_before);
}

#[test]
fn update_switches_release_and_preserves_enabled_and_running() {
    let sb = Sandbox::new("update");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");
    sb.host.clear_calls();
    let report = sb
        .run(
            ServiceAction::Update,
            Some(&sb.payload("0.1.3-bbbb", true)),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect("update");
    assert_eq!(report.outcome, ApplyOutcome::Updated);
    assert_eq!(report.previous_build_id.as_deref(), Some("0.1.2-aaaa"));
    assert_eq!(
        sb.host.verbs(),
        ["stop awccd.service", "daemon-reload", "start awccd.service"]
    );
    assert_eq!(
        fs::read_link(sb.path("/opt/awcc-rs/service/current")).expect("link"),
        std::path::Path::new("releases/0.1.3-bbbb")
    );
    assert!(!sb.path("/opt/awcc-rs/service/releases/0.1.2-aaaa").exists());
    assert_eq!(record(&sb).build_id, "0.1.3-bbbb");
    assert_eq!(report.enabled, Some(true));
    assert_eq!(report.active, Some(true));
}

#[test]
fn update_keeps_a_disabled_stopped_service_disabled_and_stopped() {
    let sb = Sandbox::new("update-off");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");
    sb.host.with(|s| {
        s.enabled = Some(false);
        s.active = false;
    });
    sb.host.clear_calls();
    let report = sb
        .run(
            ServiceAction::Update,
            Some(&sb.payload("0.1.3-bbbb", true)),
            // A failing probe must not matter: the service is intentionally not started.
            &ProbeFails("never asked"),
            Sandbox::options(),
        )
        .expect("update");
    assert_eq!(report.outcome, ApplyOutcome::Updated);
    assert_eq!(sb.host.verbs(), ["daemon-reload"]);
    assert_eq!(report.enabled, Some(false));
    assert_eq!(report.active, Some(false));
}

#[test]
fn update_requires_an_existing_installation() {
    let sb = Sandbox::new("update-none");
    let error = sb
        .run(
            ServiceAction::Update,
            Some(&sb.payload("0.1.3-bbbb", true)),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect_err("not installed");
    assert!(matches!(error, InstallError::NotInstalled), "{error}");
}

#[test]
fn failed_health_check_rolls_back_update_completely() {
    let sb = Sandbox::new("rollback");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");
    let unit_before = fs::read(sb.path("/etc/systemd/system/awccd.service")).expect("unit");
    let record_before = fs::read(sb.path("/var/lib/awcc/install-owner.json")).expect("record");
    sb.host.clear_calls();

    let error = sb
        .run(
            ServiceAction::Update,
            Some(&sb.payload("0.1.3-bbbb", false)),
            &ProbeFails("protocol mismatch"),
            Sandbox::options(),
        )
        .expect_err("health failure");
    match &error {
        InstallError::HealthCheckFailed {
            reason, rollback, ..
        } => {
            assert!(reason.contains("protocol mismatch"), "{reason}");
            assert!(rollback.restored, "{rollback:?}");
        }
        other => panic!("unexpected {other}"),
    }
    assert!(error.to_string().contains("previous state was restored"));
    assert_eq!(
        fs::read_link(sb.path("/opt/awcc-rs/service/current")).expect("link"),
        std::path::Path::new("releases/0.1.2-aaaa")
    );
    assert_eq!(
        fs::read(sb.path("/etc/systemd/system/awccd.service")).expect("unit"),
        unit_before
    );
    assert_eq!(
        fs::read(sb.path("/var/lib/awcc/install-owner.json")).expect("rec"),
        record_before
    );
    assert!(!sb.path("/opt/awcc-rs/service/releases/0.1.3-bbbb").exists());
    assert!(
        sb.path("/opt/awcc-rs/service/releases/0.1.2-aaaa/bin/awcc-cli")
            .is_file()
    );
    assert!(!sb.path("/var/lib/awcc/install-journal.json").exists());
    sb.host.with(|s| {
        assert_eq!(s.enabled, Some(true));
        assert!(s.active, "previous running state restored");
    });
}

#[test]
fn failed_fresh_install_leaves_no_trace_and_disables() {
    let sb = Sandbox::new("fresh-fail");
    sb.host.with(|s| s.start_crashes = true);
    let error = sb
        .run(
            ServiceAction::Install,
            Some(&sb.payload("0.1.2-aaaa", true)),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect_err("crash");
    assert!(
        matches!(error, InstallError::HealthCheckFailed { .. }),
        "{error}"
    );
    assert!(!sb.path("/etc/systemd/system/awccd.service").exists());
    assert!(!sb.path("/opt/awcc-rs/service/current").exists());
    assert!(!sb.path("/opt/awcc-rs/service/releases/0.1.2-aaaa").exists());
    assert!(!sb.path("/var/lib/awcc/install-owner.json").exists());
    sb.host.with(|s| {
        assert_ne!(s.enabled, Some(true), "enablement undone");
        assert!(!s.active || s.start_crashes);
    });
    assert!(
        sb.host
            .verbs()
            .contains(&"disable awccd.service".to_owned())
    );
}

#[test]
fn start_failure_rolls_back_a_fresh_install() {
    let sb = Sandbox::new("start-fail");
    sb.host.with(|s| {
        s.failing_verbs.insert("start".to_owned());
    });
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("start fails");
    match error {
        InstallError::StepFailed { step, rollback, .. } => {
            assert!(step.contains("starting"));
            assert!(rollback.restored);
        }
        other => panic!("unexpected {other}"),
    }
    assert!(!sb.path("/etc/systemd/system/awccd.service").exists());
}

#[test]
fn interrupted_update_is_recovered_from_the_journal_on_the_next_run() {
    let sb = Sandbox::new("journal");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");

    // The update fails while reloading systemd AND the rollback's own reload fails: the journal
    // must survive so a later run can finish the recovery.
    sb.host.with(|s| {
        s.failing_verbs.insert("daemon-reload".to_owned());
    });
    let error = sb
        .run(
            ServiceAction::Update,
            Some(&sb.payload("0.1.3-bbbb", true)),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect_err("fails with incomplete rollback");
    match &error {
        InstallError::StepFailed { rollback, .. } => assert!(!rollback.restored, "{rollback:?}"),
        other => panic!("unexpected {other}"),
    }
    assert!(sb.path("/var/lib/awcc/install-journal.json").exists());
    let status = inspect(&sb.paths, &sb.host);
    assert!(status.interrupted_operation);

    // Fault cleared: the next run recovers (rolls back) first, then performs the update.
    sb.host.with(|s| s.failing_verbs.clear());
    let report = sb
        .run(
            ServiceAction::Update,
            Some(&sb.payload("0.1.3-bbbb", true)),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect("recovered update");
    assert!(report.recovered_interrupted);
    assert_eq!(report.outcome, ApplyOutcome::Updated);
    assert_eq!(report.build_id.as_deref(), Some("0.1.3-bbbb"));
    assert!(!sb.path("/var/lib/awcc/install-journal.json").exists());
}

#[test]
fn recovery_alone_restores_the_previous_release() {
    let sb = Sandbox::new("journal-restore");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");
    sb.host.with(|s| {
        s.failing_verbs.insert("daemon-reload".to_owned());
    });
    let _ = sb
        .run(
            ServiceAction::Update,
            Some(&sb.payload("0.1.3-bbbb", true)),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect_err("fails");
    sb.host.with(|s| s.failing_verbs.clear());
    // Re-running the *same old* build: recovery rolls back, then the old build is Unchanged.
    let report = sb
        .run(
            ServiceAction::Install,
            Some(&sb.payload("0.1.2-aaaa", true)),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect("recover");
    assert!(report.recovered_interrupted);
    assert_eq!(report.outcome, ApplyOutcome::Unchanged);
    assert_eq!(
        fs::read_link(sb.path("/opt/awcc-rs/service/current")).expect("link"),
        std::path::Path::new("releases/0.1.2-aaaa")
    );
}

#[test]
fn masked_unit_is_never_unmasked() {
    let sb = Sandbox::new("masked");
    fs::create_dir_all(sb.path("/etc/systemd/system")).expect("mkdir");
    std::os::unix::fs::symlink("/dev/null", sb.path("/etc/systemd/system/awccd.service"))
        .expect("mask");
    sb.host.with(|s| s.masked = true);
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("masked");
    assert!(matches!(error, InstallError::Masked { .. }), "{error}");
    assert!(error.to_string().contains("never unmasks"));
    assert_eq!(
        fs::read_link(sb.path("/etc/systemd/system/awccd.service")).expect("still a mask"),
        std::path::Path::new("/dev/null")
    );
    assert!(sb.host.verbs().iter().all(|v| !v.contains("unmask")));
    assert!(
        sb.host.verbs().is_empty(),
        "nothing mutated: {:?}",
        sb.host.verbs()
    );
    assert!(!sb.path("/opt").exists());
    let status = inspect(&sb.paths, &sb.host);
    assert!(status.masked);
    assert_eq!(status.unit_state, UnitState::Masked);
}

#[test]
fn admin_dropins_are_preserved_across_install_update_and_uninstall() {
    let sb = Sandbox::new("dropin");
    let dropin_dir = sb.path("/etc/systemd/system/awccd.service.d");
    fs::create_dir_all(&dropin_dir).expect("mkdir");
    fs::write(dropin_dir.join("local.conf"), "[Service]\nNice=5\n").expect("dropin");

    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");
    assert_eq!(inspect(&sb.paths, &sb.host).dropins, ["local.conf"]);
    sb.run(
        ServiceAction::Update,
        Some(&sb.payload("0.1.3-bbbb", true)),
        &ProbeOk,
        Sandbox::options(),
    )
    .expect("update");
    assert_eq!(
        fs::read_to_string(dropin_dir.join("local.conf")).expect("kept"),
        "[Service]\nNice=5\n"
    );
    let report = sb
        .run(
            ServiceAction::Uninstall { purge: true },
            None,
            &ProbeOk,
            Sandbox::options(),
        )
        .expect("uninstall");
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("kept your drop-ins"))
    );
    assert_eq!(
        fs::read_to_string(dropin_dir.join("local.conf")).expect("kept"),
        "[Service]\nNice=5\n"
    );
}

#[test]
fn uninstall_keeps_settings_and_purge_removes_state() {
    let sb = Sandbox::new("uninstall");
    let mut options = Sandbox::options();
    options.authorized_users_seed = Some("1000".to_owned());
    sb.run(
        ServiceAction::Install,
        Some(&sb.payload("0.1.2-aaaa", true)),
        &ProbeOk,
        options,
    )
    .expect("install");
    assert_eq!(
        fs::read_to_string(sb.path("/etc/awcc/authorized-users")).expect("seed"),
        "1000\n"
    );
    assert!(record(&sb).created_authorized_users);
    fs::write(sb.path("/var/lib/awcc/settings.json"), "{\"x\":1}").expect("settings");

    sb.host.clear_calls();
    let report = sb
        .run(
            ServiceAction::Uninstall { purge: false },
            None,
            &ProbeOk,
            Sandbox::options(),
        )
        .expect("uninstall");
    assert_eq!(report.outcome, ApplyOutcome::Uninstalled);
    assert_eq!(
        sb.host.verbs(),
        [
            "stop awccd.service",
            "disable awccd.service",
            "daemon-reload"
        ]
    );
    assert!(!sb.path("/etc/systemd/system/awccd.service").exists());
    assert!(!sb.path("/opt/awcc-rs/service/current").exists());
    assert!(
        !sb.path("/opt/awcc-rs").exists(),
        "empty install dirs are pruned"
    );
    assert!(!sb.path("/var/lib/awcc/install-owner.json").exists());
    assert!(
        sb.path("/var/lib/awcc/settings.json").exists(),
        "settings kept"
    );
    assert!(
        sb.path("/etc/awcc/authorized-users").exists(),
        "authorized users kept"
    );

    // Reinstall then purge.
    let mut options = Sandbox::options();
    options.authorized_users_seed = None;
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("reinstall");
    let report = sb
        .run(
            ServiceAction::Uninstall { purge: true },
            None,
            &ProbeOk,
            options,
        )
        .expect("purge");
    assert!(report.purged);
    assert!(!sb.path("/var/lib/awcc").exists(), "state purged");
    // The first install created authorized-users; the reinstall found it existing, but the
    // record from the first install is gone, so only a record-tracked creation is purged.
    assert!(sb.path("/etc/awcc/authorized-users").exists());
}

#[test]
fn purge_removes_authorized_users_created_by_the_installer() {
    let sb = Sandbox::new("purge-auth");
    let mut options = Sandbox::options();
    options.authorized_users_seed = Some("alice".to_owned());
    sb.run(
        ServiceAction::Install,
        Some(&sb.payload("0.1.2-aaaa", true)),
        &ProbeOk,
        options,
    )
    .expect("install");
    sb.run(
        ServiceAction::Uninstall { purge: true },
        None,
        &ProbeOk,
        Sandbox::options(),
    )
    .expect("purge");
    assert!(!sb.path("/etc/awcc").exists());
    assert!(!sb.path("/var/lib/awcc").exists());
}

#[test]
fn uninstall_never_removes_a_unit_the_installer_did_not_write() {
    let sb = Sandbox::new("uninstall-edited");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");
    // Admin edits the unit in place: it no longer matches the record, so it is not ours.
    fs::write(
        sb.path("/etc/systemd/system/awccd.service"),
        "[Service]\nExecStart=/bin/true\n",
    )
    .expect("edit");
    let error = sb
        .run(
            ServiceAction::Uninstall { purge: false },
            None,
            &ProbeOk,
            Sandbox::options(),
        )
        .expect_err("modified unit is not ours");
    assert!(matches!(error, InstallError::UnknownUnit { .. }), "{error}");
    assert!(sb.path("/etc/systemd/system/awccd.service").exists());
}

#[test]
fn uninstall_without_installation_is_a_noop() {
    let sb = Sandbox::new("uninstall-none");
    let report = sb
        .run(
            ServiceAction::Uninstall { purge: false },
            None,
            &ProbeOk,
            Sandbox::options(),
        )
        .expect("noop");
    assert_eq!(report.outcome, ApplyOutcome::NothingToUninstall);
    assert!(
        sb.tree().iter().all(|p| p.starts_with("run")),
        "{:?}",
        sb.tree()
    );
}

#[test]
fn concurrent_operation_is_rejected_by_the_lock() {
    let sb = Sandbox::new("lock");
    fs::create_dir_all(sb.path("/var/lib/awcc")).expect("mkdir");
    let held = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(sb.path("/var/lib/awcc/.install.lock"))
        .expect("lock file");
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).expect("lock");
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("locked");
    assert!(matches!(error, InstallError::LockHeld { .. }), "{error}");
    drop(held);
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("free again");
}

#[test]
fn missing_systemd_is_a_typed_error() {
    let sb = Sandbox::new("nosystemd");
    fs::remove_dir_all(sb.path("/run")).expect("rm");
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("no systemd");
    assert!(matches!(error, InstallError::UnsupportedInit), "{error}");
}

#[test]
fn symlinked_install_ancestor_is_refused() {
    let sb = Sandbox::new("symlink-ancestor");
    let elsewhere = sb.tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("mkdir");
    std::os::unix::fs::symlink(&elsewhere, sb.path("/opt")).expect("symlink");
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("unsafe");
    assert!(matches!(error, InstallError::UnsafePath { .. }), "{error}");
    assert_eq!(
        fs::read_dir(&elsewhere).expect("dir").count(),
        0,
        "nothing written through it"
    );
    assert!(!sb.path("/etc/systemd/system/awccd.service").exists());
}

#[test]
fn current_that_is_a_real_directory_is_refused() {
    let sb = Sandbox::new("current-dir");
    fs::create_dir_all(sb.path("/opt/awcc-rs/service/current")).expect("mkdir");
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("unsafe");
    assert!(matches!(error, InstallError::UnsafePath { .. }), "{error}");
}

#[test]
fn damaged_stale_release_directory_is_replaced() {
    let sb = Sandbox::new("stale");
    let stale = sb.path("/opt/awcc-rs/service/releases/0.1.2-aaaa");
    fs::create_dir_all(stale.join("bin")).expect("mkdir");
    fs::write(stale.join("bin/awcc-cli"), "tampered").expect("tamper");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install over stale");
    let content = fs::read_to_string(stale.join("bin/awcc-cli")).expect("binary");
    assert!(content.contains("awcc-cli 0.1.2-aaaa"));
}

#[test]
fn every_host_command_runs_with_the_clean_environment() {
    let sb = Sandbox::new("env");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("install");
    sb.run(
        ServiceAction::Uninstall { purge: false },
        None,
        &ProbeOk,
        Sandbox::options(),
    )
    .expect("uninstall");
    let calls = sb.host.with(|s| s.calls.clone());
    assert!(!calls.is_empty());
    for (tool, args, env) in calls {
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH", "LC_ALL"], "{tool:?} {args:?}");
        assert_eq!(env[0].1, "/usr/sbin:/usr/bin:/sbin:/bin");
        assert!(
            args.iter()
                .all(|a| !a.contains("LD_") && !a.contains("APPDIR"))
        );
    }
}

#[test]
fn missing_payload_source_is_an_error_not_a_panic() {
    let sb = Sandbox::new("nopayload");
    let error = sb
        .run(ServiceAction::Install, None, &ProbeOk, Sandbox::options())
        .expect_err("no payload");
    assert!(matches!(error, InstallError::Payload(_)), "{error}");
}
