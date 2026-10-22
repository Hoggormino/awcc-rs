//! ELC 2.0 (`187c:0550` / `187c:0551`) USB HID 33-byte lighting protocol,
//! 28-LED chunking, zone/group targeting (`0x0F01` metadata), volatile vs boot-slot `0x0061`
//! persistence, Stealth dimming (`0x26`), and power-button state animations (`0x005B`..=`0x0060`).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Fixed 33-byte USB HID report size for Alienware ELC (`187c:0550` / `0551`).
pub const REPORT_LEN: usize = 33;
/// Preamble byte (`0x03`) at offset 0 of every ELC 2.0 USB report.
pub const PREAMBLE: u8 = 0x03;

/// Query command (`0x20`).
pub const CMD_REQUEST: u8 = 0x20;
pub const REQ_FIRMWARE_VERSION: u8 = 0x00;
pub const REQ_STATUS: u8 = 0x01;
pub const REQ_ELC_CONFIG: u8 = 0x02;
pub const REQ_ANIMATION_COUNT: u8 = 0x03;

/// Standard animation slot command (`0x21`).
pub const CMD_ANIMATION: u8 = 0x21;
/// User/system state animation command (`0x22`), used for power-button state slots `0x005B..=0x0060`.
pub const CMD_USER_ANIMATION: u8 = 0x22;

pub const ANIM_CONFIG_START: u16 = 0x0001;
pub const ANIM_CONFIG_SAVE: u16 = 0x0002;
pub const ANIM_CONFIG_PLAY: u16 = 0x0003;
pub const ANIM_REMOVE: u16 = 0x0004;
pub const ANIM_PLAY: u16 = 0x0005;
pub const ANIM_SET_DEFAULT: u16 = 0x0006;
pub const ANIM_SET_STARTUP: u16 = 0x0007;

/// Zone selection command (`0x23`), chunked to at most 28 LEDs per 33-byte report.
pub const CMD_ZONE_SELECT: u8 = 0x23;
pub const MAX_ZONES_PER_PACKET: usize = 28;

/// Animation action command (`0x24`).
pub const CMD_ADD_ACTION: u8 = 0x24;
pub const ACTION_COLOR: u8 = 0x00;
pub const ACTION_PULSE: u8 = 0x01;
pub const ACTION_MORPH: u8 = 0x02;

/// Brightness dimming command (`0x26`), chunked to at most 28 LEDs per 33-byte report.
pub const CMD_SET_DIM: u8 = 0x26;

/// Direct volatile static color command (`0x27`), chunked to at most 26 LEDs per 33-byte report.
pub const CMD_SET_COLOR: u8 = 0x27;
pub const MAX_SET_COLOR_ZONES_PER_PACKET: usize = 26;

/// Non-volatile boot-default animation slot (`0x0061`).
pub const BOOT_DEFAULT_SLOT: u16 = 0x0061;
/// Volatile live-preview animation slot (`0x0000`).
pub const VOLATILE_SLOT: u16 = 0x0000;
/// Power-button LED ID (`0x02`), excluded from general chassis/keyboard effects.
pub const POWER_BUTTON_LED_ID: u8 = 0x02;

/// 33-byte USB HID report sent to or received from the ELC controller.
pub type HidReport33 = [u8; REPORT_LEN];

/// Pad a slice into a zero-filled 33-byte `HidReport33`.
#[must_use]
pub fn pad_report(bytes: &[u8]) -> HidReport33 {
    let mut report = [0_u8; REPORT_LEN];
    let len = bytes.len().min(REPORT_LEN);
    report[..len].copy_from_slice(&bytes[..len]);
    report
}

/// Decoded response from `0x20 0x02` (`GetELCFWConfiguration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElcConfig {
    pub platform_id: u16,
    pub active_zones: u8,
}

/// Parse an ELC configuration report (`[0x03, 0x20, 0x02, platform_hi, platform_lo, active_zones, ...]`).
#[must_use]
pub fn parse_elc_config_report(buf: &[u8]) -> Option<ElcConfig> {
    if buf.len() < 6 {
        return None;
    }
    let base = if buf[0] == PREAMBLE {
        0
    } else if buf.len() >= 7 && buf[1] == PREAMBLE {
        1
    } else {
        return None;
    };
    if buf[base] == PREAMBLE && buf[base + 1] == CMD_REQUEST && buf[base + 2] == REQ_ELC_CONFIG {
        let platform_id = (u16::from(buf[base + 3]) << 8) | u16::from(buf[base + 4]);
        let active_zones = buf[base + 5];
        Some(ElcConfig {
            platform_id,
            active_zones,
        })
    } else {
        None
    }
}

/// Named zone groups for Alienware ELC 2.0 keyboard and chassis controllers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZoneTarget {
    /// All non-power zones configured for the device (`[0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00]` on m15 R6).
    All,
    /// 4-zone standard keyboard (`[0x04, 0x05, 0x06, 0x07]`).
    Keyboard,
    /// Alienhead lid logo (`[0x03]`).
    Logo,
    /// Rear stadium / tron loop (`[0x01, 0x00]`).
    Tron,
    /// Power-button state LED (`[0x02]`, programmed via `0x22` state slots `0x005B..=0x0060`).
    PowerButton,
    /// Explicit list of zone IDs (excluding `0x02` power button).
    Custom(Vec<u8>),
}

