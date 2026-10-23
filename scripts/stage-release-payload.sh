#!/usr/bin/env bash
# Build (optional) and stage the common AWCC Linux release payload into a normalized directory layout.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"
# shellcheck source=scripts/service-payload.sh
source "${SCRIPT_DIR}/service-payload.sh"

DO_BUILD=0
ENFORCE_GLIBC_MAX=""
BIN_DIR=""
OUT_DIR=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --build)
            DO_BUILD=1
            shift
            ;;
        --bin-dir)
            BIN_DIR="$2"
            shift 2
            ;;
        --out-dir)
            OUT_DIR="$2"
            shift 2
            ;;
        --enforce-glibc-max)
            ENFORCE_GLIBC_MAX="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--build] [--bin-dir <dir>] [--out-dir <dir>] [--enforce-glibc-max <ver>]" >&2
            exit 1
            ;;
    esac
done

# Guard against machine-specific CPU codegen flags that would break x86_64 portability
if [[ "${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}" == *"target-cpu=native"* ]]; then
    echo "ERROR: target-cpu=native detected in RUSTFLAGS; refusing to stage non-portable binaries." >&2
    exit 1
fi
if [[ -f "${AWCC_REPO_ROOT}/.cargo/config.toml" ]] && grep -q "target-cpu=native" "${AWCC_REPO_ROOT}/.cargo/config.toml"; then
    echo "ERROR: target-cpu=native detected in .cargo/config.toml; refusing to stage non-portable binaries." >&2
    exit 1
fi

if [[ "${DO_BUILD}" -eq 1 ]]; then
    echo "==> Building release workspace for target ${AWCC_TARGET} (--locked)..."
    cargo build --manifest-path "${AWCC_REPO_ROOT}/Cargo.toml" --workspace --release --locked --target "${AWCC_TARGET}"
fi

if [[ -z "${BIN_DIR}" ]]; then
    if [[ -x "${AWCC_REPO_ROOT}/target/${AWCC_TARGET}/release/awcc-cli" ]]; then
        BIN_DIR="${AWCC_REPO_ROOT}/target/${AWCC_TARGET}/release"
    elif [[ -x "${AWCC_REPO_ROOT}/target/release/awcc-cli" ]]; then
        BIN_DIR="${AWCC_REPO_ROOT}/target/release"
    else
        echo "ERROR: Could not find compiled release binaries in target/${AWCC_TARGET}/release or target/release." >&2
        exit 1
    fi
fi

if [[ -z "${OUT_DIR}" ]]; then
    OUT_DIR="${AWCC_REPO_ROOT}/dist/staging/${AWCC_TARBALL_BASENAME}"
fi
BIN_DIR="$(realpath -m "${BIN_DIR}")"
OUT_DIR="$(realpath -m "${OUT_DIR}")"

for bin in awcc-cli awcc-gui; do
    if [[ ! -x "${BIN_DIR}/${bin}" ]]; then
        echo "ERROR: Missing executable '${BIN_DIR}/${bin}'" >&2
        exit 1
    fi
done

rm -rf "${OUT_DIR}"
mkdir -p \
    "${OUT_DIR}/bin" \
    "${OUT_DIR}/share/awcc" \
    "${OUT_DIR}/share/systemd" \
    "${OUT_DIR}/share/modules-load.d" \
    "${OUT_DIR}/share/udev/rules.d" \
    "${OUT_DIR}/share/libalpm/hooks" \
    "${OUT_DIR}/share/libalpm/scripts" \
    "${OUT_DIR}/share/applications" \
    "${OUT_DIR}/share/icons/hicolor" \
    "${OUT_DIR}/share/metainfo" \
    "${OUT_DIR}/share/licenses/awcc-rs" \
    "${OUT_DIR}/share/doc/awcc-rs"

install -m 0755 "${AWCC_REPO_ROOT}/packaging/linux/powerprofilesctl" "${OUT_DIR}/bin/powerprofilesctl"
install -m 0755 "${BIN_DIR}/awcc-cli" "${OUT_DIR}/bin/awcc-cli"
install -m 0755 "${BIN_DIR}/awcc-gui" "${OUT_DIR}/bin/awcc-gui"

if command -v strip >/dev/null 2>&1; then
    strip --strip-unneeded "${OUT_DIR}/bin/awcc-cli" "${OUT_DIR}/bin/awcc-gui"
fi

install -m 0644 "${AWCC_REPO_ROOT}/crates/awcc-core/data/database.json" "${OUT_DIR}/share/awcc/database.json"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/awcc-gui.desktop" "${OUT_DIR}/share/applications/awcc-gui.desktop"
mkdir -p "${OUT_DIR}/share/dbus-1/system-services" "${OUT_DIR}/share/dbus-1/system.d" "${OUT_DIR}/share/polkit-1/actions"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/power-profiles/awcc-power-profiles.conf" "${OUT_DIR}/share/dbus-1/system.d/awcc-power-profiles.conf"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/power-profiles/awcc-power-profiles.policy" "${OUT_DIR}/share/polkit-1/actions/awcc-power-profiles.policy"
for name in org.freedesktop.UPower.PowerProfiles net.hadess.PowerProfiles; do
    printf '[D-BUS Service]\nName=%s\nExec=/usr/bin/awcc-cli -d\nUser=root\nSystemdService=awccd.service\n' "$name" > "${OUT_DIR}/share/dbus-1/system-services/${name}.service"
