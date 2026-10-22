//! AWCC Linux — Slint Material 3 (`@material`) desktop application & headless verification runner.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use awcc_core::{
    FanCurveConfig, FanCurvePoint, Observation, OperationLifecycle, SimScenario, ThermalMode,
    ThermalModeState,
    lighting::{LightingEffect, PowerStateSlot, ZoneTarget},
};
use awcc_diagnostics::{
    DiagnosticsConfig, DiagnosticsRuntime, export::BuildIdentityReport, global_diagnostics,
    init_global_diagnostics, install_panic_hook, redact::Redactor, resolve_log_level,
    resolve_raw_usb_opt_in,
};
use awcc_platform::WriteAuditEntry;
use awcc_platform::usb_elc::{ElcDiagnosticsSummary, LightingStateSource};
use awcc_service::{
    SendError, ServiceCommand, SystemSnapshot, ThermalOrchestrator, ThermalServiceHandle,
    WorkerFailure,
    installer::{
        self, ApplyOutcome, ApplyReport, InstallPaths, Ownership, Progress, ServiceAction,
        ServiceStatusReport, SystemRunner, UnitState,
    },
    ipc::{DaemonClient, ServiceProbe},
    resolve_host_device_profile, resolve_simulated_m15_r6_profile,
    wire::ControlState,
};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::{
    Color, ComponentHandle, Model, ModelRc, PhysicalSize, SharedString, Timer, TimerMode, VecModel,
};

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::todo)]
mod slint_generated {
    slint::include_modules!();
}
use slint_generated::{AuditRow, MainWindow};

/// CLI options for `awcc-gui`.
#[derive(Debug, Clone, Default)]
struct CliOptions {
    show_help: bool,
    show_version: bool,
    sim_mode: bool,
    device_info: bool,
    query_mode: bool,
    screenshot_path: Option<PathBuf>,
    width: u32,
    height: u32,
    scenario: String,
    log_level: Option<String>,
    verbose: bool,
    trace_raw_usb: bool,
}

fn parse_cli_args<I>(args: I) -> Result<CliOptions>
where
    I: IntoIterator<Item = String>,
{
    let mut opts = CliOptions {
        width: 1120,
        height: 720,
        scenario: "normal".to_string(),
        ..CliOptions::default()
    };

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" | "help" => opts.show_help = true,
            "-V" | "--version" => opts.show_version = true,
            "--sim" | "--test-mode" => opts.sim_mode = true,
            "-v" | "--verbose" => opts.verbose = true,
            "--trace-raw-usb" => opts.trace_raw_usb = true,
            "--log-level" => {
                let Some(val) = iter.next() else {
                    bail!("--log-level requires a value (error, warn, info, debug, trace)");
                };
                resolve_log_level(Some(&val), false, None).map_err(|e| anyhow::anyhow!("{e}"))?;
                opts.log_level = Some(val);
            }
            s if s.starts_with("--log-level=") => {
                let val = s["--log-level=".len()..].to_string();
                resolve_log_level(Some(&val), false, None).map_err(|e| anyhow::anyhow!("{e}"))?;
                opts.log_level = Some(val);
            }
            "device-info" | "--device-info" => opts.device_info = true,
            "query-mode" | "--query-mode" => opts.query_mode = true,
            "--screenshot" => {
                let Some(path) = iter.next() else {
                    bail!("--screenshot requires an output file path");
                };
                opts.screenshot_path = Some(PathBuf::from(path));
            }
            "--width" => {
                let Some(val) = iter.next() else {
                    bail!("--width requires a pixel width");
                };
                opts.width = val.parse::<u32>().context("invalid --width value")?;
            }
            "--height" => {
                let Some(val) = iter.next() else {
                    bail!("--height requires a pixel height");
                };
                opts.height = val.parse::<u32>().context("invalid --height value")?;
            }
            "--scenario" => {
                let Some(val) = iter.next() else {
                    bail!(
                        "--scenario requires a scenario name (normal|compact|rollback|unknown|lighting|diagnostics|settings)"
                    );
                };
                opts.scenario = val;
            }
            other => bail!("unknown argument `{other}` (run with `--help` for usage)"),
        }
    }

    Ok(opts)
}

fn help_text() -> &'static str {
    "\
AWCC Linux (Rust + Slint @material 1.1.0 Desktop Application)

USAGE:
  awcc-gui [OPTIONS | COMMAND]

PASSIVE COMMANDS (0 hardware writes):
  -h, --help                 Print this help message and exit
  -V, --version              Print version, source revision, target, toolchain, and package format
  device-info, --device-info Print resolved DMI profile, ACPI prefix, and database bitmasks
  query-mode, --query-mode   Query current thermal mode and exit

OPTIONS:
  --sim                      Run against the in-memory Alienware m15 R6 simulator (with fault-injection bar)
  -v, --verbose              Set console/session log filter to DEBUG (overridden by --log-level)
  --log-level <LEVEL>        Set log filter explicitly: error | warn | info | debug | trace
                             Precedence: --log-level > -v/--verbose > AWCC_LOG env > info (default)
  --trace-raw-usb            Opt in to recording raw 33-byte USB HID response frames in audit logs
  --screenshot <PATH>        Render a deterministic PNG screenshot using the Slint software renderer
  --width <PX>               Window width for screenshot rendering (default: 1120)
  --height <PX>              Window height for screenshot rendering (default: 720)
  --scenario <NAME>          Preset state for screenshot: normal | compact | rollback | unknown | lighting | diagnostics | settings"
}

fn run_device_info(sim_mode: bool) -> Result<()> {
    let profile = if sim_mode {
        resolve_simulated_m15_r6_profile()?
    } else {
        resolve_host_device_profile()?
    };

    if sim_mode {
        println!(
            "Device Name      : {} (simulated target)",
            profile.device_name
        );
    } else {
        println!("Device Name      : {}", profile.device_name);
    }
    println!("SMBIOS System ID : {}", profile.smbios_system_id);
    println!("CPU Vendor       : {}", profile.cpu_vendor);
    println!(
        "ACPI Prefix      : {} ({})",
        profile.acpi_prefix,
        profile.acpi_prefix.wmax_path()
    );
    println!("Database Source  : {}", profile.database_source);
    println!("Database Matched : {}", profile.matched_in_database);
    println!(
        "featureSet       : 0x{:02x} ({})",
        profile.entry.feature_set, profile.entry.feature_set
    );
    println!(
        "thermalModes     : 0x{:02x} ({})",
        profile.entry.thermal_modes, profile.entry.thermal_modes
    );
    println!(
        "lightingModes    : 0x{:02x} ({})",
        profile.entry.lighting_modes, profile.entry.lighting_modes
    );
    println!("keyboardZones    : {:?}", profile.entry.keyboard_zones);
    Ok(())
}

fn run_query_mode(sim_mode: bool) -> Result<()> {
    let mut orch = if sim_mode {
        ThermalOrchestrator::new_simulated_m15_r6()?
    } else {
        ThermalOrchestrator::new_hardware()?
    };
    let snap = orch.query_snapshot();
    println!(
        "Current Mode: {} (Simulated: {}, Writes Executed: {})",
        snap.thermal_mode_state.display_label(),
        snap.is_simulated,
        snap.write_count()
    );
    Ok(())
}

fn parse_mode_key(key: &str) -> Option<ThermalMode> {
    match key {
        "Quiet" => Some(ThermalMode::Quiet),
        "Cool" => Some(ThermalMode::Cool),
        "Balanced" => Some(ThermalMode::Balanced),
        "Performance" => Some(ThermalMode::Performance),
        "FullSpeed" => Some(ThermalMode::FullSpeed),
        "Manual" => Some(ThermalMode::Manual),
        "GMode" => Some(ThermalMode::GMode),
        _ => None,
    }
}

fn mode_key_for(mode: ThermalMode) -> &'static str {
    match mode {
        ThermalMode::Quiet => "Quiet",
        ThermalMode::Cool => "Cool",
        ThermalMode::Balanced => "Balanced",
        ThermalMode::Performance => "Performance",
        ThermalMode::FullSpeed => "FullSpeed",
        ThermalMode::BatterySaver => "BatterySaver",
        ThermalMode::GMode => "GMode",
        ThermalMode::Manual => "Manual",
    }
}

const fn color_preset_rgb(index: i32) -> u32 {
    match index {
        1 => 0xA855F7, // Nebula Purple
        2 => 0x10B981, // Plasma Green
        3 => 0xF59E0B, // Solar Amber
        4 => 0xEF4444, // Crimson Red
        5 => 0xF8FAFC, // Pure White
        _ => 0x00F0FF, // Alien Cyan
    }
}

fn u32_to_slint_color(rgb: u32) -> Color {
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;
    Color::from_rgb_u8(r, g, b)
}

fn parse_hex_or_dec_u16(token: &str) -> Option<u16> {
    let t = token.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u16>().ok()
    }
}

fn decode_wmax_call(
    raw_cmd: &str,
    code_table: Option<&awcc_core::ThermalCodeTable>,
) -> Option<String> {
    let wmax_idx = raw_cmd.find("WMAX")?;
    let after_wmax = &raw_cmd[wmax_idx + 4..];
    let brace_start = after_wmax.find('{')?;
    let brace_end = after_wmax[brace_start..].find('}')? + brace_start;
    let header_tokens: Vec<&str> = after_wmax[..brace_start].split_whitespace().collect();
    let subcmd_str = header_tokens.last().copied()?;
    let subcmd = parse_hex_or_dec_u16(subcmd_str)?;

    let args: Vec<u16> = after_wmax[brace_start + 1..brace_end]
        .split(',')
        .filter_map(parse_hex_or_dec_u16)
        .collect();
    let arg1 = args.first().copied().unwrap_or(0);
    let arg2 = args.get(1).copied().unwrap_or(0);
    let arg3 = args.get(2).copied().unwrap_or(0);
    let arg4 = args.get(3).copied().unwrap_or(0);

    let meaning = match (subcmd, arg1) {
        (0x15, 0x01) => {
            let resolved_mode =
                code_table.and_then(|table| match table.decode_query_response(u32::from(arg2)) {
                    ThermalModeState::Known(m) => Some(m.display_name()),
                    ThermalModeState::Unknown { .. } => ThermalMode::ALL
                        .iter()
                        .copied()
                        .find(|&m| table.set_code_for(m) == arg2)
                        .map(ThermalMode::display_name),
                });
            let mode_name = resolved_mode.unwrap_or(match arg2 {
                0x00 => "Manual",
                0xA0 => "Balanced",
                0xA1 => "Quiet",
                0xA2 => "Cool",
                0xA3 => "Performance",
                0xA4 => "Full Speed",
                0xA5 => "Battery Saver",
                0xAB | 0x35 => "G-Mode",
                _ => "Custom Profile",
            });
            format!("Set Thermal Profile → {mode_name}")
        }
        (0x15, 0x02) => {
            let fan_name = match arg2 {
                0x32 => "CPU Fan",
                0x33 => "GPU Fan",
                0x36 => "Top Fan",
                0x3A => "Front Fan",
                _ => "Fan",
            };
            format!("Set {fan_name} Boost → {arg3}%")
        }
        (0x15, 0x03) => {
            let cap = 100_u16.saturating_sub(arg2);
            format!("Set CPU Temp Limit → {cap}°C (TCC Offset -{arg2}°C)")
        }
        (0x25, 0x01) => {
            if arg2 != 0 {
                "Engage G-Mode GameShift Latch".to_string()
            } else {
                "Toggle / Clear G-Mode GameShift Latch".to_string()
            }
        }
        _ => "ACPI WMAX Control Call".to_string(),
    };

    Some(format!(
        "{meaning}  ·  WMAX 0x{subcmd:02X} [0x{arg1:02X}, 0x{arg2:02X}, 0x{arg3:02X}, 0x{arg4:02X}]"
    ))
}

fn decode_usb_elc_call(raw_cmd: &str) -> Option<String> {
    if !raw_cmd.contains("USB HID") {
        return None;
    }
    let bracket_start = raw_cmd.find('[')?;
    let bracket_end = raw_cmd[bracket_start..].find(']')? + bracket_start;
    let hex_inner = &raw_cmd[bracket_start + 1..bracket_end];
    let bytes: Vec<u8> = hex_inner
        .split_whitespace()
        .filter_map(|b| u8::from_str_radix(b, 16).ok())
        .collect();
    if bytes.len() < 2 {
        return None;
    }
    let hex_upper = bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ");

    let b2 = bytes.get(2).copied().unwrap_or(0);
    let b3 = bytes.get(3).copied().unwrap_or(0);
    let b4 = bytes.get(4).copied().unwrap_or(0);
    let b5 = bytes.get(5).copied().unwrap_or(0);

    let meaning = match bytes[1] {
        0x21 => {
            let op = u16::from_be_bytes([b2, b3]);
            let slot = u16::from_be_bytes([b4, b5]);
            let slot_name = match slot {
                0x0000 => "Volatile Session (Slot 0)".to_string(),
                0x0061 => "Boot Default (0x0061)".to_string(),
                0x005B => "Power AC Sleep (0x005B)".to_string(),
                0x005C => "Power AC On (0x005C)".to_string(),
                0x005D => "Power Charging (0x005D)".to_string(),
                0x005E => "Power DC Sleep (0x005E)".to_string(),
                0x005F => "Power DC On (0x005F)".to_string(),
                0x0060 => "Power Critical (0x0060)".to_string(),
                _ => format!("Slot 0x{slot:04X}"),
            };
            let op_name = match op {
                0x0001 => "Start Animation",
                0x0002 => "Save Animation",
                0x0003 => "Play Animation",
                0x0005 => "Erase Animation Slot",
                0x0006 => "Set Boot Default Slot",
                _ => "Animation Op",
            };
            format!("{op_name} → {slot_name}")
        }
        0x22 => {
            let op = u16::from_be_bytes([b2, b3]);
            let op_name = match op {
                0x0001 => "Start Session Animation",
                0x0002 => "Save Session Animation",
                0x0003 => "Play Session Animation",
                0x0005 => "Remove Session Animation",
                0x0006 => "Set Session Default",
                _ => "Session Animation Command",
            };
            op_name.to_string()
        }
        0x23 => {
            let count = usize::from(b4);
            let zones = if bytes.len() >= 5 + count {
                &bytes[5..5 + count]
            } else {
                &[]
            };
            let zone_desc = if zones.len() >= 7 {
                "All Chassis & Keyboard Zones"
            } else {
                match zones {
                    [0x04, 0x05, 0x06, 0x07] => "4 Keyboard Zones",
                    [0x01, 0x00] | [0x00, 0x01] => "Stadium Lights",
                    [0x03] => "Alienhead Logo",
                    [0x02] => "Power Button LED",
                    _ => "Selected Zones",
                }
            };
            format!("Select Target Zones → {zone_desc} ({count} LEDs)")
        }
        0x24 => {
            let action_name = match b2 {
                0x00 => "Morph",
                0x01 => "Pulse",
                0x02 => "Static Color",
                _ => "Keyframe",
            };
            let duration_ms = u16::from_be_bytes([b3, b4]);
            let r = bytes.get(7).copied().unwrap_or(0);
            let g = bytes.get(8).copied().unwrap_or(0);
            let b = bytes.get(9).copied().unwrap_or(0);
            format!(
                "Set Lighting Keyframe → {action_name} #{r:02X}{g:02X}{b:02X} ({duration_ms} ms)"
            )
        }
        0x26 => {
            let pct = 100_u8.saturating_sub(b2.min(100));
            format!("Set Hardware Brightness → {pct}% (Dim 0x{b2:02X}, {b3} zones)")
        }
        0x27 => {
            let count = bytes.get(6).copied().unwrap_or(0);
            format!("Set Direct Color → #{b2:02X}{b3:02X}{b4:02X} ({count} zones)")
        }
        _ => "ELC Lighting Command".to_string(),
    };

    Some(format!("{meaning}  ·  ELC [{hex_upper}]"))
}

/// Format a `WriteAuditEntry` into `(meaning_and_hex, clean_outcome)` with zero `pkexec sh -c` noise.
#[must_use]
pub fn format_log_entry(
    entry: &WriteAuditEntry,
    code_table: Option<&awcc_core::ThermalCodeTable>,
) -> (String, String) {
    let raw_cmd = entry.command.as_str();
    let command_text = if let Some(wmax_formatted) = decode_wmax_call(raw_cmd, code_table) {
        wmax_formatted
    } else if let Some(usb_formatted) = decode_usb_elc_call(raw_cmd) {
        usb_formatted
    } else if raw_cmd.contains("intel_pstate/no_turbo") {
        let disabled = raw_cmd.contains("echo 1") || raw_cmd.ends_with("= 1");
        if disabled {
            "Set CPU Turbo Boost → Disabled  ·  intel_pstate/no_turbo = 0x01".to_string()
        } else {
            "Set CPU Turbo Boost → Enabled  ·  intel_pstate/no_turbo = 0x00".to_string()
        }
    } else if raw_cmd.contains("cpufreq/boost") {
        let enabled = raw_cmd.contains("echo 1") || raw_cmd.ends_with("= 1");
        if enabled {
            "Set CPU Turbo Boost → Enabled  ·  cpufreq/boost = 0x01".to_string()
        } else {
            "Set CPU Turbo Boost → Disabled  ·  cpufreq/boost = 0x00".to_string()
        }
    } else {
        raw_cmd.to_string()
    };

    let outcome_text = match entry.outcome.as_str() {
        "0x0" | "0x0 (OK)" => "OK (0x00)".to_string(),
        "0x1" => "OK (0x01)".to_string(),
        // The outgoing transfer and the optional acknowledgement read are separate facts.
        "33 bytes transferred" => match &entry.ack {
            awcc_platform::AckStatus::Received => "Sent (33B) · ACK read".to_string(),
            awcc_platform::AckStatus::NotApplicable => "Sent (33B)".to_string(),
            awcc_platform::AckStatus::Incomplete { bytes } => {
                format!("Sent (33B) · ACK incomplete ({bytes}/33)")
            }
            awcc_platform::AckStatus::Failed { reason } => {
                format!("Sent (33B) · ACK read failed: {reason}")
            }
        },
        other if other.contains("0xffffffff") || other.contains("0xFFFFFFFF") => {
            "Rejected (0xFFFFFFFF)".to_string()
        }
        other => other.to_string(),
    };

    (command_text, outcome_text)
}

