//! OxideLink response-curve, deadzone-shape, and stick-zone helpers.
//!
//! This module provides the math for shaping normalized stick inputs
//! (`ResponseCurveType`) plus zone-triggered action lookup and configurable
//! deadzone geometry (radial / axial / elliptic). It is intentionally
//! self-contained so it can be unit-tested and reused by the telemetry
//! pipeline, the calibration pipeline, and Tauri command handlers.

use std::sync::Arc;

use log::{debug, info};
use tauri::State;

use crate::state::{Action, ResponseCurveType, SharedState, StickZones};

/// Clamp a stick coordinate to the [-1, 1] unit square.
fn clamp_unit(v: f32) -> f32 {
    v.clamp(-1.0, 1.0)
}

/// Validate response-curve parameters supplied by configuration or commands.
pub fn validate_response_curve(curve: &ResponseCurveType) -> Result<(), String> {
    match curve {
        ResponseCurveType::Exponential(power) if !power.is_finite() => {
            Err("response curve power must be finite".into())
        }
        ResponseCurveType::Bezier { p1, p2 }
            if !p1.iter().chain(p2.iter()).all(|point| point.is_finite()) =>
        {
            Err("Bezier response curve control points must be finite".into())
        }
        _ => Ok(()),
    }
}

/// Apply a response curve to a single signed input value.
///
/// * `Linear`      — identity (`y = x`).
/// * `Exponential(power)` — `sign(x) * |x|^power`.
/// * `SCurve`      — smoothstep `3t^2 - 2t^3` applied to `|x|`, re-signed.
/// * `Bezier { p1, p2 }` — cubic Bézier from `(0,0)` to `(1,1)` with the
///   supplied control points; solved for `t` given `|x|` via Newton-Raphson
///   with a bisection fallback, then re-signed.
pub fn apply_response_curve(input: f32, curve: &ResponseCurveType) -> f32 {
    let input = clamp_unit(input);
    if validate_response_curve(curve).is_err() {
        return input;
    }
    if input == 0.0 {
        return 0.0;
    }
    let sign = input.signum();
    let abs = input.abs();
    let out_abs = match curve {
        ResponseCurveType::Linear => abs,
        ResponseCurveType::Exponential(power) => abs.powf(*power),
        ResponseCurveType::SCurve => abs * abs * (3.0 - 2.0 * abs),
        ResponseCurveType::Bezier { p1, p2 } => {
            let p1 = [p1[0].clamp(0.0, 1.0), p1[1].clamp(0.0, 1.0)];
            let p2 = [p2[0].clamp(0.0, 1.0), p2[1].clamp(0.0, 1.0)];
            cubic_bezier_y_for_x(abs, &p1, &p2)
        }
    };
    sign * out_abs.clamp(0.0, 1.0)
}

/// Apply a response curve to an `(x, y)` stick pair, per-axis.
///
/// This preserves the sign of each axis. Callers that want radial
/// (magnitude-preserving) shaping can compute the magnitude, run it through
/// [`apply_response_curve`], and rescale the unit vector.
pub fn apply_stick_curve(x: f32, y: f32, curve: &ResponseCurveType) -> (f32, f32) {
    (
        apply_response_curve(x, curve),
        apply_response_curve(y, curve),
    )
}

/// Apply a response curve to an `(x, y)` stick pair while preserving the
/// original magnitude direction. The curve is applied to the polar
/// magnitude and the resulting vector is rescaled.
pub fn apply_stick_curve_radial(x: f32, y: f32, curve: &ResponseCurveType) -> (f32, f32) {
    let x = clamp_unit(x);
    let y = clamp_unit(y);
    let m = (x * x + y * y).sqrt();
    if m == 0.0 {
        return (0.0, 0.0);
    }
    let new_m = apply_response_curve(m, curve).clamp(0.0, 1.0);
    let scale = new_m / m;
    (x * scale, y * scale)
}

/// Cubic Bézier x(t) for endpoints P0=(0,0) and P3=(1,1).
fn bezier_x(t: f32, p1x: f32, p2x: f32) -> f32 {
    let omt = 1.0 - t;
    3.0 * omt * omt * t * p1x + 3.0 * omt * t * t * p2x + t * t * t
}

/// Derivative of the cubic Bézier x(t) with respect to t.
fn bezier_dx(t: f32, p1x: f32, p2x: f32) -> f32 {
    let omt = 1.0 - t;
    3.0 * omt * omt * p1x + 6.0 * omt * t * (p2x - p1x) + 3.0 * t * t * (1.0 - p2x)
}

/// Cubic Bézier y(t) for endpoints P0=(0,0) and P3=(1,1).
fn bezier_y(t: f32, p1y: f32, p2y: f32) -> f32 {
    let omt = 1.0 - t;
    3.0 * omt * omt * t * p1y + 3.0 * omt * t * t * p2y + t * t * t
}

