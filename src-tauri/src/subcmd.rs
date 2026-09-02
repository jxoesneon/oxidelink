//! Subcommand Manager for the Nintendo Switch Pro Controller.
//!
//! This module provides:
//! - Constants for all Pro Controller subcommand IDs and output report types.
//! - [`SubcommandManager`] — an async reply-matching mechanism that pairs
//!   outbound subcommands with their 0x21 acknowledgement replies using
//!   `tokio::sync::oneshot` channels.
//! - Packet builder functions that construct raw HID output reports for every
//!   commonly used subcommand (device info, SPI flash read, player lights,
//!   home light, IMU enable, vibration enable, trigger elapsed).
//! - HD Rumble LRA encoding helpers (frequency / amplitude / motor / report).
//! - Parsers for device-info and stick-calibration replies.
//!
//! ## Wire format
//!
//! All subcommands are sent inside output report **0x01**:
//! ```text
//! [0x01, counter, rumble[0..8], subcmd_id, data...]
//! ```
//! Rumble-only reports use output report **0x10**:
//! ```text
//! [0x10, counter, left_motor[0..4], right_motor[0..4]]
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

use crate::state::{DeviceInfo, ImuCalibration, StickCalibration};

// ===========================================================================
//  Constants
// ===========================================================================

/// Subcommand 0x00 — Get Controller State. Returns battery + button info.
pub const SUBCMD_GET_STATE: u8 = 0x00;
/// Subcommand 0x02 — Request device info (firmware, MAC, controller type).
pub const SUBCMD_GET_DEVICE_INFO: u8 = 0x02;
/// Subcommand 0x03 — Set Input Report Mode (e.g. 0x30 for standard full reports).
pub const SUBCMD_SET_REPORT_MODE: u8 = 0x03;
/// Subcommand 0x04 — Trigger an elapsed-time input report (button/stick snapshot).
pub const SUBCMD_TRIGGER_ELAPSED: u8 = 0x04;
/// Subcommand 0x10 — SPI Flash Read. Reads `size` bytes at `address`.
pub const SUBCMD_SPI_FLASH_READ: u8 = 0x10;
/// Subcommand 0x30 — Set Player Lights (LED mask + flash pattern).
pub const SUBCMD_SET_PLAYER_LIGHTS: u8 = 0x30;
/// Subcommand 0x31 — Get Player Lights (current LED mask + flash pattern).
pub const SUBCMD_GET_PLAYER_LIGHTS: u8 = 0x31;
/// Subcommand 0x38 — Set Home Light (pulsing LED behind the Home button).
pub const SUBCMD_SET_HOME_LIGHT: u8 = 0x38;
/// Subcommand 0x40 — Enable / disable the IMU (accelerometer + gyroscope).
pub const SUBCMD_ENABLE_IMU: u8 = 0x40;
/// Subcommand 0x48 — Enable / disable HD vibration (LRA motors).
pub const SUBCMD_ENABLE_VIBRATION: u8 = 0x48;

/// Subcommand 0x41 — Set IMU Sensitivity.
/// 4 bytes: gyro range, accel range, gyro perf rate, accel AA filter
pub const SUBCMD_SET_IMU_SENSITIVITY: u8 = 0x41;

/// Subcommand 0x50 — Get Regulated Voltage.
/// Returns 16-bit battery voltage (1320-1680 → 3.3V-4.2V with 2.5x multiplier)
pub const SUBCMD_GET_VOLTAGE: u8 = 0x50;

/// Subcommand 0x11 — SPI Flash Write.
pub const SUBCMD_SPI_FLASH_WRITE: u8 = 0x11;

/// Subcommand 0x12 — SPI Flash Sector Erase.
pub const SUBCMD_SPI_SECTOR_ERASE: u8 = 0x12;

/// Output report ID 0x10 — rumble data only (no subcommand).
pub const OUTPUT_RUMBLE_ONLY: u8 = 0x10;
/// Output report ID 0x01 — subcommand (with optional rumble prefix).
pub const OUTPUT_SUBCOMMAND: u8 = 0x01;

/// Subcommand 0x21 — Set NFC mode (alias for set MCU config in NFC context).
pub const SUBCMD_SET_NFC_MODE: u8 = 0x21;
/// Subcommand 0x22 — Set NFC configuration (alias for set MCU state).
pub const SUBCMD_SET_NFC_CONFIG: u8 = 0x22;
/// Subcommand 0x23 — Get NFC data (read tag data from MCU).
pub const SUBCMD_GET_NFC_DATA: u8 = 0x23;
/// Subcommand 0x21 — Set MCU config (NFC/IR mode selection).
pub const SUBCMD_SET_MCU_CONFIG: u8 = 0x21;
/// Subcommand 0x22 — Set MCU state (suspend/resume).
pub const SUBCMD_SET_MCU_STATE: u8 = 0x22;
/// Output report ID 0x11 — NFC/IR MCU data report.
pub const OUTPUT_NFC_IR_MCU: u8 = 0x11;

// ===========================================================================
//  USB-specific commands (0x80 series)
// ===========================================================================
//
// When connected via USB, the Pro Controller uses an STM32 MCU that bridges
// UART to the Broadcom Bluetooth MCU. USB connections require a handshake
// sequence before the controller will accept standard subcommands.
//
// Reference: Linux kernel driver `drivers/hid/hid-nintendo.c`

/// USB output report ID 0x80 — USB command (handshake, baudrate, timeout).
pub const OUTPUT_USB_CMD: u8 = 0x80;

/// USB command sub-IDs (sent as byte 1 of the 0x80 report).
/// Handshake — required to establish USB communication.
pub const USB_CMD_HANDSHAKE: u8 = 0x02;
/// Baudrate 3Mbit — switch the UART to 3 Mbit/s for higher throughput.
pub const USB_CMD_BAUDRATE_3M: u8 = 0x03;
/// No timeout — prevent the USB MCU from timing out and reverting to
/// Bluetooth mode. Without this, the controller drops off USB after ~5s.
pub const USB_CMD_NO_TIMEOUT: u8 = 0x04;
/// Enable timeout — re-enable the USB timeout (used on disconnect).
pub const USB_CMD_EN_TIMEOUT: u8 = 0x05;
/// Reset — sent after enable-timeout on disconnect to fully reset the
/// USB connection. BetterJoy sends this after 0x05 to ensure the STM32
/// cleanly reverts to Bluetooth mode.
pub const USB_CMD_RESET: u8 = 0x06;
/// USB handshake response magic — the controller returns this to ack.
pub const USB_HANDSHAKE_ACK: u8 = 0x81;

// ===========================================================================
//  SPI Flash calibration addresses
// ===========================================================================

/// SPI flash address for left stick FACTORY calibration (9 bytes).
pub const SPI_ADDR_LEFT_STICK_FACTORY: u32 = 0x603D;
/// SPI flash address for right stick FACTORY calibration (9 bytes).
pub const SPI_ADDR_RIGHT_STICK_FACTORY: u32 = 0x6046;
/// SPI flash address for IMU factory calibration (24 bytes: accel + gyro
/// origin/sensitivity).
pub const SPI_ADDR_IMU_FACTORY: u32 = 0x6020;
/// SPI flash address for user left stick calibration (11 bytes: 2 magic + 9
/// data).
pub const SPI_ADDR_LEFT_STICK_USER: u32 = 0x8010;
/// SPI flash address for user right stick calibration (11 bytes: 2 magic + 9
/// data).
pub const SPI_ADDR_RIGHT_STICK_USER: u32 = 0x801B;
/// SPI flash address for user IMU calibration (26 bytes: 2 magic + 24 data).
pub const SPI_ADDR_IMU_USER: u32 = 0x8026;

/// SPI flash address for the controller serial number (16 bytes, ASCII).
pub const SPI_ADDR_SERIAL: u32 = 0x6000;
/// SPI flash address for the body color (3 bytes: RGB).
pub const SPI_ADDR_BODY_COLOR: u32 = 0x6050;
/// SPI flash address for the button color (3 bytes: RGB).
pub const SPI_ADDR_BUTTON_COLOR: u32 = 0x6053;
/// SPI flash address for the left grip color (3 bytes: RGB).
pub const SPI_ADDR_LEFT_GRIP_COLOR: u32 = 0x6056;
/// SPI flash address for the right grip color (3 bytes: RGB).
pub const SPI_ADDR_RIGHT_GRIP_COLOR: u32 = 0x6059;

/// SPI flash: Color info exists flag (0x601B, 1 byte).
/// 0x01 = use SPI-stored colors, 0x00 = use default colors.
pub const SPI_ADDR_COLOR_FLAG: u32 = 0x601B;

/// SPI flash: 6-axis horizontal offsets (0x6080, 6 bytes).
/// 3× int16LE — accelerometer offsets when controller is on a flat surface.
pub const SPI_ADDR_HORIZONTAL_OFFSETS: u32 = 0x6080;

/// SPI flash: Stick device parameters (0x6086, 18 bytes).
/// Contains factory deadzone and range parameters for sticks.
pub const SPI_ADDR_STICK_PARAMS: u32 = 0x6086;

/// User calibration magic bytes (0xB2 0xA1) that indicate valid user
/// calibration data is present.
pub const USER_CAL_MAGIC: [u8; 2] = [0xB2, 0xA1];

// ===========================================================================
//  Response / Error types
// ===========================================================================

/// A matched subcommand reply received from the controller.
///
/// `ack` has bit 7 set (0x80) for a successful ACK; otherwise the controller
/// is NACK-ing the subcommand.
#[derive(Debug, Clone)]
pub struct SubcommandResponse {
    /// The subcommand ID this reply corresponds to.
    pub subcmd_id: u8,
    /// Raw ACK byte from the 0x21 reply (bit 7 = success).
    pub ack: u8,
    /// Payload bytes following the subcommand ID in the reply.
    pub data: Vec<u8>,
    /// Millisecond timestamp at which the reply was handled.
    pub timestamp: u64,
}

/// Errors that can occur while waiting for a subcommand reply.
#[derive(Debug, Clone)]
pub enum SubcommandError {
    /// No reply was received within the configured timeout.
    Timeout,
    /// The controller explicitly NACK-ed the subcommand. Contains the raw ACK byte.
    Nack(u8),
    /// The outbound report could not be sent to the device.
    SendFailed,
}

// ===========================================================================
//  SubcommandManager
// ===========================================================================

/// Async reply-matching manager for Pro Controller subcommands.
///
/// The manager maintains a map of pending waiters keyed by subcommand ID.
/// When a 0x21 reply arrives, [`handle_reply`](Self::handle_reply) looks up
/// the waiter for that subcommand ID and delivers the result via a
/// `oneshot` channel. The caller that registered the pending request
/// awaits the `oneshot::Receiver` (typically with a `tokio::time::timeout`
/// wrapper using [`timeout_duration`](Self::timeout_duration)).
///
/// Only one waiter per subcommand ID may be active at a time. If a second
/// registration replaces the first, the previous waiter's sender is dropped
/// and its receiver will return a `RecvError`.
type PendingMap =
    Arc<Mutex<HashMap<u8, oneshot::Sender<Result<SubcommandResponse, SubcommandError>>>>>;

pub struct SubcommandManager {
    pending: PendingMap,
    timeout: Duration,
}

