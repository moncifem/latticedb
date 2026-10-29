//! Multi-version rows: a version is stamped with the LSN and id of the change set that wrote it.
//!
//! A change set is applied to a [`Table`] in one call at one LSN, so it is visible to a [`Snapshot`]
//! entirely or not at all (I3). A snapshot is a plain value naming the last applied LSN; reading with
//! it never changes the table, and later applies never change what it sees. Transactions use
//! snapshot isolation with first-committer-wins write conflicts (see `formats/isolation.md`).
//!
//! This is the in-memory core. Persistence, indexes and SQL sit on top of it in later work packages.

use lattice_core::ChangeSetId;
use lattice_core::bugs::{self, Bug};
use std::collections::BTreeMap;
use std::ops::RangeBounds;

/// A consistent read point: sees every change set applied at or before `as_of`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Snapshot {
    /// The last LSN this snapshot sees.
    pub as_of: u64,
}

/// Why a row read or write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MvccError {
    /// Change sets must be applied in increasing LSN order.
    OutOfOrder {
        /// The LSN that was offered.
        lsn: u64,
        /// The last LSN already applied.
        applied: u64,
    },
    /// Another change set wrote one of the same rows after this transaction's snapshot.
    Conflict {
        /// The LSN of the newer write.
        newer_lsn: u64,
    },
    /// The snapshot is older than the vacuum floor: versions it needs may be gone.
    SnapshotTooOld {
        /// The oldest snapshot that can still be read.
        floor: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version<V> {
    lsn: u64,
    cs: ChangeSetId,
    /// `None` is a deletion.
    value: Option<V>,
}

/// A keyed table with every retained version of every row.
#[derive(Debug, Clone)]
pub struct Table<K: Ord, V> {
    rows: BTreeMap<K, Vec<Version<V>>>,
    applied: u64,
    /// Vacuum horizon: snapshots below it may have lost versions.
    floor: u64,
}

impl<K: Ord, V> Default for Table<K, V> {
    fn default() -> Self {
        Self {
            rows: BTreeMap::new(),
            applied: 0,
            floor: 0,
        }
    }
}

impl<K: Ord + Clone, V: Clone> Table<K, V> {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last LSN applied.
    pub fn applied_lsn(&self) -> u64 {
        self.applied
    }

    /// Number of retained row versions, including tombstones. Counting visits each distinct key.
    pub fn version_count(&self) -> usize {
        self.rows.values().map(Vec::len).sum()
    }

    /// A snapshot of everything applied so far.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            as_of: self.applied,
        }
    }

    /// Applies one change set's writes atomically. `None` deletes the row. If a key appears more
    /// than once the last write wins. On error nothing is changed.
    pub fn apply(
        &mut self,
        lsn: u64,
        cs: ChangeSetId,
        writes: Vec<(K, Option<V>)>,
    ) -> Result<(), MvccError> {
        if lsn <= self.applied {
            return Err(MvccError::OutOfOrder {
                lsn,
                applied: self.applied,
            });
        }
        for (key, value) in writes {
            let versions = self.rows.entry(key).or_default();
            if bugs::active(Bug::RowHistoryOverwritten) {
                versions.clear();
            }
            match versions.last_mut() {
                Some(last) if last.lsn == lsn => last.value = value,
                _ => versions.push(Version { lsn, cs, value }),
            }
        }
        self.applied = lsn;
        Ok(())
    }

    /// The oldest snapshot that is still complete: nothing at or after it has been vacuumed.
    pub fn floor(&self) -> u64 {
        self.floor
    }

    /// Fails with `SnapshotTooOld` if vacuum may have removed versions `snap` needs.
    pub fn check_readable(&self, snap: Snapshot) -> Result<(), MvccError> {
        if snap.as_of < self.floor {
            Err(MvccError::SnapshotTooOld { floor: self.floor })
        } else {
            Ok(())
        }
    }

    /// Drops the versions no snapshot at or after `horizon` can see and returns how many went.
    ///
    /// For every key the newest version at or before the horizon stays (it is what a snapshot at the
    /// horizon reads), as does everything newer. A deletion that is the newest version at or before
    /// the horizon is dropped too, because every reader then sees the key as absent, with or without
    /// it. The horizon is clamped to the last applied LSN, and it never moves back. Reads at a
    /// snapshot below the new floor fail with `SnapshotTooOld`.
    pub fn vacuum(&mut self, horizon: u64) -> usize {
        let horizon = horizon.min(self.applied).max(self.floor);
        self.floor = horizon;
        let mut removed = 0;
        self.rows.retain(|_, versions| {
            let end = versions.partition_point(|v| v.lsn <= horizon);
            let Some(base) = end.checked_sub(1) else {
                return true;
            };
            let base_is_deletion = versions.get(base).is_some_and(|v| v.value.is_none());
            let keep_from = if base_is_deletion || bugs::active(Bug::VacuumDropsVisibleVersion) {
                end
            } else {
                base
            };
            removed += keep_from;
            versions.drain(..keep_from);
            !versions.is_empty()
        });
        removed
    }

    /// Every version as `(key, lsn, value)`, for comparing two tables in tests.
    #[cfg(test)]
    fn dump(&self) -> Vec<(K, u64, Option<V>)> {
        self.rows
            .iter()
            .flat_map(|(k, vs)| vs.iter().map(|v| (k.clone(), v.lsn, v.value.clone())))
            .collect()
    }

    fn visible(versions: &[Version<V>], snap: Snapshot) -> Option<&Version<V>> {
        if bugs::active(Bug::RowTombstoneSkipped) {
            return versions
                .iter()
                .rev()
                .find(|v| v.lsn <= snap.as_of && v.value.is_some());
        }
        // Versions are in increasing LSN order; the last one at or before the snapshot wins.
        let end = versions.partition_point(|v| v.lsn <= snap.as_of);
        end.checked_sub(1).and_then(|i| versions.get(i))
    }

    /// The value of `key` as of `snap`, or `None` if it did not exist or was deleted.
    /// Snapshots below the vacuum floor fail, including reads of absent keys.
    pub fn get(&self, snap: Snapshot, key: &K) -> Result<Option<&V>, MvccError> {
        self.check_readable(snap)?;
        Ok(self
            .rows
            .get(key)
            .and_then(|versions| Self::visible(versions, snap))
            .and_then(|v| v.value.as_ref()))
    }

    /// The change set that wrote the version of `key` visible at `snap`, even if it was a deletion.
    /// Snapshots below the vacuum floor fail.
    pub fn written_by(&self, snap: Snapshot, key: &K) -> Result<Option<ChangeSetId>, MvccError> {
        self.check_readable(snap)?;
        Ok(self
            .rows
            .get(key)
            .and_then(|versions| Self::visible(versions, snap))
            .map(|v| v.cs))
    }

    /// Every live row with a key in `range` as of `snap`, in key order.
    /// Snapshots below the vacuum floor fail, even for an empty range.
    pub fn range<R: RangeBounds<K>>(
        &self,
        snap: Snapshot,
        range: R,
    ) -> Result<Vec<(&K, &V)>, MvccError> {
        self.check_readable(snap)?;
        Ok(self
            .rows
            .range(range)
            .filter_map(|(k, versions)| {
                Self::visible(versions, snap)
                    .and_then(|v| v.value.as_ref())
                    .map(|v| (k, v))
            })
            .collect())
    }

    /// The LSN of the newest write to `key`, if any.
    pub fn newest_lsn(&self, key: &K) -> Option<u64> {
        self.rows.get(key).and_then(|v| v.last()).map(|v| v.lsn)
    }
}

