//! OxideLink library crate — re-exports shared modules and hosts the unit
//! test suite.
//!
//! The binary (`main.rs`) declares its own `mod` statements for the same
//! source files.  Both crates compile independently (lib + bin), so the
//! duplication is harmless in the interim.  The orchestrator will later
//! refactor `main.rs` to `use oxidelink::*` instead.

pub mod bthusb_monitor;
pub mod bt_reconnect;
pub mod config;
pub mod crash;
pub mod curves;
pub mod device_loop;
pub mod dsu;
pub mod gyro_mouse;
pub mod hid_parser;
pub mod hidhide;
pub mod imu;
pub mod kbm;
pub mod keepalive;
pub mod keycode;
pub mod logging;
pub mod macro_engine;
pub mod mock;
pub mod profile_manager;
pub mod state;
pub mod stick_cal;
pub mod subcmd;
pub mod telemetry;
pub mod telemetry_events;
pub mod tray;
pub mod turbo;
pub mod updater;
pub mod vixinput;
pub mod xinput;
pub use state::flick_stick;
pub mod cloud;
pub mod nfc;
pub mod overlay;

// ===========================================================================
//  Test suite
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use state::{
        timestamp_now, AppConfig, ButtonState, ControllerState, IpcEvent, RemapConfig, StickState,
    };

    // -----------------------------------------------------------------------
    //  Helpers
    // -----------------------------------------------------------------------

    /// Build a 49-byte standard input report (ID 0x30) with every button
    /// pressed and both sticks centred. Uses the correct Bluetooth layout:
    /// data[2]=battery, data[3]=right btns, data[4]=shared, data[5]=left btns.
    fn build_full_press_standard_report() -> Vec<u8> {
        let mut report = vec![0u8; 49];
        report[0] = hid_parser::REPORT_ID_STANDARD; // 0x30
        report[1] = 0x42; // timer
                          // Battery: full (8) + not charging → high nibble = 0x80, connection = 0x00
        report[2] = 0x80;
        // Right buttons: Y|X|B|A|R|ZR = 0x01|0x02|0x04|0x08|0x40|0x80 = 0xCF
        report[3] = 0x01 | 0x02 | 0x04 | 0x08 | 0x40 | 0x80; // 0xCF
                                                             // Shared buttons: minus|plus|R-stick|L-stick|home|capture = 0x3F
        report[4] = 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20; // 0x3F
                                                             // Left buttons: dpad_down|dpad_up|dpad_right|dpad_left|L|ZL = 0xCF
        report[5] = 0x01 | 0x02 | 0x04 | 0x08 | 0x40 | 0x80; // 0xCF
                                                             // Left stick centred: [0x00, 0x08, 0x80]
        report[6] = 0x00;
        report[7] = 0x08;
        report[8] = 0x80;
        // Right stick centred
        report[9] = 0x00;
        report[10] = 0x08;
        report[11] = 0x80;
        report
    }

    /// Encode a stick raw value (12-bit) into the 3-byte HID representation.
    fn encode_stick(raw_x: u16, raw_y: u16) -> [u8; 3] {
        let b0 = (raw_x & 0xFF) as u8;
        let b1 = ((raw_x >> 8) & 0x0F) as u8 | (((raw_y & 0x0F) << 4) as u8);
        let b2 = ((raw_y >> 4) & 0xFF) as u8;
        [b0, b1, b2]
    }

    /// Build a 15-byte subcommand reply (ID 0x21) with battery + buttons.
    /// battery_raw is the high-nibble value (level bits 3-1, charging bit 0).
    fn build_subcmd_reply(
        battery_raw: u8,
        connection_type: u8,
        right_btn: u8,
        shared_btn: u8,
    ) -> Vec<u8> {
        let mut report = vec![0u8; 15];
        report[0] = hid_parser::REPORT_ID_SUBCMD_REPLY; // 0x21
        report[1] = 0x00; // timer
        report[2] = ((battery_raw & 0x0F) << 4) | (connection_type & 0x0F);
        report[3] = right_btn;
        report[4] = shared_btn;
        report[5] = 0x00; // left buttons (not tested in subcmd reply)
                          // Bytes 6-12: stick + vibrator filler
                          // Byte 13: ACK byte (0x80 = simple ACK)
        report[13] = 0x80;
        // Byte 14: subcommand ID being replied to (e.g. 0x02 = device info)
        report[14] = 0x02;
        report
    }

    // ===================================================================
    //  hid_parser tests
    // ===================================================================

    mod hid_parser_tests {
        use super::*;

        // --- parse_standard_report --------------------------------------

        #[test]
        fn parse_standard_report_valid_all_buttons() {
            let data = build_full_press_standard_report();
            let parsed = hid_parser::parse_standard_report(&data).expect("should parse");

            assert_eq!(parsed.report_id, 0x30);
            assert_eq!(parsed.timer, 0x42);

            // Right buttons
            assert!(parsed.buttons.y, "Y should be pressed");
            assert!(parsed.buttons.x, "X should be pressed");
            assert!(parsed.buttons.b, "B should be pressed");
            assert!(parsed.buttons.a, "A should be pressed");
            assert!(parsed.buttons.r, "R should be pressed");
            assert!(parsed.buttons.zr, "ZR should be pressed");

            // Shared buttons
            assert!(parsed.buttons.minus, "minus should be pressed");
            assert!(parsed.buttons.plus, "plus should be pressed");
            assert!(parsed.buttons.stick_l, "stick_l should be pressed");
            assert!(parsed.buttons.stick_r, "stick_r should be pressed");
            assert!(parsed.buttons.home, "home should be pressed");
            assert!(parsed.buttons.capture, "capture should be pressed");

            // Left buttons
            assert!(parsed.buttons.dpad_down, "dpad_down should be pressed");
            assert!(parsed.buttons.dpad_up, "dpad_up should be pressed");
            assert!(parsed.buttons.dpad_right, "dpad_right should be pressed");
            assert!(parsed.buttons.dpad_left, "dpad_left should be pressed");
            assert!(parsed.buttons.l, "L should be pressed");
            assert!(parsed.buttons.zl, "ZL should be pressed");
        }

        #[test]
        fn parse_standard_report_individual_buttons() {
            // Test each right-button bit individually.
            // Empirical Pro Controller layout: bit0=X, bit1=Y, bit2=A, bit3=B, bit6=R, bit7=ZR
            let cases: [(u8, &str, fn(&ButtonState) -> bool); 6] = [
                (0x01, "x", |b| b.x),
                (0x02, "y", |b| b.y),
                (0x04, "a", |b| b.a),
                (0x08, "b", |b| b.b),
                (0x40, "r", |b| b.r),
                (0x80, "zr", |b| b.zr),
            ];
            for (bit, name, getter) in cases {
                let mut data = vec![0u8; 12];
                data[0] = 0x30;
                data[3] = bit;
                let parsed = hid_parser::parse_standard_report(&data).unwrap();
                assert!(
                    getter(&parsed.buttons),
                    "{} should be pressed for bit 0x{:02X}",
                    name,
                    bit
                );
                // All other right buttons should be off.
                assert!(
                    !parsed.buttons.a || bit == 0x04,
                    "a leaked for 0x{:02X}",
                    bit
                );
            }

            // Shared buttons: bit0=minus, bit1=plus, bit2=R-stick, bit3=L-stick, bit4=home, bit5=capture
            let shared_cases: [(u8, &str, fn(&ButtonState) -> bool); 6] = [
                (0x01, "minus", |b| b.minus),
                (0x02, "plus", |b| b.plus),
                (0x04, "stick_r", |b| b.stick_r),
                (0x08, "stick_l", |b| b.stick_l),
                (0x10, "home", |b| b.home),
                (0x20, "capture", |b| b.capture),
            ];
            for (bit, name, getter) in shared_cases {
                let mut data = vec![0u8; 12];
                data[0] = 0x30;
                data[4] = bit;
                let parsed = hid_parser::parse_standard_report(&data).unwrap();
                assert!(
                    getter(&parsed.buttons),
                    "{} should be pressed for bit 0x{:02X}",
                    name,
                    bit
                );
            }

            // Left buttons: bit0=down, bit1=up, bit2=right, bit3=left, bit6=L, bit7=ZL
            let left_cases: [(u8, &str, fn(&ButtonState) -> bool); 6] = [
                (0x01, "dpad_down", |b| b.dpad_down),
                (0x02, "dpad_up", |b| b.dpad_up),
                (0x04, "dpad_right", |b| b.dpad_right),
                (0x08, "dpad_left", |b| b.dpad_left),
                (0x40, "l", |b| b.l),
                (0x80, "zl", |b| b.zl),
            ];
            for (bit, name, getter) in left_cases {
                let mut data = vec![0u8; 12];
                data[0] = 0x30;
                data[5] = bit;
                let parsed = hid_parser::parse_standard_report(&data).unwrap();
                assert!(
                    getter(&parsed.buttons),
                    "{} should be pressed for bit 0x{:02X}",
                    name,
                    bit
                );
            }
        }

        #[test]
        fn parse_standard_report_no_buttons() {
            let mut data = vec![0u8; 12];
            data[0] = 0x30;
            let parsed = hid_parser::parse_standard_report(&data).unwrap();
            // All buttons should be false.
            let b = &parsed.buttons;
            assert!(!b.a && !b.b && !b.x && !b.y);
            assert!(!b.l && !b.r && !b.zl && !b.zr);
            assert!(!b.minus && !b.plus && !b.home && !b.capture);
            assert!(!b.stick_l && !b.stick_r);
            assert!(!b.dpad_up && !b.dpad_down && !b.dpad_left && !b.dpad_right);
        }

        #[test]
        fn parse_standard_report_too_short() {
            let data = vec![0x30, 0x00, 0x00];
            assert!(hid_parser::parse_standard_report(&data).is_none());
        }

        #[test]
        fn parse_standard_report_wrong_id() {
            let mut data = vec![0u8; 12];
            data[0] = 0x21; // wrong ID
            assert!(hid_parser::parse_standard_report(&data).is_none());
        }

        #[test]
        fn parse_standard_report_stick_center() {
            let data = build_full_press_standard_report();
            let parsed = hid_parser::parse_standard_report(&data).unwrap();
            assert_eq!(parsed.left_stick.raw_x, 0x800);
            assert_eq!(parsed.left_stick.raw_y, 0x800);
            assert!(
                (parsed.left_stick.x - 0.0).abs() < f32::EPSILON,
                "center x should be 0"
            );
            assert!(
                (parsed.left_stick.y - 0.0).abs() < f32::EPSILON,
                "center y should be 0"
            );
            assert_eq!(parsed.right_stick.raw_x, 0x800);
            assert_eq!(parsed.right_stick.raw_y, 0x800);
        }

        #[test]
        fn parse_standard_report_stick_max() {
            let mut data = vec![0u8; 12];
            data[0] = 0x30;
            // Left stick at max (0xFFF, 0xFFF)
            let stick = encode_stick(0xFFF, 0xFFF);
            data[6] = stick[0];
            data[7] = stick[1];
            data[8] = stick[2];
            // Right stick at min (0x000, 0x000)
            let rstick = encode_stick(0x000, 0x000);
            data[9] = rstick[0];
            data[10] = rstick[1];
            data[11] = rstick[2];

            let parsed = hid_parser::parse_standard_report(&data).unwrap();
            assert_eq!(parsed.left_stick.raw_x, 0xFFF);
            assert_eq!(parsed.left_stick.raw_y, 0xFFF);
            assert!(
                (parsed.left_stick.x - 1.0).abs() < 0.001,
                "max x should be ~1.0"
            );
            assert!(
                (parsed.left_stick.y - 1.0).abs() < 0.001,
                "max y should be ~1.0"
            );
            assert_eq!(parsed.right_stick.raw_x, 0x000);
            assert_eq!(parsed.right_stick.raw_y, 0x000);
            assert!(
                (parsed.right_stick.x - (-1.0)).abs() < 0.001,
                "min x should be ~-1.0"
            );
            assert!(
                (parsed.right_stick.y - (-1.0)).abs() < 0.001,
                "min y should be ~-1.0"
            );
        }

        #[test]
        fn parse_standard_report_stick_raw_decoding() {
            // Verify the exact bit-extraction formula.
            let mut data = vec![0u8; 12];
            data[0] = 0x30;
            // Left stick: raw_x = 0x3A5, raw_y = 0x6C2
            let stick = encode_stick(0x3A5, 0x6C2);
            data[6] = stick[0];
            data[7] = stick[1];
            data[8] = stick[2];
            let parsed = hid_parser::parse_standard_report(&data).unwrap();
            assert_eq!(parsed.left_stick.raw_x, 0x3A5, "raw_x decode");
            assert_eq!(parsed.left_stick.raw_y, 0x6C2, "raw_y decode");
        }

        // --- parse_subcmd_reply -----------------------------------------

        #[test]
        fn parse_subcmd_reply_valid() {
            // battery_raw = 4 (level 2 = low, not charging), connection_type = 1 (BT)
            let data = build_subcmd_reply(4, 1, 0x04, 0x02);
            let reply = hid_parser::parse_subcmd_reply(&data).expect("should parse");

            assert_eq!(reply.battery.raw, 4);
            assert!(
                !reply.battery.charging,
                "battery raw 4: bit 0 = 0 so not charging"
            );
            assert_eq!(reply.battery.connection_type, 1);
            // Buttons from right_btn=0x04 → A pressed (bit 2 = A in empirical layout)
            assert!(reply.buttons.a);
            assert!(!reply.buttons.b);
            // Buttons from shared_btn=0x02 → plus pressed
            assert!(reply.buttons.plus);
            assert!(!reply.buttons.minus);
            // Left-side buttons should all be false (byte 5 = 0)
            assert!(!reply.buttons.l);
            assert!(!reply.buttons.zl);
            assert!(!reply.buttons.dpad_up);
            // ACK and subcmd ID
            assert_eq!(reply.ack, 0x80);
            assert_eq!(reply.subcmd_id, 0x02);
        }

        #[test]
        fn parse_subcmd_reply_charging() {
            // battery_raw = 9 → level 4 (full), bit 0 = 1 → charging
            let data = build_subcmd_reply(9, 2, 0, 0);
            let reply = hid_parser::parse_subcmd_reply(&data).unwrap();
            assert_eq!(reply.battery.raw, 9);
            assert!(
                reply.battery.charging,
                "battery raw 9: bit 0 = 1 means charging"
            );
            assert_eq!(reply.battery.connection_type, 2);
        }

        #[test]
        fn parse_subcmd_reply_boundary_charging() {
            // battery_raw = 8 → level 4 (full), bit 0 = 0 → not charging
            let data = build_subcmd_reply(8, 0, 0, 0);
            let reply = hid_parser::parse_subcmd_reply(&data).unwrap();
            assert_eq!(reply.battery.raw, 8);
            assert!(
                !reply.battery.charging,
                "battery raw 8: bit 0 = 0 so not charging"
            );
        }

        #[test]
        fn parse_subcmd_reply_too_short() {
            let data = vec![0x21, 0x00, 0x00];
            assert!(hid_parser::parse_subcmd_reply(&data).is_none());
        }

        #[test]
        fn parse_subcmd_reply_wrong_id() {
            let data = vec![0x30, 0x00, 0x00, 0x00, 0x00];
            assert!(hid_parser::parse_subcmd_reply(&data).is_none());
        }

        // --- battery_raw_to_percent -------------------------------------

        #[test]
        fn battery_raw_to_percent_all_values() {
            // Pro Controller reports 5 discrete levels:
            // raw 0 (level 0, empty) → 0%
            // raw 2 (level 1, critical) → 10%
            // raw 4 (level 2, low) → 25%
            // raw 6 (level 3, medium) → 50%
            // raw 8 (level 4, full) → 100%
            let expected: [(u8, u8); 5] = [(0, 0), (2, 10), (4, 25), (6, 50), (8, 100)];
            for (raw, pct) in expected {
                assert_eq!(
                    hid_parser::battery_raw_to_percent(raw),
                    pct,
                    "raw {} should map to {}%",
                    raw,
                    pct
                );
            }
        }

        #[test]
        fn battery_raw_to_percent_charging_bit_ignored() {
            // Charging bit (bit 0) should not affect percentage.
            // raw 8 (full, not charging) = 100%, raw 9 (full, charging) = 100%
            assert_eq!(hid_parser::battery_raw_to_percent(8), 100);
            assert_eq!(hid_parser::battery_raw_to_percent(9), 100);
            // raw 6 (medium, not charging) = 50%, raw 7 (medium, charging) = 50%
            assert_eq!(hid_parser::battery_raw_to_percent(6), 50);
            assert_eq!(hid_parser::battery_raw_to_percent(7), 50);
        }

        #[test]
        fn battery_raw_to_percent_above_max() {
            assert_eq!(hid_parser::battery_raw_to_percent(255), 100);
        }

        // --- hex_string -------------------------------------------------

        #[test]
        fn hex_string_known_bytes() {
            let data = [0xAA, 0xBB, 0xCC];
            assert_eq!(hid_parser::hex_string(&data), "AA BB CC");
        }

        #[test]
        fn hex_string_empty() {
            assert_eq!(hid_parser::hex_string(&[]), "");
        }

        #[test]
        fn hex_string_single_byte() {
            assert_eq!(hid_parser::hex_string(&[0x00]), "00");
        }

        #[test]
        fn hex_string_all_zeros() {
            let data = [0u8; 4];
            assert_eq!(hid_parser::hex_string(&data), "00 00 00 00");
        }

        // --- build_rumble_report / build_zero_rumble --------------------

        #[test]
        fn build_rumble_report_length_and_header() {
            let report = hid_parser::build_rumble_report(1, 1);
            assert_eq!(report.len(), 49);
            assert_eq!(report[0], 0x10, "report[0] should be 0x10");
            assert_eq!(report[1], 0x00);
        }

        #[test]
        fn build_rumble_report_active() {
            let report = hid_parser::build_rumble_report(1, 1);
            assert_eq!(report[47], 0x20, "left active → 0x20");
            assert_eq!(report[48], 0x20, "right active → 0x20");
        }

        #[test]
        fn build_rumble_report_inactive() {
            let report = hid_parser::build_rumble_report(0, 0);
            assert_eq!(report[47], 0x00, "left inactive → 0x00");
            assert_eq!(report[48], 0x00, "right inactive → 0x00");
        }

        #[test]
        fn build_zero_rumble_length_and_header() {
            let report = hid_parser::build_zero_rumble();
            assert_eq!(report.len(), 10);
            assert_eq!(report[0], 0x10, "report[0] should be 0x10");
            assert_eq!(report[1], 0x00);
            for i in 2..10 {
                assert_eq!(report[i], 0x00, "byte {} should be zero", i);
            }
        }

        // --- constants --------------------------------------------------

        #[test]
        fn constants_are_correct() {
            assert_eq!(hid_parser::NINTENDO_VID, 0x057E);
            assert_eq!(hid_parser::PRO_CONTROLLER_PID, 0x2009);
            assert_eq!(hid_parser::REPORT_ID_STANDARD, 0x30);
            assert_eq!(hid_parser::REPORT_ID_SUBCMD_REPLY, 0x21);
            assert_eq!(hid_parser::REPORT_ID_NFC_IR, 0x31);
            assert_eq!(hid_parser::STICK_CENTER, 0x800);
            assert_eq!(hid_parser::STICK_MAX, 0xFFF);
            assert_eq!(hid_parser::STICK_MIN, 0x000);
        }

        // --- 0x3F simple HID report tests -------------------------------

        /// Build a 12-byte 0x3F (default Bluetooth) report.
        fn build_0x3f_report(btn_byte1: u8, btn_byte2: u8, hat: u8) -> Vec<u8> {
            let mut report = vec![0u8; 12];
            report[0] = hid_parser::REPORT_ID_DEFAULT_BT; // 0x3F
            report[1] = btn_byte1;
            report[2] = btn_byte2;
            report[3] = hat; // hat switch / D-pad
                             // Left stick centred at 0x8000 (big-endian)
            report[4] = 0x80;
            report[5] = 0x00;
            report[6] = 0x80;
            report[7] = 0x00;
            // Right stick centred at 0x8000 (big-endian)
            report[8] = 0x80;
            report[9] = 0x00;
            report[10] = 0x80;
            report[11] = 0x00;
            report
        }

        #[test]
        fn parse_standard_report_0x3f() {
            let data = build_0x3f_report(0, 0, 8);
            let parsed = hid_parser::parse_standard_report(&data).expect("0x3F should parse");
            assert_eq!(parsed.report_id, 0x3F);
            // 0x3F reports do not carry IMU data.
            assert!(parsed.imu.is_none(), "0x3F report should have no IMU");
        }

        #[test]
        fn parse_standard_report_0x3f_buttons() {
            // Byte 1: bit0=A, bit1=B, bit2=X, bit3=Y, bit4=L, bit5=R, bit6=ZL, bit7=ZR
            let data = build_0x3f_report(0x01 | 0x08, 0x01 | 0x04, 8);
            let parsed = hid_parser::parse_standard_report(&data).expect("should parse");
            assert!(parsed.buttons.a, "A should be pressed (bit0)");
            assert!(parsed.buttons.y, "Y should be pressed (bit3)");
            // Byte 2: bit0=Minus, bit2=Home
            assert!(parsed.buttons.minus, "Minus should be pressed (bit0)");
            assert!(parsed.buttons.home, "Home should be pressed (bit2)");
        }

        #[test]
        fn parse_standard_report_0x3f_sticks() {
            let mut data = build_0x3f_report(0, 0, 8);
            // Left stick at max X (0xFFFF), center Y (0x8000) — big-endian
            data[4] = 0xFF;
            data[5] = 0xFF;
            data[6] = 0x80;
            data[7] = 0x00;
            // Right stick at min X (0x0000), center Y (0x8000)
            data[8] = 0x00;
            data[9] = 0x00;
            data[10] = 0x80;
            data[11] = 0x00;

            let parsed = hid_parser::parse_standard_report(&data).expect("should parse");
            assert!(
                (parsed.left_stick.x - 1.0).abs() < 0.01,
                "left stick X should be ~1.0, got {}",
                parsed.left_stick.x
            );
            assert!(
                (parsed.right_stick.x - (-1.0)).abs() < 0.01,
                "right stick X should be ~-1.0, got {}",
                parsed.right_stick.x
            );
            // raw values should be 16-bit
            assert_eq!(parsed.left_stick.raw_x, 0xFFFF);
            assert_eq!(parsed.right_stick.raw_x, 0x0000);
        }

        #[test]
        fn parse_standard_report_0x3f_too_short() {
            let data = vec![0x3F, 0x00, 0x00]; // only 3 bytes
            let parsed = hid_parser::parse_standard_report(&data);
            assert!(parsed.is_none(), "short 0x3F report should return None");
        }
    }

    // ===================================================================
    //  xinput tests
    // ===================================================================

    mod xinput_tests {
        use super::*;

        fn all_pressed_buttons() -> ButtonState {
            ButtonState {
                a: true,
                b: true,
                x: true,
                y: true,
                l: true,
                r: true,
                zl: true,
                zr: true,
                minus: true,
                plus: true,
                home: true,
                capture: true,
                stick_l: true,
                stick_r: true,
                dpad_up: true,
                dpad_down: true,
                dpad_left: true,
                dpad_right: true,
                sr_right: false,
                sl_right: false,
                sr_left: false,
                sl_left: false,
            }
        }

        fn centered_stick() -> StickState {
            StickState {
                x: 0.0,
                y: 0.0,
                raw_x: 0x800,
                raw_y: 0x800,
            }
        }

        #[test]
        fn map_to_xinput_all_buttons_pressed() {
            let buttons = all_pressed_buttons();
            let stick = centered_stick();
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 0.0, 0.0);

            let expected: u16 = xinput::XINPUT_GAMEPAD_A
                | xinput::XINPUT_GAMEPAD_B
                | xinput::XINPUT_GAMEPAD_X
                | xinput::XINPUT_GAMEPAD_Y
                | xinput::XINPUT_GAMEPAD_LEFT_SHOULDER
                | xinput::XINPUT_GAMEPAD_RIGHT_SHOULDER
                | xinput::XINPUT_GAMEPAD_LEFT_THUMB
                | xinput::XINPUT_GAMEPAD_RIGHT_THUMB
                | xinput::XINPUT_GAMEPAD_BACK
                | xinput::XINPUT_GAMEPAD_START
                | xinput::XINPUT_GAMEPAD_GUIDE
                | xinput::XINPUT_GAMEPAD_DPAD_UP
                | xinput::XINPUT_GAMEPAD_DPAD_DOWN
                | xinput::XINPUT_GAMEPAD_DPAD_LEFT
                | xinput::XINPUT_GAMEPAD_DPAD_RIGHT;

            assert_eq!(state.buttons, expected);
        }

        #[test]
        fn map_to_xinput_no_buttons() {
            let buttons = ButtonState::default();
            let stick = centered_stick();
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 0.0, 0.0);
            assert_eq!(state.buttons, 0);
            assert_eq!(state.left_trigger, 0);
            assert_eq!(state.right_trigger, 0);
        }

        #[test]
        fn map_to_xinput_individual_button_flags() {
            let stick = centered_stick();

            // A
            let mut b = ButtonState::default();
            b.a = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_A);

            // B
            let mut b = ButtonState::default();
            b.b = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_B);

            // X
            let mut b = ButtonState::default();
            b.x = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_X);

            // Y
            let mut b = ButtonState::default();
            b.y = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_Y);

            // L
            let mut b = ButtonState::default();
            b.l = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_LEFT_SHOULDER);

            // R
            let mut b = ButtonState::default();
            b.r = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_RIGHT_SHOULDER);

            // stick_l
            let mut b = ButtonState::default();
            b.stick_l = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_LEFT_THUMB);

            // stick_r
            let mut b = ButtonState::default();
            b.stick_r = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_RIGHT_THUMB);

            // minus → BACK
            let mut b = ButtonState::default();
            b.minus = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_BACK);

            // plus → START
            let mut b = ButtonState::default();
            b.plus = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_START);

            // home → GUIDE
            let mut b = ButtonState::default();
            b.home = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_GUIDE);

            // dpad_up
            let mut b = ButtonState::default();
            b.dpad_up = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_DPAD_UP);

            // dpad_down
            let mut b = ButtonState::default();
            b.dpad_down = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_DPAD_DOWN);

            // dpad_left
            let mut b = ButtonState::default();
            b.dpad_left = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_DPAD_LEFT);

            // dpad_right
            let mut b = ButtonState::default();
            b.dpad_right = true;
            let s = xinput::map_to_xinput(&b, &stick, &stick, 0.0, 0.0);
            assert_eq!(s.buttons, xinput::XINPUT_GAMEPAD_DPAD_RIGHT);
        }

        #[test]
        fn map_to_xinput_stick_scaling_max() {
            let buttons = ButtonState::default();
            let stick = StickState {
                x: 1.0,
                y: 1.0,
                raw_x: 0xFFF,
                raw_y: 0xFFF,
            };
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 0.0, 0.0);
            assert_eq!(state.thumb_lx, 32767);
            assert_eq!(state.thumb_ly, 32767);
            assert_eq!(state.thumb_rx, 32767);
            assert_eq!(state.thumb_ry, 32767);
        }

        #[test]
        fn map_to_xinput_stick_scaling_min() {
            let buttons = ButtonState::default();
            let stick = StickState {
                x: -1.0,
                y: -1.0,
                raw_x: 0x000,
                raw_y: 0x000,
            };
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 0.0, 0.0);
            assert_eq!(state.thumb_lx, -32767);
            assert_eq!(state.thumb_ly, -32767);
            assert_eq!(state.thumb_rx, -32767);
            assert_eq!(state.thumb_ry, -32767);
        }

        #[test]
        fn map_to_xinput_stick_scaling_center() {
            let buttons = ButtonState::default();
            let stick = centered_stick();
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 0.0, 0.0);
            assert_eq!(state.thumb_lx, 0);
            assert_eq!(state.thumb_ly, 0);
            assert_eq!(state.thumb_rx, 0);
            assert_eq!(state.thumb_ry, 0);
        }

        #[test]
        fn map_to_xinput_trigger_scaling_max() {
            let buttons = ButtonState::default();
            let stick = centered_stick();
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 1.0, 1.0);
            assert_eq!(state.left_trigger, 255);
            assert_eq!(state.right_trigger, 255);
        }

        #[test]
        fn map_to_xinput_trigger_scaling_zero() {
            let buttons = ButtonState::default();
            let stick = centered_stick();
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 0.0, 0.0);
            assert_eq!(state.left_trigger, 0);
            assert_eq!(state.right_trigger, 0);
        }

        #[test]
        fn map_to_xinput_trigger_scaling_half() {
            let buttons = ButtonState::default();
            let stick = centered_stick();
            let state = xinput::map_to_xinput(&buttons, &stick, &stick, 0.5, 0.5);
            assert_eq!(state.left_trigger, 127); // 0.5 * 255 = 127.5 → 127
            assert_eq!(state.right_trigger, 127);
        }

        #[test]
        fn xinput_state_to_hex_format() {
            let state = xinput::XInputState {
                buttons: 0x1234,
                left_trigger: 0xAB,
                right_trigger: 0xCD,
                thumb_lx: 0x1111,
                thumb_ly: 0x2222,
                thumb_rx: 0x3333,
                thumb_ry: 0x4444,
            };
            let hex = xinput::xinput_state_to_hex(&state);
            assert_eq!(hex, "1234 AB CD 1111 2222 3333 4444");
        }

        #[test]
        fn xinput_state_to_hex_default() {
            let state = xinput::XInputState::default();
            let hex = xinput::xinput_state_to_hex(&state);
            assert_eq!(hex, "0000 00 00 0000 0000 0000 0000");
        }

        #[test]
        fn default_nintendo_to_xinput_remap_values() {
            let remap = xinput::default_nintendo_to_xinput_remap();
            assert_eq!(remap.a_to, "b");
            assert_eq!(remap.b_to, "a");
            assert_eq!(remap.x_to, "y");
            assert_eq!(remap.y_to, "x");
        }

        #[test]
        fn xinput_constants_correct() {
            assert_eq!(xinput::XINPUT_GAMEPAD_A, 0x1000);
            assert_eq!(xinput::XINPUT_GAMEPAD_B, 0x2000);
            assert_eq!(xinput::XINPUT_GAMEPAD_X, 0x4000);
            assert_eq!(xinput::XINPUT_GAMEPAD_Y, 0x8000);
            assert_eq!(xinput::XINPUT_GAMEPAD_LEFT_SHOULDER, 0x0100);
            assert_eq!(xinput::XINPUT_GAMEPAD_RIGHT_SHOULDER, 0x0200);
            assert_eq!(xinput::XINPUT_GAMEPAD_LEFT_THUMB, 0x0040);
            assert_eq!(xinput::XINPUT_GAMEPAD_RIGHT_THUMB, 0x0080);
            assert_eq!(xinput::XINPUT_GAMEPAD_BACK, 0x0020);
            assert_eq!(xinput::XINPUT_GAMEPAD_START, 0x0010);
            assert_eq!(xinput::XINPUT_GAMEPAD_GUIDE, 0x0400);
            assert_eq!(xinput::XINPUT_GAMEPAD_DPAD_UP, 0x0001);
            assert_eq!(xinput::XINPUT_GAMEPAD_DPAD_DOWN, 0x0002);
            assert_eq!(xinput::XINPUT_GAMEPAD_DPAD_LEFT, 0x0004);
            assert_eq!(xinput::XINPUT_GAMEPAD_DPAD_RIGHT, 0x0008);
        }
    }

    // ===================================================================
    //  telemetry tests
    // ===================================================================

    mod telemetry_tests {
        use super::*;

        // --- apply_deadzone ---------------------------------------------

        #[test]
        fn apply_deadzone_below_deadzone_zeros() {
            let mut stick = StickState {
                x: 0.03,
                y: 0.04,
                raw_x: 0,
                raw_y: 0,
            };
            // magnitude = sqrt(0.03^2 + 0.04^2) = 0.05 < 0.1
            telemetry::TelemetryExtractor::apply_deadzone(&mut stick, 0.1);
            assert!((stick.x - 0.0).abs() < f32::EPSILON);
            assert!((stick.y - 0.0).abs() < f32::EPSILON);
        }

        #[test]
        fn apply_deadzone_above_deadzone_scales() {
            let mut stick = StickState {
                x: 1.0,
                y: 0.0,
                raw_x: 0,
                raw_y: 0,
            };
            // magnitude = 1.0, scale = (1.0 - 0.1) / (1.0 - 0.1) = 1.0
            telemetry::TelemetryExtractor::apply_deadzone(&mut stick, 0.1);
            assert!((stick.x - 1.0).abs() < 0.001, "x should remain ~1.0");
            assert!((stick.y - 0.0).abs() < f32::EPSILON);
        }

        #[test]
        fn apply_deadzone_at_exactly_deadzone() {
            // magnitude == deadzone → magnitude < deadzone is false, so it
            // enters the scaling branch.  scale = (d - d)/(1-d) = 0.
            let mut stick = StickState {
                x: 0.1,
                y: 0.0,
                raw_x: 0,
                raw_y: 0,
            };
            telemetry::TelemetryExtractor::apply_deadzone(&mut stick, 0.1);
            // magnitude = 0.1, not < 0.1, so scale = (0.1-0.1)/(1-0.1) = 0
            assert!(
                (stick.x - 0.0).abs() < 0.001,
                "x should be ~0 at deadzone boundary"
            );
            assert!((stick.y - 0.0).abs() < f32::EPSILON);
        }

        #[test]
        fn apply_deadzone_magnitude_just_above() {
            let mut stick = StickState {
                x: 0.5,
                y: 0.0,
                raw_x: 0,
                raw_y: 0,
            };
            // magnitude = 0.5, deadzone = 0.1
            // scale = (0.5 - 0.1) / (1.0 - 0.1) = 0.4/0.9 ≈ 0.4444
            // x = (0.5/0.5) * 0.4444 = 0.4444
            telemetry::TelemetryExtractor::apply_deadzone(&mut stick, 0.1);
            let expected = 0.4_f32 / 0.9_f32;
            assert!(
                (stick.x - expected).abs() < 0.001,
                "x should be ~{:.4}",
                expected
            );
        }

        #[test]
        fn apply_deadzone_zero_stick() {
            let mut stick = StickState::default();
            telemetry::TelemetryExtractor::apply_deadzone(&mut stick, 0.08);
            assert!((stick.x - 0.0).abs() < f32::EPSILON);
            assert!((stick.y - 0.0).abs() < f32::EPSILON);
        }

        // --- apply_remap ------------------------------------------------

        #[test]
        fn apply_remap_default_swaps_ab_xy() {
            let remap = RemapConfig {
                a_to: "b".into(),
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            };

            // Press only A → after remap, B should be true (A was mapped to B target,
            // which reads original B = false).  Actually: buttons.a = remap_button("b", original)
            // = original.b.  So if original A=true, B=false, X=false, Y=false:
            // new a = original.b = false
            // new b = original.a = true
            // new x = original.y = false
            // new y = original.x = false
            let mut buttons = ButtonState::default();
            buttons.a = true;
            telemetry::TelemetryExtractor::apply_remap(&mut buttons, &remap);
            assert!(
                !buttons.a,
                "a should be false after remap (was original b=false)"
            );
            assert!(
                buttons.b,
                "b should be true after remap (was original a=true)"
            );
            assert!(!buttons.x);
            assert!(!buttons.y);
        }

        #[test]
        fn apply_remap_press_b_becomes_a() {
            let remap = RemapConfig {
                a_to: "b".into(),
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            };
            let mut buttons = ButtonState::default();
            buttons.b = true;
            telemetry::TelemetryExtractor::apply_remap(&mut buttons, &remap);
            assert!(buttons.a, "a should be true (original b=true)");
            assert!(!buttons.b, "b should be false (original a=false)");
        }

        #[test]
        fn apply_remap_press_x_becomes_y() {
            let remap = RemapConfig {
                a_to: "b".into(),
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            };
            let mut buttons = ButtonState::default();
            buttons.x = true;
            telemetry::TelemetryExtractor::apply_remap(&mut buttons, &remap);
            assert!(buttons.y, "y should be true (original x=true)");
            assert!(!buttons.x, "x should be false (original y=false)");
        }

        #[test]
        fn apply_remap_press_y_becomes_x() {
            let remap = RemapConfig {
                a_to: "b".into(),
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            };
            let mut buttons = ButtonState::default();
            buttons.y = true;
            telemetry::TelemetryExtractor::apply_remap(&mut buttons, &remap);
            assert!(buttons.x, "x should be true (original y=true)");
            assert!(!buttons.y, "y should be false (original x=false)");
        }

        #[test]
        fn apply_remap_all_four_pressed() {
            let remap = RemapConfig {
                a_to: "b".into(),
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            };
            let mut buttons = ButtonState::default();
            buttons.a = true;
            buttons.b = true;
            buttons.x = true;
            buttons.y = true;
            telemetry::TelemetryExtractor::apply_remap(&mut buttons, &remap);
            // All should remain true since swap of all-true is all-true.
            assert!(buttons.a);
            assert!(buttons.b);
            assert!(buttons.x);
            assert!(buttons.y);
        }

        #[test]
        fn apply_remap_no_buttons_pressed() {
            let remap = RemapConfig {
                a_to: "b".into(),
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            };
            let mut buttons = ButtonState::default();
            telemetry::TelemetryExtractor::apply_remap(&mut buttons, &remap);
            assert!(!buttons.a && !buttons.b && !buttons.x && !buttons.y);
        }

        #[test]
        fn apply_remap_unknown_target() {
            let remap = RemapConfig {
                a_to: "zzz".into(), // unknown target → false
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            };
            let mut buttons = ButtonState::default();
            buttons.a = true;
            buttons.b = true;
            telemetry::TelemetryExtractor::apply_remap(&mut buttons, &remap);
            assert!(!buttons.a, "unknown target should yield false");
            assert!(buttons.b, "b should be true (original a=true)");
        }

        // --- check_battery_warning --------------------------------------

        #[test]
        fn check_battery_warning_below_threshold() {
            let mut state = ControllerState::default();
            state.battery_percent = 10;
            state.charging = false;
            assert!(telemetry::TelemetryExtractor::check_battery_warning(
                &state, 15
            ));
        }

        #[test]
        fn check_battery_warning_at_threshold() {
            let mut state = ControllerState::default();
            state.battery_percent = 15;
            state.charging = false;
            assert!(telemetry::TelemetryExtractor::check_battery_warning(
                &state, 15
            ));
        }

        #[test]
        fn check_battery_warning_above_threshold() {
            let mut state = ControllerState::default();
            state.battery_percent = 50;
            state.charging = false;
            assert!(!telemetry::TelemetryExtractor::check_battery_warning(
                &state, 15
            ));
        }

        #[test]
        fn check_battery_warning_charging_is_false() {
            let mut state = ControllerState::default();
            state.battery_percent = 5;
            state.charging = true;
            assert!(!telemetry::TelemetryExtractor::check_battery_warning(
                &state, 15
            ));
        }

        #[test]
        fn check_battery_warning_zero_percent_is_false() {
            let mut state = ControllerState::default();
            state.battery_percent = 0;
            state.charging = false;
            assert!(!telemetry::TelemetryExtractor::check_battery_warning(
                &state, 15
            ));
        }

        #[test]
        fn check_battery_warning_full_is_false() {
            let mut state = ControllerState::default();
            state.battery_percent = 100;
            state.charging = false;
            assert!(!telemetry::TelemetryExtractor::check_battery_warning(
                &state, 15
            ));
        }

        // --- update_from_standard_report --------------------------------

        #[test]
        fn update_from_standard_report_updates_state() {
            let data = build_full_press_standard_report();
            let mut state = ControllerState::default();
            assert!(!state.connected);

            let parsed =
                telemetry::TelemetryExtractor::update_from_standard_report(&mut state, &data);

            assert!(parsed.is_some());
            assert!(state.connected, "state should be marked connected");
            assert!(state.buttons.a, "buttons should be updated");
            assert!(state.buttons.y);
            assert_eq!(state.left_stick.raw_x, 0x800);
            assert!(state.timestamp > 0, "timestamp should be set");
        }

        #[test]
        fn update_from_standard_report_invalid_returns_none() {
            let data = vec![0x00, 0x01, 0x02]; // too short, wrong ID
            let mut state = ControllerState::default();
            let result =
                telemetry::TelemetryExtractor::update_from_standard_report(&mut state, &data);
            assert!(result.is_none());
            assert!(
                !state.connected,
                "state should remain disconnected on failure"
            );
        }

        // --- update_from_subcmd_reply -----------------------------------

        #[test]
        fn update_from_subcmd_reply_updates_battery() {
            // raw=4: level=(4>>1)=2 → low=25%, bit 0=0 → not charging
            let data = build_subcmd_reply(4, 1, 0, 0);
            let mut state = ControllerState::default();
            assert_eq!(state.battery_percent, 0);

            let reply = telemetry::TelemetryExtractor::update_from_subcmd_reply(&mut state, &data);

            assert!(reply.is_some());
            assert!(state.connected);
            assert_eq!(state.battery_raw, 4);
            assert_eq!(state.battery_percent, 25);
            assert!(!state.charging);
            assert!(state.timestamp > 0);
        }

        #[test]
        fn update_from_subcmd_reply_charging() {
            // raw=9: level=(9>>1)=4 → full=100%, bit 0=1 → charging
            let data = build_subcmd_reply(9, 2, 0, 0);
            let mut state = ControllerState::default();
            let reply = telemetry::TelemetryExtractor::update_from_subcmd_reply(&mut state, &data);
            assert!(reply.is_some());
            assert_eq!(state.battery_raw, 9);
            assert_eq!(state.battery_percent, 100);
            assert!(state.charging);
        }

        #[test]
        fn update_from_subcmd_reply_invalid_returns_none() {
            let data = vec![0x30, 0x00]; // wrong ID, too short
            let mut state = ControllerState::default();
            let result = telemetry::TelemetryExtractor::update_from_subcmd_reply(&mut state, &data);
            assert!(result.is_none());
            assert!(!state.connected);
        }

        // --- update_signal_strength -------------------------------------

        #[test]
        fn update_signal_strength_sets_value() {
            let mut state = ControllerState::default();
            telemetry::TelemetryExtractor::update_signal_strength(&mut state, -42);
            assert_eq!(state.signal_strength, -42);
        }

        // --- IMU propagation --------------------------------------------

        #[test]
        fn update_from_standard_report_propagates_imu() {
            // Build a 49-byte 0x30 report with IMU data using the mock generator.
            let mock_gen = mock::MockGenerator::new();
            let data = mock_gen.build_imu_standard_report();
            assert_eq!(data.len(), 49);

            let mut state = ControllerState::default();
            telemetry::TelemetryExtractor::update_from_standard_report(&mut state, &data);

            assert!(
                state.imu.is_some(),
                "state.imu should be set after update with IMU report"
            );
            let imu = state.imu.as_ref().unwrap();
            // Frame 0 accel_z should be 4096 (gravity) per mock generator.
            assert_eq!(imu.frames[0].accel_z, 4096);
        }

        // --- apply_stick_calibration ------------------------------------

        #[test]
        fn apply_stick_calibration_max() {
            let cal = state::StickCalibration {
                left_center_x: 0x800,
                left_center_y: 0x800,
                left_min_x: 0x200,
                left_min_y: 0x200,
                left_max_x: 0xE00,
                left_max_y: 0xE00,
                ..Default::default()
            };
            let (x, _y) =
                telemetry::TelemetryExtractor::apply_stick_calibration(0xE00, 0x800, &cal, true);
            assert!(
                (x - 1.0).abs() < 0.001,
                "raw at max should normalize to 1.0, got {}",
                x
            );
        }

        #[test]
        fn apply_stick_calibration_center() {
            let cal = state::StickCalibration {
                left_center_x: 0x800,
                left_center_y: 0x800,
                left_min_x: 0x200,
                left_min_y: 0x200,
                left_max_x: 0xE00,
                left_max_y: 0xE00,
                ..Default::default()
            };
            let (x, y) =
                telemetry::TelemetryExtractor::apply_stick_calibration(0x800, 0x800, &cal, true);
            assert!(
                (x - 0.0).abs() < 0.001,
                "raw at center should normalize to 0.0, got {}",
                x
            );
            assert!(
                (y - 0.0).abs() < 0.001,
                "raw at center should normalize to 0.0, got {}",
                y
            );
        }

        #[test]
        fn apply_stick_calibration_min() {
            let cal = state::StickCalibration {
                left_center_x: 0x800,
                left_center_y: 0x800,
                left_min_x: 0x200,
                left_min_y: 0x200,
                left_max_x: 0xE00,
                left_max_y: 0xE00,
                ..Default::default()
            };
            let (x, _y) =
                telemetry::TelemetryExtractor::apply_stick_calibration(0x200, 0x800, &cal, true);
            assert!(
                (x - (-1.0)).abs() < 0.001,
                "raw at min should normalize to -1.0, got {}",
                x
            );
        }

        // --- update_from_device_info / calibration / lights -------------

        #[test]
        fn update_from_device_info_stores() {
            let mut state = ControllerState::default();
            assert!(state.device_info.is_none());
            let info = state::DeviceInfo {
                firmware_version: "3.72".into(),
                controller_type: 0x03,
                mac_address: "BB:8A:EA:30:57:01".into(),
                colors_from_spi: true,
                connection: "Bluetooth".into(),
                spi: None,
            };
            telemetry::TelemetryExtractor::update_from_device_info(&mut state, info.clone());
            assert!(
                state.device_info.is_some(),
                "device_info should be Some after update"
            );
            let stored = state.device_info.as_ref().unwrap();
            assert_eq!(stored.firmware_version, "3.72");
            assert_eq!(stored.controller_type, 0x03);
        }

        #[test]
        fn update_from_calibration_stores() {
            let mut state = ControllerState::default();
            assert!(state.stick_calibration.is_none());
            let cal = state::StickCalibration {
                left_center_x: 0x800,
                left_max_x: 0xE00,
                ..Default::default()
            };
            telemetry::TelemetryExtractor::update_from_calibration(&mut state, cal.clone());
            assert!(
                state.stick_calibration.is_some(),
                "stick_calibration should be Some after update"
            );
            let stored = state.stick_calibration.as_ref().unwrap();
            assert_eq!(stored.left_center_x, 0x800);
            assert_eq!(stored.left_max_x, 0xE00);
        }

        #[test]
        fn update_player_lights_stores() {
            let mut state = ControllerState::default();
            assert_eq!(state.player_lights.led_mask, 0);
            assert_eq!(state.player_lights.flash_pattern, 0);
            telemetry::TelemetryExtractor::update_player_lights(&mut state, 0b1111, 0x10);
            assert_eq!(state.player_lights.led_mask, 0b1111);
            assert_eq!(state.player_lights.flash_pattern, 0x10);
        }

        #[test]
        fn update_home_light_stores() {
            let mut state = ControllerState::default();
            assert!(!state.home_light.enabled);
            telemetry::TelemetryExtractor::update_home_light(&mut state, true, 15, 0x01);
            assert!(state.home_light.enabled);
            assert_eq!(state.home_light.brightness, 15);
            assert_eq!(state.home_light.pulse_pattern, 0x01);
        }
    }

    // ===================================================================
    //  state tests
    // ===================================================================

    mod state_tests {
        use super::*;

        #[test]
        fn app_config_default_values() {
            let config = AppConfig::default();
            assert!(
                (config.deadzone_left - 0.08).abs() < f32::EPSILON,
                "deadzone_left should be 0.08"
            );
            assert!(
                (config.deadzone_right - 0.08).abs() < f32::EPSILON,
                "deadzone_right should be 0.08"
            );
            assert_eq!(config.keepalive_interval_ms, 3000);
            assert!(
                config.adaptive_keepalive,
                "adaptive_keepalive should be true"
            );
            assert_eq!(config.battery_warning_threshold, 15);
            assert!(!config.mock_mode, "mock_mode should be false by default");
        }

        #[test]
        fn app_config_default_remap() {
            let config = AppConfig::default();
            assert_eq!(config.button_remap.a_to, "b");
            assert_eq!(config.button_remap.b_to, "a");
            assert_eq!(config.button_remap.x_to, "y");
            assert_eq!(config.button_remap.y_to, "x");
        }

        #[test]
        fn timestamp_now_nonzero() {
            let ts = timestamp_now();
            assert!(ts > 0, "timestamp_now should return a non-zero value");
        }

        #[test]
        fn timestamp_now_increases() {
            let t1 = timestamp_now();
            // Small busy-wait to ensure time advances.
            let mut acc: u64 = 0;
            for i in 0..1_000_000 {
                acc = acc.wrapping_add(i);
            }
            let t2 = timestamp_now();
            assert!(
                t2 >= t1,
                "timestamp should be monotonically non-decreasing ({}, {})",
                t1,
                t2
            );
            // Prevent optimizer from removing the loop.
            assert!(acc != 0xDEAD_BEEF || acc == 0xDEAD_BEEF);
        }

        #[test]
        fn controller_state_default_values() {
            let state = ControllerState::default();
            assert!(!state.connected);
            assert_eq!(state.battery_percent, 0);
            assert_eq!(state.battery_raw, 0);
            assert!(!state.charging);
            assert_eq!(state.signal_strength, -60);
            assert_eq!(state.timestamp, 0);
        }

        #[test]
        fn button_state_default_all_false() {
            let b = ButtonState::default();
            assert!(!b.a && !b.b && !b.x && !b.y);
            assert!(!b.l && !b.r && !b.zl && !b.zr);
            assert!(!b.minus && !b.plus && !b.home && !b.capture);
            assert!(!b.stick_l && !b.stick_r);
            assert!(!b.dpad_up && !b.dpad_down && !b.dpad_left && !b.dpad_right);
        }

        #[test]
        fn stick_state_default_zeros() {
            let s = StickState::default();
            assert!((s.x - 0.0).abs() < f32::EPSILON);
            assert!((s.y - 0.0).abs() < f32::EPSILON);
            assert_eq!(s.raw_x, 0);
            assert_eq!(s.raw_y, 0);
        }

        #[test]
        fn keep_alive_status_default() {
            let ka = state::KeepAliveStatus::default();
            assert!(!ka.active);
            assert_eq!(ka.interval_ms, 3000);
            assert_eq!(ka.last_ping, 0);
            assert_eq!(ka.power_events_detected, 0);
            assert!(!ka.adapter_sleep_prevented);
            assert!(ka.adaptive_mode);
        }

        // --- IpcEvent serialization -------------------------------------

        #[test]
        fn ipc_event_controller_state_serialization_tag() {
            let event = IpcEvent::ControllerState {
                data: ControllerState::default(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"ControllerState\""),
                "JSON should contain type tag: {}",
                json
            );
            assert!(
                json.contains("\"data\""),
                "JSON should contain data field: {}",
                json
            );
        }

        #[test]
        fn ipc_event_keepalive_status_serialization_tag() {
            let event = IpcEvent::KeepAliveStatus {
                data: state::KeepAliveStatus::default(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"KeepAliveStatus\""),
                "JSON should contain type tag: {}",
                json
            );
        }

        #[test]
        fn ipc_event_config_updated_serialization_tag() {
            let event = IpcEvent::ConfigUpdated {
                data: AppConfig::default(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"ConfigUpdated\""),
                "JSON should contain type tag: {}",
                json
            );
        }

        #[test]
        fn ipc_event_battery_warning_serialization() {
            let event = IpcEvent::BatteryWarning { percent: 10 };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"BatteryWarning\""), "{}", json);
            assert!(json.contains("\"percent\":10"), "{}", json);
        }

        #[test]
        fn ipc_event_disconnected_serialization() {
            let event = IpcEvent::Disconnected {
                reason: "timeout".into(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"Disconnected\""), "{}", json);
            assert!(json.contains("\"reason\":\"timeout\""), "{}", json);
        }

        #[test]
        fn ipc_event_reconnected_serialization() {
            let event = IpcEvent::Reconnected;
            let json = serde_json::to_string(&event).expect("should serialize");
            assert_eq!(json, "{\"type\":\"Reconnected\"}");
        }

        #[test]
        fn ipc_event_bluetooth_power_event_serialization() {
            let event = IpcEvent::BluetoothPowerEvent {
                event_type: "Power_Down".into(),
                timestamp: 12345,
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"BluetoothPowerEvent\""),
                "{}",
                json
            );
            assert!(json.contains("\"event_type\":\"Power_Down\""), "{}", json);
            assert!(json.contains("\"timestamp\":12345"), "{}", json);
        }

        #[test]
        fn ipc_event_raw_hid_report_serialization() {
            let event = IpcEvent::RawHidReport {
                hex: "30 00".into(),
                report_id: 0x30,
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"RawHidReport\""), "{}", json);
            assert!(json.contains("\"hex\":\"30 00\""), "{}", json);
            assert!(json.contains("\"report_id\":48"), "{}", json);
        }

        #[test]
        fn ipc_event_log_message_serialization() {
            let event = IpcEvent::LogMessage {
                level: "info".into(),
                message: "hello".into(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"LogMessage\""), "{}", json);
            assert!(json.contains("\"level\":\"info\""), "{}", json);
            assert!(json.contains("\"message\":\"hello\""), "{}", json);
        }

        #[test]
        fn ipc_event_controller_state_roundtrip() {
            let original = IpcEvent::ControllerState {
                data: ControllerState {
                    connected: true,
                    battery_percent: 75,
                    battery_raw: 6,
                    charging: false,
                    signal_strength: -50,
                    buttons: ButtonState::default(),
                    left_stick: StickState::default(),
                    right_stick: StickState::default(),
                    timestamp: 999,
                    ..Default::default()
                },
            };
            let json = serde_json::to_string(&original).expect("should serialize");
            let deserialized: IpcEvent = serde_json::from_str(&json).expect("should deserialize");
            if let IpcEvent::ControllerState { data } = deserialized {
                assert!(data.connected);
                assert_eq!(data.battery_percent, 75);
                assert_eq!(data.timestamp, 999);
            } else {
                panic!("deserialized event should be ControllerState variant");
            }
        }

        // --- SharedState ------------------------------------------------

        #[test]
        fn shared_state_new_initializes_defaults() {
            let shared = state::SharedState::new();
            {
                let ctrl = shared.active_controller();
                assert!(!ctrl.connected);
            }
            {
                let ka = shared.keepalive.read();
                assert!(!ka.active);
            }
            {
                let cfg = shared.config.read();
                assert!(!cfg.mock_mode, "mock_mode should be false by default");
                assert_eq!(cfg.keepalive_interval_ms, 3000);
            }
        }

        // --- New state type defaults ------------------------------------

        #[test]
        fn connection_type_default_bluetooth() {
            assert_eq!(
                state::ConnectionType::default(),
                state::ConnectionType::Bluetooth
            );
        }

        #[test]
        fn device_info_default_empty() {
            let info = state::DeviceInfo::default();
            assert!(info.firmware_version.is_empty());
            assert_eq!(info.controller_type, 0);
            assert!(info.mac_address.is_empty());
            assert!(!info.colors_from_spi);
        }

        #[test]
        fn player_lights_default_zero() {
            let lights = state::PlayerLights::default();
            assert_eq!(lights.led_mask, 0);
            assert_eq!(lights.flash_pattern, 0);
        }

        #[test]
        fn controller_state_new_fields_default() {
            let state = ControllerState::default();
            assert!(state.imu.is_none(), "imu should default to None");
            assert!(
                state.device_info.is_none(),
                "device_info should default to None"
            );
            assert!(
                state.stick_calibration.is_none(),
                "stick_calibration should default to None"
            );
            assert!(!state.imu_enabled, "imu_enabled should default to false");
            assert!(
                !state.vibration_enabled,
                "vibration_enabled should default to false"
            );
        }

        // --- ControllerState serialization with new fields --------------

        #[test]
        fn controller_state_with_imu_serialization() {
            let mut state = ControllerState::default();
            state.imu = Some(hid_parser::ImuData {
                frames: [
                    hid_parser::ImuFrame {
                        accel_x: 100,
                        accel_y: 200,
                        accel_z: 4096,
                        gyro_x: 10,
                        gyro_y: 20,
                        gyro_z: 30,
                    },
                    hid_parser::ImuFrame::default(),
                    hid_parser::ImuFrame::default(),
                ],
            });
            state.imu_enabled = true;

            let json = serde_json::to_string(&state).expect("should serialize");
            let deserialized: ControllerState =
                serde_json::from_str(&json).expect("should deserialize");
            assert!(deserialized.imu.is_some(), "imu should survive roundtrip");
            assert!(
                deserialized.imu_enabled,
                "imu_enabled should survive roundtrip"
            );
            let imu = deserialized.imu.unwrap();
            assert_eq!(imu.frames[0].accel_z, 4096);
        }

        #[test]
        fn controller_state_with_device_info_serialization() {
            let mut state = ControllerState::default();
            state.device_info = Some(state::DeviceInfo {
                firmware_version: "3.72".into(),
                controller_type: 0x03,
                mac_address: "BB:8A:EA:30:57:01".into(),
                colors_from_spi: true,
                connection: "Bluetooth".into(),
                spi: None,
            });

            let json = serde_json::to_string(&state).expect("should serialize");
            let deserialized: ControllerState =
                serde_json::from_str(&json).expect("should deserialize");
            assert!(
                deserialized.device_info.is_some(),
                "device_info should survive roundtrip"
            );
            let info = deserialized.device_info.unwrap();
            assert_eq!(info.firmware_version, "3.72");
            assert_eq!(info.mac_address, "BB:8A:EA:30:57:01");
        }

        // --- New IpcEvent serialization variants ------------------------

        #[test]
        fn ipc_event_device_info_serialization() {
            let event = IpcEvent::DeviceInfo {
                data: state::DeviceInfo {
                    firmware_version: "3.72".into(),
                    controller_type: 0x03,
                    mac_address: "BB:8A:EA:30:57:01".into(),
                    colors_from_spi: true,
                    connection: "Bluetooth".into(),
                    spi: None,
                },
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"DeviceInfo\""),
                "JSON should contain DeviceInfo tag: {}",
                json
            );
        }

        #[test]
        fn ipc_event_imu_data_serialization() {
            let event = IpcEvent::ImuData {
                frames: hid_parser::ImuData::default(),
                timestamp: 12345,
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"ImuData\""),
                "JSON should contain ImuData tag: {}",
                json
            );
            assert!(
                json.contains("\"timestamp\":12345"),
                "JSON should contain timestamp: {}",
                json
            );
        }

        #[test]
        fn ipc_event_calibration_data_serialization() {
            let event = IpcEvent::CalibrationData {
                stick: state::StickCalibration::default(),
                imu: state::ImuCalibration::default(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"CalibrationData\""),
                "JSON should contain CalibrationData tag: {}",
                json
            );
        }

        #[test]
        fn ipc_event_player_lights_changed_serialization() {
            let event = IpcEvent::PlayerLightsChanged {
                mask: 0b1111,
                pattern: 0x10,
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"PlayerLightsChanged\""),
                "JSON should contain PlayerLightsChanged tag: {}",
                json
            );
            assert!(json.contains("\"mask\":15"), "{}", json);
            assert!(json.contains("\"pattern\":16"), "{}", json);
        }

        #[test]
        fn ipc_event_home_light_changed_serialization() {
            let event = IpcEvent::HomeLightChanged {
                enabled: true,
                brightness: 15,
                pattern: 0x01,
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"HomeLightChanged\""),
                "JSON should contain HomeLightChanged tag: {}",
                json
            );
            assert!(json.contains("\"enabled\":true"), "{}", json);
            assert!(json.contains("\"brightness\":15"), "{}", json);
        }

        #[test]
        fn ipc_event_subcommand_reply_serialization() {
            let event = IpcEvent::SubcommandReply {
                subcmd_id: 0x02,
                ack: 0x80,
                data: vec![0x01, 0x02, 0x03],
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(
                json.contains("\"type\":\"SubcommandReply\""),
                "JSON should contain SubcommandReply tag: {}",
                json
            );
            assert!(json.contains("\"subcmd_id\":2"), "{}", json);
            assert!(json.contains("\"ack\":128"), "{}", json);
        }

        // --- SharedState packet_number ----------------------------------

        #[test]
        fn shared_state_next_packet_number_increments() {
            let shared = state::SharedState::new();
            assert_eq!(shared.next_packet_number(), 1);
            assert_eq!(shared.next_packet_number(), 2);
            assert_eq!(shared.next_packet_number(), 3);
        }

        #[test]
        fn shared_state_packet_number_wraps_at_15() {
            let shared = state::SharedState::new();
            // next_packet_number returns (n+1) & 0x0F, so:
            //   call 1 → 1, call 2 → 2, ... call 15 → 15, call 16 → 0
            for _ in 0..15 {
                shared.next_packet_number();
            }
            // After 15 calls, the 16th should wrap to 0.
            let n = shared.next_packet_number();
            assert_eq!(
                n, 0,
                "packet_number should wrap to 0 after 16 calls, got {}",
                n
            );
        }
    }

    // ===================================================================
    //  Cross-module integration tests
    // ===================================================================

    mod integration_tests {
        use super::*;

        #[test]
        fn full_pipeline_standard_report_to_xinput() {
            // Build a standard report with A pressed, left stick at max.
            let mut data = vec![0u8; 12];
            data[0] = 0x30;
            data[3] = 0x04; // A button (bit 2 in empirical layout)
                            // Left stick at max
            let stick = encode_stick(0xFFF, 0x800);
            data[6] = stick[0];
            data[7] = stick[1];
            data[8] = stick[2];
            // Right stick centered
            let rstick = encode_stick(0x800, 0x800);
            data[9] = rstick[0];
            data[10] = rstick[1];
            data[11] = rstick[2];

            let mut state = ControllerState::default();
            telemetry::TelemetryExtractor::update_from_standard_report(&mut state, &data);

            assert!(state.buttons.a);

            // Apply default remap (A→B) then map to xinput.
            let remap = xinput::default_nintendo_to_xinput_remap();
            telemetry::TelemetryExtractor::apply_remap(&mut state.buttons, &remap);

            // After remap, A is false, B is true.
            assert!(!state.buttons.a);
            assert!(state.buttons.b);

            // Apply deadzone.
            telemetry::TelemetryExtractor::apply_deadzone(&mut state.left_stick, 0.08);

            let xi = xinput::map_to_xinput(
                &state.buttons,
                &state.left_stick,
                &state.right_stick,
                0.0,
                0.0,
            );
            // B should be mapped to XINPUT_GAMEPAD_B.
            assert_eq!(
                xi.buttons & xinput::XINPUT_GAMEPAD_B,
                xinput::XINPUT_GAMEPAD_B
            );
            // A should NOT be set.
            assert_eq!(xi.buttons & xinput::XINPUT_GAMEPAD_A, 0);
            // Left stick X should be max.
            assert_eq!(xi.thumb_lx, 32767);
        }

        #[test]
        fn mock_generator_builds_valid_reports() {
            let mock_gen = mock::MockGenerator::new();
            let std_report = mock_gen.build_standard_report();
            assert_eq!(std_report.len(), 12);
            assert_eq!(std_report[0], 0x30);

            // Should parse successfully.
            let parsed = hid_parser::parse_standard_report(&std_report);
            assert!(parsed.is_some(), "mock standard report should parse");

            let sub_reply = mock_gen.build_subcmd_reply();
            assert_eq!(sub_reply.len(), 15);
            assert_eq!(sub_reply[0], 0x21);

            let reply = hid_parser::parse_subcmd_reply(&sub_reply);
            assert!(reply.is_some(), "mock subcmd reply should parse");
        }

        #[test]
        fn mock_generator_build_controller_state_pipeline() {
            let mock_gen = mock::MockGenerator::new();
            let config = AppConfig::default();
            let (state, hex, report_id) = mock_gen.build_controller_state(&config);

            assert!(state.connected);
            assert!(state.timestamp > 0);
            assert!(!hex.is_empty(), "hex string should not be empty");
            assert_eq!(report_id, 0x30);
            // Battery should have been set from subcmd reply.
            assert!(state.battery_percent <= 100);
        }

        #[test]
        fn bthusb_snapshot_default() {
            let snap = bthusb_monitor::BthUsbSnapshot::default();
            assert_eq!(snap.power_down_events, 0);
            assert_eq!(snap.disconnect_events, 0);
            assert_eq!(snap.link_key_faults, 0);
            assert_eq!(snap.last_event_ts, 0);
            assert_eq!(snap.last_event_id, 0);
        }

        #[test]
        fn bthusb_detect_new_power_down_logic() {
            let monitor = bthusb_monitor::BthUsbMonitor::new();
            let snap = bthusb_monitor::BthUsbSnapshot {
                power_down_events: 5,
                disconnect_events: 0,
                link_key_faults: 0,
                last_event_ts: 0,
                last_event_id: 5,
            };
            assert!(
                monitor.detect_new_power_down(&snap, 3),
                "5 > 3 should detect"
            );
            assert!(
                !monitor.detect_new_power_down(&snap, 5),
                "5 == 5 should not detect"
            );
            assert!(
                !monitor.detect_new_power_down(&snap, 10),
                "5 < 10 should not detect"
            );
        }

        #[test]
        fn bthusb_event_id_constants() {
            assert_eq!(bthusb_monitor::EVT_HCI_SIZE_MISMATCH, 5);
            assert_eq!(bthusb_monitor::EVT_REMOTE_UNPAIRED, 10);
            assert_eq!(bthusb_monitor::EVT_LINK_KEY_STORE_FAIL, 18);
        }

        #[test]
        fn keepalive_check_bthusb_power_state_no_env() {
            // Without OXIDELINK_SIMULATE_POWER_DOWN set, the function queries
            // the Bluetooth radio status via PowerShell. The result depends on
            // the system state, so we just verify it returns a Vec without
            // panicking.
            let events = keepalive::check_bthusb_power_state();
            // Should return at most a few event descriptions.
            assert!(events.len() <= 2);
        }

        // --- Mock generator expanded builders ----------------------------

        #[test]
        fn mock_build_imu_standard_report() {
            let mock_gen = mock::MockGenerator::new();
            let report = mock_gen.build_imu_standard_report();
            assert_eq!(report.len(), 49, "IMU standard report should be 49 bytes");
            assert_eq!(report[0], 0x30, "report ID should be 0x30");
            // Should parse successfully and have IMU data.
            let parsed = hid_parser::parse_standard_report(&report).expect("should parse");
            assert!(
                parsed.imu.is_some(),
                "parsed IMU standard report should contain IMU data"
            );
        }

        #[test]
        fn mock_build_device_info_reply() {
            let mock_gen = mock::MockGenerator::new();
            let reply = mock_gen.build_device_info_reply();
            assert_eq!(reply[0], 0x21, "report ID should be 0x21");
            assert_eq!(reply[14], 0x02, "subcmd ID should be 0x02 (device info)");
            // Should parse as a subcommand reply.
            let parsed = hid_parser::parse_subcmd_reply(&reply).expect("should parse");
            assert_eq!(parsed.subcmd_id, 0x02);
            // The reply data (12 bytes) should be parseable as DeviceInfo.
            let info = subcmd::parse_device_info_reply(&parsed.reply_data);
            assert!(info.is_some(), "device info reply data should parse");
            let info = info.unwrap();
            assert_eq!(info.controller_type, 0x03);
            assert!(info.colors_from_spi);
        }

        #[test]
        fn mock_build_spi_flash_reply() {
            let mock_gen = mock::MockGenerator::new();
            let reply = mock_gen.build_spi_flash_reply(0x6080, 18);
            assert_eq!(reply[0], 0x21, "report ID should be 0x21");
            assert_eq!(reply[14], 0x10, "subcmd ID should be 0x10 (SPI flash read)");
            // Total size: 15-byte header + 18-byte payload = 33
            assert_eq!(
                reply.len(),
                33,
                "SPI flash reply should be 15 + 18 = 33 bytes"
            );
            // Should parse as subcommand reply.
            let parsed = hid_parser::parse_subcmd_reply(&reply).expect("should parse");
            assert_eq!(parsed.subcmd_id, 0x10);
            // The 18-byte payload should parse as stick calibration.
            let cal = subcmd::parse_stick_calibration_reply(&parsed.reply_data);
            assert!(cal.is_some(), "SPI flash calibration data should parse");
        }

        #[test]
        fn mock_build_player_lights_reply() {
            let mock_gen = mock::MockGenerator::new();
            let reply = mock_gen.build_player_lights_reply(0b1111);
            assert_eq!(reply[0], 0x21, "report ID should be 0x21");
            assert_eq!(reply[14], 0x30, "subcmd ID should be 0x30 (player lights)");
            assert_eq!(
                reply.len(),
                15,
                "player lights reply should be 15 bytes (no payload)"
            );
            // ACK byte should have MSB set.
            assert_eq!(reply[13] & 0x80, 0x80, "ACK byte should have MSB set");
        }

        #[test]
        fn mock_build_enable_imu_reply() {
            let mock_gen = mock::MockGenerator::new();
            let reply = mock_gen.build_enable_imu_reply();
            assert_eq!(reply[0], 0x21, "report ID should be 0x21");
            assert_eq!(reply[14], 0x40, "subcmd ID should be 0x40 (enable IMU)");
            assert_eq!(
                reply.len(),
                15,
                "enable IMU reply should be 15 bytes (no payload)"
            );
            assert_eq!(reply[13] & 0x80, 0x80, "ACK byte should have MSB set");
        }

        #[test]
        fn mock_build_enable_vibration_reply() {
            let mock_gen = mock::MockGenerator::new();
            let reply = mock_gen.build_enable_vibration_reply();
            assert_eq!(reply[0], 0x21, "report ID should be 0x21");
            assert_eq!(
                reply[14], 0x48,
                "subcmd ID should be 0x48 (enable vibration)"
            );
            assert_eq!(
                reply.len(),
                15,
                "enable vibration reply should be 15 bytes (no payload)"
            );
            assert_eq!(reply[13] & 0x80, 0x80, "ACK byte should have MSB set");
        }
    }

    // ===================================================================
    //  IMU tests
    // ===================================================================

    mod imu_tests {
        use super::*;

        #[test]
        fn imu_physical_default_all_zeros() {
            let phys = imu::ImuPhysical::default();
            assert!((phys.accel_x - 0.0).abs() < f32::EPSILON);
            assert!((phys.accel_y - 0.0).abs() < f32::EPSILON);
            assert!((phys.accel_z - 0.0).abs() < f32::EPSILON);
            assert!((phys.gyro_x - 0.0).abs() < f32::EPSILON);
            assert!((phys.gyro_y - 0.0).abs() < f32::EPSILON);
            assert!((phys.gyro_z - 0.0).abs() < f32::EPSILON);
        }

        #[test]
        fn raw_to_physical_accel_one_g() {
            let frame = hid_parser::ImuFrame {
                accel_x: 4096,
                accel_y: 0,
                accel_z: 0,
                gyro_x: 0,
                gyro_y: 0,
                gyro_z: 0,
            };
            let phys = imu::raw_to_physical(&frame);
            assert!(
                (phys.accel_x - 1.0).abs() < 0.001,
                "accel_x=4096 should be ~1.0g, got {}",
                phys.accel_x
            );
            assert!(
                (phys.accel_y - 0.0).abs() < 0.001,
                "accel_y=0 should be 0.0g"
            );
        }

        #[test]
        fn raw_to_physical_gyro_one_deg_per_s() {
            let frame = hid_parser::ImuFrame {
                accel_x: 0,
                accel_y: 0,
                accel_z: 0,
                gyro_x: 13371,
                gyro_y: 0,
                gyro_z: 0,
            };
            let phys = imu::raw_to_physical(&frame);
            assert!(
                (phys.gyro_x - 1.0).abs() < 0.001,
                "gyro_x=13371 should be ~1.0 deg/s, got {}",
                phys.gyro_x
            );
        }

        #[test]
        fn calculate_tilt_level() {
            // Controller level: gravity along Z → pitch=0, roll=0
            let accel = imu::ImuPhysical {
                accel_x: 0.0,
                accel_y: 0.0,
                accel_z: 1.0,
                gyro_x: 0.0,
                gyro_y: 0.0,
                gyro_z: 0.0,
            };
            let (pitch, roll) = imu::calculate_tilt(&accel);
            assert!(
                (pitch - 0.0).abs() < 0.1,
                "level pitch should be ~0, got {}",
                pitch
            );
            assert!(
                (roll - 0.0).abs() < 0.1,
                "level roll should be ~0, got {}",
                roll
            );
        }

        #[test]
        fn calculate_tilt_pitch_90() {
            // Gravity along Y → pitch=90
            let accel = imu::ImuPhysical {
                accel_x: 0.0,
                accel_y: 1.0,
                accel_z: 0.0,
                gyro_x: 0.0,
                gyro_y: 0.0,
                gyro_z: 0.0,
            };
            let (pitch, _roll) = imu::calculate_tilt(&accel);
            assert!(
                (pitch - 90.0).abs() < 0.1,
                "pitch should be ~90 when gravity is along Y, got {}",
                pitch
            );
        }

        #[test]
        fn calculate_tilt_roll_90() {
            // Gravity along -X → roll=90
            let accel = imu::ImuPhysical {
                accel_x: -1.0,
                accel_y: 0.0,
                accel_z: 0.0,
                gyro_x: 0.0,
                gyro_y: 0.0,
                gyro_z: 0.0,
            };
            let (_pitch, roll) = imu::calculate_tilt(&accel);
            assert!(
                (roll - 90.0).abs() < 0.1,
                "roll should be ~90 when gravity is along -X, got {}",
                roll
            );
        }

        #[test]
        fn tilt_estimator_new_and_get_tilt() {
            let estimator = imu::TiltEstimator::new(0.98);
            let (pitch, roll) = estimator.get_tilt();
            assert!((pitch - 0.0).abs() < f32::EPSILON);
            assert!((roll - 0.0).abs() < f32::EPSILON);
        }

        #[test]
        fn tilt_estimator_update_and_reset() {
            let mut estimator = imu::TiltEstimator::new(0.98);
            let accel = imu::ImuPhysical {
                accel_x: 0.0,
                accel_y: 1.0,
                accel_z: 0.0,
                gyro_x: 0.0,
                gyro_y: 0.0,
                gyro_z: 0.0,
            };
            // dt = 1/180
            estimator.update(&accel, &accel, 1.0 / 180.0);
            let (pitch, _roll) = estimator.get_tilt();
            // After one update with accel showing pitch=90, pitch should be non-zero.
            assert!(
                pitch.abs() > 0.0,
                "pitch should be non-zero after update, got {}",
                pitch
            );
            // Reset.
            estimator.reset();
            let (pitch2, roll2) = estimator.get_tilt();
            assert!(
                (pitch2 - 0.0).abs() < f32::EPSILON,
                "pitch should be 0 after reset"
            );
            assert!(
                (roll2 - 0.0).abs() < f32::EPSILON,
                "roll should be 0 after reset"
            );
        }

        #[test]
        fn calibrate_gyro_bias_empty() {
            let samples: [hid_parser::ImuFrame; 0] = [];
            let bias = imu::calibrate_gyro_bias(&samples);
            assert_eq!(bias, [0, 0, 0]);
        }

        #[test]
        fn calibrate_gyro_bias_stationary() {
            let samples = [
                hid_parser::ImuFrame {
                    accel_x: 0,
                    accel_y: 0,
                    accel_z: 4096,
                    gyro_x: 10,
                    gyro_y: 20,
                    gyro_z: 30,
                },
                hid_parser::ImuFrame {
                    accel_x: 0,
                    accel_y: 0,
                    accel_z: 4096,
                    gyro_x: 12,
                    gyro_y: 18,
                    gyro_z: 32,
                },
            ];
            let bias = imu::calibrate_gyro_bias(&samples);
            // Average: (10+12)/2=11, (20+18)/2=19, (30+32)/2=31
            assert_eq!(bias[0], 11);
            assert_eq!(bias[1], 19);
            assert_eq!(bias[2], 31);
        }

        #[test]
        fn gyro_aim_config_default_disabled() {
            let config = imu::GyroAimConfig::default();
            assert!(!config.enabled, "gyro aim should be disabled by default");
        }

        #[test]
        fn map_gyro_to_stick_disabled() {
            let config = imu::GyroAimConfig::default(); // enabled = false
            let gyro = imu::ImuPhysical {
                accel_x: 0.0,
                accel_y: 0.0,
                accel_z: 0.0,
                gyro_x: 100.0,
                gyro_y: 100.0,
                gyro_z: 0.0,
            };
            let (x, y) = imu::map_gyro_to_stick(&gyro, &config);
            assert!(
                (x - 0.0).abs() < f32::EPSILON,
                "disabled gyro should map to 0"
            );
            assert!(
                (y - 0.0).abs() < f32::EPSILON,
                "disabled gyro should map to 0"
            );
        }

        #[test]
        fn map_gyro_to_stick_deadzone() {
            let config = imu::GyroAimConfig {
                enabled: true,
                sensitivity: 0.01,
                deadzone: 10.0,
            };
            // Gyro rates below deadzone → (0, 0)
            let gyro = imu::ImuPhysical {
                accel_x: 0.0,
                accel_y: 0.0,
                accel_z: 0.0,
                gyro_x: 5.0,
                gyro_y: 5.0,
                gyro_z: 0.0,
            };
            let (x, y) = imu::map_gyro_to_stick(&gyro, &config);
            assert!(
                (x - 0.0).abs() < f32::EPSILON,
                "below deadzone should map to 0"
            );
            assert!(
                (y - 0.0).abs() < f32::EPSILON,
                "below deadzone should map to 0"
            );
        }

        #[test]
        fn map_gyro_to_stick_active() {
            let config = imu::GyroAimConfig {
                enabled: true,
                sensitivity: 0.01,
                deadzone: 2.0,
            };
            let gyro = imu::ImuPhysical {
                accel_x: 0.0,
                accel_y: 0.0,
                accel_z: 0.0,
                gyro_x: 100.0,
                gyro_y: 100.0,
                gyro_z: 0.0,
            };
            let (x, y) = imu::map_gyro_to_stick(&gyro, &config);
            assert!(x.abs() > 0.0, "active gyro_y should produce non-zero x");
            assert!(y.abs() > 0.0, "active gyro_x should produce non-zero y");
        }

        #[test]
        fn map_gyro_to_stick_clamped() {
            let config = imu::GyroAimConfig {
                enabled: true,
                sensitivity: 1.0, // very high sensitivity
                deadzone: 2.0,
            };
            let gyro = imu::ImuPhysical {
                accel_x: 0.0,
                accel_y: 0.0,
                accel_z: 0.0,
                gyro_x: 1000.0,
                gyro_y: 1000.0,
                gyro_z: 0.0,
            };
            let (x, y) = imu::map_gyro_to_stick(&gyro, &config);
            assert!(x <= 1.0, "x should be clamped to 1.0, got {}", x);
            assert!(x >= -1.0, "x should be clamped to -1.0, got {}", x);
            assert!(y <= 1.0, "y should be clamped to 1.0, got {}", y);
            assert!(y >= -1.0, "y should be clamped to -1.0, got {}", y);
        }
    }

    // ===================================================================
    //  Subcmd integration tests
    // ===================================================================

    mod subcmd_integration_tests {
        use super::*;

        #[test]
        fn build_get_device_info_subcmd_via_hid_parser() {
            let pkt = hid_parser::build_get_device_info_subcmd(0);
            // Format: [0x01, counter, 8 zeros, 0x02]
            assert_eq!(pkt[0], 0x01, "output report ID should be 0x01");
            assert_eq!(pkt[1], 0, "counter should be 0");
            // bytes 2..10 = rumble (zeros)
            for i in 2..10 {
                assert_eq!(pkt[i], 0, "rumble byte {} should be 0", i);
            }
            assert_eq!(pkt[10], 0x02, "subcmd ID should be 0x02 (device info)");
            assert_eq!(pkt.len(), 11, "device info subcmd should be 11 bytes");
        }

        #[test]
        fn build_spi_flash_read_subcmd_address_le() {
            let pkt = subcmd::build_spi_flash_read_subcmd(0, 0x1234, 0x10);
            assert_eq!(pkt[0], 0x01, "output report ID");
            assert_eq!(pkt[10], 0x10, "subcmd ID should be 0x10 (SPI flash read)");
            // Address 0x1234 in little-endian: 0x34, 0x12, 0x00
            assert_eq!(pkt[11], 0x34, "address byte 0 (LE)");
            assert_eq!(pkt[12], 0x12, "address byte 1 (LE)");
            assert_eq!(pkt[13], 0x00, "address byte 2 (LE)");
            assert_eq!(pkt[14], 0x10, "size should be 0x10");
        }

        #[test]
        fn build_set_player_lights_subcmd_led_mask() {
            let pkt = subcmd::build_set_player_lights_subcmd(0, 0b1111, 0);
            assert_eq!(pkt[10], 0x30, "subcmd ID should be 0x30 (player lights)");
            // Single byte: (flash << 4) | on = (0 << 4) | 0x0F = 0x0F
            assert_eq!(pkt[11], 0b0000_1111, "combined byte: on=0xF, flash=0");
            assert_eq!(
                pkt.len(),
                12,
                "packet should be 12 bytes (10 header + subcmd + 1 data)"
            );
        }

        #[test]
        fn build_enable_imu_subcmd_true() {
            let pkt = subcmd::build_enable_imu_subcmd(0, true);
            assert_eq!(pkt[10], 0x40, "subcmd ID should be 0x40 (enable IMU)");
            assert_eq!(pkt[11], 0x01, "data should be 0x01 when enabled");
        }

        #[test]
        fn build_enable_imu_subcmd_false() {
            let pkt = subcmd::build_enable_imu_subcmd(0, false);
            assert_eq!(pkt[10], 0x40, "subcmd ID should be 0x40 (enable IMU)");
            assert_eq!(pkt[11], 0x00, "data should be 0x00 when disabled");
        }

        #[test]
        fn build_enable_vibration_subcmd_true() {
            let pkt = subcmd::build_enable_vibration_subcmd(0, true);
            assert_eq!(pkt[10], 0x48, "subcmd ID should be 0x48 (enable vibration)");
            assert_eq!(pkt[11], 0x01, "data should be 0x01 when enabled");
        }

        #[test]
        fn encode_rumble_frequency_clamped() {
            // 0.0 Hz is below the minimum (41.0) — should clamp to 41.0
            let enc = subcmd::encode_rumble_frequency(0.0);
            let expected = (41.0f32 / 10.0).log2() * 32.0;
            assert_eq!(
                enc,
                expected.round() as u8,
                "frequency 0.0 should be clamped to 41.0 Hz"
            );
        }

        #[test]
        fn encode_rumble_amplitude_clamped_high() {
            // 2.0 is above the max (0.9) — should clamp to 0.9 → 255
            let enc = subcmd::encode_rumble_amplitude(2.0);
            assert_eq!(enc, 255, "amplitude 2.0 should be clamped to 0.9 → 255");
        }

        #[test]
        fn build_rumble_report_format() {
            let report = subcmd::build_rumble_report(0, 160.0, 0.5, 160.0, 0.5);
            assert_eq!(report.len(), 10, "rumble report should be 10 bytes");
            assert_eq!(report[0], 0x10, "report ID should be 0x10");
            assert_eq!(report[1], 0, "counter should be 0");
            // bytes 2..10 = 8 bytes of motor data (4 left + 4 right)
        }

        #[test]
        fn parse_device_info_reply_valid() {
            // Build a valid 12-byte device info payload.
            let mut data = vec![0u8; 12];
            data[0] = 0x03; // firmware major
            data[1] = 0x48; // firmware minor
            data[2] = 0x03; // controller type (Pro)
            data[3] = 0x00; // reserved
            data[4] = 0xBB; // MAC[0]
            data[5] = 0x8A;
            data[6] = 0xEA;
            data[7] = 0x30;
            data[8] = 0x57;
            data[9] = 0x01; // MAC[5]
            data[10] = 0x00; // reserved
            data[11] = 0x01; // colors_from_spi

            let info = subcmd::parse_device_info_reply(&data);
            assert!(info.is_some(), "valid 12-byte device info should parse");
            let info = info.unwrap();
            assert_eq!(info.firmware_version, "3.72");
            assert_eq!(info.controller_type, 0x03);
            assert_eq!(info.mac_address, "BB:8A:EA:30:57:01");
            assert!(info.colors_from_spi);
        }

        #[test]
        fn parse_device_info_reply_too_short() {
            let data = vec![0u8; 11]; // less than 12 bytes
            let info = subcmd::parse_device_info_reply(&data);
            assert!(info.is_none(), "short device info reply should return None");
        }

        #[test]
        fn parse_stick_calibration_reply_valid() {
            // Build a valid 18-byte stick calibration payload using the
            // correct SPI flash format (relative offsets, not absolute).
            // center=0x800, min=0x200, max=0xE00
            // → max_above = max - center = 0x600
            // → min_below = center - min = 0x600
            let center = 0x800u16;
            let max_above = 0x600u16; // max - center
            let min_below = 0x600u16; // center - min

            let mut cal: Vec<u8> = Vec::with_capacity(18);
            // Left stick byte order: [max_above, center, min_below]
            for chunk in [max_above, max_above, center, center, min_below, min_below].chunks(2) {
                let x = chunk[0];
                let y = chunk[1];
                cal.push((x & 0xFF) as u8);
                cal.push(((x >> 8) & 0x0F) as u8 | ((y & 0x0F) << 4) as u8);
                cal.push(((y >> 4) & 0xFF) as u8);
            }
            // Right stick byte order: [center, min_below, max_above]
            for chunk in [center, center, min_below, min_below, max_above, max_above].chunks(2) {
                let x = chunk[0];
                let y = chunk[1];
                cal.push((x & 0xFF) as u8);
                cal.push(((x >> 8) & 0x0F) as u8 | ((y & 0x0F) << 4) as u8);
                cal.push(((y >> 4) & 0xFF) as u8);
            }

            assert_eq!(cal.len(), 18);
            let result = subcmd::parse_stick_calibration_reply(&cal);
            assert!(result.is_some(), "valid 18-byte calibration should parse");
            let cal_parsed = result.unwrap();
            assert_eq!(cal_parsed.left_center_x, 0x800);
            assert_eq!(cal_parsed.left_max_x, 0xE00);
            assert_eq!(cal_parsed.left_min_x, 0x200);
            assert_eq!(cal_parsed.right_center_x, 0x800);
        }

        #[test]
        fn parse_stick_calibration_reply_too_short() {
            let data = vec![0u8; 17]; // less than 18 bytes
            let result = subcmd::parse_stick_calibration_reply(&data);
            assert!(
                result.is_none(),
                "short calibration reply should return None"
            );
        }

        // --- USB command builder tests ---

        #[test]
        fn build_usb_handshake_format() {
            let pkt = subcmd::build_usb_handshake();
            assert_eq!(pkt[0], 0x80, "output report ID should be 0x80 (USB cmd)");
            assert_eq!(pkt[1], 0x02, "cmd ID should be 0x02 (handshake)");
            assert_eq!(pkt[2], 0x01, "handshake data byte should be 0x01");
            assert_eq!(pkt.len(), 3, "handshake packet should be 3 bytes");
        }

        #[test]
        fn build_usb_baudrate_3m_format() {
            let pkt = subcmd::build_usb_baudrate_3m();
            assert_eq!(pkt[0], 0x80, "output report ID should be 0x80 (USB cmd)");
            assert_eq!(pkt[1], 0x03, "cmd ID should be 0x03 (baudrate 3M)");
            assert_eq!(pkt[2], 0x03, "baudrate data should be 0x03");
            assert_eq!(pkt.len(), 3, "baudrate packet should be 3 bytes");
        }

        #[test]
        fn build_usb_no_timeout_format() {
            let pkt = subcmd::build_usb_no_timeout();
            assert_eq!(pkt[0], 0x80, "output report ID should be 0x80 (USB cmd)");
            assert_eq!(pkt[1], 0x04, "cmd ID should be 0x04 (no timeout)");
            assert_eq!(pkt[2], 0x00, "no-timeout data should be 0x00");
            assert_eq!(pkt.len(), 3, "no-timeout packet should be 3 bytes");
        }

        #[test]
        fn build_usb_enable_timeout_format() {
            let pkt = subcmd::build_usb_enable_timeout();
            assert_eq!(pkt[0], 0x80, "output report ID should be 0x80 (USB cmd)");
            assert_eq!(pkt[1], 0x05, "cmd ID should be 0x05 (enable timeout)");
            assert_eq!(pkt[2], 0x00, "enable-timeout data should be 0x00");
            assert_eq!(pkt.len(), 3, "enable-timeout packet should be 3 bytes");
        }

        #[test]
        fn build_usb_cmd_generic() {
            let pkt = subcmd::build_usb_cmd(0x99, &[0xAA, 0xBB]);
            assert_eq!(pkt[0], 0x80, "output report ID should be 0x80");
            assert_eq!(pkt[1], 0x99, "cmd ID should match argument");
            assert_eq!(pkt[2], 0xAA, "data byte 0 should match");
            assert_eq!(pkt[3], 0xBB, "data byte 1 should match");
            assert_eq!(pkt.len(), 4, "packet length should be 2 + data.len()");
        }

        #[test]
        fn build_usb_cmd_no_data() {
            let pkt = subcmd::build_usb_cmd(0x02, &[]);
            assert_eq!(pkt.len(), 2, "packet with no data should be 2 bytes");
            assert_eq!(pkt[0], 0x80);
            assert_eq!(pkt[1], 0x02);
        }
    }

    mod p0_feature_tests {
        use super::*;
        use state::*;

        macro_rules! roundtrip_default_test {
            ($name:ident, $ty:ty) => {
                #[test]
                fn $name() {
                    let original: $ty = <$ty>::default();
                    let json = serde_json::to_string(&original).expect("should serialize");
                    let back: $ty = serde_json::from_str(&json).expect("should deserialize");
                    assert_eq!(original, back, "roundtrip failed for {}", stringify!($ty));
                }
            };
        }

        roundtrip_default_test!(button_id_default_roundtrip, ButtonId);
        roundtrip_default_test!(stick_side_default_roundtrip, StickSide);
        roundtrip_default_test!(trigger_side_default_roundtrip, TriggerSide);
        roundtrip_default_test!(auto_rule_kind_default_roundtrip, AutoRuleKind);
        roundtrip_default_test!(match_mode_default_roundtrip, MatchMode);
        roundtrip_default_test!(
            virtual_controller_type_default_roundtrip,
            VirtualControllerType
        );
        roundtrip_default_test!(profile_default_roundtrip, Profile);
        roundtrip_default_test!(profile_manager_default_roundtrip, ProfileManager);
        roundtrip_default_test!(auto_rule_default_roundtrip, AutoRule);
        roundtrip_default_test!(macro_default_roundtrip, Macro);
        roundtrip_default_test!(macro_step_default_roundtrip, MacroStep);
        roundtrip_default_test!(action_default_roundtrip, Action);
        roundtrip_default_test!(button_mapping_default_roundtrip, ButtonMapping);
        roundtrip_default_test!(stick_action_default_roundtrip, StickAction);
        roundtrip_default_test!(stick_zones_default_roundtrip, StickZones);
        roundtrip_default_test!(trigger_zones_default_roundtrip, TriggerZones);
        roundtrip_default_test!(stick_mapping_default_roundtrip, StickMapping);
        roundtrip_default_test!(gyro_mode_default_roundtrip, GyroMode);
        roundtrip_default_test!(gyro_mapping_default_roundtrip, GyroMapping);
        roundtrip_default_test!(mappings_default_roundtrip, Mappings);
        roundtrip_default_test!(shift_activation_default_roundtrip, ShiftActivation);
        roundtrip_default_test!(shift_layer_default_roundtrip, ShiftLayer);
        roundtrip_default_test!(response_curve_type_default_roundtrip, ResponseCurveType);
        roundtrip_default_test!(log_config_default_roundtrip, LogConfig);
        roundtrip_default_test!(app_log_entry_default_roundtrip, AppLogEntry);
        roundtrip_default_test!(tray_state_default_roundtrip, TrayState);
        roundtrip_default_test!(kbm_config_default_roundtrip, KbmConfig);

        #[test]
        fn app_config_feature_fields_default() {
            let config = AppConfig::default();
            assert!(config.profile_manager.profiles.is_empty());
            assert!(config.profile_manager.active_profile_id.is_none());
            assert_eq!(config.log_config.level, "info");
            assert!(!config.kbm_config.enabled);
            assert!((config.kbm_config.mouse_sensitivity - 1.0).abs() < f32::EPSILON);
            assert!(matches!(
                config.default_virtual_controller,
                VirtualControllerType::Xbox360
            ));
            assert!(config.mappings.buttons.is_empty());
        }

        #[test]
        fn app_config_feature_fields_roundtrip() {
            let mut config = AppConfig::default();
            config.profile_manager.active_profile_id = Some("p1".into());
            config.profile_manager.default_profile_id = Some("p1".into());
            config.log_config.level = "debug".into();
            config.log_config.max_lines = 5000;
            config.kbm_config.mouse_sensitivity = 2.5;
            config.default_virtual_controller = VirtualControllerType::DualShock4;
            config.mappings.buttons.push(ButtonMapping {
                source: ButtonId::B,
                actions: vec![
                    Action::Button(ButtonId::A),
                    Action::KeyCombo(vec!["ctrl".into(), "c".into()]),
                ],
            });

            let json = serde_json::to_string(&config).expect("should serialize");
            let back: AppConfig = serde_json::from_str(&json).expect("should deserialize");
            assert_eq!(back.profile_manager.active_profile_id, Some("p1".into()));
            assert_eq!(back.log_config.level, "debug");
            assert_eq!(back.log_config.max_lines, 5000);
            assert!((back.kbm_config.mouse_sensitivity - 2.5).abs() < f32::EPSILON);
            assert!(matches!(
                back.default_virtual_controller,
                VirtualControllerType::DualShock4
            ));
            assert_eq!(back.mappings.buttons.len(), 1);
            assert_eq!(back.mappings.buttons[0].source, ButtonId::B);
            assert_eq!(back.mappings.buttons[0].actions.len(), 2);
        }

        #[test]
        fn controller_state_new_feature_fields_default() {
            let state = ControllerState::default();
            assert!(state.tray_state.visible);
            assert!(!state.tray_state.minimized);
            assert!(!state.tray_state.auto_start);
            assert!(state.active_profile_name.is_none());
        }

        #[test]
        fn controller_state_feature_fields_roundtrip() {
            let mut state = ControllerState::default();
            state.tray_state.visible = false;
            state.tray_state.minimized = true;
            state.active_profile_name = Some("gaming".into());

            let json = serde_json::to_string(&state).expect("should serialize");
            let back: ControllerState = serde_json::from_str(&json).expect("should deserialize");
            assert!(!back.tray_state.visible);
            assert!(back.tray_state.minimized);
            assert_eq!(back.active_profile_name, Some("gaming".into()));
        }

        #[test]
        fn ipc_event_profile_changed_serialization() {
            let event = IpcEvent::ProfileChanged {
                profile_id: Some("p1".into()),
                profile_name: Some("Gaming".into()),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"ProfileChanged\""), "{}", json);
            assert!(json.contains("\"profile_id\":\"p1\""), "{}", json);
            assert!(json.contains("\"profile_name\":\"Gaming\""), "{}", json);
        }

        #[test]
        fn ipc_event_log_batch_roundtrip() {
            let event = IpcEvent::LogBatch {
                logs: vec![AppLogEntry {
                    timestamp: 12345,
                    level: "info".into(),
                    target: "test".into(),
                    message: "hello".into(),
                }],
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"LogBatch\""), "{}", json);
            let back: IpcEvent = serde_json::from_str(&json).expect("should deserialize");
            match back {
                IpcEvent::LogBatch { logs } => {
                    assert_eq!(logs.len(), 1);
                    assert_eq!(logs[0].message, "hello");
                }
                _ => panic!("expected LogBatch variant"),
            }
        }

        #[test]
        fn ipc_event_tray_state_changed_roundtrip() {
            let event = IpcEvent::TrayStateChanged {
                data: TrayState::default(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"TrayStateChanged\""), "{}", json);
            let back: IpcEvent = serde_json::from_str(&json).expect("should deserialize");
            match back {
                IpcEvent::TrayStateChanged { data } => assert!(data.visible),
                _ => panic!("expected TrayStateChanged variant"),
            }
        }

        #[test]
        fn ipc_event_kbm_state_changed_roundtrip() {
            let event = IpcEvent::KbmStateChanged {
                data: KbmConfig::default(),
            };
            let json = serde_json::to_string(&event).expect("should serialize");
            assert!(json.contains("\"type\":\"KbmStateChanged\""), "{}", json);
            let back: IpcEvent = serde_json::from_str(&json).expect("should deserialize");
            match back {
                IpcEvent::KbmStateChanged { data } => {
                    assert!((data.mouse_sensitivity - 1.0).abs() < f32::EPSILON);
                }
                _ => panic!("expected KbmStateChanged variant"),
            }
        }

        #[test]
        fn macro_step_complex_variants_roundtrip() {
            let steps = vec![
                MacroStep::MouseMove(100, -50),
                MacroStep::SetStick(StickSide::Right, 0.5, -0.5),
                MacroStep::SetTrigger(TriggerSide::Left, 0.75),
                MacroStep::KeyDown("ctrl".into()),
                MacroStep::KeyUp("v".into()),
            ];
            let mac = Macro {
                id: "m1".into(),
                name: "paste".into(),
                steps,
            };
            let json = serde_json::to_string(&mac).expect("should serialize");
            let back: Macro = serde_json::from_str(&json).expect("should deserialize");
            assert_eq!(back.steps.len(), 5);
            assert!(matches!(back.steps[0], MacroStep::MouseMove(100, -50)));
            assert!(
                matches!(back.steps[2], MacroStep::SetTrigger(TriggerSide::Left, v) if (v - 0.75).abs() < f32::EPSILON)
            );
        }

        #[test]
        fn response_curve_bezier_roundtrip() {
            let curve = ResponseCurveType::Bezier {
                p1: [0.1, 0.2],
                p2: [0.8, 0.9],
            };
            let json = serde_json::to_string(&curve).expect("should serialize");
            let back: ResponseCurveType = serde_json::from_str(&json).expect("should deserialize");
            assert_eq!(curve, back);
        }

        #[test]
        fn shift_layer_with_activation_roundtrip() {
            let layer = ShiftLayer {
                id: 1,
                name: "aim".into(),
                activation: ShiftActivation::Hold(ButtonId::L),
                mappings: Mappings::default(),
            };
            let json = serde_json::to_string(&layer).expect("should serialize");
            let back: ShiftLayer = serde_json::from_str(&json).expect("should deserialize");
            assert_eq!(back.id, 1);
            assert!(matches!(
                back.activation,
                ShiftActivation::Hold(ButtonId::L)
            ));
        }

        #[test]
        fn stick_zones_actions_roundtrip() {
            let zones = StickZones {
                deadzone: 0.1,
                low: 0.3,
                medium: 0.6,
                high: 0.9,
                low_actions: vec![Action::ProfilePrev],
                medium_actions: vec![Action::GyroToggle],
                high_actions: vec![Action::ShiftLayer(1)],
            };
            let json = serde_json::to_string(&zones).expect("should serialize");
            let back: StickZones = serde_json::from_str(&json).expect("should deserialize");
            assert_eq!(back.low_actions.len(), 1);
            assert!(matches!(back.medium_actions[0], Action::GyroToggle));
        }

        // ===================================================================
        //  kbm / keycode tests
        // ===================================================================

        mod kbm_tests {
            use std::sync::Arc;
            use tokio::time::{sleep, Duration};

            use crate::kbm::{InputEvent, KbmEmulator, MockBackend};
            use crate::keycode;
            use crate::state::{
                Action, ButtonId, ButtonMapping, KbmConfig, Mappings, StickAction, StickMapping,
                StickSide, StickZones,
            };

            fn mock_emulator() -> (
                KbmEmulator,
                tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
            ) {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
                let mut cfg = KbmConfig::default();
                cfg.enabled = true;
                emu.set_config(&cfg);
                (emu, rx)
            }

            #[test]
            fn keycode_parsing() {
                assert_eq!(keycode::vk("W"), Some(0x57));
                assert_eq!(keycode::vk("Space"), Some(0x20));
                assert_eq!(keycode::vk("LShift"), Some(0xA0));
                assert_eq!(keycode::vk("ctrl"), Some(0xA2));
                assert_eq!(keycode::vk("Left"), Some(0x25));
                assert_eq!(keycode::vk("F1"), Some(0x70));
                assert_eq!(keycode::vk("unknown"), None);
            }

            #[tokio::test]
            async fn process_button_key_action() {
                let (mut emu, mut rx) = mock_emulator();
                let mappings = Mappings {
                    buttons: vec![ButtonMapping {
                        source: ButtonId::B,
                        actions: vec![Action::Key("X".into())],
                    }],
                    ..Default::default()
                };

                emu.process_button(ButtonId::B, true, &mappings);
                assert_eq!(
                    rx.try_recv(),
                    Ok(InputEvent::Key {
                        vk: 0x58,
                        down: true
                    })
                );
                assert!(rx.try_recv().is_err());

                emu.process_button(ButtonId::B, false, &mappings);
                assert_eq!(
                    rx.try_recv(),
                    Ok(InputEvent::Key {
                        vk: 0x58,
                        down: false
                    })
                );
                assert!(rx.try_recv().is_err());
            }

            #[test]
            fn process_button_mouse_action() {
                let (mut emu, mut rx) = mock_emulator();
                let mappings = Mappings {
                    buttons: vec![ButtonMapping {
                        source: ButtonId::A,
                        actions: vec![Action::MouseButton(0)],
                    }],
                    ..Default::default()
                };

                emu.process_button(ButtonId::A, true, &mappings);
                assert_eq!(
                    rx.try_recv(),
                    Ok(InputEvent::MouseButton {
                        button: 0,
                        down: true
                    })
                );
                emu.process_button(ButtonId::A, false, &mappings);
                assert_eq!(
                    rx.try_recv(),
                    Ok(InputEvent::MouseButton {
                        button: 0,
                        down: false
                    })
                );
            }

            #[tokio::test]
            async fn key_repeat_for_held_keys() {
                let (mut emu, mut rx) = mock_emulator();
                let mut cfg = KbmConfig::default();
                cfg.enabled = true;
                cfg.key_repeat_delay_ms = 10;
                cfg.key_repeat_rate_ms = 10;
                emu.set_config(&cfg);

                let mappings = Mappings {
                    buttons: vec![ButtonMapping {
                        source: ButtonId::Y,
                        actions: vec![Action::Key("W".into())],
                    }],
                    ..Default::default()
                };

                emu.process_button(ButtonId::Y, true, &mappings);
                assert_eq!(
                    rx.try_recv(),
                    Ok(InputEvent::Key {
                        vk: 0x57,
                        down: true
                    })
                );

                sleep(Duration::from_millis(200)).await;
                let mut downs = 0;
                while let Ok(ev) = rx.try_recv() {
                    if ev
                        == (InputEvent::Key {
                            vk: 0x57,
                            down: true,
                        })
                    {
                        downs += 1;
                    }
                }
                assert!(
                    downs >= 2,
                    "expected at least 2 repeat down events, got {}",
                    downs
                );

                emu.process_button(ButtonId::Y, false, &mappings);
                assert_eq!(
                    rx.try_recv(),
                    Ok(InputEvent::Key {
                        vk: 0x57,
                        down: false
                    })
                );
            }

            #[test]
            fn process_stick_wasd() {
                let (mut emu, mut rx) = mock_emulator();
                let mapping = StickMapping {
                    left_actions: vec![StickAction::Wasd],
                    right_actions: vec![],
                    zones: StickZones {
                        deadzone: 0.25,
                        ..Default::default()
                    },
                    response_curve: Default::default(),
                };
                let cfg = KbmConfig {
                    enabled: true,
                    ..Default::default()
                };

                emu.process_stick(StickSide::Left, 1.0, 0.0, &cfg, &mapping);
                assert_eq!(
                    rx.try_recv(),
                    Ok(InputEvent::Key {
                        vk: 0x44,
                        down: true
                    })
                );

                // Move to the opposite direction: D released, A pressed.
                emu.process_stick(StickSide::Left, -1.0, 0.0, &cfg, &mapping);
                let mut saw_d_up = false;
                let mut saw_a_down = false;
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        InputEvent::Key {
                            vk: 0x44,
                            down: false,
                        } => saw_d_up = true,
                        InputEvent::Key {
                            vk: 0x41,
                            down: true,
                        } => saw_a_down = true,
                        _ => {}
                    }
                }
                assert!(saw_d_up, "expected D key up");
                assert!(saw_a_down, "expected A key down");
            }

            #[test]
            fn process_stick_mouse() {
                let (mut emu, mut rx) = mock_emulator();
                let mapping = StickMapping {
                    left_actions: vec![],
                    right_actions: vec![StickAction::Mouse],
                    zones: StickZones {
                        deadzone: 0.25,
                        ..Default::default()
                    },
                    response_curve: Default::default(),
                };
                let cfg = KbmConfig {
                    enabled: true,
                    mouse_sensitivity: 2.0,
                    ..Default::default()
                };

                emu.process_stick(StickSide::Right, 0.8, -0.5, &cfg, &mapping);
                assert_eq!(rx.try_recv(), Ok(InputEvent::MouseMove { dx: 32, dy: 20 }));
            }
        }
    }
}

