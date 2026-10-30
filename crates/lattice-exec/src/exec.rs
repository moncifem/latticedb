//! Plan operators: scan, filter, project, sort, limit and the three writes.
//!
//! Every operator materialises its output as a vector of rows. Work is bounded by the statement
//! limits, with these exact guarantees:
//!
//! - `RowStore::scan_at` materialises the whole table's borrowed row references **before** this module
//!   can poll or charge anything, so a scan is unpolled and uncharged for that step. After it,
//!   cancellation and the deadline are polled for every row examined, and memory is charged for every
//!   row kept, after the row is decoded.
//! - A sort polls once per key evaluation (one per input row), charges the evaluated keys, and then
//!   sorts without polling: the comparison pass over already-charged keys is not interruptible.
//! - `RETURNING` evaluates over the row just written or deleted and does **not** consult the read
//!   policy again; the actor could already read the old row and has just been allowed to write the new
//!   one (the engine checks every transition at commit). A policy whose write rule is wider than its
//!   read rule would therefore let `RETURNING` show a row the actor cannot `SELECT`.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::rc::Rc;

use lattice_abi::Hlc;
use lattice_core::changeset::ChangeSetId;
use lattice_engines::catalog::DdlRef;
use lattice_engines::limits::{
    AllowAll, MemoryReservation, PolicyEvaluator, RowRef, RowWriteRef, StatementLimits,
};
use lattice_engines::mvcc::Snapshot;
use lattice_engines::rowops::RowKey;
use lattice_engines::store::{RowStore, RowTx};
use lattice_seams::{Clock, Io, Rng};
use lattice_sql::plan::{
    AsOfPlan, InsertSource, LogicalPlan, OutputColumn, SortKey, SqlType, TypedExpr,
};

use crate::db::Database;
use crate::error::{ErrorKind, ExecError};
use crate::eval::{EvalCtx, cast, eval, truthy};
use crate::schema::{ColumnDef, TableDef};
use crate::value::{Datum, total_order};

/// The policy a transaction is bound to: the actor's row policy, shared by the session.
#[derive(Clone)]
pub struct ActorPolicy(pub(crate) Rc<dyn PolicyEvaluator>, pub(crate) bool);

impl ActorPolicy {
    /// Wraps an evaluator supplied by a trusted host.
    pub fn new(policy: Rc<dyn PolicyEvaluator>) -> Self {
        ActorPolicy(policy, true)
    }

    /// Hides the catalog from this actor: binding and describing see no tables, so every table
    /// reference is `42P01` exactly as for a table that does not exist. Use it for an actor whose
    /// policy grants nothing, so a denied actor cannot learn table or column names from error codes
    /// or from `Parse`/`Describe`.
    #[must_use]
    pub fn hiding_catalog(mut self) -> Self {
        self.1 = false;
        self
    }

    pub(crate) fn sees_catalog(&self) -> bool {
        self.1
    }
}

impl PolicyEvaluator for ActorPolicy {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        self.0.allows_read(row)
    }

    fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
        self.0.allows_write(row)
    }

    fn authorize_ddl(&self, change: DdlRef<'_>) -> bool {
        self.0.authorize_ddl(change)
    }
}

/// An open transaction: the engine transaction (snapshot and staged writes, bound to the actor's
/// policy) and a copy of the staged writes that scans can merge in.
pub(crate) struct TxState {
    pub(crate) tx: RowTx<ActorPolicy>,
    overlay: BTreeMap<RowKey, Option<Vec<u8>>>,
}

impl TxState {
    pub(crate) fn begin<I: Io, C: Clock + Clone, R: Rng>(
        db: &Database<I, C, R>,
        policy: &ActorPolicy,
    ) -> Self {
        TxState {
            tx: db.store.begin_with_policy(policy.clone()),
            overlay: BTreeMap::new(),
        }
    }

    fn put(&mut self, table: u32, key: Vec<u8>, value: Vec<u8>) {
        self.tx.put((table, key.clone()), value.clone());
        self.overlay.insert((table, key), Some(value));
    }

    fn delete(&mut self, table: u32, key: Vec<u8>) {
        self.tx.delete((table, key.clone()));
        self.overlay.insert((table, key), None);
    }

    pub(crate) fn has_writes(&self) -> bool {
        !self.overlay.is_empty()
    }
}

/// Statement limits plus the clock they are polled against.
pub(crate) struct Gov<'a, C: Clock> {
    limits: &'a StatementLimits,
    clock: &'a C,
    cancel: &'a dyn Fn() -> bool,
    held: RefCell<Vec<MemoryReservation<'a>>>,
    slack: Cell<u64>,
}

