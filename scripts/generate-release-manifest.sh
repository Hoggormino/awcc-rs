#!/usr/bin/env bash
# Verify the complete expected distributable asset set, generate build-manifest.json and SHA256SUMS
# from the final distributable bytes, and verify PKGBUILD checksum agreement.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"

DIST_DIR="${AWCC_REPO_ROOT}/dist"
RELEASE_TAG=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dist-dir)
            DIST_DIR="$2"
            shift 2
            ;;
        --tag)
            RELEASE_TAG="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--dist-dir <dir>] [--tag <vX.Y.Z>]" >&2
            exit 1
            ;;
    esac
done

DIST_DIR="$(realpath -m "${DIST_DIR}")"

if [[ -n "${RELEASE_TAG}" ]]; then
    validate_release_tag "${RELEASE_TAG}"
fi

EXPECTED_DISTRIBUTABLES=(
    "${AWCC_TARBALL_NAME}"
    "${AWCC_APPIMAGE_NAME}"
    "${AWCC_DEB_NAME}"
    "${AWCC_RPM_NAME}"
    "${AWCC_ARCH_PKG_NAME}"
    "PKGBUILD"
    ".SRCINFO"
    "${AWCC_ARCH_INSTALL_NAME}"
)

for asset in "${EXPECTED_DISTRIBUTABLES[@]}"; do
    asset_path="${DIST_DIR}/${asset}"
    if [[ ! -f "${asset_path}" ]]; then
        echo "ERROR: Missing required release asset: ${asset_path}" >&2
        exit 1
    fi
    if [[ ! -s "${asset_path}" ]]; then
        echo "ERROR: Release asset is empty (0 bytes): ${asset_path}" >&2
        exit 1
    fi
done

# Reject unexpected top-level files in DIST_DIR (other than build-manifest.json and SHA256SUMS)
while IFS= read -r -d '' found_path; do
    base="$(basename "${found_path}")"
    case "${base}" in
        "${AWCC_TARBALL_NAME}" | "${AWCC_APPIMAGE_NAME}" | "${AWCC_DEB_NAME}" | "${AWCC_RPM_NAME}" | \
        "${AWCC_ARCH_PKG_NAME}" | "PKGBUILD" | ".SRCINFO" | "${AWCC_ARCH_INSTALL_NAME}" | "build-manifest.json" | "SHA256SUMS")
            ;;
        *)
            echo "ERROR: Unexpected file '${base}' present in release asset directory ${DIST_DIR}" >&2
            exit 1
            ;;
    esac
done < <(find "${DIST_DIR}" -maxdepth 1 -type f -print0)

# Ensure AppImage has executable permission
chmod 0755 "${DIST_DIR}/${AWCC_APPIMAGE_NAME}"

# The shipped binaries must carry the identity this manifest is about. Extract the release tarball
# and ask both executables (in a bare environment) what they were built from; a stale cached
# build, or a dirty tree in a tagged release, fails here instead of being published.
VERIFY_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-manifest-verify-XXXXXX")"
trap 'rm -rf "${VERIFY_TMP}"' EXIT
tar -xzf "${DIST_DIR}/${AWCC_TARBALL_NAME}" -C "${VERIFY_TMP}"
PAYLOAD_ROOT="${VERIFY_TMP}/${AWCC_TARBALL_BASENAME}"
CLI_VERSION_OUT="$(binary_version_output "${PAYLOAD_ROOT}/bin/awcc-cli")"
GUI_VERSION_OUT="$(binary_version_output "${PAYLOAD_ROOT}/bin/awcc-gui")"
for key in version commit commit-source dirty source-date-epoch target profile rustc; do
    if [[ "$(version_field "${CLI_VERSION_OUT}" "${key}")" != "$(version_field "${GUI_VERSION_OUT}" "${key}")" ]]; then
        echo "ERROR: awcc-cli and awcc-gui in ${AWCC_TARBALL_NAME} disagree on ${key}" >&2
        exit 1
    fi
done
# The compiler that actually built the binaries is authoritative for the manifest; this job may
# resolve a different `rustc` than the build job did.
BINARY_RUSTC="$(version_field "${CLI_VERSION_OUT}" rustc)"
if [[ -n "${BINARY_RUSTC}" && "${BINARY_RUSTC}" != "unknown" ]]; then
    AWCC_RUSTC_VERSION="${BINARY_RUSTC}"
fi
# The dirty state describes the build, not this job's checkout.
AWCC_GIT_DIRTY="unknown"
case "$(version_field "${CLI_VERSION_OUT}" dirty)" in
    false) AWCC_GIT_DIRTY="0" ;;
    true) AWCC_GIT_DIRTY="1" ;;
esac
for bin in awcc-cli awcc-gui; do
    assert_binary_identity "${PAYLOAD_ROOT}/bin/${bin}" "${AWCC_TARBALL_NAME}:${bin}"
