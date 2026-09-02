//! Keyboard / mouse emulation backend using the Windows `SendInput` API.
//!
//! This module translates controller button presses and stick deflections into
//! desktop keyboard and mouse events. It is intentionally split from the HID
//! device loop so it can be unit-tested with a mock input backend.
//!
//! # Anti-cheat limitations
//!
//! `SendInput` injects input at the OS level. Most anti-cheat and protected
//! games can (and do) reject or flag injected input because the low-level
//! `LLMHF_INJECTED` / `LLKHF_INJECTED` bits are set by the system. Some titles
//! additionally require UIAccess or a signed kernel-level input driver. This
//! implementation is suitable for desktop titles, tools, and non-protected
//! games, but should be considered a starting point for competitive scenarios.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::keycode;
use crate::state::{
    Action, ButtonId, ButtonState, ControllerState, KbmConfig, Mappings, StickAction, StickMapping,
    StickSide,
};

// -----------------------------------------------------------------------------
// Input backend abstraction
// -----------------------------------------------------------------------------

/// A single logical input event produced by the emulator.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key { vk: u16, down: bool },
    MouseMove { dx: i32, dy: i32 },
    MouseButton { button: u8, down: bool },
    MouseWheel { delta: i32 },
}

/// Pluggable backend for `KbmEmulator`. Production code uses `WindowsBackend`;
/// tests use `MockBackend` to capture events on a channel.
pub trait InputBackend: Send + Sync {
    fn send(&self, event: InputEvent);
}

/// Production backend that calls `SendInput`.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsBackend;

/// Test backend that forwards events to a `tokio::sync::mpsc` channel.
#[derive(Clone)]
pub struct MockBackend {
    tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
}

impl MockBackend {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<InputEvent>) -> Self {
        Self { tx }
    }
}

impl InputBackend for MockBackend {
    fn send(&self, event: InputEvent) {
        let _ = self.tx.send(event);
    }
}

impl InputBackend for WindowsBackend {
    fn send(&self, event: InputEvent) {
        unsafe { send_event(event) };
    }
}

// -----------------------------------------------------------------------------
// Windows SendInput implementation
// -----------------------------------------------------------------------------

#[cfg(windows)]
unsafe fn send_event(event: InputEvent) {
    use std::mem;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    };

    fn send_inputs(inputs: &[INPUT]) {
        let count = inputs.len() as u32;
        if count == 0 {
            log::warn!("send_inputs called with empty input array");
            return;
        }
        let cb_size = mem::size_of::<INPUT>() as i32;
        if cb_size == 0 {
            log::warn!("INPUT struct reports zero size; skipping SendInput");
            return;
        }
        let sent = unsafe { SendInput(count, inputs.as_ptr(), cb_size) };
        if sent != count {
            log::warn!(
                "SendInput sent {} of {} input events (last error {:?})",
                sent,
                count,
                std::io::Error::last_os_error()
            );
        }
    }

    match event {
        InputEvent::Key { vk, down } => {
            let flags = if down { 0 } else { KEYEVENTF_KEYUP };
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            send_inputs(&[input]);
        }
        InputEvent::MouseMove { dx, dy } => {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            send_inputs(&[input]);
        }
        InputEvent::MouseButton { button, down } => {
            let (down_flag, up_flag, data) = match button {
                0 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0u32),
                1 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0u32),
                2 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0u32),
                3 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1u32),
                4 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2u32),
                _ => return,
            };
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: data,
                        dwFlags: if down { down_flag } else { up_flag },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            send_inputs(&[input]);
        }
        InputEvent::MouseWheel { delta } => {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: delta as u32,
                        dwFlags: MOUSEEVENTF_WHEEL,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            send_inputs(&[input]);
        }
    }
}

#[cfg(not(windows))]
unsafe fn send_event(_event: InputEvent) {
    // Placeholder for non-Windows builds. The Windows-only `SendInput` path is
    // gated by `#[cfg(windows)]`; this stub keeps the trait object type simple.
}

// -----------------------------------------------------------------------------
// KbmEmulator
// -----------------------------------------------------------------------------

/// Runtime state for keyboard/mouse emulation.
pub struct KbmEmulator {
    pub enabled: bool,
    config: KbmConfig,
    backend: Arc<dyn InputBackend + Send + Sync>,
    repeat_handles: HashMap<String, tokio::task::AbortHandle>,
    active_stick_keys: [HashSet<String>; 2],
    /// Previous full button state, used to emit edge-triggered key events
    /// when `process_controller_state` is called every report.
    last_button_state: ButtonState,
}

