use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use borderless_core::{
    config::{AppConfig, Role, TransportMode},
    control::{ControlMode, ControlOutput, ControlState},
    geometry::{Point, Rect},
    input_event::{InputEvent, MouseMoveAbsEvent},
    protocol::{Heartbeat, Hello, WireMessage, PROTOCOL_VERSION},
};
use borderless_net::{
    agent_server::run_agent_server,
    controller_client::run_controller_client,
    transport::{ConnectionCommand, ConnectionEvent, TransportSettings},
};
use borderless_win::{
    hooks::{HookEvent, HookManager, SuppressionMode},
    inject::InputInjector,
    monitor::virtual_desktop_rect,
};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{interval, MissedTickBehavior},
};

use crate::status::{AppStatus, RunState};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(4);
const MAX_OUTSTANDING_HEARTBEATS: usize = 8;
const STALE_POINTER_LOG_INTERVAL_MILLIS: u64 = 1_000;

#[derive(Clone, Debug)]
pub enum RuntimeCommand {
    Start(AppConfig),
    Stop,
    Reconnect(AppConfig),
}

#[derive(Clone, Debug)]
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
    hook_manager: Option<Arc<Mutex<HookManager>>>,
    send_release_all_on_stop: bool,
    _tasks: Vec<JoinHandle<()>>,
    stopped: bool,
}

impl ActiveRuntime {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;

        if self.send_release_all_on_stop {
            let _ = self
                .connection_commands
                .send(ConnectionCommand::SendReliable(WireMessage::ReleaseAll));
        }
        let _ = self.session_commands.send(SessionCommand::Stop);
        let _ = self.connection_commands.send(ConnectionCommand::Stop);

        if let Some(hook_manager) = self.hook_manager.take() {
            set_hook_suppression(&hook_manager, SuppressionMode::PassThrough);
        }
    }
}

impl Drop for ActiveRuntime {
    fn drop(&mut self) {
        self.stop();
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
}

async fn run_runtime_loop(commands_rx: Receiver<RuntimeCommand>, events_tx: Sender<RuntimeEvent>) {
    let (updates_tx, updates_rx) = unbounded();
    let mut active: Option<ActiveRuntime> = None;
    let mut status = AppStatus::default();
    let mut stale_pointer_log_gate = StalePointerLogGate::default();
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
                            session.stop();
                        }

                        stale_pointer_log_gate.reset();
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
                                    &mut stale_pointer_log_gate,
                                );
                            })
                            .ok();
                    }
                    RuntimeCommand::Stop => {
                        if let Some(mut session) = active.take() {
                            session.stop();
                        }

                        stale_pointer_log_gate.reset();
                        prepare_stopped_status(&mut status);
                        emit_log(&mut status, &events_tx, "runtime stopped");
                        emit_status(&events_tx, &status);
                    }
                    RuntimeCommand::Reconnect(config) => {
                        if let Some(mut session) = active.take() {
                            session.stop();
                        }

                        stale_pointer_log_gate.reset();
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
                                    &mut stale_pointer_log_gate,
                                );
                            })
                            .ok();
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
                        &mut stale_pointer_log_gate,
                    );
                }
            }
        }
    }

    if let Some(mut session) = active {
        session.stop();
    }
}

fn start_session(
    session_id: u64,
    config: AppConfig,
    updates: Sender<TaggedSessionUpdate>,
) -> Result<ActiveRuntime, String> {
    match config.role {
        Role::Controller => start_controller_session(session_id, config, updates),
        Role::Agent => Ok(start_agent_session(session_id, config, updates)),
    }
}

