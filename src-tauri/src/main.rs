#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// OxideLink is a binary crate whose internal modules expose library-style
// API surface (Switch protocol constants, parser/builder utilities, config
// types for upcoming features, mock/test infrastructure, and not-yet-wired
// tray/updater features). These are intentionally kept as dead code rather
// than deleted, so suppress the warning crate-wide instead of littering
// every item with #[allow(dead_code)].
#![allow(dead_code)]

mod bt_reconnect;
mod bthusb_monitor;
mod cloud;
mod config;
mod crash;
mod curves;
mod device_loop;
mod dsu;
mod gyro_mouse;
mod hid_parser;
mod hidhide;
mod imu;
mod kbm;
mod keepalive;
mod keycode;
mod logging;
mod macro_engine;
mod mock;
mod nfc;
mod overlay;
mod profile_manager;
mod state;
mod stick_cal;
mod subcmd;
mod telemetry;
mod telemetry_events;
mod tray;
mod turbo;
mod updater;
mod vixinput;
mod xinput;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use parking_lot::RwLock;
use tauri::{Manager, State};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use bthusb_monitor::BthUsbMonitor;
use device_loop::DeviceLoop;
use keepalive::KeepAliveManager;
use mock::MockGenerator;
use state::{AppConfig, AppCtx, IpcEvent, SharedState, VirtualControllerType};
use vixinput::VirtualXInput;

// `imu` and `subcmd` are declared as `mod` above and are referenced directly
// (e.g. `subcmd::build_set_player_lights_subcmd`) by the extended commands below.

// Shared WebSocket IPC address. Must match the frontend constant in `src-frontend/main.js`.
pub const IPC_WS_ADDR: &str = "127.0.0.1:9001";

// ---------------------------------------------------------------------------
// Tauri commands (invoked from the frontend via `invoke`)
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_controller_state(ctx: State<'_, AppCtx>) -> state::ControllerState {
    ctx.shared.active_controller().clone()
}

#[tauri::command]
fn get_ws_addr() -> String {
    IPC_WS_ADDR.to_string()
}

#[tauri::command]
fn get_keepalive_status(ctx: State<'_, AppCtx>) -> state::KeepAliveStatus {
    ctx.shared.keepalive.read().clone()
}

#[tauri::command]
fn get_config(ctx: State<'_, AppCtx>) -> AppConfig {
    ctx.shared.config.read().clone()
}

#[tauri::command]
async fn update_config(ctx: State<'_, AppCtx>, config: AppConfig) -> Result<AppConfig, String> {
    {
        let mut c = ctx.shared.config.write();
        *c = config.clone();
    }
    // Auto-save to disk if persistence is enabled.
    if config.config_persistence_enabled {
        if let Err(e) = config::save_config_async(&config).await {
            log::warn!("Auto-save failed: {}", e);
        }
    }
    // Read the canonical config back under the lock so the IPC event and
    // return value always reflect the current shared state.
    let current = ctx.shared.config.read().clone();
    let _ = ctx.tx.send(IpcEvent::ConfigUpdated {
        data: current.clone(),
    });
    Ok(current)
}

// ---------------------------------------------------------------------------
// Config persistence Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn save_config_to_disk(ctx: State<'_, AppCtx>) -> Result<String, String> {
    let config = ctx.shared.config.read().clone();
    let path = config::save_config_async(&config).await?;
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
async fn load_config_from_disk(ctx: State<'_, AppCtx>) -> Result<Option<AppConfig>, String> {
    let loaded = config::load_config_async().await?;
    if let Some(ref new_config) = loaded {
        // Validate before applying
        config::validate_config(new_config)?;
        {
            let mut c = ctx.shared.config.write();
            *c = new_config.clone();
        }
        let _ = ctx.tx.send(IpcEvent::ConfigUpdated {
            data: new_config.clone(),
        });
        info!("Config loaded from disk and applied");
    }
    Ok(loaded)
}

#[tauri::command]
async fn export_config_to_file(ctx: State<'_, AppCtx>, path: String) -> Result<(), String> {
    let config = ctx.shared.config.read().clone();
    config::export_config_async(&config, &path).await
}

#[tauri::command]
async fn import_config_from_file(
    ctx: State<'_, AppCtx>,
    path: String,
) -> Result<AppConfig, String> {
    let new_config = config::import_config_async(&path).await?;
    {
        let mut c = ctx.shared.config.write();
        *c = new_config.clone();
    }
    let _ = ctx.tx.send(IpcEvent::ConfigUpdated {
        data: new_config.clone(),
    });
    info!("Config imported from {} and applied", path);
    Ok(new_config)
}

#[tauri::command]
async fn reset_config_to_defaults(ctx: State<'_, AppCtx>) -> Result<AppConfig, String> {
    let defaults = AppConfig::default();
    {
        let mut c = ctx.shared.config.write();
        *c = defaults.clone();
    }
    // Save the reset config to disk
    if defaults.config_persistence_enabled {
        if let Err(e) = config::save_config_async(&defaults).await {
            log::warn!("Failed to save reset config: {}", e);
        }
    }
    let _ = ctx.tx.send(IpcEvent::ConfigUpdated {
        data: defaults.clone(),
    });
    info!("Config reset to defaults");
    Ok(defaults)
}

#[tauri::command]
fn get_config_file_path() -> String {
    config::config_file_path().to_string_lossy().to_string()
}

#[tauri::command]
fn set_mock_mode(ctx: State<'_, AppCtx>, enabled: bool) -> bool {
    let mut c = ctx.shared.config.write();
    c.mock_mode = enabled;
    info!("Mock mode set to {}", enabled);
    enabled
}

#[tauri::command]
fn trigger_keepalive_boost(ctx: State<'_, AppCtx>) -> bool {
    ctx.keepalive.report_power_event("Power_Down_Manual");
    true
}

