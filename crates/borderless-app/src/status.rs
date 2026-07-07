use std::collections::VecDeque;

use borderless_core::config::TransportMode;
use uuid::Uuid;
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
    pub clipboard_enabled: bool,
    pub last_clipboard_format: Option<String>,
    pub last_clipboard_bytes: Option<u64>,
    pub clipboard_ignored_reason: Option<String>,
    pub transfer_active: bool,
    pub transfer_id: Option<Uuid>,
    pub transfer_bytes_done: u64,
    pub transfer_bytes_total: u64,
    pub transfer_current_file: Option<String>,
    pub mouse_diagnostics: Option<String>,
    pub events: VecDeque<String>,
}

impl AppStatus {
    pub fn reset_runtime_fields(&mut self) {
        self.transport_mode = None;
        self.last_error = None;
        self.recent_rtt_ms = None;
        self.average_rtt_ms = None;
        self.stale_pointer_packets = 0;
        self.latest_pointer_sequence = None;
        self.clipboard_enabled = false;
        self.last_clipboard_format = None;
        self.last_clipboard_bytes = None;
        self.clipboard_ignored_reason = None;
        self.transfer_active = false;
        self.transfer_id = None;
        self.transfer_bytes_done = 0;
        self.transfer_bytes_total = 0;
        self.transfer_current_file = None;
        self.mouse_diagnostics = None;
    }

    pub fn record_rtt(&mut self, rtt_ms: u64) {
        self.recent_rtt_ms = Some(rtt_ms);
        self.average_rtt_ms = Some(match self.average_rtt_ms {
            Some(average) => (average.saturating_mul(3).saturating_add(rtt_ms)) / 4,
            None => rtt_ms,
        });
    }

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

    #[test]
    fn reset_runtime_fields_clears_live_metrics() {
        let mut status = AppStatus {
            transport_mode: Some(TransportMode::Kcp),
            last_error: Some("boom".to_string()),
            recent_rtt_ms: Some(10),
            average_rtt_ms: Some(20),
            stale_pointer_packets: 3,
            latest_pointer_sequence: Some(99),
            clipboard_enabled: true,
            last_clipboard_format: Some("text".to_string()),
            last_clipboard_bytes: Some(12),
            clipboard_ignored_reason: Some("too large".to_string()),
            transfer_active: true,
            transfer_id: Some(Uuid::nil()),
            transfer_bytes_done: 1,
            transfer_bytes_total: 2,
            transfer_current_file: Some("a.txt".to_string()),
            mouse_diagnostics: Some("raw=1".to_string()),
            ..AppStatus::default()
        };

        status.reset_runtime_fields();

        assert_eq!(status.transport_mode, None);
        assert_eq!(status.last_error, None);
        assert_eq!(status.recent_rtt_ms, None);
        assert_eq!(status.average_rtt_ms, None);
        assert_eq!(status.stale_pointer_packets, 0);
        assert_eq!(status.latest_pointer_sequence, None);
        assert!(!status.clipboard_enabled);
        assert_eq!(status.last_clipboard_format, None);
        assert_eq!(status.last_clipboard_bytes, None);
        assert_eq!(status.clipboard_ignored_reason, None);
        assert!(!status.transfer_active);
        assert_eq!(status.transfer_id, None);
        assert_eq!(status.transfer_bytes_done, 0);
        assert_eq!(status.transfer_bytes_total, 0);
        assert_eq!(status.transfer_current_file, None);
        assert_eq!(status.mouse_diagnostics, None);
    }

    #[test]
    fn record_rtt_tracks_recent_and_weighted_average() {
        let mut status = AppStatus::default();

        status.record_rtt(100);
        status.record_rtt(200);

        assert_eq!(status.recent_rtt_ms, Some(200));
        assert_eq!(status.average_rtt_ms, Some(125));
    }
}
