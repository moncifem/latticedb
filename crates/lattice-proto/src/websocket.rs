//! Sans-I/O WebSocket (RFC 6455) server-side codec for the sync gateway.
//!
//! The sync protocol ([`crate::sync`], `formats/sync-protocol-v1.md`) runs over any ordered, reliable
//! byte stream. This module is the WebSocket layer that carries it to browsers and SDKs. It does no
//! I/O, reads no clock, draws no randomness and has no panic path: the caller reads bytes from a
//! socket, passes them in with a logical time and writes back whatever the module hands out. The
//! server never masks a frame; the mask key of a client frame is read from the frame.
//!
//! # Parts
//!
//! * [`parse_upgrade`]: the HTTP/1.1 upgrade request. Strict and bounded; every refusal is a
//!   [`HandshakeError`] that renders its own 400, 403, 426 or 431 response. [`accept_key`] is the
//!   `Sec-WebSocket-Accept` computation and [`Upgrade::response`] the 101 response.
//! * [`decode_client_frame`], [`decode_server_frame`], [`encode_frame`], [`encode_client_frame`]:
//!   single frames. Client frames must be masked, server frames must not be; lengths must use the
//!   shortest form; reserved bits and opcodes, fragmented or long control frames and payloads above
//!   the configured cap are refused as soon as the header shows them, before the payload is awaited.
//! * [`Connection`]: the connection state machine after the upgrade. It reassembles fragmented
//!   messages (with a message cap and incremental UTF-8 validation for text), answers pings, runs the
//!   close handshake and the ping and close timers on the caller's clock, and queues the frames it
//!   wants sent.
//!
//! # Driving a connection
//!
//! ```text
//! loop {
//!     let step = conn.receive(&buffer[offset..], now);
//!     offset += step.consumed;
//!     if let Some(event) = step.event { handle(event) }
//!     if step.consumed == 0 && step.event.is_none() { break }   // need more bytes
//! }
//! conn.drain_output(&mut out);   // write `out` to the socket
//! conn.tick(now);                // now and then; `next_deadline` says when
//! ```
//!
//! The caller keeps the bytes `receive` did not consume. A frame is processed whole, so the buffer
//! never needs to hold more than 14 bytes plus [`Config::max_frame_payload`].
//!
//! # Bounds
//!
//! A connection holds at most one message being reassembled (at most [`Config::max_message`] bytes),
//! one pending pong (at most 125 bytes), one pending ping (8 bytes) and one pending close (at most
//! 125 bytes). A newer ping replaces an unsent pong, which RFC 6455 section 5.5.3 allows. After a
//! protocol error the connection queues one close frame, ignores all further input and reports
//! [`Event::ProtocolError`]; the caller flushes the output and drops the socket.
//!
//! # Compression
//!
//! No extension is negotiated, so `permessage-deflate` is never active and the reserved bits must be
//! zero. This keeps frame sizes predictable and avoids compression side channels on authenticated
//! data.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha1::{Digest, Sha1};

use crate::sync;

/// The WebSocket subprotocol the sync gateway speaks.
pub const SUBPROTOCOL: &str = "lattice.sync.v1";
/// The GUID RFC 6455 section 1.3 appends to the client key to form the accept key.
pub const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The only protocol version the handshake accepts.
pub const WEBSOCKET_VERSION: &str = "13";
/// Largest payload of a control frame (RFC 6455 section 5.5).
pub const MAX_CONTROL_PAYLOAD: usize = 125;
/// Largest number of header lines an upgrade request may carry.
pub const MAX_HEADERS: usize = 64;
/// Default cap on the upgrade request head.
pub const DEFAULT_MAX_HEAD: usize = 8 * 1024;
/// Default cap on the payload of one frame: one maximal sync frame (header and payload), so a client
/// that sends it as a single unfragmented message is accepted.
pub const DEFAULT_MAX_FRAME_PAYLOAD: usize = sync::HEADER_LEN + sync::MAX_PAYLOAD;
/// Default cap on a reassembled message: one maximal sync frame (header and payload).
pub const DEFAULT_MAX_MESSAGE: usize = sync::HEADER_LEN + sync::MAX_PAYLOAD;
/// Largest close reason: a close payload is at most 125 bytes and two are the status code.
pub const MAX_CLOSE_REASON: usize = MAX_CONTROL_PAYLOAD - 2;

/// Close status codes this module sends (RFC 6455 section 7.4.1).
pub mod close_code {
    /// Normal closure.
    pub const NORMAL: u16 = 1000;
    /// The endpoint is going away, also used for a ping timeout.
    pub const GOING_AWAY: u16 = 1001;
    /// Protocol error.
    pub const PROTOCOL: u16 = 1002;
    /// A data type the endpoint cannot accept (text on a binary-only connection).
    pub const UNSUPPORTED: u16 = 1003;
    /// A text message or close reason that is not valid UTF-8.
    pub const INVALID_PAYLOAD: u16 = 1007;
    /// A message that violates policy.
    pub const POLICY: u16 = 1008;
    /// A frame or message above the configured cap.
    pub const TOO_BIG: u16 = 1009;
    /// Unexpected server condition.
    pub const INTERNAL: u16 = 1011;
}

// ---------------------------------------------------------------------------------------------
// HTTP upgrade
// ---------------------------------------------------------------------------------------------

/// A refused upgrade request. [`HandshakeError::response`] renders the HTTP response to send before
/// closing the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeError {
    /// HTTP status: 400, 403, 426 or 431.
    pub status: u16,
    /// A short fixed explanation, also the response body.
    pub reason: &'static str,
}

impl HandshakeError {
    const fn new(status: u16, reason: &'static str) -> Self {
        Self { status, reason }
    }

    const fn bad(reason: &'static str) -> Self {
        Self::new(400, reason)
    }

    /// The complete HTTP response: the status line, `Connection: close`, `Content-Length`, and for a
    /// 426 the `Sec-WebSocket-Version` header RFC 6455 section 4.4 requires. The body is the reason.
    #[must_use]
    pub fn response(&self) -> Vec<u8> {
        let phrase = match self.status {
            403 => "Forbidden",
            426 => "Upgrade Required",
            431 => "Request Header Fields Too Large",
            _ => "Bad Request",
        };
        let mut out = format!("HTTP/1.1 {} {phrase}\r\n", self.status);
        if self.status == 426 {
            out.push_str("Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n");
        }
        out.push_str("Connection: close\r\nContent-Type: text/plain; charset=utf-8\r\n");
        out.push_str(&format!(
            "Content-Length: {}\r\n\r\n{}\n",
            self.reason.len() + 1,
            self.reason
        ));
        out.into_bytes()
    }
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "websocket upgrade refused ({}): {}",
            self.status, self.reason
        )
    }
}

impl std::error::Error for HandshakeError {}

/// Which `Origin` header values the upgrade accepts. A browser always sends `Origin` on a WebSocket
/// request, and checking it is the defence against cross-site WebSocket hijacking; a non-browser
/// client usually sends none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginPolicy {
    /// Accept any origin and none.
    Any,
    /// Accept only requests without an `Origin` header (no browser may connect).
    NoBrowsers,
    /// Accept requests without an `Origin` header and those whose origin equals one of these
    /// (ASCII case-insensitive, for example `https://app.example.com`).
    AllowList(Vec<String>),
}

impl OriginPolicy {
    /// Whether `origin` (the header value, if any) is acceptable.
    #[must_use]
    pub fn allows(&self, origin: Option<&str>) -> bool {
        match (self, origin) {
            (Self::Any, _) | (_, None) => true,
            (Self::NoBrowsers, Some(_)) => false,
            (Self::AllowList(list), Some(o)) => list.iter().any(|a| a.eq_ignore_ascii_case(o)),
        }
    }
}

/// Limits and policy for [`parse_upgrade`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeConfig {
    /// Largest request head (request line, headers and the blank line) in bytes.
    pub max_head: usize,
    /// Which origins may connect.
    pub origins: OriginPolicy,
    /// Refuse a request that does not offer [`SUBPROTOCOL`].
    pub require_subprotocol: bool,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            max_head: DEFAULT_MAX_HEAD,
            origins: OriginPolicy::NoBrowsers,
            require_subprotocol: true,
        }
    }
}

/// An accepted upgrade request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    /// The request target, always starting with `/`.
    pub path: String,
    /// The `Host` header value.
    pub host: String,
    /// The `Origin` header value, if the client sent one.
    pub origin: Option<String>,
    /// The `Sec-WebSocket-Key` header value (24 base64 characters, 16 bytes decoded).
    pub key: String,
    /// The subprotocol selected for the response, if the client offered it.
    pub subprotocol: Option<&'static str>,
}

impl Upgrade {
    /// The `Sec-WebSocket-Accept` value for this request.
    #[must_use]
    pub fn accept(&self) -> String {
        accept_key(&self.key)
    }

