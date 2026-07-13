# TCP-Only Network Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove KCP and all UDP pointer paths while preserving low-latency LAN keyboard, mouse, clipboard, reconnect, and bulk file transfer over TCP.

**Architecture:** Introduce a small TCP-only connection settings type, migrate both connection endpoints and the app runtime to it, then remove the old multi-transport API and configuration fields. Keep the control TCP channel separate from the existing bulk TCP channel, and retain latest-move coalescing so pointer traffic cannot build an unbounded reliable queue.

**Tech Stack:** Rust 2021, Tokio TCP, bincode-framed control protocol, TOML/Serde configuration, existing eframe UI for this phase only.

**Depends on:** `docs/superpowers/specs/2026-07-13-tcp-slint-targeted-drag-drop-design.md`

**Produces:** A green TCP-only baseline. Run the Slint plan next.

---

## File Map

- `Cargo.toml`: remove the workspace KCP dependency after callers migrate.
- `Cargo.lock`: regenerate without `kcp-core` and `kcp-tokio`.
- `config.example.toml`: remove transport mode and pointer UDP fields.
- `README.md`: describe TCP-only setup and firewall requirements.
- `crates/borderless-core/src/config.rs`: final TCP-only configuration and legacy TOML migration tests.
- `crates/borderless-net/src/transport.rs`: TCP-only settings, commands, and events.
- `crates/borderless-net/src/controller_client.rs`: TCP connect/reconnect loop.
- `crates/borderless-net/src/agent_server.rs`: TCP accept/reconnect loop.
- `crates/borderless-net/src/lib.rs`: stop exporting deleted KCP/UDP modules.
- `crates/borderless-net/src/kcp_transport.rs`: delete.
- `crates/borderless-net/src/latest_pointer.rs`: delete.
- `crates/borderless-net/Cargo.toml`: remove `kcp-tokio`.
- `crates/borderless-app/src/runtime.rs`: TCP-only settings, pointer buffering, status handling, and tests.
- `crates/borderless-app/src/status.rs`: remove KCP/UDP metrics.
- `crates/borderless-app/src/app.rs`: remove transport selection and UDP UI while eframe still owns the window.
- `tests/manual/windows-two-machine-checklist.md`: remove KCP/UDP acceptance items.
- `crates/borderless-win/Cargo.toml`: enable the missing Shell Common feature required by the current drag prototype baseline.

---

### Task 0: Preserve and Stabilize the Current Dirty Worktree

**Files:**
- Modify: `crates/borderless-win/Cargo.toml`
- Modify: `crates/borderless-net/src/bulk_transfer.rs`
- Format: all currently modified Rust files
- Preserve untracked reference: `old_drag_drop_reference.rs`

- [ ] **Step 1: Create the implementation branch without discarding current changes**

Run:

```powershell
git switch -c codex/tcp-slint-drag-drop
```

Expected: branch `codex/tcp-slint-drag-drop` is created and all current modified/untracked files remain present.

- [ ] **Step 2: Reproduce the current baseline failures**

Run:

```powershell
cargo fmt --all -- --check
cargo test --workspace
```

Expected: formatting fails in the current drag prototype; workspace compilation reports missing `Win32_UI_Shell_Common` APIs. After compilation is unblocked, the missing-destination test currently fails while cleaning a directory that was never created.

- [ ] **Step 3: Make only the baseline fixes**

Add the missing Windows feature in `crates/borderless-win/Cargo.toml`:

```toml
windows = { workspace = true, features = [
    "implement",
    "Win32_Storage_FileSystem",
    "Win32_System_Com",
    "Win32_System_Com_StructuredStorage",
    "Win32_System_DataExchange",
    "Win32_Foundation",
    "Win32_Graphics_Gdi",
    "Win32_System_LibraryLoader",
    "Win32_System_Memory",
    "Win32_System_Ole",
    "Win32_System_SystemServices",
    "Win32_System_Threading",
    "Win32_UI_HiDpi",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_UI_Shell",
    "Win32_UI_Shell_Common",
    "Win32_UI_WindowsAndMessaging",
] }
```

