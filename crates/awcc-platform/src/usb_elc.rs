//! USB HID transport and `EffectController` for Alienware ELC 2.0 LED controllers
//! (`0x187c:0x0550` and `0x187c:0x0551`).
//!
//! Matches C++ `LightFX` (`src/LightFX.cpp`) and `EffectController` (`src/EffectController.cpp`)
//! 33-byte HID control transfer framing (`bmRequestType = 0x21`, `bRequest = 0x09`,
//! `wValue = 0x0202`, `wIndex = 0` for send; `0xA1`, `0x01`, `0x0101`, `0` for receive)
//! while providing deterministic simulation and write-audit instrumentation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use awcc_core::lighting::{
    ANIM_CONFIG_SAVE, ANIM_PLAY, ANIM_REMOVE, ANIM_SET_DEFAULT, CMD_ADD_ACTION, CMD_ANIMATION,
    CMD_REQUEST, CMD_SET_COLOR, CMD_SET_DIM, CMD_USER_ANIMATION, CMD_ZONE_SELECT, ElcConfig,
    HidReport33, LightingEffect, POWER_BUTTON_LED_ID, PREAMBLE, PowerStateSlot,
    REQ_ANIMATION_COUNT, REQ_ELC_CONFIG, REQ_FIRMWARE_VERSION, REQ_STATUS, ZoneTarget,
    build_query_cmd, build_set_color_packets, build_set_dim_packets,
    compile_all_power_button_packets, compile_composite_effect_packets,
    compile_power_button_effect_packets, compile_power_button_slot_packets,
    parse_elc_config_report,
};
use rusb::UsbContext;
use serde::{Deserialize, Serialize};

use crate::{AckStatus, TransportError, WriteAuditEntry};

/// USB Vendor ID for Alienware LED controllers (`0x187c`).
pub const ELC_VENDOR_ID: u16 = 0x187c;
/// Supported USB Product IDs for Alienware ELC 2.0 (`0x0550`, `0x0551`).
pub const ELC_PRODUCT_IDS: [u16; 2] = [0x0550, 0x0551];

/// Format the non-zero prefix (at least 10 bytes) of a 33-byte HID report as hex for audit logs.
#[must_use]
pub fn format_report_hex(report: &HidReport33) -> String {
    let end = report
        .iter()
        .rposition(|&b| b != 0)
        .map_or(8, |idx| (idx + 1).max(8).min(report.len()));
    report[..end]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Diagnostic summary returned by `raw-status` / ELC inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElcDiagnosticsSummary {
    pub vendor_id: u16,
    pub product_id: u16,
    pub status_byte: u8,
    pub firmware_version: (u8, u8, u8),
    pub animation_count: u16,
    pub elc_config: ElcConfig,
}

/// Result of one outgoing report: the transfer itself succeeded (otherwise `send_report`
/// returns `Err`), and `ack` records the separate, optional acknowledgement read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    pub ack: AckStatus,
}

impl SendReceipt {
    /// A receipt for a report that has no acknowledgement phase (query requests).
    #[must_use]
    pub const fn without_ack() -> Self {
        Self {
            ack: AckStatus::NotApplicable,
        }
    }
}

/// Aggregate result of a successful multi-report USB transaction.
///
/// `reports_sent` counts *outgoing transfers that completed in full*; the acknowledgement
/// results are kept apart so a caller can never mistake "sent" for "device confirmed".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransactionReceipt {
    pub reports_sent: usize,
    pub acks_received: usize,
    /// `(1-based report index, status)` for every acknowledgement that was missing/incomplete.
    pub ack_problems: Vec<(usize, AckStatus)>,
    /// Non-fatal problems after the transfers (releasing the interface, persisting brightness).
    pub warnings: Vec<String>,
}

impl TransactionReceipt {
    /// Receipt for an operation that resolved to zero reports.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// `true` when every sent report was followed by an intact acknowledgement read.
    #[must_use]
    pub fn fully_acknowledged(&self) -> bool {
        self.reports_sent > 0
            && self.ack_problems.is_empty()
            && self.acks_received == self.reports_sent
    }

    /// One-sentence description of the acknowledgement evidence.
    #[must_use]
    pub fn ack_summary(&self) -> String {
        if self.reports_sent == 0 {
            return "no reports were sent".to_string();
        }
        if self.fully_acknowledged() {
            return format!(
                "{} of {} reports acknowledged by the controller (acknowledgement content is not interpreted)",
                self.acks_received, self.reports_sent
            );
        }
        let first = self
            .ack_problems
            .first()
            .map(|(idx, status)| format!("report {idx}: {}", status.describe()))
            .unwrap_or_else(|| "no acknowledgement phase".to_string());
        format!(
            "{} of {} reports acknowledged; {} acknowledgement(s) missing or incomplete ({first})",
            self.acks_received,
            self.reports_sent,
            self.ack_problems.len()
        )
    }
}

/// A USB transaction that did not complete. Preserves how far it got so partial application is
/// never confused with a clean failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbTransactionError {
    pub reports_sent: usize,
    pub reports_total: usize,
    pub cause: TransportError,
}

impl UsbTransactionError {
    /// `true` when at least one report reached the controller before the failure, so the
    /// controller may be in a partially updated state.
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        self.reports_sent > 0
    }
}

impl std::fmt::Display for UsbTransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_partial() {
            write!(
                f,
                "{} (after {} of {} reports were already sent; the controller state may be partially updated)",
                self.cause, self.reports_sent, self.reports_total
            )
        } else {
            write!(f, "{}", self.cause)
        }
    }
}

impl std::error::Error for UsbTransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

impl From<TransportError> for UsbTransactionError {
    fn from(cause: TransportError) -> Self {
        Self {
            reports_sent: 0,
            reports_total: 0,
            cause,
        }
    }
}

/// Where the lighting controller's remembered state (brightness, effect, stealth) comes from.
///
/// The ELC protocol exposes no readback of applied colours or brightness, so these values are
/// only ever *remembered*, never observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightingStateSource {
    /// Built-in default assumed at startup; nothing has been commanded in this process.
    AssumedDefault,
    /// Brightness restored from the persisted brightness file; the device was not queried.
    PersistedBrightness,
    /// Set by a command in this process whose transfers completed.
    Commanded,
}

/// Abstract USB HID 33-byte control transfer transport for Alienware ELC 2.0 controllers.
pub trait UsbHidTransport: Send {
    /// Whether this transport is backed by simulation rather than physical USB hardware.
    fn is_simulated(&self) -> bool;

    /// USB `(vendor_id, product_id)` pair (`0x187c`, `0x0550` or `0x0551`).
    fn device_id(&self) -> (u16, u16);

    /// Open the USB device (`libusb_open_device_with_vid_pid`).
    fn open(&mut self) -> Result<(), TransportError>;

    /// Detach kernel driver (if active) and claim USB interface `0`.
    fn acquire(&mut self) -> Result<(), TransportError>;

    /// Release USB interface `0`.
    fn release(&mut self) -> Result<(), TransportError>;

    /// Close the USB device handle and re-attach kernel driver if previously detached.
    fn close(&mut self) -> Result<(), TransportError>;

    /// Send a 33-byte HID feature/output report (`0x21`, `0x09`, `0x0202`, `0`).
    ///
    /// `Ok` means the outgoing transfer completed in full. The optional acknowledgement read that
    /// follows a mutating report is reported separately in [`SendReceipt::ack`] and never turns a
    /// completed transfer into an error.
    fn send_report(&mut self, report: &HidReport33) -> Result<SendReceipt, TransportError>;

    /// Receive a 33-byte HID feature/input report (`0xA1`, `0x01`, `0x0101`, `0`).
    fn receive_report(&mut self) -> Result<HidReport33, TransportError>;

    /// Inspect all USB HID reports sent since initialization.
    fn sent_reports(&self) -> &[HidReport33];

    /// Inspect the human-readable write audit log.
    fn write_audit_log(&self) -> &[WriteAuditEntry];

