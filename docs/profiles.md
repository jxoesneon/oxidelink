# Profiles, auto-switch, and per-game config

OxideLink uses a profile manager to store multiple controller configurations and switch between them automatically or manually.

## Profile file

Profiles are stored at:

```text
%AppData%\OxideLink\profiles.json
```

This file is created automatically when you save your first profile.

## Creating a profile

1. Open the **Profiles** tab.
2. Click **New profile** and give it a name.
3. Adjust mappings, deadzones, gyro, Flick Stick, NFC, and other settings.
4. Click **Save**.

Each profile has:

- `id` — generated automatically (`profile-<timestamp>-<counter>`).
- `name` — displayed in the UI.
- `enabled` — whether the profile can be selected.
- `auto_rules` — rules that decide when the profile should be activated.
- `nfc` — per-profile NFC/amiibo configuration.
- `right_stick` — per-profile right-stick / Flick Stick configuration.

## Auto-switch rules

A rule has three parts:

| Field | Options | Description |
| --- | --- | --- |
| `kind` | `ProcessPath` / `WindowTitle` | Match the foreground executable path or window title. |
| `pattern` | any string | The value to match against. |
| `match_mode` | `Exact` / `Contains` / `Regex` | How `pattern` is compared. |
| `enabled` | `true` / `false` | Whether the rule participates in matching. |

Examples:

```json
{
  "kind": "ProcessPath",
  "pattern": "rocketleague.exe",
  "match_mode": "Contains",
  "enabled": true
}
```

```json
{
  "kind": "WindowTitle",
  "pattern": "^Celeste$",
  "match_mode": "Regex",
  "enabled": true
}
```

## Enabling auto-switch

1. In the **Profiles** tab, toggle **Auto-switch profiles**.
2. OxideLink polls the active window once per second.
3. When the foreground window or process matches a profile's rule, that profile becomes active.
4. If no rule matches, the **default profile** is used.

## Default profile

Set a profile as the default. It is used when:

- Auto-switch is disabled.
- No auto-rule matches the active window.
- A controller slot has no per-slot override.

## Per-controller profile overrides

`AppConfig.per_controller_profile` is a list of up to four profile IDs (one per slot). A non-null value forces that slot to use that profile regardless of the global active profile. This is useful when each player wants a different setup in local multiplayer.

## Import and export

### From the UI

- **Export all profiles** saves the current profile manager state as JSON.
- **Import profiles** replaces the current in-memory manager from JSON and persists it.

### From Tauri commands

```javascript
// Export to a file
await invoke("export_profiles", { path: "C:\\path\\to\\profiles.json" });

// Import from a file (replaces the in-memory store)
await invoke("import_profiles", { path: "C:\\path\\to\\profiles.json" });
```

You can also manually back up `%AppData%\OxideLink\profiles.json` and restore it later.

## Limitations

- The auto-switch poll interval is fixed at one second, so very fast window changes may not be detected.
- `WindowTitle` matching depends on the game setting a stable window title.
- Profile IDs are generated automatically; renaming a profile keeps the same ID.
