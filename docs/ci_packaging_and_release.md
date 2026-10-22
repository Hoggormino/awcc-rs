# CI, Linux Packaging, and Automated Release Guide

This document describes the continuous integration, Linux packaging, verification smoke tests, and automated GitHub Release pipeline for `awcc-rs` (`https://github.com/Hoggormino/awcc-rs`).

---

## 1. Workflow Triggers, Required Gates & Artifacts

The primary workflow [`ci-release.yml`](../.github/workflows/ci-release.yml) runs on:

| Trigger | Scope & Purpose | Publishes GitHub Release? |
| :--- | :--- | :--- |
| `pull_request` (`main`) | Runs all quality gates, Ubuntu 24.04 baseline build, all 5 package format builds, and clean Ubuntu 24.04 / Fedora 41 / Arch Linux container installation & smoke tests. Retains the standalone AppImage and complete verified bundle. Read-only (`contents: read`). | No |
| `push` (`main`) | Runs the full pipeline and retains the complete downloadable bundle (`awcc-rs-dist-<run_number>-<sha>`) for 30 days. | No |
| `workflow_dispatch` | Manual non-publishing validation run exercising the complete build, packaging, container install, and release dry-run pipeline, retaining the same downloadable artifacts. | No |
| `push` (`tags: v*`) | Validates strict `^v[0-9]+\.[0-9]+\.[0-9]+$` format and `Cargo.toml` version match, runs all quality and packaging/container checks against the exact tag commit, and automatically publishes the GitHub Release via draft-then-promote. | **Yes** (`publish-release` job) |

### Required Quality Gates

Every run executes the following gates before any package is accepted:

1. **Source & Tag Identity (`scripts/release-meta.sh`)**: Verifies workspace version consistency across all 5 crates (`awcc-core`, `awcc-platform`, `awcc-service`, `awcc-gui`, `awcc-cli`) and, on tags, enforces `^v[0-9]+\.[0-9]+\.[0-9]+$` and exact `Cargo.toml` version + commit SHA agreement.
2. **Workflow & Script Linting**: Pinned, SHA-256-verified `ShellCheck v0.11.0` on all packaging/release scripts and `actionlint v1.7.12` on `.github/workflows/ci-release.yml`.
3. **Desktop & AppStream Metadata Validation**: `desktop-file-validate` on `packaging/linux/awcc-gui.desktop` and `appstreamcli validate --no-net` on `packaging/linux/io.github.hoggormino.awcc_rs.metainfo.xml`.
4. **Rust Formatting**: `cargo fmt --all -- --check`
5. **Rust Linting**: `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
6. **Unit & Integration Tests**: `cargo test --workspace --all-features --locked`
7. **Baseline Binary Build & Symbol Audit**: `cargo build --workspace --release --locked --target x86_64-unknown-linux-gnu` on Ubuntu 24.04 LTS, verifying no `target-cpu=native` flags are present and enforcing a maximum required glibc symbol version of `GLIBC_2.39`.
8. **Isolated Unprivileged Smoke Tests (`scripts/smoke-test-installed.sh`)**: Executed outside the source checkout in an isolated temporary directory as an ordinary non-root user for every package format:
   - `awcc-cli --version` and `awcc-gui --version` in a bare environment (empty `PATH`, no state directory, missing device database): both binaries must report identical `version`/`commit`/`commit-source`/`dirty`/`source-date-epoch`/`target`/`profile`/`rustc`/`package-format`, and match the expected release version, commit, target, `dirty: false`, and the package's own format (`tarball`, `appimage`, `deb`, `rpm`, `arch`). See [`operation_outcomes_and_build_identity.md`](operation_outcomes_and_build_identity.md).
   - `awcc-cli --help`
   - `awcc-cli --sim device-info` (verifying `Alienware m15 R6`, `AMWW`, `Matched in DB : true`, and packaged `database.json` provenance)
   - `awcc-cli --sim query-mode`
   - Fail-closed negative check when `AWCC_DATABASE_PATH` points to a missing file
   - Deterministic software-rendered GUI screenshots (`awcc-gui --sim --screenshot`) across:
     - `1120×720` (`--scenario normal`)
     - `880×620` (`--scenario compact`)
     - `1120×720` (`--scenario rollback`, verifying error/rollback visual state)
9. **Native service lifecycle**: `scripts/test-native-package-lifecycle.py` exercises maintainer hooks with controlled systemd/kernel commands. Clean Ubuntu, Fedora and Arch containers install through an ordinary account's real `sudo apt-get`, `sudo dnf` or `sudo pacman` invocation and verify automatic authorization, boot enablement, service/module/udev files, and removal that preserves user policy/settings. Debian additionally tests purge after the CLI has been removed. These containers do not boot systemd; service start/stop command behavior is covered by the controlled lifecycle tests.

**Build identity gates.** `scripts/stage-release-payload.sh` runs both staged executables with `--version` and fails when the embedded version, target, commit, toolchain, or dirty state disagree with the release metadata (a stale cached build); `scripts/generate-release-manifest.sh` re-checks the binaries inside the final tarball, requires `awcc-cli` and `awcc-gui` to agree, records the binary-reported toolchain/`git_dirty`/`git_commit_source` in `build-manifest.json`, and refuses a dirty build for a `vX.Y.Z` tag. Each package ships `share/awcc/package-format` (written by that format's packager, not by the shared payload) so `--version` can report the installation format without compiling it into the common binaries.

### Development Artifact Locations

Open the [Actions page](https://github.com/Hoggormino/awcc-rs/actions/workflows/ci-release.yml), select the run for the commit you want to test, and use its download links in **Summary** or its **Artifacts** list. GitHub requires sign-in to download Actions artifacts. Development builds are available for pull requests as well as `main`, manual, and tag runs.

- **`awcc-rs-<version>-x86_64.AppImage`** (retained 14 days): A standalone AppImage download, uploaded as a single file without a ZIP wrapper once the Ubuntu build and smoke tests pass. Download it, run `chmod +x awcc-rs-<version>-x86_64.AppImage`, and launch it. The job summary includes a direct link and SHA-256 digest.
- **`awcc-rs-dist-<run_number>-<commit_sha>`** (retained 30 days): A ZIP containing all 5 package formats, `PKGBUILD`, `.SRCINFO`, `awcc-rs.install`, `build-manifest.json`, and `SHA256SUMS`. It is uploaded after every package's checks pass, with a link in the assembly job summary. The numeric run number keeps artifact names valid for PR refs such as `1/merge`.
- **`packages-ubuntu-<commit_sha>`** (retained 14 days): The tarball, AppImage and DEB used by downstream checks. On older runs without a standalone AppImage artifact, download and extract this ZIP to find the `.AppImage` file.
- **`packages-{fedora,arch}-<commit_sha>`** (retained 14 days): The RPM or Arch package and Arch recipe files. Keep `awcc-rs.install` beside `PKGBUILD` when building the Arch package yourself.
- **`smoke-screenshots-{ubuntu,deb,rpm,arch}-<commit_sha>`** (retained 14 days): Rendered PNG screenshots from each clean-environment smoke test.

These downloadable CI artifacts do not publish a GitHub Release; only a successful version-tag run publishes release assets.

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
| `.pkg.tar.zst` + `PKGBUILD` + `.SRCINFO` + `awcc-rs.install` | Built with `makepkg`, installed (`pacman -U`), and smoke-tested in a clean `archlinux:base-devel` (`x86_64`) container | Arch Linux / CachyOS (`x86_64`) |

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

# 6. Build and smoke-test the Arch package, PKGBUILD, .SRCINFO and awcc-rs.install (requires makepkg)
./scripts/package-arch.sh

# 7. Generate build-manifest.json and SHA256SUMS (once all 5 formats are present in dist/)
./scripts/generate-release-manifest.sh
```

