//! Sessions: transactions, snapshot isolation, conflicts, stale plans, row policy, limits, `AS OF`
//! and restart.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::cell::Cell;
use std::rc::Rc;

use common::{Db, err, fixture, ok, rows};
use lattice_core::Config;
use lattice_engines::limits::{DenyAll, PolicyEvaluator, RowRef, RowWriteRef};
use lattice_exec::{
    ActorPolicy, Database, ErrorKind, ExecLimits, ExecSession, TxStatus, decode_params,
    owning_table,
};
use lattice_seams::Clock;
use lattice_sim::SeededRng;
use lattice_sql::binder::bind_statement;
use lattice_sql::parse_statement;
use lattice_testkit::scenario::SharedIo;

fn strs(v: &[&[&str]]) -> Vec<Vec<String>> {
    v.iter()
        .map(|r| r.iter().map(ToString::to_string).collect())
        .collect()
}

fn accounts() -> (common::Fixture, ExecSession) {
    let mut f = fixture();
    let mut s = common::admin();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE acct (id INT PRIMARY KEY, bal INT NOT NULL, tag TEXT UNIQUE)",
    );
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO acct VALUES (1, 100, 'a'), (2, 50, 'b')",
    );
    (f, s)
}

// ----- transaction blocks -----

#[test]
fn begin_commit_rollback_track_the_status_and_the_tags() {
    let (mut f, mut s) = accounts();
    assert_eq!(s.status(), TxStatus::Idle);
    assert_eq!(ok(&mut f.db, &mut s, "BEGIN").tag, "BEGIN");
    assert_eq!(s.status(), TxStatus::InTransaction);
    ok(
        &mut f.db,
        &mut s,
        "UPDATE acct SET bal = bal - 30 WHERE id = 1",
    );
    assert_eq!(ok(&mut f.db, &mut s, "COMMIT").tag, "COMMIT");
    assert_eq!(s.status(), TxStatus::Idle);
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["70"]])
    );
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "DELETE FROM acct");
    assert_eq!(ok(&mut f.db, &mut s, "ROLLBACK").tag, "ROLLBACK");
    assert_eq!(s.status(), TxStatus::Idle);
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM acct"),
        strs(&[&["1"], &["2"]])
    );
}

#[test]
fn a_transaction_sees_its_own_writes_and_nobody_else_does_before_commit() {
    let (mut f, mut a) = accounts();
    let mut b = common::admin();
    ok(&mut f.db, &mut a, "BEGIN");
    ok(&mut f.db, &mut a, "INSERT INTO acct VALUES (3, 1, 'c')");
    ok(&mut f.db, &mut a, "UPDATE acct SET bal = 999 WHERE id = 2");
    ok(&mut f.db, &mut a, "DELETE FROM acct WHERE id = 1");
    // A reads its own changes: the insert, the update and the delete, merged into one view.
    assert_eq!(
        rows(&mut f.db, &mut a, "SELECT id, bal FROM acct"),
        strs(&[&["2", "999"], &["3", "1"]])
    );
    // A statement can update a row the same transaction inserted.
    ok(&mut f.db, &mut a, "UPDATE acct SET bal = 2 WHERE id = 3");
    // B sees none of it.
    assert_eq!(
        rows(&mut f.db, &mut b, "SELECT id, bal FROM acct"),
        strs(&[&["1", "100"], &["2", "50"]])
    );
    ok(&mut f.db, &mut a, "COMMIT");
    assert_eq!(
        rows(&mut f.db, &mut b, "SELECT id, bal FROM acct"),
        strs(&[&["2", "999"], &["3", "2"]])
    );
}

#[test]
fn a_transaction_keeps_its_snapshot_while_others_commit() {
    let (mut f, mut a) = accounts();
    let mut b = common::admin();
    ok(&mut f.db, &mut a, "BEGIN");
    assert_eq!(
        rows(&mut f.db, &mut a, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["100"]])
    );
    ok(&mut f.db, &mut b, "UPDATE acct SET bal = 1 WHERE id = 1");
    ok(&mut f.db, &mut b, "INSERT INTO acct VALUES (9, 9, 'z')");
    // Repeatable read: A still sees the old balance and no new row.
    assert_eq!(
        rows(&mut f.db, &mut a, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["100"]])
    );
    assert_eq!(
        rows(&mut f.db, &mut a, "SELECT id FROM acct"),
        strs(&[&["1"], &["2"]])
    );
    ok(&mut f.db, &mut a, "ROLLBACK");
    assert_eq!(
        rows(&mut f.db, &mut a, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["1"]])
    );
}

#[test]
fn the_first_committer_wins_and_the_loser_gets_40001() {
    let (mut f, mut a) = accounts();
    let mut b = common::admin();
    ok(&mut f.db, &mut a, "BEGIN");
    ok(&mut f.db, &mut b, "BEGIN");
    ok(
        &mut f.db,
        &mut a,
        "UPDATE acct SET bal = bal + 1 WHERE id = 1",
    );
    ok(
        &mut f.db,
        &mut b,
        "UPDATE acct SET bal = bal + 10 WHERE id = 1",
    );
    ok(&mut f.db, &mut a, "COMMIT");
    let e = err(&mut f.db, &mut b, "COMMIT");
    assert_eq!((e.sqlstate(), e.kind()), ("40001", ErrorKind::Conflict));
    assert_eq!(b.status(), TxStatus::Idle, "a failed commit ends the block");
    // Only A's change is there, and B can retry on a fresh snapshot.
    assert_eq!(
        rows(&mut f.db, &mut b, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["101"]])
    );
    ok(
        &mut f.db,
        &mut b,
        "UPDATE acct SET bal = bal + 10 WHERE id = 1",
    );
    assert_eq!(
        rows(&mut f.db, &mut b, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["111"]])
    );
}

#[test]
fn disjoint_writes_both_commit() {
    let (mut f, mut a) = accounts();
    let mut b = common::admin();
    ok(&mut f.db, &mut a, "BEGIN");
    ok(&mut f.db, &mut b, "BEGIN");
    ok(&mut f.db, &mut a, "UPDATE acct SET bal = 0 WHERE id = 1");
    ok(&mut f.db, &mut b, "UPDATE acct SET bal = 0 WHERE id = 2");
    ok(&mut f.db, &mut a, "COMMIT");
    ok(&mut f.db, &mut b, "COMMIT");
    assert_eq!(
        rows(&mut f.db, &mut a, "SELECT bal FROM acct ORDER BY id"),
        strs(&[&["0"], &["0"]])
    );
}

