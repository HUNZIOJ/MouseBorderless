use anyhow::{anyhow, Context};
use borderless_core::input_event::{
    InputEvent, KeyEvent, MouseButton, MouseButtonEvent, MouseMoveAbsEvent, MouseWheelEvent,
};
use crossbeam_channel::{Receiver, Sender};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK,
            KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL,
            WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
            WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
            WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
        },
    },
};

const HOOK_EVENT_QUEUE_CAPACITY: usize = 1024;

#[derive(Clone, Debug)]
pub enum HookEvent {
    PointerPosition { x: i32, y: i32 },
    Input(InputEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressionMode {
    PassThrough,
    Suppress,
}

pub struct HookManager {
    mode: Arc<AtomicBool>,
    stop: Option<HookThreadHandle>,
    forwarder: Option<JoinHandle<()>>,
}

impl HookManager {
    pub fn install(sender: Sender<HookEvent>) -> anyhow::Result<Self> {
        let mode = Arc::new(AtomicBool::new(false));
        let hook_mode = Arc::clone(&mode);
        let (hook_tx, hook_rx) = crossbeam_channel::bounded(HOOK_EVENT_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let forwarder = spawn_hook_event_forwarder(hook_rx, sender)?;

        let join = thread::Builder::new()
            .name("borderless-input-hooks".to_owned())
            .spawn(move || {
                if let Err(error) = run_hook_thread(hook_tx, hook_mode, ready_tx) {
                    tracing::error!(?error, "input hook thread exited");
                }
            })
            .context("spawn input hook thread")?;

        let ready = match ready_rx.recv() {
            Ok(ready) => ready,
            Err(error) => {
                let _ = join.join();
                let _ = forwarder.join();
                return Err(error)
                    .context("input hook thread exited before reporting hook installation");
            }
        };
        let thread_id = match ready {
            Ok(thread_id) => thread_id,
            Err(error) => {
                let _ = join.join();
                let _ = forwarder.join();
                return Err(error);
            }
        };

        Ok(Self {
            mode,
            stop: Some(HookThreadHandle::new(thread_id, join)),
            forwarder: Some(forwarder),
        })
    }

    pub fn set_suppression_mode(&self, mode: SuppressionMode) {
        set_suppression_mode(&self.mode, mode);
    }
}

impl Drop for HookManager {
    fn drop(&mut self) {
        self.set_suppression_mode(SuppressionMode::PassThrough);
        if let Some(stop) = self.stop.take() {
            if let Err(error) = stop.stop_and_join() {
                tracing::warn!(?error, "failed to stop input hook thread");
            }
        }
        if let Some(forwarder) = self.forwarder.take() {
            if let Err(error) = join_hook_thread(forwarder) {
                tracing::warn!(?error, "failed to join input hook event forwarder");
            }
        }
    }
}

struct HookThreadHandle {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
    stop_requested: Arc<AtomicBool>,
    post_quit: fn(u32) -> anyhow::Result<()>,
}

impl HookThreadHandle {
    fn new(thread_id: u32, join: JoinHandle<()>) -> Self {
        Self {
            thread_id,
            join: Some(join),
            stop_requested: Arc::new(AtomicBool::new(false)),
            post_quit: post_thread_quit_message,
        }
    }

    #[cfg(test)]
    fn test(thread_id: u32, stop_requested: Arc<AtomicBool>) -> Self {
        Self {
            thread_id,
            join: None,
            stop_requested,
            post_quit: |_| Ok(()),
        }
    }

    fn request_stop(&self) -> anyhow::Result<bool> {
        request_thread_stop_once(&self.stop_requested, self.thread_id, self.post_quit)
    }

    fn stop_and_join(mut self) -> anyhow::Result<()> {
        self.request_stop()?;
        self.join.take().map(join_hook_thread).unwrap_or(Ok(()))
    }
}

struct HookThreadState {
    sender: Sender<HookEvent>,
    mode: Arc<AtomicBool>,
}

impl HookThreadState {
    fn new(sender: Sender<HookEvent>, mode: Arc<AtomicBool>) -> Self {
        Self { sender, mode }
    }

    fn emit_mouse_events(&mut self, message: u32, data: &MSLLHOOKSTRUCT) {
        let events = mouse_hook_events(message, data);
        if events.is_empty() {
            return;
        }

        self.emit_events(events);
    }

    fn emit_keyboard_event(&mut self, message: u32, data: &KBDLLHOOKSTRUCT) {
        if let Some(event) = keyboard_hook_event(message, data) {
            self.emit_event(event);
        }
    }

    fn emit_events(&self, events: Vec<HookEvent>) {
        for event in events {
            self.emit_event(event);
        }
    }

    fn emit_event(&self, event: HookEvent) {
        let _ = self.sender.try_send(event);
    }
}

fn spawn_hook_event_forwarder(
    receiver: Receiver<HookEvent>,
    sender: Sender<HookEvent>,
) -> anyhow::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("borderless-hook-events".to_owned())
        .spawn(move || forward_hook_events(receiver, sender))
        .context("spawn input hook event forwarder")
}

fn forward_hook_events(receiver: Receiver<HookEvent>, sender: Sender<HookEvent>) {
    for event in receiver {
        match sender.try_send(event) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
        }
    }
}

thread_local! {
    static HOOK_THREAD_STATE: RefCell<Option<HookThreadState>> = const { RefCell::new(None) };
}

struct HookThreadStateGuard;

impl Drop for HookThreadStateGuard {
    fn drop(&mut self) {
        HOOK_THREAD_STATE.with(|state| {
            *state.borrow_mut() = None;
        });
    }
}

struct InstalledHooks {
    mouse: HHOOK,
    keyboard: HHOOK,
}

impl Drop for InstalledHooks {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.mouse);
            let _ = UnhookWindowsHookEx(self.keyboard);
        }
    }
}

