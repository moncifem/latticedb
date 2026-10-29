//! The only storage boundary: actual MVCC snapshots and RowTx publication through the WAL.
use crate::{
    Error, Result,
    ast::{Expr, Historical, Value},
};
use lattice_core::record::{Decoded, SEGMENT_HEADER_LEN, decode_record};
use lattice_core::{ChangeSet, Config};
use lattice_engines::{
    limits::AllowAll,
    mvcc::Snapshot,
    rowops,
    store::{RowError, RowStore, RowTx},
};
use lattice_seams::{Clock, Io, IoError, Rng};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Debug)]
pub struct Shared<I>(pub Rc<RefCell<I>>);
impl<I> Clone for Shared<I> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<I: Io> Io for Shared<I> {
    fn exists(&self, p: &str) -> bool {
        self.0.borrow().exists(p)
    }
    fn len(&self, p: &str) -> std::result::Result<u64, IoError> {
        self.0.borrow().len(p)
    }
    fn read(&self, p: &str) -> std::result::Result<Vec<u8>, IoError> {
        self.0.borrow().read(p)
    }
    fn append(&mut self, p: &str, b: &[u8]) -> std::result::Result<(), IoError> {
        self.0.borrow_mut().append(p, b)
    }
    fn truncate(&mut self, p: &str, n: u64) -> std::result::Result<(), IoError> {
        self.0.borrow_mut().truncate(p, n)
    }
    fn sync(&mut self, p: &str) -> std::result::Result<(), IoError> {
        self.0.borrow_mut().sync(p)
    }
    fn sync_dir(&mut self, p: &str) -> std::result::Result<(), IoError> {
        self.0.borrow_mut().sync_dir(p)
    }
    fn list(&self, p: &str) -> std::result::Result<Vec<String>, IoError> {
        self.0.borrow().list(p)
    }
    fn remove(&mut self, p: &str) -> std::result::Result<(), IoError> {
        self.0.borrow_mut().remove(p)
    }
    fn create_dir_all(&mut self, p: &str) -> std::result::Result<(), IoError> {
        self.0.borrow_mut().create_dir_all(p)
    }
    fn lock(&mut self, p: &str) -> std::result::Result<(), IoError> {
        self.0.borrow_mut().lock(p)
    }
    fn unlock(&mut self, p: &str) {
        self.0.borrow_mut().unlock(p)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub lsn: u64,
    pub changeset: String,
    pub wall_ms: u64,
    pub writes: Vec<lattice_engines::rowops::RowWrite>,
}
#[derive(Default)]
pub struct Session {
    pub tx: Option<RowTx>,
    pub pending: BTreeMap<(u32, Vec<u8>), Option<Vec<Value>>>,
}
pub struct Db<I: Io, C: Clock, R: Rng> {
    pub rows: RowStore<Shared<I>, C, R>,
    pub io: Shared<I>,
    pub trace: Vec<serde_json::Value>,
    pub receipts: Vec<Receipt>,
    pub tracing: bool,
    #[cfg(feature = "fault-hooks")]
    pub before_commit: Option<Box<dyn FnMut(usize)>>,
}
pub fn schema(table: &str) -> Result<(u32, Vec<&'static str>)> {
    match table {
        "tickets" => Ok((1, vec!["id", "owner", "priority"])),
        "users" => Ok((2, vec!["id", "name"])),
        _ => Err(Error::new(
            "42P01",
            "LSQL-0003",
            format!("unknown table {table}"),
        )),
    }
}
pub fn key(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Int(n) => Ok(n.to_be_bytes().to_vec()),
        _ => Err(Error::new("23502", "LSQL-0004", "integer id required")),
    }
}
impl<I: Io, C: Clock, R: Rng> Db<I, C, R> {
    pub fn open(io: I, clock: C, rng: R) -> Result<Self> {
        let io = Shared(Rc::new(RefCell::new(io)));
        let rows = RowStore::open_with_indexes(
            io.clone(),
            clock,
            rng,
            &Config {
                dir: "wal".into(),
                segment_limit: 256 * 1024,
                node: 1,
            },
            vec![lattice_engines::index::IndexDef {
                id: 9,
                table: 1,
                extract: |bytes| {
                    let row: Vec<Value> = serde_json::from_slice(bytes).ok()?;
                    match row.get(2)? {
                        Value::Int(n) => Some(n.to_be_bytes().to_vec()),
                        _ => None,
                    }
                },
            }],
        )
        .map_err(|e| Error::storage(e.to_string()))?;
        let receipts = read_receipts(&io)?;
        Ok(Self {
            rows,
            io,
            trace: vec![],
            receipts,
            tracing: true,
            #[cfg(feature = "fault-hooks")]
            before_commit: None,
        })
    }
    pub fn event(&mut self, event: impl FnOnce() -> serde_json::Value) {
        if self.tracing {
            self.trace.push(event());
        }
    }
    pub fn begin(&mut self, s: &mut Session) -> Result<()> {
        if s.tx.is_some() {
            return Err(Error::unsupported("nested BEGIN"));
        }
        let tx = self.rows.begin();
        self.event(|| serde_json::json!({"begin":tx.snapshot().as_of}));
        s.tx = Some(tx);
        Ok(())
    }
    pub fn rollback(&mut self, s: &mut Session) {
        self.event(|| serde_json::json!({"rollback":s.tx.as_ref().map(|t|t.snapshot().as_of)}));
        s.tx = None;
        s.pending.clear();
    }
    pub fn snapshot(&self, s: &Session, historical: &Option<Historical>) -> Result<Snapshot> {
        match historical {
            None => Ok(s
                .tx
                .as_ref()
                .map(|t| t.snapshot())
                .unwrap_or_else(|| self.rows.snapshot())),
            Some(Historical::ChangeSet(id)) => self
                .receipts
                .iter()
                .find(|r| &r.changeset == id)
                .map(|r| Snapshot { as_of: r.lsn })
                .ok_or_else(|| Error::new("72000", "LSQL-0006", "unknown historical change set")),
            Some(Historical::Timestamp(t)) => Ok(Snapshot {
                as_of: self
                    .receipts
                    .iter()
                    .filter(|r| r.wall_ms <= *t)
                    .map(|r| r.lsn)
                    .max()
                    .unwrap_or(0),
            }),
        }
    }
    pub fn scan(
        &mut self,
        s: &Session,
        table: &str,
        historical: &Option<Historical>,
    ) -> Result<Vec<Vec<Value>>> {
        self.scan_filtered(s, table, historical, &None, &[])
    }
    /// Push only proven primary-key bounds; the executor still checks the complete predicate.
    pub fn scan_filtered(
        &mut self,
        s: &Session,
        table: &str,
        historical: &Option<Historical>,
        filter: &Option<Expr>,
        params: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        use std::ops::Bound;
        let (id, _) = schema(table)?;
        let snap = self.snapshot(s, historical)?;
        let mut rows = BTreeMap::new();
        self.rows
            .check_snapshot(snap)
            .map_err(|e| Error::storage(e.to_string()))?;
        let bounds = primary_bounds(table, filter.as_ref(), params);
        let in_bounds = |k: &Vec<u8>| {
            let lower = match &bounds.0 {
                Bound::Included(v) => k >= v,
                Bound::Excluded(v) => k > v,
                Bound::Unbounded => true,
            };
            let upper = match &bounds.1 {
                Bound::Included(v) => k <= v,
                Bound::Excluded(v) => k < v,
                Bound::Unbounded => true,
            };
            lower && upper
        };
        // Contradictory bounds are empty, rather than panicking inside BTreeMap::range.
        let empty = match (&bounds.0, &bounds.1) {
            (Bound::Included(a), Bound::Included(b)) => a > b,
            (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
                a >= b
            }
            _ => false,
        };
        let stored = if empty {
            vec![]
        } else {
            self.rows
                .scan_at(snap, id, bounds.clone(), &AllowAll)
                .map_err(|e| Error::storage(e.to_string()))?
        };
        for (k, v) in stored {
            rows.insert(
                k.to_vec(),
                serde_json::from_slice::<Vec<Value>>(v)
                    .map_err(|e| Error::storage(e.to_string()))?,
            );
        }
        if historical.is_none() {
            for ((t, k), v) in &s.pending {
                if *t == id && !empty && in_bounds(k) {
                    match v {
                        Some(v) => {
                            rows.insert(k.clone(), v.clone());
                        }
                        None => {
                            rows.remove(k);
                        }
                    }
                }
            }
        }
        let values = rows.into_values().collect::<Vec<_>>();
        self.event(|| serde_json::json!({"scan":table,"snapshot":snap.as_of,"rows":values,"primary_bounds":format!("{bounds:?}")}));
        Ok(values)
    }
    pub fn stage(&mut self, s: &mut Session, table: &str, row: Vec<Value>) -> Result<()> {
        let (id, columns) = schema(table)?;
        if row.len() != columns.len() || !matches!(row.first(), Some(Value::Int(_))) {
            return Err(Error::new(
                "23502",
                "LSQL-0004",
                "invalid fixture row shape",
            ));
        }
        if table == "tickets" && row.iter().any(|v| !matches!(v, Value::Int(_))) {
            return Err(Error::new(
                "23502",
                "LSQL-0004",
                "ticket columns are NOT NULL bigint",
            ));
        }
        if table == "users" && !matches!(row.get(1), Some(Value::Text(_))) {
            return Err(Error::new(
                "23502",
                "LSQL-0004",
                "user name is NOT NULL text",
            ));
        }
        let k = key(&row[0])?;
        let bytes = serde_json::to_vec(&row).unwrap();
        let tx =
            s.tx.as_mut()
                .ok_or_else(|| Error::storage("stage without transaction"))?;
        tx.put((id, k.clone()), bytes);
        s.pending.insert((id, k.clone()), Some(row.clone()));
        self.event(|| serde_json::json!({"stage":table,"key":k,"row":row}));
        Ok(())
    }
    pub fn commit(&mut self, s: &mut Session) -> Result<Option<Receipt>> {
        let Some(tx) = s.tx.take() else {
            return Ok(None);
        };
        let writes = tx.clone().into_writes();
        s.pending.clear();
        #[cfg(feature = "fault-hooks")]
        if !writes.is_empty()
            && let Some(mut hook) = self.before_commit.take()
        {
            hook(writes.len());
        }
        let lsn = self.rows.commit(tx).map_err(|e| match e {
            RowError::Conflict { .. } => Error::new("40001", "LSQL-0005", e.to_string()),
            _ => Error::storage(e.to_string()),
        })?;
        if writes.is_empty() {
            return Ok(None);
        }
        let receipt = read_receipts(&self.io)?
            .into_iter()
            .find(|r| r.lsn == lsn)
            .ok_or_else(|| Error::storage("receipt missing from WAL"))?;
        if receipt.writes != writes {
            return Err(Error::storage("partial/mismatched WAL envelope"));
        }
        self.event(|| serde_json::json!({"commit":receipt}));
        self.receipts.push(receipt.clone());
        Ok(Some(receipt))
    }
    pub fn seed(&mut self, tickets: usize) -> Result<()> {
        let mut s = Session::default();
        self.begin(&mut s)?;
        for i in 0..100 {
            self.stage(
                &mut s,
                "users",
                vec![Value::Int(i), Value::Text(format!("user{i:03}"))],
            )?;
        }
        for i in 0..tickets {
            self.stage(
                &mut s,
                "tickets",
                vec![
                    Value::Int(i as i64),
                    Value::Int((i % 100) as i64),
                    Value::Int((i % 7) as i64),
                ],
            )?;
        }
        self.commit(&mut s)?;
        self.trace.clear();
        Ok(())
    }
}
pub fn read_receipts(io: &impl Io) -> Result<Vec<Receipt>> {
    let mut out = Vec::new();
    for file in io
        .list("wal")
        .map_err(|e| Error::storage(e.to_string()))?
        .into_iter()
        .filter(|f| f.ends_with(".seg"))
    {
        let bytes = io
            .read(&format!("wal/{file}"))
            .map_err(|e| Error::storage(e.to_string()))?;
        let mut at = SEGMENT_HEADER_LEN;
        while at < bytes.len() {
            let Decoded::Record(record, next) = decode_record(&bytes, at) else {
                return Err(Error::storage("undecodable WAL tail"));
            };
            at = next;
            let cs =
                ChangeSet::decode(&record.payload).map_err(|e| Error::storage(e.to_string()))?;
            let writes =
                rowops::decode(&cs.payload).map_err(|e| Error::storage(format!("{e:?}")))?;
            out.push(Receipt {
                lsn: record.lsn,
                changeset: cs.id.to_string(),
                wall_ms: cs.hlc.wall_ms,
                writes,
            });
        }
    }
    Ok(out)
}