    /// Inspect currently tracked per-zone RGB colors.
    fn zone_colors(&self) -> &BTreeMap<u8, u32>;

    /// Inspect currently tracked power-button state slot colors (`0x005B..=0x0060`).
    fn power_slot_colors(&self) -> &BTreeMap<u16, u32>;
}

impl UsbHidTransport for Box<dyn UsbHidTransport> {
    fn is_simulated(&self) -> bool {
        (**self).is_simulated()
    }

    fn device_id(&self) -> (u16, u16) {
        (**self).device_id()
    }

    fn open(&mut self) -> Result<(), TransportError> {
        (**self).open()
    }

    fn acquire(&mut self) -> Result<(), TransportError> {
        (**self).acquire()
    }

    fn release(&mut self) -> Result<(), TransportError> {
        (**self).release()
    }

    fn close(&mut self) -> Result<(), TransportError> {
        (**self).close()
    }

    fn send_report(&mut self, report: &HidReport33) -> Result<SendReceipt, TransportError> {
        (**self).send_report(report)
    }

    fn receive_report(&mut self) -> Result<HidReport33, TransportError> {
        (**self).receive_report()
    }

    fn sent_reports(&self) -> &[HidReport33] {
        (**self).sent_reports()
    }

    fn write_audit_log(&self) -> &[WriteAuditEntry] {
        (**self).write_audit_log()
    }

    fn zone_colors(&self) -> &BTreeMap<u8, u32> {
        (**self).zone_colors()
    }

    fn power_slot_colors(&self) -> &BTreeMap<u16, u32> {
        (**self).power_slot_colors()
    }
}

fn track_elc_report_state(
    report: &HidReport33,
    dim_value: &mut u8,
    pending_zones: &mut Vec<u8>,
    pending_power_slot: &mut Option<u16>,
    pending_color_recorded: &mut bool,
    zone_colors: &mut BTreeMap<u8, u32>,
    power_slot_colors: &mut BTreeMap<u16, u32>,
) {
    if report[0] != PREAMBLE {
        return;
    }
    match report[1] {
        CMD_SET_DIM => {
            *dim_value = report[2];
        }
        CMD_SET_COLOR => {
            let color =
                (u32::from(report[2]) << 16) | (u32::from(report[3]) << 8) | u32::from(report[4]);
            let count = usize::from(report[6]).min(26);
            for &z in &report[7..7 + count] {
                zone_colors.insert(z, color);
                if z == POWER_BUTTON_LED_ID {
                    power_slot_colors.insert(PowerStateSlot::AcOn.slot_id(), color);
                }
            }
        }
        CMD_ANIMATION => {
            *pending_power_slot = None;
        }
        CMD_USER_ANIMATION => {
            let op = u16::from_be_bytes([report[2], report[3]]);
            let slot_id = u16::from_be_bytes([report[4], report[5]]);
            if op == 0x0001 {
                *pending_power_slot = Some(slot_id);
            } else if op == 0x0002 || op == 0x0005 || op == 0x0006 {
                *pending_power_slot = None;
            }
        }
        CMD_ZONE_SELECT => {
            let count = usize::from(report[4]).min(28);
            if report[3] == 0 {
                pending_zones.clear();
                *pending_color_recorded = false;
            }
            pending_zones.extend_from_slice(&report[5..5 + count]);
        }
        CMD_ADD_ACTION if !*pending_color_recorded => {
            for slot_idx in 0..3 {
                let base = 2 + slot_idx * 8;
                let duration_ms = u16::from_be_bytes([report[base + 1], report[base + 2]]);
                let tempo = u16::from_be_bytes([report[base + 3], report[base + 4]]);
                if duration_ms == 0 && tempo == 0 {
                    break;
                }
                let color = (u32::from(report[base + 5]) << 16)
                    | (u32::from(report[base + 6]) << 8)
                    | u32::from(report[base + 7]);
                if color != 0 {
                    if let Some(slot) = *pending_power_slot {
                        power_slot_colors.insert(slot, color);
                        if slot == PowerStateSlot::AcOn.slot_id() {
                            zone_colors.insert(POWER_BUTTON_LED_ID, color);
                        }
                    } else {
                        for &z in pending_zones.iter() {
                            zone_colors.insert(z, color);
                            if z == POWER_BUTTON_LED_ID {
                                power_slot_colors.insert(PowerStateSlot::AcOn.slot_id(), color);
                            }
                        }
                    }
                    *pending_color_recorded = true;
                    break;
                }
            }
        }
        _ => {}
    }
}

/// Deterministic simulated Alienware ELC 2.0 controller (`0x187c:0x0550`, `0x0F01` Alienware m15 R6).
#[derive(Debug, Clone)]
pub struct SimUsbElcTransport {
    vendor_id: u16,
    product_id: u16,
    is_open: bool,
    is_acquired: bool,
    last_query_subcmd: Option<u8>,
    pending_zones: Vec<u8>,
    pending_power_slot: Option<u16>,
    pending_color_recorded: bool,
    sent_reports: Vec<HidReport33>,
    write_log: Vec<WriteAuditEntry>,
    next_write_seq: usize,
    zone_colors: BTreeMap<u8, u32>,
    power_slot_colors: BTreeMap<u16, u32>,
    dim_value: u8,
    /// Fault injection: acknowledgement status returned for every mutating report.
    ack_fault: Option<AckStatus>,
    /// Fault injection: fail the Nth outgoing transfer (1-based) and every later one.
    fail_sends_from: Option<usize>,
    mutating_sends: usize,
    /// Fault injection: every `receive_report` (diagnostic query response) fails with this text.
    receive_failure: Option<String>,
}

impl Default for SimUsbElcTransport {
    fn default() -> Self {
        Self::new_alienware_m15_r6()
    }
}

impl SimUsbElcTransport {
    /// Construct a simulated ELC `187c:0550` controller pre-configured as `0x0F01` (`Alienware m15 R6`).
    #[must_use]
    pub fn new_alienware_m15_r6() -> Self {
        let mut zone_colors = BTreeMap::new();
        for z in 0x00_u8..=0x07 {
            zone_colors.insert(z, 0x00F0FF);
        }
        let mut power_slot_colors = BTreeMap::new();
        power_slot_colors.insert(PowerStateSlot::AcOn.slot_id(), 0x00F0FF);
        power_slot_colors.insert(PowerStateSlot::AcSleep.slot_id(), 0x00F0FF);
        power_slot_colors.insert(PowerStateSlot::DcOn.slot_id(), 0x00C8FF);
        power_slot_colors.insert(PowerStateSlot::DcSleep.slot_id(), 0x00C8FF);
        power_slot_colors.insert(PowerStateSlot::Charging.slot_id(), 0xFFB400);
        power_slot_colors.insert(PowerStateSlot::CriticalBattery.slot_id(), 0xFF2828);

        Self {
            vendor_id: ELC_VENDOR_ID,
            product_id: ELC_PRODUCT_IDS[0],
            is_open: false,
            is_acquired: false,
            last_query_subcmd: None,
            pending_zones: Vec::new(),
            pending_power_slot: None,
            pending_color_recorded: false,
            sent_reports: Vec::new(),
            write_log: Vec::new(),
            next_write_seq: 0,
            zone_colors,
            power_slot_colors,
            dim_value: 0,
            ack_fault: None,
            fail_sends_from: None,
            mutating_sends: 0,
            receive_failure: None,
        }
    }

    /// Make every diagnostic response read fail with `reason` (`None` restores health).
    pub fn inject_receive_failure(&mut self, reason: Option<String>) {
        self.receive_failure = reason;
    }

    /// Make every later mutating report complete its transfer but report `fault` for the
    /// acknowledgement read (`None` restores the healthy acknowledgement).
    pub fn inject_ack_fault(&mut self, fault: Option<AckStatus>) {
        self.ack_fault = fault;
    }

    /// Fail the `n`th mutating outgoing transfer (1-based, counted from now) and all later ones.
    pub fn inject_send_failure_from(&mut self, n: Option<usize>) {
        self.mutating_sends = 0;
        self.fail_sends_from = n;
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.is_open
    }

