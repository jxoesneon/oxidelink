//! Profile manager with per-process / per-game auto-switching.
//!
//! Wraps [`crate::state::ProfileManager`] and exposes a thread-safe,
//! serializable CRUD interface plus an [`AutoSwitcher`] that polls the active
//! Windows process every second.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use regex::Regex;
use tokio::sync::broadcast;

use crate::state::{IpcEvent, ProfileManager as ProfileManagerState};

pub use crate::state::{AutoRule, AutoRuleKind, MatchMode, Profile};

/// Default file name inside the user's application data directory.
const PROFILE_FILE_NAME: &str = "profiles.json";

/// Returns the default on-disk location for the profile store.
fn default_profile_path() -> PathBuf {
    profile_store_base_dir().join(PROFILE_FILE_NAME)
}

fn profile_store_base_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("OxideLink")
}

/// Ensure `path` is absolute and resolves to a location inside `base`,
/// rejecting path traversal and symlinks that escape the base directory.
/// For write paths the file itself may not exist yet; in that case the parent
/// directory is canonicalized instead.
fn validate_path_within_base(
    path: &Path,
    base: &Path,
    allow_nonexistent: bool,
) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("path '{}' must be absolute", path.display()));
    }
    std::fs::create_dir_all(base)
        .map_err(|e| format!("failed to create base dir '{}': {}", base.display(), e))?;
    let canonical_base = base
        .canonicalize()
        .map_err(|e| format!("failed to resolve base dir '{}': {}", base.display(), e))?;
    let target = if allow_nonexistent && !path.exists() {
        match path.parent() {
            Some(parent) if parent.as_os_str().is_empty() => {
                return Err(format!("path '{}' has no parent directory", path.display()));
            }
            Some(parent) => parent
                .canonicalize()
                .map_err(|e| format!("failed to resolve parent '{}': {}", parent.display(), e))?,
            None => {
                return Err(format!("path '{}' has no parent directory", path.display()));
            }
        }
    } else {
        path.canonicalize()
            .map_err(|e| format!("failed to resolve path '{}': {}", path.display(), e))?
    };
    if !target.starts_with(&canonical_base) {
        return Err(format!(
            "path '{}' is outside the allowed directory '{}'",
            path.display(),
            base.display()
        ));
    }
    Ok(path.to_path_buf())
}

/// Per-process / per-window auto-switcher.
///
/// `AutoSwitcher` is meant to be shared behind an `Arc`. It starts a single
/// background task that polls the foreground window once per second and asks the
/// parent [`ProfileManager`] to activate the matching profile.
pub struct AutoSwitcher {
    enabled: AtomicBool,
    running: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Default for AutoSwitcher {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            running: AtomicBool::new(false),
            task: Mutex::new(None),
        }
    }
}

impl AutoSwitcher {
    /// Create a new auto-switcher in the disabled state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether auto-switching is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Enable or disable auto-switching.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Start the polling loop. This is a no-op if the loop is already running.
    ///
    /// `manager` is the profile manager that will be queried each tick. The
    /// manager must already have its event sender configured if it needs to
    /// emit `ProfileChanged` IPC events.
    pub fn start(&self, manager: ProfileManager) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut guard = self.task.lock();
        if guard.is_some() {
            return;
        }

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if !manager.is_auto_switch_enabled() {
                    continue;
                }
                match detect_active_process() {
                    Ok((process_path, window_title)) => {
                        let current_id = manager.get_active_profile().map(|p| p.id);
                        if let Some(profile) =
                            manager.find_matching_profile(&process_path, &window_title)
                        {
                            if current_id.as_deref() != Some(&profile.id) {
                                let _ = manager.set_active_profile(Some(&profile.id));
                            }
                        }
                    }
                    Err(e) => log::warn!("auto-switch process detection failed: {}", e),
                }
            }
        });

        *guard = Some(handle);
    }
}

/// Thread-safe profile manager.
#[derive(Clone)]
pub struct ProfileManager {
    inner: Arc<RwLock<ProfileManagerState>>,
    event_tx: Arc<RwLock<Option<broadcast::Sender<IpcEvent>>>>,
    auto_switch: Arc<AutoSwitcher>,
    path: PathBuf,
    id_counter: Arc<AtomicU64>,
}

