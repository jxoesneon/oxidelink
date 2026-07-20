//! Macro engine for OxideLink.
//!
//! Records controller/keyboard/mouse actions and plays them back with
//! deterministic timing. KB/M output delegates to [`crate::kbm::KbmEmulator`] via
//! [`crate::keycode::vk`] for named keys.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::State;
use tokio::sync::{broadcast, mpsc};
use tokio::time::sleep;

use crate::kbm::KbmEmulator;
use crate::state::{
    timestamp_now, ButtonId, ButtonState, ControllerState, IpcEvent, Macro, MacroStep, SharedState,
    StickSide, StickState, TriggerSide,
};

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MacroStore {
    path: PathBuf,
    macros: Arc<Mutex<Vec<Macro>>>,
}

impl MacroStore {
    pub fn macros_file_path() -> PathBuf {
        let mut path = dirs_next::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("OxideLink");
        path.push("macros.json");
        path
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            macros: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn load_from(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let list: Vec<Macro> = if path.exists() {
            let json = std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read macros file: {}", e))?;
            serde_json::from_str(&json)
                .map_err(|e| format!("Failed to parse macros file: {}", e))?
        } else {
            Vec::new()
        };
        Ok(Self {
            path,
            macros: Arc::new(Mutex::new(list)),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn load() -> Result<Self, String> {
        let path = Self::macros_file_path();
        let list: Vec<Macro> = if path.exists() {
            let json = std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read macros file: {}", e))?;
            serde_json::from_str(&json)
                .map_err(|e| format!("Failed to parse macros file: {}", e))?
        } else {
            Vec::new()
        };
        Ok(Self {
            path,
            macros: Arc::new(Mutex::new(list)),
        })
    }

    pub fn empty() -> Self {
        Self {
            path: Self::macros_file_path(),
            macros: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn list(&self) -> Vec<Macro> {
        self.macros.lock().clone()
    }

    pub fn get(&self, id: &str) -> Option<Macro> {
        self.macros.lock().iter().find(|m| m.id == id).cloned()
    }

    pub fn save(&self, mac: &Macro) -> Result<(), String> {
        {
            let mut list = self.macros.lock();
            if let Some(existing) = list.iter_mut().find(|m| m.id == mac.id) {
                *existing = mac.clone();
            } else {
                list.push(mac.clone());
            }
        }
        self.persist()
    }

    pub async fn save_async(&self, mac: &Macro) -> Result<(), String> {
        {
            let mut list = self.macros.lock();
            if let Some(existing) = list.iter_mut().find(|m| m.id == mac.id) {
                *existing = mac.clone();
            } else {
                list.push(mac.clone());
            }
        }
        self.persist_async().await
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        {
            let mut list = self.macros.lock();
            let before = list.len();
            list.retain(|m| m.id != id);
            if list.len() == before {
                return Ok(false);
            }
        }
        self.persist()?;
        Ok(true)
    }

    fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create macros dir: {}", e))?;
        }
        let list = self.macros.lock().clone();
        let json = serde_json::to_string_pretty(&list)
            .map_err(|e| format!("Failed to serialize macros: {}", e))?;
        std::fs::write(&self.path, json)
            .map_err(|e| format!("Failed to write macros file: {}", e))?;
        Ok(())
    }

    /// Async variant that runs blocking file I/O on a separate thread so the
    /// tokio runtime is not paused while macros are persisted.
    pub async fn persist_async(&self) -> Result<(), String> {
        let list = self.macros.lock().clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create macros dir: {}", e))?;
            }
            let json = serde_json::to_string_pretty(&list)
                .map_err(|e| format!("Failed to serialize macros: {}", e))?;
            std::fs::write(&path, json).map_err(|e| format!("Failed to write macros file: {}", e))
        })
        .await
        .map_err(|e| format!("blocking task failed: {}", e))?
    }
}

// ---------------------------------------------------------------------------
// Engine state
// ---------------------------------------------------------------------------

struct Recording {
    last_event: Instant,
    last_state: Option<ControllerState>,
    steps: Vec<MacroStep>,
}

struct EngineState {
    store: MacroStore,
    recording: Option<Recording>,
    record_stop_tx: Option<mpsc::Sender<()>>,
    record_join: Option<tokio::task::JoinHandle<Option<Macro>>>,
    play_stop_tx: Option<mpsc::Sender<()>>,
    is_playing: bool,
}

#[derive(Clone)]
pub struct MacroEngine {
    // Weak reference breaks a potential reference cycle: the SharedState holds
    // an Arc<Mutex<Option<MacroEngine>>> and the engine holds back a weak ref.
    shared: Weak<SharedState>,
    tx: broadcast::Sender<IpcEvent>,
    kbm: Option<Arc<KbmEmulator>>,
    state: Arc<Mutex<EngineState>>,
}

impl MacroEngine {
    pub fn new(
        shared: Arc<SharedState>,
        tx: broadcast::Sender<IpcEvent>,
        kbm: Option<Arc<KbmEmulator>>,
    ) -> Result<Self, String> {
        let store = MacroStore::load().unwrap_or_else(|e| {
            log::warn!("Failed to load macro store: {}", e);
            MacroStore::empty()
        });
        Ok(Self {
            shared: Arc::downgrade(&shared),
            tx,
            kbm,
            state: Arc::new(Mutex::new(EngineState {
                store,
                recording: None,
                record_stop_tx: None,
                record_join: None,
                play_stop_tx: None,
                is_playing: false,
            })),
        })
    }

