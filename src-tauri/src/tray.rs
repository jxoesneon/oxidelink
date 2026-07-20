//! System tray, minimize-to-tray, and Windows auto-start integration.
//!
//! The actual tray icon and close interception must be wired in `main.rs`.
//! This module exposes the `TrayManager`, registry helpers, and the Tauri
//! commands the frontend uses to read/update state.

use crate::config;
use crate::state::{AppCtx, IpcEvent, TrayState};
use tauri::{Manager, State};

/// Registry path used for the current-user Windows Run key.
pub const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Value name used for the startup entry.
pub const RUN_VALUE_NAME: &str = "OxideLink";

// ---------------------------------------------------------------------------
// Public pure helpers (suitable for unit tests)
// ---------------------------------------------------------------------------

/// Return the registry path the startup entry is stored under.
pub fn run_key_path() -> &'static str {
    RUN_KEY_PATH
}

/// Return the registry value name used for the startup entry.
pub fn run_value_name() -> &'static str {
    RUN_VALUE_NAME
}

// ---------------------------------------------------------------------------
// Windows registry implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod registry {
    use super::{RUN_KEY_PATH, RUN_VALUE_NAME};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPEN_CREATE_OPTIONS,
        REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn current_exe() -> Result<String, String> {
        std::env::current_exe()
            .map_err(|e| format!("failed to get current executable path: {e}"))
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn open_run_key(access: REG_SAM_FLAGS) -> Result<HKEY, String> {
        let subkey = to_wide(RUN_KEY_PATH);
        unsafe {
            let mut hkey: HKEY = null_mut();
            let status = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                null(),
                0 as REG_OPEN_CREATE_OPTIONS,
                access,
                null(),
                &mut hkey,
                null_mut(),
            );
            if status != ERROR_SUCCESS {
                return Err(format!("failed to open/create Run registry key: {status}"));
            }
            if hkey.is_null() {
                return Err("RegCreateKeyExW returned a null handle".into());
            }
            Ok(hkey)
        }
    }

    pub fn set_auto_start(enabled: bool) -> Result<(), String> {
        if enabled {
            let exe = current_exe()?;
            let value_name = to_wide(RUN_VALUE_NAME);
            let data = to_wide(&exe);
            unsafe {
                let hkey = open_run_key(KEY_WRITE)?;
                let status = RegSetValueExW(
                    hkey,
                    value_name.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr() as *const u8,
                    (data.len() * 2) as u32,
                );
                RegCloseKey(hkey);
                if status != ERROR_SUCCESS {
                    return Err(format!("RegSetValueExW failed: {status}"));
                }
            }
        } else {
            unsafe {
                let hkey = open_run_key(KEY_WRITE)?;
                let value_name = to_wide(RUN_VALUE_NAME);
                let status = RegDeleteValueW(hkey, value_name.as_ptr());
                RegCloseKey(hkey);
                // ERROR_FILE_NOT_FOUND (2) means the value is already absent.
                if status != ERROR_SUCCESS && status != 2 {
                    return Err(format!("RegDeleteValueW failed: {status}"));
                }
            }
        }
        Ok(())
    }

    pub fn get_auto_start() -> bool {
        let subkey = to_wide(RUN_KEY_PATH);
        let value_name = to_wide(RUN_VALUE_NAME);
        unsafe {
            let mut hkey: HKEY = null_mut();
            let status = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey);
            if status != ERROR_SUCCESS {
                return false;
            }

            let mut size: u32 = 0;
            let mut dtype: REG_VALUE_TYPE = 0;
            let status = RegQueryValueExW(
                hkey,
                value_name.as_ptr(),
                null(),
                &mut dtype,
                null_mut(),
                &mut size,
            );
            if status != ERROR_SUCCESS || size == 0 {
                RegCloseKey(hkey);
                return false;
            }

            let len = (size as usize / 2) + 1;
            let mut buf: Vec<u16> = vec![0; len];
            let mut read_size = (len * 2) as u32;
            let status = RegQueryValueExW(
                hkey,
                value_name.as_ptr(),
                null(),
                null_mut(),
                buf.as_mut_ptr() as *mut u8,
                &mut read_size,
            );
            RegCloseKey(hkey);
            if status != ERROR_SUCCESS {
                return false;
            }

            let stored = String::from_utf16_lossy(&buf[..(read_size as usize / 2)]);
            let stored = stored.trim_end_matches('\0');
            stored == current_exe().unwrap_or_default()
        }
    }
}

