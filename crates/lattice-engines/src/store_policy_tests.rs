//! Policy expectations are folded from literal rows, independently of the store and its indexes.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use crate::index::IndexDef;
use crate::limits::{AllowAll, DenyAll, PolicyEvaluator, RowRef, RowWriteRef};
use crate::mvcc::Snapshot;
use crate::store::{RowError, RowStore};
use lattice_abi::{Classify, ErrorClass};
use lattice_core::{
    Config,
    bugs::{self, Bug},
};
use lattice_seams::Io;
use lattice_sim::{FaultProfile, SimClock};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use proptest::prelude::*;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};

type Store = RowStore<SharedIo, SharedClock, CountRng>;
type Rows = BTreeMap<Vec<u8>, Vec<u8>>;
struct CountRng {
    calls: Rc<Cell<u64>>,
}
impl lattice_seams::Rng for CountRng {
    fn next_u64(&mut self) -> u64 {
        self.calls.set(self.calls.get() + 1);
        self.calls.get()
    }
}
fn open(disk: &SharedIo, clock: &Rc<SimClock>, calls: &Rc<Cell<u64>>) -> Store {
    RowStore::open_with_indexes(
        disk.clone(),
        SharedClock(clock.clone()),
        CountRng {
            calls: calls.clone(),
        },
        &Config {
            segment_limit: 256,
            ..Config::default()
        },
        vec![
            IndexDef {
                id: 9,
                table: 1,
                extract: |_| Some(b"all".to_vec()),
            },
            IndexDef {
                id: 10,
                table: 2,
                extract: |_| Some(b"all".to_vec()),
            },
        ],
    )
    .unwrap()
}
struct Fixture {
    disk: SharedIo,
    clock: Rc<SimClock>,
    calls: Rc<Cell<u64>>,
    store: Store,
}
impl Fixture {
    fn empty() -> Self {
        let disk = SharedIo::new(lattice_sim::SimIo::new(17, FaultProfile::clean()));
        let clock = Rc::new(SimClock::new(1_000));
        let calls = Rc::new(Cell::new(0));
        let store = open(&disk, &clock, &calls);
        Self {
            disk,
            clock,
            calls,
            store,
        }
    }
    fn populated() -> Self {
        let mut f = Self::empty();
        let mut tx = f.store.begin();
        for (table, key, value) in [
            (1, b"a", [1, 10]),
            (1, b"b", [2, 20]),
            (1, b"c", [1, 30]),
            (2, b"z", [1, 40]),
        ] {
            tx.put((table, key.to_vec()), value.to_vec());
        }
        f.store.commit(tx).unwrap();
        f
    }
}
#[derive(Clone, Copy)]
struct Owner(u8);
impl PolicyEvaluator for Owner {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        row.table == 1 && row.value.first() == Some(&self.0)
    }
    fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
        row.table == 1
            && row.before.is_none_or(|v| v.first() == Some(&self.0))
            && row.after.is_none_or(|v| v.first() == Some(&self.0))
    }
}
fn rows(store: &Store, snap: Snapshot, policy: &impl PolicyEvaluator) -> Rows {
    store
        .scan_at(snap, 1, .., policy)
        .unwrap()
        .into_iter()
        .map(|(k, v)| (k.to_vec(), v.clone()))
        .collect()
}
fn wal(disk: &SharedIo) -> BTreeMap<String, Vec<u8>> {
    disk.list("wal")
        .unwrap()
        .into_iter()
        .filter(|p| p.ends_with(".seg"))
        .map(|p| {
            let bytes = disk.read(&format!("wal/{p}")).unwrap();
            (p, bytes)
        })
        .collect()
}

