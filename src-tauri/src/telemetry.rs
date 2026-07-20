use crate::hid_parser::{
    battery_raw_to_percent, parse_standard_report, parse_subcmd_reply, ParsedInput, SubcmdReply,
};
use crate::state::{
    timestamp_now, ButtonState, ControllerState, ResponseCurveType, StickState, StickZones,
};
use log::debug;

pub struct TelemetryExtractor;

impl TelemetryExtractor {
    pub fn update_from_standard_report(
        state: &mut ControllerState,
        data: &[u8],
    ) -> Option<ParsedInput> {
        if let Some(parsed) = parse_standard_report(data) {
            state.buttons = parsed.buttons.clone();
            state.left_stick = parsed.left_stick.clone();
            state.right_stick = parsed.right_stick.clone();
            // Battery is in every 0x30 report (byte 2), not just subcmd replies.
            state.battery_raw = parsed.battery.raw;
            state.battery_percent = battery_raw_to_percent(parsed.battery.raw);
            state.charging = parsed.battery.charging;
            state.timestamp = timestamp_now();
            state.connected = true;
            // Propagate IMU data if present in the report.
            state.imu = parsed.imu.clone();

            // Update connection quality metrics derived from the timer byte.
            Self::update_connection_quality(state, parsed.timer);

            debug!(
                "Battery: {}% (raw=0x{:X}, charging={}), timer=0x{:02X}, latency={:.2}ms, rate={}Hz",
                state.battery_percent, parsed.battery.raw, state.charging, parsed.timer,
                state.connection_quality.latency_ms, state.connection_quality.report_rate_hz
            );
            Some(parsed)
        } else {
            None
        }
    }

    pub fn update_from_subcmd_reply(
        state: &mut ControllerState,
        data: &[u8],
    ) -> Option<SubcmdReply> {
        if let Some(reply) = parse_subcmd_reply(data) {
            state.battery_raw = reply.battery.raw;
            state.battery_percent = battery_raw_to_percent(reply.battery.raw);
            state.charging = reply.battery.charging;
            state.timestamp = timestamp_now();
            state.connected = true;
            debug!(
                "Subcmd reply 0x{:02X}: ACK=0x{:02X}, Battery: {}% (raw=0x{:X}, charging={})",
                reply.subcmd_id,
                reply.ack,
                state.battery_percent,
                reply.battery.raw,
                state.charging
            );
            Some(reply)
        } else {
            None
        }
    }

    pub fn update_signal_strength(state: &mut ControllerState, rssi: i8) {
        state.signal_strength = rssi;
        debug!("Signal strength: {} dBm", rssi);
    }

    pub fn check_battery_warning(state: &ControllerState, threshold: u8) -> bool {
        state.battery_percent > 0 && state.battery_percent <= threshold && !state.charging
    }

    pub fn apply_deadzone(stick: &mut StickState, deadzone: f32) {
        Self::apply_deadzone_with_shape(stick, deadzone, "radial");
    }

    pub fn apply_deadzone_with_shape(stick: &mut StickState, deadzone: f32, shape: &str) {
        let (nx, ny) = crate::curves::apply_deadzone_shape(stick.x, stick.y, deadzone, shape);
        stick.x = nx;
        stick.y = ny;
    }

    /// Apply deadzone shape, response curve, and zone logging to both sticks.
    pub fn apply_stick_curve_and_zones(
        state: &mut ControllerState,
        deadzone_left: f32,
        deadzone_right: f32,
        deadzone_shape: &str,
        response_curve: &ResponseCurveType,
        zones: &StickZones,
    ) {
        crate::curves::apply_stick_curve_and_zones(
            state,
            deadzone_left,
            deadzone_right,
            deadzone_shape,
            response_curve,
            zones,
        );
    }

    pub fn apply_remap(buttons: &mut ButtonState, remap: &crate::state::RemapConfig) {
        let original = buttons.clone();
        buttons.a = Self::remap_button(&remap.a_to, &original);
        buttons.b = Self::remap_button(&remap.b_to, &original);
        buttons.x = Self::remap_button(&remap.x_to, &original);
        buttons.y = Self::remap_button(&remap.y_to, &original);
    }

