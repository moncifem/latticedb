//! Change feed emission from committed row changes (WP36, issue #238).
//!
//! The row store keeps every committed row version at or above its vacuum floor, and the log is the
//! durable copy, so a reopened store has the same versions and therefore the same feed. This module
//! reads that history and turns each committed change set into the events of `formats/change-feed-v1.md`:
//! one event per row write, cursor `(commit lsn, index)`, ordered by commit order. Nothing here writes
//! or keeps state, so the feed after a kill and reopen is the feed before it, and a consumer that resumes
//! from the last cursor it processed sees no gap and no repeat.
//!
//! * **Index.** The index of a write inside its change set is its position in `(table, key)` order, which
//!   is the order the row store writes the payload in. Rewriting a key twice in one transaction is one
//!   write (last wins), as it is in the log.
//! * **Delivery.** Events come in cursor order, so the changes to one entity, and to related entities
//!   committed one after the other, arrive in commit order. A consumer that saves its cursor only after
//!   processing an event can see an event again after a crash of its own, never miss one.
//! * **Scope.** Events are filtered by the actor's read policy (`PolicyEvaluator::allows_read`). An
//!   upsert is emitted if the actor can read the new value. A delete is emitted if the actor could read
//!   the value it removed. Cursors and `set_len` are those of the unfiltered change set, so a filtered
//!   feed has gaps in its indexes; a gap is not a loss. A row that changes from readable to unreadable
//!   produces no event for that actor.
//! * **Resume.** Events strictly after the cursor. Vacuum removes row versions below its floor, and
//!   deletions that are the newest version at the floor, so the change set at the floor itself may be
//!   incomplete. A cursor at or below a non-zero floor is therefore refused with
//!   [`FeedReadError::CursorTooOld`]; the consumer must re-snapshot instead of resuming at a different
//!   position. With no vacuum the floor is 0 and every cursor, including [`Cursor::ORIGIN`], is valid.
//! * **Key bound.** The store refuses a put whose key is over [`MAX_KEY_BYTES`], so new history never
//!   holds one. A log from before that rule may. Such a row cannot be an event. A reader who may
//!   read it receives the events before it (the read stops there, so a short result is not the end
//!   of the feed), and a read that starts at the blocked event fails with
//!   [`FeedReadError::KeyTooLarge`] naming its cursor. Reading again from that cursor skips exactly
//!   that event, so nothing else is lost. A reader who may not read it is unaffected.
//! * **Provenance.** The store commits with no author or turn, so events carry author 0 and no turn.
//!
//! Cost: `history_from` visits every key of every table, so a call is O(total rows) even for a
//! caught-up reader, plus O(versions at or after the cursor's LSN); events (with cloned values) are
//! built for every write after the cursor, hidden tables included, before the policy filter, so
//! memory is O(history after the cursor). Nothing interrupts a running call. Issue #543.

use crate::limits::{AllowAll, PolicyEvaluator, RowRef};
use crate::rowops::MAX_KEY_BYTES;
use crate::store::RowStore;
use lattice_core::changeset::ChangeSet;
use lattice_core::feed::{ChangeEvent, ChangeOp, Cursor, EntityChange, FeedError, events_for};
use lattice_seams::{Clock, Io, Rng};
use std::fmt;

/// Why a feed could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedReadError {
    /// The cursor is at or below the vacuum floor; history it needs may be gone.
    CursorTooOld {
        /// The store's vacuum floor.
        floor: u64,
    },
    /// A retained change set has no commit time. The store's own metadata disagrees with its rows.
    MissingCommitTime {
        /// The commit LSN.
        lsn: u64,
    },
    /// The change set cannot be expressed as events (a value over the feed's size bound).
    Feed(FeedError),
    /// The first event this read would deliver carries a key over the feed's size bound. Only a log
    /// written before the store refused such keys can hold one. It blocks only readers allowed to
    /// read that row, and only once the events before it were delivered by an earlier read. Reading
    /// again from `cursor` skips that one event.
    KeyTooLarge {
        /// The cursor of the event that cannot be carried.
        cursor: Cursor,
        /// The length of its key in bytes.
        len: usize,
    },
}