    #[must_use]
    pub const fn is_acquired(&self) -> bool {
        self.is_acquired
    }

    #[must_use]
    pub const fn zone_colors(&self) -> &BTreeMap<u8, u32> {
        &self.zone_colors
    }

    #[must_use]
    pub const fn power_slot_colors(&self) -> &BTreeMap<u16, u32> {
        &self.power_slot_colors
    }

    #[must_use]
    pub const fn dim_value(&self) -> u8 {
        self.dim_value
    }

    /// Clear recorded reports (useful in unit tests between steps).
    pub fn clear_recorded_reports(&mut self) {
        self.sent_reports.clear();
        self.write_log.clear();
    }

    fn record_write(
        &mut self,
        command: String,
        outcome: impl Into<String>,
        succeeded: bool,
        ack: AckStatus,
        raw_usb_response_hex: Option<String>,
    ) {
        self.next_write_seq += 1;
        let sequence = self.next_write_seq;
        self.write_log.push(WriteAuditEntry {
            sequence,
            command,
            outcome: outcome.into(),
            succeeded,
            ack,
            op_id: None,
            raw_usb_response_hex,
        });
        if self.write_log.len() > crate::MAX_TRANSPORT_AUDIT_ENTRIES {
            let excess = self.write_log.len() - crate::MAX_TRANSPORT_AUDIT_ENTRIES;
            self.write_log.drain(..excess);
        }
    }

    fn push_sent_report(&mut self, report: HidReport33) {
        self.sent_reports.push(report);
        if self.sent_reports.len() > crate::MAX_TRANSPORT_AUDIT_ENTRIES {
            let excess = self.sent_reports.len() - crate::MAX_TRANSPORT_AUDIT_ENTRIES;
            self.sent_reports.drain(..excess);
        }
    }
}

impl UsbHidTransport for SimUsbElcTransport {
    fn is_simulated(&self) -> bool {
        true
    }

    fn device_id(&self) -> (u16, u16) {
        (self.vendor_id, self.product_id)
    }

    fn open(&mut self) -> Result<(), TransportError> {
        self.is_open = true;
        Ok(())
    }

    fn acquire(&mut self) -> Result<(), TransportError> {
        if !self.is_open {
            self.is_open = true;
        }
        self.is_acquired = true;
        Ok(())
    }

    fn release(&mut self) -> Result<(), TransportError> {
        self.is_acquired = false;
        Ok(())
    }

    fn close(&mut self) -> Result<(), TransportError> {
        self.is_acquired = false;
        self.is_open = false;
        Ok(())
    }

    fn send_report(&mut self, report: &HidReport33) -> Result<SendReceipt, TransportError> {
        // Track query vs mutating commands
        if report[0] == PREAMBLE && report[1] == CMD_REQUEST {
            self.push_sent_report(*report);
            self.last_query_subcmd = Some(report[2]);
            return Ok(SendReceipt::without_ack());
        }

        self.mutating_sends += 1;
        let hex = format_report_hex(report);
        if let Some(from) = self.fail_sends_from
            && self.mutating_sends >= from
        {
            self.record_write(
                format!("USB HID 187c:{:04x} OUT [{hex}]", self.product_id),
                "simulated libusb transfer error",
                false,
                AckStatus::NotApplicable,
                None,
            );
            return Err(TransportError::TransportUnavailable(
                "simulated libusb control transfer write failed".to_string(),
            ));
        }
        self.push_sent_report(*report);

        // Update simulated state when mutating commands arrive
        track_elc_report_state(
            report,
            &mut self.dim_value,
            &mut self.pending_zones,
            &mut self.pending_power_slot,
            &mut self.pending_color_recorded,
            &mut self.zone_colors,
            &mut self.power_slot_colors,
        );

        let ack = self.ack_fault.clone().unwrap_or(AckStatus::Received);
        let raw_resp = matches!(ack, AckStatus::Received).then(|| {
            let mut ack_buf = [0_u8; 33];
            ack_buf[0] = PREAMBLE;
            ack_buf[1] = report[1];
            ack_buf[2] = report[2];
            format_report_hex(&ack_buf)
        });
        self.record_write(
            format!("USB HID 187c:{:04x} OUT [{hex}]", self.product_id),
            "33 bytes transferred",
            true,
            ack.clone(),
            raw_resp,
        );
        Ok(SendReceipt { ack })
    }

    fn receive_report(&mut self) -> Result<HidReport33, TransportError> {
        if let Some(reason) = &self.receive_failure {
            return Err(TransportError::TransportUnavailable(reason.clone()));
        }
        let mut resp = [0u8; 33];
        resp[0] = PREAMBLE;
        resp[1] = CMD_REQUEST;
        let subcmd = self.last_query_subcmd.unwrap_or(REQ_STATUS);
        resp[2] = subcmd;
        match subcmd {
            REQ_FIRMWARE_VERSION => {
                // Firmware version 2.0.5
                resp[3] = 2;
                resp[4] = 0;
                resp[5] = 5;
            }
            REQ_STATUS => {
                // Status ready = 0x01
                resp[3] = 0x01;
            }
            REQ_ELC_CONFIG => {
                // ELC Config: platform 0x0F01 (Alienware m15 R6), 8 active zones
                resp[3] = 0x0F;
                resp[4] = 0x01;
                resp[5] = 8;
            }
            REQ_ANIMATION_COUNT => {
                // Animation count = 7
                resp[3] = 0x00;
                resp[4] = 0x07;
            }
            _ => {}
        }
        Ok(resp)
    }

    fn sent_reports(&self) -> &[HidReport33] {
        &self.sent_reports
    }

    fn write_audit_log(&self) -> &[WriteAuditEntry] {
        &self.write_log
    }

    fn zone_colors(&self) -> &BTreeMap<u8, u32> {
        &self.zone_colors
    }

    fn power_slot_colors(&self) -> &BTreeMap<u16, u32> {
        &self.power_slot_colors
    }
}

/// Real Linux `libusb` (`rusb`) USB HID transport for Alienware ELC 2.0 LED controllers
/// (`0x187c:0x0550` / `0x187c:0x0551`), matching C++ `LightFX` (`src/LightFX.cpp`)
/// with hardware ACK drain and SPI flash pacing.
pub struct LinuxUsbElcTransport {
    vendor_id: u16,
    product_id: u16,
    handle: Option<rusb::DeviceHandle<rusb::Context>>,
    is_acquired: bool,
    kernel_driver_detached: bool,
    dim_value: u8,
    pending_zones: Vec<u8>,
    pending_power_slot: Option<u16>,
    pending_color_recorded: bool,
    sent_reports: Vec<HidReport33>,
    write_log: Vec<WriteAuditEntry>,
    next_write_seq: usize,
    zone_colors: BTreeMap<u8, u32>,
    power_slot_colors: BTreeMap<u16, u32>,
}

impl Default for LinuxUsbElcTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxUsbElcTransport {
    /// Construct a new `LinuxUsbElcTransport` for `0x187c:0x0550` / `0x0551`.
    /// Does not open or claim the USB device until an operation or query is executed.
    #[must_use]
    pub fn new() -> Self {
        let mut zone_colors = BTreeMap::new();
        for z in 0x00_u8..=0x07 {
            zone_colors.insert(z, 0x00F0FF);
        }
        let mut power_slot_colors = BTreeMap::new();
        power_slot_colors.insert(PowerStateSlot::AcOn.slot_id(), 0x00F0FF);
        power_slot_colors.insert(PowerStateSlot::AcSleep.slot_id(), 0x00F0FF);
        power_slot_colors.insert(PowerStateSlot::DcOn.slot_id(), 0x00C8FF);
        power_slot_colors.insert(PowerStateSlot::DcSleep.slot_id(), 0x00C8FF);
        power_slot_colors.insert(PowerStateSlot::Charging.slot_id(), 0xFFB400);
        power_slot_colors.insert(PowerStateSlot::CriticalBattery.slot_id(), 0xFF2828);

        Self {
            vendor_id: ELC_VENDOR_ID,
            product_id: ELC_PRODUCT_IDS[0],
            handle: None,
            is_acquired: false,
            kernel_driver_detached: false,
            dim_value: 0,
            pending_zones: Vec::new(),
            pending_power_slot: None,
            pending_color_recorded: false,
            sent_reports: Vec::new(),
            write_log: Vec::new(),
            next_write_seq: 0,
            zone_colors,
            power_slot_colors,
        }
    }

