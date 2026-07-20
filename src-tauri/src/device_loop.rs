//! Background device loop that opens the real Nintendo Switch Pro Controller
//! via `hidapi` and feeds raw HID reports into the telemetry pipeline.
//!
//! The blocking `hidapi` reads/writes run on a dedicated OS thread (spawned
//! via `tokio::task::spawn_blocking`). Reports are forwarded back to the
//! async world through an unbounded channel, then parsed and dispatched as
//! IPC events. Outbound subcommands (e.g. battery polling) are sent to the
//! blocking thread through a second channel so the single `HidDevice` handle
//! is the only thing touching the hardware.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hidapi::{BusType, HidApi};
use log::{debug, info, warn};
use parking_lot::Mutex as PlMutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::hid_parser::{
    battery_raw_to_percent, build_get_state_subcmd, build_set_report_mode_subcmd,
    build_zero_rumble, hex_string, NINTENDO_VID, PRO_CONTROLLER_PID, REPORT_ID_DEFAULT_BT,
    REPORT_ID_NFC_IR, REPORT_ID_STANDARD, REPORT_ID_SUBCMD_REPLY, REPORT_ID_USB_REPLY,
};
use crate::imu;
use crate::state::{
    flick_stick::RightStickMode, timestamp_now, AppCtx, ConnectionType, ControllerState, GyroMode,
    IpcEvent, SharedState, StickCalibration, ValidationConfig, CONTROLLER_SLOTS,
};
use crate::stick_cal;
use crate::subcmd::{self, SubcommandManager};
use crate::telemetry::TelemetryExtractor;
use crate::turbo::TurboEngine;
use tauri::State;

/// Buffer size for a single HID input report. The Pro Controller uses
/// 49-byte standard reports and 63-byte subcommand replies, so 64 bytes
/// is a safe upper bound.
const READ_BUF_SIZE: usize = 64;

/// `read_timeout` value (milliseconds) used for each blocking read. Kept
/// short so the blocking thread can drain pending outbound writes promptly.
const READ_TIMEOUT_MS: i32 = 250;

/// Interval between battery-polling subcommand writes.
const STATE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Interval between keepalive zero-rumble writes. The Pro Controller
/// disconnects after ~10-15s of inactivity over Bluetooth; sending a
/// zero-rumble report every second keeps the HID interface alive.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// Interval between rumble-refresh writes. When the user has rumble enabled
/// and the controller has acknowledged vibration enable (0x48), we resend the
/// rumble report every 50 ms so the LRA motors stay active.
const RUMBLE_REFRESH_INTERVAL: Duration = Duration::from_millis(50);

/// Delay between device-open retries when the controller is absent.
const REOPEN_DELAY: Duration = Duration::from_secs(3);

/// Interval between HID device-list rescans in the multi-controller manager.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Sentinel slot value for the top-level manager loop that spawns per-slot loops.
const MANAGER_SLOT: u8 = 0xFF;

/// Inbound message: blocking read thread -> async loop.
enum DeviceMessage {
    /// A raw report was read successfully.
    Report(Vec<u8>),
    /// A read failed or the device disappeared.
    ReadError(String),
}

/// Outbound command: async loop -> blocking read/write thread.
/// Also sent from Tauri command handlers via the shared command channel.
pub enum DeviceCommand {
    /// Write a raw report/subcommand to the device.
    Write(Vec<u8>),
}

/// Owns the shared application state and the IPC broadcast sender.
///
/// A `DeviceLoop` instance is either the top-level manager (`slot == MANAGER_SLOT`)
/// or a per-slot worker (`slot` 0-3). The manager enumerates HID devices every
/// `RESCAN_INTERVAL` and spawns a `DeviceLoop` worker for each discovered Pro
/// Controller, up to `state::CONTROLLER_SLOTS`.
pub struct DeviceLoop {
    shared: Arc<SharedState>,
    tx: broadcast::Sender<IpcEvent>,
    /// Async reply-matching manager for outbound subcommands.
    subcmd_manager: Arc<SubcommandManager>,
    /// Monotonic counter used to downsample IMU events to ~60 Hz.
    imu_report_count: AtomicU32,
    /// Timestamp of the last standard report, for connection-quality tracking.
    last_report_time: PlMutex<Option<Instant>>,
    /// Advanced calibration pipeline for the left stick.
    left_stick_pipeline: PlMutex<stick_cal::StickCalibrationPipeline>,
    /// Advanced calibration pipeline for the right stick.
    right_stick_pipeline: PlMutex<stick_cal::StickCalibrationPipeline>,
    /// Partial left stick calibration parsed from SPI flash (before merge).
    left_stick_cal: PlMutex<Option<StickCalibration>>,
    /// Partial right stick calibration parsed from SPI flash (before merge).
    right_stick_cal: PlMutex<Option<StickCalibration>>,
    /// Frame counter for periodic CalibrationStatus events.
    frame_counter: AtomicU32,
    /// Timestamp of the last RawHidReport emit (throttled to ~20 Hz).
    last_raw_emit: PlMutex<Instant>,
    /// Previous drift status for left stick (to detect transitions).
    prev_left_drift: PlMutex<stick_cal::DriftStatus>,
    /// Previous drift status for right stick (to detect transitions).
    prev_right_drift: PlMutex<stick_cal::DriftStatus>,
    /// Turbo / rapid-fire + toggle engine applied to the virtual button state.
    turbo_engine: PlMutex<TurboEngine>,
    /// Timestamp of the last turbo update, used to derive `dt`.
    last_turbo_update: PlMutex<Option<Instant>>,
    /// Slot index this loop instance owns (0-3). `MANAGER_SLOT` for the manager.
    slot: u8,
    /// Path of the HID device currently claimed by this slot.
    claimed_path: PlMutex<Option<String>>,
    /// Shared set of paths claimed by running per-slot loops.
    claimed_paths: Option<Arc<PlMutex<HashSet<String>>>>,
    /// Handle for the task spawned by `start_loop`, used to abort on drop.
    own_handle: PlMutex<Option<tokio::task::AbortHandle>>,
    /// Handles for per-slot worker tasks spawned by the manager, used to abort
    /// all workers when the manager is dropped.
    child_handles: PlMutex<Vec<tokio::task::AbortHandle>>,
}

/// Fallback linear normalization when factory stick calibration is unavailable.
///
/// Maps the raw 12-bit ADC values (0–4095, center ~2048) to [-1, 1].
/// Clamps to [-1, 1] to handle overshoot past the nominal range.
fn fallback_normalize(lx: u16, ly: u16, rx: u16, ry: u16) -> (f32, f32, f32, f32) {
    let norm = |v: u16| -> f32 { ((v as f32 - 2048.0) / 2048.0).clamp(-1.0, 1.0) };
    (norm(lx), norm(ly), norm(rx), norm(ry))
}

impl DeviceLoop {
    /// Create the top-level manager loop. `main.rs` calls this and then
    /// `start_loop()`; the manager spawns per-slot `DeviceLoop` workers.
    pub fn new(shared: Arc<SharedState>, tx: broadcast::Sender<IpcEvent>) -> Self {
        Self::with_options(shared, tx, MANAGER_SLOT, None, None)
    }

    /// Create a per-slot worker loop.
    pub fn with_slot(
        shared: Arc<SharedState>,
        tx: broadcast::Sender<IpcEvent>,
        slot: u8,
        claimed_paths: Option<Arc<PlMutex<HashSet<String>>>>,
    ) -> Self {
        Self::with_options(shared, tx, slot, None, claimed_paths)
    }

    fn with_options(
        shared: Arc<SharedState>,
        tx: broadcast::Sender<IpcEvent>,
        slot: u8,
        preferred_path: Option<String>,
        claimed_paths: Option<Arc<PlMutex<HashSet<String>>>>,
    ) -> Self {
        Self {
            shared,
            tx,
            subcmd_manager: Arc::new(SubcommandManager::new(Duration::from_secs(2))),
            imu_report_count: AtomicU32::new(0),
            last_report_time: PlMutex::new(None),
            left_stick_pipeline: PlMutex::new(stick_cal::StickCalibrationPipeline::new()),
            right_stick_pipeline: PlMutex::new(stick_cal::StickCalibrationPipeline::new()),
            left_stick_cal: PlMutex::new(None),
            right_stick_cal: PlMutex::new(None),
            frame_counter: AtomicU32::new(0),
            last_raw_emit: PlMutex::new(Instant::now()),
            prev_left_drift: PlMutex::new(stick_cal::DriftStatus::Unknown),
            prev_right_drift: PlMutex::new(stick_cal::DriftStatus::Unknown),
            turbo_engine: PlMutex::new(TurboEngine::new()),
            last_turbo_update: PlMutex::new(None),
            slot,
            claimed_path: PlMutex::new(preferred_path),
            claimed_paths,
            own_handle: PlMutex::new(None),
            child_handles: PlMutex::new(Vec::new()),
        }
    }

    /// Spawns the async device loop and returns its join handle.
    /// The manager loop enumerates HID devices and spawns per-slot workers;
    /// per-slot workers open their claimed path and run a single HID lifecycle.
    pub fn start_loop(self: Arc<Self>) -> JoinHandle<()> {
        let handle = if self.slot == MANAGER_SLOT {
            let this = self.clone();
            tokio::spawn(async move {
                this.run_manager().await;
            })
        } else {
            let this = self.clone();
            tokio::spawn(async move {
                this.run().await;
            })
        };
        *self.own_handle.lock() = Some(handle.abort_handle());
        handle
    }

    async fn run_manager(self: Arc<Self>) {
        let claimed_paths: Arc<PlMutex<HashSet<String>>> = Arc::new(PlMutex::new(HashSet::new()));
        let mut handles: [Option<JoinHandle<()>>; CONTROLLER_SLOTS] = Default::default();
        let mut slot_paths: [Option<String>; CONTROLLER_SLOTS] = Default::default();
        let mut interval = tokio::time::interval(RESCAN_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            if self.shared.rescan_requested.swap(false, Ordering::SeqCst) {
                interval.reset_immediately();
            }
            interval.tick().await;

            // Clean up any finished per-slot handles.
            for slot in 0..CONTROLLER_SLOTS {
                let finished = handles[slot].as_ref().is_some_and(|h| h.is_finished());
                if finished {
                    if let Some(h) = handles[slot].take() {
                        let _ = h.await;
                    }
                    if let Some(p) = slot_paths[slot].take() {
                        claimed_paths.lock().remove(&p);
                    }
                    self.shared.set_slot_connected(slot as u8, false);
                    self.shared.slot_cmd_txs.write()[slot] = None;
                }
            }

            // Enumerate HID devices and claim new Pro Controllers.
            let devices = match tokio::task::spawn_blocking(enumerate_pro_controllers).await {
                Ok(Ok(list)) => list,
                Ok(Err(e)) => {
                    warn!("HID enumeration failed: {}", e);
                    continue;
                }
                Err(e) => {
                    warn!("HID enumeration task panicked: {}", e);
                    continue;
                }
            };

            for (path, _conn_type) in devices {
                if claimed_paths.lock().contains(&path) {
                    continue;
                }
                let slot = (0..CONTROLLER_SLOTS)
                    .find(|&i| handles[i].is_none())
                    .map(|i| i as u8);
                match slot {
                    Some(slot) => {
                        let slot_idx = slot as usize;
                        if slot_idx >= CONTROLLER_SLOTS {
                            warn!("Ignoring invalid controller slot {}", slot);
                            continue;
                        }
                        claimed_paths.lock().insert(path.clone());
                        slot_paths[slot_idx] = Some(path.clone());
                        let worker = Arc::new(DeviceLoop::with_slot(
                            self.shared.clone(),
                            self.tx.clone(),
                            slot,
                            Some(claimed_paths.clone()),
                        ));
                        *worker.claimed_path.lock() = Some(path);
                        let worker_handle = worker.start_loop();
                        self.child_handles.lock().push(worker_handle.abort_handle());
                        handles[slot_idx] = Some(worker_handle);
                    }
                    None => {
                        warn!("All controller slots full; skipping {}", path);
                        break;
                    }
                }
            }
        }
    }

