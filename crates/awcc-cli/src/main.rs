//! Command-line interface (`awcc-cli`) with drop-in compatibility for C++ `awcc` subcommands.
//!
//! Guarantees that passive commands (`--help`, `-h`, `device-info`) execute with zero ACPI or USB
//! hardware touches.

use anyhow::{Context, Result, bail};
use awcc_core::{
    DeviceProfile, FanCurveConfig, OperationLifecycle, ThermalMode,
    lighting::{LightingEffect, ZoneTarget},
    validate_fan_boost, validate_tcc_offset,
};
use awcc_service::{ServiceCommand, ThermalOrchestrator, daemon::parse_fancurve_ipc_command};

/// Parsed CLI action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Help,
    DeviceInfo,
    DaemonMode,
    GuiMode,
    QueryMode,
    SetMode(ThermalMode),
    ToggleGMode,
    QueryCpuBoost,
    SetCpuBoost(u8),
    QueryGpuBoost,
    SetGpuBoost(u8),
    QueryTurbo,
    SetTurbo(bool),
    QueryTccOffset,
    SetTccOffset(u8),
    StopFanCurve,
    ConfigureFanCurve(FanCurveConfig),
    ApplyLighting {
        effect: LightingEffect,
        target: ZoneTarget,
        persist_boot: bool,
    },
    QueryBrightness,
    SetBrightness(u8),
    ToggleStealth,
    SetStealth(bool),
    SetPowerButtonColors {
        ac_color: u32,
        dc_color: u32,
        charging_color: u32,
        critical_color: u32,
    },
    RawStatus {
        status: bool,
        firmware: bool,
        elc_config: bool,
        anim_count: bool,
    },
    TestModes,
    TestZones,
}

