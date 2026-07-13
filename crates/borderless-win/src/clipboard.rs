use anyhow::{anyhow, bail, Context};
use borderless_core::{
    clipboard::{ClipboardChangeId, ClipboardEnvelope, ClipboardPayload, RemoteFileOffer},
    file_transfer::FileManifestEntry,
};
use crossbeam_channel::Sender;
use std::{
    ffi::{OsStr, OsString},
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::Path,
    ptr, slice,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use uuid::Uuid;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{GlobalFree, BOOL, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::{
            DataExchange::{
                AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
                IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
                RemoveClipboardFormatListener, SetClipboardData,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{
                GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
            },
            Threading::GetCurrentThreadId,
        },
        UI::{
            Shell::{DragQueryFileW, DROPFILES, HDROP},
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassInfoW,
                GetMessageW, PeekMessageW, PostThreadMessageW, RegisterClassW, TranslateMessage,
                HWND_MESSAGE, MSG, PM_NOREMOVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE,
                WM_QUIT, WNDCLASSW,
            },
        },
    },
};

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;
const SUPPRESS_AFTER_REMOTE_WRITE: Duration = Duration::from_millis(750);
const OPEN_CLIPBOARD_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(2),
    Duration::from_millis(5),
    Duration::from_millis(10),
];
const CLIPBOARD_WINDOW_CLASS: &str = "BorderlessClipboardMonitorWindow";

static LOCAL_DEVICE_ID: OnceLock<Uuid> = OnceLock::new();
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static SUPPRESS_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Clone, Debug)]
pub enum ClipboardEvent {
    Changed(ClipboardEnvelope),
    Ignored(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardReadOptions {
    pub text: bool,
    pub html: bool,
    pub images: bool,
    pub files: bool,
    pub max_bytes: u64,
}

impl ClipboardReadOptions {
    pub fn all_with_max_bytes(max_bytes: u64) -> Self {
        Self {
            text: true,
            html: true,
            images: true,
            files: true,
            max_bytes,
        }
    }
}

impl Default for ClipboardReadOptions {
    fn default() -> Self {
        Self::all_with_max_bytes(u64::MAX)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardReadFormat {
    Files,
    Png,
    Dib,
    Html,
    UnicodeText,
}

pub struct ClipboardMonitor {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl ClipboardMonitor {
    pub fn start(sender: Sender<ClipboardEvent>) -> anyhow::Result<Self> {
        Self::start_with_options(sender, ClipboardReadOptions::default())
    }

    pub fn start_with_options(
        sender: Sender<ClipboardEvent>,
        options: ClipboardReadOptions,
    ) -> anyhow::Result<Self> {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let join = thread::Builder::new()
            .name("borderless-clipboard-monitor".to_string())
            .spawn(move || {
                if let Err(error) = run_clipboard_monitor_thread(sender, ready_tx, options) {
                    tracing::error!(?error, "clipboard monitor thread exited");
                }
            })
            .context("spawn clipboard monitor thread")?;

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
                Err(error).context("clipboard monitor exited before reporting readiness")
            }
        }
    }

    pub fn stop(mut self) -> anyhow::Result<()> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> anyhow::Result<()> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };

        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0))
                .context("post clipboard monitor quit message")?;
        }

        join.join()
            .map_err(|_| anyhow!("clipboard monitor thread panicked"))
    }
}

impl Drop for ClipboardMonitor {
    fn drop(&mut self) {
        if let Err(error) = self.stop_inner() {
            tracing::warn!(?error, "failed to stop clipboard monitor");
        }
    }
}

pub fn read_current_clipboard(max_bytes: u64) -> anyhow::Result<Option<ClipboardPayload>> {
    read_current_clipboard_with_options(ClipboardReadOptions::all_with_max_bytes(max_bytes))
}

