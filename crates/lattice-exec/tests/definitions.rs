//! Durable SQL table definitions (RFC 0012, issue 573): `CREATE TABLE` writes the definition in the
//! same change set as the table, and `Database::open` rebuilds the executor's schema from the log.
//! Every test here closes the database and opens it again from the same disk.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::rc::Rc;

use common::{Db, Fixture, config, err, fixture, ok, reopen, rows};
use lattice_core::bugs::{self, Bug};
use lattice_engines::catalog::CatalogOp;
use lattice_engines::sqldef::{Column, DefaultValue, DefineTable, SqlTableDef, SqlType};
use lattice_engines::store::RowStore;
use lattice_exec::Database;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(ToString::to_string).collect())
        .collect()
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

const RICH: &str = "CREATE TABLE rich (
    id BIGINT PRIMARY KEY,
    flag BOOLEAN NOT NULL DEFAULT true,
    small SMALLINT DEFAULT -7,
    n INTEGER,
    d DOUBLE PRECISION DEFAULT 1.5,
    amt NUMERIC DEFAULT 12.50,
    note TEXT DEFAULT 'it''s',
    code VARCHAR(5) UNIQUE,
    blob BYTEA DEFAULT '\\x00ff',
    u UUID,
    born DATE DEFAULT '2020-02-29',
    seen TIMESTAMPTZ DEFAULT '2020-01-01 00:00:00+00',
    doc JSONB DEFAULT '{\"a\": 1}'
)";

const RICH_ROW_SQL: &str = "SELECT id, flag, small, n, d, amt, note, code, blob, u, born, seen, doc \
    FROM rich ORDER BY id";

fn rich_rows() -> Vec<Vec<String>> {
    strs(&[
        &[
            "1",
            "t",
            "-7",
            "NULL",
            "1.5",
            "12.5",
            "it's",
            "NULL",
            "\\x00ff",
            "NULL",
            "2020-02-29",
            "2020-01-01 00:00:00+00",
            "{\"a\": 1}",
        ],
        &[
            "2",
            "f",
            "3",
            "9",
            "2.25",
            "0.10",
            "x",
            "abc",
            "\\x01",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "1999-12-31",
            "2001-02-03 04:05:06+00",
            "[1, 2]",
        ],
    ])
}

fn rich_fixture() -> Fixture {
    let mut f = fixture();
    let mut s = common::admin();
    ok(&mut f.db, &mut s, RICH);
    ok(&mut f.db, &mut s, "INSERT INTO rich (id) VALUES (1)");
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO rich VALUES (2, false, 3, 9, 2.25, 0.10, 'x', 'abc', '\\x01', \
         'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '1999-12-31', '2001-02-03 04:05:06+00', '[1, 2]')",
    );
    f
}

#[test]
fn recovered_table_is_queryable_after_reopen() {
    let mut f = rich_fixture();
    let mut s = common::admin();
    assert_eq!(rows(&mut f.db, &mut s, RICH_ROW_SQL), rich_rows());
    let before = f.db.schema().table("rich").unwrap().clone();
    let mut f = reopen(f);
    let mut s = common::admin();
    // The same table, rebuilt from the record: types, defaults, keys and constraint names.
    assert_eq!(f.db.schema().table("rich"), Some(&before));
    assert_eq!(rows(&mut f.db, &mut s, RICH_ROW_SQL), rich_rows());
    // A default recorded at creation is applied to a row inserted after the restart, and writes
    // after a restart survive another one.
    ok(&mut f.db, &mut s, "INSERT INTO rich (id) VALUES (3)");
    let mut f = reopen(f);
    let mut s = common::admin();
    let got = rows(
        &mut f.db,
        &mut s,
        "SELECT id, flag, small, d, amt, note, born FROM rich WHERE id = 3",
    );
    assert_eq!(
        got,
        strs(&[&["3", "t", "-7", "1.5", "12.5", "it's", "2020-02-29"]])
    );
}

#[test]
fn constraints_are_still_enforced_after_reopen() {
    let mut f = reopen(rich_fixture());
    let mut s = common::admin();
    assert_eq!(
        err(&mut f.db, &mut s, "INSERT INTO rich (id) VALUES (1)").sqlstate(),
        "23505"
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO rich (id, code) VALUES (5, 'abc')"
        )
        .sqlstate(),
        "23505"
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO rich (id, flag) VALUES (5, NULL)"
        )
        .sqlstate(),
        "23502"
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO rich (id, code) VALUES (5, 'toolong')"
        )
        .sqlstate(),
        "22001"
    );
    // NULLs are distinct in a UNIQUE column.
    ok(&mut f.db, &mut s, "INSERT INTO rich (id) VALUES (6)");
    ok(&mut f.db, &mut s, "INSERT INTO rich (id) VALUES (7)");
}

