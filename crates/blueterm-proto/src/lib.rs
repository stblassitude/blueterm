//! Wire protocol shared by `blueterm` (client) and `blueterm-agent` (server).
//!
//! The data path is the plain Nordic UART Service (NUS): raw bytes are written
//! by the client to [`NUS_RX`] and notified by the agent on [`NUS_TX`]. A
//! blueterm-specific [`CONTROL`] characteristic in the same service carries
//! [`ControlMessage`]s (terminal type, window size, session end). Clients that
//! only know NUS simply ignore it.

use std::fmt;

use uuid::Uuid;

/// Nordic UART Service.
pub const NUS_SERVICE: Uuid = Uuid::from_u128(0x6e400001_b5a3_f393_e0a9_e50e24dcca9e);
/// NUS RX characteristic: client writes, agent receives.
pub const NUS_RX: Uuid = Uuid::from_u128(0x6e400002_b5a3_f393_e0a9_e50e24dcca9e);
/// NUS TX characteristic: agent notifies, client receives.
pub const NUS_TX: Uuid = Uuid::from_u128(0x6e400003_b5a3_f393_e0a9_e50e24dcca9e);
/// blueterm control characteristic (write + notify).
pub const CONTROL: Uuid = Uuid::from_u128(0x37b7340b_aa46_4885_b0b3_2a07e496dc05);

/// Version of the control protocol spoken by this build.
pub const PROTOCOL_VERSION: u8 = 1;

/// Flag in [`ControlMessage::HelloAck`]: the agent only accepts writes over an
/// encrypted link from a device paired with MITM protection, so the `Hello` it
/// is replying to arrived that way. Agents before 1.0 never set it. The client
/// cannot verify it: anyone could send it.
pub const ACK_SECURE: u8 = 0x01;

/// Smallest possible ATT payload (ATT_MTU 23 minus 3 bytes of header).
pub const MIN_PAYLOAD: usize = 20;

/// Longest accepted `TERM` value.
pub const MAX_TERM_LEN: usize = 64;

/// Default terminal type when the client does not send one.
pub const DEFAULT_TERM: &str = "vt100";

/// Default window size (rows, cols) when the client does not send one.
pub const DEFAULT_SIZE: (u16, u16) = (24, 80);

const T_HELLO: u8 = 0x01;
const T_RESIZE: u8 = 0x02;
const T_HELLO_ACK: u8 = 0x81;
const T_SESSION_ENDED: u8 = 0x82;
const T_ERROR: u8 = 0x83;

/// A message on the control characteristic.
///
/// Each message occupies exactly one GATT write or notification: one type byte
/// followed by a type-specific payload. Integers are little-endian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    /// Client → agent, first message of a session.
    Hello { version: u8, rows: u16, cols: u16, term: String },
    /// Client → agent, terminal window changed.
    Resize { rows: u16, cols: u16 },
    /// Agent → client, reply to `Hello`. `flags` is a combination of `ACK_*`.
    HelloAck { version: u8, flags: u8 },
    /// Agent → client, the login session has ended.
    SessionEnded { status: i32 },
    /// Agent → client, human-readable error.
    Error(String),
}

