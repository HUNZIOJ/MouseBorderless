mod drag_drop;

use std::{
    collections::{HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use borderless_core::{
    clipboard::{ClipboardEnvelope, ClipboardPayload, RemoteFileOffer},
    config::{AppConfig, Role},
    control::{ControlMode, ControlOutput, ControlState},
    file_transfer::{FileManifestEntry, FileTransferDestination},
    geometry::{Edge, Point, Rect},
    input_event::{InputEvent, MouseMoveAbsEvent},
    protocol::{
        encode_frame, Heartbeat, Hello, ProtocolError, WireMessage, MAX_PAYLOAD_LEN,
        PROTOCOL_VERSION,
    },
};
use borderless_net::{
    agent_server::run_agent_server,
    bulk_transfer::{
        manifest_from_source_paths, run_bulk_transfer_client, run_bulk_transfer_server,
        BulkTransferCommand, BulkTransferEvent,
    },
    controller_client::run_controller_client,
    drop_authorization::DropAuthorizationRegistry,
    transport::{ConnectionCommand, ConnectionEvent, TcpConnectionSettings},
};
use borderless_win::{
    clipboard::{write_clipboard, ClipboardEvent, ClipboardMonitor, ClipboardReadOptions},
    drag_drop::{DragDropEvent, EdgeDropTarget},
    hooks::{HookEvent, HookManager, SuppressionMode},
    inject::{move_local_pointer_to, InputInjector},
    monitor::virtual_desktop_rect,
};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{interval, sleep_until, Instant as TokioInstant, MissedTickBehavior},
};
use uuid::Uuid;

use crate::status::{AppStatus, RunState};
use drag_drop::{DragDropCoordinator, DragEffect};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(4);
const MAX_OUTSTANDING_HEARTBEATS: usize = 8;
const SESSION_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const REMOTE_MOVE_COALESCE_LOG_THRESHOLD: u64 = 100;
const REMOTE_MOVE_COALESCE_LOG_WINDOW_MILLIS: u64 = 1_000;
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CLIPBOARD_PEER_NOT_CONNECTED_REASON: &str = "clipboard sync skipped: peer is not connected";
const PARK_POINTER_TOLERANCE_PX: i32 = 32;
const PARK_POINTER_EDGE_MARGIN_PX: i32 = 96;
const RAW_DELTA_RECENT_MILLIS: u64 = 50;
const RAW_FALLBACK_DELAY_MILLIS: u64 = 4;
const MOUSE_DIAGNOSTICS_EMIT_INTERVAL_MILLIS: u64 = 500;

#[derive(Clone, Debug)]
pub enum RuntimeCommand {
    Start(AppConfig),
    Stop,
    Reconnect(AppConfig),
    CancelTransfer(Uuid),
}
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RuntimeEvent {
    Status(AppStatus),
    Log(String),
}

#[derive(Clone)]
pub struct RuntimeHandle {
    commands: Sender<RuntimeCommand>,
    events: Receiver<RuntimeEvent>,
}

impl RuntimeHandle {
    pub fn spawn() -> Self {
        let (commands_tx, commands_rx) = unbounded();
        let (events_tx, events_rx) = unbounded();

        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
                .expect("create tokio runtime");

            rt.block_on(run_runtime_loop(commands_rx, events_tx));
        });

        Self {
            commands: commands_tx,
            events: events_rx,
        }
    }

    pub fn send(&self, command: RuntimeCommand) {
        let _ = self.commands.send(command);
    }

    pub fn drain_events(&self) -> Vec<RuntimeEvent> {
        self.events.try_iter().collect()
    }
}

struct ActiveRuntime {
    session_id: u64,
    connection_commands: mpsc::UnboundedSender<ConnectionCommand>,
    session_commands: mpsc::UnboundedSender<SessionCommand>,
    bulk_commands: Vec<mpsc::UnboundedSender<BulkTransferCommand>>,
    hook_manager: Option<Arc<Mutex<HookManager>>>,
    remote_control_active: Option<Arc<AtomicBool>>,
    tasks: Vec<JoinHandle<()>>,
    stopped: bool,
}

impl ActiveRuntime {
    async fn stop(&mut self) -> StopTaskOutcome {
        self.request_stop();
        wait_for_session_tasks(&mut self.tasks, SESSION_STOP_TIMEOUT).await
    }

    fn request_stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;

        if self
            .remote_control_active
            .as_ref()
            .is_some_and(|active| active.swap(false, Ordering::SeqCst))
        {
            let _ = self
                .connection_commands
                .send(ConnectionCommand::Send(WireMessage::ReleaseAll));
        }
        let _ = self.session_commands.send(SessionCommand::Stop);
        let _ = self.connection_commands.send(ConnectionCommand::Stop);
        for commands in &self.bulk_commands {
            let _ = commands.send(BulkTransferCommand::Stop);
        }

        if let Some(hook_manager) = self.hook_manager.take() {
            set_hook_suppression(&hook_manager, SuppressionMode::PassThrough);
        }
    }
}

impl Drop for ActiveRuntime {
    fn drop(&mut self) {
        self.request_stop();
        abort_session_tasks(&mut self.tasks);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StopTaskOutcome {
    completed: usize,
    aborted: usize,
}

async fn wait_for_session_tasks(
    tasks: &mut Vec<JoinHandle<()>>,
    timeout: Duration,
) -> StopTaskOutcome {
    let mut outcome = StopTaskOutcome::default();
    let deadline = TokioInstant::now() + timeout;

    for mut task in std::mem::take(tasks) {
        tokio::select! {
            result = &mut task => {
                let _ = result;
                outcome.completed += 1;
            }
            _ = sleep_until(deadline) => {
                task.abort();
                outcome.aborted += 1;
            }
        }
    }

    outcome
}

fn abort_session_tasks(tasks: &mut Vec<JoinHandle<()>>) {
    for task in tasks.drain(..) {
        task.abort();
    }
}

#[derive(Clone, Copy, Debug)]
enum SessionCommand {
    Stop,
}

#[derive(Clone, Debug)]
struct TaggedSessionUpdate {
    session_id: u64,
    update: SessionUpdate,
}

#[derive(Clone, Debug)]
enum SessionUpdate {
    Connection(ConnectionEvent),
    Error(String),
    Log(String),
    Rtt(u64),
    RunState(RunState),
    ClipboardQueued {
        format: String,
        bytes: u64,
    },
    ClipboardWritten {
        format: String,
        bytes: u64,
    },
    ClipboardIgnored(String),
    ClipboardError(String),
    BulkTransfer(BulkTransferEvent),
    ClipboardFileTransferStarted(Uuid),
    DragDropStatus {
        state: String,
        destination: Option<String>,
    },
    MouseDiagnostics(String),
}

async fn run_runtime_loop(commands_rx: Receiver<RuntimeCommand>, events_tx: Sender<RuntimeEvent>) {
    let (updates_tx, updates_rx) = unbounded();
    let mut active: Option<ActiveRuntime> = None;
    let mut status = AppStatus::default();
    let mut transfer_purposes = TransferPurposeTracker::default();
    let mut next_session_id: u64 = 1;

    loop {
        select! {
            recv(commands_rx) -> command => {
                let Ok(command) = command else {
                    break;
                };

                match command {
                    RuntimeCommand::Start(config) => {
                        if let Some(mut session) = active.take() {
                            let outcome = session.stop().await;
                            log_stop_task_outcome(&mut status, &events_tx, outcome);
                        }

                        transfer_purposes.clear();
                        prepare_running_status(&mut status, &config, RunState::Connecting);
                        emit_log(&mut status, &events_tx, "starting runtime");
                        emit_status(&events_tx, &status);

                        let session_id = next_session_id;
                        next_session_id = next_session_id.saturating_add(1);
                        active = start_session(session_id, config, updates_tx.clone())
                            .map_err(|error| {
                                apply_session_update(
                                    &mut status,
                                    &events_tx,
                                    SessionUpdate::Error(error),
                                    &mut transfer_purposes,
                                );
                            })
                            .ok();
                    }
                    RuntimeCommand::Stop => {
                        if let Some(mut session) = active.take() {
                            let outcome = session.stop().await;
                            log_stop_task_outcome(&mut status, &events_tx, outcome);
                        }

                        transfer_purposes.clear();
                        prepare_stopped_status(&mut status);
                        emit_log(&mut status, &events_tx, "runtime stopped");
                        emit_status(&events_tx, &status);
                    }
                    RuntimeCommand::Reconnect(config) => {
                        if let Some(mut session) = active.take() {
                            let outcome = session.stop().await;
                            log_stop_task_outcome(&mut status, &events_tx, outcome);
                        }

                        transfer_purposes.clear();
                        prepare_running_status(&mut status, &config, RunState::Reconnecting);
                        emit_log(&mut status, &events_tx, "reconnecting runtime");
                        emit_status(&events_tx, &status);

                        let session_id = next_session_id;
                        next_session_id = next_session_id.saturating_add(1);
                        active = start_session(session_id, config, updates_tx.clone())
                            .map_err(|error| {
                                apply_session_update(
                                    &mut status,
                                    &events_tx,
                                    SessionUpdate::Error(error),
                                    &mut transfer_purposes,
                                );
                            })
                            .ok();
                    }
                    RuntimeCommand::CancelTransfer(transfer_id) => {
                        if let Some(session) = &active {
                            for commands in &session.bulk_commands {
                                let _ = commands.send(BulkTransferCommand::Cancel(transfer_id));
                            }
                            emit_log(
                                &mut status,
                                &events_tx,
                                format!("requested transfer cancel: {transfer_id}"),
                            );
                        }
                    }
                }
            }
            recv(updates_rx) -> update => {
                let Ok(update) = update else {
                    continue;
                };

                if active
                    .as_ref()
                    .is_some_and(|session| session.session_id == update.session_id)
                {
                        apply_session_update(
                            &mut status,
                            &events_tx,
                            update.update,
                            &mut transfer_purposes,
                        );
                }
            }
        }
    }

    if let Some(mut session) = active {
        let _ = session.stop().await;
    }
}

fn start_session(
    session_id: u64,
    config: AppConfig,
    updates: Sender<TaggedSessionUpdate>,
) -> Result<ActiveRuntime, String> {
    match config.role {
        Role::Controller => start_controller_session(session_id, config, updates),
        Role::Agent => start_agent_session(session_id, config, updates),
    }
}

fn start_controller_session(
    session_id: u64,
    config: AppConfig,
    updates: Sender<TaggedSessionUpdate>,
) -> Result<ActiveRuntime, String> {
    let local_desktop = virtual_desktop_rect();
    let settings = controller_connection_settings(&config);
    let (connection_events_tx, connection_events_rx) = mpsc::unbounded_channel();
    let (connection_commands_tx, connection_commands_rx) = mpsc::unbounded_channel();
    let (session_commands_tx, session_commands_rx) = mpsc::unbounded_channel();
    let (hook_events_tx, hook_events_rx) = unbounded();
    let hook_manager = Arc::new(Mutex::new(
        HookManager::install(hook_events_tx).map_err(|error| error.to_string())?,
    ));
    let remote_control_active = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    let mut bulk_runtime = start_configured_bulk_runtime(
        session_id,
        &config,
        updates.clone(),
        BulkClientPeer::Configured(config.controller.agent_host.clone()),
    )?;
    let bulk_commands = bulk_runtime.commands.clone();

    let transport_updates = updates.clone();
    tasks.push(tokio::spawn(async move {
        if let Err(error) =
            run_controller_client(settings, connection_events_tx, connection_commands_rx).await
        {
            send_session_update(
                &transport_updates,
                session_id,
                SessionUpdate::Error(format!("controller transport exited: {error}")),
            );
        }
    }));

    let pump_updates = updates;
    let pump_commands = connection_commands_tx.clone();
    let pump_hook_manager = Arc::clone(&hook_manager);
    let pump_remote_control_active = Arc::clone(&remote_control_active);
    tasks.push(tokio::spawn(async move {
        run_controller_event_pump(
            session_id,
            config,
            local_desktop,
            pump_hook_manager,
            pump_remote_control_active,
            hook_events_rx,
            connection_events_rx,
            pump_commands,
            session_commands_rx,
            bulk_commands,
            pump_updates,
        )
        .await;
    }));
    tasks.append(&mut bulk_runtime.tasks);

    Ok(ActiveRuntime {
        session_id,
        connection_commands: connection_commands_tx,
        session_commands: session_commands_tx,
        bulk_commands: bulk_runtime.commands,
        hook_manager: Some(hook_manager),
        remote_control_active: Some(remote_control_active),
        tasks,
        stopped: false,
    })
}

fn start_agent_session(
    session_id: u64,
    config: AppConfig,
    updates: Sender<TaggedSessionUpdate>,
) -> Result<ActiveRuntime, String> {
    let local_desktop = virtual_desktop_rect();
    let settings = agent_connection_settings(&config);
    let (connection_events_tx, connection_events_rx) = mpsc::unbounded_channel();
    let (connection_commands_tx, connection_commands_rx) = mpsc::unbounded_channel();
    let (session_commands_tx, session_commands_rx) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    let mut bulk_runtime =
        start_configured_bulk_runtime(session_id, &config, updates.clone(), BulkClientPeer::None)?;
    let bulk_commands = bulk_runtime.commands.clone();

    let transport_updates = updates.clone();
    tasks.push(tokio::spawn(async move {
        if let Err(error) =
            run_agent_server(settings, connection_events_tx, connection_commands_rx).await
        {
            send_session_update(
                &transport_updates,
                session_id,
                SessionUpdate::Error(format!("agent transport exited: {error}")),
            );
        }
    }));

    let pump_updates = updates;
    let pump_commands = connection_commands_tx.clone();
    tasks.push(tokio::spawn(async move {
        run_agent_event_pump(
            session_id,
            config,
            local_desktop,
            connection_events_rx,
            pump_commands,
            session_commands_rx,
            bulk_commands,
            pump_updates,
        )
        .await;
    }));
    tasks.append(&mut bulk_runtime.tasks);

    Ok(ActiveRuntime {
        session_id,
        connection_commands: connection_commands_tx,
        session_commands: session_commands_tx,
        bulk_commands: bulk_runtime.commands,
        hook_manager: None,
        remote_control_active: None,
        tasks,
        stopped: false,
    })
}

struct BulkRuntime {
    commands: Vec<mpsc::UnboundedSender<BulkTransferCommand>>,
    tasks: Vec<JoinHandle<()>>,
}

enum BulkClientPeer {
    Configured(String),
    None,
}

fn start_configured_bulk_runtime(
    session_id: u64,
    config: &AppConfig,
    updates: Sender<TaggedSessionUpdate>,
    client_peer: BulkClientPeer,
) -> Result<BulkRuntime, String> {
    let drop_authorizations = DropAuthorizationRegistry::default();
    let cache_dir = config
        .resolved_incoming_cache_dir()
        .map_err(|error| error.to_string())?
        .to_string_lossy()
        .to_string();
    let mut runtime = BulkRuntime {
        commands: Vec::new(),
        tasks: Vec::new(),
    };

    let server_host = match config.role {
        Role::Controller => "0.0.0.0".to_string(),
        Role::Agent => config.agent.listen_host.clone(),
    };
    push_bulk_server(
        &mut runtime,
        session_id,
        server_host,
        config.sharing.bulk_transfer_port,
        cache_dir.clone(),
        drop_authorizations.clone(),
        updates.clone(),
    );

    if let BulkClientPeer::Configured(host) = client_peer {
        push_bulk_client(
            &mut runtime,
            session_id,
            host,
            config.sharing.bulk_transfer_port,
            cache_dir,
            drop_authorizations,
            updates,
        );
    }

    Ok(runtime)
}

fn push_bulk_server(
    runtime: &mut BulkRuntime,
    session_id: u64,
    listen_host: String,
    port: u16,
    cache_dir: String,
    drop_authorizations: DropAuthorizationRegistry,
    updates: Sender<TaggedSessionUpdate>,
) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let server_updates = updates.clone();

    runtime.commands.push(commands_tx);
    runtime.tasks.push(tokio::spawn(async move {
        if let Err(error) = run_bulk_transfer_server(
            listen_host,
            port,
            cache_dir,
            drop_authorizations,
            events_tx,
            commands_rx,
        )
        .await
        {
            send_session_update(
                &server_updates,
                session_id,
                SessionUpdate::Error(format!("bulk transfer server exited: {error}")),
            );
        }
    }));
    runtime
        .tasks
        .push(tokio::spawn(forward_bulk_transfer_events(
            session_id, events_rx, updates,
        )));
}

fn push_bulk_client(
    runtime: &mut BulkRuntime,
    session_id: u64,
    host: String,
    port: u16,
    cache_dir: String,
    drop_authorizations: DropAuthorizationRegistry,
    updates: Sender<TaggedSessionUpdate>,
) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let client_updates = updates.clone();

