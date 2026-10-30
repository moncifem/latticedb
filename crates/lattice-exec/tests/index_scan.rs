//! Index scans: a scan narrowed by the primary key or a `UNIQUE` constraint must return exactly what
//! a full scan returns, rows, order and errors alike, under every snapshot, transaction and policy.
//!
//! The oracle is the same predicate made unusable for narrowing (a column cast the planner cannot
//! prove total), so both queries run the same expression over the same rows and differ only in how
//! many rows they read. The tests also pin which path is chosen, so a test cannot pass by silently
//! falling back to a full scan.
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
use lattice_core::bugs::{self, Bug};
use lattice_engines::limits::{PolicyEvaluator, RowRef, RowWriteRef};
use lattice_exec::{AccessPath, ActorPolicy, ErrorKind, ExecLimits, ExecSession};
use proptest::prelude::*;

type Outcome = Result<Vec<Vec<String>>, String>;

fn run(db: &mut Db, s: &mut ExecSession, sql: &str) -> Outcome {
    s.execute(db, sql)
        .map(|r| common::text(&r))
        .map_err(|e| e.sqlstate().to_string())
}

/// `pred` and a conjunct that is always true or null where `pred` is, but that the planner cannot
/// show to be error free, so the scan reads the whole table. `v` is an `INT` in every test table.
fn forced(pred: &str) -> String {
    format!("({pred}) AND (CAST(v AS BIGINT) = v OR v IS NULL)")
}

/// Runs `select_list FROM table [as_of] WHERE pred` both ways and requires equal outcomes. Returns
/// the access path of the narrowed run.
fn same(db: &mut Db, s: &mut ExecSession, from: &str, pred: &str) -> AccessPath {
    let narrow = run(db, s, &format!("SELECT * FROM {from} WHERE {pred}"));
    let path = db.last_access_paths();
    let full = run(
        db,
        s,
        &format!("SELECT * FROM {from} WHERE {}", forced(pred)),
    );
    assert_eq!(
        db.last_access_paths(),
        vec![AccessPath::FullScan],
        "the oracle must scan the whole table: {pred}"
    );
    assert_eq!(
        narrow, full,
        "index scan differs from full scan: {from} WHERE {pred}"
    );
    path.first().copied().unwrap_or(AccessPath::FullScan)
}

fn setup_t(db: &mut Db, s: &mut ExecSession) {
    ok(
        db,
        s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, u INT UNIQUE, v INT)",
    );
}

fn setup_c(db: &mut Db, s: &mut ExecSession) {
    ok(
        db,
        s,
        "CREATE TABLE c (k TEXT, n INT, v INT, PRIMARY KEY (k, n))",
    );
}

// ----- which path is chosen -----