/// A snapshot-isolation transaction: reads see its snapshot plus its own writes.
#[derive(Debug, Clone)]
pub struct Tx<K: Ord, V> {
    snapshot: Snapshot,
    writes: BTreeMap<K, Option<V>>,
}

impl<K: Ord + Clone, V: Clone> Tx<K, V> {
    /// Starts a transaction on the table's current snapshot.
    pub fn begin(table: &Table<K, V>) -> Self {
        Self {
            snapshot: table.snapshot(),
            writes: BTreeMap::new(),
        }
    }

    /// The snapshot this transaction reads from.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
    }

    /// Reads a row, seeing this transaction's own writes first. An expired transaction fails even
    /// when it has its own buffered value for the key.
    pub fn get<'a>(&'a self, table: &'a Table<K, V>, key: &K) -> Result<Option<&'a V>, MvccError> {
        table.check_readable(self.snapshot)?;
        if bugs::active(Bug::RowReadOwnWriteIgnored) {
            return table.get(self.snapshot, key);
        }
        match self.writes.get(key) {
            Some(own) => Ok(own.as_ref()),
            None => table.get(self.snapshot, key),
        }
    }

    /// Buffers a write.
    pub fn put(&mut self, key: K, value: V) {
        self.writes.insert(key, Some(value));
    }

    /// Buffers a deletion.
    pub fn delete(&mut self, key: K) {
        self.writes.insert(key, None);
    }

    /// Checks the conflict rule without committing: fails if another change set wrote any row this
    /// transaction wrote after its snapshot (first committer wins).
    pub fn check(&self, table: &Table<K, V>) -> Result<(), MvccError> {
        table.check_readable(self.snapshot)?;
        for key in self.writes.keys() {
            if let Some(newer) = table.newest_lsn(key)
                && newer > self.snapshot.as_of
            {
                return Err(MvccError::Conflict { newer_lsn: newer });
            }
        }
        Ok(())
    }

    /// The buffered writes in key order; `None` is a deletion.
    pub fn into_writes(self) -> Vec<(K, Option<V>)> {
        self.writes.into_iter().collect()
    }

    /// Whether the transaction has written nothing.
    pub fn is_read_only(&self) -> bool {
        self.writes.is_empty()
    }

    /// Commits at `lsn` for change set `cs`. Fails, changing nothing, if another change set wrote
    /// any row this transaction wrote after its snapshot (first committer wins).
    pub fn commit(
        self,
        table: &mut Table<K, V>,
        lsn: u64,
        cs: ChangeSetId,
    ) -> Result<(), MvccError> {
        self.check(table)?;
        table.apply(lsn, cs, self.writes.into_iter().collect())
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
    use proptest::prelude::*;

    fn cs(n: u8) -> ChangeSetId {
        ChangeSetId::from_parts(u64::from(n), 0, 0)
    }

    #[test]
    fn a_snapshot_keeps_seeing_the_past() {
        let mut t: Table<u8, &str> = Table::new();
        t.apply(1, cs(1), vec![(1, Some("a"))]).unwrap();
        let old = t.snapshot();
        t.apply(2, cs(2), vec![(1, Some("b")), (2, Some("c"))])
            .unwrap();
        t.apply(3, cs(3), vec![(1, None)]).unwrap();
        assert_eq!(t.get(old, &1).unwrap(), Some(&"a"));
        assert_eq!(t.get(old, &2).unwrap(), None);
        assert_eq!(t.get(t.snapshot(), &1).unwrap(), None);
        assert_eq!(t.get(t.snapshot(), &2).unwrap(), Some(&"c"));
        assert_eq!(t.written_by(old, &1).unwrap(), Some(cs(1)));
        assert_eq!(
            t.written_by(t.snapshot(), &1).unwrap(),
            Some(cs(3)),
            "a deletion is a version too"
        );
    }

    #[test]
    fn inv_i3_a_snapshot_sees_a_change_set_entirely_or_not_at_all() {
        let mut t: Table<u8, u8> = Table::new();
        for lsn in 1..=20u64 {
            let writes = (0..5u8).map(|k| (k, Some(lsn as u8))).collect();
            t.apply(lsn, cs(lsn as u8), writes).unwrap();
        }
        for as_of in 0..=20u64 {
            let snap = Snapshot { as_of };
            let seen: Vec<Option<u8>> = (0..5u8)
                .map(|k| t.get(snap, &k).unwrap().copied())
                .collect();
            assert!(
                seen.iter().all(|v| *v == seen[0]),
                "snapshot {as_of} saw a partial change set: {seen:?}"
            );
            assert_eq!(seen[0], if as_of == 0 { None } else { Some(as_of as u8) });
        }
    }

    #[test]
    fn out_of_order_and_repeated_lsns_are_refused_without_change() {
        let mut t: Table<u8, u8> = Table::new();
        t.apply(5, cs(1), vec![(1, Some(1))]).unwrap();
        for lsn in [5, 4, 0] {
            assert_eq!(
                t.apply(lsn, cs(2), vec![(1, Some(9))]),
                Err(MvccError::OutOfOrder { lsn, applied: 5 })
            );
        }
        assert_eq!(t.get(t.snapshot(), &1).unwrap(), Some(&1));
        assert_eq!(t.applied_lsn(), 5);
    }

    #[test]
    fn the_last_write_to_a_key_in_one_change_set_wins() {
        let mut t: Table<u8, u8> = Table::new();
        t.apply(
            1,
            cs(1),
            vec![(1, Some(1)), (1, Some(2)), (1, None), (1, Some(3))],
        )
        .unwrap();
        assert_eq!(t.get(t.snapshot(), &1).unwrap(), Some(&3));
    }

    #[test]
    fn first_committer_wins_and_disjoint_writers_both_commit() {
        let mut t: Table<u8, u8> = Table::new();
        t.apply(1, cs(1), vec![(1, Some(0)), (2, Some(0))]).unwrap();
        let mut a = Tx::begin(&t);
        let mut b = Tx::begin(&t);
        let mut c = Tx::begin(&t);
        a.put(1, 10);
        b.put(1, 20);
        c.put(2, 30);
        a.commit(&mut t, 2, cs(2)).unwrap();
        assert_eq!(
            b.commit(&mut t, 3, cs(3)),
            Err(MvccError::Conflict { newer_lsn: 2 })
        );
        c.commit(&mut t, 3, cs(3)).unwrap();
        assert_eq!(t.get(t.snapshot(), &1).unwrap(), Some(&10));
        assert_eq!(t.get(t.snapshot(), &2).unwrap(), Some(&30));
    }

    #[test]
    fn write_skew_is_allowed_as_documented() {
        // Both read both rows and each writes a different one: snapshot isolation commits both.
        let mut t: Table<u8, u8> = Table::new();
        t.apply(1, cs(1), vec![(1, Some(1)), (2, Some(1))]).unwrap();
        let mut a = Tx::begin(&t);
        let mut b = Tx::begin(&t);
        assert_eq!(a.get(&t, &2).unwrap(), Some(&1));
        assert_eq!(b.get(&t, &1).unwrap(), Some(&1));
        a.put(1, 0);
        b.put(2, 0);
        a.commit(&mut t, 2, cs(2)).unwrap();
        b.commit(&mut t, 3, cs(3)).unwrap();
        assert_eq!(t.get(t.snapshot(), &1).unwrap(), Some(&0));
        assert_eq!(t.get(t.snapshot(), &2).unwrap(), Some(&0));
    }

    #[test]
    fn a_transaction_reads_its_own_writes_and_a_failed_commit_changes_nothing() {
        let mut t: Table<u8, u8> = Table::new();
        t.apply(1, cs(1), vec![(1, Some(1))]).unwrap();
        let mut tx = Tx::begin(&t);
        tx.put(1, 5);
        tx.put(2, 6);
        assert_eq!(tx.get(&t, &1).unwrap(), Some(&5));
        tx.delete(2);
        assert_eq!(tx.get(&t, &2).unwrap(), None);
        t.apply(2, cs(2), vec![(1, Some(2))]).unwrap();
        let before = t.snapshot();
        assert!(tx.commit(&mut t, 3, cs(3)).is_err());
        assert_eq!(t.snapshot(), before);
        assert_eq!(t.get(t.snapshot(), &2).unwrap(), None);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Apply(Vec<(u8, Option<u8>)>),
        Probe {
            back: u8,
            key: u8,
        },
        Scan {
            back: u8,
            lo: u8,
            hi: u8,
        },
        TxCommit {
            back: u8,
            keys: Vec<(u8, Option<u8>)>,
        },
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        let write = (0u8..6, proptest::option::of(0u8..50));
        let op = prop_oneof![
            proptest::collection::vec(write.clone(), 1..4).prop_map(Op::Apply),
            (0u8..8, 0u8..6).prop_map(|(back, key)| Op::Probe { back, key }),
            (0u8..8, 0u8..6, 0u8..7).prop_map(|(back, lo, hi)| Op::Scan { back, lo, hi }),
            (0u8..8, proptest::collection::vec(write, 1..3))
                .prop_map(|(back, keys)| Op::TxCommit { back, keys }),
        ];
        proptest::collection::vec(op, 1..60)
    }

    proptest! {
        /// The table agrees with a model that copies the whole map at every LSN: reads at any past
        /// snapshot, range scans, and the first-committer-wins rule.
        #[test]
        fn model_based_reads_and_conflicts(ops in ops()) {
            let mut table: Table<u8, u8> = Table::new();
            // history[i] is the whole map after i change sets; touched[i] the keys change set i wrote.
            let mut history: Vec<BTreeMap<u8, u8>> = vec![BTreeMap::new()];
            let mut touched: Vec<Vec<u8>> = vec![vec![]];
            let snap_back = |history: &Vec<BTreeMap<u8, u8>>, back: u8| -> usize {
                history.len() - 1 - usize::from(back).min(history.len() - 1)
            };
            for op in ops {
                match op {
                    Op::Apply(writes) => {
                        let lsn = history.len() as u64;
                        let mut next = history.last().unwrap().clone();
                        for (k, v) in &writes {
                            match v { Some(v) => { next.insert(*k, *v); } None => { next.remove(k); } }
                        }
                        table.apply(lsn, cs(lsn as u8), writes.clone()).unwrap();
                        touched.push(writes.iter().map(|(k, _)| *k).collect());
                        history.push(next);
                    }
                    Op::Probe { back, key } => {
                        let i = snap_back(&history, back);
                        let got = table.get(Snapshot { as_of: i as u64 }, &key).unwrap().copied();
                        prop_assert_eq!(got, history[i].get(&key).copied());
                    }
                    Op::Scan { back, lo, hi } => {
                        let i = snap_back(&history, back);
                        let (lo, hi) = (lo.min(hi), lo.max(hi));
                        let got: Vec<(u8, u8)> = table
                            .range(Snapshot { as_of: i as u64 }, lo..hi).unwrap()
                            .into_iter().map(|(k, v)| (*k, *v)).collect();
                        let want: Vec<(u8, u8)> = history[i].range(lo..hi).map(|(k, v)| (*k, *v)).collect();
                        prop_assert_eq!(got, want);
                    }
                    Op::TxCommit { back, keys } => {
                        let i = snap_back(&history, back);
                        // Begin a transaction on a past snapshot by building it from that point.
                        let mut tx = Tx::begin(&table);
                        tx.snapshot = Snapshot { as_of: i as u64 };
                        for (k, v) in &keys {
                            match v { Some(v) => tx.put(*k, *v), None => tx.delete(*k) }
                        }
                        let conflict = keys.iter().any(|(k, _)| touched[i + 1..].iter().any(|t| t.contains(k)));
                        let lsn = history.len() as u64;
                        let result = tx.commit(&mut table, lsn, cs(lsn as u8));
                        prop_assert_eq!(result.is_err(), conflict);
                        if result.is_ok() {
                            let mut next = history.last().unwrap().clone();
                            let mut last: BTreeMap<u8, Option<u8>> = BTreeMap::new();
                            for (k, v) in &keys { last.insert(*k, *v); }
                            for (k, v) in last {
                                match v { Some(v) => { next.insert(k, v); } None => { next.remove(&k); } }
                            }
                            touched.push(keys.iter().map(|(k, _)| *k).collect());
                            history.push(next);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn vacuum_keeps_the_version_at_the_horizon_and_everything_newer() {
        let mut t: Table<u8, &str> = Table::new();
        t.apply(1, cs(1), vec![(1, Some("a"))]).unwrap();
        t.apply(2, cs(2), vec![(1, Some("b"))]).unwrap();
        t.apply(3, cs(3), vec![(1, Some("c"))]).unwrap();
        t.apply(4, cs(4), vec![(1, Some("d"))]).unwrap();
        assert_eq!(
            t.vacuum(3),
            2,
            "versions at lsn 1 and 2 are invisible from 3 on"
        );
        for (as_of, want) in [(3, "c"), (4, "d")] {
            assert_eq!(t.get(Snapshot { as_of }, &1).unwrap(), Some(&want));
        }
        assert_eq!(
            t.written_by(Snapshot { as_of: 3 }, &1).unwrap(),
            Some(cs(3))
        );
        assert_eq!(t.vacuum(3), 0, "vacuum is idempotent");
    }

    #[test]
    fn vacuum_drops_a_deleted_row_but_not_a_row_deleted_after_the_horizon() {
        let mut t: Table<u8, u8> = Table::new();
        t.apply(1, cs(1), vec![(1, Some(1)), (2, Some(2))]).unwrap();
        t.apply(2, cs(2), vec![(1, None)]).unwrap();
        t.apply(3, cs(3), vec![(2, None)]).unwrap();
        assert_eq!(
            t.vacuum(2),
            2,
            "key 1: its insert and its tombstone; key 2 keeps lsn 1"
        );
        assert_eq!(t.get(Snapshot { as_of: 2 }, &1).unwrap(), None);
        assert_eq!(t.get(Snapshot { as_of: 2 }, &2).unwrap(), Some(&2));
        assert_eq!(t.get(Snapshot { as_of: 3 }, &2).unwrap(), None);
        assert_eq!(t.newest_lsn(&1), None);
        assert_eq!(t.newest_lsn(&2), Some(3));
    }

    #[test]
    fn vacuum_clamps_the_horizon_and_never_moves_the_floor_back() {
        let mut t: Table<u8, u8> = Table::new();
        t.apply(1, cs(1), vec![(1, Some(1))]).unwrap();
        t.apply(2, cs(2), vec![(1, Some(2))]).unwrap();
        assert_eq!(t.vacuum(99), 1);
        assert_eq!(t.floor(), 2);
        assert_eq!(t.vacuum(0), 0);
        assert_eq!(t.floor(), 2);
        assert_eq!(t.get(t.snapshot(), &1).unwrap(), Some(&2));
    }

    #[test]
    fn a_transaction_older_than_the_floor_is_refused_not_misread() {
        let mut t: Table<u8, u8> = Table::new();
        t.apply(1, cs(1), vec![(1, Some(1))]).unwrap();
        let mut old = Tx::begin(&t);
        t.apply(2, cs(2), vec![(1, Some(2))]).unwrap();
        t.apply(3, cs(3), vec![(2, Some(3))]).unwrap();
        t.vacuum(2);
        old.put(5, 5);
        assert_eq!(old.check(&t), Err(MvccError::SnapshotTooOld { floor: 2 }));
        assert_eq!(
            old.commit(&mut t, 4, cs(4)),
            Err(MvccError::SnapshotTooOld { floor: 2 })
        );
        assert_eq!(t.get(t.snapshot(), &5).unwrap(), None);
        let mut at_floor = Tx::begin(&t);
        at_floor.snapshot = Snapshot { as_of: 2 };
        at_floor.put(1, 9);
        at_floor.commit(&mut t, 4, cs(4)).unwrap();
        assert_eq!(t.get(t.snapshot(), &1).unwrap(), Some(&9));
    }

    fn history_of(steps: &[Vec<(u8, Option<u8>)>]) -> (Table<u8, u8>, Vec<BTreeMap<u8, u8>>) {
        let mut t: Table<u8, u8> = Table::new();
        let mut history = vec![BTreeMap::new()];
        for (i, writes) in steps.iter().enumerate() {
            let lsn = i as u64 + 1;
            t.apply(lsn, cs(lsn as u8), writes.clone()).unwrap();
            let mut next = history.last().unwrap().clone();
            for (k, v) in writes {
                match v {
                    Some(v) => {
                        next.insert(*k, *v);
                    }
                    None => {
                        next.remove(k);
                    }
                }
            }
            history.push(next);
        }
        (t, history)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// After a vacuum at `h` (reached in one step or two), every snapshot from `h` on reads and
        /// scans exactly what the model says, and both ways leave identical versions behind.
        #[test]
        fn vacuum_preserves_every_snapshot_at_or_after_the_horizon(
            steps in proptest::collection::vec(
                proptest::collection::vec((0u8..6, proptest::option::of(0u8..50)), 1..4),
                1..30,
            ),
            h1 in 0u64..40,
            h2 in 0u64..40,
        ) {
            let (mut t, history) = history_of(&steps);
            let (lo, hi) = (h1.min(h2), h1.max(h2));
            let mut direct = t.clone();
            direct.vacuum(hi);
            t.vacuum(lo);
            t.vacuum(hi);
            prop_assert_eq!(t.dump(), direct.dump());
            let applied = steps.len() as u64;
            for as_of in hi.min(applied)..=applied {
                let snap = Snapshot { as_of };
                let model = &history[as_of as usize];
                for k in 0u8..6 {
                    prop_assert_eq!(t.get(snap, &k).unwrap(), model.get(&k));
                }
                let got: Vec<(u8, u8)> = t.range(snap, ..).unwrap().into_iter().map(|(k, v)| (*k, *v)).collect();
                let want: Vec<(u8, u8)> = model.iter().map(|(k, v)| (*k, *v)).collect();
                prop_assert_eq!(got, want);
            }
            // Nothing newer than the horizon version is ever dropped: each key keeps at most one
            // version at or below the floor, and none of them is a deletion.
            for (k, lsn, value) in t.dump() {
                if lsn <= t.floor() {
                    prop_assert!(value.is_some(), "key {k} kept a tombstone at {lsn}");
                    prop_assert_eq!(t.dump().iter().filter(|(k2, l2, _)| *k2 == k && *l2 <= t.floor()).count(), 1);
                }
            }
        }
    }

    #[test]
    fn all_table_and_transaction_reads_reject_a_snapshot_below_the_floor() {
        let mut table: Table<u8, u8> = Table::new();
        table
            .apply(1, cs(1), vec![(1, Some(10)), (2, Some(20))])
            .unwrap();
        let old = table.snapshot();
        let mut tx = Tx::begin(&table);
        tx.put(1, 99);
        table
            .apply(2, cs(2), vec![(1, Some(11)), (2, None)])
            .unwrap();
        assert_eq!(table.version_count(), 4);
        table.vacuum(2);
        assert_eq!(table.version_count(), 1);
        let expired = Err(MvccError::SnapshotTooOld { floor: 2 });
        assert_eq!(table.get(old, &1), expired);
        assert_eq!(
            table.get(old, &200),
            expired,
            "an absent key cannot hide an expired snapshot"
        );
        assert_eq!(
            table.written_by(old, &1),
            Err(MvccError::SnapshotTooOld { floor: 2 })
        );
        assert_eq!(
            table.range(old, ..),
            Err(MvccError::SnapshotTooOld { floor: 2 })
        );
        assert_eq!(
            tx.get(&table, &1),
            expired,
            "own writes cannot revive an expired transaction"
        );
        assert_eq!(table.get(table.snapshot(), &1), Ok(Some(&11)));
        assert_eq!(table.get(table.snapshot(), &2), Ok(None));
    }

    #[test]
    fn the_vacuum_planted_bug_loses_the_exact_floor_version() {
        struct ClearBug;
        impl Drop for ClearBug {
            fn drop(&mut self) {
                bugs::clear();
            }
        }
        let mut table: Table<u8, u8> = Table::new();
        table.apply(1, cs(1), vec![(1, Some(10))]).unwrap();
        table.apply(2, cs(2), vec![(1, Some(20))]).unwrap();
        table.apply(3, cs(3), vec![(1, Some(30))]).unwrap();
        let mut control = table.clone();
        control.vacuum(2);
        assert_eq!(control.get(Snapshot { as_of: 2 }, &1), Ok(Some(&20)));
        bugs::enable(Bug::VacuumDropsVisibleVersion);
        let guard = ClearBug;
        table.vacuum(2);
        drop(guard);
        assert_eq!(table.get(Snapshot { as_of: 2 }, &1), Ok(None));
        assert_eq!(table.get(Snapshot { as_of: 3 }, &1), Ok(Some(&30)));
    }
}
