//! PostgreSQL session state machine, sans I/O.
//!
//! [`Session`] sits on top of the [`pgwire`](crate::pgwire) codec. A caller decodes one message,
//! hands it to [`Session::on_startup`] or [`Session::on_message`], and writes out what comes back in
//! the returned [`Outcome`]. The session opens no socket, reads no clock and never blocks, so the
//! whole conversation can be driven from a test, a fuzzer or the simulator. Nothing in here panics on
//! any sequence of messages.
//!
//! What it does:
//! - startup: refuses SSL and GSS encryption with `N`, then trust authentication
//!   (`AuthenticationOk`, `ParameterStatus` for `server_version` and `client_encoding`,
//!   `BackendKeyData`, `ReadyForQuery`);
//! - simple query: one `Q` message runs through a [`QueryHandler`] and ends with `ReadyForQuery`;
//! - extended query: named statements and portals, `Parse`/`Bind`/`Describe`/`Execute`/`Close`,
//!   a row limit on `Execute` with `PortalSuspended`, and the rule that after an error every message
//!   up to the next `Sync` is discarded;
//! - transaction status, from the command tags the handler returns (`BEGIN`, `COMMIT`, `ROLLBACK`)
//!   and from errors: an error inside a transaction block leaves it failed until it is rolled back.
//!
//! Limits, by design: results are text only (a binary format code is refused with `0A000`), a `Q`
//! message is passed to the handler as one statement, `Flush` has nothing to do because every reply
//! is returned at once, and `Cancel` and `client_encoding` negotiation are left to the caller.

use std::collections::{BTreeMap, VecDeque};

use crate::cancel::CancellationToken;

use crate::pgwire::{
    Backend, Column, ENCRYPTION_DECLINED, Frontend, ProtoError, Startup, Target, TxStatus,
};

/// The `server_version` reported to clients. Drivers parse it to pick features.
pub const SERVER_VERSION: &str = "16.0";

/// The most prepared statements one session may hold.
pub const MAX_STATEMENTS: usize = 1024;
/// The most portals one session may hold.
pub const MAX_PORTALS: usize = 1024;

/// One row; `None` is SQL NULL.
pub type Row = Vec<Option<Vec<u8>>>;

/// The result of running one statement.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryOutput {
    /// Result columns. Empty means the statement returns no rows (an `INSERT`, a `BEGIN`).
    pub columns: Vec<Column>,
    /// Result rows, ignored when `columns` is empty.
    pub rows: Vec<Row>,
    /// The command tag (`SELECT 3`, `INSERT 0 1`, `BEGIN`, `COMMIT`, `ROLLBACK`).
    pub tag: String,
}

/// A statement failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// Five character SQLSTATE.
    pub code: String,
    /// Human readable message.
    pub message: String,
}

impl QueryError {
    /// An error with a SQLSTATE and a message.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// What the session asks of the database. The session owns the protocol; the handler owns the SQL.
pub trait QueryHandler {
    /// Runs one statement. `params` are the bound values of an extended query, empty for a simple one.
    fn execute(&mut self, sql: &str, params: &[Option<Vec<u8>>])
    -> Result<QueryOutput, QueryError>;

    /// Checks a statement at `Parse` time and returns the columns it will produce (empty for none).
    fn describe(&mut self, sql: &str) -> Result<Vec<Column>, QueryError>;

    /// Runs with a statement-local cancellation signal. Cooperative executors override this method
    /// and poll between bounded units of work, aborting before their commit boundary. The default
    /// checks before entering the legacy handler; it cannot preempt a handler that blocks inside it.
    /// A successful result is not changed into a cancellation error after the handler returns.
    fn execute_cancellable(
        &mut self,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        check_cancellation(cancellation)?;
        self.execute(sql, params)
    }

    /// Describes with the same cooperative cancellation contract as execution.
    fn describe_cancellable(
        &mut self,
        sql: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Column>, QueryError> {
        check_cancellation(cancellation)?;
        self.describe(sql)
    }
}

/// What to do after one message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    /// A single byte to write raw, before anything else: the answer to an SSL or GSS request.
    pub encryption_reply: Option<u8>,
    /// Messages to encode and send, in order.
    pub messages: Vec<Backend>,
    /// Close the connection after sending.
    pub close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitingStartup,
    Ready,
    Closed,
}

struct Statement {
    sql: String,
    param_types: Vec<i32>,
    columns: Vec<Column>,
}

enum PortalState {
    /// Not run yet.
    Fresh,
    /// Stopped at a row limit with these rows left.
    Suspended(VecDeque<Row>, String),
    /// Ran to the end; running it again returns no rows.
    Done(String),
}

struct Portal {
    sql: String,
    params: Vec<Option<Vec<u8>>>,
    columns: Vec<Column>,
    state: PortalState,
}

/// One client connection.
pub struct Session<H> {
    handler: H,
    pid: i32,
    secret: i32,
    phase: Phase,
    tx: TxStatus,
    /// After an error in the extended flow: ignore everything up to the next `Sync`.
    discarding: bool,
    statements: BTreeMap<String, Statement>,
    portals: BTreeMap<String, Portal>,
}

impl<H: QueryHandler> Session<H> {
    /// A session waiting for the startup message. `pid` and `secret` go out in `BackendKeyData`.
    pub fn new(handler: H, pid: i32, secret: i32) -> Self {
        Self {
            handler,
            pid,
            secret,
            phase: Phase::AwaitingStartup,
            tx: TxStatus::Idle,
            discarding: false,
            statements: BTreeMap::new(),
            portals: BTreeMap::new(),
        }
    }

    /// The transaction status the next `ReadyForQuery` will carry.
    pub fn tx_status(&self) -> TxStatus {
        self.tx
    }

    /// True once the session has ended; every further message is ignored.
    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// True while the session waits for a startup message (also after an encryption refusal).
    pub fn awaiting_startup(&self) -> bool {
        self.phase == Phase::AwaitingStartup
    }

    /// The handler, for inspection.
    pub fn handler(&self) -> &H {
        &self.handler
    }

    /// Answers input the codec could not decode: a fatal protocol violation, then close.
    pub fn on_protocol_error(&mut self, err: &ProtoError) -> Outcome {
        self.fatal("08P01", err.to_string())
    }

