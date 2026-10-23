//! Command-line interface for AWCC Linux thermal, fan, lighting and service control.
//!
//! Guarantees that passive commands (`--help`, `-h`, `--version`, `-V`, `device-info`) execute
//! with zero ACPI or USB hardware touches, and that `--help`/`--version` are answered before any
//! backend (profile resolution, orchestrator, transport) is even constructed.
//!
//! # Result contract
//!
//! * Machine-readable results go to **stdout**; diagnostics, notes, and errors go to **stderr**.
//! * A state-changing command exits `0` only when its own operation was `Accepted` or
//!   `Confirmed`; an `Accepted` result carries a `note:` on stderr explaining what is not
//!   confirmed. `Failed`/`RolledBack` operations, unreadable single-value queries, and backend
//!   start-up failures exit `1` with an `error:` line on stderr and nothing on stdout.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use awcc_core::{
    DeviceProfile, FanCurveConfig, SensorReading, ThermalMode, ThermalModeState,
    lighting::{LightingEffect, ZoneTarget},
    validate_fan_boost, validate_tcc_offset,
};
use awcc_diagnostics::{
    DiagnosticsConfig, DiagnosticsRuntime, Severity, default_log_dir, global_diagnostics,
    init_global_diagnostics, install_panic_hook, resolve_log_level, resolve_raw_usb_opt_in,
};
use awcc_service::{
    CommandOutcome, ServiceCommand, ThermalOrchestrator, daemon::parse_fancurve_ipc_command,
    installer::ServiceAction,
};

/// Default output archive path for `awcc-cli diagnostics export`.
pub const DEFAULT_SUPPORT_ARCHIVE_FILENAME: &str = "awcc-support.zip";

/// Parsed CLI action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Help,
    Version,
    DeviceInfo,
    DiagnosticsRecording {
        seconds: u16,
    },
    DiagnosticsSummary {
        json: bool,
    },
    DiagnosticsExport {
        output: PathBuf,
        json: bool,
    },
    ServiceStatus {
        json: bool,
    },
    ServiceManage {
        action: ServiceAction,
        json: bool,
    },
    ServiceApply {
        action: ServiceAction,
    },
    ServiceAuthorize {
        user: String,
        authorized: bool,
    },
    ServiceAuthorizedUsers {
        json: bool,
    },
    ServiceRestoreAtStartup(bool),
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

/// Complete parsed CLI invocation including logging and diagnostic flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliInvocation {
    pub command: CliCommand,
    pub sim_mode: bool,
    pub cli_log_level: Option<String>,
    pub cli_verbose: bool,
    pub cli_trace_raw_usb: bool,
}

