//! The bounded feed read (issues #581 and #543): `read_feed_as` must return exactly what the
//! full-scan implementation it replaced returned, in pages, while examining only the commits after
//! the cursor and never more than its bounds.
//!
//! The oracle is [`full_scan`], a copy of the previous implementation: it walks every key of the
//! version table (`Table::history_from`, kept for tests only) and builds every event before it
//! filters. The cost claims are checked with the version table's own visit counter, not with time.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::*;
use lattice_core::Config;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use proptest::prelude::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

type Store = RowStore<SharedIo, SharedClock, SeededRng>;
type Key = (u32, Vec<u8>);
/// One acknowledged commit in the model: its lsn and its writes in key order.
type Commit = (u64, Vec<(Key, Option<Vec<u8>>)>);
/// What a generated commit writes: table, key byte, value byte (`None` deletes).
type Flat = (Cursor, u32, Vec<u8>, Option<Vec<u8>>, u32);
/// What a generated commit writes (see below)
type Gen = Vec<Vec<(u32, u8, Option<u8>)>>;

fn open(disk: &SharedIo, clock: &Rc<SimClock>) -> Store {
    clock.advance(1);
    RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(5),
        &Config {
            segment_limit: 512,
            ..Config::default()
        },
    )
    .unwrap()
}

fn fresh() -> (SharedIo, Rc<SimClock>, Store) {
    let disk = SharedIo::new(SimIo::new(7, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let store = open(&disk, &clock);
    (disk, clock, store)
}

/// Commits `plan` and returns what was acknowledged, built from the generator and not read back.
fn commit_all(store: &mut Store, clock: &Rc<SimClock>, plan: &Gen) -> Vec<Commit> {
    let mut history = Vec::new();
    for writes in plan {
        clock.advance(1);
        let mut tx = store.begin();
        let mut batch: BTreeMap<Key, Option<Vec<u8>>> = BTreeMap::new();
        for (table, k, v) in writes {
            let key = (*table, vec![*k]);
            match v {
                Some(b) => {
                    let value = vec![*b, *k];
                    tx.put(key.clone(), value.clone());
                    batch.insert(key, Some(value));
                }
                None => {
                    tx.delete(key.clone());
                    batch.insert(key, None);
                }
            }
        }
        let lsn = store.commit(tx).unwrap();
        history.push((lsn, batch.into_iter().collect()));
    }
    history
}

fn every_cursor(history: &[Commit]) -> Vec<Cursor> {
    let mut cs = vec![Cursor::ORIGIN];
    for (lsn, writes) in history {
        for i in 0..writes.len() as u32 {
            cs.push(Cursor::new(*lsn, i).unwrap());
        }
    }
    cs
}

/// The previous implementation, kept as the oracle: every key of every table is visited, every
/// event is built (hidden tables included) and only then filtered.
fn full_scan<P: PolicyEvaluator + ?Sized>(
    store: &Store,
    cursor: Cursor,
    policy: &P,
) -> Result<Vec<ChangeEvent>, FeedReadError> {
    let floor = store.oldest_snapshot().as_of;
    if floor > 0 && cursor.lsn() <= floor {
        return Err(FeedReadError::CursorTooOld { floor });
    }
    let history = store.versions().history_from(cursor.lsn().max(1));
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(first) = history.get(at) {
        let lsn = first.lsn;
        let len = history[at..].iter().take_while(|e| e.lsn == lsn).count();
        let group = &history[at..at + len];
        at += len;
        let mut cs = ChangeSet::new(first.cs, Vec::new());
        cs.hlc = store
            .commit_time_at(lsn)
            .ok_or(FeedReadError::MissingCommitTime { lsn })?;
        let changes = group
            .iter()
            .map(|e| EntityChange {
                kind: e.key.0,
                key: if e.key.1.len() > MAX_KEY_BYTES {
                    Vec::new()
                } else {
                    e.key.1.clone()
                },
                op: match e.value {
                    Some(v) => ChangeOp::Upsert(v.clone()),
                    None => ChangeOp::Delete,
                },
            })
            .collect();
        let events = events_for(lsn, &cs, changes).map_err(FeedReadError::Feed)?;
        for (event, entry) in events.into_iter().zip(group) {
            if event.cursor <= cursor {
                continue;
            }
            let allowed = entry.value.or(entry.previous).is_some_and(|value| {
                policy.allows_read(RowRef {
                    table: entry.key.0,
                    key: &entry.key.1,
                    value,
                })
            });
            if !allowed {
                continue;
            }
            if entry.key.1.len() > MAX_KEY_BYTES {
                if out.is_empty() {
                    return Err(FeedReadError::KeyTooLarge {
                        cursor: event.cursor,
                        len: entry.key.1.len(),
                    });
                }
                return Ok(out);
            }
            out.push(event);
        }
    }
    Ok(out)
}

/// Reads to the end in pages, resuming from each page's `resume`, and checks every page against the
/// bounds. Returns the events and the number of pages.
fn read_all<P: PolicyEvaluator + ?Sized>(
    store: &Store,
    from: Cursor,
    table: Option<u32>,
    policy: &P,
    limits: FeedLimits,
) -> Result<(Vec<ChangeEvent>, usize), FeedReadError> {
    let mut events = Vec::new();
    let mut at = from;
    for pages in 1..10_000 {
        let page = store.read_feed_as(at, table, policy, limits)?;
        assert!(page.events.len() <= limits.max_events.max(1), "event limit");
        assert!(page.scanned <= limits.max_scanned.max(1), "scan limit");
        assert!(page.events.iter().all(|e| e.cursor > at), "strictly after");
        assert!(
            page.events.len() <= page.scanned,
            "an event is a scanned version"
        );
        events.extend(page.events);
        if !page.more {
            return Ok((events, pages));
        }
        assert!(page.resume > at, "a page that says more must have moved");
        at = page.resume;
    }
    panic!("paging does not end");
}

struct EvenFirstByte(u32);
impl PolicyEvaluator for EvenFirstByte {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        row.table != self.0 && row.value.first().is_some_and(|b| b % 2 == 0)
    }
}

struct Nothing;
impl PolicyEvaluator for Nothing {}

/// Counts the rows it is asked about, per table.
struct Counting(RefCell<BTreeMap<u32, usize>>);
impl PolicyEvaluator for Counting {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        *self.0.borrow_mut().entry(row.table).or_default() += 1;
        true
    }
}

