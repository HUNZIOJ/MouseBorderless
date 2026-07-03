use crate::{geometry::Rect, input_event::InputEvent};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAGIC: u32 = 0x4244_524c;
pub const PROTOCOL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 4 + 2 + 1 + 8 + 4;
pub const MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;
pub const MAX_FRAME_LEN: usize = HEADER_LEN + MAX_PAYLOAD_LEN;

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
    ClipboardOffer(crate::clipboard::ClipboardEnvelope),
    ClipboardData(crate::clipboard::ClipboardEnvelope),
    FileTransferOffer(crate::file_transfer::FileTransferManifest),
    FileTransferProgress {
        transfer_id: uuid::Uuid,
        bytes_done: u64,
        bytes_total: u64,
    },
    FileTransferComplete {
        transfer_id: uuid::Uuid,
        ok: bool,
    },
    DragDropStart(crate::drag_drop::DragDropSession),
    DragDropCancel {
        session_id: uuid::Uuid,
    },
    DragDropCommit {
        session_id: uuid::Uuid,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileTransferProgressPayload {
    transfer_id: uuid::Uuid,
    bytes_done: u64,
    bytes_total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileTransferCompletePayload {
    transfer_id: uuid::Uuid,
    ok: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DragDropSessionPayload {
    session_id: uuid::Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedFrame {
    pub sequence: u64,
    pub message: WireMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    FrameTooShort { actual: usize },
    InvalidMagic { actual: u32 },
    VersionMismatch { expected: u16, actual: u16 },
    PayloadLengthMismatch { declared: usize, actual: usize },
    PayloadTooLarge { max: usize, actual: usize },
    UnknownMessageType { ty: u8 },
    MessageTypeMismatch { expected: u8, actual: u8 },
    Encode(String),
    Decode(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooShort { actual } => {
                write!(f, "frame shorter than header: {actual} bytes")
            }
            Self::InvalidMagic { actual } => write!(f, "invalid frame magic: {actual:#010x}"),
            Self::VersionMismatch { expected, actual } => {
                write!(
                    f,
                    "protocol version mismatch: expected {expected}, got {actual}"
                )
            }
            Self::PayloadLengthMismatch { declared, actual } => write!(
                f,
                "frame payload length mismatch: declared {declared}, got {actual}"
            ),
            Self::PayloadTooLarge { max, actual } => {
                write!(f, "frame payload too large: max {max}, got {actual}")
            }
            Self::UnknownMessageType { ty } => write!(f, "unknown message type: {ty}"),
            Self::MessageTypeMismatch { expected, actual } => write!(
                f,
                "message type does not match payload: expected {expected}, got {actual}"
            ),
            Self::Encode(message) => write!(f, "message encode error: {message}"),
            Self::Decode(message) => write!(f, "message decode error: {message}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

pub fn encode_frame(sequence: u64, message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    let payload = encode_message(message)?;
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(ProtocolError::PayloadTooLarge {
            max: MAX_PAYLOAD_LEN,
            actual: payload.len(),
        });
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| ProtocolError::PayloadTooLarge {
        max: MAX_PAYLOAD_LEN,
        actual: payload.len(),
    })?;
    let mut buf = BytesMut::with_capacity(HEADER_LEN + payload.len());
    buf.put_u32(MAGIC);
    buf.put_u16(PROTOCOL_VERSION);
    buf.put_u8(message_type(message));
    buf.put_u64(sequence);
    buf.put_u32(payload_len);
    buf.extend_from_slice(&payload);
    Ok(buf.to_vec())
}

pub fn decode_frame(raw: &[u8]) -> Result<DecodedFrame, ProtocolError> {
    if raw.len() < HEADER_LEN {
        return Err(ProtocolError::FrameTooShort { actual: raw.len() });
    }

    let mut header = raw;
    let magic = header.get_u32();
    if magic != MAGIC {
        return Err(ProtocolError::InvalidMagic { actual: magic });
    }

    let version = header.get_u16();
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: version,
        });
    }

    let ty = header.get_u8();
    let sequence = header.get_u64();
    let payload_len = header.get_u32() as usize;
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(ProtocolError::PayloadTooLarge {
            max: MAX_PAYLOAD_LEN,
            actual: payload_len,
        });
    }
    if header.len() != payload_len {
        return Err(ProtocolError::PayloadLengthMismatch {
            declared: payload_len,
            actual: header.len(),
        });
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
        WireMessage::ClipboardOffer(_) => 6,
        WireMessage::ClipboardData(_) => 7,
        WireMessage::FileTransferOffer(_) => 8,
        WireMessage::FileTransferProgress { .. } => 9,
        WireMessage::FileTransferComplete { .. } => 10,
        WireMessage::DragDropStart(_) => 11,
        WireMessage::DragDropCancel { .. } => 12,
        WireMessage::DragDropCommit { .. } => 13,
    }
}

fn encode_message(message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    match message {
        WireMessage::Hello(hello) => encode_body(hello),
        WireMessage::Input(event) => encode_body(event),
        WireMessage::Heartbeat(heartbeat) => encode_body(heartbeat),
        WireMessage::ReleaseAll => encode_body(&()),
        WireMessage::Error(message) => encode_body(message),
        WireMessage::ClipboardOffer(envelope) => encode_body(envelope),
        WireMessage::ClipboardData(envelope) => encode_body(envelope),
        WireMessage::FileTransferOffer(manifest) => encode_body(manifest),
        WireMessage::FileTransferProgress {
            transfer_id,
            bytes_done,
            bytes_total,
        } => encode_body(&FileTransferProgressPayload {
            transfer_id: *transfer_id,
            bytes_done: *bytes_done,
            bytes_total: *bytes_total,
        }),
        WireMessage::FileTransferComplete { transfer_id, ok } => {
            encode_body(&FileTransferCompletePayload {
                transfer_id: *transfer_id,
                ok: *ok,
            })
        }
        WireMessage::DragDropStart(session) => encode_body(session),
        WireMessage::DragDropCancel { session_id } => encode_body(&DragDropSessionPayload {
            session_id: *session_id,
        }),
        WireMessage::DragDropCommit { session_id } => encode_body(&DragDropSessionPayload {
            session_id: *session_id,
        }),
    }
}

fn encode_body<T: Serialize>(body: &T) -> Result<Vec<u8>, ProtocolError> {
    bincode::serialize(body).map_err(|err| ProtocolError::Encode(err.to_string()))
}

fn decode_message(ty: u8, payload: &[u8]) -> Result<WireMessage, ProtocolError> {
    match ty {
        1 => decode_body::<Hello>(ty, payload).map(WireMessage::Hello),
        2 => decode_body::<InputEvent>(ty, payload).map(WireMessage::Input),
        3 => decode_body::<Heartbeat>(ty, payload).map(WireMessage::Heartbeat),
        4 => decode_body::<()>(ty, payload).map(|()| WireMessage::ReleaseAll),
        5 => decode_body::<String>(ty, payload).map(WireMessage::Error),
        6 => decode_body::<crate::clipboard::ClipboardEnvelope>(ty, payload)
            .map(WireMessage::ClipboardOffer),
        7 => decode_body::<crate::clipboard::ClipboardEnvelope>(ty, payload)
            .map(WireMessage::ClipboardData),
        8 => decode_body::<crate::file_transfer::FileTransferManifest>(ty, payload)
            .map(WireMessage::FileTransferOffer),
        9 => decode_body::<FileTransferProgressPayload>(ty, payload).map(|body| {
            WireMessage::FileTransferProgress {
                transfer_id: body.transfer_id,
                bytes_done: body.bytes_done,
                bytes_total: body.bytes_total,
            }
        }),
        10 => decode_body::<FileTransferCompletePayload>(ty, payload).map(|body| {
            WireMessage::FileTransferComplete {
                transfer_id: body.transfer_id,
                ok: body.ok,
            }
        }),
        11 => decode_body::<crate::drag_drop::DragDropSession>(ty, payload)
            .map(WireMessage::DragDropStart),
        12 => decode_body::<DragDropSessionPayload>(ty, payload).map(|body| {
            WireMessage::DragDropCancel {
                session_id: body.session_id,
            }
        }),
        13 => decode_body::<DragDropSessionPayload>(ty, payload).map(|body| {
            WireMessage::DragDropCommit {
                session_id: body.session_id,
            }
        }),
        _ => Err(ProtocolError::UnknownMessageType { ty }),
    }
}

fn decode_body<T: for<'de> Deserialize<'de> + Serialize>(
    expected_ty: u8,
    payload: &[u8],
) -> Result<T, ProtocolError> {
    let decoded = bincode::deserialize(payload)
        .map_err(|err| decode_error_or_type_mismatch(payload, expected_ty, err.to_string()))?;
    let encoded =
        bincode::serialize(&decoded).map_err(|err| ProtocolError::Encode(err.to_string()))?;
    if encoded != payload {
        return Err(decode_error_or_type_mismatch(
            payload,
            expected_ty,
            "payload contains trailing bytes".to_string(),
        ));
    }
    Ok(decoded)
}

fn decode_error_or_type_mismatch(
    payload: &[u8],
    expected_ty: u8,
    message: String,
) -> ProtocolError {
    if let Some(actual) = payload_message_type(payload, expected_ty) {
        ProtocolError::MessageTypeMismatch {
            expected: expected_ty,
            actual,
        }
    } else {
        ProtocolError::Decode(message)
    }
}

fn payload_message_type(payload: &[u8], expected_ty: u8) -> Option<u8> {
    for ty in 1..=12 {
        if ty == expected_ty {
            continue;
        }
        if payload_matches_type(ty, payload) {
            return Some(ty);
        }
    }
    None
}

fn payload_matches_type(ty: u8, payload: &[u8]) -> bool {
    match ty {
        1 => strict_payload_matches::<Hello>(payload),
        2 => strict_payload_matches::<InputEvent>(payload),
        3 => strict_payload_matches::<Heartbeat>(payload),
        4 => strict_payload_matches::<()>(payload),
        5 => strict_payload_matches::<String>(payload),
        6 => strict_payload_matches::<crate::clipboard::ClipboardEnvelope>(payload),
        7 => strict_payload_matches::<crate::clipboard::ClipboardEnvelope>(payload),
        8 => strict_payload_matches::<crate::file_transfer::FileTransferManifest>(payload),
        9 => strict_payload_matches::<FileTransferProgressPayload>(payload),
        10 => strict_payload_matches::<FileTransferCompletePayload>(payload),
        11 => strict_payload_matches::<crate::drag_drop::DragDropSession>(payload),
        12 => strict_payload_matches::<DragDropSessionPayload>(payload),
        13 => strict_payload_matches::<DragDropSessionPayload>(payload),
        _ => false,
    }
}

fn strict_payload_matches<T: for<'de> Deserialize<'de> + Serialize>(payload: &[u8]) -> bool {
    bincode::deserialize::<T>(payload)
        .and_then(|decoded| bincode::serialize(&decoded).map(|encoded| encoded == payload))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use bytes::BufMut;

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

    #[test]
    fn oversize_payload_length_is_rejected() {
        let raw = raw_frame(1, 3, (MAX_PAYLOAD_LEN + 1) as u32, &[]);

        assert_eq!(
            decode_frame(&raw),
            Err(ProtocolError::PayloadTooLarge {
                max: MAX_PAYLOAD_LEN,
                actual: MAX_PAYLOAD_LEN + 1,
            })
        );
    }

    #[test]
    fn oversize_encode_payload_is_rejected() {
        let msg = WireMessage::Error("x".repeat(MAX_PAYLOAD_LEN + 1));

        assert_eq!(
            encode_frame(1, &msg),
            Err(ProtocolError::PayloadTooLarge {
                max: MAX_PAYLOAD_LEN,
                actual: MAX_PAYLOAD_LEN + 9,
            })
        );
    }

    #[test]
    fn message_type_mismatch_is_rejected() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop: Rect::new(0, 0, 1920, 1080),
        };
        let payload = bincode::serialize(&hello).unwrap();
        let raw = raw_frame(1, 3, payload.len() as u32, &payload);

        assert_eq!(
            decode_frame(&raw),
            Err(ProtocolError::MessageTypeMismatch {
                expected: 3,
                actual: 1,
            })
        );
    }

    #[test]
    fn hello_payload_does_not_depend_on_wire_message_enum_tag() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop: Rect::new(0, 0, 1920, 1080),
        };
        let payload = bincode::serialize(&hello).unwrap();
        let raw = raw_frame(9, 1, payload.len() as u32, &payload);

        assert_eq!(
            decode_frame(&raw),
            Ok(DecodedFrame {
                sequence: 9,
                message: WireMessage::Hello(hello),
            })
        );
    }

    #[test]
    fn unsupported_message_type_is_rejected() {
        let raw = raw_frame(1, 99, 0, &[]);

        assert_eq!(
            decode_frame(&raw),
            Err(ProtocolError::UnknownMessageType { ty: 99 })
        );
    }

    #[test]
    fn clipboard_offer_round_trips_with_stable_message_type() {
        let envelope = crate::clipboard::ClipboardEnvelope {
            change_id: crate::clipboard::ClipboardChangeId::new(uuid::Uuid::new_v4(), 42),
            payload: crate::clipboard::ClipboardPayload::UnicodeText("shared text".into()),
        };
        let msg = WireMessage::ClipboardOffer(envelope);

        assert_eq!(message_type(&msg), 6);

        let encoded = encode_frame(12, &msg).unwrap();
        let decoded = decode_frame(&encoded).unwrap();

        assert_eq!(decoded.sequence, 12);
        assert_eq!(decoded.message, msg);
    }

    #[test]
    fn new_message_type_ids_round_trip() {
        let transfer_id = uuid::Uuid::new_v4();
        let session_id = uuid::Uuid::new_v4();
        let envelope = crate::clipboard::ClipboardEnvelope {
            change_id: crate::clipboard::ClipboardChangeId::new(uuid::Uuid::new_v4(), 1),
            payload: crate::clipboard::ClipboardPayload::UnicodeText("text".into()),
        };
        let manifest = crate::file_transfer::FileTransferManifest {
            transfer_id,
            root_name: "root".into(),
            files: vec![],
            total_bytes: 0,
        };
        let drag_session = crate::drag_drop::DragDropSession {
            session_id,
            transfer_id,
            state: crate::drag_drop::DragDropState::LocalDragDetected,
        };

        let cases = [
            (WireMessage::ClipboardOffer(envelope.clone()), 6),
            (WireMessage::ClipboardData(envelope), 7),
            (WireMessage::FileTransferOffer(manifest), 8),
            (
                WireMessage::FileTransferProgress {
                    transfer_id,
                    bytes_done: 1,
                    bytes_total: 2,
                },
                9,
            ),
            (
                WireMessage::FileTransferComplete {
                    transfer_id,
                    ok: true,
                },
                10,
            ),
            (WireMessage::DragDropStart(drag_session), 11),
            (WireMessage::DragDropCancel { session_id }, 12),
            (WireMessage::DragDropCommit { session_id }, 13),
        ];

        for (message, expected_ty) in cases {
            assert_eq!(message_type(&message), expected_ty);

            let encoded = encode_frame(expected_ty as u64, &message).unwrap();
            let decoded = decode_frame(&encoded).unwrap();

            assert_eq!(decoded.sequence, expected_ty as u64);
            assert_eq!(decoded.message, message);
        }
    }

    fn raw_frame(sequence: u64, ty: u8, payload_len: u32, payload: &[u8]) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(HEADER_LEN + payload.len());
        buf.put_u32(MAGIC);
        buf.put_u16(PROTOCOL_VERSION);
        buf.put_u8(ty);
        buf.put_u64(sequence);
        buf.put_u32(payload_len);
        buf.extend_from_slice(payload);
        buf.to_vec()
    }
}
