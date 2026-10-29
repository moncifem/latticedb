//! Sync protocol v1: the message codec and the version negotiation rule.
//!
//! The wire format is specified in `formats/sync-protocol-v1.md`. This module is only the codec:
//! bytes in, messages out, and the reverse. It does no I/O, reads no clock and holds no session
//! state, so it can be tested and fuzzed without a socket. A frame is a one-byte type, a four-byte
//! big-endian payload length and the payload. Decoding is strict and bounded: an unknown type, a
//! length above the limit, a field outside its bounds, a code the protocol does not define, invalid
//! UTF-8 and bytes left over after the last field are all errors, and incomplete input is reported
//! as such so a caller can read more. Every value has exactly one encoding, so
//! `encode(decode(bytes)) == bytes` for every frame the decoder accepts. Nothing here panics on any
//! input.
//!
//! Cursors are opaque bytes of bounded length. The codec never looks inside one; their meaning
//! belongs to the change feed.

use std::fmt;

/// The lowest protocol version this implementation speaks.
pub const VERSION_MIN: u16 = 1;
/// The highest protocol version this implementation speaks.
pub const VERSION_MAX: u16 = 1;
/// Bytes before the payload: a type byte and a four-byte payload length.
pub const HEADER_LEN: usize = 5;
/// The largest payload the codec accepts or produces, in bytes.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// The smallest payload limit a `Welcome` may announce.
pub const MIN_NEGOTIATED_PAYLOAD: u32 = 1024;
/// The longest cursor, in bytes.
pub const MAX_CURSOR_LEN: usize = 64;
/// The longest subscription scope, in bytes of UTF-8.
pub const MAX_SCOPE_LEN: usize = 512;
/// The longest agent or server name, in bytes of UTF-8.
pub const MAX_NAME_LEN: usize = 64;
/// The longest error message, in bytes of UTF-8.
pub const MAX_ERROR_MESSAGE_LEN: usize = 256;
/// The most events in one `Events` frame.
pub const MAX_EVENTS_PER_FRAME: usize = 256;

const TYPE_HELLO: u8 = 0x01;
const TYPE_WELCOME: u8 = 0x02;
const TYPE_SUBSCRIBE: u8 = 0x03;
const TYPE_EVENTS: u8 = 0x04;
const TYPE_ACK: u8 = 0x05;
const TYPE_ERROR: u8 = 0x06;
const TYPE_PING: u8 = 0x07;
const TYPE_PONG: u8 = 0x08;

/// Why a frame could not be decoded or encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// The payload length is above the limit in force.
    BadLength(u32),
    /// A frame type this protocol version does not define.
    UnknownType(u8),
    /// The payload ends before its fields do.
    Truncated,
    /// Bytes remain in the payload after the last field.
    Trailing,
    /// A text field is not valid UTF-8.
    NotUtf8,
    /// A value does not fit the format (an encoded frame above [`MAX_PAYLOAD`]).
    TooLarge,
    /// A field holds a value the protocol does not allow: out of bounds, a reserved value, a control
    /// character in text or an unknown code.
    InvalidField(&'static str),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::BadLength(n) => write!(f, "payload length {n} is above the limit"),
            SyncError::UnknownType(t) => write!(f, "unknown frame type 0x{t:02x}"),
            SyncError::Truncated => write!(f, "payload is truncated"),
            SyncError::Trailing => write!(f, "unexpected bytes after the last field"),
            SyncError::NotUtf8 => write!(f, "text is not valid UTF-8"),
            SyncError::TooLarge => write!(f, "frame does not fit the size limit"),
            SyncError::InvalidField(what) => write!(f, "invalid {what}"),
        }
    }
}

impl std::error::Error for SyncError {}

/// The code in an `Error` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The version ranges of the two sides do not overlap. The sender closes the connection.
    UnsupportedVersion,
    /// The peer sent a frame that does not decode, or one that is not allowed in the current state.
    Malformed,
    /// A frame names a subscription that does not exist.
    UnknownSubscription,
    /// The subscription scope is not one the server serves.
    InvalidScope,
    /// The cursor is not one the server can resume from.
    InvalidCursor,
    /// The connection already holds as many subscriptions as the server allows.
    TooManySubscriptions,
    /// The server failed for a reason of its own.
    Internal,
}

impl ErrorCode {
    /// The number on the wire.
    pub const fn code(self) -> u16 {
        match self {
            ErrorCode::UnsupportedVersion => 1,
            ErrorCode::Malformed => 2,
            ErrorCode::UnknownSubscription => 3,
            ErrorCode::InvalidScope => 4,
            ErrorCode::InvalidCursor => 5,
            ErrorCode::TooManySubscriptions => 6,
            ErrorCode::Internal => 7,
        }
    }

    /// The code for a number on the wire, if it is defined.
    pub const fn from_code(n: u16) -> Option<ErrorCode> {
        Some(match n {
            1 => ErrorCode::UnsupportedVersion,
            2 => ErrorCode::Malformed,
            3 => ErrorCode::UnknownSubscription,
            4 => ErrorCode::InvalidScope,
            5 => ErrorCode::InvalidCursor,
            6 => ErrorCode::TooManySubscriptions,
            7 => ErrorCode::Internal,
            _ => return None,
        })
    }
}

/// One change delivered to a subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The position after this event, `1..=`[`MAX_CURSOR_LEN`] opaque bytes. Subscribing with it
    /// resumes after this event.
    pub cursor: Vec<u8>,
    /// The event body. The codec does not interpret it; the change feed defines its encoding.
    pub data: Vec<u8>,
}