fn start_controller_session(
    session_id: u64,
    config: AppConfig,
    updates: Sender<TaggedSessionUpdate>,
) -> Result<ActiveRuntime, String> {
    let local_desktop = virtual_desktop_rect();
    let settings = controller_transport_settings(&config);
    let (connection_events_tx, connection_events_rx) = mpsc::unbounded_channel();
    let (connection_commands_tx, connection_commands_rx) = mpsc::unbounded_channel();
    let (session_commands_tx, session_commands_rx) = mpsc::unbounded_channel();
    let (hook_events_tx, hook_events_rx) = unbounded();
    let hook_manager = Arc::new(Mutex::new(
        HookManager::install(hook_events_tx).map_err(|error| error.to_string())?,
    ));
    let mut tasks = Vec::new();

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
    tasks.push(tokio::spawn(async move {
        run_controller_event_pump(
            session_id,
            config,
            local_desktop,
            pump_hook_manager,
            hook_events_rx,
            connection_events_rx,
            pump_commands,
            session_commands_rx,
            pump_updates,
        )
        .await;
    }));

    Ok(ActiveRuntime {
        session_id,
        connection_commands: connection_commands_tx,
        session_commands: session_commands_tx,
        hook_manager: Some(hook_manager),
        send_release_all_on_stop: true,
        _tasks: tasks,
        stopped: false,
    })
}

fn start_agent_session(
    session_id: u64,
    config: AppConfig,
    updates: Sender<TaggedSessionUpdate>,
) -> ActiveRuntime {
    let local_desktop = virtual_desktop_rect();
    let settings = agent_transport_settings(&config);
    let (connection_events_tx, connection_events_rx) = mpsc::unbounded_channel();
    let (connection_commands_tx, connection_commands_rx) = mpsc::unbounded_channel();
    let (session_commands_tx, session_commands_rx) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();

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
            local_desktop,
            connection_events_rx,
            pump_commands,
            session_commands_rx,
            pump_updates,
        )
        .await;
    }));

    ActiveRuntime {
        session_id,
        connection_commands: connection_commands_tx,
        session_commands: session_commands_tx,
        hook_manager: None,
        send_release_all_on_stop: false,
        _tasks: tasks,
        stopped: false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_controller_event_pump(
    session_id: u64,
    config: AppConfig,
    local_desktop: Rect,
    hook_manager: Arc<Mutex<HookManager>>,
    hook_events: Receiver<HookEvent>,
    mut connection_events: mpsc::UnboundedReceiver<ConnectionEvent>,
    connection_commands: mpsc::UnboundedSender<ConnectionCommand>,
    mut session_commands: mpsc::UnboundedReceiver<SessionCommand>,
    updates: Sender<TaggedSessionUpdate>,
) {
    let mut control_state: Option<ControlState> = None;
    let mut last_pointer: Option<Point> = None;
    let mut heartbeat = HeartbeatTracker::default();
    let mut heartbeat_interval = interval(HEARTBEAT_INTERVAL);
    let mut hook_interval = interval(HOOK_POLL_INTERVAL);
    heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    hook_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = heartbeat_interval.tick() => {
                send_heartbeat(&connection_commands, &mut heartbeat);
            }
            _ = hook_interval.tick() => {
                while let Ok(event) = hook_events.try_recv() {
                    handle_controller_hook_event(
                        event,
                        &mut control_state,
                        &mut last_pointer,
                        &hook_manager,
                        &connection_commands,
                        &updates,
                        session_id,
                    );
                }
            }
            command = session_commands.recv() => {
                if matches!(command, Some(SessionCommand::Stop) | None) {
                    set_hook_suppression(&hook_manager, SuppressionMode::PassThrough);
                    send_release_all(&connection_commands);
                    break;
                }
            }
            event = connection_events.recv() => {
                let Some(event) = event else {
                    break;
                };

                handle_controller_connection_event(
                    event,
                    &config,
                    local_desktop,
                    &mut control_state,
                    &connection_commands,
                    &updates,
                    session_id,
                    &mut heartbeat,
                );
            }
        }
    }

    set_hook_suppression(&hook_manager, SuppressionMode::PassThrough);
}

