# OxideLink

> Turn your Nintendo Switch Pro Controller into a precision PC gamepad — with gyro aim, Flick Stick, macros, profiles, and DSU motion output.

OxideLink is a Windows Tauri/Rust application that connects a Nintendo Switch Pro Controller (USB or Bluetooth) and exposes it to games as a virtual Xbox 360 or DualShock 4 pad. It also supports keyboard/mouse emulation, DSU/Cemuhook motion streaming, amiibo emulation, and per-game profiles.

## Feature matrix

| Feature | Status | Notes |
| --- | --- | --- |
| Controller support | Supported | Nintendo Switch Pro Controller over USB and Bluetooth |
| Profiles | Supported | Per-game/per-process auto-switching and manual profiles |
| Macros | Supported | Record, edit, and play back button/key/mouse sequences |
| Curves / zones | Supported | Response curves (linear, exponential, S-curve, Bezier) and stick zones |
| Keyboard / mouse | Supported | Map sticks or buttons to WASD, arrow keys, mouse, and scroll |
| HidHide | Supported | Hide the physical Pro Controller from games while OxideLink is whitelisted |
| Gyro mouse | Supported | Gyro-to-mouse or gyro-to-stick with smoothing and deadzone |
| Virtual DS4 / Xbox | Supported | ViGEmBus-based Xbox 360 and DualShock 4 output |
| Multi-controller | Supported | Up to 4 controller slots |
| DSU / Cemuhook | Supported | UDP motion server on port `26760` |
| Flick Stick | Supported | Right-stick absolute camera/yaw mode |
| NFC / amiibo | Supported | Load `.bin` dumps for amiibo emulation |
| Auto-updater | Supported | `tauri-plugin-updater` with custom endpoint support |
| System tray | Supported | Minimize to tray, run on Windows login |
| In-game overlay | Placeholder | UI stub; not wired at runtime |
| Cloud / community | Placeholder | Profile sharing stub; not wired at runtime |

## Quick start

1. **Install ViGEmBus** — Download and run the latest `ViGEmBusSetup.exe` from <https://github.com/ViGEm/ViGEmBus>. A reboot is usually required.
2. **Pair your Pro Controller** — Put the controller in pairing mode (hold the small sync button next to the USB-C port until the LEDs chase), then pair it in Windows Bluetooth settings, or connect it via USB.
3. **Run OxideLink** — Launch the app, connect the controller, and set the desired virtual target in the UI (Xbox 360 or DualShock 4).

Optional: install HidHide from <https://github.com/ViGEm/HidHide> and enable **HidHide hiding** in OxideLink to prevent double-input in games.

## Build from source

```powershell
# Install Node dependencies
npm install

# Run the app in development mode
npm run tauri dev

# Build the release bundle (NSIS setup + MSI)
npm run tauri build
```

The Rust code lives in `src-tauri/`, the Vite frontend in `src-frontend/`, and bundled assets in `src-tauri/bundle/`.

## Testing

```powershell
# Rust library tests (490 tests)
cd src-tauri
cargo clippy --lib -- -D warnings
cargo test --lib

# Frontend unit tests (Vitest, 23 tests)
cd ..
npm test

# Production build
npm run build
```

### E2E tests (WebdriverIO + Tauri)

E2E tests live in `e2e-tests/` and require a release build plus `tauri-driver`:

```powershell
npm run tauri build
cd e2e-tests
npm ci
npm test
```

See `e2e-tests/README.md` for setup details.

## Documentation

- [docs/features.md](docs/features.md) — Feature-by-feature guide
- [docs/shortcuts.md](docs/shortcuts.md) — Controller shortcuts and remapping
- [docs/amiibo.md](docs/amiibo.md) — Loading and emulating amiibo `.bin` files
- [docs/flickstick.md](docs/flickstick.md) — Flick Stick setup and tuning
- [docs/gyro-mouse.md](docs/gyro-mouse.md) — Gyro-to-mouse tuning
- [docs/profiles.md](docs/profiles.md) — Profiles, auto-switch, import/export
- [docs/telemetry.md](docs/telemetry.md) — Telemetry and crash reporting
- [docs/community.md](docs/community.md) — Community links and contribution guide
- [CHANGELOG.md](CHANGELOG.md) — Release notes
- [docs/installer.md](docs/installer.md) — NSIS installer, drivers, and signing
