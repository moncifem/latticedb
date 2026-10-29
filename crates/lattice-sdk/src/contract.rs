//! Agent plugin contract v1: a strict, bounded, line-delimited JSON codec.
//!
//! The wire format is specified in `formats/plugin-contract-v1.md`. One message is one UTF-8 JSON
//! object on one line. The decoder accepts exactly that grammar and nothing else: unknown fields,
//! unknown versions, duplicate keys, floats, negative numbers, lone surrogates, over-long lines and
//! over-deep nesting are all refused, and no input makes it panic. The encoder is canonical (fixed
//! field order, minimal escaping) and refuses any message the decoder would refuse, so
//! `decode_line(encode_line(m)) == m` for every message it accepts.

use std::fmt;

/// The only protocol version this module speaks.
pub const VERSION: u64 = 1;
/// Longest accepted line, in bytes, not counting the terminating newline.
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// Longest request id, session id or record id, in bytes. Record ids may use [`MAX_RID_BYTES`].
pub const MAX_ID_BYTES: usize = 64;
/// Longest source name, in bytes.
pub const MAX_SOURCE_BYTES: usize = 64;
/// Longest external session id, in bytes.
pub const MAX_EXTERNAL_ID_BYTES: usize = 256;
/// Longest client record id, in bytes.
pub const MAX_RID_BYTES: usize = 128;
/// Longest record body, in bytes of UTF-8.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Longest error message, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 512;
/// Most records in one append and most receipts in one receipt message.
pub const MAX_RECORDS: usize = 64;
/// Largest integer on the wire: 2^53 - 1, so every JavaScript client reads it exactly.
pub const MAX_INT: u64 = (1 << 53) - 1;

const MAX_DEPTH: usize = 6;
const MAX_KEYS: usize = 16;

/// Why a line was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// The line is longer than [`MAX_LINE_BYTES`].
    TooLong,
    /// The stream ended inside a line (no terminating newline).
    Unterminated,
    /// The line is empty or only whitespace.
    EmptyLine,
    /// The line is not valid UTF-8.
    InvalidUtf8,
    /// The line is not valid JSON of the accepted subset.
    Syntax(&'static str),
    /// A number is negative, fractional, has an exponent or a leading zero, or exceeds [`MAX_INT`].
    InvalidNumber,
    /// Nesting is deeper than the contract allows.
    TooDeep,
    /// An object or array has more members than the contract allows.
    TooMany,
    /// An object repeats a key.
    DuplicateKey(String),
    /// An object has a key this message type does not define.
    UnknownField(String),
    /// A required field is absent.
    MissingField(&'static str),
    /// A field has the wrong JSON type.
    WrongType(&'static str),
    /// `v` is an integer other than 1.
    UnsupportedVersion(u64),
    /// `t` names no message type of version 1.
    UnknownType(String),
    /// A field is well typed but outside its allowed values or size.
    InvalidValue(&'static str),
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "line exceeds {MAX_LINE_BYTES} bytes"),
            Self::Unterminated => f.write_str("stream ended inside a line"),
            Self::EmptyLine => f.write_str("empty line"),
            Self::InvalidUtf8 => f.write_str("line is not valid UTF-8"),
            Self::Syntax(what) => write!(f, "invalid JSON: {what}"),
            Self::InvalidNumber => f.write_str("number must be an integer in 0..=2^53-1"),
            Self::TooDeep => f.write_str("nesting too deep"),
            Self::TooMany => f.write_str("too many members"),
            Self::DuplicateKey(k) => write!(f, "duplicate key {k:?}"),
            Self::UnknownField(k) => write!(f, "unknown field {k:?}"),
            Self::MissingField(k) => write!(f, "missing field {k:?}"),
            Self::WrongType(k) => write!(f, "field {k:?} has the wrong type"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported version {v}"),
            Self::UnknownType(t) => write!(f, "unknown message type {t:?}"),
            Self::InvalidValue(what) => write!(f, "invalid value: {what}"),
        }
    }
}

impl std::error::Error for ContractError {}

impl ContractError {
    /// The wire error code a server reports for this refusal.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::TooLong => ErrorCode::TooLarge,
            Self::UnsupportedVersion(_) => ErrorCode::UnsupportedVersion,
            _ => ErrorCode::BadRequest,
        }
    }
}

