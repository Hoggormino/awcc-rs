# AWCC Linux — Rust + Slint (`@material`)

Thermal, fan and lighting control for Alienware and Dell hardware on Linux, built
in **Rust** with a compact desktop **Material 3 GUI** using **Slint (`@material`
1.1.0)**, deterministic offline tests and device-specific validation contracts.

---

## Workspace Structure

| Crate / Directory | Description |
| :--- | :--- |
| [`crates/awcc-core`](crates/awcc-core) | Pure domain model: 4-tier `database.json` resolution, DMI/ACPI prefix rules, `ThermalMode`, `ThermalModeState` (`Unknown != Manual`), `SensorReading<T>` (`Unavailable != 0`), `OperationLifecycle`, custom 4-point monotonic fan curves, TCC offset, and USB HID ELC (`0x187c:0x0550`/`0x0551`) packet builders. |
| [`crates/awcc-diagnostics`](crates/awcc-diagnostics) | Bounded sequence-aware event & audit rings (`SeqRing`), `tracing` subscriber layer, non-blocking rotating session file sink (`$XDG_STATE_HOME/awcc-rs/logs`, 50 MiB budget), PII redactor, panic hook crash capture, and passive `.zip` support-report bundler. |
| [`crates/awcc-platform`](crates/awcc-platform) | Hardware transport traits (`ThermalTransport`, `StateStorage`, `ElcUsbTransport`) with both live Linux backends (`/proc/acpi/call` WMAX, `hwmon`, `rusb` USB HID) and deterministic `SimBackend` / `MockElcDevice`. |
| [`crates/awcc-service`](crates/awcc-service) | Non-blocking background worker (`ThermalServiceHandle`), privileged Unix-socket daemon (`/run/awcc/awcc.sock` allowlist executor), and `evdev` hotkey keybinder (`G-Mode` / brightness cycle). |
| [`crates/awcc-gui`](crates/awcc-gui) | Slint (`@material` 1.1.0) desktop GUI (`awcc-gui`) with live hardware auto-detection, interactive simulation scenario controls (`--sim`), passive support-report export, and headless PNG screenshot export (`--screenshot`). |
| [`crates/awcc-cli`](crates/awcc-cli) | Command-line interface (`awcc-cli`) supporting thermal modes, G-Mode, manual fan boost, TCC offset, custom fan curves, RGB lighting effects, `raw-status` diagnostics, and passive `diagnostics export` / `diagnostics summary`. |
| [`packaging/linux/`](packaging/linux) | Linux desktop integration (`awcc-gui.desktop`, icons, AppStream metainfo, `AppRun`) and RPM/Arch package templates (`awcc-rs.spec.in`, `PKGBUILD.in`). |
| [`scripts/`](scripts) | Reusable payload staging, multi-distro package builders (`.tar.gz`, `AppImage`, `.deb`, `.rpm`, `.pkg.tar.zst`), isolated smoke tests, and release publication scripts. |
| [`ui-libraries/material-1.1.0`](ui-libraries/material-1.1.0) | Vendored official Slint Material 3 component library (`@material` `1.1.0`, MIT license). |
| [`docs/`](docs) | Engineering specifications: [architecture](docs/architecture.md), Figma/Material 3 mapping, [`operation_outcomes_and_build_identity.md`](docs/operation_outcomes_and_build_identity.md), [`structured_logging_and_support_export.md`](docs/structured_logging_and_support_export.md), [`agent_issue_investigation_workflow.md`](docs/agent_issue_investigation_workflow.md), and [`ci_packaging_and_release.md`](docs/ci_packaging_and_release.md). |

---

## Clone, Build, and Test

Toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (`1.95.0`, workspace MSRV `1.92`):

```bash
git clone https://github.com/Hoggormino/awcc-rs.git
cd awcc-rs

# Build workspace (debug or release, locked to Cargo.lock)
cargo build --workspace --locked
cargo build --workspace --release --locked

# Run quality gates (format, Clippy, unit and integration tests)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

---

## Running the GUI (`awcc-gui`) & CLI (`awcc-cli`)

### 1. Launch the Desktop GUI (`awcc-gui`)

For a first launch without accessing a device, run:

```bash
./scripts/run-sim.sh
```

The launcher builds with the pinned toolchain and lockfile, then opens the native
Slint application against the in-memory simulator. It works from any current
directory and forwards GUI options, for example:

```bash
/path/to/awcc-rs/scripts/run-sim.sh --screenshot /tmp/awcc-preview.png --scenario lighting
```

```bash
# Auto-detect live Alienware hardware (or pass --sim for simulation mode)
cargo run -p awcc-gui