#[test]
fn concurrent_inserts_of_one_key_or_one_unique_value_conflict() {
    let (mut f, mut a) = accounts();
    let mut b = common::admin();
    ok(&mut f.db, &mut a, "BEGIN");
    ok(&mut f.db, &mut b, "BEGIN");
    ok(&mut f.db, &mut a, "INSERT INTO acct VALUES (5, 1, 'x')");
    ok(&mut f.db, &mut b, "INSERT INTO acct VALUES (5, 2, 'y')");
    ok(&mut f.db, &mut a, "COMMIT");
    assert_eq!(err(&mut f.db, &mut b, "COMMIT").sqlstate(), "40001");
    // Different keys but the same UNIQUE value: the unique entry is the row they collide on.
    ok(&mut f.db, &mut a, "BEGIN");
    ok(&mut f.db, &mut b, "BEGIN");
    ok(&mut f.db, &mut a, "INSERT INTO acct VALUES (6, 1, 'same')");
    ok(&mut f.db, &mut b, "INSERT INTO acct VALUES (7, 2, 'same')");
    ok(&mut f.db, &mut a, "COMMIT");
    assert_eq!(err(&mut f.db, &mut b, "COMMIT").sqlstate(), "40001");
    assert_eq!(
        rows(&mut f.db, &mut a, "SELECT id FROM acct WHERE tag = 'same'"),
        strs(&[&["6"]])
    );
}

#[test]
fn an_error_in_a_block_fails_it_until_rollback() {
    let (mut f, mut s) = accounts();
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "UPDATE acct SET bal = 0 WHERE id = 1");
    assert_eq!(
        err(&mut f.db, &mut s, "INSERT INTO acct VALUES (1, 1, 'q')").sqlstate(),
        "23505"
    );
    assert_eq!(s.status(), TxStatus::Failed);
    let e = err(&mut f.db, &mut s, "SELECT 1 FROM acct");
    assert_eq!(
        (e.sqlstate(), e.kind()),
        ("25P02", ErrorKind::TransactionAborted)
    );
    assert_eq!(err(&mut f.db, &mut s, "BEGIN").sqlstate(), "25P02");
    // COMMIT of a failed block rolls it back, like PostgreSQL, and the update is gone.
    assert_eq!(ok(&mut f.db, &mut s, "COMMIT").tag, "ROLLBACK");
    assert_eq!(s.status(), TxStatus::Idle);
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["100"]])
    );
    // A syntax error also fails the block.
    ok(&mut f.db, &mut s, "BEGIN");
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT FROM FROM").sqlstate(),
        "42601"
    );
    assert_eq!(s.status(), TxStatus::Failed);
    ok(&mut f.db, &mut s, "ROLLBACK");
    assert_eq!(s.status(), TxStatus::Idle);
}

#[test]
fn a_block_commits_as_one_change_set() {
    let (mut f, mut s) = accounts();
    let before = f.db.store().applied_lsn();
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "INSERT INTO acct VALUES (3, 1, 'c')");
    ok(&mut f.db, &mut s, "UPDATE acct SET bal = 0");
    ok(&mut f.db, &mut s, "DELETE FROM acct WHERE id = 2");
    assert_eq!(
        f.db.store().applied_lsn(),
        before,
        "nothing is logged before COMMIT"
    );
    ok(&mut f.db, &mut s, "COMMIT");
    assert_eq!(
        f.db.store().applied_lsn(),
        before + 1,
        "three statements, one change set"
    );
    // A read-only block and a statement that matches nothing log nothing.
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "SELECT * FROM acct");
    ok(&mut f.db, &mut s, "COMMIT");
    ok(&mut f.db, &mut s, "UPDATE acct SET bal = 1 WHERE id = 404");
    assert_eq!(f.db.store().applied_lsn(), before + 1);
}

#[test]
fn session_statements() {
    let (mut f, mut s) = accounts();
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"
        )
        .sqlstate(),
        "25P01"
    );
    ok(&mut f.db, &mut s, "BEGIN");
    ok(
        &mut f.db,
        &mut s,
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SHOW transaction_isolation"),
        strs(&[&["repeatable read"]]),
        "SERIALIZABLE is requested but snapshot isolation is what runs"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "CREATE TABLE inner_t (a INT)").sqlstate(),
        "25001"
    );
    ok(&mut f.db, &mut s, "ROLLBACK");
    assert_eq!(
        ok(&mut f.db, &mut s, "SET client_encoding = 'UTF8'").tag,
        "SET"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "SET nonsense = 1").sqlstate(),
        "42704"
    );
    assert_eq!(err(&mut f.db, &mut s, "SHOW nonsense").sqlstate(), "42704");
    ok(&mut f.db, &mut s, "SET statement_timeout = '5s'");
    assert_eq!(s.limits().timeout_ms, Some(5000));
    ok(&mut f.db, &mut s, "SET statement_timeout = 0");
    assert_eq!(s.limits().timeout_ms, None);
}

// ----- stale plans -----

