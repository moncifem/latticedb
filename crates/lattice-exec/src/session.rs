//! A client session: statement routing, transactions and statement limits.
//!
//! Outside `BEGIN ... COMMIT` every statement is its own transaction: it reads one snapshot and, if it
//! wrote anything, commits as one change set. Inside a block the snapshot is taken at `BEGIN` and all
//! statements share it (snapshot isolation); `COMMIT` commits the staged writes as one change set, and
//! a first-committer-wins conflict surfaces then as `40001`. An error inside a block leaves it failed
//! (`25P02`) until `ROLLBACK`.

use lattice_engines::limits::{CancellationFlag, StatementLimits};
use lattice_seams::{Clock, Io, Rng};
use lattice_sql::ast::{SetValue, Statement};
use lattice_sql::binder::bind_statement;
use lattice_sql::parse_statement;
use lattice_sql::plan::{BoundStatement, SqlType};

use crate::db::Database;
use crate::error::{ErrorKind, ExecError};
use crate::exec::{ActorPolicy, Exec, Gov, RunOutput, TxState};
use crate::schema::object_name;
use crate::value::{Datum, parse_text};

/// The transaction state a client sees (PostgreSQL's `ReadyForQuery` status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    /// Not in a transaction block.
    Idle,
    /// In a block.
    InTransaction,
    /// In a block that failed; only `ROLLBACK` (or `COMMIT`, which rolls back) is accepted.
    Failed,
}

/// A result column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultColumn {
    /// The name the client sees.
    pub name: String,
    /// The type of the values.
    pub ty: SqlType,
}

impl ResultColumn {
    /// The PostgreSQL type object id a client expects for this column.
    pub fn type_oid(&self) -> i32 {
        match self.ty {
            SqlType::Boolean => 16,
            SqlType::SmallInt => 21,
            SqlType::Integer => 23,
            SqlType::BigInt => 20,
            SqlType::Numeric => 1700,
            SqlType::DoublePrecision => 701,
            SqlType::Bytea => 17,
            SqlType::Timestamptz => 1184,
            SqlType::Date => 1082,
            SqlType::Uuid => 2950,
            SqlType::Jsonb => 3802,
            SqlType::Text | SqlType::Unknown => 25,
        }
    }

    /// The type size PostgreSQL reports: the width in bytes, or -1 for a variable-length type.
    pub fn type_size(&self) -> i16 {
        match self.ty {
            SqlType::Boolean => 1,
            SqlType::SmallInt => 2,
            SqlType::Integer | SqlType::Date => 4,
            SqlType::BigInt | SqlType::DoublePrecision | SqlType::Timestamptz => 8,
            SqlType::Uuid => 16,
            SqlType::Numeric
            | SqlType::Bytea
            | SqlType::Jsonb
            | SqlType::Text
            | SqlType::Unknown => -1,
        }
    }
}

/// The result of one statement.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecResult {
    /// Result columns; empty for a statement that returns no rows.
    pub columns: Vec<ResultColumn>,
    /// Result rows, one datum per column.
    pub rows: Vec<Vec<Datum>>,
    /// The PostgreSQL command tag: `SELECT n`, `INSERT 0 n`, `UPDATE n`, `DELETE n`, `BEGIN`, ...
    pub tag: String,
    /// Rows selected, inserted, updated or deleted.
    pub rows_affected: u64,
}

impl ExecResult {
    fn command(tag: &str) -> Self {
        ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            tag: tag.to_string(),
            rows_affected: 0,
        }
    }

    /// The rows in PostgreSQL's text format; `None` is `NULL`.
    pub fn text_rows(&self) -> Vec<Vec<Option<String>>> {
        self.rows
            .iter()
            .map(|r| r.iter().map(Datum::to_pg_text).collect())
            .collect()
    }
}

impl From<RunOutput> for ExecResult {
    fn from(o: RunOutput) -> Self {
        ExecResult {
            columns: o
                .columns
                .into_iter()
                .map(|(name, ty)| ResultColumn { name, ty })
                .collect(),
            rows: o.rows,
            tag: o.tag,
            rows_affected: o.affected,
        }
    }
}