Release metadata inputs (`AWCC_GIT_SHA`, `AWCC_GIT_DIRTY`, `AWCC_SOURCE_DATE_EPOCH`, `AWCC_TARGET`, `AWCC_RUSTC_VERSION`) come from `scripts/release-meta.sh` and are exactly what `crates/awcc-build-info/build.rs` embeds, so the binary and `build-manifest.json` cannot drift. A local build from a modified work tree reports `dirty: true`; a source archive without `.git` reports `commit-source: git-archive` (from `.git_archival.txt`) or `unknown`, and `dirty: unknown`.

To run the isolated CLI + headless GUI screenshot smoke test directly against any pair of binaries:

```bash
./scripts/smoke-test-installed.sh \
  --cli ./dist/staging/awcc-rs-0.1.5-x86_64-unknown-linux-gnu/bin/awcc-cli \
  --gui ./dist/staging/awcc-rs-0.1.5-x86_64-unknown-linux-gnu/bin/awcc-gui \
  --database ./dist/staging/awcc-rs-0.1.5-x86_64-unknown-linux-gnu/share/awcc/database.json \
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
   # Or download PKGBUILD and awcc-rs.install together, then build:
   makepkg -si
   ```

---

## 5. Hardware Prerequisites & Limits of Simulation-Based CI

### Real Hardware Prerequisites

Fresh native-package installation configures the background service: it enables and
starts it on a running systemd host and authorizes a validated installing
sudo/polkit account when there is no previous authorization policy. The GUI stays
unprivileged. If installation cannot identify the desktop account, or an older
installation needs repair, **Set up service** in the GUI completes authorization
and startup with administrator approval. Updates retain authorization and
intentionally disabled/stopped services; they only restart an active service.
Debian startup respects policy-rc.d, and offline/chroot installations defer live
startup.

Hardware access still requires:

1. **ACPI WMAX Thermal, Fan Boost, TCC Offset & G-Mode Control**:
   - Requires the `acpi_call` kernel module (`acpi-call-dkms` on Ubuntu/Debian, `akmod-acpi_call` on Fedora, `acpi_call` / `acpi_call-dkms` on Arch/CachyOS) exposing `/proc/acpi/call`.
   - Native packages attempt to load an already installed module and ship an app-owned module-loading entry for subsequent boots. They do not download/compile drivers or guess kernel headers during installation.
   - Normal thermal control uses `awccd.service` through `/run/awcc/awcc.sock`. The installed daemon performs privileged operations; the GUI and development `cargo run` stay unprivileged. The old per-operation `pkexec` path is not used by the normal daemon-backed client.
