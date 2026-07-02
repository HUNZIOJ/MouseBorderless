use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub vk_code: u16,
    pub pressed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseButtonEvent {
    pub button: MouseButton,
    pub pressed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseMoveAbsEvent {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseMoveDeltaEvent {
    pub dx: i32,
    pub dy: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseWheelEvent {
    pub delta: i32,
    pub horizontal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    Key(KeyEvent),
    MouseButton(MouseButtonEvent),
    MouseMoveAbs(MouseMoveAbsEvent),
    MouseMoveDelta(MouseMoveDeltaEvent),
    MouseWheel(MouseWheelEvent),
    ReleaseAll,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PressedState {
    pub keys: BTreeSet<u16>,
    pub mouse_buttons: BTreeSet<MouseButton>,
}

impl PressedState {
    pub fn apply(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key(event) if event.pressed => {
                self.keys.insert(event.vk_code);
            }
            InputEvent::Key(event) => {
                self.keys.remove(&event.vk_code);
            }
            InputEvent::MouseButton(event) if event.pressed => {
                self.mouse_buttons.insert(event.button);
            }
            InputEvent::MouseButton(event) => {
                self.mouse_buttons.remove(&event.button);
            }
            InputEvent::ReleaseAll => self.clear(),
            InputEvent::MouseMoveAbs(_)
            | InputEvent::MouseMoveDelta(_)
            | InputEvent::MouseWheel(_) => {}
        }
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.mouse_buttons.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.mouse_buttons.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_state_tracks_keyboard_and_mouse_buttons() {
        let mut pressed = PressedState::default();
        pressed.apply(&InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: true }));
        pressed.apply(&InputEvent::MouseButton(MouseButtonEvent {
            button: MouseButton::Left,
            pressed: true,
        }));

        assert!(pressed.keys.contains(&0x41));
        assert!(pressed.mouse_buttons.contains(&MouseButton::Left));

        pressed.apply(&InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: false }));
        pressed.apply(&InputEvent::MouseButton(MouseButtonEvent {
            button: MouseButton::Left,
            pressed: false,
        }));

        assert!(!pressed.keys.contains(&0x41));
        assert!(!pressed.mouse_buttons.contains(&MouseButton::Left));
    }
}
