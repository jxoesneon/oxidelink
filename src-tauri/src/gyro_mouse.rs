//! Gyro-to-mouse and gyro-to-stick mapping for OxideLink.
//!
//! Converts calibrated `ImuPhysical` gyro rates (degrees per second) into
//! desktop mouse deltas or virtual right-stick deflection. The pipeline is
//! stateful (smoothed gyro values + optional KBM output backend) and is
//! designed to be driven once per IMU frame.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::imu::ImuPhysical;
use crate::kbm::{InputBackend, KbmEmulator};
use crate::state::{AppCtx, GyroMapping, GyroMode, StickSide};

/// Gyro input processor.
///
/// Holds smoothed gyro values and a [`KbmEmulator`] handle so that, when the
/// configured mode is [`GyroMode::Mouse`], the resulting `(dx, dy)` deltas can
/// be sent to the OS as relative mouse motion.
pub struct GyroMouse {
    smooth_x: f32,
    smooth_y: f32,
    stick_x: f32,
    stick_y: f32,
    /// Fractional mouse deltas retained until they add up to a whole pixel.
    accum_x: f32,
    accum_y: f32,
    kbm: Arc<Mutex<KbmEmulator>>,
}

impl Default for GyroMouse {
    fn default() -> Self {
        Self::new()
    }
}

impl GyroMouse {
    /// Create a `GyroMouse` using the real Windows `SendInput` backend.
    pub fn new() -> Self {
        Self::with_backend(Arc::new(crate::kbm::WindowsBackend))
    }
    /// Create a `GyroMouse` with a custom input backend, used in unit tests.
    pub fn with_backend(backend: Arc<dyn InputBackend + Send + Sync>) -> Self {
        Self {
            smooth_x: 0.0,
            smooth_y: 0.0,
            stick_x: 0.0,
            stick_y: 0.0,
            accum_x: 0.0,
            accum_y: 0.0,
            kbm: Arc::new(Mutex::new(KbmEmulator::with_backend(backend))),
        }
    }

    /// Reset smoothing accumulators and any pending stick output.
    pub fn recenter(&mut self) {
        self.smooth_x = 0.0;
        self.smooth_y = 0.0;
        self.stick_x = 0.0;
        self.stick_y = 0.0;
        self.accum_x = 0.0;
        self.accum_y = 0.0;
    }

    /// Process one IMU frame and update smoothing / stick output state.
    ///
    /// - Pro Controller convention: `gyro_y` is yaw (left/right turn) and
    ///   `gyro_x` is pitch (up/down tilt). Mouse yaw → horizontal cursor
    ///   movement and pitch → vertical movement (inverted so tilting up moves
    ///   the cursor up).
    /// - Deadzone is applied in deg/s: any axis below the threshold is treated
    ///   as zero.
    /// - Exponential smoothing: `smooth = smoothing * old + (1 - smoothing) * new`.
    ///   `smoothing` is clamped to `[0.0, 0.99]` where higher values retain more
    ///   history.
    /// - Mouse deltas are `smooth * sensitivity * dt`, rounded to whole pixels.
    ///   `sensitivity` is per-axis and `dt` is the frame interval in seconds.
    ///
    /// Returns `(dx, dy)` in pixels when the mode is [`GyroMode::Mouse`].
    /// For [`GyroMode::Stick`] the returned tuple is `(0, 0)` and the stick
    /// output is read separately via [`GyroMouse::stick_output`].
    pub fn update(&mut self, imu: &ImuPhysical, dt: f32, config: &GyroMapping) -> (i32, i32) {
        // raw_x is yaw (left/right), raw_y is pitch (inverted so up is up).
        let raw_x = imu.gyro_y;
        let raw_y = -imu.gyro_x;

        // Deadzone: ignore small drift below the configured threshold (deg/s).
        let x = if raw_x.abs() < config.deadzone {
            0.0
        } else {
            raw_x
        };
        let y = if raw_y.abs() < config.deadzone {
            0.0
        } else {
            raw_y
        };

        // Exponential moving average. `smoothing` is the weight of the
        // previous smoothed value, clamped to avoid a frozen accumulator.
        let smoothing = config.smoothing.clamp(0.0, 0.99);
        self.smooth_x = smoothing * self.smooth_x + (1.0 - smoothing) * x;
        self.smooth_y = smoothing * self.smooth_y + (1.0 - smoothing) * y;

        match config.mode {
            GyroMode::Mouse => {
                self.accum_x += self.smooth_x * config.sensitivity[0] * dt;
                self.accum_y += self.smooth_y * config.sensitivity[1] * dt;
                let dx = self.accum_x.round() as i32;
                let dy = self.accum_y.round() as i32;
                self.accum_x -= dx as f32;
                self.accum_y -= dy as f32;
                (dx, dy)
            }
            GyroMode::Stick(side) => {
                let sx = (self.smooth_x * config.sensitivity[0]).clamp(-1.0, 1.0);
                let sy = (self.smooth_y * config.sensitivity[1]).clamp(-1.0, 1.0);
                // Side tag tells the integration layer which stick to drive.
                // Flip the Y axis for the left stick to keep natural ergonomics.
                let (sx, sy) = match side {
                    StickSide::Left => (-sx, -sy),
                    StickSide::Right => (sx, sy),
                };
                self.stick_x = sx;
                self.stick_y = sy;
                (0, 0)
            }
            // Off and FlickStick produce no mouse motion.
            _ => (0, 0),
        }
    }

