use anyhow::{anyhow, Context};
use borderless_core::geometry::{Edge, Rect};
use crossbeam_channel::Sender;
use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr,
    sync::Mutex,
    thread::{self, JoinHandle},
};
use uuid::Uuid;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINTL, WPARAM},
        System::{
            Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
            LibraryLoader::GetModuleHandleW,
            Ole::{
                IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
                ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
                DROPEFFECT_NONE,
            },
            SystemServices::MODIFIERKEYS_FLAGS,
            Threading::GetCurrentThreadId,
        },
        UI::{
            Shell::{DragQueryFileW, HDROP},
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassInfoW,
                GetMessageW, GetSystemMetrics, PeekMessageW, PostThreadMessageW, RegisterClassW,
                SetLayeredWindowAttributes, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
                LWA_ALPHA, MSG, PM_NOREMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
                SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNA, WINDOW_EX_STYLE, WM_QUIT,
                WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_POPUP,
            },
        },
    },
};
use windows_core::implement;

const EDGE_WINDOW_CLASS: &str = "BorderlessDragDropEdgeWindow";
const EDGE_WINDOW_THICKNESS_PX: i32 = 2;

/// Events emitted by the edge drop target. In the targeted drag-drop model
/// the native OLE drag never leaves the source machine: once the drag parks
/// over the edge window, the runtime drives remote target selection, and the
/// local `Drop`/`DragLeave` become the release/cancel signals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragDropEvent {
    LocalFileDragEntered {
        session_id: Uuid,
        transfer_id: Uuid,
        paths: Vec<String>,
    },
    LocalDragCancelled {
        session_id: Uuid,
    },
    LocalDropReleased {
        session_id: Uuid,
    },
    Error {
        session_id: Option<Uuid>,
        message: String,
    },
}

#[derive(Debug)]
pub struct EdgeDropTarget {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl EdgeDropTarget {
    pub fn install(edge: Edge, sender: Sender<DragDropEvent>) -> anyhow::Result<Self> {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let join = thread::Builder::new()
            .name("borderless-edge-drop-target".to_string())
            .spawn(move || {
                if let Err(error) = run_edge_drop_target_thread(edge, sender, ready_tx) {
                    tracing::error!(?error, "edge drop target thread exited");
                }
            })
            .context("spawn edge drop target thread")?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(error) => {
                let _ = join.join();
                Err(error).context("edge drop target exited before reporting readiness")
            }
        }
    }

    pub fn uninstall(mut self) -> anyhow::Result<()> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> anyhow::Result<()> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };

        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0))
                .context("post edge drop target quit message")?;
        }

        join.join()
            .map_err(|_| anyhow!("edge drop target thread panicked"))
    }
}

impl Drop for EdgeDropTarget {
    fn drop(&mut self) {
        if let Err(error) = self.stop_inner() {
            tracing::warn!(?error, "failed to stop edge drop target");
        }
    }
}

fn run_edge_drop_target_thread(
    edge: Edge,
    sender: Sender<DragDropEvent>,
    ready: Sender<anyhow::Result<u32>>,
) -> anyhow::Result<()> {
    let _ole = OleApartment::initialize()?;
    let thread_id = unsafe { GetCurrentThreadId() };
    ensure_message_queue();

    let hwnd = match create_edge_window(edge) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };

    let drop_target: IDropTarget = LocalDropTarget {
        sender,
        active_session: Mutex::new(None),
    }
    .into();
    if let Err(error) = unsafe { RegisterDragDrop(hwnd, &drop_target) } {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        let _ = ready.send(Err(error).context("register edge drop target"));
        return Ok(());
    }

    let _ = ready.send(Ok(thread_id));
    let loop_result = message_loop();

    unsafe {
        let _ = RevokeDragDrop(hwnd);
        let _ = DestroyWindow(hwnd);
    }
    drop(drop_target);
    loop_result
}

#[implement(IDropTarget)]
struct LocalDropTarget {
    sender: Sender<DragDropEvent>,
    active_session: Mutex<Option<Uuid>>,
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for LocalDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let Some(data_object) = pdataobj else {
            set_drop_effect(pdweffect, DROPEFFECT_NONE);
            return Ok(());
        };

        match extract_hdrop_paths(data_object) {
            Ok(paths) if !paths.is_empty() => {
                let session_id = Uuid::new_v4();
                if let Ok(mut active) = self.active_session.lock() {
                    *active = Some(session_id);
                }
                let _ = self.sender.send(DragDropEvent::LocalFileDragEntered {
                    session_id,
                    transfer_id: Uuid::new_v4(),
                    paths,
                });
                set_drop_effect(pdweffect, DROPEFFECT_COPY);
            }
            Ok(_) | Err(_) => {
                set_drop_effect(pdweffect, DROPEFFECT_NONE);
            }
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let has_active_session = self
            .active_session
            .lock()
            .ok()
            .and_then(|active| *active)
            .is_some();
        set_drop_effect(
            pdweffect,
            if has_active_session {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            },
        );
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if let Ok(mut active) = self.active_session.lock() {
            if let Some(session_id) = active.take() {
                let _ = self
                    .sender
                    .send(DragDropEvent::LocalDragCancelled { session_id });
            }
        }
        Ok(())
    }