#[test]
fn unique_constraint_still_enforced_after_reopen_with_markers_from_the_record() {
    let mut f = fixture();
    let mut s = common::admin();
    // The constraints are declared in an order that is neither the column order nor the alphabetical
    // one, so only the record can say which marker table belongs to which.
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE t (a INT, b INT, c INT, UNIQUE (c), UNIQUE (b, a))",
    );
    ok(&mut f.db, &mut s, "INSERT INTO t VALUES (1, 2, 3)");
    let before = f.db.schema().table("t").unwrap().clone();
    assert_eq!(before.uniques, vec![vec![2], vec![1, 0]]);
    let mut f = reopen(f);
    let mut s = common::admin();
    let after = f.db.schema().table("t").unwrap().clone();
    assert_eq!(after.uniques, before.uniques);
    assert_eq!(after.unique_name(0), "t_unique_0");
    assert_eq!(after.unique_name(1), "t_unique_1");
    // The entries written before the restart sit in the marker tables the record names: a duplicate
    // of constraint 0 and a duplicate of constraint 1 are each found, under the right name.
    let e = err(&mut f.db, &mut s, "INSERT INTO t VALUES (9, 8, 3)");
    assert_eq!(e.sqlstate(), "23505");
    assert!(e.message().contains("t_unique_0"), "{}", e.message());
    let e = err(&mut f.db, &mut s, "INSERT INTO t VALUES (1, 2, 4)");
    assert_eq!(e.sqlstate(), "23505");
    assert!(e.message().contains("t_unique_1"), "{}", e.message());
    // Neither is violated by a row that differs in the other columns.
    ok(&mut f.db, &mut s, "INSERT INTO t VALUES (2, 2, 4)");
    assert_eq!(
        f.db.store()
            .scan_at(
                f.db.store().snapshot(),
                before.marker_table(1),
                ..,
                &lattice_engines::limits::AllowAll
            )
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn column_order_change_cannot_remap_a_unique_marker() {
    // Two tables with the same columns and the same constraints in opposite declaration order. A
    // schema rebuilt by column order or by name would swap their markers; the record keeps each.
    let mut f = fixture();
    let mut s = common::admin();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE p (x INT, y INT, UNIQUE (x), UNIQUE (y))",
    );
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE q (x INT, y INT, UNIQUE (y), UNIQUE (x))",
    );
    ok(&mut f.db, &mut s, "INSERT INTO p VALUES (1, 10)");
    ok(&mut f.db, &mut s, "INSERT INTO q VALUES (1, 10)");
    let mut f = reopen(f);
    let mut s = common::admin();
    assert_eq!(
        f.db.schema().table("p").unwrap().uniques,
        vec![vec![0], vec![1]]
    );
    assert_eq!(
        f.db.schema().table("q").unwrap().uniques,
        vec![vec![1], vec![0]]
    );
    for table in ["p", "q"] {
        let e = err(
            &mut f.db,
            &mut s,
            &format!("INSERT INTO {table} VALUES (1, 11)"),
        );
        assert_eq!(e.sqlstate(), "23505", "{table}");
        let e = err(
            &mut f.db,
            &mut s,
            &format!("INSERT INTO {table} VALUES (2, 10)"),
        );
        assert_eq!(e.sqlstate(), "23505", "{table}");
        ok(
            &mut f.db,
            &mut s,
            &format!("INSERT INTO {table} VALUES (2, 11)"),
        );
    }
}

#[test]
fn drop_table_removes_the_definition_from_the_current_catalog_but_as_of_still_decodes_old_rows() {
    let mut f = fixture();
    let mut s = common::admin();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE gone (id INT PRIMARY KEY, v TEXT)",
    );
    ok(&mut f.db, &mut s, "INSERT INTO gone VALUES (1, 'kept')");
    let id = f.db.schema().table("gone").unwrap().id;
    let before_drop = f.db.store().catalog().applied_lsn();
    ok(&mut f.db, &mut s, "DROP TABLE gone");
    let mut f = reopen(f);
    let mut s = common::admin();
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT * FROM gone").sqlstate(),
        "42P01"
    );
    let catalog = f.db.store().catalog();
    assert!(catalog.definition(id).is_none());
    assert!(catalog.definitions().is_empty());
    assert!(
        catalog.definition_as_of(id, before_drop).is_some(),
        "an earlier snapshot can still decode the table's rows"
    );
    assert!(
        catalog
            .definition_as_of(id, catalog.applied_lsn())
            .is_none()
    );
    // The name is free again and a new table gets a new id and its own definition.
    ok(&mut f.db, &mut s, "CREATE TABLE gone (a TEXT NOT NULL)");
    assert_ne!(f.db.schema().table("gone").unwrap().id, id);
    let mut f = reopen(f);
    let mut s = common::admin();
    assert_eq!(
        err(&mut f.db, &mut s, "INSERT INTO gone VALUES (NULL)").sqlstate(),
        "23502"
    );
    ok(&mut f.db, &mut s, "INSERT INTO gone VALUES ('new')");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT a FROM gone"),
        strs(&[&["new"]])
    );
}

