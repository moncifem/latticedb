//! The row key bound (issue 444): the store refuses a put whose key the change feed cannot carry,
//! and a log that already holds such a row does not stop the feed for readers who cannot see it.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::bugs::{self, Bug};
use lattice_core::feed::{ChangeEvent, Cursor, MAX_KEY_LEN};
use lattice_core::{ChangeSet, Config, Kernel};
use lattice_engines::catalog::{CatalogOp, IndexOp};
use lattice_engines::feed_emit::FeedReadError;
use lattice_engines::index::{IndexKey, ManagedIndexDef};
use lattice_engines::limits::{AllowAll, PolicyEvaluator, RowRef};
use lattice_engines::rowops::{self, MAX_KEY_BYTES};
use lattice_engines::store::{RowError, RowStore};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;

fn config() -> Config {
    Config {
        segment_limit: 4096,
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

struct BugGuard;
impl Drop for BugGuard {
    fn drop(&mut self) {
        bugs::clear();
    }
}

/// Reads rows of one table only.
struct OnlyTable(u32);
impl PolicyEvaluator for OnlyTable {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        row.table == self.0
    }
}

fn keys(events: std::vec::IntoIter<ChangeEvent>) -> Vec<Vec<u8>> {
    events.map(|e| e.change.key).collect()
}

#[test]
fn the_store_and_feed_share_one_key_bound() {
    assert_eq!(MAX_KEY_BYTES, 4096);
    assert_eq!(MAX_KEY_BYTES, MAX_KEY_LEN);
}

#[test]
fn a_commit_with_an_oversize_key_is_refused_whole_and_leaves_the_store_usable() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    let applied = store.applied_lsn();
    let too_long = RowError::KeyTooLong {
        len: MAX_KEY_BYTES + 1,
        max: MAX_KEY_BYTES,
    };

    let mut tx = store.begin();
    tx.put((7, b"fine".to_vec()), b"v".to_vec());
    tx.put((7, vec![b'k'; MAX_KEY_BYTES + 1]), b"v".to_vec());
    assert_eq!(store.commit(tx), Err(too_long.clone()));
    // Nothing was appended, neither the oversize row nor its neighbour, and the store is not stale.
    assert_eq!(store.applied_lsn(), applied);
    assert_eq!(store.events_since(Cursor::ORIGIN).unwrap().count(), 0);
    let mut tx = store.begin();
    tx.put((7, b"fine".to_vec()), b"v".to_vec());
    let lsn = store.commit(tx).unwrap();
    assert_eq!(store.events_since(Cursor::ORIGIN).unwrap().count(), 1);

    // The bound is inclusive: a key of exactly the limit is accepted and reaches the feed.
    let mut tx = store.begin();
    tx.put((7, vec![b'k'; MAX_KEY_BYTES]), b"v".to_vec());
    let edge = store.commit(tx).unwrap();
    assert_eq!(edge, lsn + 1);
    let events: Vec<_> = store.events_since(Cursor::ORIGIN).unwrap().collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].change.key.len(), MAX_KEY_BYTES);

    // The compound (catalog) commit path enforces the same bound.
    let mut tx = store.begin();
    tx.put((7, vec![b'k'; MAX_KEY_BYTES + 1]), b"v".to_vec());
    let create = CatalogOp::Create {
        table: 9,
        name: "t".into(),
    };
    assert_eq!(
        store.commit_with_catalog(tx, vec![create], vec![]),
        Err(too_long)
    );
    assert_eq!(store.applied_lsn(), edge);
    assert_eq!(
        RowError::KeyTooLong {
            len: 5000,
            max: 4096
        }
        .to_string(),
        "row key of 5000 bytes exceeds the 4096 byte limit"
    );
}