    fn record_write(
        &mut self,
        command: String,
        outcome: impl Into<String>,
        succeeded: bool,
        ack: AckStatus,
        raw_usb_response_hex: Option<String>,
    ) {
        self.next_write_seq += 1;
        let sequence = self.next_write_seq;
        self.write_log.push(WriteAuditEntry {
            sequence,
            command,
            outcome: outcome.into(),
            succeeded,
            ack,
            op_id: None,
            raw_usb_response_hex,
        });
        if self.write_log.len() > crate::MAX_TRANSPORT_AUDIT_ENTRIES {
            let excess = self.write_log.len() - crate::MAX_TRANSPORT_AUDIT_ENTRIES;
            self.write_log.drain(..excess);
        }
    }
}

impl UsbHidTransport for LinuxUsbElcTransport {
    fn is_simulated(&self) -> bool {
        false
    }

    fn device_id(&self) -> (u16, u16) {
        (self.vendor_id, self.product_id)
    }

    fn open(&mut self) -> Result<(), TransportError> {
        if self.handle.is_some() {
            return Ok(());
        }
        let ctx = rusb::Context::new().map_err(|e| {
            TransportError::TransportUnavailable(format!("libusb init failed: {e}"))
        })?;
        let devices = ctx.devices().map_err(|e| {
            TransportError::TransportUnavailable(format!("libusb get_device_list failed: {e}"))
        })?;
        for device in devices.iter() {
            let Ok(desc) = device.device_descriptor() else {
                continue;
            };
            if desc.vendor_id() == ELC_VENDOR_ID && ELC_PRODUCT_IDS.contains(&desc.product_id()) {
                let handle = device.open().map_err(|e| {
                    TransportError::TransportUnavailable(format!(
                        "failed to open Alienware ELC {:04x}:{:04x}: {e}",
                        desc.vendor_id(),
                        desc.product_id()
                    ))
                })?;
                self.vendor_id = desc.vendor_id();
                self.product_id = desc.product_id();
                self.handle = Some(handle);
                return Ok(());
            }
        }
        Err(TransportError::TransportUnavailable(
            "Alienware ELC USB controller (187c:0550 / 187c:0551) not found".to_string(),
        ))
    }

    fn acquire(&mut self) -> Result<(), TransportError> {
        if self.is_acquired {
            return Ok(());
        }
        if self.handle.is_none() {
            self.open()?;
        }
        let Some(handle) = self.handle.as_mut() else {
            return Err(TransportError::TransportUnavailable(
                "ELC USB device handle not open".to_string(),
            ));
        };
        if let Ok(true) = handle.kernel_driver_active(0) {
            handle.detach_kernel_driver(0).map_err(|e| {
                TransportError::TransportUnavailable(format!(
                    "failed to detach kernel driver on ELC interface 0: {e}"
                ))
            })?;
            self.kernel_driver_detached = true;
        }
        if let Err(e) = handle.claim_interface(0) {
            return Err(TransportError::TransportUnavailable(format!(
                "failed to claim ELC USB interface 0: {e}"
            )));
        }
        self.is_acquired = true;
        Ok(())
    }

    fn release(&mut self) -> Result<(), TransportError> {
        if !self.is_acquired {
            return Ok(());
        }
        if let Some(handle) = self.handle.as_mut() {
            // Release interface 0 so other awcc processes can claim it, but do NOT re-attach
            // the kernel usbhid driver between transactions (which causes usbhid re-probing storms
            // and spurious SET_IDLE/GET_REPORT control transfers to the ELC).
            let _ = handle.release_interface(0);
        }
        self.is_acquired = false;
        Ok(())
    }

    fn close(&mut self) -> Result<(), TransportError> {
        if self.is_acquired {
            let _ = self.release();
        }
        if let Some(handle) = self.handle.as_mut()
            && self.kernel_driver_detached
        {
            let _ = handle.attach_kernel_driver(0);
            self.kernel_driver_detached = false;
        }
        self.handle = None;
        Ok(())
    }

    fn send_report(&mut self, report: &HidReport33) -> Result<SendReceipt, TransportError> {
        if !self.is_acquired {
            return Err(TransportError::TransportUnavailable(
                "ELC USB device not acquired before send_report".to_string(),
            ));
        }
        let Some(handle) = self.handle.as_ref() else {
            return Err(TransportError::TransportUnavailable(
                "ELC USB device handle not open".to_string(),
            ));
        };

        let is_query = report[0] == PREAMBLE && report[1] == CMD_REQUEST;
        let hex = format_report_hex(report);
        match handle.write_control(0x21, 0x09, 0x0202, 0, report, Duration::from_millis(1000)) {
            Ok(written) if written == report.len() => {
                self.sent_reports.push(*report);
                if self.sent_reports.len() > crate::MAX_TRANSPORT_AUDIT_ENTRIES {
                    let excess = self.sent_reports.len() - crate::MAX_TRANSPORT_AUDIT_ENTRIES;
                    self.sent_reports.drain(..excess);
                }
                if is_query {
                    return Ok(SendReceipt::without_ack());
                }
                // Hardware timing: sleep *before* reading the ELC input report so SPI flash
                // erase/save/default/color operations complete in the STM32 main loop and
                // enqueue their response into the USB TX queue before the control read is issued.
                let pre_ack_sleep_ms = if report[0] == PREAMBLE
                    && (report[1] == CMD_ANIMATION || report[1] == CMD_USER_ANIMATION)
                {
                    let op = u16::from_be_bytes([report[2], report[3]]);
                    if op == ANIM_CONFIG_SAVE {
                        if report[1] == CMD_USER_ANIMATION {
                            250
                        } else {
                            150
                        }
                    } else if op == ANIM_REMOVE {
                        150
                    } else if op == ANIM_SET_DEFAULT || op == ANIM_PLAY {
                        50
                    } else {
                        15
                    }
                } else if report[0] == PREAMBLE
                    && (report[1] == CMD_SET_COLOR || report[1] == CMD_SET_DIM)
                {
                    50
                } else {
                    15
                };
                std::thread::sleep(Duration::from_millis(pre_ack_sleep_ms));

                // The acknowledgement read is best-effort evidence, not a protocol requirement:
                // the legacy implementation never reads one after a set command. Its result is
                // recorded separately from the (already completed) outgoing transfer and never
                // turns that transfer into an error.
                let mut ack_buf = [0_u8; 33];
                let (ack, raw_resp) = match handle.read_control(
                    0xA1,
                    0x01,
                    0x0101,
                    0,
                    &mut ack_buf,
                    Duration::from_millis(500),
                ) {
                    Ok(n) if n == ack_buf.len() => {
                        (AckStatus::Received, Some(format_report_hex(&ack_buf)))
                    }
                    Ok(bytes) => (AckStatus::Incomplete { bytes }, None),
                    Err(e) => (
                        AckStatus::Failed {
                            reason: e.to_string(),
                        },
                        None,
                    ),
                };

                track_elc_report_state(
                    report,
                    &mut self.dim_value,
                    &mut self.pending_zones,
                    &mut self.pending_power_slot,
                    &mut self.pending_color_recorded,
                    &mut self.zone_colors,
                    &mut self.power_slot_colors,
                );
                self.record_write(
                    format!("USB HID 187c:{:04x} OUT [{hex}]", self.product_id),
                    "33 bytes transferred",
                    true,
                    ack.clone(),
                    raw_resp,
                );
                Ok(SendReceipt { ack })
            }
            Ok(written) => {
                if !is_query {
                    self.record_write(
                        format!("USB HID 187c:{:04x} OUT [{hex}]", self.product_id),
                        format!("partial write ({written}/33 bytes)"),
                        false,
                        AckStatus::NotApplicable,
                        None,
                    );
                }
                Err(TransportError::TransportUnavailable(format!(
                    "short USB HID control transfer write ({written}/33 bytes)"
                )))
            }
            Err(e) => {
                if !is_query {
                    self.record_write(
                        format!("USB HID 187c:{:04x} OUT [{hex}]", self.product_id),
                        format!("libusb error: {e}"),
                        false,
                        AckStatus::NotApplicable,
                        None,
                    );
                }
                Err(TransportError::TransportUnavailable(format!(
                    "libusb control transfer write failed: {e}"
                )))
            }
        }
    }

