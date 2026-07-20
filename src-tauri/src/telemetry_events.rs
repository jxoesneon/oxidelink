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
use serde::Serialize;
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

#[derive(Debug, Clone, Serialize)]
pub struct TelemetryEvent {
    pub timestamp: String,
    pub name: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
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