#[test]
fn a_numeric_default_reads_the_same_before_and_after_a_restart() {
    // The record keeps numeric defaults without trailing zeros, so the table normalises them when it
    // is created; `1.50` is `1.5` from the first statement on, not only after a restart.
    let mut f = fixture();
    let mut s = common::admin();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE numdef (id INT, a NUMERIC DEFAULT 1.50, b NUMERIC DEFAULT 0.00, c INT DEFAULT NULL)",
    );
    ok(&mut f.db, &mut s, "INSERT INTO numdef (id) VALUES (1)");
    let want = strs(&[&["1", "1.5", "0", "NULL"]]);
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id, a, b, c FROM numdef"),
        want
    );
    let mut f = reopen(f);
    let mut s = common::admin();
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id, a, b, c FROM numdef"),
        want
    );
    ok(&mut f.db, &mut s, "INSERT INTO numdef (id) VALUES (2)");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT a, b FROM numdef WHERE id = 2"),
        strs(&[&["1.5", "0"]])
    );
}

#[test]
fn a_column_name_the_record_cannot_hold_is_refused_and_leaves_no_table() {
    let mut f = fixture();
    let mut s = common::admin();
    // The record format keeps column names without ASCII capitals (formats/sql-definition-v1.md).
    let e = err(
        &mut f.db,
        &mut s,
        "CREATE TABLE caps (\"Id\" INT PRIMARY KEY)",
    );
    assert_eq!(e.sqlstate(), "0A000");
    assert!(f.db.schema().table("caps").is_none());
    assert!(f.db.store().catalog().current().is_empty());
    ok(&mut f.db, &mut s, "CREATE TABLE caps (id INT PRIMARY KEY)");
    let mut f = reopen(f);
    assert!(f.db.schema().table("caps").is_some());
    let mut s = common::admin();
    ok(&mut f.db, &mut s, "INSERT INTO caps VALUES (1)");
}

// ----- the upgrade of a directory written before definitions existed -----

fn new_disk() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(7, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}

fn open_db(disk: &SharedIo, clock: &Rc<SimClock>) -> Result<Db, lattice_exec::ExecError> {
    Database::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(5 + disk.mutating_ops()),
        &config(),
    )
}

/// A directory written by a build that only knew table names: one catalog entry, no definition.
fn legacy_directory() -> (SharedIo, Rc<SimClock>) {
    let (disk, clock) = new_disk();
    let mut store = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(11),
        &config(),
    )
    .unwrap();
    let tx = store.begin();
    store
        .commit_with_catalog(
            tx,
            vec![CatalogOp::Create {
                table: 1,
                name: "old".into(),
            }],
            Vec::new(),
        )
        .unwrap();
    drop(store);
    disk.restart_process();
    (disk, clock)
}

#[test]
fn an_existing_directory_needs_the_upgrade_setting_before_its_first_definition() {
    let (disk, clock) = legacy_directory();
    let mut db = open_db(&disk, &clock).unwrap();
    let mut s = common::admin();
    // The table that has no definition is not addressable and keeps its name.
    assert_eq!(
        err(&mut db, &mut s, "SELECT * FROM old").sqlstate(),
        "42P01"
    );
    assert_eq!(
        err(&mut db, &mut s, "CREATE TABLE old (a INT)").sqlstate(),
        "42P07"
    );
    let (applied, ops) = (db.store().catalog().applied_lsn(), disk.mutating_ops());
    let e = err(&mut db, &mut s, "CREATE TABLE fresh (a INT)");
    assert_eq!(e.sqlstate(), "55000");
    assert!(e.message().contains("allow format upgrade"), "{e}");
    assert!(db.schema().table("fresh").is_none());
    assert_eq!(db.store().catalog().current().len(), 1);
    assert_eq!(
        (db.store().catalog().applied_lsn(), disk.mutating_ops()),
        (applied, ops),
        "the refused CREATE TABLE changed the log or the catalog"
    );
    db.set_allow_format_upgrade(true);
    ok(&mut db, &mut s, "CREATE TABLE fresh (a INT PRIMARY KEY)");
    ok(&mut db, &mut s, "INSERT INTO fresh VALUES (1)");
    drop(db);
    disk.restart_process();
    // The log now carries the guard: the setting is off again and a later table needs nothing.
    let mut db = open_db(&disk, &clock).unwrap();
    let mut s = common::admin();
    assert_eq!(
        rows(&mut db, &mut s, "SELECT a FROM fresh"),
        strs(&[&["1"]])
    );
    ok(&mut db, &mut s, "CREATE TABLE second (b TEXT)");
}

