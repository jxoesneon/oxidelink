# Controller shortcuts and remapping

OxideLink lets you map any Pro Controller button to another button, a keyboard key, a mouse action, a macro, or a system action such as profile switching.

## Default button layout

By default OxideLink maps the Switch Pro Controller face buttons to the Xbox/PC convention:

| Physical button | Default virtual output |
| --- | --- |
| A | B |
| B | A |
| X | Y |
| Y | X |

All other buttons (L, R, ZL, ZR, D-pad, +/-, Home, Capture, stick clicks) pass through 1:1.

This default remap lives in `AppConfig.button_remap`:

```json
{
  "button_remap": {
    "a_to": "b",
    "b_to": "a",
    "x_to": "y",
    "y_to": "x"
  }
}
```

## Default shortcuts

OxideLink ships with **no global controller shortcuts bound by default**. You decide which buttons trigger system actions.

### Suggested starter layout

If you want quick-access shortcuts, consider mapping them in **Mappings > Buttons**:

| Button combo | Suggested action |
| --- | --- |
| `Capture` | `GyroToggle` — turn gyro mouse/stick on or off |
| `Home` | `ProfileNext` — cycle to the next profile |
| `Home + Minus` | `ProfilePrev` — go back to the previous profile |
| `Home + Plus` | `ProfileNext` — also cycle forward |

You can also assign `Toggle` or `Turbo` actions to any button.

## How to remap a button

1. Open the **Mappings** tab.
2. Click the source button you want to change (for example, `A` or `Capture`).
3. Add one or more **Actions**:
   - `Button(ButtonId)` — press another controller button
   - `Key("w")` — press a keyboard key
   - `KeyCombo(["ctrl", "c"])` — press a key combination
   - `MouseButton(0)` — left mouse click (`0`=left, `1`=right, etc.)
   - `Macro("macro-id")` — run a saved macro
   - `ProfileNext` / `ProfilePrev` — switch profiles
   - `GyroToggle` — toggle gyro output
   - `Turbo { button, interval_ms }` — rapid fire a button
   - `Toggle { button }` — toggle a button on/off each press
   - `ShiftLayer(1)` — activate an alternate mapping layer
4. Click **Save profile**.

## Stick actions

Sticks can be mapped from **Mappings > Sticks**:

| Stick action | Behavior |
| --- | --- |
| `Disabled` | No output |
| `Mouse` | Move the desktop mouse cursor |
| `Wasd` | WASD keyboard movement |
| `ArrowKeys` | Arrow-key movement |
| `Stick(Left/Right)` | Emulate the left or right virtual stick |
| `Scroll` | Mouse wheel scrolling |

## Shift layers

`ShiftLayer` actions temporarily swap in a second set of mappings. A shift layer can be activated:

- `Always` — active by default
- `Hold(ButtonId)` — active while the button is held
- `Toggle(ButtonId)` — toggled each press

Use this to create "shift" shortcuts such as `ZL + face buttons` without losing the base layout.