done
BINARY_DIRTY="$(version_field "${CLI_VERSION_OUT}" dirty)"
BINARY_COMMIT_SOURCE="$(version_field "${CLI_VERSION_OUT}" commit-source)"
if [[ -n "${RELEASE_TAG}" && "${BINARY_DIRTY}" != "false" ]]; then
    echo "ERROR: release ${RELEASE_TAG} binaries report dirty='${BINARY_DIRTY}'; releases must be built from a clean checkout of the tagged commit." >&2
    exit 1
fi
echo "==> Binary identity verified: commit ${AWCC_GIT_SHA} (source: ${BINARY_COMMIT_SOURCE}), dirty=${BINARY_DIRTY}, ${AWCC_RUSTC_VERSION}"

# Generate build-manifest.json
MANIFEST_PATH="${DIST_DIR}/build-manifest.json"
python3 - "${DIST_DIR}" "${MANIFEST_PATH}" "${AWCC_VERSION}" "${RELEASE_TAG:-v${AWCC_VERSION}}" \
    "${AWCC_GIT_SHA}" "${AWCC_SOURCE_DATE_EPOCH}" "${AWCC_TARGET}" "${AWCC_RUSTC_VERSION}" \
    "${BINARY_DIRTY}" "${BINARY_COMMIT_SOURCE}" \
    "${EXPECTED_DISTRIBUTABLES[@]}" <<'PY'
import hashlib, json, os, sys

dist_dir = sys.argv[1]
manifest_path = sys.argv[2]
version = sys.argv[3]
tag = sys.argv[4]
git_sha = sys.argv[5]
source_date_epoch = int(sys.argv[6])
target = sys.argv[7]
rustc_ver = sys.argv[8]
git_dirty = sys.argv[9]
commit_source = sys.argv[10]
assets = sys.argv[11:]

asset_entries = []
for name in assets:
    p = os.path.join(dist_dir, name)
    data = open(p, "rb").read()
    sha256 = hashlib.sha256(data).hexdigest()
    asset_entries.append({
        "name": name,
        "size_bytes": len(data),
        "sha256": sha256,
    })

manifest = {
    "schema_version": 1,
    "package": "awcc-rs",
    "version": version,
    "tag": tag,
    "git_commit": git_sha,
    "git_commit_source": commit_source,
    "git_dirty": git_dirty,
    "source_date_epoch": source_date_epoch,
    "target_triple": target,
    "rustc_version": rustc_ver,
    "runtime_baseline": {
        "common_payload_and_appimage": "Ubuntu 24.04 LTS (x86_64, glibc 2.39)",
        "deb_tested_on": "Ubuntu 24.04 LTS (noble, amd64)",
        "rpm_tested_on": "Fedora Linux 41 (x86_64)",
        "arch_tested_on": "Arch Linux (x86_64)",
    },
    "assets": asset_entries,
}

with open(manifest_path, "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
PY
touch -h -d "@${SOURCE_DATE_EPOCH}" "${MANIFEST_PATH}"

# Compute SHA256SUMS over all distributables + build-manifest.json
SHA256SUMS_PATH="${DIST_DIR}/SHA256SUMS"
(
    cd "${DIST_DIR}"
    sha256sum "${EXPECTED_DISTRIBUTABLES[@]}" "build-manifest.json" > "${SHA256SUMS_PATH}"
    sha256sum -c "${SHA256SUMS_PATH}"
)
touch -h -d "@${SOURCE_DATE_EPOCH}" "${SHA256SUMS_PATH}"

# Verify PKGBUILD and .SRCINFO sha256sums match the tarball checksum in SHA256SUMS
TARBALL_SUM="$(awk -v name="${AWCC_TARBALL_NAME}" '$2 == name { print $1 }' "${SHA256SUMS_PATH}")"
if [[ -z "${TARBALL_SUM}" ]]; then
    echo "ERROR: Could not find ${AWCC_TARBALL_NAME} in ${SHA256SUMS_PATH}" >&2
    exit 1
fi
grep -F "sha256sums=('${TARBALL_SUM}')" "${DIST_DIR}/PKGBUILD" >/dev/null || {
    echo "ERROR: PKGBUILD sha256sums does not match ${AWCC_TARBALL_NAME} checksum (${TARBALL_SUM}) in SHA256SUMS" >&2
    exit 1
}
grep -F "sha256sums = ${TARBALL_SUM}" "${DIST_DIR}/.SRCINFO" >/dev/null || {
    echo "ERROR: .SRCINFO sha256sums does not match ${AWCC_TARBALL_NAME} checksum (${TARBALL_SUM}) in SHA256SUMS" >&2
    exit 1
}

echo "==> Generated and verified release manifest (${MANIFEST_PATH}) and checksums (${SHA256SUMS_PATH}):"
cat "${SHA256SUMS_PATH}"
