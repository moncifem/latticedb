//! Proposed typed capability: common validation, accepted-format coexistence and refusal boundaries.
#![cfg(feature = "row")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_abi::Hlc;
use lattice_core::admission::{MANAGED_COMPOUND, TYPED_SCHEMA};
use lattice_core::bugs::{self, Bug};
use lattice_core::{Config, Kernel};
use lattice_engines::catalog::{self, CatalogOp, CatalogStore, CompoundChange, IndexOp};
use lattice_engines::index::{IndexDef, IndexKey, ManagedIndexDef};
use lattice_engines::limits::{AllowAll, DenyAll};
use lattice_engines::store::{RowError, RowStore};
use lattice_engines::typed_store::{Error, TypedStore};
use lattice_lsl::validate::Value;
use lattice_seams::Io;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::collections::BTreeMap;
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;
const SCHEMA: &str = "type Ticket { points: int in 0..=9 title: text max 20 }";
fn config() -> Config {
    Config {
        dir: "wal".into(),
        node: 1,
        ..Config::default()
    }
}
fn fresh() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(611, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}
fn open(disk: &SharedIo, clock: &Rc<SimClock>) -> Store {
    RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(611),
        &config(),
    )
    .unwrap()
}
fn value() -> Value {
    Value::Object(BTreeMap::from([
        ("points".into(), Value::Int(3)),
        ("title".into(), Value::Text("hello".into())),
    ]))
}
fn schema_op() -> CatalogOp {
    CatalogOp::DefineSchema {
        version: 7,
        source: lattice_lsl::format::format(&lattice_lsl::parser::parse(SCHEMA).unwrap()),
        tables: BTreeMap::from([(42, "Ticket".into())]),
    }
}
fn manifest(disk: &SharedIo) -> BTreeMap<String, Vec<u8>> {
    disk.list("wal")
        .unwrap()
        .into_iter()
        .map(|name| {
            let bytes = disk.read(&format!("wal/{name}")).unwrap();
            (name, bytes)
        })
        .collect()
}
fn never_called(_: &[u8]) -> Option<Vec<u8>> {
    panic!("refused typed history invoked callback")
}

#[test]
fn accepted_lcix_v1_remains_v1_and_nested_typed_catalog_requires_proposed_v2() {
    let legacy = CompoundChange {
        tables: vec![CatalogOp::Create {
            table: 42,
            name: "Ticket".into(),
        }],
        indexes: vec![],
        rows: vec![],
    };
    let bytes = catalog::encode_compound(&legacy).unwrap();
    assert_eq!(bytes.get(4), Some(&1));
    assert_eq!(catalog::decode_compound(&bytes).unwrap(), legacy);
    let typed = CompoundChange {
        tables: vec![schema_op()],
        indexes: vec![],
        rows: vec![],
    };
    let mut encoded = catalog::encode_compound(&typed).unwrap();
    assert_eq!(encoded.get(4), Some(&2));
    assert_eq!(catalog::decode_compound(&encoded).unwrap(), typed);
    *encoded.get_mut(4).unwrap() = 1;
    assert!(matches!(
        catalog::decode_compound(&encoded),
        Err(catalog::CatalogError::UnsupportedVersion(2))
    ));
    let mut false_v2 = bytes;
    *false_v2.get_mut(4).unwrap() = 2;
    assert!(matches!(
        catalog::decode_compound(&false_v2),
        Err(catalog::CatalogError::UnsupportedVersion(2))
    ));
    assert_eq!(TYPED_SCHEMA, 4);
    assert!(
        lattice_core::admission::decode_header(include_bytes!(
            "../../../formats/corpus/wal-admission-v2-unknown-capability.golden"
        ))
        .is_err()
    );
}

#[test]
fn typed_history_refuses_default_core_catalog_and_callback_consumers_without_repair() {
    let (disk, clock) = fresh();
    let mut rows = open(&disk, &clock);
    rows.commit_catalog(vec![schema_op()], vec![]).unwrap();
    drop(rows);
    // A damaged tail must remain untouched when capability preflight refuses the history.
    let segment = disk
        .list("wal")
        .unwrap()
        .into_iter()
        .find(|name| name.ends_with(".seg"))
        .unwrap();
    disk.clone()
        .append(&format!("wal/{segment}"), &[0xff, 0x01])
        .unwrap();
    let before = manifest(&disk);
    assert!(
        Kernel::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(612),
            &config()
        )
        .is_err()
    );
    assert_eq!(manifest(&disk), before);
    assert!(
        CatalogStore::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(613),
            &config()
        )
        .is_err()
    );
    assert_eq!(manifest(&disk), before);
    assert!(
        RowStore::open_with_indexes(
            disk.clone(),
            SharedClock(clock),
            SeededRng::new(614),
            &config(),
            vec![IndexDef {
                id: 9,
                table: 42,
                extract: never_called
            }]
        )
        .is_err()
    );
    assert_eq!(manifest(&disk), before);
}

