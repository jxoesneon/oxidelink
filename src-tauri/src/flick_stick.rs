//! Flick Stick camera-stick algorithm (Jibb Smart).
//!
//! Maps the right stick angle to an absolute camera yaw. A quick flick to the
//! edge snaps the camera to that direction; holding the edge rotates the camera
//! continuously at a tunable rate.

use serde::{Deserialize, Serialize};
use std::time::Instant;

// Parent `state` module provides the shared context types.
use super::{AppCtx, CONTROLLER_SLOTS};

/// Right-stick processing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightStickMode {
    /// Traditional right-stick camera / stick emulation.
    #[default]
    Camera,
    /// Flick Stick absolute-yaw mode.
    FlickStick,
}

/// Configuration for the Flick Stick camera algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FlickStickConfig {
    /// Whether Flick Stick processing is active.
    pub enabled: bool,
    /// Stick magnitude (0.0-1.0) that counts as a full-deflection flick.
    pub flick_threshold: f32,
    /// Continuous rotation rate while the stick is held at the edge.
    pub rotate_rate_deg_per_sec: f32,
    /// Stick magnitude below which input is ignored.
    pub stick_deadzone: f32,
    /// Minimum time between two flick events.
    pub flick_cooldown_ms: u64,
    /// Output smoothing factor (0.0 = no smoothing, 1.0 = maximum).
    pub output_smoothing: f32,
}

impl Default for FlickStickConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            flick_threshold: 0.90,
            rotate_rate_deg_per_sec: 360.0,
            stick_deadzone: 0.15,
            flick_cooldown_ms: 150,
            output_smoothing: 0.0,
        }
    }
}

/// Right-stick pipeline configuration, including an optional Flick Stick mode.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RightStickConfig {
    /// Selected right-stick processing mode.
    #[serde(default)]
    pub mode: RightStickMode,
    /// Flick Stick tuning parameters.
    #[serde(default)]
    pub flick_stick: FlickStickConfig,
}

/// Runtime state for Flick Stick.
#[derive(Debug, Clone, Default)]
pub struct FlickStick {
    /// Current absolute camera yaw, in degrees.
    pub current_yaw: f32,
    /// Timestamp of the last flick event.
    pub last_flick_time: Option<Instant>,
    /// Whether the stick is currently held at edge-deflection.
    pub flick_active: bool,
    /// Cached config used by `process()`.
    pub config: FlickStickConfig,
    /// Previous smoothed output, used by the output low-pass filter.
    pub last_output: f32,
}