/// A sync protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Client to server, first frame: the versions the client speaks and who it is.
    Hello {
        /// Lowest version the client speaks, at least 1.
        min_version: u16,
        /// Highest version the client speaks, at least `min_version`.
        max_version: u16,
        /// Client name, `1..=`[`MAX_NAME_LEN`] bytes.
        agent: String,
    },
    /// Server to client: the chosen version and the largest payload the server will accept.
    Welcome {
        /// The negotiated version. Must be a version this implementation knows.
        version: u16,
        /// Largest payload the sender of this message will accept, at least
        /// [`MIN_NEGOTIATED_PAYLOAD`] and at most [`MAX_PAYLOAD`].
        max_payload: u32,
        /// Server name, `1..=`[`MAX_NAME_LEN`] bytes.
        server: String,
    },
    /// Client to server: start a subscription.
    Subscribe {
        /// Client-chosen subscription id, not zero.
        sub: u32,
        /// What to subscribe to, `1..=`[`MAX_SCOPE_LEN`] bytes of text. The server defines the syntax.
        scope: String,
        /// Resume after this cursor, or empty to start from the beginning.
        cursor: Vec<u8>,
    },
    /// Server to client: a batch of events for one subscription, in delivery order.
    Events {
        /// The subscription they belong to, not zero.
        sub: u32,
        /// `1..=`[`MAX_EVENTS_PER_FRAME`] events.
        events: Vec<Event>,
    },
    /// Client to server: everything up to and including this cursor has been processed.
    Ack {
        /// The subscription, not zero.
        sub: u32,
        /// The position acknowledged, `1..=`[`MAX_CURSOR_LEN`] bytes.
        cursor: Vec<u8>,
    },
    /// Either direction: something went wrong.
    Error {
        /// What kind of failure.
        code: ErrorCode,
        /// The subscription it concerns, or zero for the whole connection.
        sub: u32,
        /// Human-readable text, at most [`MAX_ERROR_MESSAGE_LEN`] bytes.
        message: String,
    },
    /// Either direction: liveness probe. The peer answers with a `Pong` carrying the same nonce.
    Ping {
        /// Echoed in the answer.
        nonce: u64,
    },
    /// Either direction: the answer to a `Ping`.
    Pong {
        /// The nonce of the `Ping` answered.
        nonce: u64,
    },
}

/// Pick the version for a connection: the highest one both sides speak.
///
/// The client offers the range `min..=max`; this implementation speaks
/// [`VERSION_MIN`]`..=`[`VERSION_MAX`]. A range that is empty or starts below 1 is
/// [`ErrorCode::Malformed`]; two ranges that do not overlap are
/// [`ErrorCode::UnsupportedVersion`], and the server answers with that error and closes. A client
/// newer than the server therefore negotiates down to the server's highest version, and a client
/// that only speaks versions the server does not know is refused.
pub fn negotiate(min: u16, max: u16) -> Result<u16, ErrorCode> {
    if min < VERSION_MIN || min > max {
        return Err(ErrorCode::Malformed);
    }
    if min > VERSION_MAX || max < VERSION_MIN {
        return Err(ErrorCode::UnsupportedVersion);
    }
    Ok(max.min(VERSION_MAX))
}

/// Append the frame for `msg` to `out`. On error `out` is left unchanged.
///
/// Refuses any message the decoder would refuse, so a message that encodes always decodes to
/// itself.
pub fn encode(msg: &Message, out: &mut Vec<u8>) -> Result<(), SyncError> {
    let mut payload = Vec::new();
    let ty = match msg {
        Message::Hello {
            min_version,
            max_version,
            agent,
        } => {
            check_versions(*min_version, *max_version)?;
            put_u16(&mut payload, *min_version);
            put_u16(&mut payload, *max_version);
            put_text(&mut payload, agent, 1, MAX_NAME_LEN, "agent")?;
            TYPE_HELLO
        }
        Message::Welcome {
            version,
            max_payload,
            server,
        } => {
            check_welcome(*version, *max_payload)?;
            put_u16(&mut payload, *version);
            payload.extend_from_slice(&max_payload.to_be_bytes());
            put_text(&mut payload, server, 1, MAX_NAME_LEN, "server")?;
            TYPE_WELCOME
        }
        Message::Subscribe { sub, scope, cursor } => {
            check_sub(*sub)?;
            payload.extend_from_slice(&sub.to_be_bytes());
            put_text(&mut payload, scope, 1, MAX_SCOPE_LEN, "scope")?;
            put_cursor(&mut payload, cursor, 0)?;
            TYPE_SUBSCRIBE
        }
        Message::Events { sub, events } => {
            check_sub(*sub)?;
            if events.is_empty() || events.len() > MAX_EVENTS_PER_FRAME {
                return Err(SyncError::InvalidField("event count"));
            }
            payload.extend_from_slice(&sub.to_be_bytes());
            put_u16(
                &mut payload,
                u16::try_from(events.len()).map_err(|_| SyncError::TooLarge)?,
            );
            for e in events {
                put_cursor(&mut payload, &e.cursor, 1)?;
                let n = u32::try_from(e.data.len()).map_err(|_| SyncError::TooLarge)?;
                let end = payload
                    .len()
                    .checked_add(4)
                    .and_then(|len| len.checked_add(e.data.len()))
                    .ok_or(SyncError::TooLarge)?;
                if end > MAX_PAYLOAD {
                    return Err(SyncError::TooLarge);
                }
                payload.extend_from_slice(&n.to_be_bytes());
                payload.extend_from_slice(&e.data);
            }
            TYPE_EVENTS
        }
        Message::Ack { sub, cursor } => {
            check_sub(*sub)?;
            payload.extend_from_slice(&sub.to_be_bytes());
            put_cursor(&mut payload, cursor, 1)?;
            TYPE_ACK
        }
        Message::Error { code, sub, message } => {
            put_u16(&mut payload, code.code());
            payload.extend_from_slice(&sub.to_be_bytes());
            put_text(&mut payload, message, 0, MAX_ERROR_MESSAGE_LEN, "message")?;
            TYPE_ERROR
        }
        Message::Ping { nonce } => {
            payload.extend_from_slice(&nonce.to_be_bytes());
            TYPE_PING
        }
        Message::Pong { nonce } => {
            payload.extend_from_slice(&nonce.to_be_bytes());
            TYPE_PONG
        }
    };
    if payload.len() > MAX_PAYLOAD {
        return Err(SyncError::TooLarge);
    }
    let len = u32::try_from(payload.len()).map_err(|_| SyncError::TooLarge)?;
    out.push(ty);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(())
}