    runtime.commands.push(commands_tx);
    runtime.tasks.push(tokio::spawn(async move {
        if let Err(error) = run_bulk_transfer_client(
            host,
            port,
            cache_dir,
            drop_authorizations,
            events_tx,
            commands_rx,
        )
        .await
        {
            send_session_update(
                &client_updates,
                session_id,
                SessionUpdate::Error(format!("bulk transfer client exited: {error}")),
            );
        }
    }));
    runtime
        .tasks
        .push(tokio::spawn(forward_bulk_transfer_events(
            session_id, events_rx, updates,
        )));
}

async fn forward_bulk_transfer_events(
    session_id: u64,
    mut events: mpsc::UnboundedReceiver<BulkTransferEvent>,
    updates: Sender<TaggedSessionUpdate>,
) {
    while let Some(event) = events.recv().await {
        send_session_update(&updates, session_id, SessionUpdate::BulkTransfer(event));
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_controller_event_pump(
    session_id: u64,
    config: AppConfig,
    local_desktop: Rect,
    hook_manager: Arc<Mutex<HookManager>>,
    remote_control_active: Arc<AtomicBool>,
    hook_events: Receiver<HookEvent>,
    mut connection_events: mpsc::UnboundedReceiver<ConnectionEvent>,
    connection_commands: mpsc::UnboundedSender<ConnectionCommand>,
    mut session_commands: mpsc::UnboundedReceiver<SessionCommand>,
    bulk_commands: Vec<mpsc::UnboundedSender<BulkTransferCommand>>,
    updates: Sender<TaggedSessionUpdate>,
) {
    let mut control_state: Option<ControlState> = None;
    let mut last_pointer: Option<Point> = None;
    let mut pending_pointer_park: Option<Point> = None;
    let mut pending_remote_pointer_position: Option<PendingRemotePointerPosition> = None;
    let mut mouse_diagnostics = MouseDiagnostics::default();
    let mut remote_input_send_buffer = RemoteInputSendBuffer::new();
    let mut heartbeat = HeartbeatTracker::default();
    let mut heartbeat_interval = interval(HEARTBEAT_INTERVAL);
    let mut hook_interval = interval(HOOK_POLL_INTERVAL);
    let mut clipboard_interval = interval(CLIPBOARD_POLL_INTERVAL);
    let mut clipboard_runtime = start_clipboard_runtime(&config, &updates, session_id);
    let mut clipboard_transport_ready = false;
    let mut drag_runtime = start_drag_drop_runtime(&config);
    let mut drag_drop = DragDropCoordinator::new(Role::Controller);
    let mut drag_was_remote = false;
    heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    hook_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    clipboard_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = heartbeat_interval.tick() => {
                send_heartbeat(&connection_commands, &mut heartbeat);
            }
            _ = hook_interval.tick() => {
                process_pending_remote_pointer_position(
                    local_desktop,
                    &mut control_state,
                    &mut last_pointer,
                    &mut pending_pointer_park,
                    &mut pending_remote_pointer_position,
                    &mut mouse_diagnostics,
                    &mut remote_input_send_buffer,
                    &hook_manager,
                    &remote_control_active,
                    &connection_commands,
                    &updates,
                    session_id,
                );
                while let Ok(event) = hook_events.try_recv() {
                    handle_controller_hook_event(
                        event,
                        local_desktop,
                        &mut control_state,
                        &mut last_pointer,
                        &mut pending_pointer_park,
                        &mut pending_remote_pointer_position,
                        &mut mouse_diagnostics,
                        &mut remote_input_send_buffer,
                        &hook_manager,
                        &remote_control_active,
                        &connection_commands,
                        &updates,
                        session_id,
                    );
                }
                drain_local_drag_events(
                    &mut drag_drop,
                    &mut drag_runtime,
                    &connection_commands,
                    &bulk_commands,
                    &updates,
                    session_id,
                );
                let drag_now_remote = control_state
                    .as_ref()
                    .is_some_and(|state| state.mode() == ControlMode::Remote);
                if drag_now_remote != drag_was_remote {
                    drag_was_remote = drag_now_remote;
                    let effects = drag_drop.set_control_mode(if drag_now_remote {
                        ControlMode::Remote
                    } else {
                        ControlMode::Local
                    });
                    apply_drag_effects(
                        effects,
                        &mut drag_runtime,
                        &connection_commands,
                        &bulk_commands,
                        &updates,
                        session_id,
                    );
                }
                emit_remote_send_actions(
                    remote_input_send_buffer.flush_pending_move(),
                    &connection_commands,
                    &updates,
                    session_id,
                );

                if let Some(summary) = mouse_diagnostics.summary_if_due(now_millis()) {
                    send_session_update(
                        &updates,
                        session_id,
                        SessionUpdate::MouseDiagnostics(summary),
                    );
                }
            }
            _ = clipboard_interval.tick(), if clipboard_runtime.is_some() => {
                if let Some(clipboard) = &clipboard_runtime {
                    drain_clipboard_events(
                        &clipboard.events,
                        &config,
                        &connection_commands,
                        &bulk_commands,
                        &updates,
                        session_id,
                        clipboard_transport_ready,
                    );
                }
            }
            command = session_commands.recv() => {
                match command {
                    Some(SessionCommand::Stop) | None => {
                        set_hook_suppression(&hook_manager, SuppressionMode::PassThrough);
                        if remote_control_active.swap(false, Ordering::SeqCst) {
                            emit_remote_send_actions(
                                remote_input_send_buffer.send_release_all(),
                                &connection_commands,
                                &updates,
                                session_id,
                            );
                        }
                        stop_clipboard_runtime(clipboard_runtime.take(), &updates, session_id);
                        break;
                    }
                }
            }
            event = connection_events.recv() => {
                let Some(event) = event else {
                    break;
                };

                let readiness_event = event.clone();
                if let ConnectionEvent::Message(message) = &event {
                    let effects = drag_drop.handle_peer_message(message.clone());
                    apply_drag_effects(
                        effects,
                        &mut drag_runtime,
                        &connection_commands,
                        &bulk_commands,
                        &updates,
                        session_id,
                    );
                    if matches!(message, WireMessage::Hello(hello) if hello.protocol_version == PROTOCOL_VERSION) {
                        let layout = WireMessage::PeerLayout {
                            controller_remote_position: config.controller.remote_position.clone(),
                        };
                        let _ = connection_commands.send(ConnectionCommand::Send(layout.clone()));
                        let effects = drag_drop.handle_peer_message(layout);
                        apply_drag_effects(
                            effects,
                            &mut drag_runtime,
                            &connection_commands,
                            &bulk_commands,
                            &updates,
                            session_id,
                        );
                    }
                }
                if matches!(
                    event,
                    ConnectionEvent::Disconnected(_) | ConnectionEvent::Error(_)
                ) {
                    drag_drop.reset();
                }
                handle_controller_connection_event(
                    event,
                    &config,
                    local_desktop,
                    &mut control_state,
                    &hook_manager,
                    &remote_control_active,
                    &connection_commands,
                    &updates,
                    session_id,
                    &mut heartbeat,
                );
                clipboard_transport_ready = controller_clipboard_transport_ready_after_event(
                    clipboard_transport_ready,
                    &readiness_event,
                    &control_state,
                );
            }
        }
    }

    stop_clipboard_runtime(clipboard_runtime.take(), &updates, session_id);
    set_hook_suppression(&hook_manager, SuppressionMode::PassThrough);
    remote_control_active.store(false, Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)]
