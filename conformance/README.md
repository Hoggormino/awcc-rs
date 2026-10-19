# AWCC C++ ↔ Rust Differential Conformance Harness (`awcc-1ln.2`)

This directory contains the differential conformance vectors and state-machine verification contract comparing the C++ baseline (`src/src/AcpiUtils.cpp`, `src/src/Thermals.cpp`, `src/database.json`, `src/tests/awcc_offline_tests.cpp`) against the Rust implementation (`awcc-core` and `awcc-platform`).

## Scope & Verified Contracts

1. **54-Entry `database.json` Parity**:
   - Every entry in `src/database.json` and the embedded Rust `DEFAULT_DATABASE_JSON` is verified for identical `featureSet`, `thermalModes`, `lightingModes`, and `keyboardZones` parsing (including out-of-range `> 0xFF` zone filtering and default `[0, 1, 2, 3]` vs explicit `[]` zone handling).
   - 4-tier lookup precedence (`AWCC_DATABASE_PATH` → `./database.json` → `/etc/awcc/database.json` → compiled-in `DEFAULT_DATABASE_JSON`).
2. **ACPI Prefix Resolution (`AcpiUtils::getPrefix` ↔ `resolve_acpi_prefix`)**:
   - Exact branch ordering so `"Alienware m15"` (`AMW1`) never shadows `"Alienware m15 R6"` (`AMWW`) or `"Alienware m15 Ryzen Ed. R5"` (`AMW3`), plus CPU vendor fallback (`Intel` → `AMWW`, `AMD` → `AMW3`).
3. **Asymmetric Set/Get Thermal Codes (`Thermals::patchModes` ↔ `ThermalCodeTable::for_device`)**:
   - `Alienware 17 R5` (`0x196..0x198` set vs `0x96..0x98` get), `Alienware Aurora R9` (`0x197..0x198` set vs `0x97..0x98` get), `Dell G15 5515` (`0x97`), and standard USTT codes (`0xA0..0xAB`, `0x00`).
4. **Linux Platform Transports (`LinuxAcpiTransport`, `HwmonProvider`, `PosixStateStore`)**:
   - Exact WMAX shell command formatting, `0xFFFFFFFF` / `< 0` fail-closed error handling, `Alienware Aurora R11` dual GPU fan boost (`0x36` + `0x3a`), `hwmon` (`alienware_wmi` / `dell_ddv`) priority over ACPI `0x14 0x04`/`0x05`, and `O_NOFOLLOW | O_CLOEXEC` state file persistence using the C++ `ThermalModes` integer discriminant (`0..=7`).

## Running the Conformance Suite

```sh
cargo test --manifest-path src-rust/Cargo.toml --test conformance_suite
```