#[cfg(test)]
mod curves_tests {
    use crate::curves::{
        apply_deadzone_shape, apply_response_curve, apply_stick_curve, apply_stick_curve_radial,
        zone_action,
    };
    use crate::state::{Action, ResponseCurveType, StickZones};

    #[test]
    fn linear_curve_is_identity() {
        let curve = ResponseCurveType::Linear;
        for v in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
            assert!((apply_response_curve(v, &curve) - v).abs() < 1e-5);
        }
    }

    #[test]
    fn exponential_curve_preserves_sign() {
        let curve = ResponseCurveType::Exponential(2.0);
        assert!((apply_response_curve(0.5, &curve) - 0.25).abs() < 1e-5);
        assert!((apply_response_curve(-0.5, &curve) - (-0.25)).abs() < 1e-5);
        assert!((apply_response_curve(1.0, &curve) - 1.0).abs() < 1e-5);
        assert!((apply_response_curve(-1.0, &curve) - (-1.0)).abs() < 1e-5);
    }

    #[test]
    fn s_curve_smoothstep() {
        let curve = ResponseCurveType::SCurve;
        assert!(apply_response_curve(0.0, &curve).abs() < 1e-5);
        assert!((apply_response_curve(1.0, &curve) - 1.0).abs() < 1e-5);
        assert!((apply_response_curve(0.5, &curve) - 0.5).abs() < 1e-5);
        // smoothstep(0.25) = 0.15625
        assert!((apply_response_curve(0.25, &curve) - 0.15625).abs() < 1e-5);
    }

    #[test]
    fn bezier_curve_endpoints() {
        let curve = ResponseCurveType::Bezier {
            p1: [0.1, 0.9],
            p2: [0.9, 0.1],
        };
        assert!(apply_response_curve(0.0, &curve).abs() < 1e-4);
        assert!((apply_response_curve(1.0, &curve) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn bezier_curve_midpoint_in_range() {
        let curve = ResponseCurveType::Bezier {
            p1: [0.3, 0.9],
            p2: [0.7, 0.1],
        };
        let v = apply_response_curve(0.5, &curve);
        assert!(v >= 0.0 && v <= 1.0, "bezier(0.5) = {}", v);
    }

    #[test]
    fn stick_curve_per_axis_preserves_sign() {
        let curve = ResponseCurveType::Exponential(2.0);
        let (x, y) = apply_stick_curve(-0.5, 0.8, &curve);
        assert!((x - (-0.25)).abs() < 1e-5);
        assert!((y - 0.64).abs() < 1e-5);
    }

    #[test]
    fn stick_curve_radial_preserves_direction() {
        let curve = ResponseCurveType::Exponential(2.0);
        let (x, y) = apply_stick_curve_radial(0.5, 0.0, &curve);
        assert!(y.abs() < 1e-5);
        // magnitude should be shaped: 0.5^2 = 0.25
        assert!((x - 0.25).abs() < 1e-5, "x = {}", x);
    }

    #[test]
    fn zone_action_deadzone_empty() {
        let zones = StickZones {
            deadzone: 0.1,
            low: 0.4,
            medium: 0.7,
            high: 0.95,
            low_actions: vec![Action::ProfilePrev],
            medium_actions: vec![Action::GyroToggle],
            high_actions: vec![Action::ShiftLayer(1)],
        };
        assert!(zone_action(0.05, &zones).is_empty());
    }

    #[test]
    fn zone_action_boundaries() {
        let zones = StickZones {
            deadzone: 0.1,
            low: 0.4,
            medium: 0.7,
            high: 0.95,
            low_actions: vec![Action::ProfilePrev],
            medium_actions: vec![Action::GyroToggle],
            high_actions: vec![Action::ShiftLayer(1)],
        };
        assert_eq!(zone_action(0.25, &zones).len(), 1);
        assert!(matches!(zone_action(0.25, &zones)[0], Action::ProfilePrev));
        assert!(matches!(zone_action(0.55, &zones)[0], Action::GyroToggle));
        assert!(matches!(
            zone_action(0.85, &zones)[0],
            Action::ShiftLayer(1)
        ));
    }

    #[test]
    fn radial_deadzone_zeros_and_scales() {
        let (x, y) = apply_deadzone_shape(0.03, 0.04, 0.1, "radial");
        assert!(x.abs() < f32::EPSILON && y.abs() < f32::EPSILON);

        let (x, y) = apply_deadzone_shape(0.5, 0.0, 0.1, "radial");
        let expected = 0.4_f32 / 0.9_f32;
        assert!((x - expected).abs() < 1e-4, "x = {}", x);
        assert!(y.abs() < f32::EPSILON);
    }

    #[test]
    fn axial_deadzone_per_axis() {
        let (x, y) = apply_deadzone_shape(0.05, 0.9, 0.1, "axial");
        assert!(x.abs() < f32::EPSILON);
        let expected = (0.9 - 0.1) / (1.0 - 0.1);
        assert!((y - expected).abs() < 1e-4, "y = {}", y);
    }

    #[test]
    fn elliptic_deadzone_shape() {
        // Points inside the elliptical gate should zero out.
        let (x, y) = apply_deadzone_shape(0.05, 0.05, 0.1, "elliptic");
        assert!(x.abs() < 1e-3 && y.abs() < 1e-3);

        // A large diagonal input should make it through (scaled).
        let (x, y) = apply_deadzone_shape(1.0, 1.0, 0.1, "elliptic");
        assert!(x > 0.0 && y > 0.0);
    }

    #[test]
    fn unknown_deadzone_shape_fallback_radial() {
        let (x, y) = apply_deadzone_shape(0.03, 0.04, 0.1, "unknown");
        assert!(x.abs() < f32::EPSILON && y.abs() < f32::EPSILON);
    }
}

#[cfg(test)]
mod profile_manager_tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{env, fs};

    use crate::profile_manager::{AutoRule, AutoRuleKind, MatchMode, ProfileManager};
    use crate::state::timestamp_now;

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_manager() -> (ProfileManager, PathBuf) {
        let n = TEST_ID.fetch_add(1, Ordering::SeqCst);
        let dir = env::temp_dir().join("oxidelink").join(format!(
            "profile-test-{}-{}-{}",
            timestamp_now(),
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.json");
        (ProfileManager::with_path(&path), path)
    }

    fn rule(kind: AutoRuleKind, pattern: &str, mode: MatchMode) -> AutoRule {
        AutoRule {
            kind,
            pattern: pattern.to_string(),
            match_mode: mode,
            enabled: true,
        }
    }

    #[test]
    fn create_update_delete_and_list_profiles() {
        let (pm, _path) = temp_manager();
        let p1 = pm.create_profile("A".into(), None).unwrap();
        let p2 = pm.create_profile("B".into(), None).unwrap();
        assert_eq!(pm.list_profiles().len(), 2);

        let mut updated = p1.clone();
        updated.name = "A-renamed".into();
        pm.update_profile(updated.clone()).unwrap();
        assert_eq!(pm.get_profile(&p1.id).unwrap().name, "A-renamed");

        pm.delete_profile(&p2.id).unwrap();
        assert_eq!(pm.list_profiles().len(), 1);
    }

    #[test]
    fn active_profile_works() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("Active".into(), None).unwrap();
        assert!(pm.get_active_profile().is_none());

        pm.set_active_profile(Some(&p.id)).unwrap();
        assert_eq!(pm.get_active_profile().unwrap().id, p.id);

        pm.set_active_profile(None).unwrap();
        assert!(pm.get_active_profile().is_none());

        assert!(pm.set_active_profile(Some("missing")).is_err());
    }

    #[test]
    fn exact_process_path_matching() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("Rocket".into(), None).unwrap();
        let mut profile = p.clone();
        profile.auto_rules.push(rule(
            AutoRuleKind::ProcessPath,
            r"c:\games\rocketleague.exe",
            MatchMode::Exact,
        ));
        pm.update_profile(profile).unwrap();

        assert!(pm
            .find_matching_profile(r"c:\games\rocketleague.exe", "Window")
            .is_some());
        assert!(pm
            .find_matching_profile(r"c:\games\notrocket.exe", "Window")
            .is_none());
    }

    #[test]
    fn contains_window_title_matching() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("Editor".into(), None).unwrap();
        let mut profile = p.clone();
        profile.auto_rules.push(rule(
            AutoRuleKind::WindowTitle,
            "Visual Studio",
            MatchMode::Contains,
        ));
        pm.update_profile(profile).unwrap();

        assert!(pm
            .find_matching_profile("app.exe", "Visual Studio Code")
            .is_some());
        assert!(pm.find_matching_profile("app.exe", "Notepad").is_none());
    }

    #[test]
    fn regex_matching_and_disabled_rules() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("Regex".into(), None).unwrap();
        let mut profile = p.clone();
        profile.auto_rules.push(rule(
            AutoRuleKind::ProcessPath,
            r"game_\d+\.exe$",
            MatchMode::Regex,
        ));
        let mut disabled = rule(AutoRuleKind::WindowTitle, "Bad", MatchMode::Contains);
        disabled.enabled = false;
        profile.auto_rules.push(disabled);
        pm.update_profile(profile).unwrap();

        assert!(pm
            .find_matching_profile(r"c:\x\game_42.exe", "title")
            .is_some());
        assert!(pm
            .find_matching_profile(r"c:\x\game.exe", "title")
            .is_none());
        assert!(pm.find_matching_profile("app.exe", "Bad").is_none());
    }

    #[test]
    fn default_profile_fallback() {
        let (pm, _path) = temp_manager();
        let fallback = pm.create_profile("Fallback".into(), None).unwrap();
        let matched = pm.create_profile("Matched".into(), None).unwrap();

        let mut matched_p = matched.clone();
        matched_p
            .auto_rules
            .push(rule(AutoRuleKind::WindowTitle, "Target", MatchMode::Exact));
        pm.update_profile(matched_p).unwrap();

        pm.set_default_profile_id(Some(fallback.id.clone()))
            .unwrap();

        // no rule match -> default
        assert_eq!(
            pm.find_matching_profile("app.exe", "Other").unwrap().id,
            fallback.id
        );
        // rule match -> matched profile
        assert_eq!(
            pm.find_matching_profile("app.exe", "Target").unwrap().id,
            matched.id
        );
    }

    #[test]
    fn serialization_roundtrip() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("Saved".into(), None).unwrap();
        pm.set_active_profile(Some(&p.id)).unwrap();
        pm.set_default_profile_id(Some(p.id.clone())).unwrap();

        let json = pm.to_json().unwrap();
        let (pm2, _path2) = temp_manager();
        pm2.from_json(&json).unwrap();
        assert_eq!(pm2.list_profiles().len(), 1);
        assert_eq!(pm2.get_active_profile().unwrap().id, p.id);
        assert_eq!(pm2.get_default_profile_id().unwrap(), p.id);
    }

    #[test]
    fn export_import_roundtrip() {
        let (pm, _path) = temp_manager();
        let _p = pm.create_profile("Export".into(), None).unwrap();

        let export_path = _path.parent().unwrap().join("export.json");
        pm.export_to_path(&export_path).unwrap();

        // Import into a fresh manager whose store lives in the same directory,
        // so the export path is still under the importer's allowed base.
        let import_path = _path.parent().unwrap().join("profiles2.json");
        let pm2 = ProfileManager::with_path(&import_path);
        let imported = pm2.import_from_path(&export_path).unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].name, "Export");
    }

    #[test]
    fn auto_switch_state() {
        let (pm, _path) = temp_manager();
        assert!(!pm.is_auto_switch_enabled());
        pm.set_auto_switch_enabled(true);
        assert!(pm.is_auto_switch_enabled());
        pm.set_auto_switch_enabled(false);
        assert!(!pm.is_auto_switch_enabled());
    }
}

