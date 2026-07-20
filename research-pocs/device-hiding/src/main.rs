//! Proof-of-concept: enumerate HID devices to find the Switch Pro Controller and check HidHide install.

use std::ptr;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, SPDRP_HARDWAREID, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CLASSES_ROOT, KEY_READ,
};

const NINTENDO_VID: &str = "VID_057E";
const PRO_PID: &str = "PID_2009";

unsafe fn is_hidhide_installed() -> bool {
    let path: Vec<u16> = r"Installer\Dependencies\NSS.Drivers.HidHide.x64"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut key: HKEY = ptr::null_mut();
    if RegOpenKeyExW(HKEY_CLASSES_ROOT, path.as_ptr(), 0, KEY_READ, &mut key) != 0 {
        return false;
    }
    let mut dtype = 0u32;
    let mut size = 0u32;
    let found = RegQueryValueExW(key, ptr::null(), ptr::null(), &mut dtype, ptr::null_mut(), &mut size) == 0;
    RegCloseKey(key);
    found
}

unsafe fn can_open_hidhide() -> bool {
    let name: Vec<u16> = r"\\.\HidHide".encode_utf16().chain(std::iter::once(0)).collect();
    let h = CreateFileW(
        name.as_ptr(),
        0, // no access needed to test open
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        ptr::null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        ptr::null_mut(),
    );
    if h == -1isize as HANDLE || h == ptr::null_mut() {
        return false;
    }
    CloseHandle(h);
    true
}

unsafe fn find_pro_controller() -> Option<String> {
    let hid_guid = GUID {
        data1: 0x4D1E55B2,
        data2: 0xF16F,
        data3: 0x11CF,
        data4: [0x88, 0xCB, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30],
    };
    let set = SetupDiGetClassDevsW(
        &hid_guid,
        ptr::null(),
        ptr::null_mut() as HWND,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    );
    if set == -1isize {
        return None;
    }

    let mut dev_info = SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ClassGuid: GUID {
            data1: 0,
            data2: 0,
            data3: 0,
            data4: [0; 8],
        },
        DevInst: 0,
        Reserved: 0,
    };

    let mut idx = 0u32;
    while SetupDiEnumDeviceInfo(set, idx, &mut dev_info) != 0 {
        idx += 1;
        let mut hwid = [0u16; 256];
        let mut needed = 0u32;
        if SetupDiGetDeviceRegistryPropertyW(
            set,
            &mut dev_info,
            SPDRP_HARDWAREID,
            ptr::null_mut(),
            hwid.as_mut_ptr() as *mut u8,
            hwid.len() as u32 * 2,
            &mut needed,
        ) != 0
        {
            let s = String::from_utf16_lossy(&hwid[..(needed as usize / 2)]);
            if s.contains(NINTENDO_VID) && s.contains(PRO_PID) {
                let mut id = [0u16; 256];
                let mut id_needed = 0u32;
                if SetupDiGetDeviceInstanceIdW(
                    set,
                    &mut dev_info,
                    id.as_mut_ptr(),
                    id.len() as u32,
                    &mut id_needed,
                ) != 0
                {
                    SetupDiDestroyDeviceInfoList(set);
                    let len = id.iter().position(|&c| c == 0).unwrap_or(id_needed as usize);
                    return Some(String::from_utf16_lossy(&id[..len]));
                }
            }
        }
    }
    SetupDiDestroyDeviceInfoList(set);
    None
}

fn main() {
    unsafe {
        println!(
            "HidHide installed: {} (openable: {})",
            is_hidhide_installed(),
            can_open_hidhide()
        );
        match find_pro_controller() {
            Some(path) => println!("Found Pro Controller: {}", path),
            None => println!("Pro Controller not found"),
        }
    }
}
