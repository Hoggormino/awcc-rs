# AWCC Linux — Rust + Slint (`@material`) Rewrite (`awcc-rs`)

Unofficial Alienware Command Center for Linux rewritten in **Rust** with a compact desktop **Material 3 GUI** built on **Slint (`@material` 1.1.0)** and backed by comprehensive offline test suites and **Alienware m15 R6 (`AMWW`)** hardware validation contracts.

---

## Workspace Structure

| Crate / Directory | Description |
| :--- | :--- |
| [`crates/awcc-core`](crates/awcc-core) | Pure domain model: 4-tier `database.json` resolution, DMI/ACPI prefix rules, `ThermalMode`, `ThermalModeState` (`Unknown != Manual`), `SensorReading<T>` (`Unavailable != 0`), `OperationLifecycle`, custom 4-point monotonic fan curves, TCC offset, and USB HID ELC (`0x187c:0x0550`/`0x0551`) packet builders. |
| [`crates/awcc-platform`](crates/awcc-platform) | Hardware transport traits (`ThermalTransport`, `StateStorage`, `ElcUsbTransport`) with both live Linux backends (`/proc/acpi/call` WMAX, `hwmon`, `rusb` USB HID) and deterministic `SimBackend` / `MockElcDevice`. |
| [`crates/awcc-service`](crates/awcc-service) | Non-blocking background worker (`ThermalServiceHandle`), privileged Unix-socket daemon (`/tmp/awcc.sock` allowlist executor), and `evdev` hotkey keybinder (`G-Mode` / brightness cycle). |
| [`crates/awcc-gui`](crates/awcc-gui) | Slint (`@material` 1.1.0) desktop GUI (`awcc-gui`) with live hardware auto-detection, interactive simulation scenario controls (`--sim`), and headless PNG screenshot export (`--screenshot`). |
| [`crates/awcc-cli`](crates/awcc-cli) | Command-line interface and daemon entry point (`awcc`) supporting thermal modes, G-Mode, manual fan boost, TCC offset, custom fan curves, RGB lighting effects, and `raw-status` diagnostics. |
| [`conformance/`](conformance) | C++ baseline conformance vectors (`cpp_baseline_vectors.json`) verified by `crates/awcc-platform/tests/conformance_suite.rs`. |
| [`ui-libraries/material-1.1.0`](ui-libraries/material-1.1.0) | Vendored official Slint Material 3 component library (`@material` `1.1.0`, MIT license). |
| [`docs/`](docs) | Durable engineering specifications: C++ source assessment & compatibility map, architecture & migration strategy, and Figma/Material 3 mapping. |

---

## Clone, Build, and Test

```bash
git clone https://github.com/Hoggormino/awcc-rs.git
cd awcc-rs

# Build workspace (debug or release)
cargo build --workspace
cargo build --workspace --release

# Run quality gates (format, Clippy, unit & conformance tests)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

---

## Running the GUI & CLI

### 1. Launch the Desktop GUI (`awcc-gui`)
```bash
# Auto-detect live Alienware hardware (falls back to simulation if unavailable, or pass --sim)
cargo run -p awcc-gui

# Explicitly run in Simulated Hardware Mode (Alienware m15 R6 · \_SB.AMWW.WMAX)
cargo run -p awcc-gui -- --sim
```

### 2. Run CLI Commands (`awcc`)
```bash
cargo run -p awcc-cli -- --help
cargo run -p awcc-cli -- device-info
cargo run -p awcc-cli -- query-mode
```

### 3. Render Deterministic Screenshots (`1120×720` Normal, `880×620` Compact, Rollback, Lighting, Diagnostics)
```bash
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-normal-1120x720.png --width 1120 --height 720 --scenario normal
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-compact-880x620.png --width 880 --height 620 --scenario compact
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-rollback-1120x720.png --width 1120 --height 720 --scenario rollback
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-lighting-1120x720.png --width 1120 --height 720 --scenario lighting
cargo run -p awcc-gui -- --sim --screenshot docs/screenshots/prototype-diagnostics-1120x720.png --width 1120 --height 720 --scenario diagnostics
```
