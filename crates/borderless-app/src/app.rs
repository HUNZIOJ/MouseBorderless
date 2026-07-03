use std::{path::Path, time::Duration};

use borderless_core::config::{AppConfig, RemotePosition, Role, TransportMode};
use eframe::egui;

use crate::{
    runtime::{RuntimeCommand, RuntimeEvent, RuntimeHandle},
    status::{AppStatus, RunState},
};

const CONFIG_PATH: &str = "config.toml";

pub struct BorderlessApp {
    config: AppConfig,
    status: AppStatus,
    runtime: RuntimeHandle,
    config_error: Option<String>,
}

impl BorderlessApp {
    pub fn new() -> Self {
        let (config, config_error) = match AppConfig::load_from_path(CONFIG_PATH) {
            Ok(config) => (config, None),
            Err(err) => (
                AppConfig::default(),
                Path::new(CONFIG_PATH)
                    .exists()
                    .then(|| format!("Failed to load {CONFIG_PATH}: {err}")),
            ),
        };

        Self {
            config,
            status: AppStatus::default(),
            runtime: RuntimeHandle::spawn(),
            config_error,
        }
    }

    fn save_config(&mut self) -> bool {
        self.config_error = match self.config.save_to_path(CONFIG_PATH) {
            Ok(()) => None,
            Err(err) => Some(format!("Failed to save {CONFIG_PATH}: {err}")),
        };

        self.config_error.is_none()
    }

    fn start_runtime(&mut self) {
        if self.save_config() {
            self.runtime
                .send(RuntimeCommand::Start(self.config.clone()));
        }
    }

    fn reconnect_runtime(&mut self) {
        if self.save_config() {
            self.runtime
                .send(RuntimeCommand::Reconnect(self.config.clone()));
        }
    }

    fn handle_runtime_events(&mut self) {
        for event in self.runtime.drain_events() {
            match event {
                RuntimeEvent::Status(mut status) => {
                    status.events = self.status.events.clone();
                    self.status = status;
                }
                RuntimeEvent::Log(message) => self.status.push_log(message),
            }
        }
    }

