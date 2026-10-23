#!/usr/bin/env bash
# Build and validate the Debian/Ubuntu .deb package from the staged AWCC Linux payload.
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

STAGED_DIR="$(realpath -m "${STAGED_DIR}")"
OUT_DIR="$(realpath -m "${OUT_DIR}")"

if [[ ! -d "${STAGED_DIR}" ]]; then
    "${SCRIPT_DIR}/stage-release-payload.sh" --out-dir "${STAGED_DIR}"
fi

mkdir -p "${OUT_DIR}"
DEB_PATH="${OUT_DIR}/${AWCC_DEB_NAME}"

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-deb-XXXXXX")"
cleanup() {
    rm -rf "${WORK_TMP}"
}
trap cleanup EXIT

PKG_ROOT="${WORK_TMP}/pkg"
mkdir -p \
    "${PKG_ROOT}/DEBIAN" \
    "${PKG_ROOT}/etc/awcc" \
    "${PKG_ROOT}/usr/bin" \
    "${PKG_ROOT}/usr/share/awcc" \
    "${PKG_ROOT}/usr/lib/systemd/system" \
    "${PKG_ROOT}/usr/lib/modules-load.d" \
    "${PKG_ROOT}/usr/lib/udev/rules.d" \
    "${PKG_ROOT}/usr/share/applications" \
    "${PKG_ROOT}/usr/share/icons/hicolor/scalable/apps" \
    "${PKG_ROOT}/usr/share/icons/hicolor/256x256/apps" \
    "${PKG_ROOT}/usr/share/metainfo" \
    "${PKG_ROOT}/usr/share/licenses/awcc-rs" \
    "${PKG_ROOT}/usr/share/doc/awcc-rs"

install -m 0755 "${STAGED_DIR}/bin/powerprofilesctl" "${PKG_ROOT}/usr/bin/powerprofilesctl"
install -m 0755 "${STAGED_DIR}/bin/awcc-cli" "${PKG_ROOT}/usr/bin/awcc-cli"
install -m 0755 "${STAGED_DIR}/bin/awcc-gui" "${PKG_ROOT}/usr/bin/awcc-gui"
install -m 0644 "${STAGED_DIR}/share/awcc/database.json" "${PKG_ROOT}/etc/awcc/database.json"
install -m 0644 "${STAGED_DIR}/share/awcc/database.json" "${PKG_ROOT}/usr/share/awcc/database.json"
printf 'deb\n' > "${PKG_ROOT}/usr/share/awcc/package-format"
chmod 0644 "${PKG_ROOT}/usr/share/awcc/package-format"
install -m 0644 "${STAGED_DIR}/share/applications/awcc-gui.desktop" "${PKG_ROOT}/usr/share/applications/awcc-gui.desktop"
mkdir -p "${PKG_ROOT}/usr/share/dbus-1/system-services"
cp "${STAGED_DIR}/share/dbus-1/system-services/"* "${PKG_ROOT}/usr/share/dbus-1/system-services/"
mkdir -p "${PKG_ROOT}/usr/share/dbus-1/system.d"
cp "${STAGED_DIR}/share/dbus-1/system.d/"* "${PKG_ROOT}/usr/share/dbus-1/system.d/"
mkdir -p "${PKG_ROOT}/usr/share/polkit-1/actions"
cp "${STAGED_DIR}/share/polkit-1/actions/"* "${PKG_ROOT}/usr/share/polkit-1/actions/"
install -m 0644 "${STAGED_DIR}/share/systemd/awccd.service" "${PKG_ROOT}/usr/lib/systemd/system/awccd.service"
install -m 0644 "${STAGED_DIR}/share/modules-load.d/awcc-rs.conf" "${PKG_ROOT}/usr/lib/modules-load.d/awcc-rs.conf"
install -m 0644 "${STAGED_DIR}/share/udev/rules.d/70-awcc-rs.rules" "${PKG_ROOT}/usr/lib/udev/rules.d/70-awcc-rs.rules"
install -m 0644 "${STAGED_DIR}/share/icons/hicolor/scalable/apps/awcc-gui.svg" "${PKG_ROOT}/usr/share/icons/hicolor/scalable/apps/awcc-gui.svg"
install -m 0644 "${STAGED_DIR}/share/icons/hicolor/256x256/apps/awcc-gui.png" "${PKG_ROOT}/usr/share/icons/hicolor/256x256/apps/awcc-gui.png"
install -m 0644 "${STAGED_DIR}/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml" "${PKG_ROOT}/usr/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml"
install -m 0644 "${STAGED_DIR}/share/licenses/awcc-rs/LICENSE" "${PKG_ROOT}/usr/share/licenses/awcc-rs/LICENSE"
install -m 0644 "${STAGED_DIR}/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md" "${PKG_ROOT}/usr/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md"
install -m 0644 "${STAGED_DIR}/share/licenses/awcc-rs/LICENSE" "${PKG_ROOT}/usr/share/doc/awcc-rs/copyright"
install -m 0644 "${STAGED_DIR}/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md" "${PKG_ROOT}/usr/share/doc/awcc-rs/THIRD_PARTY_NOTICES.md"
install -m 0644 "${STAGED_DIR}/share/doc/awcc-rs/README.md" "${PKG_ROOT}/usr/share/doc/awcc-rs/README.md"
install -m 0644 "${STAGED_DIR}/share/doc/awcc-rs/INSTALL.txt" "${PKG_ROOT}/usr/share/doc/awcc-rs/INSTALL.txt"