    fn receive_report(&mut self) -> Result<HidReport33, TransportError> {
        if !self.is_acquired {
            return Err(TransportError::TransportUnavailable(
                "ELC USB device not acquired before receive_report".to_string(),
            ));
        }
        let Some(handle) = self.handle.as_ref() else {
            return Err(TransportError::TransportUnavailable(
                "ELC USB device handle not open".to_string(),
            ));
        };

        std::thread::sleep(Duration::from_millis(10));
        let mut buf = [0_u8; 33];
        let read_len = handle
            .read_control(0xA1, 0x01, 0x0101, 0, &mut buf, Duration::from_millis(1000))
            .map_err(|e| {
                TransportError::TransportUnavailable(format!(
                    "libusb control transfer read failed: {e}"
                ))
            })?;
        if read_len == 0 {
            return Err(TransportError::TransportUnavailable(
                "libusb control transfer read returned 0 bytes".to_string(),
            ));
        }
        Ok(buf)
    }

    fn sent_reports(&self) -> &[HidReport33] {
        &self.sent_reports
    }

    fn write_audit_log(&self) -> &[WriteAuditEntry] {
        &self.write_log
    }

    fn zone_colors(&self) -> &BTreeMap<u8, u32> {
        &self.zone_colors
    }

    fn power_slot_colors(&self) -> &BTreeMap<u16, u32> {
        &self.power_slot_colors
    }
}

impl Drop for LinuxUsbElcTransport {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// High-level lighting & ELC controller matching C++ `EffectController` (`src/EffectController.cpp`)
/// with Phase 3 extensions: per-zone/group targeting, volatile vs boot-slot `0x0061`, Stealth mode,
/// effect speed (`duration_ms`), and power-button state animations (`0x005B..=0x0060`).
#[derive(Debug, Clone)]
pub struct EffectController<T: UsbHidTransport> {
    transport: T,
    zones: Vec<u8>,
    zone_effects: BTreeMap<u8, LightingEffect>,
    brightness: u8,
    pre_stealth_brightness: u8,
    stealth_enabled: bool,
    brightness_path_override: Option<PathBuf>,
    state_source: LightingStateSource,
}

impl<T: UsbHidTransport> EffectController<T> {
    /// Construct a new `EffectController` with the given USB transport and active zone IDs.
    #[must_use]
    pub fn new(transport: T, zones: Vec<u8>) -> Self {
        let persisted =
            Self::read_brightness_from_path(Path::new("/etc/awcc/brightness")).filter(|&b| b > 0);
        let state_source = if persisted.is_some() {
            LightingStateSource::PersistedBrightness
        } else {
            LightingStateSource::AssumedDefault
        };
        let initial_brightness = persisted.unwrap_or(100);
        let mut zone_effects = BTreeMap::new();
        for &z in &zones {
            zone_effects.insert(z, LightingEffect::Static { color: 0x00F0FF });
        }
        zone_effects
            .entry(POWER_BUTTON_LED_ID)
            .or_insert(LightingEffect::Static { color: 0x00F0FF });
        Self {
            transport,
            zones,
            zone_effects,
            brightness: initial_brightness,
            pre_stealth_brightness: initial_brightness,
            stealth_enabled: false,
            brightness_path_override: None,
            state_source,
        }
    }

    /// Override the brightness state file path (for hermetic offline testing).
    #[must_use]
    pub fn with_brightness_path(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if let Some(b) = Self::read_brightness_from_path(&path) {
            self.brightness = b;
            if b > 0 {
                self.pre_stealth_brightness = b;
            }
            self.state_source = LightingStateSource::PersistedBrightness;
        }
        self.brightness_path_override = Some(path);
        self
    }

    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    #[must_use]
    pub fn zones(&self) -> &[u8] {
        &self.zones
    }

    pub fn set_zones(&mut self, zones: Vec<u8>) {
        for &z in &zones {
            self.zone_effects
                .entry(z)
                .or_insert(LightingEffect::Static { color: 0x00F0FF });
        }
        self.zone_effects
            .entry(POWER_BUTTON_LED_ID)
            .or_insert(LightingEffect::Static { color: 0x00F0FF });
        self.zones = zones;
    }

    #[must_use]
    pub const fn brightness(&self) -> u8 {
        self.brightness
    }

    #[must_use]
    pub const fn is_stealth(&self) -> bool {
        self.stealth_enabled
    }

    /// Provenance of the remembered brightness/stealth/effect state. The ELC protocol has no
    /// readback for these, so they are assumed, restored, or commanded — never observed.
    #[must_use]
    pub const fn state_source(&self) -> LightingStateSource {
        self.state_source
    }

    fn brightness_file_path(&self) -> PathBuf {
        self.brightness_path_override
            .clone()
            .unwrap_or_else(|| PathBuf::from("/etc/awcc/brightness"))
    }

    fn read_brightness_from_path(path: &Path) -> Option<u8> {
        let content = fs::read_to_string(path).ok()?;
        let parsed = content.trim().parse::<u16>().ok()?;
        if parsed <= 100 {
            #[allow(clippy::cast_possible_truncation)]
            Some(parsed as u8)
        } else {
            None
        }
    }

    /// Persist the brightness preference. A failure is not a device failure, so it is returned
    /// as a warning for the caller to surface instead of being discarded.
    fn persist_brightness(&self, brightness: u8) -> Option<String> {
        let path = self.brightness_file_path();
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return Some(format!(
                "brightness {brightness}% was sent but not saved to {}: {e}",
                path.display()
            ));
        }
        fs::write(&path, format!("{brightness}\n")).err().map(|e| {
            format!(
                "brightness {brightness}% was sent but not saved to {}: {e}",
                path.display()
            )
        })
    }

    fn run_transaction(
        &mut self,
        reports: &[HidReport33],
    ) -> Result<TransactionReceipt, UsbTransactionError> {
        let total = reports.len();
        self.transport
            .acquire()
            .map_err(|cause| UsbTransactionError {
                reports_sent: 0,
                reports_total: total,
                cause,
            })?;
        let mut receipt = TransactionReceipt::empty();
        for report in reports {
            match self.transport.send_report(report) {
                Ok(sent) => {
                    receipt.reports_sent += 1;
                    match sent.ack {
                        AckStatus::Received => receipt.acks_received += 1,
                        AckStatus::NotApplicable => {}
                        problem => receipt.ack_problems.push((receipt.reports_sent, problem)),
                    }
                }
                Err(cause) => {
                    let _ = self.transport.release();
                    return Err(UsbTransactionError {
                        reports_sent: receipt.reports_sent,
                        reports_total: total,
                        cause,
                    });
                }
            }
        }
        // Every transfer already completed; failing to release the interface afterwards is
        // cleanup trouble, not evidence that the writes failed.
        if let Err(e) = self.transport.release() {
            receipt
                .warnings
                .push(format!("USB interface release failed after transfer: {e}"));
        }
        Ok(receipt)
    }

    fn all_dimmable_zones(&self) -> Vec<u8> {
        let mut zones = self.zones.clone();
        if !zones.contains(&POWER_BUTTON_LED_ID) {
            zones.push(POWER_BUTTON_LED_ID);
        }
        zones
    }