impl ZoneTarget {
    #[must_use]
    pub fn parse_spec(spec: &str) -> Self {
        let normalized = spec.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" | "all" => Self::All,
            "keyboard" | "kbd" => Self::Keyboard,
            "logo" | "alienhead" | "head" => Self::Logo,
            "tron" | "stadium" => Self::Tron,
            "power" | "power-button" | "power-led" | "0x02" | "0x2" | "2" => Self::PowerButton,
            _ => {
                let mut zones = Vec::new();
                for tok in normalized.split(',') {
                    let t = tok.trim();
                    if t.is_empty() {
                        continue;
                    }
                    let parsed = if let Some(hex) = t.strip_prefix("0x") {
                        u8::from_str_radix(hex, 16).ok()
                    } else {
                        t.parse::<u8>().ok()
                    };
                    if let Some(z) = parsed
                        && z != POWER_BUTTON_LED_ID
                    {
                        zones.push(z);
                    }
                }
                Self::Custom(zones)
            }
        }
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::All => "All Zones",
            Self::Keyboard => "Keyboard (4-Zone)",
            Self::Logo => "Alienhead Logo",
            Self::Tron => "Stadium Lights",
            Self::PowerButton => "Power Button",
            Self::Custom(_) => "Custom Zones",
        }
    }

    #[must_use]
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Keyboard,
            2 => Self::Logo,
            3 => Self::Tron,
            4 => Self::PowerButton,
            _ => Self::All,
        }
    }

    #[must_use]
    pub const fn index(&self) -> i32 {
        match self {
            Self::All => 0,
            Self::Keyboard => 1,
            Self::Logo => 2,
            Self::Tron => 3,
            Self::PowerButton => 4,
            Self::Custom(_) => 5,
        }
    }

    /// Resolve the concrete ordered list of ELC zone IDs against the device's `all_zones` list.
    #[must_use]
    pub fn resolve(&self, all_zones: &[u8]) -> Vec<u8> {
        let filter_or_default = |allowed: &[u8]| -> Vec<u8> {
            let filtered: Vec<u8> = all_zones
                .iter()
                .copied()
                .filter(|z| allowed.contains(z))
                .collect();
            if filtered.is_empty() {
                allowed.to_vec()
            } else {
                filtered
            }
        };

        match self {
            Self::All => {
                let mut zones = all_zones.to_vec();
                if !zones.is_empty() && !zones.contains(&POWER_BUTTON_LED_ID) {
                    zones.push(POWER_BUTTON_LED_ID);
                }
                zones
            }
            Self::Keyboard => filter_or_default(&[0x04, 0x05, 0x06, 0x07]),
            Self::Logo => filter_or_default(&[0x03]),
            Self::Tron => filter_or_default(&[0x01, 0x00]),
            Self::PowerButton => vec![POWER_BUTTON_LED_ID],
            Self::Custom(zones) => zones
                .iter()
                .copied()
                .filter(|&z| z != POWER_BUTTON_LED_ID)
                .collect(),
        }
    }
}

impl fmt::Display for ZoneTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Power-button firmware animation slots (`0x005B..=0x0060` = `91..=96`) on LED `0x02`,
/// matching Alienware ELC 2.0 firmware definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum PowerStateSlot {
    /// `91` (`0x005B`): `"AC - Sleep"`
    AcSleep = 0x005B,
    /// `92` (`0x005C`): `"AC - Fully Charge"` (AC Plugged in & Charged)
    AcOn = 0x005C,
    /// `93` (`0x005D`): `"AC - Charging"` (AC Plugged in & Charging)
    Charging = 0x005D,
    /// `94` (`0x005E`): `"DC - Sleep"`
    DcSleep = 0x005E,
    /// `95` (`0x005F`): `"DC - Working"` (DC On / Running on Battery)
    DcOn = 0x005F,
    /// `96` (`0x0060`): `"DC - Low Battery"` (Critical Battery)
    CriticalBattery = 0x0060,
}

impl PowerStateSlot {
    pub const ALL: [Self; 6] = [
        Self::AcSleep,
        Self::AcOn,
        Self::Charging,
        Self::DcSleep,
        Self::DcOn,
        Self::CriticalBattery,
    ];

    #[must_use]
    pub const fn slot_id(self) -> u16 {
        self as u16
    }
}

/// Lighting effect specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightingEffect {
    Static { color: u32 },
    Breathe { color: u32, duration_ms: u16 },
    Spectrum { duration_ms: u16 },
    Wave { color: u32, duration_ms: u16 },
    Rainbow { duration_ms: u16 },
    BackAndForth { color: u32, duration_ms: u16 },
    DefaultBlue,
}

