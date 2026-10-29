//! Independent format and public-store witnesses for draft catalog-managed indexes.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::bugs::{self, Bug};
use lattice_core::{ChangeSet, Config, Kernel};
use lattice_engines::catalog::{
    self, CatalogError, CatalogOp, CatalogStore, CompoundChange, IndexOp,
};
use lattice_engines::index::{IndexDef, IndexKey, ManagedIndexDef};
use lattice_engines::mvcc::Snapshot;
use lattice_engines::store::{RowError, RowStore};
use lattice_seams::Io;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;
const COMPOUND: &[u8] = include_bytes!("../../../formats/corpus/catalog-index-v1-compound.golden");
const LEGACY: &[u8] = include_bytes!("../../../formats/corpus/catalog-v1-table.golden");
const DROP: &[u8] = include_bytes!("../../../formats/corpus/catalog-index-v1-drop.golden");
const SLICE: &[u8] = include_bytes!("../../../formats/corpus/catalog-index-v1-slice.golden");

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
    // Each open gets a fresh timestamp: resetting a seeded RNG at the same clock tick would reuse ids.
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
fn golden_change() -> CompoundChange {
    CompoundChange {
        tables: vec![CatalogOp::Create {
            table: 7,
            name: "items".into(),
        }],
        indexes: vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
        rows: vec![
            ((7, b"a".to_vec()), Some(b"x".to_vec())),
            ((7, b"b".to_vec()), None),
        ],
    }
}
fn rows(store: &Store) -> BTreeMap<Vec<u8>, Vec<u8>> {
    store
        .scan_at(store.snapshot(), 7, .., &lattice_engines::limits::AllowAll)
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key.to_vec(), value.clone()))
        .collect()
}
fn wal(disk: &SharedIo) -> Vec<(String, Vec<u8>)> {
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
fn independent_compound_bytes_pin_the_nested_legacy_catalog_and_all_fields() {
    let expected = golden_change();
    assert_eq!(catalog::decode_compound(COMPOUND), Ok(expected.clone()));
    assert_eq!(catalog::encode_compound(&expected).unwrap(), COMPOUND);
    assert_eq!(catalog::encode(&expected.tables).unwrap(), LEGACY);
    assert_eq!(catalog::decode(LEGACY), Ok(expected.tables));
    for bytes in [DROP, SLICE] {
        let decoded = catalog::decode_compound(bytes).unwrap();
        assert_eq!(catalog::encode_compound(&decoded).unwrap(), bytes);
    }
    assert_eq!(
        catalog::decode_compound(DROP).unwrap().indexes,
        vec![IndexOp::Drop { id: 9 }]
    );
    let slice = ManagedIndexDef {
        id: 12,
        table: 7,
        name: "prefix".into(),
        key: IndexKey::Slice {
            offset: 1,
            length: 2,
        },
    };
    assert_eq!(
        catalog::decode_compound(SLICE).unwrap().indexes,
        vec![IndexOp::Create(slice)]
    );
    for cut in 0..COMPOUND.len() {
        assert!(
            catalog::decode_compound(&COMPOUND[..cut]).is_err(),
            "cut{cut}"
        );
    }
    let mut trailing = COMPOUND.to_vec();
    trailing.push(0);
    assert_eq!(
        catalog::decode_compound(&trailing),
        Err(CatalogError::Trailing)
    );
    for version in [0, 2, 255] {
        let mut bytes = COMPOUND.to_vec();
        bytes[4] = version;
        assert_eq!(
            catalog::decode_compound(&bytes),
            Err(CatalogError::UnsupportedVersion(version))
        );
    }
    let mut count = DROP.to_vec();
    count[9..13].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        catalog::decode_compound(&count),
        Err(CatalogError::TooLarge)
    );
}