    /// Set hardware brightness (`0..=100%`) across all configured zones (including Power LED `0x02`)
    /// and persist to `/etc/awcc/brightness`.
    pub fn set_brightness(
        &mut self,
        brightness_pct: u8,
    ) -> Result<TransactionReceipt, UsbTransactionError> {
        let clamped = brightness_pct.min(100);
        let dim = 100_u8.saturating_sub(clamped);
        let reports = build_set_dim_packets(dim, &self.all_dimmable_zones());
        let mut receipt = self.run_transaction(&reports)?;
        self.brightness = clamped;
        self.state_source = LightingStateSource::Commanded;
        if clamped > 0 {
            self.pre_stealth_brightness = clamped;
            self.stealth_enabled = false;
        } else {
            self.stealth_enabled = true;
        }
        receipt.warnings.extend(self.persist_brightness(clamped));
        Ok(receipt)
    }

    /// Enable or disable Stealth (`Go Dark`) mode, preserving pre-stealth brightness on restore.
    pub fn set_stealth(&mut self, enable: bool) -> Result<TransactionReceipt, UsbTransactionError> {
        let zones = self.all_dimmable_zones();
        let mut receipt;
        if enable {
            if self.brightness > 0 {
                self.pre_stealth_brightness = self.brightness;
            }
            let reports = build_set_dim_packets(100, &zones);
            receipt = self.run_transaction(&reports)?;
            self.brightness = 0;
            self.stealth_enabled = true;
        } else {
            let restore = if self.pre_stealth_brightness > 0 {
                self.pre_stealth_brightness
            } else {
                100
            };
            let dim = 100_u8.saturating_sub(restore);
            let reports = build_set_dim_packets(dim, &zones);
            receipt = self.run_transaction(&reports)?;
            self.brightness = restore;
            self.stealth_enabled = false;
            receipt.warnings.extend(self.persist_brightness(restore));
        }
        self.state_source = LightingStateSource::Commanded;
        Ok(receipt)
    }

    /// Toggle Stealth (`Go Dark`) mode.
    pub fn toggle_stealth(&mut self) -> Result<TransactionReceipt, UsbTransactionError> {
        self.set_stealth(!self.stealth_enabled)
    }

    fn build_composite_groups(
        &self,
        default_effect: LightingEffect,
        extra_zones: &[u8],
    ) -> Vec<(LightingEffect, Vec<u8>)> {
        let mut ordered_zones: Vec<u8> = self.zones.clone();
        if !ordered_zones.is_empty() && !ordered_zones.contains(&POWER_BUTTON_LED_ID) {
            ordered_zones.push(POWER_BUTTON_LED_ID);
        }
        for &z in extra_zones {
            if !ordered_zones.contains(&z) {
                ordered_zones.push(z);
            }
        }

        let mut groups: Vec<(LightingEffect, Vec<u8>)> = Vec::new();
        for z in ordered_zones {
            let eff = self.zone_effects.get(&z).copied().unwrap_or(default_effect);
            if let Some((_, grp_zones)) = groups.iter_mut().find(|(e, _)| *e == eff) {
                grp_zones.push(z);
            } else {
                groups.push((eff, vec![z]));
            }
        }
        groups
    }

    /// Apply a lighting effect to `target` zones, either volatilely (live session slot `0x0000`)
    /// or persisted into boot default slot `0x0061` (`persist_boot == true`), while preserving
    /// the active effects on all other configured zones. When `target` includes LED `0x02`
    /// (`ZoneTarget::PowerButton` or `ZoneTarget::All`), sends the `0x21` composite animation
    /// *first* and then programs the hardware power-button state slots (`0x005B..=0x0060`) and
    /// `0x27` `FlashConfig` color on LED `0x02` *second* so Event 4 from `0x22 FINISH_SAVE` never
    /// races an in-progress `0x21` recording.
    pub fn apply_effect(
        &mut self,
        effect: LightingEffect,
        target: &ZoneTarget,
        persist_boot: bool,
    ) -> Result<TransactionReceipt, UsbTransactionError> {
        let resolved = target.resolve(&self.zones);
        if resolved.is_empty() {
            return Ok(TransactionReceipt::empty());
        }
        // Remember the previous per-zone effects: they must only change once the transfers
        // actually completed, otherwise a failed request would silently become the baseline for
        // every later composite effect.
        let previous_effects = self.zone_effects.clone();
        for &z in &resolved {
            self.zone_effects.insert(z, effect);
        }

        let groups = self.build_composite_groups(effect, &resolved);
        let mut reports = compile_composite_effect_packets(&groups, persist_boot);
        if resolved.contains(&POWER_BUTTON_LED_ID) {
            reports.extend(compile_power_button_effect_packets(effect));
        }
        match self.run_transaction(&reports) {
            Ok(receipt) => {
                self.state_source = LightingStateSource::Commanded;
                Ok(receipt)
            }
            Err(err) => {
                self.zone_effects = previous_effects;
                Err(err)
            }
        }
    }

    /// Apply direct volatile per-zone color (`0x27` `SendSetColor`) without touching flash animations.
    pub fn set_direct_color(
        &mut self,
        color: u32,
        target: &ZoneTarget,
    ) -> Result<TransactionReceipt, UsbTransactionError> {
        let resolved = target.resolve(&self.zones);
        let reports = build_set_color_packets(color, &resolved);
        self.run_transaction(&reports)
    }

    /// Program a single power-button state slot (`0x005B..=0x0060`) on LED `0x02`.
    pub fn set_power_button_slot(
        &mut self,
        slot: PowerStateSlot,
        color: u32,
        pulse: bool,
    ) -> Result<TransactionReceipt, UsbTransactionError> {
        let mut reports = compile_power_button_slot_packets(slot, color, pulse);
        if matches!(
            slot,
            PowerStateSlot::AcOn | PowerStateSlot::Charging | PowerStateSlot::DcOn
        ) {
            reports.extend(build_set_color_packets(color, &[POWER_BUTTON_LED_ID]));
        }
        self.run_transaction(&reports)
    }

    /// Program all 6 power-button state slots (`0x005B..=0x0060`) on LED `0x02` matching
    /// C++ `EffectController::setAllPowerButtonStates`, synchronize LED `0x02` in the
    /// active `0x21` composite animation first, and then program `0x22` slots + `0x27` `FlashConfig`
    /// color on LED `0x02`.
    pub fn set_all_power_button_states(
        &mut self,
        ac_color: u32,
        dc_color: u32,
        charging_color: u32,
        critical_color: u32,
    ) -> Result<TransactionReceipt, UsbTransactionError> {
        let ac_effect = LightingEffect::Static { color: ac_color };
        let previous_power_effect = self.zone_effects.insert(POWER_BUTTON_LED_ID, ac_effect);
        let groups = self.build_composite_groups(ac_effect, &[POWER_BUTTON_LED_ID]);
        let mut reports = compile_composite_effect_packets(&groups, true);
        reports.extend(compile_all_power_button_packets(
            ac_color,
            dc_color,
            charging_color,
            critical_color,
        ));
        match self.run_transaction(&reports) {
            Ok(receipt) => {
                self.state_source = LightingStateSource::Commanded;
                Ok(receipt)
            }
            Err(err) => {
                match previous_power_effect {
                    Some(prev) => {
                        self.zone_effects.insert(POWER_BUTTON_LED_ID, prev);
                    }
                    None => {
                        self.zone_effects.remove(&POWER_BUTTON_LED_ID);
                    }
                }
                Err(err)
            }
        }
    }

    fn query_while_acquired(&mut self, subcmd: u8) -> Result<HidReport33, TransportError> {
        self.transport.send_report(&build_query_cmd(subcmd))?;
        self.transport.receive_report()
    }

    const fn report_base(resp: &HidReport33) -> usize {
        if resp[0] == PREAMBLE {
            0
        } else if resp[1] == PREAMBLE {
            1
        } else {
            0
        }
    }

    /// Query ELC `0x20 0x02` platform ID and active zone count.
    pub fn query_elc_config(&mut self) -> Result<ElcConfig, TransportError> {
        self.transport.acquire()?;
        let res = self.query_while_acquired(REQ_ELC_CONFIG);
        let _ = self.transport.release();
        let resp = res?;
        parse_elc_config_report(&resp).ok_or_else(|| {
            TransportError::TransportUnavailable("Invalid ELC 0x20 0x02 config response".into())
        })
    }

