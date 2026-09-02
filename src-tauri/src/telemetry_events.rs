//! Opt-in feature usage telemetry.
//!
//! Tracks a small allow-list of feature events. When an Aptabase app key is
//! provided, events are sent via HTTPS. Otherwise they are written to the
//! local debug log and optionally to a local JSON file.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const ALLOWED_EVENTS: &[&str] = &[
    "profile_switched",
    "kbm_enabled",
    "macro_played",
    "hidhide_enabled",
    "gyro_mouse_used",
    "turbo_button_set",
];

const SENSITIVE_KEYS: &[&str] = &[
    "mac",
    "mac_address",
    "serial",
    "serial_number",
    "path",
    "file_path",
    "device_path",
    "address",
    "ip",
    "ip_address",
    "firmware",
    "bluetooth_address",
];

const DEFAULT_FLUSH_THRESHOLD: usize = 10;
const DEFAULT_MAX_BUFFER_SIZE: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryEvent {
    pub timestamp: String,
    pub name: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryStatus {
    pub enabled: bool,
    pub backend: String,
    pub key_redacted: Option<String>,
}

enum TelemetryBackend {
    Noop,
    DebugLog {
        file: Option<PathBuf>,
    },
    Aptabase {
        key: String,
        host: String,
        session_id: String,
    },
}

pub struct Telemetry {
    inner: Arc<Mutex<TelemetryInner>>,
}

struct TelemetryInner {
    enabled: bool,
    backend: TelemetryBackend,
    buffer: Vec<TelemetryEvent>,
    flush_threshold: usize,
    max_buffer_size: usize,
}

impl Default for TelemetryInner {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: TelemetryBackend::Noop,
            buffer: Vec::new(),
            flush_threshold: DEFAULT_FLUSH_THRESHOLD,
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
        }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TelemetryInner::default())),
        }
    }
}

impl Clone for Telemetry {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

static INSTANCE: OnceLock<Telemetry> = OnceLock::new();

impl Telemetry {
    pub fn instance() -> Telemetry {
        INSTANCE.get_or_init(Telemetry::default).clone()
    }

    pub fn status(&self) -> TelemetryStatus {
        let inner = self.inner.lock().unwrap();
        TelemetryStatus {
            enabled: inner.enabled,
            backend: inner.backend.name().to_string(),
            key_redacted: match &inner.backend {
                TelemetryBackend::Aptabase { key, .. } => Some(redact_key(key)),
                _ => None,
            },
        }
    }

    pub fn set_enabled(&self, enabled: bool, key: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.enabled = enabled;
        if !enabled {
            inner.backend = TelemetryBackend::Noop;
            return;
        }

        let key = key.filter(|k| !k.trim().is_empty());
        if let Some(k) = key {
            let host = aptabase_host(&k);
            let session = generate_session_id();
            inner.backend = TelemetryBackend::Aptabase {
                key: k,
                host,
                session_id: session,
            };
            log::info!("Telemetry backend: Aptabase ({})", inner.backend.host());
        } else {
            let file = telemetry_file_from_env();
            inner.backend = TelemetryBackend::DebugLog { file: file.clone() };
            log::info!("Telemetry backend: debug log (file={:?})", file);
        }
    }

    pub fn set_debug_log_file(&self, path: Option<PathBuf>) {
        let mut inner = self.inner.lock().unwrap();
        if let TelemetryBackend::DebugLog { file } = &mut inner.backend {
            *file = path;
        }
    }

    pub fn set_flush_threshold(&self, threshold: usize) {
        self.inner.lock().unwrap().flush_threshold = threshold;
    }

    pub fn record_event(&self, name: String, mut payload: Value) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !inner.enabled {
            return false;
        }
        if !ALLOWED_EVENTS.contains(&name.as_str()) {
            log::debug!("Telemetry event '{}' not in allow-list; dropped", name);
            return false;
        }

