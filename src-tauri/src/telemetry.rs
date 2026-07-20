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