    /// Handles a startup-phase message.
    pub fn on_startup(&mut self, msg: Startup) -> Outcome {
        if self.phase == Phase::Closed {
            return closed();
        }
        if self.phase != Phase::AwaitingStartup {
            return self.fatal("08P01", "unexpected startup message".to_string());
        }
        match msg {
            Startup::SslRequest | Startup::GssEncRequest => Outcome {
                encryption_reply: Some(ENCRYPTION_DECLINED),
                ..Outcome::default()
            },
            // A cancel request arrives on its own connection and gets no reply.
            Startup::Cancel { .. } => {
                self.phase = Phase::Closed;
                closed()
            }
            Startup::Params(params) => {
                if !params.iter().any(|(k, v)| k == "user" && !v.is_empty()) {
                    return self.fatal(
                        "28000",
                        "no user name specified in the startup packet".to_string(),
                    );
                }
                self.phase = Phase::Ready;
                Outcome {
                    messages: vec![
                        Backend::AuthenticationOk,
                        Backend::ParameterStatus {
                            name: "server_version".into(),
                            value: SERVER_VERSION.into(),
                        },
                        Backend::ParameterStatus {
                            name: "client_encoding".into(),
                            value: "UTF8".into(),
                        },
                        Backend::BackendKeyData {
                            pid: self.pid,
                            secret: self.secret,
                        },
                        Backend::ReadyForQuery(self.tx),
                    ],
                    ..Outcome::default()
                }
            }
        }
    }

    /// Handles a message without an externally cancellable signal, for legacy sans-I/O callers.
    /// Hosts routing CancelRequest must use [`Session::on_message_with_cancellation`] instead.
    pub fn on_message(&mut self, msg: Frontend) -> Outcome {
        self.on_message_with_cancellation(msg, &CancellationToken::default())
    }

    /// Handles a message with the fresh token allocated for that operation by the host registry.
    /// The token reaches execution and Parse-time description. It is not retained between messages;
    /// idle cancellation must not poison a subsequent statement. Cancellation is cooperative and
    /// does not interrupt an uncooperative handler, blocking I/O or already-computed result output.
    pub fn on_message_with_cancellation(
        &mut self,
        msg: Frontend,
        cancellation: &CancellationToken,
    ) -> Outcome {
        match self.phase {
            Phase::Closed => return closed(),
            Phase::AwaitingStartup => {
                return self.fatal("08P01", "expected a startup message".to_string());
            }
            Phase::Ready => {}
        }
        let mut out = Vec::new();
        match msg {
            Frontend::Terminate => {
                self.phase = Phase::Closed;
                return closed();
            }
            Frontend::Password(_)
            | Frontend::SaslInitialResponse { .. }
            | Frontend::SaslResponse(_) => {
                return self.fatal("08P01", "unexpected password message".to_string());
            }
            Frontend::Sync => {
                self.discarding = false;
                out.push(Backend::ReadyForQuery(self.tx));
            }
            _ if self.discarding => {}
            Frontend::Flush => {}
            Frontend::Query(sql) => {
                self.simple_query(&sql, &mut out, cancellation);
                out.push(Backend::ReadyForQuery(self.tx));
            }
            other => {
                if let Err(e) = self.extended(other, &mut out, cancellation) {
                    if self.tx == TxStatus::InTransaction {
                        self.tx = TxStatus::Failed;
                    }
                    out.push(error_message(&e));
                    self.discarding = true;
                }
            }
        }
        Outcome {
            messages: out,
            ..Outcome::default()
        }
    }

    fn fatal(&mut self, code: &str, message: String) -> Outcome {
        self.phase = Phase::Closed;
        Outcome {
            messages: vec![Backend::ErrorResponse {
                severity: "FATAL".into(),
                code: code.into(),
                message,
            }],
            close: true,
            ..Outcome::default()
        }
    }

    fn simple_query(
        &mut self,
        sql: &str,
        out: &mut Vec<Backend>,
        cancellation: &CancellationToken,
    ) {
        if is_blank(sql) {
            out.push(Backend::EmptyQueryResponse);
            return;
        }
        match self.run(sql, &[], cancellation) {
            Ok(result) => {
                if !result.columns.is_empty() {
                    out.push(Backend::RowDescription(result.columns));
                    out.extend(result.rows.into_iter().map(Backend::DataRow));
                }
                out.push(Backend::CommandComplete(result.tag));
            }
            Err(e) => out.push(error_message(&e)),
        }
    }

