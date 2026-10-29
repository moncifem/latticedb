//! MCP server v0 (WP52, #254), first slice: a sans-I/O message codec, a tool registry and a scoping
//! layer for the Model Context Protocol.
//!
//! # Protocol revision
//!
//! This module targets the Model Context Protocol specification revision **2026-07-28**
//! (<https://modelcontextprotocol.io/specification/2026-07-28>), read on 2026-10-29. That revision
//! is stateless: it has no handshake and every request must carry `_meta` with
//! `io.modelcontextprotocol/protocolVersion` and `io.modelcontextprotocol/clientCapabilities`; a
//! request without them is refused with `-32602`, and another version with `-32022`. As the
//! revision's dual-era server rules allow, a connection that opens with the legacy (**2025-11-25**)
//! `initialize` request is served with legacy semantics (`_meta` optional) for its lifetime; a
//! missing `_meta` never selects that mode. `notifications/initialized` is swallowed. The supported subset
//! and everything that is not implemented (transports, resources, prompts, authentication, tasks,
//! pagination, subscription streams) are listed in `docs/mcp-v0.md`.
//!
//! # Shape
//!
//! [`Server::handle`] takes the bytes of one JSON-RPC message and the authenticated [`Caller`] and
//! returns the bytes of the response, or `None` for a notification. It performs no I/O, keeps one
//! piece of per-connection state (the [`Mode`], set only by `initialize`) and never panics. Messages are bounded ([`MAX_MESSAGE_BYTES`],
//! [`MAX_DEPTH`], [`MAX_ITEMS`], [`MAX_NODES`]); JSON-RPC batches are refused with `-32600`
//! (the revision defines no batching); duplicate object keys are refused, so a message has exactly
//! one reading.
//!
//! A `tools/call` is processed in a fixed order, and the [`ToolBackend`] is reached only at the end:
//! known tool, tool granted to the caller, arguments valid against the tool's schema, target scope
//! covered by a grant. A backend's output is bounded by the type ([`ToolOutput`]), so the server
//! never fails a call after the backend has acted. Every refusal is a JSON-RPC error with a stable code and a fixed message that
//! contains no caller-supplied text and no data from the store.
//!
//! The JSON reader here is deliberately separate from the strict line grammar in
//! [`crate::contract`] (which has no negative numbers, no fractions and a depth of six): JSON-RPC
//! needs the general grammar, bounded.

use std::fmt::Write as _;

/// The protocol revision this module targets.
pub const PROTOCOL_VERSION: &str = "2026-07-28";
/// The legacy revision answered for `initialize`.
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
/// Largest accepted message, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// Deepest accepted JSON nesting.
pub const MAX_DEPTH: usize = 12;
/// Most members of one array or object.
pub const MAX_ITEMS: usize = 64;
/// Most JSON values in one message.
pub const MAX_NODES: usize = 4096;
/// Longest accepted `value` argument of `lattice_write`, in characters (as JSON Schema counts
/// `maxLength`; the message bound limits the bytes).
pub const MAX_VALUE_CHARS: usize = 64 * 1024;
/// Largest tool output the server will return, in bytes.
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// Longest scope string, in bytes.
pub const MAX_SCOPE_BYTES: usize = 128;

/// JSON-RPC and application error codes. Application codes are outside the JSON-RPC reserved range
/// `-32768..=-32000`, as the specification recommends.
pub mod code {
    /// The message is not valid JSON, or is outside the bounded grammar.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON is not a valid JSON-RPC request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method is not implemented.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Unknown tool or arguments that fail the schema.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Server-side failure that is not the caller's doing.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// The request names a protocol version the server does not implement (MCP 2026-07-28).
    pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
    /// The caller holds no grant for the tool.
    pub const TOOL_NOT_GRANTED: i64 = 40301;
    /// The caller holds a grant for the tool, but not for the target scope (or the row policy
    /// behind the backend refused).
    pub const SCOPE_DENIED: i64 = 40302;
    /// The backend could not serve the call.
    pub const BACKEND_UNAVAILABLE: i64 = 50001;
}

// ---------------------------------------------------------------------------------------------
// Bounded JSON reader
// ---------------------------------------------------------------------------------------------

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`
    Null,
    /// `true` or `false`
    Bool(bool),
    /// An integer that fits `i64`.
    Int(i64),
    /// Any other finite number.
    Float(f64),
    /// A string (always valid UTF-8).
    Str(String),
    /// An array of at most [`MAX_ITEMS`] values.
    Arr(Vec<Json>),
    /// An object of at most [`MAX_ITEMS`] members with distinct keys, in source order.
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The member `key` of an object, if this is an object that has it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Why a message could not be read as bounded JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Larger than [`MAX_MESSAGE_BYTES`].
    TooLarge,
    /// Not UTF-8.
    InvalidUtf8,
    /// Not JSON.
    Syntax,
    /// Nested deeper than [`MAX_DEPTH`].
    TooDeep,
    /// More than [`MAX_ITEMS`] members or [`MAX_NODES`] values.
    TooMany,
    /// An object repeats a key.
    DuplicateKey,
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    nodes: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), ParseError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(ParseError::Syntax)
        }
    }

    fn literal(&mut self, lit: &[u8], v: Json) -> Result<Json, ParseError> {
        if self.b.get(self.i..).is_some_and(|r| r.starts_with(lit)) {
            self.i += lit.len();
            Ok(v)
        } else {
            Err(ParseError::Syntax)
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::TooDeep);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ParseError::TooMany);
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b'0'..=b'9' | b'-') => self.number(),
            _ => Err(ParseError::Syntax),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let lead = self.peek();
        let n = self.digits();
        if n == 0 || (lead == Some(b'0') && n > 1) {
            return Err(ParseError::Syntax);
        }
        let mut integral = true;
        if self.peek() == Some(b'.') {
            integral = false;
            self.i += 1;
            if self.digits() == 0 {
                return Err(ParseError::Syntax);
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            integral = false;
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return Err(ParseError::Syntax);
            }
        }
        let text = self
            .b
            .get(start..self.i)
            .and_then(|s| std::str::from_utf8(s).ok())
            .ok_or(ParseError::Syntax)?;
        if integral && let Ok(n) = text.parse::<i64>() {
            return Ok(Json::Int(n));
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Json::Float(f)),
            _ => Err(ParseError::Syntax),
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = self
                .peek()
                .and_then(|c| char::from(c).to_digit(16))
                .ok_or(ParseError::Syntax)?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let hi = self.hex4()?;
        let cp = match hi {
            0xD800..=0xDBFF => {
                self.eat(b'\\')?;
                self.eat(b'u')?;
                let lo = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(ParseError::Syntax);
                }
                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(ParseError::Syntax),
            _ => hi,
        };
        char::from_u32(cp).ok_or(ParseError::Syntax)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.eat(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.peek().ok_or(ParseError::Syntax)?;
            self.i += 1;
            match c {
                b'"' => break,
                0..=0x1f => return Err(ParseError::Syntax),
                b'\\' => {
                    let e = self.peek().ok_or(ParseError::Syntax)?;
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
                        _ => return Err(ParseError::Syntax),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| ParseError::InvalidUtf8)
    }

    fn array(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            if items.len() >= MAX_ITEMS {
                return Err(ParseError::TooMany);
            }
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(ParseError::Syntax),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.eat(b'{')?;
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
                return Err(ParseError::DuplicateKey);
            }
            if items.len() >= MAX_ITEMS {
                return Err(ParseError::TooMany);
            }
            self.skip_ws();
            self.eat(b':')?;
            let v = self.value(depth + 1)?;
            items.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(items));
                }
                _ => return Err(ParseError::Syntax),
            }
        }
    }
}

/// Read one complete JSON document from `bytes`.
///
/// # Errors
/// A [`ParseError`] for anything outside the bounded grammar. Never panics.
pub fn parse_json(bytes: &[u8]) -> Result<Json, ParseError> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ParseError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ParseError::InvalidUtf8)?;
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
        nodes: 0,
    };
    let v = p.value(0)?;
    p.skip_ws();
    if p.peek().is_some() {
        return Err(ParseError::Syntax);
    }
    Ok(v)
}

pub(crate) fn put_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------------------------
// JSON-RPC envelope
// ---------------------------------------------------------------------------------------------

/// A JSON-RPC request id: MCP allows strings and integers, never null.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Id {
    /// Integer id.
    Num(i64),
    /// String id.
    Str(String),
}

impl Id {
    fn encode(&self, out: &mut String) {
        match self {
            Id::Num(n) => {
                let _ = write!(out, "{n}");
            }
            Id::Str(s) => put_str(out, s),
        }
    }
}

/// A JSON-RPC error: a code, a fixed message and optional pre-rendered JSON `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// Numeric code, see [`code`].
    pub code: i64,
    /// Fixed text. Never contains caller-supplied or stored data.
    pub message: &'static str,
    /// Raw JSON for the `data` member, built only from static names.
    pub data: Option<String>,
}