/// Kind of a session record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A user or assistant message.
    Message,
    /// A tool invocation.
    ToolCall,
    /// A tool result.
    ToolResult,
    /// A free-form annotation.
    Note,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Note => "note",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "message" => Self::Message,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            "note" => Self::Note,
            _ => return None,
        })
    }
}

/// Outcome of one record in an append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Stored now, at a new position.
    Recorded,
    /// Already stored earlier under the same record id; nothing changed.
    Duplicate,
    /// The turn was superseded; nothing was written.
    Fenced,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Duplicate => "duplicate",
            Self::Fenced => "fenced",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "recorded" => Self::Recorded,
            "duplicate" => Self::Duplicate,
            "fenced" => Self::Fenced,
            _ => return None,
        })
    }
}

/// State of an agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// The turn is the current one.
    Running,
    /// The current turn ended normally.
    Idle,
    /// Another turn took over; writes under this turn are fenced.
    Superseded,
}

impl TurnState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Superseded => "superseded",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "idle" => Self::Idle,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

/// Error codes of the `error` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The line is malformed or violates a rule of the contract.
    BadRequest,
    /// `v` is not 1.
    UnsupportedVersion,
    /// The session id is not known.
    UnknownSession,
    /// The turn number was never issued for the session.
    UnknownTurn,
    /// No watch with that id is open.
    UnknownWatch,
    /// The request contradicts an earlier one (same key, different content).
    Conflict,
    /// The line exceeds the size limit.
    TooLarge,
    /// The plugin failed for a reason that is not the client's.
    Internal,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::UnsupportedVersion => "unsupported_version",
            Self::UnknownSession => "unknown_session",
            Self::UnknownTurn => "unknown_turn",
            Self::UnknownWatch => "unknown_watch",
            Self::Conflict => "conflict",
            Self::TooLarge => "too_large",
            Self::Internal => "internal",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "bad_request" => Self::BadRequest,
            "unsupported_version" => Self::UnsupportedVersion,
            "unknown_session" => Self::UnknownSession,
            "unknown_turn" => Self::UnknownTurn,
            "unknown_watch" => Self::UnknownWatch,
            "conflict" => Self::Conflict,
            "too_large" => Self::TooLarge,
            "internal" => Self::Internal,
            _ => return None,
        })
    }
}

/// One record in an append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Client-chosen idempotency key, unique within the session.
    pub rid: String,
    /// What the record is.
    pub kind: Kind,
    /// Payload text.
    pub body: String,
}

/// The outcome for one appended record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The record id the receipt answers.
    pub rid: String,
    /// What happened.
    pub status: Status,
    /// Position of the record; present exactly when the status is not `fenced`.
    pub pos: Option<u64>,
}