#[cfg(test)]
mod hidhide_tests {
    use crate::hidhide::{
        ctl_code, decode_multi_sz_bytes, encode_multi_sz_bytes, HIDHIDE_ACCESS,
        HIDHIDE_DEVICE_TYPE, HIDHIDE_METHOD, IOCTL_ADD_SESSION_BLACKLIST,
        IOCTL_CLR_SESSION_BLACKLIST, IOCTL_GET_ACTIVE, IOCTL_GET_BLACKLIST, IOCTL_GET_WHITELIST,
        IOCTL_SET_ACTIVE, IOCTL_SET_BLACKLIST, IOCTL_SET_WHITELIST,
    };

    #[test]
    fn hidhide_ioctl_codes_match_deliverable() {
        // CTL_CODE(0x8000, function, METHOD_BUFFERED, FILE_ANY_ACCESS)
        assert_eq!(ctl_code(0x8000, 0x800, 0, 0), IOCTL_GET_WHITELIST);
        assert_eq!(ctl_code(0x8000, 0x801, 0, 0), IOCTL_SET_WHITELIST);
        assert_eq!(ctl_code(0x8000, 0x802, 0, 0), IOCTL_GET_BLACKLIST);
        assert_eq!(ctl_code(0x8000, 0x803, 0, 0), IOCTL_SET_BLACKLIST);
        assert_eq!(ctl_code(0x8000, 0x804, 0, 0), IOCTL_GET_ACTIVE);
        assert_eq!(ctl_code(0x8000, 0x805, 0, 0), IOCTL_SET_ACTIVE);
        assert_eq!(ctl_code(0x8000, 0x808, 0, 0), IOCTL_ADD_SESSION_BLACKLIST);
        assert_eq!(ctl_code(0x8000, 0x809, 0, 0), IOCTL_CLR_SESSION_BLACKLIST);

        // Spot-check numeric values.
        assert_eq!(IOCTL_GET_WHITELIST, 0x80002000);
        assert_eq!(IOCTL_SET_BLACKLIST, 0x8000200C);
        assert_eq!(IOCTL_GET_ACTIVE, 0x80002010);
        assert_eq!(IOCTL_CLR_SESSION_BLACKLIST, 0x80002024);
    }