    fn Drop(
        &self,
        _pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        if let Ok(mut active) = self.active_session.lock() {
            if let Some(session_id) = active.take() {
                let _ = self
                    .sender
                    .send(DragDropEvent::LocalDropReleased { session_id });
            }
        }
        set_drop_effect(pdweffect, DROPEFFECT_COPY);
        Ok(())
    }
}

struct OleApartment;

impl OleApartment {
    fn initialize() -> anyhow::Result<Self> {
        unsafe {
            OleInitialize(None).context("initialize OLE apartment")?;
        }
        Ok(Self)
    }
}

impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe {
            OleUninitialize();
        }
    }
}

fn create_edge_window(edge: Edge) -> anyhow::Result<HWND> {
    let desktop = virtual_desktop_rect();
    let rect = edge_window_rect(edge, desktop, EDGE_WINDOW_THICKNESS_PX);
    let hwnd = create_drag_window(rect)?;
    unsafe {
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA)
            .context("make edge drop target transparent")?;
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }
    Ok(hwnd)
}

fn create_drag_window(rect: Rect) -> anyhow::Result<HWND> {
    let class_name = wide_null(EDGE_WINDOW_CLASS);
    let module: HINSTANCE = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .context("get current module handle")?
        .into();
    let window_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(drag_window_proc),
        hInstance: module,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };

    let atom = unsafe { RegisterClassW(&window_class) };
    if atom == 0 {
        let mut existing = WNDCLASSW::default();
        unsafe {
            GetClassInfoW(module, PCWSTR(class_name.as_ptr()), &mut existing)
                .context("register drag/drop window class")?;
        }
    }

    let ex_style: WINDOW_EX_STYLE =
        WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
    unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WS_POPUP,
            rect.left,
            rect.top,
            rect.width,
            rect.height,
            None,
            None,
            module,
            None,
        )
    }
    .context("create drag/drop window")
}

unsafe extern "system" fn drag_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn message_loop() -> anyhow::Result<()> {
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        match result.0 {
            -1 => return Err(windows::core::Error::from_win32()).context("get drag/drop message"),
            0 => return Ok(()),
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

fn ensure_message_queue() {
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, HWND::default(), 0, 0, PM_NOREMOVE);
    }
}

fn virtual_desktop_rect() -> Rect {
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn edge_window_rect(edge: Edge, desktop: Rect, thickness: i32) -> Rect {
    let thickness = thickness.clamp(1, desktop.width.max(desktop.height).max(1));
    match edge {
        Edge::Left => Rect::new(
            desktop.left,
            desktop.top,
            thickness.min(desktop.width),
            desktop.height,
        ),
        Edge::Right => Rect::new(
            desktop.left + desktop.width.saturating_sub(thickness.min(desktop.width)),
            desktop.top,
            thickness.min(desktop.width),
            desktop.height,
        ),
        Edge::Top => Rect::new(
            desktop.left,
            desktop.top,
            desktop.width,
            thickness.min(desktop.height),
        ),
        Edge::Bottom => Rect::new(
            desktop.left,
            desktop.top + desktop.height.saturating_sub(thickness.min(desktop.height)),
            desktop.width,
            thickness.min(desktop.height),
        ),
    }
}

fn extract_hdrop_paths(data_object: &IDataObject) -> windows::core::Result<Vec<String>> {
    let format = hdrop_format_etc();
    let mut medium = unsafe { data_object.GetData(&format)? };
    let paths = unsafe { paths_from_hdrop(HDROP(medium.u.hGlobal.0)) };
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    Ok(paths)
}

unsafe fn paths_from_hdrop(hdrop: HDROP) -> Vec<String> {
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, index, None) };
        if len == 0 {
            continue;
        }
        let mut buffer = vec![0u16; len as usize + 1];
        let written = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) };
        if written == 0 {
            continue;
        }
        paths.push(
            OsString::from_wide(&buffer[..written as usize])
                .to_string_lossy()
                .into_owned(),
        );
    }
    paths
}

fn hdrop_format_etc() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn set_drop_effect(target: *mut DROPEFFECT, effect: DROPEFFECT) {
    if !target.is_null() {
        unsafe {
            *target = effect;
        }
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_window_rect_places_two_pixel_strip_on_configured_edge() {
        let desktop = Rect::new(-100, 50, 1920, 1080);

        assert_eq!(
            edge_window_rect(Edge::Right, desktop, 2),
            Rect::new(1818, 50, 2, 1080)
        );
        assert_eq!(
            edge_window_rect(Edge::Top, desktop, 3),
            Rect::new(-100, 50, 1920, 3)
        );
    }

    #[test]
    fn edge_drop_target_installs_and_uninstalls() {
        let (sender, receiver) = crossbeam_channel::unbounded();

        let target = EdgeDropTarget::install(Edge::Right, sender).unwrap();

        target.uninstall().unwrap();
        assert!(receiver
            .try_iter()
            .all(|event| !matches!(event, DragDropEvent::Error { .. })));
    }
}
