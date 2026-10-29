//! A seeded workload for the row store, checked against a model that keeps the whole row map at
//! every commit. It looks for the failures the row planted bugs cause: a lost update, a change set
//! that is only partly visible (I3), and rows that come back or vanish after a restart (I2).
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_core::bugs::{self, Bug};
use lattice_engines::index::IndexDef;
use lattice_engines::mvcc::Snapshot;
use lattice_engines::store::{RowError, RowStore};
use lattice_seams::Rng;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::collections::BTreeMap;
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;
type Rows = BTreeMap<(u32, Vec<u8>), Vec<u8>>;

const TABLES: u32 = 3;
const KEYS: u8 = 4;

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 600,
        node: 1,
    }
}

fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64) -> Store {
    RowStore::open_with_indexes(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
        (0..TABLES)
            .map(|table| IndexDef {
                id: table,
                table,
                extract: |value| value.first().map(|v| vec![*v]),
            })
            .collect(),
    )
    .unwrap()
}

type Write = ((u32, Vec<u8>), Option<Vec<u8>>);

fn random_writes(rng: &mut SeededRng, n: u64) -> Vec<Write> {
    (0..n)
        .map(|_| {
            let table = rng.below(u64::from(TABLES)) as u32;
            let key = vec![rng.below(u64::from(KEYS)) as u8];
            let value = if rng.below(4) == 0 {
                None
            } else {
                Some(vec![rng.below(250) as u8, (rng.next_u64() & 0xFF) as u8])
            };
            ((table, key), value)
        })
        .collect()
}

