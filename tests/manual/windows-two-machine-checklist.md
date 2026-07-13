# Windows Two-Machine Acceptance Checklist

## Setup

- Controller computer has physical keyboard and mouse.
- Agent computer is on the same LAN.
- Agent firewall allows the control TCP port and bulk transfer TCP port.
- Both apps are running at the same privilege level.

## Test Record

- Date: Not run
- Controller OS/build and scaling: Not recorded
- Agent OS/build and scaling: Not recorded
- Borderless build/commit: Not recorded
- Network/firewall notes: Not recorded

## Connection

- [ ] Agent shows waiting or connected state.
- [ ] Controller connects to agent IP and port.
- [ ] Both GUIs show connected state.
- [ ] RTT appears and updates.
- [ ] The connection reports TCP in the GUI.

## Edge Switching

- [ ] Left layout enters and returns correctly.
- [ ] Right layout enters and returns correctly.
- [ ] Top layout enters and returns correctly.
- [ ] Bottom layout enters and returns correctly.

## Mouse

- [ ] Remote pointer moves smoothly.
- [ ] Remote pointer remains smooth during sustained edge crossing without double-speed jumps.
- [ ] Mouse diagnostics show raw delta and fallback activity while moving remotely.
- [ ] Left click works.
- [ ] Right click works.
- [ ] Middle click works when available.
- [ ] Wheel works vertically.

## Keyboard

- [ ] Regular letters work.
- [ ] Modifier keys press and release correctly.
- [ ] Key repeat does not get stuck.
- [ ] Pressed keys release after Stop.

## Clipboard

- [ ] Unicode text syncs controller to agent.
- [ ] Unicode text syncs agent to controller.
- [ ] HTML formatting syncs into a rich text target.
- [ ] Image clipboard syncs into Paint.
- [ ] Clipboard loop prevention avoids repeated re-sync events.
- [ ] Oversized clipboard content is refused with a clear GUI reason.

## Cross-Machine Copy/Paste

- [ ] Single copied file pastes into remote Explorer.
- [ ] Multiple copied files paste into remote Explorer.
- [ ] Copied folder pastes with relative structure preserved.
- [ ] File name conflict creates a renamed file instead of overwriting.
- [ ] Transfer progress appears in GUI.
- [ ] Cancel leaves only temporary partial files.

## Drag/Drop: Controller to Agent

| Scenario | Status | Notes |
| --- | --- | --- |
| Single file to desktop | Not run | Confirm source remains and target contents match. |
| Multiple files to Explorer current directory | Not run | Release over empty Explorer space. |
| Folder to a filesystem folder icon | Not run | Confirm exact icon directory and nested contents. |
| Same-name target conflict | Not run | Confirm automatic rename and no overwrite. |
| Cancel before release | Not run | Confirm no payload starts and input returns locally. |
| Disconnect during transfer | Not run | Confirm partial files are removed and input recovers. |
| Unwritable destination | Not run | Confirm clear failure and no cache/desktop fallback. |
| Large file progress | Not run | Confirm progress, destination, and verified completion. |

## Drag/Drop: Agent to Controller

| Scenario | Status | Notes |
| --- | --- | --- |
| Single file to desktop | Not run | Confirm source remains and target contents match. |
| Multiple files to Explorer current directory | Not run | Release over empty Explorer space. |
| Folder to a filesystem folder icon | Not run | Confirm exact icon directory and nested contents. |
| Same-name target conflict | Not run | Confirm automatic rename and no overwrite. |
| Cancel before release | Not run | Confirm no payload starts and input returns locally. |
| Disconnect during transfer | Not run | Confirm partial files are removed and input recovers. |
| Unwritable destination | Not run | Confirm clear failure and no cache/desktop fallback. |
| Large file progress | Not run | Confirm progress, destination, and verified completion. |

## Recovery

- [ ] Stop restores local controller input.
- [ ] Agent disconnect releases pressed state.
- [ ] Controller reconnects after agent restarts.
- [ ] Bulk transfer reconnects or fails clearly after agent restart.
- [ ] GUI logs explain permission errors.
- [ ] GUI logs explain likely firewall issues when the control TCP port is blocked.
- [ ] GUI logs explain likely firewall issues when the bulk transfer port is blocked.