/// Decode one frame from the front of `buf` with the default payload limit, [`MAX_PAYLOAD`].
///
/// Returns `Ok(None)` when `buf` holds only part of a frame, and otherwise the message and the
/// number of bytes it used.
pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, SyncError> {
    decode_with_limit(buf, MAX_PAYLOAD)
}

/// Like [`decode`], with a payload limit negotiated lower than [`MAX_PAYLOAD`] (a limit above it is
/// treated as [`MAX_PAYLOAD`]).
///
/// The type and the length are judged as soon as the five header bytes are present, so a hostile
/// peer cannot make the reader wait for a payload it will refuse.
pub fn decode_with_limit(buf: &[u8], limit: usize) -> Result<Option<(Message, usize)>, SyncError> {
    let Some((&ty, rest)) = buf.split_first() else {
        return Ok(None);
    };
    if !(TYPE_HELLO..=TYPE_PONG).contains(&ty) {
        return Err(SyncError::UnknownType(ty));
    }
    let Some(len_bytes) = rest.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(array(len_bytes)?);
    let len_usize = usize::try_from(len).map_err(|_| SyncError::BadLength(len))?;
    if len_usize > limit.min(MAX_PAYLOAD) {
        return Err(SyncError::BadLength(len));
    }
    let end = HEADER_LEN
        .checked_add(len_usize)
        .ok_or(SyncError::BadLength(len))?;
    let Some(payload) = buf.get(HEADER_LEN..end) else {
        return Ok(None);
    };
    Ok(Some((decode_payload(ty, payload)?, end)))
}

fn decode_payload(ty: u8, payload: &[u8]) -> Result<Message, SyncError> {
    let mut r = Reader { buf: payload };
    let msg = match ty {
        TYPE_HELLO => {
            let min_version = r.u16()?;
            let max_version = r.u16()?;
            check_versions(min_version, max_version)?;
            Message::Hello {
                min_version,
                max_version,
                agent: r.text(1, MAX_NAME_LEN, "agent")?,
            }
        }
        TYPE_WELCOME => {
            let version = r.u16()?;
            let max_payload = r.u32()?;
            check_welcome(version, max_payload)?;
            Message::Welcome {
                version,
                max_payload,
                server: r.text(1, MAX_NAME_LEN, "server")?,
            }
        }
        TYPE_SUBSCRIBE => {
            let sub = r.u32()?;
            check_sub(sub)?;
            Message::Subscribe {
                sub,
                scope: r.text(1, MAX_SCOPE_LEN, "scope")?,
                cursor: r.cursor(0)?,
            }
        }
        TYPE_EVENTS => {
            let sub = r.u32()?;
            check_sub(sub)?;
            let count = usize::from(r.u16()?);
            if count == 0 || count > MAX_EVENTS_PER_FRAME {
                return Err(SyncError::InvalidField("event count"));
            }
            let mut events = Vec::with_capacity(count);
            for _ in 0..count {
                let cursor = r.cursor(1)?;
                let n = usize::try_from(r.u32()?).map_err(|_| SyncError::Truncated)?;
                events.push(Event {
                    cursor,
                    data: r.take(n)?.to_vec(),
                });
            }
            Message::Events { sub, events }
        }
        TYPE_ACK => {
            let sub = r.u32()?;
            check_sub(sub)?;
            Message::Ack {
                sub,
                cursor: r.cursor(1)?,
            }
        }
        TYPE_ERROR => {
            let code =
                ErrorCode::from_code(r.u16()?).ok_or(SyncError::InvalidField("error code"))?;
            Message::Error {
                code,
                sub: r.u32()?,
                message: r.text(0, MAX_ERROR_MESSAGE_LEN, "message")?,
            }
        }
        TYPE_PING => Message::Ping { nonce: r.u64()? },
        TYPE_PONG => Message::Pong { nonce: r.u64()? },
        other => return Err(SyncError::UnknownType(other)),
    };
    if !r.buf.is_empty() {
        return Err(SyncError::Trailing);
    }
    Ok(msg)
}

fn check_versions(min: u16, max: u16) -> Result<(), SyncError> {
    if min < VERSION_MIN || min > max {
        return Err(SyncError::InvalidField("version range"));
    }
    Ok(())
}

