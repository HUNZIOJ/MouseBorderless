use anyhow::Context;
use borderless_core::{
    drag_drop::{DropResolutionKind, DropTargetSummary},
    geometry::Point,
};
use std::{os::windows::ffi::OsStringExt, path::PathBuf, ptr};
use uuid::Uuid;
use windows::{
    core::Interface,
    Win32::{
        Foundation::{HWND, MAX_PATH, POINT},
        Graphics::Gdi::ScreenToClient,
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider,
            CLSCTX_ALL, COINIT_APARTMENTTHREADED,
        },
        UI::{
            Shell::{
                Common::ITEMIDLIST, FOLDERID_Desktop, IFolderView, IFolderView2, IPersistFolder2,
                IShellBrowser, IShellItem, IShellWindows, SHGetKnownFolderPath,
                SHGetPathFromIDListW, ShellWindows, KF_FLAG_DEFAULT, SIGDN_FILESYSPATH,
                SVGIO_ALLVIEW, SWC_DESKTOP, SWFO_NEEDDISPATCH,
            },
            WindowsAndMessaging::{GetAncestor, GetClassNameW, WindowFromPoint, GA_ROOT},
        },
    },
};

const SID_STOP_LEVEL_BROWSER: windows::core::GUID =
    windows::core::GUID::from_u128(0x4C96BE40_915C_11CF_99D3_00AA004AE837);

#[derive(Clone, Debug, PartialEq, Eq)]
enum DropCandidate {
    FolderIcon(PathBuf),
    ExplorerDirectory(PathBuf),
    Desktop(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDropTarget {
    pub destination_dir: PathBuf,
    pub display_name: String,
    pub resolution_kind: DropResolutionKind,
}

impl ResolvedDropTarget {
    pub fn summary(&self, authorization: Uuid) -> DropTargetSummary {
        DropTargetSummary {
            display_name: self.display_name.clone(),
            resolution_kind: self.resolution_kind,
            authorization,
        }
    }
}

fn select_drop_candidate(candidates: Vec<DropCandidate>) -> anyhow::Result<DropCandidate> {
    candidates
        .iter()
        .find(|candidate| matches!(candidate, DropCandidate::FolderIcon(_)))
        .cloned()
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| matches!(candidate, DropCandidate::ExplorerDirectory(_)))
                .cloned()
        })
        .or_else(|| {
            candidates
                .into_iter()
                .find(|candidate| matches!(candidate, DropCandidate::Desktop(_)))
        })
        .context("release over the desktop or a filesystem Explorer folder")
}

fn candidate_to_target(candidate: DropCandidate) -> ResolvedDropTarget {
    let (destination_dir, resolution_kind) = match candidate {
        DropCandidate::FolderIcon(path) => (path, DropResolutionKind::FolderIcon),
        DropCandidate::ExplorerDirectory(path) => (path, DropResolutionKind::ExplorerDirectory),
        DropCandidate::Desktop(path) => (path, DropResolutionKind::Desktop),
    };
    let display_name = destination_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| destination_dir.display().to_string());
    ResolvedDropTarget {
        destination_dir,
        display_name,
        resolution_kind,
    }
}

pub fn resolve_drop_target(point: Point) -> anyhow::Result<ResolvedDropTarget> {
    let _com = ComApartment::initialize()?;
    let root = root_window_at(point).context("no window under release point")?;
    let mut candidates = Vec::new();

    if let Some(view) = ShellViewContext::for_root(root)? {
        if let Some(folder) = view.folder_icon_at_screen_point(point)? {
            candidates.push(DropCandidate::FolderIcon(folder));
        }
        if let Some(current) = view.current_filesystem_directory()? {
            candidates.push(DropCandidate::ExplorerDirectory(current));
        }
    }

    if is_desktop_window(root) {
        if let Some(view) = ShellViewContext::desktop()? {
            if let Some(folder) = view.folder_icon_at_screen_point(point)? {
                candidates.push(DropCandidate::FolderIcon(folder));
            }
        }
        candidates.push(DropCandidate::Desktop(desktop_directory()?));
    }

    select_drop_candidate(candidates).map(candidate_to_target)
}