impl RpcError {
    fn new(code: i64, message: &'static str) -> Self {
        Self {
            code,
            message,
            data: None,
        }
    }

    fn field(message: &'static str, field: &'static str) -> Self {
        let mut data = String::from("{\"field\":");
        put_str(&mut data, field);
        data.push('}');
        Self {
            code: code::INVALID_PARAMS,
            message,
            data: Some(data),
        }
    }
}

/// A request that expects a response.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Correlation id.
    pub id: Id,
    /// Method name.
    pub method: String,
    /// `params`, always an object when present.
    pub params: Option<Json>,
}

/// A message that expects no response.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// Method name.
    pub method: String,
    /// `params`, always an object when present.
    pub params: Option<Json>,
}

/// A decoded client message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Needs a response.
    Request(Request),
    /// Needs none.
    Notification(Notification),
}

/// A message that could not be decoded, with the id to answer under when it could be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The request id, when the message got far enough to have a valid one.
    pub id: Option<Id>,
    /// The error to send back.
    pub error: RpcError,
}

fn fail(id: Option<Id>, c: i64, m: &'static str) -> Failure {
    Failure {
        id,
        error: RpcError::new(c, m),
    }
}

/// Decode one JSON-RPC message.
///
/// # Errors
/// A [`Failure`] for malformed JSON (`-32700`), a batch, a response, an unknown envelope member or a
/// bad id or `params` (`-32600`). Never panics.
pub fn decode(bytes: &[u8]) -> Result<Incoming, Failure> {
    let root = parse_json(bytes).map_err(|_| fail(None, code::PARSE_ERROR, "parse error"))?;
    let Json::Obj(members) = root else {
        let m = if matches!(root, Json::Arr(_)) {
            "batch requests are not supported"
        } else {
            "invalid request"
        };
        return Err(fail(None, code::INVALID_REQUEST, m));
    };
    let mut id = None;
    if let Some((_, v)) = members.iter().find(|(k, _)| k == "id") {
        id = match v {
            Json::Int(n) => Some(Id::Num(*n)),
            Json::Str(s) => Some(Id::Str(s.clone())),
            _ => return Err(fail(None, code::INVALID_REQUEST, "invalid request id")),
        };
    }
    let bad = |m| fail(id.clone(), code::INVALID_REQUEST, m);
    let mut method = None;
    let mut params = None;
    let mut version_ok = false;
    for (k, v) in &members {
        match (k.as_str(), v) {
            ("jsonrpc", Json::Str(s)) if s == "2.0" => version_ok = true,
            ("id", _) => {}
            ("method", Json::Str(s)) => method = Some(s.clone()),
            ("params", Json::Obj(_)) => params = Some(v.clone()),
            _ => return Err(bad("invalid request")),
        }
    }
    let (true, Some(method)) = (version_ok, method) else {
        return Err(bad("invalid request"));
    };
    Ok(match id {
        Some(id) => Incoming::Request(Request { id, method, params }),
        None => Incoming::Notification(Notification { method, params }),
    })
}

/// Encode a success response around an already-rendered `result` object.
#[must_use]
pub fn encode_result(id: &Id, result: &str) -> String {
    let mut out = String::from("{\"jsonrpc\":\"2.0\",\"id\":");
    id.encode(&mut out);
    out.push_str(",\"result\":");
    out.push_str(result);
    out.push('}');
    out
}

/// Encode an error response; `id` is `None` (rendered `null`) when the request id was unreadable.
#[must_use]
pub fn encode_error(id: Option<&Id>, e: &RpcError) -> String {
    let mut out = String::from("{\"jsonrpc\":\"2.0\",\"id\":");
    match id {
        Some(id) => id.encode(&mut out),
        None => out.push_str("null"),
    }
    let _ = write!(out, ",\"error\":{{\"code\":{},\"message\":", e.code);
    put_str(&mut out, e.message);
    if let Some(d) = &e.data {
        out.push_str(",\"data\":");
        out.push_str(d);
    }
    out.push_str("}}");
    out
}

// ---------------------------------------------------------------------------------------------
// Tools, schemas and validation
// ---------------------------------------------------------------------------------------------

/// The three tools of v0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `lattice_read`
    Read,
    /// `lattice_write`
    Write,
    /// `lattice_subscribe`
    Subscribe,
}

#[derive(Debug, Clone, Copy)]
enum Fmt {
    /// No control characters at all.
    Key,
    /// No control characters except tab, line feed and carriage return.
    Text,
    Scope,
    Ident,
}

#[derive(Debug, Clone, Copy)]
enum Ty {
    Str { min: usize, max: usize, fmt: Fmt },
    Int { min: i64, max: i64 },
}

#[derive(Debug, Clone, Copy)]
struct Prop {
    name: &'static str,
    ty: Ty,
    required: bool,
    desc: &'static str,
}

const MAX_SAFE_INT: i64 = (1 << 53) - 1;

const SCOPE: Prop = Prop {
    name: "scope",
    ty: Ty::Str {
        min: 1,
        max: MAX_SCOPE_BYTES,
        fmt: Fmt::Scope,
    },
    required: true,
    desc: "Scope path such as workspace/project; must be covered by one of your grants.",
};
const TABLE: Prop = Prop {
    name: "table",
    ty: Ty::Str {
        min: 1,
        max: 64,
        fmt: Fmt::Ident,
    },
    required: true,
    desc: "Table name: ASCII letters, digits and underscore.",
};
const KEY_REQ: Prop = Prop {
    name: "key",
    ty: Ty::Str {
        min: 1,
        max: 256,
        fmt: Fmt::Key,
    },
    required: true,
    desc: "Row key.",
};
const KEY_OPT: Prop = Prop {
    name: "key",
    ty: Ty::Str {
        min: 1,
        max: 256,
        fmt: Fmt::Key,
    },
    required: false,
    desc: "Row key; omit to read a page of rows.",
};
const LIMIT: Prop = Prop {
    name: "limit",
    ty: Ty::Int { min: 1, max: 1000 },
    required: false,
    desc: "Most rows to return (default 100).",
};
const VALUE: Prop = Prop {
    name: "value",
    ty: Ty::Str {
        min: 0,
        max: MAX_VALUE_CHARS,
        fmt: Fmt::Text,
    },
    required: true,
    desc: "Row value as text.",
};
const EXPECTED: Prop = Prop {
    name: "expected_version",
    ty: Ty::Int {
        min: 0,
        max: MAX_SAFE_INT,
    },
    required: false,
    desc: "Write only if the row is at this version.",
};
const AFTER: Prop = Prop {
    name: "after",
    ty: Ty::Int {
        min: 0,
        max: MAX_SAFE_INT,
    },
    required: false,
    desc: "Resume after this cursor (default 0, from the start).",
};

impl Tool {
    /// All tools, in the order `tools/list` returns them.
    pub const ALL: [Tool; 3] = [Tool::Read, Tool::Write, Tool::Subscribe];

    /// The wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Tool::Read => "lattice_read",
            Tool::Write => "lattice_write",
            Tool::Subscribe => "lattice_subscribe",
        }
    }

    /// Look a tool up by wire name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    fn description(self) -> &'static str {
        match self {
            Tool::Read => "Read one row or a page of rows from a table in a scope you may read.",
            Tool::Write => "Write one row to a table in a scope you may write.",
            Tool::Subscribe => "Subscribe to changes of a table in a scope you may read.",
        }
    }

    fn props(self) -> &'static [Prop] {
        match self {
            Tool::Read => &[SCOPE, TABLE, KEY_OPT, LIMIT],
            Tool::Write => &[SCOPE, TABLE, KEY_REQ, VALUE, EXPECTED],
            Tool::Subscribe => &[SCOPE, TABLE, AFTER],
        }
    }

    /// The tool's `inputSchema` as JSON text (JSON Schema 2020-12 subset).
    #[must_use]
    pub fn input_schema(self) -> String {
        let mut out = String::from("{\"type\":\"object\",\"properties\":{");
        for (i, p) in self.props().iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            put_str(&mut out, p.name);
            out.push(':');
            match p.ty {
                Ty::Str { min, max, .. } => {
                    let _ = write!(
                        out,
                        "{{\"type\":\"string\",\"minLength\":{min},\"maxLength\":{max},"
                    );
                }
                Ty::Int { min, max } => {
                    let _ = write!(
                        out,
                        "{{\"type\":\"integer\",\"minimum\":{min},\"maximum\":{max},"
                    );
                }
            }
            out.push_str("\"description\":");
            put_str(&mut out, p.desc);
            out.push('}');
        }
        out.push_str("},\"required\":[");
        let mut first = true;
        for p in self.props().iter().filter(|p| p.required) {
            if !first {
                out.push(',');
            }
            first = false;
            put_str(&mut out, p.name);
        }
        out.push_str("],\"additionalProperties\":false}");
        out
    }
}