impl LightingEffect {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Static { .. } => "Static",
            Self::Breathe { .. } => "Breathe",
            Self::Spectrum { .. } => "Spectrum",
            Self::Wave { .. } => "Wave",
            Self::Rainbow { .. } => "Rainbow",
            Self::BackAndForth { .. } => "Back and Forth",
            Self::DefaultBlue => "Default Blue",
        }
    }

    #[must_use]
    pub fn from_ui(index: i32, color: u32, duration_ms: u16) -> Self {
        let duration_ms = duration_ms.clamp(50, 10_000);
        match index {
            1 => Self::Breathe { color, duration_ms },
            2 => Self::Spectrum { duration_ms },
            3 => Self::Wave { color, duration_ms },
            4 => Self::Rainbow { duration_ms },
            5 => Self::BackAndForth { color, duration_ms },
            6 => Self::DefaultBlue,
            _ => Self::Static { color },
        }
    }
}

/// Build a 6-byte animation control report (`0x21` or `0x22`).
#[must_use]
pub fn build_animation_cmd(cmd: u8, op: u16, slot_id: u16) -> HidReport33 {
    let raw = [
        PREAMBLE,
        cmd,
        ((op >> 8) & 0xFF) as u8,
        (op & 0xFF) as u8,
        ((slot_id >> 8) & 0xFF) as u8,
        (slot_id & 0xFF) as u8,
    ];
    pad_report(&raw)
}

/// Build a 3-byte query report (`0x20`).
#[must_use]
pub fn build_query_cmd(subcmd: u8) -> HidReport33 {
    pad_report(&[PREAMBLE, CMD_REQUEST, subcmd])
}

/// Build chunked `0x23` (`SendZoneSelect`) 33-byte reports (`<= 28` LEDs per report).
#[must_use]
pub fn build_zone_select_packets(loop_flag: u8, zones: &[u8]) -> Vec<HidReport33> {
    let mut reports = Vec::new();
    for (pkt_idx, chunk) in zones.chunks(MAX_ZONES_PER_PACKET).enumerate() {
        let mut raw = Vec::with_capacity(5 + chunk.len());
        raw.push(PREAMBLE);
        raw.push(CMD_ZONE_SELECT);
        raw.push(loop_flag);
        #[allow(clippy::cast_possible_truncation)]
        raw.push(pkt_idx as u8);
        #[allow(clippy::cast_possible_truncation)]
        raw.push(chunk.len() as u8);
        raw.extend_from_slice(chunk);
        reports.push(pad_report(&raw));
    }
    reports
}

