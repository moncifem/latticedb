//! Typed schema/catalog/row integration against the simulated durable log.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use lattice_core::Config;
use lattice_engines::catalog::{self, CatalogOp};
use lattice_engines::limits::AllowAll;
use lattice_engines::store::RowStore;
use lattice_engines::typed_store::{Error, TypedStore};
use lattice_lsl::validate::Value;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type Store = TypedStore<SharedIo, SharedClock, SeededRng>;
fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 400,
        node: 1,
    }
}
fn fresh(seed: u64) -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(seed, FaultProfile::clean())),
        Rc::new(SimClock::new(1000)),
    )
}
fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64) -> Result<Store, Error> {
    TypedStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
    )
}
fn object(fields: &[(&str, Value)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}
fn ticket(points: i64) -> Value {
    object(&[
        ("title", Value::Text("work".into())),
        ("points", Value::Int(points)),
    ])
}
const V1: &str = "facet Titled { title: text max 12 } type Ticket = Titled { points: int in 0..=5 } type Doc = Titled { body: text } type Design = Titled { approved: bool }";
const V2: &str = "facet Titled { title: text max 12 } type Ticket = Titled { aaa: bool, points: int in 0..=10 } type Doc = Titled { body: text } type Design = Titled { approved: bool }";

#[test]
fn three_schema_types_have_distinct_physical_tables_and_survive_reopen() {
    let (disk, clock) = fresh(1);
    let mut s = open(&disk, &clock, 1).unwrap();
    let (version, lsn) = s.install_schema(V1).unwrap();
    let schema = s.catalog().schema(version).unwrap();
    assert_eq!(schema.layouts.len(), 3);
    assert_eq!(schema.installed_lsn, lsn);
    assert_eq!(s.catalog().as_of(lsn - 1).len(), 0);
    assert_eq!(s.catalog().as_of(lsn).len(), 3);
    assert_eq!(s.snapshot().as_of, lsn);
    for layout in schema.layouts.values() {
        assert!(
            !layout
                .columns
                .iter()
                .any(|c| c.name == "attribute" || c.name == "value")
        );
    }
    let mut tx = s.begin();
    s.put(&mut tx, "Ticket", b"a", &ticket(3)).unwrap();
    s.put(
        &mut tx,
        "Doc",
        b"a",
        &object(&[
            ("title", Value::Text("doc".into())),
            ("body", Value::Text("body".into())),
        ]),
    )
    .unwrap();
    s.put(
        &mut tx,
        "Design",
        b"a",
        &object(&[
            ("title", Value::Text("design".into())),
            ("approved", Value::Bool(true)),
        ]),
    )
    .unwrap();
    assert_eq!(s.get(&tx, "Ticket", b"a").unwrap(), Some(ticket(3)));
    s.commit(tx).unwrap();
    let snap = s.snapshot();
    drop(s);
    let s = open(&disk, &clock, 100).unwrap();
    assert_eq!(
        s.get_at(snap, "Ticket", b"a", &AllowAll).unwrap(),
        Some(ticket(3))
    );
    for name in ["Ticket", "Doc", "Design"] {
        assert_eq!(s.scan_at(snap, name, &AllowAll).unwrap().len(), 1);
    }
}

#[test]
fn validation_rejects_bad_refinements_missing_unknown_and_wrong_types_before_wal_append() {
    let (disk, clock) = fresh(2);
    let mut s = open(&disk, &clock, 2).unwrap();
    s.install_schema(V1).unwrap();
    let before = s.snapshot();
    let mut tx = s.begin();
    for invalid in [
        ticket(6),
        object(&[
            ("title", Value::Text("too long for title".into())),
            ("points", Value::Int(1)),
        ]),
        object(&[("title", Value::Text("work".into()))]),
        object(&[
            ("title", Value::Text("work".into())),
            ("points", Value::Bool(true)),
        ]),
        object(&[
            ("title", Value::Text("work".into())),
            ("points", Value::Int(1)),
            ("unknown", Value::Bool(true)),
        ]),
    ] {
        assert!(matches!(
            s.put(&mut tx, "Ticket", b"bad", &invalid),
            Err(Error::InvalidValue(_))
        ));
    }
    s.commit(tx).unwrap();
    assert_eq!(s.snapshot(), before);
    assert!(
        s.get_at(before, "Ticket", b"bad", &AllowAll)
            .unwrap()
            .is_none()
    );
}

#[test]
fn original_layout_dispatch_survives_reordering_and_recovery_and_schema_changes_fence_writers() {
    let (disk, clock) = fresh(3);
    let mut s = open(&disk, &clock, 3).unwrap();
    let (v1, _) = s.install_schema(V1).unwrap();
    let ids = s.catalog().current();
    let mut tx = s.begin();
    s.put(&mut tx, "Ticket", b"old", &ticket(3)).unwrap();
    s.commit(tx).unwrap();
    let old = s.snapshot();
    let mut stale = s.begin();
    s.put(&mut stale, "Ticket", b"stale", &ticket(2)).unwrap();
    let (v2, _) = s.install_schema(V2).unwrap();
    assert!(v2 > v1);
    assert_eq!(s.catalog().current(), ids);
    assert!(matches!(s.commit(stale), Err(Error::SchemaChanged)));
    let mut tx = s.begin();
    let new = object(&[
        ("aaa", Value::Bool(true)),
        ("title", Value::Text("work".into())),
        ("points", Value::Int(8)),
    ]);
    s.put(&mut tx, "Ticket", b"new", &new).unwrap();
    s.commit(tx).unwrap();
    let now = s.snapshot();
    assert_eq!(
        s.get_at(now, "Ticket", b"old", &AllowAll).unwrap(),
        Some(ticket(3))
    );
    drop(s);
    let s = open(&disk, &clock, 300).unwrap();
    assert_eq!(
        s.get_at(old, "Ticket", b"old", &AllowAll).unwrap(),
        Some(ticket(3))
    );
    assert_eq!(
        s.get_at(now, "Ticket", b"old", &AllowAll).unwrap(),
        Some(ticket(3))
    );
    assert_eq!(
        s.get_at(now, "Ticket", b"new", &AllowAll).unwrap(),
        Some(new)
    );
    assert!(
        s.get_at(old, "Ticket", b"new", &AllowAll)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_foreign_transaction_cannot_cross_stores_even_with_identical_schemas_and_seeds() {
    let (a, clock) = fresh(4);
    let (b, _) = fresh(4);
    let mut first = open(&a, &clock, 4).unwrap();
    let mut second = open(&b, &clock, 4).unwrap();
    first.install_schema(V1).unwrap();
    second.install_schema(V1).unwrap();
    let mut tx = first.begin();
    first.put(&mut tx, "Ticket", b"a", &ticket(2)).unwrap();
    assert!(matches!(second.commit(tx), Err(Error::ForeignTransaction)));
}

#[test]
fn raw_row_api_and_legacy_catalog_payloads_remain_usable_with_one_writer() {
    let (disk, clock) = fresh(5);
    let mut rows = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(5),
        &config(),
    )
    .unwrap();
    let ops = vec![CatalogOp::Create {
        table: 50,
        name: "raw".into(),
    }];
    assert_eq!(catalog::encode(&ops).unwrap().get(4), Some(&1));
    rows.commit_catalog(ops, Vec::new()).unwrap();
    let mut tx = rows.begin();
    tx.put((50, b"key".to_vec()), b"bytes".to_vec());
    rows.commit(tx).unwrap();
    let snap = rows.snapshot();
    drop(rows);
    let rows = RowStore::open(disk, SharedClock(clock), SeededRng::new(500), &config()).unwrap();
    assert_eq!(rows.catalog().id_of(snap.as_of, "raw"), Some(50));
    assert_eq!(
        rows.get_at(snap, 50, b"key", &AllowAll).unwrap(),
        Some(&b"bytes".to_vec())
    );
}

#[test]
fn schema_install_is_atomic_across_power_cuts_and_acknowledged_typed_rows_survive() {
    for cut in 0..35u64 {
        let (disk, clock) = fresh(cut + 10);
        let mut s = open(&disk, &clock, cut + 10).unwrap();
        disk.arm_power_cut(cut);
        let installed = s.install_schema(V1).is_ok();
        drop(s);
        disk.crash();
        disk.set_profile(FaultProfile::clean());
        let mut s = open(&disk, &clock, cut + 1000).unwrap();
        let count = s.catalog().current().len();
        assert!(
            count == 0 || count == 3,
            "cut {cut}: {count} partial tables"
        );
        if installed {
            assert_eq!(count, 3);
        }
        if count == 0 {
            s.install_schema(V1).unwrap();
        }
        disk.arm_power_cut(cut);
        let mut tx = s.begin();
        s.put(&mut tx, "Ticket", b"a", &ticket(3)).unwrap();
        s.put(
            &mut tx,
            "Doc",
            b"a",
            &object(&[
                ("title", Value::Text("doc".into())),
                ("body", Value::Text("body".into())),
            ]),
        )
        .unwrap();
        let acked = s.commit(tx).is_ok();
        drop(s);
        disk.crash();
        disk.set_profile(FaultProfile::clean());
        let s = open(&disk, &clock, cut + 2000).unwrap();
        let snap = s.snapshot();
        let ticket = s.get_at(snap, "Ticket", b"a", &AllowAll).unwrap().is_some();
        let doc = s.get_at(snap, "Doc", b"a", &AllowAll).unwrap().is_some();
        assert_eq!(ticket, doc, "cut {cut}: partial typed transaction");
        if acked {
            assert!(ticket, "cut {cut}: acknowledged row missing");
        }
    }
}

#[test]
fn read_only_transaction_returns_its_catalog_snapshot_and_does_not_append() {
    let (disk, clock) = fresh(70);
    let mut store = open(&disk, &clock, 70).unwrap();
    let (_, lsn) = store.install_schema(V1).unwrap();
    let tx = store.begin();
    assert_eq!(tx.snapshot().as_of, lsn);
    assert_eq!(store.commit(tx).unwrap(), lsn);
    assert_eq!(store.snapshot().as_of, lsn);
}

#[test]
fn schema_codec_is_canonical_bounded_and_semantic_failure_is_atomic() {
    use lattice_engines::catalog::{Catalog, CatalogError};
    use lattice_lsl::{format, parser};
    use std::collections::BTreeMap;
    let source = format::format(&parser::parse(V1).unwrap());
    let op = CatalogOp::DefineSchema {
        version: 1,
        source,
        tables: BTreeMap::from([
            (1, "Design".into()),
            (2, "Doc".into()),
            (3, "Ticket".into()),
        ]),
    };
    let encoded = catalog::encode(std::slice::from_ref(&op)).unwrap();
    assert_eq!(encoded.get(4), Some(&2));
    assert_eq!(catalog::decode(&encoded).unwrap(), vec![op.clone()]);
    assert_eq!(
        catalog::encode(&catalog::decode(&encoded).unwrap()).unwrap(),
        encoded
    );
    for len in 0..encoded.len() {
        assert!(
            catalog::decode(&encoded[..len]).is_err(),
            "prefix {len} accepted"
        );
    }
    let mut with_tail = encoded;
    with_tail.push(0);
    assert_eq!(catalog::decode(&with_tail), Err(CatalogError::Trailing));
    let mut catalog = Catalog::new();
    let mut invalid = op.clone();
    if let CatalogOp::DefineSchema { tables, .. } = &mut invalid {
        tables.remove(&1);
    }
    assert_eq!(
        catalog.apply(
            1,
            &[
                CatalogOp::Create {
                    table: 9,
                    name: "must-not-survive".into()
                },
                invalid
            ]
        ),
        Err(CatalogError::InvalidMapping)
    );
    assert!(catalog.current().is_empty());
    assert!(catalog.schema(1).is_none());
    catalog.apply(1, &[op]).unwrap();
    assert_eq!(catalog.current().len(), 3);
}

#[test]
fn malformed_known_schema_records_stop_row_recovery_instead_of_being_skipped() {
    use lattice_core::{ChangeSet, Kernel};
    use std::collections::BTreeMap;
    let (disk, clock) = fresh(80);
    let mut kernel = Kernel::open_streaming_with_capabilities(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(80),
        &config(),
        lattice_core::admission::MANAGED_COMPOUND | lattice_core::admission::TYPED_SCHEMA,
        |_, _| {},
    )
    .unwrap()
    .0;
    let payload = catalog::encode(&[CatalogOp::DefineSchema {
        version: 1,
        source: "not valid LSL".into(),
        tables: BTreeMap::from([(1, "Ticket".into())]),
    }])
    .unwrap();
    let id = kernel.new_id();
    kernel
        .commit_with_capabilities(
            ChangeSet::new(id, payload),
            lattice_core::admission::TYPED_SCHEMA,
        )
        .unwrap();
    drop(kernel);
    let result = RowStore::open(disk, SharedClock(clock), SeededRng::new(81), &config());
    assert!(matches!(
        result,
        Err(lattice_engines::store::RowError::Corrupt(_))
    ));
}

#[test]
fn independent_python_catalog_and_typed_envelope_goldens_are_byte_exact() {
    const CATALOG: &[u8] = include_bytes!("../../../formats/corpus/catalog-v2-ticket.golden");
    const ROW: &[u8] = include_bytes!("../../../formats/corpus/typed-envelope-v1-ticket.golden");
    let ops = catalog::decode(CATALOG).unwrap();
    assert_eq!(catalog::encode(&ops).unwrap(), CATALOG);
    let (disk, clock) = fresh(90);
    let mut raw = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(90),
        &config(),
    )
    .unwrap();
    raw.commit_catalog(ops, Vec::new()).unwrap();
    let mut tx = raw.begin();
    tx.put((42, b"golden".to_vec()), ROW.to_vec());
    raw.commit(tx).unwrap();
    drop(raw);
    let mut typed = open(&disk, &clock, 91).unwrap();
    let value = object(&[
        ("points", Value::Int(3)),
        ("title", Value::Text("hello".into())),
    ]);
    assert_eq!(
        typed
            .get_at(typed.snapshot(), "Ticket", b"golden", &AllowAll)
            .unwrap(),
        Some(value.clone())
    );
    let mut tx = typed.begin();
    typed.put(&mut tx, "Ticket", b"reencoded", &value).unwrap();
    typed.commit(tx).unwrap();
    let snap = typed.snapshot();
    drop(typed);
    let raw = RowStore::open(disk, SharedClock(clock), SeededRng::new(92), &config()).unwrap();
    assert_eq!(
        raw.get_at(snap, 42, b"reencoded", &AllowAll)
            .unwrap()
            .unwrap()
            .as_slice(),
        ROW
    );
}

#[test]
fn uncataloged_raw_history_is_never_adopted_by_new_typed_tables() {
    for deleted in [false, true] {
        let (disk, clock) = fresh(101 + u64::from(deleted));
        let mut raw = RowStore::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(101),
            &config(),
        )
        .unwrap();
        let mut tx = raw.begin();
        tx.put((1, b"k".to_vec()), b"raw".to_vec());
        raw.commit(tx).unwrap();
        let original = raw.snapshot();
        if deleted {
            let mut tx = raw.begin();
            tx.delete((1, b"k".to_vec()));
            raw.commit(tx).unwrap();
        }
        drop(raw);
        let mut typed = open(&disk, &clock, 102).unwrap();
        typed.install_schema("type Ticket { title: text }").unwrap();
        let table = typed
            .catalog()
            .id_of(typed.snapshot().as_of, "Ticket")
            .unwrap();
        assert_ne!(table, 1, "raw table history was reused, deleted={deleted}");
        assert!(
            typed
                .get_at(typed.snapshot(), "Ticket", b"k", &AllowAll)
                .unwrap()
                .is_none()
        );
        let value = object(&[("title", Value::Text("typed".into()))]);
        let mut tx = typed.begin();
        typed.put(&mut tx, "Ticket", b"k", &value).unwrap();
        typed.commit(tx).unwrap();
        drop(typed);
        let typed = open(&disk, &clock, 103).unwrap();
        assert_eq!(
            typed
                .get_at(typed.snapshot(), "Ticket", b"k", &AllowAll)
                .unwrap(),
            Some(value)
        );
        drop(typed);
        let raw = RowStore::open(disk, SharedClock(clock), SeededRng::new(104), &config()).unwrap();
        assert_eq!(
            raw.get_at(original, 1, b"k", &AllowAll).unwrap(),
            Some(&b"raw".to_vec())
        );
        if deleted {
            assert!(
                raw.get_at(raw.snapshot(), 1, b"k", &AllowAll)
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn catalogued_raw_tombstones_also_prevent_typed_adoption() {
    for deleted in [false, true] {
        let (disk, clock) = fresh(110 + u64::from(deleted));
        let mut raw = RowStore::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(110),
            &config(),
        )
        .unwrap();
        raw.commit_catalog(
            vec![CatalogOp::Create {
                table: 1,
                name: "Ticket".into(),
            }],
            Vec::new(),
        )
        .unwrap();
        let mut tx = raw.begin();
        tx.put((1, b"k".to_vec()), b"raw".to_vec());
        raw.commit(tx).unwrap();
        if deleted {
            let mut tx = raw.begin();
            tx.delete((1, b"k".to_vec()));
            raw.commit(tx).unwrap();
        }
        drop(raw);
        let mut typed = open(&disk, &clock, 111).unwrap();
        let before = typed.snapshot();
        assert!(
            matches!(
                typed.install_schema("type Ticket { title: text }"),
                Err(Error::Schema(_))
            ),
            "raw catalog table was adopted, deleted={deleted}"
        );
        assert_eq!(
            typed.snapshot(),
            before,
            "rejection appended a catalog record"
        );
        drop(typed);
        assert!(open(&disk, &clock, 112).is_ok());
    }
}

#[test]
fn delete_only_and_mixed_uncataloged_ids_remain_reserved_after_replay() {
    let (disk, clock) = fresh(120);
    let mut raw = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(120),
        &config(),
    )
    .unwrap();
    let mut tx = raw.begin();
    tx.delete((1, Vec::new()));
    tx.put((2, b"deleted".to_vec()), b"raw".to_vec());
    tx.put((4, b"live".to_vec()), b"raw".to_vec());
    raw.commit(tx).unwrap();
    let mut tx = raw.begin();
    tx.delete((2, b"deleted".to_vec()));
    raw.commit(tx).unwrap();
    for table in [1, 2, 4] {
        assert!(raw.has_table_history(table));
    }
    drop(raw);
    let mut typed = open(&disk, &clock, 121).unwrap();
    typed.install_schema(V1).unwrap();
    let ids: Vec<u32> = typed.catalog().current().into_keys().collect();
    assert_eq!(ids, vec![3, 5, 6]);
    drop(typed);
    let typed = open(&disk, &clock, 122).unwrap();
    assert_eq!(
        typed.catalog().current().into_keys().collect::<Vec<_>>(),
        ids
    );
    drop(typed);
    let raw = RowStore::open(disk, SharedClock(clock), SeededRng::new(123), &config()).unwrap();
    for table in [1, 2, 4] {
        assert!(raw.has_table_history(table));
    }
    assert!(
        raw.get_at(raw.snapshot(), 1, &[], &AllowAll)
            .unwrap()
            .is_none()
    );
    assert!(
        raw.get_at(raw.snapshot(), 2, b"deleted", &AllowAll)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        raw.get_at(raw.snapshot(), 4, b"live", &AllowAll).unwrap(),
        Some(&b"raw".to_vec())
    );
}

#[test]
fn schema_catalog_commits_and_replay_reject_prior_raw_history() {
    use lattice_core::{ChangeSet, Kernel};
    use lattice_engines::catalog::CatalogError;
    use lattice_engines::store::RowError;
    use lattice_lsl::{format, parser};
    use std::collections::BTreeMap;
    let (disk, clock) = fresh(130);
    let mut raw = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(130),
        &config(),
    )
    .unwrap();
    let mut tx = raw.begin();
    tx.delete((1, b"gone".to_vec()));
    raw.commit(tx).unwrap();
    let before = raw.snapshot();
    let op = CatalogOp::DefineSchema {
        version: 1,
        source: format::format(&parser::parse("type Ticket { title: text }").unwrap()),
        tables: BTreeMap::from([(1, "Ticket".into())]),
    };
    assert!(matches!(
        raw.commit_catalog(vec![op.clone()], Vec::new()),
        Err(RowError::InvalidCatalog(CatalogError::IdUsed(1)))
    ));
    assert_eq!(raw.snapshot(), before);
    assert!(raw.catalog().current().is_empty());
    drop(raw);
    // An explicit typed-capability writer can still lie about semantic validation; reject its record.
    let mut kernel = Kernel::open_streaming_with_capabilities(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(131),
        &config(),
        lattice_core::admission::MANAGED_COMPOUND | lattice_core::admission::TYPED_SCHEMA,
        |_, _| {},
    )
    .unwrap()
    .0;
    let id = kernel.new_id();
    kernel
        .commit_with_capabilities(
            ChangeSet::new(id, catalog::encode(&[op]).unwrap()),
            lattice_core::admission::TYPED_SCHEMA,
        )
        .unwrap();
    drop(kernel);
    assert!(matches!(
        RowStore::open(disk, SharedClock(clock), SeededRng::new(132), &config()),
        Err(RowError::Corrupt(_))
    ));
}

#[test]
fn raw_commits_enforce_current_typed_schema_before_wal_append() {
    use lattice_abi::{Classify, ErrorClass};
    use lattice_engines::{store::RowError, typed};
    let (disk, clock) = fresh(210);
    let mut typed = open(&disk, &clock, 210).unwrap();
    let (version, _) = typed.install_schema(V1).unwrap();
    let table = typed
        .catalog()
        .id_of(typed.snapshot().as_of, "Ticket")
        .unwrap();
    let layout = typed
        .catalog()
        .layout_at(table, typed.snapshot().as_of)
        .unwrap()
        .1;
    let encode = |value: &Value| {
        let mut bytes = b"LTYP".to_vec();
        bytes.push(1);
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&typed::encode_row(layout, value).unwrap());
        bytes
    };
    let valid = encode(&ticket(3));
    // Codec encoding itself does not enforce constraints; the store must enforce them.
    let invalid = encode(&ticket(99));
    drop(typed);
    let mut raw = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(211),
        &config(),
    )
    .unwrap();
    let before = raw.snapshot();
    for bytes in [b"untyped bytes".to_vec(), invalid] {
        let mut tx = raw.begin();
        tx.put((table, b"bad".to_vec()), bytes);
        tx.put((999, b"mixed".to_vec()), b"must not leak".to_vec());
        let error = raw.commit(tx).unwrap_err();
        assert!(matches!(error, RowError::InvalidTyped(_)));
        assert_eq!(error.class(), ErrorClass::Rejected);
        assert_eq!(raw.snapshot(), before);
        assert!(
            raw.get_at(raw.snapshot(), 999, b"mixed", &AllowAll)
                .unwrap()
                .is_none()
        );
    }
    let mut tx = raw.begin();
    tx.put((table, b"valid".to_vec()), valid.clone());
    raw.commit(tx).unwrap();
    drop(raw);
    let mut typed = open(&disk, &clock, 212).unwrap();
    assert_eq!(
        typed
            .get_at(typed.snapshot(), "Ticket", b"valid", &AllowAll)
            .unwrap(),
        Some(ticket(3))
    );
    typed.install_schema(V2).unwrap();
    drop(typed);
    let mut raw = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(213),
        &config(),
    )
    .unwrap();
    let before = raw.snapshot();
    let mut tx = raw.begin();
    tx.put((table, b"superseded".to_vec()), valid);
    assert!(matches!(raw.commit(tx), Err(RowError::InvalidTyped(_))));
    assert_eq!(raw.snapshot(), before);
    raw.commit_catalog(vec![CatalogOp::Drop { table }], Vec::new())
        .unwrap();
    let mut tx = raw.begin();
    tx.delete((table, b"valid".to_vec()));
    assert!(matches!(raw.commit(tx), Err(RowError::InvalidTyped(_))));
}