fn format_clean_lifecycle_message(
    lifecycle: &OperationLifecycle,
    code_table: Option<&awcc_core::ThermalCodeTable>,
) -> String {
    match lifecycle {
        OperationLifecycle::RolledBack { summary, reason } => {
            if let Some(decoded) = decode_wmax_call(reason, code_table) {
                format!("{summary} — Rejected: {decoded}")
            } else {
                format!("{summary} — {reason}")
            }
        }
        OperationLifecycle::Failed { summary, error } => {
            if let Some(decoded) = decode_wmax_call(error, code_table) {
                format!("{summary}: Hardware rejected {decoded}")
            } else {
                format!("{summary}: {error}")
            }
        }
        _ => lifecycle.detail_message(),
    }
}

const fn interval_label_for_ms(ms: i32) -> &'static str {
    match ms {
        500 => "0.5s",
        2000 => "2.0s",
        5000 => "5.0s",
        _ => "1.0s",
    }
}

/// Apply an immutable `SystemSnapshot` onto the Slint `MainWindow`.
fn sync_snapshot_to_ui(ui: &MainWindow, snap: &SystemSnapshot, sync_staged_sliders: bool) {
    ui.set_is_simulated(snap.is_simulated);
    sync_service_control_to_ui(ui, snap);
    ui.set_scenario_index(snap.scenario.index());
    ui.set_device_name(SharedString::from(snap.profile.device_name.as_str()));
    ui.set_acpi_path(SharedString::from(snap.profile.acpi_prefix.wmax_path()));
    ui.set_smbios_id(SharedString::from(snap.profile.smbios_system_id.as_str()));
    ui.set_db_source(SharedString::from(snap.profile.database_source.to_string()));
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    ui.set_write_count(snap.write_count() as i32);

    ui.set_lifecycle_badge(SharedString::from(snap.lifecycle.badge_label()));
    ui.set_lifecycle_message(SharedString::from(format_clean_lifecycle_message(
        &snap.lifecycle,
        Some(&snap.profile.code_table),
    )));
    ui.set_lifecycle_is_error(snap.lifecycle.is_error_or_rollback());

    ui.set_active_mode_label(SharedString::from(snap.thermal_mode_state.display_label()));
    match &snap.thermal_mode_state {
        ThermalModeState::Known(mode) => {
            let code = snap.profile.code_table.code_for(*mode);
            ui.set_active_mode_code_label(SharedString::from(format!("USTT 0x{code:02X}")));
            ui.set_mode_is_unknown(false);
            ui.set_selected_mode_key(SharedString::from(mode_key_for(*mode)));
        }
        ThermalModeState::Unknown {
            raw_code: Some(raw),
            ..
        } => {
            ui.set_active_mode_code_label(SharedString::from(format!("RAW 0x{raw:02X}")));
            ui.set_mode_is_unknown(true);
            ui.set_selected_mode_key(SharedString::from("Unknown"));
        }
        ThermalModeState::Unknown { raw_code: None, .. } => {
            ui.set_active_mode_code_label(SharedString::from("ERR"));
            ui.set_mode_is_unknown(true);
            ui.set_selected_mode_key(SharedString::from("Unknown"));
        }
    }

    let gmode_active = snap.game_shift_latched()
        || matches!(
            snap.thermal_mode_state,
            ThermalModeState::Known(ThermalMode::GMode)
        );
    ui.set_gmode_active(gmode_active);
    let restore_label = snap
        .pre_gmode_restore_target
        .unwrap_or(ThermalMode::Balanced)
        .display_name();
    ui.set_pre_gmode_restore_label(SharedString::from(restore_label));

    ui.set_cpu_temp_text(SharedString::from(
        snap.telemetry.cpu_temp_c.formatted_value(),
    ));
    ui.set_gpu_temp_text(SharedString::from(
        snap.telemetry.gpu_temp_c.formatted_value(),
    ));
    ui.set_cpu_rpm_text(SharedString::from(
        snap.telemetry.cpu_fan_rpm.formatted_value(),
    ));
    ui.set_gpu_rpm_text(SharedString::from(
        snap.telemetry.gpu_fan_rpm.formatted_value(),
    ));
    ui.set_cpu_boost_text(SharedString::from(
        snap.telemetry.cpu_boost_pct.formatted_value(),
    ));
    ui.set_gpu_boost_text(SharedString::from(
        snap.telemetry.gpu_boost_pct.formatted_value(),
    ));

    let cpu_boost_val = f32::from(snap.telemetry.cpu_boost_pct.value_or(0));
    let gpu_boost_val = f32::from(snap.telemetry.gpu_boost_pct.value_or(0));
    ui.set_cpu_boost_progress((cpu_boost_val / 100.0).clamp(0.0, 1.0));
    ui.set_gpu_boost_progress((gpu_boost_val / 100.0).clamp(0.0, 1.0));
    ui.set_sensors_available(snap.telemetry.cpu_temp_c.is_available());
    sync_turbo(ui, &snap.cpu_turbo);
    ui.set_telemetry_notice(SharedString::from(telemetry_notice(snap)));
    ui.set_build_summary(SharedString::from(build_summary()));

    ui.set_tcc_offset_text(SharedString::from(snap.tcc_offset_c.formatted_value()));
    if sync_staged_sliders || !ui.get_tcc_dirty() {
        ui.set_staged_tcc_offset(f32::from(snap.tcc_offset_c.value_or(0)));
        ui.set_tcc_dirty(false);
    }

    if sync_staged_sliders || !ui.get_boost_dirty() {
        ui.set_staged_cpu_boost(cpu_boost_val);
        ui.set_staged_gpu_boost(gpu_boost_val);
        ui.set_boost_dirty(false);
    }

    ui.set_fan_curve_active(snap.fan_curve.enabled);
    if (sync_staged_sliders || !ui.get_curve_dirty()) && snap.fan_curve.cpu_points.len() >= 4 {
        ui.set_staged_curve_p1(i32::from(snap.fan_curve.cpu_points[0].boost_pct));
        ui.set_staged_curve_p2(i32::from(snap.fan_curve.cpu_points[1].boost_pct));
        ui.set_staged_curve_p3(i32::from(snap.fan_curve.cpu_points[2].boost_pct));
        ui.set_staged_curve_p4(i32::from(snap.fan_curve.cpu_points[3].boost_pct));
        ui.set_staged_curve_hysteresis(f32::from(snap.fan_curve.hysteresis_c));
        ui.set_curve_dirty(false);
    }

    // Sync lighting state
    ui.set_lighting_stealth_enabled(snap.lighting_stealth_enabled);
    if sync_staged_sliders {
        ui.set_staged_lighting_brightness(f32::from(snap.lighting_brightness_pct));
    }
    let persist_str = if snap.lighting_persist_boot {
        "Saved as Boot Default"
    } else {
        "Session Only"
    };
    let is_commanded = snap.lighting_state_source == LightingStateSource::Commanded;
    let logo_commanded = is_commanded && snap.lighting_commanded_zones.contains(&0x03);
    let tron_commanded = is_commanded
        && (snap.lighting_commanded_zones.contains(&0x00)
            || snap.lighting_commanded_zones.contains(&0x01));
    let kbd_commanded =
        is_commanded && (0x04_u8..=0x07).any(|z| snap.lighting_commanded_zones.contains(&z));
    let power_commanded = is_commanded
        && (snap.lighting_commanded_zones.contains(&0x02)
            || snap
                .lighting_commanded_power_slots
                .contains(&PowerStateSlot::AcOn.slot_id()));
    let any_zone_commanded = logo_commanded || tron_commanded || kbd_commanded || power_commanded;
    let all_zones_commanded = logo_commanded && tron_commanded && kbd_commanded && power_commanded;

    // The ELC protocol has no readback of colours or brightness: say where the values come from.
    let provenance = match snap.lighting_state_source {
        LightingStateSource::Commanded if all_zones_commanded => {
            " · remembered from last command (no ELC colour readback)"
        }
        LightingStateSource::Commanded if any_zone_commanded => {
            " · remembered for commanded zones; others show assumed defaults"
        }
        LightingStateSource::Commanded => {
            " · brightness commanded (remembered); zone swatches show assumed defaults"
        }
        LightingStateSource::PersistedBrightness => {
            " · brightness restored from saved setting, effect & swatches assumed default"
        }
        LightingStateSource::AssumedDefault => " · assumed defaults, nothing commanded yet",
    };
    ui.set_active_lighting_summary(SharedString::from(format!(
        "{} · {} · Brightness {}% · {persist_str}{provenance}",
        snap.lighting_last_target.label(),
        snap.lighting_last_effect.name(),
        snap.lighting_brightness_pct
    )));

    let logo_rgb = snap
        .lighting_zone_colors
        .get(&0x03)
        .copied()
        .unwrap_or(0x00F0FF);
    let tron_rgb = snap
        .lighting_zone_colors
        .get(&0x00)
        .or_else(|| snap.lighting_zone_colors.get(&0x01))
        .copied()
        .unwrap_or(0x00F0FF);
    let kbd_rgb = snap
        .lighting_zone_colors
        .get(&0x04)
        .copied()
        .unwrap_or(0x00F0FF);
    let pwr_rgb = snap
        .lighting_power_colors
        .get(&PowerStateSlot::AcOn.slot_id())
        .copied()
        .unwrap_or(0x00F0FF);
    ui.set_zone_logo_color(u32_to_slint_color(logo_rgb));
    ui.set_zone_tron_color(u32_to_slint_color(tron_rgb));
    ui.set_zone_kbd_color(u32_to_slint_color(kbd_rgb));
    ui.set_zone_power_color(u32_to_slint_color(pwr_rgb));
    let swatch_provenance = |commanded: bool| -> SharedString {
        if commanded {
            SharedString::from("Remembered (commanded)")
        } else {
            SharedString::from("Assumed default")
        }
    };
    ui.set_zone_logo_provenance(swatch_provenance(logo_commanded));
    ui.set_zone_tron_provenance(swatch_provenance(tron_commanded));
    ui.set_zone_kbd_provenance(swatch_provenance(kbd_commanded));
    ui.set_zone_power_provenance(swatch_provenance(power_commanded));

    ui.set_elc_firmware_summary(SharedString::from(format_elc_summary(
        &snap.elc_diagnostics,
    )));

    let latest_seq = i32::try_from(snap.audit_stats.last_seq).unwrap_or(i32::MAX);
    let retained_count = i32::try_from(snap.audit_stats.retained).unwrap_or(i32::MAX);
    let evicted_count = i32::try_from(snap.audit_stats.evicted).unwrap_or(i32::MAX);
    let cleared_count = i32::try_from(snap.audit_stats.cleared).unwrap_or(i32::MAX);

    let counter_summary = if snap.audit_stats.lifetime_total == 0 {
        "Zero unsolicited writes since startup".to_string()
    } else if snap.audit_stats.evicted > 0 && snap.audit_stats.cleared > 0 {
        format!(
            "{} lifetime · {} retained · {} evicted · {} cleared",
            snap.audit_stats.lifetime_total,
            snap.audit_stats.retained,
            snap.audit_stats.evicted,
            snap.audit_stats.cleared
        )
    } else if snap.audit_stats.evicted > 0 {
        format!(
            "{} lifetime · {} retained · {} evicted",
            snap.audit_stats.lifetime_total, snap.audit_stats.retained, snap.audit_stats.evicted
        )
    } else if snap.audit_stats.cleared > 0 {
        format!(
            "{} lifetime · {} retained · {} cleared",
            snap.audit_stats.lifetime_total, snap.audit_stats.retained, snap.audit_stats.cleared
        )
    } else {
        format!(
            "{} explicit commands recorded ({} retained)",
            snap.audit_stats.lifetime_total, snap.audit_stats.retained
        )
    };
    ui.set_audit_counter_summary(SharedString::from(counter_summary));

    let eviction_notice = if snap.audit_stats.evicted > 0 {
        format!(
            "Ring buffer gap: {} older write entries evicted at constant {} capacity (oldest retained #{:02}).",
            snap.audit_stats.evicted,
            snap.audit_stats.capacity,
            snap.audit_stats.first_retained_seq.unwrap_or(0)
        )
    } else if snap.audit_stats.cleared > 0 {
        format!(
            "Visible history cleared ({} earlier entries hidden; lifetime counter preserved).",
            snap.audit_stats.cleared
        )
    } else {
        String::new()
    };
    ui.set_audit_eviction_notice(SharedString::from(eviction_notice));

    // Rebuild the virtualized Log model whenever the newest sequence, cleared count, or retained
    // row count changes (never relying solely on `row_count() != len()`, which stalls at full ring capacity).
    let needs_audit_rebuild = sync_staged_sliders
        || ui.get_audit_latest_seq() != latest_seq
        || ui.get_audit_cleared_count() != cleared_count
        || ui.get_audit_rows().row_count() != snap.write_audit_log.len();

    ui.set_audit_latest_seq(latest_seq);
    ui.set_audit_retained_count(retained_count);
    ui.set_audit_evicted_count(evicted_count);
    ui.set_audit_cleared_count(cleared_count);

    if needs_audit_rebuild {
        let redactor = Redactor::from_environment();
        let include_raw_usb =
            ui.get_trace_raw_usb() || global_diagnostics().is_some_and(|d| d.trace_raw_usb());
        let selected_seq = ui.get_selected_audit_seq();
        let mut refreshed_selected_detail: Option<String> = None;

        let rows: Vec<AuditRow> = snap
            .write_audit_log
            .iter()
            .rev()
            .map(|entry| {
                let (command_str, outcome_str) =
                    format_log_entry(entry, Some(&snap.profile.code_table));
                let detail_str = format_audit_detail(
                    entry,
                    &command_str,
                    &outcome_str,
                    &redactor,
                    include_raw_usb,
                );
                let seq_num = i32::try_from(entry.sequence).unwrap_or(i32::MAX);
                if selected_seq > 0 && seq_num == selected_seq {
                    refreshed_selected_detail = Some(detail_str.clone());
                }
                AuditRow {
                    seq_num,
                    seq_text: SharedString::from(format!("#{:02}", entry.sequence)),
                    command: SharedString::from(command_str),
                    outcome: SharedString::from(outcome_str),
                    detail: SharedString::from(detail_str),
                    succeeded: entry.succeeded,
                }
            })
            .collect();
        ui.set_audit_rows(ModelRc::new(VecModel::from(rows)));

        if selected_seq > 0 {
            if let Some(detail) = refreshed_selected_detail {
                ui.set_selected_audit_detail(SharedString::from(detail));
            } else {
                ui.set_selected_audit_seq(0);
                ui.set_selected_audit_detail(SharedString::default());
            }
        }
    }
}

/// Format the full, copyable detail string for a single `WriteAuditEntry` (redacting PII).
///
/// Raw USB 33-byte response frames (`raw_usb_in=[...]`) are only included when `include_raw_usb`
/// is explicitly opted in (`--trace-raw-usb` / `AWCC_TRACE_RAW_USB=1`).
#[must_use]
pub fn format_audit_detail(
    entry: &WriteAuditEntry,
    decoded_command: &str,
    decoded_outcome: &str,
    redactor: &Redactor,
    include_raw_usb: bool,
) -> String {
    let op_str = entry
        .op_id
        .map_or_else(|| "none".to_string(), |id| format!("op#{id}"));
    let raw_cmd = redactor.redact_text(&entry.command);
    let raw_outcome = redactor.redact_text(&entry.outcome);
    let ack_desc = redactor.redact_text(&entry.ack.describe());
    let raw_usb_suffix = match (&entry.raw_usb_response_hex, include_raw_usb) {
        (Some(hex), true) => format!(" | raw_usb_in=[{hex}]"),
        _ => String::new(),
    };
    format!(
        "#{:02} ({op_str}) {decoded_command} -> {decoded_outcome} | raw=\"{raw_cmd}\" | outcome=\"{raw_outcome}\" | ack={ack_desc}{raw_usb_suffix}",
        entry.sequence
    )
}

fn elc_report_text(diag: &ElcDiagnosticsSummary) -> String {
    format!(
        "ELC {:04x}:{:04x} · FW {}.{}.{} · Platform 0x{:04X} ({} Active Zones, {} Stored Animations)",
        diag.vendor_id,
        diag.product_id,
        diag.firmware_version.0,
        diag.firmware_version.1,
        diag.firmware_version.2,
        diag.elc_config.platform_id,
        diag.elc_config.active_zones,
        diag.animation_count
    )
}

/// Present ELC diagnostics without ever substituting a plausible default for a missing report.
fn format_elc_summary(obs: &Observation<ElcDiagnosticsSummary>) -> String {
    match obs {
        Observation::Observed(diag) => elc_report_text(diag),
        Observation::NotObserved => "ELC controller diagnostics: not yet observed".to_string(),
        Observation::Unavailable { reason } => {
            format!("ELC controller diagnostics unavailable: {reason}")
        }
        Observation::Stale {
            last_value,
            age_secs,
            reason,
        } => format!(
            "{} — STALE: last observed {age_secs}s ago; latest query failed: {reason}",
            elc_report_text(last_value)
        ),
    }
}

