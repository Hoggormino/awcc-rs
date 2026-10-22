# Operation Outcomes, Observed State & Runtime Build Identity

This document describes how `awcc-rs` reports what a command actually achieved, how it presents
state it could not observe, and how every binary identifies the build it came from.

- Outcome and state contract: `awcc-core`, `awcc-platform`, `awcc-service`, `awcc-cli`, `awcc-gui`
- Build identity: `crates/awcc-build-info`

## 1. Operation outcome contract

Every state-changing `ServiceCommand` (thermal mode, G-Mode, manual/CPU/GPU fan boost, CPU Turbo,
TCC offset, fan curve, lighting apply, brightness, stealth, power-button LEDs) produces exactly one
**operation record** with a fresh `operation_id`. Its `OperationLifecycle` says how much is known:

| State        | Meaning                                                                                  | CLI exit |
|--------------|------------------------------------------------------------------------------------------|----------|
| `Requested`  | queued by a client; nothing executed (GUI only, set when a command is accepted by the queue) | —        |
| `Pending`    | the worker started executing; no transport result yet                                    | —        |
| `Accepted`   | the transport accepted the write; the applied device state is **not** confirmed          | `0` + `note:` on stderr |
| `Confirmed`  | accepted **and** the post-write readback matched the request                             | `0`      |
| `RolledBack` | a multi-step operation failed part-way and the earlier step was undone                   | `1`      |
| `Failed`     | the write was rejected, or the readback contradicts the request                          | `1`      |

Rules that make the states trustworthy:

1. **`Confirmed` needs an independent readback that already exists.** The end-of-operation passive
   snapshot (the one the service always took) doubles as the readback. No extra hardware query,
   retry, or command replay is issued to strengthen a result.
2. **A contradicting readback is a failure.** The firmware accepted the write but reports another
   value ⇒ `Failed` (error text names both values). A readback that is merely *unavailable* gives
   `Accepted` with the reason.
3. **Operations without a readback top out at `Accepted`.** The ELC lighting protocol has no
   readback of colours, effects, or brightness, so every lighting operation is `Accepted` with
   "the ELC protocol has no readback of applied lighting".
4. **Results are tied to the request.** `ThermalOrchestrator::execute()` derives its
   `Result<OperationSuccess, OperationError>` from an operation recorded *by that call*; if the
   operation id did not advance the result is `OperationError::NoOutcome`. A lifecycle left over
   from an earlier operation can never be reported as this command's success. Passive refreshes
   never change `operation_id`, so clients can recognise a reused outcome.
5. **Failure of a compensating step is reported as such.** A G-Mode rollback that itself fails is
   `Failed` ("rollback also failed — the GameShift latch may still be enabled"), never
   `RolledBack`. Stopping a fan curve whose profile restore fails is `Failed`.
6. **No claim without an action.** Stopping a fan curve that is not active *in this process* is
   `Accepted` ("nothing was stopped", no write); a lighting target that resolves to no zones is
   `Failed` ("nothing was sent"); a G-Mode toggle whose current state cannot be read is `Failed`
   with zero writes instead of guessing a direction.

### USB transfer vs. acknowledgement

For USB HID writes the outgoing transfer and the acknowledgement read are recorded separately
(`WriteAuditEntry::succeeded` vs. `WriteAuditEntry::ack`, `AckStatus::{Received, Incomplete,
Failed, NotApplicable}`).

- An acknowledgement is **optional** evidence: a missing/short/failed read never fails a
  completed transfer, and the *content* of a received report is not interpreted as a success code.
- A transaction that stops part-way returns `UsbTransactionError { reports_sent, reports_total }`;
  the message states "N of M reports were already sent; the controller state may be partially
  updated". The remembered lighting state (effect baseline, brightness) only advances once the
  transfers completed.
- A successful transfer with missing acknowledgements is `Accepted` and the caveat reads e.g.
  "0 of 6 reports acknowledged; 6 acknowledgement(s) missing or incomplete (report 1: ack read
  failed: timed out)".
- No acknowledgement is made mandatory, and no transaction is added.

### CLI behaviour

`awcc-cli` sends machine-readable results to **stdout** and everything else to **stderr**:

- success: result line on stdout; an `Accepted` result also prints `note: not device-confirmed: …`
- failure: nothing on stdout, `error: …` on stderr, exit status `1`
- single-value queries (`getturbo`, `cb`, `gb`, `tcc`) fail with the reason instead of printing a
  made-up value; `qm` labels any value that is not current and fails only if the thermal mode
  itself could not be read