fn gen_history() -> impl Strategy<Value = Gen> {
    prop::collection::vec(
        prop::collection::vec(
            (0u32..3, 0u8..4, prop::option::weighted(0.75, 0u8..6)),
            1..5,
        ),
        1..10,
    )
}

proptest! {
    /// For every generated history, cursor, limit pair, table filter, policy and vacuum horizon, the
    /// pages of the bounded read concatenate to exactly what the full scan returned (or fail the same
    /// way), and without limits equal the model built from the commits.
    #[test]
    fn bounded_pages_equal_the_full_scan_oracle(
        plan in gen_history(),
        vac in prop::option::of(0u64..10),
        pick in 0usize..64,
        max_events in 1usize..6,
        max_scanned in 1usize..8,
        table in prop::option::of(0u32..3),
        policy_kind in 0u8..3,
    ) {
        let (_d, clock, mut store) = fresh();
        let history = commit_all(&mut store, &clock, &plan);
        if let Some(back) = vac {
            let applied = store.applied_lsn();
            store.vacuum(applied.saturating_sub(back));
        }
        let cursors = every_cursor(&history);
        let cursor = cursors[pick % cursors.len()];
        let policy: &dyn PolicyEvaluator = match policy_kind {
            0 => &AllowAll,
            1 => &EvenFirstByte(1),
            _ => &Nothing,
        };
        let limits = FeedLimits { max_events, max_scanned };
        let oracle = full_scan(&store, cursor, policy).map(|events| {
            events
                .into_iter()
                .filter(|e| table.is_none_or(|t| e.change.kind == t))
                .collect::<Vec<_>>()
        });
        let paged = read_all(&store, cursor, table, policy, limits).map(|(events, _)| events);
        prop_assert_eq!(&paged, &oracle);
        if let Ok(page) = store.read_feed_as(cursor, table, policy, FeedLimits::UNBOUNDED) {
            prop_assert!(!page.more);
            if let Ok(want) = &oracle {
                prop_assert_eq!(&page.events, want);
            }
        }
        if store.oldest_snapshot().as_of == 0 && policy_kind == 0 && table.is_none() {
            // The model never looked at the version table.
            let model: Vec<Flat> = {
                let mut state: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
                let mut out = Vec::new();
                for (lsn, writes) in &history {
                    for (i, (key, value)) in writes.iter().enumerate() {
                        let c = Cursor::new(*lsn, i as u32).unwrap();
                        // A delete of a key that had no value changes nothing a reader could see.
                        let seen = value.is_some() || state.contains_key(key);
                        if c > cursor && seen {
                            out.push((c, key.0, key.1.clone(), value.clone(), writes.len() as u32));
                        }
                    }
                    for (key, value) in writes {
                        match value {
                            Some(v) => state.insert(key.clone(), v.clone()),
                            None => state.remove(key),
                        };
                    }
                }
                out
            };
            let got: Vec<_> = paged
                .unwrap()
                .into_iter()
                .map(|e| {
                    let v = match e.change.op {
                        ChangeOp::Delete => None,
                        ChangeOp::Upsert(v) => Some(v),
                    };
                    (e.cursor, e.change.kind, e.change.key, v, e.set_len)
                })
                .collect();
            // A delete of a key that never existed is hidden from the readable-only oracle but not
            // from allow-all, so the model needs no state to compare.
            prop_assert_eq!(got, model);
        }
    }
}