#[test]
fn rules_choose_the_expected_access_path() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    setup_c(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 200), (3, 30, NULL)",
    );
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO c VALUES ('a', 1, 1), ('a', 2, 2), ('ab', 1, 3), ('b', 1, 4)",
    );
    let cases: &[(&str, &str, AccessPath)] = &[
        ("t", "id = 2", AccessPath::PrimaryKeyPoint),
        ("t", "2 = id", AccessPath::PrimaryKeyPoint),
        ("t", "id = 2 AND v > 0", AccessPath::PrimaryKeyPoint),
        ("t", "id > 1", AccessPath::PrimaryKeyRange),
        ("t", "id <= 2 AND id >= 1", AccessPath::PrimaryKeyRange),
        ("t", "id BETWEEN 1 AND 2", AccessPath::PrimaryKeyRange),
        ("t", "u = 30", AccessPath::UniqueLookup(0)),
        ("t", "u = 30 AND id > 1", AccessPath::UniqueLookup(0)),
        ("t", "id = 3 AND u = 30", AccessPath::PrimaryKeyPoint),
        ("t", "id > 5 AND id < 3", AccessPath::PrimaryKeyRange),
        ("t", "v = 100", AccessPath::FullScan),
        ("t", "id = 1 OR id = 2", AccessPath::FullScan),
        ("t", "NOT (id = 1)", AccessPath::FullScan),
        ("t", "id <> 1", AccessPath::FullScan),
        ("t", "id IN (1, 2)", AccessPath::FullScan),
        ("t", "u > 5", AccessPath::FullScan),
        // The constant is not an exact integer: the comparison is made on numerics.
        ("t", "id = 1.5", AccessPath::FullScan),
        ("t", "id > 9223372036854775807", AccessPath::PrimaryKeyRange),
        ("t", "id > 9223372036854775808", AccessPath::FullScan),
        // NULL never matches; the full scan finds that out.
        ("t", "id = NULL", AccessPath::FullScan),
        // A term that can fail on some row keeps every row in play so its error is still raised.
        ("t", "id = 1 AND v + 1 > 0", AccessPath::FullScan),
        ("c", "k = 'a' AND n = 2", AccessPath::PrimaryKeyPoint),
        ("c", "k = 'a'", AccessPath::PrimaryKeyRange),
        ("c", "k = 'a' AND n > 1", AccessPath::PrimaryKeyRange),
        ("c", "k >= 'a' AND k < 'b'", AccessPath::PrimaryKeyRange),
        ("c", "n = 1", AccessPath::FullScan),
    ];
    for (table, pred, want) in cases {
        let got = same(&mut f.db, &mut s, table, pred);
        assert_eq!(got, *want, "{table} WHERE {pred}");
    }
}

#[test]
fn a_failing_term_still_fails_when_it_would_have_been_skipped() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, 1, 5), (2, 2, 0)",
    );
    // `10 / v` divides by zero on the row with id 2, which `id = 1` alone would never read. The
    // full scan evaluates the terms left to right and raises or not by their order; narrowing must
    // not change that, so a predicate with a term that can fail reads the whole table.
    let e = err(
        &mut f.db,
        &mut s,
        "SELECT id FROM t WHERE 10 / v > 1 AND id = 1",
    );
    assert_eq!(e.sqlstate(), "22012");
    assert_eq!(f.db.last_access_paths(), vec![AccessPath::FullScan]);
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT id FROM t WHERE id = 1 AND 10 / v > 1"
        ),
        vec![vec!["1"]]
    );
    assert_eq!(f.db.last_access_paths(), vec![AccessPath::FullScan]);
}

#[test]
fn parameters_narrow_a_scan_like_literals() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, 10, 1), (2, 20, 2), (3, 30, 3)",
    );
    let r = s
        .execute_with(
            &mut f.db,
            "SELECT id FROM t WHERE id >= $1 AND id < $2",
            &[Some(b"2".to_vec()), Some(b"3".to_vec())],
            &|| false,
        )
        .unwrap();
    assert_eq!(common::text(&r), vec![vec!["2".to_string()]]);
    assert_eq!(f.db.last_access_paths(), vec![AccessPath::PrimaryKeyRange]);
    let r = s
        .execute_with(
            &mut f.db,
            "SELECT id FROM t WHERE u = $1",
            &[Some(b"30".to_vec())],
            &|| false,
        )
        .unwrap();
    assert_eq!(common::text(&r), vec![vec!["3".to_string()]]);
    assert_eq!(f.db.last_access_paths(), vec![AccessPath::UniqueLookup(0)]);
    // A NULL parameter matches nothing.
    let r = s
        .execute_with(
            &mut f.db,
            "SELECT id FROM t WHERE id = $1",
            &[None],
            &|| false,
        )
        .unwrap();
    assert!(r.rows.is_empty());
}

#[test]
fn text_prefixes_do_not_leak_into_the_range() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_c(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO c VALUES ('', 1, 0), ('a', 1, 0), ('a', 2, 0), ('ab', 1, 0), ('abc', 1, 0), ('b', 1, 0)",
    );
    for pred in [
        "k = 'a'",
        "k = 'ab'",
        "k = ''",
        "k > 'a'",
        "k >= 'a' AND k <= 'ab'",
        "k < 'ab'",
        "k = 'a' AND n >= 2",
        "k = 'a' AND n < 2",
        "k BETWEEN 'a' AND 'ab'",
    ] {
        same(&mut f.db, &mut s, "c", pred);
    }
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT k, n FROM c WHERE k = 'a'"),
        vec![vec!["a", "1"], vec!["a", "2"]]
    );
}

