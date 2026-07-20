use crate::kbm::KbmEmulator;
use crate::macro_engine::MacroEngine;
use crate::vixinput::VirtualXInput;
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Maximum number of simultaneous controllers supported by OxideLink.
pub const CONTROLLER_SLOTS: usize = 4;

#[path = "flick_stick.rs"]
pub mod flick_stick;

/// Gate calibration sweep collector.
///
/// When `active`, the device loop feeds normalized stick samples into `samples`.
/// Once enough samples are collected (or the sweep is stopped), the device loop
/// runs `GateCalibration::calibrate()` on both stick pipelines and sets `done`.
#[derive(Default)]
pub struct GateCalibrationCollector {
    pub active: bool,
    pub done: bool,
    pub samples: Vec<(f32, f32)>,
}

impl GateCalibrationCollector {
    /// Minimum samples needed before auto-completing.
    const MIN_SAMPLES: usize = 500;

    /// Start a new sweep — clears any previous data.
    pub fn start(&mut self) {
        self.active = true;
        self.done = false;
        self.samples.clear();
    }

    /// Add a sample (only if active and not yet done).
    pub fn add(&mut self, x: f32, y: f32) {
        if self.active && !self.done {
            self.samples.push((x, y));
        }
    }

    /// Returns `true` when enough samples have been collected.
    pub fn is_ready(&self) -> bool {
        self.active && !self.done && self.samples.len() >= Self::MIN_SAMPLES
    }

    /// Mark as done and stop collecting.
    pub fn finish(&mut self) {
        self.done = true;
        self.active = false;
    }