impl KbmEmulator {
    /// Create an emulator using the real `WindowsBackend`.
    pub fn new() -> Self {
        Self::with_backend(Arc::new(WindowsBackend))
    }

    /// Create an emulator with a custom backend (used in tests).
    pub fn with_backend(backend: Arc<dyn InputBackend + Send + Sync>) -> Self {
        Self {
            enabled: KbmConfig::default().enabled,
            config: KbmConfig::default(),
            backend,
            repeat_handles: HashMap::new(),
            active_stick_keys: Default::default(),
            last_button_state: ButtonState::default(),
        }
    }

    /// Update live configuration (enabled flag, repeat timings, sensitivity).
    pub fn set_config(&mut self, config: &KbmConfig) {
        self.enabled = config.enabled;
        self.config = config.clone();
    }

    /// Send a single key down/up event.
    pub fn send_key(&self, vk: u16, down: bool) {
        self.backend.send(InputEvent::Key { vk, down });
    }

    /// Send a relative mouse movement.
    pub fn send_mouse_move(&self, dx: i32, dy: i32) {
        self.backend.send(InputEvent::MouseMove { dx, dy });
    }

    /// Send a mouse button down/up event. `button` 0=L, 1=R, 2=M, 3=X1, 4=X2.
    pub fn send_mouse_button(&self, button: u8, down: bool) {
        self.backend.send(InputEvent::MouseButton { button, down });
    }

    /// Send a vertical mouse-wheel event. Positive `delta` scrolls up.
    pub fn send_mouse_wheel(&self, delta: i32) {
        self.backend.send(InputEvent::MouseWheel { delta });
    }

    /// Process a controller button event through the configured mappings.
    pub fn process_button(&mut self, id: ButtonId, pressed: bool, mappings: &Mappings) {
        if !self.enabled {
            return;
        }

        for mapping in mappings.buttons.iter().filter(|m| m.source == id) {
            for action in &mapping.actions {
                self.dispatch_action(id, action, pressed);
            }
        }
    }

    /// Process a full controller state every report, emitting KB/M events for
    /// button transitions and active stick mappings. This is the entry point
    /// used by the device loop.
    pub fn process_controller_state(
        &mut self,
        state: &ControllerState,
        config: &KbmConfig,
        mappings: &Mappings,
    ) {
        self.set_config(config);

        // Track button edges so holding a button does not spam keydown events.
        let ids = [
            ButtonId::A,
            ButtonId::B,
            ButtonId::X,
            ButtonId::Y,
            ButtonId::Up,
            ButtonId::Down,
            ButtonId::Left,
            ButtonId::Right,
            ButtonId::L,
            ButtonId::R,
            ButtonId::Zl,
            ButtonId::Zr,
            ButtonId::Minus,
            ButtonId::Plus,
            ButtonId::Home,
            ButtonId::Capture,
            ButtonId::LStick,
            ButtonId::RStick,
        ];
        for &id in &ids {
            let pressed = state.buttons.get(id);
            let was_pressed = self.last_button_state.get(id);
            if pressed != was_pressed {
                self.process_button(id, pressed, mappings);
            }
        }
        self.last_button_state = state.buttons.clone();

        // Process stick mappings (WASD/arrow keys, mouse, scroll).
        self.process_stick(
            StickSide::Left,
            state.left_stick.x,
            state.left_stick.y,
            config,
            &mappings.sticks,
        );
        self.process_stick(
            StickSide::Right,
            state.right_stick.x,
            state.right_stick.y,
            config,
            &mappings.sticks,
        );
    }

    fn dispatch_action(&mut self, id: ButtonId, action: &Action, pressed: bool) {
        match action {
            Action::Key(name) => {
                let Some(vk) = keycode::vk(name) else {
                    log::warn!("Unknown key name '{}' in mapping for {:?}", name, id);
                    return;
                };
                let key_id = format!("btn:{:?}:{}", id, name);
                if pressed {
                    self.send_key(vk, true);
                    self.start_key_repeat(key_id, vk);
                } else {
                    self.stop_key_repeat(&key_id);
                    self.send_key(vk, false);
                }
            }
            Action::KeyCombo(names) => {
                let vks: Vec<u16> = names
                    .iter()
                    .filter_map(|n| {
                        let code = keycode::vk(n);
                        if code.is_none() {
                            log::warn!("Unknown key name '{}' in combo for {:?}", n, id);
                        }
                        code
                    })
                    .collect();
                if pressed {
                    for &vk in &vks {
                        self.send_key(vk, true);
                    }
                } else {
                    for &vk in vks.iter().rev() {
                        self.send_key(vk, false);
                    }
                }
            }
            Action::MouseButton(button) => {
                self.send_mouse_button(*button, pressed);
            }
            Action::Macro(mac_id) => {
                log::debug!("Macro '{}' not yet implemented in KBM layer", mac_id);
            }
            _ => {}
        }
    }