pub fn read_current_clipboard_with_options(
    options: ClipboardReadOptions,
) -> anyhow::Result<Option<ClipboardPayload>> {
    let html_format = registered_clipboard_format("HTML Format")?;
    let png_format = registered_clipboard_format("PNG")?;
    let _clipboard = ClipboardOpenGuard::open_read()?;

    for format in clipboard_read_format_order(options) {
        match format {
            ClipboardReadFormat::Files => {
                if let Some(files) = read_hdrop(options.max_bytes)? {
                    return Ok(Some(ClipboardPayload::Files(files)));
                }
            }
            ClipboardReadFormat::Png => {
                if let Some(bytes) = read_format_bytes(png_format, options.max_bytes)? {
                    return Ok(Some(ClipboardPayload::ImagePng(bytes)));
                }
            }
            ClipboardReadFormat::Dib => {
                if let Some(bytes) = read_format_bytes(CF_DIB, options.max_bytes)? {
                    return Ok(Some(ClipboardPayload::ImageDib(bytes)));
                }
            }
            ClipboardReadFormat::Html => {
                if let Some(html) = read_html(html_format, options.max_bytes)? {
                    return Ok(Some(ClipboardPayload::Html(html)));
                }
            }
            ClipboardReadFormat::UnicodeText => {
                if let Some(text) = read_unicode_text(options.max_bytes)? {
                    return Ok(Some(ClipboardPayload::UnicodeText(text)));
                }
            }
        }
    }

    Ok(None)
}

pub fn write_clipboard(payload: &ClipboardPayload) -> anyhow::Result<()> {
    with_remote_write_suppression(|| write_clipboard_inner(payload))
}

fn write_clipboard_inner(payload: &ClipboardPayload) -> anyhow::Result<()> {
    let html_format = registered_clipboard_format("HTML Format")?;
    let png_format = registered_clipboard_format("PNG")?;
    let owner = ClipboardOwnerWindow::create()?;

    {
        let _clipboard = ClipboardOpenGuard::open_write(owner.hwnd())?;

        unsafe {
            EmptyClipboard().context("empty clipboard")?;
        }

        match payload {
            ClipboardPayload::UnicodeText(text) => write_unicode_text(text)?,
            ClipboardPayload::Html(html) => write_bytes(html_format, &html_format_bytes(html))?,
            ClipboardPayload::ImagePng(bytes) => write_bytes(png_format, bytes)?,
            ClipboardPayload::ImageDib(bytes) => write_bytes(CF_DIB, bytes)?,
            ClipboardPayload::Files(offer) => write_hdrop(offer)?,
        }
    }

    Ok(())
}

fn run_clipboard_monitor_thread(
    sender: Sender<ClipboardEvent>,
    ready: Sender<anyhow::Result<u32>>,
    options: ClipboardReadOptions,
) -> anyhow::Result<()> {
    let thread_id = unsafe { GetCurrentThreadId() };
    ensure_message_queue();

    let hwnd = match create_clipboard_window() {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };

    let listener = match ClipboardFormatListener::register(hwnd) {
        Ok(listener) => listener,
        Err(error) => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };

    let _ = ready.send(Ok(thread_id));
    let loop_result = clipboard_message_loop(&sender, options);
    drop(listener);
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    loop_result
}

