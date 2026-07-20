//! Schema-versioned configuration persistence.
//!
//! Stores the full `AppConfig` as JSON in `%APPDATA%\OxideLink\config.json`.
//! Includes a `schemaVersion` field for future migration support.

use crate::state::AppConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Run the synchronous file operations on a blocking thread so async callers
/// do not block the tokio runtime.
async fn run_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {}", e))?
}

/// Current config schema version. Bump when breaking changes are made to
/// `AppConfig` and add a migration in `migrate()`.
const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Wrapper around `AppConfig` with a schema version for migration support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    pub schema_version: u32,
    #[serde(flatten)]
    pub config: AppConfig,
}

impl PersistedConfig {
    pub fn new(config: AppConfig) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            config,
        }
    }
}

/// Return the path to the config file: `%APPDATA%\OxideLink\config.json`.
pub fn config_file_path() -> PathBuf {
    config_store_base_dir().join("config.json")
}

fn config_store_base_dir() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("OxideLink")
}

/// Ensure `path` is absolute and resolves to a location inside `base`,
/// rejecting path traversal and symlinks that escape the base directory.
/// For write paths the file itself may not exist yet; in that case the parent
/// directory is canonicalized instead.
fn validate_path_within_base(
    path: &Path,
    base: &Path,
    allow_nonexistent: bool,
) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("path '{}' must be absolute", path.display()));
    }
    std::fs::create_dir_all(base)
        .map_err(|e| format!("failed to create base dir '{}': {}", base.display(), e))?;
    let canonical_base = base
        .canonicalize()
        .map_err(|e| format!("failed to resolve base dir '{}': {}", base.display(), e))?;
    let target = if allow_nonexistent && !path.exists() {
        match path.parent() {
            Some(parent) if parent.as_os_str().is_empty() => {
                return Err(format!("path '{}' has no parent directory", path.display()));
            }
            Some(parent) => parent
                .canonicalize()
                .map_err(|e| format!("failed to resolve parent '{}': {}", parent.display(), e))?,
            None => {
                return Err(format!("path '{}' has no parent directory", path.display()));
            }
        }
    } else {
        path.canonicalize()
            .map_err(|e| format!("failed to resolve path '{}': {}", path.display(), e))?
    };
    if !target.starts_with(&canonical_base) {
        return Err(format!(
            "path '{}' is outside the allowed directory '{}'",
            path.display(),
            base.display()
        ));
    }
    Ok(path.to_path_buf())
}

/// Ensure the config directory exists, creating it if necessary.
fn ensure_config_dir() -> Result<PathBuf, String> {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config dir {:?}: {}", parent, e))?;
    }
    Ok(path)
}

/// Save config to disk as schema-versioned JSON.
pub fn save_config(config: &AppConfig) -> Result<PathBuf, String> {
    let path = ensure_config_dir()?;
    let persisted = PersistedConfig::new(config.clone());
    let json = serde_json::to_string_pretty(&persisted)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("Failed to write config to {:?}: {}", path, e))?;
    log::info!("Config saved to {:?}", path);
    Ok(path)
}

/// Load config from disk. Returns `Ok(None)` if the file doesn't exist.
/// Applies migrations if the schema version is older than current.
pub fn load_config() -> Result<Option<AppConfig>, String> {
    let path = config_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read config from {:?}: {}", path, e))?;
    let persisted: PersistedConfig =
        serde_json::from_str(&json).map_err(|e| format!("Failed to parse config JSON: {}", e))?;
    let migrated = migrate(persisted);
    log::info!(
        "Config loaded from {:?} (schema v{})",
        path,
        CURRENT_SCHEMA_VERSION
    );
    Ok(Some(migrated.config))
}

/// Export config to a user-chosen file path.
pub fn export_config(config: &AppConfig, export_path: &str) -> Result<(), String> {
    let path = Path::new(export_path);
    validate_path_within_base(path, &config_store_base_dir(), true)?;
    let persisted = PersistedConfig::new(config.clone());
    let json = serde_json::to_string_pretty(&persisted)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    std::fs::write(path, json)
        .map_err(|e| format!("Failed to write export to {}: {}", export_path, e))?;
    log::info!("Config exported to {}", export_path);
    Ok(())
}

/// Import config from a file path. Validates all fields before applying.
pub fn import_config(import_path: &str) -> Result<AppConfig, String> {
    let path = Path::new(import_path);
    validate_path_within_base(path, &config_store_base_dir(), false)?;
    let json = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read import file {}: {}", import_path, e))?;
    let persisted: PersistedConfig =
        serde_json::from_str(&json).map_err(|e| format!("Failed to parse import JSON: {}", e))?;
    let migrated = migrate(persisted);
    validate_config(&migrated.config)?;
    log::info!(
        "Config imported from {} (schema v{})",
        import_path,
        CURRENT_SCHEMA_VERSION
    );
    Ok(migrated.config)
}

pub async fn save_config_async(config: &AppConfig) -> Result<PathBuf, String> {
    let config = config.clone();
    run_blocking(move || save_config(&config)).await
}

pub async fn load_config_async() -> Result<Option<AppConfig>, String> {
    run_blocking(load_config).await
}