/// Memory is reserved in blocks this large, so a scan does not hold one reservation per row.
const RESERVE_BLOCK: u64 = 16 * 1024;

impl<'a, C: Clock> Gov<'a, C> {
    pub(crate) fn new(
        limits: &'a StatementLimits,
        clock: &'a C,
        cancel: &'a dyn Fn() -> bool,
    ) -> Self {
        Gov {
            limits,
            clock,
            cancel,
            held: RefCell::new(Vec::new()),
            slack: Cell::new(0),
        }
    }

    /// Polls cancellation and the deadline.
    pub(crate) fn check(&self) -> Result<(), ExecError> {
        if (self.cancel)() {
            return Err(lattice_engines::limits::LimitError::Cancelled.into());
        }
        self.limits.check(self.clock).map_err(Into::into)
    }

    /// Reserves `bytes` for the rest of the statement, failing before the memory is used.
    pub(crate) fn charge(&self, bytes: u64) -> Result<(), ExecError> {
        self.check()?;
        if self.slack.get() >= bytes {
            self.slack.set(self.slack.get() - bytes);
            return Ok(());
        }
        let block = bytes.max(RESERVE_BLOCK);
        let reservation = match self.limits.reserve(self.clock, block) {
            Ok(r) => (r, block),
            Err(_) => (self.limits.reserve(self.clock, bytes)?, bytes),
        };
        self.slack.set(self.slack.get() + reservation.1 - bytes);
        self.held.borrow_mut().push(reservation.0);
        Ok(())
    }
}

/// One row on its way through the plan. `key` is the storage key while the row comes straight from
/// a scan (and so can be updated or deleted).
pub(crate) struct Tuple {
    key: Option<Vec<u8>>,
    vals: Vec<Datum>,
}

fn row_cost(vals: &[Datum]) -> u64 {
    64 + vals.iter().map(Datum::size).sum::<u64>()
}

/// What running a statement produced.
pub(crate) struct RunOutput {
    pub(crate) columns: Vec<(String, SqlType)>,
    pub(crate) rows: Vec<Vec<Datum>>,
    pub(crate) tag: String,
    pub(crate) affected: u64,
}

pub(crate) struct Exec<'a, 'g, I: Io, C: Clock + Clone, R: Rng> {
    pub(crate) db: &'a Database<I, C, R>,
    pub(crate) txs: &'a mut TxState,
    pub(crate) policy: &'a ActorPolicy,
    pub(crate) gov: &'a Gov<'g, C>,
    pub(crate) params: &'a [Datum],
    pub(crate) now_micros: i64,
}

fn output_columns(cols: &[OutputColumn]) -> Vec<(String, SqlType)> {
    cols.iter().map(|c| (c.name.clone(), c.expr.ty)).collect()
}

