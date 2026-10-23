# Bounded Structured Logging and Passive Support-Report Export

This document specifies the shared structured diagnostics layer (`crates/awcc-diagnostics`), bounded in-memory and local file retention, sequence-aware audit synchronization, passive support-report export, and privacy/redaction rules used by `awcc-cli` and `awcc-gui`.

---

## 1. Shared Structured Diagnostics (`crates/awcc-diagnostics`)

Both `awcc-cli` and `awcc-gui` initialize a shared `DiagnosticsRuntime` and register its `tracing_subscriber::Layer` in `main()` (after answering `--help` and `--version` fast-paths). Library crates (`awcc-platform`, `awcc-service`) emit structured `tracing` events that are captured alongside explicit runtime records.

### Log-Level Precedence and Console vs. File Output

Log filtering follows strict, deterministic precedence (`resolve_log_level`):

1. **CLI `--log-level <error|warn|info|debug|trace>`** (highest priority; also accepts `--log-level=<level>`). Passing an unrecognized level fails immediately with a non-zero exit code and an actionable error on `stderr`.
2. **CLI `-v` / `--verbose`**: Sets the active log filter to `DEBUG`. `-v` is never a silent no-op.
3. **`AWCC_LOG` environment variable**: Accepts `error`, `warn`, `info`, `debug`, or `trace` (or a `tracing`-style directive). If `AWCC_LOG` contains an invalid value, the runtime falls back to `INFO` and emits a structured `WARN` event (`code = "invalid_log_env"`) on `stderr` and in the session log.
4. **Default**: `INFO`.

**Stream separation:**
- Command results and machine-readable `--json` output are written exclusively to `stdout`.
- Console diagnostics and errors are written exclusively to `stderr`. Ordinary CLI commands keep default `INFO` events in the ring/file; setting `-v`, `--log-level`, or `AWCC_LOG` also sends matching events to the console. The daemon and provider setup helper send their enabled `INFO`, `WARN`, and `ERROR` events to stderr so systemd captures them in the service journal.
- Console output uses its own bounded background queue. A held stderr lock, full pipe or slow journal cannot block control workers or file/ring logging. Console drops and failures are exported separately. CLI main never holds a console lock while waiting for a worker or daemon. Console shutdown is bounded; file delivery is independent.

### Temporary `DEBUG` Recording

In `awcc-gui` (via the **Start DEBUG Recording** / **Stop DEBUG Recording** button on the **Log** screen) and `DiagnosticsRuntime::start_recording(duration)`:
- Temporarily elevates the active log filter to at least `DEBUG` for a bounded window (five minutes in the GUI) without increasing hardware polling frequency or issuing any extra ACPI/USB commands.
- Outside simulation, the GUI updates daemon recording through authorized `diagnostics.v1` IPC on a background job. Start/stop jobs are serialized, and unavailable/denied service recording is reported separately from local GUI recording. Simulation never contacts the host daemon.
- Automatically restores the prior filter level when stopped manually or when the deadline expires, recording `diagnostic_recording_started` and `diagnostic_recording_stopped` / `diagnostic_recording_expired` events.

### Raw USB Response Opt-In (`--trace-raw-usb` / `AWCC_TRACE_RAW_USB=1`)

- Standard hardware write-audit entries always preserve the decoded human-readable meaning and outgoing ACPI WMAX / USB HID hex payload.
- Verbose 33-byte raw USB HID input report dumps (`raw_usb_response_hex`) are **omitted by default** and require explicit opt-in via `--trace-raw-usb` or `AWCC_TRACE_RAW_USB=1`.
- When not opted in, `raw_usb_response_hex` is `None` in memory and omitted from `hardware-audit.jsonl` and support exports.

### Structured Event Schema and Causality

Every `Event` (`crates/awcc-diagnostics/src/event.rs`) is assigned a monotonic sequence number (`seq`) at emission and carries:

