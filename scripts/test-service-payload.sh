#!/usr/bin/env bash
# Offline checks of the privileged-service payload and the AppRun service mappings.
# Installs nothing, never runs pkexec/sudo/systemctl, needs no display.
#
# Always runs: payload verifier self-test (good and hostile payloads) and AppRun mapping test with
# stub binaries. Optionally checks real artifacts:
#   --staged-dir <dir>   staged release tree (default: dist/staging/<tarball basename> if present)
#   --appimage <file>    built AppImage (extracted with --appimage-extract, no FUSE)
#   --tarball <file>     built binary archive
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"
# shellcheck source=scripts/service-payload.sh
source "${SCRIPT_DIR}/service-payload.sh"

STAGED_DIR="${AWCC_REPO_ROOT}/dist/staging/${AWCC_TARBALL_BASENAME}"
APPIMAGE=""
TARBALL=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --staged-dir)
            STAGED_DIR="$2"
            shift 2
            ;;
        --appimage)
            APPIMAGE="$2"
            shift 2
            ;;
        --tarball)
            TARBALL="$2"
            shift 2
            ;;
        *)
            echo "Usage: $0 [--staged-dir <dir>] [--appimage <file>] [--tarball <file>]" >&2
            exit 1
            ;;
    esac
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/awcc-service-payload-test-XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

pass() { echo "ok   - $*"; }
die() {
    echo "FAIL - $*" >&2
    exit 1
}