    pub fn kbm(&self) -> Option<Arc<KbmEmulator>> {
        self.kbm.clone()
    }

    // -----------------------------------------------------------------------
    // Playback
    // -----------------------------------------------------------------------

    pub async fn play_macro(&self, mac: &Macro, kbm: Option<&KbmEmulator>) {
        // Bounded capacity 1 is sufficient for an idempotent stop signal.
        let (stop_tx, mut stop_rx) = mpsc::channel(1);
        {
            let mut state = self.state.lock();
            state.play_stop_tx = Some(stop_tx);
            state.is_playing = true;
        }

        for step in &mac.steps {
            let cancelled = tokio::select! {
                _ = stop_rx.recv() => true,
                _ = self.run_step(step, kbm) => false,
            };
            if cancelled {
                break;
            }
        }

        let _ = self.broadcast_state();
        {
            let mut state = self.state.lock();
            state.is_playing = false;
            state.play_stop_tx = None;
        }
    }

    async fn run_step(&self, step: &MacroStep, kbm: Option<&KbmEmulator>) {
        match step {
            MacroStep::WaitMs(ms) => {
                sleep(Duration::from_millis(*ms as u64)).await;
            }
            MacroStep::PressButton(btn) => {
                self.set_button(*btn, true);
            }
            MacroStep::ReleaseButton(btn) => {
                self.set_button(*btn, false);
            }
            MacroStep::KeyDown(key) => {
                if let Some(kbm) = kbm {
                    match crate::keycode::vk(key) {
                        Some(vk) => kbm.send_key(vk, true),
                        None => log::warn!("Unknown key name in macro: {}", key),
                    }
                }
            }
            MacroStep::KeyUp(key) => {
                if let Some(kbm) = kbm {
                    match crate::keycode::vk(key) {
                        Some(vk) => kbm.send_key(vk, false),
                        None => log::warn!("Unknown key name in macro: {}", key),
                    }
                }
            }
            MacroStep::MouseMove(dx, dy) => {
                if let Some(kbm) = kbm {
                    kbm.send_mouse_move(*dx as i32, *dy as i32);
                }
            }
            MacroStep::MouseDown(btn) => {
                if let Some(kbm) = kbm {
                    kbm.send_mouse_button(*btn, true);
                }
            }
            MacroStep::MouseUp(btn) => {
                if let Some(kbm) = kbm {
                    kbm.send_mouse_button(*btn, false);
                }
            }
            MacroStep::SetStick(side, x, y) => {
                self.set_stick(*side, *x, *y);
            }
            MacroStep::SetTrigger(side, value) => {
                self.set_trigger(*side, *value);
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.state.lock().is_playing
    }

    pub fn stop_playback(&self) -> bool {
        let mut state = self.state.lock();
        if let Some(tx) = state.play_stop_tx.take() {
            // Stop signals are idempotent; drop if the channel is full.
            let _ = tx.try_send(());
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------
    // Recording
    // -----------------------------------------------------------------------

    pub fn start_recording(&self) -> Result<(), String> {
        // Bounded capacity 1 is sufficient for an idempotent stop signal.
        let (stop_tx, stop_rx) = mpsc::channel(1);
        let event_rx = self.tx.subscribe();
        let recording = Recording {
            last_event: Instant::now(),
            last_state: None,
            steps: Vec::new(),
        };
        let mut state = self.state.lock();
        if state.recording.is_some() {
            return Err("Already recording".into());
        }
        state.recording = Some(recording);
        state.record_stop_tx = Some(stop_tx);
        let engine = self.clone();
        let handle = tokio::spawn(async move { engine.record_macro(stop_rx, event_rx).await });
        state.record_join = Some(handle);
        Ok(())
    }

    pub async fn stop_recording(&self, name: String) -> Result<Macro, String> {
        let handle = {
            let mut state = self.state.lock();
            if let Some(tx) = state.record_stop_tx.take() {
                // Stop signals are idempotent; drop if the channel is full.
                let _ = tx.try_send(());
            }
            state.record_join.take()
        };
        let steps = match handle {
            Some(h) => {
                h.await
                    .map_err(|e| format!("Recording task failed: {}", e))?
                    .unwrap_or_default()
                    .steps
            }
            None => return Err("Not recording".into()),
        };
        let id = {
            let state = self.state.lock();
            unique_macro_id(&state.store)
        };
        let mac = Macro { id, name, steps };
        self.save_async(&mac).await?;
        Ok(mac)
    }

    pub async fn record_macro(
        &self,
        mut stop_rx: mpsc::Receiver<()>,
        mut event_rx: broadcast::Receiver<IpcEvent>,
    ) -> Option<Macro> {
        loop {
            tokio::select! {
                _ = stop_rx.recv() => {
                    return self.finalize_recording();
                }
                event = event_rx.recv() => {
                    match event {
                        Ok(IpcEvent::ControllerState { data }) => {
                            self.handle_controller_state_during_record(data);
                        }
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
            }
        }
    }

    fn handle_controller_state_during_record(&self, data: ControllerState) {
        let mut state = self.state.lock();
        let Some(recording) = state.recording.as_mut() else {
            return;
        };
        let default_state = ControllerState::default();
        let prev = recording.last_state.as_ref().unwrap_or(&default_state);
        let now = Instant::now();
        let delta = now.duration_since(recording.last_event);
        let mut steps = Vec::new();
        if delta.as_millis() > 0 {
            steps.push(MacroStep::WaitMs(delta.as_millis() as u32));
        }
        steps.extend(diff_controller_states(prev, &data));
        recording.steps.append(&mut steps);
        recording.last_state = Some(data);
        recording.last_event = now;
    }

    fn finalize_recording(&self) -> Option<Macro> {
        let id = {
            let state = self.state.lock();
            unique_macro_id(&state.store)
        };
        let mut state = self.state.lock();
        let recording = state.recording.take()?;
        Some(Macro {
            id,
            name: "Unnamed recording".into(),
            steps: recording.steps,
        })
    }

    pub fn is_recording(&self) -> bool {
        self.state.lock().recording.is_some()
    }

    /// Record one controller-state frame. `timestamp` is stored with the frame
    /// but the recording timing uses `Instant::now()`.
    pub fn record_frame(&self, state: &ControllerState, _timestamp: u64) {
        self.handle_controller_state_during_record(state.clone());
    }

    // -----------------------------------------------------------------------
    // Store helpers
    // -----------------------------------------------------------------------

    pub fn list(&self) -> Vec<Macro> {
        self.state.lock().store.list()
    }

    pub fn load(&self, id: &str) -> Option<Macro> {
        self.state.lock().store.get(id)
    }

    pub fn save(&self, mac: &Macro) -> Result<(), String> {
        self.state.lock().store.save(mac)
    }

    pub async fn save_async(&self, mac: &Macro) -> Result<(), String> {
        let store = self.state.lock().store.clone();
        store.save_async(mac).await
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        self.state.lock().store.delete(id)
    }

    // -----------------------------------------------------------------------
    // ControllerState helpers
    // -----------------------------------------------------------------------

    fn set_button(&self, btn: ButtonId, pressed: bool) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut controller = shared.active_controller_mut();
        set_button_state(&mut controller.buttons, btn, pressed);
        let data = controller.clone();
        drop(controller);
        let _ = self.tx.send(IpcEvent::ControllerState { data });
    }

    fn set_stick(&self, side: StickSide, x: f32, y: f32) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut controller = shared.active_controller_mut();
        match side {
            StickSide::Left => set_stick_state(&mut controller.left_stick, x, y),
            StickSide::Right => set_stick_state(&mut controller.right_stick, x, y),
        }
        let data = controller.clone();
        drop(controller);
        let _ = self.tx.send(IpcEvent::ControllerState { data });
    }

    fn set_trigger(&self, side: TriggerSide, value: f32) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut controller = shared.active_controller_mut();
        match side {
            TriggerSide::Left => {
                controller.left_trigger = value;
                controller.buttons.zl = value > 0.5;
            }
            TriggerSide::Right => {
                controller.right_trigger = value;
                controller.buttons.zr = value > 0.5;
            }
        }
        let data = controller.clone();
        drop(controller);
        let _ = self.tx.send(IpcEvent::ControllerState { data });
    }

    #[allow(clippy::result_large_err)] // Err variant size is inherent to the broadcast::SendError type
    fn broadcast_state(&self) -> Result<usize, broadcast::error::SendError<IpcEvent>> {
        let Some(shared) = self.shared.upgrade() else {
            return Err(broadcast::error::SendError(IpcEvent::ControllerState {
                data: ControllerState::default(),
            }));
        };
        let data = shared.active_controller().clone();
        self.tx.send(IpcEvent::ControllerState { data })
    }
}

// ---------------------------------------------------------------------------
// Free helper functions
// ---------------------------------------------------------------------------

fn set_button_state(buttons: &mut ButtonState, btn: ButtonId, pressed: bool) {
    match btn {
        ButtonId::A => buttons.a = pressed,
        ButtonId::B => buttons.b = pressed,
        ButtonId::X => buttons.x = pressed,
        ButtonId::Y => buttons.y = pressed,
        ButtonId::Up => buttons.dpad_up = pressed,
        ButtonId::Down => buttons.dpad_down = pressed,
        ButtonId::Left => buttons.dpad_left = pressed,
        ButtonId::Right => buttons.dpad_right = pressed,
        ButtonId::L => buttons.l = pressed,
        ButtonId::R => buttons.r = pressed,
        ButtonId::Zl => buttons.zl = pressed,
        ButtonId::Zr => buttons.zr = pressed,
        ButtonId::Minus => buttons.minus = pressed,
        ButtonId::Plus => buttons.plus = pressed,
        ButtonId::Home => buttons.home = pressed,
        ButtonId::Capture => buttons.capture = pressed,
        ButtonId::LStick => buttons.stick_l = pressed,
        ButtonId::RStick => buttons.stick_r = pressed,
    }
}

fn set_stick_state(stick: &mut StickState, x: f32, y: f32) {
    stick.x = x.clamp(-1.0, 1.0);
    stick.y = y.clamp(-1.0, 1.0);
    stick.raw_x = normalized_to_raw(stick.x);
    stick.raw_y = normalized_to_raw(stick.y);
}

fn normalized_to_raw(v: f32) -> u16 {
    let raw = ((v + 1.0) / 2.0 * 0xFFF as f32).round() as u16;
    raw.min(0xFFF)
}

macro_rules! diff_button {
    ($prev:expr, $next:expr, $steps:expr, $field:ident, $id:expr) => {
        if $prev.buttons.$field != $next.buttons.$field {
            if $next.buttons.$field {
                $steps.push(MacroStep::PressButton($id));
            } else {
                $steps.push(MacroStep::ReleaseButton($id));
            }
        }
    };
}

fn diff_controller_states(prev: &ControllerState, next: &ControllerState) -> Vec<MacroStep> {
    let mut steps = Vec::new();

    diff_button!(prev, next, steps, a, ButtonId::A);
    diff_button!(prev, next, steps, b, ButtonId::B);
    diff_button!(prev, next, steps, x, ButtonId::X);
    diff_button!(prev, next, steps, y, ButtonId::Y);
    diff_button!(prev, next, steps, dpad_up, ButtonId::Up);
    diff_button!(prev, next, steps, dpad_down, ButtonId::Down);
    diff_button!(prev, next, steps, dpad_left, ButtonId::Left);
    diff_button!(prev, next, steps, dpad_right, ButtonId::Right);
    diff_button!(prev, next, steps, l, ButtonId::L);
    diff_button!(prev, next, steps, r, ButtonId::R);
    diff_button!(prev, next, steps, zl, ButtonId::Zl);
    diff_button!(prev, next, steps, zr, ButtonId::Zr);
    diff_button!(prev, next, steps, minus, ButtonId::Minus);
    diff_button!(prev, next, steps, plus, ButtonId::Plus);
    diff_button!(prev, next, steps, home, ButtonId::Home);
    diff_button!(prev, next, steps, capture, ButtonId::Capture);
    diff_button!(prev, next, steps, stick_l, ButtonId::LStick);
    diff_button!(prev, next, steps, stick_r, ButtonId::RStick);

    if (prev.left_stick.x - next.left_stick.x).abs() > 0.01
        || (prev.left_stick.y - next.left_stick.y).abs() > 0.01
    {
        steps.push(MacroStep::SetStick(
            StickSide::Left,
            next.left_stick.x,
            next.left_stick.y,
        ));
    }
    if (prev.right_stick.x - next.right_stick.x).abs() > 0.01
        || (prev.right_stick.y - next.right_stick.y).abs() > 0.01
    {
        steps.push(MacroStep::SetStick(
            StickSide::Right,
            next.right_stick.x,
            next.right_stick.y,
        ));
    }

    if (prev.left_trigger - next.left_trigger).abs() > 0.1 {
        steps.push(MacroStep::SetTrigger(TriggerSide::Left, next.left_trigger));
    }
    if (prev.right_trigger - next.right_trigger).abs() > 0.1 {
        steps.push(MacroStep::SetTrigger(
            TriggerSide::Right,
            next.right_trigger,
        ));
    }

    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kbm::{InputEvent, MockBackend};
    use crate::state::{ButtonState, StickState};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique counter so parallel tests don't collide on the same temp path.
    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_store() -> MacroStore {
        let n = TEST_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "oxidelink-macro-engine-test-{}-{}.json",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        MacroStore::with_path(path)
    }

    fn test_engine() -> (
        std::sync::Arc<SharedState>,
        MacroEngine,
        broadcast::Receiver<IpcEvent>,
    ) {
        let shared = SharedState::new();
        let (tx, rx) = broadcast::channel(64);
        let engine = MacroEngine::new(shared.clone(), tx, None).unwrap();
        (shared, engine, rx)
    }

    fn make_macro(id: &str, steps: Vec<MacroStep>) -> Macro {
        Macro {
            id: id.into(),
            name: id.into(),
            steps,
        }
    }

    // -- Existing playback smoke test ----------------------------------------

    #[tokio::test]
    async fn playback_preserves_step_order_with_mock_backend() -> Result<(), String> {
        let shared = SharedState::new();
        let (event_tx, _event_rx) = broadcast::channel(8);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let kbm = KbmEmulator::with_backend(Arc::new(MockBackend::new(input_tx)));
        let engine = MacroEngine::new(shared.clone(), event_tx, None)?;
        let macro_steps = Macro {
            id: "smoke".into(),
            name: "smoke".into(),
            steps: vec![
                MacroStep::KeyDown("a".into()),
                MacroStep::MouseMove(3, -2),
                MacroStep::PressButton(ButtonId::A),
                MacroStep::WaitMs(1),
                MacroStep::KeyUp("a".into()),
            ],
        };

        engine.play_macro(&macro_steps, Some(&kbm)).await;

        assert_eq!(
            input_rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: true
            })
        );
        assert_eq!(
            input_rx.try_recv(),
            Ok(InputEvent::MouseMove { dx: 3, dy: -2 })
        );
        assert_eq!(
            input_rx.try_recv(),
            Ok(InputEvent::Key {
                vk: 0x41,
                down: false
            })
        );
        assert!(input_rx.try_recv().is_err());
        assert!(shared.active_controller().buttons.a);
        Ok::<(), String>(())
    }

    // -- MacroStore: with_path, load_from, path ------------------------------

    #[test]
    fn store_with_path_starts_empty() {
        let store = temp_store();
        assert!(store.list().is_empty());
    }

    #[test]
    fn store_path_returns_configured_path() {
        let path = std::env::temp_dir().join("store-path-test.json");
        let store = MacroStore::with_path(&path);
        assert_eq!(store.path(), std::path::Path::new(&path));
    }

    #[test]
    fn store_load_from_nonexistent_returns_empty() {
        let path = std::env::temp_dir().join("nonexistent-macro-12345.json");
        let _ = std::fs::remove_file(&path);
        let store = MacroStore::load_from(&path).unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn store_load_from_invalid_json_returns_error() {
        let n = TEST_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("bad-macro-{}-{}.json", std::process::id(), n));
        std::fs::write(&path, "not valid json").unwrap();
        let result = MacroStore::load_from(&path);
        assert!(result.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn store_save_inserts_new_macro() {
        let store = temp_store();
        let mac = make_macro("new-1", vec![MacroStep::WaitMs(10)]);
        store.save(&mac).unwrap();
        assert_eq!(store.list().len(), 1);
        assert!(store.get("new-1").is_some());
    }

    #[test]
    fn store_save_updates_existing_macro_by_id() {
        let store = temp_store();
        let mac = make_macro("upd-1", vec![MacroStep::WaitMs(10)]);
        store.save(&mac).unwrap();

        let mut updated = mac.clone();
        updated.name = "updated-name".into();
        updated.steps = vec![MacroStep::WaitMs(20)];
        store.save(&updated).unwrap();

        assert_eq!(store.list().len(), 1);
        let loaded = store.get("upd-1").unwrap();
        assert_eq!(loaded.name, "updated-name");
        assert_eq!(loaded.steps.len(), 1);
    }

    #[test]
    fn store_delete_returns_false_for_unknown_id() {
        let store = temp_store();
        let result = store.delete("unknown").unwrap();
        assert!(!result);
    }

    #[test]
    fn store_delete_removes_macro_and_persists() {
        let store = temp_store();
        let mac = make_macro("del-1", vec![MacroStep::WaitMs(5)]);
        store.save(&mac).unwrap();
        assert!(store.get("del-1").is_some());

        let deleted = store.delete("del-1").unwrap();
        assert!(deleted);
        assert!(store.get("del-1").is_none());
        assert!(store.list().is_empty());
    }

    #[test]
    fn store_get_returns_none_for_unknown() {
        let store = temp_store();
        assert!(store.get("nope").is_none());
    }

    #[tokio::test]
    async fn store_save_async_persists_to_disk() {
        let store = temp_store();
        let mac = make_macro("async-1", vec![MacroStep::WaitMs(1)]);
        store.save_async(&mac).await.unwrap();

        let loaded = MacroStore::load_from(store.path()).unwrap();
        assert_eq!(loaded.list().len(), 1);
        assert_eq!(loaded.get("async-1").unwrap().name, "async-1");
    }

    #[tokio::test]
    async fn store_persist_async_writes_file() {
        let store = temp_store();
        store.save(&make_macro("p-1", vec![MacroStep::WaitMs(1)])).unwrap();
        // Overwrite via persist_async after modifying in-memory.
        {
            let mut list = store.macros.lock();
            list[0].name = "persisted-async".into();
        }
        store.persist_async().await.unwrap();
        let data = std::fs::read_to_string(store.path()).unwrap();
        assert!(data.contains("persisted-async"));
    }

    // -- MacroStep variants and serialization --------------------------------

    #[test]
    fn macro_step_default_is_wait_ms_zero() {
        let step = MacroStep::default();
        assert_eq!(step, MacroStep::WaitMs(0));
    }

    #[test]
    fn macro_step_wait_ms_serialization() {
        let step = MacroStep::WaitMs(250);
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"type\":\"wait_ms\""));
        assert!(json.contains("250"));
        let back: MacroStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn macro_step_press_button_serialization() {
        let step = MacroStep::PressButton(ButtonId::A);
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"type\":\"press_button\""));
        assert!(json.contains("\"a\""));
        let back: MacroStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn macro_step_release_button_serialization() {
        let step = MacroStep::ReleaseButton(ButtonId::B);
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"type\":\"release_button\""));
        let back: MacroStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn macro_step_key_down_up_serialization() {
        let down = MacroStep::KeyDown("ctrl".into());
        let up = MacroStep::KeyUp("v".into());
        let down_json = serde_json::to_string(&down).unwrap();
        let up_json = serde_json::to_string(&up).unwrap();
        assert!(down_json.contains("\"type\":\"key_down\""));
        assert!(up_json.contains("\"type\":\"key_up\""));
        assert_eq!(serde_json::from_str::<MacroStep>(&down_json).unwrap(), down);
        assert_eq!(serde_json::from_str::<MacroStep>(&up_json).unwrap(), up);
    }

