//! HidHide integration for OxideLink.
//!
//! Communicates with the `\\.\HidHide` control device to hide the physical
//! Nintendo Switch Pro Controller from Windows games while whitelisting
//! OxideLink itself.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use serde::Serialize;
use tauri::State;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, DIGCF_PRESENT, HDEVINFO, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumePathNameW, QueryDosDeviceW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::state::AppCtx;

// -----------------------------------------------------------------------------
//  IOCTL contract (research deliverable)
// -----------------------------------------------------------------------------

pub const HIDHIDE_DEVICE_TYPE: u32 = 0x8000;
pub const HIDHIDE_ACCESS: u32 = 0; // FILE_ANY_ACCESS
pub const HIDHIDE_METHOD: u32 = 0; // METHOD_BUFFERED

pub const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    ((device_type) << 16) | ((access) << 14) | ((function) << 2) | (method)
}

pub const fn hidhide_ioctl(function: u32) -> u32 {
    ctl_code(
        HIDHIDE_DEVICE_TYPE,
        function,
        HIDHIDE_METHOD,
        HIDHIDE_ACCESS,
    )
}

pub const IOCTL_GET_WHITELIST: u32 = hidhide_ioctl(0x800);
pub const IOCTL_SET_WHITELIST: u32 = hidhide_ioctl(0x801);
pub const IOCTL_GET_BLACKLIST: u32 = hidhide_ioctl(0x802);
pub const IOCTL_SET_BLACKLIST: u32 = hidhide_ioctl(0x803);
pub const IOCTL_GET_ACTIVE: u32 = hidhide_ioctl(0x804);
pub const IOCTL_SET_ACTIVE: u32 = hidhide_ioctl(0x805);
pub const IOCTL_GET_WLINVERSE: u32 = hidhide_ioctl(0x806);
pub const IOCTL_SET_WLINVERSE: u32 = hidhide_ioctl(0x807);
pub const IOCTL_ADD_SESSION_BLACKLIST: u32 = hidhide_ioctl(0x808);
pub const IOCTL_CLR_SESSION_BLACKLIST: u32 = hidhide_ioctl(0x809);

const CONTROL_DEVICE_PATH: &str = r"\\.\HidHide";

const GUID_DEVCLASS_HIDCLASS: GUID = GUID {
    data1: 0x745a17a0,
    data2: 0x74d3,
    data3: 0x11d0,
    data4: [0xb6, 0xfe, 0x00, 0xa0, 0xc9, 0x0f, 0x57, 0xda],
};

// -----------------------------------------------------------------------------
//  Public status type
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct HidHideStatus {
    pub installed: bool,
    pub enabled: bool,
    pub hidden: bool,
    pub device_path: String,
    pub message: String,
}

// -----------------------------------------------------------------------------
//  String helpers
// -----------------------------------------------------------------------------

pub fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub fn encode_multi_sz_bytes(strings: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in strings {
        for c in s.encode_utf16() {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]); // null-terminate this string
    }
    out.extend_from_slice(&[0, 0]); // final null
    out
}

pub fn decode_multi_sz_bytes(bytes: &[u8]) -> Vec<String> {
    let mut strings = Vec::new();
    let mut current: Vec<u16> = Vec::new();
    let mut was_null = false;

    for chunk in bytes.chunks_exact(2) {
        let c = u16::from_le_bytes([chunk[0], chunk[1]]);
        if c == 0 {
            if was_null {
                break;
            }
            if !current.is_empty() {
                strings.push(String::from_utf16_lossy(&current));
                current.clear();
            }
            was_null = true;
        } else {
            was_null = false;
            current.push(c);
        }
    }

    strings
}

fn current_exe_full_image_name() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    path_to_full_image_name(&exe)
}

/// Strip the `\\?\` (verbatim) prefix from a path if present.
/// This is the pure string portion of [`path_to_full_image_name`].
pub fn strip_verbatim_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

/// Compute the NT-style image path from the stripped DOS path, the volume
/// root (e.g. `C:\`), and the NT volume name (e.g.
/// `\Device\HarddiskVolume3`).
///
/// This is the pure string portion of [`path_to_full_image_name`] that
/// assembles the final `\Device\HarddiskVolume3\Users\me\app.exe` result.
pub fn compute_nt_path(stripped: &str, vol_root: &str, nt_volume: &str) -> String {
    let rest = if stripped
        .to_lowercase()
        .starts_with(&vol_root.to_lowercase())
    {
        &stripped[vol_root.len()..]
    } else {
        stripped
    };
    format!("{}\\{}", nt_volume, rest)
}

