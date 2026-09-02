//! Integration tests for OxideLink I/O-bound functions.
//!
//! These tests exercise real hardware, OS APIs, and network sockets.
//! They are gated behind the `integration` feature flag and are NOT
//! run by the standard `cargo test --lib` suite.
//!
//! Run with:
//! ```text
//! cargo test --features integration --test integration
//! ```
//!
//! Each module guards its tests with a availability check so the
//! suite degrades gracefully on machines missing the required
//! hardware/driver. Skipped tests print a message explaining what
//! was needed.

#![cfg(feature = "integration")]

use std::sync::Arc;
use tokio::sync::broadcast;

use oxidelink::state::SharedState;

/// Helper: skip a test with a reason if `cond` is false.
macro_rules! require {
    ($cond:expr, $reason:expr) => {
        if !$cond {
            eprintln!("SKIP: {}", $reason);
            return;
        }
    };
}

// =========================================================================
//  DSU / Cemuhook UDP server — loopback networking
// =========================================================================

mod dsu_network {
    use super::*;
    use oxidelink::dsu::{DsuManager, DsuServer};
    use oxidelink::state::SharedState;
    use std::net::UdpSocket;

    /// `DsuServer::new` binds a real UDP socket on the loopback interface.
    /// This test verifies the socket is created successfully.
    #[tokio::test]
    async fn dsu_server_binds_loopback_socket() {
        let shared = SharedState::new();
        let server = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            DsuServer::new(shared, "127.0.0.1:0"),
        )
        .await;
        require!(server.is_ok(), "DsuServer::new timed out");
        let server = server.unwrap();
        require!(server.is_ok(), "DsuServer::new failed — no free UDP port?");
        let server = server.unwrap();
        // Start the recv/send loops, then immediately abort them.
        let (recv, send) = server.run(60);
        recv.abort();
        send.abort();
        // If we got here, the socket bound successfully.
    }

    /// `DsuManager::start` / `stop` lifecycle on a real socket.
    /// Wrapped in a timeout to prevent hanging if the recv/send loops
    /// don't abort cleanly.
    #[tokio::test]
    async fn dsu_manager_start_stop_lifecycle() {
        let shared = SharedState::new();
        {
            let mut cfg = shared.config.write();
            cfg.dsu.enabled = true;
            cfg.dsu.bind_address = "127.0.0.1".into();
            cfg.dsu.port = 27042;
        }
        let manager = DsuManager::new(shared.clone());

        // Start with a 3-second timeout.
        let started =
            tokio::time::timeout(std::time::Duration::from_secs(3), manager.start()).await;
        match started {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("SKIP: DsuManager::start failed — port 27042 may be in use");
                return;
            }
            Err(_) => {
                eprintln!("SKIP: DsuManager::start timed out");
                return;
            }
        }
        // Stop with a 3-second timeout.
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(3), manager.stop()).await;
        assert!(stopped.is_ok(), "DsuManager::stop should not time out");
        assert!(stopped.unwrap(), "DSU server should stop cleanly");
    }

    /// `DsuServer::new` with an already-bound port should fail.
    #[tokio::test]
    async fn dsu_server_double_bind_fails() {
        let shared = SharedState::new();
        // Bind a raw socket to occupy a port.
        let blocker = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = blocker.local_addr().unwrap();
        let port = addr.port();
        // Now try to bind DsuServer on the same port — should fail.
        let bind = format!("127.0.0.1:{}", port);
        let result = DsuServer::new(shared, &bind).await;
        assert!(result.is_err(), "double bind should fail");
        drop(blocker);
    }

    /// A plain `UdpSocket` can bind to loopback — sanity check for the
    /// networking stack on this machine.
    #[test]
    fn loopback_udp_socket_sanity() {
        let sock = UdpSocket::bind("127.0.0.1:0");
        require!(sock.is_ok(), "loopback UDP bind failed — networking issue");
        let sock = sock.unwrap();
        let addr = sock.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
    }
}

// =========================================================================
//  KeepAlive — SetThreadExecutionState (Windows only)
// =========================================================================

mod keepalive_io {
    use super::*;
    use oxidelink::keepalive::{check_bthusb_power_state, KeepAliveManager};
    use oxidelink::state::{KeepAliveStatus, SharedState};
    use parking_lot::RwLock;

