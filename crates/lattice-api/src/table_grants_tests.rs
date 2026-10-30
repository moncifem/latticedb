//! Per-table grants (issue 522) through the real stack: the SQL executor, the binder's catalog and
//! the change feed, for the same accounts and tables. Expectations are written out by hand from the
//! documented semantics, never computed by the code under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use crate::exec_handler::ExecHandler;
use crate::exec_host::{Grant, PolicyFileError, PolicyTable};
use crate::feed_source::read_feed;
use crate::principal::{Principal, QueryContext};
use crate::session::QueryHandler;
use crate::subscribe::SourceError;
use lattice_core::Config;
use lattice_core::bugs::{self, Bug};
use lattice_exec::Database;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use proptest::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

type Db = Database<SharedIo, SharedClock, SeededRng>;
type Handler = ExecHandler<SharedIo, SharedClock, SeededRng>;

const TABLES: [&str; 4] = ["notes", "notes_private", "logs", "secret"];

/// What a statement is expected to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    Ok,
    /// `42P01`: the table does not exist, or the account cannot tell that it does.
    NoTable,
    /// `42501`: the table is visible and the operation is not granted.
    Denied,
}

fn open_db() -> Rc<RefCell<Db>> {
    let db = Database::open(
        SharedIo::new(SimIo::new(1, FaultProfile::clean())),
        SharedClock(Rc::new(SimClock::new(1_000))),
        SeededRng::new(2),
        &Config::default(),
    )
    .unwrap();
    Rc::new(RefCell::new(db))
}

fn table_of(policies: &PolicyTable) -> impl Fn(&str) -> lattice_exec::ActorPolicy + use<> {
    let accounts = policies.clone();
    move |n| accounts.account(n).policy()
}

fn handler(db: &Rc<RefCell<Db>>, policies: &PolicyTable) -> Handler {
    ExecHandler::new(
        db.clone(),
        policies.development().policy(),
        table_of(policies),
    )
}

/// Runs a statement, returning the SQLSTATE and message on error.
fn run(h: &mut Handler, who: &Principal, sql: &str) -> Result<(), (String, String)> {
    h.execute(&QueryContext::new(who), sql, &[])
        .map(|_| ())
        .map_err(|e| (e.code, e.message))
}

fn describe(h: &mut Handler, who: &Principal, sql: &str) -> Result<(), (String, String)> {
    h.describe(&QueryContext::new(who), sql)
        .map(|_| ())
        .map_err(|e| (e.code, e.message))
}

fn outcome(r: &Result<(), (String, String)>) -> Out {
    match r {
        Ok(()) => Out::Ok,
        Err((c, _)) if c == "42P01" => Out::NoTable,
        Err((c, _)) if c == "42501" => Out::Denied,
        Err(e) => panic!("unexpected error {e:?}"),
    }
}

/// A database with the four tables, one row each, written by the development principal.
fn seeded(policies: &PolicyTable) -> Rc<RefCell<Db>> {
    let db = open_db();
    let mut h = handler(&db, policies);
    let dev = Principal::local_development();
    for t in TABLES {
        run(
            &mut h,
            &dev,
            &format!("CREATE TABLE {t} (id INT PRIMARY KEY, v TEXT)"),
        )
        .unwrap();
        run(&mut h, &dev, &format!("INSERT INTO {t} VALUES (1, 'a')")).unwrap();
    }
    db
}

fn file_policies(text: &str) -> PolicyTable {
    PolicyTable::new(Grant::AllowAll).with_accounts(PolicyTable::parse_accounts(text).unwrap())
}

const FILE: &str = "alice\ttables: notes=rw, logs=r\n\
bob\ttables: logs=r\n\
carol\ttables: *=r\n\
dave\ttables: *=rw, secret=-\n\
erin\ttables: notes=w\n\
frank\tdeny-all\n\
legacy\tallow-all\n";

