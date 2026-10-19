# Contributing to `awcc-rs`

Thank you for contributing to `awcc-rs`! Please read this guide before opening an issue or pull request.

---

## 1. Reporting Issues & Hardware Compatibility

- Check [`SUPPORT.md`](SUPPORT.md) first for supported distributions, the five package formats, and how to collect a passive support bundle (`awcc-cli diagnostics export --output awcc-support.zip`).
- Use the structured GitHub Issue Forms in [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE/):
  - **Bug Report** (`01-bug-report.yml`)
  - **Hardware Compatibility / New Model Report** (`02-hardware-compatibility.yml`)
  - **Feature Request** (`03-feature-request.yml`)
- **Security issues**: Follow [`SECURITY.md`](SECURITY.md) — never post exploit details in a public issue.
- **Privacy**: Never include machine serial numbers or Dell Service Tags.

---

## 2. Development Setup & Quality Gates

The workspace pins Rust **`1.95.0`** in [`rust-toolchain.toml`](rust-toolchain.toml) (workspace MSRV `1.92`) and enforces locked dependencies (`--locked`).

### Required Local Quality Gates

Run all of the following from the repository root before submitting a PR:

```bash
# 1. Format check
cargo fmt --all -- --check

# 2. Strict Clippy lint check (all targets & features, zero warnings)
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# 3. Unit, integration, and C++ differential conformance suites
cargo test --workspace --all-features --locked

# 4. Isolated CLI & headless GUI screenshot smoke test
./scripts/smoke-test-installed.sh \
  --cli target/debug/awcc-cli \
  --gui target/debug/awcc-gui

# 5. Issue form, label catalog, and intake automation fixture tests
node scripts/test-issue-intake.mjs
```

If you touch shell scripts (`scripts/*.sh`, `packaging/linux/AppRun`) or GitHub Actions workflows (`.github/workflows/*.yml`), also run **ShellCheck (`0.11.0`)** and **actionlint (`1.7.12`)** (enforced automatically in CI).

---

## 3. Core Engineering & Architectural Invariants

1. **Zero Unsolicited Hardware Writes**:
   - Passive CLI commands (`--help`, `--version`, `device-info`, `diagnostics summary`, `diagnostics export`), GUI startup, tab switches, telemetry refreshes, and uncommitted slider movements must **never** issue ACPI writes (`WMAX 0x15`, `WMAX 0x25 0x01`), sysfs writes, or USB HID transfers.
2. **Honest Outcomes & Explicit Uncertainty**:
   - Preserve distinctions between `Requested`, `Accepted` (transfer sent without device readback confirmation), `Confirmed` (readback verified), `Failed`, and `RolledBack`.
   - Never silently map a failed query to `false`, `0°C`, `0 RPM`, or `ThermalMode::Manual`. Use `Observation::{NotObserved, Observed, Unavailable, Stale}` and `ThermalModeState::Unknown`.
3. **Device Support Changes Must Check All Three Quirk Layers**:
   When adding or fixing support for an Alienware / Dell G-Series model (keyed off `/sys/devices/virtual/dmi/id/product_name`), inspect all three locations in [`crates/awcc-core`](crates/awcc-core):
   - ACPI prefix resolution (`AMW0` / `AMW1` / `AMW3` / `AMWW`, where specific model substrings must precede broader prefixes like `"Alienware m15"`).
   - Per-model thermal mode set/get code overrides (`patch_modes`).
   - [`crates/awcc-core/data/database.json`](crates/awcc-core/data/database.json) (`featureSet`, `thermalModes`, `lightingModes`, `keyboardZones`).
4. **No Production Panics**:
   - Do not use `.unwrap()` or `.expect()` in production library/binary code; propagate errors with `thiserror` / `anyhow`.

---

## 4. Offline Simulation vs. Live Hardware Validation

- All automated tests run offline against deterministic mocks (`SimBackend`, `SimFaults`, `MockElcDevice`, `SimUsbElcTransport`).
- **Passing offline CI does not prove that a specific physical machine's ACPI firmware or USB ELC controller accepts a change.**
- Any PR that modifies model-specific opcodes, `database.json` profiles, or transport behavior must explicitly state in the PR description whether it has been tested on the affected physical hardware or requires `needs-hardware-test` verification.

---

## 5. Issue, Beads & Pull Request Workflow

- **GitHub Issues** host the public reporter conversation; **Beads (`bd`)** tracks implementation tasks for maintainers/agents (`Hoggormino/awcc-rs#<ISSUE_NUMBER>` cross-referenced in both the PR description and the Bead).
- For coding agents (Codex, Antigravity, Claude Code), follow the repository architecture, validation commands, and repeatable investigation runbook in [`docs/agent_issue_investigation_workflow.md`](docs/agent_issue_investigation_workflow.md).
- Fill out every section of [`.github/pull_request_template.md`](.github/pull_request_template.md): **Symptom**, **Root Cause**, **Resulting Behavior**, **Tests Actually Run**, and **Remaining Hardware Validation**.