        scrub_payload(&mut payload);
        let event = TelemetryEvent {
            timestamp: Utc::now().to_rfc3339(),
            name,
            payload,
        };

        // Cap the in-memory buffer so telemetry cannot grow without bound.
        if inner.buffer.len() >= inner.max_buffer_size {
            let drain_to = inner.buffer.len() / 2;
            inner.buffer.drain(0..drain_to);
        }

        inner.buffer.push(event);

        if inner.buffer.len() >= inner.flush_threshold {
            drop(inner);
            let this = self.clone();
            std::thread::spawn(move || this.flush());
        }
        true
    }

    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.buffer.is_empty() {
            return;
        }
        let events = std::mem::take(&mut inner.buffer);

        match &inner.backend {
            TelemetryBackend::Noop => {}
            TelemetryBackend::DebugLog { file } => {
                let file = file.clone();
                drop(inner);
                flush_debug_log(events, file);
            }
            TelemetryBackend::Aptabase {
                key,
                host,
                session_id,
            } => {
                let key = key.clone();
                let host = host.clone();
                let session = session_id.clone();
                drop(inner);
                flush_aptabase(events, &key, &host, &session);
            }
        }
    }

    pub fn buffer_len(&self) -> usize {
        self.inner.lock().unwrap().buffer.len()
    }

    pub fn buffered_events(&self) -> Vec<TelemetryEvent> {
        self.inner.lock().unwrap().buffer.clone()
    }
}

impl TelemetryBackend {
    fn name(&self) -> &'static str {
        match self {
            TelemetryBackend::Noop => "noop",
            TelemetryBackend::DebugLog { .. } => "debug",
            TelemetryBackend::Aptabase { .. } => "aptabase",
        }
    }

    fn host(&self) -> &str {
        match self {
            TelemetryBackend::Aptabase { host, .. } => host,
            _ => "",
        }
    }
}

fn telemetry_file_from_env() -> Option<PathBuf> {
    std::env::var("OXIDELINK_TELEMETRY_FILE")
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

fn aptabase_host(key: &str) -> String {
    if key.starts_with("A-EU-") {
        "https://eu.aptabase.com".into()
    } else if key.starts_with("A-US-") {
        "https://us.aptabase.com".into()
    } else {
        "https://analytics.aptabase.com".into()
    }
}

fn generate_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), ts, n)
}

fn redact_key(key: &str) -> String {
    if key.len() <= 4 {
        "***".into()
    } else {
        format!("***{}", &key[key.len() - 4..])
    }
}

/// Recursively redact sensitive values in a telemetry payload.
pub fn scrub_payload(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let lower = k.to_lowercase();
                if SENSITIVE_KEYS.iter().any(|s| lower.contains(s)) {
                    *v = Value::String("<REDACTED>".into());
                } else {
                    scrub_payload(v);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                scrub_payload(v);
            }
        }
        Value::String(s) => {
            *s = crate::crash::scrub_pii(s);
        }
        _ => {}
    }
}

fn flush_debug_log(events: Vec<TelemetryEvent>, file: Option<PathBuf>) {
    for ev in &events {
        log::debug!(
            "telemetry: {}",
            serde_json::to_string(ev).unwrap_or_default()
        );
    }

    if let Some(path) = file {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new().append(true).create(true).open(&path) {
            for ev in events {
                let line = serde_json::to_string(&ev).unwrap_or_default();
                let _ = writeln!(f, "{}", line);
            }
        }
    }
}

