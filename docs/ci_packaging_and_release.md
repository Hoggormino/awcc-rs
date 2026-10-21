# CI, Linux Packaging, and Automated Release Guide

This document describes the continuous integration, Linux packaging, verification smoke tests, and automated GitHub Release pipeline for `awcc-rs` (`https://github.com/Hoggormino/awcc-rs`).

---

## 1. Workflow Triggers, Required Gates & Artifacts

The primary workflow [`ci-release.yml`](../.github/workflows/ci-release.yml) runs on:

| Trigger | Scope & Purpose | Publishes GitHub Release? |
| :--- | :--- | :--- |
| `pull_request` (`main`) | Runs all quality gates, Ubuntu 24.04 baseline build, all 5 package format builds, and clean Ubuntu 24.04 / Fedora 41 / Arch Linux container installation & smoke tests. Read-only (`contents: read`). | No |
| `push` (`main`) | Runs the full pipeline and retains the complete downloadable bundle (`awcc-rs-dist-main-<sha>`) for 30 days. | No |
| `workflow_dispatch` | Manual non-publishing validation run exercising the complete build, packaging, container install, and release dry-run pipeline, retaining downloadable artifacts (`awcc-rs-dist-<ref>-<sha>`). | No |
| `push` (`tags: v*`) | Validates strict `^v[0-9]+\.[0-9]+\.[0-9]+$` format and `Cargo.toml` version match, runs all quality and packaging/container checks against the exact tag commit, and automatically publishes the GitHub Release via draft-then-promote. | **Yes** (`publish-release` job) |

### Required Quality Gates

Every run executes the following gates before any package is accepted:

