//! `SELECT` without `FROM`: one row with no columns as the source. Expected values are worked out
//! from PostgreSQL's rules, not from running the executor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{err, fixture, ok, rows};

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
