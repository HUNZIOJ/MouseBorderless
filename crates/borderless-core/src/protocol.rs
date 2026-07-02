use crate::{geometry::Rect, input_event::InputEvent};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAGIC: u32 = 0x4244_524c;
pub const PROTOCOL_VERSION: u16 = 1;
const HEADER_LEN: usize = 4 + 2 + 1 + 8 + 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub desktop: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub sent_millis: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireMessage {
    Hello(Hello),
    Input(InputEvent),
    Heartbeat(Heartbeat),
    ReleaseAll,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedFrame {
    pub sequence: u64,
    pub message: WireMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(String);

impl ProtocolError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

pub fn encode_frame(sequence: u64, message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    let payload = encode_message(message)?;
    let mut buf = BytesMut::with_capacity(HEADER_LEN + payload.len());
    buf.put_u32(MAGIC);
    buf.put_u16(PROTOCOL_VERSION);
    buf.put_u8(message_type(message));
    buf.put_u64(sequence);
    buf.put_u32(payload.len() as u32);
    buf.extend_from_slice(&payload);
    Ok(buf.to_vec())
}

pub fn decode_frame(raw: &[u8]) -> Result<DecodedFrame, ProtocolError> {
    if raw.len() < HEADER_LEN {
        return Err(ProtocolError::new("frame shorter than header"));
    }

    let mut header = raw;
    let magic = header.get_u32();
    if magic != MAGIC {
        return Err(ProtocolError::new("invalid frame magic"));
    }

    let version = header.get_u16();
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::new("protocol version mismatch"));
    }

    let ty = header.get_u8();
    let sequence = header.get_u64();
    let payload_len = header.get_u32() as usize;
    if header.len() != payload_len {
        return Err(ProtocolError::new("frame payload length mismatch"));
    }

    Ok(DecodedFrame {
        sequence,
        message: decode_message(ty, header)?,
    })
}

fn message_type(message: &WireMessage) -> u8 {
    match message {
        WireMessage::Hello(_) => 1,
        WireMessage::Input(_) => 2,
        WireMessage::Heartbeat(_) => 3,
        WireMessage::ReleaseAll => 4,
        WireMessage::Error(_) => 5,
    }
}

fn encode_message(message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    bincode::serialize(message).map_err(|err| ProtocolError::new(err.to_string()))
}

fn decode_message(ty: u8, payload: &[u8]) -> Result<WireMessage, ProtocolError> {
    let decoded: WireMessage =
        bincode::deserialize(payload).map_err(|err| ProtocolError::new(err.to_string()))?;
    if message_type(&decoded) != ty {
        return Err(ProtocolError::new("message type does not match payload"));
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    #[test]
    fn encode_decode_hello_round_trips() {
        let msg = WireMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop: Rect::new(0, 0, 1920, 1080),
        });
        let encoded = encode_frame(7, &msg).unwrap();
        let decoded = decode_frame(&encoded).unwrap();
        assert_eq!(decoded.sequence, 7);
        assert_eq!(decoded.message, msg);
    }

    #[test]
    fn invalid_magic_is_rejected() {
        let mut encoded =
            encode_frame(1, &WireMessage::Heartbeat(Heartbeat { sent_millis: 1 })).unwrap();
        encoded[0] = 0;
        assert!(decode_frame(&encoded).is_err());
    }
}
