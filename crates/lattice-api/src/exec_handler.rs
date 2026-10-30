//! A [`QueryHandler`] that runs SQL through `lattice-exec`.
//!
//! The handler owns one [`ExecSession`] (the transaction block, the actor's row policy and the
//! statement limits of one connection) and shares the [`Database`] with the other connections of a
//! single-threaded host. It is **not wired into any listener**: `latticed` still answers
//! `SELECT 1`. Cancellation is routed from the session's [`CancellationToken`] into the executor's
//! per-row poll, so a cancelled statement stops between rows and commits nothing.
//!
//! **Policy.** The row policy of a connection is derived from the authenticated principal in the
//! [`QueryContext`] of its first statement, never from an allow-all default:
//!
//! - an *account* principal gets `accounts(name)`, a function the host supplies (it returns the
//!   policy for that account, which may be as restrictive as `DenyAll`);
//! - the *local-development* principal of loopback trust mode gets the `development` policy the host
//!   passes to [`ExecHandler::new`]; the host must choose it explicitly (for example `AllowAll`
//!   by name), nothing here picks one.
//!
//! The principal is fixed for a connection. The handler remembers the first one and refuses a
//! statement that arrives under a different principal (`28000`), so a policy chosen for one identity
//! cannot be reused for another.
//!
//! Transaction status is the session's own: the executor reports `BEGIN`, `COMMIT` and `ROLLBACK`
//! in its command tags, and the session derives `ReadyForQuery` from them. One known difference:
//! after a failed `COMMIT` (for example `40001`) the executor has ended the block, while the
//! session keeps reporting a failed block until the client sends `ROLLBACK`, which the executor
//! accepts as a no-op.

use std::cell::RefCell;
use std::rc::Rc;

use lattice_exec::{
    ActorPolicy, Database, ExecError, ExecLimits, ExecResult, ExecSession, ResultColumn, TxStatus,
};
use lattice_seams::{Clock, Io, Rng};

use crate::cancel::CancellationToken;
use crate::pgwire::Column;
use crate::principal::{Principal, PrincipalKind, QueryContext};
use crate::session::{QueryError, QueryHandler, QueryOutput};

/// Chooses the policy of an account principal.
type AccountPolicy = Box<dyn Fn(&str) -> ActorPolicy>;

/// Runs statements for one connection against a shared database.
pub struct ExecHandler<I: Io, C: Clock + Clone, R: Rng> {
    db: Rc<RefCell<Database<I, C, R>>>,
    development: ActorPolicy,
    accounts: AccountPolicy,
    /// The connection's principal and its session, created by the first statement.
    bound: Option<(Principal, ExecSession)>,
    /// Statement limits for the session, including the host's timeout ceiling.
    limits: ExecLimits,
}

impl<I: Io, C: Clock + Clone, R: Rng> ExecHandler<I, C, R> {
    /// A handler whose policy follows the connection's principal: `development` for the
    /// local-development principal, `accounts(name)` for an account. There is no default for either.
    pub fn new(
        db: Rc<RefCell<Database<I, C, R>>>,
        development: ActorPolicy,
        accounts: impl Fn(&str) -> ActorPolicy + 'static,
    ) -> Self {
        ExecHandler {
            db,
            development,
            accounts: Box::new(accounts),
            bound: None,
            limits: ExecLimits::default(),
        }
    }

    /// Sets the statement limits of this connection (default timeout, memory budget and the ceiling
    /// a client's `SET statement_timeout` cannot lift). Applies to the running session too.
    pub fn set_limits(&mut self, limits: ExecLimits) {
        self.limits = limits;
        if let Some((_, session)) = self.bound.as_mut() {
            session.set_limits(limits);
        }
    }

    /// The executor's view of the transaction block (`Idle` before the first statement).
    pub fn tx_status(&self) -> TxStatus {
        self.bound
            .as_ref()
            .map_or(TxStatus::Idle, |(_, session)| session.status())
    }