    #[test]
    fn hidhide_constants_reachable() {
        assert_eq!(HIDHIDE_DEVICE_TYPE, 0x8000);
        assert_eq!(HIDHIDE_METHOD, 0);
        assert_eq!(HIDHIDE_ACCESS, 0);
    }

    #[test]
    fn multi_sz_roundtrip() {
        let strings = vec![
            "HID\\VID_057E&PID_2009\\7&1234567&0&0000".to_string(),
            "HID\\{00001124-0000-1000-8000-00805f9b34fb}_VID&0002057E_PID&2009".to_string(),
        ];
        let encoded = encode_multi_sz_bytes(&strings);
        let decoded = decode_multi_sz_bytes(&encoded);
        assert_eq!(decoded, strings);
    }

    #[test]
    fn multi_sz_empty_list() {
        let encoded = encode_multi_sz_bytes(&[]);
        assert_eq!(encoded, vec![0, 0]);
        let decoded = decode_multi_sz_bytes(&encoded);
        assert!(decoded.is_empty());
    }

    #[test]
    fn multi_sz_single_entry() {
        let strings = vec!["single".to_string()];
        let encoded = encode_multi_sz_bytes(&strings);
        let decoded = decode_multi_sz_bytes(&encoded);
        assert_eq!(decoded, strings);
    }
}

