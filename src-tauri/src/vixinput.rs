//! Virtual XInput / DS4 gamepad writer via the ViGEmBus driver.
//!
//! This module implements a dual-mode virtual gamepad writer:
//!
//! - **Mode A (ViGEmBus FFI)**: Loads `ViGEmClient.dll` at runtime via
//!   `windows-sys` `LoadLibraryA` / `GetProcAddress` and uses the C ABI to
//!   create a virtual Xbox 360 or DualShock 4 gamepad and push `XInputState`
//!   updates to it.
//!
//! - **Mode B (Fallback / Display-only)**: If the DLL is not present or the
//!   driver connection fails, the module degrades gracefully. `is_connected`
//!   returns `false` and `update` is a no-op that returns `false`. The
//!   computed `XInputState` is still available to the rest of the app via the
//!   existing `get_xinput_hex` Tauri command.

use crate::state::VirtualControllerType;
use crate::xinput::{
    XInputState, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK, XINPUT_GAMEPAD_DPAD_DOWN,
    XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT, XINPUT_GAMEPAD_DPAD_UP,
    XINPUT_GAMEPAD_GUIDE, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
    XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
    XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y,
};
use log::{debug, info, warn};
use parking_lot::Mutex;
use std::ffi::CString;
use std::os::raw::c_void;
use windows_sys::Win32::Foundation::{BOOL, FARPROC, HMODULE, TRUE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExA, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_USER_DIRS,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerA, OpenServiceA, QueryServiceStatus, SC_MANAGER_CONNECT,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
};

// ---------------------------------------------------------------------------
// FFI type definitions
// ---------------------------------------------------------------------------

/// Opaque ViGEm client handle (`PVIGEM_CLIENT`).
type PvigemClient = *mut c_void;

/// Opaque ViGEm target handle (`PVIGEM_TARGET`).
type PvigemTarget = *mut c_void;

/// ViGEm error codes (`VIGEM_ERRORS`). `VIGEM_ERROR_NONE == 0`.
type VigemErrors = i32;

/// XUSB_REPORT — the C struct accepted by `vigem_target_x360_update`.
///
/// Layout maps 1:1 onto [`XInputState`].
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct XusbReport {
    pub w_buttons: u16,
    pub b_left_trigger: u8,
    pub b_right_trigger: u8,
    pub s_thumb_lx: i16,
    pub s_thumb_ly: i16,
    pub s_thumb_rx: i16,
    pub s_thumb_ry: i16,
}

impl From<&XInputState> for XusbReport {
    fn from(state: &XInputState) -> Self {
        XusbReport {
            w_buttons: state.buttons,
            b_left_trigger: state.left_trigger,
            b_right_trigger: state.right_trigger,
            s_thumb_lx: state.thumb_lx,
            s_thumb_ly: state.thumb_ly,
            s_thumb_rx: state.thumb_rx,
            s_thumb_ry: state.thumb_ry,
        }
    }
}

/// DS4_REPORT — the C struct accepted by `vigem_target_ds4_update`.
///
/// Layout is byte-compatible with the ViGEmClient `DS4_REPORT` definition.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Ds4Report {
    pub b_thumb_lx: u8,
    pub b_thumb_ly: u8,
    pub b_thumb_rx: u8,
    pub b_thumb_ry: u8,
    pub w_buttons: u16,
    pub b_special: u8,
    pub b_trigger_l: u8,
    pub b_trigger_r: u8,
}

/// Default DS4 report: centered sticks, no buttons, D-pad neutral.
impl Default for Ds4Report {
    fn default() -> Self {
        Ds4Report {
            b_thumb_lx: 0x80,
            b_thumb_ly: 0x80,
            b_thumb_rx: 0x80,
            b_thumb_ry: 0x80,
            w_buttons: DS4_DPAD_NONE,
            b_special: 0,
            b_trigger_l: 0,
            b_trigger_r: 0,
        }
    }
}

// DualShock 4 digital button constants.
pub const DS4_BUTTON_THUMB_RIGHT: u16 = 1 << 15;
pub const DS4_BUTTON_THUMB_LEFT: u16 = 1 << 14;
pub const DS4_BUTTON_OPTIONS: u16 = 1 << 13;
pub const DS4_BUTTON_SHARE: u16 = 1 << 12;
pub const DS4_BUTTON_TRIGGER_RIGHT: u16 = 1 << 11;
pub const DS4_BUTTON_TRIGGER_LEFT: u16 = 1 << 10;
pub const DS4_BUTTON_SHOULDER_RIGHT: u16 = 1 << 9;
pub const DS4_BUTTON_SHOULDER_LEFT: u16 = 1 << 8;
pub const DS4_BUTTON_TRIANGLE: u16 = 1 << 7;
pub const DS4_BUTTON_CIRCLE: u16 = 1 << 6;
pub const DS4_BUTTON_CROSS: u16 = 1 << 5;
pub const DS4_BUTTON_SQUARE: u16 = 1 << 4;

// DualShock 4 special buttons (`b_special`).
pub const DS4_SPECIAL_BUTTON_PS: u8 = 1 << 0;
pub const DS4_SPECIAL_BUTTON_TOUCHPAD: u8 = 1 << 1;

