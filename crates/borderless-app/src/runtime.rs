use std::thread;

use borderless_core::config::{AppConfig, Role, TransportMode};
use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::status::{AppStatus, RunState};

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
            let mut status = AppStatus::default();

            for command in commands_rx {
                match command {
                    RuntimeCommand::Start(config) => {
                        status.run_state = RunState::Connecting;
                        status.transport_mode = Some(runtime_transport_mode(&config));
                        send(
                            &events_tx,
                            RuntimeEvent::Log("starting runtime".to_string()),
                        );
                    }
                    RuntimeCommand::Stop => {
                        status.run_state = RunState::Stopped;
                        send(&events_tx, RuntimeEvent::Log("runtime stopped".to_string()));
                    }
                    RuntimeCommand::Reconnect(config) => {
                        status.run_state = RunState::Reconnecting;
                        status.transport_mode = Some(runtime_transport_mode(&config));
                        send(
                            &events_tx,
                            RuntimeEvent::Log("reconnecting runtime".to_string()),
                        );
                    }
                }

                send(&events_tx, RuntimeEvent::Status(status.clone()));
            }
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

fn send(sender: &Sender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = sender.send(event);
}

fn runtime_transport_mode(config: &AppConfig) -> TransportMode {
    match config.role {
        Role::Controller => config.controller.transport_mode,
        Role::Agent => config.agent.transport_mode,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use borderless_core::config::{Role, TransportMode};

    use super::*;

    #[test]
    fn runtime_emits_log_and_status_for_start_stop_and_reconnect() {
        let runtime = RuntimeHandle::spawn();

        runtime.send(RuntimeCommand::Start(AppConfig::default()));
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

        runtime.send(RuntimeCommand::Reconnect(AppConfig::default()));
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
        let mut config = AppConfig::default();
        config.role = Role::Agent;
        config.controller.transport_mode = TransportMode::Tcp;
        config.agent.transport_mode = TransportMode::Kcp;

        runtime.send(RuntimeCommand::Start(config.clone()));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status)
                    if status.run_state == RunState::Connecting
                        && status.transport_mode == Some(TransportMode::Kcp)
            )
        }));

        runtime.send(RuntimeCommand::Reconnect(config));
        let events = wait_for_events(&runtime, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Status(status)
                    if status.run_state == RunState::Reconnecting
                        && status.transport_mode == Some(TransportMode::Kcp)
            )
        }));
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