/// A validated tool invocation, handed to the [`ToolBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCall {
    /// `lattice_read`
    Read {
        /// Target scope.
        scope: String,
        /// Table.
        table: String,
        /// Row key, or `None` for a page.
        key: Option<String>,
        /// Page size, 1 to 1000 (default 100).
        limit: u32,
    },
    /// `lattice_write`
    Write {
        /// Target scope.
        scope: String,
        /// Table.
        table: String,
        /// Row key.
        key: String,
        /// Row value.
        value: String,
        /// Optimistic version check.
        expected_version: Option<u64>,
    },
    /// `lattice_subscribe`
    Subscribe {
        /// Target scope.
        scope: String,
        /// Table.
        table: String,
        /// Resume cursor.
        after: u64,
    },
}

impl ToolCall {
    /// The tool this call invokes.
    #[must_use]
    pub fn tool(&self) -> Tool {
        match self {
            ToolCall::Read { .. } => Tool::Read,
            ToolCall::Write { .. } => Tool::Write,
            ToolCall::Subscribe { .. } => Tool::Subscribe,
        }
    }

    /// The scope the call targets.
    #[must_use]
    pub fn scope(&self) -> &str {
        match self {
            ToolCall::Read { scope, .. }
            | ToolCall::Write { scope, .. }
            | ToolCall::Subscribe { scope, .. } => scope,
        }
    }
}

/// Whether `s` is a scope path: one or more `/`-separated segments of ASCII letters, digits, `_`
/// and `-`. No empty segments, no `.`, so no traversal.
#[must_use]
pub fn is_valid_scope(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SCOPE_BYTES
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

fn is_ident(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

enum Val<'a> {
    Str(&'a str),
    Int(i64),
}

fn check_prop<'a>(p: &Prop, v: &'a Json) -> Result<Val<'a>, RpcError> {
    match (p.ty, v) {
        (Ty::Str { min, max, fmt }, Json::Str(s)) => {
            // JSON Schema counts characters, not bytes, for minLength and maxLength.
            let chars = s.chars().take(max + 1).count();
            if chars < min || chars > max {
                return Err(RpcError::field("argument out of range", p.name));
            }
            let ok = match fmt {
                Fmt::Key => !s.chars().any(char::is_control),
                Fmt::Text => !s
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')),
                Fmt::Scope => is_valid_scope(s),
                Fmt::Ident => is_ident(s),
            };
            if ok {
                Ok(Val::Str(s))
            } else {
                Err(RpcError::field("argument has invalid format", p.name))
            }
        }
        (Ty::Int { min, max }, Json::Int(_) | Json::Float(_)) => {
            // JSON Schema 2020-12: a number with a zero fractional part (10.0) is an integer.
            let n = match v {
                Json::Int(n) => *n,
                Json::Float(f) if f.fract() == 0.0 && f.abs() <= MAX_SAFE_INT as f64 => *f as i64,
                _ => return Err(RpcError::field("argument has wrong type", p.name)),
            };
            if (min..=max).contains(&n) {
                Ok(Val::Int(n))
            } else {
                Err(RpcError::field("argument out of range", p.name))
            }
        }
        _ => Err(RpcError::field("argument has wrong type", p.name)),
    }
}

/// Validate `args` against the tool's schema and build the typed call.
///
/// # Errors
/// `-32602` with a fixed message and, where a schema property is at fault, `data.field` naming it.
/// Caller-supplied names and values are never echoed.
pub fn validate_args(tool: Tool, args: &Json) -> Result<ToolCall, RpcError> {
    let Json::Obj(members) = args else {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            "arguments must be an object",
        ));
    };
    let props = tool.props();
    if members
        .iter()
        .any(|(k, _)| !props.iter().any(|p| p.name == k))
    {
        return Err(RpcError::new(code::INVALID_PARAMS, "unknown argument"));
    }
    let mut vals: Vec<(&'static str, Val<'_>)> = Vec::new();
    for p in props {
        match args.get(p.name) {
            Some(v) => vals.push((p.name, check_prop(p, v)?)),
            None if p.required => return Err(RpcError::field("missing required argument", p.name)),
            None => {}
        }
    }
    let s = |name: &str| {
        vals.iter().find_map(|(n, v)| match v {
            Val::Str(s) if *n == name => Some((*s).to_owned()),
            _ => None,
        })
    };
    let n = |name: &str| {
        vals.iter().find_map(|(k, v)| match v {
            Val::Int(i) if *k == name => Some(*i),
            _ => None,
        })
    };
    let internal = || RpcError::new(code::INTERNAL_ERROR, "internal error");
    let (scope, table) = (
        s("scope").ok_or_else(internal)?,
        s("table").ok_or_else(internal)?,
    );
    Ok(match tool {
        Tool::Read => ToolCall::Read {
            scope,
            table,
            key: s("key"),
            limit: n("limit")
                .and_then(|l| u32::try_from(l).ok())
                .unwrap_or(100),
        },
        Tool::Write => ToolCall::Write {
            scope,
            table,
            key: s("key").ok_or_else(internal)?,
            value: s("value").ok_or_else(internal)?,
            expected_version: n("expected_version").and_then(|v| u64::try_from(v).ok()),
        },
        Tool::Subscribe => ToolCall::Subscribe {
            scope,
            table,
            after: n("after").and_then(|v| u64::try_from(v).ok()).unwrap_or(0),
        },
    })
}

// ---------------------------------------------------------------------------------------------
// Callers and grants
// ---------------------------------------------------------------------------------------------

/// Permission to use one tool on one scope subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    tool: Tool,
    /// `None` means every scope.
    scope: Option<String>,
}

impl Grant {
    /// A grant for `tool` on `scope` and everything under it (`a/b` covers `a/b` and `a/b/c`, not
    /// `a/bc`). `"*"` covers every scope. `None` when `scope` is neither `"*"` nor a valid scope.
    #[must_use]
    pub fn new(tool: Tool, scope: &str) -> Option<Grant> {
        if scope == "*" {
            return Some(Grant { tool, scope: None });
        }
        is_valid_scope(scope).then(|| Grant {
            tool,
            scope: Some(scope.to_owned()),
        })
    }

    fn covers(&self, tool: Tool, target: &str) -> bool {
        self.tool == tool
            && match &self.scope {
                None => true,
                Some(g) => target
                    .strip_prefix(g.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/')),
            }
    }
}

/// Why a call was refused by the scoping layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// No grant for the tool at all.
    ToolNotGranted,
    /// The tool is granted, but not for the target scope.
    ScopeDenied,
}

/// The authenticated party on whose behalf a message is handled. Authentication itself is out of
/// scope: the transport supplies this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// Opaque caller identity, passed to the backend for its row policy.
    pub id: String,
    /// What the caller may do.
    pub grants: Vec<Grant>,
}

impl Caller {
    /// Whether any grant names `tool` (decides what `tools/list` shows).
    #[must_use]
    pub fn may_use(&self, tool: Tool) -> bool {
        self.grants.iter().any(|g| g.tool == tool)
    }

    /// Check a call on `scope`.
    ///
    /// # Errors
    /// [`Denial`] naming which layer refused.
    pub fn check(&self, tool: Tool, scope: &str) -> Result<(), Denial> {
        if !self.may_use(tool) {
            Err(Denial::ToolNotGranted)
        } else if self.grants.iter().any(|g| g.covers(tool, scope)) {
            Ok(())
        } else {
            Err(Denial::ScopeDenied)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Backend and server
// ---------------------------------------------------------------------------------------------

/// What a backend returns for a successful call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    text: String,
}

/// The text given to [`ToolOutput::new`] exceeds [`MAX_OUTPUT_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputTooLarge;

/// What [`ToolOutput::or_notice`] returns instead of an oversized text.
pub const OUTPUT_OMITTED: &str = "output omitted: too large";

impl ToolOutput {
    /// Wrap result text (JSON by convention). The bound is enforced here, in the backend's hands,
    /// so an output cannot be rejected after the backend has already acted.
    ///
    /// # Errors
    /// [`OutputTooLarge`] when `text` is longer than [`MAX_OUTPUT_BYTES`].
    pub fn new(text: String) -> Result<Self, OutputTooLarge> {
        if text.len() > MAX_OUTPUT_BYTES {
            Err(OutputTooLarge)
        } else {
            Ok(Self { text })
        }
    }

    /// Like [`ToolOutput::new`], but an oversized text becomes the fixed [`OUTPUT_OMITTED`] notice.
    /// A backend that has already committed a write returns this, so the client sees success and
    /// does not retry an applied write.
    #[must_use]
    pub fn or_notice(text: String) -> Self {
        Self::new(text).unwrap_or_else(|_| Self {
            text: OUTPUT_OMITTED.to_owned(),
        })
    }

    /// The result text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Why a backend could not complete a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendError {
    /// The row policy refused (reported as [`code::SCOPE_DENIED`]).
    PolicyDenied,
    /// An optimistic-version conflict; reported as a tool execution error (`isError: true`) so the
    /// model can retry.
    Conflict,
    /// The store is unavailable (reported as [`code::BACKEND_UNAVAILABLE`]).
    Unavailable,
}

/// The store behind the tools. It is reached only with a validated call that the caller's grants
/// allow; it still owns row-level policy and receives the caller identity for it.
///
/// The server never turns a successful return into an error, so an applied write is never reported
/// as failed. A backend must therefore bound its output *before* it commits (or use
/// [`ToolOutput::or_notice`] after), because [`ToolOutput`] cannot hold more than
/// [`MAX_OUTPUT_BYTES`].
pub trait ToolBackend {
    /// Execute `call` for `caller`.
    ///
    /// # Errors
    /// A [`BackendError`].
    fn call(&mut self, caller: &Caller, call: &ToolCall) -> Result<ToolOutput, BackendError>;
}

/// Which era a connection speaks. The only per-connection state of the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Revision 2026-07-28: every request carries its own `_meta`. The initial mode.
    Stateless,
    /// The connection opened with a successful `initialize` (2025-11-25 semantics): `_meta` is
    /// optional. Set only by `initialize`, never by a missing `_meta`, and kept for the life of the
    /// [`Server`] (one server per connection).
    Legacy,
}

impl Mode {
    fn accepts(self, version: &str) -> bool {
        version == PROTOCOL_VERSION || (self == Mode::Legacy && version == LEGACY_PROTOCOL_VERSION)
    }