#[test]
fn catalog_only_writer_cannot_advertise_or_append_typed_schemas() {
    let (disk, clock) = fresh();
    let mut catalog = CatalogStore::open(
        disk.clone(),
        SharedClock(clock),
        SeededRng::new(615),
        &config(),
    )
    .unwrap();
    let before = manifest(&disk);
    assert!(catalog.commit(vec![schema_op()]).is_err());
    assert!(catalog.commit_managed(vec![schema_op()], vec![]).is_err());
    assert_eq!(manifest(&disk), before);
}

#[test]
fn managed_indexes_and_raw_compounds_share_typed_validation_and_original_layouts() {
    let (disk, clock) = fresh();
    let mut rows = open(&disk, &clock);
    rows.commit_catalog(
        vec![schema_op()],
        vec![IndexOp::Create(ManagedIndexDef {
            id: 9,
            table: 42,
            name: "by_bytes".into(),
            key: IndexKey::WholeValue,
        })],
    )
    .unwrap();
    let encoded =
        include_bytes!("../../../formats/corpus/typed-envelope-v1-ticket.golden").to_vec();
    let mut tx = rows.begin();
    tx.put((42, b"a".to_vec()), encoded.clone());
    let old = rows.commit(tx).unwrap();
    assert_eq!(
        rows.lookup_at(rows.snapshot(), 9, &encoded, &AllowAll)
            .unwrap(),
        vec![b"a".to_vec()]
    );
    let before = manifest(&disk);
    let mut invalid = rows.begin();
    invalid.put((42, b"invalid".to_vec()), b"not typed".to_vec());
    assert!(matches!(
        rows.commit_with_catalog(invalid, vec![], vec![IndexOp::Drop { id: 9 }]),
        Err(RowError::InvalidTyped(_))
    ));
    assert_eq!(manifest(&disk), before);
    assert!(rows.catalog().indexes().contains_key(&9));
    rows.check_indexes().unwrap();
    drop(rows);
    let rows = open(&disk, &clock);
    assert_eq!(
        rows.lookup_at(
            lattice_engines::mvcc::Snapshot { as_of: old },
            9,
            &encoded,
            &AllowAll
        )
        .unwrap(),
        vec![b"a".to_vec()]
    );
    rows.check_indexes().unwrap();
    assert_eq!(MANAGED_COMPOUND | TYPED_SCHEMA, 5);
}

#[test]
fn typed_policy_and_strict_hlc_are_not_bypassed_by_the_wrapper() {
    let (disk, clock) = fresh();
    let mut typed = TypedStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(616),
        &config(),
    )
    .unwrap();
    typed.install_schema(SCHEMA).unwrap();
    let mut tx = typed.begin();
    typed.put(&mut tx, "Ticket", b"a", &value()).unwrap();
    let old = typed.commit(tx).unwrap();
    let denied = typed.begin_with_policy(DenyAll);
    assert_eq!(typed.get(&denied, "Ticket", b"a").unwrap(), None);
    assert!(
        typed
            .scan_at(typed.snapshot(), "Ticket", &DenyAll)
            .unwrap()
            .is_empty()
    );
    let mut denied = typed.begin_with_policy(DenyAll);
    typed
        .put(&mut denied, "Ticket", b"denied", &value())
        .unwrap();
    let before = manifest(&disk);
    assert!(matches!(
        typed.commit(denied),
        Err(Error::Store(RowError::PermissionDenied))
    ));
    assert_eq!(manifest(&disk), before);
    let mut tx = typed.begin();
    typed.put(&mut tx, "Ticket", b"b", &value()).unwrap();
    let floor = typed.commit(tx).unwrap();
    typed.vacuum(floor);
    assert!(
        matches!(typed.get_at(lattice_engines::mvcc::Snapshot { as_of:old },"missing",b"a",&AllowAll),Err(Error::Store(RowError::SnapshotTooOld { floor:f })) if f==floor)
    );
    assert!(
        matches!(typed.try_snapshot_as_of_hlc(Hlc::default()),Err(Error::Store(RowError::SnapshotTooOld { floor:f })) if f==floor)
    );
}

struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        bugs::clear();
    }
}
#[test]
fn typed_validation_planted_defect_is_detected_at_the_raw_boundary() {
    let _reset = Reset;
    bugs::clear();
    let (disk, clock) = fresh();
    let mut rows = open(&disk, &clock);
    rows.commit_catalog(vec![schema_op()], vec![]).unwrap();
    let mut invalid = rows.begin();
    invalid.put((42, b"bad".to_vec()), b"not typed".to_vec());
    assert!(matches!(
        rows.commit(invalid.clone()),
        Err(RowError::InvalidTyped(_))
    ));
    assert_eq!(Bug::TYPED_ROWS, [Bug::TypedSchemaValidationSkipped]);
    bugs::enable(Bug::TypedSchemaValidationSkipped);
    rows.commit(invalid)
        .expect("specific planted mismatch is acceptance, not setup panic");
    assert_eq!(
        rows.get_at(rows.snapshot(), 42, b"bad", &AllowAll).unwrap(),
        Some(&b"not typed".to_vec())
    );
    bugs::clear();
    drop(rows);
    assert!(matches!(
        RowStore::open(disk, SharedClock(clock), SeededRng::new(617), &config()),
        Err(RowError::Corrupt(_))
    ));
}

#[test]
fn independent_typed_capability_fixtures_decode_and_validate_with_current_engine() {
    for bytes in [
        include_bytes!("../../../formats/corpus/typed-admission-cap4-schema-rows.golden")
            .as_slice(),
        include_bytes!("../../../formats/corpus/typed-admission-cap5-schema-rows.golden")
            .as_slice(),
    ] {
        let (disk, clock) = fresh();
        disk.clone().create_dir_all("wal").unwrap();
        disk.clone()
            .append("wal/00000000000000000001.seg", bytes)
            .unwrap();
        disk.clone().sync("wal/00000000000000000001.seg").unwrap();
        let store =
            TypedStore::open(disk, SharedClock(clock), SeededRng::new(618), &config()).unwrap();
        assert_eq!(
            store
                .get_at(store.snapshot(), "Ticket", b"key", &AllowAll)
                .unwrap(),
            Some(value())
        );
    }
}

/// Successful actual-engine histories plus independent vectors are hosted proof inputs, not a soak.
#[test]
#[ignore = "exports host files for the separately compiled immutable 45926c7 reader"]
fn export_typed_admission_histories_for_frozen_reader() {
    use std::path::Path;
    fn export(disk: &SharedIo, root: &Path, mode: &str) {
        let mut host = lattice_seams::StdIo::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
        host.create_dir_all(&root.join("wal").display().to_string())
            .unwrap();
        for (name, bytes) in manifest(disk) {
            host.append(&root.join("wal").join(name).display().to_string(), &bytes)
                .unwrap();
        }
        host.append(&root.join("wal/LOCK").display().to_string(), &[])
            .unwrap();
        host.append(
            &root.join("mode.txt").display().to_string(),
            mode.as_bytes(),
        )
        .unwrap();
    }
    let root = Path::new("target/typed-admission-cases");
    let host = lattice_seams::StdIo::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    assert!(
        !host.exists("target/typed-admission-cases"),
        "export destination must be new"
    );
    let (disk, clock) = fresh();
    let mut raw = open(&disk, &clock);
    let mut tx = raw.begin();
    tx.put((9, b"prefix".to_vec()), b"legacy".to_vec());
    raw.commit(tx).unwrap();
    drop(raw);
    export(&disk, &root.join("actual-legacy"), "legacy");
    let mut typed = TypedStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(619),
        &config(),
    )
    .unwrap();
    typed.install_schema(SCHEMA).unwrap();
    let mut tx = typed.begin();
    typed.put(&mut tx, "Ticket", b"typed", &value()).unwrap();
    typed.commit(tx).unwrap();
    drop(typed);
    export(&disk, &root.join("actual-typed"), "guarded");
    let segment = disk
        .list("wal")
        .unwrap()
        .into_iter()
        .filter(|name| name.ends_with(".seg"))
        .max()
        .unwrap();
    disk.clone()
        .append(&format!("wal/{segment}"), &[0xff, 0x01])
        .unwrap();
    export(&disk, &root.join("actual-typed-torn-tail"), "guarded");
    for (name, bytes) in [
        (
            "independent-cap4",
            include_bytes!("../../../formats/corpus/typed-admission-cap4-schema-rows.golden")
                .as_slice(),
        ),
        (
            "independent-cap5",
            include_bytes!("../../../formats/corpus/typed-admission-cap5-schema-rows.golden")
                .as_slice(),
        ),
    ] {
        let (case, _) = fresh();
        case.clone().create_dir_all("wal").unwrap();
        case.clone()
            .append("wal/00000000000000000001.seg", bytes)
            .unwrap();
        export(&case, &root.join(name), "guarded");
    }
}
