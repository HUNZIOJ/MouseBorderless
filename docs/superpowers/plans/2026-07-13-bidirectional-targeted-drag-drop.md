# Bidirectional Targeted Drag-Drop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver reliable bidirectional Windows file/folder drag-drop where pointer and button state cross the shared edge first, and verified files are copied into the exact directory selected on mouse release.

**Architecture:** Use one platform-independent `DragDropSession` state machine for both directions. The target endpoint resolves and stores its own filesystem path behind a one-time authorization token; the bulk TCP manifest carries only that token, and the source reports success only after the target verifies and atomically places every file.

**Tech Stack:** Rust 2021, Tokio TCP, bincode protocol v2, Windows OLE/COM/Shell APIs via `windows` 0.58, BLAKE3, Slint status projection.

**Depends on:** Completion of both `2026-07-13-tcp-only-network.md` and `2026-07-13-slint-control-desk.md`.

**Produces:** Completed two-machine drag-drop plus updated release artifacts.

---

## File Map

- `crates/borderless-core/src/drag_drop.rs`: final direction-aware state machine and target metadata.
- `crates/borderless-core/src/protocol.rs`: protocol v2 layout/drag messages and tests.
- `crates/borderless-core/src/file_transfer.rs`: cache vs authorized-drop destination enum.
- `crates/borderless-net/src/drop_authorization.rs`: one-time local target registry.
- `crates/borderless-net/src/bulk_transfer.rs`: consume authorization, direct placement, cleanup, and tests.
- `crates/borderless-net/src/lib.rs`: export target authorization.
- `crates/borderless-win/src/drag_drop.rs`: edge `CF_HDROP` capture and OLE lifecycle.
- `crates/borderless-win/src/drop_resolver.rs`: desktop/current-folder/folder-icon resolution.
- `crates/borderless-win/src/hooks.rs`: filter Borderless-injected release events.
- `crates/borderless-win/src/inject.rs`: tagged local left-button release.
- `crates/borderless-win/Cargo.toml`: final Shell/COM feature list.
- `crates/borderless-app/src/runtime.rs`: integrate the extracted coordinator and remove duplicate controllers.
- `crates/borderless-app/src/runtime/drag_drop.rs`: symmetric coordinator, timers, layout, messages, and tests.
- `crates/borderless-app/src/status.rs`: stable drag status and transfer result.
- `crates/borderless-app/src/ui_model.rs`: Chinese drag status projection.
- `crates/borderless-app/ui/main.slint`: drag destination/progress/error display.
- `README.md`: exact supported behavior and limits.
- `tests/manual/windows-two-machine-checklist.md`: bidirectional acceptance matrix.
- `old_drag_drop_reference.rs`: delete after tests cover the final behavior.

---

### Task 1: Finalize the Core State Machine and Protocol v2

**Files:**
- Modify: `crates/borderless-core/src/drag_drop.rs`
- Modify: `crates/borderless-core/src/protocol.rs`
- Modify: `crates/borderless-core/src/geometry.rs`
- Test: `crates/borderless-core/src/drag_drop.rs`
- Test: `crates/borderless-core/src/protocol.rs`

- [ ] **Step 1: Write direction and terminal-result tests**

Replace prototype-only tests with:

```rust
#[test]
fn both_directions_follow_the_same_happy_path() {
    for direction in [
        DragDirection::ControllerToAgent,
        DragDirection::AgentToController,
    ] {
        let mut session = DragDropSession::new(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            direction,
            vec!["C:\\src\\report.pdf".to_string()],
        );
        session.begin_remote_target_selection().unwrap();
        session.release_at(Point::new(200, 300)).unwrap();
        session
            .resolve_target(DropTargetSummary {
                display_name: "Desktop".to_string(),
                resolution_kind: DropResolutionKind::Desktop,
                authorization: Uuid::from_u128(3),
            })
            .unwrap();
        session.begin_transfer().unwrap();
        session.complete().unwrap();
        assert_eq!(session.phase(), &DragDropPhase::Completed);
    }
}

#[test]
fn sent_bytes_do_not_complete_the_drag_session() {
    let mut session = test_session(DragDirection::ControllerToAgent);
    session.begin_remote_target_selection().unwrap();
    session.release_at(Point::new(10, 20)).unwrap();
    session.resolve_target(test_target()).unwrap();
    session.begin_transfer().unwrap();

    assert_eq!(session.phase(), &DragDropPhase::Transferring);
    assert!(!session.is_terminal());
}

#[test]
fn opposite_edge_is_symmetric() {
    assert_eq!(Edge::Left.opposite(), Edge::Right);
    assert_eq!(Edge::Right.opposite(), Edge::Left);
    assert_eq!(Edge::Top.opposite(), Edge::Bottom);
    assert_eq!(Edge::Bottom.opposite(), Edge::Top);
}
```

- [ ] **Step 2: Run tests to verify the missing types**

Run:

```powershell
cargo test -p borderless-core drag_drop::tests
```

Expected: FAIL because direction, authorization target summary, and `Edge::opposite` do not exist.

- [ ] **Step 3: Implement the final core types**