    /// The session of the context's principal, created on first use.
    fn session(&mut self, ctx: &QueryContext<'_>) -> Result<&mut ExecSession, QueryError> {
        let principal = ctx.principal;
        if self.bound.is_none() {
            let policy = match principal.kind() {
                PrincipalKind::LocalDevelopment => self.development.clone(),
                PrincipalKind::Account => (self.accounts)(principal.name()),
            };
            let mut session = ExecSession::new(policy);
            session.set_limits(self.limits);
            self.bound = Some((principal.clone(), session));
        }
        match self.bound.as_mut() {
            Some((bound, session)) if bound == principal => Ok(session),
            _ => Err(QueryError::new(
                "28000",
                "the connection's principal changed after its first statement",
            )),
        }
    }

    fn run(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        let db = Rc::clone(&self.db);
        let session = self.session(ctx)?;
        // A host that lets two connections interleave inside one call would be a bug; fail the
        // statement rather than panic on the borrow.
        let mut db = db
            .try_borrow_mut()
            .map_err(|_| QueryError::new("55P03", "the database is in use by another statement"))?;
        let result = session
            .execute_with(&mut db, sql, params, &|| cancellation.is_cancelled())
            .map_err(error)?;
        Ok(output(&result))
    }
}

fn error(e: ExecError) -> QueryError {
    QueryError::new(e.sqlstate(), e.message())
}

fn column(c: &ResultColumn) -> Column {
    Column {
        name: c.name.clone(),
        table_oid: 0,
        column_attr: 0,
        type_oid: c.type_oid(),
        type_size: c.type_size(),
        type_modifier: -1,
        format: 0,
    }
}

fn output(r: &ExecResult) -> QueryOutput {
    QueryOutput {
        columns: r.columns.iter().map(column).collect(),
        rows: r
            .text_rows()
            .into_iter()
            .map(|row| row.into_iter().map(|v| v.map(String::into_bytes)).collect())
            .collect(),
        tag: r.tag.clone(),
    }
}

