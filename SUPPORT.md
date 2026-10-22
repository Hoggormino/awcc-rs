# Support & Diagnostics Guide (`awcc-rs`)

`awcc-rs` is an independent, community-maintained Linux application and CLI for Dell G-Series and Alienware systems. Support is provided on a **best-effort, volunteer basis** with no guaranteed response-time SLA.

---

## 1. Supported Platforms & Five Installation Formats

All release artifacts target **Linux `x86_64` (`x86_64-unknown-linux-gnu`)** built against an **Ubuntu 24.04 LTS (`glibc <= 2.39`)** compatibility baseline:

| Format | Artifact Pattern | Primary Target Distributions |
| :--- | :--- | :--- |
| **Debian / Ubuntu (`.deb`)** | `awcc-rs_<version>_amd64.deb` | Ubuntu 24.04 LTS+, Debian 13+, Linux Mint 22+, Pop!_OS 24.04+ |
| **Fedora / RPM (`.rpm`)** | `awcc-rs-<version>-1.x86_64.rpm` | Fedora 41+, RHEL/Alma/Rocky 10+, openSUSE Tumbleweed |
| **Arch / CachyOS (`.pkg.tar.zst`)** | `awcc-rs-<version>-1-x86_64.pkg.tar.zst` | Arch Linux, CachyOS, EndeavourOS, Manjaro |
| **Portable AppImage (`.AppImage`)** | `awcc-rs-<version>-x86_64.AppImage` | Any `x86_64` Linux desktop with `glibc >= 2.39` and FUSE (`libfuse2` / `fuse3`); starts `awcc-gui` by default or `awcc-cli` with `--cli <args>` / `ARGV0=awcc-cli` |
| **Binary Archive (`.tar.gz`)** | `awcc-rs-<version>-x86_64-unknown-linux-gnu.tar.gz` | Manual or custom distribution packaging (`/usr/bin/awcc-cli`, `/usr/bin/awcc-gui`, `/etc/awcc/database.json`, systemd & udev rules) |

### Runtime Hardware Prerequisites (Live Mode)

- **Thermal / Fan / G-Mode / TCC**: Requires the `acpi_call` kernel module (`acpi_call-dkms`, exposing `/proc/acpi/call`) and the privileged `awccd` service (`sudo systemctl enable --now awccd.service`, socket `/run/awcc/awcc.sock`).
- **RGB Lighting / Brightness / Stealth**: Communicates directly with the Alienware USB HID ELC controller (`0x187c:0x0550` or `0x187c:0x0551`) via `libusb`/`rusb`. Install `/etc/udev/rules.d/70-awcc.rules` and reload udev (`sudo udevadm control --reload-rules && sudo udevadm trigger`) for unprivileged access.

---

## 2. Tested & Configured Hardware Matrix

### Hardware-Validated Reference Target & Simulation Contract

- **Alienware m15 R6** (`\_SB.AMWW.WMAX`, SMBIOS `0x0A86`, USB ELC `187c:0550`): Simulation profile covered by deterministic offline tests (`--sim`).

### Device Database Roster (`crates/awcc-core/data/database.json` / `/etc/awcc/database.json`)

The embedded and installed `database.json` includes 54 profile entries across three families:
- **Alienware Laptops**: `Alienware m15`, `m15 R3`, `m15 R4`, `m15 Ryzen Ed. R5`, `m15 R6`, `m15 R7`, `m15 R7 AMD`, `m16 R1`, `m16 R1 AMD`, `m16 R2`, `m17 R2`, `m17 R5 AMD`, `m18 R1`, `m18 R1 AMD`, `m18 R2`, `x14`, `x14 R2`, `x15 R2`, `x16 R2`, `x17 R1`, `x17 R2`, `16 Area-51 AA16250`, `17 R5`, `18 Area-51 AA18250`, `Area-51m`, `Area-51m R2`, `16 Aurora AC16250`, `16 Aurora AC16251`, `16X Aurora AC16251`.
- **Alienware Desktops**: `Alienware Area-51 AAT2250`, `Aurora ACT1250`, `Aurora Ryzen Edition`, `Aurora R7`, `Aurora R9`, `Aurora R11`, `Aurora R12`, `Aurora R13`, `Aurora R15`, `Aurora R16`.
- **Dell G-Series Laptops**: `G3 3590`, `G5 5505`, `G5 5590`, `G7 7500`, `G7 7700`, `Dell G15 5510`, `G15 5511`, `G15 5515`, `G15 5520`, `G15 Special Edition 5521`, `G15 5525`, `G15 5530`, `G15 5535`, `Dell G16 7620`, `G16 7630`.