fn run_hook_thread(
    sender: Sender<HookEvent>,
    mode: Arc<AtomicBool>,
    ready_tx: Sender<anyhow::Result<u32>>,
) -> anyhow::Result<()> {
    let thread_id = unsafe { GetCurrentThreadId() };
    ensure_message_queue();

    HOOK_THREAD_STATE.with(|state| {
        *state.borrow_mut() = Some(HookThreadState::new(sender, mode));
    });
    let _state_guard = HookThreadStateGuard;

    let hooks = match install_low_level_hooks() {
        Ok(hooks) => {
            let _ = ready_tx.send(Ok(thread_id));
            hooks
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return Ok(());
        }
    };

    let result = message_loop();
    drop(hooks);
    result
}

fn request_thread_stop_once<F>(
    requested: &AtomicBool,
    thread_id: u32,
    post_quit: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(u32) -> anyhow::Result<()>,
{
    if requested.swap(true, Ordering::SeqCst) {
        return Ok(false);
    }

    if let Err(error) = post_quit(thread_id) {
        requested.store(false, Ordering::SeqCst);
        return Err(error);
    }

    Ok(true)
}

fn post_thread_quit_message(thread_id: u32) -> anyhow::Result<()> {
    unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
        .context("post hook thread quit message")
}

fn join_hook_thread(join: JoinHandle<()>) -> anyhow::Result<()> {
    if join.thread().id() == thread::current().id() {
        return Ok(());
    }

    join.join()
        .map_err(|_| anyhow!("input hook thread panicked"))
}

fn ensure_message_queue() {
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, HWND::default(), 0, 0, PM_NOREMOVE);
    }
}

fn install_low_level_hooks() -> anyhow::Result<InstalledHooks> {
    let module: HINSTANCE = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .context("get current module handle for input hooks")?
        .into();
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), module, 0) }
        .context("install low-level mouse hook")?;
    let keyboard =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), module, 0) } {
            Ok(keyboard) => keyboard,
            Err(error) => {
                unsafe {
                    let _ = UnhookWindowsHookEx(mouse);
                }
                return Err(error).context("install low-level keyboard hook");
            }
        };

    Ok(InstalledHooks { mouse, keyboard })
}

