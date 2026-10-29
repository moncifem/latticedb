//! A row store on top of the kernel: rows live in memory as versions, and the log is the only
//! durable copy.
//!
//! A commit is one change set. The conflict rule is checked first (first committer wins, see
//! `formats/isolation.md`), the change set goes through the kernel's commit path, and only when the
//! kernel acknowledges it durable are its writes applied to the in-memory versions at the commit LSN,
//! in one call, so every reader sees all of the change set or none of it. Recovery replays the log's
//! row change sets through the same apply, so a restart reproduces exactly the acknowledged state.

use crate::catalog::{self, Catalog, CatalogError, CatalogOp, CompoundChange, IndexOp};
use crate::index::{IndexDef, Indexes};
use crate::limits::{
    AllowAll, PolicyEvaluator, RowRef, RowWriteRef, authorize_read, authorize_write,
};
use crate::mvcc::{MvccError, Snapshot, Table, Tx};
use crate::rowops::{self, RowKey, RowOpsError};
use lattice_abi::{Classify, ErrorClass, Hlc};
use lattice_core::bugs::{self, Bug};
use lattice_core::changeset::{ChangeSet, ChangeSetId};
use lattice_core::record::RecordType;
use lattice_core::{Config, Kernel, Receipt, TxnError};
use lattice_seams::{Clock, Io, Rng};
use std::fmt;
use std::ops::{Bound, RangeBounds};

/// A transaction whose policy is bound at creation and cannot be replaced at commit.
///
/// `P` may own actor context or borrow an evaluator. Default transactions explicitly use Phase 2's
/// [`AllowAll`]. Proposed writes are buffered privately until the store checks the entire final
/// transition set before appending anything to the WAL.
#[derive(Clone)]
pub struct RowTx<P = AllowAll> {
    inner: Tx<RowKey, Vec<u8>>,
    policy: P,
}

impl<P> RowTx<P> {
    /// The immutable transaction snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.inner.snapshot()
    }

    /// Buffers a proposed write. The policy is checked at read and commit, not bypassed by staging.
    pub fn put(&mut self, key: RowKey, value: Vec<u8>) {
        self.inner.put(key, value);
    }

    /// Buffers a proposed deletion; commit checks both its old value and absence afterward.
    pub fn delete(&mut self, key: RowKey) {
        self.inner.delete(key);
    }

    /// Whether the transaction has staged no writes.
    pub fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
}

/// Borrowed rows returned by a snapshot scan, in primary-key order.
pub type ScanRows<'a> = Vec<(&'a [u8], &'a Vec<u8>)>;

/// Why a store operation failed.
#[derive(Debug, Clone, PartialEq)]
pub enum RowError {
    /// Another change set wrote one of the same rows after this transaction's snapshot. Safe to
    /// retry as a new transaction (SQLSTATE 40001).
    Conflict {
        /// The LSN of the newer write.
        newer_lsn: u64,
    },
    /// The kernel refused the change set; nothing was written.
    Rejected(TxnError),
    /// The commit failed in a way that leaves it unknown whether it is durable. The store is stale:
    /// reopen it, and the log decides.
    InDoubt(TxnError),
    /// An earlier ambiguous failure left this store behind the log; reopen it.
    Stale,
    /// The snapshot is older than the vacuum floor, so row versions it needs may be gone. History in
    /// the log is intact: reopen the store to read it.
    SnapshotTooOld {
        /// The oldest snapshot that can still be read.
        floor: u64,
    },
    /// The log holds a row change set this code cannot read.
    Corrupt(String),
    /// The index definitions cannot identify their entries unambiguously.
    InvalidIndexes(String),
    /// A row transition was denied; no part of the transaction was appended or applied.
    PermissionDenied,
    /// The catalog or managed index definition change is invalid; nothing was written.
    InvalidCatalog(CatalogError),
    /// The writes do not fit the payload format.
    Encode(RowOpsError),
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowError::Conflict { newer_lsn } => {
                write!(f, "write conflict: a row was changed at lsn {newer_lsn}")
            }
            RowError::Rejected(e) => write!(f, "rejected: {e}"),
            RowError::InDoubt(e) => write!(f, "commit outcome unknown: {e}"),
            RowError::SnapshotTooOld { floor } => {
                write!(
                    f,
                    "snapshot too old: versions below lsn {floor} were vacuumed"
                )
            }
            RowError::Stale => write!(f, "store is stale after an ambiguous failure; reopen it"),
            RowError::Corrupt(why) => write!(f, "row data corrupt: {why}"),
            RowError::InvalidIndexes(why) => write!(f, "invalid index definitions: {why}"),
            RowError::InvalidCatalog(error) => write!(f, "invalid catalog change: {error}"),
            RowError::Encode(e) => write!(f, "cannot encode writes: {e:?}"),
            RowError::PermissionDenied => f.write_str("row write denied"),
        }
    }
}

