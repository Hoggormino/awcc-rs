#!/usr/bin/env bash
# Safe, idempotent draft-then-publish GitHub Release automation for awcc-rs.
# - Validates strict vX.Y.Z tag format, Cargo.toml version match, and exact commit identity.
# - Verifies complete 9-file release asset set and SHA256SUMS / PKGBUILD agreement before upload.
# - Handles safe retries: if the release is already published and matches the recorded commit and
#   asset manifest, exits 0 cleanly; if it conflicts or is incomplete, fails with an actionable error.
# - Otherwise creates or resumes a draft release for the exact commit, uploads and verifies all
#   assets while in draft state, and promotes the draft to published automatically.
set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-meta.sh
source "${SCRIPT_DIR}/release-meta.sh"

DIST_DIR="${AWCC_REPO_ROOT}/dist"
RELEASE_TAG=""
REPO="${GITHUB_REPOSITORY:-Hoggormino/awcc-rs}"
DRY_RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dist-dir)
            DIST_DIR="$2"
            shift 2
            ;;
        --tag)
            RELEASE_TAG="$2"
            shift 2
            ;;
        --repo)
            REPO="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 --tag <vX.Y.Z> [--dist-dir <dir>] [--repo <owner/name>] [--dry-run]" >&2
            exit 1
            ;;
    esac
done

if [[ -z "${RELEASE_TAG}" ]]; then
    echo "ERROR: --tag <vX.Y.Z> is required." >&2
    exit 1
fi

DIST_DIR="$(realpath -m "${DIST_DIR}")"
validate_release_tag "${RELEASE_TAG}"

ALL_ASSETS=(
    "${AWCC_TARBALL_NAME}"
    "${AWCC_APPIMAGE_NAME}"
    "${AWCC_DEB_NAME}"
    "${AWCC_RPM_NAME}"
    "${AWCC_ARCH_PKG_NAME}"
    "PKGBUILD"
    ".SRCINFO"
    "build-manifest.json"
    "SHA256SUMS"
)

for asset in "${ALL_ASSETS[@]}"; do
    if [[ ! -s "${DIST_DIR}/${asset}" ]]; then
        echo "ERROR: Missing or empty release asset before publication: ${DIST_DIR}/${asset}" >&2
        exit 1
    fi
done

# Verify local SHA256SUMS and manifest commit identity
(
    cd "${DIST_DIR}"
    sha256sum -c "SHA256SUMS"
)

MANIFEST_COMMIT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["git_commit"])' "${DIST_DIR}/build-manifest.json")"
if [[ "${MANIFEST_COMMIT}" != "${AWCC_GIT_SHA}" ]]; then
    echo "ERROR: build-manifest.json commit (${MANIFEST_COMMIT}) does not match HEAD (${AWCC_GIT_SHA})." >&2
    exit 1
fi

# Generate release notes
NOTES_FILE="$(mktemp "${TMPDIR:-/tmp}/awcc-rs-release-notes-XXXXXX.md")"
cleanup() {
    rm -f "${NOTES_FILE}"
}
trap cleanup EXIT

cat > "${NOTES_FILE}" <<EOF
## AWCC Linux (\`awcc-rs\`) ${RELEASE_TAG}