    /// The `101 Switching Protocols` response. No extension is announced.
    #[must_use]
    pub fn response(&self) -> Vec<u8> {
        let mut out = String::from(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n",
        );
        out.push_str("Sec-WebSocket-Accept: ");
        out.push_str(&self.accept());
        out.push_str("\r\n");
        if let Some(p) = self.subprotocol {
            out.push_str("Sec-WebSocket-Protocol: ");
            out.push_str(p);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// `Sec-WebSocket-Accept` for a `Sec-WebSocket-Key` (RFC 6455 section 4.2.2): base64 of the SHA-1 of
/// the key text followed by [`ACCEPT_GUID`].
#[must_use]
pub fn accept_key(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(ACCEPT_GUID.as_bytes());
    STANDARD.encode(sha.finalize())
}

/// Parse an upgrade request using the origin policy in `cfg`. See [`parse_upgrade_with`].
///
/// # Errors
///
/// A [`HandshakeError`] for any request that is malformed or not acceptable.
pub fn parse_upgrade(
    input: &[u8],
    cfg: &UpgradeConfig,
) -> Result<Option<(Upgrade, usize)>, HandshakeError> {
    parse_upgrade_with(input, cfg, &|origin| cfg.origins.allows(origin))
}

/// Parse an HTTP/1.1 WebSocket upgrade request from the start of `input`.
///
/// The parser is stateless and has no clock: the host must time the handshake (a client that
/// trickles the head byte by byte is otherwise never cut off) and limit connections in this phase.
///
/// Returns `Ok(None)` while the head is incomplete and `Ok(Some((upgrade, used)))` once it is whole;
/// `used` is the length of the head, and any bytes after it already belong to the WebSocket stream.
/// `origin_ok` is the origin hook: it gets the `Origin` header value (or `None`) and returns whether
/// to accept; it replaces `cfg.origins`.
///
/// The request must be `GET <target> HTTP/1.1` with exactly one each of `Host`, `Sec-WebSocket-Key`
/// (16 bytes in canonical base64) and `Sec-WebSocket-Version: 13`, a `Connection` header listing
/// `upgrade`, and an `Upgrade` header listing `websocket`. Header lines must be plain `name: value`
/// pairs ended by CRLF: no folding, no control characters, no non-ASCII bytes. A body
/// (`Content-Length` other than 0, or `Transfer-Encoding`) is refused. The head is bounded by
/// `cfg.max_head` bytes and [`MAX_HEADERS`] lines, and a request that cannot be a `GET` is refused
/// after its first bytes, without waiting for the rest.
///
/// # Errors
///
/// 400 for a malformed request, 403 for a refused origin, 426 for a version other than 13 and 431
/// for a head that is too large.
pub fn parse_upgrade_with(
    input: &[u8],
    cfg: &UpgradeConfig,
    origin_ok: &dyn Fn(Option<&str>) -> bool,
) -> Result<Option<(Upgrade, usize)>, HandshakeError> {
    let window = input.get(..input.len().min(cfg.max_head)).unwrap_or(input);
    let probe = b"GET ";
    let n = window.len().min(probe.len());
    if window.get(..n) != probe.get(..n) {
        return Err(HandshakeError::bad("the request method must be GET"));
    }
    let Some(end) = window.windows(4).position(|w| w == b"\r\n\r\n") else {
        if input.len() >= cfg.max_head {
            return Err(HandshakeError::new(431, "the request head is too large"));
        }
        return Ok(None);
    };
    let head = window.get(..end).unwrap_or_default();
    let Ok(text) = std::str::from_utf8(head) else {
        return Err(HandshakeError::bad("the request head must be ASCII"));
    };
    if !text.is_ascii() {
        return Err(HandshakeError::bad("the request head must be ASCII"));
    }
    let upgrade = parse_head(text, cfg, origin_ok)?;
    Ok(Some((upgrade, end + 4)))
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn has_token<'a>(values: impl Iterator<Item = &'a str>, token: &str) -> bool {
    values
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

type Headers<'a> = [(String, &'a str)];

fn values<'a>(headers: &'a Headers<'a>, name: &'a str) -> impl Iterator<Item = &'a str> {
    headers
        .iter()
        .filter(move |(n, _)| n == name)
        .map(|(_, v)| *v)
}

fn single<'a>(
    headers: &'a Headers<'a>,
    name: &'a str,
    missing: &'static str,
) -> Result<&'a str, HandshakeError> {
    let mut it = values(headers, name);
    match (it.next(), it.next()) {
        (Some(v), None) => Ok(v),
        (None, _) => Err(HandshakeError::bad(missing)),
        (Some(_), Some(_)) => Err(HandshakeError::bad(
            "a header that must appear once is repeated",
        )),
    }
}

fn parse_head(
    text: &str,
    cfg: &UpgradeConfig,
    origin_ok: &dyn Fn(Option<&str>) -> bool,
) -> Result<Upgrade, HandshakeError> {
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    if request_line.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(HandshakeError::bad("control character in the request line"));
    }
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HandshakeError::bad("malformed request line"));
    };
    if method != "GET" {
        return Err(HandshakeError::bad("the request method must be GET"));
    }
    if version != "HTTP/1.1" {
        return Err(HandshakeError::bad("the request must use HTTP/1.1"));
    }
    if !target.starts_with('/') {
        return Err(HandshakeError::bad("the request target must start with /"));
    }

    let mut headers: Vec<(String, &str)> = Vec::new();
    for line in lines {
        if headers.len() >= MAX_HEADERS {
            return Err(HandshakeError::new(431, "too many header lines"));
        }
        if line.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
            return Err(HandshakeError::bad("control character in a header line"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HandshakeError::bad("malformed header line"));
        };
        if !is_token(name) {
            return Err(HandshakeError::bad("malformed header name"));
        }
        headers.push((
            name.to_ascii_lowercase(),
            value.trim_matches(|c| c == ' ' || c == '\t'),
        ));
    }

    if headers
        .iter()
        .any(|(n, v)| (n == "content-length" && *v != "0") || n == "transfer-encoding")
    {
        return Err(HandshakeError::bad(
            "an upgrade request must not have a body",
        ));
    }
    let host = single(&headers, "host", "missing Host header")?;
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]_".contains(&b))
    {
        return Err(HandshakeError::bad("malformed Host header"));
    }
    if !has_token(values(&headers, "upgrade"), "websocket") {
        return Err(HandshakeError::bad("missing Upgrade: websocket"));
    }
    if !has_token(values(&headers, "connection"), "upgrade") {
        return Err(HandshakeError::bad("missing Connection: Upgrade"));
    }
    let version = single(
        &headers,
        "sec-websocket-version",
        "missing Sec-WebSocket-Version header",
    )?;
    if version != WEBSOCKET_VERSION {
        return Err(HandshakeError::new(
            426,
            "only WebSocket version 13 is supported",
        ));
    }
    let key = single(
        &headers,
        "sec-websocket-key",
        "missing Sec-WebSocket-Key header",
    )?;
    let key_ok = key.len() == 24 && STANDARD.decode(key).is_ok_and(|k| k.len() == 16);
    if !key_ok {
        return Err(HandshakeError::bad(
            "Sec-WebSocket-Key must be 16 bytes in base64",
        ));
    }
    let mut origins = values(&headers, "origin");
    let origin = match (origins.next(), origins.next()) {
        (_, Some(_)) => {
            return Err(HandshakeError::bad(
                "a header that must appear once is repeated",
            ));
        }
        (o, None) => o,
    };
    if !origin_ok(origin) {
        return Err(HandshakeError::new(403, "this origin is not allowed"));
    }
    // Subprotocol names are case-sensitive (RFC 6455 section 4.1), unlike the Upgrade and Connection tokens.
    let subprotocol = values(&headers, "sec-websocket-protocol")
        .flat_map(|v| v.split(','))
        .any(|t| t.trim() == SUBPROTOCOL)
        .then_some(SUBPROTOCOL);
    if subprotocol.is_none() && cfg.require_subprotocol {
        return Err(HandshakeError::bad(
            "the subprotocol lattice.sync.v1 must be offered",
        ));
    }
    Ok(Upgrade {
        path: target.to_owned(),
        host: host.to_owned(),
        origin: origin.map(str::to_owned),
        key: key.to_owned(),
        subprotocol,
    })
}

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/// A frame opcode this module accepts. Reserved opcodes are protocol errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    /// 0x0: continues a fragmented message.
    Continuation,
    /// 0x1: starts a text message.
    Text,
    /// 0x2: starts a binary message.
    Binary,
    /// 0x8: close.
    Close,
    /// 0x9: ping.
    Ping,
    /// 0xA: pong.
    Pong,
}

impl Opcode {
    const fn from_nibble(n: u8) -> Option<Self> {
        match n {
            0x0 => Some(Self::Continuation),
            0x1 => Some(Self::Text),
            0x2 => Some(Self::Binary),
            0x8 => Some(Self::Close),
            0x9 => Some(Self::Ping),
            0xA => Some(Self::Pong),
            _ => None,
        }
    }

    const fn nibble(self) -> u8 {
        match self {
            Self::Continuation => 0x0,
            Self::Text => 0x1,
            Self::Binary => 0x2,
            Self::Close => 0x8,
            Self::Ping => 0x9,
            Self::Pong => 0xA,
        }
    }

    /// Whether this is a control frame (close, ping or pong).
    #[must_use]
    pub const fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

/// One decoded frame. The payload is already unmasked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The FIN bit: this is the last (or only) frame of its message.
    pub fin: bool,
    /// The opcode.
    pub opcode: Opcode,
    /// The payload, unmasked.
    pub payload: Vec<u8>,
}

/// A protocol violation found while decoding, with the close code to answer it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Violation {
    /// The close status code: 1002 for a protocol error, 1009 for a frame above the cap.
    pub code: u16,
    /// A short fixed explanation.
    pub reason: &'static str,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "websocket violation ({}): {}", self.code, self.reason)
    }
}

impl std::error::Error for Violation {}

const fn protocol(reason: &'static str) -> Violation {
    Violation {
        code: close_code::PROTOCOL,
        reason,
    }
}