#[test]
fn deny_all_hides_points_scans_index_keys_and_transaction_local_values() {
    let f = Fixture::populated();
    let snap = f.store.snapshot();
    assert_eq!(f.store.get_at(snap, 1, b"a", &DenyAll).unwrap(), None);
    assert_eq!(rows(&f.store, snap, &DenyAll), Rows::new());
    assert!(
        f.store
            .lookup_at(snap, 9, b"all", &DenyAll)
            .unwrap()
            .is_empty()
    );
    assert!(
        f.store
            .lookup_at(snap, 10, b"all", &DenyAll)
            .unwrap()
            .is_empty()
    );
    let mut tx = f.store.begin_with_policy(DenyAll);
    tx.put((1, b"own".to_vec()), vec![1, 99]);
    assert_eq!(f.store.get(&tx, 1, b"a").unwrap(), None);
    assert_eq!(f.store.get(&tx, 1, b"own").unwrap(), None);
    assert!(
        f.store.check_indexes().is_ok(),
        "RLS must not filter the scrubber's complete state"
    );
    assert_eq!(rows(&f.store, snap, &AllowAll).len(), 3);
}

#[test]
fn actor_contexts_share_the_store_without_leaking_between_statements_or_snapshots() {
    let mut f = Fixture::populated();
    let old = f.store.snapshot();
    let first = Owner(1);
    let second = Owner(2);
    let tx = f.store.begin_with_policy(&first);
    let mut admin = f.store.begin();
    admin.put((1, b"a".to_vec()), vec![2, 11]);
    f.store.commit(admin).unwrap();
    assert_eq!(f.store.get(&tx, 1, b"a").unwrap(), Some(&vec![1, 10]));
    assert_eq!(f.store.get_at(old, 1, b"b", &first).unwrap(), None);
    assert_eq!(
        rows(&f.store, old, &first),
        [(b"a".to_vec(), vec![1, 10]), (b"c".to_vec(), vec![1, 30])].into()
    );
    assert_eq!(
        rows(&f.store, f.store.snapshot(), &first),
        [(b"c".to_vec(), vec![1, 30])].into()
    );
    assert_eq!(
        rows(&f.store, f.store.snapshot(), &second),
        [(b"a".to_vec(), vec![2, 11]), (b"b".to_vec(), vec![2, 20])].into()
    );
    assert_eq!(
        f.store.lookup_at(old, 9, b"all", &first).unwrap(),
        vec![b"a".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        f.store
            .lookup_at(f.store.snapshot(), 9, b"all", &first)
            .unwrap(),
        vec![b"c".to_vec()]
    );
    assert!(
        f.store
            .lookup_at(f.store.snapshot(), 10, b"all", &first)
            .unwrap()
            .is_empty(),
        "authorize the index's real table"
    );
    assert_eq!(
        f.store.get_at(f.store.snapshot(), 2, b"z", &first).unwrap(),
        None
    );
}

#[test]
fn denial_rejects_the_whole_batch_before_id_or_wal_changes_and_recovery_is_unfiltered() {
    let mut f = Fixture::populated();
    let before = wal(&f.disk);
    let lsn = f.store.applied_lsn();
    let calls = f.calls.get();
    let mut tx = f.store.begin_with_policy(Owner(1));
    tx.put((1, b"a".to_vec()), vec![1, 11]); // Allowed, but must not publish if another row is denied.
    tx.delete((1, b"b".to_vec()));
    tx.put((1, b"new".to_vec()), vec![1, 99]);
    let error = f.store.commit(tx).unwrap_err();
    assert_eq!(error, RowError::PermissionDenied);
    assert_eq!(error.class(), ErrorClass::Rejected);
    assert_eq!(
        f.calls.get(),
        calls,
        "denial must not allocate a change-set id"
    );
    assert_eq!(wal(&f.disk), before);
    assert_eq!(f.store.applied_lsn(), lsn);
    assert_eq!(
        rows(&f.store, f.store.snapshot(), &AllowAll),
        [
            (b"a".to_vec(), vec![1, 10]),
            (b"b".to_vec(), vec![2, 20]),
            (b"c".to_vec(), vec![1, 30])
        ]
        .into()
    );
    drop(f.store);
    f.disk.crash();
    let store = open(&f.disk, &f.clock, &f.calls);
    assert_eq!(rows(&store, store.snapshot(), &DenyAll), Rows::new());
    assert_eq!(
        store
            .lookup_at(store.snapshot(), 9, b"all", &AllowAll)
            .unwrap(),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    store.check_indexes().unwrap();
}

#[test]
fn write_policy_checks_original_and_final_values_including_inserts_and_deletes() {
    let mut f = Fixture::populated();
    let mut allowed = f.store.begin_with_policy(Owner(1));
    allowed.put((1, b"a".to_vec()), vec![1, 12]);
    allowed.delete((1, b"c".to_vec()));
    allowed.put((1, b"new".to_vec()), vec![1, 99]);
    assert_eq!(f.store.get(&allowed, 1, b"a").unwrap(), Some(&vec![1, 12]));
    assert_eq!(f.store.get(&allowed, 1, b"c").unwrap(), None);
    f.store.commit(allowed).unwrap();
    assert_eq!(
        rows(&f.store, f.store.snapshot(), &Owner(1)),
        [(b"a".to_vec(), vec![1, 12]), (b"new".to_vec(), vec![1, 99])].into()
    );
    for (key, after) in [
        (b"a".as_slice(), vec![2, 0]),
        (b"b".as_slice(), vec![1, 0]),
        (b"insert".as_slice(), vec![2, 0]),
    ] {
        let before = wal(&f.disk);
        let mut denied = f.store.begin_with_policy(Owner(1));
        denied.put((1, key.to_vec()), after);
        assert_eq!(f.store.commit(denied), Err(RowError::PermissionDenied));
        assert_eq!(wal(&f.disk), before);
    }
    struct ReadOnly;
    impl PolicyEvaluator for ReadOnly {
        fn allows_read(&self, _: RowRef<'_>) -> bool {
            true
        }
    }
    let mut denied = f.store.begin_with_policy(ReadOnly);
    denied.delete((1, b"a".to_vec()));
    assert_eq!(f.store.commit(denied), Err(RowError::PermissionDenied));
}

#[test]
fn floor_errors_win_before_authorization_and_denial_wins_before_conflict() {
    struct Audit(Rc<Cell<u64>>);
    impl PolicyEvaluator for Audit {
        fn allows_write(&self, _: RowWriteRef<'_>) -> bool {
            self.0.set(self.0.get() + 1);
            false
        }
    }
    let mut f = Fixture::populated();
    let calls = Rc::new(Cell::new(0));
    let mut stale = f.store.begin_with_policy(Audit(calls.clone()));
    stale.put((1, b"a".to_vec()), vec![9]);
    let mut other = f.store.begin();
    other.put((1, b"a".to_vec()), vec![2, 11]);
    let winner = f.store.commit(other).unwrap();
    let before = wal(&f.disk);
    // Authorization runs before the conflict check: the denied actor learns nothing about the
    // newer write at `winner`.
    let _ = winner;
    assert_eq!(f.store.commit(stale), Err(RowError::PermissionDenied));
    assert_eq!(calls.get(), 1);
    assert_eq!(wal(&f.disk), before);
    calls.set(0);
    let old = Snapshot { as_of: 0 };
    let mut expired = f.store.begin_with_policy(Audit(calls.clone()));
    expired.put((1, b"x".to_vec()), vec![9]);
    let read_only = f.store.begin_with_policy(DenyAll);
    let mut other = f.store.begin();
    other.put((1, b"a".to_vec()), vec![2, 12]);
    let floor = f.store.commit(other).unwrap();
    f.store.vacuum(floor);
    let before = wal(&f.disk);
    assert_eq!(
        f.store.get_at(old, 1, b"missing", &DenyAll),
        Err(RowError::SnapshotTooOld { floor })
    );
    assert_eq!(
        f.store.scan_at(old, 1, .., &DenyAll),
        Err(RowError::SnapshotTooOld { floor })
    );
    assert_eq!(
        f.store.lookup_at(old, 9, b"all", &DenyAll),
        Err(RowError::SnapshotTooOld { floor })
    );
    assert_eq!(
        f.store.get(&expired, 1, b"x"),
        Err(RowError::SnapshotTooOld { floor })
    );
    assert_eq!(
        f.store.commit(expired),
        Err(RowError::SnapshotTooOld { floor })
    );
    assert_eq!(
        f.store.commit(read_only),
        Err(RowError::SnapshotTooOld { floor })
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(wal(&f.disk), before);
}

#[test]
fn policies_receive_the_real_key_and_complete_final_transition() {
    struct KeyOnly;
    impl PolicyEvaluator for KeyOnly {
        fn allows_read(&self, row: RowRef<'_>) -> bool {
            row.table == 1 && row.key == b"a"
        }
        fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
            row.table == 1 && row.key == b"a"
        }
    }
    let mut f = Fixture::populated();
    let snap = f.store.snapshot();
    assert_eq!(
        rows(&f.store, snap, &KeyOnly),
        [(b"a".to_vec(), vec![1, 10])].into()
    );
    assert_eq!(
        f.store.lookup_at(snap, 9, b"all", &KeyOnly).unwrap(),
        vec![b"a".to_vec()]
    );
    assert_eq!(f.store.get_at(snap, 1, b"c", &KeyOnly).unwrap(), None);
    let mut denied = f.store.begin_with_policy(KeyOnly);
    denied.put((1, b"a".to_vec()), vec![1, 11]);
    denied.put((1, b"c".to_vec()), vec![1, 31]);
    let before = wal(&f.disk);
    assert_eq!(f.store.commit(denied), Err(RowError::PermissionDenied));
    assert_eq!(wal(&f.disk), before);

    type Transition = (u32, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);
    struct Audit(std::cell::RefCell<Vec<Transition>>);
    impl PolicyEvaluator for Audit {
        fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
            self.0.borrow_mut().push((
                row.table,
                row.key.to_vec(),
                row.before.map(<[u8]>::to_vec),
                row.after.map(<[u8]>::to_vec),
            ));
            true
        }
    }
    let audit = Audit(std::cell::RefCell::new(Vec::new()));
    let mut tx = f.store.begin_with_policy(&audit);
    tx.put((1, b"a".to_vec()), vec![1, 11]);
    tx.put((1, b"a".to_vec()), vec![1, 12]);
    tx.delete((1, b"b".to_vec()));
    tx.put((1, b"new".to_vec()), vec![1, 99]);
    f.store.commit(tx).unwrap();
    assert_eq!(
        *audit.0.borrow(),
        vec![
            (1, b"a".to_vec(), Some(vec![1, 10]), Some(vec![1, 12])),
            (1, b"b".to_vec(), Some(vec![2, 20]), None),
            (1, b"new".to_vec(), None, Some(vec![1, 99])),
        ]
    );
}

#[test]
fn stale_store_refuses_before_consulting_the_policy_or_allocating_an_id() {
    struct Audit(Rc<Cell<u64>>);
    impl PolicyEvaluator for Audit {
        fn allows_write(&self, _: RowWriteRef<'_>) -> bool {
            self.0.set(self.0.get() + 1);
            false
        }
    }
    let mut f = Fixture::populated();
    f.disk.set_profile(FaultProfile {
        sync_error_ppm: 1_000_000,
        ..FaultProfile::clean()
    });
    let mut in_doubt = f.store.begin();
    in_doubt.put((1, b"uncertain".to_vec()), vec![3]);
    assert!(matches!(
        f.store.commit(in_doubt),
        Err(RowError::InDoubt(_))
    ));
    f.disk.set_profile(FaultProfile::clean());
    let calls = Rc::new(Cell::new(0));
    let mut tx = f.store.begin_with_policy(Audit(calls.clone()));
    tx.put((1, b"denied".to_vec()), vec![9]);
    let before = wal(&f.disk);
    let rng_calls = f.calls.get();
    assert_eq!(f.store.commit(tx), Err(RowError::Stale));
    assert_eq!(calls.get(), 0);
    assert_eq!(f.calls.get(), rng_calls);
    assert_eq!(wal(&f.disk), before);
}

#[test]
fn authorization_does_not_silently_repair_an_index_reference_to_a_deleted_row() {
    let mut f = Fixture::populated();
    {
        let _guard = Enabled::new(Bug::RowIndexStaleEntries);
        let mut tx = f.store.begin();
        tx.delete((1, b"a".to_vec()));
        f.store.commit(tx).unwrap();
    }
    assert!(matches!(
        f.store.lookup_at(f.store.snapshot(), 9, b"all", &AllowAll),
        Err(RowError::Corrupt(_))
    ));
    assert!(
        f.store.check_indexes().is_err(),
        "the raw consistency oracle must still see the stale entry"
    );
}

struct Enabled;
impl Enabled {
    fn new(bug: Bug) -> Self {
        bugs::enable(bug);
        Self
    }
}
impl Drop for Enabled {
    fn drop(&mut self) {
        bugs::clear();
    }
}
#[derive(Debug, PartialEq)]
enum Violation {
    RowExposed,
    DeniedWritePublished,
}
fn read_witness(f: &Fixture) -> Option<Violation> {
    let snap = f.store.snapshot();
    let mut tx = f.store.begin_with_policy(DenyAll);
    tx.put((1, b"own".to_vec()), vec![1, 55]);
    let wrong = f.store.get_at(snap, 1, b"a", &DenyAll).unwrap().is_some()
        || f.store.get(&tx, 1, b"own").unwrap().is_some()
        || !f.store.scan_at(snap, 1, .., &DenyAll).unwrap().is_empty()
        || !f
            .store
            .lookup_at(snap, 9, b"all", &DenyAll)
            .unwrap()
            .is_empty();
    wrong.then_some(Violation::RowExposed)
}
fn write_witness(f: &mut Fixture) -> Option<Violation> {
    let mut tx = f.store.begin_with_policy(DenyAll);
    tx.put((1, b"forbidden".to_vec()), vec![9]);
    match f.store.commit(tx) {
        Err(RowError::PermissionDenied) => None,
        Ok(_) => {
            assert_eq!(
                f.store
                    .get_at(f.store.snapshot(), 1, b"forbidden", &AllowAll)
                    .unwrap(),
                Some(&vec![9])
            );
            Some(Violation::DeniedWritePublished)
        }
        Err(error) => panic!("setup or unrelated commit error is not detection: {error}"),
    }
}
#[test]
fn independent_policy_witnesses_have_clean_controls_and_detect_each_guard() {
    for bug in Bug::POLICY {
        let mut f = Fixture::populated();
        assert_eq!(
            if bug == Bug::RowPolicyReadSkipped {
                read_witness(&f)
            } else {
                write_witness(&mut f)
            },
            None
        );
        let guard = Enabled::new(bug);
        let observed = if bug == Bug::RowPolicyReadSkipped {
            read_witness(&f)
        } else {
            write_witness(&mut f)
        };
        assert_eq!(
            observed,
            Some(if bug == Bug::RowPolicyReadSkipped {
                Violation::RowExposed
            } else {
                Violation::DeniedWritePublished
            })
        );
        drop(guard);
    }
}

proptest! {
    #[test]
    fn actor_write_workloads_match_an_independent_transition_model(
        operations in prop::collection::vec((0u8..3, 0u8..8, 0u8..3, any::<bool>(), 0u8..8, 0u8..3, any::<bool>(), any::<bool>()), 0..25)
    ) {
        let mut f = Fixture::empty();
        let mut model = Rows::new();
        for (actor, first, first_owner, delete_first, second, second_owner, delete_second, reopen) in operations {
            let mut proposed = Rows::new();
            // This separate map records the final transition for duplicate staged keys.
            let mut transitions = BTreeMap::new();
            for (key, owner, delete) in [(first, first_owner, delete_first), (second, second_owner, delete_second)] {
                transitions.insert(vec![key], if delete { None } else { Some(vec![owner, key]) });
            }
            let allowed = transitions.iter().all(|(key, after)| {
                model.get(key).is_none_or(|before| before[0] == actor)
                    && after.as_ref().is_none_or(|value| value[0] == actor)
            });
            let before = wal(&f.disk);
            let rng_before = f.calls.get();
            let mut tx = f.store.begin_with_policy(Owner(actor));
            for (key, owner, delete) in [(first, first_owner, delete_first), (second, second_owner, delete_second)] {
                if delete {
                    tx.delete((1, vec![key]));
                } else {
                    tx.put((1, vec![key]), vec![owner, key]);
                }
            }
            for (key, after) in &transitions {
                let expected = after.as_ref().filter(|value| value[0] == actor);
                prop_assert_eq!(f.store.get(&tx, 1, key).unwrap(), expected);
            }
            match f.store.commit(tx) {
                Ok(_) => {
                    prop_assert!(allowed, "an unauthorized transition was appended");
                    for (key, after) in transitions {
                        if let Some(value) = after {
                            model.insert(key, value);
                        } else {
                            model.remove(&key);
                        }
                    }
                }
                Err(RowError::PermissionDenied) => {
                    prop_assert!(!allowed, "an authorized transition was refused");
                    prop_assert_eq!(wal(&f.disk), before);
                    prop_assert_eq!(f.calls.get(), rng_before);
                }
                Err(error) => prop_assert!(false, "unexpected transition error: {error}"),
            }
            if reopen {
                drop(f.store);
                f.disk.crash();
                f.store = open(&f.disk, &f.clock, &f.calls);
            }
            let snap = f.store.snapshot();
            prop_assert_eq!(rows(&f.store, snap, &AllowAll), model.clone());
            for observer in 0..3 {
                proposed.clear();
                proposed.extend(model.iter().filter(|(_,value)| value[0] == observer).map(|(key,value)| (key.clone(),value.clone())));
                prop_assert_eq!(rows(&f.store, snap, &Owner(observer)), proposed.clone());
                prop_assert_eq!(f.store.lookup_at(snap, 9, b"all", &Owner(observer)).unwrap(), proposed.keys().cloned().collect::<Vec<_>>());
            }
            f.store.check_indexes().unwrap();
        }
    }

    #[test]
    fn actor_reads_match_an_independent_map(
        rows_input in prop::collection::vec((0u8..3, any::<u8>()), 0..35),
        actor in 0u8..3,
        reopen in any::<bool>(),
    ) {
        let mut f = Fixture::empty();
        let mut tx = f.store.begin();
        let mut model = Rows::new();
        for (n, (owner, payload)) in rows_input.into_iter().enumerate() {
            let key = (n as u32).to_be_bytes().to_vec();
            let value = vec![owner, payload];
            tx.put((1, key.clone()), value.clone());
            model.insert(key, value);
        }
        f.store.commit(tx).unwrap();
        if reopen {
            drop(f.store);
            f.disk.crash();
            f.store = open(&f.disk, &f.clock, &f.calls);
        }
        let expected: Rows = model.iter()
            .filter(|(_, value)| value[0] == actor)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let snap = f.store.snapshot();
        prop_assert_eq!(rows(&f.store, snap, &Owner(actor)), expected.clone());
        prop_assert_eq!(
            f.store.lookup_at(snap, 9, b"all", &Owner(actor)).unwrap(),
            expected.keys().cloned().collect::<Vec<_>>()
        );
        for (key, value) in &model {
            prop_assert_eq!(
                f.store.get_at(snap, 1, key, &Owner(actor)).unwrap(),
                if value[0] == actor { Some(value) } else { None }
            );
        }
        prop_assert!(rows(&f.store, snap, &DenyAll).is_empty());
        f.store.check_indexes().unwrap();
    }
}

#[test]
fn denied_actor_never_learns_a_hidden_row_was_written_but_allowed_actor_still_conflicts() {
    let mut f = Fixture::populated();
    // Owner(1) reads "a" ([1, 10]). Owner(2) is not allowed to touch it.
    let mut hidden_writer = f.store.begin_with_policy(Owner(1));
    hidden_writer.put((1, b"a".to_vec()), vec![1, 11]);
    let mut denied = f.store.begin_with_policy(Owner(2));
    denied.put((1, b"a".to_vec()), vec![2, 99]);
    let mut racer = f.store.begin();
    racer.put((1, b"a".to_vec()), vec![1, 12]);
    let newer = f.store.commit(racer).unwrap();
    let before = wal(&f.disk);
    assert_eq!(f.store.commit(denied), Err(RowError::PermissionDenied));
    assert_eq!(wal(&f.disk), before);
    // The permitted actor still gets the retryable conflict.
    assert_eq!(
        f.store.commit(hidden_writer),
        Err(RowError::Conflict { newer_lsn: newer })
    );
}

#[test]
fn blind_insert_on_a_hidden_key_is_denied_or_conflicts_depending_on_the_policy() {
    let mut f = Fixture::populated();
    // Owner(2) blind-inserts at "n" while a row it cannot see appears there.
    let mut blind_ok = f.store.begin_with_policy(Owner(2));
    blind_ok.put((1, b"n".to_vec()), vec![2, 1]);
    let mut blind_bad = f.store.begin_with_policy(Owner(2));
    blind_bad.put((1, b"n".to_vec()), vec![1, 1]);
    let mut other = f.store.begin();
    other.put((1, b"n".to_vec()), vec![1, 7]);
    let newer = f.store.commit(other).unwrap();
    // Against its own snapshot "n" is absent and the insert is allowed, so first-committer-wins
    // reports the conflict. This reveals that the key now exists (documented in the module docs).
    assert_eq!(
        f.store.commit(blind_ok),
        Err(RowError::Conflict { newer_lsn: newer })
    );
    // An insert the policy denies is rejected before the conflict can leak anything.
    assert_eq!(f.store.commit(blind_bad), Err(RowError::PermissionDenied));
}

#[test]
fn a_failure_after_the_durable_commit_marks_the_store_stale() {
    let mut f = Fixture::populated();
    // Plant an applied position far ahead of the log so the in-memory apply fails after the
    // kernel has acknowledged the change set as durable.
    f.store
        .table
        .apply(
            1_000_000,
            lattice_core::changeset::ChangeSetId::from_parts(9, 9, 9),
            Vec::new(),
        )
        .unwrap();
    let mut tx = f.store.begin();
    tx.put((1, b"durable".to_vec()), vec![1, 1]);
    assert!(matches!(f.store.commit(tx), Err(RowError::Corrupt(_))));
    let mut next = f.store.begin();
    next.put((1, b"after".to_vec()), vec![1, 2]);
    assert_eq!(f.store.commit(next), Err(RowError::Stale));
    // The log decides: a reopen sees the acknowledged write and not the refused one.
    let Fixture {
        disk,
        clock,
        calls,
        store,
    } = f;
    drop(store);
    let reopened = open(&disk, &clock, &calls);
    let snap = reopened.snapshot();
    assert_eq!(
        reopened.get_at(snap, 1, b"durable", &AllowAll),
        Ok(Some(&vec![1, 1]))
    );
    assert_eq!(reopened.get_at(snap, 1, b"after", &AllowAll), Ok(None));
}

mod limited_reads {
    use super::*;
    use crate::limits::{CancellationFlag, LimitError, StatementLimits};

    fn many(n: u8) -> Fixture {
        let mut f = Fixture::empty();
        let mut tx = f.store.begin();
        for i in 0..n {
            tx.put((1, vec![i]), vec![1, i]);
        }
        f.store.commit(tx).unwrap();
        f
    }

    /// Counts policy calls and runs a hook on each one, so a limit trips mid-scan.
    struct Hook<F: Fn(u64)>(Cell<u64>, F);
    impl<F: Fn(u64)> PolicyEvaluator for Hook<F> {
        fn allows_read(&self, _: RowRef<'_>) -> bool {
            self.0.set(self.0.get() + 1);
            (self.1)(self.0.get());
            true
        }
    }

    #[test]
    fn unlimited_results_match_the_plain_reads_and_hold_their_charge_until_dropped() {
        let f = many(5);
        let limits =
            StatementLimits::new(&*f.clock, None, 1 << 20, CancellationFlag::default()).unwrap();
        let snap = f.store.snapshot();
        let got = f
            .store
            .scan_at_limited(snap, 1, .., &AllowAll, &limits, &*f.clock)
            .unwrap();
        let plain = f.store.scan_at(snap, 1, .., &AllowAll).unwrap();
        assert_eq!(got.value, plain);
        let (_, used, _) = limits.memory();
        assert!(used >= 5 * 4, "each row is charged, got {used}");
        drop(got);
        assert_eq!(limits.memory().1, 0);
        let keys = f
            .store
            .lookup_at_limited(snap, 9, b"all", &AllowAll, &limits, &*f.clock)
            .unwrap();
        assert_eq!(
            keys.value,
            f.store.lookup_at(snap, 9, b"all", &AllowAll).unwrap()
        );
    }

    #[test]
    fn cancellation_stops_a_long_scan_and_lookup_midway() {
        let f = many(50);
        let flag = CancellationFlag::default();
        let limits = StatementLimits::new(&*f.clock, None, 1 << 20, flag.clone()).unwrap();
        let hook = Hook(Cell::new(0), |n| {
            if n == 3 {
                flag.cancel();
            }
        });
        let snap = f.store.snapshot();
        assert_eq!(
            f.store
                .scan_at_limited(snap, 1, .., &hook, &limits, &*f.clock)
                .map(|r| r.value.len()),
            Err(RowError::Limit(LimitError::Cancelled))
        );
        assert_eq!(hook.0.get(), 3, "the scan polled before the next row");
        assert_eq!(limits.memory().1, 0, "a stopped scan holds no charge");
        let hook = Hook(Cell::new(0), |_| ());
        assert_eq!(
            f.store
                .lookup_at_limited(snap, 9, b"all", &hook, &limits, &*f.clock)
                .map(|r| r.value.len()),
            Err(RowError::Limit(LimitError::Cancelled))
        );
        assert_eq!(hook.0.get(), 0);
    }

    #[test]
    fn a_deadline_stops_a_long_scan_and_lookup() {
        let f = many(50);
        let limits =
            StatementLimits::new(&*f.clock, Some(5), 1 << 20, CancellationFlag::default()).unwrap();
        let hook = Hook(Cell::new(0), |_| f.clock.advance(1));
        let snap = f.store.snapshot();
        assert_eq!(
            f.store
                .scan_at_limited(snap, 1, .., &hook, &limits, &*f.clock)
                .map(|r| r.value.len()),
            Err(RowError::Limit(LimitError::DeadlineExceeded))
        );
        assert_eq!(hook.0.get(), 5);
        assert_eq!(
            f.store
                .lookup_at_limited(snap, 9, b"all", &AllowAll, &limits, &*f.clock)
                .map(|r| r.value.len()),
            Err(RowError::Limit(LimitError::DeadlineExceeded))
        );
    }

    #[test]
    fn the_memory_budget_stops_a_scan_and_a_failed_scan_releases_its_charge() {
        let f = many(50);
        let snap = f.store.snapshot();
        let limits =
            StatementLimits::new(&*f.clock, None, 200, CancellationFlag::default()).unwrap();
        let err = f
            .store
            .scan_at_limited(snap, 1, .., &AllowAll, &limits, &*f.clock)
            .map(|r| r.value.len())
            .unwrap_err();
        assert!(matches!(
            err,
            RowError::Limit(LimitError::MemoryExceeded { limit: 200, .. })
        ));
        let (limit, used, peak) = limits.memory();
        assert_eq!((limit, used), (200, 0));
        assert!(peak > 0 && peak <= 200);
        let err = f
            .store
            .lookup_at_limited(snap, 9, b"all", &AllowAll, &limits, &*f.clock)
            .map(|r| r.value.len())
            .unwrap_err();
        assert!(matches!(
            err,
            RowError::Limit(LimitError::MemoryExceeded { .. })
        ));
        assert_eq!(limits.memory().1, 0);
    }
}