impl Default for ProfileManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProfileManager {
    /// Create a profile manager with the default on-disk path.
    pub fn new() -> Self {
        Self::with_path(default_profile_path())
    }

    /// Create a profile manager backed by a specific file path.
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        let pm = Self {
            inner: Arc::new(RwLock::new(ProfileManagerState::default())),
            event_tx: Arc::new(RwLock::new(None)),
            auto_switch: Arc::new(AutoSwitcher::new()),
            path: path.into(),
            id_counter: Arc::new(AtomicU64::new(1)),
        };
        let _ = pm.load();
        pm
    }

    /// Set the broadcast channel used to emit `ProfileChanged` events.
    pub fn set_event_sender(&self, tx: broadcast::Sender<IpcEvent>) {
        *self.event_tx.write() = Some(tx);
    }

    fn emit_profile_changed(&self) {
        let tx = self.event_tx.read().clone();
        if let Some(tx) = tx {
            let (id, name) = match self.get_active_profile() {
                Some(p) => (Some(p.id), Some(p.name)),
                None => (None, None),
            };
            let _ = tx.send(IpcEvent::ProfileChanged {
                profile_id: id,
                profile_name: name,
            });
        }
    }

    /// Load the profile store from disk, replacing the in-memory state.
    pub fn load(&self) -> Result<(), String> {
        if !self.path.exists() {
            return Ok(());
        }
        let data = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let loaded: ProfileManagerState = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        *self.inner.write() = loaded;
        Ok(())
    }

    /// Persist the current profile store to disk.
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_string_pretty(&*self.inner.read()).map_err(|e| e.to_string())?;
        fs::write(&self.path, data).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Serialize the current profile store to a JSON string.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&*self.inner.read()).map_err(|e| e.to_string())
    }

    /// Replace the in-memory store from a JSON string and persist it.
    pub fn from_json(&self, json: &str) -> Result<(), String> {
        let loaded: ProfileManagerState = serde_json::from_str(json).map_err(|e| e.to_string())?;
        *self.inner.write() = loaded;
        self.save()
    }

    /// List all profiles.
    pub fn list_profiles(&self) -> Vec<Profile> {
        self.inner.read().profiles.clone()
    }

    /// Get a single profile by id.
    pub fn get_profile(&self, id: &str) -> Option<Profile> {
        self.inner
            .read()
            .profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// Create a new profile.
    pub fn create_profile(
        &self,
        name: String,
        auto_rules: Option<Vec<AutoRule>>,
    ) -> Result<Profile, String> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("profile name is required".into());
        }
        let id = format!(
            "profile-{}-{}",
            crate::state::timestamp_now(),
            self.id_counter.fetch_add(1, Ordering::SeqCst)
        );
        let now = crate::state::timestamp_now();
        let profile = Profile {
            id,
            name,
            enabled: true,
            auto_rules: auto_rules.unwrap_or_default(),
            created_at: now,
            updated_at: now,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        self.inner.write().profiles.push(profile.clone());
        self.save()?;
        Ok(profile)
    }

    /// Update an existing profile in place.
    pub fn update_profile(&self, profile: Profile) -> Result<Profile, String> {
        if profile.name.trim().is_empty() {
            return Err("profile name is required".into());
        }
        let mut state = self.inner.write();
        // `idx` is derived and used while this write lock remains held.
        let idx = state
            .profiles
            .iter()
            .position(|p| p.id == profile.id)
            .ok_or_else(|| "profile not found".to_string())?;
        let mut updated = profile;
        updated.updated_at = crate::state::timestamp_now();
        state.profiles[idx] = updated.clone();
        drop(state);
        self.save()?;
        self.emit_profile_changed();
        Ok(updated)
    }

    /// Delete a profile by id.
    pub fn delete_profile(&self, id: &str) -> Result<bool, String> {
        let mut state = self.inner.write();
        let idx = state
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| "profile not found".to_string())?;
        state.profiles.remove(idx);
        if state.active_profile_id.as_deref() == Some(id) {
            state.active_profile_id = None;
        }
        if state.default_profile_id.as_deref() == Some(id) {
            state.default_profile_id = None;
        }
        drop(state);
        self.save()?;
        self.emit_profile_changed();
        Ok(true)
    }

    /// Set the active profile. `None` clears the active profile.
    pub fn set_active_profile(&self, id: Option<&str>) -> Result<Option<Profile>, String> {
        let mut state = self.inner.write();
        if let Some(id) = id {
            if !state.profiles.iter().any(|p| p.id == id) {
                return Err("profile not found".into());
            }
            state.active_profile_id = Some(id.to_string());
        } else {
            state.active_profile_id = None;
        }
        drop(state);
        self.save()?;
        self.emit_profile_changed();
        Ok(self.get_active_profile())
    }

    /// Get the currently active profile, if any.
    pub fn get_active_profile(&self) -> Option<Profile> {
        let state = self.inner.read();
        state
            .active_profile_id
            .as_ref()
            .and_then(|id| state.profiles.iter().find(|p| p.id == *id).cloned())
    }

    /// Set the default fallback profile id.
    pub fn set_default_profile_id(&self, id: Option<String>) -> Result<(), String> {
        if let Some(ref id) = id {
            if !self.inner.read().profiles.iter().any(|p| &p.id == id) {
                return Err("profile not found".into());
            }
        }
        self.inner.write().default_profile_id = id;
        self.save()?;
        Ok(())
    }

    /// Get the default fallback profile id, if set.
    pub fn get_default_profile_id(&self) -> Option<String> {
        self.inner.read().default_profile_id.clone()
    }

    /// Find the first enabled profile whose auto-rules match the current
    /// process path and window title. If no rule matches and a default profile
    /// is configured, the default is returned as a fallback.
    pub fn find_matching_profile(&self, process_path: &str, window_title: &str) -> Option<Profile> {
        let state = self.inner.read();
        find_matching_profile_state(&state, process_path, window_title)
    }

    /// Enable or disable the auto-switcher.
    pub fn set_auto_switch_enabled(&self, enabled: bool) {
        self.auto_switch.set_enabled(enabled);
        if enabled {
            self.start_auto_switcher();
        }
    }

    /// Whether the auto-switcher is enabled.
    pub fn is_auto_switch_enabled(&self) -> bool {
        self.auto_switch.is_enabled()
    }

    /// Start the background auto-switch polling loop.
    pub fn start_auto_switcher(&self) {
        // The loop needs an event sender to be useful; without it there is
        // no way to notify the frontend of changes.
        if self.event_tx.read().is_none() {
            return;
        }
        self.auto_switch.start(self.clone());
    }

    /// Export the profile store to `path`.
    pub fn export_to_path(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref();
        let base = self
            .path
            .parent()
            .ok_or_else(|| "profile store path has no parent directory".to_string())?;
        validate_path_within_base(path, base, true)?;
        let data = serde_json::to_string_pretty(&*self.inner.read()).map_err(|e| e.to_string())?;
        fs::write(path, data).map_err(|e| e.to_string())
    }

    /// Import the profile store from `path`, replacing the in-memory store.
    pub fn import_from_path(&self, path: impl AsRef<Path>) -> Result<Vec<Profile>, String> {
        let path = path.as_ref();
        let base = self
            .path
            .parent()
            .ok_or_else(|| "profile store path has no parent directory".to_string())?;
        validate_path_within_base(path, base, false)?;
        let data = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let loaded: ProfileManagerState = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        *self.inner.write() = loaded;
        self.save()?;
        self.emit_profile_changed();
        Ok(self.list_profiles())
    }
}