// DualShock 4 D-pad directions (lower nibble of `w_buttons`).
pub const DS4_DPAD_NONE: u16 = 0x8;
pub const DS4_DPAD_NORTH: u16 = 0x0;
pub const DS4_DPAD_NORTHEAST: u16 = 0x1;
pub const DS4_DPAD_EAST: u16 = 0x2;
pub const DS4_DPAD_SOUTHEAST: u16 = 0x3;
pub const DS4_DPAD_SOUTH: u16 = 0x4;
pub const DS4_DPAD_SOUTHWEST: u16 = 0x5;
pub const DS4_DPAD_WEST: u16 = 0x6;
pub const DS4_DPAD_NORTHWEST: u16 = 0x7;

/// Scale an XInput thumbstick axis (`i16`, -32768..32767) to a DS4 byte
/// centered at 0x80.
fn scale_stick_to_ds4(v: i16) -> u8 {
    (((v as i32 + 32768) * 255 + 32767) / 65535).clamp(0, 255) as u8
}

impl From<&XInputState> for Ds4Report {
    fn from(state: &XInputState) -> Self {
        let mut report = Ds4Report {
            b_thumb_lx: scale_stick_to_ds4(state.thumb_lx),
            b_thumb_ly: scale_stick_to_ds4(state.thumb_ly),
            b_thumb_rx: scale_stick_to_ds4(state.thumb_rx),
            b_thumb_ry: scale_stick_to_ds4(state.thumb_ry),
            b_trigger_l: state.left_trigger,
            b_trigger_r: state.right_trigger,
            ..Default::default()
        };

        let mut buttons: u16 = 0;

        if state.buttons & XINPUT_GAMEPAD_A != 0 {
            buttons |= DS4_BUTTON_CROSS;
        }
        if state.buttons & XINPUT_GAMEPAD_B != 0 {
            buttons |= DS4_BUTTON_CIRCLE;
        }
        if state.buttons & XINPUT_GAMEPAD_X != 0 {
            buttons |= DS4_BUTTON_SQUARE;
        }
        if state.buttons & XINPUT_GAMEPAD_Y != 0 {
            buttons |= DS4_BUTTON_TRIANGLE;
        }
        if state.buttons & XINPUT_GAMEPAD_LEFT_SHOULDER != 0 {
            buttons |= DS4_BUTTON_SHOULDER_LEFT;
        }
        if state.buttons & XINPUT_GAMEPAD_RIGHT_SHOULDER != 0 {
            buttons |= DS4_BUTTON_SHOULDER_RIGHT;
        }
        if state.buttons & XINPUT_GAMEPAD_LEFT_THUMB != 0 {
            buttons |= DS4_BUTTON_THUMB_LEFT;
        }
        if state.buttons & XINPUT_GAMEPAD_RIGHT_THUMB != 0 {
            buttons |= DS4_BUTTON_THUMB_RIGHT;
        }
        if state.buttons & XINPUT_GAMEPAD_BACK != 0 {
            buttons |= DS4_BUTTON_SHARE;
        }
        if state.buttons & XINPUT_GAMEPAD_START != 0 {
            buttons |= DS4_BUTTON_OPTIONS;
        }
        if state.buttons & XINPUT_GAMEPAD_GUIDE != 0 {
            report.b_special |= DS4_SPECIAL_BUTTON_PS;
        }

        if state.left_trigger > 0 {
            buttons |= DS4_BUTTON_TRIGGER_LEFT;
        }
        if state.right_trigger > 0 {
            buttons |= DS4_BUTTON_TRIGGER_RIGHT;
        }

        // D-pad: translate the four cardinal bits into the DS4 HAT value.
        let up = state.buttons & XINPUT_GAMEPAD_DPAD_UP != 0;
        let down = state.buttons & XINPUT_GAMEPAD_DPAD_DOWN != 0;
        let left = state.buttons & XINPUT_GAMEPAD_DPAD_LEFT != 0;
        let right = state.buttons & XINPUT_GAMEPAD_DPAD_RIGHT != 0;

        let dpad = if up && right {
            DS4_DPAD_NORTHEAST
        } else if right && down {
            DS4_DPAD_SOUTHEAST
        } else if down && left {
            DS4_DPAD_SOUTHWEST
        } else if left && up {
            DS4_DPAD_NORTHWEST
        } else if up {
            DS4_DPAD_NORTH
        } else if right {
            DS4_DPAD_EAST
        } else if down {
            DS4_DPAD_SOUTH
        } else if left {
            DS4_DPAD_WEST
        } else {
            DS4_DPAD_NONE
        };

        report.w_buttons = (buttons & !0xF) | (dpad & 0xF);
        report
    }
}

// ---------------------------------------------------------------------------
// FFI function pointer types
// ---------------------------------------------------------------------------

#[allow(non_camel_case_types)]
type VIGEM_ALLOC = unsafe extern "C" fn() -> PvigemClient;
#[allow(non_camel_case_types)]
type VIGEM_FREE = unsafe extern "C" fn(client: PvigemClient);
#[allow(non_camel_case_types)]
type VIGEM_CONNECT = unsafe extern "C" fn(client: PvigemClient) -> VigemErrors;
#[allow(non_camel_case_types)]
type VIGEM_DISCONNECT = unsafe extern "C" fn(client: PvigemClient) -> VigemErrors;