    #[test]
    fn macro_step_mouse_move_serialization() {
        let step = MacroStep::MouseMove(100, -50);
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"type\":\"mouse_move\""));
        let back: MacroStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn macro_step_mouse_down_up_serialization() {
        let down = MacroStep::MouseDown(0);
        let up = MacroStep::MouseUp(1);
        let down_json = serde_json::to_string(&down).unwrap();
        let up_json = serde_json::to_string(&up).unwrap();
        assert!(down_json.contains("\"type\":\"mouse_down\""));
        assert!(up_json.contains("\"type\":\"mouse_up\""));
        assert_eq!(serde_json::from_str::<MacroStep>(&down_json).unwrap(), down);
        assert_eq!(serde_json::from_str::<MacroStep>(&up_json).unwrap(), up);
    }

    #[test]
    fn macro_step_set_stick_serialization() {
        let step = MacroStep::SetStick(StickSide::Left, 0.5, -0.5);
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"type\":\"set_stick\""));
        assert!(json.contains("\"left\""));
        let back: MacroStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn macro_step_set_trigger_serialization() {
        let step = MacroStep::SetTrigger(TriggerSide::Right, 0.75);
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"type\":\"set_trigger\""));
        assert!(json.contains("\"right\""));
        let back: MacroStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn macro_full_serialization_all_variants() {
        let mac = make_macro(
            "full",
            vec![
                MacroStep::WaitMs(100),
                MacroStep::PressButton(ButtonId::A),
                MacroStep::ReleaseButton(ButtonId::B),
                MacroStep::KeyDown("ctrl".into()),
                MacroStep::KeyUp("v".into()),
                MacroStep::MouseMove(100, -50),
                MacroStep::MouseDown(0),
                MacroStep::MouseUp(0),
                MacroStep::SetStick(StickSide::Left, 0.5, -0.5),
                MacroStep::SetTrigger(TriggerSide::Right, 0.75),
            ],
        );
        let json = serde_json::to_string(&mac).unwrap();
        let back: Macro = serde_json::from_str(&json).unwrap();
        assert_eq!(mac, back);
    }

    // -- MacroEngine playback: is_playing, cancel ----------------------------

    #[tokio::test]
    async fn is_playing_false_when_idle() {
        let (_shared, engine, _rx) = test_engine();
        assert!(!engine.is_playing());
    }

    #[tokio::test]
    async fn stop_playback_returns_false_when_not_playing() {
        let (_shared, engine, _rx) = test_engine();
        assert!(!engine.stop_playback());
    }

    #[tokio::test]
    async fn play_macro_sets_is_playing_during_playback() {
        let (_shared, engine, _rx) = test_engine();
        let mac = make_macro("play-state", vec![MacroStep::WaitMs(2000)]);
        let engine2 = engine.clone();
        let mac2 = mac.clone();
        tokio::spawn(async move {
            engine2.play_macro(&mac2, None).await;
        });
        sleep(Duration::from_millis(10)).await;
        assert!(engine.is_playing());
        engine.stop_playback();
        sleep(Duration::from_millis(20)).await;
        assert!(!engine.is_playing());
    }

    #[tokio::test]
    async fn play_macro_empty_steps_completes_immediately() {
        let (_shared, engine, _rx) = test_engine();
        let mac = make_macro("empty", vec![]);
        engine.play_macro(&mac, None).await;
        assert!(!engine.is_playing());
    }

    #[tokio::test]
    async fn play_macro_cancel_skips_remaining_steps() {
        let (_shared, engine, _rx) = test_engine();
        let mac = make_macro(
            "cancel-test",
            vec![
                MacroStep::WaitMs(5000),
                MacroStep::PressButton(ButtonId::A),
            ],
        );
        let engine2 = engine.clone();
        let mac2 = mac.clone();
        tokio::spawn(async move {
            engine2.play_macro(&mac2, None).await;
        });
        sleep(Duration::from_millis(10)).await;
        assert!(engine.is_playing());
        assert!(engine.stop_playback());
        sleep(Duration::from_millis(20)).await;
        assert!(!engine.is_playing());
        // The PressButton step should have been skipped.
        assert!(!_shared.active_controller().buttons.a);
    }

    // -- Macro recording -----------------------------------------------------

    #[tokio::test]
    async fn start_recording_sets_is_recording_true() {
        let (_shared, engine, _rx) = test_engine();
        assert!(!engine.is_recording());
        engine.start_recording().unwrap();
        assert!(engine.is_recording());
        // Clean up: stop recording (name doesn't matter for this test).
        // We need to send a controller state or just stop.
        let _ = engine.stop_recording("test".into()).await;
    }

    #[tokio::test]
    async fn start_recording_twice_errors() {
        let (_shared, engine, _rx) = test_engine();
        engine.start_recording().unwrap();
        assert!(engine.start_recording().is_err());
        let _ = engine.stop_recording("test".into()).await;
    }

    #[tokio::test]
    async fn stop_recording_without_start_errors() {
        let (_shared, engine, _rx) = test_engine();
        let result = engine.stop_recording("test".into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn record_frame_captures_button_press() {
        let (shared, engine, _rx) = test_engine();
        engine.start_recording().unwrap();

        let mut state = ControllerState::default();
        state.buttons.a = true;
        engine.record_frame(&state, 1000);

        sleep(Duration::from_millis(5)).await;

        let mac = engine.stop_recording("rec-frame".into()).await.unwrap();
        assert!(mac
            .steps
            .iter()
            .any(|s| matches!(s, MacroStep::PressButton(ButtonId::A))));
        let _ = &shared;
    }

    #[tokio::test]
    async fn record_frame_captures_stick_movement() {
        let (_shared, engine, _rx) = test_engine();
        engine.start_recording().unwrap();

        let mut state = ControllerState::default();
        state.left_stick.x = 0.8;
        state.left_stick.y = -0.3;
        engine.record_frame(&state, 2000);

        sleep(Duration::from_millis(5)).await;

        let mac = engine.stop_recording("rec-stick".into()).await.unwrap();
        assert!(mac
            .steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetStick(StickSide::Left, _, _))));
    }

    #[tokio::test]
    async fn record_frame_captures_trigger_change() {
        let (_shared, engine, _rx) = test_engine();
        engine.start_recording().unwrap();

        let mut state = ControllerState::default();
        state.right_trigger = 0.9;
        engine.record_frame(&state, 3000);

        sleep(Duration::from_millis(5)).await;

        let mac = engine.stop_recording("rec-trigger".into()).await.unwrap();
        assert!(mac
            .steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetTrigger(TriggerSide::Right, _))));
    }

    // -- Macro validation: unique_macro_id -----------------------------------

    #[test]
    fn unique_macro_id_returns_base_when_not_taken() {
        let store = temp_store();
        let id = unique_macro_id(&store);
        assert!(id.starts_with("macro-"));
    }

    #[test]
    fn unique_macro_id_appends_suffix_when_base_taken() {
        let store = temp_store();
        // Manually insert a macro with the base id pattern.
        let base = format!("macro-{}", timestamp_now());
        let mac = make_macro(&base, vec![MacroStep::WaitMs(1)]);
        store.save(&mac).unwrap();

        let id = unique_macro_id(&store);
        // The generated id should differ from the base since base is taken.
        assert_ne!(id, base);
        assert!(id.starts_with("macro-"));
    }

    // -- Pure helper functions ------------------------------------------------

    #[test]
    fn set_button_state_sets_each_button() {
        let mut buttons = ButtonState::default();
        set_button_state(&mut buttons, ButtonId::A, true);
        assert!(buttons.a);
        set_button_state(&mut buttons, ButtonId::B, true);
        assert!(buttons.b);
        set_button_state(&mut buttons, ButtonId::Up, true);
        assert!(buttons.dpad_up);
        set_button_state(&mut buttons, ButtonId::Down, true);
        assert!(buttons.dpad_down);
        set_button_state(&mut buttons, ButtonId::Left, true);
        assert!(buttons.dpad_left);
        set_button_state(&mut buttons, ButtonId::Right, true);
        assert!(buttons.dpad_right);
        set_button_state(&mut buttons, ButtonId::L, true);
        assert!(buttons.l);
        set_button_state(&mut buttons, ButtonId::R, true);
        assert!(buttons.r);
        set_button_state(&mut buttons, ButtonId::Zl, true);
        assert!(buttons.zl);
        set_button_state(&mut buttons, ButtonId::Zr, true);
        assert!(buttons.zr);
        set_button_state(&mut buttons, ButtonId::Minus, true);
        assert!(buttons.minus);
        set_button_state(&mut buttons, ButtonId::Plus, true);
        assert!(buttons.plus);
        set_button_state(&mut buttons, ButtonId::Home, true);
        assert!(buttons.home);
        set_button_state(&mut buttons, ButtonId::Capture, true);
        assert!(buttons.capture);
        set_button_state(&mut buttons, ButtonId::LStick, true);
        assert!(buttons.stick_l);
        set_button_state(&mut buttons, ButtonId::RStick, true);
        assert!(buttons.stick_r);
    }

    #[test]
    fn set_button_state_clears_button() {
        let mut buttons = ButtonState::default();
        set_button_state(&mut buttons, ButtonId::A, true);
        assert!(buttons.a);
        set_button_state(&mut buttons, ButtonId::A, false);
        assert!(!buttons.a);
    }

    #[test]
    fn set_stick_state_clamps_to_range() {
        let mut stick = StickState::default();
        set_stick_state(&mut stick, 2.0, -2.0);
        assert_eq!(stick.x, 1.0);
        assert_eq!(stick.y, -1.0);
    }

    #[test]
    fn set_stick_state_updates_raw_values() {
        let mut stick = StickState::default();
        set_stick_state(&mut stick, 0.0, 0.0);
        // Center (0,0) maps to mid-range raw value.
        assert_eq!(stick.raw_x, normalized_to_raw(0.0));
        assert_eq!(stick.raw_y, normalized_to_raw(0.0));
    }

    #[test]
    fn normalized_to_raw_center_is_mid_range() {
        let raw = normalized_to_raw(0.0);
        assert_eq!(raw, 0x800); // (0+1)/2 * 0xFFF = 2047.5 -> 2048
    }

    #[test]
    fn normalized_to_raw_min_is_zero() {
        let raw = normalized_to_raw(-1.0);
        assert_eq!(raw, 0);
    }

    #[test]
    fn normalized_to_raw_max_is_full_scale() {
        let raw = normalized_to_raw(1.0);
        assert_eq!(raw, 0xFFF);
    }

    #[test]
    fn normalized_to_raw_clamps_overflow() {
        let raw = normalized_to_raw(2.0);
        assert_eq!(raw, 0xFFF);
    }

    #[test]
    fn diff_controller_states_no_change_returns_empty() {
        let state = ControllerState::default();
        let steps = diff_controller_states(&state, &state);
        assert!(steps.is_empty());
    }

    #[test]
    fn diff_controller_states_button_press_generates_press_step() {
        let prev = ControllerState::default();
        let mut next = ControllerState::default();
        next.buttons.a = true;
        let steps = diff_controller_states(&prev, &next);
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::PressButton(ButtonId::A))));
    }

    #[test]
    fn diff_controller_states_button_release_generates_release_step() {
        let mut prev = ControllerState::default();
        prev.buttons.b = true;
        let next = ControllerState::default();
        let steps = diff_controller_states(&prev, &next);
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::ReleaseButton(ButtonId::B))));
    }

    #[test]
    fn diff_controller_states_stick_change_generates_set_stick() {
        let prev = ControllerState::default();
        let mut next = ControllerState::default();
        next.left_stick.x = 0.5;
        let steps = diff_controller_states(&prev, &next);
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetStick(StickSide::Left, 0.5, _))));
    }

    #[test]
    fn diff_controller_states_stick_below_threshold_no_step() {
        let prev = ControllerState::default();
        let mut next = ControllerState::default();
        next.left_stick.x = 0.005; // below 0.01 threshold
        let steps = diff_controller_states(&prev, &next);
        assert!(!steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetStick(StickSide::Left, _, _))));
    }

    #[test]
    fn diff_controller_states_trigger_change_generates_set_trigger() {
        let prev = ControllerState::default();
        let mut next = ControllerState::default();
        next.left_trigger = 0.5;
        let steps = diff_controller_states(&prev, &next);
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetTrigger(TriggerSide::Left, _))));
    }

    #[test]
    fn diff_controller_states_trigger_below_threshold_no_step() {
        let prev = ControllerState::default();
        let mut next = ControllerState::default();
        next.right_trigger = 0.05; // below 0.1 threshold
        let steps = diff_controller_states(&prev, &next);
        assert!(!steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetTrigger(TriggerSide::Right, _))));
    }

    #[test]
    fn diff_controller_states_multiple_changes_all_captured() {
        let prev = ControllerState::default();
        let mut next = ControllerState::default();
        next.buttons.a = true;
        next.buttons.x = true;
        next.right_stick.y = -0.8;
        next.right_trigger = 0.9;
        let steps = diff_controller_states(&prev, &next);
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::PressButton(ButtonId::A))));
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::PressButton(ButtonId::X))));
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetStick(StickSide::Right, _, _))));
        assert!(steps
            .iter()
            .any(|s| matches!(s, MacroStep::SetTrigger(TriggerSide::Right, _))));
    }

    // -- MacroEngine store helpers -------------------------------------------

    #[test]
    fn engine_list_returns_store_contents() {
        let store = temp_store();
        // We can't easily inject a store into MacroEngine, but we can test
        // the list/save/delete path through the engine's own store.
        // MacroEngine::new uses MacroStore::load() which reads the default
        // config path. Instead, test the store directly.
        let mac = make_macro("list-1", vec![MacroStep::WaitMs(1)]);
        store.save(&mac).unwrap();
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn engine_save_and_load_roundtrip_through_store() {
        let store = temp_store();
        let mac = make_macro("rt-1", vec![MacroStep::PressButton(ButtonId::Y)]);
        store.save(&mac).unwrap();
        let loaded = store.get("rt-1").unwrap();
        assert_eq!(loaded.steps.len(), 1);
        assert_eq!(loaded.steps[0], MacroStep::PressButton(ButtonId::Y));
    }
}

