//! Advanced stick calibration pipeline for the Nintendo Switch Pro Controller.
//!
//! This module implements a complete, multi-stage stick calibration pipeline:
//!
//! 1. [`AdaptiveDeadzone`] — tracks the noise floor during rest periods and
//!    dynamically adjusts the radial deadzone, with anti-deadzone remapping so
//!    small intentional movements are not crushed.
//! 2. [`CenterAutoCal`] — exponential-moving-average based auto-centering that
//!    learns the true electrical center while the stick is resting, then locks
//!    until the user requests a recalibration.
//! 3. [`DriftDetector`] — statistical drift detection using the median magnitude
//!    and inter-percentile spread of recent samples.
//! 4. [`GateCalibration`] — maps the Pro Controller's octagonal physical gate to
//!    a unit circle, either from a measured 32-point sweep or from a pure
//!    mathematical octagon model.
//! 5. [`ResponseCurve`] — selectable response shaping (linear, exponential,
//!    S-curve, cubic Bézier) applied to the stick magnitude while preserving
//!    direction.
//! 6. [`StickCalibrationPipeline`] — chains all of the above together into a
//!    single `process(x, y)` call returning the calibrated vector plus a
//!    [`DriftStatus`].
//!
//! All public types implement `Serialize`/`Deserialize` so the frontend can
//! inspect live calibration state via Tauri commands.

use serde::{Deserialize, Serialize};

// ===========================================================================
//  1. Adaptive Deadzone
// ===========================================================================

/// Adaptive radial deadzone that tracks the stick's noise floor during rest
/// periods and dynamically resizes itself.
///
/// The deadzone is `noise_floor * safety_margin`, clamped to
/// `[min_deadzone, max_deadzone]`. When a vector falls inside the deadzone it
/// is zeroed; otherwise it is remapped from `[deadzone, 1.0]` → `[0, 1.0]`
/// preserving direction so there is no "dead zone" felt by the user
/// (anti-deadzone normalization).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveDeadzone {
    /// Circular buffer of rest-period magnitudes.
    pub noise_samples: Vec<f32>,
    /// Maximum number of samples retained.
    pub buffer_size: usize,
    /// Median of the current noise-sample buffer.
    pub current_noise_floor: f32,
    /// Multiplier applied to the noise floor (default 1.5).
    pub safety_margin: f32,
    /// Minimum deadzone value (default 0.01).
    pub min_deadzone: f32,
    /// Maximum deadzone value (default 0.15).
    pub max_deadzone: f32,
    /// Cached adaptive deadzone value.
    pub current_deadzone: f32,
}

impl Default for AdaptiveDeadzone {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveDeadzone {
    /// Create a new adaptive deadzone with default parameters.
    pub fn new() -> Self {
        Self {
            noise_samples: Vec::with_capacity(256),
            buffer_size: 256,
            current_noise_floor: 0.0,
            safety_margin: 1.5,
            min_deadzone: 0.01,
            max_deadzone: 0.15,
            current_deadzone: 0.01,
        }
    }

    /// Record a rest-period magnitude sample and recompute the noise floor.
    ///
    /// Should be called every frame the stick is detected as resting.
    pub fn update_noise_floor(&mut self, magnitude: f32) {
        if self.noise_samples.len() >= self.buffer_size {
            // Circular buffer: drop the oldest sample.
            self.noise_samples.remove(0);
        }
        self.no_samples_push(magnitude);
        self.current_noise_floor = self.median(&self.noise_samples);
        self.current_deadzone = self.get_deadzone();
    }

    /// Current adaptive deadzone value.
    pub fn get_deadzone(&self) -> f32 {
        let base = self.current_noise_floor * self.safety_margin;
        base.clamp(self.min_deadzone, self.max_deadzone)
    }

    /// Apply the radial deadzone with anti-deadzone normalization.
    ///
    /// Returns `(0.0, 0.0)` when `magnitude < deadzone`, otherwise remaps
    /// `[deadzone, 1.0]` → `[0, 1.0]` preserving direction.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let magnitude = (x * x + y * y).sqrt();
        let deadzone = self.get_deadzone();
        if magnitude < deadzone || magnitude == 0.0 {
            return (0.0, 0.0);
        }
        // Anti-deadzone: remap [deadzone, 1.0] → [0, 1.0].
        let scaled = (magnitude - deadzone) / (1.0 - deadzone);
        let scaled = scaled.min(1.0);
        let ratio = scaled / magnitude;
        (x * ratio, y * ratio)
    }

    /// Reset the noise buffer (e.g. when switching controllers).
    pub fn reset(&mut self) {
        self.noise_samples.clear();
        self.current_noise_floor = 0.0;
        self.current_deadzone = self.min_deadzone;
    }

    // -- helpers -----------------------------------------------------------

    fn no_samples_push(&mut self, magnitude: f32) {
        self.noise_samples.push(magnitude);
    }

    fn median(&self, samples: &[f32]) -> f32 {
        let mut sorted: Vec<f32> = samples.iter().copied().filter(|v| v.is_finite()).collect();
        if sorted.is_empty() {
            return 0.0;
        }
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len().is_multiple_of(2) {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }

    /// Percentile (0..=100) of the current noise buffer.
    #[allow(dead_code)] // statistical utility, kept for future drift analysis
    fn percentile(&self, p: f32) -> f32 {
        if self.noise_samples.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = self
            .noise_samples
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        if sorted.is_empty() {
            return 0.0;
        }
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((p / 100.0) * (sorted.len() - 1) as f32).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
}

// ===========================================================================
//  2. Center Auto-Calibration (EMA-based)
// ===========================================================================

/// Exponential-moving-average based auto-centering.
///
/// While the stick magnitude stays below `movement_threshold` and the estimate
/// is not yet locked, the center is nudged toward the current reading using
/// `alpha`. After `lock_threshold` consecutive resting frames the center is
/// considered stable and updates stop until [`unlock`](Self::unlock) is called.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CenterAutoCal {
    /// Estimated center X.
    pub center_x: f32,
    /// Estimated center Y.
    pub center_y: f32,
    /// EMA smoothing factor (default 0.1).
    pub alpha: f32,
    /// Consecutive resting frames seen so far.
    pub lock_counter: u32,
    /// Frames of rest required before locking (default 100 ≈ 1.7s @ 60Hz).
    pub lock_threshold: u32,
    /// Whether the center estimate is frozen.
    pub locked: bool,
    /// Magnitude below which the stick is considered "resting" (default 0.03).
    pub movement_threshold: f32,
}

impl CenterAutoCal {
    /// Create a new auto-centerer seeded with an initial center estimate.
    pub fn new(initial_x: f32, initial_y: f32) -> Self {
        Self {
            center_x: initial_x,
            center_y: initial_y,
            alpha: 0.1,
            lock_counter: 0,
            lock_threshold: 100,
            locked: false,
            movement_threshold: 0.03,
        }
    }