impl Classify for RowError {
    fn class(&self) -> ErrorClass {
        match self {
            RowError::Conflict { .. } | RowError::Stale => ErrorClass::Retryable,
            RowError::Rejected(e) | RowError::InDoubt(e) => e.class(),
            RowError::SnapshotTooOld { .. } => ErrorClass::Rejected,
            RowError::Corrupt(_) => ErrorClass::Fatal,
            RowError::Encode(_)
            | RowError::InvalidIndexes(_)
            | RowError::PermissionDenied
            | RowError::InvalidCatalog(_) => ErrorClass::Rejected,
        }
    }
}

fn mvcc_error(error: MvccError) -> RowError {
    match error {
        MvccError::Conflict { newer_lsn } => RowError::Conflict { newer_lsn },
        MvccError::SnapshotTooOld { floor } => RowError::SnapshotTooOld { floor },
        MvccError::OutOfOrder { .. } => RowError::Stale,
    }
}

fn may_read(
    policy: &(impl PolicyEvaluator + ?Sized),
    table: u32,
    key: &[u8],
    value: &[u8],
) -> bool {
    bugs::active(Bug::RowPolicyReadSkipped)
        || authorize_read(policy, RowRef { table, key, value }).is_ok()
}

/// What one [`RowStore::vacuum`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VacuumStats {
    /// The horizon that was applied (clamped to the applied LSN and never below an earlier one).
    pub horizon: u64,
    /// Row versions removed.
    pub row_versions: usize,
    /// Index entry versions removed.
    pub index_versions: usize,
}

/// Rows over the kernel.
pub struct RowStore<I: Io, C: Clock, R: Rng> {
    kernel: Kernel<I, C, R>,
    table: Table<RowKey, Vec<u8>>,
    indexes: Indexes,
    catalog: Catalog,
    legacy_indexes: bool,
    /// Commit LSN and timestamp of each row change set at or above the vacuum floor. The HLC grows
    /// with the LSN, so `AS OF` can name a retained point by change set or by time.
    commit_lsn: std::collections::BTreeMap<ChangeSetId, u64>,
    commit_times: Vec<(Hlc, u64, ChangeSetId)>,
    stale: bool,
}

impl<I: Io, C: Clock, R: Rng> RowStore<I, C, R> {
    /// Opens the log and rebuilds the rows from every row change set in it, streaming, so recovery
    /// memory is the row state itself and one log segment.
    pub fn open(io: I, clock: C, rng: R, config: &Config) -> Result<Self, RowError> {
        Self::open_with_indexes(io, clock, rng, config, Vec::new())
    }