#[test]
fn replay_refuses_crc_valid_rows_that_violate_the_persisted_schema() {
    use lattice_core::{Kernel, changeset::ChangeSet};
    use lattice_engines::{rowops, store::RowError};
    let (disk, clock) = fresh(220);
    let mut typed = open(&disk, &clock, 220).unwrap();
    typed.install_schema(V1).unwrap();
    let table = typed
        .catalog()
        .id_of(typed.snapshot().as_of, "Ticket")
        .unwrap();
    drop(typed);
    // A deliberately lying explicit-capability writer injects corruption. Default old/core-only
    // clients cannot reopen this history; recovery still validates even trusted capability claims.
    let mut kernel = Kernel::open_streaming_with_capabilities(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(221),
        &config(),
        lattice_core::admission::MANAGED_COMPOUND | lattice_core::admission::TYPED_SCHEMA,
        |_, _| {},
    )
    .unwrap()
    .0;
    let id = kernel.new_id();
    let payload =
        rowops::encode(&[((table, b"bad".to_vec()), Some(b"invalid envelope".to_vec()))]).unwrap();
    kernel
        .commit_with_capabilities(
            ChangeSet::new(id, payload),
            lattice_core::admission::TYPED_SCHEMA,
        )
        .unwrap();
    drop(kernel);
    assert!(matches!(
        RowStore::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(222),
            &config()
        ),
        Err(RowError::Corrupt(_))
    ));
    assert!(matches!(
        open(&disk, &clock, 223),
        Err(Error::Store(RowError::Corrupt(_)))
    ));
}
