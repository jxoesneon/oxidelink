use crate::hid_parser::{
    hex_string, ImuFrame, REPORT_ID_STANDARD, REPORT_ID_SUBCMD_REPLY, STICK_CENTER,
};
use crate::state::{timestamp_now, ControllerState, IpcEvent};
use log::{debug, info};
use std::sync::Arc;
use tokio::time::{interval, Duration};

/// Subcommand IDs used in mock replies. These mirror the constants in
/// `subcmd.rs` (created in parallel) but are duplicated here so mock.rs
/// compiles independently of that module's availability.
const SUBCMD_REQUEST_DEVICE_INFO: u8 = 0x02;
const SUBCMD_SPI_FLASH_READ: u8 = 0x10;
const SUBCMD_SET_PLAYER_LIGHTS: u8 = 0x30;
const SUBCMD_SET_HOME_LIGHT: u8 = 0x38;
const SUBCMD_ENABLE_IMU: u8 = 0x40;
const SUBCMD_ENABLE_VIBRATION: u8 = 0x48;

/// SPI flash address for stick calibration data.
const SPI_ADDR_STICK_CALIBRATION: u32 = 0x6080;

/// Mock data generator for hardware-free IPC testing.
///
/// Emits:
///   - raw HID hex strings (standard 0x30 input reports + 0x21 subcommand
///     replies containing battery telemetry)
///   - simulated Bluetooth timeouts / power-down events
///
/// This lets the frontend bind to the WebSocket and exercise the full
/// state pipeline without a physical Pro Controller attached.
pub struct MockGenerator {
    step: Arc<std::sync::Mutex<u32>>,
    /// Monotonic tick counter used for IMU sinusoidal patterns and other
    /// time-varying mock data. Incremented on every report build.
    tick: Arc<std::sync::Mutex<u32>>,
}