    /// Opens the store with indexes derived from the row history. Index definitions must be
    /// deterministic and use distinct ids, and must be supplied again on every reopen. A nonempty
    /// callback list selects legacy mode and refuses LCIX compound payloads instead of replacing
    /// persisted definitions with process-local guesses. An empty list selects managed mode.
    pub fn open_with_indexes(
        io: I,
        clock: C,
        rng: R,
        config: &Config,
        index_defs: Vec<IndexDef>,
    ) -> Result<Self, RowError> {
        let mut ids = std::collections::BTreeSet::new();
        if index_defs.iter().any(|def| !ids.insert(def.id)) {
            return Err(RowError::InvalidIndexes("duplicate index id".into()));
        }
        let legacy_indexes = !index_defs.is_empty();
        let mut table: Table<RowKey, Vec<u8>> = Table::new();
        let mut indexes = Indexes::new(index_defs);
        let mut catalog = Catalog::new();
        let mut commit_lsn = std::collections::BTreeMap::new();
        let mut commit_times: Vec<(Hlc, u64, ChangeSetId)> = Vec::new();
        let mut failure: Option<RowError> = None;
        let (kernel, _, _) = Kernel::open_streaming(io, clock, rng, config, |r| {
            if failure.is_some() || RecordType::from_u8(r.rtype) != Some(RecordType::ChangeSet) {
                return;
            }
            let cs = match ChangeSet::decode(&r.payload) {
                Ok(cs) => cs,
                Err(e) => {
                    failure = Some(RowError::Corrupt(format!(
                        "change set at lsn {}: {e}",
                        r.lsn
                    )));
                    return;
                }
            };
            let applied = (|| -> Result<bool, RowError> {
                let (mut writes, next_catalog) = if catalog::is_compound_payload(&cs.payload) {
                    if legacy_indexes {
                        return Err(RowError::InvalidIndexes(
                            "callback mode cannot replay managed compound changes".into(),
                        ));
                    }
                    let change = catalog::decode_compound(&cs.payload).map_err(|e| {
                        RowError::Corrupt(format!("compound at lsn {}: {e}", r.lsn))
                    })?;
                    let mut next = catalog.clone();
                    next.apply_change(r.lsn, &change.tables, &change.indexes)
                        .map_err(|e| RowError::Corrupt(format!("catalog at lsn {}: {e}", r.lsn)))?;
                    (change.rows, Some(next))
                } else if catalog::is_catalog_payload(&cs.payload) {
                    let ops = catalog::decode(&cs.payload)
                        .map_err(|e| RowError::Corrupt(format!("catalog at lsn {}: {e}", r.lsn)))?;
                    let mut next = catalog.clone();
                    next.apply(r.lsn, &ops)
                        .map_err(|e| RowError::Corrupt(format!("catalog at lsn {}: {e}", r.lsn)))?;
                    (Vec::new(), Some(next))
                } else if rowops::is_row_payload(&cs.payload) {
                    (
                        rowops::decode(&cs.payload).map_err(|e| {
                            RowError::Corrupt(format!("row writes at lsn {}: {e:?}", r.lsn))
                        })?,
                        None,
                    )
                } else {
                    return Ok(false);
                };
                if bugs::active(Bug::RowRecoveryDropsDeletes) {
                    writes.retain(|(_, value)| value.is_some());
                }
                if let Some(next) = next_catalog {
                    let definitions = if bugs::active(Bug::ManagedIndexesNotRecovered) {
                        Default::default()
                    } else {
                        next.indexes()
                    };
                    let changes = indexes
                        .catalog_changes_for(&table, &writes, &definitions)
                        .map_err(mvcc_error)?;
                    table.apply(r.lsn, cs.id, writes).map_err(mvcc_error)?;
                    indexes
                        .apply_catalog(r.lsn, cs.id, changes, definitions)
                        .map_err(mvcc_error)?;
                    catalog = next;
                } else {
                    let changes = indexes.changes_for(&table, &writes).map_err(mvcc_error)?;
                    table
                        .apply(r.lsn, cs.id, writes)
                        .and_then(|()| indexes.apply(r.lsn, cs.id, changes))
                        .map_err(mvcc_error)?;
                }
                Ok(true)
            })();
            match applied {
                Ok(true) => {
                    commit_lsn.insert(cs.id, r.lsn);
                    commit_times.push((cs.hlc, r.lsn, cs.id));
                }
                Ok(false) => {}
                Err(error) => failure = Some(error),
            }
        })
        .map_err(RowError::Rejected)?;
        match failure {
            Some(e) => Err(e),
            None => Ok(Self {
                kernel,
                table,
                indexes,
                catalog,
                legacy_indexes,
                commit_lsn,
                commit_times,
                stale: false,
            }),
        }
    }

    /// Versioned table and managed-index definitions recovered from the shared WAL.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Commits catalog and managed-index operations without row writes.
    pub fn commit_catalog(
        &mut self,
        tables: Vec<CatalogOp>,
        indexes: Vec<IndexOp>,
    ) -> Result<u64, RowError> {
        self.commit_with_catalog(self.begin(), tables, indexes)
    }

    /// Atomically commits table/index metadata and this transaction's row mutations.
    ///
    /// New indexes are built synchronously from currently visible rows, then maintained for these
    /// writes. This can block the caller and is not an online-backfill implementation. Metadata is
    /// checked before WAL append; rows, entries and definitions publish at the single returned LSN.
    /// Ordinary row-only calls retain the original LROW payload. Callback mode cannot write LCIX.
    pub fn commit_with_catalog<P: PolicyEvaluator>(
        &mut self,
        tx: RowTx<P>,
        tables: Vec<CatalogOp>,
        indexes: Vec<IndexOp>,
    ) -> Result<u64, RowError> {
        if tables.is_empty() && indexes.is_empty() {
            return self.commit(tx);
        }
        if self.stale {
            return Err(RowError::Stale);
        }
        if self.legacy_indexes {
            return Err(RowError::InvalidIndexes(
                "callback mode cannot write managed compound changes".into(),
            ));
        }
        self.check_snapshot(tx.snapshot())?;
        if !bugs::active(Bug::RowConflictCheckSkipped) {
            tx.inner.check(&self.table).map_err(mvcc_error)?;
        }
        let (_, definitions) = self
            .catalog
            .check_change(&tables, &indexes)
            .map_err(RowError::InvalidCatalog)?;
        let snapshot = tx.snapshot();
        let RowTx { inner, policy } = tx;
        let writes = inner.into_writes();
        if !bugs::active(Bug::RowPolicyWriteSkipped) {
            for ((table, key), after) in &writes {
                let before = self
                    .table
                    .get(snapshot, &(*table, key.clone()))
                    .map_err(mvcc_error)?;
                authorize_write(
                    &policy,
                    RowWriteRef {
                        table: *table,
                        key,
                        before: before.map(Vec::as_slice),
                        after: after.as_deref(),
                    },
                )
                .map_err(|_| RowError::PermissionDenied)?;
            }
        }
        let changes = self
            .indexes
            .catalog_changes_for(&self.table, &writes, &definitions)
            .map_err(mvcc_error)?;
        let payload = catalog::encode_compound(&CompoundChange {
            tables: tables.clone(),
            indexes: indexes.clone(),
            rows: writes.clone(),
        })
        .map_err(RowError::InvalidCatalog)?;
        let id = self.kernel.new_id();
        match self.kernel.commit(ChangeSet::new(id, payload)) {
            Ok(Receipt::Recorded { lsn, hlc }) => {
                let applied = (|| {
                    self.table.apply(lsn, id, writes).map_err(mvcc_error)?;
                    self.indexes
                        .apply_catalog(lsn, id, changes, definitions)
                        .map_err(mvcc_error)?;
                    self.catalog
                        .apply_change(lsn, &tables, &indexes)
                        .map_err(|e| {
                            RowError::Corrupt(format!("catalog apply after commit: {e}"))
                        })?;
                    Ok(())
                })();
                if let Err(error) = applied {
                    self.stale = true;
                    return Err(error);
                }
                self.commit_lsn.insert(id, lsn);
                self.commit_times.push((hlc, lsn, id));
                Ok(lsn)
            }
            Ok(Receipt::Duplicate) => {
                self.stale = true;
                Err(RowError::Corrupt(
                    "a fresh change set id was a duplicate".into(),
                ))
            }
            Err(error) if error.class() == ErrorClass::Rejected => Err(RowError::Rejected(error)),
            Err(error) => {
                self.stale = true;
                Err(RowError::InDoubt(error))
            }
        }
    }

