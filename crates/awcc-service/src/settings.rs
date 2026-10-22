//! Service-owned persistent settings (`/var/lib/awcc/settings.json`).
//!
//! The file records *desired* configuration only: curve parameters, whether the curve should be
//! running (the user's intent) and whether it is restored when the daemon starts. What the
//! hardware is actually doing is reported separately and is never read back from this file.
//!
//! Writes are atomic (temporary file in the same directory, `fsync`, `rename`, directory
//! `fsync`) and files are created `0600`. A file written by a newer schema is never overwritten.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use awcc_core::FanCurveConfig;
use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};

use crate::daemon::DaemonRpcRequest;

/// Current settings schema version.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
/// Persistent service state directory.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/awcc";
/// File name of the settings document inside the state directory.
pub const SETTINGS_FILE_NAME: &str = "settings.json";
/// Upper bound on the settings file size that will be read.
pub const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

/// Validated, versioned service settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSettings {
    pub schema_version: u32,
    /// DMI product name the settings were saved for.
    pub device_name: String,
    /// ACPI prefix the settings were saved for.
    pub acpi_prefix: String,
    /// Saved curve parameters (`enabled` is always stored as `false`; see `intent_active`).
    pub curve: FanCurveConfig,
    /// The user's intent: run the curve.
    pub intent_active: bool,
    /// Resume the curve when the daemon starts (default off on a fresh installation).
    pub restore_at_startup: bool,
}

impl ServiceSettings {
    /// Fresh-install settings: default curve parameters, nothing active, no restore.
    #[must_use]
    pub fn fresh(device_name: &str, acpi_prefix: &str) -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            device_name: device_name.to_string(),
            acpi_prefix: acpi_prefix.to_string(),
            curve: FanCurveConfig::default(),
            intent_active: false,
            restore_at_startup: false,
        }
    }
}

/// Validate curve parameters with the same domain rules as the live configure operation.
///
/// # Errors
/// Returns the stable validation message when the curve is outside the supported ranges.
pub fn validate_curve(config: &FanCurveConfig) -> Result<(), String> {
    let mut probe = config.clone();
    probe.enabled = true;
    DaemonRpcRequest::ConfigureFanCurve { config: probe }.validate()
}

/// Outcome of loading the settings file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsLoad {
    /// No file exists (fresh installation).
    Missing,
    /// Valid settings for this device.
    Loaded(ServiceSettings),
    /// Malformed, truncated, oversized, unreadable or semantically invalid.
    Invalid { reason: String },
    /// Written by a newer schema; kept untouched so a newer daemon can still read it.
    Incompatible { found_version: u32 },
    /// Saved for a different device or ACPI prefix.
    WrongDevice { reason: String },
}