/// Solve for `t` such that `bezier_x(t) == target_x`, then return `bezier_y(t)`.
///
/// Uses Newton-Raphson with bisection fallback to handle near-flat slopes.
fn cubic_bezier_y_for_x(target_x: f32, p1: &[f32; 2], p2: &[f32; 2]) -> f32 {
    let target_x = target_x.clamp(0.0, 1.0);
    let p1x = p1[0];
    let p1y = p1[1];
    let p2x = p2[0];
    let p2y = p2[1];

    // Defensive clamp: control-point x values should stay inside [0,1]
    // for a monotonic, invertible curve. We clamp silently so the UI
    // cannot produce undefined shaping.
    let p1x = p1x.clamp(0.0, 1.0);
    let p2x = p2x.clamp(0.0, 1.0);

    let mut t = target_x; // initial guess
    for _ in 0..8 {
        let x = bezier_x(t, p1x, p2x);
        let dx = bezier_dx(t, p1x, p2x);
        let error = x - target_x;
        if error.abs() < 1e-6 {
            break;
        }
        if dx.abs() < 1e-6 {
            break;
        }
        t -= error / dx;
        t = t.clamp(0.0, 1.0);
    }

    // Bisection refinement / fallback.
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    for _ in 0..32 {
        let mid = (lo + hi) * 0.5;
        let x = bezier_x(mid, p1x, p2x);
        if x < target_x {
            lo = mid;
        } else {
            hi = mid;
        }
        if (x - target_x).abs() < 1e-7 {
            break;
        }
    }
    t = ((lo + hi) * 0.5).clamp(0.0, 1.0);

    bezier_y(t, p1y, p2y).clamp(0.0, 1.0)
}

/// Return the actions associated with the zone that `magnitude` currently
/// occupies.
///
/// * `magnitude <= zones.deadzone` → empty vector.
/// * `zones.deadzone < magnitude <= zones.low` → `low_actions`.
/// * `zones.low < magnitude <= zones.medium` → `medium_actions`.
/// * `zones.medium < magnitude` → `high_actions`.
pub fn zone_action(magnitude: f32, zones: &StickZones) -> Vec<Action> {
    if magnitude <= zones.deadzone {
        Vec::new()
    } else if magnitude <= zones.low {
        zones.low_actions.clone()
    } else if magnitude <= zones.medium {
        zones.medium_actions.clone()
    } else {
        zones.high_actions.clone()
    }
}

/// Apply a deadzone of the requested shape to a 2D stick vector.
///
/// Supported shapes:
/// * `radial`   — circular deadzone; preserves direction and rescales the
///   magnitude from `deadzone..1` to `0..1`.
/// * `axial`    — independent per-axis deadzone and rescale.
/// * `elliptic` — elliptical gate (vertical axis 75% of horizontal); any
///   other string falls back to `radial`.
pub fn apply_deadzone_shape(x: f32, y: f32, deadzone: f32, shape: &str) -> (f32, f32) {
    let shape = shape.to_lowercase();
    match shape.as_str() {
        "radial" => apply_radial_deadzone(x, y, deadzone),
        "axial" => apply_axial_deadzone(x, y, deadzone),
        "elliptic" => apply_elliptic_deadzone(x, y, deadzone),
        _ => apply_radial_deadzone(x, y, deadzone),
    }
}

fn apply_radial_deadzone(x: f32, y: f32, deadzone: f32) -> (f32, f32) {
    let m = (x * x + y * y).sqrt();
    if m <= deadzone || m == 0.0 {
        return (0.0, 0.0);
    }
    let scale = ((m - deadzone) / (1.0 - deadzone)).clamp(0.0, 1.0);
    let scale = scale / m;
    (x * scale, y * scale)
}

fn apply_axial_deadzone(x: f32, y: f32, deadzone: f32) -> (f32, f32) {
    fn axis(v: f32, deadzone: f32) -> f32 {
        let av = v.abs();
        if av <= deadzone {
            0.0
        } else {
            v.signum() * ((av - deadzone) / (1.0 - deadzone)).clamp(0.0, 1.0)
        }
    }
    (axis(x, deadzone), axis(y, deadzone))
}

fn apply_elliptic_deadzone(x: f32, y: f32, deadzone: f32) -> (f32, f32) {
    if deadzone <= 0.0 || x == 0.0 && y == 0.0 {
        return (0.0, 0.0);
    }
    // Horizontal semi-axis = deadzone, vertical = 0.75 * deadzone.
    let a = deadzone.max(1e-6);
    let b = (deadzone * 0.75).max(1e-6);
    let r = ((x / a).powi(2) + (y / b).powi(2)).sqrt();
    if r <= 1.0 {
        return (0.0, 0.0);
    }
    // Rescale the elliptic radius from 1..r_max to 0..1, where r_max is the
    // maximum elliptic radius on the unit-square boundary.
    let r_max = (1.0 / a).max(1.0 / b);
    if r_max <= 1.0 {
        return (0.0, 0.0);
    }
    let scale = ((r - 1.0) / (r_max - 1.0)).clamp(0.0, 1.0) / r;
    (x * scale, y * scale)
}