    /// Runs a statement through the handler, enforcing the failed-transaction rule and keeping the
    /// transaction status current.
    fn run(
        &mut self,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        if self.tx == TxStatus::Failed && !ends_transaction(sql) {
            return Err(QueryError::new(
                "25P02",
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        let result = check_cancellation(cancellation)
            .and_then(|()| self.handler.execute_cancellable(sql, params, cancellation));
        match result {
            Ok(result) => {
                match result.tag.as_str() {
                    "BEGIN" if self.tx == TxStatus::Idle => self.tx = TxStatus::InTransaction,
                    "COMMIT" | "ROLLBACK" => self.tx = TxStatus::Idle,
                    _ => {}
                }
                Ok(result)
            }
            Err(e) => {
                if self.tx == TxStatus::InTransaction {
                    self.tx = TxStatus::Failed;
                }
                Err(e)
            }
        }
    }

    fn extended(
        &mut self,
        msg: Frontend,
        out: &mut Vec<Backend>,
        cancellation: &CancellationToken,
    ) -> Result<(), QueryError> {
        match msg {
            Frontend::Parse {
                name,
                query,
                param_types,
            } => {
                if !name.is_empty() && self.statements.contains_key(&name) {
                    return Err(QueryError::new(
                        "42P05",
                        format!("prepared statement \"{name}\" already exists"),
                    ));
                }
                if !self.statements.contains_key(&name) && self.statements.len() >= MAX_STATEMENTS {
                    return Err(QueryError::new("54000", "too many prepared statements"));
                }
                let columns = if is_blank(&query) {
                    Vec::new()
                } else {
                    check_cancellation(cancellation)?;
                    self.handler.describe_cancellable(&query, cancellation)?
                };
                self.statements.insert(
                    name,
                    Statement {
                        sql: query,
                        param_types,
                        columns,
                    },
                );
                out.push(Backend::ParseComplete);
            }
            Frontend::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            } => {
                if param_formats.contains(&1) || result_formats.contains(&1) {
                    return Err(QueryError::new(
                        "0A000",
                        "binary format codes are not supported",
                    ));
                }
                let stmt = self.statements.get(&statement).ok_or_else(|| {
                    QueryError::new(
                        "26000",
                        format!("prepared statement \"{statement}\" does not exist"),
                    )
                })?;
                if !stmt.param_types.is_empty() && stmt.param_types.len() != params.len() {
                    return Err(QueryError::new(
                        "08P01",
                        format!(
                            "bind message supplies {} parameters, but prepared statement \"{}\" requires {}",
                            params.len(),
                            statement,
                            stmt.param_types.len()
                        ),
                    ));
                }
                if !portal.is_empty() && self.portals.contains_key(&portal) {
                    return Err(QueryError::new(
                        "42P03",
                        format!("portal \"{portal}\" already exists"),
                    ));
                }
                if !self.portals.contains_key(&portal) && self.portals.len() >= MAX_PORTALS {
                    return Err(QueryError::new("54000", "too many portals"));
                }
                let made = Portal {
                    sql: stmt.sql.clone(),
                    params,
                    columns: stmt.columns.clone(),
                    state: PortalState::Fresh,
                };
                self.portals.insert(portal, made);
                out.push(Backend::BindComplete);
            }
            Frontend::Describe(Target::Statement(name)) => {
                let stmt = self.statements.get(&name).ok_or_else(|| {
                    QueryError::new(
                        "26000",
                        format!("prepared statement \"{name}\" does not exist"),
                    )
                })?;
                out.push(Backend::ParameterDescription(stmt.param_types.clone()));
                out.push(shape(&stmt.columns));
            }
            Frontend::Describe(Target::Portal(name)) => {
                let portal = self.portals.get(&name).ok_or_else(|| {
                    QueryError::new("34000", format!("portal \"{name}\" does not exist"))
                })?;
                out.push(shape(&portal.columns));
            }
            Frontend::Close(Target::Statement(name)) => {
                self.statements.remove(&name);
                out.push(Backend::CloseComplete);
            }
            Frontend::Close(Target::Portal(name)) => {
                self.portals.remove(&name);
                out.push(Backend::CloseComplete);
            }
            Frontend::Execute { portal, max_rows } => {
                if let Err(error) = self.execute_portal(&portal, max_rows, out, cancellation) {
                    if error.code == "57014" {
                        self.portals.remove(&portal);
                    }
                    return Err(error);
                }
            }
            // Handled by the caller: these never reach here.
            Frontend::Query(_)
            | Frontend::Terminate
            | Frontend::Password(_)
            | Frontend::SaslInitialResponse { .. }
            | Frontend::SaslResponse(_)
            | Frontend::Sync
            | Frontend::Flush => {}
        }
        Ok(())
    }

    fn execute_portal(
        &mut self,
        name: &str,
        max_rows: i32,
        out: &mut Vec<Backend>,
        cancellation: &CancellationToken,
    ) -> Result<(), QueryError> {
        let missing = || QueryError::new("34000", format!("portal \"{name}\" does not exist"));
        let portal = self.portals.get(name).ok_or_else(missing)?;
        let state = match &portal.state {
            PortalState::Fresh => {
                check_cancellation(cancellation)?;
                let sql = portal.sql.clone();
                let params = portal.params.clone();
                if is_blank(&sql) {
                    out.push(Backend::EmptyQueryResponse);
                    if let Some(p) = self.portals.get_mut(name) {
                        p.state = PortalState::Done(String::new());
                    }
                    return Ok(());
                }
                let result = self.run(&sql, &params, cancellation)?;
                let rows = if result.columns.is_empty() {
                    VecDeque::new()
                } else {
                    result.rows.into()
                };
                PortalState::Suspended(rows, result.tag)
            }
            PortalState::Suspended(rows, tag) => PortalState::Suspended(rows.clone(), tag.clone()),
            PortalState::Done(tag) => PortalState::Suspended(VecDeque::new(), tag.clone()),
        };
        let PortalState::Suspended(mut rows, tag) = state else {
            return Ok(());
        };
        let limit = match usize::try_from(max_rows) {
            Ok(0) | Err(_) => usize::MAX,
            Ok(n) => n,
        };
        let mut sent = 0;
        while sent < limit {
            let Some(row) = rows.pop_front() else { break };
            out.push(Backend::DataRow(row));
            sent += 1;
        }
        let next = if rows.is_empty() {
            out.push(Backend::CommandComplete(tag.clone()));
            PortalState::Done(tag)
        } else {
            out.push(Backend::PortalSuspended);
            PortalState::Suspended(rows, tag)
        };
        if let Some(p) = self.portals.get_mut(name) {
            p.state = next;
        }
        Ok(())
    }
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), QueryError> {
    if cancellation.is_cancelled() {
        Err(QueryError::new(
            "57014",
            "canceling statement due to user request",
        ))
    } else {
        Ok(())
    }
}

fn closed() -> Outcome {
    Outcome {
        close: true,
        ..Outcome::default()
    }
}

fn error_message(e: &QueryError) -> Backend {
    Backend::ErrorResponse {
        severity: "ERROR".into(),
        code: e.code.clone(),
        message: e.message.clone(),
    }
}

fn shape(columns: &[Column]) -> Backend {
    if columns.is_empty() {
        Backend::NoData
    } else {
        Backend::RowDescription(columns.to_vec())
    }
}

/// Only whitespace and semicolons: PostgreSQL answers `EmptyQueryResponse`.
fn is_blank(sql: &str) -> bool {
    sql.chars().all(|c| c.is_whitespace() || c == ';')
}

/// Statements a failed transaction block still accepts.
fn ends_transaction(sql: &str) -> bool {
    let word: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(word.as_str(), "ROLLBACK" | "ABORT" | "COMMIT" | "END")
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
    use crate::pgwire::{PROTOCOL_3_0, decode_frontend, decode_startup};
    use proptest::prelude::*;

    /// A tiny database: `SELECT 1` and `SELECT 3` return rows, `BEGIN`/`COMMIT`/`ROLLBACK` return
    /// their tags, `FAIL` errors, `BAD` is a syntax error, `SELECT $1` echoes its parameter.
    #[derive(Default)]
    struct Fake {
        executed: Vec<String>,
    }

    fn col() -> Column {
        Column {
            name: "n".into(),
            table_oid: 0,
            column_attr: 0,
            type_oid: 23,
            type_size: 4,
            type_modifier: -1,
            format: 0,
        }
    }

    impl QueryHandler for Fake {
        fn execute(
            &mut self,
            sql: &str,
            params: &[Option<Vec<u8>>],
        ) -> Result<QueryOutput, QueryError> {
            self.executed.push(sql.to_string());
            let select = |rows: Vec<Row>| {
                let tag = format!("SELECT {}", rows.len());
                Ok(QueryOutput {
                    columns: vec![col()],
                    rows,
                    tag,
                })
            };
            match sql.trim().to_ascii_uppercase().as_str() {
                "SELECT 1" => select(vec![vec![Some(b"1".to_vec())]]),
                "SELECT 3" => select((1..=3).map(|i: u8| vec![Some(vec![b'0' + i])]).collect()),
                "SELECT $1" => select(vec![vec![params.first().cloned().flatten()]]),
                "BEGIN" | "COMMIT" | "ROLLBACK" => Ok(QueryOutput {
                    tag: sql.trim().to_ascii_uppercase(),
                    ..QueryOutput::default()
                }),
                "FAIL" => Err(QueryError::new("XX000", "boom")),
                _ => Ok(QueryOutput {
                    tag: "OK".into(),
                    ..QueryOutput::default()
                }),
            }
        }

        fn describe(&mut self, sql: &str) -> Result<Vec<Column>, QueryError> {
            match sql.trim() {
                "BAD" => Err(QueryError::new("42601", "syntax error")),
                s if s.starts_with("SELECT") => Ok(vec![col()]),
                _ => Ok(Vec::new()),
            }
        }
    }

    fn ready() -> Session<Fake> {
        let mut s = Session::new(Fake::default(), 1, 2);
        s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        s
    }

    fn wire(out: &Outcome) -> Vec<u8> {
        let mut bytes = Vec::new();
        for m in &out.messages {
            m.encode(&mut bytes).unwrap();
        }
        bytes
    }

    fn err_code(m: &Backend) -> &str {
        match m {
            Backend::ErrorResponse { code, .. } => code,
            other => panic!("not an error: {other:?}"),
        }
    }

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        v.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    /// Decodes client bytes and feeds them through the session.
    fn feed(s: &mut Session<Fake>, bytes: &[u8]) -> Outcome {
        let (msg, used) = decode_frontend(bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        s.on_message(msg)
    }

    fn q(sql: &str) -> Frontend {
        Frontend::Query(sql.into())
    }

    fn parse(name: &str, sql: &str) -> Frontend {
        Frontend::Parse {
            name: name.into(),
            query: sql.into(),
            param_types: vec![],
        }
    }

    fn bind(portal: &str, statement: &str, params: Vec<Option<Vec<u8>>>) -> Frontend {
        Frontend::Bind {
            portal: portal.into(),
            statement: statement.into(),
            param_formats: vec![],
            params,
            result_formats: vec![],
        }
    }

    fn exec(portal: &str, max_rows: i32) -> Frontend {
        Frontend::Execute {
            portal: portal.into(),
            max_rows,
        }
    }

    #[test]
    fn ssl_request_bytes_get_n_and_startup_continues() {
        // 00 00 00 08 04 d2 16 2f is the SSLRequest: length 8, code 80877103.
        let bytes = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];
        let (msg, _) = decode_startup(&bytes).unwrap().unwrap();
        let mut s = Session::new(Fake::default(), 1, 2);
        let out = s.on_startup(msg);
        assert_eq!(out.encryption_reply, Some(b'N'));
        assert!(out.messages.is_empty() && !out.close);
        assert!(s.awaiting_startup());
        // GSSENCRequest (80877104) is refused the same way.
        let (msg, _) = decode_startup(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x30])
            .unwrap()
            .unwrap();
        assert_eq!(s.on_startup(msg).encryption_reply, Some(b'N'));
        assert!(s.awaiting_startup());
    }

    #[test]
    fn trust_startup_sends_the_exact_handshake_bytes() {
        // length 21, protocol 3.0, "user\0u\0", terminator.
        let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0u\0\0");
        let mut bytes = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&body);
        let (msg, _) = decode_startup(&bytes).unwrap().unwrap();
        let mut s = Session::new(Fake::default(), 1, 2);
        let out = s.on_startup(msg);

        let mut expected = vec![b'R', 0, 0, 0, 8, 0, 0, 0, 0];
        expected.extend_from_slice(&[b'S', 0, 0, 0, 24]);
        expected.extend_from_slice(b"server_version\x0016.0\0");
        expected.extend_from_slice(&[b'S', 0, 0, 0, 25]);
        expected.extend_from_slice(b"client_encoding\0UTF8\0");
        expected.extend_from_slice(&[b'K', 0, 0, 0, 12, 0, 0, 0, 1, 0, 0, 0, 2]);
        expected.extend_from_slice(&[b'Z', 0, 0, 0, 5, b'I']);
        assert_eq!(wire(&out), expected);
        assert!(!out.close && !s.is_closed());
    }

