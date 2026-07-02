use std::collections::VecDeque;

use borderless_core::config::TransportMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunState {
    #[default]
    Stopped,
    Waiting,
    Connecting,
    Connected,
    LocalControl,
    RemoteControl,
    Reconnecting,
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct AppStatus {
    pub run_state: RunState,
    pub transport_mode: Option<TransportMode>,
    pub last_error: Option<String>,
    pub recent_rtt_ms: Option<u64>,
    pub average_rtt_ms: Option<u64>,
    pub stale_pointer_packets: u64,
    pub latest_pointer_sequence: Option<u64>,
    pub events: VecDeque<String>,
}

impl AppStatus {
    pub fn push_log(&mut self, message: impl Into<String>) {
        if self.events.len() == 100 {
            self.events.pop_front();
        }
        self.events.push_back(message.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_keeps_recent_entries() {
        let mut status = AppStatus::default();
        for i in 0..150 {
            status.push_log(format!("event {i}"));
        }
        assert_eq!(status.events.len(), 100);
        assert_eq!(status.events.front().unwrap(), "event 50");
    }

    #[test]
    fn event_log_accepts_str_messages() {
        let mut status = AppStatus::default();
        status.push_log("ready");

        assert_eq!(status.events.front().unwrap(), "ready");
    }
}
