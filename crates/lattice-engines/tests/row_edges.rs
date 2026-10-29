//! Row engine edges found by mutation testing: one change set writing a key twice, other record kinds
//! in the shared log, and a commit the kernel rejects.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_core::changeset::{ChangeSetId, TurnRef};
use lattice_engines::mvcc::Table;
use lattice_engines::store::{RowError, RowStore};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 1 << 20,
        node: 1,
    }
}

fn fresh() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(1, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}

fn open(
    disk: &SharedIo,
    clock: &Rc<SimClock>,
    seed: u64,
) -> Result<RowStore<SharedIo, SharedClock, SeededRng>, RowError> {
    RowStore::open_with_indexes(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
        Vec::new(),
    )
}

#[test]
fn a_key_written_twice_in_one_change_set_keeps_one_version_with_the_last_value() {
    let mut table: Table<u32, u32> = Table::new();
    let cs = ChangeSetId::from_parts(1, 1, 1);
    table
        .apply(1, cs, vec![(7, Some(1)), (7, Some(2))])
        .unwrap();
    assert_eq!(table.version_count(), 1);
    assert_eq!(table.get(table.snapshot(), &7).unwrap(), Some(&2));
    // A write and a delete of the same key in one change set also leave one version, a deletion.
    table
        .apply(
            2,
            ChangeSetId::from_parts(2, 1, 1),
            vec![(7, Some(3)), (7, None)],
        )
        .unwrap();
    assert_eq!(table.version_count(), 2);
    assert_eq!(table.get(table.snapshot(), &7).unwrap(), None);
}

#[test]
fn records_of_other_kinds_in_the_shared_log_are_skipped_when_the_rows_are_rebuilt() {
    use lattice_core::Kernel;
    let (disk, clock) = fresh();
    {
        let (mut kernel, _) = Kernel::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(1),
            &config(),
        )
        .unwrap();
        kernel
            .begin_turn(TurnRef {
                session: 1,
                turn: 1,
            })
            .unwrap();
    }
    let mut store = open(&disk, &clock, 2).unwrap();
    let mut tx = store.begin();
    tx.put((1, b"k".to_vec()), b"v".to_vec());
    assert!(store.commit(tx).is_ok());
}

#[test]
fn a_change_the_kernel_rejects_is_rejected_and_leaves_the_store_usable() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock, 1).unwrap();
    let mut tx = store.begin();
    tx.put((1, b"big".to_vec()), vec![0u8; 17 * 1024 * 1024]);
    match store.commit(tx) {
        Err(RowError::Rejected(_)) => {}
        other => panic!("expected the kernel to reject an oversized change, got {other:?}"),
    }
    let mut tx = store.begin();
    tx.put((1, b"small".to_vec()), b"v".to_vec());
    assert!(store.commit(tx).is_ok());
}

#[test]
fn ranges_that_select_nothing_return_nothing_and_a_single_key_range_returns_that_key() {
    use std::ops::Bound::{Excluded, Included};
    let mut table: Table<u32, u32> = Table::new();
    table
        .apply(
            1,
            ChangeSetId::from_parts(1, 1, 1),
            (1..=9).map(|k| (k, Some(k * 10))).collect(),
        )
        .unwrap();
    let snap = table.snapshot();
    let keys = |rows: Vec<(&u32, &u32)>| rows.into_iter().map(|(k, _)| *k).collect::<Vec<_>>();
    // Inverted or empty bounds select nothing, whatever mix of inclusive and exclusive ends.
    assert!(
        table
            .range(snap, (Included(5), Included(3)))
            .unwrap()
            .is_empty()
    );
    assert!(table.range(snap, 5..5).unwrap().is_empty());
    assert!(
        table
            .range(snap, (Excluded(5), Included(5)))
            .unwrap()
            .is_empty()
    );
    assert!(
        table
            .range(snap, (Excluded(5), Excluded(5)))
            .unwrap()
            .is_empty()
    );
    assert!(
        table
            .range(snap, (Included(6), Excluded(5)))
            .unwrap()
            .is_empty()
    );
    // A one-key range is not empty, and neighbouring exclusive bounds are plain ranges.
    assert_eq!(keys(table.range(snap, 5..=5).unwrap()), vec![5]);
    assert_eq!(
        keys(table.range(snap, (Included(5), Excluded(6))).unwrap()),
        vec![5]
    );
    assert_eq!(
        keys(table.range(snap, (Excluded(4), Included(5))).unwrap()),
        vec![5]
    );
    assert_eq!(keys(table.range(snap, 3..=5).unwrap()), vec![3, 4, 5]);
}
