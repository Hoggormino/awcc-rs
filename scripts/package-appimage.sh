#!/usr/bin/env bash
# Assemble AppDir, bundle non-base runtime dependencies (respecting AppImage base/driver exclusions),
# build the x86_64 AppImage with pinned checksum-verified appimagetool 1.9.1, and smoke-test via --appimage-extract.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"

STAGED_DIR="${AWCC_REPO_ROOT}/dist/staging/${AWCC_TARBALL_BASENAME}"
OUT_DIR="${AWCC_REPO_ROOT}/dist"
TOOLS_CACHE_DIR="${AWCC_TOOLS_CACHE_DIR:-${AWCC_REPO_ROOT}/target/tools-cache}"
RUN_SMOKE=1

APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"

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
        --tools-cache-dir)
            TOOLS_CACHE_DIR="$2"
            shift 2
            ;;
        --skip-smoke)
            RUN_SMOKE=0
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--staged-dir <dir>] [--out-dir <dir>] [--tools-cache-dir <dir>] [--skip-smoke]" >&2
            exit 1
            ;;
    esac
done

STAGED_DIR="$(realpath -m "${STAGED_DIR}")"
OUT_DIR="$(realpath -m "${OUT_DIR}")"
TOOLS_CACHE_DIR="$(realpath -m "${TOOLS_CACHE_DIR}")"

if [[ ! -d "${STAGED_DIR}" ]]; then
    "${SCRIPT_DIR}/stage-release-payload.sh" --out-dir "${STAGED_DIR}"
fi

mkdir -p "${OUT_DIR}" "${TOOLS_CACHE_DIR}"
APPIMAGE_PATH="${OUT_DIR}/${AWCC_APPIMAGE_NAME}"

# Ensure pinned appimagetool 1.9.1 is present and SHA-256 verified
APPIMAGETOOL_BIN="${TOOLS_CACHE_DIR}/appimagetool-${APPIMAGETOOL_VERSION}-x86_64.AppImage"
if [[ ! -x "${APPIMAGETOOL_BIN}" ]]; then
    echo "==> Downloading appimagetool ${APPIMAGETOOL_VERSION} from ${APPIMAGETOOL_URL}..."
    curl -fsSL -o "${APPIMAGETOOL_BIN}.tmp" "${APPIMAGETOOL_URL}"
    echo "${APPIMAGETOOL_SHA256}  ${APPIMAGETOOL_BIN}.tmp" | sha256sum -c -
    chmod 0755 "${APPIMAGETOOL_BIN}.tmp"
    mv "${APPIMAGETOOL_BIN}.tmp" "${APPIMAGETOOL_BIN}"
else
    echo "${APPIMAGETOOL_SHA256}  ${APPIMAGETOOL_BIN}" | sha256sum -c - >/dev/null
fi

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-appimage-XXXXXX")"
cleanup() {
    rm -rf "${WORK_TMP}"
}
trap cleanup EXIT

APPDIR="${WORK_TMP}/AppDir"
mkdir -p \
    "${APPDIR}/usr/bin" \
    "${APPDIR}/usr/lib" \
    "${APPDIR}/usr/share/awcc" \
    "${APPDIR}/usr/share/applications" \
    "${APPDIR}/usr/share/icons/hicolor/scalable/apps" \
    "${APPDIR}/usr/share/icons/hicolor/256x256/apps" \
    "${APPDIR}/usr/share/metainfo" \
    "${APPDIR}/usr/share/licenses/awcc-rs" \
    "${APPDIR}/usr/share/doc/awcc-rs"

install -m 0755 "${AWCC_REPO_ROOT}/packaging/linux/AppRun" "${APPDIR}/AppRun"
install -m 0644 "${STAGED_DIR}/share/applications/awcc-gui.desktop" "${APPDIR}/awcc-gui.desktop"
install -m 0644 "${STAGED_DIR}/share/icons/hicolor/scalable/apps/awcc-gui.svg" "${APPDIR}/awcc-gui.svg"
install -m 0644 "${STAGED_DIR}/share/icons/hicolor/256x256/apps/awcc-gui.png" "${APPDIR}/awcc-gui.png"
ln -sf "awcc-gui.png" "${APPDIR}/.DirIcon"