fn clipboard_message_loop(
    sender: &Sender<ClipboardEvent>,
    options: ClipboardReadOptions,
) -> anyhow::Result<()> {
    let mut message = MSG::default();

    loop {
        let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        match result.0 {
            -1 => return Err(windows::core::Error::from_win32()).context("get clipboard message"),
            0 => return Ok(()),
            _ if message.message == WM_CLIPBOARDUPDATE => handle_clipboard_update(sender, options),
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

fn handle_clipboard_update(sender: &Sender<ClipboardEvent>, options: ClipboardReadOptions) {
    if take_local_suppress_window() {
        let _ = sender.send(ClipboardEvent::Ignored(
            "suppressed local clipboard echo".to_string(),
        ));
        return;
    }

    match read_current_clipboard_with_options(options) {
        Ok(Some(payload)) => {
            let envelope = ClipboardEnvelope {
                change_id: ClipboardChangeId::new(local_device_id(), next_sequence()),
                payload,
            };
            let _ = sender.send(ClipboardEvent::Changed(envelope));
        }
        Ok(None) => {
            let _ = sender.send(ClipboardEvent::Ignored(
                "clipboard contains no supported format".to_string(),
            ));
        }
        Err(error) => {
            let _ = sender.send(ClipboardEvent::Error(error.to_string()));
        }
    }
}

fn clipboard_read_format_order(options: ClipboardReadOptions) -> Vec<ClipboardReadFormat> {
    let mut formats = Vec::with_capacity(5);
    if options.files {
        formats.push(ClipboardReadFormat::Files);
    }
    if options.images {
        formats.push(ClipboardReadFormat::Png);
        formats.push(ClipboardReadFormat::Dib);
    }
    if options.html {
        formats.push(ClipboardReadFormat::Html);
    }
    if options.text {
        formats.push(ClipboardReadFormat::UnicodeText);
    }
    formats
}

fn create_clipboard_window() -> anyhow::Result<HWND> {
    let class_name = wide_null(CLIPBOARD_WINDOW_CLASS);
    let module: HINSTANCE = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .context("get current module handle")?
        .into();
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(clipboard_window_proc),
        hInstance: module,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };

    let atom = unsafe { RegisterClassW(&window_class) };
    if atom == 0 {
        let mut existing = WNDCLASSW::default();
        unsafe {
            GetClassInfoW(module, PCWSTR(class_name.as_ptr()), &mut existing)
                .context("register clipboard window class")?;
        }
    }

    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            module,
            None,
        )
    }
    .context("create clipboard message window")
}

unsafe extern "system" fn clipboard_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn ensure_message_queue() {
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, HWND::default(), 0, 0, PM_NOREMOVE);
    }
}

struct ClipboardFormatListener(HWND);

impl ClipboardFormatListener {
    fn register(hwnd: HWND) -> anyhow::Result<Self> {
        unsafe {
            AddClipboardFormatListener(hwnd).context("add clipboard format listener")?;
        }
        Ok(Self(hwnd))
    }
}

impl Drop for ClipboardFormatListener {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveClipboardFormatListener(self.0);
        }
    }
}

struct ClipboardOpenGuard;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardOpenMode {
    Read,
    Write,
}

impl ClipboardOpenGuard {
    fn open_read() -> anyhow::Result<Self> {
        Self::open(ClipboardOpenMode::Read, None)
    }

    fn open_write(owner: HWND) -> anyhow::Result<Self> {
        Self::open(ClipboardOpenMode::Write, Some(owner))
    }

    fn open(mode: ClipboardOpenMode, owner: Option<HWND>) -> anyhow::Result<Self> {
        let hwnd = clipboard_open_hwnd(mode, owner)?;
        open_clipboard_with_retry(hwnd).context("open clipboard")?;
        Ok(Self)
    }
}

fn clipboard_open_hwnd(mode: ClipboardOpenMode, owner: Option<HWND>) -> anyhow::Result<HWND> {
    match mode {
        ClipboardOpenMode::Read => Ok(HWND::default()),
        ClipboardOpenMode::Write => {
            let Some(owner) = owner.filter(|hwnd| !hwnd.is_invalid()) else {
                bail!("clipboard write requires a non-null owner window");
            };
            Ok(owner)
        }
    }
}

fn open_clipboard_with_retry(hwnd: HWND) -> windows::core::Result<()> {
    let mut attempt = 0;
    loop {
        match unsafe { OpenClipboard(hwnd) } {
            Ok(()) => return Ok(()),
            Err(error) => {
                let Some(delay) = open_clipboard_retry_delay(attempt) else {
                    return Err(error);
                };
                attempt += 1;
                thread::sleep(delay);
            }
        }
    }
}

