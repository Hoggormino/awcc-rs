## Related Issue & Tracking

- **GitHub Issue**: Fixes `Hoggormino/awcc-rs#<ISSUE_NUMBER>` (or `Refs Hoggormino/awcc-rs#<ISSUE_NUMBER>`)
- **Beads Task ID** *(maintainers/agents, if applicable)*: `awcc-...`

## 1. Symptom

<!-- Describe the user-visible symptom, CLI/GUI behavior, or missing model profile reported in the issue. -->

## 2. Root Cause

<!-- Explain the confirmed cause in the codebase (citing exact crate/file/function), or state clearly if part of the diagnosis remains a hypothesis pending live hardware verification. -->

## 3. Resulting Behavior

<!-- Describe what awcc-cli / awcc-gui / awcc-service now does, including error handling and outcome reporting (Confirmed / Accepted / Failed / RolledBack). -->

## 4. Tests Actually Run

<!-- Check only the commands you actually executed and list any new regression tests added. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --workspace --all-features --locked`
- [ ] `./scripts/smoke-test-installed.sh --cli target/debug/awcc-cli --gui target/debug/awcc-gui`
- [ ] `node scripts/test-issue-intake.mjs`
- **New/updated regression tests**:
  - `...`

## 5. Remaining Hardware Validation

<!-- Passing offline/simulated CI does NOT confirm model-specific ACPI WMAX or USB ELC behavior on physical hardware. State clearly what was tested in simulation vs. what still requires verification on the affected Alienware / Dell G-Series model (`needs-hardware-test`). -->

- **Tested on physical hardware?**: `No (simulated/offline only)` / `Yes (<exact model name>, BIOS <version>)`
- **Follow-up hardware checks required**:
  - `...`
