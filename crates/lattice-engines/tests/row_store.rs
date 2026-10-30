//! The row store against the simulated disk: snapshot isolation, first-committer-wins, recovery and
//! the atomic visibility of a change set across the log and the rows (I2, I3).
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_engines::index::IndexDef;
use lattice_engines::store::{RowError, RowStore};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::ops::Bound;
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
    RowStore::open_with_indexes(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
        (1..=3)
            .map(|table| IndexDef {
                id: table,
                table,
                extract: |v| Some(v.to_vec()),
            })
            .collect(),
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
    assert_eq!(
        store
            .get_at(before, 1, b"a", &lattice_engines::limits::AllowAll)
            .unwrap(),
        Some(&b"one".to_vec())
    );
    assert_eq!(
        store
            .get_at(
                store.snapshot(),
                1,
                b"a",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        None
    );
}

#[test]
fn a_transaction_reads_its_own_writes_then_its_snapshot() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    put(&mut store, 1, b"k", b"committed");
    let mut tx = store.begin();
    assert_eq!(
        store.get(&tx, 1, b"k").unwrap(),
        Some(&b"committed".to_vec())
    );
    tx.put((1, b"k".to_vec()), b"mine".to_vec());
    assert_eq!(store.get(&tx, 1, b"k").unwrap(), Some(&b"mine".to_vec()));
    // Nothing is visible to others until it commits.
    assert_eq!(
        store
            .get_at(
                store.snapshot(),
                1,
                b"k",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
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
        store
            .get_at(
                store.snapshot(),
                1,
                b"row",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
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
    assert!(store.get(&a, 1, b"y").unwrap().is_some());
    assert!(store.get(&b, 1, b"x").unwrap().is_some());
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
    assert_eq!(
        store
            .get_at(
                store.snapshot(),
                1,
                b"other",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        None
    );
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
    let rows = store
        .scan_at(snap, 1, .., &lattice_engines::limits::AllowAll)
        .unwrap();
    let keys: Vec<&[u8]> = rows.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, vec![b"a".as_slice(), b"b", b"c"]);
    let bounded = store
        .scan_at(
            snap,
            1,
            b"b".to_vec()..=b"c".to_vec(),
            &lattice_engines::limits::AllowAll,
        )
        .unwrap();
    assert_eq!(bounded.len(), 2);
}

#[test]
fn inverted_and_equal_excluded_scan_bounds_are_empty_not_a_panic() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    for k in ["a", "b", "c"] {
        put(&mut store, 1, k.as_bytes(), k.as_bytes());
    }
    let snap = store.snapshot();
    let policy = &lattice_engines::limits::AllowAll;
    let inverted = store
        .scan_at(snap, 1, b"z".to_vec()..b"a".to_vec(), policy)
        .unwrap();
    assert!(inverted.is_empty());
    let equal_excluded = store
        .scan_at(
            snap,
            1,
            (
                Bound::Excluded(b"b".to_vec()),
                Bound::Excluded(b"b".to_vec()),
            ),
            policy,
        )
        .unwrap();
    assert!(equal_excluded.is_empty());
    let equal_half_open = store
        .scan_at(
            snap,
            1,
            (
                Bound::Included(b"b".to_vec()),
                Bound::Excluded(b"b".to_vec()),
            ),
            policy,
        )
        .unwrap();
    assert!(equal_half_open.is_empty());
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
    assert_eq!(
        store
            .get_at(snap, 1, b"a", &lattice_engines::limits::AllowAll)
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .get_at(snap, 1, b"b", &lattice_engines::limits::AllowAll)
            .unwrap(),
        Some(&b"2".to_vec())
    );
    assert_eq!(
        store
            .get_at(snap, 2, b"z", &lattice_engines::limits::AllowAll)
            .unwrap(),
        Some(&b"9".to_vec())
    );
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
        s.check_indexes().unwrap();
        for n in attempted {
            for table in 1..=3 {
                let expected = if s
                    .get_at(
                        snap,
                        table,
                        &n.to_le_bytes(),
                        &lattice_engines::limits::AllowAll,
                    )
                    .unwrap()
                    .is_some()
                {
                    vec![n.to_le_bytes().to_vec()]
                } else {
                    Vec::new()
                };
                assert_eq!(
                    s.lookup_at(
                        snap,
                        table,
                        &[n as u8; 5],
                        &lattice_engines::limits::AllowAll
                    )
                    .unwrap(),
                    expected,
                    "seed {seed}: index {table}"
                );
            }
            let present: Vec<bool> = (1..=3u32)
                .map(|t| {
                    s.get_at(
                        snap,
                        t,
                        &n.to_le_bytes(),
                        &lattice_engines::limits::AllowAll,
                    )
                    .unwrap()
                    .is_some()
                })
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

#[test]
fn index_snapshots_preserve_updates_deletes_and_zero_byte_keys_after_recovery() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    let mut tx = store.begin();
    for (key, value) in [
        (vec![0], vec![]),
        (vec![0, 0], vec![0]),
        (vec![1], vec![0]),
        (vec![255], vec![0, 0]),
    ] {
        tx.put((1, key), value);
    }
    store.commit(tx).unwrap();
    let old = store.snapshot();
    let mut tx = store.begin();
    tx.put((1, vec![0, 0]), vec![255]);
    tx.delete((1, vec![1]));
    store.commit(tx).unwrap();
    let current = store.snapshot();
    for reopen in 0..2 {
        assert_eq!(
            store
                .lookup_at(old, 1, &[], &lattice_engines::limits::AllowAll)
                .unwrap(),
            vec![vec![0]]
        );
        assert_eq!(
            store
                .lookup_at(old, 1, &[0], &lattice_engines::limits::AllowAll)
                .unwrap(),
            vec![vec![0, 0], vec![1]]
        );
        assert_eq!(
            store
                .lookup_at(old, 1, &[0, 0], &lattice_engines::limits::AllowAll)
                .unwrap(),
            vec![vec![255]]
        );
        assert!(
            store
                .lookup_at(current, 1, &[0], &lattice_engines::limits::AllowAll)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .lookup_at(current, 1, &[255], &lattice_engines::limits::AllowAll)
                .unwrap(),
            vec![vec![0, 0]]
        );
        store.check_indexes_at(old).unwrap();
        store.check_indexes_at(current).unwrap();
        drop(store);
        store = open(&disk, &clock, reopen + 100).unwrap();
    }
}

#[test]
fn duplicate_index_ids_are_rejected_before_opening_the_log() {
    use lattice_abi::{Classify, ErrorClass};
    use lattice_seams::Io;
    let (disk, clock) = fresh();
    let duplicate = IndexDef {
        id: 1,
        table: 1,
        extract: |v| Some(v.to_vec()),
    };
    let result = RowStore::open_with_indexes(
        disk.clone(),
        SharedClock(clock),
        SeededRng::new(1),
        &config(),
        vec![duplicate, duplicate],
    );
    let error = match result {
        Err(e) => e,
        Ok(_) => panic!("duplicate index ids were accepted"),
    };
    assert!(matches!(error, RowError::InvalidIndexes(_)));
    assert_eq!(error.class(), ErrorClass::Rejected);
    assert!(!disk.exists("wal"));
    assert!(open(&disk, &Rc::new(SimClock::new(1_000)), 2).is_ok());
}

#[test]
fn as_of_changeset_and_as_of_timestamp_name_points_in_history_and_survive_a_restart() {
    let (disk, clock) = fresh();
    let mut lsns = Vec::new();
    {
        let mut store = open(&disk, &clock, 1).unwrap();
        for (i, v) in [b"one".as_slice(), b"two", b"three"].iter().enumerate() {
            clock.advance(10);
            lsns.push(put(&mut store, 1, b"k", v));
            assert_eq!(lsns.len(), i + 1);
        }
        check_history(&store, &lsns);
    }
    // After a restart the same names resolve to the same snapshots.
    let store = open(&disk, &clock, 2).unwrap();
    check_history(&store, &lsns);
}

fn check_history(store: &Store, lsns: &[u64]) {
    let values: [&[u8]; 3] = [b"one", b"two", b"three"];
    for (lsn, want) in lsns.iter().zip(values) {
        // By change set: the snapshot sees that commit and nothing later.
        let id = store.changeset_id_at(*lsn).unwrap();
        let snap = store.snapshot_as_of_changeset(id).unwrap();
        assert_eq!(snap.as_of, *lsn);
        assert_eq!(
            store
                .get_at(snap, 1, b"k", &lattice_engines::limits::AllowAll)
                .unwrap(),
            Some(&want.to_vec())
        );
        // By timestamp: exactly at the commit, the commit is visible; a tick before, it is not.
        let t = store.commit_time_at(*lsn).unwrap();
        assert_eq!(store.snapshot_as_of_hlc(t).unwrap().as_of, *lsn);
        let earlier = lattice_abi::Hlc {
            counter: t.counter.saturating_sub(1),
            ..t
        };
        let before = store
            .snapshot_as_of_hlc(if t.counter == 0 {
                lattice_abi::Hlc {
                    wall_ms: t.wall_ms - 1,
                    counter: u32::MAX,
                    node: t.node,
                }
            } else {
                earlier
            })
            .unwrap();
        assert!(before.as_of < *lsn);
    }
    // Before the first commit there is nothing; after the last, everything.
    assert_eq!(
        store
            .snapshot_as_of_hlc(lattice_abi::Hlc::default())
            .unwrap()
            .as_of,
        0
    );
    let latest = store
        .snapshot_as_of_hlc(lattice_abi::Hlc {
            wall_ms: u64::MAX,
            counter: u32::MAX,
            node: u32::MAX,
        })
        .unwrap();
    assert_eq!(latest.as_of, *lsns.last().unwrap());
    // A change set that was never committed is not a point in history.
    assert!(
        store
            .snapshot_as_of_changeset(lattice_core::ChangeSetId::from_parts(1, 2, 3))
            .is_none()
    );
}
