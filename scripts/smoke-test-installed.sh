#!/usr/bin/env bash
# Deterministic, unprivileged CLI + headless GUI screenshot smoke test runner.
# Executes outside the source checkout in an isolated temporary directory so accidental
# reliance on source-tree files or nearby database.json cannot be hidden.
set -euo pipefail

CLI_BIN=""
GUI_BIN=""
DATABASE_PATH=""
EXPECT_DB_SUBSTRING=""
LABEL="local"
SCREENSHOT_DIR=""
APPRUN_BIN=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cli)
            CLI_BIN="$2"
            shift 2
            ;;
        --gui)
            GUI_BIN="$2"
            shift 2
            ;;
        --database)
            DATABASE_PATH="$2"
            shift 2
            ;;
        --expect-db-source)
            EXPECT_DB_SUBSTRING="$2"
            shift 2
            ;;
        --label)
            LABEL="$2"
            shift 2
            ;;
        --screenshot-dir)
            SCREENSHOT_DIR="$2"
            shift 2
            ;;
        --apprun)
            APPRUN_BIN="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 --cli <path> --gui <path> [--database <path>] [--expect-db-source <str>] [--label <name>] [--screenshot-dir <dir>] [--apprun <path>]" >&2
            exit 1
            ;;
    esac
done

if [[ -z "${CLI_BIN}" || -z "${GUI_BIN}" ]]; then
    echo "ERROR: --cli and --gui are required." >&2
    exit 1
fi

if [[ ! -x "${CLI_BIN}" ]]; then
    echo "ERROR: CLI binary is missing or not executable: ${CLI_BIN}" >&2
    exit 1
fi
if [[ ! -x "${GUI_BIN}" ]]; then
    echo "ERROR: GUI binary is missing or not executable: ${GUI_BIN}" >&2
    exit 1
fi
if [[ -n "${DATABASE_PATH}" && ! -f "${DATABASE_PATH}" ]]; then
    echo "ERROR: Specified --database file does not exist: ${DATABASE_PATH}" >&2
    exit 1
fi

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-smoke-${LABEL}-XXXXXX")"
cleanup() {
    rm -rf "${WORK_TMP}"
}
trap cleanup EXIT

ISOLATED_CWD="${WORK_TMP}/empty-cwd"
ISOLATED_HOME="${WORK_TMP}/home"
ISOLATED_STATE="${WORK_TMP}/xdg-state"
mkdir -p "${ISOLATED_CWD}" "${ISOLATED_HOME}" "${ISOLATED_STATE}"

if [[ -z "${SCREENSHOT_DIR}" ]]; then
    SCREENSHOT_DIR="${WORK_TMP}/screenshots"
fi
mkdir -p "${SCREENSHOT_DIR}"

run_isolated() {
    (
        cd "${ISOLATED_CWD}"
        if [[ -n "${DATABASE_PATH}" ]]; then
            env HOME="${ISOLATED_HOME}" XDG_STATE_HOME="${ISOLATED_STATE}" AWCC_DATABASE_PATH="${DATABASE_PATH}" "$@"
        else
            env -u AWCC_DATABASE_PATH HOME="${ISOLATED_HOME}" XDG_STATE_HOME="${ISOLATED_STATE}" "$@"
        fi
    )
}

echo "==> [${LABEL}] Running isolated CLI smoke tests in ${ISOLATED_CWD}..."
HELP_OUT="$(run_isolated "${CLI_BIN}" --help)"
grep -q "Usage: awcc-cli <command> \[options\]" <<<"${HELP_OUT}"

DEV_OUT="$(run_isolated "${CLI_BIN}" --sim device-info)"
grep -q "^Device Name     : Alienware m15 R6$" <<<"${DEV_OUT}"
grep -q "^ACPI Prefix     : AMWW (\\\\_SB\.AMWW\.WMAX)$" <<<"${DEV_OUT}"
grep -q "^Matched in DB   : true$" <<<"${DEV_OUT}"

if [[ -n "${DATABASE_PATH}" ]]; then
    grep -F "Database Source : AWCC_DATABASE_PATH (${DATABASE_PATH})" <<<"${DEV_OUT}" >/dev/null
elif [[ -n "${EXPECT_DB_SUBSTRING}" ]]; then
    grep -F "${EXPECT_DB_SUBSTRING}" <<<"${DEV_OUT}" >/dev/null
fi

# Also test passive non-sim device-info when /etc/awcc/database.json or --database is present
PASSIVE_DEV_OUT="$(run_isolated "${CLI_BIN}" device-info)"
grep -q "^Matched in DB   :" <<<"${PASSIVE_DEV_OUT}"

QM_OUT="$(run_isolated "${CLI_BIN}" --sim query-mode)"
grep -q "Current Thermal Mode: Balanced (GameShift: false" <<<"${QM_OUT}"

# Fail-closed check: pointing AWCC_DATABASE_PATH at a missing file must fail with non-zero exit code
if (
    cd "${ISOLATED_CWD}"
    env HOME="${ISOLATED_HOME}" XDG_STATE_HOME="${ISOLATED_STATE}" \
        AWCC_DATABASE_PATH="${WORK_TMP}/definitely-missing-database.json" \
        "${CLI_BIN}" --sim device-info >/dev/null 2>&1
); then
    echo "ERROR: [${LABEL}] awcc-cli --sim device-info unexpectedly succeeded with a missing AWCC_DATABASE_PATH" >&2
    exit 1
fi

echo "==> [${LABEL}] Running isolated GUI passive & headless screenshot smoke tests..."
GUI_HELP_OUT="$(run_isolated "${GUI_BIN}" --help)"
grep -q "awcc-gui \[OPTIONS | COMMAND\]" <<<"${GUI_HELP_OUT}"