#[test]
fn create_update_drop_recreate_and_reopen_preserve_the_definition_timeline() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store.commit_catalog(vec![table(7)], vec![]).unwrap();
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"alpha".to_vec());
    tx.put((7, b"b".to_vec()), b"beta".to_vec());
    let before = store.commit(tx).unwrap();
    let born = store
        .commit_catalog(
            vec![],
            vec![IndexOp::Create(definition(
                9,
                7,
                IndexKey::Slice {
                    offset: 0,
                    length: 1,
                },
            ))],
        )
        .unwrap();
    assert_eq!(
        store
            .lookup_at(
                Snapshot { as_of: before },
                9,
                b"a",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        store
            .lookup_at(
                Snapshot { as_of: born },
                9,
                b"a",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"gamma".to_vec());
    tx.delete((7, b"b".to_vec()));
    let changed = store.commit(tx).unwrap();
    assert!(
        store
            .lookup_at(
                Snapshot { as_of: changed },
                9,
                b"a",
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .lookup_at(
                Snapshot { as_of: changed },
                9,
                b"g",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    let replaced = store
        .commit_catalog(
            vec![],
            vec![
                IndexOp::Drop { id: 9 },
                IndexOp::Create(definition(10, 7, IndexKey::WholeValue)),
            ],
        )
        .unwrap();
    assert_eq!(
        store
            .lookup_at(
                Snapshot { as_of: born },
                9,
                b"b",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"b".to_vec()]
    );
    assert!(
        store
            .lookup_at(
                Snapshot { as_of: replaced },
                9,
                b"g",
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .lookup_at(
                Snapshot { as_of: replaced },
                10,
                b"gamma",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    assert!(store.catalog().indexes_as_of(born).contains_key(&9));
    assert_eq!(
        store
            .catalog()
            .indexes()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![10]
    );
    for at in [0, before, born, changed, replaced] {
        store.check_indexes_at(Snapshot { as_of: at }).unwrap();
    }
    drop(store);
    disk.crash();
    let mut store = open(&disk, &clock);
    for at in [0, before, born, changed, replaced] {
        store.check_indexes_at(Snapshot { as_of: at }).unwrap();
    }
    assert_eq!(
        store
            .lookup_at(
                Snapshot { as_of: born },
                9,
                b"a",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                10,
                b"gamma",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    let prior = wal(&disk);
    assert_eq!(
        store.commit_catalog(
            vec![],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))]
        ),
        Err(RowError::InvalidCatalog(CatalogError::IndexIdUsed(9)))
    );
    assert_eq!(
        store.commit_catalog(vec![CatalogOp::Drop { table: 7 }], vec![]),
        Err(RowError::InvalidCatalog(CatalogError::TableHasIndexes(7)))
    );
    assert_eq!(wal(&disk), prior, "invalid metadata must not append WAL");
    let dropped = store
        .commit_catalog(
            vec![CatalogOp::Drop { table: 7 }],
            vec![IndexOp::Drop { id: 10 }],
        )
        .unwrap();
    assert!(store.catalog().current().is_empty());
    assert!(store.catalog().indexes().is_empty());
    assert!(
        store
            .lookup_at(
                Snapshot { as_of: dropped },
                10,
                b"gamma",
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .lookup_at(
                Snapshot { as_of: replaced },
                10,
                b"gamma",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
}

#[test]
fn compound_metadata_and_repeated_row_writes_publish_at_one_lsn() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"x".to_vec());
    tx.put((7, b"b".to_vec()), b"y".to_vec());
    let at = store
        .commit_with_catalog(
            tx,
            vec![table(7)],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
        )
        .unwrap();
    assert_eq!(at, 1);
    assert_eq!(store.catalog().applied_lsn(), at);
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"x",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    assert!(store.catalog().indexes_as_of(0).is_empty());
    assert!(
        store
            .scan_at(
                Snapshot { as_of: 0 },
                7,
                ..,
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .is_empty()
    );
    store.check_indexes_at(store.snapshot()).unwrap();
    drop(store);
    disk.crash();
    let mut store = open(&disk, &clock);
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"first".to_vec());
    tx.put((7, b"a".to_vec()), b"last".to_vec());
    store
        .commit_with_catalog(
            tx,
            vec![CatalogOp::Rename {
                table: 7,
                name: "renamed".into(),
            }],
            vec![],
        )
        .unwrap();
    assert!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"first",
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"last",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    store.check_indexes_at(store.snapshot()).unwrap();
}

#[test]
fn catalog_store_metadata_and_default_row_open_recover_without_callbacks() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store.commit_catalog(vec![table(7)], vec![]).unwrap();
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"z".to_vec());
    store.commit(tx).unwrap();
    drop(store);
    let mut cat = CatalogStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(27),
        &config(),
    )
    .unwrap();
    cat.commit_managed(
        vec![],
        vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
    )
    .unwrap();
    assert!(cat.catalog().indexes().contains_key(&9));
    drop(cat);
    let store = open(&disk, &clock);
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"z",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    store.check_indexes().unwrap();
    drop(store);
    let cat = CatalogStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(27),
        &config(),
    )
    .unwrap();
    assert!(cat.catalog().indexes().contains_key(&9));
    drop(cat);
    let legacy = RowStore::open_with_indexes(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(27),
        &config(),
        vec![IndexDef {
            id: 8,
            table: 7,
            extract: |v| Some(v.to_vec()),
        }],
    );
    assert!(
        matches!(legacy, Err(RowError::InvalidIndexes(_))),
        "callback mode must not silently replace persisted definitions"
    );
}

#[test]
fn malformed_claimed_compound_payload_refuses_both_catalog_and_row_recovery() {
    for bytes in [b"LCIX".to_vec(), {
        let mut b = COMPOUND.to_vec();
        b.push(0);
        b
    }] {
        let (disk, clock) = fresh();
        let (mut kernel, _) = Kernel::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(31),
            &config(),
        )
        .unwrap();
        let id = kernel.new_id();
        kernel.commit(ChangeSet::new(id, bytes)).unwrap();
        drop(kernel);
        assert!(matches!(
            RowStore::open(
                disk.clone(),
                SharedClock(clock.clone()),
                SeededRng::new(31),
                &config()
            ),
            Err(RowError::Corrupt(_))
        ));
        assert!(matches!(
            CatalogStore::open(
                disk.clone(),
                SharedClock(clock.clone()),
                SeededRng::new(31),
                &config()
            ),
            Err(catalog::StoreError::Corrupt(_))
        ));
    }
}

struct BugGuard;
impl Drop for BugGuard {
    fn drop(&mut self) {
        bugs::clear();
    }
}
#[test]
fn the_managed_recovery_planted_bug_fails_an_independent_semantic_oracle() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"x".to_vec());
    store
        .commit_with_catalog(
            tx,
            vec![table(7)],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
        )
        .unwrap();
    drop(store);
    disk.crash();
    {
        let correct = open(&disk, &clock);
        assert_eq!(
            correct
                .lookup_at(
                    correct.snapshot(),
                    9,
                    b"x",
                    &lattice_engines::limits::AllowAll
                )
                .unwrap(),
            vec![b"a".to_vec()]
        );
        correct.check_indexes().unwrap();
    }
    bugs::enable(Bug::ManagedIndexesNotRecovered);
    let _guard = BugGuard;
    let broken = open(&disk, &clock);
    assert!(broken.catalog().indexes().contains_key(&9));
    assert_ne!(
        broken
            .lookup_at(
                broken.snapshot(),
                9,
                b"x",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()],
        "recovery defect must cause the expected semantic mismatch"
    );
    assert!(
        broken.check_indexes().is_err(),
        "scrubber must derive the definition from the catalog, not the broken index registry"
    );
}