Use these public shapes:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragDirection {
    ControllerToAgent,
    AgentToController,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropResolutionKind {
    Desktop,
    ExplorerDirectory,
    FolderIcon,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropTargetSummary {
    pub display_name: String,
    pub resolution_kind: DropResolutionKind,
    pub authorization: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragDropPhase {
    LocalDragDetected,
    RemoteTargetSelecting,
    RemoteDropReleased { release_point: Point },
    RemoteTargetResolved { target: DropTargetSummary },
    Transferring,
    Completed,
    Cancelled { reason: String },
    Failed { reason: String },
}
```

`DragDropSession` stores `direction`, and its transition methods remain the only way to change phase. `complete` is legal only from `Transferring`; `cancel` and `fail` reject an already terminal session.

Add to `geometry.rs`:

```rust
impl Edge {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
}
```

- [ ] **Step 4: Replace drag protocol messages and bump the version**

Set:

```rust
pub const PROTOCOL_VERSION: u16 = 2;
```

Use these wire variants, keeping message type IDs `1..=10` stable for existing non-drag messages and assigning `11..=18` in this order:

```rust
PeerLayout {
    controller_remote_position: crate::config::RemotePosition,
},
DragDropEntered {
    session_id: Uuid,
    transfer_id: Uuid,
    item_count: u32,
},
DragDropReleased {
    session_id: Uuid,
    point: crate::geometry::Point,
},
DragDropTargetResolved {
    session_id: Uuid,
    target: crate::drag_drop::DropTargetSummary,
},
DragDropTargetFailed {
    session_id: Uuid,
    reason: String,
},
DragDropCancel {
    session_id: Uuid,
    reason: String,
},
DragDropTransferStarted {
    session_id: Uuid,
    transfer_id: Uuid,
},
DragDropTransferResult {
    session_id: Uuid,
    transfer_id: Uuid,
    ok: bool,
    reason: Option<String>,
},
```

Create explicit payload structs as the protocol already does; update `message_type`, `encode_message`, `decode_message`, and strict payload matching.

- [ ] **Step 5: Add round-trip and mismatch tests**

Test all eight messages in a table and assert message IDs `11..=18`. Add:

```rust
#[test]
fn old_protocol_header_is_rejected_before_payload_decode() {
    let mut frame = encode_frame(
        1,
        &WireMessage::Heartbeat(Heartbeat { sent_millis: 1 }),
    )
    .unwrap();
    frame[4..6].copy_from_slice(&1_u16.to_be_bytes());

    assert_eq!(
        decode_frame(&frame),
        Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: 1,
        })
    );
}
```

- [ ] **Step 6: Run core tests and commit**

Run:

```powershell
cargo test -p borderless-core
```

Expected: PASS.

```powershell
git add crates/borderless-core/src/drag_drop.rs crates/borderless-core/src/geometry.rs crates/borderless-core/src/protocol.rs
git commit -m "feat: define bidirectional drag-drop protocol"
```

---

### Task 2: Add One-Time Local Destination Authorization

**Files:**
- Modify: `crates/borderless-core/src/file_transfer.rs`
- Create: `crates/borderless-net/src/drop_authorization.rs`
- Modify: `crates/borderless-net/src/lib.rs`
- Test: `crates/borderless-net/src/drop_authorization.rs`

- [ ] **Step 1: Write authorization registry tests**

Create the file with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_is_bound_to_session_transfer_and_consumed_once() {
        let registry = DropAuthorizationRegistry::default();
        let session_id = Uuid::from_u128(1);
        let transfer_id = Uuid::from_u128(2);
        let token = registry.register(
            session_id,
            transfer_id,
            PathBuf::from("C:\\Users\\demo\\Desktop"),
            Duration::from_secs(30),
        );

        assert!(registry
            .consume(session_id, transfer_id, token)
            .unwrap()
            .ends_with("Desktop"));
        assert!(registry.consume(session_id, transfer_id, token).is_err());
    }

    #[test]
    fn wrong_transfer_id_does_not_consume_authorization() {
        let registry = DropAuthorizationRegistry::default();
        let session_id = Uuid::from_u128(1);
        let transfer_id = Uuid::from_u128(2);
        let token = registry.register(
            session_id,
            transfer_id,
            PathBuf::from("C:\\drop"),
            Duration::from_secs(30),
        );

        assert!(registry
            .consume(session_id, Uuid::from_u128(99), token)
            .is_err());
        assert!(registry.consume(session_id, transfer_id, token).is_ok());
    }

    #[test]
    fn revoke_session_removes_all_authorizations() {
        let registry = DropAuthorizationRegistry::default();
        let session_id = Uuid::from_u128(1);
        let transfer_id = Uuid::from_u128(2);
        let token = registry.register(
            session_id,
            transfer_id,
            PathBuf::from("C:\\drop"),
            Duration::from_secs(30),
        );
        registry.revoke_session(session_id);
        assert!(registry.consume(session_id, transfer_id, token).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run:

```powershell
cargo test -p borderless-net drop_authorization::tests
```

Expected: FAIL because the module and registry are absent.

- [ ] **Step 3: Implement the registry**

Use:

```rust
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct DropAuthorizationRegistry {
    inner: Arc<Mutex<HashMap<(Uuid, Uuid), AuthorizedDrop>>>,
}

struct AuthorizedDrop {
    transfer_id: Uuid,
    destination_dir: PathBuf,
    expires_at: Instant,
}

impl DropAuthorizationRegistry {
    pub fn register(
        &self,
        session_id: Uuid,
        transfer_id: Uuid,
        destination_dir: PathBuf,
        ttl: Duration,
    ) -> Uuid {
        let token = Uuid::new_v4();
        self.inner.lock().expect("authorization registry poisoned").insert(
            (session_id, token),
            AuthorizedDrop {
                transfer_id,
                destination_dir,
                expires_at: Instant::now() + ttl,
            },
        );
        token
    }

    pub fn consume(
        &self,
        session_id: Uuid,
        transfer_id: Uuid,
        token: Uuid,
    ) -> anyhow::Result<PathBuf> {
        let mut entries = self.inner.lock().expect("authorization registry poisoned");
        let entry = entries
            .get(&(session_id, token))
            .context("drop destination authorization not found")?;
        if entry.transfer_id != transfer_id {
            bail!("drop destination transfer id mismatch");
        }
        if Instant::now() > entry.expires_at {
            entries.remove(&(session_id, token));
            bail!("drop destination authorization expired");
        }
        Ok(entries
            .remove(&(session_id, token))
            .expect("authorization disappeared")
            .destination_dir)
    }

    pub fn revoke_session(&self, session_id: Uuid) {
        self.inner
            .lock()
            .expect("authorization registry poisoned")
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    pub fn clear(&self) {
        self.inner.lock().expect("authorization registry poisoned").clear();
    }
}
```

- [ ] **Step 4: Replace absolute destinations in manifests**

In `file_transfer.rs` use:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileTransferDestination {
    IncomingCache,
    AuthorizedDrop {
        session_id: Uuid,
        authorization: Uuid,
    },
}

pub struct FileTransferManifest {
    pub transfer_id: Uuid,
    pub root_name: String,
    pub files: Vec<FileManifestEntry>,
    pub total_bytes: u64,
    pub destination: FileTransferDestination,
}
```

`manifest_from_source_paths` sets `FileTransferDestination::IncomingCache`. No wire struct may contain a target absolute path.

- [ ] **Step 5: Run tests and commit**

Run:

```powershell
cargo test -p borderless-net drop_authorization::tests
cargo test -p borderless-core
```

Expected: PASS after updating manifest literals in tests.

```powershell
git add crates/borderless-core/src/file_transfer.rs crates/borderless-core/src/protocol.rs crates/borderless-net/src/drop_authorization.rs crates/borderless-net/src/lib.rs crates/borderless-net/src/bulk_transfer.rs
git commit -m "feat: authorize local drop destinations"
```

---

### Task 3: Enforce Authorization and Cleanup in Bulk Transfer

**Files:**
- Modify: `crates/borderless-net/src/bulk_transfer.rs`
- Test: `crates/borderless-net/src/bulk_transfer.rs`

- [ ] **Step 1: Add authorized destination and rejection tests**

Replace tests that pass `destination_directory: Some(path)` with registry-backed tests:

```rust
#[tokio::test]
async fn authorized_drop_receives_files_directly() {
    let root = temp_dir("authorized_drop_receives_files_directly");
    let cache = root.join("cache");
    let destination = root.join("drop");
    fs::create_dir_all(&destination).unwrap();
    let registry = DropAuthorizationRegistry::default();
    let session_id = Uuid::new_v4();
    let transfer_id = Uuid::new_v4();
    let token = registry.register(
        session_id,
        transfer_id,
        destination.clone(),
        Duration::from_secs(30),
    );
    let manifest = FileTransferManifest {
        transfer_id,
        root_name: "note.txt".to_string(),
        files: vec![FileManifestEntry::file("note.txt", 4)],
        total_bytes: 4,
        destination: FileTransferDestination::AuthorizedDrop {
            session_id,
            authorization: token,
        },
    };

    let resolved = resolve_receive_destination(&cache, &registry, &manifest)
        .await
        .unwrap();
    assert_eq!(resolved, destination);
    assert!(registry.consume(session_id, transfer_id, token).is_err());
}

#[tokio::test]
async fn forged_or_missing_drop_authorization_is_rejected() {
    let manifest = FileTransferManifest {
        transfer_id: Uuid::from_u128(2),
        root_name: "note.txt".to_string(),
        files: vec![FileManifestEntry::file("note.txt", 4)],
        total_bytes: 4,
        destination: FileTransferDestination::AuthorizedDrop {
            session_id: Uuid::from_u128(1),
            authorization: Uuid::from_u128(99),
        },
    };
    let registry = DropAuthorizationRegistry::default();
    assert!(resolve_receive_destination(Path::new("cache"), &registry, &manifest)
        .await
        .is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```powershell
cargo test -p borderless-net authorized_drop_receives_files_directly
cargo test -p borderless-net forged_or_missing_drop_authorization_is_rejected
```

Expected: FAIL because bulk receive does not accept a registry.

- [ ] **Step 3: Thread the registry through both bulk endpoints**

Add `DropAuthorizationRegistry` to `run_bulk_transfer_server`, `run_bulk_transfer_client`, `run_connected_session`, and `read_incoming`. Resolve the directory with:

```rust
async fn resolve_receive_destination(
    cache_dir: &Path,
    registry: &DropAuthorizationRegistry,
    manifest: &FileTransferManifest,
) -> anyhow::Result<PathBuf> {
    match &manifest.destination {
        FileTransferDestination::IncomingCache => {
            tokio::fs::create_dir_all(cache_dir).await?;
            Ok(cache_dir.to_path_buf())
        }
        FileTransferDestination::AuthorizedDrop {
            session_id,
            authorization,
        } => {
            let destination = registry.consume(
                *session_id,
                manifest.transfer_id,
                *authorization,
            )?;
            validate_destination_dir(&destination).await?;
            Ok(destination)
        }
    }
}
```

`handle_manifest` uses only this returned local path. Remove all `destination_directory` code.

- [ ] **Step 4: Add deterministic cleanup for partial files**

Track temporary paths per transfer in `ReceiveTransfer`:

```rust
temp_paths: HashSet<PathBuf>,
```

Insert each `.borderless-part` path in `prepare_receive_file`, remove it after a successful rename, and add:

```rust
async fn cleanup_temp_paths(temp_paths: &HashSet<PathBuf>) {
    for path in temp_paths {
        if let Err(error) = tokio::fs::remove_file(path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(?error, path = %path.display(), "remove partial drop file");
            }
        }
    }
}
```

Call `cleanup_temp_paths(&transfer.temp_paths)` on cancel, read-loop error, hash failure, target disappearance, and endpoint stop. Completed final paths are never included in this set.

- [ ] **Step 5: Add a disconnect cleanup regression test**

Add:

```rust
#[tokio::test]
async fn disconnect_cleanup_removes_only_partial_files() {
    let root = temp_dir("disconnect_cleanup_removes_only_partial_files");
    fs::create_dir_all(&root).unwrap();
    let partial = root.join("report.pdf.borderless-part");
    let existing = root.join("existing.pdf");
    fs::write(&partial, b"partial").unwrap();
    fs::write(&existing, b"keep").unwrap();
    let paths = HashSet::from([partial.clone()]);

    cleanup_temp_paths(&paths).await;

    assert!(!partial.exists());
    assert_eq!(fs::read(&existing).unwrap(), b"keep");
    fs::remove_dir_all(root).unwrap();
}
```

- [ ] **Step 6: Run bulk tests and commit**

Run:

```powershell
cargo test -p borderless-net bulk_transfer::tests
```

Expected: PASS, including authorization, conflict rename, empty directory, cancel, and cleanup tests.

```powershell
git add crates/borderless-net/src/bulk_transfer.rs crates/borderless-net/src/drop_authorization.rs crates/borderless-core/src/file_transfer.rs
git commit -m "feat: write authorized drops atomically"
```

---

### Task 4: Stabilize OLE Edge Capture and Tagged Input Release

**Files:**
- Modify: `crates/borderless-win/src/drag_drop.rs`
- Modify: `crates/borderless-win/src/hooks.rs`
- Modify: `crates/borderless-win/src/inject.rs`
- Test: `crates/borderless-win/src/drag_drop.rs`
- Test: `crates/borderless-win/src/hooks.rs`
- Test: `crates/borderless-win/src/inject.rs`

- [ ] **Step 1: Add injected-release filtering tests**

Add a shared constant and test the predicate:

```rust
#[test]
fn borderless_injected_mouse_events_are_not_reemitted_to_runtime() {
    let data = MSLLHOOKSTRUCT {
        dwExtraInfo: BORDERLESS_INPUT_MARKER,
        ..Default::default()
    };
    assert!(is_borderless_injected_mouse_event(&data));
}
```

Add an event lifecycle test to `drag_drop.rs`:

```rust
#[test]
fn native_drag_emits_one_terminal_event() {
    let session_id = Uuid::from_u128(42);
    let active = Mutex::new(Some(session_id));

    assert_eq!(
        finish_local_drag(&active, LocalDragFinish::Released),
        Some(DragDropEvent::LocalDropReleased { session_id })
    );
    assert_eq!(finish_local_drag(&active, LocalDragFinish::Cancelled), None);
}
```

- [ ] **Step 2: Run focused tests to verify failure**

Run:

```powershell
cargo test -p borderless-win borderless_injected_mouse_events_are_not_reemitted_to_runtime
```

Expected: FAIL because the marker and predicate are absent.

- [ ] **Step 3: Tag local input created by Borderless**

In `inject.rs`:

```rust
pub const BORDERLESS_INPUT_MARKER: usize = 0x4244_524C;
```

Set `dwExtraInfo: BORDERLESS_INPUT_MARKER` on pointer parking and `release_local_left_button` mouse inputs. Do not tag remote `InputInjector` input if that endpoint also needs to observe ordinary injected target movement for Windows UI behavior; only the controller-side Hook predicate decides whether to emit a runtime event.

In `hooks.rs`:

```rust
fn is_borderless_injected_mouse_event(data: &MSLLHOOKSTRUCT) -> bool {
    data.dwExtraInfo == BORDERLESS_INPUT_MARKER
}
```

In `mouse_hook_proc`, pass tagged events to Windows but do not call `emit_mouse_events` for them. Physical events still emit and are suppressed during remote control.

- [ ] **Step 4: Make edge capture accept only valid `CF_HDROP` paths**

Before emitting `LocalFileDragEntered`, filter paths with `Path::exists`, reject an empty result with `DROPEFFECT_NONE`, and keep one active session in the OLE target mutex. Use one terminal helper from both `DragLeave` and `Drop`:

```rust
#[derive(Clone, Copy)]
enum LocalDragFinish {
    Released,
    Cancelled,
}

fn finish_local_drag(
    active: &Mutex<Option<Uuid>>,
    finish: LocalDragFinish,
) -> Option<DragDropEvent> {
    let session_id = active.lock().ok()?.take()?;
    Some(match finish {
        LocalDragFinish::Released => DragDropEvent::LocalDropReleased { session_id },
        LocalDragFinish::Cancelled => DragDropEvent::LocalDragCancelled { session_id },
    })
}
```

Send the returned event when present. This guarantees `DragLeave` and `Drop` cannot both terminate one session.

- [ ] **Step 5: Run Windows tests and commit**

Run:

```powershell
cargo test -p borderless-win
```

Expected: PASS; installing/uninstalling the edge window leaves no OLE registration behind.

```powershell
git add crates/borderless-win/src/drag_drop.rs crates/borderless-win/src/hooks.rs crates/borderless-win/src/inject.rs
git commit -m "fix: stabilize native drag handoff input"
```

---

### Task 5: Resolve Exact Desktop and Explorer Folder Targets

**Files:**
- Modify: `crates/borderless-win/src/drop_resolver.rs`
- Modify: `crates/borderless-win/Cargo.toml`
- Test: `crates/borderless-win/src/drop_resolver.rs`

- [ ] **Step 1: Extract and test resolution priority**

Define a pure candidate type and tests:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
enum DropCandidate {
    FolderIcon(PathBuf),
    ExplorerDirectory(PathBuf),
    Desktop(PathBuf),
}

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
        select_drop_candidate(vec![DropCandidate::ExplorerDirectory(PathBuf::from("C:\\Work"))])
            .unwrap(),
        DropCandidate::ExplorerDirectory(PathBuf::from("C:\\Work"))
    );
}

#[test]
fn unsupported_shell_location_returns_an_error() {
    assert!(select_drop_candidate(Vec::new()).is_err());
}
```

- [ ] **Step 2: Run tests to verify failure**

Run:

```powershell
cargo test -p borderless-win drop_resolver::tests
```

Expected: FAIL because candidate selection and folder hit testing are absent.

- [ ] **Step 3: Implement deterministic candidate selection**

```rust
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
```

Convert the winner to this local-only result. Only `summary()` crosses the network:

```rust
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

fn candidate_to_target(candidate: DropCandidate) -> ResolvedDropTarget {
    let (destination_dir, resolution_kind) = match candidate {
        DropCandidate::FolderIcon(path) => (path, DropResolutionKind::FolderIcon),
        DropCandidate::ExplorerDirectory(path) => {
            (path, DropResolutionKind::ExplorerDirectory)
        }
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
```

- [ ] **Step 4: Add Shell folder-icon hit testing**

For the root window under the screen point:

1. Match its `IShellBrowser` through `IShellWindows` and `SID_STopLevelBrowser`.
2. Query `IShellView`, cast to `IFolderView2`, and get its HWND.
3. Convert the release point with `ScreenToClient`.
4. Enumerate view items with `ItemCount`/`Item`; get each item rectangle with `IFolderView2::GetItemRect`.
5. For the first rectangle containing the client point, resolve the child to a filesystem path using the parent `IShellFolder` plus Shell item display name APIs.
6. Accept the hit only when `std::fs::metadata(path).is_dir()`.
7. If no directory icon is hit, resolve the view's current folder via `IPersistFolder2::GetCurFolder`.

For desktop windows (`Progman`/`WorkerW`), query the desktop shell view by `IShellWindows::FindWindowSW`, perform the same item rectangle check, then fall back to `FOLDERID_Desktop`.

Return an explicit error for Quick Access, This PC, search results, archive contents, and non-filesystem namespace items.

Keep COM details behind this concrete orchestration, so priority is testable without COM:

```rust
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
```

`ShellViewContext::folder_icon_at_screen_point` must enumerate item PIDLs and rectangles, convert the screen point to the view HWND client coordinates, and return only a path whose metadata is a directory. `current_filesystem_directory` must return `Ok(None)` when `SHGetPathFromIDListW` cannot produce a filesystem path; it must not turn that case into the desktop or cache directory.

- [ ] **Step 5: Verify Windows features and compile**

Ensure `crates/borderless-win/Cargo.toml` contains the features needed by the compiled APIs, including:

```toml
"Win32_UI_Shell",
"Win32_UI_Shell_Common",
"Win32_UI_WindowsAndMessaging",
"Win32_System_Com",
```

Run:

```powershell
cargo test -p borderless-win drop_resolver::tests
cargo check -p borderless-win
```

Expected: PASS.

- [ ] **Step 6: Commit**

```powershell
git add crates/borderless-win/src/drop_resolver.rs crates/borderless-win/Cargo.toml
git commit -m "feat: resolve exact Windows drop folders"
```

---

### Task 6: Extract a Symmetric Runtime Coordinator

**Files:**
- Create: `crates/borderless-app/src/runtime/drag_drop.rs`
- Modify: `crates/borderless-app/src/runtime.rs`
- Test: `crates/borderless-app/src/runtime/drag_drop.rs`

- [ ] **Step 1: Write race-order tests before extraction**

Use a pure coordinator harness with recorded effects:

```rust
#[test]
fn agent_entered_after_control_returns_is_immediately_handed_to_controller() {
    let mut coordinator = DragDropCoordinator::new(Role::Controller);
    coordinator.set_control_mode(ControlMode::Local);

    let effects = coordinator.handle_peer_message(WireMessage::DragDropEntered {
        session_id: Uuid::from_u128(1),
        transfer_id: Uuid::from_u128(2),
        item_count: 1,
    });

    assert!(effects.contains(&DragEffect::AwaitLocalRelease {
        session_id: Uuid::from_u128(1),
    }));
}

#[test]
fn control_returns_after_agent_entered_produces_the_same_effect() {
    let mut coordinator = DragDropCoordinator::new(Role::Controller);
    coordinator.set_control_mode(ControlMode::Remote);
    coordinator.handle_peer_message(WireMessage::DragDropEntered {
        session_id: Uuid::from_u128(1),
        transfer_id: Uuid::from_u128(2),
        item_count: 1,
    });

    let effects = coordinator.set_control_mode(ControlMode::Local);
    assert!(effects.contains(&DragEffect::AwaitLocalRelease {
        session_id: Uuid::from_u128(1),
    }));
}

#[test]
fn layout_installs_only_the_reciprocal_agent_edge() {
    let effects = DragDropCoordinator::new(Role::Agent).handle_peer_message(
        WireMessage::PeerLayout {
            controller_remote_position: RemotePosition::Right,
        },
    );
    assert!(effects.contains(&DragEffect::InstallEdge(Edge::Left)));
}
```

- [ ] **Step 2: Run tests to verify the new module is absent**

Run:

```powershell
cargo test -p borderless-app runtime::drag_drop::tests
```

Expected: FAIL because the coordinator/effect module does not exist.

- [ ] **Step 3: Define coordinator inputs and effects**

Create:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragEffect {
    InstallEdge(Edge),
    UninstallEdge,
    Send(WireMessage),
    AwaitLocalRelease { session_id: Uuid },
    ResolveTarget { session_id: Uuid, point: Point },
    BeginTransfer {
        session_id: Uuid,
        transfer_id: Uuid,
        authorization: Uuid,
    },
    CancelBulk { transfer_id: Uuid },
    RevokeAuthorization { session_id: Uuid },
    RestoreLocalInput,
    Status { state: String, destination: Option<String> },
}

pub struct DragDropCoordinator {
    role: Role,
    control_mode: ControlMode,
    peer_layout: Option<RemotePosition>,
    active: Option<DragDropSession>,
    peer_offer: Option<PeerDragOffer>,
    connection_generation: u64,
}
```

Methods accept local edge events, control mode changes, local left release, peer messages, bulk events, disconnect/stop, and timeout ticks. Methods return effects and perform no COM, network, filesystem, or UI calls.

- [ ] **Step 4: Implement layout and race-independent handoff**

Controller source direction is `ControllerToAgent`; agent source direction is `AgentToController`. On `PeerLayout`, install `edge_for_position(position)` on the controller and its `opposite()` on the agent. When a peer offer arrives, calculate handoff readiness from the current control mode immediately; do not wait only for a future mode-change event.

Reject a second offer while a session is active by returning:

```rust
DragEffect::Send(WireMessage::DragDropCancel {
    session_id,
    reason: "another drag-drop session is active".to_string(),
})
```

- [ ] **Step 5: Remove duplicate runtime coordinators**

Delete `ControllerDragDrop`, `AgentDragDrop`, `DragSourcePhase`, `DragSourceSession`, `IncomingDragSession`, and their helper branches from `runtime.rs`. Keep platform and async work in `runtime.rs`, but route every relevant event through one `DragDropCoordinator` instance per session.

- [ ] **Step 6: Run coordinator and app tests, then commit**

Run:

```powershell
cargo test -p borderless-app runtime::drag_drop::tests
cargo test -p borderless-app
```

Expected: PASS.

```powershell
git add crates/borderless-app/src/runtime.rs crates/borderless-app/src/runtime/drag_drop.rs
git commit -m "refactor: unify bidirectional drag coordination"
```

---

### Task 7: Connect Target Resolution, Transfer, and Final Acknowledgement

**Files:**
- Modify: `crates/borderless-app/src/runtime.rs`
- Modify: `crates/borderless-app/src/runtime/drag_drop.rs`
- Modify: `crates/borderless-app/src/status.rs`
- Modify: `crates/borderless-net/src/bulk_transfer.rs`
- Test: `crates/borderless-app/src/runtime/drag_drop.rs`

- [ ] **Step 1: Write final-result and forged-completion tests**

Add:

```rust
#[test]
fn sender_completes_only_after_matching_target_result() {
    let (mut coordinator, session_id, transfer_id) = transferring_source();

    assert!(coordinator
        .handle_bulk_event(BulkTransferEvent::Sent { transfer_id })
        .iter()
        .all(|effect| !matches!(effect, DragEffect::Status { state, .. } if state == "拖放完成")));

    let effects = coordinator.handle_peer_message(WireMessage::DragDropTransferResult {
        session_id,
        transfer_id,
        ok: true,
        reason: None,
    });
    assert!(effects.contains(&DragEffect::Status {
        state: "拖放完成".to_string(),
        destination: None,
    }));
}

#[test]
fn wrong_transfer_result_is_ignored() {
    let (mut coordinator, session_id, _) = transferring_source();
    let effects = coordinator.handle_peer_message(WireMessage::DragDropTransferResult {
        session_id,
        transfer_id: Uuid::from_u128(999),
        ok: true,
        reason: None,
    });
    assert!(effects.is_empty());
}
```

- [ ] **Step 2: Run tests to verify current source completes too early**

Run:

```powershell
cargo test -p borderless-app sender_completes_only_after_matching_target_result
```

Expected: FAIL because transfer purposes currently use bulk `Sent`/`Offered` heuristics rather than session acknowledgement.

- [ ] **Step 3: Register a target before replying**

When `DragEffect::ResolveTarget` is produced, run `resolve_drop_target(point)` in `spawn_blocking`. On success:

1. validate the local directory exists and is writable;
2. call `registry.register(session_id, transfer_id, destination, Duration::from_secs(30))`;
3. send `DragDropTargetResolved` with display name, kind, and returned token;
4. update target UI state to “目标目录已确认”.

On failure, send `DragDropTargetFailed`, revoke the session, and update both UI states to failure.

- [ ] **Step 4: Build an authorized manifest at the source**

After `DragDropTargetResolved`, build the normal manifest, enforce `max_file_transfer_bytes`, then set:

```rust
manifest.destination = FileTransferDestination::AuthorizedDrop {
    session_id,
    authorization: target.authorization,
};
```

Send `DragDropTransferStarted` over control TCP before `BulkTransferCommand::SendFiles`.

- [ ] **Step 5: Send target results from bulk terminal events**

Maintain explicit maps:

```rust
source_drag_transfers: HashMap<Uuid, Uuid>, // transfer -> session
target_drag_transfers: HashMap<Uuid, Uuid>, // transfer -> session
```

On target `BulkTransferEvent::Completed`, send:

```rust
WireMessage::DragDropTransferResult {
    session_id,
    transfer_id,
    ok: true,
    reason: None,
}
```

On target `Failed` or `Cancelled`, send `ok: false` with a reason. Source `Sent` means only “bytes submitted”; it leaves the coordinator in `Transferring`.

- [ ] **Step 6: Run tests and commit**

Run:

```powershell
cargo test -p borderless-app runtime::drag_drop::tests
cargo test -p borderless-net bulk_transfer::tests
```

Expected: PASS.

```powershell
git add crates/borderless-app/src/runtime.rs crates/borderless-app/src/runtime/drag_drop.rs crates/borderless-app/src/status.rs crates/borderless-net/src/bulk_transfer.rs
git commit -m "feat: complete targeted drops after receiver ack"
```

---

### Task 8: Cancellation, Disconnect, Generation, and Timeout Recovery

**Files:**
- Modify: `crates/borderless-app/src/runtime/drag_drop.rs`
- Modify: `crates/borderless-app/src/runtime.rs`
- Modify: `crates/borderless-net/src/drop_authorization.rs`
- Test: `crates/borderless-app/src/runtime/drag_drop.rs`

- [ ] **Step 1: Add recovery tests**

```rust
#[test]
fn disconnect_cancels_drag_bulk_and_releases_input() {
    let (mut coordinator, _session_id, transfer_id) = transferring_source();
    let effects = coordinator.on_disconnect(7);
    assert!(effects.contains(&DragEffect::CancelBulk { transfer_id }));
    assert!(effects.contains(&DragEffect::Send(WireMessage::ReleaseAll)));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        DragEffect::Status { state, .. } if state.contains("连接中断")
    )));
    assert!(coordinator.active_session().is_none());
}

