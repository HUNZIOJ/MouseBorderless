use anyhow::{bail, Context};
use borderless_core::{
    drag_drop::{DropResolutionKind, DropTarget},
    geometry::Point,
};
use std::os::windows::ffi::OsStringExt;
use windows::{
    core::Interface,
    Win32::{
        Foundation::{HWND, MAX_PATH, POINT},
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, IServiceProvider, CLSCTX_ALL,
            COINIT_APARTMENTTHREADED,
        },
        UI::{
            Shell::{
                Common::ITEMIDLIST, FOLDERID_Desktop, IFolderView, IPersistFolder2, IShellBrowser,
                IShellWindows, SHGetKnownFolderPath, SHGetPathFromIDListW, ShellWindows,
                KF_FLAG_DEFAULT,
            },
            WindowsAndMessaging::{
                GetAncestor, GetClassNameW, GetForegroundWindow, WindowFromPoint, GA_ROOT,
            },
        },
    },
};

// Service ID for the top-level browser (SID_STopLevelBrowser from shlguid.h).
const SID_STOP_LEVEL_BROWSER: windows::core::GUID =
    windows::core::GUID::from_u128(0x4C96BE40_915C_11CF_99D3_00AA004AE837);

/// Resolve an on-screen release point into a destination directory.
///
/// Resolution order (per the targeted drag-drop design):
/// 1. desktop hit -> the user's desktop directory
/// 2. Explorer window under the point -> that window's current directory
/// 3. foreground Explorer window -> its current directory (fallback)
/// 4. explicit failure — never silently falls back to the cache directory
pub fn resolve_drop_target(point: Point) -> anyhow::Result<DropTarget> {
    let _com = ComApartment::initialize()?;

    let root = root_window_at(point);

    if let Some(root) = root {
        if is_desktop_window(root) {
            return Ok(DropTarget {
                destination_dir: desktop_directory()?,
                resolution_kind: DropResolutionKind::DesktopHit,
            });
        }

        if let Some(directory) = explorer_directory_for_root(root)? {
            return Ok(DropTarget {
                destination_dir: directory,
                resolution_kind: DropResolutionKind::ExplorerHit,
            });
        }
    }

    let foreground = unsafe { GetForegroundWindow() };
    if !foreground.is_invalid() {
        let foreground_root = unsafe { GetAncestor(foreground, GA_ROOT) };
        if let Some(directory) = explorer_directory_for_root(foreground_root)? {
            return Ok(DropTarget {
                destination_dir: directory,
                resolution_kind: DropResolutionKind::ExplorerFallback,
            });
        }
    }

    bail!(
        "no drop destination at ({}, {}): release over the desktop or an Explorer window",
        point.x,
        point.y
    )
}

fn root_window_at(point: Point) -> Option<HWND> {
    let hwnd = unsafe {
        WindowFromPoint(POINT {
            x: point.x,
            y: point.y,
        })
    };
    if hwnd.is_invalid() {
        return None;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_invalid() {
        None
    } else {
        Some(root)
    }
}

fn is_desktop_window(root: HWND) -> bool {
    matches!(
        window_class_name(root).as_deref(),
        Some("Progman") | Some("WorkerW")
    )
}

fn window_class_name(hwnd: HWND) -> Option<String> {
    let mut buffer = [0u16; 256];
    let written = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if written <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..written as usize]))
}

fn desktop_directory() -> anyhow::Result<String> {
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) }
        .context("resolve desktop directory")?;
    let value = unsafe { path.to_string() }.context("desktop directory is not valid UTF-16")?;
    Ok(value)
}

/// Find the Explorer window whose top-level HWND matches `root` and return
/// the directory its folder view currently shows.
fn explorer_directory_for_root(root: HWND) -> anyhow::Result<Option<String>> {
    let shell_windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }
        .context("create ShellWindows enumerator")?;
    let count = unsafe { shell_windows.Count() }.context("count shell windows")?;

    for index in 0..count {
        let Ok(dispatch) = (unsafe { shell_windows.Item(&windows::core::VARIANT::from(index)) })
        else {
            continue;
        };
        let Ok(provider) = dispatch.cast::<IServiceProvider>() else {
            continue;
        };
        let Ok(browser) =
            (unsafe { provider.QueryService::<IShellBrowser>(&SID_STOP_LEVEL_BROWSER) })
        else {
            continue;
        };
        let Ok(hwnd) = (unsafe { browser.GetWindow() }) else {
            continue;
        };
        if hwnd != root {
            continue;
        }

        let view = unsafe { browser.QueryActiveShellView() }
            .context("query active shell view for matched Explorer window")?;
        let folder_view: IFolderView = view
            .cast()
            .context("Explorer view does not expose IFolderView")?;
        let persist: IPersistFolder2 =
            unsafe { folder_view.GetFolder() }.context("get folder from Explorer view")?;
        let pidl = unsafe { persist.GetCurFolder() }.context("get current folder id list")?;
        let directory = path_from_pidl(pidl);
        unsafe {
            windows::Win32::System::Com::CoTaskMemFree(Some(pidl.cast()));
        }
        return Ok(directory);
    }

    Ok(None)
}

fn path_from_pidl(pidl: *const ITEMIDLIST) -> Option<String> {
    let mut buffer = [0u16; MAX_PATH as usize];
    let ok = unsafe { SHGetPathFromIDListW(pidl, &mut buffer) }.as_bool();
    if !ok {
        return None;
    }
    let len = buffer.iter().position(|&unit| unit == 0)?;
    Some(
        std::ffi::OsString::from_wide(&buffer[..len])
            .to_string_lossy()
            .into_owned(),
    )
}

struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    fn initialize() -> anyhow::Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        // S_FALSE means the apartment already exists on this thread; keep it.
        Ok(Self {
            initialized: result.is_ok(),
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