fn power_cut(cut: Option<u64>) -> u64 {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store.commit_catalog(vec![table(7)], vec![]).unwrap();
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"old".to_vec());
    tx.put((7, b"b".to_vec()), b"keep".to_vec());
    store.commit(tx).unwrap();
    let before = disk.mutating_ops();
    if let Some(cut) = cut {
        disk.arm_power_cut(cut);
    }
    let mut tx = store.begin();
    tx.put((7, b"a".to_vec()), b"new".to_vec());
    tx.delete((7, b"b".to_vec()));
    tx.put((7, b"c".to_vec()), b"new".to_vec());
    let acknowledged = store
        .commit_with_catalog(
            tx,
            vec![CatalogOp::Rename {
                table: 7,
                name: "after".into(),
            }],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
        )
        .is_ok();
    let ops = disk.mutating_ops() - before;
    let cut_observed = disk.is_powered_off();
    drop(store);
    disk.crash();
    disk.set_profile(FaultProfile::clean());
    let recovered = open(&disk, &clock);
    let indexed = recovered.catalog().indexes().contains_key(&9);
    let expected = if indexed {
        BTreeMap::from([
            (b"a".to_vec(), b"new".to_vec()),
            (b"c".to_vec(), b"new".to_vec()),
        ])
    } else {
        BTreeMap::from([
            (b"a".to_vec(), b"old".to_vec()),
            (b"b".to_vec(), b"keep".to_vec()),
        ])
    };
    assert_eq!(
        rows(&recovered),
        expected,
        "cut{cut:?}: partial compound row publication"
    );
    assert_eq!(
        recovered.catalog().current().get(&7).map(String::as_str),
        Some(if indexed { "after" } else { "table7" })
    );
    if acknowledged {
        assert!(indexed, "acknowledged compound was lost at cut{cut:?}");
    }
    if indexed {
        assert_eq!(
            recovered
                .lookup_at(
                    recovered.snapshot(),
                    9,
                    b"new",
                    &lattice_engines::limits::AllowAll
                )
                .unwrap(),
            vec![b"a".to_vec(), b"c".to_vec()]
        );
    } else {
        assert!(
            recovered
                .lookup_at(
                    recovered.snapshot(),
                    9,
                    b"new",
                    &lattice_engines::limits::AllowAll
                )
                .unwrap()
                .is_empty()
        );
    }
    recovered.check_indexes().unwrap();
    drop(recovered);
    let again = open(&disk, &clock);
    assert_eq!(rows(&again), expected);
    again.check_indexes().unwrap();
    if cut.is_some_and(|cut| cut < ops) {
        assert!(cut_observed);
    }
    ops
}
#[test]
fn every_observed_compound_publication_cut_recovers_all_or_none() {
    let operations = power_cut(None);
    assert!(operations > 0);
    eprintln!(
        "compound publication: {operations} mutating operations, sweeping cuts 0..={operations}"
    );
    for cut in 0..=operations {
        power_cut(Some(cut));
    }
}