/// A contract message, in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Client: create or look up a session by source and external id (`session.create`).
    SessionCreate {
        /// Request id.
        id: String,
        /// Source system name.
        source: String,
        /// Id of the session in the source system.
        external_id: String,
        /// Parent session id, for a child session.
        parent: Option<String>,
    },
    /// Plugin: the session (`session.created`).
    SessionCreated {
        /// Request id being answered.
        id: String,
        /// Plugin session id.
        session: String,
        /// True when this call created it, false when it already existed.
        created: bool,
        /// Parent session id, when the session is a child.
        parent: Option<String>,
    },
    /// Client: append records under a turn (`append`).
    Append {
        /// Request id.
        id: String,
        /// Session id.
        session: String,
        /// Turn number the records belong to.
        turn: u64,
        /// One to [`MAX_RECORDS`] records.
        records: Vec<Record>,
    },
    /// Plugin: one receipt per appended record, in order (`append.receipt`).
    AppendReceipt {
        /// Request id being answered.
        id: String,
        /// Session id.
        session: String,
        /// Turn number of the append.
        turn: u64,
        /// Receipts, same order and length as the records.
        receipts: Vec<Receipt>,
    },
    /// Client: take the turn, superseding the current one (`turn.take`).
    TurnTake {
        /// Request id.
        id: String,
        /// Session id.
        session: String,
    },
    /// Client: end a turn (`turn.end`).
    TurnEnd {
        /// Request id.
        id: String,
        /// Session id.
        session: String,
        /// Turn number.
        turn: u64,
    },
    /// Plugin: the state of a turn (`turn.state`).
    TurnStateReply {
        /// Request id being answered.
        id: String,
        /// Session id.
        session: String,
        /// Turn number.
        turn: u64,
        /// State.
        state: TurnState,
    },
    /// Client: open a watch from a cursor (`watch`).
    Watch {
        /// Watch id; every stream message of this watch carries it.
        id: String,
        /// Session id.
        session: String,
        /// Last position the client has seen; 0 for the start.
        cursor: u64,
    },
    /// Client: close a watch (`watch.stop`).
    WatchStop {
        /// Watch id.
        id: String,
    },
    /// Plugin: the watch is closed (`watch.stopped`).
    WatchStopped {
        /// Watch id.
        id: String,
    },
    /// Plugin: the watch is open; replay follows (`ready`).
    Ready {
        /// Watch id.
        id: String,
        /// Session id.
        session: String,
        /// Highest position in the session now.
        head: u64,
    },
    /// Plugin: a record at a position (`item`).
    Item {
        /// Watch id.
        id: String,
        /// Session id.
        session: String,
        /// Position of the record.
        pos: u64,
        /// Turn the record was written under.
        turn: u64,
        /// Client record id.
        rid: String,
        /// Record kind.
        kind: Kind,
        /// Record payload.
        body: String,
    },
    /// Plugin: a turn changed state at a position (`turn.event`).
    TurnEvent {
        /// Watch id.
        id: String,
        /// Session id.
        session: String,
        /// Position of the event.
        pos: u64,
        /// Turn number.
        turn: u64,
        /// New state.
        state: TurnState,
    },
    /// Plugin: replay is complete; everything after is incremental (`live`).
    Live {
        /// Watch id.
        id: String,
        /// Session id.
        session: String,
    },
    /// Plugin: a request failed (`error`).
    Error {
        /// Request id, or `None` when the line could not be attributed.
        id: Option<String>,
        /// Machine-readable code.
        code: ErrorCode,
        /// Human-readable text.
        message: String,
    },
}

impl Message {
    /// The `t` value of this message.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SessionCreate { .. } => "session.create",
            Self::SessionCreated { .. } => "session.created",
            Self::Append { .. } => "append",
            Self::AppendReceipt { .. } => "append.receipt",
            Self::TurnTake { .. } => "turn.take",
            Self::TurnEnd { .. } => "turn.end",
            Self::TurnStateReply { .. } => "turn.state",
            Self::Watch { .. } => "watch",
            Self::WatchStop { .. } => "watch.stop",
            Self::WatchStopped { .. } => "watch.stopped",
            Self::Ready { .. } => "ready",
            Self::Item { .. } => "item",
            Self::TurnEvent { .. } => "turn.event",
            Self::Live { .. } => "live",
            Self::Error { .. } => "error",
        }
    }

    /// True for messages a client sends to a plugin.
    #[must_use]
    pub fn is_request(&self) -> bool {
        matches!(
            self,
            Self::SessionCreate { .. }
                | Self::Append { .. }
                | Self::TurnTake { .. }
                | Self::TurnEnd { .. }
                | Self::Watch { .. }
                | Self::WatchStop { .. }
        )
    }

    /// Check every size, charset and cross-field rule of the contract.
    ///
    /// # Errors
    /// [`ContractError::InvalidValue`] naming the first rule that fails.
    pub fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::SessionCreate {
                id,
                source,
                external_id,
                parent,
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                source_name(source)?;
                external(external_id)?;
                opt_token(parent.as_deref(), "parent")
            }
            Self::SessionCreated {
                id,
                session,
                parent,
                ..
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                opt_token(parent.as_deref(), "parent")
            }
            Self::Append {
                id,
                session,
                turn,
                records,
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                number(*turn, 1, "turn")?;
                count(records.len(), "records")?;
                for r in records {
                    token(&r.rid, MAX_RID_BYTES, "rid")?;
                    if r.body.len() > MAX_BODY_BYTES {
                        return Err(ContractError::InvalidValue("body too long"));
                    }
                }
                Ok(())
            }
            Self::AppendReceipt {
                id,
                session,
                turn,
                receipts,
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                number(*turn, 1, "turn")?;
                count(receipts.len(), "receipts")?;
                for r in receipts {
                    token(&r.rid, MAX_RID_BYTES, "rid")?;
                    match (r.status, r.pos) {
                        (Status::Fenced, None) => {}
                        (Status::Fenced, Some(_)) => {
                            return Err(ContractError::InvalidValue("fenced receipt has pos"));
                        }
                        (_, Some(p)) => number(p, 1, "pos")?,
                        (_, None) => {
                            return Err(ContractError::InvalidValue("receipt needs pos"));
                        }
                    }
                }
                Ok(())
            }
            Self::TurnTake { id, session } | Self::Live { id, session } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")
            }
            Self::TurnEnd { id, session, turn }
            | Self::TurnStateReply {
                id, session, turn, ..
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                number(*turn, 1, "turn")
            }
            Self::Watch {
                id,
                session,
                cursor,
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                number(*cursor, 0, "cursor")
            }
            Self::WatchStop { id } | Self::WatchStopped { id } => token(id, MAX_ID_BYTES, "id"),
            Self::Ready { id, session, head } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                number(*head, 0, "head")
            }
            Self::Item {
                id,
                session,
                pos,
                turn,
                rid,
                body,
                ..
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                number(*pos, 1, "pos")?;
                number(*turn, 1, "turn")?;
                token(rid, MAX_RID_BYTES, "rid")?;
                if body.len() > MAX_BODY_BYTES {
                    return Err(ContractError::InvalidValue("body too long"));
                }
                Ok(())
            }
            Self::TurnEvent {
                id,
                session,
                pos,
                turn,
                ..
            } => {
                token(id, MAX_ID_BYTES, "id")?;
                token(session, MAX_ID_BYTES, "session")?;
                number(*pos, 1, "pos")?;
                number(*turn, 1, "turn")
            }
            Self::Error { id, message, .. } => {
                opt_token(id.as_deref(), "id")?;
                if message.len() > MAX_MESSAGE_BYTES {
                    return Err(ContractError::InvalidValue("message too long"));
                }
                Ok(())
            }
        }
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')
}

