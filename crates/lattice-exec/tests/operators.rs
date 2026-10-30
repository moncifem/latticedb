//! One test group per operator, with expected values worked out by hand from PostgreSQL's rules.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{Db, err, fixture, ok, rows};
use lattice_exec::ExecSession;

fn session_with_notes() -> (Db, ExecSession) {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE notes (id INT PRIMARY KEY, title TEXT, score INT)",
    );
    let r = ok(
        &mut f.db,
        &mut s,
        "INSERT INTO notes VALUES (3, 'gamma', 20), (1, 'alpha', 10), (2, 'beta', NULL), (4, NULL, 20)",
    );
    assert_eq!(r.tag, "INSERT 0 4");
    (f.db, s)
}

fn strs(v: &[&[&str]]) -> Vec<Vec<String>> {
    v.iter()
        .map(|r| r.iter().map(ToString::to_string).collect())
        .collect()
}

// ----- Scan -----

#[test]
fn scan_returns_rows_in_primary_key_order_and_tags_the_count() {
    let (mut db, mut s) = session_with_notes();
    let r = ok(&mut db, &mut s, "SELECT id FROM notes");
    assert_eq!(r.tag, "SELECT 4");
    assert_eq!(
        common::text(&r),
        strs(&[&["1"], &["2"], &["3"], &["4"]]),
        "keys sort as integers, not as insertion order"
    );
    assert_eq!(r.columns[0].name, "id");
    let r = ok(&mut db, &mut s, "SELECT * FROM notes WHERE id = 2");
    assert_eq!(common::text(&r), strs(&[&["2", "beta", "NULL"]]));
    assert_eq!(
        r.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "title", "score"]
    );
}

#[test]
fn negative_integer_keys_sort_before_positive_ones() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(&mut f.db, &mut s, "CREATE TABLE k (id BIGINT PRIMARY KEY)");
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO k VALUES (5), (-3), (0), (-100), (7)",
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id FROM k"),
        strs(&[&["-100"], &["-3"], &["0"], &["5"], &["7"]])
    );
}

#[test]
fn an_empty_table_scans_to_no_rows() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(&mut f.db, &mut s, "CREATE TABLE e (id INT PRIMARY KEY)");
    let r = ok(&mut f.db, &mut s, "SELECT * FROM e");
    assert_eq!((r.rows.len(), r.tag.as_str()), (0, "SELECT 0"));
}

// ----- Filter -----

#[test]
fn filter_keeps_only_true_so_null_comparisons_drop_rows() {
    let (mut db, mut s) = session_with_notes();
    // score = NULL is never true; IS NULL is.
    assert!(rows(&mut db, &mut s, "SELECT id FROM notes WHERE score = NULL").is_empty());
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id FROM notes WHERE score IS NULL"),
        strs(&[&["2"]])
    );
    // NOT (NULL) is NULL, so the NULL-score row is in neither half.
    let high = rows(&mut db, &mut s, "SELECT id FROM notes WHERE score >= 20");
    let not_high = rows(
        &mut db,
        &mut s,
        "SELECT id FROM notes WHERE NOT (score >= 20)",
    );
    assert_eq!(high, strs(&[&["3"], &["4"]]));
    assert_eq!(not_high, strs(&[&["1"]]));
}

#[test]
fn filter_three_valued_and_or() {
    let (mut db, mut s) = session_with_notes();
    // id 2 has a NULL score: NULL AND true is NULL (dropped), NULL OR true is true (kept).
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE score > 5 AND id >= 2"
        ),
        strs(&[&["3"], &["4"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE score > 15 OR id = 2"
        ),
        strs(&[&["2"], &["3"], &["4"]])
    );
    // false AND NULL is false; false OR NULL is NULL: neither keeps id 2 (score NULL, id 2 > 5 false).
    assert!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE id > 5 AND score > 5"
        )
        .is_empty()
    );
    assert!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE id = 2 AND score > 5"
        )
        .is_empty()
    );
}