#[test]
fn the_oversize_key_planted_bug_is_caught_by_the_feed() {
    for mutated in [false, true] {
        bugs::clear();
        let _guard = BugGuard;
        let (disk, clock) = fresh();
        let mut store = open(&disk, &clock);
        if mutated {
            bugs::enable(Bug::RowOversizeKeyAccepted);
        }
        let mut tx = store.begin();
        tx.put((7, vec![b'k'; MAX_KEY_BYTES + 1]), b"v".to_vec());
        let committed = store.commit(tx);
        let feed = store
            .events_since(Cursor::ORIGIN)
            .map(|events| events.count());
        if mutated {
            // The defect lets one writer poison the feed for everyone.
            assert!(committed.is_ok());
            assert!(
                matches!(feed, Err(FeedReadError::KeyTooLarge { .. })),
                "{feed:?}"
            );
        } else {
            assert!(matches!(committed, Err(RowError::KeyTooLong { .. })));
            assert_eq!(feed.unwrap(), 0);
        }
    }
}

/// What a binary from before the key bound could have written: the plain row payload.
fn legacy_log(disk: &SharedIo, clock: &Rc<SimClock>, commits: Vec<Vec<rowops::RowWrite>>) {
    let (mut kernel, _) = Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(23),
        &config(),
    )
    .unwrap();
    for writes in commits {
        let id = kernel.new_id();
        kernel
            .commit(ChangeSet::new(id, rowops::encode(&writes).unwrap()))
            .unwrap();
    }
}