    /// Cancel an in-progress sweep.
    pub fn cancel(&mut self) {
        self.active = false;
        self.done = false;
        self.samples.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum ConnectionType {
    #[default]
    Bluetooth,
    Usb,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceInfo {
    pub firmware_version: String,
    pub controller_type: u8,
    pub mac_address: String,
    pub colors_from_spi: bool,
    /// "Bluetooth" or "USB" — populated from ControllerState.connection_type.
    pub connection: String,
    /// SPI flash diagnostic data (serial number, colors, calibration status).
    pub spi: Option<SpiInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpiInfo {
    /// Factory calibration present (sticks + IMU parsed successfully).
    pub calibration: bool,
    /// Serial number (ASCII, may be blank on some controllers).
    pub serial: String,
    /// Body color as CSS rgb string, e.g. "rgb(40,40,40)".
    pub body_color: String,
    /// Grip color as CSS rgb string.
    pub grip_color: String,
    /// Button color (RGB string, e.g. "rgb(255,255,255)")
    pub button_color: String,
    /// Whether the controller's SPI flash has color info (0x601B flag)
    pub use_spi_colors: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlayerLights {
    pub led_mask: u8,
    pub flash_pattern: u8,
}

/// Virtual controller (ViGEmBus) connection status — updated by the vixinput loop.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VixInputStatus {
    /// Whether any virtual pad is connected to ViGEmBus.
    pub connected: bool,
    /// Whether the virtual Xbox 360 target is connected.
    pub xbox_connected: bool,
    /// Whether the virtual DualShock 4 target is connected.
    pub ds4_connected: bool,
    /// Whether ViGEmClient.dll was successfully loaded.
    pub dll_loaded: bool,
    /// Whether the ViGEmBus driver is running.
    pub driver_connected: bool,
    /// Display-only mode (no virtual gamepad output).
    pub display_only: bool,
    /// The currently emulated controller type.
    pub target_type: VirtualControllerType,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HomeLight {
    pub enabled: bool,
    pub brightness: u8,
    pub pulse_pattern: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RumbleState {
    pub enabled: bool,
    pub left_amplitude: f32,
    pub right_amplitude: f32,
    pub left_frequency: f32,
    pub right_frequency: f32,
}

/// Stick calibration data parsed from SPI flash.
///
/// Stores the absolute center/min/max values for both sticks. The min/max
/// values are computed from the relative offsets stored in SPI flash:
/// `abs_max = center + max_above`, `abs_min = center - min_below`.
///
/// `source` indicates where the calibration came from: `"factory"`, `"user"`,
/// or `"default"` (Linux kernel fallback). `valid` is `true` when
/// `min < center < max` holds for both axes of both sticks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StickCalibration {
    // Absolute values (computed from relative offsets)
    pub left_center_x: u16,
    pub left_center_y: u16,
    pub left_min_x: u16,
    pub left_min_y: u16,
    pub left_max_x: u16,
    pub left_max_y: u16,
    pub right_center_x: u16,
    pub right_center_y: u16,
    pub right_min_x: u16,
    pub right_min_y: u16,
    pub right_max_x: u16,
    pub right_max_y: u16,
    // Source: "factory", "user", or "default"
    pub source: String,
    // Whether calibration is valid (min < center < max)
    pub valid: bool,
}

/// IMU calibration data parsed from SPI flash (address 0x6020, 24 bytes).
///
/// The 24 bytes contain four groups of 3× int16LE:
/// - Accel origin XYZ
/// - Accel sensitivity XYZ
/// - Gyro origin XYZ
/// - Gyro sensitivity XYZ
///
/// `source` indicates where the calibration came from: `"factory"`, `"user"`,
/// or `"default"`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImuCalibration {
    pub accel_origin: [i16; 3],
    pub accel_sensitivity: [i16; 3],
    pub gyro_origin: [i16; 3],
    pub gyro_sensitivity: [i16; 3],
    pub source: String, // "factory", "user", or "default"
    /// 6-axis horizontal offsets from SPI flash (0x6080).
    /// 3× int16LE — accelerometer offsets when controller is on a flat surface.
    /// Default: [0, 0, 0] when SPI flash is uninitialized.
    pub horizontal_offsets: [i16; 3],
}

/// Advanced stick calibration configuration for adaptive deadzone, drift
/// detection, gate calibration, and response curve shaping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StickCalibrationConfig {
    pub adaptive_deadzone_enabled: bool,
    pub center_auto_cal_enabled: bool,
    pub drift_detection_enabled: bool,
    pub gate_calibration_enabled: bool,
    pub response_curve_type: String, // "linear", "exponential", "s-curve", "bezier"
    pub response_curve_power: f32,   // for exponential
    pub bezier_p1: [f32; 2],         // bezier control point 1
    pub bezier_p2: [f32; 2],         // bezier control point 2
    pub deadzone_safety_margin: f32, // default 1.5
    pub min_deadzone: f32,           // default 0.01
    pub max_deadzone: f32,           // default 0.15
    pub deadzone_shape: String,      // "radial", "axial", "elliptic"
}

impl Default for StickCalibrationConfig {
    fn default() -> Self {
        Self {
            adaptive_deadzone_enabled: true,
            center_auto_cal_enabled: true,
            drift_detection_enabled: true,
            gate_calibration_enabled: false, // off by default, requires user calibration
            response_curve_type: "exponential".into(),
            response_curve_power: 1.3,
            bezier_p1: [0.3, 0.9],
            bezier_p2: [0.7, 0.1],
            deadzone_safety_margin: 1.5,
            min_deadzone: 0.01,
            max_deadzone: 0.15,
            deadzone_shape: "radial".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConnectionQuality {
    pub latency_ms: f32,
    pub packet_loss_rate: f32,
    pub last_report_timer: u8,
    pub report_rate_hz: u16,
    /// Total number of HID input reports received since connection.
    pub total_packets: u64,
    /// Number of reports that arrived with a timer gap indicating a dropped frame.
    pub dropped: u64,
    /// Number of subcommand retries (timeouts that were resent).
    pub retries: u64,
}

/// NFC subsystem configuration (Wave 4 amiibo emulation).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct NfcConfig {
    /// Whether NFC/amiibo emulation is enabled.
    pub enabled: bool,
    /// Path to the .bin currently selected for emulation.
    pub emulate_bin: Option<String>,
    /// UID of the last successfully loaded or scanned tag.
    pub last_uid: Option<String>,
}

/// NFC subsystem runtime status.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NfcState {
    pub mode: crate::subcmd::NfcMode,
    pub enabled: bool,
    pub last_tag: Option<crate::subcmd::NfcTagData>,
    pub last_ir_frame: Option<crate::subcmd::IrCameraData>,
    pub scan_count: u32,
    /// Whether a tag is currently presented/read.
    pub tag_present: bool,
    /// UID of the current/presented tag as a hex string.
    pub uid: Option<String>,
    /// Loaded amiibo/NTAG215 dump bytes for emulation.
    pub amiibo_data: Option<Vec<u8>>,
    /// Last error message (if any).
    pub error: Option<String>,
}

// =============================================================================
//  Profile / macro / mapping feature set (P0–PX)
// =============================================================================

/// Identifiers for every button on the Nintendo Switch Pro Controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ButtonId {
    #[default]
    A,
    B,
    X,
    Y,
    Up,
    Down,
    Left,
    Right,
    L,
    R,
    Zl,
    Zr,
    Minus,
    Plus,
    Home,
    Capture,
    LStick,
    RStick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StickSide {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TriggerSide {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoRuleKind {
    #[default]
    ProcessPath,
    WindowTitle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    Exact,
    #[default]
    Contains,
    Regex,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AutoRule {
    pub kind: AutoRuleKind,
    pub pattern: String,
    #[serde(default)]
    pub match_mode: MatchMode,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub auto_rules: Vec<AutoRule>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Per-profile NFC/amiibo configuration.
    #[serde(default)]
    pub nfc: NfcConfig,
    /// Per-profile right-stick configuration (Flick Stick mode selection).
    #[serde(default)]
    pub right_stick: flick_stick::RightStickConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProfileManager {
    pub profiles: Vec<Profile>,
    pub active_profile_id: Option<String>,
    pub default_profile_id: Option<String>,
    pub last_applied: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Macro {
    pub id: String,
    pub name: String,
    pub steps: Vec<MacroStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MacroStep {
    WaitMs(u32),
    PressButton(ButtonId),
    ReleaseButton(ButtonId),
    KeyDown(String),
    KeyUp(String),
    MouseMove(i16, i16),
    MouseDown(u8),
    MouseUp(u8),
    SetStick(StickSide, f32, f32),
    SetTrigger(TriggerSide, f32),
}

impl Default for MacroStep {
    fn default() -> Self {
        MacroStep::WaitMs(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Action {
    Button(ButtonId),
    Key(String),
    KeyCombo(Vec<String>),
    MouseButton(u8),
    Macro(String),
    ProfileNext,
    ProfilePrev,
    GyroToggle,
    Turbo { button: ButtonId, interval_ms: u32 },
    Toggle { button: ButtonId },
    ShiftLayer(u8),
}

impl Default for Action {
    fn default() -> Self {
        Action::Button(ButtonId::A)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ButtonMapping {
    pub source: ButtonId,
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum StickAction {
    #[default]
    Disabled,
    Mouse,
    Wasd,
    ArrowKeys,
    Stick(StickSide),
    Scroll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StickZones {
    pub deadzone: f32,
    pub low: f32,
    pub medium: f32,
    pub high: f32,
    pub low_actions: Vec<Action>,
    pub medium_actions: Vec<Action>,
    pub high_actions: Vec<Action>,
}

impl Default for StickZones {
    fn default() -> Self {
        Self {
            deadzone: 0.0,
            low: 0.25,
            medium: 0.5,
            high: 0.75,
            low_actions: Vec::new(),
            medium_actions: Vec::new(),
            high_actions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerZones {
    pub deadzone: f32,
    pub low: f32,
    pub medium: f32,
    pub high: f32,
    pub low_actions: Vec<Action>,
    pub medium_actions: Vec<Action>,
    pub high_actions: Vec<Action>,
}

impl Default for TriggerZones {
    fn default() -> Self {
        Self {
            deadzone: 0.0,
            low: 0.25,
            medium: 0.5,
            high: 0.75,
            low_actions: Vec::new(),
            medium_actions: Vec::new(),
            high_actions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StickMapping {
    pub left_actions: Vec<StickAction>,
    pub right_actions: Vec<StickAction>,
    pub zones: StickZones,
    #[serde(default)]
    pub response_curve: ResponseCurveType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum GyroMode {
    #[default]
    Off,
    Mouse,
    Stick(StickSide),
    FlickStick,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GyroMapping {
    pub mode: GyroMode,
    pub sensitivity: [f32; 2],
    pub smoothing: f32,
    pub deadzone: f32,
}

impl Default for GyroMapping {
    fn default() -> Self {
        Self {
            mode: GyroMode::default(),
            sensitivity: [1.0, 1.0],
            smoothing: 0.0,
            deadzone: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mappings {
    pub buttons: Vec<ButtonMapping>,
    pub sticks: StickMapping,
    pub gyro: GyroMapping,
    /// Global fallback turbo period in milliseconds.
    pub turbo_interval_ms: u32,
    /// Global turbo duty cycle (0.0–1.0).
    pub turbo_duty_cycle: f32,
}

impl Default for Mappings {
    fn default() -> Self {
        Self {
            buttons: Vec::new(),
            sticks: StickMapping::default(),
            gyro: GyroMapping::default(),
            turbo_interval_ms: 100,
            turbo_duty_cycle: 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ShiftActivation {
    #[default]
    Always,
    Hold(ButtonId),
    Toggle(ButtonId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ShiftLayer {
    pub id: u8,
    pub name: String,
    pub activation: ShiftActivation,
    pub mappings: Mappings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResponseCurveType {
    #[default]
    Linear,
    Exponential(f32),
    SCurve,
    Bezier {
        p1: [f32; 2],
        p2: [f32; 2],
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogConfig {
    pub level: String,
    pub max_lines: usize,
    pub ring_buffer: bool,
    pub log_file: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            max_lines: 1000,
            ring_buffer: true,
            log_file: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AppLogEntry {
    pub timestamp: u64,
    pub level: String,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrayState {
    pub visible: bool,
    pub minimized: bool,
    pub auto_start: bool,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            visible: true,
            minimized: false,
            auto_start: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KbmConfig {
    pub enabled: bool,
    pub anti_cheat_mode: bool,
    pub mouse_sensitivity: f32,
    pub key_repeat_delay_ms: u32,
    pub key_repeat_rate_ms: u32,
}

impl Default for KbmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            anti_cheat_mode: false,
            mouse_sensitivity: 1.0,
            key_repeat_delay_ms: 250,
            key_repeat_rate_ms: 33,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VirtualControllerType {
    #[default]
    Xbox360,
    DualShock4,
}

// =============================================================================
//  Controller / app state
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerState {
    pub connected: bool,
    pub battery_percent: u8,
    pub battery_raw: u8,
    pub charging: bool,
    pub signal_strength: i8,
    pub buttons: ButtonState,
    pub left_stick: StickState,
    pub right_stick: StickState,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub timestamp: u64,
    pub connection_type: ConnectionType,
    pub device_info: Option<DeviceInfo>,
    pub player_lights: PlayerLights,
    pub home_light: HomeLight,
    pub rumble: RumbleState,
    pub stick_calibration: Option<StickCalibration>,
    pub imu_calibration: Option<ImuCalibration>,
    pub connection_quality: ConnectionQuality,
    pub imu: Option<crate::hid_parser::ImuData>,
    pub imu_enabled: bool,
    pub vibration_enabled: bool,
    pub nfc: NfcState,
    /// Raw battery voltage from subcommand 0x50 (regulated voltage).
    /// Range: 1320-1680 (maps to 3.3V-4.2V with 2.5x multiplier).
    /// 0 means not yet polled.
    pub battery_voltage_mv: u16,
    /// IMU gyroscope sensitivity range (0=±250dps, 1=±500dps, 2=±1000dps, 3=±2000dps)
    pub imu_gyro_range: u8,
    /// IMU accelerometer sensitivity range (0=±8G, 1=±4G, 2=±2G, 3=±16G)
    pub imu_accel_range: u8,
    /// Current input report mode (0x30=standard, 0x31=simple HID, 0x3F=NFC/IR).
    pub report_mode: u8,
    /// Tray / window visibility state exposed to the frontend.
    #[serde(default)]
    pub tray_state: TrayState,
    /// Display name of the currently active profile, if any.
    #[serde(default)]
    pub active_profile_name: Option<String>,
    /// 0-based multi-controller slot index (0-3).
    #[serde(default)]
    pub slot_index: u8,
    /// Absolute camera yaw for Flick Stick mode, in degrees [0, 360).
    #[serde(default)]
    pub camera_yaw: f32,
    /// Whether the right stick is currently in a Flick Stick flick.
    #[serde(default)]
    pub flick_active: bool,
    /// Accumulated gyro-to-mouse delta for the current report (pixels).
    #[serde(default)]
    pub gyro_mouse_delta: (i32, i32),
    /// Whether this controller slot has passed real-device validation.
    #[serde(default)]
    pub validated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ButtonState {
    pub a: bool,
    pub b: bool,
    pub x: bool,
    pub y: bool,
    pub l: bool,
    pub r: bool,
    pub zl: bool,
    pub zr: bool,
    pub minus: bool,
    pub plus: bool,
    pub home: bool,
    pub capture: bool,
    pub stick_l: bool,
    pub stick_r: bool,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    /// Right Joy-Con SR button (not used on Pro Controller but parsed for completeness)
    pub sr_right: bool,
    /// Right Joy-Con SL button
    pub sl_right: bool,
    /// Left Joy-Con SR button
    pub sr_left: bool,
    /// Left Joy-Con SL button
    pub sl_left: bool,
}

impl ButtonState {
    /// Read a single button by its logical identifier.
    pub fn get(&self, id: ButtonId) -> bool {
        match id {
            ButtonId::A => self.a,
            ButtonId::B => self.b,
            ButtonId::X => self.x,
            ButtonId::Y => self.y,
            ButtonId::Up => self.dpad_up,
            ButtonId::Down => self.dpad_down,
            ButtonId::Left => self.dpad_left,
            ButtonId::Right => self.dpad_right,
            ButtonId::L => self.l,
            ButtonId::R => self.r,
            ButtonId::Zl => self.zl,
            ButtonId::Zr => self.zr,
            ButtonId::Minus => self.minus,
            ButtonId::Plus => self.plus,
            ButtonId::Home => self.home,
            ButtonId::Capture => self.capture,
            ButtonId::LStick => self.stick_l,
            ButtonId::RStick => self.stick_r,
        }
    }

    /// Write a single button by its logical identifier.
    pub fn set(&mut self, id: ButtonId, pressed: bool) {
        match id {
            ButtonId::A => self.a = pressed,
            ButtonId::B => self.b = pressed,
            ButtonId::X => self.x = pressed,
            ButtonId::Y => self.y = pressed,
            ButtonId::Up => self.dpad_up = pressed,
            ButtonId::Down => self.dpad_down = pressed,
            ButtonId::Left => self.dpad_left = pressed,
            ButtonId::Right => self.dpad_right = pressed,
            ButtonId::L => self.l = pressed,
            ButtonId::R => self.r = pressed,
            ButtonId::Zl => self.zl = pressed,
            ButtonId::Zr => self.zr = pressed,
            ButtonId::Minus => self.minus = pressed,
            ButtonId::Plus => self.plus = pressed,
            ButtonId::Home => self.home = pressed,
            ButtonId::Capture => self.capture = pressed,
            ButtonId::LStick => self.stick_l = pressed,
            ButtonId::RStick => self.stick_r = pressed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct StickState {
    pub x: f32,
    pub y: f32,
    pub raw_x: u16,
    pub raw_y: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepAliveStatus {
    pub active: bool,
    pub interval_ms: u64,
    pub last_ping: u64,
    pub power_events_detected: u32,
    pub adapter_sleep_prevented: bool,
    pub adaptive_mode: bool,
}

/// DSU/Cemuhook UDP motion server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DsuConfig {
    /// Whether the DSU server should start automatically on app launch.
    pub enabled: bool,
    /// Bind address, e.g. "127.0.0.1".
    pub bind_address: String,
    /// UDP port for the DSU server.
    pub port: u16,
    /// Pad data transmission rate in Hz.
    pub update_rate_hz: u32,
}

impl Default for DsuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "127.0.0.1".into(),
            port: 26760,
            update_rate_hz: 60,
        }
    }
}

/// Real-device validation settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ValidationConfig {
    pub enable_real_device_checks: bool,
    pub strict_calibration_requirements: bool,
    pub mock_mode: bool,
    pub require_vigembus: bool,
    pub require_hidhide: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub deadzone_left: f32,
    pub deadzone_right: f32,
    pub keepalive_interval_ms: u64,
    pub adaptive_keepalive: bool,
    pub battery_warning_threshold: u8,
    pub button_remap: RemapConfig,
    pub mock_mode: bool,
    pub stick_calibration_config: StickCalibrationConfig,
    /// Auto-save config to disk on changes.
    pub config_persistence_enabled: bool,
    /// Automatically reconnect to controller after disconnect.
    pub auto_reconnect: bool,
    /// Reconnect retry interval in seconds (1-10).
    pub reconnect_interval_s: u64,
    /// Bluetooth power-down detection via BthUsbMonitor.
    pub bt_power_detection_enabled: bool,
    /// Battery voltage polling interval in seconds (5/10/30/60).
    pub battery_polling_interval_s: u64,
    /// When true, closing the main window minimizes to tray instead of quitting.
    pub close_to_tray: bool,
    /// Launch OxideLink on Windows login.
    #[serde(default)]
    pub auto_start: bool,
    /// Close the main window to the system tray instead of quitting.
    #[serde(default)]
    pub tray_minimize: bool,
    /// Native Windows notification preferences.
    pub notification_config: NotificationConfig,
    /// Profile collection and active/default selection.
    #[serde(default)]
    pub profile_manager: ProfileManager,
    /// Logging sink configuration.
    #[serde(default)]
    pub log_config: LogConfig,
    /// Keyboard/mouse output settings.
    #[serde(default)]
    pub kbm_config: KbmConfig,
    /// Preferred emulated controller type for virtual output.
    #[serde(default)]
    pub default_virtual_controller: VirtualControllerType,
    /// Full button/stick/gyro mapping configuration.
    #[serde(default)]
    pub mappings: Mappings,
    /// Hide the physical Nintendo Switch Pro Controller via HidHide.
    #[serde(default)]
    pub hidhide_enabled: bool,
    /// Automatically re-apply HidHide hiding on app startup.
    #[serde(default)]
    pub hidhide_auto_hide: bool,
    /// DSU/Cemuhook UDP motion server configuration.
    #[serde(default)]
    pub dsu: DsuConfig,
    /// Per-slot active profile IDs (index 0-3); `None` means "use default".
    #[serde(default)]
    pub per_controller_profile: Vec<Option<String>>,
    /// Opt-in crash reporting.
    #[serde(default)]
    pub crash_reporting_enabled: bool,
    /// Sentry DSN for crash reporting (use "test" for local file mode).
    #[serde(default)]
    pub crash_reporting_dsn: Option<String>,
    /// Opt-in feature usage telemetry.
    #[serde(default)]
    pub telemetry_enabled: bool,
    /// Aptabase app key for telemetry.
    #[serde(default)]
    pub telemetry_key: Option<String>,
    /// Custom updater endpoint URL. When empty, the bundled update server endpoints are used.
    #[serde(default)]
    pub update_endpoint: String,
    /// NFC/amiibo subsystem configuration.
    #[serde(default)]
    pub nfc: NfcConfig,
    /// Right-stick processing configuration (Camera / Flick Stick).
    #[serde(default)]
    pub right_stick: flick_stick::RightStickConfig,
    /// Master switch: require real-device validation before virtual output.
    #[serde(default)]
    pub real_device_validation: bool,
    /// Detailed real-device validation flags.
    #[serde(default)]
    pub validation: ValidationConfig,
}

/// User-configurable native notification preferences.
/// Category toggles act as group-level switches; per-event toggles provide
/// granular control within each category. An event fires only when both its
/// category toggle AND its per-event toggle are enabled (AND the master toggle).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    /// Master toggle — when false, no notifications are emitted.
    pub enabled: bool,
    /// Critical events: controller disconnected, Bluetooth power-down.
    pub critical_enabled: bool,
    /// Warning events: low battery, drift detected.
    pub warning_enabled: bool,
    /// Info events: controller reconnected.
    pub info_enabled: bool,
    // --- Per-event granular toggles ---
    pub notify_disconnect: bool,
    pub notify_bt_power: bool,
    pub notify_low_battery: bool,
    pub notify_drift: bool,
    pub notify_reconnect: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            critical_enabled: true,
            warning_enabled: true,
            info_enabled: true,
            notify_disconnect: true,
            notify_bt_power: true,
            notify_low_battery: true,
            notify_drift: true,
            notify_reconnect: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemapConfig {
    pub a_to: String,
    pub b_to: String,
    pub x_to: String,
    pub y_to: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            deadzone_left: 0.08,
            deadzone_right: 0.08,
            keepalive_interval_ms: 3000,
            adaptive_keepalive: true,
            battery_warning_threshold: 15,
            button_remap: RemapConfig {
                a_to: "b".into(),
                b_to: "a".into(),
                x_to: "y".into(),
                y_to: "x".into(),
            },
            mock_mode: false,
            stick_calibration_config: StickCalibrationConfig::default(),
            config_persistence_enabled: true,
            auto_reconnect: true,
            reconnect_interval_s: 3,
            bt_power_detection_enabled: true,
            battery_polling_interval_s: 30,
            close_to_tray: true,
            auto_start: false,
            tray_minimize: true,
            notification_config: NotificationConfig::default(),
            profile_manager: ProfileManager::default(),
            log_config: LogConfig::default(),
            kbm_config: KbmConfig::default(),
            default_virtual_controller: VirtualControllerType::default(),
            mappings: Mappings::default(),
            hidhide_enabled: false,
            hidhide_auto_hide: false,
            dsu: DsuConfig::default(),
            per_controller_profile: vec![None; CONTROLLER_SLOTS],
            crash_reporting_enabled: false,
            crash_reporting_dsn: None,
            telemetry_enabled: false,
            telemetry_key: None,
            update_endpoint: String::new(),
            nfc: NfcConfig::default(),
            right_stick: flick_stick::RightStickConfig::default(),
            real_device_validation: false,
            validation: ValidationConfig::default(),
        }
    }
}

impl Default for ControllerState {
    fn default() -> Self {
        ControllerState {
            connected: false,
            battery_percent: 0,
            battery_raw: 0,
            charging: false,
            signal_strength: -60,
            buttons: ButtonState::default(),
            left_stick: StickState::default(),
            right_stick: StickState::default(),
            left_trigger: 0.0,
            right_trigger: 0.0,
            timestamp: 0,
            connection_type: ConnectionType::default(),
            device_info: None,
            player_lights: PlayerLights::default(),
            home_light: HomeLight::default(),
            rumble: RumbleState::default(),
            stick_calibration: None,
            imu_calibration: None,
            connection_quality: ConnectionQuality::default(),
            imu: None,
            imu_enabled: false,
            vibration_enabled: false,
            nfc: NfcState::default(),
            battery_voltage_mv: 0,
            imu_gyro_range: 0,
            imu_accel_range: 0,
            report_mode: 0x30, // standard full report
            tray_state: TrayState::default(),
            active_profile_name: None,
            slot_index: 0,
            camera_yaw: 0.0,
            flick_active: false,
            gyro_mouse_delta: (0, 0),
            validated: false,
        }
    }
}

impl Default for KeepAliveStatus {
    fn default() -> Self {
        KeepAliveStatus {
            active: false,
            interval_ms: 3000,
            last_ping: 0,
            power_events_detected: 0,
            adapter_sleep_prevented: false,
            adaptive_mode: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcEvent {
    ControllerState {
        data: ControllerState,
    },
    KeepAliveStatus {
        data: KeepAliveStatus,
    },
    ConfigUpdated {
        data: AppConfig,
    },
    BatteryWarning {
        percent: u8,
    },
    Disconnected {
        reason: String,
    },
    Reconnected,
    BluetoothPowerEvent {
        event_type: String,
        timestamp: u64,
    },
    DriftDetected {
        stick: String,
        status: String,
    },
    RawHidReport {
        hex: String,
        report_id: u8,
    },
    LogMessage {
        level: String,
        message: String,
    },
    DeviceInfo {
        data: DeviceInfo,
    },
    ImuData {
        frames: crate::hid_parser::ImuData,
        timestamp: u64,
    },
    CalibrationData {
        stick: StickCalibration,
        imu: ImuCalibration,
    },
    PlayerLightsChanged {
        mask: u8,
        pattern: u8,
    },
    HomeLightChanged {
        enabled: bool,
        brightness: u8,
        pattern: u8,
    },
    SubcommandReply {
        subcmd_id: u8,
        ack: u8,
        data: Vec<u8>,
    },
    ConnectionQuality {
        data: ConnectionQuality,
    },
    BatteryState {
        percent: u8,
        charging: bool,
        raw: u8,
        health: String,
    },
    NfcTagScanned {
        tag: crate::subcmd::NfcTagData,
    },
    NfcModeChanged {
        mode: crate::subcmd::NfcMode,
    },
    IrFrameReceived {
        frame: crate::subcmd::IrCameraData,
    },
    CalibrationStatus {
        data: crate::stick_cal::CalibrationStatus,
    },
    /// Emitted when the active profile changes.
    ProfileChanged {
        profile_id: Option<String>,
        profile_name: Option<String>,
    },
    /// Batch of recent log entries for the frontend log viewer.
    LogBatch {
        logs: Vec<AppLogEntry>,
    },
    /// Tray icon / window state changed.
    TrayStateChanged {
        data: TrayState,
    },
    /// KB/M output configuration changed.
    KbmStateChanged {
        data: KbmConfig,
    },
}

/// A handle to one controller slot backed by a shared `[ControllerState; 4]`.
///
/// `read()` and `write()` return mapped guards that deref to `ControllerState`,
/// so `shared.slots[i].read()` and `shared.slots[i].write()` view the same
/// underlying per-slot state.
#[derive(Clone)]
pub struct ControllerSlot {
    lock: Arc<RwLock<[ControllerState; CONTROLLER_SLOTS]>>,
    index: usize,
}

impl ControllerSlot {
    pub fn new(lock: Arc<RwLock<[ControllerState; CONTROLLER_SLOTS]>>, index: usize) -> Self {
        Self { lock, index }
    }

    pub fn read(&self) -> MappedRwLockReadGuard<'_, ControllerState> {
        RwLockReadGuard::map(self.lock.read(), |arr| &arr[self.index])
    }

    pub fn write(&self) -> MappedRwLockWriteGuard<'_, ControllerState> {
        RwLockWriteGuard::map(self.lock.write(), |arr| &mut arr[self.index])
    }
}

pub struct SharedState {
    /// Per-slot controller state (slots 0-3).
    pub slots: [ControllerSlot; CONTROLLER_SLOTS],
    pub keepalive: RwLock<KeepAliveStatus>,
    pub config: RwLock<AppConfig>,
    pub packet_number: AtomicU8,
    /// Gyro-aim configuration shared between the frontend and the XInput
    /// mapping pipeline.
    pub gyro_aim: RwLock<crate::imu::GyroAimConfig>,
    /// Advanced stick calibration configuration (adaptive deadzone, drift
    /// detection, response curves, etc.).
    pub stick_calibration_config: RwLock<StickCalibrationConfig>,
    /// Per-slot command channels to the connected HID devices.
    /// `None` for empty slots; `Some(tx)` when a device loop is running.
    pub slot_cmd_txs: RwLock<
        [Option<tokio::sync::mpsc::Sender<crate::device_loop::DeviceCommand>>; CONTROLLER_SLOTS],
    >,
    /// UI / routing selected slot for legacy commands that do not specify one.
    pub selected_slot: AtomicU8,
    /// Bitmask of currently connected physical slots (bit 0 = slot 0, etc.).
    pub active_controllers: AtomicU8,
    /// Gate calibration sweep collector — shared between Tauri commands
    /// (start/status) and the device loop (sample feeding + completion).
    pub gate_cal_collector: Mutex<GateCalibrationCollector>,
    /// Virtual controller (ViGEmBus) connection status — updated by the vixinput
    /// loop, read by the `get_vixinput_status` Tauri command.
    pub vixinput_status: RwLock<VixInputStatus>,
    /// Gyro-to-mouse/stick processor. Updated from the device loop and
    /// recentered from the frontend.
    pub gyro_mouse: Arc<Mutex<crate::gyro_mouse::GyroMouse>>,
    /// The active virtual controller output device.
    pub vixinput: Mutex<VirtualXInput>,
    /// Keyboard/mouse emulator shared across all slots.
    pub kbm: Arc<Mutex<KbmEmulator>>,
    /// Macro recorder shared with the Tauri command frontend.
    pub macro_engine: Arc<Mutex<Option<MacroEngine>>>,
    /// Per-slot Flick Stick camera processors.
    pub flick_stick: [Mutex<flick_stick::FlickStick>; CONTROLLER_SLOTS],
    /// In-game overlay runtime state (lightweight, no Tauri runtime types).
    pub overlay: Mutex<Option<crate::overlay::OverlayState>>,
    /// Set by the `rescan_controllers` Tauri command; cleared by the device manager.
    pub rescan_requested: AtomicBool,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        let slots_lock = Arc::new(RwLock::new([
            ControllerState::default(),
            ControllerState::default(),
            ControllerState::default(),
            ControllerState::default(),
        ]));
        // Stamp each slot with its index so frontend code can identify slots.
        {
            let mut arr = slots_lock.write();
            for (i, state) in arr.iter_mut().enumerate() {
                state.slot_index = i as u8;
            }
        }
        let slots = [
            ControllerSlot::new(slots_lock.clone(), 0),
            ControllerSlot::new(slots_lock.clone(), 1),
            ControllerSlot::new(slots_lock.clone(), 2),
            ControllerSlot::new(slots_lock.clone(), 3),
        ];
        Arc::new(Self {
            slots,
            keepalive: RwLock::new(KeepAliveStatus::default()),
            config: RwLock::new(AppConfig::default()),
            packet_number: AtomicU8::new(0),
            gyro_aim: RwLock::new(crate::imu::GyroAimConfig::default()),
            stick_calibration_config: RwLock::new(StickCalibrationConfig::default()),
            slot_cmd_txs: RwLock::new([None, None, None, None]),
            selected_slot: AtomicU8::new(0),
            active_controllers: AtomicU8::new(0),
            gate_cal_collector: Mutex::new(GateCalibrationCollector::default()),
            vixinput_status: RwLock::new(VixInputStatus::default()),
            gyro_mouse: Arc::new(Mutex::new(crate::gyro_mouse::GyroMouse::new())),
            vixinput: Mutex::new(VirtualXInput::new_fallback()),
            kbm: Arc::new(Mutex::new(KbmEmulator::new())),
            macro_engine: Arc::new(Mutex::new(None)),
            flick_stick: std::array::from_fn(|_| Mutex::new(flick_stick::FlickStick::new())),
            overlay: Mutex::new(None),
            rescan_requested: AtomicBool::new(false),
        })
    }

    pub fn next_packet_number(&self) -> u8 {
        let n = self.packet_number.fetch_add(1, Ordering::SeqCst);
        n.wrapping_add(1) & 0x0F
    }

    /// Return a read guard for the controller slot currently selected in the UI.
    pub fn active_controller(&self) -> MappedRwLockReadGuard<'_, ControllerState> {
        let idx = self.selected_slot.load(Ordering::Relaxed) as usize;
        self.slots[idx.min(CONTROLLER_SLOTS - 1)].read()
    }

    /// Return a write guard for the controller slot currently selected in the UI.
    pub fn active_controller_mut(&self) -> MappedRwLockWriteGuard<'_, ControllerState> {
        let idx = self.selected_slot.load(Ordering::Relaxed) as usize;
        self.slots[idx.min(CONTROLLER_SLOTS - 1)].write()
    }

    /// Mark a slot as (dis)connected and update the `active_controllers` bitmask.
    pub fn set_slot_connected(&self, slot: u8, connected: bool) {
        let idx = slot as usize;
        if idx >= CONTROLLER_SLOTS {
            return;
        }
        let mask = 1u8 << idx;
        if connected {
            self.active_controllers.fetch_or(mask, Ordering::SeqCst);
        } else {
            self.active_controllers.fetch_and(!mask, Ordering::SeqCst);
        }
        let mut state = self.slots[idx].write();
        state.connected = connected;
        if connected {
            state.timestamp = timestamp_now();
        }
    }

    /// Check whether the physical bitmask marks a slot as connected.
    pub fn is_slot_active(&self, slot: u8) -> bool {
        let idx = slot as usize;
        if idx >= CONTROLLER_SLOTS {
            return false;
        }
        let mask = 1u8 << idx;
        (self.active_controllers.load(Ordering::SeqCst) & mask) != 0
    }

    /// Send a raw subcommand packet to the currently selected slot.
    pub fn send_device_command(&self, buf: Vec<u8>) -> Result<(), String> {
        let slot = self.selected_slot.load(Ordering::SeqCst);
        self.send_device_command_to_slot(slot, buf)
    }

    /// Send a raw subcommand packet to a specific slot.
    pub fn send_device_command_to_slot(&self, slot: u8, buf: Vec<u8>) -> Result<(), String> {
        let idx = slot as usize;
        if idx >= CONTROLLER_SLOTS {
            return Err(format!("Invalid slot {}", slot));
        }
        let guard = self.slot_cmd_txs.read();
        match guard[idx].as_ref() {
            Some(tx) => tx
                .try_send(crate::device_loop::DeviceCommand::Write(buf))
                .map_err(|e| match e {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                        format!("HID device command channel full for slot {}", slot)
                    }
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                        format!("HID device command channel closed for slot {}", slot)
                    }
                }),
            None => Err(format!("No controller connected on slot {}", slot)),
        }
    }

    /// Request that the device manager rescans HID devices on its next cycle.
    pub fn request_rescan(&self) {
        self.rescan_requested.store(true, Ordering::SeqCst);
    }
}

/// Tauri-managed shared state container.
#[derive(Clone)]
pub struct AppCtx {
    pub shared: Arc<SharedState>,
    pub tx: broadcast::Sender<IpcEvent>,
    pub keepalive: Arc<crate::keepalive::KeepAliveManager>,
}

pub fn timestamp_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| {
            d.as_secs()
                .saturating_mul(1_000)
                .saturating_add(d.subsec_millis() as u64)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===========================================================================
    //  GateCalibrationCollector
    // ===========================================================================

    #[test]
    fn gate_cal_collector_default_is_inactive() {
        let collector = GateCalibrationCollector::default();
        assert!(!collector.active);
        assert!(!collector.done);
        assert!(collector.samples.is_empty());
    }

    #[test]
    fn gate_cal_collector_start_activates_and_clears() {
        let mut collector = GateCalibrationCollector::default();
        collector.samples.push((1.0, 2.0));
        collector.start();
        assert!(collector.active);
        assert!(!collector.done);
        assert!(collector.samples.is_empty());
    }

    #[test]
    fn gate_cal_collector_add_when_active() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        collector.add(0.1, 0.2);
        collector.add(0.3, 0.4);
        assert_eq!(collector.samples.len(), 2);
        assert_eq!(collector.samples[0], (0.1, 0.2));
        assert_eq!(collector.samples[1], (0.3, 0.4));
    }

    #[test]
    fn gate_cal_collector_add_ignored_when_inactive() {
        let mut collector = GateCalibrationCollector::default();
        collector.add(0.1, 0.2);
        assert!(collector.samples.is_empty());
    }

    #[test]
    fn gate_cal_collector_add_ignored_when_done() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        collector.finish();
        collector.add(0.1, 0.2);
        assert!(collector.samples.is_empty());
    }

    #[test]
    fn gate_cal_collector_is_ready_false_below_threshold() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        for _ in 0..100 {
            collector.add(0.5, 0.5);
        }
        assert!(!collector.is_ready());
    }

    #[test]
    fn gate_cal_collector_is_ready_true_at_threshold() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        for _ in 0..500 {
            collector.add(0.5, 0.5);
        }
        assert!(collector.is_ready());
    }

    #[test]
    fn gate_cal_collector_is_ready_false_when_done() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        for _ in 0..500 {
            collector.add(0.5, 0.5);
        }
        collector.finish();
        assert!(!collector.is_ready());
    }

    #[test]
    fn gate_cal_collector_is_ready_false_when_inactive() {
        let mut collector = GateCalibrationCollector::default();
        for _ in 0..500 {
            collector.add(0.5, 0.5);
        }
        // Inactive, so samples won't be added
        assert!(!collector.is_ready());
    }

    #[test]
    fn gate_cal_collector_finish_sets_done_and_inactive() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        collector.add(1.0, 1.0);
        collector.finish();
        assert!(collector.done);
        assert!(!collector.active);
        // Samples are preserved after finish
        assert_eq!(collector.samples.len(), 1);
    }

    #[test]
    fn gate_cal_collector_cancel_clears_everything() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        collector.add(1.0, 2.0);
        collector.add(3.0, 4.0);
        collector.cancel();
        assert!(!collector.active);
        assert!(!collector.done);
        assert!(collector.samples.is_empty());
    }

    #[test]
    fn gate_cal_collector_start_after_finish_resets() {
        let mut collector = GateCalibrationCollector::default();
        collector.start();
        collector.add(1.0, 1.0);
        collector.finish();
        collector.start();
        assert!(collector.active);
        assert!(!collector.done);
        assert!(collector.samples.is_empty());
    }

    #[test]
    fn gate_cal_collector_min_samples_is_500() {
        assert_eq!(GateCalibrationCollector::MIN_SAMPLES, 500);
    }

    // ===========================================================================
    //  ConnectionType
    // ===========================================================================

    #[test]
    fn connection_type_default_is_bluetooth() {
        assert_eq!(ConnectionType::default(), ConnectionType::Bluetooth);
    }

    #[test]
    fn connection_type_usb_variant() {
        let ct = ConnectionType::Usb;
        assert_eq!(ct, ConnectionType::Usb);
    }

    #[test]
    fn connection_type_serialization() {
        let bt = ConnectionType::Bluetooth;
        let s = serde_json::to_string(&bt).unwrap();
        assert_eq!(s, "\"Bluetooth\"");
        let usb = ConnectionType::Usb;
        let s = serde_json::to_string(&usb).unwrap();
        assert_eq!(s, "\"Usb\"");
    }

    #[test]
    fn connection_type_deserialization() {
        let bt: ConnectionType = serde_json::from_str("\"Bluetooth\"").unwrap();
        assert_eq!(bt, ConnectionType::Bluetooth);
        let usb: ConnectionType = serde_json::from_str("\"Usb\"").unwrap();
        assert_eq!(usb, ConnectionType::Usb);
    }

    // ===========================================================================
    //  DeviceInfo
    // ===========================================================================

    #[test]
    fn device_info_default_all_empty() {
        let di = DeviceInfo::default();
        assert!(di.firmware_version.is_empty());
        assert_eq!(di.controller_type, 0);
        assert!(di.mac_address.is_empty());
        assert!(!di.colors_from_spi);
        assert!(di.connection.is_empty());
        assert!(di.spi.is_none());
    }

    #[test]
    fn device_info_serialization_round_trip() {
        let di = DeviceInfo {
            firmware_version: "2.0".into(),
            controller_type: 3,
            mac_address: "AA:BB:CC:DD:EE:FF".into(),
            colors_from_spi: true,
            connection: "Bluetooth".into(),
            spi: Some(SpiInfo {
                calibration: true,
                serial: "SN123".into(),
                body_color: "rgb(40,40,40)".into(),
                grip_color: "rgb(20,20,20)".into(),
                button_color: "rgb(255,255,255)".into(),
                use_spi_colors: true,
            }),
        };
        let s = serde_json::to_string(&di).unwrap();
        let back: DeviceInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back.firmware_version, "2.0");
        assert_eq!(back.controller_type, 3);
        assert_eq!(back.mac_address, "AA:BB:CC:DD:EE:FF");
        assert!(back.colors_from_spi);
        assert_eq!(back.connection, "Bluetooth");
        assert!(back.spi.is_some());
        let spi = back.spi.unwrap();
        assert!(spi.calibration);
        assert_eq!(spi.serial, "SN123");
        assert_eq!(spi.body_color, "rgb(40,40,40)");
    }

    #[test]
    fn device_info_serialization_no_spi() {
        let di = DeviceInfo {
            firmware_version: "1.0".into(),
            controller_type: 1,
            mac_address: "11:22:33:44:55:66".into(),
            colors_from_spi: false,
            connection: "USB".into(),
            spi: None,
        };
        let s = serde_json::to_string(&di).unwrap();
        assert!(s.contains("\"firmware_version\":\"1.0\""));
        assert!(s.contains("\"controller_type\":1"));
        assert!(s.contains("\"spi\":null"));
    }

    // ===========================================================================
    //  SpiInfo
    // ===========================================================================

    #[test]
    fn spi_info_default_all_empty() {
        let spi = SpiInfo::default();
        assert!(!spi.calibration);
        assert!(spi.serial.is_empty());
        assert!(spi.body_color.is_empty());
        assert!(spi.grip_color.is_empty());
        assert!(spi.button_color.is_empty());
        assert!(!spi.use_spi_colors);
    }

    #[test]
    fn spi_info_serialization_round_trip() {
        let spi = SpiInfo {
            calibration: true,
            serial: "XYZ".into(),
            body_color: "rgb(1,2,3)".into(),
            grip_color: "rgb(4,5,6)".into(),
            button_color: "rgb(7,8,9)".into(),
            use_spi_colors: false,
        };
        let s = serde_json::to_string(&spi).unwrap();
        let back: SpiInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back.calibration, spi.calibration);
        assert_eq!(back.serial, spi.serial);
        assert_eq!(back.body_color, spi.body_color);
        assert_eq!(back.grip_color, spi.grip_color);
        assert_eq!(back.button_color, spi.button_color);
        assert_eq!(back.use_spi_colors, spi.use_spi_colors);
    }

    // ===========================================================================
    //  ControllerState defaults
    // ===========================================================================

    #[test]
    fn controller_state_default_values() {
        let state = ControllerState::default();
        assert!(!state.connected);
        assert_eq!(state.battery_percent, 0);
        assert_eq!(state.battery_raw, 0);
        assert!(!state.charging);
        assert_eq!(state.signal_strength, -60);
        assert_eq!(state.left_trigger, 0.0);
        assert_eq!(state.right_trigger, 0.0);
        assert_eq!(state.timestamp, 0);
        assert_eq!(state.connection_type, ConnectionType::Bluetooth);
        assert!(state.device_info.is_none());
        assert!(state.stick_calibration.is_none());
        assert!(state.imu_calibration.is_none());
        assert!(!state.imu_enabled);
        assert!(!state.vibration_enabled);
        assert_eq!(state.battery_voltage_mv, 0);
        assert_eq!(state.imu_gyro_range, 0);
        assert_eq!(state.imu_accel_range, 0);
        assert_eq!(state.report_mode, 0x30);
        assert_eq!(state.slot_index, 0);
        assert_eq!(state.camera_yaw, 0.0);
        assert!(!state.flick_active);
        assert_eq!(state.gyro_mouse_delta, (0, 0));
        assert!(!state.validated);
    }

    #[test]
    fn controller_state_default_buttons_all_false() {
        let state = ControllerState::default();
        let b = &state.buttons;
        assert!(!b.a);
        assert!(!b.b);
        assert!(!b.x);
        assert!(!b.y);
        assert!(!b.l);
        assert!(!b.r);
        assert!(!b.zl);
        assert!(!b.zr);
        assert!(!b.minus);
        assert!(!b.plus);
        assert!(!b.home);
        assert!(!b.capture);
        assert!(!b.stick_l);
        assert!(!b.stick_r);
        assert!(!b.dpad_up);
        assert!(!b.dpad_down);
        assert!(!b.dpad_left);
        assert!(!b.dpad_right);
    }

    #[test]
    fn controller_state_default_sticks_zero() {
        let state = ControllerState::default();
        assert_eq!(state.left_stick.x, 0.0);
        assert_eq!(state.left_stick.y, 0.0);
        assert_eq!(state.left_stick.raw_x, 0);
        assert_eq!(state.left_stick.raw_y, 0);
        assert_eq!(state.right_stick.x, 0.0);
        assert_eq!(state.right_stick.y, 0.0);
        assert_eq!(state.right_stick.raw_x, 0);
        assert_eq!(state.right_stick.raw_y, 0);
    }

    // ===========================================================================
    //  ButtonState get/set
    // ===========================================================================

    #[test]
    fn button_state_get_set_all_buttons() {
        let mut b = ButtonState::default();
        for id in [
            ButtonId::A,
            ButtonId::B,
            ButtonId::X,
            ButtonId::Y,
            ButtonId::Up,
            ButtonId::Down,
            ButtonId::Left,
            ButtonId::Right,
            ButtonId::L,
            ButtonId::R,
            ButtonId::Zl,
            ButtonId::Zr,
            ButtonId::Minus,
            ButtonId::Plus,
            ButtonId::Home,
            ButtonId::Capture,
            ButtonId::LStick,
            ButtonId::RStick,
        ] {
            b.set(id, true);
            assert!(b.get(id), "button {:?} should be true after set", id);
            b.set(id, false);
            assert!(!b.get(id), "button {:?} should be false after unset", id);
        }
    }

    #[test]
    fn button_state_default_is_all_false() {
        let b = ButtonState::default();
        for id in [
            ButtonId::A,
            ButtonId::B,
            ButtonId::X,
            ButtonId::Y,
            ButtonId::Up,
            ButtonId::Down,
            ButtonId::Left,
            ButtonId::Right,
            ButtonId::L,
            ButtonId::R,
            ButtonId::Zl,
            ButtonId::Zr,
            ButtonId::Minus,
            ButtonId::Plus,
            ButtonId::Home,
            ButtonId::Capture,
            ButtonId::LStick,
            ButtonId::RStick,
        ] {
            assert!(!b.get(id), "button {:?} should be false by default", id);
        }
    }

    #[test]
    fn button_state_partial_eq() {
        let mut a = ButtonState::default();
        let mut b = ButtonState::default();
        assert_eq!(a, b);
        a.a = true;
        assert_ne!(a, b);
        b.a = true;
        assert_eq!(a, b);
    }

    // ===========================================================================
    //  StickState
    // ===========================================================================

    #[test]
    fn stick_state_default_values() {
        let s = StickState::default();
        assert_eq!(s.x, 0.0);
        assert_eq!(s.y, 0.0);
        assert_eq!(s.raw_x, 0);
        assert_eq!(s.raw_y, 0);
    }

    #[test]
    fn stick_state_partial_eq() {
        let a = StickState {
            x: 0.5,
            y: -0.3,
            raw_x: 100,
            raw_y: 200,
        };
        let b = StickState {
            x: 0.5,
            y: -0.3,
            raw_x: 100,
            raw_y: 200,
        };
        assert_eq!(a, b);
    }

    // ===========================================================================
    //  StickZones defaults
    // ===========================================================================

    #[test]
    fn stick_zones_default_values() {
        let z = StickZones::default();
        assert_eq!(z.deadzone, 0.0);
        assert_eq!(z.low, 0.25);
        assert_eq!(z.medium, 0.5);
        assert_eq!(z.high, 0.75);
        assert!(z.low_actions.is_empty());
        assert!(z.medium_actions.is_empty());
        assert!(z.high_actions.is_empty());
    }

    // ===========================================================================
    //  TriggerZones defaults
    // ===========================================================================

    #[test]
    fn trigger_zones_default_values() {
        let z = TriggerZones::default();
        assert_eq!(z.deadzone, 0.0);
        assert_eq!(z.low, 0.25);
        assert_eq!(z.medium, 0.5);
        assert_eq!(z.high, 0.75);
        assert!(z.low_actions.is_empty());
        assert!(z.medium_actions.is_empty());
        assert!(z.high_actions.is_empty());
    }

    // ===========================================================================
    //  GyroMapping defaults
    // ===========================================================================

    #[test]
    fn gyro_mapping_default_values() {
        let g = GyroMapping::default();
        assert_eq!(g.mode, GyroMode::Off);
        assert_eq!(g.sensitivity, [1.0, 1.0]);
        assert_eq!(g.smoothing, 0.0);
        assert_eq!(g.deadzone, 0.0);
    }

    // ===========================================================================
    //  Mappings defaults
    // ===========================================================================

    #[test]
    fn mappings_default_values() {
        let m = Mappings::default();
        assert!(m.buttons.is_empty());
        assert_eq!(m.turbo_interval_ms, 100);
        assert_eq!(m.turbo_duty_cycle, 0.5);
        assert_eq!(m.gyro.mode, GyroMode::Off);
    }

    // ===========================================================================
    //  LogConfig defaults
    // ===========================================================================

    #[test]
    fn log_config_default_values() {
        let lc = LogConfig::default();
        assert_eq!(lc.level, "info");
        assert_eq!(lc.max_lines, 1000);
        assert!(lc.ring_buffer);
        assert!(!lc.log_file);
    }

    // ===========================================================================
    //  TrayState defaults
    // ===========================================================================

    #[test]
    fn tray_state_default_values() {
        let ts = TrayState::default();
        assert!(ts.visible);
        assert!(!ts.minimized);
        assert!(!ts.auto_start);
    }

    // ===========================================================================
    //  KbmConfig defaults
    // ===========================================================================

    #[test]
    fn kbm_config_default_values() {
        let kc = KbmConfig::default();
        assert!(!kc.enabled);
        assert!(!kc.anti_cheat_mode);
        assert_eq!(kc.mouse_sensitivity, 1.0);
        assert_eq!(kc.key_repeat_delay_ms, 250);
        assert_eq!(kc.key_repeat_rate_ms, 33);
    }

    // ===========================================================================
    //  DsuConfig defaults
    // ===========================================================================

    #[test]
    fn dsu_config_default_values() {
        let dc = DsuConfig::default();
        assert!(!dc.enabled);
        assert_eq!(dc.bind_address, "127.0.0.1");
        assert_eq!(dc.port, 26760);
        assert_eq!(dc.update_rate_hz, 60);
    }

    // ===========================================================================
    //  NotificationConfig defaults
    // ===========================================================================

    #[test]
    fn notification_config_default_all_enabled() {
        let nc = NotificationConfig::default();
        assert!(nc.enabled);
        assert!(nc.critical_enabled);
        assert!(nc.warning_enabled);
        assert!(nc.info_enabled);
        assert!(nc.notify_disconnect);
        assert!(nc.notify_bt_power);
        assert!(nc.notify_low_battery);
        assert!(nc.notify_drift);
        assert!(nc.notify_reconnect);
    }

    // ===========================================================================
    //  KeepAliveStatus defaults
    // ===========================================================================

    #[test]
    fn keepalive_status_default_values() {
        let ka = KeepAliveStatus::default();
        assert!(!ka.active);
        assert_eq!(ka.interval_ms, 3000);
        assert_eq!(ka.last_ping, 0);
        assert_eq!(ka.power_events_detected, 0);
        assert!(!ka.adapter_sleep_prevented);
        assert!(ka.adaptive_mode);
    }

    // ===========================================================================
    //  StickCalibrationConfig defaults
    // ===========================================================================

    #[test]
    fn stick_cal_config_default_values() {
        let sc = StickCalibrationConfig::default();
        assert!(sc.adaptive_deadzone_enabled);
        assert!(sc.center_auto_cal_enabled);
        assert!(sc.drift_detection_enabled);
        assert!(!sc.gate_calibration_enabled);
        assert_eq!(sc.response_curve_type, "exponential");
        assert_eq!(sc.response_curve_power, 1.3);
        assert_eq!(sc.bezier_p1, [0.3, 0.9]);
        assert_eq!(sc.bezier_p2, [0.7, 0.1]);
        assert_eq!(sc.deadzone_safety_margin, 1.5);
        assert_eq!(sc.min_deadzone, 0.01);
        assert_eq!(sc.max_deadzone, 0.15);
        assert_eq!(sc.deadzone_shape, "radial");
    }

    // ===========================================================================
    //  AppConfig defaults
    // ===========================================================================

    #[test]
    fn app_config_default_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.deadzone_left, 0.08);
        assert_eq!(cfg.deadzone_right, 0.08);
        assert_eq!(cfg.keepalive_interval_ms, 3000);
        assert!(cfg.adaptive_keepalive);
        assert_eq!(cfg.battery_warning_threshold, 15);
        assert!(!cfg.mock_mode);
        assert!(cfg.config_persistence_enabled);
        assert!(cfg.auto_reconnect);
        assert_eq!(cfg.reconnect_interval_s, 3);
        assert!(cfg.bt_power_detection_enabled);
        assert_eq!(cfg.battery_polling_interval_s, 30);
        assert!(cfg.close_to_tray);
        assert!(!cfg.auto_start);
        assert!(cfg.tray_minimize);
        assert!(!cfg.hidhide_enabled);
        assert!(!cfg.hidhide_auto_hide);
        assert!(!cfg.crash_reporting_enabled);
        assert!(cfg.crash_reporting_dsn.is_none());
        assert!(!cfg.telemetry_enabled);
        assert!(cfg.telemetry_key.is_none());
        assert!(cfg.update_endpoint.is_empty());
        assert!(!cfg.real_device_validation);
    }

    #[test]
    fn app_config_default_button_remap() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.button_remap.a_to, "b");
        assert_eq!(cfg.button_remap.b_to, "a");
        assert_eq!(cfg.button_remap.x_to, "y");
        assert_eq!(cfg.button_remap.y_to, "x");
    }

    #[test]
    fn app_config_default_per_controller_profile() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.per_controller_profile.len(), CONTROLLER_SLOTS);
        for p in &cfg.per_controller_profile {
            assert!(p.is_none());
        }
    }

    #[test]
    fn app_config_default_dsu() {
        let cfg = AppConfig::default();
        assert!(!cfg.dsu.enabled);
        assert_eq!(cfg.dsu.bind_address, "127.0.0.1");
        assert_eq!(cfg.dsu.port, 26760);
    }

    // ===========================================================================
    //  VirtualControllerType
    // ===========================================================================

    #[test]
    fn virtual_controller_type_default_is_xbox360() {
        assert_eq!(
            VirtualControllerType::default(),
            VirtualControllerType::Xbox360
        );
    }

    #[test]
    fn virtual_controller_type_serialization() {
        let xbox = serde_json::to_string(&VirtualControllerType::Xbox360).unwrap();
        assert_eq!(xbox, "\"xbox360\"");
        let ds4 = serde_json::to_string(&VirtualControllerType::DualShock4).unwrap();
        assert_eq!(ds4, "\"dualshock4\"");
    }

    // ===========================================================================
    //  NfcConfig
    // ===========================================================================

    #[test]
    fn nfc_config_default_values() {
        let nfc = NfcConfig::default();
        assert!(!nfc.enabled);
        assert!(nfc.emulate_bin.is_none());
        assert!(nfc.last_uid.is_none());
    }

    #[test]
    fn nfc_config_serialization_round_trip() {
        let nfc = NfcConfig {
            enabled: true,
            emulate_bin: Some("/path/to/bin".into()),
            last_uid: Some("UID123".into()),
        };
        let s = serde_json::to_string(&nfc).unwrap();
        let back: NfcConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, nfc);
    }

    // ===========================================================================
    //  ValidationConfig
    // ===========================================================================

    #[test]
    fn validation_config_default_all_false() {
        let vc = ValidationConfig::default();
        assert!(!vc.enable_real_device_checks);
        assert!(!vc.strict_calibration_requirements);
        assert!(!vc.mock_mode);
        assert!(!vc.require_vigembus);
        assert!(!vc.require_hidhide);
    }

    // ===========================================================================
    //  SharedState initialization
    // ===========================================================================

    #[test]
    fn shared_state_initializes_four_slots() {
        let shared = SharedState::new();
        for i in 0..CONTROLLER_SLOTS {
            let state = shared.slots[i].read();
            assert_eq!(state.slot_index, i as u8);
            assert!(!state.connected);
        }
    }

    #[test]
    fn shared_state_default_config() {
        let shared = SharedState::new();
        let cfg = shared.config.read();
        assert_eq!(cfg.deadzone_left, 0.08);
        assert_eq!(cfg.battery_warning_threshold, 15);
    }

    #[test]
    fn shared_state_default_keepalive() {
        let shared = SharedState::new();
        let ka = shared.keepalive.read();
        assert!(!ka.active);
        assert_eq!(ka.interval_ms, 3000);
    }

    #[test]
    fn shared_state_slot_cmd_txs_all_none() {
        let shared = SharedState::new();
        let txs = shared.slot_cmd_txs.read();
        for tx in txs.iter() {
            assert!(tx.is_none());
        }
    }

    #[test]
    fn shared_state_active_controllers_starts_zero() {
        let shared = SharedState::new();
        assert_eq!(shared.active_controllers.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shared_state_selected_slot_starts_zero() {
        let shared = SharedState::new();
        assert_eq!(shared.selected_slot.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shared_state_rescan_requested_starts_false() {
        let shared = SharedState::new();
        assert!(!shared.rescan_requested.load(Ordering::SeqCst));
    }

    // ===========================================================================
    //  SharedState::next_packet_number
    // ===========================================================================

    #[test]
    fn next_packet_number_increments() {
        let shared = SharedState::new();
        let a = shared.next_packet_number();
        let b = shared.next_packet_number();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
    }

    #[test]
    fn next_packet_number_wraps_at_16() {
        let shared = SharedState::new();
        // Call 16 times; the 16th should be 16 & 0x0F = 0.
        for _ in 0..15 {
            shared.next_packet_number();
        }
        assert_eq!(shared.next_packet_number(), 16 & 0x0F);
        assert_eq!(shared.next_packet_number(), 17 & 0x0F);
    }

    // ===========================================================================
    //  SharedState::set_slot_connected / is_slot_active
    // ===========================================================================

    #[test]
    fn set_slot_connected_marks_active() {
        let shared = SharedState::new();
        shared.set_slot_connected(0, true);
        assert!(shared.is_slot_active(0));
        let state = shared.slots[0].read();
        assert!(state.connected);
    }

    #[test]
    fn set_slot_connected_disconnects() {
        let shared = SharedState::new();
        shared.set_slot_connected(1, true);
        assert!(shared.is_slot_active(1));
        shared.set_slot_connected(1, false);
        assert!(!shared.is_slot_active(1));
        let state = shared.slots[1].read();
        assert!(!state.connected);
    }

    #[test]
    fn set_slot_connected_multiple_slots() {
        let shared = SharedState::new();
        shared.set_slot_connected(0, true);
        shared.set_slot_connected(2, true);
        assert!(shared.is_slot_active(0));
        assert!(!shared.is_slot_active(1));
        assert!(shared.is_slot_active(2));
        assert!(!shared.is_slot_active(3));
    }

    #[test]
    fn set_slot_connected_invalid_slot_ignored() {
        let shared = SharedState::new();
        shared.set_slot_connected(99, true);
        assert!(!shared.is_slot_active(99));
    }

    #[test]
    fn is_slot_active_invalid_slot_false() {
        let shared = SharedState::new();
        assert!(!shared.is_slot_active(10));
    }

    #[test]
    fn set_slot_connected_sets_timestamp() {
        let shared = SharedState::new();
        shared.set_slot_connected(0, true);
        let state = shared.slots[0].read();
        assert!(state.timestamp > 0);
    }

    // ===========================================================================
    //  SharedState::request_rescan
    // ===========================================================================

    #[test]
    fn request_rescan_sets_flag() {
        let shared = SharedState::new();
        assert!(!shared.rescan_requested.load(Ordering::SeqCst));
        shared.request_rescan();
        assert!(shared.rescan_requested.load(Ordering::SeqCst));
    }

    // ===========================================================================
    //  SharedState::send_device_command_to_slot
    // ===========================================================================

    #[test]
    fn send_device_command_invalid_slot_returns_error() {
        let shared = SharedState::new();
        let result = shared.send_device_command_to_slot(99, vec![0x01]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid slot"));
    }

    #[test]
    fn send_device_command_no_controller_returns_error() {
        let shared = SharedState::new();
        let result = shared.send_device_command_to_slot(0, vec![0x01]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No controller connected"));
    }

    // ===========================================================================
    //  ControllerSlot
    // ===========================================================================

    #[test]
    fn controller_slot_read_write_roundtrip() {
        let lock = Arc::new(RwLock::new([
            ControllerState::default(),
            ControllerState::default(),
            ControllerState::default(),
            ControllerState::default(),
        ]));
        let slot = ControllerSlot::new(lock, 2);
        {
            let mut state = slot.write();
            state.battery_percent = 75;
        }
        let state = slot.read();
        assert_eq!(state.battery_percent, 75);
    }

    // ===========================================================================
    //  ButtonId serialization
    // ===========================================================================

    #[test]
    fn button_id_serialization_lowercase() {
        assert_eq!(serde_json::to_string(&ButtonId::A).unwrap(), "\"a\"");
        assert_eq!(serde_json::to_string(&ButtonId::B).unwrap(), "\"b\"");
        assert_eq!(serde_json::to_string(&ButtonId::Zl).unwrap(), "\"zl\"");
        assert_eq!(
            serde_json::to_string(&ButtonId::LStick).unwrap(),
            "\"lstick\""
        );
    }

    #[test]
    fn button_id_default_is_a() {
        assert_eq!(ButtonId::default(), ButtonId::A);
    }

    // ===========================================================================
    //  MacroStep / Action defaults
    // ===========================================================================

    #[test]
    fn macro_step_default_is_wait_ms_zero() {
        assert_eq!(MacroStep::default(), MacroStep::WaitMs(0));
    }

    #[test]
    fn action_default_is_button_a() {
        assert_eq!(Action::default(), Action::Button(ButtonId::A));
    }

    // ===========================================================================
    //  ResponseCurveType
    // ===========================================================================

    #[test]
    fn response_curve_type_default_is_linear() {
        assert_eq!(ResponseCurveType::default(), ResponseCurveType::Linear);
    }

    #[test]
    fn response_curve_type_serialization() {
        assert_eq!(
            serde_json::to_string(&ResponseCurveType::Linear).unwrap(),
            "{\"type\":\"linear\"}"
        );
        assert_eq!(
            serde_json::to_string(&ResponseCurveType::SCurve).unwrap(),
            "{\"type\":\"s_curve\"}"
        );
    }

    // ===========================================================================
    //  ShiftActivation
    // ===========================================================================

    #[test]
    fn shift_activation_default_is_always() {
        assert_eq!(ShiftActivation::default(), ShiftActivation::Always);
    }

    // ===========================================================================
    //  CONTROLLER_SLOTS constant
    // ===========================================================================

    #[test]
    fn controller_slots_is_four() {
        assert_eq!(CONTROLLER_SLOTS, 4);
    }

    // ===========================================================================
    //  timestamp_now
    // ===========================================================================

    #[test]
    fn timestamp_now_returns_nonzero() {
        let ts = timestamp_now();
        assert!(ts > 0);
    }

    #[test]
    fn timestamp_now_non_decreasing() {
        let a = timestamp_now();
        for _ in 0..1000 {
            std::hint::black_box(0u8);
        }
        let b = timestamp_now();
        assert!(b >= a);
    }

    // ===========================================================================
    //  AppLogEntry
    // ===========================================================================

    #[test]
    fn app_log_entry_default() {
        let entry = AppLogEntry::default();
        assert_eq!(entry.timestamp, 0);
        assert!(entry.level.is_empty());
        assert!(entry.target.is_empty());
        assert!(entry.message.is_empty());
    }

    #[test]
    fn app_log_entry_serialization_round_trip() {
        let entry = AppLogEntry {
            timestamp: 12345,
            level: "info".into(),
            target: "oxidelink::state".into(),
            message: "test message".into(),
        };
        let s = serde_json::to_string(&entry).unwrap();
        let back: AppLogEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back, entry);
    }

    // ===========================================================================
    //  Profile / ProfileManager defaults
    // ===========================================================================

    #[test]
    fn profile_default_values() {
        let p = Profile::default();
        assert!(p.id.is_empty());
        assert!(p.name.is_empty());
        assert!(!p.enabled);
        assert!(p.auto_rules.is_empty());
        assert_eq!(p.created_at, 0);
        assert_eq!(p.updated_at, 0);
    }

    #[test]
    fn profile_manager_default_values() {
        let pm = ProfileManager::default();
        assert!(pm.profiles.is_empty());
        assert!(pm.active_profile_id.is_none());
        assert!(pm.default_profile_id.is_none());
        assert!(pm.last_applied.is_none());
    }

    // ===========================================================================
    //  StickMapping defaults
    // ===========================================================================

    #[test]
    fn stick_mapping_default_values() {
        let sm = StickMapping::default();
        assert!(sm.left_actions.is_empty());
        assert!(sm.right_actions.is_empty());
        assert_eq!(sm.response_curve, ResponseCurveType::Linear);
    }

    // ===========================================================================
    //  StickAction default
    // ===========================================================================

    #[test]
    fn stick_action_default_is_disabled() {
        assert_eq!(StickAction::default(), StickAction::Disabled);
    }

    // ===========================================================================
    //  GyroMode default
    // ===========================================================================

    #[test]
    fn gyro_mode_default_is_off() {
        assert_eq!(GyroMode::default(), GyroMode::Off);
    }

    // ===========================================================================
    //  AutoRule defaults
    // ===========================================================================

    #[test]
    fn auto_rule_default_values() {
        let ar = AutoRule::default();
        assert_eq!(ar.kind, AutoRuleKind::ProcessPath);
        assert!(ar.pattern.is_empty());
        assert_eq!(ar.match_mode, MatchMode::Contains);
        assert!(!ar.enabled);
    }

    #[test]
    fn auto_rule_kind_serialization() {
        assert_eq!(
            serde_json::to_string(&AutoRuleKind::ProcessPath).unwrap(),
            "\"process_path\""
        );
        assert_eq!(
            serde_json::to_string(&AutoRuleKind::WindowTitle).unwrap(),
            "\"window_title\""
        );
    }

    #[test]
    fn match_mode_serialization() {
        assert_eq!(serde_json::to_string(&MatchMode::Exact).unwrap(), "\"exact\"");
        assert_eq!(
            serde_json::to_string(&MatchMode::Contains).unwrap(),
            "\"contains\""
        );
        assert_eq!(
            serde_json::to_string(&MatchMode::Regex).unwrap(),
            "\"regex\""
        );
    }

    // ===========================================================================
    //  StickCalibration defaults
    // ===========================================================================

    #[test]
    fn stick_calibration_default_all_zero() {
        let sc = StickCalibration::default();
        assert_eq!(sc.left_center_x, 0);
        assert_eq!(sc.left_center_y, 0);
        assert_eq!(sc.left_min_x, 0);
        assert_eq!(sc.left_min_y, 0);
        assert_eq!(sc.left_max_x, 0);
        assert_eq!(sc.left_max_y, 0);
        assert_eq!(sc.right_center_x, 0);
        assert_eq!(sc.right_center_y, 0);
        assert_eq!(sc.right_min_x, 0);
        assert_eq!(sc.right_min_y, 0);
        assert_eq!(sc.right_max_x, 0);
        assert_eq!(sc.right_max_y, 0);
        assert!(sc.source.is_empty());
        assert!(!sc.valid);
    }

    // ===========================================================================
    //  ImuCalibration defaults
    // ===========================================================================

    #[test]
    fn imu_calibration_default_all_zero() {
        let ic = ImuCalibration::default();
        assert_eq!(ic.accel_origin, [0, 0, 0]);
        assert_eq!(ic.accel_sensitivity, [0, 0, 0]);
        assert_eq!(ic.gyro_origin, [0, 0, 0]);
        assert_eq!(ic.gyro_sensitivity, [0, 0, 0]);
        assert!(ic.source.is_empty());
        assert_eq!(ic.horizontal_offsets, [0, 0, 0]);
    }

    // ===========================================================================
    //  RumbleState defaults
    // ===========================================================================

    #[test]
    fn rumble_state_default_values() {
        let rs = RumbleState::default();
        assert!(!rs.enabled);
        assert_eq!(rs.left_amplitude, 0.0);
        assert_eq!(rs.right_amplitude, 0.0);
        assert_eq!(rs.left_frequency, 0.0);
        assert_eq!(rs.right_frequency, 0.0);
    }

    // ===========================================================================
    //  VixInputStatus defaults
    // ===========================================================================

    #[test]
    fn vixinput_status_default_values() {
        let vs = VixInputStatus::default();
        assert!(!vs.connected);
        assert!(!vs.xbox_connected);
        assert!(!vs.ds4_connected);
        assert!(!vs.dll_loaded);
        assert!(!vs.driver_connected);
        assert!(!vs.display_only);
        assert_eq!(vs.target_type, VirtualControllerType::Xbox360);
    }

    // ===========================================================================
    //  Macro defaults
    // ===========================================================================

    #[test]
    fn macro_default_values() {
        let m = Macro::default();
        assert!(m.id.is_empty());
        assert!(m.name.is_empty());
        assert!(m.steps.is_empty());
    }

    // ===========================================================================
    //  ShiftLayer defaults
    // ===========================================================================

    #[test]
    fn shift_layer_default_values() {
        let sl = ShiftLayer::default();
        assert_eq!(sl.id, 0);
        assert!(sl.name.is_empty());
        assert_eq!(sl.activation, ShiftActivation::Always);
    }
}