impl SubcommandManager {
    /// Create a new manager with the given reply timeout.
    pub fn new(timeout: Duration) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout,
        }
    }

    /// Register a pending waiter for `subcmd_id` and return the receiver
    /// that will be fulfilled when the reply arrives.
    ///
    /// If a waiter for the same subcommand ID already exists it is replaced
    /// (the old sender is dropped, causing the old receiver to error).
    pub async fn register_pending(
        &self,
        subcmd_id: u8,
    ) -> oneshot::Receiver<Result<SubcommandResponse, SubcommandError>> {
        let (tx, rx) = oneshot::channel();
        let mut map = self.pending.lock().await;
        if map.insert(subcmd_id, tx).is_some() {
            warn!(
                "Replaced existing pending waiter for subcmd 0x{:02X}",
                subcmd_id
            );
        }
        debug!("Registered pending waiter for subcmd 0x{:02X}", subcmd_id);
        rx
    }

    /// Deliver a reply to the pending waiter for `subcmd_id`, if any.
    ///
    /// If `ack & 0x80 != 0` the result is `Ok(SubcommandResponse)`;
    /// otherwise it is `Err(SubcommandError::Nack(ack))`.
    ///
    /// If no waiter is registered for this subcommand ID the reply is
    /// silently discarded (logged at debug level).
    pub async fn handle_reply(&self, subcmd_id: u8, ack: u8, data: Vec<u8>) {
        let mut map = self.pending.lock().await;
        match map.remove(&subcmd_id) {
            Some(sender) => {
                let timestamp = crate::state::timestamp_now();
                if ack & 0x80 != 0 {
                    debug!(
                        "Subcmd 0x{:02X} ACK (0x{:02X}), {} data bytes",
                        subcmd_id,
                        ack,
                        data.len()
                    );
                    let _ = sender.send(Ok(SubcommandResponse {
                        subcmd_id,
                        ack,
                        data,
                        timestamp,
                    }));
                } else {
                    warn!("Subcmd 0x{:02X} NACK (ack=0x{:02X})", subcmd_id, ack);
                    let _ = sender.send(Err(SubcommandError::Nack(ack)));
                }
            }
            None => {
                debug!(
                    "Reply for subcmd 0x{:02X} with no pending waiter — discarding",
                    subcmd_id
                );
            }
        }
    }

    /// Returns the configured timeout duration for awaiting replies.
    pub fn timeout_duration(&self) -> Duration {
        self.timeout
    }
}

// ===========================================================================
//  Packet builders
// ===========================================================================

/// Build a raw subcommand output report (report ID 0x01).
///
/// Layout: `[0x01, counter, rumble[0..8], subcmd_id, data...]`
///
/// The 8-byte rumble prefix is sent with every subcommand; pass all zeros
/// to disable rumble while sending the subcommand.
pub fn build_subcommand_packet(
    counter: u8,
    rumble: [u8; 8],
    subcmd_id: u8,
    data: &[u8],
) -> Vec<u8> {
    let mut report = Vec::with_capacity(11 + data.len());
    report.push(OUTPUT_SUBCOMMAND); // 0x01
    report.push(counter);
    report.extend_from_slice(&rumble); // 8 bytes
    report.push(subcmd_id);
    report.extend_from_slice(data);
    report
}

/// Build subcommand 0x02 — Request Device Info.
///
/// No additional data is required; the controller replies with a 12-byte
/// payload containing firmware version, controller type, MAC address, and
/// a flag indicating whether SPI-stored colors are used.
pub fn build_get_device_info_subcmd(counter: u8) -> Vec<u8> {
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_GET_DEVICE_INFO, &[])
}

/// Build subcommand 0x10 — SPI Flash Read.
///
/// `address` is encoded little-endian across the first three data bytes,
/// followed by `size` (number of bytes to read, max 0x1D per request).
pub fn build_spi_flash_read_subcmd(counter: u8, address: u32, size: u8) -> Vec<u8> {
    let data = [
        (address & 0xFF) as u8,
        ((address >> 8) & 0xFF) as u8,
        ((address >> 16) & 0xFF) as u8,
        size,
    ];
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SPI_FLASH_READ, &data)
}

/// Build subcommand 0x30 — Set Player Lights.
///
/// Build subcommand 0x30 — Set Player Lights.
///
/// The Pro Controller has 4 player LEDs. Subcommand 0x30 takes a **single
/// byte** where:
///
/// - Low nibble (bits 0–3): which LEDs to keep ON steadily
/// - High nibble (bits 4–7): which LEDs to FLASH
/// - If an LED is set in both nibbles, ON overrides flashing
///
/// `led_mask` is a bitfield (bit 0 = LED 1, bit 3 = LED 4) for steady-on.
/// `flash_mask` is the same bitfield format for which LEDs should flash.
pub fn build_set_player_lights_subcmd(counter: u8, led_mask: u8, flash_mask: u8) -> Vec<u8> {
    let combined = ((flash_mask & 0x0F) << 4) | (led_mask & 0x0F);
    let data = [combined];
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SET_PLAYER_LIGHTS, &data)
}

/// Build subcommand 0x38 — Set Home Light.
///
/// The Home button LED ring supports complex pulsing patterns via a
/// 25-byte mini-cycle configuration. This builder creates the full 25-byte
/// payload per the dekuNukem spec:
///
/// - Byte 0 high nibble: number of mini cycles (1–15)
/// - Byte 0 low nibble: global mini cycle duration (0=OFF, 1=8ms … F=175ms)
/// - Byte 1 high nibble: LED start intensity (0–F = 0–100%)
/// - Byte 1 low nibble: number of full cycles (0=repeat forever, 1–15)
/// - Bytes 2–24: mini cycle configurations (intensity, fade, duration)
///
/// `brightness` is 0–100 (percent). `pattern` selects a preset:
/// "solid", "breathing", "blink", "fade", "rainbow", "chase", "wave".
pub fn build_set_home_light_subcmd(
    counter: u8,
    enabled: bool,
    brightness: u8,
    pattern: &str,
) -> Vec<u8> {
    let intensity = ((brightness as u16 * 15 + 50) / 100).min(15) as u8; // 0-15
    let mut data = vec![0xFFu8; 25]; // Fill with 0xFF (unused mini cycles)

    if !enabled || intensity == 0 {
        // OFF: 1 mini cycle, 0ms duration, 0% start, 1 full cycle
        data[0] = 0x10; // 1 cycle, 0ms (OFF)
        data[1] = 0x01; // 0% start, 1 full cycle
        data[2] = 0xFF;
        data[3] = 0xFF;
        data[4] = 0xFF;
    } else {
        match pattern {
            "solid" => {
                // Solid on at given brightness, forever
                data[0] = 0x1F; // 1 mini cycle, 175ms global duration
                data[1] = intensity << 4; // start intensity, repeat forever
                data[2] = (intensity << 4) | intensity; // MC1 + MC2 at same intensity
                data[3] = 0xFF; // no fade, long duration
                data[4] = 0xFF;
            }
            "breathing" => {
                // Slow breathe: fade up and down over ~1.5s cycles
                data[0] = 0x2F; // 2 mini cycles, 175ms global
                data[1] = intensity << 4; // start at intensity, repeat forever
                data[2] = intensity; // MC1=off, MC2=full intensity
                data[3] = (0x8 << 4) | 0x8; // slow fade to MC1, long hold
                data[4] = (0x8 << 4) | 0x8; // slow fade to MC2, long hold
            }
            "blink" => {
                // Fast blink: on/off rapidly
                data[0] = 0x24; // 2 mini cycles, ~40ms global
                data[1] = intensity << 4; // start at intensity, forever
                data[2] = intensity; // MC1=off, MC2=on
                data[3] = 0x11; // fast transition
                data[4] = 0x11;
            }
            "fade" => {
                // Single slow fade in, then hold
                data[0] = 0x1F; // 1 mini cycle, 175ms
                data[1] = 0x0; // start at 0%, forever
                data[2] = (intensity << 4) | intensity;
                data[3] = (0xF << 4) | 0xF; // very slow fade
                data[4] = 0xFF;
            }
            "wave" => {
                // Multi-step intensity wave: dim → bright → dim → off
                data[0] = 0x48; // 4 mini cycles, ~80ms global
                data[1] = intensity << 4; // start at intensity, forever
                data[2] = intensity << 4; // MC1=on, MC2=off
                data[3] = (0x4 << 4) | 0x4; // medium fade
                data[4] = (0x4 << 4) | 0x4;
                data[5] = intensity; // MC3=off, MC4=on
                data[6] = (0x4 << 4) | 0x4;
                data[7] = (0x4 << 4) | 0x4;
            }
            _ => {
                // Default: solid on
                data[0] = 0x1F;
                data[1] = intensity << 4;
                data[2] = (intensity << 4) | intensity;
                data[3] = 0xFF;
                data[4] = 0xFF;
            }
        }
    }

    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SET_HOME_LIGHT, &data)
}

/// Build subcommand 0x40 — Enable / Disable IMU.
///
/// `enabled = true` sends `0x01`; `false` sends `0x00`.
pub fn build_enable_imu_subcmd(counter: u8, enabled: bool) -> Vec<u8> {
    let data = [if enabled { 0x01 } else { 0x00 }];
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_ENABLE_IMU, &data)
}

/// Build subcommand 0x48 — Enable / Disable Vibration.
///
/// `enabled = true` sends `0x01`; `false` sends `0x00`.
pub fn build_enable_vibration_subcmd(counter: u8, enabled: bool) -> Vec<u8> {
    let data = [if enabled { 0x01 } else { 0x00 }];
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_ENABLE_VIBRATION, &data)
}

/// Build subcommand 0x04 — Trigger Elapsed Time Report.
///
/// Requests an immediate input report containing the current button / stick
/// state. No additional data is required.
pub fn build_trigger_elapsed_subcmd(counter: u8) -> Vec<u8> {
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_TRIGGER_ELAPSED, &[])
}

/// Build subcommand 0x03 — Set Input Report Mode.
/// mode: 0x30=standard, 0x31=NFC/IR, 0x3F=simple HID
pub fn build_set_report_mode_subcmd(counter: u8, mode: u8) -> Vec<u8> {
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SET_REPORT_MODE, &[mode])
}

/// Build subcommand 0x31 — Get Player Lights.
pub fn build_get_player_lights_subcmd(counter: u8) -> Vec<u8> {
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_GET_PLAYER_LIGHTS, &[])
}

/// Build subcommand 0x41 — Set IMU Sensitivity.
/// gyro_range: 0=±250dps, 1=±500dps, 2=±1000dps, 3=±2000dps (default)
/// accel_range: 0=±8G (default), 1=±4G, 2=±2G, 3=±16G
/// gyro_rate: 0=833Hz, 1=208Hz (default)
/// accel_filter: 0=200Hz, 1=100Hz (default)
pub fn build_set_imu_sensitivity_subcmd(
    counter: u8,
    gyro_range: u8,
    accel_range: u8,
    gyro_rate: u8,
    accel_filter: u8,
) -> Vec<u8> {
    let data = [gyro_range, accel_range, gyro_rate, accel_filter];
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SET_IMU_SENSITIVITY, &data)
}

/// Build subcommand 0x50 — Get Regulated Voltage.
pub fn build_get_voltage_subcmd(counter: u8) -> Vec<u8> {
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_GET_VOLTAGE, &[])
}

/// Build subcommand 0x11 — SPI Flash Write.
/// address: SPI flash address (little-endian 3 bytes)
/// data: data to write (max 0x1D = 29 bytes)
pub fn build_spi_flash_write_subcmd(counter: u8, address: u32, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + data.len());
    payload.push((address & 0xFF) as u8);
    payload.push(((address >> 8) & 0xFF) as u8);
    payload.push(((address >> 16) & 0xFF) as u8);
    payload.push(data.len() as u8);
    payload.extend_from_slice(data);
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SPI_FLASH_WRITE, &payload)
}