    async fn run(self: Arc<Self>) {
        let slot_idx = self.slot as usize;
        if slot_idx >= CONTROLLER_SLOTS {
            warn!(
                "Refusing to start device loop for invalid slot {}",
                self.slot
            );
            return;
        }

        // Attempt to open the controller. Returns the opened device or gives up
        // so the manager can reschedule a worker on the next rescan.
        let device = match self.open_with_retry().await {
            Some(dev) => dev,
            None => {
                self.set_connected(false);
                return;
            }
        };

        // We successfully connected.
        info!("Pro Controller connected (slot {})", self.slot);
        let _ = self.tx.send(IpcEvent::Reconnected);
        self.set_connected(true);

        // Read the detected connection type for connection-specific init.
        let conn_type = self.slot_state().read().connection_type;

        // Bridge between the async loop and the blocking read/write thread.
        // Bounded to avoid unbounded memory growth if the async side stalls.
        let (msg_tx, mut msg_rx) = mpsc::channel::<DeviceMessage>(1024);
        let (cmd_tx, cmd_rx) = mpsc::channel::<DeviceCommand>(128);

        // Publish the command channel to SharedState so Tauri command
        // handlers (enable_imu, set_player_lights, send_rumble, etc.)
        // can actually send subcommands to the connected controller.
        self.shared.slot_cmd_txs.write()[slot_idx] = Some(cmd_tx.clone());

        let blocking_handle = tokio::task::spawn_blocking(move || {
            blocking_device_loop(device, msg_tx, cmd_rx);
        });

        // USB connections require a handshake sequence before the
        // controller will accept standard subcommands. Without the
        // "no timeout" command, the STM32 bridge MCU reverts to
        // Bluetooth mode after ~5 seconds and the USB HID device
        // disappears.
        if conn_type == ConnectionType::Usb {
            info!("USB connection detected — sending USB handshake sequence");
            // 1. Handshake — establish USB communication with the STM32.
            if let Err(err) = cmd_tx.try_send(DeviceCommand::Write(subcmd::build_usb_handshake())) {
                warn!("Failed to send USB handshake command: {}", err);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            // 2. Baudrate 3Mbit — switch UART to 3 Mbit/s.
            if let Err(err) = cmd_tx.try_send(DeviceCommand::Write(subcmd::build_usb_baudrate_3m()))
            {
                warn!("Failed to send USB baudrate command: {}", err);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            // 3. No timeout — prevent USB→Bluetooth fallback.
            if let Err(err) = cmd_tx.try_send(DeviceCommand::Write(subcmd::build_usb_no_timeout()))
            {
                warn!("Failed to send USB no-timeout command: {}", err);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            info!("USB handshake sequence sent (handshake + baudrate + no-timeout)");
        }

        // Send the set-input-report-mode subcommand (0x03 → 0x30) so the
        // controller starts streaming standard input reports. Without this,
        // the Pro Controller stays silent over Bluetooth and disconnects
        // from inactivity within ~10 seconds.
        let _ = cmd_tx.try_send(DeviceCommand::Write(build_set_report_mode_subcmd()));
        info!("Sent set-report-mode subcommand (0x03 → 0x30)");

        // Send the full initialization sequence: request device info,
        // enable IMU, enable vibration, and read stick calibration from
        // SPI flash. Each subcommand is spaced ~100 ms apart to give the
        // controller time to process and reply.
        self.send_init_sequence(&cmd_tx).await;

        // Drive the async side until the blocking thread stops.
        // Keep a clone of cmd_tx so we can send the USB enable-timeout
        // command after dispatch_loop returns (it consumes cmd_tx).
        let cmd_tx_for_cleanup = cmd_tx.clone();
        self.dispatch_loop(&mut msg_rx, cmd_tx, conn_type).await;

        // USB graceful disconnect: re-enable the USB timeout so the
        // STM32 bridge MCU can revert the controller to Bluetooth mode.
        // Without this, the controller may stay stuck in USB mode after
        // the host closes the HID device.
        if conn_type == ConnectionType::Usb {
            info!("Sending USB enable-timeout for graceful disconnect");
            let _ = cmd_tx_for_cleanup
                .try_send(DeviceCommand::Write(subcmd::build_usb_enable_timeout()));
            // Give the blocking thread time to drain the command before
            // we await its completion.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _ = blocking_handle.await;
        self.set_connected(false);
        // Clear the command channel so Tauri commands fail gracefully
        // instead of sending into a dead channel.
        self.shared.slot_cmd_txs.write()[slot_idx] = None;
        info!("Pro Controller disconnected (slot {})", self.slot);
    }

    /// Tries to open the Pro Controller.
    ///
    /// If `claimed_path` is set, the worker attempts to open that specific
    /// HID path once and gives up on failure (the manager will retry on the
    /// next rescan). If no path is claimed, the function falls back to the
    /// original enumeration-based retry loop.
    async fn open_with_retry(&self) -> Option<hidapi::HidDevice> {
        let preferred = self.claimed_path.lock().clone();
        if let Some(path) = preferred {
            match tokio::task::spawn_blocking(move || try_open_path(Some(path))).await {
                Ok(Ok((dev, conn_type, opened_path))) => {
                    info!("Detected connection type: {:?}", conn_type);
                    self.slot_state().write().connection_type = conn_type;
                    *self.claimed_path.lock() = Some(opened_path);
                    return Some(dev);
                }
                Ok(Err(e)) => {
                    warn!("Failed to open Pro Controller: {}", e);
                    let _ = self.tx.send(IpcEvent::Disconnected {
                        reason: format!("open failed: {}", e),
                    });
                }
                Err(e) => {
                    warn!("Open task panicked: {}", e);
                }
            }
            None
        } else {
            // Fallback: enumerate and retry forever.
            loop {
                match tokio::task::spawn_blocking(|| try_open_path(None)).await {
                    Ok(Ok((dev, conn_type, opened_path))) => {
                        info!("Detected connection type: {:?}", conn_type);
                        self.slot_state().write().connection_type = conn_type;
                        *self.claimed_path.lock() = Some(opened_path);
                        return Some(dev);
                    }
                    Ok(Err(e)) => {
                        warn!("Failed to open Pro Controller: {}", e);
                        let _ = self.tx.send(IpcEvent::Disconnected {
                            reason: format!("open failed: {}", e),
                        });
                        tokio::time::sleep(REOPEN_DELAY).await;
                    }
                    Err(e) => {
                        warn!("Open task panicked: {}", e);
                        tokio::time::sleep(REOPEN_DELAY).await;
                    }
                }
            }
        }
    }

    /// Consumes reports from the blocking thread until it stops, dispatching
    /// IPC events and periodically polling battery state.
    async fn dispatch_loop(
        self: &Arc<Self>,
        msg_rx: &mut mpsc::Receiver<DeviceMessage>,
        cmd_tx: mpsc::Sender<DeviceCommand>,
        conn_type: ConnectionType,
    ) {
        let mut last_state_poll = Instant::now();
        let mut last_keepalive = Instant::now();
        let mut last_rumble_refresh = Instant::now();
        let mut last_profile_check = Instant::now();

        loop {
            tokio::select! {
                biased;

                Some(msg) = msg_rx.recv() => {
                    match msg {
                        DeviceMessage::Report(data) => {
                            self.handle_report(&data);
                        }
                        DeviceMessage::ReadError(reason) => {
                            warn!("HID read error: {}", reason);
                            let _ = self.tx.send(IpcEvent::Disconnected { reason });
                            return;
                        }
                    }
                }

                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    // Rumble refresh: if the user has rumble enabled and the
                    // controller has acknowledged vibration enable (0x48),
                    // resend the rumble report to keep the LRA motors active.
                    if last_rumble_refresh.elapsed() >= RUMBLE_REFRESH_INTERVAL {
                        last_rumble_refresh = Instant::now();
                        let (rumble_on, vib_on, lf, la, rf, ra) = {
                            let s = self.slot_state().read();
                            (
                                s.rumble.enabled,
                                s.vibration_enabled,
                                s.rumble.left_frequency,
                                s.rumble.left_amplitude,
                                s.rumble.right_frequency,
                                s.rumble.right_amplitude,
                            )
                        };
                        if rumble_on && vib_on {
                            let counter = self.shared.next_packet_number();
                            let pkt = subcmd::build_rumble_report(counter, lf, la, rf, ra);
                            let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
                        }
                    }

                    // Keepalive: send a zero-rumble report every second to
                    // prevent the Pro Controller from sleeping over Bluetooth.
                    // USB connections don't need keepalive — the USB bus
                    // keeps the device active, and unnecessary writes could
                    // interfere with the STM32 bridge MCU.
                    if conn_type != ConnectionType::Usb
                        && last_keepalive.elapsed() >= KEEPALIVE_INTERVAL
                    {
                        last_keepalive = Instant::now();
                        let _ = cmd_tx.try_send(DeviceCommand::Write(build_zero_rumble()));
                    }

                    // Periodic battery poll. The controller emits a 0x21
                    // subcommand reply after we send the get-state subcommand;
                    // the blocking thread will read it and forward it here.
                    if last_state_poll.elapsed() >= STATE_POLL_INTERVAL {
                        last_state_poll = Instant::now();
                        let subcmd = build_get_state_subcmd();
                        let _ = cmd_tx.try_send(DeviceCommand::Write(subcmd));
                    }

                    // Periodic profile auto-switch check (every ~1s).
                    if last_profile_check.elapsed() >= Duration::from_secs(1) {
                        last_profile_check = Instant::now();
                        self.check_profile_auto_switch();
                    }
                }
            }
        }
    }

    /// Routes a raw report to the appropriate telemetry handler and emits the
    /// corresponding IPC events.
    fn handle_report(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let report_id = data[0];

        // Emit the raw report for debug/UI consumers, but throttle to ~20 Hz
        // to avoid flooding the broadcast channel (which has a 256-message
        // buffer). At 120 Hz, raw HID reports alone would fill the buffer in
        // ~2 seconds if the WebSocket client is slow to drain.
        let now = Instant::now();
        let should_emit_raw = {
            let mut last = self.last_raw_emit.lock();
            if now.duration_since(*last) >= Duration::from_millis(50) {
                *last = now;
                true
            } else {
                false
            }
        };
        if should_emit_raw {
            let _ = self.tx.send(IpcEvent::RawHidReport {
                hex: hex_string(data),
                report_id,
            });
        }

        match report_id {
            REPORT_ID_STANDARD | REPORT_ID_DEFAULT_BT => {
                self.handle_standard_report(data);
            }
            REPORT_ID_NFC_IR => {
                self.handle_nfc_ir_report(data);
            }
            REPORT_ID_SUBCMD_REPLY => {
                self.handle_subcmd_reply(data);
            }
            REPORT_ID_USB_REPLY => {
                self.handle_usb_reply(data);
            }
            other => debug!("Unhandled report id 0x{:02X} ({} bytes)", other, data.len()),
        }
    }

    /// Poll the foreground window and switch to the first matching auto-profile.
    fn check_profile_auto_switch(&self) {
        let (process_path, window_title) = match crate::profile_manager::detect_active_process() {
            Ok(v) => v,
            Err(e) => {
                log::debug!("Profile auto-switch detection failed: {}", e);
                return;
            }
        };

        let cfg = self.shared.config.read();
        let Some(profile) = crate::profile_manager::find_matching_profile_state(
            &cfg.profile_manager,
            &process_path,
            &window_title,
        ) else {
            return;
        };

        let current_id = cfg.profile_manager.active_profile_id.clone();
        let profile_id = profile.id.clone();
        let profile_name = profile.name.clone();
        drop(cfg);

        if current_id.as_deref() != Some(&profile_id) {
            let mut cfg = self.shared.config.write();
            cfg.profile_manager.active_profile_id = Some(profile_id.clone());
            cfg.profile_manager.last_applied = Some(profile_id.clone());
            drop(cfg);

            let _ = self.tx.send(IpcEvent::ProfileChanged {
                profile_id: Some(profile_id),
                profile_name: Some(profile_name),
            });
        }
    }

    /// Processes a 0x31 NFC/IR input report: update the per-slot NFC state and
    /// emit tag/scan events when a tag is detected.
    fn handle_nfc_ir_report(&self, data: &[u8]) {
        let mut state = self.slot_state().read().clone();
        crate::nfc::apply_nfc_report(&mut state.nfc, data);

        if let Some(ref tag) = state.nfc.last_tag {
            let _ = self.tx.send(IpcEvent::NfcTagScanned { tag: tag.clone() });
        }

        *self.slot_state().write() = state;
    }

    /// Processes a 0x30 standard input report: updates sticks/buttons, applies
    /// deadzone + remap from config, writes back to shared state, emits a
    /// `ControllerState` event.
    fn handle_standard_report(&self, data: &[u8]) {
        // Read only the config fields needed for this hot path; drop the
        // read guard before doing the heavier per-report work.
        let (
            deadzone_left,
            deadzone_right,
            button_remap,
            kbm_enabled,
            kbm_config,
            mappings,
            right_stick_mode,
            flick_stick_config,
            deadzone_shape,
        ) = {
            let cfg = self.shared.config.read();
            let kbm_config = cfg.kbm_config.clone();
            let mappings = cfg.mappings.clone();
            let button_remap = cfg.button_remap.clone();
            let right_stick = cfg.right_stick;
            let deadzone_shape = cfg.stick_calibration_config.deadzone_shape.clone();
            (
                cfg.deadzone_left,
                cfg.deadzone_right,
                button_remap,
                kbm_config.enabled,
                kbm_config,
                mappings,
                right_stick.mode,
                right_stick.flick_stick,
                deadzone_shape,
            )
        };

        // Time since the last standard report; used for Flick Stick continuous
        // rotation and dt-driven subsystems. Defaults to one 60 Hz frame.
        const DEFAULT_REPORT_DT: f32 = 1.0 / 60.0;
        let report_dt = {
            let last = self.last_report_time.lock();
            last.map(|t| t.elapsed().as_secs_f32())
                .unwrap_or(DEFAULT_REPORT_DT)
        };

        // Acquire a write guard once and mutate the shared slot state in place.
        let mut state = self.slot_state().write();

        let parsed = match TelemetryExtractor::update_from_standard_report(&mut state, data) {
            Some(p) => p,
            None => return,
        };

        // If valid stick calibration is available, re-normalize the raw stick
        // values using the calibration data (piecewise linear, Linux kernel
        // formula) and run them through the advanced calibration pipeline
        // (adaptive deadzone, center auto-cal, drift detection, gate cal,
        // response curve).
        //
        // When factory calibration is unavailable (SPI flash uninitialized),
        // fall back to a simple linear normalization from the raw 12-bit ADC
        // range (0–4095, center ~2048) to [-1, 1]. This still allows gate
        // calibration sweeps to capture the physical gate boundary.
        let (lx, ly, rx, ry) = if let Some(ref cal) = state.stick_calibration {
            if cal.valid {
                (
                    subcmd::normalize_stick_calibrated(
                        state.left_stick.raw_x,
                        cal.left_center_x,
                        cal.left_min_x,
                        cal.left_max_x,
                    ),
                    subcmd::normalize_stick_calibrated(
                        state.left_stick.raw_y,
                        cal.left_center_y,
                        cal.left_min_y,
                        cal.left_max_y,
                    ),
                    subcmd::normalize_stick_calibrated(
                        state.right_stick.raw_x,
                        cal.right_center_x,
                        cal.right_min_x,
                        cal.right_max_x,
                    ),
                    subcmd::normalize_stick_calibrated(
                        state.right_stick.raw_y,
                        cal.right_center_y,
                        cal.right_min_y,
                        cal.right_max_y,
                    ),
                )
            } else {
                fallback_normalize(
                    state.left_stick.raw_x,
                    state.left_stick.raw_y,
                    state.right_stick.raw_x,
                    state.right_stick.raw_y,
                )
            }
        } else {
            fallback_normalize(
                state.left_stick.raw_x,
                state.left_stick.raw_y,
                state.right_stick.raw_x,
                state.right_stick.raw_y,
            )
        };

        let (plx, ply, _) = self.left_stick_pipeline.lock().process(lx, ly);
        let (prx, pry, _) = self.right_stick_pipeline.lock().process(rx, ry);

        state.left_stick.x = plx;
        state.left_stick.y = ply;
        state.right_stick.x = prx;
        state.right_stick.y = pry;

        // Feed samples into the gate calibration collector if active.
        // We collect both sticks' normalized pre-pipeline values so the
        // sweep captures the physical gate boundary.
        {
            let mut collector = self.shared.gate_cal_collector.lock();
            if collector.active && !collector.done {
                collector.add(lx, ly);
                collector.add(rx, ry);
                // Auto-complete once we have enough samples.
                if collector.is_ready() {
                    let samples = collector.samples.clone();
                    self.left_stick_pipeline.lock().gate_cal.calibrate(&samples);
                    self.right_stick_pipeline
                        .lock()
                        .gate_cal
                        .calibrate(&samples);
                    collector.finish();
                    info!(
                        "Gate calibration complete — {} samples collected",
                        samples.len()
                    );
                }
            }
        }

        // Periodically send calibration status (every 60 frames ≈ 1s @ 60Hz).
        let frame = self.frame_counter.fetch_add(1, Ordering::SeqCst);
        if frame.is_multiple_of(60) {
            // Sync UI config to pipelines before reading status.
            let cal_config = self.shared.stick_calibration_config.read().clone();
            self.left_stick_pipeline.lock().reconfigure(&cal_config);
            self.right_stick_pipeline.lock().reconfigure(&cal_config);
            let left_status = self.left_stick_pipeline.lock().get_status();
            let right_status = self.right_stick_pipeline.lock().get_status();
            let left_drift = left_status.drift_status;
            let right_drift = right_status.drift_status;
            let _ = self
                .tx
                .send(IpcEvent::CalibrationStatus { data: left_status });

            // Check for drift status transitions → emit DriftDetected notification.
            let mut prev_left = self.prev_left_drift.lock();
            let mut prev_right = self.prev_right_drift.lock();

            if left_drift != *prev_left {
                if matches!(
                    left_drift,
                    stick_cal::DriftStatus::Drifting | stick_cal::DriftStatus::Fail
                ) {
                    let _ = self.tx.send(IpcEvent::DriftDetected {
                        stick: "Left".into(),
                        status: format!("{:?}", left_drift),
                    });
                }
                *prev_left = left_drift;
            }
            if right_drift != *prev_right {
                if matches!(
                    right_drift,
                    stick_cal::DriftStatus::Drifting | stick_cal::DriftStatus::Fail
                ) {
                    let _ = self.tx.send(IpcEvent::DriftDetected {
                        stick: "Right".into(),
                        status: format!("{:?}", right_drift),
                    });
                }
                *prev_right = right_drift;
            }
        }

        // Every ~30 seconds (at 60Hz that's ~1800 frames), poll battery
        // voltage via subcommand 0x50 (Get Regulated Voltage). The reply is
        // handled in `handle_subcmd_reply` under SUBCMD_GET_VOLTAGE.
        if frame.is_multiple_of(1800) {
            if let Some(cmd_tx) = self.shared.slot_cmd_txs.read()[self.slot as usize].as_ref() {
                let counter = self.shared.next_packet_number();
                let pkt = subcmd::build_get_voltage_subcmd(counter);
                if let Err(e) = cmd_tx.try_send(DeviceCommand::Write(pkt)) {
                    warn!("HID command channel full; dropping voltage poll: {:?}", e);
                }
            }
        }

        TelemetryExtractor::apply_stick_curve_and_zones(
            &mut state,
            deadzone_left,
            deadzone_right,
            &deadzone_shape,
            &mappings.sticks.response_curve,
            &mappings.sticks.zones,
        );

        // Apply turbo / toggle mappings to the physical button state before the
        // final face-button remap. This lets `Mappings` use physical ButtonIds
        // while the output still respects the user's A/B/X/Y swap.
        let now = Instant::now();
        let dt = {
            let mut last = self.last_turbo_update.lock();
            let dt = last.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
            *last = Some(now);
            dt
        };
        {
            let mut engine = self.turbo_engine.lock();
            engine.set_global(mappings.turbo_interval_ms, mappings.turbo_duty_cycle);
            state.buttons = engine.update(&state.buttons, dt, &mappings);
        }

        TelemetryExtractor::apply_remap(&mut state.buttons, &button_remap);

        // KB/M output: translate the full controller state to keyboard/mouse
        // events whenever KB/M emulation is enabled.
        if kbm_enabled {
            let mut kbm = self.shared.kbm.lock();
            kbm.process_controller_state(&state, &kbm_config, &mappings);
        }

        // Flick Stick right-stick camera processing.
        if right_stick_mode == RightStickMode::FlickStick {
            let mut fs = self.shared.flick_stick[self.slot as usize].lock();
            let (delta_yaw, flicked) = fs.process_with_config(
                state.right_stick.x,
                state.right_stick.y,
                report_dt,
                &flick_stick_config,
            );
            state.camera_yaw = (state.camera_yaw + delta_yaw).rem_euclid(360.0);
            state.flick_active = flicked;
        }

        // Macro recording: feed each processed frame to the macro engine.
        {
            let engine = self.shared.macro_engine.lock();
            if let Some(engine) = engine.as_ref() {
                if engine.is_recording() {
                    engine.record_frame(&state, state.timestamp);
                }
            }
        }

        // Propagate IMU data from 0x30 reports. The Pro Controller sends 3 IMU
        // frames per report at ~60 Hz (180 Hz IMU sampling). We store the full
        // data in state and emit a downsampled IpcEvent::ImuData every 2nd
        // report (~60 Hz) to avoid flooding the frontend.
        if let Some(ref imu_data) = parsed.imu {
            state.imu = Some(imu_data.clone());

            // Log physical IMU values for the first frame at debug level.
            if log::log_enabled!(log::Level::Debug) {
                let physical = imu::raw_to_physical(&imu_data.frames[0]);
                debug!(
                    "IMU frame 0: accel=({:.2},{:.2},{:.2})g gyro=({:.2},{:.2},{:.2})dps",
                    physical.accel_x,
                    physical.accel_y,
                    physical.accel_z,
                    physical.gyro_x,
                    physical.gyro_y,
                    physical.gyro_z
                );
            }

            // Gyro-to-mouse / stick mapping: process all 3 IMU frames in the
            // report. At ~60 Hz reports this is an effective 180 Hz IMU rate,
            // so each frame is ~5.55 ms. Accumulate per-frame mouse deltas and
            // emit a single mouse move at the end of the report.
            const IMU_FRAME_DT: f32 = 1.0 / 180.0;
            let gyro_config = mappings.gyro.clone();
            let imu_cal = state.imu_calibration.clone();
            let mut gyro_mouse = self.shared.gyro_mouse.lock();
            let mut accumulated_dx = 0i32;
            let mut accumulated_dy = 0i32;
            for frame in &imu_data.frames {
                let physical = imu_cal.as_ref().map_or_else(
                    || imu::raw_to_physical(frame),
                    |cal| imu::raw_to_physical_calibrated(frame, cal),
                );
                let (dx, dy) = gyro_mouse.update(&physical, IMU_FRAME_DT, &gyro_config);
                if matches!(gyro_config.mode, GyroMode::Mouse) {
                    accumulated_dx = accumulated_dx.saturating_add(dx);
                    accumulated_dy = accumulated_dy.saturating_add(dy);
                }
            }
            state.gyro_mouse_delta = (accumulated_dx, accumulated_dy);
            if matches!(gyro_config.mode, GyroMode::Mouse)
                && (accumulated_dx != 0 || accumulated_dy != 0)
            {
                gyro_mouse.send_mouse_move(accumulated_dx, accumulated_dy);
                state.gyro_mouse_delta = (0, 0);
            }

            let count = self.imu_report_count.fetch_add(1, Ordering::SeqCst);
            if count.is_multiple_of(2) {
                let _ = self.tx.send(IpcEvent::ImuData {
                    frames: imu_data.clone(),
                    timestamp: timestamp_now(),
                });
            }
        }

        // Update connection-quality metrics from the timer byte gaps.
        self.update_connection_quality(&mut state, parsed.timer);

        // Emit connection-quality event so the frontend can display live
        // latency / report-rate metrics independently of the full state.
        let connection_quality = state.connection_quality.clone();
        let state_data = state.clone();
        drop(state);

        let _ = self.tx.send(IpcEvent::ConnectionQuality {
            data: connection_quality,
        });
        let _ = self.tx.send(IpcEvent::ControllerState { data: state_data });
    }

    /// Processes a 0x81 USB command reply from the STM32 bridge MCU.
    ///
    /// USB replies use report ID 0x81 (not 0x21). The reply contains the
    /// command ID being acknowledged and a status byte. We log the result
    /// for diagnostic purposes — the USB handshake sequence is fire-and-
    /// forget, but verifying the ACK helps troubleshoot USB connection
    /// issues.
    fn handle_usb_reply(&self, data: &[u8]) {
        if data.len() < 2 {
            warn!("USB reply too short ({} bytes) — ignoring", data.len());
            return;
        }
        // data[0] = report ID (0x81)
        // data[1] = command ID being acknowledged
        // data[2] = status (0x00 = success, non-zero = error)
        let cmd_id = data[1];
        let status = if data.len() >= 3 { data[2] } else { 0 };
        let cmd_name = match cmd_id {
            subcmd::USB_CMD_HANDSHAKE => "handshake",
            subcmd::USB_CMD_BAUDRATE_3M => "baudrate-3M",
            subcmd::USB_CMD_NO_TIMEOUT => "no-timeout",
            subcmd::USB_CMD_EN_TIMEOUT => "enable-timeout",
            _ => "unknown",
        };
        if status == 0 {
            info!("USB reply: {} (0x{:02X}) ACK — success", cmd_name, cmd_id);
        } else {
            warn!(
                "USB reply: {} (0x{:02X}) NACK — status=0x{:02X}",
                cmd_name, cmd_id, status
            );
        }
    }

    /// Processes a 0x21 subcommand reply: updates battery/charging info in
    /// shared state, emits a `ControllerState` event, and emits a
    /// `BatteryWarning` if the level is at/below the configured threshold.
    fn handle_subcmd_reply(&self, data: &[u8]) {
        let threshold = self.shared.config.read().battery_warning_threshold;

        let mut state = self.slot_state().read().clone();

        let reply = match TelemetryExtractor::update_from_subcmd_reply(&mut state, data) {
            Some(r) => r,
            None => return,
        };

        // Keep the percent consistent with the raw value via the shared helper.
        state.battery_percent = battery_raw_to_percent(state.battery_raw);

        // Route the reply based on the subcommand ID to update the relevant
        // state fields and emit dedicated IPC events.
        let subcmd_id = reply.subcmd_id;
        let ack = reply.ack;
        match subcmd_id {
            // 0x02 — Device Info: parse firmware, MAC, controller type.
            subcmd::SUBCMD_GET_DEVICE_INFO => {
                if let Some(mut info) = subcmd::parse_device_info_reply(&reply.reply_data) {
                    // Populate connection type from the controller state.
                    info.connection = match state.connection_type {
                        ConnectionType::Usb => "USB".to_string(),
                        ConnectionType::Bluetooth => "Bluetooth".to_string(),
                    };
                    // Carry over any SPI info already parsed from flash reads.
                    if let Some(existing) = state.device_info.as_ref().and_then(|d| d.spi.as_ref())
                    {
                        info.spi = Some(existing.clone());
                    }
                    state.device_info = Some(info.clone());
                    let _ = self.tx.send(IpcEvent::DeviceInfo { data: info });
                }
            }
            // 0x10 — SPI Flash Read: route based on the echoed SPI address.
            // The reply_data header is:
            //   [0..3] = read address (little-endian, 3 bytes)
            //   [3]    = read size
            //   [4..]  = flash data
            // ACK byte should be 0x90 (ACK with data) for successful SPI reads.
            subcmd::SUBCMD_SPI_FLASH_READ => {
                let hex: String = reply
                    .reply_data
                    .iter()
                    .map(|b| format!("{:02X}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                info!(
                    "SPI flash read reply (ack=0x{:02X}): {} bytes: {}",
                    ack,
                    reply.reply_data.len(),
                    hex
                );
                // Validate ACK: 0x90 = ACK with data, 0x80 = ACK without data
                if ack != 0x90 {
                    warn!(
                        "SPI flash read got non-standard ACK 0x{:02X} (expected 0x90) — data may be invalid",
                        ack
                    );
                }
                if reply.reply_data.len() >= 5 {
                    let addr = (reply.reply_data[0] as u32)
                        | ((reply.reply_data[1] as u32) << 8)
                        | ((reply.reply_data[2] as u32) << 16);
                    let flash_data = &reply.reply_data[4..];
                    info!(
                        "SPI flash read: addr=0x{:04X}, {} data bytes",
                        addr,
                        flash_data.len()
                    );

                    match addr {
                        // Left stick factory calibration (0x603D, 9 bytes)
                        subcmd::SPI_ADDR_LEFT_STICK_FACTORY => {
                            if let Some(left_cal) = subcmd::parse_left_stick_calibration(flash_data)
                            {
                                info!("Parsed left stick factory calibration");
                                *self.left_stick_cal.lock() = Some(left_cal);
                                self.try_merge_stick_calibration(&mut state);
                            } else {
                                warn!("Failed to parse left stick factory calibration");
                            }
                        }
                        // Right stick factory calibration (0x6046, 9 bytes)
                        subcmd::SPI_ADDR_RIGHT_STICK_FACTORY => {
                            if let Some(right_cal) =
                                subcmd::parse_right_stick_calibration(flash_data)
                            {
                                info!("Parsed right stick factory calibration");
                                *self.right_stick_cal.lock() = Some(right_cal);
                                self.try_merge_stick_calibration(&mut state);
                            } else {
                                warn!("Failed to parse right stick factory calibration");
                            }
                        }
                        // IMU factory calibration (0x6020, 24 bytes)
                        subcmd::SPI_ADDR_IMU_FACTORY => {
                            if let Some(imu_cal) = subcmd::parse_imu_calibration(flash_data) {
                                info!("Parsed IMU factory calibration");
                                state.imu_calibration = Some(imu_cal.clone());
                                let _ = self.tx.send(IpcEvent::CalibrationData {
                                    stick: state
                                        .stick_calibration
                                        .clone()
                                        .unwrap_or_else(subcmd::default_stick_calibration),
                                    imu: imu_cal,
                                });
                            } else {
                                info!("IMU factory calibration unavailable — using default");
                                let default_imu = subcmd::default_imu_calibration();
                                state.imu_calibration = Some(default_imu.clone());
                                let _ = self.tx.send(IpcEvent::CalibrationData {
                                    stick: state
                                        .stick_calibration
                                        .clone()
                                        .unwrap_or_else(subcmd::default_stick_calibration),
                                    imu: default_imu,
                                });
                            }
                        }
                        // Left stick user calibration (0x8010, 11 bytes)
                        subcmd::SPI_ADDR_LEFT_STICK_USER => {
                            if subcmd::check_user_cal_magic(flash_data) && flash_data.len() >= 11 {
                                if let Some(left_cal) =
                                    subcmd::parse_left_stick_calibration(&flash_data[2..])
                                {
                                    info!("Parsed left stick user calibration (magic OK)");
                                    *self.left_stick_cal.lock() = Some(left_cal);
                                    self.try_merge_stick_calibration(&mut state);
                                }
                            } else {
                                debug!(
                                    "Left stick user calibration: no valid magic, keeping factory"
                                );
                            }
                        }
                        // Right stick user calibration (0x801B, 11 bytes)
                        subcmd::SPI_ADDR_RIGHT_STICK_USER => {
                            if subcmd::check_user_cal_magic(flash_data) && flash_data.len() >= 11 {
                                if let Some(right_cal) =
                                    subcmd::parse_right_stick_calibration(&flash_data[2..])
                                {
                                    info!("Parsed right stick user calibration (magic OK)");
                                    *self.right_stick_cal.lock() = Some(right_cal);
                                    self.try_merge_stick_calibration(&mut state);
                                }
                            } else {
                                debug!(
                                    "Right stick user calibration: no valid magic, keeping factory"
                                );
                            }
                        }
                        // IMU user calibration (0x8026, 26 bytes)
                        subcmd::SPI_ADDR_IMU_USER => {
                            if subcmd::check_user_cal_magic(flash_data) && flash_data.len() >= 26 {
                                if let Some(imu_cal) =
                                    subcmd::parse_imu_calibration(&flash_data[2..])
                                {
                                    info!("Parsed IMU user calibration (magic OK)");
                                    state.imu_calibration = Some(imu_cal.clone());
                                    let _ = self.tx.send(IpcEvent::CalibrationData {
                                        stick: state
                                            .stick_calibration
                                            .clone()
                                            .unwrap_or_else(subcmd::default_stick_calibration),
                                        imu: imu_cal,
                                    });
                                }
                            } else {
                                debug!("IMU user calibration: no valid magic, keeping factory");
                            }
                        }
                        // Serial number (0x6000, 16 bytes ASCII)
                        subcmd::SPI_ADDR_SERIAL => {
                            let serial: String = flash_data
                                .iter()
                                .take_while(|b| **b != 0)
                                .map(|b| *b as char)
                                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                                .collect();
                            info!("SPI serial number: {:?}", serial);
                            let mut di = state.device_info.clone().unwrap_or_default();
                            let mut spi = di.spi.clone().unwrap_or_default();
                            spi.serial = serial;
                            di.spi = Some(spi);
                            state.device_info = Some(di);
                            self.emit_device_info(&state);
                        }
                        // Body color (0x6050, 3 bytes RGB)
                        subcmd::SPI_ADDR_BODY_COLOR => {
                            if flash_data.len() >= 3 {
                                let color = format!(
                                    "rgb({},{},{})",
                                    flash_data[0], flash_data[1], flash_data[2]
                                );
                                info!("SPI body color: {}", color);
                                let mut di = state.device_info.clone().unwrap_or_default();
                                let mut spi = di.spi.clone().unwrap_or_default();
                                spi.body_color = color;
                                di.spi = Some(spi);
                                state.device_info = Some(di);
                                self.emit_device_info(&state);
                            }
                        }
                        // Left grip color (0x6056, 3 bytes RGB)
                        subcmd::SPI_ADDR_LEFT_GRIP_COLOR => {
                            if flash_data.len() >= 3 {
                                let color = format!(
                                    "rgb({},{},{})",
                                    flash_data[0], flash_data[1], flash_data[2]
                                );
                                info!("SPI left grip color: {}", color);
                                let mut di = state.device_info.clone().unwrap_or_default();
                                let mut spi = di.spi.clone().unwrap_or_default();
                                spi.grip_color = color;
                                di.spi = Some(spi);
                                state.device_info = Some(di);
                                self.emit_device_info(&state);
                            }
                        }
                        // Right grip color (0x6059, 3 bytes RGB)
                        subcmd::SPI_ADDR_RIGHT_GRIP_COLOR => {
                            if flash_data.len() >= 3 {
                                let color = format!(
                                    "rgb({},{},{})",
                                    flash_data[0], flash_data[1], flash_data[2]
                                );
                                info!("SPI right grip color: {}", color);
                                // For now, we only display one grip color in the UI.
                                // If left grip wasn't read yet, use this one.
                                let mut di = state.device_info.clone().unwrap_or_default();
                                let mut spi = di.spi.clone().unwrap_or_default();
                                if spi.grip_color.is_empty() {
                                    spi.grip_color = color;
                                }
                                di.spi = Some(spi);
                                state.device_info = Some(di);
                                self.emit_device_info(&state);
                            }
                        }
                        // Color flag (0x601B, 1 byte): 0x01 means use SPI colors.
                        subcmd::SPI_ADDR_COLOR_FLAG => {
                            if !flash_data.is_empty() {
                                let use_spi = flash_data[0] == 0x01;
                                let mut di = state.device_info.clone().unwrap_or_default();
                                let mut spi = di.spi.clone().unwrap_or_default();
                                spi.use_spi_colors = use_spi;
                                if !use_spi {
                                    // Reset to defaults if flag is 0
                                    spi.body_color = "rgb(85,85,85)".to_string(); // default gray
                                    spi.button_color = "rgb(255,255,255)".to_string(); // default white
                                    spi.grip_color = "rgb(255,255,255)".to_string();
                                }
                                di.spi = Some(spi);
                                state.device_info = Some(di);
                                self.emit_device_info(&state);
                                info!("SPI color flag: use_spi_colors={}", use_spi);
                            }
                        }
                        // Button color (0x6053, 3 bytes RGB)
                        subcmd::SPI_ADDR_BUTTON_COLOR => {
                            if flash_data.len() >= 3 {
                                let color = format!(
                                    "rgb({},{},{})",
                                    flash_data[0], flash_data[1], flash_data[2]
                                );
                                info!("SPI button color: {}", color);
                                let mut di = state.device_info.clone().unwrap_or_default();
                                let mut spi = di.spi.clone().unwrap_or_default();
                                spi.button_color = color;
                                di.spi = Some(spi);
                                state.device_info = Some(di);
                                self.emit_device_info(&state);
                            }
                        }
                        // Horizontal offsets (0x6080, 6 bytes: 3× int16LE)
                        subcmd::SPI_ADDR_HORIZONTAL_OFFSETS => {
                            if flash_data.len() >= 6 {
                                let offsets = [
                                    i16::from_le_bytes([flash_data[0], flash_data[1]]),
                                    i16::from_le_bytes([flash_data[2], flash_data[3]]),
                                    i16::from_le_bytes([flash_data[4], flash_data[5]]),
                                ];
                                // Store in IMU calibration if present, otherwise
                                // create a default with the offsets applied.
                                if let Some(ref mut cal) = state.imu_calibration {
                                    cal.horizontal_offsets = offsets;
                                }
                                info!("SPI horizontal offsets: {:?}", offsets);
                            }
                        }
                        _ => {
                            debug!("Unhandled SPI flash read address: 0x{:06X}", addr);
                        }
                    }
                } else {
                    warn!(
                        "SPI flash read reply too short: {} bytes (expected >=5)",
                        reply.reply_data.len()
                    );
                }
            }
            // 0x30 — Set Player Lights: the controller ACKs with no reply data.
            // The 0x21 report is 49 bytes, so data[15..] contains IMU/button
            // bytes, NOT actual reply data. The correct PlayerLightsChanged
            // event is already sent by the Tauri command handler, so we just
            // log the ACK here.
            subcmd::SUBCMD_SET_PLAYER_LIGHTS => {
                debug!("Player lights subcommand ACKed (0x30)");
            }
            // 0x38 — Set Home Light: the controller ACKs with no reply data.
            // The correct HomeLightChanged event is already sent by the Tauri
            // command handler, so we just log the ACK here.
            subcmd::SUBCMD_SET_HOME_LIGHT => {
                debug!("Home light subcommand ACKed (0x38)");
            }
            // 0x40 — Enable IMU: mark IMU as active in state.
            subcmd::SUBCMD_ENABLE_IMU => {
                state.imu_enabled = true;
                info!("IMU enabled (subcmd 0x40 ACK)");
            }
            // 0x48 — Enable Vibration: mark vibration as active in state.
            subcmd::SUBCMD_ENABLE_VIBRATION => {
                state.vibration_enabled = true;
                info!("Vibration enabled (subcmd 0x48 ACK)");
            }
            // 0x50 — Get Regulated Voltage: parse the 16-bit LE voltage value
            // from the reply data and store it as millivolts (2.5x multiplier).
            subcmd::SUBCMD_GET_VOLTAGE => {
                if reply.reply_data.len() >= 2 {
                    let voltage = u16::from_le_bytes([reply.reply_data[0], reply.reply_data[1]]);
                    let voltage_mv = (voltage as u32).saturating_mul(25) / 10; // 2.5x multiplier
                    state.battery_voltage_mv = voltage_mv.min(u16::MAX as u32) as u16;
                    info!("Battery voltage: {} (raw) → {}mV", voltage, voltage_mv);
                }
            }
            // 0x03 — Set Report Mode ACK: log confirmation.
            subcmd::SUBCMD_SET_REPORT_MODE => {
                debug!("Report mode subcommand ACKed (0x03)");
            }
            // 0x31 — Get Player Lights: parse the LED state byte.
            // Low nibble = steady-on LEDs, high nibble = flashing LEDs.
            subcmd::SUBCMD_GET_PLAYER_LIGHTS => {
                if !reply.reply_data.is_empty() {
                    let lights_byte = reply.reply_data[0];
                    state.player_lights.led_mask = lights_byte & 0x0F;
                    state.player_lights.flash_pattern = (lights_byte >> 4) & 0x0F;
                    info!(
                        "Player lights: led_mask=0x{:02X}, flash_pattern=0x{:02X}",
                        state.player_lights.led_mask, state.player_lights.flash_pattern
                    );
                }
            }
            _ => debug!(
                "Unhandled subcmd reply 0x{:02X} ({} data bytes)",
                subcmd_id,
                reply.reply_data.len()
            ),
        }

        // Forward the reply to the SubcommandManager so any async waiter
        // registered via `register_pending` is fulfilled.
        let manager = self.subcmd_manager.clone();
        let reply_data = reply.reply_data.clone();
        tokio::spawn(async move {
            manager.handle_reply(subcmd_id, ack, reply_data).await;
        });

        // Emit a generic SubcommandReply event for debug/UI consumers.
        let _ = self.tx.send(IpcEvent::SubcommandReply {
            subcmd_id,
            ack,
            data: reply.reply_data,
        });

        let warn_low = TelemetryExtractor::check_battery_warning(&state, threshold);

        *self.slot_state().write() = state.clone();

        // Emit a dedicated battery-state event so the frontend can update the
        // battery widget without waiting for the full ControllerState payload.
        let _ = self.tx.send(IpcEvent::BatteryState {
            percent: state.battery_percent,
            charging: state.charging,
            raw: state.battery_raw,
            health: battery_health_label(state.battery_raw),
        });

        let _ = self.tx.send(IpcEvent::ControllerState { data: state });

        if warn_low {
            let percent = self.slot_state().read().battery_percent;
            let _ = self.tx.send(IpcEvent::BatteryWarning { percent });
        }
    }

    /// Attempts to merge left and right stick calibration partials. When both
    /// are available, merges, validates, and stores the result in `state`.
    /// Falls back to default calibration if validation fails. Sends a
    /// `CalibrationData` IPC event with the merged result.
    fn try_merge_stick_calibration(&self, state: &mut ControllerState) {
        let left = self.left_stick_cal.lock().clone();
        let right = self.right_stick_cal.lock().clone();
        if let (Some(left), Some(right)) = (left, right) {
            let merged = subcmd::merge_stick_calibration(&left, &right);
            if merged.valid {
                info!(
                    "Stick calibration merged: L center=({},{}), max=({},{}), min=({},{}) | R center=({},{}), max=({},{}), min=({},{})",
                    merged.left_center_x, merged.left_center_y,
                    merged.left_max_x, merged.left_max_y,
                    merged.left_min_x, merged.left_min_y,
                    merged.right_center_x, merged.right_center_y,
                    merged.right_max_x, merged.right_max_y,
                    merged.right_min_x, merged.right_min_y,
                );
                state.stick_calibration = Some(merged.clone());
                let _ = self.tx.send(IpcEvent::CalibrationData {
                    stick: merged,
                    imu: state
                        .imu_calibration
                        .clone()
                        .unwrap_or_else(subcmd::default_imu_calibration),
                });
            } else {
                warn!("Merged stick calibration failed validation — using default");
                let default = subcmd::default_stick_calibration();
                state.stick_calibration = Some(default.clone());
                let _ = self.tx.send(IpcEvent::CalibrationData {
                    stick: default,
                    imu: state
                        .imu_calibration
                        .clone()
                        .unwrap_or_else(subcmd::default_imu_calibration),
                });
            }
        }
    }

    /// Sends the post-connection initialization subcommand sequence:
    /// device info (0x02), enable IMU (0x40), enable vibration (0x48), and
    /// SPI flash calibration reads (0x10 at addresses 0x603D, 0x6046, 0x6020
    /// for factory calibration, and 0x8010, 0x801B, 0x8026 for user calibration).
    ///
    /// Each subcommand is spaced ~100 ms apart to give the controller time to
    /// process and reply. The set-report-mode subcommand (0x03 → 0x30) is sent
    /// separately before this method is called.
    async fn send_init_sequence(&self, cmd_tx: &mpsc::Sender<DeviceCommand>) {
        // 1. Request device info (0x02) — firmware, MAC, controller type.
        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_get_device_info_subcmd(counter);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        info!("Sent get-device-info subcommand (0x02)");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 2. Enable IMU (0x40) — turns on the accelerometer + gyroscope so
        //    0x30 standard reports include the 3 IMU frames.
        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_enable_imu_subcmd(counter, true);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        info!("Sent enable-IMU subcommand (0x40)");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 3. Enable vibration (0x48) — enables the HD Rumble LRA motors.
        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_enable_vibration_subcmd(counter, true);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        info!("Sent enable-vibration subcommand (0x48)");
        // Wait 300ms before SPI reads — the controller needs time to settle
        // after enabling IMU/vibration, and input reports (0x30) can interfere
        // with 0x21 subcommand replies if sent too quickly.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 4. Read stick + IMU calibration from SPI flash. The factory
        //    calibration is stored at three separate addresses:
        //      - 0x603D (9 bytes): left stick factory calibration
        //      - 0x6046 (9 bytes): right stick factory calibration
        //      - 0x6020 (24 bytes): IMU factory calibration
        //    User calibration (optional, overrides factory if magic 0xB2 0xA1):
        //      - 0x8010 (11 bytes): left stick user calibration
        //      - 0x801B (11 bytes): right stick user calibration
        //      - 0x8026 (26 bytes): IMU user calibration
        //    Each read is spaced ~150 ms apart to avoid 0x21 reply collisions
        //    with continuous 0x30 input reports. Replies are routed in
        //    `handle_subcmd_reply` based on the echoed SPI address.
        let counter = self.shared.next_packet_number();
        let pkt =
            subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_LEFT_STICK_FACTORY, 9);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt =
            subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_RIGHT_STICK_FACTORY, 9);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_IMU_FACTORY, 24);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt =
            subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_LEFT_STICK_USER, 11);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt =
            subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_RIGHT_STICK_USER, 11);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_IMU_USER, 26);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        // 5. Read diagnostic SPI flash data: serial number, body color, grip colors.
        //    Serial is often blank on Pro Controllers — this is normal.
        //    Colors should be present on special edition controllers.
        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_SERIAL, 16);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_BODY_COLOR, 3);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_LEFT_GRIP_COLOR, 3);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt =
            subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_RIGHT_GRIP_COLOR, 3);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        // 6. Read color flag, button color, and horizontal offsets from SPI flash.
        //    - 0x601B (1 byte):  color flag — 0x01 means use SPI colors.
        //    - 0x6053 (3 bytes): button color (RGB).
        //    - 0x6080 (6 bytes): 6-axis horizontal offsets (3× int16LE).
        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_COLOR_FLAG, 1);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_BUTTON_COLOR, 3);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counter = self.shared.next_packet_number();
        let pkt =
            subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_HORIZONTAL_OFFSETS, 6);
        let _ = cmd_tx.try_send(DeviceCommand::Write(pkt));
        info!("Sent SPI flash read subcommands (factory + user calibration + diagnostics)");
    }

    /// Emit a DeviceInfo event with connection type populated from state.
    fn emit_device_info(&self, state: &ControllerState) {
        if let Some(mut info) = state.device_info.clone() {
            info.connection = match state.connection_type {
                ConnectionType::Usb => "USB".to_string(),
                ConnectionType::Bluetooth => "Bluetooth".to_string(),
            };
            // Mark calibration as present if stick_calibration is parsed.
            if let Some(ref mut spi) = info.spi {
                spi.calibration = state.stick_calibration.is_some();
            }
            let _ = self.tx.send(IpcEvent::DeviceInfo { data: info });
        }
    }

    /// Updates `state.connection_quality` from the gap between consecutive
    /// standard reports. The inter-report interval is used as a latency proxy
    /// and to estimate the report rate in Hz.
    fn update_connection_quality(&self, state: &mut ControllerState, timer: u8) {
        let now = Instant::now();
        let mut last_time = self.last_report_time.lock();
        state.connection_quality.total_packets =
            state.connection_quality.total_packets.wrapping_add(1);
        if let Some(prev) = *last_time {
            let elapsed_ms = now.duration_since(prev).as_millis() as f32;
            if elapsed_ms > 0.0 {
                state.connection_quality.latency_ms = elapsed_ms;
                state.connection_quality.report_rate_hz = (1000.0 / elapsed_ms) as u16;
                // Detect dropped frames: if the timer byte jumped by more than
                // 2 (expecting ~1 per report at 15ms intervals), count as dropped.
                let prev_timer = state.connection_quality.last_report_timer;
                let delta = timer.wrapping_sub(prev_timer) as i16;
                if !(0..=2).contains(&delta) {
                    state.connection_quality.dropped =
                        state.connection_quality.dropped.wrapping_add(1);
                    // Estimate packet loss rate from dropped/total ratio.
                    let total = state.connection_quality.total_packets.max(1) as f32;
                    state.connection_quality.packet_loss_rate =
                        (state.connection_quality.dropped as f32 / total) * 100.0;
                }
            }
        }
        state.connection_quality.last_report_timer = timer;
        *last_time = Some(now);
    }

    /// Returns the shared state handle for the slot this loop instance owns.
    fn slot_state(&self) -> &crate::state::ControllerSlot {
        let idx = self.slot as usize;
        debug_assert!(
            idx < CONTROLLER_SLOTS,
            "device loop slot must be validated before use"
        );
        &self.shared.slots[idx]
    }

    /// Updates the `connected` flag on the shared controller state.
    fn set_connected(&self, connected: bool) {
        if !connected {
            // Release the claimed path so another slot / rescan can claim it.
            if let Some(path) = self.claimed_path.lock().take() {
                if let Some(ref set) = self.claimed_paths {
                    set.lock().remove(&path);
                }
            }
        }
        self.shared.set_slot_connected(self.slot, connected);
    }
}

