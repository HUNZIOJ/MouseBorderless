use anyhow::{anyhow, Context};
use borderless_core::{
    geometry::{Point, Rect},
    input_event::{
        InputEvent, KeyEvent, MouseButton, MouseButtonEvent, MouseMoveAbsEvent, MouseWheelEvent,
        PressedState,
    },
};
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
        VIRTUAL_KEY,
    },
    WindowsAndMessaging::{XBUTTON1, XBUTTON2},
};

pub const BORDERLESS_INPUT_MARKER: usize = 0x4244_524C;

pub struct InputInjector {
    desktop: Rect,
    pressed: PressedState,
}

impl InputInjector {
    pub fn new(desktop: Rect) -> Self {
        Self {
            desktop,
            pressed: PressedState::default(),
        }
    }

    pub fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        let mut next_pressed = self.pressed.clone();
        let inputs = build_inputs(event, &mut next_pressed, self.desktop);
        send_inputs(&inputs).with_context(|| format!("send input event: {event:?}"))?;
        self.pressed = next_pressed;
        Ok(())
    }

    pub fn release_all(&mut self) -> anyhow::Result<()> {
        let mut next_pressed = self.pressed.clone();
        let inputs = build_inputs(&InputEvent::ReleaseAll, &mut next_pressed, self.desktop);
        send_inputs(&inputs).context("release all pressed input")?;
        self.pressed = next_pressed;
        Ok(())
    }
}

pub fn manual_move_check(desktop: Rect) -> anyhow::Result<()> {
    let center = Point::new(
        desktop.left.saturating_add(desktop.width / 2),
        desktop.top.saturating_add(desktop.height / 2),
    );
    let mut injector = InputInjector::new(desktop);
    injector.inject(&InputEvent::MouseMoveAbs(MouseMoveAbsEvent {
        x: center.x,
        y: center.y,
    }))
}

pub fn move_local_pointer_to(desktop: Rect, point: Point) -> anyhow::Result<()> {
    let inputs = local_pointer_move_inputs(desktop, point);
    send_inputs(&inputs).with_context(|| format!("move local pointer to {point:?}"))
}

/// Release the physical left button locally so an in-progress native OLE
/// drag ends (dropping on whatever window is under the parked pointer).
/// Used when the drag release is detected via the hook while local input
/// is suppressed for remote control.
pub fn release_local_left_button() -> anyhow::Result<()> {
    let inputs = [local_left_button_release_input()];
    send_inputs(&inputs).context("release local left mouse button")
}

fn local_left_button_release_input() -> INPUT {
    tag_borderless_mouse_input(mouse_button_input(&MouseButtonEvent {
        button: MouseButton::Left,
        pressed: false,
    }))
}

fn local_pointer_move_inputs(desktop: Rect, point: Point) -> Vec<INPUT> {
    let mut pressed = PressedState::default();
    build_inputs(
        &InputEvent::MouseMoveAbs(MouseMoveAbsEvent {
            x: point.x,
            y: point.y,
        }),
        &mut pressed,
        desktop,
    )
    .into_iter()
    .map(tag_borderless_mouse_input)
    .collect()
}

fn tag_borderless_mouse_input(mut input: INPUT) -> INPUT {
    input.Anonymous.mi.dwExtraInfo = BORDERLESS_INPUT_MARKER;
    input
}

fn build_inputs(event: &InputEvent, pressed: &mut PressedState, desktop: Rect) -> Vec<INPUT> {
    match event {
        InputEvent::Key(event) => {
            pressed.apply(&InputEvent::Key(*event));
            vec![key_input(event)]
        }
        InputEvent::MouseButton(event) => {
            pressed.apply(&InputEvent::MouseButton(*event));
            vec![mouse_button_input(event)]
        }
        InputEvent::MouseMoveAbs(event) => {
            let (x, y) = normalize(Point::new(event.x, event.y), desktop);
            vec![mouse_input(
                x,
                y,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )]
        }
        InputEvent::MouseWheel(event) => vec![mouse_wheel_input(event)],
        InputEvent::MouseMoveDelta(_) => Vec::new(),
        InputEvent::ReleaseAll => {
            let mut inputs = Vec::with_capacity(pressed.keys.len() + pressed.mouse_buttons.len());
            inputs.extend(pressed.keys.iter().copied().map(|vk_code| {
                key_input(&KeyEvent {
                    vk_code,
                    pressed: false,
                })
            }));
            inputs.extend(pressed.mouse_buttons.iter().copied().map(|button| {
                mouse_button_input(&MouseButtonEvent {
                    button,
                    pressed: false,
                })
            }));
            pressed.clear();
            inputs
        }
    }
}