cat > "${PKG_ROOT}/DEBIAN/conffiles" <<'EOF'
/etc/awcc/database.json
EOF
chmod 0644 "${PKG_ROOT}/DEBIAN/conffiles"

for hook in postinst prerm postrm; do
    bash "${SCRIPT_DIR}/render-native-package-hook.sh" "deb.${hook}" > "${PKG_ROOT}/DEBIAN/${hook}"
    chmod 0755 "${PKG_ROOT}/DEBIAN/${hook}"
done

# Compute shared library dependencies dynamically with dpkg-shlibdeps when on Debian/Ubuntu,
# or use verified Ubuntu 24.04 shlibs baseline when building on a non-Debian host.
SHLIB_DEPENDS="libc6 (>= 2.39), libfontconfig1 (>= 2.12.6), libgcc-s1 (>= 4.2), libusb-1.0-0 (>= 2:1.0.22)"
if command -v dpkg-shlibdeps >/dev/null 2>&1; then
    mkdir -p "${WORK_TMP}/debian"
    cat > "${WORK_TMP}/debian/control" <<EOF
Source: awcc-rs
Section: utils
Priority: optional
Maintainer: AWCC Linux Contributors <noreply@github.com>

Package: awcc-rs
Architecture: ${AWCC_DEB_ARCH}
Description: Unofficial Alienware Command Center for Linux
EOF
    if (
        cd "${WORK_TMP}"
        dpkg-shlibdeps -O "${PKG_ROOT}/usr/bin/awcc-cli" "${PKG_ROOT}/usr/bin/awcc-gui" > "${WORK_TMP}/shlibs.out" 2>/dev/null
    ); then
        COMPUTED="$(sed -n 's/^shlibs:Depends=//p' "${WORK_TMP}/shlibs.out")"
        if [[ -n "${COMPUTED}" ]]; then
            SHLIB_DEPENDS="${COMPUTED}"
        fi
    fi
fi

INSTALLED_SIZE="$(du -sk "${PKG_ROOT}/usr" "${PKG_ROOT}/etc" | awk '{s+=$1} END {print s}')"

cat > "${PKG_ROOT}/DEBIAN/control" <<EOF
Package: awcc-rs
Version: ${AWCC_VERSION}
Section: utils
Priority: optional
Architecture: ${AWCC_DEB_ARCH}
Installed-Size: ${INSTALLED_SIZE}
Depends: ${SHLIB_DEPENDS}, init-system-helpers (>= 1.54~), util-linux, dbus, policykit-1 | polkitd
Provides: power-profiles-daemon
Conflicts: power-profiles-daemon, tuned-ppd, tlp-pd
Replaces: power-profiles-daemon
Recommends: libxkbcommon0, libxkbcommon-x11-0, libx11-6, libxcursor1, libxi6, libxrender1, libwayland-client0, libwayland-egl1, libegl1, libgl1, policykit-1 | pkexec
Suggests: acpi-call-dkms
Maintainer: AWCC Linux Contributors <https://github.com/Hoggormino/awcc-rs>
Homepage: https://github.com/Hoggormino/awcc-rs
Description: Unofficial Alienware Command Center for Linux (Rust + Slint Material 3)
 AWCC Linux (awcc-rs) provides a Material 3 desktop application (awcc-gui) and
 command-line interface (awcc-cli) for thermal profile control, GameShift
 (G-Mode), manual fan boost, custom 4-point fan curves, CPU TCC offset, and
 Alienware USB HID ELC RGB lighting, plus a deterministic simulation mode (--sim).