#[test]
fn a_new_directory_needs_no_upgrade_setting() {
    // `fixture` never sets it and every other test creates tables in a new directory.
    let mut f = fixture();
    assert!(!f.db.store().allow_format_upgrade());
    ok(&mut f.db, &mut common::admin(), "CREATE TABLE t (a INT)");
}

#[test]
fn a_stored_definition_the_executor_cannot_serve_refuses_to_open() {
    let (disk, clock) = new_disk();
    let mut store = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(11),
        &config(),
    )
    .unwrap();
    let tx = store.begin();
    // Structurally valid for the engine, which cannot tell that the default is not JSON.
    let definition = SqlTableDef {
        columns: vec![Column {
            name: "doc".into(),
            ty: SqlType::Jsonb,
            varchar_limit: 0,
            not_null: false,
            default: Some(DefaultValue::Jsonb("not json".into())),
        }],
        primary_key: None,
        uniques: Vec::new(),
    };
    store
        .commit_with_definitions(
            tx,
            vec![CatalogOp::Create {
                table: 1,
                name: "bad".into(),
            }],
            Vec::new(),
            vec![DefineTable {
                table_id: 1,
                definition,
            }],
        )
        .unwrap();
    drop(store);
    disk.restart_process();
    match open_db(&disk, &clock) {
        Err(e) => {
            assert_eq!(e.sqlstate(), "XX001");
            assert!(e.message().contains("bad"), "{e}");
        }
        Ok(_) => panic!("a definition the executor cannot use must refuse the open"),
    }
}

/// Commits one table with one definition through the engine, then closes the store.
fn write_definition(
    disk: &SharedIo,
    clock: &Rc<SimClock>,
    id: u32,
    name: &str,
    column: Column,
) -> Result<(), lattice_engines::store::RowError> {
    let mut store = RowStore::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(11),
        &config(),
    )
    .unwrap();
    let tx = store.begin();
    let result = store.commit_with_definitions(
        tx,
        vec![CatalogOp::Create {
            table: id,
            name: name.into(),
        }],
        Vec::new(),
        vec![DefineTable {
            table_id: id,
            definition: SqlTableDef {
                columns: vec![column],
                primary_key: None,
                uniques: Vec::new(),
            },
        }],
    );
    drop(store);
    disk.restart_process();
    result.map(|_| ())
}

fn numeric_column(default: &str) -> Column {
    Column {
        name: "n".into(),
        ty: SqlType::Numeric,
        varchar_limit: 0,
        not_null: false,
        default: Some(DefaultValue::Numeric(default.into())),
    }
}

#[test]
fn a_numeric_default_the_executor_cannot_parse_refuses_to_open() {
    let (disk, clock) = new_disk();
    // Canonical for the engine, but 46 places overflow the executor's 128-bit numeric.
    let text = format!("1{}", "0".repeat(45));
    write_definition(&disk, &clock, 1, "wide", numeric_column(&text)).unwrap();
    match open_db(&disk, &clock) {
        Err(e) => {
            assert_eq!(e.sqlstate(), "XX001");
            assert!(e.message().contains("wide"), "{e}");
        }
        Ok(_) => panic!("an unparsable numeric default must refuse the open"),
    }
}

#[test]
fn a_table_id_past_the_executor_limit_never_reaches_the_executor() {
    // The engine refuses a definition for an id above 2^26 - 1 before it is written, so the
    // executor's own check of the id (also XX001) cannot be reached through the log; the same holds
    // for a definition whose table is not in the catalog names, which replay already refuses.
    let (disk, clock) = new_disk();
    assert!(write_definition(&disk, &clock, 1 << 26, "huge", numeric_column("1")).is_err());
    let db = open_db(&disk, &clock).unwrap();
    assert!(db.schema().table("huge").is_none());
    assert!(db.store().catalog().definitions().is_empty());
}

// ----- crash points -----