    #[test]
    fn startup_without_a_user_is_fatal() {
        let mut s = Session::new(Fake::default(), 1, 2);
        let out = s.on_startup(Startup::Params(vec![("database".into(), "d".into())]));
        assert_eq!(err_code(&out.messages[0]), "28000");
        assert!(out.close && s.is_closed());
    }

    #[test]
    fn messages_before_startup_and_a_second_startup_are_fatal() {
        let mut s = Session::new(Fake::default(), 1, 2);
        let out = s.on_message(q("SELECT 1"));
        assert_eq!(err_code(&out.messages[0]), "08P01");
        assert!(out.close);

        let mut s = ready();
        let out = s.on_startup(Startup::SslRequest);
        assert_eq!(err_code(&out.messages[0]), "08P01");
        assert!(out.close);
    }

    #[test]
    fn cancel_request_closes_silently() {
        let mut s = Session::new(Fake::default(), 1, 2);
        let out = s.on_startup(Startup::Cancel { pid: 1, secret: 2 });
        assert!(out.messages.is_empty() && out.close && s.is_closed());
    }

    #[test]
    fn simple_query_sends_the_exact_result_bytes() {
        let mut s = ready();
        // 'Q' length 13 "SELECT 1\0"
        let out = feed(&mut s, &frame(b'Q', b"SELECT 1\0"));
        let mut expected = vec![b'T', 0, 0, 0, 26, 0, 1, b'n', 0];
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // table oid, attribute number
        expected.extend_from_slice(&[0, 0, 0, 23, 0, 4]); // int4, size 4
        expected.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0, 0]); // modifier -1, text
        expected.extend_from_slice(&[b'D', 0, 0, 0, 11, 0, 1, 0, 0, 0, 1, b'1']);
        expected.extend_from_slice(&[b'C', 0, 0, 0, 13]);
        expected.extend_from_slice(b"SELECT 1\0");
        expected.extend_from_slice(&[b'Z', 0, 0, 0, 5, b'I']);
        assert_eq!(wire(&out), expected);
    }

    #[test]
    fn empty_query_gets_empty_response() {
        let mut s = ready();
        // 'Q' length 5, just the terminator; and a query of only a semicolon.
        for sql in [&b"\0"[..], b" ; \0"] {
            let out = feed(&mut s, &frame(b'Q', sql));
            assert_eq!(
                out.messages,
                vec![
                    Backend::EmptyQueryResponse,
                    Backend::ReadyForQuery(TxStatus::Idle)
                ]
            );
        }
        assert!(s.handler().executed.is_empty());
    }

    #[test]
    fn command_without_rows_sends_only_the_tag() {
        let mut s = ready();
        let out = s.on_message(q("INSERT"));
        assert_eq!(
            out.messages,
            vec![
                Backend::CommandComplete("OK".into()),
                Backend::ReadyForQuery(TxStatus::Idle)
            ]
        );
    }

    #[test]
    fn simple_query_error_keeps_the_session_open() {
        let mut s = ready();
        let out = s.on_message(q("FAIL"));
        assert_eq!(err_code(&out.messages[0]), "XX000");
        assert_eq!(out.messages[1], Backend::ReadyForQuery(TxStatus::Idle));
        assert!(!out.close);
        assert_eq!(s.on_message(q("SELECT 1")).messages.len(), 4);
    }

    #[test]
    fn transaction_status_follows_tags_and_errors() {
        let mut s = ready();
        let z = |o: Outcome| o.messages.last().cloned().unwrap();
        assert_eq!(
            z(s.on_message(q("BEGIN"))),
            Backend::ReadyForQuery(TxStatus::InTransaction)
        );
        assert_eq!(
            z(s.on_message(q("SELECT 1"))),
            Backend::ReadyForQuery(TxStatus::InTransaction)
        );
        assert_eq!(
            z(s.on_message(q("FAIL"))),
            Backend::ReadyForQuery(TxStatus::Failed)
        );
        // A failed block refuses everything but its end, without calling the handler.
        let before = s.handler().executed.len();
        let out = s.on_message(q("SELECT 1"));
        assert_eq!(err_code(&out.messages[0]), "25P02");
        assert_eq!(out.messages[1], Backend::ReadyForQuery(TxStatus::Failed));
        assert_eq!(s.handler().executed.len(), before);
        assert_eq!(
            z(s.on_message(q("rollback"))),
            Backend::ReadyForQuery(TxStatus::Idle)
        );
        assert_eq!(
            z(s.on_message(q("BEGIN"))),
            Backend::ReadyForQuery(TxStatus::InTransaction)
        );
        assert_eq!(
            z(s.on_message(q("COMMIT"))),
            Backend::ReadyForQuery(TxStatus::Idle)
        );
        // An error outside a block does not start one.
        assert_eq!(
            z(s.on_message(q("FAIL"))),
            Backend::ReadyForQuery(TxStatus::Idle)
        );
    }

    #[test]
    fn terminate_closes_and_later_input_is_ignored() {
        let mut s = ready();
        let out = feed(&mut s, &frame(b'X', b""));
        assert!(out.close && out.messages.is_empty() && s.is_closed());
        let out = s.on_message(q("SELECT 1"));
        assert!(out.close && out.messages.is_empty());
        assert!(s.on_startup(Startup::SslRequest).messages.is_empty());
    }

    #[test]
    fn password_message_is_fatal_under_trust() {
        let mut s = ready();
        let out = s.on_message(Frontend::Password(b"x\0".to_vec()));
        assert_eq!(err_code(&out.messages[0]), "08P01");
        assert!(out.close);
    }

    #[test]
    fn protocol_error_is_fatal() {
        let mut s = ready();
        let out = s.on_protocol_error(&ProtoError::UnknownTag(b'!'));
        assert_eq!(err_code(&out.messages[0]), "08P01");
        assert!(out.close && s.is_closed());
    }

    #[test]
    fn extended_flow_sends_the_exact_reply_bytes() {
        let mut s = ready();
        // P: unnamed statement "SELECT 1", no parameter types.
        let out = feed(&mut s, &frame(b'P', b"\0SELECT 1\0\0\0"));
        assert_eq!(wire(&out), [b'1', 0, 0, 0, 4]);
        // B: unnamed portal, unnamed statement, no formats, no params, no result formats.
        let out = feed(&mut s, &frame(b'B', b"\0\0\0\0\0\0\0\0"));
        assert_eq!(wire(&out), [b'2', 0, 0, 0, 4]);
        // E: unnamed portal, no limit.
        let out = feed(&mut s, &frame(b'E', b"\0\0\0\0\0"));
        let mut expected = vec![b'D', 0, 0, 0, 11, 0, 1, 0, 0, 0, 1, b'1'];
        expected.extend_from_slice(&[b'C', 0, 0, 0, 13]);
        expected.extend_from_slice(b"SELECT 1\0");
        assert_eq!(wire(&out), expected);
        // S
        let out = feed(&mut s, &frame(b'S', b""));
        assert_eq!(wire(&out), [b'Z', 0, 0, 0, 5, b'I']);
    }

    #[test]
    fn describe_statement_and_portal() {
        let mut s = ready();
        s.on_message(Frontend::Parse {
            name: "s".into(),
            query: "SELECT $1".into(),
            param_types: vec![25],
        });
        s.on_message(parse("ins", "INSERT"));
        let out = s.on_message(Frontend::Describe(Target::Statement("s".into())));
        assert_eq!(
            out.messages,
            vec![
                Backend::ParameterDescription(vec![25]),
                Backend::RowDescription(vec![col()])
            ]
        );
        let out = s.on_message(Frontend::Describe(Target::Statement("ins".into())));
        assert_eq!(
            out.messages,
            vec![Backend::ParameterDescription(vec![]), Backend::NoData]
        );
        s.on_message(bind("p", "ins", vec![]));
        let out = s.on_message(Frontend::Describe(Target::Portal("p".into())));
        assert_eq!(out.messages, vec![Backend::NoData]);
    }

    #[test]
    fn bind_passes_parameters_and_checks_their_count() {
        let mut s = ready();
        s.on_message(Frontend::Parse {
            name: "s".into(),
            query: "SELECT $1".into(),
            param_types: vec![25],
        });
        let out = s.on_message(bind("p", "s", vec![]));
        assert_eq!(err_code(&out.messages[0]), "08P01");
        s.on_message(Frontend::Sync);
        s.on_message(bind("p", "s", vec![Some(b"hi".to_vec())]));
        let out = s.on_message(exec("p", 0));
        assert_eq!(
            out.messages[0],
            Backend::DataRow(vec![Some(b"hi".to_vec())])
        );
    }

    #[test]
    fn row_limit_suspends_and_resumes_the_portal() {
        let mut s = ready();
        s.on_message(parse("", "SELECT 3"));
        s.on_message(bind("p", "", vec![]));
        let row = |c: u8| Backend::DataRow(vec![Some(vec![c])]);
        let out = s.on_message(exec("p", 2));
        assert_eq!(
            out.messages,
            vec![row(b'1'), row(b'2'), Backend::PortalSuspended]
        );
        let out = s.on_message(exec("p", 2));
        assert_eq!(
            out.messages,
            vec![row(b'3'), Backend::CommandComplete("SELECT 3".into())]
        );
        // Once done, running it again returns no rows and the handler is not called again.
        let out = s.on_message(exec("p", 0));
        assert_eq!(
            out.messages,
            vec![Backend::CommandComplete("SELECT 3".into())]
        );
        assert_eq!(s.handler().executed, vec!["SELECT 3"]);
    }

    #[test]
    fn error_skips_everything_until_sync() {
        let mut s = ready();
        let out = s.on_message(parse("", "BAD"));
        assert_eq!(err_code(&out.messages[0]), "42601");
        // Discarded: nothing is answered and nothing is stored.
        for m in [
            parse("x", "SELECT 1"),
            bind("p", "x", vec![]),
            exec("p", 0),
            q("SELECT 1"),
            Frontend::Flush,
        ] {
            assert!(s.on_message(m).messages.is_empty());
        }
        let out = s.on_message(Frontend::Sync);
        assert_eq!(out.messages, vec![Backend::ReadyForQuery(TxStatus::Idle)]);
        // After Sync the session works, and "x" never existed.
        let out = s.on_message(bind("p", "x", vec![]));
        assert_eq!(err_code(&out.messages[0]), "26000");
        s.on_message(Frontend::Sync);
        assert_eq!(s.on_message(q("SELECT 1")).messages.len(), 4);
    }

    #[test]
    fn error_in_a_transaction_fails_it_at_sync() {
        let mut s = ready();
        s.on_message(q("BEGIN"));
        s.on_message(parse("", "FAIL"));
        s.on_message(bind("", "", vec![]));
        let out = s.on_message(exec("", 0));
        assert_eq!(err_code(&out.messages[0]), "XX000");
        let out = s.on_message(Frontend::Sync);
        assert_eq!(out.messages, vec![Backend::ReadyForQuery(TxStatus::Failed)]);
        assert_eq!(s.tx_status(), TxStatus::Failed);
    }

    #[test]
    fn named_objects_are_unique_and_unnamed_ones_are_replaced() {
        let mut s = ready();
        assert_eq!(
            s.on_message(parse("a", "SELECT 1")).messages,
            vec![Backend::ParseComplete]
        );
        let out = s.on_message(parse("a", "SELECT 1"));
        assert_eq!(err_code(&out.messages[0]), "42P05");
        s.on_message(Frontend::Sync);
        assert_eq!(
            s.on_message(parse("", "SELECT 1")).messages,
            vec![Backend::ParseComplete]
        );
        assert_eq!(
            s.on_message(parse("", "SELECT 3")).messages,
            vec![Backend::ParseComplete]
        );
        assert_eq!(
            s.on_message(bind("p", "a", vec![])).messages,
            vec![Backend::BindComplete]
        );
        let out = s.on_message(bind("p", "a", vec![]));
        assert_eq!(err_code(&out.messages[0]), "42P03");
        s.on_message(Frontend::Sync);
        assert_eq!(
            s.on_message(bind("", "a", vec![])).messages,
            vec![Backend::BindComplete]
        );
        assert_eq!(
            s.on_message(bind("", "a", vec![])).messages,
            vec![Backend::BindComplete]
        );
    }

    #[test]
    fn close_removes_and_is_idempotent() {
        let mut s = ready();
        s.on_message(parse("a", "SELECT 1"));
        s.on_message(bind("p", "a", vec![]));
        let c = Frontend::Close;
        assert_eq!(
            s.on_message(c(Target::Portal("p".into()))).messages,
            vec![Backend::CloseComplete]
        );
        assert_eq!(
            s.on_message(c(Target::Portal("p".into()))).messages,
            vec![Backend::CloseComplete]
        );
        assert_eq!(
            s.on_message(c(Target::Statement("a".into()))).messages,
            vec![Backend::CloseComplete]
        );
        let out = s.on_message(exec("p", 0));
        assert_eq!(err_code(&out.messages[0]), "34000");
        s.on_message(Frontend::Sync);
        let out = s.on_message(bind("p", "a", vec![]));
        assert_eq!(err_code(&out.messages[0]), "26000");
    }

    #[test]
    fn binary_formats_are_refused() {
        let mut s = ready();
        s.on_message(parse("", "SELECT 1"));
        let out = s.on_message(Frontend::Bind {
            portal: "".into(),
            statement: "".into(),
            param_formats: vec![],
            params: vec![],
            result_formats: vec![1],
        });
        assert_eq!(err_code(&out.messages[0]), "0A000");
    }

    #[test]
    fn empty_statement_executes_to_empty_response() {
        let mut s = ready();
        s.on_message(parse("", ";"));
        s.on_message(bind("", "", vec![]));
        let out = s.on_message(exec("", 0));
        assert_eq!(out.messages, vec![Backend::EmptyQueryResponse]);
        assert!(s.handler().executed.is_empty());
    }

    #[test]
    fn statement_limit_is_enforced() {
        let mut s = ready();
        for i in 0..MAX_STATEMENTS {
            assert_eq!(
                s.on_message(parse(&format!("s{i}"), "SELECT 1")).messages,
                vec![Backend::ParseComplete]
            );
        }
        let out = s.on_message(parse("one-more", "SELECT 1"));
        assert_eq!(err_code(&out.messages[0]), "54000");
    }

    fn name() -> impl Strategy<Value = String> {
        "[ab]{0,1}"
    }

    fn sql() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("SELECT 1".to_string()),
            Just("SELECT 3".to_string()),
            Just("SELECT $1".to_string()),
            Just("BEGIN".to_string()),
            Just("COMMIT".to_string()),
            Just("ROLLBACK".to_string()),
            Just("FAIL".to_string()),
            Just("BAD".to_string()),
            Just(";".to_string()),
            "[ -~]{0,12}",
        ]
    }

    fn target() -> impl Strategy<Value = Target> {
        prop_oneof![
            name().prop_map(Target::Statement),
            name().prop_map(Target::Portal)
        ]
    }

    fn frontend() -> impl Strategy<Value = Frontend> {
        prop_oneof![
            sql().prop_map(Frontend::Query),
            (name(), sql(), proptest::collection::vec(any::<i32>(), 0..3)).prop_map(
                |(name, query, param_types)| Frontend::Parse {
                    name,
                    query,
                    param_types
                }
            ),
            (
                name(),
                name(),
                proptest::collection::vec(0i16..=1, 0..2),
                proptest::collection::vec(
                    proptest::option::of(proptest::collection::vec(any::<u8>(), 0..4)),
                    0..3
                ),
                proptest::collection::vec(0i16..=1, 0..2),
            )
                .prop_map(
                    |(portal, statement, param_formats, params, result_formats)| Frontend::Bind {
                        portal,
                        statement,
                        param_formats,
                        params,
                        result_formats,
                    }
                ),
            target().prop_map(Frontend::Describe),
            target().prop_map(Frontend::Close),
            (name(), 0i32..4).prop_map(|(portal, max_rows)| Frontend::Execute { portal, max_rows }),
            Just(Frontend::Sync),
            Just(Frontend::Flush),
            Just(Frontend::Terminate),
            proptest::collection::vec(any::<u8>(), 0..4).prop_map(Frontend::Password),
        ]
    }

    fn startup() -> impl Strategy<Value = Startup> {
        prop_oneof![
            Just(Startup::SslRequest),
            Just(Startup::GssEncRequest),
            Just(Startup::Cancel { pid: 1, secret: 2 }),
            Just(Startup::Params(vec![("user".into(), "u".into())])),
            Just(Startup::Params(vec![])),
        ]
    }

    proptest! {
        /// Any order of startup and frontend messages never panics, every reply encodes, a closed
        /// session stays closed and quiet, and every Sync of a live session ends in ReadyForQuery.
        #[test]
        fn arbitrary_message_orderings_hold_the_protocol_rules(
            starts in proptest::collection::vec(startup(), 0..3),
            msgs in proptest::collection::vec(frontend(), 0..40),
        ) {
            let mut s = Session::new(Fake::default(), 7, 9);
            let mut sink = Vec::new();
            for st in starts {
                let out = s.on_startup(st);
                for m in &out.messages { prop_assert!(m.encode(&mut sink).is_ok()); }
            }
            for msg in msgs {
                let was_closed = s.is_closed();
                let was_ready = !was_closed && !s.awaiting_startup();
                let is_sync = msg == Frontend::Sync;
                let out = s.on_message(msg);
                for m in &out.messages { prop_assert!(m.encode(&mut sink).is_ok()); }
                if was_closed {
                    prop_assert!(out.messages.is_empty() && out.close);
                }
                if was_ready && is_sync {
                    prop_assert_eq!(out.messages.last(), Some(&Backend::ReadyForQuery(s.tx_status())));
                }
            }
        }

        /// Raw bytes go through the codec and then the session without a panic.
        #[test]
        fn arbitrary_bytes_through_codec_and_session_never_panic(
            bytes in proptest::collection::vec(any::<u8>(), 0..400),
        ) {
            let mut s = ready();
            let mut at = 0;
            while let Some(rest) = bytes.get(at..) {
                match decode_frontend(rest) {
                    Ok(Some((msg, used))) => { s.on_message(msg); at += used.max(1); }
                    Ok(None) => break,
                    Err(e) => { s.on_protocol_error(&e); break; }
                }
            }
        }
    }

    struct CancelRng;
    impl lattice_seams::Rng for CancelRng {
        fn next_u64(&mut self) -> u64 {
            17
        }
    }
    fn cancellation() -> (
        crate::cancel::CancelRegistry,
        crate::cancel::CancelKey,
        CancellationToken,
    ) {
        let mut registry = crate::cancel::CancelRegistry::new(1).unwrap();
        let key = registry.register(&mut CancelRng, 0, 100).unwrap();
        let token = registry.begin_query(key, 0).unwrap();
        (registry, key, token)
    }

    #[test]
    fn cancelled_simple_query_does_not_enter_legacy_handler_and_a_fresh_token_works() {
        let (mut registry, key, token) = cancellation();
        registry.cancel(key, 0);
        let mut session = ready();
        let out = session.on_message_with_cancellation(q("SELECT 1"), &token);
        assert_eq!(err_code(&out.messages[0]), "57014");
        assert_eq!(out.messages[1], Backend::ReadyForQuery(TxStatus::Idle));
        assert!(session.handler().executed.is_empty());
        let fresh = registry.begin_query(key, 0).unwrap();
        let out = session.on_message_with_cancellation(q("SELECT 1"), &fresh);
        assert_eq!(out.messages[1], Backend::DataRow(vec![Some(b"1".to_vec())]));
        assert_eq!(session.handler().executed, ["SELECT 1"]);
    }

    #[test]
    fn extended_parse_and_bind_errors_abort_the_transaction_and_sync_does_not_undo_it() {
        for (bad, code) in [
            (parse("", "BAD"), "42601"),
            (bind("", "missing", vec![]), "26000"),
        ] {
            let mut session = ready();
            session.on_message(q("BEGIN"));
            let out = session.on_message(bad);
            assert_eq!(err_code(&out.messages[0]), code);
            assert_eq!(session.tx_status(), TxStatus::Failed);
            assert_eq!(
                session.on_message(Frontend::Sync).messages,
                [Backend::ReadyForQuery(TxStatus::Failed)]
            );
            let out = session.on_message(q("SELECT 1"));
            assert_eq!(err_code(&out.messages[0]), "25P02");
            assert_eq!(session.handler().executed, ["BEGIN"]);
            session.on_message(q("ROLLBACK"));
            assert_eq!(session.tx_status(), TxStatus::Idle);
        }
    }

    #[test]
    fn cancelled_parse_aborts_active_transaction_and_cancelled_fresh_portal_cannot_resume() {
        let (mut registry, key, token) = cancellation();
        registry.cancel(key, 0);
        let mut session = ready();
        session.on_message(q("BEGIN"));
        let out = session.on_message_with_cancellation(parse("", "SELECT 1"), &token);
        assert_eq!(err_code(&out.messages[0]), "57014");
        assert_eq!(session.tx_status(), TxStatus::Failed);
        let mut session = ready();
        session.on_message(parse("s", "SELECT 3"));
        session.on_message(bind("p", "s", vec![]));
        let out = session.on_message_with_cancellation(exec("p", 0), &token);
        assert_eq!(err_code(&out.messages[0]), "57014");
        session.on_message(Frontend::Sync);
        assert_eq!(
            err_code(&session.on_message(exec("p", 0)).messages[0]),
            "34000"
        );
        assert!(session.handler().executed.is_empty());
    }

    #[test]
    fn cooperative_handler_receives_registry_token_but_late_cancellation_cannot_relabel_a_commit() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Cooperative {
            registry: Rc<RefCell<crate::cancel::CancelRegistry>>,
            key: crate::cancel::CancelKey,
            committed: bool,
            fallback: Fake,
        }
        impl QueryHandler for Cooperative {
            fn execute(
                &mut self,
                sql: &str,
                params: &[Option<Vec<u8>>],
            ) -> Result<QueryOutput, QueryError> {
                self.fallback.execute(sql, params)
            }
            fn describe(&mut self, sql: &str) -> Result<Vec<Column>, QueryError> {
                self.fallback.describe(sql)
            }
            fn execute_cancellable(
                &mut self,
                sql: &str,
                params: &[Option<Vec<u8>>],
                token: &CancellationToken,
            ) -> Result<QueryOutput, QueryError> {
                match sql {
                    "ABORT" => {
                        self.registry.borrow_mut().cancel(self.key, 0);
                        assert!(token.is_cancelled(), "handler received a detached token");
                        Err(QueryError::new("57014", "cooperative abort"))
                    }
                    "COMMIT" => {
                        self.committed = true; // Cancellation is observed only after our commit boundary.
                        self.registry.borrow_mut().cancel(self.key, 0);
                        assert!(token.is_cancelled());
                        Ok(QueryOutput {
                            tag: "COMMIT".into(),
                            ..QueryOutput::default()
                        })
                    }
                    _ => self.execute(sql, params),
                }
            }
        }
        let (registry, key, _) = cancellation();
        let registry = Rc::new(RefCell::new(registry));
        let handler = Cooperative {
            registry: registry.clone(),
            key,
            committed: false,
            fallback: Fake::default(),
        };
        let mut session = Session::new(handler, key.pid, key.secret);
        session.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        for sql in ["BEGIN", "ABORT", "ROLLBACK", "BEGIN"] {
            let token = registry.borrow_mut().begin_query(key, 0).unwrap();
            let out = session.on_message_with_cancellation(q(sql), &token);
            if sql == "ABORT" {
                assert_eq!(err_code(&out.messages[0]), "57014");
                assert_eq!(session.tx_status(), TxStatus::Failed);
            }
        }
        let token = registry.borrow_mut().begin_query(key, 0).unwrap();
        let out = session.on_message_with_cancellation(q("COMMIT"), &token);
        assert!(session.handler().committed);
        assert_eq!(
            out.messages,
            [
                Backend::CommandComplete("COMMIT".into()),
                Backend::ReadyForQuery(TxStatus::Idle)
            ]
        );
    }

    #[test]
    fn cancelled_resume_preserves_committed_returning_rows_and_completion() {
        struct Returning {
            committed: bool,
            calls: usize,
        }
        impl QueryHandler for Returning {
            fn execute(
                &mut self,
                _: &str,
                _: &[Option<Vec<u8>>],
            ) -> Result<QueryOutput, QueryError> {
                self.calls += 1;
                self.committed = true; // Simulated commit boundary before returning cached rows.
                Ok(QueryOutput {
                    columns: vec![col()],
                    rows: vec![vec![Some(b"11".to_vec())], vec![Some(b"22".to_vec())]],
                    tag: "INSERT 0 2".into(),
                })
            }
            fn describe(&mut self, _: &str) -> Result<Vec<Column>, QueryError> {
                Ok(vec![col()])
            }
        }
        let mut session = Session::new(
            Returning {
                committed: false,
                calls: 0,
            },
            1,
            2,
        );
        session.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        session.on_message(parse("s", "INSERT RETURNING"));
        session.on_message(bind("p", "s", vec![]));
        assert_eq!(
            session.on_message(exec("p", 1)).messages,
            [
                Backend::DataRow(vec![Some(b"11".to_vec())]),
                Backend::PortalSuspended,
            ]
        );
        assert!(session.handler().committed);
        let (mut registry, key, token) = cancellation();
        registry.cancel(key, 0);
        assert_eq!(
            session
                .on_message_with_cancellation(exec("p", 1), &token)
                .messages,
            [
                Backend::DataRow(vec![Some(b"22".to_vec())]),
                Backend::CommandComplete("INSERT 0 2".into()),
            ]
        );
        // A completed portal also returns its successful completion even with a cancelled token.
        assert_eq!(
            session
                .on_message_with_cancellation(exec("p", 0), &token)
                .messages,
            [Backend::CommandComplete("INSERT 0 2".into()),]
        );
        assert_eq!(session.handler().calls, 1);
        assert_eq!(session.tx_status(), TxStatus::Idle);
    }
}