#[allow(non_camel_case_types)]
type VIGEM_TARGET_X360_ALLOC = unsafe extern "C" fn() -> PvigemTarget;
#[allow(non_camel_case_types)]
type VIGEM_TARGET_X360_FREE = unsafe extern "C" fn(target: PvigemTarget);
#[allow(non_camel_case_types)]
type VIGEM_TARGET_X360_UPDATE = unsafe extern "C" fn(
    client: PvigemClient,
    target: PvigemTarget,
    report: XusbReport,
) -> VigemErrors;

#[allow(non_camel_case_types)]
type VIGEM_TARGET_DS4_ALLOC = unsafe extern "C" fn() -> PvigemTarget;
#[allow(non_camel_case_types)]
type VIGEM_TARGET_DS4_FREE = unsafe extern "C" fn(target: PvigemTarget);
#[allow(non_camel_case_types)]
type VIGEM_TARGET_DS4_REGISTER =
    unsafe extern "C" fn(client: PvigemClient, target: PvigemTarget) -> VigemErrors;
#[allow(non_camel_case_types)]
type VIGEM_TARGET_DS4_UPDATE = unsafe extern "C" fn(
    client: PvigemClient,
    target: PvigemTarget,
    report: Ds4Report,
) -> VigemErrors;
#[allow(non_camel_case_types)]
type VIGEM_TARGET_DS4_UNREGISTER =
    unsafe extern "C" fn(client: PvigemClient, target: PvigemTarget) -> VigemErrors;

#[allow(non_camel_case_types)]
type VIGEM_TARGET_ADD =
    unsafe extern "C" fn(client: PvigemClient, target: PvigemTarget) -> VigemErrors;
#[allow(non_camel_case_types)]
type VIGEM_TARGET_REMOVE =
    unsafe extern "C" fn(client: PvigemClient, target: PvigemTarget) -> VigemErrors;
#[allow(non_camel_case_types)]
type VIGEM_TARGET_SET_VID = unsafe extern "C" fn(target: PvigemTarget, vid: u16);
#[allow(non_camel_case_types)]
type VIGEM_TARGET_SET_PID = unsafe extern "C" fn(target: PvigemTarget, pid: u16);

// ---------------------------------------------------------------------------
// Bundled function pointers
// ---------------------------------------------------------------------------

/// All resolved ViGEmClient.dll entry points.
///
/// `None` fields indicate the function was not found in the DLL.
#[derive(Default)]
struct VigemApi {
    alloc: Option<VIGEM_ALLOC>,
    free: Option<VIGEM_FREE>,
    connect: Option<VIGEM_CONNECT>,
    disconnect: Option<VIGEM_DISCONNECT>,
    target_x360_alloc: Option<VIGEM_TARGET_X360_ALLOC>,
    target_x360_free: Option<VIGEM_TARGET_X360_FREE>,
    target_add: Option<VIGEM_TARGET_ADD>,
    target_remove: Option<VIGEM_TARGET_REMOVE>,
    target_x360_update: Option<VIGEM_TARGET_X360_UPDATE>,
    target_set_vid: Option<VIGEM_TARGET_SET_VID>,
    target_set_pid: Option<VIGEM_TARGET_SET_PID>,
    // DualShock 4 entry points
    target_ds4_alloc: Option<VIGEM_TARGET_DS4_ALLOC>,
    target_ds4_free: Option<VIGEM_TARGET_DS4_FREE>,
    target_ds4_register: Option<VIGEM_TARGET_DS4_REGISTER>,
    target_ds4_update: Option<VIGEM_TARGET_DS4_UPDATE>,
    target_ds4_unregister: Option<VIGEM_TARGET_DS4_UNREGISTER>,
}

impl VigemApi {
    /// Required client + generic target management entry points.
    fn is_client_complete(&self) -> bool {
        self.alloc.is_some()
            && self.free.is_some()
            && self.connect.is_some()
            && self.disconnect.is_some()
            && self.target_add.is_some()
            && self.target_remove.is_some()
            && self.target_set_vid.is_some()
            && self.target_set_pid.is_some()
    }

    fn is_x360_complete(&self) -> bool {
        self.is_client_complete()
            && self.target_x360_alloc.is_some()
            && self.target_x360_free.is_some()
            && self.target_x360_update.is_some()
    }

    fn is_ds4_complete(&self) -> bool {
        self.is_client_complete()
            && self.target_ds4_alloc.is_some()
            && self.target_ds4_free.is_some()
            && self.target_ds4_update.is_some()
    }

    fn is_target_complete(&self, kind: VirtualControllerType) -> bool {
        match kind {
            VirtualControllerType::Xbox360 => self.is_x360_complete(),
            VirtualControllerType::DualShock4 => self.is_ds4_complete(),
        }
    }

    fn add_fn_for_kind(&self, kind: VirtualControllerType) -> Option<VIGEM_TARGET_ADD> {
        match kind {
            VirtualControllerType::DualShock4 => self.target_ds4_register.or(self.target_add),
            VirtualControllerType::Xbox360 => self.target_add,
        }
    }

