//! AWCC Linux — Slint Material 3 (`@material`) desktop application & headless verification runner.

use std::cell::RefCell;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use awcc_core::{
    DeviceProfile, FanCurveConfig, FanCurvePoint, SimScenario, ThermalMode, ThermalModeState,
    lighting::{LightingEffect, PowerStateSlot, ZoneTarget},
};
use awcc_service::{
    ServiceCommand, SystemSnapshot, ThermalOrchestrator, ThermalServiceHandle,
    resolve_host_device_profile,
};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::{
    Color, ComponentHandle, ModelRc, PhysicalSize, SharedString, Timer, TimerMode, VecModel,
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
                        "--scenario requires a scenario name (normal|compact|rollback|unknown|lighting|diagnostics)"
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
  --scenario <NAME>          Preset state for screenshot: normal | compact | rollback | unknown | lighting | diagnostics"
    );
}

fn run_device_info(sim_mode: bool) -> Result<()> {
    let profile = if sim_mode {
        DeviceProfile::alienware_m15_r6()?
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
    ui.set_lifecycle_message(SharedString::from(snap.lifecycle.detail_message()));
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
        "Boot Slot 0x0061"
    } else {
        "Volatile Session"
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

    let rows: Vec<AuditRow> = snap
        .write_audit_log
        .iter()
        .map(|entry| AuditRow {
            seq_text: SharedString::from(format!("#{:02}", entry.sequence)),
            command: SharedString::from(entry.command.as_str()),
            outcome: SharedString::from(entry.outcome.as_str()),
            succeeded: entry.succeeded,
        })
        .collect();
    ui.set_audit_rows(ModelRc::new(VecModel::from(rows)));
}

fn wire_ui_callbacks(ui: &MainWindow, service: &Rc<RefCell<ThermalServiceHandle>>) {
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
        "diagnostics" => {
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
    wire_ui_callbacks(&ui, &service);

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
    fn ui_staging_never_emits_hardware_writes_until_apply_is_invoked() {
        i_slint_backend_testing::init_no_event_loop();
        let orchestrator =
            ThermalOrchestrator::new_simulated_m15_r6().expect("simulated m15 R6 should init");
        let service = Rc::new(RefCell::new(ThermalServiceHandle::spawn(orchestrator)));
        let ui = MainWindow::new().expect("MainWindow should instantiate under testing backend");

        let initial_snap = service.borrow().current_snapshot().clone();
        sync_snapshot_to_ui(&ui, &initial_snap, true);
        wire_ui_callbacks(&ui, &service);

        assert_eq!(ui.get_write_count(), 0);
        assert_eq!(ui.get_active_mode_label().as_str(), "Balanced");
        assert!(!ui.get_boost_dirty());
        assert!(!ui.get_tcc_dirty());

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
