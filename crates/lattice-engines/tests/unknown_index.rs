//! Unknown-index lookups, wrong-magic decode errors and definition-history vacuum. Kept apart
//! from `managed_indexes.rs`, whose test count the WAL admission workflow pins.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_engines::catalog::{self, CatalogError, CatalogOp};
use lattice_engines::index::{IndexKey, ManagedIndexDef};
use lattice_engines::mvcc::Snapshot;
use lattice_engines::store::{RowError, RowStore};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use proptest::prelude::*;
use std::rc::Rc;

use catalog::IndexOp;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;

fn config() -> Config {
    Config {
        segment_limit: 128,
        ..Config::default()
    }
}
fn fresh() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(17, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}
fn open(disk: &SharedIo, clock: &Rc<SimClock>) -> Store {
    clock.advance(1);
    RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(17),
        &config(),
    )
    .unwrap()
}
fn table(id: u32) -> CatalogOp {
    CatalogOp::Create {
        table: id,
        name: format!("table{id}"),
    }
}
fn definition(id: u32, table: u32, key: IndexKey) -> ManagedIndexDef {
    ManagedIndexDef {
        id,
        table,
        name: "by_value".into(),
        key,
    }
}

fn allow() -> lattice_engines::limits::AllowAll {
    lattice_engines::limits::AllowAll
}

#[test]
fn an_index_unknown_at_the_snapshot_is_an_error_not_an_empty_result() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store.commit_catalog(vec![table(7)], vec![]).unwrap();
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"alpha".to_vec());
    let before_create = store.commit(tx).unwrap();
    let created = store
        .commit_catalog(
            vec![],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
        )
        .unwrap();
    let dropped = store
        .commit_catalog(vec![], vec![IndexOp::Drop { id: 9 }])
        .unwrap();
    let missing = |index, snapshot| Err(RowError::NoSuchIndex { index, snapshot });
    // Before creation, although a matching row exists.
    assert_eq!(
        store.lookup_at(
            Snapshot {
                as_of: before_create
            },
            9,
            b"alpha",
            &allow()
        ),
        missing(9, before_create)
    );
    // While the index exists the lookup answers.
    assert_eq!(
        store.lookup_at(Snapshot { as_of: created }, 9, b"alpha", &allow()),
        Ok(vec![b"a".to_vec()])
    );
    // After the drop.
    assert_eq!(
        store.lookup_at(Snapshot { as_of: dropped }, 9, b"alpha", &allow()),
        missing(9, dropped)
    );
    // An id that never existed.
    assert_eq!(
        store.lookup_at(Snapshot { as_of: created }, 77, b"alpha", &allow()),
        missing(77, created)
    );
    assert_eq!(
        RowError::NoSuchIndex {
            index: 9,
            snapshot: 4
        }
        .to_string(),
        "no index 9 exists at snapshot 4"
    );
}

#[test]
fn a_wrong_magic_is_reported_as_bad_magic_not_truncated() {
    use lattice_engines::rowops::{self, RowOpsError};
    assert_eq!(
        catalog::decode(b"XXXX\x01\0\0\0\0"),
        Err(CatalogError::BadMagic)
    );
    assert_eq!(
        catalog::decode(b"LROW\x01\0\0\0\0"),
        Err(CatalogError::BadMagic)
    );
    assert_eq!(catalog::decode(b"LCA"), Err(CatalogError::Truncated));
    assert_eq!(
        rowops::decode(b"XXXX\x01\0\0\0\0"),
        Err(RowOpsError::BadMagic)
    );
    assert_eq!(
        rowops::decode(b"LCAT\x01\0\0\0\0"),
        Err(RowOpsError::BadMagic)
    );
    assert_eq!(rowops::decode(b"LRO"), Err(RowOpsError::Truncated));
    // The nested legacy catalog of a compound payload is checked for its magic too.
    let mut envelope = b"LCIX\x01".to_vec();
    envelope.extend_from_slice(&9u32.to_le_bytes());
    envelope.extend_from_slice(b"XXXX\x01\0\0\0\0");
    assert_eq!(
        catalog::decode_compound(&envelope),
        Err(CatalogError::BadMagic)
    );
}

