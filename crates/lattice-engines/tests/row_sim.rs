//! A model-checked simulation of the row store: several open transactions, readers holding old
//! snapshots, commits, conflicts and reopens, compared step by step with a plain map oracle.
//!
//! It checks the behaviour in `formats/isolation.md`: a transaction reads its snapshot plus its own
//! writes and never anything newer; a commit succeeds exactly when no row it wrote was committed after
//! its snapshot; a committed change set is visible whole (I3); a reader's snapshot never changes;
//! reopening reproduces what was committed. The planted row bugs must each be caught.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_core::bugs::{self, Bug};
use lattice_engines::mvcc::Snapshot;
use lattice_engines::rowops::RowKey;
use lattice_engines::store::{RowError, RowStore, RowTx};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::collections::BTreeMap;
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;
type State = BTreeMap<RowKey, Vec<u8>>;
type Writes = BTreeMap<RowKey, Option<Vec<u8>>>;

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 600,
        node: 1,
    }
}

/// What a store holds at one snapshot, read through the public scan.
fn scan_all(store: &Store, snap: Snapshot) -> State {
    let mut out = State::new();
    for table in 1..=3u32 {
        for (k, v) in store.scan_at(snap, table, ..) {
            out.insert((table, k.to_vec()), v.clone());
        }
    }
    out
}

struct Model {
    /// Committed change sets in commit order.
    history: Vec<(u64, Writes)>,
}

impl Model {
    fn state_at(&self, lsn: u64) -> State {
        let mut s = State::new();
        for (l, writes) in &self.history {
            if *l > lsn {
                break;
            }
            for (k, v) in writes {
                match v {
                    Some(v) => {
                        s.insert(k.clone(), v.clone());
                    }
                    None => {
                        s.remove(k);
                    }
                }
            }
        }
        s
    }

    fn newest_commit_of(&self, key: &RowKey) -> Option<u64> {
        self.history
            .iter()
            .rev()
            .find(|(_, w)| w.contains_key(key))
            .map(|(l, _)| *l)
    }
}

struct Open {
    tx: RowTx,
    snap: u64,
    writes: Writes,
}

fn run(seed: u64, steps: usize) -> Vec<String> {
    let mut rng = SeededRng::new(seed);
    let disk = SharedIo::new(SimIo::new(seed, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let open = |salt: u64| {
        RowStore::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(seed ^ salt),
            &config(),
        )
        .unwrap()
    };
    let mut store: Store = open(1);
    let mut model = Model { history: vec![] };
    let mut open_txs: Vec<Open> = Vec::new();
    let mut readers: Vec<Snapshot> = Vec::new();
    let mut violations: Vec<String> = Vec::new();
    let mut reopens = 0u64;

    for step in 0..steps {
        match rng.below(100) {
            0..=14 if open_txs.len() < 4 => {
                let tx = store.begin();
                let snap = tx.snapshot().as_of;
                open_txs.push(Open {
                    tx,
                    snap,
                    writes: Writes::new(),
                });
            }
            15..=44 if !open_txs.is_empty() => {
                let i = rng.below(open_txs.len() as u64) as usize;
                let o = &mut open_txs[i];
                for _ in 0..1 + rng.below(3) {
                    let key: RowKey = (1 + rng.below(3) as u32, vec![b'a' + rng.below(5) as u8]);
                    if rng.below(5) == 0 {
                        o.tx.delete(key.clone());
                        o.writes.insert(key, None);
                    } else {
                        let v = vec![step as u8, rng.below(256) as u8];
                        o.tx.put(key.clone(), v.clone());
                        o.writes.insert(key, Some(v));
                    }
                }
            }
            45..=64 if !open_txs.is_empty() => {
                let i = rng.below(open_txs.len() as u64) as usize;
                let o = &open_txs[i];
                let key: RowKey = (1 + rng.below(3) as u32, vec![b'a' + rng.below(5) as u8]);
                let expected = match o.writes.get(&key) {
                    Some(own) => own.clone(),
                    None => model.state_at(o.snap).get(&key).cloned(),
                };
                let got = store.get(&o.tx, key.0, &key.1).cloned();
                if got != expected {
                    violations.push(format!(
                        "I3: seed {seed} step {step}: a read saw {got:?}, snapshot {} holds {expected:?}",
                        o.snap
                    ));
                }
            }
            65..=84 if !open_txs.is_empty() => {
                let i = rng.below(open_txs.len() as u64) as usize;
                let o = open_txs.remove(i);
                let conflicts = o
                    .writes
                    .keys()
                    .any(|k| model.newest_commit_of(k).is_some_and(|l| l > o.snap));
                match store.commit(o.tx) {
                    Ok(lsn) => {
                        if !o.writes.is_empty() {
                            if conflicts {
                                violations.push(format!(
                                    "lost update: seed {seed} step {step}: a conflicting commit succeeded"
                                ));
                            }
                            model.history.push((lsn, o.writes));
                            let seen = scan_all(&store, store.snapshot());
                            if seen != model.state_at(lsn) {
                                violations.push(format!(
                                    "I3: seed {seed} step {step}: change set at lsn {lsn} is not visible whole"
                                ));
                            }
                        }
                    }
                    Err(RowError::Conflict { .. }) => {
                        if !conflicts {
                            violations.push(format!("spurious conflict: seed {seed} step {step}"));
                        }
                    }
                    Err(e) => violations.push(format!("seed {seed} step {step}: {e}")),
                }
            }
            85..=92 => readers.push(store.snapshot()),
            93..=95 => {
                // Restart: open transactions and snapshots die with the process; the log decides.
                open_txs.clear();
                readers.clear();
                drop(store);
                reopens += 1;
                store = open(1000 + reopens);
                let latest = model.history.last().map_or(0, |(l, _)| *l);
                if scan_all(&store, store.snapshot()) != model.state_at(latest) {
                    violations.push(format!(
                        "recovery: seed {seed} step {step}: reopened rows differ from the committed state"
                    ));
                }
            }
            _ => {}
        }
        // A reader's snapshot never changes, however many commits land after it.
        for snap in &readers {
            if scan_all(&store, *snap) != model.state_at(snap.as_of) {
                violations.push(format!(
                    "I3: seed {seed} step {step}: the snapshot at lsn {} changed",
                    snap.as_of
                ));
                break;
            }
        }
    }
    violations
}

#[test]
fn the_clean_row_store_matches_the_model_over_many_seeds() {
    for seed in 0..300 {
        let v = run(seed, 140);
        assert!(v.is_empty(), "{:?}", v.first());
    }
}

#[test]
fn each_planted_row_bug_is_caught() {
    for bug in Bug::ROW {
        bugs::enable(bug);
        let caught = (0..300u64).find_map(|seed| run(seed, 140).into_iter().next());
        bugs::clear();
        assert!(caught.is_some(), "{bug:?} went undetected");
    }
}
