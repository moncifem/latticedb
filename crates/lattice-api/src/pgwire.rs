//! PostgreSQL wire protocol v3: the message codec.
//!
//! This is only the codec: bytes in, messages out, and the reverse. It does no I/O and holds no
//! session state, so it can be tested and fuzzed without a socket. Decoding is strict and bounded:
//! a message longer than [`MAX_MESSAGE_LEN`], a string without its terminator, an unknown tag or
//! bytes left over after the fields are errors, and incomplete input is reported as such so a caller
//! can read more. Nothing in here panics on any input.
//!
//! Scope: the startup phase (protocol 3.0, SSL and GSS encryption requests, cancel requests), the
//! simple query flow (`Query`, `Terminate`, `PasswordMessage`), the extended query flow (`Parse`,
//! `Bind`, `Describe`, `Execute`, `Close`, `Sync`, `Flush`) and the server messages that answer them.
//! `CopyData`, `FunctionCall` and the rest are refused with [`ProtoError::UnknownTag`].

use std::fmt;

/// The largest message, length field included, the codec will accept or produce.
pub const MAX_MESSAGE_LEN: usize = 1 << 20;

/// Protocol version 3.0 as sent in a startup message.
pub const PROTOCOL_3_0: i32 = 196_608;
const SSL_REQUEST: i32 = 80_877_103;
const GSS_ENC_REQUEST: i32 = 80_877_104;
const CANCEL_REQUEST: i32 = 80_877_102;

/// Why a message could not be decoded or encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// The length field is smaller than the field itself or larger than [`MAX_MESSAGE_LEN`].
    BadLength(i64),
    /// A message tag this codec does not handle yet.
    UnknownTag(u8),
    /// A startup code that is not protocol 3.0 or a known request.
    UnsupportedProtocol(i32),
    /// A string field is not valid UTF-8.
    NotUtf8,
    /// A string field has no terminator inside the message.
    Unterminated,
    /// The message ends before its fields do.
    Truncated,
    /// Bytes remain after the last field.
    Trailing,
    /// A value does not fit the wire format (a count above 32767, a field above 2 GiB).
    TooLarge,
    /// A field holds a value the protocol does not allow (a negative count, a format code other than 0
    /// or 1, a describe or close target other than `S` or `P`).
    InvalidField(&'static str),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::BadLength(n) => write!(f, "invalid message length {n}"),
            ProtoError::UnknownTag(t) => write!(f, "unsupported message type 0x{t:02x}"),
            ProtoError::UnsupportedProtocol(c) => write!(f, "unsupported protocol code {c}"),
            ProtoError::NotUtf8 => write!(f, "string is not valid UTF-8"),
            ProtoError::Unterminated => write!(f, "string is not terminated"),
            ProtoError::Truncated => write!(f, "message is truncated"),
            ProtoError::Trailing => write!(f, "unexpected bytes after the last field"),
            ProtoError::TooLarge => write!(f, "value does not fit the wire format"),
            ProtoError::InvalidField(what) => write!(f, "invalid {what}"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// A message a client sends before the session is established (no type byte).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// Protocol 3.0 with its parameters (`user`, `database`, ...) in the order sent.
    Params(Vec<(String, String)>),
    /// The client asks to switch to TLS; answer with one byte, `S` or `N`.
    SslRequest,
    /// The client asks for GSS encryption; answer `N` unless supported.
    GssEncRequest,
    /// Cancel the query running on the session with this process id and secret.
    Cancel {
        /// Backend process id from `BackendKeyData`.
        pid: i32,
        /// Secret key from `BackendKeyData`.
        secret: i32,
    },
}

/// A message a client sends once the session exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frontend {
    /// `Q`: a simple query.
    Query(String),
    /// `X`: close the session.
    Terminate,
    /// `p`: a password (or other authentication response) as bytes.
    Password(Vec<u8>),
    /// `P`: prepare a statement under a name (empty is the unnamed statement).
    Parse {
        /// Statement name.
        name: String,
        /// Query text.
        query: String,
        /// Parameter type object ids; 0 leaves the type to the server.
        param_types: Vec<i32>,
    },
    /// `B`: bind parameter values to a prepared statement, making a portal.
    Bind {
        /// Portal name (empty is the unnamed portal).
        portal: String,
        /// Statement name.
        statement: String,
        /// Format code per parameter: none (all text), one (for all) or one each; 0 text, 1 binary.
        param_formats: Vec<i16>,
        /// Parameter values; `None` is SQL NULL.
        params: Vec<Option<Vec<u8>>>,
        /// Format code per result column, with the same none, one or one-each rule.
        result_formats: Vec<i16>,
    },
    /// `D`: describe a statement or a portal.
    Describe(Target),
    /// `C`: close a statement or a portal.
    Close(Target),
    /// `E`: run a portal, returning at most `max_rows` rows (0 means all).
    Execute {
        /// Portal name.
        portal: String,
        /// Row limit; 0 is unlimited.
        max_rows: i32,
    },
    /// `S`: end an extended query cycle; the server answers `ReadyForQuery`.
    Sync,
    /// `H`: ask the server to send what it has pending.
    Flush,
}