// ===========================================================================
//  HD Rumble encoding
// ===========================================================================

/// Encode a vibration frequency (Hz) into the Pro Controller's LRA format.
///
/// The Pro Controller uses a logarithmic encoding: `log2(hz / 10) * 32`.
/// The input is clamped to the valid LRA frequency range of 41.0–1253.0 Hz
/// before encoding.
pub fn encode_rumble_frequency(hz: f32) -> u8 {
    let clamped = hz.clamp(41.0, 1253.0);
    let encoded = (clamped / 10.0).log2() * 32.0;
    encoded.round() as u8
}

/// Encode a vibration amplitude (0.0–1.0) into the Pro Controller's LRA format.
///
/// Uses a simplified linear mapping clamped to 0.0–0.9 to stay within safe
/// LRA drive limits: `amplitude * 255 / 0.9`.
pub fn encode_rumble_amplitude(amp: f32) -> u8 {
    let clamped = amp.clamp(0.0, 0.9);
    let encoded = clamped * 255.0 / 0.9;
    encoded.round() as u8
}

/// Encode a rumble motor command using dekuNukem's HD rumble protocol.
///
/// Each motor uses 4 bytes with complex bit-packing:
/// - Byte 0: HF frequency low byte
/// - Byte 1: HF amplitude + HF frequency high bit
/// - Byte 2: LF frequency + LF amplitude high bit
/// - Byte 3: LF amplitude low byte
///
/// Frequency encoding: encoded_hex_freq = round(log2(freq/10) * 32)
/// HF frequency: (encoded_hex_freq - 0x60) * 4, range 0x0004-0x01FC
/// LF frequency: encoded_hex_freq - 0x40, range 0x01-0x7F
///
/// Amplitude encoding (piecewise logarithmic):
/// - amp > 0.23: encoded = round(log2(amp * 8.7) * 32)
/// - 0.12 < amp <= 0.23: encoded = round(log2(amp * 17.0) * 16.0)
/// - amp <= 0.12: encoded = 0 (off)
/// - HF amplitude: encoded * 2
/// - LF amplitude: encoded / 2 + 64
pub fn encode_rumble_motor(freq_hz: f32, amplitude: f32) -> [u8; 4] {
    // Clamp inputs to safe ranges
    let freq_hz = freq_hz.clamp(41.0, 1253.0);
    let amplitude = amplitude.clamp(0.0, 0.9);

    // 1. Encode frequency
    let encoded_freq = (freq_hz / 10.0).log2() * 32.0;
    let encoded_freq = encoded_freq.round() as i16;

    // 2. Convert to HF/LF ranges
    let hf = (encoded_freq - 0x60) * 4; // HF: 0x0004-0x01FC
    let lf = (encoded_freq - 0x40) as u8; // LF: 0x01-0x7F

    // 3. Encode amplitude (piecewise logarithmic)
    let encoded_amp: f32 = if amplitude > 0.23 {
        (amplitude * 8.7).log2() * 32.0
    } else if amplitude > 0.12 {
        (amplitude * 17.0).log2() * 16.0
    } else {
        0.0
    };
    let encoded_amp = encoded_amp.round() as u8;

    // 4. Convert to HF/LF amplitudes
    let hf_amp = encoded_amp.saturating_mul(2);
    let lf_amp = (encoded_amp / 2).saturating_add(64);

    // 5. Byte-packing with bit-swapping
    let byte0 = (hf & 0xFF) as u8;
    let byte1 = hf_amp.wrapping_add(((hf >> 8) & 0xFF) as u8);
    let byte2 = lf.wrapping_add(((lf_amp as u16 >> 8) & 0xFF) as u8);
    let byte3 = lf_amp;

    [byte0, byte1, byte2, byte3]
}

/// Build a rumble-only output report (report ID 0x10).
///
/// Layout: `[0x10, counter, left_motor[0..4], right_motor[0..4]]` (10 bytes).
///
/// Pass `0.0` for both amplitudes to stop vibration (equivalent to a
/// zero-rumble keepalive packet).
pub fn build_rumble_report(
    counter: u8,
    left_freq: f32,
    left_amp: f32,
    right_freq: f32,
    right_amp: f32,
) -> Vec<u8> {
    let left = encode_rumble_motor(left_freq, left_amp);
    let right = encode_rumble_motor(right_freq, right_amp);
    let mut report = vec![0u8; 10];
    report[0] = OUTPUT_RUMBLE_ONLY; // 0x10
    report[1] = counter;
    report[2..6].copy_from_slice(&left);
    report[6..10].copy_from_slice(&right);
    report
}

// ===========================================================================
//  CRC-8-CCITT (polynomial 0x07)
// ===========================================================================

/// CRC-8-CCITT lookup table for polynomial 0x07.
/// Used for NFC/IR MCU configuration subcommand (0x21) checksums.
const CRC8_TABLE: [u8; 256] = [
    0x00, 0x07, 0x0E, 0x09, 0x1C, 0x1B, 0x12, 0x15, 0x38, 0x3F, 0x36, 0x31, 0x24, 0x23, 0x2A, 0x2D,
    0x70, 0x77, 0x7E, 0x79, 0x6C, 0x6B, 0x62, 0x65, 0x48, 0x4F, 0x46, 0x41, 0x54, 0x53, 0x5A, 0x5D,
    0xE0, 0xE7, 0xEE, 0xE9, 0xFC, 0xFB, 0xF2, 0xF5, 0xD8, 0xDF, 0xD6, 0xD1, 0xC4, 0xC3, 0xCA, 0xCD,
    0x90, 0x97, 0x9E, 0x99, 0x8C, 0x8B, 0x82, 0x85, 0xA8, 0xAF, 0xA6, 0xA1, 0xB4, 0xB3, 0xBA, 0xBD,
    0xC7, 0xC0, 0xC9, 0xCE, 0xDB, 0xDC, 0xD5, 0xD2, 0xFF, 0xF8, 0xF1, 0xF6, 0xE3, 0xE4, 0xED, 0xEA,
    0xB7, 0xB0, 0xB9, 0xBE, 0xAB, 0xAC, 0xA5, 0xA2, 0x8F, 0x88, 0x81, 0x86, 0x93, 0x94, 0x9D, 0x9A,
    0x27, 0x20, 0x29, 0x2E, 0x3B, 0x3C, 0x35, 0x32, 0x1F, 0x18, 0x11, 0x16, 0x03, 0x04, 0x0D, 0x0A,
    0x57, 0x50, 0x59, 0x5E, 0x4B, 0x4C, 0x45, 0x42, 0x6F, 0x68, 0x61, 0x66, 0x73, 0x74, 0x7D, 0x7A,
    0x89, 0x8E, 0x87, 0x80, 0x95, 0x92, 0x9B, 0x9C, 0xB1, 0xB6, 0xBF, 0xB8, 0xAD, 0xAA, 0xA3, 0xA4,
    0xF9, 0xFE, 0xF7, 0xF0, 0xE5, 0xE2, 0xEB, 0xEC, 0xC1, 0xC6, 0xCF, 0xC8, 0xDD, 0xDA, 0xD3, 0xD4,
    0x69, 0x6E, 0x67, 0x60, 0x75, 0x72, 0x7B, 0x7C, 0x51, 0x56, 0x5F, 0x58, 0x4D, 0x4A, 0x43, 0x44,
    0x19, 0x1E, 0x17, 0x10, 0x05, 0x02, 0x0B, 0x0C, 0x21, 0x26, 0x2F, 0x28, 0x3D, 0x3A, 0x33, 0x34,
    0x4E, 0x49, 0x40, 0x47, 0x52, 0x55, 0x5C, 0x5B, 0x76, 0x71, 0x78, 0x7F, 0x6A, 0x6D, 0x64, 0x63,
    0x3E, 0x39, 0x30, 0x37, 0x22, 0x25, 0x2C, 0x2B, 0x06, 0x01, 0x08, 0x0F, 0x1A, 0x1D, 0x14, 0x13,
    0xAE, 0xA9, 0xA0, 0xA7, 0xB2, 0xB5, 0xBC, 0xBB, 0x96, 0x91, 0x98, 0x9F, 0x8A, 0x8D, 0x84, 0x83,
    0xDE, 0xD9, 0xD0, 0xD7, 0xC2, 0xC5, 0xCC, 0xCB, 0xE6, 0xE1, 0xE8, 0xEF, 0xFA, 0xFD, 0xF4, 0xF3,
];

/// Calculate CRC-8-CCITT (polynomial 0x07) for NFC/IR MCU config.
/// Covers all bytes of the input data.
pub fn crc8_ccitt(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        crc = CRC8_TABLE[(crc ^ byte) as usize];
    }
    crc
}

// ===========================================================================
//  Reply parsers
// ===========================================================================

/// Parse a 12-byte device-info reply payload (subcommand 0x02).
///
/// Reply layout (bytes after the subcmd ID in the 0x21 reply):
/// - byte 0: firmware major
/// - byte 1: firmware minor
/// - byte 2: controller type (0x03 = Pro Controller)
/// - byte 3: (reserved / zero)
/// - bytes 4–9: MAC address (6 bytes)
/// - bytes 10: (reserved / zero)
/// - byte 11: `colors_from_spi` flag (0x01 = use SPI-stored colors)
///
/// Returns `None` if the payload is too short.
pub fn parse_device_info_reply(data: &[u8]) -> Option<DeviceInfo> {
    if data.len() < 12 {
        warn!(
            "Device info reply too short: {} bytes (expected 12)",
            data.len()
        );
        return None;
    }

    let fw_major = data[0];
    let fw_minor = data[1];
    let firmware_version = format!("{}.{}", fw_major, fw_minor);

    let controller_type = data[2];

    let mac = &data[4..10];
    let mac_address = format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let colors_from_spi = data[11] == 0x01;

    info!(
        "Device info: fw={}, type=0x{:02X}, mac={}, colors_from_spi={}",
        firmware_version, controller_type, mac_address, colors_from_spi
    );

    Some(DeviceInfo {
        firmware_version,
        controller_type,
        mac_address,
        colors_from_spi,
        connection: String::new(),
        spi: None,
    })
}

/// Decode two 12-bit values from a 3-byte group.
///
/// Packing (same as stick input reports):
/// - `val1 = byte[0] | ((byte[1] & 0x0F) << 8)`
/// - `val2 = ((byte[1] >> 4) & 0x0F) | (byte[2] << 4)`
fn decode_12bit_pair(group: &[u8]) -> (u16, u16) {
    let v1 = (group[0] as u16) | ((group[1] as u16 & 0x0F) << 8);
    let v2 = ((group[1] as u16 >> 4) & 0x0F) | ((group[2] as u16) << 4);
    (v1, v2)
}

/// Encode two 12-bit values into a 3-byte group (inverse of [`decode_12bit_pair`]).
#[cfg(test)]
fn encode_12bit_pair(v1: u16, v2: u16) -> [u8; 3] {
    let b0 = (v1 & 0xFF) as u8;
    let b1 = ((v1 >> 8) & 0x0F) as u8 | (((v2 & 0x0F) << 4) as u8);
    let b2 = ((v2 >> 4) & 0xFF) as u8;
    [b0, b1, b2]
}

/// Check if a data block is all zeros (empty/uninitialized SPI flash).
fn is_all_zeros(data: &[u8]) -> bool {
    data.iter().all(|&b| b == 0)
}

