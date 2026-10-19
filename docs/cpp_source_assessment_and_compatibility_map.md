# C++ Source Assessment and Rust Compatibility Map

## 1. Baseline Reference State

- **C++ Repository Path**: `src`
- **Baseline Git Commit**: `5be8e4c1551dda39dbc719625818d28fbce4ad3c` (`upstream/master` / `origin/master` — *"Add Support for G15 5525 (#110)"*)
- **Local Uncommitted Modifications (Included in Baseline)**:
  The working tree in `src` contains validated **Workstream A** corrections across 11 modified files (`CMakeLists.txt`, `include/AcpiUtils.h`, `include/Daemon.h`, `include/EffectController.h`, `include/LightFX.h`, `include/Thermals.h`, `src/AcpiUtils.cpp`, `src/EffectController.cpp`, `src/LightFX.cpp`, `src/Thermals.cpp`, `src/main.cpp`) and 1 untracked test suite (`tests/awcc_offline_tests.cpp`, 12/12 CTest assertions passing).
  These local modifications are part of the authoritative baseline for the Rust rewrite.

---

## 2. Existing Linux Application Assessment

### 2.1 Entry Points, Major Components, Build System, and Dependencies

| Component | Source Files | Responsibility |
| :--- | :--- | :--- |
| **CLI & Process Entry** | `src/main.cpp` | Parses `argv[1]`. Passive commands (`--help`, `-h`, `device-info`) short-circuit before hardware initialization. Active CLI commands, daemon (`-d`), and GUI (`-g`) construct `LightFX` $\rightarrow$ `EffectController` $\rightarrow$ `Daemon` $\rightarrow$ `AcpiUtils` $\rightarrow$ `Thermals` in sequence. |
| **ACPI WMAX & Device DB** | `include/AcpiUtils.h`, `src/AcpiUtils.cpp`, `include/database.h`, `database.json` | Resolves DMI `product_name` against `database.json`, determines ACPI prefix (`AMW0`/`AMW1`/`AMW3`/`AMWW`), formats `\_SB.<PREFIX>.WMAX 0 <method> {b0, b1, b2, b3}` commands, parses `0x...` hex responses from `/proc/acpi/call`, reads CPU/GPU temps and RPM/boost, and controls Intel/AMD CPU turbo boost via sysfs. |
| **Thermal & G-Mode Logic** | `include/Thermals.h`, `src/Thermals.cpp` | Maps `ThermalModes` (`Quiet`, `Balanced`, `Performance`, `BatterySaver`, `Cool`, `FullSpeed`, `GMode`, `Manual`) to WMAX `0x14`/`0x15` USTT codes (`0xa0`–`0xab`) with per-device overrides (`patchModes()`), coordinates two-step G-Mode (`0x25` GameShift + `0x15 0xab`), and persists pre-G-Mode state in `/var/tmp/awcc_pre_gmode_state`. |
| **USB HID Lighting (`LightFX`)** | `include/LightFX.h`, `src/LightFX.cpp` | Direct `libusb-1.0` control transfers (`bmRequestType=0x21`/`0xa1`, `bRequest=0x09`/`0x01`, `wValue=0x202`, `wIndex=0x00`, 33-byte payload) to Alienware ELC (`187c:0550` / `187c:0551`). |
| **Lighting Effects (`EffectController`)** | `include/EffectController.h`, `src/EffectController.cpp` | High-level RGB animations (`Static`, `Spectrum`, `Breathe`, `Rainbow`, `Wave`, `BackAndForth`), brightness control (`0`–`10`, inverted to `0x00`–`0x64` hardware dim scale), `/etc/awcc/brightness` persistence, and `raw-status` diagnostic queries. |
| **Privileged Daemon & IPC (`Daemon`)** | `include/Daemon.h`, `src/Daemon.cpp` | Runs as `awccd.service` (`awcc -d`), listens on `/tmp/awcc.sock` (permissions `0666`), validates incoming command strings against `m_CommandAllowed` regex allowlist, and spawns `KeyBinder`. When invoked from an unprivileged CLI/GUI client (`m_running == false`), forwards pre-built command strings over `/tmp/awcc.sock`. |
| **Evdev KeyBinder (`KeyBinder`)** | `include/KeyBinder.h`, `src/KeyBinder.cpp` | Scans `/dev/input/event*` via `libevdev` for `"Dell WMI hotkeys"` (`KEY_PROG1` = `148` G-key) and AT keyboard (`KEY_BRL_DOT7` = `0x1f7` brightness key), invoking callbacks on key release (`ev.value == 0`). |
| **Dear ImGui GUI (`Gui` / `Render`)** | `src/gui/Gui.cpp`, `src/gui/Render.cpp` | Requires `awccd` running (`/tmp/awcc.sock` present), initializes GLFW + OpenGL3 + Dear ImGui, and renders thermal mode radio buttons, fan boost sliders, lighting effect controls, and CPU turbo checkbox. |

