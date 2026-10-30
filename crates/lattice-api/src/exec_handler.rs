//! A [`QueryHandler`] that runs SQL through `lattice-exec`.
//!
//! The handler owns one [`ExecSession`] (the transaction block, the actor's row policy and the
//! statement limits of one connection) and shares the [`Database`] with the other connections of a
//! single-threaded host. It is **not wired into any listener**: `latticed` still answers
//! `SELECT 1`. Cancellation is routed from the session's [`CancellationToken`] into the executor's
//! per-row poll, so a cancelled statement stops between rows and commits nothing.
//!
//! **Policy.** The handler is built from an [`ActorPolicy`], never from an allow-all default. The
//! connection's actor is not known to this crate yet: authentication and the principal live in the
//! session layer, and the query context that carries them (a principal per connection) is being
//! designed separately. When it lands, the host derives the policy from the context's principal at
//! connection setup and passes it here; this constructor is the one place that decision is made, so
//! no executor call can happen with a policy nobody chose. Until then a host builds the policy
//! explicitly, and tests use `AllowAll` by name.
//!
//! Transaction status is the session's own: the executor reports `BEGIN`, `COMMIT` and `ROLLBACK`
//! in its command tags, and the session derives `ReadyForQuery` from them. One known difference:
//! after a failed `COMMIT` (for example `40001`) the executor has ended the block, while the
//! session keeps reporting a failed block until the client sends `ROLLBACK`, which the executor
//! accepts as a no-op.

use std::cell::RefCell;
use std::rc::Rc;

use lattice_exec::{
    ActorPolicy, Database, ExecError, ExecResult, ExecSession, ResultColumn, TxStatus,
};
use lattice_seams::{Clock, Io, Rng};

use crate::cancel::CancellationToken;
use crate::pgwire::Column;
use crate::session::{QueryError, QueryHandler, QueryOutput};

/// Runs statements for one connection against a shared database.
pub struct ExecHandler<I: Io, C: Clock + Clone, R: Rng> {
    db: Rc<RefCell<Database<I, C, R>>>,
    session: ExecSession,
}

impl<I: Io, C: Clock + Clone, R: Rng> ExecHandler<I, C, R> {
    /// A handler for a connection acting under `policy`. The policy is required: there is no
    /// default and no allow-all constructor, so a host cannot reach the executor without deciding
    /// which rows the connection may touch.
    pub fn new(db: Rc<RefCell<Database<I, C, R>>>, policy: ActorPolicy) -> Self {
        ExecHandler {
            db,
            session: ExecSession::new(policy),
        }
    }

    /// The executor's view of the transaction block.
    pub fn tx_status(&self) -> TxStatus {
        self.session.status()
    }

    fn run(
        &mut self,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        // A host that lets two connections interleave inside one call would be a bug; fail the
        // statement rather than panic on the borrow.
        let mut db = self
            .db
            .try_borrow_mut()
            .map_err(|_| QueryError::new("55P03", "the database is in use by another statement"))?;
        let result = self
            .session
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
        sql: &str,
        params: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError> {
        self.run(sql, params, &CancellationToken::default())
    }

    fn describe(&mut self, sql: &str) -> Result<Vec<Column>, QueryError> {
        let db = self
            .db
            .try_borrow()
            .map_err(|_| QueryError::new("55P03", "the database is in use by another statement"))?;
        let cols = self.session.describe(&db, sql).map_err(error)?;
        Ok(cols.iter().map(column).collect())
    }

    fn execute_cancellable(
        &mut self,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        self.run(sql, params, cancellation)
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
        let handler = ExecHandler::new(db.clone(), admin());
        let mut s = Session::new(handler, 1, 2);
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
        let mut handler = ExecHandler::new(db.clone(), admin());
        handler
            .execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)", &[])
            .unwrap();
        handler
            .execute("INSERT INTO t VALUES (1, 0), (2, 0)", &[])
            .unwrap();
        let mut registry = CancelRegistry::new(4).unwrap();
        let mut rng = SeededRng::new(9);
        let key = registry.register(&mut rng, 0, 1_000).unwrap();
        let token = registry.begin_query(key, 1).unwrap();
        assert!(registry.cancel(key, 2));
        let e = handler
            .execute_cancellable("UPDATE t SET v = 1", &[], &token)
            .unwrap_err();
        assert_eq!(e.code, "57014");
        let out = handler.execute("SELECT v FROM t ORDER BY id", &[]).unwrap();
        assert_eq!(
            out.rows,
            vec![vec![Some(b"0".to_vec())], vec![Some(b"0".to_vec())]]
        );
        // A statement that is not cancelled still runs.
        assert_eq!(
            handler.execute("UPDATE t SET v = 1", &[]).unwrap().tag,
            "UPDATE 2"
        );
    }

    #[test]
    fn describe_reports_columns_without_running_the_statement() {
        let db = shared_db();
        let mut handler = ExecHandler::new(db, admin());
        handler
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, n NUMERIC)", &[])
            .unwrap();
        let cols = handler
            .describe("INSERT INTO t VALUES (1, 1.5) RETURNING id, n")
            .unwrap();
        assert_eq!(
            cols.iter().map(|c| c.type_oid).collect::<Vec<_>>(),
            [20, 1700]
        );
        assert!(
            handler
                .execute("SELECT id FROM t", &[])
                .unwrap()
                .rows
                .is_empty(),
            "describe wrote nothing"
        );
        assert!(handler.describe("BEGIN").unwrap().is_empty());
        assert_eq!(
            handler.describe("SELECT nope FROM t").unwrap_err().code,
            "42703"
        );
    }
}
