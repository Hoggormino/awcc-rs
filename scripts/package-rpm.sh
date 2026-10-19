#!/usr/bin/env bash
# Build and validate the Fedora .rpm package from the staged AWCC Linux payload using rpmbuild.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"

STAGED_DIR="${AWCC_REPO_ROOT}/dist/staging/${AWCC_TARBALL_BASENAME}"
OUT_DIR="${AWCC_REPO_ROOT}/dist"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --staged-dir)
            STAGED_DIR="$2"
            shift 2
            ;;
        --out-dir)
            OUT_DIR="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--staged-dir <dir>] [--out-dir <dir>]" >&2
            exit 1
            ;;
    esac
done

if ! command -v rpmbuild >/dev/null 2>&1; then
    echo "ERROR: rpmbuild is required to build the .rpm package (install rpm-build / rpm)." >&2
    exit 1
fi

STAGED_DIR="$(realpath -m "${STAGED_DIR}")"
OUT_DIR="$(realpath -m "${OUT_DIR}")"

if [[ ! -d "${STAGED_DIR}" ]]; then
    "${SCRIPT_DIR}/stage-release-payload.sh" --out-dir "${STAGED_DIR}"
fi

mkdir -p "${OUT_DIR}"
RPM_PATH="${OUT_DIR}/${AWCC_RPM_NAME}"

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-rpm-XXXXXX")"
cleanup() {
    rm -rf "${WORK_TMP}"
}
trap cleanup EXIT

TOPDIR="${WORK_TMP}/rpmbuild"
mkdir -p "${TOPDIR}"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}

SPEC_FILE="${TOPDIR}/SPECS/awcc-rs.spec"
sed \
    -e "s/@VERSION@/${AWCC_VERSION}/g" \
    -e "s/@RELEASE@/${AWCC_PKGREL}/g" \
    "${AWCC_REPO_ROOT}/packaging/linux/awcc-rs.spec.in" > "${SPEC_FILE}"

SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}" rpmbuild \
    -bb \
    --target "${AWCC_ARCH}" \
    --define "_topdir ${TOPDIR}" \
    --define "staged_root ${STAGED_DIR}" \
    --define "dist %{nil}" \
    --define "source_date_epoch_from_changelog 0" \
    --define "clamp_mtime_to_source_date_epoch 1" \
    --define "use_source_date_epoch_as_buildtime 1" \
    "${SPEC_FILE}"

BUILT_RPM="$(find "${TOPDIR}/RPMS" -type f -name "awcc-rs-${AWCC_VERSION}-*.rpm" | head -n 1)"
if [[ -z "${BUILT_RPM}" || ! -f "${BUILT_RPM}" ]]; then
    echo "ERROR: rpmbuild did not produce an RPM file under ${TOPDIR}/RPMS" >&2
    exit 1
fi

cp -f "${BUILT_RPM}" "${RPM_PATH}"
touch -h -d "@${SOURCE_DATE_EPOCH}" "${RPM_PATH}"

# Validate RPM metadata, dependencies, and file list
if command -v rpm >/dev/null 2>&1; then
    RPM_INFO="$(rpm -qip "${RPM_PATH}" 2>/dev/null)"
    grep -q "^Name        : awcc-rs" <<<"${RPM_INFO}"
    grep -q "^Version     : ${AWCC_VERSION}" <<<"${RPM_INFO}"
    grep -q "^Architecture: ${AWCC_ARCH}" <<<"${RPM_INFO}"

    RPM_REQS="$(rpm -qpR "${RPM_PATH}" 2>/dev/null)"
    grep -q "libusb-1.0.so.0" <<<"${RPM_REQS}"
    grep -q "libfontconfig.so.1" <<<"${RPM_REQS}"

    RPM_FILES="$(rpm -qpl "${RPM_PATH}" 2>/dev/null)"
    grep -qx "/usr/bin/awcc-cli" <<<"${RPM_FILES}"
    grep -qx "/usr/bin/awcc-gui" <<<"${RPM_FILES}"
    grep -qx "/etc/awcc/database.json" <<<"${RPM_FILES}"
    grep -qx "/usr/share/awcc/database.json" <<<"${RPM_FILES}"
    grep -qx "/usr/share/awcc/package-format" <<<"${RPM_FILES}"
    grep -qx "/usr/share/applications/awcc-gui.desktop" <<<"${RPM_FILES}"
    grep -qx "/usr/share/licenses/awcc-rs/LICENSE" <<<"${RPM_FILES}"
    grep -qx "/usr/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md" <<<"${RPM_FILES}"
fi

echo "==> Built and verified Fedora RPM package: ${RPM_PATH}"
