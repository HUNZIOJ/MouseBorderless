use borderless_core::{config::TransportMode, protocol::WireMessage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Waiting,
    Connecting(String),
    Connected { peer: String, mode: TransportMode },
    Disconnected(String),
    Message(WireMessage),
    LatestPointer { x: i32, y: i32, sequence: u64 },
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
        format!("{}:{}", self.host, self.reliable_port)
    }

    pub fn pointer_addr(&self) -> String {
        format!("{}:{}", self.host, self.pointer_port)
    }
}
