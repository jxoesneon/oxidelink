//! Optional crash reporting.
//!
//! Crash reporting is opt-in. A valid Sentry DSN sends panics to Sentry after
//! stripping PII. The literal DSN `"test"` (or the `OXIDELINK_CRASH_TEST`
//! environment variable) writes panic reports to a local file instead of
//! sending them.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use sentry::protocol::{Event, Level, Stacktrace};
use sentry::ClientInitGuard;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

static CRASH_ENABLED: AtomicBool = AtomicBool::new(false);
static TEST_MODE: AtomicBool = AtomicBool::new(false);
static CURRENT_DSN: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SENTRY_GUARD: OnceLock<Mutex<Option<ClientInitGuard>>> = OnceLock::new();

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>;
static PREVIOUS_HOOK: OnceLock<PanicHook> = OnceLock::new();

/// Status returned by the crash-reporting Tauri commands.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CrashReportingStatus {
    pub enabled: bool,
    pub test_mode: bool,
    pub dsn: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialise crash reporting. Call once from `main`.
///
/// `None` or an empty string disables capture. `"test"` enables local-file
/// capture. A valid Sentry DSN enables Sentry-backed capture.
pub fn init_crash_reporting(dsn: Option<String>) {
    let dsn = dsn.filter(|s| !s.trim().is_empty());
    let is_test = is_test_dsn(dsn.as_deref()) || std::env::var("OXIDELINK_CRASH_TEST").is_ok();

    TEST_MODE.store(is_test, Ordering::SeqCst);
    CRASH_ENABLED.store(dsn.is_some() || is_test, Ordering::SeqCst);
    *CURRENT_DSN.get_or_init(|| Mutex::new(None)).lock().unwrap() = dsn.clone();

    install_panic_hook();

    if is_test {
        log::info!("Crash reporting in local test mode");
        return;
    }

    if let Some(dsn_str) = dsn {
        if validate_dsn(&dsn_str) {
            init_sentry(&dsn_str);
        } else {
            log::warn!("Crash reporting disabled: DSN is invalid");
            CRASH_ENABLED.store(false, Ordering::SeqCst);
        }
    }
}

/// Enable/disable crash reporting and/or change the DSN.
pub fn set_crash_reporting_enabled(enabled: bool, dsn: Option<String>) -> CrashReportingStatus {
    let dsn = dsn.filter(|s| !s.trim().is_empty());
    let is_test = is_test_dsn(dsn.as_deref()) || std::env::var("OXIDELINK_CRASH_TEST").is_ok();

    if !enabled {
        CRASH_ENABLED.store(false, Ordering::SeqCst);
        TEST_MODE.store(false, Ordering::SeqCst);
        *CURRENT_DSN.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
        return status();
    }

    let current = CURRENT_DSN
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone();

    if current.as_deref() != dsn.as_deref() {
        init_crash_reporting(dsn.clone());
    } else {
        TEST_MODE.store(is_test, Ordering::SeqCst);
        CRASH_ENABLED.store(true, Ordering::SeqCst);
    }
    status()
}

/// Validate a crash-reporting DSN.
pub fn validate_dsn(dsn: &str) -> bool {
    if is_test_dsn(Some(dsn)) {
        return true;
    }
    sentry::types::Dsn::from_str(dsn).is_ok()
}