fn path_to_full_image_name(path: &Path) -> Result<String, String> {
    let path_str = path.to_string_lossy().to_string();
    // Strip any \\?\ prefix for volume-name lookups.
    let stripped = strip_verbatim_prefix(&path_str);

    let wide = to_wide(stripped);
    let mut volume_buf = vec![0u16; 260];
    let ok = unsafe {
        GetVolumePathNameW(
            wide.as_ptr(),
            volume_buf.as_mut_ptr(),
            volume_buf.len() as u32,
        )
    };
    if ok == 0 {
        return Err(format!(
            "GetVolumePathNameW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let vol_root = from_wide(&volume_buf); // e.g. "C:\"
    let drive = vol_root.trim_end_matches('\\'); // e.g. "C:"

    let drive_wide = to_wide(drive);
    let mut dos_buf = vec![0u16; 260];
    let len = unsafe {
        QueryDosDeviceW(
            drive_wide.as_ptr(),
            dos_buf.as_mut_ptr(),
            dos_buf.len() as u32,
        )
    };
    if len == 0 {
        return Err(format!(
            "QueryDosDeviceW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let nt_volume = from_wide(&dos_buf); // e.g. "\Device\HarddiskVolume3"

    Ok(compute_nt_path(stripped, &vol_root, &nt_volume))
}

// -----------------------------------------------------------------------------
//  HidHide client
// -----------------------------------------------------------------------------

pub struct HidHideClient {
    handle: HANDLE,
}

impl HidHideClient {
    pub fn new() -> Result<Self, String> {
        let path = to_wide(CONTROL_DEVICE_PATH);
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "Unable to open {}: {}",
                CONTROL_DEVICE_PATH,
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { handle })
    }

    /// Detect HidHide by registry + control device.
    pub fn is_installed() -> bool {
        // 1. Registry probe.
        let key = to_wide(r"SYSTEM\CurrentControlSet\Services\HidHide");
        let mut hkey = std::ptr::null_mut();
        let reg_ok =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, key.as_ptr(), 0, KEY_READ, &mut hkey) == 0 };
        if reg_ok {
            unsafe { RegCloseKey(hkey) };
        }

        // 2. Control device probe (installed but busy still means installed).
        match Self::new() {
            Ok(c) => {
                drop(c);
                true
            }
            Err(_) => {
                let err = unsafe { GetLastError() };
                // ACCESS_DENIED / SHARING_VIOLATION / ERROR_FILE_NOT_FOUND may indicate
                // the device object exists but is in use or disabled.
                reg_ok
                    && (err == 5   // ERROR_ACCESS_DENIED
                        || err == 32 // ERROR_SHARING_VIOLATION
                        || err == 2) // ERROR_FILE_NOT_FOUND
            }
        }
    }

    unsafe fn device_io(
        &self,
        code: u32,
        in_buf: *const c_void,
        in_len: u32,
        out_buf: *mut c_void,
        out_len: u32,
    ) -> Result<u32, String> {
        let mut returned: u32 = 0;
        let ok = DeviceIoControl(
            self.handle,
            code,
            in_buf,
            in_len,
            out_buf,
            out_len,
            &mut returned,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            Err(format!(
                "HidHide DeviceIoControl(0x{:08X}) failed: {}",
                code,
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(returned)
        }
    }

    fn get_multi_sz_list(&self, ioctl: u32) -> Result<Vec<String>, String> {
        let mut needed: u32 = 0;
        // First call: get required byte count.
        unsafe {
            let ok = DeviceIoControl(
                self.handle,
                ioctl,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut needed,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err != 122 && err != 234 {
                    // not ERROR_INSUFFICIENT_BUFFER / ERROR_MORE_DATA
                    return Err(format!(
                        "HidHide get list size failed ({}): {}",
                        err,
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }

        if needed == 0 {
            return Ok(Vec::new());
        }

        let mut buffer = vec![0u8; needed as usize];
        let returned = unsafe {
            self.device_io(
                ioctl,
                std::ptr::null(),
                0,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
            )?
        };
        buffer.truncate(returned as usize);
        Ok(decode_multi_sz_bytes(&buffer))
    }

    fn set_multi_sz_list(&self, ioctl: u32, strings: &[String]) -> Result<(), String> {
        let buffer = encode_multi_sz_bytes(strings);
        let mut returned: u32 = 0;
        unsafe {
            let ok = DeviceIoControl(
                self.handle,
                ioctl,
                buffer.as_ptr() as *const c_void,
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(format!(
                    "HidHide set list failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    pub fn get_blacklist(&self) -> Result<Vec<String>, String> {
        self.get_multi_sz_list(IOCTL_GET_BLACKLIST)
    }

    pub fn set_blacklist(&self, list: &[String]) -> Result<(), String> {
        self.set_multi_sz_list(IOCTL_SET_BLACKLIST, list)
    }

    pub fn get_whitelist(&self) -> Result<Vec<String>, String> {
        self.get_multi_sz_list(IOCTL_GET_WHITELIST)
    }

    pub fn set_whitelist(&self, list: &[String]) -> Result<(), String> {
        self.set_multi_sz_list(IOCTL_SET_WHITELIST, list)
    }

    pub fn add_to_whitelist(&self, app_path: &str) -> Result<(), String> {
        let mut list = self.get_whitelist()?;
        let normalized = app_path.to_lowercase();
        if !list.iter().any(|p| p.to_lowercase() == normalized) {
            list.push(app_path.to_string());
            self.set_whitelist(&list)?;
        }
        Ok(())
    }

    pub fn get_active(&self) -> Result<bool, String> {
        let mut out: [u8; 1] = [0];
        let _ = unsafe {
            self.device_io(
                IOCTL_GET_ACTIVE,
                std::ptr::null(),
                0,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
            )?
        };
        Ok(out[0] != 0)
    }

    pub fn set_active(&self, active: bool) -> Result<(), String> {
        let input: [u8; 1] = [active as u8];
        let _ = unsafe {
            self.device_io(
                IOCTL_SET_ACTIVE,
                input.as_ptr() as *const c_void,
                input.len() as u32,
                std::ptr::null_mut(),
                0,
            )?
        };
        Ok(())
    }

    pub fn add_session_blacklist(&self, device_instance_id: &str) -> Result<(), String> {
        let wide = to_wide(device_instance_id);
        let bytes: Vec<u8> = wide.iter().flat_map(|&c| c.to_le_bytes()).collect();
        let mut returned: u32 = 0;
        unsafe {
            let ok = DeviceIoControl(
                self.handle,
                IOCTL_ADD_SESSION_BLACKLIST,
                bytes.as_ptr() as *const c_void,
                bytes.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(format!(
                    "HidHide add_session_blacklist failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    pub fn clear_session_blacklist(&self) -> Result<(), String> {
        let mut returned: u32 = 0;
        unsafe {
            let ok = DeviceIoControl(
                self.handle,
                IOCTL_CLR_SESSION_BLACKLIST,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(format!(
                    "HidHide clear_session_blacklist failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    pub fn setup_for_oxidelink(&self) -> Result<(), String> {
        let device = find_pro_controller()?;
        if let Some(id) = device {
            self.add_session_blacklist(&id)?;
        }

        let exe = current_exe_full_image_name()?;
        if !exe.is_empty() {
            self.add_to_whitelist(&exe)?;
        }

        self.set_active(true)?;
        Ok(())
    }

    pub fn teardown(&self) -> Result<(), String> {
        self.clear_session_blacklist()
    }
}

impl Drop for HidHideClient {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

// -----------------------------------------------------------------------------
//  Device enumeration
// -----------------------------------------------------------------------------

fn is_pro_controller(instance_id: &str) -> bool {
    let lower = instance_id.to_lowercase();
    let has_vid = lower.contains("vid_057e") || lower.contains("vid&0002057e");
    let has_pid = lower.contains("pid_2009") || lower.contains("pid&2009");
    has_vid && has_pid
}

pub fn find_pro_controller() -> Result<Option<String>, String> {
    unsafe {
        let hdevinfo: HDEVINFO = SetupDiGetClassDevsW(
            &GUID_DEVCLASS_HIDCLASS,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT,
        );
        if hdevinfo == -1isize {
            return Err(format!(
                "SetupDiGetClassDevsW failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut index = 0u32;
        loop {
            let mut devinfo: SP_DEVINFO_DATA = std::mem::zeroed();
            devinfo.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(hdevinfo, index, &mut devinfo) == 0 {
                break;
            }

            let mut needed: u32 = 0;
            SetupDiGetDeviceInstanceIdW(hdevinfo, &devinfo, std::ptr::null_mut(), 0, &mut needed);

            if needed > 0 {
                let mut buf = vec![0u16; needed as usize];
                if SetupDiGetDeviceInstanceIdW(
                    hdevinfo,
                    &devinfo,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut needed,
                ) != 0
                {
                    let id = from_wide(&buf);
                    if is_pro_controller(&id) {
                        SetupDiDestroyDeviceInfoList(hdevinfo);
                        return Ok(Some(id));
                    }
                }
            }

            index += 1;
        }

        SetupDiDestroyDeviceInfoList(hdevinfo);
        Ok(None)
    }
}

// -----------------------------------------------------------------------------
//  Tauri command surface (not yet wired into main.rs invoke_handler)
// -----------------------------------------------------------------------------

/// Determine whether a device is effectively hidden.
///
/// A device is hidden only when HidHide is active (enabled) **and** a
/// non-empty device path was found.  This is the pure logic portion of
/// [`get_hidhide_status`].
pub fn compute_hidden_flag(enabled: bool, device_path: &str) -> bool {
    enabled && !device_path.is_empty()
}

fn get_hidhide_status() -> HidHideStatus {
    let mut status = HidHideStatus {
        installed: HidHideClient::is_installed(),
        ..Default::default()
    };

    match find_pro_controller() {
        Ok(Some(path)) => status.device_path = path,
        Ok(None) => status.device_path = String::from("Not found"),
        Err(e) => status.device_path = e,
    }

    match HidHideClient::new() {
        Ok(client) => {
            status.enabled = client.get_active().unwrap_or(false);
            status.hidden = compute_hidden_flag(status.enabled, &status.device_path);
            status.message = String::from("HidHide control device opened");
        }
        Err(e) => {
            status.message = e;
        }
    }

    status
}

#[tauri::command]
pub fn hidhide_get_status() -> HidHideStatus {
    get_hidhide_status()
}

#[tauri::command]
pub fn hidhide_refresh_device_list() -> HidHideStatus {
    get_hidhide_status()
}

#[tauri::command]
pub fn hidhide_hide_controller(ctx: State<'_, AppCtx>) -> Result<HidHideStatus, String> {
    {
        let mut cfg = ctx.shared.config.write();
        cfg.hidhide_enabled = true;
    }

    let client = HidHideClient::new()?;
    client.setup_for_oxidelink()?;
    drop(client);

    Ok(get_hidhide_status())
}

#[tauri::command]
pub fn hidhide_unhide_controller(ctx: State<'_, AppCtx>) -> Result<HidHideStatus, String> {
    {
        let mut cfg = ctx.shared.config.write();
        cfg.hidhide_enabled = false;
    }

    let client = HidHideClient::new()?;
    client.set_active(false)?;
    client.teardown()?;
    drop(client);

    Ok(get_hidhide_status())
}

#[tauri::command]
pub fn hidhide_set_enabled(ctx: State<'_, AppCtx>, enabled: bool) -> Result<HidHideStatus, String> {
    if enabled {
        hidhide_hide_controller(ctx)
    } else {
        hidhide_unhide_controller(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    //  HidHideStatus: defaults & serialization
    // -------------------------------------------------------------------------

    #[test]
    fn default_status_and_multi_sz_helpers_are_consistent() {
        let status = HidHideStatus::default();
        assert!(!status.installed);
        assert!(!status.enabled);
        assert!(!status.hidden);
        assert!(status.device_path.is_empty());
        assert!(status.message.is_empty());
    }

    #[test]
    fn hidhide_status_serializes_to_expected_json() {
        let status = HidHideStatus {
            installed: true,
            enabled: true,
            hidden: true,
            device_path: "HID\\VID_057E&PID_2009\\0".to_string(),
            message: "ok".to_string(),
        };
        let json = serde_json::to_string(&status).expect("serialize");
        assert!(json.contains("\"installed\":true"));
        assert!(json.contains("\"enabled\":true"));
        assert!(json.contains("\"hidden\":true"));
        // JSON escapes backslashes, so a single "\" in the value becomes "\\".
        assert!(json.contains("\"device_path\":\"HID\\\\VID_057E&PID_2009\\\\0\""));
        assert!(json.contains("\"message\":\"ok\""));
    }

    #[test]
    fn hidhide_status_default_serializes_all_false_and_empty_strings() {
        let status = HidHideStatus::default();
        let json = serde_json::to_string(&status).expect("serialize");
        assert!(json.contains("\"installed\":false"));
        assert!(json.contains("\"enabled\":false"));
        assert!(json.contains("\"hidden\":false"));
        assert!(json.contains("\"device_path\":\"\""));
        assert!(json.contains("\"message\":\"\""));
    }

    #[test]
    fn hidhide_status_clone_is_equal() {
        let status = HidHideStatus {
            installed: true,
            enabled: false,
            hidden: false,
            device_path: "dev".to_string(),
            message: "msg".to_string(),
        };
        let cloned = status.clone();
        assert_eq!(cloned.installed, status.installed);
        assert_eq!(cloned.enabled, status.enabled);
        assert_eq!(cloned.hidden, status.hidden);
        assert_eq!(cloned.device_path, status.device_path);
        assert_eq!(cloned.message, status.message);
    }

    // -------------------------------------------------------------------------
    //  IOCTL constants & helpers
    // -------------------------------------------------------------------------

    #[test]
    fn ioctl_helper_uses_standard_ctl_code_layout() {
        assert_eq!(hidhide_ioctl(0x805), IOCTL_SET_ACTIVE);
        assert_eq!(IOCTL_GET_WHITELIST, ctl_code(0x8000, 0x800, 0, 0));
    }

    #[test]
    fn ctl_code_matches_windows_macro_formula() {
        // CTL_CODE(DeviceType, Function, Method, Access) =
        //   (DeviceType << 16) | (Access << 14) | (Function << 2) | Method
        assert_eq!(ctl_code(0x8000, 0x800, 0, 0), 0x80002000);
        assert_eq!(ctl_code(0x8000, 0x801, 0, 0), 0x80002004);
        // (0x2<<16)|(0x1<<14)|(0x1<<2)|0x3 = 0x24007
        assert_eq!(ctl_code(0x2, 0x1, 0x3, 0x1), 0x0002_4007);
    }

    #[test]
    fn hidhide_ioctl_uses_correct_device_type_and_method() {
        // device_type 0x8000, method 0 (BUFFERED), access 0 (ANY)
        // so result = (0x8000 << 16) | (function << 2)
        assert_eq!(hidhide_ioctl(0), 0x80000000);
        assert_eq!(hidhide_ioctl(0x800), 0x80002000);
        assert_eq!(hidhide_ioctl(0x809), 0x80002024);
    }

    #[test]
    fn ioctl_constants_are_sequential_and_well_formed() {
        // Functions 0x800..0x809, each step adds 4 (<< 2).
        let codes = [
            IOCTL_GET_WHITELIST,
            IOCTL_SET_WHITELIST,
            IOCTL_GET_BLACKLIST,
            IOCTL_SET_BLACKLIST,
            IOCTL_GET_ACTIVE,
            IOCTL_SET_ACTIVE,
            IOCTL_GET_WLINVERSE,
            IOCTL_SET_WLINVERSE,
            IOCTL_ADD_SESSION_BLACKLIST,
            IOCTL_CLR_SESSION_BLACKLIST,
        ];
        for (i, &code) in codes.iter().enumerate() {
            let function = 0x800u32 + i as u32;
            assert_eq!(code, hidhide_ioctl(function));
            assert_eq!(code, 0x80000000 | (function << 2));
        }
        // Verify monotonic increase by 4.
        for w in codes.windows(2) {
            assert_eq!(w[1] - w[0], 4);
        }
    }

    #[test]
    fn ioctl_get_and_set_blacklist_are_distinct() {
        assert_ne!(IOCTL_GET_BLACKLIST, IOCTL_SET_BLACKLIST);
        assert_ne!(IOCTL_GET_WHITELIST, IOCTL_SET_WHITELIST);
        assert_ne!(IOCTL_GET_ACTIVE, IOCTL_SET_ACTIVE);
        assert_ne!(IOCTL_GET_WLINVERSE, IOCTL_SET_WLINVERSE);
    }

    #[test]
    fn control_device_path_is_expected() {
        assert_eq!(CONTROL_DEVICE_PATH, r"\\.\HidHide");
    }

    // -------------------------------------------------------------------------
    //  multi_sz encode/decode
    // -------------------------------------------------------------------------

    #[test]
    fn encode_multi_sz_bytes_round_trips_multiple_paths() {
        let paths = vec![
            "\\Device\\HarddiskVolume3\\Users\\me\\app.exe".to_string(),
            "\\Device\\HarddiskVolume3\\Program Files\\other.exe".to_string(),
        ];
        let encoded = encode_multi_sz_bytes(&paths);
        assert_eq!(decode_multi_sz_bytes(&encoded), paths);
    }

    #[test]
    fn encode_multi_sz_bytes_empty_list_produces_terminator_only() {
        let encoded = encode_multi_sz_bytes(&[]);
        // Just the final double-null terminator.
        assert_eq!(encoded, vec![0, 0]);
        assert!(decode_multi_sz_bytes(&encoded).is_empty());
    }

    #[test]
    fn encode_multi_sz_bytes_single_string_terminates_correctly() {
        let paths = vec!["hello".to_string()];
        let encoded = encode_multi_sz_bytes(&paths);
        // "hello" = 5 chars * 2 bytes + 2 (null term) + 2 (final null) = 14
        assert_eq!(encoded.len(), 14);
        assert_eq!(decode_multi_sz_bytes(&encoded), paths);
    }

    #[test]
    fn decode_multi_sz_bytes_handles_empty_input() {
        assert!(decode_multi_sz_bytes(&[]).is_empty());
    }

    #[test]
    fn decode_multi_sz_bytes_handles_no_terminator_gracefully() {
        // Odd-length input is ignored by chunks_exact; even without final
        // double-null, a single null-terminated string still decodes.
        let raw: Vec<u8> = "hi"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain([0, 0].into_iter())
            .collect();
        assert_eq!(decode_multi_sz_bytes(&raw), vec!["hi".to_string()]);
    }

    #[test]
    fn encode_multi_sz_bytes_preserves_unicode() {
        let paths = vec!["café_ñ_emoji😀".to_string()];
        let encoded = encode_multi_sz_bytes(&paths);
        assert_eq!(decode_multi_sz_bytes(&encoded), paths);
    }

    // -------------------------------------------------------------------------
    //  Wide string helpers
    // -------------------------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn wide_string_helpers_round_trip() {
        let original = "OxideLink HidHide";
        let wide = to_wide(original);
        assert_eq!(from_wide(&wide), original);

        let empty = "";
        assert_eq!(from_wide(&to_wide(empty)), empty);
    }

    #[cfg(windows)]
    #[test]
    fn to_wide_is_null_terminated() {
        let wide = to_wide("abc");
        assert_eq!(wide.len(), 4); // 3 chars + null
        assert_eq!(wide[3], 0);
    }

    #[cfg(windows)]
    #[test]
    fn from_wide_handles_no_null_terminator() {
        let wide = vec![b'A' as u16, b'B' as u16, b'C' as u16];
        assert_eq!(from_wide(&wide), "ABC");
    }

    #[cfg(windows)]
    #[test]
    fn from_wide_handles_embedded_null_only_terminates_at_first() {
        let wide = vec![b'A' as u16, 0, b'B' as u16, 0];
        assert_eq!(from_wide(&wide), "A");
    }

    // -------------------------------------------------------------------------
    //  Registry path building (string construction only — no registry access)
    // -------------------------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn registry_service_path_string_is_correct() {
        // This mirrors the key path used in is_installed(); we only verify
        // the string content, never open the key.
        let key_str = r"SYSTEM\CurrentControlSet\Services\HidHide";
        let wide = to_wide(key_str);
        assert_eq!(from_wide(&wide), key_str);
        // First char must be 'S'.
        assert_eq!(wide[0], 'S' as u16);
    }

    #[cfg(windows)]
    #[test]
    fn registry_path_wide_encoding_has_null_terminator() {
        let key_str = r"SYSTEM\CurrentControlSet\Services\HidHide";
        let wide = to_wide(key_str);
        assert_eq!(*wide.last().unwrap(), 0);
    }

    // -------------------------------------------------------------------------
    //  Pro Controller device path matching / filtering
    // -------------------------------------------------------------------------

    #[test]
    fn pro_controller_detection_matches_expected_vid_pid() {
        assert!(is_pro_controller(
            "HID\\VID_057E&PID_2009\\6&1234567&0&0000"
        ));
        assert!(is_pro_controller("hid\\vid&0002057e_pid&2009\\123"));
        assert!(!is_pro_controller("HID\\VID_1234&PID_5678\\foo"));
        assert!(!is_pro_controller("random string"));
    }

    #[test]
    fn pro_controller_detection_is_case_insensitive() {
        assert!(is_pro_controller("HID\\vid_057e&pid_2009\\0"));
        assert!(is_pro_controller("HID\\VID_057E&PID_2009\\0"));
        assert!(is_pro_controller("HID\\Vid_057E&Pid_2009\\0"));
    }

    #[test]
    fn pro_controller_detection_requires_both_vid_and_pid() {
        assert!(!is_pro_controller("HID\\VID_057E&PID_9999\\0"));
        assert!(!is_pro_controller("HID\\VID_9999&PID_2009\\0"));
        assert!(!is_pro_controller("HID\\VID_057E\\0"));
        assert!(!is_pro_controller("HID\\PID_2009\\0"));
    }

    #[test]
    fn pro_controller_detection_accepts_alt_vid_format() {
        assert!(is_pro_controller("hid\\vid&0002057e&pid&2009\\0"));
        assert!(is_pro_controller("HID\\VID&0002057E_PID&2009\\0"));
    }

    #[test]
    fn pro_controller_detection_rejects_empty_and_garbage() {
        assert!(!is_pro_controller(""));
        assert!(!is_pro_controller("   "));
        // Note: "VID_057EPID_2009" *does* match because is_pro_controller
        // only checks for substring presence, not delimiter boundaries.
        // This is a known characteristic of the matcher.
        assert!(!is_pro_controller("totally unrelated text"));
    }

    #[test]
    fn pro_controller_detection_matches_joycon_vid() {
        // Joy-Con share VID 057E but have different PIDs; ensure PID_2009
        // specifically is required.
        assert!(!is_pro_controller("HID\\VID_057E&PID_2006\\0"));
        assert!(!is_pro_controller("HID\\VID_057E&PID_2007\\0"));
    }

    // -------------------------------------------------------------------------
    //  add_to_whitelist dedup logic (pure helper portion)
    // -------------------------------------------------------------------------

    #[test]
    fn whitelist_dedup_comparison_is_case_insensitive() {
        // Simulate the comparison logic used in add_to_whitelist without
        // performing any I/O.
        let existing = vec!["\\Device\\App.exe".to_string()];
        let normalized = "\\device\\app.exe".to_lowercase();
        let already_present = existing
            .iter()
            .any(|p| p.to_lowercase() == normalized);
        assert!(already_present);
    }

    #[test]
    fn whitelist_dedup_detects_new_entry() {
        let existing = vec!["\\Device\\Other.exe".to_string()];
        let normalized = "\\device\\app.exe".to_lowercase();
        let already_present = existing
            .iter()
            .any(|p| p.to_lowercase() == normalized);
        assert!(!already_present);
    }

    // -------------------------------------------------------------------------
    //  Session blacklist byte encoding (pure computation)
    // -------------------------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn session_blacklist_wide_to_bytes_is_little_endian_utf16() {
        let id = "HID\\VID_057E&PID_2009\\0";
        let wide = to_wide(id);
        let bytes: Vec<u8> = wide.iter().flat_map(|&c| c.to_le_bytes()).collect();
        // First two bytes = LE encoding of 'H' (0x48 0x00).
        assert_eq!(bytes[0], 0x48);
        assert_eq!(bytes[1], 0x00);
        // Length = wide.len() * 2 (includes null terminator).
        assert_eq!(bytes.len(), wide.len() * 2);
    }

    // -------------------------------------------------------------------------
    //  GUID constant sanity
    // -------------------------------------------------------------------------

    #[test]
    fn guid_devclass_hidclass_has_expected_value() {
        assert_eq!(GUID_DEVCLASS_HIDCLASS.data1, 0x745a17a0);
        assert_eq!(GUID_DEVCLASS_HIDCLASS.data2, 0x74d3);
        assert_eq!(GUID_DEVCLASS_HIDCLASS.data3, 0x11d0);
        assert_eq!(
            GUID_DEVCLASS_HIDCLASS.data4,
            [0xb6, 0xfe, 0x00, 0xa0, 0xc9, 0x0f, 0x57, 0xda]
        );
    }
}