fn send_inputs(inputs: &[INPUT]) -> anyhow::Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }

    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(send_input_mismatch_error(sent, inputs.len()))
    }
}

fn send_input_mismatch_error(sent: u32, expected: usize) -> anyhow::Error {
    anyhow!(
        "SendInput sent {sent} of {expected} input events; last Windows error: {}",
        windows::core::Error::from_win32()
    )
}

fn normalize(point: Point, desktop: Rect) -> (i32, i32) {
    (
        normalize_axis(point.x, desktop.left, desktop.width),
        normalize_axis(point.y, desktop.top, desktop.height),
    )
}

fn normalize_axis(value: i32, start: i32, len: i32) -> i32 {
    if len <= 1 {
        return 0;
    }

    let max_offset = i64::from(len - 1);
    let offset = (i64::from(value) - i64::from(start)).clamp(0, max_offset);
    ((offset * 65_535) / max_offset) as i32
}

fn key_input(event: &KeyEvent) -> INPUT {
    let mut flags = if event.pressed {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    if is_extended_vk(event.vk_code) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(event.vk_code),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn is_extended_vk(vk_code: u16) -> bool {
    matches!(
        vk_code,
        0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0x6F | 0x90 | 0xA3 | 0xA5
    )
}

fn mouse_button_input(event: &MouseButtonEvent) -> INPUT {
    let (flags, mouse_data) = match (event.button, event.pressed) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
        (MouseButton::X1, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
        (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
        (MouseButton::X2, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
    };

    mouse_input(0, 0, mouse_data, flags)
}

fn mouse_wheel_input(event: &MouseWheelEvent) -> INPUT {
    let flags = if event.horizontal {
        MOUSEEVENTF_HWHEEL
    } else {
        MOUSEEVENTF_WHEEL
    };

    mouse_input(0, 0, event.delta as u32, flags)
}

fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use borderless_core::input_event::{
        InputEvent, KeyEvent, MouseButton, MouseButtonEvent, MouseMoveAbsEvent, MouseMoveDeltaEvent,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_VIRTUALDESK,
    };

    #[test]
    fn normalize_maps_desktop_endpoints_to_windows_absolute_range() {
        let desktop = Rect::new(10, 20, 101, 201);

        assert_eq!(normalize(Point::new(10, 20), desktop), (0, 0));
        assert_eq!(normalize(Point::new(110, 220), desktop), (65_535, 65_535));
    }

    #[test]
    fn normalize_clamps_points_outside_desktop() {
        let desktop = Rect::new(10, 20, 101, 201);

        assert_eq!(normalize(Point::new(-500, 10_000), desktop), (0, 65_535));
    }

    #[test]
    fn build_inputs_tracks_pressed_state_and_mouse_delta_is_noop() {
        let desktop = Rect::new(0, 0, 1920, 1080);
        let mut pressed = PressedState::default();

        let key_down = build_inputs(
            &InputEvent::Key(KeyEvent {
                vk_code: 0x41,
                pressed: true,
            }),
            &mut pressed,
            desktop,
        );
        let button_down = build_inputs(
            &InputEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Left,
                pressed: true,
            }),
            &mut pressed,
            desktop,
        );
        let delta = build_inputs(
            &InputEvent::MouseMoveDelta(MouseMoveDeltaEvent { dx: 10, dy: -5 }),
            &mut pressed,
            desktop,
        );

        assert_eq!(key_down.len(), 1);
        assert_eq!(button_down.len(), 1);
        assert!(delta.is_empty());
        assert!(pressed.keys.contains(&0x41));
        assert!(pressed.mouse_buttons.contains(&MouseButton::Left));
    }

    #[test]
    fn absolute_mouse_move_targets_virtual_desktop() {
        let desktop = Rect::new(-1920, 0, 3840, 1080);
        let mut pressed = PressedState::default();

        let inputs = build_inputs(
            &InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x: -10, y: 10 }),
            &mut pressed,
            desktop,
        );

        assert_eq!(inputs.len(), 1);
        let flags = unsafe { inputs[0].Anonymous.mi.dwFlags };
        assert_ne!(flags.0 & MOUSEEVENTF_VIRTUALDESK.0, 0);
    }

    #[test]
    fn local_pointer_move_inputs_use_absolute_virtual_desktop_coordinates() {
        let desktop = Rect::new(-1920, 0, 3840, 1080);
        let inputs = local_pointer_move_inputs(desktop, Point::new(-10, 10));

        assert_eq!(inputs.len(), 1);
        let mouse = unsafe { inputs[0].Anonymous.mi };
        assert_eq!(
            mouse.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
        );
        assert_eq!(mouse.dx, normalize_axis(-10, desktop.left, desktop.width));
        assert_eq!(mouse.dy, normalize_axis(10, desktop.top, desktop.height));
        assert_eq!(mouse.dwExtraInfo, BORDERLESS_INPUT_MARKER);
    }

    #[test]
    fn local_left_button_release_is_tagged() {
        let input = local_left_button_release_input();
        let mouse = unsafe { input.Anonymous.mi };

        assert_eq!(mouse.dwFlags, MOUSEEVENTF_LEFTUP);
        assert_eq!(mouse.dwExtraInfo, BORDERLESS_INPUT_MARKER);
    }

    #[test]
    fn keyboard_input_marks_extended_keys() {
        let arrow_down = key_input(&KeyEvent {
            vk_code: 0x25,
            pressed: true,
        });
        let arrow_up = key_input(&KeyEvent {
            vk_code: 0x25,
            pressed: false,
        });
        let regular_key = key_input(&KeyEvent {
            vk_code: 0x41,
            pressed: true,
        });

        let arrow_down_flags = unsafe { arrow_down.Anonymous.ki.dwFlags };
        let arrow_up_flags = unsafe { arrow_up.Anonymous.ki.dwFlags };
        let regular_flags = unsafe { regular_key.Anonymous.ki.dwFlags };

        assert_ne!(
            arrow_down_flags.0 & KEYEVENTF_EXTENDEDKEY.0,
            0,
            "left arrow key-down must be marked as extended"
        );
        assert_ne!(
            arrow_up_flags.0 & KEYEVENTF_EXTENDEDKEY.0,
            0,
            "left arrow key-up must be marked as extended"
        );
        assert_eq!(regular_flags.0 & KEYEVENTF_EXTENDEDKEY.0, 0);
    }

    #[test]
    fn mouse_buttons_include_x_button_data() {
        let x1_down = mouse_button_input(&MouseButtonEvent {
            button: MouseButton::X1,
            pressed: true,
        });
        let x2_up = mouse_button_input(&MouseButtonEvent {
            button: MouseButton::X2,
            pressed: false,
        });

        let x1 = unsafe { x1_down.Anonymous.mi };
        let x2 = unsafe { x2_up.Anonymous.mi };

        assert_eq!(x1.dwFlags, MOUSEEVENTF_XDOWN);
        assert_eq!(x1.mouseData, u32::from(XBUTTON1));
        assert_eq!(x2.dwFlags, MOUSEEVENTF_XUP);
        assert_eq!(x2.mouseData, u32::from(XBUTTON2));
    }

    #[test]
    fn mouse_wheel_inputs_preserve_axis_and_delta() {
        let vertical = mouse_wheel_input(&MouseWheelEvent {
            delta: -120,
            horizontal: false,
        });
        let horizontal = mouse_wheel_input(&MouseWheelEvent {
            delta: 240,
            horizontal: true,
        });

        let vertical_input = unsafe { vertical.Anonymous.mi };
        let horizontal_input = unsafe { horizontal.Anonymous.mi };

        assert_eq!(vertical_input.dwFlags, MOUSEEVENTF_WHEEL);
        assert_eq!(vertical_input.mouseData, (-120i32) as u32);
        assert_eq!(horizontal_input.dwFlags, MOUSEEVENTF_HWHEEL);
        assert_eq!(horizontal_input.mouseData, 240);
    }

    #[test]
    fn send_input_mismatch_error_includes_win32_context() {
        let message = send_input_mismatch_error(0, 1).to_string();

        assert!(message.contains("SendInput sent 0 of 1 input events"));
        assert!(message.contains("last Windows error"));
    }

    #[test]
    fn build_inputs_releases_all_pressed_state() {
        let desktop = Rect::new(0, 0, 1920, 1080);
        let mut pressed = PressedState::default();
        pressed.keys.insert(0x41);
        pressed.keys.insert(0x42);
        pressed.mouse_buttons.insert(MouseButton::Left);
        pressed.mouse_buttons.insert(MouseButton::X2);

        let releases = build_inputs(&InputEvent::ReleaseAll, &mut pressed, desktop);

        assert_eq!(releases.len(), 4);
        assert!(pressed.is_empty());
    }
}
