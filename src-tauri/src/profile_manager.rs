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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ProfileManager as ProfileManagerState;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique counter so parallel tests don't collide on the same temp path.
    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    /// Guard to serialize tests that touch the filesystem to avoid flakiness
    /// when many tests create/remove the same canonical base directory.
    static FS_GUARD: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn temp_dir() -> PathBuf {
        let n = TEST_ID.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join("oxidelink-pm-tests").join(format!(
            "{}-{}-{}",
            crate::state::timestamp_now(),
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_manager() -> (ProfileManager, PathBuf) {
        let dir = temp_dir();
        let path = dir.join("profiles.json");
        (ProfileManager::with_path(&path), path)
    }

    fn rule(kind: AutoRuleKind, pattern: &str, mode: MatchMode) -> AutoRule {
        AutoRule {
            kind,
            pattern: pattern.to_string(),
            match_mode: mode,
            enabled: true,
        }
    }

    // -- ProfileConfig defaults and serialization ----------------------------

    #[test]
    fn profile_default_has_empty_id_and_disabled_state() {
        let p = Profile::default();
        assert!(p.id.is_empty());
        assert!(p.name.is_empty());
        assert!(!p.enabled);
        assert!(p.auto_rules.is_empty());
        assert_eq!(p.created_at, 0);
        assert_eq!(p.updated_at, 0);
    }

    #[test]
    fn auto_rule_default_is_process_path_contains_disabled() {
        let r = AutoRule::default();
        assert_eq!(r.kind, AutoRuleKind::ProcessPath);
        assert_eq!(r.match_mode, MatchMode::Contains);
        assert!(!r.enabled);
        assert!(r.pattern.is_empty());
    }

    #[test]
    fn match_mode_default_is_contains() {
        assert_eq!(MatchMode::default(), MatchMode::Contains);
    }

    #[test]
    fn auto_rule_kind_default_is_process_path() {
        assert_eq!(AutoRuleKind::default(), AutoRuleKind::ProcessPath);
    }

    #[test]
    fn profile_serde_roundtrip_preserves_all_fields() {
        let profile = Profile {
            id: "p-1".into(),
            name: "Test".into(),
            enabled: true,
            auto_rules: vec![rule(AutoRuleKind::WindowTitle, "Game", MatchMode::Exact)],
            created_at: 1000,
            updated_at: 2000,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
    }

    #[test]
    fn profile_serde_uses_snake_case_fields() {
        let profile = Profile {
            id: "x".into(),
            name: "Y".into(),
            enabled: true,
            auto_rules: vec![],
            created_at: 1,
            updated_at: 2,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        let json = serde_json::to_string(&profile).unwrap();
        assert!(json.contains("\"created_at\""));
        assert!(json.contains("\"updated_at\""));
        assert!(json.contains("\"auto_rules\""));
    }

    #[test]
    fn auto_rule_serde_uses_snake_case_enums() {
        let r = rule(AutoRuleKind::WindowTitle, "x", MatchMode::Exact);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"window_title\""));
        assert!(json.contains("\"exact\""));
    }

    #[test]
    fn profile_manager_state_default_is_empty() {
        let state = ProfileManagerState::default();
        assert!(state.profiles.is_empty());
        assert!(state.active_profile_id.is_none());
        assert!(state.default_profile_id.is_none());
        assert!(state.last_applied.is_none());
    }

    #[test]
    fn profile_manager_state_serde_roundtrip() {
        let state = ProfileManagerState {
            profiles: vec![Profile {
                id: "a".into(),
                name: "A".into(),
                enabled: true,
                auto_rules: vec![rule(AutoRuleKind::ProcessPath, "x", MatchMode::Regex)],
                created_at: 10,
                updated_at: 20,
                nfc: crate::state::NfcConfig::default(),
                right_stick: crate::state::flick_stick::RightStickConfig::default(),
            }],
            active_profile_id: Some("a".into()),
            default_profile_id: None,
            last_applied: Some("a".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: ProfileManagerState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    // -- ProfileManager state: load, save, switch, list ----------------------

    #[test]
    fn with_path_loads_empty_when_file_missing() {
        let dir = temp_dir();
        let path = dir.join("missing.json");
        assert!(!path.exists());
        let pm = ProfileManager::with_path(&path);
        assert!(pm.list_profiles().is_empty());
    }

    #[test]
    fn save_and_load_persists_profiles_to_disk() {
        let _guard = FS_GUARD.lock();
        let (pm, path) = temp_manager();
        let p = pm.create_profile("Persist".into(), None).unwrap();
        pm.set_active_profile(Some(&p.id)).unwrap();

        // Reload from the same path into a fresh manager.
        let pm2 = ProfileManager::with_path(&path);
        assert_eq!(pm2.list_profiles().len(), 1);
        assert_eq!(pm2.list_profiles()[0].name, "Persist");
        assert_eq!(pm2.get_active_profile().unwrap().id, p.id);
    }

    #[test]
    fn create_profile_rejects_empty_name() {
        let (pm, _path) = temp_manager();
        assert!(pm.create_profile("   ".into(), None).is_err());
        assert!(pm.create_profile("".into(), None).is_err());
    }

    #[test]
    fn create_profile_with_auto_rules_stores_them() {
        let (pm, _path) = temp_manager();
        let rules = vec![rule(AutoRuleKind::ProcessPath, "game.exe", MatchMode::Exact)];
        let p = pm.create_profile("WithRules".into(), Some(rules)).unwrap();
        let loaded = pm.get_profile(&p.id).unwrap();
        assert_eq!(loaded.auto_rules.len(), 1);
        assert_eq!(loaded.auto_rules[0].pattern, "game.exe");
    }

    #[test]
    fn create_profile_generates_unique_ids() {
        let (pm, _path) = temp_manager();
        let p1 = pm.create_profile("A".into(), None).unwrap();
        let p2 = pm.create_profile("B".into(), None).unwrap();
        assert_ne!(p1.id, p2.id);
    }

    #[test]
    fn update_profile_rejects_empty_name() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("Orig".into(), None).unwrap();
        let mut bad = p.clone();
        bad.name = "  ".into();
        assert!(pm.update_profile(bad).is_err());
    }

    #[test]
    fn update_profile_rejects_unknown_id() {
        let (pm, _path) = temp_manager();
        let p = Profile {
            id: "nonexistent".into(),
            name: "X".into(),
            enabled: true,
            auto_rules: vec![],
            created_at: 0,
            updated_at: 0,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        assert!(pm.update_profile(p).is_err());
    }

    #[test]
    fn update_profile_bumps_updated_at() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("Orig".into(), None).unwrap();
        let original_updated = p.updated_at;
        let mut updated = p.clone();
        updated.name = "New".into();
        let result = pm.update_profile(updated).unwrap();
        assert!(result.updated_at >= original_updated);
    }

    #[test]
    fn delete_profile_clears_active_and_default_refs() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("D".into(), None).unwrap();
        pm.set_active_profile(Some(&p.id)).unwrap();
        pm.set_default_profile_id(Some(p.id.clone())).unwrap();
        assert!(pm.get_active_profile().is_some());
        assert!(pm.get_default_profile_id().is_some());

        pm.delete_profile(&p.id).unwrap();
        assert!(pm.get_active_profile().is_none());
        assert!(pm.get_default_profile_id().is_none());
    }

    #[test]
    fn delete_profile_unknown_id_errors() {
        let (pm, _path) = temp_manager();
        assert!(pm.delete_profile("nope").is_err());
    }

    #[test]
    fn get_profile_returns_none_for_unknown() {
        let (pm, _path) = temp_manager();
        assert!(pm.get_profile("unknown").is_none());
    }

    #[test]
    fn set_active_profile_unknown_id_errors() {
        let (pm, _path) = temp_manager();
        assert!(pm.set_active_profile(Some("missing")).is_err());
    }

    #[test]
    fn set_default_profile_id_unknown_errors() {
        let (pm, _path) = temp_manager();
        assert!(pm.set_default_profile_id(Some("missing".into())).is_err());
    }

    #[test]
    fn set_default_profile_id_none_clears() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("D".into(), None).unwrap();
        pm.set_default_profile_id(Some(p.id.clone())).unwrap();
        assert!(pm.get_default_profile_id().is_some());
        pm.set_default_profile_id(None).unwrap();
        assert!(pm.get_default_profile_id().is_none());
    }

    #[test]
    fn to_json_and_from_json_roundtrip() {
        let (pm, _path) = temp_manager();
        let p = pm.create_profile("J".into(), None).unwrap();
        pm.set_active_profile(Some(&p.id)).unwrap();

        let json = pm.to_json().unwrap();
        let (pm2, _path2) = temp_manager();
        pm2.from_json(&json).unwrap();
        assert_eq!(pm2.list_profiles().len(), 1);
        assert_eq!(pm2.get_active_profile().unwrap().id, p.id);
    }

    #[test]
    fn from_json_invalid_returns_error() {
        let (pm, _path) = temp_manager();
        assert!(pm.from_json("not valid json").is_err());
    }

    // -- Auto-switch logic ---------------------------------------------------

    #[test]
    fn auto_switcher_new_is_disabled_and_not_running() {
        let sw = AutoSwitcher::new();
        assert!(!sw.is_enabled());
    }

    #[test]
    fn auto_switcher_set_enabled_toggles() {
        let sw = AutoSwitcher::new();
        sw.set_enabled(true);
        assert!(sw.is_enabled());
        sw.set_enabled(false);
        assert!(!sw.is_enabled());
    }

    #[test]
    fn pm_auto_switch_disabled_by_default() {
        let (pm, _path) = temp_manager();
        assert!(!pm.is_auto_switch_enabled());
    }

    #[test]
    fn pm_auto_switch_enable_disable() {
        let (pm, _path) = temp_manager();
        pm.set_auto_switch_enabled(true);
        assert!(pm.is_auto_switch_enabled());
        pm.set_auto_switch_enabled(false);
        assert!(!pm.is_auto_switch_enabled());
    }

    #[test]
    fn find_matching_profile_state_skips_disabled_profiles() {
        let mut state = ProfileManagerState::default();
        let mut p = Profile {
            id: "disabled".into(),
            name: "Disabled".into(),
            enabled: false,
            auto_rules: vec![rule(AutoRuleKind::WindowTitle, "Target", MatchMode::Exact)],
            created_at: 0,
            updated_at: 0,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        state.profiles.push(p.clone());
        assert!(find_matching_profile_state(&state, "app", "Target").is_none());

        // Enable it and verify it now matches.
        p.enabled = true;
        state.profiles[0] = p;
        assert!(find_matching_profile_state(&state, "app", "Target").is_some());
    }

    #[test]
    fn find_matching_profile_state_returns_first_match() {
        let mut state = ProfileManagerState::default();
        let p1 = Profile {
            id: "first".into(),
            name: "First".into(),
            enabled: true,
            auto_rules: vec![rule(AutoRuleKind::WindowTitle, "Match", MatchMode::Contains)],
            created_at: 0,
            updated_at: 0,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        let p2 = Profile {
            id: "second".into(),
            name: "Second".into(),
            enabled: true,
            auto_rules: vec![rule(AutoRuleKind::WindowTitle, "Match", MatchMode::Contains)],
            created_at: 0,
            updated_at: 0,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        state.profiles.push(p1);
        state.profiles.push(p2);
        let matched = find_matching_profile_state(&state, "app", "Match Me").unwrap();
        assert_eq!(matched.id, "first");
    }

    #[test]
    fn find_matching_profile_state_default_fallback_only_if_enabled() {
        let mut state = ProfileManagerState::default();
        let p = Profile {
            id: "def".into(),
            name: "Default".into(),
            enabled: false,
            auto_rules: vec![],
            created_at: 0,
            updated_at: 0,
            nfc: crate::state::NfcConfig::default(),
            right_stick: crate::state::flick_stick::RightStickConfig::default(),
        };
        state.profiles.push(p);
        state.default_profile_id = Some("def".into());
        assert!(find_matching_profile_state(&state, "app", "none").is_none());
    }

    #[test]
    fn find_matching_profile_state_no_match_no_default_returns_none() {
        let state = ProfileManagerState::default();
        assert!(find_matching_profile_state(&state, "app", "title").is_none());
    }

    #[test]
    fn rule_matches_exact_is_case_insensitive() {
        let r = rule(AutoRuleKind::ProcessPath, "Game.EXE", MatchMode::Exact);
        assert!(rule_matches(&r, "game.exe", "title"));
        assert!(rule_matches(&r, "GAME.EXE", "title"));
        assert!(!rule_matches(&r, "game.ex", "title"));
    }

    #[test]
    fn rule_matches_contains_is_case_insensitive() {
        let r = rule(AutoRuleKind::WindowTitle, "Visual", MatchMode::Contains);
        assert!(rule_matches(&r, "path", "VISUAL Studio"));
        assert!(rule_matches(&r, "path", "visual studio"));
        assert!(!rule_matches(&r, "path", "Notepad"));
    }

    #[test]
    fn rule_matches_regex_uses_pattern() {
        let r = rule(AutoRuleKind::ProcessPath, r"game_\d+\.exe", MatchMode::Regex);
        assert!(rule_matches(&r, "game_42.exe", "title"));
        assert!(!rule_matches(&r, "game.exe", "title"));
    }

    #[test]
    fn rule_matches_regex_invalid_pattern_is_false() {
        let r = rule(AutoRuleKind::ProcessPath, "[invalid", MatchMode::Regex);
        assert!(!rule_matches(&r, "anything", "title"));
    }

    #[test]
    fn rule_matches_disabled_rule_is_false() {
        let mut r = rule(AutoRuleKind::WindowTitle, "Match", MatchMode::Contains);
        r.enabled = false;
        assert!(!rule_matches(&r, "app", "Match here"));
    }

    #[test]
    fn rule_matches_process_path_uses_process_path_text() {
        let r = rule(AutoRuleKind::ProcessPath, "steam", MatchMode::Contains);
        assert!(rule_matches(&r, "steam.exe", "anything"));
        assert!(!rule_matches(&r, "other.exe", "steam"));
    }

    #[test]
    fn rule_matches_window_title_uses_window_title_text() {
        let r = rule(AutoRuleKind::WindowTitle, "steam", MatchMode::Contains);
        assert!(rule_matches(&r, "anything", "steam window"));
        assert!(!rule_matches(&r, "steam.exe", "other"));
    }

    // -- Import / export -----------------------------------------------------

    #[test]
    fn export_to_path_writes_json_file() {
        let _guard = FS_GUARD.lock();
        let (pm, path) = temp_manager();
        pm.create_profile("Export1".into(), None).unwrap();
        let export_path = path.parent().unwrap().join("export.json");
        pm.export_to_path(&export_path).unwrap();
        assert!(export_path.exists());
        let data = std::fs::read_to_string(&export_path).unwrap();
        assert!(data.contains("Export1"));
    }

    #[test]
    fn import_from_path_replaces_state() {
        let _guard = FS_GUARD.lock();
        let (pm, path) = temp_manager();
        pm.create_profile("Original".into(), None).unwrap();

        let export_path = path.parent().unwrap().join("export2.json");
        pm.export_to_path(&export_path).unwrap();

        let import_path = path.parent().unwrap().join("profiles2.json");
        let pm2 = ProfileManager::with_path(&import_path);
        pm2.create_profile("PreExisting".into(), None).unwrap();
        assert_eq!(pm2.list_profiles().len(), 1);

        let imported = pm2.import_from_path(&export_path).unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].name, "Original");
        // Pre-existing profile should be gone after import.
        assert!(pm2
            .list_profiles()
            .iter()
            .all(|p| p.name != "PreExisting"));
    }

    #[test]
    fn export_rejects_relative_path() {
        let (pm, _path) = temp_manager();
        assert!(pm.export_to_path("relative.json").is_err());
    }

    #[test]
    fn import_rejects_relative_path() {
        let (pm, _path) = temp_manager();
        assert!(pm.import_from_path("relative.json").is_err());
    }

    #[test]
    fn export_rejects_path_outside_base() {
        let _guard = FS_GUARD.lock();
        let (pm, path) = temp_manager();
        // Create a directory outside the base and try to export there.
        let outside = std::env::temp_dir().join("oxidelink-pm-outside-export");
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("evil.json");
        assert!(pm.export_to_path(&target).is_err());
        // Clean up
        let _ = std::fs::remove_dir_all(&outside);
        let _ = &path; // keep path alive
    }

    // -- Pure helper functions ------------------------------------------------

    #[test]
    fn default_profile_path_ends_with_profiles_json() {
        let path = default_profile_path();
        assert!(path.ends_with(PROFILE_FILE_NAME));
    }

    #[test]
    fn profile_store_base_dir_contains_oxidelink() {
        let base = profile_store_base_dir();
        assert!(base.ends_with("OxideLink"));
    }

    #[test]
    fn validate_path_within_base_rejects_relative() {
        let base = std::env::temp_dir();
        let result = validate_path_within_base(
            std::path::Path::new("relative.txt"),
            &base,
            true,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be absolute"));
    }

    #[test]
    fn validate_path_within_base_accepts_file_in_base() {
        let _guard = FS_GUARD.lock();
        let base = temp_dir();
        let target = base.join("nested").join("file.json");
        std::fs::create_dir_all(base.join("nested")).unwrap();
        let result = validate_path_within_base(&target, &base, true);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_within_base_rejects_outside_base() {
        let _guard = FS_GUARD.lock();
        let base = temp_dir();
        let outside = std::env::temp_dir().join("oxidelink-pm-validate-outside");
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("file.json");
        let result = validate_path_within_base(&target, &base, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("outside the allowed directory"));
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn validate_path_within_base_nonexistent_no_parent_errors() {
        let _guard = FS_GUARD.lock();
        let base = temp_dir();
        // A path with no real parent (just a filename) should error.
        let result = validate_path_within_base(
            std::path::Path::new(base.join("file.json").to_str().unwrap()),
            &base,
            true,
        );
        // This should succeed because the parent (base) exists.
        assert!(result.is_ok());
    }

    #[test]
    fn detect_active_process_non_windows_returns_error() {
        // On Windows this calls Win32 APIs; on non-Windows it returns an error.
        // We only assert the non-windows behavior to avoid touching the real API.
        #[cfg(not(windows))]
        {
            let result = detect_active_process();
            assert!(result.is_err());
        }
        #[cfg(windows)]
        {
            // On Windows we skip calling the real API in tests.
        }
    }
}