// A Welcome names the one version this implementation speaks; an unknown version is refused
// rather than guessed at.
fn check_welcome(version: u16, max_payload: u32) -> Result<(), SyncError> {
    if !(VERSION_MIN..=VERSION_MAX).contains(&version) {
        return Err(SyncError::InvalidField("version"));
    }
    let limit = usize::try_from(max_payload).map_err(|_| SyncError::InvalidField("max payload"))?;
    if max_payload < MIN_NEGOTIATED_PAYLOAD || limit > MAX_PAYLOAD {
        return Err(SyncError::InvalidField("max payload"));
    }
    Ok(())
}

fn check_sub(sub: u32) -> Result<(), SyncError> {
    if sub == 0 {
        return Err(SyncError::InvalidField("subscription id"));
    }
    Ok(())
}

fn check_text(s: &str, min: usize, max: usize, what: &'static str) -> Result<(), SyncError> {
    if s.len() < min || s.len() > max || s.chars().any(char::is_control) {
        return Err(SyncError::InvalidField(what));
    }
    Ok(())
}

fn put_u16(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_text(
    out: &mut Vec<u8>,
    s: &str,
    min: usize,
    max: usize,
    what: &'static str,
) -> Result<(), SyncError> {
    check_text(s, min, max, what)?;
    put_u16(
        out,
        u16::try_from(s.len()).map_err(|_| SyncError::TooLarge)?,
    );
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn put_cursor(out: &mut Vec<u8>, c: &[u8], min: usize) -> Result<(), SyncError> {
    if c.len() < min || c.len() > MAX_CURSOR_LEN {
        return Err(SyncError::InvalidField("cursor"));
    }
    put_u16(
        out,
        u16::try_from(c.len()).map_err(|_| SyncError::TooLarge)?,
    );
    out.extend_from_slice(c);
    Ok(())
}

fn array<const N: usize>(b: &[u8]) -> Result<[u8; N], SyncError> {
    <[u8; N]>::try_from(b).map_err(|_| SyncError::Truncated)
}

/// A cursor over a payload that fails with [`SyncError::Truncated`] instead of reading past it.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SyncError> {
        let (head, tail) = self.buf.split_at_checked(n).ok_or(SyncError::Truncated)?;
        self.buf = tail;
        Ok(head)
    }

    fn u16(&mut self) -> Result<u16, SyncError> {
        Ok(u16::from_be_bytes(array(self.take(2)?)?))
    }

    fn u32(&mut self) -> Result<u32, SyncError> {
        Ok(u32::from_be_bytes(array(self.take(4)?)?))
    }

    fn u64(&mut self) -> Result<u64, SyncError> {
        Ok(u64::from_be_bytes(array(self.take(8)?)?))
    }

    fn text(&mut self, min: usize, max: usize, what: &'static str) -> Result<String, SyncError> {
        let n = usize::from(self.u16()?);
        if n < min || n > max {
            return Err(SyncError::InvalidField(what));
        }
        let s = std::str::from_utf8(self.take(n)?).map_err(|_| SyncError::NotUtf8)?;
        check_text(s, min, max, what)?;
        Ok(s.to_string())
    }

    fn cursor(&mut self, min: usize) -> Result<Vec<u8>, SyncError> {
        let n = usize::from(self.u16()?);
        if n < min || n > MAX_CURSOR_LEN {
            return Err(SyncError::InvalidField("cursor"));
        }
        Ok(self.take(n)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use proptest::prelude::*;

    fn enc(msg: &Message) -> Vec<u8> {
        let mut out = Vec::new();
        encode(msg, &mut out).unwrap();
        out
    }

    fn hello() -> Message {
        Message::Hello {
            min_version: 1,
            max_version: 1,
            agent: "sdk-rs".into(),
        }
    }

    /// Frames written by hand from the format document, one per message type and shape. The
    /// expected bytes are not produced by the encoder.
    fn vectors() -> Vec<(Message, Vec<u8>)> {
        let scope = "ws/main".to_string();
        let scope_bytes = [0x77, 0x73, 0x2f, 0x6d, 0x61, 0x69, 0x6e];
        let mut sub1 = vec![0x03, 0, 0, 0, 0x0f, 0, 0, 0, 0x01, 0, 0x07];
        sub1.extend_from_slice(&scope_bytes);
        sub1.extend_from_slice(&[0, 0]);
        let mut sub2 = vec![0x03, 0, 0, 0, 0x17, 0, 0, 0, 0x02, 0, 0x07];
        sub2.extend_from_slice(&scope_bytes);
        sub2.extend_from_slice(&[0, 0x08, 1, 2, 3, 4, 5, 6, 7, 8]);
        vec![
            (
                hello(),
                vec![
                    0x01, 0, 0, 0, 0x0c, 0, 0x01, 0, 0x01, 0, 0x06, 0x73, 0x64, 0x6b, 0x2d, 0x72,
                    0x73,
                ],
            ),
            (
                Message::Hello {
                    min_version: 1,
                    max_version: 3,
                    agent: "a".into(),
                },
                vec![0x01, 0, 0, 0, 0x07, 0, 0x01, 0, 0x03, 0, 0x01, 0x61],
            ),
            (
                Message::Welcome {
                    version: 1,
                    max_payload: 1_048_576,
                    server: "lattice".into(),
                },
                vec![
                    0x02, 0, 0, 0, 0x0f, 0, 0x01, 0x00, 0x10, 0x00, 0x00, 0, 0x07, 0x6c, 0x61,
                    0x74, 0x74, 0x69, 0x63, 0x65,
                ],
            ),
            (
                Message::Subscribe {
                    sub: 1,
                    scope: scope.clone(),
                    cursor: vec![],
                },
                sub1,
            ),
            (
                Message::Subscribe {
                    sub: 2,
                    scope,
                    cursor: vec![1, 2, 3, 4, 5, 6, 7, 8],
                },
                sub2,
            ),
            (
                Message::Events {
                    sub: 1,
                    events: vec![
                        Event {
                            cursor: vec![0, 1],
                            data: vec![0x61],
                        },
                        Event {
                            cursor: vec![0, 2],
                            data: vec![],
                        },
                    ],
                },
                vec![
                    0x04, 0, 0, 0, 0x17, 0, 0, 0, 0x01, 0, 0x02, 0, 0x02, 0, 0x01, 0, 0, 0, 0x01,
                    0x61, 0, 0x02, 0, 0x02, 0, 0, 0, 0,
                ],
            ),
            (
                Message::Ack {
                    sub: 1,
                    cursor: vec![0, 2],
                },
                vec![0x05, 0, 0, 0, 0x08, 0, 0, 0, 0x01, 0, 0x02, 0, 0x02],
            ),
            (
                Message::Error {
                    code: ErrorCode::UnsupportedVersion,
                    sub: 0,
                    message: "version".into(),
                },
                vec![
                    0x06, 0, 0, 0, 0x0f, 0, 0x01, 0, 0, 0, 0, 0, 0x07, 0x76, 0x65, 0x72, 0x73,
                    0x69, 0x6f, 0x6e,
                ],
            ),
            (
                Message::Error {
                    code: ErrorCode::UnknownSubscription,
                    sub: 9,
                    message: String::new(),
                },
                vec![0x06, 0, 0, 0, 0x08, 0, 0x03, 0, 0, 0, 0x09, 0, 0],
            ),
            (
                Message::Ping { nonce: 1 },
                vec![0x07, 0, 0, 0, 0x08, 0, 0, 0, 0, 0, 0, 0, 0x01],
            ),
            (
                Message::Pong {
                    nonce: 0x0102_0304_0506_0708,
                },
                vec![0x08, 0, 0, 0, 0x08, 1, 2, 3, 4, 5, 6, 7, 8],
            ),
        ]
    }

    #[test]
    fn byte_vectors_encode_and_decode() {
        for (msg, bytes) in vectors() {
            assert_eq!(enc(&msg), bytes, "encode {msg:?}");
            assert_eq!(
                decode(&bytes),
                Ok(Some((msg.clone(), bytes.len()))),
                "{msg:?}"
            );
        }
    }

    #[test]
    fn decode_uses_only_the_first_frame() {
        let mut two = enc(&Message::Ping { nonce: 1 });
        let first = two.len();
        two.extend(enc(&Message::Pong { nonce: 2 }));
        assert_eq!(decode(&two), Ok(Some((Message::Ping { nonce: 1 }, first))));
        assert_eq!(
            decode(&two[first..]),
            Ok(Some((Message::Pong { nonce: 2 }, first)))
        );
    }

    #[test]
    fn every_proper_prefix_is_incomplete() {
        for (_, bytes) in vectors() {
            for cut in 0..bytes.len() {
                assert_eq!(decode(&bytes[..cut]), Ok(None), "prefix {cut} of {bytes:?}");
            }
        }
    }

    fn refused(bytes: &[u8]) -> SyncError {
        decode(bytes).expect_err("must be refused")
    }

    #[test]
    fn header_refusals() {
        assert_eq!(refused(&[0x00, 0, 0, 0, 0]), SyncError::UnknownType(0));
        assert_eq!(refused(&[0x09, 0, 0, 0, 0]), SyncError::UnknownType(9));
        assert_eq!(refused(&[0xff]), SyncError::UnknownType(0xff));
        // A length one above the limit is refused on the header alone.
        assert_eq!(
            refused(&[0x07, 0x00, 0x10, 0x00, 0x01]),
            SyncError::BadLength(1_048_577)
        );
        assert_eq!(
            refused(&[0x07, 0xff, 0xff, 0xff, 0xff]),
            SyncError::BadLength(u32::MAX)
        );
        // The limit itself is accepted as a length; the payload is then simply not there yet.
        assert_eq!(decode(&[0x07, 0x00, 0x10, 0x00, 0x00]), Ok(None));
        // A lower negotiated limit applies.
        let ping = enc(&Message::Ping { nonce: 1 });
        assert_eq!(decode_with_limit(&ping, 7), Err(SyncError::BadLength(8)));
        assert!(decode_with_limit(&ping, 8).is_ok());
        // A limit above the maximum is clamped.
        assert_eq!(
            decode_with_limit(&[0x07, 0x00, 0x10, 0x00, 0x01], usize::MAX),
            Err(SyncError::BadLength(1_048_577))
        );
    }

    #[test]
    fn payload_refusals() {
        // Trailing byte after a Hello.
        assert_eq!(
            refused(&[
                0x01, 0, 0, 0, 0x0d, 0, 1, 0, 1, 0, 6, 0x73, 0x64, 0x6b, 0x2d, 0x72, 0x73, 0
            ]),
            SyncError::Trailing
        );
        // Ping payload one byte long and one byte short.
        assert_eq!(
            refused(&[0x07, 0, 0, 0, 0x09, 0, 0, 0, 0, 0, 0, 0, 1, 0]),
            SyncError::Trailing
        );
        assert_eq!(
            refused(&[0x07, 0, 0, 0, 0x07, 0, 0, 0, 0, 0, 0, 1]),
            SyncError::Truncated
        );
        // Text length runs past the payload.
        assert_eq!(
            refused(&[0x01, 0, 0, 0, 0x07, 0, 1, 0, 1, 0, 3, 0x61]),
            SyncError::Truncated
        );
        // Version range: min above max, min zero.
        assert_eq!(
            refused(&[0x01, 0, 0, 0, 0x07, 0, 2, 0, 1, 0, 1, 0x61]),
            SyncError::InvalidField("version range")
        );
        assert_eq!(
            refused(&[0x01, 0, 0, 0, 0x07, 0, 0, 0, 1, 0, 1, 0x61]),
            SyncError::InvalidField("version range")
        );
        // Empty agent, invalid UTF-8, control character, name above the limit.
        assert_eq!(
            refused(&[0x01, 0, 0, 0, 0x06, 0, 1, 0, 1, 0, 0]),
            SyncError::InvalidField("agent")
        );
        assert_eq!(
            refused(&[0x01, 0, 0, 0, 0x07, 0, 1, 0, 1, 0, 1, 0xff]),
            SyncError::NotUtf8
        );
        assert_eq!(
            refused(&[0x01, 0, 0, 0, 0x07, 0, 1, 0, 1, 0, 1, 0x0a]),
            SyncError::InvalidField("agent")
        );
        let mut long = vec![0x01, 0, 0, 0, 0x47, 0, 1, 0, 1, 0, 65];
        long.extend(std::iter::repeat_n(b'a', 65));
        assert_eq!(refused(&long), SyncError::InvalidField("agent"));
        // Welcome with a version nobody knows, and with payload limits outside the range.
        assert_eq!(
            refused(&[0x02, 0, 0, 0, 0x09, 0, 2, 0, 0x10, 0, 0, 0, 1, 0x78]),
            SyncError::InvalidField("version")
        );
        assert_eq!(
            refused(&[0x02, 0, 0, 0, 0x09, 0, 1, 0, 0, 0x03, 0xff, 0, 1, 0x78]),
            SyncError::InvalidField("max payload")
        );
        assert_eq!(
            refused(&[0x02, 0, 0, 0, 0x09, 0, 1, 0, 0x10, 0, 0x01, 0, 1, 0x78]),
            SyncError::InvalidField("max payload")
        );
        // Subscribe with id zero; cursor above the limit.
        assert_eq!(
            refused(&[0x03, 0, 0, 0, 0x09, 0, 0, 0, 0, 0, 1, 0x78, 0, 0]),
            SyncError::InvalidField("subscription id")
        );
        let mut cur = vec![0x03, 0, 0, 0, 0x4a, 0, 0, 0, 1, 0, 1, 0x78, 0, 65];
        cur.extend(std::iter::repeat_n(0u8, 65));
        assert_eq!(refused(&cur), SyncError::InvalidField("cursor"));
        // Ack with an empty cursor.
        assert_eq!(
            refused(&[0x05, 0, 0, 0, 0x06, 0, 0, 0, 1, 0, 0]),
            SyncError::InvalidField("cursor")
        );
        // Events: zero events, 257 events, and an event with an empty cursor.
        assert_eq!(
            refused(&[0x04, 0, 0, 0, 0x06, 0, 0, 0, 1, 0, 0]),
            SyncError::InvalidField("event count")
        );
        assert_eq!(
            refused(&[0x04, 0, 0, 0, 0x06, 0, 0, 0, 1, 0x01, 0x01]),
            SyncError::InvalidField("event count")
        );
        assert_eq!(
            refused(&[0x04, 0, 0, 0, 0x0c, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0]),
            SyncError::InvalidField("cursor")
        );
        // An event whose data length runs past the payload.
        assert_eq!(
            refused(&[0x04, 0, 0, 0, 0x0d, 0, 0, 0, 1, 0, 1, 0, 1, 7, 0, 0, 0, 5]),
            SyncError::Truncated
        );
        // Error codes 0 and 8 are not defined.
        assert_eq!(
            refused(&[0x06, 0, 0, 0, 0x08, 0, 0, 0, 0, 0, 0, 0, 0]),
            SyncError::InvalidField("error code")
        );
        assert_eq!(
            refused(&[0x06, 0, 0, 0, 0x08, 0, 8, 0, 0, 0, 0, 0, 0]),
            SyncError::InvalidField("error code")
        );
    }

    #[test]
    fn encoder_refuses_what_the_decoder_refuses() {
        let bad = [
            Message::Hello {
                min_version: 0,
                max_version: 1,
                agent: "a".into(),
            },
            Message::Hello {
                min_version: 2,
                max_version: 1,
                agent: "a".into(),
            },
            Message::Hello {
                min_version: 1,
                max_version: 1,
                agent: String::new(),
            },
            Message::Hello {
                min_version: 1,
                max_version: 1,
                agent: "a\nb".into(),
            },
            Message::Welcome {
                version: 2,
                max_payload: 4096,
                server: "s".into(),
            },
            Message::Welcome {
                version: 1,
                max_payload: 1023,
                server: "s".into(),
            },
            Message::Subscribe {
                sub: 0,
                scope: "s".into(),
                cursor: vec![],
            },
            Message::Subscribe {
                sub: 1,
                scope: "s".into(),
                cursor: vec![0; 65],
            },
            Message::Subscribe {
                sub: 1,
                scope: "x".repeat(513),
                cursor: vec![],
            },
            Message::Events {
                sub: 1,
                events: vec![],
            },
            Message::Events {
                sub: 1,
                events: vec![
                    Event {
                        cursor: vec![1],
                        data: vec![]
                    };
                    257
                ],
            },
            Message::Events {
                sub: 1,
                events: vec![Event {
                    cursor: vec![],
                    data: vec![],
                }],
            },
            Message::Events {
                sub: 1,
                events: vec![Event {
                    cursor: vec![1],
                    data: vec![0; MAX_PAYLOAD],
                }],
            },
            Message::Ack {
                sub: 1,
                cursor: vec![],
            },
            Message::Error {
                code: ErrorCode::Internal,
                sub: 0,
                message: "x".repeat(257),
            },
        ];
        for m in bad {
            let mut out = vec![0xaa];
            assert!(encode(&m, &mut out).is_err(), "{m:?}");
            assert_eq!(out, [0xaa], "output untouched on error");
        }
    }

    #[test]
    fn largest_valid_frames_round_trip() {
        let m = Message::Events {
            sub: u32::MAX,
            events: vec![Event {
                cursor: vec![0xff; MAX_CURSOR_LEN],
                data: vec![7; MAX_PAYLOAD - 4 - 2 - 2 - MAX_CURSOR_LEN - 4],
            }],
        };
        let bytes = enc(&m);
        assert_eq!(bytes.len(), HEADER_LEN + MAX_PAYLOAD);
        assert_eq!(decode(&bytes), Ok(Some((m, bytes.len()))));
        let full = Message::Events {
            sub: 1,
            events: (0..MAX_EVENTS_PER_FRAME)
                .map(|i| Event {
                    cursor: vec![u8::try_from(i % 256).unwrap()],
                    data: vec![],
                })
                .collect(),
        };
        let bytes = enc(&full);
        assert_eq!(decode(&bytes), Ok(Some((full, bytes.len()))));
    }

    /// The table of the format document: what a server speaking only version 1 does with an offer.
    #[test]
    fn negotiation_table() {
        let table: [(u16, u16, Result<u16, ErrorCode>); 11] = [
            (1, 1, Ok(1)),
            (1, 2, Ok(1)),
            (1, 65535, Ok(1)),
            (0, 1, Err(ErrorCode::Malformed)),
            (0, 0, Err(ErrorCode::Malformed)),
            (2, 1, Err(ErrorCode::Malformed)),
            (2, 2, Err(ErrorCode::UnsupportedVersion)),
            (2, 9, Err(ErrorCode::UnsupportedVersion)),
            (65535, 65535, Err(ErrorCode::UnsupportedVersion)),
            (0, 65535, Err(ErrorCode::Malformed)),
            (1, 0, Err(ErrorCode::Malformed)),
        ];
        for (min, max, want) in table {
            assert_eq!(negotiate(min, max), want, "offer {min}..={max}");
        }
    }

    #[test]
    fn error_codes_round_trip_and_reject_the_rest() {
        for n in 0..=u16::MAX {
            match ErrorCode::from_code(n) {
                Some(c) => assert_eq!(c.code(), n),
                None => assert!(n == 0 || n > 7),
            }
        }
    }

    /// Text of `min..=max` bytes: at most `max / 4` characters of up to four bytes each.
    fn text(min: usize, max: usize) -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                proptest::char::range('a', 'z'),
                proptest::char::range('0', '9'),
                Just('/'),
                Just('é'),
                Just('語'),
                Just('😀'),
            ],
            min..=max / 4,
        )
        .prop_map(|cs| cs.into_iter().collect())
    }

    fn cursor(min: usize) -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), min..=MAX_CURSOR_LEN)
    }

    fn event() -> impl Strategy<Value = Event> {
        (cursor(1), proptest::collection::vec(any::<u8>(), 0..64))
            .prop_map(|(cursor, data)| Event { cursor, data })
    }

    fn code() -> impl Strategy<Value = ErrorCode> {
        (1u16..=7).prop_map(|n| ErrorCode::from_code(n).unwrap())
    }

    fn message() -> impl Strategy<Value = Message> {
        prop_oneof![
            (1u16..=u16::MAX)
                .prop_flat_map(|min| (Just(min), min..=u16::MAX, text(1, MAX_NAME_LEN)))
                .prop_map(|(min_version, max_version, agent)| Message::Hello {
                    min_version,
                    max_version,
                    agent
                }),
            (
                Just(1u16),
                MIN_NEGOTIATED_PAYLOAD..=1_048_576u32,
                text(1, MAX_NAME_LEN)
            )
                .prop_map(|(version, max_payload, server)| Message::Welcome {
                    version,
                    max_payload,
                    server
                }),
            (1u32.., text(1, MAX_SCOPE_LEN), cursor(0))
                .prop_map(|(sub, scope, cursor)| Message::Subscribe { sub, scope, cursor }),
            (1u32.., proptest::collection::vec(event(), 1..=8))
                .prop_map(|(sub, events)| Message::Events { sub, events }),
            (1u32.., cursor(1)).prop_map(|(sub, cursor)| Message::Ack { sub, cursor }),
            (code(), any::<u32>(), text(0, MAX_ERROR_MESSAGE_LEN))
                .prop_map(|(code, sub, message)| Message::Error { code, sub, message }),
            any::<u64>().prop_map(|nonce| Message::Ping { nonce }),
            any::<u64>().prop_map(|nonce| Message::Pong { nonce }),
        ]
    }

    /// Valid frames damaged in one way, and raw noise.
    fn wild_bytes() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            proptest::collection::vec(any::<u8>(), 0..96),
            (message(), any::<u8>(), any::<prop::sample::Index>(), 0u8..4).prop_map(
                |(m, byte, at, how)| {
                    let mut b = enc(&m);
                    match how {
                        0 => {
                            let i = at.index(b.len());
                            b[i] = byte;
                        }
                        1 => b.truncate(at.index(b.len())),
                        2 => b.push(byte),
                        _ => {
                            let i = at.index(b.len());
                            b.insert(i, byte);
                        }
                    }
                    b
                }
            ),
        ]
    }

    /// The committed fuzz seeds mean what their names say, so the fuzzer starts from real frames.
    #[test]
    fn fuzz_seeds_are_what_they_claim() {
        macro_rules! seed {
            ($name:literal) => {
                include_bytes!(concat!("../../../fuzz/seeds/sync_frame/", $name)).as_slice()
            };
        }
        for (n, b) in [
            ("hello", seed!("hello")),
            ("hello-range", seed!("hello-range")),
            ("welcome", seed!("welcome")),
            ("subscribe-start", seed!("subscribe-start")),
            ("subscribe-resume", seed!("subscribe-resume")),
            ("events", seed!("events")),
            ("ack", seed!("ack")),
            ("error", seed!("error")),
            ("error-subscription", seed!("error-subscription")),
            ("ping", seed!("ping")),
            ("pong", seed!("pong")),
        ] {
            let (_, used) = decode(b)
                .unwrap()
                .unwrap_or_else(|| panic!("{n} incomplete"));
            assert_eq!(used, b.len(), "{n}");
        }
        let two = seed!("two-frames");
        let (_, first) = decode(two).unwrap().unwrap();
        assert!(first < two.len() && decode(&two[first..]).unwrap().is_some());
        assert_eq!(decode(seed!("trailing-byte")), Err(SyncError::Trailing));
        assert_eq!(
            decode(seed!("unknown-type")),
            Err(SyncError::UnknownType(9))
        );
        assert_eq!(
            decode(seed!("oversize-length")),
            Err(SyncError::BadLength(1_048_577))
        );
        assert_eq!(decode(seed!("truncated")), Ok(None));
        assert_eq!(
            decode(seed!("zero-version")),
            Err(SyncError::InvalidField("version range"))
        );
        assert_eq!(
            decode(seed!("bad-error-code")),
            Err(SyncError::InvalidField("error code"))
        );
    }

    proptest! {
        #[test]
        fn every_message_round_trips(m in message()) {
            let bytes = enc(&m);
            prop_assert_eq!(decode(&bytes), Ok(Some((m.clone(), bytes.len()))));
            // Any proper prefix is incomplete, never an error and never a message.
            for cut in [0, 1, HEADER_LEN.min(bytes.len() - 1), bytes.len() - 1] {
                prop_assert_eq!(decode(&bytes[..cut]), Ok(None));
            }
            // A byte appended after the frame is not part of it.
            let mut more = bytes.clone();
            more.push(0xee);
            prop_assert_eq!(decode(&more), Ok(Some((m, bytes.len()))));
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_decoded_frames_re_encode(b in wild_bytes()) {
            if let Ok(Some((m, used))) = decode(&b) {
                prop_assert!(used <= b.len());
                prop_assert_eq!(enc(&m), b[..used].to_vec());
            }
        }

        #[test]
        fn trailing_payload_bytes_are_refused(m in message(), extra in 1u32..4) {
            let mut b = enc(&m);
            let len = u32::from_be_bytes([b[1], b[2], b[3], b[4]]) + extra;
            b[1..5].copy_from_slice(&len.to_be_bytes());
            b.extend(std::iter::repeat_n(0u8, extra as usize));
            // Every message is self-delimiting, so a longer payload always leaves bytes over.
            prop_assert_eq!(decode(&b), Err(SyncError::Trailing));
        }
    }
}