struct ShellViewContext {
    folder_view: IFolderView,
    folder_view2: IFolderView2,
    view_hwnd: HWND,
}

impl ShellViewContext {
    fn for_root(root: HWND) -> anyhow::Result<Option<Self>> {
        let shell_windows = shell_windows()?;
        let count = unsafe { shell_windows.Count() }.context("count shell windows")?;

        for index in 0..count {
            let Ok(dispatch) =
                (unsafe { shell_windows.Item(&windows::core::VARIANT::from(index)) })
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
            if hwnd == root {
                return Self::from_browser(&browser).map(Some);
            }
        }

        Ok(None)
    }

    fn desktop() -> anyhow::Result<Option<Self>> {
        let shell_windows = shell_windows()?;
        let mut hwnd = 0_i32;
        let Ok(dispatch) = (unsafe {
            shell_windows.FindWindowSW(
                ptr::null(),
                ptr::null(),
                SWC_DESKTOP,
                &mut hwnd,
                SWFO_NEEDDISPATCH,
            )
        }) else {
            return Ok(None);
        };
        let Ok(provider) = dispatch.cast::<IServiceProvider>() else {
            return Ok(None);
        };
        let Ok(browser) =
            (unsafe { provider.QueryService::<IShellBrowser>(&SID_STOP_LEVEL_BROWSER) })
        else {
            return Ok(None);
        };
        Self::from_browser(&browser).map(Some)
    }

    fn from_browser(browser: &IShellBrowser) -> anyhow::Result<Self> {
        let shell_view = unsafe { browser.QueryActiveShellView() }
            .context("query active Explorer shell view")?;
        let view_hwnd = unsafe { shell_view.GetWindow() }.context("query shell view window")?;
        let folder_view = shell_view
            .cast::<IFolderView>()
            .context("shell view does not expose IFolderView")?;
        let folder_view2 = shell_view
            .cast::<IFolderView2>()
            .context("shell view does not expose IFolderView2")?;
        Ok(Self {
            folder_view,
            folder_view2,
            view_hwnd,
        })
    }

    fn folder_icon_at_screen_point(&self, point: Point) -> anyhow::Result<Option<PathBuf>> {
        let mut client_point = POINT {
            x: point.x,
            y: point.y,
        };
        if !unsafe { ScreenToClient(self.view_hwnd, &mut client_point) }.as_bool() {
            return Err(windows::core::Error::from_win32()).context("map drop point to shell view");
        }

        let count = unsafe { self.folder_view.ItemCount(SVGIO_ALLVIEW) }
            .context("count shell view items")?;
        let mut spacing = POINT::default();
        unsafe { self.folder_view.GetSpacing(&mut spacing) }.context("query shell item spacing")?;
        let mut view_mode = Default::default();
        let mut icon_size = 0_i32;
        unsafe {
            self.folder_view2
                .GetViewModeAndIconSize(&mut view_mode, &mut icon_size)
        }
        .context("query shell icon size")?;
        let hit_width = spacing.x.max(icon_size.saturating_mul(3)).max(48);
        let hit_height = spacing.y.max(icon_size.saturating_add(32)).max(48);

        for index in 0..count {
            let pidl = match unsafe { self.folder_view.Item(index) } {
                Ok(pidl) => pidl,
                Err(_) => continue,
            };
            let item_position = unsafe { self.folder_view.GetItemPosition(pidl) };
            unsafe {
                CoTaskMemFree(Some(pidl.cast()));
            }
            let Ok(item_position) = item_position else {
                continue;
            };
            if !point_hits_item_cell(client_point, item_position, hit_width, hit_height) {
                continue;
            }

            let Ok(item) = (unsafe { self.folder_view2.GetItem::<IShellItem>(index) }) else {
                continue;
            };
            let Some(path) = filesystem_path_from_item(&item) else {
                continue;
            };
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                return Ok(Some(path));
            }
        }