#[tauri::command]
fn get_xinput_hex(ctx: State<'_, AppCtx>) -> String {
    let cs = ctx.shared.active_controller();
    let config = ctx.shared.config.read();
    let buttons = cs.buttons.clone();
    let mut left = cs.left_stick.clone();
    let mut right = cs.right_stick.clone();
    telemetry::TelemetryExtractor::apply_deadzone(&mut left, config.deadzone_left);
    telemetry::TelemetryExtractor::apply_deadzone(&mut right, config.deadzone_right);
    // ControllerState.buttons are already remapped in the device loop, so only
    // deadzone is applied here before converting to XInput.
    let zl = if buttons.zl { 1.0 } else { 0.0 };
    let zr = if buttons.zr { 1.0 } else { 0.0 };
    let xi = xinput::map_to_xinput(&buttons, &left, &right, zl, zr);
    xinput::xinput_state_to_hex(&xi)
}

#[tauri::command]
fn get_vixinput_status(ctx: State<'_, AppCtx>) -> state::VixInputStatus {
    ctx.shared.vixinput_status.read().clone()
}

#[tauri::command]
fn get_virtual_controller_type(ctx: State<'_, AppCtx>) -> VirtualControllerType {
    ctx.shared.vixinput.lock().kind()
}

#[tauri::command]
fn set_virtual_controller_type(
    ctx: State<'_, AppCtx>,
    kind: VirtualControllerType,
) -> Result<VirtualControllerType, String> {
    {
        let mut cfg = ctx.shared.config.write();
        cfg.default_virtual_controller = kind;
    }

    // Persist the choice if config persistence is enabled.
    if ctx.shared.config.read().config_persistence_enabled {
        let cfg = ctx.shared.config.read().clone();
        if let Err(e) = config::save_config(&cfg) {
            log::warn!(
                "Failed to save config after virtual controller change: {}",
                e
            );
        }
    }

    {
        let mut vix = ctx.shared.vixinput.lock();
        vix.set_kind(kind);
    }

    // Publish the updated connection status.
    {
        let vix = ctx.shared.vixinput.lock();
        let connected = vix.is_connected();
        let kind = vix.kind();
        let dll_loaded = vix.is_dll_loaded();
        let mut st = ctx.shared.vixinput_status.write();
        st.connected = connected;
        st.target_type = kind;
        st.xbox_connected = connected && kind == VirtualControllerType::Xbox360;
        st.ds4_connected = connected && kind == VirtualControllerType::DualShock4;
        st.dll_loaded = dll_loaded;
        st.driver_connected = connected;
        st.display_only = !connected;
    }

    Ok(kind)
}

#[tauri::command]
fn get_vigembus_status(ctx: State<'_, AppCtx>) -> vixinput::VigemBusStatus {
    let mut status = vixinput::detect_vigembus_driver_status();
    let vix = ctx.shared.vixinput.lock();
    let xbox = vix.is_connected() && vix.kind() == VirtualControllerType::Xbox360;
    let ds4 = vix.is_connected() && vix.kind() == VirtualControllerType::DualShock4;
    status.xbox_target_connected = xbox;
    status.ds4_target_connected = ds4;
    status.virtual_pad_connected = xbox || ds4;
    status
}

const VIGEMBUS_INSTALLER_URL: &str =
    "https://github.com/ViGEm/ViGEmBus/releases/latest/download/ViGEmBus_x64_setup.exe";

#[tauri::command]
fn install_vigembus() -> Result<(), String> {
    let (installed, running, _) = vixinput::detect_vigembus_driver();
    if installed && running {
        info!("install_vigembus: ViGEmBus already installed and running — nothing to do.");
        return Ok(());
    }

    let temp_path = std::env::temp_dir().join("ViGEmBus_setup.exe");
    info!(
        "install_vigembus: downloading {} -> {}",
        VIGEMBUS_INSTALLER_URL,
        temp_path.display()
    );

    // Download the installer in-process with ureq to avoid shelling out to PowerShell.
    let response = ureq::get(VIGEMBUS_INSTALLER_URL)
        .timeout(std::time::Duration::from_secs(120))
        .call()
        .map_err(|e| format!("Failed to download ViGEmBus installer: {}", e))?;
    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(&temp_path)
        .map_err(|e| format!("Failed to create temp file {}: {}", temp_path.display(), e))?;
    std::io::copy(&mut reader, &mut file)
        .map_err(|e| format!("Failed to write ViGEmBus installer: {}", e))?;

    // Launch the installer (UAC elevation prompt will appear).
    std::process::Command::new(&temp_path)
        .spawn()
        .map_err(|e| format!("Failed to launch installer: {}", e))?;
    info!("ViGEmBus installer launched — user needs to complete UAC prompt");
    Ok(())
}

// ---------------------------------------------------------------------------
// Extended Tauri commands — device info, lights, rumble, IMU, gyro aim
// ---------------------------------------------------------------------------
//
// These commands update the shared controller state and build the appropriate
// subcommand packets, then send them to the connected controller via the
// shared device command channel (`SharedState::send_device_command`).

#[tauri::command]
fn get_device_info(ctx: State<'_, AppCtx>) -> Option<state::DeviceInfo> {
    ctx.shared.active_controller().device_info.clone()
}

#[tauri::command]
fn set_player_lights(
    ctx: State<'_, AppCtx>,
    led_mask: u8,
    flash_pattern: u8,
) -> Result<bool, String> {
    // Convert preset index to flash mask bitfield:
    // 0=solid (no flash), 1=chase, 2=blink, 3=pulse
    // For chase/blink/pulse, LEDs should ONLY flash (not be steady-on),
    // because "on overrides flashing" per the dekuNukem spec.
    // So the on mask is 0 and the flash mask is the selected LEDs.
    let (on_mask, flash_mask) = match flash_pattern {
        0 => (led_mask, 0u8),         // solid: steady on, no flashing
        1 | 2 | 3 => (0u8, led_mask), // chase/blink/pulse: flash only
        _ => (led_mask, 0u8),
    };
    {
        let mut ctrl = ctx.shared.active_controller_mut();
        ctrl.player_lights.led_mask = led_mask;
        ctrl.player_lights.flash_pattern = flash_pattern;
    }
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_set_player_lights_subcmd(counter, on_mask, flash_mask);
    let _ = ctx.tx.send(IpcEvent::PlayerLightsChanged {
        mask: led_mask,
        pattern: flash_pattern,
    });
    ctx.shared.send_device_command(pkt)?;
    Ok(true)
}

