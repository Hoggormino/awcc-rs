//! Hostile and malformed payloads are rejected before anything is installed.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod installer_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use awcc_diagnostics::hash::sha256_hex;
use awcc_service::installer::payload::{parse_manifest, verify_payload};
use awcc_service::installer::{InstallError, PayloadError, PayloadSource, ServiceAction};
use installer_common::{ProbeOk, Sandbox, TempDir, standard_payload};

fn manifest_line(path: &str, bytes: &[u8]) -> String {
    format!("{}  {path}\n", sha256_hex(bytes))
}

fn base_manifest() -> String {
    format!(
        "build-id: x1\nversion: 0.1.2\n{}{}",
        manifest_line("bin/awcc-cli", b"bin"),
        manifest_line("share/awcc/database.json", b"{}")
    )
}

fn expect_payload_error(sb: &Sandbox, dir: &Path) -> PayloadError {
    let error = sb
        .run(
            ServiceAction::Install,
            Some(dir),
            &ProbeOk,
            Sandbox::options(),
        )
        .expect_err("hostile payload must be rejected");
    // Rejected before any state change.
    assert!(!sb.path("/etc/systemd/system/awccd.service").exists());
    assert!(!sb.path("/opt/awcc-rs/service/current").exists());
    assert!(sb.host.verbs().is_empty(), "{:?}", sb.host.verbs());
    match error {
        InstallError::Payload(inner) => inner,
        other => panic!("expected a payload error, got {other}"),
    }
}

fn fresh(sb: &Sandbox, name: &str) -> std::path::PathBuf {
    let dir = sb.tmp.path().join(name);
    standard_payload(&dir, "x1", false);
    dir
}

#[test]
fn good_payload_verifies() {
    let tmp = TempDir::new("good");
    standard_payload(&tmp.path().join("p"), "x1", true);
    let verified = verify_payload(&PayloadSource::new(tmp.path().join("p")), false).expect("ok");
    assert_eq!(verified.build_id, "x1");
    assert_eq!(verified.files.len(), 3);
    assert!(
        verified
            .files
            .iter()
            .any(|f| f.path == "bin/awcc-cli" && f.executable)
    );
}

#[test]
fn manifest_with_path_traversal_is_rejected() {
    let text = format!(
        "{}{}",
        base_manifest(),
        manifest_line("bin/../../etc/passwd", b"x")
    );
    assert!(matches!(
        parse_manifest(&text),
        Err(PayloadError::Unsafe { .. })
    ));
}

#[test]
fn manifest_with_absolute_path_is_rejected() {
    let text = format!("{}{}", base_manifest(), manifest_line("/etc/passwd", b"x"));
    assert!(matches!(
        parse_manifest(&text),
        Err(PayloadError::Unsafe { .. })
    ));
}

#[test]
fn manifest_entries_outside_the_allowed_roots_or_odd_names_are_rejected() {
    for bad in [
        "etc/passwd",
        "bin//x",
        "bin/x/",
        "bin\\x",
        "MANIFEST.sha256",
        "./bin/x",
    ] {
        let text = format!("{}{}", base_manifest(), manifest_line(bad, b"x"));
        assert!(parse_manifest(&text).is_err(), "{bad}");
    }
}

#[test]
fn manifest_needs_build_id_version_and_required_files() {
    assert!(parse_manifest("version: 1\n").is_err());
    assert!(parse_manifest("build-id: a\n").is_err());
    assert!(parse_manifest("build-id: a\nversion: 1\n").is_err());
    assert!(parse_manifest("build-id: ../x\nversion: 1\n").is_err());
    let dup = format!(
        "{}{}",
        base_manifest(),
        manifest_line("bin/awcc-cli", b"bin")
    );
    assert!(parse_manifest(&dup).is_err());
    assert!(parse_manifest(&format!("{}garbage line\n", base_manifest())).is_err());
}

#[test]
fn installing_a_traversal_manifest_changes_nothing() {
    let sb = Sandbox::new("traversal");
    let dir = fresh(&sb, "p");
    let mut text = fs::read_to_string(dir.join("MANIFEST.sha256")).expect("manifest");
    text.push_str(&manifest_line("bin/../../escape", b"x"));
    fs::write(dir.join("MANIFEST.sha256"), text).expect("write");
    assert!(matches!(
        expect_payload_error(&sb, &dir),
        PayloadError::Unsafe { .. }
    ));
}

#[test]
fn symlink_in_payload_is_rejected() {
    let sb = Sandbox::new("symlink");
    let dir = fresh(&sb, "p");
    std::os::unix::fs::symlink("/etc/passwd", dir.join("lib-evil")).expect("symlink");
    assert!(matches!(
        expect_payload_error(&sb, &dir),
        PayloadError::Unsafe { .. }
    ));
}