fn open_clipboard_retry_delay(attempt: usize) -> Option<Duration> {
    OPEN_CLIPBOARD_RETRY_DELAYS.get(attempt).copied()
}

struct ClipboardOwnerWindow {
    hwnd: HWND,
}

impl ClipboardOwnerWindow {
    fn create() -> anyhow::Result<Self> {
        create_clipboard_window().map(|hwnd| Self { hwnd })
    }

    fn hwnd(&self) -> HWND {
        self.hwnd
    }
}

impl Drop for ClipboardOwnerWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Drop for ClipboardOpenGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
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
            bail!("global clipboard memory lock failed");
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
            .context("allocate global clipboard memory")?;
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

fn read_unicode_text(max_bytes: u64) -> anyhow::Result<Option<String>> {
    if !format_available(CF_UNICODETEXT) {
        return Ok(None);
    }

    let handle = clipboard_data_hglobal(CF_UNICODETEXT)?;
    let size = unsafe { GlobalSize(handle) };
    ensure_size_within_limit(size as u64, max_bytes, "clipboard text")?;
    if size < 2 {
        return Ok(Some(String::new()));
    }

    let lock = GlobalLockGuard::lock(handle)?;
    let units = unsafe { slice::from_raw_parts(lock.ptr.cast::<u16>(), size / 2) };
    let len = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    Ok(Some(String::from_utf16_lossy(&units[..len])))
}

fn read_html(format: u32, max_bytes: u64) -> anyhow::Result<Option<String>> {
    let Some(bytes) = read_format_bytes(format, max_bytes)? else {
        return Ok(None);
    };
    Ok(Some(
        String::from_utf8_lossy(trim_trailing_nuls(&bytes)).into_owned(),
    ))
}

fn read_format_bytes(format: u32, max_bytes: u64) -> anyhow::Result<Option<Vec<u8>>> {
    if !format_available(format) {
        return Ok(None);
    }

    let handle = clipboard_data_hglobal(format)?;
    let size = unsafe { GlobalSize(handle) };
    ensure_size_within_limit(size as u64, max_bytes, "clipboard data")?;
    if size == 0 {
        return Ok(Some(Vec::new()));
    }

    let lock = GlobalLockGuard::lock(handle)?;
    let bytes = unsafe { slice::from_raw_parts(lock.ptr.cast::<u8>(), size) }.to_vec();
    Ok(Some(bytes))
}

fn read_hdrop(max_bytes: u64) -> anyhow::Result<Option<RemoteFileOffer>> {
    if !format_available(CF_HDROP) {
        return Ok(None);
    }

    let handle = clipboard_data_hglobal(CF_HDROP)?;
    let hdrop_memory_size = unsafe { GlobalSize(handle) };
    ensure_size_within_limit(hdrop_memory_size as u64, max_bytes, "clipboard file list")?;

    let hdrop = HDROP(handle.0);
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    if count == 0 {
        return Ok(None);
    }

    let mut files = Vec::with_capacity(count as usize);
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

        let path = OsString::from_wide(&buffer[..written as usize])
            .to_string_lossy()
            .into_owned();
        files.push(file_manifest_entry_for_path(&path));
    }

    if files.is_empty() {
        return Ok(None);
    }

    let offer = RemoteFileOffer {
        transfer_id: Uuid::new_v4(),
        files,
    };
    Ok(Some(offer))
}

fn file_manifest_entry_for_path(path: &str) -> FileManifestEntry {
    match std::fs::metadata(path) {
        Ok(metadata) => FileManifestEntry {
            relative_path: path.to_string(),
            size_bytes: if metadata.is_dir() { 0 } else { metadata.len() },
            is_dir: metadata.is_dir(),
            blake3_hex: None,
        },
        Err(_) => FileManifestEntry {
            relative_path: path.to_string(),
            size_bytes: 0,
            is_dir: Path::new(path).is_dir(),
            blake3_hex: None,
        },
    }
}