    fn show_role(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Role");
            ui.radio_value(&mut self.config.role, Role::Controller, "Controller");
            ui.radio_value(&mut self.config.role, Role::Agent, "Agent");
        });
    }

    fn show_controller(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Controller")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("controller_config")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Agent IP");
                        ui.text_edit_singleline(&mut self.config.controller.agent_host);
                        ui.end_row();

                        ui.label("Port");
                        ui.add(
                            egui::DragValue::new(&mut self.config.controller.agent_port)
                                .range(1..=u16::MAX),
                        );
                        ui.end_row();

                        ui.label("Transport");
                        transport_selector(ui, &mut self.config.controller.transport_mode);
                        ui.end_row();

                        ui.label("Pointer UDP");
                        ui.add(
                            egui::DragValue::new(&mut self.config.controller.pointer_port)
                                .range(1..=u16::MAX),
                        );
                        ui.end_row();

                        ui.label("Remote position");
                        remote_position_selector(ui, &mut self.config.controller.remote_position);
                        ui.end_row();
                    });
            });
    }

    fn show_agent(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Agent")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("agent_config")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Listen IP");
                        ui.text_edit_singleline(&mut self.config.agent.listen_host);
                        ui.end_row();

                        ui.label("Port");
                        ui.add(
                            egui::DragValue::new(&mut self.config.agent.listen_port)
                                .range(1..=u16::MAX),
                        );
                        ui.end_row();

                        ui.label("Transport");
                        transport_selector(ui, &mut self.config.agent.transport_mode);
                        ui.end_row();

                        ui.label("Pointer UDP");
                        ui.add(
                            egui::DragValue::new(&mut self.config.agent.pointer_port)
                                .range(1..=u16::MAX),
                        );
                        ui.end_row();
                    });
            });
    }

    fn show_sharing(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Clipboard/files")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.config.sharing.clipboard_text, "Clipboard text");
                    ui.checkbox(&mut self.config.sharing.clipboard_html, "Clipboard HTML");
                    ui.checkbox(
                        &mut self.config.sharing.clipboard_images,
                        "Clipboard images",
                    );
                    ui.checkbox(&mut self.config.sharing.file_copy_paste, "File copy paste");
                    ui.checkbox(
                        &mut self.config.sharing.real_file_drag_drop,
                        "Real file drag drop",
                    );
                });

                ui.separator();

                egui::Grid::new("sharing_config")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Bulk transfer port");
                        ui.add(
                            egui::DragValue::new(&mut self.config.sharing.bulk_transfer_port)
                                .range(1..=u16::MAX),
                        );
                        ui.end_row();

                        ui.label("Max clipboard bytes");
                        ui.add(
                            egui::DragValue::new(&mut self.config.sharing.max_clipboard_bytes)
                                .range(1..=u64::MAX),
                        );
                        ui.end_row();

                        ui.label("Max file bytes");
                        ui.add(
                            egui::DragValue::new(&mut self.config.sharing.max_file_transfer_bytes)
                                .range(1..=u64::MAX),
                        );
                        ui.end_row();

                        ui.label("Incoming cache");
                        ui.text_edit_singleline(&mut self.config.sharing.incoming_cache_dir);
                        ui.end_row();
                    });
            });
    }

    fn show_status(&self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Status")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("status_values")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("State");
                        ui.colored_label(
                            state_color(&self.status.run_state),
                            format!("{:?}", self.status.run_state),
                        );
                        ui.end_row();

                        ui.label("Transport");
                        ui.label(option_text(self.status.transport_mode));
                        ui.end_row();

                        ui.label("RTT");
                        ui.label(option_ms(self.status.recent_rtt_ms));
                        ui.end_row();

                        ui.label("Average RTT");
                        ui.label(option_ms(self.status.average_rtt_ms));
                        ui.end_row();

                        ui.label("Latest pointer sequence");
                        ui.label(option_number(self.status.latest_pointer_sequence));
                        ui.end_row();

                        ui.label("Stale pointer packets");
                        ui.label(self.status.stale_pointer_packets.to_string());
                        ui.end_row();

                        ui.label("Clipboard enabled");
                        ui.label(if self.status.clipboard_enabled {
                            "yes"
                        } else {
                            "no"
                        });
                        ui.end_row();

                        ui.label("Clipboard format");
                        ui.label(option_text(self.status.last_clipboard_format.as_deref()));
                        ui.end_row();

                        ui.label("Clipboard bytes");
                        ui.label(option_number(self.status.last_clipboard_bytes));
                        ui.end_row();

                        ui.label("Clipboard ignored");
                        ui.label(option_text(self.status.clipboard_ignored_reason.as_deref()));
                        ui.end_row();

                        ui.label("Transfer");
                        ui.label(if self.status.transfer_active {
                            format!(
                                "{}/{} bytes",
                                self.status.transfer_bytes_done, self.status.transfer_bytes_total
                            )
                        } else {
                            "-".to_string()
                        });
                        ui.end_row();

                        ui.label("Transfer file");
                        ui.label(option_text(self.status.transfer_current_file.as_deref()));
                        ui.end_row();

                        ui.label("Drag/drop enabled");
                        ui.label(if self.status.drag_drop_enabled {
                            "yes"
                        } else {
                            "no"
                        });
                        ui.end_row();

                        ui.label("Drag/drop state");
                        ui.label(option_text(self.status.drag_drop_state.as_deref()));
                        ui.end_row();

                        ui.label("Mouse diagnostics");
                        ui.label(option_text(self.status.mouse_diagnostics.as_deref()));
                        ui.end_row();
                    });

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            self.status.transfer_active && self.status.transfer_id.is_some(),
                            egui::Button::new("Cancel transfer"),
                        )
                        .clicked()
                    {
                        if let Some(transfer_id) = self.status.transfer_id {
                            self.runtime
                                .send(RuntimeCommand::CancelTransfer(transfer_id));
                        }
                    }

                    if ui
                        .add_enabled(
                            self.status.active_drag_session.is_some(),
                            egui::Button::new("Cancel drag"),
                        )
                        .clicked()
                    {
                        if let Some(session_id) = self.status.active_drag_session {
                            self.runtime
                                .send(RuntimeCommand::CancelDragDrop(session_id));
                        }
                    }
                });
            });
    }

    fn show_event_log(&self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Event log")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for event in &self.status.events {
                            ui.label(event);
                        }
                    });
            });
    }
}