#[test]
fn integer_extremes_are_exact() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (-9223372036854775808, 1, 1), (-1, 2, 2), (0, 3, 3), (9223372036854775807, 4, 4)",
    );
    for pred in [
        "id = -9223372036854775808",
        "id = 9223372036854775807",
        "id < -9223372036854775808",
        "id > 9223372036854775807",
        "id >= 9223372036854775807",
        "id <= -9223372036854775808",
        "id > -9223372036854775808 AND id < 9223372036854775807",
        "id BETWEEN -9223372036854775808 AND 9223372036854775807",
        "id >= 0 AND id <= 0",
        "id > -1 AND id < 1",
    ] {
        same(&mut f.db, &mut s, "t", pred);
    }
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT id FROM t WHERE id <= -1 ORDER BY id"
        ),
        vec![vec!["-9223372036854775808"], vec!["-1"]]
    );
}

#[test]
fn the_inclusive_upper_bound_keeps_its_boundary_row() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3)",
    );
    for pred in ["id <= 2", "id BETWEEN 1 AND 2", "3 >= id AND id >= 2"] {
        let got = rows(&mut f.db, &mut s, &format!("SELECT id FROM t WHERE {pred}"));
        assert!(got.iter().any(|r| r[0] == "2"), "{pred}: {got:?}");
        same(&mut f.db, &mut s, "t", pred);
    }
}

// ----- visibility: snapshots, open transactions, row policy -----

#[test]
fn as_of_snapshots_agree_with_a_full_scan() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, 10, 1), (2, 20, 2)",
    );
    let first =
        f.db.store()
            .changeset_id_at(f.db.store().applied_lsn())
            .unwrap();
    ok(&mut f.db, &mut s, "UPDATE t SET u = 11 WHERE id = 1");
    ok(&mut f.db, &mut s, "DELETE FROM t WHERE id = 2");
    ok(&mut f.db, &mut s, "INSERT INTO t VALUES (3, 20, 3)");
    let from = format!("t AS OF CHANGESET '{first}'");
    assert_eq!(
        same(&mut f.db, &mut s, &from, "u = 20"),
        AccessPath::UniqueLookup(0)
    );
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            &format!("SELECT id FROM {from} WHERE u = 20")
        ),
        vec![vec!["2"]]
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM t WHERE u = 20"),
        vec![vec!["3"]]
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM t WHERE u = 10"),
        Vec::<Vec<String>>::new()
    );
    for pred in ["id >= 1", "id = 2", "id < 3", "u = 10", "u = 11"] {
        same(&mut f.db, &mut s, &from, pred);
        same(&mut f.db, &mut s, "t", pred);
    }
    // A snapshot that does not exist fails identically, narrowed or not.
    let bad = "t AS OF CHANGESET '00000000000000000000000000'";
    for pred in ["id = 1", "id > 0", "u = 10", "id > 5 AND id < 3"] {
        let narrow = run(
            &mut f.db,
            &mut s,
            &format!("SELECT * FROM {bad} WHERE {pred}"),
        );
        let full = run(
            &mut f.db,
            &mut s,
            &format!("SELECT * FROM {bad} WHERE {}", forced(pred)),
        );
        assert_eq!(narrow, Err("72000".to_string()), "{pred}");
        assert_eq!(full, narrow, "{pred}");
    }
}