fn write_unicode_text(text: &str) -> anyhow::Result<()> {
    let bytes = wide_null(text)
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    write_bytes(CF_UNICODETEXT, &bytes)
}

fn write_hdrop(offer: &RemoteFileOffer) -> anyhow::Result<()> {
    let paths = offer
        .files
        .iter()
        .map(|file| wide_null(&file.relative_path))
        .collect::<Vec<_>>();
    let path_units = paths.iter().map(Vec::len).sum::<usize>() + 1;
    let byte_len = size_of::<DROPFILES>() + path_units * size_of::<u16>();
    let memory = OwnedGlobalMemory::new(byte_len)?;
    {
        let lock = GlobalLockGuard::lock(memory.handle)?;
        unsafe {
            ptr::write_unaligned(
                lock.ptr.cast::<DROPFILES>(),
                DROPFILES {
                    pFiles: size_of::<DROPFILES>() as u32,
                    pt: POINT { x: 0, y: 0 },
                    fNC: BOOL(0),
                    fWide: BOOL(1),
                },
            );

            let mut cursor = lock
                .ptr
                .cast::<u8>()
                .add(size_of::<DROPFILES>())
                .cast::<u16>();
            for path in &paths {
                ptr::copy_nonoverlapping(path.as_ptr(), cursor, path.len());
                cursor = cursor.add(path.len());
            }
            *cursor = 0;
        }
    }

    set_clipboard_data(CF_HDROP, memory)
}

fn write_bytes(format: u32, bytes: &[u8]) -> anyhow::Result<()> {
    let memory = OwnedGlobalMemory::copy_from_slice(bytes)?;
    set_clipboard_data(format, memory)
}

fn set_clipboard_data(format: u32, memory: OwnedGlobalMemory) -> anyhow::Result<()> {
    let handle = memory.release();
    match unsafe { SetClipboardData(format, windows::Win32::Foundation::HANDLE(handle.0)) } {
        Ok(_) => Ok(()),
        Err(error) => {
            unsafe {
                let _ = GlobalFree(handle);
            }
            Err(error).context("set clipboard data")
        }
    }
}

fn clipboard_data_hglobal(format: u32) -> anyhow::Result<HGLOBAL> {
    let handle = unsafe { GetClipboardData(format) }.context("get clipboard data")?;
    if handle.is_invalid() {
        bail!("clipboard data handle is invalid");
    }
    Ok(HGLOBAL(handle.0))
}

fn registered_clipboard_format(name: &str) -> anyhow::Result<u32> {
    let wide = wide_null(name);
    let format = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    if format == 0 {
        return Err(windows::core::Error::from_win32())
            .with_context(|| format!("register clipboard format {name}"));
    }
    Ok(format)
}

fn format_available(format: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(format).is_ok() }
}

fn html_format_bytes(html: &str) -> Vec<u8> {
    if html.starts_with("Version:") {
        return nul_terminated_bytes(html.as_bytes());
    }

    let before_fragment = "<html><body><!--StartFragment-->";
    let after_fragment = "<!--EndFragment--></body></html>";
    let body = format!("{before_fragment}{html}{after_fragment}");
    let empty_header =
        "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let start_html = empty_header.len();
    let end_html = start_html + body.len();
    let start_fragment = start_html + before_fragment.len();
    let end_fragment = start_fragment + html.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );

    nul_terminated_bytes(format!("{header}{body}").as_bytes())
}

fn nul_terminated_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if !out.ends_with(&[0]) {
        out.push(0);
    }
    out
}

fn trim_trailing_nuls(bytes: &[u8]) -> &[u8] {
    let len = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |i| i + 1);
    &bytes[..len]
}