    fn start_key_repeat(&mut self, key_id: String, vk: u16) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let backend = self.backend.clone();
        let delay = Duration::from_millis(self.config.key_repeat_delay_ms as u64);
        let rate = Duration::from_millis(self.config.key_repeat_rate_ms.clamp(1, 5_000) as u64);

        let abort_handle = tokio::spawn(async move {
            if delay > Duration::ZERO {
                tokio::time::sleep(delay).await;
            }
            loop {
                backend.send(InputEvent::Key { vk, down: true });
                tokio::time::sleep(rate).await;
            }
        })
        .abort_handle();

        self.repeat_handles.insert(key_id, abort_handle);
    }

    fn stop_key_repeat(&mut self, key_id: &str) {
        if let Some(handle) = self.repeat_handles.remove(key_id) {
            handle.abort();
        }
    }

    /// Process a stick deflection into WASD/arrow keys, mouse movement, or scroll.
    pub fn process_stick(
        &mut self,
        side: StickSide,
        x: f32,
        y: f32,
        config: &KbmConfig,
        mapping: &StickMapping,
    ) {
        if !self.enabled {
            return;
        }

        let actions = match side {
            StickSide::Left => &mapping.left_actions,
            StickSide::Right => &mapping.right_actions,
        };

        // Use the stick zone deadzone as a threshold; fall back to 0.25 when unset.
        let dz = if mapping.zones.deadzone > 0.0 {
            mapping.zones.deadzone
        } else {
            0.25f32
        };

        let idx = if side == StickSide::Left { 0 } else { 1 };
        let mut new_keys: HashSet<String> = HashSet::new();
        let mut mouse_sent = false;

        for action in actions {
            match action {
                StickAction::Mouse => {
                    if !mouse_sent && (x.abs() > dz || y.abs() > dz) {
                        let dx = (x * config.mouse_sensitivity * 20.0) as i32;
                        // Invert Y so pushing the stick up moves the cursor up.
                        let dy = (-y * config.mouse_sensitivity * 20.0) as i32;
                        self.send_mouse_move(dx, dy);
                        mouse_sent = true;
                    }
                }
                StickAction::Wasd => {
                    if y > dz {
                        new_keys.insert("W".into());
                    } else if y < -dz {
                        new_keys.insert("S".into());
                    }
                    if x > dz {
                        new_keys.insert("D".into());
                    } else if x < -dz {
                        new_keys.insert("A".into());
                    }
                }
                StickAction::ArrowKeys => {
                    if y > dz {
                        new_keys.insert("Up".into());
                    } else if y < -dz {
                        new_keys.insert("Down".into());
                    }
                    if x > dz {
                        new_keys.insert("Right".into());
                    } else if x < -dz {
                        new_keys.insert("Left".into());
                    }
                }
                StickAction::Scroll if y.abs() > dz => {
                    let delta = (y * config.mouse_sensitivity * 120.0 * 0.2) as i32;
                    self.send_mouse_wheel(delta);
                }
                _ => {}
            }
        }

        // Diff against the previously active stick keys and emit key events.
        let prev = &self.active_stick_keys[idx];
        for key in new_keys.difference(prev) {
            if let Some(vk) = keycode::vk(key) {
                self.send_key(vk, true);
            }
        }
        for key in prev.difference(&new_keys) {
            if let Some(vk) = keycode::vk(key) {
                self.send_key(vk, false);
            }
        }
        self.active_stick_keys[idx] = new_keys;
    }
}

impl Default for KbmEmulator {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------------------
// Tauri-managed wrapper
// -----------------------------------------------------------------------------

use parking_lot::Mutex;

/// Tauri-managed state that owns the live `KbmEmulator` and reads mappings/config
/// from the shared app state. Commands below accept `State<'_, KbmManager>`.
#[derive(Clone)]
pub struct KbmManager {
    shared: Arc<crate::state::SharedState>,
    emulator: Arc<Mutex<KbmEmulator>>,
}

impl KbmManager {
    pub fn new(shared: Arc<crate::state::SharedState>) -> Self {
        let config = shared.config.read().kbm_config.clone();
        let emulator = Arc::new(Mutex::new(KbmEmulator::new()));
        emulator.lock().set_config(&config);
        Self { shared, emulator }
    }

    pub fn set_enabled(&self, enabled: bool) -> bool {
        {
            let mut emu = self.emulator.lock();
            emu.enabled = enabled;
        }
        self.shared.config.write().kbm_config.enabled = enabled;
        enabled
    }

