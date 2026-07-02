use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::OsString,
    fmt, fs,
    net::IpAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Controller,
    Agent,
}

impl Default for Role {
    fn default() -> Self {
        Self::Controller
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePosition {
    Left,
    Right,
    Top,
    Bottom,
}

impl Default for RemotePosition {
    fn default() -> Self {
        Self::Right
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportMode {
    Tcp,
    Kcp,
}

impl Default for TransportMode {
    fn default() -> Self {
        Self::Tcp
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerConfig {
    pub agent_host: String,
    pub agent_port: u16,
    pub transport_mode: TransportMode,
    pub pointer_port: u16,
    pub remote_position: RemotePosition,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            agent_host: "192.168.1.2".to_string(),
            agent_port: 24800,
            transport_mode: TransportMode::Tcp,
            pointer_port: 24801,
            remote_position: RemotePosition::Right,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub listen_host: String,
    pub listen_port: u16,
    pub transport_mode: TransportMode,
    pub pointer_port: u16,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            listen_host: "0.0.0.0".to_string(),
            listen_port: 24800,
            transport_mode: TransportMode::Tcp,
            pointer_port: 24801,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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

impl Default for SharingConfig {
    fn default() -> Self {
        Self {
            clipboard_text: true,
            clipboard_html: true,
            clipboard_images: true,
            file_copy_paste: true,
            real_file_drag_drop: true,
            max_clipboard_bytes: 32 * 1024 * 1024,
            max_file_transfer_bytes: 20 * 1024 * 1024 * 1024,
            bulk_transfer_port: 24802,
            incoming_cache_dir: "%LOCALAPPDATA%\\Borderless\\Incoming".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub role: Role,
    pub edge_trigger_px: i32,
    pub debug_logging: bool,
    pub controller: ControllerConfig,
    pub agent: AgentConfig,
    pub sharing: SharingConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    Io { message: String },
    TomlDeserialize { message: String },
    TomlSerialize { message: String },
    Validation { message: String },
    MissingEnvironment { variable: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message } => write!(f, "I/O error: {message}"),
            Self::TomlDeserialize { message } => write!(f, "TOML deserialize error: {message}"),
            Self::TomlSerialize { message } => write!(f, "TOML serialize error: {message}"),
            Self::Validation { message } => write!(f, "validation error: {message}"),
            Self::MissingEnvironment { variable } => {
                write!(f, "missing environment variable: {variable}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            role: Role::Controller,
            edge_trigger_px: 2,
            debug_logging: false,
            controller: ControllerConfig::default(),
            agent: AgentConfig::default(),
            sharing: SharingConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.edge_trigger_px < 1 || self.edge_trigger_px > 32 {
            return Err(ConfigError::Validation {
                message: "edge_trigger_px must be between 1 and 32".to_string(),
            });
        }

        if self.controller.agent_port == 0 {
            return Err(ConfigError::Validation {
                message: "controller.agent_port must be greater than 0".to_string(),
            });
        }

        if self.controller.pointer_port == 0 {
            return Err(ConfigError::Validation {
                message: "controller.pointer_port must be greater than 0".to_string(),
            });
        }

        if self.agent.listen_port == 0 {
            return Err(ConfigError::Validation {
                message: "agent.listen_port must be greater than 0".to_string(),
            });
        }

        if self.agent.pointer_port == 0 {
            return Err(ConfigError::Validation {
                message: "agent.pointer_port must be greater than 0".to_string(),
            });
        }

        if self.sharing.bulk_transfer_port == 0 {
            return Err(ConfigError::Validation {
                message: "sharing.bulk_transfer_port must be greater than 0".to_string(),
            });
        }

        if self.sharing.max_clipboard_bytes == 0 {
            return Err(ConfigError::Validation {
                message: "sharing.max_clipboard_bytes must be greater than 0".to_string(),
            });
        }

        if self.sharing.max_file_transfer_bytes == 0 {
            return Err(ConfigError::Validation {
                message: "sharing.max_file_transfer_bytes must be greater than 0".to_string(),
            });
        }

        if self.sharing.incoming_cache_dir.trim().is_empty() {
            return Err(ConfigError::Validation {
                message: "sharing.incoming_cache_dir must not be empty".to_string(),
            });
        }

        if self.controller.agent_host.trim().is_empty() {
            return Err(ConfigError::Validation {
                message: "controller.agent_host must not be empty".to_string(),
            });
        }

        if self.agent.listen_host.parse::<IpAddr>().is_err() {
            return Err(ConfigError::Validation {
                message: "agent.listen_host must be an IP address".to_string(),
            });
        }

        Ok(())
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|err| ConfigError::Io {
            message: err.to_string(),
        })?;
        let config: Self = toml::from_str(&raw).map_err(|err| ConfigError::TomlDeserialize {
            message: err.to_string(),
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let raw = toml::to_string_pretty(self).map_err(|err| ConfigError::TomlSerialize {
            message: err.to_string(),
        })?;
        fs::write(path, raw).map_err(|err| ConfigError::Io {
            message: err.to_string(),
        })
    }

    pub fn resolved_incoming_cache_dir(&self) -> Result<PathBuf, ConfigError> {
        self.resolve_incoming_cache_dir_with(|name| env::var_os(name))
    }

    fn resolve_incoming_cache_dir_with<F>(&self, lookup: F) -> Result<PathBuf, ConfigError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        const LOCAL_APP_DATA: &str = "%LOCALAPPDATA%";

        let raw = self.sharing.incoming_cache_dir.trim();
        if let Some(suffix) = raw.strip_prefix(LOCAL_APP_DATA) {
            let base = lookup("LOCALAPPDATA").ok_or_else(|| ConfigError::MissingEnvironment {
                variable: "LOCALAPPDATA".to_string(),
            })?;
            let suffix = suffix.trim_start_matches(['\\', '/']);
            let mut resolved = PathBuf::from(base);
            if !suffix.is_empty() {
                resolved.push(suffix);
            }
            Ok(resolved)
        } else {
            Ok(PathBuf::from(raw))
        }
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

    #[test]
    fn partial_toml_uses_defaults_for_missing_sections() {
        let decoded: AppConfig = toml::from_str("role = \"agent\"").unwrap();

        assert_eq!(decoded.role, Role::Agent);
        assert_eq!(decoded.edge_trigger_px, 2);
        assert!(!decoded.debug_logging);
        assert_eq!(decoded.controller.remote_position, RemotePosition::Right);
        assert_eq!(decoded.controller.transport_mode, TransportMode::Tcp);
        assert_eq!(decoded.agent.listen_host, "0.0.0.0");
        assert_eq!(decoded.agent.pointer_port, 24801);
        assert!(decoded.sharing.clipboard_text);
        assert_eq!(decoded.sharing.bulk_transfer_port, 24802);
    }

    #[test]
    fn incoming_cache_dir_resolves_local_app_data_placeholder() {
        let config = AppConfig::default();

        let resolved = config.resolve_incoming_cache_dir_with(|name| {
            (name == "LOCALAPPDATA")
                .then(|| std::ffi::OsString::from("C:\\Users\\Tester\\AppData\\Local"))
        });

        assert_eq!(
            resolved.unwrap(),
            Path::new("C:\\Users\\Tester\\AppData\\Local")
                .join("Borderless")
                .join("Incoming")
        );
    }

    #[test]
    fn load_from_path_reports_validation_errors() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "borderless-invalid-config-{}.toml",
            std::process::id()
        ));

        fs::write(&path, "role = \"controller\"\nedge_trigger_px = 0\n").unwrap();

        let err = AppConfig::load_from_path(&path).unwrap_err();
        fs::remove_file(&path).unwrap();

        match err {
            ConfigError::Validation { message } => {
                assert!(message.contains("edge_trigger_px"));
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }
}