/// What `Describe` and `Close` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A prepared statement (`S`).
    Statement(String),
    /// A portal (`P`).
    Portal(String),
}

/// The state the server reports in `ReadyForQuery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    /// Not in a transaction block.
    Idle,
    /// In a transaction block.
    InTransaction,
    /// In a failed transaction block; only `ROLLBACK` is accepted.
    Failed,
}

/// One column of a `RowDescription`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Object id of the source table, or 0.
    pub table_oid: i32,
    /// Attribute number in the source table, or 0.
    pub column_attr: i16,
    /// Object id of the data type.
    pub type_oid: i32,
    /// Type size in bytes, negative for variable length.
    pub type_size: i16,
    /// Type modifier, -1 when none.
    pub type_modifier: i32,
    /// 0 for text, 1 for binary.
    pub format: i16,
}

/// A message the server sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `R` 0: authentication succeeded.
    AuthenticationOk,
    /// `R` 3: the client must send a cleartext password.
    AuthenticationCleartextPassword,
    /// `S`: a run-time parameter the client should know (`server_version`, `client_encoding`, ...).
    ParameterStatus {
        /// Parameter name.
        name: String,
        /// Parameter value.
        value: String,
    },
    /// `K`: the process id and secret a client needs to cancel a query.
    BackendKeyData {
        /// Backend process id.
        pid: i32,
        /// Secret key.
        secret: i32,
    },
    /// `Z`: the server is ready for a new query.
    ReadyForQuery(TxStatus),
    /// `T`: the shape of the rows that follow.
    RowDescription(Vec<Column>),
    /// `D`: one row; `None` is SQL NULL.
    DataRow(Vec<Option<Vec<u8>>>),
    /// `C`: a statement finished, with its tag (`SELECT 3`, `INSERT 0 1`).
    CommandComplete(String),
    /// `I`: the query string was empty.
    EmptyQueryResponse,
    /// `1`: `Parse` succeeded.
    ParseComplete,
    /// `2`: `Bind` succeeded.
    BindComplete,
    /// `3`: `Close` succeeded.
    CloseComplete,
    /// `n`: the described statement or portal returns no rows.
    NoData,
    /// `t`: the parameter types of a described statement.
    ParameterDescription(Vec<i32>),
    /// `s`: `Execute` stopped at its row limit; run the portal again for more.
    PortalSuspended,
    /// `E`: an error, with a severity, a five character SQLSTATE and a message.
    ErrorResponse {
        /// `ERROR`, `FATAL` or `PANIC`.
        severity: String,
        /// SQLSTATE code.
        code: String,
        /// Human readable message.
        message: String,
    },
}