/// Kani proofs, run by `.github/workflows/kani.yml`; see `docs/verification.md`.
#[cfg(kani)]
#[allow(clippy::indexing_slicing)]
mod proofs {
    use super::*;

    /// `decode` never panics on a byte string of up to 24 bytes (the five header bytes plus a
    /// payload of up to 19), and when it returns a frame the length it reports is within the
    /// input. A bounded proof: larger payloads, and so the largest events and texts, are
    /// covered by the property and fuzz tests only.
    #[kani::proof]
    #[kani::unwind(26)]
    fn frame_decode_never_panics() {
        let raw: [u8; 24] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= raw.len());
        if let Ok(Some((_, used))) = decode(&raw[..len]) {
            assert!(used >= HEADER_LEN && used <= len);
        }
    }

    /// The same with a symbolic payload limit, including limits above and below the frame.
    #[kani::proof]
    #[kani::unwind(26)]
    fn frame_decode_with_limit_never_panics() {
        let raw: [u8; 24] = kani::any();
        let len: usize = kani::any();
        let limit: usize = kani::any();
        kani::assume(len <= raw.len());
        if let Ok(Some((_, used))) = decode_with_limit(&raw[..len], limit) {
            assert!(used >= HEADER_LEN && used <= len);
            assert!(used - HEADER_LEN <= limit.min(MAX_PAYLOAD));
        }
    }
}
