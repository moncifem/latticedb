//! Ten distinct storage mutants, each killed by a public-operation witness and a clean control.
//! A setup error or panic fails the test; only the intended semantic mismatch kills a mutant.
//! This is E2-4 evidence, not fault-soak or Phase 2 exit evidence.
#![cfg(feature = "row")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_core::Config;
use lattice_core::bugs::{self, Bug};
use lattice_engines::index::IndexDef;
use lattice_engines::store::{RowError, RowStore};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;

struct Fixture {
    disk: SharedIo,
    clock: Rc<SimClock>,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let disk = SharedIo::new(SimIo::new(7, FaultProfile::clean()));
        let clock = Rc::new(SimClock::new(1_000));
        let store = Self::open(&disk, &clock, 10);
        Self { disk, clock, store }
    }

    fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64) -> Store {
        RowStore::open_with_indexes(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(seed),
            &Config {
                dir: "wal".into(),
                segment_limit: 600,
                node: 1,
            },
            vec![IndexDef {
                id: 9,
                table: 1,
                extract: |v| Some(v.to_vec()),
            }],
        )
        .expect("witness setup/recovery must succeed; an error is not mutant detection")
    }

    fn write(&mut self, writes: &[(&[u8], Option<&[u8]>)]) {
        let mut tx = self.store.begin();
        for (key, value) in writes {
            match value {
                Some(value) => tx.put((1, key.to_vec()), value.to_vec()),
                None => tx.delete((1, key.to_vec())),
            }
        }
        self.store.commit(tx).expect("witness commit must succeed");
    }

    fn reopen(self) -> Self {
        let Self { disk, clock, store } = self;
        drop(store);
        let store = Self::open(&disk, &clock, 11);
        Self { disk, clock, store }
    }
}

