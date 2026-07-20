//! In-game overlay window (Wave 4).
//!
//! Provides a transparent, click-through Webview2 overlay for in-game controller
//! status, FPS-style metrics, and quick profile switching.  The overlay window is
//! stored in a module-level singleton so the Tauri commands can show/hide/update it
//! without adding fields to `AppConfig` or `SharedState`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Position, Size, State, WebviewUrl,
    WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::state::{AppCtx, ControllerState, SharedState};

const OVERLAY_WINDOW_LABEL: &str = "overlay";
const OVERLAY_HTML: &str = "overlay.html";

/// Minimum width/height of the overlay content area, in logical pixels.
const BASE_WIDTH: f64 = 320.0;
const BASE_HEIGHT: f64 = 180.0;

/// Distance from monitor edges when snapped to a corner.
const CORNER_PADDING: f64 = 12.0;

/// Overlay runtime configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    /// Whether the in-game overlay can be shown at all.
    pub enabled: bool,
    /// Keyboard shortcut used to toggle the overlay, e.g. `"Shift+F11"`.
    pub toggle_hotkey: String,
    /// Background opacity of the overlay panel, 0.0–1.0.
    pub opacity: f32,
    /// Initial position: `"top-left"`, `"top-right"`, `"bottom-left"`,
    /// `"bottom-right"`, or `"center"`.
    pub position: String,
    /// Show the battery bar in the overlay.
    pub show_battery: bool,
    /// Show the active profile name in the overlay.
    pub show_profile: bool,
    /// Show a simple FPS counter in the overlay.
    pub show_fps: bool,
    /// UI scale multiplier for the overlay window and content.
    pub scale: f32,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            toggle_hotkey: "Shift+F11".into(),
            opacity: 0.9,
            position: "top-left".into(),
            show_battery: true,
            show_profile: true,
            show_fps: false,
            scale: 1.0,
        }
    }
}

impl OverlayConfig {
    /// Validate configuration values without mutating.
    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err(format!("opacity {} out of range [0.0, 1.0]", self.opacity));
        }
        if !matches!(
            self.position.as_str(),
            "top-left" | "top-right" | "bottom-left" | "bottom-right" | "center"
        ) {
            return Err(format!("position '{}' is invalid", self.position));
        }
        if self.scale <= 0.0 || self.scale > 4.0 {
            return Err(format!("scale {} out of range (0.0, 4.0]", self.scale));
        }
        parse_hotkey(&self.toggle_hotkey).map_err(|e| format!("toggle_hotkey invalid: {}", e))?;
        Ok(())
    }

    /// Clamp/normalize values in place so the config is always safe to use.
    pub fn sanitize(&mut self) {
        self.opacity = self.opacity.clamp(0.0, 1.0);
        self.scale = self.scale.clamp(0.25, 4.0);
        if !matches!(
            self.position.as_str(),
            "top-left" | "top-right" | "bottom-left" | "bottom-right" | "center"
        ) {
            self.position = "top-left".into();
        }
        if parse_hotkey(&self.toggle_hotkey).is_err() {
            self.toggle_hotkey = "Shift+F11".into();
        }
        self.toggle_hotkey = self
            .toggle_hotkey
            .split('+')
            .map(|s| s.trim().to_string())
            .collect::<Vec<_>>()
            .join("+");
    }
}

/// Payload emitted to the overlay window whenever the controller state changes.
#[derive(Serialize, Clone)]
struct OverlayStatePayload {
    state: ControllerState,
    profile_name: Option<String>,
}

/// Manages the lifecycle and content of the overlay webview window.
pub struct OverlayWindow {
    app_handle: AppHandle,
    config: Mutex<OverlayConfig>,
    visible: AtomicBool,
}