/// Find the first enabled profile whose auto-rules match the current
/// process path and window title against a plain `ProfileManagerState`.
pub fn find_matching_profile_state(
    state: &ProfileManagerState,
    process_path: &str,
    window_title: &str,
) -> Option<Profile> {
    for profile in &state.profiles {
        if !profile.enabled {
            continue;
        }
        for rule in &profile.auto_rules {
            if rule_matches(rule, process_path, window_title) {
                return Some(profile.clone());
            }
        }
    }
    // Fallback to the configured default profile.
    if let Some(default_id) = state.default_profile_id.as_ref() {
        if let Some(profile) = state
            .profiles
            .iter()
            .find(|p| p.id == *default_id && p.enabled)
        {
            return Some(profile.clone());
        }
    }
    None
}

fn rule_matches(rule: &AutoRule, process_path: &str, window_title: &str) -> bool {
    if !rule.enabled {
        return false;
    }
    let text = match rule.kind {
        AutoRuleKind::ProcessPath => process_path,
        AutoRuleKind::WindowTitle => window_title,
    };
    match rule.match_mode {
        MatchMode::Exact => text.eq_ignore_ascii_case(&rule.pattern),
        MatchMode::Contains => text.to_lowercase().contains(&rule.pattern.to_lowercase()),
        MatchMode::Regex => Regex::new(&rule.pattern)
            .map(|re| re.is_match(text))
            .unwrap_or(false),
    }
}