#[cfg(test)]
mod macro_engine_tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::time::{sleep, Duration};

    use crate::macro_engine::{MacroEngine, MacroStore};
    use crate::state::{
        ButtonId, ControllerState, IpcEvent, Macro, MacroStep, SharedState, StickSide, TriggerSide,
    };

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_engine() -> (
        Arc<SharedState>,
        MacroEngine,
        tokio::sync::broadcast::Receiver<IpcEvent>,
    ) {
        let shared = SharedState::new();
        let (tx, rx) = tokio::sync::broadcast::channel(64);
        let engine = MacroEngine::new(shared.clone(), tx, None).unwrap();
        (shared, engine, rx)
    }

    fn temp_store() -> MacroStore {
        let n = TEST_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "oxidelink-macro-test-{}-{}.json",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        MacroStore::with_path(path)
    }

    #[test]
    fn macro_step_serialization() {
        let mac = Macro {
            id: "m1".into(),
            name: "test".into(),
            steps: vec![
                MacroStep::WaitMs(100),
                MacroStep::PressButton(ButtonId::A),
                MacroStep::ReleaseButton(ButtonId::B),
                MacroStep::KeyDown("ctrl".into()),
                MacroStep::KeyUp("v".into()),
                MacroStep::MouseMove(100, -50),
                MacroStep::MouseDown(0),
                MacroStep::MouseUp(0),
                MacroStep::SetStick(StickSide::Left, 0.5, -0.5),
                MacroStep::SetTrigger(TriggerSide::Right, 0.75),
            ],
        };
        let json = serde_json::to_string(&mac).expect("serialize");
        assert!(json.contains("\"type\":\"wait_ms\""));
        assert!(json.contains("\"type\":\"press_button\""));
        assert!(json.contains("\"type\":\"set_stick\""));
        assert!(json.contains("\"type\":\"set_trigger\""));
        let back: Macro = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(mac, back);
    }

    #[tokio::test]
    async fn macro_playback_timing_and_events() {
        let (_shared, engine, mut rx) = test_engine();
        let mac = Macro {
            id: "timing".into(),
            name: "timing".into(),
            steps: vec![
                MacroStep::PressButton(ButtonId::A),
                MacroStep::WaitMs(2),
                MacroStep::ReleaseButton(ButtonId::A),
            ],
        };
        let start = Instant::now();
        let engine2 = engine.clone();
        let mac2 = mac.clone();
        tokio::spawn(async move {
            engine2.play_macro(&mac2, None).await;
        });
        // Wait for the short macro to finish.
        sleep(Duration::from_millis(50)).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(2),
            "macro should wait at least 2 ms"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "macro should finish quickly"
        );

        let mut saw_press = false;
        let mut saw_release = false;
        while let Ok(ev) = rx.try_recv() {
            if let IpcEvent::ControllerState { data } = ev {
                if data.buttons.a {
                    saw_press = true;
                } else {
                    saw_release = true;
                }
            }
        }
        assert!(saw_press, "should emit a pressed state");
        assert!(saw_release, "should emit a released state");
    }

    #[tokio::test]
    async fn macro_playback_cancellable() {
        let (_shared, engine, _rx) = test_engine();
        let mac = Macro {
            id: "long".into(),
            name: "long".into(),
            steps: vec![MacroStep::WaitMs(5000), MacroStep::PressButton(ButtonId::A)],
        };
        let engine2 = engine.clone();
        let mac2 = mac.clone();
        tokio::spawn(async move {
            engine2.play_macro(&mac2, None).await;
        });
        sleep(Duration::from_millis(5)).await;
        assert!(engine.is_playing());
        assert!(engine.stop_playback());
        sleep(Duration::from_millis(20)).await;
        assert!(!engine.is_playing());
    }

    #[tokio::test]
    async fn macro_record_captures_state_changes() {
        let (engine, tx) = {
            let shared = SharedState::new();
            let (tx, _rx) = tokio::sync::broadcast::channel(64);
            let engine = MacroEngine::new(shared, tx.clone(), None).unwrap();
            (engine, tx)
        };

        engine.start_recording().expect("start recording");

        let mut state1 = ControllerState::default();
        state1.buttons.a = true;
        tx.send(IpcEvent::ControllerState {
            data: state1.clone(),
        })
        .ok();

        sleep(Duration::from_millis(5)).await;

        let mut state2 = state1.clone();
        state2.buttons.a = false;
        state2.left_stick.x = 0.75;
        tx.send(IpcEvent::ControllerState { data: state2 }).ok();

        sleep(Duration::from_millis(5)).await;

        let mac = engine
            .stop_recording("rec".into())
            .await
            .expect("stop recording");

        assert!(mac
            .steps
            .iter()
            .any(|s| matches!(s, MacroStep::PressButton(ButtonId::A))));
        assert!(mac
            .steps
            .iter()
            .any(|s| matches!(s, MacroStep::ReleaseButton(ButtonId::A))));
        assert!(mac
            .steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetStick(StickSide::Left, _, _))));
    }

    #[test]
    fn macro_store_save_and_load() {
        let store = temp_store();
        let mac = Macro {
            id: "store-1".into(),
            name: "saved".into(),
            steps: vec![MacroStep::WaitMs(10)],
        };
        store.save(&mac).expect("save");
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.get("store-1").unwrap().name, "saved");

        let json = std::fs::read_to_string(store.path()).expect("read file");
        assert!(json.contains("store-1"));

        let loaded = MacroStore::load_from(store.path()).expect("load");
        assert_eq!(loaded.list().len(), 1);
        assert_eq!(loaded.get("store-1").unwrap().name, "saved");

        store.delete("store-1").expect("delete");
        assert!(store.get("store-1").is_none());
    }

    mod logging_tests {
        use crate::logging::{clear_logs, get_logs, init_logging, set_log_level, LogCollector};
        use crate::state::{timestamp_now, AppLogEntry, IpcEvent, LogConfig};
        use log::{Level, LevelFilter, Log, Record};

        fn log_static(
            collector: &LogCollector,
            level: Level,
            target: &'static str,
            msg: &'static str,
        ) {
            let args = format_args!("{}", msg);
            let record = Record::builder()
                .args(args)
                .level(level)
                .target(target)
                .build();
            collector.log(&record);
        }

        fn test_config(capacity: usize, max_level: LevelFilter, log_file: bool) -> LogConfig {
            LogConfig {
                level: max_level.to_string().to_lowercase(),
                max_lines: capacity,
                ring_buffer: true,
                log_file,
            }
        }

        #[test]
        fn ring_buffer_eviction() {
            let cfg = test_config(3, LevelFilter::Trace, false);
            let collector = LogCollector::new(&cfg, LevelFilter::Trace).expect("new collector");
            for text in ["a", "b", "c", "d", "e"] {
                log_static(&collector, Level::Info, "test", text);
            }
            let logs = collector.recent(None, None, None);
            assert_eq!(logs.len(), 3);
            assert_eq!(logs[0].message, "c");
            assert_eq!(logs[1].message, "d");
            assert_eq!(logs[2].message, "e");
        }

        #[test]
        fn level_filtering_and_search() {
            let cfg = test_config(100, LevelFilter::Warn, false);
            let collector = LogCollector::new(&cfg, LevelFilter::Warn).expect("new collector");
            log_static(&collector, Level::Info, "test", "info msg");
            log_static(&collector, Level::Warn, "test", "warn msg");
            log_static(&collector, Level::Error, "test", "error msg");

            let all = collector.recent(None, None, None);
            assert_eq!(all.len(), 2);
            assert!(!all.iter().any(|e| e.level == "info"));

            let warn = collector.recent(Some("warn".into()), None, None);
            assert_eq!(warn.len(), 1);
            assert_eq!(warn[0].message, "warn msg");

            let search = collector.recent(None, Some("error".into()), None);
            assert_eq!(search.len(), 1);
            assert_eq!(search[0].level, "error");
        }

        #[test]
        fn log_entry_serialization() {
            let entry = AppLogEntry {
                timestamp: timestamp_now(),
                level: "info".into(),
                target: "test".into(),
                message: "hello".into(),
            };
            let json = serde_json::to_string(&entry).expect("serialize");
            assert!(json.contains("\"level\":\"info\""), "{}", json);
            let back: AppLogEntry = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, entry);
        }

        #[test]
        fn log_batch_ipc_serialization() {
            let event = IpcEvent::LogBatch {
                logs: vec![AppLogEntry {
                    timestamp: 12345,
                    level: "debug".into(),
                    target: "test".into(),
                    message: "batch".into(),
                }],
            };
            let json = serde_json::to_string(&event).expect("serialize");
            assert!(json.contains("\"type\":\"LogBatch\""), "{}", json);
            let back: IpcEvent = serde_json::from_str(&json).expect("deserialize");
            if let IpcEvent::LogBatch { logs } = back {
                assert_eq!(logs.len(), 1);
                assert_eq!(logs[0].message, "batch");
            } else {
                panic!("expected LogBatch");
            }
        }

        #[test]
        fn file_output_writes_to_disk() {
            let base = dirs_next::data_dir().unwrap_or_else(|| std::env::temp_dir());
            let path = base.join("OxideLink").join("logs").join(format!(
                "oxidelink-{}.log",
                chrono::Local::now().format("%Y-%m-%d")
            ));
            let _ = std::fs::remove_file(&path);

            let cfg = test_config(100, LevelFilter::Info, true);
            let collector =
                LogCollector::new(&cfg, LevelFilter::Info).expect("new collector with file");
            log_static(&collector, Level::Info, "file::test", "persisted line");
            // Drop to close the buffered file handle before reading.
            drop(collector);

            let contents = std::fs::read_to_string(&path).expect("read log file");
            assert!(contents.contains("persisted line"), "{}", contents);
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn init_logging_and_commands() {
            let cfg = LogConfig::default();
            let c1 = init_logging(&cfg).expect("first init");
            // Subsequent calls must return the already-installed collector.
            let c2 = init_logging(&cfg).expect("second init");
            assert_eq!(c1.recent(None, None, None), c2.recent(None, None, None));

            clear_logs();
            log::info!("command test log");
            let logs = get_logs(None, None, Some(50));
            assert!(!logs.is_empty());
            assert!(logs.iter().any(|e| e.message == "command test log"));

            set_log_level("debug".into()).expect("set debug");
            log::debug!("debug level test");
            let debug_logs = get_logs(Some("debug".into()), None, None);
            assert_eq!(debug_logs.len(), 1);
            assert_eq!(debug_logs[0].message, "debug level test");

            clear_logs();
            assert!(get_logs(None, None, None).is_empty());
        }
    }

    mod vixinput_tests {
        use crate::state::VirtualControllerType;
        use crate::vixinput::{Ds4Report, VirtualXInput};
        use crate::xinput::{
            XInputState, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
            XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
            XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_GUIDE, XINPUT_GAMEPAD_LEFT_SHOULDER,
            XINPUT_GAMEPAD_LEFT_THUMB, XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB,
            XINPUT_GAMEPAD_START, XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y,
        };

        #[test]
        fn ds4_report_default_is_centered_and_neutral() {
            let r = Ds4Report::default();
            assert_eq!(r.b_thumb_lx, 0x80);
            assert_eq!(r.b_thumb_ly, 0x80);
            assert_eq!(r.b_thumb_rx, 0x80);
            assert_eq!(r.b_thumb_ry, 0x80);
            assert_eq!(r.w_buttons & 0xF, 0x8);
            assert_eq!(r.b_trigger_l, 0);
            assert_eq!(r.b_trigger_r, 0);
            assert_eq!(r.b_special, 0);
        }

        #[test]
        fn ds4_report_from_xinput_state_maps_buttons_triggers_and_axes() {
            let state = XInputState {
                buttons: XINPUT_GAMEPAD_A
                    | XINPUT_GAMEPAD_B
                    | XINPUT_GAMEPAD_X
                    | XINPUT_GAMEPAD_Y
                    | XINPUT_GAMEPAD_LEFT_SHOULDER
                    | XINPUT_GAMEPAD_RIGHT_SHOULDER
                    | XINPUT_GAMEPAD_LEFT_THUMB
                    | XINPUT_GAMEPAD_RIGHT_THUMB
                    | XINPUT_GAMEPAD_BACK
                    | XINPUT_GAMEPAD_START
                    | XINPUT_GAMEPAD_GUIDE,
                left_trigger: 200,
                right_trigger: 255,
                thumb_lx: -32768,
                thumb_ly: 0,
                thumb_rx: 32767,
                thumb_ry: 0,
            };
            let r = Ds4Report::from(&state);

            // Axes scaled from i16 to u8 centered at 0x80.
            assert_eq!(r.b_thumb_lx, 0x00);
            assert_eq!(r.b_thumb_ly, 0x80);
            assert_eq!(r.b_thumb_rx, 0xFF);
            assert_eq!(r.b_thumb_ry, 0x80);

            // Triggers copied and reflected in button bits.
            assert_eq!(r.b_trigger_l, 200);
            assert_eq!(r.b_trigger_r, 255);
            assert!(r.w_buttons & (1 << 10) != 0);
            assert!(r.w_buttons & (1 << 11) != 0);

            // Face / shoulder / thumb / menu buttons.
            assert!(r.w_buttons & (1 << 5) != 0); // Cross
            assert!(r.w_buttons & (1 << 6) != 0); // Circle
            assert!(r.w_buttons & (1 << 4) != 0); // Square
            assert!(r.w_buttons & (1 << 7) != 0); // Triangle
            assert!(r.w_buttons & (1 << 8) != 0); // L1
            assert!(r.w_buttons & (1 << 9) != 0); // R1
            assert!(r.w_buttons & (1 << 14) != 0); // L3
            assert!(r.w_buttons & (1 << 15) != 0); // R3
            assert!(r.w_buttons & (1 << 12) != 0); // Share
            assert!(r.w_buttons & (1 << 13) != 0); // Options
            assert!(r.b_special & (1 << 0) != 0); // PS
        }

        #[test]
        fn ds4_report_dpad_mapping() {
            let mut state = XInputState::default();

            state.buttons = XINPUT_GAMEPAD_DPAD_UP;
            assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, 0x0); // North

            state.buttons = XINPUT_GAMEPAD_DPAD_RIGHT;
            assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, 0x2); // East

            state.buttons = XINPUT_GAMEPAD_DPAD_DOWN;
            assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, 0x4); // South

            state.buttons = XINPUT_GAMEPAD_DPAD_LEFT;
            assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, 0x6); // West

            state.buttons = XINPUT_GAMEPAD_DPAD_UP | XINPUT_GAMEPAD_DPAD_RIGHT;
            assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, 0x1); // Northeast
        }

        #[test]
        fn virtual_xinput_kind_matches_constructor() {
            let vix = VirtualXInput::new(VirtualControllerType::DualShock4);
            assert_eq!(vix.kind(), VirtualControllerType::DualShock4);
        }

        #[test]
        fn virtual_xinput_set_kind_changes_target_type() {
            let mut vix = VirtualXInput::new(VirtualControllerType::Xbox360);
            assert_eq!(vix.kind(), VirtualControllerType::Xbox360);
            vix.set_kind(VirtualControllerType::DualShock4);
            assert_eq!(vix.kind(), VirtualControllerType::DualShock4);
        }
    }
}

