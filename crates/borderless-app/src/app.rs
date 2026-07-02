use std::{path::Path, time::Duration};

use borderless_core::config::{AppConfig, RemotePosition, Role, TransportMode};
use eframe::egui;

use crate::{
    runtime::{RuntimeCommand, RuntimeEvent, RuntimeHandle},
    status::AppStatus,
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
                        ui.label(format!("{:?}", self.status.run_state));
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

                    if ui.button("Start").clicked() {
                        self.start_runtime();
                    }

                    if ui.button("Stop").clicked() {
                        self.runtime.send(RuntimeCommand::Stop);
                    }

                    if ui.button("Reconnect").clicked() {
                        self.runtime
                            .send(RuntimeCommand::Reconnect(self.config.clone()));
                    }
                });

                if let Some(error) = &self.config_error {
                    ui.colored_label(egui::Color32::RED, error);
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