proptest! {
    #[test]
    fn arbitrary_compound_inputs_never_panic_and_success_reencodes_exactly(bytes in prop::collection::vec(any::<u8>(),0..512)) {
        if let Ok(change)=catalog::decode_compound(&bytes) {prop_assert_eq!(catalog::encode_compound(&change).unwrap(),bytes);}
    }
    #[test]
    fn slice_extraction_matches_a_wide_integer_bounds_model(offset in any::<u32>(),length in any::<u32>(),value in prop::collection::vec(any::<u8>(),0..128)) {
        let key=IndexKey::Slice{offset,length};let end=u64::from(offset)+u64::from(length);
        prop_assert_eq!(key.is_valid(),end<=u64::from(u32::MAX));
        let expected=if end<=value.len() as u64 {Some(value[offset as usize..end as usize].to_vec())}else{None};
        prop_assert_eq!(key.extract(&value),expected);
    }
}

#[test]
fn invalid_definitions_and_conflicting_compound_transactions_append_nothing() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store.commit_catalog(vec![table(7)], vec![]).unwrap();
    let before = wal(&disk);
    assert_eq!(
        store.commit_catalog(
            vec![],
            vec![IndexOp::Create(definition(9, 8, IndexKey::WholeValue))]
        ),
        Err(RowError::InvalidCatalog(CatalogError::NoSuchTable(8)))
    );
    assert_eq!(
        store.commit_catalog(
            vec![],
            vec![IndexOp::Create(definition(
                9,
                7,
                IndexKey::Slice {
                    offset: u32::MAX,
                    length: 1
                }
            ))]
        ),
        Err(RowError::InvalidCatalog(CatalogError::BadIndexKey))
    );
    assert_eq!(
        store.commit_catalog(
            vec![],
            vec![
                IndexOp::Create(definition(9, 7, IndexKey::WholeValue)),
                IndexOp::Create(definition(10, 7, IndexKey::WholeValue))
            ]
        ),
        Err(RowError::InvalidCatalog(CatalogError::IndexNameTaken(
            "by_value".into()
        )))
    );
    assert_eq!(wal(&disk), before);
    let mut old = store.begin();
    old.put((7, b"key".to_vec()), b"stale".to_vec());
    let mut new = store.begin();
    new.put((7, b"key".to_vec()), b"current".to_vec());
    let newer_lsn = store.commit(new).unwrap();
    let before = wal(&disk);
    assert_eq!(
        store.commit_with_catalog(
            old,
            vec![CatalogOp::Rename {
                table: 7,
                name: "wrong".into()
            }],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))]
        ),
        Err(RowError::Conflict { newer_lsn })
    );
    assert_eq!(wal(&disk), before);
    assert!(store.catalog().indexes().is_empty());
    assert_eq!(
        store.catalog().current().get(&7).map(String::as_str),
        Some("table7")
    );
}