### Limits of Simulated Validation (`--sim`)

Automated CI and local `--sim` runs validate command encoding, ACPI/USB state transitions, G-Mode rollback, error propagation, and GUI rendering against mocked transports (`SimBackend`, `MockElcDevice`). **Passing offline or `--sim` tests does not prove that a specific laptop's BIOS or USB controller firmware accepts a given opcode.** Model-specific hardware fixes remain marked `needs-hardware-test` until confirmed on the physical machine.

---

## 3. Collecting Diagnostic Evidence for Bug & Hardware Reports

### Option A: Passive Support Bundle (`awcc-support.zip`) — Recommended

When `awcc-cli` or `awcc-gui` can start, export a bounded, redacted support archive. **Export is 100% passive**: it never issues hardware queries, never prompts for root/`pkexec`, and never changes settings.

**From the CLI:**
```bash
awcc-cli diagnostics export --output awcc-support.zip
```
*(If using the AppImage: `./awcc-rs-<version>-x86_64.AppImage --cli diagnostics export --output awcc-support.zip`.)*

**From the GUI (`awcc-gui`):**
1. Open the **Log** tab in the left navigation rail.
2. *(Optional)* Click **Start DEBUG Recording**, reproduce the issue once, and click **Stop DEBUG Recording**.
3. Click **Export Support Report** (writes `awcc-support-<utc>-<session_id>.zip` under `$XDG_STATE_HOME/awcc-rs/logs/`, defaulting to `~/.local/state/awcc-rs/logs/`) or **Copy Summary**.

**What is inside `awcc-support.zip`:**
- `summary.txt` and `report.json` (`schema_version: 1`)
- `build-info.json` (exact version, commit SHA, target, `rustc`, and installation format)
- `events.jsonl` and `hardware-audit.jsonl` (recent structured events and decoded/hex hardware write audit)
- `settings.json` (allowlisted non-sensitive settings)

**Privacy & Redaction:**
Local usernames, hostnames, `$HOME` paths, UUIDs, MAC addresses, and serial/service-tag patterns are scrubbed automatically (`<redacted>` / `<user>` / `<hostname>`). **Never manually type or paste your machine serial number or Dell Service Tag into an issue.**

### Option B: Application Startup / Launch Failures (When Export Cannot Run)

If `awcc-gui` or `awcc-cli` crashes or exits before a support bundle can be exported, run the binary from a terminal with `-v` (verbose `DEBUG` stderr logging) and paste the output into the bug report:

```bash
# Check build identity (runs before any hardware, service worker, or GUI initialization):
awcc-cli --version
awcc-gui --version

# Capture startup diagnostics on stderr:
awcc-gui -v 2>&1 | tee awcc-startup.log
awcc-cli -v device-info 2>&1 | tee -a awcc-startup.log
```

---

## 4. Where to Open an Issue

- **Bug Report**: [`.github/ISSUE_TEMPLATE/01-bug-report.yml`](.github/ISSUE_TEMPLATE/01-bug-report.yml)
- **Hardware Compatibility / New Model Report**: [`.github/ISSUE_TEMPLATE/02-hardware-compatibility.yml`](.github/ISSUE_TEMPLATE/02-hardware-compatibility.yml)
- **Feature Request**: [`.github/ISSUE_TEMPLATE/03-feature-request.yml`](.github/ISSUE_TEMPLATE/03-feature-request.yml)
- **Security Vulnerabilities**: Do **not** open a public issue. Follow [`SECURITY.md`](SECURITY.md).
