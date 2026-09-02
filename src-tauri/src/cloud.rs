//! Cloud / community profile sharing (Wave 4).
//!
//! Provides profile upload/download/share-code generation and a community
//! profile browser. Config is stored in a separate JSON file to keep the
//! main `AppConfig` small.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::profile_manager::Profile;

/// Cloud service configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CloudConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub username: String,
    pub accepted_terms: bool,
}

/// Validate that a cloud endpoint uses HTTPS and does not reference placeholder
/// `.example` or `example.com` hosts.
fn validate_cloud_endpoint(endpoint: &str) -> Result<(), String> {
    if endpoint.is_empty() {
        return Ok(());
    }
    let lower = endpoint.to_lowercase();
    if lower.starts_with("http://") {
        return Err("cloud endpoint must use https://, http:// is not allowed".into());
    }
    if !lower.starts_with("https://") {
        return Err("cloud endpoint must use https://".into());
    }
    let rest = &endpoint[8..];
    let host_end = rest.find(&['/', ':'][..]).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        return Err("cloud endpoint must have a host".into());
    }
    let host_lower = host.to_lowercase();
    if host_lower.contains(".example") || host_lower.ends_with("example.com") {
        return Err("cloud endpoint cannot reference .example or example.com hosts".into());
    }
    Ok(())
}

/// A shared community profile record.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct CloudProfile {
    pub id: String,
    pub name: String,
    pub author: String,
    pub description: String,
    pub download_url: String,
    pub tags: Vec<String>,
    pub downloads: u64,
    pub rating: f64,
    pub created_at: String,
}

/// Path to the cloud config file: `%APPDATA%\OxideLink\cloud.json`.
fn cloud_config_path() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("OxideLink")
        .join("cloud.json")
}

/// Directory for cached community profile JSON files.
fn community_cache_dir() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("OxideLink")
        .join("community_cache")
}

/// Load the cloud configuration from disk, returning defaults if missing.
pub fn load_cloud_config() -> CloudConfig {
    let path = cloud_config_path();
    if !path.exists() {
        return CloudConfig::default();
    }
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => CloudConfig::default(),
    }
}

