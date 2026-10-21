//! Service payload discovery and strict verification.
//!
//! The payload is a directory staged by `scripts/stage-release-payload.sh`:
//!
//! ```text
//! MANIFEST.sha256
//! bin/awcc-cli
//! lib/libusb-1.0.so.0      (private libraries the service needs; optional)
//! share/awcc/database.json
//! ```
//!
//! `MANIFEST.sha256` holds `build-id: <id>` and `version: <v>` lines followed by sorted
//! `<sha256>  <relative path>` lines (compatible with `sha256sum -c` once the two metadata lines
//! are filtered out). The manifest only proves *integrity* of this release's files; it does not
//! prove who published the release. The administrator who runs the installer authorizes this
//! release.
//!
//! Verification rejects hostile trees: traversal or absolute paths, symlinks, special files,
//! setuid/setgid/sticky or group/world-writable modes, unlisted files, hash mismatches, and
//! (when privileged checks are on) anything not owned by root.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use awcc_diagnostics::hash::sha256_hex;
use thiserror::Error;

/// Manifest file name inside a payload and inside an installed release.
pub const MANIFEST_NAME: &str = "MANIFEST.sha256";
/// Payload location relative to the directory holding the running executable.
pub const PAYLOAD_RELATIVE_PATH: &str = "../share/awcc/service-payload";
/// Executable every payload must carry.
pub const REQUIRED_BINARY: &str = "bin/awcc-cli";
/// Database every payload must carry.
pub const REQUIRED_DATABASE: &str = "share/awcc/database.json";

const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 768 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_DEPTH: usize = 12;
const NOFOLLOW: i32 = rustix::fs::OFlags::NOFOLLOW.bits() as i32;

/// Why a payload was rejected.
#[derive(Debug, Error)]
pub enum PayloadError {
    #[error(
        "this build carries no service payload at {0}; native packages install awccd.service \
         through the package manager instead"
    )]
    Missing(String),
    #[error("payload manifest is invalid: {0}")]
    BadManifest(String),
    #[error("payload entry `{path}` is unsafe: {reason}")]
    Unsafe { path: String, reason: String },
    #[error("payload file `{0}` does not match its manifest hash")]
    HashMismatch(String),
    #[error("payload contains `{0}`, which the manifest does not list")]
    Unlisted(String),
    #[error("payload is missing required file `{0}`")]
    MissingFile(String),
    #[error("payload is too large (limit {0} bytes)")]
    TooLarge(u64),
    #[error("cannot read payload file `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Where the verified payload is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadSource {
    dir: PathBuf,
}

impl PayloadSource {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Payload next to a (canonical) executable: `<exe dir>/../share/awcc/service-payload`. This
    /// resolves identically for a tarball tree and for an extracted AppImage, and deliberately
    /// ignores `APPDIR` so an environment variable cannot redirect a privileged install.
    ///
    /// # Errors
    /// [`PayloadError::Missing`] when the directory does not exist.
    pub fn for_executable(exe: &Path) -> Result<Self, PayloadError> {
        let dir = exe
            .parent()
            .map_or_else(PathBuf::new, |p| p.join(PAYLOAD_RELATIVE_PATH));
        match fs::canonicalize(&dir) {
            Ok(canonical) if canonical.is_dir() => Ok(Self { dir: canonical }),
            _ => Err(PayloadError::Missing(dir.display().to_string())),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// One manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub executable: bool,
}

/// A payload whose manifest parsed and whose files all matched.
#[derive(Debug, Clone)]
pub struct VerifiedPayload {
    pub dir: PathBuf,
    pub build_id: String,
    pub version: String,
    pub files: Vec<PayloadFile>,
    pub manifest_bytes: Vec<u8>,
}

/// Parsed manifest (no filesystem access yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedManifest {
    pub build_id: String,
    pub version: String,
    pub entries: BTreeMap<String, String>,
}

pub(crate) fn valid_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

/// Check that `path` is a clean relative path made of normal components only.
///
/// # Errors
/// A human-readable reason when it is not.
pub fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 240 {
        return Err("empty or too long".to_owned());
    }
    if path.starts_with('/') {
        return Err("absolute path".to_owned());
    }
    if path.bytes().any(|b| b == 0 || b == b'\\' || b < 0x20) {
        return Err("contains control or backslash characters".to_owned());
    }
    let mut depth = 0;
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => return Err("path traversal (`..`)".to_owned()),
            _ => return Err("non-normal path component".to_owned()),
        }
    }
    if path.contains("//") || path.ends_with('/') {
        return Err("non-canonical path".to_owned());
    }
    if depth > MAX_DEPTH {
        return Err("too deeply nested".to_owned());
    }
    Ok(())
}