EOF
chmod 0644 "${PKG_ROOT}/DEBIAN/control"

(
    cd "${PKG_ROOT}"
    find etc usr -type f -print0 | sort -z | xargs -0 md5sum > "${PKG_ROOT}/DEBIAN/md5sums"
)
chmod 0644 "${PKG_ROOT}/DEBIAN/md5sums"
find "${PKG_ROOT}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +

rm -f "${DEB_PATH}"
if command -v dpkg-deb >/dev/null 2>&1; then
    SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}" dpkg-deb --root-owner-group -Zgzip --build "${PKG_ROOT}" "${DEB_PATH}"
else
    # Deterministic standard Debian ar archive builder for non-Debian developer machines
    AR_WORK="${WORK_TMP}/ar"
    mkdir -p "${AR_WORK}"
    printf '2.0\n' > "${AR_WORK}/debian-binary"
    tar --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner \
        -C "${PKG_ROOT}/DEBIAN" -cf - . | gzip -n -9 > "${AR_WORK}/control.tar.gz"
    tar --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner \
        -C "${PKG_ROOT}" --exclude='./DEBIAN' -cf - ./etc ./usr | gzip -n -9 > "${AR_WORK}/data.tar.gz"
    touch -h -d "@${SOURCE_DATE_EPOCH}" "${AR_WORK}/debian-binary" "${AR_WORK}/control.tar.gz" "${AR_WORK}/data.tar.gz"
    ar rcsD "${DEB_PATH}" "${AR_WORK}/debian-binary" "${AR_WORK}/control.tar.gz" "${AR_WORK}/data.tar.gz"
fi
touch -h -d "@${SOURCE_DATE_EPOCH}" "${DEB_PATH}"

# Validate .deb structure and metadata
if command -v dpkg-deb >/dev/null 2>&1; then
    DEB_INFO="$(dpkg-deb --info "${DEB_PATH}")"
    grep -q "Package: awcc-rs" <<<"${DEB_INFO}"
    grep -q "Version: ${AWCC_VERSION}" <<<"${DEB_INFO}"
    grep -q "Architecture: ${AWCC_DEB_ARCH}" <<<"${DEB_INFO}"
    grep -q "libusb-1.0-0" <<<"${DEB_INFO}"
    DEB_CONTENTS="$(dpkg-deb --contents "${DEB_PATH}")"
    grep -q "./usr/bin/awcc-cli" <<<"${DEB_CONTENTS}"
    grep -q "./usr/bin/awcc-gui" <<<"${DEB_CONTENTS}"
    grep -q "./etc/awcc/database.json" <<<"${DEB_CONTENTS}"
    grep -q "./usr/share/awcc/package-format" <<<"${DEB_CONTENTS}"
    grep -q "./usr/lib/systemd/system/awccd.service" <<<"${DEB_CONTENTS}"
    grep -q "./usr/lib/modules-load.d/awcc-rs.conf" <<<"${DEB_CONTENTS}"
    grep -q "./usr/lib/udev/rules.d/70-awcc-rs.rules" <<<"${DEB_CONTENTS}"
    for hook in postinst prerm postrm; do
        dpkg-deb --ctrl-tarfile "${DEB_PATH}" | tar -xOf - "./${hook}" > "${WORK_TMP}/${hook}"
        cmp "${WORK_TMP}/${hook}" "${PKG_ROOT}/DEBIAN/${hook}"
    done
else
    ar t "${DEB_PATH}" | grep -qx "debian-binary"
    ar t "${DEB_PATH}" | grep -qx "control.tar.gz"
    ar t "${DEB_PATH}" | grep -qx "data.tar.gz"
fi

echo "==> Built and verified Debian package: ${DEB_PATH}"