#[test]
fn in_between_and_like() {
    let (mut db, mut s) = session_with_notes();
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE id IN (1, 3, 9)"
        ),
        strs(&[&["1"], &["3"]])
    );
    // x NOT IN (list with NULL) is never true unless a match is impossible to rule out... it is NULL.
    assert!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE id NOT IN (9, NULL)"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE id NOT IN (1, 2)"
        ),
        strs(&[&["3"], &["4"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE id BETWEEN 2 AND 3"
        ),
        strs(&[&["2"], &["3"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE id NOT BETWEEN 2 AND 3"
        ),
        strs(&[&["1"], &["4"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE title LIKE '%a'"
        ),
        strs(&[&["1"], &["2"], &["3"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE title LIKE 'g_mma'"
        ),
        strs(&[&["3"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE title NOT LIKE '%a'"
        ),
        Vec::<Vec<String>>::new()
    );
}

// ----- Project -----

#[test]
fn project_evaluates_expressions_with_postgres_types() {
    let (mut db, mut s) = session_with_notes();
    let r = rows(
        &mut db,
        &mut s,
        "SELECT id * 2 + 1, score / 3, score % 3, -id, title || '!', coalesce(score, -1), nullif(score, 20) FROM notes WHERE id IN (1, 2, 3) ORDER BY id",
    );
    assert_eq!(
        r,
        strs(&[
            &["3", "3", "1", "-1", "alpha!", "10", "10"],
            &["5", "NULL", "NULL", "-2", "beta!", "-1", "NULL"],
            &["7", "6", "2", "-3", "gamma!", "20", "NULL"],
        ])
    );
}

#[test]
fn numeric_is_exact_and_double_is_not() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE n (id INT PRIMARY KEY, x NUMERIC, y NUMERIC, f DOUBLE PRECISION, g DOUBLE PRECISION)",
    );
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO n VALUES (1, 0.1, 0.2, 0.1, 0.2)",
    );
    // 0.1 + 0.2 is exactly 0.3 in numeric, and the usual 0.30000000000000004 in binary floating point.
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT x + y, x * 3, f + g, 1 / 3.0 FROM n"
        ),
        strs(&[&[
            "0.3",
            "0.3",
            "0.30000000000000004",
            "0.33333333333333333333"
        ]])
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT x = 0.10, f = 0.1, x < y FROM n"),
        strs(&[&["t", "t", "t"]]),
        "numeric equality ignores the scale it was written with"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT x / (y - 0.2) FROM n").sqlstate(),
        "22012"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT f / (g - 0.2) FROM n").sqlstate(),
        "22012"
    );
}

#[test]
fn string_functions() {
    let (mut db, mut s) = session_with_notes();
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT upper(title), length(title), substring(title, 2, 3), position('m' IN title), trim(BOTH 'a' FROM title) FROM notes WHERE id = 3"
        ),
        strs(&[&["GAMMA", "5", "amm", "3", "gamm"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT lower('ÀB'), trim('  x '), abs(-4), round(2.5), round(1.2345, 2) FROM notes WHERE id = 1"
        ),
        strs(&[&["àb", "x", "4", "3", "1.23"]])
    );
}

// ----- Sort -----

#[test]
fn sort_places_nulls_last_ascending_and_first_descending_by_default() {
    let (mut db, mut s) = session_with_notes();
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id FROM notes ORDER BY score, id"),
        strs(&[&["1"], &["3"], &["4"], &["2"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes ORDER BY score DESC, id"
        ),
        strs(&[&["2"], &["3"], &["4"], &["1"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes ORDER BY score NULLS FIRST, id DESC"
        ),
        strs(&[&["2"], &["1"], &["4"], &["3"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes ORDER BY score DESC NULLS LAST, id DESC"
        ),
        strs(&[&["4"], &["3"], &["1"], &["2"]])
    );
}

#[test]
fn sort_by_alias_position_and_expression() {
    let (mut db, mut s) = session_with_notes();
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id AS k FROM notes ORDER BY k DESC"),
        strs(&[&["4"], &["3"], &["2"], &["1"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT title, id FROM notes ORDER BY 2 DESC"
        ),
        strs(&[
            &["NULL", "4"],
            &["gamma", "3"],
            &["beta", "2"],
            &["alpha", "1"]
        ])
    );
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id FROM notes ORDER BY -id"),
        strs(&[&["4"], &["3"], &["2"], &["1"]])
    );
    // Text sorts bytewise: uppercase before lowercase.
    ok(&mut db, &mut s, "INSERT INTO notes VALUES (5, 'Zed', 0)");
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT title FROM notes WHERE id >= 3 ORDER BY title NULLS FIRST"
        ),
        strs(&[&["NULL"], &["Zed"], &["gamma"]])
    );
}

#[test]
fn sort_is_stable_for_equal_keys() {
    let (mut db, mut s) = session_with_notes();
    // Both id 3 and 4 have score 20; rows arrive in key order, and a stable sort keeps it.
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes WHERE score = 20 ORDER BY score"
        ),
        strs(&[&["3"], &["4"]])
    );
}

// ----- Limit / Offset -----

#[test]
fn limit_and_offset_apply_after_the_sort() {
    let (mut db, mut s) = session_with_notes();
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes ORDER BY id DESC LIMIT 2"
        ),
        strs(&[&["4"], &["3"]])
    );
    assert_eq!(
        rows(
            &mut db,
            &mut s,
            "SELECT id FROM notes ORDER BY id LIMIT 2 OFFSET 1"
        ),
        strs(&[&["2"], &["3"]])
    );
    assert!(rows(&mut db, &mut s, "SELECT id FROM notes LIMIT 0").is_empty());
    assert!(rows(&mut db, &mut s, "SELECT id FROM notes OFFSET 10").is_empty());
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id FROM notes OFFSET 3"),
        strs(&[&["4"]])
    );
}

#[test]
fn limit_does_not_evaluate_rows_it_never_returns() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE z (id INT PRIMARY KEY, d INT)",
    );
    ok(&mut f.db, &mut s, "INSERT INTO z VALUES (1, 1), (2, 0)");
    // Row 2 would divide by zero; PostgreSQL stops after the first row, and so does the executor.
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT 10 / d FROM z ORDER BY id LIMIT 1"
        ),
        strs(&[&["10"]])
    );
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT 10 / d FROM z ORDER BY id").sqlstate(),
        "22012"
    );
}