#[test]
fn an_open_transactions_own_writes_are_seen_through_the_index() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, 10, 1), (2, 20, 2), (3, 30, 3)",
    );
    ok(&mut f.db, &mut s, "BEGIN");
    ok(&mut f.db, &mut s, "UPDATE t SET u = 99 WHERE id = 1");
    ok(&mut f.db, &mut s, "DELETE FROM t WHERE id = 2");
    ok(&mut f.db, &mut s, "INSERT INTO t VALUES (4, 40, 4)");
    ok(&mut f.db, &mut s, "UPDATE t SET id = 30 WHERE id = 3");
    for pred in [
        "u = 99",
        "u = 10",
        "u = 20",
        "u = 40",
        "u = 30",
        "id = 1",
        "id = 2",
        "id = 4",
        "id = 3",
        "id = 30",
        "id >= 1",
        "id < 5",
        "id BETWEEN 2 AND 4",
        "id > 3",
    ] {
        same(&mut f.db, &mut s, "t", pred);
    }
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM t WHERE u = 99"),
        vec![vec!["1"]]
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM t WHERE u = 10"),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM t WHERE id = 30"),
        vec![vec!["30"]]
    );
    ok(&mut f.db, &mut s, "ROLLBACK");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM t WHERE u = 10"),
        vec![vec!["1"]]
    );
}

/// Rows whose key ends in an even byte are readable; the entry tables of unique constraints and
/// every table id with the marker bit are readable, as for the real hosts' evaluators.
struct EvenKeys;

impl PolicyEvaluator for EvenKeys {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        row.table & 0x8000_0000 != 0 || row.key.last().is_some_and(|b| b % 2 == 0)
    }
    fn allows_write(&self, _row: RowWriteRef<'_>) -> bool {
        true
    }
}

/// Denies every read of an entry table. A unique lookup must still authorize only the main row.
struct NoMarkers;

impl PolicyEvaluator for NoMarkers {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        row.table & 0x8000_0000 == 0 && row.key.last().is_some_and(|b| b % 2 == 0)
    }
    fn allows_write(&self, _row: RowWriteRef<'_>) -> bool {
        true
    }
}

fn policy_fixture(policy: Rc<dyn PolicyEvaluator>) -> (common::Fixture, ExecSession, ExecSession) {
    let mut f = fixture();
    let mut admin = common::admin();
    setup_t(&mut f.db, &mut admin);
    ok(
        &mut f.db,
        &mut admin,
        "INSERT INTO t VALUES (1, 10, 1), (2, 20, 2), (3, 30, 3), (4, 40, 4)",
    );
    (f, admin, ExecSession::new(ActorPolicy::new(policy)))
}

fn hidden_unique_row_stays_hidden() -> Result<(), String> {
    let (mut f, mut admin, mut actor) = policy_fixture(Rc::new(EvenKeys));
    for (pred, visible) in [
        ("u = 10", 0usize),
        ("u = 20", 1),
        ("u = 30", 0),
        ("u = 40", 1),
    ] {
        let sql = format!("SELECT id FROM t WHERE {pred}");
        let got = run(&mut f.db, &mut actor, &sql).map_err(|e| e.to_string())?;
        if got.len() != visible {
            return Err(format!(
                "{pred}: the actor saw {} rows instead of {visible}",
                got.len()
            ));
        }
        if f.db.last_access_paths() != vec![AccessPath::UniqueLookup(0)] {
            return Err("not an index lookup".into());
        }
        // The administrator sees the row, so the actor's empty answer is the policy, not the data.
        if run(&mut f.db, &mut admin, &sql)
            .map_err(|e| e.to_string())?
            .len()
            != 1
        {
            return Err("the row is missing for the administrator".into());
        }
    }
    Ok(())
}

#[test]
fn a_hidden_row_is_never_returned_by_a_unique_lookup() {
    assert_eq!(hidden_unique_row_stays_hidden(), Ok(()));
}

#[test]
fn skipping_the_policy_in_the_unique_lookup_is_caught() {
    let _g = Planted::on(Bug::ExecIndexPolicySkipped);
    assert!(hidden_unique_row_stays_hidden().is_err());
}

