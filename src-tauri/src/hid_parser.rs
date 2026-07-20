use crate::state::{ButtonState, StickState};
use log::{debug, warn};
use serde::{Deserialize, Serialize};

// Re-export subcmd builders so callers can use them directly from hid_parser.
#[allow(unused_imports)]
pub use crate::subcmd::{
    build_enable_imu_subcmd, build_enable_vibration_subcmd, build_get_device_info_subcmd,
    build_rumble_report as build_hd_rumble_report, build_set_home_light_subcmd,
    build_set_player_lights_subcmd, build_spi_flash_read_subcmd, parse_ir_camera_data,
    parse_nfc_tag_data, IrCameraData, NfcMode, NfcTagData,
};

pub const NINTENDO_VID: u16 = 0x057E;
pub const PRO_CONTROLLER_PID: u16 = 0x2009;

pub const REPORT_ID_STANDARD: u8 = 0x30;
pub const REPORT_ID_SUBCMD_REPLY: u8 = 0x21;
pub const REPORT_ID_NFC_IR: u8 = 0x31;
/// Default report the Pro Controller sends over Bluetooth before it receives
/// the set-report-mode subcommand (0x03). Has the same button/stick layout as
/// 0x30 — only the IMU data differs.
pub const REPORT_ID_DEFAULT_BT: u8 = 0x3F;
/// USB command reply report ID (0x81). Sent by the STM32 bridge MCU in
/// response to 0x80 USB commands (handshake, baudrate, timeout).
pub const REPORT_ID_USB_REPLY: u8 = 0x81;

pub const STICK_CENTER: u16 = 0x800;
pub const STICK_MAX: u16 = 0xFFF;
pub const STICK_MIN: u16 = 0x000;

/// Battery info extracted from byte 2 of standard/subcmd reports.
/// High nibble: bits 3-1 = level (0=empty, 2=critical, 4=low, 6=medium, 8=full),
/// bit 0 = charging flag.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BatteryInfo {
    pub raw: u8,
    pub charging: bool,
    pub connection_type: u8,
}

/// IMU sensor frame: 3-axis accelerometer + 3-axis gyroscope (int16 LE).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ImuFrame {
    pub accel_x: i16,
    pub accel_y: i16,
    pub accel_z: i16,
    pub gyro_x: i16,
    pub gyro_y: i16,
    pub gyro_z: i16,
}

