use anyhow::{anyhow, ensure};
use bytes::{Buf, BufMut, BytesMut};
use tokio::net::UdpSocket;

const POINTER_MAGIC: u32 = 0x4250_5452;
const POINTER_LEN: usize = 4 + 8 + 4 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerPacket {
    pub sequence: u64,
    pub x: i32,
    pub y: i32,
}

impl PointerPacket {
    pub fn encode(self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(POINTER_LEN);
        buf.put_u32(POINTER_MAGIC);
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
            sequence: raw.get_u64(),
            x: raw.get_i32(),
            y: raw.get_i32(),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct LatestPointerState {
    latest_sequence: u64,
}

impl LatestPointerState {
    pub fn accept(&mut self, packet: PointerPacket) -> Option<(i32, i32)> {
        if packet.sequence <= self.latest_sequence {
            return None;
        }

        self.latest_sequence = packet.sequence;
        Some((packet.x, packet.y))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_pointer_packets_are_ignored() {
        let mut state = LatestPointerState::default();

        assert_eq!(
            state.accept(PointerPacket {
                sequence: 10,
                x: 100,
                y: 200,
            }),
            Some((100, 200))
        );
        assert_eq!(
            state.accept(PointerPacket {
                sequence: 9,
                x: 300,
                y: 400,
            }),
            None
        );
        assert_eq!(
            state.accept(PointerPacket {
                sequence: 11,
                x: 500,
                y: 600,
            }),
            Some((500, 600))
        );
    }

    #[test]
    fn pointer_packet_round_trips() {
        let packet = PointerPacket {
            sequence: 42,
            x: -10,
            y: 900,
        };
        let encoded = packet.encode();
        assert_eq!(PointerPacket::decode(&encoded).unwrap(), packet);
    }
}