#[cfg(windows)]
pub use registry::{
    get_auto_start as get_auto_start_registry, set_auto_start as set_auto_start_registry,
};

#[cfg(not(windows))]
pub fn set_auto_start_registry(_enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn get_auto_start_registry() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Tray manager
// ---------------------------------------------------------------------------

/// Manages the system tray icon, menu, and minimize-to-tray behavior.
pub struct TrayManager;

impl TrayManager {
    /// Build the tray icon with a Show / Hide / Exit context menu.
    pub fn build(app: &tauri::AppHandle) -> Result<tauri::tray::TrayIcon, String> {
        use tauri::image::Image;

        // Prefer the embedded default window icon (icons/32x32.png, etc.).
        // Fall back to a simple 1x1 red icon if Tauri has no default icon.
        let icon = app
            .default_window_icon()
            .map(|i| i.to_owned())
            .unwrap_or_else(|| Image::new_owned(vec![255u8, 0, 0, 255], 1, 1));

        let menu = tauri::menu::MenuBuilder::new(app)
            .text("show", "Show")
            .text("hide", "Hide")
            .separator()
            .text("quit", "Exit")
            .build()
            .map_err(|e| format!("failed to build tray menu: {e}"))?;

        let tray = tauri::tray::TrayIconBuilder::with_id("main-tray")
            .icon(icon)
            .tooltip("OxideLink — Pro Controller Manager")
            .show_menu_on_left_click(false)
            .menu(&menu)
            .on_menu_event(|app, event| {
                let id = event.id().as_ref();
                match id {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            let _ = set_tray_minimize_for_app(app, false);
                        }
                    }
                    "hide" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.hide();
                            let _ = set_tray_minimize_for_app(app, true);
                        }
                    }
                    "quit" => {
                        log::info!("Quit requested from tray");
                        app.exit(0);
                    }
                    _ => {}
                }
            })
            .on_tray_icon_event(|tray, event| {
                if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                    let app = tray.app_handle();
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                        let _ = set_tray_minimize_for_app(app, false);
                    }
                }
            })
            .build(app)
            .map_err(|e| format!("failed to build tray icon: {e}"))?;

        Ok(tray)
    }

    /// Build and leak the tray icon so it stays alive for the app lifetime.
    pub fn install(app: &tauri::AppHandle) -> Result<(), String> {
        let tray = Self::build(app)?;
        std::mem::forget(tray);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tray state helpers
// ---------------------------------------------------------------------------

/// Update the runtime tray minimize/visible state and emit `TrayStateChanged`.
pub fn set_tray_minimize(ctx: &AppCtx, minimize: bool) -> TrayState {
    let state = {
        let mut controller = ctx.shared.active_controller_mut();
        controller.tray_state.minimized = minimize;
        controller.tray_state.visible = !minimize;
        controller.tray_state.clone()
    };
    let _ = ctx.tx.send(IpcEvent::TrayStateChanged {
        data: state.clone(),
    });
    state
}

/// Read the runtime tray minimize flag.
pub fn get_tray_minimize(ctx: &AppCtx) -> bool {
    ctx.shared.active_controller().tray_state.minimized
}

fn set_tray_minimize_for_app(app: &tauri::AppHandle, minimize: bool) -> Option<TrayState> {
    app.try_state::<AppCtx>()
        .map(|ctx| set_tray_minimize(&ctx, minimize))
}

/// Intercept `CloseRequested` and minimize to tray instead of quitting when
/// `AppConfig.tray_minimize` or `AppConfig.close_to_tray` is enabled.
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    use tauri::WindowEvent;
    if let WindowEvent::CloseRequested { api, .. } = event {
        let app = window.app_handle();
        let ctx = app.state::<AppCtx>();
        let minimize = {
            let config = ctx.shared.config.read();
            config.tray_minimize || config.close_to_tray
        };
        if minimize {
            let _ = window.hide();
            api.prevent_close();
            set_tray_minimize(&ctx, true);
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn set_auto_start(ctx: State<AppCtx>, enabled: bool) -> Result<bool, String> {
    set_auto_start_registry(enabled)?;

    {
        let mut config = ctx.shared.config.write();
        config.auto_start = enabled;
    }

    let state = {
        let mut controller = ctx.shared.active_controller_mut();
        controller.tray_state.auto_start = enabled;
        controller.tray_state.clone()
    };
    let _ = ctx.tx.send(IpcEvent::TrayStateChanged { data: state });

    // Persist the config change to disk when persistence is enabled.
    let cfg = ctx.shared.config.read().clone();
    if cfg.config_persistence_enabled {
        let _ = config::save_config(&cfg)?;
    }

    Ok(enabled)
}

#[tauri::command]
pub fn get_auto_start() -> bool {
    get_auto_start_registry()
}

#[tauri::command]
pub fn set_tray_state(ctx: State<AppCtx>, state: TrayState, app: tauri::AppHandle) -> TrayState {
    // Normalize: minimized means the window is hidden.
    let minimized = state.minimized;
    let visible = !minimized;
    let state = TrayState {
        visible,
        minimized,
        auto_start: state.auto_start,
    };

    {
        let mut controller = ctx.shared.active_controller_mut();
        controller.tray_state = state.clone();
    }

    if let Some(window) = app.get_webview_window("main") {
        if minimized {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }

    let _ = ctx.tx.send(IpcEvent::TrayStateChanged {
        data: state.clone(),
    });

    state
}

#[tauri::command]
pub fn get_tray_state(ctx: State<AppCtx>) -> TrayState {
    ctx.shared.active_controller().tray_state.clone()
}

// ---------------------------------------------------------------------------
// Tests (pure logic only — no AppHandle, no system tray, no Windows APIs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{IpcEvent, TrayState};

    /// Menu item identifiers used by [`TrayManager::build`]. Kept in sync with
    /// the strings passed to `MenuBuilder::text` so the menu-event handler can
    /// dispatch correctly.
    const MENU_ID_SHOW: &str = "show";
    const MENU_ID_HIDE: &str = "hide";
    const MENU_ID_QUIT: &str = "quit";

    // -- Constants / pure helpers ------------------------------------------

    #[test]
    fn run_key_path_returns_expected_registry_path() {
        assert_eq!(
            run_key_path(),
            r"Software\Microsoft\Windows\CurrentVersion\Run"
        );
        assert_eq!(run_key_path(), RUN_KEY_PATH);
    }

    #[test]
    fn run_value_name_returns_app_name() {
        assert_eq!(run_value_name(), "OxideLink");
        assert_eq!(run_value_name(), RUN_VALUE_NAME);
    }

    #[test]
    fn registry_constants_are_non_empty() {
        assert!(!RUN_KEY_PATH.is_empty());
        assert!(!RUN_VALUE_NAME.is_empty());
    }

    // -- TrayState defaults ------------------------------------------------

    #[test]
    fn tray_state_default_is_visible_not_minimized_no_autostart() {
        let s = TrayState::default();
        assert!(s.visible);
        assert!(!s.minimized);
        assert!(!s.auto_start);
    }

    #[test]
    fn tray_state_default_is_consistent() {
        // Two defaults must be equal.
        assert_eq!(TrayState::default(), TrayState::default());
    }

    // -- TrayState transitions / normalization -----------------------------

    /// Mirrors the normalization performed in the `set_tray_state` command:
    /// `visible` must always be the logical negation of `minimized`.
    fn normalize(state: TrayState) -> TrayState {
        let minimized = state.minimized;
        TrayState {
            visible: !minimized,
            minimized,
            auto_start: state.auto_start,
        }
    }

    #[test]
    fn normalize_minimized_hides_window() {
        let raw = TrayState {
            visible: true, // intentionally inconsistent input
            minimized: true,
            auto_start: false,
        };
        let n = normalize(raw);
        assert!(n.minimized);
        assert!(!n.visible, "minimized => visible must be false");
        assert!(!n.auto_start);
    }

    #[test]
    fn normalize_visible_shows_window() {
        let raw = TrayState {
            visible: false, // intentionally inconsistent input
            minimized: false,
            auto_start: true,
        };
        let n = normalize(raw);
        assert!(!n.minimized);
        assert!(n.visible, "not minimized => visible must be true");
        assert!(n.auto_start);
    }

    #[test]
    fn normalize_preserves_auto_start_flag() {
        for auto in [false, true] {
            let raw = TrayState {
                visible: true,
                minimized: false,
                auto_start: auto,
            };
            assert_eq!(normalize(raw).auto_start, auto);
        }
    }

    #[test]
    fn tray_state_visible_xor_minimized_after_normalize() {
        // After normalization exactly one of (visible, minimized) is true.
        for minimized in [false, true] {
            let n = normalize(TrayState {
                visible: minimized, // garbage input
                minimized,
                auto_start: false,
            });
            assert_ne!(n.visible, n.minimized);
        }
    }

    // -- Serialization -----------------------------------------------------

    #[test]
    fn tray_state_serializes_to_expected_json() {
        let s = TrayState {
            visible: true,
            minimized: false,
            auto_start: true,
        };
        let json = serde_json::to_value(&s).expect("serialize");
        assert_eq!(json["visible"], serde_json::Value::Bool(true));
        assert_eq!(json["minimized"], serde_json::Value::Bool(false));
        assert_eq!(json["auto_start"], serde_json::Value::Bool(true));
    }

    #[test]
    fn tray_state_round_trips_through_json() {
        let original = TrayState {
            visible: false,
            minimized: true,
            auto_start: true,
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: TrayState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    #[test]
    fn tray_state_default_round_trips_through_json() {
        let original = TrayState::default();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: TrayState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    #[test]
    fn tray_state_deserializes_from_flat_json() {
        let json = r#"{"visible":false,"minimized":true,"auto_start":false}"#;
        let s: TrayState = serde_json::from_str(json).expect("deserialize");
        assert!(!s.visible);
        assert!(s.minimized);
        assert!(!s.auto_start);
    }

    // -- Event payload -----------------------------------------------------

    #[test]
    fn tray_state_changed_event_carries_state() {
        let state = TrayState {
            visible: false,
            minimized: true,
            auto_start: true,
        };
        let evt = IpcEvent::TrayStateChanged {
            data: state.clone(),
        };
        let json = serde_json::to_value(&evt).expect("serialize event");
        assert_eq!(json["type"], "TrayStateChanged");
        assert_eq!(json["data"]["minimized"], serde_json::Value::Bool(true));
        assert_eq!(json["data"]["visible"], serde_json::Value::Bool(false));
        assert_eq!(json["data"]["auto_start"], serde_json::Value::Bool(true));
    }

    #[test]
    fn tray_state_changed_event_round_trips() {
        let state = TrayState {
            visible: true,
            minimized: false,
            auto_start: false,
        };
        let evt = IpcEvent::TrayStateChanged {
            data: state.clone(),
        };
        let json = serde_json::to_string(&evt).expect("serialize");
        let back: IpcEvent = serde_json::from_str(&json).expect("deserialize");
        match back {
            IpcEvent::TrayStateChanged { data } => assert_eq!(data, state),
            other => panic!("expected TrayStateChanged, got {other:?}"),
        }
    }

    // -- Menu item ID dispatch (pure string matching) ----------------------

    /// Replicates the `match id { ... }` dispatch inside `TrayManager::build`'s
    /// `on_menu_event` closure without requiring an `AppHandle`.
    fn dispatch_menu_id(id: &str) -> &'static str {
        match id {
            MENU_ID_SHOW => "show",
            MENU_ID_HIDE => "hide",
            MENU_ID_QUIT => "quit",
            _ => "unknown",
        }
    }

    #[test]
    fn menu_id_show_dispatches_correctly() {
        assert_eq!(dispatch_menu_id(MENU_ID_SHOW), "show");
    }

    #[test]
    fn menu_id_hide_dispatches_correctly() {
        assert_eq!(dispatch_menu_id(MENU_ID_HIDE), "hide");
    }

    #[test]
    fn menu_id_quit_dispatches_correctly() {
        assert_eq!(dispatch_menu_id(MENU_ID_QUIT), "quit");
    }

    #[test]
    fn menu_id_unknown_dispatches_to_unknown() {
        assert_eq!(dispatch_menu_id("foobar"), "unknown");
        assert_eq!(dispatch_menu_id(""), "unknown");
    }

    #[test]
    fn menu_ids_are_non_empty_and_distinct() {
        let ids = [MENU_ID_SHOW, MENU_ID_HIDE, MENU_ID_QUIT];
        for id in ids {
            assert!(!id.is_empty(), "menu id must not be empty");
        }
        // All three are distinct.
        assert_ne!(MENU_ID_SHOW, MENU_ID_HIDE);
        assert_ne!(MENU_ID_SHOW, MENU_ID_QUIT);
        assert_ne!(MENU_ID_HIDE, MENU_ID_QUIT);
    }

    // -- TrayManager (zero-sized marker struct) ----------------------------

    #[test]
    fn tray_manager_is_zero_sized() {
        assert_eq!(std::mem::size_of::<TrayManager>(), 0);
    }
}
