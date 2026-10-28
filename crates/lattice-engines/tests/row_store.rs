//! The row store against the simulated disk: snapshot isolation, first-committer-wins, recovery and
//! the atomic visibility of a change set across the log and the rows (I2, I3).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_engines::store::{RowError, RowStore};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 400,
        node: 1,
    }
}

fn fresh() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(1, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}

fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64) -> Result<Store, RowError> {
    RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
    )
}

fn put(store: &mut Store, table: u32, key: &[u8], value: &[u8]) -> u64 {
    let mut tx = store.begin();
    tx.put((table, key.to_vec()), value.to_vec());
    store.commit(tx).unwrap()
}

#[test]
fn a_snapshot_keeps_seeing_what_it_saw_while_later_commits_land() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    put(&mut store, 1, b"a", b"one");
    let before = store.snapshot();
    put(&mut store, 1, b"a", b"two");
    let mut tx = store.begin();
    tx.delete((1, b"a".to_vec()));
    store.commit(tx).unwrap();
    assert_eq!(store.get_at(before, 1, b"a"), Some(&b"one".to_vec()));
    assert_eq!(store.get_at(store.snapshot(), 1, b"a"), None);
}

#[test]
fn a_transaction_reads_its_own_writes_then_its_snapshot() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    put(&mut store, 1, b"k", b"committed");
    let mut tx = store.begin();
    assert_eq!(store.get(&tx, 1, b"k"), Some(&b"committed".to_vec()));
    tx.put((1, b"k".to_vec()), b"mine".to_vec());
    assert_eq!(store.get(&tx, 1, b"k"), Some(&b"mine".to_vec()));
    // Nothing is visible to others until it commits.
    assert_eq!(
        store.get_at(store.snapshot(), 1, b"k"),
        Some(&b"committed".to_vec())
    );
}

#[test]
fn the_first_committer_wins_and_a_retry_succeeds() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    put(&mut store, 1, b"row", b"v0");
    let mut a = store.begin();
    let mut b = store.begin();
    a.put((1, b"row".to_vec()), b"from a".to_vec());
    b.put((1, b"row".to_vec()), b"from b".to_vec());
    let a_lsn = store.commit(a).unwrap();
    match store.commit(b) {
        Err(RowError::Conflict { newer_lsn }) => assert_eq!(newer_lsn, a_lsn),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(
        store.get_at(store.snapshot(), 1, b"row"),
        Some(&b"from a".to_vec())
    );
    // The loser retries on a fresh snapshot and wins.
    let mut retry = store.begin();
    retry.put((1, b"row".to_vec()), b"from b".to_vec());
    store.commit(retry).unwrap();
}

#[test]
fn transactions_on_different_rows_do_not_conflict_but_write_skew_is_allowed() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    put(&mut store, 1, b"x", b"0");
    put(&mut store, 1, b"y", b"0");
    let mut a = store.begin();
    let mut b = store.begin();
    // Each reads the other's row and writes its own: allowed under snapshot isolation.
    assert!(store.get(&a, 1, b"y").is_some());
    assert!(store.get(&b, 1, b"x").is_some());
    a.put((1, b"x".to_vec()), b"1".to_vec());
    b.put((1, b"y".to_vec()), b"1".to_vec());
    store.commit(a).unwrap();
    store.commit(b).unwrap();
}

#[test]
fn a_rejected_commit_leaves_no_trace_in_the_rows() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    put(&mut store, 1, b"row", b"v0");
    let lsn = store.applied_lsn();
    let mut a = store.begin();
    let mut b = store.begin();
    a.put((1, b"row".to_vec()), b"a".to_vec());
    b.put((1, b"row".to_vec()), b"b".to_vec());
    b.put((1, b"other".to_vec()), b"b-other".to_vec());
    store.commit(a).unwrap();
    assert!(store.commit(b).is_err());
    // None of the loser's writes, including the row it did not conflict on, are visible.
    assert_eq!(store.get_at(store.snapshot(), 1, b"other"), None);
    assert!(store.applied_lsn() > lsn);
}

