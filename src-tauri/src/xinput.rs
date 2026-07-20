use crate::imu::{map_gyro_to_stick, GyroAimConfig, ImuPhysical};
use crate::state::ButtonState;

#[derive(Debug, Clone, Default)]
pub struct XInputState {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

pub const XINPUT_GAMEPAD_A: u16 = 0x1000;
pub const XINPUT_GAMEPAD_B: u16 = 0x2000;
pub const XINPUT_GAMEPAD_X: u16 = 0x4000;
pub const XINPUT_GAMEPAD_Y: u16 = 0x8000;
pub const XINPUT_GAMEPAD_LEFT_SHOULDER: u16 = 0x0100;
pub const XINPUT_GAMEPAD_RIGHT_SHOULDER: u16 = 0x0200;
pub const XINPUT_GAMEPAD_LEFT_THUMB: u16 = 0x0040;
pub const XINPUT_GAMEPAD_RIGHT_THUMB: u16 = 0x0080;
pub const XINPUT_GAMEPAD_BACK: u16 = 0x0020;
pub const XINPUT_GAMEPAD_START: u16 = 0x0010;
pub const XINPUT_GAMEPAD_GUIDE: u16 = 0x0400;
pub const XINPUT_GAMEPAD_DPAD_UP: u16 = 0x0001;
pub const XINPUT_GAMEPAD_DPAD_DOWN: u16 = 0x0002;
pub const XINPUT_GAMEPAD_DPAD_LEFT: u16 = 0x0004;
pub const XINPUT_GAMEPAD_DPAD_RIGHT: u16 = 0x0008;

pub fn map_to_xinput(
    buttons: &ButtonState,
    left_stick: &crate::state::StickState,
    right_stick: &crate::state::StickState,
    zl_analog: f32,
    zr_analog: f32,
) -> XInputState {
    let mut btn = 0u16;

    if buttons.a {
        btn |= XINPUT_GAMEPAD_A;
    }
    if buttons.b {
        btn |= XINPUT_GAMEPAD_B;
    }
    if buttons.x {
        btn |= XINPUT_GAMEPAD_X;
    }
    if buttons.y {
        btn |= XINPUT_GAMEPAD_Y;
    }
    if buttons.l {
        btn |= XINPUT_GAMEPAD_LEFT_SHOULDER;
    }
    if buttons.r {
        btn |= XINPUT_GAMEPAD_RIGHT_SHOULDER;
    }
    if buttons.stick_l {
        btn |= XINPUT_GAMEPAD_LEFT_THUMB;
    }
    if buttons.stick_r {
        btn |= XINPUT_GAMEPAD_RIGHT_THUMB;
    }
    if buttons.minus {
        btn |= XINPUT_GAMEPAD_BACK;
    }
    if buttons.plus {
        btn |= XINPUT_GAMEPAD_START;
    }
    if buttons.home {
        btn |= XINPUT_GAMEPAD_GUIDE;
    }
    if buttons.dpad_up {
        btn |= XINPUT_GAMEPAD_DPAD_UP;
    }
    if buttons.dpad_down {
        btn |= XINPUT_GAMEPAD_DPAD_DOWN;
    }
    if buttons.dpad_left {
        btn |= XINPUT_GAMEPAD_DPAD_LEFT;
    }
    if buttons.dpad_right {
        btn |= XINPUT_GAMEPAD_DPAD_RIGHT;
    }

    let thumb_lx = (left_stick.x * 32767.0) as i16;
    let thumb_ly = (left_stick.y * 32767.0) as i16;
    let thumb_rx = (right_stick.x * 32767.0) as i16;
    let thumb_ry = (right_stick.y * 32767.0) as i16;

    let left_trigger = (zl_analog * 255.0) as u8;
    let right_trigger = (zr_analog * 255.0) as u8;

    XInputState {
        buttons: btn,
        left_trigger,
        right_trigger,
        thumb_lx,
        thumb_ly,
        thumb_rx,
        thumb_ry,
    }
}

pub fn xinput_state_to_hex(state: &XInputState) -> String {
    format!(
        "{:04X} {:02X} {:02X} {:04X} {:04X} {:04X} {:04X}",
        state.buttons,
        state.left_trigger,
        state.right_trigger,
        (state.thumb_lx as u16),
        (state.thumb_ly as u16),
        (state.thumb_rx as u16),
        (state.thumb_ry as u16),
    )
}

pub fn default_nintendo_to_xinput_remap() -> crate::state::RemapConfig {
    crate::state::RemapConfig {
        a_to: "b".into(),
        b_to: "a".into(),
        x_to: "y".into(),
        y_to: "x".into(),
    }
}

/// Map to XInput with optional gyro-aim augmentation.
///
/// Starts from the base right-stick value, then adds gyro-derived deflection
/// (clamped to the -1.0..=1.0 range) when a gyro sample and an enabled
/// [`GyroAimConfig`] are supplied. All other axes/buttons/triggers are mapped
/// exactly as in [`map_to_xinput`]; only the right thumbstick is overridden.
pub fn map_to_xinput_with_gyro(
    buttons: &ButtonState,
    left_stick: &crate::state::StickState,
    right_stick: &crate::state::StickState,
    zl_analog: f32,
    zr_analog: f32,
    gyro: Option<&ImuPhysical>,
    gyro_config: &GyroAimConfig,
) -> XInputState {
    // Start from the base right-stick deflection.
    let mut rx = right_stick.x;
    let mut ry = right_stick.y;

    // Augment with gyro aim when a sample is available. `map_gyro_to_stick`
    // already honours `gyro_config.enabled`, returning (0, 0) when disabled.
    if let Some(g) = gyro {
        let (gx, gy) = map_gyro_to_stick(g, gyro_config);
        rx = (rx + gx).clamp(-1.0, 1.0);
        ry = (ry + gy).clamp(-1.0, 1.0);
    }

    // Build the base XInput state, then override the right thumbstick with the
    // gyro-augmented values.
    let mut xi = map_to_xinput(buttons, left_stick, right_stick, zl_analog, zr_analog);
    xi.thumb_rx = (rx * 32767.0) as i16;
    xi.thumb_ry = (ry * 32767.0) as i16;
    xi
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imu::{GyroAimConfig, ImuPhysical};
    use crate::state::{ButtonState, RemapConfig, StickState};

    // -----------------------------------------------------------------------
    //  XInputState defaults
    // -----------------------------------------------------------------------

    #[test]
    fn xinput_state_default_all_zero() {
        let state = XInputState::default();
        assert_eq!(state.buttons, 0);
        assert_eq!(state.left_trigger, 0);
        assert_eq!(state.right_trigger, 0);
        assert_eq!(state.thumb_lx, 0);
        assert_eq!(state.thumb_ly, 0);
        assert_eq!(state.thumb_rx, 0);
        assert_eq!(state.thumb_ry, 0);
    }

    // -----------------------------------------------------------------------
    //  Button constant values
    // -----------------------------------------------------------------------

    #[test]
    fn button_constants_are_distinct() {
        let all = [
            XINPUT_GAMEPAD_A,
            XINPUT_GAMEPAD_B,
            XINPUT_GAMEPAD_X,
            XINPUT_GAMEPAD_Y,
            XINPUT_GAMEPAD_LEFT_SHOULDER,
            XINPUT_GAMEPAD_RIGHT_SHOULDER,
            XINPUT_GAMEPAD_LEFT_THUMB,
            XINPUT_GAMEPAD_RIGHT_THUMB,
            XINPUT_GAMEPAD_BACK,
            XINPUT_GAMEPAD_START,
            XINPUT_GAMEPAD_GUIDE,
            XINPUT_GAMEPAD_DPAD_UP,
            XINPUT_GAMEPAD_DPAD_DOWN,
            XINPUT_GAMEPAD_DPAD_LEFT,
            XINPUT_GAMEPAD_DPAD_RIGHT,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "constants at {i} and {j} collide");
            }
        }
    }

    #[test]
    fn button_constants_expected_values() {
        assert_eq!(XINPUT_GAMEPAD_A, 0x1000);
        assert_eq!(XINPUT_GAMEPAD_B, 0x2000);
        assert_eq!(XINPUT_GAMEPAD_X, 0x4000);
        assert_eq!(XINPUT_GAMEPAD_Y, 0x8000);
        assert_eq!(XINPUT_GAMEPAD_LEFT_SHOULDER, 0x0100);
        assert_eq!(XINPUT_GAMEPAD_RIGHT_SHOULDER, 0x0200);
        assert_eq!(XINPUT_GAMEPAD_LEFT_THUMB, 0x0040);
        assert_eq!(XINPUT_GAMEPAD_RIGHT_THUMB, 0x0080);
        assert_eq!(XINPUT_GAMEPAD_BACK, 0x0020);
        assert_eq!(XINPUT_GAMEPAD_START, 0x0010);
        assert_eq!(XINPUT_GAMEPAD_GUIDE, 0x0400);
        assert_eq!(XINPUT_GAMEPAD_DPAD_UP, 0x0001);
        assert_eq!(XINPUT_GAMEPAD_DPAD_DOWN, 0x0002);
        assert_eq!(XINPUT_GAMEPAD_DPAD_LEFT, 0x0004);
        assert_eq!(XINPUT_GAMEPAD_DPAD_RIGHT, 0x0008);
    }

    // -----------------------------------------------------------------------
    //  map_to_xinput — buttons
    // -----------------------------------------------------------------------

    #[test]
    fn map_to_xinput_no_buttons_pressed() {
        let buttons = ButtonState::default();
        let left = StickState::default();
        let right = StickState::default();
        let state = map_to_xinput(&buttons, &left, &right, 0.0, 0.0);
        assert_eq!(state.buttons, 0);
    }

    #[test]
    fn map_to_xinput_a_button() {
        let mut buttons = ButtonState::default();
        buttons.a = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_A);
    }

    #[test]
    fn map_to_xinput_b_button() {
        let mut buttons = ButtonState::default();
        buttons.b = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_B);
    }

    #[test]
    fn map_to_xinput_x_button() {
        let mut buttons = ButtonState::default();
        buttons.x = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_X);
    }

    #[test]
    fn map_to_xinput_y_button() {
        let mut buttons = ButtonState::default();
        buttons.y = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_Y);
    }

    #[test]
    fn map_to_xinput_left_shoulder() {
        let mut buttons = ButtonState::default();
        buttons.l = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_LEFT_SHOULDER);
    }

    #[test]
    fn map_to_xinput_right_shoulder() {
        let mut buttons = ButtonState::default();
        buttons.r = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_RIGHT_SHOULDER);
    }

    #[test]
    fn map_to_xinput_left_thumb() {
        let mut buttons = ButtonState::default();
        buttons.stick_l = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_LEFT_THUMB);
    }

    #[test]
    fn map_to_xinput_right_thumb() {
        let mut buttons = ButtonState::default();
        buttons.stick_r = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_RIGHT_THUMB);
    }

    #[test]
    fn map_to_xinput_back_button() {
        let mut buttons = ButtonState::default();
        buttons.minus = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_BACK);
    }

    #[test]
    fn map_to_xinput_start_button() {
        let mut buttons = ButtonState::default();
        buttons.plus = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_START);
    }

    #[test]
    fn map_to_xinput_guide_button() {
        let mut buttons = ButtonState::default();
        buttons.home = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_GUIDE);
    }

    #[test]
    fn map_to_xinput_dpad_up() {
        let mut buttons = ButtonState::default();
        buttons.dpad_up = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_DPAD_UP);
    }

    #[test]
    fn map_to_xinput_dpad_down() {
        let mut buttons = ButtonState::default();
        buttons.dpad_down = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_DPAD_DOWN);
    }

    #[test]
    fn map_to_xinput_dpad_left() {
        let mut buttons = ButtonState::default();
        buttons.dpad_left = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_DPAD_LEFT);
    }

    #[test]
    fn map_to_xinput_dpad_right() {
        let mut buttons = ButtonState::default();
        buttons.dpad_right = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        assert_eq!(state.buttons, XINPUT_GAMEPAD_DPAD_RIGHT);
    }

    #[test]
    fn map_to_xinput_all_buttons_combined() {
        let mut buttons = ButtonState::default();
        buttons.a = true;
        buttons.b = true;
        buttons.x = true;
        buttons.y = true;
        buttons.l = true;
        buttons.r = true;
        buttons.stick_l = true;
        buttons.stick_r = true;
        buttons.minus = true;
        buttons.plus = true;
        buttons.home = true;
        buttons.dpad_up = true;
        buttons.dpad_down = true;
        buttons.dpad_left = true;
        buttons.dpad_right = true;
        let state = map_to_xinput(&buttons, &StickState::default(), &StickState::default(), 0.0, 0.0);
        let expected = XINPUT_GAMEPAD_A
            | XINPUT_GAMEPAD_B
            | XINPUT_GAMEPAD_X
            | XINPUT_GAMEPAD_Y
            | XINPUT_GAMEPAD_LEFT_SHOULDER
            | XINPUT_GAMEPAD_RIGHT_SHOULDER
            | XINPUT_GAMEPAD_LEFT_THUMB
            | XINPUT_GAMEPAD_RIGHT_THUMB
            | XINPUT_GAMEPAD_BACK
            | XINPUT_GAMEPAD_START
            | XINPUT_GAMEPAD_GUIDE
            | XINPUT_GAMEPAD_DPAD_UP
            | XINPUT_GAMEPAD_DPAD_DOWN
            | XINPUT_GAMEPAD_DPAD_LEFT
            | XINPUT_GAMEPAD_DPAD_RIGHT;
        assert_eq!(state.buttons, expected);
    }

    // -----------------------------------------------------------------------
    //  map_to_xinput — sticks
    // -----------------------------------------------------------------------

    #[test]
    fn map_to_xinput_stick_full_positive() {
        let left = StickState {
            x: 1.0,
            y: 1.0,
            ..Default::default()
        };
        let state = map_to_xinput(&ButtonState::default(), &left, &StickState::default(), 0.0, 0.0);
        assert_eq!(state.thumb_lx, 32767);
        assert_eq!(state.thumb_ly, 32767);
        assert_eq!(state.thumb_rx, 0);
        assert_eq!(state.thumb_ry, 0);
    }

    #[test]
    fn map_to_xinput_stick_full_negative() {
        let left = StickState {
            x: -1.0,
            y: -1.0,
            ..Default::default()
        };
        let state = map_to_xinput(&ButtonState::default(), &left, &StickState::default(), 0.0, 0.0);
        assert_eq!(state.thumb_lx, -32767);
        assert_eq!(state.thumb_ly, -32767);
    }

    #[test]
    fn map_to_xinput_right_stick() {
        let right = StickState {
            x: 0.5,
            y: -0.5,
            ..Default::default()
        };
        let state = map_to_xinput(&ButtonState::default(), &StickState::default(), &right, 0.0, 0.0);
        assert_eq!(state.thumb_rx, (0.5 * 32767.0) as i16);
        assert_eq!(state.thumb_ry, (-0.5 * 32767.0) as i16);
    }

    #[test]
    fn map_to_xinput_stick_zero() {
        let state = map_to_xinput(
            &ButtonState::default(),
            &StickState::default(),
            &StickState::default(),
            0.0,
            0.0,
        );
        assert_eq!(state.thumb_lx, 0);
        assert_eq!(state.thumb_ly, 0);
        assert_eq!(state.thumb_rx, 0);
        assert_eq!(state.thumb_ry, 0);
    }

    // -----------------------------------------------------------------------
    //  map_to_xinput — triggers
    // -----------------------------------------------------------------------

    #[test]
    fn map_to_xinput_triggers_zero() {
        let state = map_to_xinput(
            &ButtonState::default(),
            &StickState::default(),
            &StickState::default(),
            0.0,
            0.0,
        );
        assert_eq!(state.left_trigger, 0);
        assert_eq!(state.right_trigger, 0);
    }

    #[test]
    fn map_to_xinput_triggers_full() {
        let state = map_to_xinput(
            &ButtonState::default(),
            &StickState::default(),
            &StickState::default(),
            1.0,
            1.0,
        );
        assert_eq!(state.left_trigger, 255);
        assert_eq!(state.right_trigger, 255);
    }

    #[test]
    fn map_to_xinput_triggers_half() {
        let state = map_to_xinput(
            &ButtonState::default(),
            &StickState::default(),
            &StickState::default(),
            0.5,
            0.5,
        );
        assert_eq!(state.left_trigger, (0.5 * 255.0) as u8);
        assert_eq!(state.right_trigger, (0.5 * 255.0) as u8);
    }

    #[test]
    fn map_to_xinput_triggers_clamped() {
        let state = map_to_xinput(
            &ButtonState::default(),
            &StickState::default(),
            &StickState::default(),
            2.0,
            2.0,
        );
        // (2.0 * 255.0) as u8 saturates to 255 in Rust (float-to-int casts
        // saturate since Rust 1.45, they do not wrap).
        assert_eq!(state.left_trigger, 255u8);
        assert_eq!(state.right_trigger, 255u8);
    }

    // -----------------------------------------------------------------------
    //  xinput_state_to_hex
    // -----------------------------------------------------------------------

    #[test]
    fn xinput_state_to_hex_format() {
        let state = XInputState {
            buttons: 0x1234,
            left_trigger: 0xAB,
            right_trigger: 0xCD,
            thumb_lx: 0x1111,
            thumb_ly: 0x2222,
            thumb_rx: 0x3333,
            thumb_ry: 0x4444,
        };
        let hex = xinput_state_to_hex(&state);
        assert_eq!(hex, "1234 AB CD 1111 2222 3333 4444");
    }

    #[test]
    fn xinput_state_to_hex_all_zero() {
        let state = XInputState::default();
        let hex = xinput_state_to_hex(&state);
        assert_eq!(hex, "0000 00 00 0000 0000 0000 0000");
    }

    #[test]
    fn xinput_state_to_hex_negative_thumb() {
        let state = XInputState {
            thumb_lx: -1,
            thumb_ly: -32768,
            ..Default::default()
        };
        let hex = xinput_state_to_hex(&state);
        // -1 as u16 = 0xFFFF, -32768 as u16 = 0x8000
        assert_eq!(hex, "0000 00 00 FFFF 8000 0000 0000");
    }

    #[test]
    fn xinput_state_to_hex_max_values() {
        let state = XInputState {
            buttons: 0xFFFF,
            left_trigger: 0xFF,
            right_trigger: 0xFF,
            thumb_lx: 0x7FFF,
            thumb_ly: 0x7FFF,
            thumb_rx: 0x7FFF,
            thumb_ry: 0x7FFF,
        };
        let hex = xinput_state_to_hex(&state);
        assert_eq!(hex, "FFFF FF FF 7FFF 7FFF 7FFF 7FFF");
    }

    // -----------------------------------------------------------------------
    //  default_nintendo_to_xinput_remap
    // -----------------------------------------------------------------------

    #[test]
    fn default_remap_swaps_ab_and_xy() {
        let remap = default_nintendo_to_xinput_remap();
        assert_eq!(remap.a_to, "b");
        assert_eq!(remap.b_to, "a");
        assert_eq!(remap.x_to, "y");
        assert_eq!(remap.y_to, "x");
    }

    #[test]
    fn default_remap_matches_app_config_default() {
        let remap = default_nintendo_to_xinput_remap();
        let cfg_remap = crate::state::AppConfig::default().button_remap;
        assert_eq!(remap.a_to, cfg_remap.a_to);
        assert_eq!(remap.b_to, cfg_remap.b_to);
        assert_eq!(remap.x_to, cfg_remap.x_to);
        assert_eq!(remap.y_to, cfg_remap.y_to);
    }

    // -----------------------------------------------------------------------
    //  map_to_xinput_with_gyro
    // -----------------------------------------------------------------------

    #[test]
    fn map_to_xinput_with_gyro_disabled_no_change() {
        let buttons = ButtonState::default();
        let right = StickState {
            x: 0.5,
            y: 0.3,
            ..Default::default()
        };
        let gyro = ImuPhysical {
            gyro_x: 100.0,
            gyro_y: 100.0,
            ..Default::default()
        };
        let config = GyroAimConfig::default(); // enabled = false
        let state = map_to_xinput_with_gyro(
            &buttons,
            &StickState::default(),
            &right,
            0.0,
            0.0,
            Some(&gyro),
            &config,
        );
        // Gyro disabled, so right stick should be unchanged.
        assert_eq!(state.thumb_rx, (0.5 * 32767.0) as i16);
        assert_eq!(state.thumb_ry, (0.3 * 32767.0) as i16);
    }

    #[test]
    fn map_to_xinput_with_gyro_none_no_change() {
        let right = StickState {
            x: 0.5,
            y: 0.3,
            ..Default::default()
        };
        let config = GyroAimConfig {
            enabled: true,
            ..Default::default()
        };
        let state = map_to_xinput_with_gyro(
            &ButtonState::default(),
            &StickState::default(),
            &right,
            0.0,
            0.0,
            None,
            &config,
        );
        assert_eq!(state.thumb_rx, (0.5 * 32767.0) as i16);
        assert_eq!(state.thumb_ry, (0.3 * 32767.0) as i16);
    }

    #[test]
    fn map_to_xinput_with_gyro_enabled_adds_deflection() {
        let right = StickState {
            x: 0.0,
            y: 0.0,
            ..Default::default()
        };
        let gyro = ImuPhysical {
            gyro_x: 0.0,
            gyro_y: 50.0,
            ..Default::default()
        };
        let config = GyroAimConfig {
            enabled: true,
            sensitivity: 0.01,
            deadzone: 2.0,
        };
        let state = map_to_xinput_with_gyro(
            &ButtonState::default(),
            &StickState::default(),
            &right,
            0.0,
            0.0,
            Some(&gyro),
            &config,
        );
        // gyro_y=50, x = 50*0.01 = 0.5
        assert_eq!(state.thumb_rx, (0.5 * 32767.0) as i16);
        assert_eq!(state.thumb_ry, 0);
    }

    #[test]
    fn map_to_xinput_with_gyro_clamps_to_max() {
        let right = StickState {
            x: 0.5,
            y: 0.0,
            ..Default::default()
        };
        let gyro = ImuPhysical {
            gyro_x: 0.0,
            gyro_y: 200.0,
            ..Default::default()
        };
        let config = GyroAimConfig {
            enabled: true,
            sensitivity: 0.01,
            deadzone: 2.0,
        };
        let state = map_to_xinput_with_gyro(
            &ButtonState::default(),
            &StickState::default(),
            &right,
            0.0,
            0.0,
            Some(&gyro),
            &config,
        );
        // rx = 0.5 + 2.0 = 2.5, clamped to 1.0
        assert_eq!(state.thumb_rx, 32767);
    }

    #[test]
    fn map_to_xinput_with_gyro_clamps_to_min() {
        let right = StickState {
            x: -0.5,
            y: 0.0,
            ..Default::default()
        };
        let gyro = ImuPhysical {
            gyro_x: 0.0,
            gyro_y: -200.0,
            ..Default::default()
        };
        let config = GyroAimConfig {
            enabled: true,
            sensitivity: 0.01,
            deadzone: 2.0,
        };
        let state = map_to_xinput_with_gyro(
            &ButtonState::default(),
            &StickState::default(),
            &right,
            0.0,
            0.0,
            Some(&gyro),
            &config,
        );
        // rx = -0.5 + (-2.0) = -2.5, clamped to -1.0
        assert_eq!(state.thumb_rx, -32767);
    }

    #[test]
    fn map_to_xinput_with_gyro_deadzone_zeros_small_rotation() {
        let right = StickState::default();
        let gyro = ImuPhysical {
            gyro_x: 1.0, // below deadzone of 2.0
            gyro_y: 1.0,
            ..Default::default()
        };
        let config = GyroAimConfig {
            enabled: true,
            sensitivity: 0.01,
            deadzone: 2.0,
        };
        let state = map_to_xinput_with_gyro(
            &ButtonState::default(),
            &StickState::default(),
            &right,
            0.0,
            0.0,
            Some(&gyro),
            &config,
        );
        assert_eq!(state.thumb_rx, 0);
        assert_eq!(state.thumb_ry, 0);
    }

    #[test]
    fn map_to_xinput_with_gyro_preserves_buttons_and_triggers() {
        let mut buttons = ButtonState::default();
        buttons.a = true;
        let left = StickState {
            x: 0.5,
            y: 0.0,
            ..Default::default()
        };
        let config = GyroAimConfig::default();
        let state = map_to_xinput_with_gyro(
            &buttons,
            &left,
            &StickState::default(),
            0.7,
            0.3,
            None,
            &config,
        );
        assert_eq!(state.buttons, XINPUT_GAMEPAD_A);
        assert_eq!(state.thumb_lx, (0.5 * 32767.0) as i16);
        assert_eq!(state.left_trigger, (0.7 * 255.0) as u8);
        assert_eq!(state.right_trigger, (0.3 * 255.0) as u8);
    }

    // -----------------------------------------------------------------------
    //  RemapConfig field check (no PartialEq derive)
    // -----------------------------------------------------------------------

    #[test]
    fn remap_config_fields_accessible() {
        let remap = RemapConfig {
            a_to: "b".into(),
            b_to: "a".into(),
            x_to: "y".into(),
            y_to: "x".into(),
        };
        assert_eq!(remap.a_to, "b");
        assert_eq!(remap.b_to, "a");
        assert_eq!(remap.x_to, "y");
        assert_eq!(remap.y_to, "x");
    }
}