/// Persist the cloud configuration to disk.
pub fn save_cloud_config(config: &CloudConfig) -> Result<(), String> {
    let path = cloud_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Get the current cloud configuration.
#[tauri::command]
pub fn get_cloud_config() -> CloudConfig {
    load_cloud_config()
}

/// Set the cloud configuration.
#[tauri::command]
pub fn set_cloud_config(config: CloudConfig) -> Result<CloudConfig, String> {
    validate_cloud_endpoint(&config.endpoint)?;
    if let Err(e) = save_cloud_config(&config) {
        log::warn!("failed to save cloud config: {}", e);
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[cfg(not(test))]
fn http_get(url: &str) -> Result<String, String> {
    validate_cloud_endpoint(url)?;
    ureq::get(url)
        .timeout(REQUEST_TIMEOUT)
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

#[cfg(not(test))]
fn http_post(url: &str, body: &str) -> Result<String, String> {
    validate_cloud_endpoint(url)?;
    ureq::post(url)
        .set("Content-Type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send_string(body)
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Test mocks for localhost endpoints
// ---------------------------------------------------------------------------

#[cfg(test)]
fn http_get(url: &str) -> Result<String, String> {
    validate_cloud_endpoint(url)?;
    if is_localhost(url) {
        return mock_network_response(url, "GET", None);
    }
    ureq::get(url)
        .timeout(REQUEST_TIMEOUT)
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
fn http_post(url: &str, body: &str) -> Result<String, String> {
    validate_cloud_endpoint(url)?;
    if is_localhost(url) {
        return mock_network_response(url, "POST", Some(body));
    }
    ureq::post(url)
        .set("Content-Type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send_string(body)
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
fn is_localhost(url: &str) -> bool {
    url.contains("localhost") || url.contains("127.0.0.1")
}

#[cfg(test)]
fn mock_network_response(url: &str, method: &str, _body: Option<&str>) -> Result<String, String> {
    let path = url.split('?').next().unwrap_or(url);
    if method == "GET" {
        if path.ends_with("/profiles") && !path.ends_with("/profiles/") {
            return Ok(r#"[
                {
                    "id": "p1",
                    "name": "Aim",
                    "author": "alice",
                    "description": "Fast aim profile",
                    "download_url": "http://localhost:9999/profiles/p1",
                    "tags": ["fps"],
                    "downloads": 42,
                    "rating": 4.5,
                    "created_at": "2024-01-01"
                },
                {
                    "id": "p2",
                    "name": "RPG",
                    "author": "bob",
                    "description": "RPG profile",
                    "download_url": "http://localhost:9999/profiles/p2",
                    "tags": ["rpg"],
                    "downloads": 3,
                    "rating": 3.0,
                    "created_at": "2024-01-02"
                }
            ]"#
            .to_string());
        }
        if path.contains("/profiles/") {
            return Ok(r#"{
                "id": "p1",
                "name": "Aim",
                "enabled": true,
                "auto_rules": [],
                "created_at": 1,
                "updated_at": 1
            }"#
            .to_string());
        }
    } else if method == "POST" && path.ends_with("/profiles") {
        return Ok(r#""share-mock-abc123""#.to_string());
    }
    Err(format!("mock: unexpected {} {}", method, url))
}

// ---------------------------------------------------------------------------
// Share code generation
// ---------------------------------------------------------------------------

const SHARE_CODE_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
static SHARE_CODE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a short, URL-safe, process-unique share code.
fn generate_share_code() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let counter = SHARE_CODE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let value = (secs << 32) | (counter & 0xFFFF_FFFF);
    encode_u64(value)
}

fn encode_u64(mut n: u64) -> String {
    if n == 0 {
        return (SHARE_CODE_ALPHABET[0] as char).to_string();
    }
    let mut chars = Vec::new();
    while n > 0 {
        chars.push(SHARE_CODE_ALPHABET[(n & 0x3F) as usize]);
        n >>= 6;
    }
    chars.reverse();
    String::from_utf8(chars).unwrap()
}

// ---------------------------------------------------------------------------
// Community cache
// ---------------------------------------------------------------------------

fn cache_profile(id: &str, profile: &Profile) -> Result<(), String> {
    let dir = community_cache_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.json", id));
    let json = serde_json::to_string_pretty(profile).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Core networking functions
// ---------------------------------------------------------------------------

fn cloud_endpoint(config: &CloudConfig) -> String {
    config.endpoint.trim_end_matches('/').to_string()
}

fn list_community_profiles_with_config(
    config: &CloudConfig,
    tags: Option<String>,
) -> Result<Vec<CloudProfile>, String> {
    if !config.enabled {
        return Err("cloud sharing is disabled".into());
    }
    let mut url = format!("{}/profiles", cloud_endpoint(config));
    if let Some(tags) = tags {
        if !tags.is_empty() {
            url.push_str("?tags=");
            url.push_str(&tags);
        }
    }
    let body = http_get(&url)?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

fn download_profile_with_config(config: &CloudConfig, id: String) -> Result<Profile, String> {
    if !config.enabled {
        return Err("cloud sharing is disabled".into());
    }
    let url = format!("{}/profiles/{}", cloud_endpoint(config), id);
    let body = http_get(&url)?;
    let profile: Profile = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    cache_profile(&id, &profile)?;
    Ok(profile)
}

fn get_profile_by_code_with_config(config: &CloudConfig, code: String) -> Result<Profile, String> {
    if !config.enabled {
        return Err("cloud sharing is disabled".into());
    }
    let url = format!("{}/profiles/{}", cloud_endpoint(config), code);
    let body = http_get(&url)?;
    let profile: Profile = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    cache_profile(&profile.id, &profile)?;
    Ok(profile)
}

fn upload_profile_with_config(config: &CloudConfig, profile: Profile) -> Result<String, String> {
    if !config.enabled {
        return Err("cloud sharing is disabled".into());
    }
    let url = format!("{}/profiles", cloud_endpoint(config));
    let body = serde_json::to_string(&profile).map_err(|e| e.to_string())?;
    let response = http_post(&url, &body)?;

    // Servers may return a plain JSON string, an object with `share_code`/`id`,
    // or no useful body. Parse what we can and fall back to a generated code.
    if let Ok(code) = serde_json::from_str::<String>(&response) {
        if !code.is_empty() {
            return Ok(code);
        }
    }
    if let Ok(obj) = serde_json::from_str::<Value>(&response) {
        if let Some(s) = obj.get("share_code").and_then(|v| v.as_str()) {
            return Ok(s.to_string());
        }
        if let Some(s) = obj.get("id").and_then(|v| v.as_str()) {
            return Ok(s.to_string());
        }
    }
    Ok(generate_share_code())
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// List community profiles.
#[tauri::command]
pub async fn list_community_profiles(tags: Option<String>) -> Result<Vec<CloudProfile>, String> {
    let config = load_cloud_config();
    tokio::task::spawn_blocking(move || list_community_profiles_with_config(&config, tags))
        .await
        .map_err(|e| e.to_string())?
}

/// Download a community profile by id.
#[tauri::command]
pub async fn download_profile(id: String) -> Result<Profile, String> {
    let config = load_cloud_config();
    tokio::task::spawn_blocking(move || download_profile_with_config(&config, id))
        .await
        .map_err(|e| e.to_string())?
}

/// Upload the active profile and return a share code.
#[tauri::command]
pub async fn upload_profile(profile: Profile) -> Result<String, String> {
    let config = load_cloud_config();
    tokio::task::spawn_blocking(move || upload_profile_with_config(&config, profile))
        .await
        .map_err(|e| e.to_string())?
}

/// Download a profile using a community share code.
#[tauri::command]
pub async fn get_profile_by_code(code: String) -> Result<Profile, String> {
    let config = load_cloud_config();
    tokio::task::spawn_blocking(move || get_profile_by_code_with_config(&config, code))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_config_default_and_round_trip() {
        let cfg = CloudConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.endpoint.is_empty());
        assert!(cfg.api_key.is_none());
        assert!(cfg.username.is_empty());
        assert!(!cfg.accepted_terms);

        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: CloudConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, cfg2);
    }

    #[test]
    fn cloud_endpoint_validation_requires_non_placeholder_https() {
        assert!(validate_cloud_endpoint("https://cloud.oxidelink.dev/api").is_ok());
        assert!(validate_cloud_endpoint("http://cloud.oxidelink.dev").is_err());
        assert!(validate_cloud_endpoint("https://api.oxidelink.example").is_err());
        assert!(validate_cloud_endpoint("https://example.com/api").is_err());
    }

    #[test]
    fn parse_sample_cloud_profile() {
        let json = r#"{
            "id": "cp1",
            "name": "Pro FPS",
            "author": "ciel",
            "description": "fast aim",
            "download_url": "https://api.oxidelink.example/profiles/cp1",
            "tags": ["fps", "pc"],
            "downloads": 1234,
            "rating": 4.75,
            "created_at": "2024-06-15T12:34:56Z"
        }"#;
        let cp: CloudProfile = serde_json::from_str(json).unwrap();
        assert_eq!(cp.id, "cp1");
        assert_eq!(cp.name, "Pro FPS");
        assert_eq!(cp.author, "ciel");
        assert_eq!(cp.description, "fast aim");
        assert_eq!(
            cp.download_url,
            "https://api.oxidelink.example/profiles/cp1"
        );
        assert_eq!(cp.tags, vec!["fps", "pc"]);
        assert_eq!(cp.downloads, 1234);
        assert!((cp.rating - 4.75).abs() < f64::EPSILON);
        assert_eq!(cp.created_at, "2024-06-15T12:34:56Z");
    }

    #[test]
    fn share_code_is_unique_and_url_safe() {
        let allowed: std::collections::HashSet<char> =
            SHARE_CODE_ALPHABET.iter().map(|&b| b as char).collect();
        let mut seen = std::collections::HashSet::new();

        for _ in 0..200 {
            let code = generate_share_code();
            assert!(
                code.chars().all(|c| allowed.contains(&c)),
                "code '{}' contains an invalid character",
                code
            );
            assert!(seen.insert(code.clone()), "duplicate share code: {}", code);
        }
    }

    #[test]
    fn network_calls_use_localhost_mock() {
        let mut config = CloudConfig::default();
        config.enabled = true;
        config.endpoint = "https://localhost:9999".into();

        let profiles = list_community_profiles_with_config(&config, Some("fps".into())).unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, "p1");

        let profile = download_profile_with_config(&config, "p1".into()).unwrap();
        assert_eq!(profile.id, "p1");

        let by_code = get_profile_by_code_with_config(&config, "share-mock".into()).unwrap();
        assert_eq!(by_code.id, "p1");

        let code = upload_profile_with_config(&config, profile).unwrap();
        assert_eq!(code, "share-mock-abc123");
    }

    #[test]
    fn validate_cloud_endpoint_accepts_empty_and_localhost() {
        assert!(validate_cloud_endpoint("").is_ok());
        assert!(validate_cloud_endpoint("https://localhost:9999").is_ok());
        assert!(validate_cloud_endpoint("https://127.0.0.1:8080").is_ok());
    }

    #[test]
    fn validate_cloud_endpoint_rejects_http_and_placeholders() {
        assert!(validate_cloud_endpoint("http://localhost:9999").is_err());
        assert!(validate_cloud_endpoint("https://api.oxidelink.example").is_err());
        assert!(validate_cloud_endpoint("https://example.com").is_err());
        assert!(validate_cloud_endpoint("ftp://host").is_err());
    }

    // -----------------------------------------------------------------------
    // CloudConfig serialization & defaults
    // -----------------------------------------------------------------------

    #[test]
    fn cloud_config_serde_with_api_key_present() {
        let cfg = CloudConfig {
            enabled: true,
            endpoint: "https://cloud.oxidelink.dev".into(),
            api_key: Some("secret-key-123".into()),
            username: "ciel".into(),
            accepted_terms: true,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"api_key\":\"secret-key-123\""));
        assert!(json.contains("\"enabled\":true"));
        assert!(json.contains("\"accepted_terms\":true"));

        let decoded: CloudConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn cloud_config_serde_with_api_key_absent() {
        let json = r#"{
            "enabled": true,
            "endpoint": "https://cloud.oxidelink.dev",
            "username": "ciel",
            "accepted_terms": true
        }"#;
        let cfg: CloudConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.api_key.is_none());
        assert!(cfg.enabled);
        assert_eq!(cfg.endpoint, "https://cloud.oxidelink.dev");
    }

    #[test]
    fn cloud_config_serde_partial_uses_defaults() {
        // With #[serde(default)], missing fields should fall back to defaults.
        let json = r#"{"enabled": true}"#;
        let cfg: CloudConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert!(cfg.endpoint.is_empty());
        assert!(cfg.api_key.is_none());
        assert!(cfg.username.is_empty());
        assert!(!cfg.accepted_terms);
    }

    #[test]
    fn cloud_config_serde_empty_object() {
        let cfg: CloudConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, CloudConfig::default());
    }

    #[test]
    fn cloud_config_clone_preserves_equality() {
        let cfg = CloudConfig {
            enabled: true,
            endpoint: "https://cloud.oxidelink.dev".into(),
            api_key: Some("key".into()),
            username: "user".into(),
            accepted_terms: true,
        };
        assert_eq!(cfg, cfg.clone());
    }

    #[test]
    fn cloud_config_pretty_json_contains_all_fields() {
        let cfg = CloudConfig {
            enabled: false,
            endpoint: "https://cloud.oxidelink.dev".into(),
            api_key: None,
            username: "tester".into(),
            accepted_terms: false,
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(json.contains("\"enabled\""));
        assert!(json.contains("\"endpoint\""));
        assert!(json.contains("\"api_key\""));
        assert!(json.contains("\"username\""));
        assert!(json.contains("\"accepted_terms\""));
    }

    // -----------------------------------------------------------------------
    // validate_cloud_endpoint edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn validate_cloud_endpoint_https_with_path_and_port() {
        assert!(validate_cloud_endpoint("https://cloud.oxidelink.dev:8443/api/v1").is_ok());
        assert!(validate_cloud_endpoint("https://api.oxidelink.dev/v2/profiles").is_ok());
    }

    #[test]
    fn validate_cloud_endpoint_rejects_empty_host() {
        // "https://" with no host should fail.
        assert!(validate_cloud_endpoint("https://").is_err());
        assert!(validate_cloud_endpoint("https:///path").is_err());
    }

    #[test]
    fn validate_cloud_endpoint_rejects_no_scheme() {
        assert!(validate_cloud_endpoint("cloud.oxidelink.dev").is_err());
        assert!(validate_cloud_endpoint("just-a-host").is_err());
    }

    #[test]
    fn validate_cloud_endpoint_case_insensitive_scheme_check() {
        // Uppercase HTTPS should still be accepted (lowercased before check).
        assert!(validate_cloud_endpoint("HTTPS://cloud.oxidelink.dev").is_ok());
        // Mixed-case HTTP should be rejected.
        assert!(validate_cloud_endpoint("HTTP://cloud.oxidelink.dev").is_err());
    }

    #[test]
    fn validate_cloud_endpoint_rejects_ftp_and_other_schemes() {
        assert!(validate_cloud_endpoint("ftp://cloud.oxidelink.dev").is_err());
        assert!(validate_cloud_endpoint("ws://cloud.oxidelink.dev").is_err());
        assert!(validate_cloud_endpoint("file:///path").is_err());
    }

    #[test]
    fn validate_cloud_endpoint_rejects_example_subdomain() {
        assert!(validate_cloud_endpoint("https://my.example.org").is_err());
        assert!(validate_cloud_endpoint("https://sub.example.com").is_err());
    }

    #[test]
    fn validate_cloud_endpoint_accepts_localhost_variants() {
        assert!(validate_cloud_endpoint("https://localhost").is_ok());
        assert!(validate_cloud_endpoint("https://localhost:3000").is_ok());
        assert!(validate_cloud_endpoint("https://127.0.0.1:8080").is_ok());
    }

    // -----------------------------------------------------------------------
    // encode_u64 pure helper
    // -----------------------------------------------------------------------

    #[test]
    fn encode_u64_zero_returns_first_alphabet_char() {
        let result = encode_u64(0);
        assert_eq!(result, "A");
    }

    #[test]
    fn encode_u64_one_returns_b() {
        // 1 & 0x3F = 1 -> alphabet[1] = 'B'
        let result = encode_u64(1);
        assert_eq!(result, "B");
    }

    #[test]
    fn encode_u64_63_returns_last_char() {
        // 63 & 0x3F = 63 -> alphabet[63] = last char '_'
        let result = encode_u64(63);
        assert_eq!(result, "_");
    }

    #[test]
    fn encode_u64_64_returns_two_chars() {
        // 64 = 1*64 + 0 -> push alphabet[0]='A', then alphabet[1]='B', reversed -> "BA"
        let result = encode_u64(64);
        assert_eq!(result, "BA");
    }

    #[test]
    fn encode_u64_output_only_contains_alphabet_chars() {
        let allowed: std::collections::HashSet<char> =
            SHARE_CODE_ALPHABET.iter().map(|&b| b as char).collect();
        for n in [0u64, 1, 63, 64, 4096, 1_000_000, u64::MAX / 2, u64::MAX] {
            let encoded = encode_u64(n);
            assert!(
                encoded.chars().all(|c| allowed.contains(&c)),
                "encode_u64({}) = '{}' contains invalid char",
                n,
                encoded
            );
        }
    }

    // -----------------------------------------------------------------------
    // generate_share_code (guarded by mutex to avoid static counter races)
    // -----------------------------------------------------------------------

    static SHARE_CODE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn generate_share_code_is_non_empty() {
        let _guard = SHARE_CODE_GUARD.lock().unwrap();
        let code = generate_share_code();
        assert!(!code.is_empty());
    }

    #[test]
    fn generate_share_code_uses_alphabet_only() {
        let _guard = SHARE_CODE_GUARD.lock().unwrap();
        let allowed: std::collections::HashSet<char> =
            SHARE_CODE_ALPHABET.iter().map(|&b| b as char).collect();
        for _ in 0..50 {
            let code = generate_share_code();
            assert!(
                code.chars().all(|c| allowed.contains(&c)),
                "code '{}' contains invalid char",
                code
            );
        }
    }

    #[test]
    fn generate_share_code_consecutive_calls_differ() {
        let _guard = SHARE_CODE_GUARD.lock().unwrap();
        let c1 = generate_share_code();
        let c2 = generate_share_code();
        // The counter increments so the encoded value should differ.
        assert_ne!(c1, c2);
    }

    // -----------------------------------------------------------------------
    // cloud_endpoint helper
    // -----------------------------------------------------------------------

    #[test]
    fn cloud_endpoint_trims_single_trailing_slash() {
        let cfg = CloudConfig {
            endpoint: "https://localhost:9999/".into(),
            ..Default::default()
        };
        assert_eq!(cloud_endpoint(&cfg), "https://localhost:9999");
    }

    #[test]
    fn cloud_endpoint_trims_multiple_trailing_slashes() {
        let cfg = CloudConfig {
            endpoint: "https://localhost:9999///".into(),
            ..Default::default()
        };
        assert_eq!(cloud_endpoint(&cfg), "https://localhost:9999");
    }

    #[test]
    fn cloud_endpoint_no_trailing_slash_unchanged() {
        let cfg = CloudConfig {
            endpoint: "https://cloud.oxidelink.dev/api".into(),
            ..Default::default()
        };
        assert_eq!(cloud_endpoint(&cfg), "https://cloud.oxidelink.dev/api");
    }

    #[test]
    fn cloud_endpoint_empty_string_stays_empty() {
        let cfg = CloudConfig::default();
        assert_eq!(cloud_endpoint(&cfg), "");
    }

    // -----------------------------------------------------------------------
    // CloudProfile serialization
    // -----------------------------------------------------------------------

    #[test]
    fn cloud_profile_default_values() {
        let cp = CloudProfile::default();
        assert!(cp.id.is_empty());
        assert!(cp.name.is_empty());
        assert!(cp.author.is_empty());
        assert!(cp.description.is_empty());
        assert!(cp.download_url.is_empty());
        assert!(cp.tags.is_empty());
        assert_eq!(cp.downloads, 0);
        assert!((cp.rating - 0.0).abs() < f64::EPSILON);
        assert!(cp.created_at.is_empty());
    }

    #[test]
    fn cloud_profile_serde_round_trip() {
        let cp = CloudProfile {
            id: "test-1".into(),
            name: "Test Profile".into(),
            author: "tester".into(),
            description: "A test profile".into(),
            download_url: "https://localhost:9999/profiles/test-1".into(),
            tags: vec!["fps".into(), "test".into()],
            downloads: 999,
            rating: 5.0,
            created_at: "2024-12-31T23:59:59Z".into(),
        };
        let json = serde_json::to_string(&cp).unwrap();
        let decoded: CloudProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(cp, decoded);
    }

    #[test]
    fn cloud_profile_serde_uses_snake_case_fields() {
        let cp = CloudProfile {
            id: "x".into(),
            name: "X".into(),
            author: "a".into(),
            description: "d".into(),
            download_url: "u".into(),
            tags: vec![],
            downloads: 1,
            rating: 2.0,
            created_at: "c".into(),
        };
        let json = serde_json::to_string(&cp).unwrap();
        assert!(json.contains("\"download_url\""));
        assert!(json.contains("\"created_at\""));
    }

    // -----------------------------------------------------------------------
    // Disabled-config error paths (no network needed)
    // -----------------------------------------------------------------------

    #[test]
    fn list_community_profiles_disabled_returns_error() {
        let config = CloudConfig::default(); // enabled = false
        let result = list_community_profiles_with_config(&config, None);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "cloud sharing is disabled");
    }

    #[test]
    fn list_community_profiles_disabled_with_tags_returns_error() {
        let config = CloudConfig::default();
        let result = list_community_profiles_with_config(&config, Some("fps".into()));
        assert!(result.is_err());
    }

    #[test]
    fn download_profile_disabled_returns_error() {
        let config = CloudConfig::default();
        let result = download_profile_with_config(&config, "p1".into());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "cloud sharing is disabled");
    }

    #[test]
    fn upload_profile_disabled_returns_error() {
        let config = CloudConfig::default();
        let profile = Profile::default();
        let result = upload_profile_with_config(&config, profile);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "cloud sharing is disabled");
    }

    #[test]
    fn get_profile_by_code_disabled_returns_error() {
        let config = CloudConfig::default();
        let result = get_profile_by_code_with_config(&config, "code1".into());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "cloud sharing is disabled");
    }

    // -----------------------------------------------------------------------
    // Mock HTTP paths (localhost)
    // -----------------------------------------------------------------------

    #[test]
    fn is_localhost_detects_localhost_and_127() {
        assert!(is_localhost("https://localhost:9999/profiles"));
        assert!(is_localhost("https://127.0.0.1:8080/api"));
        assert!(!is_localhost("https://cloud.oxidelink.dev"));
    }

    #[test]
    fn mock_network_response_get_profiles_list() {
        let url = "https://localhost:9999/profiles";
        let result = mock_network_response(url, "GET", None);
        assert!(result.is_ok());
        let body = result.unwrap();
        assert!(body.contains("\"id\": \"p1\""));
        assert!(body.contains("\"id\": \"p2\""));
    }

    #[test]
    fn mock_network_response_get_single_profile() {
        let url = "https://localhost:9999/profiles/p1";
        let result = mock_network_response(url, "GET", None);
        assert!(result.is_ok());
        let body = result.unwrap();
        assert!(body.contains("\"id\": \"p1\""));
        assert!(body.contains("\"enabled\": true"));
    }

    #[test]
    fn mock_network_response_post_profiles_returns_share_code() {
        let url = "https://localhost:9999/profiles";
        let result = mock_network_response(url, "POST", Some("{}"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), r#""share-mock-abc123""#);
    }

    #[test]
    fn mock_network_response_unknown_path_returns_error() {
        let url = "https://localhost:9999/unknown";
        let result = mock_network_response(url, "GET", None);
        assert!(result.is_err());
    }

    #[test]
    fn mock_network_response_delete_method_returns_error() {
        let url = "https://localhost:9999/profiles";
        let result = mock_network_response(url, "DELETE", None);
        assert!(result.is_err());
    }

    #[test]
    fn list_community_profiles_mock_no_tags() {
        let config = CloudConfig {
            enabled: true,
            endpoint: "https://localhost:9999".into(),
            ..Default::default()
        };
        let profiles = list_community_profiles_with_config(&config, None).unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, "p1");
        assert_eq!(profiles[0].name, "Aim");
        assert_eq!(profiles[0].author, "alice");
        assert_eq!(profiles[0].downloads, 42);
        assert!((profiles[0].rating - 4.5).abs() < f64::EPSILON);
        assert_eq!(profiles[1].id, "p2");
        assert_eq!(profiles[1].tags, vec!["rpg"]);
    }

    #[test]
    fn list_community_profiles_mock_with_tags() {
        let config = CloudConfig {
            enabled: true,
            endpoint: "https://localhost:9999".into(),
            ..Default::default()
        };
        // The mock ignores query params but still returns the list.
        let profiles = list_community_profiles_with_config(&config, Some("fps".into())).unwrap();
        assert_eq!(profiles.len(), 2);
    }

    #[test]
    fn upload_profile_mock_returns_share_code() {
        let config = CloudConfig {
            enabled: true,
            endpoint: "https://localhost:9999".into(),
            ..Default::default()
        };
        let profile = Profile {
            id: "test".into(),
            name: "Test".into(),
            enabled: true,
            ..Default::default()
        };
        let code = upload_profile_with_config(&config, profile).unwrap();
        assert_eq!(code, "share-mock-abc123");
    }

    #[test]
    fn download_profile_mock_returns_profile() {
        let config = CloudConfig {
            enabled: true,
            endpoint: "https://localhost:9999".into(),
            ..Default::default()
        };
        let profile = download_profile_with_config(&config, "p1".into()).unwrap();
        assert_eq!(profile.id, "p1");
        assert_eq!(profile.name, "Aim");
        assert!(profile.enabled);
    }

    #[test]
    fn get_profile_by_code_mock_returns_profile() {
        let config = CloudConfig {
            enabled: true,
            endpoint: "https://localhost:9999".into(),
            ..Default::default()
        };
        let profile = get_profile_by_code_with_config(&config, "any-code".into()).unwrap();
        assert_eq!(profile.id, "p1");
    }

    // -----------------------------------------------------------------------
    // set_cloud_config validation (rejection path only — no disk write on err)
    // -----------------------------------------------------------------------

    #[test]
    fn set_cloud_config_rejects_http_endpoint() {
        let cfg = CloudConfig {
            enabled: true,
            endpoint: "http://localhost:9999".into(),
            ..Default::default()
        };
        let result = set_cloud_config(cfg);
        assert!(result.is_err());
    }

    #[test]
    fn set_cloud_config_rejects_placeholder_endpoint() {
        let cfg = CloudConfig {
            enabled: true,
            endpoint: "https://example.com".into(),
            ..Default::default()
        };
        let result = set_cloud_config(cfg);
        assert!(result.is_err());
    }

    #[test]
    fn set_cloud_config_rejects_no_scheme() {
        let cfg = CloudConfig {
            enabled: true,
            endpoint: "just-a-host".into(),
            ..Default::default()
        };
        let result = set_cloud_config(cfg);
        assert!(result.is_err());
    }
}
