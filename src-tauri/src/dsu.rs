//! Cemuhook/DSU UDP motion server.
//!
//! Exposes Pro Controller IMU data to emulators (Cemu, Dolphin, Ryujinx, etc.)
//! over the standard DSU protocol on UDP port 26760.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};

use crate::hid_parser::ImuData;
use crate::imu::{raw_to_physical, raw_to_physical_calibrated, ImuPhysical};
use crate::state::{ButtonState, ConnectionType, ControllerState, SharedState, StickState};

const PROTOCOL_VERSION: u16 = 1001;
const MSG_VERSION: u32 = 0x100000;
const MSG_PORT_INFO: u32 = 0x100001;
const MSG_PAD_DATA: u32 = 0x100002;
const SERVER_MAGIC: &[u8; 4] = b"DSUS";
const CLIENT_MAGIC: &[u8; 4] = b"DSUC";
const DEFAULT_SERVER_ID: u32 = 0x4F584944; // "OXID"
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const SLOT_COUNT: usize = 4;

/// DSU server runtime handle and Tauri command facade.
#[derive(Clone)]
pub struct DsuManager {
    shared: Arc<SharedState>,
    inner: Arc<Mutex<DsuManagerInner>>,
}

struct DsuManagerInner {
    server: Option<DsuServer>,
    handles: Option<(JoinHandle<()>, JoinHandle<()>)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DsuStatus {
    pub running: bool,
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub update_rate_hz: u32,
}

impl DsuManager {
    pub fn new(shared: Arc<SharedState>) -> Self {
        Self {
            shared,
            inner: Arc::new(Mutex::new(DsuManagerInner {
                server: None,
                handles: None,
            })),
        }
    }

    /// Start the DSU server if it is not already running.
    pub async fn start(&self) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.server.is_some() {
            return false;
        }

        let cfg = self.shared.config.read().dsu.clone();
        let bind = format!("{}:{}", cfg.bind_address, cfg.port);
        let server = match DsuServer::new(self.shared.clone(), &bind).await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Failed to bind DSU server to {}: {}", bind, e);
                return false;
            }
        };

        let handles = server.run(cfg.update_rate_hz);
        log::info!(
            "DSU server started on {} at {} Hz",
            bind,
            cfg.update_rate_hz
        );
        inner.server = Some(server);
        inner.handles = Some(handles);
        true
    }

    /// Stop the running DSU server.
    pub async fn stop(&self) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.server.is_none() {
            return false;
        }
        if let Some((recv, send)) = inner.handles.take() {
            recv.abort();
            send.abort();
        }
        inner.server = None;
        log::info!("DSU server stopped");
        true
    }

    /// Get the current DSU server status.
    pub async fn status(&self) -> DsuStatus {
        let inner = self.inner.lock().await;
        let cfg = self.shared.config.read().dsu.clone();
        DsuStatus {
            running: inner.server.is_some(),
            enabled: cfg.enabled,
            bind_address: cfg.bind_address,
            port: cfg.port,
            update_rate_hz: cfg.update_rate_hz,
        }
    }
}

/// Tauri command: start the DSU server.
#[tauri::command]
pub async fn dsu_start(manager: tauri::State<'_, DsuManager>) -> Result<bool, String> {
    Ok(manager.start().await)
}

/// Tauri command: stop the DSU server.
#[tauri::command]
pub async fn dsu_stop(manager: tauri::State<'_, DsuManager>) -> Result<bool, String> {
    Ok(manager.stop().await)
}

/// Tauri command: get DSU server status.
#[tauri::command]
pub async fn dsu_get_status(manager: tauri::State<'_, DsuManager>) -> Result<DsuStatus, String> {
    Ok(manager.status().await)
}

/// Per-client subscription and activity tracking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Subscription {
    All,
    Slot(u8),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Client {
    pub(crate) last_seen: Instant,
    pub(crate) subscription: Subscription,
}

impl Client {
    pub(crate) fn new(subscription: Subscription) -> Self {
        Self {
            last_seen: Instant::now(),
            subscription,
        }
    }

    pub(crate) fn is_active(&self, now: Instant) -> bool {
        now.duration_since(self.last_seen) < CLIENT_TIMEOUT
    }

    pub(crate) fn wants_slot(&self, slot: u8) -> bool {
        match self.subscription {
            Subscription::All => true,
            Subscription::Slot(s) => s == slot,
        }
    }
}

/// UDP DSU server.
#[derive(Clone)]
pub struct DsuServer {
    socket: Arc<UdpSocket>,
    clients: Arc<Mutex<HashMap<SocketAddr, Client>>>,
    counter: Arc<AtomicU32>,
    server_id: u32,
    shared: Arc<SharedState>,
}