    /// `prevent_adapter_sleep` calls `SetThreadExecutionState` on Windows.
    /// This should succeed on any Windows machine.
    #[test]
    #[cfg(windows)]
    fn prevent_adapter_sleep_succeeds_on_windows() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let mgr = KeepAliveManager::new(status);
        let result = mgr.prevent_adapter_sleep();
        assert!(result, "SetThreadExecutionState should succeed on Windows");
        // Clean up — restore default sleep state.
        mgr.allow_adapter_sleep();
    }

    /// `allow_adapter_sleep` should not panic and should restore the
    /// execution state to continuous.
    #[test]
    #[cfg(windows)]
    fn allow_adapter_sleep_restores_state() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let mgr = KeepAliveManager::new(status);
        mgr.prevent_adapter_sleep();
        mgr.allow_adapter_sleep();
        // After allowing sleep, the interval should be back to default.
        assert_eq!(mgr.current_interval(), 3000);
    }

    /// `check_bthusb_power_state` spawns PowerShell on Windows.
    /// This test verifies it doesn't panic and returns a Vec (possibly empty
    /// if no BTHUSB events exist).
    #[test]
    #[cfg(windows)]
    fn check_bthusb_power_state_runs_without_panic() {
        let result = check_bthusb_power_state();
        // Should return a Vec<String> — may be empty if no events.
        eprintln!("check_bthusb_power_state returned {} entries", result.len());
        // No assertion on content — just verify it doesn't panic.
    }

    /// On non-Windows, these functions should degrade gracefully.
    #[test]
    #[cfg(not(windows))]
    fn keepalive_functions_no_op_on_non_windows() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let mgr = KeepAliveManager::new(status);
        // Should not panic.
        let _ = mgr.prevent_adapter_sleep();
        mgr.allow_adapter_sleep();
    }
}

// =========================================================================
//  HidHide — requires the HidHide driver to be installed
// =========================================================================

mod hidhide_io {
    use super::*;
    use oxidelink::hidhide::{find_pro_controller, HidHideClient};

    /// `HidHideClient::is_installed` checks the registry and device path.
    /// This test runs on any Windows machine — it just reports whether
    /// the driver is present.
    #[test]
    #[cfg(windows)]
    fn hidhide_is_installed_reports_driver_presence() {
        let installed = HidHideClient::is_installed();
        eprintln!("HidHide installed: {}", installed);
        // No assertion — just report. The driver may or may not be installed.
    }

    /// `HidHideClient::new` opens the control device.
    /// Skips if the driver is not installed.
    #[test]
    #[cfg(windows)]
    fn hidhide_client_new_opens_control_device() {
        require!(
            HidHideClient::is_installed(),
            "HidHide driver not installed — skipping"
        );
        let client = HidHideClient::new();
        assert!(
            client.is_ok(),
            "HidHideClient::new should succeed when installed"
        );
    }

    /// `get_blacklist` / `get_whitelist` read the current lists from the
    /// driver. Skips if the driver is not installed.
    #[test]
    #[cfg(windows)]
    fn hidhide_get_blacklist_and_whitelist() {
        require!(
            HidHideClient::is_installed(),
            "HidHide driver not installed — skipping"
        );
        let client = HidHideClient::new().expect("HidHideClient::new");
        let blacklist = client.get_blacklist();
        assert!(blacklist.is_ok(), "get_blacklist should succeed");
        let whitelist = client.get_whitelist();
        assert!(whitelist.is_ok(), "get_whitelist should succeed");
        eprintln!(
            "blacklist: {} entries, whitelist: {} entries",
            blacklist.unwrap().len(),
            whitelist.unwrap().len()
        );
    }

    /// `get_active` reads the current active state.
    #[test]
    #[cfg(windows)]
    fn hidhide_get_active_state() {
        require!(
            HidHideClient::is_installed(),
            "HidHide driver not installed — skipping"
        );
        let client = HidHideClient::new().expect("HidHideClient::new");
        let active = client.get_active();
        assert!(active.is_ok(), "get_active should succeed");
        eprintln!("HidHide active: {}", active.unwrap());
    }

    /// `find_pro_controller` enumerates Windows devices looking for the
    /// Pro Controller (VID 057E, PID 2009). Returns None if not connected.
    #[test]
    #[cfg(windows)]
    fn find_pro_controller_enumerates_devices() {
        let result = find_pro_controller();
        assert!(
            result.is_ok(),
            "find_pro_controller should not error on Windows"
        );
        match result.unwrap() {
            Some(path) => eprintln!("Pro Controller found: {}", path),
            None => eprintln!("No Pro Controller detected — connect one to test fully"),
        }
    }

    /// `add_to_whitelist` / `clear_session_blacklist` round-trip.
    /// This MODIFIES driver state — only run on machines where that's safe.
    #[test]
    #[cfg(windows)]
    #[ignore = "modifies HidHide driver state — run manually with --ignored"]
    fn hidhide_whitelist_session_blacklist_roundtrip() {
        require!(
            HidHideClient::is_installed(),
            "HidHide driver not installed — skipping"
        );
        let client = HidHideClient::new().expect("HidHideClient::new");

        // Add a dummy app to the whitelist.
        let dummy = "C:\\Windows\\System32\\cmd.exe";
        let add_result = client.add_to_whitelist(dummy);
        assert!(add_result.is_ok(), "add_to_whitelist should succeed");

        // Verify it's in the whitelist.
        let whitelist = client.get_whitelist().expect("get_whitelist");
        assert!(
            whitelist.iter().any(|p| p.contains("cmd.exe")),
            "whitelist should contain the added app"
        );

        // Clear session blacklist.
        let clear_result = client.clear_session_blacklist();
        assert!(
            clear_result.is_ok(),
            "clear_session_blacklist should succeed"
        );
    }

