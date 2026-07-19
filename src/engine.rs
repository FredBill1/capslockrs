//! Platform-independent input state machine.

pub const VK_BACK: u32 = 0x08;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_SPACE: u32 = 0x20;
pub const VK_END: u32 = 0x23;
pub const VK_HOME: u32 = 0x24;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_DELETE: u32 = 0x2E;
pub const VK_A: u32 = 0x41;
pub const VK_B: u32 = 0x42;
pub const VK_D: u32 = 0x44;
pub const VK_E: u32 = 0x45;
pub const VK_F: u32 = 0x46;
pub const VK_G: u32 = 0x47;
pub const VK_H: u32 = 0x48;
pub const VK_I: u32 = 0x49;
pub const VK_J: u32 = 0x4A;
pub const VK_K: u32 = 0x4B;
pub const VK_L: u32 = 0x4C;
pub const VK_N: u32 = 0x4E;
pub const VK_O: u32 = 0x4F;
pub const VK_P: u32 = 0x50;
pub const VK_Q: u32 = 0x51;
pub const VK_R: u32 = 0x52;
pub const VK_S: u32 = 0x53;
pub const VK_T: u32 = 0x54;
pub const VK_U: u32 = 0x55;
pub const VK_W: u32 = 0x57;
pub const VK_Y: u32 = 0x59;
pub const VK_F13: u32 = 0x7C;
pub const VK_F14: u32 = 0x7D;
pub const VK_F15: u32 = 0x7E;
pub const VK_OEM_1: u32 = 0xBA;
pub const VK_OEM_PERIOD: u32 = 0xBE;
pub const VK_OEM_2: u32 = 0xBF;
pub const VK_OEM_6: u32 = 0xDD;
pub const VK_CAPITAL: u32 = 0x14;

pub const MOD_CTRL: u8 = 1;
pub const MOD_SHIFT: u8 = 2;
pub const MOD_ALT: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyAction {
    pub key: u32,
    pub modifiers: u8,
    pub repeat: u8,
}