/// Thread-local guards are cleared on success and unwind, including a panicking setup operation.
struct Enabled;
impl Enabled {
    fn new(bug: Option<Bug>) -> Self {
        bugs::clear();
        if let Some(bug) = bug {
            bugs::enable(bug);
        }
        Self
    }
}
impl Drop for Enabled {
    fn drop(&mut self) {
        bugs::clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Violation {
    LostUpdate,
    PartialChangeSet,
    RecoveryResurrection,
    StaleIndex,
    MutableSnapshot,
    TombstoneResurrection,
    ReadOwnWrites,
    ExclusiveRange,
    MissingIndex,
    PrefixEquality,
}

fn witness(bug: Bug) -> Option<Violation> {
    let mut f = Fixture::new();
    match bug {
        Bug::RowConflictCheckSkipped => {
            f.write(&[(b"a", Some(b"old"))]);
            let mut stale = f.store.begin();
            stale.put((1, b"a".to_vec()), b"loser".to_vec());
            f.write(&[(b"a", Some(b"winner"))]);
            match f.store.commit(stale) {
                Err(RowError::Conflict { .. }) => None,
                Ok(_) => Some(Violation::LostUpdate),
                Err(e) => panic!("wrong conflict outcome: {e}"),
            }
        }
        Bug::RowPartialApply => {
            f.write(&[(b"a", Some(b"one")), (b"b", Some(b"two"))]);
            let expected = [
                (b"a".to_vec(), b"one".to_vec()),
                (b"b".to_vec(), b"two".to_vec()),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>();
            let got = f
                .store
                .scan_at(f.store.snapshot(), 1, ..)
                .unwrap()
                .into_iter()
                .map(|(k, v)| (k.to_vec(), v.clone()))
                .collect::<BTreeMap<_, _>>();
            (got != expected).then_some(Violation::PartialChangeSet)
        }
        Bug::RowRecoveryDropsDeletes => {
            f.write(&[(b"a", Some(b"old"))]);
            f.write(&[(b"a", None)]);
            // Validate that the deletion is visible before restart too.
            assert_eq!(f.store.get_at(f.store.snapshot(), 1, b"a").unwrap(), None);
            f = f.reopen();
            f.store
                .get_at(f.store.snapshot(), 1, b"a")
                .unwrap()
                .is_some()
                .then_some(Violation::RecoveryResurrection)
        }
        Bug::RowIndexStaleEntries => {
            f.write(&[(b"a", Some(b"old"))]);
            f.write(&[(b"a", Some(b"new"))]);
            let stale = f.store.lookup_at(f.store.snapshot(), 9, b"old").unwrap();
            let inconsistent = f.store.check_indexes().is_err();
            (!stale.is_empty() && inconsistent).then_some(Violation::StaleIndex)
        }
        Bug::RowHistoryOverwritten => {
            f.write(&[(b"a", Some(b"old"))]);
            let old = f.store.snapshot();
            f.write(&[(b"a", Some(b"new"))]);
            // Current state alone would miss this mutation.
            assert_eq!(
                f.store
                    .get_at(f.store.snapshot(), 1, b"a")
                    .unwrap()
                    .map(Vec::as_slice),
                Some(b"new".as_slice())
            );
            (f.store.get_at(old, 1, b"a").unwrap().map(Vec::as_slice) != Some(b"old".as_slice()))
                .then_some(Violation::MutableSnapshot)
        }
        Bug::RowTombstoneSkipped => {
            f.write(&[(b"a", Some(b"old"))]);
            let live = f.store.snapshot();
            f.write(&[(b"a", None)]);
            assert_eq!(
                f.store.get_at(live, 1, b"a").unwrap().map(Vec::as_slice),
                Some(b"old".as_slice())
            );
            f.store
                .get_at(f.store.snapshot(), 1, b"a")
                .unwrap()
                .is_some()
                .then_some(Violation::TombstoneResurrection)
        }
        Bug::RowReadOwnWriteIgnored => {
            f.write(&[(b"a", Some(b"old")), (b"b", Some(b"delete"))]);
            let snap = f.store.snapshot();
            let mut tx = f.store.begin();
            tx.put((1, b"a".to_vec()), b"own".to_vec());
            tx.delete((1, b"b".to_vec()));
            tx.put((1, b"c".to_vec()), b"insert".to_vec());
            let expected = [Some(b"own".as_slice()), None, Some(b"insert".as_slice())];
            let got =
                [b"a", b"b", b"c"].map(|k| f.store.get(&tx, 1, k).unwrap().map(Vec::as_slice));
            assert_eq!(f.store.snapshot(), snap, "buffered writes must not commit");
            (got != expected).then_some(Violation::ReadOwnWrites)
        }
        Bug::RowScanUpperBoundIncluded => {
            let rows = [
                (b"a".to_vec(), b"one".to_vec()),
                (b"b".to_vec(), b"two".to_vec()),
                (b"c".to_vec(), b"three".to_vec()),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>();
            for (k, v) in &rows {
                f.write(&[(k, Some(v))]);
            }
            let mut differs = false;
            // Present and absent bounds, excluded lower neighbor, empty and unbounded ranges.
            for (lo, hi) in [
                (
                    Bound::Included(b"a".to_vec()),
                    Bound::Excluded(b"c".to_vec()),
                ),
                (
                    Bound::Excluded(b"a".to_vec()),
                    Bound::Excluded(b"b".to_vec()),
                ),
                (
                    Bound::Included(b"a".to_vec()),
                    Bound::Excluded(b"bb".to_vec()),
                ),
                (Bound::Unbounded, Bound::Excluded(b"0".to_vec())),
                (Bound::Excluded(b"c".to_vec()), Bound::Unbounded),
            ] {
                let expected = rows
                    .range((lo.clone(), hi.clone()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>();
                let got = f
                    .store
                    .scan_at(f.store.snapshot(), 1, (lo, hi))
                    .unwrap()
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.clone()))
                    .collect::<Vec<_>>();
                differs |= got != expected;
            }
            differs.then_some(Violation::ExclusiveRange)
        }
        Bug::RowIndexMissingInsert => {
            f.write(&[(b"a", Some(b"same")), (b"b", Some(b"same"))]);
            let expected = vec![b"a".to_vec(), b"b".to_vec()];
            let missing = f.store.lookup_at(f.store.snapshot(), 9, b"same").unwrap() != expected;
            let inconsistent = f.store.check_indexes().is_err();
            (missing && inconsistent).then_some(Violation::MissingIndex)
        }
        Bug::RowIndexPrefixLookup => {
            let rows = [
                (b"pk0".to_vec(), vec![]),
                (b"pk1".to_vec(), b"a".to_vec()),
                (b"pk2".to_vec(), b"ab".to_vec()),
                (b"pk3".to_vec(), vec![0]),
                (b"pk4".to_vec(), vec![0, 1]),
                (b"pk5".to_vec(), b"abcd".to_vec()),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>();
            for (k, v) in &rows {
                f.write(&[(k, Some(v))]);
            }
            // Absent prefixes and neighbors must return no rows, even when longer keys exist.
            let mut differs = false;
            for probe in [
                vec![],
                b"a".to_vec(),
                b"ab".to_vec(),
                vec![0],
                vec![0, 1],
                b"p".to_vec(),
                b"ac".to_vec(),
                b"abc".to_vec(),
                vec![0, 0],
                vec![0, 2],
            ] {
                let expected = rows
                    .iter()
                    .filter(|(_, v)| **v == probe)
                    .map(|(k, _)| k.clone())
                    .collect::<Vec<_>>();
                differs |= f.store.lookup_at(f.store.snapshot(), 9, &probe).unwrap() != expected;
            }
            // The data and index maintenance agree: this defect is specifically the read predicate.
            assert!(f.store.check_indexes().is_ok());
            differs.then_some(Violation::PrefixEquality)
        }
        _ => panic!("not a storage witness: {bug:?}"),
    }
}

fn expected(bug: Bug) -> Violation {
    match bug {
        Bug::RowConflictCheckSkipped => Violation::LostUpdate,
        Bug::RowPartialApply => Violation::PartialChangeSet,
        Bug::RowRecoveryDropsDeletes => Violation::RecoveryResurrection,
        Bug::RowIndexStaleEntries => Violation::StaleIndex,
        Bug::RowHistoryOverwritten => Violation::MutableSnapshot,
        Bug::RowTombstoneSkipped => Violation::TombstoneResurrection,
        Bug::RowReadOwnWriteIgnored => Violation::ReadOwnWrites,
        Bug::RowScanUpperBoundIncluded => Violation::ExclusiveRange,
        Bug::RowIndexMissingInsert => Violation::MissingIndex,
        Bug::RowIndexPrefixLookup => Violation::PrefixEquality,
        _ => panic!("not a storage witness: {bug:?}"),
    }
}

#[test]
fn every_storage_witness_passes_without_a_mutant() {
    let _guard = Enabled::new(None);
    assert_eq!(Bug::STORAGE.len(), 10);
    for bug in Bug::STORAGE {
        assert_eq!(witness(bug), None, "clean control: {bug:?}");
    }
}

#[test]
fn ten_storage_mutants_fail_their_specific_semantic_oracle() {
    for bug in Bug::STORAGE {
        let _guard = Enabled::new(Some(bug));
        assert_eq!(witness(bug), Some(expected(bug)), "mutant: {bug:?}");
    }
}

#[test]
fn guard_scope_cleans_up_on_unwind() {
    let result = std::panic::catch_unwind(|| {
        let _guard = Enabled::new(Some(Bug::RowIndexMissingInsert));
        panic!("exercise cleanup");
    });
    assert!(result.is_err());
    for bug in Bug::STORAGE {
        assert!(!bugs::active(bug));
    }
    assert_eq!(witness(Bug::RowIndexMissingInsert), None);
}