install -m 0755 "${STAGED_DIR}/bin/awcc-cli" "${APPDIR}/usr/bin/awcc-cli"
install -m 0755 "${STAGED_DIR}/bin/awcc-gui" "${APPDIR}/usr/bin/awcc-gui"
install -m 0644 "${STAGED_DIR}/share/awcc/database.json" "${APPDIR}/usr/share/awcc/database.json"
printf 'appimage\n' > "${APPDIR}/usr/share/awcc/package-format"
chmod 0644 "${APPDIR}/usr/share/awcc/package-format"
install -m 0644 "${STAGED_DIR}/share/applications/awcc-gui.desktop" "${APPDIR}/usr/share/applications/awcc-gui.desktop"
install -m 0644 "${STAGED_DIR}/share/icons/hicolor/scalable/apps/awcc-gui.svg" "${APPDIR}/usr/share/icons/hicolor/scalable/apps/awcc-gui.svg"
install -m 0644 "${STAGED_DIR}/share/icons/hicolor/256x256/apps/awcc-gui.png" "${APPDIR}/usr/share/icons/hicolor/256x256/apps/awcc-gui.png"
install -m 0644 "${STAGED_DIR}/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml" "${APPDIR}/usr/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml"
install -m 0644 "${STAGED_DIR}/share/licenses/awcc-rs/LICENSE" "${APPDIR}/usr/share/licenses/awcc-rs/LICENSE"
install -m 0644 "${STAGED_DIR}/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md" "${APPDIR}/usr/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md"
install -m 0644 "${STAGED_DIR}/share/doc/awcc-rs/README.md" "${APPDIR}/usr/share/doc/awcc-rs/README.md"
install -m 0644 "${STAGED_DIR}/share/doc/awcc-rs/INSTALL.txt" "${APPDIR}/usr/share/doc/awcc-rs/INSTALL.txt"

# AppImage base-library & host GPU/display driver excludelist per https://docs.appimage.org/reference/best-practices.html
is_excluded_appimage_lib() {
    local soname="$1"
    case "${soname}" in
        ld-linux*.so* | libc.so* | libm.so* | libdl.so* | libpthread.so* | librt.so* | libresolv.so* | libutil.so* | libnsl.so* | \
        libgcc_s.so* | libstdc++.so* | libudev.so* | libsystemd.so* | \
        libfontconfig.so* | libfreetype.so* | libharfbuzz*.so* | libexpat.so* | libz.so* | libbz2.so* | \
        libGL.so* | libEGL.so* | libGLX*.so* | libOpenGL.so* | libGLdispatch.so* | libGLES*.so* | \
        libdrm*.so* | libgbm.so* | libvulkan.so* | \
        libX11.so* | libX11-xcb.so* | libxcb.so* | libxcb-dri*.so* | libxcb-glx.so* | libxcb-present.so* | libxcb-sync.so* | libxcb-shm.so* | libXau.so* | libXdmcp.so* | \
        libwayland-client.so* | libwayland-egl.so* | libwayland-cursor.so* | libwayland-server.so*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

resolve_lib_path() {
    local soname="$1"
    local resolved=""
    resolved="$(ldd "${STAGED_DIR}/bin/awcc-gui" "${STAGED_DIR}/bin/awcc-cli" 2>/dev/null \
        | awk -v target="${soname}" '$1 == target && $3 ~ /^\// { print $3; exit }')"
    if [[ -z "${resolved}" ]] && command -v ldconfig >/dev/null 2>&1; then
        resolved="$(ldconfig -p 2>/dev/null \
            | awk -v target="${soname}" '$1 == target && $(NF) ~ /^\// { print $(NF); exit }')"
    fi
    if [[ -z "${resolved}" ]]; then
        for dir in /lib/x86_64-linux-gnu /usr/lib/x86_64-linux-gnu /usr/lib64 /usr/lib; do
            if [[ -e "${dir}/${soname}" ]]; then
                resolved="${dir}/${soname}"
                break
            fi
        done
    fi
    printf '%s' "${resolved}"
}

# Bundle direct non-base dependencies (libusb-1.0.so.0) and winit dlopen'd non-base keyboard/cursor/input libs
NON_BASE_CANDIDATES=(
    "libusb-1.0.so.0"
    "libxkbcommon.so.0"
    "libxkbcommon-x11.so.0"
    "libxcb-xkb.so.1"
    "libXcursor.so.1"
    "libXi.so.6"
    "libXrender.so.1"
    "libXfixes.so.3"
    "libXext.so.6"
)

for soname in "${NON_BASE_CANDIDATES[@]}"; do
    if is_excluded_appimage_lib "${soname}"; then
        continue
    fi
    lib_path="$(resolve_lib_path "${soname}")"
    if [[ -n "${lib_path}" && -f "${lib_path}" ]]; then
        cp -L "${lib_path}" "${APPDIR}/usr/lib/${soname}"
        chmod 0644 "${APPDIR}/usr/lib/${soname}"
        echo "==> Bundled non-base shared library into AppDir: ${soname} (${lib_path})"
    elif [[ "${soname}" == "libusb-1.0.so.0" ]]; then
        echo "ERROR: Required runtime library ${soname} could not be resolved on build host" >&2
        exit 1
    fi
done

find "${APPDIR}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +

echo "==> Building AppImage ${APPIMAGE_PATH} with appimagetool ${APPIMAGETOOL_VERSION}..."
rm -f "${APPIMAGE_PATH}"
ARCH="${AWCC_ARCH}" SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}" APPIMAGE_EXTRACT_AND_RUN=1 \
    "${APPIMAGETOOL_BIN}" --no-appstream "${APPDIR}" "${APPIMAGE_PATH}"
