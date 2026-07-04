# Controller-to-Agent Targeted Drag/Drop Design

Date: 2026-07-04

## Goal

Refactor drag/drop so a user can drag files from the controller machine into the agent machine, move the pointer to a destination on the agent side, release there, and only then start file transfer. The transferred files should land in the destination implied by the release location.

The first implementation pass supports only `controller -> agent`.

## Scope

This design targets standard Windows file-drop destinations:

- Desktop
- Explorer folder views
- Explorer windows where the release point cannot be resolved to a more specific drop target, in which case the current Explorer directory is used

This design does not attempt to support arbitrary application-specific drag/drop contracts.

## Non-Goals

- `agent -> controller` reverse drag/drop in this pass
- Continuing the original Windows drag session across machines
- Supporting non-`CF_HDROP` destinations
- Falling back to the incoming cache directory when the release destination cannot be resolved

## User Experience

### Primary Flow

1. The user starts a normal file drag on the controller machine.
2. The drag enters the configured shared edge handoff zone.
3. Borderless creates a drag/drop session but does not start file transfer.
4. The pointer crosses into the agent side and continues moving there for target selection only.
5. The user releases the mouse on the agent side.
6. The agent resolves the release location into a destination directory:
   - preferred: the concrete desktop or Explorer target implied by the release point
   - fallback: the current Explorer window directory
7. If destination resolution succeeds, the controller starts preparing and sending files.
8. The agent writes the transferred files directly into the resolved destination directory.
9. Progress and failures are shown in the GUI.

### Cancellation

- If the user cancels before release, no file transfer starts.
- If the connection fails before release, the session ends with no transferred files.
- If the destination cannot be resolved after release, the operation fails with a clear error and does not silently drop files into cache.

## Design Summary

The current drag/drop path mixes three concerns:

- local edge drag detection on the controller
- remote pointer control during drag
- agent-side remote drag continuation using `DoDragDrop`

The refactor changes the model from "continue a remote drag session" to "select a remote destination, then perform a targeted file copy after release."

The new model separates:

- pointer handoff and target selection
- destination resolution on the agent
- file transfer into a final directory

This keeps the controller drag interaction responsive and avoids requiring the user to hold the mouse button while data transfers.

## Architecture

### 1. Core Domain

Restore and redefine `crates/borderless-core/src/drag_drop.rs` around targeted transfer semantics.

Recommended session states:

- `LocalDragDetected`
- `RemoteTargetSelecting`
- `RemoteDropReleased`
- `RemoteTargetResolved`
- `Transferring`
- `Completed`
- `Cancelled`
- `Failed`

`DragDropSession` should continue to carry stable IDs for the drag session and the eventual transfer, but it should no longer imply that the agent will start a second native drag session.

Add a target payload type to represent the resolved agent destination. This should carry resolved drop semantics rather than raw coordinates. A good first shape is:

- destination directory absolute path
- resolution kind: desktop hit, explorer target hit, or explorer-window fallback

### 2. Windows Integration

#### Controller-side edge drag detection

Keep the edge drop target in `borderless-win`, but narrow its responsibility:

- detect local `CF_HDROP` drags entering the shared edge zone
- capture source paths
- emit session lifecycle events for enter and cancel

Release while selecting a remote target should be coordinated by the controller runtime from the local mouse-button-up event plus the last known remote pointer position, rather than by continuing a second native drag session on the agent.

It should not attempt to own the agent-side continuation drag behavior.

#### Agent-side drop target resolver

Add a Windows-specific resolver responsible for converting an agent-side release point into a destination directory.

Responsibilities:

- resolve desktop drops
- resolve Explorer view/window drops
- if a specific target is unavailable, fall back to the current Explorer window directory
- return an explicit failure when no safe destination can be resolved

This resolver should be isolated from transfer logic so it can be tested independently.

### 3. App Runtime

Split the runtime drag/drop orchestration conceptually into:

- `ControllerDragDropCoordinator`
- `AgentDropTargetCoordinator`

These can begin as focused structs inside `runtime.rs`, but the design goal is to remove the current monolithic state machine feel and create explicit ownership boundaries.

#### Controller coordinator responsibilities

- track local drag handoff sessions
- enter remote target selection mode once the pointer crosses the edge
- detect local left-button release while the pointer is selecting a target on the agent side
- interpret that release against the last known remote pointer position
- wait for the agent to resolve a destination
- only then prepare the manifest and start bulk transfer
- cancel cleanly on local cancel, remote cancel, or connection failure

#### Agent coordinator responsibilities

- receive remote release coordinates
- resolve target directory
- report target resolution success or failure
- receive the drag transfer request
- write files directly into the resolved destination directory
- surface progress and errors

### 4. Bulk Transfer

Reuse the existing transfer channel, but extend drag/drop transfer so the agent writes directly to the resolved destination directory instead of always landing in the incoming cache and then using a second-stage drag.

This requires drag/drop transfers to carry destination metadata into the receive path.

The receive side should still use temporary partial files and only rename into final paths when each file is complete and verified.

## Protocol Changes