    fn remove_fn_for_kind(&self, kind: VirtualControllerType) -> Option<VIGEM_TARGET_REMOVE> {
        match kind {
            VirtualControllerType::DualShock4 => self.target_ds4_unregister.or(self.target_remove),
            VirtualControllerType::Xbox360 => self.target_remove,
        }
    }
}

// ---------------------------------------------------------------------------
// VirtualXInput
// ---------------------------------------------------------------------------

/// Mutable ViGEm FFI state. Kept behind a single mutex so the raw handles are
/// never accessed concurrently from multiple threads.
struct VirtualXInputInner {
    /// Loaded module handle, kept alive so the function pointers remain valid.
    module: Option<HMODULE>,
    /// Resolved FFI entry points.
    api: VigemApi,
    /// Allocated ViGEm client handle.
    client: PvigemClient,
    /// Allocated Xbox 360 or DS4 target handle.
    target: PvigemTarget,
    /// `true` once the target has been added to the bus.
    connected: bool,
    /// Currently emulated controller type.
    kind: VirtualControllerType,
}

impl Default for VirtualXInputInner {
    fn default() -> Self {
        Self {
            module: None,
            api: VigemApi::default(),
            client: std::ptr::null_mut(),
            target: std::ptr::null_mut(),
            connected: false,
            kind: VirtualControllerType::default(),
        }
    }
}

/// Dual-mode virtual gamepad writer supporting Xbox 360 or DualShock 4 output.
///
/// On construction the struct attempts to load `ViGEmClient.dll`, resolve the
/// full C ABI, allocate a client + selected target, and connect to the
/// ViGEmBus driver. If any step fails the struct silently falls back to
/// display-only mode.
pub struct VirtualXInput {
    inner: Mutex<VirtualXInputInner>,
}

// SAFETY: VirtualXInput owns raw FFI handles, but all access is serialized by
// the internal `inner` mutex, so the handles are never used concurrently or
// moved while in use. This justifies the manual `Send + Sync` impls.
unsafe impl Send for VirtualXInput {}
unsafe impl Sync for VirtualXInput {}

impl VirtualXInput {
    /// Attempt to load `ViGEmClient.dll` and connect a virtual gamepad of `kind`.
    ///
    /// Never panics — on any failure the struct is constructed in fallback
    /// (display-only) mode.
    pub fn new(kind: VirtualControllerType) -> Self {
        Self::new_impl(kind)
    }

    /// Creates a display-only fallback without attempting to load the DLL.
    pub fn new_fallback() -> Self {
        VirtualXInput {
            inner: Mutex::new(VirtualXInputInner::default()),
        }
    }

    fn new_impl(kind: VirtualControllerType) -> Self {
        let vix = VirtualXInput {
            inner: Mutex::new(VirtualXInputInner {
                kind,
                ..VirtualXInputInner::default()
            }),
        };

        {
            let mut inner = vix.inner.lock();
            if !Self::ensure_client_loaded(&mut inner) {
                drop(inner);
                return vix;
            }

            if let Err(err) = Self::connect_target(&mut inner, kind) {
                warn!("Failed to connect initial virtual {kind:?} target (error {err}).");
            }
        }

        vix
    }

    fn ensure_client_loaded(inner: &mut VirtualXInputInner) -> bool {
        // Already connected/ready.
        if !inner.client.is_null() {
            return true;
        }

        // 1. Load the DLL. Use LoadLibraryExA with restricted search flags
        // to prevent SafeDllSearchMode from hanging on network paths.
        if inner.module.is_none() {
            let dll_name = CString::new("ViGEmClient.dll").expect("valid CString");
            let flags = LOAD_LIBRARY_SEARCH_APPLICATION_DIR
                | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS
                | LOAD_LIBRARY_SEARCH_USER_DIRS;
            let module = unsafe {
                LoadLibraryExA(dll_name.as_ptr() as *const u8, std::ptr::null_mut(), flags)
            };
            if module.is_null() {
                warn!(
                    "ViGEmClient.dll not found — virtual controller disabled (display-only mode). \
                 Install ViGEmBus to enable virtual gamepad output."
                );
                return false;
            }
            inner.module = Some(module);
        }

        // 2. Resolve every required entry point.
        if let Some(module) = inner.module {
            inner.api = resolve_api(module);
        }
        if !inner.api.is_target_complete(inner.kind) {
            warn!(
                "ViGEmClient.dll loaded but one or more required entry points are missing \
             for {:?} — virtual controller disabled (display-only mode).",
                inner.kind
            );
            return false;
        }

        // 3. Allocate + connect the client.
        let alloc = inner.api.alloc.ok_or_else(|| {
            warn!("VIGEM_ALLOC entry point missing — virtual controller disabled.");
        });
        let alloc = match alloc {
            Ok(f) => f,
            Err(_) => return false,
        };
        let client = unsafe { alloc() };
        if client.is_null() {
            warn!("VIGEM_ALLOC returned null — virtual controller disabled.");
            return false;
        }

        let connect = inner.api.connect.ok_or_else(|| {
            warn!("vigem_connect entry point missing — virtual controller disabled.");
        });
        let connect = match connect {
            Ok(f) => f,
            Err(_) => {
                // We cannot call free if it is also missing, but at least try
                // to release the client if free is available.
                if let Some(free) = inner.api.free {
                    unsafe { free(client) };
                }
                return false;
            }
        };
        let err = unsafe { connect(client) };
        if err != 0 {
            warn!("vigem_connect failed (error {err}) — virtual controller disabled. Is the ViGEmBus driver installed?");
            if let Some(free) = inner.api.free {
                unsafe { free(client) };
            }
            return false;
        }

        inner.client = client;
        true
    }

