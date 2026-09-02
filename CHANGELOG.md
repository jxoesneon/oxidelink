# Changelog

All notable changes to OxideLink are documented in this file.

## [0.2.0] - 2026-09-02

Initial public release of OxideLink, combining core controller support, output emulation, motion features, and the build pipeline.

### Repository governance and public readiness

- SECURITY.md, CONTRIBUTING.md, and CODE_OF_CONDUCT.md (Contributor Covenant 2.1)
- GitHub issue templates (bug report, feature request) and pull request template
- Dependabot configuration for npm, cargo, and github-actions weekly updates
- FUNDING.yml placeholder for future sponsorship
- Repository topics: tauri, rust, windows, nintendo-switch, pro-controller, vigembus, hidhide, gyro-aim,, flick-stick, dsu, cemuhook, amiibo, gamepad, controller, xinput, dualshock
- Branch protection on main requiring the test check
- Vulnerability alerts and automated security fixes enabled
- README polished with CI/release/license/platform badges and table of contents
- Fixed placeholder GitHub URL in docs/community.md
- NSIS bundle README documenting 0-byte driver installer placeholders

### CI improvements

- Rust dependency caching via Swatinem/rust-cache@v2
- npm dependency caching via setup-node
- cargo fmt --check step added
- Switched from npm install to npm ci for reproducible builds
- Concurrency groups to cancel stale runs

### Dependency remediation

- Upgraded Vite 5 to 8 (fixed esbuild and nanoid vulnerabilities)
- Upgraded e2e-tests WebdriverIO packages to 9.30+ and @wdio/tauri-service to 1.3.0
- Vendored extract-zip with symlink path traversal patch (CVE-2026-56876 / GHSA-jmr9-qjv8-65gv)
- npm audit: 0 vulnerabilities across root and e2e-tests
- Dependabot alerts: 0 open

### Code quality

- cargo fmt applied to all Rust source files
- Fixed clippy chunks_exact_to_as_chunks lint (hidhide.rs, subcmd.rs)

### Release pipeline fixes

- Removed hard-coded developer signing path from tauri.conf.json
- Fixed release workflow signing condition (env scoping bug)

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