    /// Query ELC `0x20 0x01` status byte.
    pub fn query_status(&mut self) -> Result<u8, TransportError> {
        self.transport.acquire()?;
        let res = self.query_while_acquired(REQ_STATUS);
        let _ = self.transport.release();
        let resp = res?;
        let base = Self::report_base(&resp);
        Ok(resp[base + 3])
    }

    /// Query ELC `0x20 0x00` firmware version `(major, minor, patch)`.
    pub fn query_firmware_version(&mut self) -> Result<(u8, u8, u8), TransportError> {
        self.transport.acquire()?;
        let res = self.query_while_acquired(REQ_FIRMWARE_VERSION);
        let _ = self.transport.release();
        let resp = res?;
        let base = Self::report_base(&resp);
        Ok((resp[base + 3], resp[base + 4], resp[base + 5]))
    }

    /// Query ELC `0x20 0x03` stored animation count.
    pub fn query_animation_count(&mut self) -> Result<u16, TransportError> {
        self.transport.acquire()?;
        let res = self.query_while_acquired(REQ_ANIMATION_COUNT);
        let _ = self.transport.release();
        let resp = res?;
        let base = Self::report_base(&resp);
        Ok(u16::from_be_bytes([resp[base + 3], resp[base + 4]]))
    }

    /// Query full ELC diagnostics summary (`raw-status` parity) within a single acquired USB session.
    pub fn query_diagnostics_summary(&mut self) -> Result<ElcDiagnosticsSummary, TransportError> {
        self.transport.acquire()?;
        let res = (|| {
            let status_resp = self.query_while_acquired(REQ_STATUS)?;
            let s_base = Self::report_base(&status_resp);
            let status_byte = status_resp[s_base + 3];

            let fw_resp = self.query_while_acquired(REQ_FIRMWARE_VERSION)?;
            let fw_base = Self::report_base(&fw_resp);
            let firmware_version = (
                fw_resp[fw_base + 3],
                fw_resp[fw_base + 4],
                fw_resp[fw_base + 5],
            );

            let anim_resp = self.query_while_acquired(REQ_ANIMATION_COUNT)?;
            let a_base = Self::report_base(&anim_resp);
            let animation_count =
                u16::from_be_bytes([anim_resp[a_base + 3], anim_resp[a_base + 4]]);

            let cfg_resp = self.query_while_acquired(REQ_ELC_CONFIG)?;
            let elc_config = parse_elc_config_report(&cfg_resp).ok_or_else(|| {
                TransportError::TransportUnavailable("Invalid ELC 0x20 0x02 config response".into())
            })?;

            let (vendor_id, product_id) = self.transport.device_id();
            Ok(ElcDiagnosticsSummary {
                vendor_id,
                product_id,
                status_byte,
                firmware_version,
                animation_count,
                elc_config,
            })
        })();
        let _ = self.transport.release();
        res
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn effect_controller_queries_elc_diagnostics_without_write_audit_pollution() {
        let sim = SimUsbElcTransport::new_alienware_m15_r6();
        let zones = vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00];
        let mut ctrl = EffectController::new(sim, zones);

        let diag = ctrl
            .query_diagnostics_summary()
            .expect("query ELC diagnostics");
        assert_eq!(diag.vendor_id, 0x187c);
        assert_eq!(diag.product_id, 0x0550);
        assert_eq!(diag.status_byte, 0x01);
        assert_eq!(diag.firmware_version, (2, 0, 5));
        assert_eq!(diag.animation_count, 7);
        assert_eq!(diag.elc_config.platform_id, 0x0F01);
        assert_eq!(diag.elc_config.active_zones, 8);
        // Passive 0x20 queries must not pollute the mutating write audit log
        assert!(ctrl.transport().write_audit_log().is_empty());
    }