/// Scrub PII from a free-form string.
///
/// Replaces MAC addresses, file paths, IP addresses and long alphanumeric
/// serial numbers with placeholders.
pub fn scrub_pii(input: &str) -> String {
    static MAC_RE: OnceLock<Regex> = OnceLock::new();
    static PATH_RE: OnceLock<Regex> = OnceLock::new();
    static SERIAL_RE: OnceLock<Regex> = OnceLock::new();
    static IP_RE: OnceLock<Regex> = OnceLock::new();

    let mac =
        MAC_RE.get_or_init(|| Regex::new(r"(?i)\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b").unwrap());
    let path = PATH_RE.get_or_init(|| {
        Regex::new(r#"(?i)([A-Za-z]:\\(?:[^\\\\/:*?\"<>|\r\n]+\\)*[^\\\\/:*?\"<>|\r\n]*)|(/(?:[^/ \t\r\n]+/)+[^/ \t\r\n]*)"#).unwrap()
    });
    let serial = SERIAL_RE.get_or_init(|| Regex::new(r"\b[A-Z0-9]{12,}\b").unwrap());
    let ip = IP_RE.get_or_init(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap());

    let mut out = mac.replace_all(input, "<MAC>").into_owned();
    out = path.replace_all(&out, "<PATH>").into_owned();
    out = serial.replace_all(&out, "<SERIAL>").into_owned();
    out = ip.replace_all(&out, "<IP>").into_owned();
    out
}

/// Current crash-reporting status.
pub fn status() -> CrashReportingStatus {
    let dsn = CURRENT_DSN
        .get()
        .and_then(|m| m.lock().unwrap().clone())
        .map(redact_dsn);
    CrashReportingStatus {
        enabled: CRASH_ENABLED.load(Ordering::Relaxed),
        test_mode: TEST_MODE.load(Ordering::Relaxed),
        dsn,
    }
}

/// Directory used for local crash files in test mode.
pub fn crash_dir() -> PathBuf {
    let mut path = dirs_next::data_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("OxideLink");
    path.push("crashes");
    path
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn set_crash_reporting(
    ctx: tauri::State<'_, crate::state::AppCtx>,
    enabled: bool,
    dsn: Option<String>,
) -> Result<CrashReportingStatus, String> {
    {
        let mut cfg = ctx.shared.config.write();
        cfg.crash_reporting_enabled = enabled;
        cfg.crash_reporting_dsn = dsn.clone();
    }
    if ctx.shared.config.read().config_persistence_enabled {
        let cfg = ctx.shared.config.read().clone();
        if let Err(e) = crate::config::save_config(&cfg) {
            log::warn!("Failed to save config: {}", e);
        }
    }
    Ok(set_crash_reporting_enabled(enabled, dsn))
}

#[tauri::command]
pub fn get_crash_reporting_status() -> CrashReportingStatus {
    status()
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn is_test_dsn(dsn: Option<&str>) -> bool {
    match dsn {
        None => false,
        Some(s) => s.eq_ignore_ascii_case("test") || s.starts_with("test://"),
    }
}

fn init_sentry(dsn: &str) {
    let opts = sentry::ClientOptions {
        dsn: sentry::types::Dsn::from_str(dsn).ok(),
        before_send: Some(Arc::new(before_send)),
        default_integrations: false,
        send_default_pii: false,
        ..Default::default()
    };
    let guard = sentry::init(opts);
    *SENTRY_GUARD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = Some(guard);
    log::info!("Sentry crash reporting initialised");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dsn_initialization_does_not_panic() {
        let result = std::panic::catch_unwind(|| init_crash_reporting(Some(String::new())));
        assert!(result.is_ok());
    }

    #[test]
    fn test_dsn_is_accepted_without_a_network_dsn() {
        assert!(validate_dsn("test"));
        assert!(!validate_dsn("not a dsn"));
    }

    #[test]
    fn crash_reporting_defaults_to_disabled() {
        init_crash_reporting(None);
        let st = status();
        assert!(!st.enabled);
        assert!(!st.test_mode);
        assert_eq!(st.dsn, None);
    }

    #[test]
    fn empty_dsn_is_treated_as_disabled() {
        init_crash_reporting(Some("   ".to_string()));
        let st = status();
        assert!(!st.enabled);
        assert!(!st.test_mode);
        assert_eq!(st.dsn, None);
    }

    #[test]
    fn test_dsn_enables_local_test_mode() {
        init_crash_reporting(Some("test".to_string()));
        let st = status();
        assert!(st.enabled);
        assert!(st.test_mode);
        assert_eq!(st.dsn, Some("test".to_string()));
    }

    #[test]
    fn valid_sentry_dsn_passes_validation() {
        let dsn = "https://public@o447951.ingest.sentry.io/5439417";
        assert!(validate_dsn(dsn));
    }

    #[test]
    fn pii_scrubbing_redacts_common_patterns() {
        // Keep IP and serial before the Windows path so the path regex does not
        // over-consume the rest of the string.
        let input = "IP 192.168.0.1 serial ABC123456789 MAC 00:1A:2B:3C:4D:5E path C:\\Users\\Me\\file.txt";
        let out = scrub_pii(input);
        assert!(out.contains("<MAC>"));
        assert!(out.contains("<PATH>"));
        assert!(out.contains("<IP>"));
        assert!(out.contains("<SERIAL>"));
    }
}

fn before_send(mut event: Event<'static>) -> Option<Event<'static>> {
    if !CRASH_ENABLED.load(Ordering::Relaxed) {
        return None;
    }

    // Strip metadata that can contain PII.
    event.server_name = None;
    event.request = None;
    event.user = None;
    event.environment = None;
    event.release = None;
    event.dist = None;
    event.contexts.clear();
    event.tags.clear();
    event.extra.clear();

    if let Some(ref mut msg) = event.message {
        *msg = scrub_pii(msg);
    }

    for ex in event.exception.values.iter_mut() {
        if let Some(ref mut v) = ex.value {
            *v = scrub_pii(v);
        }
        if let Some(ref mut st) = ex.stacktrace {
            scrub_stacktrace(st);
        }
        if let Some(ref mut st) = ex.raw_stacktrace {
            scrub_stacktrace(st);
        }
    }

    Some(event)
}

fn scrub_stacktrace(st: &mut Stacktrace) {
    for frame in st.frames.iter_mut() {
        if let Some(ref mut p) = frame.abs_path {
            *p = scrub_pii(p);
        }
        if let Some(ref mut p) = frame.filename {
            *p = scrub_pii(p);
        }
    }
}

fn install_panic_hook() {
    if PREVIOUS_HOOK.get().is_some() {
        return;
    }
    let prev = panic::take_hook();
    PREVIOUS_HOOK
        .set(Box::new(move |info: &panic::PanicHookInfo<'_>| {
            handle_panic(info);
            prev(info);
        }))
        .ok();
    panic::set_hook(Box::new(|info| {
        if let Some(hook) = PREVIOUS_HOOK.get() {
            hook(info);
        }
    }));
}

fn handle_panic(info: &panic::PanicHookInfo<'_>) {
    if !CRASH_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "panic occurred".to_string()
    };

    let location = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()));

    let mut message = format!("panic: {}", scrub_pii(&payload));
    if let Some(loc) = location {
        message.push_str(&format!(" at {}", scrub_pii(&loc)));
    }

    if TEST_MODE.load(Ordering::Relaxed) {
        write_local_crash(&message);
    } else if sentry::Hub::current().client().is_some() {
        sentry::capture_message(&message, Level::Fatal);
    }
}

fn write_local_crash(message: &str) {
    let dir = crash_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        log::error!("Failed to create crash dir {:?}: {}", dir, e);
        return;
    }

    let name = format!(
        "crash_{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let path = dir.join(name);

    let mut file = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            log::error!("Failed to open crash file {:?}: {}", path, e);
            return;
        }
    };

    if let Err(e) = writeln!(file, "{}", message) {
        log::error!("Failed to write crash file: {}", e);
    } else {
        log::info!("Crash report written to {:?}", path);
    }
}

fn redact_dsn(dsn: String) -> String {
    if dsn.eq_ignore_ascii_case("test") || dsn.starts_with("test://") {
        return dsn;
    }
    match sentry::types::Dsn::from_str(&dsn) {
        Ok(parsed) => format!(
            "{}://...@{} /{}",
            parsed.scheme(),
            parsed.host(),
            parsed.project_id()
        ),
        Err(_) => "invalid".into(),
    }
}
