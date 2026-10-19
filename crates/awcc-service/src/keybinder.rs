//! Linux `evdev` hotkey discovery and scan-code decoding for Alienware G-Mode (`104` / `0x68`)
//! and Light (`105` / `0x69`) keys.
//!
//! Matches C++ `KeyBinder` (`src/KeyBinder.cpp`).

use std::fs;
use std::path::{Path, PathBuf};

/// `EV_MSC` event type (`0x04`) in `<linux/input-event-codes.h>`.
pub const EV_MSC: u16 = 0x04;
/// `MSC_SCAN` event code (`0x04`) in `<linux/input-event-codes.h>`.
pub const MSC_SCAN: u16 = 0x04;

/// Alienware G-Mode key scan code (`104` / `0x68`) emitted by `"AT Translated Set 2 keyboard"`.
pub const SCANCODE_GMODE_KEY: i32 = 104;
/// Alienware Light / Stealth key scan code (`105` / `0x69`) emitted by `"AT Translated Set 2 keyboard"`.
pub const SCANCODE_LIGHT_KEY: i32 = 105;

/// Semantic action triggered by an Alienware hardware hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    /// Toggle G-Mode (`104` / `0x68`).
    ToggleGMode,
    /// Cycle brightness or Stealth mode (`105` / `0x69`).
    CycleBrightness,
}

/// Decode a Linux `input_event` `(type, code, value)` triple into an `HotkeyAction` if applicable.
#[must_use]
pub const fn decode_input_event(ev_type: u16, ev_code: u16, ev_value: i32) -> Option<HotkeyAction> {
    if ev_type == EV_MSC && ev_code == MSC_SCAN {
        match ev_value {
            SCANCODE_GMODE_KEY => Some(HotkeyAction::ToggleGMode),
            SCANCODE_LIGHT_KEY => Some(HotkeyAction::CycleBrightness),
            _ => None,
        }
    } else {
        None
    }
}

/// Discover the `/dev/input/eventN` device path corresponding to `"AT Translated Set 2 keyboard"`
/// by scanning `<sys_class_input>/event*/device/name`.
#[must_use]
pub fn find_at_keyboard_event_device(
    sys_class_input: &Path,
    dev_input_root: &Path,
) -> Option<PathBuf> {
    let entries = fs::read_dir(sys_class_input).ok()?;
    let mut candidates: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|d| d.path())).collect();
    candidates.sort();

    for entry_path in candidates {
        let file_name = entry_path.file_name()?.to_string_lossy();
        if !file_name.starts_with("event") {
            continue;
        }
        let name_file = entry_path.join("device").join("name");
        if let Ok(name) = fs::read_to_string(&name_file)
            && name.trim() == "AT Translated Set 2 keyboard"
        {
            return Some(dev_input_root.join(file_name.as_ref()));
        }
    }
    None
}

/// Compute the next brightness step in the C++ `KeyBinder::m_pressedLight` cycle
/// (`100 -> 75 -> 50 -> 25 -> 0 -> 100`).
#[must_use]
pub const fn next_brightness_step(current_brightness: u8) -> u8 {
    if current_brightness > 75 {
        75
    } else if current_brightness > 50 {
        50
    } else if current_brightness > 25 {
        25
    } else if current_brightness > 0 {
        0
    } else {
        100
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decode_input_event_recognizes_gmode_104_and_light_105_scancodes() {
        assert_eq!(
            decode_input_event(EV_MSC, MSC_SCAN, 104),
            Some(HotkeyAction::ToggleGMode)
        );
        assert_eq!(
            decode_input_event(EV_MSC, MSC_SCAN, 105),
            Some(HotkeyAction::CycleBrightness)
        );
        assert_eq!(decode_input_event(EV_MSC, MSC_SCAN, 42), None);
        assert_eq!(decode_input_event(0x01, MSC_SCAN, 104), None);
    }

    #[test]
    fn find_at_keyboard_event_device_discovers_matching_sysfs_node() {
        let tmp_dir =
            std::env::temp_dir().join(format!("awcc_rust_keybinder_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        let sys_input = tmp_dir.join("sys_input");
        let dev_input = tmp_dir.join("dev_input");
        fs::create_dir_all(sys_input.join("event0/device")).unwrap();
        fs::create_dir_all(sys_input.join("event3/device")).unwrap();
        fs::write(sys_input.join("event0/device/name"), "Power Button\n").unwrap();
        fs::write(
            sys_input.join("event3/device/name"),
            "AT Translated Set 2 keyboard\n",
        )
        .unwrap();

        let found = find_at_keyboard_event_device(&sys_input, &dev_input);
        assert_eq!(found, Some(dev_input.join("event3")));

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn brightness_cycle_steps_match_keybinder_sequence() {
        assert_eq!(next_brightness_step(100), 75);
        assert_eq!(next_brightness_step(75), 50);
        assert_eq!(next_brightness_step(50), 25);
        assert_eq!(next_brightness_step(25), 0);
        assert_eq!(next_brightness_step(0), 100);
    }
}
