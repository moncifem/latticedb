//! PostgreSQL session state machine, sans I/O.
//!
//! [`Session`] sits on top of the [`pgwire`](crate::pgwire) codec. A caller decodes one message,
//! hands it to [`Session::on_startup`] or [`Session::on_message`], and writes out what comes back in
//! the returned [`Outcome`]. The session opens no socket, reads no clock and never blocks, so the
//! whole conversation can be driven from a test, a fuzzer or the simulator. Nothing in here panics on
//! any sequence of messages.
//!
//! What it does:
//! - startup: leaves host encryption to the caller and offers development trust or an explicit
//!   configured SCRAM exchange before successful startup
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
use crate::principal::{Principal, QueryContext};

use crate::pgwire::{
    Backend, Column, ENCRYPTION_DECLINED, Frontend, ProtoError, SaslPhase, Startup, Target,
    TxStatus,
};

/// The `server_version` reported to clients. Drivers parse it to pick features.
pub const SERVER_VERSION: &str = "16.0";

/// The most prepared statements one session may hold.
pub const MAX_STATEMENTS: usize = 1024;
/// The most portals one session may hold. Drivers keep a handful open; the old limit of 1,024 let
/// one connection pin a thousand copies of a large result.
pub const MAX_PORTALS: usize = 128;
/// The most bytes one session may keep in portals: the statement text and bound parameters of
/// every open portal plus the rows a suspended portal still has to send. A client that fills it
/// gets `53200` and the session stays usable after `Close`. It matches the default statement
/// memory budget, so one maximal result still fits but a hundred of them do not.
pub const MAX_PORTAL_BYTES: usize = 64 * 1024 * 1024;

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
///
/// Every call carries a [`QueryContext`] with the principal that authenticated the connection. The
/// session calls the handler only after authentication completed, so the principal is never a
/// placeholder for "not yet known".
pub trait QueryHandler {
    /// Runs one statement. `params` are the bound values of an extended query, empty for a simple one.
    fn execute(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError>;

    /// Checks a statement at `Parse` time and returns the columns it will produce (empty for none).
    fn describe(&mut self, ctx: &QueryContext<'_>, sql: &str) -> Result<Vec<Column>, QueryError>;

    /// Runs with a statement-local cancellation signal. Cooperative executors override this method
    /// and poll between bounded units of work, aborting before their commit boundary. The default
    /// checks before entering the legacy handler; it cannot preempt a handler that blocks inside it.
    /// A successful result is not changed into a cancellation error after the handler returns.
    fn execute_cancellable(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        check_cancellation(cancellation)?;
        self.execute(ctx, sql, params)
    }

    /// The wire session has failed the connection's open transaction block because of an error it
    /// raised itself, one that never reached [`QueryHandler::execute`] or [`QueryHandler::describe`]
    /// (a cancel seen before dispatch, an unsupported `Bind`, an unknown portal, a duplicate name).
    /// A handler that keeps its own transaction state must fail its block too, so a later `COMMIT` is
    /// answered `ROLLBACK` as in PostgreSQL. The default does nothing, which is right for a handler
    /// with no state of its own. Called at most once per failed block, only on the `InTransaction`
    /// to `Failed` transition, and also after errors the handler itself returned (it must tolerate
    /// being told twice).
    fn abort_transaction(&mut self, _ctx: &QueryContext<'_>) {}

    /// Describes with the same cooperative cancellation contract as execution.
    fn describe_cancellable(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Column>, QueryError> {
        check_cancellation(cancellation)?;
        self.describe(ctx, sql)
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
    SaslInitial,
    SaslFinal,
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
    /// Bytes of the session's portal budget this portal holds: its text and parameters, plus the
    /// rows still waiting in `Suspended`. Returned when the portal goes away.
    charged: usize,
}

const CELL_SLOT: usize = std::mem::size_of::<Option<Vec<u8>>>();

/// What a row costs in memory: the outer vector, one slot per cell and the cell bytes.
fn row_bytes(row: &Row) -> usize {
    let data = row
        .iter()
        .flatten()
        .fold(0usize, |n, c| n.saturating_add(c.len()));
    std::mem::size_of::<Row>()
        .saturating_add(row.len().saturating_mul(CELL_SLOT))
        .saturating_add(data)
}

/// What a portal costs before it holds any rows: its statement text and bound parameters.
fn portal_base_bytes(sql: &str, params: &[Option<Vec<u8>>]) -> usize {
    let data = params
        .iter()
        .flatten()
        .fold(0usize, |n, c| n.saturating_add(c.len()));
    sql.len()
        .saturating_add(params.len().saturating_mul(CELL_SLOT))
        .saturating_add(data)
        .saturating_add(64)
}

fn over_portal_budget() -> QueryError {
    QueryError::new(
        "53200",
        "too much memory held by open portals; close portals or fetch their rows",
    )
}

/// One client connection.
pub struct Session<H> {
    handler: H,
    pid: i32,
    secret: i32,
    phase: Phase,
    authentication: Option<crate::scram::ScramServer>,
    /// The identity this connection will have once startup completes. For SCRAM it is the account
    /// the exchange was selected for (absent for an unknown user); for trust it is the fixed
    /// development principal. It becomes `principal` only when `AuthenticationOk` is produced.
    pending: Option<Principal>,
    /// Set once, when authentication completes; never changed or cleared afterwards.
    principal: Option<Principal>,
    tx: TxStatus,
    /// After an error in the extended flow: ignore everything up to the next `Sync`.
    discarding: bool,
    statements: BTreeMap<String, Statement>,
    portals: BTreeMap<String, Portal>,
    /// Sum of `Portal::charged` over `portals`.
    portal_bytes: usize,
    portal_budget: usize,
}

impl<H: QueryHandler> Session<H> {
    /// A session in explicit local development trust: no credential is checked and the connection
    /// is [`Principal::local_development`], whatever `user` the client names. The host must have
    /// configured this mode and restricted it to loopback. `pid` and `secret` go out in
    /// `BackendKeyData`.
    pub fn local_trust(handler: H, pid: i32, secret: i32) -> Self {
        Self::build(
            handler,
            pid,
            secret,
            None,
            Some(Principal::local_development()),
        )
    }

    fn build(
        handler: H,
        pid: i32,
        secret: i32,
        authentication: Option<crate::scram::ScramServer>,
        pending: Option<Principal>,
    ) -> Self {
        Self {
            handler,
            pid,
            secret,
            phase: Phase::AwaitingStartup,
            authentication,
            pending,
            principal: None,
            tx: TxStatus::Idle,
            discarding: false,
            statements: BTreeMap::new(),
            portals: BTreeMap::new(),
            portal_bytes: 0,
            portal_budget: MAX_PORTAL_BYTES,
        }
    }

    /// Replaces the per-session portal byte budget ([`MAX_PORTAL_BYTES`] by default).
    #[must_use]
    pub fn with_portal_budget(mut self, bytes: usize) -> Self {
        self.portal_budget = bytes;
        self
    }

    /// Drops a portal and returns what it held to the budget.
    fn remove_portal(&mut self, name: &str) {
        if let Some(portal) = self.portals.remove(name) {
            self.portal_bytes = self.portal_bytes.saturating_sub(portal.charged);
        }
    }

    /// PostgreSQL destroys every portal when the transaction ends.
    fn drop_all_portals(&mut self) {
        self.portals.clear();
        self.portal_bytes = 0;
    }

    /// Requires a configured SCRAM exchange before any query or successful startup output.
    /// The host must select the verifier from the startup user and provide protected transport.
    /// `account` is the principal returned by [`Accounts::exchange`](crate::auth::Accounts::exchange):
    /// `None` for a synthetic unknown-user verifier, which must never succeed. The startup `user`
    /// must equal the account's name or startup is refused.
    pub fn with_scram(
        handler: H,
        pid: i32,
        secret: i32,
        exchange: crate::scram::ScramServer,
        account: Option<Principal>,
    ) -> Self {
        Self::build(handler, pid, secret, Some(exchange), account)
    }

    /// The authenticated principal, or `None` until authentication has completed. It never changes
    /// afterwards: there is no re-authentication or role switch on a connection.
    pub fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }

    /// The only permitted frontend password layout during an authentication exchange.
    pub fn sasl_phase(&self) -> Option<SaslPhase> {
        match self.phase {
            Phase::SaslInitial => Some(SaslPhase::InitialResponse),
            Phase::SaslFinal => Some(SaslPhase::Response),
            _ => None,
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
                let mut users = params.iter().filter(|(k, _)| k == "user");
                let user = users
                    .next()
                    .map(|(_, v)| v.as_str())
                    .filter(|v| !v.is_empty());
                let Some(user) = user else {
                    return self.fatal(
                        "28000",
                        "no user name specified in the startup packet".to_string(),
                    );
                };
                // The startup name only selects an account; it is never the identity. A second
                // `user` is refused. In a SCRAM session the refusal is the same 28P01 an unknown
                // account ends with, so the error does not tell known from unknown names.
                if users.next().is_some() {
                    return if self.authentication.is_some() {
                        self.fatal("28P01", "password authentication failed".to_string())
                    } else {
                        self.fatal(
                            "28000",
                            "more than one user in the startup packet".to_string(),
                        )
                    };
                }
                // A startup parameter that changes behaviour is refused when the executor cannot honour
                // it, as `SET` refuses it, instead of being accepted and ignored. The answer depends
                // on the parameter alone, so it does not tell known accounts from unknown ones.
                for (key, value) in &params {
                    if !lattice_exec::setting_value_supported(key, value) {
                        return self.fatal(
                            "22023",
                            format!("invalid value for parameter \"{key}\": \"{value}\""),
                        );
                    }
                }
                // A name that is not the account the exchange was built for can never authenticate.
                // Drop the pending principal and let the exchange run and fail at its end, exactly
                // like an unknown account, instead of answering at once (no known-vs-unknown oracle
                // for a host that wires the selection differently).
                if self.authentication.is_some()
                    && self.pending.as_ref().is_some_and(|p| p.name() != user)
                {
                    self.pending = None;
                }
                if self.authentication.is_some() {
                    self.phase = Phase::SaslInitial;
                    Outcome {
                        messages: vec![crate::scram::ScramServer::offer()],
                        ..Outcome::default()
                    }
                } else {
                    self.startup_success()
                }
            }
        }
    }

    fn startup_success(&mut self) -> Outcome {
        // The only place a principal is published. No pending principal means no way in.
        let Some(principal) = self.pending.take() else {
            return self.fatal("28000", "no authenticated principal".to_string());
        };
        self.principal = Some(principal);
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

    fn authenticate(&mut self, message: Frontend) -> Outcome {
        if matches!(message, Frontend::Terminate) {
            self.authentication = None;
            self.phase = Phase::Closed;
            return closed();
        }
        let Some(exchange) = self.authentication.as_mut() else {
            return self.fatal("28P01", "password authentication failed".into());
        };
        let result = match (self.phase, message) {
            (
                Phase::SaslInitial,
                Frontend::SaslInitialResponse {
                    mechanism,
                    data: Some(data),
                },
            ) => exchange.initial(&mechanism, &data),
            (Phase::SaslFinal, Frontend::SaslResponse(data)) => exchange.finish(&data),
            _ => Err(crate::scram::ScramError::InvalidState),
        };
        match result {
            Ok(message) if self.phase == Phase::SaslInitial => {
                self.phase = Phase::SaslFinal;
                Outcome {
                    messages: vec![message],
                    ..Outcome::default()
                }
            }
            Ok(message) if self.pending.is_some() && exchange.is_authenticated() => {
                self.authentication = None;
                let mut outcome = self.startup_success();
                outcome.messages.insert(0, message);
                outcome
            }
            _ => {
                self.authentication = None;
                self.fatal("28P01", "password authentication failed".into())
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
            Phase::SaslInitial | Phase::SaslFinal => return self.authenticate(msg),
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
                    self.fail_block();
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
        self.authentication = None;
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
        let result = check_cancellation(cancellation).and_then(|()| {
            let principal = self.principal.as_ref().ok_or_else(no_principal)?;
            self.handler.execute_cancellable(
                &QueryContext::new(principal),
                sql,
                params,
                cancellation,
            )
        });
        match result {
            Ok(result) => {
                match result.tag.as_str() {
                    "BEGIN" if self.tx == TxStatus::Idle => self.tx = TxStatus::InTransaction,
                    "COMMIT" | "ROLLBACK" => {
                        self.tx = TxStatus::Idle;
                        self.drop_all_portals();
                    }
                    _ => {}
                }
                Ok(result)
            }
            Err(e) => {
                self.fail_block();
                Err(e)
            }
        }
    }

    /// Fails an open transaction block and tells the handler, so both sides agree.
    fn fail_block(&mut self) {
        if self.tx == TxStatus::InTransaction {
            self.tx = TxStatus::Failed;
            if let Some(principal) = self.principal.as_ref() {
                self.handler
                    .abort_transaction(&QueryContext::new(principal));
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
                    let principal = self.principal.as_ref().ok_or_else(no_principal)?;
                    self.handler.describe_cancellable(
                        &QueryContext::new(principal),
                        &query,
                        cancellation,
                    )?
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
                let charged = portal_base_bytes(&stmt.sql, &params);
                let made = Portal {
                    sql: stmt.sql.clone(),
                    params,
                    columns: stmt.columns.clone(),
                    state: PortalState::Fresh,
                    charged,
                };
                // Binding over the unnamed portal replaces it, so its bytes do not count against
                // the new one. Check first: a refused bind must leave the old portal in place.
                let replaced = self.portals.get(&portal).map_or(0, |p| p.charged);
                let others = self.portal_bytes.saturating_sub(replaced);
                if others.saturating_add(charged) > self.portal_budget {
                    return Err(over_portal_budget());
                }
                self.remove_portal(&portal);
                self.portal_bytes += charged;
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
                self.remove_portal(&name);
                out.push(Backend::CloseComplete);
            }
            Frontend::Execute { portal, max_rows } => {
                if let Err(error) = self.execute_portal(&portal, max_rows, out, cancellation) {
                    if error.code == "57014" {
                        self.remove_portal(&portal);
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
        let fresh = matches!(portal.state, PortalState::Fresh);
        let (mut rows, tag) = match &portal.state {
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
                // Never fail after running: a statement with side effects (INSERT ... RETURNING)
                // would be committed while the client sees an error. So refuse before it starts when
                // the session already holds more than its budget. Once it ran, its result is always
                // kept (it is bounded by the statement memory budget), which makes the budget soft:
                // retained bytes stay under the budget plus one result.
                if self.portal_bytes > self.portal_budget {
                    return Err(over_portal_budget());
                }
                let result = self.run(&sql, &params, cancellation)?;
                let rows: VecDeque<Row> = if result.columns.is_empty() {
                    VecDeque::new()
                } else {
                    result.rows.into()
                };
                (rows, result.tag)
            }
            PortalState::Suspended(..) => {
                // Move the rows out instead of copying them: a client that reads one row at a time
                // would otherwise pay for the whole remainder on every message.
                let Some(p) = self.portals.get_mut(name) else {
                    return Ok(());
                };
                match std::mem::replace(&mut p.state, PortalState::Done(String::new())) {
                    PortalState::Suspended(rows, tag) => (rows, tag),
                    other => {
                        p.state = other;
                        return Ok(());
                    }
                }
            }
            PortalState::Done(tag) => (VecDeque::new(), tag.clone()),
        };
        let limit = match usize::try_from(max_rows) {
            Ok(0) | Err(_) => usize::MAX,
            Ok(n) => n,
        };
        let mut batch = Vec::new();
        let mut sent_bytes = 0usize;
        while batch.len() < limit {
            let Some(row) = rows.pop_front() else { break };
            sent_bytes = sent_bytes.saturating_add(row_bytes(&row));
            batch.push(row);
        }
        let Some(portal) = self.portals.get_mut(name) else {
            // The statement ended the transaction and took the portal with it.
            out.extend(batch.into_iter().map(Backend::DataRow));
            out.push(Backend::CommandComplete(tag));
            return Ok(());
        };
        // What stays behind is charged to the session. The first run measures the rows once; later
        // runs subtract what they sent, so no message costs more than the rows it moves.
        let base = portal_base_bytes(&portal.sql, &portal.params);
        let kept = if rows.is_empty() {
            0
        } else if fresh {
            rows.iter()
                .fold(0usize, |n, r| n.saturating_add(row_bytes(r)))
        } else {
            portal
                .charged
                .saturating_sub(base)
                .saturating_sub(sent_bytes)
        };
        let charged = base.saturating_add(kept);
        let others = self.portal_bytes.saturating_sub(portal.charged);
        portal.charged = charged;
        self.portal_bytes = others.saturating_add(charged);
        out.extend(batch.into_iter().map(Backend::DataRow));
        portal.state = if rows.is_empty() {
            out.push(Backend::CommandComplete(tag.clone()));
            PortalState::Done(tag)
        } else {
            out.push(Backend::PortalSuspended);
            PortalState::Suspended(rows, tag)
        };
        Ok(())
    }
}

/// A ready session always has a principal; if it somehow did not, the handler is not called.
fn no_principal() -> QueryError {
    QueryError::new("28000", "no authenticated principal")
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
        /// The principal each `execute` and `describe` call carried, as `Display` text.
        seen: Vec<String>,
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
            ctx: &QueryContext<'_>,
            sql: &str,
            params: &[Option<Vec<u8>>],
        ) -> Result<QueryOutput, QueryError> {
            self.executed.push(sql.to_string());
            self.seen.push(ctx.principal.to_string());
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

        fn describe(
            &mut self,
            ctx: &QueryContext<'_>,
            sql: &str,
        ) -> Result<Vec<Column>, QueryError> {
            self.seen.push(ctx.principal.to_string());
            match sql.trim() {
                "BAD" => Err(QueryError::new("42601", "syntax error")),
                s if s.starts_with("SELECT") => Ok(vec![col()]),
                _ => Ok(Vec::new()),
            }
        }
    }

    fn ready() -> Session<Fake> {
        let mut s = Session::local_trust(Fake::default(), 1, 2);
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
        let mut s = Session::local_trust(Fake::default(), 1, 2);
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
        let mut s = Session::local_trust(Fake::default(), 1, 2);
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
        let mut s = Session::local_trust(Fake::default(), 1, 2);
        let out = s.on_startup(Startup::Params(vec![("database".into(), "d".into())]));
        assert_eq!(err_code(&out.messages[0]), "28000");
        assert!(out.close && s.is_closed());
    }

    #[test]
    fn startup_parameters_the_executor_cannot_honour_are_refused() {
        let params = |extra: (&str, &str)| {
            Startup::Params(vec![
                ("user".into(), "u".into()),
                (extra.0.into(), extra.1.into()),
            ])
        };
        for ok in [
            ("TimeZone", "UTC"),
            ("TimeZone", "Etc/UTC"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("application_name", "psql"),
            ("extra_float_digits", "3"),
            ("database", "d"),
        ] {
            let mut s = Session::local_trust(Fake::default(), 1, 2);
            let out = s.on_startup(params(ok));
            assert!(!out.close && !s.is_closed(), "{ok:?} should connect");
        }
        for bad in [
            ("TimeZone", "America/New_York"),
            ("client_encoding", "LATIN1"),
            ("DateStyle", "SQL, DMY"),
        ] {
            let mut s = Session::local_trust(Fake::default(), 1, 2);
            let out = s.on_startup(params(bad));
            assert_eq!(err_code(&out.messages[0]), "22023", "{bad:?}");
            assert!(out.close && s.is_closed());
        }
    }

    #[test]
    fn messages_before_startup_and_a_second_startup_are_fatal() {
        let mut s = Session::local_trust(Fake::default(), 1, 2);
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
        let mut s = Session::local_trust(Fake::default(), 1, 2);
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
            let mut s = Session::local_trust(Fake::default(), 7, 9);
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
                ctx: &QueryContext<'_>,
                sql: &str,
                params: &[Option<Vec<u8>>],
            ) -> Result<QueryOutput, QueryError> {
                self.fallback.execute(ctx, sql, params)
            }
            fn describe(
                &mut self,
                ctx: &QueryContext<'_>,
                sql: &str,
            ) -> Result<Vec<Column>, QueryError> {
                self.fallback.describe(ctx, sql)
            }
            fn execute_cancellable(
                &mut self,
                ctx: &QueryContext<'_>,
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
                    _ => self.execute(ctx, sql, params),
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
        let mut session = Session::local_trust(handler, key.pid, key.secret);
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
                _: &QueryContext<'_>,
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
            fn describe(
                &mut self,
                _: &QueryContext<'_>,
                _: &str,
            ) -> Result<Vec<Column>, QueryError> {
                Ok(vec![col()])
            }
        }
        let mut session = Session::local_trust(
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

    fn scram_session(known: bool) -> Session<Fake> {
        // Published RFC7677 account, independently provisioned verifier in auth tests.
        let verifier = crate::scram::Verifier::from_prepared_password(
            b"pencil",
            &[
                91, 109, 153, 104, 157, 18, 53, 142, 236, 160, 75, 20, 18, 54, 250, 129,
            ],
            4096,
        )
        .unwrap();
        let exchange = crate::scram::ScramServer::with_nonce_suffix(
            verifier,
            "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0",
        )
        .unwrap();
        Session::with_scram(
            Fake::default(),
            17,
            23,
            exchange,
            known.then(|| Principal::account("user")),
        )
    }
    fn scram_first() -> Frontend {
        Frontend::SaslInitialResponse {
            mechanism: "SCRAM-SHA-256".into(),
            data: Some(b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO".to_vec()),
        }
    }
    fn scram_final() -> Frontend {
        Frontend::SaslResponse(b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=".to_vec())
    }
    #[test]
    fn scram_success_orders_final_before_startup_and_gates_query_handler() {
        let mut session = scram_session(true);
        let offer = session.on_startup(Startup::Params(vec![("user".into(), "user".into())]));
        assert_eq!(
            offer.messages,
            [Backend::AuthenticationSasl(vec!["SCRAM-SHA-256".into()])]
        );
        assert_eq!(session.sasl_phase(), Some(SaslPhase::InitialResponse));
        let first = session.on_message(scram_first());
        assert_eq!(first.messages.len(), 1);
        assert!(matches!(
            first.messages.first(),
            Some(Backend::AuthenticationSaslContinue(_))
        ));
        assert!(session.handler().executed.is_empty());
        let final_out = session.on_message(scram_final());
        assert_eq!(
            final_out.messages.first(),
            Some(&Backend::AuthenticationSaslFinal(
                b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=".to_vec()
            ))
        );
        assert_eq!(final_out.messages.get(1), Some(&Backend::AuthenticationOk));
        assert!(final_out.messages.contains(&Backend::BackendKeyData {
            pid: 17,
            secret: 23
        }));
        assert_eq!(session.sasl_phase(), None);
        assert!(!final_out.close);
        session.on_message(q("SELECT 1"));
        assert_eq!(session.handler().executed, ["SELECT 1"]);
    }
    #[test]
    fn scram_early_queries_wrong_order_and_unknown_proof_are_terminal() {
        for input in [
            q("SELECT 1"),
            Frontend::Sync,
            Frontend::Password(b"pencil".to_vec()),
            scram_final(),
            Frontend::SaslInitialResponse {
                mechanism: "SCRAM-SHA-256-PLUS".into(),
                data: Some(vec![]),
            },
        ] {
            let mut session = scram_session(true);
            session.on_startup(Startup::Params(vec![("user".into(), "user".into())]));
            let out = session.on_message(input);
            assert!(out.close);
            assert_eq!(err_code(&out.messages[0]), "28P01");
            assert!(session.handler().executed.is_empty());
            assert!(session.on_message(scram_first()).messages.is_empty());
        }
        // Even a mathematically valid decoy proof must never authenticate an unknown account.
        let mut session = scram_session(false);
        session.on_startup(Startup::Params(vec![("user".into(), "missing".into())]));
        session.on_message(scram_first());
        let out = session.on_message(scram_final());
        assert!(out.close);
        assert_eq!(
            out.messages,
            [Backend::ErrorResponse {
                severity: "FATAL".into(),
                code: "28P01".into(),
                message: "password authentication failed".into()
            }]
        );
    }

    #[test]
    fn scram_principal_is_the_store_account_and_reaches_every_handler_call() {
        let mut session = scram_session(true);
        assert!(session.principal().is_none());
        session.on_startup(Startup::Params(vec![("user".into(), "user".into())]));
        session.on_message(scram_first());
        assert!(session.principal().is_none(), "not published mid-exchange");
        session.on_message(scram_final());
        assert_eq!(session.principal(), Some(&Principal::account("user")));
        session.on_message(q("SELECT 1"));
        session.on_message(Frontend::Parse {
            name: "s".into(),
            query: "SELECT 3".into(),
            param_types: vec![],
        });
        // One execute and one describe, both as the authenticated account.
        assert_eq!(session.handler().seen, ["account:user", "account:user"]);
        assert_eq!(session.principal(), Some(&Principal::account("user")));
    }

    #[test]
    fn startup_user_that_differs_from_the_exchange_account_fails_like_an_unknown_account() {
        // A second `user` is refused at once with the generic failure.
        for params in [
            vec![
                ("user".to_string(), "user".to_string()),
                ("user".to_string(), "other".to_string()),
            ],
            vec![
                ("user".to_string(), "user".to_string()),
                ("user".to_string(), "user".to_string()),
            ],
        ] {
            let mut session = scram_session(true);
            let out = session.on_startup(Startup::Params(params));
            assert!(out.close);
            assert_eq!(err_code(&out.messages[0]), "28P01");
            assert!(session.principal().is_none());
            assert!(session.on_message(scram_first()).messages.is_empty());
            assert!(session.handler().seen.is_empty());
        }
        // A different single name: the exchange is offered and fails at the end, even with the
        // correct proof for the account the verifier belongs to, and the failure bytes equal the
        // unknown-account failure.
        let mut mismatch = scram_session(true);
        let offer = mismatch.on_startup(Startup::Params(vec![("user".into(), "other".into())]));
        assert!(!offer.close);
        mismatch.on_message(scram_first());
        let failed = mismatch.on_message(scram_final());
        let mut unknown = scram_session(false);
        unknown.on_startup(Startup::Params(vec![("user".into(), "other".into())]));
        unknown.on_message(scram_first());
        let expected = unknown.on_message(scram_final());
        assert!(failed.close);
        assert_eq!(failed, expected);
        assert_eq!(err_code(&failed.messages[0]), "28P01");
        assert!(mismatch.principal().is_none());
        assert!(mismatch.handler().seen.is_empty());
    }

    #[test]
    fn unauthenticated_input_never_reaches_the_handler_or_publishes_a_principal() {
        let mut session = scram_session(true);
        // Before startup.
        let out = session.on_message(q("SELECT 1"));
        assert_eq!(err_code(&out.messages[0]), "08P01");
        let mut session = scram_session(true);
        session.on_startup(Startup::Params(vec![("user".into(), "user".into())]));
        // A wrong proof ends the exchange without a principal.
        session.on_message(scram_first());
        let bad = Frontend::SaslResponse(b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_vec());
        let out = session.on_message(bad);
        assert!(out.close);
        assert!(session.principal().is_none());
        assert!(session.on_message(q("SELECT 1")).messages.is_empty());
        assert!(session.handler().seen.is_empty());
    }

    #[test]
    fn trust_mode_yields_the_labeled_development_principal_whatever_user_is_named() {
        let mut session = Session::local_trust(Fake::default(), 1, 2);
        assert!(session.principal().is_none());
        session.on_startup(Startup::Params(vec![("user".into(), "root".into())]));
        assert_eq!(session.principal(), Some(&Principal::local_development()));
        session.on_message(q("SELECT 1"));
        assert_eq!(session.handler().seen, ["local-development"]);
        // Trust still needs a non-empty, single `user`.
        let mut session = Session::local_trust(Fake::default(), 1, 2);
        let out = session.on_startup(Startup::Params(vec![
            ("user".into(), "a".into()),
            ("user".into(), "b".into()),
        ]));
        assert_eq!(err_code(&out.messages[0]), "28000");
        assert!(session.principal().is_none());
    }

    #[test]
    fn a_ready_session_without_a_principal_fails_closed() {
        let mut session = Session::local_trust(Fake::default(), 1, 2);
        session.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        // Test-only: simulate a bug that lost the principal after startup.
        session.principal = None;
        let out = session.on_message(q("SELECT 1"));
        assert_eq!(err_code(&out.messages[0]), "28000");
        assert!(session.handler().executed.is_empty());
    }

    /// `ROWS <n> <bytes>` returns n rows of that many bytes; `BEGIN`/`COMMIT` return their tags.
    #[derive(Default)]
    struct Wide {
        /// `INSERT` statements that ran (each returns one row, like `INSERT ... RETURNING`).
        inserts: usize,
        /// Times the session asked the handler to fail its open block.
        aborts: usize,
    }

    impl QueryHandler for Wide {
        fn abort_transaction(&mut self, _ctx: &QueryContext<'_>) {
            self.aborts += 1;
        }

        fn execute(
            &mut self,
            _ctx: &QueryContext<'_>,
            sql: &str,
            _params: &[Option<Vec<u8>>],
        ) -> Result<QueryOutput, QueryError> {
            let words: Vec<&str> = sql.split_whitespace().collect();
            if words.first() == Some(&"INSERT") {
                self.inserts += 1;
                return Ok(QueryOutput {
                    columns: vec![col()],
                    rows: vec![vec![Some(b"1".to_vec())]],
                    tag: "INSERT 0 1".into(),
                });
            }
            if let ["ROWS", n, width] = words[..] {
                let n: usize = n.parse().unwrap();
                let width: usize = width.parse().unwrap();
                return Ok(QueryOutput {
                    columns: vec![col()],
                    rows: (0..n).map(|_| vec![Some(vec![b'x'; width])]).collect(),
                    tag: format!("SELECT {n}"),
                });
            }
            Ok(QueryOutput {
                tag: sql.trim().to_ascii_uppercase(),
                ..QueryOutput::default()
            })
        }

        fn describe(
            &mut self,
            _ctx: &QueryContext<'_>,
            sql: &str,
        ) -> Result<Vec<Column>, QueryError> {
            Ok(if sql.starts_with("ROWS") || sql.starts_with("INSERT") {
                vec![col()]
            } else {
                Vec::new()
            })
        }
    }

    fn wide_session() -> Session<Wide> {
        let mut s = Session::local_trust(Wide::default(), 1, 2);
        s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        s
    }

    /// The heap address of the first cell of the `index`th row still waiting in portal `p`.
    fn waiting_row_address(session: &Session<Wide>, index: usize) -> usize {
        let PortalState::Suspended(rows, _) = &session.portals["p"].state else {
            panic!("portal must be suspended");
        };
        rows[index][0].as_ref().unwrap().as_ptr() as usize
    }

    #[test]
    fn reading_a_portal_row_by_row_never_copies_the_remaining_rows() {
        // A deep copy of the remainder gives every row a new heap buffer. After each Execute, the
        // row that was second in line must be the same allocation now at the front.
        let mut s = wide_session();
        s.on_message(parse("", "ROWS 10000 8"));
        s.on_message(bind("p", "", vec![]));
        assert_eq!(s.on_message(exec("p", 1)).messages.len(), 2);
        for _ in 0..500 {
            let second = waiting_row_address(&s, 1);
            let last = waiting_row_address(&s, 9000 - 1 - 500);
            let out = s.on_message(exec("p", 1));
            assert_eq!(out.messages.len(), 2);
            assert_eq!(out.messages[1], Backend::PortalSuspended);
            assert_eq!(waiting_row_address(&s, 0), second);
            // The tail did not move either, so nothing was rebuilt.
            assert_eq!(waiting_row_address(&s, 9000 - 2 - 500), last);
        }
        // Reading it all out still yields every row exactly once.
        let rest = s.on_message(exec("p", 0));
        assert_eq!(rest.messages.len(), 10_000 - 501 + 1);
        assert_eq!(
            rest.messages.last(),
            Some(&Backend::CommandComplete("SELECT 10000".into()))
        );
    }

    #[test]
    fn portal_bytes_follow_the_rows_still_waiting_and_return_when_the_portal_goes() {
        let mut s = wide_session();
        s.on_message(parse("", "ROWS 100 1000"));
        s.on_message(bind("p", "", vec![]));
        let base = s.portal_bytes;
        assert_eq!(base, s.portals["p"].charged);
        s.on_message(exec("p", 10));
        let ninety = s.portal_bytes - base;
        assert!(ninety >= 90 * 1000, "90 rows of 1000 bytes are retained");
        assert!(
            ninety < 100 * 1000,
            "the ten sent rows are no longer charged"
        );
        s.on_message(exec("p", 10));
        let eighty = s.portal_bytes - base;
        assert_eq!(ninety - eighty, ninety / 90 * 10);
        // Reading to the end releases everything but the portal's own text.
        s.on_message(exec("p", 0));
        assert_eq!(s.portal_bytes, base);
        s.on_message(Frontend::Close(Target::Portal("p".into())));
        assert_eq!(s.portal_bytes, 0);
    }

    #[test]
    fn a_session_over_its_portal_budget_refuses_before_running_and_stays_usable() {
        let mut s = wide_session().with_portal_budget(64 * 1024);
        s.on_message(parse("big", "ROWS 100 1000"));
        s.on_message(parse("write", "INSERT RETURNING"));
        s.on_message(bind("p", "big", vec![]));
        s.on_message(bind("w", "write", vec![]));
        // The first result is always admitted once it ran: the budget is soft by one result.
        let out = s.on_message(exec("p", 1));
        assert_eq!(out.messages.last(), Some(&Backend::PortalSuspended));
        assert!(s.portal_bytes > 64 * 1024);
        // The next statement is refused before it starts, so it has no side effect.
        let out = s.on_message(exec("w", 0));
        assert_eq!(err_code(&out.messages[0]), "53200");
        assert_eq!(s.handler().inserts, 0);
        assert_eq!(s.handler().aborts, 0, "no block was open");
        s.on_message(Frontend::Sync);
        // Closing the big portal frees the budget, and the same portal then runs.
        s.on_message(Frontend::Close(Target::Portal("p".into())));
        let out = s.on_message(exec("w", 0));
        assert_eq!(out.messages.len(), 2);
        assert_eq!(s.handler().inserts, 1);
        // A result read to the end in one message is never held, whatever its size.
        s.on_message(parse("rows", "ROWS 100 1000"));
        s.on_message(bind("r", "rows", vec![]));
        assert_eq!(s.on_message(exec("r", 0)).messages.len(), 101);
    }

    #[test]
    fn a_refusal_inside_a_block_fails_the_handlers_block_too() {
        let mut s = wide_session().with_portal_budget(64 * 1024);
        s.on_message(parse("begin", "BEGIN"));
        s.on_message(bind("b", "begin", vec![]));
        s.on_message(exec("b", 0));
        assert_eq!(s.tx_status(), TxStatus::InTransaction);
        s.on_message(parse("big", "ROWS 100 1000"));
        s.on_message(parse("write", "INSERT RETURNING"));
        s.on_message(bind("p", "big", vec![]));
        s.on_message(bind("w", "write", vec![]));
        s.on_message(exec("p", 1));
        let out = s.on_message(exec("w", 0));
        assert_eq!(err_code(&out.messages[0]), "53200");
        assert_eq!(s.handler().inserts, 0);
        // The session's block failed, and so did the handler's: a COMMIT cannot commit it.
        assert_eq!(s.handler().aborts, 1);
        s.on_message(Frontend::Sync);
        assert_eq!(s.tx_status(), TxStatus::Failed);
        // A refused Bind inside a block is a wire-session error too.
        let mut t = wide_session().with_portal_budget(64 * 1024);
        t.on_message(parse("begin", "BEGIN"));
        t.on_message(bind("b", "begin", vec![]));
        t.on_message(exec("b", 0));
        t.on_message(parse("big", "ROWS 100 1000"));
        t.on_message(bind("p", "big", vec![]));
        t.on_message(exec("p", 1));
        let out = t.on_message(bind("q", "big", vec![]));
        assert_eq!(err_code(&out.messages[0]), "53200");
        assert_eq!(t.handler().aborts, 1);
    }

    #[test]
    fn a_client_that_opens_many_suspended_portals_hits_the_budget_not_the_heap() {
        let mut s = wide_session().with_portal_budget(1024 * 1024);
        s.on_message(parse("s", "ROWS 500 1000"));
        let mut refused = 0;
        for i in 0..MAX_PORTALS {
            let name = format!("p{i}");
            let bound = s.on_message(bind(&name, "s", vec![]));
            let out = s.on_message(exec(&name, 1));
            if err_code_of(&bound.messages) == Some("53200")
                || err_code_of(&out.messages) == Some("53200")
            {
                refused += 1;
                s.on_message(Frontend::Sync);
            }
            // Soft cap: the budget plus the one result admitted while under it.
            assert!(s.portal_bytes <= 1024 * 1024 + 600_000);
        }
        // Each portal keeps about 500 KB, so three fit before the session is over budget.
        assert!(refused >= MAX_PORTALS - 4, "refused {refused}");
        // Closing frees the budget for new work.
        let names: Vec<String> = s.portals.keys().cloned().collect();
        for name in names {
            s.on_message(Frontend::Close(Target::Portal(name)));
        }
        assert_eq!(s.portal_bytes, 0);
        s.on_message(bind("again", "s", vec![]));
        let out = s.on_message(exec("again", 1));
        assert_eq!(out.messages.last(), Some(&Backend::PortalSuspended));
    }

    fn err_code_of(messages: &[Backend]) -> Option<&str> {
        messages.iter().find_map(|m| match m {
            Backend::ErrorResponse { code, .. } => Some(code.as_str()),
            _ => None,
        })
    }

    #[test]
    fn binding_more_than_max_portals_is_refused_and_replacing_the_unnamed_portal_is_not() {
        let mut s = wide_session();
        s.on_message(parse("s", "ROWS 1 1"));
        for i in 0..MAX_PORTALS {
            let out = s.on_message(bind(&format!("p{i}"), "s", vec![]));
            assert_eq!(out.messages, vec![Backend::BindComplete]);
        }
        let out = s.on_message(bind("one-more", "s", vec![]));
        assert_eq!(err_code(&out.messages[0]), "54000");
        s.on_message(Frontend::Sync);
        // The unnamed portal can be rebound any number of times without growing the charge.
        let mut t = wide_session();
        t.on_message(parse("s", "ROWS 1 1"));
        t.on_message(bind("", "s", vec![]));
        let once = t.portal_bytes;
        for _ in 0..50 {
            t.on_message(bind("", "s", vec![]));
        }
        assert_eq!(t.portal_bytes, once);
    }

    #[test]
    fn commit_and_rollback_drop_every_portal_and_its_bytes() {
        let mut s = wide_session();
        s.on_message(parse("rows", "ROWS 50 100"));
        s.on_message(parse("begin", "BEGIN"));
        s.on_message(parse("commit", "COMMIT"));
        s.on_message(parse("rollback", "ROLLBACK"));
        for end in ["commit", "rollback"] {
            s.on_message(bind("b", "begin", vec![]));
            s.on_message(exec("b", 0));
            s.on_message(bind("p", "rows", vec![]));
            s.on_message(exec("p", 1));
            assert_eq!(s.portals.len(), 2);
            assert!(s.portal_bytes > 5000);
            s.on_message(bind("e", end, vec![]));
            let out = s.on_message(exec("e", 0));
            assert_eq!(
                out.messages,
                vec![Backend::CommandComplete(end.to_uppercase())]
            );
            assert!(s.portals.is_empty());
            assert_eq!(s.portal_bytes, 0);
            let out = s.on_message(exec("p", 0));
            assert_eq!(err_code(&out.messages[0]), "34000");
            s.on_message(Frontend::Sync);
        }
    }

    #[test]
    fn a_refused_rebind_keeps_the_portal_it_would_have_replaced() {
        let mut s = wide_session().with_portal_budget(64 * 1024);
        s.on_message(parse("big", "ROWS 100 1000"));
        s.on_message(parse("small", "ROWS 3 10"));
        s.on_message(bind("", "small", vec![]));
        s.on_message(bind("hog", "big", vec![]));
        s.on_message(exec("hog", 1));
        assert!(s.portal_bytes > 64 * 1024);
        let before = s.portal_bytes;
        let out = s.on_message(bind("", "small", vec![]));
        assert_eq!(err_code(&out.messages[0]), "53200");
        s.on_message(Frontend::Sync);
        // The unnamed portal is still there and still charged.
        assert!(s.portals.contains_key(""));
        assert_eq!(s.portal_bytes, before);
        // Once the hog is closed the old portal still runs and the same rebind works.
        s.on_message(Frontend::Close(Target::Portal("hog".into())));
        assert_eq!(s.on_message(exec("", 0)).messages.len(), 4);
        assert_eq!(
            s.on_message(bind("", "small", vec![])).messages,
            vec![Backend::BindComplete]
        );
    }
}
