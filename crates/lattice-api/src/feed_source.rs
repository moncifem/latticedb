//! A kernel-backed [`EventSource`]: committed row changes of the SQL executor's database, served to
//! a [`SubscriptionHost`](crate::subscribe::SubscriptionHost).
//!
//! [`FeedSource`] asks the executor actor (see [`crate::exec_host`]) for one bounded read at a time,
//! so the single-threaded row store is never shared. The actor calls `read_feed`, which does the
//! work:
//!
//! * **Scope.** `table/<name>` names a table of the engine catalog. Any other scope, a table that
//!   does not exist and every table for a principal whose policy hides the catalog are all
//!   [`SourceError::UnknownScope`], decided before the cursor is looked at, so a denied principal
//!   cannot tell a table it may not read from one that is not served, by existence or by cursor
//!   error.
//! * **Policy.** The principal becomes an [`ActorPolicy`](lattice_exec::ActorPolicy) through
//!   [`PolicyTable::grant_for`], the mapping the SQL handler uses: the development principal gets the
//!   host's development grant, an account its listed grant, an unlisted account deny-all. There is no
//!   allow-all fallback. The policy filters the feed in the engine ([`RowStore::events_since_as`]).
//! * **Cursor.** The wire cursor is the 12 byte feed cursor (`lsn u64 | index u32`, big endian), so
//!   byte order is cursor order. An empty cursor is the origin. A cursor of another length, or one past
//!   the newest applied commit, is [`SourceError::BadCursor`]; one at or below the vacuum floor is
//!   [`SourceError::TooOld`].
//! * **Payload.** `Event::data` is the `change-feed-v1` encoding of the event.
//! * **Oversize keys.** A log from before the store bounded keys may hold a row the feed cannot
//!   carry. The engine returns the events before it, and then fails a read that starts at it. The source
//!   keeps reading from the last cursor it saw, whatever table it belonged to, and skips exactly the
//!   blocked event, so a reader is never wedged and no other event is lost or repeated. Events of
//!   other tables are passed over the same way, so a page is empty only at the end of the feed.
//!
//! # Bounds
//!
//! One read returns at most `min(limit, max_events)` events, and the host asks again from the last
//! cursor it sent. That bounds only what is returned and encoded. The cost of a read is not bounded
//! by it: `history_from` visits every key of every table (O(total rows), even when the reader is
//! caught up) plus the versions after the cursor, and `events_since_as` builds events and clones
//! values for every write after the cursor, hidden tables included, before the policy filter, so
//! memory per read is O(history after the cursor). A running scan is never interrupted: a
//! cancellation or a reader that gave up is seen only before the scan starts. A reader that is not
//! caught up scans twice, because the source reads once more from the last cursor to confirm the end.
//! Only principals with an allow-all grant reach the scan today. The engine-side limit, a
//! per-principal cap and an LSN index are issue #543. A reader that waits longer than the statement
//! ceiling plus slack gets [`SourceError::Unavailable`], and its queued read is skipped if it has not
//! started; a denied request still takes a queue slot.

use crate::exec_host::{FeedHandle, Grant, PolicyTable};
use crate::principal::Principal;
use crate::subscribe::{EventSource, SourceError};
use crate::sync::{Event, MAX_CURSOR_LEN};
use lattice_core::bugs::{self, Bug};
use lattice_core::feed::Cursor;
use lattice_engines::feed_emit::FeedReadError;
use lattice_engines::limits::PolicyEvaluator;
use lattice_engines::store::RowStore;
use lattice_exec::Database;
use lattice_seams::{Clock, Io, Rng};

/// The scope prefix that names a table.
const TABLE_PREFIX: &str = "table/";

/// Most times one read goes back to the engine after a short answer. Each trip passes at least one
/// event or skips one oversize-key event; the bound only matters for a log with very many such
/// events.
const MAX_ROUNDS: usize = 64;

/// A change-feed reader backed by the executor actor. Cheap to clone through
/// [`ExecHost::feed_source`](crate::exec_host::ExecHost::feed_source); a read blocks the calling
/// thread until the actor answers or the host's wait budget is spent.
pub struct FeedSource {
    handle: FeedHandle,
    max_events: usize,
}