impl FlickStick {
    /// Create a new Flick Stick processor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a processor seeded with the given config.
    pub fn with_config(config: FlickStickConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Replace the cached config.
    pub fn set_config(&mut self, config: FlickStickConfig) {
        self.config = config;
    }

    /// Reset the camera yaw and flick state.
    pub fn reset(&mut self) {
        self.current_yaw = 0.0;
        self.last_flick_time = None;
        self.flick_active = false;
        self.last_output = 0.0;
    }

    /// Process a stick deflection and return `(camera_delta_yaw_deg, flicked)`.
    pub fn process(&mut self, stick_x: f32, stick_y: f32, dt_secs: f32) -> (f32, bool) {
        let cfg = self.config;
        self.process_with_config(stick_x, stick_y, dt_secs, &cfg)
    }

    /// Process a stick deflection using an externally supplied config.
    pub fn process_with_config(
        &mut self,
        stick_x: f32,
        stick_y: f32,
        dt_secs: f32,
        config: &FlickStickConfig,
    ) -> (f32, bool) {
        if !config.enabled {
            self.flick_active = false;
            self.last_output = 0.0;
            return (0.0, false);
        }

        let magnitude = (stick_x * stick_x + stick_y * stick_y).sqrt();
        if magnitude < config.stick_deadzone || magnitude == 0.0 {
            self.flick_active = false;
            self.last_output = 0.0;
            return (0.0, false);
        }

        // Angle measured from the positive Y axis (stick up = 0 deg).
        // +90 deg = stick right, -90 deg = stick left, 180 deg = stick down.
        let angle = f32::atan2(stick_x, stick_y);
        let angle_deg = angle.to_degrees();

        // Flick: transition from near-center to full deflection while cooldown elapsed.
        if magnitude > config.flick_threshold && !self.flick_active {
            let now = Instant::now();
            let cooldown_elapsed = self
                .last_flick_time
                .map(|t| now.duration_since(t).as_millis() as u64 >= config.flick_cooldown_ms)
                .unwrap_or(true);
            if cooldown_elapsed {
                let delta = normalize_degrees(angle_deg - self.current_yaw);
                self.current_yaw = angle_deg;
                self.last_flick_time = Some(now);
                self.flick_active = true;
                return (self.smooth_output(delta, config.output_smoothing), true);
            }
        }

        // Continuous rotation while the stick is held at the edge.
        if magnitude > config.flick_threshold && self.flick_active {
            // sin(angle) with our convention equals the normalised horizontal
            // component (x / magnitude), so left/right rotate left/right.
            let rotate = config.rotate_rate_deg_per_sec * angle.sin() * dt_secs;
            self.current_yaw += rotate;
            return (self.smooth_output(rotate, config.output_smoothing), false);
        }

        self.flick_active = false;
        (0.0, false)
    }

    fn smooth_output(&mut self, raw: f32, smoothing: f32) -> f32 {
        let s = smoothing.clamp(0.0, 1.0);
        let out = raw * (1.0 - s) + self.last_output * s;
        self.last_output = out;
        out
    }
}

fn normalize_degrees(delta: f32) -> f32 {
    (delta + 180.0).rem_euclid(360.0) - 180.0
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Read the current Flick Stick configuration.
#[tauri::command]
pub fn get_flick_stick_config(ctx: tauri::State<'_, AppCtx>) -> FlickStickConfig {
    ctx.shared.config.read().right_stick.flick_stick
}

/// Update the Flick Stick configuration and propagate it to runtime state.
#[tauri::command]
pub fn set_flick_stick_config(
    ctx: tauri::State<'_, AppCtx>,
    config: FlickStickConfig,
) -> FlickStickConfig {
    ctx.shared.config.write().right_stick.flick_stick = config;
    for slot in &ctx.shared.flick_stick {
        slot.lock().set_config(config);
    }
    config
}

/// Reset the Flick Stick yaw for a slot (or the currently selected slot).
#[tauri::command]
pub fn reset_flick_stick_yaw(ctx: tauri::State<'_, AppCtx>, slot: Option<u8>) -> bool {
    use std::sync::atomic::Ordering;
    let slot = slot.unwrap_or_else(|| ctx.shared.selected_slot.load(Ordering::SeqCst));
    if slot as usize >= CONTROLLER_SLOTS {
        return false;
    }
    let idx = slot as usize;
    ctx.shared.flick_stick[idx].lock().reset();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    fn test_cfg() -> FlickStickConfig {
        FlickStickConfig {
            enabled: true,
            flick_threshold: 0.9,
            rotate_rate_deg_per_sec: 360.0,
            stick_deadzone: 0.1,
            flick_cooldown_ms: 0,
            output_smoothing: 0.0,
        }
    }

    #[test]
    fn flick_stick_stub_compiles() {
        let mut fs = FlickStick::new();
        // Default config has enabled=false, so the stub-like result is still valid.
        let (x, flick) = fs.process(1.0, 0.0, 0.016);
        assert_eq!(x, 0.0);
        assert!(!flick);
    }

    #[test]
    fn flick_detection_right() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(flick, "should report a flick");
        assert!(
            (delta - 90.0).abs() < 0.1,
            "right flick delta was {}",
            delta
        );
        assert!((fs.current_yaw - 90.0).abs() < 0.1);
    }

    #[test]
    fn flick_detection_up() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(0.0, 1.0, 0.016);
        assert!(flick);
        assert!(delta.abs() < 0.1, "up flick delta was {}", delta);
    }