#[test]
fn invisible_atomic_create_drop_still_reserves_the_index_id_after_recovery() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store
        .commit_catalog(
            vec![table(7)],
            vec![
                IndexOp::Create(definition(9, 7, IndexKey::WholeValue)),
                IndexOp::Drop { id: 9 },
            ],
        )
        .unwrap();
    assert!(store.catalog().indexes().is_empty());
    store.check_indexes().unwrap();
    drop(store);
    disk.crash();
    let mut store = open(&disk, &clock);
    assert!(store.catalog().indexes().is_empty());
    assert_eq!(
        store.commit_catalog(
            vec![],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))]
        ),
        Err(RowError::InvalidCatalog(CatalogError::IndexIdUsed(9)))
    );
    assert!(
        store
            .commit_catalog(
                vec![],
                vec![IndexOp::Create(definition(10, 7, IndexKey::WholeValue))]
            )
            .is_ok()
    );
}

#[test]
fn creation_after_vacuum_builds_current_rows_without_requiring_expired_history() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store.commit_catalog(vec![table(7)], vec![]).unwrap();
    for value in [b"old".as_slice(), b"middle", b"current"] {
        let mut tx = store.begin();
        tx.put((7, b"a".to_vec()), value.to_vec());
        store.commit(tx).unwrap();
    }
    let floor = store.applied_lsn();
    store.vacuum(floor);
    store
        .commit_catalog(
            vec![],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
        )
        .unwrap();
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"current",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    assert!(
        store
            .lookup_at(
                Snapshot { as_of: floor },
                9,
                b"current",
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        store.lookup_at(
            Snapshot { as_of: floor - 1 },
            9,
            b"old",
            &lattice_engines::limits::AllowAll
        ),
        Err(RowError::SnapshotTooOld { .. })
    ));
    store.check_indexes().unwrap();
    drop(store);
    disk.crash();
    let store = open(&disk, &clock);
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"current",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    store.check_indexes().unwrap();
}

#[test]
fn byte_window_extraction_has_exact_independent_offset_length_and_short_value_results() {
    assert_eq!(
        IndexKey::Slice {
            offset: 1,
            length: 2
        }
        .extract(b"abcd"),
        Some(b"bc".to_vec())
    );
    assert_eq!(
        IndexKey::Slice {
            offset: 1,
            length: 2
        }
        .extract(b"ab"),
        None
    );
    assert_eq!(
        IndexKey::Slice {
            offset: 4,
            length: 0
        }
        .extract(b"abcd"),
        Some(Vec::new())
    );
    assert_eq!(
        IndexKey::Slice {
            offset: 5,
            length: 0
        }
        .extract(b"abcd"),
        None
    );
    assert_eq!(
        IndexKey::Slice {
            offset: u32::MAX,
            length: 1
        }
        .extract(b"abcd"),
        None
    );
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    let mut tx = store.begin();
    tx.put((7, b"first".to_vec()), b"abcd".to_vec());
    tx.put((7, b"short".to_vec()), b"ab".to_vec());
    store
        .commit_with_catalog(
            tx,
            vec![table(7)],
            vec![IndexOp::Create(definition(
                9,
                7,
                IndexKey::Slice {
                    offset: 1,
                    length: 2,
                },
            ))],
        )
        .unwrap();
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"bc",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"first".to_vec()]
    );
    assert!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"ab",
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .is_empty()
    );
    drop(store);
    disk.crash();
    let store = open(&disk, &clock);
    assert_eq!(
        store
            .lookup_at(
                store.snapshot(),
                9,
                b"bc",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"first".to_vec()]
    );
    store.check_indexes().unwrap();
}