impl OverlayWindow {
    /// Create a new overlay manager with the given handle and configuration.
    pub fn new(app_handle: AppHandle, config: OverlayConfig) -> Self {
        Self {
            app_handle,
            config: Mutex::new(config),
            visible: AtomicBool::new(false),
        }
    }

    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::SeqCst)
    }

    /// Compute the overlay's initial logical position for the configured anchor.
    fn resolve_position(&self, width: f64, height: f64) -> (f64, f64) {
        let fallback = || (100.0, 100.0);
        let monitor = match self.app_handle.primary_monitor() {
            Ok(Some(m)) => m,
            _ => return fallback(),
        };

        let sf = monitor.scale_factor();
        let mw = monitor.size().width as f64 / sf;
        let mh = monitor.size().height as f64 / sf;
        let mx = monitor.position().x as f64 / sf;
        let my = monitor.position().y as f64 / sf;

        let cfg = self.config.lock();
        let (x, y) = match cfg.position.as_str() {
            "top-right" => (mx + mw - width - CORNER_PADDING, my + CORNER_PADDING),
            "bottom-left" => (mx + CORNER_PADDING, my + mh - height - CORNER_PADDING),
            "bottom-right" => (
                mx + mw - width - CORNER_PADDING,
                my + mh - height - CORNER_PADDING,
            ),
            "center" => (mx + (mw - width) / 2.0, my + (mh - height) / 2.0),
            _ => (mx + CORNER_PADDING, my + CORNER_PADDING),
        };

        (x.max(0.0), y.max(0.0))
    }

    /// Show the overlay window, creating it if necessary.
    pub fn show(&self) -> Result<(), String> {
        let cfg = self.config.lock().clone();
        if !cfg.enabled {
            return Err("overlay is disabled in config".into());
        }

        if let Some(window) = self.app_handle.get_webview_window(OVERLAY_WINDOW_LABEL) {
            window.show().map_err(|e| e.to_string())?;
            self.visible.store(true, Ordering::SeqCst);
            return Ok(());
        }

        let width = BASE_WIDTH * cfg.scale as f64;
        let height = BASE_HEIGHT * cfg.scale as f64;
        let (x, y) = self.resolve_position(width, height);

        let window = WebviewWindowBuilder::new(
            &self.app_handle,
            OVERLAY_WINDOW_LABEL,
            WebviewUrl::App(OVERLAY_HTML.into()),
        )
        .transparent(true)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .inner_size(width, height)
        .position(x, y)
        .focused(false)
        .focusable(false)
        .title("OxideLink Overlay")
        .shadow(false)
        .build()
        .map_err(|e| e.to_string())?;

        // The overlay panel itself receives cursor events; transparent body areas
        // pass events through via CSS `pointer-events` rules.
        let _ = window.set_ignore_cursor_events(false);

        self.visible.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Hide the overlay window.
    pub fn hide(&self) -> Result<(), String> {
        if let Some(window) = self.app_handle.get_webview_window(OVERLAY_WINDOW_LABEL) {
            window.hide().map_err(|e| e.to_string())?;
        }
        self.visible.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Toggle visibility and return the new visibility state.
    pub fn toggle(&mut self) -> bool {
        if self.is_visible() {
            if let Err(e) = self.hide() {
                log::warn!("Failed to hide overlay: {}", e);
            }
            false
        } else {
            match self.show() {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("Failed to show overlay: {}", e);
                    false
                }
            }
        }
    }

    /// Emit the latest controller state to the overlay webview.
    pub fn update_state(&self, state: &ControllerState, profile_name: Option<String>) {
        if !self.is_visible() {
            return;
        }

        let payload = OverlayStatePayload {
            state: state.clone(),
            profile_name,
        };

        if let Some(window) = self.app_handle.get_webview_window(OVERLAY_WINDOW_LABEL) {
            if let Err(e) = window.emit("overlay-state", payload) {
                log::warn!("Failed to emit overlay-state: {}", e);
            }
        }
    }

    /// Update the overlay config and apply size/position changes if the window exists.
    pub fn set_config(&mut self, config: OverlayConfig) {
        *self.config.lock() = config;

        if !self.is_visible() {
            return;
        }

        if let Some(window) = self.app_handle.get_webview_window(OVERLAY_WINDOW_LABEL) {
            let width = BASE_WIDTH * self.config.lock().scale as f64;
            let height = BASE_HEIGHT * self.config.lock().scale as f64;
            let (x, y) = self.resolve_position(width, height);

            let _ = window.set_size(Size::Logical(LogicalSize { width, height }));
            let _ = window.set_position(Position::Logical(LogicalPosition { x, y }));
        }
    }
}

/// Lightweight overlay runtime state stored in `SharedState`.
/// Keeps Tauri runtime types out of the shared state so unit tests do not
/// need to link WebView2 / global-shortcut runtime entry points.
#[derive(Clone, Debug, Default)]
pub struct OverlayState {
    pub config: OverlayConfig,
    pub visible: bool,
}

/// Module-level singleton for the actual Tauri webview window.
static OVERLAY: OnceLock<Mutex<OverlayWindow>> = OnceLock::new();

/// Path to the overlay config file: `%APPDATA%\OxideLink\overlay.json`.
pub fn overlay_config_path() -> PathBuf {
    let mut path = dirs_next::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("OxideLink");
    path.push("overlay.json");
    path
}

