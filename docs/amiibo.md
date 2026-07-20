# amiibo / NFC emulation

OxideLink can load an amiibo or NTAG215 `.bin` dump and present it to the console/game through the controller's NFC/IR report path.

## Supported dump formats

Two file sizes are validated:

| Size | Type |
| --- | --- |
| 540 bytes | PowerSaves-style trimmed dump |
| 572 bytes | Raw full NTAG215 dump |

Dumps with other sizes are rejected with an error.

## How to load a `.bin`

### From the UI

1. Open the **NFC** tab.
2. Enable **NFC / amiibo**.
3. Click **Load .bin** and choose your dump.
4. The UID and size are shown; the tag is marked as present.

### From a Tauri command

```javascript
await invoke("load_amiibo_bin", { path: "C:\\path\\to\\amiibo.bin" });
```

The command reads the file, validates the header, enables NFC, stores the path in `AppConfig.nfc.emulate_bin`, and updates the controller state.

### Validate without enabling

To check a dump without changing runtime state:

```javascript
const state = await invoke("emulate_amiibo_from_path", { path: "C:\\path\\to\\amiibo.bin" });
```

This returns the parsed `NfcState` but does not persist it.

## What gets emulated

- The UID is extracted from the first 4 or 7 bytes (NXP manufacturer prefix `0x04` uses 7 bytes).
- The full `.bin` bytes are stored in `NfcState.amiibo_data` for downstream reporting.
- `NfcState.tag_present` is set to `true`.

## Limitations

- OxideLink validates the header and size only; it does not decrypt, verify signatures, or emulate full NTAG215 anticollision.
- Some games require a specific timing sequence or re-scan behavior that may not match real hardware.
- Raw 572-byte dumps may contain dynamic lock bytes and config pages, but these are not modified by OxideLink.
- NFC/IR reports are tied to the standard 0x31 input report path.

## Troubleshooting

- **"Unsupported amiibo/NTAG215 dump size"** — the file is not 540 or 572 bytes. Re-dump or trim it with a known-good tool.
- **No tag is detected in-game** — ensure NFC is enabled, the `.bin` is valid, and the controller is sending 0x31 reports.