Replace the unconditional cleanup at the end of `missing_destination_directory_fails_transfer_without_killing_session`:

```rust
if root.exists() {
    fs::remove_dir_all(root).unwrap();
}
```

Format the workspace:

```powershell
cargo fmt --all
```

- [ ] **Step 4: Verify the stabilized baseline**

Run:

```powershell
cargo fmt --all -- --check
cargo test --workspace
```

Expected: both commands exit `0`; all workspace tests pass.

- [ ] **Step 5: Commit the recoverable prototype checkpoint**

Stage only product, docs, configuration, and manual checklist files. Do not add `old_drag_drop_reference.rs`.

```powershell
git add README.md config.example.toml crates tests/manual/windows-two-machine-checklist.md
git commit -m "chore: checkpoint targeted drag-drop prototype"
```

Expected: the current prototype becomes recoverable on the feature branch; `old_drag_drop_reference.rs` remains untracked for later comparison and deletion.

---

### Task 1: Introduce a TCP-Only Connection Contract

**Files:**
- Modify: `crates/borderless-net/src/transport.rs`
- Test: `crates/borderless-net/src/transport.rs`

- [ ] **Step 1: Add failing tests for the new contract**

Add these tests alongside the existing transport tests:

```rust
#[test]
fn tcp_settings_format_ipv4_and_ipv6_addresses() {
    let ipv4 = TcpConnectionSettings {
        host: "127.0.0.1".to_string(),
        port: 24800,
    };
    let ipv6 = TcpConnectionSettings {
        host: "::1".to_string(),
        port: 24800,
    };

    assert_eq!(ipv4.peer_addr(), "127.0.0.1:24800");
    assert_eq!(ipv6.peer_addr(), "[::1]:24800");
    assert_eq!(
        ipv6.socket_addr().unwrap(),
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 24800)
    );
}

```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```powershell
cargo test -p borderless-net transport::tests::tcp_settings_format_ipv4_and_ipv6_addresses
```

Expected: FAIL to compile because `TcpConnectionSettings` does not exist.

- [ ] **Step 3: Add the TCP-only types without removing the legacy types yet**

Add to `transport.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpConnectionSettings {
    pub host: String,
    pub port: u16,
}

impl TcpConnectionSettings {
    pub fn peer_addr(&self) -> String {
        addr_string(&self.host, self.port)
    }

    pub fn socket_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.peer_addr().parse()?)
    }
}
```

Keep `ConnectionEvent`, `TransportSettings`, pointer commands, and UDP-only events unchanged until Tasks 2 and 3 migrate all callers. This keeps the workspace green after the additive contract commit.

- [ ] **Step 4: Run focused and workspace tests**

Run:

```powershell
cargo test -p borderless-net transport::tests
cargo test --workspace
```

Expected: PASS; no behavior has changed yet.

- [ ] **Step 5: Commit**

```powershell
git add crates/borderless-net/src/transport.rs
git commit -m "refactor: introduce TCP-only connection settings"
```

---

### Task 2: Rewrite Controller and Agent Endpoints Around TCP

**Files:**
- Modify: `crates/borderless-net/src/controller_client.rs`
- Modify: `crates/borderless-net/src/agent_server.rs`
- Modify: `crates/borderless-net/src/transport.rs`
- Modify: `crates/borderless-net/src/lib.rs`
- Modify: `crates/borderless-net/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/borderless-app/src/runtime.rs`
- Delete: `crates/borderless-net/src/kcp_transport.rs`
- Delete: `crates/borderless-net/src/latest_pointer.rs`
- Test: `crates/borderless-net/src/controller_client.rs`
- Test: `crates/borderless-net/src/agent_server.rs`

- [ ] **Step 1: Replace KCP tests with TCP-only behavior tests**

Keep the existing TCP connect/stop, idle timeout, reconnect delay, and error-message tests. Delete KCP and UDP tests. Add one explicit pointer-over-TCP test to `controller_client.rs`:

```rust
#[tokio::test]
async fn pointer_input_is_delivered_on_the_control_tcp_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut transport = TcpFramedTransport::new(stream).unwrap();
        transport.read_frame().await.unwrap().message
    });
    let client = tokio::spawn(run_controller_client(
        TcpConnectionSettings {
            host: "127.0.0.1".to_string(),
            port,
        },
        event_tx,
        command_rx,
    ));

    while !matches!(event_rx.recv().await, Some(ConnectionEvent::Connected { .. })) {}
    command_tx
        .send(ConnectionCommand::Send(WireMessage::Input(
            InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x: 400, y: 300 }),
        )))
        .unwrap();

    assert!(matches!(
        server.await.unwrap(),
        WireMessage::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x: 400, y: 300 }))
    ));
    command_tx.send(ConnectionCommand::Stop).unwrap();
    client.await.unwrap().unwrap();
}
```

- [ ] **Step 2: Run the focused test to verify the API mismatch**

Run:

```powershell
cargo test -p borderless-net controller_client::tests::pointer_input_is_delivered_on_the_control_tcp_connection
```

Expected: FAIL to compile because the endpoint still accepts `TransportSettings` and `SendReliable`.

- [ ] **Step 3: Collapse the connection command to TCP semantics**

Replace `ConnectionCommand` with:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionCommand {
    Send(WireMessage),
    Stop,
}
```

At the same time, remove the now-meaningless mode from connected events while retaining UDP diagnostic variants until Task 3:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Waiting,
    Connecting(String),
    Connected { peer: String },
    Disconnected(String),
    Message(WireMessage),
    LatestPointer { x: i32, y: i32, sequence: u64 },
    StalePointerPackets { count: u64 },
    Error(String),
}
```

Update `run_controller_client` to accept `TcpConnectionSettings` and remove its transport-mode match:

```rust
pub async fn run_controller_client(
    settings: TcpConnectionSettings,
    events: UnboundedSender<ConnectionEvent>,
    mut commands: UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    emit(&events, ConnectionEvent::Waiting);

    loop {
        let peer = settings.peer_addr();
        emit(&events, ConnectionEvent::Connecting(peer.clone()));

        let wait_after_disconnect = match connect_or_stop(
            TcpFramedTransport::connect(&peer),
            &mut commands,
        )
        .await
        {
            ConnectAttempt::Connected(transport) => {
                emit(&events, ConnectionEvent::Connected { peer: peer.clone() });
                if run_tcp_connection(transport, &events, &mut commands).await? {
                    return Ok(());
                }
                true
            }
            ConnectAttempt::Failed(error) => {
                emit(
                    &events,
                    ConnectionEvent::Error(connection_failure_message(&peer, &error)),
                );
                if wait_before_reconnect(&mut commands).await {
                    return Ok(());
                }
                false
            }
            ConnectAttempt::Stopped => return Ok(()),
        };

        emit(&events, ConnectionEvent::Disconnected(peer));
        if wait_after_disconnect && wait_before_reconnect(&mut commands).await {
            return Ok(());
        }
    }
}
```

In `run_tcp_connection`, handle only `Send` and `Stop`:

```rust
match command {
    Some(ConnectionCommand::Send(message)) => {
        if driver_tx.send(ReliableDriverCommand::Send(message)).is_err() {
            emit(events, ConnectionEvent::Error("TCP control channel closed".to_string()));
            return Ok(false);
        }
    }
    Some(ConnectionCommand::Stop) | None => {
        stop_driver(driver_tx, driver).await;
        return Ok(true);
    }
}
```

In `agent_server.rs`, change the public entry point to the same TCP-only settings and command contract:

```rust
pub async fn run_agent_server(
    settings: TcpConnectionSettings,
    events: UnboundedSender<ConnectionEvent>,
    mut commands: UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    emit(&events, ConnectionEvent::Waiting);
    run_tcp_server(settings, events, &mut commands).await
}
```

Delete all KCP pointer endpoint code, KCP driver traits/implementations, UDP imports, and `latest_pointer_as_reliable` adapters. Keep the generic TCP reader/writer driver and its timeout behavior.

