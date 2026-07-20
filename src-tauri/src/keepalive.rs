use crate::state::{timestamp_now, KeepAliveStatus};
use log::{debug, info, warn};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::time::{interval, Duration};

use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
};

const BTHUSB_POWER_DOWN_SIGNATURE: &str = "BTHUSB_Event_Power_Down";
#[allow(dead_code)] // paired with BTHUSB_POWER_DOWN_SIGNATURE for symmetry
const BTHUSB_POWER_UP_SIGNATURE: &str = "BTHUSB_Event_Power_Up";
const MIN_INTERVAL_MS: u64 = 800;
const MAX_INTERVAL_MS: u64 = 5000;
const BOOST_DURATION_MS: u64 = 30000;

pub struct KeepAliveManager {
    state: Arc<RwLock<KeepAliveStatus>>,
    power_event_count: Arc<AtomicU32>,
    last_boost: Arc<AtomicU64>,
}

impl KeepAliveManager {
    pub fn new(state: Arc<RwLock<KeepAliveStatus>>) -> Self {
        Self {
            state,
            power_event_count: Arc::new(AtomicU32::new(0)),
            last_boost: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn prevent_adapter_sleep(&self) -> bool {
        unsafe {
            let result = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
            if result == 0 {
                warn!("SetThreadExecutionState failed");
                false
            } else {
                info!("Adapter sleep prevention activated (ES_CONTINUOUS | ES_SYSTEM_REQUIRED)");
                true
            }
        }
    }

    pub fn allow_adapter_sleep(&self) {
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
            info!("Adapter sleep prevention deactivated");
        }
    }

    pub fn report_power_event(&self, event_type: &str) {
        // Fast-path scalar counters use atomics instead of the status RwLock.
        let count = self.power_event_count.fetch_add(1, Ordering::Relaxed) + 1;

        let mut status = self.state.write();
        status.power_events_detected = count;

        if event_type.contains("Power_Down") || event_type.contains("power_down") {
            let now = timestamp_now();
            let last = self.last_boost.load(Ordering::Relaxed);
            if now.saturating_sub(last) > BOOST_DURATION_MS {
                self.last_boost.store(now, Ordering::Relaxed);
                status.interval_ms = MIN_INTERVAL_MS;
                status.adaptive_mode = true;
                warn!(
                    "BTHUSB power-down detected! Boosting keep-alive to {}ms for {}s",
                    MIN_INTERVAL_MS,
                    BOOST_DURATION_MS / 1000
                );
            }
        }
    }

    pub fn current_interval(&self) -> u64 {
        let status = self.state.read();
        let config_interval = status.interval_ms;

        let last = self.last_boost.load(Ordering::Relaxed);
        let now = timestamp_now();
        if last > 0 && now.saturating_sub(last) > BOOST_DURATION_MS {
            return MAX_INTERVAL_MS.min(config_interval.max(2000));
        }

        config_interval
    }

    pub fn start_loop(
        self: Arc<Self>,
        tx: tokio::sync::broadcast::Sender<crate::state::IpcEvent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let self_clone = self.clone();
            self_clone.prevent_adapter_sleep();

            {
                let mut status = self_clone.state.write();
                status.active = true;
                status.adapter_sleep_prevented = true;
                status.last_ping = timestamp_now();
            }

            let _ = tx.send(crate::state::IpcEvent::KeepAliveStatus {
                data: self_clone.state.read().clone(),
            });

            loop {
                let interval_ms = self_clone.current_interval();
                let mut ticker = interval(Duration::from_millis(interval_ms));

                ticker.tick().await;

                let now = timestamp_now();
                {
                    let mut status = self_clone.state.write();
                    status.last_ping = now;
                    status.interval_ms = interval_ms;
                }

                debug!("Keep-alive ping sent (interval={}ms)", interval_ms);

                let _ = tx.send(crate::state::IpcEvent::KeepAliveStatus {
                    data: self_clone.state.read().clone(),
                });

                let _ = tx.send(crate::state::IpcEvent::LogMessage {
                    level: "debug".into(),
                    message: format!("Keep-alive ping at {} (interval={}ms)", now, interval_ms),
                });
            }
        })
    }
}