    fn connect_target(
        inner: &mut VirtualXInputInner,
        kind: VirtualControllerType,
    ) -> Result<(), VigemErrors> {
        let alloc = match kind {
            VirtualControllerType::Xbox360 => inner.api.target_x360_alloc,
            VirtualControllerType::DualShock4 => inner.api.target_ds4_alloc,
        };
        let free = match kind {
            VirtualControllerType::Xbox360 => inner.api.target_x360_free,
            VirtualControllerType::DualShock4 => inner.api.target_ds4_free,
        };
        let add = inner.api.add_fn_for_kind(kind);

        if alloc.is_none() || free.is_none() || add.is_none() {
            warn!("Required ViGEm entry points for {:?} are missing.", kind);
            return Err(0);
        }

        if inner.client.is_null() {
            warn!("Cannot connect target: ViGEm client is null.");
            return Err(0);
        }

        let alloc_fn = alloc.ok_or_else(|| {
            warn!("Target allocation entry point for {:?} is missing.", kind);
            0
        })?;
        let target = unsafe { alloc_fn() };
        if target.is_null() {
            warn!("Target allocation for {:?} returned null.", kind);
            return Err(0);
        }

        // Use official VID/PID values so games recognise the pad.
        let (vid, pid) = match kind {
            VirtualControllerType::Xbox360 => (0x045E, 0x028E),
            VirtualControllerType::DualShock4 => (0x054C, 0x05C4),
        };
        if let Some(set_vid) = inner.api.target_set_vid {
            unsafe { set_vid(target, vid) };
        }
        if let Some(set_pid) = inner.api.target_set_pid {
            unsafe { set_pid(target, pid) };
        }

        let add_fn = add.ok_or_else(|| {
            warn!("Target add entry point for {:?} is missing.", kind);
            0
        })?;
        let err = unsafe { add_fn(inner.client, target) };
        if err != 0 {
            warn!("vigem_target_add failed for {:?} (error {err}).", kind);
            if let Some(free_fn) = free {
                unsafe { free_fn(target) };
            }
            return Err(err);
        }

        inner.target = target;
        inner.kind = kind;
        inner.connected = true;
        info!("Virtual {:?} gamepad registered via ViGEmBus.", kind);
        Ok(())
    }

    fn remove_and_free_current(inner: &mut VirtualXInputInner) {
        if inner.connected {
            if let Some(remove) = inner.api.remove_fn_for_kind(inner.kind) {
                if !inner.client.is_null() && !inner.target.is_null() {
                    let err = unsafe { remove(inner.client, inner.target) };
                    if err != 0 {
                        warn!("vigem_target_remove failed (error {err}).");
                    }
                }
            }
            inner.connected = false;
        }

        match inner.kind {
            VirtualControllerType::Xbox360 => {
                if let Some(free) = inner.api.target_x360_free {
                    if !inner.target.is_null() {
                        unsafe { free(inner.target) };
                    }
                }
            }
            VirtualControllerType::DualShock4 => {
                if let Some(free) = inner.api.target_ds4_free {
                    if !inner.target.is_null() {
                        unsafe { free(inner.target) };
                    }
                }
            }
        }

        inner.target = std::ptr::null_mut();
    }

    fn disconnect_internal(inner: &mut VirtualXInputInner) {
        Self::remove_and_free_current(inner);

        if !inner.client.is_null() {
            if let Some(disconnect) = inner.api.disconnect {
                let err = unsafe { disconnect(inner.client) };
                if err != 0 {
                    warn!("vigem_disconnect failed (error {err}).");
                }
            }
            if let Some(free) = inner.api.free {
                unsafe { free(inner.client) };
            }
        }

        inner.client = std::ptr::null_mut();
        inner.connected = false;
        inner.module.take();
    }

    /// Returns the currently emulated controller type.
    pub fn kind(&self) -> VirtualControllerType {
        self.inner.lock().kind
    }

    /// `true` if a virtual pad is connected to the ViGEmBus driver.
    pub fn is_connected(&self) -> bool {
        self.inner.lock().connected
    }

    /// `true` if ViGEmClient.dll was successfully loaded.
    pub fn is_dll_loaded(&self) -> bool {
        self.inner.lock().module.is_some()
    }