Inside `run_tcp_server`, bind `TcpListener` with `settings.peer_addr()`, emit `ConnectionEvent::Waiting` before accepting, emit `ConnectionEvent::Connected { peer }` after accept, and pass every `ConnectionCommand::Send(message)` to the existing TCP driver. `ConnectionCommand::Stop` must stop the active driver and return `Ok(())`; EOF or timeout must emit `Disconnected` and resume the accept loop. No KCP or UDP branch remains.

- [ ] **Step 4: Migrate app call sites and remove KCP modules**

In `runtime.rs`, create TCP settings directly:

```rust
fn controller_connection_settings(config: &AppConfig) -> TcpConnectionSettings {
    TcpConnectionSettings {
        host: config.controller.agent_host.clone(),
        port: config.controller.agent_port,
    }
}

fn agent_connection_settings(config: &AppConfig) -> TcpConnectionSettings {
    TcpConnectionSettings {
        host: config.agent.listen_host.clone(),
        port: config.agent.listen_port,
    }
}
```

Mechanically replace `ConnectionCommand::SendReliable(message)` with `ConnectionCommand::Send(message)`. Replace latest-pointer commands with `WireMessage::Input(InputEvent::MouseMoveAbs(...))` sent through `Send`.

Update the temporary app status consumer so this task compiles before Task 3 removes the dead fields:

```rust
ConnectionEvent::Connected { .. } => status.run_state = RunState::Connected,
```

and log the fixed transport explicitly:

```rust
ConnectionEvent::Connected { peer } => Some(format!("connected {peer} via TCP")),
```

Update tests that previously expected `status.transport_mode` to assert only the connected run state. Leave the unused status field itself for Task 3 so this task remains focused on the endpoint contract.

Remove these exports from `borderless-net/src/lib.rs`:

```rust
pub mod kcp_transport;
pub mod latest_pointer;
```

Delete `kcp_transport.rs` and `latest_pointer.rs`. Remove `kcp-tokio.workspace = true` from `borderless-net/Cargo.toml` and `kcp-tokio = "0.7"` from the workspace manifest.

- [ ] **Step 5: Regenerate the lock file and run network tests**

Run:

```powershell
cargo check --workspace
cargo test -p borderless-net
```

Expected: PASS; `Cargo.lock` no longer contains `kcp-core` or `kcp-tokio`.

- [ ] **Step 6: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/borderless-net crates/borderless-app/src/runtime.rs
git commit -m "refactor: use TCP for all control traffic"
```

---

### Task 3: Make Pointer Buffering and Status TCP-Only

**Files:**
- Modify: `crates/borderless-app/src/runtime.rs`
- Modify: `crates/borderless-app/src/status.rs`
- Test: `crates/borderless-app/src/runtime.rs`
- Test: `crates/borderless-app/src/status.rs`

- [ ] **Step 1: Replace mode-specific buffer tests with ordering tests**

Delete KCP pointer-path and stale-packet tests. Replace constructor calls with `RemoteInputSendBuffer::new()`. Add:

```rust
#[test]
fn reliable_input_flushes_latest_pointer_before_click() {
    let mut buffer = RemoteInputSendBuffer::new();
    assert!(buffer.send_pointer(Point::new(10, 20), 1).is_empty());
    assert!(buffer.send_pointer(Point::new(30, 40), 2).is_empty());

    let actions = buffer.send_reliable_input(InputEvent::MouseButton(MouseButtonEvent {
        button: MouseButton::Left,
        pressed: true,
    }));

    assert_eq!(
        actions,
        vec![
            RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::Input(
                InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x: 30, y: 40 })
            ))),
            RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::Input(
                InputEvent::MouseButton(MouseButtonEvent {
                    button: MouseButton::Left,
                    pressed: true,
                })
            ))),
        ]
    );
}
```

Update the status reset test so it initializes only TCP-relevant metrics:

```rust
let mut status = AppStatus {
    last_error: Some("boom".to_string()),
    recent_rtt_ms: Some(10),
    average_rtt_ms: Some(20),
    clipboard_enabled: true,
    transfer_active: true,
    transfer_id: Some(Uuid::nil()),
    transfer_bytes_done: 1,
    transfer_bytes_total: 2,
    transfer_current_file: Some("a.txt".to_string()),
    ..AppStatus::default()
};
```

- [ ] **Step 2: Run focused tests and confirm compilation fails**

Run:

```powershell
cargo test -p borderless-app reliable_input_flushes_latest_pointer_before_click
cargo test -p borderless-app status::tests::reset_runtime_fields_clears_live_metrics
```

Expected: FAIL because the buffer constructor still requires `TransportMode` and status still exposes UDP metrics.

- [ ] **Step 3: Simplify the send buffer**

Replace the mode field and constructor:

```rust
struct RemoteInputSendBuffer {
    pending_move: Option<Point>,
    dropped_moves: u64,
}

