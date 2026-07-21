//! Bluetooth auto-reconnect for the Pro Controller.
//!
//! When the Pro Controller is disconnected from USB, the STM32 bridge MCU
//! reverts to Bluetooth mode after its default timeout (~5 seconds). The
//! controller then tries to connect to the last paired host. However, Windows
//! does not always automatically accept the incoming connection even when the
//! device is paired — the HID service may need to be re-enabled.
//!
//! This module uses the Win32 Bluetooth API (`BluetoothSetServiceState`) to
//! re-enable the HID service for the paired Pro Controller, which triggers
//! Windows to initiate (or accept) a Bluetooth connection. This mirrors the
//! behavior of the Nintendo Switch, where unplugging USB seamlessly
//! transitions to Bluetooth.
//!
//! All functions are `cfg(windows)` only.

#![cfg(windows)]

use log::{info, warn};
use windows_sys::Win32::Devices::Bluetooth::{
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindFirstRadio,
    BluetoothFindNextDevice, BluetoothFindNextRadio, BluetoothFindRadioClose,
    BluetoothGetDeviceInfo, BluetoothSetServiceState, BLUETOOTH_DEVICE_INFO,
    BLUETOOTH_DEVICE_SEARCH_PARAMS, BLUETOOTH_FIND_RADIO_PARAMS, HBLUETOOTH_DEVICE_FIND,
    HBLUETOOTH_RADIO_FIND,
};
use windows_sys::Win32::Foundation::{FALSE, HANDLE, TRUE};

/// GUID for the Bluetooth HID service (Human Interface Device).
/// {00001124-0000-1000-8000-00805F9B34FB}
const HID_SERVICE_GUID: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(
    0x00001124_0000_1000_8000_00805F9B34FB,
);

/// Maximum number of Bluetooth radios to scan.
const MAX_RADIOS: usize = 8;

/// Maximum number of Bluetooth devices to scan per radio.
const MAX_DEVICES: usize = 256;

/// Enables the HID service for the paired Pro Controller, triggering a
/// Bluetooth connection. Returns `true` if the service was successfully
/// enabled (or was already enabled) for at least one paired Pro Controller.
///
/// This function enumerates all Bluetooth radios, then for each radio
/// enumerates all remembered/authenticated Bluetooth devices looking for
/// one named "Pro Controller". When found, it calls
/// `BluetoothSetServiceState` to enable the HID service, which causes
/// Windows to connect to the device.
pub fn trigger_pro_controller_reconnect() -> bool {
    let mut found = false;
    let mut connected = false;

    // Enumerate Bluetooth radios.
    let mut radio_params: BLUETOOTH_FIND_RADIO_PARAMS =
        unsafe { std::mem::zeroed() };
    radio_params.dwSize = std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32;

    let mut radio_handle: HANDLE = std::ptr::null_mut();
    let radio_find: HBLUETOOTH_RADIO_FIND = unsafe {
        BluetoothFindFirstRadio(&radio_params, &mut radio_handle)
    };

    if radio_find.is_null() {
        warn!("No Bluetooth radios found — cannot trigger BT reconnect");
        return false;
    }

    let mut radio_count = 0;
    loop {
        radio_count += 1;
        if trigger_reconnect_on_radio(radio_handle, &mut found, &mut connected) {
            break;
        }
        // Close the radio handle before getting the next one.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(radio_handle);
        }
        if radio_count >= MAX_RADIOS {
            break;
        }
        let mut next_handle: HANDLE = std::ptr::null_mut();
        let ok = unsafe { BluetoothFindNextRadio(radio_find, &mut next_handle) };
        if ok == FALSE {
            break;
        }
        radio_handle = next_handle;
    }

    unsafe {
        BluetoothFindRadioClose(radio_find);
    }

    if found {
        if connected {
            info!("Bluetooth reconnect triggered for Pro Controller");
        } else {
            warn!(
                "Pro Controller found in Bluetooth cache but HID service enable failed"
            );
        }
    } else {
        warn!(
            "No paired Pro Controller found in Bluetooth device cache — \
             controller may not be paired with Windows"
        );
    }

    connected
}

