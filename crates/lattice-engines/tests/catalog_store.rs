//! The catalog against the simulated disk: persistence, versions, sharing one log with the row store,
//! and atomic visibility of a catalog change under power cuts.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_engines::catalog::{CatalogError, CatalogOp, CatalogStore, StoreError};
use lattice_engines::store::RowStore;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type Cat = CatalogStore<SharedIo, SharedClock, SeededRng>;

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 600,
        node: 1,
    }
}

fn fresh() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(1, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}

fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64) -> Result<Cat, StoreError> {
    CatalogStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
    )
}

fn create(table: u32, name: &str) -> CatalogOp {
    CatalogOp::Create {
        table,
        name: name.into(),
    }
}

#[test]
fn the_catalog_and_its_versions_survive_a_restart() {
    let (disk, clock) = fresh();
    let mut cat = open(&disk, &clock, 1).unwrap();
    let a = cat
        .commit(vec![create(1, "issues"), create(2, "pages")])
        .unwrap();
    let b = cat
        .commit(vec![CatalogOp::Rename {
            table: 1,
            name: "tickets".into(),
        }])
        .unwrap();
    let c = cat.commit(vec![CatalogOp::Drop { table: 2 }]).unwrap();
    drop(cat);
    let cat = open(&disk, &clock, 2).unwrap();
    let catalog = cat.catalog();
    assert_eq!(catalog.id_of(a, "issues"), Some(1));
    assert_eq!(catalog.id_of(b, "tickets"), Some(1));
    assert_eq!(catalog.id_of(b, "pages"), Some(2));
    assert_eq!(catalog.id_of(c, "pages"), None);
    assert_eq!(catalog.current().len(), 1);
}

#[test]
fn a_refused_change_writes_nothing() {
    let (disk, clock) = fresh();
    let mut cat = open(&disk, &clock, 1).unwrap();
    cat.commit(vec![create(1, "a")]).unwrap();
    let before = cat.catalog().applied_lsn();
    let err = cat
        .commit(vec![create(2, "b"), CatalogOp::Drop { table: 9 }])
        .unwrap_err();
    assert_eq!(err, StoreError::Invalid(CatalogError::NoSuchTable(9)));
    assert_eq!(cat.catalog().applied_lsn(), before);
    drop(cat);
    let cat = open(&disk, &clock, 2).unwrap();
    assert_eq!(
        cat.catalog().current().len(),
        1,
        "the refused change set left no trace in the log"
    );
}

#[test]
fn the_catalog_and_the_row_store_share_one_log_without_reading_each_others_change_sets() {
    let (disk, clock) = fresh();
    let mut cat = open(&disk, &clock, 1).unwrap();
    cat.commit(vec![create(1, "issues")]).unwrap();
    drop(cat);
    let mut rows = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(2),
        &config(),
    )
    .unwrap();
    let mut tx = rows.begin();
    tx.put((1, b"k".to_vec()), b"v".to_vec());
    rows.commit(tx).unwrap();
    drop(rows);
    let mut cat = open(&disk, &clock, 3).unwrap();
    assert_eq!(cat.catalog().current().len(), 1);
    cat.commit(vec![create(2, "pages")]).unwrap();
    drop(cat);
    let rows = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(4),
        &config(),
    )
    .unwrap();
    assert_eq!(
        rows.get_at(rows.snapshot(), 1, b"k").unwrap(),
        Some(&b"v".to_vec())
    );
    drop(rows); // one writer at a time holds the log
    let cat = open(&disk, &clock, 5).unwrap();
    assert_eq!(cat.catalog().current().len(), 2);
}

/// Every catalog change creates two tables, so after any crash a change is present as both tables or
/// neither, and everything acknowledged is present.
#[test]
fn a_catalog_change_is_all_there_or_not_at_all_across_crashes() {
    for seed in 0..60u64 {
        let (disk, clock) = fresh();
        let mut rng = SeededRng::new(seed);
        let mut acked: Vec<u32> = Vec::new();
        let mut attempted: Vec<u32> = Vec::new();
        let mut cat = Some(open(&disk, &clock, seed).unwrap());
        for n in 0..40u32 {
            if rng.below(6) == 0 {
                disk.arm_power_cut(rng.below(8));
            }
            let Some(c) = cat.as_mut() else {
                disk.crash();
                cat = open(&disk, &clock, seed + 1000 + u64::from(n)).ok();
                continue;
            };
            attempted.push(n);
            let ops = vec![
                create(10 * n + 1, &format!("t{n}a")),
                create(10 * n + 2, &format!("t{n}b")),
            ];
            match c.commit(ops) {
                Ok(_) => acked.push(n),
                Err(_) => drop(cat.take()),
            }
            if rng.below(8) == 0 {
                drop(cat.take());
                disk.crash();
                cat = open(&disk, &clock, seed + 2000 + u64::from(n)).ok();
            }
        }
        drop(cat);
        disk.crash();
        disk.set_profile(FaultProfile::clean());
        let cat = open(&disk, &clock, seed + 9000).unwrap();
        let now = cat.catalog().current();
        for n in attempted {
            let a = now.contains_key(&(10 * n + 1));
            let b = now.contains_key(&(10 * n + 2));
            assert_eq!(a, b, "seed {seed}: change {n} is partial");
            if acked.contains(&n) {
                assert!(a, "seed {seed}: acknowledged change {n} was lost");
            }
        }
    }
}