/// The existing key codec orders signed negatives after nonnegatives. An equality is always safe;
/// interval pushdown is limited to a proven nonnegative lower bound, leaving every other case to
/// the full predicate. Do not treat an unproven upper bound as a complete signed-key range.
pub fn primary_bounds(
    table: &str,
    filter: Option<&Expr>,
    params: &[Value],
) -> (std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>) {
    use std::ops::Bound::{self, Excluded, Included, Unbounded};
    fn value(e: &Expr, params: &[Value]) -> Option<i64> {
        match e {
            Expr::Lit(Value::Int(n)) => Some(*n),
            Expr::Param(n) => {
                n.checked_sub(1)
                    .and_then(|at| params.get(at))
                    .and_then(|v| match v {
                        Value::Int(n) => Some(*n),
                        _ => None,
                    })
            }
            _ => None,
        }
    }
    fn terms(e: &Expr, table: &str, params: &[Value], out: &mut Vec<(String, i64)>) {
        if let Expr::Binary(a, op, b) = e {
            if op == "and" {
                terms(a, table, params, out);
                terms(b, table, params, out);
                return;
            }
            let col =
                |e: &Expr| matches!(e, Expr::Col(c) if c == "id" || c == &format!("{table}.id"));
            if col(a)
                && let Some(n) = value(b, params)
            {
                out.push((op.clone(), n));
            } else if col(b)
                && let Some(n) = value(a, params)
            {
                let reversed = match op.as_str() {
                    ">" => "<",
                    "<" => ">",
                    ">=" => "<=",
                    "<=" => ">=",
                    "=" => "=",
                    _ => return,
                };
                out.push((reversed.into(), n));
            }
        }
    }
    let mut comparisons = vec![];
    if let Some(e) = filter {
        terms(e, table, params, &mut comparisons);
    }
    if let Some((_, n)) = comparisons.iter().find(|(op, _)| op == "=") {
        let key = n.to_be_bytes().to_vec();
        return (Included(key.clone()), Included(key));
    }
    let mut lo: Bound<i64> = Unbounded;
    let mut hi: Bound<i64> = Unbounded;
    for (op, n) in comparisons {
        match op.as_str() {
            ">" | ">=" if n >= 0 => {
                let next = if op == ">" { Excluded(n) } else { Included(n) };
                let stronger = match &lo {
                    Unbounded => true,
                    Included(v) => n > *v || n == *v && op == ">",
                    Excluded(v) => n > *v,
                };
                if stronger {
                    lo = next;
                }
            }
            "<" | "<=" => {
                let next = if op == "<" { Excluded(n) } else { Included(n) };
                let stronger = match &hi {
                    Unbounded => true,
                    Included(v) => n < *v || n == *v && op == "<",
                    Excluded(v) => n < *v,
                };
                if stronger {
                    hi = next;
                }
            }
            _ => {}
        }
    }
    if matches!(lo, Unbounded) || matches!(hi,Included(n)|Excluded(n) if n<0) {
        return (Unbounded, Unbounded);
    }
    let bytes = |b: Bound<i64>| match b {
        Included(n) => Included(n.to_be_bytes().to_vec()),
        Excluded(n) => Excluded(n.to_be_bytes().to_vec()),
        Unbounded => Unbounded,
    };
    // An unbounded upper edge must stop before negative keys, which sort after positive keys.
    let hi = if matches!(hi, Unbounded) {
        Excluded(i64::MIN.to_be_bytes().to_vec())
    } else {
        bytes(hi)
    };
    (bytes(lo), hi)
}