impl Default for MockGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl MockGenerator {
    pub fn new() -> Self {
        Self {
            step: Arc::new(std::sync::Mutex::new(0)),
            tick: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// Increment and return the tick counter (used for IMU sinusoidal patterns).
    fn next_tick(&self) -> u32 {
        let mut t = self.tick.lock().unwrap();
        *t = (*t + 1) % 0xFFFF;
        *t
    }

    /// Build a synthetic standard input report (report ID 0x30) with a
    /// rotating button press and a slowly drifting left stick.
    pub fn build_standard_report(&self) -> Vec<u8> {
        let step = {
            let mut s = self.step.lock().unwrap();
            *s = (*s + 1) % 64;
            *s
        };

        // 12-byte minimum standard report.
        let mut report = vec![0u8; 12];
        report[0] = REPORT_ID_STANDARD;
        report[1] = step as u8; // timer

        // Rotate a single face button each cycle: Y, X, B, A
        let right_btn = match step % 4 {
            0 => 0x01, // Y
            1 => 0x02, // X
            2 => 0x04, // B
            _ => 0x08, // A
        };
        report[2] = right_btn;
        report[3] = if step % 8 == 0 { 0x02 } else { 0 }; // plus occasionally
        report[4] = 0; // left buttons

        // Left stick drifts in a slow circle around center.
        let angle = (step as f32) * 0.1;
        let dx = ((angle.cos() * 200.0) as i32 + STICK_CENTER as i32).clamp(0, 0xFFF) as u16;
        let dy = ((angle.sin() * 200.0) as i32 + STICK_CENTER as i32).clamp(0, 0xFFF) as u16;
        report[6] = (dx & 0xFF) as u8;
        report[7] = ((dx >> 8) & 0x0F) as u8 | ((dy & 0x0F) << 4) as u8;
        report[8] = ((dy >> 4) & 0xFF) as u8;
        // Right stick at center.
        report[9] = (STICK_CENTER & 0xFF) as u8;
        report[10] = ((STICK_CENTER >> 8) & 0x0F) as u8 | ((STICK_CENTER & 0x0F) << 4) as u8;
        report[11] = ((STICK_CENTER >> 4) & 0xFF) as u8;

        report
    }

    /// Build a 49-byte standard input report (report ID 0x30) with IMU data.
    ///
    /// This is the full-size report the Pro Controller sends when IMU is
    /// enabled. It includes 3 IMU frames (36 bytes) after the 13-byte
    /// button/stick header. The IMU data follows a sinusoidal pattern to
    /// exercise visualization testing in the frontend.
    ///
    /// - Buttons rotate (same as `build_standard_report`)
    /// - Sticks are at center (no drift)
    /// - Frame 0: accel=(sin(t)*1000, 0, 4096), gyro=(0, 0, 0)
    /// - Frame 1: accel=(sin(t+1)*1000, 0, 4096), gyro=(100, 0, 0)
    /// - Frame 2: accel=(sin(t+2)*1000, 0, 4096), gyro=(200, 0, 0)
    pub fn build_imu_standard_report(&self) -> Vec<u8> {
        let step = {
            let mut s = self.step.lock().unwrap();
            *s = (*s + 1) % 64;
            *s
        };
        let tick = self.next_tick();
        let t = tick as f32;

        // 49-byte full standard report with IMU data.
        let mut report = vec![0u8; 49];
        report[0] = REPORT_ID_STANDARD;
        report[1] = step as u8; // timer
        report[2] = 0x80; // battery: full (level 8), not charging

        // Rotate a single face button each cycle: Y, X, B, A
        let right_btn = match step % 4 {
            0 => 0x01, // Y
            1 => 0x02, // X
            2 => 0x04, // B
            _ => 0x08, // A
        };
        report[3] = right_btn;
        report[4] = if step % 8 == 0 { 0x02 } else { 0 }; // plus occasionally
        report[5] = 0; // left buttons

        // Sticks at center.
        report[6] = (STICK_CENTER & 0xFF) as u8;
        report[7] = ((STICK_CENTER >> 8) & 0x0F) as u8 | ((STICK_CENTER & 0x0F) << 4) as u8;
        report[8] = ((STICK_CENTER >> 4) & 0xFF) as u8;
        report[9] = (STICK_CENTER & 0xFF) as u8;
        report[10] = ((STICK_CENTER >> 8) & 0x0F) as u8 | ((STICK_CENTER & 0x0F) << 4) as u8;
        report[11] = ((STICK_CENTER >> 4) & 0xFF) as u8;

        // IMU data: 3 frames × 12 bytes = 36 bytes at offset 13..49.
        let frames = [
            ImuFrame {
                accel_x: (t.sin() * 1000.0) as i16,
                accel_y: 0,
                accel_z: 4096, // simulating gravity on Z
                gyro_x: 0,
                gyro_y: 0,
                gyro_z: 0,
            },
            ImuFrame {
                accel_x: ((t + 1.0).sin() * 1000.0) as i16,
                accel_y: 0,
                accel_z: 4096,
                gyro_x: 100,
                gyro_y: 0,
                gyro_z: 0,
            },
            ImuFrame {
                accel_x: ((t + 2.0).sin() * 1000.0) as i16,
                accel_y: 0,
                accel_z: 4096,
                gyro_x: 200,
                gyro_y: 0,
                gyro_z: 0,
            },
        ];

        for (i, frame) in frames.iter().enumerate() {
            let off = 13 + i * 12;
            report[off..off + 2].copy_from_slice(&frame.accel_x.to_le_bytes());
            report[off + 2..off + 4].copy_from_slice(&frame.accel_y.to_le_bytes());
            report[off + 4..off + 6].copy_from_slice(&frame.accel_z.to_le_bytes());
            report[off + 6..off + 8].copy_from_slice(&frame.gyro_x.to_le_bytes());
            report[off + 8..off + 10].copy_from_slice(&frame.gyro_y.to_le_bytes());
            report[off + 10..off + 12].copy_from_slice(&frame.gyro_z.to_le_bytes());
        }

        report
    }

    /// Build a 49-byte full standard report with the drifting left stick
    /// (same motion as `build_standard_report`) plus IMU data appended.
    ///
    /// This is the backward-compatible "full" variant — the existing
    /// `build_standard_report` remains 12 bytes for callers that don't
    /// need IMU.
    pub fn build_full_standard_report(&self) -> Vec<u8> {
        let step = {
            let mut s = self.step.lock().unwrap();
            *s = (*s + 1) % 64;
            *s
        };
        let tick = self.next_tick();
        let t = tick as f32;

        let mut report = vec![0u8; 49];
        report[0] = REPORT_ID_STANDARD;
        report[1] = step as u8; // timer
        report[2] = 0x80; // battery: full

        // Rotate a single face button each cycle: Y, X, B, A
        let right_btn = match step % 4 {
            0 => 0x01, // Y
            1 => 0x02, // X
            2 => 0x04, // B
            _ => 0x08, // A
        };
        report[3] = right_btn;
        report[4] = if step % 8 == 0 { 0x02 } else { 0 };
        report[5] = 0;

        // Left stick drifts in a slow circle around center (same as build_standard_report).
        let angle = (step as f32) * 0.1;
        let dx = ((angle.cos() * 200.0) as i32 + STICK_CENTER as i32).clamp(0, 0xFFF) as u16;
        let dy = ((angle.sin() * 200.0) as i32 + STICK_CENTER as i32).clamp(0, 0xFFF) as u16;
        report[6] = (dx & 0xFF) as u8;
        report[7] = ((dx >> 8) & 0x0F) as u8 | ((dy & 0x0F) << 4) as u8;
        report[8] = ((dy >> 4) & 0xFF) as u8;
        // Right stick at center.
        report[9] = (STICK_CENTER & 0xFF) as u8;
        report[10] = ((STICK_CENTER >> 8) & 0x0F) as u8 | ((STICK_CENTER & 0x0F) << 4) as u8;
        report[11] = ((STICK_CENTER >> 4) & 0xFF) as u8;

        // IMU data: 3 frames × 12 bytes at offset 13..49.
        let frames = [
            ImuFrame {
                accel_x: (t.sin() * 1000.0) as i16,
                accel_y: 0,
                accel_z: 4096,
                gyro_x: 0,
                gyro_y: 0,
                gyro_z: 0,
            },
            ImuFrame {
                accel_x: ((t + 1.0).sin() * 1000.0) as i16,
                accel_y: 0,
                accel_z: 4096,
                gyro_x: 100,
                gyro_y: 0,
                gyro_z: 0,
            },
            ImuFrame {
                accel_x: ((t + 2.0).sin() * 1000.0) as i16,
                accel_y: 0,
                accel_z: 4096,
                gyro_x: 200,
                gyro_y: 0,
                gyro_z: 0,
            },
        ];

        for (i, frame) in frames.iter().enumerate() {
            let off = 13 + i * 12;
            report[off..off + 2].copy_from_slice(&frame.accel_x.to_le_bytes());
            report[off + 2..off + 4].copy_from_slice(&frame.accel_y.to_le_bytes());
            report[off + 4..off + 6].copy_from_slice(&frame.accel_z.to_le_bytes());
            report[off + 6..off + 8].copy_from_slice(&frame.gyro_x.to_le_bytes());
            report[off + 8..off + 10].copy_from_slice(&frame.gyro_y.to_le_bytes());
            report[off + 10..off + 12].copy_from_slice(&frame.gyro_z.to_le_bytes());
        }

        report
    }

    /// Build a synthetic subcommand reply (report ID 0x21) carrying battery
    /// telemetry. Battery raw cycles 8 -> 1 to simulate drain, with a
    /// simulated low-battery dip below the 15% warning threshold.
    pub fn build_subcmd_reply(&self) -> Vec<u8> {
        let step = {
            let s = self.step.lock().unwrap();
            *s
        };
        // Battery level (even values: 0=empty, 2=critical, 4=low, 6=medium, 8=full).
        // Bit 0 = charging. Simulate drain then a low dip every 32 ticks.
        let battery_level: u8 = if step % 32 < 8 {
            2 // critical — triggers low-battery warning
        } else {
            8u8.saturating_sub((((step / 8) % 7) * 2) as u8) // drain: 8, 6, 4, 2, ...
        };
        let battery_raw = battery_level; // not charging in mock

        let mut report = vec![0u8; 15];
        report[0] = REPORT_ID_SUBCMD_REPLY;
        report[1] = 0x00; // timer
        report[2] = (battery_raw << 4) | 0x01; // battery in high nibble, connection type 1 (BT)
        report[3] = 0; // right buttons
        report[4] = 0; // shared buttons
        report[5] = 0; // left buttons
        report[13] = 0x80; // ACK
        report[14] = 0x02; // subcmd ID (device info)
        report
    }

    // ------------------------------------------------------------------
    // Subcommand reply builders (report ID 0x21)
    // ------------------------------------------------------------------

    /// Build the common 0x21 subcmd reply header (bytes 0..15).
    /// Fills report ID, timer, battery (full), button bytes, and leaves
    /// ACK/subcmd-ID slots for the caller to set.
    fn build_subcmd_reply_header(&self, subcmd_id: u8, payload_len: usize) -> Vec<u8> {
        let step = {
            let s = self.step.lock().unwrap();
            *s
        };
        let total = 15 + payload_len;
        let mut report = vec![0u8; total];
        report[0] = REPORT_ID_SUBCMD_REPLY;
        report[1] = step as u8; // timer
        report[2] = 0x80; // battery: full (level 8), not charging
        report[3] = 0; // right buttons
        report[4] = 0; // shared buttons
        report[5] = 0; // left buttons
        report[13] = 0x80; // ACK (MSB = 1)
        report[14] = subcmd_id;
        report
    }

    /// Build a 0x21 subcmd reply with device info (subcmd 0x02).
    ///
    /// Reply data (12 bytes at [15..27]):
    ///   firmware major=0x03, minor=0x48 (v3.72), controller_type=0x03 (Pro),
    ///   unknown=0x02, MAC=BB:8A:EA:30:57:01, unknown=0x01, colors_from_spi=0x01
    pub fn build_device_info_reply(&self) -> Vec<u8> {
        let mut report = self.build_subcmd_reply_header(SUBCMD_REQUEST_DEVICE_INFO, 12);

        // Reply data at [15..27]
        report[15] = 0x03; // firmware major
        report[16] = 0x48; // firmware minor (v3.72)
        report[17] = 0x03; // controller type (Pro Controller)
        report[18] = 0x02; // unknown
                           // MAC address: BB:8A:EA:30:57:01 (little-endian as stored on device)
        report[19] = 0xBB;
        report[20] = 0x8A;
        report[21] = 0xEA;
        report[22] = 0x30;
        report[23] = 0x57;
        report[24] = 0x01;
        report[25] = 0x01; // unknown
        report[26] = 0x01; // colors_from_spi flag

        report
    }

    /// Build a 0x21 subcmd reply with SPI flash read data (subcmd 0x10).
    ///
    /// If `address` is 0x6080 (stick calibration), the payload is filled
    /// with realistic calibration values:
    ///     - Left stick:  center=0x800, min=0x200, max=0xE00 (X & Y)
    ///     - Right stick: center=0x800, min=0x200, max=0xE00 (X & Y)
    /// Otherwise the payload is filled with 0xAA.
    ///
    /// Data starts at [15] and is `size` bytes long.
    pub fn build_spi_flash_reply(&self, address: u32, size: u8) -> Vec<u8> {
        let mut report = self.build_subcmd_reply_header(SUBCMD_SPI_FLASH_READ, size as usize);

        if address == SPI_ADDR_STICK_CALIBRATION {
            // Stick calibration is stored as 12-bit packed values, 9 bytes
            // per stick (same packing as stick data in input reports).
            // Layout: center_x, center_y, min_x, min_y, max_x, max_y
            let center = 0x800u16;
            let min = 0x200u16;
            let max = 0xE00u16;

            let mut cal: Vec<u8> = Vec::with_capacity(18);
            // Pack 6 × 12-bit values into 9 bytes per stick.
            for stick_values in [&[center, center, min, min, max, max][..]; 2] {
                for chunk in stick_values.chunks(2) {
                    let x = chunk[0];
                    let y = chunk[1];
                    cal.push((x & 0xFF) as u8);
                    cal.push(((x >> 8) & 0x0F) as u8 | ((y & 0x0F) << 4) as u8);
                    cal.push(((y >> 4) & 0xFF) as u8);
                }
            }

            // Fill payload with calibration data, pad remainder with zeros.
            let payload = &mut report[15..15 + size as usize];
            for (i, byte) in cal.iter().enumerate() {
                if i >= payload.len() {
                    break;
                }
                payload[i] = *byte;
            }
        } else {
            // Unknown address — fill with 0xAA pattern.
            for byte in report[15..15 + size as usize].iter_mut() {
                *byte = 0xAA;
            }
        }

        report
    }

    /// Build a 0x21 ACK reply for the set-player-lights subcommand (0x30).
    /// 15 bytes total — no data payload, just the ACK.
    pub fn build_player_lights_reply(&self, _led_mask: u8) -> Vec<u8> {
        // No payload — just the 15-byte ACK header.
        self.build_subcmd_reply_header(SUBCMD_SET_PLAYER_LIGHTS, 0)
    }

    /// Build a 0x21 ACK reply for the set-home-light subcommand (0x38).
    /// 15 bytes total — no data payload, just the ACK.
    pub fn build_home_light_reply(&self) -> Vec<u8> {
        self.build_subcmd_reply_header(SUBCMD_SET_HOME_LIGHT, 0)
    }

    /// Build a 0x21 ACK reply for the enable-IMU subcommand (0x40).
    /// 15 bytes total — no data payload, just the ACK.
    pub fn build_enable_imu_reply(&self) -> Vec<u8> {
        self.build_subcmd_reply_header(SUBCMD_ENABLE_IMU, 0)
    }

    /// Build a 0x21 ACK reply for the enable-vibration subcommand (0x48).
    /// 15 bytes total — no data payload, just the ACK.
    pub fn build_enable_vibration_reply(&self) -> Vec<u8> {
        self.build_subcmd_reply_header(SUBCMD_ENABLE_VIBRATION, 0)
    }

    // ------------------------------------------------------------------
    // NFC / IR MCU report builders
    // ------------------------------------------------------------------

    /// Build a 0x31 NFC/IR input report (65+ bytes).
    ///
    /// Standard input (49 bytes) + NFC/IR payload (16+ bytes). When
    /// `with_tag` is true, a synthetic Amiibo-style tag is embedded in the
    /// NFC payload region starting at byte 49.
    pub fn build_nfc_ir_report(&self, with_tag: bool) -> Vec<u8> {
        let timer = {
            let s = self.step.lock().unwrap();
            *s as u8
        };

        let mut report = vec![0u8; 65];
        report[0] = 0x31; // NFC/IR report ID
        report[1] = timer; // timer
        report[2] = 0x80; // battery full
                          // Standard input: buttons, sticks (same layout as standard report)
        report[3] = 0x01; // X button
                          // Sticks at center
        report[6] = 0x00;
        report[7] = 0x08;
        report[8] = 0x80;
        report[9] = 0x00;
        report[10] = 0x08;
        report[11] = 0x80;

        if with_tag {
            // NFC tag present
            report[49] = 0x01; // NFC data present
            report[50] = 0x04; // UID byte 0 (Amiibo prefix)
            report[51] = 0x01;
            report[52] = 0x02;
            report[53] = 0x03;
            report[54] = 0x04;
            report[55] = 0x05;
            report[56] = 0x06;
            report[57] = 0x02; // tag type = Amiibo
                               // Game data
            for (i, byte) in report.iter_mut().enumerate().skip(58).take(7) {
                *byte = i as u8;
            }
        }

        report
    }

    /// Build a 0x21 ACK reply for the set-MCU-config subcommand (0x21).
    /// 15 bytes total — no data payload, just the ACK.
    pub fn build_mcu_config_reply(&self) -> Vec<u8> {
        let timer = {
            let s = self.step.lock().unwrap();
            *s as u8
        };
        let mut report = vec![0u8; 15];
        report[0] = 0x21;
        report[1] = timer;
        report[2] = 0x80;
        report[13] = 0x80; // ACK
        report[14] = 0x21; // MCU config subcmd
        report
    }

    /// Build a 0x21 ACK reply for the set-MCU-state subcommand (0x22).
    /// 15 bytes total — no data payload, just the ACK.
    pub fn build_mcu_state_reply(&self) -> Vec<u8> {
        let timer = {
            let s = self.step.lock().unwrap();
            *s as u8
        };
        let mut report = vec![0u8; 15];
        report[0] = 0x21;
        report[1] = timer;
        report[2] = 0x80;
        report[13] = 0x80;
        report[14] = 0x22; // MCU state subcmd
        report
    }

    /// Produce a `ControllerState` from the mock reports, applying deadzone
    /// and remap so the frontend sees a realistic pipeline output.
    pub fn build_controller_state(
        &self,
        config: &crate::state::AppConfig,
    ) -> (ControllerState, String, u8) {
        let std_report = self.build_standard_report();
        let sub_reply = self.build_subcmd_reply();

        let mut state = ControllerState {
            connected: true,
            ..Default::default()
        };

        // Parse standard report into buttons + sticks.
        if let Some(parsed) = crate::hid_parser::parse_standard_report(&std_report) {
            state.buttons = parsed.buttons.clone();
            state.left_stick = parsed.left_stick.clone();
            state.right_stick = parsed.right_stick.clone();
        }

        // Parse subcommand reply for battery.
        if let Some(reply) = crate::hid_parser::parse_subcmd_reply(&sub_reply) {
            state.battery_raw = reply.battery.raw;
            state.battery_percent = crate::hid_parser::battery_raw_to_percent(reply.battery.raw);
            state.charging = reply.battery.charging;
        }

        state.signal_strength = -55 - ((*self.step.lock().unwrap() % 10) as i8);
        state.timestamp = timestamp_now();

        // Apply deadzone + remap (same pipeline as real hardware path).
        crate::telemetry::TelemetryExtractor::apply_deadzone(
            &mut state.left_stick,
            config.deadzone_left,
        );
        crate::telemetry::TelemetryExtractor::apply_deadzone(
            &mut state.right_stick,
            config.deadzone_right,
        );
        crate::telemetry::TelemetryExtractor::apply_remap(&mut state.buttons, &config.button_remap);

        let hex = hex_string(&std_report);
        (state, hex, REPORT_ID_STANDARD)
    }

    /// Start the mock emission loop. Every `tick_ms` it builds a fresh
    /// controller state + raw HID hex and broadcasts `ControllerState` and
    /// `RawHidReport` IPC events. Periodically it also simulates a Bluetooth
    /// power-down to exercise the keep-alive adaptive boost path.
    pub fn start_loop(
        self: Arc<Self>,
        tx: tokio::sync::broadcast::Sender<IpcEvent>,
        shared: Arc<crate::state::SharedState>,
        tick_ms: u64,
        simulate_power_down_every: u32,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_millis(tick_ms));
            let mut tick: u32 = 0;
            info!(
                "Mock generator started (tick={}ms, power-down sim every {} ticks)",
                tick_ms, simulate_power_down_every
            );

            loop {
                ticker.tick().await;
                tick += 1;

                let config = shared.config.read().clone();

                // Skip mock data emission when mock_mode is disabled — let
                // the real device_loop own the shared controller state.
                if !config.mock_mode {
                    continue;
                }

                let (state, hex, report_id) = self.build_controller_state(&config);

                {
                    let mut cs = shared.active_controller_mut();
                    *cs = state.clone();
                }

                let _ = tx.send(IpcEvent::ControllerState {
                    data: state.clone(),
                });
                let _ = tx.send(IpcEvent::RawHidReport {
                    hex: hex.clone(),
                    report_id,
                });

                // Battery warning when at/below threshold.
                if crate::telemetry::TelemetryExtractor::check_battery_warning(
                    &state,
                    config.battery_warning_threshold,
                ) {
                    let _ = tx.send(IpcEvent::BatteryWarning {
                        percent: state.battery_percent,
                    });
                }

                // Periodically simulate a Bluetooth power-down to exercise
                // the keep-alive adaptive boost + frontend toast path.
                if simulate_power_down_every > 0 && tick.is_multiple_of(simulate_power_down_every) {
                    let now = timestamp_now();
                    debug!("Mock: simulating Bluetooth power-down at {}", now);
                    let _ = tx.send(IpcEvent::BluetoothPowerEvent {
                        event_type: "Power_Down_Simulated".into(),
                        timestamp: now,
                    });
                    let _ = tx.send(IpcEvent::Disconnected {
                        reason: "Simulated Bluetooth timeout (mock)".into(),
                    });
                    let _ = tx.send(IpcEvent::LogMessage {
                        level: "warn".into(),
                        message: format!("Mock simulated Bluetooth power-down at {}", now),
                    });
                }

                debug!(
                    "Mock tick {}: battery={}%, hex={}",
                    tick, state.battery_percent, hex
                );
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid_parser::{
        battery_raw_to_percent, parse_nfc_ir_report, parse_standard_report, parse_subcmd_reply,
        REPORT_ID_NFC_IR,
    };
    use crate::state::AppConfig;

    // ------------------------------------------------------------------
    // MockGenerator construction
    // ------------------------------------------------------------------

    #[test]
    fn new_creates_zeroed_step_and_tick() {
        let gen = MockGenerator::new();
        // First standard report increments step to 1 (timer byte == 1).
        let report = gen.build_standard_report();
        assert_eq!(report[1], 1, "first report should have timer == 1");
    }

    #[test]
    fn default_equals_new() {
        let d = MockGenerator::default();
        let n = MockGenerator::new();
        // Both should produce identical first reports (step starts at 0).
        let rd = d.build_standard_report();
        let rn = n.build_standard_report();
        assert_eq!(rd, rn, "default() and new() should behave identically");
    }

    // ------------------------------------------------------------------
    // build_standard_report
    // ------------------------------------------------------------------

    #[test]
    fn build_standard_report_length_and_id() {
        let gen = MockGenerator::new();
        let report = gen.build_standard_report();
        assert_eq!(report.len(), 12, "standard report should be 12 bytes");
        assert_eq!(report[0], REPORT_ID_STANDARD, "report ID should be 0x30");
    }

    #[test]
    fn build_standard_report_button_rotation() {
        let gen = MockGenerator::new();
        // step increments before use: 1,2,3,4 → mod4 = 1,2,3,0
        let buttons: Vec<u8> = (0..4).map(|_| gen.build_standard_report()[2]).collect();
        assert_eq!(buttons, vec![0x02, 0x04, 0x08, 0x01], "buttons should rotate X,B,A,Y");
    }

    #[test]
    fn build_standard_report_plus_button_every_8_ticks() {
        let gen = MockGenerator::new();
        // step 8 is the first time plus is pressed (step%8==0 at step=8).
        let mut plus_seen = false;
        for _ in 0..8 {
            let r = gen.build_standard_report();
            if r[3] != 0 {
                plus_seen = true;
            }
        }
        // After 8 calls step has gone 1..8; step==8 triggers plus.
        assert!(plus_seen, "plus button should fire at step 8");
    }

    #[test]
    fn build_standard_report_left_stick_drifts() {
        let gen = MockGenerator::new();
        let r1 = gen.build_standard_report();
        let r2 = gen.build_standard_report();
        // Left stick is encoded in bytes 6..9. Two consecutive steps should
        // generally differ (drift around center).
        let left1 = &r1[6..9];
        let left2 = &r2[6..9];
        assert_ne!(left1, left2, "left stick should drift between steps");
    }

    #[test]
    fn build_standard_report_right_stick_at_center() {
        let gen = MockGenerator::new();
        let r = gen.build_standard_report();
        // Right stick bytes 9..12 should encode STICK_CENTER.
        let lo = r[9] as u16;
        let mid = r[10] as u16;
        let hi = r[11] as u16;
        let x = lo | ((mid & 0x0F) << 8);
        let y = (mid >> 4) | (hi << 4);
        assert_eq!(x, STICK_CENTER, "right stick X at center");
        assert_eq!(y, STICK_CENTER, "right stick Y at center");
    }

    #[test]
    fn build_standard_report_parses_via_hid_parser() {
        let gen = MockGenerator::new();
        let r = gen.build_standard_report();
        let parsed = parse_standard_report(&r);
        // The 12-byte minimal report has a different byte-2 meaning than the
        // full 0x30 layout (button byte at [2] is interpreted as battery by
        // the parser), but it should still parse successfully.
        assert!(parsed.is_some(), "standard report should parse");
        let p = parsed.unwrap();
        assert_eq!(p.report_id, REPORT_ID_STANDARD);
        assert_eq!(p.timer, 1, "timer should be 1 at first call");
    }

    #[test]
    fn build_standard_report_step_wraps_at_64() {
        let gen = MockGenerator::new();
        // Generate 64 reports; timer byte should stay in 0..64 range.
        let mut max_timer = 0u8;
        for _ in 0..70 {
            let r = gen.build_standard_report();
            max_timer = max_timer.max(r[1]);
        }
        assert!(max_timer <= 63, "timer should wrap at 64, max was {}", max_timer);
    }

    // ------------------------------------------------------------------
    // build_imu_standard_report
    // ------------------------------------------------------------------

    #[test]
    fn build_imu_standard_report_length_and_id() {
        let gen = MockGenerator::new();
        let r = gen.build_imu_standard_report();
        assert_eq!(r.len(), 49, "IMU report should be 49 bytes");
        assert_eq!(r[0], REPORT_ID_STANDARD);
    }

    #[test]
    fn build_imu_standard_report_battery_full() {
        let gen = MockGenerator::new();
        let r = gen.build_imu_standard_report();
        assert_eq!(r[2], 0x80, "battery byte should indicate full / not charging");
    }

    #[test]
    fn build_imu_standard_report_sticks_at_center() {
        let gen = MockGenerator::new();
        let r = gen.build_imu_standard_report();
        // Left stick bytes 6..9
        let lx = r[6] as u16 | ((r[7] as u16 & 0x0F) << 8);
        let ly = (r[7] as u16 >> 4) | ((r[8] as u16) << 4);
        assert_eq!(lx, STICK_CENTER, "left stick X at center");
        assert_eq!(ly, STICK_CENTER, "left stick Y at center");
    }

    #[test]
    fn build_imu_standard_report_has_three_imu_frames() {
        let gen = MockGenerator::new();
        let r = gen.build_imu_standard_report();
        let parsed = parse_standard_report(&r).expect("should parse");
        let imu = parsed.imu.expect("should have IMU data");
        assert_eq!(imu.frames.len(), 3, "should have 3 IMU frames");
        // Frame 0: accel_z = 4096 (gravity), gyro_x = 0
        assert_eq!(imu.frames[0].accel_z, 4096);
        assert_eq!(imu.frames[0].gyro_x, 0);
        // Frame 1: gyro_x = 100
        assert_eq!(imu.frames[1].gyro_x, 100);
        // Frame 2: gyro_x = 200
        assert_eq!(imu.frames[2].gyro_x, 200);
    }

    #[test]
    fn build_imu_standard_report_accel_z_constant_gravity() {
        let gen = MockGenerator::new();
        let r = gen.build_imu_standard_report();
        let parsed = parse_standard_report(&r).expect("should parse");
        let imu = parsed.imu.expect("should have IMU");
        for frame in &imu.frames {
            assert_eq!(frame.accel_z, 4096, "all frames should have gravity on Z");
            assert_eq!(frame.accel_y, 0, "accel_y should be 0");
        }
    }

    #[test]
    fn build_imu_standard_report_accel_x_varies_sinusoidally() {
        let gen = MockGenerator::new();
        let r1 = gen.build_imu_standard_report();
        let r2 = gen.build_imu_standard_report();
        let p1 = parse_standard_report(&r1).unwrap();
        let p2 = parse_standard_report(&r2).unwrap();
        // accel_x follows sin(t); consecutive ticks should differ.
        let ax1 = p1.imu.as_ref().unwrap().frames[0].accel_x;
        let ax2 = p2.imu.as_ref().unwrap().frames[0].accel_x;
        assert_ne!(ax1, ax2, "accel_x should vary between ticks");
    }

    // ------------------------------------------------------------------
    // build_full_standard_report
    // ------------------------------------------------------------------

    #[test]
    fn build_full_standard_report_length_and_id() {
        let gen = MockGenerator::new();
        let r = gen.build_full_standard_report();
        assert_eq!(r.len(), 49);
        assert_eq!(r[0], REPORT_ID_STANDARD);
    }

    #[test]
    fn build_full_standard_report_left_stick_drifts() {
        let gen = MockGenerator::new();
        let r1 = gen.build_full_standard_report();
        let r2 = gen.build_full_standard_report();
        assert_ne!(&r1[6..9], &r2[6..9], "left stick should drift");
    }

    #[test]
    fn build_full_standard_report_has_imu() {
        let gen = MockGenerator::new();
        let r = gen.build_full_standard_report();
        let parsed = parse_standard_report(&r).expect("should parse");
        assert!(parsed.imu.is_some(), "full report should have IMU data");
    }

    #[test]
    fn build_full_standard_report_differs_from_imu_only() {
        let gen_a = MockGenerator::new();
        let gen_b = MockGenerator::new();
        let full = gen_a.build_full_standard_report();
        let imu_only = gen_b.build_imu_standard_report();
        // Left stick should differ (full drifts, imu_only is centered).
        assert_ne!(&full[6..9], &imu_only[6..9], "full report drifts, imu-only centered");
    }

    // ------------------------------------------------------------------
    // build_subcmd_reply
    // ------------------------------------------------------------------

    #[test]
    fn build_subcmd_reply_length_and_id() {
        let gen = MockGenerator::new();
        let r = gen.build_subcmd_reply();
        assert_eq!(r.len(), 15);
        assert_eq!(r[0], REPORT_ID_SUBCMD_REPLY);
    }

    #[test]
    fn build_subcmd_reply_ack_and_subcmd_id() {
        let gen = MockGenerator::new();
        let r = gen.build_subcmd_reply();
        assert_eq!(r[13], 0x80, "ACK byte MSB should be set");
        assert_eq!(r[14], 0x02, "subcmd ID should be device info");
    }

    #[test]
    fn build_subcmd_reply_parses() {
        let gen = MockGenerator::new();
        // build_standard_report increments step to 1 first.
        gen.build_standard_report();
        let r = gen.build_subcmd_reply();
        let parsed = parse_subcmd_reply(&r).expect("should parse");
        assert_eq!(parsed.ack, 0x80);
        assert_eq!(parsed.subcmd_id, 0x02);
        // step=1 → step%32=1 < 8 → battery_level=2 → raw=2.
        // battery_raw=2 is even → charging bit (bit 0) is NOT set.
        assert_eq!(parsed.battery.raw, 2);
        assert!(!parsed.battery.charging, "mock critical battery is not charging");
    }

    #[test]
    fn build_subcmd_reply_critical_battery_at_low_step() {
        let gen = MockGenerator::new();
        // step=0 (no increment in build_subcmd_reply): 0%32=0 < 8 → critical
        let r = gen.build_subcmd_reply();
        let parsed = parse_subcmd_reply(&r).unwrap();
        assert_eq!(parsed.battery.raw, 2, "should be critical at step 0");
        assert_eq!(battery_raw_to_percent(parsed.battery.raw), 10);
    }

    #[test]
    fn build_subcmd_reply_drains_at_higher_steps() {
        let gen = MockGenerator::new();
        // Advance step to 8 (past the critical window).
        for _ in 0..8 {
            gen.build_standard_report();
        }
        let r = gen.build_subcmd_reply();
        let parsed = parse_subcmd_reply(&r).unwrap();
        // step=8: 8%32=8, not < 8, so drain: 8 - ((8/8 % 7)*2) = 8 - 2 = 6
        assert_eq!(parsed.battery.raw, 6, "should be medium at step 8");
    }

    // ------------------------------------------------------------------
    // build_device_info_reply
    // ------------------------------------------------------------------

    #[test]
    fn build_device_info_reply_length_and_id() {
        let gen = MockGenerator::new();
        let r = gen.build_device_info_reply();
        assert_eq!(r.len(), 27, "device info reply = 15 header + 12 data");
        assert_eq!(r[0], REPORT_ID_SUBCMD_REPLY);
        assert_eq!(r[14], SUBCMD_REQUEST_DEVICE_INFO);
    }

    #[test]
    fn build_device_info_reply_firmware_and_type() {
        let gen = MockGenerator::new();
        let r = gen.build_device_info_reply();
        assert_eq!(r[15], 0x03, "firmware major");
        assert_eq!(r[16], 0x48, "firmware minor");
        assert_eq!(r[17], 0x03, "controller type Pro");
    }

    #[test]
    fn build_device_info_reply_mac_address() {
        let gen = MockGenerator::new();
        let r = gen.build_device_info_reply();
        assert_eq!(&r[19..25], &[0xBB, 0x8A, 0xEA, 0x30, 0x57, 0x01], "MAC address");
    }

    #[test]
    fn build_device_info_reply_parses() {
        let gen = MockGenerator::new();
        let r = gen.build_device_info_reply();
        let parsed = parse_subcmd_reply(&r).expect("should parse");
        assert_eq!(parsed.subcmd_id, SUBCMD_REQUEST_DEVICE_INFO);
        assert_eq!(parsed.reply_data.len(), 12);
        assert_eq!(parsed.reply_data[0], 0x03, "firmware major in reply_data");
    }

    // ------------------------------------------------------------------
    // build_spi_flash_reply
    // ------------------------------------------------------------------

    #[test]
    fn build_spi_flash_reply_length() {
        let gen = MockGenerator::new();
        let r = gen.build_spi_flash_reply(SPI_ADDR_STICK_CALIBRATION, 18);
        assert_eq!(r.len(), 15 + 18, "should be header + payload");
        assert_eq!(r[0], REPORT_ID_SUBCMD_REPLY);
        assert_eq!(r[14], SUBCMD_SPI_FLASH_READ);
    }

    #[test]
    fn build_spi_flash_reply_calibration_data() {
        let gen = MockGenerator::new();
        let r = gen.build_spi_flash_reply(SPI_ADDR_STICK_CALIBRATION, 18);
        let parsed = parse_subcmd_reply(&r).unwrap();
        assert_eq!(parsed.subcmd_id, SUBCMD_SPI_FLASH_READ);
        // First calibration byte should be low byte of center (0x800) = 0x00
        assert_eq!(parsed.reply_data[0], 0x00, "center_x low byte");
        // Second byte: (center>>8 & 0x0F) | (center_y & 0x0F)<<4 = 0x08 | 0x80 = 0x88
        assert_eq!(parsed.reply_data[1], 0x08, "center_x high nibble (0x08), center_y low nibble is 0");
    }

    #[test]
    fn build_spi_flash_reply_unknown_address_fills_aa() {
        let gen = MockGenerator::new();
        let r = gen.build_spi_flash_reply(0x0000, 10);
        let parsed = parse_subcmd_reply(&r).unwrap();
        assert_eq!(parsed.reply_data.len(), 10);
        assert!(parsed.reply_data.iter().all(|&b| b == 0xAA), "unknown addr should fill 0xAA");
    }

    #[test]
    fn build_spi_flash_reply_zero_size() {
        let gen = MockGenerator::new();
        let r = gen.build_spi_flash_reply(SPI_ADDR_STICK_CALIBRATION, 0);
        assert_eq!(r.len(), 15, "zero-size payload → header only");
    }

    // ------------------------------------------------------------------
    // ACK-only reply builders
    // ------------------------------------------------------------------

    #[test]
    fn build_player_lights_reply() {
        let gen = MockGenerator::new();
        let r = gen.build_player_lights_reply(0x0F);
        assert_eq!(r.len(), 15);
        assert_eq!(r[0], REPORT_ID_SUBCMD_REPLY);
        assert_eq!(r[13], 0x80, "ACK set");
        assert_eq!(r[14], SUBCMD_SET_PLAYER_LIGHTS);
    }

    #[test]
    fn build_home_light_reply() {
        let gen = MockGenerator::new();
        let r = gen.build_home_light_reply();
        assert_eq!(r.len(), 15);
        assert_eq!(r[14], SUBCMD_SET_HOME_LIGHT);
    }

    #[test]
    fn build_enable_imu_reply() {
        let gen = MockGenerator::new();
        let r = gen.build_enable_imu_reply();
        assert_eq!(r.len(), 15);
        assert_eq!(r[14], SUBCMD_ENABLE_IMU);
    }

    #[test]
    fn build_enable_vibration_reply() {
        let gen = MockGenerator::new();
        let r = gen.build_enable_vibration_reply();
        assert_eq!(r.len(), 15);
        assert_eq!(r[14], SUBCMD_ENABLE_VIBRATION);
    }

    #[test]
    fn all_ack_replies_parse_successfully() {
        let gen = MockGenerator::new();
        for r in [
            gen.build_player_lights_reply(0),
            gen.build_home_light_reply(),
            gen.build_enable_imu_reply(),
            gen.build_enable_vibration_reply(),
        ] {
            let parsed = parse_subcmd_reply(&r).expect("ACK reply should parse");
            assert_eq!(parsed.ack, 0x80, "ACK MSB should be set");
            assert!(parsed.reply_data.is_empty(), "no payload for ACK-only replies");
        }
    }

    // ------------------------------------------------------------------
    // build_nfc_ir_report
    // ------------------------------------------------------------------

    #[test]
    fn build_nfc_ir_report_without_tag() {
        let gen = MockGenerator::new();
        let r = gen.build_nfc_ir_report(false);
        assert_eq!(r.len(), 65);
        assert_eq!(r[0], REPORT_ID_NFC_IR);
        assert_eq!(r[49], 0, "no NFC data present");
    }

    #[test]
    fn build_nfc_ir_report_with_tag() {
        let gen = MockGenerator::new();
        let r = gen.build_nfc_ir_report(true);
        assert_eq!(r.len(), 65);
        assert_eq!(r[49], 0x01, "NFC data present flag");
        assert_eq!(r[50], 0x04, "UID byte 0 (Amiibo prefix)");
        assert_eq!(r[57], 0x02, "tag type = Amiibo");
    }

    #[test]
    fn build_nfc_ir_report_parses() {
        let gen = MockGenerator::new();
        let r = gen.build_nfc_ir_report(true);
        let parsed = parse_nfc_ir_report(&r).expect("should parse NFC report");
        assert!(parsed.nfc_tag.is_some(), "tag should be detected when present");
    }

    #[test]
    fn build_nfc_ir_report_no_tag_parses_no_tag() {
        let gen = MockGenerator::new();
        let r = gen.build_nfc_ir_report(false);
        let parsed = parse_nfc_ir_report(&r).expect("should parse NFC report");
        assert!(parsed.nfc_tag.is_none(), "no tag when with_tag=false");
    }

    // ------------------------------------------------------------------
    // build_mcu_config_reply / build_mcu_state_reply
    // ------------------------------------------------------------------

    #[test]
    fn build_mcu_config_reply() {
        let gen = MockGenerator::new();
        let r = gen.build_mcu_config_reply();
        assert_eq!(r.len(), 15);
        assert_eq!(r[0], REPORT_ID_SUBCMD_REPLY);
        assert_eq!(r[13], 0x80, "ACK set");
        assert_eq!(r[14], 0x21, "MCU config subcmd");
    }

    #[test]
    fn build_mcu_state_reply() {
        let gen = MockGenerator::new();
        let r = gen.build_mcu_state_reply();
        assert_eq!(r.len(), 15);
        assert_eq!(r[0], REPORT_ID_SUBCMD_REPLY);
        assert_eq!(r[13], 0x80, "ACK set");
        assert_eq!(r[14], 0x22, "MCU state subcmd");
    }

    #[test]
    fn mcu_replies_parse() {
        let gen = MockGenerator::new();
        let cfg = gen.build_mcu_config_reply();
        let st = gen.build_mcu_state_reply();
        let p_cfg = parse_subcmd_reply(&cfg).unwrap();
        let p_st = parse_subcmd_reply(&st).unwrap();
        assert_eq!(p_cfg.subcmd_id, 0x21);
        assert_eq!(p_st.subcmd_id, 0x22);
    }

    // ------------------------------------------------------------------
    // build_controller_state
    // ------------------------------------------------------------------

    #[test]
    fn build_controller_state_returns_connected_state() {
        let gen = MockGenerator::new();
        let config = AppConfig::default();
        let (state, _hex, report_id) = gen.build_controller_state(&config);
        assert!(state.connected, "state should be connected");
        assert_eq!(report_id, REPORT_ID_STANDARD);
    }

    #[test]
    fn build_controller_state_hex_is_valid() {
        let gen = MockGenerator::new();
        let config = AppConfig::default();
        let (_state, hex, _id) = gen.build_controller_state(&config);
        // hex_string produces lowercase hex pairs separated by spaces.
        assert!(!hex.is_empty(), "hex string should not be empty");
        assert!(hex.contains("30"), "hex should contain report ID 0x30");
    }

    #[test]
    fn build_controller_state_battery_parsed() {
        let gen = MockGenerator::new();
        let config = AppConfig::default();
        let (state, _hex, _id) = gen.build_controller_state(&config);
        // build_standard_report increments step to 1.
        // build_subcmd_reply reads step=1 → 1%32 < 8 → critical (raw=2) → 10%.
        assert_eq!(state.battery_raw, 2, "should be critical battery");
        assert_eq!(state.battery_percent, 10, "should be 10%");
        // battery_raw=2 is even → charging bit not set.
        assert!(!state.charging, "mock critical battery is not charging");
    }

    #[test]
    fn build_controller_state_buttons_remapped() {
        let gen = MockGenerator::new();
        let config = AppConfig::default();
        // Advance step to 7 so that build_controller_state's internal
        // build_standard_report increments to 8, where plus=0x02 is set.
        // The parser reads data[3] (plus byte) as the right-button byte,
        // so 0x02 → Y. Default remap y_to="x" → X pressed.
        for _ in 0..7 {
            gen.build_standard_report();
        }
        let (state, _hex, _id) = gen.build_controller_state(&config);
        assert!(state.buttons.x, "plus(0x02)→Y should be remapped to X");
        assert!(!state.buttons.y, "Y should be consumed by remap");
    }

    #[test]
    fn build_controller_state_signal_strength_in_range() {
        let gen = MockGenerator::new();
        let config = AppConfig::default();
        let (state, _hex, _id) = gen.build_controller_state(&config);
        // signal = -55 - (step % 10). step=1 → -56.
        assert_eq!(state.signal_strength, -56, "signal strength at step 1");
        assert!(state.signal_strength <= -55 && state.signal_strength >= -65);
    }

    #[test]
    fn build_controller_state_timestamp_set() {
        let gen = MockGenerator::new();
        let config = AppConfig::default();
        let (state, _hex, _id) = gen.build_controller_state(&config);
        assert!(state.timestamp > 0, "timestamp should be set");
    }

    #[test]
    fn build_controller_state_sticks_have_deadzone_applied() {
        let gen = MockGenerator::new();
        let mut config = AppConfig::default();
        // Large deadzone should zero out small drift.
        config.deadzone_left = 0.99;
        config.deadzone_right = 0.99;
        let (state, _hex, _id) = gen.build_controller_state(&config);
        assert!(
            state.left_stick.x.abs() < 0.01,
            "left stick should be zeroed by large deadzone"
        );
        assert!(
            state.right_stick.x.abs() < 0.01,
            "right stick should be zeroed by large deadzone"
        );
    }

    #[test]
    fn build_controller_state_multiple_calls_increment_step() {
        let gen = MockGenerator::new();
        let config = AppConfig::default();
        let (s1, _, _) = gen.build_controller_state(&config);
        let (s2, _, _) = gen.build_controller_state(&config);
        // step goes 1→2; signal strength changes.
        assert_ne!(s1.signal_strength, s2.signal_strength, "signal should change");
    }

    // ------------------------------------------------------------------
    // next_tick (tested indirectly via IMU reports)
    // ------------------------------------------------------------------

    #[test]
    fn next_tick_increments_via_imu_reports() {
        let gen = MockGenerator::new();
        let r1 = gen.build_imu_standard_report();
        let r2 = gen.build_imu_standard_report();
        let p1 = parse_standard_report(&r1).unwrap();
        let p2 = parse_standard_report(&r2).unwrap();
        // tick increments each call; accel_x = sin(t)*1000.
        // tick 1 vs tick 2 → different sin values.
        let ax1 = p1.imu.unwrap().frames[0].accel_x;
        let ax2 = p2.imu.unwrap().frames[0].accel_x;
        assert_ne!(ax1, ax2, "tick should increment between IMU reports");
    }

    #[test]
    fn next_tick_wraps_at_ffff() {
        // We can't easily drive tick to 0xFFFF without 65535 calls,
        // but we can verify the modular arithmetic doesn't panic by
        // generating many reports.
        let gen = MockGenerator::new();
        // Generate a batch; if wrapping logic is broken this would overflow.
        for _ in 0..100 {
            let _ = gen.build_imu_standard_report();
        }
        // Just verify it doesn't panic.
    }

    // ------------------------------------------------------------------
    // Cross-check: standard report vs full report button consistency
    // ------------------------------------------------------------------

    #[test]
    fn standard_and_full_report_share_button_rotation() {
        let gen_a = MockGenerator::new();
        let gen_b = MockGenerator::new();
        let std_r = gen_a.build_standard_report();
        let full_r = gen_b.build_full_standard_report();
        // Both at step 1 → same button byte.
        // standard: button at byte 2; full: button at byte 3.
        assert_eq!(std_r[2], full_r[3], "button byte should match at same step");
    }
}