/// (account, table, [select, insert, update, delete]) written out by hand.
/// Update and delete read the rows they change, so they need both bits.
#[rustfmt::skip]
fn expected() -> Vec<(&'static str, &'static str, [Out; 4])> {
    use Out::{Denied as D, NoTable as N, Ok as K};
    vec![
        // alice: notes=rw, logs=r. `notes_private` shares a prefix and must stay invisible.
        ("alice", "notes",         [K, K, K, K]),
        ("alice", "notes_private", [N, N, N, N]),
        ("alice", "logs",          [K, D, D, D]),
        ("alice", "secret",        [N, N, N, N]),
        // bob: logs=r only.
        ("bob",   "notes",         [N, N, N, N]),
        ("bob",   "notes_private", [N, N, N, N]),
        ("bob",   "logs",          [K, D, D, D]),
        ("bob",   "secret",        [N, N, N, N]),
        // carol: *=r. Everything readable, nothing writable.
        ("carol", "notes",         [K, D, D, D]),
        ("carol", "notes_private", [K, D, D, D]),
        ("carol", "logs",          [K, D, D, D]),
        ("carol", "secret",        [K, D, D, D]),
        // dave: *=rw with secret carved out by an explicit empty entry.
        ("dave",  "notes",         [K, K, K, K]),
        ("dave",  "notes_private", [K, K, K, K]),
        ("dave",  "logs",          [K, K, K, K]),
        ("dave",  "secret",        [N, N, N, N]),
        // erin: write without read. Visible; reading (and so update and delete) is refused.
        ("erin",  "notes",         [D, K, D, D]),
        ("erin",  "notes_private", [N, N, N, N]),
        ("erin",  "logs",          [N, N, N, N]),
        ("erin",  "secret",        [N, N, N, N]),
        // frank (deny-all), an account absent from the file, and the old allow-all keyword.
        ("frank", "notes",         [N, N, N, N]),
        ("frank", "secret",        [N, N, N, N]),
        ("zed",   "notes",         [N, N, N, N]),
        ("zed",   "secret",        [N, N, N, N]),
        ("legacy","notes",         [K, K, K, K]),
        ("legacy","secret",        [K, K, K, K]),
    ]
}

/// The whole matrix: every account, table and operation, at run time and at `Describe`.
/// Returns whether every cell matched, so a planted bug can be shown to break it.
fn matrix_holds() -> bool {
    let policies = file_policies(FILE);
    let db = seeded(&policies);
    let mut next_id = 1000;
    for (account, table, outs) in expected() {
        let who = Principal::account(account);
        let mut h = handler(&db, &policies);
        next_id += 1;
        let sqls = [
            format!("SELECT id, v FROM {table}"),
            format!("INSERT INTO {table} VALUES ({next_id}, 'x')"),
            format!("UPDATE {table} SET v = 'y' WHERE id = 1"),
            format!("DELETE FROM {table} WHERE id = 99"),
        ];
        for (sql, want) in sqls.iter().zip(outs) {
            let got = outcome(&run(&mut h, &who, sql));
            if got != want {
                eprintln!("{account} {sql}: wanted {want:?}, got {got:?}");
                return false;
            }
            // Describe sees the same catalog and the same permission, before anything runs.
            let described = outcome(&describe(&mut h, &who, sql));
            if described != want {
                eprintln!("{account} describe {sql}: wanted {want:?}, got {described:?}");
                return false;
            }
        }
    }
    true
}

#[test]
fn table_grants_gate_reads_writes_and_feed_alike() {
    bugs::clear();
    assert!(matrix_holds());
}

#[test]
fn planted_grant_bugs_break_the_sql_matrix() {
    for bug in Bug::TABLE_GRANTS {
        bugs::clear();
        bugs::enable(bug);
        let caught = !matrix_holds();
        bugs::clear();
        assert!(caught, "{bug:?} must fail the SQL grant matrix");
    }
}

/// What the feed does for `account` reading `table`, from the start.
fn feed(
    db: &Rc<RefCell<Db>>,
    policies: &PolicyTable,
    account: &str,
    scope: &str,
    after: &[u8],
) -> Result<usize, SourceError> {
    read_feed(
        &db.borrow(),
        policies,
        &Principal::account(account),
        scope,
        after,
        100,
    )
    .map(|e| e.len())
}