#[tauri::command]
fn set_home_light(
    ctx: State<'_, AppCtx>,
    enabled: bool,
    brightness: u8,
    pattern: String,
) -> Result<bool, String> {
    {
        let mut ctrl = ctx.shared.active_controller_mut();
        ctrl.home_light.enabled = enabled;
        ctrl.home_light.brightness = brightness;
        ctrl.home_light.pulse_pattern = match pattern.as_str() {
            "solid" => 0,
            "breathing" => 1,
            "blink" => 2,
            "fade" => 3,
            "wave" => 4,
            _ => 0,
        };
    }
    let pulse_pattern = ctx.shared.active_controller().home_light.pulse_pattern;
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_set_home_light_subcmd(counter, enabled, brightness, &pattern);
    let _ = ctx.tx.send(IpcEvent::HomeLightChanged {
        enabled,
        brightness,
        pattern: pulse_pattern,
    });
    ctx.shared.send_device_command(pkt)?;
    Ok(true)
}

#[tauri::command]
fn send_rumble(
    ctx: State<'_, AppCtx>,
    left_amp: f32,
    right_amp: f32,
    left_freq: f32,
    right_freq: f32,
) -> Result<bool, String> {
    {
        let mut ctrl = ctx.shared.active_controller_mut();
        ctrl.rumble.left_amplitude = left_amp;
        ctrl.rumble.right_amplitude = right_amp;
        ctrl.rumble.left_frequency = left_freq;
        ctrl.rumble.right_frequency = right_freq;
        ctrl.rumble.enabled = left_amp > 0.0 || right_amp > 0.0;
    }
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_rumble_report(counter, left_freq, left_amp, right_freq, right_amp);
    ctx.shared.send_device_command(pkt)?;
    Ok(true)
}

#[tauri::command]
fn enable_imu(ctx: State<'_, AppCtx>, enabled: bool) -> Result<bool, String> {
    ctx.shared.active_controller_mut().imu_enabled = enabled;
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_enable_imu_subcmd(counter, enabled);
    ctx.shared.send_device_command(pkt)?;
    info!("enable_imu: subcommand 0x40 sent (enabled={})", enabled);
    Ok(true)
}

#[tauri::command]
fn enable_vibration(ctx: State<'_, AppCtx>, enabled: bool) -> Result<bool, String> {
    ctx.shared.active_controller_mut().vibration_enabled = enabled;
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_enable_vibration_subcmd(counter, enabled);
    ctx.shared.send_device_command(pkt)?;
    info!(
        "enable_vibration: subcommand 0x48 sent (enabled={})",
        enabled
    );
    Ok(true)
}

#[tauri::command]
fn get_imu_data(ctx: State<'_, AppCtx>) -> Option<hid_parser::ImuData> {
    ctx.shared.active_controller().imu.clone()
}

#[tauri::command]
fn get_calibration_data(ctx: State<'_, AppCtx>) -> Option<state::StickCalibration> {
    ctx.shared.active_controller().stick_calibration.clone()
}

#[tauri::command]
fn set_gyro_aim(
    ctx: State<'_, AppCtx>,
    enabled: bool,
    sensitivity: f32,
    deadzone: f32,
) -> Result<bool, String> {
    {
        let mut cfg = ctx.shared.gyro_aim.write();
        cfg.enabled = enabled;
        cfg.sensitivity = sensitivity;
        cfg.deadzone = deadzone;
    }
    info!(
        "Gyro aim {} (sensitivity={}, deadzone={})",
        if enabled { "enabled" } else { "disabled" },
        sensitivity,
        deadzone
    );
    Ok(true)
}

#[tauri::command]
async fn recalibrate_sticks(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    // Re-read stick + IMU calibration from SPI flash (factory addresses).
    // Space reads 150ms apart to avoid flooding the controller.
    let shared = ctx.shared.clone();

    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_LEFT_STICK_FACTORY, 9);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_RIGHT_STICK_FACTORY, 9);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_IMU_FACTORY, 24);
    shared.send_device_command(pkt)?;

    log::info!("recalibrate_sticks: SPI flash read subcommands sent (factory calibration)");
    Ok(true)
}

#[tauri::command]
async fn refresh_spi_diagnostics(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    // Re-read diagnostic SPI flash data: serial number, body color, grip colors.
    // Space reads 150ms apart to avoid flooding the controller and stalling
    // input reports — the 0x21 subcommand replies can collide with 0x30
    // standard reports if sent too rapidly.
    let shared = ctx.shared.clone();

    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_SERIAL, 16);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_BODY_COLOR, 3);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_LEFT_GRIP_COLOR, 3);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_RIGHT_GRIP_COLOR, 3);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Color flag (0x601B)
    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_COLOR_FLAG, 1);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Button color (0x6053)
    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_BUTTON_COLOR, 3);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Horizontal offsets (0x6080)
    let counter = shared.next_packet_number();
    let pkt = subcmd::build_spi_flash_read_subcmd(counter, subcmd::SPI_ADDR_HORIZONTAL_OFFSETS, 6);
    shared.send_device_command(pkt)?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    log::info!("refresh_spi_diagnostics: SPI flash read subcommands sent (serial + colors)");
    Ok(true)
}

#[tauri::command]
fn reset_factory_calibration(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    // Reset to factory calibration — clear user calibration so it is re-read
    // from SPI flash on the next connection.
    log::info!("reset_factory_calibration requested");
    ctx.shared.active_controller_mut().stick_calibration = None;
    Ok(true)
}

#[tauri::command]
fn clear_drift(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    // Reset stick centers to the factory default (0x800 = midpoint of 12-bit range).
    let mut ctrl = ctx.shared.active_controller_mut();
    if let Some(ref mut cal) = ctrl.stick_calibration {
        cal.left_center_x = 0x800;
        cal.left_center_y = 0x800;
        cal.right_center_x = 0x800;
        cal.right_center_y = 0x800;
    }
    log::info!("clear_drift: stick centers reset to 0x800");
    Ok(true)
}

