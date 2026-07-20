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