/// Return the lowest controller slot whose bit is unset in `active_mask`.
#[allow(dead_code)] // slot-management utility, kept for future use
fn lowest_free_slot(active_mask: u8) -> Option<u8> {
    for i in 0..CONTROLLER_SLOTS {
        let mask = 1u8 << (i as u32);
        if active_mask & mask == 0 {
            return Some(i as u8);
        }
    }
    None
}

/// Derive a battery health label from the raw battery nibble.
/// The Pro Controller reports 5 discrete levels: 0=empty, 2=critical,
/// 4=low, 6=medium, 8=full. The charging bit (bit 0) is masked out.
fn battery_health_label(raw: u8) -> String {
    let level = (raw >> 1) & 0x07;
    match level {
        0 => "Empty".to_string(),
        1 => "Critical".to_string(),
        2 => "Low".to_string(),
        3 => "Medium".to_string(),
        _ => "Full".to_string(),
    }
}

/// Runs on a blocking thread: owns the `HidDevice`, reads reports in a loop,
/// and services any outbound write commands between reads. Forwards each
/// report (or read error) to the async loop via `msg_tx`.
///
/// Blocking helper: enumerate all currently attached Pro Controllers and
/// return their HID paths together with the detected connection type.
fn enumerate_pro_controllers() -> Result<Vec<(String, ConnectionType)>, String> {
    let api = HidApi::new().map_err(|e| format!("HidApi::new: {}", e))?;

    let mut devices = Vec::new();
    for dev_info in api.device_list() {
        if dev_info.vendor_id() == NINTENDO_VID && dev_info.product_id() == PRO_CONTROLLER_PID {
            let path = dev_info.path().to_string_lossy().into_owned();
            let connection_type = match dev_info.bus_type() {
                BusType::Usb => ConnectionType::Usb,
                _ => ConnectionType::Bluetooth,
            };
            devices.push((path, connection_type));
        }
    }
    Ok(devices)
}