#[test]
fn a_negative_limit_parameter_is_refused_at_run_time() {
    let (mut db, mut s) = session_with_notes();
    let e = s
        .execute_with(
            &mut db,
            "SELECT id FROM notes LIMIT $1",
            &[Some(b"-1".to_vec())],
            &|| false,
        )
        .unwrap_err();
    assert_eq!(e.sqlstate(), "2201W");
}

// ----- Insert -----

#[test]
fn insert_fills_defaults_and_returns_columns() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE items (id INT PRIMARY KEY, label TEXT NOT NULL, qty INT DEFAULT 7, note TEXT DEFAULT 'n/a', ok BOOLEAN DEFAULT true)",
    );
    let r = ok(
        &mut f.db,
        &mut s,
        "INSERT INTO items (id, label) VALUES (1, 'bolt') RETURNING qty, note, ok, id + 100",
    );
    assert_eq!(r.tag, "INSERT 0 1");
    assert_eq!(common::text(&r), strs(&[&["7", "n/a", "t", "101"]]));
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO items (label, id, qty) VALUES ('nut', 2, NULL)",
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id, qty FROM items ORDER BY id"),
        strs(&[&["1", "7"], &["2", "NULL"]]),
        "an explicit NULL overrides the default"
    );
}

#[test]
fn insert_select_copies_rows_with_assignment_casts() {
    let (mut db, mut s) = session_with_notes();
    ok(
        &mut db,
        &mut s,
        "CREATE TABLE copy (id BIGINT PRIMARY KEY, score NUMERIC)",
    );
    let r = ok(
        &mut db,
        &mut s,
        "INSERT INTO copy SELECT id + 10, score FROM notes WHERE score IS NOT NULL",
    );
    assert_eq!(r.tag, "INSERT 0 3");
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id, score FROM copy ORDER BY id"),
        strs(&[&["11", "10"], &["13", "20"], &["14", "20"]])
    );
}

#[test]
fn constraint_violations_use_the_documented_sqlstates() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE u (id INT PRIMARY KEY, label TEXT NOT NULL, sku TEXT UNIQUE, a INT, b INT, UNIQUE (a, b))",
    );
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO u VALUES (1, 'x', 'S1', 1, 1)",
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO u VALUES (1, 'y', 'S2', 2, 2)"
        )
        .sqlstate(),
        "23505"
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO u VALUES (2, 'y', 'S1', 2, 2)"
        )
        .sqlstate(),
        "23505"
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO u VALUES (2, 'y', 'S2', 1, 1)"
        )
        .sqlstate(),
        "23505"
    );
    // NULL is not a value for NOT NULL, supplied through a parameter so only the executor can see it.
    let e = s
        .execute_with(
            &mut f.db,
            "INSERT INTO u VALUES (3, $1, 'S3', 3, 3)",
            &[None],
            &|| false,
        )
        .unwrap_err();
    assert_eq!(e.sqlstate(), "23502");
    // NULLs are distinct for UNIQUE, so any number of rows may leave the column NULL.
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO u VALUES (4, 'a', NULL, NULL, 1), (5, 'b', NULL, NULL, 1)",
    );
    // Two equal keys inside one statement collide too.
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO u VALUES (6, 'a', 'S6', 6, 6), (6, 'b', 'S7', 7, 7)"
        )
        .sqlstate(),
        "23505"
    );
}