impl fmt::Display for FeedReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CursorTooOld { floor } => {
                write!(f, "the cursor is at or below the vacuum floor {floor}")
            }
            Self::MissingCommitTime { lsn } => write!(f, "no commit time for lsn {lsn}"),
            Self::Feed(e) => write!(f, "{e}"),
            Self::KeyTooLarge { cursor, len } => write!(
                f,
                "the event at lsn {} index {} has a {len} byte key over the {} byte feed limit; \
                 resume from that cursor to skip it",
                cursor.lsn(),
                cursor.index(),
                MAX_KEY_BYTES
            ),
        }
    }
}

impl std::error::Error for FeedReadError {}

impl<I: Io, C: Clock, R: Rng> RowStore<I, C, R> {
    /// The events strictly after `cursor`, in commit order, for a reader with no access restrictions.
    pub fn events_since(
        &self,
        cursor: Cursor,
    ) -> Result<std::vec::IntoIter<ChangeEvent>, FeedReadError> {
        self.events_since_as(cursor, &AllowAll)
    }

    /// The events strictly after `cursor`, in commit order, limited to what `policy` may read.
    pub fn events_since_as<P: PolicyEvaluator + ?Sized>(
        &self,
        cursor: Cursor,
        policy: &P,
    ) -> Result<std::vec::IntoIter<ChangeEvent>, FeedReadError> {
        let floor = self.oldest_snapshot().as_of;
        if floor > 0 && cursor.lsn() <= floor {
            return Err(FeedReadError::CursorTooOld { floor });
        }
        let history = self.history_from(cursor.lsn().max(1));
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(first) = history.get(at) {
            let lsn = first.lsn;
            let len = history
                .get(at..)
                .unwrap_or_default()
                .iter()
                .take_while(|e| e.lsn == lsn)
                .count();
            let group = history.get(at..at + len).unwrap_or_default();
            at += len;
            let mut cs = ChangeSet::new(first.cs, Vec::new());
            cs.hlc = self
                .commit_time_at(lsn)
                .ok_or(FeedReadError::MissingCommitTime { lsn })?;
            let changes = group
                .iter()
                .map(|e| EntityChange {
                    kind: e.key.0,
                    // An oversize key (only in a log from before the store bounded keys) is
                    // replaced here so the rest of the set still becomes events; its own event is
                    // never emitted, see below.
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
                    // Everything before the blocked event is delivered first, so skipping it
                    // loses nothing. The read that starts at the prefix's last cursor then
                    // reports the block as its error.
                    if out.is_empty() {
                        return Err(FeedReadError::KeyTooLarge {
                            cursor: event.cursor,
                            len: entry.key.1.len(),
                        });
                    }
                    return Ok(out.into_iter());
                }
                out.push(event);
            }
        }
        Ok(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
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
    use std::collections::BTreeMap;
    use std::rc::Rc;

    type Store = RowStore<SharedIo, SharedClock, SeededRng>;
    type Key = (u32, Vec<u8>);
    /// One acknowledged commit in the model: its lsn and its writes in key order.
    type Commit = (u64, Vec<(Key, Option<Vec<u8>>)>);
    /// (cursor, table, key, value, set_len)
    type Flat = Vec<(Cursor, u32, Vec<u8>, Option<Vec<u8>>, u32)>;

    fn open(disk: &SharedIo, clock: &Rc<SimClock>) -> Store {
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

    /// A small deterministic generator, so the model never shares code with the store.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    fn run(seed: u64, commits: usize, store: &mut Store, clock: &Rc<SimClock>) -> Vec<Commit> {
        let mut rng = Lcg(seed);
        let mut history = Vec::new();
        for _ in 0..commits {
            clock.advance(1);
            let mut tx = store.begin();
            let mut batch: BTreeMap<Key, Option<Vec<u8>>> = BTreeMap::new();
            for _ in 0..=rng.next(3) {
                let key = (rng.next(3) as u32, vec![b'a' + rng.next(4) as u8]);
                if rng.next(4) == 0 {
                    tx.delete(key.clone());
                    batch.insert(key, None);
                } else {
                    let v = vec![rng.next(6) as u8, rng.next(250) as u8];
                    tx.put(key.clone(), v.clone());
                    batch.insert(key, Some(v));
                }
            }
            let lsn = store.commit(tx).unwrap();
            history.push((lsn, batch.into_iter().collect()));
        }
        history
    }

    /// The independent expectation: every write of every commit after `cursor` whose new value (or,
    /// for a delete, removed value) satisfies `readable`.
    fn expected(history: &[Commit], cursor: Cursor, readable: &dyn Fn(u32, &[u8]) -> bool) -> Flat {
        let mut state: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        let mut out = Vec::new();
        for (lsn, writes) in history {
            let set_len = writes.len() as u32;
            for (i, (key, value)) in writes.iter().enumerate() {
                let seen = value.clone().or_else(|| state.get(key).cloned());
                let c = Cursor::new(*lsn, i as u32).unwrap();
                if c > cursor && seen.as_deref().is_some_and(|v| readable(key.0, v)) {
                    out.push((c, key.0, key.1.clone(), value.clone(), set_len));
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
    }

    fn flatten(events: impl Iterator<Item = ChangeEvent>) -> Flat {
        events
            .map(|e| {
                let v = match e.change.op {
                    ChangeOp::Delete => None,
                    ChangeOp::Upsert(v) => Some(v),
                };
                (e.cursor, e.change.kind, e.change.key, v, e.set_len)
            })
            .collect()
    }

    /// The simulator-style oracle: the feed must equal the model exactly and be strictly ordered.
    fn judge(feed: &Flat, model: &Flat) -> Result<(), String> {
        if feed.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err("cursors are not strictly increasing".into());
        }
        if feed != model {
            return Err(format!(
                "feed has {} events, model {}",
                feed.len(),
                model.len()
            ));
        }
        Ok(())
    }

    fn fixture(seed: u64, commits: usize) -> (SharedIo, Rc<SimClock>, Store, Vec<Commit>) {
        let disk = SharedIo::new(SimIo::new(seed, FaultProfile::clean()));
        let clock = Rc::new(SimClock::new(1_000));
        let mut store = open(&disk, &clock);
        let history = run(seed, commits, &mut store, &clock);
        (disk, clock, store, history)
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

    #[test]
    fn feed_from_any_cursor_equals_the_commit_history() {
        for seed in 0..12 {
            let (_d, _c, store, history) = fixture(seed, 14);
            for cursor in every_cursor(&history) {
                let feed = flatten(store.events_since(cursor).unwrap());
                judge(&feed, &expected(&history, cursor, &|_, _| true)).unwrap();
            }
        }
    }

    #[test]
    fn events_carry_commit_provenance_and_encode() {
        let (_d, _c, store, history) = fixture(3, 6);
        let events: Vec<_> = store.events_since(Cursor::ORIGIN).unwrap().collect();
        let total: usize = history.iter().map(|(_, w)| w.len()).sum();
        assert_eq!(events.len(), total);
        assert!(total > 0);
        for e in &events {
            assert_eq!(e.hlc, store.commit_time_at(e.cursor.lsn()).unwrap());
            assert_eq!(e.change_set, store.changeset_id_at(e.cursor.lsn()).unwrap());
            assert_eq!(ChangeEvent::decode(&e.encode().unwrap()).unwrap(), *e);
        }
    }

    #[test]
    fn kill_and_reopen_re_emits_no_gap_and_no_duplicate() {
        for seed in 0..8 {
            let (disk, clock, store, mut history) = fixture(seed, 10);
            let before = flatten(store.events_since(Cursor::ORIGIN).unwrap());
            // The consumer processed up to a cursor in the middle, then the process died.
            let cursors = every_cursor(&history);
            let mid = cursors[cursors.len() / 2];
            drop(store);
            disk.crash();
            disk.restart_process();
            let mut store = open(&disk, &clock);
            let after = flatten(store.events_since(Cursor::ORIGIN).unwrap());
            assert_eq!(after, before, "same cursors and events after reopen");
            let resumed = flatten(store.events_since(mid).unwrap());
            judge(&resumed, &expected(&history, mid, &|_, _| true)).unwrap();
            // New commits after the reopen continue the feed without reusing a cursor.
            history.extend(run(seed + 100, 4, &mut store, &clock));
            let tail = flatten(store.events_since(mid).unwrap());
            judge(&tail, &expected(&history, mid, &|_, _| true)).unwrap();
        }
    }

    struct EvenFirstByte(u32);
    impl PolicyEvaluator for EvenFirstByte {
        fn allows_read(&self, row: RowRef<'_>) -> bool {
            row.table != self.0 && row.value.first().is_some_and(|b| b % 2 == 0)
        }
    }

    struct Nothing;
    impl PolicyEvaluator for Nothing {}

    #[test]
    fn rows_the_actor_cannot_read_produce_no_events() {
        let policy = EvenFirstByte(1);
        let readable = |t: u32, v: &[u8]| t != 1 && v.first().is_some_and(|b| b % 2 == 0);
        let mut filtered_something = false;
        for seed in 0..12 {
            let (_d, _c, store, history) = fixture(seed, 14);
            for cursor in every_cursor(&history) {
                let feed = flatten(store.events_since_as(cursor, &policy).unwrap());
                let model = expected(&history, cursor, &readable);
                judge(&feed, &model).unwrap();
                assert!(feed.iter().all(|e| e.1 != 1));
                filtered_something |= feed.len() < expected(&history, cursor, &|_, _| true).len();
            }
        }
        assert!(filtered_something);
        let (_d, _c, store, _h) = fixture(1, 10);
        assert_eq!(
            store
                .events_since_as(Cursor::ORIGIN, &Nothing)
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_cursor_at_or_below_the_vacuum_floor_is_refused() {
        let (_d, _c, mut store, history) = fixture(2, 12);
        let applied = store.applied_lsn();
        let floor = store.vacuum(applied - 3).horizon;
        assert!(floor > 0);
        for cursor in [Cursor::ORIGIN, Cursor::new(floor, 0).unwrap()] {
            assert_eq!(
                store.events_since(cursor).err(),
                Some(FeedReadError::CursorTooOld { floor })
            );
        }
        let after = Cursor::new(floor + 1, 0).unwrap();
        let feed = flatten(store.events_since(after).unwrap());
        judge(&feed, &expected(&history, after, &|_, _| true)).unwrap();
    }

    // The planted bugs. They corrupt the emitted stream the way a faulty emitter would, and the oracle
    // used above must reject every one wherever the mutation applies.

    fn skip_an_event(feed: &mut Flat) -> bool {
        if feed.is_empty() {
            return false;
        }
        feed.remove(feed.len() / 2);
        true
    }

    fn reorder_entity_events(feed: &mut Flat) -> bool {
        let pair = (0..feed.len()).find_map(|i| {
            ((i + 1)..feed.len())
                .find(|&j| feed[i].1 == feed[j].1 && feed[i].2 == feed[j].2)
                .map(|j| (i, j))
        });
        match pair {
            Some((i, j)) => {
                feed.swap(i, j);
                true
            }
            None => false,
        }
    }

    /// A resume that includes the cursor's own event: a repeat on every resume.
    fn resume_inclusive(store: &Store, cursor: Cursor) -> Option<Flat> {
        let all = flatten(store.events_since(Cursor::ORIGIN).unwrap());
        let at = all.iter().position(|e| e.0 == cursor)?;
        Some(all[at..].to_vec())
    }

    #[test]
    fn planted_feed_bugs_are_caught_by_the_oracle() {
        let (mut skipped, mut reordered, mut inclusive) = (0, 0, 0);
        for seed in 0..12 {
            let (_d, _c, store, history) = fixture(seed, 14);
            for cursor in every_cursor(&history) {
                let model = expected(&history, cursor, &|_, _| true);
                let mut feed = flatten(store.events_since(cursor).unwrap());
                if skip_an_event(&mut feed) {
                    skipped += 1;
                    assert!(judge(&feed, &model).is_err(), "skipped event missed");
                }
                let mut feed = flatten(store.events_since(cursor).unwrap());
                if reorder_entity_events(&mut feed) {
                    reordered += 1;
                    assert!(judge(&feed, &model).is_err(), "reordered entity missed");
                }
                if let Some(feed) = resume_inclusive(&store, cursor) {
                    inclusive += 1;
                    assert!(judge(&feed, &model).is_err(), "inclusive resume missed");
                }
            }
        }
        assert!(skipped > 100 && reordered > 100 && inclusive > 100);
    }
}
