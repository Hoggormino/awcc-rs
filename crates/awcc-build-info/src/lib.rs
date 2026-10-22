//! Shared build identity for `awcc-cli`, `awcc-gui`, and later phases (daemon, diagnostics).
//!
//! The identity is resolved by `build.rs` before compilation (see the precedence rules in that
//! file) and embedded as string constants. Hashes of the *final* release artifacts are
//! deliberately not embedded: they live in the external `build-manifest.json`, because embedding
//! a hash of the artifact inside the artifact would be a dependency cycle.
//!
//! The installation format is *not* compiled in. The compiled payload is reused unchanged for the
//! tarball, AppImage, `.deb`, `.rpm`, and Arch package, so the format is read at run time from a
//! marker file each package ships next to the binaries (`../share/awcc/package-format`). A binary
//! without such a marker (for example `cargo run`) reports `unknown`.

use std::fs::File;
use std::io::Read;
use std::path::Path;

mod identity;

pub use identity::{CommitSource, DirtyState};

/// Path of the package-format marker, relative to the directory containing the executable.
pub const PACKAGE_MARKER_RELATIVE_PATH: &str = "../share/awcc/package-format";

/// Maximum number of marker bytes read; anything longer is not a valid marker.
const MARKER_READ_LIMIT: u64 = 64;

/// Installation format as declared by a package-provided marker file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageFormat {
    Tarball,
    AppImage,
    Deb,
    Rpm,
    Arch,
    /// No (valid) marker found. Never guessed from the executable path or distribution.
    Unknown,
}

impl PackageFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tarball => "tarball",
            Self::AppImage => "appimage",
            Self::Deb => "deb",
            Self::Rpm => "rpm",
            Self::Arch => "arch",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the contents of a marker file. Only the exact known tokens are accepted.
    #[must_use]
    pub fn from_marker(contents: &str) -> Self {
        match contents.trim() {
            "tarball" => Self::Tarball,
            "appimage" => Self::AppImage,
            "deb" => Self::Deb,
            "rpm" => Self::Rpm,
            "arch" => Self::Arch,
            _ => Self::Unknown,
        }
    }
}

/// Read the package marker next to `exe` (`<exe dir>/../share/awcc/package-format`).
#[must_use]
pub fn detect_package_format_for(exe: &Path) -> PackageFormat {
    let resolved = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let Some(dir) = resolved.parent() else {
        return PackageFormat::Unknown;
    };
    let marker = dir.join(PACKAGE_MARKER_RELATIVE_PATH);
    let Ok(file) = File::open(marker) else {
        return PackageFormat::Unknown;
    };
    let mut contents = String::new();
    if file
        .take(MARKER_READ_LIMIT)
        .read_to_string(&mut contents)
        .is_err()
    {
        return PackageFormat::Unknown;
    }
    PackageFormat::from_marker(&contents)
}

/// Detect the package format of the running executable.
#[must_use]
pub fn detect_package_format() -> PackageFormat {
    std::env::current_exe()
        .map(|exe| detect_package_format_for(&exe))
        .unwrap_or(PackageFormat::Unknown)
}

/// Build identity resolved before compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    /// Application version (`CARGO_PKG_VERSION` of the workspace).
    pub version: String,
    /// Full source commit, or `None` when it could not be established.
    pub commit: Option<String>,
    pub commit_source: CommitSource,
    pub dirty: DirtyState,
    /// Commit timestamp (`SOURCE_DATE_EPOCH` semantics) when known.
    pub source_date_epoch: Option<u64>,
    pub target: String,
    pub profile: String,
    /// First line of `rustc --version` for the compiler that built this binary.
    pub rustc: String,
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn or_unknown(value: &str) -> String {
    non_empty(value).unwrap_or_else(|| "unknown".to_string())
}

impl BuildInfo {
    /// The identity embedded into the running binary.
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: non_empty(env!("AWCC_BUILD_COMMIT")),
            commit_source: CommitSource::from_token(env!("AWCC_BUILD_COMMIT_SOURCE")),
            dirty: DirtyState::from_token(env!("AWCC_BUILD_DIRTY")),
            source_date_epoch: env!("AWCC_BUILD_SOURCE_DATE_EPOCH").parse().ok(),
            target: or_unknown(env!("AWCC_BUILD_TARGET")),
            profile: or_unknown(env!("AWCC_BUILD_PROFILE")),
            rustc: or_unknown(env!("AWCC_BUILD_RUSTC")),
        }
    }

    /// First 12 characters of the commit, if known.
    #[must_use]
    pub fn short_commit(&self) -> Option<&str> {
        self.commit.as_deref().map(|c| &c[..c.len().min(12)])
    }

    /// Compact `0.1.0 (2e350363fb18)` / `0.1.0 (2e350363fb18, dirty)` / `0.1.0 (unknown revision)`.
    #[must_use]
    pub fn headline(&self) -> String {
        let revision = match (self.short_commit(), self.dirty) {
            (Some(short), DirtyState::Clean) => short.to_string(),
            (Some(short), DirtyState::Dirty) => format!("{short}, dirty"),
            (Some(short), DirtyState::Unknown) => format!("{short}, dirty state unknown"),
            (None, _) => "unknown revision".to_string(),
        };
        format!("{} ({revision})", self.version)
    }

    /// Render the multi-line `--version` text for `binary_name`.
    ///
    /// The first line is human oriented; the following `key: value` lines are stable and
    /// machine-parseable (used by the packaging smoke tests and the release identity check).
    #[must_use]
    pub fn render_version(&self, binary_name: &str, package_format: PackageFormat) -> String {
        let epoch = self
            .source_date_epoch
            .map_or_else(|| "unknown".to_string(), |e| e.to_string());
        [
            format!("{binary_name} {}", self.headline()),
            format!("version: {}", self.version),
            format!("commit: {}", self.commit.as_deref().unwrap_or("unknown")),
            format!("commit-source: {}", self.commit_source.as_str()),
            format!("dirty: {}", self.dirty.as_str()),
            format!("source-date-epoch: {epoch}"),
            format!("target: {}", self.target),
            format!("profile: {}", self.profile),
            format!("rustc: {}", self.rustc),
            format!("package-format: {}", package_format.as_str()),
        ]
        .join("\n")
    }
}