#[cfg(test)]
mod tray_tests {
    use super::*;
    use crate::state::AppCtx;

    #[test]
    fn run_key_path_matches_windows_run_key() {
        assert_eq!(
            tray::run_key_path(),
            r"Software\Microsoft\Windows\CurrentVersion\Run"
        );
    }

    #[test]
    fn run_value_name_is_oxidelink() {
        assert_eq!(tray::run_value_name(), "OxideLink");
    }

    #[test]
    fn tray_state_minimize_helpers_roundtrip() {
        let (tx, _rx) = tokio::sync::broadcast::channel(4);
        let ctx = AppCtx {
            shared: state::SharedState::new(),
            tx,
            keepalive: std::sync::Arc::new(keepalive::KeepAliveManager::new(std::sync::Arc::new(
                parking_lot::RwLock::new(state::KeepAliveStatus::default()),
            ))),
        };

        assert!(!tray::get_tray_minimize(&ctx));

        let st = tray::set_tray_minimize(&ctx, true);
        assert!(st.minimized);
        assert!(!st.visible);
        assert!(tray::get_tray_minimize(&ctx));

        let st = tray::set_tray_minimize(&ctx, false);
        assert!(!st.minimized);
        assert!(st.visible);
        assert!(!tray::get_tray_minimize(&ctx));
    }

