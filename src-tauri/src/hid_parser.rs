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
mod tests {
    use super::*;

    /// Encode two 12-bit values into the 3-byte HID stick representation.
    fn encode_stick_12(raw_x: u16, raw_y: u16) -> [u8; 3] {
        let b0 = (raw_x & 0xFF) as u8;
        let b1 = ((raw_x >> 8) & 0x0F) as u8 | (((raw_y & 0x0F) << 4) as u8);
        let b2 = ((raw_y >> 4) & 0xFF) as u8;
        [b0, b1, b2]
    }

    /// Encode two 12-bit values into a 3-byte group (inverse of decode_12bit_pair).
    fn encode_12bit_pair(v1: u16, v2: u16) -> [u8; 3] {
        let b0 = (v1 & 0xFF) as u8;
        let b1 = ((v1 >> 8) & 0x0F) as u8 | (((v2 & 0x0F) << 4) as u8);
        let b2 = ((v2 >> 4) & 0xFF) as u8;
        [b0, b1, b2]
    }

    /// Build a 12-byte 0x3F (default Bluetooth) report.
    fn build_0x3f(btn1: u8, btn2: u8, hat: u8) -> Vec<u8> {
        let mut data = vec![0u8; 12];
        data[0] = REPORT_ID_DEFAULT_BT;
        data[1] = btn1;
        data[2] = btn2;
        data[3] = hat;
        // Left stick centred at 0x8000 (big-endian)
        data[4] = 0x80;
        data[5] = 0x00;
        data[6] = 0x80;
        data[7] = 0x00;
        // Right stick centred at 0x8000 (big-endian)
        data[8] = 0x80;
        data[9] = 0x00;
        data[10] = 0x80;
        data[11] = 0x00;
        data
    }

    // --- struct defaults --------------------------------------------------

    #[test]
    fn battery_info_default_is_zero() {
        let b = BatteryInfo::default();
        assert_eq!(b.raw, 0);
        assert!(!b.charging);
        assert_eq!(b.connection_type, 0);
    }

    #[test]
    fn imu_frame_default_is_zero() {
        let f = ImuFrame::default();
        assert_eq!(f.accel_x, 0);
        assert_eq!(f.accel_y, 0);
        assert_eq!(f.accel_z, 0);
        assert_eq!(f.gyro_x, 0);
        assert_eq!(f.gyro_y, 0);
        assert_eq!(f.gyro_z, 0);
    }

    #[test]
    fn imu_data_default_has_three_zero_frames() {
        let d = ImuData::default();
        assert_eq!(d.frames.len(), 3);
        for frame in &d.frames {
            assert_eq!(*frame, ImuFrame::default());
        }
    }

    #[test]
    fn parsed_input_fields_accessible() {
        let p = ParsedInput {
            buttons: ButtonState::default(),
            left_stick: StickState::default(),
            right_stick: StickState::default(),
            timer: 0x42,
            report_id: REPORT_ID_STANDARD,
            battery: BatteryInfo::default(),
            imu: None,
            vibrator: 0,
        };
        assert_eq!(p.timer, 0x42);
        assert_eq!(p.report_id, REPORT_ID_STANDARD);
        assert!(p.imu.is_none());
        assert_eq!(p.vibrator, 0);
    }

    // --- parse_battery_info -----------------------------------------------

    #[test]
    fn parse_battery_info_not_charging() {
        let b = parse_battery_info(0x80);
        assert_eq!(b.raw, 0x08);
        assert!(!b.charging);
        assert_eq!(b.connection_type, 0x00);
    }

    #[test]
    fn parse_battery_info_charging() {
        let b = parse_battery_info(0x91);
        assert_eq!(b.raw, 0x09);
        assert!(b.charging);
        assert_eq!(b.connection_type, 0x01);
    }

    #[test]
    fn parse_battery_info_connection_type_preserved() {
        let b = parse_battery_info(0x4A);
        assert_eq!(b.raw, 0x04);
        assert!(!b.charging);
        assert_eq!(b.connection_type, 0x0A);
    }