fn parse_rgb_triplet(r_s: &str, g_s: &str, b_s: &str) -> Result<u32> {
    let r = r_s.parse::<u8>().context("invalid R component (0..=255)")?;
    let g = g_s.parse::<u8>().context("invalid G component (0..=255)")?;
    let b = b_s.parse::<u8>().context("invalid B component (0..=255)")?;
    Ok((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
}

/// Parse command-line arguments into a `CliCommand` without touching hardware.
pub fn parse_cli_args(args: &[String]) -> Result<CliCommand> {
    if args.is_empty() {
        return Ok(CliCommand::Help);
    }

    let mut zone_target = ZoneTarget::All;
    let mut persist_boot = true;
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" | "help" => return Ok(CliCommand::Help),
            "--test-mode" | "-v" | "--sim" => {}
            "-d" | "--daemon" => return Ok(CliCommand::DaemonMode),
            "-g" | "--gui" => return Ok(CliCommand::GuiMode),
            "--volatile" => persist_boot = false,
            "--persist" => persist_boot = true,
            "--zone" => {
                i += 1;
                let spec = args.get(i).context("missing value for `--zone`")?;
                zone_target = ZoneTarget::parse_spec(spec);
            }
            _ => positional.push(arg.to_string()),
        }
        i += 1;
    }

    let Some(cmd) = positional.first().map(String::as_str) else {
        return Ok(CliCommand::Help);
    };

    match cmd {
        "device-info" => Ok(CliCommand::DeviceInfo),
        "qm" | "query-mode" => Ok(CliCommand::QueryMode),
        "q" | "quiet" => Ok(CliCommand::SetMode(ThermalMode::Quiet)),
        "bs" | "battery-saver" => Ok(CliCommand::SetMode(ThermalMode::BatterySaver)),
        "b" | "balanced" => Ok(CliCommand::SetMode(ThermalMode::Balanced)),
        "fs" | "full-speed" => Ok(CliCommand::SetMode(ThermalMode::FullSpeed)),
        "p" | "performance" => Ok(CliCommand::SetMode(ThermalMode::Performance)),
        "g" | "gmode" => Ok(CliCommand::SetMode(ThermalMode::GMode)),
        "c" | "cool" => Ok(CliCommand::SetMode(ThermalMode::Cool)),
        "m" | "manual" => Ok(CliCommand::SetMode(ThermalMode::Manual)),
        "gt" => Ok(CliCommand::ToggleGMode),
        "cb" => Ok(CliCommand::QueryCpuBoost),
        "scb" => {
            let v = positional
                .get(1)
                .context("missing CPU boost percentage (0..=100)")?
                .parse::<u16>()?;
            Ok(CliCommand::SetCpuBoost(validate_fan_boost(v)?))
        }
        "gb" => Ok(CliCommand::QueryGpuBoost),
        "sgb" => {
            let v = positional
                .get(1)
                .context("missing GPU boost percentage (0..=100)")?
                .parse::<u16>()?;
            Ok(CliCommand::SetGpuBoost(validate_fan_boost(v)?))
        }
        "getturbo" => Ok(CliCommand::QueryTurbo),
        "st" => {
            let v = positional
                .get(1)
                .context("missing turbo state (0 or 1)")?
                .parse::<u8>()?;
            Ok(CliCommand::SetTurbo(v != 0))
        }
        "tcc" => {
            if let Some(arg1) = positional.get(1) {
                let offset = arg1.parse::<i32>()?;
                Ok(CliCommand::SetTccOffset(validate_tcc_offset(offset)?))
            } else {
                Ok(CliCommand::QueryTccOffset)
            }
        }
        "fancurve" => {
            let Some(arg1) = positional.get(1) else {
                bail!(
                    "usage: awcc-cli fancurve stop | <t1> <b1> <t2> <b2> <t3> <b3> <t4> <b4> [hyst]"
                );
            };
            if arg1.eq_ignore_ascii_case("stop") || arg1.eq_ignore_ascii_case("off") {
                return Ok(CliCommand::StopFanCurve);
            }
            let joined = format!("FANCURVE {}", positional[1..].join(" "));
            match parse_fancurve_ipc_command(&joined) {
                Some(awcc_service::daemon::FanCurveIpcCommand::Stop) => {
                    Ok(CliCommand::StopFanCurve)
                }
                Some(awcc_service::daemon::FanCurveIpcCommand::Configure(cfg)) => {
                    Ok(CliCommand::ConfigureFanCurve(cfg))
                }
                None => bail!(
                    "invalid fancurve arguments; expected 4 <temp 20..105> <boost 0..100> pairs"
                ),
            }
        }
        "static" => {
            if positional.len() < 4 {
                bail!("usage: awcc-cli static <R> <G> <B> [--zone ...] [--volatile|--persist]");
            }
            let color = parse_rgb_triplet(&positional[1], &positional[2], &positional[3])?;
            Ok(CliCommand::ApplyLighting {
                effect: LightingEffect::Static { color },
                target: zone_target,
                persist_boot,
            })
        }
        "breathe" | "wave" | "bkf" => {
            if positional.len() < 4 {
                bail!("usage: awcc-cli {cmd} <R> <G> <B> [duration_ms]");
            }
            let color = parse_rgb_triplet(&positional[1], &positional[2], &positional[3])?;
            let duration_ms = positional
                .get(4)
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(500)
                .clamp(50, 10_000);
            let effect = match cmd {
                "breathe" => LightingEffect::Breathe { color, duration_ms },
                "wave" => LightingEffect::Wave { color, duration_ms },
                _ => LightingEffect::BackAndForth { color, duration_ms },
            };
            Ok(CliCommand::ApplyLighting {
                effect,
                target: zone_target,
                persist_boot,
            })
        }
        "spectrum" | "rainbow" => {
            let default_dur = if cmd == "spectrum" { 1100 } else { 500 };
            let duration_ms = positional
                .get(1)
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(default_dur)
                .clamp(50, 10_000);
            let effect = if cmd == "spectrum" {
                LightingEffect::Spectrum { duration_ms }
            } else {
                LightingEffect::Rainbow { duration_ms }
            };
            Ok(CliCommand::ApplyLighting {
                effect,
                target: zone_target,
                persist_boot,
            })
        }
        "defaultblue" => Ok(CliCommand::ApplyLighting {
            effect: LightingEffect::DefaultBlue,
            target: zone_target,
            persist_boot,
        }),
        "brightness" => {
            if let Some(arg1) = positional.get(1) {
                let pct = match arg1.as_str() {
                    "f" | "full" => 100,
                    "h" | "half" => 50,
                    "m" | "medium" => 25,
                    "d" | "dim" => 10,
                    "o" | "off" => 0,
                    other => other.parse::<u8>().context("brightness must be 0..=100")?,
                };
                Ok(CliCommand::SetBrightness(pct.min(100)))
            } else {
                Ok(CliCommand::QueryBrightness)
            }
        }
        "stealth" => {
            if let Some(arg1) = positional.get(1) {
                match arg1.to_ascii_lowercase().as_str() {
                    "on" | "1" | "true" => Ok(CliCommand::SetStealth(true)),
                    "off" | "0" | "false" => Ok(CliCommand::SetStealth(false)),
                    _ => Ok(CliCommand::ToggleStealth),
                }
            } else {
                Ok(CliCommand::ToggleStealth)
            }
        }
        "power-led" => {
            if positional.len() < 4 {
                bail!(
                    "usage: awcc-cli power-led <AC_R> <AC_G> <AC_B> [<DC_R> <DC_G> <DC_B> <CHG_R> <CHG_G> <CHG_B> <CRIT_R> <CRIT_G> <CRIT_B>]"
                );
            }
            let ac_color = parse_rgb_triplet(&positional[1], &positional[2], &positional[3])?;
            let dc_color = if positional.len() >= 7 {
                parse_rgb_triplet(&positional[4], &positional[5], &positional[6])?
            } else {
                ac_color
            };
            let charging_color = if positional.len() >= 10 {
                parse_rgb_triplet(&positional[7], &positional[8], &positional[9])?
            } else {
                ac_color
            };
            let critical_color = if positional.len() >= 13 {
                parse_rgb_triplet(&positional[10], &positional[11], &positional[12])?
            } else {
                0xFF0000
            };
            Ok(CliCommand::SetPowerButtonColors {
                ac_color,
                dc_color,
                charging_color,
                critical_color,
            })
        }
        "raw-status" => {
            let flags = &positional[1..];
            let all = flags.is_empty() || flags.iter().any(|f| f == "--all");
            Ok(CliCommand::RawStatus {
                status: all || flags.iter().any(|f| f == "--status"),
                firmware: all || flags.iter().any(|f| f == "--firmware-version"),
                elc_config: all || flags.iter().any(|f| f == "--elc-config"),
                anim_count: all || flags.iter().any(|f| f == "--animation-count"),
            })
        }
        "test-modes" => Ok(CliCommand::TestModes),
        "test-zones" => Ok(CliCommand::TestZones),
        other => bail!("unknown command `{other}` (run with `--help` for usage)"),
    }
}