1. **Source & Tag Identity (`scripts/release-meta.sh`)**: Verifies workspace version consistency across all 5 crates (`awcc-core`, `awcc-platform`, `awcc-service`, `awcc-gui`, `awcc-cli`) and, on tags, enforces `^v[0-9]+\.[0-9]+\.[0-9]+$` and exact `Cargo.toml` version + commit SHA agreement.
2. **Workflow & Script Linting**: Pinned, SHA-256-verified `ShellCheck v0.11.0` on all packaging/release scripts and `actionlint v1.7.12` on `.github/workflows/ci-release.yml`.
3. **Desktop & AppStream Metadata Validation**: `desktop-file-validate` on `packaging/linux/awcc-gui.desktop` and `appstreamcli validate --no-net` on `packaging/linux/io.github.hoggormino.awcc_rs.metainfo.xml`.
4. **Rust Formatting**: `cargo fmt --all -- --check`
5. **Rust Linting**: `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
6. **Unit & Integration Tests**: `cargo test --workspace --all-features --locked`
7. **Differential Conformance Suite**: `cargo test -p awcc-platform --test conformance_suite --locked` (validating 54 device entries, ACPI prefix precedence, hex parser edge cases, per-model mode overrides, and WMAX shell traces against committed baseline vectors).
8. **Baseline Binary Build & Symbol Audit**: `cargo build --workspace --release --locked --target x86_64-unknown-linux-gnu` on Ubuntu 24.04 LTS, verifying no `target-cpu=native` flags are present and enforcing a maximum required glibc symbol version of `GLIBC_2.39`.
9. **Isolated Unprivileged Smoke Tests (`scripts/smoke-test-installed.sh`)**: Executed outside the source checkout in an isolated temporary directory as an ordinary non-root user for every package format:
   - `awcc-cli --version` and `awcc-gui --version` in a bare environment (empty `PATH`, no state directory, missing device database): both binaries must report identical `version`/`commit`/`commit-source`/`dirty`/`source-date-epoch`/`target`/`profile`/`rustc`/`package-format`, and match the expected release version, commit, target, `dirty: false`, and the package's own format (`tarball`, `appimage`, `deb`, `rpm`, `arch`). See [`operation_outcomes_and_build_identity.md`](operation_outcomes_and_build_identity.md).
   - `awcc-cli --help`
   - `awcc-cli --sim device-info` (verifying `Alienware m15 R6`, `AMWW`, `Matched in DB : true`, and packaged `database.json` provenance)
   - `awcc-cli --sim query-mode`
   - Fail-closed negative check when `AWCC_DATABASE_PATH` points to a missing file
   - Deterministic software-rendered GUI screenshots (`awcc-gui --sim --screenshot`) across:
     - `1120×720` (`--scenario normal`)
     - `880×620` (`--scenario compact`)
     - `1120×720` (`--scenario rollback`, verifying error/rollback visual state)

**Build identity gates.** `scripts/stage-release-payload.sh` runs both staged executables with `--version` and fails when the embedded version, target, commit, toolchain, or dirty state disagree with the release metadata (a stale cached build); `scripts/generate-release-manifest.sh` re-checks the binaries inside the final tarball, requires `awcc-cli` and `awcc-gui` to agree, records the binary-reported toolchain/`git_dirty`/`git_commit_source` in `build-manifest.json`, and refuses a dirty build for a `vX.Y.Z` tag. Each package ships `share/awcc/package-format` (written by that format's packager, not by the shared payload) so `--version` can report the installation format without compiling it into the common binaries.

### Development Artifact Locations

On successful `main`, `workflow_dispatch`, and tag runs, the `assemble-and-verify-bundle` job uploads:
- **`awcc-rs-dist-<ref_name>-<commit_sha>`** (retained 30 days): Contains all 5 package formats, `PKGBUILD`, `.SRCINFO`, `build-manifest.json`, and `SHA256SUMS`.
- **`smoke-screenshots-{ubuntu,deb,rpm,arch}-<commit_sha>`** (retained 14 days): Contains the rendered PNG screenshots from each clean-environment smoke test.

---

## 2. Toolchain, Baseline Environments, Packaging Tools & Action Version Policy

### Rust Toolchain

- **Workspace MSRV (`Cargo.toml`)**: `rust-version = "1.92"` (reconciled with `slint = "1.18.1"` and `slint-build = "1.18.1"`, which require Rust `1.92`).
- **Pinned Compiler (`rust-toolchain.toml`)**: `1.95.0` (`x86_64-unknown-linux-gnu`, components `rustfmt` and `clippy`).
- **Lockfile Policy**: `Cargo.lock` is committed and `--locked` is passed to all Cargo commands in CI and packaging scripts.

### Validated Baseline Environments

| Format | Build / Validation Environment | Minimum Runtime Baseline |
| :--- | :--- | :--- |
| Common Payload & `.tar.gz` | Ubuntu 24.04 LTS (`ubuntu-24.04` runner, `x86_64`) | `glibc >= 2.39`, `libusb-1.0.so.0`, `libfontconfig.so.1` |
| `AppImage` | Ubuntu 24.04 LTS (`x86_64`), tested via `--appimage-extract` (no FUSE required) | Ubuntu 24.04+ or any `x86_64` distro with `glibc >= 2.39` and host `libfontconfig.so.1` |
| `.deb` | Built on Ubuntu 24.04 LTS; installed & smoke-tested in a clean `ubuntu:24.04` container via `apt-get install` | Ubuntu 24.04 LTS (`noble`, `amd64`) |
| `.rpm` | Built, installed (`dnf install`), and smoke-tested in a clean `fedora:41` (`x86_64`) container | Fedora Linux 41+ (`x86_64`) |
| `.pkg.tar.zst` + `PKGBUILD` + `.SRCINFO` | Built with `makepkg`, installed (`pacman -U`), and smoke-tested in a clean `archlinux:base-devel` (`x86_64`) container | Arch Linux / CachyOS (`x86_64`) |

### Selected Packaging Tools & Rationale

We reuse the single Ubuntu 24.04 `x86_64-unknown-linux-gnu` release payload across all five formats (`scripts/stage-release-payload.sh`) so every package ships identical `awcc-cli` and `awcc-gui` binaries:

- **`.tar.gz` (`scripts/package-tarball.sh`)**: GNU `tar` (`--sort=name --owner=0 --group=0 --numeric-owner`) + `gzip -n -9` for deterministic archive ordering and metadata.
- **`AppImage` (`scripts/package-appimage.sh`)**: Explicit `AppDir` staging + pinned, SHA-256-verified `AppImage/appimagetool` `1.9.1` (`ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0`) run with `APPIMAGE_EXTRACT_AND_RUN=1`. Bundles `libusb-1.0.so.0` and `winit`'s dynamically loaded (`dlopen`) non-base X11/Wayland input/cursor libraries (`libxkbcommon*`, `libXcursor`, `libXi`, `libXrender`, `libXfixes`, `libXext`) while strictly excluding `glibc` and host Mesa/EGL/GL/Wayland/X11/fontconfig drivers per the official AppImage excludelist.
- **`.deb` (`scripts/package-deb.sh`)**: Native `dpkg-deb --root-owner-group` + `dpkg-shlibdeps` on Ubuntu 24.04 (with a deterministic `ar` + `tar` fallback for local builds on non-Debian hosts). Avoids third-party Rust recompilation wrappers and computes shared-library dependencies directly from the staged ELF binaries.
- **`.rpm` (`scripts/package-rpm.sh`)**: Standard `rpmbuild` using [`packaging/linux/awcc-rs.spec.in`](../packaging/linux/awcc-rs.spec.in), leveraging RPM's native ELF `find-requires` generator (`libusb-1.0.so.0()(64bit)`, `libfontconfig.so.1()(64bit)`, `libc.so.6(GLIBC_2.39)(64bit)`).
- **Arch `.pkg.tar.zst` (`scripts/package-arch.sh`)**: Standard `makepkg` + `makepkg --printsrcinfo` from [`packaging/linux/PKGBUILD.in`](../packaging/linux/PKGBUILD.in), consuming the finalized versioned release tarball (`awcc-rs-<version>-x86_64-unknown-linux-gnu.tar.gz`) and its real SHA-256 checksum.

### GitHub Action & Pinned Tool Versions

Third-party GitHub Actions use verified floating major tags (`owner/action@vN`), kept current via weekly Dependabot PRs ([`.github/dependabot.yml`](../.github/dependabot.yml)):

| Action / Tool | Selected Reference / Version | Latest Stable Release Checked | Official Source |
| :--- | :--- | :--- | :--- |
| `actions/checkout` | `actions/checkout@v7` | `v7.0.1` | https://github.com/actions/checkout/releases/tag/v7.0.1 |
| `actions/upload-artifact` | `actions/upload-artifact@v7` | `v7.0.2` | https://github.com/actions/upload-artifact/releases/tag/v7.0.2 |
| `actions/download-artifact` | `actions/download-artifact@v8` | `v8.0.2` | https://github.com/actions/download-artifact/releases/tag/v8.0.2 |
| `Swatinem/rust-cache` | `Swatinem/rust-cache@v2` | `v2.9.2` | https://github.com/Swatinem/rust-cache/releases/tag/v2.9.2 |
| `dtolnay/rust-toolchain` | `dtolnay/rust-toolchain@v1` | `v1` | https://github.com/dtolnay/rust-toolchain/blob/v1/action.yml |
| `AppImage/appimagetool` | `1.9.1` (SHA-256 verified) | `1.9.1` | https://github.com/AppImage/appimagetool/releases/tag/1.9.1 |
| `rhysd/actionlint` | `v1.7.12` (SHA-256 verified) | `v1.7.12` | https://github.com/rhysd/actionlint/releases/tag/v1.7.12 |
| `koalaman/shellcheck` | `v0.11.0` (SHA-256 verified) | `v0.11.0` | https://github.com/koalaman/shellcheck/releases/tag/v0.11.0 |

---

## 3. Building & Validating Packages Locally

From the repository root:

```bash
# 1. Stage the release payload (builds release binaries with --locked if --build is passed)
./scripts/stage-release-payload.sh --build