/// Detect the foreground process path and window title.
#[cfg(windows)]
pub fn detect_active_process() -> Result<(String, String), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return Err("no foreground window".into());
        }

        let mut title_buf = [0u16; 512];
        let title_len = GetWindowTextW(
            hwnd,
            title_buf.as_mut_ptr() as *mut _,
            title_buf.len() as i32,
        );
        let title = String::from_utf16_lossy(&title_buf[..title_len as usize]);

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return Err("could not get window process id".into());
        }

        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return Err("could not open foreground process".into());
        }

        let mut path_buf = [0u16; 1024];
        let mut size: u32 = path_buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, path_buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if ok == 0 {
            return Err("could not query process image name".into());
        }

        let path = String::from_utf16_lossy(&path_buf[..size as usize]).to_lowercase();
        Ok((path, title))
    }
}

#[cfg(not(windows))]
pub fn detect_active_process() -> Result<(String, String), String> {
    Err("process detection is only implemented on Windows".into())
}

// ---------------------------------------------------------------------------
// Tauri commands (free functions)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_profiles(pm: tauri::State<'_, ProfileManager>) -> Result<Vec<Profile>, String> {
    Ok(pm.list_profiles())
}

#[tauri::command]
pub fn create_profile(
    pm: tauri::State<'_, ProfileManager>,
    name: String,
    auto_rules: Option<Vec<AutoRule>>,
) -> Result<Profile, String> {
    pm.create_profile(name, auto_rules)
}

#[tauri::command]
pub fn update_profile(
    pm: tauri::State<'_, ProfileManager>,
    profile: Profile,
) -> Result<Profile, String> {
    pm.update_profile(profile)
}

#[tauri::command]
pub fn delete_profile(pm: tauri::State<'_, ProfileManager>, id: String) -> Result<bool, String> {
    pm.delete_profile(&id)
}

#[tauri::command]
pub fn set_active_profile(
    pm: tauri::State<'_, ProfileManager>,
    id: Option<String>,
) -> Result<Option<Profile>, String> {
    pm.set_active_profile(id.as_deref())
}

#[tauri::command]
pub fn get_active_profile(pm: tauri::State<'_, ProfileManager>) -> Result<Option<Profile>, String> {
    Ok(pm.get_active_profile())
}

#[tauri::command]
pub fn set_auto_switch_enabled(
    pm: tauri::State<'_, ProfileManager>,
    enabled: bool,
) -> Result<bool, String> {
    pm.set_auto_switch_enabled(enabled);
    Ok(pm.is_auto_switch_enabled())
}

#[tauri::command]
pub fn get_auto_switch_enabled(pm: tauri::State<'_, ProfileManager>) -> Result<bool, String> {
    Ok(pm.is_auto_switch_enabled())
}

#[tauri::command]
pub fn export_profiles(
    pm: tauri::State<'_, ProfileManager>,
    path: String,
) -> Result<String, String> {
    pm.export_to_path(&path)?;
    Ok(path)
}

#[tauri::command]
pub fn import_profiles(
    pm: tauri::State<'_, ProfileManager>,
    path: String,
) -> Result<Vec<Profile>, String> {
    pm.import_from_path(&path)
}