fn array<const N: usize>(input: &[u8], at: usize) -> Option<[u8; N]> {
    input.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// Decode one frame a client sent: it must be masked. Returns `Ok(None)` while the frame is
/// incomplete and the frame with the number of bytes it used once it is whole.
///
/// Everything that can be judged from the header is judged as soon as those bytes are present: a
/// reserved bit or opcode, a missing mask, a control frame that is fragmented or longer than 125
/// bytes, a length not in its shortest form, a 64-bit length with the top bit set, and a length above
/// `max_payload`.
///
/// # Errors
///
/// A [`Violation`] with close code 1002, or 1009 for a payload above `max_payload`.
pub fn decode_client_frame(
    input: &[u8],
    max_payload: usize,
) -> Result<Option<(Frame, usize)>, Violation> {
    decode_frame(input, max_payload, true)
}

/// Decode one frame a server sent: it must not be masked. The server side of a connection never
/// needs this; SDK clients and tests do. Rules and results are those of [`decode_client_frame`].
///
/// # Errors
///
/// A [`Violation`] as for [`decode_client_frame`].
pub fn decode_server_frame(
    input: &[u8],
    max_payload: usize,
) -> Result<Option<(Frame, usize)>, Violation> {
    decode_frame(input, max_payload, false)
}

fn decode_frame(
    input: &[u8],
    max_payload: usize,
    masked: bool,
) -> Result<Option<(Frame, usize)>, Violation> {
    let Some(&b0) = input.first() else {
        return Ok(None);
    };
    if b0 & 0x70 != 0 {
        return Err(protocol("reserved bits must be zero"));
    }
    let fin = b0 & 0x80 != 0;
    let Some(opcode) = Opcode::from_nibble(b0 & 0x0f) else {
        return Err(protocol("reserved opcode"));
    };
    let Some(&b1) = input.get(1) else {
        return Ok(None);
    };
    if (b1 & 0x80 != 0) != masked {
        return Err(protocol(if masked {
            "client frames must be masked"
        } else {
            "server frames must not be masked"
        }));
    }
    let short = usize::from(b1 & 0x7f);
    if opcode.is_control() {
        if !fin {
            return Err(protocol("control frames must not be fragmented"));
        }
        if short > MAX_CONTROL_PAYLOAD {
            return Err(protocol("control frame payload above 125 bytes"));
        }
    }
    let (len, mut at) = match short {
        126 => {
            let Some(b) = array::<2>(input, 2) else {
                return Ok(None);
            };
            let n = u16::from_be_bytes(b);
            if n < 126 {
                return Err(protocol("length not in its shortest form"));
            }
            (u64::from(n), 4)
        }
        127 => {
            let Some(b) = array::<8>(input, 2) else {
                return Ok(None);
            };
            let n = u64::from_be_bytes(b);
            if n >> 63 != 0 {
                return Err(protocol("64-bit length with the top bit set"));
            }
            if n < 1 << 16 {
                return Err(protocol("length not in its shortest form"));
            }
            (n, 10)
        }
        n => (n as u64, 2),
    };
    let too_big = Violation {
        code: close_code::TOO_BIG,
        reason: "frame payload above the limit",
    };
    let len = usize::try_from(len).map_err(|_| too_big)?;
    if len > max_payload {
        return Err(too_big);
    }
    let key = if masked {
        let Some(k) = array::<4>(input, at) else {
            return Ok(None);
        };
        at += 4;
        Some(k)
    } else {
        None
    };
    let total = at.checked_add(len).ok_or(too_big)?;
    let Some(body) = input.get(at..total) else {
        return Ok(None);
    };
    let payload = match key {
        Some(k) => body
            .iter()
            .zip(k.iter().cycle())
            .map(|(b, m)| b ^ m)
            .collect(),
        None => body.to_vec(),
    };
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload,
        },
        total,
    )))
}

/// Why a frame could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// A control frame with `fin` clear or a payload above 125 bytes.
    BadControlFrame,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("control frames must be final and at most 125 bytes")
    }
}

impl std::error::Error for EncodeError {}

fn push_header(out: &mut Vec<u8>, fin: bool, opcode: Opcode, len: usize, mask: bool) {
    out.push(u8::from(fin) << 7 | opcode.nibble());
    let m = u8::from(mask) << 7;
    if len < 126 {
        out.push(m | len as u8);
    } else if let Ok(n) = u16::try_from(len) {
        out.push(m | 126);
        out.extend_from_slice(&n.to_be_bytes());
    } else {
        out.push(m | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

/// Append one unmasked (server to client) frame, with the shortest length form.
///
/// # Errors
///
/// [`EncodeError::BadControlFrame`] for a control frame that is not final or is longer than 125
/// bytes; nothing is appended then.
pub fn encode_frame(
    fin: bool,
    opcode: Opcode,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    if opcode.is_control() && (!fin || payload.len() > MAX_CONTROL_PAYLOAD) {
        return Err(EncodeError::BadControlFrame);
    }
    out.reserve(payload.len().saturating_add(10));
    push_header(out, fin, opcode, payload.len(), false);
    out.extend_from_slice(payload);
    Ok(())
}

/// Append one masked (client to server) frame with the given mask key. The server never calls this;
/// it exists for SDK clients and tests, and takes the key from the caller so it stays deterministic.
///
/// # Errors
///
/// [`EncodeError::BadControlFrame`] as for [`encode_frame`].
pub fn encode_client_frame(
    fin: bool,
    opcode: Opcode,
    payload: &[u8],
    key: [u8; 4],
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    if opcode.is_control() && (!fin || payload.len() > MAX_CONTROL_PAYLOAD) {
        return Err(EncodeError::BadControlFrame);
    }
    out.reserve(payload.len().saturating_add(14));
    push_header(out, fin, opcode, payload.len(), true);
    out.extend_from_slice(&key);
    out.extend(payload.iter().zip(key.iter().cycle()).map(|(b, m)| b ^ m));
    Ok(())
}

/// Whether a peer may put `code` in a close frame (RFC 6455 section 7.4): the registered codes and
/// the private range, but not 1004, 1005, 1006, 1015, nor anything below 1000. Codes 1012 to 1014 are
/// accepted because IANA registers them, although old Autobahn versions (cases 7.9.7 to 7.9.9) treat
/// them as invalid.
#[must_use]
pub const fn is_valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

fn parse_close(payload: &[u8]) -> Result<(Option<u16>, String), Violation> {
    match payload.len() {
        0 => Ok((None, String::new())),
        1 => Err(protocol("close payload of one byte")),
        _ => {
            let code = u16::from_be_bytes(array::<2>(payload, 0).ok_or(protocol("close payload"))?);
            if !is_valid_close_code(code) {
                return Err(protocol("invalid close status code"));
            }
            let reason =
                std::str::from_utf8(payload.get(2..).unwrap_or_default()).map_err(|_| {
                    Violation {
                        code: close_code::INVALID_PAYLOAD,
                        reason: "close reason is not valid UTF-8",
                    }
                })?;
            Ok((Some(code), reason.to_owned()))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------------------------

/// Limits and timers of a [`Connection`]. Times are in the unit of the caller's logical clock
/// (milliseconds is natural); the module never reads a real clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Largest payload of one frame, checked from the header (default 1 MiB).
    pub max_frame_payload: usize,
    /// Largest reassembled message (default one maximal sync frame).
    pub max_message: usize,
    /// Accept text messages. A binary-only gateway sets this to false and answers text with 1003.
    pub allow_text: bool,
    /// Send a ping after this long without receiving a frame; 0 disables pings.
    pub ping_interval: u64,
    /// Give up on a peer that has not answered a ping with any frame within this long.
    pub pong_timeout: u64,
    /// Give up on a close handshake the peer has not answered within this long.
    pub close_timeout: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_frame_payload: DEFAULT_MAX_FRAME_PAYLOAD,
            max_message: DEFAULT_MAX_MESSAGE,
            allow_text: true,
            ping_interval: 30_000,
            pong_timeout: 10_000,
            close_timeout: 5_000,
        }
    }
}

/// A complete message after reassembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A text message, valid UTF-8.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
}

/// Something the caller must act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A whole message arrived.
    Message(Message),
    /// The peer closed. The echo is queued; flush the output, then drop the socket.
    Closed {
        /// The status code the peer sent, if it sent one.
        code: Option<u16>,
        /// The reason text, possibly empty.
        reason: String,
    },
    /// The peer broke the protocol. A close frame with `code` is queued; flush the output, then drop
    /// the socket. The connection ignores all further input.
    ProtocolError {
        /// The close code that was queued.
        code: u16,
        /// A short fixed explanation.
        reason: &'static str,
    },
    /// A ping or close timer ran out. Drop the socket (after flushing the queued close, if any).
    TimedOut,
}

/// The result of one [`Connection::receive`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Bytes of the input that were used. Zero with no event means more bytes are needed.
    pub consumed: usize,
    /// What the frame produced, if anything.
    pub event: Option<Event>,
}

/// Why a send or close request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The connection is closing or closed.
    NotOpen,
    /// The message is above [`Config::max_message`].
    TooLarge,
    /// The close code is not allowed in a close frame, or the reason is above 123 bytes.
    BadClose,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotOpen => "the connection is not open",
            Self::TooLarge => "the message is above the configured limit",
            Self::BadClose => "invalid close code or reason",
        })
    }
}

impl std::error::Error for SendError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    CloseSent,
    Closed,
    Failed,
}

#[derive(Debug)]
struct Assembly {
    text: bool,
    buf: Vec<u8>,
    /// Prefix of `buf` already known to be valid UTF-8 (text only).
    valid: usize,
}

/// Extend `valid` over the new bytes of `buf`. `Err` means the bytes can never become valid UTF-8; an
/// incomplete character at the end is not an error yet.
fn extend_utf8(buf: &[u8], valid: usize) -> Result<usize, ()> {
    let tail = buf.get(valid..).ok_or(())?;
    match std::str::from_utf8(tail) {
        Ok(_) => Ok(buf.len()),
        Err(e) if e.error_len().is_some() => Err(()),
        Err(e) => Ok(valid + e.valid_up_to()),
    }
}

/// The state of one WebSocket connection after the upgrade.
#[derive(Debug)]
pub struct Connection {
    cfg: Config,
    phase: Phase,
    asm: Option<Assembly>,
    pong: Option<Vec<u8>>,
    ping: Option<[u8; 8]>,
    close: Option<Vec<u8>>,
    last_rx: u64,
    pings_sent: u64,
    ping_deadline: Option<u64>,
    close_deadline: Option<u64>,
}

impl Connection {
    /// A new open connection; `now` is the logical time of the upgrade.
    #[must_use]
    pub fn new(cfg: Config, now: u64) -> Self {
        Self {
            cfg,
            phase: Phase::Open,
            asm: None,
            pong: None,
            ping: None,
            close: None,
            last_rx: now,
            pings_sent: 0,
            ping_deadline: None,
            close_deadline: None,
        }
    }