# 2. Build and smoke-test the binary .tar.gz archive
./scripts/package-tarball.sh

# 3. Build and smoke-test the x86_64 AppImage (extracts with --appimage-extract; no FUSE required)
./scripts/package-appimage.sh

# 4. Build and validate the Debian/Ubuntu .deb package
./scripts/package-deb.sh

# 5. Build and validate the Fedora .rpm package (requires rpmbuild)
./scripts/package-rpm.sh

# 6. Build and smoke-test the Arch Linux PKGBUILD, .SRCINFO, and .pkg.tar.zst (requires makepkg)
./scripts/package-arch.sh

# 7. Generate build-manifest.json and SHA256SUMS (once all 5 formats are present in dist/)
./scripts/generate-release-manifest.sh
```

Release metadata inputs (`AWCC_GIT_SHA`, `AWCC_GIT_DIRTY`, `AWCC_SOURCE_DATE_EPOCH`, `AWCC_TARGET`, `AWCC_RUSTC_VERSION`) come from `scripts/release-meta.sh` and are exactly what `crates/awcc-build-info/build.rs` embeds, so the binary and `build-manifest.json` cannot drift. A local build from a modified work tree reports `dirty: true`; a source archive without `.git` reports `commit-source: git-archive` (from `.git_archival.txt`) or `unknown`, and `dirty: unknown`.

To run the isolated CLI + headless GUI screenshot smoke test directly against any pair of binaries:

```bash
./scripts/smoke-test-installed.sh \
  --cli ./dist/staging/awcc-rs-0.1.0-x86_64-unknown-linux-gnu/bin/awcc-cli \
  --gui ./dist/staging/awcc-rs-0.1.0-x86_64-unknown-linux-gnu/bin/awcc-gui \
  --database ./dist/staging/awcc-rs-0.1.0-x86_64-unknown-linux-gnu/share/awcc/database.json \
  --label local \
  --expect-package-format unknown   # optional identity expectations: --expect-version/-commit/-target/-dirty
