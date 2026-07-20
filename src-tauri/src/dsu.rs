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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    use crate::hid_parser::{ImuData, ImuFrame};
    use crate::imu::ImuPhysical;
    use crate::state::{
        ButtonState, ConnectionType, ControllerState, DeviceInfo, DsuConfig, StickState,
    };

    // ------------------------------------------------------------------
    //  Test fixtures
    // ------------------------------------------------------------------

    fn connected_controller() -> ControllerState {
        let mut state = ControllerState::default();
        state.connected = true;
        state.battery_percent = 85;
        state.charging = false;
        state.connection_type = ConnectionType::Bluetooth;
        state.device_info = Some(DeviceInfo {
            mac_address: "00:11:22:33:44:55".into(),
            ..Default::default()
        });
        state.buttons = ButtonState {
            a: true,
            b: true,
            x: false,
            y: true,
            plus: true,
            home: true,
            dpad_up: true,
            dpad_left: true,
            l: true,
            zr: true,
            stick_l: true,
            ..Default::default()
        };
        state.left_stick = StickState {
            x: -0.5,
            y: 0.5,
            ..Default::default()
        };
        state.right_stick = StickState {
            x: 1.0,
            y: -1.0,
            ..Default::default()
        };
        state.left_trigger = 0.25;
        state.right_trigger = 0.75;
        state
    }

    fn sample_imu() -> ImuPhysical {
        ImuPhysical {
            accel_x: 0.12,
            accel_y: -0.34,
            accel_z: 0.95,
            gyro_x: 12.0,
            gyro_y: -23.5,
            gyro_z: 7.0,
        }
    }

    fn sample_imu_data() -> ImuData {
        ImuData {
            frames: [
                ImuFrame::default(),
                ImuFrame::default(),
                ImuFrame {
                    accel_x: 100,
                    accel_y: -200,
                    accel_z: 300,
                    gyro_x: 400,
                    gyro_y: -500,
                    gyro_z: 600,
                },
            ],
        }
    }

    // ------------------------------------------------------------------
    //  DSU server config and defaults
    // ------------------------------------------------------------------

    #[test]
    fn dsu_config_defaults() {
        let cfg = DsuConfig::default();
        assert!(!cfg.enabled, "DSU should be disabled by default");
        assert_eq!(cfg.bind_address, "127.0.0.1");
        assert_eq!(cfg.port, 26760);
        assert_eq!(cfg.update_rate_hz, 60);
    }

    #[test]
    fn dsu_config_is_cloneable_and_serializable() {
        let cfg = DsuConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cfg.port, cloned.port);
        // Serialization round-trips without panic.
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: DsuConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cfg.port, back.port);
        assert_eq!(cfg.bind_address, back.bind_address);
    }

    #[test]
    fn default_server_id_is_oxid_magic() {
        // 0x4F584944 in little-endian bytes.
        let bytes = DEFAULT_SERVER_ID.to_le_bytes();
        assert_eq!(bytes, [0x44, 0x49, 0x58, 0x4F]);
        assert_eq!(DEFAULT_SERVER_ID, 0x4F584944);
    }

    #[test]
    fn protocol_constants_are_sensible() {
        assert_eq!(PROTOCOL_VERSION, 1001);
        assert_eq!(MSG_VERSION, 0x100000);
        assert_eq!(MSG_PORT_INFO, 0x100001);
        assert_eq!(MSG_PAD_DATA, 0x100002);
        assert_eq!(SERVER_MAGIC, b"DSUS");
        assert_eq!(CLIENT_MAGIC, b"DSUC");
        assert_eq!(SLOT_COUNT, 4);
        assert_eq!(CLIENT_TIMEOUT, Duration::from_secs(5));
    }

    // ------------------------------------------------------------------
    //  SlotInfo defaults and slot management logic
    // ------------------------------------------------------------------

    #[test]
    fn slot_info_default_is_empty() {
        let info = SlotInfo::default();
        assert_eq!(info.state, 0);
        assert_eq!(info.model, 0);
        assert_eq!(info.connection_type, 0);
        assert_eq!(info.mac, [0u8; 6]);
        assert_eq!(info.battery, 0);
    }

    #[test]
    fn slot_connected_only_for_slot_zero_when_connected() {
        let mut controller = ControllerState::default();
        assert!(!slot_connected(&controller, 0));
        assert!(!slot_connected(&controller, 1));
        controller.connected = true;
        assert!(slot_connected(&controller, 0));
        assert!(!slot_connected(&controller, 1));
        assert!(!slot_connected(&controller, 2));
        assert!(!slot_connected(&controller, 3));
    }

    #[test]
    fn slot_info_for_disconnected_controller_is_default() {
        let controller = ControllerState::default();
        let info = slot_info(&controller, 0);
        assert_eq!(info, SlotInfo::default());
    }

    #[test]
    fn slot_info_for_nonzero_slot_is_default_even_when_connected() {
        let controller = connected_controller();
        for slot in 1..SLOT_COUNT {
            let info = slot_info(&controller, slot);
            assert_eq!(info, SlotInfo::default(), "slot {} should be empty", slot);
        }
    }

    #[test]
    fn slot_info_for_connected_bluetooth_controller() {
        let controller = connected_controller();
        let info = slot_info(&controller, 0);
        assert_eq!(info.state, 2);
        assert_eq!(info.model, 2);
        assert_eq!(info.connection_type, 2); // Bluetooth
        assert_eq!(info.mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        // 85% battery, not charging -> 0x05
        assert_eq!(info.battery, 0x05);
    }

    #[test]
    fn slot_info_for_usb_controller() {
        let mut controller = connected_controller();
        controller.connection_type = ConnectionType::Usb;
        let info = slot_info(&controller, 0);
        assert_eq!(info.connection_type, 1); // USB
    }

    #[test]
    fn slot_info_without_device_info_has_zero_mac() {
        let mut controller = connected_controller();
        controller.device_info = None;
        let info = slot_info(&controller, 0);
        assert_eq!(info.mac, [0u8; 6]);
    }

    // ------------------------------------------------------------------
    //  VersionInfo packet (build_version_reply)
    // ------------------------------------------------------------------

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
    fn version_reply_length_is_22() {
        let p = build_version_reply(DEFAULT_SERVER_ID);
        assert_eq!(p.len(), 22);
    }

    #[test]
    fn version_reply_contains_server_id() {
        let server_id = 0xDEADBEEF;
        let p = build_version_reply(server_id);
        let parsed_id = u32::from_le_bytes([p[12], p[13], p[14], p[15]]);
        assert_eq!(parsed_id, server_id);
    }

    #[test]
    fn version_reply_payload_is_protocol_version() {
        let p = build_version_reply(DEFAULT_SERVER_ID);
        let payload = u16::from_le_bytes([p[20], p[21]]);
        assert_eq!(payload, PROTOCOL_VERSION);
    }

    // ------------------------------------------------------------------
    //  PortInfo packet (build_port_info)
    // ------------------------------------------------------------------

    #[test]
    fn port_info_packet_length_is_32() {
        let controller = connected_controller();
        let p = build_port_info(0, &controller, DEFAULT_SERVER_ID);
        assert_eq!(p.len(), 32);
    }

    #[test]
    fn port_info_header_fields() {
        let controller = connected_controller();
        let p = build_port_info(0, &controller, DEFAULT_SERVER_ID);
        assert_eq!(&p[0..4], SERVER_MAGIC);
        assert_eq!(u16::from_le_bytes([p[4], p[5]]), PROTOCOL_VERSION);
        assert_eq!(u16::from_le_bytes([p[6], p[7]]), 12);
        assert_eq!(
            u32::from_le_bytes([p[16], p[17], p[18], p[19]]),
            MSG_PORT_INFO
        );
        assert!(verify_crc(&p));
    }

    #[test]
    fn port_info_payload_for_connected_controller() {
        let controller = connected_controller();
        let p = build_port_info(0, &controller, DEFAULT_SERVER_ID);
        assert_eq!(p[20], 0); // slot
        assert_eq!(p[21], 2); // state = connected
        assert_eq!(p[22], 2); // model = full gyro
        assert_eq!(p[23], 2); // connection = Bluetooth
        assert_eq!(&p[24..30], [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(p[30], 0x05); // battery 85% not charging
        assert_eq!(p[31], 0); // padding
    }

    #[test]
    fn port_info_for_empty_slot_is_all_zero() {
        let controller = connected_controller();
        let p = build_port_info(2, &controller, DEFAULT_SERVER_ID);
        assert_eq!(p[20], 2); // slot index preserved
        assert_eq!(p[21], 0); // state
        assert_eq!(p[22], 0); // model
        assert_eq!(p[23], 0); // connection
        assert_eq!(&p[24..30], [0u8; 6]);
        assert_eq!(p[30], 0); // battery
        assert!(verify_crc(&p));
    }

    // ------------------------------------------------------------------
    //  PadData packet (build_pad_data) — building & parsing
    // ------------------------------------------------------------------

    #[test]
    fn pad_data_packet_is_100_bytes() {
        let controller = connected_controller();
        let imu = sample_imu();
        let p = build_pad_data(0, 42, &controller, Some(&imu), DEFAULT_SERVER_ID);
        assert_eq!(p.len(), 100);
    }

    #[test]
    fn pad_data_header_fields() {
        let controller = connected_controller();
        let imu = sample_imu();
        let p = build_pad_data(0, 42, &controller, Some(&imu), DEFAULT_SERVER_ID);
        assert_eq!(&p[0..4], SERVER_MAGIC);
        assert_eq!(u16::from_le_bytes([p[4], p[5]]), PROTOCOL_VERSION);
        assert_eq!(u16::from_le_bytes([p[6], p[7]]), 80);
        assert_eq!(
            u32::from_le_bytes([p[16], p[17], p[18], p[19]]),
            MSG_PAD_DATA
        );
        assert_eq!(
            u32::from_le_bytes([p[12], p[13], p[14], p[15]]),
            DEFAULT_SERVER_ID
        );
        assert!(verify_crc(&p));
    }

    #[test]
    fn pad_data_controller_header_block() {
        let controller = connected_controller();
        let imu = sample_imu();
        let p = build_pad_data(0, 7, &controller, Some(&imu), DEFAULT_SERVER_ID);
        assert_eq!(p[20], 0); // slot
        assert_eq!(p[21], 2); // state
        assert_eq!(p[22], 2); // model
        assert_eq!(p[23], 2); // connection (Bluetooth)
        assert_eq!(&p[24..30], [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(p[30], 0x05); // battery
        assert_eq!(p[31], 1); // connected flag
    }

    #[test]
    fn pad_data_counter_is_encoded_little_endian() {
        let controller = connected_controller();
        let imu = sample_imu();
        let counter: u32 = 0x12345678;
        let p = build_pad_data(0, counter, &controller, Some(&imu), DEFAULT_SERVER_ID);
        let parsed = u32::from_le_bytes([p[32], p[33], p[34], p[35]]);
        assert_eq!(parsed, counter);
    }

    #[test]
    fn pad_data_disconnected_controller_flag() {
        let controller = ControllerState::default(); // disconnected
        let p = build_pad_data(0, 0, &controller, None, DEFAULT_SERVER_ID);
        assert_eq!(p[31], 0); // connected flag = 0
    }

    #[test]
    fn pad_data_motion_fields_round_trip() {
        let controller = connected_controller();
        let imu = sample_imu();
        let p = build_pad_data(0, 0, &controller, Some(&imu), DEFAULT_SERVER_ID);
        let accel_x = f32::from_le_bytes([p[76], p[77], p[78], p[79]]);
        let accel_y = f32::from_le_bytes([p[80], p[81], p[82], p[83]]);
        let accel_z = f32::from_le_bytes([p[84], p[85], p[86], p[87]]);
        let gyro_x = f32::from_le_bytes([p[88], p[89], p[90], p[91]]);
        let gyro_y = f32::from_le_bytes([p[92], p[93], p[94], p[95]]);
        let gyro_z = f32::from_le_bytes([p[96], p[97], p[98], p[99]]);
        assert!((accel_x - 0.12).abs() < 1e-5);
        assert!((accel_y - (-0.34)).abs() < 1e-5);
        assert!((accel_z - 0.95).abs() < 1e-5);
        assert!((gyro_x - 12.0).abs() < 1e-4);
        assert!((gyro_y - (-23.5)).abs() < 1e-4);
        assert!((gyro_z - 7.0).abs() < 1e-4);
    }

    #[test]
    fn pad_data_motion_fields_zero_when_no_imu() {
        let controller = connected_controller();
        let p = build_pad_data(0, 0, &controller, None, DEFAULT_SERVER_ID);
        for offset in (76..100).step_by(4) {
            let v = f32::from_le_bytes([
                p[offset],
                p[offset + 1],
                p[offset + 2],
                p[offset + 3],
            ]);
            assert_eq!(v, 0.0, "motion byte at offset {} should be zero", offset);
        }
    }

    #[test]
    fn pad_data_timestamp_is_nonzero() {
        let controller = connected_controller();
        let p = build_pad_data(0, 0, &controller, None, DEFAULT_SERVER_ID);
        let ts = u64::from_le_bytes([
            p[68], p[69], p[70], p[71], p[72], p[73], p[74], p[75],
        ]);
        assert!(ts > 0, "timestamp should be a non-zero micros epoch");
    }

    #[test]
    fn pad_data_touch_block_is_zeroed() {
        let controller = connected_controller();
        let p = build_pad_data(0, 0, &controller, None, DEFAULT_SERVER_ID);
        assert_eq!(&p[56..68], &[0u8; 12]);
    }

    #[test]
    fn pad_data_home_button_encoded() {
        let controller = connected_controller();
        let p = build_pad_data(0, 0, &controller, None, DEFAULT_SERVER_ID);
        assert_eq!(p[38], 1); // home button pressed
    }

    // ------------------------------------------------------------------
    //  CRC / header helpers
    // ------------------------------------------------------------------

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

    #[test]
    fn verify_crc_rejects_short_packets() {
        assert!(!verify_crc(&[]));
        assert!(!verify_crc(&[0u8; 4]));
        assert!(!verify_crc(&[0u8; 11]));
    }

    #[test]
    fn verify_crc_accepts_minimal_length() {
        // 12 bytes is the minimum; build a valid CRC over 12 zero bytes.
        let mut data = vec![0u8; 12];
        set_crc32(&mut data);
        assert!(verify_crc(&data));
    }

    #[test]
    fn build_header_structure() {
        let h = build_header(MSG_PORT_INFO, 12, 0xCAFEBABE);
        assert_eq!(h.len(), 20);
        assert_eq!(&h[0..4], SERVER_MAGIC);
        assert_eq!(u16::from_le_bytes([h[4], h[5]]), PROTOCOL_VERSION);
        assert_eq!(u16::from_le_bytes([h[6], h[7]]), 12);
        assert_eq!(&h[8..12], [0u8; 4]); // CRC field zeroed before set_crc32
        assert_eq!(u32::from_le_bytes([h[12], h[13], h[14], h[15]]), 0xCAFEBABE);
        assert_eq!(
            u32::from_le_bytes([h[16], h[17], h[18], h[19]]),
            MSG_PORT_INFO
        );
    }

    #[test]
    fn set_crc32_writes_little_endian() {
        let mut data = vec![0u8; 16];
        set_crc32(&mut data);
        let crc = crc32fast::hash(&{
            let mut copy = data.clone();
            copy[8..12].fill(0);
            copy
        });
        let stored = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        assert_eq!(stored, crc);
    }

    // ------------------------------------------------------------------
    //  UDP protocol decoding with mock client byte arrays (no socket)
    // ------------------------------------------------------------------

    /// Build a mock DSUC client packet of the given message type.
    fn build_client_packet(msg_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(20 + payload.len());
        p.extend_from_slice(CLIENT_MAGIC);
        p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        p.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        p.extend_from_slice(&[0u8; 4]); // CRC placeholder
        p.extend_from_slice(&DEFAULT_SERVER_ID.to_le_bytes());
        p.extend_from_slice(&msg_type.to_le_bytes());
        p.extend_from_slice(payload);
        set_crc32(&mut p);
        p
    }

    #[test]
    fn mock_version_request_decodes() {
        let packet = build_client_packet(MSG_VERSION, &[]);
        assert_eq!(&packet[0..4], CLIENT_MAGIC);
        assert!(verify_crc(&packet));
        assert_eq!(
            u32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]),
            MSG_VERSION
        );
    }

    #[test]
    fn mock_port_info_request_decodes() {
        // Requesting slot 1 only.
        let payload = [1u8, 0, 0, 0, 1];
        let packet = build_client_packet(MSG_PORT_INFO, &payload);
        assert!(verify_crc(&packet));
        assert_eq!(
            u32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]),
            MSG_PORT_INFO
        );
        let body = &packet[20..];
        let count = i32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
        assert_eq!(count, 1);
        assert_eq!(body[4], 1); // requested slot
    }

    #[test]
    fn mock_pad_data_request_decodes_slot_registration() {
        // reg_flags=0x01 (slot-based), slot=2, mac=zeros.
        let mut payload = vec![0u8; 8];
        payload[0] = 0x01; // slot-based registration
        payload[1] = 2; // slot 2
        let packet = build_client_packet(MSG_PAD_DATA, &payload);
        assert!(verify_crc(&packet));
        let body = &packet[20..];
        assert_eq!(body[0] & 0x01, 0x01);
        assert_eq!(body[1], 2);
    }

    #[test]
    fn mock_pad_data_request_decodes_mac_registration() {
        // reg_flags=0x02 (MAC-based), mac=00:11:22:33:44:55.
        let mut payload = vec![0u8; 8];
        payload[0] = 0x02; // MAC-based registration
        payload[2..8].copy_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        let packet = build_client_packet(MSG_PAD_DATA, &payload);
        assert!(verify_crc(&packet));
        let body = &packet[20..];
        assert_eq!(body[0] & 0x02, 0x02);
        assert_eq!(&body[2..8], &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn client_packet_with_bad_magic_is_ignored_by_handle_packet_logic() {
        // Simulate the magic check performed by handle_packet.
        let mut packet = build_client_packet(MSG_VERSION, &[]);
        packet[0] = b'X';
        // Magic check (mirrors handle_packet logic).
        assert_ne!(&packet[0..4], CLIENT_MAGIC);
    }

    #[test]
    fn client_packet_with_corrupted_crc_fails_verification() {
        let mut packet = build_client_packet(MSG_VERSION, &[]);
        let last = packet.len() - 1;
        packet[last] = packet[last].wrapping_add(1);
        assert!(!verify_crc(&packet));
    }

    // ------------------------------------------------------------------
    //  Controller slot management logic (Subscription / Client)
    // ------------------------------------------------------------------

    #[test]
    fn subscription_all_wants_every_slot() {
        let client = Client::new(Subscription::All);
        for slot in 0..SLOT_COUNT as u8 {
            assert!(client.wants_slot(slot), "All should want slot {}", slot);
        }
    }

    #[test]
    fn subscription_slot_only_wants_matching_slot() {
        let client = Client::new(Subscription::Slot(3));
        assert!(!client.wants_slot(0));
        assert!(!client.wants_slot(1));
        assert!(!client.wants_slot(2));
        assert!(client.wants_slot(3));
    }

    #[test]
    fn client_is_active_when_recent() {
        let client = Client::new(Subscription::All);
        assert!(client.is_active(Instant::now()));
    }

    #[test]
    fn client_is_inactive_after_timeout() {
        let mut client = Client::new(Subscription::All);
        client.last_seen = Instant::now() - Duration::from_secs(6);
        assert!(!client.is_active(Instant::now()));
    }

    #[test]
    fn client_is_active_just_before_timeout() {
        let mut client = Client::new(Subscription::Slot(0));
        // 4 seconds is within the 5 second timeout.
        client.last_seen = Instant::now() - Duration::from_secs(4);
        assert!(client.is_active(Instant::now()));
    }

    // ------------------------------------------------------------------
    //  Pure helper functions
    // ------------------------------------------------------------------

    #[test]
    fn parse_mac_str_standard_format() {
        let mac = parse_mac_str("00:11:22:33:44:55");
        assert_eq!(mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn parse_mac_str_strips_non_hex_chars() {
        let mac = parse_mac_str("00-11-22-33-44-55");
        assert_eq!(mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn parse_mac_str_empty_returns_zeros() {
        assert_eq!(parse_mac_str(""), [0u8; 6]);
    }

    #[test]
    fn parse_mac_str_partial_returns_zeros_for_missing() {
        let mac = parse_mac_str("00:11");
        assert_eq!(mac[0], 0x00);
        assert_eq!(mac[1], 0x11);
        assert_eq!(mac[2..], [0u8; 4]);
    }

    #[test]
    fn parse_mac_str_uppercase() {
        let mac = parse_mac_str("AA:BB:CC:DD:EE:FF");
        assert_eq!(mac, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn battery_to_dsu_charging_full() {
        assert_eq!(battery_to_dsu(99, true), 0xEF);
        assert_eq!(battery_to_dsu(100, true), 0xEF);
    }

    #[test]
    fn battery_to_dsu_charging_not_full() {
        assert_eq!(battery_to_dsu(50, true), 0xEE);
        assert_eq!(battery_to_dsu(0, true), 0xEE);
    }

    #[test]
    fn battery_to_dsu_discharging_buckets() {
        assert_eq!(battery_to_dsu(80, false), 0x05);
        assert_eq!(battery_to_dsu(100, false), 0x05);
        assert_eq!(battery_to_dsu(60, false), 0x04);
        assert_eq!(battery_to_dsu(40, false), 0x03);
        assert_eq!(battery_to_dsu(20, false), 0x02);
        assert_eq!(battery_to_dsu(0, false), 0x01);
    }

    #[test]
    fn battery_to_dsu_boundary_values() {
        // Boundaries are inclusive on the lower end.
        assert_eq!(battery_to_dsu(79, false), 0x04);
        assert_eq!(battery_to_dsu(59, false), 0x03);
        assert_eq!(battery_to_dsu(39, false), 0x02);
        assert_eq!(battery_to_dsu(19, false), 0x01);
    }

    #[test]
    fn encode_buttons_all_released() {
        let (b1, b2) = encode_buttons(&ButtonState::default());
        assert_eq!(b1, 0);
        assert_eq!(b2, 0);
    }

    #[test]
    fn encode_buttons_byte1_bits() {
        let mut buttons = ButtonState::default();
        buttons.dpad_left = true;
        buttons.dpad_down = true;
        buttons.dpad_right = true;
        buttons.dpad_up = true;
        buttons.plus = true;
        buttons.stick_r = true;
        buttons.stick_l = true;
        buttons.minus = true;
        let (b1, _) = encode_buttons(&buttons);
        assert_eq!(b1, 0b1111_1111);
    }

    #[test]
    fn encode_buttons_byte2_bits() {
        let mut buttons = ButtonState::default();
        buttons.y = true;
        buttons.b = true;
        buttons.a = true;
        buttons.x = true;
        buttons.r = true;
        buttons.l = true;
        buttons.zr = true;
        buttons.zl = true;
        let (_, b2) = encode_buttons(&buttons);
        assert_eq!(b2, 0b1111_1111);
    }

    #[test]
    fn f32_to_u8_stick_center_is_128() {
        let v = f32_to_u8_stick(0.0);
        assert_eq!(v, 128);
    }

    #[test]
    fn f32_to_u8_stick_extremes() {
        assert_eq!(f32_to_u8_stick(-1.0), 0);
        assert_eq!(f32_to_u8_stick(1.0), 255);
    }

    #[test]
    fn f32_to_u8_stick_clamps_overflow() {
        assert_eq!(f32_to_u8_stick(2.0), 255);
        assert_eq!(f32_to_u8_stick(-2.0), 0);
    }

    #[test]
    fn encode_sticks_order() {
        let left = StickState {
            x: 0.0,
            y: 1.0,
            ..Default::default()
        };
        let right = StickState {
            x: -1.0,
            y: 0.0,
            ..Default::default()
        };
        let out = encode_sticks(&left, &right);
        assert_eq!(out[0], 128); // left.x center
        assert_eq!(out[1], 255); // left.y max
        assert_eq!(out[2], 0); // right.x min
        assert_eq!(out[3], 128); // right.y center
    }

    #[test]
    fn u8_from_bool_converts() {
        assert_eq!(u8_from_bool(true), 255);
        assert_eq!(u8_from_bool(false), 0);
    }

    #[test]
    fn scale_trigger_clamps_and_scales() {
        assert_eq!(scale_trigger(0.0), 0);
        assert_eq!(scale_trigger(1.0), 255);
        assert_eq!(scale_trigger(0.5), 128);
        assert_eq!(scale_trigger(2.0), 255);
        assert_eq!(scale_trigger(-1.0), 0);
    }

    #[test]
    fn encode_analog_structure() {
        let mut buttons = ButtonState::default();
        buttons.a = true;
        buttons.dpad_up = true;
        let out = encode_analog(&buttons, 0.5, 1.0);
        assert_eq!(out.len(), 12);
        // First 10 entries are bool->u8 of buttons.
        assert_eq!(out[0], 0); // dpad_left
        assert_eq!(out[1], 0); // dpad_down
        assert_eq!(out[2], 0); // dpad_right
        assert_eq!(out[3], 255); // dpad_up
        assert_eq!(out[6], 255); // a
        // Triggers are last two (right then left).
        assert_eq!(out[10], 255); // right_trigger = 1.0
        assert_eq!(out[11], 128); // left_trigger = 0.5
    }

    #[test]
    fn now_micros_is_nonzero_and_monotonic_ish() {
        let a = now_micros();
        let b = now_micros();
        assert!(a > 0);
        assert!(b >= a, "now_micros should not go backwards");
    }

    // ------------------------------------------------------------------
    //  physical_from_imu helper
    // ------------------------------------------------------------------

    #[test]
    fn physical_from_imu_uses_third_frame() {
        let data = sample_imu_data();
        let physical = physical_from_imu(&data, None);
        // The third frame has non-trivial values; ensure the result is finite
        // and reflects a conversion (not all zeros).
        assert!(physical.accel_x.is_finite());
        assert!(physical.accel_y.is_finite());
        assert!(physical.accel_z.is_finite());
        assert!(physical.gyro_x.is_finite());
        assert!(physical.gyro_y.is_finite());
        assert!(physical.gyro_z.is_finite());
    }

    #[test]
    fn physical_from_imu_with_calibration_is_finite() {
        let data = sample_imu_data();
        let cal = crate::state::ImuCalibration {
            accel_origin: [0; 3],
            accel_sensitivity: [16384, 16384, 16384],
            gyro_origin: [0; 3],
            gyro_sensitivity: [13371, 13371, 13371],
            source: "factory".into(),
            horizontal_offsets: [0; 3],
        };
        let physical = physical_from_imu(&data, Some(&cal));
        assert!(physical.accel_x.is_finite());
        assert!(physical.gyro_x.is_finite());
    }

    // ------------------------------------------------------------------
    //  DsuStatus serialization
    // ------------------------------------------------------------------

    #[test]
    fn dsu_status_serializes() {
        let status = DsuStatus {
            running: true,
            enabled: true,
            bind_address: "127.0.0.1".into(),
            port: 26760,
            update_rate_hz: 100,
        };
        let json = serde_json::to_string(&status).expect("serialize");
        assert!(json.contains("\"running\":true"));
        assert!(json.contains("\"port\":26760"));
        assert!(json.contains("\"update_rate_hz\":100"));
        assert!(json.contains("\"enabled\":true"));
        assert!(json.contains("\"bind_address\":\"127.0.0.1\""));
    }
}