fn handle_controller_connection_event(
    event: ConnectionEvent,
    config: &AppConfig,
    local_desktop: Rect,
    control_state: &mut Option<ControlState>,
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
        ConnectionEvent::Message(WireMessage::Hello(hello)) => {
            if hello.protocol_version != PROTOCOL_VERSION {
                let error = format!(
                    "protocol version mismatch: expected {}, got {}",
                    PROTOCOL_VERSION, hello.protocol_version
                );
                let _ = connection_commands.send(ConnectionCommand::SendReliable(
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
        ConnectionEvent::Message(WireMessage::Error(error)) => {
            send_session_update(updates, session_id, SessionUpdate::Error(error));
        }
        ConnectionEvent::Disconnected(_) | ConnectionEvent::Error(_) => {
            *control_state = None;
        }
        ConnectionEvent::Waiting
        | ConnectionEvent::Connecting(_)
        | ConnectionEvent::Connected { .. }
        | ConnectionEvent::LatestPointer { .. }
        | ConnectionEvent::StalePointerPackets { .. }
        | ConnectionEvent::Message(WireMessage::Input(_))
        | ConnectionEvent::Message(WireMessage::ReleaseAll) => {}
    }
}

fn handle_controller_hook_event(
    event: HookEvent,
    control_state: &mut Option<ControlState>,
    last_pointer: &mut Option<Point>,
    hook_manager: &Arc<Mutex<HookManager>>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    match event {
        HookEvent::PointerPosition { x, y } => {
            let point = Point::new(x, y);
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
                        hook_manager,
                        connection_commands,
                        updates,
                        session_id,
                    );
                }
                ControlMode::Remote => {
                    let previous = last_pointer.replace(point).unwrap_or(point);
                    let dx = point.x.saturating_sub(previous.x);
                    let dy = point.y.saturating_sub(previous.y);
                    if dx == 0 && dy == 0 {
                        return;
                    }

                    let output = state.apply_remote_delta(dx, dy);
                    handle_control_output(
                        output,
                        hook_manager,
                        connection_commands,
                        updates,
                        session_id,
                    );
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
                    control_state,
                    hook_manager,
                    connection_commands,
                    updates,
                    session_id,
                );
            }
        }
    }
}

fn send_remote_input(
    input: InputEvent,
    control_state: &mut Option<ControlState>,
    hook_manager: &Arc<Mutex<HookManager>>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    match input {
        InputEvent::MouseMoveAbs(_) => {}
        InputEvent::MouseMoveDelta(delta) => {
            if let Some(state) = control_state.as_mut() {
                let output = state.apply_remote_delta(delta.dx, delta.dy);
                handle_control_output(
                    output,
                    hook_manager,
                    connection_commands,
                    updates,
                    session_id,
                );
            }
        }
        InputEvent::ReleaseAll => send_release_all(connection_commands),
        event => {
            let _ = connection_commands
                .send(ConnectionCommand::SendReliable(WireMessage::Input(event)));
        }
    }
}