#[test]
fn a_plan_bound_before_a_schema_change_is_refused_with_a_clear_error() {
    let (mut f, mut s) = accounts();
    let stmt = parse_statement("SELECT bal FROM acct WHERE id = 1").unwrap();
    let bound = bind_statement(&stmt, &f.db.catalog_view()).unwrap();
    // Same catalog: the plan runs.
    let r = s.execute_bound(&mut f.db, &bound, &[], &|| false).unwrap();
    assert_eq!(common::text(&r), strs(&[&["100"]]));
    // Data changes do not move the catalog version.
    ok(&mut f.db, &mut s, "UPDATE acct SET bal = 5");
    assert!(s.execute_bound(&mut f.db, &bound, &[], &|| false).is_ok());
    // Any schema change does, even one on another table.
    ok(&mut f.db, &mut s, "CREATE TABLE other (x INT)");
    let e = s
        .execute_bound(&mut f.db, &bound, &[], &|| false)
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::StalePlan);
    assert!(
        e.message().contains("stale plan") && e.message().contains("bind the statement again"),
        "{e}"
    );
    // Re-binding works, and a dropped and re-created table of the same name is stale again.
    let again = bind_statement(&stmt, &f.db.catalog_view()).unwrap();
    assert!(s.execute_bound(&mut f.db, &again, &[], &|| false).is_ok());
    ok(&mut f.db, &mut s, "DROP TABLE acct");
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE acct (id INT PRIMARY KEY, bal INT NOT NULL, tag TEXT UNIQUE)",
    );
    let e = s
        .execute_bound(&mut f.db, &again, &[], &|| false)
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::StalePlan);
    // A stale error inside a block fails the block like any other error.
    ok(&mut f.db, &mut s, "BEGIN");
    assert!(s.execute_bound(&mut f.db, &again, &[], &|| false).is_err());
    assert_eq!(s.status(), TxStatus::Failed);
}

#[test]
fn bound_statements_check_their_parameters() {
    let (mut f, mut s) = accounts();
    let stmt = parse_statement("SELECT id FROM acct WHERE bal > $1").unwrap();
    let bound = bind_statement(&stmt, &f.db.catalog_view()).unwrap();
    let p = decode_params(&bound.param_types, &[Some(b"60".to_vec())]).unwrap();
    let r = s.execute_bound(&mut f.db, &bound, &p, &|| false).unwrap();
    assert_eq!(common::text(&r), strs(&[&["1"]]));
    assert_eq!(
        s.execute_bound(&mut f.db, &bound, &[], &|| false)
            .unwrap_err()
            .sqlstate(),
        "08P01"
    );
    assert_eq!(
        decode_params(&bound.param_types, &[Some(b"x".to_vec())])
            .unwrap_err()
            .sqlstate(),
        "22P02"
    );
    let nulls = decode_params(&bound.param_types, &[None]).unwrap();
    let r = s
        .execute_bound(&mut f.db, &bound, &nulls, &|| false)
        .unwrap();
    assert!(r.rows.is_empty(), "a comparison with NULL is never true");
}

// ----- row policy -----

/// The actor may touch rows whose integer key is even. The key bytes are the order-preserving form
/// of the integer, so the last byte's low bit is the id's parity. Entries of unique constraints
/// (tables with the top bit set) are not rows of the actor's and are left alone.
struct EvenIds;

fn even(table: u32, key: &[u8]) -> bool {
    table & 0x8000_0000 != 0 || key.last().is_some_and(|b| b % 2 == 0)
}

impl PolicyEvaluator for EvenIds {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        even(row.table, row.key)
    }
    fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
        even(row.table, row.key)
    }
}

fn numbers_with_policy(
    policy: Rc<dyn PolicyEvaluator>,
) -> (common::Fixture, ExecSession, ExecSession) {
    let mut f = fixture();
    let mut admin = common::admin();
    ok(
        &mut f.db,
        &mut admin,
        "CREATE TABLE nums (id INT PRIMARY KEY, v INT)",
    );
    ok(
        &mut f.db,
        &mut admin,
        "INSERT INTO nums VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
    );
    (f, admin, ExecSession::new(ActorPolicy::new(policy)))
}

#[test]
fn reads_only_see_rows_the_actor_may_read() {
    let (mut f, mut admin, mut actor) = numbers_with_policy(Rc::new(EvenIds));
    assert_eq!(
        rows(&mut f.db, &mut actor, "SELECT id FROM nums"),
        strs(&[&["2"], &["4"]])
    );
    // Hidden rows are gone before the filter, so they cannot leak through a predicate either.
    assert_eq!(
        rows(&mut f.db, &mut actor, "SELECT id FROM nums WHERE v = 10"),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(rows(&mut f.db, &mut admin, "SELECT id FROM nums").len(), 4);
}

#[test]
fn writes_only_reach_rows_the_actor_may_read_and_write() {
    let (mut f, mut admin, mut actor) = numbers_with_policy(Rc::new(EvenIds));
    assert_eq!(
        ok(&mut f.db, &mut actor, "UPDATE nums SET v = v + 1").tag,
        "UPDATE 2"
    );
    assert_eq!(
        rows(&mut f.db, &mut admin, "SELECT v FROM nums ORDER BY id"),
        strs(&[&["10"], &["21"], &["30"], &["41"]])
    );
    assert_eq!(
        ok(&mut f.db, &mut actor, "DELETE FROM nums").tag,
        "DELETE 2"
    );
    assert_eq!(
        rows(&mut f.db, &mut admin, "SELECT id FROM nums"),
        strs(&[&["1"], &["3"]])
    );
    // Inserting a row the policy forbids is refused at commit and writes nothing.
    let e = err(
        &mut f.db,
        &mut actor,
        "INSERT INTO nums VALUES (5, 50), (6, 60)",
    );
    assert_eq!((e.sqlstate(), e.kind()), ("42501", ErrorKind::Permission));
    assert_eq!(
        rows(&mut f.db, &mut admin, "SELECT id FROM nums"),
        strs(&[&["1"], &["3"]])
    );
    ok(&mut f.db, &mut actor, "INSERT INTO nums VALUES (8, 80)");
    // A key collision with a hidden row is still reported, as PostgreSQL does for row security.
    assert_eq!(
        err(&mut f.db, &mut actor, "INSERT INTO nums VALUES (1, 0)").sqlstate(),
        "23505"
    );
}

#[test]
fn a_transactions_own_writes_obey_the_read_policy_too() {
    let (mut f, _admin, mut actor) = numbers_with_policy(Rc::new(EvenIds));
    ok(&mut f.db, &mut actor, "BEGIN");
    ok(&mut f.db, &mut actor, "INSERT INTO nums VALUES (7, 70)");
    assert_eq!(
        rows(&mut f.db, &mut actor, "SELECT id FROM nums"),
        strs(&[&["2"], &["4"]]),
        "the staged odd row is hidden from its own writer"
    );
    assert_eq!(err(&mut f.db, &mut actor, "COMMIT").sqlstate(), "42501");
}

#[test]
fn deny_all_reads_nothing_and_writes_nothing() {
    let (mut f, mut admin, mut actor) = numbers_with_policy(Rc::new(DenyAll));
    assert!(rows(&mut f.db, &mut actor, "SELECT id FROM nums").is_empty());
    assert_eq!(
        err(&mut f.db, &mut actor, "INSERT INTO nums VALUES (9, 9)").sqlstate(),
        "42501"
    );
    assert_eq!(rows(&mut f.db, &mut admin, "SELECT id FROM nums").len(), 4);
}

// ----- limits -----

/// A clock that moves one millisecond every time it is read, so a deadline passes after a fixed
/// amount of work and the test needs no real time.
#[derive(Clone)]
struct TickClock(Rc<Cell<u64>>);

impl Clock for TickClock {
    fn now_ms(&self) -> u64 {
        self.0.set(self.0.get() + 1);
        self.0.get()
    }
}

fn ticking_db() -> (Database<SharedIo, TickClock, SeededRng>, ExecSession) {
    let disk = SharedIo::new(lattice_sim::SimIo::new(
        3,
        lattice_sim::FaultProfile::clean(),
    ));
    let mut db = Database::open(
        disk,
        TickClock(Rc::new(Cell::new(1_000))),
        SeededRng::new(5),
        &Config {
            dir: "wal".into(),
            segment_limit: 1 << 20,
            node: 1,
        },
    )
    .unwrap();
    let mut s = common::admin();
    s.execute(&mut db, "CREATE TABLE big (id INT PRIMARY KEY, v INT)")
        .unwrap();
    let values: Vec<String> = (0..200).map(|i| format!("({i}, {i})")).collect();
    s.execute(
        &mut db,
        &format!("INSERT INTO big VALUES {}", values.join(", ")),
    )
    .unwrap();
    (db, s)
}

#[test]
fn a_statement_timeout_aborts_the_statement_and_its_writes() {
    let (mut db, mut s) = ticking_db();
    s.execute(&mut db, "SET statement_timeout = 50").unwrap();
    let e = s.execute(&mut db, "UPDATE big SET v = v + 1").unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("57014", ErrorKind::Limit));
    assert!(e.message().contains("timeout"), "{e}");
    s.execute(&mut db, "SET statement_timeout = 0").unwrap();
    let r = s
        .execute(&mut db, "SELECT v FROM big WHERE id = 0")
        .unwrap();
    assert_eq!(
        common::text(&r),
        strs(&[&["0"]]),
        "the aborted update changed nothing"
    );
    // A generous timeout lets the same statement finish.
    s.execute(&mut db, "SET statement_timeout = 100000")
        .unwrap();
    assert_eq!(
        s.execute(&mut db, "UPDATE big SET v = v + 1").unwrap().tag,
        "UPDATE 200"
    );
}