/// Format the `--help` output string.
#[must_use]
pub fn format_help() -> String {
    [
        "AWCC (Rust Rewrite) — Alienware Command Center CLI",
        "",
        "Usage: awcc-cli <command> [options]",
        "",
        "Passive Commands (zero hardware writes):",
        "  --help, -h               Show this help message",
        "  device-info              Show resolved device database profile and capabilities",
        "",
        "Thermal & Fan Commands:",
        "  qm                       Query active thermal mode and telemetry",
        "  q | bs | b | c | p | fs  Switch to Quiet / BatterySaver / Balanced / Cool / Performance / FullSpeed",
        "  g | gt                   Enter G-Mode or toggle G-Mode with prior mode restoration",
        "  m                        Switch to Manual thermal mode",
        "  cb / scb <0..100>        Query or set CPU manual fan boost percentage",
        "  gb / sgb <0..100>        Query or set GPU manual fan boost percentage",
        "  getturbo / st <0|1>      Query or set CPU Turbo Boost",
        "  tcc [0..15]              Query or set CPU TCC Offset (°C reduction from TjMax)",
        "  fancurve stop | <4 pts>  Stop or configure custom 4-point temperature/boost fan curve",
        "",
        "Lighting & ELC (187c:0550/0551) Commands:",
        "  static <R> <G> <B>       Apply static RGB color",
        "  breathe <R> <G> <B> [ms] Apply breathing RGB effect with optional duration_ms",
        "  wave <R> <G> <B> [ms]    Apply wave RGB effect with optional duration_ms",
        "  bkf <R> <G> <B> [ms]     Apply back-and-forth RGB effect with optional duration_ms",
        "  spectrum [ms]            Apply spectrum cycle with optional duration_ms",
        "  rainbow [ms]             Apply rainbow wave with optional duration_ms",
        "  defaultblue              Apply Alienware default cyan/blue profile",
        "  brightness [0..100|f|h]  Query or set ELC hardware brightness",
        "  stealth [on|off|toggle]  Control Stealth (Go Dark) lighting mode",
        "  power-led <R> <G> <B>..  Program power-button state slots (0x005B..0x0060) on LED 0x02",
        "  raw-status [--all]       Query ELC 0x20 status, firmware version, and 0x0F01 config",
        "  --zone <all|keyboard|logo|tron|0,1..>  Target specific ELC zone group",
        "  --volatile | --persist   Apply volatilely (live session) or persist to boot slot 0x0061",
    ]
    .join("\n")
}