/// Atomic persistent store for [`ServiceSettings`].
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    #[must_use]
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self {
            path: state_dir.as_ref().join(SETTINGS_FILE_NAME),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load and validate settings for `device_name` / `acpi_prefix`.
    #[must_use]
    pub fn load(&self, device_name: &str, acpi_prefix: &str) -> SettingsLoad {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags((OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK).bits() as i32);
        let mut file = match options.open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SettingsLoad::Missing,
            Err(e) => {
                return SettingsLoad::Invalid {
                    reason: format!("cannot open settings: {e}"),
                };
            }
        };
        let Ok(meta) = file.metadata() else {
            return SettingsLoad::Invalid {
                reason: "cannot stat settings".to_string(),
            };
        };
        if !meta.is_file() {
            return SettingsLoad::Invalid {
                reason: "settings path is not a regular file".to_string(),
            };
        }
        if meta.len() > MAX_SETTINGS_BYTES {
            return SettingsLoad::Invalid {
                reason: format!("settings file exceeds {MAX_SETTINGS_BYTES} bytes"),
            };
        }
        if meta.mode() & 0o022 != 0 {
            return SettingsLoad::Invalid {
                reason: "settings file is group/world-writable".to_string(),
            };
        }
        if meta.uid() != rustix::process::geteuid().as_raw() {
            return SettingsLoad::Invalid {
                reason: "settings file is not owned by the service user".to_string(),
            };
        }
        let mut text = String::new();
        if let Err(e) = (&mut file)
            .take(MAX_SETTINGS_BYTES)
            .read_to_string(&mut text)
        {
            return SettingsLoad::Invalid {
                reason: format!("cannot read settings: {e}"),
            };
        }
        // Detect a newer schema before strict parsing so unknown fields do not mask it.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(version) = value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
            && version > u64::from(SETTINGS_SCHEMA_VERSION)
        {
            return SettingsLoad::Incompatible {
                found_version: u32::try_from(version).unwrap_or(u32::MAX),
            };
        }
        let settings: ServiceSettings = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                return SettingsLoad::Invalid {
                    reason: format!("malformed settings: {e}"),
                };
            }
        };
        if settings.schema_version != SETTINGS_SCHEMA_VERSION {
            return SettingsLoad::Invalid {
                reason: format!("unsupported schema version {}", settings.schema_version),
            };
        }
        if settings.device_name != device_name || settings.acpi_prefix != acpi_prefix {
            return SettingsLoad::WrongDevice {
                reason: format!(
                    "settings were saved for `{}` ({}), this device is `{device_name}` ({acpi_prefix})",
                    settings.device_name, settings.acpi_prefix
                ),
            };
        }
        if let Err(reason) = validate_curve(&settings.curve) {
            return SettingsLoad::Invalid { reason };
        }
        SettingsLoad::Loaded(settings)
    }

    /// Atomically replace the settings file.
    ///
    /// # Errors
    /// Returns the underlying I/O error. Failures before rename leave the previous file untouched;
    /// a directory-fsync failure after rename means the new file's durability is uncertain.
    pub fn save(&self, settings: &ServiceSettings) -> std::io::Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("settings path has no parent directory"))?;
        fs::create_dir_all(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir()
            || meta.mode() & 0o022 != 0
            || meta.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "settings directory must be owned by the service user and not writable by others",
            ));
        }
        if let SettingsLoad::Incompatible { found_version } =
            self.load(&settings.device_name, &settings.acpi_prefix)
        {
            return Err(std::io::Error::other(format!(
                "refusing to overwrite settings schema {found_version} with schema {SETTINGS_SCHEMA_VERSION}"
            )));
        }
        let mut stored = settings.clone();
        stored.curve.enabled = false;
        let mut body = serde_json::to_vec_pretty(&stored).map_err(std::io::Error::other)?;
        body.push(b'\n');
        let tmp = dir.join(format!(".{SETTINGS_FILE_NAME}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let write_result = file.write_all(&body).and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        File::open(dir).and_then(|d| d.sync_all())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("awcc_settings_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_then_saved_round_trips_with_private_mode() {
        let d = dir("roundtrip");
        let store = SettingsStore::new(&d);
        assert_eq!(store.load("M15", "AMWW"), SettingsLoad::Missing);
        let mut s = ServiceSettings::fresh("M15", "AMWW");
        s.intent_active = true;
        s.restore_at_startup = true;
        store.save(&s).unwrap();
        assert_eq!(store.load("M15", "AMWW"), SettingsLoad::Loaded(s));
        let mode = fs::metadata(store.path()).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(
            !fs::read_dir(&d)
                .unwrap()
                .any(|e| { e.unwrap().file_name().to_string_lossy().ends_with(".tmp") })
        );
    }

    #[test]
    fn fresh_settings_never_activate_or_restore() {
        let s = ServiceSettings::fresh("M15", "AMWW");
        assert!(!s.intent_active);
        assert!(!s.restore_at_startup);
        assert!(!s.curve.enabled);
    }

    #[test]
    fn corrupt_truncated_wrong_device_and_newer_schema_are_distinguished() {
        let d = dir("bad");
        let store = SettingsStore::new(&d);
        fs::write(store.path(), b"{ not json").unwrap();
        assert!(matches!(
            store.load("M15", "AMWW"),
            SettingsLoad::Invalid { .. }
        ));

        let s = ServiceSettings::fresh("Other", "AMWW");
        let _ = fs::remove_file(store.path());
        store.save(&s).unwrap();
        assert!(matches!(
            store.load("M15", "AMWW"),
            SettingsLoad::WrongDevice { .. }
        ));

        fs::write(store.path(), br#"{"schema_version":99,"future":"field"}"#).unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.load("M15", "AMWW"),
            SettingsLoad::Incompatible { found_version: 99 }
        );
    }

    #[test]
    fn out_of_range_curve_is_rejected_not_clamped() {
        let d = dir("range");
        let store = SettingsStore::new(&d);
        let mut s = ServiceSettings::fresh("M15", "AMWW");
        s.curve.cpu_points[1].boost_pct = 250;
        store.save(&s).unwrap();
        assert!(matches!(
            store.load("M15", "AMWW"),
            SettingsLoad::Invalid { .. }
        ));
    }

    #[test]
    fn writable_by_others_and_symlinked_files_are_refused() {
        let d = dir("perm");
        let store = SettingsStore::new(&d);
        store.save(&ServiceSettings::fresh("M15", "AMWW")).unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            store.load("M15", "AMWW"),
            SettingsLoad::Invalid { .. }
        ));

        let target = d.join("real.json");
        fs::rename(store.path(), &target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, store.path()).unwrap();
        assert!(matches!(
            store.load("M15", "AMWW"),
            SettingsLoad::Invalid { .. }
        ));
    }

    #[test]
    fn save_refuses_a_newer_schema_even_if_it_appeared_after_loading() {
        let d = dir("newer_after_load");
        let store = SettingsStore::new(&d);
        assert_eq!(store.load("M15", "AMWW"), SettingsLoad::Missing);
        let future = br#"{"schema_version":99,"future":"preserve this"}"#;
        fs::write(store.path(), future).unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
        let error = store
            .save(&ServiceSettings::fresh("M15", "AMWW"))
            .unwrap_err();
        assert!(error.to_string().contains("schema 99"));
        assert_eq!(fs::read(store.path()).unwrap(), future);
    }

    #[test]
    fn save_refuses_a_shared_or_symlinked_settings_directory() {
        let d = dir("unsafe_dir");
        let store = SettingsStore::new(&d);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(store.save(&ServiceSettings::fresh("M15", "AMWW")).is_err());
        assert!(!store.path().exists());
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        let link = d.join("link");
        std::os::unix::fs::symlink(&d, &link).unwrap();
        assert!(
            SettingsStore::new(&link)
                .save(&ServiceSettings::fresh("M15", "AMWW"))
                .is_err()
        );
        assert!(!store.path().exists());
    }
}