impl KeyAction {
    pub const fn new(key: u32, modifiers: u8, repeat: u8) -> Self {
        Self {
            key,
            modifiers,
            repeat,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    Key(KeyAction),
    Click { index: usize, active: bool },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Outcome {
    pub suppress: bool,
    pub effect: Option<Effect>,
}

#[derive(Debug, Default)]
pub struct Engine {
    caps_down: bool,
    caps_down_at: u32,
    caps_used: bool,
    suppressed: [u64; 4],
    click_active: [bool; 3],
}

impl Engine {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn synchronize_physical(
        &mut self,
        caps_down: bool,
        click_active: [bool; 3],
        timestamp_ms: u32,
    ) {
        self.reset();
        self.caps_down = caps_down;
        self.caps_down_at = timestamp_ms;
        // Never synthesize Esc for a Caps press whose original down event was not observed.
        self.caps_used = caps_down;
        self.click_active = click_active;
    }

    pub fn process(
        &mut self,
        vk: u32,
        is_down: bool,
        timestamp_ms: u32,
        standard_modifier_down: bool,
    ) -> Outcome {
        if vk == VK_CAPITAL {
            if is_down {
                if !self.caps_down {
                    self.caps_down = true;
                    self.caps_down_at = timestamp_ms;
                    self.caps_used = false;
                }
                return Outcome {
                    suppress: true,
                    effect: None,
                };
            }

            if self.caps_down {
                let elapsed = timestamp_ms.wrapping_sub(self.caps_down_at);
                let send_escape = elapsed < 300 && !self.caps_used;
                self.caps_down = false;
                return Outcome {
                    suppress: true,
                    effect: send_escape.then_some(Effect::Key(KeyAction::new(VK_ESCAPE, 0, 1))),
                };
            }

            return Outcome {
                suppress: true,
                effect: None,
            };
        }

        if let Some(index) = click_index(vk) {
            if is_down {
                if standard_modifier_down {
                    return Outcome::default();
                }
                if !self.click_active[index] {
                    self.click_active[index] = true;
                    return Outcome {
                        suppress: true,
                        effect: Some(Effect::Click {
                            index,
                            active: true,
                        }),
                    };
                }
                return Outcome {
                    suppress: true,
                    effect: None,
                };
            }

            if self.click_active[index] {
                self.click_active[index] = false;
                return Outcome {
                    suppress: true,
                    effect: Some(Effect::Click {
                        index,
                        active: false,
                    }),
                };
            }
            return Outcome::default();
        }

        if self.is_suppressed(vk) {
            if !is_down {
                self.set_suppressed(vk, false);
                return Outcome {
                    suppress: true,
                    effect: None,
                };
            }
            return Outcome {
                suppress: true,
                effect: (self.caps_down && !standard_modifier_down)
                    .then(|| mapping(vk))
                    .flatten()
                    .map(Effect::Key),
            };
        }

        if is_down
            && self.caps_down
            && !standard_modifier_down
            && let Some(action) = mapping(vk)
        {
            self.caps_used = true;
            self.set_suppressed(vk, true);
            return Outcome {
                suppress: true,
                effect: Some(Effect::Key(action)),
            };
        }

        Outcome::default()
    }

    fn is_suppressed(&self, vk: u32) -> bool {
        let index = (vk / 64) as usize;
        index < self.suppressed.len() && self.suppressed[index] & (1_u64 << (vk % 64)) != 0
    }

    fn set_suppressed(&mut self, vk: u32, value: bool) {
        let index = (vk / 64) as usize;
        if index >= self.suppressed.len() {
            return;
        }
        let mask = 1_u64 << (vk % 64);
        if value {
            self.suppressed[index] |= mask;
        } else {
            self.suppressed[index] &= !mask;
        }
    }
}

pub fn mapping(vk: u32) -> Option<KeyAction> {
    let action = match vk {
        VK_E => KeyAction::new(VK_UP, 0, 1),
        VK_S => KeyAction::new(VK_LEFT, 0, 1),
        VK_D => KeyAction::new(VK_DOWN, 0, 1),
        VK_F => KeyAction::new(VK_RIGHT, 0, 1),
        VK_A => KeyAction::new(VK_LEFT, MOD_CTRL, 1),
        VK_G => KeyAction::new(VK_RIGHT, MOD_CTRL, 1),
        VK_W => KeyAction::new(VK_BACK, 0, 1),
        VK_R => KeyAction::new(VK_DELETE, 0, 1),
        VK_T => KeyAction::new(VK_UP, 0, 5),
        VK_B => KeyAction::new(VK_DOWN, 0, 5),
        VK_I => KeyAction::new(VK_UP, MOD_SHIFT, 1),
        VK_J => KeyAction::new(VK_LEFT, MOD_SHIFT, 1),
        VK_K => KeyAction::new(VK_DOWN, MOD_SHIFT, 1),
        VK_L => KeyAction::new(VK_RIGHT, MOD_SHIFT, 1),
        VK_H => KeyAction::new(VK_LEFT, MOD_CTRL | MOD_SHIFT, 1),
        VK_OEM_PERIOD => KeyAction::new(VK_RIGHT, MOD_CTRL | MOD_SHIFT, 1),
        VK_U => KeyAction::new(VK_HOME, MOD_SHIFT, 1),
        VK_O => KeyAction::new(VK_END, MOD_SHIFT, 1),
        VK_P => KeyAction::new(VK_HOME, 0, 1),
        VK_OEM_1 => KeyAction::new(VK_END, 0, 1),
        VK_Y => KeyAction::new(VK_UP, MOD_SHIFT, 5),
        VK_N => KeyAction::new(VK_DOWN, MOD_SHIFT, 5),
        VK_Q => KeyAction::new(VK_Q, MOD_ALT | MOD_SHIFT, 1),
        VK_OEM_2 => KeyAction::new(VK_OEM_2, MOD_ALT | MOD_SHIFT, 1),
        VK_SPACE => KeyAction::new(VK_OEM_6, MOD_ALT | MOD_SHIFT, 1),
        _ => return None,
    };
    Some(action)
}

fn click_index(vk: u32) -> Option<usize> {
    match vk {
        VK_F13 => Some(0),
        VK_F14 => Some(1),
        VK_F15 => Some(2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_table_contains_every_reference_hotkey() {
        let expected = [
            (VK_E, KeyAction::new(VK_UP, 0, 1)),
            (VK_S, KeyAction::new(VK_LEFT, 0, 1)),
            (VK_D, KeyAction::new(VK_DOWN, 0, 1)),
            (VK_F, KeyAction::new(VK_RIGHT, 0, 1)),
            (VK_A, KeyAction::new(VK_LEFT, MOD_CTRL, 1)),
            (VK_G, KeyAction::new(VK_RIGHT, MOD_CTRL, 1)),
            (VK_W, KeyAction::new(VK_BACK, 0, 1)),
            (VK_R, KeyAction::new(VK_DELETE, 0, 1)),
            (VK_T, KeyAction::new(VK_UP, 0, 5)),
            (VK_B, KeyAction::new(VK_DOWN, 0, 5)),
            (VK_I, KeyAction::new(VK_UP, MOD_SHIFT, 1)),
            (VK_J, KeyAction::new(VK_LEFT, MOD_SHIFT, 1)),
            (VK_K, KeyAction::new(VK_DOWN, MOD_SHIFT, 1)),
            (VK_L, KeyAction::new(VK_RIGHT, MOD_SHIFT, 1)),
            (VK_H, KeyAction::new(VK_LEFT, MOD_CTRL | MOD_SHIFT, 1)),
            (
                VK_OEM_PERIOD,
                KeyAction::new(VK_RIGHT, MOD_CTRL | MOD_SHIFT, 1),
            ),
            (VK_U, KeyAction::new(VK_HOME, MOD_SHIFT, 1)),
            (VK_O, KeyAction::new(VK_END, MOD_SHIFT, 1)),
            (VK_P, KeyAction::new(VK_HOME, 0, 1)),
            (VK_OEM_1, KeyAction::new(VK_END, 0, 1)),
            (VK_Y, KeyAction::new(VK_UP, MOD_SHIFT, 5)),
            (VK_N, KeyAction::new(VK_DOWN, MOD_SHIFT, 5)),
            (VK_Q, KeyAction::new(VK_Q, MOD_ALT | MOD_SHIFT, 1)),
            (VK_OEM_2, KeyAction::new(VK_OEM_2, MOD_ALT | MOD_SHIFT, 1)),
            (VK_SPACE, KeyAction::new(VK_OEM_6, MOD_ALT | MOD_SHIFT, 1)),
        ];
        assert_eq!(expected.len(), 25);
        for (source, action) in expected {
            assert_eq!(mapping(source), Some(action));
        }
    }

    #[test]
    fn short_caps_tap_sends_escape() {
        let mut engine = Engine::default();
        assert!(engine.process(VK_CAPITAL, true, 1000, false).suppress);
        assert_eq!(
            engine.process(VK_CAPITAL, false, 1299, false).effect,
            Some(Effect::Key(KeyAction::new(VK_ESCAPE, 0, 1)))
        );
    }

    #[test]
    fn three_hundred_ms_is_a_hold() {
        let mut engine = Engine::default();
        engine.process(VK_CAPITAL, true, 1000, false);
        assert_eq!(engine.process(VK_CAPITAL, false, 1300, false).effect, None);
    }

    #[test]
    fn mapped_key_cancels_escape_and_pairs_suppression() {
        let mut engine = Engine::default();
        engine.process(VK_CAPITAL, true, 100, false);
        let down = engine.process(VK_E, true, 110, false);
        assert_eq!(down.effect, mapping(VK_E).map(Effect::Key));
        assert!(engine.process(VK_E, false, 120, false).suppress);
        assert_eq!(engine.process(VK_CAPITAL, false, 130, false).effect, None);
    }

    #[test]
    fn key_repeat_emits_repeated_actions() {
        let mut engine = Engine::default();
        engine.process(VK_CAPITAL, true, 100, false);
        assert!(engine.process(VK_T, true, 110, false).effect.is_some());
        assert!(engine.process(VK_T, true, 140, false).effect.is_some());
    }

    #[test]
    fn unmapped_key_passes_and_does_not_cancel_escape() {
        let mut engine = Engine::default();
        engine.process(VK_CAPITAL, true, 100, false);
        assert_eq!(engine.process(0x5A, true, 110, false), Outcome::default());
        assert!(
            engine
                .process(VK_CAPITAL, false, 120, false)
                .effect
                .is_some()
        );
    }

    #[test]
    fn modifier_prevents_plain_hotkey() {
        let mut engine = Engine::default();
        engine.process(VK_CAPITAL, true, 100, false);
        assert_eq!(engine.process(VK_E, true, 110, true), Outcome::default());
    }

    #[test]
    fn clickers_start_once_and_stop_on_release() {
        let mut engine = Engine::default();
        assert_eq!(
            engine.process(VK_F13, true, 1, false).effect,
            Some(Effect::Click {
                index: 0,
                active: true
            })
        );
        assert_eq!(engine.process(VK_F13, true, 2, false).effect, None);
        assert_eq!(
            engine.process(VK_F13, false, 3, false).effect,
            Some(Effect::Click {
                index: 0,
                active: false
            })
        );
    }

    #[test]
    fn timestamp_wraparound_is_handled() {
        let mut engine = Engine::default();
        engine.process(VK_CAPITAL, true, u32::MAX - 10, false);
        assert!(engine.process(VK_CAPITAL, false, 5, false).effect.is_some());
    }
}