/// Blocking function that constructs a `HidApi` and opens the Pro Controller.
/// Called via `spawn_blocking` to avoid stalling the async executor.
///
/// Returns the opened `HidDevice` together with the detected `ConnectionType`
/// (USB vs Bluetooth) and the path string, determined from the hidapi `BusType`
/// of the matching device. If `claimed_path` is provided, the matching device
/// is opened by path; otherwise the first Pro Controller found is opened.
fn try_open_path(
    claimed_path: Option<String>,
) -> Result<(hidapi::HidDevice, ConnectionType, String), String> {
    let api = HidApi::new().map_err(|e| format!("HidApi::new: {}", e))?;

    for dev_info in api.device_list() {
        if dev_info.vendor_id() == NINTENDO_VID && dev_info.product_id() == PRO_CONTROLLER_PID {
            let path = dev_info.path();
            let path_str = path.to_string_lossy().into_owned();
            if let Some(ref target) = claimed_path {
                if &path_str != target {
                    continue;
                }
            }
            let bus_type = dev_info.bus_type();
            info!(
                "Opening Pro Controller by path: {} (bus={:?})",
                path_str, bus_type
            );
            let dev = api
                .open_path(path)
                .map_err(|e| format!("open_path({}): {}", path_str, e))?;

            let connection_type = match bus_type {
                BusType::Usb => ConnectionType::Usb,
                _ => ConnectionType::Bluetooth,
            };

            return Ok((dev, connection_type, path_str));
        }
    }

    if let Some(p) = claimed_path {
        Err(format!("claimed path {} not found", p))
    } else {
        Err(format!(
            "No Pro Controller (VID={:#06X}, PID={:#06X}) found in HID enumeration",
            NINTENDO_VID, PRO_CONTROLLER_PID
        ))
    }
}