        Ok(None)
    }

    fn current_filesystem_directory(&self) -> anyhow::Result<Option<PathBuf>> {
        let persist: IPersistFolder2 =
            unsafe { self.folder_view.GetFolder() }.context("get folder from Explorer view")?;
        let pidl = unsafe { persist.GetCurFolder() }.context("get current folder id list")?;
        let directory = path_from_pidl(pidl);
        unsafe {
            CoTaskMemFree(Some(pidl.cast()));
        }
        Ok(directory)
    }
}

fn shell_windows() -> anyhow::Result<IShellWindows> {
    unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }
        .context("create ShellWindows enumerator")
}

fn point_hits_item_cell(point: POINT, item: POINT, width: i32, height: i32) -> bool {
    let left = item.x.saturating_sub(width / 4);
    let top = item.y.saturating_sub(height / 4);
    point.x >= left
        && point.x < left.saturating_add(width)
        && point.y >= top
        && point.y < top.saturating_add(height)
}

fn filesystem_path_from_item(item: &IShellItem) -> Option<PathBuf> {
    let raw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    let value = unsafe { raw.to_string() }.ok().map(PathBuf::from);
    unsafe {
        CoTaskMemFree(Some(raw.0.cast()));
    }
    value
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
    (!root.is_invalid()).then_some(root)
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
    (written > 0).then(|| String::from_utf16_lossy(&buffer[..written as usize]))
}

fn desktop_directory() -> anyhow::Result<PathBuf> {
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) }
        .context("resolve desktop directory")?;
    let value = unsafe { path.to_string() }.context("desktop directory is not valid UTF-16")?;
    unsafe {
        CoTaskMemFree(Some(path.0.cast()));
    }
    Ok(PathBuf::from(value))
}

fn path_from_pidl(pidl: *const ITEMIDLIST) -> Option<PathBuf> {
    let mut buffer = [0u16; MAX_PATH as usize];
    if !unsafe { SHGetPathFromIDListW(pidl, &mut buffer) }.as_bool() {
        return None;
    }
    let len = buffer.iter().position(|&unit| unit == 0)?;
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])))
}

struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    fn initialize() -> anyhow::Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_icon_wins_over_explorer_directory() {
        let candidates = vec![
            DropCandidate::ExplorerDirectory(PathBuf::from("C:\\Work")),
            DropCandidate::FolderIcon(PathBuf::from("C:\\Work\\Reports")),
        ];
        assert_eq!(
            select_drop_candidate(candidates).unwrap(),
            DropCandidate::FolderIcon(PathBuf::from("C:\\Work\\Reports"))
        );
    }

    #[test]
    fn explorer_empty_area_uses_current_directory() {
        assert_eq!(
            select_drop_candidate(vec![DropCandidate::ExplorerDirectory(PathBuf::from(
                "C:\\Work",
            ))])
            .unwrap(),
            DropCandidate::ExplorerDirectory(PathBuf::from("C:\\Work"))
        );
    }

    #[test]
    fn unsupported_shell_location_returns_an_error() {
        assert!(select_drop_candidate(Vec::new()).is_err());
    }

    #[test]
    fn target_summary_contains_no_local_path() {
        let target = candidate_to_target(DropCandidate::FolderIcon(PathBuf::from(
            "C:\\Work\\Reports",
        )));

        assert_eq!(
            target.summary(Uuid::from_u128(9)),
            DropTargetSummary {
                display_name: "Reports".to_string(),
                resolution_kind: DropResolutionKind::FolderIcon,
                authorization: Uuid::from_u128(9),
            }
        );
    }
}