**Build System & Dependencies (`CMakeLists.txt`)**:
- C++23 (`CMAKE_CXX_STANDARD 23`), forced `Release` build type (`CMAKE_BUILD_TYPE Release`) because `AcpiUtils::executeAcpiCommand` short-circuits under `#ifndef NDEBUG`.
- CMake `FetchContent`: `loguru`, `nlohmann_json`, `glfw`, `imgui`, `libusb` (`v1.0.29`), `stb`.
- `ExternalProject_Add` + Meson: `libevdev` (`1.13.6`).
- System packages: `X11`, `OpenGL`, `Threads`, and runtime `acpi_call` kernel module (`/proc/acpi/call`) + `pkexec`.

### 2.2 CLI Commands, Output, Exit Behavior, and Service Integration

| CLI Command | Arguments | Privilege / Transport | Current Exit & Output Behavior |
| :--- | :--- | :--- | :--- |
| `--help` / `-h` | None | None (passive) | Prints usage table and exits `0` without opening USB or ACPI. |
| `device-info` | None | Sysfs DMI read only (passive) | Prints DMI product name, resolved ACPI prefix, CPU vendor, `database.json` match status, feature/thermal/lighting bitmasks, and keyboard zones. Exits `0`. |
| `query-mode` | None | Daemon socket or root `/proc/acpi/call` | Queries WMAX `0x14 0x0b` (plus `0x25 0x02` fallback when needed) and prints mode name or `Unknown (0x..)`. Exits `0` on valid mode, `1` on failure/unknown. |
| `<mode-code>` (`q`, `b`, `p`, `bs`, `c`, `fs`, `g`, `m`) | None | Daemon socket or root `/proc/acpi/call` | Switches thermal mode (`Quiet`, `Balanced`, `Performance`, `BatterySaver`, `Cool`, `FullSpeed`, `GMode`, `Manual`). Exits `0` on confirmed `0x0` ACPI response, `1` on unsupported or failed transition. |
| `gt` | None | Daemon socket or root `/proc/acpi/call` | Toggles G-Mode on/off, restoring previous thermal mode from `/var/tmp/awcc_pre_gmode_state` when leaving G-Mode. |
| `cb` / `gb` | `[0-100]` (optional) | Daemon socket or root `/proc/acpi/call` | Without value: queries current CPU/GPU fan boost (`0x14 0x0c 0x32`/`0x33`). With value: switches to `Manual` mode and writes boost (`0x15 0x02 0x32`/`0x33 <val>`). |
| `temp` / `rpm` | None | Daemon socket or root `/proc/acpi/call` | Queries CPU/GPU temperature (`0x14 0x04`) or fan RPM (`0x14 0x05`). |
| `turbo` | `on \| off` | Daemon socket or root sysfs write | Toggles `/sys/devices/system/cpu/intel_pstate/no_turbo` or `/sys/devices/system/cpu/cpufreq/boost`. |
| `brightness` | `<0-10>` | Direct USB HID (`187c:0550`/`0551`) | Sets hardware keyboard brightness and saves to `/etc/awcc/brightness` (when root). |
| `static` / `spectrum` / `breathe` / `rainbow` / `wave` / `bkf` | `<hex>` / `<duration>` / `[zones...]` | Direct USB HID (`187c:0550`/`0551`) | Applies hardware lighting animation to specified keyboard zones (or all configured zones). |
| `raw-status` | `[--firmware] [--status] [--elc-config] [--animation-count] [--reset-all]` | Direct USB HID (`187c:0550`/`0551`) | Queries read-only USB HID status by default; `--reset-all` explicitly opts into destructive `hardReset()`. |
| `test-modes` | None | Root required (`geteuid() == 0`) | Actively probes `0xa0`–`0xab` thermal mode codes via `0x15 0x01` and prints a `database.json` `thermalModes` bitmask. |
| `test-zones` | None | Interactive TTY (`isatty(STDIN_FILENO)`) | Iterates zones `0`–`31`, coloring each green and prompting `y/n` to build a `keyboardZones` array. |
| `-d` | None | Root (`awccd.service`) | Runs background daemon (`Daemon::init`) serving `/tmp/awcc.sock` and `KeyBinder` listeners. |
| `-g` | None | User session (requires `/tmp/awcc.sock`) | Starts GUI; refuses to launch if `/tmp/awcc.sock` does not exist. |