// =============================================================================
// Tauri commands (not yet wired into main.rs invoke_handler list)
// =============================================================================

/// Update the active stick response curve in `AppConfig.mappings.sticks`.
#[tauri::command]
pub fn set_mapping_response_curve(
    ctx: State<'_, Arc<SharedState>>,
    curve: ResponseCurveType,
) -> Result<(), String> {
    validate_response_curve(&curve)?;
    let shared = Arc::clone(&*ctx);
    {
        let mut config = shared.config.write();
        config.mappings.sticks.response_curve = curve.clone();
    }
    // Mirror the curve parameters into `StickCalibrationConfig` so the advanced
    // calibration pipeline stays consistent (pipeline curve is disabled by
    // setting it to linear; the telemetry stage owns the shaping).
    {
        let mut cal = shared.stick_calibration_config.write();
        cal.response_curve_type = "linear".into();
        cal.response_curve_power = 1.0;
        match curve {
            ResponseCurveType::Bezier { p1, p2 } => {
                cal.bezier_p1 = p1;
                cal.bezier_p2 = p2;
            }
            _ => {
                cal.bezier_p1 = [0.3, 0.9];
                cal.bezier_p2 = [0.7, 0.1];
            }
        }
    }
    info!("Response curve set to {:?}", curve);
    Ok(())
}

/// Read the active stick response curve from `AppConfig.mappings.sticks`.
#[tauri::command]
pub fn get_response_curve(ctx: State<'_, Arc<SharedState>>) -> ResponseCurveType {
    let shared = Arc::clone(&*ctx);
    let curve = shared.config.read().mappings.sticks.response_curve.clone();
    curve
}

/// Update the stick zone thresholds/actions in `AppConfig.mappings.sticks`.
#[tauri::command]
pub fn set_stick_zones(ctx: State<'_, Arc<SharedState>>, zones: StickZones) -> Result<(), String> {
    if zones.deadzone > zones.low || zones.low > zones.medium || zones.medium > zones.high {
        return Err("Stick zone thresholds must be non-decreasing".into());
    }
    let shared = Arc::clone(&*ctx);
    shared.config.write().mappings.sticks.zones = zones;
    info!("Stick zones updated");
    Ok(())
}

/// Read the current stick zone configuration.
#[tauri::command]
pub fn get_stick_zones(ctx: State<'_, Arc<SharedState>>) -> StickZones {
    let shared = Arc::clone(&*ctx);
    let zones = shared.config.read().mappings.sticks.zones.clone();
    zones
}

/// Convenience helper for the telemetry pipeline: applies deadzone-shape,
/// response curve, and zone logging to both sticks without needing the full
/// `AppConfig`.
pub fn apply_stick_curve_and_zones(
    state: &mut crate::state::ControllerState,
    deadzone_left: f32,
    deadzone_right: f32,
    deadzone_shape: &str,
    response_curve: &ResponseCurveType,
    zones: &StickZones,
) {
    // Left stick
    let (lx, ly) = apply_deadzone_shape(
        state.left_stick.x,
        state.left_stick.y,
        deadzone_left,
        deadzone_shape,
    );
    let (lx, ly) = apply_stick_curve(lx, ly, response_curve);
    state.left_stick.x = lx;
    state.left_stick.y = ly;

    // Right stick
    let (rx, ry) = apply_deadzone_shape(
        state.right_stick.x,
        state.right_stick.y,
        deadzone_right,
        deadzone_shape,
    );
    let (rx, ry) = apply_stick_curve(rx, ry, response_curve);
    state.right_stick.x = rx;
    state.right_stick.y = ry;

    // Zone actions
    let left_mag = (lx * lx + ly * ly).sqrt();
    let right_mag = (rx * rx + ry * ry).sqrt();

    for action in zone_action(left_mag, zones) {
        debug!("Left stick zone action triggered: {:?}", action);
    }
    for action in zone_action(right_mag, zones) {
        debug!("Right stick zone action triggered: {:?}", action);
    }
}