| Field | Description |
| :--- | :--- |
| `session_id` | 12-character hex session identifier unique to the running process |
| `seq` | Monotonic per-session sequence number (`1..`), preserved across snapshots and exports |
| `ts_utc` / `unix_ms` | RFC 3339 millisecond UTC timestamp and Unix epoch milliseconds |
| `severity` | `ERROR`, `WARN`, `INFO`, `DEBUG`, or `TRACE` |
| `component` | Originating subsystem (e.g., `awcc::startup`, `awcc_service`, `awcc_platform::usb_elc`) |
| `category` | `app` (lifecycle/worker), `diagnostic_read` (telemetry/status polls), or `hardware_write` (mutating ACPI/USB commands) |
| `op_id` | Optional operation ticket ID carried end-to-end across the GUI queue (`CommandTicket`), `ThermalServiceHandle`, `ThermalOrchestrator`, transport execution, and rollback |
| `action` / `outcome` | Structured operation name and resulting lifecycle state (`confirmed`, `accepted`, `failed`, `rolled_back`) |
| `code` | Machine-stable event/error code (e.g., `startup`, `service_command_completed`, `gmode_rollback`) |
| `elapsed_ms` | Monotonic execution duration in milliseconds where applicable |
| `repeat_count` | Count of suppressed identical `WARN`/`ERROR` failures folded within the rate-limit window |

- **Telemetry noise suppression & failure rate-limiting**: Routine telemetry refreshes are logged at `DEBUG` (`category = diagnostic_read`), never `INFO`. Repeated identical `WARN`/`ERROR` failures from the same component within the rate-limit window (default 10 seconds) are folded into a single summary event with `repeat_count`.

---

## 2. Bounded Retention and Reliable Event Delivery

### In-Memory Ring Buffers and Sequence Cursors (`SeqRing<T>`)

Both application events (`DiagnosticsRuntime::event_ring`, capacity `512`) and mutating hardware audit records (`ThermalOrchestrator::write_audit_ring`, capacity `256`) use `SeqRing<T>` (`crates/awcc-diagnostics/src/ring.rs`):

- **Constant capacity**: When full, pushing a new item evicts the oldest item in place.
- **Sequence-aware synchronization (`SeqRing::since(cursor)`)**:
  - `ThermalOrchestrator::sync_transport_audits` tracks `synced_acpi_audit_seq` and `synced_usb_audit_seq` (sequence cursors, never `.len()`), ensuring new transport writes continue to synchronize after the underlying 256-entry transport ring fills.
  - `awcc-gui` (`sync_snapshot_to_ui`) tracks `audit-latest-seq`, `audit-retained-count`, `audit-evicted-count`, and `audit-cleared-count` rather than comparing `row_count() != write_audit_log.len()`, so the GUI table continues updating on every new write even when full at 256 rows and displays an explicit eviction-gap notice when older entries have rolled off.
- **Distinct counters (`RingStats`)**:
  - `lifetime_total`: Monotonic count of all records ever emitted in the session (survives capacity eviction and user history clears).
  - `retained`: Number of entries currently held in the ring buffer.
  - `evicted`: Number of older entries dropped due to ring capacity.
  - `cleared`: Number of entries hidden by an explicit **Clear Visible** action.

### Local File Rotation and Budget (`FileSink`)

Session logs are written under `$XDG_STATE_HOME/awcc-rs/logs` (defaulting to `~/.local/state/awcc-rs/logs`, overridable via `AWCC_LOG_DIR`):

- **50 MiB total retained budget** (`MAX_TOTAL_LOG_BYTES = 50 * 1024 * 1024`):
  - Per-segment cap: **5 MiB** (`MAX_BYTES_PER_SEGMENT`), up to **4 segments per session** (`session-<utc>-<pid>-<session_id>.<0..3>.jsonl`, max 20 MiB per session).
  - Latest passive session evidence is cached alongside the logs in `evidence-<session_id>.json`.
  - On startup and rotation, inactive sessions (whose `<pid>` is no longer alive in `/proc/<pid>`) are pruned oldest-first to stay within the 50 MiB budget and 24-session cap.