    fn remap_button(target: &str, original: &ButtonState) -> bool {
        match target.to_lowercase().as_str() {
            "a" => original.a,
            "b" => original.b,
            "x" => original.x,
            "y" => original.y,
            "l" => original.l,
            "r" => original.r,
            "zl" => original.zl,
            "zr" => original.zr,
            "minus" => original.minus,
            "plus" => original.plus,
            "home" => original.home,
            "capture" => original.capture,
            _ => false,
        }
    }

    /// Apply stick calibration to normalize raw stick values.
    pub fn apply_stick_calibration(
        raw_x: u16,
        raw_y: u16,
        cal: &crate::state::StickCalibration,
        is_left: bool,
    ) -> (f32, f32) {
        let (cx, cy, min_x, min_y, max_x, max_y) = if is_left {
            (
                cal.left_center_x,
                cal.left_center_y,
                cal.left_min_x,
                cal.left_min_y,
                cal.left_max_x,
                cal.left_max_y,
            )
        } else {
            (
                cal.right_center_x,
                cal.right_center_y,
                cal.right_min_x,
                cal.right_min_y,
                cal.right_max_x,
                cal.right_max_y,
            )
        };

        let x = crate::hid_parser::normalize_stick_calibrated(raw_x, cx, min_x, max_x);
        let y = crate::hid_parser::normalize_stick_calibrated(raw_y, cy, min_y, max_y);
        (x, y)
    }

    /// Update connection quality metrics from the timer byte.
    ///
    /// The Pro Controller increments an 8-bit timer every input report. By
    /// comparing consecutive reports we can estimate the inter-report gap and
    /// derive a smoothed latency / report-rate figure. At 120 Hz the expected
    /// gap is ~2 ticks; at 60 Hz it is ~4 ticks. Each tick is ~8.33 ms at the
    /// 120 Hz baseline.
    pub fn update_connection_quality(state: &mut ControllerState, timer: u8) {
        let quality = &mut state.connection_quality;

        if quality.last_report_timer != 0 {
            let gap = if timer >= quality.last_report_timer {
                (timer - quality.last_report_timer) as f32
            } else {
                ((256u32 - quality.last_report_timer as u32) + timer as u32) as f32
            };

            // At 120 Hz the expected gap is ~2 ticks per report; at 60 Hz ~4.
            let expected_gap = if state.imu_enabled { 2.0 } else { 4.0 };
            let latency = (gap / expected_gap) * 8.33; // 8.33 ms per tick at 120 Hz

            // Smoothed average (exponential moving average).
            if quality.latency_ms == 0.0 {
                quality.latency_ms = latency;
            } else {
                quality.latency_ms = quality.latency_ms * 0.9 + latency * 0.1;
            }

            // Estimate report rate from the per-report latency.
            if latency > 0.0 {
                let rate = 1000.0 / latency;
                quality.report_rate_hz = rate as u16;
            }
        }

        quality.last_report_timer = timer;
    }

    /// Update state from a device info reply.
    pub fn update_from_device_info(state: &mut ControllerState, info: crate::state::DeviceInfo) {
        state.device_info = Some(info);
    }

    /// Update state from stick calibration data.
    pub fn update_from_calibration(
        state: &mut ControllerState,
        cal: crate::state::StickCalibration,
    ) {
        state.stick_calibration = Some(cal);
    }

    /// Update player lights state.
    pub fn update_player_lights(state: &mut ControllerState, mask: u8, pattern: u8) {
        state.player_lights.led_mask = mask;
        state.player_lights.flash_pattern = pattern;
    }

    /// Update home light state.
    pub fn update_home_light(
        state: &mut ControllerState,
        enabled: bool,
        brightness: u8,
        pattern: u8,
    ) {
        state.home_light.enabled = enabled;
        state.home_light.brightness = brightness;
        state.home_light.pulse_pattern = pattern;
    }

    /// Build a [`CalibrationStatus`] snapshot from the pipeline state for
    /// telemetry / UI display.
    pub fn build_calibration_status(
        pipeline: &crate::stick_cal::StickCalibrationPipeline,
    ) -> crate::stick_cal::CalibrationStatus {
        pipeline.get_status()
    }

    /// Check whether stick calibration is present and valid in the controller
    /// state.
    pub fn has_valid_calibration(state: &ControllerState) -> bool {
        state
            .stick_calibration
            .as_ref()
            .map(|cal| cal.valid)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AppConfig, ConnectionQuality, DeviceInfo, HomeLight, PlayerLights, RemapConfig,
        StickCalibration,
    };