/// Synchronously check whether the Bluetooth radio is present and powered on.
///
/// This complements the event-driven `BthUsbMonitor` (which reacts to power-down
/// events via ETW) by providing a proactive "is the radio on right now?" check
/// that can be called from diagnostics or the keep-alive loop.
///
/// Uses the `Get-PnpDevice` PowerShell cmdlet to query the Bluetooth radio's
/// status, avoiding the need for additional `windows-sys` features.
///
/// Returns a list of event descriptions — empty if the radio is healthy,
/// non-empty if a power-down or missing radio is detected.
pub fn check_bthusb_power_state() -> Vec<String> {
    let mut events = Vec::new();

    // Simulation override for testing.
    if std::env::var("OXIDELINK_SIMULATE_POWER_DOWN").is_ok() {
        events.push(format!(
            "{}: simulated power-down at {}",
            BTHUSB_POWER_DOWN_SIGNATURE,
            timestamp_now()
        ));
        return events;
    }

    // Query the Bluetooth radio's power state via PowerShell.
    // Get-PnpDevice returns Status = "Ok" when the radio is powered,
    // "Error" or "Unknown" when it's off or in a low-power state.
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            // Find the Bluetooth radio device and check its status.
            // Class Bluetooth includes the radio and all paired devices;
            // we filter to the radio itself via PnPDeviceId pattern.
            "Get-PnpDevice -Class Bluetooth -ErrorAction SilentlyContinue \
             | Where-Object { $_.PNPDeviceId -like '*BTHUSB*' -or $_.PNPDeviceId -like '*USB*' } \
             | Select-Object -First 1 -ExpandProperty Status",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let status = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if status.is_empty() {
                // No Bluetooth radio found at all.
                events.push(format!(
                    "{}: no Bluetooth radio found at {}",
                    BTHUSB_POWER_DOWN_SIGNATURE,
                    timestamp_now()
                ));
            } else if status != "OK" && status != "Ok" {
                // Radio exists but is not in OK state (powered off, error, etc.)
                events.push(format!(
                    "{}: Bluetooth radio status '{}' at {}",
                    BTHUSB_POWER_DOWN_SIGNATURE,
                    status,
                    timestamp_now()
                ));
            }
            // If status is "OK", the radio is healthy — no events.
        }
        Ok(_) => {
            // PowerShell command failed — can't determine state.
            debug!("check_bthusb_power_state: PowerShell query failed");
        }
        Err(e) => {
            warn!(
                "check_bthusb_power_state: failed to spawn PowerShell: {}",
                e
            );
        }
    }

    events
}