/// Check if a data block contains only 0xFFF bogus values (uninitialized).
fn is_all_fff(data: &[u8]) -> bool {
    // Decode all 3-byte groups and check if all values are 0xFFF
    if data.len() < 9 {
        return false;
    }
    for chunk in data.chunks_exact(3) {
        let (v1, v2) = decode_12bit_pair(chunk);
        if v1 != 0xFFF || v2 != 0xFFF {
            return false;
        }
    }
    true
}

/// Parse a 9-byte stick calibration block with the specified byte order and
/// convert relative offsets to absolute values.
///
/// Returns `(center_x, center_y, min_x, min_y, max_x, max_y)` as absolute
/// values, or `None` if the data is too short or appears to be uninitialized
/// (all zeros or all 0xFFF).
///
/// # Byte order
///
/// - **Left stick** (`is_left = true`): `[max_above, center, min_below]`
///   - Bytes 0–2: max_above_center (X, Y) — relative offset ABOVE center
///   - Bytes 3–5: center (X, Y)
///   - Bytes 6–8: min_below_center (X, Y) — relative offset BELOW center
///
/// - **Right stick** (`is_left = false`): `[center, min_below, max_above]`
///   - Bytes 0–2: center (X, Y)
///   - Bytes 3–5: min_below_center (X, Y) — relative offset BELOW center
///   - Bytes 6–8: max_above_center (X, Y) — relative offset ABOVE center
///
/// # Relative → absolute conversion
///
/// `abs_max = center + max_above`, `abs_min = center - min_below`.
fn parse_stick_cal_block(data: &[u8], is_left: bool) -> Option<(u16, u16, u16, u16, u16, u16)> {
    if data.len() < 9 {
        warn!(
            "Stick calibration block too short: {} bytes (expected 9)",
            data.len()
        );
        return None;
    }

    // Check for uninitialized SPI flash
    if is_all_zeros(&data[..9]) {
        warn!("Stick calibration block is all zeros — SPI flash uninitialized");
        return None;
    }
    if is_all_fff(&data[..9]) {
        warn!("Stick calibration block is all 0xFFF — SPI flash uninitialized");
        return None;
    }

    let (center_x, center_y, min_below_x, min_below_y, max_above_x, max_above_y) = if is_left {
        // Left: [max_above, center, min_below]
        let (max_above_x, max_above_y) = decode_12bit_pair(&data[0..3]);
        let (center_x, center_y) = decode_12bit_pair(&data[3..6]);
        let (min_below_x, min_below_y) = decode_12bit_pair(&data[6..9]);
        (
            center_x,
            center_y,
            min_below_x,
            min_below_y,
            max_above_x,
            max_above_y,
        )
    } else {
        // Right: [center, min_below, max_above]
        let (center_x, center_y) = decode_12bit_pair(&data[0..3]);
        let (min_below_x, min_below_y) = decode_12bit_pair(&data[3..6]);
        let (max_above_x, max_above_y) = decode_12bit_pair(&data[6..9]);
        (
            center_x,
            center_y,
            min_below_x,
            min_below_y,
            max_above_x,
            max_above_y,
        )
    };

    // Convert relative offsets to absolute values.
    // abs_max = center + max_above, abs_min = center - min_below
    let abs_max_x = center_x.saturating_add(max_above_x);
    let abs_max_y = center_y.saturating_add(max_above_y);
    let abs_min_x = center_x.saturating_sub(min_below_x);
    let abs_min_y = center_y.saturating_sub(min_below_y);

    debug!(
        "Stick cal block (is_left={}): center=({},{}), min_below=({},{}, abs_min=({},{})), max_above=({},{}, abs_max=({},{})",
        is_left,
        center_x, center_y,
        min_below_x, min_below_y, abs_min_x, abs_min_y,
        max_above_x, max_above_y, abs_max_x, abs_max_y,
    );

    Some((
        center_x, center_y, abs_min_x, abs_min_y, abs_max_x, abs_max_y,
    ))
}

/// Parse a 9-byte left stick calibration block (factory format: max, center,
/// min).
///
/// Returns a partial [`StickCalibration`] with only the left stick fields
/// populated (right stick fields are zero). The `source` field is set to
/// `"factory"` and `valid` is `false` until full validation is performed.
pub fn parse_left_stick_calibration(data: &[u8]) -> Option<StickCalibration> {
    let (cx, cy, min_x, min_y, max_x, max_y) = parse_stick_cal_block(data, true)?;
    Some(StickCalibration {
        left_center_x: cx,
        left_center_y: cy,
        left_min_x: min_x,
        left_min_y: min_y,
        left_max_x: max_x,
        left_max_y: max_y,
        source: "factory".into(),
        valid: false,
        ..Default::default()
    })
}

/// Parse a 9-byte right stick calibration block (factory format: center, min,
/// max).
///
/// Returns a partial [`StickCalibration`] with only the right stick fields
/// populated (left stick fields are zero). The `source` field is set to
/// `"factory"` and `valid` is `false` until full validation is performed.
pub fn parse_right_stick_calibration(data: &[u8]) -> Option<StickCalibration> {
    let (cx, cy, min_x, min_y, max_x, max_y) = parse_stick_cal_block(data, false)?;
    Some(StickCalibration {
        right_center_x: cx,
        right_center_y: cy,
        right_min_x: min_x,
        right_min_y: min_y,
        right_max_x: max_x,
        right_max_y: max_y,
        source: "factory".into(),
        valid: false,
        ..Default::default()
    })
}

/// Merge left and right stick calibration blocks into a single
/// [`StickCalibration`].
///
/// Both partial calibrations must have been parsed from their respective SPI
/// flash blocks. The `source` is taken from the left calibration (both should
/// have the same source). The `valid` flag is set based on
/// [`validate_stick_calibration`].
pub fn merge_stick_calibration(
    left: &StickCalibration,
    right: &StickCalibration,
) -> StickCalibration {
    let mut merged = StickCalibration {
        left_center_x: left.left_center_x,
        left_center_y: left.left_center_y,
        left_min_x: left.left_min_x,
        left_min_y: left.left_min_y,
        left_max_x: left.left_max_x,
        left_max_y: left.left_max_y,
        right_center_x: right.right_center_x,
        right_center_y: right.right_center_y,
        right_min_x: right.right_min_x,
        right_min_y: right.right_min_y,
        right_max_x: right.right_max_x,
        right_max_y: right.right_max_y,
        source: left.source.clone(),
        valid: false,
    };
    merged.valid = validate_stick_calibration(&merged);
    if !merged.valid {
        warn!("Merged stick calibration failed validation — using default values");
    }
    merged
}

/// Parse a 24-byte IMU calibration data block (factory format from SPI flash
/// address 0x6020).
///
/// Layout (4 groups of 3× int16LE):
/// - Bytes 0–5: Accel origin XYZ (3× int16LE)
/// - Bytes 6–11: Accel sensitivity XYZ (3× int16LE)
/// - Bytes 12–17: Gyro origin XYZ (3× int16LE)
/// - Bytes 18–23: Gyro sensitivity XYZ (3× int16LE)
///
/// Returns `None` if the data is too short or appears to be uninitialized
/// (all zeros).
pub fn parse_imu_calibration(data: &[u8]) -> Option<ImuCalibration> {
    if data.len() < 24 {
        warn!(
            "IMU calibration data too short: {} bytes (expected 24)",
            data.len()
        );
        return None;
    }

    // Check for uninitialized SPI flash (all zeros)
    if is_all_zeros(&data[..24]) {
        warn!("IMU calibration data is all zeros — SPI flash uninitialized");
        return None;
    }

    let read_i16 = |offset: usize| -> i16 { i16::from_le_bytes([data[offset], data[offset + 1]]) };

    let cal = ImuCalibration {
        accel_origin: [read_i16(0), read_i16(2), read_i16(4)],
        accel_sensitivity: [read_i16(6), read_i16(8), read_i16(10)],
        gyro_origin: [read_i16(12), read_i16(14), read_i16(16)],
        gyro_sensitivity: [read_i16(18), read_i16(20), read_i16(22)],
        source: "factory".into(),
        horizontal_offsets: [0, 0, 0],
    };

    debug!(
        "IMU calibration: accel_origin=[{},{},{}], accel_sens=[{},{},{}], gyro_origin=[{},{},{}], gyro_sens=[{},{},{}]",
        cal.accel_origin[0], cal.accel_origin[1], cal.accel_origin[2],
        cal.accel_sensitivity[0], cal.accel_sensitivity[1], cal.accel_sensitivity[2],
        cal.gyro_origin[0], cal.gyro_origin[1], cal.gyro_origin[2],
        cal.gyro_sensitivity[0], cal.gyro_sensitivity[1], cal.gyro_sensitivity[2],
    );

    Some(cal)
}

/// Check if user calibration magic bytes (0xB2 0xA1) are present at the start
/// of a user calibration block.
///
/// User calibration blocks in SPI flash (0x8010, 0x801B, 0x8026) begin with
/// two magic bytes. If the magic is 0xB2 0xA1, valid user calibration data
/// follows at offset 2. If the magic is 0xFF 0xFF or anything else, the
/// factory calibration should be used instead.
pub fn check_user_cal_magic(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == USER_CAL_MAGIC[0] && data[1] == USER_CAL_MAGIC[1]
}

/// Validate stick calibration: `min < center < max` for both axes of both
/// sticks (using absolute values).
pub fn validate_stick_calibration(cal: &StickCalibration) -> bool {
    cal.left_min_x < cal.left_center_x
        && cal.left_center_x < cal.left_max_x
        && cal.left_min_y < cal.left_center_y
        && cal.left_center_y < cal.left_max_y
        && cal.right_min_x < cal.right_center_x
        && cal.right_center_x < cal.right_max_x
        && cal.right_min_y < cal.right_center_y
        && cal.right_center_y < cal.right_max_y
}

/// Default stick calibration values (Linux kernel fallback).
///
/// Used when SPI flash calibration data is missing, all zeros, or fails
/// validation.
pub fn default_stick_calibration() -> StickCalibration {
    StickCalibration {
        left_center_x: 2000,
        left_center_y: 2000,
        left_min_x: 500,
        left_min_y: 500,
        left_max_x: 3500,
        left_max_y: 3500,
        right_center_x: 2000,
        right_center_y: 2000,
        right_min_x: 500,
        right_min_y: 500,
        right_max_x: 3500,
        right_max_y: 3500,
        source: "default".into(),
        valid: true,
    }
}

/// Default IMU calibration (Linux kernel fallback).
///
/// Used when SPI flash calibration data is missing or all zeros.
/// - Accel sensitivity: 0x4000 (16384 = ~4096 counts/G for ±8G)
/// - Gyro sensitivity: 0x343B (13371 = ±2000 dps)
pub fn default_imu_calibration() -> ImuCalibration {
    ImuCalibration {
        accel_origin: [0, 0, 0],
        accel_sensitivity: [0x4000, 0x4000, 0x4000],
        gyro_origin: [0, 0, 0],
        gyro_sensitivity: [0x343B, 0x343B, 0x343B],
        source: "default".into(),
        horizontal_offsets: [0, 0, 0],
    }
}

/// Normalize a raw stick value using calibration data (piecewise linear,
/// Linux kernel / SDL formula).
///
/// Maps raw values to the range -1.0..=1.0 where 0.0 is center. Values above
/// center are scaled by `(max - center)` and values below center by
/// `(center - min)`, then clamped to ±32767 and divided to produce a float.
pub fn normalize_stick_calibrated(raw: u16, center: u16, min: u16, max: u16) -> f32 {
    const MAX_STICK_MAG: i32 = 32767;
    let raw = raw as i32;
    let center = center as i32;
    let min = min as i32;
    let max = max as i32;
    let new_val = if raw > center {
        (raw - center) * MAX_STICK_MAG / (max - center).max(1)
    } else {
        (center - raw) * -MAX_STICK_MAG / (center - min).max(1)
    };
    let clamped = new_val.clamp(-MAX_STICK_MAG, MAX_STICK_MAG);
    clamped as f32 / MAX_STICK_MAG as f32
}

