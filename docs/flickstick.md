# Flick Stick setup and tuning

Flick Stick maps the right stick to absolute camera yaw. Flick to an edge to snap the camera to that direction, then hold the edge to rotate continuously.

## Enable Flick Stick

1. Open **Mappings > Right Stick**.
2. Set **Mode** to `FlickStick`.
3. Adjust the tuning parameters below.

## Tuning parameters

| Parameter | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Master toggle for Flick Stick processing. |
| `flick_threshold` | `0.90` | Stick magnitude (0.0-1.0) that counts as a full-deflection flick. |
| `rotate_rate_deg_per_sec` | `360.0` | Continuous rotation rate while the stick is held at the edge. |
| `stick_deadzone` | `0.15` | Stick magnitude below which input is ignored. |
| `flick_cooldown_ms` | `150` | Minimum time between two flick events. |
| `output_smoothing` | `0.0` | Output smoothing factor (0.0 = none, 1.0 = maximum). |

## How it works

- The stick angle is measured from the positive Y axis: stick up = `0°`, right = `+90°`, left = `-90°`, down = `180°`.
- When the stick crosses `flick_threshold` and the cooldown has elapsed, the camera yaw snaps to that angle.
- While the stick remains past the threshold, the camera rotates continuously at `rotate_rate_deg_per_sec` scaled by the horizontal component.
- The delta is normalized to the `[-180, 180]` range to take the shortest rotation path.

## Recommended tuning

| Game style | `flick_threshold` | `rotate_rate_deg_per_sec` | `stick_deadzone` |
| --- | --- | --- | --- |
| Fast paced / FPS | `0.95` | `480` | `0.10` |
| Third-person adventure | `0.85` | `300` | `0.15` |
| Precision / simulator | `0.90` | `180` | `0.20` |

Lower the cooldown if you want rapid re-flicks; raise it if you accidentally double-flick.

## Limitations

- Flick Stick is a right-stick mode; it cannot be active at the same time as the default camera/stick mode for that stick.
- Smoothing adds latency; set it to `0.0` for the most responsive flicks.
- Not every game reads right-stick input as pure camera yaw; games with acceleration or smoothing may need additional in-game tuning.

## Restoring camera

There is no dedicated "recenter" command for Flick Stick. To reset the camera, momentarily release the right stick; the current yaw resets on the next flick.