/// Parse and validate manifest text.
///
/// # Errors
/// [`PayloadError::BadManifest`] or [`PayloadError::Unsafe`].
pub fn parse_manifest(text: &str) -> Result<ParsedManifest, PayloadError> {
    let mut build_id = None;
    let mut version = None;
    let mut entries = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("build-id: ") {
            if !valid_token(value) || build_id.replace(value.to_owned()).is_some() {
                return Err(PayloadError::BadManifest(format!(
                    "line {number}: invalid or repeated build-id"
                )));
            }
        } else if let Some(value) = line.strip_prefix("version: ") {
            if !valid_token(value) || version.replace(value.to_owned()).is_some() {
                return Err(PayloadError::BadManifest(format!(
                    "line {number}: invalid or repeated version"
                )));
            }
        } else {
            let Some((hash, path)) = line.split_once("  ") else {
                return Err(PayloadError::BadManifest(format!(
                    "line {number}: expected `<sha256>  <path>`"
                )));
            };
            if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(PayloadError::BadManifest(format!(
                    "line {number}: bad sha256 digest"
                )));
            }
            validate_relative_path(path).map_err(|reason| PayloadError::Unsafe {
                path: path.to_owned(),
                reason,
            })?;
            let top = path.split('/').next().unwrap_or_default();
            if !matches!(top, "bin" | "lib" | "share") || path == MANIFEST_NAME {
                return Err(PayloadError::Unsafe {
                    path: path.to_owned(),
                    reason: "outside bin/, lib/ and share/".to_owned(),
                });
            }
            if entries.insert(path.to_owned(), hash.to_owned()).is_some() {
                return Err(PayloadError::BadManifest(format!(
                    "line {number}: duplicate entry `{path}`"
                )));
            }
        }
    }
    let build_id =
        build_id.ok_or_else(|| PayloadError::BadManifest("missing build-id line".to_owned()))?;
    let version =
        version.ok_or_else(|| PayloadError::BadManifest("missing version line".to_owned()))?;
    for required in [REQUIRED_BINARY, REQUIRED_DATABASE] {
        if !entries.contains_key(required) {
            return Err(PayloadError::MissingFile(required.to_owned()));
        }
    }
    Ok(ParsedManifest {
        build_id,
        version,
        entries,
    })
}

fn io_err(path: &str, source: io::Error) -> PayloadError {
    PayloadError::Io {
        path: path.to_owned(),
        source,
    }
}