fn handle_control_output(
    output: ControlOutput,
    hook_manager: &Arc<Mutex<HookManager>>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    match output {
        ControlOutput::None => {}
        ControlOutput::EnterRemote(point) => {
            set_hook_suppression(hook_manager, SuppressionMode::Suppress);
            send_latest_pointer(connection_commands, point);
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
            send_latest_pointer(connection_commands, point);
        }
        ControlOutput::ReturnLocal(_) => {
            set_hook_suppression(hook_manager, SuppressionMode::PassThrough);
            send_release_all(connection_commands);
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

async fn run_agent_event_pump(
    session_id: u64,
    local_desktop: Rect,
    mut connection_events: mpsc::UnboundedReceiver<ConnectionEvent>,
    connection_commands: mpsc::UnboundedSender<ConnectionCommand>,
    mut session_commands: mpsc::UnboundedReceiver<SessionCommand>,
    updates: Sender<TaggedSessionUpdate>,
) {
    let mut injector: Option<InputInjector> = None;
    let mut heartbeat = HeartbeatTracker::default();

    loop {
        tokio::select! {
            command = session_commands.recv() => {
                if matches!(command, Some(SessionCommand::Stop) | None) {
                    release_agent_input(&mut injector, &updates, session_id);
                    break;
                }
            }
            event = connection_events.recv() => {
                let Some(event) = event else {
                    break;
                };

                handle_agent_connection_event(
                    event,
                    local_desktop,
                    &mut injector,
                    &connection_commands,
                    &updates,
                    session_id,
                    &mut heartbeat,
                );
            }
        }
    }

    release_agent_input(&mut injector, &updates, session_id);
}

fn handle_agent_connection_event(
    event: ConnectionEvent,
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
            let _ = connection_commands.send(ConnectionCommand::SendReliable(WireMessage::Hello(
                Hello {
                    protocol_version: PROTOCOL_VERSION,
                    desktop: local_desktop,
                },
            )));
        }
        ConnectionEvent::Disconnected(_) => {
            release_agent_input(injector, updates, session_id);
            *injector = None;
        }
        ConnectionEvent::Message(WireMessage::Input(input)) => {
            inject_agent_input(injector, input, updates, session_id);
        }
        ConnectionEvent::LatestPointer { x, y, .. } => {
            inject_agent_input(
                injector,
                InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x, y }),
                updates,
                session_id,
            );
        }
        ConnectionEvent::Message(WireMessage::ReleaseAll) => {
            release_agent_input(injector, updates, session_id);
        }
        ConnectionEvent::Message(WireMessage::Heartbeat(message)) => {
            handle_heartbeat_message(message, heartbeat, connection_commands, updates, session_id);
        }
        ConnectionEvent::Message(WireMessage::Error(error)) => {
            send_session_update(updates, session_id, SessionUpdate::Error(error));
        }
        ConnectionEvent::Error(_) => {
            release_agent_input(injector, updates, session_id);
            *injector = None;
        }
        ConnectionEvent::Waiting
        | ConnectionEvent::Connecting(_)
        | ConnectionEvent::StalePointerPackets { .. }
        | ConnectionEvent::Message(WireMessage::Hello(_)) => {}
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
) {
    if let Some(injector) = injector.as_mut() {
        if let Err(error) = injector.release_all() {
            send_session_update(
                updates,
                session_id,
                SessionUpdate::Error(format!("release all failed: {error}")),
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
        let _ = connection_commands.send(ConnectionCommand::SendReliable(WireMessage::Heartbeat(
            message,
        )));
    }
}

fn send_heartbeat(
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    heartbeat: &mut HeartbeatTracker,
) {
    let sent_millis = now_millis();
    heartbeat.sent(sent_millis);
    let _ = connection_commands.send(ConnectionCommand::SendReliable(WireMessage::Heartbeat(
        Heartbeat { sent_millis },
    )));
}

fn send_latest_pointer(
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    point: Point,
) {
    let _ = connection_commands.send(ConnectionCommand::SendLatestPointer {
        x: point.x,
        y: point.y,
    });
}

fn send_release_all(connection_commands: &mpsc::UnboundedSender<ConnectionCommand>) {
    let _ = connection_commands.send(ConnectionCommand::SendReliable(WireMessage::ReleaseAll));
}

fn set_hook_suppression(hook_manager: &Arc<Mutex<HookManager>>, suppression_mode: SuppressionMode) {
    if let Ok(manager) = hook_manager.lock() {
        manager.set_suppression_mode(suppression_mode);
    }
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
    status.transport_mode = Some(runtime_transport_mode(config));
}

fn prepare_stopped_status(status: &mut AppStatus) {
    status.reset_runtime_fields();
    status.run_state = RunState::Stopped;
}

fn apply_session_update(
    status: &mut AppStatus,
    events: &Sender<RuntimeEvent>,
    update: SessionUpdate,
    stale_pointer_log_gate: &mut StalePointerLogGate,
) {
    let mut status_changed = false;

    match update {
        SessionUpdate::Connection(event) => {
            status_changed = connection_event_updates_status(&event);
            apply_connection_event_to_status(status, &event);
            let log_message = match &event {
                ConnectionEvent::StalePointerPackets { count } => {
                    stale_pointer_log_gate.maybe_log(*count, now_millis())
                }
                _ => connection_event_log(&event),
            };
            if let Some(message) = log_message {
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
    }

    if status_changed {
        emit_status(events, status);
    }
}

fn apply_connection_event_to_status(status: &mut AppStatus, event: &ConnectionEvent) {
    match event {
        ConnectionEvent::Waiting => status.run_state = RunState::Waiting,
        ConnectionEvent::Connecting(_) => status.run_state = RunState::Connecting,
        ConnectionEvent::Connected { mode, .. } => {
            status.run_state = RunState::Connected;
            status.transport_mode = Some(*mode);
            status.last_error = None;
        }
        ConnectionEvent::Disconnected(_) => status.run_state = RunState::Reconnecting,
        ConnectionEvent::LatestPointer { sequence, .. } => {
            status.latest_pointer_sequence = Some(*sequence);
        }
        ConnectionEvent::StalePointerPackets { count } => {
            status.stale_pointer_packets = *count;
        }
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
            | ConnectionEvent::LatestPointer { .. }
            | ConnectionEvent::StalePointerPackets { .. }
            | ConnectionEvent::Error(_)
            | ConnectionEvent::Message(WireMessage::Error(_))
    )
}

fn connection_event_log(event: &ConnectionEvent) -> Option<String> {
    match event {
        ConnectionEvent::Waiting => Some("waiting for connection".to_string()),
        ConnectionEvent::Connecting(peer) => Some(format!("connection endpoint {peer}")),
        ConnectionEvent::Connected { peer, mode } => Some(format!("connected {peer} via {mode:?}")),
        ConnectionEvent::Disconnected(peer) => Some(format!("disconnected {peer}")),
        ConnectionEvent::Error(error) => Some(format!("connection error: {error}")),
        ConnectionEvent::Message(WireMessage::Error(error)) => Some(format!("peer error: {error}")),
        ConnectionEvent::Message(_)
        | ConnectionEvent::LatestPointer { .. }
        | ConnectionEvent::StalePointerPackets { .. } => None,
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

fn runtime_transport_mode(config: &AppConfig) -> TransportMode {
    match &config.role {
        Role::Controller => config.controller.transport_mode,
        Role::Agent => config.agent.transport_mode,
    }
}

fn controller_transport_settings(config: &AppConfig) -> TransportSettings {
    TransportSettings {
        mode: config.controller.transport_mode,
        host: config.controller.agent_host.clone(),
        reliable_port: config.controller.agent_port,
        pointer_port: config.controller.pointer_port,
    }
}

fn agent_transport_settings(config: &AppConfig) -> TransportSettings {
    TransportSettings {
        mode: config.agent.transport_mode,
        host: config.agent.listen_host.clone(),
        reliable_port: config.agent.listen_port,
        pointer_port: config.agent.pointer_port,
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

#[derive(Debug, Default)]
struct StalePointerLogGate {
    last_logged_count: u64,
    last_logged_at_millis: Option<u64>,
}

impl StalePointerLogGate {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn maybe_log(&mut self, count: u64, now_millis: u64) -> Option<String> {
        if count == self.last_logged_count {
            return None;
        }

        if let Some(last_logged_at_millis) = self.last_logged_at_millis {
            let elapsed = now_millis.saturating_sub(last_logged_at_millis);
            if elapsed < STALE_POINTER_LOG_INTERVAL_MILLIS {
                return None;
            }
        }

        self.last_logged_count = count;
        self.last_logged_at_millis = Some(now_millis);
        Some(format!("KCP UDP stale pointer packets: {count}"))
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
        net::{TcpListener, UdpSocket},
        time::{Duration, Instant},
    };

    use borderless_core::{
        config::{Role, TransportMode},
        protocol::WireMessage,
    };
    use borderless_net::transport::ConnectionEvent;

    use super::*;

    #[test]
    fn controller_transport_settings_map_controller_config() {
        let mut config = AppConfig::default();
        config.role = Role::Controller;
        config.controller.agent_host = "agent.local".to_string();
        config.controller.agent_port = 34567;
        config.controller.transport_mode = TransportMode::Kcp;
        config.controller.pointer_port = 34568;

        let settings = controller_transport_settings(&config);

        assert_eq!(settings.mode, TransportMode::Kcp);
        assert_eq!(settings.host, "agent.local");
        assert_eq!(settings.reliable_port, 34567);
        assert_eq!(settings.pointer_port, 34568);
    }

    #[test]
    fn agent_transport_settings_map_agent_config() {
        let mut config = AppConfig::default();
        config.role = Role::Agent;
        config.agent.listen_host = "127.0.0.1".to_string();
        config.agent.listen_port = 45678;
        config.agent.transport_mode = TransportMode::Kcp;
        config.agent.pointer_port = 45679;

        let settings = agent_transport_settings(&config);

        assert_eq!(settings.mode, TransportMode::Kcp);
        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.reliable_port, 45678);
        assert_eq!(settings.pointer_port, 45679);
    }

    #[test]
    fn connection_events_update_status_state_sequence_and_error() {
        let mut status = AppStatus::default();

        apply_connection_event_to_status(
            &mut status,
            &ConnectionEvent::Connected {
                peer: "127.0.0.1:24800".to_string(),
                mode: TransportMode::Kcp,
            },
        );
        assert_eq!(status.run_state, RunState::Connected);
        assert_eq!(status.transport_mode, Some(TransportMode::Kcp));

        apply_connection_event_to_status(
            &mut status,
            &ConnectionEvent::LatestPointer {
                x: 10,
                y: 20,
                sequence: 99,
            },
        );
        assert_eq!(status.latest_pointer_sequence, Some(99));
        apply_connection_event_to_status(
            &mut status,
            &ConnectionEvent::StalePointerPackets { count: 4 },
        );
        assert_eq!(status.stale_pointer_packets, 4);
        assert!(connection_event_updates_status(
            &ConnectionEvent::StalePointerPackets { count: 4 }
        ));

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
    fn stale_pointer_log_gate_logs_count_changes_at_most_once_per_second() {
        let mut gate = StalePointerLogGate::default();

        assert_eq!(
            gate.maybe_log(1, 1_000),
            Some("KCP UDP stale pointer packets: 1".to_string())
        );
        assert_eq!(gate.maybe_log(2, 1_500), None);
        assert_eq!(gate.maybe_log(2, 1_999), None);
        assert_eq!(
            gate.maybe_log(2, 2_000),
            Some("KCP UDP stale pointer packets: 2".to_string())
        );
        assert_eq!(gate.maybe_log(3, 2_500), None);
        assert_eq!(
            gate.maybe_log(3, 3_000),
            Some("KCP UDP stale pointer packets: 3".to_string())
        );
        assert_eq!(gate.maybe_log(3, 4_000), None);
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
        let config = safe_agent_config(TransportMode::Tcp);

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
                RuntimeEvent::Status(status)
                    if status.run_state == RunState::Stopped && status.transport_mode.is_none()
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

    #[test]
    fn runtime_command_is_cloneable() {
        let command = RuntimeCommand::Start(AppConfig::default());
        let cloned = command.clone();

        assert!(matches!(cloned, RuntimeCommand::Start(_)));
    }

    #[test]
    fn agent_start_and_reconnect_report_agent_transport_mode() {
        let runtime = RuntimeHandle::spawn();
        let config = safe_agent_config(TransportMode::Tcp);

        runtime.send(RuntimeCommand::Start(config.clone()));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status)
                    if status.run_state == RunState::Connecting
                        && status.transport_mode == Some(TransportMode::Tcp)
            )
        }));

        runtime.send(RuntimeCommand::Reconnect(config));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status)
                    if status.run_state == RunState::Reconnecting
                        && status.transport_mode == Some(TransportMode::Tcp)
            )
        }));

        runtime.send(RuntimeCommand::Stop);
    }

    fn safe_agent_config(transport_mode: TransportMode) -> AppConfig {
        let mut config = AppConfig::default();
        config.role = Role::Agent;
        config.agent.listen_host = "127.0.0.1".to_string();
        config.agent.transport_mode = transport_mode;
        config.agent.listen_port = match transport_mode {
            TransportMode::Tcp => unused_tcp_port(),
            TransportMode::Kcp => unused_udp_port(),
        };
        config.agent.pointer_port = unused_udp_port();
        config
    }

    fn unused_tcp_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn unused_udp_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0")
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