#[test]
fn cancellation_aborts_between_rows() {
    let (mut f, mut s) = accounts();
    let polls = Cell::new(0u32);
    let cancel = || {
        polls.set(polls.get() + 1);
        polls.get() > 1
    };
    let e = s
        .execute_with(&mut f.db, "UPDATE acct SET bal = 0", &[], &cancel)
        .unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("57014", ErrorKind::Limit));
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT bal FROM acct ORDER BY id"),
        strs(&[&["100"], &["50"]])
    );
    // Cancelled before it starts.
    let e = s
        .execute_with(&mut f.db, "SELECT 1 FROM acct", &[], &|| true)
        .unwrap_err();
    assert_eq!(e.sqlstate(), "57014");
    // Inside a block, a cancelled statement fails the block.
    ok(&mut f.db, &mut s, "BEGIN");
    assert!(
        s.execute_with(&mut f.db, "SELECT 1 FROM acct", &[], &|| true)
            .is_err()
    );
    assert_eq!(s.status(), TxStatus::Failed);
}

#[test]
fn the_memory_budget_is_reserved_before_rows_are_kept() {
    let (mut db, mut s) = ticking_db();
    s.set_limits(ExecLimits {
        timeout_ms: None,
        memory_bytes: 8 * 1024,
        transaction_bytes: u64::MAX,
        max_timeout_ms: None,
    });
    let e = s
        .execute(&mut db, "SELECT * FROM big ORDER BY v DESC")
        .unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("53200", ErrorKind::Limit));
    // A query that keeps few rows fits the same budget.
    assert_eq!(
        s.execute(&mut db, "SELECT id FROM big WHERE id = 7")
            .unwrap()
            .rows
            .len(),
        1
    );
    // Writes are bounded too, and a refused write leaves the table alone.
    s.set_limits(ExecLimits {
        timeout_ms: None,
        memory_bytes: 1024,
        transaction_bytes: u64::MAX,
        max_timeout_ms: None,
    });
    assert_eq!(
        s.execute(&mut db, "DELETE FROM big")
            .unwrap_err()
            .sqlstate(),
        "53200"
    );
    s.set_limits(ExecLimits::default());
    assert_eq!(
        common::text(
            &s.execute(&mut db, "SELECT id FROM big WHERE id = 199")
                .unwrap()
        ),
        strs(&[&["199"]])
    );
}

// ----- AS OF and restart -----