/// Who may read which table's feed: exactly the accounts with the read bit. The one inserted row
/// per table is the single event of a readable feed (the create is a catalog change, not a row).
fn feed_readable(account: &str, table: &str) -> bool {
    expected()
        .iter()
        .find(|(a, t, _)| *a == account && *t == table)
        .is_some_and(|(_, _, outs)| {
            // `select` succeeds exactly when the read bit is held.
            outs[0] == Out::Ok
        })
}

fn feed_matrix_holds() -> bool {
    let policies = file_policies(FILE);
    let db = seeded(&policies);
    for (account, table, _) in expected() {
        let got = feed(&db, &policies, account, &format!("table/{table}"), &[]);
        let want = if feed_readable(account, table) {
            Ok(1)
        } else {
            Err(SourceError::UnknownScope)
        };
        if got != want {
            eprintln!("feed {account} {table}: wanted {want:?}, got {got:?}");
            return false;
        }
    }
    true
}

#[test]
fn feed_serves_exactly_the_readable_tables() {
    bugs::clear();
    assert!(feed_matrix_holds());
}

#[test]
fn planted_grant_bugs_break_the_feed_matrix() {
    // Only the prefix bug reaches the feed (the feed never writes).
    bugs::clear();
    bugs::enable(Bug::GrantTableMatchByPrefix);
    let caught = !feed_matrix_holds();
    bugs::clear();
    assert!(caught);
}

#[test]
fn a_denied_table_is_the_same_as_an_unserved_one_even_for_bad_cursors() {
    let policies = file_policies(FILE);
    let db = seeded(&policies);
    let junk: [&[u8]; 4] = [b"", b"junk", &[0xff; 13], &[0u8; 64]];
    for after in junk {
        for (denied, absent) in [
            ("secret", "no_such_table"),
            ("notes_private", "other_missing"),
        ] {
            let a = feed(&db, &policies, "alice", &format!("table/{denied}"), after);
            let b = feed(&db, &policies, "alice", &format!("table/{absent}"), after);
            assert_eq!(a, b);
            assert_eq!(a, Err(SourceError::UnknownScope));
        }
        // A write-only table is not served either, and not distinguishable.
        assert_eq!(
            feed(&db, &policies, "erin", "table/notes", after),
            Err(SourceError::UnknownScope)
        );
    }
    // The same junk on a readable table is a cursor error, so the answers above are not generic.
    assert_eq!(
        feed(&db, &policies, "alice", "table/notes", &[0xff; 13]),
        Err(SourceError::BadCursor)
    );
    // Not a table scope at all.
    assert_eq!(
        feed(&db, &policies, "alice", "notes", &[]),
        Err(SourceError::UnknownScope)
    );
}

#[test]
fn a_hidden_table_reads_exactly_like_a_missing_one() {
    let policies = file_policies(FILE);
    let db = seeded(&policies);
    let who = Principal::account("bob");
    let norm =
        |r: Result<(), (String, String)>, name: &str| r.map_err(|(c, m)| (c, m.replace(name, "X")));
    for tpl in [
        "SELECT * FROM {T}",
        "SELECT id FROM {T} WHERE id = 1",
        "INSERT INTO {T} VALUES (7, 'z')",
        "UPDATE {T} SET v = 'q'",
        "DELETE FROM {T}",
    ] {
        for hidden in ["secret", "notes", "notes_private"] {
            let mut h = handler(&db, &policies);
            let a = norm(run(&mut h, &who, &tpl.replace("{T}", hidden)), hidden);
            let b = norm(run(&mut h, &who, &tpl.replace("{T}", "absent")), "absent");
            assert_eq!(a, b, "{tpl} on {hidden}");
            assert_eq!(a.as_ref().unwrap_err().0, "42P01");
            let a = norm(describe(&mut h, &who, &tpl.replace("{T}", hidden)), hidden);
            let b = norm(
                describe(&mut h, &who, &tpl.replace("{T}", "absent")),
                "absent",
            );
            assert_eq!(a, b);
        }
    }
}

