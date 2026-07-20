# OxideLink features

This guide covers every major feature in OxideLink, what it does, how to turn it on, and known limitations.

---

## Controller support

**What it does:** Connects a Nintendo Switch Pro Controller to OxideLink over USB or Bluetooth, parses raw HID reports, and keeps the connection alive with adaptive keep-alive packets.

**How to enable:** Pair the controller in Windows, then click **Rescan / Connect** in OxideLink. The active transport is shown in the status chip.

**Limitations:**
- Currently focused on the Nintendo Switch Pro Controller.
- Non-Pro controllers may report different `controller_type` values and are not fully tested.

---

## Profiles

**What it does:** Lets you save multiple controller configurations and switch between them manually or automatically when a specific game or window becomes active.

**How to enable:**
1. Open the **Profiles** tab and create a new profile.
2. Add auto-rules (`ProcessPath` or `WindowTitle`) with `Exact`, `Contains`, or `Regex` matching.
3. Enable **Auto-switch profiles** so the foreground-window poll applies the best match.

**Limitations:**
- Auto-switch polls once per second; rapid window changes may not be caught immediately.
- The default profile is used when no auto-rule matches.

---

## Macros

**What it does:** Records or hand-builds sequences of controller button presses, keyboard keys, mouse moves, and stick/trigger sets, then plays them back.

**How to enable:**
1. Open the **Macros** tab.
2. Click **Record**, perform the inputs, then **Stop**.
3. Assign the macro to a controller button in the **Mappings** tab.

**Limitations:**
- Macro playback cannot cross anti-cheat boundaries; `SendInput` may be rejected by protected games.
- Unknown key names in a macro are logged and ignored.

---

## Curves and zones

**What it does:** Adjusts how stick deflections translate into output values and splits a stick into zones (`low`, `medium`, `high`) that can trigger different actions.

**How to enable:**
1. In **Mappings > Sticks**, choose a response curve: `Linear`, `Exponential(factor)`, `SCurve`, or `Bezier { p1, p2 }`.
2. Configure `StickZones` with `deadzone`, `low`, `medium`, and `high` thresholds and assign actions to each zone.

**Limitations:**
- Bezier curves require both control points in the `[0, 1]` range.
- Zone thresholds must be ordered `deadzone < low < medium < high` for predictable behavior.

---

## Keyboard and mouse (KB/M)

**What it does:** Maps controller buttons or stick directions to keyboard keys, mouse movement, mouse buttons, or scroll wheel input.

**How to enable:**
1. In **Mappings**, set a stick `StickAction` to `Wasd`, `ArrowKeys`, `Mouse`, or `Scroll`.
2. For buttons, add a `Key` or `KeyCombo` action.
3. Enable **KB/M output** in Settings if there is a master toggle.

**Limitations:**
- Uses Windows `SendInput`, which marks input as injected (`LLMHF_INJECTED` / `LLKHF_INJECTED`). Many anti-cheat and protected titles will reject or flag it.
- Competitive online games may require a signed kernel-level input driver instead.

---

## HidHide

**What it does:** Hides the physical Nintendo Switch Pro Controller from Windows so games only see the virtual ViGEmBus pad. OxideLink whitelists itself so it can still read the controller.

**How to enable:**
1. Install the HidHide driver.
2. In OxideLink Settings, enable **Hide physical controller** and optionally **Auto-apply on startup**.

**Limitations:**
- HidHide requires administrator privileges to install and configure.
- A reboot is sometimes needed before the device is hidden correctly.
- The bundled `HidHideInstaller.exe` in this repo is a placeholder and must be replaced with the real installer before distribution.

---

## Gyro mouse

**What it does:** Converts Pro Controller gyroscope motion into desktop mouse movement or a virtual stick deflection.

**How to enable:**
1. Go to **Mappings > Gyro**.
2. Set **Mode** to `Mouse` for cursor control or `Stick(Left/Right)` to drive a stick.
3. Tune `sensitivity` (per-axis), `smoothing`, and `deadzone` (deg/s).

**Limitations:**
- Gyro data is only available after IMU reports are enabled; make sure the controller is fully initialized.
- `SendInput` mouse injection has the same anti-cheat caveats as KB/M.