#[test]
fn as_of_reads_an_earlier_snapshot_by_time_or_by_changeset() {
    let mut f = fixture();
    let mut s = common::admin();
    f.clock.set(2_000);
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE h (id INT PRIMARY KEY, v TEXT)",
    );
    f.clock.set(5_000);
    ok(&mut f.db, &mut s, "INSERT INTO h VALUES (1, 'first')");
    let first_lsn = f.db.store().applied_lsn();
    f.clock.set(9_000);
    ok(&mut f.db, &mut s, "UPDATE h SET v = 'second'");
    ok(&mut f.db, &mut s, "INSERT INTO h VALUES (2, 'new')");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT v FROM h ORDER BY id"),
        strs(&[&["second"], &["new"]])
    );
    // 7 seconds after the epoch is between the two commits.
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT v FROM h AS OF TIMESTAMP '1970-01-01 00:00:07+00'"
        ),
        strs(&[&["first"]])
    );
    // Before the first row commit the table is empty.
    assert!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT v FROM h AS OF TIMESTAMP '1970-01-01 00:00:03+00'"
        )
        .is_empty()
    );
    let id = f.db.store().changeset_id_at(first_lsn).unwrap();
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            &format!("SELECT v FROM h AS OF CHANGESET '{id}'")
        ),
        strs(&[&["first"]])
    );
    let e = err(
        &mut f.db,
        &mut s,
        "SELECT v FROM h AS OF CHANGESET '00000000000000000000000000'",
    );
    assert_eq!(e.sqlstate(), "72000");
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "SELECT v FROM h AS OF CHANGESET 'not-an-id'"
        )
        .sqlstate(),
        "22P02"
    );
    // A block's own uncommitted writes are not part of a historical snapshot.
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "INSERT INTO h VALUES (3, 'staged')");
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT v FROM h AS OF TIMESTAMP '1970-01-01 00:00:07+00'"
        ),
        strs(&[&["first"]])
    );
    ok(&mut f.db, &mut s, "ROLLBACK");
}

#[test]
fn committed_rows_survive_a_restart_once_the_columns_are_attached_again() {
    let (mut f, mut s) = accounts();
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "UPDATE acct SET bal = 7 WHERE id = 2");
    ok(&mut f.db, &mut s, "ROLLBACK");
    ok(&mut f.db, &mut s, "UPDATE acct SET bal = 8 WHERE id = 2");
    let def = f.db.schema().table("acct").unwrap().clone();
    let version_before = f.db.catalog_version();
    drop(f.db);
    f.disk.restart_process();
    let mut db: Db = Database::open(
        f.disk.clone(),
        lattice_testkit::scenario::SharedClock(f.clock.clone()),
        SeededRng::new(99),
        &common::config(),
    )
    .unwrap();
    let mut s = common::admin();
    // The engine remembers the table's name and id, but not its columns.
    assert_eq!(
        err(&mut db, &mut s, "SELECT * FROM acct").sqlstate(),
        "42P01"
    );
    assert_eq!(
        err(&mut db, &mut s, "CREATE TABLE acct (a INT)").sqlstate(),
        "42P07",
        "the name is taken in the catalog"
    );
    db.attach_table(def.clone()).unwrap();
    assert!(db.catalog_version() > 0 && version_before > 0);
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id, bal, tag FROM acct ORDER BY id"),
        strs(&[&["1", "100", "a"], &["2", "8", "b"]])
    );
    // Unique entries came back with the rows.
    assert_eq!(
        err(&mut db, &mut s, "INSERT INTO acct VALUES (3, 1, 'a')").sqlstate(),
        "23505"
    );
    // Attaching under the wrong id or name is refused.
    let mut wrong = def;
    wrong.id += 50;
    assert_eq!(db.attach_table(wrong).unwrap_err().sqlstate(), "42P01");
}

// ----- interim rules recorded in the review -----

#[test]
fn a_duplicate_key_held_by_a_hidden_row_is_23505_and_reveals_only_the_constraint_name() {
    // Interim rule (P2-1): unique and primary-key violations keep PostgreSQL's row-security
    // semantics, so the existence of the key leaks. Nothing else may: the message is the constraint
    // name and no value, and the error kind is a constraint error.
    let (mut f, _admin, mut actor) = numbers_with_policy(Rc::new(EvenIds));
    assert!(rows(&mut f.db, &mut actor, "SELECT id FROM nums WHERE id = 1").is_empty());
    let e = err(&mut f.db, &mut actor, "INSERT INTO nums VALUES (1, 12345)");
    assert_eq!((e.sqlstate(), e.kind()), ("23505", ErrorKind::Constraint));
    assert_eq!(
        e.message(),
        "duplicate key value violates unique constraint \"nums_pkey\""
    );
    // A key nobody holds is simply refused by the policy, not reported as a duplicate.
    assert_eq!(
        err(&mut f.db, &mut actor, "INSERT INTO nums VALUES (9, 1)").sqlstate(),
        "42501"
    );
}

/// Allows exactly the rows of one table, deciding for a derived `UNIQUE` entry table as it decides for
/// its owner.
struct OneTable {
    table: u32,
    follow_owner: bool,
}

impl OneTable {
    fn ok(&self, physical: u32) -> bool {
        let t = if self.follow_owner {
            owning_table(physical)
        } else {
            physical
        };
        t == self.table
    }
}

impl PolicyEvaluator for OneTable {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        self.ok(row.table)
    }
    fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
        self.ok(row.table)
    }
}

#[test]
fn a_restrictive_policy_must_follow_the_owning_table_of_unique_entries() {
    let mut f = fixture();
    let mut admin = common::admin();
    ok(
        &mut f.db,
        &mut admin,
        "CREATE TABLE u (id INT PRIMARY KEY, tag TEXT UNIQUE)",
    );
    let id = f.db.schema().table("u").unwrap().id;
    // The derived entry tables have bit 31 set and map back to the table.
    assert!(lattice_exec::is_marker_table(
        f.db.schema().table("u").unwrap().marker_table(0)
    ));
    assert_eq!(
        owning_table(f.db.schema().table("u").unwrap().marker_table(0)),
        id
    );
    assert_eq!(owning_table(id), id);
    let mut actor = ExecSession::new(ActorPolicy::new(Rc::new(OneTable {
        table: id,
        follow_owner: true,
    })));
    ok(
        &mut f.db,
        &mut actor,
        "INSERT INTO u VALUES (1, 'a'), (2, 'b')",
    );
    assert_eq!(
        rows(&mut f.db, &mut actor, "SELECT id, tag FROM u"),
        strs(&[&["1", "a"], &["2", "b"]])
    );
    ok(&mut f.db, &mut actor, "UPDATE u SET tag = 'c' WHERE id = 1");
    ok(&mut f.db, &mut actor, "DELETE FROM u WHERE id = 2");
    // A policy that compares the raw id refuses the entry table, and so every write to the table.
    let mut naive = ExecSession::new(ActorPolicy::new(Rc::new(OneTable {
        table: id,
        follow_owner: false,
    })));
    assert_eq!(
        err(&mut f.db, &mut naive, "INSERT INTO u VALUES (3, 'z')").sqlstate(),
        "42501"
    );
}

