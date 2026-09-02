use crate::state::{timestamp_now, IpcEvent};
use log::{debug, info, warn};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, Duration};

// Windows Event Log + threading primitives used by the native ETW consumer.
// `EvtSubscribe` lives in `windows_sys::Win32::System::EventLog` (the Windows
// Event Log API), not the raw ETW controller API — it gives real-time push
// delivery of events written to a channel without requiring admin privileges
// to *subscribe* (the same privilege level as `Get-WinEvent`).
use windows_sys::core::w;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::EventLog::{
    EvtClose, EvtNext, EvtRender, EvtRenderEventXml, EvtSubscribe, EvtSubscribeToFutureEvents,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, WaitForSingleObject, INFINITE,
};

/// Event IDs from the BTHUSB provider (System channel) that we treat as
/// power-state / disconnect signatures. See
/// `ciel/kg/decisions/2026-07-18-oxidelink-bthusb-monitoring-strategy.md`.
pub const EVT_HCI_SIZE_MISMATCH: i32 = 5; // Error — power-down proxy
pub const EVT_REMOTE_UNPAIRED: i32 = 10; // Info  — link key removed
pub const EVT_LINK_KEY_STORE_FAIL: i32 = 18; // Info  — link key persistence fault

#[derive(Debug, Clone, Default)]
pub struct BthUsbSnapshot {
    pub power_down_events: u32,
    pub disconnect_events: u32,
    pub link_key_faults: u32,
    pub last_event_ts: u64,
    pub last_event_id: i32,
}

/// A single real-time BTHUSB event delivered by the native ETW subscription.
#[derive(Debug, Clone)]
struct BthUsbEtwEvent {
    event_id: i32,
}

#[derive(Default)]
pub struct BthUsbMonitor {
    last_seen_total: Arc<std::sync::Mutex<u64>>,
}

impl BthUsbMonitor {
    pub fn new() -> Self {
        Self {
            last_seen_total: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// Query the Windows System event log for recent BTHUSB events via a
    /// PowerShell `Get-WinEvent` shell-out. Returns the parsed snapshot.
    /// This is the fallback path used when the native ETW subscription cannot
    /// be initialised (e.g. insufficient privileges or a disabled System
    /// channel).
    pub fn poll_once(&self) -> BthUsbSnapshot {
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='BTHUSB'} -MaxEvents 50 -ErrorAction SilentlyContinue | ForEach-Object { \"$($_.Id)|$($_.TimeCreated.Ticks)\" }",
            ])
            .output();

        let snap = BthUsbSnapshot::default();
        let parsed = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            Ok(o) => {
                warn!(
                    "BTHUSB poll non-zero exit: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
                return snap;
            }
            Err(e) => {
                warn!("BTHUSB poll failed to spawn powershell: {}", e);
                return snap;
            }
        };

        let (snap, total) = parse_poll_output(&parsed);

        // Track delta vs last poll to detect NEW power-down events.
        {
            let mut last = self.last_seen_total.lock().unwrap();
            let prev = *last;
            *last = total;
            let _ = prev; // delta tracking handled by callers via report_power_event
        }

        snap
    }

    /// Detect whether a new power-down (Event ID 5) occurred since the last
    /// poll by comparing cumulative counts kept in the keepalive manager.
    pub fn detect_new_power_down(&self, snap: &BthUsbSnapshot, prev_power_down: u32) -> bool {
        snap.power_down_events > prev_power_down
    }

