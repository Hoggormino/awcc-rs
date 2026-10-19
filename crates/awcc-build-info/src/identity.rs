//! Pure identity parsing helpers shared by `build.rs` and the library.
//!
//! This file is compiled twice: once as a module of the build script (which resolves the
//! identity before compilation) and once as a module of the library (which renders it). Keeping
//! it free of I/O makes every precedence rule unit-testable.
#![allow(dead_code)]

/// Where the embedded source revision came from, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitSource {
    /// Explicit `AWCC_GIT_SHA` supplied by the release scripts (`scripts/release-meta.sh`).
    Env,
    /// `git rev-parse HEAD` inside the workspace work tree (including linked worktrees).
    Git,
    /// A `git archive` / forge source tarball carrying a substituted `.git_archival.txt`.
    GitArchive,
    /// `GITHUB_SHA` from CI, used only when neither of the above is available.
    GithubEnv,
    /// No trustworthy revision information was available at build time.
    Unknown,
}

impl CommitSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Git => "git",
            Self::GitArchive => "git-archive",
            Self::GithubEnv => "github-env",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn from_token(token: &str) -> Self {
        match token {
            "env" => Self::Env,
            "git" => Self::Git,
            "git-archive" => Self::GitArchive,
            "github-env" => Self::GithubEnv,
            _ => Self::Unknown,
        }
    }
}

/// Whether tracked files differed from the recorded revision when the binary was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyState {
    Clean,
    Dirty,
    /// Not determinable (source archive, or git unavailable): never reported as clean.
    Unknown,
}

impl DirtyState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "false",
            Self::Dirty => "true",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn from_token(token: &str) -> Self {
        parse_dirty_flag(token)
    }
}

/// Accept a full SHA-1 (40) or SHA-256 (64) hex object id and normalise it to lowercase.
#[must_use]
pub fn normalize_commit(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if (trimmed.len() == 40 || trimmed.len() == 64)
        && trimmed.bytes().all(|b| b.is_ascii_hexdigit())
    {
        Some(trimmed.to_ascii_lowercase())
    } else {
        None
    }
}

/// Parse `AWCC_GIT_DIRTY`-style flags. Anything unrecognised stays `Unknown`.
#[must_use]
pub fn parse_dirty_flag(raw: &str) -> DirtyState {
    match raw.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "clean" | "no" => DirtyState::Clean,
        "1" | "true" | "dirty" | "yes" => DirtyState::Dirty,
        _ => DirtyState::Unknown,
    }
}

/// Extract the substituted commit from a `.git_archival.txt` body.
///
/// Returns `None` when the file still contains the unexpanded `$Format:%H$` placeholder (i.e. it
/// was read from a plain checkout rather than a `git archive`).
#[must_use]
pub fn parse_archival_commit(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let value = line.strip_prefix("commit:")?.trim();
        if value.starts_with('$') {
            None
        } else {
            normalize_commit(value)
        }
    })
}

/// Extract the substituted commit timestamp from a `.git_archival.txt` body.
#[must_use]
pub fn parse_archival_epoch(body: &str) -> Option<u64> {
    body.lines().find_map(|line| {
        let value = line.strip_prefix("commit-date:")?.trim();
        if value.starts_with('$') {
            None
        } else {
            value.parse().ok()
        }
    })
}

/// Decide the dirty state from the strongest available evidence.
///
/// `explicit` is the `AWCC_GIT_DIRTY` override, `git_status_dirty` is the result of
/// `git status --porcelain --untracked-files=no` (when the commit is known to be the checked-out
/// `HEAD`). Archives and unknown sources never claim to be clean.
#[must_use]
pub fn resolve_dirty(explicit: Option<&str>, git_status_dirty: Option<bool>) -> DirtyState {
    if let Some(flag) = explicit {
        return parse_dirty_flag(flag);
    }
    match git_status_dirty {
        Some(true) => DirtyState::Dirty,
        Some(false) => DirtyState::Clean,
        None => DirtyState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "2e350363fb1830a8449edd56bdd491f6c0ba2563";

    #[test]
    fn commit_normalisation_accepts_only_full_hex_object_ids() {
        assert_eq!(normalize_commit(SHA).as_deref(), Some(SHA));
        assert_eq!(
            normalize_commit(&format!("  {}\n", SHA.to_ascii_uppercase())).as_deref(),
            Some(SHA)
        );
        assert!(
            normalize_commit("2e35036").is_none(),
            "abbreviations are rejected"
        );
        assert!(normalize_commit("").is_none());
        assert!(normalize_commit(&"g".repeat(40)).is_none());
        assert!(normalize_commit(&"a".repeat(64)).is_some());
    }

    #[test]
    fn dirty_flags_never_default_to_clean() {
        assert_eq!(parse_dirty_flag("0"), DirtyState::Clean);
        assert_eq!(parse_dirty_flag("false"), DirtyState::Clean);
        assert_eq!(parse_dirty_flag("1"), DirtyState::Dirty);
        assert_eq!(parse_dirty_flag("TRUE"), DirtyState::Dirty);
        assert_eq!(parse_dirty_flag(""), DirtyState::Unknown);
        assert_eq!(parse_dirty_flag("maybe"), DirtyState::Unknown);
    }

    #[test]
    fn explicit_dirty_override_beats_git_status() {
        assert_eq!(resolve_dirty(Some("1"), Some(false)), DirtyState::Dirty);
        assert_eq!(resolve_dirty(Some("0"), Some(true)), DirtyState::Clean);
        assert_eq!(resolve_dirty(None, Some(true)), DirtyState::Dirty);
        assert_eq!(resolve_dirty(None, Some(false)), DirtyState::Clean);
        assert_eq!(resolve_dirty(None, None), DirtyState::Unknown);
        assert_eq!(
            resolve_dirty(Some("garbage"), Some(false)),
            DirtyState::Unknown
        );
    }

    #[test]
    fn archival_file_ignores_unexpanded_placeholders() {
        let raw = "commit: $Format:%H$\ncommit-date: $Format:%ct$\n";
        assert!(parse_archival_commit(raw).is_none());
        assert!(parse_archival_epoch(raw).is_none());

        let expanded = format!("commit: {SHA}\ncommit-date: 1760000000\n");
        assert_eq!(parse_archival_commit(&expanded).as_deref(), Some(SHA));
        assert_eq!(parse_archival_epoch(&expanded), Some(1_760_000_000));
    }

    #[test]
    fn source_tokens_roundtrip() {
        for src in [
            CommitSource::Env,
            CommitSource::Git,
            CommitSource::GitArchive,
            CommitSource::GithubEnv,
            CommitSource::Unknown,
        ] {
            assert_eq!(CommitSource::from_token(src.as_str()), src);
        }
        assert_eq!(CommitSource::from_token("???"), CommitSource::Unknown);
    }
}