#[test]
fn stale_connection_generation_messages_are_ignored() {
    let mut coordinator = DragDropCoordinator::new(Role::Controller);
    coordinator.begin_connection_generation(8);
    assert!(coordinator
        .handle_tagged_peer_message(7, WireMessage::DragDropCancel {
            session_id: Uuid::new_v4(),
            reason: "old connection".to_string(),
        })
        .is_empty());
}

#[test]
fn target_authorization_timeout_fails_without_starting_bulk() {
    let (mut coordinator, _session_id) = awaiting_target_source();
    let effects = coordinator.tick(Instant::now() + Duration::from_secs(31));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        DragEffect::Status { state, .. } if state.contains("超时")
    )));
    assert!(effects.iter().all(|effect| !matches!(effect, DragEffect::BeginTransfer { .. })));
}
```

- [ ] **Step 2: Run recovery tests to verify failure**

Run:

```powershell
cargo test -p borderless-app runtime::drag_drop::tests::disconnect_cancels_drag_bulk_and_releases_input
cargo test -p borderless-app runtime::drag_drop::tests::stale_connection_generation_messages_are_ignored
```

Expected: FAIL because generation and complete recovery effects are not implemented.

- [ ] **Step 3: Implement explicit recovery**

Use one terminal path for stop, disconnect, transport error, peer cancel, and timeout:

```rust
fn terminate_active(&mut self, reason: String, failed: bool) -> Vec<DragEffect> {
    let Some(mut session) = self.active.take() else {
        return vec![DragEffect::RestoreLocalInput];
    };
    let session_id = session.session_id();
    let transfer_id = session.transfer_id();
    if failed {
        let _ = session.fail(reason.clone());
    } else {
        let _ = session.cancel(reason.clone());
    }
    self.peer_offer = None;

    vec![
        DragEffect::CancelBulk { transfer_id },
        DragEffect::RevokeAuthorization { session_id },
        DragEffect::Send(WireMessage::ReleaseAll),
        DragEffect::RestoreLocalInput,
        DragEffect::Status {
            state: if failed {
                format!("拖放失败：{reason}")
            } else {
                format!("拖放已取消：{reason}")
            },
            destination: None,
        },
    ]
}