    /// Update the center estimate. Only updates while resting and unlocked.
    pub fn update(&mut self, x: f32, y: f32) {
        let magnitude = (x * x + y * y).sqrt();
        if magnitude < self.movement_threshold && !self.locked {
            self.center_x = self.alpha * x + (1.0 - self.alpha) * self.center_x;
            self.center_y = self.alpha * y + (1.0 - self.alpha) * self.center_y;
            self.lock_counter += 1;
            if self.lock_counter >= self.lock_threshold {
                self.locked = true;
            }
        }
    }

    /// Force unlock and reset the lock counter (e.g. user-requested recal).
    pub fn unlock(&mut self) {
        self.locked = false;
        self.lock_counter = 0;
    }

    /// Current center estimate.
    pub fn get_center(&self) -> (f32, f32) {
        (self.center_x, self.center_y)
    }

    /// Apply center offset correction.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (x - self.center_x, y - self.center_y)
    }
}

// ===========================================================================
//  3. Drift Detection
// ===========================================================================

/// Result of a drift-detection analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DriftStatus {
    /// Median magnitude below `0.10` — no drift detected.
    Pass,
    /// Median magnitude in `[0.10, 0.25)` — early drift warning.
    Drifting,
    /// Median magnitude `>= 0.25` — drift failure.
    Fail,
    /// Spread too high — the stick was being moved during the sample window.
    #[default]
    Unknown,
}

/// Statistical drift detector using a rolling buffer of magnitudes.
///
/// The median magnitude classifies the stick as `Pass` / `Drifting` / `Fail`.
/// If the inter-percentile spread is too large the stick was likely being
/// moved and the status is `Unknown` instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftDetector {
    /// Rolling magnitude history.
    pub magnitude_history: Vec<f32>,
    /// Maximum samples retained (default 500).
    pub buffer_size: usize,
    /// Median below this → `Pass` (default 0.10).
    pub pass_threshold: f32,
    /// Median at/above this → `Fail` (default 0.25).
    pub fail_threshold: f32,
    /// Spread at/above this → `Unknown` (default 0.02).
    pub spread_limit: f32,
}

impl Default for DriftDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl DriftDetector {
    /// Create a new drift detector with default thresholds.
    pub fn new() -> Self {
        Self {
            magnitude_history: Vec::with_capacity(500),
            buffer_size: 500,
            pass_threshold: 0.10,
            fail_threshold: 0.25,
            spread_limit: 0.02,
        }
    }

    /// Record a magnitude sample (call every frame).
    pub fn record(&mut self, magnitude: f32) {
        if self.magnitude_history.len() >= self.buffer_size {
            self.magnitude_history.remove(0);
        }
        self.magnitude_history.push(magnitude);
    }

    /// Current drift status.
    pub fn get_status(&self) -> DriftStatus {
        if self.magnitude_history.is_empty() {
            return DriftStatus::Unknown;
        }
        let median = self.median(&self.magnitude_history);
        let spread = self.percentile(90.0) - self.percentile(10.0);
        if spread >= self.spread_limit {
            return DriftStatus::Unknown;
        }
        if median < self.pass_threshold {
            DriftStatus::Pass
        } else if median < self.fail_threshold {
            DriftStatus::Drifting
        } else {
            DriftStatus::Fail
        }
    }

    /// Reset the history buffer.
    pub fn reset(&mut self) {
        self.magnitude_history.clear();
    }

    // -- helpers -----------------------------------------------------------

    fn median(&self, samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len().is_multiple_of(2) {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }

    fn percentile(&self, p: f32) -> f32 {
        if self.magnitude_history.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = self.magnitude_history.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((p / 100.0) * (sorted.len() - 1) as f32).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
}

// ===========================================================================
//  4. Gate Calibration (Octagon → Circle)
// ===========================================================================

/// Octagonal-gate to unit-circle normalization.
///
/// The Pro Controller's physical gate is octagonal, so diagonal deflections
/// reach the edge at a smaller electrical radius than cardinal ones. This
/// remaps every angle to a unit circle using either a measured 32-point sweep
/// or a pure mathematical octagon model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCalibration {
    /// Whether gate normalization is active.
    pub enabled: bool,
    /// 32-point angular boundary map (normalized radius at each angle).
    pub radii: [f32; 32],
    /// Whether `radii` has been populated from a physical sweep.
    pub calibrated: bool,
}

impl Default for GateCalibration {
    fn default() -> Self {
        Self::new()
    }
}

impl GateCalibration {
    /// Create a new gate calibrator, disabled by default.
    pub fn new() -> Self {
        Self {
            enabled: false,
            radii: [1.0; 32],
            calibrated: false,
        }
    }