/// Format the passive `device-info` output without touching ACPI or USB hardware.
#[must_use]
pub fn format_device_info(profile: &DeviceProfile) -> String {
    let modes: Vec<&str> = profile
        .entry
        .supported_thermal_modes()
        .into_iter()
        .map(ThermalMode::display_name)
        .collect();
    let zones: Vec<String> = profile
        .entry
        .keyboard_zones
        .iter()
        .map(|z| format!("0x{z:02X}"))
        .collect();
    [
        format!("Device Name     : {}", profile.device_name),
        format!("CPU Vendor      : {}", profile.cpu_vendor),
        format!(
            "ACPI Prefix     : {} ({})",
            profile.acpi_prefix,
            profile.acpi_prefix.wmax_path()
        ),
        format!("Database Source : {}", profile.database_source),
        format!("Matched in DB   : {}", profile.matched_in_database),
        format!("Thermal Modes   : {}", modes.join(", ")),
        format!("Keyboard Zones  : {}", zones.join(", ")),
    ]
    .join("\n")
}

/// Execute a parsed `CliCommand` against an `ThermalOrchestrator` (only invoked for active commands).
pub fn execute_active_cli_command(
    cmd: CliCommand,
    orch: &mut ThermalOrchestrator,
) -> Result<String> {
    match cmd {
        CliCommand::Help => Ok(format_help()),
        CliCommand::DeviceInfo => {
            let snap = orch.query_snapshot();
            Ok(format_device_info(&snap.profile))
        }
        CliCommand::QueryMode => {
            let snap = orch.query_snapshot();
            Ok(format!(
                "Current Thermal Mode: {} (GameShift: {}, CPU: {}, GPU: {}, TCC Offset: {})",
                snap.thermal_mode_state.display_label(),
                snap.game_shift_enabled,
                snap.telemetry.cpu_temp_c.formatted_value(),
                snap.telemetry.gpu_temp_c.formatted_value(),
                snap.tcc_offset_c.formatted_value()
            ))
        }
        CliCommand::SetMode(mode) => {
            let snap = orch.handle_command(ServiceCommand::SetThermalMode(mode));
            match snap.lifecycle {
                OperationLifecycle::Confirmed { summary } => Ok(summary),
                OperationLifecycle::RolledBack { summary, reason } => {
                    bail!("{summary}: {reason}")
                }
                OperationLifecycle::Failed { summary, error } => bail!("{summary}: {error}"),
                _ => Ok(format!("Mode set to {mode}")),
            }
        }
        CliCommand::ToggleGMode => {
            let snap = orch.handle_command(ServiceCommand::ToggleGMode);
            match snap.lifecycle {
                OperationLifecycle::Confirmed { summary } => Ok(summary),
                OperationLifecycle::RolledBack { summary, reason } => {
                    bail!("{summary}: {reason}")
                }
                OperationLifecycle::Failed { summary, error } => bail!("{summary}: {error}"),
                _ => Ok("Toggled G-Mode".to_string()),
            }
        }
        CliCommand::QueryCpuBoost => {
            let snap = orch.query_snapshot();
            Ok(format!(
                "CPU Fan Boost: {}",
                snap.telemetry.cpu_boost_pct.formatted_value()
            ))
        }
        CliCommand::SetCpuBoost(cpu_pct) => {
            let cur_gpu = match orch.query_snapshot().telemetry.gpu_boost_pct {
                awcc_core::SensorReading::Valid { value, .. } => value,
                awcc_core::SensorReading::Stale { .. }
                | awcc_core::SensorReading::Unavailable { .. } => cpu_pct,
            };
            let snap = orch.handle_command(ServiceCommand::ApplyManualBoost {
                cpu_boost_pct: cpu_pct,
                gpu_boost_pct: cur_gpu,
            });
            Ok(format!(
                "CPU Fan Boost set to {}%",
                snap.telemetry.cpu_boost_pct.formatted_value()
            ))
        }
        CliCommand::QueryGpuBoost => {
            let snap = orch.query_snapshot();
            Ok(format!(
                "GPU Fan Boost: {}",
                snap.telemetry.gpu_boost_pct.formatted_value()
            ))
        }
        CliCommand::SetGpuBoost(gpu_pct) => {
            let cur_cpu = match orch.query_snapshot().telemetry.cpu_boost_pct {
                awcc_core::SensorReading::Valid { value, .. } => value,
                awcc_core::SensorReading::Stale { .. }
                | awcc_core::SensorReading::Unavailable { .. } => gpu_pct,
            };
            let snap = orch.handle_command(ServiceCommand::ApplyManualBoost {
                cpu_boost_pct: cur_cpu,
                gpu_boost_pct: gpu_pct,
            });
            Ok(format!(
                "GPU Fan Boost set to {}%",
                snap.telemetry.gpu_boost_pct.formatted_value()
            ))
        }
        CliCommand::QueryTurbo => {
            let snap = orch.query_snapshot();
            Ok(format!("CPU Turbo Boost: {}", snap.cpu_turbo_enabled))
        }
        CliCommand::SetTurbo(enabled) => {
            let _ = orch.handle_command(ServiceCommand::SetCpuTurbo(enabled));
            Ok(format!("CPU Turbo Boost set to {enabled}"))
        }
        CliCommand::QueryTccOffset => {
            let snap = orch.query_snapshot();
            Ok(format!(
                "CPU TCC Offset: {}",
                snap.tcc_offset_c.formatted_value()
            ))
        }
        CliCommand::SetTccOffset(offset) => {
            let snap = orch.handle_command(ServiceCommand::SetTccOffset(offset));
            match snap.lifecycle {
                OperationLifecycle::Confirmed { summary } => Ok(summary),
                OperationLifecycle::Failed { summary, error } => bail!("{summary}: {error}"),
                _ => Ok(format!("CPU TCC Offset set to {offset}°C")),
            }
        }
        CliCommand::StopFanCurve => {
            let cfg = FanCurveConfig::default();
            let snap = orch.handle_command(ServiceCommand::SetFanCurve(cfg));
            Ok(format!(
                "Custom fan curve stopped (mode: {})",
                snap.thermal_mode_state.display_label()
            ))
        }
        CliCommand::ConfigureFanCurve(cfg) => {
            let snap = orch.handle_command(ServiceCommand::SetFanCurve(cfg));
            Ok(format!(
                "Custom fan curve active (mode: {})",
                snap.thermal_mode_state.display_label()
            ))
        }
        CliCommand::ApplyLighting {
            effect,
            target,
            persist_boot,
        } => {
            let snap = orch.handle_command(ServiceCommand::ApplyLighting {
                effect,
                target,
                persist_boot,
            });
            match snap.lifecycle {
                OperationLifecycle::Confirmed { summary } => Ok(summary),
                OperationLifecycle::Failed { summary, error } => bail!("{summary}: {error}"),
                _ => Ok("Lighting applied".to_string()),
            }
        }
        CliCommand::QueryBrightness => {
            let snap = orch.query_snapshot();
            Ok(format!("ELC Brightness: {}%", snap.lighting_brightness_pct))
        }
        CliCommand::SetBrightness(pct) => {
            let snap = orch.handle_command(ServiceCommand::SetBrightness(pct));
            Ok(format!(
                "ELC Brightness set to {}%",
                snap.lighting_brightness_pct
            ))
        }
        CliCommand::ToggleStealth => {
            let snap = orch.handle_command(ServiceCommand::ToggleStealth);
            Ok(format!("Stealth Mode: {}", snap.lighting_stealth_enabled))
        }
        CliCommand::SetStealth(enable) => {
            let snap = orch.handle_command(ServiceCommand::SetStealth(enable));
            Ok(format!("Stealth Mode: {}", snap.lighting_stealth_enabled))
        }
        CliCommand::SetPowerButtonColors {
            ac_color,
            dc_color,
            charging_color,
            critical_color,
        } => {
            let snap = orch.handle_command(ServiceCommand::SetPowerButtonColors {
                ac_color,
                dc_color,
                charging_color,
                critical_color,
            });
            match snap.lifecycle {
                OperationLifecycle::Confirmed { summary } => Ok(summary),
                OperationLifecycle::Failed { summary, error } => bail!("{summary}: {error}"),
                _ => Ok("Power button colors programmed".to_string()),
            }
        }
        CliCommand::RawStatus {
            status,
            firmware,
            elc_config,
            anim_count,
        } => {
            let diag = orch
                .refresh_elc_diagnostics()
                .or_else(|| orch.query_snapshot().elc_diagnostics);
            let Some(diag) = diag else {
                bail!("ELC diagnostics unavailable");
            };
            let mut lines = Vec::new();
            if status {
                lines.push(format!("Status          : 0x{:02X}", diag.status_byte));
            }
            if firmware {
                lines.push(format!(
                    "Firmware Version: {}.{}.{}",
                    diag.firmware_version.0, diag.firmware_version.1, diag.firmware_version.2
                ));
            }
            if elc_config {
                lines.push(format!(
                    "ELC Config      : platform=0x{:04X}, active_zones={}",
                    diag.elc_config.platform_id, diag.elc_config.active_zones
                ));
            }
            if anim_count {
                lines.push(format!("Animation Count : {}", diag.animation_count));
            }
            Ok(lines.join("\n"))
        }
        CliCommand::TestModes => {
            let snap = orch.query_snapshot();
            Ok(format!(
                "Supported thermalModes bitmap for `{}`: {:08b}",
                snap.profile.device_name, snap.profile.entry.thermal_modes
            ))
        }
        CliCommand::TestZones => {
            let snap = orch.query_snapshot();
            let zones: Vec<String> = snap
                .profile
                .entry
                .keyboard_zones
                .iter()
                .map(|z| format!("0x{z:02x}"))
                .collect();
            Ok(format!("Configured keyboardZones: [{}]", zones.join(", ")))
        }
        CliCommand::DaemonMode => Ok("awccd daemon mode ready".to_string()),
        CliCommand::GuiMode => Ok("awcc-gui mode ready".to_string()),
    }
}