fn big_history(commits: usize, per_commit: usize) -> Gen {
    (0..commits)
        .map(|c| {
            (0..per_commit)
                .map(|i| {
                    (
                        0u32,
                        ((c * per_commit + i) % 200) as u8,
                        Some((c % 6) as u8),
                    )
                })
                .collect()
        })
        .collect()
}

#[test]
fn a_caught_up_reader_visits_no_version_however_large_the_table() {
    // Keys are single bytes, so a table of this size has only 200 distinct keys but thousands of
    // versions: the point is that the cost depends on the new commits only.
    for commits in [20, 200] {
        let (_d, clock, mut store) = fresh();
        let history = commit_all(&mut store, &clock, &big_history(commits, 5));
        let last = {
            let (lsn, w) = history.last().unwrap();
            Cursor::new(*lsn, w.len() as u32 - 1).unwrap()
        };
        let before = store.versions().visited();
        let page = store
            .read_feed_as(last, None, &AllowAll, FeedLimits::UNBOUNDED)
            .unwrap();
        assert!(page.events.is_empty() && !page.more);
        assert_eq!(page.scanned, 0);
        assert_eq!(
            store.versions().visited(),
            before,
            "{commits} commits: a caught-up read must not touch the rows"
        );
        // Two new commits: the read visits exactly their versions, whatever came before.
        let more = commit_all(&mut store, &clock, &big_history(2, 3));
        let new: usize = more.iter().map(|(_, w)| w.len()).sum();
        let before = store.versions().visited();
        let page = store
            .read_feed_as(last, None, &AllowAll, FeedLimits::UNBOUNDED)
            .unwrap();
        assert_eq!(page.events.len(), new);
        assert_eq!(store.versions().visited() - before, new as u64);
    }
}

#[test]
fn one_read_examines_at_most_its_scan_bound_and_resumes_exactly() {
    let (_d, clock, mut store) = fresh();
    let history = commit_all(&mut store, &clock, &big_history(40, 4));
    let total: usize = history.iter().map(|(_, w)| w.len()).sum();
    // A reader that may see nothing: every version is passed over, 10 per read.
    let limits = FeedLimits {
        max_events: 3,
        max_scanned: 10,
    };
    let mut at = Cursor::ORIGIN;
    let mut reads = 0;
    let mut visited = 0u64;
    loop {
        let before = store.versions().visited();
        let page = store.read_feed_as(at, None, &Nothing, limits).unwrap();
        visited += store.versions().visited() - before;
        assert!(page.events.is_empty());
        assert!(page.scanned <= 10);
        assert_eq!(store.versions().visited() - before, page.scanned as u64);
        reads += 1;
        if !page.more {
            break;
        }
        assert!(page.resume > at);
        at = page.resume;
    }
    assert_eq!(visited, total as u64, "every version examined exactly once");
    assert_eq!(reads, total.div_ceil(10));
}