    /// Whether the connection is finished: the close handshake completed, a protocol error or a
    /// timeout ended it. The caller drops the socket after flushing the output.
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Closed | Phase::Failed)
    }

    /// Whether messages can still be sent.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.phase == Phase::Open
    }

    /// Process at most one frame from the start of `input`. See the module documentation for the
    /// loop. A finished connection consumes nothing and reports nothing.
    pub fn receive(&mut self, input: &[u8], now: u64) -> Step {
        let none = Step {
            consumed: 0,
            event: None,
        };
        if self.is_done() {
            return none;
        }
        let (frame, used) = match decode_client_frame(input, self.cfg.max_frame_payload) {
            Ok(Some(found)) => found,
            Ok(None) => return none,
            Err(v) => {
                return Step {
                    consumed: 0,
                    event: Some(self.fail(v.code, v.reason)),
                };
            }
        };
        self.last_rx = now;
        self.ping_deadline = None;
        let event = match frame.opcode {
            Opcode::Ping => {
                if self.phase == Phase::Open {
                    self.pong = Some(frame.payload);
                }
                None
            }
            Opcode::Pong => None,
            Opcode::Close => Some(self.on_close(&frame.payload)),
            Opcode::Text | Opcode::Binary | Opcode::Continuation => self.on_data(frame),
        };
        Step {
            consumed: used,
            event,
        }
    }

    fn fail(&mut self, code: u16, reason: &'static str) -> Event {
        if self.phase == Phase::Open {
            self.queue_close(code, "");
        }
        self.phase = Phase::Failed;
        self.asm = None;
        self.pong = None;
        self.ping = None;
        Event::ProtocolError { code, reason }
    }

    fn queue_close(&mut self, code: u16, reason: &str) {
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(reason.as_bytes());
        self.close = Some(payload);
        self.pong = None;
        self.ping = None;
    }

    fn on_close(&mut self, payload: &[u8]) -> Event {
        let (code, reason) = match parse_close(payload) {
            Ok(parsed) => parsed,
            Err(v) => return self.fail(v.code, v.reason),
        };
        if self.phase == Phase::Open {
            // Echo the code the peer sent (or an empty close), as RFC 6455 section 5.5.1 asks.
            self.close = Some(code.map(|c| c.to_be_bytes().to_vec()).unwrap_or_default());
            self.pong = None;
            self.ping = None;
        }
        self.phase = Phase::Closed;
        self.asm = None;
        Event::Closed { code, reason }
    }

    fn on_data(&mut self, frame: Frame) -> Option<Event> {
        if self.phase != Phase::Open {
            // We already sent our close; data that was in flight is dropped.
            return None;
        }
        let mut asm = match (frame.opcode, self.asm.take()) {
            (Opcode::Continuation, Some(a)) => a,
            (Opcode::Continuation, None) => {
                return Some(self.fail(
                    close_code::PROTOCOL,
                    "continuation frame without a started message",
                ));
            }
            (_, Some(_)) => {
                return Some(self.fail(
                    close_code::PROTOCOL,
                    "new data frame inside a fragmented message",
                ));
            }
            (op, None) => {
                let text = op == Opcode::Text;
                if text && !self.cfg.allow_text {
                    return Some(
                        self.fail(close_code::UNSUPPORTED, "text messages are not accepted"),
                    );
                }
                Assembly {
                    text,
                    buf: Vec::new(),
                    valid: 0,
                }
            }
        };
        if asm.buf.len().saturating_add(frame.payload.len()) > self.cfg.max_message {
            return Some(self.fail(close_code::TOO_BIG, "message above the limit"));
        }
        asm.buf.extend_from_slice(&frame.payload);
        let bad_utf8 = "text message is not valid UTF-8";
        if asm.text {
            match extend_utf8(&asm.buf, asm.valid) {
                Ok(v) => asm.valid = v,
                Err(()) => return Some(self.fail(close_code::INVALID_PAYLOAD, bad_utf8)),
            }
        }
        if !frame.fin {
            self.asm = Some(asm);
            return None;
        }
        if asm.text {
            return Some(match String::from_utf8(asm.buf) {
                Ok(s) => Event::Message(Message::Text(s)),
                Err(_) => self.fail(close_code::INVALID_PAYLOAD, bad_utf8),
            });
        }
        Some(Event::Message(Message::Binary(asm.buf)))
    }

    /// Run the timers at logical time `now`. A quiet open connection gets a ping; an unanswered ping
    /// or close handshake ends the connection with [`Event::TimedOut`].
    pub fn tick(&mut self, now: u64) -> Option<Event> {
        match self.phase {
            Phase::Open => {
                if let Some(deadline) = self.ping_deadline {
                    if now >= deadline {
                        self.queue_close(close_code::GOING_AWAY, "");
                        self.phase = Phase::Failed;
                        self.asm = None;
                        return Some(Event::TimedOut);
                    }
                } else if self.cfg.ping_interval > 0
                    && now.saturating_sub(self.last_rx) >= self.cfg.ping_interval
                {
                    self.pings_sent = self.pings_sent.wrapping_add(1);
                    self.ping = Some(self.pings_sent.to_be_bytes());
                    self.ping_deadline = Some(now.saturating_add(self.cfg.pong_timeout));
                }
                None
            }
            Phase::CloseSent => {
                if self.close_deadline.is_some_and(|d| now >= d) {
                    self.phase = Phase::Failed;
                    return Some(Event::TimedOut);
                }
                None
            }
            Phase::Closed | Phase::Failed => None,
        }
    }

    /// The next logical time at which [`Connection::tick`] has work, if any.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        match self.phase {
            Phase::Open => self.ping_deadline.or_else(|| {
                (self.cfg.ping_interval > 0)
                    .then(|| self.last_rx.saturating_add(self.cfg.ping_interval))
            }),
            Phase::CloseSent => self.close_deadline,
            Phase::Closed | Phase::Failed => None,
        }
    }

    /// Append the queued control frames (pong, ping, close) to `out` and clear the queue.
    ///
    /// The order is pong, ping, close. Data frames written by `send_text` and `send_binary` go
    /// straight to the caller's buffer, so call this after writing them if a close must follow the
    /// data. The ping deadline starts when `tick` queues the ping, not when it is written.
    pub fn drain_output(&mut self, out: &mut Vec<u8>) {
        let control = |out: &mut Vec<u8>, op: Opcode, payload: &[u8]| {
            let payload = payload.get(..MAX_CONTROL_PAYLOAD).unwrap_or(payload);
            push_header(out, true, op, payload.len(), false);
            out.extend_from_slice(payload);
        };
        if let Some(p) = self.pong.take() {
            control(out, Opcode::Pong, &p);
        }
        if let Some(p) = self.ping.take() {
            control(out, Opcode::Ping, &p);
        }
        if let Some(p) = self.close.take() {
            control(out, Opcode::Close, &p);
        }
    }

    /// Append a text message as one unmasked frame.
    ///
    /// # Errors
    ///
    /// [`SendError::NotOpen`] once a close was sent or received, [`SendError::TooLarge`] above
    /// [`Config::max_message`].
    pub fn send_text(&mut self, text: &str, out: &mut Vec<u8>) -> Result<(), SendError> {
        self.send(Opcode::Text, text.as_bytes(), out)
    }

    /// Append a binary message as one unmasked frame.
    ///
    /// # Errors
    ///
    /// As for [`Connection::send_text`].
    pub fn send_binary(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), SendError> {
        self.send(Opcode::Binary, data, out)
    }

    fn send(&mut self, op: Opcode, payload: &[u8], out: &mut Vec<u8>) -> Result<(), SendError> {
        if self.phase != Phase::Open {
            return Err(SendError::NotOpen);
        }
        if payload.len() > self.cfg.max_message {
            return Err(SendError::TooLarge);
        }
        encode_frame(true, op, payload, out).map_err(|_| SendError::TooLarge)
    }

    /// Start the close handshake: queue a close frame (see [`Connection::drain_output`]) and wait up
    /// to [`Config::close_timeout`] for the peer's close.
    ///
    /// # Errors
    ///
    /// [`SendError::NotOpen`] if a close is already under way, [`SendError::BadClose`] for a code
    /// that [`is_valid_close_code`] refuses or a reason above [`MAX_CLOSE_REASON`] bytes.
    pub fn close(&mut self, code: u16, reason: &str, now: u64) -> Result<(), SendError> {
        if self.phase != Phase::Open {
            return Err(SendError::NotOpen);
        }
        if !is_valid_close_code(code) || reason.len() > MAX_CLOSE_REASON {
            return Err(SendError::BadClose);
        }
        self.queue_close(code, reason);
        self.phase = Phase::CloseSent;
        self.close_deadline = Some(now.saturating_add(self.cfg.close_timeout));
        self.asm = None;
        Ok(())
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

    const KEY: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    /// A masked client frame built by hand, independent of `encode_client_frame`.
    fn masked(first: u8, payload: &[u8], key: [u8; 4]) -> Vec<u8> {
        let mut v = vec![first];
        match payload.len() {
            n if n < 126 => v.push(0x80 | n as u8),
            n if n < 65536 => {
                v.push(0x80 | 126);
                v.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                v.push(0x80 | 127);
                v.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        v.extend_from_slice(&key);
        for (i, b) in payload.iter().enumerate() {
            v.push(b ^ key[i % 4]);
        }
        v
    }

    fn drive(conn: &mut Connection, mut input: &[u8], now: u64) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let step = conn.receive(input, now);
            input = &input[step.consumed..];
            let idle = step.consumed == 0 && step.event.is_none();
            events.extend(step.event);
            if idle {
                return events;
            }
        }
    }

    fn open() -> Connection {
        Connection::new(Config::default(), 0)
    }

    fn output(conn: &mut Connection) -> Vec<u8> {
        let mut out = Vec::new();
        conn.drain_output(&mut out);
        out
    }

    fn protocol_error(events: &[Event]) -> u16 {
        match events {
            [Event::ProtocolError { code, .. }] => *code,
            other => panic!("expected one protocol error, got {other:?}"),
        }
    }

    // ---- RFC 6455 section 5.7 examples -------------------------------------------------------

    #[test]
    fn rfc_unmasked_hello_decodes_and_encodes() {
        let bytes = [0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        let (f, used) = decode_server_frame(&bytes, 1024).unwrap().unwrap();
        assert_eq!(
            (f.fin, f.opcode, f.payload.as_slice(), used),
            (true, Opcode::Text, &b"Hello"[..], 7)
        );
        let mut out = Vec::new();
        encode_frame(true, Opcode::Text, b"Hello", &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn rfc_masked_hello_decodes_and_encodes() {
        let bytes = [
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        let (f, used) = decode_client_frame(&bytes, 1024).unwrap().unwrap();
        assert_eq!(
            (f.fin, f.opcode, f.payload.as_slice(), used),
            (true, Opcode::Text, &b"Hello"[..], 11)
        );
        let mut out = Vec::new();
        encode_client_frame(true, Opcode::Text, b"Hello", KEY, &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn rfc_fragmented_unmasked_message() {
        let first = [0x01, 0x03, 0x48, 0x65, 0x6c];
        let last = [0x80, 0x02, 0x6c, 0x6f];
        let (a, _) = decode_server_frame(&first, 1024).unwrap().unwrap();
        let (b, _) = decode_server_frame(&last, 1024).unwrap().unwrap();
        assert_eq!((a.fin, a.opcode), (false, Opcode::Text));
        assert_eq!((b.fin, b.opcode), (true, Opcode::Continuation));
        let mut out = Vec::new();
        encode_frame(false, Opcode::Text, &[0x48, 0x65, 0x6c], &mut out).unwrap();
        encode_frame(true, Opcode::Continuation, b"lo", &mut out).unwrap();
        assert_eq!(out, [first.as_slice(), last.as_slice()].concat());
    }

    #[test]
    fn rfc_ping_and_masked_pong() {
        let ping = [0x89, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        let (f, _) = decode_server_frame(&ping, 1024).unwrap().unwrap();
        assert_eq!(
            (f.opcode, f.payload.as_slice()),
            (Opcode::Ping, &b"Hello"[..])
        );
        let pong = [
            0x8a, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        let (f, _) = decode_client_frame(&pong, 1024).unwrap().unwrap();
        assert_eq!(
            (f.opcode, f.payload.as_slice()),
            (Opcode::Pong, &b"Hello"[..])
        );
    }

    #[test]
    fn rfc_256_byte_binary_message() {
        let mut bytes = vec![0x82, 0x7e, 0x01, 0x00];
        bytes.extend(std::iter::repeat_n(0xab, 256));
        let (f, used) = decode_server_frame(&bytes, 1 << 20).unwrap().unwrap();
        assert_eq!(
            (f.opcode, f.payload.len(), used),
            (Opcode::Binary, 256, 260)
        );
        let mut out = Vec::new();
        encode_frame(true, Opcode::Binary, &[0xab; 256], &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn rfc_64_kib_binary_message() {
        let mut bytes = vec![0x82, 0x7f, 0, 0, 0, 0, 0, 0x01, 0x00, 0x00];
        bytes.extend(std::iter::repeat_n(0xcd, 65536));
        let (f, used) = decode_server_frame(&bytes, 1 << 20).unwrap().unwrap();
        assert_eq!(
            (f.opcode, f.payload.len(), used),
            (Opcode::Binary, 65536, 65546)
        );
        let mut out = Vec::new();
        encode_frame(true, Opcode::Binary, &vec![0xcd; 65536], &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn length_boundaries_use_the_shortest_form() {
        for (len, header) in [
            (0usize, vec![0x82, 0]),
            (125, vec![0x82, 125]),
            (126, vec![0x82, 126, 0, 126]),
            (65535, vec![0x82, 126, 0xff, 0xff]),
            (65536, vec![0x82, 127, 0, 0, 0, 0, 0, 1, 0, 0]),
        ] {
            let mut out = Vec::new();
            encode_frame(true, Opcode::Binary, &vec![7; len], &mut out).unwrap();
            assert_eq!(out[..header.len()], header[..], "length {len}");
            assert_eq!(out.len(), header.len() + len);
        }
    }

    // ---- frame decoding errors ---------------------------------------------------------------

    #[test]
    fn unmasked_client_frame_is_a_protocol_error() {
        let v = decode_client_frame(&[0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f], 1024).unwrap_err();
        assert_eq!(v.code, 1002);
        // As soon as the second byte is there, before any payload.
        assert_eq!(
            decode_client_frame(&[0x81, 0x05], 1024).unwrap_err().code,
            1002
        );
        // A masked frame from the server side is wrong too.
        assert_eq!(
            decode_server_frame(&masked(0x81, b"x", KEY), 1024)
                .unwrap_err()
                .code,
            1002
        );
    }

    #[test]
    fn reserved_bits_and_opcodes_are_refused() {
        for bit in [0x40, 0x20, 0x10] {
            assert_eq!(
                decode_client_frame(&masked(0x81 | bit, b"x", KEY), 64)
                    .unwrap_err()
                    .code,
                1002
            );
        }
        for op in (3..=7).chain(11..=15) {
            assert_eq!(
                decode_client_frame(&masked(0x80 | op, b"", KEY), 64)
                    .unwrap_err()
                    .code,
                1002,
                "op {op}"
            );
        }
        // Judged from the first byte alone.
        assert_eq!(decode_client_frame(&[0x83], 64).unwrap_err().code, 1002);
    }

    #[test]
    fn control_frames_are_short_and_final() {
        assert_eq!(
            decode_client_frame(&masked(0x09, b"", KEY), 64)
                .unwrap_err()
                .code,
            1002
        );
        assert_eq!(
            decode_client_frame(&masked(0x08, b"", KEY), 64)
                .unwrap_err()
                .code,
            1002
        );
        let long = masked(0x89, &[0; 126], KEY);
        assert_eq!(decode_client_frame(&long, 1 << 20).unwrap_err().code, 1002);
        // 125 bytes is the largest ping.
        assert!(
            decode_client_frame(&masked(0x89, &[0; 125], KEY), 1 << 20)
                .unwrap()
                .is_some()
        );
        let mut out = Vec::new();
        assert_eq!(
            encode_frame(true, Opcode::Ping, &[0; 126], &mut out),
            Err(EncodeError::BadControlFrame)
        );
        assert_eq!(
            encode_frame(false, Opcode::Close, b"", &mut out),
            Err(EncodeError::BadControlFrame)
        );
        assert!(out.is_empty());
    }

    #[test]
    fn lengths_must_be_minimal_and_in_range() {
        // 125 sent in the 16-bit form, 65535 in the 64-bit form.
        let mut f = vec![0x82, 0x80 | 126, 0, 125];
        f.extend_from_slice(&KEY);
        f.extend_from_slice(&[0; 125]);
        assert_eq!(decode_client_frame(&f, 1 << 20).unwrap_err().code, 1002);
        let mut f = vec![0x82, 0x80 | 127, 0, 0, 0, 0, 0, 0, 0xff, 0xff];
        f.extend_from_slice(&KEY);
        assert_eq!(decode_client_frame(&f, 1 << 20).unwrap_err().code, 1002);
        // Top bit of the 64-bit length set.
        let mut f = vec![0x82, 0x80 | 127, 0x80, 0, 0, 0, 0, 0, 0, 0];
        f.extend_from_slice(&KEY);
        assert_eq!(decode_client_frame(&f, usize::MAX).unwrap_err().code, 1002);
    }

    #[test]
    fn oversize_frame_is_refused_from_the_header() {
        // Header of a 65536-byte frame with a 1000-byte cap, no payload bytes at all.
        let header = [0x82, 0x80 | 127, 0, 0, 0, 0, 0, 1, 0, 0, 1, 2, 3, 4];
        assert_eq!(decode_client_frame(&header, 1000).unwrap_err().code, 1009);
        // A huge declared length is refused by the cap, and does not overflow even without one.
        let huge = [
            0x82,
            0x80 | 127,
            0x7f,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            1,
            2,
            3,
            4,
        ];
        assert_eq!(decode_client_frame(&huge, 1 << 20).unwrap_err().code, 1009);
        assert_eq!(decode_client_frame(&huge, usize::MAX), Ok(None));
        // Exactly at the cap is fine.
        assert!(
            decode_client_frame(&masked(0x82, &[1; 1000], KEY), 1000)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            decode_client_frame(&masked(0x82, &[1; 1001], KEY), 1000)
                .unwrap_err()
                .code,
            1009
        );
    }

    #[test]
    fn incomplete_frames_wait() {
        let whole = masked(0x82, &[9; 300], KEY);
        for cut in 0..whole.len() {
            assert_eq!(
                decode_client_frame(&whole[..cut], 1 << 20),
                Ok(None),
                "cut {cut}"
            );
        }
        assert!(decode_client_frame(&whole, 1 << 20).unwrap().is_some());
    }

    // ---- connection: messages ----------------------------------------------------------------

    #[test]
    fn text_and_binary_messages_are_delivered() {
        let mut c = open();
        let mut input = masked(0x81, b"Hello", KEY);
        input.extend(masked(0x82, &[1, 2, 3], [9, 8, 7, 6]));
        let events = drive(&mut c, &input, 1);
        assert_eq!(
            events,
            [
                Event::Message(Message::Text("Hello".into())),
                Event::Message(Message::Binary(vec![1, 2, 3]))
            ]
        );
        assert!(output(&mut c).is_empty());
    }

    #[test]
    fn fragmented_message_with_interleaved_ping() {
        let mut c = open();
        let mut input = masked(0x01, &[0x48, 0x65, 0x6c], KEY);
        input.extend(masked(0x89, b"mid", KEY));
        input.extend(masked(0x80, b"lo", KEY));
        let events = drive(&mut c, &input, 1);
        assert_eq!(events, [Event::Message(Message::Text("Hello".into()))]);
        // The pong went out in between, unmasked, with the ping's payload.
        assert_eq!(output(&mut c), [0x8a, 0x03, b'm', b'i', b'd']);
    }

    #[test]
    fn multibyte_character_may_span_fragments() {
        let euro = "€".as_bytes(); // e2 82 ac
        let mut c = open();
        let mut input = masked(0x01, &euro[..1], KEY);
        input.extend(masked(0x00, &euro[1..2], KEY));
        input.extend(masked(0x80, &euro[2..], KEY));
        assert_eq!(
            drive(&mut c, &input, 1),
            [Event::Message(Message::Text("€".into()))]
        );
    }

    #[test]
    fn invalid_utf8_closes_with_1007() {
        let mut c = open();
        assert_eq!(
            protocol_error(&drive(&mut c, &masked(0x81, &[0xff], KEY), 1)),
            1007
        );
        assert_eq!(output(&mut c), [0x88, 0x02, 0x03, 0xef]);
        assert!(c.is_done());
        // Overlong encoding, a surrogate and a truncated character at FIN.
        for bad in [&[0xc0u8, 0x80][..], &[0xed, 0xa0, 0x80], &[0xe2, 0x82]] {
            let mut c = open();
            assert_eq!(
                protocol_error(&drive(&mut c, &masked(0x81, bad, KEY), 1)),
                1007,
                "{bad:x?}"
            );
        }
    }

    #[test]
    fn invalid_utf8_in_a_fragment_fails_before_fin() {
        let mut c = open();
        let mut input = masked(0x01, b"ok", KEY);
        input.extend(masked(0x00, &[0xc0], KEY));
        assert_eq!(protocol_error(&drive(&mut c, &input, 1)), 1007);
    }

    #[test]
    fn binary_messages_are_not_validated() {
        let mut c = open();
        let events = drive(&mut c, &masked(0x82, &[0xff, 0xfe], KEY), 1);
        assert_eq!(events, [Event::Message(Message::Binary(vec![0xff, 0xfe]))]);
    }

    #[test]
    fn text_can_be_refused() {
        let mut c = Connection::new(
            Config {
                allow_text: false,
                ..Config::default()
            },
            0,
        );
        assert_eq!(
            protocol_error(&drive(&mut c, &masked(0x81, b"hi", KEY), 1)),
            1003
        );
        let mut c = Connection::new(
            Config {
                allow_text: false,
                ..Config::default()
            },
            0,
        );
        assert_eq!(drive(&mut c, &masked(0x82, b"hi", KEY), 1).len(), 1);
    }

    #[test]
    fn message_cap_applies_across_fragments() {
        let cfg = Config {
            max_message: 10,
            max_frame_payload: 8,
            ..Config::default()
        };
        let mut c = Connection::new(cfg.clone(), 0);
        let mut input = masked(0x02, &[0; 6], KEY);
        input.extend(masked(0x00, &[0; 4], KEY));
        input.extend(masked(0x80, &[0; 1], KEY));
        let events = drive(&mut c, &input, 1);
        assert_eq!(protocol_error(&events), 1009);
        // Exactly at the cap is delivered.
        let mut c = Connection::new(cfg, 0);
        let mut input = masked(0x02, &[0; 6], KEY);
        input.extend(masked(0x80, &[0; 4], KEY));
        assert_eq!(
            drive(&mut c, &input, 1),
            [Event::Message(Message::Binary(vec![0; 10]))]
        );
    }

    #[test]
    fn frame_cap_is_checked_from_the_header() {
        let cfg = Config {
            max_frame_payload: 100,
            ..Config::default()
        };
        let mut c = Connection::new(cfg, 0);
        let header = [0x82, 0x80 | 126, 0x10, 0x00, 1, 2, 3, 4];
        assert_eq!(protocol_error(&drive(&mut c, &header, 1)), 1009);
        assert_eq!(output(&mut c), [0x88, 0x02, 0x03, 0xf1]);
    }

    #[test]
    fn broken_fragmentation_is_a_protocol_error() {
        // Continuation without a start.
        let mut c = open();
        assert_eq!(
            protocol_error(&drive(&mut c, &masked(0x80, b"x", KEY), 1)),
            1002
        );
        // A new data frame while a message is open.
        let mut c = open();
        let mut input = masked(0x01, b"a", KEY);
        input.extend(masked(0x81, b"b", KEY));
        assert_eq!(protocol_error(&drive(&mut c, &input, 1)), 1002);
        // A fragmented control frame.
        let mut c = open();
        assert_eq!(
            protocol_error(&drive(&mut c, &masked(0x09, b"", KEY), 1)),
            1002
        );
    }

    #[test]
    fn protocol_error_is_terminal_and_ignores_later_input() {
        let mut c = open();
        let mut input = masked(0x83, b"", KEY);
        input.extend(masked(0x81, b"late", KEY));
        let events = drive(&mut c, &input, 1);
        assert_eq!(protocol_error(&events), 1002);
        assert!(c.is_done());
        assert_eq!(drive(&mut c, &masked(0x81, b"late", KEY), 2), []);
        assert_eq!(output(&mut c), [0x88, 0x02, 0x03, 0xea]);
        assert!(output(&mut c).is_empty());
        let mut out = Vec::new();
        assert_eq!(c.send_text("x", &mut out), Err(SendError::NotOpen));
    }

    // ---- connection: control frames ----------------------------------------------------------

    #[test]
    fn ping_is_answered_with_a_pong_and_a_newer_ping_replaces_it() {
        let mut c = open();
        assert_eq!(drive(&mut c, &masked(0x89, b"Hello", KEY), 1), []);
        assert_eq!(output(&mut c), [0x8a, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f]);
        let mut input = masked(0x89, b"one", KEY);
        input.extend(masked(0x89, b"two", KEY));
        input.extend(masked(0x8a, b"unsolicited", KEY));
        drive(&mut c, &input, 2);
        assert_eq!(output(&mut c), [0x8a, 0x03, b't', b'w', b'o']);
    }

    #[test]
    fn close_is_echoed() {
        let mut c = open();
        let mut payload = 1000u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        let events = drive(&mut c, &masked(0x88, &payload, KEY), 1);
        assert_eq!(
            events,
            [Event::Closed {
                code: Some(1000),
                reason: "bye".into()
            }]
        );
        assert_eq!(output(&mut c), [0x88, 0x02, 0x03, 0xe8]);
        assert!(c.is_done());
        // An empty close is answered with an empty close.
        let mut c = open();
        assert_eq!(
            drive(&mut c, &masked(0x88, b"", KEY), 1),
            [Event::Closed {
                code: None,
                reason: String::new()
            }]
        );
        assert_eq!(output(&mut c), [0x88, 0x00]);
    }

    #[test]
    fn close_payload_rules() {
        // One byte.
        let mut c = open();
        assert_eq!(
            protocol_error(&drive(&mut c, &masked(0x88, &[0x03], KEY), 1)),
            1002
        );
        // Codes that must not appear on the wire.
        for code in [0u16, 999, 1004, 1005, 1006, 1015, 1016, 2999, 5000] {
            let mut c = open();
            let events = drive(&mut c, &masked(0x88, &code.to_be_bytes(), KEY), 1);
            assert_eq!(protocol_error(&events), 1002, "code {code}");
        }
        for code in [
            1000u16, 1001, 1002, 1003, 1007, 1008, 1009, 1010, 1011, 1012, 1013, 1014, 3000, 4999,
        ] {
            let mut c = open();
            let events = drive(&mut c, &masked(0x88, &code.to_be_bytes(), KEY), 1);
            assert!(matches!(events[..], [Event::Closed { .. }]), "code {code}");
        }
        // A reason that is not UTF-8.
        let mut c = open();
        let events = drive(&mut c, &masked(0x88, &[0x03, 0xe8, 0xff], KEY), 1);
        assert_eq!(protocol_error(&events), 1007);
    }

    #[test]
    fn our_close_waits_for_the_peer() {
        let mut c = open();
        c.close(1000, "done", 100).unwrap();
        assert!(!c.is_open() && !c.is_done());
        let mut want = vec![0x88, 0x06, 0x03, 0xe8];
        want.extend_from_slice(b"done");
        assert_eq!(output(&mut c), want);
        assert_eq!(c.close(1000, "", 100), Err(SendError::NotOpen));
        // Data in flight is dropped, then the peer's close completes the handshake.
        let mut input = masked(0x81, b"late", KEY);
        input.extend(masked(0x88, &1000u16.to_be_bytes(), KEY));
        assert_eq!(
            drive(&mut c, &input, 101),
            [Event::Closed {
                code: Some(1000),
                reason: String::new()
            }]
        );
        assert!(c.is_done());
        assert!(output(&mut c).is_empty());
    }

    #[test]
    fn close_arguments_are_checked() {
        let mut c = open();
        assert_eq!(c.close(1005, "", 0), Err(SendError::BadClose));
        assert_eq!(c.close(1000, &"x".repeat(124), 0), Err(SendError::BadClose));
        assert!(c.is_open());
        c.close(1011, &"x".repeat(123), 0).unwrap();
        assert_eq!(output(&mut c).len(), 2 + 125);
    }

    #[test]
    fn sending_is_bounded_and_unmasked() {
        let cfg = Config {
            max_message: 4,
            ..Config::default()
        };
        let mut c = Connection::new(cfg, 0);
        let mut out = Vec::new();
        c.send_binary(&[1, 2, 3, 4], &mut out).unwrap();
        c.send_text("hi", &mut out).unwrap();
        assert_eq!(out, [0x82, 4, 1, 2, 3, 4, 0x81, 2, b'h', b'i']);
        assert_eq!(c.send_binary(&[0; 5], &mut out), Err(SendError::TooLarge));
        assert_eq!(out.len(), 10);
    }

    // ---- connection: timers ------------------------------------------------------------------

    #[test]
    fn quiet_connection_is_pinged_then_timed_out() {
        let cfg = Config {
            ping_interval: 100,
            pong_timeout: 50,
            ..Config::default()
        };
        let mut c = Connection::new(cfg, 1000);
        assert_eq!(c.next_deadline(), Some(1100));
        assert_eq!(c.tick(1099), None);
        assert!(output(&mut c).is_empty());
        assert_eq!(c.tick(1100), None);
        assert_eq!(output(&mut c), [0x89, 0x08, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(c.next_deadline(), Some(1150));
        // No second ping while one is outstanding.
        assert_eq!(c.tick(1149), None);
        assert!(output(&mut c).is_empty());
        assert_eq!(c.tick(1150), Some(Event::TimedOut));
        assert!(c.is_done());
        assert_eq!(output(&mut c), [0x88, 0x02, 0x03, 0xe9]);
    }

    #[test]
    fn any_frame_answers_the_ping() {
        let cfg = Config {
            ping_interval: 100,
            pong_timeout: 50,
            ..Config::default()
        };
        let mut c = Connection::new(cfg, 0);
        c.tick(100);
        output(&mut c);
        drive(&mut c, &masked(0x8a, &[0, 0, 0, 0, 0, 0, 0, 1], KEY), 120);
        assert_eq!(c.tick(150), None);
        assert!(c.is_open());
        // The quiet period restarts from the last frame.
        assert_eq!(c.next_deadline(), Some(220));
        c.tick(220);
        assert_eq!(output(&mut c), [0x89, 0x08, 0, 0, 0, 0, 0, 0, 0, 2]);
    }

    #[test]
    fn pings_can_be_disabled() {
        let cfg = Config {
            ping_interval: 0,
            ..Config::default()
        };
        let mut c = Connection::new(cfg, 0);
        assert_eq!(c.next_deadline(), None);
        assert_eq!(c.tick(u64::MAX), None);
        assert!(output(&mut c).is_empty());
    }

    #[test]
    fn unanswered_close_times_out() {
        let cfg = Config {
            close_timeout: 30,
            ..Config::default()
        };
        let mut c = Connection::new(cfg, 0);
        c.close(1000, "", 10).unwrap();
        assert_eq!(c.next_deadline(), Some(40));
        assert_eq!(c.tick(39), None);
        assert_eq!(c.tick(40), Some(Event::TimedOut));
        assert!(c.is_done());
    }

    #[test]
    fn partial_frames_do_not_count_as_activity() {
        let cfg = Config {
            ping_interval: 100,
            pong_timeout: 50,
            ..Config::default()
        };
        let mut c = Connection::new(cfg, 0);
        assert_eq!(drive(&mut c, &masked(0x82, b"abc", KEY)[..5], 90), []);
        c.tick(100);
        assert!(!output(&mut c).is_empty());
    }

    #[test]
    fn byte_by_byte_delivery_equals_whole_delivery() {
        let mut stream = masked(0x01, &[0x48, 0x65, 0x6c], KEY);
        stream.extend(masked(0x89, b"p", [1, 2, 3, 4]));
        stream.extend(masked(0x80, b"lo", [5, 6, 7, 8]));
        stream.extend(masked(0x82, &[3; 300], [9, 9, 9, 9]));
        let whole = drive(&mut open(), &stream, 1);
        let mut c = open();
        let mut buf: Vec<u8> = Vec::new();
        let mut events = Vec::new();
        for b in &stream {
            buf.push(*b);
            loop {
                let step = c.receive(&buf, 1);
                buf.drain(..step.consumed);
                let idle = step.consumed == 0 && step.event.is_none();
                events.extend(step.event);
                if idle {
                    break;
                }
            }
        }
        assert_eq!(events, whole);
        assert_eq!(whole.len(), 2);
    }

    // ---- handshake ---------------------------------------------------------------------------

    const RFC_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

    fn request(extra: &str) -> String {
        format!(
            "GET /sync HTTP/1.1\r\nHost: db.example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {RFC_KEY}\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: lattice.sync.v1\r\n{extra}\r\n"
        )
    }

    fn parse(req: &str) -> Result<Option<(Upgrade, usize)>, HandshakeError> {
        parse_upgrade(req.as_bytes(), &UpgradeConfig::default())
    }

    fn status(req: &str) -> u16 {
        parse(req).unwrap_err().status
    }

    #[test]
    fn accept_key_matches_rfc_vector() {
        assert_eq!(accept_key(RFC_KEY), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn valid_upgrade_is_accepted() {
        let req = request("");
        let (up, used) = parse(&req).unwrap().unwrap();
        assert_eq!(used, req.len());
        assert_eq!(up.path, "/sync");
        assert_eq!(up.host, "db.example.com");
        assert_eq!(up.origin, None);
        assert_eq!(up.subprotocol, Some(SUBPROTOCOL));
        let resp = String::from_utf8(up.response()).unwrap();
        assert_eq!(
            resp,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
             Sec-WebSocket-Protocol: lattice.sync.v1\r\n\r\n"
        );
    }

    #[test]
    fn bytes_after_the_head_are_left_alone() {
        let mut req = request("").into_bytes();
        let head = req.len();
        req.extend_from_slice(&masked(0x82, b"x", KEY));
        let (_, used) = parse_upgrade(&req, &UpgradeConfig::default())
            .unwrap()
            .unwrap();
        assert_eq!(used, head);
    }

    #[test]
    fn header_names_and_tokens_are_case_insensitive_and_lists_work() {
        let req = "GET / HTTP/1.1\r\nhost: h\r\nUPGRADE: WebSocket\r\nconnection: keep-alive, Upgrade\r\n\
                   sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version:13\r\n\
                   Sec-WebSocket-Protocol: chat, lattice.sync.v1\r\n\r\n";
        assert!(parse(req).unwrap().is_some());
    }

    #[test]
    fn incomplete_head_waits_and_oversize_head_is_refused() {
        let req = request("");
        for cut in 0..req.len() {
            assert_eq!(
                parse_upgrade(&req.as_bytes()[..cut], &UpgradeConfig::default()),
                Ok(None),
                "cut {cut}"
            );
        }
        let cfg = UpgradeConfig {
            max_head: 64,
            ..UpgradeConfig::default()
        };
        assert_eq!(parse_upgrade(req.as_bytes(), &cfg).unwrap_err().status, 431);
        let endless = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(100));
        assert_eq!(
            parse_upgrade(endless.as_bytes(), &cfg).unwrap_err().status,
            431
        );
        // A head whose terminator lies beyond the cap is not accepted either.
        let cfg = UpgradeConfig {
            max_head: req.len() - 1,
            ..UpgradeConfig::default()
        };
        assert_eq!(parse_upgrade(req.as_bytes(), &cfg).unwrap_err().status, 431);
        let many: String = (0..70).map(|i| format!("X-{i}: v\r\n")).collect();
        assert_eq!(status(&request(&many)), 431);
    }

    #[test]
    fn method_and_version_are_checked_early() {
        assert_eq!(status(&request("").replace("GET", "POST")), 400);
        assert_eq!(status(&request("").replace("GET", "get")), 400);
        assert_eq!(status(&request("").replace("HTTP/1.1", "HTTP/1.0")), 400);
        assert_eq!(status(&request("").replace("/sync", "sync")), 400);
        assert_eq!(
            status(&request("").replace("/sync HTTP", "/sync  HTTP")),
            400
        );
        // "POST" is refused after one byte, without waiting for a whole head.
        assert_eq!(
            parse_upgrade(b"P", &UpgradeConfig::default())
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(parse_upgrade(b"GE", &UpgradeConfig::default()), Ok(None));
    }

    #[test]
    fn every_required_header_is_required() {
        for name in [
            "Host",
            "Upgrade",
            "Connection",
            "Sec-WebSocket-Key",
            "Sec-WebSocket-Version",
        ] {
            let req: String = request("")
                .split("\r\n")
                .filter(|l| !l.starts_with(&format!("{name}:")))
                .collect::<Vec<_>>()
                .join("\r\n");
            assert_eq!(status(&req), 400, "{name}");
        }
        assert_eq!(
            status(&request("").replace("Upgrade: websocket", "Upgrade: h2c")),
            400
        );
        assert_eq!(
            status(&request("").replace("Connection: Upgrade", "Connection: close")),
            400
        );
    }

    #[test]
    fn wrong_version_gets_426_with_the_supported_version() {
        let e = parse(&request("").replace("Version: 13", "Version: 8")).unwrap_err();
        assert_eq!(e.status, 426);
        let resp = String::from_utf8(e.response()).unwrap();
        assert!(resp.starts_with("HTTP/1.1 426 Upgrade Required\r\n"));
        assert!(resp.contains("Sec-WebSocket-Version: 13\r\n"));
        assert_eq!(
            status(&request("").replace("Version: 13", "Version: 13, 8")),
            426
        );
    }

    #[test]
    fn key_must_be_sixteen_bytes_of_canonical_base64() {
        for bad in [
            "",
            "dGhlIHNhbXBsZSBub25jZQ=",   // 23 chars
            "dGhlIHNhbXBsZSBub25jZQ===", // 25 chars
            "dGhlIHNhbXBsZSBub25jZR==",  // non-zero trailing bits
            "dGhlIHNhbXBsZSBub25j!Q==",  // not base64
            "dGhlIHNhbXBsZSBub25jZXIK",  // 18 bytes
            "AAAAAAAAAAAAAAAAAAAAAA==",  // valid, but see below
        ] {
            let req = request("").replace(RFC_KEY, bad);
            let ok = bad == "AAAAAAAAAAAAAAAAAAAAAA==";
            assert_eq!(parse(&req).is_ok(), ok, "{bad:?}");
        }
    }

    #[test]
    fn repeated_single_headers_and_bodies_are_refused() {
        assert_eq!(status(&request("Host: other\r\n")), 400);
        assert_eq!(
            status(&request(&format!("Sec-WebSocket-Key: {RFC_KEY}\r\n"))),
            400
        );
        assert_eq!(status(&request("Sec-WebSocket-Version: 13\r\n")), 400);
        assert_eq!(
            status(&request("Origin: https://a\r\nOrigin: https://b\r\n")),
            400
        );
        assert_eq!(status(&request("Content-Length: 5\r\n")), 400);
        assert_eq!(status(&request("Transfer-Encoding: chunked\r\n")), 400);
        assert!(parse(&request("Content-Length: 0\r\n")).unwrap().is_some());
    }

    #[test]
    fn malformed_head_syntax_is_refused() {
        assert_eq!(status(&request("X-Folded: a\r\n continued\r\n")), 400);
        assert_eq!(status(&request("No colon here\r\n")), 400);
        assert_eq!(status(&request("Bad Name: v\r\n")), 400);
        assert_eq!(status(&request(": empty\r\n")), 400);
        assert_eq!(status(&request("X: a\nb\r\n")), 400);
        assert_eq!(status(&request("X: a\0b\r\n")), 400);
        assert_eq!(status(&request("X: na\u{ef}ve\r\n")), 400);
        assert_eq!(status(&request("Host : db\r\n")), 400);
        assert_eq!(
            status(&request("").replace("db.example.com", "evil host")),
            400
        );
        assert!(
            parse_upgrade(
                b"GET / HTTP/1.1\r\nHost: \xff\r\n\r\n",
                &UpgradeConfig::default()
            )
            .is_err()
        );
    }

    #[test]
    fn subprotocol_match_is_case_sensitive() {
        let upper = request("").replace("lattice.sync.v1", "Lattice.Sync.V1");
        assert_eq!(status(&upper), 400);
    }

    #[test]
    fn masked_frame_of_exactly_126_bytes_uses_the_16_bit_form() {
        let f = masked(0x82, &[1; 126], KEY);
        assert_eq!(f[1], 0x80 | 126);
        let (frame, used) = decode_client_frame(&f, 1 << 20).unwrap().unwrap();
        assert_eq!((frame.payload.len(), used), (126, f.len()));
    }

    #[test]
    fn delivery_split_at_any_point_equals_whole_delivery() {
        let mut stream = masked(0x01, &[0x48, 0x65], KEY);
        stream.extend(masked(0x89, b"p", [1, 2, 3, 4]));
        stream.extend(masked(0x80, b"y", [5, 6, 7, 8]));
        stream.extend(masked(0x88, &1000u16.to_be_bytes(), KEY));
        let whole = drive(&mut open(), &stream, 1);
        assert_eq!(whole.len(), 2);
        for split in 0..=stream.len() {
            let mut c = open();
            let mut pending: Vec<u8> = Vec::new();
            let mut events = Vec::new();
            for chunk in [&stream[..split], &stream[split..]] {
                pending.extend_from_slice(chunk);
                loop {
                    let step = c.receive(&pending, 1);
                    pending.drain(..step.consumed);
                    let idle = step.consumed == 0 && step.event.is_none();
                    events.extend(step.event);
                    if idle {
                        break;
                    }
                }
            }
            assert_eq!(events, whole, "split {split}");
        }
    }

    #[test]
    fn subprotocol_is_negotiated() {
        let none = request("").replace("Sec-WebSocket-Protocol: lattice.sync.v1\r\n", "");
        assert_eq!(status(&none), 400);
        let other = request("").replace("lattice.sync.v1", "lattice.sync.v2");
        assert_eq!(status(&other), 400);
        let lax = UpgradeConfig {
            require_subprotocol: false,
            ..UpgradeConfig::default()
        };
        let (up, _) = parse_upgrade(none.as_bytes(), &lax).unwrap().unwrap();
        assert_eq!(up.subprotocol, None);
        assert!(
            !String::from_utf8(up.response())
                .unwrap()
                .contains("Sec-WebSocket-Protocol")
        );
        // Offered in a second header line.
        let split = request("Sec-WebSocket-Protocol: x\r\n");
        assert_eq!(
            parse(&split).unwrap().unwrap().0.subprotocol,
            Some(SUBPROTOCOL)
        );
    }

    #[test]
    fn origin_policy_and_hook() {
        // Default: no browsers.
        assert_eq!(status(&request("Origin: https://evil.example\r\n")), 403);
        assert!(parse(&request("")).unwrap().is_some());
        let any = UpgradeConfig {
            origins: OriginPolicy::Any,
            ..UpgradeConfig::default()
        };
        let (up, _) = parse_upgrade(request("Origin: https://x.example\r\n").as_bytes(), &any)
            .unwrap()
            .unwrap();
        assert_eq!(up.origin.as_deref(), Some("https://x.example"));
        let list = UpgradeConfig {
            origins: OriginPolicy::AllowList(vec!["https://app.example.com".into()]),
            ..UpgradeConfig::default()
        };
        assert!(
            parse_upgrade(
                request("Origin: HTTPS://APP.example.com\r\n").as_bytes(),
                &list
            )
            .is_ok()
        );
        assert!(parse_upgrade(request("").as_bytes(), &list).is_ok());
        let e = parse_upgrade(
            request("Origin: https://app.example.com.evil\r\n").as_bytes(),
            &list,
        )
        .unwrap_err();
        assert_eq!(e.status, 403);
        // The hook replaces the configured policy.
        let hook = |o: Option<&str>| o == Some("https://only.example");
        let ok = request("Origin: https://only.example\r\n");
        assert!(parse_upgrade_with(ok.as_bytes(), &list, &hook).is_ok());
        assert_eq!(
            parse_upgrade_with(request("").as_bytes(), &list, &hook)
                .unwrap_err()
                .status,
            403
        );
    }

    #[test]
    fn error_responses_are_complete_http() {
        for (status, phrase) in [
            (400, "Bad Request"),
            (403, "Forbidden"),
            (431, "Request Header Fields Too Large"),
        ] {
            let e = HandshakeError {
                status,
                reason: "why",
            };
            let resp = String::from_utf8(e.response()).unwrap();
            assert!(resp.starts_with(&format!("HTTP/1.1 {status} {phrase}\r\n")));
            assert!(resp.contains("Connection: close\r\n"));
            assert!(resp.ends_with("\r\n\r\nwhy\n"));
            assert!(resp.contains("Content-Length: 4\r\n"));
        }
    }

    // ---- sync frames over WebSocket ----------------------------------------------------------

    #[test]
    fn a_maximal_sync_frame_fits_the_default_limits() {
        let cfg = Config::default();
        assert_eq!(cfg.max_message, sync::HEADER_LEN + sync::MAX_PAYLOAD);
        assert!(cfg.max_frame_payload >= cfg.max_message);
        // The largest legal sync frame as one unfragmented message, as browsers send it.
        let data = vec![5u8; sync::HEADER_LEN + sync::MAX_PAYLOAD];
        let input = masked(0x82, &data, KEY);
        let mut c = Connection::new(cfg, 0);
        assert_eq!(
            drive(&mut c, &input, 1),
            [Event::Message(Message::Binary(data))]
        );
    }

    // ---- properties --------------------------------------------------------------------------

    fn payload() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            8 => proptest::collection::vec(any::<u8>(), 0..200),
            1 => prop_oneof![Just(125usize), Just(126), Just(127), Just(65535), Just(65536), Just(65537)]
                .prop_flat_map(|n| proptest::collection::vec(any::<u8>(), n..=n)),
        ]
    }

    fn opcode() -> impl Strategy<Value = Opcode> {
        prop_oneof![
            Just(Opcode::Continuation),
            Just(Opcode::Text),
            Just(Opcode::Binary),
            Just(Opcode::Close),
            Just(Opcode::Ping),
            Just(Opcode::Pong),
        ]
    }

    proptest! {
        #[test]
        fn server_encode_then_decode_round_trips(fin in any::<bool>(), op in opcode(), data in payload()) {
            let (fin, data) = if op.is_control() { (true, data.into_iter().take(125).collect()) } else { (fin, data) };
            let mut out = Vec::new();
            encode_frame(fin, op, &data, &mut out).unwrap();
            let (frame, used) = decode_server_frame(&out, 1 << 20).unwrap().unwrap();
            prop_assert_eq!(used, out.len());
            prop_assert_eq!(frame, Frame { fin, opcode: op, payload: data.clone() });
            // The server never masks: the same bytes are refused as a client frame.
            prop_assert_eq!(decode_client_frame(&out, 1 << 20).unwrap_err().code, 1002);
            // One byte short is incomplete, never an error.
            prop_assert_eq!(decode_server_frame(&out[..out.len() - 1], 1 << 20), Ok(None));
        }

        #[test]
        fn client_encode_then_decode_round_trips(
            fin in any::<bool>(), op in opcode(), data in payload(), key in any::<[u8; 4]>()
        ) {
            let (fin, data) = if op.is_control() { (true, data.into_iter().take(125).collect()) } else { (fin, data) };
            let mut out = Vec::new();
            encode_client_frame(fin, op, &data, key, &mut out).unwrap();
            let (frame, used) = decode_client_frame(&out, 1 << 20).unwrap().unwrap();
            prop_assert_eq!(used, out.len());
            prop_assert_eq!(frame, Frame { fin, opcode: op, payload: data });
            prop_assert_eq!(decode_server_frame(&out, 1 << 20).unwrap_err().code, 1002);
        }

        #[test]
        fn fragmented_text_reassembles(text in "\\PC{0,60}", cuts in proptest::collection::vec(0usize..64, 0..5), key in any::<[u8; 4]>()) {
            // Cut the UTF-8 bytes anywhere, including inside a character.
            let bytes = text.as_bytes();
            let mut points: Vec<usize> = cuts.into_iter().map(|c| c % (bytes.len() + 1)).collect();
            points.push(0);
            points.push(bytes.len());
            points.sort_unstable();
            let mut input = Vec::new();
            let pieces: Vec<&[u8]> = points.windows(2).map(|w| &bytes[w[0]..w[1]]).collect();
            for (i, piece) in pieces.iter().enumerate() {
                let op = if i == 0 { Opcode::Text } else { Opcode::Continuation };
                encode_client_frame(i + 1 == pieces.len(), op, piece, key, &mut input).unwrap();
            }
            let mut c = open();
            prop_assert_eq!(drive(&mut c, &input, 1), vec![Event::Message(Message::Text(text))]);
        }

        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..400)) {
            let _ = decode_client_frame(&data, 256);
            let _ = decode_server_frame(&data, 256);
            let _ = parse_upgrade(&data, &UpgradeConfig::default());
            let mut c = Connection::new(Config { max_frame_payload: 256, max_message: 300, ..Config::default() }, 0);
            let events = drive(&mut c, &data, 1);
            // Whatever happened, the queue is bounded and the connection can still be ticked.
            let out = output(&mut c);
            prop_assert!(out.len() <= 2 + 125 + 2 + 8 + 2 + 125);
            let _ = c.tick(1_000_000);
            prop_assert!(events.len() <= data.len());
        }

        #[test]
        fn arbitrary_masked_frames_never_panic(
            first in any::<u8>(), len in 0usize..300, key in any::<[u8; 4]>(), tail in proptest::collection::vec(any::<u8>(), 0..300)
        ) {
            let mut input = masked(first, &tail[..len.min(tail.len())], key);
            input.extend_from_slice(&tail);
            let mut c = open();
            let _ = drive(&mut c, &input, 1);
            let mut out = Vec::new();
            c.drain_output(&mut out);
        }

        #[test]
        fn mutated_requests_never_panic(pos in 0usize..300, byte in any::<u8>()) {
            let mut req = request("Origin: https://a.example\r\n").into_bytes();
            let at = pos % req.len();
            req[at] = byte;
            let cfg = UpgradeConfig { origins: OriginPolicy::Any, ..UpgradeConfig::default() };
            let _ = parse_upgrade(&req, &cfg);
        }
    }
}