fn token(s: &str, max: usize, what: &'static str) -> Result<(), ContractError> {
    if s.is_empty() || s.len() > max || !s.chars().all(is_token_char) {
        return Err(ContractError::InvalidValue(what));
    }
    Ok(())
}

fn opt_token(s: Option<&str>, what: &'static str) -> Result<(), ContractError> {
    s.map_or(Ok(()), |s| token(s, MAX_ID_BYTES, what))
}

fn source_name(s: &str) -> Result<(), ContractError> {
    let ok = !s.is_empty()
        && s.len() <= MAX_SOURCE_BYTES
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(ContractError::InvalidValue("source"))
    }
}

fn external(s: &str) -> Result<(), ContractError> {
    if s.is_empty() || s.len() > MAX_EXTERNAL_ID_BYTES || s.chars().any(char::is_control) {
        return Err(ContractError::InvalidValue("external_id"));
    }
    Ok(())
}

fn number(n: u64, min: u64, what: &'static str) -> Result<(), ContractError> {
    if n < min || n > MAX_INT {
        return Err(ContractError::InvalidValue(what));
    }
    Ok(())
}

fn count(n: usize, what: &'static str) -> Result<(), ContractError> {
    if n == 0 || n > MAX_RECORDS {
        return Err(ContractError::InvalidValue(what));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// JSON subset parser
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
enum Json {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    fn expect(&mut self, c: u8, what: &'static str) -> Result<(), ContractError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(ContractError::Syntax(what))
        }
    }

    fn literal(&mut self, lit: &[u8], v: Json) -> Result<Json, ContractError> {
        if self.b.get(self.i..).is_some_and(|r| r.starts_with(lit)) {
            self.i += lit.len();
            Ok(v)
        } else {
            Err(ContractError::Syntax("bad literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ContractError> {
        if depth > MAX_DEPTH {
            return Err(ContractError::TooDeep);
        }
        self.skip_ws();
        match self.peek() {
            None => Err(ContractError::Syntax("unexpected end")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b'0'..=b'9' | b'-') => self.number(),
            Some(_) => Err(ContractError::Syntax("unexpected byte")),
        }
    }

    fn number(&mut self) -> Result<Json, ContractError> {
        let first = self.peek();
        let mut n: u64 = 0;
        let mut digits = 0usize;
        while let Some(d @ b'0'..=b'9') = self.peek() {
            if digits == 1 && first == Some(b'0') {
                return Err(ContractError::InvalidNumber);
            }
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add(u64::from(d - b'0')))
                .filter(|n| *n <= MAX_INT)
                .ok_or(ContractError::InvalidNumber)?;
            digits += 1;
            self.i += 1;
        }
        // A sign, fraction or exponent leaves a byte that is not a number terminator.
        if digits == 0 || matches!(self.peek(), Some(b'.' | b'e' | b'E' | b'-' | b'+')) {
            return Err(ContractError::InvalidNumber);
        }
        Ok(Json::Num(n))
    }

    fn hex4(&mut self) -> Result<u32, ContractError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = self
                .peek()
                .and_then(|c| char::from(c).to_digit(16))
                .ok_or(ContractError::Syntax("bad \\u escape"))?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ContractError> {
        self.expect(b'"', "expected string")?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(ContractError::Syntax("unterminated string"));
            };
            self.i += 1;
            match c {
                b'"' => break,
                0..=0x1f => return Err(ContractError::Syntax("raw control character in string")),
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err(ContractError::Syntax("unterminated escape"));
                    };
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return Err(ContractError::Syntax("bad escape")),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| ContractError::InvalidUtf8)
    }

    fn unicode_escape(&mut self) -> Result<char, ContractError> {
        let hi = self.hex4()?;
        let cp = match hi {
            0xD800..=0xDBFF => {
                if self.peek() != Some(b'\\') {
                    return Err(ContractError::Syntax("lone surrogate"));
                }
                self.i += 1;
                self.expect(b'u', "lone surrogate")?;
                let lo = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(ContractError::Syntax("lone surrogate"));
                }
                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(ContractError::Syntax("lone surrogate")),
            _ => hi,
        };
        char::from_u32(cp).ok_or(ContractError::Syntax("bad code point"))
    }

    fn array(&mut self, depth: usize) -> Result<Json, ContractError> {
        self.expect(b'[', "expected array")?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            if items.len() >= MAX_RECORDS {
                return Err(ContractError::TooMany);
            }
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(ContractError::Syntax("expected , or ]")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ContractError> {
        self.expect(b'{', "expected object")?;
        let mut items: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(items));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            if items.iter().any(|(k, _)| *k == key) {
                return Err(ContractError::DuplicateKey(key));
            }
            if items.len() >= MAX_KEYS {
                return Err(ContractError::TooMany);
            }
            self.skip_ws();
            self.expect(b':', "expected :")?;
            let v = self.value(depth + 1)?;
            items.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(items));
                }
                _ => return Err(ContractError::Syntax("expected , or }")),
            }
        }
    }
}