#[test]
fn scans_are_ordered_per_table_and_respect_the_snapshot() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    for (t, k) in [(1u32, "b"), (1, "a"), (1, "c"), (2, "a")] {
        put(&mut store, t, k.as_bytes(), k.as_bytes());
    }
    let snap = store.snapshot();
    put(&mut store, 1, b"d", b"d");
    let rows = store.scan_at(snap, 1, ..);
    let keys: Vec<&[u8]> = rows.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, vec![b"a".as_slice(), b"b", b"c"]);
    let bounded = store.scan_at(snap, 1, b"b".to_vec()..=b"c".to_vec());
    assert_eq!(bounded.len(), 2);
}

#[test]
fn reopening_reproduces_the_committed_rows_and_ignores_other_change_sets() {
    let (disk, clock) = fresh();
    {
        let mut store = open(&disk, &clock, 1).unwrap();
        put(&mut store, 1, b"a", b"1");
        put(&mut store, 1, b"b", b"2");
        let mut tx = store.begin();
        tx.delete((1, b"a".to_vec()));
        tx.put((2, b"z".to_vec()), b"9".to_vec());
        store.commit(tx).unwrap();
    }
    // Another engine's change set in the same log.
    lattice_core::ledger::append(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(5),
        "sim",
        "{}",
    )
    .unwrap();
    let store = open(&disk, &clock, 2).unwrap();
    let snap = store.snapshot();
    assert_eq!(store.get_at(snap, 1, b"a"), None);
    assert_eq!(store.get_at(snap, 1, b"b"), Some(&b"2".to_vec()));
    assert_eq!(store.get_at(snap, 2, b"z"), Some(&b"9".to_vec()));
}

/// I2 and I3 under power cuts: every transaction writes a distinct marker row in each of several
/// tables, so after any crash a transaction is present in all its tables or in none, and everything
/// the store acknowledged is present.
#[test]
fn a_change_set_is_all_in_the_rows_or_not_at_all_across_crashes() {
    for seed in 0..60u64 {
        let (disk, clock) = fresh();
        let mut rng = SeededRng::new(seed);
        let mut acked: Vec<u64> = Vec::new();
        let mut attempted: Vec<u64> = Vec::new();
        let mut store = Some(open(&disk, &clock, seed).unwrap());
        for n in 0..40u64 {
            if rng.below(6) == 0 {
                disk.arm_power_cut(rng.below(8));
            }
            let Some(s) = store.as_mut() else {
                disk.crash();
                store = open(&disk, &clock, seed + 1000 + n).ok();
                continue;
            };
            let mut tx = s.begin();
            for table in 1..=3u32 {
                tx.put((table, n.to_le_bytes().to_vec()), vec![n as u8; 5]);
            }
            attempted.push(n);
            match s.commit(tx) {
                Ok(_) => acked.push(n),
                Err(_) => drop(store.take()), // the process is gone; recover next round
            }
            if rng.below(8) == 0 {
                // Release the log lock before the next open takes it.
                drop(store.take());
                disk.crash();
                store = open(&disk, &clock, seed + 2000 + n).ok();
            }
        }
        drop(store);
        disk.crash();
        // The final check runs on a disk without further faults.
        disk.set_profile(FaultProfile::clean());
        let s = open(&disk, &clock, seed + 9000).unwrap();
        let snap = s.snapshot();
        for n in attempted {
            let present: Vec<bool> = (1..=3u32)
                .map(|t| s.get_at(snap, t, &n.to_le_bytes()).is_some())
                .collect();
            let all = present.iter().all(|p| *p);
            let none = present.iter().all(|p| !*p);
            assert!(
                all || none,
                "seed {seed}: transaction {n} is partial: {present:?}"
            );
            if acked.contains(&n) {
                assert!(all, "seed {seed}: acknowledged transaction {n} was lost");
            }
        }
    }
}