/// Scans devices on a single Bluetooth radio for a Pro Controller and
/// attempts to enable the HID service. Returns `true` if a Pro Controller
/// was found and the HID service was successfully enabled.
fn trigger_reconnect_on_radio(radio_handle: HANDLE, found: &mut bool, connected: &mut bool) -> bool {
    // Set up search params: only return authenticated (paired) devices.
    let mut search_params: BLUETOOTH_DEVICE_SEARCH_PARAMS =
        unsafe { std::mem::zeroed() };
    search_params.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32;
    search_params.fReturnAuthenticated = TRUE;
    search_params.fReturnRemembered = TRUE;
    search_params.fReturnConnected = TRUE;
    search_params.fReturnUnknown = FALSE;
    search_params.fIssueInquiry = FALSE;
    search_params.cTimeoutMultiplier = 0;
    search_params.hRadio = radio_handle;

    let mut device_info: BLUETOOTH_DEVICE_INFO = unsafe { std::mem::zeroed() };
    device_info.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32;

    let device_find: HBLUETOOTH_DEVICE_FIND =
        unsafe { BluetoothFindFirstDevice(&search_params, &mut device_info) };

    if device_find.is_null() {
        return false;
    }

    let mut device_count = 0;
    loop {
        device_count += 1;

        // Refresh device info (fConnected, stLastSeen, etc.)
        let _ = unsafe { BluetoothGetDeviceInfo(radio_handle, &mut device_info) };

        let name = wide_to_string(&device_info.szName);
        if is_pro_controller_name(&name) {
            *found = true;
            let addr = format_bt_address(&device_info.Address);
            info!(
                "Found paired Pro Controller via Bluetooth: name=\"{}\" addr={} connected={}",
                name,
                addr,
                device_info.fConnected != FALSE
            );

            if device_info.fConnected == FALSE {
                // Enable the HID service — this triggers Windows to
                // initiate a connection to the device.
                let result = unsafe {
                    BluetoothSetServiceState(
                        radio_handle,
                        &device_info,
                        &HID_SERVICE_GUID,
                        0, // BLUETOOTH_SERVICE_ENABLE
                    )
                };
                if result == 0 {
                    info!("HID service enabled for Pro Controller ({}) — BT reconnect triggered", addr);
                    *connected = true;
                    unsafe {
                        BluetoothFindDeviceClose(device_find);
                    }
                    return true;
                } else {
                    warn!(
                        "BluetoothSetServiceState failed with error code {} for Pro Controller ({})",
                        result, addr
                    );
                }
            } else {
                info!("Pro Controller ({}) is already connected over Bluetooth", addr);
                *connected = true;
                unsafe {
                    BluetoothFindDeviceClose(device_find);
                }
                return true;
            }
        }

        if device_count >= MAX_DEVICES {
            break;
        }
        let ok = unsafe { BluetoothFindNextDevice(device_find, &mut device_info) };
        if ok == FALSE {
            break;
        }
    }

    unsafe {
        BluetoothFindDeviceClose(device_find);
    }
    false
}

/// Converts a wide (UTF-16) null-terminated array to a Rust String.
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Checks if a Bluetooth device name looks like a Pro Controller.
/// The Pro Controller advertises as "Pro Controller" over Bluetooth.
fn is_pro_controller_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("pro controller")
}

/// Formats a `BLUETOOTH_ADDRESS` as a colon-separated MAC string.
fn format_bt_address(addr: &windows_sys::Win32::Devices::Bluetooth::BLUETOOTH_ADDRESS) -> String {
    let bytes = unsafe { addr.Anonymous.rgBytes };
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bytes[5], bytes[4], bytes[3], bytes[2], bytes[1], bytes[0]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_pro_controller_name() {
        assert!(is_pro_controller_name("Pro Controller"));
        assert!(is_pro_controller_name("Nintendo Pro Controller"));
        assert!(is_pro_controller_name("pro controller"));
        assert!(!is_pro_controller_name("Joy-Con (L)"));
        assert!(!is_pro_controller_name(""));
        assert!(!is_pro_controller_name("Unknown Device"));
    }

    #[test]
    fn test_wide_to_string() {
        let buf: [u16; 248] = {
            let mut arr = [0u16; 248];
            let s = "Pro Controller";
            for (i, c) in s.encode_utf16().enumerate() {
                arr[i] = c;
            }
            arr
        };
        assert_eq!(wide_to_string(&buf), "Pro Controller");
    }

    #[test]
    fn test_wide_to_string_empty() {
        let buf = [0u16; 248];
        assert_eq!(wide_to_string(&buf), "");
    }

    #[test]
    fn test_format_bt_address() {
        let mut addr: windows_sys::Win32::Devices::Bluetooth::BLUETOOTH_ADDRESS =
            unsafe { std::mem::zeroed() };
        unsafe {
            addr.Anonymous.rgBytes = [0x22, 0x6C, 0x78, 0x77, 0x31, 0x48];
        }
        assert_eq!(format_bt_address(&addr), "48:31:77:78:6C:22");
    }

    #[test]
    fn test_hid_service_guid() {
        // Verify the GUID matches the Bluetooth HID service UUID.
        assert_eq!(HID_SERVICE_GUID.data1, 0x00001124);
    }
}