    // -----------------------------------------------------------------------
    //  check_battery_warning
    // -----------------------------------------------------------------------

    #[test]
    fn battery_warning_false_when_zero_percent() {
        let state = ControllerState::default();
        assert!(!TelemetryExtractor::check_battery_warning(&state, 15));
    }

    #[test]
    fn battery_warning_true_when_below_threshold_and_not_charging() {
        let mut state = ControllerState::default();
        state.battery_percent = 10;
        state.charging = false;
        assert!(TelemetryExtractor::check_battery_warning(&state, 15));
    }

    #[test]
    fn battery_warning_true_when_exactly_at_threshold() {
        let mut state = ControllerState::default();
        state.battery_percent = 15;
        state.charging = false;
        assert!(TelemetryExtractor::check_battery_warning(&state, 15));
    }

    #[test]
    fn battery_warning_false_when_above_threshold() {
        let mut state = ControllerState::default();
        state.battery_percent = 20;
        state.charging = false;
        assert!(!TelemetryExtractor::check_battery_warning(&state, 15));
    }

    #[test]
    fn battery_warning_false_when_charging() {
        let mut state = ControllerState::default();
        state.battery_percent = 5;
        state.charging = true;
        assert!(!TelemetryExtractor::check_battery_warning(&state, 15));
    }

    #[test]
    fn battery_warning_false_when_high_battery() {
        let mut state = ControllerState::default();
        state.battery_percent = 100;
        state.charging = false;
        assert!(!TelemetryExtractor::check_battery_warning(&state, 15));
    }

    // -----------------------------------------------------------------------
    //  update_signal_strength
    // -----------------------------------------------------------------------