impl eframe::App for BorderlessApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_runtime_events();
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Borderless");
                ui.add_space(8.0);

                self.show_role(ui);
                ui.separator();

                self.show_controller(ui);
                self.show_agent(ui);

                if self.config.controller.transport_mode == TransportMode::Kcp
                    || self.config.agent.transport_mode == TransportMode::Kcp
                {
                    ui.label("KCP uses UDP for reliable control and a separate UDP port for pointer updates.");
                }

                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Edge trigger");
                    ui.add(
                        egui::Slider::new(&mut self.config.edge_trigger_px, 1..=32).suffix(" px"),
                    );
                    ui.checkbox(&mut self.config.debug_logging, "Debug");
                });

                self.show_sharing(ui);

                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.save_config();
                    }

                    if ui
                        .add_enabled(can_start(self.status.run_state), egui::Button::new("Start"))
                        .clicked()
                    {
                        self.start_runtime();
                    }

                    if ui
                        .add_enabled(can_stop(self.status.run_state), egui::Button::new("Stop"))
                        .clicked()
                    {
                        self.runtime.send(RuntimeCommand::Stop);
                    }

                    if ui
                        .add_enabled(
                            can_reconnect(self.status.run_state),
                            egui::Button::new("Reconnect"),
                        )
                        .clicked()
                    {
                        self.reconnect_runtime();
                    }
                });

                if let Some(error) = &self.config_error {
                    ui.colored_label(egui::Color32::RED, error);
                }

                if should_show_permission_guidance(&self.status) {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "If the target window runs as administrator, run Borderless as administrator on both computers.",
                    );
                }

                self.show_status(ui);
                self.show_event_log(ui);
            });
        });
    }
}

fn transport_selector(ui: &mut egui::Ui, transport_mode: &mut TransportMode) {
    ui.horizontal(|ui| {
        ui.radio_value(transport_mode, TransportMode::Tcp, "TCP");
        ui.radio_value(transport_mode, TransportMode::Kcp, "KCP");
    });
}

fn remote_position_selector(ui: &mut egui::Ui, remote_position: &mut RemotePosition) {
    ui.horizontal(|ui| {
        ui.radio_value(remote_position, RemotePosition::Left, "Left");
        ui.radio_value(remote_position, RemotePosition::Right, "Right");
        ui.radio_value(remote_position, RemotePosition::Top, "Top");
        ui.radio_value(remote_position, RemotePosition::Bottom, "Bottom");
    });
}

fn option_text<T: std::fmt::Debug>(value: Option<T>) -> String {
    value
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "-".to_string())
}

fn option_ms(value: Option<u64>) -> String {
    value
        .map(|value| format!("{value} ms"))
        .unwrap_or_else(|| "-".to_string())
}

fn option_number(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn state_color(state: &RunState) -> egui::Color32 {
    match state {
        RunState::Stopped => egui::Color32::GRAY,
        RunState::Waiting | RunState::Connecting | RunState::Reconnecting => egui::Color32::YELLOW,
        RunState::Connected | RunState::LocalControl => egui::Color32::GREEN,
        RunState::RemoteControl => egui::Color32::LIGHT_BLUE,
        RunState::Error => egui::Color32::RED,
    }
}

fn can_start(state: RunState) -> bool {
    matches!(state, RunState::Stopped | RunState::Error)
}

fn can_stop(state: RunState) -> bool {
    state != RunState::Stopped
}

fn can_reconnect(state: RunState) -> bool {
    state != RunState::Stopped
}

fn should_show_permission_guidance(status: &AppStatus) -> bool {
    status
        .last_error
        .as_deref()
        .is_some_and(is_permission_sensitive_error)
}

fn is_permission_sensitive_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("hook") || message.contains("input injection") || message.contains("sendinput")
}