#[test]
fn catalog_index_history_is_bounded_by_the_floor_and_keeps_used_ids() {
    let mut cat = catalog::Catalog::new();
    cat.apply_change(1, &[table(7)], &[]).unwrap();
    for (lsn, id) in [(2u64, 1u32), (4, 2), (6, 3)] {
        cat.apply_change(
            lsn,
            &[],
            &[IndexOp::Create(definition(id, 7, IndexKey::WholeValue))],
        )
        .unwrap();
        cat.apply_change(lsn + 1, &[], &[IndexOp::Drop { id }])
            .unwrap();
    }
    // Versions at lsn 1..=7; the newest at or below 5 is lsn 5, so 1..=4 go.
    let expect: Vec<_> = (0..=8).map(|at| cat.indexes_as_of(at)).collect();
    assert_eq!(cat.vacuum_index_versions(5), 4);
    for at in 5..=8 {
        assert_eq!(cat.indexes_as_of(at), expect[at as usize]);
    }
    assert_eq!(cat.vacuum_index_versions(5), 0);
    assert_eq!(cat.vacuum_index_versions(100), 2);
    assert_eq!(cat.indexes(), expect[8]);
    assert_eq!(
        cat.apply_change(
            9,
            &[],
            &[IndexOp::Create(definition(1, 7, IndexKey::WholeValue))]
        ),
        Err(CatalogError::IndexIdUsed(1))
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn vacuuming_definition_history_changes_no_lookup_at_or_after_the_floor(
        ops in prop::collection::vec((0u8..4, 0u8..3), 1..24),
        horizon in 0u64..40,
    ) {
        let (disk, clock) = fresh();
        let mut store = open(&disk, &clock);
        store.commit_catalog(vec![table(7)], vec![]).unwrap();
        let mut live: Vec<u32> = Vec::new();
        let mut next_id = 1u32;
        for (kind, arg) in ops {
            match kind {
                0 | 1 => {
                    let mut tx = store.begin();
                    tx.put((7, vec![arg]), vec![arg % 2]);
                    store.commit(tx).unwrap();
                }
                2 => {
                    let mut def = definition(next_id, 7, IndexKey::WholeValue);
                    def.name = format!("by_value{next_id}");
                    store.commit_catalog(vec![], vec![IndexOp::Create(def)]).unwrap();
                    live.push(next_id);
                    next_id += 1;
                }
                _ => {
                    if !live.is_empty() {
                        let id = live.remove(usize::from(arg) % live.len());
                        store.commit_catalog(vec![], vec![IndexOp::Drop { id }]).unwrap();
                    }
                }
            }
        }
        let applied = store.applied_lsn();
        let lookups = |store: &Store, from: u64| {
            let mut seen = Vec::new();
            for at in from..=applied {
                let snap = Snapshot { as_of: at };
                for id in 0..=next_id + 1 {
                    for key in 0u8..2 {
                        seen.push(((at, id, key), store.lookup_at(snap, id, &[key], &allow())));
                    }
                }
            }
            seen
        };
        let defs = |store: &Store, from: u64| -> Vec<_> {
            (from..=applied).map(|at| store.catalog().indexes_as_of(at)).collect()
        };
        let all_lookups = lookups(&store, 0);
        let all_defs = defs(&store, 0);
        store.vacuum(horizon);
        let floor = store.oldest_snapshot().as_of;
        let kept_lookups: Vec<_> = all_lookups
            .into_iter()
            .filter(|((at, _, _), _)| *at >= floor)
            .collect();
        prop_assert_eq!(lookups(&store, floor), kept_lookups);
        let kept_defs: Vec<_> = all_defs.into_iter().skip(floor as usize).collect();
        prop_assert_eq!(defs(&store, floor), kept_defs);
        for at in floor..=applied {
            let checked = store.check_indexes_at(Snapshot { as_of: at });
            prop_assert_eq!(checked, Ok(()));
        }
    }
}
