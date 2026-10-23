//! Reversible, write-ahead provider handover. Package and AppImage paths use the same journal.
use crate::installer::{
    HostCommand, HostRunner, HostTool, InstallPaths, ensure_dir_chain, write_atomic,
};
use awcc_diagnostics::hash::sha256_hex;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
const RECORD: &str = "/var/lib/awcc/provider-handover.json";
pub(crate) const UNITS: &[&str] = &[
    "power-profiles-daemon.service",
    "tuned-ppd.service",
    "tuned.service",
    "tlp.service",
    "tlp-pd.service",
    "auto-cpufreq.service",
    "system76-power.service",
];
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Unit {
    name: String,
    active: bool,
    enabled: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    path: String,
    hash: String,
}
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Record {
    #[serde(default)]
    ready: bool,
    units: Vec<Unit>,
    files: Vec<File>,
}
fn command(runner: &dyn HostRunner, tool: HostTool, args: &[&str]) -> Result<String, String> {
    let started = Instant::now();
    tracing::debug!(target:"awcc_service::provider_install", code="provider_command_started", tool=?tool, command=?args, "running fixed provider handover command");
    let output = runner
        .run(
            &HostCommand::new(tool, args.iter().copied()).with_timeout(Duration::from_secs(15)),
            None,
        )
        .map_err(|e| { tracing::error!(target:"awcc_service::provider_install", code="provider_command_failed", tool=?tool, command=?args, error=%e, elapsed_ms=started.elapsed().as_millis() as u64, "provider handover command could not complete"); e.to_string() })?;
    if output.code != Some(0) {
        tracing::error!(target:"awcc_service::provider_install", code="provider_command_failed", tool=?tool, command=?args, exit_code=?output.code, error=%output.stderr, "provider handover command returned failure");
        return Err(format!("{}: {}", args.join(" "), output.stderr));
    }
    if args.first() != Some(&"show") {
        tracing::info!(target:"awcc_service::provider_install", code="provider_command_completed", tool=?tool, command=?args, elapsed_ms=started.elapsed().as_millis() as u64, "provider handover action completed");
    }
    Ok(output.stdout)
}
fn save(paths: &InstallPaths, record: &Record) -> Result<(), String> {
    ensure_dir_chain(paths, "/var/lib/awcc", 0o755).map_err(|e| e.to_string())?;
    write_atomic(
        &paths.join(RECORD),
        &serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?,
        0o600,
    )
    .map_err(|e| e.to_string())
}
fn load(paths: &InstallPaths) -> Result<Option<Record>, String> {
    let path = paths.join(RECORD);
    if !path.exists() {
        return Ok(None);
    }
    crate::installer::check_existing_dir_chain(paths, "/var/lib/awcc")
        .map_err(|e| e.to_string())?;
    let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    use std::os::unix::fs::MetadataExt;
    if !meta.is_file()
        || meta.mode() & 0o022 != 0
        || (paths.is_privileged() && meta.uid() != 0)
        || meta.len() > 65536
    {
        return Err("unsafe provider handover journal".into());
    }
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map(Some)
        .map_err(|e| e.to_string())
}
/// Installation is authorized by the existing elevated setup flow. Refuses administrator
/// activation overrides instead of silently replacing them. The original executable stays installed.
pub fn install(
    paths: &InstallPaths,
    runner: &dyn HostRunner,
    executable: &str,
) -> Result<(), String> {
    tracing::info!(target:"awcc_service::provider_install", code="provider_handover_started", "inspecting provider ownership before takeover");
    ensure_dir_chain(paths, "/etc/systemd/system", 0o755).map_err(|e| e.to_string())?;
    if let Some(record) = load(paths)? {
        if record.ready {
            for file in &record.files {
                if !fs::symlink_metadata(paths.join(&file.path)).is_ok_and(|m| m.is_file())
                    || !fs::read(paths.join(&file.path)).is_ok_and(|b| sha256_hex(&b) == file.hash)
                {
                    return Err(format!(
                        "provider registration changed: {}; preserve and review the administrator change",
                        file.path
                    ));
                }
            }
            for unit in &record.units {
                if !fs::read_link(paths.join(&format!("/etc/systemd/system/{}", unit.name)))
                    .is_ok_and(|p| p == Path::new("/dev/null"))
                {
                    return Err(format!(
                        "provider mask changed: {}; explicit handover review required",
                        unit.name
                    ));
                }
            }
            tracing::info!(target:"awcc_service::provider_install", code="provider_handover_unchanged", "recorded provider handover remains valid");
            return Ok(());
        }
        tracing::warn!(target:"awcc_service::provider_install", code="provider_handover_interrupted", "recovering interrupted provider handover before retry");
        remove(paths, runner)?;
    }
    if !executable.starts_with('/') || executable.contains('\n') {
        return Err("invalid service executable".into());
    }
    let mut record = Record::default();
    let mut files = vec![
        (
            "/etc/dbus-1/system.d/awcc-power-profiles.conf".to_owned(),
            include_str!("../../../packaging/linux/power-profiles/awcc-power-profiles.conf")
                .to_owned(),
        ),
        (
            "/usr/share/polkit-1/actions/awcc-power-profiles.policy".to_owned(),
            include_str!("../../../packaging/linux/power-profiles/awcc-power-profiles.policy")
                .to_owned(),
        ),
    ];
    for name in [crate::power_bus::MODERN.0, crate::power_bus::COMPAT.0] {
        files.push((format!("/usr/local/share/dbus-1/system-services/{name}.service"),format!("[D-BUS Service]\nName={name}\nExec={executable} -d\nUser=root\nSystemdService=awccd.service\n")));
    }
    files.retain(|(path, body)| {
        !fs::symlink_metadata(paths.join(path)).is_ok_and(|m| {
            m.is_file()
                && m.mode() & 0o022 == 0
                && (!paths.is_privileged() || m.uid() == 0)
                && fs::read(paths.join(path)).is_ok_and(|b| b == body.as_bytes())
        })
    });
    for (path, body) in &files {
        if fs::symlink_metadata(paths.join(path)).is_ok() {
            return Err(format!(
                "administrator provider registration already exists: {path}"
            ));
        }
        let parent = Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .ok_or("invalid provider path")?;
        ensure_dir_chain(paths, parent, 0o755).map_err(|e| e.to_string())?;
        record.files.push(File {
            path: path.clone(),
            hash: sha256_hex(body.as_bytes()),
        });
    }
    for unit in UNITS {
        let props = command(
            runner,
            HostTool::Systemctl,
            &[
                "show",
                unit,
                "--property=LoadState,ActiveState,UnitFileState,DropInPaths",
            ],
        )?;
        // Package removal can delete the unit file while its process keeps running.
        // LoadState describes the file, independently of the unit's runtime state.
        let needs_stop = props.lines().any(|p| {
            matches!(
                p,
                "ActiveState=active"
                    | "ActiveState=activating"
                    | "ActiveState=reloading"
                    | "ActiveState=refreshing"
                    | "ActiveState=deactivating"
            )
        });
        if needs_stop || !props.lines().any(|p| p == "LoadState=not-found") {
            let property = |key: &str| {
                props
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .unwrap_or("unavailable")
                    .to_string()
            };
            tracing::info!(target:"awcc_service::provider_install", code="provider_unit_detected", unit, load_state=%property("LoadState="), active_state=%property("ActiveState="), unit_file_state=%property("UnitFileState="), needs_stop, "known provider inspected before handover");
        }
        if props.lines().any(|p| p == "LoadState=not-found") && !needs_stop {
            continue;
        }
        if props.lines().any(|p| p == "UnitFileState=masked-runtime") {
            return Err(format!(
                "administrator runtime mask for {unit}; explicit handover review required"
            ));
        }
        if props.lines().any(|p| p == "UnitFileState=masked") {
            continue;
        }
        if props
            .lines()
            .filter_map(|p| p.strip_prefix("DropInPaths="))
            .any(|p| {
                p.split_whitespace()
                    .any(|v| v.starts_with("/etc/") || v.starts_with("/run/"))
            })
            || fs::symlink_metadata(paths.join(&format!("/etc/systemd/system/{unit}"))).is_ok()
        {
            return Err(format!(
                "administrator override for {unit}; explicit handover required"
            ));
        }
        record.units.push(Unit {
            name: unit.to_string(),
            active: props.lines().any(|p| {
                matches!(
                    p,
                    "ActiveState=active"
                        | "ActiveState=activating"
                        | "ActiveState=reloading"
                        | "ActiveState=refreshing"
                )
            }),
            enabled: props.lines().any(|p| p == "UnitFileState=enabled"),
        });
    }
    tracing::info!(target:"awcc_service::provider_install", code="provider_handover_planned", units=?record.units.iter().map(|unit| unit.name.as_str()).collect::<Vec<_>>(), registrations=record.files.len(), "saving handover journal before changes");
    save(paths, &record)?;
    let result: Result<(), String> = (|| {
        for unit in &record.units {
            command(runner, HostTool::Systemctl, &["mask", "--now", &unit.name])?;
        }
        for (path, body) in files {
            write_atomic(&paths.join(&path), body.as_bytes(), 0o644).map_err(|e| e.to_string())?;
        }
        command(
            runner,
            HostTool::Busctl,
            &[
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReloadConfig",
            ],
        )?;
        Ok(())
    })();
    if result.is_ok() {
        record.ready = true;
        save(paths, &record)?;
        tracing::info!(target:"awcc_service::provider_install", code="provider_handover_completed", "provider registrations and masks installed");
    }
    if let Err(e) = result {
        let recovery = remove(paths, runner);
        tracing::error!(target:"awcc_service::provider_install", code="provider_handover_failed", error=%e, recovery=?recovery, "provider handover failed; recovery outcome recorded");
        return Err(format!(
            "provider handover failed: {e}; recovery: {recovery:?}"
        ));
    }
    Ok(())
}
/// Restore only recorded masks/registrations which still match what AWCC installed.
pub fn remove(paths: &InstallPaths, runner: &dyn HostRunner) -> Result<(), String> {
    tracing::info!(target:"awcc_service::provider_install", code="provider_recovery_started", "restoring recorded provider state");
    let Some(record) = load(paths)? else {
        tracing::info!(target:"awcc_service::provider_install", code="provider_recovery_not_needed", "no provider journal to restore");
        return Ok(());
    };
    for file in &record.files {
        let parent = Path::new(&file.path)
            .parent()
            .and_then(Path::to_str)
            .ok_or("invalid provider path")?;
        crate::installer::check_existing_dir_chain(paths, parent).map_err(|e| e.to_string())?;
        let path = paths.join(&file.path);
        if let Ok(meta) = fs::symlink_metadata(&path)
            && (!meta.is_file() || !fs::read(&path).is_ok_and(|b| sha256_hex(&b) == file.hash))
        {
            return Err(format!(
                "preserved changed provider registration {}; recovery requires administrator review",
                file.path
            ));
        }
    }
    for file in &record.files {
        let known = file.path == "/etc/dbus-1/system.d/awcc-power-profiles.conf"
            || file.path == "/usr/share/polkit-1/actions/awcc-power-profiles.policy"
            || [crate::power_bus::MODERN.0, crate::power_bus::COMPAT.0]
                .iter()
                .any(|n| {
                    file.path == format!("/usr/local/share/dbus-1/system-services/{n}.service")
                });
        if !known {
            return Err("unexpected handover file path".into());
        }
        let path = paths.join(&file.path);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
            && fs::read(&path).is_ok_and(|body| sha256_hex(&body) == file.hash)
        {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    for unit in &record.units {
        if !UNITS.contains(&unit.name.as_str()) {
            return Err("unexpected handover unit".into());
        }
        let mask = paths.join(&format!("/etc/systemd/system/{}", unit.name));
        if fs::read_link(&mask).is_ok_and(|p| p == Path::new("/dev/null")) {
            if paths.systemd_runtime_dir().is_dir() {
                command(runner, HostTool::Systemctl, &["unmask", &unit.name])?;
                if unit.active {
                    let props = command(
                        runner,
                        HostTool::Systemctl,
                        &["show", &unit.name, "--property=LoadState"],
                    )?;
                    // A package transaction may have removed the old executable/unit.
                    // Remove our mask without trying to restart a missing provider.
                    if !props.lines().any(|p| p == "LoadState=not-found") {
                        command(runner, HostTool::Systemctl, &["start", &unit.name])?;
                    }
                }
            } else {
                fs::remove_file(mask).map_err(|e| e.to_string())?;
            }
        }
    }
    if paths.systemd_runtime_dir().is_dir() {
        command(
            runner,
            HostTool::Busctl,
            &[
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReloadConfig",
            ],
        )?;
    }
    fs::remove_file(paths.join(RECORD)).map_err(|e| e.to_string())?;
    tracing::info!(target:"awcc_service::provider_install", code="provider_recovery_completed", "recorded provider changes restored");
    Ok(())
}

pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Passive startup health: checks the negotiated socket state after both bus names were acquired.
pub fn health() -> Result<(), String> {
    let client =
        crate::ipc::DaemonClient::system("awcc-provider-health", env!("CARGO_PKG_VERSION"));
    let until = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if matches!(client.probe(),crate::ipc::ServiceProbe::Running{status,..} if status.control_running && status.power_provider_ready)
        {
            return Ok(());
        }
        if std::time::Instant::now() >= until {
            return Err(
                "PowerProfiles provider did not become ready; inspect awccd.service logs".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::installer::{HostError, HostOutput};
    use std::sync::Mutex;
    struct Fake {
        root: std::path::PathBuf,
        calls: Mutex<Vec<Vec<String>>>,
        fail_reload: AtomicBool,
        provider_load_state: &'static str,
        provider_active_state: &'static str,
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    impl HostRunner for Fake {
        fn run(
            &self,
            c: &HostCommand,
            _: Option<&mut dyn FnMut(&str)>,
        ) -> Result<HostOutput, HostError> {
            let args = c
                .args
                .iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            self.calls.lock().unwrap().push(args.clone());
            let mut output = String::new();
            let mut code = 0;
            if c.tool == HostTool::Systemctl {
                match args[0].as_str() {
                    "show" if args[1] == "power-profiles-daemon.service" => {
                        output = format!(
                            "LoadState={}\nActiveState={}\nUnitFileState={}\nDropInPaths=\n",
                            self.provider_load_state,
                            self.provider_active_state,
                            if self.provider_load_state == "loaded" {
                                "enabled"
                            } else {
                                ""
                            }
                        )
                    }
                    "show" => output = "LoadState=not-found\n".into(),
                    "mask" => {
                        let p = self
                            .root
                            .join("etc/systemd/system/power-profiles-daemon.service");
                        fs::create_dir_all(p.parent().unwrap()).unwrap();
                        std::os::unix::fs::symlink("/dev/null", p).unwrap();
                    }
                    "unmask" => {
                        fs::remove_file(
                            self.root
                                .join("etc/systemd/system/power-profiles-daemon.service"),
                        )
                        .unwrap();
                    }
                    "start" if self.provider_load_state == "not-found" => code = 1,
                    _ => {}
                }
            } else if self.fail_reload.swap(false, Ordering::SeqCst) {
                code = 1;
            }
            Ok(HostOutput {
                code: Some(code),
                stdout: output,
                stderr: String::new(),
            })
        }
    }
    fn fixture(name: &str) -> (InstallPaths, Fake) {
        let root = std::env::temp_dir().join(format!(
            "awcc-provider-install-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("run/systemd/system")).unwrap();
        (
            InstallPaths::with_root(&root),
            Fake {
                root,
                calls: Mutex::new(vec![]),
                fail_reload: AtomicBool::new(false),
                provider_load_state: "loaded",
                provider_active_state: "active",
            },
        )
    }
    #[test]
    fn handover_is_idempotent_and_removal_restores_only_recorded_state() {
        let (paths, fake) = fixture("roundtrip");
        install(&paths, &fake, "/usr/bin/awcc-cli").unwrap();
        let count = fake.calls.lock().unwrap().len();
        install(&paths, &fake, "/usr/bin/awcc-cli").unwrap();
        assert_eq!(fake.calls.lock().unwrap().len(), count);
        remove(&paths, &fake).unwrap();
        assert!(!paths.join(RECORD).exists());
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["start", "power-profiles-daemon.service"])
        );
        assert!(!paths.join("/usr/local/share/dbus-1/system-services/org.freedesktop.UPower.PowerProfiles.service").exists());
    }
    #[test]
    fn removed_but_running_provider_is_stopped_and_not_restarted_on_removal() {
        for active_state in [
            "active",
            "activating",
            "reloading",
            "refreshing",
            "deactivating",
        ] {
            let (paths, mut fake) = fixture(&format!("removed-{active_state}"));
            fake.provider_load_state = "not-found";
            fake.provider_active_state = active_state;
            install(&paths, &fake, "/usr/bin/awcc-cli").unwrap();
            let record = load(&paths).unwrap().unwrap();
            assert!(record.ready);
            assert_eq!(record.units.len(), 1);
            assert_eq!(record.units[0].name, "power-profiles-daemon.service");
            assert_eq!(record.units[0].active, active_state != "deactivating");
            assert!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| { c == &["mask", "--now", "power-profiles-daemon.service"] })
            );
            remove(&paths, &fake).unwrap();
            assert!(!paths.join(RECORD).exists());
            assert!(
                fs::symlink_metadata(
                    paths.join("/etc/systemd/system/power-profiles-daemon.service")
                )
                .is_err()
            );
            assert!(
                !fake
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| { c == &["start", "power-profiles-daemon.service"] })
            );
        }
    }
    #[test]
    fn failed_handover_with_removed_provider_cleans_up_without_restart() {
        let (paths, mut fake) = fixture("removed-rollback");
        fake.provider_load_state = "not-found";
        fake.fail_reload.store(true, Ordering::SeqCst);
        let error = install(&paths, &fake, "/usr/bin/awcc-cli").unwrap_err();
        assert!(error.contains("recovery: Ok(())"), "{error}");
        assert!(!paths.join(RECORD).exists());
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| { c == &["unmask", "power-profiles-daemon.service"] })
        );
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| { c == &["start", "power-profiles-daemon.service"] })
        );
    }
    #[test]
    fn absent_inactive_provider_is_not_masked_or_restarted() {
        for active_state in ["inactive", "failed"] {
            let (paths, mut fake) = fixture(&format!("absent-{active_state}"));
            fake.provider_load_state = "not-found";
            fake.provider_active_state = active_state;
            install(&paths, &fake, "/usr/bin/awcc-cli").unwrap();
            assert!(load(&paths).unwrap().unwrap().units.is_empty());
            remove(&paths, &fake).unwrap();
            assert!(
                !fake
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| { matches!(c[0].as_str(), "mask" | "unmask" | "start") })
            );
        }
    }
    #[test]
    fn failed_handover_recovers_and_administrator_edits_are_preserved() {
        let (paths, fake) = fixture("rollback");
        fake.fail_reload.store(true, Ordering::SeqCst);
        assert!(install(&paths, &fake, "/usr/bin/awcc-cli").is_err());
        assert!(!paths.join(RECORD).exists());
        install(&paths, &fake, "/usr/bin/awcc-cli").unwrap();
        let path = paths.join("/etc/dbus-1/system.d/awcc-power-profiles.conf");
        fs::write(&path, "administrator configuration").unwrap();
        assert!(remove(&paths, &fake).is_err());
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "administrator configuration"
        );
        assert!(paths.join(RECORD).exists());
    }
}