pub fn on_disconnect(&mut self, generation: u64) -> Vec<DragEffect> {
    if generation != self.connection_generation {
        return Vec::new();
    }
    let mut effects = self.terminate_active("连接中断".to_string(), true);
    effects.push(DragEffect::UninstallEdge);
    effects
}
```

The runtime maps `RestoreLocalInput` to Hook pass-through plus local pointer/input cleanup, `RevokeAuthorization` to the registry, and `CancelBulk` to every active bulk command sender. Stop adds `UninstallEdge`; a target failure sends `DragDropTransferResult` before terminating.

Tag incoming connection events with the runtime session generation already used by `TaggedSessionUpdate`. The coordinator ignores a different generation before matching session IDs.

- [ ] **Step 4: Expire registry entries**

Add this registry method and run it from the existing drag poll/timer tick:

```rust
pub fn purge_expired(&self, now: Instant) -> Vec<Uuid> {
    let mut entries = self.inner.lock().expect("authorization registry poisoned");
    let expired = entries
        .iter()
        .filter_map(|(&(session_id, token), entry)| {
            (now > entry.expires_at).then_some((session_id, token))
        })
        .collect::<Vec<_>>();
    let mut sessions = Vec::new();
    for (session_id, token) in expired {
        entries.remove(&(session_id, token));
        if !sessions.contains(&session_id) {
            sessions.push(session_id);
        }
    }
    sessions
}
```

For every returned session ID, the target coordinator emits a timeout status and `DragDropTargetFailed` if the peer is still connected.

- [ ] **Step 5: Run app and network tests, then commit**

Run:

```powershell
cargo test -p borderless-app runtime::drag_drop::tests
cargo test -p borderless-net
```

Expected: PASS.

```powershell
git add crates/borderless-app/src/runtime.rs crates/borderless-app/src/runtime/drag_drop.rs crates/borderless-net/src/drop_authorization.rs crates/borderless-net/src/bulk_transfer.rs
git commit -m "fix: recover drag sessions on cancel and disconnect"
```

---

### Task 9: Slint Status, Documentation, and Final Verification

**Files:**
- Modify: `crates/borderless-app/src/status.rs`
- Modify: `crates/borderless-app/src/ui_model.rs`
- Modify: `crates/borderless-app/src/ui_bridge.rs`
- Modify: `crates/borderless-app/ui/main.slint`
- Modify: `README.md`
- Modify: `config.example.toml`
- Modify: `tests/manual/windows-two-machine-checklist.md`
- Delete: `old_drag_drop_reference.rs`

- [ ] **Step 1: Add UI projection tests for every drag terminal state**

```rust
#[test]
fn drag_status_projects_destination_and_failure() {
    let status = AppStatus {
        drag_drop_state: Some("拖放失败：目标目录不可写".to_string()),
        drag_drop_destination: Some("Desktop\\Reports".to_string()),
        ..AppStatus::default()
    };
    let view = UiSnapshot::from_status(&status);
    assert_eq!(view.drag_state, "拖放失败：目标目录不可写");
    assert_eq!(view.transfer_destination, "Desktop\\Reports");
}
```

Extend `UiSnapshot` with `drag_state` (the destination field already exists from the Slint phase), add `in property <string> drag-state: ""` to `AppWindow`, and bind it in `apply_status_to_window`:

```rust
window.set_drag_state(view.drag_state.into());
```

- [ ] **Step 2: Show the confirmed drag lifecycle in Slint**

The transfer band must display these stable states without changing its dimensions:

```text
文件已到达共享边缘
正在选择目标目录
正在解析目标目录
正在传输到目标
拖放完成
拖放已取消
拖放失败：<原因>
```

Replace the fixed transfer band visibility and contents with this stable-height form. Before bytes begin it shows the drag lifecycle and destination without a progress bar or Cancel button; during transfer it shows file, bytes, progress, destination, and Cancel:

```slint
Rectangle {
    visible: root.transfer-active || root.drag-state != "";
    height: 104px;
    background: white;
    border-width: 1px;
    border-color: #d7dddf;
    VerticalLayout {
        padding: 14px;
        HorizontalLayout {
            alignment: space-between;
            Text {
                text: root.transfer-active ? root.transfer-file : root.drag-state;
                font-weight: 700;
            }
            Button {
                visible: root.transfer-active;
                text: "取消";
                clicked => { root.cancel-transfer-requested(); }
            }
        }
        if root.transfer-active : ProgressIndicator {
            progress: root.transfer-progress;
        }
        Text {
            text: root.transfer-active
                ? root.transfer-detail + " · " + root.transfer-destination
                : root.transfer-destination;
            color: #68777e;
        }
    }
}
```

- [ ] **Step 3: Update documentation and manual acceptance**

README must state:

- bidirectional drag-drop;
- files transfer only after release;
- supported desktop, Explorer current directory, and folder-icon targets;
- always-copy behavior;
- conflict auto-rename;
- unsupported Shell/application targets fail clearly;
- control and bulk TCP firewall ports.

Expand the manual checklist into separate controller-to-agent and agent-to-controller rows for:

```text
single file to desktop
multiple files to Explorer current directory
folder to folder icon
same-name auto-rename
cancel before release
disconnect during transfer
unwritable destination
large file progress
```

- [ ] **Step 4: Delete the obsolete reference after comparison**

Verify every behavior still needed from `old_drag_drop_reference.rs` is represented by a final module or test, then delete it:

```powershell
Remove-Item -LiteralPath old_drag_drop_reference.rs
```

- [ ] **Step 5: Run the automated final gate**

Run:

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p borderless-app
```