- `scb`/`sgb` write both fans together, so they refuse (exit `1`, zero writes) when the other
  fan's current boost cannot be read instead of guessing it
- `fancurve …` in `awcc-cli` applies the first step only and says so: continuous tracking needs a
  long-lived service such as `awcc-gui`
- `-d` (daemon) and `-g` (GUI) no longer print a false "ready" message; they exit `1` with an
  explanation (the daemon is not implemented in the CLI; use `awcc-gui` for the GUI)

## 2. Unavailable and stale state

State is never silently replaced by a plausible value.

- `Observation<T>` (`NotObserved` / `Observed` / `Unavailable { reason }` / `Stale { last_value,
  age_secs, reason }`) is used for the GameShift latch, CPU turbo, and ELC diagnostics.
  `SensorReading::Stale` gained `age_secs`.
- `age_secs` is the age of the last good value **when the snapshot was taken**.
- Stale readings never drive decisions: the fan-curve step skips on stale/unavailable
  temperatures, and write confirmation only accepts fresh readbacks.
- The CPU turbo query returns an error when no control file is readable (it used to default to
  "enabled").
- ELC diagnostics are probed once; a failure is kept as `Unavailable` and only an explicit
  `Refresh` (or `raw-status`) probes again. A failed probe never produces a firmware version.
- `SystemSnapshot::is_simulated` says which backend was *selected*; the GUI shows `LIVE BACKEND`
  / `SIMULATED HW` accordingly. It is not proof that every subsystem is communicating — the
  per-value observations carry that.
- Lighting brightness/effect/stealth are *remembered*, not observed (`LightingStateSource`:
  `AssumedDefault`, `PersistedBrightness`, `Commanded`); the GUI summary says which.
- Brightness-file persistence failures are reported as warnings instead of being discarded.

### GUI presentation

- A queued request shows `REQUESTED`; it is not replaced by an older outcome while the worker has
  not yet answered (`CommandTicket::seq` vs. `SystemSnapshot::command_seq`).
- An enqueue rejection shows `NOT SENT` with the distinct cause (queue full vs. worker stopped).
  Staged edits stay staged and nothing is replayed.
- A stopped worker shows `SERVICE STOPPED` with the panic message when there is one, once, and
  halts the auto-refresh timer.
- Unavailable/stale values are labelled in place (`58°C (stale 9s)`); the turbo switch is disabled
  and explained when its state is not current; the Settings card lists what is not current and why,
  and shows the build identity. Initial property defaults are neutral (`--`), not plausible numbers.
- The audit log shows `Sent (33B) · ACK read`, `… · ACK read failed: …`, never a bare "ACK".

### Service and worker failures

`ThermalServiceHandle::spawn` returns `Result<_, ServiceError>` (thread-start errors are kept).
`send` returns `Result<CommandTicket, SendError>` with `QueueFull { capacity }` vs.
`WorkerDisconnected(reason)`; `worker_failure()` reports `Panicked(message)` /
`ExitedUnexpectedly`. Dropping the handle can no longer deadlock when the queue is full, and a
worker panic during shutdown is reported on stderr. A rejected command is never retried.

## 3. Runtime build identity

`awcc-cli --version` and `awcc-gui --version` (also `-V`) print the same shared identity:

```text
awcc-cli 0.1.0 (2e350363fb18)
version: 0.1.0
commit: 2e350363fb1830a8449edd56bdd491f6c0ba2563
commit-source: git
dirty: false
source-date-epoch: 1792419362
target: x86_64-unknown-linux-gnu
profile: release
rustc: rustc 1.95.0 (59807616e 2026-04-14)
package-format: deb
```

The first line is for humans; the `key: value` lines are stable and parsed by the packaging smoke
tests and the release identity check. `-v` keeps its legacy no-op "verbose" meaning in the CLI.

`--version` and `--help` are answered before any profile resolution, orchestrator, transport,
service worker, Slint window, or privilege prompt, so they work without hardware, a graphical
session, or a writable state directory. (There is no file logger in the Rust applications.) This
is tested by injecting spy backends that count construction, and black-box against the real
binaries in an empty-`PATH`, no-state-directory, missing-database environment, with a control
case proving that environment really breaks profile resolution.