#[cfg(test)]
mod tests {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn start_runtime_does_not_send_start_when_config_is_invalid() {
        let runtime = RuntimeHandle::spawn();
        let mut app = BorderlessApp {
            config: AppConfig::default(),
            status: AppStatus::default(),
            runtime,
            config_error: None,
        };
        app.config.edge_trigger_px = 0;

        app.start_runtime();

        assert!(app
            .config_error
            .as_deref()
            .is_some_and(|error| error.contains("edge_trigger_px")));
        assert!(wait_for_events(&app.runtime, Duration::from_millis(100)).is_empty());
    }

    #[test]
    fn reconnect_runtime_does_not_send_reconnect_when_config_is_invalid() {
        let runtime = RuntimeHandle::spawn();
        let mut app = BorderlessApp {
            config: AppConfig::default(),
            status: AppStatus::default(),
            runtime,
            config_error: None,
        };
        app.config.edge_trigger_px = 0;

        app.reconnect_runtime();

        assert!(app
            .config_error
            .as_deref()
            .is_some_and(|error| error.contains("edge_trigger_px")));
        assert!(wait_for_events(&app.runtime, Duration::from_millis(100)).is_empty());
    }

    #[test]
    fn action_buttons_follow_runtime_state() {
        let states = [
            RunState::Stopped,
            RunState::Waiting,
            RunState::Connecting,
            RunState::Connected,
            RunState::LocalControl,
            RunState::RemoteControl,
            RunState::Reconnecting,
            RunState::Error,
        ];

        for state in states {
            assert_eq!(
                can_start(state),
                matches!(state, RunState::Stopped | RunState::Error),
                "unexpected Start enabled state for {state:?}"
            );
            assert_eq!(
                can_stop(state),
                state != RunState::Stopped,
                "unexpected Stop enabled state for {state:?}"
            );
            assert_eq!(
                can_reconnect(state),
                state != RunState::Stopped,
                "unexpected Reconnect enabled state for {state:?}"
            );
        }
    }

    #[test]
    fn state_colors_cover_every_runtime_state() {
        let cases = [
            (RunState::Stopped, egui::Color32::GRAY),
            (RunState::Waiting, egui::Color32::YELLOW),
            (RunState::Connecting, egui::Color32::YELLOW),
            (RunState::Connected, egui::Color32::GREEN),
            (RunState::LocalControl, egui::Color32::GREEN),
            (RunState::RemoteControl, egui::Color32::LIGHT_BLUE),
            (RunState::Reconnecting, egui::Color32::YELLOW),
            (RunState::Error, egui::Color32::RED),
        ];

        for (state, color) in cases {
            assert_eq!(state_color(&state), color);
        }
    }

    #[test]
    fn permission_guidance_only_shows_for_input_or_hook_errors() {
        let mut status = AppStatus {
            last_error: Some("input injection failed: access denied".to_string()),
            ..AppStatus::default()
        };
        assert!(should_show_permission_guidance(&status));

        status.last_error = Some("access denied while opening config.toml".to_string());
        assert!(!should_show_permission_guidance(&status));

        status.last_error = Some("connection refused".to_string());
        assert!(!should_show_permission_guidance(&status));

        status.last_error = None;
        status.push_log("failed to install hook");
        assert!(!should_show_permission_guidance(&status));

        status.last_error = Some("SendInput sent 0 of 1 input events".to_string());
        assert!(should_show_permission_guidance(&status));
    }

    fn wait_for_events(runtime: &RuntimeHandle, timeout: Duration) -> Vec<RuntimeEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();

        while Instant::now() < deadline {
            events.extend(runtime.drain_events());
            if !events.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        events
    }
}