/// Load overlay config from disk, falling back to defaults.
pub fn load_overlay_config() -> OverlayConfig {
    let path = overlay_config_path();
    if path.exists() {
        if let Ok(json) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<OverlayConfig>(&json) {
                Ok(mut cfg) => {
                    cfg.sanitize();
                    return cfg;
                }
                Err(e) => log::warn!("Failed to parse overlay config {:?}: {}", path, e),
            }
        }
    }
    OverlayConfig::default()
}

/// Save overlay config to disk.
pub fn save_overlay_config(config: &OverlayConfig) -> Result<PathBuf, String> {
    let path = overlay_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create overlay config dir {:?}: {}", parent, e))?;
    }

    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize overlay config: {}", e))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("Failed to write overlay config to {:?}: {}", path, e))?;

    log::info!("Overlay config saved to {:?}", path);
    Ok(path)
}

/// Parse a hotkey string such as `"Shift+F11"` or `"Ctrl+Alt+T"` into
/// normalized modifiers and a key.  Used for validation and (eventually)
/// `tauri-plugin-global-shortcut` registration.
pub fn parse_hotkey(hotkey: &str) -> Result<(Vec<String>, String), String> {
    if hotkey.trim().is_empty() {
        return Err("hotkey is empty".into());
    }

    let raw_parts: Vec<&str> = hotkey.split('+').collect();
    for part in &raw_parts {
        if part.trim().is_empty() {
            return Err("hotkey contains an empty segment".into());
        }
    }

    let parts: Vec<String> = raw_parts
        .into_iter()
        .map(|s| s.trim().to_lowercase())
        .collect();

    let key = parts.last().unwrap().to_ascii_uppercase();
    let modifiers: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|m| match m.as_str() {
            "shift" => Ok("Shift".to_string()),
            "ctrl" | "control" => Ok("Control".to_string()),
            "alt" | "option" | "menu" => Ok("Alt".to_string()),
            "super" | "command" | "cmd" | "win" | "meta" => Ok("Super".to_string()),
            other => Err(format!("unknown modifier '{}'", other)),
        })
        .collect::<Result<Vec<_>, _>>()?;

    if key.is_empty() {
        return Err("hotkey key is empty".into());
    }

    Ok((modifiers, key))
}

// ---------------------------------------------------------------------------
// Initialization helpers
// ---------------------------------------------------------------------------

/// Register (or re-register) the overlay toggle global shortcut.
/// Unregisters any existing shortcuts first so config changes take effect.
pub fn register_overlay_hotkey(shared: &SharedState, app: &AppHandle) -> Result<(), String> {
    let cfg = shared
        .overlay
        .lock()
        .as_ref()
        .map(|o| o.config.clone())
        .unwrap_or_else(load_overlay_config);
    if !cfg.enabled || cfg.toggle_hotkey.trim().is_empty() {
        let _ = app.global_shortcut().unregister_all();
        return Ok(());
    }

    let shortcut: Shortcut = cfg.toggle_hotkey.parse().map_err(|e| {
        format!(
            "failed to parse overlay hotkey '{}': {}",
            cfg.toggle_hotkey, e
        )
    })?;

    let _ = app.global_shortcut().unregister_all();

    let app_for_handler = app.clone();
    app.global_shortcut()
        .on_shortcut(shortcut, move |_app, _shortcut, event| {
            if event.state == ShortcutState::Pressed {
                let app = app_for_handler.clone();
                tauri::async_runtime::spawn(async move {
                    let ctx = app.state::<AppCtx>();
                    let _ = toggle_overlay_inner(&ctx.shared);
                });
            }
        })
        .map_err(|e| {
            format!(
                "failed to register overlay hotkey '{}': {}",
                cfg.toggle_hotkey, e
            )
        })?;

    Ok(())
}

