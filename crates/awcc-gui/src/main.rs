//! AWCC Linux — Slint Material 3 (`@material`) desktop application & headless verification runner.

use std::cell::RefCell;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use awcc_core::{
    FanCurveConfig, FanCurvePoint, OperationLifecycle, SimScenario, ThermalMode, ThermalModeState,
    lighting::{LightingEffect, PowerStateSlot, ZoneTarget},
};
use awcc_platform::WriteAuditEntry;
use awcc_service::{
    ServiceCommand, SystemSnapshot, ThermalOrchestrator, ThermalServiceHandle,
    resolve_host_device_profile, resolve_simulated_m15_r6_profile,
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
    sim_mode: bool,
    device_info: bool,
    query_mode: bool,
    screenshot_path: Option<PathBuf>,
    width: u32,
    height: u32,
    scenario: String,
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
            "--sim" | "--test-mode" => opts.sim_mode = true,
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

fn print_help() {
    println!(
        "\
AWCC Linux (Rust + Slint @material 1.1.0 Desktop Application)

USAGE:
  awcc-gui [OPTIONS | COMMAND]

PASSIVE COMMANDS (0 hardware writes):
  -h, --help                 Print this help message and exit
  device-info, --device-info Print resolved DMI profile, ACPI prefix, and database bitmasks
  query-mode, --query-mode   Query current thermal mode and exit

OPTIONS:
  --sim                      Run against the in-memory Alienware m15 R6 simulator (with fault-injection bar)
  --screenshot <PATH>        Render a deterministic PNG screenshot using the Slint software renderer
  --width <PX>               Window width for screenshot rendering (default: 1120)
  --height <PX>              Window height for screenshot rendering (default: 720)
  --scenario <NAME>          Preset state for screenshot: normal | compact | rollback | unknown | lighting | diagnostics | settings"
    );
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
            let resolved_mode = code_table.and_then(|table| {
                match table.decode_query_response(u32::from(arg2)) {
                    ThermalModeState::Known(m) => Some(m.display_name()),
                    ThermalModeState::Unknown { .. } => ThermalMode::ALL
                        .iter()
                        .copied()
                        .find(|&m| table.set_code_for(m) == arg2)
                        .map(ThermalMode::display_name),
                }
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
        "33 bytes transferred" => "OK (33B ACK)".to_string(),
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

    let gmode_active = snap.game_shift_enabled
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
    ui.set_cpu_turbo_enabled(snap.cpu_turbo_enabled);

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
    ui.set_active_lighting_summary(SharedString::from(format!(
        "{} · {} · Brightness {}% · {persist_str}",
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

    if let Some(diag) = &snap.elc_diagnostics {
        ui.set_elc_firmware_summary(SharedString::from(format!(
            "ELC {:04x}:{:04x} · FW {}.{}.{} · Platform 0x{:04X} ({} Active Zones, {} Stored Animations)",
            diag.vendor_id,
            diag.product_id,
            diag.firmware_version.0,
            diag.firmware_version.1,
            diag.firmware_version.2,
            diag.elc_config.platform_id,
            diag.elc_config.active_zones,
            diag.animation_count
        )));
    }

    // Only rebuild the virtualized Log model when the number of recorded entries changes.
    if sync_staged_sliders || ui.get_audit_rows().row_count() != snap.write_audit_log.len() {
        let rows: Vec<AuditRow> = snap
            .write_audit_log
            .iter()
            .rev()
            .map(|entry| {
                let (command_str, outcome_str) =
                    format_log_entry(entry, Some(&snap.profile.code_table));
                AuditRow {
                    seq_text: SharedString::from(format!("#{:02}", entry.sequence)),
                    command: SharedString::from(command_str),
                    outcome: SharedString::from(outcome_str),
                    succeeded: entry.succeeded,
                }
            })
            .collect();
        ui.set_audit_rows(ModelRc::new(VecModel::from(rows)));
    }
}

fn restart_auto_refresh_timer(
    timer: &Timer,
    ui_weak: slint::Weak<MainWindow>,
    service: Rc<RefCell<ThermalServiceHandle>>,
    enabled: bool,
    interval_ms: i32,
) {
    if !enabled {
        timer.stop();
        return;
    }
    let ms = u64::try_from(interval_ms.clamp(250, 60_000)).unwrap_or(1000);
    timer.start(TimerMode::Repeated, Duration::from_millis(ms), move || {
        if let Some(w) = ui_weak.upgrade() {
            let cmd = if w.get_fan_curve_active() {
                ServiceCommand::StepFanCurve
            } else {
                ServiceCommand::Refresh
            };
            service.borrow().send(cmd);
        }
    });
}

fn wire_ui_callbacks(
    ui: &MainWindow,
    service: &Rc<RefCell<ThermalServiceHandle>>,
    refresh_timer: &Rc<Timer>,
) {
    {
        let svc = Rc::clone(service);
        ui.on_request_refresh(move || {
            svc.borrow().send(ServiceCommand::Refresh);
        });
    }

    {
        let svc = Rc::clone(service);
        ui.on_select_thermal_mode(move |mode_str| {
            if let Some(mode) = parse_mode_key(mode_str.as_str()) {
                svc.borrow().send(ServiceCommand::SetThermalMode(mode));
            }
        });
    }

    {
        let svc = Rc::clone(service);
        ui.on_request_toggle_gmode(move || {
            svc.borrow().send(ServiceCommand::ToggleGMode);
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
        let svc = Rc::clone(service);
        ui.on_apply_staged_boost(move || {
            if let Some(w) = ui_weak.upgrade() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let cpu = w.get_staged_cpu_boost().round().clamp(0.0, 100.0) as u8;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let gpu = w.get_staged_gpu_boost().round().clamp(0.0, 100.0) as u8;
                w.set_boost_dirty(false);
                svc.borrow().send(ServiceCommand::ApplyManualBoost {
                    cpu_boost_pct: cpu,
                    gpu_boost_pct: gpu,
                });
            }
        });
    }

    {
        let svc = Rc::clone(service);
        ui.on_toggle_cpu_turbo(move |enabled| {
            svc.borrow().send(ServiceCommand::SetCpuTurbo(enabled));
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
        let svc = Rc::clone(service);
        ui.on_apply_staged_tcc(move || {
            if let Some(w) = ui_weak.upgrade() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let offset = w.get_staged_tcc_offset().round().clamp(0.0, 15.0) as u8;
                w.set_tcc_dirty(false);
                svc.borrow().send(ServiceCommand::SetTccOffset(offset));
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
        let svc = Rc::clone(service);
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
                w.set_curve_dirty(false);
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
                svc.borrow().send(ServiceCommand::SetFanCurve(cfg));
            }
        });
    }

    {
        let svc = Rc::clone(service);
        ui.on_stop_fan_curve(move || {
            svc.borrow()
                .send(ServiceCommand::SetFanCurve(FanCurveConfig::default()));
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
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
                svc.borrow().send(ServiceCommand::ApplyLighting {
                    effect,
                    target,
                    persist_boot,
                });
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        ui.on_apply_staged_brightness(move || {
            if let Some(w) = ui_weak.upgrade() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let pct = w.get_staged_lighting_brightness().round().clamp(0.0, 100.0) as u8;
                svc.borrow().send(ServiceCommand::SetBrightness(pct));
            }
        });
    }

    {
        let svc = Rc::clone(service);
        ui.on_toggle_stealth_mode(move |enable| {
            svc.borrow().send(ServiceCommand::SetStealth(enable));
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
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
                svc.borrow().send(ServiceCommand::ApplyLighting {
                    effect,
                    target: ZoneTarget::PowerButton,
                    persist_boot: true,
                });
            }
        });
    }

    {
        let svc = Rc::clone(service);
        ui.on_select_sim_scenario(move |idx| {
            let scenario = SimScenario::from_index(idx);
            svc.borrow().send(ServiceCommand::SetSimScenario(scenario));
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
        let timer = Rc::clone(refresh_timer);
        ui.on_toggle_auto_refresh(move |enabled| {
            if let Some(w) = ui_weak.upgrade() {
                let interval_ms = w.get_refresh_interval_ms();
                restart_auto_refresh_timer(
                    &timer,
                    w.as_weak(),
                    Rc::clone(&svc),
                    enabled,
                    interval_ms,
                );
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(service);
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
                    enabled,
                    interval_ms,
                );
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
        ThermalOrchestrator::new_hardware()
            .or_else(|_| ThermalOrchestrator::new_simulated_m15_r6())?
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

fn main() -> Result<()> {
    let opts = parse_cli_args(std::env::args().skip(1))?;

    if opts.show_help {
        print_help();
        return Ok(());
    }
    if opts.device_info {
        return run_device_info(opts.sim_mode);
    }
    if opts.query_mode {
        return run_query_mode(opts.sim_mode);
    }
    if let Some(screenshot_path) = &opts.screenshot_path {
        return render_headless_screenshot(
            screenshot_path,
            opts.width,
            opts.height,
            &opts.scenario,
            opts.sim_mode,
        );
    }

    let orchestrator = if opts.sim_mode {
        ThermalOrchestrator::new_simulated_m15_r6()?
    } else {
        ThermalOrchestrator::new_hardware()?
    };
    let service = Rc::new(RefCell::new(ThermalServiceHandle::spawn(orchestrator)));

    let ui = MainWindow::new().context("failed to create Slint MainWindow")?;
    {
        let initial_snap = service.borrow().current_snapshot().clone();
        sync_snapshot_to_ui(&ui, &initial_snap, true);
    }
    let refresh_timer = Rc::new(Timer::default());
    wire_ui_callbacks(&ui, &service, &refresh_timer);

    // Start periodic auto-refresh timer (default 1000ms = 1.0s, configurable in Settings).
    restart_auto_refresh_timer(
        &refresh_timer,
        ui.as_weak(),
        Rc::clone(&service),
        ui.get_auto_refresh_enabled(),
        ui.get_refresh_interval_ms(),
    );

    // Non-blocking 50ms poll timer to apply newly published worker snapshots on the UI thread.
    let poll_timer = Timer::default();
    {
        let ui_weak = ui.as_weak();
        let svc = Rc::clone(&service);
        poll_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            if let Some(w) = ui_weak.upgrade()
                && let Some(snap) = svc.borrow_mut().poll_latest()
            {
                sync_snapshot_to_ui(&w, snap, false);
            }
        });
    }

    ui.run().context("Slint event loop exited with error")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

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
            command: "pkexec sh -c 'echo \"\\_SB.AMWW.WMAX 0 0x15 {0x1,0xa3,0x0,0x00}\" > /proc/acpi/call && cat /proc/acpi/call'".to_string(),
            outcome: "0x0".to_string(),
            succeeded: true,
        };
        let (cmd, out) = format_log_entry(&pkexec_entry, None);
        assert!(!cmd.contains("pkexec"));
        assert!(cmd.contains("Set Thermal Profile → Performance"));
        assert!(cmd.contains("WMAX 0x15 [0x01, 0xA3, 0x00, 0x00]"));
        assert_eq!(out, "OK (0x00)");

        let usb_entry = WriteAuditEntry {
            sequence: 2,
            command: "USB HID 187c:0550 OUT [03 24 02 07 d0 00 fa f5 9e 0b]".to_string(),
            outcome: "33 bytes transferred".to_string(),
            succeeded: true,
        };
        let (usb_cmd, usb_out) = format_log_entry(&usb_entry, None);
        assert!(usb_cmd.contains("Set Lighting Keyframe → Static Color #F59E0B (2000 ms)"));
        assert!(usb_cmd.contains("ELC [03 24 02 07 D0 00 FA F5 9E 0B]"));
        assert_eq!(usb_out, "OK (33B ACK)");
    }

    #[test]
    fn ui_staging_never_emits_hardware_writes_until_apply_is_invoked() {
        i_slint_backend_testing::init_no_event_loop();
        let orchestrator =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");
        let service = Rc::new(RefCell::new(ThermalServiceHandle::spawn(orchestrator)));
        let ui = MainWindow::new().expect("MainWindow should instantiate under testing backend");

        let initial_snap = service.borrow().current_snapshot().clone();
        sync_snapshot_to_ui(&ui, &initial_snap, true);
        let refresh_timer = Rc::new(Timer::default());
        wire_ui_callbacks(&ui, &service, &refresh_timer);

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
        flipped_snap.game_shift_enabled = true;
        flipped_snap.thermal_mode_state = ThermalModeState::Known(ThermalMode::GMode);
        flipped_snap.cpu_turbo_enabled = false;
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
}