/// Up to 3 IMU frames per 0x30 report (60 Hz → 180 Hz IMU sampling).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ImuData {
    pub frames: [ImuFrame; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedInput {
    pub buttons: ButtonState,
    pub left_stick: StickState,
    pub right_stick: StickState,
    pub timer: u8,
    pub report_id: u8,
    pub battery: BatteryInfo,
    pub imu: Option<ImuData>,
    /// Vibrator input report (byte 12 of 0x30 report).
    /// Indicates the controller's rumble pattern request state.
    pub vibrator: u8,
}

#[derive(Debug, Clone)]
pub struct SubcmdReply {
    pub battery: BatteryInfo,
    pub buttons: ButtonState,
    pub ack: u8,
    /// ACK data type (byte 13 & 0x7F). 0x00 = simple ACK/NACK, other values indicate data type.
    pub ack_data_type: u8,
    pub subcmd_id: u8,
    pub reply_data: Vec<u8>,
}

/// Parsed 0x31 NFC/IR input report
/// Contains standard input data (0-48) + NFC/IR payload (49+)
#[derive(Debug, Clone)]
pub struct ParsedNfcReport {
    pub standard: ParsedInput, // Standard input portion (bytes 0-48)
    pub nfc_tag: Option<crate::subcmd::NfcTagData>,
    pub ir_frame: Option<crate::subcmd::IrCameraData>,
}

pub fn parse_standard_report(data: &[u8]) -> Option<ParsedInput> {
    if data.len() < 12 {
        warn!("Standard report too short: {} bytes", data.len());
        return None;
    }

    let report_id = data[0];
    if report_id != REPORT_ID_STANDARD && report_id != REPORT_ID_DEFAULT_BT {
        debug!("Not a standard report (ID=0x{:02X})", report_id);
        return None;
    }

    // 0x3F (default Bluetooth report) uses a different button/stick layout
    // than 0x30 — delegate to the simple HID parser.
    if report_id == REPORT_ID_DEFAULT_BT {
        return parse_simple_hid_report(data);
    }

    let timer = data[1];

    // Byte 2: high nibble = battery level + charging, low nibble = connection info.
    //   bits 3-1 of high nibble: 0=empty, 2=critical, 4=low, 6=medium, 8=full
    //   bit 0 of high nibble: 1=charging
    let battery = parse_battery_info(data[2]);

    // Bluetooth 0x30 report layout (dekuNukem spec):
    //   data[3] = right Joy-Con buttons (Y/X/B/A/SR/SL/R/ZR)
    //   data[4] = shared buttons (minus/plus/R-stick/L-stick/home/capture)
    //   data[5] = left Joy-Con buttons (down/up/right/left/SR/SL/L/ZL)
    //   data[6..9] = left stick (12-bit packed), data[9..12] = right stick
    //   data[13..49] = IMU data (only in 0x30, 3 frames × 12 bytes)
    let right_btn = data[3];
    let shared_btn = data[4];
    let left_btn = data[5];

    let buttons = parse_buttons(right_btn, shared_btn, left_btn);

    let left_stick = parse_stick(&data[6..9]);
    let right_stick = parse_stick(&data[9..12]);

    // IMU data is only present in 0x30 reports (not 0x3F).
    let imu = if report_id == REPORT_ID_STANDARD && data.len() >= 49 {
        Some(parse_imu(&data[13..49]))
    } else {
        None
    };

    Some(ParsedInput {
        buttons,
        left_stick,
        right_stick,
        timer,
        report_id,
        battery,
        imu,
        vibrator: data.get(12).copied().unwrap_or(0),
    })
}

/// Parse battery info from byte 2 of any standard/subcmd input report.
/// High nibble: bits 3-1 = level (even: 0=empty,2=critical,4=low,6=medium,8=full),
/// bit 0 = charging. Low nibble = connection info.
pub fn parse_battery_info(byte2: u8) -> BatteryInfo {
    let high_nibble = (byte2 >> 4) & 0x0F;
    BatteryInfo {
        raw: high_nibble,
        charging: high_nibble & 0x01 != 0,
        connection_type: byte2 & 0x0F,
    }
}

/// Shared button parsing for 0x30 and 0x21 reports.
/// Note: dekuNukem spec says bit0=Y, bit1=X, bit2=B, bit3=A, but empirical
/// testing on a Pro Controller over Bluetooth shows bit0=X, bit1=Y, bit2=A,
/// bit3=B. We use the empirically verified layout.
fn parse_buttons(right_btn: u8, shared_btn: u8, left_btn: u8) -> ButtonState {
    ButtonState {
        // Byte 3 (Right): bit0=X, bit1=Y, bit2=A, bit3=B, bit4=SR, bit5=SL, bit6=R, bit7=ZR
        x: right_btn & 0x01 != 0,
        y: right_btn & 0x02 != 0,
        a: right_btn & 0x04 != 0,
        b: right_btn & 0x08 != 0,
        sr_right: right_btn & 0x10 != 0,
        sl_right: right_btn & 0x20 != 0,
        r: right_btn & 0x40 != 0,
        zr: right_btn & 0x80 != 0,
        // Byte 4 (Shared): bit0=Minus, bit1=Plus, bit2=R-Stick, bit3=L-Stick, bit4=Home, bit5=Capture
        minus: shared_btn & 0x01 != 0,
        plus: shared_btn & 0x02 != 0,
        stick_r: shared_btn & 0x04 != 0,
        stick_l: shared_btn & 0x08 != 0,
        home: shared_btn & 0x10 != 0,
        capture: shared_btn & 0x20 != 0,
        // Byte 5 (Left): bit0=Down, bit1=Up, bit2=Right, bit3=Left, bit4=SR, bit5=SL, bit6=L, bit7=ZL
        dpad_down: left_btn & 0x01 != 0,
        dpad_up: left_btn & 0x02 != 0,
        dpad_right: left_btn & 0x04 != 0,
        dpad_left: left_btn & 0x08 != 0,
        sr_left: left_btn & 0x10 != 0,
        sl_left: left_btn & 0x20 != 0,
        l: left_btn & 0x40 != 0,
        zl: left_btn & 0x80 != 0,
    }
}

/// Parse 3 IMU frames from bytes 13..49 of a 0x30 report.
/// Each frame is 12 bytes: 3 int16 accel (X,Y,Z) + 3 int16 gyro (X,Y,Z), all little-endian.
fn parse_imu(data: &[u8]) -> ImuData {
    let mut frames = [ImuFrame::default(); 3];
    for (i, frame) in frames.iter_mut().enumerate() {
        let off = i * 12;
        if off + 12 > data.len() {
            break;
        }
        *frame = ImuFrame {
            accel_x: i16::from_le_bytes([data[off], data[off + 1]]),
            accel_y: i16::from_le_bytes([data[off + 2], data[off + 3]]),
            accel_z: i16::from_le_bytes([data[off + 4], data[off + 5]]),
            gyro_x: i16::from_le_bytes([data[off + 6], data[off + 7]]),
            gyro_y: i16::from_le_bytes([data[off + 8], data[off + 9]]),
            gyro_z: i16::from_le_bytes([data[off + 10], data[off + 11]]),
        };
    }
    ImuData { frames }
}

fn parse_stick(data: &[u8]) -> StickState {
    if data.len() < 3 {
        return StickState::default();
    }

    let raw_x = (data[0] as u16) | ((data[1] as u16 & 0x0F) << 8);
    let raw_y = ((data[1] as u16 >> 4) & 0x0F) | ((data[2] as u16) << 4);

    let x = normalize_stick(raw_x);
    let y = normalize_stick(raw_y);

    StickState { x, y, raw_x, raw_y }
}

fn normalize_stick(raw: u16) -> f32 {
    if raw == STICK_CENTER {
        return 0.0;
    }
    let normalized = (raw as f32 - STICK_CENTER as f32) / (STICK_MAX as f32 - STICK_CENTER as f32);
    normalized.clamp(-1.0, 1.0)
}

/// Parse a 3-byte stick report using factory calibration data.
///
/// Unlike [`parse_stick`] which uses the default center (0x800) and max
/// (0xFFF), this function uses the provided calibrated center/min/max to
/// produce a properly normalized value via
/// [`subcmd::normalize_stick_calibrated`]. Falls back to the default
/// [`normalize_stick`] when `cal_valid` is `false`.
#[allow(clippy::too_many_arguments)] // parsing function with independent calibration axes
pub fn parse_stick_calibrated(
    data: &[u8],
    center_x: u16,
    min_x: u16,
    max_x: u16,
    center_y: u16,
    min_y: u16,
    max_y: u16,
    cal_valid: bool,
) -> StickState {
    if data.len() < 3 {
        return StickState::default();
    }

    let raw_x = (data[0] as u16) | ((data[1] as u16 & 0x0F) << 8);
    let raw_y = ((data[1] as u16 >> 4) & 0x0F) | ((data[2] as u16) << 4);

    let (x, y) = if cal_valid {
        (
            crate::subcmd::normalize_stick_calibrated(raw_x, center_x, min_x, max_x),
            crate::subcmd::normalize_stick_calibrated(raw_y, center_y, min_y, max_y),
        )
    } else {
        (normalize_stick(raw_x), normalize_stick(raw_y))
    };

    StickState { x, y, raw_x, raw_y }
}

pub fn parse_subcmd_reply(data: &[u8]) -> Option<SubcmdReply> {
    if data.len() < 15 {
        warn!("Subcommand reply too short: {} bytes", data.len());
        return None;
    }

    let report_id = data[0];
    if report_id != REPORT_ID_SUBCMD_REPLY {
        debug!("Not a subcommand reply (ID=0x{:02X})", report_id);
        return None;
    }

    let battery = parse_battery_info(data[2]);

    let right_btn = data[3];
    let shared_btn = data[4];
    // 0x21 reports don't include the left button byte in the same position,
    // but byte 5 is present per the standard report format.
    if data.len() <= 5 {
        warn!("Subcommand reply missing left button byte");
        return None;
    }
    let left_btn = data[5];
    let buttons = parse_buttons(right_btn, shared_btn, left_btn);

    // Byte 13 = ACK byte (MSB=1 for ACK), byte 14 = subcommand ID being replied to.
    let ack = data[13];
    let subcmd_id = data[14];
    let reply_data = data[15..].to_vec();

    Some(SubcmdReply {
        battery,
        buttons,
        ack,
        ack_data_type: ack & 0x7F,
        subcmd_id,
        reply_data,
    })
}

/// Parse a 0x31 NFC/IR input report
/// The first 49 bytes are the same as a standard 0x30 report
/// Bytes 49+ contain NFC tag data or IR camera frame data
pub fn parse_nfc_ir_report(data: &[u8]) -> Option<ParsedNfcReport> {
    if data.is_empty() {
        return None;
    }

    let report_id = data[0];
    if report_id != REPORT_ID_NFC_IR {
        return None;
    }

    // Parse standard input portion (first 49 bytes)
    // Reuse the standard report parser by temporarily treating it as 0x30
    let standard = if data.len() >= 49 {
        let mut standard_data = data[..49].to_vec();
        standard_data[0] = REPORT_ID_STANDARD; // Temporarily set to 0x30 for parsing
        parse_standard_report(&standard_data)
    } else {
        // Short report — parse what we have
        let mut standard_data = data.to_vec();
        if !standard_data.is_empty() {
            standard_data[0] = REPORT_ID_STANDARD;
        }
        parse_standard_report(&standard_data)
    };

    let standard = standard?;

    // Parse NFC/IR payload (bytes 49+)
    let nfc_tag = crate::subcmd::parse_nfc_tag_data(data);
    let ir_frame = crate::subcmd::parse_ir_camera_data(data);

    Some(ParsedNfcReport {
        standard,
        nfc_tag,
        ir_frame,
    })
}

/// Convert the battery level nibble (bits 3-1 of the high nibble) to a percentage.
///
/// The Pro Controller reports only 5 discrete battery levels via the high
/// nibble of byte 1 (bits 7-5 = level, bit 4 = charging):
///
///   raw 0  → Empty     → 0%
///   raw 2  → Critical  → 10%
///   raw 4  → Low       → 25%
///   raw 6  → Medium    → 50%
///   raw 8  → Full      → 100%
///
/// The charging bit (bit 0 of the high nibble) does not affect the percentage.
/// The raw value passed here should be the full high nibble (including charging bit).
pub fn battery_raw_to_percent(raw: u8) -> u8 {
    // Strip the charging bit (bit 0) and extract the 3-bit level (0–4).
    let level = (raw >> 1) & 0x07;
    match level {
        0 => 0,   // empty
        1 => 10,  // critical (raw 2)
        2 => 25,  // low (raw 4)
        3 => 50,  // medium (raw 6)
        _ => 100, // full (raw 8+)
    }
}

pub fn build_rumble_report(left: u8, right: u8) -> Vec<u8> {
    let mut report = vec![0u8; 49];
    report[0] = 0x10;
    report[1] = 0x00;
    report[49 - 2] = if left > 0 { 0x20 } else { 0x00 };
    report[49 - 1] = if right > 0 { 0x20 } else { 0x00 };
    report
}

pub fn build_zero_rumble() -> Vec<u8> {
    let mut report = vec![0u8; 10];
    report[0] = 0x10;
    report[1] = 0x00;
    for byte in report.iter_mut().skip(2) {
        *byte = 0x00;
    }
    report
}

pub fn build_get_state_subcmd() -> Vec<u8> {
    // Subcommand 0x00 (Get Controller State) over Bluetooth output report 0x01.
    // Format: [0x01, counter, 8×rumble, subcmd_id, ...data]
    let mut report = vec![0u8; 11];
    report[0] = 0x01; // output report ID (subcommand)
    report[1] = 0x00; // packet counter
                      // bytes 2..9 = rumble (zeros)
    report[10] = 0x00; // subcommand 0x00 = Get Controller State
    report
}

/// Builds subcommand 0x03 (Set Input Report Mode) with mode 0x30 (standard).
/// This tells the Pro Controller to stream 0x30 standard input reports
/// whenever buttons/sticks change. Without this, the controller stays silent
/// over Bluetooth and eventually disconnects from inactivity.
pub fn build_set_report_mode_subcmd() -> Vec<u8> {
    let mut report = vec![0u8; 12];
    report[0] = 0x01; // output report ID (subcommand)
    report[1] = 0x01; // packet counter (increment from the get-state one)
                      // bytes 2..9 = rumble (zeros)
    report[10] = 0x03; // subcommand 0x03 = Set Input Report Mode
    report[11] = 0x30; // mode 0x30 = standard input reports
    report
}

pub fn hex_string(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a 0x3F "simple HID" report (the default Bluetooth report the Pro
/// Controller sends before receiving the set-report-mode subcommand).
///
/// Unlike 0x30, this report uses a standard HID gamepad layout:
/// - Byte 0: report ID (0x3F)
/// - Bytes 1-2: 2-byte button status (different bit packing than 0x30)
/// - Byte 3: hat switch (D-pad)
/// - Bytes 4-7: left stick as u16 X/Y (big-endian)
/// - Bytes 8-11: right stick as u16 X/Y (big-endian)
///
/// Button mapping (byte 1 / byte 2):
/// - Byte 1: bit0=A, bit1=B, bit2=X, bit3=Y, bit4=L, bit5=R, bit6=ZL, bit7=ZR
/// - Byte 2: bit0=Minus, bit1=Plus, bit2=Home, bit3=Capture, bit4=L-stick, bit5=R-stick
pub fn parse_simple_hid_report(data: &[u8]) -> Option<ParsedInput> {
    if data.len() < 12 {
        warn!("Simple HID report too short: {} bytes", data.len());
        return None;
    }

    let report_id = data[0];
    if report_id != REPORT_ID_DEFAULT_BT {
        debug!("Not a simple HID report (ID=0x{:02X})", report_id);
        return None;
    }

    let timer = data[1];

    // 0x3F reports do not carry battery info in byte 2 the same way 0x30 does;
    // we still attempt to parse it for consistency but it may be zero.
    let battery = if data.len() > 2 {
        parse_battery_info(data[2])
    } else {
        BatteryInfo::default()
    };

    let btn_byte1 = data[1];
    let btn_byte2 = data[2];

    // Byte 1 bits: bit0=A, bit1=B, bit2=X, bit3=Y, bit4=L, bit5=R, bit6=ZL, bit7=ZR
    // Byte 2 bits: bit0=Minus, bit1=Plus, bit2=Home, bit3=Capture, bit4=L-stick, bit5=R-stick
    let buttons = ButtonState {
        a: btn_byte1 & 0x01 != 0,
        b: btn_byte1 & 0x02 != 0,
        x: btn_byte1 & 0x04 != 0,
        y: btn_byte1 & 0x08 != 0,
        l: btn_byte1 & 0x10 != 0,
        r: btn_byte1 & 0x20 != 0,
        zl: btn_byte1 & 0x40 != 0,
        zr: btn_byte1 & 0x80 != 0,
        minus: btn_byte2 & 0x01 != 0,
        plus: btn_byte2 & 0x02 != 0,
        home: btn_byte2 & 0x04 != 0,
        capture: btn_byte2 & 0x08 != 0,
        stick_l: btn_byte2 & 0x10 != 0,
        stick_r: btn_byte2 & 0x20 != 0,
        // 0x3F reports do not expose SR/SL buttons — set them to false.
        sr_right: false,
        sl_right: false,
        sr_left: false,
        sl_left: false,
        // D-pad from the hat switch (byte 3). Standard HID hat values:
        // 0=up,1=up-right,2=right,3=down-right,4=down,5=down-left,6=left,7=up-left,8=neutral.
        dpad_up: matches!(data[3], 0 | 1 | 7),
        dpad_down: matches!(data[3], 3..=5),
        dpad_left: matches!(data[3], 5..=7),
        dpad_right: matches!(data[3], 1..=3),
    };

    // Left stick: u16 X/Y big-endian at bytes [4..8], center = 0x8000.
    let left_stick = parse_simple_stick(&data[4..8]);
    // Right stick: u16 X/Y big-endian at bytes [8..12], center = 0x8000.
    let right_stick = parse_simple_stick(&data[8..12]);

    // 0x3F reports do not include IMU data.
    Some(ParsedInput {
        buttons,
        left_stick,
        right_stick,
        timer,
        report_id,
        battery,
        imu: None,
        vibrator: 0,
    })
}

/// Parse a 16-bit big-endian stick pair (X, Y) with center 0x8000.
fn parse_simple_stick(data: &[u8]) -> StickState {
    if data.len() < 4 {
        return StickState::default();
    }
    let raw_x = u16::from_be_bytes([data[0], data[1]]);
    let raw_y = u16::from_be_bytes([data[2], data[3]]);
    let center: u16 = 0x8000;
    let x = normalize_stick_16(raw_x, center);
    let y = normalize_stick_16(raw_y, center);
    StickState { x, y, raw_x, raw_y }
}

/// Normalize a 16-bit stick value around `center` (0x8000) to the range -1..1.
fn normalize_stick_16(raw: u16, center: u16) -> f32 {
    if raw == center {
        return 0.0;
    }
    let max: u16 = 0xFFFF;
    if raw < center {
        let range = center as f32;
        if range > 0.0 {
            -((center - raw) as f32 / range)
        } else {
            0.0
        }
    } else {
        let range = (max - center) as f32;
        if range > 0.0 {
            (raw - center) as f32 / range
        } else {
            0.0
        }
    }
    .clamp(-1.0, 1.0)
}

/// Parse device info from a subcommand reply's data field.
pub fn parse_device_info_from_reply(reply: &SubcmdReply) -> Option<crate::state::DeviceInfo> {
    crate::subcmd::parse_device_info_reply(&reply.reply_data)
}

/// Parse stick calibration from a SPI flash read reply.
pub fn parse_stick_calibration_from_reply(
    reply: &SubcmdReply,
) -> Option<crate::state::StickCalibration> {
    crate::subcmd::parse_stick_calibration_reply(&reply.reply_data)
}

/// Normalize a raw stick value using calibration data.
///
/// Given a raw stick reading and the calibrated center/min/max for that axis,
/// returns a value in the range -1.0..=1.0 where 0.0 is the center. Values
/// below center are scaled by `(center - min)` and values above center by
/// `(max - center)`, matching the asymmetric calibration the Pro Controller
/// stores in SPI flash.
pub fn normalize_stick_calibrated(raw: u16, center: u16, min: u16, max: u16) -> f32 {
    if raw < center {
        let range = (center - min) as f32;
        if range > 0.0 {
            -((center - raw) as f32 / range)
        } else {
            0.0
        }
    } else {
        let range = (max - center) as f32;
        if range > 0.0 {
            (raw - center) as f32 / range
        } else {
            0.0
        }
    }
    .clamp(-1.0, 1.0)
}

#[cfg(test)]
mod nfc_tests {
    use super::*;

    #[test]
    fn parse_nfc_ir_report_valid() {
        // Build a 0x31 report with standard input + NFC data
        let mut data = vec![0u8; 65];
        data[0] = REPORT_ID_NFC_IR;
        data[1] = 0x42; // timer
        data[2] = 0x80; // battery
                        // NFC payload at byte 49+
        data[49] = 0x01; // NFC present
        data[50..57].copy_from_slice(&[0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]); // UID
        data[57] = 0x02; // tag type = Amiibo

        let parsed = parse_nfc_ir_report(&data);
        assert!(parsed.is_some());
        let report = parsed.unwrap();
        assert!(report.nfc_tag.is_some());
        let tag = report.nfc_tag.unwrap();
        assert!(tag.is_amiibo);
    }

    #[test]
    fn parse_nfc_ir_report_wrong_id() {
        let mut data = vec![0u8; 65];
        data[0] = 0x30; // Wrong report ID
        assert!(parse_nfc_ir_report(&data).is_none());
    }

    #[test]
    fn parse_nfc_ir_report_empty() {
        assert!(parse_nfc_ir_report(&[]).is_none());
    }

    #[test]
    fn parse_nfc_ir_report_no_nfc_data() {
        let mut data = vec![0u8; 49];
        data[0] = REPORT_ID_NFC_IR;
        data[1] = 0x42;
        data[2] = 0x80;

        let parsed = parse_nfc_ir_report(&data);
        assert!(parsed.is_some());
        let report = parsed.unwrap();
        assert!(report.nfc_tag.is_none());
        assert!(report.ir_frame.is_none());
    }
}