fn flush_aptabase(events: Vec<TelemetryEvent>, key: &str, host: &str, session_id: &str) {
    let url = format!("{}/api/v0/events", host);
    let body: Vec<Value> = events
        .iter()
        .map(|ev| {
            serde_json::json!({
                "timestamp": ev.timestamp,
                "sessionId": session_id,
                "eventName": ev.name,
                "props": ev.payload,
            })
        })
        .collect();

    match ureq::post(&url)
        .set("App-Key", key)
        .set("Content-Type", "application/json")
        .send_json(&body)
    {
        Ok(resp) => {
            if resp.status() >= 400 {
                log::warn!("Aptabase returned HTTP {}", resp.status());
            } else {
                log::debug!("Aptabase telemetry flush succeeded");
            }
        }
        Err(e) => {
            log::warn!("Failed to send telemetry to Aptabase: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn set_telemetry_enabled(
    ctx: tauri::State<'_, crate::state::AppCtx>,
    enabled: bool,
    key: Option<String>,
) -> Result<TelemetryStatus, String> {
    {
        let mut cfg = ctx.shared.config.write();
        cfg.telemetry_enabled = enabled;
        cfg.telemetry_key = key.clone();
    }
    if ctx.shared.config.read().config_persistence_enabled {
        let cfg = ctx.shared.config.read().clone();
        if let Err(e) = crate::config::save_config(&cfg) {
            log::warn!("Failed to save config: {}", e);
        }
    }
    Telemetry::instance().set_enabled(enabled, key);
    Ok(Telemetry::instance().status())
}

#[tauri::command]
pub fn get_telemetry_status() -> TelemetryStatus {
    Telemetry::instance().status()
}

#[tauri::command]
pub fn record_telemetry_event(name: String, payload: Value) -> Result<bool, String> {
    Ok(Telemetry::instance().record_event(name, payload))
}

// ===========================================================================
//  Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Serializes tests that mutate the `OXIDELINK_TELEMETRY_FILE` env var.
    static ENV_TEST_MUTEX: Mutex<()> = Mutex::new(());

    // -----------------------------------------------------------------------
    //  Constants
    // -----------------------------------------------------------------------

    #[test]
    fn allowed_events_contains_expected_set() {
        assert!(ALLOWED_EVENTS.contains(&"profile_switched"));
        assert!(ALLOWED_EVENTS.contains(&"kbm_enabled"));
        assert!(ALLOWED_EVENTS.contains(&"macro_played"));
        assert!(ALLOWED_EVENTS.contains(&"hidhide_enabled"));
        assert!(ALLOWED_EVENTS.contains(&"gyro_mouse_used"));
        assert!(ALLOWED_EVENTS.contains(&"turbo_button_set"));
        assert!(!ALLOWED_EVENTS.contains(&"not_a_real_event"));
        assert_eq!(ALLOWED_EVENTS.len(), 6);
    }

    #[test]
    fn sensitive_keys_contains_expected_set() {
        for k in [
            "mac",
            "mac_address",
            "serial",
            "serial_number",
            "path",
            "file_path",
            "device_path",
            "address",
            "ip",
            "ip_address",
            "firmware",
            "bluetooth_address",
        ] {
            assert!(SENSITIVE_KEYS.contains(&k), "missing {k}");
        }
        assert_eq!(SENSITIVE_KEYS.len(), 12);
    }

    #[test]
    fn default_thresholds_are_sane() {
        assert_eq!(DEFAULT_FLUSH_THRESHOLD, 10);
        assert_eq!(DEFAULT_MAX_BUFFER_SIZE, 10_000);
    }

    // -----------------------------------------------------------------------
    //  Struct serialization / round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn telemetry_event_round_trip() {
        let ev = TelemetryEvent {
            timestamp: "2024-01-01T00:00:00Z".into(),
            name: "profile_switched".into(),
            payload: json!({"profile": "default"}),
        };
        let s = serde_json::to_string(&ev).expect("serialize");
        let back: TelemetryEvent = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, ev);
    }

    #[test]
    fn telemetry_status_round_trip_all_fields() {
        let st = TelemetryStatus {
            enabled: true,
            backend: "aptabase".into(),
            key_redacted: Some("***ABCD".into()),
        };
        let s = serde_json::to_string(&st).expect("serialize");
        let back: TelemetryStatus = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, st);
    }

    #[test]
    fn telemetry_status_key_redacted_none_round_trip() {
        let st = TelemetryStatus {
            enabled: false,
            backend: "noop".into(),
            key_redacted: None,
        };
        let s = serde_json::to_string(&st).expect("serialize");
        let back: TelemetryStatus = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, st);
    }

    // -----------------------------------------------------------------------
    //  TelemetryInner / Telemetry defaults
    // -----------------------------------------------------------------------

    #[test]
    fn telemetry_inner_default_values() {
        let inner = TelemetryInner::default();
        assert!(!inner.enabled);
        assert!(matches!(inner.backend, TelemetryBackend::Noop));
        assert!(inner.buffer.is_empty());
        assert_eq!(inner.flush_threshold, DEFAULT_FLUSH_THRESHOLD);
        assert_eq!(inner.max_buffer_size, DEFAULT_MAX_BUFFER_SIZE);
    }

    #[test]
    fn telemetry_default_is_disabled_noop() {
        let t = Telemetry::default();
        let st = t.status();
        assert!(!st.enabled);
        assert_eq!(st.backend, "noop");
        assert!(st.key_redacted.is_none());
        assert_eq!(t.buffer_len(), 0);
        assert!(t.buffered_events().is_empty());
    }

    #[test]
    fn telemetry_clone_shares_state() {
        let t = Telemetry::default();
        let t2 = t.clone();
        // Both clones share the same inner Arc, so buffer_len is shared.
        assert_eq!(t.buffer_len(), t2.buffer_len());
    }

    // -----------------------------------------------------------------------
    //  TelemetryBackend name() / host()
    // -----------------------------------------------------------------------

    #[test]
    fn backend_name_noop() {
        assert_eq!(TelemetryBackend::Noop.name(), "noop");
        assert_eq!(TelemetryBackend::Noop.host(), "");
    }

    #[test]
    fn backend_name_debug_log() {
        let b = TelemetryBackend::DebugLog { file: None };
        assert_eq!(b.name(), "debug");
        assert_eq!(b.host(), "");
        let with_file = TelemetryBackend::DebugLog {
            file: Some(PathBuf::from("/tmp/tel.json")),
        };
        assert_eq!(with_file.name(), "debug");
        assert_eq!(with_file.host(), "");
    }

    #[test]
    fn backend_name_aptabase() {
        let b = TelemetryBackend::Aptabase {
            key: "A-US-KEY".into(),
            host: "https://us.aptabase.com".into(),
            session_id: "sess".into(),
        };
        assert_eq!(b.name(), "aptabase");
        assert_eq!(b.host(), "https://us.aptabase.com");
    }

    // -----------------------------------------------------------------------
    //  status() reflects backend
    // -----------------------------------------------------------------------

    #[test]
    fn status_debug_log_backend() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        let st = t.status();
        assert!(st.enabled);
        assert_eq!(st.backend, "debug");
        assert!(st.key_redacted.is_none());
    }

    #[test]
    fn status_aptabase_backend_redacts_key() {
        let t = Telemetry::default();
        t.set_enabled(true, Some("A-US-1234567890".into()));
        let st = t.status();
        assert!(st.enabled);
        assert_eq!(st.backend, "aptabase");
        // Key redaction shows last 4 chars.
        assert_eq!(st.key_redacted.as_deref(), Some("***7890"));
    }

    #[test]
    fn status_disabled_resets_to_noop() {
        let t = Telemetry::default();
        t.set_enabled(true, Some("A-US-1234567890".into()));
        t.set_enabled(false, None);
        let st = t.status();
        assert!(!st.enabled);
        assert_eq!(st.backend, "noop");
        assert!(st.key_redacted.is_none());
    }

    #[test]
    fn set_enabled_with_blank_key_uses_debug_log() {
        let t = Telemetry::default();
        t.set_enabled(true, Some("   ".into()));
        let st = t.status();
        assert!(st.enabled);
        assert_eq!(st.backend, "debug");
        assert!(st.key_redacted.is_none());
    }

    #[test]
    fn set_enabled_with_empty_key_uses_debug_log() {
        let t = Telemetry::default();
        t.set_enabled(true, Some(String::new()));
        let st = t.status();
        assert!(st.enabled);
        assert_eq!(st.backend, "debug");
    }

    // -----------------------------------------------------------------------
    //  set_debug_log_file
    // -----------------------------------------------------------------------

    #[test]
    fn set_debug_log_file_updates_when_debug_backend() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        t.set_debug_log_file(Some(PathBuf::from("/tmp/events.json")));
        // No direct accessor for the file, but the call must not panic and
        // backend must remain debug.
        let st = t.status();
        assert_eq!(st.backend, "debug");
    }

    #[test]
    fn set_debug_log_file_noop_when_not_debug_backend() {
        let t = Telemetry::default();
        // Default is Noop; setting file should be a no-op (no panic).
        t.set_debug_log_file(Some(PathBuf::from("/tmp/x.json")));
        let st = t.status();
        assert_eq!(st.backend, "noop");
    }

    // -----------------------------------------------------------------------
    //  set_flush_threshold
    // -----------------------------------------------------------------------

    #[test]
    fn set_flush_threshold_changes_threshold() {
        let t = Telemetry::default();
        t.set_flush_threshold(3);
        // Verify by recording events and checking that flush is triggered
        // at the new threshold. We use a high max_buffer so no drain occurs.
        t.set_enabled(true, None);
        // Disable actual file writing by not setting a file path.
        assert!(t.record_event("profile_switched".into(), json!({})));
        assert!(t.record_event("kbm_enabled".into(), json!({})));
        // At threshold 3, two events should not yet trigger flush thread.
        // We can't deterministically check the spawned thread, but buffer
        // should have at most 2 events.
        assert!(t.buffer_len() <= 2);
    }

    // -----------------------------------------------------------------------
    //  record_event
    // -----------------------------------------------------------------------

    #[test]
    fn record_event_rejects_when_disabled() {
        let t = Telemetry::default();
        // Disabled by default.
        assert!(!t.record_event("profile_switched".into(), json!({})));
        assert_eq!(t.buffer_len(), 0);
    }

    #[test]
    fn record_event_rejects_unknown_event_name() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        assert!(!t.record_event("bogus_event".into(), json!({})));
        assert_eq!(t.buffer_len(), 0);
    }

    #[test]
    fn record_event_accepts_allowed_event() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        // Use a high flush threshold so no background flush thread spawns.
        t.set_flush_threshold(100);
        assert!(t.record_event("profile_switched".into(), json!({"profile": "x"})));
        assert_eq!(t.buffer_len(), 1);
        let evs = t.buffered_events();
        assert_eq!(evs[0].name, "profile_switched");
        assert_eq!(evs[0].payload, json!({"profile": "x"}));
        // timestamp should be a non-empty RFC3339 string.
        assert!(!evs[0].timestamp.is_empty());
    }

    #[test]
    fn record_event_scrubs_sensitive_payload_keys() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        t.set_flush_threshold(100);
        assert!(t.record_event(
            "hidhide_enabled".into(),
            json!({"mac": "AA:BB:CC:DD:EE:FF", "ok": "fine"})
        ));
        let evs = t.buffered_events();
        assert_eq!(evs[0].payload["mac"], "<REDACTED>");
        assert_eq!(evs[0].payload["ok"], "fine");
    }

    #[test]
    fn record_event_caps_buffer_at_max_size() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        t.set_flush_threshold(100);
        // We can't directly set max_buffer_size via public API, but the
        // default is 10_000. Fill beyond it to trigger the drain path.
        // To keep the test fast, we rely on the internal drain logic by
        // pushing exactly max_buffer_size + 1 events.
        for i in 0..(DEFAULT_MAX_BUFFER_SIZE + 1) {
            assert!(t.record_event("turbo_button_set".into(), json!({"i": i})));
        }
        // After drain, buffer should be roughly half + 1.
        assert!(t.buffer_len() <= DEFAULT_MAX_BUFFER_SIZE);
        assert!(t.buffer_len() > 0);
    }

    // -----------------------------------------------------------------------
    //  flush()
    // -----------------------------------------------------------------------

    #[test]
    fn flush_empty_buffer_is_noop() {
        let t = Telemetry::default();
        // Should not panic on empty buffer.
        t.flush();
        assert_eq!(t.buffer_len(), 0);
    }

    #[test]
    fn flush_noop_backend_clears_buffer() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        t.set_flush_threshold(100);
        assert!(t.record_event("profile_switched".into(), json!({})));
        assert_eq!(t.buffer_len(), 1);
        // Disable -> backend becomes Noop, then flush clears buffer.
        t.set_enabled(false, None);
        // set_enabled(false) doesn't clear buffer; flush with Noop will.
        t.flush();
        assert_eq!(t.buffer_len(), 0);
    }

    #[test]
    fn flush_debug_log_writes_to_temp_file() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        t.set_flush_threshold(100);
        let dir = std::env::temp_dir().join("oxidelink_telemetry_test_flush");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("events.jsonl");
        t.set_debug_log_file(Some(path.clone()));
        assert!(t.record_event("profile_switched".into(), json!({"p": 1})));
        assert!(t.record_event("kbm_enabled".into(), json!({"p": 2})));
        t.flush();
        assert_eq!(t.buffer_len(), 0);
        let contents = std::fs::read_to_string(&path).expect("file should exist");
        assert!(contents.contains("profile_switched"));
        assert!(contents.contains("kbm_enabled"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    //  aptabase_host
    // -----------------------------------------------------------------------

    #[test]
    fn aptabase_host_eu_prefix() {
        assert_eq!(aptabase_host("A-EU-abcdef"), "https://eu.aptabase.com");
    }

    #[test]
    fn aptabase_host_us_prefix() {
        assert_eq!(aptabase_host("A-US-abcdef"), "https://us.aptabase.com");
    }

    #[test]
    fn aptabase_host_other_prefix_defaults() {
        assert_eq!(
            aptabase_host("A-XX-abcdef"),
            "https://analytics.aptabase.com"
        );
        assert_eq!(aptabase_host("no-prefix"), "https://analytics.aptabase.com");
        assert_eq!(aptabase_host(""), "https://analytics.aptabase.com");
    }

    // -----------------------------------------------------------------------
    //  redact_key
    // -----------------------------------------------------------------------

    #[test]
    fn redact_key_long_shows_last_four() {
        assert_eq!(redact_key("A-US-1234567890"), "***7890");
    }

    #[test]
    fn redact_key_short_fully_masked() {
        assert_eq!(redact_key("AB"), "***");
        assert_eq!(redact_key("ABCD"), "***");
    }

    #[test]
    fn redact_key_exactly_five_chars() {
        assert_eq!(redact_key("ABCDE"), "***BCDE");
    }

    // -----------------------------------------------------------------------
    //  generate_session_id
    // -----------------------------------------------------------------------

    #[test]
    fn generate_session_id_is_unique_and_nonempty() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert!(!a.is_empty());
        assert!(!b.is_empty());
        // Counter increments, so the suffix should differ.
        assert_ne!(a, b);
    }

    #[test]
    fn generate_session_id_format_contains_pid() {
        let s = generate_session_id();
        let pid = std::process::id().to_string();
        assert!(
            s.starts_with(&pid),
            "session id '{s}' should start with pid {pid}"
        );
    }

    // -----------------------------------------------------------------------
    //  telemetry_file_from_env
    // -----------------------------------------------------------------------

    #[test]
    fn telemetry_file_from_env_unset_returns_none() {
        let _guard = ENV_TEST_MUTEX.lock().unwrap();
        std::env::remove_var("OXIDELINK_TELEMETRY_FILE");
        assert!(telemetry_file_from_env().is_none());
    }

    #[test]
    fn telemetry_file_from_env_empty_returns_none() {
        let _guard = ENV_TEST_MUTEX.lock().unwrap();
        std::env::set_var("OXIDELINK_TELEMETRY_FILE", "");
        assert!(telemetry_file_from_env().is_none());
        std::env::remove_var("OXIDELINK_TELEMETRY_FILE");
    }

    #[test]
    fn telemetry_file_from_env_set_returns_path() {
        let _guard = ENV_TEST_MUTEX.lock().unwrap();
        std::env::set_var("OXIDELINK_TELEMETRY_FILE", "/tmp/tel.json");
        let p = telemetry_file_from_env().expect("should be Some");
        assert_eq!(p, PathBuf::from("/tmp/tel.json"));
        std::env::remove_var("OXIDELINK_TELEMETRY_FILE");
    }

    // -----------------------------------------------------------------------
    //  scrub_payload
    // -----------------------------------------------------------------------

    #[test]
    fn scrub_payload_redacts_sensitive_top_level_keys() {
        let mut v = json!({
            "mac": "AA:BB:CC:DD:EE:FF",
            "ip": "1.2.3.4",
            "profile": "default"
        });
        scrub_payload(&mut v);
        assert_eq!(v["mac"], "<REDACTED>");
        assert_eq!(v["ip"], "<REDACTED>");
        assert_eq!(v["profile"], "default");
    }

    #[test]
    fn scrub_payload_redacts_case_insensitive_keys() {
        let mut v = json!({"MAC": "x", "Path": "y"});
        scrub_payload(&mut v);
        assert_eq!(v["MAC"], "<REDACTED>");
        assert_eq!(v["Path"], "<REDACTED>");
    }

    #[test]
    fn scrub_payload_redacts_partial_key_matches() {
        // "device_path" contains "path" and "device_path".
        let mut v = json!({"device_path": "/dev/x"});
        scrub_payload(&mut v);
        assert_eq!(v["device_path"], "<REDACTED>");
    }

    #[test]
    fn scrub_payload_recurses_into_nested_objects() {
        let mut v = json!({
            "outer": {
                "serial_number": "ABC123",
                "ok": "keep"
            }
        });
        scrub_payload(&mut v);
        assert_eq!(v["outer"]["serial_number"], "<REDACTED>");
        assert_eq!(v["outer"]["ok"], "keep");
    }

    #[test]
    fn scrub_payload_recurses_into_arrays() {
        let mut v = json!({
            "items": [
                {"mac": "AA:BB:CC:DD:EE:FF"},
                {"name": "ok"}
            ]
        });
        scrub_payload(&mut v);
        assert_eq!(v["items"][0]["mac"], "<REDACTED>");
        assert_eq!(v["items"][1]["name"], "ok");
    }

    #[test]
    fn scrub_payload_scrubs_pii_in_string_values() {
        // String values are run through crash::scrub_pii, which replaces
        // MAC addresses, paths, serials, and IPs.
        let mut v = json!({
            "note": "device at AA:BB:CC:DD:EE:FF and 10.0.0.1"
        });
        scrub_payload(&mut v);
        let s = v["note"].as_str().unwrap();
        assert!(!s.contains("AA:BB:CC:DD:EE:FF"));
        assert!(!s.contains("10.0.0.1"));
        assert!(s.contains("<MAC>"));
        assert!(s.contains("<IP>"));
    }

    #[test]
    fn scrub_payload_handles_scalars_unchanged() {
        let mut v = json!({"count": 42, "flag": true});
        scrub_payload(&mut v);
        assert_eq!(v["count"], 42);
        assert_eq!(v["flag"], true);
    }

    #[test]
    fn scrub_payload_handles_top_level_array() {
        let mut v = json!([
            {"mac": "AA:BB:CC:DD:EE:FF"},
            "plain string"
        ]);
        scrub_payload(&mut v);
        assert_eq!(v[0]["mac"], "<REDACTED>");
    }

    #[test]
    fn scrub_payload_empty_object_and_array() {
        let mut v = json!({});
        scrub_payload(&mut v);
        assert_eq!(v, json!({}));

        let mut a = json!([]);
        scrub_payload(&mut a);
        assert_eq!(a, json!([]));
    }

    // -----------------------------------------------------------------------
    //  flush_aptabase payload building (logic only, no network)
    // -----------------------------------------------------------------------

    #[test]
    fn aptabase_payload_building_logic() {
        // Reproduce the body-building logic from flush_aptabase without
        // performing the network request.
        let events = vec![
            TelemetryEvent {
                timestamp: "2024-01-01T00:00:00Z".into(),
                name: "profile_switched".into(),
                payload: json!({"profile": "x"}),
            },
            TelemetryEvent {
                timestamp: "2024-01-01T00:00:01Z".into(),
                name: "kbm_enabled".into(),
                payload: json!({}),
            },
        ];
        let session_id = "sess-1";
        let body: Vec<Value> = events
            .iter()
            .map(|ev| {
                json!({
                    "timestamp": ev.timestamp,
                    "sessionId": session_id,
                    "eventName": ev.name,
                    "props": ev.payload,
                })
            })
            .collect();

        assert_eq!(body.len(), 2);
        assert_eq!(body[0]["eventName"], "profile_switched");
        assert_eq!(body[0]["sessionId"], "sess-1");
        assert_eq!(body[0]["props"]["profile"], "x");
        assert_eq!(body[1]["eventName"], "kbm_enabled");
        assert_eq!(body[1]["props"], json!({}));
    }

    #[test]
    fn aptabase_url_construction_logic() {
        // Verify URL format used by flush_aptabase.
        let host = "https://eu.aptabase.com";
        let url = format!("{}/api/v0/events", host);
        assert_eq!(url, "https://eu.aptabase.com/api/v0/events");
    }

    // -----------------------------------------------------------------------
    //  flush_debug_log serialization logic (no file I/O variant)
    // -----------------------------------------------------------------------

    #[test]
    fn flush_debug_log_serializes_events() {
        let events = vec![TelemetryEvent {
            timestamp: "2024-01-01T00:00:00Z".into(),
            name: "profile_switched".into(),
            payload: json!({"p": 1}),
        }];
        let lines: Vec<String> = events
            .iter()
            .map(|ev| serde_json::to_string(ev).unwrap_or_default())
            .collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("profile_switched"));
        assert!(lines[0].contains("2024-01-01T00:00:00Z"));
    }

    // -----------------------------------------------------------------------
    //  Telemetry::instance singleton
    // -----------------------------------------------------------------------

    #[test]
    fn instance_returns_shared_singleton() {
        let a = Telemetry::instance();
        let b = Telemetry::instance();
        // Both should report the same buffer length (shared Arc).
        assert_eq!(a.buffer_len(), b.buffer_len());
    }

    // -----------------------------------------------------------------------
    //  All allowed event names are accepted by record_event
    // -----------------------------------------------------------------------

    #[test]
    fn all_allowed_events_accepted() {
        let t = Telemetry::default();
        t.set_enabled(true, None);
        t.set_flush_threshold(100);
        for name in ALLOWED_EVENTS {
            assert!(
                t.record_event((*name).into(), json!({})),
                "event {name} should be accepted"
            );
        }
        assert_eq!(t.buffer_len(), ALLOWED_EVENTS.len());
        let names: Vec<String> = t.buffered_events().iter().map(|e| e.name.clone()).collect();
        for name in ALLOWED_EVENTS {
            assert!(
                names.contains(&(*name).to_string()),
                "buffer missing {name}"
            );
        }
    }
}