/// Render `--version` for the running binary (embedded identity + detected package format).
#[must_use]
pub fn version_text(binary_name: &str) -> String {
    BuildInfo::current().render_version(binary_name, detect_package_format())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn sample() -> BuildInfo {
        BuildInfo {
            version: "0.1.0".into(),
            commit: Some("2e350363fb1830a8449edd56bdd491f6c0ba2563".into()),
            commit_source: CommitSource::Git,
            dirty: DirtyState::Clean,
            source_date_epoch: Some(1_760_000_000),
            target: "x86_64-unknown-linux-gnu".into(),
            profile: "release".into(),
            rustc: "rustc 1.95.0 (59807616e 2026-04-14)".into(),
        }
    }

    #[test]
    fn version_text_has_stable_machine_readable_fields() {
        let text = sample().render_version("awcc-cli", PackageFormat::Deb);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("awcc-cli 0.1.0 (2e350363fb18)"));
        let fields: Vec<&str> = lines.collect();
        assert_eq!(
            fields,
            [
                "version: 0.1.0",
                "commit: 2e350363fb1830a8449edd56bdd491f6c0ba2563",
                "commit-source: git",
                "dirty: false",
                "source-date-epoch: 1760000000",
                "target: x86_64-unknown-linux-gnu",
                "profile: release",
                "rustc: rustc 1.95.0 (59807616e 2026-04-14)",
                "package-format: deb",
            ]
        );
    }

    #[test]
    fn unknown_and_dirty_states_are_reported_explicitly() {
        let mut info = sample();
        info.dirty = DirtyState::Dirty;
        assert_eq!(info.headline(), "0.1.0 (2e350363fb18, dirty)");

        info.dirty = DirtyState::Unknown;
        assert_eq!(info.headline(), "0.1.0 (2e350363fb18, dirty state unknown)");

        info.commit = None;
        info.commit_source = CommitSource::Unknown;
        info.source_date_epoch = None;
        let text = info.render_version("awcc-gui", PackageFormat::Unknown);
        assert!(text.starts_with("awcc-gui 0.1.0 (unknown revision)"));
        assert!(text.contains("\ncommit: unknown\n"));
        assert!(text.contains("\ncommit-source: unknown\n"));
        assert!(text.contains("\ndirty: unknown\n"));
        assert!(text.contains("\nsource-date-epoch: unknown\n"));
        assert!(text.contains("\npackage-format: unknown"));
    }

    #[test]
    fn package_marker_accepts_only_known_tokens() {
        assert_eq!(
            PackageFormat::from_marker("appimage\n"),
            PackageFormat::AppImage
        );
        assert_eq!(PackageFormat::from_marker(" rpm "), PackageFormat::Rpm);
        assert_eq!(
            PackageFormat::from_marker("flatpak"),
            PackageFormat::Unknown
        );
        assert_eq!(PackageFormat::from_marker(""), PackageFormat::Unknown);
        assert_eq!(
            PackageFormat::from_marker("deb extra"),
            PackageFormat::Unknown
        );
    }

    #[test]
    fn marker_is_read_relative_to_the_executable_and_unknown_without_it() {
        let root =
            std::env::temp_dir().join(format!("awcc_build_info_marker_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let bin = root.join("bin");
        let share = root.join("share/awcc");
        fs::create_dir_all(&bin).expect("create bin");
        fs::create_dir_all(&share).expect("create share");
        let exe = bin.join("awcc-cli");
        fs::write(&exe, b"").expect("create fake exe");

        assert_eq!(detect_package_format_for(&exe), PackageFormat::Unknown);

        fs::write(share.join("package-format"), "arch\n").expect("write marker");
        assert_eq!(detect_package_format_for(&exe), PackageFormat::Arch);

        // Oversized or garbage markers are not trusted.
        fs::write(share.join("package-format"), "x".repeat(4096)).expect("write bogus marker");
        assert_eq!(detect_package_format_for(&exe), PackageFormat::Unknown);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn embedded_identity_is_self_consistent() {
        let info = BuildInfo::current();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.target.is_empty());
        if let Some(commit) = &info.commit {
            assert!(commit.len() == 40 || commit.len() == 64);
            assert_ne!(info.commit_source, CommitSource::Unknown);
        } else {
            assert_eq!(info.commit_source, CommitSource::Unknown);
            assert_eq!(info.dirty, DirtyState::Unknown);
        }
    }
}