#[test]
fn re_attaching_a_table_with_another_definition_in_the_same_process_is_refused() {
    let (mut f, _s) = accounts();
    let def = f.db.schema().table("acct").unwrap().clone();
    assert!(f.db.detach_table("acct"));
    assert_eq!(
        common::err(&mut f.db, &mut common::admin(), "SELECT * FROM acct").sqlstate(),
        "42P01"
    );
    // Dropping the UNIQUE constraint would silently stop enforcing it.
    let mut weaker = def.clone();
    weaker.uniques.clear();
    let e = f.db.attach_table(weaker).unwrap_err();
    assert_eq!(e.sqlstate(), "42P17");
    let mut renumbered = def.clone();
    renumbered.columns.truncate(2);
    assert_eq!(
        f.db.attach_table(renumbered).unwrap_err().sqlstate(),
        "42P17"
    );
    // The original definition goes back in and the rows are intact.
    f.db.attach_table(def).unwrap();
    let mut s = common::admin();
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM acct"),
        strs(&[&["1"], &["2"]])
    );
}

#[test]
fn an_expired_snapshot_is_its_own_retryable_kind() {
    let mut f = fixture();
    let mut s = common::admin();
    ok(&mut f.db, &mut s, "CREATE TABLE h (id INT PRIMARY KEY)");
    let e = err(
        &mut f.db,
        &mut s,
        "SELECT id FROM h AS OF CHANGESET '00000000000000000000000000'",
    );
    assert_eq!(
        (e.sqlstate(), e.kind()),
        ("72000", ErrorKind::SnapshotTooOld)
    );
}

// ----- review fixes -----

#[test]
fn ddl_runs_under_the_actors_policy() {
    let mut f = fixture();
    let mut admin = common::admin();
    ok(
        &mut f.db,
        &mut admin,
        "CREATE TABLE keep (id INT PRIMARY KEY)",
    );
    // A deny-all actor may not create or drop tables, and learns nothing about which names exist.
    let mut denied = ExecSession::new(ActorPolicy::new(Rc::new(DenyAll)));
    for sql in [
        "CREATE TABLE fresh (id INT)",
        "CREATE TABLE keep (id INT)",
        "CREATE TABLE IF NOT EXISTS keep (id INT)",
        "DROP TABLE keep",
        "DROP TABLE IF EXISTS keep",
        "DROP TABLE nothing_here",
    ] {
        let e = err(&mut f.db, &mut denied, sql);
        assert_eq!(
            (e.sqlstate(), e.kind()),
            ("42501", ErrorKind::Permission),
            "{sql}"
        );
    }
    assert!(f.db.schema().table("keep").is_some() && f.db.schema().table("fresh").is_none());
    // An allowing policy still creates and drops.
    ok(&mut f.db, &mut admin, "CREATE TABLE fresh (id INT)");
    ok(&mut f.db, &mut admin, "DROP TABLE fresh");
    assert!(f.db.schema().table("fresh").is_none());
}

#[test]
fn a_policy_that_allows_row_access_but_not_ddl_cannot_change_the_catalog() {
    struct RowsOnly;
    impl PolicyEvaluator for RowsOnly {
        fn allows_read(&self, _: RowRef<'_>) -> bool {
            true
        }
        fn allows_write(&self, _: RowWriteRef<'_>) -> bool {
            true
        }
    }
    let mut f = fixture();
    let mut s = ExecSession::new(ActorPolicy::new(Rc::new(RowsOnly)));
    assert_eq!(
        err(&mut f.db, &mut s, "CREATE TABLE t (id INT)").sqlstate(),
        "42501"
    );
}

#[test]
fn the_host_ceiling_on_the_statement_timeout_cannot_be_lifted_by_the_client() {
    let (mut f, mut s) = accounts();
    s.set_limits(ExecLimits {
        timeout_ms: Some(900),
        memory_bytes: 1 << 20,
        transaction_bytes: u64::MAX,
        max_timeout_ms: Some(100),
    });
    assert_eq!(
        s.limits().timeout_ms,
        Some(100),
        "the default is clamped to the ceiling"
    );
    ok(&mut f.db, &mut s, "SET statement_timeout = 0");
    assert_eq!(s.limits().timeout_ms, Some(100));
    ok(&mut f.db, &mut s, "SET statement_timeout = '1min'");
    assert_eq!(s.limits().timeout_ms, Some(100));
    ok(&mut f.db, &mut s, "SET statement_timeout = 40");
    assert_eq!(s.limits().timeout_ms, Some(40), "a client may tighten it");
    ok(&mut f.db, &mut s, "SET statement_timeout = DEFAULT");
    assert_eq!(s.limits().timeout_ms, Some(100));
}

#[test]
fn attaching_a_table_above_the_largest_id_is_refused() {
    let (mut f, _s) = accounts();
    let mut def = f.db.schema().table("acct").unwrap().clone();
    f.db.detach_table("acct");
    def.id = lattice_exec::schema::MAX_TABLE_ID + 1;
    assert_eq!(f.db.attach_table(def).unwrap_err().sqlstate(), "42P01");
}

#[test]
fn a_host_can_fail_the_open_block_so_commit_rolls_back() {
    let (mut f, mut s) = accounts();
    // Outside a block it does nothing.
    s.abort_transaction();
    assert_eq!(s.status(), TxStatus::Idle);
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "INSERT INTO acct VALUES (3, 1, 'c')");
    s.abort_transaction();
    assert_eq!(s.status(), TxStatus::Failed);
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT id FROM acct").sqlstate(),
        "25P02"
    );
    assert_eq!(ok(&mut f.db, &mut s, "COMMIT").tag, "ROLLBACK");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM acct WHERE id = 3"),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn an_actor_that_hides_the_catalog_gets_undefined_table_for_an_existing_table() {
    let (mut f, _admin) = accounts();
    let hidden = ActorPolicy::new(Rc::new(DenyAll)).hiding_catalog();
    let mut s = ExecSession::new(hidden);
    for sql in [
        "SELECT bal FROM acct",
        "SELECT nope FROM acct",
        "INSERT INTO acct VALUES (9, 9, 'z')",
        "DELETE FROM acct",
    ] {
        let existing = err(&mut f.db, &mut s, sql);
        let absent = err(&mut f.db, &mut s, &sql.replace("acct", "absent"));
        assert_eq!(existing.sqlstate(), "42P01", "{sql}");
        assert_eq!(
            existing.message().replace("acct", "X"),
            absent.message().replace("absent", "X"),
            "{sql}"
        );
        // Parse/Describe answers the same way.
        let described = s.describe(&f.db, sql).unwrap_err();
        assert_eq!(described.sqlstate(), "42P01", "{sql}");
    }
    // A plain deny-all policy still sees the catalog: hiding it is an explicit choice.
    let visible = ExecSession::new(ActorPolicy::new(Rc::new(DenyAll)));
    assert!(visible.describe(&f.db, "SELECT bal FROM acct").is_ok());
}