---

## 3. Status of Previous Review Concerns in the C++ Baseline

Each concern raised in prior audits (`docs/m15_r6_validation_and_audit.md`) was verified directly against the current C++ working tree (`src`):

| # | Review Concern | Status in C++ Working Tree (`src/`) | Rust Rewrite Requirement |
| :--- | :--- | :--- | :--- |
| **1** | **USB device lifetime (`libusb_free_device_list` before `libusb_open`)** | **Fixed in Workstream A** (`src/LightFX.cpp:25-77`): `libusb_open` now runs before `libusb_free_device_list(devs, 1)`, tracks `m_interfaceClaimed` / `m_kernelDriverDetached`, and guards all transfers when `m_handle == nullptr`. | Enforce via RAII wrapper type owning the `rusb` / `libusb` handle and interface claim; make invalid handle states unrepresentable (`Option<UsbController>`). |
| **2** | **USB transfer failure handling & unchecked `m_deviceReceive`** | **Fixed in Workstream A** (`src/LightFX.cpp:128-235`): `m_deviceSend` and `m_deviceReceive` check exact transfer byte count (`33`) and return `bool`; query helpers return `std::optional`. | Return `Result<[u8; 33], HidError>` for all USB control transfers; never ignore short reads/writes. |
| **3** | **Failed thermal queries (`-1` / `0xffffffff`) mistaken for `Manual` (`0x00`)** | **Fixed in Workstream A** (`src/Thermals.cpp:144-169`, `include/Thermals.h:26`): Added `ThermalModes::Unknown`. When `AcpiUtils::queryThermalMode()` fails (`-1`) or returns an unmapped byte, `Thermals::queryThermalMode()` returns `ThermalModes::Unknown` instead of falling through to `Manual`. | Represent thermal mode query result as an explicit algebraic type (`ThermalModeState::Known(ThermalMode)` vs `ThermalModeState::UnknownRaw(u32)` vs `Err(AcpiError)`). `Unknown` must never alias `Manual`. |
| **4** | **ACPI success-response validation (`0x15` returning `0xffffffff` treated as success)** | **Fixed in Workstream A** (`src/AcpiUtils.cpp:475-491`, `src/Thermals.cpp:69-142`): `setThermalMode` checks `res == 0` (`0x0`), and `Thermals::setThermalMode` returns `bool` (`false` when ACPI rejects or fails). | Validate exact WMAX return code (`0x0` = success, `0xffffffff` = hardware rejection) and surface typed `ThermalError::HardwareRejected` / `OperationLifecycle::Failed`. |
| **5** | **G-Mode partial transition rollback & cross-process state storage** | **Fixed in Workstream A** (`src/Thermals.cpp:80-139, 179-219`): Entering G-Mode saves prior mode to `/var/tmp/awcc_pre_gmode_state`; if step 2 (`0x15 0x01 0xab`) fails after step 1 (`0x25 0x01 0x01`), it rolls back `0x25 0x01 0x00`. Leaving G-Mode reads `/var/tmp/awcc_pre_gmode_state` across CLI invocations. | Preserve two-step G-Mode state machine with deterministic rollback (`RolledBack` lifecycle state) and atomic state file persistence (`StateStorage` trait). |
| **6** | **Database configuration precedence (`AWCC_DATABASE_PATH` vs `./database.json` vs `/etc/awcc/database.json` vs embedded)** | **Fixed in Workstream A** (`src/AcpiUtils.cpp:63-112`): Implements 4-tier lookup precedence with fallback to compiled-in `DEFAULT_DATABASE_JSON`. | Preserve exact 4-tier database resolution precedence in `awcc-core::database` with diagnostic provenance reporting (`DatabaseSource`). |
| **7** | **Diagnostic side effects (`raw-status` calling `hardReset()`, `device-info` probing hardware)** | **Fixed in Workstream A** (`src/EffectController.cpp:365-391`, `src/main.cpp:25-100`): `raw-status` is read-only unless `--reset-all` is passed; `device-info` and `--help` run passively without opening USB or ACPI. | Enforce **Zero Unsolicited Hardware Writes** across all passive CLI commands, startup paths, and GUI refreshes. |
| **8** | **GUI startup & refresh side effects (`Render.cpp` resetting sliders to `0`, blocking UI thread on ACPI calls)** | **Partially mitigated in C++ (`src/gui/Render.cpp:74-105`), unresolved architecturally**: C++ `Render.cpp` still executes synchronous IPC/ACPI queries on the render thread when `Refresh` or radio buttons are clicked, and initializes fan boost sliders to `0` until `Refresh` is clicked. | Fix in Rust (`awcc-service` + `awcc-gui`): all hardware/mock operations run on a non-blocking worker channel; startup passively reads current mode & telemetry without writing; manual fan boost sliders use explicit `Apply` / `Reset` with dirty state tracking. |
| **9** | **Daemon security: regex string execution (`popen`) over `0666` `/tmp/awcc.sock`** | **Unresolved in C++ (`src/Daemon.cpp:17-25, 118-137`)**: Daemon still accepts raw shell command strings matching `m_CommandAllowed` and runs them via `popen`. | Replace raw shell-command IPC with structured, typed RPC requests in Phase 4 (`awcc-1ln.4`), while keeping CLI compatibility. |