/// Per-statement resource limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecLimits {
    /// Statement timeout in milliseconds of the database clock; `None` for no timeout.
    pub timeout_ms: Option<u64>,
    /// Bytes a statement may hold in result and working rows.
    pub memory_bytes: u64,
    /// The host's ceiling for the timeout. A client's `SET statement_timeout` is clamped to it, and
    /// `0`, `DEFAULT` or a larger value fall back to it. `None` leaves the client free.
    pub max_timeout_ms: Option<u64>,
}

impl Default for ExecLimits {
    fn default() -> Self {
        ExecLimits {
            timeout_ms: None,
            memory_bytes: 64 << 20,
            max_timeout_ms: None,
        }
    }
}

/// One client's session over a [`Database`].
pub struct ExecSession {
    policy: ActorPolicy,
    limits: ExecLimits,
    txn: Option<TxState>,
    failed: bool,
}

/// Settings `SET` accepts and ignores, because drivers send them at startup.
const IGNORED_SETTINGS: &[&str] = &[
    "client_encoding",
    "application_name",
    "extra_float_digits",
    "datestyle",
    "timezone",
];

fn no_cancel() -> bool {
    false
}

impl ExecSession {
    /// A session whose transactions are bound to the actor's `policy`.
    pub fn new(policy: ActorPolicy) -> Self {
        ExecSession {
            policy,
            limits: ExecLimits::default(),
            txn: None,
            failed: false,
        }
    }

    /// The transaction status after the last statement.
    pub fn status(&self) -> TxStatus {
        match (&self.txn, self.failed) {
            (None, _) => TxStatus::Idle,
            (Some(_), false) => TxStatus::InTransaction,
            (Some(_), true) => TxStatus::Failed,
        }
    }

    /// The limits applied to each statement.
    pub fn limits(&self) -> ExecLimits {
        self.limits
    }

    /// Replaces the limits for later statements.
    pub fn set_limits(&mut self, limits: ExecLimits) {
        // The effective timeout never exceeds the ceiling, whatever the caller left in `timeout_ms`.
        let mut limits = limits;
        limits.timeout_ms = match (limits.timeout_ms, limits.max_timeout_ms) {
            (None, cap) => cap,
            (Some(r), Some(cap)) => Some(r.min(cap)),
            (Some(r), None) => Some(r),
        };
        self.limits = limits;
    }

    /// Runs one statement.
    pub fn execute<I: Io, C: Clock + Clone, R: Rng>(
        &mut self,
        db: &mut Database<I, C, R>,
        sql: &str,
    ) -> Result<ExecResult, ExecError> {
        self.execute_with(db, sql, &[], &no_cancel)
    }

    /// Runs one statement with text-format parameters (`None` is `NULL`) and a cancellation poll,
    /// which is called between bounded units of work and aborts the statement when it returns `true`.
    pub fn execute_with<I: Io, C: Clock + Clone, R: Rng>(
        &mut self,
        db: &mut Database<I, C, R>,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancel: &dyn Fn() -> bool,
    ) -> Result<ExecResult, ExecError> {
        let r = self.execute_inner(db, sql, params, cancel);
        if r.is_err() && self.txn.is_some() {
            self.failed = true;
        }
        r
    }

    /// Runs a statement that was bound earlier, after checking its catalog version.
    pub fn execute_bound<I: Io, C: Clock + Clone, R: Rng>(
        &mut self,
        db: &mut Database<I, C, R>,
        bound: &BoundStatement,
        params: &[Datum],
        cancel: &dyn Fn() -> bool,
    ) -> Result<ExecResult, ExecError> {
        let r = if self.failed {
            Err(aborted())
        } else {
            self.run_bound(db, bound, params, cancel)
        };
        if r.is_err() && self.txn.is_some() {
            self.failed = true;
        }
        r
    }

    /// The columns a statement would return, without running it.
    pub fn describe<I: Io, C: Clock + Clone, R: Rng>(
        &self,
        db: &Database<I, C, R>,
        sql: &str,
    ) -> Result<Vec<ResultColumn>, ExecError> {
        let stmt = parse_statement(sql)?;
        match &stmt {
            Statement::Query(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_) => {
                let bound = bind_statement(&stmt, &db.catalog_view())?;
                Ok(bound
                    .plan
                    .output()
                    .map(|cols| {
                        cols.iter()
                            .map(|c| ResultColumn {
                                name: c.name.clone(),
                                ty: c.expr.ty,
                            })
                            .collect()
                    })
                    .unwrap_or_default())
            }
            Statement::Show { name } => Ok(vec![ResultColumn {
                name: name.value.to_lowercase(),
                ty: SqlType::Text,
            }]),
            _ => Ok(Vec::new()),
        }
    }