GUI_DEV_OUT="$(run_isolated "${GUI_BIN}" --sim --device-info)"
grep -q "Device Name      : Alienware m15 R6 (simulated target)" <<<"${GUI_DEV_OUT}"

GUI_QM_OUT="$(run_isolated "${GUI_BIN}" --sim --query-mode)"
grep -q "Current Mode: Balanced (Simulated: true, Writes Executed: 0)" <<<"${GUI_QM_OUT}"

IMG_NORMAL="${SCREENSHOT_DIR}/${LABEL}-normal-1120x720.png"
IMG_COMPACT="${SCREENSHOT_DIR}/${LABEL}-compact-880x620.png"
IMG_ROLLBACK="${SCREENSHOT_DIR}/${LABEL}-rollback-1120x720.png"

run_isolated "${GUI_BIN}" --sim --screenshot "${IMG_NORMAL}" --width 1120 --height 720 --scenario normal
run_isolated "${GUI_BIN}" --sim --screenshot "${IMG_COMPACT}" --width 880 --height 620 --scenario compact
run_isolated "${GUI_BIN}" --sim --screenshot "${IMG_ROLLBACK}" --width 1120 --height 720 --scenario rollback

# Validate PNG headers, dimensions, and non-blank pixel data
python3 - "${IMG_NORMAL}:1120:720" "${IMG_COMPACT}:880:620" "${IMG_ROLLBACK}:1120:720" <<'PY'
import os, struct, sys

for spec in sys.argv[1:]:
    path, exp_w, exp_h = spec.rsplit(":", 2)
    exp_w, exp_h = int(exp_w), int(exp_h)
    if not os.path.isfile(path):
        raise SystemExit(f"ERROR: Missing screenshot {path}")
    size = os.path.getsize(path)
    if size < 4096:
        raise SystemExit(f"ERROR: Screenshot {path} is suspiciously small ({size} bytes)")
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"ERROR: Invalid PNG signature in {path}")
    ihdr_len, ihdr_tag = struct.unpack(">I4s", data[8:16])
    if ihdr_tag != b"IHDR" or ihdr_len != 13:
        raise SystemExit(f"ERROR: Missing IHDR chunk in {path}")
    w, h, bit_depth, color_type = struct.unpack(">IIBB", data[16:26])
    if (w, h) != (exp_w, exp_h):
        raise SystemExit(f"ERROR: Screenshot {path} dimensions {w}x{h} != expected {exp_w}x{exp_h}")
    if bit_depth != 8 or color_type != 6:
        raise SystemExit(f"ERROR: Screenshot {path} unexpected PNG format bit_depth={bit_depth} color_type={color_type}")

norm_bytes = open(sys.argv[1].rsplit(":", 2)[0], "rb").read()
roll_bytes = open(sys.argv[3].rsplit(":", 2)[0], "rb").read()
if norm_bytes == roll_bytes:
    raise SystemExit("ERROR: normal and rollback screenshots are byte-identical; scenario state did not render!")
print("Verified 3 screenshots (normal 1120x720, compact 880x620, rollback 1120x720): valid PNG headers, dimensions, and distinct visual states.")
PY

# If an AppImage AppRun launcher was provided, also test AppRun default GUI and --cli / ARGV0=awcc-cli modes
if [[ -n "${APPRUN_BIN}" ]]; then
    echo "==> [${LABEL}] Verifying AppRun launcher default GUI, --cli flag, ARGV0 dispatch, and exit-code forwarding..."
    APPRUN_CLI_OUT="$(
        cd "${ISOLATED_CWD}"
        env -u AWCC_DATABASE_PATH HOME="${ISOLATED_HOME}" XDG_STATE_HOME="${ISOLATED_STATE}" \
            "${APPRUN_BIN}" --cli --sim device-info
    )"
    grep -q "^Device Name     : Alienware m15 R6$" <<<"${APPRUN_CLI_OUT}"
    grep -q "Database Source : AWCC_DATABASE_PATH (" <<<"${APPRUN_CLI_OUT}"

    APPRUN_ARGV0_OUT="$(
        cd "${ISOLATED_CWD}"
        env -u AWCC_DATABASE_PATH HOME="${ISOLATED_HOME}" XDG_STATE_HOME="${ISOLATED_STATE}" \
            ARGV0="awcc-cli" "${APPRUN_BIN}" --sim query-mode
    )"
    grep -q "Current Thermal Mode: Balanced" <<<"${APPRUN_ARGV0_OUT}"

    # Verify non-zero exit code forwarding through AppRun
    set +e
    (
        cd "${ISOLATED_CWD}"
        "${APPRUN_BIN}" --cli definitely-invalid-subcommand >/dev/null 2>&1
    )
    APPRUN_ERR_RC=$?
    set -e
    if [[ "${APPRUN_ERR_RC}" -eq 0 ]]; then
        echo "ERROR: AppRun failed to forward non-zero exit code from awcc-cli" >&2
        exit 1
    fi

    IMG_APPRUN="${SCREENSHOT_DIR}/${LABEL}-apprun-normal-1120x720.png"
    (
        cd "${ISOLATED_CWD}"
        env -u AWCC_DATABASE_PATH HOME="${ISOLATED_HOME}" XDG_STATE_HOME="${ISOLATED_STATE}" \
            "${APPRUN_BIN}" --sim --screenshot "${IMG_APPRUN}" --width 1120 --height 720 --scenario normal
    )
    [[ -s "${IMG_APPRUN}" ]]
fi

echo "==> [${LABEL}] Smoke test passed!"