fn handle_controller_connection_event(
    event: ConnectionEvent,
    config: &AppConfig,
    local_desktop: Rect,
    control_state: &mut Option<ControlState>,
    hook_manager: &Arc<Mutex<HookManager>>,
    remote_control_active: &Arc<AtomicBool>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
    heartbeat: &mut HeartbeatTracker,
) {
    send_session_update(
        updates,
        session_id,
        SessionUpdate::Connection(event.clone()),
    );
    let recovery_action = controller_recovery_action_for_connection_event(&event, control_state);

    match event {
        ConnectionEvent::Message(WireMessage::Hello(hello)) => {
            if hello.protocol_version != PROTOCOL_VERSION {
                let error = format!(
                    "protocol version mismatch: expected {}, got {}",
                    PROTOCOL_VERSION, hello.protocol_version
                );
                let _ = connection_commands.send(ConnectionCommand::Send(
                    WireMessage::Error(error.clone()),
                ));
                send_session_update(updates, session_id, SessionUpdate::Error(error));
                return;
            }

            *control_state = Some(ControlState::new(
                local_desktop,
                hello.desktop,
                config.controller.remote_position.clone(),
                config.edge_trigger_px,
            ));
            remote_control_active.store(false, Ordering::SeqCst);
            send_session_update(
                updates,
                session_id,
                SessionUpdate::Log("agent hello received".to_string()),
            );
            send_session_update(
                updates,
                session_id,
                SessionUpdate::RunState(RunState::LocalControl),
            );
        }
        ConnectionEvent::Message(WireMessage::Heartbeat(message)) => {
            handle_heartbeat_message(message, heartbeat, connection_commands, updates, session_id);
        }
        ConnectionEvent::Message(WireMessage::ClipboardOffer(envelope)) => {
            handle_remote_clipboard_message(
                WireMessage::ClipboardOffer(envelope),
                config,
                updates,
                session_id,
            );
        }
        ConnectionEvent::Message(WireMessage::ClipboardData(envelope)) => {
            handle_remote_clipboard_message(
                WireMessage::ClipboardData(envelope),
                config,
                updates,
                session_id,
            );
        }
        ConnectionEvent::Message(WireMessage::FileTransferOffer(manifest)) => {
            send_session_update(
                updates,
                session_id,
                SessionUpdate::ClipboardFileTransferStarted(manifest.transfer_id),
            );
        }
        ConnectionEvent::Message(WireMessage::Error(error)) => {
            apply_controller_recovery_action(
                recovery_action,
                hook_manager,
                remote_control_active,
                connection_commands,
            );
            send_session_update(updates, session_id, SessionUpdate::Error(error));
        }
        ConnectionEvent::Disconnected(_) | ConnectionEvent::Error(_) => {
            apply_controller_recovery_action(
                recovery_action,
                hook_manager,
                remote_control_active,
                connection_commands,
            );
        }
        ConnectionEvent::Waiting
        | ConnectionEvent::Connecting(_)
        | ConnectionEvent::Connected { .. }
        | ConnectionEvent::Message(WireMessage::Input(_))
        | ConnectionEvent::Message(WireMessage::ReleaseAll)
        | ConnectionEvent::Message(WireMessage::FileTransferProgress { .. })
        | ConnectionEvent::Message(WireMessage::FileTransferComplete { .. })
        // Drag-drop messages are consumed by DragDropCoordinator in the pump.
        | ConnectionEvent::Message(WireMessage::PeerLayout { .. })
        | ConnectionEvent::Message(WireMessage::DragDropReleased { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTargetResolved { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTargetFailed { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTransferStarted { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTransferResult { .. })
        | ConnectionEvent::Message(WireMessage::DragDropCancel { .. })
        | ConnectionEvent::Message(WireMessage::DragDropEntered { .. }) => {}
    }
}

fn controller_clipboard_transport_ready_after_event(
    current: bool,
    event: &ConnectionEvent,
    control_state: &Option<ControlState>,
) -> bool {
    match event {
        ConnectionEvent::Message(WireMessage::Hello(hello)) => {
            hello.protocol_version == PROTOCOL_VERSION && control_state.is_some()
        }
        ConnectionEvent::Waiting
        | ConnectionEvent::Connecting(_)
        | ConnectionEvent::Connected { .. }
        | ConnectionEvent::Disconnected(_)
        | ConnectionEvent::Error(_)
        | ConnectionEvent::Message(WireMessage::Error(_)) => false,
        _ => current,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ControllerRecoveryAction {
    pass_through: bool,
    release_all: bool,
}

fn controller_recovery_action_for_connection_event(
    event: &ConnectionEvent,
    control_state: &mut Option<ControlState>,
) -> ControllerRecoveryAction {
    if !matches!(
        event,
        ConnectionEvent::Disconnected(_)
            | ConnectionEvent::Error(_)
            | ConnectionEvent::Message(WireMessage::Error(_))
    ) {
        return ControllerRecoveryAction::default();
    }

    let was_remote = control_state
        .as_ref()
        .is_some_and(|state| state.mode() == ControlMode::Remote);
    *control_state = None;

    ControllerRecoveryAction {
        pass_through: was_remote,
        release_all: was_remote,
    }
}

fn apply_controller_recovery_action(
    action: ControllerRecoveryAction,
    hook_manager: &Arc<Mutex<HookManager>>,
    remote_control_active: &Arc<AtomicBool>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
) {
    if action.pass_through {
        set_hook_suppression(hook_manager, SuppressionMode::PassThrough);
    }
    if action.release_all {
        emit_recovery_release_all(remote_control_active, || {
            send_release_all(connection_commands)
        });
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RemoteSendAction {
    Command(ConnectionCommand),
    Log(String),
}

#[derive(Clone, Debug)]
struct RemoteInputSendBuffer {
    pending_move: Option<Point>,
    coalescing_log_gate: RemoteMoveCoalescingLogGate,
}

impl RemoteInputSendBuffer {
    fn new() -> Self {
        Self {
            pending_move: None,
            coalescing_log_gate: RemoteMoveCoalescingLogGate::default(),
        }
    }

    fn send_pointer(&mut self, point: Point, now_millis: u64) -> Vec<RemoteSendAction> {
        let mut actions = Vec::new();
        if self.pending_move.replace(point).is_some() {
            if let Some(emission) = self.coalescing_log_gate.record(now_millis) {
                actions.push(RemoteSendAction::Log(emission.log_message()));
            }
        }
        actions
    }

    fn send_reliable_input(&mut self, event: InputEvent) -> Vec<RemoteSendAction> {
        let mut actions = self.flush_pending_move();
        actions.push(RemoteSendAction::Command(ConnectionCommand::Send(
            WireMessage::Input(event),
        )));
        actions
    }

    fn send_release_all(&mut self) -> Vec<RemoteSendAction> {
        let mut actions = self.flush_pending_move();
        actions.push(RemoteSendAction::Command(ConnectionCommand::Send(
            WireMessage::ReleaseAll,
        )));
        actions
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RemoteMoveCoalescingLogGate {
    window_start_millis: Option<u64>,
    coalesced_in_window: u64,
    emitted_in_window: bool,
}

impl RemoteMoveCoalescingLogGate {
    fn record(&mut self, now_millis: u64) -> Option<RemoteMoveCoalescingEmission> {
        if self.window_start_millis.is_none_or(|start| {
            now_millis.saturating_sub(start) >= REMOTE_MOVE_COALESCE_LOG_WINDOW_MILLIS
        }) {
            self.window_start_millis = Some(now_millis);
            self.coalesced_in_window = 0;
            self.emitted_in_window = false;
        }

        self.coalesced_in_window = self.coalesced_in_window.saturating_add(1);
        if self.coalesced_in_window > REMOTE_MOVE_COALESCE_LOG_THRESHOLD && !self.emitted_in_window
        {
            self.emitted_in_window = true;
            Some(RemoteMoveCoalescingEmission {
                count: self.coalesced_in_window,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RemoteMoveCoalescingEmission {
    count: u64,
}

impl RemoteMoveCoalescingEmission {
    fn log_message(self) -> String {
        format!(
            "coalesced remote mouse moves: {} in the last second",
            self.count
        )
    }
}

fn emit_remote_send_actions(
    actions: Vec<RemoteSendAction>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    for action in actions {
        match action {
            RemoteSendAction::Command(command) => {
                let _ = connection_commands.send(command);
            }
            RemoteSendAction::Log(message) => {
                send_session_update(updates, session_id, SessionUpdate::Log(message));
            }
        }
    }
}

fn emit_return_local_release_all(
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
    remote_control_active: &Arc<AtomicBool>,
    emit: impl FnOnce(Vec<RemoteSendAction>),
) {
    let actions = remote_input_send_buffer.send_release_all();
    emit_before_clearing_remote_control(remote_control_active, || emit(actions));
}

fn emit_recovery_release_all(remote_control_active: &Arc<AtomicBool>, emit: impl FnOnce()) {
    emit_before_clearing_remote_control(remote_control_active, emit);
}

fn emit_before_clearing_remote_control(
    remote_control_active: &Arc<AtomicBool>,
    emit: impl FnOnce(),
) {
    emit();
    remote_control_active.store(false, Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)]
fn handle_controller_hook_event(
    event: HookEvent,
    local_desktop: Rect,
    control_state: &mut Option<ControlState>,
    last_pointer: &mut Option<Point>,
    pending_pointer_park: &mut Option<Point>,
    pending_remote_pointer_position: &mut Option<PendingRemotePointerPosition>,
    mouse_diagnostics: &mut MouseDiagnostics,
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
    hook_manager: &Arc<Mutex<HookManager>>,
    remote_control_active: &Arc<AtomicBool>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    match event {
        HookEvent::PointerPosition { x, y } => {
            let point = Point::new(x, y);
            let remote_mode = control_state
                .as_ref()
                .is_some_and(|state| state.mode() == ControlMode::Remote);
            mouse_diagnostics.record_hook_position(point, remote_mode);
            let had_pending_park = pending_pointer_park.is_some();
            if consume_pending_pointer_park(point, last_pointer, pending_pointer_park) {
                mouse_diagnostics.record_pointer_park_consumed();
                return;
            }
            if had_pending_park {
                mouse_diagnostics.record_pointer_park_missed();
            }

            let Some(state) = control_state.as_mut() else {
                *last_pointer = Some(point);
                return;
            };

            match state.mode() {
                ControlMode::Local => {
                    *last_pointer = Some(point);
                    let output = state.observe_local_pointer(point);
                    handle_control_output(
                        output,
                        local_desktop,
                        last_pointer,
                        pending_pointer_park,
                        mouse_diagnostics,
                        false,
                        remote_input_send_buffer,
                        hook_manager,
                        remote_control_active,
                        connection_commands,
                        updates,
                        session_id,
                    );
                }
                ControlMode::Remote => {
                    let raw_delta_recent = mouse_diagnostics.raw_delta_recent(now_millis());
                    if !raw_delta_recent {
                        *pending_remote_pointer_position = Some(PendingRemotePointerPosition {
                            point,
                            observed_millis: now_millis(),
                        });
                        return;
                    }
                    match remote_pointer_position_action(
                        point,
                        local_desktop,
                        last_pointer,
                        raw_delta_recent,
                    ) {
                        RemotePointerPositionAction::TrackOnly { repark_after_move } => {
                            if repark_after_move {
                                park_local_pointer_for_remote_control(
                                    local_desktop,
                                    last_pointer,
                                    pending_pointer_park,
                                    mouse_diagnostics,
                                    hook_manager,
                                    updates,
                                    session_id,
                                );
                            }
                        }
                        RemotePointerPositionAction::Delta {
                            dx,
                            dy,
                            repark_after_move,
                        } => {
                            let output = state.apply_remote_delta(dx, dy);
                            handle_control_output(
                                output,
                                local_desktop,
                                last_pointer,
                                pending_pointer_park,
                                mouse_diagnostics,
                                repark_after_move,
                                remote_input_send_buffer,
                                hook_manager,
                                remote_control_active,
                                connection_commands,
                                updates,
                                session_id,
                            );
                        }
                    }
                }
            }
        }
        HookEvent::Input(input) => {
            if control_state
                .as_ref()
                .is_some_and(|state| state.mode() == ControlMode::Remote)
            {
                send_remote_input(
                    input,
                    local_desktop,
                    control_state,
                    last_pointer,
                    pending_pointer_park,
                    pending_remote_pointer_position,
                    mouse_diagnostics,
                    remote_input_send_buffer,
                    hook_manager,
                    remote_control_active,
                    connection_commands,
                    updates,
                    session_id,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingRemotePointerPosition {
    point: Point,
    observed_millis: u64,
}

fn pending_remote_pointer_position_due(
    pending: PendingRemotePointerPosition,
    now_millis: u64,
) -> bool {
    now_millis.saturating_sub(pending.observed_millis) >= RAW_FALLBACK_DELAY_MILLIS
}

fn clear_pending_remote_pointer_position_after_raw_delta(
    pending_remote_pointer_position: &mut Option<PendingRemotePointerPosition>,
    last_pointer: &mut Option<Point>,
) -> bool {
    let Some(pending) = pending_remote_pointer_position.take() else {
        return false;
    };
    *last_pointer = Some(pending.point);
    true
}

#[allow(clippy::too_many_arguments)]
fn process_pending_remote_pointer_position(
    local_desktop: Rect,
    control_state: &mut Option<ControlState>,
    last_pointer: &mut Option<Point>,
    pending_pointer_park: &mut Option<Point>,
    pending_remote_pointer_position: &mut Option<PendingRemotePointerPosition>,
    mouse_diagnostics: &mut MouseDiagnostics,
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
    hook_manager: &Arc<Mutex<HookManager>>,
    remote_control_active: &Arc<AtomicBool>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    let Some(pending) = *pending_remote_pointer_position else {
        return;
    };
    let now = now_millis();
    if !pending_remote_pointer_position_due(pending, now) {
        return;
    }

    *pending_remote_pointer_position = None;

    if mouse_diagnostics.raw_delta_recent(now) {
        *last_pointer = Some(pending.point);
        return;
    }

    let Some(state) = control_state.as_mut() else {
        *last_pointer = Some(pending.point);
        return;
    };
    if state.mode() != ControlMode::Remote {
        *last_pointer = Some(pending.point);
        return;
    }

    match remote_pointer_position_action(pending.point, local_desktop, last_pointer, false) {
        RemotePointerPositionAction::TrackOnly { repark_after_move } => {
            if repark_after_move {
                park_local_pointer_for_remote_control(
                    local_desktop,
                    last_pointer,
                    pending_pointer_park,
                    mouse_diagnostics,
                    hook_manager,
                    updates,
                    session_id,
                );
            }
        }
        RemotePointerPositionAction::Delta {
            dx,
            dy,
            repark_after_move,
        } => {
            let output = state.apply_remote_delta(dx, dy);
            handle_control_output(
                output,
                local_desktop,
                last_pointer,
                pending_pointer_park,
                mouse_diagnostics,
                repark_after_move,
                remote_input_send_buffer,
                hook_manager,
                remote_control_active,
                connection_commands,
                updates,
                session_id,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn send_remote_input(
    input: InputEvent,
    local_desktop: Rect,
    control_state: &mut Option<ControlState>,
    last_pointer: &mut Option<Point>,
    pending_pointer_park: &mut Option<Point>,
    pending_remote_pointer_position: &mut Option<PendingRemotePointerPosition>,
    mouse_diagnostics: &mut MouseDiagnostics,
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
    hook_manager: &Arc<Mutex<HookManager>>,
    remote_control_active: &Arc<AtomicBool>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    match input {
        InputEvent::MouseMoveAbs(_) => {}
        InputEvent::MouseMoveDelta(delta) => {
            let _ = clear_pending_remote_pointer_position_after_raw_delta(
                pending_remote_pointer_position,
                last_pointer,
            );
            mouse_diagnostics.record_raw_delta(delta.dx, delta.dy, now_millis());
            if let Some(state) = control_state.as_mut() {
                let output = state.apply_remote_delta(delta.dx, delta.dy);
                handle_control_output(
                    output,
                    local_desktop,
                    last_pointer,
                    pending_pointer_park,
                    mouse_diagnostics,
                    false,
                    remote_input_send_buffer,
                    hook_manager,
                    remote_control_active,
                    connection_commands,
                    updates,
                    session_id,
                );
            }
        }
        InputEvent::ReleaseAll => emit_remote_send_actions(
            remote_input_send_buffer.send_release_all(),
            connection_commands,
            updates,
            session_id,
        ),
        event => {
            emit_remote_send_actions(
                remote_input_send_buffer.send_reliable_input(event),
                connection_commands,
                updates,
                session_id,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_control_output(
    output: ControlOutput,
    local_desktop: Rect,
    last_pointer: &mut Option<Point>,
    pending_pointer_park: &mut Option<Point>,
    mouse_diagnostics: &mut MouseDiagnostics,
    repark_after_move: bool,
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
    hook_manager: &Arc<Mutex<HookManager>>,
    remote_control_active: &Arc<AtomicBool>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    let return_local_target = return_local_pointer_target(&output);

    match output {
        ControlOutput::None => {}
        ControlOutput::EnterRemote(point) => {
            remote_control_active.store(true, Ordering::SeqCst);
            mouse_diagnostics.record_remote_move(point);
            emit_remote_send_actions(
                remote_input_send_buffer.send_pointer(point, now_millis()),
                connection_commands,
                updates,
                session_id,
            );
            park_local_pointer_for_remote_control(
                local_desktop,
                last_pointer,
                pending_pointer_park,
                mouse_diagnostics,
                hook_manager,
                updates,
                session_id,
            );
            set_hook_suppression(hook_manager, SuppressionMode::Suppress);
            send_session_update(
                updates,
                session_id,
                SessionUpdate::Log("entered remote control".to_string()),
            );
            send_session_update(
                updates,
                session_id,
                SessionUpdate::RunState(RunState::RemoteControl),
            );
        }
        ControlOutput::MoveRemote(point) => {
            mouse_diagnostics.record_remote_move(point);
            emit_remote_send_actions(
                remote_input_send_buffer.send_pointer(point, now_millis()),
                connection_commands,
                updates,
                session_id,
            );
            if repark_after_move {
                park_local_pointer_for_remote_control(
                    local_desktop,
                    last_pointer,
                    pending_pointer_park,
                    mouse_diagnostics,
                    hook_manager,
                    updates,
                    session_id,
                );
            }
        }
        ControlOutput::ReturnLocal(_) => {
            mouse_diagnostics.record_return_local();
            let Some(point) = return_local_target else {
                return;
            };
            set_hook_suppression(hook_manager, SuppressionMode::PassThrough);
            *last_pointer = Some(point);
            *pending_pointer_park = Some(point);
            if let Err(error) = move_local_pointer_to(local_desktop, point) {
                *pending_pointer_park = None;
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::Error(format!("local pointer move failed: {error}")),
                );
            }
            emit_return_local_release_all(
                remote_input_send_buffer,
                remote_control_active,
                |actions| {
                    emit_remote_send_actions(actions, connection_commands, updates, session_id);
                },
            );
            send_session_update(
                updates,
                session_id,
                SessionUpdate::Log("returned to local control".to_string()),
            );
            send_session_update(
                updates,
                session_id,
                SessionUpdate::RunState(RunState::LocalControl),
            );
        }
    }
}

fn return_local_pointer_target(output: &ControlOutput) -> Option<Point> {
    match output {
        ControlOutput::ReturnLocal(point) => Some(*point),
        ControlOutput::None | ControlOutput::EnterRemote(_) | ControlOutput::MoveRemote(_) => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_event_pump(
    session_id: u64,
    config: AppConfig,
    local_desktop: Rect,
    mut connection_events: mpsc::UnboundedReceiver<ConnectionEvent>,
    connection_commands: mpsc::UnboundedSender<ConnectionCommand>,
    mut session_commands: mpsc::UnboundedReceiver<SessionCommand>,
    bulk_commands: Vec<mpsc::UnboundedSender<BulkTransferCommand>>,
    updates: Sender<TaggedSessionUpdate>,
) {
    let mut injector: Option<InputInjector> = None;
    let mut heartbeat = HeartbeatTracker::default();
    let mut clipboard_interval = interval(CLIPBOARD_POLL_INTERVAL);
    let mut clipboard_runtime = start_clipboard_runtime(&config, &updates, session_id);
    let mut clipboard_transport_ready = false;
    let mut drag_runtime = start_drag_drop_runtime(&config);
    let mut drag_drop = DragDropCoordinator::new(Role::Agent);
    let mut drag_interval = interval(DRAG_DROP_POLL_INTERVAL);
    clipboard_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    drag_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = drag_interval.tick(), if drag_runtime.is_some() => {
                drain_local_drag_events(
                    &mut drag_drop,
                    &mut drag_runtime,
                    &connection_commands,
                    &bulk_commands,
                    &updates,
                    session_id,
                );
            }
            _ = clipboard_interval.tick(), if clipboard_runtime.is_some() => {
                if let Some(clipboard) = &clipboard_runtime {
                    drain_clipboard_events(
                        &clipboard.events,
                        &config,
                        &connection_commands,
                        &bulk_commands,
                        &updates,
                        session_id,
                        clipboard_transport_ready,
                    );
                }
            }
            command = session_commands.recv() => {
                match command {
                    Some(SessionCommand::Stop) | None => {
                        release_agent_input(&mut injector, &updates, session_id, false);
                        stop_clipboard_runtime(clipboard_runtime.take(), &updates, session_id);
                        break;
                    }
                }
            }
            event = connection_events.recv() => {
                let Some(event) = event else {
                    break;
                };

                let readiness_event = event.clone();
                if let ConnectionEvent::Message(message) = &event {
                    let effects = drag_drop.handle_peer_message(message.clone());
                    apply_drag_effects(
                        effects,
                        &mut drag_runtime,
                        &connection_commands,
                        &bulk_commands,
                        &updates,
                        session_id,
                    );
                }
                if matches!(
                    event,
                    ConnectionEvent::Disconnected(_) | ConnectionEvent::Error(_)
                ) {
                    drag_drop.reset();
                }
                handle_agent_connection_event(
                    event,
                    &config,
                    local_desktop,
                    &mut injector,
                    &connection_commands,
                    &updates,
                    session_id,
                    &mut heartbeat,
                );
                clipboard_transport_ready = agent_clipboard_transport_ready_after_event(
                    clipboard_transport_ready,
                    &readiness_event,
                );
            }
        }
    }

    stop_clipboard_runtime(clipboard_runtime.take(), &updates, session_id);
    release_agent_input(&mut injector, &updates, session_id, false);
}

#[allow(clippy::too_many_arguments)]
fn handle_agent_connection_event(
    event: ConnectionEvent,
    config: &AppConfig,
    local_desktop: Rect,
    injector: &mut Option<InputInjector>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
    heartbeat: &mut HeartbeatTracker,
) {
    send_session_update(
        updates,
        session_id,
        SessionUpdate::Connection(event.clone()),
    );

    match event {
        ConnectionEvent::Connected { .. } => {
            *injector = Some(InputInjector::new(local_desktop));
            let _ = connection_commands.send(ConnectionCommand::Send(WireMessage::Hello(
                Hello {
                    protocol_version: PROTOCOL_VERSION,
                    desktop: local_desktop,
                },
            )));
        }
        ConnectionEvent::Disconnected(_) => {
            release_agent_input(injector, updates, session_id, true);
            *injector = None;
        }
        ConnectionEvent::Message(WireMessage::Input(input)) => {
            inject_agent_input(injector, input, updates, session_id);
        }
        ConnectionEvent::Message(WireMessage::ReleaseAll) => {
            release_agent_input(injector, updates, session_id, false);
        }
        ConnectionEvent::Message(WireMessage::Heartbeat(message)) => {
            handle_heartbeat_message(message, heartbeat, connection_commands, updates, session_id);
        }
        ConnectionEvent::Message(WireMessage::ClipboardOffer(envelope)) => {
            handle_remote_clipboard_message(
                WireMessage::ClipboardOffer(envelope),
                config,
                updates,
                session_id,
            );
        }
        ConnectionEvent::Message(WireMessage::ClipboardData(envelope)) => {
            handle_remote_clipboard_message(
                WireMessage::ClipboardData(envelope),
                config,
                updates,
                session_id,
            );
        }
        ConnectionEvent::Message(WireMessage::FileTransferOffer(manifest)) => {
            send_session_update(
                updates,
                session_id,
                SessionUpdate::ClipboardFileTransferStarted(manifest.transfer_id),
            );
        }
        ConnectionEvent::Message(WireMessage::Error(error)) => {
            send_session_update(updates, session_id, SessionUpdate::Error(error));
        }
        ConnectionEvent::Error(_) => {
            release_agent_input(injector, updates, session_id, true);
            *injector = None;
        }
        ConnectionEvent::Waiting
        | ConnectionEvent::Connecting(_)
        | ConnectionEvent::Message(WireMessage::Hello(_))
        | ConnectionEvent::Message(WireMessage::FileTransferProgress { .. })
        | ConnectionEvent::Message(WireMessage::FileTransferComplete { .. })
        // Drag-drop messages are consumed by DragDropCoordinator in the pump.
        | ConnectionEvent::Message(WireMessage::PeerLayout { .. })
        | ConnectionEvent::Message(WireMessage::DragDropReleased { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTargetResolved { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTargetFailed { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTransferStarted { .. })
        | ConnectionEvent::Message(WireMessage::DragDropTransferResult { .. })
        | ConnectionEvent::Message(WireMessage::DragDropCancel { .. })
        | ConnectionEvent::Message(WireMessage::DragDropEntered { .. })
         => {}
    }
}

fn agent_clipboard_transport_ready_after_event(current: bool, event: &ConnectionEvent) -> bool {
    match event {
        ConnectionEvent::Connected { .. } => true,
        ConnectionEvent::Waiting
        | ConnectionEvent::Connecting(_)
        | ConnectionEvent::Disconnected(_)
        | ConnectionEvent::Error(_)
        | ConnectionEvent::Message(WireMessage::Error(_)) => false,
        _ => current,
    }
}

fn inject_agent_input(
    injector: &mut Option<InputInjector>,
    input: InputEvent,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    let Some(injector) = injector.as_mut() else {
        send_session_update(
            updates,
            session_id,
            SessionUpdate::Log("dropped input before agent connection was ready".to_string()),
        );
        return;
    };

    let result = match input {
        InputEvent::ReleaseAll => injector.release_all(),
        event => injector.inject(&event),
    };

    if let Err(error) = result {
        send_session_update(
            updates,
            session_id,
            SessionUpdate::Error(format!("input injection failed: {error}")),
        );
    }
}

fn release_agent_input(
    injector: &mut Option<InputInjector>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
    log_after_disconnect: bool,
) {
    if let Some(injector) = injector.as_mut() {
        if let Err(error) = injector.release_all() {
            send_session_update(
                updates,
                session_id,
                SessionUpdate::Error(format!("release all failed: {error}")),
            );
        } else if log_after_disconnect {
            send_session_update(
                updates,
                session_id,
                SessionUpdate::Log("released all pressed input after disconnect".to_string()),
            );
        }
    }
}

fn handle_heartbeat_message(
    message: Heartbeat,
    heartbeat: &mut HeartbeatTracker,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    let now = now_millis();
    if let Some(rtt) = heartbeat.receive(message.sent_millis, now) {
        send_session_update(updates, session_id, SessionUpdate::Rtt(rtt));
    } else {
        let _ = connection_commands.send(ConnectionCommand::Send(WireMessage::Heartbeat(message)));
    }
}

fn send_heartbeat(
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    heartbeat: &mut HeartbeatTracker,
) {
    let sent_millis = now_millis();
    heartbeat.sent(sent_millis);
    let _ = connection_commands.send(ConnectionCommand::Send(WireMessage::Heartbeat(Heartbeat {
        sent_millis,
    })));
}

fn send_release_all(connection_commands: &mpsc::UnboundedSender<ConnectionCommand>) {
    let _ = connection_commands.send(ConnectionCommand::Send(WireMessage::ReleaseAll));
}

fn set_hook_suppression(hook_manager: &Arc<Mutex<HookManager>>, suppression_mode: SuppressionMode) {
    if let Ok(manager) = hook_manager.lock() {
        manager.set_suppression_mode(suppression_mode);
    }
}

fn mark_local_pointer_parked_for_remote_control(
    local_desktop: Rect,
    last_pointer: &mut Option<Point>,
) -> Point {
    let anchor = Point::new(
        local_desktop.left.saturating_add(local_desktop.width / 2),
        local_desktop.top.saturating_add(local_desktop.height / 2),
    );
    *last_pointer = Some(anchor);
    anchor
}

fn consume_pending_pointer_park(
    point: Point,
    last_pointer: &mut Option<Point>,
    pending_pointer_park: &mut Option<Point>,
) -> bool {
    let Some(anchor) = pending_pointer_park.take() else {
        return false;
    };

    if point_within_tolerance(point, anchor, PARK_POINTER_TOLERANCE_PX) {
        *last_pointer = Some(anchor);
        true
    } else {
        false
    }
}

fn point_within_tolerance(point: Point, target: Point, tolerance: i32) -> bool {
    point.x.abs_diff(target.x) <= tolerance as u32 && point.y.abs_diff(target.y) <= tolerance as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemotePointerPositionAction {
    TrackOnly {
        repark_after_move: bool,
    },
    Delta {
        dx: i32,
        dy: i32,
        repark_after_move: bool,
    },
}

fn remote_pointer_position_action(
    point: Point,
    local_desktop: Rect,
    last_pointer: &mut Option<Point>,
    raw_delta_recent: bool,
) -> RemotePointerPositionAction {
    let previous = last_pointer.replace(point);
    let repark_after_move = should_repark_local_pointer_for_remote_control(point, local_desktop);

    if raw_delta_recent {
        return RemotePointerPositionAction::TrackOnly { repark_after_move };
    }

    let Some(previous) = previous else {
        return RemotePointerPositionAction::TrackOnly { repark_after_move };
    };
    let dx = signed_delta(previous.x, point.x);
    let dy = signed_delta(previous.y, point.y);
    if dx == 0 && dy == 0 {
        RemotePointerPositionAction::TrackOnly { repark_after_move }
    } else {
        RemotePointerPositionAction::Delta {
            dx,
            dy,
            repark_after_move,
        }
    }
}

fn signed_delta(previous: i32, next: i32) -> i32 {
    (i64::from(next) - i64::from(previous)).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn should_repark_local_pointer_for_remote_control(point: Point, local_desktop: Rect) -> bool {
    if !local_desktop.is_valid() {
        return false;
    }

    point.x
        <= local_desktop
            .left
            .saturating_add(PARK_POINTER_EDGE_MARGIN_PX)
        || point.x
            >= local_desktop
                .right()
                .saturating_sub(PARK_POINTER_EDGE_MARGIN_PX)
        || point.y
            <= local_desktop
                .top
                .saturating_add(PARK_POINTER_EDGE_MARGIN_PX)
        || point.y
            >= local_desktop
                .bottom()
                .saturating_sub(PARK_POINTER_EDGE_MARGIN_PX)
}

fn park_local_pointer_for_remote_control(
    local_desktop: Rect,
    last_pointer: &mut Option<Point>,
    pending_pointer_park: &mut Option<Point>,
    mouse_diagnostics: &mut MouseDiagnostics,
    hook_manager: &Arc<Mutex<HookManager>>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    mouse_diagnostics.record_pointer_park_attempt();
    let anchor = mark_local_pointer_parked_for_remote_control(local_desktop, last_pointer);
    *pending_pointer_park = Some(anchor);
    set_hook_suppression(hook_manager, SuppressionMode::PassThrough);
    if let Err(error) = move_local_pointer_to(local_desktop, anchor) {
        *pending_pointer_park = None;
        send_session_update(
            updates,
            session_id,
            SessionUpdate::Error(format!("local pointer park failed: {error}")),
        );
    }
    set_hook_suppression(hook_manager, SuppressionMode::Suppress);
}

#[derive(Default)]
struct MouseDiagnostics {
    hook_positions_local: u64,
    hook_positions_remote: u64,
    raw_deltas: u64,
    remote_moves: u64,
    pointer_park_attempts: u64,
    pointer_park_consumed: u64,
    pointer_park_missed: u64,
    return_local: u64,
    last_raw_delta: Option<(i32, i32)>,
    last_raw_delta_millis: Option<u64>,
    last_local_pointer: Option<Point>,
    last_remote_pointer: Option<Point>,
    last_emit_millis: Option<u64>,
}

impl MouseDiagnostics {
    fn record_hook_position(&mut self, point: Point, remote_mode: bool) {
        if remote_mode {
            self.hook_positions_remote = self.hook_positions_remote.saturating_add(1);
        } else {
            self.hook_positions_local = self.hook_positions_local.saturating_add(1);
        }
        self.last_local_pointer = Some(point);
    }

    fn record_raw_delta(&mut self, dx: i32, dy: i32, now_millis: u64) {
        self.raw_deltas = self.raw_deltas.saturating_add(1);
        self.last_raw_delta = Some((dx, dy));
        self.last_raw_delta_millis = Some(now_millis);
    }

    fn record_remote_move(&mut self, point: Point) {
        self.remote_moves = self.remote_moves.saturating_add(1);
        self.last_remote_pointer = Some(point);
    }

    fn record_pointer_park_attempt(&mut self) {
        self.pointer_park_attempts = self.pointer_park_attempts.saturating_add(1);
    }

    fn record_pointer_park_consumed(&mut self) {
        self.pointer_park_consumed = self.pointer_park_consumed.saturating_add(1);
    }

    fn record_pointer_park_missed(&mut self) {
        self.pointer_park_missed = self.pointer_park_missed.saturating_add(1);
    }

    fn record_return_local(&mut self) {
        self.return_local = self.return_local.saturating_add(1);
    }

    fn raw_delta_recent(&self, now_millis: u64) -> bool {
        self.last_raw_delta_millis
            .is_some_and(|last| now_millis.saturating_sub(last) <= RAW_DELTA_RECENT_MILLIS)
    }

    fn summary_if_due(&mut self, now_millis: u64) -> Option<String> {
        if self.last_emit_millis.is_some_and(|last| {
            now_millis.saturating_sub(last) < MOUSE_DIAGNOSTICS_EMIT_INTERVAL_MILLIS
        }) {
            return None;
        }

        self.last_emit_millis = Some(now_millis);
        Some(self.summary())
    }

    fn summary(&self) -> String {
        format!(
            "hook local={}, hook remote={}, raw={}, sent={}, parks={}/{}/{}, returns={}, last delta={}, local={}, remote={}",
            self.hook_positions_local,
            self.hook_positions_remote,
            self.raw_deltas,
            self.remote_moves,
            self.pointer_park_attempts,
            self.pointer_park_consumed,
            self.pointer_park_missed,
            self.return_local,
            format_delta(self.last_raw_delta),
            format_point(self.last_local_pointer),
            format_point(self.last_remote_pointer)
        )
    }
}

fn format_delta(delta: Option<(i32, i32)>) -> String {
    delta
        .map(|(dx, dy)| format!("{dx},{dy}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_point(point: Option<Point>) -> String {
    point
        .map(|point| format!("{},{}", point.x, point.y))
        .unwrap_or_else(|| "-".to_string())
}

struct ClipboardRuntime {
    monitor: ClipboardMonitor,
    events: Receiver<ClipboardEvent>,
}

#[derive(Debug, PartialEq, Eq)]
enum ClipboardSendDecision {
    Send { format: &'static str, bytes: u64 },
    Ignore(String),
}

#[derive(Debug, PartialEq, Eq)]
enum RemoteClipboardMessageAction {
    Write {
        envelope: ClipboardEnvelope,
        format: &'static str,
        bytes: u64,
    },
    Ignore(String),
}

#[cfg(test)]
enum ClipboardStatusUpdate<'a> {
    Queued(&'a ClipboardPayload),
    Ignored(String),
    Error(String),
}

fn start_clipboard_runtime(
    config: &AppConfig,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) -> Option<ClipboardRuntime> {
    if !clipboard_enabled(config) {
        return None;
    }

    let (sender, events) = unbounded();
    match ClipboardMonitor::start_with_options(sender, clipboard_read_options(config)) {
        Ok(monitor) => {
            send_session_update(
                updates,
                session_id,
                SessionUpdate::Log("clipboard monitor started".to_string()),
            );
            Some(ClipboardRuntime { monitor, events })
        }
        Err(error) => {
            send_session_update(
                updates,
                session_id,
                SessionUpdate::ClipboardError(format!("clipboard monitor failed: {error}")),
            );
            None
        }
    }
}

fn stop_clipboard_runtime(
    clipboard: Option<ClipboardRuntime>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    let Some(clipboard) = clipboard else {
        return;
    };

    if let Err(error) = clipboard.monitor.stop() {
        send_session_update(
            updates,
            session_id,
            SessionUpdate::ClipboardError(format!("clipboard monitor stop failed: {error}")),
        );
    }
}

fn drain_clipboard_events(
    receiver: &Receiver<ClipboardEvent>,
    config: &AppConfig,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    bulk_commands: &[mpsc::UnboundedSender<BulkTransferCommand>],
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
    clipboard_transport_ready: bool,
) {
    while let Ok(event) = receiver.try_recv() {
        handle_clipboard_event(
            event,
            config,
            connection_commands,
            bulk_commands,
            updates,
            session_id,
            clipboard_transport_ready,
        );
    }
}

fn handle_clipboard_event(
    event: ClipboardEvent,
    config: &AppConfig,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    bulk_commands: &[mpsc::UnboundedSender<BulkTransferCommand>],
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
    clipboard_transport_ready: bool,
) {
    match event {
        ClipboardEvent::Changed(envelope) => {
            if !clipboard_transport_ready {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::ClipboardIgnored(
                        CLIPBOARD_PEER_NOT_CONNECTED_REASON.to_string(),
                    ),
                );
                return;
            }

            match clipboard_data_send_decision(config, &envelope) {
                ClipboardSendDecision::Send { format, bytes } => {
                    if let ClipboardPayload::Files(offer) = &envelope.payload {
                        handle_local_clipboard_file_offer(
                            config,
                            offer,
                            bytes,
                            connection_commands,
                            bulk_commands,
                            updates,
                            session_id,
                        );
                        return;
                    }

                    if connection_commands
                        .send(ConnectionCommand::Send(WireMessage::ClipboardData(
                            envelope,
                        )))
                        .is_ok()
                    {
                        send_session_update(
                            updates,
                            session_id,
                            SessionUpdate::ClipboardQueued {
                                format: format.to_string(),
                                bytes,
                            },
                        );
                    } else {
                        send_session_update(
                            updates,
                            session_id,
                            SessionUpdate::ClipboardError(
                                "clipboard send failed: connection command channel closed"
                                    .to_string(),
                            ),
                        );
                    }
                }
                ClipboardSendDecision::Ignore(reason) => {
                    send_session_update(
                        updates,
                        session_id,
                        SessionUpdate::ClipboardIgnored(reason),
                    );
                }
            }
        }
        ClipboardEvent::Ignored(reason) => {
            send_session_update(updates, session_id, SessionUpdate::ClipboardIgnored(reason));
        }
        ClipboardEvent::Error(error) => {
            send_session_update(updates, session_id, SessionUpdate::ClipboardError(error));
        }
    }
}

fn handle_local_clipboard_file_offer(
    config: &AppConfig,
    offer: &RemoteFileOffer,
    bytes: u64,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    bulk_commands: &[mpsc::UnboundedSender<BulkTransferCommand>],
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    let Some(commands) = bulk_commands.last() else {
        send_session_update(
            updates,
            session_id,
            SessionUpdate::ClipboardError(
                "file copy/paste failed: bulk transfer channel is not available".to_string(),
            ),
        );
        return;
    };

    match file_transfer_request_from_clipboard_offer(offer) {
        Ok((manifest, source_paths)) => {
            if manifest.total_bytes > config.sharing.max_file_transfer_bytes {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::ClipboardIgnored(format!(
                        "clipboard files are {} bytes, file transfer limit is {}",
                        manifest.total_bytes, config.sharing.max_file_transfer_bytes
                    )),
                );
                return;
            }
            if connection_commands
                .send(ConnectionCommand::Send(WireMessage::FileTransferOffer(
                    manifest.clone(),
                )))
                .is_err()
            {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::ClipboardError(
                        "file copy/paste failed: connection command channel closed".to_string(),
                    ),
                );
                return;
            }
            let send_result = commands.send(BulkTransferCommand::SendFiles {
                manifest: manifest.clone(),
                source_paths,
            });
            if send_result.is_ok() {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::ClipboardQueued {
                        format: "files".to_string(),
                        bytes: manifest.total_bytes.max(bytes),
                    },
                );
            } else {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::ClipboardError(
                        "file copy/paste failed: bulk transfer command channel closed".to_string(),
                    ),
                );
            }
        }
        Err(error) => send_session_update(
            updates,
            session_id,
            SessionUpdate::ClipboardError(format!("file copy/paste failed: {error}")),
        ),
    }
}

fn file_transfer_request_from_clipboard_offer(
    offer: &RemoteFileOffer,
) -> Result<
    (
        borderless_core::file_transfer::FileTransferManifest,
        Vec<String>,
    ),
    String,
> {
    if offer.files.is_empty() {
        return Err("clipboard file list is empty".to_string());
    }

    let source_paths = offer
        .files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<Vec<_>>();
    let manifest = manifest_from_source_paths(offer.transfer_id, &source_paths)
        .map_err(|error| error.to_string())?;

    Ok((manifest, source_paths))
}

fn handle_remote_clipboard_message(
    message: WireMessage,
    config: &AppConfig,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    match remote_clipboard_message_action(config, message) {
        RemoteClipboardMessageAction::Write {
            envelope,
            format,
            bytes,
        } => match write_clipboard(&envelope.payload) {
            Ok(()) => {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::ClipboardWritten {
                        format: format.to_string(),
                        bytes,
                    },
                );
            }
            Err(error) => {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::ClipboardError(format!("clipboard write failed: {error}")),
                );
            }
        },
        RemoteClipboardMessageAction::Ignore(reason) => {
            send_session_update(updates, session_id, SessionUpdate::ClipboardIgnored(reason));
        }
    }
}

fn remote_clipboard_message_action(
    config: &AppConfig,
    message: WireMessage,
) -> RemoteClipboardMessageAction {
    let WireMessage::ClipboardData(envelope) = message else {
        return RemoteClipboardMessageAction::Ignore(
            "clipboard offer ignored: data message required".to_string(),
        );
    };

    match clipboard_data_write_decision(config, &envelope) {
        ClipboardSendDecision::Send { format, bytes } => RemoteClipboardMessageAction::Write {
            envelope,
            format,
            bytes,
        },
        ClipboardSendDecision::Ignore(reason) => RemoteClipboardMessageAction::Ignore(reason),
    }
}

fn clipboard_enabled(config: &AppConfig) -> bool {
    config.sharing.clipboard_text
        || config.sharing.clipboard_html
        || config.sharing.clipboard_images
        || config.sharing.file_copy_paste
}

fn clipboard_read_options(config: &AppConfig) -> ClipboardReadOptions {
    ClipboardReadOptions {
        text: config.sharing.clipboard_text,
        html: config.sharing.clipboard_html,
        images: config.sharing.clipboard_images,
        files: config.sharing.file_copy_paste,
        max_bytes: config
            .sharing
            .max_clipboard_bytes
            .min(MAX_PAYLOAD_LEN as u64),
    }
}

fn clipboard_payload_format(payload: &ClipboardPayload) -> &'static str {
    match payload {
        ClipboardPayload::UnicodeText(_) => "text",
        ClipboardPayload::Html(_) => "html",
        ClipboardPayload::ImagePng(_) => "png",
        ClipboardPayload::ImageDib(_) => "dib",
        ClipboardPayload::Files(_) => "files",
    }
}

fn clipboard_wire_bytes(payload: &ClipboardPayload) -> u64 {
    match payload {
        ClipboardPayload::UnicodeText(value) | ClipboardPayload::Html(value) => value.len() as u64,
        ClipboardPayload::ImagePng(bytes) | ClipboardPayload::ImageDib(bytes) => bytes.len() as u64,
        ClipboardPayload::Files(offer) => clipboard_file_path_metadata_bytes(offer),
    }
}

fn clipboard_file_path_metadata_bytes(offer: &borderless_core::clipboard::RemoteFileOffer) -> u64 {
    // The reliable control channel carries CF_HDROP-style metadata; file bytes move on the bulk channel.
    offer.files.iter().fold(0u64, |total, file| {
        total.saturating_add(file.relative_path.len() as u64)
    })
}

fn clipboard_send_decision(
    config: &AppConfig,
    payload: &ClipboardPayload,
) -> ClipboardSendDecision {
    if let Some(reason) = clipboard_disabled_reason(config, payload) {
        return ClipboardSendDecision::Ignore(reason);
    }

    let format = clipboard_payload_format(payload);
    if let ClipboardPayload::Files(offer) = payload {
        let bytes = offer
            .files
            .iter()
            .fold(0u64, |total, file| total.saturating_add(file.size_bytes));
        let limit = config.sharing.max_file_transfer_bytes;
        if bytes > limit {
            return ClipboardSendDecision::Ignore(format!(
                "clipboard files are {bytes} bytes, file transfer limit is {limit}"
            ));
        }
    }

    let bytes = clipboard_wire_bytes(payload);
    let limit = config.sharing.max_clipboard_bytes;
    if bytes > limit {
        ClipboardSendDecision::Ignore(format!(
            "clipboard {format} is {bytes} bytes, limit is {limit}"
        ))
    } else {
        ClipboardSendDecision::Send { format, bytes }
    }
}

fn clipboard_data_send_decision(
    config: &AppConfig,
    envelope: &ClipboardEnvelope,
) -> ClipboardSendDecision {
    clipboard_protocol_decision(
        config,
        envelope,
        WireMessage::ClipboardData(envelope.clone()),
    )
}

fn clipboard_data_write_decision(
    config: &AppConfig,
    envelope: &ClipboardEnvelope,
) -> ClipboardSendDecision {
    clipboard_protocol_decision(
        config,
        envelope,
        WireMessage::ClipboardData(envelope.clone()),
    )
}

fn clipboard_protocol_decision(
    config: &AppConfig,
    envelope: &ClipboardEnvelope,
    message: WireMessage,
) -> ClipboardSendDecision {
    match clipboard_send_decision(config, &envelope.payload) {
        ClipboardSendDecision::Send { format, bytes } => match encode_frame(0, &message) {
            Ok(_) => ClipboardSendDecision::Send { format, bytes },
            Err(error) => {
                ClipboardSendDecision::Ignore(clipboard_protocol_ignored_reason(format, error))
            }
        },
        ClipboardSendDecision::Ignore(reason) => ClipboardSendDecision::Ignore(reason),
    }
}

fn clipboard_protocol_ignored_reason(format: &'static str, error: ProtocolError) -> String {
    match error {
        ProtocolError::PayloadTooLarge { max, actual } => format!(
            "clipboard {format} encoded message is too large: {actual} bytes, protocol limit is {max}"
        ),
        error => format!("clipboard {format} encoded message cannot be sent: {error}"),
    }
}

fn clipboard_disabled_reason(config: &AppConfig, payload: &ClipboardPayload) -> Option<String> {
    match payload {
        ClipboardPayload::UnicodeText(_) if !config.sharing.clipboard_text => {
            Some("clipboard text sharing is disabled".to_string())
        }
        ClipboardPayload::Html(_) if !config.sharing.clipboard_html => {
            Some("clipboard html sharing is disabled".to_string())
        }
        ClipboardPayload::ImagePng(_) | ClipboardPayload::ImageDib(_)
            if !config.sharing.clipboard_images =>
        {
            Some("clipboard image sharing is disabled".to_string())
        }
        ClipboardPayload::Files(_) if !config.sharing.file_copy_paste => {
            Some("clipboard file copy/paste is disabled".to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
fn apply_clipboard_status_update(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    update: ClipboardStatusUpdate<'_>,
) {
    match update {
        ClipboardStatusUpdate::Queued(payload) => {
            apply_clipboard_queued_status(
                status,
                events,
                clipboard_payload_format(payload).to_string(),
                clipboard_wire_bytes(payload),
            );
        }
        ClipboardStatusUpdate::Ignored(reason) => {
            apply_clipboard_ignored_status(status, events, reason);
        }
        ClipboardStatusUpdate::Error(error) => {
            apply_clipboard_error_status(status, events, error);
        }
    }

    emit_status(events, status);
}

fn apply_clipboard_queued_status(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    format: String,
    bytes: u64,
) {
    status.last_clipboard_format = Some(format.clone());
    status.last_clipboard_bytes = Some(bytes);
    status.clipboard_ignored_reason = None;
    emit_log(
        status,
        events,
        format!("clipboard queued: {format} ({bytes} bytes)"),
    );
}

fn apply_clipboard_written_status(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    format: String,
    bytes: u64,
) {
    status.last_clipboard_format = Some(format.clone());
    status.last_clipboard_bytes = Some(bytes);
    status.clipboard_ignored_reason = None;
    emit_log(
        status,
        events,
        format!("clipboard written: {format} ({bytes} bytes)"),
    );
}

fn apply_clipboard_ignored_status(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    reason: String,
) {
    status.clipboard_ignored_reason = Some(reason.clone());
    emit_log(status, events, format!("clipboard ignored: {reason}"));
}

fn apply_clipboard_error_status(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    error: String,
) {
    status.last_error = Some(error.clone());
    status.clipboard_ignored_reason = Some(error.clone());
    emit_log(status, events, format!("clipboard error: {error}"));
}

// ---------------------------------------------------------------------------
// Targeted drag-drop coordination
//
// The native OLE drag never leaves the source machine. Dragging into the
// shared edge captures the source paths; crossing the edge switches to
// remote target selection (pointer only); releasing resolves a destination
// directory on the drop side and only then starts a bulk transfer that
// writes directly into that directory.
// ---------------------------------------------------------------------------

const DRAG_DROP_POLL_INTERVAL: Duration = Duration::from_millis(25);

struct DragDropRuntime {
    target: Option<EdgeDropTarget>,
    installed_edge: Option<Edge>,
    events: Receiver<DragDropEvent>,
    sender: Sender<DragDropEvent>,
}

impl DragDropRuntime {
    fn install_edge(&mut self, edge: Edge) -> anyhow::Result<()> {
        if self.installed_edge == Some(edge) {
            return Ok(());
        }
        self.target = None;
        self.installed_edge = None;
        self.target = Some(EdgeDropTarget::install(edge, self.sender.clone())?);
        self.installed_edge = Some(edge);
        Ok(())
    }

    fn uninstall_edge(&mut self) {
        self.target = None;
        self.installed_edge = None;
    }
}

fn start_drag_drop_runtime(config: &AppConfig) -> Option<DragDropRuntime> {
    if !config.sharing.file_drag_drop {
        return None;
    }
    let (sender, events) = unbounded();
    Some(DragDropRuntime {
        target: None,
        installed_edge: None,
        events,
        sender,
    })
}

fn drain_local_drag_events(
    coordinator: &mut DragDropCoordinator,
    drag_runtime: &mut Option<DragDropRuntime>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    bulk_commands: &[mpsc::UnboundedSender<BulkTransferCommand>],
    updates: &Sender<TaggedSessionUpdate>,
    runtime_session_id: u64,
) {
    let events = drag_runtime
        .as_ref()
        .map(|runtime| runtime.events.try_iter().collect::<Vec<_>>())
        .unwrap_or_default();

    for event in events {
        let effects = match event {
            DragDropEvent::LocalFileDragEntered {
                session_id,
                transfer_id,
                paths,
            } => coordinator.begin_local_drag(session_id, transfer_id, paths),
            DragDropEvent::LocalDragCancelled { session_id } => {
                coordinator.cancel_local_drag(session_id, "本机拖拽已取消")
            }
            DragDropEvent::LocalDropReleased { session_id } => {
                coordinator.native_drop_released(session_id)
            }
            DragDropEvent::Error { message, .. } => vec![DragEffect::Status {
                state: format!("拖放失败：{message}"),
                destination: None,
            }],
        };
        apply_drag_effects(
            effects,
            drag_runtime,
            connection_commands,
            bulk_commands,
            updates,
            runtime_session_id,
        );
    }
}

fn apply_drag_effects(
    effects: Vec<DragEffect>,
    drag_runtime: &mut Option<DragDropRuntime>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    bulk_commands: &[mpsc::UnboundedSender<BulkTransferCommand>],
    updates: &Sender<TaggedSessionUpdate>,
    runtime_session_id: u64,
) {
    for effect in effects {
        match effect {
            DragEffect::InstallEdge(edge) => {
                let Some(runtime) = drag_runtime.as_mut() else {
                    continue;
                };
                if let Err(error) = runtime.install_edge(edge) {
                    send_drag_drop_status(
                        updates,
                        runtime_session_id,
                        format!("拖放边缘监听失败：{error}"),
                        None,
                    );
                }
            }
            DragEffect::UninstallEdge => {
                if let Some(runtime) = drag_runtime.as_mut() {
                    runtime.uninstall_edge();
                }
            }
            DragEffect::Send(message) => {
                let _ = connection_commands.send(ConnectionCommand::Send(message));
            }
            DragEffect::CancelBulk { transfer_id } => {
                for commands in bulk_commands {
                    let _ = commands.send(BulkTransferCommand::Cancel(transfer_id));
                }
            }
            DragEffect::Status { state, destination } => {
                send_drag_drop_status(updates, runtime_session_id, state, destination);
            }
            DragEffect::AwaitLocalRelease { .. } => {
                send_drag_drop_status(updates, runtime_session_id, "请在目标位置松开鼠标", None);
            }
            DragEffect::ResolveTarget { .. }
            | DragEffect::BeginTransfer { .. }
            | DragEffect::RevokeAuthorization { .. }
            | DragEffect::RestoreLocalInput => {}
        }
    }
}

fn send_drag_drop_status(
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
    state: impl Into<String>,
    destination: Option<String>,
) {
    send_session_update(
        updates,
        session_id,
        SessionUpdate::DragDropStatus {
            state: state.into(),
            destination,
        },
    );
}
fn send_session_update(
    sender: &Sender<TaggedSessionUpdate>,
    session_id: u64,
    update: SessionUpdate,
) {
    let _ = sender.send(TaggedSessionUpdate { session_id, update });
}

fn prepare_running_status(status: &mut AppStatus, config: &AppConfig, run_state: RunState) {
    status.reset_runtime_fields();
    status.run_state = run_state;
    status.clipboard_enabled = clipboard_enabled(config);
}

fn prepare_stopped_status(status: &mut AppStatus) {
    status.reset_runtime_fields();
    status.run_state = RunState::Stopped;
}

#[derive(Default)]
struct TransferPurposeTracker {
    clipboard_transfer_ids: HashSet<Uuid>,
    drag_transfer_ids: HashSet<Uuid>,
    pending_completed: Vec<PendingCompletedTransfer>,
}

impl TransferPurposeTracker {
    fn clear(&mut self) {
        self.clipboard_transfer_ids.clear();
        self.drag_transfer_ids.clear();
        self.pending_completed.clear();
    }

    fn remember_clipboard(&mut self, transfer_id: Uuid) -> Option<Vec<String>> {
        self.clipboard_transfer_ids.insert(transfer_id);
        self.take_pending_completed(transfer_id)
    }

    fn remember_unknown_completed(&mut self, transfer_id: Uuid, cache_paths: Vec<String>) {
        self.pending_completed
            .retain(|pending| pending.transfer_id != transfer_id);
        self.pending_completed.push(PendingCompletedTransfer {
            transfer_id,
            cache_paths,
        });
    }

    fn take_pending_completed(&mut self, transfer_id: Uuid) -> Option<Vec<String>> {
        let index = self
            .pending_completed
            .iter()
            .position(|pending| pending.transfer_id == transfer_id)?;
        Some(self.pending_completed.remove(index).cache_paths)
    }

    fn remember_drag(&mut self, transfer_id: Uuid) {
        self.drag_transfer_ids.insert(transfer_id);
    }

    fn is_drag(&self, transfer_id: Uuid) -> bool {
        self.drag_transfer_ids.contains(&transfer_id)
    }

    fn is_clipboard(&self, transfer_id: Uuid) -> bool {
        self.clipboard_transfer_ids.contains(&transfer_id)
    }

    fn forget(&mut self, transfer_id: Uuid) {
        self.clipboard_transfer_ids.remove(&transfer_id);
        self.drag_transfer_ids.remove(&transfer_id);
        self.pending_completed
            .retain(|pending| pending.transfer_id != transfer_id);
    }
}

struct PendingCompletedTransfer {
    transfer_id: Uuid,
    cache_paths: Vec<String>,
}

fn apply_session_update(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    update: SessionUpdate,
    transfer_purposes: &mut TransferPurposeTracker,
) {
    let mut status_changed = false;

    match update {
        SessionUpdate::Connection(event) => {
            status_changed = connection_event_updates_status(&event);
            apply_connection_event_to_status(status, &event);
            if let Some(message) = connection_event_log(&event) {
                emit_log(status, events, message);
            }
        }
        SessionUpdate::Error(error) => {
            status.run_state = RunState::Error;
            status.last_error = Some(error.clone());
            emit_log(status, events, format!("runtime error: {error}"));
            status_changed = true;
        }
        SessionUpdate::Log(message) => {
            emit_log(status, events, message);
        }
        SessionUpdate::Rtt(rtt) => {
            status.record_rtt(rtt);
            status_changed = true;
        }
        SessionUpdate::RunState(run_state) => {
            if status.run_state != run_state {
                status.run_state = run_state;
                status_changed = true;
            }
        }
        SessionUpdate::ClipboardQueued { format, bytes } => {
            apply_clipboard_queued_status(status, events, format, bytes);
            status_changed = true;
        }
        SessionUpdate::ClipboardWritten { format, bytes } => {
            apply_clipboard_written_status(status, events, format, bytes);
            status_changed = true;
        }
        SessionUpdate::ClipboardIgnored(reason) => {
            apply_clipboard_ignored_status(status, events, reason);
            status_changed = true;
        }
        SessionUpdate::ClipboardError(error) => {
            apply_clipboard_error_status(status, events, error);
            status_changed = true;
        }
        SessionUpdate::BulkTransfer(event) => {
            if let BulkTransferEvent::Offered(manifest) = &event {
                if matches!(
                    &manifest.destination,
                    FileTransferDestination::AuthorizedDrop { .. }
                ) {
                    transfer_purposes.remember_drag(manifest.transfer_id);
                    status.drag_drop_state = Some("receiving dropped files".to_string());
                }
            }
            match event {
                BulkTransferEvent::Completed {
                    transfer_id,
                    cache_paths,
                } => {
                    if transfer_purposes.is_drag(transfer_id) {
                        status.drag_drop_state = Some("drop completed".to_string());
                        apply_bulk_transfer_status_update(
                            status,
                            events,
                            BulkTransferEvent::Completed {
                                transfer_id,
                                cache_paths,
                            },
                            false,
                        );
                        transfer_purposes.forget(transfer_id);
                    } else if transfer_purposes.is_clipboard(transfer_id) {
                        apply_bulk_transfer_status_update(
                            status,
                            events,
                            BulkTransferEvent::Completed {
                                transfer_id,
                                cache_paths,
                            },
                            true,
                        );
                        transfer_purposes.forget(transfer_id);
                    } else {
                        status.transfer_active = false;
                        status.transfer_id = Some(transfer_id);
                        status.transfer_current_file = None;
                        emit_log(
                            status,
                            events,
                            format!("bulk transfer {transfer_id} completed, waiting for intent"),
                        );
                        transfer_purposes.remember_unknown_completed(transfer_id, cache_paths);
                    }
                }
                event => {
                    let transfer_id = bulk_transfer_event_id(&event);
                    let is_terminal = bulk_transfer_event_is_terminal(&event);
                    if let (Some(transfer_id), true) = (transfer_id, is_terminal) {
                        if transfer_purposes.is_drag(transfer_id) {
                            status.drag_drop_state = Some(match &event {
                                BulkTransferEvent::Cancelled(_) => "drop cancelled".to_string(),
                                BulkTransferEvent::Failed { error, .. } => {
                                    format!("drop failed: {error}")
                                }
                                _ => "drop completed".to_string(),
                            });
                        }
                    }
                    apply_bulk_transfer_status_update(status, events, event, true);
                    if let Some(transfer_id) = transfer_id {
                        if is_terminal {
                            transfer_purposes.forget(transfer_id);
                        }
                    }
                }
            }
            status_changed = true;
        }
        SessionUpdate::ClipboardFileTransferStarted(transfer_id) => {
            if let Some(cache_paths) = transfer_purposes.remember_clipboard(transfer_id) {
                apply_bulk_transfer_status_update(
                    status,
                    events,
                    BulkTransferEvent::Completed {
                        transfer_id,
                        cache_paths,
                    },
                    true,
                );
                transfer_purposes.forget(transfer_id);
                status_changed = true;
            }
        }
        SessionUpdate::DragDropStatus { state, destination } => {
            emit_log(status, events, format!("drag-drop: {state}"));
            status.drag_drop_state = Some(state);
            if destination.is_some() {
                status.drag_drop_destination = destination;
            }
            status_changed = true;
        }
        SessionUpdate::MouseDiagnostics(summary) => {
            status.mouse_diagnostics = Some(summary);
            status_changed = true;
        }
    }

    if status_changed {
        emit_status(events, status);
    }
}

fn apply_bulk_transfer_status_update(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    event: BulkTransferEvent,
    write_clipboard_on_complete: bool,
) {
    match event {
        BulkTransferEvent::Offered(manifest) => {
            status.transfer_active = true;
            status.transfer_id = Some(manifest.transfer_id);
            status.transfer_bytes_done = 0;
            status.transfer_bytes_total = manifest.total_bytes;
            status.transfer_current_file = Some(manifest.root_name.clone());
            emit_log(
                status,
                events,
                format!(
                    "bulk transfer offered: {} ({} bytes)",
                    manifest.root_name, manifest.total_bytes
                ),
            );
        }
        BulkTransferEvent::Progress {
            transfer_id,
            bytes_done,
            bytes_total,
            current_file,
        } => {
            status.transfer_active = true;
            status.transfer_id = Some(transfer_id);
            status.transfer_bytes_done = bytes_done;
            status.transfer_bytes_total = bytes_total;
            status.transfer_current_file = Some(current_file.clone());
            emit_log(
                status,
                events,
                format!(
                    "bulk transfer {transfer_id}: {bytes_done}/{bytes_total} bytes ({current_file})"
                ),
            );
        }
        BulkTransferEvent::Sent { transfer_id } => {
            status.transfer_active = false;
            status.transfer_id = Some(transfer_id);
            status.transfer_current_file = None;
            emit_log(status, events, format!("bulk transfer {transfer_id} sent"));
        }
        BulkTransferEvent::Completed {
            transfer_id,
            cache_paths,
        } => {
            status.transfer_active = false;
            status.transfer_id = Some(transfer_id);
            status.transfer_bytes_done = status.transfer_bytes_total;
            status.transfer_current_file = None;
            if write_clipboard_on_complete {
                let clipboard_payload =
                    clipboard_payload_for_completed_bulk_transfer(transfer_id, &cache_paths);
                match write_clipboard(&clipboard_payload) {
                    Ok(()) => {
                        status.last_clipboard_format = Some("files".to_string());
                        status.last_clipboard_bytes =
                            Some(clipboard_wire_bytes(&clipboard_payload));
                        status.clipboard_ignored_reason = None;
                        emit_log(status, events, "Remote files ready to paste");
                    }
                    Err(error) => {
                        status.last_error = Some(format!("file clipboard write failed: {error}"));
                        emit_log(
                            status,
                            events,
                            format!("file clipboard write failed: {error}"),
                        );
                    }
                }
            } else {
                emit_log(
                    status,
                    events,
                    "Targeted files written to resolved destination",
                );
            }
            emit_log(
                status,
                events,
                format!(
                    "bulk transfer {transfer_id} completed: {} file(s)",
                    cache_paths.len()
                ),
            );
        }
        BulkTransferEvent::Cancelled(transfer_id) => {
            status.transfer_active = false;
            status.transfer_id = Some(transfer_id);
            status.transfer_current_file = None;
            emit_log(
                status,
                events,
                format!("bulk transfer {transfer_id} cancelled"),
            );
        }
        BulkTransferEvent::Failed { transfer_id, error } => {
            status.transfer_active = false;
            status.transfer_id = Some(transfer_id);
            status.transfer_current_file = None;
            status.last_error = Some(error.clone());
            emit_log(
                status,
                events,
                format!("bulk transfer {transfer_id} failed: {error}"),
            );
        }
    }
}

fn bulk_transfer_event_id(event: &BulkTransferEvent) -> Option<Uuid> {
    match event {
        BulkTransferEvent::Offered(manifest) => Some(manifest.transfer_id),
        BulkTransferEvent::Progress { transfer_id, .. }
        | BulkTransferEvent::Sent { transfer_id }
        | BulkTransferEvent::Completed { transfer_id, .. }
        | BulkTransferEvent::Cancelled(transfer_id)
        | BulkTransferEvent::Failed { transfer_id, .. } => Some(*transfer_id),
    }
}

fn bulk_transfer_event_is_terminal(event: &BulkTransferEvent) -> bool {
    matches!(
        event,
        BulkTransferEvent::Sent { .. }
            | BulkTransferEvent::Completed { .. }
            | BulkTransferEvent::Cancelled(_)
            | BulkTransferEvent::Failed { .. }
    )
}

fn clipboard_payload_for_completed_bulk_transfer(
    transfer_id: Uuid,
    cache_paths: &[String],
) -> ClipboardPayload {
    ClipboardPayload::Files(RemoteFileOffer {
        transfer_id,
        files: cache_paths
            .iter()
            .map(|path| FileManifestEntry::file(path.clone(), 0))
            .collect(),
    })
}

fn apply_connection_event_to_status(status: &mut AppStatus, event: &ConnectionEvent) {
    match event {
        ConnectionEvent::Waiting => status.run_state = RunState::Waiting,
        ConnectionEvent::Connecting(_) => status.run_state = RunState::Connecting,
        ConnectionEvent::Connected { .. } => {
            status.run_state = RunState::Connected;
            status.last_error = None;
        }
        ConnectionEvent::Disconnected(_) => status.run_state = RunState::Reconnecting,
        ConnectionEvent::Error(error) => {
            status.run_state = RunState::Error;
            status.last_error = Some(error.clone());
        }
        ConnectionEvent::Message(WireMessage::Error(error)) => {
            status.run_state = RunState::Error;
            status.last_error = Some(error.clone());
        }
        ConnectionEvent::Message(_) => {}
    }
}

fn connection_event_updates_status(event: &ConnectionEvent) -> bool {
    matches!(
        event,
        ConnectionEvent::Waiting
            | ConnectionEvent::Connecting(_)
            | ConnectionEvent::Connected { .. }
            | ConnectionEvent::Disconnected(_)
            | ConnectionEvent::Error(_)
            | ConnectionEvent::Message(WireMessage::Error(_))
    )
}

fn connection_event_log(event: &ConnectionEvent) -> Option<String> {
    match event {
        ConnectionEvent::Waiting => Some("waiting for connection".to_string()),
        ConnectionEvent::Connecting(peer) => Some(format!("connection endpoint {peer}")),
        ConnectionEvent::Connected { peer } => Some(format!("connected {peer} via TCP")),
        ConnectionEvent::Disconnected(peer) => Some(format!("disconnected {peer}")),
        ConnectionEvent::Error(error) => Some(format!("connection error: {error}")),
        ConnectionEvent::Message(WireMessage::Error(error)) => Some(format!("peer error: {error}")),
        ConnectionEvent::Message(_) => None,
    }
}

fn emit_log(status: &mut AppStatus, sender: &Sender<RuntimeEvent>, message: impl Into<String>) {
    let message = message.into();
    status.push_log(message.clone());
    send(sender, RuntimeEvent::Log(message));
}

fn emit_status(sender: &Sender<RuntimeEvent>, status: &AppStatus) {
    send(sender, RuntimeEvent::Status(status.clone()));
}

fn send(sender: &Sender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = sender.send(event);
}

fn log_stop_task_outcome(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    outcome: StopTaskOutcome,
) {
    if outcome.aborted > 0 {
        emit_log(
            status,
            events,
            format!(
                "aborted {} runtime task(s) after shutdown timeout",
                outcome.aborted
            ),
        );
    }
}

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

#[derive(Debug, Default)]
struct HeartbeatTracker {
    outstanding: VecDeque<u64>,
}

impl HeartbeatTracker {
    fn sent(&mut self, sent_millis: u64) {
        self.outstanding.push_back(sent_millis);
        while self.outstanding.len() > MAX_OUTSTANDING_HEARTBEATS {
            self.outstanding.pop_front();
        }
    }

    fn receive(&mut self, sent_millis: u64, now_millis: u64) -> Option<u64> {
        let index = self
            .outstanding
            .iter()
            .position(|outstanding| *outstanding == sent_millis)?;
        self.outstanding.remove(index);
        now_millis.checked_sub(sent_millis)
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use borderless_core::{
        clipboard::{ClipboardChangeId, ClipboardEnvelope, ClipboardPayload},
        config::{RemotePosition, Role},
        file_transfer::FileManifestEntry,
        input_event::{InputEvent, KeyEvent, MouseButton, MouseButtonEvent, MouseWheelEvent},
        protocol::WireMessage,
    };
    use borderless_net::transport::ConnectionEvent;

    use super::*;

    #[test]
    fn controller_connection_settings_map_controller_config() {
        let mut config = AppConfig {
            role: Role::Controller,
            ..AppConfig::default()
        };
        config.controller.agent_host = "agent.local".to_string();
        config.controller.agent_port = 34567;

        let settings = controller_connection_settings(&config);

        assert_eq!(settings.host, "agent.local");
        assert_eq!(settings.port, 34567);
    }

    #[test]
    fn agent_connection_settings_map_agent_config() {
        let mut config = AppConfig {
            role: Role::Agent,
            ..AppConfig::default()
        };
        config.agent.listen_host = "127.0.0.1".to_string();
        config.agent.listen_port = 45678;

        let settings = agent_connection_settings(&config);

        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.port, 45678);
    }

    #[test]
    fn connection_events_update_status_state_sequence_and_error() {
        let mut status = AppStatus::default();

        apply_connection_event_to_status(
            &mut status,
            &ConnectionEvent::Connected {
                peer: "127.0.0.1:24800".to_string(),
            },
        );
        assert_eq!(status.run_state, RunState::Connected);

        apply_connection_event_to_status(&mut status, &ConnectionEvent::Error("boom".to_string()));
        assert_eq!(status.run_state, RunState::Error);
        assert_eq!(status.last_error.as_deref(), Some("boom"));

        apply_connection_event_to_status(
            &mut status,
            &ConnectionEvent::Message(WireMessage::ReleaseAll),
        );
        assert_eq!(status.run_state, RunState::Error);
    }

    #[test]
    fn clipboard_enabled_tracks_any_clipboard_sharing_flag() {
        let mut config = AppConfig::default();
        config.sharing.clipboard_text = false;
        config.sharing.clipboard_html = false;
        config.sharing.clipboard_images = false;
        config.sharing.file_copy_paste = false;

        assert!(!clipboard_enabled(&config));

        config.sharing.clipboard_html = true;
        assert!(clipboard_enabled(&config));
    }

    #[test]
    fn clipboard_payload_format_names_are_human_readable() {
        assert_eq!(
            clipboard_payload_format(&ClipboardPayload::UnicodeText("hi".to_string())),
            "text"
        );
        assert_eq!(
            clipboard_payload_format(&ClipboardPayload::Html("<b>hi</b>".to_string())),
            "html"
        );
        assert_eq!(
            clipboard_payload_format(&ClipboardPayload::ImagePng(vec![1, 2, 3])),
            "png"
        );
        assert_eq!(
            clipboard_payload_format(&ClipboardPayload::ImageDib(vec![1, 2, 3])),
            "dib"
        );
        assert_eq!(
            clipboard_payload_format(&ClipboardPayload::Files(
                borderless_core::clipboard::RemoteFileOffer {
                    transfer_id: Default::default(),
                    files: vec![FileManifestEntry::file("a.txt", 4)],
                },
            )),
            "files"
        );
    }

    #[test]
    fn clipboard_send_decision_enforces_enabled_formats_and_size_limit() {
        let mut config = AppConfig::default();
        config.sharing.max_clipboard_bytes = 4;
        config.sharing.clipboard_html = false;

        assert_eq!(
            clipboard_send_decision(&config, &ClipboardPayload::UnicodeText("four".to_string())),
            ClipboardSendDecision::Send {
                format: "text",
                bytes: 4
            }
        );
        assert_eq!(
            clipboard_send_decision(
                &config,
                &ClipboardPayload::UnicodeText("too large".to_string()),
            ),
            ClipboardSendDecision::Ignore("clipboard text is 9 bytes, limit is 4".to_string())
        );
        assert_eq!(
            clipboard_send_decision(&config, &ClipboardPayload::Html("<b>x</b>".to_string())),
            ClipboardSendDecision::Ignore("clipboard html sharing is disabled".to_string())
        );
    }

    #[test]
    fn clipboard_file_path_metadata_size_ignores_future_content_size() {
        let mut config = AppConfig::default();
        config.sharing.max_clipboard_bytes = 64;
        config.sharing.max_file_transfer_bytes = u64::MAX;
        let payload = ClipboardPayload::Files(borderless_core::clipboard::RemoteFileOffer {
            transfer_id: Default::default(),
            files: vec![FileManifestEntry::file("small-path.txt", u64::MAX)],
        });

        assert_eq!(
            clipboard_send_decision(&config, &payload),
            ClipboardSendDecision::Send {
                format: "files",
                bytes: 14
            }
        );
    }

    #[test]
    fn clipboard_data_larger_than_protocol_limit_is_ignored_before_enqueue() {
        let mut config = AppConfig::default();
        config.sharing.max_clipboard_bytes = (32 * 1024 * 1024) as u64;
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::ImagePng(vec![
                0;
                borderless_core::protocol::MAX_PAYLOAD_LEN
            ]),
        };

        assert!(matches!(
            clipboard_data_send_decision(&config, &envelope),
            ClipboardSendDecision::Ignore(reason)
                if reason.contains("clipboard png encoded message is too large")
                    && reason.contains("protocol limit")
        ));
    }

    #[test]
    fn clipboard_data_larger_than_protocol_limit_is_ignored_before_write_status() {
        let mut config = AppConfig::default();
        config.sharing.max_clipboard_bytes = (32 * 1024 * 1024) as u64;
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::ImagePng(vec![
                0;
                borderless_core::protocol::MAX_PAYLOAD_LEN
            ]),
        };

        assert!(matches!(
            clipboard_data_write_decision(&config, &envelope),
            ClipboardSendDecision::Ignore(reason)
                if reason.contains("clipboard png encoded message is too large")
                    && reason.contains("protocol limit")
        ));
    }

    #[test]
    fn clipboard_event_does_not_enqueue_oversized_protocol_data() {
        let mut config = AppConfig::default();
        config.sharing.max_clipboard_bytes = (32 * 1024 * 1024) as u64;
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::ImagePng(vec![
                0;
                borderless_core::protocol::MAX_PAYLOAD_LEN
            ]),
        };
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = unbounded();

        handle_clipboard_event(
            ClipboardEvent::Changed(envelope),
            &config,
            &connection_commands_tx,
            &[],
            &updates_tx,
            7,
            true,
        );

        assert!(connection_commands_rx.try_recv().is_err());
        assert!(updates_rx.try_iter().any(|update| {
            matches!(
                update.update,
                SessionUpdate::ClipboardIgnored(reason)
                    if reason.contains("clipboard png encoded message is too large")
            )
        }));
    }

    #[test]
    fn clipboard_event_without_transport_ready_is_ignored_before_enqueue() {
        let config = AppConfig::default();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::UnicodeText("waiting".to_string()),
        };
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = unbounded();

        handle_clipboard_event(
            ClipboardEvent::Changed(envelope),
            &config,
            &connection_commands_tx,
            &[],
            &updates_tx,
            7,
            false,
        );

        assert!(connection_commands_rx.try_recv().is_err());
        assert!(updates_rx.try_iter().any(|update| {
            matches!(
                update.update,
                SessionUpdate::ClipboardIgnored(reason)
                    if reason == "clipboard sync skipped: peer is not connected"
            )
        }));
    }

    #[test]
    fn clipboard_event_with_transport_ready_enqueues_data_and_reports_queued() {
        let config = AppConfig::default();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::UnicodeText("ready".to_string()),
        };
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = unbounded();

        handle_clipboard_event(
            ClipboardEvent::Changed(envelope.clone()),
            &config,
            &connection_commands_tx,
            &[],
            &updates_tx,
            7,
            true,
        );

        assert_eq!(
            connection_commands_rx.try_recv(),
            Ok(ConnectionCommand::Send(WireMessage::ClipboardData(
                envelope
            )))
        );
        assert!(updates_rx.try_iter().any(|update| {
            matches!(
                update.update,
                SessionUpdate::ClipboardQueued { format, bytes }
                    if format == "text" && bytes == 5
            )
        }));
    }

    #[test]
    fn clipboard_file_payload_over_file_transfer_limit_is_ignored() {
        let mut config = AppConfig::default();
        config.sharing.max_clipboard_bytes = 1024;
        config.sharing.max_file_transfer_bytes = 10;
        let payload = ClipboardPayload::Files(borderless_core::clipboard::RemoteFileOffer {
            transfer_id: Default::default(),
            files: vec![FileManifestEntry::file("large.bin", 11)],
        });

        assert_eq!(
            clipboard_send_decision(&config, &payload),
            ClipboardSendDecision::Ignore(
                "clipboard files are 11 bytes, file transfer limit is 10".to_string()
            )
        );
    }

    #[test]
    fn clipboard_file_event_does_not_send_local_paths_over_control_channel() {
        let mut config = AppConfig::default();
        config.sharing.file_copy_paste = true;
        let root = std::env::temp_dir().join(format!(
            "borderless-clipboard-file-event-{}",
            unused_tcp_port()
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.txt");
        fs::write(&source, b"hello").unwrap();
        let source_path = source.to_string_lossy().to_string();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::Files(borderless_core::clipboard::RemoteFileOffer {
                transfer_id: Default::default(),
                files: vec![FileManifestEntry::file(source_path.clone(), 5)],
            }),
        };
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (bulk_commands_tx, mut bulk_commands_rx) = mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = unbounded();

        handle_clipboard_event(
            ClipboardEvent::Changed(envelope),
            &config,
            &connection_commands_tx,
            std::slice::from_ref(&bulk_commands_tx),
            &updates_tx,
            7,
            true,
        );

        assert!(matches!(
            connection_commands_rx.try_recv(),
            Ok(ConnectionCommand::Send(WireMessage::FileTransferOffer(manifest)))
                if manifest.files.iter().all(|entry| !entry.relative_path.contains(':'))
                    && manifest.files.iter().any(|entry| entry.relative_path == "source.txt")
        ));
        assert!(matches!(
            bulk_commands_rx.try_recv(),
            Ok(BulkTransferCommand::SendFiles { source_paths, .. }) if source_paths == vec![source_path]
        ));
        assert!(updates_rx.try_iter().any(|update| {
            matches!(
                update.update,
                SessionUpdate::ClipboardQueued { format, bytes }
                    if format == "files"
                        && bytes == source.to_string_lossy().len() as u64
            )
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clipboard_folder_transfer_request_expands_children_and_totals_bytes() {
        let root = std::env::temp_dir().join(format!(
            "borderless-clipboard-folder-event-{}",
            unused_tcp_port()
        ));
        let source_dir = root.join("docs");
        fs::create_dir_all(source_dir.join("nested")).unwrap();
        fs::write(source_dir.join("nested").join("note.txt"), b"note").unwrap();
        fs::write(source_dir.join("root.txt"), b"root-file").unwrap();
        let source_path = source_dir.to_string_lossy().to_string();
        let offer = borderless_core::clipboard::RemoteFileOffer {
            transfer_id: Default::default(),
            files: vec![FileManifestEntry::file(source_path.clone(), 0)],
        };

        let (manifest, source_paths) = file_transfer_request_from_clipboard_offer(&offer).unwrap();

        assert_eq!(source_paths, vec![source_path]);
        assert_eq!(manifest.root_name, "docs");
        assert_eq!(manifest.total_bytes, 13);
        assert!(manifest
            .files
            .iter()
            .any(|entry| entry.relative_path == "docs/nested/note.txt" && entry.size_bytes == 4));
        assert!(manifest
            .files
            .iter()
            .any(|entry| entry.relative_path == "docs/root.txt" && entry.size_bytes == 9));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clipboard_folder_event_over_file_transfer_limit_is_ignored_after_expansion() {
        let mut config = AppConfig::default();
        config.sharing.file_copy_paste = true;
        config.sharing.max_file_transfer_bytes = 3;
        let root = std::env::temp_dir().join(format!(
            "borderless-clipboard-folder-limit-{}",
            unused_tcp_port()
        ));
        let source_dir = root.join("docs");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("note.txt"), b"note").unwrap();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::Files(borderless_core::clipboard::RemoteFileOffer {
                transfer_id: Default::default(),
                files: vec![FileManifestEntry::file(
                    source_dir.to_string_lossy().to_string(),
                    0,
                )],
            }),
        };
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (bulk_commands_tx, mut bulk_commands_rx) = mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = unbounded();

        handle_clipboard_event(
            ClipboardEvent::Changed(envelope),
            &config,
            &connection_commands_tx,
            &[bulk_commands_tx],
            &updates_tx,
            7,
            true,
        );

        assert!(connection_commands_rx.try_recv().is_err());
        assert!(bulk_commands_rx.try_recv().is_err());
        assert!(updates_rx.try_iter().any(|update| {
            matches!(
                update.update,
                SessionUpdate::ClipboardIgnored(reason)
                    if reason == "clipboard files are 4 bytes, file transfer limit is 3"
            )
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_bulk_transfer_paths_become_file_clipboard_payload() {
        let transfer_id = Default::default();
        let payload = clipboard_payload_for_completed_bulk_transfer(
            transfer_id,
            &["C:/cache/a.txt".to_string(), "C:/cache/b.txt".to_string()],
        );

        assert_eq!(
            payload,
            ClipboardPayload::Files(borderless_core::clipboard::RemoteFileOffer {
                transfer_id,
                files: vec![
                    FileManifestEntry::file("C:/cache/a.txt", 0),
                    FileManifestEntry::file("C:/cache/b.txt", 0),
                ],
            })
        );
    }

    #[test]
    fn sent_bulk_transfer_clears_sender_progress() {
        let transfer_id = Uuid::from_u128(20);
        let mut status = AppStatus {
            transfer_active: true,
            transfer_id: Some(transfer_id),
            transfer_bytes_done: 10,
            transfer_bytes_total: 20,
            transfer_current_file: Some("docs/a.txt".to_string()),
            ..Default::default()
        };
        let (events_tx, _events_rx) = unbounded();

        apply_bulk_transfer_status_update(
            &mut status,
            &events_tx,
            BulkTransferEvent::Sent { transfer_id },
            true,
        );

        assert!(!status.transfer_active);
        assert_eq!(status.transfer_current_file, None);
    }
    #[test]
    fn remote_clipboard_offer_is_ignored_instead_of_written_as_data() {
        let config = AppConfig::default();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::UnicodeText("offer".to_string()),
        };

        assert_eq!(
            remote_clipboard_message_action(&config, WireMessage::ClipboardOffer(envelope)),
            RemoteClipboardMessageAction::Ignore(
                "clipboard offer ignored: data message required".to_string()
            )
        );
    }

    #[test]
    fn remote_clipboard_data_is_selected_for_local_write() {
        let config = AppConfig::default();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::UnicodeText("data".to_string()),
        };

        assert_eq!(
            remote_clipboard_message_action(&config, WireMessage::ClipboardData(envelope.clone())),
            RemoteClipboardMessageAction::Write {
                envelope,
                format: "text",
                bytes: 4,
            }
        );
    }

    #[test]
    fn controller_clipboard_transport_ready_waits_for_valid_hello_and_resets() {
        let config = AppConfig::default();
        let local_desktop = Rect::new(0, 0, 100, 100);
        let remote_desktop = Rect::new(100, 0, 100, 100);
        let valid_hello = ConnectionEvent::Message(WireMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop: remote_desktop,
        }));
        let invalid_hello = ConnectionEvent::Message(WireMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            desktop: remote_desktop,
        }));
        let control_state = Some(ControlState::new(
            local_desktop,
            remote_desktop,
            config.controller.remote_position.clone(),
            config.edge_trigger_px,
        ));

        assert!(!controller_clipboard_transport_ready_after_event(
            false,
            &valid_hello,
            &None,
        ));
        assert!(controller_clipboard_transport_ready_after_event(
            false,
            &valid_hello,
            &control_state,
        ));
        assert!(!controller_clipboard_transport_ready_after_event(
            true,
            &invalid_hello,
            &control_state,
        ));
        assert!(!controller_clipboard_transport_ready_after_event(
            true,
            &ConnectionEvent::Disconnected("agent".to_string()),
            &control_state,
        ));
    }

    #[test]
    fn agent_clipboard_transport_ready_follows_connection_state() {
        assert!(agent_clipboard_transport_ready_after_event(
            false,
            &ConnectionEvent::Connected {
                peer: "controller".to_string(),
            },
        ));
        assert!(agent_clipboard_transport_ready_after_event(
            true,
            &ConnectionEvent::Message(WireMessage::ReleaseAll),
        ));
        assert!(!agent_clipboard_transport_ready_after_event(
            true,
            &ConnectionEvent::Waiting,
        ));
        assert!(!agent_clipboard_transport_ready_after_event(
            true,
            &ConnectionEvent::Error("closed".to_string()),
        ));
    }

    #[test]
    fn clipboard_data_under_protocol_limit_still_sends() {
        let config = AppConfig::default();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::UnicodeText("small".to_string()),
        };

        assert_eq!(
            clipboard_data_send_decision(&config, &envelope),
            ClipboardSendDecision::Send {
                format: "text",
                bytes: 5
            }
        );
    }

    #[test]
    fn clipboard_monitor_options_follow_sharing_config_and_size_limit() {
        let mut config = AppConfig::default();
        config.sharing.clipboard_text = true;
        config.sharing.clipboard_html = false;
        config.sharing.clipboard_images = false;
        config.sharing.file_copy_paste = true;
        config.sharing.max_clipboard_bytes = 1234;

        let options = clipboard_read_options(&config);

        assert!(options.text);
        assert!(!options.html);
        assert!(!options.images);
        assert!(options.files);
        assert_eq!(options.max_bytes, 1234);
    }

    #[test]
    fn clipboard_monitor_options_cap_size_at_protocol_payload_limit() {
        let mut config = AppConfig::default();
        config.sharing.max_clipboard_bytes = (32 * 1024 * 1024) as u64;

        let options = clipboard_read_options(&config);

        assert_eq!(
            options.max_bytes,
            borderless_core::protocol::MAX_PAYLOAD_LEN as u64
        );
    }

    #[test]
    fn clipboard_status_updates_record_queued_and_ignored_events() {
        let (events_tx, events_rx) = unbounded();
        let mut status = AppStatus::default();
        let envelope = ClipboardEnvelope {
            change_id: ClipboardChangeId::new(Default::default(), 1),
            payload: ClipboardPayload::UnicodeText("hello".to_string()),
        };

        apply_clipboard_status_update(
            &mut status,
            &events_tx,
            ClipboardStatusUpdate::Queued(&envelope.payload),
        );

        assert_eq!(status.last_clipboard_format.as_deref(), Some("text"));
        assert_eq!(status.last_clipboard_bytes, Some(5));
        assert_eq!(status.clipboard_ignored_reason, None);
        assert!(status
            .events
            .iter()
            .any(|message| message == "clipboard queued: text (5 bytes)"));

        apply_clipboard_status_update(
            &mut status,
            &events_tx,
            ClipboardStatusUpdate::Ignored("too large".to_string()),
        );

        assert_eq!(
            status.clipboard_ignored_reason.as_deref(),
            Some("too large")
        );
        assert!(status
            .events
            .iter()
            .any(|message| message == "clipboard ignored: too large"));

        apply_clipboard_status_update(
            &mut status,
            &events_tx,
            ClipboardStatusUpdate::Error("clipboard busy".to_string()),
        );

        assert_eq!(status.last_error.as_deref(), Some("clipboard busy"));
        assert_eq!(
            status.clipboard_ignored_reason.as_deref(),
            Some("clipboard busy")
        );
        assert!(events_rx
            .try_iter()
            .any(|event| matches!(event, RuntimeEvent::Status(status) if status.last_clipboard_bytes == Some(5))));
    }

    #[test]
    fn remote_input_send_buffer_coalesces_tcp_moves_and_preserves_reliable_order() {
        let mut buffer = RemoteInputSendBuffer::new();
        let key_down = InputEvent::Key(KeyEvent {
            vk_code: 0x41,
            pressed: true,
        });
        let left_down = InputEvent::MouseButton(MouseButtonEvent {
            button: MouseButton::Left,
            pressed: true,
        });
        let wheel = InputEvent::MouseWheel(MouseWheelEvent {
            delta: 120,
            horizontal: false,
        });
        let mut actions = Vec::new();

        actions.extend(buffer.send_pointer(Point::new(10, 10), 1_000));
        actions.extend(buffer.send_pointer(Point::new(20, 20), 1_001));
        actions.extend(buffer.send_reliable_input(key_down.clone()));
        actions.extend(buffer.send_pointer(Point::new(30, 30), 1_002));
        actions.extend(buffer.send_pointer(Point::new(40, 40), 1_003));
        actions.extend(buffer.send_reliable_input(left_down.clone()));
        actions.extend(buffer.send_reliable_input(wheel.clone()));
        actions.extend(buffer.send_release_all());
        actions.extend(buffer.flush_pending_move());

        assert_eq!(
            actions,
            vec![
                RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::Input(
                    InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x: 20, y: 20 })
                ))),
                RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::Input(key_down))),
                RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::Input(
                    InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x: 40, y: 40 })
                ))),
                RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::Input(left_down))),
                RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::Input(wheel))),
                RemoteSendAction::Command(ConnectionCommand::Send(WireMessage::ReleaseAll)),
            ]
        );
    }

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

    #[test]
    fn remote_input_send_buffer_logs_tcp_move_coalescing_once_per_second() {
        let mut buffer = RemoteInputSendBuffer::new();
        let mut log_actions = Vec::new();

        for i in 0..=102 {
            log_actions.extend(
                buffer
                    .send_pointer(Point::new(i, i), 1_000)
                    .into_iter()
                    .filter(|action| matches!(action, RemoteSendAction::Log(_))),
            );
        }
        log_actions.extend(
            buffer
                .send_pointer(Point::new(200, 200), 1_500)
                .into_iter()
                .filter(|action| matches!(action, RemoteSendAction::Log(_))),
        );

        assert_eq!(
            log_actions,
            vec![RemoteSendAction::Log(
                "coalesced remote mouse moves: 101 in the last second".to_string()
            )]
        );

        for i in 0..=101 {
            log_actions.extend(
                buffer
                    .send_pointer(Point::new(300 + i, 300 + i), 2_100)
                    .into_iter()
                    .filter(|action| matches!(action, RemoteSendAction::Log(_))),
            );
        }

        assert_eq!(
            log_actions,
            vec![
                RemoteSendAction::Log(
                    "coalesced remote mouse moves: 101 in the last second".to_string()
                ),
                RemoteSendAction::Log(
                    "coalesced remote mouse moves: 101 in the last second".to_string()
                ),
            ]
        );
    }

    #[test]
    fn controller_terminal_connection_events_recover_from_remote_mode() {
        for event in [
            ConnectionEvent::Disconnected("agent".to_string()),
            ConnectionEvent::Error("network down".to_string()),
            ConnectionEvent::Message(WireMessage::Error("peer error".to_string())),
        ] {
            let mut control_state = Some(remote_control_state());

            let action =
                controller_recovery_action_for_connection_event(&event, &mut control_state);

            assert_eq!(
                action,
                ControllerRecoveryAction {
                    pass_through: true,
                    release_all: true,
                }
            );
            assert!(control_state.is_none());
        }
    }

    #[test]
    fn return_local_control_output_preserves_pointer_target() {
        let point = Point::new(1917, 540);

        assert_eq!(
            return_local_pointer_target(&ControlOutput::ReturnLocal(point)),
            Some(point)
        );
        assert_eq!(
            return_local_pointer_target(&ControlOutput::MoveRemote(point)),
            None
        );
    }

    #[test]
    fn remote_control_parks_local_pointer_away_from_edge_for_delta_tracking() {
        let local_desktop = Rect::new(0, 0, 1920, 1080);
        let mut last_pointer = Some(Point::new(1919, 540));

        let anchor = mark_local_pointer_parked_for_remote_control(local_desktop, &mut last_pointer);

        assert_eq!(anchor, Point::new(960, 540));
        assert_eq!(last_pointer, Some(anchor));
    }

    #[test]
    fn parked_remote_control_delta_moves_remote_instead_of_returning_immediately() {
        let local_desktop = Rect::new(0, 0, 1920, 1080);
        let remote_desktop = Rect::new(0, 0, 1280, 720);
        let mut state = ControlState::new(local_desktop, remote_desktop, RemotePosition::Right, 2);
        assert!(matches!(
            state.observe_local_pointer(Point::new(1919, 540)),
            ControlOutput::EnterRemote(_)
        ));
        let mut last_pointer = Some(Point::new(1919, 540));
        let anchor = mark_local_pointer_parked_for_remote_control(local_desktop, &mut last_pointer);
        let next_local = Point::new(anchor.x + 20, anchor.y);
        let previous = last_pointer.replace(next_local).unwrap();

        let output = state.apply_remote_delta(
            next_local.x.saturating_sub(previous.x),
            next_local.y.saturating_sub(previous.y),
        );

        assert_eq!(output, ControlOutput::MoveRemote(Point::new(20, 359)));
        assert_eq!(state.mode(), ControlMode::Remote);
    }

    #[test]
    fn parked_pointer_position_is_consumed_before_delta_calculation() {
        let anchor = Point::new(960, 540);
        let mut last_pointer = Some(Point::new(900, 540));
        let mut pending_park = Some(anchor);

        assert!(consume_pending_pointer_park(
            Point::new(961, 540),
            &mut last_pointer,
            &mut pending_park
        ));
        assert_eq!(last_pointer, Some(anchor));
        assert_eq!(pending_park, None);
    }

    #[test]
    fn parked_pointer_position_consumes_small_sendinput_landing_error() {
        let anchor = Point::new(960, 540);
        let mut last_pointer = Some(Point::new(900, 540));
        let mut pending_park = Some(anchor);

        assert!(consume_pending_pointer_park(
            Point::new(982, 536),
            &mut last_pointer,
            &mut pending_park
        ));
        assert_eq!(last_pointer, Some(anchor));
        assert_eq!(pending_park, None);
    }

    #[test]
    fn remote_move_reparks_only_near_local_desktop_edge() {
        let local_desktop = Rect::new(0, 0, 1920, 1080);

        assert!(!should_repark_local_pointer_for_remote_control(
            Point::new(960, 540),
            local_desktop
        ));
        assert!(should_repark_local_pointer_for_remote_control(
            Point::new(1910, 540),
            local_desktop
        ));
    }

    #[test]
    fn remote_pointer_position_falls_back_to_signed_delta_without_recent_raw_input() {
        let local_desktop = Rect::new(0, 0, 1920, 1080);
        let mut last_pointer = Some(Point::new(960, 540));

        let action = remote_pointer_position_action(
            Point::new(940, 545),
            local_desktop,
            &mut last_pointer,
            false,
        );

        assert_eq!(
            action,
            RemotePointerPositionAction::Delta {
                dx: -20,
                dy: 5,
                repark_after_move: false
            }
        );
        assert_eq!(last_pointer, Some(Point::new(940, 545)));
    }

    #[test]
    fn remote_pointer_position_does_not_double_apply_when_raw_input_is_recent() {
        let local_desktop = Rect::new(0, 0, 1920, 1080);
        let mut last_pointer = Some(Point::new(960, 540));

        let action = remote_pointer_position_action(
            Point::new(940, 545),
            local_desktop,
            &mut last_pointer,
            true,
        );

        assert_eq!(
            action,
            RemotePointerPositionAction::TrackOnly {
                repark_after_move: false
            }
        );
        assert_eq!(last_pointer, Some(Point::new(940, 545)));
    }

    #[test]
    fn raw_delta_after_pending_absolute_clears_fallback_and_tracks_pointer() {
        let mut pending = Some(PendingRemotePointerPosition {
            point: Point::new(940, 545),
            observed_millis: 100,
        });
        let mut last_pointer = Some(Point::new(960, 540));

        assert!(clear_pending_remote_pointer_position_after_raw_delta(
            &mut pending,
            &mut last_pointer
        ));

        assert_eq!(pending, None);
        assert_eq!(last_pointer, Some(Point::new(940, 545)));
    }

    #[test]
    fn pending_remote_pointer_position_waits_for_raw_fallback_delay() {
        let pending = PendingRemotePointerPosition {
            point: Point::new(940, 545),
            observed_millis: 100,
        };

        assert!(!pending_remote_pointer_position_due(pending, 103));
        assert!(pending_remote_pointer_position_due(pending, 104));
    }

    #[test]
    fn mouse_diagnostics_summary_records_cross_screen_event_counts() {
        let mut diagnostics = MouseDiagnostics::default();

        diagnostics.record_hook_position(Point::new(1919, 540), false);
        diagnostics.record_hook_position(Point::new(960, 540), true);
        diagnostics.record_raw_delta(-7, 3, 100);
        diagnostics.record_remote_move(Point::new(1200, 360));
        diagnostics.record_pointer_park_attempt();
        diagnostics.record_pointer_park_consumed();
        diagnostics.record_pointer_park_missed();
        diagnostics.record_return_local();

        assert!(diagnostics.raw_delta_recent(130));
        assert!(!diagnostics.raw_delta_recent(151));
        assert_eq!(
            diagnostics.summary(),
            "hook local=1, hook remote=1, raw=1, sent=1, parks=1/1/1, returns=1, last delta=-7,3, local=960,540, remote=1200,360"
        );
    }

    #[test]
    fn return_local_pointer_warp_is_consumed_before_edge_detection() {
        let return_point = Point::new(1917, 540);
        let mut last_pointer = Some(return_point);
        let mut pending_warp = Some(return_point);

        assert!(consume_pending_pointer_park(
            Point::new(1919, 540),
            &mut last_pointer,
            &mut pending_warp
        ));
        assert_eq!(last_pointer, Some(return_point));
        assert_eq!(pending_warp, None);
    }

    #[test]
    fn heartbeat_tracker_reports_only_returned_local_heartbeats() {
        let mut tracker = HeartbeatTracker::default();
        tracker.sent(1000);

        assert_eq!(tracker.receive(500, 2000), None);
        assert_eq!(tracker.receive(1000, 1125), Some(125));
        assert_eq!(tracker.receive(1000, 1200), None);
    }

    #[test]
    fn runtime_emits_log_and_status_for_start_stop_and_reconnect() {
        let runtime = RuntimeHandle::spawn();
        let config = safe_agent_config();

        runtime.send(RuntimeCommand::Start(config.clone()));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(
            |event| matches!(event, RuntimeEvent::Log(message) if message == "starting runtime")
        ));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status) if status.run_state == RunState::Connecting
            )
        }));

        runtime.send(RuntimeCommand::Stop);
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(
            |event| matches!(event, RuntimeEvent::Log(message) if message == "runtime stopped")
        ));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status) if status.run_state == RunState::Stopped
            )
        }));

        runtime.send(RuntimeCommand::Reconnect(config));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(|event| {
            matches!(event, RuntimeEvent::Log(message) if message == "reconnecting runtime")
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status) if status.run_state == RunState::Reconnecting
            )
        }));

        runtime.send(RuntimeCommand::Stop);
    }

    #[tokio::test]
    async fn active_runtime_stop_sends_release_all_when_remote_active() {
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (session_commands_tx, mut session_commands_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let _ = session_commands_rx.recv().await;
        });
        let mut runtime = ActiveRuntime {
            session_id: 7,
            connection_commands: connection_commands_tx,
            session_commands: session_commands_tx,
            bulk_commands: Vec::new(),
            hook_manager: None,
            remote_control_active: Some(Arc::new(AtomicBool::new(true))),
            tasks: vec![task],
            stopped: false,
        };

        let outcome = runtime.stop().await;

        assert_eq!(
            outcome,
            StopTaskOutcome {
                completed: 1,
                aborted: 0,
            }
        );
        assert_eq!(
            connection_commands_rx.recv().await,
            Some(ConnectionCommand::Send(WireMessage::ReleaseAll))
        );
        assert_eq!(
            connection_commands_rx.recv().await,
            Some(ConnectionCommand::Stop)
        );
    }

    #[tokio::test]
    async fn active_runtime_stop_skips_release_all_when_local() {
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (session_commands_tx, mut session_commands_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let _ = session_commands_rx.recv().await;
        });
        let mut runtime = ActiveRuntime {
            session_id: 7,
            connection_commands: connection_commands_tx,
            session_commands: session_commands_tx,
            bulk_commands: Vec::new(),
            hook_manager: None,
            remote_control_active: Some(Arc::new(AtomicBool::new(false))),
            tasks: vec![task],
            stopped: false,
        };

        let outcome = runtime.stop().await;

        assert_eq!(
            outcome,
            StopTaskOutcome {
                completed: 1,
                aborted: 0,
            }
        );
        assert_eq!(
            connection_commands_rx.recv().await,
            Some(ConnectionCommand::Stop)
        );
        assert!(connection_commands_rx.try_recv().is_err());
    }

    #[test]
    fn agent_disconnect_release_logs_released_pressed_input() {
        let (connection_commands_tx, _connection_commands_rx) = mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = unbounded();
        let config = AppConfig::default();
        let mut injector = Some(InputInjector::new(Rect::new(0, 0, 1920, 1080)));
        let mut heartbeat = HeartbeatTracker::default();

        handle_agent_connection_event(
            ConnectionEvent::Disconnected("controller".to_string()),
            &config,
            Rect::new(0, 0, 1920, 1080),
            &mut injector,
            &connection_commands_tx,
            &updates_tx,
            42,
            &mut heartbeat,
        );

        let updates = updates_rx.try_iter().collect::<Vec<_>>();
        assert!(updates.iter().any(|update| {
            matches!(
                &update.update,
                SessionUpdate::Log(message)
                    if message == "released all pressed input after disconnect"
            )
        }));
        assert!(injector.is_none());
    }

    #[test]
    fn return_local_release_all_keeps_remote_active_until_actions_are_emitted() {
        let remote_control_active = Arc::new(AtomicBool::new(true));
        let mut buffer = RemoteInputSendBuffer::new();
        let mut observed: Option<(bool, Vec<RemoteSendAction>)> = None;

        emit_return_local_release_all(&mut buffer, &remote_control_active, |actions| {
            observed = Some((remote_control_active.load(Ordering::SeqCst), actions));
        });

        assert_eq!(
            observed,
            Some((
                true,
                vec![RemoteSendAction::Command(ConnectionCommand::Send(
                    WireMessage::ReleaseAll
                ))],
            ))
        );
        assert!(!remote_control_active.load(Ordering::SeqCst));
    }

    #[test]
    fn recovery_release_all_keeps_remote_active_until_release_is_enqueued() {
        let remote_control_active = Arc::new(AtomicBool::new(true));
        let mut observed_remote_active = None;

        emit_recovery_release_all(&remote_control_active, || {
            observed_remote_active = Some(remote_control_active.load(Ordering::SeqCst));
        });

        assert_eq!(observed_remote_active, Some(true));
        assert!(!remote_control_active.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn wait_for_session_tasks_aborts_pending_tasks_after_timeout() {
        let mut tasks = vec![tokio::spawn(async {
            std::future::pending::<()>().await;
        })];

        let outcome = wait_for_session_tasks(&mut tasks, Duration::from_millis(1)).await;

        assert_eq!(
            outcome,
            StopTaskOutcome {
                completed: 0,
                aborted: 1,
            }
        );
        assert!(tasks.is_empty());
    }

    #[test]
    fn runtime_command_is_cloneable() {
        let command = RuntimeCommand::Start(AppConfig::default());
        let cloned = command.clone();

        assert!(matches!(cloned, RuntimeCommand::Start(_)));
    }

    #[test]
    fn agent_start_and_reconnect_report_connection_states() {
        let runtime = RuntimeHandle::spawn();
        let config = safe_agent_config();

        runtime.send(RuntimeCommand::Start(config.clone()));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status)
                    if status.run_state == RunState::Connecting
            )
        }));

        runtime.send(RuntimeCommand::Reconnect(config));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status)
                    if status.run_state == RunState::Reconnecting
            )
        }));

        runtime.send(RuntimeCommand::Stop);
    }

    fn safe_agent_config() -> AppConfig {
        let mut config = AppConfig {
            role: Role::Agent,
            ..AppConfig::default()
        };
        config.agent.listen_host = "127.0.0.1".to_string();
        config.agent.listen_port = unused_tcp_port();
        config.sharing.clipboard_text = false;
        config.sharing.clipboard_html = false;
        config.sharing.clipboard_images = false;
        config.sharing.file_copy_paste = false;
        config.sharing.bulk_transfer_port = unused_tcp_port();
        config.sharing.incoming_cache_dir = std::env::temp_dir()
            .join(format!("borderless-runtime-test-{}", unused_tcp_port()))
            .to_string_lossy()
            .to_string();
        config
    }

    fn remote_control_state() -> ControlState {
        let mut state = ControlState::new(
            Rect::new(0, 0, 1920, 1080),
            Rect::new(0, 0, 1280, 720),
            RemotePosition::Right,
            2,
        );
        assert!(matches!(
            state.observe_local_pointer(Point::new(1919, 540)),
            ControlOutput::EnterRemote(_)
        ));
        assert_eq!(state.mode(), ControlMode::Remote);
        state
    }

    fn unused_tcp_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn wait_for_events(runtime: &RuntimeHandle, count: usize) -> Vec<RuntimeEvent> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut events = Vec::new();

        while Instant::now() < deadline && events.len() < count {
            events.extend(runtime.drain_events());
            thread::sleep(Duration::from_millis(10));
        }

        events
    }
}