    #[test]
    fn effect_controller_stealth_toggle_and_brightness_persistence() {
        let tmp_dir =
            std::env::temp_dir().join(format!("awcc_rust_brightness_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();
        let b_file = tmp_dir.join("brightness");
        fs::write(&b_file, "80\n").unwrap();

        let sim = SimUsbElcTransport::new_alienware_m15_r6();
        let zones = vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00];
        let mut ctrl = EffectController::new(sim, zones).with_brightness_path(&b_file);
        assert_eq!(ctrl.brightness(), 80);
        assert!(!ctrl.is_stealth());

        // Enable stealth -> dim_value becomes 100 (off), brightness 0, file still 80
        ctrl.set_stealth(true).unwrap();
        assert!(ctrl.is_stealth());
        assert_eq!(ctrl.brightness(), 0);
        assert_eq!(ctrl.transport().dim_value(), 100);
        assert_eq!(fs::read_to_string(&b_file).unwrap().trim(), "80");

        // Toggle stealth off -> restores 80% brightness (dim_value = 20)
        ctrl.toggle_stealth().unwrap();
        assert!(!ctrl.is_stealth());
        assert_eq!(ctrl.brightness(), 80);
        assert_eq!(ctrl.transport().dim_value(), 20);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn effect_controller_zone_targeting_preserves_other_zones_and_programs_power_button() {
        let sim = SimUsbElcTransport::new_alienware_m15_r6();
        let zones = vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00];
        let mut ctrl = EffectController::new(sim, zones);

        // Volatile Static effect on Keyboard only (4 zones: 4, 5, 6, 7), preserving 0x00F0FF on Tron (1, 0), Logo (3) & Power (2):
        // Start(0x0000) + ZoneSelect([1, 3, 0, 2]) + AddAction(0x00F0FF) + ZoneSelect([4, 5, 6, 7]) + AddAction(0xFF0040) + Play(0x0000) = 6 reports
        let sent_volatile = ctrl
            .apply_effect(
                LightingEffect::Static { color: 0xFF0040 },
                &ZoneTarget::Keyboard,
                false,
            )
            .unwrap()
            .reports_sent;
        assert_eq!(sent_volatile, 6);
        assert!(!ctrl.transport().is_acquired());
        assert_eq!(ctrl.transport().zone_colors()[&0x04], 0xFF0040);
        assert_eq!(ctrl.transport().zone_colors()[&0x07], 0xFF0040);
        // Logo (0x03), Tron (0x00, 0x01), and Power (0x02) preserved at 0x00F0FF!
        assert_eq!(ctrl.transport().zone_colors()[&0x03], 0x00F0FF);
        assert_eq!(ctrl.transport().zone_colors()[&0x00], 0x00F0FF);
        assert_eq!(ctrl.transport().zone_colors()[&0x02], 0x00F0FF);

        ctrl.transport_mut().clear_recorded_reports();

        // Persisted Static effect on Logo (0x03) -> preserves Tron/Power (0x00F0FF) AND Keyboard (0xFF0040):
        // Remove(0x61) + Start(0x61) + 3 series * (ZoneSelect + AddAction) + Save(0x61) + SetDefault(0x61) = 10 reports
        let sent_persist = ctrl
            .apply_effect(
                LightingEffect::Static { color: 0x00FF80 },
                &ZoneTarget::Logo,
                true,
            )
            .unwrap()
            .reports_sent;
        assert_eq!(sent_persist, 10);
        assert_eq!(ctrl.transport().zone_colors()[&0x03], 0x00FF80);
        assert_eq!(ctrl.transport().zone_colors()[&0x04], 0xFF0040);
        assert_eq!(ctrl.transport().zone_colors()[&0x00], 0x00F0FF);

        ctrl.transport_mut().clear_recorded_reports();

        // Program all 6 power-button state slots (1 SetColor + 6 * 5 + 1 Play + 1 SetColor = 33 reports) + composite 0x21 (10 reports) = 43 reports
        let sent_pwr = ctrl
            .set_all_power_button_states(0x00F0FF, 0x0078FF, 0xFFB400, 0xFF0000)
            .unwrap()
            .reports_sent;
        assert_eq!(sent_pwr, 43);
        assert_eq!(
            ctrl.transport().power_slot_colors()[&PowerStateSlot::AcOn.slot_id()],
            0x00F0FF
        );
        assert_eq!(
            ctrl.transport().power_slot_colors()[&PowerStateSlot::DcOn.slot_id()],
            0x0078FF
        );

        ctrl.transport_mut().clear_recorded_reports();

        // ZoneTarget::All also programs the power-button state slots (0x005B..=0x0060) and 0x27 SetColor on LED 0x02
        let sent_all = ctrl
            .apply_effect(
                LightingEffect::Static { color: 0xFFA000 },
                &ZoneTarget::All,
                true,
            )
            .unwrap()
            .reports_sent;
        assert!(sent_all > 32);
        assert_eq!(ctrl.transport().zone_colors()[&0x02], 0xFFA000);
        assert_eq!(
            ctrl.transport().power_slot_colors()[&PowerStateSlot::AcOn.slot_id()],
            0xFFA000
        );
    }

    #[test]
    fn back_and_forth_and_wave_update_all_zone_preview_colors_including_alienhead_0x03() {
        let sim = SimUsbElcTransport::new_alienware_m15_r6();
        let zones = vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00];
        let mut ctrl = EffectController::new(sim, zones);

        // Apply BackAndForth (Solar Amber 0xF59E0B, 600ms) on ZoneTarget::All
        ctrl.apply_effect(
            LightingEffect::BackAndForth {
                color: 0xF59E0B,
                duration_ms: 600,
            },
            &ZoneTarget::All,
            true,
        )
        .unwrap();

        for z in 0x00_u8..=0x07 {
            assert_eq!(
                ctrl.transport().zone_colors()[&z],
                0xF59E0B,
                "BackAndForth must update preview color for zone 0x{z:02X}"
            );
        }

        // Apply Wave (Emerald 0x10B981, 500ms) on ZoneTarget::Keyboard only
        ctrl.apply_effect(
            LightingEffect::Wave {
                color: 0x10B981,
                duration_ms: 500,
            },
            &ZoneTarget::Keyboard,
            false,
        )
        .unwrap();

        for z in 0x04_u8..=0x07 {
            assert_eq!(
                ctrl.transport().zone_colors()[&z],
                0x10B981,
                "Wave on Keyboard must update preview color for zone 0x{z:02X}"
            );
        }
        assert_eq!(ctrl.transport().zone_colors()[&0x03], 0xF59E0B);
    }
    fn healthy_controller() -> EffectController<SimUsbElcTransport> {
        let zones = vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00];
        EffectController::new(SimUsbElcTransport::new_alienware_m15_r6(), zones)
    }

    #[test]
    fn completed_transfer_with_failed_ack_keeps_transfer_and_ack_outcomes_distinct() {
        let mut ctrl = healthy_controller();
        ctrl.transport_mut()
            .inject_ack_fault(Some(AckStatus::Failed {
                reason: "timed out".to_string(),
            }));

        let receipt = ctrl
            .apply_effect(
                LightingEffect::Static { color: 0xFF0040 },
                &ZoneTarget::Keyboard,
                false,
            )
            .expect("an unacknowledged transfer is not a transfer failure");

        // All six outgoing transfers completed...
        assert_eq!(receipt.reports_sent, 6);
        // ...but not one acknowledgement arrived, and that is recorded rather than hidden.
        assert_eq!(receipt.acks_received, 0);
        assert_eq!(receipt.ack_problems.len(), 6);
        assert!(!receipt.fully_acknowledged());
        assert!(
            receipt
                .ack_summary()
                .starts_with("0 of 6 reports acknowledged")
        );
        assert!(receipt.ack_summary().contains("timed out"));

        // The audit trail records the transfer as successful and the ack separately.
        let log = ctrl.transport().write_audit_log();
        assert_eq!(log.len(), 6);
        assert!(log.iter().all(|e| e.succeeded));
        assert!(
            log.iter()
                .all(|e| matches!(e.ack, AckStatus::Failed { .. }))
        );
        // Remembered (commanded) state follows the completed transfers, not the ack.
        assert_eq!(ctrl.state_source(), LightingStateSource::Commanded);
    }

    #[test]
    fn short_ack_read_is_counted_as_a_problem_not_a_failure() {
        let mut ctrl = healthy_controller();
        ctrl.transport_mut()
            .inject_ack_fault(Some(AckStatus::Incomplete { bytes: 8 }));
        let receipt = ctrl.set_brightness(40).expect("transfer completes");
        assert!(receipt.reports_sent > 0);
        assert_eq!(receipt.acks_received, 0);
        assert!(
            receipt
                .ack_problems
                .iter()
                .all(|(_, s)| *s == AckStatus::Incomplete { bytes: 8 })
        );
        assert_eq!(ctrl.brightness(), 40);
    }

    #[test]
    fn healthy_transfers_report_every_ack_received() {
        let mut ctrl = healthy_controller();
        let receipt = ctrl
            .apply_effect(
                LightingEffect::Static { color: 0x112233 },
                &ZoneTarget::Logo,
                false,
            )
            .unwrap();
        assert!(receipt.fully_acknowledged());
        assert_eq!(receipt.acks_received, receipt.reports_sent);
        assert!(
            ctrl.transport()
                .write_audit_log()
                .iter()
                .all(|e| e.ack == AckStatus::Received)
        );
    }

    #[test]
    fn partial_transfer_failure_reports_progress_and_restores_effect_baseline() {
        let mut ctrl = healthy_controller();
        ctrl.transport_mut().inject_send_failure_from(Some(3));
        let err = ctrl
            .apply_effect(
                LightingEffect::Static { color: 0xFF0040 },
                &ZoneTarget::Keyboard,
                false,
            )
            .expect_err("third transfer fails");
        assert_eq!(err.reports_sent, 2);
        assert_eq!(err.reports_total, 6);
        assert!(err.is_partial());
        assert!(err.to_string().contains("partially updated"), "{err}");
        assert!(
            matches!(
                ctrl.state_source(),
                LightingStateSource::AssumedDefault | LightingStateSource::PersistedBrightness
            ),
            "a failed command must not become the remembered state"
        );

        // A later healthy command must start from the pre-failure baseline: the failed
        // keyboard effect must not leak into the composite sent for the Logo.
        ctrl.transport_mut().inject_send_failure_from(None);
        let recovered = ctrl
            .apply_effect(
                LightingEffect::Static { color: 0x00FF80 },
                &ZoneTarget::Logo,
                true,
            )
            .unwrap();
        let mut fresh = healthy_controller();
        let reference = fresh
            .apply_effect(
                LightingEffect::Static { color: 0x00FF80 },
                &ZoneTarget::Logo,
                true,
            )
            .unwrap();
        assert_eq!(recovered.reports_sent, reference.reports_sent);
    }

    #[test]
    fn failure_before_any_transfer_is_not_reported_as_partial() {
        let mut ctrl = healthy_controller();
        ctrl.transport_mut().inject_send_failure_from(Some(1));
        let err = ctrl.set_brightness(10).expect_err("first transfer fails");
        assert_eq!(err.reports_sent, 0);
        assert!(!err.is_partial());
        assert!(!err.to_string().contains("partially"));
        assert_ne!(
            ctrl.brightness(),
            10,
            "brightness must not advance on failure"
        );
    }

    #[test]
    fn brightness_persistence_failure_is_a_warning_not_a_transfer_failure() {
        let tmp_dir =
            std::env::temp_dir().join(format!("awcc_rust_bright_warn_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();
        let blocker = tmp_dir.join("file");
        fs::write(&blocker, "x").unwrap();
        // The parent of the brightness path is a regular file, so it can never be created.
        let mut ctrl = healthy_controller().with_brightness_path(blocker.join("brightness"));

        let receipt = ctrl.set_brightness(55).expect("device transfer succeeds");
        assert_eq!(ctrl.brightness(), 55);
        assert!(
            receipt.warnings.iter().any(|w| w.contains("not saved")),
            "{:?}",
            receipt.warnings
        );
        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