#[test]
fn policy_filtering_is_identical_for_index_and_full_scans() {
    for policy in [
        Rc::new(EvenKeys) as Rc<dyn PolicyEvaluator>,
        Rc::new(NoMarkers),
    ] {
        let (mut f, _admin, mut actor) = policy_fixture(policy);
        for pred in [
            "id = 1",
            "id = 2",
            "id >= 2",
            "id <= 3",
            "id BETWEEN 1 AND 4",
            "u = 10",
            "u = 20",
            "u = 30",
            "u = 40",
            "u = 50",
            "id > 9 AND id < 3",
        ] {
            same(&mut f.db, &mut actor, "t", pred);
        }
        // Writes obey the same visibility: a hidden row is not updated or deleted through the index.
        let n = |tag: &str| tag.to_string();
        let r = ok(&mut f.db, &mut actor, "UPDATE t SET v = 0 WHERE u = 10");
        assert_eq!(r.tag, n("UPDATE 0"));
        let r = ok(&mut f.db, &mut actor, "UPDATE t SET v = 0 WHERE u = 20");
        assert_eq!(r.tag, n("UPDATE 1"));
        let r = ok(
            &mut f.db,
            &mut actor,
            "DELETE FROM t WHERE id >= 1 AND id <= 3",
        );
        assert_eq!(r.tag, n("DELETE 1"));
    }
}

/// Switches a defect off again however the test ends.
struct Planted;

impl Planted {
    fn on(b: Bug) -> Self {
        bugs::enable(b);
        Planted
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        bugs::clear();
    }
}

fn boundary_rows_survive() -> Result<(), String> {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3)",
    );
    for (pred, want) in [
        ("id <= 2", 2usize),
        ("id BETWEEN 2 AND 3", 2),
        ("id >= 1 AND id <= 1", 1),
    ] {
        let got = run(&mut f.db, &mut s, &format!("SELECT id FROM t WHERE {pred}"))
            .map_err(|e| e.to_string())?;
        if got.len() != want {
            return Err(format!("{pred}: {} rows instead of {want}", got.len()));
        }
    }
    Ok(())
}

#[test]
fn an_exclusive_inclusive_bound_is_caught() {
    assert_eq!(boundary_rows_survive(), Ok(()));
    let _g = Planted::on(Bug::ExecIndexUpperBoundOpen);
    assert!(boundary_rows_survive().is_err());
}

// ----- limits and cancellation -----

#[test]
fn the_index_loop_polls_cancellation_and_charges_memory() {
    let mut f = fixture();
    let mut s = common::admin();
    setup_t(&mut f.db, &mut s);
    let values: Vec<String> = (0..300).map(|i| format!("({i}, {i}, {i})")).collect();
    ok(
        &mut f.db,
        &mut s,
        &format!("INSERT INTO t VALUES {}", values.join(", ")),
    );

    // Cancellation is polled for every candidate row of a range.
    let polls = Cell::new(0u32);
    let cancel = || {
        polls.set(polls.get() + 1);
        polls.get() > 5
    };
    let e = s
        .execute_with(
            &mut f.db,
            "SELECT * FROM t WHERE id >= 10 AND id < 200",
            &[],
            &cancel,
        )
        .unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("57014", ErrorKind::Limit));
    assert_eq!(f.db.last_access_paths(), vec![AccessPath::PrimaryKeyRange]);
    // And before a point lookup starts.
    let e = s
        .execute_with(&mut f.db, "SELECT * FROM t WHERE u = 5", &[], &|| true)
        .unwrap_err();
    assert_eq!(e.sqlstate(), "57014");
    let e = s
        .execute_with(&mut f.db, "SELECT * FROM t WHERE id = 5", &[], &|| true)
        .unwrap_err();
    assert_eq!(e.sqlstate(), "57014");

    // Memory is charged for the rows a range keeps, so a wide range is refused and a narrow one fits.
    s.set_limits(ExecLimits {
        timeout_ms: None,
        memory_bytes: 8 * 1024,
        max_timeout_ms: None,
    });
    let e = s
        .execute(&mut f.db, "SELECT * FROM t WHERE id >= 0 AND id < 300")
        .unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("53200", ErrorKind::Limit));
    assert_eq!(
        s.execute(&mut f.db, "SELECT * FROM t WHERE id >= 10 AND id < 20")
            .unwrap()
            .rows
            .len(),
        10
    );
}

// ----- differential property test -----