fn parse_rgb_triplet(r_s: &str, g_s: &str, b_s: &str) -> Result<u32> {
    let r = r_s.parse::<u8>().context("invalid R component (0..=255)")?;
    let g = g_s.parse::<u8>().context("invalid G component (0..=255)")?;
    let b = b_s.parse::<u8>().context("invalid B component (0..=255)")?;
    Ok((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
}

/// Parse command-line arguments into a `CliCommand` without touching hardware.
pub fn parse_cli_args(args: &[String]) -> Result<CliCommand> {
    parse_cli_invocation(args).map(|inv| inv.command)
}

/// Parse command-line arguments and diagnostic/logging flags without touching hardware.
pub fn parse_cli_invocation(args: &[String]) -> Result<CliInvocation> {
    if args.is_empty() {
        return Ok(CliInvocation {
            command: CliCommand::Help,
            sim_mode: false,
            cli_log_level: None,
            cli_verbose: false,
            cli_trace_raw_usb: false,
        });
    }

    // Fast-path scan for `--help` / `--version` so they always succeed immediately.
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                return Ok(CliInvocation {
                    command: CliCommand::Help,
                    sim_mode: false,
                    cli_log_level: None,
                    cli_verbose: false,
                    cli_trace_raw_usb: false,
                });
            }
            "-V" | "--version" => {
                return Ok(CliInvocation {
                    command: CliCommand::Version,
                    sim_mode: false,
                    cli_log_level: None,
                    cli_verbose: false,
                    cli_trace_raw_usb: false,
                });
            }
            _ => {}
        }
    }

    let mut zone_target = ZoneTarget::All;
    let mut persist_boot = true;
    let mut sim_mode = false;
    let mut cli_log_level: Option<String> = None;
    let mut cli_verbose = false;
    let mut cli_trace_raw_usb = false;
    let mut positional = Vec::new();
    let mut launcher = None;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(level_val) = arg.strip_prefix("--log-level=") {
            cli_log_level = Some(level_val.to_string());
            i += 1;
            continue;
        }
        match arg {
            "help" if positional.is_empty() => {
                return Ok(CliInvocation {
                    command: CliCommand::Help,
                    sim_mode,
                    cli_log_level,
                    cli_verbose,
                    cli_trace_raw_usb,
                });
            }
            "--test-mode" | "--sim" => sim_mode = true,
            "-v" | "--verbose" => cli_verbose = true,
            "--trace-raw-usb" => cli_trace_raw_usb = true,
            "--log-level" => {
                i += 1;
                let val = args.get(i).context(
                    "missing value for `--log-level` (expected error|warn|info|debug|trace)",
                )?;
                cli_log_level = Some(val.clone());
            }
            "-d" | "--daemon" => {
                if launcher.replace(CliCommand::DaemonMode).is_some() {
                    bail!("choose only one of --daemon or --gui");
                }
            }
            "-g" | "--gui" => {
                if launcher.replace(CliCommand::GuiMode).is_some() {
                    bail!("choose only one of --daemon or --gui");
                }
            }
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

    // Validate `--log-level` explicitly if provided on the CLI.
    if let Some(level_str) = &cli_log_level {
        resolve_log_level(Some(level_str.as_str()), cli_verbose, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    if let Some(command) = launcher {
        if !positional.is_empty() {
            bail!("--daemon and --gui cannot be combined with a command or extra arguments");
        }
        return Ok(CliInvocation {
            command,
            sim_mode,
            cli_log_level,
            cli_verbose,
            cli_trace_raw_usb,
        });
    }

    let Some(cmd) = positional.first().map(String::as_str) else {
        return Ok(CliInvocation {
            command: CliCommand::Help,
            sim_mode,
            cli_log_level,
            cli_verbose,
            cli_trace_raw_usb,
        });
    };

    let command = match cmd {
        "device-info" => Ok(CliCommand::DeviceInfo),
        "diagnostics" | "diag" => parse_diagnostics_subcommand(&positional[1..]),
        "service" => parse_service_subcommand(&positional[1..], false),
        "service-apply" => parse_service_subcommand(&positional[1..], true),
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
                Ok(CliCommand::StopFanCurve)
            } else {
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
    }?;

    Ok(CliInvocation {
        command,
        sim_mode,
        cli_log_level,
        cli_verbose,
        cli_trace_raw_usb,
    })
}

fn parse_service_subcommand(rest: &[String], elevated: bool) -> Result<CliCommand> {
    let Some(sub) = rest.first().map(String::as_str) else {
        bail!(
            "usage: awcc-cli service status|install|setup|update|uninstall|authorize|revoke|authorized-users|restore-at-startup"
        );
    };
    if !elevated && matches!(sub, "authorize" | "revoke" | "restore-at-startup") {
        if rest.len() != 2 {
            bail!(
                "usage: awcc-cli service {sub} <{}>",
                if sub == "restore-at-startup" {
                    "on|off"
                } else {
                    "user-or-uid"
                }
            );
        }
        return if sub == "restore-at-startup" {
            match rest[1].as_str() {
                "on" => Ok(CliCommand::ServiceRestoreAtStartup(true)),
                "off" => Ok(CliCommand::ServiceRestoreAtStartup(false)),
                _ => bail!("restore-at-startup expects on or off"),
            }
        } else {
            Ok(CliCommand::ServiceAuthorize {
                user: rest[1].clone(),
                authorized: sub == "authorize",
            })
        };
    }
    let mut json = false;
    let mut purge = false;
    for arg in &rest[1..] {
        match arg.as_str() {
            "--json" if !elevated && !json => json = true,
            "--purge" if sub == "uninstall" && !purge => purge = true,
            _ => bail!("unknown or duplicate service {sub} option `{arg}`"),
        }
    }
    let action = match sub {
        "status" if !elevated => return Ok(CliCommand::ServiceStatus { json }),
        "authorized-users" if !elevated => {
            return Ok(CliCommand::ServiceAuthorizedUsers { json });
        }
        "install" => ServiceAction::Install,
        "setup" => ServiceAction::Setup,
        "update" => ServiceAction::Update,
        "uninstall" => ServiceAction::Uninstall { purge },
        _ => bail!("unknown service subcommand `{sub}`"),
    };
    Ok(if elevated {
        CliCommand::ServiceApply { action }
    } else {
        CliCommand::ServiceManage { action, json }
    })
}

fn parse_diagnostics_subcommand(rest: &[String]) -> Result<CliCommand> {
    let mut sub = "summary";
    let mut idx = 0;
    if let Some(first) = rest.first().map(String::as_str)
        && !first.starts_with('-')
    {
        sub = first;
        idx = 1;
    }

    match sub {
        "record" => {
            let seconds = match &rest[idx..] {
                [] => 300,
                [flag, value] if flag == "--seconds" => value
                    .parse::<u16>()
                    .context("recording duration must be 1..=300 seconds")?,
                _ => bail!("usage: awcc-cli diagnostics record [--seconds 1..300]"),
            };
            if !(1..=300).contains(&seconds) {
                bail!("recording duration must be 1..=300 seconds");
            }
            Ok(CliCommand::DiagnosticsRecording { seconds })
        }
        "stop-recording" if idx == rest.len() => {
            Ok(CliCommand::DiagnosticsRecording { seconds: 0 })
        }
        "summary" | "status" => {
            let mut json = false;
            while idx < rest.len() {
                match rest[idx].as_str() {
                    "--json" => json = true,
                    other => bail!(
                        "unknown `diagnostics summary` option `{other}` (usage: `awcc-cli diagnostics summary [--json]`)"
                    ),
                }
                idx += 1;
            }
            Ok(CliCommand::DiagnosticsSummary { json })
        }
        "export" => {
            let mut output: Option<PathBuf> = None;
            let mut json = false;
            while idx < rest.len() {
                let arg = rest[idx].as_str();
                if let Some(val) = arg.strip_prefix("--output=") {
                    if val.trim().is_empty() {
                        bail!("`--output` path must not be empty");
                    }
                    output = Some(PathBuf::from(val));
                } else {
                    match arg {
                        "-o" | "--output" => {
                            idx += 1;
                            let val = rest
                                .get(idx)
                                .context("missing archive path for `--output`")?;
                            if val.trim().is_empty() {
                                bail!("`--output` path must not be empty");
                            }
                            output = Some(PathBuf::from(val));
                        }
                        "--json" => json = true,
                        other if !other.starts_with('-') && output.is_none() => {
                            output = Some(PathBuf::from(other));
                        }
                        other => bail!(
                            "unknown `diagnostics export` option `{other}` (usage: `awcc-cli diagnostics export [--output awcc-support.zip] [--json]`)"
                        ),
                    }
                }
                idx += 1;
            }
            Ok(CliCommand::DiagnosticsExport {
                output: output.unwrap_or_else(|| PathBuf::from(DEFAULT_SUPPORT_ARCHIVE_FILENAME)),
                json,
            })
        }
        other => {
            bail!(
                "unknown `diagnostics` subcommand `{other}` (expected `export`, `summary`, `record` or `stop-recording`)"
            )
        }
    }
}

/// Format the `--help` output string.
#[must_use]
pub fn format_help() -> String {
    [
        "AWCC Linux — thermal, fan and lighting control",
        "",
        "Usage: awcc-cli <command> [options]",
        "",
        "Passive Commands (zero hardware probes or writes):",
        "  --help, -h               Show this help message",
        "  --version, -V            Show version, source revision, target, toolchain, and package format",
        "  device-info              Show resolved device database profile and capabilities",
        "  diagnostics summary      Print passive redacted diagnostic summary (`--json` for report.json)",
        "  diagnostics export [--output awcc-support.zip]",
        "                           Export bounded passive support archive (.zip) without probing hardware",
        "  service status [--json]  Show installation ownership and daemon availability without prompting",
        "  service authorized-users [--json]  List users authorized to control the daemon",
        "",
        "Background Service:",
        "  service install          Install/start the bundled service (requests administrator authorization)",
        "  service setup            Authorize this account and enable/start an installed service",
        "  service update           Update an installer-managed service; native packages use their package manager",
        "  service uninstall [--purge]  Remove an installer-managed service; --purge also deletes its settings",
        "  service authorize <user-or-uid>  Allow a desktop user to control hardware (run with sudo)",
        "  service revoke <user-or-uid>     Revoke a user's access (run with sudo)",
        "  service restore-at-startup <on|off>  Opt into or out of restoring saved curve intent",
        "  -d, --daemon             Run the privileged daemon (normally launched by systemd)",
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
        "",
        "Logging & Diagnostics Options (precedence: --log-level > -v > AWCC_LOG > info):",
        "  --log-level <level>      Set log filter level: error | warn | info | debug | trace",
        "  diagnostics record [--seconds 1..300]  Record daemon DEBUG events temporarily",
        "  diagnostics stop-recording  Stop daemon DEBUG recording",
        "  -v, --verbose            Enable verbose DEBUG diagnostics on stderr",
        "  --trace-raw-usb          Opt in to verbose raw 33-byte USB input report hex in audit/exports",
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

/// Result of an active command: what goes to stdout and what goes to stderr.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOutput {
    /// The machine-readable result line(s).
    pub stdout: String,
    /// Notes/warnings that qualify the result (printed to stderr as `note:` lines).
    pub notes: Vec<String>,
}

impl CliOutput {
    fn line(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            notes: Vec::new(),
        }
    }
}

/// Map the outcome of one state-changing command onto stdout/stderr.
///
/// The result is derived from the operation recorded by *this* command; a failure becomes an
/// `Err` (non-zero exit status), and an `Accepted` result keeps its caveat as a note.
fn finish_operation(outcome: CommandOutcome) -> Result<CliOutput> {
    let result = outcome
        .result
        .context("internal error: a state-changing command produced no operation result")?;
    let success = result?;
    let mut notes = Vec::new();
    if let Some(caveat) = success.caveat() {
        notes.push(format!("not device-confirmed: {caveat}"));
    }
    Ok(CliOutput {
        stdout: success.summary,
        notes,
    })
}

fn describe_sensor<T: std::fmt::Display + Copy>(reading: &SensorReading<T>) -> String {
    reading.formatted_value()
}

fn unreadable<T: std::fmt::Display + Copy>(
    what: &str,
    reading: &SensorReading<T>,
) -> anyhow::Error {
    anyhow::anyhow!(
        "{what} unavailable: {}",
        reading.problem().unwrap_or("not reported")
    )
}

/// Execute passive `diagnostics summary` or `diagnostics export` without constructing any
/// hardware or simulated backends.
fn execute_passive_diagnostics_command(
    cmd: &CliCommand,
    inv: &CliInvocation,
    log_dir: Option<&Path>,
    service_evidence: awcc_diagnostics::service::ServiceSupport,
) -> Result<CliOutput> {
    let resolved_level = resolve_log_level(
        inv.cli_log_level.as_deref(),
        inv.cli_verbose,
        std::env::var("AWCC_LOG").ok().as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let trace_raw_usb = resolve_raw_usb_opt_in(
        inv.cli_trace_raw_usb,
        std::env::var("AWCC_TRACE_RAW_USB").ok().as_deref(),
    );

    let mut cfg = DiagnosticsConfig::from_env_and_cli("awcc-cli", resolved_level, trace_raw_usb);
    cfg.enable_file_sink = false;
    cfg.log_dir = log_dir.map(Path::to_path_buf);
    cfg.enable_stderr_console = false;
    let runtime = DiagnosticsRuntime::new(cfg);

    let mut extra_settings = BTreeMap::new();
    extra_settings.insert(
        "cli_mode".to_string(),
        if inv.sim_mode { "simulated" } else { "default" }.to_string(),
    );
    let mut bundle = runtime
        .assemble_support_bundle(None, extra_settings)
        .context("failed to assemble passive support bundle")?;
    bundle.attach_service_support(service_evidence)?;

    match cmd {
        CliCommand::DiagnosticsSummary { json: true } => Ok(CliOutput::line(bundle.report_json)),
        CliCommand::DiagnosticsSummary { json: false } => Ok(CliOutput::line(bundle.summary_txt)),
        CliCommand::DiagnosticsExport { output, json } => {
            let bytes = bundle.write_to_path(output).with_context(|| {
                format!("failed to write support archive to {}", output.display())
            })?;
            let origin = bundle.report.evidence.origin.as_str();
            if *json {
                let payload = serde_json::json!({
                    "status": "exported",
                    "output": output.display().to_string(),
                    "bytes": bytes,
                    "schema_version": bundle.report.schema_version,
                    "export_mode": bundle.report.export_mode,
                    "evidence_origin": origin,
                    "observation_age_ms": bundle.report.evidence.observation_age_ms,
                });
                Ok(CliOutput::line(serde_json::to_string_pretty(&payload)?))
            } else {
                let mut notes = Vec::new();
                notes.push(format!(
                    "passive export completed with zero hardware queries (evidence origin: {origin})"
                ));
                Ok(CliOutput {
                    stdout: format!(
                        "Exported support report to {} ({bytes} bytes, schema v{}, origin={origin})",
                        output.display(),
                        bundle.report.schema_version
                    ),
                    notes,
                })
            }
        }
        _ => unreachable!("only called for passive diagnostics commands"),
    }
}

/// Execute a parsed `CliCommand` against an `ThermalOrchestrator` (only invoked for active commands).
pub fn execute_active_cli_command(
    cmd: CliCommand,
    orch: &mut ThermalOrchestrator,
) -> Result<CliOutput> {
    match cmd {
        CliCommand::Help => Ok(CliOutput::line(format_help())),
        CliCommand::Version => Ok(CliOutput::line(awcc_build_info::version_text("awcc-cli"))),
        CliCommand::DeviceInfo => {
            let snap = orch.query_snapshot();
            Ok(CliOutput::line(format_device_info(&snap.profile)))
        }
        CliCommand::DiagnosticsSummary { .. } | CliCommand::DiagnosticsExport { .. } => {
            let default_inv = CliInvocation {
                command: cmd.clone(),
                sim_mode: false,
                cli_log_level: None,
                cli_verbose: false,
                cli_trace_raw_usb: false,
            };
            execute_passive_diagnostics_command(
                &cmd,
                &default_inv,
                default_log_dir().as_deref(),
                awcc_diagnostics::service::ServiceSupport {
                    state: "unavailable".into(),
                    gaps: vec!["No service evidence collector was supplied".into()],
                    ..Default::default()
                },
            )
        }
        CliCommand::QueryMode => {
            let snap = orch.query_snapshot();
            if let ThermalModeState::Unknown {
                raw_code: None,
                reason,
            } = &snap.thermal_mode_state
            {
                bail!("thermal mode query failed: {reason}");
            }
            let mut notes = Vec::new();
            if let ThermalModeState::Unknown {
                raw_code: Some(code),
                reason,
            } = &snap.thermal_mode_state
            {
                notes.push(format!(
                    "thermal mode 0x{code:02X} is not recognised: {reason}"
                ));
            }
            if let Some(why) = snap.game_shift.problem() {
                notes.push(format!("GameShift state is not current: {why}"));
            }
            for (name, reading) in [
                ("CPU temperature", &snap.telemetry.cpu_temp_c),
                ("GPU temperature", &snap.telemetry.gpu_temp_c),
            ] {
                if let Some(why) = reading.problem() {
                    notes.push(format!("{name} is not current: {why}"));
                }
            }
            if let Some(why) = snap.tcc_offset_c.problem() {
                notes.push(format!("TCC offset is not current: {why}"));
            }
            Ok(CliOutput {
                stdout: format!(
                    "Current Thermal Mode: {} (GameShift: {}, CPU: {}, GPU: {}, TCC Offset: {})",
                    snap.thermal_mode_state.display_label(),
                    snap.game_shift.describe_with(|v| v.to_string()),
                    describe_sensor(&snap.telemetry.cpu_temp_c),
                    describe_sensor(&snap.telemetry.gpu_temp_c),
                    describe_sensor(&snap.tcc_offset_c),
                ),
                notes,
            })
        }
        CliCommand::SetMode(mode) => {
            finish_operation(orch.execute(ServiceCommand::SetThermalMode(mode)))
        }
        CliCommand::ToggleGMode => finish_operation(orch.execute(ServiceCommand::ToggleGMode)),
        CliCommand::QueryCpuBoost => {
            let snap = orch.query_snapshot();
            match snap.telemetry.cpu_boost_pct.fresh_value() {
                Some(v) => Ok(CliOutput::line(format!("CPU Fan Boost: {v}%"))),
                None => Err(unreadable("CPU fan boost", &snap.telemetry.cpu_boost_pct)),
            }
        }
        CliCommand::SetCpuBoost(cpu_pct) => {
            let snap = orch.query_snapshot();
            let Some(cur_gpu) = snap.telemetry.gpu_boost_pct.fresh_value() else {
                bail!(
                    "cannot set the CPU fan boost: the current GPU boost could not be read ({}); both fans are written together and the GPU value will not be guessed",
                    snap.telemetry
                        .gpu_boost_pct
                        .problem()
                        .unwrap_or("not reported")
                );
            };
            finish_operation(orch.execute(ServiceCommand::ApplyManualBoost {
                cpu_boost_pct: cpu_pct,
                gpu_boost_pct: cur_gpu,
            }))
        }
        CliCommand::QueryGpuBoost => {
            let snap = orch.query_snapshot();
            match snap.telemetry.gpu_boost_pct.fresh_value() {
                Some(v) => Ok(CliOutput::line(format!("GPU Fan Boost: {v}%"))),
                None => Err(unreadable("GPU fan boost", &snap.telemetry.gpu_boost_pct)),
            }
        }
        CliCommand::SetGpuBoost(gpu_pct) => {
            let snap = orch.query_snapshot();
            let Some(cur_cpu) = snap.telemetry.cpu_boost_pct.fresh_value() else {
                bail!(
                    "cannot set the GPU fan boost: the current CPU boost could not be read ({}); both fans are written together and the CPU value will not be guessed",
                    snap.telemetry
                        .cpu_boost_pct
                        .problem()
                        .unwrap_or("not reported")
                );
            };
            finish_operation(orch.execute(ServiceCommand::ApplyManualBoost {
                cpu_boost_pct: cur_cpu,
                gpu_boost_pct: gpu_pct,
            }))
        }
        CliCommand::QueryTurbo => {
            let snap = orch.query_snapshot();
            match snap.cpu_turbo.fresh() {
                Some(enabled) => Ok(CliOutput::line(format!("CPU Turbo Boost: {enabled}"))),
                None => bail!(
                    "CPU Turbo Boost state unavailable: {}",
                    snap.cpu_turbo.problem().unwrap_or("not observed")
                ),
            }
        }
        CliCommand::SetTurbo(enabled) => {
            finish_operation(orch.execute(ServiceCommand::SetCpuTurbo(enabled)))
        }
        CliCommand::QueryTccOffset => {
            let snap = orch.query_snapshot();
            match snap.tcc_offset_c.fresh_value() {
                Some(v) => Ok(CliOutput::line(format!("CPU TCC Offset: {v}°C"))),
                None => Err(unreadable("CPU TCC offset", &snap.tcc_offset_c)),
            }
        }
        CliCommand::SetTccOffset(offset) => {
            finish_operation(orch.execute(ServiceCommand::SetTccOffset(offset)))
        }
        CliCommand::StopFanCurve => {
            finish_operation(orch.execute(ServiceCommand::SetFanCurve(FanCurveConfig::default())))
        }
        CliCommand::ConfigureFanCurve(cfg) => {
            let outcome = orch.execute(ServiceCommand::SetFanCurve(cfg));
            let service_owned = outcome.snapshot.service_control.is_some();
            let mut out = finish_operation(outcome)?;
            let note = if service_owned {
                "the background service owns this curve and manages it after this command exits"
            } else {
                "this process exits after this command; the curve will not keep following temperatures"
            };
            out.notes.push(note.to_string());
            Ok(out)
        }
        CliCommand::ApplyLighting {
            effect,
            target,
            persist_boot,
        } => finish_operation(orch.execute(ServiceCommand::ApplyLighting {
            effect,
            target,
            persist_boot,
        })),
        CliCommand::QueryBrightness => {
            let snap = orch.query_snapshot();
            let mut out =
                CliOutput::line(format!("ELC Brightness: {}%", snap.lighting_brightness_pct));
            out.notes.push(
                "brightness is a remembered value, not a device readback (the ELC protocol has none)"
                    .to_string(),
            );
            Ok(out)
        }
        CliCommand::SetBrightness(pct) => {
            finish_operation(orch.execute(ServiceCommand::SetBrightness(pct)))
        }
        CliCommand::ToggleStealth => finish_operation(orch.execute(ServiceCommand::ToggleStealth)),
        CliCommand::SetStealth(enable) => {
            finish_operation(orch.execute(ServiceCommand::SetStealth(enable)))
        }
        CliCommand::SetPowerButtonColors {
            ac_color,
            dc_color,
            charging_color,
            critical_color,
        } => finish_operation(orch.execute(ServiceCommand::SetPowerButtonColors {
            ac_color,
            dc_color,
            charging_color,
            critical_color,
        })),
        CliCommand::RawStatus {
            status,
            firmware,
            elc_config,
            anim_count,
        } => {
            let diag = orch
                .refresh_elc_diagnostics()
                .map_err(|e| anyhow::anyhow!("ELC diagnostics unavailable: {e}"))?;
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
            Ok(CliOutput::line(lines.join("\n")))
        }
        CliCommand::TestModes => {
            let snap = orch.query_snapshot();
            Ok(CliOutput::line(format!(
                "Supported thermalModes bitmap for `{}`: {:08b}",
                snap.profile.device_name, snap.profile.entry.thermal_modes
            )))
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
            Ok(CliOutput::line(format!(
                "Configured keyboardZones: [{}]",
                zones.join(", ")
            )))
        }
        CliCommand::DaemonMode => {
            bail!("daemon mode is dispatched before command execution")
        }
        CliCommand::DiagnosticsRecording { .. }
        | CliCommand::ServiceStatus { .. }
        | CliCommand::ServiceManage { .. }
        | CliCommand::ServiceApply { .. }
        | CliCommand::ServiceAuthorize { .. }
        | CliCommand::ServiceAuthorizedUsers { .. }
        | CliCommand::ServiceRestoreAtStartup(_) => {
            bail!("service management is dispatched before command execution")
        }
        CliCommand::GuiMode => {
            bail!("GUI mode (-g) is not part of awcc-cli; run `awcc-gui` instead")
        }
    }
}

/// Build identity advertised by the daemon handshake.
fn daemon_build_summary() -> awcc_service::ipc::BuildSummary {
    let info = awcc_build_info::BuildInfo::current();
    awcc_service::ipc::BuildSummary {
        version: info.version.clone(),
        commit: info.commit.clone().unwrap_or_else(|| "unknown".to_string()),
        package_format: awcc_build_info::detect_package_format()
            .as_str()
            .to_string(),
    }
}

/// Run the privileged daemon (the single entry point shared by every installation format) until
/// SIGTERM/SIGINT. It owns thermal control; see `awcc_service::control` for the policies.
fn run_privileged_daemon() -> Result<CliOutput> {
    let report = awcc_service::daemon::run_system_daemon(daemon_build_summary()).map_err(|error| {
        tracing::error!(target:"awcc_service::daemon", code="daemon_start_or_run_failed", error=%error, "privileged daemon could not run");
        error
    }).context("the privileged daemon could not run")?;
    Ok(CliOutput::line(format!("daemon stopped: {report}")))
}

fn service_status(json: bool) -> Result<CliOutput> {
    use awcc_service::installer::{InstallPaths, SystemRunner};
    use awcc_service::ipc::{DaemonClient, ServiceProbe};
    let installation =
        awcc_service::installer::inspect(&InstallPaths::system(), &SystemRunner::default());
    let daemon = DaemonClient::system("awcc-cli", env!("CARGO_PKG_VERSION")).probe();
    if json {
        return Ok(CliOutput::line(serde_json::to_string_pretty(
            &serde_json::json!({ "installation": installation, "daemon": daemon }),
        )?));
    }
    let availability = match &daemon {
        ServiceProbe::Running {
            version, commit, ..
        } => format!("available ({version}, {commit})"),
        ServiceProbe::NotRunning { reason } => format!("not running: {reason}"),
        ServiceProbe::AccessDenied { message } => format!("access denied: {message}"),
        ServiceProbe::Incompatible { message } => format!("incompatible: {message}"),
    };
    Ok(CliOutput {
        stdout: format!(
            "Managed by: {}\nUnit state: {:?}\nEnabled: {}\nInstalled build: {}\nDaemon: {availability}",
            installation.managed_by,
            installation.unit_state,
            installation
                .enabled
                .map_or("unknown", |enabled| if enabled { "yes" } else { "no" }),
            installation.installed_build_id.as_deref().unwrap_or("none"),
        ),
        notes: installation.problems,
    })
}

fn manage_service(action: ServiceAction, json: bool, err: &mut dyn Write) -> Result<CliOutput> {
    let executable = std::env::current_exe().context("cannot resolve awcc-cli")?;
    let report = awcc_service::management::apply_system(action, &executable, &mut |progress| {
        let _ = writeln!(err, "{}", progress.message);
        let _ = err.flush();
    })?;
    Ok(CliOutput {
        stdout: if json {
            serde_json::to_string_pretty(&report)?
        } else {
            format!("Service {}: {:?}", report.action, report.outcome)
        },
        notes: report.notes,
    })
}

fn service_authorized_users(json: bool) -> Result<CliOutput> {
    let users = awcc_service::ipc::authorized_users::list(
        &awcc_service::installer::InstallPaths::system().authorized_users_path(),
    )?;
    Ok(CliOutput::line(if json {
        serde_json::to_string_pretty(&users)?
    } else {
        users.into_iter().collect::<Vec<_>>().join("\n")
    }))
}

/// Hardware/profile construction, injectable so tests can prove which paths touch a backend.
pub trait CliBackends {
    fn diagnostic_recording(
        &mut self,
        _seconds: u16,
    ) -> Result<awcc_diagnostics::service::RecordingStatus> {
        bail!("service recording backend unavailable")
    }

    fn support_evidence(&mut self, _simulated: bool) -> awcc_diagnostics::service::ServiceSupport {
        awcc_diagnostics::service::ServiceSupport {
            state: "unavailable".into(),
            gaps: vec!["Backend diagnostic evidence unavailable".into()],
            ..Default::default()
        }
    }
    /// Resolve the device profile (`--sim` selects the simulated m15 R6 profile).
    fn device_profile(&mut self, sim: bool) -> Result<DeviceProfile>;
    /// Construct the orchestrator that owns the ACPI/USB transports.
    fn orchestrator(&mut self, sim: bool) -> Result<ThermalOrchestrator>;
}

/// Production backends: real profile resolution and transports (or the simulator with `--sim`).
pub struct RealBackends;

impl CliBackends for RealBackends {
    fn diagnostic_recording(
        &mut self,
        seconds: u16,
    ) -> Result<awcc_diagnostics::service::RecordingStatus> {
        awcc_service::ipc::DaemonClient::system("awcc-cli-recording", env!("CARGO_PKG_VERSION"))
            .diagnostic_recording(seconds)
            .map_err(Into::into)
    }

    fn support_evidence(&mut self, simulated: bool) -> awcc_diagnostics::service::ServiceSupport {
        awcc_service::diagnostics::collect(simulated)
    }
    fn device_profile(&mut self, sim: bool) -> Result<DeviceProfile> {
        if sim {
            awcc_platform::resolve_simulated_m15_r6_profile()
                .context("failed to resolve simulated device profile from database")
        } else {
            awcc_platform::resolve_host_device_profile()
                .context("failed to resolve host device profile from database")
        }
    }

    fn orchestrator(&mut self, sim: bool) -> Result<ThermalOrchestrator> {
        if sim {
            ThermalOrchestrator::new_simulated_m15_r6()
                .context("failed to initialize simulated AWCC orchestrator")
        } else {
            ThermalOrchestrator::new_hardware()
                .context("failed to initialize hardware AWCC orchestrator")
        }
    }
}

fn emit(out: &mut dyn Write, err: &mut dyn Write, result: Result<CliOutput>) -> u8 {
    match result {
        Ok(output) => {
            let _ = writeln!(out, "{}", output.stdout);
            for note in &output.notes {
                let _ = writeln!(err, "note: {note}");
            }
            0
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e:#}");
            1
        }
    }
}

/// Run one CLI invocation and return the process exit status.
///
/// `--help` and `--version` are answered before any backend is constructed, so they work with no
/// hardware, no graphical session, no privilege prompt, and no writable state directory.
pub fn run(
    raw_args: &[String],
    backends: &mut dyn CliBackends,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    run_with_log_dir(raw_args, backends, default_log_dir().as_deref(), out, err)
}

/// Like [`run`] but with an explicit session-log directory for passive diagnostics export/summary.
pub fn run_with_log_dir(
    raw_args: &[String],
    backends: &mut dyn CliBackends,
    log_dir: Option<&Path>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let inv = match parse_cli_invocation(raw_args) {
        Ok(inv) => inv,
        Err(e) => return emit(out, err, Err(e)),
    };

    // Fast-paths return before any logger or backend is initialized.
    match &inv.command {
        CliCommand::Help => return emit(out, err, Ok(CliOutput::line(format_help()))),
        CliCommand::Version => {
            return emit(
                out,
                err,
                Ok(CliOutput::line(awcc_build_info::version_text("awcc-cli"))),
            );
        }
        _ => {}
    }

    // Service management and status never construct a device profile or hardware backend.
    // In particular the elevated helper must emit only the JSONL protocol on stdout.
    let management = matches!(
        inv.command,
        CliCommand::DiagnosticsRecording { .. }
            | CliCommand::ServiceStatus { .. }
            | CliCommand::ServiceManage { .. }
            | CliCommand::ServiceApply { .. }
            | CliCommand::ServiceAuthorize { .. }
            | CliCommand::ServiceAuthorizedUsers { .. }
            | CliCommand::ServiceRestoreAtStartup(_)
    );
    if management && inv.sim_mode {
        return emit(
            out,
            err,
            Err(anyhow::anyhow!(
                "service management cannot be combined with --sim"
            )),
        );
    }
    match &inv.command {
        CliCommand::DiagnosticsRecording { seconds } => {
            let result = backends.diagnostic_recording(*seconds).map(|status| {
                CliOutput::line(if status.active {
                    format!(
                        "Service DEBUG recording active ({}; no extra hardware polls)",
                        status.remaining_secs.map_or_else(
                            || "remaining time unavailable".into(),
                            |seconds| format!("{seconds}s remaining")
                        )
                    )
                } else {
                    "Service DEBUG recording stopped".into()
                })
            });
            return emit(out, err, result);
        }
        CliCommand::ServiceStatus { json } => return emit(out, err, service_status(*json)),
        CliCommand::ServiceManage { action, json } => {
            let result = manage_service(*action, *json, err);
            return emit(out, err, result);
        }
        CliCommand::ServiceApply { action } => {
            return match std::env::current_exe() {
                Ok(executable) => {
                    awcc_service::management::run_system_apply_command(*action, &executable, out)
                }
                Err(error) => {
                    let event = awcc_service::installer::ApplyEvent::Failed {
                        kind: "io".to_owned(),
                        message: error.to_string(),
                    };
                    let _ = serde_json::to_writer(&mut *out, &event);
                    let _ = writeln!(out);
                    1
                }
            };
        }
        CliCommand::ServiceAuthorize { user, authorized } => {
            let result = awcc_service::management::set_authorized_user_system(user, *authorized)
                .map(|changed| {
                    CliOutput::line(format!(
                        "{} {user}: {}",
                        if *authorized { "Authorize" } else { "Revoke" },
                        if changed { "updated" } else { "unchanged" }
                    ))
                })
                .map_err(Into::into);
            return emit(out, err, result);
        }
        CliCommand::ServiceAuthorizedUsers { json } => {
            return emit(out, err, service_authorized_users(*json));
        }
        CliCommand::ServiceRestoreAtStartup(enabled) => {
            let result =
                awcc_service::ipc::DaemonClient::system("awcc-cli", env!("CARGO_PKG_VERSION"))
                    .execute(awcc_service::ipc::Op::SetRestoreAtStartup { enabled: *enabled })
                    .map_err(anyhow::Error::from)
                    .and_then(|report| report.lifecycle.into_result().map_err(Into::into))
                    .map(|success| {
                        let notes = success
                            .caveat()
                            .map(|note| vec![note.to_owned()])
                            .unwrap_or_default();
                        CliOutput {
                            stdout: success.summary,
                            notes,
                        }
                    });
            return emit(out, err, result);
        }
        _ => {}
    }

    let resolved_level = match resolve_log_level(
        inv.cli_log_level.as_deref(),
        inv.cli_verbose,
        std::env::var("AWCC_LOG").ok().as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => return emit(out, err, Err(anyhow::anyhow!("{e}"))),
    };
    let trace_raw_usb = resolve_raw_usb_opt_in(
        inv.cli_trace_raw_usb,
        std::env::var("AWCC_TRACE_RAW_USB").ok().as_deref(),
    );

    if let Some(warn_msg) = &resolved_level.env_warning {
        let _ = writeln!(
            err,
            "[WARN] awcc::diagnostics [invalid_log_env]: {warn_msg}"
        );
    }
    if resolved_level.level >= Severity::Debug {
        let _ = writeln!(
            err,
            "[DEBUG] awcc::cli [log_config]: level={} (source={}), sim={}, trace_raw_usb={}, cmd={:?}",
            resolved_level.level.as_str(),
            resolved_level.source,
            inv.sim_mode,
            trace_raw_usb,
            inv.command
        );
    }

    let result = match inv.command.clone() {
        CliCommand::Help | CliCommand::Version => unreachable!("handled above"),
        CliCommand::DiagnosticsRecording { .. }
        | CliCommand::ServiceStatus { .. }
        | CliCommand::ServiceManage { .. }
        | CliCommand::ServiceApply { .. }
        | CliCommand::ServiceAuthorize { .. }
        | CliCommand::ServiceAuthorizedUsers { .. }
        | CliCommand::ServiceRestoreAtStartup(_) => unreachable!("handled above"),
        // Passive commands must never instantiate USB or ACPI hardware!
        CliCommand::DeviceInfo => backends
            .device_profile(inv.sim_mode)
            .map(|profile| CliOutput::line(format_device_info(&profile))),
        CliCommand::DiagnosticsSummary { .. } | CliCommand::DiagnosticsExport { .. } => {
            execute_passive_diagnostics_command(
                &inv.command,
                &inv,
                log_dir,
                backends.support_evidence(inv.sim_mode),
            )
        }
        CliCommand::DaemonMode if inv.sim_mode => Err(anyhow::anyhow!(
            "daemon mode serves real hardware only and cannot be combined with --sim"
        )),
        // Daemon startup constructs its own thermal owner; don't construct a client backend.
        CliCommand::DaemonMode => run_privileged_daemon(),
        other => backends.orchestrator(inv.sim_mode).and_then(|mut orch| {
            orch.set_trace_raw_usb(trace_raw_usb);
            execute_active_cli_command(other, &mut orch)
        }),
    };
    emit(out, err, result)
}

fn main() -> std::process::ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    if raw_args.first().is_some_and(|s| s == "--powerprofilesctl") {
        return match awcc_service::power_bus::run_cli(&raw_args[1..]) {
            Ok(code) => std::process::ExitCode::from(code),
            Err(e) => {
                eprintln!("{e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if raw_args.first().is_some_and(|s| s == "--provider-health") {
        return match awcc_service::provider_install::health() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if raw_args.first().is_some_and(|s| s == "--provider-handover") {
        if !awcc_service::provider_install::is_root() {
            eprintln!("provider handover requires root");
            return std::process::ExitCode::FAILURE;
        }
        if let Ok(level) = resolve_log_level(None, false, None) {
            let mut cfg = DiagnosticsConfig::from_env_and_cli("awcc-provider-setup", level, false);
            cfg.log_dir = Some(PathBuf::from("/var/lib/awcc/awcc-rs/logs"));
            cfg.service_console = true;
            init_global_diagnostics(cfg);
        }
        let paths = awcc_service::installer::InstallPaths::system();
        let runner = awcc_service::installer::SystemRunner::default();
        let result = match raw_args.get(1).map(String::as_str) {
            Some("install") => {
                std::env::current_exe()
                    .map_err(|e| e.to_string())
                    .and_then(|path| {
                        let executable = if path.starts_with("/opt/awcc-rs/service/") {
                            "/opt/awcc-rs/service/current/bin/awcc-cli".to_string()
                        } else {
                            path.to_str()
                                .ok_or("service executable path is not UTF-8")?
                                .to_string()
                        };
                        awcc_service::provider_install::install(&paths, &runner, &executable)
                    })
            }
            Some("remove") => awcc_service::provider_install::remove(&paths, &runner),
            _ => Err("expected install or remove".into()),
        };
        let exit = match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(target:"awcc_service::provider_install", code="provider_setup_failed", error=%e, "provider setup could not complete");
                eprintln!("{e}");
                std::process::ExitCode::FAILURE
            }
        };
        if let Some(diag) = global_diagnostics() {
            diag.shutdown(Duration::from_secs(1));
        }
        return exit;
    }
    // Only initialize global file/tracing diagnostics and panic hook when not running a pure
    // fast-path (`--help`, `--version`) or passive `diagnostics` export.
    if let Ok(inv) = parse_cli_invocation(&raw_args)
        && !matches!(
            inv.command,
            CliCommand::Help
                | CliCommand::Version
                | CliCommand::DiagnosticsSummary { .. }
                | CliCommand::DiagnosticsExport { .. }
                | CliCommand::DiagnosticsRecording { .. }
                | CliCommand::ServiceStatus { .. }
                | CliCommand::ServiceManage { .. }
                | CliCommand::ServiceApply { .. }
                | CliCommand::ServiceAuthorize { .. }
                | CliCommand::ServiceAuthorizedUsers { .. }
                | CliCommand::ServiceRestoreAtStartup(_)
        )
        && let Ok(resolved_level) = resolve_log_level(
            inv.cli_log_level.as_deref(),
            inv.cli_verbose,
            std::env::var("AWCC_LOG").ok().as_deref(),
        )
    {
        let trace_raw_usb = resolve_raw_usb_opt_in(
            inv.cli_trace_raw_usb,
            std::env::var("AWCC_TRACE_RAW_USB").ok().as_deref(),
        );
        let mut cfg =
            DiagnosticsConfig::from_env_and_cli("awcc-cli", resolved_level, trace_raw_usb);
        cfg.service_console = matches!(inv.command, CliCommand::DaemonMode);
        let runtime = init_global_diagnostics(cfg);
        install_panic_hook(runtime);
    }

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code = run(&raw_args, &mut RealBackends, &mut stdout, &mut stderr);
    if let Some(diag) = global_diagnostics() {
        diag.shutdown(Duration::from_millis(250));
    }
    std::process::ExitCode::from(code)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use awcc_platform::{
        AckStatus, MemoryStateStorage, SimBackend, SimFaults, usb_elc::SimUsbElcTransport,
    };

    use super::*;

    fn to_args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    /// Backends that count every construction so tests can prove which paths touch them, and
    /// that can install simulated faults on the orchestrators they hand out.
    #[derive(Default)]
    struct SpyBackends {
        profile_calls: usize,
        orchestrator_calls: usize,
        faults: SimFaults,
        usb: Option<SimUsbElcTransport>,
    }

    impl CliBackends for SpyBackends {
        fn device_profile(&mut self, _sim: bool) -> Result<DeviceProfile> {
            self.profile_calls += 1;
            Ok(DeviceProfile::alienware_m15_r6()?)
        }

        fn orchestrator(&mut self, _sim: bool) -> Result<ThermalOrchestrator> {
            self.orchestrator_calls += 1;
            let mut orch = ThermalOrchestrator::with_lighting_transport(
                DeviceProfile::alienware_m15_r6()?,
                Box::new(SimBackend::new_alienware_m15_r6()),
                Box::new(MemoryStateStorage::new(Some(ThermalMode::Balanced))),
                Box::new(
                    self.usb
                        .take()
                        .unwrap_or_else(SimUsbElcTransport::new_alienware_m15_r6),
                ),
            );
            orch.set_sim_faults(self.faults.clone());
            Ok(orch)
        }
    }

    struct Run {
        code: u8,
        stdout: String,
        stderr: String,
    }

    fn run_cli(args: &[&str], backends: &mut SpyBackends) -> Run {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&to_args(args), backends, &mut out, &mut err);
        Run {
            code,
            stdout: String::from_utf8(out).unwrap(),
            stderr: String::from_utf8(err).unwrap(),
        }
    }

    #[test]
    fn service_lifecycle_routes_parse_and_reject_unsafe_option_combinations() {
        assert_eq!(
            parse_cli_args(&to_args(&["service", "install"])).unwrap(),
            CliCommand::ServiceManage {
                action: ServiceAction::Install,
                json: false
            },
        );
        assert_eq!(
            parse_cli_args(&to_args(&["service-apply", "uninstall", "--purge"])).unwrap(),
            CliCommand::ServiceApply {
                action: ServiceAction::Uninstall { purge: true }
            },
        );
        assert_eq!(
            parse_cli_args(&to_args(&["service", "status", "--json"])).unwrap(),
            CliCommand::ServiceStatus { json: true },
        );
        assert_eq!(
            parse_cli_args(&to_args(&["service", "setup", "--json"])).unwrap(),
            CliCommand::ServiceManage {
                action: ServiceAction::Setup,
                json: true
            },
        );
        assert_eq!(
            parse_cli_args(&to_args(&["service-apply", "setup"])).unwrap(),
            CliCommand::ServiceApply {
                action: ServiceAction::Setup
            },
        );
        assert_eq!(
            parse_cli_args(&to_args(&["service", "restore-at-startup", "off"])).unwrap(),
            CliCommand::ServiceRestoreAtStartup(false),
        );
        for args in [
            &["service", "install", "--purge"][..],
            &["service", "update", "--purge"],
            &["service", "setup", "--purge"],
            &["service", "setup", "1000"],
            &["service", "uninstall", "--purge", "--purge"],
            &["service-apply", "status"],
            &["service-apply", "install", "--json"],
            &["service", "authorize"],
            &["service", "revoke", "alice", "bob"],
            &["service", "restore-at-startup", "maybe"],
        ] {
            assert!(parse_cli_args(&to_args(args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn service_commands_cannot_escape_simulation_to_manage_the_host() {
        let mut backends = SpyBackends::default();
        for args in [
            &["--sim", "service", "install"][..],
            &["--sim", "service", "update"],
            &["--sim", "service", "setup"],
            &["--sim", "service-apply", "setup"],
            &["--sim", "service", "uninstall", "--purge"],
            &["--sim", "service-apply", "install"],
            &["--sim", "service", "authorize", "1000"],
            &["--sim", "service", "restore-at-startup", "on"],
        ] {
            let run = run_cli(args, &mut backends);
            assert_eq!(run.code, 1, "{args:?}");
            assert!(run.stdout.is_empty());
            assert!(run.stderr.contains("cannot be combined with --sim"));
        }
        assert_eq!(backends.profile_calls, 0);
        assert_eq!(backends.orchestrator_calls, 0);
    }

    #[test]
    fn daemon_flags_validate_the_entire_invocation_before_startup() {
        let mut backends = SpyBackends::default();
        for args in [&["--sim", "--daemon"][..], &["--daemon", "--sim"]] {
            let run = run_cli(args, &mut backends);
            assert_eq!(run.code, 1);
            assert!(run.stdout.is_empty());
            assert!(run.stderr.contains("cannot be combined with --sim"));
        }
        for args in [
            &["--daemon", "--gui"][..],
            &["--daemon", "unexpected"],
            &["service", "install", "--daemon"],
        ] {
            assert!(parse_cli_args(&to_args(args)).is_err());
        }
        assert_eq!(backends.profile_calls, 0);
        assert_eq!(backends.orchestrator_calls, 0);
    }

    #[test]
    fn passive_cli_commands_parse_and_format_without_hardware() {
        assert_eq!(
            parse_cli_args(&to_args(&["--help"])).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            parse_cli_args(&to_args(&["--version"])).unwrap(),
            CliCommand::Version
        );
        assert_eq!(
            parse_cli_args(&to_args(&["-V"])).unwrap(),
            CliCommand::Version
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
    fn help_and_version_never_construct_any_backend() {
        for args in [
            &["--version"][..],
            &["-V"],
            &["--sim", "--version"],
            &["--version", "st", "0"],
            &["--help"],
            &["-h"],
            &["help"],
            &[],
        ] {
            let mut spy = SpyBackends::default();
            let result = run_cli(args, &mut spy);
            assert_eq!(result.code, 0, "{args:?}: {}", result.stderr);
            assert_eq!(spy.profile_calls, 0, "{args:?} resolved a device profile");
            assert_eq!(spy.orchestrator_calls, 0, "{args:?} built an orchestrator");
            assert!(result.stderr.is_empty(), "{args:?}: {}", result.stderr);
        }

        // Positive controls: the spy really observes construction.
        let mut spy = SpyBackends::default();
        let info = run_cli(&["device-info"], &mut spy);
        assert_eq!(info.code, 0);
        assert_eq!((spy.profile_calls, spy.orchestrator_calls), (1, 0));
        let mut spy = SpyBackends::default();
        run_cli(&["qm"], &mut spy);
        assert_eq!((spy.profile_calls, spy.orchestrator_calls), (0, 1));
    }

    #[test]
    fn version_output_is_the_shared_build_identity() {
        let mut spy = SpyBackends::default();
        let result = run_cli(&["--version"], &mut spy);
        let first = result.stdout.lines().next().unwrap();
        assert!(first.starts_with("awcc-cli "), "{first}");
        for key in [
            "version:",
            "commit:",
            "commit-source:",
            "dirty:",
            "source-date-epoch:",
            "target:",
            "profile:",
            "rustc:",
            "package-format:",
        ] {
            assert!(
                result.stdout.lines().any(|l| l.starts_with(key)),
                "missing `{key}` in:\n{}",
                result.stdout
            );
        }
        assert!(
            result
                .stdout
                .contains(&format!("version: {}", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn active_cli_commands_execute_thermal_tcc_fancurve_and_lighting_operations() {
        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().unwrap();

        // 1. Set thermal mode FullSpeed (`fs`)
        let cmd_fs = parse_cli_args(&to_args(&["fs"])).unwrap();
        let out_fs = execute_active_cli_command(cmd_fs, &mut orch).unwrap();
        assert!(out_fs.stdout.contains("Full Speed"));
        assert!(
            out_fs.notes.is_empty(),
            "a confirmed result carries no caveat"
        );

        // 2. Set TCC Offset (`tcc 10`)
        let cmd_tcc = parse_cli_args(&to_args(&["tcc", "10"])).unwrap();
        let out_tcc = execute_active_cli_command(cmd_tcc, &mut orch).unwrap();
        assert!(out_tcc.stdout.contains("10°C"));

        // 3. Configure custom fan curve (`fancurve 45 10 60 40 75 75 85 100 3`)
        let cmd_fc = parse_cli_args(&to_args(&[
            "fancurve", "45", "10", "60", "40", "75", "75", "85", "100", "3",
        ]))
        .unwrap();
        let out_fc = execute_active_cli_command(cmd_fc, &mut orch).unwrap();
        assert!(out_fc.stdout.contains("Custom fan curve active"));
        assert!(
            out_fc
                .notes
                .iter()
                .any(|n| n.contains("exits after this command")),
            "the CLI must say the curve does not keep running: {:?}",
            out_fc.notes
        );
        assert!(
            out_fc
                .notes
                .iter()
                .all(|n| !n.contains("background service")),
            "a local curve must not claim service ownership: {:?}",
            out_fc.notes
        );

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
        assert!(out_light.stdout.contains("Keyboard (4-Zone)"));
        assert!(out_light.stdout.contains("volatile session"));
        // Lighting has no readback: success is explicitly unconfirmed.
        assert!(
            out_light
                .notes
                .iter()
                .any(|n| n.contains("no readback of applied lighting")),
            "{:?}",
            out_light.notes
        );

        // 5. Query raw-status
        let cmd_raw = parse_cli_args(&to_args(&["raw-status", "--all"])).unwrap();
        let out_raw = execute_active_cli_command(cmd_raw, &mut orch).unwrap();
        assert!(out_raw.stdout.contains("platform=0x0F01"));
        assert!(out_raw.stdout.contains("2.0.5"));
    }

    #[test]
    fn service_backed_fan_curve_reports_ownership_only_on_success() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use awcc_core::OperationLifecycle;
        use awcc_service::{
            RemoteThermal,
            control::{ControlCore, ControlOp, NullSettingsSink},
            ipc::ClientError,
            settings::SettingsLoad,
            wire::ThermalView,
        };

        struct SimulatedService {
            core: ControlCore,
            unavailable: Arc<AtomicBool>,
        }

        impl RemoteThermal for SimulatedService {
            fn refresh(&mut self) -> Result<ThermalView, ClientError> {
                Ok(self.core.refresh())
            }

            fn execute(
                &mut self,
                op: ControlOp,
            ) -> Result<(ThermalView, OperationLifecycle), ClientError> {
                if self.unavailable.load(Ordering::SeqCst) {
                    return Err(ClientError::Unavailable("test connection lost".to_string()));
                }
                assert!(matches!(&op, ControlOp::ConfigureCurve(_)));
                let reply = self.core.handle(op);
                let lifecycle = reply
                    .result
                    .map_err(|error| ClientError::Rejected(error.message))?;
                Ok((reply.view, lifecycle))
            }
        }

        let unavailable = Arc::new(AtomicBool::new(false));
        let service = SimulatedService {
            core: ControlCore::new(
                ThermalOrchestrator::new_simulated_m15_r6().unwrap(),
                SettingsLoad::Missing,
                Arc::new(NullSettingsSink),
                Duration::from_secs(2),
            ),
            unavailable: Arc::clone(&unavailable),
        };
        let mut orch = ThermalOrchestrator::new_service_backed(Box::new(service)).unwrap();
        let cmd = parse_cli_args(&to_args(&[
            "fancurve", "45", "10", "60", "40", "75", "75", "85", "100", "3",
        ]))
        .unwrap();
        let out = execute_active_cli_command(cmd.clone(), &mut orch).unwrap();
        assert!(
            out.notes
                .iter()
                .any(|note| note.contains("background service owns this curve")),
            "{:?}",
            out.notes
        );
        assert!(
            out.notes
                .iter()
                .all(|note| !note.contains("exits after this command")),
            "{:?}",
            out.notes
        );

        unavailable.store(true, Ordering::SeqCst);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = emit(
            &mut stdout,
            &mut stderr,
            execute_active_cli_command(cmd, &mut orch),
        );
        assert_eq!(code, 1);
        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains("test connection lost"), "{stderr}");
        assert!(!stderr.contains("background service owns"), "{stderr}");
    }

    // --- Regression: setters used to print success after failure -------------------------------

    #[test]
    fn rejected_turbo_write_exits_nonzero_with_an_error_and_no_stdout() {
        let mut spy = SpyBackends {
            faults: SimFaults {
                reject_turbo_write: true,
                ..SimFaults::default()
            },
            ..SpyBackends::default()
        };
        let result = run_cli(&["--sim", "st", "0"], &mut spy);
        assert_eq!(result.code, 1);
        assert!(result.stdout.is_empty(), "stdout: {}", result.stdout);
        assert!(
            result
                .stderr
                .contains("error: Failed to toggle CPU Turbo Boost"),
            "{}",
            result.stderr
        );
        assert!(!result.stderr.contains("set to"), "{}", result.stderr);
    }

    #[test]
    fn turbo_success_is_reported_only_for_a_matching_readback() {
        let mut spy = SpyBackends::default();
        let ok = run_cli(&["--sim", "st", "0"], &mut spy);
        assert_eq!(ok.code, 0);
        assert_eq!(ok.stdout.trim(), "CPU Turbo Boost disabled");
        assert!(ok.stderr.is_empty(), "confirmed: {}", ok.stderr);

        let mut stuck = SpyBackends {
            faults: SimFaults {
                turbo_readback_stuck: Some(true),
                ..SimFaults::default()
            },
            ..SpyBackends::default()
        };
        let contradicted = run_cli(&["--sim", "st", "0"], &mut stuck);
        assert_eq!(contradicted.code, 1);
        assert!(contradicted.stdout.is_empty());
        assert!(
            contradicted
                .stderr
                .contains("reports turbo enabled instead of disabled")
        );

        let mut blind = SpyBackends {
            faults: SimFaults {
                turbo_readback_unavailable: true,
                ..SimFaults::default()
            },
            ..SpyBackends::default()
        };
        let unconfirmed = run_cli(&["--sim", "st", "0"], &mut blind);
        assert_eq!(unconfirmed.code, 0, "accepted writes still exit 0");
        assert_eq!(unconfirmed.stdout.trim(), "CPU Turbo Boost disabled");
        assert!(
            unconfirmed.stderr.contains("note: not device-confirmed"),
            "{}",
            unconfirmed.stderr
        );
    }

    #[test]
    fn failed_lighting_setters_exit_nonzero_instead_of_echoing_the_requested_value() {
        for args in [
            &["--sim", "brightness", "40"][..],
            &["--sim", "stealth", "on"],
            &["--sim", "static", "255", "0", "0"],
        ] {
            let mut usb = SimUsbElcTransport::new_alienware_m15_r6();
            usb.inject_send_failure_from(Some(1));
            let mut spy = SpyBackends {
                usb: Some(usb),
                ..SpyBackends::default()
            };
            let result = run_cli(args, &mut spy);
            assert_eq!(result.code, 1, "{args:?}: {}", result.stderr);
            assert!(result.stdout.is_empty(), "{args:?}: {}", result.stdout);
            assert!(
                result.stderr.starts_with("error: "),
                "{args:?}: {}",
                result.stderr
            );
        }
    }

    #[test]
    fn completed_transfer_with_missing_ack_exits_zero_but_is_not_called_confirmed() {
        let mut usb = SimUsbElcTransport::new_alienware_m15_r6();
        usb.inject_ack_fault(Some(AckStatus::Failed {
            reason: "timed out".to_string(),
        }));
        let mut spy = SpyBackends {
            usb: Some(usb),
            ..SpyBackends::default()
        };
        let result = run_cli(&["--sim", "brightness", "40"], &mut spy);
        assert_eq!(result.code, 0, "{}", result.stderr);
        assert!(result.stdout.contains("Set lighting brightness to 40%"));
        assert!(
            result.stderr.contains("not device-confirmed"),
            "{}",
            result.stderr
        );
        assert!(result.stderr.contains("timed out"), "{}", result.stderr);
        assert!(!result.stdout.to_lowercase().contains("confirmed"));
    }

    #[test]
    fn setting_one_fan_without_a_readable_counterpart_refuses_to_guess_and_writes_nothing() {
        for (args, who) in [(&["scb", "50"][..], "GPU"), (&["sgb", "50"][..], "CPU")] {
            let mut orch = ThermalOrchestrator::new_simulated_m15_r6().unwrap();
            orch.set_sim_scenario(awcc_core::SimScenario::SensorUnavailable);
            let cmd = parse_cli_args(&to_args(args)).unwrap();
            let err = execute_active_cli_command(cmd, &mut orch).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("{who} value will not be guessed")),
                "{err}"
            );
            assert_eq!(orch.query_snapshot().write_count(), 0, "{args:?} wrote");
        }
    }

    #[test]
    fn stopping_a_fan_curve_in_a_fresh_process_does_not_claim_it_stopped_anything() {
        let mut spy = SpyBackends::default();
        let result = run_cli(&["--sim", "fancurve", "stop"], &mut spy);
        assert_eq!(result.code, 0);
        assert!(
            result.stdout.contains("nothing was stopped"),
            "{}",
            result.stdout
        );
        assert!(result.stderr.contains("not device-confirmed"));
    }

    #[test]
    fn unreadable_single_value_queries_exit_nonzero_with_the_reason() {
        let mut spy = SpyBackends {
            faults: SimFaults {
                turbo_readback_unavailable: true,
                ..SimFaults::default()
            },
            ..SpyBackends::default()
        };
        let turbo = run_cli(&["--sim", "getturbo"], &mut spy);
        assert_eq!(turbo.code, 1);
        assert!(turbo.stdout.is_empty());
        assert!(
            turbo.stderr.contains("CPU Turbo Boost state unavailable"),
            "{}",
            turbo.stderr
        );

        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().unwrap();
        orch.set_sim_scenario(awcc_core::SimScenario::SensorUnavailable);
        for cmd in [
            CliCommand::QueryCpuBoost,
            CliCommand::QueryGpuBoost,
            CliCommand::QueryTccOffset,
        ] {
            let err = execute_active_cli_command(cmd, &mut orch).unwrap_err();
            assert!(err.to_string().contains("unavailable"), "{err}");
        }
    }

    #[test]
    fn query_mode_labels_a_failed_game_shift_query_instead_of_printing_false() {
        let mut spy = SpyBackends {
            faults: SimFaults {
                game_shift_query_unavailable: true,
                ..SimFaults::default()
            },
            ..SpyBackends::default()
        };
        let result = run_cli(&["--sim", "qm"], &mut spy);
        assert_eq!(result.code, 0);
        assert!(
            result.stdout.contains("GameShift: unavailable ("),
            "{}",
            result.stdout
        );
        assert!(!result.stdout.contains("GameShift: false"));
        assert!(result.stderr.contains("GameShift state is not current"));
    }

    #[test]
    fn failed_thermal_mode_query_is_an_error_not_a_manual_mode_line() {
        let mut spy = SpyBackends {
            faults: SimFaults {
                thermal_mode_query_unavailable: true,
                ..SimFaults::default()
            },
            ..SpyBackends::default()
        };
        let result = run_cli(&["--sim", "qm"], &mut spy);
        assert_eq!(result.code, 1);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.contains("thermal mode query failed"));
    }

    #[test]
    fn raw_status_reports_the_underlying_usb_failure() {
        let mut usb = SimUsbElcTransport::new_alienware_m15_r6();
        usb.inject_receive_failure(Some("simulated USB read timeout".to_string()));
        let mut spy = SpyBackends {
            usb: Some(usb),
            ..SpyBackends::default()
        };
        let result = run_cli(&["--sim", "raw-status"], &mut spy);
        assert_eq!(result.code, 1);
        assert!(result.stdout.is_empty());
        assert!(
            result.stderr.contains("simulated USB read timeout"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn daemon_and_gui_flags_no_longer_print_a_false_ready_message() {
        for flag in ["-d", "-g"] {
            let mut spy = SpyBackends::default();
            let result = run_cli(&["--sim", flag], &mut spy);
            assert_eq!(result.code, 1, "{flag}");
            assert!(result.stdout.is_empty(), "{flag}: {}", result.stdout);
            assert!(
                !result.stderr.contains("ready"),
                "{flag}: {}",
                result.stderr
            );
        }
    }

    #[test]
    fn daemon_recording_commands_never_construct_hardware_and_validate_bounds() {
        #[derive(Default)]
        struct RecordingBackend(Vec<u16>);
        impl CliBackends for RecordingBackend {
            fn device_profile(&mut self, _: bool) -> Result<DeviceProfile> {
                panic!("recording must not resolve hardware")
            }
            fn orchestrator(&mut self, _: bool) -> Result<ThermalOrchestrator> {
                panic!("recording must not construct hardware")
            }
            fn diagnostic_recording(
                &mut self,
                seconds: u16,
            ) -> Result<awcc_diagnostics::service::RecordingStatus> {
                self.0.push(seconds);
                Ok(awcc_diagnostics::service::RecordingStatus {
                    active: seconds > 0,
                    remaining_secs: (seconds > 0).then_some(u64::from(seconds)),
                })
            }
        }
        let mut backend = RecordingBackend::default();
        for (args, expected) in [
            (&["diagnostics", "record"][..], 300),
            (&["diagnostics", "record", "--seconds", "45"], 45),
            (&["diagnostics", "stop-recording"], 0),
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            assert_eq!(run(&to_args(args), &mut backend, &mut out, &mut err), 0);
            assert_eq!(backend.0.last(), Some(&expected));
            assert!(err.is_empty());
        }
        for args in [
            &["diagnostics", "record", "--seconds", "0"][..],
            &["diagnostics", "record", "--seconds", "301"],
            &["--sim", "diagnostics", "record"],
        ] {
            let before = backend.0.len();
            assert_ne!(
                run(
                    &to_args(args),
                    &mut backend,
                    &mut Vec::new(),
                    &mut Vec::new()
                ),
                0
            );
            assert_eq!(backend.0.len(), before);
        }
    }

    #[test]
    fn backend_start_failure_is_reported_on_stderr() {
        struct Failing;
        impl CliBackends for Failing {
            fn device_profile(&mut self, _sim: bool) -> Result<DeviceProfile> {
                bail!("profile boom")
            }
            fn orchestrator(&mut self, _sim: bool) -> Result<ThermalOrchestrator> {
                bail!("orchestrator boom")
            }
        }
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&to_args(&["qm"]), &mut Failing, &mut out, &mut err);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("orchestrator boom")
        );
    }

    #[test]
    fn verbose_flag_and_log_level_controls_are_enforced_with_stderr_diagnostics() {
        // `-v` is no longer a silent no-op: it enables DEBUG diagnostics on stderr while keeping
        // stdout strictly machine-readable.
        let mut spy = SpyBackends::default();
        let verbose = run_cli(&["-v", "--sim", "qm"], &mut spy);
        assert_eq!(verbose.code, 0);
        assert!(verbose.stdout.starts_with("Current Thermal Mode: Balanced"));
        assert!(!verbose.stdout.contains("[DEBUG]"));
        assert!(
            verbose
                .stderr
                .contains("[DEBUG] awcc::cli [log_config]: level=DEBUG (source=cli_verbose)"),
            "stderr: {}",
            verbose.stderr
        );

        // Explicit `--log-level trace` overrides `-v`.
        let mut spy = SpyBackends::default();
        let trace = run_cli(&["-v", "--log-level", "trace", "--sim", "qm"], &mut spy);
        assert_eq!(trace.code, 0);
        assert!(
            trace.stderr.contains("level=TRACE (source=cli_log_level)"),
            "stderr: {}",
            trace.stderr
        );

        // Invalid `--log-level` fails explicitly with exit 1 and zero backend touches.
        let mut spy = SpyBackends::default();
        let bad = run_cli(&["--log-level", "not-a-level", "--sim", "qm"], &mut spy);
        assert_eq!(bad.code, 1);
        assert!(bad.stdout.is_empty());
        assert!(
            bad.stderr.contains("invalid log level 'not-a-level'"),
            "stderr: {}",
            bad.stderr
        );
        assert_eq!(spy.profile_calls, 0);
        assert_eq!(spy.orchestrator_calls, 0);
    }

    #[test]
    fn diagnostics_export_and_summary_are_strictly_passive_and_produce_valid_redacted_zip() {
        let tmp = std::env::temp_dir().join(format!(
            "awcc-cli-diag-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // 1. When no prior session evidence exists, `diagnostics export` still succeeds passively
        //    with `origin=unavailable` and zero backend calls.
        let empty_log_dir = tmp.join("empty-logs");
        let zip_unavail = tmp.join("support-unavail.zip");
        let mut spy = SpyBackends::default();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with_log_dir(
            &to_args(&[
                "diagnostics",
                "export",
                "--output",
                zip_unavail.to_str().unwrap(),
            ]),
            &mut spy,
            Some(&empty_log_dir),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        assert_eq!(
            spy.profile_calls, 0,
            "passive export must not resolve live profile"
        );
        assert_eq!(
            spy.orchestrator_calls, 0,
            "passive export must not open transports"
        );
        let stdout_str = String::from_utf8(out).unwrap();
        assert!(stdout_str.contains("origin=unavailable"), "{stdout_str}");

        let archive_bytes = std::fs::read(&zip_unavail).unwrap();
        let files = awcc_diagnostics::zip::read_stored_zip(&archive_bytes).unwrap();
        for expected in [
            "summary.txt",
            "report.json",
            "events.jsonl",
            "hardware-audit.jsonl",
            "build-info.json",
            "settings.json",
        ] {
            assert!(
                files.iter().any(|e| e.name == expected),
                "missing {expected} in zip"
            );
        }

        // 2. Persist simulated session evidence containing sensitive paths/serials into `populated-logs`
        //    and verify `diagnostics export` ingests it passively with observation age and redacts PII.
        let pop_log_dir = tmp.join("populated-logs");
        let sink =
            awcc_diagnostics::sink::FileSink::start(awcc_diagnostics::sink::SinkConfig::new(
                pop_log_dir.clone(),
                "a1b2c3d4e5f60718".to_string(),
                std::process::id(),
            ))
            .unwrap();
        let mut orch = ThermalOrchestrator::new_simulated_m15_r6().unwrap();
        orch.set_trace_raw_usb(true);
        let _ = orch.set_thermal_mode(ThermalMode::Performance);
        let snap = orch.set_brightness(50);
        let mut evidence = snap.to_session_evidence(
            "a1b2c3d4e5f60718",
            awcc_diagnostics::export::BuildIdentityReport::current("awcc-gui"),
            true,
        );
        assert!(
            evidence
                .audit_entries
                .iter()
                .any(|e| e.get("raw_usb_response_hex").is_some()),
            "persisted session evidence must contain raw_usb_response_hex before opt-out export"
        );
        evidence.subsystem_health.last_error = Some(
            "failed reading /home/alice/.config/awcc with Service Tag: ABC1234 and MAC aa:bb:cc:dd:ee:ff"
                .to_string(),
        );
        sink.publish_evidence(serde_json::to_string_pretty(&evidence).unwrap());
        sink.shutdown(Duration::from_secs(1));

        let zip_pop = tmp.join("awcc-support.zip");
        let mut spy2 = SpyBackends::default();
        let (mut out2, mut err2) = (Vec::new(), Vec::new());
        let code2 = run_with_log_dir(
            &to_args(&[
                "diagnostics",
                "export",
                "--output",
                zip_pop.to_str().unwrap(),
            ]),
            &mut spy2,
            Some(&pop_log_dir),
            &mut out2,
            &mut err2,
        );
        assert_eq!(code2, 0, "stderr: {}", String::from_utf8_lossy(&err2));
        assert_eq!(spy2.profile_calls, 0);
        assert_eq!(spy2.orchestrator_calls, 0);
        let stdout2 = String::from_utf8(out2).unwrap();
        assert!(
            stdout2.contains("origin=persisted_session_file"),
            "{stdout2}"
        );

        let pop_files =
            awcc_diagnostics::zip::read_stored_zip(&std::fs::read(&zip_pop).unwrap()).unwrap();
        let find_entry = |name: &str| -> String {
            String::from_utf8(
                pop_files
                    .iter()
                    .find(|e| e.name == name)
                    .unwrap_or_else(|| panic!("missing {name}"))
                    .data
                    .clone(),
            )
            .unwrap()
        };
        for entry in &pop_files {
            let text = std::str::from_utf8(&entry.data).unwrap();
            assert!(
                !text.contains("raw_usb_response_hex"),
                "raw_usb_response_hex must not appear in {} without --trace-raw-usb: {text}",
                entry.name
            );
        }
        let report_json = find_entry("report.json");
        let summary_txt = find_entry("summary.txt");
        let audit_jsonl = find_entry("hardware-audit.jsonl");

        assert!(report_json.contains("\"schema_version\": 1"));
        assert!(report_json.contains("\"origin\": \"persisted_session_file\""));
        assert!(report_json.contains("\"observation_age_ms\":"));
        assert!(report_json.contains("Alienware m15 R6"));
        assert!(
            !report_json.contains("/home/alice"),
            "home path must be redacted: {report_json}"
        );
        assert!(
            !report_json.contains("ABC1234"),
            "service tag must be redacted: {report_json}"
        );
        assert!(
            !report_json.contains("aa:bb:cc:dd:ee:ff"),
            "MAC must be redacted: {report_json}"
        );
        assert!(
            !summary_txt.contains("/home/alice") && !summary_txt.contains("ABC1234"),
            "summary.txt must be redacted: {summary_txt}"
        );
        assert!(
            audit_jsonl.contains("WMAX 0 0x15"),
            "hardware-audit.jsonl must include recorded ACPI writes: {audit_jsonl}"
        );

        // Verify explicit `--trace-raw-usb` includes `raw_usb_response_hex` in the exported archive.
        let zip_opt_in = tmp.join("awcc-support-raw-usb.zip");
        let (mut opt_out, mut opt_err) = (Vec::new(), Vec::new());
        let opt_code = run_with_log_dir(
            &to_args(&[
                "--trace-raw-usb",
                "diagnostics",
                "export",
                "--output",
                zip_opt_in.to_str().unwrap(),
            ]),
            &mut spy2,
            Some(&pop_log_dir),
            &mut opt_out,
            &mut opt_err,
        );
        assert_eq!(opt_code, 0, "stderr: {}", String::from_utf8_lossy(&opt_err));
        let opt_files =
            awcc_diagnostics::zip::read_stored_zip(&std::fs::read(&zip_opt_in).unwrap()).unwrap();
        let opt_audit = String::from_utf8(
            opt_files
                .iter()
                .find(|e| e.name == "hardware-audit.jsonl")
                .unwrap()
                .data
                .clone(),
        )
        .unwrap();
        let opt_report = String::from_utf8(
            opt_files
                .iter()
                .find(|e| e.name == "report.json")
                .unwrap()
                .data
                .clone(),
        )
        .unwrap();
        assert!(opt_audit.contains("raw_usb_response_hex"));
        assert!(opt_report.contains("raw_usb_response_hex"));

        // 3. `diagnostics summary` prints human summary by default and valid JSON with `--json`.
        let (mut sum_out, mut sum_err) = (Vec::new(), Vec::new());
        let sum_code = run_with_log_dir(
            &to_args(&["diagnostics", "summary", "--json"]),
            &mut spy2,
            Some(&pop_log_dir),
            &mut sum_out,
            &mut sum_err,
        );
        assert_eq!(sum_code, 0);
        assert_eq!((spy2.profile_calls, spy2.orchestrator_calls), (0, 0));
        let sum_str = String::from_utf8(sum_out.clone()).unwrap();
        assert!(
            !sum_str.contains("raw_usb_response_hex"),
            "diagnostics summary --json must also omit raw_usb_response_hex without opt-in"
        );
        let parsed_json: serde_json::Value = serde_json::from_slice(&sum_out).unwrap();
        assert_eq!(parsed_json["schema_version"], 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