/// Parse an 18-byte stick-calibration payload (legacy compatibility wrapper).
///
/// This function splits the 18 bytes into two 9-byte blocks (left stick bytes
/// 0–8, right stick bytes 9–17) and delegates to [`parse_left_stick_calibration`]
/// and [`parse_right_stick_calibration`]. The results are merged and validated.
///
/// **Note**: The old implementation read from SPI flash address 0x6080 (which
/// is actually "6-Axis Horizontal Offsets" for Joy-Con sideways mode). The
/// correct addresses are [`SPI_ADDR_LEFT_STICK_FACTORY`] (0x603D) and
/// [`SPI_ADDR_RIGHT_STICK_FACTORY`] (0x6046). This wrapper exists for backward
/// compatibility with callers that still pass a combined 18-byte block.
pub fn parse_stick_calibration_reply(data: &[u8]) -> Option<StickCalibration> {
    if data.len() < 18 {
        warn!(
            "Stick calibration reply too short: {} bytes (expected 18)",
            data.len()
        );
        return None;
    }

    let left = parse_left_stick_calibration(&data[0..9])?;
    let right = parse_right_stick_calibration(&data[9..18])?;
    let mut merged = merge_stick_calibration(&left, &right);
    if !merged.valid {
        warn!("Stick calibration failed validation — falling back to defaults");
        merged = default_stick_calibration();
    }
    Some(merged)
}

// ===========================================================================
//  NFC / IR MCU configuration & parsing
// ===========================================================================

/// MCU operating mode for the NFC/IR subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum NfcMode {
    /// MCU disabled (all-zero config).
    #[default]
    Disabled,
    /// NFC / Amiibo tag reading.
    Nfc,
    /// IR camera mode (Joy-Con only, defined for completeness).
    IrCamera,
    /// MCU passthrough mode.
    Passthrough,
}

/// Build set MCU config subcommand (0x21) for NFC/IR mode.
///
/// Configures the MCU for NFC tag reading or IR camera operation. The first
/// data byte selects the mode (0x00 disabled, 0x21 NFC, 0x31 IR, 0x41
/// passthrough); the remaining bytes are reserved/zero in this simplified
/// implementation. The last data byte (index 39) is a CRC-8-CCITT checksum
/// covering data[0..39].
pub fn build_set_mcu_config_subcmd(counter: u8, mode: NfcMode) -> Vec<u8> {
    let mode_byte = match mode {
        NfcMode::Disabled => 0x00,
        NfcMode::Nfc => 0x21,
        NfcMode::IrCamera => 0x31,
        NfcMode::Passthrough => 0x41,
    };
    let mut data = vec![0u8; 40];
    data[0] = mode_byte;
    // Compute CRC-8-CCITT over data[0..39] and store at data[39].
    data[39] = crc8_ccitt(&data[0..39]);
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SET_MCU_CONFIG, &data)
}

/// Build set MCU state subcommand (0x22) — suspend/resume the MCU.
///
/// `suspended = true` sends `0x00` (suspended); `false` sends `0x01` (active).
pub fn build_set_mcu_state_subcmd(counter: u8, suspended: bool) -> Vec<u8> {
    let data = if suspended { vec![0x00] } else { vec![0x01] };
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_SET_MCU_STATE, &data)
}

/// Build NFC tag read subcommand (0x23) — read tag data from the MCU.
///
/// No additional data is required; the controller replies with the current
/// NFC tag payload (if any) via a 0x31 input report.
pub fn build_get_nfc_data_subcmd(counter: u8) -> Vec<u8> {
    build_subcommand_packet(counter, [0u8; 8], SUBCMD_GET_NFC_DATA, &[])
}

/// NFC tag data extracted from 0x31 input reports.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NfcTagData {
    /// Unique identifier (4–7 bytes).
    pub uid: Vec<u8>,
    /// Tag type byte.
    pub tag_type: u8,
    /// Raw tag data payload.
    pub data: Vec<u8>,
    /// Detected as an Amiibo.
    pub is_amiibo: bool,
    /// Amiibo game data section.
    pub game_data: Vec<u8>,
}

/// Parse NFC tag data from a 0x31 input report payload.
///
/// The 0x31 report has the standard input (49 bytes) followed by the NFC/IR
/// payload starting at byte 49. Returns `None` if the report is too short or
/// no tag is present.
pub fn parse_nfc_tag_data(report: &[u8]) -> Option<NfcTagData> {
    if report.len() < 60 {
        return None;
    }

    // NFC data starts at byte 49 in a 0x31 report.
    let nfc_payload = &report[49..];

    // Check for valid NFC tag presence.
    if nfc_payload.is_empty() || nfc_payload[0] == 0x00 {
        return None;
    }

    // Extract UID (first 7 bytes after status).
    let uid_start = 1;
    let uid_end = (uid_start + 7).min(nfc_payload.len());
    let uid = nfc_payload[uid_start..uid_end].to_vec();

    // Tag type from byte 8.
    let tag_type = if nfc_payload.len() > 8 {
        nfc_payload[8]
    } else {
        0
    };

    // Amiibo detection: check for NDEF header or specific tag type.
    let is_amiibo = tag_type == 0x02 || (uid.len() >= 4 && uid[0] == 0x04);

    // Game data starts after header (typically byte 9+).
    let game_data_start = 9;
    let game_data = if nfc_payload.len() > game_data_start {
        nfc_payload[game_data_start..].to_vec()
    } else {
        Vec::new()
    };

    Some(NfcTagData {
        uid,
        tag_type,
        data: nfc_payload.to_vec(),
        is_amiibo,
        game_data,
    })
}

/// IR camera frame data extracted from 0x31 input reports.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IrCameraData {
    /// Frame width in pixels.
    pub width: u16,
    /// Frame height in pixels.
    pub height: u16,
    /// Pixel format identifier.
    pub pixel_format: u8,
    /// Raw frame pixel data.
    pub frame_data: Vec<u8>,
}

/// Parse IR camera data from a 0x31 input report.
///
/// The IR payload starts at byte 49. The first four bytes encode width and
/// height (little-endian u16 each), byte 4 is the pixel format, and the
/// remaining bytes are the frame data. Returns `None` if the report is too
/// short.
pub fn parse_ir_camera_data(report: &[u8]) -> Option<IrCameraData> {
    if report.len() < 60 {
        return None;
    }

    let ir_payload = &report[49..];
    if ir_payload.len() < 4 {
        return None;
    }

    let width = u16::from_le_bytes([ir_payload[0], ir_payload[1]]);
    let height = u16::from_le_bytes([ir_payload[2], ir_payload[3]]);
    let pixel_format = if ir_payload.len() > 4 {
        ir_payload[4]
    } else {
        0
    };
    let frame_data = if ir_payload.len() > 5 {
        ir_payload[5..].to_vec()
    } else {
        Vec::new()
    };

    Some(IrCameraData {
        width,
        height,
        pixel_format,
        frame_data,
    })
}

// ===========================================================================
//  USB command builders (0x80 series)
// ===========================================================================
//
// USB-connected Pro Controllers use an STM32 bridge MCU that requires a
// handshake sequence before accepting standard subcommands. These builders
// construct the raw 0x80 output reports for the USB initialization sequence.
//
// Wire format for USB commands:
//   [0x80, cmd_id, data...]
//
// Reference: Linux kernel `drivers/hid/hid-nintendo.c` (joycon_usb_send)

/// Build a USB command report (output report 0x80).
///
/// `cmd_id` is one of `USB_CMD_HANDSHAKE`, `USB_CMD_BAUDRATE_3M`,
/// `USB_CMD_NO_TIMEOUT`, `USB_CMD_EN_TIMEOUT`.
/// `data` is the optional payload bytes following the command ID.
pub fn build_usb_cmd(cmd_id: u8, data: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(2 + data.len());
    pkt.push(OUTPUT_USB_CMD);
    pkt.push(cmd_id);
    pkt.extend_from_slice(data);
    pkt
}

/// Build the USB handshake command (0x80 0x02).
///
/// This is the first command that must be sent after opening a USB-connected
/// Pro Controller. It establishes communication with the STM32 bridge MCU.
pub fn build_usb_handshake() -> Vec<u8> {
    build_usb_cmd(USB_CMD_HANDSHAKE, &[0x01])
}

/// Build the USB baudrate-3M command (0x80 0x03).
///
/// Switches the internal UART from 115200 baud to 3 Mbit/s for higher
/// throughput between the STM32 USB MCU and the Broadcom Bluetooth MCU.
pub fn build_usb_baudrate_3m() -> Vec<u8> {
    build_usb_cmd(USB_CMD_BAUDRATE_3M, &[0x03])
}

/// Build the USB no-timeout command (0x80 0x04).
///
/// **Critical for USB operation.** Without this, the STM32 bridge MCU times
/// out after ~5 seconds and reverts the controller to Bluetooth mode,
/// causing the USB HID device to disappear. This command disables the
/// timeout so the controller stays in USB mode indefinitely.
pub fn build_usb_no_timeout() -> Vec<u8> {
    build_usb_cmd(USB_CMD_NO_TIMEOUT, &[0x00])
}

/// Build the USB enable-timeout command (0x80 0x05).
///
/// Re-enables the USB timeout. Used on graceful disconnect to allow the
/// controller to fall back to Bluetooth mode.
pub fn build_usb_enable_timeout() -> Vec<u8> {
    build_usb_cmd(USB_CMD_EN_TIMEOUT, &[0x00])
}

/// Build the USB reset command (0x80 0x06).
///
/// Sent after enable-timeout on disconnect to fully reset the USB
/// connection. BetterJoy sends both 0x05 and 0x06 on disconnect to
/// ensure the STM32 cleanly reverts to Bluetooth mode.
pub fn build_usb_reset() -> Vec<u8> {
    build_usb_cmd(USB_CMD_RESET, &[0x01])
}

