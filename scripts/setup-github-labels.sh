#!/usr/bin/env bash
# Idempotent GitHub label provisioning script for Hoggormino/awcc-rs.
# Creates or updates required issue-intake, area, and priority labels while preserving
# all pre-existing or unrelated repository labels.
set -euo pipefail

REPO="Hoggormino/awcc-rs"
DRY_RUN=false
LIST_JSON=false

usage() {
    cat <<'EOF'
Usage: ./scripts/setup-github-labels.sh [OPTIONS]

Options:
  --repo <owner/name>   Target GitHub repository (default: Hoggormino/awcc-rs)
  --dry-run             Print the gh label commands that would run without mutating GitHub
  --list-json           Print the managed label catalog as JSON and exit
  -h, --help            Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo)
            REPO="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --list-json)
            LIST_JSON=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "ERROR: Unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

# Format: name|color|description
# Reuses existing GitHub default labels (`bug`, `enhancement`, `documentation`, `duplicate`,
# `invalid`, `question`, `wontfix`) and adds the concise triage, area, priority, and CI labels.
LABELS=(
    "bug|d73a4a|Something isn't working"
    "enhancement|a2eeef|New feature or request"
    "needs-triage|fbca04|Awaiting initial maintainer review and classification"
    "needs-info|d876e3|More diagnostic or reproduction details are needed from the reporter"
    "confirmed|0e8a16|Reproduced or verified by a maintainer"
    "needs-hardware-test|5319e7|Requires verification on live Alienware hardware"
    "area: packaging|006b75|Packages (.deb, .rpm, .pkg.tar.zst, AppImage, .tar.gz), systemd, or udev"
    "area: gui|1d76db|Slint Material 3 desktop interface (awcc-gui)"
    "area: thermal|d93f0b|ACPI WMAX thermal profiles, G-Mode, fan boost, fan curves, or TCC"
    "area: lighting|c5def5|USB HID ELC RGB lighting zones, effects, brightness, or stealth"
    "area: hardware|bfd4f2|Device database.json profile, DMI/SMBIOS matching, or new Alienware model"
    "priority: high|b60205|High-priority issue selected by a maintainer"
    "ci|ededed|Continuous integration and GitHub Actions workflows"
    "dependencies|0366d6|Dependency and GitHub Action updates"
)

if [[ "${LIST_JSON}" == "true" ]]; then
    printf '[\n'
    for i in "${!LABELS[@]}"; do
        IFS='|' read -r name color desc <<<"${LABELS[$i]}"
        comma=","
        if [[ "$i" -eq $((${#LABELS[@]} - 1)) ]]; then
            comma=""
        fi
        printf '  {"name": "%s", "color": "%s", "description": "%s"}%s\n' \
            "${name}" "${color}" "${desc}" "${comma}"
    done
    printf ']\n'
    exit 0
fi

if [[ "${DRY_RUN}" == "true" ]]; then
    echo "==> [DRY-RUN] Would create or update ${#LABELS[@]} labels on ${REPO} (preserving all other labels):"
    for entry in "${LABELS[@]}"; do
        IFS='|' read -r name color desc <<<"${entry}"
        echo "  gh label create $(printf '%q' "${name}") --repo $(printf '%q' "${REPO}") --color $(printf '%q' "${color}") --description $(printf '%q' "${desc}") --force"
    done
    exit 0
fi

if ! command -v gh >/dev/null 2>&1; then
    echo "ERROR: GitHub CLI (gh) is required to apply labels." >&2
    exit 1
fi

echo "==> Synchronizing ${#LABELS[@]} managed labels on ${REPO} (preserving unrelated labels)..."
for entry in "${LABELS[@]}"; do
    IFS='|' read -r name color desc <<<"${entry}"
    gh label create "${name}" \
        --repo "${REPO}" \
        --color "${color}" \
        --description "${desc}" \
        --force
    echo "    ✓ ${name} (#${color})"
done
echo "==> Label synchronization complete."