    /// Push an [`XInputState`] snapshot to the virtual gamepad.
    ///
    /// Returns `true` on success, `false` in fallback mode or on FFI failure.
    pub fn update(&self, state: &XInputState) -> bool {
        let inner = self.inner.lock();

        if !inner.connected {
            debug!("update called in fallback mode — skipping ViGEm push.");
            return false;
        }

        match inner.kind {
            VirtualControllerType::Xbox360 => {
                let report = XusbReport::from(state);
                let update = match inner.api.target_x360_update {
                    Some(f) => f,
                    None => return false,
                };

                let err = unsafe { update(inner.client, inner.target, report) };
                if err != 0 {
                    warn!("vigem_target_x360_update failed (error {err}).");
                    return false;
                }
            }
            VirtualControllerType::DualShock4 => {
                let report = Ds4Report::from(state);
                let update = match inner.api.target_ds4_update {
                    Some(f) => f,
                    None => return false,
                };

                let err = unsafe { update(inner.client, inner.target, report) };
                if err != 0 {
                    warn!("vigem_target_ds4_update failed (error {err}).");
                    return false;
                }
            }
        }

        true
    }

    /// Change the active emulated controller type, disconnecting any existing
    /// target and (re)creating the requested one.
    pub fn set_kind(&mut self, kind: VirtualControllerType) {
        let mut inner = self.inner.lock();

        if inner.kind == kind && inner.connected {
            return;
        }

        Self::remove_and_free_current(&mut inner);
        inner.kind = kind;

        if !Self::ensure_client_loaded(&mut inner) {
            warn!(
                "Cannot set virtual controller type to {:?}: ViGEmClient.dll not available.",
                kind
            );
            return;
        }

        if let Err(err) = Self::connect_target(&mut inner, kind) {
            warn!("Failed to connect virtual {kind:?} target (error {err}).");
        }
    }

    /// Tear down the virtual pad: remove the target, disconnect, and free all
    /// handles. Safe to call on a fallback-mode instance (no-op).
    pub fn disconnect(self) {
        Self::disconnect_internal(&mut self.inner.lock());
    }
}

impl Default for VirtualXInput {
    fn default() -> Self {
        Self::new(VirtualControllerType::default())
    }
}

impl Drop for VirtualXInput {
    /// Defensive drop: if [`disconnect`] was not called explicitly, still
    /// release the ViGEm resources. Explicit `disconnect` is preferred because
    /// it consumes `self` and prevents double-free; this is a safety net.
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        if !inner.connected && inner.target.is_null() && inner.client.is_null() {
            inner.module.take();
            return;
        }
        Self::disconnect_internal(&mut inner);
    }
}

// ---------------------------------------------------------------------------
// DLL symbol resolution helpers
// ---------------------------------------------------------------------------

/// Resolve every required ViGEmClient.dll entry point from `module`.
fn resolve_api(module: HMODULE) -> VigemApi {
    let mut api = VigemApi {
        alloc: get_proc::<VIGEM_ALLOC>(module, "VIGEM_ALLOC"),
        free: get_proc::<VIGEM_FREE>(module, "VIGEM_FREE"),
        connect: get_proc::<VIGEM_CONNECT>(module, "vigem_connect"),
        disconnect: get_proc::<VIGEM_DISCONNECT>(module, "vigem_disconnect"),
        ..Default::default()
    };

    api.target_x360_alloc = get_proc::<VIGEM_TARGET_X360_ALLOC>(module, "vigem_target_x360_alloc");
    api.target_x360_free = get_proc::<VIGEM_TARGET_X360_FREE>(module, "vigem_target_x360_free");
    api.target_x360_update =
        get_proc::<VIGEM_TARGET_X360_UPDATE>(module, "vigem_target_x360_update");

    api.target_ds4_alloc = get_proc::<VIGEM_TARGET_DS4_ALLOC>(module, "vigem_target_ds4_alloc");
    api.target_ds4_free = get_proc::<VIGEM_TARGET_DS4_FREE>(module, "vigem_target_ds4_free");
    api.target_ds4_register =
        get_proc::<VIGEM_TARGET_DS4_REGISTER>(module, "vigem_target_ds4_register");
    api.target_ds4_update = get_proc::<VIGEM_TARGET_DS4_UPDATE>(module, "vigem_target_ds4_update");
    api.target_ds4_unregister =
        get_proc::<VIGEM_TARGET_DS4_UNREGISTER>(module, "vigem_target_ds4_unregister");

    api.target_add = get_proc::<VIGEM_TARGET_ADD>(module, "vigem_target_add");
    api.target_remove = get_proc::<VIGEM_TARGET_REMOVE>(module, "vigem_target_remove");
    api.target_set_vid = get_proc::<VIGEM_TARGET_SET_VID>(module, "vigem_target_set_vid");
    api.target_set_pid = get_proc::<VIGEM_TARGET_SET_PID>(module, "vigem_target_set_pid");

    api
}

/// Look up a named export and transmute it to the requested function-pointer type.
///
/// Returns `None` if the export is missing or the pointer is null.
fn get_proc<T>(module: HMODULE, name: &str) -> Option<T> {
    let cname = CString::new(name).ok()?;
    let proc = unsafe { GetProcAddress(module, cname.as_ptr() as *const u8) };
    if proc.is_none() {
        debug!("ViGEm export not found: {name}");
        return None;
    }
    // SAFETY: `T` is always a `Option<unsafe extern "C" fn(...)>`-compatible
    // function pointer type whose layout matches a raw pointer. The transmute
    // is sound because `FARPROC` is itself a function pointer (`Option<unsafe
    // extern "system" fn() -> isize>`).
    Some(unsafe { std::mem::transmute_copy::<FARPROC, T>(&proc) })
}