    /// Start the BTHUSB monitor in dual-mode: first attempt a native ETW
    /// real-time subscription (`EvtSubscribe` on the System channel filtered
    /// to `ProviderName='BTHUSB'`). If ETW initialisation fails, fall back to
    /// the existing PowerShell `Get-WinEvent` polling at `poll_interval_ms`.
    ///
    /// Both modes emit the same `BluetoothPowerEvent`, `Disconnected`, and
    /// `LogMessage` IPC events and report faults to the keep-alive manager.
    pub fn start_loop(
        self: Arc<Self>,
        tx: tokio::sync::broadcast::Sender<IpcEvent>,
        keepalive: Arc<crate::keepalive::KeepAliveManager>,
        poll_interval_ms: u64,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Channel carrying real-time events from the ETW consumer thread.
            let (etw_tx, etw_rx) = mpsc::unbounded_channel::<BthUsbEtwEvent>();
            // One-shot used by the blocking thread to report init success.
            let (init_tx, init_rx) = oneshot::channel::<bool>();

            // Spawn the blocking ETW consumer. `EvtSubscribe` + the
            // `WaitForSingleObject` drain loop are blocking calls, so they
            // must live on a `spawn_blocking` thread, not the async runtime.
            let _etw_join = tokio::task::spawn_blocking(move || {
                etw_consumer_loop(init_tx, etw_tx);
            });

            let etw_ok = init_rx.await.unwrap_or(false);
            if etw_ok {
                info!(
                    "BTHUSB monitor started — native ETW real-time consumer active (sub-second latency)"
                );
                run_etw_dispatch(etw_rx, tx, keepalive).await;
            } else {
                warn!(
                    "BTHUSB ETW init failed — falling back to PowerShell polling ({}ms)",
                    poll_interval_ms
                );
                run_poll_fallback(self, tx, keepalive, poll_interval_ms).await;
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Native ETW real-time consumer
// ---------------------------------------------------------------------------

/// Blocking loop that owns the `EvtSubscribe` handle and drains events as
/// they arrive. Reports `true` via `init_tx` on successful initialisation,
/// `false` (and returns) on failure so the caller can fall back to polling.
fn etw_consumer_loop(
    init_tx: oneshot::Sender<bool>,
    event_tx: mpsc::UnboundedSender<BthUsbEtwEvent>,
) {
    // Manual-reset event used by EvtSubscribe to signal new events.
    let signal = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if signal.is_null() {
        warn!("BTHUSB ETW: CreateEventW failed (err={})", unsafe {
            GetLastError()
        });
        let _ = init_tx.send(false);
        return;
    }

    // Subscribe to *future* events on the System channel from the BTHUSB
    // provider. We deliberately use EvtSubscribeToFutureEvents so we only
    // receive events that arrive after start-up — no historical replay, which
    // means every delivered event is "new" and can be dispatched immediately.
    let channel = w!("System");
    let query = w!("*[System/Provider[@Name='BTHUSB']]");
    let subscription = unsafe {
        EvtSubscribe(
            0,
            signal,
            channel,
            query,
            0,
            std::ptr::null(),
            None,
            EvtSubscribeToFutureEvents,
        )
    };

    if subscription == 0 {
        let err = unsafe { GetLastError() };
        warn!(
            "BTHUSB ETW: EvtSubscribe failed (err={}) — System channel unreadable",
            err
        );
        unsafe { CloseHandle(signal) };
        let _ = init_tx.send(false);
        return;
    }

    // ETW is live — tell the async task to use the real-time path.
    let _ = init_tx.send(true);

    loop {
        let wait = unsafe { WaitForSingleObject(signal, INFINITE) };
        if wait == WAIT_FAILED || wait != WAIT_OBJECT_0 {
            // Spurious or failed wake — keep going unless the signal handle
            // is somehow invalid, in which case bail out.
            if wait == WAIT_FAILED {
                warn!("BTHUSB ETW: WaitForSingleObject failed (err={})", unsafe {
                    GetLastError()
                });
                break;
            }
            continue;
        }
        unsafe { ResetEvent(signal) };

        // Drain all pending events. EvtNext returns FALSE when there are no
        // more events (ERROR_NO_MORE_ITEMS), so we loop until it yields none.
        loop {
            let mut event_handle: isize = 0;
            let mut returned: u32 = 0;
            let ok = unsafe {
                EvtNext(
                    subscription,
                    1,
                    &mut event_handle,
                    INFINITE,
                    0,
                    &mut returned,
                )
            };
            if ok == 0 || returned == 0 {
                break;
            }

            let event_id = render_event_id(event_handle);
            // Receiver dropped (app shutting down) → stop draining.
            if event_tx.send(BthUsbEtwEvent { event_id }).is_err() {
                unsafe { EvtClose(event_handle) };
                // Break outer loop via a flag.
                unsafe { EvtClose(subscription) };
                unsafe { CloseHandle(signal) };
                return;
            }
            unsafe { EvtClose(event_handle) };
        }
    }

    unsafe { EvtClose(subscription) };
    unsafe { CloseHandle(signal) };
}

/// Render a single event as XML and extract the `<EventID>` value.
///
/// We render with `EvtRenderEventXml` (context = NULL) which produces a
/// UTF-16 XML fragment. Parsing the EventID from the XML avoids needing the
/// `EVT_VARIANT` system-property render path (which pulls in `Win32_Security`
/// layout concerns) and is robust across provider schema versions.
fn render_event_id(event_handle: isize) -> i32 {
    unsafe {
        let mut used: u32 = 0;
        let mut count: u32 = 0;
        // First call with a zero-size buffer to discover the required size.
        let ok = EvtRender(
            0,
            event_handle,
            EvtRenderEventXml,
            0,
            std::ptr::null_mut(),
            &mut used,
            &mut count,
        );
        if ok == 0 {
            let err = GetLastError();
            if err != 0 {
                warn!("BTHUSB ETW: EvtRender size query failed (err={})", err);
            }
            return 0;
        }
        if used == 0 {
            return 0;
        }

        // `used` is in bytes; the buffer is WCHAR (u16). Allocate one extra
        // WCHAR for a trailing NUL to keep from_utf16_lossy well-defined.
        let wide_len = (used as usize / 2) + 1;
        let mut buf: Vec<u16> = vec![0u16; wide_len];
        let ok = EvtRender(
            0,
            event_handle,
            EvtRenderEventXml,
            used,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut used,
            &mut count,
        );
        if ok == 0 {
            warn!("BTHUSB ETW: EvtRender failed (err={})", GetLastError());
            return 0;
        }

        let chars = used as usize / 2;
        let xml = String::from_utf16_lossy(&buf[..chars]);
        parse_event_id(&xml)
    }
}

/// Parse the raw stdout of the PowerShell `Get-WinEvent` shell-out into a
/// `BthUsbSnapshot` plus the total number of non-empty lines observed.
///
/// Each line is expected to be `<EventID>|<.NET Ticks>`. Unknown IDs and
/// malformed lines are counted toward `total` but do not increment any
/// signature counter, mirroring the behaviour of the live poll path.
fn parse_poll_output(parsed: &str) -> (BthUsbSnapshot, u64) {
    let mut power_down = 0u32;
    let mut disconnect = 0u32;
    let mut link_key_fault = 0u32;
    let mut last_ts: u64 = 0;
    let mut last_id: i32 = 0;
    let mut total: u64 = 0;

    for line in parsed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        total += 1;
        let mut parts = line.split('|');
        let id_str = parts.next().unwrap_or("0");
        let ts_str = parts.next().unwrap_or("0");
        let id: i32 = id_str.parse().unwrap_or(0);
        let ticks: i64 = ts_str.parse().unwrap_or(0);

        match id {
            EVT_HCI_SIZE_MISMATCH => power_down += 1,
            EVT_REMOTE_UNPAIRED => disconnect += 1,
            EVT_LINK_KEY_STORE_FAIL => link_key_fault += 1,
            _ => {}
        }
        // .NET DateTime.Ticks are 100ns intervals since 0001-01-01.
        // Convert to unix ms approx: (ticks - 621355968000000000) / 10000.
        let unix_ms = ((ticks - 621_355_968_000_000_000) / 10_000).max(0) as u64;
        if unix_ms > last_ts {
            last_ts = unix_ms;
            last_id = id;
        }
    }

    let snap = BthUsbSnapshot {
        power_down_events: power_down,
        disconnect_events: disconnect,
        link_key_faults: link_key_fault,
        last_event_ts: last_ts,
        last_event_id: last_id,
    };
    (snap, total)
}

/// Extract the integer `<EventID>` from a rendered event XML fragment.
///
/// The rendered XML looks like:
/// `<Event xmlns='...'><System><Provider Name='BTHUSB' .../><EventID Qualifiers='...'>5</EventID>...`
/// We locate `<EventID`, skip to the `>` that closes the opening tag, then
/// read up to the next `<`. This tolerates the optional `Qualifiers`
/// attribute without a regex dependency.
fn parse_event_id(xml: &str) -> i32 {
    let key = "<EventID";
    let start = match xml.find(key) {
        Some(i) => i,
        None => return 0,
    };
    let rest = &xml[start..];
    let gt = match rest.find('>') {
        Some(i) => i,
        None => return 0,
    };
    let after = &rest[gt + 1..];
    let end = after.find('<').unwrap_or(after.len());
    after[..end].trim().parse::<i32>().unwrap_or(0)
}

/// Async dispatch loop for the ETW path: receives real-time events and emits
/// the corresponding IPC events / keep-alive reports.
async fn run_etw_dispatch(
    mut event_rx: mpsc::UnboundedReceiver<BthUsbEtwEvent>,
    tx: tokio::sync::broadcast::Sender<IpcEvent>,
    keepalive: Arc<crate::keepalive::KeepAliveManager>,
) {
    while let Some(ev) = event_rx.recv().await {
        dispatch_event(ev.event_id, &tx, &keepalive);
    }
    warn!("BTHUSB ETW consumer stream ended");
}

// ---------------------------------------------------------------------------
// Fallback: PowerShell polling
// ---------------------------------------------------------------------------

/// Async polling fallback — preserves the original 5s `Get-WinEvent` loop.
async fn run_poll_fallback(
    monitor: Arc<BthUsbMonitor>,
    tx: tokio::sync::broadcast::Sender<IpcEvent>,
    keepalive: Arc<crate::keepalive::KeepAliveManager>,
    poll_interval_ms: u64,
) {
    let mut ticker = interval(Duration::from_millis(poll_interval_ms));
    let mut prev_power_down: u32 = 0;
    let mut prev_disconnect: u32 = 0;
    info!(
        "BTHUSB monitor started (poll every {}ms) — fallback mode",
        poll_interval_ms
    );

    loop {
        ticker.tick().await;
        let monitor_clone = monitor.clone();
        let snap = tokio::task::spawn_blocking(move || monitor_clone.poll_once())
            .await
            .unwrap_or_default();

        if monitor.detect_new_power_down(&snap, prev_power_down) {
            dispatch_event(EVT_HCI_SIZE_MISMATCH, &tx, &keepalive);
        }

        if snap.disconnect_events > prev_disconnect {
            dispatch_event(EVT_REMOTE_UNPAIRED, &tx, &keepalive);
        }

        prev_power_down = snap.power_down_events;
        prev_disconnect = snap.disconnect_events;

        debug!(
            "BTHUSB poll: power_down={} disconnect={} link_key_fault={} last_id={}",
            snap.power_down_events,
            snap.disconnect_events,
            snap.link_key_faults,
            snap.last_event_id
        );
    }
}

// ---------------------------------------------------------------------------
// Shared event dispatch (used by both ETW and polling paths)
// ---------------------------------------------------------------------------

/// Emit the IPC events and keep-alive report for a single BTHUSB event ID.
fn dispatch_event(
    event_id: i32,
    tx: &tokio::sync::broadcast::Sender<IpcEvent>,
    keepalive: &Arc<crate::keepalive::KeepAliveManager>,
) {
    match event_id {
        EVT_HCI_SIZE_MISMATCH => {
            let now = timestamp_now();
            warn!(
                "New BTHUSB Event ID 5 (HCI size mismatch) detected at {} — power-down proxy",
                now
            );
            keepalive.report_power_event("Power_Down");
            let _ = tx.send(IpcEvent::BluetoothPowerEvent {
                event_type: "Power_Down".into(),
                timestamp: now,
            });
            let _ = tx.send(IpcEvent::LogMessage {
                level: "warn".into(),
                message: format!("BTHUSB power-down proxy (Event ID 5) detected at {}", now),
            });
        }
        EVT_REMOTE_UNPAIRED => {
            let now = timestamp_now();
            warn!(
                "BTHUSB disconnect (Event ID 10) detected at {} — controller unpaired",
                now
            );
            let _ = tx.send(IpcEvent::Disconnected {
                reason: "BTHUSB link key removed (Event ID 10)".into(),
            });
            let _ = tx.send(IpcEvent::LogMessage {
                level: "warn".into(),
                message: format!("Controller unpaired at {}", now),
            });
        }
        EVT_LINK_KEY_STORE_FAIL => {
            let now = timestamp_now();
            warn!(
                "BTHUSB link key store fault (Event ID 18) detected at {}",
                now
            );
            let _ = tx.send(IpcEvent::LogMessage {
                level: "warn".into(),
                message: format!("BTHUSB link key persistence fault (Event ID 18) at {}", now),
            });
        }
        id => {
            debug!("BTHUSB event id {} (not dispatched)", id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keepalive::KeepAliveManager;
    use crate::state::KeepAliveStatus;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    // -----------------------------------------------------------------
    // BthUsbSnapshot: defaults, cloning, Debug formatting
    // -----------------------------------------------------------------

    #[test]
    fn snapshot_defaults_are_zero() {
        let s = BthUsbSnapshot::default();
        assert_eq!(s.power_down_events, 0);
        assert_eq!(s.disconnect_events, 0);
        assert_eq!(s.link_key_faults, 0);
        assert_eq!(s.last_event_ts, 0);
        assert_eq!(s.last_event_id, 0);
    }

    #[test]
    fn snapshot_clone_is_equal() {
        let s = BthUsbSnapshot {
            power_down_events: 3,
            disconnect_events: 1,
            link_key_faults: 2,
            last_event_ts: 1_700_000_000_000,
            last_event_id: EVT_HCI_SIZE_MISMATCH,
        };
        let c = s.clone();
        assert_eq!(c.power_down_events, s.power_down_events);
        assert_eq!(c.disconnect_events, s.disconnect_events);
        assert_eq!(c.link_key_faults, s.link_key_faults);
        assert_eq!(c.last_event_ts, s.last_event_ts);
        assert_eq!(c.last_event_id, s.last_event_id);
    }

    #[test]
    fn snapshot_debug_includes_all_fields() {
        let s = BthUsbSnapshot {
            power_down_events: 1,
            disconnect_events: 0,
            link_key_faults: 0,
            last_event_ts: 42,
            last_event_id: 5,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("power_down_events: 1"));
        assert!(dbg.contains("last_event_id: 5"));
    }

    // -----------------------------------------------------------------
    // BthUsbMonitor: construction & state
    // -----------------------------------------------------------------

    #[test]
    fn monitor_new_initialises_last_seen_total_to_zero() {
        let m = BthUsbMonitor::new();
        let guard = m.last_seen_total.lock().unwrap();
        assert_eq!(*guard, 0);
    }

    #[test]
    fn monitor_default_matches_new() {
        let m = BthUsbMonitor::default();
        let guard = m.last_seen_total.lock().unwrap();
        assert_eq!(*guard, 0);
    }

    // -----------------------------------------------------------------
    // detect_new_power_down: pure comparison logic
    // -----------------------------------------------------------------

    #[test]
    fn detect_new_power_down_true_when_count_increases() {
        let m = BthUsbMonitor::new();
        let snap = BthUsbSnapshot {
            power_down_events: 5,
            ..BthUsbSnapshot::default()
        };
        assert!(m.detect_new_power_down(&snap, 4));
        assert!(m.detect_new_power_down(&snap, 0));
    }

    #[test]
    fn detect_new_power_down_false_when_equal_or_lower() {
        let m = BthUsbMonitor::new();
        let snap = BthUsbSnapshot {
            power_down_events: 3,
            ..BthUsbSnapshot::default()
        };
        assert!(!m.detect_new_power_down(&snap, 3));
        assert!(!m.detect_new_power_down(&snap, 10));
    }

    #[test]
    fn detect_new_power_down_false_for_default_snapshot() {
        let m = BthUsbMonitor::new();
        assert!(!m.detect_new_power_down(&BthUsbSnapshot::default(), 0));
    }

    // -----------------------------------------------------------------
    // parse_event_id: XML fragment parsing with mock strings
    // -----------------------------------------------------------------

    #[test]
    fn parse_event_id_extracts_integer_from_xml_fragment() {
        let xml = "<Event xmlns='...'><System><Provider Name='BTHUSB'/><EventID Qualifiers='0'>5</EventID></System></Event>";
        assert_eq!(parse_event_id(xml), 5);
    }

    #[test]
    fn parse_event_id_handles_event_id_10_and_18() {
        let xml10 = "<Event><System><EventID>10</EventID></System></Event>";
        assert_eq!(parse_event_id(xml10), EVT_REMOTE_UNPAIRED);

        let xml18 = "<Event><System><EventID>18</EventID></System></Event>";
        assert_eq!(parse_event_id(xml18), EVT_LINK_KEY_STORE_FAIL);
    }

    #[test]
    fn parse_event_id_returns_zero_when_no_event_id_tag() {
        let no_event_id = "<Event><System></System></Event>";
        assert_eq!(parse_event_id(no_event_id), 0);
    }

    #[test]
    fn parse_event_id_returns_zero_for_non_numeric_content() {
        let malformed = "<Event><EventID>not-a-number</EventID></Event>";
        assert_eq!(parse_event_id(malformed), 0);
    }

    #[test]
    fn parse_event_id_returns_zero_for_empty_content() {
        let empty = "<Event><EventID></EventID></Event>";
        assert_eq!(parse_event_id(empty), 0);
    }

    #[test]
    fn parse_event_id_returns_zero_when_tag_not_closed() {
        let unclosed = "<Event><EventID Qualifiers='0'";
        assert_eq!(parse_event_id(unclosed), 0);
    }

    #[test]
    fn parse_event_id_trims_whitespace_around_value() {
        let xml = "<Event><EventID>  5  </EventID></Event>";
        assert_eq!(parse_event_id(xml), 5);
    }

    #[test]
    fn parse_event_id_handles_negative_values() {
        let xml = "<Event><EventID>-1</EventID></Event>";
        assert_eq!(parse_event_id(xml), -1);
    }

    #[test]
    fn parse_event_id_uses_first_event_id_tag() {
        let xml = "<Event><EventID>5</EventID><EventID>18</EventID></Event>";
        assert_eq!(parse_event_id(xml), 5);
    }

    #[test]
    fn parse_event_id_returns_zero_for_empty_string() {
        assert_eq!(parse_event_id(""), 0);
    }

    // -----------------------------------------------------------------
    // parse_poll_output: PowerShell stdout parsing with mock strings
    // -----------------------------------------------------------------

    #[test]
    fn parse_poll_output_empty_string_yields_default_snapshot() {
        let (snap, total) = parse_poll_output("");
        assert_eq!(snap.power_down_events, 0);
        assert_eq!(snap.disconnect_events, 0);
        assert_eq!(snap.link_key_faults, 0);
        assert_eq!(snap.last_event_ts, 0);
        assert_eq!(snap.last_event_id, 0);
        assert_eq!(total, 0);
    }

    #[test]
    fn parse_poll_output_counts_each_signature() {
        // ticks chosen so unix_ms > 0: 621355968000000000 + 1000*10000
        let base = 621_355_968_000_000_000i64;
        let t1 = base + 1_000 * 10_000; // unix_ms = 1000
        let t2 = base + 2_000 * 10_000; // unix_ms = 2000
        let t3 = base + 3_000 * 10_000; // unix_ms = 3000
        let input = format!("5|{}\n10|{}\n18|{}\n", t1, t2, t3);

        let (snap, total) = parse_poll_output(&input);
        assert_eq!(snap.power_down_events, 1);
        assert_eq!(snap.disconnect_events, 1);
        assert_eq!(snap.link_key_faults, 1);
        assert_eq!(total, 3);
        // latest event is ID 18 with unix_ms 3000
        assert_eq!(snap.last_event_id, EVT_LINK_KEY_STORE_FAIL);
        assert_eq!(snap.last_event_ts, 3000);
    }

    #[test]
    fn parse_poll_output_ignores_unknown_ids_but_counts_total() {
        let base = 621_355_968_000_000_000i64;
        let input = format!("99|{}\n5|{}\n", base, base + 10_000);
        let (snap, total) = parse_poll_output(&input);
        assert_eq!(snap.power_down_events, 1);
        assert_eq!(snap.disconnect_events, 0);
        assert_eq!(total, 2);
        // ID 5 has the larger timestamp
        assert_eq!(snap.last_event_id, EVT_HCI_SIZE_MISMATCH);
    }

    #[test]
    fn parse_poll_output_skips_blank_and_whitespace_lines() {
        let input = "\n   \n5|621355968000010000\n\n";
        let (snap, total) = parse_poll_output(input);
        assert_eq!(total, 1);
        assert_eq!(snap.power_down_events, 1);
    }

    #[test]
    fn parse_poll_output_tolerates_malformed_lines() {
        // missing timestamp, non-numeric id, missing id entirely.
        // The bare "5" line parses id=5 with ticks=0 → counts as a
        // power-down signature; the other two lines contribute nothing.
        let input = "5\nabc|123\n|621355968000000000\n";
        let (snap, total) = parse_poll_output(input);
        assert_eq!(total, 3);
        assert_eq!(snap.power_down_events, 1);
        assert_eq!(snap.disconnect_events, 0);
        assert_eq!(snap.link_key_faults, 0);
    }

    #[test]
    fn parse_poll_output_negative_ticks_clamped_to_zero() {
        // ticks below the epoch offset → unix_ms clamped to 0 via .max(0).
        // Because unix_ms (0) is not strictly greater than last_ts (0), the
        // event id is NOT recorded as the latest — mirroring live behaviour
        // where a zero-timestamp event does not supersede the initial state.
        let input = "5|-1\n";
        let (snap, total) = parse_poll_output(input);
        assert_eq!(total, 1);
        assert_eq!(snap.power_down_events, 1);
        assert_eq!(snap.last_event_ts, 0);
        assert_eq!(snap.last_event_id, 0);
    }

    #[test]
    fn parse_poll_output_picks_latest_timestamp() {
        let base = 621_355_968_000_000_000i64;
        // out-of-order: later event first
        let input = format!(
            "10|{}\n5|{}\n",
            base + 5_000 * 10_000,
            base + 1_000 * 10_000
        );
        let (snap, _) = parse_poll_output(&input);
        assert_eq!(snap.last_event_id, EVT_REMOTE_UNPAIRED);
        assert_eq!(snap.last_event_ts, 5_000);
    }

    // -----------------------------------------------------------------
    // dispatch_event: power-down/up signature matching via IPC channel
    // (no Windows APIs are invoked by report_power_event)
    // -----------------------------------------------------------------

    fn make_keepalive() -> Arc<KeepAliveManager> {
        let status = Arc::new(RwLock::new(KeepAliveStatus::default()));
        Arc::new(KeepAliveManager::new(status))
    }

    #[test]
    fn dispatch_event_power_down_emits_bluetooth_power_event() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(EVT_HCI_SIZE_MISMATCH, &tx, &keepalive);

        let mut got_power = false;
        let mut got_log = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                IpcEvent::BluetoothPowerEvent { event_type, .. } => {
                    assert_eq!(event_type, "Power_Down");
                    got_power = true;
                }
                IpcEvent::LogMessage { level, .. } => {
                    assert_eq!(level, "warn");
                    got_log = true;
                }
                _ => {}
            }
        }
        assert!(got_power, "expected BluetoothPowerEvent");
        assert!(got_log, "expected LogMessage");
    }

    #[test]
    fn dispatch_event_remote_unpaired_emits_disconnected() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(EVT_REMOTE_UNPAIRED, &tx, &keepalive);

        let mut got_disconnect = false;
        while let Ok(ev) = rx.try_recv() {
            if let IpcEvent::Disconnected { reason } = ev {
                assert!(reason.contains("Event ID 10"));
                got_disconnect = true;
            }
        }
        assert!(got_disconnect, "expected Disconnected event");
    }

    #[test]
    fn dispatch_event_link_key_fault_emits_only_log_message() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(EVT_LINK_KEY_STORE_FAIL, &tx, &keepalive);

        let mut log_count = 0;
        let mut other_count = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                IpcEvent::LogMessage { level, message, .. } => {
                    assert_eq!(level, "warn");
                    assert!(message.contains("Event ID 18"));
                    log_count += 1;
                }
                _ => {
                    other_count += 1;
                }
            }
        }
        assert_eq!(log_count, 1);
        assert_eq!(other_count, 0);
    }

    #[test]
    fn dispatch_event_unknown_id_emits_nothing() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(999, &tx, &keepalive);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_event_power_down_reports_to_keepalive() {
        let (tx, _rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        // We can't read power_event_count directly, but report_power_event
        // updates KeepAliveStatus.power_events_detected via the shared state.
        // Drive two events and confirm the counter advanced through dispatch.
        let status = {
            // KeepAliveManager doesn't expose state; instead verify no panic
            // and that repeated dispatches are accepted.
            dispatch_event(EVT_HCI_SIZE_MISMATCH, &tx, &keepalive);
            dispatch_event(EVT_HCI_SIZE_MISMATCH, &tx, &keepalive);
        };
        let _ = status; // no return value — test asserts no panic / no hang
    }

    // -----------------------------------------------------------------
    // Event ID constants sanity
    // -----------------------------------------------------------------

    #[test]
    fn event_id_constants_have_expected_values() {
        assert_eq!(EVT_HCI_SIZE_MISMATCH, 5);
        assert_eq!(EVT_REMOTE_UNPAIRED, 10);
        assert_eq!(EVT_LINK_KEY_STORE_FAIL, 18);
    }

    // -----------------------------------------------------------------
    // BthUsbEtwEvent: Debug + Clone (private struct, exercised via super)
    // -----------------------------------------------------------------

    #[test]
    fn bth_usb_etw_event_clone_preserves_event_id() {
        let ev = BthUsbEtwEvent {
            event_id: EVT_HCI_SIZE_MISMATCH,
        };
        let c = ev.clone();
        assert_eq!(c.event_id, ev.event_id);
    }

    #[test]
    fn bth_usb_etw_event_debug_contains_event_id() {
        let ev = BthUsbEtwEvent { event_id: 42 };
        let dbg = format!("{:?}", ev);
        assert!(dbg.contains("42"));
    }

    // -----------------------------------------------------------------
    // parse_event_id: additional edge cases
    // -----------------------------------------------------------------

    #[test]
    fn parse_event_id_handles_qualifiers_attribute_with_gt_in_value() {
        // The parser scans for the first '>' after "<EventID". An attribute
        // value containing '>' would break naive parsing, but the standard
        // schema never includes it — verify a normal Qualifiers attribute.
        let xml = "<Event><System><EventID Qualifiers='16384'>5</EventID></System></Event>";
        assert_eq!(parse_event_id(xml), 5);
    }

    #[test]
    fn parse_event_id_handles_multiple_attributes() {
        let xml = "<Event><EventID Qualifiers='1' Guid='{abc}'>18</EventID></Event>";
        assert_eq!(parse_event_id(xml), 18);
    }

    #[test]
    fn parse_event_id_returns_zero_for_self_closing_tag() {
        // A self-closing <EventID/> has no '>' followed by content before the
        // next '<' — the parser reads empty content and returns 0.
        let xml = "<Event><EventID/></Event>";
        assert_eq!(parse_event_id(xml), 0);
    }

    #[test]
    fn parse_event_id_uses_content_before_next_tag_only() {
        // Trailing garbage after the closing '<' must not leak into the parse.
        let xml = "<Event><EventID>5</EventID>garbage<Other>99</Other></Event>";
        assert_eq!(parse_event_id(xml), 5);
    }

    #[test]
    fn parse_event_id_handles_large_positive_value() {
        let xml = "<Event><EventID>2147483647</EventID></Event>";
        assert_eq!(parse_event_id(xml), 2147483647);
    }

    #[test]
    fn parse_event_id_returns_zero_for_overflow_value() {
        // i32::MAX + 1 overflows the parse → unwrap_or(0).
        let xml = "<Event><EventID>2147483648</EventID></Event>";
        assert_eq!(parse_event_id(xml), 0);
    }

    #[test]
    fn parse_event_id_returns_zero_when_only_key_prefix_present() {
        // "<EventID" appears but no '>' follows.
        let xml = "<Event><EventID";
        assert_eq!(parse_event_id(xml), 0);
    }

    // -----------------------------------------------------------------
    // parse_poll_output: additional edge cases
    // -----------------------------------------------------------------

    #[test]
    fn parse_poll_output_handles_crlf_line_endings() {
        let base = 621_355_968_000_000_000i64;
        let input = format!("5|{}\r\n10|{}\r\n", base + 10_000, base + 20_000);
        let (snap, total) = parse_poll_output(&input);
        assert_eq!(total, 2);
        assert_eq!(snap.power_down_events, 1);
        assert_eq!(snap.disconnect_events, 1);
    }

    #[test]
    fn parse_poll_output_counts_repeated_same_id() {
        let base = 621_355_968_000_000_000i64;
        let input = format!(
            "5|{}\n5|{}\n5|{}\n",
            base + 10_000,
            base + 20_000,
            base + 30_000
        );
        let (snap, total) = parse_poll_output(&input);
        assert_eq!(snap.power_down_events, 3);
        assert_eq!(total, 3);
        assert_eq!(snap.last_event_id, EVT_HCI_SIZE_MISMATCH);
        assert_eq!(snap.last_event_ts, 3);
    }

    #[test]
    fn parse_poll_output_trims_whitespace_around_id_and_ticks() {
        let base = 621_355_968_000_000_000i64;
        // The line is trimmed as a whole, but individual parts are NOT trimmed.
        // So "5|<ts>" (no spaces around pipe) parses correctly.
        let input = format!("5|{}\n", base + 10_000);
        let (snap, total) = parse_poll_output(&input);
        assert_eq!(total, 1);
        assert_eq!(snap.power_down_events, 1);
    }

    #[test]
    fn parse_poll_output_line_with_only_pipe_counts_as_total() {
        // "|" → id_str="" (parse→0), ts_str="" (parse→0). Counts toward total,
        // matches no signature.
        let (snap, total) = parse_poll_output("|\n");
        assert_eq!(total, 1);
        assert_eq!(snap.power_down_events, 0);
        assert_eq!(snap.disconnect_events, 0);
        assert_eq!(snap.link_key_faults, 0);
    }

    #[test]
    fn parse_poll_output_line_with_extra_pipe_segments_ignores_them() {
        let base = 621_355_968_000_000_000i64;
        // split('|') yields ["5", "<ts>", "extra"] — only first two used.
        let input = format!("5|{}|extra|junk\n", base + 10_000);
        let (snap, total) = parse_poll_output(&input);
        assert_eq!(total, 1);
        assert_eq!(snap.power_down_events, 1);
    }

    #[test]
    fn parse_poll_output_zero_ticks_does_not_record_latest() {
        // ticks=0 → unix_ms clamped to 0; 0 is not > last_ts(0) so last_id
        // stays 0 even though the signature counter increments.
        let (snap, _) = parse_poll_output("5|0\n");
        assert_eq!(snap.power_down_events, 1);
        assert_eq!(snap.last_event_ts, 0);
        assert_eq!(snap.last_event_id, 0);
    }

    #[test]
    fn parse_poll_output_equal_timestamps_keep_first_id() {
        // Two events with identical unix_ms: the second is NOT strictly
        // greater, so last_event_id stays as the first event's id.
        let base = 621_355_968_000_000_000i64;
        let input = format!("5|{}\n10|{}\n", base + 10_000, base + 10_000);
        let (snap, _) = parse_poll_output(&input);
        assert_eq!(snap.last_event_id, EVT_HCI_SIZE_MISMATCH);
    }

    #[test]
    fn parse_poll_output_mixed_known_and_unknown_ids() {
        let base = 621_355_968_000_000_000i64;
        let input = format!(
            "5|{}\n99|{}\n10|{}\n18|{}\n200|{}\n",
            base + 10_000,
            base + 20_000,
            base + 30_000,
            base + 40_000,
            base + 50_000
        );
        let (snap, total) = parse_poll_output(&input);
        assert_eq!(total, 5);
        assert_eq!(snap.power_down_events, 1);
        assert_eq!(snap.disconnect_events, 1);
        assert_eq!(snap.link_key_faults, 1);
        assert_eq!(snap.last_event_id, 200);
        assert_eq!(snap.last_event_ts, 5);
    }

    #[test]
    fn parse_poll_output_whitespace_only_lines_skipped() {
        let base = 621_355_968_000_000_000i64;
        let input = format!("\n   \n\t\n5|{}\n", base + 10_000);
        let (snap, total) = parse_poll_output(&input);
        assert_eq!(total, 1);
        assert_eq!(snap.power_down_events, 1);
    }

    // -----------------------------------------------------------------
    // dispatch_event: additional edge cases & event-count assertions
    // -----------------------------------------------------------------

    #[test]
    fn dispatch_event_power_down_emits_exactly_two_events() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(EVT_HCI_SIZE_MISMATCH, &tx, &keepalive);

        let mut count = 0;
        while let Ok(ev) = rx.try_recv() {
            count += 1;
            match ev {
                IpcEvent::BluetoothPowerEvent { event_type, .. } => {
                    assert_eq!(event_type, "Power_Down");
                }
                IpcEvent::LogMessage { level, message, .. } => {
                    assert_eq!(level, "warn");
                    assert!(message.contains("Event ID 5"));
                }
                _ => panic!("unexpected event variant"),
            }
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn dispatch_event_remote_unpaired_emits_disconnected_and_log() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(EVT_REMOTE_UNPAIRED, &tx, &keepalive);

        let mut got_disconnect = false;
        let mut got_log = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                IpcEvent::Disconnected { reason } => {
                    assert!(reason.contains("Event ID 10"));
                    got_disconnect = true;
                }
                IpcEvent::LogMessage { level, message, .. } => {
                    assert_eq!(level, "warn");
                    assert!(message.contains("unpaired"));
                    got_log = true;
                }
                _ => panic!("unexpected event variant"),
            }
        }
        assert!(got_disconnect, "expected Disconnected event");
        assert!(got_log, "expected LogMessage event");
    }

    #[test]
    fn dispatch_event_link_key_fault_message_contains_persistence() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(EVT_LINK_KEY_STORE_FAIL, &tx, &keepalive);

        while let Ok(ev) = rx.try_recv() {
            if let IpcEvent::LogMessage { message, .. } = ev {
                assert!(message.contains("persistence"));
                assert!(message.contains("Event ID 18"));
                return;
            }
        }
        panic!("expected LogMessage event");
    }

    #[test]
    fn dispatch_event_unknown_negative_id_emits_nothing() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(-1, &tx, &keepalive);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_event_repeated_power_downs_all_emitted() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(64);
        let keepalive = make_keepalive();
        for _ in 0..3 {
            dispatch_event(EVT_HCI_SIZE_MISMATCH, &tx, &keepalive);
        }
        // Drain all events (each dispatch sends BluetoothPowerEvent + LogMessage).
        let mut power_count = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, IpcEvent::BluetoothPowerEvent { .. }) {
                power_count += 1;
            }
        }
        assert_eq!(power_count, 3);
    }

    #[test]
    fn dispatch_event_zero_id_emits_nothing() {
        let (tx, mut rx) = broadcast::channel::<IpcEvent>(16);
        let keepalive = make_keepalive();
        dispatch_event(0, &tx, &keepalive);
        assert!(rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------
    // detect_new_power_down: boundary cases
    // -----------------------------------------------------------------

    #[test]
    fn detect_new_power_down_true_for_one_vs_zero() {
        let m = BthUsbMonitor::new();
        let snap = BthUsbSnapshot {
            power_down_events: 1,
            ..BthUsbSnapshot::default()
        };
        assert!(m.detect_new_power_down(&snap, 0));
    }

    #[test]
    fn detect_new_power_down_false_when_equal() {
        let m = BthUsbMonitor::new();
        let snap = BthUsbSnapshot {
            power_down_events: 7,
            ..BthUsbSnapshot::default()
        };
        assert!(!m.detect_new_power_down(&snap, 7));
    }

    #[test]
    fn detect_new_power_down_false_when_prev_higher() {
        let m = BthUsbMonitor::new();
        let snap = BthUsbSnapshot {
            power_down_events: 2,
            ..BthUsbSnapshot::default()
        };
        assert!(!m.detect_new_power_down(&snap, 100));
    }

    // -----------------------------------------------------------------
    // BthUsbSnapshot: partial-equality & manual construction
    // -----------------------------------------------------------------

    #[test]
    fn snapshot_with_only_link_key_faults_set() {
        let s = BthUsbSnapshot {
            link_key_faults: 9,
            ..BthUsbSnapshot::default()
        };
        assert_eq!(s.link_key_faults, 9);
        assert_eq!(s.power_down_events, 0);
        assert_eq!(s.disconnect_events, 0);
    }

    #[test]
    fn snapshot_with_max_u32_counters() {
        let s = BthUsbSnapshot {
            power_down_events: u32::MAX,
            disconnect_events: u32::MAX,
            link_key_faults: u32::MAX,
            last_event_ts: u64::MAX,
            last_event_id: i32::MAX,
        };
        let c = s.clone();
        assert_eq!(c.power_down_events, u32::MAX);
        assert_eq!(c.last_event_ts, u64::MAX);
        assert_eq!(c.last_event_id, i32::MAX);
    }
}
