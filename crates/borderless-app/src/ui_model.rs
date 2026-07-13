use borderless_core::config::{AppConfig, RemotePosition, Role};

use crate::status::{AppStatus, RunState};

#[derive(Clone, Debug, PartialEq)]
pub struct UiSnapshot {
    pub connection_label: String,
    pub control_label: String,
    pub latency_label: String,
    pub connected: bool,
    pub running: bool,
    pub transfer_active: bool,
    pub transfer_progress: f32,
    pub transfer_file: String,
    pub transfer_detail: String,
    pub transfer_destination: String,
    pub last_error: String,
}

impl UiSnapshot {
    pub fn from_status(status: &AppStatus) -> Self {
        let connection_label = match status.run_state {
            RunState::Stopped => "已停止",
            RunState::Waiting => "等待连接",
            RunState::Connecting => "正在连接",
            RunState::Connected | RunState::LocalControl | RunState::RemoteControl => "已连接",
            RunState::Reconnecting => "正在重连",
            RunState::Error => "连接错误",
        }
        .to_string();
        let control_label = match status.run_state {
            RunState::RemoteControl => "远程电脑",
            _ => "本机",
        }
        .to_string();
        let transfer_progress = if status.transfer_bytes_total == 0 {
            0.0
        } else {
            status.transfer_bytes_done as f32 / status.transfer_bytes_total as f32
        }
        .clamp(0.0, 1.0);

        Self {
            connection_label,
            control_label,
            latency_label: status
                .recent_rtt_ms
                .map(|value| format!("{value} ms"))
                .unwrap_or_else(|| "-".to_string()),
            connected: matches!(
                status.run_state,
                RunState::Connected | RunState::LocalControl | RunState::RemoteControl
            ),
            running: status.run_state != RunState::Stopped,
            transfer_active: status.transfer_active,
            transfer_progress,
            transfer_file: status.transfer_current_file.clone().unwrap_or_default(),
            transfer_detail: format!(
                "{} / {} bytes",
                status.transfer_bytes_done, status.transfer_bytes_total
            ),
            transfer_destination: status.drag_drop_destination.clone().unwrap_or_default(),
            last_error: status.last_error.clone().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDraft {
    pub role: Role,
    pub target_host: String,
    pub target_port: u16,
    pub listen_host: String,
    pub listen_port: u16,
    pub remote_position: RemotePosition,
    pub clipboard_text: bool,
    pub clipboard_html: bool,
    pub clipboard_images: bool,
    pub file_copy_paste: bool,
    pub file_drag_drop: bool,
}

impl ConfigDraft {
    pub fn apply_to(&self, config: &mut AppConfig) {
        config.role = self.role.clone();
        config.controller.agent_host = self.target_host.clone();
        config.controller.agent_port = self.target_port;
        config.controller.remote_position = self.remote_position.clone();
        config.agent.listen_host = self.listen_host.clone();
        config.agent.listen_port = self.listen_port;
        config.sharing.clipboard_text = self.clipboard_text;
        config.sharing.clipboard_html = self.clipboard_html;
        config.sharing.clipboard_images = self.clipboard_images;
        config.sharing.file_copy_paste = self.file_copy_paste;
        config.sharing.file_drag_drop = self.file_drag_drop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{AppStatus, RunState};
    use borderless_core::config::{AppConfig, RemotePosition, Role};

    #[test]
    fn connected_remote_status_projects_to_chinese_labels() {
        let status = AppStatus {
            run_state: RunState::RemoteControl,
            recent_rtt_ms: Some(3),
            transfer_active: true,
            transfer_bytes_done: 248,
            transfer_bytes_total: 400,
            transfer_current_file: Some("design.pdf".to_string()),
            ..AppStatus::default()
        };

        let view = UiSnapshot::from_status(&status);
        assert_eq!(view.connection_label, "已连接");
        assert_eq!(view.control_label, "远程电脑");
        assert_eq!(view.latency_label, "3 ms");
        assert_eq!(view.transfer_progress, 0.62);
        assert_eq!(view.transfer_file, "design.pdf");
    }

    #[test]
    fn config_draft_updates_only_visible_connection_fields() {
        let mut config = AppConfig::default();
        let draft = ConfigDraft {
            role: Role::Agent,
            target_host: "192.168.1.20".to_string(),
            target_port: 24880,
            listen_host: "0.0.0.0".to_string(),
            listen_port: 24900,
            remote_position: RemotePosition::Left,
            clipboard_text: false,
            clipboard_html: true,
            clipboard_images: true,
            file_copy_paste: true,
            file_drag_drop: true,
        };

        draft.apply_to(&mut config);
        assert_eq!(config.role, Role::Agent);
        assert_eq!(config.agent.listen_host, "0.0.0.0");
        assert_eq!(config.agent.listen_port, 24900);
        assert_eq!(config.controller.agent_host, "192.168.1.20");
        assert_eq!(config.controller.agent_port, 24880);
        assert_eq!(config.controller.remote_position, RemotePosition::Left);
        assert!(!config.sharing.clipboard_text);
    }
}
