//! `SELECT` without `FROM`: one row with no columns as the source. Expected values are worked out
//! from PostgreSQL's rules, not from running the executor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::rc::Rc;

use common::{err, fixture, ok, rows};
use lattice_engines::limits::DenyAll;
use lattice_exec::{ActorPolicy, ExecSession, TxStatus};

fn strs(v: &[&[&str]]) -> Vec<Vec<String>> {
    v.iter()
        .map(|r| r.iter().map(ToString::to_string).collect())
        .collect()
}

#[test]
fn constant_expressions_yield_exactly_one_row() {
    let mut f = fixture();
    let mut s = common::admin();
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT 1 + 2, 'a' || 'b', NULL IS NULL"),
        strs(&[&["3", "ab", "t"]])
    );
    let r = ok(&mut f.db, &mut s, "SELECT 7 AS seven");
    assert_eq!(r.tag, "SELECT 1");
    assert_eq!(r.columns.len(), 1);
}

#[test]
fn where_limit_and_offset_apply_to_the_single_row() {
    let mut f = fixture();
    let mut s = common::admin();
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT 1 WHERE 1 = 1"),
        strs(&[&["1"]])
    );
    assert!(rows(&mut f.db, &mut s, "SELECT 1 WHERE 1 = 2").is_empty());
    assert!(rows(&mut f.db, &mut s, "SELECT 1 WHERE NULL").is_empty());
    assert!(rows(&mut f.db, &mut s, "SELECT 1 LIMIT 0").is_empty());
    assert!(rows(&mut f.db, &mut s, "SELECT 1 OFFSET 1").is_empty());
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT 1 LIMIT 5 OFFSET 0"),
        strs(&[&["1"]])
    );
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT 1 LIMIT -1").sqlstate(),
        "2201W"
    );
}

#[test]
fn constant_errors_surface_with_their_sqlstate() {
    let mut f = fixture();
    let mut s = common::admin();
    assert_eq!(err(&mut f.db, &mut s, "SELECT 1 / 0").sqlstate(), "22012");
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT 2147483647 + 1").sqlstate(),
        "22003"
    );
}

#[test]
fn there_is_no_table_to_name() {
    let mut f = fixture();
    let mut s = common::admin();
    assert_eq!(err(&mut f.db, &mut s, "SELECT *").sqlstate(), "42601");
    assert_eq!(err(&mut f.db, &mut s, "SELECT x").sqlstate(), "42703");
    assert_eq!(err(&mut f.db, &mut s, "SELECT t.*").sqlstate(), "42P01");
}

#[test]
fn constant_select_feeds_an_insert() {
    let mut f = fixture();
    let mut s = common::admin();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE t (id INT PRIMARY KEY, n TEXT)",
    );
    let r = ok(&mut f.db, &mut s, "INSERT INTO t SELECT 1, 'x'");
    assert_eq!(r.tag, "INSERT 0 1");
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id, n FROM t"),
        strs(&[&["1", "x"]])
    );
}

#[test]
fn date_text_shaped_like_a_date_but_out_of_range_is_22008() {
    let mut f = fixture();
    let mut s = common::admin();
    // PostgreSQL: "date/time field value out of range" for a shaped value, "invalid input syntax"
    // (22007) for text that is not shaped like a date.
    for sql in [
        "SELECT date '2023-02-29'",
        "SELECT date '2023-13-01'",
        "SELECT date '2023-00-10'",
        "SELECT CAST('2024-04-31' AS timestamptz)",
        "SELECT CAST('2024-01-01 25:00:00' AS timestamptz)",
        "SELECT CAST('2024-01-01 10:61:00' AS timestamptz)",
    ] {
        assert_eq!(err(&mut f.db, &mut s, sql).sqlstate(), "22008", "{sql}");
    }
    for sql in [
        "SELECT date 'not a date'",
        "SELECT date '2023-02'",
        "SELECT CAST('2024-01-01 ab:cd' AS timestamptz)",
    ] {
        assert_eq!(err(&mut f.db, &mut s, sql).sqlstate(), "22007", "{sql}");
    }
}

#[test]
fn a_principal_that_sees_nothing_can_still_run_a_constant_select() {
    let mut f = fixture();
    let mut admin = common::admin();
    ok(&mut f.db, &mut admin, "CREATE TABLE t (id INT PRIMARY KEY)");
    // Deny-all, catalog hidden: no table is visible, but a constant query reads no table, so no
    // row policy is consulted. The session itself is all it needs.
    let mut s = ExecSession::new(ActorPolicy::new(Rc::new(DenyAll)).hiding_catalog());
    assert_eq!(rows(&mut f.db, &mut s, "SELECT 1"), strs(&[&["1"]]));
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT id FROM t").sqlstate(),
        "42P01"
    );
}

#[test]
fn a_constant_select_in_a_failed_block_is_refused() {
    let mut f = fixture();
    let mut s = common::admin();
    ok(&mut f.db, &mut s, "BEGIN");
    assert_eq!(err(&mut f.db, &mut s, "SELECT 1 / 0").sqlstate(), "22012");
    assert_eq!(s.status(), TxStatus::Failed);
    assert_eq!(err(&mut f.db, &mut s, "SELECT 1").sqlstate(), "25P02");
    ok(&mut f.db, &mut s, "ROLLBACK");
    assert_eq!(rows(&mut f.db, &mut s, "SELECT 1"), strs(&[&["1"]]));
}

#[test]
fn null_limit_and_offset_mean_no_bound() {
    let mut f = fixture();
    let mut s = common::admin();
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT 1 LIMIT NULL"),
        strs(&[&["1"]])
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT 1 OFFSET NULL"),
        strs(&[&["1"]])
    );
}

#[test]
fn a_parameter_in_a_constant_select_is_bound() {
    let mut f = fixture();
    let mut s = common::admin();
    let r = s
        .execute_with(&mut f.db, "SELECT $1 + 1", &[Some(b"41".to_vec())], &|| {
            false
        })
        .unwrap();
    assert_eq!(common::text(&r), strs(&[&["42"]]));
}

#[test]
fn hour_24_is_only_the_end_of_the_day() {
    let mut f = fixture();
    let mut s = common::admin();
    // PostgreSQL accepts 24:00:00 and rejects any later 24:xx.
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT CAST(CAST('2024-01-01 24:00:00+00' AS timestamptz) AS text)"
        ),
        strs(&[&["2024-01-02 00:00:00+00"]])
    );
    for sql in [
        "SELECT CAST('2024-01-01 24:30:00' AS timestamptz)",
        "SELECT CAST('2024-01-01 24:00:01' AS timestamptz)",
    ] {
        assert_eq!(err(&mut f.db, &mut s, sql).sqlstate(), "22008", "{sql}");
    }
}

#[test]
fn junk_after_a_date_is_a_syntax_error_even_when_the_date_is_out_of_range() {
    let mut f = fixture();
    let mut s = common::admin();
    // PostgreSQL reads every field before it checks ranges, so the unknown word wins.
    for sql in [
        "SELECT date '2023-02-29 junk'",
        "SELECT CAST('2023-02-29 junk' AS timestamptz)",
    ] {
        assert_eq!(err(&mut f.db, &mut s, sql).sqlstate(), "22007", "{sql}");
    }
}