The current `DragDropStart / DragDropCommit / DragDropCancel` protocol is centered on remote drag continuation. Replace or redefine it around targeted drop resolution.

Recommended message flow:

- `DragDropEntered`
  - controller reports a new local edge drag session
  - no transfer starts

- `DragDropReleased { session_id, point }`
  - controller reports the agent-side release point derived from the last remote pointer position when local left-button-up occurs

- `DragDropTargetResolved { session_id, target }`
  - agent resolved a destination directory

- `DragDropTargetFailed { session_id, reason }`
  - agent could not resolve a destination

- `DragDropTransferStart { session, manifest, target }`
  - controller now starts the targeted file transfer

- `DragDropCancel { session_id }`
  - either side cancels the pending or active drag/drop transfer

If keeping old enum names would increase confusion, prefer introducing new message names rather than overloading existing semantics.

The protocol should carry resolved destination semantics, not just coordinates, once target resolution is complete.

## Runtime State Flow

### Controller

1. Local file drag enters edge target.
2. Session enters `LocalDragDetected`.
3. Pointer crosses to remote; session enters `RemoteTargetSelecting`.
4. User releases on agent side.
5. Controller sends `DragDropReleased`.
6. Agent replies with resolved target or failure.
7. On success, controller:
   - validates size limits
   - builds manifest
   - starts targeted transfer
8. On completion, controller marks the session completed.

### Agent

1. Receives remote pointer updates while controller drag is selecting a target.
2. Receives `DragDropReleased`.
3. Resolves the release point into a directory.
4. Replies with `DragDropTargetResolved` or `DragDropTargetFailed`.
5. Receives targeted transfer start.
6. Writes files into the resolved destination directory.
7. Emits progress and completion/failure events.

## Destination Resolution Rules

Resolution order:

1. Explicit desktop hit
2. Explicit Explorer target/folder view hit
3. Current Explorer window directory fallback
4. Fail with clear UI error

Rules:

- Do not silently fall back to the incoming cache directory.
- If a resolved directory is unavailable or no longer exists when transfer begins, fail explicitly.
- If a resolved target is not writable, fail explicitly.

## File Placement Rules

- Drag/drop transfers write directly to the resolved target directory.
- Name conflict handling uses the existing rename-conflict strategy, but now applies in the final destination directory.
- Temporary partial files remain temporary until validation passes.
- No completed file should appear under its final name before the write and verification path has finished.

## Error Handling

### Before release

- Cancel, disconnect, or edge exit ends the session without starting transfer.
- No cache files are created.

### After release, before resolution

- If the agent cannot resolve a target, the session fails with a clear reason.
- No transfer starts.

### During transfer

- Network failure cancels the transfer and preserves only temporary partial files.
- Validation failure removes failed partial outputs and reports the reason.
- Target directory disappearance or permission failure aborts the transfer and reports the reason.

### Recovery

- Controller returns to normal local control once the release has committed target selection.
- Agent clears transient target-selection state after success, failure, or cancellation.
- A stale session message from a disconnected session should be ignored.

## UI and Status

The GUI should expose the drag/drop lifecycle in terms that match the new behavior:

- selecting remote target
- resolving destination
- transferring to target directory
- completed
- cancelled
- failed

Useful status fields:

- active drag/drop session ID
- resolved destination summary
- current transfer file
- bytes done / total
- failure reason

The UI should no longer imply that the agent is continuing an active native drag session.

## Testing

### Unit tests

- drag/drop session state transitions
- protocol message encode/decode for new drag/drop messages
- controller release-before-transfer flow
- agent target resolution fallback order
- direct-to-destination transfer path
- rename conflict handling in final destination
- cancellation before release
- cancellation after release but before transfer
- failure to resolve destination
- failure when target directory is missing or not writable

### Integration-style tests

- controller release on desktop resolves to desktop directory
- controller release on Explorer resolves to current folder
- invalid release point falls back to Explorer window directory
- invalid release point with no Explorer fallback fails clearly
- disconnect before release does not create files
- disconnect during transfer only leaves temporary partials

### Manual validation

- drag from controller Explorer into agent desktop
- drag from controller Explorer into an agent Explorer window
- release over a non-target area inside Explorer and verify fallback to the current Explorer directory
- large file progress
- cancellation during target selection
- cancellation during transfer
- target permission failure

## Migration Plan

1. Restore core drag/drop domain module with new targeted-transfer semantics.
2. Reintroduce protocol support for drag/drop messages using the new flow.
3. Rework controller runtime from "remote drag continuation" to "remote target selection."
4. Remove the agent-side `DoDragDrop` dependency path from the runtime.
5. Add destination-resolution support on the agent.
6. Extend bulk transfer to accept drag/drop destination directories.
7. Update UI status and logs to match the new semantics.
8. Remove obsolete remote-drag continuation code and tests.

## Open Design Choices Settled For This Pass

- First pass supports `controller -> agent` only.
- No transfer starts until the user releases on the agent side.
- If the exact release target is unavailable, fall back to the current Explorer directory.
- If destination resolution still fails, abort with a clear error.
- Standard `CF_HDROP` desktop and Explorer targets are the only supported destinations in this pass.
