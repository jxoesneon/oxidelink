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

    // --- Engine defaults ----------------------------------------------------

    #[test]
    fn new_engine_has_default_interval_and_duty() {
        let engine = TurboEngine::new();
        // Defaults are 100 ms / 0.5 duty. We verify behaviour indirectly by
        // driving a turbo with interval_ms == 0 (which falls back to global).
        let mut e = engine;
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 0, // use global
                }],
            }],
            ..Default::default()
        };

        // First frame: target pressed.
        let out = e.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));

        // 60 ms later with 50 ms on-phase (global 100ms / 0.5 duty) -> off.
        let out = e.update(&buttons, 0.060, &mappings);
        assert!(!out.get(ButtonId::B));
    }

    // --- set_global clamping ------------------------------------------------

    #[test]
    fn set_global_clamps_interval_to_minimum_one() {
        let mut engine = TurboEngine::new();
        engine.set_global(0, 0.5);
        // interval is clamped to 1 ms internally; drive a turbo with global
        // fallback to confirm it still oscillates rather than dividing by zero.
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 0,
                }],
            }],
            ..Default::default()
        };
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));
        // A large dt should flip it off (on-phase = 1ms * 0.5 = 0.5ms).
        let out = engine.update(&buttons, 1.0, &mappings);
        assert!(!out.get(ButtonId::B));
    }

    #[test]
    fn set_global_clamps_duty_cycle_to_zero_and_one() {
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 0,
                }],
            }],
            ..Default::default()
        };

        // duty clamped to 0 -> on-phase is 0 ms, so any dt flips to off.
        let mut engine = TurboEngine::new();
        engine.set_global(100, -1.0);
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));
        let out = engine.update(&buttons, 0.001, &mappings);
        assert!(!out.get(ButtonId::B), "zero duty should turn off quickly");

        // Now duty clamped to 1.0 -> on-phase is the full interval (100 ms),
        // off-phase is 0 ms. The button stays on for 100 ms then flips off and
        // immediately back on over consecutive frames.
        let mut engine2 = TurboEngine::new();
        engine2.set_global(100, 2.0);
        let out = engine2.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));
        // Small dt: still within the 100 ms on-phase.
        let out = engine2.update(&buttons, 0.001, &mappings);
        assert!(out.get(ButtonId::B));
        // Cross the 100 ms on-phase -> flips off.
        let out = engine2.update(&buttons, 0.110, &mappings);
        assert!(!out.get(ButtonId::B));
        // Any further dt: off-phase is 0 ms, so it flips back on immediately.
        let out = engine2.update(&buttons, 0.001, &mappings);
        assert!(out.get(ButtonId::B));
    }

    // --- Duty cycle logic ---------------------------------------------------

    #[test]
    fn custom_duty_cycle_extends_on_phase() {
        let mut engine = TurboEngine::new();
        engine.set_global(100, 0.75); // 75 ms on / 25 ms off
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 0, // use global
                }],
            }],
            ..Default::default()
        };

        // First frame: pressed.
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));
        // 60 ms: still on (on-phase is 75 ms).
        let out = engine.update(&buttons, 0.060, &mappings);
        assert!(out.get(ButtonId::B));
        // 20 ms more (total 80 ms): now past 75 ms on-phase -> off.
        let out = engine.update(&buttons, 0.020, &mappings);
        assert!(!out.get(ButtonId::B));
        // 30 ms more: past 25 ms off-phase -> on.
        let out = engine.update(&buttons, 0.030, &mappings);
        assert!(out.get(ButtonId::B));
    }

    #[test]
    fn per_mapping_interval_overrides_global() {
        let mut engine = TurboEngine::new();
        engine.set_global(1000, 0.5); // global would be 500ms on
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 40, // 20 ms on / 20 ms off
                }],
            }],
            ..Default::default()
        };

        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::B));
        // 25 ms: past 20 ms on-phase -> off (proves per-mapping interval used).
        let out = engine.update(&buttons, 0.025, &mappings);
        assert!(!out.get(ButtonId::B));
    }

    // --- Toggle state machine ----------------------------------------------

    #[test]
    fn toggle_double_press_in_one_frame_is_no_op() {
        let mut engine = TurboEngine::new();
        // Two mappings from the same source to the same toggle target produce
        // two flips in a single update -> even count -> no net change.
        let mappings = Mappings {
            buttons: vec![
                ButtonMapping {
                    source: ButtonId::A,
                    actions: vec![Action::Toggle {
                        button: ButtonId::B,
                    }],
                },
                ButtonMapping {
                    source: ButtonId::X,
                    actions: vec![Action::Toggle {
                        button: ButtonId::B,
                    }],
                },
            ],
            ..Default::default()
        };

        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        buttons.set(ButtonId::X, true);

        // Both rise in the same frame -> 2 flips -> B stays off.
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(!out.get(ButtonId::B));

        // Release both.
        let mut released = ButtonState::default();
        engine.update(&released, 0.0, &mappings);

        // Now press only A -> 1 flip -> B on.
        released.set(ButtonId::A, true);
        let out = engine.update(&released, 0.0, &mappings);
        assert!(out.get(ButtonId::B));
    }

    #[test]
    fn toggle_state_persists_across_releases() {
        let mut engine = TurboEngine::new();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Toggle {
                    button: ButtonId::Y,
                }],
            }],
            ..Default::default()
        };

        let mut buttons = ButtonState::default();
        // Press -> on.
        buttons.set(ButtonId::A, true);
        assert!(engine.update(&buttons, 0.0, &mappings).get(ButtonId::Y));
        // Hold -> stays on.
        assert!(engine.update(&buttons, 0.1, &mappings).get(ButtonId::Y));
        // Release -> stays on.
        buttons.set(ButtonId::A, false);
        assert!(engine.update(&buttons, 0.1, &mappings).get(ButtonId::Y));
        // Press again -> off.
        buttons.set(ButtonId::A, true);
        assert!(!engine.update(&buttons, 0.0, &mappings).get(ButtonId::Y));
        // Press again -> on.
        buttons.set(ButtonId::A, false);
        engine.update(&buttons, 0.0, &mappings);
        buttons.set(ButtonId::A, true);
        assert!(engine.update(&buttons, 0.0, &mappings).get(ButtonId::Y));
    }

    // --- Stale state cleanup -----------------------------------------------

    #[test]
    fn removing_mapping_clears_turbo_state() {
        let mut engine = TurboEngine::new();
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);

        let with_mapping = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![Action::Turbo {
                    button: ButtonId::B,
                    interval_ms: 100,
                }],
            }],
            ..Default::default()
        };
        // Prime the turbo state.
        engine.update(&buttons, 0.0, &with_mapping);

        // Remove the mapping entirely.
        let empty = Mappings::default();
        let out = engine.update(&buttons, 0.0, &empty);
        // Target is no longer overridden; B defaults to false (not held).
        assert!(!out.get(ButtonId::B));
        // Source still passes through.
        assert!(out.get(ButtonId::A));
    }

    #[test]
    fn removing_mapping_clears_toggle_state() {
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
        buttons.set(ButtonId::A, true);
        // Toggle B on.
        engine.update(&buttons, 0.0, &mappings);

        // Remove mapping; B should revert to its physical (false) state.
        let empty = Mappings::default();
        buttons.set(ButtonId::A, false);
        let out = engine.update(&buttons, 0.0, &empty);
        assert!(!out.get(ButtonId::B));
    }

    // --- Multiple sources / targets ----------------------------------------

    #[test]
    fn multiple_sources_feed_same_turbo_target() {
        let mut engine = TurboEngine::new();
        let mappings = Mappings {
            buttons: vec![
                ButtonMapping {
                    source: ButtonId::A,
                    actions: vec![Action::Turbo {
                        button: ButtonId::B,
                        interval_ms: 100,
                    }],
                },
                ButtonMapping {
                    source: ButtonId::X,
                    actions: vec![Action::Turbo {
                        button: ButtonId::B,
                        interval_ms: 100,
                    }],
                },
            ],
            ..Default::default()
        };

        // Hold only A: turbo starts.
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        assert!(engine.update(&buttons, 0.0, &mappings).get(ButtonId::B));

        // Release A but hold X: target should stay held (any_held true).
        let mut buttons2 = ButtonState::default();
        buttons2.set(ButtonId::X, true);
        // First frame after switch: X rising, A falling. Turbo stays on.
        let out = engine.update(&buttons2, 0.0, &mappings);
        assert!(out.get(ButtonId::B));

        // Release everything: target resets to off.
        let released = ButtonState::default();
        let out = engine.update(&released, 0.0, &mappings);
        assert!(!out.get(ButtonId::B));
    }

    #[test]
    fn non_turbo_actions_are_ignored_by_engine() {
        let mut engine = TurboEngine::new();
        let mappings = Mappings {
            buttons: vec![ButtonMapping {
                source: ButtonId::A,
                actions: vec![
                    Action::Button(ButtonId::B),
                    Action::Key("Space".to_string()),
                    Action::ProfileNext,
                ],
            }],
            ..Default::default()
        };
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        // No turbo/toggle actions -> B passes through its physical state (false).
        let out = engine.update(&buttons, 0.0, &mappings);
        assert!(out.get(ButtonId::A));
        assert!(!out.get(ButtonId::B));
    }

    #[test]
    fn empty_mappings_passes_through() {
        let mut engine = TurboEngine::new();
        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        buttons.set(ButtonId::B, true);
        let out = engine.update(&buttons, 0.016, &Mappings::default());
        assert!(out.get(ButtonId::A));
        assert!(out.get(ButtonId::B));
    }

    #[test]
    fn turbo_resets_after_release_and_repress() {
        let mut engine = TurboEngine::new();
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

        let mut buttons = ButtonState::default();
        buttons.set(ButtonId::A, true);
        // Press -> on.
        assert!(engine.update(&buttons, 0.0, &mappings).get(ButtonId::B));
        // 60 ms -> off.
        assert!(!engine.update(&buttons, 0.060, &mappings).get(ButtonId::B));

        // Release -> resets.
        buttons.set(ButtonId::A, false);
        engine.update(&buttons, 0.0, &mappings);

        // Repress -> on again (fresh start).
        buttons.set(ButtonId::A, true);
        assert!(engine.update(&buttons, 0.0, &mappings).get(ButtonId::B));
    }
}