fn unique_macro_id(store: &MacroStore) -> String {
    let base = format!("macro-{}", timestamp_now());
    if store.get(&base).is_none() {
        return base;
    }
    for n in 1..1000 {
        let candidate = format!("{}-{}", base, n);
        if store.get(&candidate).is_none() {
            return candidate;
        }
    }
    base
}

// ---------------------------------------------------------------------------
// Tauri-managed state + commands
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MacroState {
    engine: MacroEngine,
}

impl MacroState {
    pub fn new(shared: Arc<SharedState>, tx: broadcast::Sender<IpcEvent>) -> Result<Self, String> {
        let engine = MacroEngine::new(shared.clone(), tx, Some(Arc::new(KbmEmulator::new())))?;
        *shared.macro_engine.lock() = Some(engine.clone());
        Ok(Self { engine })
    }

    pub fn engine(&self) -> MacroEngine {
        self.engine.clone()
    }
}

// NOTE: these commands are free functions and are intentionally not yet wired
// into `main.rs` `invoke_handler`. A later subagent will wire them.

#[tauri::command]
pub fn macro_list(ctx: State<'_, MacroState>) -> Result<Vec<Macro>, String> {
    Ok(ctx.engine().list())
}

#[tauri::command]
pub fn macro_create(ctx: State<'_, MacroState>, mut mac: Macro) -> Result<Macro, String> {
    let engine = ctx.engine();
    if mac.id.is_empty() {
        let base = format!("macro-{}", timestamp_now());
        let mut id = base.clone();
        let mut n = 1;
        while engine.load(&id).is_some() {
            id = format!("{}-{}", base, n);
            n += 1;
            if n > 1000 {
                return Err("Could not generate unique macro id".into());
            }
        }
        mac.id = id;
    }
    if engine.load(&mac.id).is_some() {
        return Err(format!("Macro with id {} already exists", mac.id));
    }
    engine.save(&mac)?;
    Ok(mac)
}