impl RemoteInputSendBuffer {
    fn new() -> Self {
        Self {
            pending_move: None,
            dropped_moves: 0,
        }
    }

    fn send_pointer(&mut self, point: Point, _now_millis: u64) -> Vec<RemoteSendAction> {
        if self.pending_move.replace(point).is_some() {
            self.dropped_moves = self.dropped_moves.saturating_add(1);
        }
        Vec::new()
    }

    fn flush_pending_move(&mut self) -> Vec<RemoteSendAction> {
        self.pending_move
            .take()
            .map(|point| {
                vec![RemoteSendAction::Command(ConnectionCommand::Send(
                    WireMessage::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent {
                        x: point.x,
                        y: point.y,
                    })),
                ))]
            })
            .unwrap_or_default()
    }
}
```

Keep the existing coalescing log gate and reliable-input ordering logic, but remove all `TransportMode` matches and `latest_pointer_command`.

- [ ] **Step 4: Remove UDP metrics from status and event handling**

Remove these fields and their reset logic:

```rust
pub transport_mode: Option<TransportMode>,
pub stale_pointer_packets: u64,
pub latest_pointer_sequence: Option<u64>,
```

Delete `StalePointerPacketGate`, `StalePointerPacketEmission`, `apply_stale_pointer_packet_update`, their tests, and all `ConnectionEvent::LatestPointer` / `StalePointerPackets` arms. A connected event now updates only the run state:

```rust
ConnectionEvent::Connected { .. } => status.run_state = RunState::Connected,
```

Log TCP explicitly:

```rust
ConnectionEvent::Connected { peer } => Some(format!("connected {peer} via TCP")),
```

- [ ] **Step 5: Run app and workspace tests**

Run:

```powershell
cargo test -p borderless-app
cargo test --workspace
```

Expected: PASS; no app status or runtime symbol references latest-pointer UDP state.

- [ ] **Step 6: Commit**

```powershell
git add crates/borderless-app/src/runtime.rs crates/borderless-app/src/status.rs
git commit -m "refactor: coalesce pointer input on TCP"
```

---

### Task 4: Remove Legacy Transport Configuration and UI

**Files:**
- Modify: `crates/borderless-core/src/config.rs`
- Modify: `crates/borderless-app/src/app.rs`
- Modify: `config.example.toml`
- Modify: `README.md`
- Modify: `tests/manual/windows-two-machine-checklist.md`
- Test: `crates/borderless-core/src/config.rs`

- [ ] **Step 1: Add the legacy migration regression test**

Add to `config.rs` tests:

```rust
#[test]
fn legacy_transport_fields_are_ignored_and_not_reserialized() {
    let legacy = r#"
role = "controller"

[controller]
agent_host = "192.168.1.20"
agent_port = 24800
transport_mode = "kcp"
pointer_port = 24801
remote_position = "right"

[agent]
listen_host = "0.0.0.0"
listen_port = 24800
transport_mode = "kcp"
pointer_port = 24801
"#;

    let config: AppConfig = toml::from_str(legacy).unwrap();
    assert_eq!(config.controller.agent_host, "192.168.1.20");
    assert_eq!(config.controller.agent_port, 24800);
    assert_eq!(config.agent.listen_port, 24800);

    let encoded = toml::to_string_pretty(&config).unwrap();
    assert!(!encoded.contains("transport_mode"));
    assert!(!encoded.contains("pointer_port"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```powershell
cargo test -p borderless-core legacy_transport_fields_are_ignored_and_not_reserialized
```

Expected: FAIL because current serialization still emits both legacy fields.

- [ ] **Step 3: Remove the legacy configuration types and validation**

Delete `TransportMode`. Make the final configuration shapes:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerConfig {
    pub agent_host: String,
    pub agent_port: u16,
    pub remote_position: RemotePosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub listen_host: String,
    pub listen_port: u16,
}
```

Defaults remain `192.168.1.2:24800` for the controller target and `0.0.0.0:24800` for the agent listener. Delete pointer-port validation and update round-trip/default tests to assert only these fields.

- [ ] **Step 4: Remove transport controls from the eframe UI**

Remove the `TransportMode` import, `transport_selector`, both Pointer UDP rows, the KCP explanation, and transport/latest-pointer status rows. Keep a read-only `TCP` label next to each port until Slint replaces this UI in the next plan.

Update `config.example.toml` controller and agent sections to:

```toml
[controller]
agent_host = "192.168.1.2"
agent_port = 24800
remote_position = "right"

[agent]
listen_host = "0.0.0.0"
listen_port = 24800
```

Update README and the manual checklist so firewall instructions list:

- control TCP port `24800`
- bulk transfer TCP port `24802`

No text should tell users to choose a transport or open a UDP port.

- [ ] **Step 5: Verify migration and search for leftovers**

Run:

```powershell
cargo test -p borderless-core
cargo test -p borderless-app
rg -n -i "kcp|pointer_port|pointer udp|UdpSocket|TransportMode" Cargo.toml Cargo.lock config.example.toml README.md crates tests/manual
```

Expected: both test commands pass; the search returns no product/config/test matches.

- [ ] **Step 6: Commit**

```powershell
git add crates/borderless-core/src/config.rs crates/borderless-app/src/app.rs config.example.toml README.md tests/manual/windows-two-machine-checklist.md
git commit -m "refactor: remove legacy transport configuration"
```

---

### Task 5: TCP-Only Quality Gate

**Files:**
- No planned file changes.

- [ ] **Step 1: Confirm dependency removal**

Run:

```powershell
cargo tree -p borderless-net
rg -n -i "kcp-core|kcp-tokio|UdpSocket|pointer_port|SendLatestPointer|LatestPointer|StalePointerPackets" Cargo.toml Cargo.lock crates config.example.toml
```

Expected: neither command output contains KCP or UDP pointer implementation symbols.

- [ ] **Step 2: Run formatting and all tests**

Run:

```powershell
cargo fmt --all -- --check
cargo test --workspace
```

Expected: both commands exit `0` with zero failures.

- [ ] **Step 3: Run strict lint**

Run:

```powershell
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: exit `0` with no warnings. If it fails, return to the task that introduced the finding, add a focused regression test where behavior is involved, and rerun that task before continuing this gate.

- [ ] **Step 4: Build the TCP-only app**

Run:

```powershell
cargo build --release -p borderless-app
```

Expected: `target/release/borderless.exe` is produced successfully.

- [ ] **Step 5: Confirm the phase is clean**

Run:

```powershell
git status --short
```

Expected: no uncommitted files from the TCP-only tasks. The intentionally untracked `old_drag_drop_reference.rs` may still appear.

---

## Phase Acceptance

- Old `transport_mode = "kcp"` configuration loads and is saved without transport/UDP fields.
- Control, keyboard, mouse, clipboard control, heartbeat, disconnect, and reconnect use TCP only.
- Bulk files still use their separate TCP port.
- Pointer moves remain coalesced and button order tests pass.
- Workspace format, tests, Clippy, and release build pass.
- Proceed to `docs/superpowers/plans/2026-07-13-slint-control-desk.md`.