/// What the server answers to an SSL or GSS encryption request that it declines.
pub const ENCRYPTION_DECLINED: u8 = b'N';
/// What the server answers to an SSL request that it accepts.
pub const ENCRYPTION_ACCEPTED: u8 = b'S';

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        let end = self.at.checked_add(n).ok_or(ProtoError::Truncated)?;
        let s = self.bytes.get(self.at..end).ok_or(ProtoError::Truncated)?;
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, ProtoError> {
        self.take(1)?.first().copied().ok_or(ProtoError::Truncated)
    }

    fn i16(&mut self) -> Result<i16, ProtoError> {
        let b: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| ProtoError::Truncated)?;
        Ok(i16::from_be_bytes(b))
    }

    /// A non-negative `i16` count.
    fn count(&mut self, what: &'static str) -> Result<usize, ProtoError> {
        usize::try_from(self.i16()?).map_err(|_| ProtoError::InvalidField(what))
    }

    /// `n` format codes, each 0 (text) or 1 (binary).
    fn formats(&mut self, what: &'static str) -> Result<Vec<i16>, ProtoError> {
        let n = self.count(what)?;
        let mut out = Vec::new();
        for _ in 0..n {
            let code = self.i16()?;
            if code != 0 && code != 1 {
                return Err(ProtoError::InvalidField(what));
            }
            out.push(code);
        }
        Ok(out)
    }

    fn i32(&mut self) -> Result<i32, ProtoError> {
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ProtoError::Truncated)?;
        Ok(i32::from_be_bytes(b))
    }

    /// A zero terminated UTF-8 string.
    fn cstr(&mut self) -> Result<String, ProtoError> {
        let rest = self.bytes.get(self.at..).ok_or(ProtoError::Truncated)?;
        let len = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or(ProtoError::Unterminated)?;
        let text = std::str::from_utf8(rest.get(..len).ok_or(ProtoError::Truncated)?)
            .map_err(|_| ProtoError::NotUtf8)?
            .to_string();
        self.at += len + 1;
        Ok(text)
    }

    fn finished(&self) -> bool {
        self.at == self.bytes.len()
    }
}

fn read_len(buf: &[u8], at: usize) -> Option<i32> {
    let b: [u8; 4] = buf.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(i32::from_be_bytes(b))
}

fn checked_total(len: i32, min: usize) -> Result<usize, ProtoError> {
    let n = i64::from(len);
    if n < min as i64 || n > MAX_MESSAGE_LEN as i64 {
        return Err(ProtoError::BadLength(n));
    }
    usize::try_from(n).map_err(|_| ProtoError::BadLength(n))
}

/// Decodes one startup-phase message from the front of `buf`.
///
/// Returns `Ok(None)` if more bytes are needed, otherwise the message and how many bytes it used.
pub fn decode_startup(buf: &[u8]) -> Result<Option<(Startup, usize)>, ProtoError> {
    let Some(len) = read_len(buf, 0) else {
        return Ok(None);
    };
    let total = checked_total(len, 8)?;
    if buf.len() < total {
        return Ok(None);
    }
    let body = buf.get(4..total).ok_or(ProtoError::Truncated)?;
    let mut r = Reader { bytes: body, at: 0 };
    let code = r.i32()?;
    let msg = match code {
        PROTOCOL_3_0 => {
            let mut params = Vec::new();
            loop {
                let key = r.cstr()?;
                if key.is_empty() {
                    break;
                }
                let value = r.cstr()?;
                params.push((key, value));
            }
            Startup::Params(params)
        }
        SSL_REQUEST => Startup::SslRequest,
        GSS_ENC_REQUEST => Startup::GssEncRequest,
        CANCEL_REQUEST => Startup::Cancel {
            pid: r.i32()?,
            secret: r.i32()?,
        },
        other => return Err(ProtoError::UnsupportedProtocol(other)),
    };
    if !r.finished() {
        return Err(ProtoError::Trailing);
    }
    Ok(Some((msg, total)))
}

