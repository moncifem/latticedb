//! A seeded workload for the row store, checked against a model that keeps the whole row map at
//! every commit. It looks for the failures the row planted bugs cause: a lost update, a change set
//! that is only partly visible (I3), and rows that come back or vanish after a restart (I2).
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
            let got = store.lookup_at(snap, table, &[value]);
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
            let got = store.get_at(snap, table, &[key]);
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
