//! Model-based tests for the row store.
//!
//! A small in-memory reference model written from the documented semantics (`formats/isolation.md`:
//! snapshot isolation, first-committer-wins, `AS OF CHANGESET` and `AS OF TIMESTAMP`) is driven with
//! the same generated operation sequences as the real store. Several transactions are open at once,
//! with writes, deletes, reads, commits, aborts, clock moves, remembered snapshots and clean reopens.
//! After every step the store and the model must agree on rows, scans, secondary-index lookups and
//! commit metadata, and every remembered snapshot must still show what the model says it showed.
//!
//! `row_sim.rs` covers pairs of overlapping transactions and a map oracle at remembered snapshots.
//! This file adds interleaved transactions of any number, read-your-own-writes and stale reads, range
//! scans with every bound kind, `AS OF` by change set and by HLC (including counter and node
//! tiebreaks and a clock that goes backwards), conflict LSNs, and index lookups at every historical
//! snapshot, all after clean reopens too. The model also exists in deliberately wrong variants: the
//! harness has to fail for each of them, or it proves nothing.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_abi::Hlc;
use lattice_core::{ChangeSetId, Config};
use lattice_engines::index::IndexDef;
use lattice_engines::mvcc::Snapshot;
use lattice_engines::store::{RowError, RowStore, RowTx};
use lattice_seams::Clock;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use proptest::collection::vec as pvec;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;
type Key = (u32, Vec<u8>);
type Rows = BTreeMap<Key, Vec<u8>>;

const NODE: u32 = 1;
const TABLES: u32 = 3;
const MAX_OPEN: usize = 4;

/// Keys include the empty key and keys that are prefixes of each other.
const KEYS: [&[u8]; 6] = [b"", b"\x00", b"\x01", b"\x01\x00", b"\x01\x00\x00", b"\x02"];
/// Values include the empty value and values with zero bytes (index key escaping).
const VALUES: [&[u8]; 7] = [
    b"",
    b"\x00",
    b"\x01",
    b"\x01\x00",
    b"\x00\x01",
    b"\x02\x02\x02",
    b"\x07",
];
/// (index id, table) of the indexes the store is opened with.
const INDEXES: [(u32, u32); 3] = [(10, 0), (11, 1), (12, 1)];

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 300,
        node: NODE,
    }
}

fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64) -> Store {
    RowStore::open_with_indexes(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
        vec![
            // First byte of the value; the empty value is not indexed.
            IndexDef {
                id: 10,
                table: 0,
                extract: |v| v.first().map(|b| vec![*b]),
            },
            // The whole value, including the empty one.
            IndexDef {
                id: 11,
                table: 1,
                extract: |v| Some(v.to_vec()),
            },
            // The length, only for values of odd length. Shares a table with index 11.
            IndexDef {
                id: 12,
                table: 1,
                extract: |v| (v.len() % 2 == 1).then(|| vec![v.len() as u8]),
            },
        ],
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// The reference model
// ---------------------------------------------------------------------------------------------

/// Ways the model can be made wrong on purpose. `None` is the real model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flaw {
    None,
    /// Overlapping writers never conflict.
    LastCommitterWins,
    /// A delete does not count as a write for conflict detection.
    DeletesDontConflict,
    /// `AS OF TIMESTAMP t` uses `hlc < t` instead of `hlc <= t`.
    AsOfTimeExclusive,
    /// `AS OF CHANGESET c` lands on the change set before `c`.
    AsOfChangesetOffByOne,
    /// A transaction reads only its snapshot.
    TxIgnoresOwnWrites,
    /// Index lookups at a historical snapshot answer from the newest rows.
    IndexUsesCurrentRows,
    /// An excluded scan end is treated as included.
    ScanEndInclusive,
    /// The last commit is forgotten when the store is reopened.
    ReopenLosesLastCommit,
}

const FLAWS: [Flaw; 8] = [
    Flaw::LastCommitterWins,
    Flaw::DeletesDontConflict,
    Flaw::AsOfTimeExclusive,
    Flaw::AsOfChangesetOffByOne,
    Flaw::TxIgnoresOwnWrites,
    Flaw::IndexUsesCurrentRows,
    Flaw::ScanEndInclusive,
    Flaw::ReopenLosesLastCommit,
];

struct Commit {
    lsn: u64,
    /// (wall ms, counter); the node is always `NODE`.
    hlc: (u64, u32),
    /// The whole table state right after this commit.
    rows: Rows,
}

struct MTx {
    snap: u64,
    writes: BTreeMap<Key, Option<Vec<u8>>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Pred {
    ReadOnly(u64),
    Conflict(u64),
    Commit,
}

struct Model {
    flaw: Flaw,
    commits: Vec<Commit>,
    /// LSN of the newest write to each key, as the conflict rule sees it.
    newest: BTreeMap<Key, u64>,
    last_hlc: (u64, u32),
    empty: Rows,
}

impl Model {
    fn new(flaw: Flaw) -> Self {
        Self {
            flaw,
            commits: Vec::new(),
            newest: BTreeMap::new(),
            last_hlc: (0, 0),
            empty: Rows::new(),
        }
    }

    fn last_lsn(&self) -> u64 {
        self.commits.last().map_or(0, |c| c.lsn)
    }

    /// The table as of `as_of`: everything committed at or before it.
    fn rows_at(&self, as_of: u64) -> &Rows {
        self.commits
            .iter()
            .rev()
            .find(|c| c.lsn <= as_of)
            .map_or(&self.empty, |c| &c.rows)
    }

    fn tx_get<'a>(&'a self, tx: &'a MTx, key: &Key) -> Option<&'a Vec<u8>> {
        if self.flaw != Flaw::TxIgnoresOwnWrites
            && let Some(own) = tx.writes.get(key)
        {
            return own.as_ref();
        }
        self.rows_at(tx.snap).get(key)
    }

    fn predict(&self, tx: &MTx) -> Pred {
        if tx.writes.is_empty() {
            return Pred::ReadOnly(tx.snap);
        }
        if self.flaw != Flaw::LastCommitterWins {
            // The store reports the first conflicting row in key order.
            for key in tx.writes.keys() {
                if let Some(&newer) = self.newest.get(key)
                    && newer > tx.snap
                {
                    return Pred::Conflict(newer);
                }
            }
        }
        Pred::Commit
    }

    /// The HLC the next commit gets at wall-clock time `now`.
    fn tick(&self, now: u64) -> (u64, u32) {
        let (last_wall, last_counter) = self.last_hlc;
        let wall = now.max(last_wall);
        if wall == last_wall {
            (wall, last_counter + 1)
        } else {
            (wall, 0)
        }
    }

    fn apply(&mut self, lsn: u64, now: u64, tx: &MTx) {
        let hlc = self.tick(now);
        self.last_hlc = hlc;
        let mut rows = self.rows_at(u64::MAX).clone();
        for (key, value) in &tx.writes {
            match value {
                Some(v) => {
                    rows.insert(key.clone(), v.clone());
                }
                None => {
                    rows.remove(key);
                }
            }
            if !(value.is_none() && self.flaw == Flaw::DeletesDontConflict) {
                self.newest.insert(key.clone(), lsn);
            }
        }
        self.commits.push(Commit { lsn, hlc, rows });
    }

    /// `AS OF TIMESTAMP t`.
    fn as_of_time(&self, t: (u64, u32, u32)) -> u64 {
        self.commits
            .iter()
            .rev()
            .find(|c| {
                let h = (c.hlc.0, c.hlc.1, NODE);
                if self.flaw == Flaw::AsOfTimeExclusive {
                    h < t
                } else {
                    h <= t
                }
            })
            .map_or(0, |c| c.lsn)
    }

    /// `AS OF CHANGESET` for the `i`th commit.
    fn as_of_changeset(&self, i: usize) -> u64 {
        let i = if self.flaw == Flaw::AsOfChangesetOffByOne {
            i.checked_sub(1)
        } else {
            Some(i)
        };
        i.and_then(|i| self.commits.get(i)).map_or(0, |c| c.lsn)
    }

    fn on_reopen(&mut self) {
        if self.flaw == Flaw::ReopenLosesLastCommit {
            self.commits.pop();
        }
    }

    /// What index `idx` keys a row value by; written separately from the store's extractors.
    fn index_key(idx: u32, value: &[u8]) -> Option<Vec<u8>> {
        match idx {
            10 => {
                if value.is_empty() {
                    None
                } else {
                    Some(value[..1].to_vec())
                }
            }
            11 => Some(value.to_vec()),
            12 => (value.len() & 1 == 1).then(|| vec![value.len() as u8]),
            _ => None,
        }
    }

    /// Every index key a value in the universe can produce, plus one nothing produces.
    fn index_universe(idx: u32) -> BTreeSet<Vec<u8>> {
        let mut out: BTreeSet<Vec<u8>> = VALUES
            .iter()
            .filter_map(|v| Self::index_key(idx, v))
            .collect();
        out.insert(vec![9, 9, 9]);
        out
    }

    /// Primary keys the index finds for `ikey` as of `as_of`, in key order.
    fn lookup(&self, as_of: u64, idx: u32, table: u32, ikey: &[u8]) -> Vec<Vec<u8>> {
        let rows = if self.flaw == Flaw::IndexUsesCurrentRows {
            self.rows_at(u64::MAX)
        } else {
            self.rows_at(as_of)
        };
        rows.iter()
            .filter(|((t, _), v)| *t == table && Self::index_key(idx, v).as_deref() == Some(ikey))
            .map(|((_, pk), _)| pk.clone())
            .collect()
    }

    /// Rows of `table` inside the bounds as of `as_of`, in key order.
    fn scan(
        &self,
        as_of: u64,
        table: u32,
        lo: &Bound<Vec<u8>>,
        hi: &Bound<Vec<u8>>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let hi = match hi {
            Bound::Excluded(k) if self.flaw == Flaw::ScanEndInclusive => Bound::Included(k.clone()),
            other => other.clone(),
        };
        self.rows_at(as_of)
            .iter()
            .filter(|((t, k), _)| {
                *t == table
                    && match lo {
                        Bound::Included(b) => k >= b,
                        Bound::Excluded(b) => k > b,
                        Bound::Unbounded => true,
                    }
                    && match &hi {
                        Bound::Included(b) => k <= b,
                        Bound::Excluded(b) => k < b,
                        Bound::Unbounded => true,
                    }
            })
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Op {
    Begin,
    Put {
        tx: u8,
        table: u8,
        key: u8,
        val: u8,
    },
    Delete {
        tx: u8,
        table: u8,
        key: u8,
    },
    Read {
        tx: u8,
        table: u8,
        key: u8,
    },
    Commit {
        tx: u8,
    },
    Abort {
        tx: u8,
    },
    Advance {
        ms: u8,
    },
    SetBack {
        ms: u8,
    },
    Remember,
    Verify,
    ReadAt {
        sel: u16,
        table: u8,
        key: u8,
    },
    Scan {
        sel: u16,
        table: u8,
        a: u8,
        b: u8,
        lo: u8,
        hi: u8,
    },
    AsOfChangeset {
        commit: u8,
    },
    AsOfTime {
        commit: u8,
        dw: u8,
        dc: u8,
        node: u8,
    },
    Reopen,
}

/// Mostly a few hot keys, so concurrent transactions collide, with the whole key set now and then.
fn key() -> impl Strategy<Value = u8> {
    prop_oneof![4 => 0u8..2, 1 => 0u8..6]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => Just(Op::Begin),
        10 => (any::<u8>(), 0u8..3, key(), 0u8..7)
            .prop_map(|(tx, table, key, val)| Op::Put { tx, table, key, val }),
        4 => (any::<u8>(), 0u8..3, key())
            .prop_map(|(tx, table, key)| Op::Delete { tx, table, key }),
        5 => (any::<u8>(), 0u8..3, 0u8..6)
            .prop_map(|(tx, table, key)| Op::Read { tx, table, key }),
        7 => any::<u8>().prop_map(|tx| Op::Commit { tx }),
        2 => any::<u8>().prop_map(|tx| Op::Abort { tx }),
        3 => (0u8..4).prop_map(|ms| Op::Advance { ms }),
        1 => (1u8..4).prop_map(|ms| Op::SetBack { ms }),
        2 => Just(Op::Remember),
        2 => Just(Op::Verify),
        4 => (any::<u16>(), 0u8..3, 0u8..6)
            .prop_map(|(sel, table, key)| Op::ReadAt { sel, table, key }),
        4 => (any::<u16>(), 0u8..3, 0u8..6, 0u8..6, 0u8..3, 0u8..3)
            .prop_map(|(sel, table, a, b, lo, hi)| Op::Scan { sel, table, a, b, lo, hi }),
        3 => any::<u8>().prop_map(|commit| Op::AsOfChangeset { commit }),
        4 => (any::<u8>(), 0u8..5, 0u8..3, 0u8..3)
            .prop_map(|(commit, dw, dc, node)| Op::AsOfTime { commit, dw, dc, node }),
        2 => Just(Op::Reopen),
    ]
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    pvec(op(), 1..70)
}

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

fn key_of(table: u8, key: u8) -> Key {
    (
        u32::from(table),
        KEYS[usize::from(key) % KEYS.len()].to_vec(),
    )
}

fn bound(kind: u8, key: &[u8]) -> Bound<Vec<u8>> {
    match kind % 3 {
        0 => Bound::Included(key.to_vec()),
        1 => Bound::Excluded(key.to_vec()),
        _ => Bound::Unbounded,
    }
}

/// Compares everything observable at `as_of`: every key, a full scan of every table, every index
/// lookup, and the index consistency check (I9).
fn check_at(store: &Store, model: &Model, as_of: u64) -> Result<(), String> {
    let snap = Snapshot { as_of };
    let rows = model.rows_at(as_of);
    for table in 0..TABLES {
        for key in KEYS {
            let got = store
                .get_at(snap, table, key, &lattice_engines::limits::AllowAll)
                .unwrap();
            let want = rows.get(&(table, key.to_vec()));
            ensure!(
                got == want,
                "get at {as_of} table {table} key {key:?}: store {got:?}, model {want:?}"
            );
        }
        let got: Vec<(Vec<u8>, Vec<u8>)> = store
            .scan_at(snap, table, .., &lattice_engines::limits::AllowAll)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.to_vec(), v.clone()))
            .collect();
        let want = model.scan(as_of, table, &Bound::Unbounded, &Bound::Unbounded);
        ensure!(
            got == want,
            "full scan at {as_of} table {table}: store {got:?}, model {want:?}"
        );
    }
    for (idx, table) in INDEXES {
        for ikey in Model::index_universe(idx) {
            let got = store
                .lookup_at(snap, idx, &ikey, &lattice_engines::limits::AllowAll)
                .unwrap();
            let want = model.lookup(as_of, idx, table, &ikey);
            ensure!(
                got == want,
                "lookup at {as_of} index {idx} key {ikey:?}: store {got:?}, model {want:?}"
            );
        }
    }
    store
        .check_indexes_at(snap)
        .map_err(|why| format!("indexes at {as_of}: {why}"))
}

/// Compares the commit metadata the store keeps against the model and the ids seen so far.
fn check_metadata(store: &Store, model: &Model, ids: &[ChangeSetId]) -> Result<(), String> {
    ensure!(
        store.applied_lsn() == model.last_lsn(),
        "applied lsn: store {}, model {}",
        store.applied_lsn(),
        model.last_lsn()
    );
    ensure!(ids.len() == model.commits.len(), "id bookkeeping drifted");
    let known: BTreeSet<u64> = model.commits.iter().map(|c| c.lsn).collect();
    for (i, c) in model.commits.iter().enumerate() {
        let want = Hlc {
            wall_ms: c.hlc.0,
            counter: c.hlc.1,
            node: NODE,
        };
        ensure!(
            store.commit_time_at(c.lsn) == Some(want),
            "commit time at {}: store {:?}, model {want:?}",
            c.lsn,
            store.commit_time_at(c.lsn)
        );
        ensure!(
            store.changeset_id_at(c.lsn) == Some(ids[i]),
            "change set id at {} changed",
            c.lsn
        );
        ensure!(
            store.snapshot_as_of_changeset(ids[i]) == Some(Snapshot { as_of: c.lsn }),
            "AS OF CHANGESET for commit {i} (lsn {})",
            c.lsn
        );
    }
    for lsn in 0..=model.last_lsn() + 2 {
        if !known.contains(&lsn) {
            ensure!(
                store.changeset_id_at(lsn).is_none() && store.commit_time_at(lsn).is_none(),
                "lsn {lsn} is not a row commit but the store names one"
            );
        }
    }
    Ok(())
}

#[derive(Default, Debug)]
struct Stats {
    commits: u32,
    read_only_commits: u32,
    conflicts: u32,
    aborts: u32,
    reopens: u32,
    own_write_reads: u32,
    stale_reads: u32,
    as_of_changeset: u32,
    as_of_time_hits: u32,
    as_of_time_empty: u32,
    scans: u32,
    nonempty_lookups: u32,
    backwards_clock: u32,
    same_ms_commits: u32,
}

/// Runs `ops` against a fresh store and a model with the given flaw. The first disagreement is the
/// error.
fn run(ops: &[Op], flaw: Flaw) -> Result<Stats, String> {
    let disk = SharedIo::new(SimIo::new(7, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let mut store = open(&disk, &clock, 1);
    let mut reopen_seed = 1u64;
    let mut model = Model::new(flaw);
    let mut ids: Vec<ChangeSetId> = Vec::new();
    let mut open_txs: Vec<(RowTx, MTx)> = Vec::new();
    let mut remembered: Vec<u64> = Vec::new();
    let mut stats = Stats::default();

    for (step, op) in ops.iter().enumerate() {
        let at = |why: String| format!("step {step} ({op:?}): {why}");
        // A transaction op with nothing open starts one, so generated sequences stay productive.
        let needs_tx = matches!(
            op,
            Op::Put { .. }
                | Op::Delete { .. }
                | Op::Read { .. }
                | Op::Commit { .. }
                | Op::Abort { .. }
        );
        if needs_tx && open_txs.is_empty() {
            let tx = store.begin();
            let snap = tx.snapshot().as_of;
            ensure!(
                snap == model.last_lsn(),
                "{}",
                at(format!("begin: store {snap}, model {}", model.last_lsn()))
            );
            open_txs.push((
                tx,
                MTx {
                    snap,
                    writes: BTreeMap::new(),
                },
            ));
        }
        match *op {
            Op::Begin => {
                if open_txs.len() < MAX_OPEN {
                    let tx = store.begin();
                    let snap = tx.snapshot().as_of;
                    ensure!(
                        snap == model.last_lsn(),
                        "{}",
                        at(format!("begin: store {snap}, model {}", model.last_lsn()))
                    );
                    open_txs.push((
                        tx,
                        MTx {
                            snap,
                            writes: BTreeMap::new(),
                        },
                    ));
                }
            }
            Op::Put {
                tx,
                table,
                key,
                val,
            } => {
                let i = usize::from(tx) % open_txs.len();
                let k = key_of(table, key);
                let v = VALUES[usize::from(val) % VALUES.len()].to_vec();
                let (rtx, mtx) = &mut open_txs[i];
                rtx.put(k.clone(), v.clone());
                mtx.writes.insert(k, Some(v));
            }
            Op::Delete { tx, table, key } => {
                let i = usize::from(tx) % open_txs.len();
                let k = key_of(table, key);
                let (rtx, mtx) = &mut open_txs[i];
                rtx.delete(k.clone());
                mtx.writes.insert(k, None);
            }
            Op::Read { tx, table, key } => {
                let i = usize::from(tx) % open_txs.len();
                let k = key_of(table, key);
                let (rtx, mtx) = &open_txs[i];
                let got = store.get(rtx, k.0, &k.1).unwrap();
                let want = model.tx_get(mtx, &k);
                ensure!(
                    got == want,
                    "{}",
                    at(format!("tx read {k:?}: store {got:?}, model {want:?}"))
                );
                if mtx.writes.contains_key(&k) {
                    stats.own_write_reads += 1;
                } else if model.rows_at(mtx.snap) != model.rows_at(u64::MAX) {
                    stats.stale_reads += 1;
                }
            }
            Op::Abort { tx } => {
                let i = usize::from(tx) % open_txs.len();
                drop(open_txs.remove(i));
                stats.aborts += 1;
            }
            Op::Commit { tx } => {
                let i = usize::from(tx) % open_txs.len();
                let (rtx, mtx) = open_txs.remove(i);
                let pred = model.predict(&mtx);
                let res = store.commit(rtx);
                match (&pred, res) {
                    (Pred::ReadOnly(want), Ok(got)) if *want == got => {
                        stats.read_only_commits += 1;
                    }
                    (Pred::Conflict(want), Err(RowError::Conflict { newer_lsn }))
                        if *want == newer_lsn =>
                    {
                        stats.conflicts += 1;
                    }
                    (Pred::Commit, Ok(lsn)) => {
                        ensure!(
                            lsn > model.last_lsn(),
                            "{}",
                            at(format!(
                                "lsn {lsn} did not advance past {}",
                                model.last_lsn()
                            ))
                        );
                        let before = model.last_hlc;
                        model.apply(lsn, clock.now_ms(), &mtx);
                        if model.last_hlc.0 == before.0 {
                            stats.same_ms_commits += 1;
                        }
                        let id = store.changeset_id_at(lsn);
                        ensure!(
                            id.is_some(),
                            "{}",
                            at("no change set id for the commit".into())
                        );
                        let id = id.unwrap();
                        ensure!(
                            !ids.contains(&id),
                            "{}",
                            at("a change set id repeated".into())
                        );
                        ids.push(id);
                        stats.commits += 1;
                    }
                    (pred, res) => {
                        return Err(at(format!(
                            "commit: model predicted {pred:?}, store gave {res:?}"
                        )));
                    }
                }
            }
            Op::Advance { ms } => clock.advance(u64::from(ms)),
            Op::SetBack { ms } => {
                clock.set(clock.now_ms().saturating_sub(u64::from(ms)));
                stats.backwards_clock += 1;
            }
            Op::Remember => remembered.push(store.snapshot().as_of),
            Op::Verify => {
                for &as_of in &remembered {
                    check_at(&store, &model, as_of).map_err(at)?;
                }
            }
            Op::ReadAt { sel, table, key } => {
                let as_of = pick_snapshot(sel, &model, &remembered, store.snapshot().as_of);
                let k = key_of(table, key);
                let got = store
                    .get_at(
                        Snapshot { as_of },
                        k.0,
                        &k.1,
                        &lattice_engines::limits::AllowAll,
                    )
                    .unwrap();
                let want = model.rows_at(as_of).get(&k);
                ensure!(
                    got == want,
                    "{}",
                    at(format!(
                        "get at {as_of} {k:?}: store {got:?}, model {want:?}"
                    ))
                );
            }
            Op::Scan {
                sel,
                table,
                a,
                b,
                lo,
                hi,
            } => {
                let as_of = pick_snapshot(sel, &model, &remembered, store.snapshot().as_of);
                let (mut ka, mut kb) = (
                    KEYS[usize::from(a) % KEYS.len()].to_vec(),
                    KEYS[usize::from(b) % KEYS.len()].to_vec(),
                );
                if ka > kb {
                    std::mem::swap(&mut ka, &mut kb);
                }
                let lob = bound(lo, &ka);
                let mut hib = bound(hi, &kb);
                // The store's range panics on an empty excluded range, like a BTreeMap.
                if ka == kb && matches!((&lob, &hib), (Bound::Excluded(_), Bound::Excluded(_))) {
                    hib = Bound::Included(kb.clone());
                }
                let got: Vec<(Vec<u8>, Vec<u8>)> = store
                    .scan_at(
                        Snapshot { as_of },
                        u32::from(table),
                        (lob.clone(), hib.clone()),
                        &lattice_engines::limits::AllowAll,
                    )
                    .unwrap()
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.clone()))
                    .collect();
                let want = model.scan(as_of, u32::from(table), &lob, &hib);
                ensure!(
                    got == want,
                    "{}",
                    at(format!(
                        "scan {lob:?}..{hib:?} at {as_of} table {table}: store {got:?}, model {want:?}"
                    ))
                );
                stats.scans += 1;
            }
            Op::AsOfChangeset { commit } => {
                if model.commits.is_empty() {
                    // No change set exists yet, so any id is unknown.
                    let unknown = ChangeSetId::from_parts(5, 5, 5);
                    ensure!(
                        store.snapshot_as_of_changeset(unknown).is_none(),
                        "{}",
                        at("an unknown change set id resolved".into())
                    );
                } else {
                    let c = usize::from(commit) % model.commits.len();
                    let got = store.snapshot_as_of_changeset(ids[c]).map(|s| s.as_of);
                    let want = model.as_of_changeset(c);
                    ensure!(
                        got == Some(want),
                        "{}",
                        at(format!("AS OF CHANGESET {c}: store {got:?}, model {want}"))
                    );
                    check_at(&store, &model, want).map_err(at)?;
                    remembered.push(want);
                    stats.as_of_changeset += 1;
                }
            }
            Op::AsOfTime {
                commit,
                dw,
                dc,
                node,
            } => {
                let base = if model.commits.is_empty() {
                    (clock.now_ms(), 0)
                } else {
                    model.commits[usize::from(commit) % model.commits.len()].hlc
                };
                let (wall, counter) = match dw {
                    0 => (base.0.saturating_sub(1), base.1),
                    1 => (base.0, base.1),
                    2 => (base.0 + 1, base.1),
                    3 => (0, 0),
                    _ => (u64::MAX, u32::MAX),
                };
                let counter = match dc {
                    0 => counter.saturating_sub(1),
                    1 => counter,
                    _ => counter.saturating_add(1),
                };
                let t = (wall, counter, u32::from(node));
                let got = store
                    .snapshot_as_of_hlc(Hlc {
                        wall_ms: t.0,
                        counter: t.1,
                        node: t.2,
                    })
                    .as_of;
                let want = model.as_of_time(t);
                ensure!(
                    got == want,
                    "{}",
                    at(format!("AS OF TIMESTAMP {t:?}: store {got}, model {want}"))
                );
                check_at(&store, &model, want).map_err(at)?;
                remembered.push(want);
                if want == 0 {
                    stats.as_of_time_empty += 1;
                } else {
                    stats.as_of_time_hits += 1;
                }
            }
            Op::Reopen => {
                // A clean shutdown ends every session: open transactions go with the process.
                open_txs.clear();
                drop(store);
                reopen_seed += 1;
                store = open(&disk, &clock, reopen_seed);
                model.on_reopen();
                ids.truncate(model.commits.len());
                check_metadata(&store, &model, &ids).map_err(at)?;
                for &as_of in &remembered {
                    check_at(&store, &model, as_of).map_err(at)?;
                }
                stats.reopens += 1;
            }
        }
        // After every step the newest state must agree, and so must the metadata cheaply checkable.
        let now = store.snapshot().as_of;
        ensure!(
            now == model.last_lsn(),
            "{}",
            at(format!(
                "newest snapshot: store {now}, model {}",
                model.last_lsn()
            ))
        );
        check_at(&store, &model, now).map_err(at)?;
        for (rtx, mtx) in &open_txs {
            ensure!(
                rtx.snapshot().as_of == mtx.snap,
                "{}",
                at("an open transaction's snapshot moved".into())
            );
        }
    }

    // At the end, every state in history, not only the remembered ones, must still read the same.
    check_metadata(&store, &model, &ids).map_err(|e| format!("end: {e}"))?;
    let mut all: Vec<u64> = model.commits.iter().map(|c| c.lsn).collect();
    all.push(0);
    all.extend(&remembered);
    for as_of in all {
        check_at(&store, &model, as_of).map_err(|e| format!("end: {e}"))?;
    }
    for (idx, table) in INDEXES {
        for ikey in Model::index_universe(idx) {
            if !model.lookup(u64::MAX, idx, table, &ikey).is_empty() {
                stats.nonempty_lookups += 1;
            }
        }
    }
    Ok(stats)
}

/// A snapshot to read at: a remembered one, a commit's, an arbitrary LSN (past the end too), or the
/// newest.
fn pick_snapshot(sel: u16, model: &Model, remembered: &[u64], newest: u64) -> u64 {
    let n = usize::from(sel / 4);
    match sel % 4 {
        0 if !remembered.is_empty() => remembered[n % remembered.len()],
        1 if !model.commits.is_empty() => model.commits[n % model.commits.len()].lsn,
        2 => (n as u64) % (model.last_lsn() + 4),
        _ => newest,
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    /// The real store and the real model agree on every generated history.
    #[test]
    fn the_row_store_matches_the_model(ops in ops()) {
        if let Err(why) = run(&ops, Flaw::None) {
            prop_assert!(false, "{why}");
        }
    }
}

/// The model must be able to disagree: each wrong variant has to be caught by the same harness,
/// otherwise a passing run above says nothing.
#[test]
fn every_wrong_model_is_caught() {
    for flaw in FLAWS {
        let mut runner = TestRunner::deterministic();
        let strategy = ops();
        let mut caught = None;
        for _ in 0..600 {
            let ops = strategy.new_tree(&mut runner).unwrap().current();
            if let Err(why) = run(&ops, flaw) {
                caught = Some(why);
                break;
            }
        }
        let why = caught.unwrap_or_else(|| panic!("the harness never noticed {flaw:?}"));
        eprintln!("{flaw:?}: {why}");
    }
}

/// The generator reaches the cases the properties are about, so a pass is not vacuous.
#[test]
fn the_generated_histories_reach_every_interesting_case() {
    let mut runner = TestRunner::deterministic();
    let strategy = ops();
    let mut total = Stats::default();
    for _ in 0..300 {
        let ops = strategy.new_tree(&mut runner).unwrap().current();
        let s = run(&ops, Flaw::None).unwrap_or_else(|why| panic!("{why}"));
        total.commits += s.commits;
        total.read_only_commits += s.read_only_commits;
        total.conflicts += s.conflicts;
        total.aborts += s.aborts;
        total.reopens += s.reopens;
        total.own_write_reads += s.own_write_reads;
        total.stale_reads += s.stale_reads;
        total.as_of_changeset += s.as_of_changeset;
        total.as_of_time_hits += s.as_of_time_hits;
        total.as_of_time_empty += s.as_of_time_empty;
        total.scans += s.scans;
        total.nonempty_lookups += s.nonempty_lookups;
        total.backwards_clock += s.backwards_clock;
        total.same_ms_commits += s.same_ms_commits;
    }
    eprintln!("{total:?}");
    assert!(total.commits > 0, "{total:?}");
    assert!(total.read_only_commits > 0, "{total:?}");
    assert!(total.conflicts > 0, "{total:?}");
    assert!(total.aborts > 0, "{total:?}");
    assert!(total.reopens > 0, "{total:?}");
    assert!(total.own_write_reads > 0, "{total:?}");
    assert!(total.stale_reads > 0, "{total:?}");
    assert!(total.as_of_changeset > 0, "{total:?}");
    assert!(total.as_of_time_hits > 0, "{total:?}");
    assert!(total.as_of_time_empty > 0, "{total:?}");
    assert!(total.scans > 0, "{total:?}");
    assert!(total.nonempty_lookups > 0, "{total:?}");
    assert!(total.backwards_clock > 0, "{total:?}");
    assert!(total.same_ms_commits > 0, "{total:?}");
}

/// Write skew is allowed by snapshot isolation (documented): two transactions that read each
/// other's rows and write different ones both commit, in the store and in the model.
#[test]
fn write_skew_commits_in_both_the_store_and_the_model() {
    let script = [
        Op::Begin,
        Op::Begin,
        Op::Read {
            tx: 0,
            table: 0,
            key: 1,
        },
        Op::Put {
            tx: 0,
            table: 0,
            key: 0,
            val: 2,
        },
        Op::Read {
            tx: 1,
            table: 0,
            key: 0,
        },
        Op::Put {
            tx: 1,
            table: 0,
            key: 1,
            val: 2,
        },
        Op::Commit { tx: 0 },
        Op::Commit { tx: 0 },
    ];
    let stats = run(&script, Flaw::None).unwrap();
    assert_eq!(stats.commits, 2);
    assert_eq!(stats.conflicts, 0);
}

/// A delete after a concurrent update conflicts and reports the update's LSN; a delete of a row
/// that never existed is still a write.
#[test]
fn deletes_conflict_like_any_other_write() {
    let script = [
        Op::Begin,
        Op::Begin,
        Op::Delete {
            tx: 0,
            table: 1,
            key: 2,
        },
        Op::Put {
            tx: 1,
            table: 1,
            key: 2,
            val: 3,
        },
        Op::Commit { tx: 1 },
        Op::Commit { tx: 0 },
    ];
    let stats = run(&script, Flaw::None).unwrap();
    assert_eq!((stats.commits, stats.conflicts), (1, 1));
}