#[test]
fn ddl_needs_the_ddl_bit_by_name_or_on_the_wildcard() {
    let policies = file_policies(
        "admin\ttables: *=rwd\nmaker\ttables: made=d\nwriter\ttables: *=rw\nnarrow\ttables: *=d, secret=-\n",
    );
    let db = seeded(&policies);
    let try_sql = |account: &str, sql: &str| {
        let mut h = handler(&db, &policies);
        outcome(&run(&mut h, &Principal::account(account), sql))
    };
    // rw is not ddl.
    assert_eq!(
        try_sql("writer", "CREATE TABLE w1 (id INT PRIMARY KEY)"),
        Out::Denied
    );
    assert_eq!(try_sql("writer", "DROP TABLE logs"), Out::Denied);
    // A ddl grant on a name covers that name only.
    assert_eq!(
        try_sql("maker", "CREATE TABLE other (id INT PRIMARY KEY)"),
        Out::Denied
    );
    assert_eq!(
        try_sql("maker", "CREATE TABLE made (id INT PRIMARY KEY)"),
        Out::Ok
    );
    assert_eq!(try_sql("maker", "DROP TABLE logs"), Out::Denied);
    // ddl alone does not read: the table is visible and the read is refused.
    assert_eq!(try_sql("maker", "SELECT * FROM made"), Out::Denied);
    assert_eq!(try_sql("maker", "DROP TABLE made"), Out::Ok);
    // `*=d` with a carve-out.
    assert_eq!(
        try_sql("narrow", "CREATE TABLE anything (id INT PRIMARY KEY)"),
        Out::Ok
    );
    assert_eq!(
        try_sql("narrow", "CREATE TABLE secret (id INT PRIMARY KEY)"),
        Out::Denied
    );
    assert_eq!(try_sql("narrow", "DROP TABLE secret"), Out::Denied);
    assert_eq!(try_sql("narrow", "DROP TABLE IF EXISTS never_was"), Out::Ok);
    // The wildcard administrator, including DROP of a table it never listed by name.
    assert_eq!(try_sql("admin", "DROP TABLE logs"), Out::Ok);
    // Denied DDL answers alike for a table that exists and one that does not.
    assert_eq!(
        try_sql("writer", "DROP TABLE secret"),
        try_sql("writer", "DROP TABLE absent")
    );
}

#[test]
fn unlisted_account_and_unlisted_table_are_denied() {
    let policies = file_policies(FILE);
    let db = seeded(&policies);
    // Absent from the file: deny-all, and the catalog is empty for it.
    let mut h = handler(&db, &policies);
    let who = Principal::account("stranger");
    for t in TABLES {
        assert_eq!(
            outcome(&run(&mut h, &who, &format!("SELECT * FROM {t}"))),
            Out::NoTable
        );
        assert_eq!(
            feed(&db, &policies, "stranger", &format!("table/{t}"), &[]),
            Err(SourceError::UnknownScope)
        );
    }
    // A table the account's list does not name and no `*` covers.
    let mut h = handler(&db, &policies);
    assert_eq!(
        outcome(&run(
            &mut h,
            &Principal::account("alice"),
            "SELECT * FROM secret"
        )),
        Out::NoTable
    );
    // The development principal follows its own configured grant, not the account table.
    let deny_dev = PolicyTable::new(Grant::DenyAll);
    let mut h = handler(&db, &deny_dev);
    assert_eq!(
        outcome(&run(
            &mut h,
            &Principal::local_development(),
            "SELECT * FROM notes"
        )),
        Out::NoTable
    );
}

#[test]
fn a_table_created_after_the_grants_follows_the_wildcard() {
    let policies = file_policies("carol\ttables: *=r\nalice\ttables: notes=rw\n");
    let db = seeded(&policies);
    let mut dev = handler(&db, &policies);
    run(
        &mut dev,
        &Principal::local_development(),
        "CREATE TABLE later (id INT PRIMARY KEY, v TEXT)",
    )
    .unwrap();
    run(
        &mut dev,
        &Principal::local_development(),
        "INSERT INTO later VALUES (1, 'a')",
    )
    .unwrap();
    let mut c = handler(&db, &policies);
    assert_eq!(
        outcome(&run(
            &mut c,
            &Principal::account("carol"),
            "SELECT * FROM later"
        )),
        Out::Ok
    );
    assert_eq!(feed(&db, &policies, "carol", "table/later", &[]), Ok(1));
    let mut a = handler(&db, &policies);
    assert_eq!(
        outcome(&run(
            &mut a,
            &Principal::account("alice"),
            "SELECT * FROM later"
        )),
        Out::NoTable
    );
    assert_eq!(
        feed(&db, &policies, "alice", "table/later", &[]),
        Err(SourceError::UnknownScope)
    );
}