### Metadata precedence (`crates/awcc-build-info/build.rs`)

The identity is resolved before compilation and embedded; hashes of final release artifacts stay
in the external `build-manifest.json` (embedding it would be a dependency cycle).

| Field | Precedence |
|-------|------------|
| commit | 1. `AWCC_GIT_SHA` (validated 40/64-hex; exported by `scripts/release-meta.sh`) → 2. `git rev-parse HEAD` in the workspace work tree (ordinary clone or linked worktree) → 3. substituted `.git_archival.txt` (`git archive` / forge tarballs, no `.git`) → 4. `GITHUB_SHA` → 5. `unknown` |
| commit-source | `env`, `git`, `git-archive`, `github-env`, or `unknown` |
| dirty | `AWCC_GIT_DIRTY` → `git status --porcelain --untracked-files=no` when the commit is the checked-out `HEAD` → `unknown` (archives and unknown revisions are never reported clean) |
| source-date-epoch | `AWCC_SOURCE_DATE_EPOCH` → `SOURCE_DATE_EPOCH` → `git log -1 --format=%ct` → `.git_archival.txt` → `unknown` |
| target / profile | Cargo's `TARGET` / `PROFILE` |
| rustc | first line of `$RUSTC --version` |
| package-format | run time, see below |

- The git repository is only used when the workspace root *is* the top of the work tree, so a
  source archive unpacked inside an unrelated repository does not inherit its revision.
- Dirty means *tracked* files differ from the recorded revision; untracked files are ignored.
  `release-meta.sh` and `build.rs` use the same definition.
- The build script re-runs when `AWCC_*`/`GITHUB_SHA` change, when `HEAD`, the current branch ref,
  `packed-refs`, or the loose-ref directories change, and when any source under `crates/`,
  `ui-libraries/`, `Cargo.toml`, `Cargo.lock`, or `rust-toolchain.toml` changes — so rebuilding
  after `HEAD` moves cannot embed the old commit. The identity lives in its own crate so only the
  two binaries relink when it changes.
- `scripts/stage-release-payload.sh` runs both staged binaries with `--version` and fails if the
  embedded version, target, commit, toolchain, or dirty state disagree with the release metadata
  (a stale cached build). `scripts/generate-release-manifest.sh` re-checks the binaries inside the
  final tarball, requires `awcc-cli` and `awcc-gui` to agree on every field, records the
  *binary-reported* toolchain and dirty state in `build-manifest.json`, and refuses a dirty build
  for a tagged release.

### Package format hint

The compiled payload is shared by all five formats, so the format is never compiled in. Each
package ships a marker `…/share/awcc/package-format` (one of `tarball`, `appimage`, `deb`, `rpm`,
`arch`) next to its binaries; the executable reads `<exe dir>/../share/awcc/package-format`
(max 64 bytes, exact tokens only). A binary run from `target/` or without a valid marker reports
`package-format: unknown`; the format is never guessed from the path or the distribution.

## 4. Verification

- `cargo test --workspace --all-features --locked` covers: failed setters, readback
  confirm/mismatch/unavailable, stale lifecycle reuse, G-Mode rollback failure, unreadable-state
  refusal, unavailable/stale observation with injected clock, ELC unavailable/recovery/stale,
  successful transfer + failed/short acknowledgement, partial transfers, worker start-up failure,
  queue saturation vs. disconnection, worker panic, drop with a full queue, GUI enqueue rejection,
  pending-request presentation, and the CLI exit/stdout/stderr contract.
- `scripts/smoke-test-installed.sh` runs `--version` from the installed/extracted payload outside
  the source checkout in every format, requires both binaries to agree, and checks
  `--expect-version/-commit/-target/-dirty/-package-format`.

## 5. Remaining hardware-dependent uncertainty

- Readback-based confirmation (`WMAX 0x14` mode/boost/TCC queries, `intel_pstate/no_turbo`) was
  tested with simulation and offline fault injection, not live firmware. If a device
  reports a rounded or delayed value, an accepted write will show as `Failed` ("readback reports …")
  even though it applied; the error text names both values.
- The ELC acknowledgement report's *content* is not interpreted (no firmware success codes are
  known), and lighting has no readback, so lighting results cannot exceed `Accepted`.
- The lighting colour preview and zone colours are remembered values seeded with defaults until
  commanded; the GUI labels brightness/effect provenance but the per-zone preview swatches do not
  yet.