#[test]
fn nested_catalog_count_is_bounded_before_decoding_or_allocating_its_operations() {
    // Only the nested LCAT header is present: the oversized count must win over child truncation.
    let mut nested = b"LCAT\x01".to_vec();
    nested.extend_from_slice(&4097u32.to_le_bytes());
    let mut envelope = b"LCIX\x01".to_vec();
    envelope.extend_from_slice(&9u32.to_le_bytes());
    envelope.extend_from_slice(&nested);
    assert_eq!(
        catalog::decode_compound(&envelope),
        Err(CatalogError::TooLarge)
    );
    // Legacy decoding keeps its existing contract; the new bound belongs only to LCIX nesting.
    assert_eq!(catalog::decode(&nested), Err(CatalogError::Truncated));
    for count in [4096u32, 4097] {
        let mut nested = b"LCAT\x01".to_vec();
        nested.extend_from_slice(&count.to_le_bytes());
        for id in 0..count {
            nested.push(3);
            nested.extend_from_slice(&id.to_le_bytes());
        }
        let mut envelope = b"LCIX\x01".to_vec();
        envelope.extend_from_slice(&(nested.len() as u32).to_le_bytes());
        envelope.extend_from_slice(&nested);
        envelope.extend_from_slice(&0u32.to_le_bytes());
        envelope.extend_from_slice(&0u32.to_le_bytes());
        if count == 4096 {
            let decoded = catalog::decode_compound(&envelope).unwrap();
            assert_eq!(decoded.tables.len(), 4096);
            assert_eq!(catalog::encode_compound(&decoded).unwrap(), envelope);
        } else {
            assert_eq!(
                catalog::decode_compound(&envelope),
                Err(CatalogError::TooLarge)
            );
        }
    }
}

#[test]
fn row_count_does_not_inherit_the_index_operation_limit() {
    let change = CompoundChange {
        tables: vec![table(7)],
        indexes: Vec::new(),
        rows: (0u32..4097)
            .map(|id| ((7, id.to_le_bytes().to_vec()), None))
            .collect(),
    };
    let encoded = catalog::encode_compound(&change).unwrap();
    assert_eq!(catalog::decode_compound(&encoded), Ok(change));
}

#[test]
fn empty_index_recovery_omission_is_detected_before_a_later_write_can_bypass_maintenance() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    let at = store
        .commit_catalog(
            vec![table(7)],
            vec![IndexOp::Create(definition(9, 7, IndexKey::WholeValue))],
        )
        .unwrap();
    assert!(rows(&store).is_empty());
    store.check_indexes_at(Snapshot { as_of: 0 }).unwrap();
    store.check_indexes().unwrap();
    drop(store);
    disk.crash();
    {
        let correct = open(&disk, &clock);
        assert!(correct.catalog().indexes().contains_key(&9));
        assert!(
            correct
                .lookup_at(
                    correct.snapshot(),
                    9,
                    b"x",
                    &lattice_engines::limits::AllowAll
                )
                .unwrap()
                .is_empty()
        );
        correct.check_indexes_at(Snapshot { as_of: at }).unwrap();
        correct.check_indexes().unwrap();
    }
    bugs::enable(Bug::ManagedIndexesNotRecovered);
    let _guard = BugGuard;
    let mut broken = open(&disk, &clock);
    assert!(broken.catalog().indexes().contains_key(&9));
    assert!(rows(&broken).is_empty());
    let error = broken
        .check_indexes()
        .expect_err("an empty entry map must not conceal a missing live definition");
    assert!(error.contains("definitions"));
    let mut tx = broken.begin();
    tx.put((7, b"a".to_vec()), b"x".to_vec());
    broken.commit(tx).unwrap();
    assert_ne!(
        broken
            .lookup_at(
                broken.snapshot(),
                9,
                b"x",
                &lattice_engines::limits::AllowAll
            )
            .unwrap(),
        vec![b"a".to_vec()]
    );
    assert!(broken.check_indexes().is_err());
}