/// A control message could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Empty,
    Truncated { kind: u8 },
    InvalidTerm,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Empty => write!(f, "empty control message"),
            DecodeError::Truncated { kind } => write!(f, "truncated control message type {kind:#04x}"),
            DecodeError::InvalidTerm => write!(f, "invalid TERM value in Hello"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Returns true if `term` is acceptable as a `TERM` value: non-empty, at most
/// [`MAX_TERM_LEN`] bytes of `[A-Za-z0-9._+-]`.
pub fn is_valid_term(term: &str) -> bool {
    !term.is_empty()
        && term.len() <= MAX_TERM_LEN
        && term.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

impl ControlMessage {
    /// Builds a `Hello` for this protocol version. An invalid `term` is
    /// replaced by [`DEFAULT_TERM`].
    pub fn hello(rows: u16, cols: u16, term: &str) -> Self {
        let term = if is_valid_term(term) { term } else { DEFAULT_TERM };
        ControlMessage::Hello { version: PROTOCOL_VERSION, rows, cols, term: term.to_owned() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        match self {
            ControlMessage::Hello { version, rows, cols, term } => {
                out.push(T_HELLO);
                out.push(*version);
                out.extend_from_slice(&rows.to_le_bytes());
                out.extend_from_slice(&cols.to_le_bytes());
                let term = &term.as_bytes()[..term.len().min(MAX_TERM_LEN)];
                out.push(term.len() as u8);
                out.extend_from_slice(term);
            }
            ControlMessage::Resize { rows, cols } => {
                out.push(T_RESIZE);
                out.extend_from_slice(&rows.to_le_bytes());
                out.extend_from_slice(&cols.to_le_bytes());
            }
            ControlMessage::HelloAck { version, flags } => {
                out.extend_from_slice(&[T_HELLO_ACK, *version, *flags]);
            }
            ControlMessage::SessionEnded { status } => {
                out.push(T_SESSION_ENDED);
                out.extend_from_slice(&status.to_le_bytes());
            }
            ControlMessage::Error(msg) => {
                out.push(T_ERROR);
                out.extend_from_slice(msg.as_bytes());
            }
        }
        out
    }

    /// Decodes one control message. Returns `Ok(None)` for unknown message
    /// types, which must be ignored for forward compatibility. Trailing bytes
    /// after a known message are ignored for the same reason.
    pub fn decode(buf: &[u8]) -> Result<Option<Self>, DecodeError> {
        let (&kind, p) = buf.split_first().ok_or(DecodeError::Empty)?;
        let need = |n: usize| if p.len() >= n { Ok(()) } else { Err(DecodeError::Truncated { kind }) };
        let u16_at = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
        let msg = match kind {
            T_HELLO => {
                need(6)?;
                let term_len = p[5] as usize;
                need(6 + term_len)?;
                let term = std::str::from_utf8(&p[6..6 + term_len]).map_err(|_| DecodeError::InvalidTerm)?;
                if !is_valid_term(term) {
                    return Err(DecodeError::InvalidTerm);
                }
                ControlMessage::Hello { version: p[0], rows: u16_at(1), cols: u16_at(3), term: term.to_owned() }
            }
            T_RESIZE => {
                need(4)?;
                ControlMessage::Resize { rows: u16_at(0), cols: u16_at(2) }
            }
            T_HELLO_ACK => {
                need(2)?;
                ControlMessage::HelloAck { version: p[0], flags: p[1] }
            }
            T_SESSION_ENDED => {
                need(4)?;
                ControlMessage::SessionEnded { status: i32::from_le_bytes([p[0], p[1], p[2], p[3]]) }
            }
            T_ERROR => ControlMessage::Error(String::from_utf8_lossy(p).into_owned()),
            _ => return Ok(None),
        };
        Ok(Some(msg))
    }
}

/// A unit of traffic between session logic and a BLE transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Raw terminal bytes (NUS RX/TX). May be longer than one ATT payload;
    /// transports split it as needed.
    Data(Vec<u8>),
    /// A control message.
    Control(ControlMessage),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: ControlMessage) {
        let enc = m.encode();
        assert_eq!(ControlMessage::decode(&enc), Ok(Some(m)));
    }

    #[test]
    fn roundtrips() {
        roundtrip(ControlMessage::hello(50, 132, "xterm-256color"));
        roundtrip(ControlMessage::Resize { rows: 1, cols: 65535 });
        roundtrip(ControlMessage::HelloAck { version: 1, flags: 0 });
        roundtrip(ControlMessage::HelloAck { version: 1, flags: ACK_SECURE });
        roundtrip(ControlMessage::SessionEnded { status: -1 });
        roundtrip(ControlMessage::SessionEnded { status: 130 });
        roundtrip(ControlMessage::Error("boom".into()));
    }

    #[test]
    fn hello_layout() {
        let enc = ControlMessage::hello(24, 80, "vt100").encode();
        assert_eq!(enc, [0x01, 1, 24, 0, 80, 0, 5, b'v', b't', b'1', b'0', b'0']);
    }

    #[test]
    fn hello_size_is_bounded() {
        // The largest Hello must fit one write at macOS' typical negotiated
        // ATT MTU of 185 (182 bytes of payload); short ones fit even MTU 23.
        let long = "x".repeat(MAX_TERM_LEN);
        assert_eq!(ControlMessage::hello(24, 80, &long).encode().len(), 7 + MAX_TERM_LEN);
        const { assert!(7 + MAX_TERM_LEN <= 182) };
        assert!(ControlMessage::hello(24, 80, "xterm").encode().len() <= MIN_PAYLOAD);
    }

    #[test]
    fn unknown_type_ignored() {
        assert_eq!(ControlMessage::decode(&[0x7f, 1, 2, 3]), Ok(None));
    }

    #[test]
    fn trailing_bytes_ignored() {
        assert_eq!(
            ControlMessage::decode(&[0x02, 10, 0, 20, 0, 0xff]),
            Ok(Some(ControlMessage::Resize { rows: 10, cols: 20 }))
        );
    }

    #[test]
    fn malformed() {
        assert_eq!(ControlMessage::decode(&[]), Err(DecodeError::Empty));
        assert_eq!(ControlMessage::decode(&[0x02, 1, 0, 2]), Err(DecodeError::Truncated { kind: 0x02 }));
        assert_eq!(
            ControlMessage::decode(&[0x01, 1, 24, 0, 80, 0, 9, b'x']),
            Err(DecodeError::Truncated { kind: 0x01 })
        );
        assert_eq!(ControlMessage::decode(&[0x82, 1]), Err(DecodeError::Truncated { kind: 0x82 }));
        // TERM with a space / shell metacharacter
        assert_eq!(
            ControlMessage::decode(&[0x01, 1, 24, 0, 80, 0, 3, b'a', b' ', b'b']),
            Err(DecodeError::InvalidTerm)
        );
        assert_eq!(ControlMessage::decode(&[0x01, 1, 24, 0, 80, 0, 0]), Err(DecodeError::InvalidTerm));
    }

    #[test]
    fn invalid_term_replaced() {
        assert_eq!(
            ControlMessage::hello(1, 1, "evil;rm -rf"),
            ControlMessage::Hello { version: PROTOCOL_VERSION, rows: 1, cols: 1, term: DEFAULT_TERM.into() }
        );
    }
}
