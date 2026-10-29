//! Harness compiled against actual immutable source 45926c7, never a copied decoder.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use lattice_core::{Config, Kernel};
use lattice_engines::catalog::CatalogStore;
use lattice_engines::index::IndexDef;
use lattice_engines::limits::AllowAll;
use lattice_engines::store::RowStore;
use lattice_seams::{OsRng, StdIo, SystemClock};
use std::sync::atomic::{AtomicUsize, Ordering};
static CALLBACKS: AtomicUsize = AtomicUsize::new(0);
fn callback(value: &[u8]) -> Option<Vec<u8>> {
    CALLBACKS.fetch_add(1, Ordering::Relaxed);
    Some(value.to_vec())
}
fn refused<T, E: std::fmt::Debug>(result: Result<T, E>, kind: &str) {
    let error = match result {
        Ok(_) => panic!("{kind} accepted typed history"),
        Err(error) => format!("{error:?}"),
    };
    assert!(
        error.contains("header") && error.contains("unknown or empty required capabilities"),
        "unrelated {kind} refusal: {error}"
    );
    println!("{kind}: {error}");
}
fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("case directory");
    let mode = args.next().expect("legacy or guarded");
    let config = Config {
        dir: "wal".into(),
        ..Config::default()
    };
    if mode == "legacy" {
        let rows = RowStore::open(StdIo::new(&dir), SystemClock, OsRng, &config)
            .expect("legacy prefix readable");
        assert_eq!(
            rows.get_at(rows.snapshot(), 9, b"prefix", &AllowAll)
                .unwrap(),
            Some(&b"legacy".to_vec())
        );
        println!("legacy literal prefix preserved");
        return;
    }
    assert_eq!(mode, "guarded");
    refused(
        Kernel::open(StdIo::new(&dir), SystemClock, OsRng, &config),
        "core",
    );
    refused(
        CatalogStore::open(StdIo::new(&dir), SystemClock, OsRng, &config),
        "catalog",
    );
    refused(
        RowStore::open(StdIo::new(&dir), SystemClock, OsRng, &config),
        "managed",
    );
    refused(
        RowStore::open_with_indexes(
            StdIo::new(&dir),
            SystemClock,
            OsRng,
            &config,
            vec![IndexDef {
                id: 9,
                table: 9,
                extract: callback,
            }],
        ),
        "callback",
    );
    assert_eq!(
        CALLBACKS.load(Ordering::Relaxed),
        0,
        "callback ran before refusal"
    );
    println!("four actual frozen readers refused; callbacks=0");
}
