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
