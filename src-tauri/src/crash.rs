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
    use sentry::protocol::Value;
    use sentry::protocol::{Context, Exception, Frame, OsContext, Request, User, Values};
    use std::borrow::Cow;
    use std::sync::Mutex;

    /// Serializes tests that mutate the global `CRASH_ENABLED` flag.
    static CRASH_TEST_MUTEX: Mutex<()> = Mutex::new(());

    /// Helper that locks the test mutex, recovering from any prior poison.
    macro_rules! crash_mutex_guard {
        () => {
            CRASH_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
        };
    }

    #[test]
    fn empty_dsn_initialization_does_not_panic() {
        let _guard = crash_mutex_guard!();
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
        let _guard = crash_mutex_guard!();
        init_crash_reporting(None);
        let st = status();
        assert!(!st.enabled);
        assert!(!st.test_mode);
        assert_eq!(st.dsn, None);
    }

    #[test]
    fn empty_dsn_is_treated_as_disabled() {
        let _guard = crash_mutex_guard!();
        init_crash_reporting(Some("   ".to_string()));
        let st = status();
        assert!(!st.enabled);
        assert!(!st.test_mode);
        assert_eq!(st.dsn, None);
    }

    #[test]
    fn test_dsn_enables_local_test_mode() {
        let _guard = crash_mutex_guard!();
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
        let input =
            "IP 192.168.0.1 serial ABC123456789 MAC 00:1A:2B:3C:4D:5E path C:\\Users\\Me\\file.txt";
        let out = scrub_pii(input);
        assert!(out.contains("<MAC>"));
        assert!(out.contains("<PATH>"));
        assert!(out.contains("<IP>"));
        assert!(out.contains("<SERIAL>"));
    }

    // -----------------------------------------------------------------------
    // CrashReportingStatus serialization
    // -----------------------------------------------------------------------

    #[test]
    fn crash_reporting_status_serializes_expected_fields() {
        let st = CrashReportingStatus {
            enabled: true,
            test_mode: false,
            dsn: Some("test".to_string()),
        };
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(json.contains("\"enabled\":true"));
        assert!(json.contains("\"test_mode\":false"));
        assert!(json.contains("\"dsn\":\"test\""));
    }

    #[test]
    fn crash_reporting_status_serializes_none_dsn() {
        let st = CrashReportingStatus {
            enabled: false,
            test_mode: false,
            dsn: None,
        };
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(json.contains("\"enabled\":false"));
        assert!(json.contains("\"dsn\":null"));
    }

    #[test]
    fn crash_reporting_status_clones_correctly() {
        let st = CrashReportingStatus {
            enabled: true,
            test_mode: true,
            dsn: Some("test".to_string()),
        };
        let cloned = st.clone();
        assert_eq!(st.enabled, cloned.enabled);
        assert_eq!(st.test_mode, cloned.test_mode);
        assert_eq!(st.dsn, cloned.dsn);
    }

    // -----------------------------------------------------------------------
    // DSN validation
    // -----------------------------------------------------------------------

    #[test]
    fn validate_dsn_rejects_empty_string() {
        assert!(!validate_dsn(""));
    }

    #[test]
    fn validate_dsn_accepts_test_variants() {
        assert!(validate_dsn("test"));
        assert!(validate_dsn("TEST"));
        assert!(validate_dsn("Test"));
        assert!(validate_dsn("test://anything"));
    }

    #[test]
    fn validate_dsn_accepts_valid_sentry_dsn() {
        let dsn = "https://public@o447951.ingest.sentry.io/5439417";
        assert!(validate_dsn(dsn));
    }

    #[test]
    fn validate_dsn_rejects_invalid_formats() {
        assert!(!validate_dsn("not a dsn"));
        assert!(!validate_dsn("http://"));
        assert!(!validate_dsn("ftp://example.com"));
        assert!(!validate_dsn("garbage"));
        assert!(!validate_dsn("https://"));
    }

    // -----------------------------------------------------------------------
    // is_test_dsn helper
    // -----------------------------------------------------------------------

    #[test]
    fn is_test_dsn_handles_none() {
        assert!(!is_test_dsn(None));
    }

    #[test]
    fn is_test_dsn_recognizes_test_literal_case_insensitive() {
        assert!(is_test_dsn(Some("test")));
        assert!(is_test_dsn(Some("TEST")));
        assert!(is_test_dsn(Some("TeSt")));
    }

    #[test]
    fn is_test_dsn_recognizes_test_scheme() {
        assert!(is_test_dsn(Some("test://foo")));
        assert!(is_test_dsn(Some("test://")));
    }

    #[test]
    fn is_test_dsn_rejects_other_values() {
        assert!(!is_test_dsn(Some("")));
        assert!(!is_test_dsn(Some("other")));
        assert!(!is_test_dsn(Some(
            "https://o447951.ingest.sentry.io/5439417"
        )));
    }

    // -----------------------------------------------------------------------
    // PII scrubbing
    // -----------------------------------------------------------------------

    #[test]
    fn scrub_pii_passes_through_clean_text() {
        let input = "nothing sensitive here";
        assert_eq!(scrub_pii(input), input);
    }

    #[test]
    fn scrub_pii_handles_empty_string() {
        assert_eq!(scrub_pii(""), "");
    }

    #[test]
    fn scrub_pii_redacts_mac_addresses() {
        let out = scrub_pii("mac 00:1A:2B:3C:4D:5E and 01-23-45-67-89-ab");
        assert!(out.contains("<MAC>"));
        assert!(!out.contains("00:1A:2B"));
        assert!(!out.contains("01-23-45"));
    }

    #[test]
    fn scrub_pii_redacts_ipv4_addresses() {
        let out = scrub_pii("server 192.168.0.1 and 10.0.0.255");
        assert!(out.contains("<IP>"));
        assert!(!out.contains("192.168.0.1"));
        assert!(!out.contains("10.0.0.255"));
    }

    #[test]
    fn scrub_pii_redacts_windows_paths() {
        let out = scrub_pii("file at C:\\Users\\Me\\secret.txt");
        assert!(out.contains("<PATH>"));
        assert!(!out.contains("C:\\Users"));
    }

    #[test]
    fn scrub_pii_redacts_unix_paths() {
        let out = scrub_pii("config /etc/passwd and /home/user/data");
        assert!(out.contains("<PATH>"));
        assert!(!out.contains("/etc/passwd"));
        assert!(!out.contains("/home/user"));
    }

    #[test]
    fn scrub_pii_redacts_long_serial_numbers() {
        let out = scrub_pii("serial ABC123456789 and XYZ999888777");
        assert!(out.contains("<SERIAL>"));
        assert!(!out.contains("ABC123456789"));
        assert!(!out.contains("XYZ999888777"));
    }

    #[test]
    fn scrub_pii_does_not_redact_short_alphanumeric() {
        // Tokens shorter than 12 chars should be left alone.
        let out = scrub_pii("id ABC123");
        assert_eq!(out, "id ABC123");
    }

    #[test]
    fn scrub_pii_redacts_multiple_patterns_at_once() {
        let input = "ip 192.168.1.1 mac AA:BB:CC:DD:EE:FF serial DEADBEEF1234";
        let out = scrub_pii(input);
        assert!(out.contains("<IP>"));
        assert!(out.contains("<MAC>"));
        assert!(out.contains("<SERIAL>"));
    }

    // -----------------------------------------------------------------------
    // redact_dsn helper
    // -----------------------------------------------------------------------

    #[test]
    fn redact_dsn_passes_through_test_literal() {
        assert_eq!(redact_dsn("test".to_string()), "test");
    }

    #[test]
    fn redact_dsn_passes_through_test_scheme() {
        assert_eq!(redact_dsn("test://foo".to_string()), "test://foo");
    }

    #[test]
    fn redact_dsn_redacts_valid_sentry_dsn() {
        let dsn = "https://public@o447951.ingest.sentry.io/5439417";
        let redacted = redact_dsn(dsn.to_string());
        assert!(!redacted.contains("public"));
        assert!(redacted.contains("https"));
        assert!(redacted.contains("o447951.ingest.sentry.io"));
        assert!(redacted.contains("5439417"));
    }

    #[test]
    fn redact_dsn_returns_invalid_for_garbage() {
        assert_eq!(redact_dsn("garbage".to_string()), "invalid");
    }

    // -----------------------------------------------------------------------
    // before_send event payload building (no network)
    // -----------------------------------------------------------------------

    #[test]
    fn before_send_returns_none_when_disabled() {
        let _guard = crash_mutex_guard!();
        CRASH_ENABLED.store(false, Ordering::SeqCst);
        let event = Event::default();
        let result = before_send(event);
        assert!(result.is_none());
    }

    #[test]
    fn before_send_strips_pii_metadata_fields_when_enabled() {
        let _guard = crash_mutex_guard!();
        CRASH_ENABLED.store(true, Ordering::SeqCst);
        let mut event = Event::default();
        event.server_name = Some(Cow::Owned("my-server".to_string()));
        event.environment = Some(Cow::Owned("production".to_string()));
        event.release = Some(Cow::Owned("1.0.0".to_string()));
        event.dist = Some(Cow::Owned("dist1".to_string()));
        event.user = Some(User::default());
        event.request = Some(Request::default());
        event.contexts.insert(
            "os".to_string(),
            Context::Os(Box::new(OsContext::default())),
        );
        event.tags.insert("host".to_string(), "my-host".to_string());
        event
            .extra
            .insert("note".to_string(), Value::String("secret".to_string()));

        let result = before_send(event).expect("event should be returned when enabled");
        assert!(result.server_name.is_none());
        assert!(result.environment.is_none());
        assert!(result.release.is_none());
        assert!(result.dist.is_none());
        assert!(result.user.is_none());
        assert!(result.request.is_none());
        assert!(result.contexts.is_empty());
        assert!(result.tags.is_empty());
        assert!(result.extra.is_empty());
    }

    #[test]
    fn before_send_scrubs_message_pii() {
        let _guard = crash_mutex_guard!();
        CRASH_ENABLED.store(true, Ordering::SeqCst);
        let mut event = Event::default();
        event.message = Some("error at 192.168.0.1 for C:\\Users\\me\\file.txt".to_string());
        let result = before_send(event).expect("event returned");
        let msg = result.message.expect("message present");
        assert!(msg.contains("<IP>"));
        assert!(msg.contains("<PATH>"));
        assert!(!msg.contains("192.168.0.1"));
        assert!(!msg.contains("C:\\Users"));
    }

    #[test]
    fn before_send_scrubs_exception_value_and_stacktrace() {
        let _guard = crash_mutex_guard!();
        CRASH_ENABLED.store(true, Ordering::SeqCst);
        let mut event = Event::default();
        let mut ex = Exception::default();
        ex.value = Some("panic involving 10.0.0.5".to_string());
        let mut frame = Frame::default();
        frame.abs_path = Some("C:\\Users\\bob\\src\\main.rs".to_string());
        frame.filename = Some("main.rs".to_string());
        let st = Stacktrace {
            frames: vec![frame],
            ..Default::default()
        };
        ex.stacktrace = Some(st);
        event.exception = Values::from(vec![ex]);

        let result = before_send(event).expect("event returned");
        let ex_out = &result.exception.values[0];
        let val = ex_out.value.as_ref().expect("exception value present");
        assert!(val.contains("<IP>"));
        assert!(!val.contains("10.0.0.5"));
        let st_out = ex_out.stacktrace.as_ref().expect("stacktrace present");
        let frame_out = &st_out.frames[0];
        let abs = frame_out.abs_path.as_ref().expect("abs_path present");
        assert!(abs.contains("<PATH>"));
        assert!(!abs.contains("C:\\Users\\bob"));
    }

    #[test]
    fn before_send_scrubs_raw_stacktrace() {
        let _guard = crash_mutex_guard!();
        CRASH_ENABLED.store(true, Ordering::SeqCst);
        let mut event = Event::default();
        let mut ex = Exception::default();
        let mut frame = Frame::default();
        frame.abs_path = Some("/home/alice/app/index.js".to_string());
        ex.raw_stacktrace = Some(Stacktrace {
            frames: vec![frame],
            ..Default::default()
        });
        event.exception = Values::from(vec![ex]);

        let result = before_send(event).expect("event returned");
        let ex_out = &result.exception.values[0];
        let st_out = ex_out
            .raw_stacktrace
            .as_ref()
            .expect("raw stacktrace present");
        let abs = st_out.frames[0]
            .abs_path
            .as_ref()
            .expect("abs_path present");
        assert!(abs.contains("<PATH>"));
        assert!(!abs.contains("/home/alice"));
    }

    // -----------------------------------------------------------------------
    // scrub_stacktrace helper
    // -----------------------------------------------------------------------

    #[test]
    fn scrub_stacktrace_redacts_all_frames() {
        let mut st = Stacktrace {
            frames: vec![
                Frame {
                    abs_path: Some("C:\\Users\\a\\f1.rs".to_string()),
                    filename: Some("f1.rs".to_string()),
                    ..Default::default()
                },
                Frame {
                    abs_path: Some("/home/b/f2.js".to_string()),
                    filename: Some("f2.js".to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        scrub_stacktrace(&mut st);
        for frame in &st.frames {
            let abs = frame.abs_path.as_ref().expect("abs_path present");
            assert!(abs.contains("<PATH>"));
            assert!(!abs.contains("Users"));
            assert!(!abs.contains("home"));
        }
    }

    // -----------------------------------------------------------------------
    // crash_dir
    // -----------------------------------------------------------------------

    #[test]
    fn crash_dir_ends_with_oxide_link_crashes() {
        let dir = crash_dir();
        let components: Vec<_> = dir
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        assert!(components.iter().any(|c| c == "OxideLink"));
        assert!(components.iter().any(|c| c == "crashes"));
    }

    // -----------------------------------------------------------------------
    // set_crash_reporting_enabled state transitions
    // -----------------------------------------------------------------------

    #[test]
    fn set_crash_reporting_disabled_clears_state() {
        let _guard = crash_mutex_guard!();
        let st = set_crash_reporting_enabled(false, None);
        assert!(!st.enabled);
        assert!(!st.test_mode);
        assert_eq!(st.dsn, None);
    }

    #[test]
    fn set_crash_reporting_enabled_with_test_dsn() {
        let _guard = crash_mutex_guard!();
        let st = set_crash_reporting_enabled(true, Some("test".to_string()));
        assert!(st.enabled);
        assert!(st.test_mode);
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
