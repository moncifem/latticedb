//! Strict historical HLC resolution: literal predecessors, retention and reopen behavior.
#![cfg(feature = "row")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_abi::Hlc;
use lattice_core::bugs::{self, Bug};
use lattice_core::changeset::TurnRef;
use lattice_core::{Config, Kernel};
use lattice_engines::limits::AllowAll;
use lattice_engines::mvcc::Snapshot;
use lattice_engines::store::{RowError, RowStore};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;

fn hlc(wall_ms: u64, counter: u32, node: u32) -> Hlc {
    Hlc {
        wall_ms,
        counter,
        node,
    }
}

fn config() -> Config {
    Config {
        dir: "wal".into(),
        node: 1,
        ..Config::default()
    }
}

fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64) -> Store {
    RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
    )
    .expect("fixture recovery must succeed")
}

fn write(store: &mut Store, value: &[u8]) -> u64 {
    let mut tx = store.begin();
    tx.put((1, b"key".to_vec()), value.to_vec());
    store.commit(tx).expect("fixture commit must succeed")
}

fn turn(disk: &SharedIo, clock: &Rc<SimClock>, turn: u64) {
    let (mut kernel, _) = Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(100 + turn),
        &config(),
    )
    .expect("fixture kernel recovery must succeed");
    kernel
        .begin_turn(TurnRef { session: 1, turn })
        .expect("fixture turn must commit");
}

// Deliberate WAL gaps: turn at LSN1, row at2, turn at3, rows at4 and5. Timestamps are
// literal fixture inputs, not values learned from either timestamp resolver under test.
fn fixture() -> (SharedIo, Rc<SimClock>, Store) {
    let disk = SharedIo::new(SimIo::new(41, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    turn(&disk, &clock, 1);
    let mut store = open(&disk, &clock, 1);
    assert_eq!(write(&mut store, b"one"), 2);
    assert_eq!(store.commit_time_at(2), Some(hlc(1_000, 0, 1)));
    drop(store);
    turn(&disk, &clock, 2);
    let mut store = open(&disk, &clock, 2);
    assert_eq!(write(&mut store, b"two"), 4);
    assert_eq!(store.commit_time_at(4), Some(hlc(1_000, 1, 1)));
    clock.advance(10);
    assert_eq!(write(&mut store, b"three"), 5);
    assert_eq!(store.commit_time_at(5), Some(hlc(1_010, 0, 1)));
    (disk, clock, store)
}

const CASES: &[(Hlc, u64)] = &[
    (
        Hlc {
            wall_ms: 0,
            counter: 0,
            node: 0,
        },
        0,
    ),
    (
        Hlc {
            wall_ms: 999,
            counter: u32::MAX,
            node: u32::MAX,
        },
        0,
    ),
    (
        Hlc {
            wall_ms: 1_000,
            counter: 0,
            node: 0,
        },
        0,
    ),
    (
        Hlc {
            wall_ms: 1_000,
            counter: 0,
            node: 1,
        },
        2,
    ),
    (
        Hlc {
            wall_ms: 1_000,
            counter: 0,
            node: u32::MAX,
        },
        2,
    ),
    (
        Hlc {
            wall_ms: 1_000,
            counter: 1,
            node: 0,
        },
        2,
    ),
    (
        Hlc {
            wall_ms: 1_000,
            counter: 1,
            node: 1,
        },
        4,
    ),
    (
        Hlc {
            wall_ms: 1_009,
            counter: u32::MAX,
            node: u32::MAX,
        },
        4,
    ),
    (
        Hlc {
            wall_ms: 1_010,
            counter: 0,
            node: 0,
        },
        4,
    ),
    (
        Hlc {
            wall_ms: 1_010,
            counter: 0,
            node: 1,
        },
        5,
    ),
    (
        Hlc {
            wall_ms: u64::MAX,
            counter: u32::MAX,
            node: u32::MAX,
        },
        5,
    ),
];

fn check_cases(store: &Store, floor: u64) {
    for &(time, predecessor) in CASES {
        let expected = if predecessor < floor {
            Err(RowError::SnapshotTooOld { floor })
        } else {
            Ok(Snapshot { as_of: predecessor })
        };
        assert_eq!(
            store.try_snapshot_as_of_hlc(time),
            expected,
            "{time:?} at floor {floor}"
        );
        // Pin the independently specified old clamping contract alongside the strict API.
        assert_eq!(
            store.snapshot_as_of_hlc_clamped(time),
            Snapshot {
                as_of: predecessor.max(floor)
            }
        );
        if predecessor >= floor {
            let value = match predecessor {
                0 => None,
                2 => Some(b"one".to_vec()),
                4 => Some(b"two".to_vec()),
                5 => Some(b"three".to_vec()),
                _ => panic!("unknown literal fixture LSN"),
            };
            assert_eq!(
                store.get_at(Snapshot { as_of: predecessor }, 1, b"key", &AllowAll),
                Ok(value.as_ref())
            );
        }
    }
}

#[test]
fn strict_hlc_empty_store_accepts_empty_past_and_future_snapshots() {
    let disk = SharedIo::new(SimIo::new(42, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let mut store = open(&disk, &clock, 1);
    store.vacuum(u64::MAX);
    for time in [Hlc::default(), hlc(u64::MAX, u32::MAX, u32::MAX)] {
        assert_eq!(
            store.try_snapshot_as_of_hlc(time),
            Ok(Snapshot { as_of: 0 })
        );
    }
}

#[test]
fn strict_hlc_uses_full_hlc_and_gapped_lsn_predecessors_without_vacuum() {
    let (_, _, store) = fixture();
    check_cases(&store, 0);
}

#[test]
fn strict_hlc_rejects_expired_history_including_a_floor_between_row_commits() {
    let (_, _, mut store) = fixture();
    assert_eq!(store.vacuum(3).horizon, 3);
    check_cases(&store, 3);
    assert_eq!(store.vacuum(4).horizon, 4);
    check_cases(&store, 4);
}

#[test]
fn strict_hlc_reopen_restores_history_and_reapplied_vacuum_restores_errors() {
    let (disk, clock, mut store) = fixture();
    store.vacuum(3);
    check_cases(&store, 3);
    drop(store);
    disk.crash();
    let mut replayed = open(&disk, &clock, 3);
    check_cases(&replayed, 0);
    assert_eq!(replayed.vacuum(3).horizon, 3);
    check_cases(&replayed, 3);
}

struct Enabled;
impl Drop for Enabled {
    fn drop(&mut self) {
        bugs::clear();
    }
}

#[test]
fn strict_hlc_expiration_oracle_detects_clamping_defect_and_clean_control() {
    let _guard = Enabled;
    bugs::clear();
    let (_, _, mut store) = fixture();
    store.vacuum(4);
    let request = hlc(1_000, 0, 1);
    let expected = Err(RowError::SnapshotTooOld { floor: 4 });
    assert_eq!(store.try_snapshot_as_of_hlc(request), expected);
    assert_eq!(Bug::HLC_SNAPSHOT, [Bug::HlcSnapshotClamped]);
    bugs::enable(Bug::HlcSnapshotClamped);
    // Exact wrong success, not a panic, setup failure or unrelated error, is the planted witness.
    assert_eq!(
        store.try_snapshot_as_of_hlc(request),
        Ok(Snapshot { as_of: 4 })
    );
    assert_ne!(store.try_snapshot_as_of_hlc(request), expected);
    bugs::clear();
    assert_eq!(store.try_snapshot_as_of_hlc(request), expected);
}
