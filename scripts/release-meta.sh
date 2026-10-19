#!/usr/bin/env bash
# Central metadata resolver and tag/version validator for awcc-rs build & release scripts.
set -euo pipefail

AWCC_REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export AWCC_REPO_ROOT

AWCC_VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' "${AWCC_REPO_ROOT}/Cargo.toml" | head -n 1)"
if [[ -z "${AWCC_VERSION}" ]]; then
    echo "ERROR: Failed to extract workspace version from ${AWCC_REPO_ROOT}/Cargo.toml" >&2
    exit 1
fi
export AWCC_VERSION

AWCC_TARGET="${AWCC_TARGET:-x86_64-unknown-linux-gnu}"
AWCC_ARCH="x86_64"
AWCC_DEB_ARCH="amd64"
AWCC_PKGREL="${AWCC_PKGREL:-1}"
export AWCC_TARGET AWCC_ARCH AWCC_DEB_ARCH AWCC_PKGREL

if [[ -z "${AWCC_GIT_SHA:-}" ]]; then
    if command -v git >/dev/null 2>&1 && git -C "${AWCC_REPO_ROOT}" rev-parse HEAD >/dev/null 2>&1; then
        AWCC_GIT_SHA="$(git -C "${AWCC_REPO_ROOT}" rev-parse HEAD)"
    else
        AWCC_GIT_SHA="${GITHUB_SHA:-unknown}"
    fi
fi
AWCC_GIT_SHORT_SHA="${AWCC_GIT_SHORT_SHA:-${AWCC_GIT_SHA:0:12}}"
if [[ -z "${AWCC_SOURCE_DATE_EPOCH:-}" ]]; then
    if [[ -n "${SOURCE_DATE_EPOCH:-}" ]]; then
        AWCC_SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}"
    elif command -v git >/dev/null 2>&1 && git -C "${AWCC_REPO_ROOT}" log -1 --format=%ct HEAD >/dev/null 2>&1; then
        AWCC_SOURCE_DATE_EPOCH="$(git -C "${AWCC_REPO_ROOT}" log -1 --format=%ct HEAD)"
    else
        AWCC_SOURCE_DATE_EPOCH="1700000000"
    fi
fi
export AWCC_GIT_SHA AWCC_GIT_SHORT_SHA AWCC_SOURCE_DATE_EPOCH
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-${AWCC_SOURCE_DATE_EPOCH}}"

AWCC_RUSTC_VERSION="$(rustc --version 2>/dev/null || echo 'unknown')"
export AWCC_RUSTC_VERSION

# Canonical asset filenames
AWCC_TARBALL_BASENAME="awcc-rs-${AWCC_VERSION}-${AWCC_TARGET}"
AWCC_TARBALL_NAME="${AWCC_TARBALL_BASENAME}.tar.gz"
AWCC_APPIMAGE_NAME="awcc-rs-${AWCC_VERSION}-${AWCC_ARCH}.AppImage"
AWCC_DEB_NAME="awcc-rs_${AWCC_VERSION}_${AWCC_DEB_ARCH}.deb"
AWCC_RPM_NAME="awcc-rs-${AWCC_VERSION}-${AWCC_PKGREL}.${AWCC_ARCH}.rpm"
AWCC_ARCH_PKG_NAME="awcc-rs-${AWCC_VERSION}-${AWCC_PKGREL}-${AWCC_ARCH}.pkg.tar.zst"
export AWCC_TARBALL_BASENAME AWCC_TARBALL_NAME AWCC_APPIMAGE_NAME AWCC_DEB_NAME AWCC_RPM_NAME AWCC_ARCH_PKG_NAME

validate_release_tag() {
    local tag="$1"
    if [[ ! "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        echo "ERROR: Invalid release tag '${tag}'. Only stable tags matching ^v[0-9]+\\.[0-9]+\\.[0-9]+$ are permitted." >&2
        return 1
    fi

    local tag_version="${tag#v}"
    if [[ "${tag_version}" != "${AWCC_VERSION}" ]]; then
        echo "ERROR: Tag version '${tag_version}' (from '${tag}') does not match Cargo.toml workspace version '${AWCC_VERSION}'." >&2
        return 1
    fi

    # Verify all workspace crates agree with AWCC_VERSION in Cargo.lock / cargo metadata
    if command -v cargo >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
        python3 - "${AWCC_REPO_ROOT}" "${AWCC_VERSION}" <<'PY'
import json, subprocess, sys
repo_root, expected = sys.argv[1], sys.argv[2]
out = subprocess.check_output(
    ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
    cwd=repo_root,
    text=True,
)
meta = json.loads(out)
for pkg in meta.get("packages", []):
    if pkg["version"] != expected:
        print(f"ERROR: Crate {pkg['name']} has version {pkg['version']} != {expected}", file=sys.stderr)
        sys.exit(1)
PY
    fi

    if git -C "${AWCC_REPO_ROOT}" rev-parse -q --verify "refs/tags/${tag}" >/dev/null 2>&1; then
        local tag_commit
        tag_commit="$(git -C "${AWCC_REPO_ROOT}" rev-list -n 1 "refs/tags/${tag}")"
        if [[ "${tag_commit}" != "${AWCC_GIT_SHA}" ]]; then
            echo "ERROR: Tag '${tag}' points to commit ${tag_commit}, which does not match checked-out HEAD ${AWCC_GIT_SHA}." >&2
            return 1
        fi
    fi

    echo "Validated release tag ${tag} -> version ${AWCC_VERSION} at commit ${AWCC_GIT_SHA}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        --validate-tag)
            if [[ -z "${2:-}" ]]; then
                echo "Usage: $0 --validate-tag <vX.Y.Z>" >&2
                exit 1
            fi
            validate_release_tag "$2"
            ;;
        --print-env)
            cat <<EOF
AWCC_VERSION=${AWCC_VERSION}
AWCC_TARGET=${AWCC_TARGET}
AWCC_ARCH=${AWCC_ARCH}
AWCC_DEB_ARCH=${AWCC_DEB_ARCH}
AWCC_GIT_SHA=${AWCC_GIT_SHA}
AWCC_GIT_SHORT_SHA=${AWCC_GIT_SHORT_SHA}
AWCC_SOURCE_DATE_EPOCH=${AWCC_SOURCE_DATE_EPOCH}
AWCC_TARBALL_NAME=${AWCC_TARBALL_NAME}
AWCC_APPIMAGE_NAME=${AWCC_APPIMAGE_NAME}
AWCC_DEB_NAME=${AWCC_DEB_NAME}
AWCC_RPM_NAME=${AWCC_RPM_NAME}
AWCC_ARCH_PKG_NAME=${AWCC_ARCH_PKG_NAME}
EOF
            ;;
        *)
            echo "Usage: $0 [--print-env | --validate-tag <vX.Y.Z>]" >&2
            exit 1
            ;;
    esac
fi
