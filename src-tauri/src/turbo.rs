//! Turbo / rapid-fire and toggle engine.
//!
//! `TurboEngine` reads the physical [`ButtonState`] plus the configured
//! `Mappings` and produces a virtual [`ButtonState`] where turbo/toggle actions
//! override the physical output of their target buttons. Non-turbo buttons are
//! passed through unchanged.

use std::collections::HashMap;

use crate::state::AppCtx;
use crate::state::{Action, ButtonId, ButtonMapping, ButtonState, IpcEvent, Mappings};

/// Per-target turbo oscillator state.
#[derive(Debug, Clone, Default)]
struct TurboState {
    /// Whether any mapped source is currently held.
    held: bool,
    /// Current virtual output of the target button.
    output: bool,
    /// Time accumulated in the current on/off phase (seconds).
    elapsed: f32,
}

/// Engine that applies rapid-fire and toggle mappings to a [`ButtonState`].
#[derive(Debug, Clone, Default)]
pub struct TurboEngine {
    /// Last known physical held state per source button (used for edge detection).
    last_held: HashMap<ButtonId, bool>,
    /// Current virtual output for toggle-mapped targets.
    toggle_outputs: HashMap<ButtonId, bool>,
    /// Rapid-fire oscillator state per target button.
    turbo_states: HashMap<ButtonId, TurboState>,
    /// Global fallback turbo period (ms).
    global_interval_ms: u32,
    /// Global turbo duty cycle (0.0–1.0).
    duty_cycle: f32,
}

impl TurboEngine {
    /// Create a new engine with 100 ms / 50 % defaults.
    pub fn new() -> Self {
        Self {
            global_interval_ms: 100,
            duty_cycle: 0.5,
            ..Default::default()
        }
    }

    /// Set the global fallback turbo interval and duty cycle.
    ///
    /// These are used when a `Turbo` action has `interval_ms == 0`.
    pub fn set_global(&mut self, interval_ms: u32, duty_cycle: f32) {
        self.global_interval_ms = interval_ms.max(1);
        self.duty_cycle = duty_cycle.clamp(0.0, 1.0);
    }

    /// Apply turbo and toggle mappings to `buttons` and return the virtual state.
    ///
    /// `dt` is the elapsed time since the last call in seconds.
    pub fn update(&mut self, buttons: &ButtonState, dt: f32, mappings: &Mappings) -> ButtonState {
        let mut out = buttons.clone();

        // Determine which targets are still referenced so we can reset stale state
        // when a mapping is removed.
        let mut active_targets: HashMap<ButtonId, bool> = HashMap::new();

        // Pass 1: collect requests per target.
        let mut turbo_requests: HashMap<ButtonId, (bool, u32)> = HashMap::new();
        let mut toggle_flips: HashMap<ButtonId, u8> = HashMap::new();

        for mapping in &mappings.buttons {
            let source_held = buttons.get(mapping.source);
            let was_held = *self.last_held.get(&mapping.source).unwrap_or(&false);
            let rising = source_held && !was_held;
            self.last_held.insert(mapping.source, source_held);

            for action in &mapping.actions {
                match action {
                    Action::Turbo {
                        button,
                        interval_ms,
                    } => {
                        let (any_held, stored_interval) =
                            turbo_requests.entry(*button).or_insert((false, 0));
                        *any_held |= source_held;
                        if *stored_interval == 0 {
                            *stored_interval = *interval_ms;
                        }
                        active_targets.insert(*button, true);
                    }
                    Action::Toggle { button } => {
                        if rising {
                            *toggle_flips.entry(*button).or_insert(0) += 1;
                        }
                        active_targets.insert(*button, true);
                    }
                    _ => {}
                }
            }
        }

        // Remove stale state for targets that are no longer mapped.
        self.toggle_outputs
            .retain(|k, _| active_targets.contains_key(k));
        self.turbo_states
            .retain(|k, _| active_targets.contains_key(k));

        // Pass 2: evaluate toggle outputs.
        for (target, flips) in toggle_flips {
            if flips > 0 {
                let current = self.toggle_outputs.get(&target).copied().unwrap_or(false);
                let new_state = if flips % 2 == 1 { !current } else { current };
                self.toggle_outputs.insert(target, new_state);
            }
        }

        for (target, state) in &self.toggle_outputs {
            out.set(*target, *state);
        }

        // Pass 3: evaluate turbo oscillators.
        for (target, (any_held, interval_ms)) in turbo_requests {
            let interval = if interval_ms > 0 {
                interval_ms
            } else {
                self.global_interval_ms
            }
            .max(1);

            let state = self.turbo_states.entry(target).or_default();
            if !any_held {
                state.held = false;
                state.output = false;
                state.elapsed = 0.0;
            } else {
                if !state.held {
                    // First report where the source is held: start with the button pressed.
                    state.held = true;
                    state.output = true;
                    state.elapsed = 0.0;
                } else {
                    // Accumulate time and flip at the configured duty boundaries.
                    state.elapsed += dt;
                    let on_dur = (interval as f32) * self.duty_cycle / 1000.0;
                    let off_dur = (interval as f32) * (1.0 - self.duty_cycle) / 1000.0;
                    let threshold = if state.output { on_dur } else { off_dur };
                    if state.elapsed >= threshold {
                        state.elapsed -= threshold;
                        state.output = !state.output;
                    }
                }
            }

            out.set(target, state.output);
        }

        out
    }
}