    fn execute_inner<I: Io, C: Clock + Clone, R: Rng>(
        &mut self,
        db: &mut Database<I, C, R>,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancel: &dyn Fn() -> bool,
    ) -> Result<ExecResult, ExecError> {
        let stmt = parse_statement(sql)?;
        if self.failed {
            return match stmt {
                Statement::Rollback | Statement::Commit => {
                    self.txn = None;
                    self.failed = false;
                    Ok(ExecResult::command("ROLLBACK"))
                }
                _ => Err(aborted()),
            };
        }
        match stmt {
            Statement::Begin { .. } => {
                if self.txn.is_none() {
                    self.txn = Some(TxState::begin(db, &self.policy));
                }
                Ok(ExecResult::command("BEGIN"))
            }
            Statement::Commit => {
                if let Some(state) = self.txn.take()
                    && state.has_writes()
                {
                    db.store.commit(state.tx)?;
                }
                Ok(ExecResult::command("COMMIT"))
            }
            Statement::Rollback => {
                self.txn = None;
                Ok(ExecResult::command("ROLLBACK"))
            }
            Statement::SetTransaction { .. } => {
                if self.txn.is_none() {
                    return Err(ExecError::new(
                        ErrorKind::Bind,
                        "25P01",
                        "SET TRANSACTION can only be used in transaction blocks",
                    ));
                }
                // REPEATABLE READ and SERIALIZABLE are both run as snapshot isolation.
                Ok(ExecResult::command("SET"))
            }
            Statement::Set { name, value } => self.set(&name.value.to_lowercase(), value),
            Statement::Show { name } => {
                let key = name.value.to_lowercase();
                let value = match key.as_str() {
                    "transaction_isolation" => "repeatable read".to_string(),
                    "statement_timeout" => match self.limits.timeout_ms {
                        Some(ms) => format!("{ms}ms"),
                        None => "0".to_string(),
                    },
                    _ => {
                        return Err(ExecError::new(
                            ErrorKind::Bind,
                            "42704",
                            format!("unrecognized configuration parameter \"{key}\""),
                        ));
                    }
                };
                Ok(ExecResult {
                    columns: vec![ResultColumn {
                        name: key,
                        ty: SqlType::Text,
                    }],
                    rows: vec![vec![Datum::Text(value)]],
                    tag: "SHOW".to_string(),
                    rows_affected: 1,
                })
            }
            Statement::CreateTable(ct) => {
                self.no_ddl_in_block()?;
                db.create_table(&ct, &self.policy)?;
                Ok(ExecResult::command("CREATE TABLE"))
            }
            Statement::DropTable { name, if_exists } => {
                self.no_ddl_in_block()?;
                db.drop_table(&object_name(&name)?, if_exists, &self.policy)?;
                Ok(ExecResult::command("DROP TABLE"))
            }
            Statement::AlterTable(_) | Statement::CreateIndex(_) | Statement::DropIndex { .. } => {
                Err(ExecError::unsupported(
                    "ALTER TABLE, CREATE INDEX and DROP INDEX are not executed yet",
                ))
            }
            stmt @ (Statement::Query(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)) => {
                let bound = bind_statement(&stmt, &db.catalog_view())?;
                let values = decode_params(&bound.param_types, params)?;
                self.run_bound(db, &bound, &values, cancel)
            }
        }
    }

    fn no_ddl_in_block(&self) -> Result<(), ExecError> {
        if self.txn.is_some() {
            return Err(ExecError::new(
                ErrorKind::Unsupported,
                "25001",
                "CREATE TABLE and DROP TABLE cannot run inside a transaction block",
            ));
        }
        Ok(())
    }

    fn set(&mut self, name: &str, value: Option<SetValue>) -> Result<ExecResult, ExecError> {
        if name == "statement_timeout" {
            let ms = match value {
                None => None,
                Some(SetValue::Number(n) | SetValue::String(n) | SetValue::Word(n)) => {
                    parse_timeout_ms(&n)?
                }
            };
            // The client may tighten the timeout but never lift the host's ceiling.
            self.limits.timeout_ms = match (ms, self.limits.max_timeout_ms) {
                (None, cap) => cap,
                (Some(r), Some(cap)) => Some(r.min(cap)),
                (Some(r), None) => Some(r),
            };
            return Ok(ExecResult::command("SET"));
        }
        if IGNORED_SETTINGS.contains(&name) {
            return Ok(ExecResult::command("SET"));
        }
        Err(ExecError::new(
            ErrorKind::Bind,
            "42704",
            format!("unrecognized configuration parameter \"{name}\""),
        ))
    }

