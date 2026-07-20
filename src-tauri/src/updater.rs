//! Auto-updater commands and helpers.
//!
//! Wraps `tauri-plugin-updater` with OxideLink-specific defaults:
//! * `update_endpoint` can be overridden at runtime through `AppConfig`.
//! * Version comparison and manifest parsing helpers are unit-tested in `lib.rs`.

use crate::config;
use crate::state::AppCtx;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Update check result returned to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub date: Option<String>,
    pub signature: Option<String>,
}

/// Compare two semver strings.
///
/// Returns `Ok(true)` when `candidate` is newer than `current`.
/// Non-semver strings return an error.
pub fn is_update_newer(current: &str, candidate: &str) -> Result<bool, String> {
    let current = semver::Version::parse(current)
        .map_err(|e| format!("failed to parse current version '{}': {}", current, e))?;
    let candidate = semver::Version::parse(candidate)
        .map_err(|e| format!("failed to parse candidate version '{}': {}", candidate, e))?;
    Ok(candidate > current)
}

/// Parse a Tauri-compatible update manifest into `UpdateInfo`.
///
/// Accepts both top-level (`version`, `notes`, `pub_date`, `signature`/`url`)
/// and nested `platforms.windows-x86_64` signatures for local tests.
pub fn parse_update_manifest(value: &Value) -> Result<UpdateInfo, String> {
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "manifest missing 'version'".to_string())?
        .to_string();

    let notes = value
        .get("notes")
        .or_else(|| value.get("body"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let date = value
        .get("pub_date")
        .or_else(|| value.get("date"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let signature = value
        .get("signature")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let signature = signature.or_else(|| {
        value
            .get("platforms")
            .and_then(|p| p.get("windows-x86_64"))
            .and_then(|w| w.get("signature"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    Ok(UpdateInfo {
        version,
        notes,
        date,
        signature,
    })
}

/// Generate a sample update manifest suitable for local testing.
pub fn generate_sample_update_manifest() -> Value {
    serde_json::json!({
        "version": "0.2.0",
        "notes": "Wave 3 auto-updater and build pipeline release.\n- Self-updates via tauri-plugin-updater\n- NSIS installer with optional HidHide/ViGEmBus integration",
        "pub_date": "2026-07-19T00:00:00Z",
        "signature": "sample-signature-placeholder",
        "url": "https://example.com/oxidelink/0.2.0/OxideLink_0.2.0_x64-setup.exe",
        "platforms": {
            "windows-x86_64": {
                "signature": "sample-signature-placeholder",
                "url": "https://example.com/oxidelink/0.2.0/OxideLink_0.2.0_x64-setup.exe"
            }
        }
    })
}

/// Build the update endpoint URL configured in `AppConfig`.
fn update_endpoint(app: &AppHandle) -> String {
    app.state::<AppCtx>()
        .shared
        .config
        .read()
        .update_endpoint
        .clone()
}

/// Fetch the available update using the configured endpoint (if any).
async fn fetch_update(app: AppHandle) -> Result<Option<Update>, String> {
    let endpoint = update_endpoint(&app);

    let builder = app.updater_builder();

    let update = if endpoint.trim().is_empty() {
        builder
            .build()
            .map_err(|e| format!("failed to build updater: {}", e))?
    } else {
        let url = tauri::Url::parse(&endpoint)
            .map_err(|e| format!("invalid update endpoint '{}': {}", endpoint, e))?;
        builder
            .endpoints(vec![url])
            .map_err(|e| format!("failed to configure updater endpoints: {}", e))?
            .build()
            .map_err(|e| format!("failed to build updater: {}", e))?
    };

    update
        .check()
        .await
        .map_err(|e| format!("update check failed: {}", e))
}

/// Check whether a newer version is available.
#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = fetch_update(app).await?;

    Ok(update.map(|u| UpdateInfo {
        version: u.version.clone(),
        notes: u.body.clone().unwrap_or_default(),
        date: u.date.map(|d| d.to_string()),
        signature: if u.signature.is_empty() {
            None
        } else {
            Some(u.signature.clone())
        },
    }))
}

/// Download and install the latest update.
///
/// Returns `true` when the update was downloaded and installed successfully.
/// The application may restart as part of the installation.
#[tauri::command]
pub async fn download_and_install_update(app: AppHandle) -> Result<bool, String> {
    let update = match fetch_update(app).await? {
        Some(u) => u,
        None => return Ok(false),
    };

    update
        .download_and_install(
            |_chunk, _total| {
                // Progress callbacks can be wired to frontend events later.
            },
            || {
                log::info!("update download completed, starting installation");
            },
        )
        .await
        .map_err(|e| format!("failed to download/install update: {}", e))?;

    Ok(true)
}

/// Persist a custom update endpoint in `AppConfig`.
#[tauri::command]
pub fn set_update_endpoint(endpoint: String, ctx: State<'_, AppCtx>) -> Result<(), String> {
    let mut cfg = ctx.shared.config.write();
    cfg.update_endpoint = endpoint;
    let persistence = cfg.config_persistence_enabled;
    let cfg_clone = cfg.clone();
    drop(cfg);

    if persistence {
        config::save_config(&cfg_clone)?;
    }

    Ok(())
}

/// Return the currently configured update endpoint.
#[tauri::command]
pub fn get_update_endpoint(ctx: State<'_, AppCtx>) -> String {
    ctx.shared.config.read().update_endpoint.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_detects_newer_release() {
        assert!(is_update_newer("0.1.0", "0.2.0").unwrap());
        assert!(is_update_newer("0.1.0", "0.1.1").unwrap());
        assert!(is_update_newer("0.9.9", "1.0.0").unwrap());
    }

    #[test]
    fn version_comparison_rejects_equal_or_older() {
        assert!(!is_update_newer("0.2.0", "0.2.0").unwrap());
        assert!(!is_update_newer("0.2.0", "0.1.0").unwrap());
        assert!(!is_update_newer("1.0.0", "0.9.9").unwrap());
    }

    #[test]
    fn version_comparison_errors_on_invalid_input() {
        assert!(is_update_newer("not-a-version", "0.2.0").is_err());
        assert!(is_update_newer("0.1.0", "also-bad").is_err());
    }

    #[test]
    fn manifest_parsing_top_level_fields() {
        let value = serde_json::json!({
            "version": "0.2.0",
            "notes": "Wave 3 release",
            "pub_date": "2026-07-19T00:00:00Z",
            "signature": "top-level-sig"
        });
        let info = parse_update_manifest(&value).expect("should parse");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.notes, "Wave 3 release");
        assert_eq!(info.date, Some("2026-07-19T00:00:00Z".into()));
        assert_eq!(info.signature, Some("top-level-sig".into()));
    }

    #[test]
    fn manifest_parsing_platform_signature_fallback() {
        let value = serde_json::json!({
            "version": "0.2.0",
            "body": "platform release",
            "date": "2026-07-20",
            "platforms": {
                "windows-x86_64": {
                    "signature": "platform-sig",
                    "url": "https://example.com/setup.exe"
                }
            }
        });
        let info = parse_update_manifest(&value).expect("should parse");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.notes, "platform release");
        assert_eq!(info.date, Some("2026-07-20".into()));
        assert_eq!(info.signature, Some("platform-sig".into()));
    }

    #[test]
    fn manifest_parsing_requires_version() {
        let value = serde_json::json!({"notes": "missing version"});
        assert!(parse_update_manifest(&value).is_err());
    }

    #[test]
    fn sample_manifest_roundtrips_through_parser() {
        let manifest = generate_sample_update_manifest();
        let info = parse_update_manifest(&manifest).expect("sample should parse");
        assert_eq!(info.version, "0.2.0");
        assert!(!info.notes.is_empty());
        assert!(info.signature.is_some());
    }
}
