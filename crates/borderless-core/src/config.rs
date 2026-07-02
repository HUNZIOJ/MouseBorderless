use serde::{Deserialize, Serialize};
use std::{fmt, fs, net::IpAddr, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Controller,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePosition {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportMode {
    Tcp,
    Kcp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerConfig {
    pub agent_host: String,
    pub agent_port: u16,
    pub transport_mode: TransportMode,
    pub pointer_port: u16,
    pub remote_position: RemotePosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub listen_host: String,
    pub listen_port: u16,
    pub transport_mode: TransportMode,
    pub pointer_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharingConfig {
    pub clipboard_text: bool,
    pub clipboard_html: bool,
    pub clipboard_images: bool,
    pub file_copy_paste: bool,
    pub real_file_drag_drop: bool,
    pub max_clipboard_bytes: u64,
    pub max_file_transfer_bytes: u64,
    pub bulk_transfer_port: u16,
    pub incoming_cache_dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub role: Role,
    pub edge_trigger_px: i32,
    pub debug_logging: bool,
    pub controller: ControllerConfig,
    pub agent: AgentConfig,
    pub sharing: SharingConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            role: Role::Controller,
            edge_trigger_px: 2,
            debug_logging: false,
            controller: ControllerConfig {
                agent_host: "192.168.1.2".to_string(),
                agent_port: 24800,
                transport_mode: TransportMode::Tcp,
                pointer_port: 24801,
                remote_position: RemotePosition::Right,
            },
            agent: AgentConfig {
                listen_host: "0.0.0.0".to_string(),
                listen_port: 24800,
                transport_mode: TransportMode::Tcp,
                pointer_port: 24801,
            },
            sharing: SharingConfig {
                clipboard_text: true,
                clipboard_html: true,
                clipboard_images: true,
                file_copy_paste: true,
                real_file_drag_drop: true,
                max_clipboard_bytes: 32 * 1024 * 1024,
                max_file_transfer_bytes: 20 * 1024 * 1024 * 1024,
                bulk_transfer_port: 24802,
                incoming_cache_dir: "%LOCALAPPDATA%\\Borderless\\Incoming".to_string(),
            },
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.edge_trigger_px < 1 || self.edge_trigger_px > 32 {
            return Err(ConfigError::new("edge_trigger_px must be between 1 and 32"));
        }

        if self.controller.agent_port == 0 {
            return Err(ConfigError::new(
                "controller.agent_port must be greater than 0",
            ));
        }

        if self.controller.pointer_port == 0 {
            return Err(ConfigError::new(
                "controller.pointer_port must be greater than 0",
            ));
        }

        if self.agent.listen_port == 0 {
            return Err(ConfigError::new(
                "agent.listen_port must be greater than 0",
            ));
        }

        if self.agent.pointer_port == 0 {
            return Err(ConfigError::new(
                "agent.pointer_port must be greater than 0",
            ));
        }

        if self.sharing.bulk_transfer_port == 0 {
            return Err(ConfigError::new(
                "sharing.bulk_transfer_port must be greater than 0",
            ));
        }

        if self.sharing.max_clipboard_bytes == 0 {
            return Err(ConfigError::new(
                "sharing.max_clipboard_bytes must be greater than 0",
            ));
        }

        if self.sharing.max_file_transfer_bytes == 0 {
            return Err(ConfigError::new(
                "sharing.max_file_transfer_bytes must be greater than 0",
            ));
        }

        if self.sharing.incoming_cache_dir.trim().is_empty() {
            return Err(ConfigError::new(
                "sharing.incoming_cache_dir must not be empty",
            ));
        }

        if self.controller.agent_host.trim().is_empty() {
            return Err(ConfigError::new(
                "controller.agent_host must not be empty",
            ));
        }

        if self.agent.listen_host.parse::<IpAddr>().is_err() {
            return Err(ConfigError::new("agent.listen_host must be an IP address"));
        }

        Ok(())
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|err| ConfigError::new(err.to_string()))?;
        let config: Self = toml::from_str(&raw).map_err(|err| ConfigError::new(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let raw = toml::to_string_pretty(self).map_err(|err| ConfigError::new(err.to_string()))?;
        fs::write(path, raw).map_err(|err| ConfigError::new(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_controller_config() {
        let config = AppConfig::default();
        assert_eq!(config.role, Role::Controller);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_edge_width_is_rejected() {
        let mut config = AppConfig::default();
        config.edge_trigger_px = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("edge_trigger_px"));
    }

    #[test]
    fn toml_round_trip_preserves_role_and_position() {
        let config = AppConfig::default();
        let encoded = toml::to_string_pretty(&config).unwrap();
        let decoded: AppConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.role, Role::Controller);
        assert_eq!(decoded.controller.remote_position, RemotePosition::Right);
        assert_eq!(decoded.controller.transport_mode, TransportMode::Tcp);
        assert_eq!(decoded.controller.pointer_port, 24801);
        assert!(decoded.sharing.clipboard_text);
        assert!(decoded.sharing.file_copy_paste);
        assert!(decoded.sharing.real_file_drag_drop);
    }
}