    fn run_bound<I: Io, C: Clock + Clone, R: Rng>(
        &mut self,
        db: &mut Database<I, C, R>,
        bound: &BoundStatement,
        params: &[Datum],
        cancel: &dyn Fn() -> bool,
    ) -> Result<ExecResult, ExecError> {
        if bound.catalog_version != db.catalog_version() {
            return Err(ExecError::new(
                ErrorKind::StalePlan,
                "0A000",
                format!(
                    "stale plan: bound at catalog version {} but the catalog is at {}; bind the statement again",
                    bound.catalog_version,
                    db.catalog_version()
                ),
            ));
        }
        if params.len() != bound.param_types.len() {
            return Err(ExecError::new(
                ErrorKind::Bind,
                "08P01",
                format!(
                    "the statement takes {} parameters but {} were supplied",
                    bound.param_types.len(),
                    params.len()
                ),
            ));
        }
        let limits = StatementLimits::new(
            &db.clock,
            self.limits.timeout_ms,
            self.limits.memory_bytes,
            CancellationFlag::default(),
        )?;
        let gov = Gov::new(&limits, &db.clock, cancel);
        let now_micros = i64::try_from(db.clock.now_ms())
            .unwrap_or(i64::MAX / 1000)
            .saturating_mul(1000);
        let policy = self.policy.clone();
        if let Some(state) = self.txn.as_mut() {
            let mut ex = Exec {
                db: &*db,
                txs: state,
                policy: &policy,
                gov: &gov,
                params,
                now_micros,
            };
            return ex.run(&bound.plan).map(ExecResult::from);
        }
        let mut state = TxState::begin(&*db, &policy);
        let out = {
            let mut ex = Exec {
                db: &*db,
                txs: &mut state,
                policy: &policy,
                gov: &gov,
                params,
                now_micros,
            };
            ex.run(&bound.plan)?
        };
        if state.has_writes() {
            // A last look at cancellation: after this point the change set is committed.
            gov.check()?;
            db.store.commit(state.tx)?;
        }
        Ok(ExecResult::from(out))
    }
}

fn aborted() -> ExecError {
    ExecError::new(
        ErrorKind::TransactionAborted,
        "25P02",
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

fn parse_timeout_ms(text: &str) -> Result<Option<u64>, ExecError> {
    let t = text.trim().to_ascii_lowercase();
    let (digits, unit) = match t.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => t.split_at(at),
        None => (t.as_str(), "ms"),
    };
    let n: u64 = digits.parse().map_err(|_| {
        ExecError::new(
            ErrorKind::Bind,
            "22023",
            format!("invalid value for parameter \"statement_timeout\": \"{text}\""),
        )
    })?;
    let factor = match unit.trim() {
        "ms" | "" => 1,
        "s" => 1000,
        "min" => 60_000,
        _ => {
            return Err(ExecError::new(
                ErrorKind::Bind,
                "22023",
                format!("invalid value for parameter \"statement_timeout\": \"{text}\""),
            ));
        }
    };
    let ms = n.saturating_mul(factor);
    Ok(if ms == 0 { None } else { Some(ms) })
}

/// Converts text-format parameters to values of the types the binder inferred.
pub fn decode_params(
    types: &[SqlType],
    params: &[Option<Vec<u8>>],
) -> Result<Vec<Datum>, ExecError> {
    if types.len() != params.len() {
        return Err(ExecError::new(
            ErrorKind::Bind,
            "08P01",
            format!(
                "bind message supplies {} parameters, but the statement requires {}",
                params.len(),
                types.len()
            ),
        ));
    }
    types
        .iter()
        .zip(params)
        .map(|(ty, p)| match p {
            None => Ok(Datum::Null),
            Some(bytes) => {
                let s = std::str::from_utf8(bytes).map_err(|_| {
                    ExecError::eval("22021", "invalid byte sequence for encoding \"UTF8\"")
                })?;
                parse_text(s, *ty)
            }
        })
        .collect()
}