/// Runs `CREATE TABLE t` on a disk that loses power after `cut` mutating operations, restarts the
/// machine and checks that the table is either whole or absent.
fn cut_during_create(
    profile: FaultProfile,
    with_earlier_table: bool,
    cut: u64,
) -> Result<(), String> {
    let mut f = fixture();
    let mut s = common::admin();
    f.disk.set_profile(profile);
    if with_earlier_table {
        ok(&mut f.db, &mut s, "CREATE TABLE base (id INT PRIMARY KEY)");
        ok(&mut f.db, &mut s, "INSERT INTO base VALUES (1)");
    }
    f.disk.arm_power_cut(cut);
    let attempt = s.execute(
        &mut f.db,
        "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(4) UNIQUE NOT NULL DEFAULT 'd')",
    );
    let Fixture { db, disk, clock } = f;
    drop(db);
    disk.crash();
    let mut db = open_db(&disk, &clock).map_err(|e| format!("cut {cut}: reopen failed: {e}"))?;
    let mut s = common::admin();
    let named = db.store().catalog().current().values().any(|n| n == "t");
    let defined = db.schema().table("t").is_some();
    if named != defined {
        return Err(format!(
            "cut {cut}: the catalog has the table: {named}, SQL has its columns: {defined}"
        ));
    }
    if attempt.is_ok() && !defined {
        return Err(format!("cut {cut}: an acknowledged CREATE TABLE was lost"));
    }
    if with_earlier_table {
        let kept = rows(&mut db, &mut s, "SELECT id FROM base");
        if kept != strs(&[&["1"]]) {
            return Err(format!("cut {cut}: the earlier table changed: {kept:?}"));
        }
    }
    if !defined {
        // Whole or absent: an absent table leaves its name free.
        s.execute(&mut db, "CREATE TABLE t (id INT PRIMARY KEY)")
            .map_err(|e| format!("cut {cut}: the name is not free: {e}"))?;
        return Ok(());
    }
    s.execute(&mut db, "INSERT INTO t (id, v) VALUES (1, 'a')")
        .map_err(|e| format!("cut {cut}: insert failed: {e}"))?;
    s.execute(&mut db, "INSERT INTO t (id) VALUES (2)")
        .map_err(|e| format!("cut {cut}: default not applied: {e}"))?;
    match s.execute(&mut db, "INSERT INTO t (id, v) VALUES (3, 'a')") {
        Err(e) if e.sqlstate() == "23505" => Ok(()),
        other => Err(format!("cut {cut}: UNIQUE not enforced: {other:?}")),
    }
}

fn create_ops(with_earlier_table: bool) -> u64 {
    let mut f = fixture();
    let mut s = common::admin();
    if with_earlier_table {
        ok(&mut f.db, &mut s, "CREATE TABLE base (id INT PRIMARY KEY)");
        ok(&mut f.db, &mut s, "INSERT INTO base VALUES (1)");
    }
    let before = f.disk.mutating_ops();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(4) UNIQUE NOT NULL DEFAULT 'd')",
    );
    f.disk.mutating_ops() - before
}

#[test]
fn a_power_cut_at_every_point_of_create_table_leaves_the_table_whole_or_absent() {
    let mut points = 0;
    for with_earlier_table in [false, true] {
        let total = create_ops(with_earlier_table);
        assert!(total >= 2, "a commit is at least an append and a sync");
        for profile in [FaultProfile::clean(), FaultProfile::crash_heavy()] {
            // One past the last operation: the cut never hits and the CREATE is acknowledged.
            for cut in 0..=total {
                cut_during_create(profile, with_earlier_table, cut).unwrap();
                points += 1;
            }
        }
    }
    assert!(points >= 8);
}

#[test]
fn a_table_created_without_its_definition_is_caught_by_the_restart_check() {
    fn survives_a_restart() -> Result<(), String> {
        let mut f = fixture();
        let mut s = common::admin();
        ok(&mut f.db, &mut s, "CREATE TABLE t (id INT PRIMARY KEY)");
        ok(&mut f.db, &mut s, "INSERT INTO t VALUES (1)");
        let mut f = reopen(f);
        let mut s = common::admin();
        match s.execute(&mut f.db, "SELECT id FROM t") {
            Ok(r) if r.rows.len() == 1 => Ok(()),
            Ok(r) => Err(format!("{} rows came back", r.rows.len())),
            Err(e) => Err(format!("the table did not survive: {e}")),
        }
    }
    assert_eq!(survives_a_restart(), Ok(()));
    let _g = Planted::on(Bug::ExecDefinitionNotPersisted);
    assert!(survives_a_restart().is_err());
}