/// Build a single `0x24` (`SendAddAction`) 33-byte report.
#[must_use]
pub fn build_add_action_packet(
    action: u8,
    duration_ms: u16,
    tempo: u16,
    color: u32,
) -> HidReport33 {
    let raw = [
        PREAMBLE,
        CMD_ADD_ACTION,
        action,
        ((duration_ms >> 8) & 0xFF) as u8,
        (duration_ms & 0xFF) as u8,
        ((tempo >> 8) & 0xFF) as u8,
        (tempo & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        (color & 0xFF) as u8,
    ];
    pad_report(&raw)
}

/// Build `0x24` animation action 33-byte reports packing up to 3 actions (`8` bytes each
/// at offsets `2..10`, `10..18`, `18..26`) per report, matching the ELC 2.0 firmware
/// multi-action packing format.
#[must_use]
pub fn build_add_actions_packets(actions: &[(u8, u16, u16, u32)]) -> Vec<HidReport33> {
    let mut reports = Vec::new();
    for chunk in actions.chunks(3) {
        let mut raw = [0_u8; REPORT_LEN];
        raw[0] = PREAMBLE;
        raw[1] = CMD_ADD_ACTION;
        for (idx, &(action, duration_ms, tempo, color)) in chunk.iter().enumerate() {
            let base = 2 + idx * 8;
            raw[base] = action;
            raw[base + 1] = ((duration_ms >> 8) & 0xFF) as u8;
            raw[base + 2] = (duration_ms & 0xFF) as u8;
            raw[base + 3] = ((tempo >> 8) & 0xFF) as u8;
            raw[base + 4] = (tempo & 0xFF) as u8;
            raw[base + 5] = ((color >> 16) & 0xFF) as u8;
            raw[base + 6] = ((color >> 8) & 0xFF) as u8;
            raw[base + 7] = (color & 0xFF) as u8;
        }
        reports.push(raw);
    }
    reports
}

/// Build chunked `0x26` (`SendSetDim`) 33-byte reports (`<= 28` LEDs per report).
#[must_use]
pub fn build_set_dim_packets(dim: u8, zones: &[u8]) -> Vec<HidReport33> {
    let mut reports = Vec::new();
    for (pkt_idx, chunk) in zones.chunks(MAX_ZONES_PER_PACKET).enumerate() {
        let mut raw = Vec::with_capacity(5 + chunk.len());
        raw.push(PREAMBLE);
        raw.push(CMD_SET_DIM);
        raw.push(dim);
        #[allow(clippy::cast_possible_truncation)]
        raw.push(pkt_idx as u8);
        #[allow(clippy::cast_possible_truncation)]
        raw.push(chunk.len() as u8);
        raw.extend_from_slice(chunk);
        reports.push(pad_report(&raw));
    }
    reports
}

/// Build chunked `0x27` (`SendSetColor`) 33-byte reports (`<= 26` LEDs per report).
#[must_use]
pub fn build_set_color_packets(color: u32, zones: &[u8]) -> Vec<HidReport33> {
    let mut reports = Vec::new();
    for chunk in zones.chunks(MAX_SET_COLOR_ZONES_PER_PACKET) {
        let mut raw = Vec::with_capacity(7 + chunk.len());
        raw.push(PREAMBLE);
        raw.push(CMD_SET_COLOR);
        raw.push(((color >> 16) & 0xFF) as u8);
        raw.push(((color >> 8) & 0xFF) as u8);
        raw.push((color & 0xFF) as u8);
        raw.push(0x00);
        #[allow(clippy::cast_possible_truncation)]
        raw.push(chunk.len() as u8);
        raw.extend_from_slice(chunk);
        reports.push(pad_report(&raw));
    }
    reports
}

/// Append the `0x23` (`SendZoneSelect`) and `0x24` (`SendAddAction`) series packets for a single
/// `(effect, zones)` group inside an active animation recording block.
pub fn append_effect_series_packets(
    reports: &mut Vec<HidReport33>,
    effect: LightingEffect,
    zones: &[u8],
) {
    if zones.is_empty() {
        return;
    }

    const MORPH_TEMPO_MS: u16 = 15;
    const RAINBOW_COLORS: [u32; 7] = [
        0xFF0000, 0xFFA500, 0xFFFF00, 0x008000, 0x00BFFF, 0x0000FF, 0x800080,
    ];

    match effect {
        LightingEffect::Static { color } => {
            reports.extend(build_zone_select_packets(1, zones));
            reports.push(build_add_action_packet(ACTION_COLOR, 2000, 250, color));
        }
        LightingEffect::Breathe { color, duration_ms } => {
            reports.extend(build_zone_select_packets(1, zones));
            reports.extend(build_add_actions_packets(&[
                (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, color),
                (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, 0),
            ]));
        }
        LightingEffect::Spectrum { duration_ms } => {
            reports.extend(build_zone_select_packets(1, zones));
            let actions: Vec<(u8, u16, u16, u32)> = RAINBOW_COLORS
                .into_iter()
                .map(|c| (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, c))
                .collect();
            reports.extend(build_add_actions_packets(&actions));
        }
        LightingEffect::Wave { color, duration_ms } => {
            let kb_zones: Vec<u8> = zones.iter().copied().filter(|&z| z >= 0x04).collect();
            let aux_zones: Vec<u8> = zones.iter().copied().filter(|&z| z < 0x04).collect();
            let chaser_zones: &[u8] = if !kb_zones.is_empty() && !aux_zones.is_empty() {
                reports.extend(build_zone_select_packets(1, &aux_zones));
                reports.extend(build_add_actions_packets(&[
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, color),
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, 0),
                ]));
                &kb_zones
            } else {
                zones
            };
            let n = chaser_zones.len();
            if n == 1 {
                reports.extend(build_zone_select_packets(1, chaser_zones));
                reports.extend(build_add_actions_packets(&[
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, color),
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, 0),
                ]));
            } else {
                for (i, &z) in chaser_zones.iter().enumerate() {
                    reports.extend(build_zone_select_packets(1, &[z]));
                    let actions: Vec<(u8, u16, u16, u32)> = (0..n)
                        .map(|j| {
                            let c = if i == j { color } else { 0 };
                            (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, c)
                        })
                        .collect();
                    reports.extend(build_add_actions_packets(&actions));
                }
            }
        }
        LightingEffect::Rainbow { duration_ms } => {
            for (i, &z) in zones.iter().enumerate() {
                reports.extend(build_zone_select_packets(1, &[z]));
                let actions: Vec<(u8, u16, u16, u32)> = (0..RAINBOW_COLORS.len())
                    .map(|j| {
                        let c = RAINBOW_COLORS[(i + j) % RAINBOW_COLORS.len()];
                        (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, c)
                    })
                    .collect();
                reports.extend(build_add_actions_packets(&actions));
            }
        }
        LightingEffect::BackAndForth { color, duration_ms } => {
            let kb_zones: Vec<u8> = zones.iter().copied().filter(|&z| z >= 0x04).collect();
            let aux_zones: Vec<u8> = zones.iter().copied().filter(|&z| z < 0x04).collect();
            let chaser_zones: &[u8] = if !kb_zones.is_empty() && !aux_zones.is_empty() {
                reports.extend(build_zone_select_packets(1, &aux_zones));
                reports.extend(build_add_actions_packets(&[
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, color),
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, 0),
                ]));
                &kb_zones
            } else {
                zones
            };
            let n = chaser_zones.len();
            if n == 1 {
                reports.extend(build_zone_select_packets(1, chaser_zones));
                reports.extend(build_add_actions_packets(&[
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, color),
                    (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, 0),
                ]));
            } else {
                let mut seq: Vec<usize> = (0..n).collect();
                for idx in (1..n.saturating_sub(1)).rev() {
                    seq.push(idx);
                }
                for (i, &z) in chaser_zones.iter().enumerate() {
                    reports.extend(build_zone_select_packets(1, &[z]));
                    let actions: Vec<(u8, u16, u16, u32)> = seq
                        .iter()
                        .map(|&step| {
                            let c = if i == step { color } else { 0 };
                            (ACTION_MORPH, duration_ms, MORPH_TEMPO_MS, c)
                        })
                        .collect();
                    reports.extend(build_add_actions_packets(&actions));
                }
            }
        }
        LightingEffect::DefaultBlue => {
            reports.extend(build_zone_select_packets(1, zones));
            reports.push(build_add_action_packet(ACTION_COLOR, 2000, 250, 0x00FFFF));
        }
    }
}