/// An object whose fields are consumed one by one; leftovers are unknown fields.
struct Fields(Vec<(String, Json)>);

impl Fields {
    fn from_json(j: Json) -> Result<Self, ContractError> {
        match j {
            Json::Obj(items) => Ok(Self(items)),
            _ => Err(ContractError::Syntax("expected object")),
        }
    }

    fn take(&mut self, key: &str) -> Option<Json> {
        let at = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(at).1)
    }

    fn string(&mut self, key: &'static str) -> Result<String, ContractError> {
        match self.take(key) {
            Some(Json::Str(s)) => Ok(s),
            Some(_) => Err(ContractError::WrongType(key)),
            None => Err(ContractError::MissingField(key)),
        }
    }

    fn opt_string(&mut self, key: &'static str) -> Result<Option<String>, ContractError> {
        match self.take(key) {
            Some(Json::Str(s)) => Ok(Some(s)),
            Some(_) => Err(ContractError::WrongType(key)),
            None => Ok(None),
        }
    }

    fn num(&mut self, key: &'static str) -> Result<u64, ContractError> {
        match self.take(key) {
            Some(Json::Num(n)) => Ok(n),
            Some(_) => Err(ContractError::WrongType(key)),
            None => Err(ContractError::MissingField(key)),
        }
    }

    fn boolean(&mut self, key: &'static str) -> Result<bool, ContractError> {
        match self.take(key) {
            Some(Json::Bool(b)) => Ok(b),
            Some(_) => Err(ContractError::WrongType(key)),
            None => Err(ContractError::MissingField(key)),
        }
    }

    fn array(&mut self, key: &'static str) -> Result<Vec<Json>, ContractError> {
        match self.take(key) {
            Some(Json::Arr(a)) => Ok(a),
            Some(_) => Err(ContractError::WrongType(key)),
            None => Err(ContractError::MissingField(key)),
        }
    }

    fn enum_of<T>(
        &mut self,
        key: &'static str,
        parse: fn(&str) -> Option<T>,
    ) -> Result<T, ContractError> {
        let s = self.string(key)?;
        parse(&s).ok_or(ContractError::InvalidValue(key))
    }

    fn finish(self) -> Result<(), ContractError> {
        match self.0.into_iter().next() {
            Some((k, _)) => Err(ContractError::UnknownField(k)),
            None => Ok(()),
        }
    }
}