    mod dsu_tests {
        use super::*;
        use crate::imu::ImuPhysical;
        use crate::state::{ButtonState, ConnectionType, ControllerState, StickState};
        use std::time::{Duration, Instant};

        fn connected_controller() -> ControllerState {
            let mut state = ControllerState::default();
            state.connected = true;
            state.battery_percent = 85;
            state.charging = false;
            state.connection_type = ConnectionType::Bluetooth;
            state.device_info = Some(crate::state::DeviceInfo {
                mac_address: "00:11:22:33:44:55".into(),
                ..Default::default()
            });
            state.buttons = ButtonState {
                a: true,
                b: true,
                dpad_up: true,
                home: true,
                ..Default::default()
            };
            state.left_stick = StickState {
                x: -0.5,
                y: 0.5,
                ..Default::default()
            };
            state.right_stick = StickState {
                x: 1.0,
                y: -1.0,
                ..Default::default()
            };
            state.left_trigger = 0.25;
            state.right_trigger = 0.75;
            state
        }

        fn sample_imu() -> ImuPhysical {
            ImuPhysical {
                accel_x: 0.12,
                accel_y: -0.34,
                accel_z: 0.95,
                gyro_x: 12.0,
                gyro_y: -23.5,
                gyro_z: 7.0,
            }
        }