#[test]
fn one_read_builds_at_most_max_events_and_says_when_there_is_more() {
    let (_d, clock, mut store) = fresh();
    let history = commit_all(&mut store, &clock, &big_history(30, 4));
    let total: usize = history.iter().map(|(_, w)| w.len()).sum();
    let before = store.versions().visited();
    let page = store
        .read_feed_as(
            Cursor::ORIGIN,
            None,
            &AllowAll,
            FeedLimits {
                max_events: 3,
                max_scanned: 1_000,
            },
        )
        .unwrap();
    assert_eq!(page.events.len(), 3);
    assert!(page.more);
    assert_eq!(page.resume, page.events[2].cursor);
    assert_eq!(
        store.versions().visited() - before,
        3,
        "O(limit), not O(history)"
    );
    // Exactly at the end: the last event fills the limit and there is nothing after it.
    let last = {
        let (lsn, w) = history.last().unwrap();
        Cursor::new(*lsn, w.len() as u32 - 2).unwrap()
    };
    let page = store
        .read_feed_as(
            last,
            None,
            &AllowAll,
            FeedLimits {
                max_events: 1,
                max_scanned: 9,
            },
        )
        .unwrap();
    assert_eq!((page.events.len(), page.more), (1, false));
    assert!(total > 100);
}

#[test]
fn a_table_filter_passes_other_tables_over_without_asking_the_policy() {
    let (_d, clock, mut store) = fresh();
    let plan: Gen = (0..6)
        .map(|c| {
            vec![
                (0, c as u8 % 4, Some(1)),
                (1, c as u8 % 4, Some(2)),
                (2, 0, Some(3)),
            ]
        })
        .collect();
    commit_all(&mut store, &clock, &plan);
    let counting = Counting(RefCell::new(BTreeMap::new()));
    let page = store
        .read_feed_as(Cursor::ORIGIN, Some(1), &counting, FeedLimits::UNBOUNDED)
        .unwrap();
    assert_eq!(page.events.len(), 6);
    assert!(page.events.iter().all(|e| e.change.kind == 1));
    assert_eq!(page.scanned, 18, "other tables still count as examined");
    // A denied or hidden table's rows are never shown to the policy, let alone built.
    assert_eq!(*counting.0.borrow(), BTreeMap::from([(1, 6)]));
}

#[test]
fn the_index_is_rebuilt_on_open_and_a_caught_up_read_is_still_free() {
    let (disk, clock, mut store) = fresh();
    let history = commit_all(&mut store, &clock, &big_history(30, 4));
    let all_before = read_all(
        &store,
        Cursor::ORIGIN,
        None,
        &AllowAll,
        FeedLimits {
            max_events: 7,
            max_scanned: 11,
        },
    )
    .unwrap()
    .0;
    assert_eq!(all_before.len(), 120);
    drop(store);
    disk.crash();
    disk.restart_process();
    let mut store = open(&disk, &clock);
    let all_after = read_all(
        &store,
        Cursor::ORIGIN,
        None,
        &AllowAll,
        FeedLimits {
            max_events: 7,
            max_scanned: 11,
        },
    )
    .unwrap()
    .0;
    assert_eq!(all_after, all_before, "same events from the rebuilt index");
    let (lsn, w) = history.last().unwrap();
    let last = Cursor::new(*lsn, w.len() as u32 - 1).unwrap();
    let before = store.versions().visited();
    let page = store
        .read_feed_as(last, None, &AllowAll, FeedLimits::UNBOUNDED)
        .unwrap();
    assert!(page.events.is_empty());
    assert_eq!(store.versions().visited(), before);
    // New commits after the reopen join the index.
    let more = commit_all(&mut store, &clock, &big_history(1, 2));
    let page = store
        .read_feed_as(last, None, &AllowAll, FeedLimits::UNBOUNDED)
        .unwrap();
    assert_eq!(page.events.len(), more[0].1.len());
}