#[test]
fn a_failed_statement_changes_nothing() {
    let (mut db, mut s) = session_with_notes();
    let e = err(
        &mut db,
        &mut s,
        "INSERT INTO notes VALUES (10, 'ten', 1), (1, 'dup', 1)",
    );
    assert_eq!(e.sqlstate(), "23505");
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id FROM notes WHERE id >= 10"),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn value_length_and_integer_range_are_checked_on_assignment() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE r (id INT PRIMARY KEY, name VARCHAR(3), small SMALLINT)",
    );
    assert_eq!(
        err(&mut f.db, &mut s, "INSERT INTO r VALUES (1, 'abcd', 1)").sqlstate(),
        "22001"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "INSERT INTO r VALUES (1, 'abc', 40000)").sqlstate(),
        "22003"
    );
    ok(&mut f.db, &mut s, "INSERT INTO r VALUES (1, 'äbc', 32767)");
    // Arithmetic on a column is only known at run time.
    assert_eq!(
        err(&mut f.db, &mut s, "UPDATE r SET small = small + 1").sqlstate(),
        "22003"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "SELECT small * small * small FROM r").sqlstate(),
        "22003"
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT small FROM r"),
        strs(&[&["32767"]]),
        "the failed update left the row alone"
    );
}

// ----- Update -----

#[test]
fn update_changes_matching_rows_and_reports_the_count() {
    let (mut db, mut s) = session_with_notes();
    let r = ok(
        &mut db,
        &mut s,
        "UPDATE notes SET score = score + 1, title = upper(title) WHERE id <= 3 RETURNING id, score, title",
    );
    assert_eq!(r.tag, "UPDATE 3");
    assert_eq!(
        common::text(&r),
        strs(&[
            &["1", "11", "ALPHA"],
            &["2", "NULL", "BETA"],
            &["3", "21", "GAMMA"]
        ])
    );
    assert_eq!(
        ok(&mut db, &mut s, "UPDATE notes SET score = 0 WHERE id = 99").tag,
        "UPDATE 0"
    );
    // The right-hand side sees the old row, so swapping works.
    ok(
        &mut db,
        &mut s,
        "CREATE TABLE sw (id INT PRIMARY KEY, a INT, b INT)",
    );
    ok(&mut db, &mut s, "INSERT INTO sw VALUES (1, 10, 20)");
    ok(&mut db, &mut s, "UPDATE sw SET a = b, b = a");
    assert_eq!(
        rows(&mut db, &mut s, "SELECT a, b FROM sw"),
        strs(&[&["20", "10"]])
    );
}

#[test]
fn update_of_key_columns_moves_the_row_and_keeps_constraints() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE p (id INT PRIMARY KEY, sku TEXT UNIQUE)",
    );
    ok(&mut f.db, &mut s, "INSERT INTO p VALUES (1, 'a'), (2, 'b')");
    ok(
        &mut f.db,
        &mut s,
        "UPDATE p SET id = 10, sku = 'z' WHERE id = 1",
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT id, sku FROM p ORDER BY id"),
        strs(&[&["2", "b"], &["10", "z"]])
    );
    assert_eq!(
        err(&mut f.db, &mut s, "UPDATE p SET id = 2 WHERE id = 10").sqlstate(),
        "23505"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "UPDATE p SET sku = 'b' WHERE id = 10").sqlstate(),
        "23505"
    );
    // The released unique value can be taken again.
    ok(&mut f.db, &mut s, "UPDATE p SET sku = 'y' WHERE id = 10");
    ok(&mut f.db, &mut s, "INSERT INTO p VALUES (3, 'z')");
}

// ----- Delete -----

#[test]
fn delete_removes_rows_and_frees_their_keys() {
    let (mut db, mut s) = session_with_notes();
    let r = ok(
        &mut db,
        &mut s,
        "DELETE FROM notes WHERE score IS NULL OR id = 1 RETURNING id, title",
    );
    assert_eq!(r.tag, "DELETE 2");
    assert_eq!(common::text(&r), strs(&[&["1", "alpha"], &["2", "beta"]]));
    assert_eq!(
        rows(&mut db, &mut s, "SELECT id FROM notes"),
        strs(&[&["3"], &["4"]])
    );
    ok(&mut db, &mut s, "INSERT INTO notes VALUES (1, 'again', 1)");
    assert_eq!(ok(&mut db, &mut s, "DELETE FROM notes").tag, "DELETE 3");
    assert!(rows(&mut db, &mut s, "SELECT id FROM notes").is_empty());
}