impl<I: Io, C: Clock + Clone, R: Rng> QueryHandler for ExecHandler<I, C, R> {
    fn execute(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError> {
        self.run(ctx, sql, params, &CancellationToken::default())
    }

    fn describe(&mut self, ctx: &QueryContext<'_>, sql: &str) -> Result<Vec<Column>, QueryError> {
        let db = Rc::clone(&self.db);
        let session = self.session(ctx)?;
        let db = db
            .try_borrow()
            .map_err(|_| QueryError::new("55P03", "the database is in use by another statement"))?;
        let cols = session.describe(&db, sql).map_err(error)?;
        Ok(cols.iter().map(column).collect())
    }

    fn execute_cancellable(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        self.run(ctx, sql, params, cancellation)
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
    use crate::cancel::CancelRegistry;
    use crate::pgwire::{Backend, Frontend, Startup, TxStatus as WireStatus};
    use crate::session::Session;
    use lattice_core::Config;
    use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
    use lattice_testkit::scenario::{SharedClock, SharedIo};

    type Db = Database<SharedIo, SharedClock, SeededRng>;

    fn new_handler(db: Rc<RefCell<Db>>) -> ExecHandler<SharedIo, SharedClock, SeededRng> {
        // The development policy is chosen by name; accounts are denied everything.
        ExecHandler::new(db, admin(), |_| {
            ActorPolicy::new(Rc::new(lattice_engines::limits::DenyAll))
        })
    }

    fn admin() -> ActorPolicy {
        ActorPolicy::new(Rc::new(lattice_engines::limits::AllowAll))
    }

    fn shared_db() -> Rc<RefCell<Db>> {
        let db = Database::open(
            SharedIo::new(SimIo::new(1, FaultProfile::clean())),
            SharedClock(Rc::new(SimClock::new(1_000))),
            SeededRng::new(2),
            &Config::default(),
        )
        .unwrap();
        Rc::new(RefCell::new(db))
    }

    fn ready_session(
        db: &Rc<RefCell<Db>>,
    ) -> Session<ExecHandler<SharedIo, SharedClock, SeededRng>> {
        let handler = new_handler(db.clone());
        let mut s = Session::local_trust(handler, 1, 2);
        let out = s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        assert!(!out.close);
        s
    }

    fn query(
        s: &mut Session<ExecHandler<SharedIo, SharedClock, SeededRng>>,
        sql: &str,
    ) -> Vec<Backend> {
        s.on_message(Frontend::Query(sql.into())).messages
    }

    fn error_code(messages: &[Backend]) -> Option<String> {
        messages.iter().find_map(|m| match m {
            Backend::ErrorResponse { code, .. } => Some(code.clone()),
            _ => None,
        })
    }

    #[test]
    fn a_simple_query_conversation_runs_through_the_executor() {
        let db = shared_db();
        let mut s = ready_session(&db);
        assert!(
            error_code(&query(
                &mut s,
                "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, ok BOOLEAN)"
            ))
            .is_none()
        );
        let out = query(
            &mut s,
            "INSERT INTO t VALUES (1, 'a', true), (2, NULL, false)",
        );
        assert!(out.contains(&Backend::CommandComplete("INSERT 0 2".into())));
        let out = query(&mut s, "SELECT id, name, ok FROM t ORDER BY id");
        let Backend::RowDescription(cols) = &out[0] else {
            panic!("expected a row description, got {out:?}");
        };
        assert_eq!(
            cols.iter()
                .map(|c| (c.name.as_str(), c.type_oid, c.type_size))
                .collect::<Vec<_>>(),
            [("id", 23, 4), ("name", 25, -1), ("ok", 16, 1)]
        );
        assert_eq!(
            out[1],
            Backend::DataRow(vec![
                Some(b"1".to_vec()),
                Some(b"a".to_vec()),
                Some(b"t".to_vec())
            ])
        );
        assert_eq!(
            out[2],
            Backend::DataRow(vec![Some(b"2".to_vec()), None, Some(b"f".to_vec())])
        );
        assert!(out.contains(&Backend::CommandComplete("SELECT 2".into())));
        assert_eq!(out.last(), Some(&Backend::ReadyForQuery(WireStatus::Idle)));
    }

    #[test]
    fn transaction_status_follows_the_blocks_and_errors() {
        let db = shared_db();
        let mut s = ready_session(&db);
        query(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");
        assert_eq!(
            query(&mut s, "BEGIN").last(),
            Some(&Backend::ReadyForQuery(WireStatus::InTransaction))
        );
        query(&mut s, "INSERT INTO t VALUES (1)");
        let out = query(&mut s, "INSERT INTO t VALUES (1)");
        assert_eq!(error_code(&out).as_deref(), Some("23505"));
        assert_eq!(
            out.last(),
            Some(&Backend::ReadyForQuery(WireStatus::Failed))
        );
        assert_eq!(
            query(&mut s, "SELECT * FROM t").last(),
            Some(&Backend::ReadyForQuery(WireStatus::Failed))
        );
        assert_eq!(
            query(&mut s, "ROLLBACK").last(),
            Some(&Backend::ReadyForQuery(WireStatus::Idle))
        );
        let out = query(&mut s, "SELECT id FROM t");
        assert!(out.contains(&Backend::CommandComplete("SELECT 0".into())));
    }

    #[test]
    fn two_connections_share_one_database_with_snapshot_isolation() {
        let db = shared_db();
        let mut a = ready_session(&db);
        let mut b = ready_session(&db);
        query(&mut a, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
        query(&mut a, "INSERT INTO t VALUES (1, 0)");
        query(&mut a, "BEGIN");
        query(&mut b, "BEGIN");
        query(&mut a, "UPDATE t SET v = 1");
        query(&mut b, "UPDATE t SET v = 2");
        assert!(error_code(&query(&mut a, "COMMIT")).is_none());
        assert_eq!(
            error_code(&query(&mut b, "COMMIT")).as_deref(),
            Some("40001")
        );
    }

    #[test]
    fn the_extended_protocol_binds_text_parameters_with_inferred_types() {
        let db = shared_db();
        let mut s = ready_session(&db);
        query(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
        query(&mut s, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");
        let msgs = |s: &mut Session<_>, m: Frontend| s.on_message(m).messages;
        let parsed = msgs(
            &mut s,
            Frontend::Parse {
                name: "q".into(),
                query: "SELECT name FROM t WHERE id >= $1 ORDER BY id".into(),
                param_types: vec![],
            },
        );
        assert!(parsed.contains(&Backend::ParseComplete), "{parsed:?}");
        msgs(
            &mut s,
            Frontend::Bind {
                portal: String::new(),
                statement: "q".into(),
                param_formats: vec![],
                params: vec![Some(b"2".to_vec())],
                result_formats: vec![],
            },
        );
        let out = msgs(
            &mut s,
            Frontend::Execute {
                portal: String::new(),
                max_rows: 0,
            },
        );
        assert!(
            out.contains(&Backend::DataRow(vec![Some(b"c".to_vec())])),
            "{out:?}"
        );
        assert!(out.contains(&Backend::CommandComplete("SELECT 2".into())));
    }

    #[test]
    fn a_cancelled_token_stops_the_statement_before_it_changes_anything() {
        let db = shared_db();
        let mut handler = new_handler(db.clone());
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        handler
            .execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY, v INT)", &[])
            .unwrap();
        handler
            .execute(&ctx, "INSERT INTO t VALUES (1, 0), (2, 0)", &[])
            .unwrap();
        let mut registry = CancelRegistry::new(4).unwrap();
        let mut rng = SeededRng::new(9);
        let key = registry.register(&mut rng, 0, 1_000).unwrap();
        let token = registry.begin_query(key, 1).unwrap();
        assert!(registry.cancel(key, 2));
        let e = handler
            .execute_cancellable(&ctx, "UPDATE t SET v = 1", &[], &token)
            .unwrap_err();
        assert_eq!(e.code, "57014");
        let out = handler
            .execute(&ctx, "SELECT v FROM t ORDER BY id", &[])
            .unwrap();
        assert_eq!(
            out.rows,
            vec![vec![Some(b"0".to_vec())], vec![Some(b"0".to_vec())]]
        );
        // A statement that is not cancelled still runs.
        assert_eq!(
            handler
                .execute(&ctx, "UPDATE t SET v = 1", &[])
                .unwrap()
                .tag,
            "UPDATE 2"
        );
    }

    #[test]
    fn describe_reports_columns_without_running_the_statement() {
        let db = shared_db();
        let mut handler = new_handler(db);
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        handler
            .execute(
                &ctx,
                "CREATE TABLE t (id BIGINT PRIMARY KEY, n NUMERIC)",
                &[],
            )
            .unwrap();
        let cols = handler
            .describe(&ctx, "INSERT INTO t VALUES (1, 1.5) RETURNING id, n")
            .unwrap();
        assert_eq!(
            cols.iter().map(|c| c.type_oid).collect::<Vec<_>>(),
            [20, 1700]
        );
        assert!(
            handler
                .execute(&ctx, "SELECT id FROM t", &[])
                .unwrap()
                .rows
                .is_empty(),
            "describe wrote nothing"
        );
        assert!(handler.describe(&ctx, "BEGIN").unwrap().is_empty());
        assert_eq!(
            handler
                .describe(&ctx, "SELECT nope FROM t")
                .unwrap_err()
                .code,
            "42703"
        );
    }

    #[test]
    fn the_policy_follows_the_principal_of_the_connection() {
        let db = shared_db();
        let dev = Principal::local_development();
        let mut developer = new_handler(db.clone());
        let ctx = QueryContext::new(&dev);
        developer
            .execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        developer
            .execute(&ctx, "INSERT INTO t VALUES (1)", &[])
            .unwrap();
        // An account gets the host's account policy, here deny-all: it sees nothing and cannot
        // write, while the development principal sees the row.
        let alice = Principal::account("alice");
        let actx = QueryContext::new(&alice);
        let mut account = new_handler(db.clone());
        assert!(
            account
                .execute(&actx, "SELECT id FROM t", &[])
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(
            account
                .execute(&actx, "INSERT INTO t VALUES (2)", &[])
                .unwrap_err()
                .code,
            "42501"
        );
        assert_eq!(
            developer
                .execute(&ctx, "SELECT id FROM t", &[])
                .unwrap()
                .rows
                .len(),
            1
        );
    }

    #[test]
    fn an_account_policy_is_chosen_by_the_account_name() {
        let db = shared_db();
        let dev = Principal::local_development();
        let mut setup = new_handler(db.clone());
        let ctx = QueryContext::new(&dev);
        setup
            .execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        setup
            .execute(&ctx, "INSERT INTO t VALUES (1)", &[])
            .unwrap();
        let by_name = |db: Rc<RefCell<Db>>| {
            ExecHandler::new(db, admin(), |name| {
                if name == "reader" {
                    admin()
                } else {
                    ActorPolicy::new(Rc::new(lattice_engines::limits::DenyAll))
                }
            })
        };
        let reader = Principal::account("reader");
        let other = Principal::account("other");
        let rows = |h: &mut ExecHandler<_, _, _>, p: &Principal| {
            h.execute(&QueryContext::new(p), "SELECT id FROM t", &[])
                .unwrap()
                .rows
                .len()
        };
        assert_eq!(rows(&mut by_name(db.clone()), &reader), 1);
        assert_eq!(rows(&mut by_name(db), &other), 0);
    }

    #[test]
    fn a_connection_cannot_change_principal() {
        let db = shared_db();
        let mut h = new_handler(db);
        let dev = Principal::local_development();
        h.execute(&QueryContext::new(&dev), "CREATE TABLE t (id INT)", &[])
            .unwrap();
        let alice = Principal::account("alice");
        let e = h
            .execute(&QueryContext::new(&alice), "SELECT id FROM t", &[])
            .unwrap_err();
        assert_eq!(e.code, "28000");
        assert_eq!(h.tx_status(), TxStatus::Idle);
    }

    #[test]
    fn ddl_follows_the_principals_policy() {
        let db = shared_db();
        let dev = Principal::local_development();
        let mut developer = new_handler(db.clone());
        // The development policy allows DDL; the account policy (deny-all here) does not.
        developer
            .execute(&QueryContext::new(&dev), "CREATE TABLE t (id INT)", &[])
            .unwrap();
        let alice = Principal::account("alice");
        let mut account = new_handler(db);
        let actx = QueryContext::new(&alice);
        for sql in ["CREATE TABLE u (id INT)", "DROP TABLE t"] {
            assert_eq!(
                account.execute(&actx, sql, &[]).unwrap_err().code,
                "42501",
                "{sql}"
            );
        }
        assert_eq!(
            developer
                .execute(&QueryContext::new(&dev), "DROP TABLE t", &[])
                .unwrap()
                .tag,
            "DROP TABLE"
        );
    }

    #[test]
    fn the_host_timeout_ceiling_reaches_the_session() {
        let db = shared_db();
        let dev = Principal::local_development();
        let ctx = QueryContext::new(&dev);
        let mut h = new_handler(db);
        h.set_limits(ExecLimits {
            timeout_ms: Some(50),
            memory_bytes: 1 << 20,
            max_timeout_ms: Some(50),
        });
        h.execute(&ctx, "SET statement_timeout = 0", &[]).unwrap();
        let shown = h.execute(&ctx, "SHOW statement_timeout", &[]).unwrap();
        assert_eq!(shown.rows, vec![vec![Some(b"50ms".to_vec())]]);
    }
}
