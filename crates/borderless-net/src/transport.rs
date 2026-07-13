use borderless_core::protocol::WireMessage;
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Waiting,
    Connecting(String),
    Connected { peer: String },
    Disconnected(String),
    Message(WireMessage),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionCommand {
    Send(WireMessage),
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpConnectionSettings {
    pub host: String,
    pub port: u16,
}

impl TcpConnectionSettings {
    pub fn peer_addr(&self) -> String {
        addr_string(&self.host, self.port)
    }

    pub fn socket_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.peer_addr().parse()?)
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
    use std::net::{IpAddr, Ipv6Addr, SocketAddr};

    #[test]
    fn tcp_settings_format_ipv4_and_ipv6_addresses() {
        let ipv4 = TcpConnectionSettings {
            host: "127.0.0.1".to_string(),
            port: 24800,
        };
        let ipv6 = TcpConnectionSettings {
            host: "::1".to_string(),
            port: 24800,
        };

        assert_eq!(ipv4.peer_addr(), "127.0.0.1:24800");
        assert_eq!(ipv6.peer_addr(), "[::1]:24800");
        assert_eq!(
            ipv6.socket_addr().unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 24800)
        );
    }
}
