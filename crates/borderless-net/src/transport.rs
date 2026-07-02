use borderless_core::{config::TransportMode, protocol::WireMessage};
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Waiting,
    Connecting(String),
    Connected { peer: String, mode: TransportMode },
    Disconnected(String),
    Message(WireMessage),
    LatestPointer { x: i32, y: i32, sequence: u64 },
    StalePointerPackets { count: u64 },
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionCommand {
    SendReliable(WireMessage),
    SendLatestPointer { x: i32, y: i32 },
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportSettings {
    pub mode: TransportMode,
    pub host: String,
    pub reliable_port: u16,
    pub pointer_port: u16,
}

impl TransportSettings {
    pub fn peer_addr(&self) -> String {
        addr_string(&self.host, self.reliable_port)
    }

    pub fn reliable_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.peer_addr().parse()?)
    }

    pub fn pointer_addr_string(&self) -> String {
        addr_string(&self.host, self.pointer_port)
    }

    pub fn pointer_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.pointer_addr_string().parse()?)
    }
}

fn addr_string(host: &str, port: u16) -> String {
    match host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{host}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use borderless_core::config::TransportMode;
    use std::net::{IpAddr, Ipv6Addr, SocketAddr};

    #[test]
    fn reliable_addr_parses_ipv4() {
        let settings = TransportSettings {
            mode: TransportMode::Tcp,
            host: "127.0.0.1".to_string(),
            reliable_port: 24800,
            pointer_port: 24801,
        };

        assert_eq!(
            settings.reliable_addr().unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 24800))
        );
    }

    #[test]
    fn address_helpers_bracket_ipv6() {
        let settings = TransportSettings {
            mode: TransportMode::Kcp,
            host: "::1".to_string(),
            reliable_port: 24800,
            pointer_port: 24801,
        };

        assert_eq!(settings.peer_addr(), "[::1]:24800");
        assert_eq!(settings.pointer_addr_string(), "[::1]:24801");
        assert_eq!(
            settings.pointer_addr().unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 24801)
        );
    }
}