/// Initialize the overlay window from the application setup hook.
pub fn init_overlay(shared: &SharedState, app: &AppHandle) {
    let config = load_overlay_config();
    *shared.overlay.lock() = Some(OverlayState {
        config: config.clone(),
        visible: false,
    });
    let _ = OVERLAY.set(Mutex::new(OverlayWindow::new(app.clone(), config)));
    if let Err(e) = register_overlay_hotkey(shared, app) {
        log::info!("Overlay hotkey not active: {}", e);
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

fn toggle_overlay_inner(shared: &SharedState) -> bool {
    let visible = OVERLAY
        .get()
        .map(|mutex| {
            let mut overlay = mutex.lock();
            overlay.toggle()
        })
        .unwrap_or(false);
    if let Some(st) = shared.overlay.lock().as_mut() {
        st.visible = visible;
    }
    visible
}

/// Show or hide the overlay window.
#[tauri::command]
pub fn toggle_overlay(ctx: State<'_, AppCtx>) -> bool {
    toggle_overlay_inner(&ctx.shared)
}

/// Get the current overlay configuration from disk.
#[tauri::command]
pub fn get_overlay_config() -> OverlayConfig {
    load_overlay_config()
}

/// Update and persist overlay configuration.
#[tauri::command]
pub fn set_overlay_config(
    ctx: State<'_, AppCtx>,
    app: AppHandle,
    mut config: OverlayConfig,
) -> OverlayConfig {
    config.sanitize();
    if let Err(e) = save_overlay_config(&config) {
        log::warn!("Failed to save overlay config: {}", e);
    }
    {
        let mut st = ctx.shared.overlay.lock();
        if let Some(ref mut state) = st.as_mut() {
            state.config = config.clone();
        }
    }
    if let Some(mutex) = OVERLAY.get() {
        let mut overlay = mutex.lock();
        overlay.set_config(config.clone());
    }
    if let Err(e) = register_overlay_hotkey(&ctx.shared, &app) {
        log::warn!("{}", e);
    }
    config
}

/// Push the latest controller state to the overlay webview.
#[tauri::command]
pub fn update_overlay_state(
    _ctx: State<'_, AppCtx>,
    state: ControllerState,
    profile_name: Option<String>,
) -> bool {
    OVERLAY
        .get()
        .map(|mutex| {
            let overlay = mutex.lock();
            if overlay.is_visible() {
                overlay.update_state(&state, profile_name);
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
}

/// Non-command helper the backend can call from the IPC event loop to keep the
/// overlay in sync with the live controller state.
pub fn emit_overlay_state(
    _shared: &SharedState,
    state: &ControllerState,
    profile_name: Option<String>,
) {
    if let Some(mutex) = OVERLAY.get() {
        let overlay = mutex.lock();
        overlay.update_state(state, profile_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ControllerState;
    use std::sync::{Mutex as StdMutex, OnceLock};

    /// Serializes tests that touch the real overlay config file on disk so they
    /// do not race with each other when cargo runs tests in parallel.
    static FILE_TEST_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();

    fn file_lock() -> &'static StdMutex<()> {
        FILE_TEST_LOCK.get_or_init(|| StdMutex::new(()))
    }

    // -----------------------------------------------------------------------
    // OverlayConfig defaults & field coverage
    // -----------------------------------------------------------------------

    #[test]
    fn default_overlay_config() {
        let cfg = OverlayConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.toggle_hotkey, "Shift+F11");
        assert_eq!(cfg.position, "top-left");
        assert!(cfg.show_battery);
        assert!(cfg.show_profile);
        assert!(!cfg.show_fps);
        assert!((cfg.opacity - 0.9).abs() < f32::EPSILON);
        assert!((cfg.scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn default_overlay_config_all_fields_explicit() {
        let cfg = OverlayConfig::default();
        // Verify every field individually for full field coverage.
        assert_eq!(cfg.enabled, false);
        assert_eq!(cfg.toggle_hotkey, "Shift+F11");
        assert_eq!(cfg.opacity, 0.9);
        assert_eq!(cfg.position, "top-left");
        assert_eq!(cfg.show_battery, true);
        assert_eq!(cfg.show_profile, true);
        assert_eq!(cfg.show_fps, false);
        assert_eq!(cfg.scale, 1.0);
    }

    #[test]
    fn overlay_config_is_clone_and_eq() {
        let a = OverlayConfig::default();
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn overlay_config_debug_repr_contains_fields() {
        let cfg = OverlayConfig::default();
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("OverlayConfig"));
        assert!(dbg.contains("enabled"));
        assert!(dbg.contains("toggle_hotkey"));
        assert!(dbg.contains("opacity"));
        assert!(dbg.contains("position"));
    }

    // -----------------------------------------------------------------------
    // Serialization round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn config_serialization_roundtrip() {
        let cfg = OverlayConfig {
            enabled: true,
            toggle_hotkey: "Ctrl+Shift+F12".into(),
            opacity: 0.5,
            position: "bottom-right".into(),
            show_battery: false,
            show_profile: true,
            show_fps: true,
            scale: 1.5,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: OverlayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn config_serialization_roundtrip_all_positions() {
        for pos in [
            "top-left",
            "top-right",
            "bottom-left",
            "bottom-right",
            "center",
        ] {
            let cfg = OverlayConfig {
                position: pos.into(),
                ..OverlayConfig::default()
            };
            let json = serde_json::to_string(&cfg).unwrap();
            let parsed: OverlayConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(cfg, parsed, "roundtrip failed for position {}", pos);
        }
    }

    #[test]
    fn config_deserialize_with_serde_default_fills_missing_fields() {
        // An empty JSON object should produce the default config thanks to
        // `#[serde(default)]`.
        let parsed: OverlayConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, OverlayConfig::default());
    }

    #[test]
    fn config_deserialize_partial_json_uses_defaults_for_missing() {
        let json = r#"{"enabled": true, "opacity": 0.3}"#;
        let parsed: OverlayConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.enabled);
        assert!((parsed.opacity - 0.3).abs() < f32::EPSILON);
        // Remaining fields come from Default.
        assert_eq!(parsed.toggle_hotkey, "Shift+F11");
        assert_eq!(parsed.position, "top-left");
        assert!(parsed.show_battery);
        assert!(parsed.show_profile);
        assert!(!parsed.show_fps);
        assert!((parsed.scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn config_pretty_serialization_is_valid_json() {
        let cfg = OverlayConfig::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(json.contains("\"enabled\""));
        assert!(json.contains("\"toggle_hotkey\""));
        let parsed: OverlayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    #[test]
    fn config_validation_catches_invalid_values() {
        assert!(OverlayConfig {
            opacity: 1.5,
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
        assert!(OverlayConfig {
            position: "off-screen".into(),
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
        assert!(OverlayConfig {
            scale: 0.0,
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
        assert!(OverlayConfig {
            toggle_hotkey: "".into(),
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn config_validation_accepts_default() {
        assert!(OverlayConfig::default().validate().is_ok());
    }

    #[test]
    fn config_validation_accepts_all_valid_positions() {
        for pos in [
            "top-left",
            "top-right",
            "bottom-left",
            "bottom-right",
            "center",
        ] {
            let cfg = OverlayConfig {
                position: pos.into(),
                ..OverlayConfig::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "position '{}' should be valid",
                pos
            );
        }
    }

    #[test]
    fn config_validation_opacity_boundaries() {
        // 0.0 and 1.0 are inclusive boundaries.
        assert!(OverlayConfig {
            opacity: 0.0,
            ..OverlayConfig::default()
        }
        .validate()
        .is_ok());
        assert!(OverlayConfig {
            opacity: 1.0,
            ..OverlayConfig::default()
        }
        .validate()
        .is_ok());
        // Just outside the range.
        assert!(OverlayConfig {
            opacity: -0.01,
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
        assert!(OverlayConfig {
            opacity: 1.01,
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn config_validation_scale_boundaries() {
        // scale must be > 0.0 and <= 4.0
        assert!(OverlayConfig {
            scale: 0.001,
            ..OverlayConfig::default()
        }
        .validate()
        .is_ok());
        assert!(OverlayConfig {
            scale: 4.0,
            ..OverlayConfig::default()
        }
        .validate()
        .is_ok());
        assert!(OverlayConfig {
            scale: 0.0,
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
        assert!(OverlayConfig {
            scale: -1.0,
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
        assert!(OverlayConfig {
            scale: 4.01,
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn config_validation_invalid_hotkey_errors() {
        assert!(OverlayConfig {
            toggle_hotkey: "BogusMod+K".into(),
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
        assert!(OverlayConfig {
            toggle_hotkey: "   ".into(),
            ..OverlayConfig::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn config_validation_error_messages_are_descriptive() {
        let err = OverlayConfig {
            opacity: 2.5,
            ..OverlayConfig::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("opacity"));
        assert!(err.contains("2.5"));

        let err = OverlayConfig {
            position: "middle".into(),
            ..OverlayConfig::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("position"));
        assert!(err.contains("middle"));

        let err = OverlayConfig {
            scale: 5.0,
            ..OverlayConfig::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("scale"));
        assert!(err.contains("5"));
    }

    // -----------------------------------------------------------------------
    // Sanitize
    // -----------------------------------------------------------------------

    #[test]
    fn config_sanitize_fixes_invalid_values() {
        let mut cfg = OverlayConfig {
            opacity: 2.0,
            scale: 10.0,
            position: "invalid".into(),
            toggle_hotkey: "++bad".into(),
            ..OverlayConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.opacity, 1.0);
        assert_eq!(cfg.scale, 4.0);
        assert_eq!(cfg.position, "top-left");
        assert_eq!(cfg.toggle_hotkey, "Shift+F11");
    }

    #[test]
    fn config_sanitize_clamps_opacity_low() {
        let mut cfg = OverlayConfig {
            opacity: -5.0,
            ..OverlayConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.opacity, 0.0);
    }

    #[test]
    fn config_sanitize_clamps_scale_low() {
        let mut cfg = OverlayConfig {
            scale: 0.0,
            ..OverlayConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.scale, 0.25);
    }

    #[test]
    fn config_sanitize_clamps_scale_high() {
        let mut cfg = OverlayConfig {
            scale: 100.0,
            ..OverlayConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.scale, 4.0);
    }

    #[test]
    fn config_sanitize_keeps_valid_position() {
        for pos in ["top-right", "bottom-left", "bottom-right", "center"] {
            let mut cfg = OverlayConfig {
                position: pos.into(),
                ..OverlayConfig::default()
            };
            cfg.sanitize();
            assert_eq!(cfg.position, pos);
        }
    }

    #[test]
    fn config_sanitize_normalizes_hotkey_whitespace() {
        let mut cfg = OverlayConfig {
            toggle_hotkey: "  Ctrl +  Shift + F9 ".into(),
            ..OverlayConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.toggle_hotkey, "Ctrl+Shift+F9");
    }

    #[test]
    fn config_sanitize_keeps_valid_hotkey() {
        let mut cfg = OverlayConfig {
            toggle_hotkey: "Alt+F4".into(),
            ..OverlayConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.toggle_hotkey, "Alt+F4");
    }

    #[test]
    fn config_sanitize_result_is_valid() {
        let mut cfg = OverlayConfig {
            opacity: 99.0,
            scale: -1.0,
            position: "nowhere".into(),
            toggle_hotkey: "garbage++".into(),
            ..OverlayConfig::default()
        };
        cfg.sanitize();
        assert!(cfg.validate().is_ok(), "sanitized config should validate");
    }

    #[test]
    fn config_sanitize_does_not_touch_valid_config() {
        let mut cfg = OverlayConfig {
            enabled: true,
            toggle_hotkey: "Ctrl+Alt+T".into(),
            opacity: 0.5,
            position: "center".into(),
            show_battery: true,
            show_profile: false,
            show_fps: true,
            scale: 2.0,
        };
        let before = cfg.clone();
        cfg.sanitize();
        assert_eq!(cfg, before);
    }

    // -----------------------------------------------------------------------
    // parse_hotkey — valid formats, invalid formats, edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn hotkey_parsing() {
        let (mods, key) = parse_hotkey("Shift+F11").unwrap();
        assert_eq!(mods, vec!["Shift"]);
        assert_eq!(key, "F11");

        let (mods, key) = parse_hotkey("Ctrl+Alt+T").unwrap();
        assert_eq!(mods, vec!["Control", "Alt"]);
        assert_eq!(key, "T");

        assert!(parse_hotkey("").is_err());
        assert!(parse_hotkey("Shift+Unknown+K").is_err());
        assert!(parse_hotkey("+").is_err());
    }

    #[test]
    fn hotkey_normalization_and_validation() {
        let (mods, key) = parse_hotkey("  ctrl + shift + f10 ").unwrap();
        assert_eq!(mods, vec!["Control", "Shift"]);
        assert_eq!(key, "F10");

        assert!(parse_hotkey("Ctrl+").is_err());
        assert!(parse_hotkey("Shift++F").is_err());
    }

    #[test]
    fn hotkey_single_key_no_modifiers() {
        let (mods, key) = parse_hotkey("F12").unwrap();
        assert!(mods.is_empty());
        assert_eq!(key, "F12");
    }

    #[test]
    fn hotkey_single_letter_key() {
        let (mods, key) = parse_hotkey("K").unwrap();
        assert!(mods.is_empty());
        assert_eq!(key, "K");
    }

    #[test]
    fn hotkey_lowercase_key_is_uppercased() {
        let (mods, key) = parse_hotkey("ctrl+t").unwrap();
        assert_eq!(mods, vec!["Control"]);
        assert_eq!(key, "T");
    }

    #[test]
    fn hotkey_ctrl_alias() {
        let (mods, _) = parse_hotkey("Ctrl+K").unwrap();
        assert_eq!(mods, vec!["Control"]);

        let (mods, _) = parse_hotkey("Control+K").unwrap();
        assert_eq!(mods, vec!["Control"]);
    }

    #[test]
    fn hotkey_alt_aliases() {
        for alias in ["Alt", "Option", "Menu"] {
            let (mods, _) = parse_hotkey(&format!("{}+K", alias)).unwrap();
            assert_eq!(mods, vec!["Alt"], "alias {} should map to Alt", alias);
        }
    }

    #[test]
    fn hotkey_super_aliases() {
        for alias in ["Super", "Command", "Cmd", "Win", "Meta"] {
            let (mods, _) = parse_hotkey(&format!("{}+K", alias)).unwrap();
            assert_eq!(mods, vec!["Super"], "alias {} should map to Super", alias);
        }
    }

    #[test]
    fn hotkey_all_modifiers_combined() {
        let (mods, key) = parse_hotkey("Ctrl+Alt+Shift+Super+F5").unwrap();
        assert_eq!(mods, vec!["Control", "Alt", "Shift", "Super"]);
        assert_eq!(key, "F5");
    }

    #[test]
    fn hotkey_whitespace_only_is_error() {
        assert!(parse_hotkey("   ").is_err());
    }

    #[test]
    fn hotkey_leading_plus_is_error() {
        assert!(parse_hotkey("+K").is_err());
    }

    #[test]
    fn hotkey_trailing_plus_is_error() {
        assert!(parse_hotkey("Ctrl+K+").is_err());
    }

    #[test]
    fn hotkey_empty_string_is_error() {
        assert!(parse_hotkey("").is_err());
    }

    #[test]
    fn hotkey_only_plus_is_error() {
        assert!(parse_hotkey("+").is_err());
    }

    #[test]
    fn hotkey_multiple_plus_is_error() {
        assert!(parse_hotkey("Shift++F").is_err());
        assert!(parse_hotkey("+++").is_err());
    }

    #[test]
    fn hotkey_unknown_modifier_is_error_with_name() {
        let err = parse_hotkey("Bogus+K").unwrap_err();
        assert!(err.contains("unknown modifier"));
        assert!(err.contains("bogus"));
    }

    #[test]
    fn hotkey_case_insensitive_modifiers() {
        let (mods, _) = parse_hotkey("SHIFT+k").unwrap();
        assert_eq!(mods, vec!["Shift"]);

        let (mods, _) = parse_hotkey("sHiFt+k").unwrap();
        assert_eq!(mods, vec!["Shift"]);
    }

    #[test]
    fn hotkey_error_messages_are_descriptive() {
        let err = parse_hotkey("").unwrap_err();
        assert_eq!(err, "hotkey is empty");

        let err = parse_hotkey("Shift+").unwrap_err();
        assert_eq!(err, "hotkey contains an empty segment");
    }

    // -----------------------------------------------------------------------
    // OverlayState
    // -----------------------------------------------------------------------

    #[test]
    fn overlay_state_defaults_to_disabled_and_not_visible() {
        let st = OverlayState::default();
        assert!(!st.visible);
        assert!(!st.config.enabled);
        assert_eq!(st.config.position, "top-left");
        assert_eq!(st.config.toggle_hotkey, "Shift+F11");
    }

    #[test]
    fn overlay_state_all_fields() {
        let cfg = OverlayConfig {
            enabled: true,
            toggle_hotkey: "Ctrl+T".into(),
            opacity: 0.3,
            position: "center".into(),
            show_battery: false,
            show_profile: false,
            show_fps: true,
            scale: 2.0,
        };
        let st = OverlayState {
            config: cfg.clone(),
            visible: true,
        };
        assert!(st.visible);
        assert_eq!(st.config, cfg);
    }

    #[test]
    fn overlay_state_is_clone_and_debug() {
        let st = OverlayState::default();
        let cloned = st.clone();
        assert_eq!(cloned.visible, st.visible);
        assert_eq!(cloned.config, st.config);

        let dbg = format!("{:?}", st);
        assert!(dbg.contains("OverlayState"));
        assert!(dbg.contains("visible"));
    }

    #[test]
    fn overlay_state_visible_toggle_simulation() {
        // Simulate the state transitions that OverlayWindow::toggle would drive
        // without requiring a real AppHandle.
        let mut st = OverlayState::default();
        assert!(!st.visible);

        // show -> visible true
        st.visible = true;
        assert!(st.visible);

        // toggle -> hide
        st.visible = !st.visible;
        assert!(!st.visible);

        // toggle -> show
        st.visible = !st.visible;
        assert!(st.visible);
    }

    // -----------------------------------------------------------------------
    // OverlayStatePayload (emit_overlay_state event payload structure)
    // -----------------------------------------------------------------------

    #[test]
    fn config_payload_serialization_uses_controller_state() {
        let state = ControllerState::default();
        let payload = OverlayStatePayload {
            state: state.clone(),
            profile_name: Some("Default".into()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("profile_name"));
        assert!(json.contains("Default"));
    }

    #[test]
    fn payload_with_none_profile_name_serializes_null() {
        let state = ControllerState::default();
        let payload = OverlayStatePayload {
            state,
            profile_name: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"profile_name\":null"));
    }

    #[test]
    fn payload_with_some_profile_name_serializes_string() {
        let state = ControllerState::default();
        let payload = OverlayStatePayload {
            state,
            profile_name: Some("MyProfile".into()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"profile_name\":\"MyProfile\""));
    }

    #[test]
    fn payload_contains_controller_state_fields() {
        let state = ControllerState::default();
        let payload = OverlayStatePayload {
            state: state.clone(),
            profile_name: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        // ControllerState has a `connected` field.
        assert!(json.contains("\"connected\""));
        assert!(json.contains("\"battery_percent\""));
    }

    #[test]
    fn payload_is_clone() {
        let state = ControllerState::default();
        let payload = OverlayStatePayload {
            state,
            profile_name: Some("X".into()),
        };
        let _cloned = payload.clone();
    }

    // -----------------------------------------------------------------------
    // Save / load config (round-trip through the real config path)
    // -----------------------------------------------------------------------

    #[test]
    fn save_then_load_overlay_config_roundtrip() {
        let _guard = file_lock().lock().unwrap();
        let original = OverlayConfig {
            enabled: true,
            toggle_hotkey: "Ctrl+Alt+F7".into(),
            opacity: 0.42,
            position: "bottom-right".into(),
            show_battery: false,
            show_profile: false,
            show_fps: true,
            scale: 1.75,
        };

        let saved_path = save_overlay_config(&original).expect("save should succeed");
        assert!(saved_path.exists(), "config file should exist after save");

        let loaded = load_overlay_config();
        // load_overlay_config sanitizes, so compare against a sanitized copy.
        let mut expected = original.clone();
        expected.sanitize();
        assert_eq!(loaded, expected);

        // Restore a default config so the test does not leave stray state.
        let _ = save_overlay_config(&OverlayConfig::default());
    }

    #[test]
    fn save_overlay_config_creates_parent_dirs() {
        let _guard = file_lock().lock().unwrap();
        // The config path includes %APPDATA%\OxideLink\ which save creates if
        // missing.  Saving the default config should always succeed.
        let path = save_overlay_config(&OverlayConfig::default()).expect("save should succeed");
        assert!(path.exists());
    }

    #[test]
    fn overlay_config_path_ends_with_overlay_json() {
        let path = overlay_config_path();
        assert!(path.ends_with("overlay.json"));
        assert!(path.to_string_lossy().contains("OxideLink"));
    }

    #[test]
    fn load_overlay_config_returns_default_when_file_missing() {
        let _guard = file_lock().lock().unwrap();
        // Remove the config file so load falls back to defaults.
        let path = overlay_config_path();
        let _ = std::fs::remove_file(&path);
        let cfg = load_overlay_config();
        assert_eq!(cfg, OverlayConfig::default());
    }

    #[test]
    fn load_overlay_config_sanitizes_loaded_values() {
        let _guard = file_lock().lock().unwrap();
        // Write a config with out-of-range values; load should clamp them.
        let bad_json = r#"{
            "enabled": true,
            "toggle_hotkey": "Shift+F11",
            "opacity": 5.0,
            "position": "center",
            "show_battery": true,
            "show_profile": true,
            "show_fps": false,
            "scale": 50.0
        }"#;
        let path = overlay_config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, bad_json).unwrap();

        let cfg = load_overlay_config();
        assert_eq!(cfg.opacity, 1.0);
        assert_eq!(cfg.scale, 4.0);

        // Restore default.
        let _ = save_overlay_config(&OverlayConfig::default());
    }

    #[test]
    fn load_overlay_config_falls_back_on_invalid_json() {
        let _guard = file_lock().lock().unwrap();
        let path = overlay_config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, "{ not valid json }").unwrap();

        let cfg = load_overlay_config();
        assert_eq!(cfg, OverlayConfig::default());

        let _ = save_overlay_config(&OverlayConfig::default());
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn overlay_constants_have_expected_values() {
        assert_eq!(OVERLAY_WINDOW_LABEL, "overlay");
        assert_eq!(OVERLAY_HTML, "overlay.html");
        assert_eq!(BASE_WIDTH, 320.0);
        assert_eq!(BASE_HEIGHT, 180.0);
        assert_eq!(CORNER_PADDING, 12.0);
    }
}