    /// Starts a transaction on the newest snapshot with Phase 2's explicit allow-all policy.
    /// Request handlers with actor context must use [`Self::begin_with_policy`] instead.
    pub fn begin(&self) -> RowTx {
        self.begin_with_policy(AllowAll)
    }

    /// Binds an actor policy to a transaction. It may own context or borrow an immutable evaluator;
    /// the store never installs mutable global principal state.
    pub fn begin_with_policy<P: PolicyEvaluator>(&self, policy: P) -> RowTx<P> {
        RowTx {
            inner: Tx::begin(&self.table),
            policy,
        }
    }

    /// The newest snapshot: everything committed so far.
    pub fn snapshot(&self) -> Snapshot {
        self.table.snapshot()
    }

    /// Reads a row inside a transaction: its own writes first, then its snapshot. Its bound actor
    /// policy applies to both; denied rows are returned as absent. An expired transaction fails
    /// even if the requested value is buffered in its own writes.
    pub fn get<'a, P: PolicyEvaluator>(
        &'a self,
        tx: &'a RowTx<P>,
        table: u32,
        key: &[u8],
    ) -> Result<Option<&'a Vec<u8>>, RowError> {
        let value = tx
            .inner
            .get(&self.table, &(table, key.to_vec()))
            .map_err(mvcc_error)?;
        Ok(value.filter(|value| may_read(&tx.policy, table, key, value)))
    }

    /// Reads a row as of a snapshot with the statement's explicit actor policy. Denied rows are
    /// returned as absent; snapshots below the vacuum floor fail before authorization.
    pub fn get_at(
        &self,
        snap: Snapshot,
        table: u32,
        key: &[u8],
        policy: &(impl PolicyEvaluator + ?Sized),
    ) -> Result<Option<&Vec<u8>>, RowError> {
        let value = self
            .table
            .get(snap, &(table, key.to_vec()))
            .map_err(mvcc_error)?;
        Ok(value.filter(|value| may_read(policy, table, key, value)))
    }

    /// Authorized live rows of `table` in `range` as of `snap`, in primary-key order. Hidden rows
    /// never enter the returned batch; subsequent filters, joins and aggregates must use this view.
    pub fn scan_at<B: RangeBounds<Vec<u8>>>(
        &self,
        snap: Snapshot,
        table: u32,
        range: B,
        policy: &(impl PolicyEvaluator + ?Sized),
    ) -> Result<ScanRows<'_>, RowError> {
        self.check_snapshot(snap)?;
        let lo = match range.start_bound() {
            Bound::Included(k) => Bound::Included((table, k.clone())),
            Bound::Excluded(k) => Bound::Excluded((table, k.clone())),
            Bound::Unbounded => Bound::Included((table, Vec::new())),
        };
        let hi = match range.end_bound() {
            Bound::Included(k) => Bound::Included((table, k.clone())),
            Bound::Excluded(k) if bugs::active(Bug::RowScanUpperBoundIncluded) => {
                Bound::Included((table, k.clone()))
            }
            Bound::Excluded(k) => Bound::Excluded((table, k.clone())),
            Bound::Unbounded => match table.checked_add(1) {
                Some(next) => Bound::Excluded((next, Vec::new())),
                None => Bound::Unbounded,
            },
        };
        Ok(self
            .table
            .range(snap, (lo, hi))
            .map_err(mvcc_error)?
            .into_iter()
            .filter(|((_, k), v)| may_read(policy, table, k, v))
            .map(|((_, k), v)| (k.as_slice(), v))
            .collect())
    }

    /// Primary keys whose index key equals `key` at `snap` and whose visible row is authorized.
    /// A candidate referencing an absent row is corruption. Authorization does not repair stale
    /// index predicates; the unfiltered scrubber remains the independent consistency oracle.
    pub fn lookup_at(
        &self,
        snap: Snapshot,
        index: u32,
        key: &[u8],
        policy: &(impl PolicyEvaluator + ?Sized),
    ) -> Result<Vec<Vec<u8>>, RowError> {
        self.check_snapshot(snap)?;
        let Some(table) = self.indexes.table_for(snap, index) else {
            return Ok(Vec::new());
        };
        let candidates = self.indexes.lookup(snap, index, key).map_err(mvcc_error)?;
        let mut allowed = Vec::new();
        for primary in candidates {
            let value = self
                .table
                .get(snap, &(table, primary.clone()))
                .map_err(mvcc_error)?;
            let value =
                value.ok_or_else(|| RowError::Corrupt("index references an absent row".into()))?;
            // The index scrubber, not authorization, checks whether the extracted key is correct.
            // Rechecking that predicate here would conceal stale-entry defects from callers.
            if may_read(policy, table, &primary, value) {
                allowed.push(primary);
            }
        }
        Ok(allowed)
    }

    /// Rebuilds expected index entries from the rows at `snap` and reports any difference (I9).
    pub fn check_indexes_at(&self, snap: Snapshot) -> Result<(), String> {
        self.check_snapshot(snap).map_err(|e| e.to_string())?;
        let live = self
            .indexes
            .live_entries(snap)
            .map_err(|e| format!("{e:?}"))?;
        let definitions = self.catalog.indexes_as_of(snap.as_of);
        let expected = self
            .indexes
            .expected_entries_catalog(&self.table, snap, Some(&definitions))
            .map_err(|e| format!("{e:?}"))?;
        if live == expected {
            Ok(())
        } else {
            Err(format!(
                "indexes differ from the rows: {} stale or extra, {} missing",
                live.iter()
                    .filter(|(key, value)| expected.get(*key) != Some(*value))
                    .count(),
                expected
                    .iter()
                    .filter(|(key, value)| live.get(*key) != Some(*value))
                    .count()
            ))
        }
    }

    /// Checks index consistency at the current snapshot (I9).
    pub fn check_indexes(&self) -> Result<(), String> {
        self.check_indexes_at(self.snapshot())
    }

    /// The oldest snapshot that is still complete. Reads below it fail with `SnapshotTooOld`;
    /// a reopened store has every version again because the log keeps all history.
    pub fn oldest_snapshot(&self) -> Snapshot {
        Snapshot {
            as_of: self.table.floor(),
        }
    }

    /// Fails with `SnapshotTooOld` if `snap` is below the vacuum floor. Reads already enforce this;
    /// callers can use the same check to reject an expired `AS OF` before starting other work.
    pub fn check_snapshot(&self, snap: Snapshot) -> Result<(), RowError> {
        self.table.check_readable(snap).map_err(|e| match e {
            MvccError::SnapshotTooOld { floor } => RowError::SnapshotTooOld { floor },
            _ => RowError::Stale,
        })
    }

    /// The horizon to vacuum to: no newer than the oldest snapshot an open transaction still reads
    /// (`oldest_open`), and keeping the last `retain_lsns` commits readable for `AS OF`. Pass the
    /// result to [`RowStore::vacuum`].
    pub fn vacuum_horizon(&self, oldest_open: Option<Snapshot>, retain_lsns: u64) -> u64 {
        let by_retention = self.applied_lsn().saturating_sub(retain_lsns);
        oldest_open.map_or(by_retention, |s| s.as_of.min(by_retention))
    }

    /// Removes row and index versions that no snapshot at or after `horizon` can see. The newest
    /// version visible at the horizon is always kept, so every read at a snapshot from `horizon` on
    /// returns what it returned before. The log is untouched, which is why recovery followed by a
    /// vacuum gives the same state. Open transactions below the horizon are refused at commit with
    /// `SnapshotTooOld`; compute the horizon with [`RowStore::vacuum_horizon`] to avoid that.
    pub fn vacuum(&mut self, horizon: u64) -> VacuumStats {
        let row_versions = self.table.vacuum(horizon);
        let applied_horizon = self.table.floor();
        let index_versions = self.indexes.vacuum(applied_horizon);
        self.commit_lsn.retain(|_, lsn| *lsn >= applied_horizon);
        let before_floor = self
            .commit_times
            .partition_point(|(_, lsn, _)| *lsn < applied_horizon);
        if before_floor != 0 {
            self.commit_times.drain(..before_floor);
            self.commit_times.shrink_to_fit();
        }
        VacuumStats {
            horizon: applied_horizon,
            row_versions,
            index_versions,
        }
    }

    /// Vacuums once retained row versions reach `version_threshold`; returns `None` below it.
    ///
    /// The threshold includes tombstones and is based on rows, not the number of index entries.
    /// The horizon respects the oldest open snapshot and the retention window. This is an explicit
    /// maintenance call, not a background worker; zero requests a vacuum even for an empty store.
    pub fn vacuum_if_needed(
        &mut self,
        oldest_open: Option<Snapshot>,
        retain_lsns: u64,
        version_threshold: usize,
    ) -> Option<VacuumStats> {
        if self.table.version_count() < version_threshold {
            return None;
        }
        Some(self.vacuum(self.vacuum_horizon(oldest_open, retain_lsns)))
    }

    /// `AS OF CHANGESET c`: the snapshot that sees everything up to and including change set `id`, or
    /// `None` if no row change set with that id is retained at or above the vacuum floor.
    pub fn snapshot_as_of_changeset(&self, id: ChangeSetId) -> Option<Snapshot> {
        self.commit_lsn
            .get(&id)
            .filter(|lsn| **lsn >= self.table.floor())
            .map(|lsn| Snapshot { as_of: *lsn })
    }

    /// `AS OF TIMESTAMP t`: the snapshot at the newest row change set whose timestamp is not after
    /// `t`, clamped to the vacuum floor when that history has expired. Before any vacuum it is
    /// the empty snapshot if `t` is before the first commit.
    pub fn snapshot_as_of_hlc(&self, t: Hlc) -> Snapshot {
        let end = self.commit_times.partition_point(|(h, _, _)| *h <= t);
        let as_of = end
            .checked_sub(1)
            .and_then(|i| self.commit_times.get(i))
            .map_or(0, |(_, lsn, _)| *lsn);
        Snapshot {
            as_of: as_of.max(self.table.floor()),
        }
    }

    /// The id of the row change set committed at `lsn`, if retained at or above the vacuum floor.
    pub fn changeset_id_at(&self, lsn: u64) -> Option<ChangeSetId> {
        let i = self
            .commit_times
            .binary_search_by_key(&lsn, |(_, l, _)| *l)
            .ok()?;
        self.commit_times.get(i).map(|(_, _, id)| *id)
    }

    /// The timestamp of the row change set at `lsn`, if retained at or above the vacuum floor.
    pub fn commit_time_at(&self, lsn: u64) -> Option<Hlc> {
        let i = self
            .commit_times
            .binary_search_by_key(&lsn, |(_, l, _)| *l)
            .ok()?;
        self.commit_times.get(i).map(|(h, _, _)| *h)
    }

    /// The commit LSN of the newest change set applied.
    pub fn applied_lsn(&self) -> u64 {
        self.table.applied_lsn()
    }

    /// Commits a transaction as one change set and returns its commit LSN (the snapshot LSN for a
    /// transaction that wrote nothing). Stale/floor/conflict checks precede authorization; every
    /// final transition is then checked under the bound actor policy before allocating an id or
    /// appending anything. One denied transition rejects the entire batch.
    pub fn commit<P: PolicyEvaluator>(&mut self, tx: RowTx<P>) -> Result<u64, RowError> {
        if self.stale {
            return Err(RowError::Stale);
        }
        self.check_snapshot(tx.snapshot())?;
        if tx.is_read_only() {
            return Ok(tx.snapshot().as_of);
        }
        if !bugs::active(Bug::RowConflictCheckSkipped) {
            tx.inner.check(&self.table).map_err(|e| match e {
                MvccError::Conflict { newer_lsn } => RowError::Conflict { newer_lsn },
                MvccError::SnapshotTooOld { floor } => RowError::SnapshotTooOld { floor },
                MvccError::OutOfOrder { .. } => RowError::Stale,
            })?;
        }
        let snapshot = tx.snapshot();
        let RowTx { inner, policy } = tx;
        let writes = inner.into_writes();
        if !bugs::active(Bug::RowPolicyWriteSkipped) {
            for ((table, key), after) in &writes {
                let before = self
                    .table
                    .get(snapshot, &(*table, key.clone()))
                    .map_err(mvcc_error)?;
                authorize_write(
                    &policy,
                    RowWriteRef {
                        table: *table,
                        key,
                        before: before.map(Vec::as_slice),
                        after: after.as_deref(),
                    },
                )
                .map_err(|_| RowError::PermissionDenied)?;
            }
        }
        let payload = rowops::encode(&writes).map_err(RowError::Encode)?;
        let id = self.kernel.new_id();
        match self.kernel.commit(ChangeSet::new(id, payload)) {
            Ok(Receipt::Recorded { lsn, hlc }) => {
                let writes: Vec<_> = if bugs::active(Bug::RowPartialApply) {
                    writes.into_iter().take(1).collect()
                } else {
                    writes
                };
                let changes = self
                    .indexes
                    .changes_for(&self.table, &writes)
                    .map_err(mvcc_error)?;
                self.table
                    .apply(lsn, id, writes)
                    .and_then(|()| self.indexes.apply(lsn, id, changes))
                    .map_err(|e| RowError::Corrupt(format!("apply after commit: {e:?}")))?;
                self.commit_lsn.insert(id, lsn);
                self.commit_times.push((hlc, lsn, id));
                Ok(lsn)
            }
            Ok(Receipt::Duplicate) => {
                self.stale = true;
                Err(RowError::Corrupt(
                    "a fresh change set id was a duplicate".into(),
                ))
            }
            Err(e) if e.class() == ErrorClass::Rejected => Err(RowError::Rejected(e)),
            Err(e) => {
                // It may or may not have reached the disk; only the log knows.
                self.stale = true;
                Err(RowError::InDoubt(e))
            }
        }
    }
}

