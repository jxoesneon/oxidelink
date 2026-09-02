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

    // -----------------------------------------------------------------------
    // UpdateInfo struct & serialization
    // -----------------------------------------------------------------------

    #[test]
    fn update_info_construction_with_all_fields() {
        let info = UpdateInfo {
            version: "1.2.3".into(),
            notes: "Release notes".into(),
            date: Some("2026-01-01T00:00:00Z".into()),
            signature: Some("sig-abc".into()),
        };
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.notes, "Release notes");
        assert_eq!(info.date, Some("2026-01-01T00:00:00Z".into()));
        assert_eq!(info.signature, Some("sig-abc".into()));
    }

    #[test]
    fn update_info_construction_with_none_fields() {
        let info = UpdateInfo {
            version: "0.1.0".into(),
            notes: String::new(),
            date: None,
            signature: None,
        };
        assert_eq!(info.version, "0.1.0");
        assert!(info.notes.is_empty());
        assert!(info.date.is_none());
        assert!(info.signature.is_none());
    }

    #[test]
    fn update_info_serialization_produces_snake_case_fields() {
        let info = UpdateInfo {
            version: "2.0.0".into(),
            notes: "notes".into(),
            date: Some("2026-06-06".into()),
            signature: Some("sig".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"version\""));
        assert!(json.contains("\"notes\""));
        assert!(json.contains("\"date\""));
        assert!(json.contains("\"signature\""));
    }

    #[test]
    fn update_info_serialization_none_fields_omit() {
        let info = UpdateInfo {
            version: "2.0.0".into(),
            notes: "notes".into(),
            date: None,
            signature: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        // serde serializes None as null by default (not omitted).
        assert!(json.contains("\"date\":null"));
        assert!(json.contains("\"signature\":null"));
    }

    #[test]
    fn update_info_clone_preserves_fields() {
        let info = UpdateInfo {
            version: "1.0.0".into(),
            notes: "test".into(),
            date: Some("d".into()),
            signature: Some("s".into()),
        };
        let cloned = info.clone();
        assert_eq!(cloned.version, info.version);
        assert_eq!(cloned.notes, info.notes);
        assert_eq!(cloned.date, info.date);
        assert_eq!(cloned.signature, info.signature);
    }

    // -----------------------------------------------------------------------
    // Version comparison — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn version_comparison_pre_release_is_lower() {
        // 1.0.0-alpha is older than 1.0.0
        assert!(is_update_newer("1.0.0-alpha", "1.0.0").unwrap());
        // 1.0.0-alpha is older than 1.0.0-beta
        assert!(is_update_newer("1.0.0-alpha", "1.0.0-beta").unwrap());
    }

    #[test]
    fn version_comparison_pre_release_vs_release() {
        // A release version is newer than a pre-release of the same.
        assert!(is_update_newer("1.0.0-rc.1", "1.0.0").unwrap());
        assert!(!is_update_newer("1.0.0", "1.0.0-rc.1").unwrap());
    }

    #[test]
    fn version_comparison_build_metadata_does_not_override_major() {
        // Build metadata should not make an older version appear newer.
        assert!(is_update_newer("1.0.0+build1", "1.1.0+build2").unwrap());
        assert!(!is_update_newer("1.1.0+build1", "1.0.0+build2").unwrap());
    }

    #[test]
    fn version_comparison_major_version_jump() {
        assert!(is_update_newer("1.9.9", "2.0.0").unwrap());
        assert!(is_update_newer("1.0.0", "10.0.0").unwrap());
    }

    #[test]
    fn version_comparison_minor_and_patch_increments() {
        assert!(is_update_newer("1.0.0", "1.0.1").unwrap());
        assert!(is_update_newer("1.0.0", "1.1.0").unwrap());
        assert!(!is_update_newer("1.1.0", "1.0.5").unwrap());
    }

    #[test]
    fn version_comparison_error_message_contains_version() {
        let err = is_update_newer("bad-version", "0.1.0").unwrap_err();
        assert!(err.contains("bad-version"));
        assert!(err.contains("failed to parse current version"));
    }

    #[test]
    fn version_comparison_error_message_contains_candidate() {
        let err = is_update_newer("0.1.0", "not-valid").unwrap_err();
        assert!(err.contains("not-valid"));
        assert!(err.contains("failed to parse candidate version"));
    }

    #[test]
    fn version_comparison_empty_strings_error() {
        assert!(is_update_newer("", "0.1.0").is_err());
        assert!(is_update_newer("0.1.0", "").is_err());
    }

    // -----------------------------------------------------------------------
    // parse_update_manifest — all branches
    // -----------------------------------------------------------------------

    #[test]
    fn manifest_parsing_body_alias_for_notes() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "body": "body field notes"
        });
        let info = parse_update_manifest(&value).unwrap();
        assert_eq!(info.notes, "body field notes");
    }

    #[test]
    fn manifest_parsing_notes_takes_priority_over_body() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "notes": "notes field",
            "body": "body field"
        });
        let info = parse_update_manifest(&value).unwrap();
        assert_eq!(info.notes, "notes field");
    }

    #[test]
    fn manifest_parsing_date_alias() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "date": "2026-03-15"
        });
        let info = parse_update_manifest(&value).unwrap();
        assert_eq!(info.date, Some("2026-03-15".into()));
    }

    #[test]
    fn manifest_parsing_pub_date_takes_priority_over_date() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "pub_date": "2026-03-15",
            "date": "2026-03-14"
        });
        let info = parse_update_manifest(&value).unwrap();
        assert_eq!(info.date, Some("2026-03-15".into()));
    }

    #[test]
    fn manifest_parsing_missing_notes_defaults_empty() {
        let value = serde_json::json!({"version": "1.0.0"});
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.notes.is_empty());
    }

    #[test]
    fn manifest_parsing_missing_date_is_none() {
        let value = serde_json::json!({"version": "1.0.0"});
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.date.is_none());
    }

    #[test]
    fn manifest_parsing_missing_signature_is_none() {
        let value = serde_json::json!({"version": "1.0.0"});
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.signature.is_none());
    }

    #[test]
    fn manifest_parsing_top_level_signature_overrides_platform() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "signature": "top-level-sig",
            "platforms": {
                "windows-x86_64": {
                    "signature": "platform-sig"
                }
            }
        });
        let info = parse_update_manifest(&value).unwrap();
        assert_eq!(info.signature, Some("top-level-sig".into()));
    }

    #[test]
    fn manifest_parsing_platform_without_windows_key() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "platforms": {
                "darwin-aarch64": {
                    "signature": "mac-sig"
                }
            }
        });
        let info = parse_update_manifest(&value).unwrap();
        // No windows-x86_64 key and no top-level signature -> None.
        assert!(info.signature.is_none());
    }

    #[test]
    fn manifest_parsing_platform_without_signature_field() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "platforms": {
                "windows-x86_64": {
                    "url": "https://example.com/setup.exe"
                }
            }
        });
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.signature.is_none());
    }

    #[test]
    fn manifest_parsing_non_string_version_errors() {
        let value = serde_json::json!({"version": 123});
        assert!(parse_update_manifest(&value).is_err());
    }

    #[test]
    fn manifest_parsing_version_null_errors() {
        let value = serde_json::json!({"version": null});
        assert!(parse_update_manifest(&value).is_err());
    }

    #[test]
    fn manifest_parsing_empty_object_errors() {
        let value = serde_json::json!({});
        assert!(parse_update_manifest(&value).is_err());
    }

    #[test]
    fn manifest_parsing_empty_notes_string() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "notes": ""
        });
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.notes.is_empty());
    }

    #[test]
    fn manifest_parsing_empty_body_string() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "body": ""
        });
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.notes.is_empty());
    }

    #[test]
    fn manifest_parsing_notes_non_string_falls_back_to_empty() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "notes": 42
        });
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.notes.is_empty());
    }

    #[test]
    fn manifest_parsing_date_non_string_is_none() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "pub_date": 12345
        });
        let info = parse_update_manifest(&value).unwrap();
        assert!(info.date.is_none());
    }

    #[test]
    fn manifest_parsing_signature_non_string_uses_platform_fallback() {
        let value = serde_json::json!({
            "version": "1.0.0",
            "signature": 99,
            "platforms": {
                "windows-x86_64": {
                    "signature": "platform-sig"
                }
            }
        });
        let info = parse_update_manifest(&value).unwrap();
        // Top-level signature is non-string so falls back to platform.
        assert_eq!(info.signature, Some("platform-sig".into()));
    }

    // -----------------------------------------------------------------------
    // generate_sample_update_manifest
    // -----------------------------------------------------------------------

    #[test]
    fn sample_manifest_has_version_field() {
        let manifest = generate_sample_update_manifest();
        assert!(manifest.get("version").is_some());
        assert_eq!(manifest["version"], "0.2.0");
    }

    #[test]
    fn sample_manifest_has_notes_field() {
        let manifest = generate_sample_update_manifest();
        assert!(manifest.get("notes").is_some());
        assert!(manifest["notes"].as_str().unwrap().contains("auto-updater"));
    }

    #[test]
    fn sample_manifest_has_pub_date() {
        let manifest = generate_sample_update_manifest();
        assert!(manifest.get("pub_date").is_some());
        assert_eq!(manifest["pub_date"], "2026-07-19T00:00:00Z");
    }

    #[test]
    fn sample_manifest_has_top_level_signature() {
        let manifest = generate_sample_update_manifest();
        assert_eq!(manifest["signature"], "sample-signature-placeholder");
    }

    #[test]
    fn sample_manifest_has_url() {
        let manifest = generate_sample_update_manifest();
        assert!(manifest.get("url").is_some());
        assert!(manifest["url"]
            .as_str()
            .unwrap()
            .contains("OxideLink_0.2.0"));
    }

    #[test]
    fn sample_manifest_has_platforms_object() {
        let manifest = generate_sample_update_manifest();
        let platforms = manifest.get("platforms").expect("platforms should exist");
        let win = platforms
            .get("windows-x86_64")
            .expect("windows-x86_64 should exist");
        assert_eq!(win["signature"], "sample-signature-placeholder");
        assert!(win.get("url").is_some());
    }

    #[test]
    fn sample_manifest_platform_and_top_level_signatures_match() {
        let manifest = generate_sample_update_manifest();
        let top = manifest["signature"].as_str().unwrap();
        let plat = manifest["platforms"]["windows-x86_64"]["signature"]
            .as_str()
            .unwrap();
        assert_eq!(top, plat);
    }

    #[test]
    fn sample_manifest_version_is_valid_semver() {
        let manifest = generate_sample_update_manifest();
        let version = manifest["version"].as_str().unwrap();
        assert!(semver::Version::parse(version).is_ok());
    }

    #[test]
    fn sample_manifest_parsed_date_is_some() {
        let manifest = generate_sample_update_manifest();
        let info = parse_update_manifest(&manifest).unwrap();
        assert_eq!(info.date, Some("2026-07-19T00:00:00Z".into()));
    }

    #[test]
    fn sample_manifest_parsed_notes_contain_release_info() {
        let manifest = generate_sample_update_manifest();
        let info = parse_update_manifest(&manifest).unwrap();
        assert!(info.notes.contains("Wave 3"));
        assert!(info.notes.contains("NSIS"));
    }

    // -----------------------------------------------------------------------
    // Combined: manifest + version comparison
    // -----------------------------------------------------------------------

    #[test]
    fn sample_manifest_version_is_newer_than_initial() {
        let manifest = generate_sample_update_manifest();
        let info = parse_update_manifest(&manifest).unwrap();
        assert!(is_update_newer("0.1.0", &info.version).unwrap());
    }

    #[test]
    fn sample_manifest_version_is_not_newer_than_itself() {
        let manifest = generate_sample_update_manifest();
        let info = parse_update_manifest(&manifest).unwrap();
        assert!(!is_update_newer(&info.version, &info.version).unwrap());
    }
}