/// The `HidDevice` lives only on this blocking thread, satisfying the
/// requirement that blocking hidapi calls never run on the tokio async
/// runtime thread.
fn blocking_device_loop(
    device: hidapi::HidDevice,
    msg_tx: mpsc::Sender<DeviceMessage>,
    mut cmd_rx: mpsc::Receiver<DeviceCommand>,
) {
    loop {
        // Drain any pending outbound writes before reading. Writes are
        // non-blocking relative to the device (a control transfer) but still
        // a blocking syscall, hence running here on the blocking thread.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                DeviceCommand::Write(buf) => {
                    if let Err(e) = device.write(&buf) {
                        debug!("HID write failed: {}", e);
                    }
                }
            }
        }

        let mut buf = [0u8; READ_BUF_SIZE];
        match device.read_timeout(&mut buf, READ_TIMEOUT_MS) {
            Ok(0) => {
                // No data within the timeout window; loop and re-check writes.
                continue;
            }
            Ok(n) => {
                let data = buf[..n].to_vec();
                if let Err(e) = msg_tx.try_send(DeviceMessage::Report(data)) {
                    warn!("HID report channel full; dropping report: {:?}", e);
                }
            }
            Err(e) => {
                let _ = msg_tx.try_send(DeviceMessage::ReadError(format!("{}", e)));
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_controllers(ctx: State<'_, AppCtx>) -> Vec<crate::state::ControllerState> {
    ctx.shared.slots.iter().map(|s| s.read().clone()).collect()
}

#[tauri::command]
pub fn get_controller(slot: u8, ctx: State<'_, AppCtx>) -> crate::state::ControllerState {
    let idx = slot as usize;
    if idx >= CONTROLLER_SLOTS {
        return ControllerState::default();
    }
    ctx.shared.slots[idx].read().clone()
}

#[tauri::command]
pub fn set_active_slot(slot: u8, ctx: State<'_, AppCtx>) -> Result<(), String> {
    if slot as usize >= CONTROLLER_SLOTS {
        return Err(format!("Invalid slot {}", slot));
    }
    ctx.shared.selected_slot.store(slot, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub fn rescan_controllers(ctx: State<'_, AppCtx>) {
    ctx.shared.request_rescan();
}

// ---------------------------------------------------------------------------
// Validation flags
// ---------------------------------------------------------------------------

/// Frontend payload for real-device validation settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationFlags {
    pub real_device_validation: bool,
    pub validation: ValidationConfig,
}

#[tauri::command]
pub fn get_validation_flags(ctx: State<'_, AppCtx>) -> ValidationFlags {
    let cfg = ctx.shared.config.read();
    ValidationFlags {
        real_device_validation: cfg.real_device_validation,
        validation: cfg.validation.clone(),
    }
}

#[tauri::command]
pub fn set_validation_flags(ctx: State<'_, AppCtx>, flags: ValidationFlags) -> ValidationFlags {
    let mut cfg = ctx.shared.config.write();
    cfg.real_device_validation = flags.real_device_validation;
    cfg.validation = flags.validation.clone();
    flags
}

/// Pure helper used by [`validate_current_controller`] and unit tests.
pub(crate) fn validate_controller_state(
    idx: usize,
    state: &mut ControllerState,
    cfg: &crate::state::AppConfig,
    vigem_connected: bool,
    hidhide_hidden: bool,
) -> Result<bool, String> {
    if idx >= CONTROLLER_SLOTS {
        return Err("Invalid controller slot".into());
    }
    let enabled = cfg.real_device_validation || cfg.validation.enable_real_device_checks;
    if !enabled || cfg.validation.mock_mode {
        state.validated = true;
        return Ok(true);
    }

    if !state.connected {
        state.validated = false;
        return Err("Controller not connected".into());
    }

    if cfg.validation.strict_calibration_requirements {
        let stick_ok = state
            .stick_calibration
            .as_ref()
            .map(|c| c.valid)
            .unwrap_or(false);
        if !stick_ok {
            state.validated = false;
            return Err("Controller stick calibration is invalid or missing".into());
        }
    }

    if cfg.validation.require_vigembus && !vigem_connected {
        state.validated = false;
        return Err("ViGEmBus driver is not connected".into());
    }

    if cfg.validation.require_hidhide && !hidhide_hidden {
        state.validated = false;
        return Err("Physical controller is not hidden by HidHide".into());
    }

    state.validated = true;
    Ok(true)
}

#[tauri::command]
pub fn validate_current_controller(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    let cfg = ctx.shared.config.read().clone();
    let idx = ctx.shared.selected_slot.load(Ordering::SeqCst) as usize;

    let mut state = ctx.shared.slots[idx.min(CONTROLLER_SLOTS - 1)].write();
    let vigem_connected = ctx.shared.vixinput_status.read().driver_connected;
    let hidhide_hidden = crate::hidhide::hidhide_get_status().hidden;
    validate_controller_state(idx, &mut state, &cfg, vigem_connected, hidhide_hidden)
}

#[cfg(test)]
fn controller_output_is_allowed(cfg: &crate::state::AppConfig, state: &ControllerState) -> bool {
    !cfg.real_device_validation || cfg.validation.mock_mode || state.validated
}

impl Drop for DeviceLoop {
    fn drop(&mut self) {
        // Abort our own spawned task and any worker tasks we created as the
        // manager. Closing the command channel is handled by dropping the
        // sender stored in SharedState when the task exits.
        if let Some(handle) = self.own_handle.lock().take() {
            handle.abort();
        }
        for handle in self.child_handles.lock().drain(..) {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        controller_output_is_allowed, lowest_free_slot, validate_controller_state, DeviceLoop,
    };
    use crate::state::{ControllerState, SharedState, CONTROLLER_SLOTS};
    use parking_lot::Mutex;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::broadcast;

    #[test]
    fn lowest_free_slot_returns_first_unset_bit() {
        assert_eq!(lowest_free_slot(0b0000_0000), Some(0));
        assert_eq!(lowest_free_slot(0b0000_0001), Some(1));
        assert_eq!(lowest_free_slot(0b0000_0011), Some(2));
        assert_eq!(lowest_free_slot(0b0000_0111), Some(3));
        assert_eq!(lowest_free_slot(0b0000_1111), None);
        assert_eq!(lowest_free_slot(0b0000_1010), Some(0));
    }

    #[test]
    fn selected_slot_is_clamped_and_out_of_range_commands_are_rejected() {
        use std::sync::atomic::Ordering;

        let shared = SharedState::new();
        shared.selected_slot.store(99, Ordering::SeqCst);
        assert_eq!(shared.active_controller().slot_index, 3);
        assert!(shared.send_device_command_to_slot(4, Vec::new()).is_err());
    }

    #[test]
    fn validation_mode_requires_a_validated_controller_unless_mocked() {
        let mut config = crate::state::AppConfig::default();
        config.real_device_validation = true;
        let controller = ControllerState::default();
        assert!(!controller_output_is_allowed(&config, &controller));

        config.validation.mock_mode = true;
        assert!(controller_output_is_allowed(&config, &controller));
    }

    #[test]
    fn path_is_released_on_disconnect() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(4);
        let claimed_paths: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let path = "\\\\?\\hid#test".to_string();
        let loop_instance =
            DeviceLoop::with_slot(shared.clone(), tx, 0, Some(claimed_paths.clone()));
        *loop_instance.claimed_path.lock() = Some(path.clone());
        claimed_paths.lock().insert(path.clone());

        assert!(!shared.is_slot_active(0));
        loop_instance.set_connected(true);
        assert!(shared.is_slot_active(0));
        assert!(claimed_paths.lock().contains(&path));

        loop_instance.set_connected(false);

        assert!(!shared.is_slot_active(0));
        assert!(!claimed_paths.lock().contains(&path));
        assert!(loop_instance.claimed_path.lock().is_none());
    }

    // -----------------------------------------------------------------------
    // New dispatch-path unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn kbm_process_controller_state_emits_key_events() {
        use crate::kbm::{InputEvent, KbmEmulator, MockBackend};
        use crate::state::{Action, ButtonId, ButtonMapping, KbmConfig, Mappings};
        use std::sync::Arc;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let backend = Arc::new(MockBackend::new(tx));
        let mut kbm = KbmEmulator::with_backend(backend);

        let mut config = KbmConfig::default();
        config.enabled = true;

        let mut mappings = Mappings::default();
        mappings.buttons.push(ButtonMapping {
            source: ButtonId::A,
            actions: vec![Action::Key("a".into())],
        });

        // First report: A pressed.
        let mut state = ControllerState::default();
        state.buttons.a = true;
        kbm.process_controller_state(&state, &config, &mappings);

        let ev = rx.try_recv().expect("expected keydown event");
        assert!(matches!(ev, InputEvent::Key { down: true, .. }));

        // Second report: A released.
        state.buttons.a = false;
        kbm.process_controller_state(&state, &config, &mappings);

        let ev = rx.try_recv().expect("expected keyup event");
        assert!(matches!(ev, InputEvent::Key { down: false, .. }));
    }

    #[test]
    fn flick_stick_process_with_config_updates_yaw() {
        use crate::state::flick_stick::{FlickStick, FlickStickConfig};

        let cfg = FlickStickConfig {
            enabled: true,
            flick_threshold: 0.9,
            rotate_rate_deg_per_sec: 360.0,
            stick_deadzone: 0.1,
            flick_cooldown_ms: 0,
            output_smoothing: 0.0,
        };
        let mut fs = FlickStick::with_config(cfg);
        let (delta, flicked) = fs.process_with_config(1.0, 0.0, 0.016, &cfg);
        assert!(flicked, "should report a flick");
        assert!(delta.abs() > 0.0, "delta yaw should be non-zero");
    }

    #[test]
    fn nfc_apply_nfc_report_clears_tag_when_no_payload() {
        use crate::nfc;
        use crate::state::NfcState;

        let mut state = NfcState::default();
        state.tag_present = true;
        nfc::apply_nfc_report(&mut state, &[0x31]);
        assert!(!state.tag_present);
        assert!(state.uid.is_none());
    }

    #[test]
    fn profile_manager_state_matches_process_path() {
        use crate::profile_manager::find_matching_profile_state;
        use crate::state::{AutoRule, AutoRuleKind, MatchMode, Profile, ProfileManager};

        let mut pm = ProfileManager::default();
        pm.profiles.push(Profile {
            id: "p1".into(),
            name: "Game".into(),
            enabled: true,
            auto_rules: vec![AutoRule {
                kind: AutoRuleKind::ProcessPath,
                pattern: "game.exe".into(),
                match_mode: MatchMode::Contains,
                enabled: true,
            }],
            ..Default::default()
        });

        let profile = find_matching_profile_state(&pm, "C:\\\\game.exe", "Window")
            .expect("should match process path");
        assert_eq!(profile.id, "p1");
    }

    #[tokio::test]
    async fn macro_engine_record_frame_captures_input() {
        use crate::macro_engine::MacroEngine;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(4);
        let engine = MacroEngine::new(shared, tx, None).unwrap();

        engine.start_recording().unwrap();
        assert!(engine.is_recording());

        let mut state = ControllerState::default();
        state.buttons.a = true;
        engine.record_frame(&state, 123);

        // Capturing needs a transition, so provide a second frame with A released.
        state.buttons.a = false;
        engine.record_frame(&state, 124);

        let mac = engine.stop_recording("test".into()).await.unwrap();
        assert!(!mac.steps.is_empty(), "recorded macro should have steps");
    }

    #[test]
    fn validate_controller_passes_when_checks_disabled_or_mock_mode() {
        use crate::state::AppConfig;

        let mut state = ControllerState::default();
        let mut cfg = AppConfig::default();

        // Validation disabled by default.
        assert!(validate_controller_state(0, &mut state, &cfg, false, false).unwrap());
        assert!(state.validated);

        // Real checks enabled but mock mode short-circuits them.
        state.validated = false;
        cfg.real_device_validation = true;
        cfg.validation.mock_mode = true;
        assert!(validate_controller_state(0, &mut state, &cfg, false, false).unwrap());
        assert!(state.validated);
    }

    #[test]
    fn validate_controller_fails_when_not_connected() {
        use crate::state::AppConfig;

        let mut state = ControllerState::default();
        state.connected = false;
        let mut cfg = AppConfig::default();
        cfg.real_device_validation = true;
        cfg.validation.enable_real_device_checks = true;

        let result = validate_controller_state(0, &mut state, &cfg, true, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not connected"));
        assert!(!state.validated);
    }

    #[test]
    fn validate_controller_fails_on_missing_calibration() {
        use crate::state::{AppConfig, StickCalibration};

        let mut state = ControllerState::default();
        state.connected = true;
        let mut cfg = AppConfig::default();
        cfg.real_device_validation = true;
        cfg.validation.enable_real_device_checks = true;
        cfg.validation.strict_calibration_requirements = true;

        let result = validate_controller_state(0, &mut state, &cfg, true, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("calibration"));
        assert!(!state.validated);

        state.stick_calibration = Some(StickCalibration {
            valid: true,
            ..Default::default()
        });
        assert!(validate_controller_state(0, &mut state, &cfg, true, true).unwrap());
        assert!(state.validated);
    }

    #[test]
    fn validate_controller_checks_vigembus_and_hidhide() {
        use crate::state::AppConfig;

        let mut state = ControllerState::default();
        state.connected = true;
        let mut cfg = AppConfig::default();
        cfg.real_device_validation = true;
        cfg.validation.enable_real_device_checks = true;
        cfg.validation.require_vigembus = true;
        cfg.validation.require_hidhide = true;

        // Both required, ViGEmBus missing.
        let result = validate_controller_state(0, &mut state, &cfg, false, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ViGEmBus"));

        // ViGEmBus OK, HidHide missing.
        let result = validate_controller_state(0, &mut state, &cfg, true, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("HidHide"));

        // Both OK.
        assert!(validate_controller_state(0, &mut state, &cfg, true, true).unwrap());
        assert!(state.validated);
    }

    #[test]
    fn validate_controller_rejects_out_of_bounds_slot() {
        use crate::state::AppConfig;

        let mut state = ControllerState::default();
        let cfg = AppConfig::default();
        let result = validate_controller_state(CONTROLLER_SLOTS, &mut state, &cfg, true, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("slot"));
    }

    // -----------------------------------------------------------------------
    // fallback_normalize — pure linear normalization from 12-bit ADC to [-1,1]
    // -----------------------------------------------------------------------

    #[test]
    fn fallback_normalize_center_is_zero() {
        let (lx, ly, rx, ry) = super::fallback_normalize(2048, 2048, 2048, 2048);
        assert!((lx - 0.0).abs() < 1e-6);
        assert!((ly - 0.0).abs() < 1e-6);
        assert!((rx - 0.0).abs() < 1e-6);
        assert!((ry - 0.0).abs() < 1e-6);
    }

    #[test]
    fn fallback_normalize_max_is_one() {
        let (lx, ly, rx, ry) = super::fallback_normalize(4095, 4095, 4095, 4095);
        assert!((lx - 1.0).abs() < 1e-3);
        assert!((ly - 1.0).abs() < 1e-3);
        assert!((rx - 1.0).abs() < 1e-3);
        assert!((ry - 1.0).abs() < 1e-3);
    }

    #[test]
    fn fallback_normalize_min_is_neg_one() {
        let (lx, ly, rx, ry) = super::fallback_normalize(0, 0, 0, 0);
        assert!((lx - (-1.0)).abs() < 1e-3);
        assert!((ly - (-1.0)).abs() < 1e-3);
        assert!((rx - (-1.0)).abs() < 1e-3);
        assert!((ry - (-1.0)).abs() < 1e-3);
    }

    #[test]
    fn fallback_normalize_clamps_overshoot() {
        // Values above 4095 are not possible with u16, but the clamp ensures
        // that values near the extremes map to exactly ±1.0.
        let (lx, _, _, _) = super::fallback_normalize(4096, 2048, 2048, 2048);
        assert!((lx - 1.0).abs() < 1e-6, "4096 should clamp to 1.0, got {}", lx);
    }

    #[test]
    fn fallback_normalize_midpoint_is_half() {
        let (lx, _, _, _) = super::fallback_normalize(3072, 2048, 2048, 2048);
        assert!((lx - 0.5).abs() < 1e-3, "3072 should map to ~0.5, got {}", lx);
    }

    // -----------------------------------------------------------------------
    // battery_health_label — all 5 levels + charging bit masking
    // -----------------------------------------------------------------------

    #[test]
    fn battery_health_label_all_levels() {
        use super::battery_health_label;

        // Level 0 (raw >> 1 == 0) → Empty
        assert_eq!(battery_health_label(0x00), "Empty");
        // Level 1 (raw >> 1 == 1) → Critical
        assert_eq!(battery_health_label(0x02), "Critical");
        // Level 2 (raw >> 1 == 2) → Low
        assert_eq!(battery_health_label(0x04), "Low");
        // Level 3 (raw >> 1 == 3) → Medium
        assert_eq!(battery_health_label(0x06), "Medium");
        // Level 4+ (raw >> 1 >= 4) → Full
        assert_eq!(battery_health_label(0x08), "Full");
        assert_eq!(battery_health_label(0x0E), "Full");
    }

    #[test]
    fn battery_health_label_masks_charging_bit() {
        use super::battery_health_label;

        // Bit 0 is the charging flag; it should be masked out.
        // 0x09 = level 4 (Full) + charging bit → "Full"
        assert_eq!(battery_health_label(0x09), "Full");
        // 0x01 = level 0 (Empty) + charging bit → "Empty"
        assert_eq!(battery_health_label(0x01), "Empty");
        // 0x03 = level 1 (Critical) + charging bit → "Critical"
        assert_eq!(battery_health_label(0x03), "Critical");
    }

    // -----------------------------------------------------------------------
    // lowest_free_slot — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn lowest_free_slot_all_combinations() {
        use super::lowest_free_slot;

        // Every possible 4-bit mask (CONTROLLER_SLOTS == 4)
        for mask in 0..=0x0F_u8 {
            let result = lowest_free_slot(mask);
            // Find the expected lowest unset bit manually.
            let expected = (0..CONTROLLER_SLOTS)
                .find(|&i| mask & (1 << i) == 0)
                .map(|i| i as u8);
            assert_eq!(result, expected, "mask={:#06b}", mask);
        }
    }

    #[test]
    fn lowest_free_slot_ignores_high_bits() {
        use super::lowest_free_slot;

        // Bits above slot 3 should be ignored — slot 0 is still free.
        assert_eq!(lowest_free_slot(0b1111_0000), Some(0));
        // All 4 slots used, high bits set → None.
        assert_eq!(lowest_free_slot(0b1111_1111), None);
    }

    // -----------------------------------------------------------------------
    // validate_controller_state — additional branch coverage
    // -----------------------------------------------------------------------

    #[test]
    fn validate_controller_enable_real_device_checks_alone_suffices() {
        use crate::state::{AppConfig, ValidationConfig};

        let mut state = ControllerState { connected: true, ..Default::default() };
        let cfg = AppConfig {
            validation: ValidationConfig {
                enable_real_device_checks: true,
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(validate_controller_state(0, &mut state, &cfg, true, true).unwrap());
        assert!(state.validated);
    }

    #[test]
    fn validate_controller_strict_cal_with_invalid_cal_fails() {
        use crate::state::{AppConfig, StickCalibration, ValidationConfig};

        let mut state = ControllerState {
            connected: true,
            stick_calibration: Some(StickCalibration {
                valid: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = AppConfig {
            real_device_validation: true,
            validation: ValidationConfig {
                enable_real_device_checks: true,
                strict_calibration_requirements: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = validate_controller_state(0, &mut state, &cfg, true, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("calibration"));
        assert!(!state.validated);
    }

    #[test]
    fn validate_controller_require_vigembus_only() {
        use crate::state::{AppConfig, ValidationConfig};

        let mut state = ControllerState { connected: true, ..Default::default() };
        let cfg = AppConfig {
            real_device_validation: true,
            validation: ValidationConfig {
                enable_real_device_checks: true,
                require_vigembus: true,
                require_hidhide: false,
                ..Default::default()
            },
            ..Default::default()
        };

        // ViGEmBus missing → fail.
        let result = validate_controller_state(0, &mut state, &cfg, false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ViGEmBus"));

        // ViGEmBus present → pass (HidHide not required).
        assert!(validate_controller_state(0, &mut state, &cfg, true, false).unwrap());
        assert!(state.validated);
    }

    #[test]
    fn validate_controller_require_hidhide_only() {
        use crate::state::{AppConfig, ValidationConfig};

        let mut state = ControllerState { connected: true, ..Default::default() };
        let cfg = AppConfig {
            real_device_validation: true,
            validation: ValidationConfig {
                enable_real_device_checks: true,
                require_vigembus: false,
                require_hidhide: true,
                ..Default::default()
            },
            ..Default::default()
        };

        // HidHide missing → fail.
        let result = validate_controller_state(0, &mut state, &cfg, true, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("HidHide"));

        // HidHide present → pass.
        assert!(validate_controller_state(0, &mut state, &cfg, false, true).unwrap());
        assert!(state.validated);
    }

    #[test]
    fn validate_controller_no_strict_cal_passes_without_calibration() {
        use crate::state::{AppConfig, ValidationConfig};

        let mut state = ControllerState { connected: true, ..Default::default() };
        let cfg = AppConfig {
            real_device_validation: true,
            validation: ValidationConfig {
                enable_real_device_checks: true,
                ..Default::default()
            },
            ..Default::default()
        };
        // strict_calibration_requirements is false by default.
        // No stick calibration set, but should still pass.

        assert!(validate_controller_state(0, &mut state, &cfg, false, false).unwrap());
        assert!(state.validated);
    }

    // -----------------------------------------------------------------------
    // controller_output_is_allowed — additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn controller_output_allowed_when_validated() {
        use crate::state::AppConfig;

        let config = AppConfig {
            real_device_validation: true,
            ..Default::default()
        };
        let controller = ControllerState {
            validated: true,
            ..Default::default()
        };
        assert!(controller_output_is_allowed(&config, &controller));
    }

    #[test]
    fn controller_output_allowed_when_validation_disabled() {
        use crate::state::AppConfig;

        let config = AppConfig::default();
        let controller = ControllerState::default();
        // real_device_validation defaults to false.
        assert!(controller_output_is_allowed(&config, &controller));
    }

    // -----------------------------------------------------------------------
    // handle_report — input report parsing and dispatching with mock data
    // -----------------------------------------------------------------------

    #[test]
    fn handle_report_empty_data_is_noop() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        // Empty slice should return immediately without panicking.
        loop_instance.handle_report(&[]);
    }

    #[test]
    fn handle_report_unknown_report_id_does_not_panic() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        // Report ID 0xFF is not handled — should just log debug.
        let data = [0xFFu8, 0x01, 0x02, 0x03];
        loop_instance.handle_report(&data);
    }

    #[test]
    fn handle_report_standard_report_updates_state() {
        use crate::mock::MockGenerator;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(256);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        let mock = MockGenerator::new();
        let report = mock.build_full_standard_report();

        loop_instance.handle_report(&report);

        // After processing a standard report, the controller state should be
        // updated: connected == true, battery parsed, buttons set.
        let state = shared.slots[0].read();
        assert!(state.connected, "state.connected should be true after report");
        assert!(state.battery_percent > 0, "battery_percent should be > 0");
        // The mock report rotates face buttons (Y/X/B/A). At step 1, X is pressed.
        let any_button = state.buttons.a || state.buttons.b
            || state.buttons.x || state.buttons.y;
        assert!(any_button, "at least one face button should be pressed");
    }

    #[test]
    fn handle_report_imu_standard_report_populates_imu() {
        use crate::mock::MockGenerator;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(256);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        let mock = MockGenerator::new();
        let report = mock.build_imu_standard_report();

        loop_instance.handle_report(&report);

        let state = shared.slots[0].read();
        assert!(state.imu.is_some(), "IMU data should be populated");
        let imu = state.imu.as_ref().unwrap();
        assert_eq!(imu.frames.len(), 3, "should have 3 IMU frames");
        // Frame 0: accel_z should be 4096 (gravity).
        assert_eq!(imu.frames[0].accel_z, 4096);
    }

    #[test]
    fn handle_report_standard_report_emits_controller_state_event() {
        use crate::mock::MockGenerator;
        use crate::state::IpcEvent;

        let shared = SharedState::new();
        let (tx, mut rx) = broadcast::channel(256);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        let mock = MockGenerator::new();
        let report = mock.build_full_standard_report();

        loop_instance.handle_report(&report);

        // Drain events and check for ControllerState.
        let mut found_controller_state = false;
        let mut found_connection_quality = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, IpcEvent::ControllerState { .. }) {
                found_controller_state = true;
            }
            if matches!(ev, IpcEvent::ConnectionQuality { .. }) {
                found_connection_quality = true;
            }
        }
        assert!(found_controller_state, "ControllerState event should be emitted");
        assert!(found_connection_quality, "ConnectionQuality event should be emitted");
    }

    #[test]
    fn handle_report_nfc_ir_report_clears_or_sets_tag() {
        use crate::hid_parser::REPORT_ID_NFC_IR;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(256);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        // Pre-set a tag in state.
        {
            let mut state = shared.slots[0].write();
            state.nfc.tag_present = true;
        }

        // Send a minimal NFC/IR report with no payload — should clear the tag.
        let data = [REPORT_ID_NFC_IR];
        loop_instance.handle_report(&data);

        let state = shared.slots[0].read();
        assert!(!state.nfc.tag_present, "NFC tag should be cleared");
    }

    #[test]
    fn handle_report_usb_reply_short_data_does_not_panic() {
        use crate::hid_parser::REPORT_ID_USB_REPLY;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        // USB reply with only 1 byte (too short) — should log warning, not panic.
        let data = [REPORT_ID_USB_REPLY];
        loop_instance.handle_report(&data);

        // USB reply with 2 bytes — should parse command ID.
        let data = [REPORT_ID_USB_REPLY, 0x04];
        loop_instance.handle_report(&data);
    }

    #[test]
    fn handle_report_multiple_standard_reports_increment_packets() {
        use crate::mock::MockGenerator;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(256);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        let mock = MockGenerator::new();

        // Process 3 reports.
        for _ in 0..3 {
            let report = mock.build_full_standard_report();
            loop_instance.handle_report(&report);
        }

        let state = shared.slots[0].read();
        assert!(
            state.connection_quality.total_packets >= 3,
            "total_packets should be >= 3, got {}",
            state.connection_quality.total_packets
        );
    }

    // -----------------------------------------------------------------------
    // update_connection_quality — timer gap / dropped frame detection
    // -----------------------------------------------------------------------

    #[test]
    fn update_connection_quality_increments_total_packets() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        {
            let mut state = shared.slots[0].write();
            let initial = state.connection_quality.total_packets;
            loop_instance.update_connection_quality(&mut state, 0x01);
            assert_eq!(
                state.connection_quality.total_packets,
                initial.wrapping_add(1)
            );
        }
    }

    #[test]
    fn update_connection_quality_detects_dropped_frames() {
        use std::thread::sleep;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        {
            let mut state = shared.slots[0].write();
            // First report — sets last_report_timer.
            loop_instance.update_connection_quality(&mut state, 0x01);
            assert_eq!(state.connection_quality.dropped, 0);

            // Sleep >1ms so elapsed_ms > 0 and the timer-delta check runs.
            sleep(Duration::from_millis(5));

            // Second report with a large timer jump (0x01 → 0x10 = delta 15).
            // Delta > 2 → should count as dropped.
            loop_instance.update_connection_quality(&mut state, 0x10);
            assert_eq!(
                state.connection_quality.dropped, 1,
                "large timer gap should count as dropped"
            );
        }
    }

    #[test]
    fn update_connection_quality_normal_timer_gap_no_drop() {
        use std::thread::sleep;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        {
            let mut state = shared.slots[0].write();
            // First report.
            loop_instance.update_connection_quality(&mut state, 0x01);
            sleep(Duration::from_millis(5));
            // Second report with normal gap (delta 1).
            loop_instance.update_connection_quality(&mut state, 0x02);
            assert_eq!(state.connection_quality.dropped, 0);
            sleep(Duration::from_millis(5));
            // Third report with delta 2 (still within 0..=2 range).
            loop_instance.update_connection_quality(&mut state, 0x04);
            assert_eq!(state.connection_quality.dropped, 0);
        }
    }

    #[test]
    fn update_connection_quality_wrapping_timer() {
        use std::thread::sleep;

        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        {
            let mut state = shared.slots[0].write();
            // Timer near wraparound: 0xFE → 0x00 (wrapping_sub gives 2, within range).
            loop_instance.update_connection_quality(&mut state, 0xFE);
            sleep(Duration::from_millis(5));
            loop_instance.update_connection_quality(&mut state, 0x00);
            assert_eq!(state.connection_quality.dropped, 0);
        }
    }

    // -----------------------------------------------------------------------
    // emit_device_info — connection type string mapping
    // -----------------------------------------------------------------------

    #[test]
    fn emit_device_info_sets_connection_string() {
        use crate::state::{ConnectionType, DeviceInfo};

        let shared = SharedState::new();
        let (tx, mut rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        // Set up state with device info and USB connection.
        {
            let mut state = shared.slots[0].write();
            state.device_info = Some(DeviceInfo {
                firmware_version: "1.0".into(),
                ..Default::default()
            });
            state.connection_type = ConnectionType::Usb;
        }

        let state = shared.slots[0].read().clone();
        loop_instance.emit_device_info(&state);

        // Check that a DeviceInfo event was emitted with "USB" connection.
        let mut found = false;
        while let Ok(ev) = rx.try_recv() {
            if let crate::state::IpcEvent::DeviceInfo { data } = ev {
                assert_eq!(data.connection, "USB");
                found = true;
            }
        }
        assert!(found, "DeviceInfo event should be emitted");
    }

    #[test]
    fn emit_device_info_bluetooth_connection_string() {
        use crate::state::{ConnectionType, DeviceInfo};

        let shared = SharedState::new();
        let (tx, mut rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        {
            let mut state = shared.slots[0].write();
            state.device_info = Some(DeviceInfo::default());
            state.connection_type = ConnectionType::Bluetooth;
        }

        let state = shared.slots[0].read().clone();
        loop_instance.emit_device_info(&state);

        let mut found = false;
        while let Ok(ev) = rx.try_recv() {
            if let crate::state::IpcEvent::DeviceInfo { data } = ev {
                assert_eq!(data.connection, "Bluetooth");
                found = true;
            }
        }
        assert!(found, "DeviceInfo event should be emitted for Bluetooth");
    }

    #[test]
    fn emit_device_info_no_event_when_device_info_is_none() {
        let shared = SharedState::new();
        let (tx, mut rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        // device_info is None by default — should not emit.
        let state = shared.slots[0].read().clone();
        loop_instance.emit_device_info(&state);

        assert!(rx.try_recv().is_err(), "no event should be emitted");
    }

    #[test]
    fn emit_device_info_marks_calibration_when_stick_cal_present() {
        use crate::state::{DeviceInfo, SpiInfo, StickCalibration};

        let shared = SharedState::new();
        let (tx, mut rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 0, None);

        {
            let mut state = shared.slots[0].write();
            state.device_info = Some(DeviceInfo {
                spi: Some(SpiInfo::default()),
                ..Default::default()
            });
            state.stick_calibration = Some(StickCalibration {
                valid: true,
                ..Default::default()
            });
        }

        let state = shared.slots[0].read().clone();
        loop_instance.emit_device_info(&state);

        let mut found = false;
        while let Ok(ev) = rx.try_recv() {
            if let crate::state::IpcEvent::DeviceInfo { data } = ev {
                if let Some(spi) = data.spi {
                    assert!(spi.calibration, "calibration should be true");
                    found = true;
                }
            }
        }
        assert!(found, "DeviceInfo with calibration flag should be emitted");
    }

    // -----------------------------------------------------------------------
    // IMU processing math — raw_to_physical with mock IMU data
    // -----------------------------------------------------------------------

    #[test]
    fn imu_raw_to_physical_default_scales() {
        use crate::hid_parser::ImuFrame;
        use crate::imu;

        let frame = ImuFrame {
            accel_x: 4096,
            accel_y: 0,
            accel_z: 4096,
            gyro_x: 13371,
            gyro_y: 0,
            gyro_z: 0,
        };
        let physical = imu::raw_to_physical(&frame);
        // accel_z = 4096 * (1/4096) = 1.0 g (gravity)
        assert!((physical.accel_z - 1.0).abs() < 1e-6, "accel_z should be ~1.0g");
        // gyro_x = 13371 * (1/13371) = 1.0 deg/s
        assert!((physical.gyro_x - 1.0).abs() < 1e-6, "gyro_x should be ~1.0 dps");
        // accel_y = 0
        assert!((physical.accel_y - 0.0).abs() < 1e-6);
    }

    #[test]
    fn imu_raw_to_physical_calibrated_with_factory_cal() {
        use crate::hid_parser::ImuFrame;
        use crate::imu;
        use crate::state::ImuCalibration;

        let frame = ImuFrame {
            accel_x: 1000,
            accel_y: 0,
            accel_z: 4096,
            gyro_x: 100,
            gyro_y: 0,
            gyro_z: 0,
        };
        // Realistic factory calibration values.
        let cal = ImuCalibration {
            accel_origin: [0, 0, 0],
            accel_sensitivity: [16384, 16384, 16384],
            gyro_origin: [0, 0, 0],
            gyro_sensitivity: [13371, 13371, 13371],
            ..Default::default()
        };
        let physical = imu::raw_to_physical_calibrated(&frame, &cal);
        // accel_x = 1000 * (1/(16384-0) * 4) = 1000 * 4/16384 ≈ 0.244
        let expected_ax = 1000.0 * 4.0 / 16384.0;
        assert!(
            (physical.accel_x - expected_ax).abs() < 1e-4,
            "accel_x should be ~{}, got {}",
            expected_ax,
            physical.accel_x
        );
        // gyro_x = (100 - 0) * (936 / (13371 - 0)) ≈ 7.0
        let expected_gx = 100.0 * 936.0 / 13371.0;
        assert!(
            (physical.gyro_x - expected_gx).abs() < 1e-2,
            "gyro_x should be ~{}, got {}",
            expected_gx,
            physical.gyro_x
        );
    }

    #[test]
    fn imu_raw_to_physical_calibrated_degenerate_falls_back() {
        use crate::hid_parser::ImuFrame;
        use crate::imu;
        use crate::state::ImuCalibration;

        let frame = ImuFrame {
            accel_x: 4096,
            accel_y: 0,
            accel_z: 4096,
            gyro_x: 13371,
            gyro_y: 0,
            gyro_z: 0,
        };
        // Degenerate calibration: sensitivity == origin → should use default scale.
        let cal = ImuCalibration {
            accel_origin: [100, 100, 100],
            accel_sensitivity: [100, 100, 100], // diff < 10 → fallback
            gyro_origin: [200, 200, 200],
            gyro_sensitivity: [200, 200, 200], // diff < 10 → fallback
            ..Default::default()
        };
        let physical = imu::raw_to_physical_calibrated(&frame, &cal);
        // Should match the uncalibrated default.
        let default_physical = imu::raw_to_physical(&frame);
        assert!((physical.accel_x - default_physical.accel_x).abs() < 1e-6);
        assert!((physical.gyro_x - default_physical.gyro_x).abs() < 1e-6);
    }

    #[test]
    fn imu_calculate_tilt_flat_is_zero() {
        use crate::hid_parser::ImuFrame;
        use crate::imu;

        // Flat: gravity on Z, no tilt.
        let frame = ImuFrame {
            accel_x: 0,
            accel_y: 0,
            accel_z: 4096,
            gyro_x: 0,
            gyro_y: 0,
            gyro_z: 0,
        };
        let physical = imu::raw_to_physical(&frame);
        let (pitch, roll) = imu::calculate_tilt(&physical);
        assert!((pitch - 0.0).abs() < 1e-3, "pitch should be ~0, got {}", pitch);
        assert!((roll - 0.0).abs() < 1e-3, "roll should be ~0, got {}", roll);
    }

    #[test]
    fn imu_calculate_tilt_forward_pitch() {
        use crate::hid_parser::ImuFrame;
        use crate::imu;

        // Tilt forward: gravity on Y (positive pitch).
        let frame = ImuFrame {
            accel_x: 0,
            accel_y: 4096,
            accel_z: 0,
            gyro_x: 0,
            gyro_y: 0,
            gyro_z: 0,
        };
        let physical = imu::raw_to_physical(&frame);
        let (pitch, _roll) = imu::calculate_tilt(&physical);
        // pitch = atan2(y, z) = atan2(1.0, 0.0) = 90 degrees
        assert!((pitch - 90.0).abs() < 1e-2, "pitch should be ~90, got {}", pitch);
    }

    #[test]
    fn imu_tilt_estimator_converges() {
        use crate::hid_parser::ImuFrame;
        use crate::imu::{self, TiltEstimator};

        let mut estimator = TiltEstimator::new(0.98);
        // Simulate a flat position for many iterations — should converge to ~0.
        let frame = ImuFrame {
            accel_x: 0,
            accel_y: 0,
            accel_z: 4096,
            gyro_x: 0,
            gyro_y: 0,
            gyro_z: 0,
        };
        let dt = 1.0 / 180.0;
        for _ in 0..100 {
            let physical = imu::raw_to_physical(&frame);
            estimator.update(&physical, &physical, dt);
        }
        let (pitch, roll) = estimator.get_tilt();
        assert!((pitch - 0.0).abs() < 1.0, "pitch should converge to ~0, got {}", pitch);
        assert!((roll - 0.0).abs() < 1.0, "roll should converge to ~0, got {}", roll);
    }

    // -----------------------------------------------------------------------
    // set_connected — slot state management
    // -----------------------------------------------------------------------

    #[test]
    fn set_connected_true_sets_slot_active() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 1, None);

        assert!(!shared.is_slot_active(1));
        loop_instance.set_connected(true);
        assert!(shared.is_slot_active(1));

        // Clean up.
        loop_instance.set_connected(false);
        assert!(!shared.is_slot_active(1));
    }

    #[test]
    fn set_connected_false_without_claimed_path_is_safe() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared.clone(), tx, 2, None);

        // No claimed path set — should not panic.
        loop_instance.set_connected(false);
        assert!(!shared.is_slot_active(2));
    }

    // -----------------------------------------------------------------------
    // DeviceLoop construction — manager vs worker slot
    // -----------------------------------------------------------------------

    #[test]
    fn device_loop_new_creates_manager_slot() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::new(shared, tx);
        assert_eq!(loop_instance.slot, super::MANAGER_SLOT);
    }

    #[test]
    fn device_loop_with_slot_creates_worker_slot() {
        let shared = SharedState::new();
        let (tx, _rx) = broadcast::channel(64);
        let loop_instance = DeviceLoop::with_slot(shared, tx, 2, None);
        assert_eq!(loop_instance.slot, 2);
    }
}