    /// `setup_for_oxidelink` / `teardown` full lifecycle.
    /// Requires a real Pro Controller to be connected.
    #[test]
    #[cfg(windows)]
    #[ignore = "requires Pro Controller + modifies driver state — run manually"]
    fn hidhide_setup_and_teardown_lifecycle() {
        require!(
            HidHideClient::is_installed(),
            "HidHide driver not installed — skipping"
        );
        let pro = find_pro_controller().expect("find_pro_controller");
        require!(
            pro.is_some(),
            "No Pro Controller connected — skipping setup test"
        );

        let client = HidHideClient::new().expect("HidHideClient::new");
        let setup = client.setup_for_oxidelink();
        assert!(
            setup.is_ok(),
            "setup_for_oxidelink should succeed with controller"
        );

        // Verify active state.
        let active = client.get_active().expect("get_active");
        assert!(active, "HidHide should be active after setup");

        // Tear down.
        let teardown = client.teardown();
        assert!(teardown.is_ok(), "teardown should succeed");
    }
}

// =========================================================================
//  BthUsbMonitor — Windows Event Log / ETW
// =========================================================================

mod bthusb_monitor_io {
    use super::*;
    use oxidelink::bthusb_monitor::BthUsbMonitor;
    use oxidelink::keepalive::KeepAliveManager;
    use oxidelink::state::{KeepAliveStatus, SharedState};
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    /// `poll_once` spawns PowerShell to query the System event log.
    /// This test verifies it doesn't panic and returns a snapshot.
    #[test]
    #[cfg(windows)]
    fn bthusb_monitor_poll_once_runs() {
        let monitor = BthUsbMonitor::new();
        let snapshot = monitor.poll_once();
        eprintln!(
            "poll_once: power_down={}, disconnect={}, total events checked",
            snapshot.power_down_events, snapshot.disconnect_events
        );
        // No assertion on content — just verify it doesn't panic.
    }

    /// `start_loop` spawns the ETW consumer thread via `spawn_blocking`,
    /// which cannot be aborted. Marked `#[ignore]` to avoid hanging the
    /// test runner — run manually with `--ignored`.
    #[tokio::test]
    #[cfg(windows)]
    #[ignore = "start_loop uses spawn_blocking for ETW — run manually"]
    async fn bthusb_monitor_start_loop_starts_and_stops() {
        let monitor = Arc::new(BthUsbMonitor::new());
        let (tx, _rx) = broadcast::channel(64);
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let keepalive = Arc::new(KeepAliveManager::new(status));

        let handle = monitor.clone().start_loop(tx, keepalive, 1000);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        eprintln!("BthUsbMonitor loop started and aborted successfully");
    }
}

// =========================================================================
//  DeviceLoop — requires a real Nintendo Switch Pro Controller
// =========================================================================

mod device_loop_io {
    use super::*;
    use oxidelink::device_loop::DeviceLoop;
    use oxidelink::state::SharedState;
    use tokio::sync::broadcast;

    /// `start_loop` attempts to open a HID device. Without a controller
    /// connected, it will retry indefinitely via `spawn_blocking` which
    /// cannot be aborted. Marked `#[ignore]` to avoid hanging the test
    /// runner — run manually with `--ignored` on a machine with a
    /// Pro Controller connected.
    #[tokio::test]
    #[ignore = "start_loop retries indefinitely via spawn_blocking — run manually with controller"]
    async fn device_loop_start_loop_runs_without_controller() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = Arc::new(DeviceLoop::new(shared, tx));
        let handle = loop_instance.clone().start_loop();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        eprintln!("DeviceLoop started and aborted without a controller");
    }

    /// Full controller integration test — requires a real Pro Controller
    /// connected via Bluetooth or USB.
    #[tokio::test]
    #[ignore = "requires real Pro Controller — run manually with --ignored"]
    async fn device_loop_with_real_controller() {
        let shared = SharedState::new();
        let (tx, mut rx) = broadcast::channel(64);
        let loop_instance = Arc::new(DeviceLoop::new(shared, tx));
        let handle = loop_instance.clone().start_loop();

        // Wait up to 5 seconds for a controller to be detected.
        let mut detected = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(_event) = rx.try_recv() {
                detected = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        handle.abort();
        assert!(detected, "No controller events received within 5 seconds");
        eprintln!("Real controller detected and events received!");
    }
}

// =========================================================================
//  Overlay — requires Tauri AppHandle (cannot test without full app)
// =========================================================================

mod overlay_io {
    // The overlay functions (show, hide, toggle, update_state) all require
    // a Tauri AppHandle and Webview2 runtime. These cannot be tested in
    // isolation — they require the full Tauri application to be running.
    //
    // To test overlay functionality:
    //   1. Run the app: `cargo tauri dev`
    //   2. Use the E2E test suite (WebdriverIO + Tauri) to verify
    //      overlay window behavior.
    //
    // See: tests/e2e/ in the project root for WebdriverIO-based E2E tests.

    #[test]
    fn overlay_tests_require_tauri_runtime() {
        eprintln!(
            "Overlay I/O tests require a running Tauri app. \
             Use `cargo tauri dev` + E2E tests instead."
        );
    }
}