// ---------------------------------------------------------------------------
// Advanced stick calibration Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn set_response_curve(
    ctx: State<'_, AppCtx>,
    curve_type: String,
    power: f32,
) -> Result<(), String> {
    let mut config = ctx.shared.stick_calibration_config.write();
    config.response_curve_type = curve_type;
    config.response_curve_power = power;
    Ok(())
}

#[tauri::command]
fn set_calibration_option(
    ctx: State<'_, AppCtx>,
    option: String,
    enabled: bool,
) -> Result<(), String> {
    let mut config = ctx.shared.stick_calibration_config.write();
    match option.as_str() {
        "adaptive-deadzone" => config.adaptive_deadzone_enabled = enabled,
        "center-auto-cal" => config.center_auto_cal_enabled = enabled,
        "drift-detection" => config.drift_detection_enabled = enabled,
        "gate-calibration" => config.gate_calibration_enabled = enabled,
        _ => return Err("Unknown option".into()),
    }
    Ok(())
}

#[tauri::command]
fn start_gate_calibration(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    let mut collector = ctx.shared.gate_cal_collector.lock();
    collector.start();
    info!("Gate calibration sweep started — sweep both sticks around their gates");
    Ok(true)
}

#[tauri::command]
fn get_gate_calibration_status(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    let collector = ctx.shared.gate_cal_collector.lock();
    Ok(collector.done)
}

#[tauri::command]
fn get_imu_calibration(ctx: State<'_, AppCtx>) -> Option<state::ImuCalibration> {
    ctx.shared.active_controller().imu_calibration.clone()
}

// ---------------------------------------------------------------------------
// NFC / IR MCU Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn set_nfc_mode(ctx: State<'_, AppCtx>, mode: u8) -> Result<bool, String> {
    let nfc_mode = match mode {
        0 => subcmd::NfcMode::Disabled,
        1 => subcmd::NfcMode::Nfc,
        2 => subcmd::NfcMode::IrCamera,
        3 => subcmd::NfcMode::Passthrough,
        _ => return Err("Invalid NFC mode".to_string()),
    };
    ctx.shared.active_controller_mut().nfc.mode = nfc_mode;
    ctx.shared.active_controller_mut().nfc.enabled = mode != 0;
    log::info!("set_nfc_mode: {:?}", nfc_mode);
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_set_mcu_config_subcmd(counter, nfc_mode);
    ctx.shared.send_device_command(pkt)?;
    let _ = ctx.tx.send(IpcEvent::NfcModeChanged { mode: nfc_mode });
    Ok(true)
}

#[tauri::command]
fn get_nfc_data(ctx: State<'_, AppCtx>) -> Option<subcmd::NfcTagData> {
    ctx.shared.active_controller().nfc.last_tag.clone()
}

#[tauri::command]
fn get_nfc_state(ctx: State<'_, AppCtx>) -> state::NfcState {
    ctx.shared.active_controller().nfc.clone()
}

// ---------------------------------------------------------------------------
// Extended Tauri commands — IMU sensitivity, voltage, report mode, player lights
// ---------------------------------------------------------------------------

#[tauri::command]
fn set_imu_sensitivity(
    ctx: State<'_, AppCtx>,
    gyro_range: u8,
    accel_range: u8,
    gyro_rate: u8,
    accel_filter: u8,
) -> Result<(), String> {
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_set_imu_sensitivity_subcmd(
        counter,
        gyro_range,
        accel_range,
        gyro_rate,
        accel_filter,
    );
    ctx.shared.send_device_command(pkt)?;
    // Update state
    let mut ctrl = ctx.shared.active_controller_mut();
    ctrl.imu_gyro_range = gyro_range;
    ctrl.imu_accel_range = accel_range;
    Ok(())
}

#[tauri::command]
fn get_battery_voltage(ctx: State<'_, AppCtx>) -> Result<u16, String> {
    // Trigger a voltage poll
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_get_voltage_subcmd(counter);
    ctx.shared.send_device_command(pkt)?;
    // Return last known value (will be updated asynchronously)
    Ok(ctx.shared.active_controller().battery_voltage_mv)
}

#[tauri::command]
fn set_report_mode(ctx: State<'_, AppCtx>, mode: u8) -> Result<u8, String> {
    // Validate mode: 0x00=active, 0x30=standard, 0x31=simple HID, 0x3F=NFC/IR
    if !matches!(mode, 0x00 | 0x30 | 0x31 | 0x3F) {
        return Err(format!("Invalid report mode: 0x{:02X}", mode));
    }
    let previous = {
        let mut ctrl = ctx.shared.active_controller_mut();
        let prev = ctrl.report_mode;
        ctrl.report_mode = mode;
        prev
    };
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_set_report_mode_subcmd(counter, mode);
    ctx.shared.send_device_command(pkt)?;
    info!("set_report_mode: 0x{:02X} → 0x{:02X}", previous, mode);
    Ok(previous)
}

#[tauri::command]
fn get_home_light(ctx: State<'_, AppCtx>) -> state::HomeLight {
    ctx.shared.active_controller().home_light.clone()
}

#[tauri::command]
fn get_imu_sensitivity(ctx: State<'_, AppCtx>) -> (u8, u8) {
    let ctrl = ctx.shared.active_controller();
    (ctrl.imu_gyro_range, ctrl.imu_accel_range)
}

#[tauri::command]
fn get_player_lights(ctx: State<'_, AppCtx>) -> Result<state::PlayerLights, String> {
    // Trigger a player lights query — the reply handler in device_loop
    // updates ControllerState.player_lights asynchronously.
    let counter = ctx.shared.next_packet_number();
    let pkt = subcmd::build_get_player_lights_subcmd(counter);
    ctx.shared.send_device_command(pkt)?;
    // Return last known state (will be refreshed when the reply arrives).
    Ok(ctx.shared.active_controller().player_lights.clone())
}