const BIG: &[&str] = &[
    "-9223372036854775808",
    "-9223372036854775807",
    "-1",
    "0",
    "1",
    "2",
    "3",
    "9223372036854775806",
    "9223372036854775807",
];
const BIG_EXTRA: &[&str] = &["9223372036854775808", "-9223372036854775809", "1.5", "NULL"];
const SMALL: &[&str] = &["-2147483648", "-1", "0", "1", "2", "2147483647"];
const SMALL_EXTRA: &[&str] = &["2147483648", "0.5", "NULL"];
const KEYS: &[&str] = &["", "a", "ab", "abc", "b", "B", "é", "a b"];
const NS: &[&str] = &["-1", "0", "1", "2"];

#[derive(Clone, Copy, Debug)]
enum Col {
    Id,
    U,
    V,
    K,
    N,
}

impl Col {
    fn name(self) -> &'static str {
        match self {
            Col::Id => "id",
            Col::U => "u",
            Col::V => "v",
            Col::K => "k",
            Col::N => "n",
        }
    }
    /// Literals comparable with the column, including ones outside its type or `NULL`.
    fn consts(self) -> Vec<String> {
        let quote = |xs: &[&str]| xs.iter().map(|x| format!("'{x}'")).collect::<Vec<_>>();
        let plain = |xs: &[&str]| xs.iter().map(ToString::to_string).collect::<Vec<_>>();
        match self {
            Col::Id => [BIG, BIG_EXTRA].iter().flat_map(|x| plain(x)).collect(),
            Col::U | Col::V | Col::N => [SMALL, SMALL_EXTRA, NS]
                .iter()
                .flat_map(|x| plain(x))
                .collect(),
            Col::K => {
                let mut v = quote(KEYS);
                v.push("NULL".into());
                v
            }
        }
    }
}

fn term(cols: &'static [Col]) -> impl Strategy<Value = String> {
    (
        0..cols.len(),
        0..7usize,
        any::<prop::sample::Index>(),
        any::<prop::sample::Index>(),
    )
        .prop_map(move |(c, kind, i, j)| {
            let col = cols[c];
            let consts = col.consts();
            let a = &consts[i.index(consts.len())];
            let b = &consts[j.index(consts.len())];
            let n = col.name();
            let op = ["=", "<", "<=", ">", ">=", "<>"][(kind + i.index(3)) % 6];
            match kind {
                0 | 1 => format!("{n} {op} {a}"),
                2 => format!("{a} {op} {n}"),
                3 => format!("{n} BETWEEN {a} AND {b}"),
                4 => format!("{n} IS NULL"),
                5 => format!("{n} IN ({a}, {b})"),
                _ => format!("NOT ({n} {op} {a})"),
            }
        })
}

fn predicate(cols: &'static [Col]) -> impl Strategy<Value = String> {
    (
        prop::collection::vec(term(cols), 1..4),
        0..8usize,
        term(cols),
    )
        .prop_map(|(terms, shape, extra)| match shape {
            0 => format!("({}) OR ({extra})", terms.join(" AND ")),
            _ => terms.join(" AND "),
        })
}

const T_COLS: &[Col] = &[Col::Id, Col::U, Col::V];
const C_COLS: &[Col] = &[Col::K, Col::N, Col::V];

#[derive(Clone, Debug)]
enum Op {
    Insert(usize, Option<i32>, Option<i32>),
    Update(String),
    Delete(String),
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            4 => (0..BIG.len(), prop::option::of(-2..3i32), prop::option::of(-2..3i32))
                .prop_map(|(a, u, v)| Op::Insert(a, u, v)),
            1 => predicate(T_COLS).prop_map(Op::Update),
            1 => predicate(T_COLS).prop_map(Op::Delete),
        ],
        0..14,
    )
}

fn lit(v: Option<i32>) -> String {
    v.map_or("NULL".into(), |x| x.to_string())
}

