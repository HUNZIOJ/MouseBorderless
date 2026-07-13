# Borderless

Borderless shares one keyboard, mouse, clipboard, and files (copy/paste and drag/drop) between two Windows computers on the same LAN.

## Requirements

- Windows 10 or Windows 11
- Rust stable MSVC toolchain
- Both computers on the same LAN
- Same privilege level on both computers when controlling elevated windows
- TCP mode requires the agent listen port.
- KCP mode requires the reliable UDP port and the pointer UDP port.
- File copy/paste requires the bulk transfer TCP port.

## Run

```powershell
cargo run -p borderless-app
```

## Controller Setup

1. Choose `Controller`.
2. Enter the agent computer IP and port.
3. Choose transport mode: `TCP` for stable default behavior, `KCP` for low-latency UDP behavior.
4. If using KCP, confirm the pointer UDP port.
5. Choose the agent position: left, right, top, or bottom.
6. Enable clipboard text, HTML, image sync, and file copy/paste as needed.
7. Confirm bulk transfer port, incoming cache folder for copy/paste cache, and transfer limits.
8. Click `Save`.
9. Click `Start`.

## Agent Setup

1. Choose `Agent`.
2. Set listen IP to `0.0.0.0`.
3. Set listen port to match the controller.
4. Choose the same transport mode as the controller.
5. If using KCP, confirm the pointer UDP port.
6. Enable matching clipboard and file sharing options.
7. Confirm bulk transfer port and the incoming cache folder for copy/paste cache.
8. Click `Save`.
9. Click `Start`.

## Firewall

Allow the app to listen on the configured port on the agent computer. In KCP mode, also allow the pointer UDP port. For file copy/paste, allow the bulk transfer TCP port.

## Clipboard and Files

- Text, HTML, and image clipboard sync can be toggled separately.
- Copied files and folders are transferred to the peer cache folder, then written to the peer clipboard as local paths.
- `incoming_cache_dir` is used for copy/paste file cache and clipboard-backed file offers.
- Large transfer progress, cancellation, and errors appear in the GUI.

## Drag and Drop

- Drag files toward the shared edge on either computer; the pointer hands off to the other side for choosing a destination.
- No data is transferred until you release the mouse; files are then written directly into the folder you released over.
- Supported drop destinations: the desktop and Explorer folder windows. If the exact release target cannot be resolved, the current Explorer window's directory is used; otherwise the drop fails with a clear error (nothing is silently written to the cache folder).
- Toggle with `file_drag_drop` in the sharing settings; it uses the same bulk transfer port as copy/paste.

## Permissions

If the target window runs as administrator, run Borderless as administrator on both computers.

## Build Portable Release

```powershell
cargo build --release -p borderless-app
if (Test-Path dist\Borderless-windows-x64) { Remove-Item -Recurse -Force dist\Borderless-windows-x64 }
if (Test-Path dist\Borderless-windows-x64.zip) { Remove-Item -Force dist\Borderless-windows-x64.zip }
New-Item -ItemType Directory -Force dist\Borderless-windows-x64 | Out-Null
Copy-Item target\release\borderless.exe dist\Borderless-windows-x64\
Copy-Item config.example.toml dist\Borderless-windows-x64\
Copy-Item README.md dist\Borderless-windows-x64\
Compress-Archive -Force dist\Borderless-windows-x64\* dist\Borderless-windows-x64.zip
```
