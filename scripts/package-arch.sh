#!/usr/bin/env bash
# Generate the versioned Arch Linux PKGBUILD and .SRCINFO (pointing at the final GitHub release tarball URL
# and its real SHA-256 checksum), pre-populate SRCDEST with the finalized tarball, and build + validate
# the .pkg.tar.zst package with makepkg prior to release publication.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"

OUT_DIR="${AWCC_REPO_ROOT}/dist"
TARBALL_PATH=""
RUN_SMOKE=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out-dir)
            OUT_DIR="$2"
            shift 2
            ;;
        --tarball)
            TARBALL_PATH="$2"
            shift 2
            ;;
        --skip-smoke)
            RUN_SMOKE=0
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--out-dir <dir>] [--tarball <path>] [--skip-smoke]" >&2
            exit 1
            ;;
    esac
done

if ! command -v makepkg >/dev/null 2>&1; then
    echo "ERROR: makepkg is required to build the Arch Linux package and .SRCINFO." >&2
    exit 1
fi

mkdir -p "${OUT_DIR}"
if [[ -z "${TARBALL_PATH}" ]]; then
    TARBALL_PATH="${OUT_DIR}/${AWCC_TARBALL_NAME}"
fi

if [[ ! -f "${TARBALL_PATH}" ]]; then
    echo "==> Finalized release tarball not found at ${TARBALL_PATH}; building it first..."
    "${SCRIPT_DIR}/package-tarball.sh" --out-dir "${OUT_DIR}" --skip-smoke
fi

TARBALL_SHA256="$(sha256sum "${TARBALL_PATH}" | awk '{print $1}')"
if [[ ! "${TARBALL_SHA256}" =~ ^[0-9a-f]{64}$ ]]; then
    echo "ERROR: Failed to compute valid SHA-256 checksum for ${TARBALL_PATH}" >&2
    exit 1
fi

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-arch-XXXXXX")"
cleanup() {
    rm -rf "${WORK_TMP}"
}
trap cleanup EXIT

sed \
    -e "s/@PKGVER@/${AWCC_VERSION}/g" \
    -e "s/@PKGREL@/${AWCC_PKGREL}/g" \
    -e "s/@TARBALL_SHA256@/${TARBALL_SHA256}/g" \
    "${AWCC_REPO_ROOT}/packaging/linux/PKGBUILD.in" > "${WORK_TMP}/PKGBUILD"

# Verify PKGBUILD never uses SKIP and points at the final versioned GitHub release asset URL
EXPECTED_SOURCE_URL="https://github.com/Hoggormino/awcc-rs/releases/download/v${AWCC_VERSION}/${AWCC_TARBALL_NAME}"
if grep -qi "SKIP" "${WORK_TMP}/PKGBUILD"; then
    echo "ERROR: PKGBUILD contains 'SKIP' in checksums" >&2
    exit 1
fi

# Pre-populate SRCDEST with the finalized tarball so makepkg verifies its real SHA-256 checksum
# before the GitHub Release is published.
cp -f "${TARBALL_PATH}" "${WORK_TMP}/${AWCC_TARBALL_NAME}"

(
    cd "${WORK_TMP}"
    makepkg --printsrcinfo > "${WORK_TMP}/.SRCINFO"
    SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}" PKGDEST="${WORK_TMP}" SRCDEST="${WORK_TMP}" BUILDDIR="${WORK_TMP}/build" \
        PKGEXT='.pkg.tar.zst' \
        makepkg --nodeps --force --cleanbuild --noconfirm
)

# Validate generated .SRCINFO
grep -q "pkgbase = awcc-rs" "${WORK_TMP}/.SRCINFO"
grep -q "pkgver = ${AWCC_VERSION}" "${WORK_TMP}/.SRCINFO"
grep -q "pkgrel = ${AWCC_PKGREL}" "${WORK_TMP}/.SRCINFO"
grep -q "arch = x86_64" "${WORK_TMP}/.SRCINFO"
grep -F "source = ${EXPECTED_SOURCE_URL}" "${WORK_TMP}/.SRCINFO" >/dev/null
grep -F "sha256sums = ${TARBALL_SHA256}" "${WORK_TMP}/.SRCINFO" >/dev/null

BUILT_PKG="${WORK_TMP}/${AWCC_ARCH_PKG_NAME}"
if [[ ! -f "${BUILT_PKG}" ]]; then
    echo "ERROR: makepkg did not produce expected package ${BUILT_PKG}" >&2
    exit 1
fi

cp -f "${WORK_TMP}/PKGBUILD" "${OUT_DIR}/PKGBUILD"
cp -f "${WORK_TMP}/.SRCINFO" "${OUT_DIR}/.SRCINFO"
cp -f "${BUILT_PKG}" "${OUT_DIR}/${AWCC_ARCH_PKG_NAME}"
touch -h -d "@${SOURCE_DATE_EPOCH}" "${OUT_DIR}/PKGBUILD" "${OUT_DIR}/.SRCINFO" "${OUT_DIR}/${AWCC_ARCH_PKG_NAME}"

# Inspect .pkg.tar.zst contents and verify installed layout
EXTRACT_PKG="${WORK_TMP}/pkg-extract"
mkdir -p "${EXTRACT_PKG}"
tar --zstd -xf "${OUT_DIR}/${AWCC_ARCH_PKG_NAME}" -C "${EXTRACT_PKG}"

grep -q "pkgname = awcc-rs" "${EXTRACT_PKG}/.PKGINFO"
grep -q "pkgver = ${AWCC_VERSION}-${AWCC_PKGREL}" "${EXTRACT_PKG}/.PKGINFO"
grep -q "arch = x86_64" "${EXTRACT_PKG}/.PKGINFO"
grep -q "depend = libusb" "${EXTRACT_PKG}/.PKGINFO"
grep -q "depend = fontconfig" "${EXTRACT_PKG}/.PKGINFO"

for req_exec in "usr/bin/awcc-cli" "usr/bin/awcc-gui"; do
    if [[ ! -x "${EXTRACT_PKG}/${req_exec}" ]]; then
        echo "ERROR: Arch package missing executable ${req_exec}" >&2
        exit 1
    fi
done
for req_file in \
    "etc/awcc/database.json" \
    "usr/share/awcc/database.json" \
    "usr/share/applications/awcc-gui.desktop" \
    "usr/share/icons/hicolor/scalable/apps/awcc-gui.svg" \
    "usr/share/icons/hicolor/256x256/apps/awcc-gui.png" \
    "usr/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml" \
    "usr/share/licenses/awcc-rs/LICENSE" \
    "usr/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md"; do
    if [[ ! -s "${EXTRACT_PKG}/${req_file}" ]]; then
        echo "ERROR: Arch package missing required file ${req_file}" >&2
        exit 1
    fi
done

if [[ "${RUN_SMOKE}" -eq 1 ]]; then
    "${SCRIPT_DIR}/smoke-test-installed.sh" \
        --cli "${EXTRACT_PKG}/usr/bin/awcc-cli" \
        --gui "${EXTRACT_PKG}/usr/bin/awcc-gui" \
        --database "${EXTRACT_PKG}/etc/awcc/database.json" \
        --label "arch-pkg" \
        --screenshot-dir "${OUT_DIR}/smoke-screenshots"
fi

echo "==> Built and verified Arch Linux artifacts:"
echo "    - ${OUT_DIR}/${AWCC_ARCH_PKG_NAME}"
echo "    - ${OUT_DIR}/PKGBUILD"
echo "    - ${OUT_DIR}/.SRCINFO"