#[tauri::command]
pub fn macro_update(ctx: State<'_, MacroState>, mac: Macro) -> Result<Macro, String> {
    let engine = ctx.engine();
    if mac.id.is_empty() {
        return Err("Macro id is empty".into());
    }
    if engine.load(&mac.id).is_none() {
        return Err(format!("Macro with id {} not found", mac.id));
    }
    engine.save(&mac)?;
    Ok(mac)
}

#[tauri::command]
pub fn macro_delete(ctx: State<'_, MacroState>, id: String) -> Result<bool, String> {
    ctx.engine().delete(&id)
}

#[tauri::command]
pub fn macro_load(ctx: State<'_, MacroState>, id: String) -> Result<Option<Macro>, String> {
    Ok(ctx.engine().load(&id))
}

#[tauri::command]
pub async fn macro_play(ctx: State<'_, MacroState>, id: String) -> Result<bool, String> {
    let engine = ctx.engine();
    let mac = engine
        .load(&id)
        .ok_or_else(|| format!("Macro {} not found", id))?;
    let kbm = engine.kbm();
    tokio::spawn(async move {
        engine.play_macro(&mac, kbm.as_deref()).await;
    });
    Ok(true)
}

#[tauri::command]
pub fn macro_stop(ctx: State<'_, MacroState>) -> Result<bool, String> {
    Ok(ctx.engine().stop_playback())
}

#[tauri::command]
pub async fn macro_record_start(ctx: State<'_, MacroState>) -> Result<bool, String> {
    ctx.engine().start_recording()?;
    Ok(true)
}

#[tauri::command]
pub async fn macro_record_stop(ctx: State<'_, MacroState>, name: String) -> Result<Macro, String> {
    ctx.engine().stop_recording(name).await
}