    /// Return the last computed virtual stick output.
    ///
    /// This is populated when the configured [`GyroMode`] is
    /// [`GyroMode::Stick`]. Values are in `[-1.0, 1.0]`.
    pub fn stick_output(&self) -> (f32, f32) {
        (self.stick_x, self.stick_y)
    }

    /// Emit a relative mouse move through the backing KBM emulator.
    pub fn send_mouse_move(&self, dx: i32, dy: i32) {
        self.kbm.lock().send_mouse_move(dx, dy);
    }
}

/// Thin helper that coordinates the shared gyro mouse state, the live
/// configuration, and the frontend Tauri commands.
#[derive(Clone)]
pub struct GyroMouseManager {
    shared: Arc<crate::state::SharedState>,
}

impl GyroMouseManager {
    pub fn new(shared: Arc<crate::state::SharedState>) -> Self {
        Self { shared }
    }

    pub fn set_mode(&self, mode: GyroMode) {
        self.shared.config.write().mappings.gyro.mode = mode;
    }

    pub fn config(&self) -> GyroMapping {
        self.shared.config.read().mappings.gyro.clone()
    }

    pub fn set_config(&self, config: GyroMapping) {
        self.shared.config.write().mappings.gyro = config;
    }

    pub fn recenter(&self) {
        self.shared.gyro_mouse.lock().recenter();
    }
}

// -----------------------------------------------------------------------------
// Tauri commands (free functions; not yet wired into main.rs invoke_handler)
// -----------------------------------------------------------------------------

#[tauri::command]
pub fn set_gyro_mode(ctx: tauri::State<'_, AppCtx>, mode: GyroMode) -> GyroMapping {
    let mgr = GyroMouseManager::new(ctx.shared.clone());
    mgr.set_mode(mode);
    mgr.config()
}

#[tauri::command]
pub fn get_gyro_config(ctx: tauri::State<'_, AppCtx>) -> GyroMapping {
    GyroMouseManager::new(ctx.shared.clone()).config()
}

#[tauri::command]
pub fn set_gyro_config(ctx: tauri::State<'_, AppCtx>, config: GyroMapping) -> GyroMapping {
    let mgr = GyroMouseManager::new(ctx.shared.clone());
    mgr.set_config(config);
    mgr.config()
}