#[test]
fn vacuum_prunes_the_index_and_keeps_the_cursor_rules() {
    let (_d, clock, mut store) = fresh();
    let history = commit_all(&mut store, &clock, &big_history(20, 3));
    let applied = store.applied_lsn();
    let floor = store.vacuum(applied - 5).horizon;
    assert!(floor > 0);
    // Nothing at or below the floor is reachable through the index.
    assert!(store.versions().commits_from(0).all(|c| c.lsn() > floor));
    for cursor in [Cursor::ORIGIN, Cursor::new(floor, 0).unwrap()] {
        assert_eq!(
            store
                .read_feed_as(cursor, None, &AllowAll, FeedLimits::UNBOUNDED)
                .err(),
            Some(FeedReadError::CursorTooOld { floor })
        );
    }
    let after = Cursor::new(floor + 1, 0).unwrap();
    let (events, _) = read_all(
        &store,
        after,
        None,
        &AllowAll,
        FeedLimits {
            max_events: 4,
            max_scanned: 4,
        },
    )
    .unwrap();
    let want: usize = history
        .iter()
        .filter(|(l, _)| *l > floor)
        .map(|(_, w)| w.len())
        .sum();
    assert_eq!(events.len(), want - 1, "everything after (floor + 1, 0)");
}

// The planted bugs: each corrupts the bound it is named for, and the check above must fail.

struct BugGuard;
impl Drop for BugGuard {
    fn drop(&mut self) {
        lattice_core::bugs::clear();
    }
}

fn witness<F: Fn() -> Result<(), String>>(bug: Bug, check: F) {
    lattice_core::bugs::clear();
    let _guard = BugGuard;
    assert_eq!(check(), Ok(()), "the check must pass without the bug");
    lattice_core::bugs::enable(bug);
    assert!(
        check().is_err(),
        "{bug:?} must make the bounded-read check fail"
    );
}

#[test]
fn the_event_limit_planted_bug_is_caught() {
    witness(Bug::FeedLimitIgnored, || {
        let (_d, clock, mut store) = fresh();
        commit_all(&mut store, &clock, &big_history(10, 4));
        let page = store
            .read_feed_as(
                Cursor::ORIGIN,
                None,
                &AllowAll,
                FeedLimits {
                    max_events: 2,
                    max_scanned: 100,
                },
            )
            .map_err(|e| e.to_string())?;
        if page.events.len() > 2 || !page.more {
            return Err(format!("{} events, more {}", page.events.len(), page.more));
        }
        Ok(())
    });
}

#[test]
fn the_scan_bound_planted_bug_is_caught() {
    witness(Bug::FeedScanBudgetIgnored, || {
        let (_d, clock, mut store) = fresh();
        commit_all(&mut store, &clock, &big_history(10, 4));
        let before = store.versions().visited();
        let page = store
            .read_feed_as(
                Cursor::ORIGIN,
                None,
                &Nothing,
                FeedLimits {
                    max_events: 2,
                    max_scanned: 5,
                },
            )
            .map_err(|e| e.to_string())?;
        let visited = store.versions().visited() - before;
        if visited > 5 || page.scanned > 5 || !page.more {
            return Err(format!("visited {visited}, scanned {}", page.scanned));
        }
        Ok(())
    });
}

#[test]
fn the_commit_index_planted_bug_is_caught() {
    witness(Bug::FeedIndexCommitSkipped, || {
        let (_d, clock, mut store) = fresh();
        let history = commit_all(&mut store, &clock, &big_history(6, 3));
        let want: usize = history.iter().map(|(_, w)| w.len()).sum();
        let got = store
            .events_since(Cursor::ORIGIN)
            .map_err(|e| e.to_string())?
            .count();
        if got != want {
            return Err(format!("{got} events, {want} committed"));
        }
        Ok(())
    });
}