#[cfg(test)]
mod vacuum_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
    use lattice_testkit::scenario::{SharedClock, SharedIo};
    use std::rc::Rc;

    type Store = RowStore<SharedIo, SharedClock, SeededRng>;

    fn open(disk: &SharedIo, clock: &Rc<SimClock>) -> Store {
        RowStore::open_with_indexes(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(17),
            &Config {
                segment_limit: 256,
                ..Config::default()
            },
            vec![IndexDef {
                id: 9,
                table: 1,
                extract: |v| Some(v.to_vec()),
            }],
        )
        .unwrap()
    }

    fn fixture() -> (SharedIo, Rc<SimClock>, Store) {
        let disk = SharedIo::new(SimIo::new(17, FaultProfile::clean()));
        let clock = Rc::new(SimClock::new(1_000));
        let mut store = open(&disk, &clock);
        for (a, other_key, other) in [
            (1, b"b", Some(9)),
            (2, b"b", None),
            (3, b"c", Some(7)),
            (4, b"c", None),
        ] {
            clock.advance(1);
            let mut tx = store.begin();
            tx.put((1, b"a".to_vec()), vec![a]);
            match other {
                Some(value) => tx.put((1, other_key.to_vec()), vec![value]),
                None => tx.delete((1, other_key.to_vec())),
            }
            store.commit(tx).unwrap();
        }
        (disk, clock, store)
    }

    #[test]
    fn vacuum_prunes_commit_metadata_and_timestamp_snapshots_stop_at_the_floor() {
        let (_, _, mut store) = fixture();
        let commits = store.commit_times.clone();
        let stats = store.vacuum(3);
        assert_eq!(stats.horizon, 3);
        assert_eq!(store.commit_times.len(), 2);
        assert_eq!(store.commit_lsn.len(), 2);
        for (_, _, id) in &commits {
            assert!(
                store.kernel.contains(id),
                "vacuum must not prune kernel deduplication state"
            );
        }
        assert_eq!(store.commit_time_at(1), None);
        assert_eq!(store.changeset_id_at(2), None);
        assert_eq!(store.snapshot_as_of_changeset(commits[0].2), None);
        assert_eq!(store.commit_time_at(3), Some(commits[2].0));
        assert_eq!(store.changeset_id_at(4), Some(commits[3].2));
        assert_eq!(
            store.snapshot_as_of_hlc(Hlc::default()),
            Snapshot { as_of: 3 }
        );
        assert_eq!(
            store.snapshot_as_of_hlc(commits[0].0),
            Snapshot { as_of: 3 }
        );
        assert_eq!(
            store.snapshot_as_of_hlc(commits[2].0),
            Snapshot { as_of: 3 }
        );
        assert_eq!(
            store.snapshot_as_of_hlc(commits[3].0),
            Snapshot { as_of: 4 }
        );
        assert_eq!(
            store.get_at(Snapshot { as_of: 3 }, 1, b"a", &AllowAll),
            Ok(Some(&vec![3]))
        );
        store.check_indexes_at(Snapshot { as_of: 3 }).unwrap();
        let repeat = store.vacuum(1);
        assert_eq!(repeat.horizon, 3);
        assert_eq!(repeat.row_versions, 0);
        assert_eq!(store.commit_times.len(), 2);
    }

    #[test]
    fn all_store_read_paths_reject_expired_snapshots_and_transactions() {
        let disk = SharedIo::new(SimIo::new(2, FaultProfile::clean()));
        let clock = Rc::new(SimClock::new(1_000));
        let mut store = open(&disk, &clock);
        let old = store.snapshot();
        let read_only = store.begin();
        let mut own_write = store.begin();
        own_write.put((1, b"staged".to_vec()), vec![8]);
        let mut committed = store.begin();
        committed.put((1, b"a".to_vec()), vec![3]);
        store.commit(committed).unwrap();
        store.vacuum(1);
        let expired = Err(RowError::SnapshotTooOld { floor: 1 });
        assert_eq!(store.get_at(old, 1, b"a", &AllowAll), expired);
        assert_eq!(store.get_at(old, 1, b"missing", &AllowAll), expired);
        assert_eq!(store.get(&own_write, 1, b"staged"), expired);
        assert_eq!(
            store.scan_at(old, 1, .., &AllowAll),
            Err(RowError::SnapshotTooOld { floor: 1 })
        );
        assert_eq!(
            store.lookup_at(old, 9, &[3], &AllowAll),
            Err(RowError::SnapshotTooOld { floor: 1 })
        );
        assert_eq!(
            store.indexes.lookup(old, 9, &[3]),
            Err(MvccError::SnapshotTooOld { floor: 1 })
        );
        assert_eq!(
            store.indexes.live_entries(old),
            Err(MvccError::SnapshotTooOld { floor: 1 })
        );
        assert_eq!(
            store.indexes.expected_entries(&store.table, old),
            Err(MvccError::SnapshotTooOld { floor: 1 })
        );
        assert!(
            store
                .check_indexes_at(old)
                .unwrap_err()
                .contains("snapshot too old")
        );
        assert_eq!(
            store.commit(read_only),
            Err(RowError::SnapshotTooOld { floor: 1 })
        );
        assert_eq!(
            store.commit(own_write),
            Err(RowError::SnapshotTooOld { floor: 1 })
        );
    }

    #[test]
    fn threshold_vacuum_respects_open_snapshots_and_retention() {
        let (_, _, mut store) = fixture();
        assert_eq!(store.table.version_count(), 8);
        assert_eq!(store.vacuum_if_needed(None, 0, 9), None);
        assert_eq!(store.oldest_snapshot().as_of, 0);
        let stats = store
            .vacuum_if_needed(Some(Snapshot { as_of: 2 }), 1, 8)
            .unwrap();
        assert_eq!(stats.horizon, 2);
        assert_eq!(
            store.get_at(Snapshot { as_of: 2 }, 1, b"a", &AllowAll),
            Ok(Some(&vec![2]))
        );
        assert_eq!(store.vacuum_if_needed(None, 0, usize::MAX), None);
        let stats = store.vacuum_if_needed(None, 0, 0).unwrap();
        assert_eq!(stats.horizon, 4);
        assert_eq!(store.table.version_count(), 1);
        assert_eq!(store.commit_times.len(), 1);
        assert_eq!(store.commit_lsn.len(), 1);
    }

    #[test]
    fn replay_then_vacuum_reproduces_live_rows_indexes_and_metadata_without_changing_wal() {
        let (disk, clock, mut live) = fixture();
        let segments: Vec<_> = disk
            .list("wal")
            .unwrap()
            .into_iter()
            .filter(|p| p.ends_with(".seg"))
            .map(|p| {
                let bytes = disk.read(&format!("wal/{p}")).unwrap();
                (p, bytes)
            })
            .collect();
        live.vacuum(2);
        live.vacuum(3);
        let times = live.commit_times.clone();
        let ids = live.commit_lsn.clone();
        let rows: Vec<_> = (3..=4)
            .map(|as_of| {
                let snap = Snapshot { as_of };
                live.check_indexes_at(snap).unwrap();
                live.scan_at(snap, 1, .., &AllowAll)
                    .unwrap()
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        drop(live);
        disk.crash();
        let mut replayed = open(&disk, &clock);
        assert_eq!(replayed.oldest_snapshot().as_of, 0);
        assert_eq!(replayed.commit_times.len(), 4);
        replayed.vacuum(3);
        assert_eq!(replayed.commit_times, times);
        assert_eq!(replayed.commit_lsn, ids);
        for (as_of, expected) in (3..=4).zip(rows) {
            let snap = Snapshot { as_of };
            replayed.check_indexes_at(snap).unwrap();
            let got: Vec<_> = replayed
                .scan_at(snap, 1, .., &AllowAll)
                .unwrap()
                .into_iter()
                .map(|(k, v)| (k.to_vec(), v.clone()))
                .collect();
            assert_eq!(got, expected);
        }
        for (name, before) in segments {
            assert_eq!(disk.read(&format!("wal/{name}")).unwrap(), before);
        }
    }
}

// Keep these public-API policy workloads in the library test target so Miri and ASan run them too.
#[cfg(test)]
#[path = "store_policy_tests.rs"]
mod policy_tests;
