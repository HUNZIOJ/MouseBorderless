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
    inject::{move_local_pointer_to, InputInjector},
    monitor::virtual_desktop_rect,
};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{interval, sleep_until, Instant as TokioInstant, MissedTickBehavior},
};

use crate::status::{AppStatus, RunState};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(4);
const MAX_OUTSTANDING_HEARTBEATS: usize = 8;
const STALE_POINTER_EMIT_INTERVAL_MILLIS: u64 = 1_000;
const SESSION_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const REMOTE_MOVE_COALESCE_LOG_THRESHOLD: u64 = 100;
const REMOTE_MOVE_COALESCE_LOG_WINDOW_MILLIS: u64 = 1_000;

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
}

async fn run_runtime_loop(commands_rx: Receiver<RuntimeCommand>, events_tx: Sender<RuntimeEvent>) {
    let (updates_tx, updates_rx) = unbounded();
    let mut active: Option<ActiveRuntime> = None;
    let mut status = AppStatus::default();
    let mut stale_pointer_packet_gate = StalePointerPacketGate::default();
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

                        stale_pointer_packet_gate.reset();
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
                                    &mut stale_pointer_packet_gate,
                                );
                            })
                            .ok();
                    }
                    RuntimeCommand::Stop => {
                        if let Some(mut session) = active.take() {
                            let outcome = session.stop().await;
                            log_stop_task_outcome(&mut status, &events_tx, outcome);
                        }

                        stale_pointer_packet_gate.reset();
                        prepare_stopped_status(&mut status);
                        emit_log(&mut status, &events_tx, "runtime stopped");
                        emit_status(&events_tx, &status);
                    }
                    RuntimeCommand::Reconnect(config) => {
                        if let Some(mut session) = active.take() {
                            let outcome = session.stop().await;
                            log_stop_task_outcome(&mut status, &events_tx, outcome);
                        }

                        stale_pointer_packet_gate.reset();
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
                                    &mut stale_pointer_packet_gate,
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
                        &mut stale_pointer_packet_gate,
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
        tasks,
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
        tasks,
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
    let mut remote_input_send_buffer = RemoteInputSendBuffer::new(config.controller.transport_mode);
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
                        local_desktop,
                        &mut control_state,
                        &mut last_pointer,
                        &mut remote_input_send_buffer,
                        &hook_manager,
                        &connection_commands,
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
            }
            command = session_commands.recv() => {
                if matches!(command, Some(SessionCommand::Stop) | None) {
                    set_hook_suppression(&hook_manager, SuppressionMode::PassThrough);
                    emit_remote_send_actions(
                        remote_input_send_buffer.send_release_all(),
                        &connection_commands,
                        &updates,
                        session_id,
                    );
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
                    &hook_manager,
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
    hook_manager: &Arc<Mutex<HookManager>>,
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
            apply_controller_recovery_action(recovery_action, hook_manager, connection_commands);
            send_session_update(updates, session_id, SessionUpdate::Error(error));
        }
        ConnectionEvent::Disconnected(_) | ConnectionEvent::Error(_) => {
            apply_controller_recovery_action(recovery_action, hook_manager, connection_commands);
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
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
) {
    if action.pass_through {
        set_hook_suppression(hook_manager, SuppressionMode::PassThrough);
    }
    if action.release_all {
        send_release_all(connection_commands);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RemoteSendAction {
    Command(ConnectionCommand),
    Log(String),
}

#[derive(Clone, Debug)]
struct RemoteInputSendBuffer {
    transport_mode: TransportMode,
    pending_tcp_move: Option<Point>,
    coalescing_log_gate: RemoteMoveCoalescingLogGate,
}

impl RemoteInputSendBuffer {
    fn new(transport_mode: TransportMode) -> Self {
        Self {
            transport_mode,
            pending_tcp_move: None,
            coalescing_log_gate: RemoteMoveCoalescingLogGate::default(),
        }
    }

    fn send_pointer(&mut self, point: Point, now_millis: u64) -> Vec<RemoteSendAction> {
        match self.transport_mode {
            TransportMode::Kcp => vec![RemoteSendAction::Command(latest_pointer_command(point))],
            TransportMode::Tcp => {
                let mut actions = Vec::new();
                if self.pending_tcp_move.replace(point).is_some() {
                    if let Some(emission) = self.coalescing_log_gate.record(now_millis) {
                        actions.push(RemoteSendAction::Log(emission.log_message()));
                    }
                }
                actions
            }
        }
    }

    fn send_reliable_input(&mut self, event: InputEvent) -> Vec<RemoteSendAction> {
        let mut actions = self.flush_pending_move();
        actions.push(RemoteSendAction::Command(ConnectionCommand::SendReliable(
            WireMessage::Input(event),
        )));
        actions
    }

    fn send_release_all(&mut self) -> Vec<RemoteSendAction> {
        let mut actions = self.flush_pending_move();
        actions.push(RemoteSendAction::Command(ConnectionCommand::SendReliable(
            WireMessage::ReleaseAll,
        )));
        actions
    }

    fn flush_pending_move(&mut self) -> Vec<RemoteSendAction> {
        self.pending_tcp_move
            .take()
            .map(latest_pointer_command)
            .map(RemoteSendAction::Command)
            .into_iter()
            .collect()
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

fn latest_pointer_command(point: Point) -> ConnectionCommand {
    ConnectionCommand::SendLatestPointer {
        x: point.x,
        y: point.y,
    }
}

fn handle_controller_hook_event(
    event: HookEvent,
    local_desktop: Rect,
    control_state: &mut Option<ControlState>,
    last_pointer: &mut Option<Point>,
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
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
                        local_desktop,
                        remote_input_send_buffer,
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
                        local_desktop,
                        remote_input_send_buffer,
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
                    local_desktop,
                    control_state,
                    remote_input_send_buffer,
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
    local_desktop: Rect,
    control_state: &mut Option<ControlState>,
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
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
                    local_desktop,
                    remote_input_send_buffer,
                    hook_manager,
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

fn handle_control_output(
    output: ControlOutput,
    local_desktop: Rect,
    remote_input_send_buffer: &mut RemoteInputSendBuffer,
    hook_manager: &Arc<Mutex<HookManager>>,
    connection_commands: &mpsc::UnboundedSender<ConnectionCommand>,
    updates: &Sender<TaggedSessionUpdate>,
    session_id: u64,
) {
    let return_local_target = return_local_pointer_target(&output);

    match output {
        ControlOutput::None => {}
        ControlOutput::EnterRemote(point) => {
            set_hook_suppression(hook_manager, SuppressionMode::Suppress);
            emit_remote_send_actions(
                remote_input_send_buffer.send_pointer(point, now_millis()),
                connection_commands,
                updates,
                session_id,
            );
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
            emit_remote_send_actions(
                remote_input_send_buffer.send_pointer(point, now_millis()),
                connection_commands,
                updates,
                session_id,
            );
        }
        ControlOutput::ReturnLocal(_) => {
            let Some(point) = return_local_target else {
                return;
            };
            set_hook_suppression(hook_manager, SuppressionMode::PassThrough);
            if let Err(error) = move_local_pointer_to(local_desktop, point) {
                send_session_update(
                    updates,
                    session_id,
                    SessionUpdate::Error(format!("local pointer move failed: {error}")),
                );
            }
            emit_remote_send_actions(
                remote_input_send_buffer.send_release_all(),
                connection_commands,
                updates,
                session_id,
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
    stale_pointer_packet_gate: &mut StalePointerPacketGate,
) {
    let mut status_changed = false;

    match update {
        SessionUpdate::Connection(event) => {
            if let ConnectionEvent::StalePointerPackets { count } = event {
                if let Some(emission) = apply_stale_pointer_packet_update(
                    status,
                    stale_pointer_packet_gate,
                    count,
                    now_millis(),
                ) {
                    emit_log(status, events, emission.log_message());
                    status_changed = true;
                }
            } else {
                status_changed = connection_event_updates_status(&event);
                apply_connection_event_to_status(status, &event);
                if let Some(message) = connection_event_log(&event) {
                    emit_log(status, events, message);
                }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StalePointerPacketEmission {
    count: u64,
}

impl StalePointerPacketEmission {
    fn log_message(self) -> String {
        format!("KCP UDP stale pointer packets: {}", self.count)
    }
}

#[derive(Debug, Default)]
struct StalePointerPacketGate {
    last_emitted_count: u64,
    last_emitted_at_millis: Option<u64>,
}

impl StalePointerPacketGate {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn maybe_emit(&mut self, count: u64, now_millis: u64) -> Option<StalePointerPacketEmission> {
        if self.last_emitted_at_millis.is_some() && count == self.last_emitted_count {
            return None;
        }

        if let Some(last_emitted_at_millis) = self.last_emitted_at_millis {
            let elapsed = now_millis.saturating_sub(last_emitted_at_millis);
            if elapsed < STALE_POINTER_EMIT_INTERVAL_MILLIS {
                return None;
            }
        }

        self.last_emitted_count = count;
        self.last_emitted_at_millis = Some(now_millis);
        Some(StalePointerPacketEmission { count })
    }
}

fn apply_stale_pointer_packet_update(
    status: &mut AppStatus,
    gate: &mut StalePointerPacketGate,
    count: u64,
    now_millis: u64,
) -> Option<StalePointerPacketEmission> {
    status.stale_pointer_packets = count;
    gate.maybe_emit(count, now_millis)
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
        config::{RemotePosition, Role, TransportMode},
        input_event::{InputEvent, KeyEvent, MouseButton, MouseButtonEvent, MouseWheelEvent},
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
        assert!(!connection_event_updates_status(
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
        let mut gate = StalePointerPacketGate::default();

        assert_eq!(
            gate.maybe_emit(1, 1_000)
                .map(StalePointerPacketEmission::log_message),
            Some("KCP UDP stale pointer packets: 1".to_string())
        );
        assert_eq!(gate.maybe_emit(2, 1_500), None);
        assert_eq!(gate.maybe_emit(2, 1_999), None);
        assert_eq!(
            gate.maybe_emit(2, 2_000)
                .map(StalePointerPacketEmission::log_message),
            Some("KCP UDP stale pointer packets: 2".to_string())
        );
        assert_eq!(gate.maybe_emit(3, 2_500), None);
        assert_eq!(
            gate.maybe_emit(3, 3_000)
                .map(StalePointerPacketEmission::log_message),
            Some("KCP UDP stale pointer packets: 3".to_string())
        );
        assert_eq!(gate.maybe_emit(3, 4_000), None);
    }

    #[test]
    fn stale_pointer_packet_updates_emit_latest_count_at_cadence() {
        let mut status = AppStatus::default();
        let mut gate = StalePointerPacketGate::default();

        let samples = [(1, 1_000), (2, 1_010), (3, 1_500), (4, 1_999), (5, 2_000)];
        let emissions = samples
            .into_iter()
            .filter_map(|(count, now)| {
                apply_stale_pointer_packet_update(&mut status, &mut gate, count, now)
                    .map(|emission| emission.count)
            })
            .collect::<Vec<_>>();

        assert_eq!(emissions, vec![1, 5]);
        assert_eq!(status.stale_pointer_packets, 5);
    }

    #[test]
    fn remote_input_send_buffer_coalesces_tcp_moves_and_preserves_reliable_order() {
        let mut buffer = RemoteInputSendBuffer::new(TransportMode::Tcp);
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
                RemoteSendAction::Command(ConnectionCommand::SendLatestPointer { x: 20, y: 20 }),
                RemoteSendAction::Command(ConnectionCommand::SendReliable(WireMessage::Input(
                    key_down
                ))),
                RemoteSendAction::Command(ConnectionCommand::SendLatestPointer { x: 40, y: 40 }),
                RemoteSendAction::Command(ConnectionCommand::SendReliable(WireMessage::Input(
                    left_down
                ))),
                RemoteSendAction::Command(ConnectionCommand::SendReliable(WireMessage::Input(
                    wheel
                ))),
                RemoteSendAction::Command(ConnectionCommand::SendReliable(WireMessage::ReleaseAll)),
            ]
        );
    }

    #[test]
    fn remote_input_send_buffer_keeps_kcp_pointer_moves_on_latest_pointer_path() {
        let mut buffer = RemoteInputSendBuffer::new(TransportMode::Kcp);
        let mut actions = Vec::new();

        actions.extend(buffer.send_pointer(Point::new(10, 10), 1_000));
        actions.extend(buffer.send_pointer(Point::new(20, 20), 1_001));

        assert_eq!(
            actions,
            vec![
                RemoteSendAction::Command(ConnectionCommand::SendLatestPointer { x: 10, y: 10 }),
                RemoteSendAction::Command(ConnectionCommand::SendLatestPointer { x: 20, y: 20 }),
            ]
        );
    }

    #[test]
    fn remote_input_send_buffer_logs_tcp_move_coalescing_once_per_second() {
        let mut buffer = RemoteInputSendBuffer::new(TransportMode::Tcp);
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

    #[tokio::test]
    async fn active_runtime_stop_sends_shutdown_commands_and_awaits_tasks() {
        let (connection_commands_tx, mut connection_commands_rx) = mpsc::unbounded_channel();
        let (session_commands_tx, mut session_commands_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let _ = session_commands_rx.recv().await;
        });
        let mut runtime = ActiveRuntime {
            session_id: 7,
            connection_commands: connection_commands_tx,
            session_commands: session_commands_tx,
            hook_manager: None,
            send_release_all_on_stop: true,
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
            Some(ConnectionCommand::SendReliable(WireMessage::ReleaseAll))
        );
        assert_eq!(
            connection_commands_rx.recv().await,
            Some(ConnectionCommand::Stop)
        );
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