/// Compile a composite multi-group animation packet sequence covering one or more `(effect, zones)`
/// pairs inside a single ELC animation slot (`0x0061` when `persist_boot_default` is `true`, or
/// volatile slot `0x0000` when `false`).
///
/// Using a composite animation slot allows targeting individual zone groups (`Keyboard`, `Logo`,
/// `Tron`, `PowerButton`) without erasing the active effects on other zones.
#[must_use]
pub fn compile_composite_effect_packets(
    groups: &[(LightingEffect, Vec<u8>)],
    persist_boot_default: bool,
) -> Vec<HidReport33> {
    if groups.iter().all(|(_, z)| z.is_empty()) {
        return Vec::new();
    }

    let mut reports = Vec::new();
    if persist_boot_default {
        reports.push(build_animation_cmd(
            CMD_ANIMATION,
            ANIM_REMOVE,
            BOOT_DEFAULT_SLOT,
        ));
        reports.push(build_animation_cmd(
            CMD_ANIMATION,
            ANIM_CONFIG_START,
            BOOT_DEFAULT_SLOT,
        ));
    } else {
        reports.push(build_animation_cmd(
            CMD_ANIMATION,
            ANIM_CONFIG_START,
            VOLATILE_SLOT,
        ));
    }

    for (effect, zones) in groups {
        append_effect_series_packets(&mut reports, *effect, zones);
    }

    if persist_boot_default {
        reports.push(build_animation_cmd(
            CMD_ANIMATION,
            ANIM_CONFIG_SAVE,
            BOOT_DEFAULT_SLOT,
        ));
        reports.push(build_animation_cmd(
            CMD_ANIMATION,
            ANIM_SET_DEFAULT,
            BOOT_DEFAULT_SLOT,
        ));
    } else {
        reports.push(build_animation_cmd(
            CMD_ANIMATION,
            ANIM_CONFIG_PLAY,
            VOLATILE_SLOT,
        ));
    }

    reports
}

/// Compile a complete `LightingEffect` packet sequence for `zones`.
///
/// When `persist_boot_default` is `true`, records into non-volatile slot `0x0061`
/// (`Remove(0x0061)` -> `Start(0x0061)` -> actions -> `Save(0x0061)` -> `SetDefault(0x0061)`).
/// When `false` (volatile), records into volatile slot `0x0000`
/// (`Start(0x0000)` -> actions -> `ConfigPlay(0x0000)`) so that even `Static` cleanly replaces
/// any running hardware animation.
#[must_use]
pub fn compile_effect_packets(
    effect: LightingEffect,
    zones: &[u8],
    persist_boot_default: bool,
) -> Vec<HidReport33> {
    if zones.is_empty() {
        return Vec::new();
    }
    compile_composite_effect_packets(&[(effect, zones.to_vec())], persist_boot_default)
}

/// Compile the `0x22` (`SendUserAnimConfig`) packet sequence for a single power-button state slot
/// (`0x005B..=0x0060`) on LED `0x02` (`POWER_BUTTON_LED_ID`), matching Dell
/// `LightingManager::savePowerAnimation` (`Remove(0x0004)` -> `Start(0x0001)` -> `ZoneSelect([0x02])`
/// -> `AddAction` -> `Save(0x0002)`).
#[must_use]
pub fn compile_power_button_slot_packets(
    slot: PowerStateSlot,
    color: u32,
    pulse: bool,
) -> Vec<HidReport33> {
    if slot == PowerStateSlot::CriticalBattery && pulse {
        let slot_id = slot.slot_id();
        let mut reports = Vec::with_capacity(5);
        reports.push(build_animation_cmd(
            CMD_USER_ANIMATION,
            ANIM_REMOVE,
            slot_id,
        ));
        reports.push(build_animation_cmd(
            CMD_USER_ANIMATION,
            ANIM_CONFIG_START,
            slot_id,
        ));
        reports.extend(build_zone_select_packets(1, &[POWER_BUTTON_LED_ID]));
        reports.push(build_add_action_packet(ACTION_PULSE, 2000, 100, color));
        reports.push(build_animation_cmd(
            CMD_USER_ANIMATION,
            ANIM_CONFIG_SAVE,
            slot_id,
        ));
        return reports;
    }
    compile_power_button_morph_slot_packets(slot, color, if pulse { 0 } else { color }, 1000)
}