    #[test]
    fn flick_detection_left() {
        let mut fs = FlickStick::with_config(test_cfg());
        fs.process(1.0, 0.0, 0.016);
        // Bring stick to centre and flick left.
        fs.process(0.0, 0.0, 0.016);
        let (delta, flick) = fs.process(-1.0, 0.0, 0.016);
        assert!(flick);
        assert!(
            (delta + 180.0).abs() < 0.1 || (delta - 180.0).abs() < 0.1,
            "left flick delta was {}",
            delta
        );
    }

    #[test]
    fn continuous_rotation_right() {
        let mut fs = FlickStick::with_config(test_cfg());
        fs.process(1.0, 0.0, 0.016);
        let (delta, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(!flick, "second frame should not be a flick");
        let expected = 360.0 * 0.016; // 5.76
        assert!(
            (delta - expected).abs() < 0.1,
            "rotation delta was {}, expected {}",
            delta,
            expected
        );
    }

    #[test]
    fn continuous_rotation_left() {
        let mut fs = FlickStick::with_config(test_cfg());
        fs.process(1.0, 0.0, 0.016);
        let (delta, flick) = fs.process(-1.0, 0.0, 0.016);
        assert!(!flick, "second frame should not be a flick");
        // Stick is now left; rotate rate should be negative.
        let expected = -360.0 * 0.016;
        assert!(
            (delta - expected).abs() < 0.1,
            "left rotation delta was {}, expected {}",
            delta,
            expected
        );
    }

    #[test]
    fn deadzone_returns_zero() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(0.05, 0.0, 0.016);
        assert!(!flick);
        assert_eq!(delta, 0.0);
        assert!(!fs.flick_active);
    }

