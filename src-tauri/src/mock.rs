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