/// Update the mapping for `button` to the supplied `action`.
///
/// If the button already has a mapping entry, its action list is replaced;
/// otherwise a new entry is appended. The updated `Mappings` are returned.
#[tauri::command]
pub fn set_turbo_button(
    ctx: tauri::State<'_, AppCtx>,
    button: ButtonId,
    action: Action,
) -> Mappings {
    let mappings = {
        let mut cfg = ctx.shared.config.write();
        if let Some(idx) = cfg.mappings.buttons.iter().position(|m| m.source == button) {
            cfg.mappings.buttons[idx].actions = vec![action];
        } else {
            cfg.mappings.buttons.push(ButtonMapping {
                source: button,
                actions: vec![action],
            });
        }
        cfg.mappings.clone()
    };

    // Persist and broadcast like a normal config change.
    let cfg = ctx.shared.config.read().clone();
    if cfg.config_persistence_enabled {
        if let Err(e) = crate::config::save_config(&cfg) {
            log::warn!("Failed to save turbo config: {}", e);
        }
    }
    let _ = ctx.tx.send(IpcEvent::ConfigUpdated { data: cfg });

    mappings
}

/// Return the current turbo/toggle mapping configuration.
#[tauri::command]
pub fn get_turbo_settings(ctx: tauri::State<'_, AppCtx>) -> Mappings {
    ctx.shared.config.read().mappings.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turbo_oscillates_while_held() {
        let mut engine = TurboEngine::new();
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);

        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 100, // 50 ms on / 50 ms off at 0.5 duty
                }],
            }],
            ..Default::default()
        };

        // First update: target starts pressed.
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));

        // After 60 ms: should have flipped to off (on phase 50 ms exceeded).
        let out = engine.update(&buttons, 0.060, &mappings);
        assert!(!out.get(ButtonId::B));

        // After another 60 ms: off phase 50 ms exceeded, flip back on.
        let out = engine.update(&buttons, 0.060, &mappings);
        assert!(out.get(ButtonId::B));

        // Source released: target should turn off and reset.
        let mut released = ButtonState::default();
        released.set(ButtonId::A, false);
        let out = engine.update(&released, 0.010, &mappings);
        assert!(!out.get(ButtonId::B));
    }

    #[test]
    fn toggle_flips_on_press_edge() {
        let mut engine = TurboEngine::new();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Toggle {
                    button: ButtonId::B,
                }],
            }],
            ..Default::default()
        };

        let mut buttons = ButtonState::default();

        // No press: B stays off.
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(!out.get(ButtonId::B));

        // Press edge: B toggles on.
        buttons.set(ButtonId::A, true);
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));

        // Held: B stays on.
        let out = engine.update(&buttons, 0.016, &mappings);
        assert!(out.get(ButtonId::B));

        // Release: B stays on.
        buttons.set(ButtonId::A, false);
        let out = engine.update(&buttons, 0.016, &mappings);
        assert!(out.get(ButtonId::B));

        // Next press edge: B toggles off.
        buttons.set(ButtonId::A, true);
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(!out.get(ButtonId::B));
    }

    #[test]
    fn pass_through_unmapped_buttons() {
        let mut engine = TurboEngine::new();
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        buttons.set(ButtonId::L, true);

        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 100,
                }],
            }],
            ..Default::default()
        };

        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::A));
        assert!(out.get(ButtonId::L));
    }
}
