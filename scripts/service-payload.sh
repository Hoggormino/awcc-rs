#!/usr/bin/env bash
# Build and verify the privileged-service payload that ships inside the tarball and the AppImage
# (`share/awcc/service-payload`). Source this file, or run `service-payload.sh verify <dir>`.
#
# Layout (everything the root-side installer copies to /opt/awcc-rs/service/releases/<build-id>):
#   MANIFEST.sha256          `build-id:` / `version:` lines, then sorted `<sha256>  <path>` lines
#   bin/awcc-cli             byte-identical to the release binary
#   lib/libusb-1.0.so.0      private libraries the service needs (never GUI libraries)
#   share/awcc/database.json
#
# The manifest checks integrity only; it does not authenticate who published the release.
# Nothing here installs or changes anything on the host.
set -euo pipefail

SERVICE_PAYLOAD_RELATIVE="share/awcc/service-payload"
SERVICE_PAYLOAD_TOKEN_RE='^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$'

# Resolve a shared library soname through the dynamic dependencies of a binary.
service_payload_resolve_lib() {
    local binary="$1" soname="$2" resolved=""
    resolved="$(ldd "${binary}" 2>/dev/null \
        | awk -v target="${soname}" '$1 == target && $3 ~ /^\// { print $3; exit }')"
    if [[ -z "${resolved}" ]] && command -v ldconfig >/dev/null 2>&1; then
        resolved="$(ldconfig -p 2>/dev/null \
            | awk -v target="${soname}" '$1 == target && $(NF) ~ /^\// { print $(NF); exit }')"
    fi
    printf '%s' "${resolved}"
}

# build_service_payload <staged-dir>: create <staged-dir>/share/awcc/service-payload.
# Requires AWCC_VERSION, AWCC_GIT_SHORT_SHA and AWCC_GIT_DIRTY (see release-meta.sh).
build_service_payload() {
    local staged="$1"
    local payload="${staged}/${SERVICE_PAYLOAD_RELATIVE}"
    rm -rf "${payload}"
    mkdir -p "${payload}/bin" "${payload}/lib" "${payload}/share/awcc"
    install -m 0755 "${staged}/bin/awcc-cli" "${payload}/bin/awcc-cli"
    install -m 0644 "${staged}/share/awcc/database.json" "${payload}/share/awcc/database.json"

    local -a service_libraries=(libusb-1.0.so.0)
    local soname lib_path

    for soname in "${service_libraries[@]}"; do
        lib_path="$(service_payload_resolve_lib "${staged}/bin/awcc-cli" "${soname}")"
        if [[ -z "${lib_path}" || ! -f "${lib_path}" ]]; then
            echo "ERROR: could not resolve ${soname} for the service payload" >&2
            return 1
        fi
        cp -L "${lib_path}" "${payload}/lib/${soname}"
        chmod 0644 "${payload}/lib/${soname}"
    done

    local bin_sha build_id
    bin_sha="$(sha256sum "${payload}/bin/awcc-cli" | cut -c1-8)"
    build_id="${AWCC_VERSION}-${AWCC_GIT_SHORT_SHA}"
    if [[ "${AWCC_GIT_DIRTY:-unknown}" != "0" || "${AWCC_GIT_SHORT_SHA}" == "unknown" ]]; then
        # An uncommitted or unidentifiable build must never collide with a clean release.
        build_id="${build_id}-dev${bin_sha}"
    fi
    if [[ ! "${build_id}" =~ ${SERVICE_PAYLOAD_TOKEN_RE} ]]; then
        echo "ERROR: derived build id '${build_id}' is not a valid payload token" >&2
        return 1
    fi

    {
        echo "build-id: ${build_id}"
        echo "version: ${AWCC_VERSION}"
        (cd "${payload}" && LC_ALL=C find bin lib share -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
    } >"${payload}/MANIFEST.sha256"
    chmod 0644 "${payload}/MANIFEST.sha256"
    echo "==> Built service payload ${build_id} at ${payload}"
}

# verify_service_payload <payload-dir>: structural and integrity checks, no installation.
verify_service_payload() {
    local dir="$1" failures=0
    fail() {
        echo "ERROR: service payload ${dir}: $*" >&2
        failures=1
    }
    local manifest="${dir}/MANIFEST.sha256"
    if [[ ! -f "${manifest}" || -L "${manifest}" ]]; then
        echo "ERROR: service payload ${dir}: MANIFEST.sha256 is missing" >&2
        return 1
    fi

    local build_id version
    build_id="$(sed -n 's/^build-id: //p' "${manifest}")"
    version="$(sed -n 's/^version: //p' "${manifest}")"
    [[ "${build_id}" =~ ${SERVICE_PAYLOAD_TOKEN_RE} ]] || fail "invalid or missing build-id '${build_id}'"
    [[ "${version}" =~ ${SERVICE_PAYLOAD_TOKEN_RE} ]] || fail "invalid or missing version '${version}'"

    local entries line
    entries="$(grep -vE '^(build-id: |version: |#|$)' "${manifest}" || true)"
    while IFS= read -r line; do
        [[ -z "${line}" ]] && continue
        if [[ ! "${line}" =~ ^[0-9a-f]{64}\ \ (bin|lib|share)/[A-Za-z0-9._+/@-]+$ ]] \
            || [[ "${line}" == *'..'* || "${line}" == *'//'* ]]; then
            fail "malformed or unsafe manifest line: ${line}"
        fi
    done <<<"${entries}"

    if [[ "${failures}" -eq 0 ]]; then
        (cd "${dir}" && printf '%s\n' "${entries}" | sha256sum --quiet --strict -c -) \
            || fail "sha256 verification failed"
        local listed present
        listed="$(printf '%s\n' "${entries}" | sed 's/^[0-9a-f]\{64\}  //' | LC_ALL=C sort)"
        present="$(cd "${dir}" && LC_ALL=C find . -type f ! -name MANIFEST.sha256 | sed 's|^\./||' | LC_ALL=C sort)"
        [[ "${listed}" == "${present}" ]] || fail "file set differs from the manifest"
        grep -qE '^[0-9a-f]{64}  bin/awcc-cli$' <<<"${entries}" || fail "bin/awcc-cli is not listed"
        grep -qE '^[0-9a-f]{64}  share/awcc/database.json$' <<<"${entries}" || fail "database.json is not listed"
    fi

    if [[ -n "$(find "${dir}" ! -type f ! -type d -print -quit)" ]]; then
        fail "contains symlinks or special files"
    fi
    if [[ -n "$(find "${dir}" -perm /7000 -print -quit)" ]]; then
        fail "contains setuid/setgid/sticky entries"
    fi
    if [[ -n "$(find "${dir}" -perm /022 -print -quit)" ]]; then
        fail "contains group- or world-writable entries"
    fi
    [[ -x "${dir}/bin/awcc-cli" ]] || fail "bin/awcc-cli is not executable"
    [[ "${failures}" -eq 0 ]]
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        verify)
            verify_service_payload "${2:?usage: $0 verify <payload-dir>}"
            echo "Service payload ${2} verified."
            ;;
        *)
            echo "Usage: $0 verify <payload-dir>" >&2
            exit 1
            ;;
    esac
fi