fn check_dir_meta(
    meta: &fs::Metadata,
    shown: &str,
    enforce_owner: bool,
) -> Result<(), PayloadError> {
    let unsafe_ = |reason: &str| PayloadError::Unsafe {
        path: shown.to_owned(),
        reason: reason.to_owned(),
    };
    if meta.mode() & 0o7000 != 0 && meta.mode() & 0o1000 == 0 {
        return Err(unsafe_("setuid/setgid directory"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(unsafe_("directory writable by group or others"));
    }
    if enforce_owner && meta.uid() != 0 {
        return Err(unsafe_("not owned by root"));
    }
    Ok(())
}

fn check_file_meta(
    meta: &fs::Metadata,
    shown: &str,
    enforce_owner: bool,
) -> Result<(), PayloadError> {
    let unsafe_ = |reason: &str| PayloadError::Unsafe {
        path: shown.to_owned(),
        reason: reason.to_owned(),
    };
    if !meta.is_file() {
        return Err(unsafe_("not a regular file"));
    }
    if meta.mode() & 0o7000 != 0 {
        return Err(unsafe_("setuid, setgid or sticky bit set"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(unsafe_("writable by group or others"));
    }
    if enforce_owner && meta.uid() != 0 {
        return Err(unsafe_("not owned by root"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(PayloadError::TooLarge(MAX_FILE_BYTES));
    }
    Ok(())
}

/// Walk `root` and return every regular file as a `/`-joined relative path.
///
/// # Errors
/// Any symlink, special file, or unsafe directory/file mode is an error.
pub fn scan_tree(root: &Path, enforce_owner: bool) -> Result<BTreeSet<String>, PayloadError> {
    fn walk(
        dir: &Path,
        prefix: &str,
        depth: usize,
        enforce_owner: bool,
        out: &mut BTreeSet<String>,
    ) -> Result<(), PayloadError> {
        if depth > MAX_DEPTH {
            return Err(PayloadError::Unsafe {
                path: prefix.to_owned(),
                reason: "too deeply nested".to_owned(),
            });
        }
        let entries = fs::read_dir(dir).map_err(|e| io_err(prefix, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(prefix, e))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(PayloadError::Unsafe {
                    path: prefix.to_owned(),
                    reason: "non-UTF-8 file name".to_owned(),
                });
            };
            let rel = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let meta = fs::symlink_metadata(entry.path()).map_err(|e| io_err(&rel, e))?;
            if meta.file_type().is_symlink() {
                return Err(PayloadError::Unsafe {
                    path: rel,
                    reason: "symbolic link".to_owned(),
                });
            }
            if meta.is_dir() {
                check_dir_meta(&meta, &rel, enforce_owner)?;
                walk(&entry.path(), &rel, depth + 1, enforce_owner, out)?;
            } else {
                check_file_meta(&meta, &rel, enforce_owner)?;
                out.insert(rel);
            }
        }
        Ok(())
    }
    let meta = fs::symlink_metadata(root).map_err(|e| io_err(&root.display().to_string(), e))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(PayloadError::Unsafe {
            path: root.display().to_string(),
            reason: "payload root is not a plain directory".to_owned(),
        });
    }
    check_dir_meta(&meta, &root.display().to_string(), enforce_owner)?;
    let mut out = BTreeSet::new();
    walk(root, "", 0, enforce_owner, &mut out)?;
    Ok(out)
}

/// Read one regular file below `root` without following symlinks at any component.
fn read_checked(root: &Path, rel: &str, enforce_owner: bool) -> Result<Vec<u8>, PayloadError> {
    let mut current = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current.push(part);
        let meta = fs::symlink_metadata(&current).map_err(|e| io_err(rel, e))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(PayloadError::Unsafe {
                path: rel.to_owned(),
                reason: "parent is a symlink or not a directory".to_owned(),
            });
        }
        check_dir_meta(&meta, rel, enforce_owner)?;
    }
    current.push(parts.last().copied().unwrap_or_default());
    let mut file: File = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(&current)
        .map_err(|e| io_err(rel, e))?;
    let meta = file.metadata().map_err(|e| io_err(rel, e))?;
    check_file_meta(&meta, rel, enforce_owner)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_err(rel, e))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(PayloadError::TooLarge(MAX_FILE_BYTES));
    }
    Ok(bytes)
}

/// Read a verified copy of one manifest entry: the returned bytes hash to `expected`.
///
/// # Errors
/// [`PayloadError`] when the file is unsafe, unreadable, or does not match.
pub fn read_verified(
    root: &Path,
    rel: &str,
    expected: &str,
    enforce_owner: bool,
) -> Result<Vec<u8>, PayloadError> {
    let bytes = read_checked(root, rel, enforce_owner)?;
    if sha256_hex(&bytes) != expected {
        return Err(PayloadError::HashMismatch(rel.to_owned()));
    }
    Ok(bytes)
}

