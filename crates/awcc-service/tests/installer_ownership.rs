//! Ownership detection: native packages and unknown units are never overwritten.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod installer_common;

use std::fs;

use awcc_service::installer::{HostTool, InstallError, Ownership, ServiceAction, inspect};
use installer_common::{ProbeOk, Sandbox};

fn vendor_unit(sb: &Sandbox) {
    let dir = sb.path("/usr/lib/systemd/system");
    fs::create_dir_all(&dir).expect("mkdir");
    fs::write(
        dir.join("awccd.service"),
        "[Service]\nExecStart=/usr/bin/awcc-cli -d\n",
    )
    .expect("vendor unit");
}

fn assert_untouched(sb: &Sandbox) {
    assert!(!sb.path("/etc/systemd/system/awccd.service").exists());
    assert!(!sb.path("/opt").exists());
    assert!(!sb.path("/var/lib/awcc/install-owner.json").exists());
    assert!(
        sb.host.verbs().is_empty(),
        "no mutation: {:?}",
        sb.host.verbs()
    );
}

#[test]
fn rpm_owned_unit_is_refused_with_an_actionable_error() {
    let sb = Sandbox::new("rpm");
    vendor_unit(&sb);
    sb.host
        .with(|s| s.native_owner = Some((HostTool::Rpm, "awcc-rs".to_owned())));
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("native");
    match &error {
        InstallError::NativePackage {
            manager,
            package,
            unit_path,
        } => {
            assert_eq!(manager, "rpm");
            assert_eq!(package, "awcc-rs");
            assert_eq!(unit_path, "/usr/lib/systemd/system/awccd.service");
        }
        other => panic!("unexpected {other}"),
    }
    assert!(error.to_string().contains("will not shadow it"));
    assert_untouched(&sb);
}

#[test]
fn dpkg_and_pacman_owners_are_detected() {
    for (tool, name) in [(HostTool::Dpkg, "dpkg"), (HostTool::Pacman, "pacman")] {
        let sb = Sandbox::new(name);
        vendor_unit(&sb);
        sb.host.with(|s| {
            s.native_owner = Some((tool, "awcc-rs".to_owned()));
            s.missing_tools.insert("rpm");
        });
        let status = inspect(&sb.paths, &sb.host);
        assert_eq!(status.managed_by, "native-package");
        assert!(
            matches!(&status.ownership, Ownership::NativePackage { manager, package, .. }
                if manager == name && package == "awcc-rs"),
            "{:?}",
            status.ownership
        );
    }
}

#[test]
fn update_and_uninstall_also_refuse_native_packages() {
    let sb = Sandbox::new("native-all");
    vendor_unit(&sb);
    sb.host
        .with(|s| s.native_owner = Some((HostTool::Rpm, "awcc-rs".to_owned())));
    let payload = sb.payload("0.1.2-aaaa", true);
    for action in [
        ServiceAction::Update,
        ServiceAction::Uninstall { purge: true },
    ] {
        let error = sb
            .run(action, Some(&payload), &ProbeOk, Sandbox::options())
            .expect_err("native");
        assert!(
            matches!(error, InstallError::NativePackage { .. }),
            "{error}"
        );
    }
    assert_untouched(&sb);
}

#[test]
fn installer_record_does_not_override_a_new_native_package() {
    for keep_installer_unit in [true, false] {
        let sb = Sandbox::new("native-after-appimage");
        sb.install(&sb.payload("0.1.2-aaaa", true))
            .expect("initial install");
        if !keep_installer_unit {
            fs::remove_file(sb.paths.unit_path()).expect("remove old installer unit");
        }
        vendor_unit(&sb);
        sb.host
            .with(|s| s.native_owner = Some((HostTool::Rpm, "awcc-rs".to_owned())));
        let before = sb.tree();
        let owner_before = fs::read(sb.paths.record_path()).expect("owner record");
        let payload = sb.payload("0.1.3-bbbb", true);
        for action in [
            ServiceAction::Install,
            ServiceAction::Update,
            ServiceAction::Uninstall { purge: true },
        ] {
            sb.host.clear_calls();
            let error = sb
                .run(action, Some(&payload), &ProbeOk, Sandbox::options())
                .expect_err("native package must retain ownership");
            assert!(
                matches!(error, InstallError::NativePackage { .. }),
                "{error}"
            );
            assert!(sb.host.verbs().is_empty(), "{:?}", sb.host.verbs());
            assert_eq!(sb.tree(), before);
            assert_eq!(
                fs::read(sb.paths.record_path()).expect("record"),
                owner_before
            );
            sb.host.with(|s| {
                assert!(s.active, "do not stop the native service");
                assert_eq!(s.enabled, Some(true));
            });
        }
    }
}

#[test]
fn stale_installer_record_does_not_authorize_changes_to_a_manual_unit() {
    let sb = Sandbox::new("manual-after-appimage");
    sb.install(&sb.payload("0.1.2-aaaa", true))
        .expect("initial install");
    fs::remove_file(sb.paths.unit_path()).expect("remove old installer unit");
    vendor_unit(&sb);
    sb.host.clear_calls();
    let before = sb.tree();
    let error = sb
        .run(
            ServiceAction::Uninstall { purge: true },
            None,
            &ProbeOk,
            Sandbox::options(),
        )
        .expect_err("manual service must be left untouched");
    assert!(matches!(error, InstallError::UnknownUnit { .. }), "{error}");
    assert!(sb.host.verbs().is_empty());
    assert_eq!(sb.tree(), before);
}

#[test]
fn unowned_vendor_path_unit_is_unknown_and_refused() {
    let sb = Sandbox::new("unknown-vendor");
    vendor_unit(&sb);
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("unknown");
    assert!(matches!(error, InstallError::UnknownUnit { .. }), "{error}");
    assert_untouched(&sb);
}

#[test]
fn manual_unit_in_etc_without_record_is_refused_and_untouched() {
    let sb = Sandbox::new("manual");
    fs::create_dir_all(sb.path("/etc/systemd/system")).expect("mkdir");
    let manual = "[Service]\nExecStart=/home/me/awcc -d\n";
    fs::write(sb.path("/etc/systemd/system/awccd.service"), manual).expect("manual");
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("manual");
    assert!(matches!(error, InstallError::UnknownUnit { .. }), "{error}");
    assert!(error.to_string().contains("did not create"));
    assert_eq!(
        fs::read_to_string(sb.path("/etc/systemd/system/awccd.service")).expect("kept"),
        manual
    );
    assert_eq!(inspect(&sb.paths, &sb.host).managed_by, "manual");
}

#[test]
fn fragment_path_elsewhere_is_treated_as_foreign() {
    let sb = Sandbox::new("fragment");
    sb.host.with(|s| {
        s.fragment_override = Some("/usr/local/lib/systemd/system/awccd.service".to_owned())
    });
    let error = sb
        .install(&sb.payload("0.1.2-aaaa", true))
        .expect_err("foreign fragment");
    assert!(matches!(error, InstallError::UnknownUnit { .. }), "{error}");
}

#[test]
fn passive_inspect_never_writes_or_mutates() {
    let sb = Sandbox::new("passive");
    let before = sb.tree();
    let status = inspect(&sb.paths, &sb.host);
    assert_eq!(status.ownership, Ownership::None);
    assert_eq!(status.managed_by, "none");
    assert_eq!(sb.tree(), before);
    assert!(sb.host.verbs().is_empty());
    let json = serde_json::to_string(&status).expect("serialize");
    assert!(json.contains("\"managed_by\":\"none\""));
}
