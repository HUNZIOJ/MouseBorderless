use anyhow::{anyhow, bail, Context};
use borderless_core::{
    drag_drop::{DragDropSession, DragDropState},
    geometry::{Edge, Rect},
};
use crossbeam_channel::Sender;
use std::{
    ffi::{OsStr, OsString},
    mem::{size_of, ManuallyDrop},
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};
use uuid::Uuid;
use windows::{
    core::{implement, HRESULT, PCWSTR},
    Win32::{
        Foundation::{
            GlobalFree, BOOL, COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
            DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_NOTIMPL, HGLOBAL, HINSTANCE, HWND,
            LPARAM, LRESULT, OLE_E_ADVISENOTSUPPORTED, POINT, POINTL, S_OK, WPARAM,
        },
        System::{
            Com::{
                IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR,
                DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT},
            Ole::{
                DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl,
                OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
                CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
            },
            SystemServices::MODIFIERKEYS_FLAGS,
            Threading::GetCurrentThreadId,
        },
        UI::{
            Shell::{DragQueryFileW, SHCreateStdEnumFmtEtc, DROPFILES, HDROP},
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassInfoW,
                GetMessageW, GetSystemMetrics, PeekMessageW, PostThreadMessageW, RegisterClassW,
                SetLayeredWindowAttributes, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
                LWA_ALPHA, MSG, PM_NOREMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
                SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNA, WINDOW_EX_STYLE, WM_NULL, WM_QUIT,
                WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_POPUP,
            },
        },
    },
};

