# Gyro-to-mouse tuning

OxideLink converts calibrated Pro Controller gyroscope motion into either desktop mouse movement or a virtual stick deflection.

## Gyro modes

In **Mappings > Gyro**, choose one of the following modes:

| Mode | Output |
| --- | --- |
| `Off` | Gyro is ignored. |
| `Mouse` | Cursor movement via `SendInput`. |
| `Stick(Left)` | Left virtual stick deflection. |
| `Stick(Right)` | Right virtual stick deflection. |
| `FlickStick` | No mouse motion; used by the right-stick Flick Stick path. |

## Tuning parameters

The `GyroMapping` struct controls the behavior:

| Parameter | Default | Description |
| --- | --- | --- |
| `mode` | `Off` | Select one of the modes above. |
| `sensitivity` | `[1.0, 1.0]` | Per-axis multiplier. Index 0 is yaw (horizontal), index 1 is pitch (vertical). |
| `smoothing` | `0.0` | Exponential moving average weight. Higher = smoother but more latency. Clamped to `[0.0, 0.99]`. |
| `deadzone` | `0.0` | Minimum deg/s below which an axis is treated as zero. |

## Axis mapping

- `gyro_y` is treated as **yaw** (left/right turn) and maps to horizontal mouse/stick movement.
- `gyro_x` is treated as **pitch** (up/down tilt) and is inverted so tilting the controller up moves the cursor up.

## How values are computed

- **Mouse**: `delta = smoothed_value * sensitivity * dt`, rounded to whole pixels.
- **Stick**: `output = clamp(smoothed_value * sensitivity, -1.0, 1.0)`. The Y axis is flipped for the left stick to keep natural ergonomics.

## Suggested tuning workflow

1. Set `mode` to `Mouse`.
2. Start with `sensitivity = [1.0, 1.0]` and `smoothing = 0.0`.
3. Hold the controller steady and increase `deadzone` until drift stops.
4. Raise `sensitivity` until a comfortable 90° turn maps to a reasonable on-screen distance.
5. If the cursor jitters, raise `smoothing` slightly (e.g., `0.2`-`0.4`); too much smoothing will feel laggy.
6. For a flick-shoting setup, keep `smoothing` low and use a high `sensitivity`.

## Recentering

Call `gyro_recenter` or click **Recenter gyro** in the UI to reset the smoothing accumulators and stick output.

```javascript
await invoke("gyro_recenter");
```

## Limitations

- Gyro requires the controller IMU to be enabled. If no IMU reports arrive, the gyro output is zero.
- `SendInput` mouse events are injected and may be blocked by anti-cheat software.
- Stick-mode gyro output is subject to the same deadzone and response curve settings as physical stick input.