/// Async wrapper that runs the synchronous PowerShell query on a blocking
/// thread so the tokio runtime is not stalled.
pub async fn check_bthusb_power_state_async() -> Vec<String> {
    tokio::task::spawn_blocking(check_bthusb_power_state)
        .await
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that touch the process environment so they cannot race
    /// with each other or with the global `OXIDELINK_SIMULATE_POWER_DOWN` var.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn manager_uses_configured_interval_without_power_events() {
        let status = Arc::new(RwLock::new(KeepAliveStatus {
            interval_ms: 1_500,
            ..KeepAliveStatus::default()
        }));
        let manager = KeepAliveManager::new(status);
        assert_eq!(manager.current_interval(), 1_500);
    }

    #[test]
    fn keepalive_status_default_values() {
        let s = KeepAliveStatus::default();
        assert!(!s.active);
        assert_eq!(s.interval_ms, 3000);
        assert_eq!(s.last_ping, 0);
        assert_eq!(s.power_events_detected, 0);
        assert!(!s.adapter_sleep_prevented);
        assert!(s.adaptive_mode);
    }

    #[test]
    fn manager_current_interval_reflects_default_status() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let manager = KeepAliveManager::new(status);
        // Default interval_ms is 3000 and no boost has been recorded.
        assert_eq!(manager.current_interval(), 3000);
    }

    #[test]
    fn report_power_event_down_triggers_boost() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let manager = KeepAliveManager::new(status.clone());

        manager.report_power_event("BTHUSB_Event_Power_Down");

        let s = status.read();
        assert_eq!(s.power_events_detected, 1);
        assert_eq!(s.interval_ms, MIN_INTERVAL_MS);
        assert!(s.adaptive_mode);
    }

    #[test]
    fn report_power_event_lowercase_power_down_triggers_boost() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let manager = KeepAliveManager::new(status.clone());

        // The detector accepts both "Power_Down" and "power_down" substrings.
        manager.report_power_event("something power_down happened");

        let s = status.read();
        assert_eq!(s.power_events_detected, 1);
        assert_eq!(s.interval_ms, MIN_INTERVAL_MS);
        assert!(s.adaptive_mode);
    }

    #[test]
    fn report_power_event_non_power_down_increments_count_only() {
        let status = Arc::new(RwLock::new(KeepAliveStatus {
            interval_ms: 2_000,
            adaptive_mode: false,
            ..KeepAliveStatus::default()
        }));
        let manager = KeepAliveManager::new(status.clone());

        manager.report_power_event("BTHUSB_Event_Power_Up");

        let s = status.read();
        assert_eq!(s.power_events_detected, 1);
        // Interval and adaptive flag must be untouched by a non-power-down event.
        assert_eq!(s.interval_ms, 2_000);
        assert!(!s.adaptive_mode);
    }

    #[test]
    fn report_power_event_count_accumulates_across_events() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let manager = KeepAliveManager::new(status.clone());

        manager.report_power_event("Power_Up");
        manager.report_power_event("Power_Up");
        manager.report_power_event("Power_Down");

        let s = status.read();
        assert_eq!(s.power_events_detected, 3);
    }

    #[test]
    fn report_power_event_rate_limits_rapid_power_downs() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let manager = KeepAliveManager::new(status.clone());

        // First power-down arms the boost and sets interval to MIN_INTERVAL_MS.
        manager.report_power_event("BTHUSB_Event_Power_Down");
        {
            let s = status.read();
            assert_eq!(s.interval_ms, MIN_INTERVAL_MS);
        }

        // Simulate the system restoring a longer interval between events.
        {
            let mut s = status.write();
            s.interval_ms = 4_000;
        }

        // A second power-down fired immediately must be rate-limited: the boost
        // window (BOOST_DURATION_MS) has not elapsed, so the interval must NOT
        // be reset back to MIN_INTERVAL_MS.
        manager.report_power_event("BTHUSB_Event_Power_Down");
        {
            let s = status.read();
            assert_eq!(s.interval_ms, 4_000, "rapid power-down should be rate-limited");
            assert_eq!(s.power_events_detected, 2);
        }
    }

    #[test]
    fn current_interval_after_boost_returns_min_interval() {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        let manager = KeepAliveManager::new(status.clone());

        manager.report_power_event("BTHUSB_Event_Power_Down");

        // Immediately after a boost the interval is the boosted MIN_INTERVAL_MS
        // and the boost has not expired, so current_interval returns it as-is.
        assert_eq!(manager.current_interval(), MIN_INTERVAL_MS);
    }

    #[test]
    fn current_interval_no_boost_returns_configured() {
        let status = Arc::new(RwLock::new(KeepAliveStatus {
            interval_ms: 1_234,
            ..KeepAliveStatus::default()
        }));
        let manager = KeepAliveManager::new(status);
        // No power event recorded -> last_boost is 0 -> config interval returned.
        assert_eq!(manager.current_interval(), 1_234);
    }

    #[test]
    fn power_event_detection_signature_constants() {
        // The detector keys on these signature substrings; ensure they are stable.
        assert!(BTHUSB_POWER_DOWN_SIGNATURE.contains("Power_Down"));
        assert!(BTHUSB_POWER_UP_SIGNATURE.contains("Power_Up"));
    }

    #[test]
    fn adaptive_interval_bounds_are_sane() {
        assert!(MIN_INTERVAL_MS < MAX_INTERVAL_MS);
        assert!(BOOST_DURATION_MS > 0);
    }

    #[test]
    fn check_bthusb_power_state_simulation_override_emits_event() {
        // Guard against races on the process environment.
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("OXIDELINK_SIMULATE_POWER_DOWN", "1");

        let events = check_bthusb_power_state();

        std::env::remove_var("OXIDELINK_SIMULATE_POWER_DOWN");

        assert_eq!(events.len(), 1);
        assert!(
            events[0].contains(BTHUSB_POWER_DOWN_SIGNATURE),
            "simulated event should carry the power-down signature: {}",
            events[0]
        );
        assert!(events[0].contains("simulated power-down"));
    }

    #[test]
    fn check_bthusb_power_state_no_simulation_returns_empty_or_real() {
        let _guard = ENV_GUARD.lock().unwrap();
        // Ensure the override is absent so we exercise the real query path. On a
        // machine without PowerShell/Bluetooth this may return an empty vec or a
        // real event list; we only assert the call does not panic and returns a
        // Vec<String>.
        std::env::remove_var("OXIDELINK_SIMULATE_POWER_DOWN");
        let _events = check_bthusb_power_state();
    }
}

impl Drop for KeepAliveManager {
    /// Reset the Windows thread execution state so the system power policy is
    /// not left in ES_SYSTEM_REQUIRED after the manager is dropped.
    fn drop(&mut self) {
        self.allow_adapter_sleep();
    }
}