Unofficial Alienware Command Center for Linux (\`x86_64-unknown-linux-gnu\`), providing both the **Slint Material 3 Desktop GUI (\`awcc-gui\`)** and **Command-Line Interface (\`awcc-cli\`)**.

- **Source Commit**: \`${AWCC_GIT_SHA}\`
- **Rust Toolchain**: \`${AWCC_RUSTC_VERSION}\`
- **Common Binary & AppImage Baseline**: Ubuntu 24.04 LTS (\`x86_64\`, \`glibc >= 2.39\`)
- **Validated Package Environments**: Ubuntu 24.04 LTS (\`.deb\`), Fedora Linux 41 (\`.rpm\`), Arch Linux (\`.pkg.tar.zst\`, \`PKGBUILD\`, \`.SRCINFO\`)

---

### Verify Checksums (\`SHA256SUMS\`)

Download \`SHA256SUMS\` alongside your chosen package format and run:

\`\`\`bash
sha256sum --ignore-missing -c SHA256SUMS
\`\`\`

---

### Installation by Format

#### 1. AppImage (\`${AWCC_APPIMAGE_NAME}\`)
\`\`\`bash
chmod +x ./${AWCC_APPIMAGE_NAME}
# Launch Desktop GUI (or pass --sim for simulation mode):
./${AWCC_APPIMAGE_NAME}
# Invoke bundled CLI directly:
./${AWCC_APPIMAGE_NAME} --cli --help
\`\`\`

#### 2. Debian / Ubuntu 24.04+ (\`${AWCC_DEB_NAME}\`)
\`\`\`bash
sudo apt-get update
sudo apt-get install ./${AWCC_DEB_NAME}
\`\`\`

#### 3. Fedora 41+ (\`${AWCC_RPM_NAME}\`)
\`\`\`bash
sudo dnf install ./${AWCC_RPM_NAME}
\`\`\`

#### 4. Arch Linux / CachyOS (\`${AWCC_ARCH_PKG_NAME}\` or \`PKGBUILD\`)
\`\`\`bash
sudo pacman -U ./${AWCC_ARCH_PKG_NAME}
\`\`\`

#### 5. Binary Archive (\`${AWCC_TARBALL_NAME}\`)
\`\`\`bash
tar -xzf ./${AWCC_TARBALL_NAME}
cd ${AWCC_TARBALL_BASENAME}
./bin/awcc-cli --help
\`\`\`

---

### Hardware Prerequisites (Live Hardware Mode)

- **ACPI WMAX Thermal / Fan / G-Mode Control**: Requires the \`acpi_call\` kernel module (\`/proc/acpi/call\`) and either a running \`awccd\` socket service (\`/tmp/awcc.sock\`) or \`pkexec\` privileges.
- **Alienware ELC RGB Lighting (\`0x187c:0x0550\` / \`0x0551\`)**: Requires USB device read/write access via \`libusb-1.0\`.
- **Simulation Mode (\`--sim\`)**: Runs on any Linux \`x86_64\` system without Alienware hardware, root privileges, or a GPU.
EOF

if [[ "${DRY_RUN}" -eq 1 ]]; then
    echo "==> [DRY RUN] Validated tag ${RELEASE_TAG}, commit ${AWCC_GIT_SHA}, all 9 assets, and generated release notes:"
    cat "${NOTES_FILE}"
    exit 0
fi

echo "==> Inspecting existing GitHub releases for ${REPO} tag ${RELEASE_TAG}..."
EXISTING_RELEASE_JSON="$(gh api "repos/${REPO}/releases/tags/${RELEASE_TAG}" 2>/dev/null || true)"

if [[ -n "${EXISTING_RELEASE_JSON}" ]]; then
    IS_DRAFT="$(python3 -c 'import json,sys; print(str(json.loads(sys.stdin.read()).get("draft", False)).lower())' <<<"${EXISTING_RELEASE_JSON}")"
    if [[ "${IS_DRAFT}" == "false" ]]; then
        echo "==> Release ${RELEASE_TAG} is already published. Verifying source identity and recorded asset manifest for safe retry..."
        VERIFY_TMP="$(mktemp -d "${TMPDIR:-/tmp}/awcc-rs-pub-verify-XXXXXX")"
        gh release download "${RELEASE_TAG}" --repo "${REPO}" --dir "${VERIFY_TMP}" --pattern "build-manifest.json" --pattern "SHA256SUMS"
        python3 - "${EXISTING_RELEASE_JSON}" "${VERIFY_TMP}/build-manifest.json" "${AWCC_GIT_SHA}" "${AWCC_VERSION}" "${ALL_ASSETS[@]}" <<'PY'
import json, sys

rel = json.loads(sys.argv[1])
manifest_path = sys.argv[2]
expected_sha = sys.argv[3]
expected_ver = sys.argv[4]
expected_names = set(sys.argv[5:])

manifest = json.load(open(manifest_path, "r", encoding="utf-8"))
if manifest.get("git_commit") != expected_sha:
    raise SystemExit(
        f"ERROR: Published release {rel.get('tag_name')} was built from commit "
        f"{manifest.get('git_commit')}, which conflicts with current commit {expected_sha}. "
        "Refusing to overwrite published release."
    )
if manifest.get("version") != expected_ver:
    raise SystemExit(
        f"ERROR: Published release version {manifest.get('version')} != {expected_ver}."
    )

pub_assets = {a["name"]: a for a in rel.get("assets", [])}
missing = expected_names - set(pub_assets.keys())
extra = set(pub_assets.keys()) - expected_names
if missing or extra:
    raise SystemExit(
        f"ERROR: Published release asset set is incomplete or unexpected (missing={sorted(missing)}, extra={sorted(extra)}). "
        "Manual maintainer intervention required."
    )

for entry in manifest.get("assets", []):
    name = entry["name"]
    pub = pub_assets.get(name)
    if not pub or pub.get("size") != entry["size_bytes"]:
        raise SystemExit(f"ERROR: Published asset {name} size mismatch against recorded build-manifest.json")
    digest = pub.get("digest")
    if digest and digest != f"sha256:{entry['sha256']}":
        raise SystemExit(f"ERROR: Published asset {name} digest {digest} != sha256:{entry['sha256']}")

print(f"Published release {rel.get('tag_name')} verified against commit {expected_sha} and build-manifest.json.")
PY
        rm -rf "${VERIFY_TMP}"
        echo "==> Safe retry complete: published release ${RELEASE_TAG} is already intact and verified."
        exit 0
    fi
fi

# Check if a draft release for this tag already exists (gh api repos/:owner/:repo/releases lists drafts)
DRAFT_ID="$(gh api "repos/${REPO}/releases?per_page=50" \
    | python3 -c '
import json, sys
tag = sys.argv[1]
for r in json.load(sys.stdin):
    if r.get("draft") and r.get("tag_name") == tag:
        print(r["id"])
        break
' "${RELEASE_TAG}")"

if [[ -n "${DRAFT_ID}" ]]; then
    echo "==> Resuming existing draft release (id=${DRAFT_ID}) for ${RELEASE_TAG}..."
    gh release edit "${RELEASE_TAG}" \
        --repo "${REPO}" \
        --draft \
        --title "AWCC Linux ${RELEASE_TAG}" \
        --notes-file "${NOTES_FILE}"
else
    echo "==> Creating new draft release for ${RELEASE_TAG} (target ${AWCC_GIT_SHA})..."
    gh release create "${RELEASE_TAG}" \
        --repo "${REPO}" \
        --draft \
        --verify-tag \
        --title "AWCC Linux ${RELEASE_TAG}" \
        --notes-file "${NOTES_FILE}"
fi

UPLOAD_PATHS=()
for asset in "${ALL_ASSETS[@]}"; do
    UPLOAD_PATHS+=("${DIST_DIR}/${asset}")
done

echo "==> Uploading ${#UPLOAD_PATHS[@]} verified assets to draft release ${RELEASE_TAG}..."
gh release upload "${RELEASE_TAG}" "${UPLOAD_PATHS[@]}" --repo "${REPO}" --clobber

echo "==> Verifying uploaded draft release assets against local SHA256SUMS and file sizes..."
DRAFT_JSON="$(gh release view "${RELEASE_TAG}" --repo "${REPO}" --json assets,tagName,isDraft)"
python3 - "${DRAFT_JSON}" "${DIST_DIR}" "${ALL_ASSETS[@]}" <<'PY'
import hashlib, json, os, sys

rel = json.loads(sys.argv[1])
dist_dir = sys.argv[2]
expected = sys.argv[3:]

if not rel.get("isDraft"):
    raise SystemExit("ERROR: Expected release to still be in draft state during pre-publication check")

uploaded = {a["name"]: a for a in rel.get("assets", [])}
if len(uploaded) != len(expected) or set(uploaded.keys()) != set(expected):
    raise SystemExit(
        f"ERROR: Draft release asset mismatch. Expected {sorted(expected)}, got {sorted(uploaded.keys())}"
    )

for name in expected:
    local_path = os.path.join(dist_dir, name)
    local_size = os.path.getsize(local_path)
    local_sha = hashlib.sha256(open(local_path, "rb").read()).hexdigest()
    remote = uploaded[name]
    if remote.get("size") != local_size or local_size == 0:
        raise SystemExit(f"ERROR: Uploaded asset {name} size {remote.get('size')} != local {local_size}")
    remote_digest = remote.get("digest")
    if remote_digest and remote_digest != f"sha256:{local_sha}":
        raise SystemExit(f"ERROR: Uploaded asset {name} digest {remote_digest} != sha256:{local_sha}")

print(f"Verified all {len(expected)} draft release assets match local byte sizes and SHA-256 digests.")
PY

echo "==> Promoting draft release ${RELEASE_TAG} to published..."
gh release edit "${RELEASE_TAG}" --repo "${REPO}" --draft=false --latest
echo "==> Successfully published GitHub Release ${RELEASE_TAG} for commit ${AWCC_GIT_SHA}!"