#[test]
fn listed_file_that_is_a_symlink_is_rejected() {
    let sb = Sandbox::new("symlink-listed");
    let dir = fresh(&sb, "p");
    fs::remove_file(dir.join("share/awcc/database.json")).expect("rm");
    std::os::unix::fs::symlink("/etc/hostname", dir.join("share/awcc/database.json"))
        .expect("link");
    assert!(matches!(
        expect_payload_error(&sb, &dir),
        PayloadError::Unsafe { .. }
    ));
}

#[test]
fn symlinked_parent_directory_is_rejected() {
    let sb = Sandbox::new("symlink-parent");
    let dir = fresh(&sb, "p");
    let outside = sb.tmp.path().join("outside");
    fs::create_dir_all(outside.join("awcc")).expect("mkdir");
    fs::write(outside.join("awcc/database.json"), "{}").expect("write");
    fs::remove_dir_all(dir.join("share")).expect("rm");
    std::os::unix::fs::symlink(&outside, dir.join("share")).expect("link");
    assert!(matches!(
        expect_payload_error(&sb, &dir),
        PayloadError::Unsafe { .. }
    ));
}

#[test]
fn setuid_file_is_rejected() {
    let sb = Sandbox::new("setuid");
    let dir = fresh(&sb, "p");
    fs::set_permissions(dir.join("bin/awcc-cli"), fs::Permissions::from_mode(0o4755))
        .expect("chmod");
    assert!(matches!(
        expect_payload_error(&sb, &dir),
        PayloadError::Unsafe { .. }
    ));
}

#[test]
fn world_or_group_writable_file_is_rejected() {
    for mode in [0o757, 0o775] {
        let sb = Sandbox::new("writable");
        let dir = fresh(&sb, "p");
        fs::set_permissions(dir.join("bin/awcc-cli"), fs::Permissions::from_mode(mode))
            .expect("chmod");
        assert!(
            matches!(expect_payload_error(&sb, &dir), PayloadError::Unsafe { .. }),
            "{mode:o}"
        );
    }
}

#[test]
fn world_writable_directory_is_rejected() {
    let sb = Sandbox::new("dir-writable");
    let dir = fresh(&sb, "p");
    fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o777)).expect("chmod");
    assert!(matches!(
        expect_payload_error(&sb, &dir),
        PayloadError::Unsafe { .. }
    ));
}

#[test]
fn special_file_is_rejected() {
    let sb = Sandbox::new("fifo");
    let dir = fresh(&sb, "p");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        dir.join("share/awcc/pipe"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .expect("mkfifo");
    assert!(matches!(
        expect_payload_error(&sb, &dir),
        PayloadError::Unsafe { .. }
    ));
}

#[test]
fn hash_mismatch_is_rejected() {
    let sb = Sandbox::new("hash");
    let dir = fresh(&sb, "p");
    fs::write(dir.join("bin/awcc-cli"), "tampered after manifest").expect("tamper");
    assert!(
        matches!(expect_payload_error(&sb, &dir), PayloadError::HashMismatch(p) if p == "bin/awcc-cli")
    );
}

#[test]
fn unlisted_extra_file_is_rejected() {
    let sb = Sandbox::new("extra");
    let dir = fresh(&sb, "p");
    fs::write(dir.join("share/awcc/extra"), "x").expect("extra");
    assert!(
        matches!(expect_payload_error(&sb, &dir), PayloadError::Unlisted(p) if p == "share/awcc/extra")
    );
}

#[test]
fn missing_listed_file_is_rejected() {
    let sb = Sandbox::new("missing");
    let dir = fresh(&sb, "p");
    fs::remove_file(dir.join("share/awcc/database.json")).expect("rm");
    assert!(
        expect_payload_error(&sb, &dir)
            .to_string()
            .contains("database.json")
    );
}

#[test]
fn missing_payload_directory_explains_native_packages() {
    let sb = Sandbox::new("nodir");
    let missing = sb.tmp.path().join("does-not-exist");
    let error = expect_payload_error(&sb, &missing);
    assert!(matches!(error, PayloadError::Missing(_)));
    assert!(error.to_string().contains("native packages"));
}

#[test]
fn payload_is_resolved_next_to_the_executable_and_ignores_appdir() {
    let tmp = TempDir::new("resolve");
    let share = tmp.path().join("share/awcc/service-payload");
    standard_payload(&share, "x1", false);
    fs::create_dir_all(tmp.path().join("bin")).expect("mkdir");
    let exe = tmp.path().join("bin/awcc-cli");
    fs::write(&exe, "x").expect("exe");
    let source = PayloadSource::for_executable(&exe).expect("resolved");
    assert_eq!(source.dir(), fs::canonicalize(&share).expect("canonical"));
    let other = tmp.path().join("elsewhere/bin/awcc-cli");
    assert!(PayloadSource::for_executable(&other).is_err());
}