// =============================================================================
//  Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Action, ButtonId};

    /// Helper: compare two f32 values with an absolute tolerance.
    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn assert_approx(a: f32, b: f32, msg: &str) {
        assert!(approx(a, b), "{msg}: {a} != {b}");
    }

    fn assert_pair_approx((ax, ay): (f32, f32), (bx, by): (f32, f32), msg: &str) {
        assert_approx(ax, bx, &format!("{msg} (x)"));
        assert_approx(ay, by, &format!("{msg} (y)"));
    }

    // -------------------------------------------------------------------------
    //  clamp_unit
    // -------------------------------------------------------------------------

    #[test]
    fn clamp_unit_within_range_is_unchanged() {
        assert_approx(clamp_unit(0.0), 0.0, "zero unchanged");
        assert_approx(clamp_unit(0.5), 0.5, "mid unchanged");
        assert_approx(clamp_unit(-0.5), -0.5, "negative mid unchanged");
        assert_approx(clamp_unit(1.0), 1.0, "max unchanged");
        assert_approx(clamp_unit(-1.0), -1.0, "min unchanged");
    }

    #[test]
    fn clamp_unit_clamps_overflow() {
        assert_approx(clamp_unit(2.0), 1.0, "over-max clamped to 1");
        assert_approx(clamp_unit(-2.0), -1.0, "under-min clamped to -1");
        assert_approx(clamp_unit(100.0), 1.0, "large positive clamped");
        assert_approx(clamp_unit(-100.0), -1.0, "large negative clamped");
    }

    // -------------------------------------------------------------------------
    //  validate_response_curve
    // -------------------------------------------------------------------------

    #[test]
    fn validate_linear_is_ok() {
        assert!(validate_response_curve(&ResponseCurveType::Linear).is_ok());
    }

    #[test]
    fn validate_scurve_is_ok() {
        assert!(validate_response_curve(&ResponseCurveType::SCurve).is_ok());
    }

    #[test]
    fn validate_exponential_finite_is_ok() {
        assert!(validate_response_curve(&ResponseCurveType::Exponential(2.0)).is_ok());
        assert!(validate_response_curve(&ResponseCurveType::Exponential(0.5)).is_ok());
    }

    #[test]
    fn validate_exponential_nan_is_err() {
        assert!(validate_response_curve(&ResponseCurveType::Exponential(f32::NAN)).is_err());
    }

    #[test]
    fn validate_exponential_infinite_is_err() {
        assert!(validate_response_curve(&ResponseCurveType::Exponential(f32::INFINITY)).is_err());
    }

    #[test]
    fn validate_bezier_finite_is_ok() {
        let curve = ResponseCurveType::Bezier {
            p1: [0.3, 0.9],
            p2: [0.7, 0.1],
        };
        assert!(validate_response_curve(&curve).is_ok());
    }

    #[test]
    fn validate_bezier_nan_is_err() {
        let curve = ResponseCurveType::Bezier {
            p1: [f32::NAN, 0.9],
            p2: [0.7, 0.1],
        };
        assert!(validate_response_curve(&curve).is_err());
    }

    #[test]
    fn validate_bezier_infinite_is_err() {
        let curve = ResponseCurveType::Bezier {
            p1: [0.3, 0.9],
            p2: [0.7, f32::INFINITY],
        };
        assert!(validate_response_curve(&curve).is_err());
    }

    // -------------------------------------------------------------------------
    //  apply_response_curve — Linear
    // -------------------------------------------------------------------------

    #[test]
    fn linear_is_identity() {
        let curve = ResponseCurveType::Linear;
        for &v in &[0.0, 0.25, 0.5, 0.75, 1.0, -0.25, -0.5, -0.75, -1.0] {
            assert_approx(apply_response_curve(v, &curve), v, "linear identity");
        }
    }

    #[test]
    fn linear_clamps_input() {
        let curve = ResponseCurveType::Linear;
        assert_approx(apply_response_curve(2.0, &curve), 1.0, "linear clamps high");
        assert_approx(
            apply_response_curve(-2.0, &curve),
            -1.0,
            "linear clamps low",
        );
    }

    #[test]
    fn linear_zero_is_zero() {
        assert_approx(
            apply_response_curve(0.0, &ResponseCurveType::Linear),
            0.0,
            "zero",
        );
    }

    // -------------------------------------------------------------------------
    //  apply_response_curve — Exponential
    // -------------------------------------------------------------------------

    #[test]
    fn exponential_power_one_is_identity() {
        let curve = ResponseCurveType::Exponential(1.0);
        for &v in &[0.1, 0.5, 0.9, -0.1, -0.5, -0.9] {
            assert_approx(apply_response_curve(v, &curve), v, "exp power=1 identity");
        }
    }

    #[test]
    fn exponential_power_two_squares_magnitude() {
        let curve = ResponseCurveType::Exponential(2.0);
        assert_approx(apply_response_curve(0.5, &curve), 0.25, "exp 0.5^2");
        assert_approx(apply_response_curve(-0.5, &curve), -0.25, "exp -0.5^2");
        assert_approx(apply_response_curve(1.0, &curve), 1.0, "exp 1^2");
        assert_approx(apply_response_curve(-1.0, &curve), -1.0, "exp -1^2");
    }

    #[test]
    fn exponential_power_half_is_sqrt() {
        let curve = ResponseCurveType::Exponential(0.5);
        assert_approx(apply_response_curve(0.25, &curve), 0.5, "exp sqrt 0.25");
        assert_approx(apply_response_curve(-0.25, &curve), -0.5, "exp sqrt -0.25");
    }

    #[test]
    fn exponential_preserves_sign() {
        let curve = ResponseCurveType::Exponential(3.0);
        assert!(
            apply_response_curve(0.5, &curve) > 0.0,
            "positive stays positive"
        );
        assert!(
            apply_response_curve(-0.5, &curve) < 0.0,
            "negative stays negative"
        );
    }

    #[test]
    fn exponential_zero_is_zero() {
        assert_approx(
            apply_response_curve(0.0, &ResponseCurveType::Exponential(2.0)),
            0.0,
            "exp zero",
        );
    }

    #[test]
    fn exponential_clamps_output() {
        let curve = ResponseCurveType::Exponential(0.1);
        assert_approx(apply_response_curve(1.0, &curve), 1.0, "exp clamps at 1");
    }

    #[test]
    fn exponential_invalid_power_returns_clamped_input() {
        let curve = ResponseCurveType::Exponential(f32::NAN);
        // Validation fails, so the clamped input is returned unchanged.
        assert_approx(
            apply_response_curve(0.5, &curve),
            0.5,
            "invalid exp passthrough",
        );
    }

    // -------------------------------------------------------------------------
    //  apply_response_curve — SCurve
    // -------------------------------------------------------------------------

    #[test]
    fn scurve_endpoints() {
        let curve = ResponseCurveType::SCurve;
        assert_approx(apply_response_curve(0.0, &curve), 0.0, "scurve 0");
        assert_approx(apply_response_curve(1.0, &curve), 1.0, "scurve 1");
        assert_approx(apply_response_curve(-1.0, &curve), -1.0, "scurve -1");
    }

    #[test]
    fn scurve_midpoint_is_half() {
        let curve = ResponseCurveType::SCurve;
        // smoothstep at 0.5: 3*0.25 - 2*0.125 = 0.75 - 0.25 = 0.5
        assert_approx(apply_response_curve(0.5, &curve), 0.5, "scurve 0.5");
    }

    #[test]
    fn scurve_preserves_sign() {
        let curve = ResponseCurveType::SCurve;
        assert!(apply_response_curve(0.5, &curve) > 0.0, "scurve positive");
        assert!(apply_response_curve(-0.5, &curve) < 0.0, "scurve negative");
    }

    #[test]
    fn scurve_is_smoothstep() {
        let curve = ResponseCurveType::SCurve;
        let x = 0.3_f32;
        let expected = x * x * (3.0 - 2.0 * x);
        assert_approx(apply_response_curve(x, &curve), expected, "scurve formula");
    }

    // -------------------------------------------------------------------------
    //  apply_response_curve — Bezier
    // -------------------------------------------------------------------------

    #[test]
    fn bezier_linear_control_points_endpoints() {
        // Control points on the diagonal produce a monotonic curve.
        let curve = ResponseCurveType::Bezier {
            p1: [1.0 / 3.0, 1.0 / 3.0],
            p2: [2.0 / 3.0, 2.0 / 3.0],
        };
        // Endpoints are always exact.
        assert_approx(apply_response_curve(0.0, &curve), 0.0, "bezier linear 0");
        assert_approx(apply_response_curve(1.0, &curve), 1.0, "bezier linear 1");
    }

    #[test]
    fn bezier_endpoints() {
        let curve = ResponseCurveType::Bezier {
            p1: [0.3, 0.9],
            p2: [0.7, 0.1],
        };
        assert_approx(apply_response_curve(0.0, &curve), 0.0, "bezier 0");
        assert_approx(apply_response_curve(1.0, &curve), 1.0, "bezier 1");
    }

    #[test]
    fn bezier_preserves_sign() {
        let curve = ResponseCurveType::Bezier {
            p1: [0.3, 0.9],
            p2: [0.7, 0.1],
        };
        assert!(apply_response_curve(0.5, &curve) >= 0.0, "bezier positive");
        assert!(apply_response_curve(-0.5, &curve) <= 0.0, "bezier negative");
    }

    #[test]
    fn bezier_output_in_unit_range() {
        let curve = ResponseCurveType::Bezier {
            p1: [0.1, 0.95],
            p2: [0.9, 0.05],
        };
        for &v in &[0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            let out = apply_response_curve(v, &curve);
            assert!(
                (0.0..=1.0).contains(&out),
                "bezier out of range for {v}: {out}"
            );
        }
    }

    #[test]
    fn bezier_clamps_control_points_outside_unit() {
        // Control points outside [0,1] should be clamped silently.
        let curve = ResponseCurveType::Bezier {
            p1: [-1.0, 2.0],
            p2: [2.0, -1.0],
        };
        // Should not panic and should produce a value in [0,1].
        let out = apply_response_curve(0.5, &curve);
        assert!(
            (0.0..=1.0).contains(&out),
            "clamped bezier out of range: {out}"
        );
    }

    // -------------------------------------------------------------------------
    //  apply_stick_curve (per-axis)
    // -------------------------------------------------------------------------

    #[test]
    fn stick_curve_linear_is_identity_pair() {
        let curve = ResponseCurveType::Linear;
        assert_pair_approx(
            apply_stick_curve(0.5, -0.3, &curve),
            (0.5, -0.3),
            "stick linear",
        );
    }

    #[test]
    fn stick_curve_exponential_applies_per_axis() {
        let curve = ResponseCurveType::Exponential(2.0);
        let (x, y) = apply_stick_curve(0.5, -0.5, &curve);
        assert_approx(x, 0.25, "stick exp x");
        assert_approx(y, -0.25, "stick exp y");
    }

    #[test]
    fn stick_curve_clamps_inputs() {
        let curve = ResponseCurveType::Linear;
        let (x, y) = apply_stick_curve(2.0, -2.0, &curve);
        assert_approx(x, 1.0, "stick clamp x");
        assert_approx(y, -1.0, "stick clamp y");
    }

    #[test]
    fn stick_curve_zero_is_zero() {
        let curve = ResponseCurveType::Exponential(2.0);
        let (x, y) = apply_stick_curve(0.0, 0.0, &curve);
        assert_pair_approx((x, y), (0.0, 0.0), "stick zero");
    }

    // -------------------------------------------------------------------------
    //  apply_stick_curve_radial
    // -------------------------------------------------------------------------

    #[test]
    fn stick_curve_radial_zero_is_zero() {
        let curve = ResponseCurveType::Exponential(2.0);
        assert_pair_approx(
            apply_stick_curve_radial(0.0, 0.0, &curve),
            (0.0, 0.0),
            "radial zero",
        );
    }

    #[test]
    fn stick_curve_radial_preserves_direction() {
        let curve = ResponseCurveType::Exponential(2.0);
        let (x, y) = apply_stick_curve_radial(0.6, 0.8, &curve);
        // Original magnitude is 1.0; after exp(2.0) it's still 1.0.
        let m = (x * x + y * y).sqrt();
        assert_approx(m, 1.0, "radial preserves unit magnitude");
        // Direction preserved: ratio x:y should be 0.6:0.8 = 3:4
        assert_approx(x / y, 0.6 / 0.8, "radial direction preserved");
    }

    #[test]
    fn stick_curve_radial_scales_magnitude() {
        let curve = ResponseCurveType::Exponential(2.0);
        let (x, y) = apply_stick_curve_radial(0.3, 0.4, &curve);
        // Original magnitude = 0.5; after exp(2.0) = 0.25.
        let m = (x * x + y * y).sqrt();
        assert_approx(m, 0.25, "radial scales magnitude");
    }

    #[test]
    fn stick_curve_radial_clamps_inputs() {
        let curve = ResponseCurveType::Linear;
        let (x, y) = apply_stick_curve_radial(2.0, 2.0, &curve);
        // Clamped to (1,1), magnitude sqrt(2) > 1.0, so apply_response_curve
        // clamps the magnitude to 1.0. The vector is then scaled to unit length.
        let m = (x * x + y * y).sqrt();
        assert_approx(m, 1.0, "radial clamps magnitude to 1");
        // Direction preserved: x == y after rescaling.
        assert_approx(x, y, "radial clamped direction");
    }

    #[test]
    fn stick_curve_radial_linear_preserves_vector() {
        let curve = ResponseCurveType::Linear;
        let (x, y) = apply_stick_curve_radial(0.3, 0.4, &curve);
        assert_pair_approx((x, y), (0.3, 0.4), "radial linear identity");
    }

    // -------------------------------------------------------------------------
    //  Bézier helper functions
    // -------------------------------------------------------------------------

    #[test]
    fn bezier_x_endpoints() {
        assert_approx(bezier_x(0.0, 0.3, 0.7), 0.0, "bezier_x(0)");
        assert_approx(bezier_x(1.0, 0.3, 0.7), 1.0, "bezier_x(1)");
    }

    #[test]
    fn bezier_y_endpoints() {
        assert_approx(bezier_y(0.0, 0.9, 0.1), 0.0, "bezier_y(0)");
        assert_approx(bezier_y(1.0, 0.9, 0.1), 1.0, "bezier_y(1)");
    }

    #[test]
    fn bezier_x_midpoint() {
        // At t=0.5 with p1x=1/3, p2x=2/3 (linear), x should be 0.5.
        let x = bezier_x(0.5, 1.0 / 3.0, 2.0 / 3.0);
        assert_approx(x, 0.5, "bezier_x linear midpoint");
    }

    #[test]
    fn bezier_y_midpoint() {
        let y = bezier_y(0.5, 1.0 / 3.0, 2.0 / 3.0);
        assert_approx(y, 0.5, "bezier_y linear midpoint");
    }

    #[test]
    fn bezier_dx_positive_for_monotonic_curve() {
        // For p1x, p2x in [0,1], the derivative should be non-negative.
        let dx = bezier_dx(0.5, 0.3, 0.7);
        assert!(dx >= 0.0, "bezier_dx non-negative: {dx}");
    }

    #[test]
    fn bezier_dx_at_endpoints() {
        // dx(0) = 3*p1x, dx(1) = 3*(1-p2x)
        assert_approx(bezier_dx(0.0, 0.3, 0.7), 0.9, "bezier_dx(0)");
        assert_approx(bezier_dx(1.0, 0.3, 0.7), 0.9, "bezier_dx(1)");
    }

    #[test]
    fn cubic_bezier_y_for_x_linear_endpoints() {
        let p1 = [1.0 / 3.0, 1.0 / 3.0];
        let p2 = [2.0 / 3.0, 2.0 / 3.0];
        // Endpoints are always exact.
        assert_approx(cubic_bezier_y_for_x(0.0, &p1, &p2), 0.0, "bezier_y 0");
        assert_approx(cubic_bezier_y_for_x(1.0, &p1, &p2), 1.0, "bezier_y 1");
    }

    #[test]
    fn cubic_bezier_y_for_x_endpoints() {
        let p1 = [0.3, 0.9];
        let p2 = [0.7, 0.1];
        assert_approx(cubic_bezier_y_for_x(0.0, &p1, &p2), 0.0, "bezier_y 0");
        assert_approx(cubic_bezier_y_for_x(1.0, &p1, &p2), 1.0, "bezier_y 1");
    }

    #[test]
    fn cubic_bezier_y_for_x_in_range() {
        let p1 = [0.3, 0.9];
        let p2 = [0.7, 0.1];
        for &x in &[0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            let y = cubic_bezier_y_for_x(x, &p1, &p2);
            assert!(
                (0.0..=1.0).contains(&y),
                "bezier_y out of range for {x}: {y}"
            );
        }
    }

    #[test]
    fn cubic_bezier_y_for_x_clamps_target() {
        let p1 = [0.3, 0.9];
        let p2 = [0.7, 0.1];
        // target_x outside [0,1] is clamped.
        assert_approx(
            cubic_bezier_y_for_x(-1.0, &p1, &p2),
            0.0,
            "bezier_y clamp low",
        );
        assert_approx(
            cubic_bezier_y_for_x(2.0, &p1, &p2),
            1.0,
            "bezier_y clamp high",
        );
    }

    // -------------------------------------------------------------------------
    //  zone_action
    // -------------------------------------------------------------------------

    fn zones_with_actions() -> StickZones {
        StickZones {
            deadzone: 0.1,
            low: 0.3,
            medium: 0.6,
            high: 0.9,
            low_actions: vec![Action::Button(ButtonId::A)],
            medium_actions: vec![Action::Button(ButtonId::B), Action::Button(ButtonId::X)],
            high_actions: vec![Action::ProfileNext],
        }
    }

    #[test]
    fn zone_action_deadzone_returns_empty() {
        let zones = zones_with_actions();
        assert_eq!(zone_action(0.0, &zones), Vec::new());
        assert_eq!(zone_action(0.1, &zones), Vec::new()); // equal to deadzone
    }

    #[test]
    fn zone_action_low_zone() {
        let zones = zones_with_actions();
        assert_eq!(zone_action(0.11, &zones), vec![Action::Button(ButtonId::A)]);
        assert_eq!(zone_action(0.3, &zones), vec![Action::Button(ButtonId::A)]);
        // equal to low
    }

    #[test]
    fn zone_action_medium_zone() {
        let zones = zones_with_actions();
        assert_eq!(
            zone_action(0.31, &zones),
            vec![Action::Button(ButtonId::B), Action::Button(ButtonId::X)]
        );
        assert_eq!(
            zone_action(0.6, &zones),
            vec![Action::Button(ButtonId::B), Action::Button(ButtonId::X)]
        );
    }

    #[test]
    fn zone_action_high_zone() {
        let zones = zones_with_actions();
        assert_eq!(zone_action(0.61, &zones), vec![Action::ProfileNext]);
        assert_eq!(zone_action(1.0, &zones), vec![Action::ProfileNext]);
    }

    #[test]
    fn zone_action_default_deadzone_is_zero() {
        let zones = StickZones::default();
        // default deadzone is 0.0, so magnitude 0.0 is <= deadzone -> empty
        assert_eq!(zone_action(0.0, &zones), Vec::new());
    }

    // -------------------------------------------------------------------------
    //  apply_deadzone_shape — radial
    // -------------------------------------------------------------------------

    #[test]
    fn radial_deadzone_below_threshold_is_zero() {
        let (x, y) = apply_deadzone_shape(0.05, 0.05, 0.1, "radial");
        assert_pair_approx((x, y), (0.0, 0.0), "radial below threshold");
    }

    #[test]
    fn radial_deadzone_at_threshold_is_zero() {
        let (x, y) = apply_deadzone_shape(0.1, 0.0, 0.1, "radial");
        assert_pair_approx((x, y), (0.0, 0.0), "radial at threshold");
    }

    #[test]
    fn radial_deadzone_above_threshold_rescales() {
        let (x, y) = apply_deadzone_shape(1.0, 0.0, 0.1, "radial");
        // m=1, scale = (1-0.1)/(1-0.1) = 1.0
        assert_approx(x, 1.0, "radial full scale x");
        assert_approx(y, 0.0, "radial full scale y");
    }

    #[test]
    fn radial_deadzone_preserves_direction() {
        let (x, y) = apply_deadzone_shape(0.6, 0.8, 0.1, "radial");
        // direction ratio preserved
        assert_approx(x / y, 0.6 / 0.8, "radial direction");
    }

    #[test]
    fn radial_deadzone_zero_input_is_zero() {
        let (x, y) = apply_deadzone_shape(0.0, 0.0, 0.1, "radial");
        assert_pair_approx((x, y), (0.0, 0.0), "radial zero input");
    }

    // -------------------------------------------------------------------------
    //  apply_deadzone_shape — axial
    // -------------------------------------------------------------------------

    #[test]
    fn axial_deadzone_below_threshold_is_zero() {
        let (x, y) = apply_deadzone_shape(0.05, 0.05, 0.1, "axial");
        assert_pair_approx((x, y), (0.0, 0.0), "axial below threshold");
    }

    #[test]
    fn axial_deadzone_above_threshold_rescales() {
        let (x, y) = apply_deadzone_shape(1.0, 0.05, 0.1, "axial");
        assert_approx(x, 1.0, "axial x rescaled");
        assert_approx(y, 0.0, "axial y zeroed");
    }

    #[test]
    fn axial_deadzone_preserves_sign() {
        let (x, y) = apply_deadzone_shape(-0.5, -0.5, 0.1, "axial");
        assert!(x < 0.0, "axial x negative");
        assert!(y < 0.0, "axial y negative");
    }

    #[test]
    fn axial_deadzone_independent_axes() {
        // x above threshold, y below
        let (x, y) = apply_deadzone_shape(0.5, 0.05, 0.1, "axial");
        assert!(x > 0.0, "axial x nonzero");
        assert_approx(y, 0.0, "axial y zero");
    }

    // -------------------------------------------------------------------------
    //  apply_deadzone_shape — elliptic
    // -------------------------------------------------------------------------

    #[test]
    fn elliptic_deadzone_inside_ellipse_is_zero() {
        // At the center, inside the ellipse.
        let (x, y) = apply_deadzone_shape(0.01, 0.01, 0.1, "elliptic");
        assert_pair_approx((x, y), (0.0, 0.0), "elliptic inside");
    }

    #[test]
    fn elliptic_deadzone_zero_deadzone_is_zero() {
        let (x, y) = apply_deadzone_shape(0.5, 0.5, 0.0, "elliptic");
        assert_pair_approx((x, y), (0.0, 0.0), "elliptic zero deadzone");
    }

    #[test]
    fn elliptic_deadzone_zero_input_is_zero() {
        let (x, y) = apply_deadzone_shape(0.0, 0.0, 0.1, "elliptic");
        assert_pair_approx((x, y), (0.0, 0.0), "elliptic zero input");
    }

    #[test]
    fn elliptic_deadzone_outside_ellipse_is_nonzero() {
        let (x, y) = apply_deadzone_shape(1.0, 1.0, 0.1, "elliptic");
        // Should produce a non-zero rescaled vector.
        let m = (x * x + y * y).sqrt();
        assert!(m > 0.0, "elliptic outside nonzero: m={m}");
    }

    // -------------------------------------------------------------------------
    //  apply_deadzone_shape — fallback / case-insensitivity
    // -------------------------------------------------------------------------

    #[test]
    fn deadzone_shape_case_insensitive() {
        let upper = apply_deadzone_shape(0.5, 0.0, 0.1, "RADIAL");
        let lower = apply_deadzone_shape(0.5, 0.0, 0.1, "radial");
        assert_pair_approx(upper, lower, "case insensitive radial");
    }

    #[test]
    fn deadzone_shape_unknown_falls_back_to_radial() {
        let unknown = apply_deadzone_shape(0.5, 0.0, 0.1, "unknown_shape");
        let radial = apply_deadzone_shape(0.5, 0.0, 0.1, "radial");
        assert_pair_approx(unknown, radial, "unknown falls back to radial");
    }

    #[test]
    fn deadzone_shape_empty_string_falls_back_to_radial() {
        let empty = apply_deadzone_shape(0.5, 0.0, 0.1, "");
        let radial = apply_deadzone_shape(0.5, 0.0, 0.1, "radial");
        assert_pair_approx(empty, radial, "empty falls back to radial");
    }

    // -------------------------------------------------------------------------
    //  apply_deadzone_shape — all shapes with zero deadzone
    // -------------------------------------------------------------------------

    #[test]
    fn radial_deadzone_zero_deadzone_full_input() {
        // With deadzone=0, radial rescales (m-0)/(1-0) = m, so identity for unit vector.
        let (x, y) = apply_deadzone_shape(1.0, 0.0, 0.0, "radial");
        assert_approx(x, 1.0, "radial zero dz x");
        assert_approx(y, 0.0, "radial zero dz y");
    }

    #[test]
    fn axial_deadzone_zero_deadzone_is_identity() {
        let (x, y) = apply_deadzone_shape(0.5, -0.3, 0.0, "axial");
        assert_pair_approx((x, y), (0.5, -0.3), "axial zero dz identity");
    }
}