fn apply(db: &mut Db, s: &mut ExecSession, op: &Op) {
    // Constraint violations are part of the workload; the outcome of a write is not under test here.
    let sql = match op {
        Op::Insert(id, u, v) => {
            format!(
                "INSERT INTO t VALUES ({}, {}, {})",
                BIG[*id],
                lit(*u),
                lit(*v)
            )
        }
        Op::Update(p) => format!("UPDATE t SET v = COALESCE(v, 0) WHERE {p}"),
        Op::Delete(p) => format!("DELETE FROM t WHERE {p}"),
    };
    let _ = run(db, s, &sql);
}

proptest! {

    #[test]
    fn unique_and_primary_key_scans_equal_full_scans(
        workload in ops(),
        preds in prop::collection::vec(predicate(T_COLS), 1..6),
        pending in ops(),
        use_policy in any::<bool>(),
    ) {
        let mut f = fixture();
        let mut admin = common::admin();
        setup_t(&mut f.db, &mut admin);
        let mut ids = Vec::new();
        for op in &workload {
            apply(&mut f.db, &mut admin, op);
            ids.push(f.db.store().changeset_id_at(f.db.store().applied_lsn()));
        }
        let mut actor = if use_policy {
            ExecSession::new(ActorPolicy::new(Rc::new(EvenKeys)))
        } else {
            common::admin()
        };
        let mut sources = vec!["t".to_string()];
        for id in ids.iter().flatten().step_by(3) {
            sources.push(format!("t AS OF CHANGESET '{id}'"));
        }
        for from in &sources {
            for p in &preds {
                same(&mut f.db, &mut actor, from, p);
            }
        }
        // The same reads inside an open transaction that has written.
        ok(&mut f.db, &mut actor, "BEGIN");
        for op in &pending {
            apply(&mut f.db, &mut actor, op);
        }
        if actor.status() == lattice_exec::TxStatus::Failed {
            let _ = actor.execute(&mut f.db, "ROLLBACK");
        } else {
            for p in &preds {
                same(&mut f.db, &mut actor, "t", p);
            }
            let _ = actor.execute(&mut f.db, "ROLLBACK");
        }
    }

    #[test]
    fn composite_key_scans_equal_full_scans(
        rows_in in prop::collection::vec((0..KEYS.len(), 0..NS.len(), prop::option::of(-2..3i32)), 0..14),
        preds in prop::collection::vec(predicate(C_COLS), 1..6),
    ) {
        let mut f = fixture();
        let mut s = common::admin();
        setup_c(&mut f.db, &mut s);
        for (k, n, v) in &rows_in {
            let _ = run(
                &mut f.db,
                &mut s,
                &format!("INSERT INTO c VALUES ('{}', {}, {})", KEYS[*k], NS[*n], lit(*v)),
            );
        }
        for p in &preds {
            same(&mut f.db, &mut s, "c", p);
        }
    }

    #[test]
    fn writes_through_an_index_equal_writes_through_a_full_scan(
        workload in ops(),
        p in predicate(T_COLS),
        delete in any::<bool>(),
    ) {
        let mut a = fixture();
        let mut b = fixture();
        let mut sa = common::admin();
        let mut sb = common::admin();
        setup_t(&mut a.db, &mut sa);
        setup_t(&mut b.db, &mut sb);
        for op in &workload {
            apply(&mut a.db, &mut sa, op);
            apply(&mut b.db, &mut sb, op);
        }
        let (sql_a, sql_b) = if delete {
            (format!("DELETE FROM t WHERE {p} RETURNING *"), format!("DELETE FROM t WHERE {} RETURNING *", forced(&p)))
        } else {
            (
                format!("UPDATE t SET v = COALESCE(v, 0) + 1 WHERE {p} RETURNING *"),
                format!("UPDATE t SET v = COALESCE(v, 0) + 1 WHERE {} RETURNING *", forced(&p)),
            )
        };
        let ra = run(&mut a.db, &mut sa, &sql_a);
        let rb = run(&mut b.db, &mut sb, &sql_b);
        prop_assert_eq!(ra, rb);
        let all = "SELECT * FROM t ORDER BY id";
        prop_assert_eq!(run(&mut a.db, &mut sa, all), run(&mut b.db, &mut sb, all));
    }
}