2. **Alienware USB HID ELC Lighting (`0x187c:0x0550` / `0x187c:0x0551`)**:
   - Native packages install `70-awcc-rs.rules`, scoped to these controllers, granting `uaccess` to the active local desktop session. Rules are reloaded and matching devices refreshed during package lifecycle operations. No broad kernel-file permissions are changed.
   - Standalone AppImages/manual installs still depend on suitable host USB permissions. Remote/SSH users do not automatically receive active-local-session USB access.

### Removal and Purge

All native formats stop/disable the daemon before removing its executable, reload
systemd afterwards, and let the package manager remove its unit, module-loading
entry, udev rules and desktop files. Ordinary removal retains service settings and
authorization. Package upgrades do not run final-removal cleanup.

Debian's `apt purge awcc-rs` additionally deletes known AWCC-generated settings,
authorization and service logs; its `postrm` works even after the application
binary has gone. Cleanup validates paths and preserves unrelated files. RPM and
Arch have no separate Debian-style purge operation and retain application data
on ordinary removal. Home-directory user data and already loaded shared kernel
modules are left to their owners.

Native hooks and explicit service setup share `/var/lib/awcc/.install.lock`.
Purge retains this empty coordination file to prevent concurrent operations from
locking different inodes. Arch uses a package removal `PreTransaction` hook with
`AbortOnFail`, so a failure to stop the daemon can block removal before its files
disappear.

The USB access rule is removed and udev is refreshed. An ACL already granted to
a connected controller may remain until reconnection or reboot: systemd only
runs its [uaccess rule](https://github.com/systemd/systemd/blob/v255/rules.d/73-seat-late.rules.in)
for devices that still carry that tag. Cleanup does not guess which ACL entries
belong to an administrator or another rule and delete them.

AppImage-managed service removal has an explicit GUI option to delete saved
settings, equivalent to `--uninstall-service --purge`. Merely deleting an AppImage
does not remove the persistent service that it installed. Native services must
be removed with their package manager, even when controlled from an AppImage GUI.

### What Simulation-Based CI Validates vs. Physical Hardware

- **Validated in CI**: Binary startup and dynamic linker compatibility on clean Ubuntu 24.04, Fedora 41, and Arch Linux; actual package-hook execution and installing-user authorization; removal/purge and offline lifecycle regression tests; database lookup and DMI/ACPI prefix selection; simulated thermal state transitions, G-Mode rollback, fan curves and TCC offset validation; USB HID ELC packet construction; and software-rendered GUI layout. Service enable/start/upgrade behavior is exercised with controlled systemd commands; the package containers do not boot a real systemd daemon.
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
   - Once `quality-and-build`, `test-deb-ubuntu-24-04`, `package-and-test-rpm-fedora`, `package-and-test-arch`, and `assemble-and-verify-bundle` all pass, the `publish-release` job creates a draft GitHub Release, uploads all 10 verified assets (`.tar.gz`, `.AppImage`, `.deb`, `.rpm`, `.pkg.tar.zst`, `PKGBUILD`, `.SRCINFO`, `awcc-rs.install`, `build-manifest.json`, `SHA256SUMS`), verifies every uploaded asset's size and SHA-256 digest against the local manifest, and automatically publishes the release.

---

## 7. Failed-Release Recovery & Safe Retry Behavior

The `publish-release` job (`scripts/publish-release.sh`) is serialized per tag (`concurrency: release-publish-vX.Y.Z`, `cancel-in-progress: false`) and designed for safe retries:

- **Failure Before or During Draft Upload**:
  - The release remains an unpublished **draft**. Re-running the failed workflow job in GitHub Actions automatically locates the existing draft for `vX.Y.Z`, replaces any partial assets (`--clobber`), re-verifies all 10 assets against `SHA256SUMS` and `build-manifest.json`, and publishes the draft once complete.
- **Re-running After Publication Succeeds**:
  - If `vX.Y.Z` is already published, `scripts/publish-release.sh` downloads the published `build-manifest.json` and `SHA256SUMS`, verifies that the published release's `git_commit` matches `HEAD` and that all 10 assets are present with matching byte sizes and SHA-256 digests, and exits `0` without modifying or overwriting the published release.
- **Conflicting Tag or Incomplete Published Release**:
  - If a published release already exists for `vX.Y.Z` with a different commit SHA or missing/corrupted assets, the script fails closed with an explicit error message and never silently overwrites published assets or moves tags.

---

## 8. Required Repository Settings

- **GitHub Actions Permissions**: Standard default workflow permissions work out of the box because [`ci-release.yml`](../.github/workflows/ci-release.yml) declares top-level `permissions: contents: read` and requests `permissions: contents: write` on the `publish-release` job. Ensure repository settings under **Settings → Actions → General → Workflow permissions** allow workflows to request write permissions (or set **Read and write permissions**). No Personal Access Token (PAT) is required.