#[test]
fn like_and_concatenation_obey_the_statement_limits() {
    let (mut db, mut s) = ticking_db();
    let text = "a".repeat(30_000);
    let pattern = format!("%{}b", "a".repeat(10_000));
    let sql = format!("SELECT id FROM big WHERE '{text}' LIKE '{pattern}'");
    // The clock moves a millisecond per read and the matcher polls every thousand steps, so a
    // pathological match is cut off by a short timeout instead of running to completion.
    s.execute(&mut db, "SET statement_timeout = 50").unwrap();
    let e = s.execute(&mut db, &sql).unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("57014", ErrorKind::Limit));
    s.execute(&mut db, "SET statement_timeout = 0").unwrap();
    // Cancellation reaches the matcher too.
    let polls = Cell::new(0u32);
    let cancel = || {
        polls.set(polls.get() + 1);
        polls.get() > 20
    };
    let e = s.execute_with(&mut db, &sql, &[], &cancel).unwrap_err();
    assert_eq!(e.sqlstate(), "57014");

    // A concatenation tree is refused before the value exists, under the memory budget.
    let mut tree = String::from("s");
    for _ in 0..14 {
        tree = format!("({tree} || {tree})");
    }
    s.execute(&mut db, "CREATE TABLE strs (id INT PRIMARY KEY, s TEXT)")
        .unwrap();
    s.execute(
        &mut db,
        &format!("INSERT INTO strs VALUES (1, '{}')", "x".repeat(1000)),
    )
    .unwrap();
    s.set_limits(ExecLimits {
        timeout_ms: None,
        memory_bytes: 1 << 20,
        max_timeout_ms: None,
    });
    let e = s
        .execute(&mut db, &format!("SELECT {tree} FROM strs"))
        .unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("53200", ErrorKind::Limit));
}

#[test]
fn a_key_the_store_cannot_carry_is_a_statement_error_not_an_internal_one() {
    let (mut f, mut s) = accounts();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE named (k TEXT PRIMARY KEY, u TEXT UNIQUE)",
    );
    // A text key is its bytes plus a two byte terminator, so 4094 characters is the longest.
    let fits = "x".repeat(4094);
    ok(
        &mut f.db,
        &mut s,
        &format!("INSERT INTO named VALUES ('{fits}', 'one')"),
    );
    let long = "y".repeat(4095);
    for sql in [
        format!("INSERT INTO named VALUES ('{long}', 'two')"),
        format!("INSERT INTO named VALUES ('short', '{long}')"),
    ] {
        let e = err(&mut f.db, &mut s, &sql);
        assert_eq!(e.sqlstate(), "54000", "{e}");
        assert_eq!(e.kind(), ErrorKind::Limit);
    }
    // Inside a transaction block the error belongs to the statement, not to COMMIT.
    ok(&mut f.db, &mut s, "BEGIN");
    let e = err(
        &mut f.db,
        &mut s,
        &format!("INSERT INTO named VALUES ('{long}', 'three')"),
    );
    assert_eq!(e.sqlstate(), "54000");
    assert_eq!(s.status(), TxStatus::Failed);
    assert_eq!(ok(&mut f.db, &mut s, "COMMIT").tag, "ROLLBACK");
    // Nothing was written and the session carries on.
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT u FROM named"),
        strs(&[&["one"]])
    );
    assert_eq!(s.status(), TxStatus::Idle);
}

#[test]
fn create_and_drop_can_repeat_far_past_any_retry_bound_and_across_a_restart() {
    let mut f = fixture();
    let mut s = common::admin();
    for i in 0..130 {
        ok(&mut f.db, &mut s, "CREATE TABLE tmp (a INT)");
        ok(&mut f.db, &mut s, &format!("INSERT INTO tmp VALUES ({i})"));
        assert_eq!(
            rows(&mut f.db, &mut s, "SELECT a FROM tmp"),
            strs(&[&[&i.to_string()]])
        );
        ok(&mut f.db, &mut s, "DROP TABLE tmp");
    }
    // Ids are never reused: 130 cycles used ids 1..=130, so the next table is 131 even though no
    // table is live, and the same holds after a restart.
    ok(&mut f.db, &mut s, "CREATE TABLE keep (a INT)");
    assert_eq!(f.db.schema().table("keep").unwrap().id, 131);
    ok(&mut f.db, &mut s, "DROP TABLE keep");
    drop(f.db);
    f.disk.restart_process();
    let mut db: Db = Database::open(
        f.disk.clone(),
        lattice_testkit::scenario::SharedClock(f.clock.clone()),
        SeededRng::new(99),
        &common::config(),
    )
    .unwrap();
    let mut s = common::admin();
    ok(&mut db, &mut s, "CREATE TABLE after (a INT)");
    assert_eq!(db.schema().table("after").unwrap().id, 132);
}

#[test]
fn create_table_after_dropping_the_highest_id_takes_a_fresh_one() {
    let mut f = fixture();
    let mut s = common::admin();
    for name in ["a", "b", "c"] {
        ok(&mut f.db, &mut s, &format!("CREATE TABLE {name} (x INT)"));
    }
    ok(&mut f.db, &mut s, "DROP TABLE c");
    ok(&mut f.db, &mut s, "CREATE TABLE d (x INT)");
    assert_eq!(f.db.schema().table("d").unwrap().id, 4);
}