    fn supported_json(self) -> String {
        match self {
            Mode::Stateless => format!("[\"{PROTOCOL_VERSION}\"]"),
            Mode::Legacy => format!("[\"{PROTOCOL_VERSION}\",\"{LEGACY_PROTOCOL_VERSION}\"]"),
        }
    }
}

/// The MCP server state machine for one connection: a backend and the connection [`Mode`].
#[derive(Debug)]
pub struct Server<B> {
    backend: B,
    mode: Mode,
}

fn supported_versions_data(mode: Mode, requested: Option<&str>) -> String {
    let mut d = format!("{{\"supported\":{}", mode.supported_json());
    // Echo only a short, plain version-looking string; never arbitrary caller text.
    if let Some(r) = requested
        && r.len() <= 32
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        d.push_str(",\"requested\":");
        put_str(&mut d, r);
    }
    d.push('}');
    d
}

const COMPLETE: &str = "{\"resultType\":\"complete\"";
const SERVER_INFO: &str = "{\"name\":\"lattice\",\"version\":\"0\"}";

impl<B: ToolBackend> Server<B> {
    /// A server over `backend`.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            mode: Mode::Stateless,
        }
    }

    /// The connection mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The backend, for inspection.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Take the backend back.
    #[must_use]
    pub fn into_backend(self) -> B {
        self.backend
    }

    /// Handle one message from `caller`. Returns the response bytes, or `None` for a notification.
    /// Never panics, whatever the bytes.
    pub fn handle(&mut self, caller: &Caller, message: &[u8]) -> Option<String> {
        match decode(message) {
            Err(f) => Some(encode_error(f.id.as_ref(), &f.error)),
            Ok(Incoming::Notification(_)) => None,
            Ok(Incoming::Request(r)) => Some(match self.dispatch(caller, &r) {
                Ok(result) => encode_result(&r.id, &result),
                Err(e) => encode_error(Some(&r.id), &e),
            }),
        }
    }

    fn dispatch(&mut self, caller: &Caller, r: &Request) -> Result<String, RpcError> {
        let known = matches!(
            r.method.as_str(),
            "ping" | "server/discover" | "tools/list" | "tools/call"
        );
        if r.method == "initialize" {
            let out = Self::initialize(r.params.as_ref())?;
            self.mode = Mode::Legacy;
            return Ok(out);
        }
        if !known {
            return Err(RpcError::new(code::METHOD_NOT_FOUND, "method not found"));
        }
        check_meta(self.mode, r.params.as_ref())?;
        match r.method.as_str() {
            "ping" => Ok(format!("{COMPLETE}}}")),
            "server/discover" => Ok(format!(
                "{COMPLETE},\"supportedVersions\":{},\
                 \"capabilities\":{{\"tools\":{{}}}},\
                 \"_meta\":{{\"io.modelcontextprotocol/serverInfo\":{SERVER_INFO}}}}}",
                self.mode.supported_json()
            )),
            "tools/list" => Ok(Self::list(caller)),
            _ => self.call(caller, r.params.as_ref()),
        }
    }

    fn initialize(params: Option<&Json>) -> Result<String, RpcError> {
        let ok = params.is_some_and(|p| {
            matches!(p.get("protocolVersion"), Some(Json::Str(_)))
                && matches!(p.get("capabilities"), Some(Json::Obj(_)))
                && matches!(p.get("clientInfo"), Some(Json::Obj(_)))
        });
        if !ok {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "invalid initialize parameters",
            ));
        }
        Ok(format!(
            "{COMPLETE},\"protocolVersion\":\"{LEGACY_PROTOCOL_VERSION}\",\
             \"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},\"serverInfo\":{SERVER_INFO}}}"
        ))
    }

    fn list(caller: &Caller) -> String {
        let mut out = format!("{COMPLETE},\"tools\":[");
        let mut first = true;
        for t in Tool::ALL.into_iter().filter(|t| caller.may_use(*t)) {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str("{\"name\":");
            put_str(&mut out, t.name());
            out.push_str(",\"description\":");
            put_str(&mut out, t.description());
            out.push_str(",\"inputSchema\":");
            out.push_str(&t.input_schema());
            out.push('}');
        }
        out.push_str("]}");
        out
    }

    fn call(&mut self, caller: &Caller, params: Option<&Json>) -> Result<String, RpcError> {
        let Some(Json::Obj(members)) = params else {
            return Err(RpcError::new(code::INVALID_PARAMS, "invalid parameters"));
        };
        if members
            .iter()
            .any(|(k, _)| !matches!(k.as_str(), "name" | "arguments" | "_meta"))
        {
            return Err(RpcError::new(code::INVALID_PARAMS, "unknown parameter"));
        }
        let Some(Json::Str(name)) = params.and_then(|p| p.get("name")) else {
            return Err(RpcError::field("missing required argument", "name"));
        };
        let tool = Tool::parse(name).ok_or(RpcError::new(code::INVALID_PARAMS, "unknown tool"))?;
        if !caller.may_use(tool) {
            return Err(RpcError::new(code::TOOL_NOT_GRANTED, "tool not granted"));
        }
        let empty = Json::Obj(Vec::new());
        let args = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);
        let call = validate_args(tool, args)?;
        match caller.check(tool, call.scope()) {
            Ok(()) => {}
            Err(Denial::ToolNotGranted) => {
                return Err(RpcError::new(code::TOOL_NOT_GRANTED, "tool not granted"));
            }
            Err(Denial::ScopeDenied) => {
                return Err(RpcError::new(code::SCOPE_DENIED, "scope denied"));
            }
        }
        let (text, is_error) = match self.backend.call(caller, &call) {
            Ok(o) => (o.text, false),
            Err(BackendError::PolicyDenied) => {
                return Err(RpcError::new(code::SCOPE_DENIED, "scope denied"));
            }
            Err(BackendError::Unavailable) => {
                return Err(RpcError::new(
                    code::BACKEND_UNAVAILABLE,
                    "backend unavailable",
                ));
            }
            Err(BackendError::Conflict) => ("version conflict".to_owned(), true),
        };
        let mut out = format!("{COMPLETE},\"content\":[{{\"type\":\"text\",\"text\":");
        put_str(&mut out, &text);
        let _ = write!(out, "}}],\"isError\":{is_error}}}");
        Ok(out)
    }
}

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";