#[test]
fn a_write_in_a_block_is_gated_like_an_autocommit_one() {
    let policies = file_policies(FILE);
    let db = seeded(&policies);
    let who = Principal::account("alice");
    let mut h = handler(&db, &policies);
    run(&mut h, &who, "BEGIN").unwrap();
    assert_eq!(
        outcome(&run(
            &mut h,
            &who,
            "INSERT INTO notes VALUES (500, 'in block')"
        )),
        Out::Ok
    );
    assert_eq!(
        outcome(&run(&mut h, &who, "INSERT INTO logs VALUES (500, 'nope')")),
        Out::Denied
    );
}

#[test]
fn returning_needs_the_read_bit() {
    let policies = file_policies(FILE);
    let db = seeded(&policies);
    let mut h = handler(&db, &policies);
    let erin = Principal::account("erin");
    assert_eq!(
        outcome(&run(&mut h, &erin, "INSERT INTO notes VALUES (600, 'x')")),
        Out::Ok
    );
    let mut h = handler(&db, &policies);
    assert_eq!(
        outcome(&run(
            &mut h,
            &erin,
            "INSERT INTO notes VALUES (601, 'x') RETURNING v"
        )),
        Out::Denied
    );
}

#[test]
fn unique_constraint_tables_follow_their_owner() {
    // A table with a UNIQUE column keeps a derived entry table; a writer with the write bit on the
    // owner must be able to insert, and a reader without it must not.
    let policies = file_policies("w\ttables: u=rw\nr\ttables: u=r\n");
    let db = open_db();
    let mut h = handler(&db, &policies);
    let dev = Principal::local_development();
    run(
        &mut h,
        &dev,
        "CREATE TABLE u (id INT PRIMARY KEY, email TEXT UNIQUE)",
    )
    .unwrap();
    let mut h = handler(&db, &policies);
    assert_eq!(
        outcome(&run(
            &mut h,
            &Principal::account("w"),
            "INSERT INTO u VALUES (1, 'a@x')"
        )),
        Out::Ok
    );
    let mut h = handler(&db, &policies);
    assert_eq!(
        outcome(&run(
            &mut h,
            &Principal::account("r"),
            "INSERT INTO u VALUES (2, 'b@x')"
        )),
        Out::Denied
    );
    let mut h = handler(&db, &policies);
    assert_eq!(
        outcome(&run(&mut h, &Principal::account("r"), "SELECT * FROM u")),
        Out::Ok
    );
}