impl FeedSource {
    pub(crate) fn new(handle: FeedHandle, max_events: usize) -> Self {
        FeedSource {
            handle,
            max_events: max_events.max(1),
        }
    }
}

impl EventSource for FeedSource {
    fn read_after(
        &self,
        principal: &Principal,
        scope: &str,
        after: &[u8],
        limit: usize,
    ) -> Result<Vec<Event>, SourceError> {
        // A cursor longer than the wire allows is not one this source issued; a denied principal's
        // answer must not depend on it, so the actor checks scope first and this only bounds memory.
        if after.len() > MAX_CURSOR_LEN {
            return self.handle.read(principal, scope, &[0xff; 13], 1);
        }
        self.handle
            .read(principal, scope, after, limit.min(self.max_events))
    }
}

/// One read of `scope` for `principal` strictly after `after`, at most `limit` events. Runs on the
/// actor thread; also callable on a database owned by the caller.
pub(crate) fn read_feed<I: Io, C: Clock + Clone, R: Rng>(
    db: &Database<I, C, R>,
    policies: &PolicyTable,
    principal: &Principal,
    scope: &str,
    after: &[u8],
    limit: usize,
) -> Result<Vec<Event>, SourceError> {
    let grant = if bugs::active(Bug::FeedSourcePolicyDropped) {
        Grant::AllowAll
    } else {
        policies.grant_for(principal)
    };
    let policy = grant.policy();
    // Scope first: denied, unknown and unserved are one answer before anything about the cursor.
    let table = scope
        .strip_prefix(TABLE_PREFIX)
        .and_then(|name| db.visible_table_id(&policy, name))
        .ok_or(SourceError::UnknownScope)?;
    // Grants name tables, rows carry ids: resolve against the catalog the read is served from.
    let policy = policy.bound(&db.store().catalog().current());
    feed_page(db.store(), &policy, table, after, limit)
}