/// Decode one line (without its newline) into a validated message.
///
/// # Errors
/// A [`ContractError`] for anything outside the version 1 grammar. Never panics.
pub fn decode_line(line: &[u8]) -> Result<Message, ContractError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ContractError::TooLong);
    }
    let text = std::str::from_utf8(line).map_err(|_| ContractError::InvalidUtf8)?;
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    p.skip_ws();
    if p.peek().is_none() {
        return Err(ContractError::EmptyLine);
    }
    let root = p.value(0)?;
    p.skip_ws();
    if p.peek().is_some() {
        return Err(ContractError::Syntax("trailing data after message"));
    }
    let mut f = Fields::from_json(root)?;
    match f.take("v") {
        Some(Json::Num(VERSION)) => {}
        Some(Json::Num(other)) => return Err(ContractError::UnsupportedVersion(other)),
        Some(_) => return Err(ContractError::WrongType("v")),
        None => return Err(ContractError::MissingField("v")),
    }
    let t = f.string("t")?;
    let msg = build(&t, &mut f)?;
    f.finish()?;
    msg.validate()?;
    Ok(msg)
}

fn build(t: &str, f: &mut Fields) -> Result<Message, ContractError> {
    Ok(match t {
        "session.create" => Message::SessionCreate {
            id: f.string("id")?,
            source: f.string("source")?,
            external_id: f.string("external_id")?,
            parent: f.opt_string("parent")?,
        },
        "session.created" => Message::SessionCreated {
            id: f.string("id")?,
            session: f.string("session")?,
            created: f.boolean("created")?,
            parent: f.opt_string("parent")?,
        },
        "append" => {
            let id = f.string("id")?;
            let session = f.string("session")?;
            let turn = f.num("turn")?;
            let mut records = Vec::new();
            for j in f.array("records")? {
                let mut r = Fields::from_json(j)?;
                let rec = Record {
                    rid: r.string("rid")?,
                    kind: r.enum_of("kind", Kind::parse)?,
                    body: r.string("body")?,
                };
                r.finish()?;
                records.push(rec);
            }
            Message::Append {
                id,
                session,
                turn,
                records,
            }
        }
        "append.receipt" => {
            let id = f.string("id")?;
            let session = f.string("session")?;
            let turn = f.num("turn")?;
            let mut receipts = Vec::new();
            for j in f.array("receipts")? {
                let mut r = Fields::from_json(j)?;
                let rc = Receipt {
                    rid: r.string("rid")?,
                    status: r.enum_of("status", Status::parse)?,
                    pos: match r.take("pos") {
                        Some(Json::Num(n)) => Some(n),
                        Some(_) => return Err(ContractError::WrongType("pos")),
                        None => None,
                    },
                };
                r.finish()?;
                receipts.push(rc);
            }
            Message::AppendReceipt {
                id,
                session,
                turn,
                receipts,
            }
        }
        "turn.take" => Message::TurnTake {
            id: f.string("id")?,
            session: f.string("session")?,
        },
        "turn.end" => Message::TurnEnd {
            id: f.string("id")?,
            session: f.string("session")?,
            turn: f.num("turn")?,
        },
        "turn.state" => Message::TurnStateReply {
            id: f.string("id")?,
            session: f.string("session")?,
            turn: f.num("turn")?,
            state: f.enum_of("state", TurnState::parse)?,
        },
        "watch" => Message::Watch {
            id: f.string("id")?,
            session: f.string("session")?,
            cursor: f.num("cursor")?,
        },
        "watch.stop" => Message::WatchStop {
            id: f.string("id")?,
        },
        "watch.stopped" => Message::WatchStopped {
            id: f.string("id")?,
        },
        "ready" => Message::Ready {
            id: f.string("id")?,
            session: f.string("session")?,
            head: f.num("head")?,
        },
        "item" => Message::Item {
            id: f.string("id")?,
            session: f.string("session")?,
            pos: f.num("pos")?,
            turn: f.num("turn")?,
            rid: f.string("rid")?,
            kind: f.enum_of("kind", Kind::parse)?,
            body: f.string("body")?,
        },
        "turn.event" => Message::TurnEvent {
            id: f.string("id")?,
            session: f.string("session")?,
            pos: f.num("pos")?,
            turn: f.num("turn")?,
            state: f.enum_of("state", TurnState::parse)?,
        },
        "live" => Message::Live {
            id: f.string("id")?,
            session: f.string("session")?,
        },
        "error" => Message::Error {
            id: match f.take("id") {
                Some(Json::Str(s)) => Some(s),
                Some(Json::Null) => None,
                Some(_) => return Err(ContractError::WrongType("id")),
                None => return Err(ContractError::MissingField("id")),
            },
            code: f.enum_of("code", ErrorCode::parse)?,
            message: f.string("message")?,
        },
        other => return Err(ContractError::UnknownType(other.to_owned())),
    })
}