- **Multi-process safety**: Concurrent `awcc-gui` and `awcc-cli` processes write to separate `session-<utc>-<pid>-<session_id>.*.jsonl` files. Active writers (live `/proc/<pid>`) are never truncated, rotated, or deleted by another process.
- **Non-blocking delivery**: `FileSink` runs on a dedicated background thread fed by a bounded `sync_channel(4096)`. GUI and thermal-control threads use `try_send` and never block on disk I/O. Unwritable directories, disk-full errors, or queue saturation increment `dropped_queue` / `dropped_io` counters without altering hardware control outcomes.
- **Permissions**: The log directory is created with mode `0700` and all `.jsonl` / `.json` files are created with mode `0600`.

---

## 3. Passive Support-Report Export

Support reports can be exported from either the CLI or the GUI:

### CLI Commands

```bash
# Export a redacted support archive (.zip) passively from cached state & logs:
awcc-cli diagnostics export --output awcc-support.zip

# Print the concise human-readable diagnostic summary to stdout (or --json):
awcc-cli diagnostics summary
awcc-cli diagnostics summary --json

# Explicitly record daemon DEBUG events for a bounded window (default 300 seconds):
awcc-cli diagnostics record --seconds 60
awcc-cli diagnostics stop-recording
```

### GUI Actions (`Log` Screen)

- **Start DEBUG Recording / Stop DEBUG Recording**: Toggles temporary five-minute GUI and daemon `DEBUG` capture; service failures are explicit.
- **Copy Summary**: Copies the redacted `summary.txt` report into the application's copy buffer (`last-copied-text`) and displays status feedback.
- **Export Support Report**: Writes `awcc-support-<utc>-<session_id>.zip` to `$XDG_STATE_HOME/awcc-rs/logs/` (or `AWCC_SUPPORT_EXPORT_PATH`).
- **Row Selection & Copy Row**: Clicking any row in the hardware command log highlights it and displays its full decoded + raw hex + acknowledgement payload at the bottom of the card; clicking **Copy Row** copies that full detail line.
- **Clear Visible**: Clears currently visible rows from the table while preserving the lifetime command counter.

### Archive Contents (`awcc-support.zip`)

| Archive Entry | Contents |
| :--- | :--- |
| `summary.txt` | Concise human-readable diagnostic summary (schema version, build identity, host OS/kernel/session, backend & subsystem availability, cached telemetry/thermal/lighting state, and log/audit counters) |
| `report.json` | Versioned structured report (`schema_version: 1`, `export_mode: "passive"`, build identity, redacted host environment, diagnostic counters, and passive hardware/session evidence with observation timestamp & age) |
| `events.jsonl` | Bounded, redacted structured application and diagnostic events (`Event` records) |
| `hardware-audit.jsonl` | Bounded, redacted mutating hardware write-audit records (`HardwareAuditRecord` entries) |
| `build-info.json` | Shared `BuildIdentityReport` (version, commit SHA & source, dirty state, target, profile, `rustc`, package format) |
| `settings.json` | Allowlisted non-sensitive settings (`log_level`, `log_level_source`, `raw_usb_opt_in`, GUI refresh settings) |
| `service-diagnostics.json` | Optional daemon build, cached profile/mapping/CPU/thermal/hold state, recording and sink status, passive integration context, service-journal excerpt and evidence gaps |
| `daemon-events.jsonl` | Optional bounded daemon and previous daemon/setup session events, preserving session IDs, sequence numbers and request correlation |

### PowerProfiles timeline and cross-system evidence