fn main() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let sim_mode = raw_args.iter().any(|a| a == "--sim" || a == "--test-mode");
    let cmd = parse_cli_args(&raw_args)?;

    // Passive commands must never instantiate USB or ACPI hardware!
    match cmd {
        CliCommand::Help => {
            println!("{}", format_help());
            return Ok(());
        }
        CliCommand::DeviceInfo => {
            let profile = if sim_mode {
                awcc_platform::resolve_simulated_m15_r6_profile()
                    .context("failed to resolve simulated device profile from database")?
            } else {
                awcc_platform::resolve_host_device_profile()
                    .context("failed to resolve host device profile from database")?
            };
            println!("{}", format_device_info(&profile));
            return Ok(());
        }
        other => {
            let mut orch = if sim_mode {
                ThermalOrchestrator::new_simulated_m15_r6()
                    .context("failed to initialize simulated AWCC orchestrator")?
            } else {
                ThermalOrchestrator::new_hardware()
                    .context("failed to initialize hardware AWCC orchestrator")?
            };
            let out = execute_active_cli_command(other, &mut orch)?;
            println!("{out}");
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn to_args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn passive_cli_commands_parse_and_format_without_hardware() {
        assert_eq!(
            parse_cli_args(&to_args(&["--help"])).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            parse_cli_args(&to_args(&["device-info"])).unwrap(),
            CliCommand::DeviceInfo
        );

        let profile = DeviceProfile::alienware_m15_r6().unwrap();
        let info = format_device_info(&profile);
        assert!(info.contains("Alienware m15 R6"));
        assert!(info.contains("AMWW"));
        assert!(info.contains("Full Speed"));
        assert!(info.contains("Manual"));
    }

    #[test]
    fn active_cli_commands_execute_thermal_tcc_fancurve_and_lighting_operations() {
        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().unwrap();

        // 1. Set thermal mode FullSpeed (`fs`)
        let cmd_fs = parse_cli_args(&to_args(&["fs"])).unwrap();
        let out_fs = execute_active_cli_command(cmd_fs, &mut orch).unwrap();
        assert!(out_fs.contains("Full Speed"));

        // 2. Set TCC Offset (`tcc 10`)
        let cmd_tcc = parse_cli_args(&to_args(&["tcc", "10"])).unwrap();
        let out_tcc = execute_active_cli_command(cmd_tcc, &mut orch).unwrap();
        assert!(out_tcc.contains("10°C"));

        // 3. Configure custom fan curve (`fancurve 45 10 60 40 75 75 85 100 3`)
        let cmd_fc = parse_cli_args(&to_args(&[
            "fancurve", "45", "10", "60", "40", "75", "75", "85", "100", "3",
        ]))
        .unwrap();
        let out_fc = execute_active_cli_command(cmd_fc, &mut orch).unwrap();
        assert!(out_fc.contains("Custom fan curve active"));

        // 4. Apply volatile static lighting to keyboard zone group
        let cmd_light = parse_cli_args(&to_args(&[
            "static",
            "0",
            "240",
            "255",
            "--zone",
            "keyboard",
            "--volatile",
        ]))
        .unwrap();
        let out_light = execute_active_cli_command(cmd_light, &mut orch).unwrap();
        assert!(out_light.contains("Keyboard (4-Zone)"));
        assert!(out_light.contains("volatile session"));

        // 5. Query raw-status
        let cmd_raw = parse_cli_args(&to_args(&["raw-status", "--all"])).unwrap();
        let out_raw = execute_active_cli_command(cmd_raw, &mut orch).unwrap();
        assert!(out_raw.contains("platform=0x0F01"));
        assert!(out_raw.contains("2.0.5"));
    }
}
