//! A row store on top of the kernel: rows live in memory as versions, and the log is the only
//! durable copy.
//!
//! A commit is one change set. The conflict rule is checked first (first committer wins, see
//! `formats/isolation.md`), the change set goes through the kernel's commit path, and only when the
//! kernel acknowledges it durable are its writes applied to the in-memory versions at the commit LSN,
//! in one call, so every reader sees all of the change set or none of it. Recovery replays the log's
//! row change sets through the same apply, so a restart reproduces exactly the acknowledged state.

use crate::index::{IndexDef, Indexes};
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

/// A transaction on the store.
pub type RowTx = Tx<RowKey, Vec<u8>>;

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
            RowError::Encode(e) => write!(f, "cannot encode writes: {e:?}"),
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
            RowError::Encode(_) | RowError::InvalidIndexes(_) => ErrorClass::Rejected,
        }
    }
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
    /// Commit LSN of every row change set by id, and its timestamp in log order (the HLC grows with
    /// the LSN), so `AS OF` can name a point in history by change set or by time.
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
    /// deterministic and use distinct ids, and must be supplied again on every reopen.
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
        let mut table: Table<RowKey, Vec<u8>> = Table::new();
        let mut indexes = Indexes::new(index_defs);
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
            if !rowops::is_row_payload(&cs.payload) {
                return; // another engine's change set
            }
            match rowops::decode(&cs.payload) {
                Ok(mut writes) => {
                    if bugs::active(Bug::RowRecoveryDropsDeletes) {
                        writes.retain(|(_, value)| value.is_some());
                    }
                    let changes = indexes.changes_for(&table, &writes);
                    if let Err(e) = table
                        .apply(r.lsn, cs.id, writes)
                        .and_then(|()| indexes.apply(r.lsn, cs.id, changes))
                    {
                        failure = Some(RowError::Corrupt(format!("lsn {}: {e:?}", r.lsn)));
                    } else {
                        commit_lsn.insert(cs.id, r.lsn);
                        commit_times.push((cs.hlc, r.lsn, cs.id));
                    }
                }
                Err(e) => {
                    failure = Some(RowError::Corrupt(format!(
                        "row writes at lsn {}: {e:?}",
                        r.lsn
                    )));
                }
            }
        })
        .map_err(RowError::Rejected)?;
        match failure {
            Some(e) => Err(e),
            None => Ok(Self {
                kernel,
                table,
                indexes,
                commit_lsn,
                commit_times,
                stale: false,
            }),
        }
    }

    /// Starts a transaction on the newest snapshot.
    pub fn begin(&self) -> RowTx {
        Tx::begin(&self.table)
    }

    /// The newest snapshot: everything committed so far.
    pub fn snapshot(&self) -> Snapshot {
        self.table.snapshot()
    }

    /// Reads a row inside a transaction: its own writes first, then its snapshot.
    pub fn get<'a>(&'a self, tx: &'a RowTx, table: u32, key: &[u8]) -> Option<&'a Vec<u8>> {
        tx.get(&self.table, &(table, key.to_vec()))
    }

    /// Reads a row as of a snapshot.
    pub fn get_at(&self, snap: Snapshot, table: u32, key: &[u8]) -> Option<&Vec<u8>> {
        self.table.get(snap, &(table, key.to_vec()))
    }

    /// Every live row of `table` with a key in `range` as of `snap`, in key order.
    pub fn scan_at<B: RangeBounds<Vec<u8>>>(
        &self,
        snap: Snapshot,
        table: u32,
        range: B,
    ) -> Vec<(&[u8], &Vec<u8>)> {
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
        self.table
            .range(snap, (lo, hi))
            .into_iter()
            .map(|((_, k), v)| (k.as_slice(), v))
            .collect()
    }

    /// Primary keys whose extracted index key equals `key` at `snap`, in primary-key order.
    pub fn lookup_at(&self, snap: Snapshot, index: u32, key: &[u8]) -> Vec<Vec<u8>> {
        self.indexes.lookup(snap, index, key)
    }

    /// Rebuilds expected index entries from the rows at `snap` and reports any difference (I9).
    pub fn check_indexes_at(&self, snap: Snapshot) -> Result<(), String> {
        let live = self.indexes.live_entries(snap);
        let expected = self.indexes.expected_entries(&self.table, snap);
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

    /// The oldest snapshot that is still complete. Reads below it can miss versions that vacuum
    /// removed; a reopened store has every version again (the log keeps all history).
    pub fn oldest_snapshot(&self) -> Snapshot {
        Snapshot {
            as_of: self.table.floor(),
        }
    }

    /// Fails with `SnapshotTooOld` if `snap` is below the vacuum floor. Call it before trusting a
    /// read at a snapshot that may have outlived a vacuum, such as an `AS OF` outside the window.
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
        VacuumStats {
            horizon: applied_horizon,
            row_versions,
            index_versions,
        }
    }

    /// `AS OF CHANGESET c`: the snapshot that sees everything up to and including change set `id`, or
    /// `None` if no row change set with that id was committed.
    pub fn snapshot_as_of_changeset(&self, id: ChangeSetId) -> Option<Snapshot> {
        self.commit_lsn
            .get(&id)
            .filter(|lsn| **lsn >= self.table.floor())
            .map(|lsn| Snapshot { as_of: *lsn })
    }

    /// `AS OF TIMESTAMP t`: the snapshot at the newest row change set whose timestamp is not after
    /// `t`; the empty snapshot if `t` is before the first commit.
    pub fn snapshot_as_of_hlc(&self, t: Hlc) -> Snapshot {
        let end = self.commit_times.partition_point(|(h, _, _)| *h <= t);
        let as_of = end
            .checked_sub(1)
            .and_then(|i| self.commit_times.get(i))
            .map_or(0, |(_, lsn, _)| *lsn);
        Snapshot { as_of }
    }

    /// The id of the row change set committed at `lsn`, if there is one.
    pub fn changeset_id_at(&self, lsn: u64) -> Option<ChangeSetId> {
        let i = self
            .commit_times
            .binary_search_by_key(&lsn, |(_, l, _)| *l)
            .ok()?;
        self.commit_times.get(i).map(|(_, _, id)| *id)
    }

    /// The timestamp of the row change set committed at `lsn`, if there is one.
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
    /// transaction that wrote nothing).
    pub fn commit(&mut self, tx: RowTx) -> Result<u64, RowError> {
        if self.stale {
            return Err(RowError::Stale);
        }
        if tx.is_read_only() {
            return Ok(tx.snapshot().as_of);
        }
        if !bugs::active(Bug::RowConflictCheckSkipped) {
            tx.check(&self.table).map_err(|e| match e {
                MvccError::Conflict { newer_lsn } => RowError::Conflict { newer_lsn },
                MvccError::SnapshotTooOld { floor } => RowError::SnapshotTooOld { floor },
                MvccError::OutOfOrder { .. } => RowError::Stale,
            })?;
        }
        let writes = tx.into_writes();
        let payload = rowops::encode(&writes).map_err(RowError::Encode)?;
        let id = self.kernel.new_id();
        match self.kernel.commit(ChangeSet::new(id, payload)) {
            Ok(Receipt::Recorded { lsn, hlc }) => {
                let writes: Vec<_> = if bugs::active(Bug::RowPartialApply) {
                    writes.into_iter().take(1).collect()
                } else {
                    writes
                };
                let changes = self.indexes.changes_for(&self.table, &writes);
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