impl DsuServer {
    pub async fn new(shared: Arc<SharedState>, bind: &str) -> std::io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        Ok(Self {
            socket,
            clients: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(AtomicU32::new(0)),
            server_id: DEFAULT_SERVER_ID,
            shared,
        })
    }

    /// Spawn receive and send loops.
    pub fn run(&self, update_rate_hz: u32) -> (JoinHandle<()>, JoinHandle<()>) {
        let recv_server = self.clone();
        let recv_handle = tokio::spawn(async move { recv_server.recv_loop().await });

        let send_server = self.clone();
        let send_handle = tokio::spawn(async move { send_server.send_loop(update_rate_hz).await });

        (recv_handle, send_handle)
    }

    async fn recv_loop(self) {
        let mut buf = [0u8; 1024];
        loop {
            match self.socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    if len < 20 {
                        continue;
                    }
                    self.handle_packet(&buf[..len], src).await;
                }
                Err(e) => {
                    log::warn!("DSU recv error: {}", e);
                }
            }
        }
    }

    async fn handle_packet(&self, data: &[u8], src: SocketAddr) {
        if &data[0..4] != CLIENT_MAGIC {
            return;
        }
        if !verify_crc(data) {
            return;
        }

        let msg_type = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);

        match msg_type {
            MSG_VERSION => {
                let _ = self
                    .socket
                    .send_to(&build_version_reply(self.server_id), src)
                    .await;
            }
            MSG_PORT_INFO => {
                self.handle_port_info(data, src).await;
            }
            MSG_PAD_DATA => {
                self.handle_pad_request(data, src).await;
            }
            _ => {}
        }
    }

    async fn handle_port_info(&self, data: &[u8], src: SocketAddr) {
        let payload = &data[20..];
        if payload.len() < 4 {
            return;
        }
        let count = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        let slots = if payload.len() > 4 {
            &payload[4..]
        } else {
            &[]
        };

        let controller = self.shared.active_controller().clone();

        if count == 0 || slots.is_empty() {
            for slot in 0..SLOT_COUNT {
                let packet = build_port_info(slot as u8, &controller, self.server_id);
                let _ = self.socket.send_to(&packet, src).await;
            }
        } else {
            for (i, &slot_byte) in slots.iter().enumerate().take(count) {
                let _ = i;
                if slot_byte >= SLOT_COUNT as u8 {
                    continue;
                }
                let packet = build_port_info(slot_byte, &controller, self.server_id);
                let _ = self.socket.send_to(&packet, src).await;
            }
        }
    }

    async fn handle_pad_request(&self, data: &[u8], src: SocketAddr) {
        if data.len() < 28 {
            return;
        }
        let payload = &data[20..];
        let reg_flags = payload[0];
        let slot = payload[1];
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&payload[2..8]);

        let mut sub = Subscription::All;

        if (reg_flags & 0x01) != 0 && slot < SLOT_COUNT as u8 {
            sub = Subscription::Slot(slot);
        } else if (reg_flags & 0x02) != 0 {
            // MAC-based registration: find a slot with matching MAC.
            let controller = self.shared.active_controller().clone();
            let info = slot_info(&controller, 0);
            if info.mac == mac {
                sub = Subscription::Slot(0);
            }
        }

        let mut clients = self.clients.lock().await;
        clients.insert(src, Client::new(sub));
    }

    async fn send_loop(self, update_rate_hz: u32) {
        if update_rate_hz == 0 {
            return;
        }
        let period = Duration::from_secs_f64(1.0 / update_rate_hz.max(1) as f64);
        let mut ticker = interval(period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            let now = Instant::now();

            let controller = self.shared.active_controller().clone();
            let imu = controller
                .imu
                .as_ref()
                .map(|imu| physical_from_imu(imu, controller.imu_calibration.as_ref()));

            for slot in 0..SLOT_COUNT {
                if !slot_connected(&controller, slot) {
                    continue;
                }
                let counter = self.counter.fetch_add(1, Ordering::Relaxed);
                let packet = build_pad_data(
                    slot as u8,
                    counter,
                    &controller,
                    imu.as_ref(),
                    self.server_id,
                );

                let addrs: Vec<SocketAddr> = {
                    let clients = self.clients.lock().await;
                    clients
                        .iter()
                        .filter(|(_, c)| c.is_active(now) && c.wants_slot(slot as u8))
                        .map(|(a, _)| *a)
                        .collect()
                };

                for addr in addrs {
                    let _ = self.socket.send_to(&packet, addr).await;
                }
            }

            {
                let mut clients = self.clients.lock().await;
                clients.retain(|_, c| c.is_active(now));
            }
        }
    }
}