    #[test]
    fn cooldown_blocks_repeated_flick() {
        let cfg = FlickStickConfig {
            flick_cooldown_ms: 200,
            ..test_cfg()
        };
        let mut fs = FlickStick::with_config(cfg);
        // Flick right.
        let (_, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(flick);
        // Release, then attempt an immediate re-flick left.
        fs.process(0.0, 0.0, 0.016);
        let (delta, flick) = fs.process(-1.0, 0.0, 0.016);
        assert!(!flick, "re-flick inside cooldown should be blocked");
        assert_eq!(delta, 0.0);

        // After the cooldown expires, the same flick should register.
        thread::sleep(Duration::from_millis(250));
        let (delta, flick) = fs.process(-1.0, 0.0, 0.016);
        assert!(flick, "flick should register after cooldown");
        assert!(delta.abs() > 90.0, "turn-around delta was {}", delta);
    }

    // --- Config defaults ----------------------------------------------------

    #[test]
    fn flick_stick_config_defaults() {
        let cfg = FlickStickConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.flick_threshold - 0.90).abs() < f32::EPSILON);
        assert!((cfg.rotate_rate_deg_per_sec - 360.0).abs() < f32::EPSILON);
        assert!((cfg.stick_deadzone - 0.15).abs() < f32::EPSILON);
        assert_eq!(cfg.flick_cooldown_ms, 150);
        assert!((cfg.output_smoothing - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn right_stick_mode_default_is_camera() {
        assert_eq!(RightStickMode::default(), RightStickMode::Camera);
    }

    #[test]
    fn right_stick_config_defaults() {
        let cfg = RightStickConfig::default();
        assert_eq!(cfg.mode, RightStickMode::Camera);
        assert_eq!(cfg.flick_stick, FlickStickConfig::default());
    }

    // --- serde round-trips --------------------------------------------------

    #[test]
    fn flick_stick_config_serde_round_trip() {
        let cfg = FlickStickConfig {
            enabled: true,
            flick_threshold: 0.75,
            rotate_rate_deg_per_sec: 540.0,
            stick_deadzone: 0.2,
            flick_cooldown_ms: 300,
            output_smoothing: 0.4,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: FlickStickConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn right_stick_mode_serde_snake_case() {
        let json = serde_json::to_string(&RightStickMode::FlickStick).unwrap();
        assert_eq!(json, "\"flick_stick\"");
        let back: RightStickMode = serde_json::from_str("\"flick_stick\"").unwrap();
        assert_eq!(back, RightStickMode::FlickStick);
    }

    #[test]
    fn right_stick_config_serde_round_trip() {
        let cfg = RightStickConfig {
            mode: RightStickMode::FlickStick,
            flick_stick: FlickStickConfig {
                enabled: true,
                flick_threshold: 0.8,
                rotate_rate_deg_per_sec: 720.0,
                stick_deadzone: 0.05,
                flick_cooldown_ms: 50,
                output_smoothing: 0.25,
            },
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RightStickConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    // --- normalize_degrees helper ------------------------------------------

    #[test]
    fn normalize_degrees_wraps_to_signed_range() {
        assert!((normalize_degrees(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((normalize_degrees(90.0) - 90.0).abs() < f32::EPSILON);
        assert!((normalize_degrees(-90.0) + 90.0).abs() < f32::EPSILON);
        // 180 and -180 both map to -180 (the negative boundary).
        assert!((normalize_degrees(180.0) + 180.0).abs() < f32::EPSILON);
        assert!((normalize_degrees(-180.0) + 180.0).abs() < f32::EPSILON);
        // 270 -> -90.
        assert!((normalize_degrees(270.0) + 90.0).abs() < f32::EPSILON);
        // 360 -> 0.
        assert!((normalize_degrees(360.0) - 0.0).abs() < f32::EPSILON);
        // 540 -> 180 -> -180.
        assert!((normalize_degrees(540.0) + 180.0).abs() < f32::EPSILON);
        // -270 -> 90.
        assert!((normalize_degrees(-270.0) - 90.0).abs() < f32::EPSILON);
    }

    // --- Polar-to-cartesian / angle conversions ----------------------------

    #[test]
    fn flick_down_uses_180_degrees() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(0.0, -1.0, 0.016);
        assert!(flick);
        // Down is 180 deg from +Y; delta should be ±180.
        assert!(
            (delta - 180.0).abs() < 0.1 || (delta + 180.0).abs() < 0.1,
            "down flick delta was {}",
            delta
        );
    }

    #[test]
    fn flick_up_is_zero_delta() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(0.0, 1.0, 0.016);
        assert!(flick);
        assert!(delta.abs() < 0.1, "up flick delta was {}", delta);
        assert!(fs.current_yaw.abs() < 0.1);
    }

    #[test]
    fn flick_diagonal_right_up() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(0.7071, 0.7071, 0.016);
        assert!(flick);
        // atan2(0.7071, 0.7071) = 45 deg.
        assert!((delta - 45.0).abs() < 0.5, "diagonal delta was {}", delta);
        assert!((fs.current_yaw - 45.0).abs() < 0.5);
    }

    #[test]
    fn flick_diagonal_left_down() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(-0.7071, -0.7071, 0.016);
        assert!(flick);
        // atan2(-0.7071, -0.7071) = -135 deg.
        assert!((delta + 135.0).abs() < 0.5, "diagonal delta was {}", delta);
        assert!((fs.current_yaw + 135.0).abs() < 0.5);
    }

    // --- Disabled / deadzone edge cases ------------------------------------

    #[test]
    fn disabled_config_returns_zero() {
        let mut fs = FlickStick::with_config(FlickStickConfig {
            enabled: false,
            ..test_cfg()
        });
        let (delta, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(!flick);
        assert_eq!(delta, 0.0);
        assert!(!fs.flick_active);
    }

    #[test]
    fn magnitude_below_deadzone_returns_zero() {
        let mut fs = FlickStick::with_config(test_cfg());
        // 0.05 magnitude < 0.1 deadzone.
        let (delta, flick) = fs.process(0.05, 0.0, 0.016);
        assert!(!flick);
        assert_eq!(delta, 0.0);
        assert!(!fs.flick_active);
    }

    #[test]
    fn magnitude_just_above_deadzone_but_below_threshold_no_flick() {
        let mut fs = FlickStick::with_config(test_cfg());
        // 0.5 magnitude: above deadzone (0.1), below flick threshold (0.9).
        let (delta, flick) = fs.process(0.5, 0.0, 0.016);
        assert!(!flick, "sub-threshold input should not flick");
        assert_eq!(delta, 0.0);
        assert!(!fs.flick_active);
    }

    #[test]
    fn zero_magnitude_returns_zero() {
        let mut fs = FlickStick::with_config(test_cfg());
        let (delta, flick) = fs.process(0.0, 0.0, 0.016);
        assert!(!flick);
        assert_eq!(delta, 0.0);
    }

    // --- reset / set_config ------------------------------------------------

    #[test]
    fn reset_clears_state() {
        let mut fs = FlickStick::with_config(test_cfg());
        fs.process(1.0, 0.0, 0.016);
        assert!(fs.flick_active);
        assert!(fs.last_flick_time.is_some());
        assert!((fs.current_yaw - 90.0).abs() < 0.1);

        fs.reset();
        assert!((fs.current_yaw - 0.0).abs() < f32::EPSILON);
        assert!(fs.last_flick_time.is_none());
        assert!(!fs.flick_active);
        assert!((fs.last_output - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn set_config_replaces_cached_config() {
        let mut fs = FlickStick::new();
        // Initially disabled -> no output.
        let (delta, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(!flick);
        assert_eq!(delta, 0.0);

        // Swap in an enabled config.
        fs.set_config(test_cfg());
        let (delta, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(flick);
        assert!((delta - 90.0).abs() < 0.1);
    }

    #[test]
    fn with_config_seeds_config() {
        let cfg = test_cfg();
        let fs = FlickStick::with_config(cfg);
        assert_eq!(fs.config, cfg);
        // Other fields default.
        assert!((fs.current_yaw - 0.0).abs() < f32::EPSILON);
        assert!(fs.last_flick_time.is_none());
        assert!(!fs.flick_active);
    }

    // --- Output smoothing ---------------------------------------------------

    #[test]
    fn smoothing_low_pass_filters_output() {
        let cfg = FlickStickConfig {
            output_smoothing: 0.5,
            ..test_cfg()
        };
        let mut fs = FlickStick::with_config(cfg);
        // First flick: smoothed = raw * 0.5 + 0 * 0.5 = 45 deg (raw 90).
        let (delta, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(flick);
        assert!((delta - 45.0).abs() < 0.1, "smoothed delta was {}", delta);
        assert!((fs.last_output - 45.0).abs() < 0.1);
    }

    #[test]
    fn smoothing_clamped_to_one_uses_last_output() {
        let cfg = FlickStickConfig {
            output_smoothing: 1.0,
            ..test_cfg()
        };
        let mut fs = FlickStick::with_config(cfg);
        // First flick: smoothed = raw * 0 + 0 * 1 = 0.
        let (delta, flick) = fs.process(1.0, 0.0, 0.016);
        assert!(flick);
        assert!((delta - 0.0).abs() < f32::EPSILON);
    }

    // --- Continuous rotation details ---------------------------------------

    #[test]
    fn continuous_rotation_accumulates_yaw() {
        let mut fs = FlickStick::with_config(test_cfg());
        // Initial flick right.
        fs.process(1.0, 0.0, 0.016);
        let start_yaw = fs.current_yaw;
        // Two more frames of held-right rotation.
        fs.process(1.0, 0.0, 0.016);
        fs.process(1.0, 0.0, 0.016);
        let expected = start_yaw + 360.0 * 0.016 * 2.0;
        assert!(
            (fs.current_yaw - expected).abs() < 0.1,
            "yaw was {}, expected {}",
            fs.current_yaw,
            expected
        );
    }

    #[test]
    fn sub_threshold_held_does_not_rotate() {
        let mut fs = FlickStick::with_config(test_cfg());
        // Initial flick to seed state.
        fs.process(1.0, 0.0, 0.016);
        // Drop below threshold but above deadzone: no rotation, no flick.
        let (delta, flick) = fs.process(0.5, 0.0, 0.016);
        assert!(!flick);
        assert_eq!(delta, 0.0);
        assert!(!fs.flick_active);
    }
}