#[test]
fn a_legacy_oversize_key_stops_only_readers_who_can_see_it_and_can_be_skipped() {
    let (disk, clock) = fresh();
    let big = vec![b'b'; MAX_KEY_BYTES + 1];
    legacy_log(
        &disk,
        &clock,
        vec![
            vec![((7, b"a".to_vec()), Some(b"1".to_vec()))],
            // Table 7 sorts before table 8, so the oversize row is index 1 of this set.
            vec![
                ((7, b"c".to_vec()), Some(b"2".to_vec())),
                ((8, big.clone()), Some(b"3".to_vec())),
            ],
            vec![((7, b"d".to_vec()), Some(b"4".to_vec()))],
        ],
    );
    clock.advance(1);
    let mut store = open(&disk, &clock);

    // A reader who cannot read table 8 gets the whole feed, as if the row were not there.
    assert_eq!(
        keys(
            store
                .events_since_as(Cursor::ORIGIN, &OnlyTable(7))
                .unwrap()
        ),
        vec![b"a".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    // A reader who can read it is delivered everything before the blocked event; the read stops
    // there, so the prefix is not lost.
    let blocked = Cursor::new(2, 1).unwrap();
    let mut seen = Vec::new();
    let first = store
        .events_since_as(Cursor::ORIGIN, &AllowAll)
        .unwrap()
        .collect::<Vec<_>>();
    assert_eq!(
        first.last().map(|e| e.cursor),
        Some(Cursor::new(2, 0).unwrap())
    );
    seen.extend(first.iter().map(|e| e.change.key.clone()));
    // A read that starts at the blocked event fails with an error naming it.
    let error = FeedReadError::KeyTooLarge {
        cursor: blocked,
        len: MAX_KEY_BYTES + 1,
    };
    let after_prefix = first.last().unwrap().cursor;
    assert_eq!(
        store.events_since_as(after_prefix, &AllowAll).err(),
        Some(error)
    );
    // Reading again from the blocked cursor skips exactly that event. The whole sequence is a, c, d.
    seen.extend(keys(store.events_since_as(blocked, &AllowAll).unwrap()));
    assert_eq!(seen, vec![b"a".to_vec(), b"c".to_vec(), b"d".to_vec()]);

    // The row can still be removed, and a delete of a row that is not there reaches nobody.
    let mut tx = store.begin();
    tx.delete((8, big.clone()));
    tx.delete((8, vec![b'z'; MAX_KEY_BYTES + 1]));
    let removed = store.commit(tx).unwrap();
    assert_eq!(
        store
            .get_at(store.begin().snapshot(), 8, &big, &AllowAll)
            .unwrap(),
        None
    );
    assert_eq!(
        keys(
            store
                .events_since_as(Cursor::new(3, 0).unwrap(), &OnlyTable(7))
                .unwrap()
        ),
        Vec::<Vec<u8>>::new()
    );

    // After a vacuum past the oversize history, a cursor at the floor is refused as too old (never
    // as an oversize key) and later events arrive.
    store.vacuum(removed);
    let floor = store.oldest_snapshot().as_of;
    assert_eq!(
        store.events_since(Cursor::new(floor, 0).unwrap()).err(),
        Some(FeedReadError::CursorTooOld { floor })
    );
    for key in [b"e", b"f"] {
        let mut tx = store.begin();
        tx.put((8, key.to_vec()), b"5".to_vec());
        store.commit(tx).unwrap();
    }
    assert_eq!(
        keys(
            store
                .events_since(Cursor::new(floor + 1, 0).unwrap())
                .unwrap()
        ),
        vec![b"f".to_vec()]
    );
}

#[test]
fn the_bound_is_on_row_keys_not_on_index_values() {
    let (disk, clock) = fresh();
    let mut store = open(&disk, &clock);
    store
        .commit_catalog(
            vec![CatalogOp::Create {
                table: 7,
                name: "t".into(),
            }],
            vec![IndexOp::Create(ManagedIndexDef {
                id: 1,
                table: 7,
                name: "by_value".into(),
                key: IndexKey::WholeValue,
            })],
        )
        .unwrap();
    let mut tx = store.begin();
    tx.put(
        (7, vec![b'k'; MAX_KEY_BYTES]),
        vec![b'v'; MAX_KEY_BYTES * 2],
    );
    store.commit(tx).unwrap();
    assert_eq!(store.events_since(Cursor::ORIGIN).unwrap().count(), 1);
}

#[test]
fn a_bounded_read_pages_past_a_legacy_oversize_key_one_event_at_a_time() {
    use lattice_engines::feed_emit::FeedLimits;
    let (disk, clock) = fresh();
    legacy_log(
        &disk,
        &clock,
        vec![
            vec![((7, b"a".to_vec()), Some(b"1".to_vec()))],
            vec![
                ((7, b"c".to_vec()), Some(b"2".to_vec())),
                ((8, vec![b'b'; MAX_KEY_BYTES + 1]), Some(b"3".to_vec())),
            ],
            vec![((7, b"d".to_vec()), Some(b"4".to_vec()))],
        ],
    );
    clock.advance(1);
    let store = open(&disk, &clock);
    let limits = FeedLimits {
        max_events: 1,
        max_scanned: 2,
    };
    // A reader of table 7 never reaches the oversize row of table 8: it is passed over.
    let mut at = Cursor::ORIGIN;
    let mut seen = Vec::new();
    loop {
        let page = store.read_feed_as(at, Some(7), &AllowAll, limits).unwrap();
        seen.extend(page.events.iter().map(|e| e.change.key.clone()));
        if !page.more {
            break;
        }
        at = page.resume;
    }
    assert_eq!(seen, vec![b"a".to_vec(), b"c".to_vec(), b"d".to_vec()]);
    // A reader of every table gets the prefix, is told about the block, then skips it.
    let blocked = Cursor::new(2, 1).unwrap();
    let error = store
        .read_feed_as(
            Cursor::new(2, 0).unwrap(),
            None,
            &AllowAll,
            FeedLimits {
                max_events: 5,
                max_scanned: 5,
            },
        )
        .err();
    assert_eq!(
        error,
        Some(FeedReadError::KeyTooLarge {
            cursor: blocked,
            len: MAX_KEY_BYTES + 1
        })
    );
    let page = store
        .read_feed_as(blocked, None, &AllowAll, limits)
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].change.key, b"d".to_vec());
    assert!(!page.more);
}