Expected: all commands exit `0`.

- [ ] **Step 6: Perform two-machine Windows acceptance**

On two Windows 10/11 computers at the same privilege level, complete every row in `tests/manual/windows-two-machine-checklist.md`. Verify both directions, exact folder-icon resolution, no source deletion, no overwrite, cleanup after disconnect, and clear UI errors.

Expected: checklist contains actual pass/fail notes and any environment details; do not mark unexecuted items as passed.

- [ ] **Step 7: Build and inspect the portable release**

Run the README packaging commands to create a fresh `dist/Borderless-windows-x64.zip`. Inspect the archive and verify it contains:

```text
borderless.exe
config.example.toml
README.md
```

Run the packaged executable on both machines before declaring release readiness.

- [ ] **Step 8: Commit**

```powershell
git add crates/borderless-app README.md config.example.toml tests/manual/windows-two-machine-checklist.md
git commit -m "feat: complete bidirectional targeted drag-drop"
```

---

## Final Acceptance

- Both directions use the same state machine and pass race-order tests.
- No file bytes transfer before target release and authorization.
- Target absolute paths never cross back in a writable manifest field.
- Desktop, Explorer current directory, and filesystem folder icons resolve correctly.
- Files always copy, conflicts auto-rename, and source files remain unchanged.
- Source completes only after target verification acknowledgement.
- Cancel, timeout, stop, and disconnect restore input and remove partial files.
- Slint UI shows Chinese lifecycle, progress, destination, completion, and failure.
- Workspace format, tests, strict lint, release build, package inspection, and two-machine checklist pass.