/// Check the per-request `_meta` of the revision (2026-07-28).
///
/// `Stateless` (a connection that has not sent `initialize`): `_meta` must be an object holding the
/// protocol version (exactly [`PROTOCOL_VERSION`]) and `clientCapabilities` (an object); a missing
/// or mistyped member is `-32602`, another version is `-32022`. A missing `_meta` never selects
/// legacy behaviour. `Legacy` (the connection opened with `initialize`): `_meta` is optional, but if
/// present it must be an object and any version in it must be one of the two revisions.
fn check_meta(mode: Mode, params: Option<&Json>) -> Result<(), RpcError> {
    let meta = params.and_then(|p| p.get("_meta"));
    match (meta, mode) {
        (None, Mode::Legacy) => return Ok(()),
        (None, Mode::Stateless) => {
            return Err(RpcError::field("missing required _meta", "_meta"));
        }
        (Some(Json::Obj(_)), _) => {}
        (Some(_), _) => return Err(RpcError::field("_meta must be an object", "_meta")),
    }
    let version = meta.and_then(|m| m.get(META_VERSION));
    let caps = meta.and_then(|m| m.get(META_CAPS));
    match (version, mode) {
        (None, Mode::Legacy) => {}
        (None, Mode::Stateless) => {
            return Err(RpcError::field(
                "missing required _meta field",
                META_VERSION,
            ));
        }
        (Some(Json::Str(s)), _) if mode.accepts(s) => {}
        (Some(other), _) => {
            return Err(RpcError {
                code: code::UNSUPPORTED_PROTOCOL_VERSION,
                message: "unsupported protocol version",
                data: Some(supported_versions_data(
                    mode,
                    match other {
                        Json::Str(s) => Some(s.as_str()),
                        _ => None,
                    },
                )),
            });
        }
    }
    match (caps, mode) {
        (Some(Json::Obj(_)), _) | (None, Mode::Legacy) => Ok(()),
        (None, Mode::Stateless) => Err(RpcError::field("missing required _meta field", META_CAPS)),
        (Some(_), _) => Err(RpcError::field("_meta field has wrong type", META_CAPS)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Records every call; returns a canned reply.
    #[derive(Default)]
    struct Mock {
        calls: Vec<(String, ToolCall)>,
        reply: Option<Result<ToolOutput, BackendError>>,
        applied: u32,
        oversize: bool,
    }

    impl ToolBackend for Mock {
        fn call(&mut self, caller: &Caller, call: &ToolCall) -> Result<ToolOutput, BackendError> {
            self.calls.push((caller.id.clone(), call.clone()));
            if matches!(call, ToolCall::Write { .. }) {
                // The write is committed here, before the output is built.
                self.applied += 1;
            }
            if self.oversize {
                return Ok(ToolOutput::or_notice("x".repeat(MAX_OUTPUT_BYTES + 1)));
            }
            self.reply
                .clone()
                .unwrap_or_else(|| Ok(ToolOutput::or_notice("{\"ok\":true}".to_owned())))
        }
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#;
    const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

    /// A server on a connection that opened with `initialize` (dual-era, legacy semantics).
    fn legacy(mock: Mock) -> Server<Mock> {
        let mut s = Server::new(mock);
        let out = s.handle(&caller(&[]), INIT.as_bytes());
        assert!(out.is_some_and(|o| o.contains("\"result\"")));
        assert_eq!(s.mode(), Mode::Legacy);
        s
    }

    fn caller(grants: &[(Tool, &str)]) -> Caller {
        Caller {
            id: "agent-1".to_owned(),
            grants: grants
                .iter()
                .map(|(t, s)| Grant::new(*t, s).expect("valid grant"))
                .collect(),
        }
    }

    fn all(scope: &str) -> Caller {
        caller(&[
            (Tool::Read, scope),
            (Tool::Write, scope),
            (Tool::Subscribe, scope),
        ])
    }

    fn run(c: &Caller, msg: &str) -> (Option<String>, Server<Mock>) {
        let mut s = legacy(Mock::default());
        let out = s.handle(c, msg.as_bytes());
        (out, s)
    }

    fn call_msg(name: &str, args: &str) -> String {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{{\"name\":\"{name}\",\"arguments\":{args}}}}}"
        )
    }

    // ---- byte vectors -------------------------------------------------------------------

    #[test]
    fn vector_ping() {
        let (out, _) = run(&all("a"), r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        assert_eq!(
            out.as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}}"#)
        );
    }

    #[test]
    fn vector_initialize_and_initialized() {
        let msg = r#"{"jsonrpc":"2.0","id":"i","method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#;
        let (out, _) = run(&all("a"), msg);
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":"i","result":{"resultType":"complete","protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"lattice","version":"0"}}}"#
            )
        );
        let (out, _) = run(
            &all("a"),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert_eq!(out, None);
        // initialize without its required members is refused.
        let (out, _) = run(
            &all("a"),
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#,
        );
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"invalid initialize parameters"}}"#
            )
        );
    }

    #[test]
    fn vector_discover() {
        let (out, _) = run(
            &all("a"),
            r#"{"jsonrpc":"2.0","id":3,"method":"server/discover"}"#,
        );
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","supportedVersions":["2026-07-28","2025-11-25"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"lattice","version":"0"}}}}"#
            )
        );
    }

    #[test]
    fn vector_tools_list_filters_by_grant() {
        let c = caller(&[(Tool::Subscribe, "a")]);
        let (out, _) = run(&c, r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#);
        let expected = concat!(
            r#"{"jsonrpc":"2.0","id":4,"result":{"resultType":"complete","tools":[{"name":"lattice_subscribe","#,
            r#""description":"Subscribe to changes of a table in a scope you may read.","inputSchema":{"type":"object","#,
            r#""properties":{"scope":{"type":"string","minLength":1,"maxLength":128,"description":"Scope path such as workspace/project; must be covered by one of your grants."},"#,
            r#""table":{"type":"string","minLength":1,"maxLength":64,"description":"Table name: ASCII letters, digits and underscore."},"#,
            r#""after":{"type":"integer","minimum":0,"maximum":9007199254740991,"description":"Resume after this cursor (default 0, from the start)."}},"#,
            r#""required":["scope","table"],"additionalProperties":false}}]}}"#
        );
        assert_eq!(out.as_deref(), Some(expected));
        // Nothing granted: an empty list, not an error.
        let (out, _) = run(
            &caller(&[]),
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#,
        );
        assert_eq!(
            out.as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":4,"result":{"resultType":"complete","tools":[]}}"#)
        );
        // All three, in fixed order, each schema a parseable object.
        let (out, _) = run(
            &all("a"),
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#,
        );
        let v = parse_json(out.expect("response").as_bytes()).expect("json");
        let Some(Json::Arr(tools)) = v.get("result").and_then(|r| r.get("tools")) else {
            panic!("tools array");
        };
        let names: Vec<_> = tools.iter().filter_map(|t| t.get("name")).collect();
        assert_eq!(
            names,
            [
                &Json::Str("lattice_read".into()),
                &Json::Str("lattice_write".into()),
                &Json::Str("lattice_subscribe".into())
            ]
        );
    }

    #[test]
    fn vector_tools_call_success_and_forms() {
        let (out, s) = run(
            &all("team/a"),
            &call_msg(
                "lattice_read",
                r#"{"scope":"team/a/p","table":"items","key":"k1"}"#,
            ),
        );
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":7,"result":{"resultType":"complete","content":[{"type":"text","text":"{\"ok\":true}"}],"isError":false}}"#
            )
        );
        assert_eq!(
            s.backend().calls,
            [(
                "agent-1".to_owned(),
                ToolCall::Read {
                    scope: "team/a/p".into(),
                    table: "items".into(),
                    key: Some("k1".into()),
                    limit: 100
                }
            )]
        );
        let (_, s) = run(
            &all("team"),
            &call_msg(
                "lattice_write",
                r#"{"scope":"team","table":"t","key":"k","value":"v","expected_version":3}"#,
            ),
        );
        assert_eq!(
            s.backend().calls.first().map(|c| &c.1),
            Some(&ToolCall::Write {
                scope: "team".into(),
                table: "t".into(),
                key: "k".into(),
                value: "v".into(),
                expected_version: Some(3)
            })
        );
        let (_, s) = run(
            &all("team"),
            &call_msg(
                "lattice_subscribe",
                r#"{"scope":"team","table":"t","after":9}"#,
            ),
        );
        assert_eq!(
            s.backend().calls.first().map(|c| &c.1),
            Some(&ToolCall::Subscribe {
                scope: "team".into(),
                table: "t".into(),
                after: 9
            })
        );
        // A version conflict is a tool execution error, not a protocol error.
        let mut s = legacy(Mock {
            reply: Some(Err(BackendError::Conflict)),
            ..Mock::default()
        });
        let out = s.handle(
            &all("team"),
            call_msg(
                "lattice_write",
                r#"{"scope":"team","table":"t","key":"k","value":"v"}"#,
            )
            .as_bytes(),
        );
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":7,"result":{"resultType":"complete","content":[{"type":"text","text":"version conflict"}],"isError":true}}"#
            )
        );
    }

    #[test]
    fn vector_protocol_errors() {
        let c = all("a");
        let cases: &[(&str, &str)] = &[
            (
                "",
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#,
            ),
            (
                "{",
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#,
            ),
            (
                r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"batch requests are not supported"}}"#,
            ),
            (
                "42",
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#,
            ),
            (
                r#"{"jsonrpc":"1.0","id":5,"method":"ping"}"#,
                r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32600,"message":"invalid request"}}"#,
            ),
            (
                r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request id"}}"#,
            ),
            (
                r#"{"jsonrpc":"2.0","id":6,"result":{}}"#,
                r#"{"jsonrpc":"2.0","id":6,"error":{"code":-32600,"message":"invalid request"}}"#,
            ),
            (
                r#"{"jsonrpc":"2.0","id":8,"method":"resources/list"}"#,
                r#"{"jsonrpc":"2.0","id":8,"error":{"code":-32601,"message":"method not found"}}"#,
            ),
            (
                r#"{"jsonrpc":"2.0","id":9,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01"}}}"#,
                r#"{"jsonrpc":"2.0","id":9,"error":{"code":-32022,"message":"unsupported protocol version","data":{"supported":["2026-07-28","2025-11-25"],"requested":"1900-01-01"}}}"#,
            ),
            (
                r#"{"jsonrpc":"2.0","id":10,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
                r#"{"jsonrpc":"2.0","id":10,"result":{"resultType":"complete"}}"#,
            ),
        ];
        for (input, want) in cases {
            let (out, s) = run(&c, input);
            assert_eq!(out.as_deref(), Some(*want), "input {input}");
            assert!(s.backend().calls.is_empty());
        }
        // Unknown notifications are swallowed, never answered.
        let (out, _) = run(&c, r#"{"jsonrpc":"2.0","method":"nope/whatever"}"#);
        assert_eq!(out, None);
        // An echoed version that looks like text is dropped from the error data.
        let (out, _) = run(
            &c,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"a b\"c"}}}"#,
        );
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"unsupported protocol version","data":{"supported":["2026-07-28","2025-11-25"]}}}"#
            )
        );
    }

    #[test]
    fn json_limits_and_strictness() {
        for bad in [
            "{\"a\":1,\"a\":2}",
            "[1,]",
            "01",
            "1.",
            "-",
            "\"\\ud800\"",
            "\"\x01\"",
            "nul",
            "{} {}",
            "\u{feff}{}",
        ] {
            assert!(parse_json(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_json(b" [ -0 , 1.5e2 , -7 ] "),
            Ok(Json::Arr(vec![
                Json::Int(0),
                Json::Float(150.0),
                Json::Int(-7)
            ]))
        );
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 2), "]".repeat(MAX_DEPTH + 2));
        assert_eq!(parse_json(deep.as_bytes()), Err(ParseError::TooDeep));
        let wide = format!("[{}]", "0,".repeat(MAX_ITEMS) + "0");
        assert_eq!(parse_json(wide.as_bytes()), Err(ParseError::TooMany));
        let big = vec![b' '; MAX_MESSAGE_BYTES + 1];
        assert_eq!(parse_json(&big), Err(ParseError::TooLarge));
        assert_eq!(parse_json(&[0xff]), Err(ParseError::InvalidUtf8));
        // Too many nodes without tripping the per-array limit.
        let nodes = format!("[{}]", vec!["[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]"; 64].join(","));
        assert_eq!(parse_json(nodes.as_bytes()), Err(ParseError::TooMany));
    }

    // ---- schema validation ---------------------------------------------------------------

    #[test]
    fn schema_validation_table() {
        let c = all("a");
        let huge = format!(
            "{{\"scope\":\"a\",\"table\":\"t\",\"key\":\"k\",\"value\":\"{}\"}}",
            "x".repeat(MAX_VALUE_CHARS + 1)
        );
        let cases: Vec<(&str, String, &str, Option<&str>)> = vec![
            (
                "lattice_read",
                r#"{"table":"t"}"#.into(),
                "missing required argument",
                Some("scope"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a"}"#.into(),
                "missing required argument",
                Some("table"),
            ),
            (
                "lattice_read",
                r#"{"scope":1,"table":"t"}"#.into(),
                "argument has wrong type",
                Some("scope"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a","table":"t","limit":"5"}"#.into(),
                "argument has wrong type",
                Some("limit"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a","table":"t","limit":1.5}"#.into(),
                "argument has wrong type",
                Some("limit"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a","table":"t","limit":0}"#.into(),
                "argument out of range",
                Some("limit"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a","table":"t","limit":1001}"#.into(),
                "argument out of range",
                Some("limit"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a","table":"t","extra":1}"#.into(),
                "unknown argument",
                None,
            ),
            (
                "lattice_read",
                r#"{"scope":"a/../b","table":"t"}"#.into(),
                "argument has invalid format",
                Some("scope"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a//b","table":"t"}"#.into(),
                "argument has invalid format",
                Some("scope"),
            ),
            (
                "lattice_read",
                r#"{"scope":"*","table":"t"}"#.into(),
                "argument has invalid format",
                Some("scope"),
            ),
            (
                "lattice_read",
                r#"{"scope":"a","table":"t; DROP"}"#.into(),
                "argument has invalid format",
                Some("table"),
            ),
            (
                "lattice_read",
                r#"{"scope":"","table":"t"}"#.into(),
                "argument out of range",
                Some("scope"),
            ),
            (
                "lattice_read",
                "[]".into(),
                "arguments must be an object",
                None,
            ),
            (
                "lattice_write",
                r#"{"scope":"a","table":"t","key":"k"}"#.into(),
                "missing required argument",
                Some("value"),
            ),
            (
                "lattice_write",
                huge,
                "argument out of range",
                Some("value"),
            ),
            (
                "lattice_write",
                r#"{"scope":"a","table":"t","key":"k","value":"v","expected_version":-1}"#.into(),
                "argument out of range",
                Some("expected_version"),
            ),
            (
                "lattice_subscribe",
                r#"{"scope":"a","table":"t","after":null}"#.into(),
                "argument has wrong type",
                Some("after"),
            ),
        ];
        for (tool, args, msg, field) in cases {
            let (out, s) = run(&c, &call_msg(tool, &args));
            let v = parse_json(out.expect("response").as_bytes()).expect("json");
            let err = v.get("error").expect("error");
            assert_eq!(
                err.get("code"),
                Some(&Json::Int(-32602)),
                "{tool} {args:.80}"
            );
            assert_eq!(
                err.get("message"),
                Some(&Json::Str(msg.into())),
                "{tool} {args:.80}"
            );
            let got = err.get("data").and_then(|d| d.get("field"));
            assert_eq!(
                got,
                field.map(|f| Json::Str(f.into())).as_ref(),
                "{tool} {args:.80}"
            );
            assert!(
                s.backend().calls.is_empty(),
                "backend reached for {args:.80}"
            );
        }
    }

    #[test]
    fn call_params_shape_errors() {
        let c = all("a");
        for (msg, want) in [
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#,
                "invalid parameters",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
                "missing required argument",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":3}}"#,
                "missing required argument",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"lattice_nope"}}"#,
                "unknown tool",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"lattice_read","x":1}}"#,
                "unknown parameter",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"lattice_read"}}"#,
                "missing required argument",
            ),
        ] {
            let (out, s) = run(&c, msg);
            assert!(out.expect("response").contains(want), "{msg}");
            assert!(s.backend().calls.is_empty());
        }
    }

    // ---- grant enforcement ---------------------------------------------------------------

    #[test]
    fn grant_enforcement_table() {
        // (grants, tool, target scope, expected error code or None for allowed)
        let r = Tool::Read;
        let w = Tool::Write;
        type Row<'a> = (Vec<(Tool, &'a str)>, Tool, &'a str, Option<i64>);
        let cases: Vec<Row<'_>> = vec![
            (vec![(r, "team/a")], r, "team/a", None),
            (vec![(r, "team/a")], r, "team/a/proj", None),
            (vec![(r, "*")], r, "anything/at/all", None),
            (vec![(r, "team/a"), (w, "team/b")], w, "team/b/x", None),
            (vec![(r, "team/a")], r, "team/b", Some(code::SCOPE_DENIED)),
            (vec![(r, "team/a")], r, "team/ab", Some(code::SCOPE_DENIED)),
            (vec![(r, "team/a")], r, "team", Some(code::SCOPE_DENIED)),
            (
                vec![(r, "team/a")],
                w,
                "team/a",
                Some(code::TOOL_NOT_GRANTED),
            ),
            (
                vec![(r, "team/a"), (w, "team/b")],
                w,
                "team/a",
                Some(code::SCOPE_DENIED),
            ),
            (vec![], r, "team/a", Some(code::TOOL_NOT_GRANTED)),
        ];
        for (grants, tool, scope, want) in cases {
            let c = caller(&grants);
            let args = match tool {
                Tool::Write => format!(
                    "{{\"scope\":\"{scope}\",\"table\":\"t\",\"key\":\"k\",\"value\":\"v\"}}"
                ),
                _ => format!("{{\"scope\":\"{scope}\",\"table\":\"t\"}}"),
            };
            let (out, s) = run(&c, &call_msg(tool.name(), &args));
            let v = parse_json(out.expect("response").as_bytes()).expect("json");
            match want {
                None => {
                    assert!(v.get("result").is_some(), "{grants:?} {scope}");
                    assert_eq!(s.backend().calls.len(), 1);
                }
                Some(code) => {
                    assert_eq!(
                        v.get("error").and_then(|e| e.get("code")),
                        Some(&Json::Int(code)),
                        "{grants:?} {scope}"
                    );
                    assert!(s.backend().calls.is_empty(), "backend reached on denial");
                }
            }
        }
        assert!(Grant::new(Tool::Read, "a/../b").is_none());
        assert!(Grant::new(Tool::Read, "").is_none());
    }

    #[test]
    fn denial_leaks_nothing() {
        let c = caller(&[(Tool::Read, "team/a")]);
        let (out, _) = run(
            &c,
            &call_msg(
                "lattice_read",
                r#"{"scope":"secret/vault","table":"payroll"}"#,
            ),
        );
        let out = out.expect("response");
        assert!(
            !out.contains("secret") && !out.contains("vault") && !out.contains("payroll"),
            "{out}"
        );
        assert!(!out.contains("team"), "{out}");
        assert_eq!(
            out,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":40302,"message":"scope denied"}}"#
        );
    }

    #[test]
    fn backend_failures_map_to_stable_codes() {
        for (reply, want) in [
            (
                BackendError::PolicyDenied,
                r#""code":40302,"message":"scope denied""#,
            ),
            (
                BackendError::Unavailable,
                r#""code":50001,"message":"backend unavailable""#,
            ),
        ] {
            let mut s = legacy(Mock {
                reply: Some(Err(reply)),
                ..Mock::default()
            });
            let out = s.handle(
                &all("a"),
                call_msg("lattice_read", r#"{"scope":"a","table":"t"}"#).as_bytes(),
            );
            assert!(out.expect("response").contains(want));
        }
    }

    #[test]
    fn oversize_output_never_turns_an_applied_write_into_an_error() {
        assert_eq!(
            ToolOutput::new("x".repeat(MAX_OUTPUT_BYTES + 1)),
            Err(OutputTooLarge)
        );
        assert!(ToolOutput::new("x".repeat(MAX_OUTPUT_BYTES)).is_ok());
        let mut s = legacy(Mock {
            oversize: true,
            ..Mock::default()
        });
        let out = s.handle(
            &all("a"),
            call_msg(
                "lattice_write",
                r#"{"scope":"a","table":"t","key":"k","value":"v"}"#,
            )
            .as_bytes(),
        );
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":7,"result":{"resultType":"complete","content":[{"type":"text","text":"output omitted: too large"}],"isError":false}}"#
            )
        );
        // Applied exactly once, and reported as success: a client has nothing to retry.
        assert_eq!(s.backend().applied, 1);
        assert_eq!(s.backend().calls.len(), 1);
    }

    #[test]
    fn control_characters_are_refused_in_key_and_value() {
        let c = all("a");
        for (args, field) in [
            (
                r#"{"scope":"a","table":"t","key":"k\u0000x","value":"v"}"#,
                "key",
            ),
            (
                r#"{"scope":"a","table":"t","key":"k\n","value":"v"}"#,
                "key",
            ),
            (
                r#"{"scope":"a","table":"t","key":"k\u007f","value":"v"}"#,
                "key",
            ),
            (
                r#"{"scope":"a","table":"t","key":"k","value":"v\u0000"}"#,
                "value",
            ),
            (
                r#"{"scope":"a","table":"t","key":"k","value":"v\u001b[0m"}"#,
                "value",
            ),
        ] {
            let (out, s) = run(&c, &call_msg("lattice_write", args));
            let out = out.expect("response");
            assert!(out.contains("argument has invalid format"), "{args}: {out}");
            assert!(out.contains(&format!("\"field\":\"{field}\"")), "{args}");
            assert!(s.backend().calls.is_empty());
        }
        // Tab, newline and carriage return stay legal in a value.
        let (out, s) = run(
            &c,
            &call_msg(
                "lattice_write",
                r#"{"scope":"a","table":"t","key":"k","value":"a\tb\nc\r\n"}"#,
            ),
        );
        assert!(out.expect("response").contains("\"result\""));
        assert_eq!(s.backend().applied, 1);
    }

    #[test]
    fn lengths_count_characters_and_integers_accept_ten_point_zero() {
        let c = all("a");
        // 65536 two-byte characters are within maxLength 65536 even though they are 128 KiB.
        let ok = format!(
            r#"{{"scope":"a","table":"t","key":"k","value":"{}"}}"#,
            "\u{e9}".repeat(MAX_VALUE_CHARS)
        );
        let (out, s) = run(&c, &call_msg("lattice_write", &ok));
        assert!(out.expect("response").contains("\"result\""));
        assert_eq!(s.backend().applied, 1);
        let (out, _) = run(
            &c,
            &call_msg("lattice_read", r#"{"scope":"a","table":"t","limit":10.0}"#),
        );
        assert!(out.expect("response").contains("\"result\""));
        let (out, s) = run(
            &c,
            &call_msg("lattice_read", r#"{"scope":"a","table":"t","limit":1e1}"#),
        );
        assert!(out.expect("response").contains("\"result\""));
        assert!(matches!(
            s.backend().calls.first().map(|c| &c.1),
            Some(ToolCall::Read { limit: 10, .. })
        ));
    }

    // ---- per-request _meta and connection mode ------------------------------------------

    fn modern(method: &str, extra: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{{META}{extra}}}}}"#)
    }

    #[test]
    fn stateless_requests_must_carry_meta() {
        let c = all("a");
        let call = |meta: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{{meta}"name":"lattice_read","arguments":{{"scope":"a","table":"t"}}}}}}"#
            )
        };
        let v = r#""io.modelcontextprotocol/protocolVersion":"2026-07-28""#;
        let k = r#""io.modelcontextprotocol/clientCapabilities":{}"#;
        let cases: Vec<(String, i64, &str)> = vec![
            // no _meta at all, for every method that needs it
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.into(),
                -32602,
                "missing required _meta",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#.into(),
                -32602,
                "missing required _meta",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.into(),
                -32602,
                "missing required _meta",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#.into(),
                -32602,
                "missing required _meta",
            ),
            (call(""), -32602, "missing required _meta"),
            // _meta present but not an object
            (call(r#""_meta":"x","#), -32602, "_meta must be an object"),
            (call(r#""_meta":null,"#), -32602, "_meta must be an object"),
            (call(r#""_meta":[],"#), -32602, "_meta must be an object"),
            // a required member missing or mistyped
            (
                call(&format!(r#""_meta":{{{k}}},"#)),
                -32602,
                "missing required _meta field",
            ),
            (
                call(&format!(r#""_meta":{{{v}}},"#)),
                -32602,
                "missing required _meta field",
            ),
            (
                call(&format!(
                    r#""_meta":{{{v},"io.modelcontextprotocol/clientCapabilities":[]}},"#
                )),
                -32602,
                "_meta field has wrong type",
            ),
            // version problems are -32022, never -32602
            (
                call(&format!(
                    r#""_meta":{{"io.modelcontextprotocol/protocolVersion":"1900-01-01",{k}}},"#
                )),
                -32022,
                "unsupported protocol version",
            ),
            (
                call(&format!(
                    r#""_meta":{{"io.modelcontextprotocol/protocolVersion":7,{k}}},"#
                )),
                -32022,
                "unsupported protocol version",
            ),
            // the legacy version is not a modern version
            (
                call(&format!(
                    r#""_meta":{{"io.modelcontextprotocol/protocolVersion":"2025-11-25",{k}}},"#
                )),
                -32022,
                "unsupported protocol version",
            ),
        ];
        for (msg, want, text) in cases {
            let (out, s) = run_stateless(&c, &msg);
            let v = parse_json(out.expect("response").as_bytes()).expect("json");
            let e = v.get("error").expect("error");
            assert_eq!(e.get("code"), Some(&Json::Int(want)), "{msg}");
            assert_eq!(e.get("message"), Some(&Json::Str(text.into())), "{msg}");
            assert!(s.backend().calls.is_empty());
            assert_eq!(
                s.mode(),
                Mode::Stateless,
                "a missing _meta must not select legacy"
            );
        }
        // The -32022 data lists the modern revision and echoes the requested one.
        let (out, _) = run_stateless(
            &c,
            &call(&format!(
                r#""_meta":{{"io.modelcontextprotocol/protocolVersion":"1900-01-01",{k}}},"#
            )),
        );
        assert!(
            out.expect("response")
                .contains(r#""data":{"supported":["2026-07-28"],"requested":"1900-01-01"}"#)
        );
    }

    fn run_stateless(c: &Caller, msg: &str) -> (Option<String>, Server<Mock>) {
        let mut s = Server::new(Mock::default());
        let out = s.handle(c, msg.as_bytes());
        (out, s)
    }

    #[test]
    fn stateless_requests_with_meta_are_served() {
        let c = all("a");
        let (out, s) = run_stateless(&c, &modern("ping", ""));
        assert_eq!(
            out.as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}}"#)
        );
        assert_eq!(s.mode(), Mode::Stateless);
        let (out, _) = run_stateless(&c, &modern("server/discover", ""));
        assert_eq!(
            out.as_deref(),
            Some(
                r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"lattice","version":"0"}}}}"#
            )
        );
        let (out, _) = run_stateless(&c, &modern("tools/list", ""));
        assert!(out.expect("response").contains("lattice_read"));
        let (out, s) = run_stateless(
            &c,
            &modern(
                "tools/call",
                r#","name":"lattice_read","arguments":{"scope":"a","table":"t"}"#,
            ),
        );
        assert!(out.expect("response").contains("\"isError\":false"));
        assert_eq!(s.backend().calls.len(), 1);
        // Extra client metadata (clientInfo, progressToken) is accepted.
        let msg = r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"c","version":"1"},"progressToken":"p"}}}"#;
        assert!(
            run_stateless(&c, msg)
                .0
                .expect("response")
                .contains("\"result\"")
        );
        // Unknown methods are -32601 whether or not _meta is present.
        let (out, _) = run_stateless(&c, r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#);
        assert!(out.expect("response").contains("-32601"));
    }

    #[test]
    fn dual_era_connection_is_set_only_by_initialize() {
        let c = all("a");
        let mut s = Server::new(Mock::default());
        assert_eq!(s.mode(), Mode::Stateless);
        // Before initialize, a request without _meta is refused ...
        let out = s.handle(&c, br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        assert!(out.expect("response").contains("-32602"));
        assert_eq!(s.mode(), Mode::Stateless);
        // ... a failed initialize does not switch mode ...
        let out = s.handle(
            &c,
            br#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#,
        );
        assert!(out.expect("response").contains("-32602"));
        assert_eq!(s.mode(), Mode::Stateless);
        // ... a notification does not either ...
        assert_eq!(
            s.handle(
                &c,
                br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            ),
            None
        );
        assert_eq!(s.mode(), Mode::Stateless);
        // ... a successful initialize does.
        let out = s.handle(&c, INIT.as_bytes());
        assert!(
            out.expect("response")
                .contains("\"protocolVersion\":\"2025-11-25\"")
        );
        assert_eq!(s.mode(), Mode::Legacy);
        // Now _meta is optional ...
        let out = s.handle(&c, br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
        assert_eq!(
            out.as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete"}}"#)
        );
        // ... modern requests are still served, and the legacy version may be named.
        assert!(
            s.handle(&c, modern("ping", "").as_bytes())
                .expect("response")
                .contains("\"result\"")
        );
        let legacy_meta = r#"{"jsonrpc":"2.0","id":4,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2025-11-25"}}}"#;
        assert!(
            s.handle(&c, legacy_meta.as_bytes())
                .expect("response")
                .contains("\"result\"")
        );
        // ... but a present _meta must still be well formed and name a known version.
        let out = s.handle(
            &c,
            br#"{"jsonrpc":"2.0","id":5,"method":"ping","params":{"_meta":3}}"#,
        );
        assert!(out.expect("response").contains("-32602"));
        let bad = r#"{"jsonrpc":"2.0","id":6,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01"}}}"#;
        let out = s.handle(&c, bad.as_bytes()).expect("response");
        assert!(out.contains(r#""code":-32022"#));
        assert!(out.contains(r#""supported":["2026-07-28","2025-11-25"]"#));
        // Mode is per server (per connection): a fresh server is stateless again.
        assert_eq!(Server::new(Mock::default()).mode(), Mode::Stateless);
    }

    // ---- injection -----------------------------------------------------------------------

    #[test]
    fn jsonrpc_text_in_arguments_cannot_change_routing() {
        let inner = r#"{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"lattice_write","arguments":{"scope":"root","table":"t","key":"k","value":"owned"}}}"#;
        let mut escaped = String::new();
        put_str(&mut escaped, inner);
        // As the value, and as the key, of an ordinary write inside the caller's scope.
        for args in [
            format!("{{\"scope\":\"team\",\"table\":\"t\",\"key\":\"k\",\"value\":{escaped}}}"),
            format!("{{\"scope\":\"team\",\"table\":\"t\",\"key\":{escaped},\"value\":\"v\"}}"),
        ] {
            let c = caller(&[(Tool::Write, "team")]);
            let (out, s) = run(&c, &call_msg("lattice_write", &args));
            assert!(
                out.expect("response")
                    .starts_with(r#"{"jsonrpc":"2.0","id":7,"result""#)
            );
            // Exactly one backend call, in the caller's own scope; nothing was routed to "root".
            assert_eq!(s.backend().calls.len(), 1);
            let (
                _,
                ToolCall::Write {
                    scope, key, value, ..
                },
            ) = &s.backend().calls[0]
            else {
                panic!("write expected");
            };
            assert_eq!(scope, "team");
            assert!(key == inner || value == inner);
        }
        // The same text as a scope or table is a schema error, not a route.
        let (out, s) = run(
            &all("team"),
            &call_msg(
                "lattice_read",
                &format!("{{\"scope\":{escaped},\"table\":\"t\"}}"),
            ),
        );
        assert!(out.expect("response").contains("\"code\":-32602"));
        assert!(s.backend().calls.is_empty());
        // A top-level duplicate method cannot smuggle a second reading of the message.
        let (out, _) = run(
            &all("a"),
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","method":"tools/call"}"#,
        );
        assert!(out.expect("response").contains("-32700"));
    }

    // ---- properties ----------------------------------------------------------------------

    fn arb_id() -> impl Strategy<Value = Id> {
        prop_oneof![
            any::<i64>().prop_map(Id::Num),
            "[ -~]{0,24}".prop_map(Id::Str),
        ]
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic_and_answer_is_json(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut s = legacy(Mock::default());
            if let Some(out) = s.handle(&all("a"), &bytes) {
                prop_assert!(parse_json(out.as_bytes()).is_ok());
            }
        }

        #[test]
        fn mutated_valid_requests_never_panic(
            cut in 0usize..200,
            flip in 0usize..200,
            byte in any::<u8>(),
        ) {
            let base = call_msg("lattice_write", r#"{"scope":"a","table":"t","key":"k","value":"v"}"#).into_bytes();
            let mut m = base.clone();
            if let Some(b) = m.get_mut(flip) { *b = byte; }
            m.truncate(cut.max(1).min(m.len()));
            let mut s = legacy(Mock::default());
            if let Some(out) = s.handle(&all("a"), &m) {
                prop_assert!(parse_json(out.as_bytes()).is_ok());
            }
        }

        #[test]
        fn valid_requests_round_trip(id in arb_id(), value in "[^\\x00-\\x08\\x0b\\x0c\\x0e-\\x1f\\x7f-\\u{9f}]{0,200}") {
            let mut idj = String::new();
            id.encode(&mut idj);
            let mut vj = String::new();
            put_str(&mut vj, &value);
            let msg = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{idj},\"method\":\"tools/call\",\"params\":{{\"name\":\"lattice_write\",\"arguments\":{{\"scope\":\"a\",\"table\":\"t\",\"key\":\"k\",\"value\":{vj}}}}}}}"
            );
            // The request decodes to the id and method sent.
            let Ok(Incoming::Request(r)) = decode(msg.as_bytes()) else { panic!("request") };
            prop_assert_eq!(&r.id, &id);
            prop_assert_eq!(r.method.as_str(), "tools/call");
            // The response carries the same id and the backend saw the value byte for byte.
            let mut s = legacy(Mock::default());
            let out = s.handle(&all("a"), msg.as_bytes()).expect("response");
            let v = parse_json(out.as_bytes()).expect("json");
            let want_id = match &id { Id::Num(n) => Json::Int(*n), Id::Str(s) => Json::Str(s.clone()) };
            prop_assert_eq!(v.get("id"), Some(&want_id));
            prop_assert!(v.get("result").is_some());
            prop_assert_eq!(s.backend().calls.len(), 1);
            let (_, ToolCall::Write { value: got, .. }) = &s.backend().calls[0] else { panic!("write") };
            prop_assert_eq!(got, &value);
        }

        #[test]
        fn denied_or_invalid_never_reaches_backend(
            scope in "[a-z/]{0,12}",
            table in "[ -~]{0,12}",
        ) {
            // The caller may only touch "zzz"; scopes drawn from [a-z/] never fall under it.
            let c = caller(&[(Tool::Read, "zzz")]);
            let args = {
                let (mut s, mut t) = (String::new(), String::new());
                put_str(&mut s, &scope);
                put_str(&mut t, &table);
                format!("{{\"scope\":{s},\"table\":{t}}}")
            };
            prop_assume!(scope != "zzz" && !scope.starts_with("zzz/"));
            let mut s = legacy(Mock::default());
            let _ = s.handle(&c, call_msg("lattice_read", &args).as_bytes());
            prop_assert!(s.backend().calls.is_empty());
        }
    }
}