done
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/awccd.service" "${OUT_DIR}/share/systemd/awccd.service"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/awcc-rs.modules-load.conf" "${OUT_DIR}/share/modules-load.d/awcc-rs.conf"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/70-awcc-rs.rules" "${OUT_DIR}/share/udev/rules.d/70-awcc-rs.rules"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/05-awcc-rs-remove.hook" "${OUT_DIR}/share/libalpm/hooks/05-awcc-rs-remove.hook"
bash "${SCRIPT_DIR}/render-native-package-hook.sh" arch.pre-remove > "${OUT_DIR}/share/libalpm/scripts/awcc-rs-pre-remove"
chmod 0755 "${OUT_DIR}/share/libalpm/scripts/awcc-rs-pre-remove"
cp -a "${AWCC_REPO_ROOT}/packaging/linux/icons/hicolor/." "${OUT_DIR}/share/icons/hicolor/"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/io.github.hoggormino.awcc_rs.metainfo.xml" "${OUT_DIR}/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml"
install -m 0644 "${AWCC_REPO_ROOT}/LICENSE" "${OUT_DIR}/share/licenses/awcc-rs/LICENSE"
install -m 0644 "${AWCC_REPO_ROOT}/THIRD_PARTY_NOTICES.md" "${OUT_DIR}/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md"
install -m 0644 "${AWCC_REPO_ROOT}/README.md" "${OUT_DIR}/share/doc/awcc-rs/README.md"
install -Dm0644 "${AWCC_REPO_ROOT}/docs/power_profiles.md" "${OUT_DIR}/share/doc/awcc-rs/docs/power_profiles.md"
install -Dm0644 "${AWCC_REPO_ROOT}/docs/structured_logging_and_support_export.md" "${OUT_DIR}/share/doc/awcc-rs/docs/structured_logging_and_support_export.md"
install -Dm0644 "${AWCC_REPO_ROOT}/docs/power_profiles.md" "${OUT_DIR}/docs/power_profiles.md"
install -Dm0644 "${AWCC_REPO_ROOT}/docs/structured_logging_and_support_export.md" "${OUT_DIR}/docs/structured_logging_and_support_export.md"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/INSTALL.txt" "${OUT_DIR}/share/doc/awcc-rs/INSTALL.txt"

# Also place top-level convenience copies of LICENSE, THIRD_PARTY_NOTICES.md, README.md, and INSTALL.txt
install -m 0644 "${AWCC_REPO_ROOT}/LICENSE" "${OUT_DIR}/LICENSE"
install -m 0644 "${AWCC_REPO_ROOT}/THIRD_PARTY_NOTICES.md" "${OUT_DIR}/THIRD_PARTY_NOTICES.md"
install -m 0644 "${AWCC_REPO_ROOT}/README.md" "${OUT_DIR}/README.md"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/INSTALL.txt" "${OUT_DIR}/INSTALL.txt"

# Inspect ELF dynamic dependencies and symbol versions
if command -v objdump >/dev/null 2>&1; then
    MAX_GLIBC="$(objdump -T "${OUT_DIR}/bin/awcc-cli" "${OUT_DIR}/bin/awcc-gui" \
        | grep -oE 'GLIBC_[0-9]+\.[0-9]+' \
        | sed 's/^GLIBC_//' \
        | sort -uV \
        | tail -n 1 || true)"
    echo "==> Highest required GLIBC symbol version in staged binaries: GLIBC_${MAX_GLIBC:-unknown}"
    if [[ -n "${ENFORCE_GLIBC_MAX}" && -n "${MAX_GLIBC}" ]]; then
        HIGHEST_ALLOWED="$(printf '%s\n%s\n' "${ENFORCE_GLIBC_MAX}" "${MAX_GLIBC}" | sort -V | tail -n 1)"
        if [[ "${HIGHEST_ALLOWED}" != "${ENFORCE_GLIBC_MAX}" ]]; then
            echo "ERROR: Staged binaries require GLIBC_${MAX_GLIBC}, which exceeds baseline maximum GLIBC_${ENFORCE_GLIBC_MAX} (Ubuntu 24.04)." >&2
            exit 1
        fi
    fi
fi

# The embedded build identity must match the release metadata (guards against a stale cached
# build whose commit/toolchain no longer describes the commit being packaged).
for bin in awcc-cli awcc-gui; do
    assert_binary_identity "${OUT_DIR}/bin/${bin}" "staged ${bin}"
done
echo "==> Staged binaries report the expected build identity (commit ${AWCC_GIT_SHA}, dirty=${AWCC_GIT_DIRTY})"

# Privileged-service payload used by the AppImage/tarball `service install|update` commands.
build_service_payload "${OUT_DIR}"
verify_service_payload "${OUT_DIR}/${SERVICE_PAYLOAD_RELATIVE}"

# Normalize modification timestamps inside staging directory
find "${OUT_DIR}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +

echo "==> Staged release payload ready at ${OUT_DIR}"