/// Verify a whole tree (payload source, staged copy, or installed release) against a manifest:
/// the file set must be exactly the entries plus `MANIFEST.sha256`, and every hash must match.
///
/// # Errors
/// [`PayloadError`] describing the first problem found.
pub fn verify_tree(
    root: &Path,
    manifest_bytes: &[u8],
    manifest: &ParsedManifest,
    enforce_owner: bool,
) -> Result<Vec<PayloadFile>, PayloadError> {
    let present = scan_tree(root, enforce_owner)?;
    for path in &present {
        if path != MANIFEST_NAME && !manifest.entries.contains_key(path) {
            return Err(PayloadError::Unlisted(path.clone()));
        }
    }
    let on_disk_manifest = read_checked(root, MANIFEST_NAME, enforce_owner)?;
    if on_disk_manifest != manifest_bytes {
        return Err(PayloadError::BadManifest(
            "MANIFEST.sha256 differs from the one this release was verified with".to_owned(),
        ));
    }
    let mut total = 0_u64;
    let mut files = Vec::with_capacity(manifest.entries.len());
    for (path, hash) in &manifest.entries {
        if !present.contains(path) {
            return Err(PayloadError::MissingFile(path.clone()));
        }
        let bytes = read_verified(root, path, hash, enforce_owner)?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL_BYTES {
            return Err(PayloadError::TooLarge(MAX_TOTAL_BYTES));
        }
        files.push(PayloadFile {
            path: path.clone(),
            sha256: hash.clone(),
            size: bytes.len() as u64,
            executable: path.starts_with("bin/"),
        });
    }
    Ok(files)
}

/// Every ancestor of the payload directory must be root-owned and not replaceable by another
/// user (no group/world write, unless it is a sticky directory such as `/tmp`). This blocks a
/// pre-created or swapped AppImage extraction directory with a predictable name.
fn check_ancestors(dir: &Path) -> Result<(), PayloadError> {
    for ancestor in dir.ancestors().skip(1) {
        if ancestor == Path::new("/") || ancestor.as_os_str().is_empty() {
            break;
        }
        let shown = ancestor.display().to_string();
        let meta = fs::symlink_metadata(ancestor).map_err(|e| io_err(&shown, e))?;
        let sticky = meta.mode() & 0o1000 != 0;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(PayloadError::Unsafe {
                path: shown,
                reason: "payload ancestor is a symlink or not a directory".to_owned(),
            });
        }
        if meta.uid() != 0 {
            return Err(PayloadError::Unsafe {
                path: shown,
                reason: "payload ancestor is not owned by root".to_owned(),
            });
        }
        if meta.mode() & 0o022 != 0 && !sticky {
            return Err(PayloadError::Unsafe {
                path: shown,
                reason: "payload ancestor is writable by group or others".to_owned(),
            });
        }
    }
    Ok(())
}

/// Parse the manifest of `source` and verify the whole payload tree.
///
/// # Errors
/// [`PayloadError`] when the payload is missing, malformed, hostile, or corrupt.
pub fn verify_payload(
    source: &PayloadSource,
    enforce_owner: bool,
) -> Result<VerifiedPayload, PayloadError> {
    if !source.dir.is_dir() {
        return Err(PayloadError::Missing(source.dir.display().to_string()));
    }
    if enforce_owner {
        check_ancestors(&source.dir)?;
    }
    let manifest_bytes = read_checked(&source.dir, MANIFEST_NAME, enforce_owner)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(PayloadError::TooLarge(MAX_MANIFEST_BYTES));
    }
    let text = std::str::from_utf8(&manifest_bytes)
        .map_err(|_| PayloadError::BadManifest("not valid UTF-8".to_owned()))?;
    let manifest = parse_manifest(text)?;
    let files = verify_tree(&source.dir, &manifest_bytes, &manifest, enforce_owner)?;
    Ok(VerifiedPayload {
        dir: source.dir.clone(),
        build_id: manifest.build_id,
        version: manifest.version,
        files,
        manifest_bytes,
    })
}