    /// Build the 32-point map from physical gate-sweep samples.
    ///
    /// Each sample is binned by angle into one of 32 segments; the maximum
    /// radius observed in each segment becomes that segment's boundary.
    pub fn calibrate(&mut self, samples: &[(f32, f32)]) {
        if samples.is_empty() {
            return;
        }
        let mut max_per_segment: [f32; 32] = [0.0; 32];
        let segment_size = 360.0 / 32.0;
        for &(x, y) in samples {
            let magnitude = (x * x + y * y).sqrt();
            if magnitude == 0.0 {
                continue;
            }
            let angle_deg = y.atan2(x).to_degrees().rem_euclid(360.0);
            let segment = (angle_deg / segment_size) as usize;
            let segment = segment.min(31);
            if magnitude > max_per_segment[segment] {
                max_per_segment[segment] = magnitude;
            }
        }
        // Fill any empty segments by interpolating from neighbours so the
        // map is always continuous.
        for i in 0..32 {
            if max_per_segment[i] == 0.0 {
                let prev = max_per_segment[(i + 31) % 32];
                let next = max_per_segment[(i + 1) % 32];
                max_per_segment[i] = if prev == 0.0 && next == 0.0 {
                    1.0
                } else if prev == 0.0 {
                    next
                } else if next == 0.0 {
                    prev
                } else {
                    (prev + next) / 2.0
                };
            }
        }
        self.radii = max_per_segment;
        self.calibrated = true;
        self.enabled = true;
    }

    /// Apply octagon-to-circle normalization.
    ///
    /// If not calibrated, uses the mathematical octagon model.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        if !self.enabled {
            return (x, y);
        }
        let magnitude = (x * x + y * y).sqrt();
        if magnitude == 0.0 {
            return (0.0, 0.0);
        }
        let angle = y.atan2(x);
        let max_radius = if self.calibrated {
            self.get_radius_at_angle(angle)
        } else {
            // Mathematical octagon: max(|cos|,|sin|) + min(|cos|,|sin|)*0.4142 = 1
            let cos_a = angle.cos();
            let sin_a = angle.sin();
            1.0 / (cos_a.abs().max(sin_a.abs()) + cos_a.abs().min(sin_a.abs()) * 0.4142)
        };
        let normalized = (magnitude / max_radius).min(1.0);
        (normalized * angle.cos(), normalized * angle.sin())
    }

    /// Linearly interpolate the 32-point radius map at the given angle.
    fn get_radius_at_angle(&self, angle: f32) -> f32 {
        let angle_deg = angle.to_degrees().rem_euclid(360.0);
        let segment_size = 360.0 / 32.0;
        let segment = (angle_deg / segment_size) as usize;
        let next_segment = (segment + 1) % 32;
        let segment_angle = segment as f32 * segment_size;
        let t = (angle_deg - segment_angle) / segment_size;
        (1.0 - t) * self.radii[segment] + t * self.radii[next_segment]
    }
}

// ===========================================================================
//  5. Response Curve Shaping
// ===========================================================================

/// Selectable response-curve shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ResponseCurveType {
    /// `f(x) = x`
    Linear,
    /// `f(x) = x^power`
    #[default]
    Exponential,
    /// Smoothstep: `f(x) = x²·(3 - 2x)`
    SCurve,
    /// Cubic Bézier with two control points.
    Bezier,
}

/// Response-curve shaper applied to the stick magnitude while preserving
/// direction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseCurve {
    /// Active curve type.
    pub curve_type: ResponseCurveType,
    /// Exponent for the `Exponential` curve (default 1.3).
    pub power: f32,
    /// Bézier control point 1 (default `[0.3, 0.9]`).
    pub bezier_p1: [f32; 2],
    /// Bézier control point 2 (default `[0.7, 0.1]`).
    pub bezier_p2: [f32; 2],
}

impl Default for ResponseCurve {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseCurve {
    /// Create a new response curve with default (exponential, power 1.3).
    pub fn new() -> Self {
        Self {
            curve_type: ResponseCurveType::Exponential,
            power: 1.3,
            bezier_p1: [0.3, 0.9],
            bezier_p2: [0.7, 0.1],
        }
    }

    /// Apply the response curve to a magnitude value in `[0, 1]`.
    pub fn apply_to_magnitude(&self, input: f32) -> f32 {
        let input = input.clamp(0.0, 1.0);
        match self.curve_type {
            ResponseCurveType::Linear => input,
            ResponseCurveType::Exponential => input.powf(self.power),
            ResponseCurveType::SCurve => input * input * (3.0 - 2.0 * input),
            ResponseCurveType::Bezier => self.cubic_bezier(input),
        }
    }

    /// Apply the response curve to `(x, y)` preserving direction.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let magnitude = (x * x + y * y).sqrt();
        if magnitude == 0.0 {
            return (0.0, 0.0);
        }
        let angle = y.atan2(x);
        let new_magnitude = self.apply_to_magnitude(magnitude.min(1.0));
        (new_magnitude * angle.cos(), new_magnitude * angle.sin())
    }

    /// Cubic Bézier evaluation.
    ///
    /// `B(t) = (1-t)³·0 + 3(1-t)²·t·p1y + 3(1-t)·t²·p2y + t³·1`
    fn cubic_bezier(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        let one_minus_t = 1.0 - t;
        3.0 * one_minus_t * one_minus_t * t * self.bezier_p1[1]
            + 3.0 * one_minus_t * t * t * self.bezier_p2[1]
            + t * t * t
    }
}

// ===========================================================================
//  6. Complete Calibration Pipeline
// ===========================================================================

/// Snapshot of the pipeline's current calibration state for UI display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationStatus {
    /// Latest drift classification.
    pub drift_status: DriftStatus,
    /// Current noise floor (median of rest magnitudes).
    pub noise_floor: f32,
    /// Current adaptive deadzone radius.
    pub adaptive_deadzone: f32,
    /// Learned center offset `(x, y)`.
    pub center_offset: (f32, f32),
    /// Whether the center estimate is frozen.
    pub center_locked: bool,
    /// Whether the gate map has been measured.
    pub gate_calibrated: bool,
}

