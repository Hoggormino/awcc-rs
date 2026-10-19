#!/usr/bin/env bash
# Package the staged AWCC Linux payload into a versioned .tar.gz archive and validate an extracted copy.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"

STAGED_DIR="${AWCC_REPO_ROOT}/dist/staging/${AWCC_TARBALL_BASENAME}"
OUT_DIR="${AWCC_REPO_ROOT}/dist"
RUN_SMOKE=1

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
        --skip-smoke)
            RUN_SMOKE=0
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--staged-dir <dir>] [--out-dir <dir>] [--skip-smoke]" >&2
            exit 1
            ;;
    esac
done

if [[ ! -d "${STAGED_DIR}" ]]; then
    "${SCRIPT_DIR}/stage-release-payload.sh" --out-dir "${STAGED_DIR}"
fi

mkdir -p "${OUT_DIR}"
TARBALL_PATH="${OUT_DIR}/${AWCC_TARBALL_NAME}"

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-tarball-XXXXXX")"
cleanup() {
    rm -rf "${WORK_TMP}"
}
trap cleanup EXIT

TREE_ROOT="${WORK_TMP}/${AWCC_TARBALL_BASENAME}"
cp -a "${STAGED_DIR}" "${TREE_ROOT}"
find "${TREE_ROOT}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +

tar \
    --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "${WORK_TMP}" \
    -cf - "${AWCC_TARBALL_BASENAME}" \
    | gzip -n -9 > "${TARBALL_PATH}"

touch -h -d "@${SOURCE_DATE_EPOCH}" "${TARBALL_PATH}"
echo "==> Built binary archive: ${TARBALL_PATH}"

# Validate archive contents, structure, and executable permissions in a fresh directory
EXTRACT_DIR="${WORK_TMP}/extracted"
mkdir -p "${EXTRACT_DIR}"
tar -xzf "${TARBALL_PATH}" -C "${EXTRACT_DIR}"

EXTRACTED_ROOT="${EXTRACT_DIR}/${AWCC_TARBALL_BASENAME}"
for req_exec in "bin/awcc-cli" "bin/awcc-gui"; do
    if [[ ! -x "${EXTRACTED_ROOT}/${req_exec}" ]]; then
        echo "ERROR: Extracted archive is missing executable permission on ${req_exec}" >&2
        exit 1
    fi
done

for req_file in \
    "share/awcc/database.json" \
    "share/applications/awcc-gui.desktop" \
    "share/icons/hicolor/scalable/apps/awcc-gui.svg" \
    "share/icons/hicolor/256x256/apps/awcc-gui.png" \
    "share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml" \
    "share/licenses/awcc-rs/LICENSE" \
    "share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md" \
    "share/doc/awcc-rs/README.md" \
    "share/doc/awcc-rs/INSTALL.txt" \
    "LICENSE" \
    "THIRD_PARTY_NOTICES.md" \
    "README.md" \
    "INSTALL.txt"; do
    if [[ ! -s "${EXTRACTED_ROOT}/${req_file}" ]]; then
        echo "ERROR: Extracted archive is missing required file ${req_file}" >&2
        exit 1
    fi
done

if [[ "${RUN_SMOKE}" -eq 1 ]]; then
    "${SCRIPT_DIR}/smoke-test-installed.sh" \
        --cli "${EXTRACTED_ROOT}/bin/awcc-cli" \
        --gui "${EXTRACTED_ROOT}/bin/awcc-gui" \
        --database "${EXTRACTED_ROOT}/share/awcc/database.json" \
        --label "tarball" \
        --screenshot-dir "${OUT_DIR}/smoke-screenshots"
fi

echo "==> Validated binary archive: ${TARBALL_PATH}"