// ----- types and tables without a key -----

#[test]
fn every_column_type_round_trips() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE t (id INT PRIMARY KEY, b BOOLEAN, i2 SMALLINT, i8 BIGINT, n NUMERIC, d DOUBLE PRECISION, s TEXT, y BYTEA, u UUID, dt DATE, ts TIMESTAMPTZ, j JSONB)",
    );
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO t VALUES (1, true, -5, 9223372036854775807, 12.340, 1.5, 'héllo', '\\xdeadbeef', 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', '2024-02-29', '2024-02-29 13:45:10.25+00', '{\"k\": [1, 2]}')",
    );
    assert_eq!(
        rows(
            &mut f.db,
            &mut s,
            "SELECT b, i2, i8, n, d, s, y, u, dt, ts, j FROM t"
        ),
        strs(&[&[
            "t",
            "-5",
            "9223372036854775807",
            "12.340",
            "1.5",
            "héllo",
            "\\xdeadbeef",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "2024-02-29",
            "2024-02-29 13:45:10.25+00",
            "{\"k\": [1, 2]}"
        ]])
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO t (id, j) VALUES (2, '{oops')"
        )
        .sqlstate(),
        "22P02"
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO t (id, dt) VALUES (2, '2023-02-29')"
        )
        .sqlstate(),
        "22007"
    );
    assert_eq!(
        err(
            &mut f.db,
            &mut s,
            "INSERT INTO t (id, u) VALUES (2, 'nope')"
        )
        .sqlstate(),
        "22P02"
    );
}

#[test]
fn composite_and_text_keys_order_by_value() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE c (a TEXT, b INT, PRIMARY KEY (a, b))",
    );
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO c VALUES ('b', 1), ('a', 2), ('a', 1), ('ab', 0), ('', 5)",
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT a, b FROM c"),
        strs(&[
            &["", "5"],
            &["a", "1"],
            &["a", "2"],
            &["ab", "0"],
            &["b", "1"]
        ]),
        "a shorter text sorts before its extensions, then by the next column"
    );
    assert_eq!(
        err(&mut f.db, &mut s, "INSERT INTO c VALUES ('a', 2)").sqlstate(),
        "23505"
    );
}

#[test]
fn a_table_without_a_key_keeps_duplicate_rows_and_updates_them_individually() {
    let mut f = fixture();
    let mut s = ExecSession::allow_all();
    ok(&mut f.db, &mut s, "CREATE TABLE log (msg TEXT, n INT)");
    ok(
        &mut f.db,
        &mut s,
        "INSERT INTO log VALUES ('a', 1), ('a', 1), ('b', 2)",
    );
    assert_eq!(rows(&mut f.db, &mut s, "SELECT msg FROM log").len(), 3);
    assert_eq!(
        ok(
            &mut f.db,
            &mut s,
            "UPDATE log SET n = n + 1 WHERE msg = 'a'"
        )
        .tag,
        "UPDATE 2"
    );
    assert_eq!(
        ok(
            &mut f.db,
            &mut s,
            "DELETE FROM log WHERE n = 2 AND msg = 'a'"
        )
        .tag,
        "DELETE 2"
    );
    assert_eq!(
        rows(&mut f.db, &mut s, "SELECT msg, n FROM log"),
        strs(&[&["b", "2"]])
    );
}

#[test]
fn unsupported_statements_are_refused_with_0a000() {
    let (mut db, mut s) = session_with_notes();
    for sql in [
        "SELECT * FROM notes a JOIN notes b ON a.id = b.id",
        "SELECT count(*) FROM notes",
        "SELECT DISTINCT score FROM notes",
        "CREATE INDEX i ON notes (score)",
        "ALTER TABLE notes ADD COLUMN x INT",
        "SAVEPOINT a",
    ] {
        assert_eq!(err(&mut db, &mut s, sql).sqlstate(), "0A000", "{sql}");
    }
    // A function outside the subset is refused by the parser, not bound as an unknown name.
    assert_eq!(
        err(&mut db, &mut s, "SELECT jsonb_nope(id) FROM notes").sqlstate(),
        "0A000"
    );
    assert_eq!(err(&mut db, &mut s, "SELEC 1").sqlstate(), "42601");
    assert_eq!(
        err(&mut db, &mut s, "SELECT * FROM missing").sqlstate(),
        "42P01"
    );
    assert_eq!(
        err(&mut db, &mut s, "SELECT nope FROM notes").sqlstate(),
        "42703"
    );
}