#[test]
fn grant_file_rejects_unknown_keys_duplicates_and_oversize() {
    for (text, line) in [
        ("a\ttables:\n", 1),
        ("a\ttables: \n", 1),
        ("ok\tdeny-all\na\ttables: t=x\n", 2),
        ("a\ttables: t\n", 1),
        ("a\ttables: t=r,\n", 1),
        ("a\ttables: t=r, t=w\n", 1),
        ("a\ttables: *=r, *=w\n", 1),
        ("a\ttables: =r\n", 1),
        ("a\ttables: t=rr\n", 1),
        ("a\ttables:t=r extra\n", 1),
    ] {
        match PolicyTable::parse_accounts(text) {
            Err(PolicyFileError::BadGrant { line: l, .. }) => assert_eq!(l, line, "{text:?}"),
            other => panic!("{text:?} gave {other:?}"),
        }
    }
    // An unknown policy word is a malformed line, as before.
    for text in [
        "a\ttable: t=r\n",
        "a\tTables: t=r\n",
        "a\tallow-all extra\n",
        "a\tnope\n",
    ] {
        assert_eq!(
            PolicyTable::parse_accounts(text),
            Err(PolicyFileError::Malformed { line: 1 }),
            "{text:?}"
        );
    }
    // Structure errors keep their own variant and line number.
    assert_eq!(
        PolicyTable::parse_accounts("a\tdeny-all\na\ttables: t=r\n"),
        Err(PolicyFileError::Malformed { line: 2 })
    );
    assert_eq!(
        PolicyTable::parse_accounts("no tab here\n"),
        Err(PolicyFileError::Malformed { line: 1 })
    );
    // Size bounds.
    let many: String = (0..300)
        .map(|i| format!("t{i}=r"))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(matches!(
        PolicyTable::parse_accounts(&format!("a\ttables: {many}\n")),
        Err(PolicyFileError::BadGrant { line: 1, .. })
    ));
    let big = format!(
        "a\ttables: {}=r\n",
        "n".repeat(crate::exec_host::MAX_POLICY_BYTES)
    );
    assert_eq!(
        PolicyTable::parse_accounts(&big),
        Err(PolicyFileError::OutOfBounds)
    );
    // The old forms keep working next to the new one.
    let ok = PolicyTable::parse_accounts("a\tallow-all\nb\tdeny-all\nc\ttables: notes=rw, *=r\n")
        .unwrap();
    assert_eq!(ok.len(), 3);
    assert!(matches!(ok.get("c"), Some(Grant::Tables(_))));
    // An error never echoes the offending text.
    let e = format!(
        "{:?}",
        PolicyTable::parse_accounts("a\ttables: supersecretname=zz\n").unwrap_err()
    );
    assert!(!e.contains("supersecret"));
}

// ---- property: the SQL outcome of generated grants against a bit model ----

fn bits() -> impl Strategy<Value = Option<(bool, bool)>> {
    // None: no entry. Some((read, write)): an entry, possibly with no bits at all.
    prop::option::of((any::<bool>(), any::<bool>()))
}

fn bits_text(b: (bool, bool)) -> String {
    match b {
        (false, false) => "-".into(),
        (true, false) => "r".into(),
        (false, true) => "w".into(),
        (true, true) => "rw".into(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn sql_outcomes_follow_a_bit_model(
        named in prop::collection::vec(bits(), 3),
        star in bits(),
    ) {
        let tables = ["notes", "logs", "secret"];
        let mut entries: Vec<String> = Vec::new();
        for (t, b) in tables.iter().zip(&named) {
            if let Some(b) = b {
                entries.push(format!("{t}={}", bits_text(*b)));
            }
        }
        if let Some(b) = star {
            entries.push(format!("*={}", bits_text(b)));
        }
        if entries.is_empty() {
            return Ok(());
        }
        let policies = file_policies(&format!("p\ttables: {}\n", entries.join(", ")));
        let db = open_db();
        let dev = Principal::local_development();
        let mut h = handler(&db, &policies);
        for t in tables {
            run(&mut h, &dev, &format!("CREATE TABLE {t} (id INT PRIMARY KEY, v TEXT)")).unwrap();
            run(&mut h, &dev, &format!("INSERT INTO {t} VALUES (1, 'a')")).unwrap();
        }
        let who = Principal::account("p");
        for (i, t) in tables.iter().enumerate() {
            // Model: own entry, else the star entry, else nothing.
            let (read, write) = named[i].or(star).unwrap_or((false, false));
            let visible = read || write;
            let want = |ok: bool| if !visible { Out::NoTable } else if ok { Out::Ok } else { Out::Denied };
            let mut h = handler(&db, &policies);
            prop_assert_eq!(outcome(&run(&mut h, &who, &format!("SELECT * FROM {t}"))), want(read));
            prop_assert_eq!(outcome(&run(&mut h, &who, &format!("INSERT INTO {t} VALUES ({}, 'n')", 10 + i))), want(write));
            prop_assert_eq!(outcome(&run(&mut h, &who, &format!("UPDATE {t} SET v = 'u' WHERE id = 1"))), want(read && write));
            prop_assert_eq!(outcome(&run(&mut h, &who, &format!("DELETE FROM {t} WHERE id = 77"))), want(read && write));
            let feed_want = if read { Ok(()) } else { Err(SourceError::UnknownScope) };
            let got = feed(&db, &policies, "p", &format!("table/{t}"), &[]).map(|_| ());
            prop_assert_eq!(got, feed_want);
        }
    }
}