fn fold(rows: &mut Rows, writes: &[Write]) {
    for (k, v) in writes {
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

fn stage(store: &Store, writes: &[Write]) -> lattice_engines::store::RowTx {
    let mut tx = store.begin();
    for (k, v) in writes {
        match v {
            Some(v) => tx.put(k.clone(), v.clone()),
            None => tx.delete(k.clone()),
        }
    }
    tx
}

/// What the store shows for every key at `snap`, compared with the model; the first difference.
fn compare(store: &Store, snap: Snapshot, model: &Rows) -> Option<String> {
    if let Err(why) = store.check_indexes_at(snap) {
        return Some(format!("I9 at snapshot {}: {why}", snap.as_of));
    }
    for table in 0..TABLES {
        for value in model
            .values()
            .filter_map(|v| v.first())
            .copied()
            .chain([0, 249])
        {
            let expected: Vec<Vec<u8>> = model
                .iter()
                .filter(|((t, _), v)| *t == table && v.first() == Some(&value))
                .map(|((_, key), _)| key.clone())
                .collect();
            let got = store.lookup_at(snap, table, &[value]).unwrap();
            if got != expected {
                return Some(format!(
                    "I9 at snapshot {}: index {table} value {value}, got {got:?}, expected {expected:?}",
                    snap.as_of
                ));
            }
        }
    }
    for table in 0..TABLES {
        for key in 0..KEYS {
            let want = model.get(&(table, vec![key]));
            let got = store.get_at(snap, table, &[key]).unwrap();
            if got != want {
                return Some(format!(
                    "at snapshot {} table {table} key {key}: store {got:?}, model {want:?}",
                    snap.as_of
                ));
            }
        }
    }
    None
}

/// Runs one seeded workload and returns the first violation, if any.
fn run(seed: u64, steps: usize) -> Option<String> {
    let disk = SharedIo::new(SimIo::new(seed, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let mut rng = SeededRng::new(seed ^ 0x5eed);
    let mut store = open(&disk, &clock, seed);
    let mut reopen_seed = seed.wrapping_mul(31);
    // history[i] = (commit lsn, the whole map right after it)
    let mut history: Vec<(u64, Rows)> = vec![(0, Rows::new())];
    let mut snapshots: Vec<Snapshot> = vec![];
    let model_at = |history: &Vec<(u64, Rows)>, lsn: u64| -> Rows {
        history
            .iter()
            .rev()
            .find(|(l, _)| *l <= lsn)
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    };
    for step in 0..steps {
        match rng.below(100) {
            0..=44 => {
                let n = 1 + rng.below(3);
                let writes = random_writes(&mut rng, n);
                let tx = stage(&store, &writes);
                match store.commit(tx) {
                    Ok(lsn) => {
                        let mut next = history.last().unwrap().1.clone();
                        fold(&mut next, &writes);
                        history.push((lsn, next));
                    }
                    Err(e) => return Some(format!("step {step}: a lone commit failed: {e:?}")),
                }
                if let Some(d) = compare(&store, store.snapshot(), &history.last().unwrap().1) {
                    return Some(format!("step {step}: right after a commit, {d}"));
                }
                if rng.below(3) == 0 {
                    snapshots.push(store.snapshot());
                }
            }
            45..=69 => {
                // Two overlapping transactions: the second committer conflicts iff they share a row.
                let (na, nb) = (1 + rng.below(2), 1 + rng.below(2));
                let a = random_writes(&mut rng, na);
                let b = random_writes(&mut rng, nb);
                let (ta, tb) = (stage(&store, &a), stage(&store, &b));
                let shared = a.iter().any(|(ka, _)| b.iter().any(|(kb, _)| ka == kb));
                match store.commit(ta) {
                    Ok(lsn) => {
                        let mut next = history.last().unwrap().1.clone();
                        fold(&mut next, &a);
                        history.push((lsn, next));
                    }
                    Err(e) => return Some(format!("step {step}: first committer failed: {e:?}")),
                }
                if let Some(d) = compare(&store, store.snapshot(), &history.last().unwrap().1) {
                    return Some(format!("step {step}: right after a commit, {d}"));
                }
                match (store.commit(tb), shared) {
                    (Ok(lsn), false) => {
                        let mut next = history.last().unwrap().1.clone();
                        fold(&mut next, &b);
                        history.push((lsn, next));
                    }
                    (Err(RowError::Conflict { .. }), true) => {}
                    (Ok(_), true) => {
                        return Some(format!(
                            "step {step}: two transactions wrote the same row and both committed (lost update)"
                        ));
                    }
                    (Err(e), false) => {
                        return Some(format!("step {step}: a disjoint writer was refused: {e:?}"));
                    }
                    (Err(e), true) => {
                        return Some(format!("step {step}: wrong error for a conflict: {e:?}"));
                    }
                }
            }
            70..=89 => {
                // Every remembered snapshot must still show exactly its model.
                let mut all = snapshots.clone();
                all.push(store.snapshot());
                for snap in all {
                    if let Some(d) = compare(&store, snap, &model_at(&history, snap.as_of)) {
                        return Some(format!("step {step}: {d}"));
                    }
                }
            }
            _ => {
                drop(store);
                reopen_seed = reopen_seed.wrapping_add(1);
                store = open(&disk, &clock, reopen_seed);
                let now = store.snapshot();
                if let Some(d) = compare(&store, now, &history.last().unwrap().1) {
                    return Some(format!("step {step}: after reopening, {d}"));
                }
            }
        }
    }
    None
}

#[test]
fn the_clean_row_store_passes_every_seed() {
    for seed in 0..120 {
        if let Some(v) = run(seed, 80) {
            panic!("seed {seed}: {v}");
        }
    }
}

#[test]
fn each_row_planted_bug_is_detected() {
    for bug in Bug::ROW {
        bugs::enable(bug);
        let found = (0..120).find_map(|seed| run(seed, 80).map(|v| (seed, v)));
        bugs::clear();
        let (seed, why) = found.unwrap_or_else(|| panic!("{bug:?} was not detected in 120 seeds"));
        eprintln!("{bug:?}: caught at seed {seed}: {why}");
    }
}

fn model_at(history: &[(u64, Rows)], lsn: u64) -> Rows {
    history
        .iter()
        .rev()
        .find(|(l, _)| *l <= lsn)
        .map(|(_, m)| m.clone())
        .unwrap_or_default()
}

/// Commits, vacuums at random horizons, restarts, and after each step checks every snapshot at or
/// after the vacuum floor against the model. Returns the first violation.
fn run_vacuum(seed: u64, steps: usize) -> Option<String> {
    let disk = SharedIo::new(SimIo::new(seed, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let mut rng = SeededRng::new(seed ^ 0x7ac0);
    let mut store = open(&disk, &clock, seed);
    let mut reopen_seed = seed.wrapping_mul(17);
    let mut history: Vec<(u64, Rows)> = vec![(0, Rows::new())];
    let mut snapshots: Vec<Snapshot> = vec![];
    let mut floor = 0u64;
    let mut last_horizon = 0u64;
    let check = |store: &Store, snapshots: &[Snapshot], floor: u64, history: &[(u64, Rows)]| {
        let mut all: Vec<Snapshot> = snapshots
            .iter()
            .copied()
            .filter(|s| s.as_of >= floor)
            .collect();
        all.push(Snapshot { as_of: floor });
        all.push(store.snapshot());
        all.into_iter()
            .find_map(|snap| compare(store, snap, &model_at(history, snap.as_of)))
    };
    for step in 0..steps {
        match rng.below(100) {
            0..=54 => {
                let n = 1 + rng.below(3);
                let writes = random_writes(&mut rng, n);
                let tx = stage(&store, &writes);
                match store.commit(tx) {
                    Ok(lsn) => {
                        let mut next = history.last().unwrap().1.clone();
                        fold(&mut next, &writes);
                        history.push((lsn, next));
                    }
                    Err(e) => return Some(format!("step {step}: a lone commit failed: {e:?}")),
                }
                if rng.below(2) == 0 {
                    snapshots.push(store.snapshot());
                }
            }
            55..=84 => {
                let applied = store.applied_lsn();
                let horizon = rng.below(applied + 1);
                let mut stale = store.begin();
                let stats = store.vacuum(horizon);
                floor = floor.max(horizon.min(applied));
                last_horizon = floor;
                if stats.horizon != floor || store.oldest_snapshot().as_of != floor {
                    return Some(format!("step {step}: floor {floor}, store says {stats:?}"));
                }
                if let Some(d) = check(&store, &snapshots, floor, &history) {
                    return Some(format!("step {step}: after vacuum({horizon}), {d}"));
                }
                // A transaction begun before the vacuum is fine at the floor or above it; below it
                // the store must refuse it rather than misread.
                stale.put((0, vec![0]), vec![1, 1]);
                let before = stale.snapshot().as_of;
                match store.commit(stale) {
                    Err(RowError::SnapshotTooOld { .. }) if before < floor => {}
                    Err(RowError::SnapshotTooOld { .. }) => {
                        return Some(format!(
                            "step {step}: snapshot {before} refused at floor {floor}"
                        ));
                    }
                    Err(RowError::Conflict { .. }) => {}
                    Ok(_) if before < floor => {
                        return Some(format!("step {step}: committed below the floor"));
                    }
                    Ok(lsn) => {
                        let mut next = history.last().unwrap().1.clone();
                        fold(&mut next, &[((0, vec![0]), Some(vec![1, 1]))]);
                        history.push((lsn, next));
                    }
                    Err(e) => return Some(format!("step {step}: unexpected {e:?}")),
                }
            }
            _ => {
                drop(store);
                reopen_seed = reopen_seed.wrapping_add(1);
                store = open(&disk, &clock, reopen_seed);
                // The log keeps all history, so every remembered snapshot reads again.
                if let Some(d) = check(&store, &snapshots, 0, &history) {
                    return Some(format!("step {step}: after reopening, {d}"));
                }
                // Replay then vacuum must match the model at the old horizon too.
                store.vacuum(last_horizon);
                floor = last_horizon.min(store.applied_lsn());
                if let Some(d) = check(&store, &snapshots, floor, &history) {
                    return Some(format!("step {step}: after reopening and vacuuming, {d}"));
                }
            }
        }
    }
    None
}

#[test]
fn vacuum_keeps_every_snapshot_at_or_after_the_horizon_on_every_seed() {
    for seed in 0..120 {
        if let Some(v) = run_vacuum(seed, 80) {
            panic!("seed {seed}: {v}");
        }
    }
}

#[test]
fn each_vacuum_planted_bug_is_detected() {
    for bug in Bug::VACUUM {
        bugs::enable(bug);
        let found = (0..120).find_map(|seed| run_vacuum(seed, 80).map(|v| (seed, v)));
        bugs::clear();
        let (seed, why) = found.unwrap_or_else(|| panic!("{bug:?} was not detected in 120 seeds"));
        eprintln!("{bug:?}: caught at seed {seed}: {why}");
    }
}

#[test]
fn vacuum_is_bounded_by_open_transactions_and_retention_and_expires_as_of() {
    let disk = SharedIo::new(SimIo::new(3, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let mut store = open(&disk, &clock, 3);
    let mut ids = vec![];
    for i in 0..6u8 {
        let mut tx = store.begin();
        tx.put((0, vec![0]), vec![i, 0]);
        let lsn = store.commit(tx).unwrap();
        ids.push((lsn, store.changeset_id_at(lsn).unwrap()));
    }
    let applied = store.applied_lsn();
    let open_tx = {
        let mut tx = store.begin();
        tx.put((0, vec![1]), vec![9, 9]);
        tx
    };
    assert_eq!(store.vacuum_horizon(None, 2), applied - 2);
    assert_eq!(store.vacuum_horizon(Some(Snapshot { as_of: 1 }), 2), 1);
    assert_eq!(store.vacuum_horizon(None, 100), 0);
    let stats = store.vacuum(applied - 2);
    assert_eq!(stats.horizon, applied - 2);
    assert!(stats.row_versions > 0 && stats.index_versions > 0);
    // Inside the window AS OF still resolves and reads; outside it expires instead of misreading.
    let (keep_lsn, keep_id) = ids[ids.len() - 1];
    assert_eq!(
        store.snapshot_as_of_changeset(keep_id),
        Some(Snapshot { as_of: keep_lsn })
    );
    let (old_lsn, old_id) = ids[0];
    assert!(old_lsn < applied - 2);
    assert_eq!(store.snapshot_as_of_changeset(old_id), None);
    assert!(matches!(
        store.check_snapshot(Snapshot { as_of: old_lsn }),
        Err(RowError::SnapshotTooOld { .. })
    ));
    assert!(
        store
            .check_snapshot(Snapshot { as_of: applied - 2 })
            .is_ok()
    );
    // A transaction opened at the newest snapshot is unaffected.
    assert!(store.commit(open_tx).is_ok());
    // A restart brings the whole history back.
    drop(store);
    let store = open(&disk, &clock, 4);
    assert!(store.snapshot_as_of_changeset(old_id).is_some());
    assert_eq!(
        store.get_at(Snapshot { as_of: old_lsn }, 0, &[0]).unwrap(),
        Some(&vec![0, 0])
    );
}