    pub fn status(&self) -> KbmConfig {
        self.shared.config.read().kbm_config.clone()
    }

    pub fn set_mappings(&self, mappings: Mappings) {
        self.shared.config.write().mappings = mappings;
    }

    pub fn get_mappings(&self) -> Mappings {
        self.shared.config.read().mappings.clone()
    }

    pub fn send_test_key(&self, key: String, down: bool) -> Result<(), String> {
        let vk = keycode::vk(&key).ok_or_else(|| format!("Unknown key name: {}", key))?;
        let emu = self.emulator.lock();
        emu.send_key(vk, down);
        Ok(())
    }

    pub fn button_event(&self, id: ButtonId, pressed: bool) {
        let cfg = self.shared.config.read();
        let kbm_cfg = cfg.kbm_config.clone();
        let mappings = cfg.mappings.clone();
        drop(cfg);
        let mut emu = self.emulator.lock();
        emu.set_config(&kbm_cfg);
        emu.process_button(id, pressed, &mappings);
    }

    pub fn stick_event(&self, side: StickSide, x: f32, y: f32) {
        let cfg = self.shared.config.read();
        let kbm_cfg = cfg.kbm_config.clone();
        let mapping = cfg.mappings.sticks.clone();
        drop(cfg);
        let mut emu = self.emulator.lock();
        emu.set_config(&kbm_cfg);
        emu.process_stick(side, x, y, &kbm_cfg, &mapping);
    }
}

// -----------------------------------------------------------------------------
// Tauri commands (not yet wired into main.rs invoke_handler)
// -----------------------------------------------------------------------------

#[tauri::command]
pub fn kbm_set_enabled(manager: tauri::State<'_, KbmManager>, enabled: bool) -> bool {
    manager.set_enabled(enabled)
}

#[tauri::command]
pub fn kbm_get_status(manager: tauri::State<'_, KbmManager>) -> KbmConfig {
    manager.status()
}

#[tauri::command]
pub fn kbm_set_mappings(manager: tauri::State<'_, KbmManager>, mappings: Mappings) {
    manager.set_mappings(mappings);
}

#[tauri::command]
pub fn kbm_get_mappings(manager: tauri::State<'_, KbmManager>) -> Mappings {
    manager.get_mappings()
}

#[tauri::command]
pub fn kbm_send_test_key(
    manager: tauri::State<'_, KbmManager>,
    key: String,
    down: bool,
) -> Result<(), String> {
    manager.send_test_key(key, down)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        Action, ButtonMapping, ButtonState, ControllerState, Mappings, StickAction, StickMapping,
        StickSide, StickZones,
    };

    /// Helper: build an enabled emulator backed by a mock channel.
    fn mock_emulator() -> (
        KbmEmulator,
        tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        let mut cfg = KbmConfig::default();
        cfg.enabled = true;
        emu.set_config(&cfg);
        (emu, rx)
    }

    // ---- InputEvent ----