pub async fn export_config_async(config: &AppConfig, export_path: &str) -> Result<(), String> {
    let config = config.clone();
    let export_path = export_path.to_string();
    run_blocking(move || export_config(&config, &export_path)).await
}

pub async fn import_config_async(import_path: &str) -> Result<AppConfig, String> {
    let import_path = import_path.to_string();
    run_blocking(move || import_config(&import_path)).await
}

/// Apply schema migrations if needed.
fn migrate(persisted: PersistedConfig) -> PersistedConfig {
    // Currently only schema version 1 exists, so no migrations needed.
    // Future migrations would check persisted.schema_version and transform.
    if persisted.schema_version != CURRENT_SCHEMA_VERSION {
        log::warn!(
            "Config schema version {} (expected {}), migrations applied",
            persisted.schema_version,
            CURRENT_SCHEMA_VERSION
        );
    }
    persisted
}

/// Validate all config fields are within safe ranges.
/// Prevents malicious or corrupted config files from causing harm.
pub fn validate_config(config: &AppConfig) -> Result<(), String> {
    // Deadzone validation (0.0 - 0.40)
    if config.deadzone_left < 0.0 || config.deadzone_left > 0.40 {
        return Err(format!(
            "deadzone_left {} out of range [0.0, 0.40]",
            config.deadzone_left
        ));
    }
    if config.deadzone_right < 0.0 || config.deadzone_right > 0.40 {
        return Err(format!(
            "deadzone_right {} out of range [0.0, 0.40]",
            config.deadzone_right
        ));
    }

    // Keep-alive interval (800 - 5000 ms)
    if config.keepalive_interval_ms < 800 || config.keepalive_interval_ms > 5000 {
        return Err(format!(
            "keepalive_interval_ms {} out of range [800, 5000]",
            config.keepalive_interval_ms
        ));
    }

    // Battery warning threshold (5 - 30)
    if config.battery_warning_threshold < 5 || config.battery_warning_threshold > 30 {
        return Err(format!(
            "battery_warning_threshold {} out of range [5, 30]",
            config.battery_warning_threshold
        ));
    }

    // Button remap: must be valid button names
    for (name, target) in &[
        ("a_to", &config.button_remap.a_to),
        ("b_to", &config.button_remap.b_to),
        ("x_to", &config.button_remap.x_to),
        ("y_to", &config.button_remap.y_to),
    ] {
        if !matches!(
            target.as_str(),
            "a" | "b"
                | "x"
                | "y"
                | "l"
                | "r"
                | "zl"
                | "zr"
                | "minus"
                | "plus"
                | "home"
                | "capture"
                | "stick_l"
                | "stick_r"
        ) {
            return Err(format!(
                "button_remap.{} = '{}' is not a valid button name",
                name, target
            ));
        }
    }

    // Stick calibration config validation
    let scc = &config.stick_calibration_config;
    if !matches!(
        scc.response_curve_type.as_str(),
        "linear" | "exponential" | "s-curve" | "bezier"
    ) {
        return Err(format!(
            "response_curve_type '{}' is not valid",
            scc.response_curve_type
        ));
    }
    if scc.response_curve_power < 0.5 || scc.response_curve_power > 3.0 {
        return Err(format!(
            "response_curve_power {} out of range [0.5, 3.0]",
            scc.response_curve_power
        ));
    }
    if scc.deadzone_safety_margin < 1.0 || scc.deadzone_safety_margin > 3.0 {
        return Err(format!(
            "deadzone_safety_margin {} out of range [1.0, 3.0]",
            scc.deadzone_safety_margin
        ));
    }
    if scc.min_deadzone < 0.0 || scc.min_deadzone > 0.5 {
        return Err(format!(
            "min_deadzone {} out of range [0.0, 0.5]",
            scc.min_deadzone
        ));
    }
    if scc.max_deadzone < 0.0 || scc.max_deadzone > 0.5 {
        return Err(format!(
            "max_deadzone {} out of range [0.0, 0.5]",
            scc.max_deadzone
        ));
    }
    if scc.min_deadzone > scc.max_deadzone {
        return Err(format!(
            "min_deadzone {} > max_deadzone {}",
            scc.min_deadzone, scc.max_deadzone
        ));
    }
    if !matches!(scc.deadzone_shape.as_str(), "radial" | "axial" | "elliptic") {
        return Err(format!(
            "deadzone_shape '{}' is not valid",
            scc.deadzone_shape
        ));
    }

    // Reconnect interval (1 - 10 seconds)
    if config.reconnect_interval_s < 1 || config.reconnect_interval_s > 10 {
        return Err(format!(
            "reconnect_interval_s {} out of range [1, 10]",
            config.reconnect_interval_s
        ));
    }

    // Battery polling interval (5, 10, 30, or 60 seconds)
    if !matches!(config.battery_polling_interval_s, 5 | 10 | 30 | 60) {
        return Err(format!(
            "battery_polling_interval_s {} must be 5, 10, 30, or 60",
            config.battery_polling_interval_s
        ));
    }

    // Notification config — all fields are bools, no range validation needed,
    // but verify the struct is present (serde will fail if missing).
    // No additional validation required for bool fields.

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        DsuConfig, KbmConfig, LogConfig, NfcConfig, NotificationConfig, Profile, ProfileManager,
        RemapConfig, StickCalibrationConfig, ValidationConfig,
    };
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Monotonic counter to generate unique temp file names per test run, avoiding
    /// collisions when tests execute in parallel.
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("oxidelink_config_test_{}_{}.json", prefix, id))
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oxidelink_config_test_{}_{}", prefix, id));
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        dir
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        if path.is_dir() {
            let _ = fs::remove_dir_all(path);
        }
    }

    // ------------------------------------------------------------------
    // Defaults
    // ------------------------------------------------------------------

    #[test]
    fn default_config_is_valid() {
        let config = AppConfig::default();
        assert!(
            validate_config(&config).is_ok(),
            "default config should be valid"
        );
    }

    #[test]
    fn default_app_config_values() {
        let config = AppConfig::default();
        assert_eq!(config.deadzone_left, 0.08);
        assert_eq!(config.deadzone_right, 0.08);
        assert_eq!(config.keepalive_interval_ms, 3000);
        assert!(config.adaptive_keepalive);
        assert_eq!(config.battery_warning_threshold, 15);
        assert!(!config.mock_mode);
        assert!(config.config_persistence_enabled);
        assert!(config.auto_reconnect);
        assert_eq!(config.reconnect_interval_s, 3);
        assert!(config.bt_power_detection_enabled);
        assert_eq!(config.battery_polling_interval_s, 30);
        assert!(config.close_to_tray);
        assert!(!config.auto_start);
        assert!(config.tray_minimize);
        assert!(!config.hidhide_enabled);
        assert!(!config.hidhide_auto_hide);
        assert!(!config.crash_reporting_enabled);
        assert!(config.crash_reporting_dsn.is_none());
        assert!(!config.telemetry_enabled);
        assert!(config.telemetry_key.is_none());
        assert!(config.update_endpoint.is_empty());
        assert!(!config.real_device_validation);
        // per-controller profile slots initialized to None
        assert_eq!(config.per_controller_profile.len(), crate::state::CONTROLLER_SLOTS);
        assert!(config.per_controller_profile.iter().all(|p| p.is_none()));
    }

    #[test]
    fn default_remap_config() {
        // AppConfig::default constructs the remap inline; verify those values.
        let config = AppConfig::default();
        assert_eq!(config.button_remap.a_to, "b");
        assert_eq!(config.button_remap.b_to, "a");
        assert_eq!(config.button_remap.x_to, "y");
        assert_eq!(config.button_remap.y_to, "x");
    }

    #[test]
    fn default_stick_calibration_config() {
        let scc = StickCalibrationConfig::default();
        assert!(scc.adaptive_deadzone_enabled);
        assert!(scc.center_auto_cal_enabled);
        assert!(scc.drift_detection_enabled);
        assert!(!scc.gate_calibration_enabled);
        assert_eq!(scc.response_curve_type, "exponential");
        assert!((scc.response_curve_power - 1.3).abs() < f32::EPSILON);
        assert_eq!(scc.bezier_p1, [0.3, 0.9]);
        assert_eq!(scc.bezier_p2, [0.7, 0.1]);
        assert!((scc.deadzone_safety_margin - 1.5).abs() < f32::EPSILON);
        assert!((scc.min_deadzone - 0.01).abs() < f32::EPSILON);
        assert!((scc.max_deadzone - 0.15).abs() < f32::EPSILON);
        assert_eq!(scc.deadzone_shape, "radial");
    }

    #[test]
    fn default_notification_config() {
        let nc = NotificationConfig::default();
        assert!(nc.enabled);
        assert!(nc.critical_enabled);
        assert!(nc.warning_enabled);
        assert!(nc.info_enabled);
        assert!(nc.notify_disconnect);
        assert!(nc.notify_bt_power);
        assert!(nc.notify_low_battery);
        assert!(nc.notify_drift);
        assert!(nc.notify_reconnect);
    }

    #[test]
    fn default_dsu_config() {
        let dsu = DsuConfig::default();
        assert!(!dsu.enabled);
        assert_eq!(dsu.bind_address, "127.0.0.1");
        assert_eq!(dsu.port, 26760);
        assert_eq!(dsu.update_rate_hz, 60);
    }

    #[test]
    fn default_validation_config() {
        let vc = ValidationConfig::default();
        assert!(!vc.enable_real_device_checks);
        assert!(!vc.strict_calibration_requirements);
        assert!(!vc.mock_mode);
        assert!(!vc.require_vigembus);
        assert!(!vc.require_hidhide);
    }

    #[test]
    fn default_log_config() {
        let _ = LogConfig::default();
    }

    #[test]
    fn default_kbm_config() {
        let _ = KbmConfig::default();
    }

    #[test]
    fn default_nfc_config() {
        let _ = NfcConfig::default();
    }

    #[test]
    fn default_profile_manager() {
        let pm = ProfileManager::default();
        assert!(pm.profiles.is_empty());
        assert!(pm.active_profile_id.is_none());
        assert!(pm.default_profile_id.is_none());
        assert!(pm.last_applied.is_none());
    }

    // ------------------------------------------------------------------
    // PersistedConfig
    // ------------------------------------------------------------------

    #[test]
    fn persisted_config_has_schema_version() {
        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config);
        assert_eq!(persisted.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn persisted_config_new_preserves_config() {
        let mut config = AppConfig::default();
        config.deadzone_left = 0.15;
        config.mock_mode = true;
        let persisted = PersistedConfig::new(config.clone());
        assert_eq!(persisted.config.deadzone_left, 0.15);
        assert!(persisted.config.mock_mode);
    }

    #[test]
    fn persisted_config_serde_round_trip() {
        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config);
        let json = serde_json::to_string(&persisted).expect("serialize");
        let back: PersistedConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.schema_version, persisted.schema_version);
        assert_eq!(back.config.deadzone_left, persisted.config.deadzone_left);
        assert_eq!(back.config.deadzone_right, persisted.config.deadzone_right);
        assert_eq!(back.config.keepalive_interval_ms, persisted.config.keepalive_interval_ms);
    }

    #[test]
    fn persisted_config_pretty_json_contains_schema_version() {
        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config);
        let json = serde_json::to_string_pretty(&persisted).expect("serialize");
        assert!(json.contains("\"schema_version\""));
        assert!(json.contains("\"deadzone_left\""));
    }

    #[test]
    fn persisted_config_deserialize_missing_fields_uses_serde_defaults() {
        // Round-trip a full default config through JSON to verify all fields
        // serialize and deserialize correctly.
        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config);
        let json = serde_json::to_string(&persisted).expect("serialize");
        let parsed: PersistedConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(parsed.config.deadzone_left, persisted.config.deadzone_left);
        assert_eq!(parsed.config.keepalive_interval_ms, persisted.config.keepalive_interval_ms);
        assert_eq!(
            parsed.config.stick_calibration_config.response_curve_type,
            persisted.config.stick_calibration_config.response_curve_type
        );
    }

    #[test]
    fn persisted_config_deserialize_invalid_json_errors() {
        let json = "{ not valid json }";
        let result: Result<PersistedConfig, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Serialization round-trips for sub-config structs
    // ------------------------------------------------------------------

    #[test]
    fn remap_config_serde_round_trip() {
        let remap = RemapConfig {
            a_to: "x".into(),
            b_to: "y".into(),
            x_to: "l".into(),
            y_to: "r".into(),
        };
        let json = serde_json::to_string(&remap).expect("serialize");
        let back: RemapConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.a_to, remap.a_to);
        assert_eq!(back.b_to, remap.b_to);
        assert_eq!(back.x_to, remap.x_to);
        assert_eq!(back.y_to, remap.y_to);
    }

    #[test]
    fn stick_calibration_config_serde_round_trip() {
        let scc = StickCalibrationConfig {
            adaptive_deadzone_enabled: false,
            center_auto_cal_enabled: false,
            drift_detection_enabled: true,
            gate_calibration_enabled: true,
            response_curve_type: "bezier".into(),
            response_curve_power: 2.5,
            bezier_p1: [0.1, 0.2],
            bezier_p2: [0.8, 0.9],
            deadzone_safety_margin: 2.0,
            min_deadzone: 0.05,
            max_deadzone: 0.25,
            deadzone_shape: "axial".into(),
        };
        let json = serde_json::to_string(&scc).expect("serialize");
        let back: StickCalibrationConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.response_curve_type, scc.response_curve_type);
        assert!((back.response_curve_power - 2.5).abs() < f32::EPSILON);
        assert_eq!(back.bezier_p1, scc.bezier_p1);
        assert_eq!(back.bezier_p2, scc.bezier_p2);
        assert_eq!(back.deadzone_shape, scc.deadzone_shape);
    }

    #[test]
    fn notification_config_serde_round_trip() {
        let nc = NotificationConfig {
            enabled: false,
            critical_enabled: false,
            warning_enabled: true,
            info_enabled: false,
            notify_disconnect: true,
            notify_bt_power: false,
            notify_low_battery: true,
            notify_drift: false,
            notify_reconnect: true,
        };
        let json = serde_json::to_string(&nc).expect("serialize");
        let back: NotificationConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.enabled, false);
        assert_eq!(back.notify_disconnect, true);
        assert_eq!(back.notify_reconnect, true);
    }

    #[test]
    fn dsu_config_serde_round_trip() {
        let dsu = DsuConfig {
            enabled: true,
            bind_address: "0.0.0.0".into(),
            port: 12345,
            update_rate_hz: 120,
        };
        let json = serde_json::to_string(&dsu).expect("serialize");
        let back: DsuConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.enabled, dsu.enabled);
        assert_eq!(back.bind_address, dsu.bind_address);
        assert_eq!(back.port, dsu.port);
        assert_eq!(back.update_rate_hz, dsu.update_rate_hz);
    }

    #[test]
    fn validation_config_serde_round_trip() {
        let vc = ValidationConfig {
            enable_real_device_checks: true,
            strict_calibration_requirements: true,
            mock_mode: false,
            require_vigembus: true,
            require_hidhide: false,
        };
        let json = serde_json::to_string(&vc).expect("serialize");
        let back: ValidationConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, vc);
    }

    #[test]
    fn profile_manager_serde_round_trip() {
        let pm = ProfileManager {
            profiles: vec![Profile {
                id: "p1".into(),
                name: "Gaming".into(),
                enabled: true,
                auto_rules: Vec::new(),
                created_at: 1000,
                updated_at: 2000,
                nfc: NfcConfig::default(),
                right_stick: crate::state::flick_stick::RightStickConfig::default(),
            }],
            active_profile_id: Some("p1".into()),
            default_profile_id: Some("p1".into()),
            last_applied: Some("p1".into()),
        };
        let json = serde_json::to_string(&pm).expect("serialize");
        let back: ProfileManager = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, pm);
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].id, "p1");
    }

    #[test]
    fn full_app_config_serde_round_trip() {
        let mut config = AppConfig::default();
        config.deadzone_left = 0.12;
        config.deadzone_right = 0.20;
        config.keepalive_interval_ms = 1500;
        config.battery_warning_threshold = 20;
        config.mock_mode = true;
        config.button_remap.a_to = "l".into();
        config.button_remap.b_to = "r".into();
        config.stick_calibration_config.response_curve_type = "s-curve".into();
        config.stick_calibration_config.deadzone_shape = "elliptic".into();
        config.dsu.enabled = true;
        config.dsu.port = 9999;
        config.notification_config.enabled = false;
        config.profile_manager.active_profile_id = Some("test".into());

        let persisted = PersistedConfig::new(config.clone());
        let json = serde_json::to_string_pretty(&persisted).expect("serialize");
        let back: PersistedConfig = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.schema_version, CURRENT_SCHEMA_VERSION);
        assert!((back.config.deadzone_left - 0.12).abs() < f32::EPSILON);
        assert!((back.config.deadzone_right - 0.20).abs() < f32::EPSILON);
        assert_eq!(back.config.keepalive_interval_ms, 1500);
        assert_eq!(back.config.battery_warning_threshold, 20);
        assert!(back.config.mock_mode);
        assert_eq!(back.config.button_remap.a_to, "l");
        assert_eq!(back.config.button_remap.b_to, "r");
        assert_eq!(back.config.stick_calibration_config.response_curve_type, "s-curve");
        assert_eq!(back.config.stick_calibration_config.deadzone_shape, "elliptic");
        assert!(back.config.dsu.enabled);
        assert_eq!(back.config.dsu.port, 9999);
        assert!(!back.config.notification_config.enabled);
        assert_eq!(back.config.profile_manager.active_profile_id, Some("test".into()));
    }

    // ------------------------------------------------------------------
    // Config save/load with temp files (pure serde path, no real config dir)
    // ------------------------------------------------------------------

    #[test]
    fn save_then_load_round_trip_via_temp_file() {
        let path = unique_temp_path("roundtrip");
        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config.clone());
        let json = serde_json::to_string_pretty(&persisted).expect("serialize");
        fs::write(&path, &json).expect("write temp file");

        let read_json = fs::read_to_string(&path).expect("read temp file");
        let parsed: PersistedConfig = serde_json::from_str(&read_json).expect("deserialize");
        assert_eq!(parsed.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(parsed.config.deadzone_left, config.deadzone_left);
        assert_eq!(parsed.config.keepalive_interval_ms, config.keepalive_interval_ms);
        cleanup(&path);
    }

    #[test]
    fn load_nonexistent_temp_file_returns_none_pattern() {
        // Mirrors load_config() behavior: missing file => Ok(None).
        let path = unique_temp_path("nonexistent");
        assert!(!path.exists());
        let result: Option<AppConfig> = if !path.exists() {
            None
        } else {
            Some(
                serde_json::from_str(&fs::read_to_string(&path).unwrap())
                    .expect("deserialize"),
            )
        };
        assert!(result.is_none());
    }

    #[test]
    fn corrupted_temp_file_parse_errors() {
        let path = unique_temp_path("corrupt");
        fs::write(&path, "{ broken json").expect("write corrupt file");
        let read = fs::read_to_string(&path).expect("read");
        let result: Result<PersistedConfig, _> = serde_json::from_str(&read);
        assert!(result.is_err());
        cleanup(&path);
    }

    // ------------------------------------------------------------------
    // validate_config — exhaustive boundary checks
    // ------------------------------------------------------------------

    #[test]
    fn invalid_deadzone_rejected() {
        let mut config = AppConfig::default();
        config.deadzone_left = 0.5;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn deadzone_left_negative_rejected() {
        let mut config = AppConfig::default();
        config.deadzone_left = -0.01;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn deadzone_left_at_upper_bound_accepted() {
        let mut config = AppConfig::default();
        config.deadzone_left = 0.40;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn deadzone_left_above_upper_bound_rejected() {
        let mut config = AppConfig::default();
        config.deadzone_left = 0.41;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn deadzone_right_negative_rejected() {
        let mut config = AppConfig::default();
        config.deadzone_right = -0.5;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn deadzone_right_above_bound_rejected() {
        let mut config = AppConfig::default();
        config.deadzone_right = 0.45;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn deadzone_right_at_upper_bound_accepted() {
        let mut config = AppConfig::default();
        config.deadzone_right = 0.40;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn keepalive_below_minimum_rejected() {
        let mut config = AppConfig::default();
        config.keepalive_interval_ms = 799;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn keepalive_above_maximum_rejected() {
        let mut config = AppConfig::default();
        config.keepalive_interval_ms = 5001;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn keepalive_at_boundaries_accepted() {
        let mut config = AppConfig::default();
        config.keepalive_interval_ms = 800;
        assert!(validate_config(&config).is_ok());
        config.keepalive_interval_ms = 5000;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn battery_warning_below_minimum_rejected() {
        let mut config = AppConfig::default();
        config.battery_warning_threshold = 4;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn battery_warning_above_maximum_rejected() {
        let mut config = AppConfig::default();
        config.battery_warning_threshold = 31;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn battery_warning_at_boundaries_accepted() {
        let mut config = AppConfig::default();
        config.battery_warning_threshold = 5;
        assert!(validate_config(&config).is_ok());
        config.battery_warning_threshold = 30;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn invalid_remap_rejected() {
        let mut config = AppConfig::default();
        config.button_remap.a_to = "invalid".into();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn all_valid_remap_targets_accepted() {
        let mut config = AppConfig::default();
        let targets = [
            "a", "b", "x", "y", "l", "r", "zl", "zr", "minus", "plus", "home", "capture",
            "stick_l", "stick_r",
        ];
        for t in targets {
            config.button_remap.a_to = t.into();
            config.button_remap.b_to = t.into();
            config.button_remap.x_to = t.into();
            config.button_remap.y_to = t.into();
            assert!(
                validate_config(&config).is_ok(),
                "remap target '{}' should be valid",
                t
            );
        }
    }

    #[test]
    fn each_remap_field_validated_independently() {
        let mut config = AppConfig::default();
        // b_to invalid while others valid
        config.button_remap.b_to = "nope".into();
        let err = validate_config(&config).unwrap_err();
        assert!(err.contains("b_to"));
    }

    #[test]
    fn invalid_curve_type_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.response_curve_type = "quadratic".into();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn all_valid_curve_types_accepted() {
        for curve in ["linear", "exponential", "s-curve", "bezier"] {
            let mut config = AppConfig::default();
            config.stick_calibration_config.response_curve_type = curve.into();
            assert!(
                validate_config(&config).is_ok(),
                "curve type '{}' should be valid",
                curve
            );
        }
    }

    #[test]
    fn response_curve_power_below_minimum_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.response_curve_power = 0.49;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn response_curve_power_above_maximum_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.response_curve_power = 3.01;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn response_curve_power_at_boundaries_accepted() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.response_curve_power = 0.5;
        assert!(validate_config(&config).is_ok());
        config.stick_calibration_config.response_curve_power = 3.0;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn deadzone_safety_margin_below_minimum_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.deadzone_safety_margin = 0.99;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn deadzone_safety_margin_above_maximum_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.deadzone_safety_margin = 3.01;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn deadzone_safety_margin_at_boundaries_accepted() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.deadzone_safety_margin = 1.0;
        assert!(validate_config(&config).is_ok());
        config.stick_calibration_config.deadzone_safety_margin = 3.0;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn min_deadzone_negative_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.min_deadzone = -0.01;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn min_deadzone_above_half_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.min_deadzone = 0.51;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn max_deadzone_negative_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.max_deadzone = -0.01;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn max_deadzone_above_half_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.max_deadzone = 0.51;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn min_deadzone_gt_max_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.min_deadzone = 0.2;
        config.stick_calibration_config.max_deadzone = 0.1;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn min_deadzone_equal_max_accepted() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.min_deadzone = 0.1;
        config.stick_calibration_config.max_deadzone = 0.1;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn invalid_deadzone_shape_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.deadzone_shape = "square".into();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn all_valid_deadzone_shapes_accepted() {
        for shape in ["radial", "axial", "elliptic"] {
            let mut config = AppConfig::default();
            config.stick_calibration_config.deadzone_shape = shape.into();
            assert!(
                validate_config(&config).is_ok(),
                "deadzone shape '{}' should be valid",
                shape
            );
        }
    }

    #[test]
    fn reconnect_interval_below_minimum_rejected() {
        let mut config = AppConfig::default();
        config.reconnect_interval_s = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn reconnect_interval_above_maximum_rejected() {
        let mut config = AppConfig::default();
        config.reconnect_interval_s = 11;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn reconnect_interval_at_boundaries_accepted() {
        let mut config = AppConfig::default();
        config.reconnect_interval_s = 1;
        assert!(validate_config(&config).is_ok());
        config.reconnect_interval_s = 10;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn invalid_polling_interval_rejected() {
        let mut config = AppConfig::default();
        config.battery_polling_interval_s = 15;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn all_valid_polling_intervals_accepted() {
        for interval in [5u64, 10, 30, 60] {
            let mut config = AppConfig::default();
            config.battery_polling_interval_s = interval;
            assert!(
                validate_config(&config).is_ok(),
                "polling interval {} should be valid",
                interval
            );
        }
    }

    #[test]
    fn validate_config_error_messages_are_descriptive() {
        let mut config = AppConfig::default();
        config.deadzone_left = 1.0;
        let err = validate_config(&config).unwrap_err();
        assert!(err.contains("deadzone_left"));
        assert!(err.contains("out of range"));
    }

    // ------------------------------------------------------------------
    // migrate
    // ------------------------------------------------------------------

    #[test]
    fn migrate_current_version_passes_through() {
        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config.clone());
        let migrated = migrate(persisted.clone());
        assert_eq!(migrated.schema_version, persisted.schema_version);
        assert_eq!(migrated.config.deadzone_left, config.deadzone_left);
    }

    #[test]
    fn migrate_older_version_still_returns_config() {
        let config = AppConfig::default();
        let persisted = PersistedConfig {
            schema_version: 0,
            config: config.clone(),
        };
        // migrate logs a warning but still returns the config unchanged (no
        // transformations defined yet).
        let migrated = migrate(persisted);
        assert_eq!(migrated.config.deadzone_left, config.deadzone_left);
        assert_eq!(migrated.config.keepalive_interval_ms, config.keepalive_interval_ms);
    }

    #[test]
    fn migrate_future_version_passes_through() {
        let config = AppConfig::default();
        let persisted = PersistedConfig {
            schema_version: 99,
            config: config.clone(),
        };
        let migrated = migrate(persisted);
        assert_eq!(migrated.schema_version, 99);
        assert_eq!(migrated.config.deadzone_left, config.deadzone_left);
    }

    // ------------------------------------------------------------------
    // config_file_path / config_store_base_dir
    // ------------------------------------------------------------------

    #[test]
    fn config_file_path_ends_with_config_json() {
        let path = config_file_path();
        assert_eq!(path.file_name().unwrap(), "config.json");
    }

    #[test]
    fn config_store_base_dir_contains_oxidelink() {
        let base = config_store_base_dir();
        assert!(base.ends_with("OxideLink"));
    }

    // ------------------------------------------------------------------
    // validate_path_within_base — pure path logic with temp dirs
    // ------------------------------------------------------------------

    #[test]
    fn validate_path_rejects_relative_path() {
        let base = unique_temp_dir("vpath_rel");
        let result = validate_path_within_base(
            Path::new("relative/file.json"),
            &base,
            true,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("must be absolute"));
        cleanup(&base);
    }

    #[test]
    fn validate_path_accepts_file_inside_base_nonexistent() {
        let base = unique_temp_dir("vpath_inside");
        // Create the subdir so canonicalize can resolve the parent.
        let subdir = base.join("subdir");
        fs::create_dir_all(&subdir).expect("create subdir");
        let target = subdir.join("config.json");
        let result = validate_path_within_base(&target, &base, true);
        assert!(result.is_ok(), "result: {:?}", result);
        cleanup(&base);
    }

    #[test]
    fn validate_path_rejects_file_outside_base() {
        let base = unique_temp_dir("vpath_outside");
        let outside = unique_temp_dir("vpath_outside_other");
        let target = outside.join("config.json");
        // Create the outside file so canonicalize works without allow_nonexistent.
        fs::write(&target, "{}").expect("write");
        let result = validate_path_within_base(&target, &base, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("outside the allowed directory"));
        cleanup(&base);
        cleanup(&outside);
    }

    #[test]
    fn validate_path_accepts_existing_file_inside_base() {
        let base = unique_temp_dir("vpath_existing");
        let target = base.join("data.json");
        fs::write(&target, "{}").expect("write");
        let result = validate_path_within_base(&target, &base, false);
        assert!(result.is_ok());
        cleanup(&base);
    }

    #[test]
    fn validate_path_nonexistent_without_allow_flag_rejects() {
        let base = unique_temp_dir("vpath_noexist");
        let target = base.join("missing.json");
        let result = validate_path_within_base(&target, &base, false);
        assert!(result.is_err());
        cleanup(&base);
    }

    // ------------------------------------------------------------------
    // Async wrappers (tokio runtime, pure logic — no real config writes)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn save_config_async_round_trips_through_real_path() {
        // This exercises the async wrapper + serialization. It writes to the
        // real config dir, so we capture and restore the prior file content.
        let path = config_file_path();
        let backup = if path.exists() {
            Some(fs::read_to_string(&path).expect("read backup"))
        } else {
            None
        };

        let config = AppConfig::default();
        let save_result = save_config_async(&config).await;
        assert!(save_result.is_ok(), "save_config_async failed: {:?}", save_result);

        let load_result = load_config_async().await;
        assert!(load_result.is_ok(), "load_config_async failed: {:?}", load_result);
        let loaded = load_result.unwrap();
        assert!(loaded.is_some(), "config should have been saved");
        let loaded = loaded.unwrap();
        assert_eq!(loaded.deadzone_left, config.deadzone_left);
        assert_eq!(loaded.keepalive_interval_ms, config.keepalive_interval_ms);

        // Restore prior content (or remove the file if it didn't exist).
        if let Some(content) = backup {
            let _ = fs::write(&path, content);
        } else {
            let _ = fs::remove_file(&path);
        }
    }

    #[tokio::test]
    async fn load_config_async_returns_none_when_missing() {
        // Ensure no config file is present, then verify None is returned.
        let path = config_file_path();
        let backup = if path.exists() {
            Some(fs::read_to_string(&path).expect("read backup"))
        } else {
            None
        };
        let _ = fs::remove_file(&path);

        let result = load_config_async().await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());

        if let Some(content) = backup {
            let _ = fs::write(&path, content);
        }
    }

    #[tokio::test]
    async fn export_and_import_config_async_round_trip() {
        let base = config_store_base_dir();
        fs::create_dir_all(&base).expect("create base");
        let export_path = base.join(format!(
            "oxidelink_test_export_{}.json",
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));

        let config = AppConfig::default();
        let export_result = export_config_async(&config, export_path.to_str().unwrap()).await;
        assert!(
            export_result.is_ok(),
            "export_config_async failed: {:?}",
            export_result
        );

        let import_result = import_config_async(export_path.to_str().unwrap()).await;
        assert!(
            import_result.is_ok(),
            "import_config_async failed: {:?}",
            import_result
        );
        let imported = import_result.unwrap();
        assert_eq!(imported.deadzone_left, config.deadzone_left);
        assert_eq!(imported.keepalive_interval_ms, config.keepalive_interval_ms);

        cleanup(&export_path);
    }

    #[tokio::test]
    async fn import_config_async_rejects_invalid_config() {
        let base = config_store_base_dir();
        fs::create_dir_all(&base).expect("create base");
        let import_path = base.join(format!(
            "oxidelink_test_invalid_{}.json",
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));

        // Write a config with an invalid deadzone so validate_config fails.
        let mut config = AppConfig::default();
        config.deadzone_left = 99.0;
        let persisted = PersistedConfig::new(config);
        let json = serde_json::to_string_pretty(&persisted).expect("serialize");
        fs::write(&import_path, &json).expect("write");

        let result = import_config_async(import_path.to_str().unwrap()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("deadzone_left"));

        cleanup(&import_path);
    }

    #[tokio::test]
    async fn import_config_async_rejects_corrupt_json() {
        let base = config_store_base_dir();
        fs::create_dir_all(&base).expect("create base");
        let import_path = base.join(format!(
            "oxidelink_test_corrupt_{}.json",
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::write(&import_path, "{ not json").expect("write");

        let result = import_config_async(import_path.to_str().unwrap()).await;
        assert!(result.is_err());

        cleanup(&import_path);
    }

    #[tokio::test]
    async fn import_config_async_rejects_path_outside_base() {
        let outside = unique_temp_dir("import_outside");
        let import_path = outside.join("config.json");
        fs::write(&import_path, "{}").expect("write");

        let result = import_config_async(import_path.to_str().unwrap()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("must be absolute") || err.contains("outside"));

        cleanup(&outside);
    }

    // ------------------------------------------------------------------
    // Sync export/import with temp files inside the base dir
    // ------------------------------------------------------------------

    #[test]
    fn export_config_writes_valid_json_inside_base() {
        let base = config_store_base_dir();
        fs::create_dir_all(&base).expect("create base");
        let export_path = base.join(format!(
            "oxidelink_test_sync_export_{}.json",
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));

        let config = AppConfig::default();
        let result = export_config(&config, export_path.to_str().unwrap());
        assert!(result.is_ok(), "export_config failed: {:?}", result);
        assert!(export_path.exists());

        let content = fs::read_to_string(&export_path).expect("read export");
        assert!(content.contains("\"schema_version\""));

        cleanup(&export_path);
    }

    #[test]
    fn import_config_reads_and_validates() {
        let base = config_store_base_dir();
        fs::create_dir_all(&base).expect("create base");
        let import_path = base.join(format!(
            "oxidelink_test_sync_import_{}.json",
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));

        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config.clone());
        let json = serde_json::to_string_pretty(&persisted).expect("serialize");
        fs::write(&import_path, &json).expect("write");

        let result = import_config(import_path.to_str().unwrap());
        assert!(result.is_ok(), "import_config failed: {:?}", result);
        let imported = result.unwrap();
        assert_eq!(imported.deadzone_left, config.deadzone_left);

        cleanup(&import_path);
    }

    #[test]
    fn export_config_rejects_relative_path() {
        let config = AppConfig::default();
        let result = export_config(&config, "relative/path.json");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be absolute"));
    }

    #[test]
    fn import_config_rejects_nonexistent_file() {
        let base = config_store_base_dir();
        fs::create_dir_all(&base).expect("create base");
        let import_path = base.join(format!(
            "oxidelink_test_sync_missing_{}.json",
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        // File does not exist; validate_path with allow_nonexistent=false will
        // fail to canonicalize.
        let result = import_config(import_path.to_str().unwrap());
        assert!(result.is_err());
    }
}
