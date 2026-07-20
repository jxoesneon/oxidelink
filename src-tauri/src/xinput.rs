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