#[tauri::command]
pub fn gyro_recenter(ctx: tauri::State<'_, AppCtx>) -> bool {
    GyroMouseManager::new(ctx.shared.clone()).recenter();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imu::ImuPhysical;
    use crate::kbm::{InputEvent, MockBackend};
    use crate::state::{GyroMode, StickSide};

    /// Build a `GyroMouse` backed by a [`MockBackend`] and return the receiver
    /// for asserting on emitted events.
    fn gyro_with_mock() -> (GyroMouse, tokio::sync::mpsc::UnboundedReceiver<InputEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let gyro = GyroMouse::with_backend(Arc::new(MockBackend::new(tx)));
        (gyro, rx)
    }

    fn imu(gyro_x: f32, gyro_y: f32) -> ImuPhysical {
        ImuPhysical {
            gyro_x,
            gyro_y,
            ..ImuPhysical::default()
        }
    }

    // -------------------------------------------------------------------------
    // Defaults & state
    // -------------------------------------------------------------------------

    #[test]
    fn gyro_mouse_default_zero_state() {
        let (gyro, _rx) = gyro_with_mock();
        assert_eq!(gyro.stick_output(), (0.0, 0.0));
    }

    #[test]
    fn gyro_mapping_default_values() {
        let g = GyroMapping::default();
        assert_eq!(g.mode, GyroMode::Off);
        assert_eq!(g.sensitivity, [1.0, 1.0]);
        assert_eq!(g.smoothing, 0.0);
        assert_eq!(g.deadzone, 0.0);
    }

    #[test]
    fn recenter_clears_stick_and_accumulators() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Stick(StickSide::Right),
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // Drive a frame so stick output becomes non-zero.
        gyro.update(&imu(50.0, 50.0), 0.1, &cfg);
        assert_ne!(gyro.stick_output(), (0.0, 0.0));

        gyro.recenter();
        assert_eq!(gyro.stick_output(), (0.0, 0.0));
    }

    // -------------------------------------------------------------------------
    // update — Off / FlickStick
    // -------------------------------------------------------------------------

    #[test]
    fn update_off_mode_returns_zero_delta() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping::default(); // mode Off
        let (dx, dy) = gyro.update(&imu(100.0, 100.0), 0.1, &cfg);
        assert_eq!((dx, dy), (0, 0));
        assert_eq!(gyro.stick_output(), (0.0, 0.0));
    }

    #[test]
    fn update_flickstick_returns_zero_delta() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::FlickStick,
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        let (dx, dy) = gyro.update(&imu(100.0, 100.0), 0.1, &cfg);
        assert_eq!((dx, dy), (0, 0));
    }

    // -------------------------------------------------------------------------
    // update — Mouse mode
    // -------------------------------------------------------------------------

    #[test]
    fn update_mouse_mode_deadzone_zeros_small_input() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 5.0,
        };
        // gyro_y=3.0 < deadzone 5.0 -> raw_x zeroed -> no motion.
        let (dx, dy) = gyro.update(&imu(3.0, 3.0), 1.0, &cfg);
        assert_eq!((dx, dy), (0, 0));
    }

    #[test]
    fn update_mouse_mode_accumulates_and_rounds_to_pixels() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // raw_x = gyro_y = 10.0; smoothing=0 -> smooth_x=10. accum += 10*1*1 = 10.
        let (dx, dy) = gyro.update(&imu(0.0, 10.0), 1.0, &cfg);
        assert_eq!(dx, 10);
        assert_eq!(dy, 0);
    }

    #[test]
    fn update_mouse_mode_sensitivity_scales_output() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [2.0, 3.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // raw_x = gyro_y = 10 -> smooth 10 -> accum_x += 10*2*1 = 20.
        // raw_y = -gyro_x = -10 -> smooth -10 -> accum_y += -10*3*1 = -30.
        let (dx, dy) = gyro.update(&imu(10.0, 10.0), 1.0, &cfg);
        assert_eq!(dx, 20);
        assert_eq!(dy, -30);
    }

    #[test]
    fn update_mouse_mode_subpixel_accumulation_carries_remainder() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // gyro_y = 0.4 -> accum_x += 0.4 -> round(0.4)=0, remainder 0.4 carried.
        let (dx1, _) = gyro.update(&imu(0.0, 0.4), 1.0, &cfg);
        assert_eq!(dx1, 0);
        // Second frame: accum = 0.4 + 0.4 = 0.8 -> round(0.8)=1, remainder -0.2.
        let (dx2, _) = gyro.update(&imu(0.0, 0.4), 1.0, &cfg);
        assert_eq!(dx2, 1);
    }

    #[test]
    fn update_mouse_mode_smoothing_ema_blends_frames() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [1.0, 1.0],
            smoothing: 0.5,
            deadzone: 0.0,
        };
        // Frame 1: smooth_x = 0.5*0 + 0.5*10 = 5 -> accum += 5*1*1 = 5 -> dx=5.
        let (dx1, _) = gyro.update(&imu(0.0, 10.0), 1.0, &cfg);
        assert_eq!(dx1, 5);
        // Frame 2: smooth_x = 0.5*5 + 0.5*10 = 7.5 -> accum += 7.5 -> dx=8.
        let (dx2, _) = gyro.update(&imu(0.0, 10.0), 1.0, &cfg);
        assert_eq!(dx2, 8);
    }

    #[test]
    fn update_mouse_mode_smoothing_clamped_to_0_99() {
        let (mut gyro, _rx) = gyro_with_mock();
        // smoothing > 0.99 is clamped to 0.99 so the accumulator never freezes.
        let cfg = GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [1.0, 1.0],
            smoothing: 5.0,
            deadzone: 0.0,
        };
        let (dx, _) = gyro.update(&imu(0.0, 100.0), 1.0, &cfg);
        // smooth_x = 0.99*0 + 0.01*100 = 1.0 -> accum 1.0 -> dx=1.
        assert_eq!(dx, 1);
    }

    #[test]
    fn update_mouse_mode_pitch_inverted() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Mouse,
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // raw_y = -gyro_x. Positive pitch (gyro_x=10) -> negative dy (cursor up).
        let (_, dy) = gyro.update(&imu(10.0, 0.0), 1.0, &cfg);
        assert_eq!(dy, -10);
    }

    // -------------------------------------------------------------------------
    // update — Stick mode
    // -------------------------------------------------------------------------

    #[test]
    fn update_stick_mode_returns_zero_delta() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Stick(StickSide::Right),
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        let (dx, dy) = gyro.update(&imu(10.0, 10.0), 0.1, &cfg);
        assert_eq!((dx, dy), (0, 0));
    }

    #[test]
    fn update_stick_mode_clamps_to_unit_range() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Stick(StickSide::Right),
            sensitivity: [10.0, 10.0],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // smooth_x = 10, sx = 10*10 = 100 -> clamped to 1.0.
        gyro.update(&imu(0.0, 10.0), 0.1, &cfg);
        let (sx, sy) = gyro.stick_output();
        assert!((sx - 1.0).abs() < 1e-6, "sx should be clamped to 1.0: {sx}");
        assert!((sy - 0.0).abs() < 1e-6, "sy should be 0: {sy}");
    }

    #[test]
    fn update_stick_mode_right_side_does_not_flip() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Stick(StickSide::Right),
            sensitivity: [0.1, 0.1],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // raw_x = gyro_y = 10 -> smooth 10 -> sx = 10*0.1 = 1.0 (not flipped).
        gyro.update(&imu(0.0, 10.0), 0.1, &cfg);
        let (sx, _sy) = gyro.stick_output();
        assert!((sx - 1.0).abs() < 1e-6, "right stick should not flip: {sx}");
    }

    #[test]
    fn update_stick_mode_left_side_flips_both_axes() {
        let (mut gyro, _rx) = gyro_with_mock();
        let cfg = GyroMapping {
            mode: GyroMode::Stick(StickSide::Left),
            sensitivity: [0.1, 0.1],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        // raw_x = gyro_y = 10 -> smooth 10 -> sx = 1.0, flipped to -1.0.
        // raw_y = -gyro_x = -10 -> smooth -10 -> sy = -1.0, flipped to 1.0.
        gyro.update(&imu(10.0, 10.0), 0.1, &cfg);
        let (sx, sy) = gyro.stick_output();
        assert!(
            (sx - (-1.0)).abs() < 1e-6,
            "left stick x should be flipped: {sx}"
        );
        assert!(
            (sy - 1.0).abs() < 1e-6,
            "left stick y should be flipped: {sy}"
        );
    }

    #[test]
    fn stick_output_zero_until_stick_mode_frame() {
        let (mut gyro, _rx) = gyro_with_mock();
        // Off mode must not populate stick output.
        let cfg_off = GyroMapping::default();
        gyro.update(&imu(50.0, 50.0), 0.1, &cfg_off);
        assert_eq!(gyro.stick_output(), (0.0, 0.0));

        // Switching to stick mode populates it.
        let cfg_stick = GyroMapping {
            mode: GyroMode::Stick(StickSide::Right),
            sensitivity: [0.1, 0.1],
            smoothing: 0.0,
            deadzone: 0.0,
        };
        gyro.update(&imu(0.0, 50.0), 0.1, &cfg_stick);
        assert_ne!(gyro.stick_output(), (0.0, 0.0));
    }

    // -------------------------------------------------------------------------
    // send_mouse_move backend integration
    // -------------------------------------------------------------------------

    #[test]
    fn send_mouse_move_emits_event_via_backend() {
        let (gyro, mut rx) = gyro_with_mock();
        gyro.send_mouse_move(7, -4);
        match rx.try_recv() {
            Ok(InputEvent::MouseMove { dx, dy }) => {
                assert_eq!(dx, 7);
                assert_eq!(dy, -4);
            }
            other => panic!("expected MouseMove event, got {other:?}"),
        }
    }

    #[test]
    fn send_mouse_move_no_panic_when_channel_dropped() {
        let (gyro, rx) = gyro_with_mock();
        drop(rx);
        // Dropping the receiver must not panic the sender.
        gyro.send_mouse_move(1, 1);
    }
}