Default events record provider bus-name acquisition, competing owners, handover and recovery,
incoming profile/hold requests, authorization results, CPU and thermal outcomes, and state
publication. D-Bus and socket requests carry the same request ID through the control worker.
Policy generations link state publication with property notifications on both endpoints.
Routine property reads are not logged at INFO. Notification failures are recorded rather than
discarded. Failures retain stable codes such as `polkit_reply_decode_failed`, with expected and
received signatures; policy denial is distinct from transport or reply-decoding failure.
Failure folding includes structured fields so unrelated request IDs are not collapsed together.

Support export fetches daemon evidence through the same kernel-peer authorization as other
IPC requests. It uses published state only: no interest notification, Refresh or hardware read.
Optional `diagnostics.v1` does not change protocol 2 compatibility for normal control operations.
Pages contain at most 16 events and 24 KiB of event data; current-session export keeps at most
256 recent events. Prior daemon/setup history is scanned within fixed bounds and capped at
8 KiB. Oversized fields, evictions, clearing, scan limits, collection errors and restarts during
export are identified explicitly. Clients without daemon access still produce a report.

Passive context uses fixed read-only tool arguments and timeouts: bus ownership/features,
polkit backend version, installed bus implementation versions, desktop version when available,
known service states and KDE current/configured policy when the session exposes it. Installed
bus versions are labeled as installed components; the active service unit is reported separately.
Bus queries disable automatic service activation. CPU driver identity is retained from the
existing CPU policy plan, not queried again during export. A journal excerpt is restricted to
`awccd.service` and the recent 24-hour window. Missing permissions, tools or entries remain
unavailable evidence; export never elevates privileges or captures unrelated bus traffic.

### Strict Passive Invariant

Support-report assembly and export are **100% passive**:
- Never call `query_snapshot()`, construct hardware transports, refresh USB ELC diagnostics, invoke `/proc/acpi/call`, spawn `pkexec`/`sudo`, modify settings, or replay commands.
- In `awcc-gui`, export reads the already-cached `SystemSnapshot` held in memory (`origin = "in_memory_cache"`).
- In `awcc-cli`, export reads existing session logs and cached `evidence-<session_id>.json` from disk (`origin = "persisted_session_file"`), or resolves the static device profile from `database.json` without opening ACPI or USB transports (`origin = "unavailable"` if no prior session evidence exists).
- Authorized cached daemon diagnostics and fixed read-only integration metadata may supplement those files. They never activate a missing provider, request fresh telemetry, change a profile, or start diagnostic recording implicitly.

---

## 4. Privacy, Redaction, and Crash Evidence

- **Allowlist + Redaction (`Redactor`, `crates/awcc-diagnostics/src/redact.rs`)**:
  - Scrubs local usernames (`$USER` / `$LOGNAME`), hostnames (`/proc/sys/kernel/hostname` / `/etc/hostname`), `$HOME`, `/home/<user>`, `/var/home/<user>`, `/root`, `/run/user/<uid>`, `/run/media/<user>`, `/media/<user>`, and `/tmp/.mount_<id>` AppImage mount paths.
  - Scrubs UUIDs, MAC addresses, e-mail addresses, and values assigned to `serial`, `service_tag`, `uuid`, `asset_tag`, `hostname`, `username`, `password`, `token`, or `secret` keys across both free text (`summary.txt`, `.jsonl` messages, error chains, backtraces) and structured JSON (`report.json`, `settings.json`).
  - Deliberately preserves hardware model and profile identifiers needed for debugging (`Alienware m15 R6`, `AMWW`, `0x0A86`, firmware versions, and WMAX/ELC opcodes).
- **Crash Evidence (`install_panic_hook`)**:
  - Installed in both binaries after `--help`/`--version` handling.
  - On panic, records a redacted `ERROR` event (`code = "panic"`) containing the panic location, message, redacted backtrace, and `BuildIdentityReport` to `stderr` and flushes the background `FileSink` on a best-effort bounded timeout before process termination.