/// Manually trigger a Bluetooth reconnect for the paired Pro Controller.
/// This re-enables the HID service via the Win32 Bluetooth API, causing
/// Windows to initiate (or accept) a Bluetooth connection to the
/// controller. Useful when the automatic reconnect on USB disconnect
/// fails or when the user wants to switch from USB to Bluetooth manually.
#[tauri::command]
async fn trigger_bt_reconnect() -> Result<bool, String> {
    let result = tokio::task::spawn_blocking(bt_reconnect::trigger_pro_controller_reconnect)
        .await
        .map_err(|e| format!("BT reconnect task panicked: {}", e))?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// WebSocket IPC server (ws://127.0.0.1:9001)
// ---------------------------------------------------------------------------

async fn run_ws_server(addr: &str, tx: broadcast::Sender<IpcEvent>) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => {
            info!("OxideLink IPC WebSocket listening on ws://{}", addr);
            l
        }
        Err(e) => {
            warn!("Failed to bind WebSocket on {}: {}", addr, e);
            return;
        }
    };

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!("WS accept error: {}", e);
                continue;
            }
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("WS handshake failed for {}: {}", peer, e);
                    return;
                }
            };
            info!("WS client connected: {}", peer);

            let (mut ws_sink, mut ws_stream_rx) = ws_stream.split();
            let mut rx = tx.subscribe();

            // Pump IPC events to this client.
            //
            // The device loop emits ~150 events/second (ControllerState at 60 Hz,
            // ImuData at 30 Hz, ConnectionQuality at 60 Hz). We send every event
            // and rely on the frontend to batch DOM updates via
            // requestAnimationFrame to avoid main-thread backpressure.
            // Lag warnings are logged at debug level (after the first) to avoid
            // GB-sized log files when the frontend briefly falls behind.
            let pump = tokio::spawn(async move {
                let mut lag_warned = false;
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let json = match serialize_event(&ev) {
                                Some(j) => j,
                                None => continue,
                            };
                            if ws_sink
                                .send(tokio_tungstenite::tungstenite::Message::Text(json))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            if !lag_warned {
                                warn!(
                                    "WS broadcast receiver lagged by {} events (subsequent lags at debug)",
                                    n
                                );
                                lag_warned = true;
                            } else {
                                debug!("WS broadcast receiver lagged by {} events", n);
                            }
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            // Drain incoming (client -> server) messages; the IPC protocol is
            // server-push-only, so we simply consume and discard client frames
            // to keep the WebSocket half-open and detect disconnects.
            while let Some(msg) = ws_stream_rx.next().await {
                if msg.is_err() {
                    break;
                }
            }
            pump.abort();
            info!("WS client disconnected: {}", peer);
        });
    }
}

fn serialize_event(ev: &IpcEvent) -> Option<String> {
    serde_json::to_string(ev).ok()
}

// ---------------------------------------------------------------------------
// Background: event → native Windows toast notifications
// ---------------------------------------------------------------------------

/// Notification categories for config-based filtering.
enum NotifCategory {
    Critical,
    Warning,
    Info,
}

fn emit_toast(app: &tauri::AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        warn!("Notification failed: {}", e);
    }
}

/// Check if a notification category is enabled in the current config.
fn is_category_enabled(shared: &SharedState, category: &NotifCategory) -> bool {
    let cfg = shared.config.read();
    if !cfg.notification_config.enabled {
        return false;
    }
    match category {
        NotifCategory::Critical => cfg.notification_config.critical_enabled,
        NotifCategory::Warning => cfg.notification_config.warning_enabled,
        NotifCategory::Info => cfg.notification_config.info_enabled,
    }
}

/// Check if a specific event notification is enabled (master + category + per-event).
fn is_event_enabled(
    shared: &SharedState,
    category: &NotifCategory,
    event_field: impl Fn(&state::NotificationConfig) -> bool,
) -> bool {
    is_category_enabled(shared, category) && {
        let cfg = shared.config.read();
        event_field(&cfg.notification_config)
    }
}

/// Per-event rate limiter — prevents notification storms.
/// Returns true if the notification should be emitted (cooldown elapsed).
fn check_cooldown(
    last_fired: &mut std::collections::HashMap<String, std::time::Instant>,
    event_key: &str,
    cooldown_secs: u64,
) -> bool {
    let now = std::time::Instant::now();
    if let Some(last) = last_fired.get(event_key) {
        if now.duration_since(*last).as_secs() < cooldown_secs {
            return false; // Still in cooldown
        }
    }
    last_fired.insert(event_key.to_string(), now);
    true
}

// ---------------------------------------------------------------------------
// App bootstrap
// ---------------------------------------------------------------------------