// ---------------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------------

fn put_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => {
                out.push_str("\\u00");
                for shift in [4u32, 0] {
                    let d = (u32::from(c) >> shift) & 0xf;
                    out.push(char::from_digit(d, 16).unwrap_or('0'));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn field_str(out: &mut String, key: &str, v: &str) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":");
    put_str(out, v);
}

fn field_raw(out: &mut String, key: &str, v: &str) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":");
    out.push_str(v);
}

fn field_num(out: &mut String, key: &str, v: u64) {
    field_raw(out, key, &v.to_string());
}

/// Encode a message as one canonical line, without the terminating newline.
///
/// # Errors
/// The message violates a contract rule ([`Message::validate`]) or the line would exceed
/// [`MAX_LINE_BYTES`].
pub fn encode_line(msg: &Message) -> Result<String, ContractError> {
    msg.validate()?;
    let mut o = String::new();
    o.push_str("{\"v\":1,\"t\":");
    put_str(&mut o, msg.type_name());
    match msg {
        Message::SessionCreate {
            id,
            source,
            external_id,
            parent,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "source", source);
            field_str(&mut o, "external_id", external_id);
            if let Some(p) = parent {
                field_str(&mut o, "parent", p);
            }
        }
        Message::SessionCreated {
            id,
            session,
            created,
            parent,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_raw(&mut o, "created", if *created { "true" } else { "false" });
            if let Some(p) = parent {
                field_str(&mut o, "parent", p);
            }
        }
        Message::Append {
            id,
            session,
            turn,
            records,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "turn", *turn);
            let mut a = String::from("[");
            for (i, r) in records.iter().enumerate() {
                if i > 0 {
                    a.push(',');
                }
                a.push_str("{\"rid\":");
                put_str(&mut a, &r.rid);
                field_str(&mut a, "kind", r.kind.as_str());
                field_str(&mut a, "body", &r.body);
                a.push('}');
            }
            a.push(']');
            field_raw(&mut o, "records", &a);
        }
        Message::AppendReceipt {
            id,
            session,
            turn,
            receipts,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "turn", *turn);
            let mut a = String::from("[");
            for (i, r) in receipts.iter().enumerate() {
                if i > 0 {
                    a.push(',');
                }
                a.push_str("{\"rid\":");
                put_str(&mut a, &r.rid);
                field_str(&mut a, "status", r.status.as_str());
                if let Some(p) = r.pos {
                    field_num(&mut a, "pos", p);
                }
                a.push('}');
            }
            a.push(']');
            field_raw(&mut o, "receipts", &a);
        }
        Message::TurnTake { id, session } | Message::Live { id, session } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
        }
        Message::TurnEnd { id, session, turn } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "turn", *turn);
        }
        Message::TurnStateReply {
            id,
            session,
            turn,
            state,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "turn", *turn);
            field_str(&mut o, "state", state.as_str());
        }
        Message::Watch {
            id,
            session,
            cursor,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "cursor", *cursor);
        }
        Message::WatchStop { id } | Message::WatchStopped { id } => field_str(&mut o, "id", id),
        Message::Ready { id, session, head } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "head", *head);
        }
        Message::Item {
            id,
            session,
            pos,
            turn,
            rid,
            kind,
            body,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "pos", *pos);
            field_num(&mut o, "turn", *turn);
            field_str(&mut o, "rid", rid);
            field_str(&mut o, "kind", kind.as_str());
            field_str(&mut o, "body", body);
        }
        Message::TurnEvent {
            id,
            session,
            pos,
            turn,
            state,
        } => {
            field_str(&mut o, "id", id);
            field_str(&mut o, "session", session);
            field_num(&mut o, "pos", *pos);
            field_num(&mut o, "turn", *turn);
            field_str(&mut o, "state", state.as_str());
        }
        Message::Error { id, code, message } => {
            match id {
                Some(i) => field_str(&mut o, "id", i),
                None => field_raw(&mut o, "id", "null"),
            }
            field_str(&mut o, "code", code.as_str());
            field_str(&mut o, "message", message);
        }
    }
    o.push('}');
    if o.len() > MAX_LINE_BYTES {
        return Err(ContractError::TooLong);
    }
    Ok(o)
}