# --- 1. verifier self-test with a synthetic payload ------------------------------------------
make_payload() {
    local dir="$1"
    rm -rf "${dir}"
    mkdir -p "${dir}/bin" "${dir}/lib" "${dir}/share/awcc"
    printf '#!/bin/sh\nexit 0\n' >"${dir}/bin/awcc-cli"
    chmod 0755 "${dir}/bin/awcc-cli"
    printf 'lib' >"${dir}/lib/libusb-1.0.so.0"
    printf '{}' >"${dir}/share/awcc/database.json"
    chmod 0644 "${dir}/lib/libusb-1.0.so.0" "${dir}/share/awcc/database.json"
    {
        echo "build-id: 0.0.0-test"
        echo "version: 0.0.0"
        (cd "${dir}" && LC_ALL=C find bin lib share -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
    } >"${dir}/MANIFEST.sha256"
    chmod 0644 "${dir}/MANIFEST.sha256"
}

expect_rejected() {
    local label="$1" dir="$2"
    if verify_service_payload "${dir}" 2>/dev/null; then
        die "hostile payload accepted: ${label}"
    fi
    pass "rejected: ${label}"
}

make_payload "${WORK}/good"
verify_service_payload "${WORK}/good" || die "good synthetic payload rejected"
pass "good synthetic payload verifies"

make_payload "${WORK}/tamper"
printf 'x' >>"${WORK}/tamper/bin/awcc-cli"
expect_rejected "hash mismatch" "${WORK}/tamper"

make_payload "${WORK}/extra"
printf 'x' >"${WORK}/extra/share/awcc/unlisted"
expect_rejected "unlisted file" "${WORK}/extra"

make_payload "${WORK}/symlink"
ln -s /etc/passwd "${WORK}/symlink/lib/evil"
expect_rejected "symlink" "${WORK}/symlink"

make_payload "${WORK}/setuid"
chmod 4755 "${WORK}/setuid/bin/awcc-cli"
expect_rejected "setuid" "${WORK}/setuid"

make_payload "${WORK}/writable"
chmod 0666 "${WORK}/writable/share/awcc/database.json"
expect_rejected "world-writable" "${WORK}/writable"

make_payload "${WORK}/traversal"
printf '%s  bin/../../etc/passwd\n' "$(printf x | sha256sum | cut -d' ' -f1)" >>"${WORK}/traversal/MANIFEST.sha256"
expect_rejected "path traversal in manifest" "${WORK}/traversal"

make_payload "${WORK}/absolute"
printf '%s  /etc/passwd\n' "$(printf x | sha256sum | cut -d' ' -f1)" >>"${WORK}/absolute/MANIFEST.sha256"
expect_rejected "absolute path in manifest" "${WORK}/absolute"

make_payload "${WORK}/nobuild"
sed -i '/^build-id: /d' "${WORK}/nobuild/MANIFEST.sha256"
expect_rejected "missing build-id" "${WORK}/nobuild"

# --- 2. AppRun mappings against stub binaries ------------------------------------------------
APPRUN_SRC="${AWCC_REPO_ROOT}/packaging/linux/AppRun"

# check_apprun_mapping <apprun-file>: runs the AppRun beside stub awcc-cli/awcc-gui binaries that
# only print their argv, so nothing real (service, GUI, display) is touched.
check_apprun_mapping() {
    local apprun="$1" appdir="${WORK}/stub-appdir"
    rm -rf "${appdir}"
    mkdir -p "${appdir}/usr/bin"
    cp "${apprun}" "${appdir}/AppRun"
    chmod 0755 "${appdir}/AppRun"
    cat >"${appdir}/usr/bin/awcc-cli" <<'EOF'
#!/bin/sh
printf "CLI:"
for a in "$@"; do printf " [%s]" "$a"; done
echo
EOF

    cat >"${appdir}/usr/bin/awcc-gui" <<'EOF'
#!/bin/sh
printf "GUI:"
for a in "$@"; do printf " [%s]" "$a"; done
echo
EOF
    chmod 0755 "${appdir}/usr/bin/awcc-cli" "${appdir}/usr/bin/awcc-gui"

    local got invoked_name="AppRun"
    expect_output() {
        local want="$1"
        shift
        got="$(env -u DISPLAY -u WAYLAND_DISPLAY ARGV0="${invoked_name}" APPDIR="${appdir}" "${appdir}/AppRun" "$@")"
        [[ "${got}" == "${want}" ]] || die "AppRun (${invoked_name}) $*: expected '${want}', got '${got}'"
        pass "AppRun (${invoked_name}) $* -> ${want}"
    }
    expect_output "CLI: [service] [install]" --install-service
    expect_output "CLI: [service] [update]" --update-service
    expect_output "CLI: [service] [uninstall]" --uninstall-service
    expect_output "CLI: [service] [uninstall] [--purge]" --uninstall-service --purge
    expect_output "CLI: [service] [status]" --service-status
    expect_output "CLI: [service] [status] [--json]" --service-status --json
    expect_output "CLI: [service-apply] [install]" --service-apply install
    expect_output "CLI: [service-apply] [uninstall] [--purge]" --service-apply uninstall --purge
    expect_output "CLI: [--version]" --cli --version
    expect_output "CLI: [device-info]" cli device-info
    expect_output "GUI: [--sim]" --sim
    expect_output "GUI:"

    # Elevation relaunches the canonical image path. Its real filename may itself select CLI
    # mode, so service flags must still be translated before the name-based dispatch.
    for invoked_name in awcc-cli awcc-cli.AppImage; do
        expect_output "CLI: [service] [install]" --install-service
        expect_output "CLI: [service] [update]" --update-service
        expect_output "CLI: [service] [uninstall] [--purge]" --uninstall-service --purge
        expect_output "CLI: [service] [status] [--json]" --service-status --json
        expect_output "CLI: [service-apply] [install]" --service-apply install
        expect_output "CLI: [service-apply] [uninstall] [--purge]" --service-apply uninstall --purge
        expect_output "CLI: [service] [install]" service install
        expect_output "CLI: [--version]" --version
    done
}
check_apprun_mapping "${APPRUN_SRC}"

# --- 3. real artifacts (optional) ------------------------------------------------------------
if [[ -d "${STAGED_DIR}/${SERVICE_PAYLOAD_RELATIVE}" ]]; then
    verify_service_payload "${STAGED_DIR}/${SERVICE_PAYLOAD_RELATIVE}" || die "staged payload"
    cmp -s "${STAGED_DIR}/bin/awcc-cli" "${STAGED_DIR}/${SERVICE_PAYLOAD_RELATIVE}/bin/awcc-cli" \
        || die "staged payload awcc-cli differs from bin/awcc-cli"
    pass "staged payload verifies and carries the release awcc-cli"
else
    echo "skip - no staged payload at ${STAGED_DIR}/${SERVICE_PAYLOAD_RELATIVE}"
fi

if [[ -n "${TARBALL}" ]]; then
    mkdir -p "${WORK}/tarball"
    tar -xzf "${TARBALL}" -C "${WORK}/tarball"
    root="$(find "${WORK}/tarball" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
    verify_service_payload "${root}/${SERVICE_PAYLOAD_RELATIVE}" || die "tarball payload"
    pass "tarball payload verifies"
fi

if [[ -n "${APPIMAGE}" ]]; then
    mkdir -p "${WORK}/appimage"
    (cd "${WORK}/appimage" && "${APPIMAGE}" --appimage-extract >/dev/null)
    squash="${WORK}/appimage/squashfs-root"
    verify_service_payload "${squash}/usr/${SERVICE_PAYLOAD_RELATIVE}" || die "AppImage payload"
    cmp -s "${squash}/usr/bin/awcc-cli" "${squash}/usr/${SERVICE_PAYLOAD_RELATIVE}/bin/awcc-cli" \
        || die "AppImage payload awcc-cli differs from usr/bin/awcc-cli"
    if [[ -n "$(find "${squash}/usr/${SERVICE_PAYLOAD_RELATIVE}" -type l -print -quit)" ]]; then
        die "AppImage payload contains symlinks"
    fi
    check_apprun_mapping "${squash}/AppRun"
    pass "AppImage payload and AppRun verified"
fi

echo "All service payload checks passed."
