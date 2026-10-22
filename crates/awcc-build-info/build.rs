//! Resolve the build identity before compilation and embed it with `cargo:rustc-env`.
//!
//! Precedence (documented in `docs/operation_outcomes_and_build_identity.md`):
//!
//! 1. `AWCC_GIT_SHA` (+ optional `AWCC_GIT_DIRTY`, `AWCC_SOURCE_DATE_EPOCH`) exported by
//!    `scripts/release-meta.sh`, i.e. the exact inputs of the release manifest generator.
//! 2. `git` inside the workspace work tree (ordinary clones and linked worktrees).
//! 3. A substituted `.git_archival.txt` (`git archive` / forge source tarballs, no `.git`).
//! 4. `GITHUB_SHA` (CI fallback, mirrors `release-meta.sh`).
//! 5. Unknown — reported honestly, never guessed.
//!
//! Only information that exists before compilation is embedded; hashes of the final release
//! artifacts stay in the external `build-manifest.json`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "src/identity.rs"]
mod identity;

use identity::{
    CommitSource, DirtyState, normalize_commit, parse_archival_commit, parse_archival_epoch,
    resolve_dirty,
};

fn git(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_string())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `Some(top_level)` only when `repo_root` is itself the top of a git work tree. A source archive
/// unpacked inside some unrelated repository must not inherit that repository's revision.
fn workspace_git_usable(repo_root: &Path) -> bool {
    git(repo_root, &["rev-parse", "--show-toplevel"])
        .is_some_and(|top| same_dir(Path::new(&top), repo_root))
}

fn absolute(base: &Path, candidate: &str) -> PathBuf {
    let path = PathBuf::from(candidate);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// Re-run when the checked-out revision moves: `HEAD`, the current branch ref, `packed-refs`,
/// and the loose-ref directory (a first commit on a packed branch creates a new loose ref).
fn watch_git_state(repo_root: &Path) {
    let Some(git_dir) = git(repo_root, &["rev-parse", "--git-dir"]) else {
        return;
    };
    let git_dir = absolute(repo_root, &git_dir);
    let common_dir = git(repo_root, &["rev-parse", "--git-common-dir"])
        .map_or_else(|| git_dir.clone(), |c| absolute(repo_root, &c));

    for path in [
        git_dir.join("HEAD"),
        common_dir.join("packed-refs"),
        common_dir.join("refs").join("heads"),
        common_dir.join("refs").join("tags"),
    ] {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Ok(head) = fs::read_to_string(git_dir.join("HEAD"))
        && let Some(branch_ref) = head.trim().strip_prefix("ref:")
    {
        let loose = common_dir.join(branch_ref.trim());
        if loose.exists() {
            println!("cargo:rerun-if-changed={}", loose.display());
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn resolve_epoch(git_ok: bool, repo_root: &Path, archival_epoch: Option<u64>) -> Option<u64> {
    env_nonempty("AWCC_SOURCE_DATE_EPOCH")
        .or_else(|| env_nonempty("SOURCE_DATE_EPOCH"))
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| {
            if git_ok {
                git(repo_root, &["log", "-1", "--format=%ct", "HEAD"]).and_then(|v| v.parse().ok())
            } else {
                None
            }
        })
        .or(archival_epoch)
}

fn rustc_identity() -> String {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let repo_root = manifest_dir.join("../..");

    for key in [
        "AWCC_GIT_SHA",
        "AWCC_GIT_DIRTY",
        "AWCC_SOURCE_DATE_EPOCH",
        "SOURCE_DATE_EPOCH",
        "GITHUB_SHA",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    // Any edit to a tracked input can change the dirty state, so re-resolve identity then too.
    for rel in [
        "crates",
        "ui-libraries",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".git_archival.txt",
    ] {
        let path = repo_root.join(rel);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let git_ok = workspace_git_usable(&repo_root);
    if git_ok {
        watch_git_state(&repo_root);
    }
    let git_head = if git_ok {
        git(&repo_root, &["rev-parse", "HEAD"]).and_then(|h| normalize_commit(&h))
    } else {
        None
    };

    let archival_body = fs::read_to_string(repo_root.join(".git_archival.txt")).ok();
    let archival_commit = archival_body.as_deref().and_then(parse_archival_commit);
    let archival_epoch = archival_body.as_deref().and_then(parse_archival_epoch);

    let explicit = env_nonempty("AWCC_GIT_SHA");
    let explicit_valid = explicit.as_deref().and_then(normalize_commit);
    if explicit.is_some() && explicit_valid.is_none() {
        println!(
            "cargo:warning=ignoring AWCC_GIT_SHA: expected a full 40/64 character hex commit id"
        );
    }

    let (commit, source) = if let Some(c) = explicit_valid {
        (Some(c), CommitSource::Env)
    } else if let Some(c) = git_head.clone() {
        (Some(c), CommitSource::Git)
    } else if let Some(c) = archival_commit {
        (Some(c), CommitSource::GitArchive)
    } else if let Some(c) = env_nonempty("GITHUB_SHA").and_then(|v| normalize_commit(&v)) {
        (Some(c), CommitSource::GithubEnv)
    } else {
        (None, CommitSource::Unknown)
    };

    // `git status` only describes the recorded revision when it *is* the checked-out HEAD; an
    // explicit `AWCC_GIT_SHA` that disagrees with HEAD must not inherit HEAD's clean/dirty state.
    let git_status_dirty = match (&commit, &git_head, source) {
        (Some(c), Some(h), CommitSource::Git | CommitSource::Env) if c == h => git(
            &repo_root,
            &["status", "--porcelain", "--untracked-files=no"],
        )
        .map(|out| !out.is_empty()),
        _ => None,
    };
    let dirty_override = env_nonempty("AWCC_GIT_DIRTY");
    let dirty = if commit.is_none() {
        DirtyState::Unknown
    } else {
        resolve_dirty(dirty_override.as_deref(), git_status_dirty)
    };

    let epoch = resolve_epoch(git_ok && git_head.is_some(), &repo_root, archival_epoch);

    let emit = |key: &str, value: &str| println!("cargo:rustc-env={key}={value}");
    emit("AWCC_BUILD_COMMIT", commit.as_deref().unwrap_or(""));
    emit("AWCC_BUILD_COMMIT_SOURCE", source.as_str());
    emit("AWCC_BUILD_DIRTY", dirty.as_str());
    emit(
        "AWCC_BUILD_SOURCE_DATE_EPOCH",
        &epoch.map(|e| e.to_string()).unwrap_or_default(),
    );
    emit("AWCC_BUILD_TARGET", &env::var("TARGET").unwrap_or_default());
    emit(
        "AWCC_BUILD_PROFILE",
        &env::var("PROFILE").unwrap_or_default(),
    );
    emit("AWCC_BUILD_RUSTC", &rustc_identity());
}