/// Compute the physical IMU reading for the latest frame.
fn physical_from_imu(
    imu: &ImuData,
    calibration: Option<&crate::state::ImuCalibration>,
) -> ImuPhysical {
    // Use the newest of the three frames in a standard report.
    let frame = &imu.frames[2];
    if let Some(cal) = calibration {
        raw_to_physical_calibrated(frame, cal)
    } else {
        raw_to_physical(frame)
    }
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

pub(crate) fn verify_crc(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    let expected = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    let mut copy = data.to_vec();
    copy[8..12].fill(0);
    crc32fast::hash(&copy) == expected
}

fn set_crc32(data: &mut [u8]) {
    data[8..12].fill(0);
    let c = crc32fast::hash(data);
    data[8] = (c & 0xFF) as u8;
    data[9] = ((c >> 8) & 0xFF) as u8;
    data[10] = ((c >> 16) & 0xFF) as u8;
    data[11] = ((c >> 24) & 0xFF) as u8;
}

fn build_header(msg_type: u32, payload_len: u16, server_id: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(20);
    h.extend_from_slice(SERVER_MAGIC);
    h.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    h.extend_from_slice(&payload_len.to_le_bytes());
    h.extend_from_slice(&[0u8; 4]);
    h.extend_from_slice(&server_id.to_le_bytes());
    h.extend_from_slice(&msg_type.to_le_bytes());
    h
}

pub fn build_version_reply(server_id: u32) -> Vec<u8> {
    let mut p = build_header(MSG_VERSION, 2, server_id);
    p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    set_crc32(&mut p);
    p
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SlotInfo {
    pub(crate) state: u8,
    pub(crate) model: u8,
    pub(crate) connection_type: u8,
    pub(crate) mac: [u8; 6],
    pub(crate) battery: u8,
}

fn slot_connected(controller: &ControllerState, slot: usize) -> bool {
    slot == 0 && controller.connected
}

fn slot_info(controller: &ControllerState, slot: usize) -> SlotInfo {
    if slot != 0 || !controller.connected {
        return SlotInfo::default();
    }
    SlotInfo {
        state: 2,
        model: 2,
        connection_type: match controller.connection_type {
            ConnectionType::Usb => 1,
            ConnectionType::Bluetooth => 2,
        },
        mac: controller
            .device_info
            .as_ref()
            .map(|info| parse_mac_str(&info.mac_address))
            .unwrap_or([0; 6]),
        battery: battery_to_dsu(controller.battery_percent, controller.charging),
    }
}

pub fn build_port_info(slot: u8, controller: &ControllerState, server_id: u32) -> Vec<u8> {
    let info = slot_info(controller, slot as usize);
    let mut p = build_header(MSG_PORT_INFO, 12, server_id);
    p.push(slot);
    p.push(info.state);
    p.push(info.model);
    p.push(info.connection_type);
    p.extend_from_slice(&info.mac);
    p.push(info.battery);
    p.push(0);
    set_crc32(&mut p);
    p
}

pub fn build_pad_data(
    slot: u8,
    counter: u32,
    controller: &ControllerState,
    imu: Option<&ImuPhysical>,
    server_id: u32,
) -> Vec<u8> {
    let info = slot_info(controller, slot as usize);
    let mut p = build_header(MSG_PAD_DATA, 80, server_id);

    // Shared 11-byte controller header.
    p.push(slot);
    p.push(info.state);
    p.push(info.model);
    p.push(info.connection_type);
    p.extend_from_slice(&info.mac);
    p.push(info.battery);

    // Pad-data-specific fields.
    p.push(if controller.connected { 1 } else { 0 });
    p.extend_from_slice(&counter.to_le_bytes());

    let (digital1, digital2) = encode_buttons(&controller.buttons);
    p.push(digital1);
    p.push(digital2);
    p.push(if controller.buttons.home { 1 } else { 0 });
    p.push(0); // touch button

    p.extend_from_slice(&encode_sticks(
        &controller.left_stick,
        &controller.right_stick,
    ));
    p.extend_from_slice(&encode_analog(
        &controller.buttons,
        controller.left_trigger,
        controller.right_trigger,
    ));
    p.extend_from_slice(&[0u8; 12]); // touch data

    p.extend_from_slice(&now_micros().to_le_bytes());

    if let Some(imu) = imu {
        p.extend_from_slice(&imu.accel_x.to_le_bytes());
        p.extend_from_slice(&imu.accel_y.to_le_bytes());
        p.extend_from_slice(&imu.accel_z.to_le_bytes());
        p.extend_from_slice(&imu.gyro_x.to_le_bytes());
        p.extend_from_slice(&imu.gyro_y.to_le_bytes());
        p.extend_from_slice(&imu.gyro_z.to_le_bytes());
    } else {
        for _ in 0..6 {
            p.extend_from_slice(&0f32.to_le_bytes());
        }
    }

    set_crc32(&mut p);
    p
}

fn parse_mac_str(mac: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    let cleaned: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    for (i, byte) in out.iter_mut().enumerate() {
        let start = i * 2;
        if start + 2 <= cleaned.len() {
            if let Ok(v) = u8::from_str_radix(&cleaned[start..start + 2], 16) {
                *byte = v;
            }
        }
    }
    out
}

fn battery_to_dsu(percent: u8, charging: bool) -> u8 {
    if charging && percent >= 99 {
        0xEF
    } else if charging {
        0xEE
    } else if percent >= 80 {
        0x05
    } else if percent >= 60 {
        0x04
    } else if percent >= 40 {
        0x03
    } else if percent >= 20 {
        0x02
    } else {
        0x01
    }
}

fn encode_buttons(buttons: &ButtonState) -> (u8, u8) {
    let mut byte1 = 0u8;
    if buttons.dpad_left {
        byte1 |= 0b1000_0000;
    }
    if buttons.dpad_down {
        byte1 |= 0b0100_0000;
    }
    if buttons.dpad_right {
        byte1 |= 0b0010_0000;
    }
    if buttons.dpad_up {
        byte1 |= 0b0001_0000;
    }
    if buttons.plus {
        byte1 |= 0b0000_1000;
    }
    if buttons.stick_r {
        byte1 |= 0b0000_0100;
    }
    if buttons.stick_l {
        byte1 |= 0b0000_0010;
    }
    if buttons.minus {
        byte1 |= 0b0000_0001;
    }

    let mut byte2 = 0u8;
    if buttons.y {
        byte2 |= 0b1000_0000;
    }
    if buttons.b {
        byte2 |= 0b0100_0000;
    }
    if buttons.a {
        byte2 |= 0b0010_0000;
    }
    if buttons.x {
        byte2 |= 0b0001_0000;
    }
    if buttons.r {
        byte2 |= 0b0000_1000;
    }
    if buttons.l {
        byte2 |= 0b0000_0100;
    }
    if buttons.zr {
        byte2 |= 0b0000_0010;
    }
    if buttons.zl {
        byte2 |= 0b0000_0001;
    }

    (byte1, byte2)
}

fn encode_sticks(left: &StickState, right: &StickState) -> [u8; 4] {
    [
        f32_to_u8_stick(left.x),
        f32_to_u8_stick(left.y),
        f32_to_u8_stick(right.x),
        f32_to_u8_stick(right.y),
    ]
}

fn f32_to_u8_stick(v: f32) -> u8 {
    let clamped = v.clamp(-1.0, 1.0);
    let normalized = (clamped + 1.0) / 2.0;
    (normalized * 255.0).round() as u8
}

fn u8_from_bool(b: bool) -> u8 {
    if b {
        255
    } else {
        0
    }
}

fn scale_trigger(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn encode_analog(buttons: &ButtonState, left_trigger: f32, right_trigger: f32) -> [u8; 12] {
    [
        u8_from_bool(buttons.dpad_left),
        u8_from_bool(buttons.dpad_down),
        u8_from_bool(buttons.dpad_right),
        u8_from_bool(buttons.dpad_up),
        u8_from_bool(buttons.y),
        u8_from_bool(buttons.b),
        u8_from_bool(buttons.a),
        u8_from_bool(buttons.x),
        u8_from_bool(buttons.r),
        u8_from_bool(buttons.l),
        scale_trigger(right_trigger),
        scale_trigger(left_trigger),
    ]
}

#[cfg(test)]
mod dsu_unit_tests {
    use super::*;

    #[test]
    fn version_reply_is_valid() {
        let p = build_version_reply(0x12345678);
        assert_eq!(&p[0..4], SERVER_MAGIC);
        assert_eq!(u16::from_le_bytes([p[4], p[5]]), PROTOCOL_VERSION);
        assert_eq!(u16::from_le_bytes([p[6], p[7]]), 2);
        assert_eq!(
            u32::from_le_bytes([p[16], p[17], p[18], p[19]]),
            MSG_VERSION
        );
        assert!(verify_crc(&p));
    }

    #[test]
    fn crc32_zeroes_and_verifies() {
        let mut p = build_header(MSG_VERSION, 2, DEFAULT_SERVER_ID);
        p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        set_crc32(&mut p);
        assert!(verify_crc(&p));
        // Corrupt a byte and fail verification.
        p[20] = p[20].wrapping_add(1);
        assert!(!verify_crc(&p));
    }
}