/// One page of table `table` strictly after `after` from `store`, filtered by `policy`.
fn feed_page<I: Io, C: Clock, R: Rng, P: PolicyEvaluator + ?Sized>(
    store: &RowStore<I, C, R>,
    policy: &P,
    table: u32,
    after: &[u8],
    limit: usize,
) -> Result<Vec<Event>, SourceError> {
    let mut pos = if after.is_empty() {
        Cursor::ORIGIN
    } else {
        Cursor::from_bytes(after).map_err(|_| SourceError::BadCursor)?
    };
    if pos.lsn() > store.applied_lsn() {
        return Err(SourceError::BadCursor);
    }
    let mut out = Vec::new();
    for _ in 0..MAX_ROUNDS {
        if out.len() >= limit {
            break;
        }
        match store.events_since_as(pos, policy) {
            Ok(events) => {
                let mut any = false;
                for event in events {
                    any = true;
                    pos = event.cursor;
                    if event.change.kind != table {
                        continue;
                    }
                    let data = event.encode().map_err(|_| SourceError::Unavailable)?;
                    out.push(Event {
                        cursor: event.cursor.to_bytes().to_vec(),
                        data,
                    });
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
                if !any {
                    return Ok(out);
                }
                // The answer may have stopped before an oversize key; ask again from where it ended.
            }
            Err(FeedReadError::KeyTooLarge { cursor, .. }) => pos = cursor,
            Err(e) => return Err(source_error(&e)),
        }
    }
    if out.is_empty() && limit > 0 {
        // Gave up before reaching the end of the feed and found nothing to return: answering with an
        // empty page would say "caught up", which is untrue.
        return Err(SourceError::Unavailable);
    }
    Ok(out)
}

/// How a feed failure other than the skippable oversize key is told to the host.
fn source_error(e: &FeedReadError) -> SourceError {
    match e {
        FeedReadError::CursorTooOld { .. } => SourceError::TooOld,
        // The store's own metadata or a value over the feed's bound: nothing a cursor can fix, and
        // saying more would reveal storage detail.
        FeedReadError::MissingCommitTime { .. }
        | FeedReadError::Feed(_)
        | FeedReadError::KeyTooLarge { .. } => SourceError::Unavailable,
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
    use crate::exec_handler::ExecHandler;
    use crate::exec_host::{ExecHost, HostLimits};
    use crate::principal::QueryContext;
    use crate::session::QueryHandler;
    use crate::subscribe::{HostConfig, Output, SubscriptionHost};
    use crate::sync::Message;
    use lattice_core::feed::{ChangeEvent, ChangeOp};
    use lattice_core::{ChangeSet, Config, Kernel};
    use lattice_engines::limits::{AllowAll, DenyAll, RowRef, RowWriteRef};
    use lattice_engines::rowops::{self, MAX_KEY_BYTES};
    use lattice_exec::ActorPolicy;
    use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
    use lattice_testkit::scenario::{SharedClock, SharedIo};
    use proptest::prelude::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::rc::Rc;

    type Db = Database<SharedIo, SharedClock, SeededRng>;
    type Handler = ExecHandler<SharedIo, SharedClock, SeededRng>;

    fn open_db() -> Result<Db, lattice_exec::ExecError> {
        Database::open(
            SharedIo::new(SimIo::new(1, FaultProfile::clean())),
            SharedClock(Rc::new(SimClock::new(1_000))),
            SeededRng::new(2),
            &Config::default(),
        )
    }

    /// A database and a handler that writes to it as the development principal.
    fn writer() -> (Rc<RefCell<Db>>, Handler) {
        let db = Rc::new(RefCell::new(open_db().unwrap()));
        let handler = ExecHandler::new(db.clone(), ActorPolicy::new(Rc::new(AllowAll)), |_| {
            ActorPolicy::new(Rc::new(DenyAll))
        });
        (db, handler)
    }

    fn exec(h: &mut Handler, sql: &str) {
        let p = Principal::local_development();
        h.execute(&QueryContext::new(&p), sql, &[]).unwrap();
    }

    fn policies() -> PolicyTable {
        let accounts = PolicyTable::parse_accounts("alice\tallow-all\nbob\tdeny-all\n").unwrap();
        PolicyTable::new(Grant::AllowAll).with_accounts(accounts)
    }

    fn start_host() -> ExecHost {
        ExecHost::start(open_db, policies(), HostLimits::default()).unwrap()
    }

    fn cur(lsn: u64, index: u32) -> Vec<u8> {
        Cursor::new(lsn, index).unwrap().to_bytes().to_vec()
    }

    fn read(
        db: &Rc<RefCell<Db>>,
        who: &Principal,
        scope: &str,
        after: &[u8],
        limit: usize,
    ) -> Result<Vec<Event>, SourceError> {
        read_feed(&db.borrow(), &policies(), who, scope, after, limit)
    }

    /// Every page of `limit` events from `after` until an empty page.
    fn drain(
        db: &Rc<RefCell<Db>>,
        who: &Principal,
        scope: &str,
        after: &[u8],
        limit: usize,
    ) -> Vec<Event> {
        let mut all: Vec<Event> = Vec::new();
        let mut at = after.to_vec();
        loop {
            let page = read(db, who, scope, &at, limit).unwrap();
            assert!(page.len() <= limit);
            if page.is_empty() {
                return all;
            }
            at = page.last().unwrap().cursor.clone();
            all.extend(page);
            assert!(all.len() < 10_000, "paging does not end");
        }
    }

    // ---- the model: what the history says, built without the feed ----

    #[derive(Debug, Clone)]
    enum Step {
        Insert(usize, Vec<i64>),
        Update(usize),
        Delete(usize, i64),
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        prop::collection::vec(
            prop_oneof![
                (0usize..2, prop::collection::btree_set(1i64..8, 1..4))
                    .prop_map(|(t, s)| Step::Insert(t, s.into_iter().collect())),
                (0usize..2).prop_map(Step::Update),
                (0usize..2, 1i64..8).prop_map(|(t, i)| Step::Delete(t, i)),
            ],
            1..12,
        )
    }

    /// (table, cursor bytes, is_delete) per event, in feed order, from running `steps`. The cursor
    /// is the commit lsn the database reports after the statement and the rank of the key among the
    /// keys the statement wrote.
    fn run_steps(
        h: &mut Handler,
        db: &Rc<RefCell<Db>>,
        steps: &[Step],
    ) -> Vec<(usize, Vec<u8>, bool)> {
        exec(h, "CREATE TABLE t0 (id BIGINT PRIMARY KEY, v BIGINT)");
        exec(h, "CREATE TABLE t1 (id BIGINT PRIMARY KEY, v BIGINT)");
        let mut rows: [BTreeSet<i64>; 2] = [BTreeSet::new(), BTreeSet::new()];
        let mut model = Vec::new();
        for (n, step) in steps.iter().enumerate() {
            let (table, mut written, delete) = match step {
                Step::Insert(t, ids) => {
                    let fresh: Vec<i64> = ids
                        .iter()
                        .copied()
                        .filter(|i| !rows[*t].contains(i))
                        .collect();
                    if fresh.is_empty() {
                        continue;
                    }
                    let values: Vec<String> = fresh.iter().map(|i| format!("({i}, 0)")).collect();
                    exec(h, &format!("INSERT INTO t{t} VALUES {}", values.join(", ")));
                    rows[*t].extend(fresh.iter().copied());
                    (*t, fresh, false)
                }
                Step::Update(t) => {
                    if rows[*t].is_empty() {
                        continue;
                    }
                    // A value that differs from every earlier one, so each row really changes.
                    exec(h, &format!("UPDATE t{t} SET v = {}", n + 1));
                    (*t, rows[*t].iter().copied().collect(), false)
                }
                Step::Delete(t, id) => {
                    if !rows[*t].remove(id) {
                        continue;
                    }
                    exec(h, &format!("DELETE FROM t{t} WHERE id = {id}"));
                    (*t, vec![*id], true)
                }
            };
            let lsn = db.borrow().store().applied_lsn();
            // Keys are written in key order, which for positive integers is numeric order.
            written.sort_unstable();
            for index in 0..written.len() {
                model.push((table, cur(lsn, u32::try_from(index).unwrap()), delete));
            }
        }
        model
    }

    fn scope(t: usize) -> String {
        format!("table/t{t}")
    }

    proptest! {
        #[test]
        fn feed_source_equals_the_model_for_every_cursor(steps in steps(), limit in 1usize..5) {
            let (db, mut h) = writer();
            let model = run_steps(&mut h, &db, &steps);
            let mut cursors: Vec<Vec<u8>> = vec![Vec::new()];
            cursors.extend(model.iter().map(|(_, c, _)| c.clone()));
            // The development principal and a listed account with an allow-all grant read the same feed.
            for dev in [Principal::local_development(), Principal::account("alice")] {
              for t in 0..2 {
                for from in &cursors {
                    // Byte order is cursor order, the order the client uses.
                    let want: Vec<(&Vec<u8>, bool)> = model
                        .iter()
                        .filter(|(mt, c, _)| *mt == t && c > from)
                        .map(|(_, c, d)| (c, *d))
                        .collect();
                    let got = drain(&db, &dev, &scope(t), from, limit);
                    prop_assert_eq!(got.len(), want.len());
                    for (g, (c, d)) in got.iter().zip(&want) {
                        prop_assert_eq!(&g.cursor, *c);
                        let ev = ChangeEvent::decode(&g.data).unwrap();
                        prop_assert_eq!(ev.cursor.to_bytes().to_vec(), g.cursor.clone());
                        prop_assert_eq!(matches!(ev.change.op, ChangeOp::Delete), *d);
                    }
                }
            }
              }
        }
    }

    #[test]
    fn a_changeset_of_several_rows_is_several_events_indexed_in_key_order() {
        let (db, mut h) = writer();
        exec(&mut h, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)");
        exec(&mut h, "INSERT INTO t VALUES (3, 0), (1, 0), (2, 0)");
        let lsn = db.borrow().store().applied_lsn();
        let dev = Principal::local_development();
        let page = read(&db, &dev, "table/t", &[], 2).unwrap();
        let got: Vec<_> = page.iter().map(|e| e.cursor.clone()).collect();
        assert_eq!(got, vec![cur(lsn, 0), cur(lsn, 1)]);
        let rest = read(&db, &dev, "table/t", &page[1].cursor, 2).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].cursor, cur(lsn, 2));
        let ev = ChangeEvent::decode(&rest[0].data).unwrap();
        assert_eq!(
            (ev.cursor.lsn(), ev.cursor.index(), ev.set_len),
            (lsn, 2, 3)
        );
        // Resuming at the last event of a changeset gives nothing more.
        assert!(
            read(&db, &dev, "table/t", &rest[0].cursor, 2)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_cursor_the_source_never_issued_is_bad_for_a_reader_who_may_see_the_table() {
        let (db, mut h) = writer();
        exec(&mut h, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)");
        exec(&mut h, "INSERT INTO t VALUES (1, 0)");
        let head = db.borrow().store().applied_lsn();
        let dev = Principal::local_development();
        for bad in [
            vec![1u8; 11],
            vec![0u8; 13],
            cur(head + 1, 0),
            // Zero lsn with a non-zero index is not the origin.
            vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        ] {
            assert_eq!(
                read(&db, &dev, "table/t", &bad, 10),
                Err(SourceError::BadCursor)
            );
        }
        // The origin may be written as empty or as twelve zero bytes.
        assert_eq!(read(&db, &dev, "table/t", &[0; 12], 10).unwrap().len(), 1);
    }

    #[test]
    fn denied_and_unknown_tables_are_indistinguishable() {
        let (db, mut h) = writer();
        exec(
            &mut h,
            "CREATE TABLE secret (id BIGINT PRIMARY KEY, v BIGINT)",
        );
        exec(&mut h, "INSERT INTO secret VALUES (1, 0)");
        let head = db.borrow().store().applied_lsn();
        let dev = Principal::local_development();
        let alice = Principal::account("alice");
        let bob = Principal::account("bob");
        let eve = Principal::account("eve"); // not listed: denied
        assert_eq!(read(&db, &dev, "table/secret", &[], 10).unwrap().len(), 1);
        assert_eq!(read(&db, &alice, "table/secret", &[], 10).unwrap().len(), 1);
        for after in [
            Vec::new(),
            cur(1, 0),
            cur(head, 0),
            cur(head + 7, 0),
            vec![9u8; 5],
        ] {
            for denied in [&bob, &eve] {
                for scope in ["table/secret", "table/nothere", "ws/secret", "table/", ""] {
                    assert_eq!(
                        read(&db, denied, scope, &after, 10),
                        Err(SourceError::UnknownScope),
                        "{denied} {scope:?} {after:?}"
                    );
                }
            }
            // The unserved scopes are the same answer for a reader who may read tables.
            for scope in ["table/nothere", "ws/secret", "table/", ""] {
                assert_eq!(
                    read(&db, &alice, scope, &after, 10),
                    Err(SourceError::UnknownScope)
                );
            }
        }
    }

    #[test]
    fn the_denied_answer_is_the_same_bytes_on_the_wire() {
        let host = start_host();
        let dev = Principal::local_development();
        let mut sql = host.handler();
        for q in [
            "CREATE TABLE secret (id BIGINT PRIMARY KEY, v BIGINT)",
            "INSERT INTO secret VALUES (1, 0)",
        ] {
            sql.execute(&QueryContext::new(&dev), q, &[]).unwrap();
        }
        let source = host.feed_source(16);
        let wire = |who: Principal, scope: &str, cursor: Vec<u8>| -> Vec<Output> {
            let mut h = SubscriptionHost::new(HostConfig::new("lattice"), 0, who).unwrap();
            h.handle(
                Message::Hello {
                    min_version: 1,
                    max_version: 1,
                    agent: "t".into(),
                },
                0,
                &source,
            );
            h.handle(
                Message::Subscribe {
                    sub: 1,
                    scope: scope.into(),
                    cursor,
                },
                1,
                &source,
            )
        };
        for cursor in [Vec::new(), cur(1, 0), cur(u64::MAX, 3), vec![7; 3]] {
            let denied = wire(Principal::account("bob"), "table/secret", cursor.clone());
            let unknown = wire(Principal::account("bob"), "table/nothere", cursor.clone());
            let unknown_to_alice =
                wire(Principal::account("alice"), "table/nothere", cursor.clone());
            assert_eq!(denied, unknown);
            assert_eq!(denied, unknown_to_alice);
            assert!(
                matches!(
                    denied.as_slice(),
                    [Output::Send(Message::Error { sub: 1, .. })]
                ),
                "{denied:?}"
            );
        }
        // The same subscription for a reader who may read it gets the row.
        let ok = wire(Principal::account("alice"), "table/secret", Vec::new());
        assert!(
            matches!(ok.as_slice(), [Output::Send(Message::Events { events, .. })] if events.len() == 1),
            "{ok:?}"
        );
        host.shutdown().unwrap();
    }

    #[test]
    fn an_over_length_cursor_is_bad_for_a_reader_who_may_see_the_table_and_unknown_scope_otherwise()
    {
        let host = start_host();
        let dev = Principal::local_development();
        let mut sql = host.handler();
        sql.execute(
            &QueryContext::new(&dev),
            "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)",
            &[],
        )
        .unwrap();
        let source = host.feed_source(16);
        let long = vec![0xff; 13];
        let longer = vec![0u8; 4096];
        for c in [&long, &longer] {
            let alice = Principal::account("alice");
            assert_eq!(
                source.read_after(&alice, "table/t", c, 10),
                Err(SourceError::BadCursor)
            );
            let bob = Principal::account("bob");
            assert_eq!(
                source.read_after(&bob, "table/t", c, 10),
                Err(SourceError::UnknownScope)
            );
            assert_eq!(
                source.read_after(&bob, "table/none", c, 10),
                Err(SourceError::UnknownScope)
            );
        }
        host.shutdown().unwrap();
    }

    #[test]
    fn a_live_tail_sees_a_commit_made_after_the_cursor() {
        let host = start_host();
        let dev = Principal::local_development();
        let mut sql = host.handler();
        let mut run = |q: &str| sql.execute(&QueryContext::new(&dev), q, &[]).unwrap();
        run("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)");
        run("INSERT INTO t VALUES (1, 0)");
        let source = host.feed_source(16);
        let alice = Principal::account("alice");
        let first = source.read_after(&alice, "table/t", &[], 10).unwrap();
        assert_eq!(first.len(), 1);
        let tail = first[0].cursor.clone();
        assert!(
            source
                .read_after(&alice, "table/t", &tail, 10)
                .unwrap()
                .is_empty()
        );
        run("INSERT INTO t VALUES (2, 0)");
        run("DELETE FROM t WHERE id = 1");
        let next = source.read_after(&alice, "table/t", &tail, 10).unwrap();
        assert_eq!(next.len(), 2);
        assert!(next[0].cursor > tail && next[1].cursor > next[0].cursor);
        let deletes: Vec<bool> = next
            .iter()
            .map(|e| {
                matches!(
                    ChangeEvent::decode(&e.data).unwrap().change.op,
                    ChangeOp::Delete
                )
            })
            .collect();
        assert_eq!(deletes, [false, true]);
        host.shutdown().unwrap();
    }

    #[test]
    fn one_read_returns_at_most_max_events_and_a_reader_does_not_starve_commits() {
        let host = start_host();
        let dev = Principal::local_development();
        let mut sql = host.handler();
        sql.execute(
            &QueryContext::new(&dev),
            "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)",
            &[],
        )
        .unwrap();
        let source = host.feed_source(3);
        let seen = std::thread::scope(|s| {
            // A reader pages through the feed while the writer keeps committing on the same actor.
            let reader = s.spawn(|| {
                let mut seen = Vec::new();
                let mut at: Vec<u8> = Vec::new();
                let mut idle = 0;
                while seen.len() < 40 && idle < 50_000 {
                    let page = source.read_after(&dev, "table/t", &at, 1000).unwrap();
                    assert!(page.len() <= 3, "a read returned {} events", page.len());
                    match page.last() {
                        Some(last) => at = last.cursor.clone(),
                        None => idle += 1,
                    }
                    seen.extend(page.into_iter().map(|e| e.cursor));
                }
                seen
            });
            for i in 1..=40 {
                // Each commit is served although a reader keeps asking: reads are queued one at a
                // time behind the writer's requests. This does not bound the cost of one read (#543).
                sql.execute(
                    &QueryContext::new(&dev),
                    &format!("INSERT INTO t VALUES ({i}, 0)"),
                    &[],
                )
                .unwrap();
            }
            reader.join().unwrap()
        });
        assert_eq!(seen.len(), 40);
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        host.shutdown().unwrap();
    }

    // ---- engine edge cases on a bare store ----

    type Store = RowStore<SharedIo, SharedClock, SeededRng>;

    fn store(disk: &SharedIo, clock: &Rc<SimClock>) -> Store {
        clock.advance(1);
        RowStore::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(5),
            &Config::default(),
        )
        .unwrap()
    }

    fn fresh_disk() -> (SharedIo, Rc<SimClock>) {
        (
            SharedIo::new(SimIo::new(17, FaultProfile::clean())),
            Rc::new(SimClock::new(1_000)),
        )
    }

    fn page(
        store: &Store,
        table: u32,
        after: &[u8],
        limit: usize,
    ) -> Result<Vec<Event>, SourceError> {
        feed_page(store, &AllowAll, table, after, limit)
    }

    #[test]
    fn a_cursor_at_or_below_the_vacuum_floor_is_too_old() {
        let (disk, clock) = fresh_disk();
        let mut s = store(&disk, &clock);
        let mut lsns = Vec::new();
        for i in 0..4u8 {
            let mut tx = s.begin();
            tx.put((7, vec![i]), vec![i]);
            lsns.push(s.commit(tx).unwrap());
        }
        assert_eq!(page(&s, 7, &[], 10).unwrap().len(), 4);
        s.vacuum(lsns[1]);
        let floor = s.oldest_snapshot().as_of;
        assert!(floor > 0);
        // The origin, and any cursor at or below the floor, can no longer be resumed.
        assert_eq!(page(&s, 7, &[], 10), Err(SourceError::TooOld));
        assert_eq!(page(&s, 7, &cur(floor, 0), 10), Err(SourceError::TooOld));
        let tail = page(&s, 7, &cur(lsns[2], 0), 10).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].cursor, cur(lsns[3], 0));
    }

    #[test]
    fn feed_errors_other_than_the_skippable_key_map_to_the_defined_source_errors() {
        use lattice_core::feed::FeedError;
        assert_eq!(
            source_error(&FeedReadError::CursorTooOld { floor: 3 }),
            SourceError::TooOld
        );
        assert_eq!(
            source_error(&FeedReadError::MissingCommitTime { lsn: 3 }),
            SourceError::Unavailable
        );
        assert_eq!(
            source_error(&FeedReadError::Feed(FeedError::ZeroLsn)),
            SourceError::Unavailable
        );
    }

    /// What a binary from before the key bound could have written.
    fn legacy_log(disk: &SharedIo, clock: &Rc<SimClock>, commits: Vec<Vec<rowops::RowWrite>>) {
        let (mut kernel, _) = Kernel::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(23),
            &Config::default(),
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
    fn oversize_key_event_is_skipped_by_resuming_at_its_cursor() {
        let (disk, clock) = fresh_disk();
        let big = vec![b'b'; MAX_KEY_BYTES + 1];
        legacy_log(
            &disk,
            &clock,
            vec![
                vec![((7, b"a".to_vec()), Some(b"1".to_vec()))],
                // Table 7 sorts before table 8: the oversize row is index 1 of this set.
                vec![
                    ((7, b"c".to_vec()), Some(b"2".to_vec())),
                    ((8, big.clone()), Some(b"3".to_vec())),
                ],
                vec![((7, b"d".to_vec()), Some(b"4".to_vec()))],
                vec![((9, big), Some(b"5".to_vec()))],
            ],
        );
        let s = store(&disk, &clock);
        let all_7 = vec![cur(1, 0), cur(2, 0), cur(3, 0)];
        for limit in 1..=4 {
            // Neither a short answer before the blocked event nor the block itself wedges the
            // reader, and nothing is lost or repeated at any page size.
            let mut got = Vec::new();
            let mut at: Vec<u8> = Vec::new();
            loop {
                let p = page(&s, 7, &at, limit).unwrap();
                if p.is_empty() {
                    break;
                }
                at = p.last().unwrap().cursor.clone();
                got.extend(p.into_iter().map(|e| e.cursor));
            }
            assert_eq!(got, all_7, "limit {limit}");
        }
        // A blocked row is the only event of its table and is skipped: the table's feed is empty
        // and ends, rather than being reported as unavailable.
        assert!(page(&s, 8, &[], 5).unwrap().is_empty());
        assert!(page(&s, 9, &[], 5).unwrap().is_empty());
        // A reader positioned on the blocked cursor itself resumes after it.
        let after_block = page(&s, 7, &cur(2, 1), 5).unwrap();
        assert_eq!(after_block.len(), 1);
        assert_eq!(after_block[0].cursor, cur(3, 0));
    }

    #[test]
    fn a_policy_that_hides_a_blocked_row_is_not_blocked_by_it() {
        struct Only(u32);
        impl PolicyEvaluator for Only {
            fn allows_read(&self, row: RowRef<'_>) -> bool {
                row.table == self.0
            }
            fn allows_write(&self, _: RowWriteRef<'_>) -> bool {
                false
            }
        }
        let (disk, clock) = fresh_disk();
        let big = vec![b'b'; MAX_KEY_BYTES + 1];
        legacy_log(
            &disk,
            &clock,
            vec![vec![
                ((7, b"c".to_vec()), Some(b"2".to_vec())),
                ((8, big), Some(b"3".to_vec())),
            ]],
        );
        let s = store(&disk, &clock);
        let got = feed_page(&s, &Only(7), 7, &[], 10).unwrap();
        assert_eq!(got.len(), 1);
    }

    // ---- the planted bug ----

    #[test]
    fn a_dropped_policy_is_caught_by_the_witness() {
        use lattice_core::bugs;
        let (db, mut h) = writer();
        exec(&mut h, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)");
        exec(&mut h, "INSERT INTO t VALUES (1, 0)");
        let (alice, bob) = (Principal::account("alice"), Principal::account("bob"));
        // Alice reads the row; bob is told the table is not served.
        let witness = || {
            read(&db, &alice, "table/t", &[], 10).map(|e| e.len()) == Ok(1)
                && read(&db, &bob, "table/t", &[], 10) == Err(SourceError::UnknownScope)
        };
        bugs::clear();
        assert!(witness(), "clean build must satisfy the witness");
        bugs::enable(Bug::FeedSourcePolicyDropped);
        let caught = !witness();
        bugs::clear();
        assert!(
            caught,
            "a source that drops the policy must fail the witness"
        );
    }

    #[test]
    fn grants_follow_the_principal_with_no_allow_all_fallback() {
        let t = PolicyTable::new(Grant::DenyAll)
            .with_accounts(BTreeMap::from([("alice".to_string(), Grant::AllowAll)]));
        assert_eq!(t.grant_for(&Principal::local_development()), Grant::DenyAll);
        assert_eq!(t.grant_for(&Principal::account("alice")), Grant::AllowAll);
        assert_eq!(t.grant_for(&Principal::account("nobody")), Grant::DenyAll);
        // An account that is named like the development principal is still an account.
        assert_eq!(
            t.grant_for(&Principal::account(
                crate::principal::LOCAL_DEVELOPMENT_NAME
            )),
            Grant::DenyAll
        );
    }
}