chmod 0755 "${APPIMAGE_PATH}"
touch -h -d "@${SOURCE_DATE_EPOCH}" "${APPIMAGE_PATH}"

# Validate finished AppImage by extracting with --appimage-extract (no FUSE required) and inspecting layout
EXTRACT_WORK="${WORK_TMP}/extract-verify"
mkdir -p "${EXTRACT_WORK}"
(
    cd "${EXTRACT_WORK}"
    "${APPIMAGE_PATH}" --appimage-extract >/dev/null
)

SQUASH_ROOT="${EXTRACT_WORK}/squashfs-root"
for req_exec in "AppRun" "usr/bin/awcc-cli" "usr/bin/awcc-gui"; do
    if [[ ! -x "${SQUASH_ROOT}/${req_exec}" ]]; then
        echo "ERROR: Extracted AppImage missing executable ${req_exec}" >&2
        exit 1
    fi
done
for req_file in \
    "awcc-gui.desktop" \
    "awcc-gui.svg" \
    "awcc-gui.png" \
    ".DirIcon" \
    "usr/lib/libusb-1.0.so.0" \
    "usr/share/awcc/database.json" \
    "usr/share/awcc/package-format" \
    "usr/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml" \
    "usr/share/licenses/awcc-rs/LICENSE" \
    "usr/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md"; do
    if [[ ! -e "${SQUASH_ROOT}/${req_file}" ]]; then
        echo "ERROR: Extracted AppImage missing required file ${req_file}" >&2
        exit 1
    fi
done

# Verify excluded base/driver libraries were NOT bundled into usr/lib
for forbidden in libc.so.6 libm.so.6 libgcc_s.so.1 libGL.so.1 libEGL.so.1 libwayland-client.so.0 libX11.so.6 libfontconfig.so.1; do
    if [[ -e "${SQUASH_ROOT}/usr/lib/${forbidden}" ]]; then
        echo "ERROR: Forbidden base/driver library ${forbidden} found in AppDir/usr/lib" >&2
        exit 1
    fi
done

if [[ "${RUN_SMOKE}" -eq 1 ]]; then
    mapfile -t IDENTITY_ARGS < <(smoke_identity_args appimage)
    "${SCRIPT_DIR}/smoke-test-installed.sh" \
        "${IDENTITY_ARGS[@]}" \
        --cli "${SQUASH_ROOT}/usr/bin/awcc-cli" \
        --gui "${SQUASH_ROOT}/usr/bin/awcc-gui" \
        --database "${SQUASH_ROOT}/usr/share/awcc/database.json" \
        --apprun "${SQUASH_ROOT}/AppRun" \
        --label "appimage" \
        --screenshot-dir "${OUT_DIR}/smoke-screenshots"
fi

echo "==> Built and verified AppImage: ${APPIMAGE_PATH}"