fn message_loop() -> anyhow::Result<()> {
    let mut message = MSG::default();

    loop {
        let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        match result.0 {
            -1 => return Err(windows::core::Error::from_win32()).context("get hook message"),
            0 => return Ok(()),
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

unsafe extern "system" fn mouse_hook_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode == HC_ACTION as i32 {
        let data = unsafe { (lparam.0 as *const MSLLHOOKSTRUCT).as_ref() };
        if let Some(data) = data {
            HOOK_THREAD_STATE.with(|state| {
                if let Some(state) = state.borrow_mut().as_mut() {
                    state.emit_mouse_events(wparam.0 as u32, data);
                }
            });
        }

        if hook_should_suppress() {
            return LRESULT(1);
        }
    }

    unsafe { CallNextHookEx(HHOOK::default(), ncode, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook_proc(
    ncode: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if ncode == HC_ACTION as i32 {
        let data = unsafe { (lparam.0 as *const KBDLLHOOKSTRUCT).as_ref() };
        if let Some(data) = data {
            HOOK_THREAD_STATE.with(|state| {
                if let Some(state) = state.borrow_mut().as_mut() {
                    state.emit_keyboard_event(wparam.0 as u32, data);
                }
            });
        }

        if hook_should_suppress() {
            return LRESULT(1);
        }
    }

    unsafe { CallNextHookEx(HHOOK::default(), ncode, wparam, lparam) }
}

fn hook_should_suppress() -> bool {
    HOOK_THREAD_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .is_some_and(|state| state.mode.load(Ordering::SeqCst))
    })
}

fn set_suppression_mode(mode: &AtomicBool, suppression_mode: SuppressionMode) {
    mode.store(
        matches!(suppression_mode, SuppressionMode::Suppress),
        Ordering::SeqCst,
    );
}

fn mouse_hook_events(message: u32, data: &MSLLHOOKSTRUCT) -> Vec<HookEvent> {
    match message {
        WM_MOUSEMOVE => vec![
            HookEvent::PointerPosition {
                x: data.pt.x,
                y: data.pt.y,
            },
            HookEvent::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent {
                x: data.pt.x,
                y: data.pt.y,
            })),
        ],
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => mouse_button_event(message, data)
            .map(|event| vec![HookEvent::Input(event)])
            .unwrap_or_default(),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            vec![HookEvent::Input(InputEvent::MouseWheel(MouseWheelEvent {
                delta: signed_high_word(data.mouseData),
                horizontal: message == WM_MOUSEHWHEEL,
            }))]
        }
        _ => Vec::new(),
    }
}

fn mouse_button_event(message: u32, data: &MSLLHOOKSTRUCT) -> Option<InputEvent> {
    let (button, pressed) = match message {
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        WM_XBUTTONDOWN => (x_mouse_button(data.mouseData)?, true),
        WM_XBUTTONUP => (x_mouse_button(data.mouseData)?, false),
        _ => return None,
    };

    Some(InputEvent::MouseButton(MouseButtonEvent {
        button,
        pressed,
    }))
}

fn x_mouse_button(mouse_data: u32) -> Option<MouseButton> {
    match high_word(mouse_data) {
        XBUTTON1 => Some(MouseButton::X1),
        XBUTTON2 => Some(MouseButton::X2),
        _ => None,
    }
}

fn keyboard_hook_event(message: u32, data: &KBDLLHOOKSTRUCT) -> Option<HookEvent> {
    let pressed = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return None,
    };

    Some(HookEvent::Input(InputEvent::Key(KeyEvent {
        vk_code: data.vkCode as u16,
        pressed,
    })))
}

fn signed_high_word(value: u32) -> i32 {
    high_word(value) as i16 as i32
}