```

---

## 4. Installing Each Format & Verifying `SHA256SUMS`

### Verify Checksums

Download `SHA256SUMS` from the GitHub Release alongside your package file and run:

```bash
sha256sum --ignore-missing -c SHA256SUMS
```

### Usage by Package Format

1. **Binary Archive (`awcc-rs-<version>-x86_64-unknown-linux-gnu.tar.gz`)**:
   ```bash
   tar -xzf awcc-rs-<version>-x86_64-unknown-linux-gnu.tar.gz
   cd awcc-rs-<version>-x86_64-unknown-linux-gnu
   ./bin/awcc-cli --help
   AWCC_DATABASE_PATH=./share/awcc/database.json ./bin/awcc-gui --sim
   ```
2. **AppImage (`awcc-rs-<version>-x86_64.AppImage`)**:
   ```bash
   chmod +x awcc-rs-<version>-x86_64.AppImage
   # Launch the Slint Material 3 GUI by default:
   ./awcc-rs-<version>-x86_64.AppImage
   # Launch the GUI in simulated mode:
   ./awcc-rs-<version>-x86_64.AppImage --sim
   # Invoke the bundled awcc-cli via --cli (or by symlinking the AppImage as awcc-cli):
   ./awcc-rs-<version>-x86_64.AppImage --cli --help
   ./awcc-rs-<version>-x86_64.AppImage --cli --sim device-info
   ```
3. **Debian / Ubuntu 24.04+ (`awcc-rs_<version>_amd64.deb`)**:
   ```bash
   sudo apt-get update
   sudo apt-get install ./awcc-rs_<version>_amd64.deb
   awcc-cli --help
   awcc-gui --sim
   ```
4. **Fedora 41+ (`awcc-rs-<version>-1.x86_64.rpm`)**:
   ```bash
   sudo dnf install ./awcc-rs-<version>-1.x86_64.rpm
   awcc-cli --help
   awcc-gui --sim
   ```
5. **Arch Linux / CachyOS (`awcc-rs-<version>-1-x86_64.pkg.tar.zst` or `PKGBUILD`)**:
   ```bash
   # Install pre-built package directly:
   sudo pacman -U ./awcc-rs-<version>-1-x86_64.pkg.tar.zst
   # Or build from the release PKGBUILD:
   makepkg -si
   ```

---

## 5. Hardware Prerequisites & Limits of Simulation-Based CI

### Real Hardware Prerequisites

Package installation scripts (`postinst`, `%post`, `.INSTALL`) intentionally perform **no** privileged hardware mutations, kernel module loading, or service activation. To control physical Alienware / Dell G-Series hardware at runtime:

1. **ACPI WMAX Thermal, Fan Boost, TCC Offset & G-Mode Control**:
   - Requires the `acpi_call` kernel module (`acpi-call-dkms` on Ubuntu/Debian, `akmod-acpi_call` on Fedora, `acpi_call` / `acpi_call-dkms` on Arch/CachyOS) exposing `/proc/acpi/call`.
   - Requires the packaged `awccd.service` (privileged daemon listening on `/run/awcc/awcc.sock`, enabled by the user with `systemctl enable --now awccd.service`) or root access to write `\_SB.<PREFIX>.WMAX` commands to `/proc/acpi/call`. The per-call `pkexec` fallback is opt-in via `AWCC_ALLOW_PKEXEC=1`.
2. **Alienware USB HID ELC Lighting (`0x187c:0x0550` / `0x187c:0x0551`)**:
   - Requires read/write access to the USB device via `libusb-1.0` (either via a udev rule granting `MODE="0660", TAG+="uaccess"` for vendor `187c` products `0550`/`0551`, or root privileges).

### What Simulation-Based CI Validates vs. Physical Hardware

- **Validated in CI (`--sim`)**: Binary startup and dynamic linker compatibility on clean Ubuntu 24.04, Fedora 41, and Arch Linux; `database.json` parsing and 4-tier lookup precedence; DMI/ACPI prefix selection; thermal state machine transitions, G-Mode rollback, manual fan curves, and TCC offset validation; USB HID ELC packet construction; and deterministic software-rendered GUI layout (`1120×720` normal, `880×620` compact, and `rollback` error state).
- **Requires Physical Hardware**: Actual BIOS `\_SB.<PREFIX>.WMAX` firmware execution over `/proc/acpi/call`, physical fan tachometer response, live `hwmon` sensor readings, interactive Wayland/X11 GPU window presentation, and physical USB LED controller transfers.

---

## 6. Maintainer Procedure for the First Release (and Subsequent Releases)

Version bumps and release tags are maintainer-controlled:

1. **Update Version (if bumping from current `Cargo.toml`)**:
   - Update `version = "X.Y.Z"` under `[workspace.package]` in [`Cargo.toml`](../Cargo.toml).
   - Update `Cargo.lock` by running `cargo check --workspace` and verify `git diff Cargo.toml Cargo.lock`.
2. **Verify Local Gates**:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo test --workspace --all-features --locked
   ```
