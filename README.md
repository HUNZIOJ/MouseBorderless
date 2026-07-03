# Borderless

Borderless shares one keyboard, mouse, clipboard, copied files, and file drag/drop workflows between two Windows computers on the same LAN.

## Requirements

- Windows 10 or Windows 11
- Rust stable MSVC toolchain
- Both computers on the same LAN
- Same privilege level on both computers when controlling elevated windows
- TCP mode requires the agent listen port.
- KCP mode requires the reliable UDP port and the pointer UDP port.
- File copy/paste and file drag/drop require the bulk transfer TCP port.

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
6. Enable clipboard text, HTML, image sync, file copy/paste, and real file drag/drop as needed.
7. Confirm bulk transfer port, incoming cache folder, and transfer limits.
8. Click `Save`.
9. Click `Start`.

## Agent Setup

1. Choose `Agent`.
2. Set listen IP to `0.0.0.0`.
3. Set listen port to match the controller.
4. Choose the same transport mode as the controller.
5. If using KCP, confirm the pointer UDP port.
6. Enable matching clipboard and file sharing options.
7. Confirm bulk transfer port and incoming cache folder.
8. Click `Save`.
9. Click `Start`.

## Firewall

Allow the app to listen on the configured port on the agent computer. In KCP mode, also allow the pointer UDP port. For file copy/paste and real file drag/drop, allow the bulk transfer TCP port.

## Clipboard and Files

- Text, HTML, and image clipboard sync can be toggled separately.
- Copied files and folders are transferred to the peer cache folder, then written to the peer clipboard as local paths.
- Real file drag/drop uses a screen-edge handoff, transfers files to the peer cache folder, and starts a remote Windows file drag with the cached files.
- Large transfer progress, cancellation, and errors appear in the GUI.

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
