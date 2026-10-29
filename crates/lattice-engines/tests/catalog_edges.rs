//! Catalog edges found by mutation testing: name length limits in the encoder, the decoder and the
//! checker, renaming a table to its own name, and a change set the kernel rejects.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_engines::catalog::{
    Catalog, CatalogError, CatalogOp, CatalogStore, MAX_NAME, StoreError, decode, encode,
};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

fn create(table: u32, name: &str) -> CatalogOp {
    CatalogOp::Create {
        table,
        name: name.into(),
    }
}

fn rename(table: u32, name: &str) -> CatalogOp {
    CatalogOp::Rename {
        table,
        name: name.into(),
    }
}

#[test]
fn a_name_of_exactly_the_limit_is_accepted_everywhere_and_one_more_is_not() {
    let longest = "n".repeat(MAX_NAME);
    let too_long = "n".repeat(MAX_NAME + 1);
    let op = create(1, &longest);
    let bytes = encode(std::slice::from_ref(&op)).unwrap();
    assert_eq!(decode(&bytes).unwrap(), vec![op.clone()]);
    assert!(Catalog::new().check(&[op]).is_ok());
    assert_eq!(encode(&[create(1, &too_long)]), Err(CatalogError::BadName));
    assert_eq!(encode(&[create(1, "")]), Err(CatalogError::BadName));
    assert_eq!(encode(&[rename(1, &too_long)]), Err(CatalogError::BadName));
    assert_eq!(encode(&[rename(1, "")]), Err(CatalogError::BadName));
}

#[test]
fn the_decoder_refuses_a_name_that_is_empty_or_over_the_limit() {
    let good = encode(&[create(1, "ab")]).unwrap();
    // The name length is the u32 just before the two name bytes at the end.
    let at = good.len() - 2 - 4;
    for (len, expected) in [
        (0u32, CatalogError::BadName),
        (MAX_NAME as u32 + 1, CatalogError::BadName),
    ] {
        let mut bytes = good.clone();
        bytes[at..at + 4].copy_from_slice(&len.to_le_bytes());
        bytes.resize(at + 4 + (len as usize).max(2), b'x');
        assert_eq!(decode(&bytes), Err(expected), "length {len}");
    }
    let mut bytes = good.clone();
    bytes[at..at + 4].copy_from_slice(&(MAX_NAME as u32).to_le_bytes());
    bytes.truncate(at + 4);
    bytes.extend(std::iter::repeat_n(b'x', MAX_NAME));
    assert!(decode(&bytes).is_ok());
}

#[test]
fn the_checker_applies_the_same_name_rules_to_create_and_rename() {
    let mut catalog = Catalog::new();
    catalog.apply(1, &[create(1, "a"), create(2, "b")]).unwrap();
    let longest = "n".repeat(MAX_NAME);
    let too_long = "n".repeat(MAX_NAME + 1);
    assert!(catalog.check(&[create(3, &longest)]).is_ok());
    assert!(catalog.check(&[rename(1, &longest)]).is_ok());
    for bad in ["", too_long.as_str()] {
        assert_eq!(catalog.check(&[create(3, bad)]), Err(CatalogError::BadName));
        assert_eq!(catalog.check(&[rename(1, bad)]), Err(CatalogError::BadName));
    }
}

#[test]
fn renaming_a_table_to_its_own_name_is_fine_but_to_another_tables_name_is_not() {
    let mut catalog = Catalog::new();
    catalog.apply(1, &[create(1, "a"), create(2, "b")]).unwrap();
    assert!(catalog.check(&[rename(1, "a")]).is_ok());
    assert_eq!(
        catalog.check(&[rename(1, "b")]),
        Err(CatalogError::NameTaken("b".into()))
    );
}

#[test]
fn a_change_the_kernel_rejects_leaves_the_store_usable() {
    let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let config = Config {
        dir: "wal".into(),
        segment_limit: 1 << 20,
        node: 1,
    };
    let mut store = CatalogStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(1),
        &config,
    )
    .unwrap();
    // More than the 16 MiB record limit: one table renamed 64,000 times to 255-byte names.
    let mut ops = vec![create(1, "first")];
    ops.extend((0..64_000u32).map(|i| rename(1, &format!("{i:0>width$}", width = MAX_NAME))));
    match store.commit(ops) {
        Err(StoreError::Rejected(_)) => {}
        other => panic!("expected the kernel to reject an oversized change, got {other:?}"),
    }
    // Nothing was written and the store did not go stale.
    assert_eq!(store.catalog().current().len(), 0);
    assert!(store.commit(vec![create(1, "after")]).is_ok());
}

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 1 << 20,
        node: 1,
    }
}

#[test]
fn a_failure_after_the_write_started_is_in_doubt_and_makes_the_store_stale() {
    let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let mut store = CatalogStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(1),
        &config(),
    )
    .unwrap();
    store.commit(vec![create(1, "a")]).unwrap();
    disk.arm_power_cut(0);
    match store.commit(vec![create(2, "b")]) {
        Err(StoreError::InDoubt(_)) => {}
        other => panic!("expected an in-doubt commit after a power cut, got {other:?}"),
    }
    assert_eq!(store.commit(vec![create(3, "c")]), Err(StoreError::Stale));
}

#[test]
fn records_of_other_kinds_in_the_shared_log_are_skipped_when_the_catalog_is_rebuilt() {
    use lattice_core::Kernel;
    use lattice_core::changeset::TurnRef;
    let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
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
    let mut store = CatalogStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(2),
        &config(),
    )
    .unwrap();
    assert!(store.catalog().current().is_empty());
    assert!(store.commit(vec![create(1, "a")]).is_ok());
}