        #[test]
        fn pad_data_packet_is_100_bytes() {
            let state = connected_controller();
            let imu = sample_imu();
            let packet = dsu::build_pad_data(0, 42, &state, Some(&imu), 0x12345678);
            assert_eq!(
                packet.len(),
                100,
                "DSU pad data packet must be 100 bytes total"
            );
            assert_eq!(&packet[0..4], b"DSUS");
            assert_eq!(u16::from_le_bytes([packet[4], packet[5]]), 1001);
            assert_eq!(u16::from_le_bytes([packet[6], packet[7]]), 80);
            assert!(dsu::build_version_reply(0).len() > 0); // sanity-check CRC helper path
        }

        #[test]
        fn crc32_roundtrips_and_detects_corruption() {
            let mut packet = dsu::build_version_reply(0xDEADBEEF);
            assert!(dsu::verify_crc(&packet), "fresh packet CRC should verify");
            // Corrupt the version payload.
            let last = packet.len() - 1;
            packet[last] = packet[last].wrapping_add(1);
            assert!(
                !dsu::verify_crc(&packet),
                "corrupted packet CRC should fail"
            );
        }

        #[test]
        fn client_times_out_after_five_seconds() {
            let mut client = dsu::Client::new(dsu::Subscription::All);
            client.last_seen = Instant::now() - Duration::from_secs(6);
            assert!(!client.is_active(Instant::now()));

            let fresh = dsu::Client::new(dsu::Subscription::Slot(0));
            assert!(fresh.is_active(Instant::now()));
        }

        #[test]
        fn slot_subscription_filters_wants_slot() {
            let client = dsu::Client::new(dsu::Subscription::Slot(2));
            assert!(client.wants_slot(2));
            assert!(!client.wants_slot(0));
        }
    }
}

#[cfg(test)]
mod gyro_mouse_tests {
    use std::sync::Arc;

    use crate::gyro_mouse::GyroMouse;
    use crate::imu::ImuPhysical;
    use crate::kbm::{InputEvent, MockBackend};
    use crate::state::{GyroMapping, GyroMode, StickSide};

    fn mouse_config() -> GyroMapping {
        GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        }
    }

    fn make_imu(gyro_x: f32, gyro_y: f32) -> ImuPhysical {
        ImuPhysical {
            accel_x: 0.0,
            accel_y: 0.0,
            accel_z: 1.0,
            gyro_x,
            gyro_y,
            gyro_z: 0.0,
        }
    }

    #[test]
    fn deadzone_ignores_small_gyro_rates() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut gyro = GyroMouse::with_backend(Arc::new(MockBackend::new(tx)));
        let mut cfg = mouse_config();
        cfg.deadzone = 5.0;

        // yaw below deadzone -> zero mouse delta and no backend event.
        let imu = make_imu(0.0, 2.0);
        let (dx, dy) = gyro.update(&imu, 1.0, &cfg);
        assert_eq!(dx, 0);
        assert_eq!(dy, 0);
        assert!(rx.try_recv().is_err());

        // yaw above deadzone -> non-zero delta; send_mouse_move emits the event.
        let imu = make_imu(0.0, 10.0);
        let (dx, dy) = gyro.update(&imu, 1.0, &cfg);
        assert_eq!(dx, 10);
        assert_eq!(dy, 0);
        gyro.send_mouse_move(dx, dy);
        match rx.try_recv() {
            Ok(InputEvent::MouseMove { dx: edx, dy: edy }) => {
                assert_eq!(edx, 10);
                assert_eq!(edy, 0);
            }
            other => panic!("expected MouseMove event, got {:?}", other),
        }
    }

    #[test]
    fn exponential_smoothing_blends_input() {
        // Use a dummy backend since this test only checks the returned deltas.
        let backend = Arc::new(MockBackend::new(tokio::sync::mpsc::unbounded_channel().0));
        let mut gyro = GyroMouse::with_backend(backend);
        let mut cfg = mouse_config();
        cfg.smoothing = 0.5;

        // smoothing=0.5 -> smooth = 0.5*old + 0.5*new.
        let imu = make_imu(0.0, 10.0);
        let (dx, _) = gyro.update(&imu, 1.0, &cfg);
        assert_eq!(dx, 5); // round(0.5*0 + 0.5*10)

        let (dx2, _) = gyro.update(&imu, 1.0, &cfg);
        assert_eq!(dx2, 8); // round(0.5*5 + 0.5*10)
    }

    #[test]
    fn smoothing_zero_returns_input_immediately() {
        let backend = Arc::new(MockBackend::new(tokio::sync::mpsc::unbounded_channel().0));
        let mut gyro = GyroMouse::with_backend(backend);
        let cfg = mouse_config();

        let imu = make_imu(0.0, 7.0);
        let (dx, dy) = gyro.update(&imu, 1.0, &cfg);
        assert_eq!(dx, 7);
        assert_eq!(dy, 0);
    }

    #[test]
    fn stick_mode_returns_clamped_f32_output() {
        let backend = Arc::new(MockBackend::new(tokio::sync::mpsc::unbounded_channel().0));
        let mut gyro = GyroMouse::with_backend(backend);
        let cfg = GyroMapping {
            mode: GyroMode::Stick(StickSide::Right),
            sensitivity: [0.01, 0.01],
            smoothing: 0.0,
            deadzone: 0.0,
        };

        // gyro_y = 100 dps -> raw_x = 100 -> x = 1.0 (clamped)
        // gyro_x = 50 dps -> raw_y = -50 -> y = -0.5
        let imu = make_imu(50.0, 100.0);
        let (dx, dy) = gyro.update(&imu, 1.0, &cfg);
        assert_eq!(dx, 0);
        assert_eq!(dy, 0);

        let (sx, sy) = gyro.stick_output();
        assert!(
            (sx - 1.0).abs() < 0.001,
            "x should clamp to 1.0, got {}",
            sx
        );
        assert!((sy - (-0.5)).abs() < 0.001, "y should be -0.5, got {}", sy);
    }

    #[test]
    fn recenter_clears_smoothing_and_stick_state() {
        let backend = Arc::new(MockBackend::new(tokio::sync::mpsc::unbounded_channel().0));
        let mut gyro = GyroMouse::with_backend(backend);
        let cfg = mouse_config();

        let imu = make_imu(0.0, 10.0);
        gyro.update(&imu, 1.0, &cfg);
        gyro.recenter();

        let (dx, dy) = gyro.update(&imu, 1.0, &cfg);
        assert_eq!(dx, 10);
        assert_eq!(dy, 0);
    }

    // ===================================================================
    //  updater tests
    // ===================================================================

    mod updater_tests {
        use crate::updater;

        #[test]
        fn update_version_newer() {
            assert!(updater::is_update_newer("0.1.0", "0.2.0").unwrap());
            assert!(updater::is_update_newer("0.1.0", "0.1.1").unwrap());
            assert!(updater::is_update_newer("0.1.0", "1.0.0").unwrap());
        }

        #[test]
        fn update_version_not_newer() {
            assert!(!updater::is_update_newer("0.2.0", "0.1.0").unwrap());
            assert!(!updater::is_update_newer("0.2.0", "0.2.0").unwrap());
            assert!(!updater::is_update_newer("1.0.0", "0.9.9").unwrap());
        }

        #[test]
        fn parse_update_manifest_top_level() {
            let value = serde_json::json!({
                "version": "0.2.0",
                "notes": "Wave 3 release",
                "pub_date": "2026-07-19T00:00:00Z",
                "signature": "sample-signature-placeholder",
                "url": "https://example.com/oxidelink/0.2.0/setup.exe"
            });
            let info = updater::parse_update_manifest(&value).expect("should parse");
            assert_eq!(info.version, "0.2.0");
            assert_eq!(info.notes, "Wave 3 release");
            assert_eq!(info.date, Some("2026-07-19T00:00:00Z".into()));
            assert_eq!(info.signature, Some("sample-signature-placeholder".into()));
        }

        #[test]
        fn parse_update_manifest_platform_signature() {
            let value = serde_json::json!({
                "version": "0.2.0",
                "notes": "Wave 3 release",
                "pub_date": "2026-07-19T00:00:00Z",
                "platforms": {
                    "windows-x86_64": {
                        "signature": "platform-signature",
                        "url": "https://example.com/oxidelink/0.2.0/setup.exe"
                    }
                }
            });
            let info = updater::parse_update_manifest(&value).expect("should parse");
            assert_eq!(info.version, "0.2.0");
            assert_eq!(info.signature, Some("platform-signature".into()));
        }

        #[test]
        fn generate_sample_manifest_parses() {
            let manifest = updater::generate_sample_update_manifest();
            let info = updater::parse_update_manifest(&manifest).expect("sample should parse");
            assert_eq!(info.version, "0.2.0");
            assert!(!info.notes.is_empty());
            assert!(info.signature.is_some());
        }
    }
}