---

## 4. Compatibility Map: C++ Baseline $\rightarrow$ Rust Destination

| Capability / Behavior | C++ Location | Classification | Rust Destination Crate & Module | Migration Notes |
| :--- | :--- | :--- | :--- | :--- |
| **Device Database & 4-Tier Resolution** | `AcpiUtils::m_resolveDevicefromDatabase`, `database.json`, `include/database.h` | **Preserve** | `awcc-core::database` | Embed `database.json` via `include_str!`; support `AWCC_DATABASE_PATH` $\rightarrow$ `./database.json` $\rightarrow$ `/etc/awcc/database.json` $\rightarrow$ embedded default. |
| **DMI Device Matching & ACPI Prefix Selection** | `AcpiUtils::getPrefix`, `Helper::getDeviceName` | **Preserve** | `awcc-core::device` | Preserve exact substring precedence (`Alienware m15 R6` $\rightarrow$ `AMWW`, etc.) and CPU vendor fallback (`GenuineIntel` $\rightarrow$ `AMWW`, `AuthenticAMD` $\rightarrow$ `AMW3`). |
| **Per-Model Thermal Code Overrides** | `Thermals::patchModes` | **Preserve** | `awcc-core::thermal` | Preserve per-device overrides (e.g., `Dell G15 5510`, `Alienware m15 R5`, `Alienware Area-51m`) alongside standard `0xa0`–`0xab` USTT codes. |
| **Thermal Mode Query (`0x14 0x0b` + `0x25 0x02`)** | `AcpiUtils::queryThermalMode`, `Thermals::queryThermalMode` | **Preserve (with Workstream A fix)** | `awcc-core::thermal`, `awcc-platform::acpi` | Never conflate failed/unmapped queries (`Unknown`) with `Manual` (`0x00`). |
| **Thermal Mode Set (`0x15 0x01`) & G-Mode Toggle (`0x25 0x01` + `0x15 0x01 0xab`)** | `Thermals::setThermalMode`, `Thermals::toggleGMode` | **Preserve (with Workstream A rollback)** | `awcc-core::thermal`, `awcc-service::controller` | Preserve two-step G-Mode sequence, rollback of `0x25` if `0x15` fails, and `/var/tmp/awcc_pre_gmode_state` persistence. |
| **Telemetry Queries (CPU/GPU Temp `0x14 0x04`, RPM `0x14 0x05`, Boost `0x14 0x0c`)** | `AcpiUtils::getTemp`, `getFanRpm`, `getFanBoost` | **Correct Defect** | `awcc-core::telemetry`, `awcc-platform::acpi` | C++ returns `0` or `-1` integers that callers can confuse with `0°C` / `0 RPM`. Rust uses `SensorReading<T>` (`Valid`, `Unavailable`, `Stale`). |
| **Manual Fan Boost (`0x15 0x02 0x32`/`0x33`)** | `Thermals::setCpuBoost`, `Thermals::setGpuBoost` | **Deliberate UX Improvement** | `awcc-service::controller`, `awcc-gui` | In GUI, moving a boost slider updates staged UI state only; hardware writes happen only when the user clicks `Apply` (and automatically ensures `Manual` mode first). |
| **GUI Toolkit & Layout (`ImGui` $\rightarrow$ `Slint @material`)** | `src/gui/Gui.cpp`, `src/gui/Render.cpp` | **Deliberate Replacement** | `awcc-gui` (`ui/main.slint`) | Replace fixed-pixel Dear ImGui window with responsive Slint Material 3 (`@material`) desktop interface designed in Figma, backed by asynchronous `awcc-service` worker channels. |
| **USB HID Lighting (`187c:0550`/`0551`)** | `src/LightFX.cpp`, `src/EffectController.cpp` | **Preserve (Phase 3)** | `awcc-platform::usb_hid`, `awcc-core::lighting` | Tracked in `awcc-1ln.3` & `awcc-h4u`. |
| **Daemon IPC (`/tmp/awcc.sock`) & Evdev Hotkeys** | `src/Daemon.cpp`, `src/KeyBinder.cpp` | **Correct Defect (Phase 4)** | `awcc-service::daemon`, `awcc-platform::evdev` | Tracked in `awcc-1ln.4`; replace regex `popen` shell string execution with structured IPC commands. |

---

## 5. Representative First Feature Selection

We select **Thermal Profile Management, G-Mode Transition with Rollback, Live CPU/GPU Telemetry, and Staged Manual Fan Boost** on **Alienware m15 R6** as the representative first vertical slice (`Phase 1` prototype + `FEAT-THERM-001` / `FEAT-THERM-002`):

1. **Directly validated on Alienware m15 R6**: Hardware protocol traces for `WMAX` (`0x14`, `0x15`, `0x25`) on `Alienware m15 R6` (`AMWW`) are verified end-to-end.
2. **Exercises every architectural boundary**:
   - Domain model & database resolution (`awcc-core`)
   - Transport trait & deterministic simulation (`awcc-platform`)
   - Non-blocking worker orchestration, lifecycle states (`Idle` $\rightarrow$ `Requested` $\rightarrow$ `Pending` $\rightarrow$ `Confirmed` / `RolledBack` / `Failed`), and zero-unsolicited-write guarantee (`awcc-service`)
   - Responsive Slint `@material` desktop UI with normal (`1120×720`) and compact (`880×620`) layouts (`awcc-gui`)
