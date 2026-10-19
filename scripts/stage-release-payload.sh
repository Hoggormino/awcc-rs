#!/usr/bin/env bash
# Build (optional) and stage the common AWCC Linux release payload into a normalized directory layout.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"

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
    "${OUT_DIR}/share/applications" \
    "${OUT_DIR}/share/icons/hicolor/scalable/apps" \
    "${OUT_DIR}/share/icons/hicolor/256x256/apps" \
    "${OUT_DIR}/share/metainfo" \
    "${OUT_DIR}/share/licenses/awcc-rs" \
    "${OUT_DIR}/share/doc/awcc-rs"

install -m 0755 "${BIN_DIR}/awcc-cli" "${OUT_DIR}/bin/awcc-cli"
install -m 0755 "${BIN_DIR}/awcc-gui" "${OUT_DIR}/bin/awcc-gui"

if command -v strip >/dev/null 2>&1; then
    strip --strip-unneeded "${OUT_DIR}/bin/awcc-cli" "${OUT_DIR}/bin/awcc-gui"
fi

install -m 0644 "${AWCC_REPO_ROOT}/crates/awcc-core/data/database.json" "${OUT_DIR}/share/awcc/database.json"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/awcc-gui.desktop" "${OUT_DIR}/share/applications/awcc-gui.desktop"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/awcc-gui.svg" "${OUT_DIR}/share/icons/hicolor/scalable/apps/awcc-gui.svg"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/awcc-gui.png" "${OUT_DIR}/share/icons/hicolor/256x256/apps/awcc-gui.png"
install -m 0644 "${AWCC_REPO_ROOT}/packaging/linux/io.github.hoggormino.awcc_rs.metainfo.xml" "${OUT_DIR}/share/metainfo/io.github.hoggormino.awcc_rs.metainfo.xml"
install -m 0644 "${AWCC_REPO_ROOT}/LICENSE" "${OUT_DIR}/share/licenses/awcc-rs/LICENSE"
install -m 0644 "${AWCC_REPO_ROOT}/THIRD_PARTY_NOTICES.md" "${OUT_DIR}/share/licenses/awcc-rs/THIRD_PARTY_NOTICES.md"
install -m 0644 "${AWCC_REPO_ROOT}/README.md" "${OUT_DIR}/share/doc/awcc-rs/README.md"
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

# Normalize modification timestamps inside staging directory
find "${OUT_DIR}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +

echo "==> Staged release payload ready at ${OUT_DIR}"