3. **Commit and Push to `main`**:
   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "Release vX.Y.Z"
   git push origin main
   ```
   Wait for the `CI, Packaging & Release` workflow on `main` to succeed (or run a manual `workflow_dispatch` check).
4. **Create and Push the Annotated `vX.Y.Z` Tag**:
   ```bash
   git tag -a vX.Y.Z -m "AWCC Linux vX.Y.Z"
   git push origin vX.Y.Z
   ```
5. **Monitor Automatic Publication**:
   - Pushing `vX.Y.Z` triggers `CI, Packaging & Release`.
   - Once `quality-and-build`, `test-deb-ubuntu-24-04`, `package-and-test-rpm-fedora`, `package-and-test-arch`, and `assemble-and-verify-bundle` all pass, the `publish-release` job creates a draft GitHub Release, uploads all 9 verified assets (`.tar.gz`, `.AppImage`, `.deb`, `.rpm`, `.pkg.tar.zst`, `PKGBUILD`, `.SRCINFO`, `build-manifest.json`, `SHA256SUMS`), verifies every uploaded asset's size and SHA-256 digest against the local manifest, and automatically publishes the release.

---

## 7. Failed-Release Recovery & Safe Retry Behavior

The `publish-release` job (`scripts/publish-release.sh`) is serialized per tag (`concurrency: release-publish-vX.Y.Z`, `cancel-in-progress: false`) and designed for safe retries:

- **Failure Before or During Draft Upload**:
  - The release remains an unpublished **draft**. Re-running the failed workflow job in GitHub Actions automatically locates the existing draft for `vX.Y.Z`, replaces any partial assets (`--clobber`), re-verifies all 9 assets against `SHA256SUMS` and `build-manifest.json`, and publishes the draft once complete.
- **Re-running After Publication Succeeds**:
  - If `vX.Y.Z` is already published, `scripts/publish-release.sh` downloads the published `build-manifest.json` and `SHA256SUMS`, verifies that the published release's `git_commit` matches `HEAD` and that all 9 assets are present with matching byte sizes and SHA-256 digests, and exits `0` without modifying or overwriting the published release.
- **Conflicting Tag or Incomplete Published Release**:
  - If a published release already exists for `vX.Y.Z` with a different commit SHA or missing/corrupted assets, the script fails closed with an explicit error message and never silently overwrites published assets or moves tags.

---

## 8. Required Repository Settings

- **GitHub Actions Permissions**: Standard default workflow permissions work out of the box because [`ci-release.yml`](../.github/workflows/ci-release.yml) declares top-level `permissions: contents: read` and requests `permissions: contents: write` on the `publish-release` job. Ensure repository settings under **Settings → Actions → General → Workflow permissions** allow workflows to request write permissions (or set **Read and write permissions**). No Personal Access Token (PAT) is required.