#[test]
fn compound_writes_and_historical_managed_lookups_preserve_the_bound_actor_policy() {
    use lattice_engines::limits::{AllowAll, DenyAll, PolicyEvaluator, RowRef, RowWriteRef};
    struct Actor;
    impl PolicyEvaluator for Actor {
        fn allows_read(&self, row: RowRef<'_>) -> bool {
            row.table == 7 && row.value.starts_with(b"a")
        }
        fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
            row.table == 7
                && row.before.is_none_or(|value| value.starts_with(b"a"))
                && row.after.is_none_or(|value| value.starts_with(b"a"))
        }
    }
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    let mut tx = store.begin();
    tx.put((7, b"allowed".to_vec()), b"alpha".to_vec());
    tx.put((7, b"hidden".to_vec()), b"beta".to_vec());
    tx.put((8, b"other-table".to_vec()), b"alpha".to_vec());
    let born = store
        .commit_with_catalog(
            tx,
            vec![table(7), table(8)],
            vec![
                IndexOp::Create(definition(9, 7, IndexKey::WholeValue)),
                IndexOp::Create(definition(12, 8, IndexKey::WholeValue)),
            ],
        )
        .unwrap();
    let before = wal(&disk);
    let mut denied = store.begin_with_policy(DenyAll);
    denied.put((7, b"allowed".to_vec()), b"after".to_vec());
    assert_eq!(
        store.commit_with_catalog(
            denied,
            vec![CatalogOp::Rename {
                table: 7,
                name: "changed".into()
            }],
            Vec::new(),
        ),
        Err(RowError::PermissionDenied)
    );
    assert_eq!(wal(&disk), before);
    assert_eq!(store.snapshot().as_of, born);
    let mut mixed = store.begin_with_policy(Actor);
    mixed.put((7, b"allowed".to_vec()), b"after".to_vec());
    mixed.delete((7, b"hidden".to_vec()));
    assert_eq!(
        store.commit_with_catalog(
            mixed,
            vec![CatalogOp::Rename {
                table: 7,
                name: "changed".into()
            }],
            Vec::new(),
        ),
        Err(RowError::PermissionDenied)
    );
    assert_eq!(wal(&disk), before);
    let mut allowed = store.begin_with_policy(Actor);
    allowed.put((7, b"allowed".to_vec()), b"after".to_vec());
    let replaced = store
        .commit_with_catalog(
            allowed,
            Vec::new(),
            vec![
                IndexOp::Drop { id: 9 },
                IndexOp::Create(definition(10, 7, IndexKey::WholeValue)),
            ],
        )
        .unwrap();
    for reopen in 0..2 {
        if reopen == 1 {
            drop(store);
            disk.crash();
            store = open(&disk, &clock);
        }
        let old = Snapshot { as_of: born };
        let now = Snapshot { as_of: replaced };
        assert_eq!(
            store.lookup_at(old, 9, b"alpha", &Actor).unwrap(),
            vec![b"allowed".to_vec()]
        );
        assert!(store.lookup_at(old, 9, b"beta", &Actor).unwrap().is_empty());
        assert!(
            store
                .lookup_at(old, 12, b"alpha", &Actor)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .lookup_at(now, 9, b"alpha", &Actor)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.lookup_at(now, 10, b"after", &Actor).unwrap(),
            vec![b"allowed".to_vec()]
        );
        assert!(
            store
                .lookup_at(now, 10, b"after", &DenyAll)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.lookup_at(now, 10, b"beta", &AllowAll).unwrap(),
            vec![b"hidden".to_vec()]
        );
        store.check_indexes_at(old).unwrap();
        store.check_indexes_at(now).unwrap();
    }
}