---

## Virtual DS4 / Xbox

**What it does:** Presents the physical Pro Controller to Windows games as a virtual Xbox 360 or DualShock 4 gamepad through the ViGEmBus driver.

**How to enable:**
1. Install ViGEmBus.
2. In OxideLink Settings, set **Preferred virtual controller** to `Xbox360` or `DualShock4`.
3. The virtual pad connects automatically when a real controller is active.

**Limitations:**
- Requires ViGEmBus to be installed and running.
- The bundled `ViGEmBusSetup.exe` in this repo is a placeholder; replace it before shipping.

---

## Multi-controller

**What it does:** Supports up to four controller slots (`CONTROLLER_SLOTS = 4`), each with its own profile and state.

**How to enable:** Pair or plug in additional controllers and select the active slot in the UI. Per-controller profile overrides live in `AppConfig.per_controller_profile`.

**Limitations:**
- Slot switching is currently manual; auto-assignment of newly paired controllers is not yet implemented.

---

## DSU / Cemuhook

**What it does:** Broadcasts calibrated Pro Controller IMU (gyro + accel) data over UDP so emulators such as Cemu, Dolphin, and Ryujinx can use it as a motion source.

**How to enable:**
1. In Settings, enable the **DSU server**.
2. Configure the bind address and port (default `127.0.0.1:26760`).
3. Point your emulator's motion source at the same address.

**Limitations:**
- Only standard DSU/Cemuhook packets are sent; some emulators may need a specific controller slot to be selected.
- The server runs only while OxideLink is open.

---

## Flick Stick

**What it does:** Replaces the right stick with an absolute camera-yaw scheme: flick the stick to an edge to snap the camera to that angle, then hold the edge to rotate continuously.

**How to enable:**
1. Open **Mappings > Right Stick**.
2. Set **Mode** to `FlickStick`.
3. Tune threshold, rotation rate, deadzone, cooldown, and smoothing.

**Limitations:**
- Works best when the game expects a normal right-stick camera; native mouse support may be needed for the best PC FPS experience.
- See [flickstick.md](flickstick.md) for detailed tuning.

---

## NFC / amiibo

**What it does:** Loads `.bin` amiibo or NTAG215 dumps and emulates them when the controller reports an NFC/IR payload.

**How to enable:**
1. In the **NFC** tab, enable NFC.
2. Click **Load .bin** and choose a supported dump (540-byte PowerSaves or 572-byte raw NTAG215).
3. The controller will present the loaded tag while emulation is active.

**Limitations:**
- Only two dump sizes are validated: `540` and `572` bytes.
- This is a software emulation layer; behavior depends on how the game reads NFC data.

---

## Auto-updater

**What it does:** Checks for new OxideLink releases and installs them using `tauri-plugin-updater`.

**How to enable:** By default, the updater uses the bundled update server endpoints. To use a custom source, set **Update endpoint** in Settings.

**Limitations:**
- Updates must be signed correctly; a self-signed certificate will trigger SmartScreen warnings.
- A valid network connection and reachable endpoint are required.

---

## System tray

**What it does:** Keeps OxideLink running in the system tray, with options to minimize on close and start with Windows.

**How to enable:**
1. In Settings, enable **Close to tray** and/or **Run on login**.
2. Double-click the tray icon to restore the window.

**Limitations:**
- Tray balloon notifications depend on the Windows notification settings.

---

## In-game overlay

**What it does:** Intended to show a transparent, click-through Webview2 window with battery, profile, and FPS-style metrics plus quick profile switching.

**How to enable:** Not yet functional. The `overlay.rs` module is a Wave 4 placeholder.

**Limitations:**
- Commands (`toggle_overlay`, `get_overlay_config`, `set_overlay_config`) compile but do not create a real overlay window.

---

## Cloud / community

**What it does:** Planned profile upload, download, share codes, and a community profile browser.

**How to enable:** Not yet functional. `cloud.rs` is a Wave 4 placeholder.

**Limitations:**
- `list_community_profiles` and `upload_profile` return empty/stub results.
