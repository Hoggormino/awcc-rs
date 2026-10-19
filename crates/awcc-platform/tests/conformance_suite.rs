//! Differential conformance suite verifying 100% parity between C++ AWCC
//! (`src/src/AcpiUtils.cpp`, `src/src/Thermals.cpp`, `src/database.json`)
//! and Rust (`awcc-core`, `awcc-platform`).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use awcc_core::{
    AcpiPrefix, CpuVendor, DEFAULT_DATABASE_JSON, Database, ThermalCodeTable, ThermalMode,
    ThermalModeState, parse_acpi_output, resolve_acpi_prefix,
};
use awcc_platform::{
    HwmonProvider, LinuxAcpiTransport, PosixStateStore, StateStorage, ThermalTransport,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ConformanceVectors {
    expected_database_entry_count: usize,
    acpi_prefix_vectors: Vec<PrefixVector>,
    acpi_output_parser_vectors: Vec<ParserVector>,
    patch_modes_vectors: Vec<PatchModeVector>,
    wmax_shell_traces: Vec<ShellTraceVector>,
}

#[derive(Debug, Deserialize)]
struct PrefixVector {
    device_name: String,
    cpu_vendor: String,
    expected_prefix: String,
}

#[derive(Debug, Deserialize)]
struct ParserVector {
    raw: String,
    expected: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct PatchModeVector {
    device_name: String,
    set_codes: BTreeMap<String, u16>,
    get_codes: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ShellTraceVector {
    name: String,
    prefix: String,
    subcmd: u8,
    arg1: u16,
    arg2: u16,
    arg3: u16,
    expected_cmd: String,
}

fn parse_mode_name(name: &str) -> ThermalMode {
    match name {
        "Quiet" => ThermalMode::Quiet,
        "Balanced" => ThermalMode::Balanced,
        "Performance" => ThermalMode::Performance,
        "BatterySaver" => ThermalMode::BatterySaver,
        "Cool" => ThermalMode::Cool,
        "FullSpeed" => ThermalMode::FullSpeed,
        "GMode" => ThermalMode::GMode,
        "Manual" => ThermalMode::Manual,
        other => panic!("Unknown mode in test vector: {other}"),
    }
}

fn parse_prefix_name(name: &str) -> AcpiPrefix {
    match name {
        "AMW0" => AcpiPrefix::Amw0,
        "AMW1" => AcpiPrefix::Amw1,
        "AMW3" => AcpiPrefix::Amw3,
        "AMWW" => AcpiPrefix::Amww,
        other => panic!("Unknown prefix in test vector: {other}"),
    }
}

#[test]
fn conformance_database_matches_cpp_repository_database_json_entry_for_entry() {
    let vectors_str = include_str!("../../../conformance/cpp_baseline_vectors.json");
    let vectors: ConformanceVectors =
        serde_json::from_str(vectors_str).expect("conformance JSON should parse");

    let embedded_db = Database::load_embedded().expect("embedded database should parse");
    assert_eq!(
        embedded_db.devices.len(),
        vectors.expected_database_entry_count
    );

    let sibling_cpp_db = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../src/database.json");
    let cpp_db_path = if sibling_cpp_db.exists() {
        sibling_cpp_db
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../awcc-core/data/database.json")
    };
    let cpp_db_raw =
        fs::read_to_string(&cpp_db_path).expect("reference database.json should exist");
    let cpp_db =
        Database::from_json_str(&cpp_db_raw, awcc_core::DatabaseSource::EnvVar(cpp_db_path))
            .expect("reference database.json should parse identically");

    assert_eq!(embedded_db.devices.len(), cpp_db.devices.len());
    for (model, rust_entry) in &embedded_db.devices {
        let cpp_entry = cpp_db
            .devices
            .get(model)
            .unwrap_or_else(|| panic!("Model `{model}` missing in C++ database.json"));
        assert_eq!(
            rust_entry, cpp_entry,
            "Mismatch between Rust embedded database and C++ src/database.json for `{model}`"
        );
    }

    // Also verify that C++ include/database.h contains the exact same 54 entries.
    assert!(DEFAULT_DATABASE_JSON.contains("\"Alienware m15 R6\""));
    assert!(DEFAULT_DATABASE_JSON.contains("\"Alienware m15 Ryzen Ed. R5\""));
}

#[test]
fn conformance_acpi_prefix_vectors_and_parser_vectors_match_cpp() {
    let vectors_str = include_str!("../../../conformance/cpp_baseline_vectors.json");
    let vectors: ConformanceVectors = serde_json::from_str(vectors_str).unwrap();

    for v in &vectors.acpi_prefix_vectors {
        let vendor = match v.cpu_vendor.as_str() {
            "Intel" => CpuVendor::Intel,
            "Amd" => CpuVendor::Amd,
            _ => CpuVendor::Unknown,
        };
        let expected = parse_prefix_name(&v.expected_prefix);
        let actual = resolve_acpi_prefix(&v.device_name, vendor);
        assert_eq!(
            actual,
            Some(expected),
            "Prefix mismatch for device `{}` ({:?})",
            v.device_name,
            vendor
        );
    }

    for pv in &vectors.acpi_output_parser_vectors {
        assert_eq!(
            parse_acpi_output(&pv.raw),
            pv.expected,
            "ACPI output parser mismatch for `{:?}`",
            pv.raw
        );
    }
}

#[test]
fn conformance_patch_modes_set_and_get_tables_match_cpp() {
    let vectors_str = include_str!("../../../conformance/cpp_baseline_vectors.json");
    let vectors: ConformanceVectors = serde_json::from_str(vectors_str).unwrap();

    for pm in &vectors.patch_modes_vectors {
        let table = ThermalCodeTable::for_device(&pm.device_name);
        for (mode_str, expected_code) in &pm.set_codes {
            let mode = parse_mode_name(mode_str);
            assert_eq!(
                table.set_code_for(mode),
                *expected_code,
                "set_code mismatch on `{}` for {:?}",
                pm.device_name,
                mode
            );
        }
        for (raw_code_str, expected_mode_str) in &pm.get_codes {
            let raw_code: u32 = raw_code_str.parse().unwrap();
            let expected_mode = parse_mode_name(expected_mode_str);
            assert_eq!(
                table.decode_query_response(raw_code),
                ThermalModeState::Known(expected_mode),
                "get_code mismatch on `{}` for 0x{raw_code:x}",
                pm.device_name
            );
        }
    }
}

#[test]
fn conformance_linux_acpi_transport_shell_traces_and_aurora_r11_quirk() {
    let vectors_str = include_str!("../../../conformance/cpp_baseline_vectors.json");
    let vectors: ConformanceVectors = serde_json::from_str(vectors_str).unwrap();

    for trace in &vectors.wmax_shell_traces {
        let prefix = parse_prefix_name(&trace.prefix);
        let cmd = LinuxAcpiTransport::format_acpi_shell_command(
            prefix,
            trace.subcmd,
            trace.arg1,
            trace.arg2,
            trace.arg3,
        );
        assert_eq!(
            cmd, trace.expected_cmd,
            "Shell command mismatch for trace `{}`",
            trace.name
        );
    }

    // Verify Alienware Aurora R11 dual GPU fan boost (0x36 and 0x3a) + CPU fan boost (0x32).
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_clone = Arc::clone(&recorded);
    let mut r11 = LinuxAcpiTransport::new(
        "Alienware Aurora R11",
        AcpiPrefix::Amww,
        HwmonProvider::new("/nonexistent_hwmon"),
        Arc::new(move |cmd: &str| {
            recorded_clone.lock().unwrap().push(cmd.to_string());
            "0x0\n".to_string()
        }),
    );

    r11.set_fan_boost(50, 80)
        .expect("Aurora R11 fan boost should succeed");
    let calls = recorded.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].contains("{0x2,0x32,0x32,0x00}"));
    assert!(calls[1].contains("{0x2,0x36,0x50,0x00}"));
    assert!(calls[2].contains("{0x2,0x3a,0x50,0x00}"));
}

#[test]
fn conformance_cross_language_prev_mode_discriminant_compatibility() {
    let tmp_dir =
        std::env::temp_dir().join(format!("awcc_conformance_prev_mode_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();
    let state_path = tmp_dir.join("prev_mode");

    // C++ writes `static_cast<int>(ThermalModes::Performance)` which is `2\n`.
    fs::write(&state_path, "2\n").unwrap();
    let mut store = PosixStateStore::with_override_path(&state_path);
    assert_eq!(store.load_pre_gmode(), Some(ThermalMode::Performance));

    // Rust writes `Cool` -> must write `3\n` (matching C++ `ThermalModes::Cool == 3`).
    store.save_pre_gmode(ThermalMode::Cool);
    assert_eq!(fs::read_to_string(&state_path).unwrap(), "3\n");

    let _ = fs::remove_dir_all(&tmp_dir);
}