impl<I: Io, C: Clock + Clone, R: Rng> Exec<'_, '_, I, C, R> {
    fn ctx(&self) -> EvalCtx<'_> {
        EvalCtx {
            params: self.params,
            now_micros: self.now_micros,
        }
    }

    fn store(&self) -> &RowStore<I, C, R> {
        &self.db.store
    }

    fn table(&self, id: u64) -> Result<&TableDef, ExecError> {
        u32::try_from(id)
            .ok()
            .and_then(|id| self.db.schema.by_id(id))
            .ok_or_else(|| {
                ExecError::new(
                    ErrorKind::StalePlan,
                    "0A000",
                    "stale plan: a table it names no longer exists; bind the statement again",
                )
            })
    }

    /// Runs a bound statement's plan.
    pub(crate) fn run(&mut self, plan: &LogicalPlan) -> Result<RunOutput, ExecError> {
        match plan {
            LogicalPlan::Insert { .. } => self.insert(plan),
            LogicalPlan::Update { .. } => self.update(plan),
            LogicalPlan::Delete { .. } => self.delete(plan),
            _ => {
                let columns = plan.output().map(output_columns).unwrap_or_default();
                let rows = self.rel(plan)?;
                let n = rows.len() as u64;
                Ok(RunOutput {
                    columns,
                    rows: rows.into_iter().map(|t| t.vals).collect(),
                    tag: format!("SELECT {n}"),
                    affected: n,
                })
            }
        }
    }

    // ----- relational operators -----

    fn rel(&mut self, plan: &LogicalPlan) -> Result<Vec<Tuple>, ExecError> {
        match plan {
            LogicalPlan::Scan {
                table_id, as_of, ..
            } => self.scan(*table_id, as_of.as_ref(), None),
            LogicalPlan::Filter { input, predicate } => {
                // A filter straight over a scan runs inside it, so rows it rejects are never kept
                // and do not count against the memory budget.
                if let LogicalPlan::Scan {
                    table_id, as_of, ..
                } = &**input
                {
                    return self.scan(*table_id, as_of.as_ref(), Some(predicate));
                }
                let rows = self.rel(input)?;
                let mut out = Vec::new();
                for t in rows {
                    self.gov.check()?;
                    if truthy(&eval(predicate, &t.vals, &self.ctx())?) {
                        out.push(t);
                    }
                }
                Ok(out)
            }
            LogicalPlan::Project { input, columns } => {
                let rows = self.rel(input)?;
                self.project(rows, columns)
            }
            LogicalPlan::Sort { input, keys } => {
                let rows = self.rel(input)?;
                self.sort(rows, keys)
            }
            LogicalPlan::Limit {
                input,
                limit,
                offset,
            } => {
                let (limit, offset) = self.bounds(limit.as_ref(), offset.as_ref())?;
                // Apply the bounds below a projection so rows past the limit are never evaluated.
                if let LogicalPlan::Project {
                    input: inner,
                    columns,
                } = &**input
                {
                    let rows = self.rel(inner)?;
                    let rows = window(rows, limit, offset);
                    return self.project(rows, columns);
                }
                let rows = self.rel(input)?;
                Ok(window(rows, limit, offset))
            }
            LogicalPlan::Insert { .. }
            | LogicalPlan::Update { .. }
            | LogicalPlan::Delete { .. } => {
                Err(ExecError::internal("a write operator inside a query"))
            }
        }
    }

    fn bounds(
        &self,
        limit: Option<&TypedExpr>,
        offset: Option<&TypedExpr>,
    ) -> Result<(Option<usize>, usize), ExecError> {
        let count = |e: &TypedExpr, negative: &'static str| -> Result<Option<usize>, ExecError> {
            match eval(e, &[], &self.ctx())? {
                Datum::Null => Ok(None),
                Datum::Int(i) if i < 0 => Err(ExecError::eval(
                    negative,
                    "LIMIT/OFFSET must not be negative",
                )),
                Datum::Int(i) => Ok(Some(usize::try_from(i).unwrap_or(usize::MAX))),
                _ => Err(ExecError::bind_type("LIMIT/OFFSET must be an integer")),
            }
        };
        let limit = match limit {
            Some(e) => count(e, "2201W")?,
            None => None,
        };
        let offset = match offset {
            Some(e) => count(e, "2201X")?.unwrap_or(0),
            None => 0,
        };
        Ok((limit, offset))
    }

    fn project(
        &mut self,
        rows: Vec<Tuple>,
        columns: &[OutputColumn],
    ) -> Result<Vec<Tuple>, ExecError> {
        let mut out = Vec::with_capacity(rows.len().min(1024));
        for t in rows {
            self.gov.check()?;
            let mut vals = Vec::with_capacity(columns.len());
            for c in columns {
                vals.push(eval(&c.expr, &t.vals, &self.ctx())?);
            }
            self.gov.charge(row_cost(&vals))?;
            out.push(Tuple { key: None, vals });
        }
        Ok(out)
    }

    fn sort(&mut self, rows: Vec<Tuple>, keys: &[SortKey]) -> Result<Vec<Tuple>, ExecError> {
        let mut keyed: Vec<(Vec<Datum>, Tuple)> = Vec::with_capacity(rows.len());
        for t in rows {
            self.gov.check()?;
            let mut k = Vec::with_capacity(keys.len());
            for sk in keys {
                let v = eval(&sk.expr, &t.vals, &self.ctx())?;
                k.push(sort_key(v, sk.expr.ty)?);
            }
            self.gov.charge(row_cost(&k))?;
            keyed.push((k, t));
        }
        self.gov.check()?;
        // `sort_by` is stable, so equal keys keep their key order, as `ORDER BY` allows.
        keyed.sort_by(|(a, _), (b, _)| {
            for (sk, (x, y)) in keys.iter().zip(a.iter().zip(b)) {
                let ord = match (x.is_null(), y.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => {
                        if sk.nulls_first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (false, true) => {
                        if sk.nulls_first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    (false, false) => {
                        let o = total_order(x, y);
                        if sk.desc { o.reverse() } else { o }
                    }
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        Ok(keyed.into_iter().map(|(_, t)| t).collect())
    }

    // ----- scan -----

    fn snapshot_for(&self, as_of: Option<&AsOfPlan>) -> Result<Snapshot, ExecError> {
        let Some(a) = as_of else {
            return Ok(self.txs.tx.snapshot());
        };
        match a {
            AsOfPlan::Changeset(e) => {
                let id = match eval(e, &[], &self.ctx())? {
                    Datum::Text(s) => ChangeSetId::parse(s.trim()).map_err(|_| {
                        ExecError::eval("22P02", format!("invalid changeset id \"{s}\""))
                    })?,
                    Datum::Int(i) => ChangeSetId::from_raw(u128::try_from(i).map_err(|_| {
                        ExecError::eval("22P02", "a changeset id must not be negative")
                    })?),
                    Datum::Null => {
                        return Err(ExecError::eval("22004", "AS OF CHANGESET needs a value"));
                    }
                    _ => return Err(ExecError::bind_type("a changeset id is text or an integer")),
                };
                self.store().snapshot_as_of_changeset(id).ok_or_else(|| {
                    ExecError::new(
                        ErrorKind::SnapshotTooOld,
                        "72000",
                        "changeset not found or older than the retained history",
                    )
                })
            }
            AsOfPlan::Timestamp(e) => {
                let micros = match eval(e, &[], &self.ctx())? {
                    Datum::Timestamp(t) => t,
                    Datum::Date(d) => crate::value::date_to_micros(d)
                        .ok_or_else(|| ExecError::eval("22008", "date out of range"))?,
                    _ => return Err(ExecError::eval("22004", "AS OF TIMESTAMP needs a value")),
                };
                // The newest commit at or before that millisecond, whatever its counter and node.
                let t = Hlc {
                    wall_ms: u64::try_from(micros.div_euclid(1000)).unwrap_or(0),
                    counter: u32::MAX,
                    node: u32::MAX,
                };
                Ok(self.store().try_snapshot_as_of_hlc(t)?)
            }
        }
    }

    fn scan(
        &mut self,
        table_id: u64,
        as_of: Option<&AsOfPlan>,
        predicate: Option<&TypedExpr>,
    ) -> Result<Vec<Tuple>, ExecError> {
        let def = self.table(table_id)?;
        let snapshot = self.snapshot_for(as_of)?;
        let stored = self.store().scan_at(snapshot, def.id, .., self.policy)?;
        let overlay = as_of.is_none();
        let mut out: Vec<Tuple> = Vec::with_capacity(stored.len().min(4096));
        for (key, value) in stored {
            self.gov.check()?;
            if overlay && self.txs.overlay.contains_key(&(def.id, key.to_vec())) {
                continue;
            }
            let vals = def.decode_row(value)?;
            if let Some(p) = predicate
                && !truthy(&eval(p, &vals, &self.ctx())?)
            {
                continue;
            }
            self.gov.charge(row_cost(&vals) + key.len() as u64)?;
            out.push(Tuple {
                key: Some(key.to_vec()),
                vals,
            });
        }
        if overlay {
            let lo = (def.id, Vec::new());
            let hi = (def.id.saturating_add(1), Vec::new());
            let mut added = false;
            for ((_, key), value) in self.txs.overlay.range(lo..hi) {
                self.gov.check()?;
                let Some(value) = value else { continue };
                // The transaction's own writes are subject to the read policy like stored rows.
                if !self.policy.allows_read(RowRef {
                    table: def.id,
                    key,
                    value,
                }) {
                    continue;
                }
                let vals = def.decode_row(value)?;
                if let Some(p) = predicate
                    && !truthy(&eval(p, &vals, &self.ctx())?)
                {
                    continue;
                }
                self.gov.charge(row_cost(&vals) + key.len() as u64)?;
                out.push(Tuple {
                    key: Some(key.clone()),
                    vals,
                });
                added = true;
            }
            if added {
                out.sort_by(|a, b| a.key.cmp(&b.key));
            }
        }
        Ok(out)
    }

    // ----- writes -----

    /// The stored bytes at a key, as the transaction sees them, ignoring the read policy: a
    /// constraint check must see every row even when the actor may not read it.
    fn existing(&self, table: u32, key: &[u8]) -> Result<Option<Vec<u8>>, ExecError> {
        if let Some(v) = self.txs.overlay.get(&(table, key.to_vec())) {
            return Ok(v.clone());
        }
        Ok(self
            .store()
            .get_at(self.txs.tx.snapshot(), table, key, &AllowAll)?
            .cloned())
    }

    /// Converts a value to a column's type and checks its constraints.
    fn assign(&self, d: Datum, col: &ColumnDef) -> Result<Datum, ExecError> {
        if d.is_null() {
            if col.not_null {
                return Err(not_null(&col.name));
            }
            return Ok(Datum::Null);
        }
        let d = match (&d, col.ty) {
            (Datum::Text(_), SqlType::Text) => d,
            _ => cast(d, col.ty)?,
        };
        // Whatever produced it (a cast, `date_trunc`, arithmetic on a date), a value the row codec
        // cannot read back is refused here instead of being stored.
        match &d {
            Datum::Timestamp(t) if !crate::value::timestamp_in_range(*t) => {
                return Err(ExecError::eval("22008", "timestamp out of range"));
            }
            Datum::Date(x) if !crate::value::date_in_range(*x) => {
                return Err(ExecError::eval("22008", "date out of range"));
            }
            _ => {}
        }
        if let (Datum::Text(s), Some(n)) = (&d, col.varchar_len)
            && s.chars().count() > n as usize
        {
            return Err(ExecError::eval(
                "22001",
                format!("value too long for type character varying({n})"),
            ));
        }
        Ok(d)
    }

    /// Writes a new row (key, row bytes and unique entries), failing with `23505` on a duplicate.
    fn put_row(&mut self, def: &TableDef, row: &[Datum], key: Vec<u8>) -> Result<(), ExecError> {
        if self.existing(def.id, &key)?.is_some() {
            let name = if def.primary_key.is_empty() {
                format!("{}_rowid", def.name)
            } else {
                format!("{}_pkey", def.name)
            };
            return Err(duplicate(&name));
        }
        let mut markers = Vec::new();
        for k in 0..def.uniques.len() {
            if let Some(mk) = def.unique_key(row, k)? {
                let table = def.marker_table(k);
                if self.existing(table, &mk)?.is_some() {
                    return Err(duplicate(&def.unique_name(k)));
                }
                markers.push((table, mk));
            }
        }
        let bytes = def.encode_row(row)?;
        self.txs.put(def.id, key.clone(), bytes);
        for (table, mk) in markers {
            self.txs.put(table, mk, key.clone());
        }
        Ok(())
    }

    fn remove_row(&mut self, def: &TableDef, row: &[Datum], key: &[u8]) -> Result<(), ExecError> {
        for k in 0..def.uniques.len() {
            if let Some(mk) = def.unique_key(row, k)? {
                self.txs.delete(def.marker_table(k), mk);
            }
        }
        self.txs.delete(def.id, key.to_vec());
        Ok(())
    }

    fn returning(
        &self,
        returning: &[OutputColumn],
        row: &[Datum],
        out: &mut Vec<Vec<Datum>>,
    ) -> Result<(), ExecError> {
        if returning.is_empty() {
            return Ok(());
        }
        let mut vals = Vec::with_capacity(returning.len());
        for c in returning {
            vals.push(eval(&c.expr, row, &self.ctx())?);
        }
        self.gov.charge(row_cost(&vals))?;
        out.push(vals);
        Ok(())
    }

    fn insert(&mut self, plan: &LogicalPlan) -> Result<RunOutput, ExecError> {
        let LogicalPlan::Insert {
            table_id,
            targets,
            source,
            returning,
            ..
        } = plan
        else {
            return Err(ExecError::internal("not an insert"));
        };
        let def = self.table(*table_id)?.clone();
        let source_rows: Vec<Vec<Datum>> = match source {
            InsertSource::Values(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    self.gov.check()?;
                    let mut vals = Vec::with_capacity(r.len());
                    for e in r {
                        vals.push(eval(e, &[], &self.ctx())?);
                    }
                    self.gov.charge(row_cost(&vals))?;
                    out.push(vals);
                }
                out
            }
            InsertSource::Query(q) => self.rel(q)?.into_iter().map(|t| t.vals).collect(),
        };
        let mut produced = Vec::new();
        let mut n = 0u64;
        for src in source_rows {
            self.gov.check()?;
            let mut row: Vec<Datum> = def
                .columns
                .iter()
                .map(|c| c.default.clone().unwrap_or(Datum::Null))
                .collect();
            for (p, &col_ix) in targets.iter().enumerate() {
                let (Some(slot), Some(v)) = (row.get_mut(col_ix), src.get(p)) else {
                    return Err(ExecError::internal("insert target outside the table"));
                };
                *slot = v.clone();
            }
            for (slot, col) in row.iter_mut().zip(&def.columns) {
                let v = std::mem::replace(slot, Datum::Null);
                *slot = self.assign(v, col)?;
            }
            let rowid = if def.primary_key.is_empty() {
                self.db.alloc_rowid(def.id)?
            } else {
                0
            };
            let key = def.row_key(&row, rowid)?;
            self.put_row(&def, &row, key)?;
            self.returning(returning, &row, &mut produced)?;
            n += 1;
        }
        Ok(RunOutput {
            columns: output_columns(returning),
            rows: produced,
            tag: format!("INSERT 0 {n}"),
            affected: n,
        })
    }

    fn update(&mut self, plan: &LogicalPlan) -> Result<RunOutput, ExecError> {
        let LogicalPlan::Update {
            table_id,
            input,
            assignments,
            returning,
            ..
        } = plan
        else {
            return Err(ExecError::internal("not an update"));
        };
        let def = self.table(*table_id)?.clone();
        let targets = self.rel(input)?;
        let mut produced = Vec::new();
        let mut n = 0u64;
        for t in targets {
            self.gov.check()?;
            let Some(old_key) = t.key else {
                return Err(ExecError::internal("an update input row has no key"));
            };
            let mut row = t.vals.clone();
            for (ix, e) in assignments {
                let v = eval(e, &t.vals, &self.ctx())?;
                let col = def
                    .columns
                    .get(*ix)
                    .ok_or_else(|| ExecError::internal("assignment outside the table"))?;
                let v = self.assign(v, col)?;
                if let Some(slot) = row.get_mut(*ix) {
                    *slot = v;
                }
            }
            let new_key = if def.primary_key.is_empty() {
                old_key.clone()
            } else {
                def.row_key(&row, 0)?
            };
            self.remove_row(&def, &t.vals, &old_key)?;
            self.put_row(&def, &row, new_key)?;
            self.returning(returning, &row, &mut produced)?;
            n += 1;
        }
        Ok(RunOutput {
            columns: output_columns(returning),
            rows: produced,
            tag: format!("UPDATE {n}"),
            affected: n,
        })
    }

    fn delete(&mut self, plan: &LogicalPlan) -> Result<RunOutput, ExecError> {
        let LogicalPlan::Delete {
            table_id,
            input,
            returning,
            ..
        } = plan
        else {
            return Err(ExecError::internal("not a delete"));
        };
        let def = self.table(*table_id)?.clone();
        let targets = self.rel(input)?;
        let mut produced = Vec::new();
        let mut n = 0u64;
        for t in targets {
            self.gov.check()?;
            let Some(key) = t.key else {
                return Err(ExecError::internal("a delete input row has no key"));
            };
            self.remove_row(&def, &t.vals, &key)?;
            self.returning(returning, &t.vals, &mut produced)?;
            n += 1;
        }
        Ok(RunOutput {
            columns: output_columns(returning),
            rows: produced,
            tag: format!("DELETE {n}"),
            affected: n,
        })
    }
}

/// Brings a sort key to the variant its expression type promises, so keys of one `ORDER BY` column
/// are always the same kind of number and compare exactly (an expression such as
/// `coalesce(numeric_col, int_col)` can yield either).
fn sort_key(d: Datum, ty: SqlType) -> Result<Datum, ExecError> {
    let fits = matches!(
        (&d, ty),
        (Datum::Null, _)
            | (
                Datum::Int(_),
                SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
            )
            | (Datum::Numeric(_), SqlType::Numeric)
            | (Datum::Float(_), SqlType::DoublePrecision)
    );
    if fits || !ty.is_numeric() {
        Ok(d)
    } else {
        cast(d, ty)
    }
}

fn window(rows: Vec<Tuple>, limit: Option<usize>, offset: usize) -> Vec<Tuple> {
    let it = rows.into_iter().skip(offset);
    match limit {
        Some(l) => it.take(l).collect(),
        None => it.collect(),
    }
}

fn not_null(column: &str) -> ExecError {
    ExecError::new(
        ErrorKind::Constraint,
        "23502",
        format!("null value in column \"{column}\" violates not-null constraint"),
    )
}

fn duplicate(constraint: &str) -> ExecError {
    ExecError::new(
        ErrorKind::Constraint,
        "23505",
        format!("duplicate key value violates unique constraint \"{constraint}\""),
    )
}