/// Drive the turbo switch from the observation: it is only interactive when the latest query
/// succeeded, and otherwise says why instead of showing an invented on/off state.
fn sync_turbo(ui: &MainWindow, turbo: &Observation<bool>) {
    let (known, enabled, note) = match turbo {
        Observation::Observed(v) => (true, *v, String::new()),
        Observation::NotObserved => (false, false, "state not yet observed".to_string()),
        Observation::Unavailable { reason } => {
            (false, false, format!("state unavailable: {reason}"))
        }
        Observation::Stale {
            last_value,
            age_secs,
            reason,
        } => (
            false,
            *last_value,
            format!(
                "last known {} (stale {age_secs}s): {reason}",
                if *last_value { "on" } else { "off" }
            ),
        ),
    };
    ui.set_cpu_turbo_known(known);
    ui.set_cpu_turbo_enabled(enabled);
    ui.set_cpu_turbo_note(SharedString::from(note));
}

/// One line naming every reading that is currently unavailable or stale, with the first reason.
fn telemetry_notice(snap: &SystemSnapshot) -> String {
    let t = &snap.telemetry;
    let mut names: Vec<&str> = Vec::new();
    let mut first_reason: Option<&str> = None;
    let candidates: [(&str, Option<&str>); 9] = [
        ("CPU temp", t.cpu_temp_c.problem()),
        ("GPU temp", t.gpu_temp_c.problem()),
        ("CPU fan", t.cpu_fan_rpm.problem()),
        ("GPU fan", t.gpu_fan_rpm.problem()),
        ("CPU boost", t.cpu_boost_pct.problem()),
        ("GPU boost", t.gpu_boost_pct.problem()),
        ("TCC offset", snap.tcc_offset_c.problem()),
        ("GameShift", snap.game_shift.problem()),
        ("CPU turbo", snap.cpu_turbo.problem()),
    ];
    for (name, problem) in candidates {
        if let Some(reason) = problem {
            names.push(name);
            first_reason.get_or_insert(reason);
        }
    }
    match first_reason {
        None => String::new(),
        Some(reason) => format!(
            "Not current (unavailable or stale): {} — {reason}",
            names.join(", ")
        ),
    }
}

fn build_summary() -> &'static str {
    static SUMMARY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SUMMARY.get_or_init(|| {
        let info = awcc_build_info::BuildInfo::current();
        format!(
            "Build: awcc-gui {} · {} · {} · package: {}",
            info.headline(),
            info.target,
            info.rustc,
            awcc_build_info::detect_package_format().as_str()
        )
    })
}

/// Abstraction over desktop clipboard writes so GUI copy actions perform real clipboard
/// operations in desktop sessions and can be deterministically verified (including failure
/// modes) in headless tests.
pub trait ClipboardBackend: Send {
    fn set_text(&mut self, text: &str) -> Result<(), String>;
}

/// Real Linux desktop clipboard provider (`wl-copy` under Wayland, `xclip` / `xsel` under X11).
struct SystemClipboard;

impl SystemClipboard {
    fn try_command_with_stdin(program: &str, args: &[&str], text: &str) -> Result<(), String> {
        use std::process::{Command, Stdio};

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{program}: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| format!("{program} stdin write failed: {e}"))?;
        }

        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        return Ok(());
                    }
                    return Err(format!("{program} exited with {status}"));
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(format!("{program} wait failed: {e}")),
            }
        }
    }
}

impl ClipboardBackend for SystemClipboard {
    fn set_text(&mut self, text: &str) -> Result<(), String> {
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        let has_x11 = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());

        if !has_wayland && !has_x11 {
            return Err("no WAYLAND_DISPLAY or DISPLAY session available".to_string());
        }

        let mut errors = Vec::new();
        if has_wayland {
            match Self::try_command_with_stdin("wl-copy", &[], text) {
                Ok(()) => return Ok(()),
                Err(e) => errors.push(e),
            }
        }
        if has_x11 {
            for (prog, args) in [
                ("xclip", &["-selection", "clipboard", "-i"][..]),
                ("xsel", &["--clipboard", "--input"][..]),
            ] {
                match Self::try_command_with_stdin(prog, args, text) {
                    Ok(()) => return Ok(()),
                    Err(e) => errors.push(e),
                }
            }
        }

        Err(if errors.is_empty() {
            "no desktop clipboard utility available".to_string()
        } else {
            errors.join("; ")
        })
    }
}

enum DiagJobResult {
    Summary(Result<(String, Result<(), String>), String>),
    Export {
        out_path: PathBuf,
        result: Result<u64, String>,
    },
}

#[derive(Clone)]
struct ServiceInspection {
    installation: ServiceStatusReport,
    daemon: ServiceProbe,
}

/// All host access for the Settings service card runs on a background thread. Keeping this
/// boundary injectable also lets the callbacks be tested without systemd or authorization.
trait ServiceManagementBackend: Send + Sync {
    fn inspect(&self) -> ServiceInspection;
    fn apply(
        &self,
        action: ServiceAction,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<ApplyReport, String>;
}

struct SystemServiceManagement;

impl ServiceManagementBackend for SystemServiceManagement {
    fn inspect(&self) -> ServiceInspection {
        ServiceInspection {
            installation: installer::inspect(&InstallPaths::system(), &SystemRunner::new()),
            daemon: DaemonClient::system("awcc-gui", env!("CARGO_PKG_VERSION")).probe(),
        }
    }

