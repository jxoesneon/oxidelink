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
    fn overlay_state_defaults_to_disabled_and_not_visible() {
        let st = OverlayState::default();
        assert!(!st.visible);
        assert!(!st.config.enabled);
        assert_eq!(st.config.position, "top-left");
        assert_eq!(st.config.toggle_hotkey, "Shift+F11");
    }

    #[test]
    fn hotkey_normalization_and_validation() {
        let (mods, key) = parse_hotkey("  ctrl + shift + f10 ").unwrap();
        assert_eq!(mods, vec!["Control", "Shift"]);
        assert_eq!(key, "F10");

        assert!(parse_hotkey("Ctrl+").is_err());
        assert!(parse_hotkey("Shift++F").is_err());
    }
}