/// Full stick calibration pipeline chaining every stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StickCalibrationPipeline {
    /// Auto-centering stage.
    pub center_cal: CenterAutoCal,
    /// Adaptive deadzone stage.
    pub adaptive_deadzone: AdaptiveDeadzone,
    /// Drift detector stage.
    pub drift_detector: DriftDetector,
    /// Gate calibration stage.
    pub gate_cal: GateCalibration,
    /// Response curve stage.
    pub response_curve: ResponseCurve,
    /// Master enable flag. When `false`, `process` returns the raw input.
    pub enabled: bool,
}

impl Default for StickCalibrationPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl StickCalibrationPipeline {
    /// Create a new pipeline with default stage configurations, enabled.
    pub fn new() -> Self {
        Self {
            center_cal: CenterAutoCal::new(0.0, 0.0),
            adaptive_deadzone: AdaptiveDeadzone::new(),
            drift_detector: DriftDetector::new(),
            gate_cal: GateCalibration::new(),
            response_curve: ResponseCurve::new(),
            enabled: true,
        }
    }

    /// Process a raw stick input through the full pipeline.
    ///
    /// Returns `(x, y, drift_status)`.
    pub fn process(&mut self, x: f32, y: f32) -> (f32, f32, DriftStatus) {
        if !self.enabled {
            return (x, y, DriftStatus::Unknown);
        }

        // Step 1: Center offset correction.
        let (cx, cy) = self.center_cal.apply(x, y);

        // Step 2: Check if resting (for center update + noise floor).
        let magnitude = (cx * cx + cy * cy).sqrt();
        let is_resting = magnitude < self.center_cal.movement_threshold;

        // Step 3: Update center estimate.
        self.center_cal.update(x, y);

        // Step 4: Update noise floor.
        if is_resting {
            self.adaptive_deadzone.update_noise_floor(magnitude);
        }

        // Step 5: Record for drift detection.
        self.drift_detector.record(magnitude);
        let drift_status = self.drift_detector.get_status();

        // Step 6: Apply adaptive deadzone.
        let (dx, dy) = self.adaptive_deadzone.apply(cx, cy);

        // Step 7: Apply gate calibration.
        let (gx, gy) = self.gate_cal.apply(dx, dy);

        // Step 8: Apply response curve.
        let (rx, ry) = self.response_curve.apply(gx, gy);

        (rx, ry, drift_status)
    }

    /// Get a snapshot of the current calibration state for UI display.
    pub fn get_status(&self) -> CalibrationStatus {
        CalibrationStatus {
            drift_status: self.drift_detector.get_status(),
            noise_floor: self.adaptive_deadzone.current_noise_floor,
            adaptive_deadzone: self.adaptive_deadzone.get_deadzone(),
            center_offset: self.center_cal.get_center(),
            center_locked: self.center_cal.locked,
            gate_calibrated: self.gate_cal.calibrated,
        }
    }

    /// Force a center recalibration (unlocks the EMA estimate).
    pub fn recalibrate_center(&mut self) {
        self.center_cal.unlock();
    }

    /// Reconfigure pipeline stages from a `StickCalibrationConfig`.
    ///
    /// This syncs the UI-set config flags (adaptive deadzone, drift detection,
    /// gate calibration, response curve) to the actual pipeline stages. Should
    /// be called periodically (e.g. every ~1s) by the device loop.
    pub fn reconfigure(&mut self, config: &crate::state::StickCalibrationConfig) {
        // Adaptive deadzone: update safety margin and bounds.
        self.adaptive_deadzone.safety_margin = config.deadzone_safety_margin;
        self.adaptive_deadzone.min_deadzone = config.min_deadzone;
        self.adaptive_deadzone.max_deadzone = config.max_deadzone;

        // Gate calibration: enable/disable (but don't disable if calibrated).
        self.gate_cal.enabled = config.gate_calibration_enabled || self.gate_cal.calibrated;

        // Response curve: update type, power, and bezier points.
        self.response_curve.curve_type = match config.response_curve_type.as_str() {
            "linear" => ResponseCurveType::Linear,
            "exponential" => ResponseCurveType::Exponential,
            "s-curve" => ResponseCurveType::SCurve,
            "bezier" => ResponseCurveType::Bezier,
            _ => ResponseCurveType::Exponential, // default fallback
        };
        self.response_curve.power = config.response_curve_power;
        self.response_curve.bezier_p1 = config.bezier_p1;
        self.response_curve.bezier_p2 = config.bezier_p2;
    }

    /// Reset every stage to its initial state.
    pub fn reset(&mut self) {
        self.center_cal = CenterAutoCal::new(0.0, 0.0);
        self.adaptive_deadzone.reset();
        self.drift_detector.reset();
        self.gate_cal = GateCalibration::new();
        self.response_curve = ResponseCurve::new();
    }
}