#[test]
fn create_table_with_every_served_id_used_is_a_clear_limit_error() {
    use lattice_engines::catalog::CatalogOp;
    use lattice_engines::store::RowStore;
    let disk = SharedIo::new(lattice_sim::SimIo::new(
        7,
        lattice_sim::FaultProfile::clean(),
    ));
    let clock = Rc::new(lattice_sim::SimClock::new(1_000));
    {
        let mut store = RowStore::open(
            disk.clone(),
            lattice_testkit::scenario::SharedClock(clock.clone()),
            SeededRng::new(3),
            &common::config(),
        )
        .unwrap();
        let top = lattice_exec::schema::MAX_TABLE_ID;
        store
            .commit_catalog(
                vec![CatalogOp::Create {
                    table: top,
                    name: "last".into(),
                }],
                vec![],
            )
            .unwrap();
        store
            .commit_catalog(vec![CatalogOp::Drop { table: top }], vec![])
            .unwrap();
    }
    clock.advance(1);
    let mut db: Db = Database::open(
        disk,
        lattice_testkit::scenario::SharedClock(clock),
        SeededRng::new(4),
        &common::config(),
    )
    .unwrap();
    let mut s = common::admin();
    let e = err(&mut db, &mut s, "CREATE TABLE t (a INT)");
    assert_eq!(e.sqlstate(), "53400");
    assert_eq!(e.kind(), ErrorKind::Limit);
}

#[test]
fn staged_writes_are_capped_across_the_statements_of_a_block() {
    let (mut f, mut s) = accounts();
    s.set_limits(ExecLimits {
        transaction_bytes: 8 * 1024,
        ..ExecLimits::default()
    });
    ok(&mut f.db, &mut s, "BEGIN");
    // Each statement stages one small row, far below the statement budget; only the block as a
    // whole grows.
    let mut staged = 0;
    let failure = loop {
        let sql = format!(
            "INSERT INTO acct VALUES ({}, 1, 'tag{}')",
            1000 + staged,
            staged
        );
        match s.execute(&mut f.db, &sql) {
            Ok(_) => staged += 1,
            Err(e) => break e,
        }
        assert!(staged < 200, "the block never hit its cap");
    };
    assert_eq!(
        (failure.sqlstate(), failure.kind()),
        ("53200", ErrorKind::Limit)
    );
    assert!(staged >= 3, "a few rows fit before the cap: {staged}");
    assert_eq!(s.status(), TxStatus::Failed, "the block fails");
    ok(&mut f.db, &mut s, "ROLLBACK");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM acct ORDER BY id"),
        strs(&[&["1"], &["2"]]),
        "nothing from the block was committed"
    );
    // The cap is per transaction: a new block starts from zero.
    ok(&mut f.db, &mut s, "BEGIN");
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO acct VALUES (500, 1, 'fresh')",
    );
    ok(&mut f.db, &mut s, "COMMIT");
}

#[test]
fn rewriting_one_row_in_a_block_does_not_grow_the_staged_count() {
    let (mut f, mut s) = accounts();
    s.set_limits(ExecLimits {
        transaction_bytes: 4 * 1024,
        ..ExecLimits::default()
    });
    ok(&mut f.db, &mut s, "BEGIN");
    for i in 0..200 {
        ok(
            &mut f.db,
            &mut s,
            &format!("UPDATE acct SET bal = {i} WHERE id = 1"),
        );
    }
    ok(&mut f.db, &mut s, "COMMIT");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT bal FROM acct WHERE id = 1"),
        strs(&[&["199"]])
    );
}

#[test]
fn one_autocommit_statement_cannot_stage_more_than_the_transaction_cap() {
    let (mut f, mut s) = accounts();
    s.set_limits(ExecLimits {
        transaction_bytes: 2 * 1024,
        ..ExecLimits::default()
    });
    let values: Vec<String> = (10..110).map(|i| format!("({i}, 1, NULL)")).collect();
    let e = err(
        &mut f.db,
        &mut s,
        &format!("INSERT INTO acct VALUES {}", values.join(", ")),
    );
    assert_eq!((e.sqlstate(), e.kind()), ("53200", ErrorKind::Limit));
    assert_eq!(rows(&mut f.db, &mut s, "SELECT id FROM acct").len(), 2);
}

#[test]
fn settings_that_change_behaviour_are_accepted_only_for_the_supported_value() {
    let (mut f, mut s) = accounts();
    for sql in [
        "SET timezone = 'UTC'",
        "SET timezone = 'utc'",
        "SET timezone TO 'Etc/UTC'",
        "SET timezone = '+00'",
        "SET timezone = DEFAULT",
        "SET client_encoding = 'UTF8'",
        "SET client_encoding TO utf8",
        "SET client_encoding = 'UTF-8'",
        "SET datestyle = 'ISO, MDY'",
        "SET datestyle = 'ISO'",
        "SET application_name = 'psql'",
        "SET extra_float_digits = 3",
    ] {
        ok(&mut f.db, &mut s, sql);
    }
    for sql in [
        "SET timezone = 'America/New_York'",
        "SET timezone = '+05:30'",
        "SET timezone = 3",
        "SET client_encoding = 'LATIN1'",
        "SET client_encoding = 'SQL_ASCII'",
        "SET datestyle = 'SQL, DMY'",
        "SET datestyle = 'German'",
    ] {
        let e = err(&mut f.db, &mut s, sql);
        assert_eq!(
            (e.sqlstate(), e.kind()),
            ("22023", ErrorKind::Bind),
            "{sql}"
        );
    }
    assert_eq!(rows(&mut f.db, &mut s, "SHOW timezone"), strs(&[&["UTC"]]));
    assert_eq!(
        rows(&mut f.db, &mut s, "SHOW client_encoding"),
        strs(&[&["UTF8"]])
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SHOW datestyle"),
        strs(&[&["ISO, MDY"]])
    );
    // A refused SET inside a block fails the block, like any error.
    ok(&mut f.db, &mut s, "BEGIN");
    assert!(
        s.execute(&mut f.db, "SET timezone = 'Europe/Paris'")
            .is_err()
    );
    assert_eq!(s.status(), TxStatus::Failed);
}
