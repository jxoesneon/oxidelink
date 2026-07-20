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
