//! `GROUP BY` and `HAVING` in one session, checked against PostgreSQL 16: grouping by columns,
//! several columns, expressions, positions and output names; `NULL` keys forming one group; every
//! aggregate per group; `HAVING` over aggregates and keys; `WHERE`, `ORDER BY` an aggregate, `LIMIT` and
//! `OFFSET`; empty inputs (no rows with a key, one row without); and the grouping errors. Each step
//! lists what PostgreSQL answered (`NULL` is an empty field) in the order of an `ORDER BY`, because
//! PostgreSQL's order without one is not defined. Left out: two statements that order by a tied value
//! (their tie order is unspecified), `count(DISTINCT x)` (not built) and an aggregate in `GROUP BY` or
//! `WHERE` (`42803` there, `0A000` here).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const STEPS: &[(&str, &[&str])] = &[
    (
        "CREATE TABLE gr (id INT PRIMARY KEY, g INT, h TEXT, v INT, n NUMERIC, f DOUBLE PRECISION)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO gr VALUES (1, 1, 'a', 10, 1.5, 1.0), (2, 1, 'b', 20, 2.5, NULL), (3, 2, 'a', 30, NULL, 3.0), (4, 2, 'a', NULL, 4.0, 4.0), (5, NULL, 'b', 50, 5.0, 5.0), (6, NULL, 'b', 60, 6.0, NULL), (7, 3, NULL, 70, 7.5, 7.0), (8, 3, NULL, NULL, NULL, NULL)",
        &["INSERT 0 8"],
    ),
    (
        "CREATE TABLE em (id INT PRIMARY KEY, g INT, v INT)",
        &["CREATE TABLE"],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY g ORDER BY g",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    (
        "SELECT g, count(v), sum(v), min(v), max(v) FROM gr GROUP BY g ORDER BY g",
        &[
            "1|2|30|10|20",
            "2|1|30|30|30",
            "3|1|70|70|70",
            "|2|110|50|60",
        ],
    ),
    (
        "SELECT g, avg(v), avg(n), sum(n), sum(f), avg(f) FROM gr GROUP BY g ORDER BY g",
        &[
            "1|15.0000000000000000|2.0000000000000000|4.0|1|1",
            "2|30.0000000000000000|4.0000000000000000|4.0|7|3.5",
            "3|70.0000000000000000|7.5000000000000000|7.5|7|7",
            "|55.0000000000000000|5.5000000000000000|11.0|5|5",
        ],
    ),
    (
        "SELECT h, count(*) FROM gr GROUP BY h ORDER BY h",
        &["a|3", "b|3", "|2"],
    ),
    (
        "SELECT h, g, count(*) FROM gr GROUP BY h, g ORDER BY h, g",
        &["a|1|1", "a|2|2", "b|1|1", "b||2", "|3|2"],
    ),
    (
        "SELECT g, h, sum(v) FROM gr GROUP BY g, h ORDER BY g NULLS FIRST, h NULLS FIRST",
        &["|b|110", "1|a|10", "1|b|20", "2|a|30", "3||70"],
    ),
    (
        "SELECT g FROM gr GROUP BY g ORDER BY g",
        &["1", "2", "3", ""],
    ),
    (
        "SELECT count(*) FROM gr GROUP BY g ORDER BY 1",
        &["2", "2", "2", "2"],
    ),
    (
        "SELECT sum(v) FROM gr GROUP BY h ORDER BY 1",
        &["40", "70", "130"],
    ),
    (
        "SELECT g + 1, count(*) FROM gr GROUP BY g + 1 ORDER BY 1",
        &["2|2", "3|2", "4|2", "|2"],
    ),
    (
        "SELECT g + 1 FROM gr GROUP BY g ORDER BY 1",
        &["2", "3", "4", ""],
    ),
    (
        "SELECT g * 2, sum(v) FROM gr GROUP BY g ORDER BY 1",
        &["2|30", "4|30", "6|70", "|110"],
    ),
    (
        "SELECT upper(h), count(*) FROM gr GROUP BY upper(h) ORDER BY 1",
        &["A|3", "B|3", "|2"],
    ),
    (
        "SELECT lower(h) FROM gr GROUP BY h ORDER BY 1",
        &["a", "b", ""],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY 1 ORDER BY 1",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    (
        "SELECT h, g, count(*) FROM gr GROUP BY 2, 1 ORDER BY 1, 2",
        &["a|1|1", "a|2|2", "b|1|1", "b||2", "|3|2"],
    ),
    (
        "SELECT g AS k, count(*) AS c FROM gr GROUP BY g ORDER BY k",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    (
        "SELECT g AS k, count(*) AS c FROM gr GROUP BY k ORDER BY k",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    (
        "SELECT g AS k, count(*) FROM gr GROUP BY 1 ORDER BY k DESC",
        &["|2", "3|2", "2|2", "1|2"],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY g HAVING count(*) > 1 ORDER BY g",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    (
        "SELECT g, sum(v) FROM gr GROUP BY g HAVING sum(v) > 50 ORDER BY g",
        &["3|70", "|110"],
    ),
    (
        "SELECT g FROM gr GROUP BY g HAVING max(v) IS NULL ORDER BY g",
        &[],
    ),
    (
        "SELECT g FROM gr GROUP BY g HAVING g IS NOT NULL ORDER BY g",
        &["1", "2", "3"],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY g HAVING g > 1 ORDER BY g",
        &["2|2", "3|2"],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY g HAVING count(*) > 100",
        &[],
    ),
    (
        "SELECT g FROM gr GROUP BY g HAVING sum(v) + 1 > 60 ORDER BY g",
        &["3", ""],
    ),
    (
        "SELECT g, count(*) c FROM gr GROUP BY g HAVING count(*) = 2 AND sum(v) > 0 ORDER BY g",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    ("SELECT count(*) FROM gr HAVING count(*) > 5", &["8"]),
    ("SELECT count(*) FROM gr HAVING count(*) > 50", &[]),
    ("SELECT sum(v) FROM gr HAVING sum(v) > 0", &["240"]),
    (
        "SELECT g, count(*) FROM gr WHERE v IS NOT NULL GROUP BY g ORDER BY g",
        &["1|2", "2|1", "3|1", "|2"],
    ),
    ("SELECT g, count(*) FROM gr WHERE id > 100 GROUP BY g", &[]),
    ("SELECT count(*) FROM gr WHERE id > 100 GROUP BY g", &[]),
    (
        "SELECT g, count(*) FROM gr GROUP BY g ORDER BY count(*) DESC, g",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    (
        "SELECT g, sum(v) FROM gr GROUP BY g ORDER BY sum(v) NULLS LAST, g",
        &["1|30", "2|30", "3|70", "|110"],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY g ORDER BY g LIMIT 2 OFFSET 1",
        &["2|2", "3|2"],
    ),
    (
        "SELECT g, max(h), min(h) FROM gr GROUP BY g ORDER BY g",
        &["1|b|a", "2|a|a", "3||", "|b|b"],
    ),
    (
        "SELECT g, sum(v) / count(*) FROM gr GROUP BY g ORDER BY g",
        &["1|15", "2|15", "3|35", "|55"],
    ),
    (
        "SELECT g, coalesce(sum(v), 0) FROM gr GROUP BY g ORDER BY g",
        &["1|30", "2|30", "3|70", "|110"],
    ),
    (
        "SELECT n, count(*) FROM gr GROUP BY n ORDER BY n",
        &["1.5|1", "2.5|1", "4.0|1", "5.0|1", "6.0|1", "7.5|1", "|2"],
    ),
    (
        "SELECT f, count(*) FROM gr GROUP BY f ORDER BY f",
        &["1|1", "3|1", "4|1", "5|1", "7|1", "|3"],
    ),
    ("SELECT id, count(*) FROM gr GROUP BY g", &["ERR 42803"]),
    ("SELECT g, h, count(*) FROM gr GROUP BY g", &["ERR 42803"]),
    ("SELECT h FROM gr GROUP BY g", &["ERR 42803"]),
    ("SELECT * FROM gr GROUP BY g", &["ERR 42803"]),
    ("SELECT g, count(*) FROM gr GROUP BY nope", &["ERR 42703"]),
    ("SELECT g FROM gr GROUP BY 5", &["ERR 42P10"]),
    ("SELECT g FROM gr GROUP BY 0", &["ERR 42P10"]),
    (
        "SELECT g, count(*) FROM gr GROUP BY g HAVING v > 1",
        &["ERR 42803"],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY g ORDER BY v",
        &["ERR 42803"],
    ),
    (
        "SELECT count(*) FROM gr GROUP BY g, g ORDER BY 1",
        &["2", "2", "2", "2"],
    ),
    ("SELECT g, count(*) FROM em GROUP BY g", &[]),
    ("SELECT count(*) FROM em GROUP BY g", &[]),
    ("SELECT count(*) FROM em", &["0"]),
    ("SELECT g, sum(v) FROM em GROUP BY g HAVING sum(v) > 1", &[]),
    (
        "SELECT 1 FROM gr GROUP BY g ORDER BY 1",
        &["1", "1", "1", "1"],
    ),
    (
        "SELECT 1, count(*) FROM gr GROUP BY g ORDER BY 2, 1",
        &["1|2", "1|2", "1|2", "1|2"],
    ),
    (
        "SELECT g, 'x', count(*) FROM gr GROUP BY g ORDER BY g",
        &["1|x|2", "2|x|2", "3|x|2", "|x|2"],
    ),
    (
        "SELECT g, count(*) FROM gr GROUP BY g, h ORDER BY g, h LIMIT 3",
        &["1|1", "1|1", "2|2"],
    ),
    (
        "INSERT INTO em SELECT g, g, count(*) FROM gr GROUP BY g RETURNING id, g, v",
        &["ERR 23502"],
    ),
    ("SELECT id, g, v FROM em ORDER BY id", &[]),
    (
        "SELECT g FROM gr GROUP BY g, h HAVING h = 'a' ORDER BY g",
        &["1", "2"],
    ),
    (
        "SELECT sum(v) FROM gr GROUP BY g, h HAVING g = 1 ORDER BY 1",
        &["10", "20"],
    ),
];

#[test]
fn group_by_and_having_answer_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in STEPS {
        let got: Vec<String> = match s.execute(&mut f.db, sql) {
            Ok(r) => {
                let mut lines: Vec<String> = text(&r)
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|c| if c == "NULL" { "" } else { c.as_str() })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect();
                if r.columns.is_empty() || !sql.starts_with("SELECT") {
                    lines.push(r.tag);
                }
                lines
            }
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}

/// Two keys give four groups; grouping by the first only would give two.
fn two_keys_make_four_groups() -> Result<(), String> {
    let mut f = fixture();
    let mut s = common::admin();
    common::ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE gk (id INT PRIMARY KEY, a INT, b INT)",
    );
    common::ok(
        &mut f.db,
        &mut s,
        "INSERT INTO gk VALUES (1, 1, 1), (2, 1, 2), (3, 2, 1), (4, 2, 2), (5, 2, 2)",
    );
    let got = common::rows(
        &mut f.db,
        &mut s,
        "SELECT a, b, count(*) FROM gk GROUP BY a, b ORDER BY a, b",
    );
    let want = [
        ["1", "1", "1"],
        ["1", "2", "1"],
        ["2", "1", "1"],
        ["2", "2", "2"],
    ];
    if got != want {
        return Err(format!("{got:?}"));
    }
    Ok(())
}

#[test]
fn a_group_by_that_uses_only_its_first_key_is_caught() {
    assert_eq!(two_keys_make_four_groups(), Ok(()));
    lattice_core::bugs::enable(lattice_core::bugs::Bug::GroupKeyTruncated);
    let caught = two_keys_make_four_groups().is_err();
    lattice_core::bugs::clear();
    assert!(caught);
}

#[test]
fn the_groups_are_charged_to_the_statement_memory_budget() {
    use lattice_exec::ExecLimits;
    let mut f = fixture();
    let mut s = common::admin();
    common::ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE t (id INT PRIMARY KEY, g INT)",
    );
    let values: Vec<String> = (0..200).map(|i| format!("({i}, {i})")).collect();
    common::ok(
        &mut f.db,
        &mut s,
        &format!("INSERT INTO t VALUES {}", values.join(", ")),
    );
    let limits = |memory_bytes| ExecLimits {
        timeout_ms: None,
        memory_bytes,
        transaction_bytes: u64::MAX,
        max_timeout_ms: None,
    };
    // 40 KiB holds the 200 scanned rows and one aggregate row, but not 200 groups on top of them.
    s.set_limits(limits(40 * 1024));
    assert_eq!(
        s.execute(&mut f.db, "SELECT count(*) FROM t")
            .unwrap()
            .rows
            .len(),
        1
    );
    let e = s
        .execute(&mut f.db, "SELECT g, count(*) FROM t GROUP BY g")
        .unwrap_err();
    assert_eq!(
        (e.sqlstate(), e.kind()),
        ("53200", lattice_exec::ErrorKind::Limit)
    );
    // A few groups fit in the same budget, and the same query fits a larger one.
    assert_eq!(
        s.execute(&mut f.db, "SELECT g % 5, count(*) FROM t GROUP BY g % 5")
            .unwrap()
            .rows
            .len(),
        5
    );
    s.set_limits(limits(1024 * 1024));
    assert_eq!(
        s.execute(&mut f.db, "SELECT g, count(*) FROM t GROUP BY g")
            .unwrap()
            .rows
            .len(),
        200
    );
}
