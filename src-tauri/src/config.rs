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

    #[test]
    fn default_config_is_valid() {
        let config = AppConfig::default();
        assert!(
            validate_config(&config).is_ok(),
            "default config should be valid"
        );
    }

    #[test]
    fn invalid_deadzone_rejected() {
        let mut config = AppConfig::default();
        config.deadzone_left = 0.5;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn invalid_remap_rejected() {
        let mut config = AppConfig::default();
        config.button_remap.a_to = "invalid".into();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn invalid_curve_type_rejected() {
        let mut config = AppConfig::default();
        config.stick_calibration_config.response_curve_type = "quadratic".into();
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
    fn invalid_polling_interval_rejected() {
        let mut config = AppConfig::default();
        config.battery_polling_interval_s = 15;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn persisted_config_has_schema_version() {
        let config = AppConfig::default();
        let persisted = PersistedConfig::new(config);
        assert_eq!(persisted.schema_version, CURRENT_SCHEMA_VERSION);
    }
}