    #[test]
    fn parse_battery_info_zero() {
        let b = parse_battery_info(0x00);
        assert_eq!(b.raw, 0);
        assert!(!b.charging);
        assert_eq!(b.connection_type, 0);
    }

    #[test]
    fn parse_battery_info_max_nibbles() {
        let b = parse_battery_info(0xFF);
        assert_eq!(b.raw, 0x0F);
        assert!(b.charging);
        assert_eq!(b.connection_type, 0x0F);
    }

    // --- normalize_stick (12-bit) -----------------------------------------

    #[test]
    fn normalize_stick_center_is_zero() {
        assert!((normalize_stick(STICK_CENTER) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_stick_max_is_one() {
        assert!((normalize_stick(STICK_MAX) - 1.0).abs() < 0.001);
    }

    #[test]
    fn normalize_stick_min_is_neg_one() {
        assert!((normalize_stick(STICK_MIN) - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn normalize_stick_midpoint_positive() {
        let raw = (STICK_CENTER + STICK_MAX) / 2;
        let result = normalize_stick(raw);
        assert!(result > 0.0 && result < 1.0);
    }

    #[test]
    fn normalize_stick_midpoint_negative() {
        let raw = (STICK_CENTER + STICK_MIN) / 2;
        let result = normalize_stick(raw);
        assert!(result < 0.0 && result > -1.0);
    }

    // --- parse_stick (12-bit, 3-byte) -------------------------------------

    #[test]
    fn parse_stick_short_data_returns_default() {
        let result = parse_stick(&[0x00, 0x08]);
        assert_eq!(result, StickState::default());
    }

    #[test]
    fn parse_stick_center() {
        let data = encode_stick_12(STICK_CENTER, STICK_CENTER);
        let result = parse_stick(&data);
        assert_eq!(result.raw_x, STICK_CENTER);
        assert_eq!(result.raw_y, STICK_CENTER);
        assert!((result.x - 0.0).abs() < f32::EPSILON);
        assert!((result.y - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_stick_max_values() {
        let data = encode_stick_12(STICK_MAX, STICK_MAX);
        let result = parse_stick(&data);
        assert_eq!(result.raw_x, STICK_MAX);
        assert_eq!(result.raw_y, STICK_MAX);
        assert!((result.x - 1.0).abs() < 0.001);
        assert!((result.y - 1.0).abs() < 0.001);
    }

    #[test]
    fn parse_stick_min_values() {
        let data = encode_stick_12(STICK_MIN, STICK_MIN);
        let result = parse_stick(&data);
        assert_eq!(result.raw_x, STICK_MIN);
        assert_eq!(result.raw_y, STICK_MIN);
        assert!((result.x - (-1.0)).abs() < 0.001);
        assert!((result.y - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn parse_stick_asymmetric_values() {
        let data = encode_stick_0x3A5_0x6C2();
        let result = parse_stick(&data);
        assert_eq!(result.raw_x, 0x3A5);
        assert_eq!(result.raw_y, 0x6C2);
    }

    fn encode_stick_0x3A5_0x6C2() -> [u8; 3] {
        encode_stick_12(0x3A5, 0x6C2)
    }

    // --- parse_imu --------------------------------------------------------

    #[test]
    fn parse_imu_three_frames() {
        let mut data = vec![0u8; 36];
        // Frame 0
        data[0..2].copy_from_slice(&100i16.to_le_bytes());
        data[2..4].copy_from_slice(&200i16.to_le_bytes());
        data[4..6].copy_from_slice(&300i16.to_le_bytes());
        data[6..8].copy_from_slice(&400i16.to_le_bytes());
        data[8..10].copy_from_slice(&500i16.to_le_bytes());
        data[10..12].copy_from_slice(&600i16.to_le_bytes());
        // Frame 1
        data[12..14].copy_from_slice(&1i16.to_le_bytes());
        data[14..16].copy_from_slice(&1i16.to_le_bytes());
        data[16..18].copy_from_slice(&1i16.to_le_bytes());
        data[18..20].copy_from_slice(&1i16.to_le_bytes());
        data[20..22].copy_from_slice(&1i16.to_le_bytes());
        data[22..24].copy_from_slice(&1i16.to_le_bytes());
        // Frame 2 (negative)
        data[24..26].copy_from_slice(&(-100i16).to_le_bytes());
        data[26..28].copy_from_slice(&(-200i16).to_le_bytes());
        data[28..30].copy_from_slice(&(-300i16).to_le_bytes());
        data[30..32].copy_from_slice(&(-400i16).to_le_bytes());
        data[32..34].copy_from_slice(&(-500i16).to_le_bytes());
        data[34..36].copy_from_slice(&(-600i16).to_le_bytes());

        let imu = parse_imu(&data);
        assert_eq!(imu.frames[0].accel_x, 100);
        assert_eq!(imu.frames[0].accel_y, 200);
        assert_eq!(imu.frames[0].accel_z, 300);
        assert_eq!(imu.frames[0].gyro_x, 400);
        assert_eq!(imu.frames[0].gyro_y, 500);
        assert_eq!(imu.frames[0].gyro_z, 600);

        assert_eq!(imu.frames[1].accel_x, 1);
        assert_eq!(imu.frames[1].gyro_z, 1);

        assert_eq!(imu.frames[2].accel_x, -100);
        assert_eq!(imu.frames[2].accel_y, -200);
        assert_eq!(imu.frames[2].gyro_z, -600);
    }

    #[test]
    fn parse_imu_short_data_keeps_defaults() {
        let mut data = vec![0u8; 12];
        data[0..2].copy_from_slice(&42i16.to_le_bytes());
        let imu = parse_imu(&data);
        assert_eq!(imu.frames[0].accel_x, 42);
        assert_eq!(imu.frames[1], ImuFrame::default());
        assert_eq!(imu.frames[2], ImuFrame::default());
    }

    #[test]
    fn parse_imu_empty_data() {
        let imu = parse_imu(&[]);
        assert_eq!(imu.frames[0], ImuFrame::default());
        assert_eq!(imu.frames[1], ImuFrame::default());
        assert_eq!(imu.frames[2], ImuFrame::default());
    }

    // --- parse_standard_report with IMU / vibrator / battery --------------

    #[test]
    fn parse_standard_report_with_imu() {
        let mut data = vec![0u8; 49];
        data[0] = REPORT_ID_STANDARD;
        data[1] = 0x42;
        data[2] = 0x80;
        data[13] = 0x64; // accel_x low byte = 100
        data[14] = 0x00;

        let parsed = parse_standard_report(&data).expect("should parse");
        assert!(parsed.imu.is_some(), "0x30 with 49 bytes should have IMU");
        let imu = parsed.imu.unwrap();
        assert_eq!(imu.frames[0].accel_x, 100);
    }

    #[test]
    fn parse_standard_report_no_imu_when_short() {
        let mut data = vec![0u8; 12];
        data[0] = REPORT_ID_STANDARD;
        let parsed = parse_standard_report(&data).expect("should parse");
        assert!(parsed.imu.is_none(), "short 0x30 should have no IMU");
    }

    #[test]
    fn parse_standard_report_vibrator_byte() {
        let mut data = vec![0u8; 13];
        data[0] = REPORT_ID_STANDARD;
        data[12] = 0x55;
        let parsed = parse_standard_report(&data).expect("should parse");
        assert_eq!(parsed.vibrator, 0x55);
    }

    #[test]
    fn parse_standard_report_vibrator_default_when_missing() {
        let mut data = vec![0u8; 12];
        data[0] = REPORT_ID_STANDARD;
        let parsed = parse_standard_report(&data).expect("should parse");
        assert_eq!(parsed.vibrator, 0);
    }

    #[test]
    fn parse_standard_report_battery_parsed() {
        let mut data = vec![0u8; 12];
        data[0] = REPORT_ID_STANDARD;
        data[2] = 0x91;
        let parsed = parse_standard_report(&data).expect("should parse");
        assert_eq!(parsed.battery.raw, 0x09);
        assert!(parsed.battery.charging);
        assert_eq!(parsed.battery.connection_type, 0x01);
    }

    // --- parse_subcmd_reply: ack_data_type & reply_data -------------------

    #[test]
    fn parse_subcmd_reply_ack_data_type_simple() {
        let mut data = vec![0u8; 15];
        data[0] = REPORT_ID_SUBCMD_REPLY;
        data[13] = 0x80;
        data[14] = 0x02;
        let reply = parse_subcmd_reply(&data).expect("should parse");
        assert_eq!(reply.ack, 0x80);
        assert_eq!(reply.ack_data_type, 0x00);
    }

    #[test]
    fn parse_subcmd_reply_ack_data_type_nonzero() {
        let mut data = vec![0u8; 15];
        data[0] = REPORT_ID_SUBCMD_REPLY;
        data[13] = 0x85;
        data[14] = 0x10;
        let reply = parse_subcmd_reply(&data).expect("should parse");
        assert_eq!(reply.ack, 0x85);
        assert_eq!(reply.ack_data_type, 0x05);
        assert_eq!(reply.subcmd_id, 0x10);
    }

    #[test]
    fn parse_subcmd_reply_with_data() {
        let mut data = vec![0u8; 18];
        data[0] = REPORT_ID_SUBCMD_REPLY;
        data[13] = 0x80;
        data[14] = 0x02;
        data[15] = 0xAA;
        data[16] = 0xBB;
        data[17] = 0xCC;
        let reply = parse_subcmd_reply(&data).expect("should parse");
        assert_eq!(reply.reply_data, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn parse_subcmd_reply_empty_data() {
        let mut data = vec![0u8; 15];
        data[0] = REPORT_ID_SUBCMD_REPLY;
        data[13] = 0x80;
        data[14] = 0x02;
        let reply = parse_subcmd_reply(&data).expect("should parse");
        assert!(reply.reply_data.is_empty());
    }

    // --- build_rumble_report / build_zero_rumble --------------------------

    #[test]
    fn build_rumble_report_asymmetric_left_off() {
        let report = build_rumble_report(0, 1);
        assert_eq!(report.len(), 49);
        assert_eq!(report[47], 0x00, "left off -> 0x00");
        assert_eq!(report[48], 0x20, "right on -> 0x20");
    }

    #[test]
    fn build_rumble_report_asymmetric_right_off() {
        let report = build_rumble_report(1, 0);
        assert_eq!(report[47], 0x20, "left on -> 0x20");
        assert_eq!(report[48], 0x00, "right off -> 0x00");
    }

    #[test]
    fn build_zero_rumble_all_zeros() {
        let report = build_zero_rumble();
        assert_eq!(report.len(), 10);
        for (i, &b) in report.iter().enumerate() {
            if i < 2 {
                continue;
            }
            assert_eq!(b, 0, "byte {} should be zero", i);
        }
    }

    // --- build_get_state_subcmd / build_set_report_mode_subcmd ------------

    #[test]
    fn build_get_state_subcmd_format() {
        let report = build_get_state_subcmd();
        assert_eq!(report.len(), 11);
        assert_eq!(report[0], 0x01, "output report ID");
        assert_eq!(report[1], 0x00, "packet counter");
        assert_eq!(report[10], 0x00, "subcmd 0x00 = get state");
        for i in 2..10 {
            assert_eq!(report[i], 0, "rumble byte {} should be zero", i);
        }
    }

    #[test]
    fn build_set_report_mode_subcmd_format() {
        let report = build_set_report_mode_subcmd();
        assert_eq!(report.len(), 12);
        assert_eq!(report[0], 0x01, "output report ID");
        assert_eq!(report[1], 0x01, "packet counter");
        assert_eq!(report[10], 0x03, "subcmd 0x03 = set report mode");
        assert_eq!(report[11], 0x30, "mode 0x30 = standard");
    }

    // --- parse_device_info_from_reply -------------------------------------

    #[test]
    fn parse_device_info_from_reply_valid() {
        let mut reply_data = vec![0u8; 12];
        reply_data[0] = 0x01; // fw major
        reply_data[1] = 0x02; // fw minor
        reply_data[2] = 0x03; // controller type
        reply_data[4] = 0xAA; // MAC
        reply_data[5] = 0xBB;
        reply_data[6] = 0xCC;
        reply_data[7] = 0xDD;
        reply_data[8] = 0xEE;
        reply_data[9] = 0xFF;
        reply_data[11] = 0x01; // colors_from_spi = true

        let reply = SubcmdReply {
            battery: BatteryInfo::default(),
            buttons: ButtonState::default(),
            ack: 0x80,
            ack_data_type: 0,
            subcmd_id: 0x02,
            reply_data,
        };

        let info = parse_device_info_from_reply(&reply);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.firmware_version, "1.2");
        assert_eq!(info.controller_type, 0x03);
        assert_eq!(info.mac_address, "AA:BB:CC:DD:EE:FF");
        assert!(info.colors_from_spi);
    }

    #[test]
    fn parse_device_info_from_reply_colors_false() {
        let mut reply_data = vec![0u8; 12];
        reply_data[0] = 0x00;
        reply_data[1] = 0x01;
        reply_data[2] = 0x01;
        reply_data[11] = 0x00; // colors_from_spi = false

        let reply = SubcmdReply {
            battery: BatteryInfo::default(),
            buttons: ButtonState::default(),
            ack: 0x80,
            ack_data_type: 0,
            subcmd_id: 0x02,
            reply_data,
        };

        let info = parse_device_info_from_reply(&reply).unwrap();
        assert!(!info.colors_from_spi);
        assert_eq!(info.firmware_version, "0.1");
    }

    #[test]
    fn parse_device_info_from_reply_short_data() {
        let reply = SubcmdReply {
            battery: BatteryInfo::default(),
            buttons: ButtonState::default(),
            ack: 0x80,
            ack_data_type: 0,
            subcmd_id: 0x02,
            reply_data: vec![0x00, 0x01, 0x02],
        };
        assert!(parse_device_info_from_reply(&reply).is_none());
    }

    // --- parse_stick_calibration_from_reply -------------------------------

    #[test]
    fn parse_stick_calibration_from_reply_valid() {
        let center = 0x800u16;
        let min_below = 0x300u16;
        let max_above = 0x300u16;

        let mut data = vec![0u8; 18];
        // Left: [max_above, center, min_below]
        data[0..3].copy_from_slice(&encode_12bit_pair(max_above, max_above));
        data[3..6].copy_from_slice(&encode_12bit_pair(center, center));
        data[6..9].copy_from_slice(&encode_12bit_pair(min_below, min_below));
        // Right: [center, min_below, max_above]
        data[9..12].copy_from_slice(&encode_12bit_pair(center, center));
        data[12..15].copy_from_slice(&encode_12bit_pair(min_below, min_below));
        data[15..18].copy_from_slice(&encode_12bit_pair(max_above, max_above));

        let reply = SubcmdReply {
            battery: BatteryInfo::default(),
            buttons: ButtonState::default(),
            ack: 0x80,
            ack_data_type: 0,
            subcmd_id: 0x10,
            reply_data: data,
        };

        let cal = parse_stick_calibration_from_reply(&reply);
        assert!(cal.is_some());
        let cal = cal.unwrap();
        assert!(cal.valid);
        assert_eq!(cal.left_center_x, center);
        assert_eq!(cal.left_center_y, center);
        assert_eq!(cal.left_min_x, center - min_below);
        assert_eq!(cal.left_max_x, center + max_above);
        assert_eq!(cal.right_center_x, center);
        assert_eq!(cal.right_min_y, center - min_below);
        assert_eq!(cal.right_max_y, center + max_above);
    }

    #[test]
    fn parse_stick_calibration_from_reply_short_data() {
        let reply = SubcmdReply {
            battery: BatteryInfo::default(),
            buttons: ButtonState::default(),
            ack: 0x80,
            ack_data_type: 0,
            subcmd_id: 0x10,
            reply_data: vec![0u8; 10],
        };
        assert!(parse_stick_calibration_from_reply(&reply).is_none());
    }

    // --- normalize_stick_calibrated (hid_parser version) ------------------

    #[test]
    fn normalize_stick_calibrated_center_returns_zero() {
        let result = normalize_stick_calibrated(0x800, 0x800, 0x500, 0xB00);
        assert!((result - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_stick_calibrated_max_returns_one() {
        let result = normalize_stick_calibrated(0xB00, 0x800, 0x500, 0xB00);
        assert!((result - 1.0).abs() < 0.001);
    }

    #[test]
    fn normalize_stick_calibrated_min_returns_neg_one() {
        let result = normalize_stick_calibrated(0x500, 0x800, 0x500, 0xB00);
        assert!((result - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn normalize_stick_calibrated_above_max_clamps() {
        let result = normalize_stick_calibrated(0xFFF, 0x800, 0x500, 0xB00);
        assert!((result - 1.0).abs() < 0.001, "should clamp to 1.0, got {}", result);
    }

    #[test]
    fn normalize_stick_calibrated_below_min_clamps() {
        let result = normalize_stick_calibrated(0x000, 0x800, 0x500, 0xB00);
        assert!(
            (result - (-1.0)).abs() < 0.001,
            "should clamp to -1.0, got {}",
            result
        );
    }

    #[test]
    fn normalize_stick_calibrated_zero_range_above() {
        let result = normalize_stick_calibrated(0x900, 0x800, 0x500, 0x800);
        assert!((result - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_stick_calibrated_zero_range_below() {
        let result = normalize_stick_calibrated(0x700, 0x800, 0x800, 0xB00);
        assert!((result - 0.0).abs() < f32::EPSILON);
    }

    // --- parse_stick_calibrated -------------------------------------------

    #[test]
    fn parse_stick_calibrated_valid_calibration() {
        let data = encode_stick_12(0xB00, 0x500);
        let result = parse_stick_calibrated(&data, 0x800, 0x500, 0xB00, 0x800, 0x500, 0xB00, true);
        assert_eq!(result.raw_x, 0xB00);
        assert_eq!(result.raw_y, 0x500);
        assert!((result.x - 1.0).abs() < 0.001);
        assert!((result.y - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn parse_stick_calibrated_fallback_when_invalid() {
        let data = encode_stick_12(STICK_MAX, STICK_MIN);
        let result = parse_stick_calibrated(&data, 0, 0, 0, 0, 0, 0, false);
        assert_eq!(result.raw_x, STICK_MAX);
        assert_eq!(result.raw_y, STICK_MIN);
        assert!((result.x - 1.0).abs() < 0.001);
        assert!((result.y - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn parse_stick_calibrated_short_data() {
        let result = parse_stick_calibrated(
            &[0x00, 0x08],
            0x800,
            0x500,
            0xB00,
            0x800,
            0x500,
            0xB00,
            true,
        );
        assert_eq!(result, StickState::default());
    }

    // --- parse_simple_hid_report (0x3F) -----------------------------------

    #[test]
    fn parse_simple_hid_report_wrong_id() {
        let mut data = vec![0u8; 12];
        data[0] = 0x30;
        assert!(parse_simple_hid_report(&data).is_none());
    }

    #[test]
    fn parse_simple_hid_report_too_short() {
        let data = vec![0x3F, 0x00, 0x00];
        assert!(parse_simple_hid_report(&data).is_none());
    }

    #[test]
    fn parse_simple_hid_report_dpad_up() {
        let data = build_0x3f(0, 0, 0);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.dpad_up);
        assert!(!parsed.buttons.dpad_down);
        assert!(!parsed.buttons.dpad_left);
        assert!(!parsed.buttons.dpad_right);
    }

    #[test]
    fn parse_simple_hid_report_dpad_right() {
        let data = build_0x3f(0, 0, 2);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.dpad_right);
        assert!(!parsed.buttons.dpad_left);
    }

    #[test]
    fn parse_simple_hid_report_dpad_down() {
        let data = build_0x3f(0, 0, 4);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.dpad_down);
        assert!(!parsed.buttons.dpad_up);
    }

    #[test]
    fn parse_simple_hid_report_dpad_left() {
        let data = build_0x3f(0, 0, 6);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.dpad_left);
        assert!(!parsed.buttons.dpad_right);
    }

    #[test]
    fn parse_simple_hid_report_dpad_neutral() {
        let data = build_0x3f(0, 0, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(!parsed.buttons.dpad_up);
        assert!(!parsed.buttons.dpad_down);
        assert!(!parsed.buttons.dpad_left);
        assert!(!parsed.buttons.dpad_right);
    }

    #[test]
    fn parse_simple_hid_report_dpad_diagonal_up_right() {
        let data = build_0x3f(0, 0, 1);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.dpad_up);
        assert!(parsed.buttons.dpad_right);
        assert!(!parsed.buttons.dpad_down);
        assert!(!parsed.buttons.dpad_left);
    }

    #[test]
    fn parse_simple_hid_report_dpad_diagonal_down_left() {
        let data = build_0x3f(0, 0, 5);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.dpad_down);
        assert!(parsed.buttons.dpad_left);
        assert!(!parsed.buttons.dpad_up);
        assert!(!parsed.buttons.dpad_right);
    }

    #[test]
    fn parse_simple_hid_report_all_face_buttons() {
        let data = build_0x3f(0x0F, 0, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.a);
        assert!(parsed.buttons.b);
        assert!(parsed.buttons.x);
        assert!(parsed.buttons.y);
    }

    #[test]
    fn parse_simple_hid_report_shoulder_buttons() {
        let data = build_0x3f(0xF0, 0, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.l);
        assert!(parsed.buttons.r);
        assert!(parsed.buttons.zl);
        assert!(parsed.buttons.zr);
    }

    #[test]
    fn parse_simple_hid_report_shared_buttons() {
        let data = build_0x3f(0, 0x3F, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.buttons.minus);
        assert!(parsed.buttons.plus);
        assert!(parsed.buttons.home);
        assert!(parsed.buttons.capture);
        assert!(parsed.buttons.stick_l);
        assert!(parsed.buttons.stick_r);
    }

    #[test]
    fn parse_simple_hid_report_sr_sl_always_false() {
        let data = build_0x3f(0xFF, 0xFF, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(!parsed.buttons.sr_right);
        assert!(!parsed.buttons.sl_right);
        assert!(!parsed.buttons.sr_left);
        assert!(!parsed.buttons.sl_left);
    }

    #[test]
    fn parse_simple_hid_report_stick_center() {
        let data = build_0x3f(0, 0, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert_eq!(parsed.left_stick.raw_x, 0x8000);
        assert_eq!(parsed.left_stick.raw_y, 0x8000);
        assert!((parsed.left_stick.x - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_simple_hid_report_no_imu() {
        let data = build_0x3f(0, 0, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert!(parsed.imu.is_none());
    }

    #[test]
    fn parse_simple_hid_report_vibrator_zero() {
        let data = build_0x3f(0, 0, 8);
        let parsed = parse_simple_hid_report(&data).expect("should parse");
        assert_eq!(parsed.vibrator, 0);
    }

    // --- normalize_stick_16 -----------------------------------------------

    #[test]
    fn normalize_stick_16_center() {
        assert!((normalize_stick_16(0x8000, 0x8000) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_stick_16_max() {
        assert!((normalize_stick_16(0xFFFF, 0x8000) - 1.0).abs() < 0.001);
    }

    #[test]
    fn normalize_stick_16_min() {
        assert!((normalize_stick_16(0x0000, 0x8000) - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn normalize_stick_16_midpoint_positive() {
        let raw = 0xC000u16;
        let result = normalize_stick_16(raw, 0x8000);
        assert!(result > 0.0 && result < 1.0);
    }

    #[test]
    fn normalize_stick_16_midpoint_negative() {
        let raw = 0x4000u16;
        let result = normalize_stick_16(raw, 0x8000);
        assert!(result < 0.0 && result > -1.0);
    }

    // --- parse_simple_stick -----------------------------------------------

    #[test]
    fn parse_simple_stick_short_data() {
        let result = parse_simple_stick(&[0x80, 0x00]);
        assert_eq!(result, StickState::default());
    }

    #[test]
    fn parse_simple_stick_center() {
        let data = [0x80, 0x00, 0x80, 0x00];
        let result = parse_simple_stick(&data);
        assert_eq!(result.raw_x, 0x8000);
        assert_eq!(result.raw_y, 0x8000);
        assert!((result.x - 0.0).abs() < f32::EPSILON);
        assert!((result.y - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_simple_stick_max() {
        let data = [0xFF, 0xFF, 0xFF, 0xFF];
        let result = parse_simple_stick(&data);
        assert_eq!(result.raw_x, 0xFFFF);
        assert_eq!(result.raw_y, 0xFFFF);
        assert!((result.x - 1.0).abs() < 0.001);
        assert!((result.y - 1.0).abs() < 0.001);
    }

    // --- constants --------------------------------------------------------

    #[test]
    fn stick_constants_values() {
        assert_eq!(STICK_CENTER, 0x800);
        assert_eq!(STICK_MAX, 0xFFF);
        assert_eq!(STICK_MIN, 0x000);
        assert!(STICK_MIN < STICK_CENTER);
        assert!(STICK_CENTER < STICK_MAX);
    }

    #[test]
    fn report_id_constants() {
        assert_eq!(REPORT_ID_STANDARD, 0x30);
        assert_eq!(REPORT_ID_SUBCMD_REPLY, 0x21);
        assert_eq!(REPORT_ID_NFC_IR, 0x31);
        assert_eq!(REPORT_ID_DEFAULT_BT, 0x3F);
        assert_eq!(REPORT_ID_USB_REPLY, 0x81);
    }

    #[test]
    fn vid_pid_constants() {
        assert_eq!(NINTENDO_VID, 0x057E);
        assert_eq!(PRO_CONTROLLER_PID, 0x2009);
    }

    // --- serde roundtrips -------------------------------------------------

    #[test]
    fn imu_frame_serde_roundtrip() {
        let frame = ImuFrame {
            accel_x: 100,
            accel_y: -200,
            accel_z: 300,
            gyro_x: -400,
            gyro_y: 500,
            gyro_z: -600,
        };
        let json = serde_json::to_string(&frame).expect("serialize");
        let deserialized: ImuFrame = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(frame, deserialized);
    }

    #[test]
    fn imu_data_serde_roundtrip() {
        let data = ImuData {
            frames: [
                ImuFrame {
                    accel_x: 1,
                    accel_y: 2,
                    accel_z: 3,
                    gyro_x: 4,
                    gyro_y: 5,
                    gyro_z: 6,
                },
                ImuFrame::default(),
                ImuFrame {
                    accel_x: -1,
                    accel_y: -2,
                    accel_z: -3,
                    gyro_x: -4,
                    gyro_y: -5,
                    gyro_z: -6,
                },
            ],
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deserialized: ImuData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deserialized);
    }

    #[test]
    fn imu_frame_serde_uses_snake_case() {
        let frame = ImuFrame {
            accel_x: 1,
            accel_y: 0,
            accel_z: 0,
            gyro_x: 0,
            gyro_y: 0,
            gyro_z: 0,
        };
        let json = serde_json::to_string(&frame).expect("serialize");
        assert!(json.contains("accel_x"));
        assert!(!json.contains("accelX"));
    }

    #[test]
    fn battery_info_equality() {
        let a = BatteryInfo {
            raw: 4,
            charging: false,
            connection_type: 1,
        };
        let b = BatteryInfo {
            raw: 4,
            charging: false,
            connection_type: 1,
        };
        assert_eq!(a, b);
        let c = BatteryInfo {
            raw: 5,
            charging: false,
            connection_type: 1,
        };
        assert_ne!(a, c);
    }
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