fn main() {
    // Install the in-memory ring-buffer logger before anything else logs.
    // This replaces the previous env_logger init so the frontend log viewer
    // can query recent entries via `get_logs`/`clear_logs`.
    let loaded_config = config::load_config().unwrap_or_default();
    let log_cfg = loaded_config
        .as_ref()
        .map(|c| c.log_config.clone())
        .unwrap_or_default();
    let log_collector = logging::init_logging(&log_cfg)
        .unwrap_or_else(|e| panic!("Failed to initialize logging collector: {}", e));

    info!("OxideLink starting up");

    let shared = SharedState::new();
    if let Some(ref cfg) = loaded_config {
        if config::validate_config(cfg).is_ok() {
            *shared.config.write() = cfg.clone();
        }
    }

    // Initialise crash reporting and telemetry from persisted config.
    let cfg_for_init = shared.config.read().clone();
    if cfg_for_init.crash_reporting_enabled {
        crash::init_crash_reporting(cfg_for_init.crash_reporting_dsn);
    } else {
        crash::init_crash_reporting(None);
    }
    telemetry_events::Telemetry::instance()
        .set_enabled(cfg_for_init.telemetry_enabled, cfg_for_init.telemetry_key);

    let shared = shared;
    let (tx, _rx) = broadcast::channel::<IpcEvent>(2048);

    // Attach the broadcast channel so `LogBatch` IPC events reach the frontend.
    log_collector.set_event_sender(Some(tx.clone()));

    let keepalive = Arc::new(KeepAliveManager::new(Arc::new(RwLock::new(
        shared.keepalive.read().clone(),
    ))));

    let ctx = AppCtx {
        shared: shared.clone(),
        tx: tx.clone(),
        keepalive: keepalive.clone(),
    };

    // Wire up module state that is consumed by Tauri commands.
    let macro_state = macro_engine::MacroState::new(shared.clone(), tx.clone())
        .unwrap_or_else(|e| panic!("Failed to initialize macro engine: {}", e));
    let kbm_manager = kbm::KbmManager::new(shared.clone());
    let dsu_manager = dsu::DsuManager::new(shared.clone());
    let profile_manager = profile_manager::ProfileManager::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(ctx.clone())
        .manage(shared.clone())
        .manage(macro_state.clone())
        .manage(kbm_manager.clone())
        .manage(dsu_manager.clone())
        .manage(profile_manager.clone())
        .on_window_event(|window, event| {
            // Close-to-tray: when the user clicks the X, hide the window
            // instead of destroying it (if close_to_tray is enabled).
            // Otherwise, let the app quit normally.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let ctx = app.state::<AppCtx>();
                let close_to_tray = ctx.shared.config.read().close_to_tray;
                if close_to_tray {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            let tx_for_ws = tx.clone();
            let tx_for_notif = tx.clone();
            let tx_for_overlay = tx.clone();
            let shared_for_loops = shared.clone();
            let ka_for_loops = keepalive.clone();

            // --- System tray icon ---
            let tray_icon = tauri::tray::TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("OxideLink — Pro Controller Manager")
                .menu(
                    &tauri::menu::MenuBuilder::new(app)
                        .text("show", "Show OxideLink")
                        .separator()
                        .text("quit", "Quit OxideLink")
                        .build()?,
                )
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        info!("Quit requested from tray — shutting down");
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // Double-click the tray icon to show the window
                    if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;
            std::mem::forget(tray_icon);

            // Load persisted config on startup.
            match config::load_config() {
                Ok(Some(loaded_config)) => {
                    if config::validate_config(&loaded_config).is_ok() {
                        *shared.config.write() = loaded_config.clone();
                        info!("Persisted config loaded on startup");
                    } else {
                        warn!("Persisted config failed validation — using defaults");
                    }
                }
                Ok(None) => {
                    info!("No persisted config found — using defaults");
                }
                Err(e) => {
                    warn!("Failed to load persisted config: {} — using defaults", e);
                }
            }

            // --- Wire module runtime state now that config and the IPC bus exist ---
            let app_ctx = app.state::<AppCtx>();
            let startup_cfg = app_ctx.shared.config.read().clone();

            // Profile manager needs the IPC channel to emit ProfileChanged events.
            let pm = app.state::<profile_manager::ProfileManager>().clone();
            pm.set_event_sender(tx_for_notif.clone());

            // Start the DSU/Cemuhook UDP server if the user enabled it.
            if startup_cfg.dsu.enabled {
                let dsu: dsu::DsuManager = (*app.state::<dsu::DsuManager>()).clone();
                tauri::async_runtime::spawn(async move {
                    dsu.start().await;
                });
            }

            // Auto-hide the physical controller via HidHide if requested.
            if startup_cfg.hidhide_auto_hide && !startup_cfg.hidhide_enabled {
                let _ = hidhide::hidhide_set_enabled(app.state::<AppCtx>(), true);
            }

            // Apply the configured startup/auto-start and tray state.
            let _ = tray::set_auto_start_registry(startup_cfg.auto_start);
            {
                let mut ctrl = app_ctx.shared.active_controller_mut();
                ctrl.tray_state.auto_start = startup_cfg.auto_start;
            }

            // Initialize the overlay window (loads persisted overlay config).
            overlay::init_overlay(&app_ctx.shared, app.handle());

            // Overlay state broadcaster: push ControllerState IPC events to the overlay window.
            let shared_for_overlay = app_ctx.shared.clone();
            let overlay_broadcaster_handle = tauri::async_runtime::spawn(async move {
                let mut rx = tx_for_overlay.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(IpcEvent::ControllerState { data }) => {
                            let profile = data.active_profile_name.clone();
                            overlay::emit_overlay_state(&shared_for_overlay, &data, profile);
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            });
            std::mem::forget(overlay_broadcaster_handle);

            // All tokio::spawn calls must happen inside an async context on
            // the Tauri runtime. We spawn one master task that starts all the
            // background loops, since the module start_loop methods call
            // tokio::spawn internally and need a runtime context.

            // 1. WebSocket IPC server.
            tauri::async_runtime::spawn(async move {
                run_ws_server(IPC_WS_ADDR, tx_for_ws).await;
            });

            // 2-5. Start all background loops from within an async context so
            // their internal tokio::spawn calls find the runtime.
            let ka2 = ka_for_loops.clone();
            let tx2 = tx.clone();
            let shared2 = shared_for_loops.clone();
            tauri::async_runtime::spawn(async move {
                info!("Background loops spawn task started");

                // Keep-alive loop.
                let keepalive_handle = ka2.clone().start_loop(tx2.clone());
                std::mem::forget(keepalive_handle);

                // BTHUSB ETW monitor.
                let monitor = Arc::new(BthUsbMonitor::new());
                let monitor_handle = monitor.start_loop(tx2.clone(), ka2.clone(), 5000);
                std::mem::forget(monitor_handle);

                // Mock generator.
                let mock = Arc::new(MockGenerator::new());
                let mock_handle = mock.start_loop(tx2.clone(), shared2.clone(), 100, 30);
                std::mem::forget(mock_handle);

                // Live HID device loop.
                let device = Arc::new(DeviceLoop::new(shared2.clone(), tx2.clone()));
                let device_handle = device.start_loop();
                std::mem::forget(device_handle);

                info!("All background loops spawned");
            });

            // 6. Virtual controller writer + 100Hz push loop.
            // LoadLibraryA can block on DLL search paths, so init on a
            // blocking thread, then move the handle into the push loop.
            info!("Initializing virtual controller...");
            let vix_shared = shared_for_loops.clone();
            let vix_handle = tauri::async_runtime::spawn(async move {
                let kind = vix_shared.config.read().default_virtual_controller;
                let vixinput = tokio::task::spawn_blocking(move || VirtualXInput::new(kind))
                    .await
                    .unwrap_or_else(|_| {
                        log::warn!("VirtualXInput init panicked");
                        VirtualXInput::new_fallback()
                    });
                // Publish the VirtualXInput connection status to shared state
                // so the `get_vixinput_status` Tauri command can report it.
                {
                    let mut st = vix_shared.vixinput_status.write();
                    st.connected = vixinput.is_connected();
                    st.dll_loaded = vixinput.is_dll_loaded();
                    st.driver_connected = vixinput.is_connected();
                    st.display_only = !vixinput.is_connected();
                    st.target_type = vixinput.kind();
                    st.xbox_connected = vixinput.is_connected()
                        && vixinput.kind() == VirtualControllerType::Xbox360;
                    st.ds4_connected = vixinput.is_connected()
                        && vixinput.kind() == VirtualControllerType::DualShock4;
                }
                *vix_shared.vixinput.lock() = vixinput;
                {
                    let vix = vix_shared.vixinput.lock();
                    if vix.is_connected() {
                        info!("Virtual {:?} gamepad active via ViGEmBus", vix.kind());
                    } else {
                        info!(
                            "Virtual controller in display-only mode (ViGEmClient.dll not found)"
                        );
                    }
                }
                use tokio::time::{interval, Duration};
                let mut ticker = interval(Duration::from_millis(10));
                loop {
                    ticker.tick().await;
                    let cs = vix_shared.active_controller();
                    if !cs.connected {
                        continue;
                    }
                    let config = vix_shared.config.read();
                    if config.real_device_validation
                        && !config.validation.mock_mode
                        && !cs.validated
                    {
                        continue;
                    }
                    let buttons = cs.buttons.clone();
                    let mut left = cs.left_stick.clone();
                    let mut right = cs.right_stick.clone();
                    telemetry::TelemetryExtractor::apply_deadzone(&mut left, config.deadzone_left);
                    telemetry::TelemetryExtractor::apply_deadzone(
                        &mut right,
                        config.deadzone_right,
                    );
                    // ControllerState.buttons are already remapped/turboed in the device loop.
                    let zl = if buttons.zl { 1.0 } else { 0.0 };
                    let zr = if buttons.zr { 1.0 } else { 0.0 };
                    // Use gyro-augmented mapping when gyro aim is enabled.
                    let gyro_cfg = vix_shared.gyro_aim.read();
                    let xi = if gyro_cfg.enabled {
                        // Convert the latest IMU frame to physical units.
                        let physical = cs
                            .imu
                            .as_ref()
                            .and_then(|imu| imu.frames.first())
                            .map(imu::raw_to_physical);
                        xinput::map_to_xinput_with_gyro(
                            &buttons,
                            &left,
                            &right,
                            zl,
                            zr,
                            physical.as_ref(),
                            &gyro_cfg,
                        )
                    } else {
                        xinput::map_to_xinput(&buttons, &left, &right, zl, zr)
                    };

                    let (connected, dll_loaded, kind) = {
                        let vix = vix_shared.vixinput.lock();
                        let _ = vix.update(&xi);
                        (vix.is_connected(), vix.is_dll_loaded(), vix.kind())
                    };

                    let mut st = vix_shared.vixinput_status.write();
                    st.connected = connected;
                    st.dll_loaded = dll_loaded;
                    st.driver_connected = connected;
                    st.display_only = !connected;
                    st.target_type = kind;
                    st.xbox_connected = connected && kind == VirtualControllerType::Xbox360;
                    st.ds4_connected = connected && kind == VirtualControllerType::DualShock4;
                }
            });
            std::mem::forget(vix_handle);

            // 7. Notification forwarder (must be spawned from async context).
            let notif_handle = tauri::async_runtime::spawn(async move {
                let mut rx = tx_for_notif.subscribe();
                let mut last_fired: std::collections::HashMap<String, std::time::Instant> =
                    std::collections::HashMap::new();
                let mut last_disconnect: Option<std::time::Instant> = None;
                loop {
                    match rx.recv().await {
                        Ok(IpcEvent::BatteryWarning { percent }) => {
                            if is_event_enabled(&shared_for_loops, &NotifCategory::Warning, |nc| {
                                nc.notify_low_battery
                            }) && check_cooldown(&mut last_fired, "battery_warning", 60)
                            {
                                let title = "OxideLink — Low Battery";
                                let body = format!(
                                    "Pro Controller battery at {}%. Plug in to charge.",
                                    percent
                                );
                                emit_toast(&handle, title, &body);
                            }
                        }
                        Ok(IpcEvent::Disconnected { reason }) => {
                            last_disconnect = Some(std::time::Instant::now());
                            if is_event_enabled(&shared_for_loops, &NotifCategory::Critical, |nc| {
                                nc.notify_disconnect
                            }) && check_cooldown(&mut last_fired, "disconnected", 30)
                            {
                                let title = "OxideLink — Controller Disconnected";
                                let body = format!(
                                    "{}. Check Bluetooth or wait for auto-reconnect.",
                                    reason
                                );
                                emit_toast(&handle, title, &body);
                            }
                        }
                        Ok(IpcEvent::Reconnected) => {
                            // Suppress reconnected notification if within 30s of a disconnect
                            // (prevents flapping during unstable Bluetooth).
                            let should_notify = match last_disconnect {
                                Some(t) => {
                                    std::time::Instant::now().duration_since(t).as_secs() >= 30
                                }
                                None => true,
                            };
                            if should_notify
                                && is_event_enabled(&shared_for_loops, &NotifCategory::Info, |nc| {
                                    nc.notify_reconnect
                                })
                                && check_cooldown(&mut last_fired, "reconnected", 60)
                            {
                                let title = "OxideLink — Controller Reconnected";
                                let body = "Pro Controller is back online.";
                                emit_toast(&handle, title, body);
                            }
                        }
                        Ok(IpcEvent::BluetoothPowerEvent {
                            event_type,
                            timestamp: _,
                        }) => {
                            if is_event_enabled(&shared_for_loops, &NotifCategory::Critical, |nc| {
                                nc.notify_bt_power
                            }) && check_cooldown(&mut last_fired, "bt_power", 60)
                            {
                                let title = "OxideLink — Bluetooth Power Event";
                                let body = format!(
                                    "{} detected. Check Bluetooth adapter status.",
                                    event_type
                                );
                                emit_toast(&handle, title, &body);
                            }
                        }
                        Ok(IpcEvent::DriftDetected { stick, status }) => {
                            if is_event_enabled(&shared_for_loops, &NotifCategory::Warning, |nc| {
                                nc.notify_drift
                            }) && check_cooldown(&mut last_fired, "drift", 120)
                            {
                                let title = "OxideLink — Stick Drift Detected";
                                let body = format!(
                                    "{} stick: {}. Recalibrate in the Calibration tab.",
                                    stick, status
                                );
                                emit_toast(&handle, title, &body);
                            }
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            });
            std::mem::forget(notif_handle);

            info!("OxideLink Tauri app initialized — all background loops started");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Core
            get_controller_state,
            get_keepalive_status,
            get_ws_addr,
            trigger_keepalive_boost,
            set_mock_mode,
            // Config
            get_config,
            update_config,
            save_config_to_disk,
            load_config_from_disk,
            export_config_to_file,
            import_config_from_file,
            reset_config_to_defaults,
            get_config_file_path,
            // Controller / device
            get_device_info,
            set_player_lights,
            set_home_light,
            send_rumble,
            enable_imu,
            enable_vibration,
            get_imu_data,
            get_calibration_data,
            get_imu_calibration,
            set_imu_sensitivity,
            get_imu_sensitivity,
            get_battery_voltage,
            set_report_mode,
            get_player_lights,
            get_home_light,
            trigger_bt_reconnect,
            recalibrate_sticks,
            refresh_spi_diagnostics,
            reset_factory_calibration,
            clear_drift,
            set_gyro_aim,
            set_response_curve,
            set_calibration_option,
            start_gate_calibration,
            get_gate_calibration_status,
            set_nfc_mode,
            get_nfc_data,
            get_nfc_state,
            // Virtual output
            get_xinput_hex,
            get_vixinput_status,
            get_virtual_controller_type,
            set_virtual_controller_type,
            get_vigembus_status,
            install_vigembus,
            // Profiles
            profile_manager::list_profiles,
            profile_manager::create_profile,
            profile_manager::update_profile,
            profile_manager::delete_profile,
            profile_manager::set_active_profile,
            profile_manager::get_active_profile,
            profile_manager::set_auto_switch_enabled,
            profile_manager::get_auto_switch_enabled,
            profile_manager::export_profiles,
            profile_manager::import_profiles,
            // Macro engine
            macro_engine::macro_list,
            macro_engine::macro_create,
            macro_engine::macro_update,
            macro_engine::macro_delete,
            macro_engine::macro_load,
            macro_engine::macro_play,
            macro_engine::macro_stop,
            macro_engine::macro_record_start,
            macro_engine::macro_record_stop,
            // KB/M
            kbm::kbm_set_enabled,
            kbm::kbm_get_status,
            kbm::kbm_set_mappings,
            kbm::kbm_get_mappings,
            kbm::kbm_send_test_key,
            // HidHide
            hidhide::hidhide_get_status,
            hidhide::hidhide_refresh_device_list,
            hidhide::hidhide_hide_controller,
            hidhide::hidhide_unhide_controller,
            hidhide::hidhide_set_enabled,
            // Turbo
            turbo::set_turbo_button,
            turbo::get_turbo_settings,
            // Gyro mouse
            gyro_mouse::set_gyro_mode,
            gyro_mouse::get_gyro_config,
            gyro_mouse::set_gyro_config,
            gyro_mouse::gyro_recenter,
            // Response curves / zones
            curves::set_mapping_response_curve,
            curves::get_response_curve,
            curves::set_stick_zones,
            curves::get_stick_zones,
            // Tray / auto-start
            tray::set_auto_start,
            tray::get_auto_start,
            tray::set_tray_state,
            tray::get_tray_state,
            // Logging
            logging::get_logs,
            logging::clear_logs,
            logging::set_log_level,
            // DSU / Cemuhook
            dsu::dsu_start,
            dsu::dsu_stop,
            dsu::dsu_get_status,
            // Crash reporting
            crash::set_crash_reporting,
            crash::get_crash_reporting_status,
            // Telemetry
            telemetry_events::set_telemetry_enabled,
            telemetry_events::get_telemetry_status,
            telemetry_events::record_telemetry_event,
            // Flick Stick
            state::flick_stick::get_flick_stick_config,
            state::flick_stick::set_flick_stick_config,
            state::flick_stick::reset_flick_stick_yaw,
            // NFC / amiibo
            nfc::set_nfc_enabled,
            nfc::load_amiibo_bin,
            nfc::emulate_amiibo_from_path,
            // Updater
            updater::check_for_updates,
            updater::download_and_install_update,
            updater::get_update_endpoint,
            updater::set_update_endpoint,
            // Multi-controller
            device_loop::get_controllers,
            device_loop::get_controller,
            device_loop::set_active_slot,
            device_loop::rescan_controllers,
            // Overlay
            overlay::get_overlay_config,
            overlay::set_overlay_config,
            overlay::toggle_overlay,
            overlay::update_overlay_state,
            // Cloud
            cloud::get_cloud_config,
            cloud::set_cloud_config,
            cloud::list_community_profiles,
            cloud::download_profile,
            cloud::upload_profile,
            cloud::get_profile_by_code,
            // Real-device validation
            device_loop::get_validation_flags,
            device_loop::set_validation_flags,
            device_loop::validate_current_controller,
        ])
        .run(tauri::generate_context!())
        .expect("error while running OxideLink Tauri application");
}