/// Compile a `0x22` (`SendUserAnimConfig`) packet sequence for a single power-button state slot
/// (`0x005B..=0x0060`) on LED `0x02`, either static (`color_a == color_b`) or morphing between
/// `color_a` and `color_b` with both actions packed into a single `0x24` report.
#[must_use]
pub fn compile_power_button_morph_slot_packets(
    slot: PowerStateSlot,
    color_a: u32,
    color_b: u32,
    duration_ms: u16,
) -> Vec<HidReport33> {
    let slot_id = slot.slot_id();
    let mut reports = Vec::with_capacity(5);
    reports.push(build_animation_cmd(
        CMD_USER_ANIMATION,
        ANIM_REMOVE,
        slot_id,
    ));
    reports.push(build_animation_cmd(
        CMD_USER_ANIMATION,
        ANIM_CONFIG_START,
        slot_id,
    ));
    reports.extend(build_zone_select_packets(1, &[POWER_BUTTON_LED_ID]));
    if color_a == color_b {
        reports.push(build_add_action_packet(ACTION_COLOR, 2000, 100, color_a));
    } else {
        let dur = duration_ms.clamp(100, 10_000);
        reports.extend(build_add_actions_packets(&[
            (ACTION_MORPH, dur, 15, color_a),
            (ACTION_MORPH, dur, 15, color_b),
        ]));
    }
    reports.push(build_animation_cmd(
        CMD_USER_ANIMATION,
        ANIM_CONFIG_SAVE,
        slot_id,
    ));
    reports
}

/// Compile all 6 power-button state slots (`0x005B..=0x0060` = `91..=96`) on LED `0x02`
/// matching Alienware ELC 2.0 power-button firmware state slots:
/// - `0x005B` (`91`, `"AC - Sleep"`): Morph `ac_color` <-> `0x000000`
/// - `0x005C` (`92`, `"AC - Fully Charge"`): Static `ac_color`
/// - `0x005D` (`93`, `"AC - Charging"`): Morph `ac_color` <-> `charging_color` (or Static if equal)
/// - `0x005E` (`94`, `"DC - Sleep"`): Morph `dc_color` <-> `0x000000`
/// - `0x005F` (`95`, `"DC - Working"`): Static `dc_color`
/// - `0x0060` (`96`, `"DC - Low Battery"`): Pulse `critical_color`
///
/// Followed by `0x22 ANIM_PLAY (0x0005)` on `0x005C` (`AcOn`) so the ELC activates the updated
/// power-button state immediately.
#[must_use]
pub fn compile_all_power_button_packets(
    ac_color: u32,
    dc_color: u32,
    charging_color: u32,
    critical_color: u32,
) -> Vec<HidReport33> {
    let mut reports = Vec::with_capacity(33);
    reports.extend(build_set_color_packets(ac_color, &[POWER_BUTTON_LED_ID]));
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::AcSleep,
        ac_color,
        true,
    ));
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::AcOn,
        ac_color,
        false,
    ));
    reports.extend(compile_power_button_morph_slot_packets(
        PowerStateSlot::Charging,
        ac_color,
        charging_color,
        1000,
    ));
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::DcSleep,
        dc_color,
        true,
    ));
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::DcOn,
        dc_color,
        false,
    ));
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::CriticalBattery,
        critical_color,
        true,
    ));
    reports.push(build_animation_cmd(
        CMD_USER_ANIMATION,
        ANIM_PLAY,
        PowerStateSlot::AcOn.slot_id(),
    ));
    reports.extend(build_set_color_packets(ac_color, &[POWER_BUTTON_LED_ID]));
    reports
}