// ===========================================================================
//  7. Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers ------------------------------------------------------------

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    // ===================================================================
    //  AdaptiveDeadzone
    // ===================================================================

    mod adaptive_deadzone_tests {
        use super::*;

        #[test]
        fn defaults_are_sane() {
            let dz = AdaptiveDeadzone::new();
            assert_eq!(dz.buffer_size, 256);
            assert!((dz.safety_margin - 1.5).abs() < 1e-6);
            assert!((dz.min_deadzone - 0.01).abs() < 1e-6);
            assert!((dz.max_deadzone - 0.15).abs() < 1e-6);
            assert!((dz.get_deadzone() - 0.01).abs() < 1e-6);
        }

        #[test]
        fn noise_floor_tracking_uses_median() {
            let mut dz = AdaptiveDeadzone::new();
            // Feed a known set of rest magnitudes.
            for &m in &[0.02, 0.02, 0.03, 0.02, 0.04] {
                dz.update_noise_floor(m);
            }
            // Sorted: 0.02,0.02,0.02,0.03,0.04 → median 0.02 (index 2 of 5).
            assert!(
                (dz.current_noise_floor - 0.02).abs() < 1e-6,
                "noise floor = {}",
                dz.current_noise_floor
            );
            // deadzone = 0.02 * 1.5 = 0.03, within [0.01, 0.15].
            assert!((dz.get_deadzone() - 0.03).abs() < 1e-6);
        }

        #[test]
        fn deadzone_clamps_to_min() {
            let mut dz = AdaptiveDeadzone::new();
            for _ in 0..10 {
                dz.update_noise_floor(0.0);
            }
            // noise floor 0 → base 0 → clamped to min_deadzone.
            assert!((dz.get_deadzone() - dz.min_deadzone).abs() < 1e-6);
        }

        #[test]
        fn deadzone_clamps_to_max() {
            let mut dz = AdaptiveDeadzone::new();
            for _ in 0..10 {
                dz.update_noise_floor(1.0);
            }
            // noise floor 1.0 → base 1.5 → clamped to max_deadzone.
            assert!((dz.get_deadzone() - dz.max_deadzone).abs() < 1e-6);
        }

        #[test]
        fn circular_buffer_evicts_oldest() {
            let mut dz = AdaptiveDeadzone::new();
            dz.buffer_size = 4;
            for &m in &[0.1, 0.2, 0.3, 0.4, 0.5] {
                dz.update_noise_floor(m);
            }
            // Only the last 4 samples should remain: 0.2,0.3,0.4,0.5.
            assert_eq!(dz.noise_samples.len(), 4);
            assert!((dz.noise_samples[0] - 0.2).abs() < 1e-6);
            assert!((dz.noise_samples[3] - 0.5).abs() < 1e-6);
        }

        #[test]
        fn apply_zeroes_inside_deadzone() {
            let mut dz = AdaptiveDeadzone::new();
            dz.min_deadzone = 0.1;
            dz.max_deadzone = 0.2;
            dz.current_noise_floor = 0.1; // deadzone = 0.15
            dz.current_deadzone = dz.get_deadzone();
            let (x, y) = dz.apply(0.05, 0.05);
            let mag = (0.05f32 * 0.05 + 0.05 * 0.05).sqrt();
            assert!(mag < dz.get_deadzone());
            assert_eq!(x, 0.0);
            assert_eq!(y, 0.0);
        }

        #[test]
        fn apply_remaps_outside_deadzone_preserving_direction() {
            let mut dz = AdaptiveDeadzone::new();
            dz.min_deadzone = 0.1;
            dz.max_deadzone = 0.2;
            dz.current_noise_floor = 0.1; // deadzone = 0.15
            dz.current_deadzone = dz.get_deadzone();
            let deadzone = dz.get_deadzone();
            // Input magnitude 1.0 → remapped to 1.0 (full range).
            let (x, y) = dz.apply(1.0, 0.0);
            assert!((x - 1.0).abs() < 1e-5, "x = {}", x);
            assert!(y.abs() < 1e-5);
            // Input magnitude just above deadzone → near zero.
            let mag = deadzone + 0.01;
            let (x, y) = dz.apply(mag, 0.0);
            let expected = (mag - deadzone) / (1.0 - deadzone);
            assert!(
                (x - expected).abs() < 1e-5,
                "x = {}, expected = {}",
                x,
                expected
            );
            assert!(y.abs() < 1e-5);
        }

        #[test]
        fn apply_preserves_direction_diagonal() {
            let mut dz = AdaptiveDeadzone::new();
            dz.min_deadzone = 0.05;
            dz.max_deadzone = 0.2;
            dz.current_noise_floor = 0.05; // deadzone = 0.075
            dz.current_deadzone = dz.get_deadzone();
            let (x, y) = dz.apply(0.5, 0.5);
            // Direction should still be 45°.
            let angle = y.atan2(x);
            assert!(
                (angle - std::f32::consts::FRAC_PI_4).abs() < 1e-4,
                "angle = {} rad",
                angle
            );
        }

        #[test]
        fn reset_clears_buffer() {
            let mut dz = AdaptiveDeadzone::new();
            for _ in 0..5 {
                dz.update_noise_floor(0.05);
            }
            dz.reset();
            assert!(dz.noise_samples.is_empty());
            assert!((dz.current_noise_floor).abs() < 1e-6);
        }
    }

    // ===================================================================
    //  CenterAutoCal
    // ===================================================================

    mod center_auto_cal_tests {
        use super::*;

        #[test]
        fn defaults() {
            let c = CenterAutoCal::new(0.05, -0.02);
            assert!((c.alpha - 0.1).abs() < 1e-6);
            assert_eq!(c.lock_threshold, 100);
            assert!((c.movement_threshold - 0.03).abs() < 1e-6);
            assert!(!c.locked);
            assert_eq!(c.lock_counter, 0);
            assert!((c.center_x - 0.05).abs() < 1e-6);
            assert!((c.center_y + 0.02).abs() < 1e-6);
        }

        #[test]
        fn ema_update_when_resting() {
            let mut c = CenterAutoCal::new(0.0, 0.0);
            // Feed a constant small offset; center should converge toward it.
            for _ in 0..1000 {
                c.update(0.01, 0.0);
            }
            // After many EMA steps with alpha=0.1, center → 0.01.
            assert!(
                (c.center_x - 0.01).abs() < 1e-3,
                "center_x = {}",
                c.center_x
            );
            assert!(c.center_y.abs() < 1e-3);
        }

        #[test]
        fn does_not_update_when_moving() {
            let mut c = CenterAutoCal::new(0.0, 0.0);
            c.update(0.5, 0.5); // magnitude ~0.707 > threshold
            assert!((c.center_x).abs() < 1e-6);
            assert!((c.center_y).abs() < 1e-6);
            assert_eq!(c.lock_counter, 0);
        }

        #[test]
        fn locks_after_threshold() {
            let mut c = CenterAutoCal::new(0.0, 0.0);
            c.lock_threshold = 5;
            for _ in 0..5 {
                c.update(0.01, 0.0);
            }
            assert!(c.locked, "should be locked after threshold frames");
            assert_eq!(c.lock_counter, 5);
        }

        #[test]
        fn locked_does_not_update() {
            let mut c = CenterAutoCal::new(0.0, 0.0);
            c.lock_threshold = 3;
            for _ in 0..3 {
                c.update(0.01, 0.0);
            }
            assert!(c.locked);
            let frozen_x = c.center_x;
            // Further resting updates should not move the center.
            for _ in 0..100 {
                c.update(0.5, 0.5);
            }
            assert!((c.center_x - frozen_x).abs() < 1e-6);
        }

        #[test]
        fn unlock_resets_counter() {
            let mut c = CenterAutoCal::new(0.0, 0.0);
            c.lock_threshold = 3;
            for _ in 0..3 {
                c.update(0.01, 0.0);
            }
            assert!(c.locked);
            c.unlock();
            assert!(!c.locked);
            assert_eq!(c.lock_counter, 0);
        }

        #[test]
        fn apply_subtracts_center() {
            let c = CenterAutoCal::new(0.1, -0.05);
            let (x, y) = c.apply(0.3, 0.2);
            assert!((x - 0.2).abs() < 1e-6);
            assert!((y - 0.25).abs() < 1e-6);
        }

        #[test]
        fn get_center_returns_estimate() {
            let c = CenterAutoCal::new(0.1, 0.2);
            let (x, y) = c.get_center();
            assert!((x - 0.1).abs() < 1e-6);
            assert!((y - 0.2).abs() < 1e-6);
        }
    }

    // ===================================================================
    //  DriftDetector
    // ===================================================================

    mod drift_detector_tests {
        use super::*;

        #[test]
        fn empty_is_unknown() {
            let d = DriftDetector::new();
            assert_eq!(d.get_status(), DriftStatus::Unknown);
        }

        #[test]
        fn pass_when_median_low_and_spread_low() {
            let mut d = DriftDetector::new();
            for _ in 0..100 {
                d.record(0.02);
            }
            assert_eq!(d.get_status(), DriftStatus::Pass);
        }

        #[test]
        fn drifting_when_median_in_warning_band() {
            let mut d = DriftDetector::new();
            for _ in 0..100 {
                d.record(0.15);
            }
            // median 0.15, spread 0 → Drifting.
            assert_eq!(d.get_status(), DriftStatus::Drifting);
        }

        #[test]
        fn fail_when_median_high() {
            let mut d = DriftDetector::new();
            for _ in 0..100 {
                d.record(0.30);
            }
            assert_eq!(d.get_status(), DriftStatus::Fail);
        }

        #[test]
        fn unknown_when_spread_too_high() {
            let mut d = DriftDetector::new();
            // Mix of low and high → large spread.
            for i in 0..100 {
                d.record(if i % 2 == 0 { 0.01 } else { 0.5 });
            }
            assert_eq!(d.get_status(), DriftStatus::Unknown);
        }

        #[test]
        fn circular_buffer_evicts() {
            let mut d = DriftDetector::new();
            d.buffer_size = 4;
            for &m in &[0.1, 0.2, 0.3, 0.4, 0.5] {
                d.record(m);
            }
            assert_eq!(d.magnitude_history.len(), 4);
            assert!((d.magnitude_history[0] - 0.2).abs() < 1e-6);
        }

        #[test]
        fn reset_clears_history() {
            let mut d = DriftDetector::new();
            for _ in 0..10 {
                d.record(0.5);
            }
            d.reset();
            assert!(d.magnitude_history.is_empty());
            assert_eq!(d.get_status(), DriftStatus::Unknown);
        }

        #[test]
        fn boundary_pass_to_drifting() {
            let mut d = DriftDetector::new();
            // median exactly 0.10 → not < 0.10 → Drifting.
            for _ in 0..100 {
                d.record(0.10);
            }
            assert_eq!(d.get_status(), DriftStatus::Drifting);
        }

        #[test]
        fn boundary_drifting_to_fail() {
            let mut d = DriftDetector::new();
            for _ in 0..100 {
                d.record(0.25);
            }
            assert_eq!(d.get_status(), DriftStatus::Fail);
        }
    }

    // ===================================================================
    //  GateCalibration
    // ===================================================================

    mod gate_calibration_tests {
        use super::*;

        #[test]
        fn disabled_passthrough() {
            let g = GateCalibration::new();
            assert!(!g.enabled);
            let (x, y) = g.apply(0.5, 0.5);
            assert!((x - 0.5).abs() < 1e-6);
            assert!((y - 0.5).abs() < 1e-6);
        }

        #[test]
        fn enabled_zero_returns_zero() {
            let mut g = GateCalibration::new();
            g.enabled = true;
            let (x, y) = g.apply(0.0, 0.0);
            assert_eq!(x, 0.0);
            assert_eq!(y, 0.0);
        }

        #[test]
        fn mathematical_octagon_cardinal_unchanged() {
            let mut g = GateCalibration::new();
            g.enabled = true;
            // Along +X axis: cos=1, sin=0 → max_radius = 1/(1 + 0) = 1.
            let (x, y) = g.apply(1.0, 0.0);
            assert!((x - 1.0).abs() < 1e-5, "x = {}", x);
            assert!(y.abs() < 1e-5);
        }

        #[test]
        fn mathematical_octagon_diagonal_scaled() {
            let mut g = GateCalibration::new();
            g.enabled = true;
            // At 45°: |cos|=|sin|=0.7071 → max_radius = 1/(0.7071 + 0.7071*0.4142)
            let angle = std::f32::consts::FRAC_PI_4;
            let cos_a = angle.cos();
            let sin_a = angle.sin();
            let max_radius =
                1.0 / (cos_a.abs().max(sin_a.abs()) + cos_a.abs().min(sin_a.abs()) * 0.4142);
            // Feed a magnitude equal to the octagon edge at 45°.
            let (x, y) = g.apply(max_radius * cos_a, max_radius * sin_a);
            let out_mag = (x * x + y * y).sqrt();
            assert!((out_mag - 1.0).abs() < 1e-4, "out_mag = {}", out_mag);
        }

        #[test]
        fn calibrate_from_sweep_samples() {
            let mut g = GateCalibration::new();
            // Generate samples on a unit circle at 32 angles.
            let mut samples = Vec::new();
            for i in 0..32 {
                let angle = (i as f32) * (360.0_f32 / 32.0_f32).to_radians();
                samples.push((angle.cos(), angle.sin()));
            }
            g.calibrate(&samples);
            assert!(g.calibrated);
            assert!(g.enabled);
            // Each segment should have captured radius ~1.0.
            for &r in &g.radii {
                assert!((r - 1.0).abs() < 1e-5, "radius = {}", r);
            }
        }

        #[test]
        fn calibrate_then_apply_unit_circle() {
            let mut g = GateCalibration::new();
            // Simulate an octagonal sweep: cardinal radius 1.0, diagonal 0.7071.
            let mut samples = Vec::new();
            for i in 0..360 {
                let angle = (i as f32).to_radians();
                let cos_a = angle.cos();
                let sin_a = angle.sin();
                let max_r =
                    1.0 / (cos_a.abs().max(sin_a.abs()) + cos_a.abs().min(sin_a.abs()) * 0.4142);
                samples.push((max_r * cos_a, max_r * sin_a));
            }
            g.calibrate(&samples);
            // A diagonal input at the octagon edge should normalize to ~1.0.
            let angle = std::f32::consts::FRAC_PI_4;
            let cos_a = angle.cos();
            let sin_a = angle.sin();
            let max_r =
                1.0 / (cos_a.abs().max(sin_a.abs()) + cos_a.abs().min(sin_a.abs()) * 0.4142);
            let (x, y) = g.apply(max_r * cos_a, max_r * sin_a);
            let out_mag = (x * x + y * y).sqrt();
            assert!((out_mag - 1.0).abs() < 1e-3, "out_mag = {}", out_mag);
        }

        #[test]
        fn interpolation_between_segments() {
            let mut g = GateCalibration::new();
            g.enabled = true;
            g.calibrated = true;
            // Set two adjacent segments to 0.5 and 1.0.
            g.radii[0] = 0.5;
            g.radii[1] = 1.0;
            // Angle 0° → segment 0, t=0 → radius 0.5.
            let r0 = g.get_radius_at_angle(0.0f32.to_radians());
            assert!((r0 - 0.5).abs() < 1e-5);
            // Angle 11.25° → segment 0, t=1 → radius 1.0 (segment 1).
            let r1 = g.get_radius_at_angle(11.25f32.to_radians());
            assert!((r1 - 1.0).abs() < 1e-5);
            // Angle 5.625° → midpoint → 0.75.
            let rmid = g.get_radius_at_angle(5.625f32.to_radians());
            assert!((rmid - 0.75).abs() < 1e-4, "rmid = {}", rmid);
        }

        #[test]
        fn empty_samples_does_not_calibrate() {
            let mut g = GateCalibration::new();
            g.calibrate(&[]);
            assert!(!g.calibrated);
        }
    }

    // ===================================================================
    //  ResponseCurve
    // ===================================================================

    mod response_curve_tests {
        use super::*;

        #[test]
        fn defaults_exponential_power_1_3() {
            let r = ResponseCurve::new();
            assert_eq!(r.curve_type, ResponseCurveType::Exponential);
            assert!((r.power - 1.3).abs() < 1e-6);
            assert_eq!(r.bezier_p1, [0.3, 0.9]);
            assert_eq!(r.bezier_p2, [0.7, 0.1]);
        }

        #[test]
        fn linear_is_identity() {
            let mut r = ResponseCurve::new();
            r.curve_type = ResponseCurveType::Linear;
            for &v in &[0.0, 0.25, 0.5, 0.75, 1.0] {
                assert!((r.apply_to_magnitude(v) - v).abs() < 1e-6);
            }
        }

        #[test]
        fn exponential_powf() {
            let mut r = ResponseCurve::new();
            r.curve_type = ResponseCurveType::Exponential;
            r.power = 2.0;
            assert!((r.apply_to_magnitude(0.5) - 0.25).abs() < 1e-5);
            assert!((r.apply_to_magnitude(1.0) - 1.0).abs() < 1e-5);
            assert!((r.apply_to_magnitude(0.0) - 0.0).abs() < 1e-5);
        }

        #[test]
        fn s_curve_smoothstep() {
            let mut r = ResponseCurve::new();
            r.curve_type = ResponseCurveType::SCurve;
            // smoothstep(0)=0, smoothstep(1)=1, smoothstep(0.5)=0.5
            assert!((r.apply_to_magnitude(0.0) - 0.0).abs() < 1e-6);
            assert!((r.apply_to_magnitude(1.0) - 1.0).abs() < 1e-6);
            assert!((r.apply_to_magnitude(0.5) - 0.5).abs() < 1e-6);
            // smoothstep(0.25) = 0.0625 * 2.5 = 0.15625
            assert!((r.apply_to_magnitude(0.25) - 0.15625).abs() < 1e-6);
        }

        #[test]
        fn bezier_endpoints() {
            let mut r = ResponseCurve::new();
            r.curve_type = ResponseCurveType::Bezier;
            assert!((r.apply_to_magnitude(0.0) - 0.0).abs() < 1e-6);
            assert!((r.apply_to_magnitude(1.0) - 1.0).abs() < 1e-6);
        }

        #[test]
        fn bezier_midpoint_in_range() {
            let mut r = ResponseCurve::new();
            r.curve_type = ResponseCurveType::Bezier;
            let v = r.apply_to_magnitude(0.5);
            assert!(v >= 0.0 && v <= 1.0, "bezier(0.5) = {}", v);
        }

        #[test]
        fn apply_preserves_direction() {
            let r = ResponseCurve::new();
            let (x, y) = r.apply(0.5, 0.0);
            let angle = y.atan2(x);
            assert!(angle.abs() < 1e-4, "angle = {}", angle);
            // Magnitude should be shaped.
            let mag = (x * x + y * y).sqrt();
            assert!(mag > 0.0 && mag <= 1.0);
        }

        #[test]
        fn apply_zero_returns_zero() {
            let r = ResponseCurve::new();
            let (x, y) = r.apply(0.0, 0.0);
            assert_eq!(x, 0.0);
            assert_eq!(y, 0.0);
        }

        #[test]
        fn apply_to_magnitude_clamps_input() {
            let r = ResponseCurve::new();
            // Input > 1 should be clamped to 1.
            assert!((r.apply_to_magnitude(2.0) - 1.0).abs() < 1e-5);
            // Input < 0 should be clamped to 0.
            assert!((r.apply_to_magnitude(-1.0) - 0.0).abs() < 1e-5);
        }
    }

    // ===================================================================
    //  StickCalibrationPipeline
    // ===================================================================

    mod pipeline_tests {
        use super::*;

        #[test]
        fn disabled_passthrough() {
            let mut p = StickCalibrationPipeline::new();
            p.enabled = false;
            let (x, y, status) = p.process(0.5, 0.3);
            assert!((x - 0.5).abs() < 1e-6);
            assert!((y - 0.3).abs() < 1e-6);
            assert_eq!(status, DriftStatus::Unknown);
        }

        #[test]
        fn centered_zero_stays_zero() {
            let mut p = StickCalibrationPipeline::new();
            // Feed many zero frames so the center locks and noise floor settles.
            for _ in 0..200 {
                p.process(0.0, 0.0);
            }
            let (x, y, _) = p.process(0.0, 0.0);
            assert!(x.abs() < 1e-5, "x = {}", x);
            assert!(y.abs() < 1e-5, "y = {}", y);
        }

        #[test]
        fn full_deflection_passes_through() {
            let mut p = StickCalibrationPipeline::new();
            // Disable gate so it doesn't reshape.
            p.gate_cal.enabled = false;
            // First settle the center.
            for _ in 0..200 {
                p.process(0.0, 0.0);
            }
            // Now a full +X deflection.
            let (x, y, _) = p.process(1.0, 0.0);
            // After deadzone + response curve, magnitude should be > 0.9.
            let mag = (x * x + y * y).sqrt();
            assert!(mag > 0.9, "mag = {}", mag);
            // Direction preserved.
            assert!(y.abs() < 1e-4);
            assert!(x > 0.0);
        }

        #[test]
        fn drift_status_passes_after_rest() {
            let mut p = StickCalibrationPipeline::new();
            for _ in 0..300 {
                p.process(0.0, 0.0);
            }
            let status = p.get_status();
            assert_eq!(status.drift_status, DriftStatus::Pass);
            assert!(status.center_locked);
        }

        #[test]
        fn drift_status_fails_with_persistent_offset() {
            let mut p = StickCalibrationPipeline::new();
            // Feed a persistent offset that the center cal will chase but
            // movement_threshold is small so it should still rest.
            // Use a large offset above fail_threshold.
            for _ in 0..300 {
                p.process(0.3, 0.0);
            }
            let status = p.get_status();
            // magnitude 0.3 → Fail.
            assert_eq!(status.drift_status, DriftStatus::Fail);
        }

        #[test]
        fn recalibrate_center_unlocks() {
            let mut p = StickCalibrationPipeline::new();
            for _ in 0..200 {
                p.process(0.0, 0.0);
            }
            assert!(p.center_cal.locked);
            p.recalibrate_center();
            assert!(!p.center_cal.locked);
            assert_eq!(p.center_cal.lock_counter, 0);
        }

        #[test]
        fn reset_clears_all_stages() {
            let mut p = StickCalibrationPipeline::new();
            for _ in 0..200 {
                p.process(0.3, 0.0);
            }
            p.reset();
            assert!(!p.center_cal.locked);
            assert!(p.adaptive_deadzone.noise_samples.is_empty());
            assert!(p.drift_detector.magnitude_history.is_empty());
            assert!(!p.gate_cal.calibrated);
        }

        #[test]
        fn get_status_returns_consistent_snapshot() {
            let mut p = StickCalibrationPipeline::new();
            for _ in 0..200 {
                p.process(0.0, 0.0);
            }
            let s = p.get_status();
            assert_eq!(s.drift_status, p.drift_detector.get_status());
            assert!((s.noise_floor - p.adaptive_deadzone.current_noise_floor).abs() < 1e-6);
            assert!((s.adaptive_deadzone - p.adaptive_deadzone.get_deadzone()).abs() < 1e-6);
            assert_eq!(s.center_offset, p.center_cal.get_center());
            assert_eq!(s.center_locked, p.center_cal.locked);
            assert_eq!(s.gate_calibrated, p.gate_cal.calibrated);
        }

        #[test]
        fn pipeline_preserves_direction_for_diagonal() {
            let mut p = StickCalibrationPipeline::new();
            p.gate_cal.enabled = false;
            for _ in 0..200 {
                p.process(0.0, 0.0);
            }
            let (x, y, _) = p.process(0.5, 0.5);
            let angle = y.atan2(x);
            assert!(
                (angle - std::f32::consts::FRAC_PI_4).abs() < 1e-3,
                "angle = {} rad",
                angle
            );
        }

        #[test]
        fn noise_floor_adapts_over_time() {
            let mut p = StickCalibrationPipeline::new();
            // Feed small jitter at rest.
            for i in 0..300 {
                let jitter = if i % 2 == 0 { 0.005 } else { -0.005 };
                p.process(jitter, 0.0);
            }
            // Noise floor should be roughly the magnitude of the jitter.
            let nf = p.adaptive_deadzone.current_noise_floor;
            assert!(nf > 0.0, "noise floor = {}", nf);
            // Deadzone should be above the noise floor.
            let dz = p.adaptive_deadzone.get_deadzone();
            assert!(dz >= nf, "dz = {}, nf = {}", dz, nf);
        }
    }
}