fn high_word(value: u32) -> u16 {
    (value >> 16) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use borderless_core::input_event::{InputEvent, MouseButton};
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    use windows::Win32::{
        Foundation::POINT,
        UI::WindowsAndMessaging::{
            KBDLLHOOKSTRUCT, KBDLLHOOKSTRUCT_FLAGS, MSLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP,
            WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
            WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
            WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
        },
    };

    #[test]
    fn mouse_move_produces_pointer_position_and_absolute_input() {
        let events = mouse_hook_events(WM_MOUSEMOVE, &mouse_data_at(321, -45, 0));

        assert_eq!(events.len(), 2);
        assert!(
            matches!(events[0], HookEvent::PointerPosition { x: 321, y: -45 }),
            "first event should report pointer position, got {:?}",
            events[0]
        );
        assert!(
            matches!(
                events[1],
                HookEvent::Input(InputEvent::MouseMoveAbs(event))
                    if event.x == 321 && event.y == -45
            ),
            "second event should report absolute mouse movement, got {:?}",
            events[1]
        );
    }

    #[test]
    fn mouse_buttons_map_down_and_up_messages() {
        assert_mouse_button(WM_LBUTTONDOWN, 0, MouseButton::Left, true);
        assert_mouse_button(WM_LBUTTONUP, 0, MouseButton::Left, false);
        assert_mouse_button(WM_RBUTTONDOWN, 0, MouseButton::Right, true);
        assert_mouse_button(WM_RBUTTONUP, 0, MouseButton::Right, false);
        assert_mouse_button(WM_MBUTTONDOWN, 0, MouseButton::Middle, true);
        assert_mouse_button(WM_MBUTTONUP, 0, MouseButton::Middle, false);
        assert_mouse_button(
            WM_XBUTTONDOWN,
            u32::from(XBUTTON1) << 16,
            MouseButton::X1,
            true,
        );
        assert_mouse_button(
            WM_XBUTTONUP,
            u32::from(XBUTTON2) << 16,
            MouseButton::X2,
            false,
        );
    }

    #[test]
    fn mouse_wheel_messages_use_signed_high_word_delta() {
        let vertical = only_input(mouse_hook_events(
            WM_MOUSEWHEEL,
            &mouse_data_at(0, 0, (-120i16 as u16 as u32) << 16),
        ));
        let horizontal = only_input(mouse_hook_events(
            WM_MOUSEHWHEEL,
            &mouse_data_at(0, 0, (240u32) << 16),
        ));

        assert!(
            matches!(
                vertical,
                InputEvent::MouseWheel(event) if event.delta == -120 && !event.horizontal
            ),
            "vertical wheel should preserve signed delta, got {:?}",
            vertical
        );
        assert!(
            matches!(
                horizontal,
                InputEvent::MouseWheel(event) if event.delta == 240 && event.horizontal
            ),
            "horizontal wheel should preserve signed delta, got {:?}",
            horizontal
        );
    }

    #[test]
    fn keyboard_messages_map_to_key_input() {
        assert_key(WM_KEYDOWN, 0x41, true);
        assert_key(WM_SYSKEYDOWN, 0x12, true);
        assert_key(WM_KEYUP, 0x41, false);
        assert_key(WM_SYSKEYUP, 0x12, false);
    }

    #[test]
    fn suppression_mode_setter_toggles_shared_atomic_bool() {
        let mode = Arc::new(AtomicBool::new(false));
        let manager = HookManager {
            mode: Arc::clone(&mode),
            stop: None,
            forwarder: None,
        };

        manager.set_suppression_mode(SuppressionMode::Suppress);
        assert!(mode.load(Ordering::SeqCst));

        manager.set_suppression_mode(SuppressionMode::PassThrough);
        assert!(!mode.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_hook_manager_restores_pass_through_mode() {
        let mode = Arc::new(AtomicBool::new(false));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let manager = HookManager {
            mode: Arc::clone(&mode),
            stop: Some(HookThreadHandle::test(123, Arc::clone(&stop_requested))),
            forwarder: None,
        };

        manager.set_suppression_mode(SuppressionMode::Suppress);
        drop(manager);

        assert!(!mode.load(Ordering::SeqCst));
        assert!(stop_requested.load(Ordering::SeqCst));
    }

    #[test]
    fn stop_request_posts_quit_only_once() {
        let requested = AtomicBool::new(false);
        let post_count = AtomicUsize::new(0);

        let first = request_thread_stop_once(&requested, 42, |thread_id| {
            assert_eq!(thread_id, 42);
            post_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect("first stop request should post quit");
        let second = request_thread_stop_once(&requested, 42, |_| {
            post_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect("second stop request should be a no-op");

        assert!(first);
        assert!(!second);
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn callback_state_preserves_continuous_pointer_moves() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = HookThreadState::new(sender, Arc::new(AtomicBool::new(false)));

        state.emit_mouse_events(WM_MOUSEMOVE, &mouse_data_at(1, 2, 0));
        state.emit_mouse_events(WM_MOUSEMOVE, &mouse_data_at(3, 4, 0));

        let batch: Vec<_> = receiver.try_iter().collect();
        assert_eq!(batch.len(), 4);
        assert!(
            matches!(batch[0], HookEvent::PointerPosition { x: 1, y: 2 }),
            "first pointer position should be emitted, got {:?}",
            batch[0]
        );
        assert!(
            matches!(
                batch[1],
                HookEvent::Input(InputEvent::MouseMoveAbs(event)) if event.x == 1 && event.y == 2
            ),
            "first absolute mouse move should be emitted, got {:?}",
            batch[1]
        );
        assert!(
            matches!(batch[2], HookEvent::PointerPosition { x: 3, y: 4 }),
            "second pointer position should be emitted, got {:?}",
            batch[2]
        );
        assert!(
            matches!(
                batch[3],
                HookEvent::Input(InputEvent::MouseMoveAbs(event)) if event.x == 3 && event.y == 4
            ),
            "second absolute mouse move should be emitted, got {:?}",
            batch[3]
        );
    }

    #[test]
    fn callback_state_drops_events_when_bounded_queue_is_full() {
        let (sender, receiver) = crossbeam_channel::bounded(2);
        let mut state = HookThreadState::new(sender, Arc::new(AtomicBool::new(false)));

        state.emit_mouse_events(WM_MOUSEMOVE, &mouse_data_at(1, 2, 0));
        state.emit_mouse_events(WM_MOUSEMOVE, &mouse_data_at(3, 4, 0));

        let batch: Vec<_> = receiver.try_iter().collect();
        assert_eq!(batch.len(), 2);
        assert!(matches!(
            batch[0],
            HookEvent::PointerPosition { x: 1, y: 2 }
        ));
        assert!(matches!(
            batch[1],
            HookEvent::Input(InputEvent::MouseMoveAbs(event)) if event.x == 1 && event.y == 2
        ));
    }

    fn assert_mouse_button(message: u32, mouse_data: u32, button: MouseButton, pressed: bool) {
        let event = only_input(mouse_hook_events(message, &mouse_data_at(0, 0, mouse_data)));

        assert!(
            matches!(
                event,
                InputEvent::MouseButton(button_event)
                    if button_event.button == button && button_event.pressed == pressed
            ),
            "mouse message {message:#x} should map to {button:?}/{pressed}, got {event:?}"
        );
    }

    fn assert_key(message: u32, vk_code: u32, pressed: bool) {
        let event = keyboard_hook_event(
            message,
            &KBDLLHOOKSTRUCT {
                vkCode: vk_code,
                scanCode: 0,
                flags: KBDLLHOOKSTRUCT_FLAGS(0),
                time: 0,
                dwExtraInfo: 0,
            },
        )
        .expect("keyboard message should map to an event");

        assert!(
            matches!(
                event,
                HookEvent::Input(InputEvent::Key(key)) if key.vk_code == vk_code as u16
                    && key.pressed == pressed
            ),
            "keyboard message {message:#x} should map to key state {pressed}, got {event:?}"
        );
    }

    fn only_input(events: Vec<HookEvent>) -> InputEvent {
        assert_eq!(events.len(), 1);
        match events.into_iter().next().unwrap() {
            HookEvent::Input(event) => event,
            event => panic!("expected input event, got {event:?}"),
        }
    }

    fn mouse_data_at(x: i32, y: i32, mouse_data: u32) -> MSLLHOOKSTRUCT {
        MSLLHOOKSTRUCT {
            pt: POINT { x, y },
            mouseData: mouse_data,
            flags: 0,
            time: 0,
            dwExtraInfo: 0,
        }
    }
}