// ---------------------------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------------------------

/// Splits a byte stream into lines without ever holding more than [`MAX_LINE_BYTES`] of one.
///
/// A line that grows past the limit is discarded to its newline and reported once as
/// [`ContractError::TooLong`]; the stream stays usable afterwards.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
    skipping: bool,
}

impl Framer {
    /// An empty framer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every line completed by them, without the newline.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<Vec<u8>, ContractError>> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if self.skipping {
                    self.skipping = false;
                    out.push(Err(ContractError::TooLong));
                } else {
                    out.push(Ok(std::mem::take(&mut self.buf)));
                }
            } else if self.skipping {
                continue;
            } else if self.buf.len() >= MAX_LINE_BYTES {
                self.buf.clear();
                self.skipping = true;
            } else {
                self.buf.push(b);
            }
        }
        out
    }

    /// Signal end of stream; reports a trailing line that never got its newline.
    pub fn finish(&mut self) -> Option<Result<Vec<u8>, ContractError>> {
        if self.skipping {
            self.skipping = false;
            return Some(Err(ContractError::TooLong));
        }
        if self.buf.is_empty() {
            None
        } else {
            self.buf.clear();
            Some(Err(ContractError::Unterminated))
        }
    }
}

/// Kani proofs, run by `.github/workflows/kani.yml`; see `docs/verification.md`.
#[cfg(kani)]
#[allow(clippy::indexing_slicing)]
mod proofs {
    use super::*;

    /// For any input of up to 6 bytes pushed in two arbitrary pieces: the framer never panics,
    /// gives exactly one result per newline, never returns a line containing a newline or longer
    /// than the bytes pushed, and never holds more than [`MAX_LINE_BYTES`]. The bound is far
    /// below `MAX_LINE_BYTES` (1 MiB), so this proof does not reach the over-long-line branch;
    /// that branch is covered by the unit tests.
    #[kani::proof]
    #[kani::unwind(10)]
    fn framer_bounds_hold() {
        let raw: [u8; 6] = kani::any();
        let cut: usize = kani::any();
        kani::assume(cut <= raw.len());
        let (head, tail) = raw.split_at(cut);
        let mut framer = Framer::new();
        let mut lines = framer.push(head);
        lines.extend(framer.push(tail));
        let newlines = raw.iter().filter(|b| **b == b'\n').count();
        assert_eq!(lines.len(), newlines);
        for bytes in lines.iter().flatten() {
            assert!(bytes.len() <= raw.len());
            assert!(bytes.len() <= MAX_LINE_BYTES);
            assert!(!bytes.contains(&b'\n'));
        }
        assert!(framer.buf.len() <= MAX_LINE_BYTES);
        assert!(framer.buf.len() <= raw.len());
    }

    /// `finish` after any input of up to 6 bytes never panics and leaves the framer empty.
    #[kani::proof]
    #[kani::unwind(10)]
    fn framer_finish_resets() {
        let raw: [u8; 6] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= raw.len());
        let mut framer = Framer::new();
        let _ = framer.push(&raw[..len]);
        let _ = framer.finish();
        assert!(framer.buf.is_empty());
        assert!(!framer.skipping);
    }
}