    #[test]
    fn update_signal_strength_sets_value() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_signal_strength(&mut state, -42);
        assert_eq!(state.signal_strength, -42);
    }

    #[test]
    fn update_signal_strength_zero() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_signal_strength(&mut state, 0);
        assert_eq!(state.signal_strength, 0);
    }

    #[test]
    fn update_signal_strength_max_positive() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_signal_strength(&mut state, 127);
        assert_eq!(state.signal_strength, 127);
    }

    #[test]
    fn update_signal_strength_max_negative() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_signal_strength(&mut state, -128);
        assert_eq!(state.signal_strength, -128);
    }

    // -----------------------------------------------------------------------
    //  apply_deadzone / apply_deadzone_with_shape
    // -----------------------------------------------------------------------

    #[test]
    fn apply_deadzone_zeros_small_input() {
        let mut stick = StickState {
            x: 0.05,
            y: 0.03,
            ..Default::default()
        };
        TelemetryExtractor::apply_deadzone(&mut stick, 0.1);
        assert_eq!(stick.x, 0.0);
        assert_eq!(stick.y, 0.0);
    }

    #[test]
    fn apply_deadzone_preserves_large_input() {
        let mut stick = StickState {
            x: 0.5,
            y: 0.5,
            ..Default::default()
        };
        TelemetryExtractor::apply_deadzone(&mut stick, 0.1);
        // After radial deadzone, values should be scaled but non-zero.
        assert!(stick.x > 0.0);
        assert!(stick.y > 0.0);
    }

    #[test]
    fn apply_deadzone_with_shape_axial() {
        let mut stick = StickState {
            x: 0.05,
            y: 0.5,
            ..Default::default()
        };
        TelemetryExtractor::apply_deadzone_with_shape(&mut stick, 0.1, "axial");
        assert_eq!(stick.x, 0.0);
        assert!(stick.y > 0.0);
    }

    #[test]
    fn apply_deadzone_with_shape_unknown_defaults_to_radial() {
        let mut stick = StickState {
            x: 0.05,
            y: 0.05,
            ..Default::default()
        };
        TelemetryExtractor::apply_deadzone_with_shape(&mut stick, 0.1, "unknown");
        assert_eq!(stick.x, 0.0);
        assert_eq!(stick.y, 0.0);
    }

    #[test]
    fn apply_deadzone_zero_deadzone_passthrough() {
        let mut stick = StickState {
            x: 0.5,
            y: 0.5,
            ..Default::default()
        };
        TelemetryExtractor::apply_deadzone(&mut stick, 0.0);
        // With zero deadzone, radial shape still scales by (m-0)/(1-0) = m,
        // then divides by m, so values are unchanged.
        let m = (0.5_f32 * 0.5 + 0.5 * 0.5).sqrt();
        let scale = (m / (1.0 - 0.0)).clamp(0.0, 1.0) / m;
        assert!((stick.x - 0.5 * scale).abs() < 1e-5);
        assert!((stick.y - 0.5 * scale).abs() < 1e-5);
    }

    // -----------------------------------------------------------------------
    //  apply_remap / remap_button
    // -----------------------------------------------------------------------

    #[test]
    fn apply_remap_swaps_a_and_b() {
        let mut buttons = ButtonState::default();
        buttons.a = true;
        buttons.b = false;
        let remap = RemapConfig {
            a_to: "b".into(),
            b_to: "a".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        // After remap: a should get original.b (false), b should get original.a (true)
        assert!(!buttons.a);
        assert!(buttons.b);
    }

    #[test]
    fn apply_remap_swaps_x_and_y() {
        let mut buttons = ButtonState::default();
        buttons.x = true;
        buttons.y = false;
        let remap = RemapConfig {
            a_to: "a".into(),
            b_to: "b".into(),
            x_to: "y".into(),
            y_to: "x".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(!buttons.x);
        assert!(buttons.y);
    }

    #[test]
    fn apply_remap_to_l_trigger() {
        let mut buttons = ButtonState::default();
        buttons.l = true;
        let remap = RemapConfig {
            a_to: "l".into(),
            b_to: "b".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.a);
    }

    #[test]
    fn apply_remap_to_zl_trigger() {
        let mut buttons = ButtonState::default();
        buttons.zl = true;
        let remap = RemapConfig {
            a_to: "a".into(),
            b_to: "zl".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.b);
    }

    #[test]
    fn apply_remap_to_r_trigger() {
        let mut buttons = ButtonState::default();
        buttons.r = true;
        let remap = RemapConfig {
            a_to: "a".into(),
            b_to: "b".into(),
            x_to: "r".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.x);
    }

    #[test]
    fn apply_remap_to_zr_trigger() {
        let mut buttons = ButtonState::default();
        buttons.zr = true;
        let remap = RemapConfig {
            a_to: "a".into(),
            b_to: "b".into(),
            x_to: "x".into(),
            y_to: "zr".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.y);
    }

    #[test]
    fn apply_remap_to_minus() {
        let mut buttons = ButtonState::default();
        buttons.minus = true;
        let remap = RemapConfig {
            a_to: "minus".into(),
            b_to: "b".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.a);
    }

    #[test]
    fn apply_remap_to_plus() {
        let mut buttons = ButtonState::default();
        buttons.plus = true;
        let remap = RemapConfig {
            a_to: "a".into(),
            b_to: "plus".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.b);
    }

    #[test]
    fn apply_remap_to_home() {
        let mut buttons = ButtonState::default();
        buttons.home = true;
        let remap = RemapConfig {
            a_to: "home".into(),
            b_to: "b".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.a);
    }

    #[test]
    fn apply_remap_to_capture() {
        let mut buttons = ButtonState::default();
        buttons.capture = true;
        let remap = RemapConfig {
            a_to: "a".into(),
            b_to: "capture".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.b);
    }

    #[test]
    fn apply_remap_unknown_target_yields_false() {
        let mut buttons = ButtonState::default();
        buttons.a = true;
        let remap = RemapConfig {
            a_to: "nonexistent".into(),
            b_to: "b".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(!buttons.a);
    }

    #[test]
    fn apply_remap_case_insensitive_target() {
        let mut buttons = ButtonState::default();
        buttons.a = true;
        let remap = RemapConfig {
            a_to: "A".into(),
            b_to: "b".into(),
            x_to: "x".into(),
            y_to: "y".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(buttons.a);
    }

    #[test]
    fn apply_remap_no_source_pressed() {
        let buttons = ButtonState::default();
        let mut buttons = buttons;
        let remap = RemapConfig {
            a_to: "b".into(),
            b_to: "a".into(),
            x_to: "y".into(),
            y_to: "x".into(),
        };
        TelemetryExtractor::apply_remap(&mut buttons, &remap);
        assert!(!buttons.a);
        assert!(!buttons.b);
        assert!(!buttons.x);
        assert!(!buttons.y);
    }

    // -----------------------------------------------------------------------
    //  apply_stick_calibration
    // -----------------------------------------------------------------------

    #[test]
    fn apply_stick_calibration_center_returns_zero() {
        let cal = StickCalibration {
            left_center_x: 2048,
            left_center_y: 2048,
            left_min_x: 512,
            left_min_y: 512,
            left_max_x: 3584,
            left_max_y: 3584,
            ..Default::default()
        };
        let (x, y) = TelemetryExtractor::apply_stick_calibration(2048, 2048, &cal, true);
        assert!((x - 0.0).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
    }

    #[test]
    fn apply_stick_calibration_max_returns_one() {
        let cal = StickCalibration {
            left_center_x: 2048,
            left_center_y: 2048,
            left_min_x: 512,
            left_min_y: 512,
            left_max_x: 3584,
            left_max_y: 3584,
            ..Default::default()
        };
        let (x, y) = TelemetryExtractor::apply_stick_calibration(3584, 3584, &cal, true);
        assert!((x - 1.0).abs() < 1e-5);
        assert!((y - 1.0).abs() < 1e-5);
    }

    #[test]
    fn apply_stick_calibration_min_returns_negative_one() {
        let cal = StickCalibration {
            left_center_x: 2048,
            left_center_y: 2048,
            left_min_x: 512,
            left_min_y: 512,
            left_max_x: 3584,
            left_max_y: 3584,
            ..Default::default()
        };
        let (x, y) = TelemetryExtractor::apply_stick_calibration(512, 512, &cal, true);
        assert!((x - (-1.0)).abs() < 1e-5);
        assert!((y - (-1.0)).abs() < 1e-5);
    }

    #[test]
    fn apply_stick_calibration_right_stick_uses_right_values() {
        let cal = StickCalibration {
            right_center_x: 2000,
            right_center_y: 2000,
            right_min_x: 500,
            right_min_y: 500,
            right_max_x: 3500,
            right_max_y: 3500,
            ..Default::default()
        };
        let (x, y) = TelemetryExtractor::apply_stick_calibration(3500, 500, &cal, false);
        assert!((x - 1.0).abs() < 1e-5);
        assert!((y - (-1.0)).abs() < 1e-5);
    }

    #[test]
    fn apply_stick_calibration_clamps_beyond_max() {
        let cal = StickCalibration {
            left_center_x: 2048,
            left_center_y: 2048,
            left_min_x: 512,
            left_min_y: 512,
            left_max_x: 3584,
            left_max_y: 3584,
            ..Default::default()
        };
        let (x, y) = TelemetryExtractor::apply_stick_calibration(65535, 0, &cal, true);
        assert!((x - 1.0).abs() < 1e-5);
        assert!((y - (-1.0)).abs() < 1e-5);
    }

    #[test]
    fn apply_stick_calibration_zero_range_returns_zero() {
        let cal = StickCalibration {
            left_center_x: 2048,
            left_center_y: 2048,
            left_min_x: 2048,
            left_min_y: 2048,
            left_max_x: 2048,
            left_max_y: 2048,
            ..Default::default()
        };
        let (x, y) = TelemetryExtractor::apply_stick_calibration(3000, 1000, &cal, true);
        assert!((x - 0.0).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
    }

    // -----------------------------------------------------------------------
    //  update_connection_quality
    // -----------------------------------------------------------------------

    #[test]
    fn connection_quality_first_report_no_latency() {
        let mut state = ControllerState::default();
        state.connection_quality.last_report_timer = 0;
        TelemetryExtractor::update_connection_quality(&mut state, 10);
        // First report (last_report_timer was 0) should not compute latency.
        assert_eq!(state.connection_quality.latency_ms, 0.0);
        assert_eq!(state.connection_quality.last_report_timer, 10);
    }

    #[test]
    fn connection_quality_imu_enabled_expected_gap_two() {
        let mut state = ControllerState::default();
        state.imu_enabled = true;
        state.connection_quality.last_report_timer = 10;
        TelemetryExtractor::update_connection_quality(&mut state, 12);
        // gap=2, expected_gap=2.0, latency = (2/2)*8.33 = 8.33
        assert!((state.connection_quality.latency_ms - 8.33).abs() < 0.01);
        // rate = 1000/8.33 ≈ 120
        assert_eq!(state.connection_quality.report_rate_hz, (1000.0 / 8.33) as u16);
    }

    #[test]
    fn connection_quality_imu_disabled_expected_gap_four() {
        let mut state = ControllerState::default();
        state.imu_enabled = false;
        state.connection_quality.last_report_timer = 10;
        TelemetryExtractor::update_connection_quality(&mut state, 14);
        // gap=4, expected_gap=4.0, latency = (4/4)*8.33 = 8.33
        assert!((state.connection_quality.latency_ms - 8.33).abs() < 0.01);
    }

    #[test]
    fn connection_quality_timer_wraparound() {
        let mut state = ControllerState::default();
        state.imu_enabled = true;
        state.connection_quality.last_report_timer = 255;
        TelemetryExtractor::update_connection_quality(&mut state, 1);
        // gap = (256 - 255) + 1 = 2, expected_gap=2.0, latency = 8.33
        assert!((state.connection_quality.latency_ms - 8.33).abs() < 0.01);
    }

    #[test]
    fn connection_quality_ema_smoothing() {
        let mut state = ControllerState::default();
        state.imu_enabled = true;
        state.connection_quality.last_report_timer = 10;
        state.connection_quality.latency_ms = 10.0;
        // gap=2, latency=8.33, EMA: 10.0*0.9 + 8.33*0.1 = 9.833
        TelemetryExtractor::update_connection_quality(&mut state, 12);
        let expected = 10.0 * 0.9 + 8.33 * 0.1;
        assert!((state.connection_quality.latency_ms - expected).abs() < 0.01);
    }

    #[test]
    fn connection_quality_large_gap_lower_rate() {
        let mut state = ControllerState::default();
        state.imu_enabled = true;
        state.connection_quality.last_report_timer = 10;
        // gap=10, expected_gap=2.0, latency = (10/2)*8.33 = 41.65
        TelemetryExtractor::update_connection_quality(&mut state, 20);
        assert!((state.connection_quality.latency_ms - 41.65).abs() < 0.01);
        // rate = 1000/41.65 ≈ 24
        assert_eq!(state.connection_quality.report_rate_hz, (1000.0 / 41.65) as u16);
    }

    // -----------------------------------------------------------------------
    //  update_from_device_info
    // -----------------------------------------------------------------------

    #[test]
    fn update_from_device_info_sets_state() {
        let mut state = ControllerState::default();
        let info = DeviceInfo {
            firmware_version: "2.0".into(),
            controller_type: 1,
            mac_address: "AA:BB:CC:DD:EE:FF".into(),
            colors_from_spi: true,
            connection: "Bluetooth".into(),
            spi: None,
        };
        TelemetryExtractor::update_from_device_info(&mut state, info.clone());
        assert!(state.device_info.is_some());
        let di = state.device_info.as_ref().unwrap();
        assert_eq!(di.firmware_version, "2.0");
        assert_eq!(di.controller_type, 1);
        assert_eq!(di.mac_address, "AA:BB:CC:DD:EE:FF");
        assert!(di.colors_from_spi);
        assert_eq!(di.connection, "Bluetooth");
    }

    #[test]
    fn update_from_device_info_overwrites_previous() {
        let mut state = ControllerState::default();
        let info1 = DeviceInfo {
            firmware_version: "1.0".into(),
            ..Default::default()
        };
        TelemetryExtractor::update_from_device_info(&mut state, info1);
        let info2 = DeviceInfo {
            firmware_version: "3.0".into(),
            ..Default::default()
        };
        TelemetryExtractor::update_from_device_info(&mut state, info2);
        assert_eq!(
            state.device_info.as_ref().unwrap().firmware_version,
            "3.0"
        );
    }

    // -----------------------------------------------------------------------
    //  update_from_calibration
    // -----------------------------------------------------------------------

    #[test]
    fn update_from_calibration_sets_state() {
        let mut state = ControllerState::default();
        let cal = StickCalibration {
            left_center_x: 2048,
            valid: true,
            ..Default::default()
        };
        TelemetryExtractor::update_from_calibration(&mut state, cal.clone());
        assert!(state.stick_calibration.is_some());
        assert!(state.stick_calibration.as_ref().unwrap().valid);
        assert_eq!(
            state.stick_calibration.as_ref().unwrap().left_center_x,
            2048
        );
    }

    #[test]
    fn update_from_calibration_overwrites_previous() {
        let mut state = ControllerState::default();
        let cal1 = StickCalibration {
            source: "factory".into(),
            ..Default::default()
        };
        TelemetryExtractor::update_from_calibration(&mut state, cal1);
        let cal2 = StickCalibration {
            source: "user".into(),
            ..Default::default()
        };
        TelemetryExtractor::update_from_calibration(&mut state, cal2);
        assert_eq!(state.stick_calibration.as_ref().unwrap().source, "user");
    }

    // -----------------------------------------------------------------------
    //  update_player_lights
    // -----------------------------------------------------------------------

    #[test]
    fn update_player_lights_sets_values() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_player_lights(&mut state, 0b0001, 0b0010);
        assert_eq!(state.player_lights.led_mask, 0b0001);
        assert_eq!(state.player_lights.flash_pattern, 0b0010);
    }

    #[test]
    fn update_player_lights_zero_mask() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_player_lights(&mut state, 0, 0);
        assert_eq!(state.player_lights.led_mask, 0);
        assert_eq!(state.player_lights.flash_pattern, 0);
    }

    #[test]
    fn update_player_lights_all_leds() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_player_lights(&mut state, 0xFF, 0xFF);
        assert_eq!(state.player_lights.led_mask, 0xFF);
        assert_eq!(state.player_lights.flash_pattern, 0xFF);
    }

    // -----------------------------------------------------------------------
    //  update_home_light
    // -----------------------------------------------------------------------

    #[test]
    fn update_home_light_enabled() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_home_light(&mut state, true, 128, 0b0101);
        assert!(state.home_light.enabled);
        assert_eq!(state.home_light.brightness, 128);
        assert_eq!(state.home_light.pulse_pattern, 0b0101);
    }

    #[test]
    fn update_home_light_disabled() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_home_light(&mut state, false, 0, 0);
        assert!(!state.home_light.enabled);
        assert_eq!(state.home_light.brightness, 0);
        assert_eq!(state.home_light.pulse_pattern, 0);
    }

    #[test]
    fn update_home_light_max_brightness() {
        let mut state = ControllerState::default();
        TelemetryExtractor::update_home_light(&mut state, true, 255, 0xFF);
        assert!(state.home_light.enabled);
        assert_eq!(state.home_light.brightness, 255);
        assert_eq!(state.home_light.pulse_pattern, 0xFF);
    }

    // -----------------------------------------------------------------------
    //  has_valid_calibration
    // -----------------------------------------------------------------------

    #[test]
    fn has_valid_calibration_false_when_none() {
        let state = ControllerState::default();
        assert!(!TelemetryExtractor::has_valid_calibration(&state));
    }

    #[test]
    fn has_valid_calibration_false_when_invalid() {
        let mut state = ControllerState::default();
        state.stick_calibration = Some(StickCalibration {
            valid: false,
            ..Default::default()
        });
        assert!(!TelemetryExtractor::has_valid_calibration(&state));
    }

    #[test]
    fn has_valid_calibration_true_when_valid() {
        let mut state = ControllerState::default();
        state.stick_calibration = Some(StickCalibration {
            valid: true,
            ..Default::default()
        });
        assert!(TelemetryExtractor::has_valid_calibration(&state));
    }

    // -----------------------------------------------------------------------
    //  AppConfig default battery_warning_threshold
    // -----------------------------------------------------------------------

    #[test]
    fn app_config_default_battery_warning_threshold() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.battery_warning_threshold, 15);
    }

    // -----------------------------------------------------------------------
    //  ConnectionQuality default
    // -----------------------------------------------------------------------

    #[test]
    fn connection_quality_default_values() {
        let cq = ConnectionQuality::default();
        assert_eq!(cq.latency_ms, 0.0);
        assert_eq!(cq.packet_loss_rate, 0.0);
        assert_eq!(cq.last_report_timer, 0);
        assert_eq!(cq.report_rate_hz, 0);
        assert_eq!(cq.total_packets, 0);
        assert_eq!(cq.dropped, 0);
        assert_eq!(cq.retries, 0);
    }

    // -----------------------------------------------------------------------
    //  PlayerLights / HomeLight defaults
    // -----------------------------------------------------------------------

    #[test]
    fn player_lights_default_values() {
        let pl = PlayerLights::default();
        assert_eq!(pl.led_mask, 0);
        assert_eq!(pl.flash_pattern, 0);
    }

    #[test]
    fn home_light_default_values() {
        let hl = HomeLight::default();
        assert!(!hl.enabled);
        assert_eq!(hl.brightness, 0);
        assert_eq!(hl.pulse_pattern, 0);
    }
}