fn ensure_size_within_limit(size: u64, max_bytes: u64, label: &str) -> anyhow::Result<()> {
    if size > max_bytes {
        bail!("{label} is {size} bytes, limit is {max_bytes}");
    }
    Ok(())
}

fn local_device_id() -> Uuid {
    *LOCAL_DEVICE_ID.get_or_init(Uuid::new_v4)
}

fn next_sequence() -> u64 {
    NEXT_SEQUENCE.fetch_add(1, Ordering::SeqCst)
}

fn record_local_suppress_window() {
    if let Ok(mut suppress_until) = SUPPRESS_UNTIL.lock() {
        *suppress_until = Some(Instant::now() + SUPPRESS_AFTER_REMOTE_WRITE);
    }
}

fn with_remote_write_suppression<T>(operation: impl FnOnce() -> T) -> T {
    record_local_suppress_window();
    operation()
}

fn take_local_suppress_window() -> bool {
    let Ok(mut suppress_until) = SUPPRESS_UNTIL.lock() else {
        return false;
    };
    let Some(until) = *suppress_until else {
        return false;
    };

    if Instant::now() <= until {
        true
    } else {
        *suppress_until = None;
        false
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
fn clear_local_suppress_window_for_test() {
    if let Ok(mut suppress_until) = SUPPRESS_UNTIL.lock() {
        *suppress_until = None;
    }
}

#[cfg(test)]
fn set_local_suppress_until_for_test(until: Instant) {
    if let Ok(mut suppress_until) = SUPPRESS_UNTIL.lock() {
        *suppress_until = Some(until);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_write_open_requires_owner_window() {
        let owner = HWND(std::ptr::dangling_mut::<core::ffi::c_void>());

        assert_eq!(
            clipboard_open_hwnd(ClipboardOpenMode::Read, None).unwrap(),
            HWND::default()
        );
        assert_eq!(
            clipboard_open_hwnd(ClipboardOpenMode::Write, Some(owner)).unwrap(),
            owner
        );
        assert!(clipboard_open_hwnd(ClipboardOpenMode::Write, None)
            .unwrap_err()
            .to_string()
            .contains("owner window"));
    }

    #[test]
    fn remote_write_suppression_is_active_before_write_operation_runs() {
        clear_local_suppress_window_for_test();

        let active_during_operation = with_remote_write_suppression(take_local_suppress_window);

        assert!(active_during_operation);
    }

    #[test]
    fn remote_write_suppression_window_is_not_consumed_by_first_check_and_expires() {
        clear_local_suppress_window_for_test();

        record_local_suppress_window();

        assert!(take_local_suppress_window());
        assert!(take_local_suppress_window());

        set_local_suppress_until_for_test(Instant::now() - Duration::from_millis(1));

        assert!(!take_local_suppress_window());
    }

    #[test]
    fn open_clipboard_retry_delay_uses_short_bounded_backoff() {
        assert_eq!(
            open_clipboard_retry_delay(0),
            Some(Duration::from_millis(2))
        );
        assert_eq!(
            open_clipboard_retry_delay(1),
            Some(Duration::from_millis(5))
        );
        assert_eq!(
            open_clipboard_retry_delay(2),
            Some(Duration::from_millis(10))
        );
        assert_eq!(open_clipboard_retry_delay(3), None);
    }

    #[test]
    fn default_read_options_enable_all_formats_with_requested_size_limit() {
        let options = ClipboardReadOptions::all_with_max_bytes(42);

        assert!(options.text);
        assert!(options.html);
        assert!(options.images);
        assert!(options.files);
        assert_eq!(options.max_bytes, 42);
    }

    #[test]
    fn read_policy_skips_disabled_higher_priority_formats() {
        let options = ClipboardReadOptions {
            text: true,
            html: false,
            images: false,
            files: false,
            max_bytes: 1024,
        };

        assert_eq!(
            clipboard_read_format_order(options),
            vec![ClipboardReadFormat::UnicodeText]
        );
    }
}
