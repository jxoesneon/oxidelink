# Telemetry and crash reporting

OxideLink includes optional usage telemetry and optional crash reporting. Both are **opt-in** and disabled by default.

## Feature usage telemetry

Telemetry records a small allow-list of feature events to help the team understand which features are used.

### Allowed events

The telemetry system only records these events:

- `profile_switched`
- `kbm_enabled`
- `macro_played`
- `hidhide_enabled`
- `gyro_mouse_used`
- `turbo_button_set`

Any other event name is dropped.

### What is collected

- Event name and timestamp
- A lightweight payload containing feature metadata (for example, which profile was switched, or that a macro was triggered)

### What is NOT collected

Before telemetry is sent, payloads are scrubbed. The following keys are redacted, along with MAC addresses, file paths, IP addresses, and long alphanumeric serials:

- `mac`, `mac_address`
- `serial`, `serial_number`
- `path`, `file_path`, `device_path`
- `address`, `ip`, `ip_address`
- `firmware`, `bluetooth_address`

String values are also run through a PII scrubber that removes MAC addresses, paths, IPs, and long serial numbers.

### How to enable or disable

In Settings:

- **Telemetry** — toggle off by default.
- Provide an **Aptabase app key** to send events to the cloud, or leave it blank to log events locally (debug log).

You can also set the `OXIDELINK_TELEMETRY_FILE` environment variable to write local telemetry events to a specific JSON file.

### Backends

- `noop` — telemetry disabled.
- `debug` — events are written to the local debug log and optionally to `OXIDELINK_TELEMETRY_FILE`.
- `aptabase` — events are sent to `https://eu.aptabase.com`, `https://us.aptabase.com`, or `https://analytics.aptabase.com` depending on the key prefix.

Events are buffered and flushed in batches of 10.

## Crash reporting

Crash reporting captures Rust panics and sends them to a configured Sentry DSN. It is disabled by default.

### What is collected

- Panic message and stack trace
- The Sentry event is created after stripping PII (MACs, paths, IPs, serials)

### How to enable or disable

In Settings:

- **Crash reporting** — toggle off by default.
- **Sentry DSN** — provide a valid Sentry DSN, or use the literal string `test` for local file mode.

When `test` is used (or the `OXIDELINK_CRASH_TEST` environment variable is set), panic reports are written to a local file instead of being uploaded.

### Test mode

Test mode lets you verify crash reporting without sending data to a remote server. Reports are written to a local file path derived from the temporary directory.

## Privacy summary

| Feature | Default | Data sent | Remote endpoint |
| --- | --- | --- | --- |
| Usage telemetry | Disabled | Event names + scrubbed metadata | Optional Aptabase (key required) |
| Crash reporting | Disabled | Panic stack trace + message | Optional Sentry (DSN required) |

No telemetry or crash data leaves your machine unless you explicitly provide an endpoint/key.

## Verifying your settings

Use the in-app **Settings > Diagnostics** panel to view the active telemetry backend and crash-reporting status. The key/DSN is displayed in a redacted form (last 4 characters only).