/// Decodes one client message (with a type byte) from the front of `buf`.
///
/// Returns `Ok(None)` if more bytes are needed, otherwise the message and how many bytes it used.
pub fn decode_frontend(buf: &[u8]) -> Result<Option<(Frontend, usize)>, ProtoError> {
    let Some(&tag) = buf.first() else {
        return Ok(None);
    };
    let Some(len) = read_len(buf, 1) else {
        return Ok(None);
    };
    let total = checked_total(len, 4)?;
    // The length counts itself but not the tag byte.
    let used = total
        .checked_add(1)
        .ok_or(ProtoError::BadLength(i64::from(len)))?;
    if buf.len() < used {
        return Ok(None);
    }
    let body = buf.get(5..used).ok_or(ProtoError::Truncated)?;
    let mut r = Reader { bytes: body, at: 0 };
    let msg = match tag {
        b'Q' => Frontend::Query(r.cstr()?),
        b'X' => Frontend::Terminate,
        b'p' => return Ok(Some((Frontend::Password(body.to_vec()), used))),
        b'P' => {
            let name = r.cstr()?;
            let query = r.cstr()?;
            let n = r.count("parameter count")?;
            let mut param_types = Vec::new();
            for _ in 0..n {
                param_types.push(r.i32()?);
            }
            Frontend::Parse {
                name,
                query,
                param_types,
            }
        }
        b'B' => {
            let portal = r.cstr()?;
            let statement = r.cstr()?;
            let param_formats = r.formats("parameter format")?;
            let n = r.count("parameter count")?;
            // The protocol allows no codes (all text), one for all, or exactly one each.
            if !matches!(param_formats.len(), 0 | 1) && param_formats.len() != n {
                return Err(ProtoError::InvalidField("parameter format count"));
            }
            let mut params = Vec::new();
            for _ in 0..n {
                let len = r.i32()?;
                if len == -1 {
                    params.push(None);
                } else {
                    let len = usize::try_from(len)
                        .map_err(|_| ProtoError::InvalidField("parameter length"))?;
                    params.push(Some(r.take(len)?.to_vec()));
                }
            }
            let result_formats = r.formats("result format")?;
            Frontend::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            }
        }
        b'D' | b'C' => {
            let kind = r.u8()?;
            let name = r.cstr()?;
            let target = match kind {
                b'S' => Target::Statement(name),
                b'P' => Target::Portal(name),
                _ => return Err(ProtoError::InvalidField("describe or close target")),
            };
            if tag == b'D' {
                Frontend::Describe(target)
            } else {
                Frontend::Close(target)
            }
        }
        b'E' => {
            let portal = r.cstr()?;
            let max_rows = r.i32()?;
            if max_rows < 0 {
                return Err(ProtoError::InvalidField("row limit"));
            }
            Frontend::Execute { portal, max_rows }
        }
        b'S' => Frontend::Sync,
        b'H' => Frontend::Flush,
        other => return Err(ProtoError::UnknownTag(other)),
    };
    if !r.finished() {
        return Err(ProtoError::Trailing);
    }
    Ok(Some((msg, used)))
}

fn put_cstr(out: &mut Vec<u8>, s: &str) -> Result<(), ProtoError> {
    if s.as_bytes().contains(&0) {
        return Err(ProtoError::NotUtf8);
    }
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    Ok(())
}

fn put_i16(out: &mut Vec<u8>, n: usize) -> Result<(), ProtoError> {
    let n = i16::try_from(n).map_err(|_| ProtoError::TooLarge)?;
    out.extend_from_slice(&n.to_be_bytes());
    Ok(())
}