/// Compile a `LightingEffect` across the power-button state slots (`0x005B..=0x0060`) on LED `0x02`
/// so that targeting `ZoneTarget::PowerButton` (`0x02`) or `ZoneTarget::All` updates all active
/// power states (`AC - Fully Charge` `0x005C`, `AC - Charging` `0x005D`, `DC - Working` `0x005F`),
/// sleep/low-battery states (`0x005B`, `0x005E`, `0x0060`), issues `0x22 ANIM_PLAY (0x0005)` on
/// `0x005C` (`AcOn`), and updates the `FlashConfig` power-LED color table via `0x27` (`CMD_SET_COLOR`)
/// on LED `0x02` (required on ELC `v1.1.x` firmware where Event 4's `FUN_0800ab0e` restores LED `0x02`
/// from `FlashConfig` when awake).
#[must_use]
pub fn compile_power_button_effect_packets(effect: LightingEffect) -> Vec<HidReport33> {
    let base_color = match effect {
        LightingEffect::Static { color }
        | LightingEffect::Breathe { color, .. }
        | LightingEffect::Wave { color, .. }
        | LightingEffect::BackAndForth { color, .. } => color,
        LightingEffect::Spectrum { .. } | LightingEffect::Rainbow { .. } => 0x00F0FF,
        LightingEffect::DefaultBlue => 0x00FFFF,
    };

    let mut reports = Vec::with_capacity(37);
    reports.extend(build_set_color_packets(base_color, &[POWER_BUTTON_LED_ID]));
    // Sleep slots (0x005B AC Sleep, 0x005E DC Sleep) morph base_color <-> black
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::AcSleep,
        base_color,
        true,
    ));
    // Active awake slots (0x005C AC Fully Charge, 0x005D AC Charging, 0x005F DC Working)
    for slot in [
        PowerStateSlot::AcOn,
        PowerStateSlot::Charging,
        PowerStateSlot::DcOn,
    ] {
        let slot_id = slot.slot_id();
        reports.push(build_animation_cmd(
            CMD_USER_ANIMATION,
            ANIM_REMOVE,
            slot_id,
        ));
        reports.push(build_animation_cmd(
            CMD_USER_ANIMATION,
            ANIM_CONFIG_START,
            slot_id,
        ));
        append_effect_series_packets(&mut reports, effect, &[POWER_BUTTON_LED_ID]);
        reports.push(build_animation_cmd(
            CMD_USER_ANIMATION,
            ANIM_CONFIG_SAVE,
            slot_id,
        ));
    }
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::DcSleep,
        base_color,
        true,
    ));
    reports.extend(compile_power_button_slot_packets(
        PowerStateSlot::CriticalBattery,
        0xFF2828,
        true,
    ));
    reports.push(build_animation_cmd(
        CMD_USER_ANIMATION,
        ANIM_PLAY,
        PowerStateSlot::AcOn.slot_id(),
    ));
    reports.extend(build_set_color_packets(base_color, &[POWER_BUTTON_LED_ID]));
    reports
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn zone_select_and_set_dim_chunking_handles_0_1_7_28_29_56_57_zones() {
        for &count in &[0_usize, 1, 7, 28, 29, 56, 57] {
            let zones: Vec<u8> = (0..count).map(|i| (i & 0x7F) as u8).collect();
            let zs_pkts = build_zone_select_packets(1, &zones);
            let dim_pkts = build_set_dim_packets(40, &zones);

            let expected_pkt_count = count.div_ceil(28);
            assert_eq!(zs_pkts.len(), expected_pkt_count, "count={count}");
            assert_eq!(dim_pkts.len(), expected_pkt_count, "count={count}");

            for (pkt_idx, pkt) in zs_pkts.iter().enumerate() {
                let remaining = count - pkt_idx * 28;
                let expected_chunk = remaining.min(28) as u8;
                assert_eq!(pkt[0], 0x03);
                assert_eq!(pkt[1], 0x23);
                assert_eq!(pkt[2], 1);
                assert_eq!(pkt[3], pkt_idx as u8);
                assert_eq!(pkt[4], expected_chunk);
            }
            for (pkt_idx, pkt) in dim_pkts.iter().enumerate() {
                let remaining = count - pkt_idx * 28;
                let expected_chunk = remaining.min(28) as u8;
                assert_eq!(pkt[0], 0x03);
                assert_eq!(pkt[1], 0x26);
                assert_eq!(pkt[2], 40);
                assert_eq!(pkt[3], pkt_idx as u8);
                assert_eq!(pkt[4], expected_chunk);
            }
        }
    }

    #[test]
    fn zone_target_resolves_m15_r6_groups_and_power_button_0x02() {
        let m15_r6_all = vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00];
        assert_eq!(
            ZoneTarget::All.resolve(&m15_r6_all),
            vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x02]
        );
        assert_eq!(
            ZoneTarget::Keyboard.resolve(&m15_r6_all),
            vec![0x04, 0x05, 0x06, 0x07]
        );
        assert_eq!(ZoneTarget::Logo.resolve(&m15_r6_all), vec![0x03]);
        assert_eq!(ZoneTarget::Tron.resolve(&m15_r6_all), vec![0x01, 0x00]);
        assert_eq!(ZoneTarget::PowerButton.resolve(&m15_r6_all), vec![0x02]);
        assert_eq!(ZoneTarget::parse_spec("power"), ZoneTarget::PowerButton);
        assert_eq!(ZoneTarget::parse_spec("0x02"), ZoneTarget::PowerButton);

        let custom = ZoneTarget::parse_spec("0x04, 0x02, 0x05");
        assert_eq!(custom.resolve(&m15_r6_all), vec![0x04, 0x05]);
    }

    #[test]
    fn volatile_vs_boot_slot_0x0061_packets_are_distinct() {
        let zones = vec![0x03, 0x04];
        let volatile_static =
            compile_effect_packets(LightingEffect::Static { color: 0x00D1FF }, &zones, false);
        // Volatile Static uses slot 0x0000: Start(0) + ZoneSelect([3,4]) + AddAction(Color, 2000, 250) + Play(0) = 4 packets
        assert_eq!(volatile_static.len(), 4);
        assert_eq!(
            &volatile_static[0][..6],
            &[0x03, 0x21, 0x00, 0x01, 0x00, 0x00]
        );
        assert_eq!(
            &volatile_static[1][..7],
            &[0x03, 0x23, 0x01, 0x00, 0x02, 0x03, 0x04]
        );
        assert_eq!(
            &volatile_static[2][..10],
            &[0x03, 0x24, 0x00, 0x07, 0xD0, 0x00, 0xFA, 0x00, 0xD1, 0xFF]
        );
        assert_eq!(
            &volatile_static[3][..6],
            &[0x03, 0x21, 0x00, 0x03, 0x00, 0x00]
        );

        let persistent_static =
            compile_effect_packets(LightingEffect::Static { color: 0x00D1FF }, &zones, true);
        // Persistent Static uses slot 0x0061: Remove(0x61) + Start(0x61) + ZoneSelect + AddAction + Save(0x61) + SetDefault(0x61) = 6 packets
        assert_eq!(persistent_static.len(), 6);
        assert_eq!(
            &persistent_static[0][..6],
            &[0x03, 0x21, 0x00, 0x04, 0x00, 0x61]
        );
        assert_eq!(
            &persistent_static[4][..6],
            &[0x03, 0x21, 0x00, 0x02, 0x00, 0x61]
        );
        assert_eq!(
            &persistent_static[5][..6],
            &[0x03, 0x21, 0x00, 0x06, 0x00, 0x61]
        );
    }

    #[test]
    fn power_button_slots_use_0x22_on_led_0x02_for_platform_0x0f01() {
        assert_eq!(PowerStateSlot::AcSleep.slot_id(), 0x005B);
        assert_eq!(PowerStateSlot::AcOn.slot_id(), 0x005C);
        assert_eq!(PowerStateSlot::Charging.slot_id(), 0x005D);
        assert_eq!(PowerStateSlot::DcSleep.slot_id(), 0x005E);
        assert_eq!(PowerStateSlot::DcOn.slot_id(), 0x005F);
        assert_eq!(PowerStateSlot::CriticalBattery.slot_id(), 0x0060);

        let pkts = compile_power_button_slot_packets(PowerStateSlot::AcOn, 0x00F59B, false);
        assert_eq!(pkts.len(), 5);
        assert_eq!(&pkts[0][..6], &[0x03, 0x22, 0x00, 0x04, 0x00, 0x5C]);
        assert_eq!(&pkts[1][..6], &[0x03, 0x22, 0x00, 0x01, 0x00, 0x5C]);
        assert_eq!(&pkts[2][..6], &[0x03, 0x23, 0x01, 0x00, 0x01, 0x02]);
        assert_eq!(&pkts[4][..6], &[0x03, 0x22, 0x00, 0x02, 0x00, 0x5C]);

        // Morph slot packs both actions into a single 0x24 report (5 packets total per slot) with 15ms tempo (0x000F)
        let sleep_pkts = compile_power_button_slot_packets(PowerStateSlot::AcSleep, 0x00F59B, true);
        assert_eq!(sleep_pkts.len(), 5);
        assert_eq!(
            &sleep_pkts[3][..18],
            &[
                0x03, 0x24, 0x02, 0x03, 0xE8, 0x00, 0x0F, 0x00, 0xF5, 0x9B, 0x02, 0x03, 0xE8, 0x00,
                0x0F, 0x00, 0x00, 0x00
            ]
        );
    }

    #[test]
    fn back_and_forth_and_wave_spatial_separation_and_single_zone_morph() {
        // Single-zone BackAndForth (e.g., Logo 0x03) morphs color <-> black with 15ms tempo
        let single_bnf = compile_effect_packets(
            LightingEffect::BackAndForth {
                color: 0xF59E0B,
                duration_ms: 600,
            },
            &[0x03],
            false,
        );
        // Start(0) + ZoneSelect([3]) + AddActions([(Morph, 600, 15, 0xF59E0B), (Morph, 600, 15, 0)]) + Play(0) = 4 packets
        assert_eq!(single_bnf.len(), 4);
        assert_eq!(
            &single_bnf[2][..18],
            &[
                0x03, 0x24, 0x02, 0x02, 0x58, 0x00, 0x0F, 0xF5, 0x9E, 0x0B, 0x02, 0x02, 0x58, 0x00,
                0x0F, 0x00, 0x00, 0x00
            ]
        );

        // Multi-zone BackAndForth on All Zones (chassis + 4-zone keyboard) separates aux zones (< 0x04)
        // into continuous 2-step morphs and runs the 6-step chaser on the 4 keyboard zones (0x04..=0x07).
        let all_zones = vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x02];
        let all_bnf = compile_effect_packets(
            LightingEffect::BackAndForth {
                color: 0xF59E0B,
                duration_ms: 600,
            },
            &all_zones,
            true,
        );
        // Remove(0x61) + Start(0x61) + aux(ZoneSelect([1,3,0,2]) + 1 AddActions) + 4 kb zones * (ZoneSelect + 2 AddActions for 6 steps) + Save(0x61) + SetDefault(0x61) = 18 packets
        assert_eq!(all_bnf.len(), 18);
        assert_eq!(
            &all_bnf[2][..9],
            &[0x03, 0x23, 0x01, 0x00, 0x04, 0x01, 0x03, 0x00, 0x02]
        );
    }

    #[test]
    fn elc_config_report_parses_m15_r6_platform_0x0f01() {
        let mut raw = [0_u8; 33];
        raw[0] = 0x03;
        raw[1] = 0x20;
        raw[2] = 0x02;
        raw[3] = 0x0F;
        raw[4] = 0x01;
        raw[5] = 0x08;
        let cfg = parse_elc_config_report(&raw).expect("valid ELC config");
        assert_eq!(cfg.platform_id, 0x0F01);
        assert_eq!(cfg.active_zones, 8);
    }
}