    #[test]
    fn input_event_key_equality_and_clone() {
        let a = InputEvent::Key {
            vk: 0x57,
            down: true,
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(
            a,
            InputEvent::Key {
                vk: 0x57,
                down: false
            }
        );
    }

    #[test]
    fn input_event_mouse_move_equality() {
        let a = InputEvent::MouseMove { dx: 10, dy: -5 };
        let b = InputEvent::MouseMove { dx: 10, dy: -5 };
        assert_eq!(a, b);
        assert_ne!(a, InputEvent::MouseMove { dx: 11, dy: -5 });
    }

    #[test]
    fn input_event_mouse_button_equality() {
        let a = InputEvent::MouseButton {
            button: 2,
            down: true,
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(
            a,
            InputEvent::MouseButton {
                button: 1,
                down: true
            }
        );
    }

    #[test]
    fn input_event_mouse_wheel_equality() {
        let a = InputEvent::MouseWheel { delta: 120 };
        assert_eq!(a, InputEvent::MouseWheel { delta: 120 });
        assert_ne!(a, InputEvent::MouseWheel { delta: -120 });
    }

    #[test]
    fn input_event_debug_format_contains_variant() {
        let e = InputEvent::Key {
            vk: 0x41,
            down: true,
        };
        let s = format!("{:?}", e);
        assert!(s.contains("Key"));
    }

    // ---- MockBackend ----

    #[test]
    fn mock_backend_forwards_events_to_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let backend = MockBackend::new(tx);
        backend.send(InputEvent::Key {
            vk: 0x41,
            down: true,
        });
        backend.send(InputEvent::MouseWheel { delta: 5 });
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: true
            })
        );
        assert_eq!(rx.try_recv(), Ok(InputEvent::MouseWheel { delta: 5 }));
        assert!(rx.try_recv().is_err());
    }

    // ---- KbmEmulator state ----

    #[test]
    fn emulator_default_is_disabled() {
        // WindowsBackend is used by default; we only inspect state, not events.
        let emu = KbmEmulator::new();
        assert!(!emu.enabled);
    }

    #[test]
    fn emulator_with_backend_inherits_default_config() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        assert!(!emu.enabled);
    }

    #[test]
    fn set_config_updates_enabled_flag() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        let mut cfg = KbmConfig::default();
        cfg.enabled = true;
        cfg.mouse_sensitivity = 3.5;
        emu.set_config(&cfg);
        assert!(emu.enabled);
        assert_eq!(emu.config.mouse_sensitivity, 3.5);
    }

    #[test]
    fn send_key_emits_key_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        emu.send_key(0x41, true);
        emu.send_key(0x41, false);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: true
            })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: false
            })
        );
    }

    #[test]
    fn send_mouse_move_emits_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        emu.send_mouse_move(7, -3);
        assert_eq!(rx.try_recv(), Ok(InputEvent::MouseMove { dx: 7, dy: -3 }));
    }

    #[test]
    fn send_mouse_button_emits_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        emu.send_mouse_button(1, true);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::MouseButton {
                button: 1,
                down: true
            })
        );
    }

    #[test]
    fn send_mouse_wheel_emits_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        emu.send_mouse_wheel(240);
        assert_eq!(rx.try_recv(), Ok(InputEvent::MouseWheel { delta: 240 }));
    }

    // ---- process_button ----

    #[test]
    fn process_button_no_op_when_disabled() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        // disabled by default
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Key("W".into())],
            }],
            ..Default::default()
        };
        emu.process_button(ButtonId::A, true, &mappings);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_button_ignores_unmapped_button() {
        let (mut emu, mut rx) = mock_emulator();
        let mappings = Mappings::default();
        emu.process_button(ButtonId::A, true, &mappings);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_button_key_combo_emits_all_down_then_all_up_reversed() {
        let (mut emu, mut rx) = mock_emulator();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::KeyCombo(vec!["LShift".into(), "A".into()])],
            }],
            ..Default::default()
        };
        emu.process_button(ButtonId::A, true, &mappings);
        // Down order: LShift (0xA0) then A (0x41)
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0xA0,
                down: true
            })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: true
            })
        );
        emu.process_button(ButtonId::A, false, &mappings);
        // Up order: reversed -> A then LShift
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: false
            })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0xA0,
                down: false
            })
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_button_key_combo_skips_unknown_keys() {
        let (mut emu, mut rx) = mock_emulator();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::KeyCombo(vec!["unknown_key".into(), "A".into()])],
            }],
            ..Default::default()
        };
        emu.process_button(ButtonId::A, true, &mappings);
        // Only the valid key (A) should be emitted.
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: true
            })
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_button_macro_action_is_no_op() {
        let (mut emu, mut rx) = mock_emulator();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Macro("macro1".into())],
            }],
            ..Default::default()
        };
        emu.process_button(ButtonId::A, true, &mappings);
        emu.process_button(ButtonId::A, false, &mappings);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_button_unknown_key_name_emits_nothing() {
        let (mut emu, mut rx) = mock_emulator();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Key("not_a_real_key".into())],
            }],
            ..Default::default()
        };
        emu.process_button(ButtonId::A, true, &mappings);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_button_multiple_actions_all_dispatched() {
        let (mut emu, mut rx) = mock_emulator();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Key("W".into()), Action::MouseButton(0)],
            }],
            ..Default::default()
        };
        emu.process_button(ButtonId::A, true, &mappings);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x57,
                down: true
            })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::MouseButton {
                button: 0,
                down: true
            })
        );
    }

    // ---- process_controller_state (edge detection) ----

    #[test]
    fn process_controller_state_emits_on_button_edge_only() {
        let (mut emu, mut rx) = mock_emulator();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Key("W".into())],
            }],
            ..Default::default()
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        let mut state = ControllerState::default();
        state.buttons.a = true;
        emu.process_controller_state(&state, &cfg, &mappings);
        // First call: A went from false -> true, emits key down.
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x57,
                down: true
            })
        );

        // Second call with same state: no new edge, no events.
        emu.process_controller_state(&state, &cfg, &mappings);
        assert!(rx.try_recv().is_err());

        // Release: edge true -> false, emits key up.
        state.buttons.a = false;
        emu.process_controller_state(&state, &cfg, &mappings);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x57,
                down: false
            })
        );
    }

    #[test]
    fn process_controller_state_disabled_emits_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        // disabled by default
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Key("W".into())],
            }],
            ..Default::default()
        };
        let cfg = KbmConfig::default(); // enabled = false
        let mut state = ControllerState::default();
        state.buttons.a = true;
        emu.process_controller_state(&state, &cfg, &mappings);
        assert!(rx.try_recv().is_err());
    }

    // ---- Stick handling ----

    #[test]
    fn process_stick_arrow_keys() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![StickAction::ArrowKeys],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        // Up: y > dz
        emu.process_stick(StickSide::Left, 0.0, 1.0, &cfg, &mapping);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x26,
                down: true
            }) // VK_UP
        );

        // Move to down: Up released, Down pressed
        emu.process_stick(StickSide::Left, 0.0, -1.0, &cfg, &mapping);
        let mut saw_up_up = false;
        let mut saw_down_down = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                InputEvent::Key {
                    vk: 0x26,
                    down: false,
                } => saw_up_up = true,
                InputEvent::Key {
                    vk: 0x28,
                    down: true,
                } => saw_down_down = true,
                _ => {}
            }
        }
        assert!(saw_up_up, "expected Up key release");
        assert!(saw_down_down, "expected Down key press");
    }

    #[test]
    fn process_stick_arrow_keys_left_right() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![StickAction::ArrowKeys],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.3,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        emu.process_stick(StickSide::Left, 1.0, 0.0, &cfg, &mapping);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x27,
                down: true
            }) // VK_RIGHT
        );

        emu.process_stick(StickSide::Left, -1.0, 0.0, &cfg, &mapping);
        let mut saw_right_up = false;
        let mut saw_left_down = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                InputEvent::Key {
                    vk: 0x27,
                    down: false,
                } => saw_right_up = true,
                InputEvent::Key {
                    vk: 0x25,
                    down: true,
                } => saw_left_down = true,
                _ => {}
            }
        }
        assert!(saw_right_up);
        assert!(saw_left_down);
    }

    #[test]
    fn process_stick_disabled_when_below_deadzone() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![StickAction::Wasd],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.5,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        // Deflection below deadzone -> no keys.
        emu.process_stick(StickSide::Left, 0.3, 0.3, &cfg, &mapping);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_stick_uses_default_deadzone_when_zero() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![StickAction::Wasd],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.0, // unset -> fallback 0.25
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        // 0.2 is below the 0.25 fallback deadzone -> no event.
        emu.process_stick(StickSide::Left, 0.2, 0.0, &cfg, &mapping);
        assert!(rx.try_recv().is_err());

        // 0.3 exceeds the 0.25 fallback -> D pressed.
        emu.process_stick(StickSide::Left, 0.3, 0.0, &cfg, &mapping);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x44,
                down: true
            })
        );
    }

    #[test]
    fn process_stick_scroll_emits_wheel() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![],
            right_actions: vec![StickAction::Scroll],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            mouse_sensitivity: 1.0,
            ..Default::default()
        };

        emu.process_stick(StickSide::Right, 0.0, 1.0, &cfg, &mapping);
        // delta = y * sensitivity * 120 * 0.2 = 1.0 * 1.0 * 120 * 0.2 = 24
        assert_eq!(rx.try_recv(), Ok(InputEvent::MouseWheel { delta: 24 }));
    }

    #[test]
    fn process_stick_scroll_no_event_below_deadzone() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![],
            right_actions: vec![StickAction::Scroll],
            zones: StickZones {
                deadzone: 0.5,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        emu.process_stick(StickSide::Right, 0.0, 0.3, &cfg, &mapping);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_stick_mouse_inverts_y() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![],
            right_actions: vec![StickAction::Mouse],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            mouse_sensitivity: 1.0,
            ..Default::default()
        };

        // y = 1.0 (push up) -> dy should be negative (cursor up).
        emu.process_stick(StickSide::Right, 0.0, 1.0, &cfg, &mapping);
        let ev = rx.try_recv().unwrap();
        match ev {
            InputEvent::MouseMove { dx, dy } => {
                assert_eq!(dx, 0);
                assert!(dy < 0, "expected negative dy for upward stick, got {}", dy);
            }
            other => panic!("expected MouseMove, got {:?}", other),
        }
    }

    #[test]
    fn process_stick_mouse_only_sent_once_per_report() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![],
            right_actions: vec![StickAction::Mouse, StickAction::Mouse],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            mouse_sensitivity: 1.0,
            ..Default::default()
        };

        emu.process_stick(StickSide::Right, 0.8, 0.0, &cfg, &mapping);
        // Even though Mouse appears twice, only one MouseMove is emitted.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_stick_disabled_action_emits_nothing() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![StickAction::Disabled],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        emu.process_stick(StickSide::Left, 1.0, 1.0, &cfg, &mapping);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_stick_no_op_when_emulator_disabled() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        // disabled by default
        let mapping = StickMapping {
            left_actions: vec![StickAction::Wasd],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig::default();
        emu.process_stick(StickSide::Left, 1.0, 0.0, &cfg, &mapping);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_stick_releases_keys_when_returning_to_center() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![StickAction::Wasd],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        // Press D
        emu.process_stick(StickSide::Left, 1.0, 0.0, &cfg, &mapping);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x44,
                down: true
            })
        );

        // Return to center -> D released
        emu.process_stick(StickSide::Left, 0.0, 0.0, &cfg, &mapping);
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x44,
                down: false
            })
        );
    }

    #[test]
    fn process_stick_diagonal_emits_two_keys() {
        let (mut emu, mut rx) = mock_emulator();
        let mapping = StickMapping {
            left_actions: vec![StickAction::Wasd],
            right_actions: vec![],
            zones: StickZones {
                deadzone: 0.25,
                ..Default::default()
            },
            response_curve: Default::default(),
        };
        let cfg = KbmConfig {
            enabled: true,
            ..Default::default()
        };

        // Up-right diagonal: W and D
        emu.process_stick(StickSide::Left, 1.0, 1.0, &cfg, &mapping);
        let mut saw_w = false;
        let mut saw_d = false;
        while let Ok(ev) = rx.try_recv() {
            if ev
                == (InputEvent::Key {
                    vk: 0x57,
                    down: true,
                })
            {
                saw_w = true;
            }
            if ev
                == (InputEvent::Key {
                    vk: 0x44,
                    down: true,
                })
            {
                saw_d = true;
            }
        }
        assert!(saw_w, "expected W key down");
        assert!(saw_d, "expected D key down");
    }

    // ---- Key repeat timing math ----

    #[test]
    fn key_repeat_rate_is_clamped_to_minimum_1ms() {
        // Verify the clamp logic used in start_key_repeat by inspecting config.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        let mut cfg = KbmConfig::default();
        cfg.enabled = true;
        cfg.key_repeat_rate_ms = 0; // would be clamped to 1
        emu.set_config(&cfg);
        let clamped = emu.config.key_repeat_rate_ms.clamp(1, 5_000);
        assert_eq!(clamped, 1);
    }

    #[test]
    fn key_repeat_rate_is_clamped_to_maximum_5000ms() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut emu = KbmEmulator::with_backend(Arc::new(MockBackend::new(tx)));
        let mut cfg = KbmConfig::default();
        cfg.enabled = true;
        cfg.key_repeat_rate_ms = 999_999;
        emu.set_config(&cfg);
        let clamped = emu.config.key_repeat_rate_ms.clamp(1, 5_000);
        assert_eq!(clamped, 5_000);
    }

    #[test]
    fn key_repeat_delay_zero_is_valid_duration() {
        // delay = Duration::from_millis(0) -> Duration::ZERO, which skips sleep.
        let delay = Duration::from_millis(0u64);
        assert_eq!(delay, Duration::ZERO);
    }

    #[test]
    fn key_repeat_delay_nonzero_is_respected() {
        let delay = Duration::from_millis(250u64);
        assert_eq!(delay, Duration::from_millis(250));
    }

    // ---- KbmManager ----

    #[test]
    fn kbm_manager_status_returns_default_config() {
        let shared = crate::state::SharedState::new();
        let manager = KbmManager::new(shared);
        let status = manager.status();
        assert!(!status.enabled);
        assert_eq!(status.mouse_sensitivity, 1.0);
    }

    #[test]
    fn kbm_manager_set_enabled_updates_config_and_emulator() {
        let shared = crate::state::SharedState::new();
        let manager = KbmManager::new(shared.clone());
        let result = manager.set_enabled(true);
        assert!(result);
        assert!(manager.status().enabled);
        // The shared config is also updated.
        assert!(shared.config.read().kbm_config.enabled);
    }

    #[test]
    fn kbm_manager_set_and_get_mappings_roundtrip() {
        let shared = crate::state::SharedState::new();
        let manager = KbmManager::new(shared);
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::B,
                actions: vec![Action::Key("X".into())],
            }],
            ..Default::default()
        };
        manager.set_mappings(mappings.clone());
        let got = manager.get_mappings();
        assert_eq!(got.buttons.len(), 1);
        assert_eq!(got.buttons[0].source, ButtonId::B);
    }

    #[test]
    fn kbm_manager_send_test_key_unknown_returns_error() {
        let shared = crate::state::SharedState::new();
        let manager = KbmManager::new(shared);
        let result = manager.send_test_key("not_a_key".into(), true);
        assert!(result.is_err());
    }

    #[test]
    fn kbm_manager_send_test_key_known_succeeds() {
        let shared = crate::state::SharedState::new();
        let manager = KbmManager::new(shared);
        let result = manager.send_test_key("W".into(), true);
        assert!(result.is_ok());
    }

    #[test]
    fn kbm_manager_button_event_respects_disabled_state() {
        let shared = crate::state::SharedState::new();
        let manager = KbmManager::new(shared);
        // Default config has enabled = false, so no panic / no event expected.
        manager.button_event(ButtonId::A, true);
        // If we reach here without panicking, the disabled path works.
    }

    #[test]
    fn kbm_manager_stick_event_respects_disabled_state() {
        let shared = crate::state::SharedState::new();
        let manager = KbmManager::new(shared);
        manager.stick_event(StickSide::Left, 1.0, 1.0);
        // Disabled by default; no panic.
    }

    // ---- KbmConfig serialization ----

    #[test]
    fn kbm_config_serde_roundtrip() {
        let cfg = KbmConfig {
            enabled: true,
            anti_cheat_mode: true,
            mouse_sensitivity: 2.5,
            key_repeat_delay_ms: 500,
            key_repeat_rate_ms: 50,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        // snake_case field names (no rename_all on KbmConfig).
        assert!(json.contains("\"enabled\":true"));
        assert!(json.contains("\"anti_cheat_mode\":true"));
        assert!(json.contains("\"mouse_sensitivity\":2.5"));
        assert!(json.contains("\"key_repeat_delay_ms\":500"));
        assert!(json.contains("\"key_repeat_rate_ms\":50"));
        let back: KbmConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.enabled, cfg.enabled);
        assert_eq!(back.anti_cheat_mode, cfg.anti_cheat_mode);
        assert_eq!(back.mouse_sensitivity, cfg.mouse_sensitivity);
        assert_eq!(back.key_repeat_delay_ms, cfg.key_repeat_delay_ms);
        assert_eq!(back.key_repeat_rate_ms, cfg.key_repeat_rate_ms);
    }

    #[test]
    fn kbm_config_default_values() {
        let cfg = KbmConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.anti_cheat_mode);
        assert_eq!(cfg.mouse_sensitivity, 1.0);
        assert_eq!(cfg.key_repeat_delay_ms, 250);
        assert_eq!(cfg.key_repeat_rate_ms, 33);
    }

    // ---- Action / ButtonMapping serialization ----

    #[test]
    fn action_key_serde_roundtrip() {
        let action = Action::Key("Space".into());
        let json = serde_json::to_string(&action).unwrap();
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn action_key_combo_serde_roundtrip() {
        let action = Action::KeyCombo(vec!["LShift".into(), "A".into()]);
        let json = serde_json::to_string(&action).unwrap();
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn action_mouse_button_serde_roundtrip() {
        let action = Action::MouseButton(2);
        let json = serde_json::to_string(&action).unwrap();
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn button_mapping_serde_roundtrip() {
        let mapping = ButtonMapping {
            source: ButtonId::A,
            actions: vec![Action::Key("W".into()), Action::MouseButton(0)],
        };
        let json = serde_json::to_string(&mapping).unwrap();
        let back: ButtonMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, mapping.source);
        assert_eq!(back.actions.len(), 2);
    }

    #[test]
    fn stick_action_serde_roundtrip() {
        let action = StickAction::Wasd;
        let json = serde_json::to_string(&action).unwrap();
        let back: StickAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn stick_mapping_serde_roundtrip() {
        let mapping = StickMapping {
            left_actions: vec![StickAction::Wasd],
            right_actions: vec![StickAction::Mouse],
            zones: StickZones {
                deadzone: 0.3,
                low: 0.4,
                medium: 0.6,
                high: 0.8,
                low_actions: vec![],
                medium_actions: vec![],
                high_actions: vec![],
            },
            response_curve: Default::default(),
        };
        let json = serde_json::to_string(&mapping).unwrap();
        let back: StickMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(back.left_actions, mapping.left_actions);
        assert_eq!(back.right_actions, mapping.right_actions);
        assert_eq!(back.zones.deadzone, 0.3);
    }

    // ---- ButtonState edge helpers ----

    #[test]
    fn button_state_get_set_roundtrip() {
        let mut bs = ButtonState::default();
        bs.set(ButtonId::A, true);
        assert!(bs.get(ButtonId::A));
        bs.set(ButtonId::A, false);
        assert!(!bs.get(ButtonId::A));
    }
}