// ---------------------------------------------------------------------------
// ViGEmBus driver detection
// ---------------------------------------------------------------------------

/// Aggregated ViGEmBus status reported to the frontend.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VigemBusStatus {
    /// `true` if the ViGEmBus kernel driver service is installed.
    pub driver_installed: bool,
    /// `true` if the ViGEmBus service is currently running.
    pub driver_running: bool,
    /// `true` if `ViGEmClient.dll` is findable on the search path.
    pub dll_found: bool,
    /// Resolved path to the DLL, if found.
    pub dll_path: Option<String>,
    /// `true` if a virtual gamepad is currently connected.
    pub virtual_pad_connected: bool,
    /// `true` if a virtual Xbox 360 target is currently connected.
    pub xbox_target_connected: bool,
    /// `true` if a virtual DualShock 4 target is currently connected.
    pub ds4_target_connected: bool,
    /// Driver version string, if detectable.
    pub version: Option<String>,
}

/// Check if the ViGEmBus kernel driver is installed and running.
///
/// Queries the Windows Service Control Manager for the `ViGEmBus` service.
/// Returns `(installed, running, version)`. The version is currently `None`
/// because the SCM does not expose the driver file version directly.
pub fn detect_vigembus_driver() -> (bool, bool, Option<String>) {
    let scm_name = CString::new("ServicesActiveDatabase").unwrap_or_default();
    let service_name = CString::new("ViGEmBus").expect("valid CString");

    // Open the SCM with minimal access (connect only).
    let scm = unsafe {
        OpenSCManagerA(
            std::ptr::null(),
            scm_name.as_ptr() as *const u8,
            SC_MANAGER_CONNECT,
        )
    };
    if scm.is_null() {
        debug!("detect_vigembus_driver: OpenSCManagerA failed — assuming not installed.");
        return (false, false, None);
    }

    // Open the ViGEmBus service with query-status access.
    let service = unsafe {
        OpenServiceA(
            scm,
            service_name.as_ptr() as *const u8,
            SERVICE_QUERY_STATUS,
        )
    };
    if service.is_null() {
        // Service does not exist → not installed.
        unsafe { CloseServiceHandle(scm) };
        debug!("detect_vigembus_driver: ViGEmBus service not found.");
        return (false, false, None);
    }

    // Query the current service status.
    let mut status = SERVICE_STATUS {
        dwServiceType: 0,
        dwCurrentState: 0,
        dwControlsAccepted: 0,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    let ok: BOOL = unsafe { QueryServiceStatus(service, &mut status) };
    unsafe { CloseServiceHandle(service) };
    unsafe { CloseServiceHandle(scm) };

    if ok != TRUE {
        debug!("detect_vigembus_driver: QueryServiceStatus failed.");
        // Service exists but we could not query it — treat as installed.
        return (true, false, None);
    }

    let running = status.dwCurrentState == SERVICE_RUNNING;
    (true, running, None)
}

/// Build a full [`VigemBusStatus`] snapshot for the frontend.
pub fn detect_vigembus_driver_status() -> VigemBusStatus {
    let (installed, running, version) = detect_vigembus_driver();

    // Check if ViGEmClient.dll is findable on the PATH or in the current dir.
    let dll_name = "ViGEmClient.dll";
    let mut dll_found = std::path::Path::new(dll_name).exists();
    let mut dll_path: Option<String> = None;

    if !dll_found {
        if let Ok(paths) = std::env::var("PATH") {
            for dir in paths.split(';') {
                let candidate = std::path::Path::new(dir).join(dll_name);
                if candidate.exists() {
                    dll_found = true;
                    dll_path = candidate.to_str().map(|s| s.to_string());
                    break;
                }
            }
        }
    } else {
        dll_path = Some(dll_name.to_string());
    }

    VigemBusStatus {
        driver_installed: installed,
        driver_running: running,
        dll_found,
        dll_path,
        virtual_pad_connected: false,
        xbox_target_connected: false,
        ds4_target_connected: false,
        version,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xusb_report_from_xinput_state_maps_fields_1to1() {
        let state = XInputState {
            buttons: 0x1000 | 0x0001,
            left_trigger: 128,
            right_trigger: 255,
            thumb_lx: 1234,
            thumb_ly: -5678,
            thumb_rx: -1,
            thumb_ry: 32767,
        };
        let report = XusbReport::from(&state);
        assert_eq!(report.w_buttons, state.buttons);
        assert_eq!(report.b_left_trigger, state.left_trigger);
        assert_eq!(report.b_right_trigger, state.right_trigger);
        assert_eq!(report.s_thumb_lx, state.thumb_lx);
        assert_eq!(report.s_thumb_ly, state.thumb_ly);
        assert_eq!(report.s_thumb_rx, state.thumb_rx);
        assert_eq!(report.s_thumb_ry, state.thumb_ry);
    }

    #[test]
    fn fallback_mode_when_dll_missing() {
        // On a machine without ViGEmClient.dll this must not panic and must
        // report disconnected.
        let vix = VirtualXInput::new(VirtualControllerType::default());
        assert!(!vix.is_connected());
        let state = XInputState::default();
        assert!(!vix.update(&state));
    }

    #[test]
    fn vigem_api_is_complete_false_when_empty() {
        let api = VigemApi::default();
        assert!(!api.is_client_complete());
    }

    #[test]
    fn ds4_report_default_is_centered_and_neutral() {
        let r = Ds4Report::default();
        assert_eq!(r.b_thumb_lx, 0x80);
        assert_eq!(r.b_thumb_ly, 0x80);
        assert_eq!(r.b_thumb_rx, 0x80);
        assert_eq!(r.b_thumb_ry, 0x80);
        assert_eq!(r.w_buttons & 0xF, DS4_DPAD_NONE);
        assert_eq!(r.b_trigger_l, 0);
        assert_eq!(r.b_trigger_r, 0);
        assert_eq!(r.b_special, 0);
    }

    #[test]
    fn ds4_report_from_xinput_state_maps_buttons_triggers_and_axes() {
        let state = XInputState {
            buttons: XINPUT_GAMEPAD_A
                | XINPUT_GAMEPAD_B
                | XINPUT_GAMEPAD_X
                | XINPUT_GAMEPAD_Y
                | XINPUT_GAMEPAD_LEFT_SHOULDER
                | XINPUT_GAMEPAD_RIGHT_SHOULDER
                | XINPUT_GAMEPAD_LEFT_THUMB
                | XINPUT_GAMEPAD_RIGHT_THUMB
                | XINPUT_GAMEPAD_BACK
                | XINPUT_GAMEPAD_START
                | XINPUT_GAMEPAD_GUIDE,
            left_trigger: 200,
            right_trigger: 255,
            thumb_lx: -32768,
            thumb_ly: 0,
            thumb_rx: 32767,
            thumb_ry: 0,
        };
        let r = Ds4Report::from(&state);

        // Axes scaled from i16 to u8 centered at 0x80.
        assert_eq!(r.b_thumb_lx, 0x00);
        assert_eq!(r.b_thumb_ly, 0x80);
        assert_eq!(r.b_thumb_rx, 0xFF);
        assert_eq!(r.b_thumb_ry, 0x80);

        // Triggers copied and also reflected in button bits.
        assert_eq!(r.b_trigger_l, 200);
        assert_eq!(r.b_trigger_r, 255);
        assert!(r.w_buttons & DS4_BUTTON_TRIGGER_LEFT != 0);
        assert!(r.w_buttons & DS4_BUTTON_TRIGGER_RIGHT != 0);

        // Face / shoulder / thumb / menu buttons.
        assert!(r.w_buttons & DS4_BUTTON_CROSS != 0);
        assert!(r.w_buttons & DS4_BUTTON_CIRCLE != 0);
        assert!(r.w_buttons & DS4_BUTTON_SQUARE != 0);
        assert!(r.w_buttons & DS4_BUTTON_TRIANGLE != 0);
        assert!(r.w_buttons & DS4_BUTTON_SHOULDER_LEFT != 0);
        assert!(r.w_buttons & DS4_BUTTON_SHOULDER_RIGHT != 0);
        assert!(r.w_buttons & DS4_BUTTON_THUMB_LEFT != 0);
        assert!(r.w_buttons & DS4_BUTTON_THUMB_RIGHT != 0);
        assert!(r.w_buttons & DS4_BUTTON_SHARE != 0);
        assert!(r.w_buttons & DS4_BUTTON_OPTIONS != 0);
        assert!(r.b_special & DS4_SPECIAL_BUTTON_PS != 0);
    }

    #[test]
    fn ds4_report_dpad_mapping() {
        let mut state = XInputState::default();

        state.buttons = XINPUT_GAMEPAD_DPAD_UP;
        assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, DS4_DPAD_NORTH);

        state.buttons = XINPUT_GAMEPAD_DPAD_RIGHT;
        assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, DS4_DPAD_EAST);

        state.buttons = XINPUT_GAMEPAD_DPAD_DOWN;
        assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, DS4_DPAD_SOUTH);

        state.buttons = XINPUT_GAMEPAD_DPAD_LEFT;
        assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, DS4_DPAD_WEST);

        state.buttons = XINPUT_GAMEPAD_DPAD_UP | XINPUT_GAMEPAD_DPAD_RIGHT;
        assert_eq!(Ds4Report::from(&state).w_buttons & 0xF, DS4_DPAD_NORTHEAST);
    }

    #[test]
    fn virtual_xinput_kind_matches_constructor() {
        let vix = VirtualXInput::new(VirtualControllerType::DualShock4);
        // Even in fallback mode (no DLL) the requested kind is reported.
        assert_eq!(vix.kind(), VirtualControllerType::DualShock4);
    }

    #[test]
    fn virtual_xinput_set_kind_changes_target_type() {
        let mut vix = VirtualXInput::new(VirtualControllerType::Xbox360);
        assert_eq!(vix.kind(), VirtualControllerType::Xbox360);
        vix.set_kind(VirtualControllerType::DualShock4);
        assert_eq!(vix.kind(), VirtualControllerType::DualShock4);
    }
}