// ===========================================================================
//  Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Constants ---------------------------------------------------------

    #[test]
    fn constants_have_expected_values() {
        assert_eq!(SUBCMD_GET_STATE, 0x00);
        assert_eq!(SUBCMD_GET_DEVICE_INFO, 0x02);
        assert_eq!(SUBCMD_SET_REPORT_MODE, 0x03);
        assert_eq!(SUBCMD_TRIGGER_ELAPSED, 0x04);
        assert_eq!(SUBCMD_SPI_FLASH_READ, 0x10);
        assert_eq!(SUBCMD_SET_PLAYER_LIGHTS, 0x30);
        assert_eq!(SUBCMD_GET_PLAYER_LIGHTS, 0x31);
        assert_eq!(SUBCMD_SET_HOME_LIGHT, 0x38);
        assert_eq!(SUBCMD_ENABLE_IMU, 0x40);
        assert_eq!(SUBCMD_ENABLE_VIBRATION, 0x48);
        assert_eq!(OUTPUT_RUMBLE_ONLY, 0x10);
        assert_eq!(OUTPUT_SUBCOMMAND, 0x01);
    }

    // --- build_subcommand_packet -------------------------------------------

    #[test]
    fn build_subcommand_packet_layout() {
        let rumble = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];
        let pkt = build_subcommand_packet(0x05, rumble, 0x30, &[0x01, 0x02]);
        assert_eq!(pkt[0], 0x01);
        assert_eq!(pkt[1], 0x05);
        assert_eq!(&pkt[2..10], &rumble[..]);
        assert_eq!(pkt[10], 0x30);
        assert_eq!(&pkt[11..], &[0x01, 0x02]);
    }

    #[test]
    fn build_subcommand_packet_no_data() {
        let pkt = build_subcommand_packet(0x00, [0u8; 8], 0x00, &[]);
        assert_eq!(pkt.len(), 11);
        assert_eq!(pkt[0], 0x01);
        assert_eq!(pkt[10], 0x00);
    }

    // --- build_get_device_info_subcmd --------------------------------------

    #[test]
    fn build_get_device_info_subcmd_format() {
        let pkt = build_get_device_info_subcmd(0x01);
        assert_eq!(pkt[0], 0x01);
        assert_eq!(pkt[1], 0x01);
        assert_eq!(pkt[10], 0x02);
        assert_eq!(pkt.len(), 11);
    }

    // --- build_spi_flash_read_subcmd ---------------------------------------

    #[test]
    fn build_spi_flash_read_subcmd_format() {
        let pkt = build_spi_flash_read_subcmd(0x02, 0x006080, 0x12);
        assert_eq!(pkt[0], 0x01);
        assert_eq!(pkt[1], 0x02);
        assert_eq!(pkt[10], 0x10);
        // address little-endian: 0x80, 0x60, 0x00
        assert_eq!(pkt[11], 0x80);
        assert_eq!(pkt[12], 0x60);
        assert_eq!(pkt[13], 0x00);
        assert_eq!(pkt[14], 0x12);
    }

    // --- build_set_player_lights_subcmd ------------------------------------

    #[test]
    fn build_set_player_lights_subcmd_format() {
        // led_mask=0x01 (LED1 on), flash_mask=0x02 (LED2 flashing)
        // Combined byte: (0x02 << 4) | 0x01 = 0x21
        let pkt = build_set_player_lights_subcmd(0x03, 0x01, 0x02);
        assert_eq!(pkt[10], 0x30);
        assert_eq!(pkt[11], 0x21); // (flash << 4) | on
        assert_eq!(pkt.len(), 12); // 10 header + 1 subcmd + 1 data
    }

    #[test]
    fn build_set_player_lights_subcmd_all_on() {
        let pkt = build_set_player_lights_subcmd(0x03, 0x0F, 0x00);
        assert_eq!(pkt[11], 0x0F); // all on, no flash
    }

    #[test]
    fn build_set_player_lights_subcmd_all_flash() {
        let pkt = build_set_player_lights_subcmd(0x03, 0x00, 0x0F);
        assert_eq!(pkt[11], 0xF0); // all flashing, none steady
    }

    // --- build_set_home_light_subcmd ---------------------------------------

    #[test]
    fn build_set_home_light_subcmd_solid() {
        let pkt = build_set_home_light_subcmd(0x04, true, 100, "solid");
        assert_eq!(pkt[10], 0x38);
        assert_eq!(pkt.len(), 36); // 10 header + 1 subcmd + 25 data
                                   // Byte 0: 1 mini cycle, 175ms duration
        assert_eq!(pkt[11], 0x1F);
        // Byte 1: high nibble = intensity (15 for 100%), low = 0 (forever)
        assert_eq!(pkt[12] >> 4, 0x0F);
    }

    #[test]
    fn build_set_home_light_subcmd_breathing() {
        let pkt = build_set_home_light_subcmd(0x04, true, 50, "breathing");
        assert_eq!(pkt[10], 0x38);
        assert_eq!(pkt.len(), 36);
        // 2 mini cycles
        assert_eq!(pkt[11] >> 4, 0x02);
    }

    #[test]
    fn build_set_home_light_subcmd_disabled() {
        let pkt = build_set_home_light_subcmd(0x04, false, 0, "solid");
        assert_eq!(pkt[10], 0x38);
        assert_eq!(pkt[11], 0x10); // 1 cycle, 0ms (OFF)
        assert_eq!(pkt[12], 0x01); // 0% start, 1 full cycle
    }

    // --- build_enable_imu_subcmd -------------------------------------------

    #[test]
    fn build_enable_imu_subcmd_format() {
        let on = build_enable_imu_subcmd(0x05, true);
        assert_eq!(on[10], 0x40);
        assert_eq!(on[11], 0x01);

        let off = build_enable_imu_subcmd(0x05, false);
        assert_eq!(off[10], 0x40);
        assert_eq!(off[11], 0x00);
    }

    // --- build_enable_vibration_subcmd -------------------------------------

    #[test]
    fn build_enable_vibration_subcmd_format() {
        let on = build_enable_vibration_subcmd(0x06, true);
        assert_eq!(on[10], 0x48);
        assert_eq!(on[11], 0x01);

        let off = build_enable_vibration_subcmd(0x06, false);
        assert_eq!(off[10], 0x48);
        assert_eq!(off[11], 0x00);
    }

    // --- build_trigger_elapsed_subcmd --------------------------------------

    #[test]
    fn build_trigger_elapsed_subcmd_format() {
        let pkt = build_trigger_elapsed_subcmd(0x07);
        assert_eq!(pkt[10], 0x04);
        assert_eq!(pkt.len(), 11);
    }

    // --- Rumble encoding ----------------------------------------------------

    #[test]
    fn encode_rumble_frequency_clamps_low() {
        let enc = encode_rumble_frequency(10.0);
        // Clamped to 41.0 Hz: log2(41/10)*32 = log2(4.1)*32 ≈ 2.036*32 ≈ 65
        let expected = (41.0f32 / 10.0).log2() * 32.0;
        assert_eq!(enc, expected.round() as u8);
    }

    #[test]
    fn encode_rumble_frequency_clamps_high() {
        let enc = encode_rumble_frequency(5000.0);
        // Clamped to 1253.0 Hz
        let expected = (1253.0f32 / 10.0).log2() * 32.0;
        assert_eq!(enc, expected.round() as u8);
    }

    #[test]
    fn encode_rumble_frequency_midrange() {
        let enc = encode_rumble_frequency(160.0);
        // log2(160/10)*32 = log2(16)*32 = 4*32 = 128
        assert_eq!(enc, 128);
    }

    #[test]
    fn encode_rumble_amplitude_clamps() {
        assert_eq!(encode_rumble_amplitude(0.0), 0);
        assert_eq!(encode_rumble_amplitude(0.9), 255);
        assert_eq!(encode_rumble_amplitude(1.0), 255); // clamped to 0.9
        assert_eq!(encode_rumble_amplitude(-1.0), 0); // clamped to 0.0
    }

    #[test]
    fn encode_rumble_amplitude_midrange() {
        // 0.45 * 255 / 0.9 = 127.5 → 128 (rounded)
        assert_eq!(encode_rumble_amplitude(0.45), 128);
    }

    #[test]
    fn encode_rumble_motor_format() {
        // New HD rumble encoding: 4 bytes with HF/LF bit-packing per dekuNukem.
        // Just verify the format is 4 bytes and that non-zero amplitude produces
        // non-zero output.
        let motor = encode_rumble_motor(160.0, 0.45);
        assert_eq!(motor.len(), 4);
        // With amplitude > 0.23, encoded_amp should be non-zero
        // log2(0.45 * 8.7) * 32 = log2(3.915) * 32 ≈ 1.968 * 32 ≈ 63
        // hf_amp = 63 * 2 = 126, lf_amp = 63/2 + 64 = 95
        // The exact bytes depend on the bit-packing, just check non-zero.
        assert!(
            motor[1] != 0 || motor[3] != 0,
            "amplitude bytes should be non-zero for amp=0.45"
        );
    }

    #[test]
    fn build_rumble_report_format() {
        let report = build_rumble_report(0x08, 160.0, 0.45, 320.0, 0.9);
        assert_eq!(report.len(), 10);
        assert_eq!(report[0], 0x10);
        assert_eq!(report[1], 0x08);
        let left = encode_rumble_motor(160.0, 0.45);
        let right = encode_rumble_motor(320.0, 0.9);
        assert_eq!(&report[2..6], &left[..]);
        assert_eq!(&report[6..10], &right[..]);
    }

    #[test]
    fn build_rumble_report_zero() {
        let report = build_rumble_report(0x00, 160.0, 0.0, 160.0, 0.0);
        assert_eq!(report.len(), 10);
        assert_eq!(report[0], 0x10);
        // With amplitude 0, encoded_amp = 0.
        // HF amplitude = 0 * 2 = 0, LF amplitude = 0/2 + 64 = 0x40.
        // HF freq byte 1 = hf_amp(0) + hf_high_bit = 0
        // LF freq byte 3 = lf_amp & 0xFF = 0x40
        // Check HF amplitude bytes (byte 1 and byte 5 of the report)
        assert_eq!(report[3], 0); // left motor HF amplitude byte
        assert_eq!(report[7], 0); // right motor HF amplitude byte
    }

    // --- parse_device_info_reply -------------------------------------------

    #[test]
    fn parse_device_info_reply_valid() {
        let mut data = vec![0u8; 12];
        data[0] = 0x02; // fw major
        data[1] = 0x08; // fw minor
        data[2] = 0x03; // Pro Controller
        data[3] = 0x00;
        data[4] = 0x11; // MAC
        data[5] = 0x22;
        data[6] = 0x33;
        data[7] = 0x44;
        data[8] = 0x55;
        data[9] = 0x66;
        data[10] = 0x00;
        data[11] = 0x01; // colors_from_spi = true

        let info = parse_device_info_reply(&data).expect("should parse");
        assert_eq!(info.firmware_version, "2.8");
        assert_eq!(info.controller_type, 0x03);
        assert_eq!(info.mac_address, "11:22:33:44:55:66");
        assert!(info.colors_from_spi);
    }

    #[test]
    fn parse_device_info_reply_too_short() {
        assert!(parse_device_info_reply(&[0u8; 5]).is_none());
    }

    // --- parse_stick_calibration_reply (legacy wrapper) --------------------

    #[test]
    fn parse_stick_calibration_reply_valid() {
        // Build 18 bytes with known 12-bit packed values.
        // Left stick (factory format: max_above, center, min_below):
        //   max_above_x=0x200, max_above_y=0x300, center_x=0x800, center_y=0x800,
        //   min_below_x=0x300, min_below_y=0x200
        // Right stick (factory format: center, min_below, max_above):
        //   center_x=0x900, center_y=0x900, min_below_x=0x100, min_below_y=0x100,
        //   max_above_x=0x700, max_above_y=0x700

        let mut data = vec![0u8; 18];
        // Left: max_above, center, min_below
        data[0..3].copy_from_slice(&encode_12bit_pair(0x200, 0x300));
        data[3..6].copy_from_slice(&encode_12bit_pair(0x800, 0x800));
        data[6..9].copy_from_slice(&encode_12bit_pair(0x300, 0x200));
        // Right: center, min_below, max_above
        data[9..12].copy_from_slice(&encode_12bit_pair(0x900, 0x900));
        data[12..15].copy_from_slice(&encode_12bit_pair(0x100, 0x100));
        data[15..18].copy_from_slice(&encode_12bit_pair(0x700, 0x700));

        let cal = parse_stick_calibration_reply(&data).expect("should parse");
        // Left: center=0x800, abs_max=0x800+0x200=0xA00, abs_min=0x800-0x300=0x500
        assert_eq!(cal.left_center_x, 0x800);
        assert_eq!(cal.left_center_y, 0x800);
        assert_eq!(cal.left_max_x, 0xA00);
        assert_eq!(cal.left_max_y, 0xB00);
        assert_eq!(cal.left_min_x, 0x500);
        assert_eq!(cal.left_min_y, 0x600);
        // Right: center=0x900, abs_max=0x900+0x700=0x1000, abs_min=0x900-0x100=0x800
        assert_eq!(cal.right_center_x, 0x900);
        assert_eq!(cal.right_center_y, 0x900);
        assert_eq!(cal.right_max_x, 0x1000);
        assert_eq!(cal.right_max_y, 0x1000);
        assert_eq!(cal.right_min_x, 0x800);
        assert_eq!(cal.right_min_y, 0x800);
        // Should be valid (min < center < max for all axes)
        assert!(cal.valid);
    }

    #[test]
    fn parse_stick_calibration_reply_too_short() {
        assert!(parse_stick_calibration_reply(&[0u8; 10]).is_none());
    }

    // --- parse_left_stick_calibration --------------------------------------

    #[test]
    fn parse_left_stick_calibration_valid() {
        // Left stick factory format: [max_above, center, min_below]
        let mut data = [0u8; 9];
        data[0..3].copy_from_slice(&encode_12bit_pair(0x200, 0x200));
        data[3..6].copy_from_slice(&encode_12bit_pair(0x800, 0x800));
        data[6..9].copy_from_slice(&encode_12bit_pair(0x300, 0x300));

        let cal = parse_left_stick_calibration(&data).expect("should parse");
        assert_eq!(cal.left_center_x, 0x800);
        assert_eq!(cal.left_center_y, 0x800);
        assert_eq!(cal.left_max_x, 0xA00); // 0x800 + 0x200
        assert_eq!(cal.left_max_y, 0xA00);
        assert_eq!(cal.left_min_x, 0x500); // 0x800 - 0x300
        assert_eq!(cal.left_min_y, 0x500);
        assert_eq!(cal.source, "factory");
        // Right stick fields should be zero
        assert_eq!(cal.right_center_x, 0);
    }

    #[test]
    fn parse_left_stick_calibration_too_short() {
        assert!(parse_left_stick_calibration(&[0u8; 5]).is_none());
    }

    #[test]
    fn parse_left_stick_calibration_all_zeros() {
        assert!(parse_left_stick_calibration(&[0u8; 9]).is_none());
    }

    #[test]
    fn parse_left_stick_calibration_all_fff() {
        // All 0xFFF values → uninitialized
        let mut data = [0u8; 9];
        for chunk in data.chunks_exact_mut(3) {
            chunk.copy_from_slice(&encode_12bit_pair(0xFFF, 0xFFF));
        }
        assert!(parse_left_stick_calibration(&data).is_none());
    }

    // --- parse_right_stick_calibration -------------------------------------

    #[test]
    fn parse_right_stick_calibration_valid() {
        // Right stick factory format: [center, min_below, max_above]
        let mut data = [0u8; 9];
        data[0..3].copy_from_slice(&encode_12bit_pair(0x900, 0x900));
        data[3..6].copy_from_slice(&encode_12bit_pair(0x100, 0x100));
        data[6..9].copy_from_slice(&encode_12bit_pair(0x700, 0x700));

        let cal = parse_right_stick_calibration(&data).expect("should parse");
        assert_eq!(cal.right_center_x, 0x900);
        assert_eq!(cal.right_center_y, 0x900);
        assert_eq!(cal.right_max_x, 0x1000); // 0x900 + 0x700
        assert_eq!(cal.right_max_y, 0x1000);
        assert_eq!(cal.right_min_x, 0x800); // 0x900 - 0x100
        assert_eq!(cal.right_min_y, 0x800);
        assert_eq!(cal.source, "factory");
        // Left stick fields should be zero
        assert_eq!(cal.left_center_x, 0);
    }

    #[test]
    fn parse_right_stick_calibration_too_short() {
        assert!(parse_right_stick_calibration(&[0u8; 5]).is_none());
    }

    #[test]
    fn parse_right_stick_calibration_all_zeros() {
        assert!(parse_right_stick_calibration(&[0u8; 9]).is_none());
    }

    // --- left vs right byte order difference -------------------------------

    #[test]
    fn left_and_right_byte_order_differ() {
        // The same 9 bytes parsed as left vs right should give different
        // results because the byte order is different.
        let mut data = [0u8; 9];
        // As left: [max_above=0x200, center=0x800, min_below=0x300]
        data[0..3].copy_from_slice(&encode_12bit_pair(0x200, 0x200));
        data[3..6].copy_from_slice(&encode_12bit_pair(0x800, 0x800));
        data[6..9].copy_from_slice(&encode_12bit_pair(0x300, 0x300));

        let left_cal = parse_left_stick_calibration(&data).unwrap();
        let right_cal = parse_right_stick_calibration(&data).unwrap();

        // As left: center=0x800, max=0xA00, min=0x500
        assert_eq!(left_cal.left_center_x, 0x800);
        assert_eq!(left_cal.left_max_x, 0xA00);
        assert_eq!(left_cal.left_min_x, 0x500);

        // As right: center=0x200 (first group), min_below=0x800, max_above=0x300
        // abs_min = 0x200 - 0x800 = 0 (saturating), abs_max = 0x200 + 0x300 = 0x500
        assert_eq!(right_cal.right_center_x, 0x200);
        assert_eq!(right_cal.right_max_x, 0x500);
        assert_eq!(right_cal.right_min_x, 0x000); // saturating sub
    }

    // --- merge_stick_calibration -------------------------------------------

    #[test]
    fn merge_stick_calibration_combines_both() {
        let mut left_data = [0u8; 9];
        left_data[0..3].copy_from_slice(&encode_12bit_pair(0x200, 0x200));
        left_data[3..6].copy_from_slice(&encode_12bit_pair(0x800, 0x800));
        left_data[6..9].copy_from_slice(&encode_12bit_pair(0x300, 0x300));
        let left = parse_left_stick_calibration(&left_data).unwrap();

        let mut right_data = [0u8; 9];
        right_data[0..3].copy_from_slice(&encode_12bit_pair(0x900, 0x900));
        right_data[3..6].copy_from_slice(&encode_12bit_pair(0x100, 0x100));
        right_data[6..9].copy_from_slice(&encode_12bit_pair(0x700, 0x700));
        let right = parse_right_stick_calibration(&right_data).unwrap();

        let merged = merge_stick_calibration(&left, &right);
        assert_eq!(merged.left_center_x, 0x800);
        assert_eq!(merged.right_center_x, 0x900);
        assert!(merged.valid);
    }

    // --- parse_imu_calibration ---------------------------------------------

    #[test]
    fn parse_imu_calibration_valid() {
        let mut data = vec![0u8; 24];
        // Accel origin: [10, -20, 30]
        data[0..2].copy_from_slice(&10i16.to_le_bytes());
        data[2..4].copy_from_slice(&(-20i16).to_le_bytes());
        data[4..6].copy_from_slice(&30i16.to_le_bytes());
        // Accel sensitivity: [0x4000, 0x4001, 0x4002]
        data[6..8].copy_from_slice(&0x4000i16.to_le_bytes());
        data[8..10].copy_from_slice(&0x4001i16.to_le_bytes());
        data[10..12].copy_from_slice(&0x4002i16.to_le_bytes());
        // Gyro origin: [-5, 0, 5]
        data[12..14].copy_from_slice(&(-5i16).to_le_bytes());
        data[14..16].copy_from_slice(&0i16.to_le_bytes());
        data[16..18].copy_from_slice(&5i16.to_le_bytes());
        // Gyro sensitivity: [0x343B, 0x343C, 0x343D]
        data[18..20].copy_from_slice(&0x343Bi16.to_le_bytes());
        data[20..22].copy_from_slice(&0x343Ci16.to_le_bytes());
        data[22..24].copy_from_slice(&0x343Di16.to_le_bytes());

        let cal = parse_imu_calibration(&data).expect("should parse");
        assert_eq!(cal.accel_origin, [10, -20, 30]);
        assert_eq!(cal.accel_sensitivity, [0x4000, 0x4001, 0x4002]);
        assert_eq!(cal.gyro_origin, [-5, 0, 5]);
        assert_eq!(cal.gyro_sensitivity, [0x343B, 0x343C, 0x343D]);
        assert_eq!(cal.source, "factory");
    }

    #[test]
    fn parse_imu_calibration_too_short() {
        assert!(parse_imu_calibration(&[0u8; 10]).is_none());
    }

    #[test]
    fn parse_imu_calibration_all_zeros() {
        assert!(parse_imu_calibration(&[0u8; 24]).is_none());
    }

    // --- check_user_cal_magic ----------------------------------------------

    #[test]
    fn check_user_cal_magic_valid() {
        assert!(check_user_cal_magic(&[0xB2, 0xA1, 0x00, 0x01]));
    }

    #[test]
    fn check_user_cal_magic_ff() {
        assert!(!check_user_cal_magic(&[0xFF, 0xFF, 0x00, 0x01]));
    }

    #[test]
    fn check_user_cal_magic_wrong() {
        assert!(!check_user_cal_magic(&[0x00, 0x00, 0x00, 0x01]));
    }

    #[test]
    fn check_user_cal_magic_too_short() {
        assert!(!check_user_cal_magic(&[0xB2]));
        assert!(!check_user_cal_magic(&[]));
    }

    // --- validate_stick_calibration ----------------------------------------

    #[test]
    fn validate_stick_calibration_valid() {
        let cal = StickCalibration {
            left_center_x: 2000,
            left_center_y: 2000,
            left_min_x: 500,
            left_min_y: 500,
            left_max_x: 3500,
            left_max_y: 3500,
            right_center_x: 2000,
            right_center_y: 2000,
            right_min_x: 500,
            right_min_y: 500,
            right_max_x: 3500,
            right_max_y: 3500,
            source: "default".into(),
            valid: true,
        };
        assert!(validate_stick_calibration(&cal));
    }

    #[test]
    fn validate_stick_calibration_min_eq_center() {
        let cal = StickCalibration {
            left_center_x: 2000,
            left_min_x: 2000, // min == center → invalid
            ..default_stick_calibration()
        };
        assert!(!validate_stick_calibration(&cal));
    }

    #[test]
    fn validate_stick_calibration_center_eq_max() {
        let cal = StickCalibration {
            left_center_x: 3500,
            left_max_x: 3500, // center == max → invalid
            ..default_stick_calibration()
        };
        assert!(!validate_stick_calibration(&cal));
    }

    #[test]
    fn validate_stick_calibration_right_invalid() {
        let cal = StickCalibration {
            right_min_x: 3000,
            right_center_x: 2000, // min > center → invalid
            ..default_stick_calibration()
        };
        assert!(!validate_stick_calibration(&cal));
    }

    // --- default_stick_calibration -----------------------------------------

    #[test]
    fn default_stick_calibration_values() {
        let cal = default_stick_calibration();
        assert_eq!(cal.left_center_x, 2000);
        assert_eq!(cal.left_center_y, 2000);
        assert_eq!(cal.left_min_x, 500);
        assert_eq!(cal.left_min_y, 500);
        assert_eq!(cal.left_max_x, 3500);
        assert_eq!(cal.left_max_y, 3500);
        assert_eq!(cal.right_center_x, 2000);
        assert_eq!(cal.right_center_y, 2000);
        assert_eq!(cal.right_min_x, 500);
        assert_eq!(cal.right_min_y, 500);
        assert_eq!(cal.right_max_x, 3500);
        assert_eq!(cal.right_max_y, 3500);
        assert_eq!(cal.source, "default");
        assert!(cal.valid);
        assert!(validate_stick_calibration(&cal));
    }

    // --- default_imu_calibration -------------------------------------------

    #[test]
    fn default_imu_calibration_values() {
        let cal = default_imu_calibration();
        assert_eq!(cal.accel_origin, [0, 0, 0]);
        assert_eq!(cal.accel_sensitivity, [0x4000, 0x4000, 0x4000]);
        assert_eq!(cal.gyro_origin, [0, 0, 0]);
        assert_eq!(cal.gyro_sensitivity, [0x343B, 0x343B, 0x343B]);
        assert_eq!(cal.source, "default");
    }

    // --- normalize_stick_calibrated ----------------------------------------

    #[test]
    fn normalize_stick_calibrated_at_center() {
        let val = normalize_stick_calibrated(2000, 2000, 500, 3500);
        assert!(
            (val - 0.0).abs() < 0.001,
            "center should be 0.0, got {}",
            val
        );
    }

    #[test]
    fn normalize_stick_calibrated_at_max() {
        let val = normalize_stick_calibrated(3500, 2000, 500, 3500);
        assert!((val - 1.0).abs() < 0.001, "max should be 1.0, got {}", val);
    }

    #[test]
    fn normalize_stick_calibrated_at_min() {
        let val = normalize_stick_calibrated(500, 2000, 500, 3500);
        assert!(
            (val - (-1.0)).abs() < 0.001,
            "min should be -1.0, got {}",
            val
        );
    }

    #[test]
    fn normalize_stick_calibrated_above_max_clamps() {
        let val = normalize_stick_calibrated(4000, 2000, 500, 3500);
        assert!(
            (val - 1.0).abs() < 0.001,
            "above max should clamp to 1.0, got {}",
            val
        );
    }

    #[test]
    fn normalize_stick_calibrated_below_min_clamps() {
        let val = normalize_stick_calibrated(0, 2000, 500, 3500);
        assert!(
            (val - (-1.0)).abs() < 0.001,
            "below min should clamp to -1.0, got {}",
            val
        );
    }

    #[test]
    fn normalize_stick_calibrated_asymmetric() {
        // Asymmetric calibration: center=2000, min=1900 (close), max=3500 (far)
        // Below center: (2000 - 1950) * -32767 / (2000 - 1900) = 50 * -32767 / 100 = -16383.5
        // → -16383 / 32767 ≈ -0.5
        let val = normalize_stick_calibrated(1950, 2000, 1900, 3500);
        assert!(
            (val - (-0.5)).abs() < 0.01,
            "asymmetric below center should be ~-0.5, got {}",
            val
        );
    }

    // --- SPI flash address constants ---------------------------------------

    #[test]
    fn spi_flash_addresses_have_expected_values() {
        assert_eq!(SPI_ADDR_LEFT_STICK_FACTORY, 0x603D);
        assert_eq!(SPI_ADDR_RIGHT_STICK_FACTORY, 0x6046);
        assert_eq!(SPI_ADDR_IMU_FACTORY, 0x6020);
        assert_eq!(SPI_ADDR_LEFT_STICK_USER, 0x8010);
        assert_eq!(SPI_ADDR_RIGHT_STICK_USER, 0x801B);
        assert_eq!(SPI_ADDR_IMU_USER, 0x8026);
    }

    #[test]
    fn user_cal_magic_constant() {
        assert_eq!(USER_CAL_MAGIC, [0xB2, 0xA1]);
    }

    // --- SubcommandManager --------------------------------------------------

    #[tokio::test]
    async fn manager_register_and_handle_ack() {
        let mgr = SubcommandManager::new(Duration::from_secs(1));
        let rx = mgr.register_pending(0x02).await;

        mgr.handle_reply(0x02, 0x80, vec![0x01, 0x02, 0x03]).await;

        let result = rx.await.expect("receiver not dropped");
        let resp = result.expect("should be Ok");
        assert_eq!(resp.subcmd_id, 0x02);
        assert_eq!(resp.ack, 0x80);
        assert_eq!(resp.data, vec![0x01, 0x02, 0x03]);
    }

    #[tokio::test]
    async fn manager_handle_nack() {
        let mgr = SubcommandManager::new(Duration::from_secs(1));
        let rx = mgr.register_pending(0x30).await;

        mgr.handle_reply(0x30, 0x00, vec![]).await;

        let result = rx.await.expect("receiver not dropped");
        match result {
            Err(SubcommandError::Nack(0x00)) => {}
            other => panic!("expected Nack(0x00), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn manager_handle_reply_no_waiter() {
        let mgr = SubcommandManager::new(Duration::from_secs(1));
        // Should not panic when no waiter is registered.
        mgr.handle_reply(0x40, 0x80, vec![]).await;
    }

    #[test]
    fn manager_timeout_duration() {
        let mgr = SubcommandManager::new(Duration::from_millis(500));
        assert_eq!(mgr.timeout_duration(), Duration::from_millis(500));
    }

    // --- NFC / IR constants -------------------------------------------------

    #[test]
    fn nfc_ir_constants_have_expected_values() {
        assert_eq!(SUBCMD_SET_NFC_MODE, 0x21);
        assert_eq!(SUBCMD_SET_NFC_CONFIG, 0x22);
        assert_eq!(SUBCMD_GET_NFC_DATA, 0x23);
        assert_eq!(SUBCMD_SET_MCU_CONFIG, 0x21);
        assert_eq!(SUBCMD_SET_MCU_STATE, 0x22);
        assert_eq!(OUTPUT_NFC_IR_MCU, 0x11);
    }

    // --- build_set_mcu_config_subcmd ---------------------------------------

    #[test]
    fn build_set_mcu_config_nfc_mode() {
        let pkt = build_set_mcu_config_subcmd(0x09, NfcMode::Nfc);
        assert_eq!(pkt[0], 0x01);
        assert_eq!(pkt[1], 0x09);
        assert_eq!(pkt[10], 0x21); // subcmd id
                                   // 40 data bytes, first byte is the NFC mode selector.
        assert_eq!(pkt.len(), 11 + 40);
        assert_eq!(pkt[11], 0x21);
        // Bytes 12..49 should be zero (config parameters), but byte 50
        // (data[39]) is now the CRC-8-CCITT checksum, which may be non-zero.
        for i in 12..(11 + 39) {
            assert_eq!(pkt[i], 0x00);
        }
        // Last byte is CRC — just verify it's present (may be 0 or non-zero)
        let _crc_byte = pkt[11 + 39];
    }

    #[test]
    fn build_set_mcu_config_disabled() {
        let pkt = build_set_mcu_config_subcmd(0x01, NfcMode::Disabled);
        assert_eq!(pkt[10], 0x21);
        assert_eq!(pkt.len(), 11 + 40);
        // All 40 data bytes zero.
        for i in 11..(11 + 40) {
            assert_eq!(pkt[i], 0x00);
        }
    }

    #[test]
    fn build_set_mcu_config_ir_and_passthrough() {
        let ir = build_set_mcu_config_subcmd(0x02, NfcMode::IrCamera);
        assert_eq!(ir[11], 0x31);

        let pt = build_set_mcu_config_subcmd(0x03, NfcMode::Passthrough);
        assert_eq!(pt[11], 0x41);
    }

    // --- build_set_mcu_state_subcmd ----------------------------------------

    #[test]
    fn build_set_mcu_state_suspended() {
        let pkt = build_set_mcu_state_subcmd(0x04, true);
        assert_eq!(pkt[0], 0x01);
        assert_eq!(pkt[1], 0x04);
        assert_eq!(pkt[10], 0x22);
        assert_eq!(pkt.len(), 12);
        assert_eq!(pkt[11], 0x00);
    }

    #[test]
    fn build_set_mcu_state_active() {
        let pkt = build_set_mcu_state_subcmd(0x05, false);
        assert_eq!(pkt[10], 0x22);
        assert_eq!(pkt.len(), 12);
        assert_eq!(pkt[11], 0x01);
    }

    // --- build_get_nfc_data_subcmd -----------------------------------------

    #[test]
    fn build_get_nfc_data_subcmd_format() {
        let pkt = build_get_nfc_data_subcmd(0x0A);
        assert_eq!(pkt[0], 0x01);
        assert_eq!(pkt[1], 0x0A);
        assert_eq!(pkt[10], 0x23);
        assert_eq!(pkt.len(), 11); // no data bytes
    }

    // --- parse_nfc_tag_data ------------------------------------------------

    #[test]
    fn parse_nfc_tag_data_valid() {
        // Build a 70-byte 0x31 report with a non-zero NFC payload at byte 49.
        let mut report = vec![0u8; 70];
        report[49] = 0x01; // status non-zero
                           // UID bytes 50..57
        for i in 0..7 {
            report[50 + i] = 0x10 + i as u8;
        }
        report[57] = 0x02; // tag_type at byte 49+8=57

        let tag = parse_nfc_tag_data(&report).expect("should parse");
        assert_eq!(tag.uid, vec![0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16]);
        assert_eq!(tag.tag_type, 0x02);
        assert!(tag.is_amiibo); // tag_type == 0x02
        assert!(!tag.data.is_empty());
    }

    #[test]
    fn parse_nfc_tag_data_too_short() {
        assert!(parse_nfc_tag_data(&[0u8; 59]).is_none());
    }

    #[test]
    fn parse_nfc_tag_data_empty() {
        // 60-byte report but NFC payload starts with 0x00 (no tag).
        let report = vec![0u8; 60];
        assert!(parse_nfc_tag_data(&report).is_none());
    }

    #[test]
    fn parse_nfc_tag_data_amiibo() {
        // UID starting with 0x04 should be detected as Amiibo.
        let mut report = vec![0u8; 70];
        report[49] = 0x01; // status non-zero
        report[50] = 0x04; // uid[0] == 0x04 → amiibo
        report[51] = 0xAB;
        report[52] = 0xCD;
        report[53] = 0xEF;
        // tag_type not 0x02, but uid[0]==0x04 triggers amiibo detection
        report[57] = 0x00;

        let tag = parse_nfc_tag_data(&report).expect("should parse");
        assert!(tag.is_amiibo);
        assert_eq!(tag.uid[0], 0x04);
    }

    // --- parse_ir_camera_data ----------------------------------------------

    #[test]
    fn parse_ir_camera_data_valid() {
        // 60-byte report: ir_payload = report[49..60] (11 bytes).
        // width=320 (0x0140), height=240 (0x00F0), fmt=0x01, frame=ir_payload[5..11].
        let mut report = vec![0u8; 60];
        report[49] = 0x40;
        report[50] = 0x01;
        report[51] = 0xF0;
        report[52] = 0x00;
        report[53] = 0x01; // pixel_format
        report[54] = 0xAA; // frame data start
        report[55] = 0xBB;

        let ir = parse_ir_camera_data(&report).expect("should parse");
        assert_eq!(ir.width, 320);
        assert_eq!(ir.height, 240);
        assert_eq!(ir.pixel_format, 0x01);
        // frame_data is ir_payload[5..] = 6 bytes: [0xAA, 0xBB, 0, 0, 0, 0]
        assert_eq!(ir.frame_data.len(), 6);
        assert_eq!(ir.frame_data[0], 0xAA);
        assert_eq!(ir.frame_data[1], 0xBB);
    }

    #[test]
    fn parse_ir_camera_data_too_short() {
        assert!(parse_ir_camera_data(&[0u8; 59]).is_none());
    }
}