const EDGE_WINDOW_CLASS: &str = "BorderlessDragDropEdgeWindow";
const EDGE_WINDOW_THICKNESS_PX: i32 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragDropEvent {
    LocalFileDragEntered {
        session: DragDropSession,
        paths: Vec<String>,
    },
    LocalDragCancelled {
        session_id: Uuid,
    },
    LocalDropCommitted {
        session_id: Uuid,
    },
    RemoteDropStarted {
        session_id: Uuid,
    },
    RemoteDropFinished {
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

#[derive(Debug)]
pub struct RemoteFileDrag {
    session_id: Uuid,
    cancel: Arc<AtomicBool>,
    commit: Arc<AtomicBool>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl RemoteFileDrag {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub fn commit(&self) {
        self.commit.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub fn stop(mut self) -> anyhow::Result<()> {
        self.cancel();
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> anyhow::Result<()> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };

        join.join()
            .map_err(|_| anyhow!("remote file drag thread panicked"))
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    fn wake(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_NULL, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for RemoteFileDrag {
    fn drop(&mut self) {
        self.cancel();
        if let Err(error) = self.stop_inner() {
            tracing::warn!(?error, "failed to stop remote file drag");
        }
    }
}

pub fn start_remote_file_drag(
    session_id: Uuid,
    cache_paths: Vec<String>,
    sender: Sender<DragDropEvent>,
) -> anyhow::Result<RemoteFileDrag> {
    if cache_paths.is_empty() {
        bail!("remote file drag requires at least one cache path");
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let commit = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let thread_commit = Arc::clone(&commit);
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let join = thread::Builder::new()
        .name("borderless-remote-file-drag".to_string())
        .spawn(move || {
            if let Err(error) = run_remote_file_drag_thread(
                session_id,
                cache_paths,
                sender,
                thread_cancel,
                thread_commit,
                ready_tx,
            ) {
                tracing::error!(?error, "remote file drag thread exited");
            }
        })
        .context("spawn remote file drag thread")?;

    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Ok(RemoteFileDrag {
            session_id,
            cancel,
            commit,
            thread_id,
            join: Some(join),
        }),
        Ok(Err(error)) => {
            let _ = join.join();
            Err(error)
        }
        Err(error) => {
            let _ = join.join();
            Err(error).context("remote file drag exited before reporting readiness")
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

fn run_remote_file_drag_thread(
    session_id: Uuid,
    cache_paths: Vec<String>,
    sender: Sender<DragDropEvent>,
    cancel: Arc<AtomicBool>,
    commit: Arc<AtomicBool>,
    ready: Sender<anyhow::Result<u32>>,
) -> anyhow::Result<()> {
    let _ole = OleApartment::initialize()?;
    let thread_id = unsafe { GetCurrentThreadId() };
    ensure_message_queue();
    let owner = match create_hidden_drag_window() {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };

    let data_object: IDataObject = FileDataObject {
        payload: build_hdrop_payload(&cache_paths),
    }
    .into();
    let drop_source: IDropSource = FileDropSource { cancel, commit }.into();
    let _ = ready.send(Ok(thread_id));

    let _ = sender.send(DragDropEvent::RemoteDropStarted { session_id });
    let mut effect = DROPEFFECT_NONE;
    let result = unsafe { DoDragDrop(&data_object, &drop_source, DROPEFFECT_COPY, &mut effect) };

    unsafe {
        let _ = DestroyWindow(owner);
    }

    let _ = sender.send(remote_drag_finish_event(session_id, result));

    Ok(())
}

fn remote_drag_finish_event(session_id: Uuid, result: HRESULT) -> DragDropEvent {
    if result == DRAGDROP_S_DROP || (result.is_ok() && result != DRAGDROP_S_CANCEL) {
        DragDropEvent::RemoteDropFinished { session_id }
    } else if result == DRAGDROP_S_CANCEL {
        DragDropEvent::LocalDragCancelled { session_id }
    } else {
        DragDropEvent::Error {
            session_id: Some(session_id),
            message: format!("remote DoDragDrop failed: {result:?}"),
        }
    }
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
                let event = local_drag_enter_event(paths);
                let DragDropEvent::LocalFileDragEntered { session, .. } = &event else {
                    unreachable!("local_drag_enter_event returns LocalFileDragEntered");
                };
                if let Ok(mut active) = self.active_session.lock() {
                    *active = Some(session.session_id);
                }
                let _ = self.sender.send(event);
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
        cancel_active_local_drag(&self.active_session, &self.sender);
        Ok(())
    }

    fn Drop(
        &self,
        _pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        commit_active_local_drop(&self.active_session, &self.sender);
        set_drop_effect(pdweffect, DROPEFFECT_COPY);
        Ok(())
    }
}

fn cancel_active_local_drag(active_session: &Mutex<Option<Uuid>>, sender: &Sender<DragDropEvent>) {
    if let Ok(mut active) = active_session.lock() {
        if let Some(session_id) = active.take() {
            let _ = sender.send(DragDropEvent::LocalDragCancelled { session_id });
        }
    }
}

fn commit_active_local_drop(active_session: &Mutex<Option<Uuid>>, sender: &Sender<DragDropEvent>) {
    if let Ok(mut active) = active_session.lock() {
        if let Some(session_id) = active.take() {
            let _ = sender.send(DragDropEvent::LocalDropCommitted { session_id });
        }
    }
}

#[implement(IDropSource)]
struct FileDropSource {
    cancel: Arc<AtomicBool>,
    commit: Arc<AtomicBool>,
}

#[allow(non_snake_case)]
impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(
        &self,
        fescapepressed: BOOL,
        grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        query_continue_drag_result(
            fescapepressed.as_bool(),
            self.cancel.load(Ordering::SeqCst),
            self.commit.load(Ordering::SeqCst),
            grfkeystate,
        )
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> windows::core::HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

#[implement(IDataObject)]
struct FileDataObject {
    payload: Vec<u8>,
}

#[allow(non_snake_case)]
impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if !format_matches_hdrop(pformatetcin) {
            return Err(windows::core::Error::from_hresult(DV_E_FORMATETC));
        }

        let memory = OwnedGlobalMemory::copy_from_slice(&self.payload)
            .map_err(|error| windows::core::Error::new(DV_E_FORMATETC, error.to_string()))?;

        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: memory.release(),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows::core::HRESULT {
        if format_matches_hdrop(pformatetc) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> windows::core::HRESULT {
        if !pformatetcout.is_null() {
            unsafe {
                *pformatetcout = FORMATETC::default();
            }
        }
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if DATADIR(dwdirection as i32) != DATADIR_GET {
            return Err(windows::core::Error::from_hresult(E_NOTIMPL));
        }
        unsafe { SHCreateStdEnumFmtEtc(&[hdrop_format_etc()]) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Option<&IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(windows::core::Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(windows::core::Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
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

struct GlobalLockGuard {
    handle: HGLOBAL,
    ptr: *mut core::ffi::c_void,
}

impl GlobalLockGuard {
    fn lock(handle: HGLOBAL) -> anyhow::Result<Self> {
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            bail!("global memory lock failed");
        }
        Ok(Self { handle, ptr })
    }
}

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.handle);
        }
    }
}

struct OwnedGlobalMemory {
    handle: HGLOBAL,
}

impl OwnedGlobalMemory {
    fn new(size: usize) -> anyhow::Result<Self> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size) }
            .context("allocate global drag/drop memory")?;
        Ok(Self { handle })
    }

    fn copy_from_slice(bytes: &[u8]) -> anyhow::Result<Self> {
        let memory = Self::new(bytes.len())?;
        {
            let lock = GlobalLockGuard::lock(memory.handle)?;
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), lock.ptr.cast::<u8>(), bytes.len());
            }
        }
        Ok(memory)
    }

    fn release(mut self) -> HGLOBAL {
        let handle = self.handle;
        self.handle = HGLOBAL::default();
        handle
    }
}

impl Drop for OwnedGlobalMemory {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = GlobalFree(self.handle);
            }
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

fn create_hidden_drag_window() -> anyhow::Result<HWND> {
    create_drag_window(Rect::new(0, 0, 1, 1))
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

fn format_matches_hdrop(format: *const FORMATETC) -> bool {
    if format.is_null() {
        return false;
    }
    let format = unsafe { *format };
    format.cfFormat == CF_HDROP.0
        && format.dwAspect == DVASPECT_CONTENT.0
        && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
}

fn set_drop_effect(target: *mut DROPEFFECT, effect: DROPEFFECT) {
    if !target.is_null() {
        unsafe {
            *target = effect;
        }
    }
}

fn local_drag_enter_event(paths: Vec<String>) -> DragDropEvent {
    DragDropEvent::LocalFileDragEntered {
        session: DragDropSession {
            session_id: Uuid::new_v4(),
            transfer_id: Uuid::new_v4(),
            state: DragDropState::LocalDragDetected,
        },
        paths,
    }
}

fn query_continue_drag_result(
    escape_pressed: bool,
    cancel_requested: bool,
    commit_requested: bool,
    key_state: MODIFIERKEYS_FLAGS,
) -> windows::core::HRESULT {
    let _ = key_state;
    if escape_pressed || cancel_requested {
        DRAGDROP_S_CANCEL
    } else if commit_requested {
        DRAGDROP_S_DROP
    } else {
        S_OK
    }
}

fn build_hdrop_payload(paths: &[String]) -> Vec<u8> {
    let path_units = paths
        .iter()
        .map(|path| wide_null(path).len())
        .sum::<usize>()
        + 1;
    let byte_len = size_of::<DROPFILES>() + path_units * size_of::<u16>();
    let mut bytes = vec![0u8; byte_len];

    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: BOOL(0),
        fWide: BOOL(1),
    };
    unsafe {
        ptr::write_unaligned(bytes.as_mut_ptr().cast::<DROPFILES>(), header);
        let mut cursor = bytes.as_mut_ptr().add(size_of::<DROPFILES>()).cast::<u16>();
        for path in paths.iter().map(|path| wide_null(path)) {
            ptr::copy_nonoverlapping(path.as_ptr(), cursor, path.len());
            cursor = cursor.add(path.len());
        }
        *cursor = 0;
    }
    bytes
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdrop_payload_uses_dropfiles_header_and_double_nul_utf16_paths() {
        let bytes = build_hdrop_payload(&["C:\\tmp\\a.txt".to_string(), "D:\\b.bin".to_string()]);

        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 20);
        assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(bytes[8..12].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(bytes[12..16].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1);

        let units = bytes[20..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
            .collect::<Vec<_>>();
        let expected = "C:\\tmp\\a.txt\0D:\\b.bin\0\0"
            .encode_utf16()
            .collect::<Vec<_>>();
        assert_eq!(units, expected);
    }

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
    fn local_drag_enter_event_carries_session_and_paths() {
        let paths = vec!["C:\\tmp\\a.txt".to_string()];

        let event = local_drag_enter_event(paths.clone());

        let DragDropEvent::LocalFileDragEntered {
            session,
            paths: event_paths,
        } = event
        else {
            panic!("expected local drag enter event");
        };
        assert_eq!(session.state, DragDropState::LocalDragDetected);
        assert_eq!(event_paths, paths);
        assert_eq!(session.session_id.get_version_num(), 4);
        assert_eq!(session.transfer_id.get_version_num(), 4);
    }

    #[test]
    fn active_local_drag_cancel_emits_once_and_clears_session() {
        let session_id = Uuid::from_u128(42);
        let active_session = Mutex::new(Some(session_id));
        let (sender, receiver) = crossbeam_channel::unbounded();

        cancel_active_local_drag(&active_session, &sender);
        cancel_active_local_drag(&active_session, &sender);

        let events = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.first(),
            Some(DragDropEvent::LocalDragCancelled { session_id: id }) if *id == session_id
        ));
        assert_eq!(*active_session.lock().unwrap(), None);
    }

    #[test]
    fn active_local_drop_commit_emits_once_without_cancel() {
        let session_id = Uuid::from_u128(43);
        let active_session = Mutex::new(Some(session_id));
        let (sender, receiver) = crossbeam_channel::unbounded();

        commit_active_local_drop(&active_session, &sender);
        commit_active_local_drop(&active_session, &sender);

        let events = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![DragDropEvent::LocalDropCommitted { session_id }]
        );
        assert_eq!(*active_session.lock().unwrap(), None);
    }

    #[test]
    fn remote_drag_result_maps_cancel_before_generic_success() {
        let session_id = Uuid::from_u128(44);

        assert_eq!(
            remote_drag_finish_event(session_id, DRAGDROP_S_CANCEL),
            DragDropEvent::LocalDragCancelled { session_id }
        );
        assert_eq!(
            remote_drag_finish_event(session_id, DRAGDROP_S_DROP),
            DragDropEvent::RemoteDropFinished { session_id }
        );
    }

    #[test]
    fn drag_source_query_waits_for_explicit_commit_or_cancel() {
        assert_eq!(
            query_continue_drag_result(true, false, false, MODIFIERKEYS_FLAGS(1)),
            DRAGDROP_S_CANCEL
        );
        assert_eq!(
            query_continue_drag_result(false, true, false, MODIFIERKEYS_FLAGS(1)),
            DRAGDROP_S_CANCEL
        );
        assert_eq!(
            query_continue_drag_result(false, false, true, MODIFIERKEYS_FLAGS(1)),
            DRAGDROP_S_DROP
        );
        assert_eq!(
            query_continue_drag_result(false, false, false, MODIFIERKEYS_FLAGS(0)),
            S_OK
        );
    }

    #[test]
    fn edge_drop_target_installs_without_reporting_unsupported() {
        let (sender, receiver) = crossbeam_channel::unbounded();

        let target = EdgeDropTarget::install(Edge::Right, sender).unwrap();

        target.uninstall().unwrap();
        assert!(receiver.try_iter().all(|event| {
            !matches!(
                event,
                DragDropEvent::Error {
                    message,
                    ..
                } if message.contains("not implemented")
            )
        }));
    }
}
