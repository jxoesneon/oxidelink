# Changelog

All notable changes to OxideLink are documented in this file.

## [0.2.0] - 2026-07-19

Initial public release of OxideLink, combining core controller support, output emulation, motion features, and the build pipeline.

### Wave 1 — Core controller support

- Nintendo Switch Pro Controller HID report parsing over USB and Bluetooth
- Bluetooth keep-alive and adaptive power management
- Battery, charging, and connection-quality telemetry
- Stick calibration from SPI flash (factory/user/default)
- IMU calibration from SPI flash
- Player LEDs and Home button LED control
- Connection state and telemetry IPC events over WebSocket

### Wave 2 — Configuration, calibration, and logging

- App config persistence with serde JSON
- Deadzone and response curve support for sticks
- Button remap (A↔B, X↔Y Switch-to-PC layout)
- App log viewer with search, filter, copy, and clear
- Diagnostic rolling averages for signal and battery
- Stick gate calibration sweep collector

### Wave 3 — Output, build pipeline, and updater

- Virtual Xbox 360 and DualShock 4 output through ViGEmBus
- Runtime loading of `ViGEmClient.dll` with graceful fallback
- Windows `SendInput` keyboard and mouse backend
- HidHide integration for hiding the physical Pro Controller
- Auto-updater via `tauri-plugin-updater`
- NSIS installer with optional bundled HidHide/ViGEmBus driver pages
- Code signing with `signtool` and self-signed PFX support
- System tray minimize and Windows Run-key auto-start

### Wave 4 — Advanced motion, macros, and community features

- Gyro-to-mouse and gyro-to-stick mapping with smoothing and deadzone
- Flick Stick right-stick camera mode
- DSU / Cemuhook UDP motion server on port `26760`
- Macro recorder and playback engine (button/key/mouse/stick/trigger steps)
- Profile manager with per-process and per-window auto-switching
- Profile import/export JSON commands
- NFC / amiibo `.bin` emulation (540-byte PowerSaves and 572-byte raw NTAG215)
- Opt-in telemetry with allow-listed events and PII scrubbing
- Opt-in Sentry crash reporting with local test mode
- In-game overlay module (placeholder UI stub)
- Cloud / community profile sharing module (placeholder API stub)

### Known issues and blockers

- The bundled `HidHideInstaller.exe` and `ViGEmBusSetup.exe` are 0-byte placeholders. Replace them with real installer binaries before shipping.
- Self-signed code signing triggers Windows SmartScreen warnings; use a CA-issued certificate for public releases.
- The in-game overlay is not yet wired to a visible window.
- Cloud/community profile sharing returns stub results.

## [0.1.0] - pre-release

- Project scaffolding and initial research prototypes.