# Explicitly run in Simulated Hardware Mode (Alienware m15 R6 · \_SB.AMWW.WMAX)
cargo run -p awcc-gui --locked -- --sim
```

### 2. Run CLI Commands & Passive Diagnostics (`awcc-cli`)
```bash
cargo run -p awcc-cli -- --help
cargo run -p awcc-cli -- --version
cargo run -p awcc-cli -- device-info
cargo run -p awcc-cli -- --sim device-info
cargo run -p awcc-cli -- -v --sim query-mode
cargo run -p awcc-cli -- diagnostics summary
cargo run -p awcc-cli -- diagnostics export --output awcc-support.zip
```

### 3. Render Deterministic Screenshots (`1120×720` Normal, `880×620` Compact, Rollback, Lighting, Diagnostics)
```bash
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-normal-1120x720.png --width 1120 --height 720 --scenario normal
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-compact-880x620.png --width 880 --height 620 --scenario compact
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-rollback-1120x720.png --width 1120 --height 720 --scenario rollback
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-lighting-1120x720.png --width 1120 --height 720 --scenario lighting
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-diagnostics-1120x720.png --width 1120 --height 720 --scenario diagnostics
```

---

## Packaging, CI & Releases

Every push to `main`, pull request, manual `workflow_dispatch` run, and `vX.Y.Z` release tag builds and smoke-tests all five Linux `x86_64` package formats:

1. **Binary Archive**: `awcc-rs-<version>-x86_64-unknown-linux-gnu.tar.gz`
2. **AppImage**: `awcc-rs-<version>-x86_64.AppImage` (launches `awcc-gui` by default; pass `--cli <args>` or symlink as `awcc-cli` for CLI mode)
3. **Debian / Ubuntu 24.04+**: `awcc-rs_<version>_amd64.deb`
4. **Fedora 41+**: `awcc-rs-<version>-1.x86_64.rpm`
5. **Arch Linux / CachyOS**: `awcc-rs-<version>-1-x86_64.pkg.tar.zst` + `PKGBUILD` + `awcc-rs.install` + `.SRCINFO`

### Install once, then launch as your user

On a normal systemd desktop, a fresh DEB, RPM or Arch package installation enables
and starts `awccd.service`. Installation through `sudo` or polkit also authorizes
the identified installing account when no authorization policy already exists.
Launch `awcc-gui` as your normal user afterwards. Native packages include boot-time
loading for an installed `acpi_call` module and USB access rules for the supported
Alienware lighting controllers.

If a software center or root shell cannot identify your desktop account, or you
are upgrading an older installation that was never configured, choose **Set up
service** in the GUI. One administrator prompt authorizes your account and enables
and starts the existing service. The CLI equivalent is `awcc-cli service setup`.
Upgrades preserve authorization and deliberately disabled/stopped services.

For an AppImage, open it normally and use **Set up service** to reach the installation
confirmation. The installed service continues running after the GUI closes. You
can also use the AppImage with an existing native-package service; the GUI's setup
action configures it, while updates/removal remain with its package manager.

Package removal stops/disables the service and removes package-owned integration
files. Debian `apt purge awcc-rs` also removes AWCC's generated system settings and
authorization; ordinary removal retains them. AppImage removal offers **Also delete
saved service settings**, equivalent to `--uninstall-service --purge`. See the
[installation guide](packaging/linux/INSTALL.txt) for cleanup details and kernel
driver prerequisites.

### Saved thermal preset

Selecting a firmware preset such as Cool or Quiet saves the accepted choice in
the background service. The service restores it when it starts, before the GUI
opens. Failed requests preserve the previous choice. Manual fan control and
G-Mode are transient and clear automatic preset restoration; custom fan curves
have their own startup preference and take precedence when enabled.

A fresh installation makes no preset change until you select one. After upgrading
from a version that did not save presets, select your preferred preset once.
Settings save failures appear in the GUI's service details.

Desktop power managers can also select firmware profiles and overwrite AWCC's
restored choice at login or when power sources change. Configure one owner of
firmware thermal profiles; see [power management integration](packaging/linux/INSTALL.txt).

For development builds, the Actions run has a separately named **`.AppImage`**
artifact and a complete **`awcc-rs-dist-…`** bundle. Use the download links in the
run summary; on older builds the AppImage is inside **`packages-ubuntu-…`**.

`awcc-cli --version` / `awcc-gui --version` report the version, source commit (or `unknown`), dirty state, target, toolchain, and installation format; see [`docs/operation_outcomes_and_build_identity.md`](docs/operation_outcomes_and_build_identity.md) for that metadata and for how command outcomes (`Accepted` vs. `Confirmed` vs. `Failed`) and unavailable/stale state are reported. See [`docs/structured_logging_and_support_export.md`](docs/structured_logging_and_support_export.md) for log-level controls (`--log-level`, `-v`, `AWCC_LOG`), bounded session logs under `$XDG_STATE_HOME/awcc-rs/logs`, and passive redacted support bundles (`awcc-cli diagnostics export`).

See [`docs/ci_packaging_and_release.md`](docs/ci_packaging_and_release.md) for local packaging instructions, checksum verification (`SHA256SUMS`), hardware prerequisites, and maintainer release procedures.

---

## Support, Contributing & Security

- **Support & Diagnostics**: [`SUPPORT.md`](SUPPORT.md) — supported platforms, hardware matrix, passive support export (`awcc-cli diagnostics export`), and startup-failure capture.
- **Contributing, Local Agent & Hosted Issue Investigation**: [`CONTRIBUTING.md`](CONTRIBUTING.md), [`docs/agent_issue_investigation_workflow.md`](docs/agent_issue_investigation_workflow.md), and [`.github/workflows/issue-investigate.yml`](.github/workflows/issue-investigate.yml).
- **Security Policy**: [`SECURITY.md`](SECURITY.md) — private vulnerability reporting instructions.