impl Backend {
    /// Appends the wire form of this message to `out`. On error `out` is left as it was.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        let start = out.len();
        let result = self.encode_inner(out);
        if result.is_err() {
            out.truncate(start);
        }
        result
    }

    fn encode_inner(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        let start = out.len();
        let tag = match self {
            Backend::AuthenticationOk | Backend::AuthenticationCleartextPassword => b'R',
            Backend::ParameterStatus { .. } => b'S',
            Backend::BackendKeyData { .. } => b'K',
            Backend::ReadyForQuery(_) => b'Z',
            Backend::RowDescription(_) => b'T',
            Backend::DataRow(_) => b'D',
            Backend::CommandComplete(_) => b'C',
            Backend::EmptyQueryResponse => b'I',
            Backend::ParseComplete => b'1',
            Backend::BindComplete => b'2',
            Backend::CloseComplete => b'3',
            Backend::NoData => b'n',
            Backend::ParameterDescription(_) => b't',
            Backend::PortalSuspended => b's',
            Backend::ErrorResponse { .. } => b'E',
        };
        out.push(tag);
        out.extend_from_slice(&[0; 4]); // the length, filled in below
        match self {
            Backend::AuthenticationOk => out.extend_from_slice(&0i32.to_be_bytes()),
            Backend::AuthenticationCleartextPassword => out.extend_from_slice(&3i32.to_be_bytes()),
            Backend::ParameterStatus { name, value } => {
                put_cstr(out, name)?;
                put_cstr(out, value)?;
            }
            Backend::BackendKeyData { pid, secret } => {
                out.extend_from_slice(&pid.to_be_bytes());
                out.extend_from_slice(&secret.to_be_bytes());
            }
            Backend::ReadyForQuery(status) => out.push(match status {
                TxStatus::Idle => b'I',
                TxStatus::InTransaction => b'T',
                TxStatus::Failed => b'E',
            }),
            Backend::RowDescription(columns) => {
                put_i16(out, columns.len())?;
                for c in columns {
                    put_cstr(out, &c.name)?;
                    out.extend_from_slice(&c.table_oid.to_be_bytes());
                    out.extend_from_slice(&c.column_attr.to_be_bytes());
                    out.extend_from_slice(&c.type_oid.to_be_bytes());
                    out.extend_from_slice(&c.type_size.to_be_bytes());
                    out.extend_from_slice(&c.type_modifier.to_be_bytes());
                    out.extend_from_slice(&c.format.to_be_bytes());
                }
            }
            Backend::DataRow(values) => {
                put_i16(out, values.len())?;
                for v in values {
                    match v {
                        None => out.extend_from_slice(&(-1i32).to_be_bytes()),
                        Some(bytes) => {
                            let n = i32::try_from(bytes.len()).map_err(|_| ProtoError::TooLarge)?;
                            out.extend_from_slice(&n.to_be_bytes());
                            out.extend_from_slice(bytes);
                        }
                    }
                }
            }
            Backend::CommandComplete(tag) => put_cstr(out, tag)?,
            Backend::EmptyQueryResponse
            | Backend::ParseComplete
            | Backend::BindComplete
            | Backend::CloseComplete
            | Backend::NoData
            | Backend::PortalSuspended => {}
            Backend::ParameterDescription(oids) => {
                put_i16(out, oids.len())?;
                for oid in oids {
                    out.extend_from_slice(&oid.to_be_bytes());
                }
            }
            Backend::ErrorResponse {
                severity,
                code,
                message,
            } => {
                for (field, value) in [
                    (b'S', severity),
                    (b'V', severity),
                    (b'C', code),
                    (b'M', message),
                ] {
                    out.push(field);
                    put_cstr(out, value)?;
                }
                out.push(0);
            }
        }
        let len = out.len() - start - 1;
        if len > MAX_MESSAGE_LEN {
            return Err(ProtoError::TooLarge);
        }
        let len = u32::try_from(len).map_err(|_| ProtoError::TooLarge)?;
        out.get_mut(start + 1..start + 5)
            .ok_or(ProtoError::Truncated)?
            .copy_from_slice(&len.to_be_bytes());
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

    fn startup_bytes(params: &[(&str, &str)]) -> Vec<u8> {
        let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
        for (k, v) in params {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut out = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    fn query_bytes(sql: &str) -> Vec<u8> {
        let mut out = vec![b'Q'];
        out.extend(((sql.len() + 5) as i32).to_be_bytes());
        out.extend(sql.as_bytes());
        out.push(0);
        out
    }

    #[test]
    fn documented_byte_sequences_decode_and_encode() {
        // SSLRequest, from the protocol documentation.
        let (m, used) = decode_startup(&[0, 0, 0, 8, 4, 210, 22, 47])
            .unwrap()
            .unwrap();
        assert_eq!((m, used), (Startup::SslRequest, 8));
        // Query "SELECT 1".
        let (m, used) = decode_frontend(&query_bytes("SELECT 1")).unwrap().unwrap();
        assert_eq!((m, used), (Frontend::Query("SELECT 1".into()), 14));
        // AuthenticationOk and ReadyForQuery(idle).
        let mut out = Vec::new();
        Backend::AuthenticationOk.encode(&mut out).unwrap();
        assert_eq!(out, [b'R', 0, 0, 0, 8, 0, 0, 0, 0]);
        out.clear();
        Backend::ReadyForQuery(TxStatus::Idle)
            .encode(&mut out)
            .unwrap();
        assert_eq!(out, [b'Z', 0, 0, 0, 5, b'I']);
    }

    #[test]
    fn a_startup_message_carries_its_parameters_in_order() {
        let bytes = startup_bytes(&[("user", "ada"), ("database", "work")]);
        let (m, used) = decode_startup(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(
            m,
            Startup::Params(vec![
                ("user".into(), "ada".into()),
                ("database".into(), "work".into())
            ])
        );
    }

    #[test]
    fn a_cancel_request_names_the_session() {
        let mut bytes = 16i32.to_be_bytes().to_vec();
        bytes.extend(CANCEL_REQUEST.to_be_bytes());
        bytes.extend(7i32.to_be_bytes());
        bytes.extend((-3i32).to_be_bytes());
        assert_eq!(
            decode_startup(&bytes).unwrap().unwrap().0,
            Startup::Cancel { pid: 7, secret: -3 }
        );
    }

    fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend(((body.len() + 4) as i32).to_be_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn messages_outside_the_supported_set_are_refused_by_name() {
        // CopyData is not implemented.
        assert_eq!(
            decode_frontend(&[b'd', 0, 0, 0, 4]),
            Err(ProtoError::UnknownTag(b'd'))
        );
    }

    #[test]
    fn extended_query_messages_decode_from_documented_bytes() {
        let parse = framed(b'P', b"s1\0SELECT $1\0\0\x01\0\0\0\x17");
        assert_eq!(
            decode_frontend(&parse).unwrap().unwrap().0,
            Frontend::Parse {
                name: "s1".into(),
                query: "SELECT $1".into(),
                param_types: vec![23]
            }
        );
        // Bind: portal "", statement "s1", one text format, two params (a value and NULL), one binary result.
        let bind = framed(
            b'B',
            b"\0s1\0\0\x01\0\0\0\x02\0\0\0\x02hi\xff\xff\xff\xff\0\x01\0\x01",
        );
        assert_eq!(
            decode_frontend(&bind).unwrap().unwrap().0,
            Frontend::Bind {
                portal: String::new(),
                statement: "s1".into(),
                param_formats: vec![0],
                params: vec![Some(b"hi".to_vec()), None],
                result_formats: vec![1]
            }
        );
        assert_eq!(
            decode_frontend(&framed(b'D', b"Ss1\0")).unwrap().unwrap().0,
            Frontend::Describe(Target::Statement("s1".into()))
        );
        assert_eq!(
            decode_frontend(&framed(b'C', b"P\0")).unwrap().unwrap().0,
            Frontend::Close(Target::Portal(String::new()))
        );
        assert_eq!(
            decode_frontend(&framed(b'E', b"\0\0\0\0\x05"))
                .unwrap()
                .unwrap()
                .0,
            Frontend::Execute {
                portal: String::new(),
                max_rows: 5
            }
        );
        assert_eq!(
            decode_frontend(&[b'S', 0, 0, 0, 4]).unwrap().unwrap(),
            (Frontend::Sync, 5)
        );
        assert_eq!(
            decode_frontend(&[b'H', 0, 0, 0, 4]).unwrap().unwrap(),
            (Frontend::Flush, 5)
        );
    }

    #[test]
    fn extended_query_messages_reject_values_the_protocol_forbids() {
        let bad = |tag, body: &[u8]| decode_frontend(&framed(tag, body));
        // Negative parameter count in Parse.
        assert!(matches!(
            bad(b'P', b"\0\0\xff\xff"),
            Err(ProtoError::InvalidField(_))
        ));
        // Format code 2.
        assert!(matches!(
            bad(b'B', b"\0\0\0\x01\0\x02\0\0\0\0"),
            Err(ProtoError::InvalidField(_))
        ));
        // Two format codes for three parameters.
        assert!(matches!(
            bad(b'B', b"\0\0\0\x02\0\0\0\0\0\x03"),
            Err(ProtoError::InvalidField(_))
        ));
        // Parameter length -2.
        assert!(matches!(
            bad(b'B', b"\0\0\0\0\0\x01\xff\xff\xff\xfe\0\0"),
            Err(ProtoError::InvalidField(_))
        ));
        // Parameter longer than the message.
        assert_eq!(
            bad(b'B', b"\0\0\0\0\0\x01\0\0\0\x09ab\0\0"),
            Err(ProtoError::Truncated)
        );
        // Describe target other than S or P, and a negative row limit.
        assert!(matches!(
            bad(b'D', b"X\0"),
            Err(ProtoError::InvalidField(_))
        ));
        assert!(matches!(
            bad(b'E', b"\0\xff\xff\xff\xff"),
            Err(ProtoError::InvalidField(_))
        ));
        // Bytes after Sync's empty body.
        assert_eq!(bad(b'S', b"x"), Err(ProtoError::Trailing));
    }

    #[test]
    fn extended_query_replies_encode_to_documented_bytes() {
        let enc = |m: Backend| {
            let mut out = Vec::new();
            m.encode(&mut out).unwrap();
            out
        };
        assert_eq!(enc(Backend::ParseComplete), [b'1', 0, 0, 0, 4]);
        assert_eq!(enc(Backend::BindComplete), [b'2', 0, 0, 0, 4]);
        assert_eq!(enc(Backend::CloseComplete), [b'3', 0, 0, 0, 4]);
        assert_eq!(enc(Backend::NoData), [b'n', 0, 0, 0, 4]);
        assert_eq!(enc(Backend::PortalSuspended), [b's', 0, 0, 0, 4]);
        assert_eq!(
            enc(Backend::ParameterDescription(vec![23, 25])),
            [b't', 0, 0, 0, 14, 0, 2, 0, 0, 0, 23, 0, 0, 0, 25]
        );
    }

    #[test]
    fn malformed_messages_are_errors_and_incomplete_ones_ask_for_more() {
        // Too small and too large lengths.
        assert_eq!(
            decode_frontend(&[b'Q', 0, 0, 0, 3]),
            Err(ProtoError::BadLength(3))
        );
        let huge = (MAX_MESSAGE_LEN as i32 + 1).to_be_bytes();
        assert!(matches!(
            decode_frontend(&[b'Q', huge[0], huge[1], huge[2], huge[3]]),
            Err(ProtoError::BadLength(_))
        ));
        // No terminator, bytes after the terminator, invalid UTF-8.
        assert_eq!(
            decode_frontend(&[b'Q', 0, 0, 0, 6, b'a', b'b']),
            Err(ProtoError::Unterminated)
        );
        assert_eq!(
            decode_frontend(&[b'Q', 0, 0, 0, 7, b'a', 0, b'b']),
            Err(ProtoError::Trailing)
        );
        assert_eq!(
            decode_frontend(&[b'Q', 0, 0, 0, 6, 0xFF, 0]),
            Err(ProtoError::NotUtf8)
        );
        // An unsupported protocol version.
        let mut v2 = 8i32.to_be_bytes().to_vec();
        v2.extend(131_072i32.to_be_bytes());
        assert_eq!(
            decode_startup(&v2),
            Err(ProtoError::UnsupportedProtocol(131_072))
        );
        // Every proper prefix of a valid message is "need more", never an error.
        let q = query_bytes("SELECT 1");
        for cut in 0..q.len() {
            assert_eq!(decode_frontend(&q[..cut]), Ok(None), "cut {cut}");
        }
    }

    #[test]
    fn encoding_refuses_what_the_format_cannot_hold_and_leaves_the_buffer_alone() {
        let mut out = b"keep".to_vec();
        let too_many = Backend::DataRow(vec![None; 40_000]);
        assert_eq!(too_many.encode(&mut out), Err(ProtoError::TooLarge));
        let nul = Backend::CommandComplete("a\0b".into());
        assert!(nul.encode(&mut out).is_err());
        assert_eq!(out, b"keep");
    }

    #[test]
    fn a_row_description_and_data_row_have_the_documented_layout() {
        let mut out = Vec::new();
        Backend::RowDescription(vec![Column {
            name: "n".into(),
            table_oid: 0,
            column_attr: 0,
            type_oid: 23,
            type_size: 4,
            type_modifier: -1,
            format: 0,
        }])
        .encode(&mut out)
        .unwrap();
        // tag, length, 1 column, "n\0", 3 ints and 2 shorts
        assert_eq!(out[0], b'T');
        assert_eq!(
            u32::from_be_bytes(out[1..5].try_into().unwrap()) as usize,
            out.len() - 1
        );
        assert_eq!(&out[5..7], &[0, 1]);
        out.clear();
        Backend::DataRow(vec![Some(b"42".to_vec()), None])
            .encode(&mut out)
            .unwrap();
        assert_eq!(
            out,
            [
                b'D', 0, 0, 0, 16, 0, 2, 0, 0, 0, 2, b'4', b'2', 255, 255, 255, 255
            ]
        );
    }

    proptest! {
        #[test]
        fn a_query_survives_encoding_and_every_split_point(sql in "[ -~]{0,200}") {
            let bytes = query_bytes(&sql);
            let (m, used) = decode_frontend(&bytes).unwrap().unwrap();
            prop_assert_eq!(m, Frontend::Query(sql));
            prop_assert_eq!(used, bytes.len());
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_never_overrun(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            for result in [
                decode_frontend(&bytes).map(|o| o.map(|(_, n)| n)),
                decode_startup(&bytes).map(|o| o.map(|(_, n)| n)),
            ] {
                if let Ok(Some(n)) = result {
                    prop_assert!(n <= bytes.len());
                }
            }
        }

        #[test]
        fn every_backend_message_declares_its_own_length(
            name in "[a-z_]{0,20}", value in "[ -~]{0,40}", cols in proptest::collection::vec(proptest::option::of(proptest::collection::vec(any::<u8>(), 0..20)), 0..10)
        ) {
            for msg in [
                Backend::ParameterStatus { name: name.clone(), value: value.clone() },
                Backend::DataRow(cols.clone()),
                Backend::CommandComplete(value.clone()),
                Backend::ErrorResponse { severity: "ERROR".into(), code: "42601".into(), message: value.clone() },
            ] {
                let mut out = Vec::new();
                msg.encode(&mut out).unwrap();
                let declared = u32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
                prop_assert_eq!(declared, out.len() - 1);
            }
        }
    }
}