    fn apply(
        &self,
        action: ServiceAction,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<ApplyReport, String> {
        // Both release archives and AppImages place the CLI beside the GUI. The management
        // helper selects the original APPIMAGE when present; it never elevates this GUI.
        let cli = std::env::current_exe()
            .map_err(|error| format!("Cannot locate awcc-cli: {error}"))?
            .with_file_name("awcc-cli");
        awcc_service::management::apply_system(action, &cli, progress)
            .map_err(|error| error.to_string())
    }
}

enum ServiceJobEvent {
    Progress(String),
    Outcome(Result<ApplyReport, String>),
    Status(Box<ServiceInspection>),
}

fn disable_service_management_ui(ui: &MainWindow, label: &str, detail: &str) {
    ui.set_service_state_label(label.into());
    ui.set_service_state_detail(detail.into());
    ui.set_service_state_is_problem(false);
    ui.set_service_managed_by("none".into());
    ui.set_service_can_install(false);
    ui.set_service_can_setup(false);
    ui.set_service_can_update(false);
    ui.set_service_can_remove(false);
    ui.set_service_restore_available(false);
    ui.set_service_confirming_install(false);
    ui.set_service_confirming_remove(false);
    ui.set_service_remove_purge(false);
    ui.set_service_version_line("".into());
    ui.set_service_curve_summary("".into());
}

fn sync_service_control_to_ui(ui: &MainWindow, snap: &SystemSnapshot) {
    if snap.is_simulated {
        disable_service_management_ui(
            ui,
            "Simulation",
            "Simulation runs in this window. Service installation and host status checks are disabled.",
        );
        return;
    }
    ui.set_service_restore_available(
        snap.service_control.is_some() && snap.service_error.is_none(),
    );
    if let Some(control) = &snap.service_control {
        ui.set_service_restore_at_startup(control.restore_at_startup);
        let state = match control.state {
            ControlState::Idle => "No custom fan curve is running.",
            ControlState::Active => "The service is applying the custom fan curve.",
            ControlState::Fallback => "The custom fan curve is suspended in fallback.",
            ControlState::RestoreFailed => "The saved fan curve could not be restored.",
        };
        let mut details = vec![state.to_string()];
        if let Some(reason) = &control.failure_reason {
            details.push(reason.clone());
        }
        if let Some(problem) = &control.settings_problem {
            details.push(format!("Saved settings: {problem}"));
        }
        if let Some(error) = &control.settings_save_error {
            details.push(format!("Settings could not be saved: {error}"));
        } else if control.settings_save_pending {
            details.push("Saving settings…".into());
        }
        if let Some(error) = &snap.service_error {
            details.insert(0, format!("Last known state; service unavailable: {error}"));
        }
        ui.set_service_curve_summary(details.join(" ").into());
    } else {
        ui.set_service_curve_summary(snap.service_error.as_ref().map_or_else(
            || "Waiting for the service control state.".into(),
            |error| format!("Service control unavailable: {error}").into(),
        ));
    }
}

fn sync_service_inspection_to_ui(ui: &MainWindow, inspection: &ServiceInspection) {
    let report = &inspection.installation;
    let mut label = match report.unit_state {
        UnitState::Missing => "Not installed",
        UnitState::Stopped => "Stopped",
        UnitState::Running => "Running",
        UnitState::Activating => "Starting",
        UnitState::Deactivating => "Stopping",
        UnitState::Failed => "Failed",
        UnitState::Masked => "Masked",
        UnitState::Unknown => "Status unavailable",
    };
    let mut problem = matches!(
        report.unit_state,
        UnitState::Failed | UnitState::Masked | UnitState::Unknown
    ) || !report.problems.is_empty();
    let mut details = vec![match &report.ownership {
        Ownership::None => "No service installation was found.".to_string(),
        Ownership::AppimageInstaller { .. } => {
            "Managed by this application's service installer.".to_string()
        }
        Ownership::NativePackage {
            manager, package, ..
        } => format!(
            "Managed by {manager} package {package}. Use the package manager for updates and removal."
        ),
        Ownership::Unknown { unit_path, reason } => {
            format!("Managed outside this application at {unit_path}: {reason}")
        }
    }];
    if let Some(enabled) = report.enabled {
        details.push(if enabled {
            "Enabled at boot.".into()
        } else {
            "Disabled at boot.".into()
        });
    }
    let mut service_version = report.version.as_deref();
    match &inspection.daemon {
        ServiceProbe::Running {
            version, status, ..
        } => {
            service_version = Some(version);
            if !status.control_running {
                label = "Control unavailable";
                problem = true;
                details.push("The daemon has not published its control state.".into());
            } else if report.unit_state == UnitState::Missing {
                label = "Running outside systemd";
            }
        }
        ServiceProbe::AccessDenied { message } => {
            label = "Access denied";
            problem = true;
            details.push(format!(
                "{message}. Select Set up service to authorize your account with administrator approval."
            ));
        }
        ServiceProbe::Incompatible { message } => {
            label = "Incompatible service";
            problem = true;
            details.push(message.clone());
        }
        ServiceProbe::NotRunning { reason } => {
            if report.unit_state == UnitState::Running {
                label = "Unreachable";
                problem = true;
            }
            if report.unit_state != UnitState::Missing {
                details.push(reason.clone());
            }
        }
    }
    details.extend(report.problems.iter().cloned());
    ui.set_service_state_label(label.into());
    ui.set_service_state_detail(details.join(" ").into());
    ui.set_service_state_is_problem(problem);
    ui.set_service_managed_by(report.managed_by.as_str().into());
    ui.set_service_version_line(
        format!(
            "GUI {} · service {}",
            env!("CARGO_PKG_VERSION"),
            service_version.unwrap_or("not available")
        )
        .into(),
    );
    let installer_owned = matches!(report.ownership, Ownership::AppimageInstaller { .. });
    let manageable = report.unit_state != UnitState::Unknown && !report.masked;
    let supported_owner = installer_owned
        || matches!(&report.ownership, Ownership::NativePackage { package, .. } if package == "awcc-rs");
    let ready = matches!(&inspection.daemon, ServiceProbe::Running { status, .. } if status.control_running);
    ui.set_service_can_setup(
        manageable
            && supported_owner
            && !report.interrupted_operation
            && !matches!(inspection.daemon, ServiceProbe::Incompatible { .. })
            && (!ready || report.enabled == Some(false)),
    );
    ui.set_service_can_install(
        manageable
            && matches!(report.ownership, Ownership::None)
            && report.unit_state == UnitState::Missing
            && report.problems.is_empty()
            && matches!(inspection.daemon, ServiceProbe::NotRunning { .. }),
    );
    ui.set_service_can_update(manageable && installer_owned);
    ui.set_service_can_remove(manageable && installer_owned);
    if !ui.get_service_can_install() {
        ui.set_service_confirming_install(false);
    }
    if !ui.get_service_can_remove() {
        ui.set_service_confirming_remove(false);
    }
}

fn service_apply_message(report: &ApplyReport) -> String {
    let outcome = match report.outcome {
        ApplyOutcome::Installed => "Background service installed.",
        ApplyOutcome::Updated => "Background service updated.",
        ApplyOutcome::Configured => "Background service configured.",
        ApplyOutcome::Unchanged => "This service build is already installed.",
        ApplyOutcome::Uninstalled if report.purged => {
            "Background service removed. Saved service settings were deleted."
        }
        ApplyOutcome::Uninstalled => "Background service removed. Saved settings were kept.",
        ApplyOutcome::NothingToUninstall => "No application-managed service was installed.",
    };
    std::iter::once(outcome)
        .chain(report.notes.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn start_service_job(ui: &MainWindow, ops: &SharedOps, action: Option<ServiceAction>) {
    // Guard the callbacks themselves as well as the buttons: simulation must never reach a
    // host backend, even if a callback is invoked programmatically.
    if ui.get_is_simulated() || ops.borrow().service_busy {
        return;
    }
    if let Some(action) = action {
        let allowed = match action {
            ServiceAction::Install => {
                ui.get_service_can_install() && ui.get_service_confirming_install()
            }
            ServiceAction::Update => ui.get_service_can_update(),
            ServiceAction::Setup => ui.get_service_can_setup(),
            ServiceAction::Uninstall { .. } => {
                ui.get_service_can_remove() && ui.get_service_confirming_remove()
            }
        };
        if !allowed {
            return;
        }
        ui.set_service_result("".into());
        ui.set_service_result_is_error(false);
        ui.set_service_confirming_install(false);
        ui.set_service_confirming_remove(false);
    }
    let (backend, tx) = {
        let mut state = ops.borrow_mut();
        state.service_busy = true;
        (
            Arc::clone(&state.service_backend),
            state.service_job_tx.clone(),
        )
    };
    ui.set_service_busy(true);
    ui.set_service_progress(
        if action.is_some() {
            "Waiting for administrator authorization…"
        } else {
            "Checking service status…"
        }
        .into(),
    );
    let spawned = std::thread::Builder::new()
        .name("awcc-service-settings".into())
        .spawn(move || {
            if let Some(action) = action {
                let result = backend.apply(action, &mut |progress| {
                    let _ = tx.send(ServiceJobEvent::Progress(progress.message));
                });
                let _ = tx.send(ServiceJobEvent::Outcome(result));
                let _ = tx.send(ServiceJobEvent::Progress("Checking service status…".into()));
            }
            let _ = tx.send(ServiceJobEvent::Status(Box::new(backend.inspect())));
        });
    if let Err(error) = spawned {
        ops.borrow_mut().service_busy = false;
        ui.set_service_busy(false);
        ui.set_service_progress("".into());
        ui.set_service_result(format!("Cannot start service task: {error}").into());
        ui.set_service_result_is_error(true);
    }
}

fn poll_service_jobs(ui: &MainWindow, ops: &SharedOps) {
    loop {
        let received = ops.borrow().service_job_rx.try_recv().ok();
        let Some(event) = received else {
            break;
        };
        match event {
            ServiceJobEvent::Progress(message) => ui.set_service_progress(message.into()),
            ServiceJobEvent::Outcome(result) => {
                ui.set_service_result_is_error(result.is_err());
                ui.set_service_result(match result {
                    Ok(report) => service_apply_message(&report).into(),
                    Err(error) => error.into(),
                });
            }
            ServiceJobEvent::Status(inspection) => {
                sync_service_inspection_to_ui(ui, &inspection);
                ops.borrow_mut().service_busy = false;
                ui.set_service_busy(false);
                ui.set_service_progress("".into());
            }
        }
    }
}

/// A user request that was queued but that the worker has not reported on yet.
#[derive(Debug, Clone)]
struct PendingRequest {
    seq: u64,
    label: String,
}

/// UI-side bookkeeping for requests in flight, worker-failure notices, and passive diagnostics.
struct UiOps {
    pending: Option<PendingRequest>,
    worker_failure_shown: bool,
    diagnostics: Arc<DiagnosticsRuntime>,
    export_output_override: Option<PathBuf>,
    clipboard: Arc<std::sync::Mutex<Box<dyn ClipboardBackend>>>,
    diagnostics_busy: bool,
    diag_job_tx: std::sync::mpsc::Sender<DiagJobResult>,
    diag_job_rx: std::sync::mpsc::Receiver<DiagJobResult>,
    diag_job_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    service_backend: Arc<dyn ServiceManagementBackend>,
    service_busy: bool,
    service_job_tx: std::sync::mpsc::Sender<ServiceJobEvent>,
    service_job_rx: std::sync::mpsc::Receiver<ServiceJobEvent>,
}

impl Default for UiOps {
    fn default() -> Self {
        let diagnostics = global_diagnostics().cloned().unwrap_or_else(|| {
            let resolved = resolve_log_level(None, false, None)
                .unwrap_or_else(|_| unreachable!("default INFO level is valid"));
            let mut cfg = DiagnosticsConfig::from_env_and_cli("awcc-gui", resolved, false);
            cfg.enable_file_sink = false;
            cfg.enable_stderr_console = false;
            DiagnosticsRuntime::new(cfg)
        });
        let (diag_job_tx, diag_job_rx) = std::sync::mpsc::channel();
        let (service_job_tx, service_job_rx) = std::sync::mpsc::channel();
        Self {
            pending: None,
            worker_failure_shown: false,
            diagnostics,
            export_output_override: None,
            clipboard: Arc::new(std::sync::Mutex::new(Box::new(SystemClipboard))),
            diagnostics_busy: false,
            diag_job_tx,
            diag_job_rx,
            diag_job_hook: None,
            service_backend: Arc::new(SystemServiceManagement),
            service_busy: false,
            service_job_tx,
            service_job_rx,
        }
    }
}

type SharedOps = Rc<RefCell<UiOps>>;
type SharedService = Rc<RefCell<ThermalServiceHandle>>;

fn poll_diagnostics_jobs(ui: &MainWindow, ops: &SharedOps) -> bool {
    let mut completed_any = false;
    loop {
        let received = ops.borrow().diag_job_rx.try_recv().ok();
        let Some(job) = received else {
            break;
        };
        completed_any = true;
        ops.borrow_mut().diagnostics_busy = false;
        ui.set_diagnostics_busy(false);
        match job {
            DiagJobResult::Summary(Ok((summary_txt, Ok(())))) => {
                ui.set_clipboard_fallback_visible(false);
                ui.set_last_copied_text(SharedString::from(summary_txt.as_str()));
                ui.set_diagnostics_status_text(SharedString::from(
                    "Copied redacted diagnostic summary to clipboard.",
                ));
            }
            DiagJobResult::Summary(Ok((summary_txt, Err(clip_err)))) => {
                ui.set_clipboard_fallback_visible(true);
                ui.set_last_copied_text(SharedString::from(summary_txt.as_str()));
                ui.set_diagnostics_status_text(SharedString::from(format!(
                    "Clipboard unavailable ({clip_err}) — summary is ready in the read-only fallback below, or use Export Support Report."
                )));
            }
            DiagJobResult::Summary(Err(err)) => {
                ui.set_diagnostics_status_text(SharedString::from(format!(
                    "Failed to assemble diagnostic summary: {err}"
                )));
            }
            DiagJobResult::Export {
                out_path,
                result: Ok(bytes),
            } => {
                ui.set_last_export_path(SharedString::from(out_path.display().to_string()));
                ui.set_diagnostics_status_text(SharedString::from(format!(
                    "Exported passive support report -> {} ({bytes} B)",
                    out_path.display()
                )));
            }
            DiagJobResult::Export {
                out_path: _,
                result: Err(err),
            } => {
                ui.set_diagnostics_status_text(SharedString::from(format!(
                    "Support report export failed: {err}"
                )));
            }
        }
    }
    completed_any
}

fn sync_recording_state_to_ui(ui: &MainWindow, diag: &DiagnosticsRuntime) {
    let active = diag.is_recording_active();
    if ui.get_diagnostic_recording_active() != active {
        ui.set_diagnostic_recording_active(active);
        if active {
            ui.set_diagnostic_recording_label(SharedString::from("Stop DEBUG Recording"));
        } else {
            ui.set_diagnostic_recording_label(SharedString::from("Start DEBUG Recording"));
        }
    }
}

fn collect_gui_settings(ui: &MainWindow) -> BTreeMap<String, String> {
    let mut settings = BTreeMap::new();
    settings.insert(
        "auto_refresh_enabled".to_string(),
        ui.get_auto_refresh_enabled().to_string(),
    );
    settings.insert(
        "refresh_interval_ms".to_string(),
        ui.get_refresh_interval_ms().to_string(),
    );
    settings.insert(
        "staged_lighting_persist_boot".to_string(),
        ui.get_staged_lighting_persist_boot().to_string(),
    );
    settings.insert("sim_mode".to_string(), ui.get_is_simulated().to_string());
    settings
}

fn default_support_export_path(session_id: &str) -> PathBuf {
    if let Ok(custom) = std::env::var("AWCC_SUPPORT_EXPORT_PATH") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let filename = format!(
        "awcc-support-{}-{session_id}.zip",
        awcc_diagnostics::time::compact_utc(awcc_diagnostics::time::now_unix_millis())
    );
    if let Some(dir) = awcc_diagnostics::default_log_dir() {
        return dir.join(filename);
    }
    std::env::temp_dir().join(filename)
}

fn show_lifecycle(ui: &MainWindow, badge: &str, message: &str, is_error: bool) {
    ui.set_lifecycle_badge(SharedString::from(badge));
    ui.set_lifecycle_message(SharedString::from(message));
    ui.set_lifecycle_is_error(is_error);
}

/// A queued request is only *requested*: nothing has executed and nothing is confirmed.
fn show_requested(ui: &MainWindow, pending: &PendingRequest) {
    show_lifecycle(
        ui,
        "REQUESTED",
        &format!(
            "Requested: {} — waiting for the service worker",
            pending.label
        ),
        false,
    );
}

fn show_worker_failure(ui: &MainWindow, failure: &WorkerFailure) {
    show_lifecycle(
        ui,
        "SERVICE STOPPED",
        &format!("The service {failure}. Hardware controls no longer respond; restart awcc-gui."),
        true,
    );
}

/// Surface a stopped worker once and stop the periodic refresh that can no longer be served.
fn report_worker_failure_once(
    ui: &MainWindow,
    svc: &SharedService,
    ops: &SharedOps,
    refresh_timer: &Timer,
) {
    if ops.borrow().worker_failure_shown {
        return;
    }
    let failure = svc.borrow().worker_failure();
    if let Some(failure) = failure {
        show_worker_failure(ui, &failure);
        ops.borrow_mut().worker_failure_shown = true;
        refresh_timer.stop();
    }
}

/// Queue a user-initiated command. Returns `true` only if it was queued.
///
/// A rejection is shown immediately (queue full vs. worker stopped stay distinct), is never
/// retried, and leaves any staged edits untouched so the user can try again.
fn submit(ui: &MainWindow, svc: &SharedService, ops: &SharedOps, cmd: ServiceCommand) -> bool {
    let label = cmd.label();
    let state_changing = cmd.is_state_changing();
    let sent = svc.borrow().send(cmd);
    match sent {
        Ok(ticket) => {
            if state_changing {
                let pending = PendingRequest {
                    seq: ticket.seq,
                    label,
                };
                show_requested(ui, &pending);
                ops.borrow_mut().pending = Some(pending);
            }
            true
        }
        Err(err) => {
            show_lifecycle(ui, "NOT SENT", &format!("{label}: {err}"), true);
            false
        }
    }
}

/// Queue a periodic tick (`Refresh` / `StepFanCurve`). A full queue just skips the tick (never
/// replayed); a stopped worker is reported once.
fn submit_background(
    ui: &MainWindow,
    svc: &SharedService,
    ops: &SharedOps,
    refresh_timer: &Timer,
    cmd: ServiceCommand,
) {
    let sent = svc.borrow().send(cmd);
    if let Err(SendError::WorkerDisconnected(_)) = sent {
        report_worker_failure_once(ui, svc, ops, refresh_timer);
    }
}

/// Apply a snapshot published by the worker, keeping a still-unanswered request visible as
/// `REQUESTED` instead of letting an older outcome stand in for it.
fn apply_polled_snapshot(ui: &MainWindow, snap: &SystemSnapshot, ops: &SharedOps) {
    sync_snapshot_to_ui(ui, snap, false);
    let diag = Arc::clone(&ops.borrow().diagnostics);
    sync_recording_state_to_ui(ui, &diag);
    poll_diagnostics_jobs(ui, ops);
    let pending = ops.borrow().pending.clone();
    if let Some(pending) = pending {
        if snap.command_seq >= pending.seq {
            ops.borrow_mut().pending = None;
        } else {
            show_requested(ui, &pending);
        }
    }
}

fn restart_auto_refresh_timer(
    timer: &Rc<Timer>,
    ui_weak: slint::Weak<MainWindow>,
    service: SharedService,
    ops: SharedOps,
    enabled: bool,
    interval_ms: i32,
) {
    if !enabled {
        timer.stop();
        return;
    }
    let ms = u64::try_from(interval_ms.clamp(250, 60_000)).unwrap_or(1000);
    let timer_for_tick = Rc::clone(timer);
    timer.start(TimerMode::Repeated, Duration::from_millis(ms), move || {
        if let Some(w) = ui_weak.upgrade() {
            let cmd = if w.get_fan_curve_active() {
                ServiceCommand::StepFanCurve
            } else {
                ServiceCommand::Refresh
            };
            submit_background(&w, &service, &ops, &timer_for_tick, cmd);
        }
    });
}

fn wire_ui_callbacks(
    ui: &MainWindow,
    service: &SharedService,
    ops: &SharedOps,
    refresh_timer: &Rc<Timer>,
) {
    let trace_raw_usb = ops.borrow().diagnostics.trace_raw_usb();
    if ui.get_trace_raw_usb() != trace_raw_usb {
        ui.set_trace_raw_usb(trace_raw_usb);
        let snap = service.borrow().current_snapshot().clone();
        sync_snapshot_to_ui(ui, &snap, true);
    }

    // Every user action goes through `submit`, which surfaces enqueue rejection.
    let submit_cmd: Rc<dyn Fn(ServiceCommand) -> bool> = {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        let ops = Rc::clone(ops);
        Rc::new(move |cmd| {
            ui_weak
                .upgrade()
                .is_some_and(|w| submit(&w, &svc, &ops, cmd))
        })
    };

    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        ui.on_refresh_service_status(move || {
            if let Some(w) = ui_weak.upgrade() {
                start_service_job(&w, &ops, None);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        ui.on_install_service(move || {
            if let Some(w) = ui_weak.upgrade() {
                start_service_job(&w, &ops, Some(ServiceAction::Install));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        ui.on_update_service(move || {
            if let Some(w) = ui_weak.upgrade() {
                start_service_job(&w, &ops, Some(ServiceAction::Update));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        ui.on_setup_service(move || {
            if let Some(w) = ui_weak.upgrade() {
                start_service_job(&w, &ops, Some(ServiceAction::Setup));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        ui.on_uninstall_service(move || {
            if let Some(w) = ui_weak.upgrade() {
                let purge = w.get_service_remove_purge();
                start_service_job(&w, &ops, Some(ServiceAction::Uninstall { purge }));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_set_restore_at_startup(move |enabled| {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            if w.get_is_simulated() || w.get_service_busy() || !w.get_service_restore_available() {
                return;
            }
            // Keep the switch at the last confirmed value until the service replies.
            let confirmed = svc
                .borrow()
                .current_snapshot()
                .service_control
                .as_ref()
                .is_some_and(|control| control.restore_at_startup);
            w.set_service_restore_at_startup(confirmed);
            submit_cmd(ServiceCommand::SetRestoreAtStartup(enabled));
        });
    }

    {
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_request_refresh(move || {
            submit_cmd(ServiceCommand::Refresh);
        });
    }

    {
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_select_thermal_mode(move |mode_str| {
            if let Some(mode) = parse_mode_key(mode_str.as_str()) {
                submit_cmd(ServiceCommand::SetThermalMode(mode));
            }
        });
    }

    {
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_request_toggle_gmode(move || {
            submit_cmd(ServiceCommand::ToggleGMode);
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_stage_cpu_boost(move |val| {
            if let Some(w) = ui_weak.upgrade() {
                w.set_staged_cpu_boost(val.round());
                w.set_boost_dirty(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_stage_gpu_boost(move |val| {
            if let Some(w) = ui_weak.upgrade() {
                w.set_staged_gpu_boost(val.round());
                w.set_boost_dirty(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        ui.on_reset_staged_boost(move || {
            if let Some(w) = ui_weak.upgrade() {
                let snap = svc.borrow().current_snapshot().clone();
                sync_snapshot_to_ui(&w, &snap, true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_apply_staged_boost(move || {
            if let Some(w) = ui_weak.upgrade() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let cpu = w.get_staged_cpu_boost().round().clamp(0.0, 100.0) as u8;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let gpu = w.get_staged_gpu_boost().round().clamp(0.0, 100.0) as u8;
                // Staged edits are only discarded once the request was actually queued.
                if submit_cmd(ServiceCommand::ApplyManualBoost {
                    cpu_boost_pct: cpu,
                    gpu_boost_pct: gpu,
                }) {
                    w.set_boost_dirty(false);
                }
            }
        });
    }

    {
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_toggle_cpu_turbo(move |enabled| {
            submit_cmd(ServiceCommand::SetCpuTurbo(enabled));
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_stage_tcc_offset(move |val| {
            if let Some(w) = ui_weak.upgrade() {
                w.set_staged_tcc_offset(val.round());
                w.set_tcc_dirty(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_apply_staged_tcc(move || {
            if let Some(w) = ui_weak.upgrade() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let offset = w.get_staged_tcc_offset().round().clamp(0.0, 15.0) as u8;
                if submit_cmd(ServiceCommand::SetTccOffset(offset)) {
                    w.set_tcc_dirty(false);
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_stage_fan_curve(move |p45, p60, p75, p85| {
            if let Some(w) = ui_weak.upgrade() {
                let p1 = p45.clamp(0, 100);
                let p2 = p60.clamp(p1, 100);
                let p3 = p75.clamp(p2, 100);
                let p4 = p85.clamp(p3, 100);
                w.set_staged_curve_p1(p1);
                w.set_staged_curve_p2(p2);
                w.set_staged_curve_p3(p3);
                w.set_staged_curve_p4(p4);
                w.set_curve_dirty(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_apply_fan_curve(move || {
            if let Some(w) = ui_weak.upgrade() {
                let c1 = w.get_staged_curve_p1().clamp(0, 100);
                let c2 = w.get_staged_curve_p2().clamp(c1, 100);
                let c3 = w.get_staged_curve_p3().clamp(c2, 100);
                let c4 = w.get_staged_curve_p4().clamp(c3, 100);
                w.set_staged_curve_p1(c1);
                w.set_staged_curve_p2(c2);
                w.set_staged_curve_p3(c3);
                w.set_staged_curve_p4(c4);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let (p1, p2, p3, p4) = (c1 as u8, c2 as u8, c3 as u8, c4 as u8);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let hyst = w.get_staged_curve_hysteresis().round().clamp(1.0, 15.0) as u16;
                let pts = vec![
                    FanCurvePoint {
                        temp_c: 45,
                        boost_pct: p1,
                    },
                    FanCurvePoint {
                        temp_c: 60,
                        boost_pct: p2,
                    },
                    FanCurvePoint {
                        temp_c: 75,
                        boost_pct: p3,
                    },
                    FanCurvePoint {
                        temp_c: 85,
                        boost_pct: p4,
                    },
                ];
                let cfg = FanCurveConfig {
                    enabled: true,
                    hysteresis_c: hyst,
                    cpu_points: pts.clone(),
                    gpu_points: pts,
                };
                if submit_cmd(ServiceCommand::SetFanCurve(cfg)) {
                    w.set_curve_dirty(false);
                }
            }
        });
    }

    {
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_stop_fan_curve(move || {
            submit_cmd(ServiceCommand::SetFanCurve(FanCurveConfig::default()));
        });
    }

    {
        let ui_weak = ui.as_weak();
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_apply_staged_lighting(move || {
            if let Some(w) = ui_weak.upgrade() {
                let target = ZoneTarget::from_index(w.get_staged_zone_group_index());
                let color = color_preset_rgb(w.get_staged_color_preset_index());
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let duration_ms = w
                    .get_staged_lighting_duration_ms()
                    .round()
                    .clamp(50.0, 10_000.0) as u16;
                let effect =
                    LightingEffect::from_ui(w.get_staged_effect_index(), color, duration_ms);
                let persist_boot = w.get_staged_lighting_persist_boot();
                submit_cmd(ServiceCommand::ApplyLighting {
                    effect,
                    target,
                    persist_boot,
                });
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_apply_staged_brightness(move || {
            if let Some(w) = ui_weak.upgrade() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let pct = w.get_staged_lighting_brightness().round().clamp(0.0, 100.0) as u8;
                submit_cmd(ServiceCommand::SetBrightness(pct));
            }
        });
    }

    {
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_toggle_stealth_mode(move |enable| {
            submit_cmd(ServiceCommand::SetStealth(enable));
        });
    }

    {
        let ui_weak = ui.as_weak();
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_program_power_led_states(move || {
            if let Some(w) = ui_weak.upgrade() {
                let color = color_preset_rgb(w.get_staged_color_preset_index());
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let duration_ms = w
                    .get_staged_lighting_duration_ms()
                    .round()
                    .clamp(50.0, 10_000.0) as u16;
                let effect =
                    LightingEffect::from_ui(w.get_staged_effect_index(), color, duration_ms);
                submit_cmd(ServiceCommand::ApplyLighting {
                    effect,
                    target: ZoneTarget::PowerButton,
                    persist_boot: true,
                });
            }
        });
    }

    {
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_select_sim_scenario(move |idx| {
            let scenario = SimScenario::from_index(idx);
            submit_cmd(ServiceCommand::SetSimScenario(scenario));
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        let ops = Rc::clone(ops);
        let timer = Rc::clone(refresh_timer);
        ui.on_toggle_auto_refresh(move |enabled| {
            if let Some(w) = ui_weak.upgrade() {
                let interval_ms = w.get_refresh_interval_ms();
                restart_auto_refresh_timer(
                    &timer,
                    w.as_weak(),
                    Rc::clone(&svc),
                    Rc::clone(&ops),
                    enabled,
                    interval_ms,
                );
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        let ops = Rc::clone(ops);
        let timer = Rc::clone(refresh_timer);
        ui.on_select_refresh_interval(move |interval_ms| {
            if let Some(w) = ui_weak.upgrade() {
                w.set_refresh_interval_ms(interval_ms);
                w.set_refresh_interval_label(SharedString::from(interval_label_for_ms(
                    interval_ms,
                )));
                let enabled = w.get_auto_refresh_enabled();
                restart_auto_refresh_timer(
                    &timer,
                    w.as_weak(),
                    Rc::clone(&svc),
                    Rc::clone(&ops),
                    enabled,
                    interval_ms,
                );
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        ui.on_toggle_diagnostic_recording(move || {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            let diag = Arc::clone(&ops.borrow().diagnostics);
            if diag.is_recording_active() {
                diag.stop_recording();
                sync_recording_state_to_ui(&w, &diag);
                w.set_diagnostics_status_text(SharedString::from(
                    "Stopped temporary DEBUG recording — restored prior log filter.",
                ));
            } else {
                diag.start_recording(Duration::from_secs(300));
                sync_recording_state_to_ui(&w, &diag);
                w.set_diagnostics_status_text(SharedString::from(
                    "Temporary DEBUG recording active (5m timeout, 0 extra hardware polls).",
                ));
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        let ops = Rc::clone(ops);
        ui.on_copy_diagnostic_summary(move || {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            if ops.borrow().diagnostics_busy {
                return;
            }
            // Strictly passive: uses the cached snapshot already held in memory without
            // sending Refresh or touching hardware.
            let (snap, worker_alive) = {
                let handle = svc.borrow();
                (
                    handle.current_snapshot().clone(),
                    handle.worker_failure().is_none(),
                )
            };
            let gui_settings = collect_gui_settings(&w);
            let (diag, clipboard, tx, hook) = {
                let mut borrowed = ops.borrow_mut();
                borrowed.diagnostics_busy = true;
                (
                    Arc::clone(&borrowed.diagnostics),
                    Arc::clone(&borrowed.clipboard),
                    borrowed.diag_job_tx.clone(),
                    borrowed.diag_job_hook.clone(),
                )
            };
            w.set_diagnostics_busy(true);
            w.set_diagnostics_status_text(SharedString::from(
                "Assembling redacted diagnostic summary in background...",
            ));
            let spawn_res = std::thread::Builder::new()
                .name("awcc-diag-summary".into())
                .spawn(move || {
                    if let Some(h) = hook {
                        h();
                    }
                    let evidence = snap.to_session_evidence(
                        diag.session_id(),
                        BuildIdentityReport::current("awcc-gui"),
                        worker_alive,
                    );
                    let res = diag
                        .assemble_support_bundle(Some(evidence), gui_settings)
                        .map_err(|e| e.to_string())
                        .map(|bundle| {
                            let clip_res = match clipboard.lock() {
                                Ok(mut guard) => guard.set_text(&bundle.summary_txt),
                                Err(poisoned) => {
                                    poisoned.into_inner().set_text(&bundle.summary_txt)
                                }
                            };
                            (bundle.summary_txt, clip_res)
                        });
                    let _ = tx.send(DiagJobResult::Summary(res));
                    let _ = slint::invoke_from_event_loop(|| {});
                });
            if let Err(err) = spawn_res {
                ops.borrow_mut().diagnostics_busy = false;
                w.set_diagnostics_busy(false);
                w.set_diagnostics_status_text(SharedString::from(format!(
                    "Failed to start background summary worker: {err}"
                )));
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        let ops = Rc::clone(ops);
        ui.on_export_support_report(move || {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            if ops.borrow().diagnostics_busy {
                return;
            }
            // Strictly passive: uses the cached snapshot already held in memory without
            // sending Refresh or touching hardware.
            let (snap, worker_alive) = {
                let handle = svc.borrow();
                (
                    handle.current_snapshot().clone(),
                    handle.worker_failure().is_none(),
                )
            };
            let gui_settings = collect_gui_settings(&w);
            let (diag, override_path, tx, hook) = {
                let mut borrowed = ops.borrow_mut();
                borrowed.diagnostics_busy = true;
                (
                    Arc::clone(&borrowed.diagnostics),
                    borrowed.export_output_override.clone(),
                    borrowed.diag_job_tx.clone(),
                    borrowed.diag_job_hook.clone(),
                )
            };
            let out_path =
                override_path.unwrap_or_else(|| default_support_export_path(diag.session_id()));
            w.set_diagnostics_busy(true);
            w.set_diagnostics_status_text(SharedString::from(
                "Exporting passive support report in background...",
            ));
            let spawn_res = std::thread::Builder::new()
                .name("awcc-diag-export".into())
                .spawn(move || {
                    if let Some(h) = hook {
                        h();
                    }
                    let evidence = snap.to_session_evidence(
                        diag.session_id(),
                        BuildIdentityReport::current("awcc-gui"),
                        worker_alive,
                    );
                    let result = diag
                        .assemble_support_bundle(Some(evidence), gui_settings)
                        .and_then(|bundle| bundle.write_to_path(&out_path))
                        .map_err(|e| e.to_string());
                    let _ = tx.send(DiagJobResult::Export { out_path, result });
                    let _ = slint::invoke_from_event_loop(|| {});
                });
            if let Err(err) = spawn_res {
                ops.borrow_mut().diagnostics_busy = false;
                w.set_diagnostics_busy(false);
                w.set_diagnostics_status_text(SharedString::from(format!(
                    "Failed to start background export worker: {err}"
                )));
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        let submit_cmd = Rc::clone(&submit_cmd);
        ui.on_clear_audit_history(move || {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            ops.borrow().diagnostics.clear_visible_events();
            if submit_cmd(ServiceCommand::ClearAuditHistory) {
                w.set_selected_audit_seq(0);
                w.set_selected_audit_detail(SharedString::default());
                w.set_diagnostics_status_text(SharedString::from(
                    "Cleared visible history (lifetime counters preserved).",
                ));
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_select_audit_row(move |seq_num| {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            let rows = w.get_audit_rows();
            for idx in 0..rows.row_count() {
                if let Some(row) = rows.row_data(idx)
                    && row.seq_num == seq_num
                {
                    w.set_selected_audit_seq(seq_num);
                    w.set_selected_audit_detail(row.detail.clone());
                    w.set_diagnostics_status_text(SharedString::from(format!(
                        "Selected command #{seq_num:02} — click Copy Row to copy full payload."
                    )));
                    break;
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let ops = Rc::clone(ops);
        ui.on_copy_selected_audit_row(move || {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            let detail = w.get_selected_audit_detail();
            if !detail.is_empty() {
                let seq = w.get_selected_audit_seq();
                let clipboard = Arc::clone(&ops.borrow().clipboard);
                let clip_res = match clipboard.lock() {
                    Ok(mut guard) => guard.set_text(detail.as_str()),
                    Err(poisoned) => poisoned.into_inner().set_text(detail.as_str()),
                };
                w.set_last_copied_text(detail);
                match clip_res {
                    Ok(()) => {
                        w.set_clipboard_fallback_visible(false);
                        w.set_diagnostics_status_text(SharedString::from(format!(
                            "Copied full audit detail for command #{seq:02} to clipboard."
                        )));
                    }
                    Err(err) => {
                        w.set_clipboard_fallback_visible(true);
                        w.set_diagnostics_status_text(SharedString::from(format!(
                            "Clipboard unavailable ({err}) — detail for command #{seq:02} is ready in the read-only fallback below."
                        )));
                    }
                }
            }
        });
    }
}

/// Software-renderer platform for deterministic headless screenshot generation.
struct HeadlessSoftwarePlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl slint::platform::Platform for HeadlessSoftwarePlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn render_headless_screenshot(
    output_path: &Path,
    width: u32,
    height: u32,
    scenario: &str,
    sim_mode: bool,
) -> Result<()> {
    let sw_window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    sw_window.set_size(PhysicalSize::new(width, height));

    slint::platform::set_platform(Box::new(HeadlessSoftwarePlatform {
        window: sw_window.clone(),
    }))
    .map_err(|e| anyhow::anyhow!("failed to set headless Slint platform: {e}"))?;

    let mut orch = if !sim_mode && scenario == "normal" {
        match ThermalOrchestrator::new_hardware() {
            Ok(orch) => orch,
            Err(err) => {
                eprintln!(
                    "note: hardware backend unavailable ({err}); rendering the simulated backend instead"
                );
                ThermalOrchestrator::new_simulated_m15_r6()?
            }
        }
    } else {
        ThermalOrchestrator::new_simulated_m15_r6()?
    };

    let ui = MainWindow::new().context("failed to instantiate MainWindow")?;
    let snap = match scenario {
        "compact" => {
            let s = orch.query_snapshot();
            sync_snapshot_to_ui(&ui, &s, true);
            ui.set_staged_cpu_boost(72.0);
            ui.set_staged_gpu_boost(68.0);
            ui.set_boost_dirty(true);
            s
        }
        "rollback" => {
            orch.set_sim_scenario(SimScenario::AcpiSetFailureRollback);
            let s = orch.toggle_gmode();
            sync_snapshot_to_ui(&ui, &s, true);
            s
        }
        "unknown" => {
            let s = orch.set_sim_scenario(SimScenario::UnknownModeResponse);
            sync_snapshot_to_ui(&ui, &s, true);
            s
        }
        "lighting" => {
            let _ = orch.apply_lighting(
                LightingEffect::Static { color: 0xA855F7 },
                ZoneTarget::Keyboard,
                false,
            );
            let s = orch.apply_lighting(
                LightingEffect::Static { color: 0x00F0FF },
                ZoneTarget::Logo,
                true,
            );
            sync_snapshot_to_ui(&ui, &s, true);
            ui.set_active_nav_index(1);
            ui.set_staged_zone_group_index(1);
            ui.set_staged_effect_index(1);
            ui.set_staged_color_preset_index(1);
            s
        }
        "diagnostics" | "log" => {
            orch.set_sim_scenario(SimScenario::AcpiSetFailureRollback);
            let _ = orch.toggle_gmode();
            orch.set_sim_scenario(SimScenario::Normal);
            let _ = orch.set_thermal_mode(ThermalMode::Performance);
            let s = orch.apply_lighting(
                LightingEffect::Static { color: 0x00F0FF },
                ZoneTarget::Keyboard,
                false,
            );
            sync_snapshot_to_ui(&ui, &s, true);
            ui.set_active_nav_index(2);
            let rows = ui.get_audit_rows();
            for idx in 0..rows.row_count() {
                if let Some(row) = rows.row_data(idx)
                    && row.seq_num == 2
                {
                    ui.set_selected_audit_seq(2);
                    ui.set_selected_audit_detail(row.detail.clone());
                    break;
                }
            }
            s
        }
        "settings" => {
            let s = orch.query_snapshot();
            sync_snapshot_to_ui(&ui, &s, true);
            ui.set_active_nav_index(3);
            s
        }
        _ => {
            let s = orch.query_snapshot();
            sync_snapshot_to_ui(&ui, &s, true);
            s
        }
    };
    let _ = snap;
    disable_service_management_ui(
        &ui,
        "Screenshot preview",
        "Service management is disabled while rendering a screenshot.",
    );

    ui.show().context("failed to show headless MainWindow")?;
    sw_window.set_size(PhysicalSize::new(width, height));
    slint::platform::update_timers_and_animations();
    sw_window.request_redraw();

    let pixel_count = (width as usize) * (height as usize);
    let mut pixel_buf = vec![PremultipliedRgbaColor::default(); pixel_count];

    let drew = sw_window.draw_if_needed(|renderer| {
        renderer.render(&mut pixel_buf, width as usize);
    });
    if !drew {
        bail!("Slint software renderer did not draw frame");
    }

    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(output_path)
        .with_context(|| format!("failed to create screenshot file {}", output_path.display()))?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder.write_header()?;

    let mut rgba_bytes = Vec::with_capacity(pixel_count * 4);
    for px in &pixel_buf {
        rgba_bytes.push(px.red);
        rgba_bytes.push(px.green);
        rgba_bytes.push(px.blue);
        rgba_bytes.push(px.alpha);
    }
    png_writer.write_image_data(&rgba_bytes)?;
    println!(
        "Rendered screenshot ({width}x{height}, scenario={scenario}) -> {}",
        output_path.display()
    );
    Ok(())
}

/// The leaf actions of the GUI binary. Injectable so tests can prove that `--help` and
/// `--version` reach none of them (no profile resolution, no orchestrator, no service worker,
/// no Slint window).
trait GuiRuntime {
    fn device_info(&mut self, sim_mode: bool) -> Result<()>;
    fn query_mode(&mut self, sim_mode: bool) -> Result<()>;
    fn screenshot(&mut self, path: &Path, opts: &CliOptions) -> Result<()>;
    fn interactive(&mut self, opts: &CliOptions) -> Result<()>;
}

struct RealRuntime;

impl GuiRuntime for RealRuntime {
    fn device_info(&mut self, sim_mode: bool) -> Result<()> {
        run_device_info(sim_mode)
    }

    fn query_mode(&mut self, sim_mode: bool) -> Result<()> {
        run_query_mode(sim_mode)
    }

    fn screenshot(&mut self, path: &Path, opts: &CliOptions) -> Result<()> {
        render_headless_screenshot(path, opts.width, opts.height, &opts.scenario, opts.sim_mode)
    }

    fn interactive(&mut self, opts: &CliOptions) -> Result<()> {
        run_interactive(opts.sim_mode)
    }
}

/// Route the parsed options. `--help`/`--version` are answered here, before any runtime action.
fn dispatch(opts: &CliOptions, out: &mut dyn Write, rt: &mut dyn GuiRuntime) -> Result<()> {
    if opts.show_help {
        writeln!(out, "{}", help_text()).context("failed to write help text")?;
        return Ok(());
    }
    if opts.show_version {
        writeln!(out, "{}", awcc_build_info::version_text("awcc-gui"))
            .context("failed to write version text")?;
        return Ok(());
    }
    if opts.device_info {
        return rt.device_info(opts.sim_mode);
    }
    if opts.query_mode {
        return rt.query_mode(opts.sim_mode);
    }
    if let Some(screenshot_path) = &opts.screenshot_path {
        return rt.screenshot(screenshot_path, opts);
    }
    rt.interactive(opts)
}

fn run_interactive(sim_mode: bool) -> Result<()> {
    let orchestrator = if sim_mode {
        ThermalOrchestrator::new_simulated_m15_r6()?
    } else {
        ThermalOrchestrator::new_hardware()?
    };
    let service = Rc::new(RefCell::new(
        ThermalServiceHandle::spawn(orchestrator).context("failed to start the service")?,
    ));
    let ops: SharedOps = Rc::new(RefCell::new(UiOps::default()));

    let ui = MainWindow::new().context("failed to create Slint MainWindow")?;
    {
        let initial_snap = service.borrow().current_snapshot().clone();
        sync_snapshot_to_ui(&ui, &initial_snap, true);
    }
    let refresh_timer = Rc::new(Timer::default());
    wire_ui_callbacks(&ui, &service, &ops, &refresh_timer);
    if !sim_mode {
        start_service_job(&ui, &ops, None);
    }

    // Start periodic auto-refresh timer (default 1000ms = 1.0s, configurable in Settings).
    restart_auto_refresh_timer(
        &refresh_timer,
        ui.as_weak(),
        Rc::clone(&service),
        Rc::clone(&ops),
        ui.get_auto_refresh_enabled(),
        ui.get_refresh_interval_ms(),
    );

    // Non-blocking 50ms poll timer to apply newly published worker snapshots on the UI thread
    // and to notice a worker that stopped.
    let poll_timer = Timer::default();
    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(&service);
        let ops = Rc::clone(&ops);
        let refresh_timer = Rc::clone(&refresh_timer);
        poll_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(w) = ui_weak.upgrade() else {
                return;
            };
            if let Some(snap) = svc.borrow_mut().poll_latest() {
                apply_polled_snapshot(&w, snap, &ops);
            } else {
                let diag = Arc::clone(&ops.borrow().diagnostics);
                sync_recording_state_to_ui(&w, &diag);
                poll_diagnostics_jobs(&w, &ops);
            }
            poll_service_jobs(&w, &ops);
            report_worker_failure_once(&w, &svc, &ops, &refresh_timer);
        });
    }

    ui.run().context("Slint event loop exited with error")?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    let result = parse_cli_args(std::env::args().skip(1)).and_then(|opts| {
        if !opts.show_help && !opts.show_version {
            let resolved_level = resolve_log_level(
                opts.log_level.as_deref(),
                opts.verbose,
                std::env::var("AWCC_LOG").ok().as_deref(),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            let trace_raw_usb = resolve_raw_usb_opt_in(
                opts.trace_raw_usb,
                std::env::var("AWCC_TRACE_RAW_USB").ok().as_deref(),
            );
            let cfg =
                DiagnosticsConfig::from_env_and_cli("awcc-gui", resolved_level, trace_raw_usb);
            let runtime = init_global_diagnostics(cfg);
            install_panic_hook(runtime);
        }
        let mut stdout = std::io::stdout().lock();
        dispatch(&opts, &mut stdout, &mut RealRuntime)
    });
    if let Some(diag) = global_diagnostics() {
        diag.shutdown(Duration::from_millis(250));
    }
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeClipboardState {
        history: Vec<String>,
        fail_with: Option<String>,
    }

    #[derive(Clone, Default)]
    struct FakeClipboard {
        state: Arc<std::sync::Mutex<FakeClipboardState>>,
    }

    impl FakeClipboard {
        fn last_copied(&self) -> Option<String> {
            self.state.lock().unwrap().history.last().cloned()
        }

        fn copy_count(&self) -> usize {
            self.state.lock().unwrap().history.len()
        }

        fn set_failure(&self, err: Option<&str>) {
            self.state.lock().unwrap().fail_with = err.map(str::to_string);
        }
    }

    impl ClipboardBackend for FakeClipboard {
        fn set_text(&mut self, text: &str) -> Result<(), String> {
            let mut guard = self.state.lock().unwrap();
            if let Some(err) = &guard.fail_with {
                return Err(err.clone());
            }
            guard.history.push(text.to_string());
            Ok(())
        }
    }

    fn wait_for_diagnostics_job(ui: &MainWindow, ops: &SharedOps) {
        for _ in 0..400 {
            if poll_diagnostics_jobs(ui, ops) && !ops.borrow().diagnostics_busy {
                return;
            }
            if !ops.borrow().diagnostics_busy {
                poll_diagnostics_jobs(ui, ops);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for background diagnostics job to complete");
    }

    #[test]
    fn cli_parser_handles_passive_commands_and_screenshot_flags() {
        let opts = parse_cli_args(vec![
            "--screenshot".to_string(),
            "/tmp/out.png".to_string(),
            "--width".to_string(),
            "880".to_string(),
            "--height".to_string(),
            "620".to_string(),
            "--scenario".to_string(),
            "lighting".to_string(),
        ])
        .expect("valid screenshot args should parse");

        assert_eq!(opts.screenshot_path, Some(PathBuf::from("/tmp/out.png")));
        assert_eq!(opts.width, 880);
        assert_eq!(opts.height, 620);
        assert_eq!(opts.scenario, "lighting");
    }

    #[test]
    fn format_log_entry_strips_pkexec_and_includes_meaning_and_hex() {
        let pkexec_entry = WriteAuditEntry {
            sequence: 1,
            op_id: None,
            command: "pkexec sh -c 'echo \"\\_SB.AMWW.WMAX 0 0x15 {0x1,0xa3,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'".to_string(),
            outcome: "0x0".to_string(),
            succeeded: true,
            ack: awcc_platform::AckStatus::NotApplicable,
            raw_usb_response_hex: None,
        };
        let (cmd, out) = format_log_entry(&pkexec_entry, None);
        assert!(!cmd.contains("pkexec"));
        assert!(cmd.contains("Set Thermal Profile → Performance"));
        assert!(cmd.contains("WMAX 0x15 [0x01, 0xA3, 0x00, 0x00]"));
        assert_eq!(out, "OK (0x00)");

        let usb_entry = WriteAuditEntry {
            sequence: 2,
            op_id: Some(7),
            command: "USB HID 187c:0550 OUT [03 24 02 07 d0 00 fa f5 9e 0b]".to_string(),
            outcome: "33 bytes transferred".to_string(),
            succeeded: true,
            ack: awcc_platform::AckStatus::Received,
            raw_usb_response_hex: None,
        };
        let (usb_cmd, usb_out) = format_log_entry(&usb_entry, None);
        assert!(usb_cmd.contains("Set Lighting Keyframe → Static Color #F59E0B (2000 ms)"));
        assert!(usb_cmd.contains("ELC [03 24 02 07 D0 00 FA F5 9E 0B]"));
        assert_eq!(usb_out, "Sent (33B) · ACK read");

        // A completed transfer whose acknowledgement failed must not read as acknowledged.
        let unacked = WriteAuditEntry {
            ack: awcc_platform::AckStatus::Failed {
                reason: "timed out".to_string(),
            },
            ..usb_entry
        };
        let (_, unacked_out) = format_log_entry(&unacked, None);
        assert_eq!(unacked_out, "Sent (33B) · ACK read failed: timed out");
        assert!(!unacked_out.contains("OK"));
    }

    #[test]
    fn ui_staging_never_emits_hardware_writes_until_apply_is_invoked() {
        i_slint_backend_testing::init_no_event_loop();
        let orchestrator =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");
        let service = Rc::new(RefCell::new(
            ThermalServiceHandle::spawn(orchestrator).expect("service worker should start"),
        ));
        let ops: SharedOps = Rc::new(RefCell::new(UiOps::default()));
        let ui = MainWindow::new().expect("MainWindow should instantiate under testing backend");

        let initial_snap = service.borrow().current_snapshot().clone();
        sync_snapshot_to_ui(&ui, &initial_snap, true);
        let refresh_timer = Rc::new(Timer::default());
        wire_ui_callbacks(&ui, &service, &ops, &refresh_timer);

        assert_eq!(ui.get_write_count(), 0);
        assert_eq!(ui.get_active_mode_label().as_str(), "Balanced");
        assert!(!ui.get_boost_dirty());
        assert!(!ui.get_tcc_dirty());
        assert!(ui.get_auto_refresh_enabled());
        assert_eq!(ui.get_refresh_interval_ms(), 1000);

        // Changing refresh interval in Settings updates UI state without hardware writes:
        ui.invoke_select_refresh_interval(500);
        assert_eq!(ui.get_refresh_interval_ms(), 500);
        assert_eq!(ui.get_refresh_interval_label().as_str(), "0.5s");
        assert_eq!(ui.get_write_count(), 0);

        // Programmatically syncing a snapshot where G-Mode, Turbo, and Stealth flip must
        // NEVER fire `Switch.checked_state_changed` or `Slider.value_changed` feedback loops:
        let mut flipped_snap = initial_snap;
        flipped_snap.game_shift = Observation::Observed(true);
        flipped_snap.thermal_mode_state = ThermalModeState::Known(ThermalMode::GMode);
        flipped_snap.cpu_turbo = Observation::Observed(false);
        flipped_snap.lighting_stealth_enabled = true;
        sync_snapshot_to_ui(&ui, &flipped_snap, true);
        assert!(ui.get_gmode_active());
        assert!(!ui.get_cpu_turbo_enabled());
        assert!(ui.get_lighting_stealth_enabled());
        assert!(!ui.get_boost_dirty());
        assert!(!ui.get_tcc_dirty());

        // User drags CPU/GPU boost, TCC offset, and FanCurveEditor handles without clicking Apply:
        ui.invoke_stage_cpu_boost(80.0);
        ui.invoke_stage_gpu_boost(70.0);
        ui.invoke_stage_tcc_offset(10.0);
        ui.invoke_stage_fan_curve(15, 45, 75, 100);
        assert!(ui.get_boost_dirty());
        assert!(ui.get_tcc_dirty());
        assert!(ui.get_curve_dirty());
        assert_eq!(ui.get_staged_curve_p1(), 15);
        assert_eq!(ui.get_staged_curve_p2(), 45);
        assert_eq!(ui.get_staged_curve_p3(), 75);
        assert_eq!(ui.get_staged_curve_p4(), 100);

        // Monotonic non-decreasing invariant: dips below preceding points are clamped out:
        ui.invoke_stage_fan_curve(30, 20, 70, 65);
        assert_eq!(ui.get_staged_curve_p1(), 30);
        assert_eq!(ui.get_staged_curve_p2(), 30);
        assert_eq!(ui.get_staged_curve_p3(), 70);
        assert_eq!(ui.get_staged_curve_p4(), 70);
        assert_eq!(ui.get_write_count(), 0);
    }
    // --- `--help` / `--version` must not initialise anything ---------------------------------

    #[derive(Default)]
    struct SpyRuntime {
        calls: Vec<&'static str>,
    }

    impl GuiRuntime for SpyRuntime {
        fn device_info(&mut self, _sim_mode: bool) -> Result<()> {
            self.calls.push("device_info");
            Ok(())
        }
        fn query_mode(&mut self, _sim_mode: bool) -> Result<()> {
            self.calls.push("query_mode");
            Ok(())
        }
        fn screenshot(&mut self, _path: &Path, _opts: &CliOptions) -> Result<()> {
            self.calls.push("screenshot");
            Ok(())
        }
        fn interactive(&mut self, _opts: &CliOptions) -> Result<()> {
            self.calls.push("interactive");
            Ok(())
        }
    }

    fn dispatch_args(args: &[&str]) -> (Vec<&'static str>, String) {
        let opts = parse_cli_args(args.iter().map(|a| (*a).to_string())).expect("args parse");
        let mut rt = SpyRuntime::default();
        let mut out = Vec::new();
        dispatch(&opts, &mut out, &mut rt).expect("dispatch succeeds");
        (rt.calls, String::from_utf8(out).unwrap())
    }

    #[test]
    fn help_and_version_never_reach_any_runtime_action() {
        for args in [
            &["--version"][..],
            &["-V"],
            &["--sim", "--version"],
            &["--version", "device-info"],
            &["--version", "--screenshot", "/tmp/never.png"],
            &["--help"],
            &["-h"],
            &["help", "query-mode"],
        ] {
            let (calls, out) = dispatch_args(args);
            assert!(calls.is_empty(), "{args:?} reached {calls:?}");
            assert!(!out.is_empty(), "{args:?} printed nothing");
        }
        let (_, version) = dispatch_args(&["--version"]);
        assert!(version.starts_with("awcc-gui "), "{version}");
        assert!(version.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))));
        let (_, help) = dispatch_args(&["--help"]);
        assert!(help.contains("--version"));

        // Positive controls: the spy really observes the runtime being reached.
        assert_eq!(dispatch_args(&["device-info"]).0, ["device_info"]);
        assert_eq!(dispatch_args(&["--query-mode"]).0, ["query_mode"]);
        assert_eq!(
            dispatch_args(&["--screenshot", "/tmp/x.png"]).0,
            ["screenshot"]
        );
        assert_eq!(dispatch_args(&["--sim"]).0, ["interactive"]);
    }

    // --- Unavailable / stale presentation ---------------------------------------------------

    fn healthy_snapshot() -> SystemSnapshot {
        ThermalOrchestrator::new_simulated_m15_r6()
            .expect("sim")
            .query_snapshot()
    }

    #[test]
    fn missing_firmware_diagnostics_never_show_a_plausible_default() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let healthy = healthy_snapshot();
        let good = healthy
            .elc_diagnostics
            .fresh()
            .cloned()
            .expect("sim ELC report");

        let mut snap = healthy.clone();
        sync_snapshot_to_ui(&ui, &snap, true);
        assert!(ui.get_elc_firmware_summary().contains("FW 2.0.5"));

        snap.elc_diagnostics = Observation::NotObserved;
        sync_snapshot_to_ui(&ui, &snap, true);
        let text = ui.get_elc_firmware_summary();
        assert!(text.contains("not yet observed"), "{text}");
        assert!(
            !text.contains("FW "),
            "stale firmware text must not survive: {text}"
        );

        snap.elc_diagnostics = Observation::Unavailable {
            reason: "USB read timeout".to_string(),
        };
        sync_snapshot_to_ui(&ui, &snap, true);
        let text = ui.get_elc_firmware_summary();
        assert!(text.contains("unavailable: USB read timeout"), "{text}");
        assert!(!text.contains("FW "), "{text}");

        snap.elc_diagnostics = Observation::Stale {
            last_value: good,
            age_secs: 30,
            reason: "USB read timeout".to_string(),
        };
        sync_snapshot_to_ui(&ui, &snap, true);
        let text = ui.get_elc_firmware_summary();
        assert!(text.contains("STALE"), "{text}");
        assert!(text.contains("30s ago"), "{text}");
        assert!(text.contains("USB read timeout"), "{text}");
    }

    #[test]
    fn turbo_switch_is_disabled_and_explained_when_the_state_is_not_current() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let mut snap = healthy_snapshot();

        snap.cpu_turbo = Observation::Observed(true);
        sync_snapshot_to_ui(&ui, &snap, true);
        assert!(ui.get_cpu_turbo_known());
        assert!(ui.get_cpu_turbo_enabled());
        assert!(ui.get_cpu_turbo_note().is_empty());

        snap.cpu_turbo = Observation::Unavailable {
            reason: "no readable CPU turbo control file".to_string(),
        };
        sync_snapshot_to_ui(&ui, &snap, true);
        assert!(!ui.get_cpu_turbo_known());
        assert!(!ui.get_cpu_turbo_enabled(), "a failed query is not 'on'");
        assert!(ui.get_cpu_turbo_note().contains("state unavailable"));

        snap.cpu_turbo = Observation::Stale {
            last_value: true,
            age_secs: 7,
            reason: "sysfs read failed".to_string(),
        };
        sync_snapshot_to_ui(&ui, &snap, true);
        assert!(!ui.get_cpu_turbo_known(), "stale state is not interactive");
        assert!(ui.get_cpu_turbo_note().contains("stale 7s"));
        assert!(ui.get_cpu_turbo_note().contains("sysfs read failed"));
    }

    #[test]
    fn stale_telemetry_and_assumed_lighting_state_are_labelled() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let mut snap = healthy_snapshot();
        snap.telemetry.cpu_temp_c = awcc_core::SensorReading::Stale {
            last_value: 58,
            unit: "°C",
            age_secs: 9,
            reason: "ACPI WMAX 0x14 sensor query returned 0xFFFFFFFF".to_string(),
        };
        snap.lighting_state_source = LightingStateSource::AssumedDefault;
        sync_snapshot_to_ui(&ui, &snap, true);
        assert_eq!(ui.get_cpu_temp_text().as_str(), "58°C (stale 9s)");
        assert!(!ui.get_sensors_available());
        let notice = ui.get_telemetry_notice();
        assert!(notice.contains("CPU temp"), "{notice}");
        assert!(notice.contains("0xFFFFFFFF"), "{notice}");
        assert!(
            ui.get_active_lighting_summary()
                .contains("assumed defaults")
        );
        assert_eq!(ui.get_zone_logo_provenance().as_str(), "Assumed default");
        assert_eq!(ui.get_zone_tron_provenance().as_str(), "Assumed default");
        assert_eq!(ui.get_zone_kbd_provenance().as_str(), "Assumed default");
        assert_eq!(ui.get_zone_power_provenance().as_str(), "Assumed default");
        assert!(ui.get_build_summary().starts_with("Build: awcc-gui "));

        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");
        let after_dim = orch.set_brightness(55);
        sync_snapshot_to_ui(&ui, &after_dim, true);
        assert!(
            ui.get_active_lighting_summary()
                .contains("zone swatches show assumed defaults"),
            "summary: {}",
            ui.get_active_lighting_summary()
        );
        assert_eq!(ui.get_zone_logo_provenance().as_str(), "Assumed default");
        assert_eq!(ui.get_zone_kbd_provenance().as_str(), "Assumed default");

        let after_kbd = orch.apply_lighting(
            LightingEffect::Static { color: 0xA855F7 },
            ZoneTarget::Keyboard,
            false,
        );
        sync_snapshot_to_ui(&ui, &after_kbd, true);
        assert_eq!(
            ui.get_zone_kbd_provenance().as_str(),
            "Remembered (commanded)"
        );
        assert_eq!(ui.get_zone_logo_provenance().as_str(), "Assumed default");
        assert_eq!(ui.get_zone_tron_provenance().as_str(), "Assumed default");
        assert_eq!(ui.get_zone_power_provenance().as_str(), "Assumed default");
        assert!(
            ui.get_active_lighting_summary()
                .contains("remembered for commanded zones; others show assumed defaults"),
            "summary: {}",
            ui.get_active_lighting_summary()
        );

        let after_all = orch.apply_lighting(
            LightingEffect::Static { color: 0x00F0FF },
            ZoneTarget::All,
            true,
        );
        sync_snapshot_to_ui(&ui, &after_all, true);
        assert_eq!(
            ui.get_zone_logo_provenance().as_str(),
            "Remembered (commanded)"
        );
        assert_eq!(
            ui.get_zone_tron_provenance().as_str(),
            "Remembered (commanded)"
        );
        assert_eq!(
            ui.get_zone_kbd_provenance().as_str(),
            "Remembered (commanded)"
        );
        assert_eq!(
            ui.get_zone_power_provenance().as_str(),
            "Remembered (commanded)"
        );
        assert!(
            ui.get_active_lighting_summary()
                .contains("remembered from last command (no ELC colour readback)"),
            "summary: {}",
            ui.get_active_lighting_summary()
        );

        let healthy = healthy_snapshot();
        sync_snapshot_to_ui(&ui, &healthy, true);
        assert!(ui.get_telemetry_notice().is_empty());
    }

    // --- Enqueue rejection / worker failure -------------------------------------------------

    fn gated_service(capacity: usize) -> (SharedService, std::sync::mpsc::Sender<()>) {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let orchestrator = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");
        let handle = ThermalServiceHandle::spawn_with(orchestrator, capacity, move |body| {
            std::thread::Builder::new().spawn(move || {
                let _ = release_rx.recv();
                body();
            })
        })
        .expect("gated worker");
        (Rc::new(RefCell::new(handle)), release_tx)
    }

    fn wired_ui(service: &SharedService) -> (MainWindow, SharedOps, Rc<Timer>) {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let default_ops = UiOps {
            clipboard: Arc::new(std::sync::Mutex::new(Box::new(FakeClipboard::default()))),
            ..Default::default()
        };
        let ops: SharedOps = Rc::new(RefCell::new(default_ops));
        let timer = Rc::new(Timer::default());
        let snap = service.borrow().current_snapshot().clone();
        sync_snapshot_to_ui(&ui, &snap, true);
        wire_ui_callbacks(&ui, service, &ops, &timer);
        (ui, ops, timer)
    }

    fn service_inspection(ownership: Ownership, state: UnitState) -> ServiceInspection {
        ServiceInspection {
            installation: ServiceStatusReport {
                managed_by: ownership.managed_by().into(),
                ownership,
                unit_state: state,
                enabled: (state != UnitState::Missing).then_some(true),
                masked: state == UnitState::Masked,
                installed_build_id: None,
                version: None,
                unit_path: None,
                dropins: Vec::new(),
                interrupted_operation: false,
                problems: Vec::new(),
            },
            daemon: if state == UnitState::Running {
                ServiceProbe::Running {
                    daemon_protocol: 1,
                    version: env!("CARGO_PKG_VERSION").into(),
                    commit: "test".into(),
                    package_format: "appimage".into(),
                    status: awcc_service::ipc::DaemonStatus {
                        uptime_secs: 2,
                        device_name: "Test device".into(),
                        acpi_prefix: "TEST".into(),
                        control_running: true,
                    },
                    capabilities: Vec::new(),
                }
            } else {
                ServiceProbe::NotRunning {
                    reason: "No socket".into(),
                }
            },
        }
    }

    struct FakeServiceManagement {
        inspection: std::sync::Mutex<ServiceInspection>,
        actions: std::sync::Mutex<Vec<ServiceAction>>,
        inspections: std::sync::atomic::AtomicUsize,
        failure: std::sync::Mutex<Option<String>>,
        entered: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl Default for FakeServiceManagement {
        fn default() -> Self {
            Self {
                inspection: std::sync::Mutex::new(service_inspection(
                    Ownership::None,
                    UnitState::Missing,
                )),
                actions: std::sync::Mutex::new(Vec::new()),
                inspections: std::sync::atomic::AtomicUsize::new(0),
                failure: std::sync::Mutex::new(None),
                entered: None,
                release: std::sync::Mutex::new(None),
            }
        }
    }

    impl ServiceManagementBackend for FakeServiceManagement {
        fn inspect(&self) -> ServiceInspection {
            self.inspections
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inspection.lock().unwrap().clone()
        }

        fn apply(
            &self,
            action: ServiceAction,
            progress: &mut dyn FnMut(Progress),
        ) -> Result<ApplyReport, String> {
            self.actions.lock().unwrap().push(action);
            progress(Progress {
                step: installer::ProgressStep::Staging,
                message: "Staging the service".into(),
            });
            if let Some(entered) = &self.entered {
                let _ = entered.send(());
            }
            if let Some(release) = self.release.lock().unwrap().take() {
                release.recv().unwrap();
            }
            if let Some(failure) = self.failure.lock().unwrap().clone() {
                return Err(failure);
            }
            let outcome = match action {
                ServiceAction::Install => ApplyOutcome::Installed,
                ServiceAction::Update => ApplyOutcome::Updated,
                ServiceAction::Setup => ApplyOutcome::Configured,
                ServiceAction::Uninstall { .. } => ApplyOutcome::Uninstalled,
            };
            let previous_owner = self
                .inspection
                .lock()
                .unwrap()
                .installation
                .ownership
                .clone();
            *self.inspection.lock().unwrap() = if matches!(action, ServiceAction::Uninstall { .. })
            {
                service_inspection(Ownership::None, UnitState::Missing)
            } else if action == ServiceAction::Setup {
                service_inspection(previous_owner, UnitState::Running)
            } else {
                service_inspection(
                    Ownership::AppimageInstaller {
                        build_id: "test-build".into(),
                    },
                    UnitState::Running,
                )
            };
            Ok(ApplyReport {
                action: action.name().into(),
                outcome,
                build_id: Some("test-build".into()),
                previous_build_id: None,
                enabled: Some(true),
                active: Some(true),
                recovered_interrupted: false,
                purged: matches!(action, ServiceAction::Uninstall { purge: true }),
                notes: Vec::new(),
            })
        }
    }

    fn wait_for_service_job(ui: &MainWindow, ops: &SharedOps) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while ops.borrow().service_busy {
            poll_service_jobs(ui, ops);
            assert!(
                std::time::Instant::now() < deadline,
                "service task timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn service_status_buttons_follow_installation_ownership_and_daemon_health() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let absent = service_inspection(Ownership::None, UnitState::Missing);
        sync_service_inspection_to_ui(&ui, &absent);
        assert_eq!(ui.get_service_state_label(), "Not installed");
        assert!(ui.get_service_can_install());
        assert!(!ui.get_service_can_update());
        assert!(!ui.get_service_can_remove());

        let native = service_inspection(
            Ownership::NativePackage {
                manager: "dpkg".into(),
                package: "awcc-rs".into(),
                unit_path: "/usr/lib/systemd/system/awccd.service".into(),
            },
            UnitState::Running,
        );
        sync_service_inspection_to_ui(&ui, &native);
        assert_eq!(ui.get_service_managed_by(), "native-package");
        assert!(!ui.get_service_can_install());
        assert!(!ui.get_service_can_setup());
        assert!(!ui.get_service_can_update());
        assert!(!ui.get_service_can_remove());
        assert!(ui.get_service_state_detail().contains("package manager"));

        let mut ours = service_inspection(
            Ownership::AppimageInstaller {
                build_id: "test-build".into(),
            },
            UnitState::Running,
        );
        sync_service_inspection_to_ui(&ui, &ours);
        assert_eq!(ui.get_service_state_label(), "Running");
        assert!(ui.get_service_can_update());
        assert!(ui.get_service_can_remove());
        assert!(!ui.get_service_state_is_problem());

        ours.daemon = ServiceProbe::AccessDenied {
            message: "User is not authorized".into(),
        };
        sync_service_inspection_to_ui(&ui, &ours);
        assert_eq!(ui.get_service_state_label(), "Access denied");
        assert!(ui.get_service_state_detail().contains("Set up service"));
        assert!(ui.get_service_can_setup());
        assert!(
            ui.get_service_can_update(),
            "ownership is independent of IPC access"
        );
        assert!(ui.get_service_state_is_problem());
        ours.daemon = ServiceProbe::Incompatible {
            message: "Upgrade the service protocol".into(),
        };
        sync_service_inspection_to_ui(&ui, &ours);
        assert_eq!(ui.get_service_state_label(), "Incompatible service");
        assert!(
            ui.get_service_state_detail()
                .contains("Upgrade the service protocol")
        );
        ours.daemon = ServiceProbe::NotRunning {
            reason: "Connection refused".into(),
        };
        sync_service_inspection_to_ui(&ui, &ours);
        assert_eq!(ui.get_service_state_label(), "Unreachable");

        ours.installation.unit_state = UnitState::Unknown;
        ours.installation.problems.push("systemctl failed".into());
        sync_service_inspection_to_ui(&ui, &ours);
        assert!(!ui.get_service_can_update());
        assert!(!ui.get_service_can_remove());
        assert!(ui.get_service_state_detail().contains("systemctl failed"));
        assert!(!ui.get_service_can_setup());
    }

    #[test]
    fn simulation_never_runs_service_status_installation_or_restore_callbacks() {
        let (service, release) = gated_service(1);
        let (ui, ops, _timer) = wired_ui(&service);
        let backend = Arc::new(FakeServiceManagement::default());
        ops.borrow_mut().service_backend = backend.clone();
        assert_eq!(ui.get_service_state_label(), "Simulation");
        // Even a stale/programmatically altered button state cannot bypass simulation mode.
        ui.set_service_can_install(true);
        ui.set_service_can_setup(true);
        ui.set_service_can_update(true);
        ui.set_service_can_remove(true);
        ui.set_service_confirming_install(true);
        ui.set_service_confirming_remove(true);
        ui.set_service_restore_available(true);
        ui.invoke_refresh_service_status();
        ui.invoke_install_service();
        ui.invoke_setup_service();
        ui.invoke_update_service();
        ui.invoke_uninstall_service();
        ui.invoke_set_restore_at_startup(true);
        assert!(!ops.borrow().service_busy);
        assert!(ops.borrow().pending.is_none());
        assert!(backend.actions.lock().unwrap().is_empty());
        assert_eq!(
            backend
                .inspections
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        release.send(()).unwrap();
    }

    #[test]
    fn service_lifecycle_callbacks_require_confirmation_run_async_and_surface_results() {
        let (service, release_service) = gated_service(1);
        let (ui, ops, _timer) = wired_ui(&service);
        ui.set_is_simulated(false);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let backend = Arc::new(FakeServiceManagement {
            entered: Some(entered_tx),
            release: std::sync::Mutex::new(Some(release_rx)),
            failure: std::sync::Mutex::new(Some(
                "Administrator authorization was cancelled".into(),
            )),
            ..Default::default()
        });
        ops.borrow_mut().service_backend = backend.clone();

        ui.invoke_refresh_service_status();
        wait_for_service_job(&ui, &ops);
        assert!(ui.get_service_can_install());
        ui.invoke_install_service();
        assert!(
            !ui.get_service_busy(),
            "install requires the confirmation step"
        );
        assert!(backend.actions.lock().unwrap().is_empty());
        ui.set_service_confirming_install(true);
        ui.invoke_install_service();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(ui.get_service_busy());
        assert!(!ui.get_service_confirming_install());
        poll_service_jobs(&ui, &ops);
        assert_eq!(ui.get_service_progress(), "Staging the service");
        ui.invoke_stage_cpu_boost(77.0);
        assert_eq!(ui.get_staged_cpu_boost(), 77.0, "the UI remains responsive");
        ui.invoke_install_service();
        ui.invoke_update_service();
        ui.invoke_refresh_service_status();
        assert_eq!(*backend.actions.lock().unwrap(), [ServiceAction::Install]);

        release_tx.send(()).unwrap();
        wait_for_service_job(&ui, &ops);
        assert!(ui.get_service_result_is_error());
        assert!(ui.get_service_result().contains("cancelled"));
        assert!(ui.get_service_can_install());
        assert!(!ui.get_service_busy());

        *backend.failure.lock().unwrap() = None;
        ui.set_service_confirming_install(true);
        ui.invoke_install_service();
        wait_for_service_job(&ui, &ops);
        assert!(!ui.get_service_result_is_error());
        assert_eq!(ui.get_service_result(), "Background service installed.");
        assert!(ui.get_service_can_update());
        assert!(ui.get_service_can_remove());
        ui.invoke_update_service();
        wait_for_service_job(&ui, &ops);
        assert_eq!(ui.get_service_result(), "Background service updated.");
        ui.invoke_uninstall_service();
        assert!(
            !ui.get_service_busy(),
            "removal requires the confirmation step"
        );
        ui.set_service_confirming_remove(true);
        ui.invoke_uninstall_service();
        wait_for_service_job(&ui, &ops);
        assert!(ui.get_service_result().contains("Saved settings were kept"));
        assert_eq!(
            *backend.actions.lock().unwrap(),
            [
                ServiceAction::Install,
                ServiceAction::Install,
                ServiceAction::Update,
                ServiceAction::Uninstall { purge: false },
            ]
        );
        assert_eq!(service.borrow().current_snapshot().command_seq, 0);
        release_service.send(()).unwrap();
    }

    #[test]
    fn native_service_setup_is_available_without_terminal_commands_and_preserves_ownership() {
        let (service, release_service) = gated_service(1);
        let (ui, ops, _timer) = wired_ui(&service);
        ui.set_is_simulated(false);
        let native_owner = Ownership::NativePackage {
            manager: "pacman".into(),
            package: "awcc-rs".into(),
            unit_path: "/usr/lib/systemd/system/awccd.service".into(),
        };
        let mut inspection = service_inspection(native_owner.clone(), UnitState::Stopped);
        inspection.installation.enabled = Some(false);
        let backend = Arc::new(FakeServiceManagement {
            inspection: std::sync::Mutex::new(inspection),
            failure: std::sync::Mutex::new(Some(
                "Administrator authorization was cancelled".into(),
            )),
            ..Default::default()
        });
        ops.borrow_mut().service_backend = backend.clone();
        ui.invoke_refresh_service_status();
        wait_for_service_job(&ui, &ops);
        assert!(ui.get_service_can_setup());
        assert!(!ui.get_service_can_install());
        assert!(!ui.get_service_can_update());
        assert!(!ui.get_service_can_remove());

        ui.invoke_setup_service();
        wait_for_service_job(&ui, &ops);
        assert!(ui.get_service_result_is_error());
        assert!(ui.get_service_result().contains("cancelled"));
        assert!(
            ui.get_service_can_setup(),
            "cancelled setup remains retryable"
        );

        *backend.failure.lock().unwrap() = None;
        ui.invoke_setup_service();
        wait_for_service_job(&ui, &ops);
        assert_eq!(ui.get_service_result(), "Background service configured.");
        assert_eq!(ui.get_service_state_label(), "Running");
        assert!(!ui.get_service_can_setup());
        assert_eq!(ui.get_service_managed_by(), "native-package");
        assert_eq!(
            backend.inspection.lock().unwrap().installation.ownership,
            native_owner
        );
        assert!(!ui.get_service_can_update());
        assert!(!ui.get_service_can_remove());
        ui.invoke_setup_service();
        assert_eq!(
            *backend.actions.lock().unwrap(),
            [ServiceAction::Setup, ServiceAction::Setup]
        );
        release_service.send(()).unwrap();
    }

    #[test]
    fn service_setup_never_offers_to_override_a_mask_or_unrecognized_installation() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let mut inspection = service_inspection(
            Ownership::AppimageInstaller {
                build_id: "test-build".into(),
            },
            UnitState::Masked,
        );
        sync_service_inspection_to_ui(&ui, &inspection);
        assert!(!ui.get_service_can_setup());
        inspection.installation.masked = false;
        inspection.installation.unit_state = UnitState::Stopped;
        inspection.installation.interrupted_operation = true;
        sync_service_inspection_to_ui(&ui, &inspection);
        assert!(!ui.get_service_can_setup());
        inspection.installation.interrupted_operation = false;
        inspection.installation.ownership = Ownership::Unknown {
            unit_path: "/etc/systemd/system/awccd.service".into(),
            reason: "manually installed".into(),
        };
        sync_service_inspection_to_ui(&ui, &inspection);
        assert!(!ui.get_service_can_setup());
    }

    #[test]
    fn service_removal_purges_settings_only_when_selected_and_confirmed() {
        let (service, release_service) = gated_service(1);
        let (ui, ops, _timer) = wired_ui(&service);
        ui.set_is_simulated(false);
        let backend = Arc::new(FakeServiceManagement {
            inspection: std::sync::Mutex::new(service_inspection(
                Ownership::AppimageInstaller {
                    build_id: "test-build".into(),
                },
                UnitState::Running,
            )),
            ..Default::default()
        });
        ops.borrow_mut().service_backend = backend.clone();
        ui.invoke_refresh_service_status();
        wait_for_service_job(&ui, &ops);
        assert!(!ui.get_service_remove_purge());
        ui.set_service_remove_purge(true);
        ui.invoke_uninstall_service();
        assert!(
            backend.actions.lock().unwrap().is_empty(),
            "purge still needs removal confirmation"
        );
        ui.set_service_confirming_remove(true);
        ui.invoke_uninstall_service();
        wait_for_service_job(&ui, &ops);
        assert_eq!(
            *backend.actions.lock().unwrap(),
            [ServiceAction::Uninstall { purge: true }]
        );
        assert!(ui.get_service_result().contains("settings were deleted"));
        release_service.send(()).unwrap();
    }

    #[test]
    fn restore_preference_waits_for_confirmation_and_surfaces_settings_save_failures() {
        let (service, release) = gated_service(1);
        let (ui, ops, _timer) = wired_ui(&service);
        ui.set_is_simulated(false);
        ui.set_service_restore_available(true);
        ui.invoke_set_restore_at_startup(true);
        assert!(!ui.get_service_restore_at_startup());
        assert_eq!(ui.get_lifecycle_badge(), "REQUESTED");
        assert!(
            ops.borrow()
                .pending
                .as_ref()
                .unwrap()
                .label
                .contains("startup")
        );

        let mut confirmed = healthy_snapshot();
        confirmed.is_simulated = false;
        confirmed.service_control = Some(awcc_service::wire::ControlStatus {
            state: ControlState::Idle,
            saved_curve: FanCurveConfig::default(),
            intent_active: false,
            restore_at_startup: true,
            last_evaluation_age_secs: None,
            last_outcome: None,
            failure_reason: None,
            tick_interval_ms: 1000,
            uptime_secs: 1,
            cpu_sensor_fresh: true,
            settings_problem: None,
            settings_save_pending: false,
            settings_save_error: Some("Disk full".into()),
        });
        sync_snapshot_to_ui(&ui, &confirmed, false);
        assert!(ui.get_service_restore_at_startup());
        assert!(ui.get_service_restore_available());
        assert!(ui.get_service_curve_summary().contains("Disk full"));
        confirmed.service_error = Some("Connection lost".into());
        sync_snapshot_to_ui(&ui, &confirmed, false);
        assert!(!ui.get_service_restore_available());
        assert!(ui.get_service_curve_summary().contains("Last known state"));
        release.send(()).unwrap();
    }

    #[test]
    fn enqueue_rejection_is_shown_distinctly_and_keeps_staged_edits() {
        let (service, release) = gated_service(1);
        let (ui, ops, _timer) = wired_ui(&service);

        // First apply is queued: only *requested*, staged edits are consumed.
        ui.invoke_stage_cpu_boost(80.0);
        ui.invoke_apply_staged_boost();
        assert_eq!(ui.get_lifecycle_badge().as_str(), "REQUESTED");
        assert!(!ui.get_lifecycle_is_error());
        assert!(!ui.get_boost_dirty());
        assert_eq!(ops.borrow().pending.as_ref().map(|p| p.seq), Some(1));

        // Second apply hits the full queue: reported, not replayed, edits kept.
        ui.invoke_stage_cpu_boost(60.0);
        assert!(ui.get_boost_dirty());
        ui.invoke_apply_staged_boost();
        assert_eq!(ui.get_lifecycle_badge().as_str(), "NOT SENT");
        assert!(ui.get_lifecycle_is_error());
        assert!(ui.get_lifecycle_message().contains("queue is full"));
        assert!(ui.get_boost_dirty(), "rejected edits must stay staged");
        assert_eq!(
            ops.borrow().pending.as_ref().map(|p| p.seq),
            Some(1),
            "a rejected request must not displace the one in flight"
        );

        release.send(()).expect("release worker");
    }

    #[test]
    fn disconnected_worker_rejection_is_reported_as_stopped_not_full() {
        let orchestrator = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");
        let handle = ThermalServiceHandle::spawn_with(orchestrator, 4, |body| {
            drop(body);
            std::thread::Builder::new().spawn(|| {})
        })
        .expect("worker that exits at once");
        let service: SharedService = Rc::new(RefCell::new(handle));
        let (ui, _ops, _timer) = wired_ui(&service);

        ui.invoke_stage_tcc_offset(9.0);
        ui.invoke_apply_staged_tcc();
        assert_eq!(ui.get_lifecycle_badge().as_str(), "NOT SENT");
        assert!(ui.get_lifecycle_message().contains("not running"));
        assert!(!ui.get_lifecycle_message().contains("queue is full"));
        assert!(ui.get_tcc_dirty());
    }

    #[test]
    fn pending_request_is_not_replaced_by_an_older_outcome() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let ops: SharedOps = Rc::new(RefCell::new(UiOps::default()));
        ops.borrow_mut().pending = Some(PendingRequest {
            seq: 2,
            label: "Switch thermal mode to Quiet".to_string(),
        });

        let mut old = healthy_snapshot();
        old.command_seq = 1;
        old.lifecycle = OperationLifecycle::Confirmed {
            summary: "an earlier operation".to_string(),
        };
        apply_polled_snapshot(&ui, &old, &ops);
        assert_eq!(ui.get_lifecycle_badge().as_str(), "REQUESTED");
        assert!(
            ui.get_lifecycle_message()
                .contains("Switch thermal mode to Quiet")
        );
        assert!(ops.borrow().pending.is_some());

        let mut answered = old.clone();
        answered.command_seq = 2;
        answered.lifecycle = OperationLifecycle::Failed {
            summary: "Failed to set thermal mode to Quiet".to_string(),
            error: "rejected".to_string(),
        };
        apply_polled_snapshot(&ui, &answered, &ops);
        assert_eq!(ui.get_lifecycle_badge().as_str(), "FAILED");
        assert!(ui.get_lifecycle_is_error());
        assert!(ops.borrow().pending.is_none());
    }

    #[test]
    fn accepted_outcome_is_presented_with_its_caveat_and_not_as_confirmed() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let mut snap = healthy_snapshot();
        snap.lifecycle = OperationLifecycle::Accepted {
            summary: "Applied Static to Keyboard".to_string(),
            caveat: "the ELC protocol has no readback of applied lighting".to_string(),
        };
        sync_snapshot_to_ui(&ui, &snap, true);
        assert_eq!(ui.get_lifecycle_badge().as_str(), "ACCEPTED");
        assert!(!ui.get_lifecycle_is_error());
        assert!(ui.get_lifecycle_message().contains("no readback"));
    }

    #[test]
    fn a_stopped_worker_is_surfaced_once_and_halts_auto_refresh() {
        let orchestrator = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");
        let handle = ThermalServiceHandle::spawn_with(orchestrator, 4, |body| {
            std::thread::Builder::new().spawn(move || {
                let _held = body;
                panic!("simulated worker crash");
            })
        })
        .expect("panicking worker");
        let service: SharedService = Rc::new(RefCell::new(handle));
        let (ui, ops, timer) = wired_ui(&service);
        ui.set_auto_refresh_enabled(true);
        restart_auto_refresh_timer(
            &timer,
            ui.as_weak(),
            Rc::clone(&service),
            Rc::clone(&ops),
            true,
            1000,
        );
        assert!(timer.running());

        for _ in 0..400 {
            if service.borrow().worker_failure().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        report_worker_failure_once(&ui, &service, &ops, &timer);
        assert_eq!(ui.get_lifecycle_badge().as_str(), "SERVICE STOPPED");
        assert!(ui.get_lifecycle_is_error());
        assert!(
            ui.get_lifecycle_message()
                .contains("simulated worker crash")
        );
        assert!(ops.borrow().worker_failure_shown);
        assert!(
            !timer.running(),
            "auto-refresh must stop once the worker is gone"
        );

        // A later user action is rejected with the same cause, and the notice is not repeated.
        ui.set_lifecycle_message(SharedString::from("cleared"));
        report_worker_failure_once(&ui, &service, &ops, &timer);
        assert_eq!(ui.get_lifecycle_message().as_str(), "cleared");
        ui.invoke_toggle_cpu_turbo(false);
        assert_eq!(ui.get_lifecycle_badge().as_str(), "NOT SENT");
        assert!(
            ui.get_lifecycle_message()
                .contains("simulated worker crash")
        );
    }

    #[test]
    fn full_audit_ring_continues_updating_gui_rows_and_shows_eviction_gap_and_clear_history() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");

        // Fill the 256-entry ring exactly to capacity:
        for _ in 0..256 {
            let snap = orch.set_thermal_mode(ThermalMode::Balanced);
            sync_snapshot_to_ui(&ui, &snap, false);
        }
        assert_eq!(ui.get_write_count(), 256);
        assert_eq!(ui.get_audit_retained_count(), 256);
        assert_eq!(ui.get_audit_evicted_count(), 0);
        assert_eq!(ui.get_audit_rows().row_count(), 256);
        assert_eq!(
            ui.get_audit_rows().row_data(0).unwrap().seq_text.as_str(),
            "#256"
        );
        assert!(ui.get_audit_eviction_notice().is_empty());

        // Issue 10 more writes at constant ring capacity (row_count remains 256!):
        for i in 1..=10 {
            let mode = if i % 2 == 0 {
                ThermalMode::Performance
            } else {
                ThermalMode::Quiet
            };
            let snap = orch.set_thermal_mode(mode);
            // `sync_staged_sliders = false` exercises the normal polled update path!
            sync_snapshot_to_ui(&ui, &snap, false);
        }
        assert_eq!(ui.get_write_count(), 266);
        assert_eq!(ui.get_audit_retained_count(), 256);
        assert_eq!(ui.get_audit_evicted_count(), 10);
        assert_eq!(ui.get_audit_rows().row_count(), 256);
        assert_eq!(
            ui.get_audit_rows().row_data(0).unwrap().seq_text.as_str(),
            "#266",
            "newest row must advance past #256 even when row_count() stays 256"
        );
        assert_eq!(
            ui.get_audit_rows().row_data(255).unwrap().seq_text.as_str(),
            "#11",
            "oldest retained row must advance to #11 after 10 evictions"
        );
        assert!(
            ui.get_audit_eviction_notice()
                .contains("10 older write entries evicted"),
            "notice: {}",
            ui.get_audit_eviction_notice()
        );
        assert!(
            ui.get_audit_counter_summary()
                .contains("266 lifetime · 256 retained · 10 evicted"),
            "summary: {}",
            ui.get_audit_counter_summary()
        );

        // Clearing visible history empties `audit-rows` while preserving `write-count` (266)
        // and allowing subsequent writes (#267) to appear immediately.
        let cleared_snap = orch.execute(ServiceCommand::ClearAuditHistory).snapshot;
        sync_snapshot_to_ui(&ui, &cleared_snap, false);
        assert_eq!(ui.get_write_count(), 266);
        assert_eq!(ui.get_audit_retained_count(), 0);
        assert_eq!(ui.get_audit_cleared_count(), 256);
        assert_eq!(ui.get_audit_rows().row_count(), 0);

        let after_clear = orch.set_thermal_mode(ThermalMode::Cool);
        sync_snapshot_to_ui(&ui, &after_clear, false);
        assert_eq!(ui.get_write_count(), 267);
        assert_eq!(ui.get_audit_retained_count(), 1);
        assert_eq!(ui.get_audit_rows().row_count(), 1);
        assert_eq!(
            ui.get_audit_rows().row_data(0).unwrap().seq_text.as_str(),
            "#267"
        );
    }

    #[test]
    fn gui_support_export_copy_summary_recording_toggle_and_row_detail_are_strictly_passive() {
        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");
        // Seed one write before spawning the worker so there is an audit row to inspect:
        let _ = orch.set_thermal_mode(ThermalMode::Performance);

        let service: SharedService = Rc::new(RefCell::new(
            ThermalServiceHandle::spawn(orch).expect("worker"),
        ));
        let (ui, ops, _timer) = wired_ui(&service);

        let tmp = std::env::temp_dir().join(format!(
            "awcc-gui-support-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let zip_path = tmp.join("gui-support.zip");
        ops.borrow_mut().export_output_override = Some(zip_path.clone());

        assert_eq!(ui.get_write_count(), 1);
        assert_eq!(ui.get_audit_rows().row_count(), 1);

        // 1. Select and copy audit row #1:
        ui.invoke_select_audit_row(1);
        assert_eq!(ui.get_selected_audit_seq(), 1);
        assert!(
            ui.get_selected_audit_detail()
                .contains("Set Thermal Profile → Performance"),
            "detail: {}",
            ui.get_selected_audit_detail()
        );
        ui.invoke_copy_selected_audit_row();
        assert_eq!(ui.get_last_copied_text(), ui.get_selected_audit_detail());

        // 2. Toggle temporary DEBUG recording on and off (and verify auto-expiry):
        assert!(!ui.get_diagnostic_recording_active());
        ui.invoke_toggle_diagnostic_recording();
        assert!(ui.get_diagnostic_recording_active());
        assert!(ops.borrow().diagnostics.is_recording_active());
        assert_eq!(
            ops.borrow().diagnostics.active_level(),
            awcc_diagnostics::event::Severity::Debug
        );
        ui.invoke_toggle_diagnostic_recording();
        assert!(!ui.get_diagnostic_recording_active());
        assert!(!ops.borrow().diagnostics.is_recording_active());
        assert_eq!(
            ops.borrow().diagnostics.active_level(),
            awcc_diagnostics::event::Severity::Info
        );

        // Short-duration recording expires and `apply_polled_snapshot` updates UI state:
        ops.borrow()
            .diagnostics
            .start_recording(Duration::from_millis(50));
        sync_recording_state_to_ui(&ui, &ops.borrow().diagnostics);
        assert!(ui.get_diagnostic_recording_active());
        std::thread::sleep(Duration::from_millis(65));
        let snap = service.borrow().current_snapshot().clone();
        apply_polled_snapshot(&ui, &snap, &ops);
        assert!(!ui.get_diagnostic_recording_active());

        // 3. Copy diagnostic summary and export support report — neither may enqueue any worker
        //    command or change `write_count` / `command_seq`:
        let seq_before = service.borrow().current_snapshot().command_seq;
        ui.invoke_copy_diagnostic_summary();
        wait_for_diagnostics_job(&ui, &ops);
        assert!(
            ui.get_last_copied_text()
                .contains("AWCC Diagnostic Summary"),
            "copied summary: {}",
            ui.get_last_copied_text()
        );
        assert!(
            ui.get_last_copied_text().contains("worker_alive=true"),
            "copied summary: {}",
            ui.get_last_copied_text()
        );
        ui.invoke_export_support_report();
        wait_for_diagnostics_job(&ui, &ops);
        assert_eq!(
            ui.get_last_export_path().as_str(),
            zip_path.display().to_string()
        );
        assert_eq!(
            service.borrow().current_snapshot().command_seq,
            seq_before,
            "passive summary/export must not enqueue any service command"
        );
        assert_eq!(ui.get_write_count(), 1);

        let entries =
            awcc_diagnostics::zip::read_stored_zip(&std::fs::read(&zip_path).unwrap()).unwrap();
        for expected in [
            "summary.txt",
            "report.json",
            "events.jsonl",
            "hardware-audit.jsonl",
            "build-info.json",
            "settings.json",
        ] {
            assert!(
                entries.iter().any(|e| e.name == expected),
                "missing {expected} in GUI support zip"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn clipboard_copy_actions_write_to_backend_and_report_failures_honestly() {
        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");
        let _ = orch.set_thermal_mode(ThermalMode::Quiet);
        let service: SharedService = Rc::new(RefCell::new(
            ThermalServiceHandle::spawn(orch).expect("worker"),
        ));
        let (ui, ops, _timer) = wired_ui(&service);

        let fake_clip = FakeClipboard::default();
        ops.borrow_mut().clipboard = Arc::new(std::sync::Mutex::new(Box::new(fake_clip.clone())));

        // 1. Successful row copy writes exact detail to clipboard and reports "Copied...":
        ui.invoke_select_audit_row(1);
        ui.invoke_copy_selected_audit_row();
        assert_eq!(fake_clip.copy_count(), 1);
        assert_eq!(
            fake_clip.last_copied().as_deref(),
            Some(ui.get_selected_audit_detail().as_str())
        );
        assert!(!ui.get_clipboard_fallback_visible());
        assert!(
            ui.get_diagnostics_status_text().starts_with("Copied "),
            "expected 'Copied...' status, got: {}",
            ui.get_diagnostics_status_text()
        );

        // 2. Successful summary copy writes summary text to clipboard and reports "Copied...":
        ui.invoke_copy_diagnostic_summary();
        wait_for_diagnostics_job(&ui, &ops);
        assert_eq!(fake_clip.copy_count(), 2);
        assert!(
            fake_clip
                .last_copied()
                .as_deref()
                .unwrap_or_default()
                .contains("AWCC Diagnostic Summary")
        );
        assert!(!ui.get_clipboard_fallback_visible());
        assert!(
            ui.get_diagnostics_status_text().starts_with("Copied "),
            "expected 'Copied...' status, got: {}",
            ui.get_diagnostics_status_text()
        );

        // 3. When clipboard backend fails, neither action may claim "Copied"; both surface a
        //    clear failure/fallback status and expose the read-only fallback text:
        fake_clip.set_failure(Some("headless session has no clipboard owner"));
        ui.invoke_copy_selected_audit_row();
        assert_eq!(fake_clip.copy_count(), 2);
        assert!(ui.get_clipboard_fallback_visible());
        assert_eq!(ui.get_last_copied_text(), ui.get_selected_audit_detail());
        let row_fail_status = ui.get_diagnostics_status_text();
        assert!(
            row_fail_status.contains("Clipboard unavailable"),
            "got: {row_fail_status}"
        );
        assert!(
            !row_fail_status.contains("Copied"),
            "failed clipboard copy must never claim 'Copied': {row_fail_status}"
        );

        ui.invoke_copy_diagnostic_summary();
        wait_for_diagnostics_job(&ui, &ops);
        assert_eq!(fake_clip.copy_count(), 2);
        assert!(ui.get_clipboard_fallback_visible());
        assert!(
            ui.get_last_copied_text()
                .contains("AWCC Diagnostic Summary")
        );
        let summary_fail_status = ui.get_diagnostics_status_text();
        assert!(
            summary_fail_status.contains("Clipboard unavailable"),
            "got: {summary_fail_status}"
        );
        assert!(
            !summary_fail_status.contains("Copied"),
            "failed summary copy must never claim 'Copied': {summary_fail_status}"
        );
    }

    #[test]
    fn async_summary_and_export_run_off_ui_thread_prevent_duplicate_triggers_and_tolerate_window_drop()
     {
        let orch = ThermalOrchestrator::new_simulated_m15_r6().expect("sim");
        let service: SharedService = Rc::new(RefCell::new(
            ThermalServiceHandle::spawn(orch).expect("worker"),
        ));
        let (ui, ops, _timer) = wired_ui(&service);

        let tmp = std::env::temp_dir().join(format!(
            "awcc-gui-async-diag-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let zip_path = tmp.join("async-support.zip");
        ops.borrow_mut().export_output_override = Some(zip_path.clone());

        let fake_clip = FakeClipboard::default();
        ops.borrow_mut().clipboard = Arc::new(std::sync::Mutex::new(Box::new(fake_clip.clone())));

        // Gate the background worker so we can observe the in-flight state on the UI thread:
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let hook_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let hook_runs = Arc::clone(&hook_runs);
            let release_rx = Arc::clone(&release_rx);
            ops.borrow_mut().diag_job_hook = Some(Arc::new(move || {
                hook_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = entered_tx.send(());
                let _ = release_rx.lock().unwrap().recv();
            }));
        }

        // 1. Trigger `copy_diagnostic_summary` — returns immediately on UI thread while worker waits:
        ui.invoke_copy_diagnostic_summary();
        assert!(ui.get_diagnostics_busy());
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("background summary thread should start");

        // UI state updates remain responsive while background summary is in flight:
        ui.invoke_stage_cpu_boost(77.0);
        assert_eq!(ui.get_staged_cpu_boost(), 77.0);
        assert!(ui.get_boost_dirty());

        // Duplicate clicks on Copy Summary or Export Support Report while busy are ignored:
        ui.invoke_copy_diagnostic_summary();
        ui.invoke_export_support_report();
        assert_eq!(hook_runs.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Release the background summary job and poll completion:
        release_tx.send(()).unwrap();
        wait_for_diagnostics_job(&ui, &ops);
        assert!(!ui.get_diagnostics_busy());
        assert_eq!(fake_clip.copy_count(), 1);

        // 2. Trigger `export_support_report` with the same gate and verify async completion:
        ui.invoke_export_support_report();
        assert!(ui.get_diagnostics_busy());
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("background export thread should start");
        release_tx.send(()).unwrap();
        wait_for_diagnostics_job(&ui, &ops);
        assert!(!ui.get_diagnostics_busy());
        assert!(zip_path.exists());

        // 3. Dropping the window/ops while a background job is in flight must not panic:
        ui.invoke_copy_diagnostic_summary();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("third background thread should start");
        drop(ui);
        drop(ops);
        release_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(25));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn audit_row_detail_and_copy_respect_trace_raw_usb_opt_in() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().expect("window");
        let canary = "03 24 aa bb cc dd ee ff 11 22 33 44";

        let mut snap = healthy_snapshot();
        snap.write_audit_log.push(WriteAuditEntry {
            sequence: 1,
            op_id: Some(9),
            command: "USB HID 187c:0550 OUT [03 24 02 07 d0 00 fa f5 9e 0b]".to_string(),
            outcome: "33 bytes transferred".to_string(),
            succeeded: true,
            ack: awcc_platform::AckStatus::Received,
            raw_usb_response_hex: Some(canary.to_string()),
        });
        snap.audit_stats.lifetime_total = 1;
        snap.audit_stats.retained = 1;
        snap.audit_stats.last_seq = 1;

        // Default (`trace-raw-usb == false`): raw USB response hex must NOT appear in row detail:
        ui.set_trace_raw_usb(false);
        sync_snapshot_to_ui(&ui, &snap, true);
        let row_default = ui.get_audit_rows().row_data(0).expect("row 0");
        assert!(
            !row_default.detail.contains(canary),
            "default GUI audit row detail must omit raw_usb_response_hex: {}",
            row_default.detail
        );
        assert!(!row_default.detail.contains("raw_usb_in="));

        // Explicit opt-in (`trace-raw-usb == true`): raw USB response hex is included:
        ui.set_trace_raw_usb(true);
        sync_snapshot_to_ui(&ui, &snap, true);
        let row_opt_in = ui.get_audit_rows().row_data(0).expect("row 0");
        assert!(
            row_opt_in.detail.contains(canary),
            "opt-in GUI audit row detail must include raw_usb_response_hex: {}",
            row_opt_in.detail
        );
    }

    #[test]
    fn verbose_and_log_level_cli_flags_parse_and_reject_invalid_values() {
        let opts = parse_cli_args(vec![
            "-v".to_string(),
            "--log-level".to_string(),
            "debug".to_string(),
            "--trace-raw-usb".to_string(),
            "--sim".to_string(),
        ])
        .expect("valid flags");
        assert!(opts.verbose);
        assert_eq!(opts.log_level.as_deref(), Some("debug"));
        assert!(opts.trace_raw_usb);

        let bad = parse_cli_args(vec!["--log-level=not-valid".to_string()]);
        assert!(bad.is_err());
    }
}
