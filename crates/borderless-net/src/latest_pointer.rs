use anyhow::{anyhow, ensure};
use bytes::{Buf, BufMut, BytesMut};
use std::{collections::BTreeSet, net::SocketAddr};
use tokio::net::UdpSocket;
use uuid::Uuid;

const POINTER_MAGIC: u32 = 0x4250_5452;
const POINTER_LEN: usize = 4 + 16 + 8 + 4 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerPacket {
    pub session_id: u128,
    pub sequence: u64,
    pub x: i32,
    pub y: i32,
}

impl PointerPacket {
    pub fn encode(self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(POINTER_LEN);
        buf.put_u32(POINTER_MAGIC);
        buf.put_u128(self.session_id);
        buf.put_u64(self.sequence);
        buf.put_i32(self.x);
        buf.put_i32(self.y);
        buf.to_vec()
    }

    pub fn decode(raw: &[u8]) -> anyhow::Result<Self> {
        ensure!(
            raw.len() == POINTER_LEN,
            "pointer packet length mismatch: expected {}, got {}",
            POINTER_LEN,
            raw.len()
        );

        let mut raw = raw;
        let magic = raw.get_u32();
        ensure!(
            magic == POINTER_MAGIC,
            "invalid pointer packet magic: {magic:#010x}"
        );

        Ok(Self {
            session_id: raw.get_u128(),
            sequence: raw.get_u64(),
            x: raw.get_i32(),
            y: raw.get_i32(),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct LatestPointerState {
    current_session_id: Option<u128>,
    retired_session_ids: BTreeSet<u128>,
    latest_sequence: u64,
    stale_pointer_packets: u64,
}

impl LatestPointerState {
    pub fn accept(&mut self, packet: PointerPacket) -> Option<(i32, i32)> {
        if self.retired_session_ids.contains(&packet.session_id) {
            self.record_stale_packet();
            return None;
        }

        if self.current_session_id != Some(packet.session_id) {
            if let Some(current_session_id) = self.current_session_id {
                self.retired_session_ids.insert(current_session_id);
            }
            self.current_session_id = Some(packet.session_id);
            self.latest_sequence = 0;
        }

        if packet.sequence <= self.latest_sequence {
            self.record_stale_packet();
            return None;
        }

        self.latest_sequence = packet.sequence;
        Some((packet.x, packet.y))
    }

    pub fn stale_pointer_packets(&self) -> u64 {
        self.stale_pointer_packets
    }

    fn record_stale_packet(&mut self) {
        self.stale_pointer_packets = self.stale_pointer_packets.saturating_add(1);
    }

    fn retire_current_session(&mut self) {
        if let Some(current_session_id) = self.current_session_id.take() {
            self.retired_session_ids.insert(current_session_id);
        }
        self.latest_sequence = 0;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LatestPointerSession {
    inbound: LatestPointerState,
    outbound_session_id: u128,
    next_sequence: u64,
}

impl Default for LatestPointerSession {
    fn default() -> Self {
        Self {
            inbound: LatestPointerState::default(),
            outbound_session_id: Uuid::new_v4().as_u128(),
            next_sequence: 1,
        }
    }
}

impl LatestPointerSession {
    pub(crate) fn begin_reliable_session(&mut self) {
        self.outbound_session_id = Uuid::new_v4().as_u128();
        self.next_sequence = 1;
        self.inbound.retire_current_session();
    }

    pub(crate) fn next_packet(&mut self, x: i32, y: i32) -> PointerPacket {
        let packet = PointerPacket {
            session_id: self.outbound_session_id,
            sequence: self.next_sequence,
            x,
            y,
        };
        self.next_sequence += 1;
        packet
    }

    pub(crate) fn accept(&mut self, packet: PointerPacket) -> Option<(i32, i32)> {
        self.inbound.accept(packet)
    }

    pub(crate) fn stale_pointer_packets(&self) -> u64 {
        self.inbound.stale_pointer_packets()
    }
}

pub async fn send_pointer(
    socket: &UdpSocket,
    target: &str,
    packet: PointerPacket,
) -> anyhow::Result<()> {
    let encoded = packet.encode();
    let sent = socket.send_to(&encoded, target).await?;
    if sent == encoded.len() {
        Ok(())
    } else {
        Err(anyhow!(
            "partial pointer packet send: sent {} of {} bytes",
            sent,
            encoded.len()
        ))
    }
}

pub(crate) fn source_matches_peer(source: SocketAddr, peer: SocketAddr) -> bool {
    source == peer
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::time::{timeout, Duration};

    #[test]
    fn stale_pointer_packets_are_ignored() {
        let mut state = LatestPointerState::default();

        assert_eq!(
            state.accept(PointerPacket {
                session_id: 7,
                sequence: 10,
                x: 100,
                y: 200,
            }),
            Some((100, 200))
        );
        assert_eq!(state.stale_pointer_packets(), 0);
        assert_eq!(
            state.accept(PointerPacket {
                session_id: 7,
                sequence: 9,
                x: 300,
                y: 400,
            }),
            None
        );
        assert_eq!(state.stale_pointer_packets(), 1);
        assert_eq!(
            state.accept(PointerPacket {
                session_id: 7,
                sequence: 11,
                x: 500,
                y: 600,
            }),
            Some((500, 600))
        );
        assert_eq!(state.stale_pointer_packets(), 1);
    }

    #[test]
    fn new_pointer_session_accepts_sequence_one_after_previous_high_sequence() {
        let mut state = LatestPointerState::default();

        assert_eq!(
            state.accept(PointerPacket {
                session_id: 7,
                sequence: 900,
                x: 100,
                y: 200,
            }),
            Some((100, 200))
        );
        assert_eq!(
            state.accept(PointerPacket {
                session_id: 8,
                sequence: 1,
                x: 300,
                y: 400,
            }),
            Some((300, 400))
        );
    }

    #[test]
    fn retired_pointer_session_packets_are_ignored_after_new_session() {
        let mut state = LatestPointerState::default();

        assert_eq!(
            state.accept(PointerPacket {
                session_id: 7,
                sequence: 900,
                x: 100,
                y: 200,
            }),
            Some((100, 200))
        );
        assert_eq!(
            state.accept(PointerPacket {
                session_id: 8,
                sequence: 1,
                x: 300,
                y: 400,
            }),
            Some((300, 400))
        );
        assert_eq!(
            state.accept(PointerPacket {
                session_id: 7,
                sequence: 901,
                x: 500,
                y: 600,
            }),
            None
        );
        assert_eq!(state.stale_pointer_packets(), 1);
    }

    #[test]
    fn pointer_packet_round_trips() {
        let packet = PointerPacket {
            session_id: 123_456_789,
            sequence: 42,
            x: -10,
            y: 900,
        };
        let encoded = packet.encode();
        assert_eq!(PointerPacket::decode(&encoded).unwrap(), packet);
    }

    #[tokio::test]
    async fn udp_loopback_pointer_packets_accept_only_fresh_sequences() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = receiver.local_addr().unwrap().to_string();
        let peer = sender.local_addr().unwrap();

        for packet in [
            PointerPacket {
                session_id: 7,
                sequence: 10,
                x: 100,
                y: 200,
            },
            PointerPacket {
                session_id: 7,
                sequence: 9,
                x: 300,
                y: 400,
            },
            PointerPacket {
                session_id: 7,
                sequence: 11,
                x: 500,
                y: 600,
            },
        ] {
            send_pointer(&sender, &target, packet).await.unwrap();
        }

        let mut state = LatestPointerState::default();
        let mut buf = [0u8; 64];
        let mut accepted = Vec::new();

        for _ in 0..3 {
            let (len, source) = timeout(Duration::from_secs(1), receiver.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(source_matches_peer(source, peer));
            let packet = PointerPacket::decode(&buf[..len]).unwrap();
            if let Some((x, y)) = state.accept(packet) {
                accepted.push((packet.sequence, x, y));
            }
        }

        assert_eq!(accepted, [(10, 100, 200), (11, 500, 600)]);
        assert_eq!(state.stale_pointer_packets(), 1);
    }

    #[test]
    fn source_filter_accepts_only_exact_peer_socket_addr() {
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 24800);
        let same_ip_different_port =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 24801);
        let different_ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 24801);

        assert!(source_matches_peer(peer, peer));
        assert!(!source_matches_peer(same_ip_different_port, peer));
        assert!(!source_matches_peer(different_ip, peer));
    }

    #[test]
    fn pointer_session_sequence_continues_above_persistent_receive_state_after_reconnect() {
        let mut session = LatestPointerSession::default();

        for sequence in 1..=50 {
            let packet = session.next_packet(sequence, sequence * 10);
            assert_eq!(packet.sequence, sequence as u64);
        }

        assert_eq!(
            session.accept(PointerPacket {
                session_id: 1,
                sequence: 50,
                x: 100,
                y: 200,
            }),
            Some((100, 200))
        );

        let post_reconnect = session.next_packet(51, 510);
        assert_eq!(post_reconnect.sequence, 51);
        assert_eq!(session.accept(post_reconnect), Some((51, 510)));
    }

    #[test]
    fn reliable_reconnect_retires_current_inbound_session_before_new_udp_arrives() {
        let mut session = LatestPointerSession::default();
        let old_outbound = session.next_packet(1, 2);
        assert_eq!(old_outbound.sequence, 1);

        assert_eq!(
            session.accept(PointerPacket {
                session_id: 7,
                sequence: 50,
                x: 100,
                y: 200,
            }),
            Some((100, 200))
        );

        session.begin_reliable_session();

        let new_outbound = session.next_packet(3, 4);
        assert_ne!(new_outbound.session_id, old_outbound.session_id);
        assert_eq!(new_outbound.sequence, 1);

        assert_eq!(
            session.accept(PointerPacket {
                session_id: 7,
                sequence: 51,
                x: 300,
                y: 400,
            }),
            None
        );
        assert_eq!(session.stale_pointer_packets(), 1);
        assert_eq!(
            session.accept(PointerPacket {
                session_id: 8,
                sequence: 1,
                x: 500,
                y: 600,
            }),
            Some((500, 600))
        );
    }
}
