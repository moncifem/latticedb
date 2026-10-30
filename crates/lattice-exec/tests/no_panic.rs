//! No statement the binder accepts may panic the executor, make it return a malformed result or leave
//! partial writes behind, and the same statements must give the same outcomes on every run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{Db, fixture};
use lattice_exec::{ExecResult, ExecSession};
use proptest::prelude::*;

const SETUP: [&str; 3] = [
    "CREATE TABLE t (id INT PRIMARY KEY, i2 SMALLINT, i4 INT, i8 BIGINT, n NUMERIC, f DOUBLE PRECISION, s TEXT, b BOOLEAN, d DATE, ts TIMESTAMPTZ, u INT UNIQUE)",
    "INSERT INTO t VALUES (1, 1, 2147483647, 9223372036854775807, 1.5, 0.1, 'abc', true, '2024-02-29', '2024-02-29 10:20:30+00', 1), (2, -32768, -2147483648, 0, 0, 0, '', false, '2000-01-01', '2000-01-01 00:00:00+00', 2), (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    "INSERT INTO t (id, i4, s) VALUES (4, 0, 'xyz'), (5, 7, 'ab%_')",
];

fn int_lit() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("0".to_string()),
        Just("1".to_string()),
        Just("-1".to_string()),
        Just("2147483647".to_string()),
        Just("2147483648".to_string()),
        Just("32767".to_string()),
        Just("9223372036854775807".to_string()),
        (0i64..100).prop_map(|n| n.to_string()),
    ]
}

fn int_expr() -> BoxedStrategy<String> {
    let leaf = prop_oneof![
        int_lit(),
        Just("i2".to_string()),
        Just("i4".to_string()),
        Just("i8".to_string()),
        Just("id".to_string()),
        Just("u".to_string()),
    ];
    leaf.prop_recursive(4, 24, 3, |inner| {
        prop_oneof![
            (
                inner.clone(),
                prop_oneof![Just("+"), Just("-"), Just("*"), Just("/"), Just("%")],
                inner.clone()
            )
                .prop_map(|(a, op, b)| format!("({a} {op} {b})")),
            inner.clone().prop_map(|a| format!("abs({a})")),
            inner.clone().prop_map(|a| format!("(-{a})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("coalesce({a}, {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("nullif({a}, {b})")),
            text_expr_leaf().prop_map(|t| format!("length({t})")),
            inner.clone().prop_map(|a| format!("CAST({a} AS smallint)")),
            num_expr().prop_map(|a| format!("CAST({a} AS integer)")),
        ]
    })
    .boxed()
}

fn num_expr() -> BoxedStrategy<String> {
    let leaf = prop_oneof![
        Just("n".to_string()),
        Just("f".to_string()),
        Just("1.5".to_string()),
        Just("0.0".to_string()),
        Just("1e308".to_string()),
        Just("i4".to_string()),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            (
                inner.clone(),
                prop_oneof![Just("+"), Just("-"), Just("*"), Just("/")],
                inner.clone()
            )
                .prop_map(|(a, op, b)| format!("({a} {op} {b})")),
            inner.clone().prop_map(|a| format!("round({a})")),
            (inner.clone(), 0i32..5).prop_map(|(a, d)| format!("round({a}, {d})")),
            inner.prop_map(|a| format!("abs({a})")),
        ]
    })
    .boxed()
}

fn text_expr_leaf() -> BoxedStrategy<String> {
    prop_oneof![
        Just("s".to_string()),
        Just("'hello'".to_string()),
        Just("''".to_string()),
        Just("'%a_'".to_string()),
    ]
    .boxed()
}

fn text_expr() -> BoxedStrategy<String> {
    text_expr_leaf()
        .prop_recursive(3, 12, 2, |inner| {
            prop_oneof![
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} || {b})")),
                inner.clone().prop_map(|a| format!("upper({a})")),
                inner.clone().prop_map(|a| format!("lower({a})")),
                (inner.clone(), -3i32..6, 0i32..5)
                    .prop_map(|(a, f, l)| format!("substring({a}, {f}, {l})")),
                inner.clone().prop_map(|a| format!("trim({a})")),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("coalesce({a}, {b})")),
                int_expr().prop_map(|a| format!("CAST({a} AS text)")),
            ]
        })
        .boxed()
}

fn bool_expr() -> BoxedStrategy<String> {
    let leaf = prop_oneof![
        (
            int_expr(),
            prop_oneof![
                Just("="),
                Just("<>"),
                Just("<"),
                Just("<="),
                Just(">"),
                Just(">=")
            ],
            int_expr()
        )
            .prop_map(|(a, op, b)| format!("({a} {op} {b})")),
        (text_expr(), text_expr()).prop_map(|(a, b)| format!("({a} LIKE {b})")),
        int_expr().prop_map(|a| format!("({a} IS NULL)")),
        (int_expr(), int_expr(), int_expr())
            .prop_map(|(a, l, h)| format!("({a} BETWEEN {l} AND {h})")),
        (int_expr(), int_expr(), int_expr()).prop_map(|(a, x, y)| format!("({a} IN ({x}, {y}))")),
        Just("b".to_string()),
        Just("(d < '2020-01-01')".to_string()),
        Just("(ts >= '2000-01-01 00:00:00+00')".to_string()),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} AND {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} OR {b})")),
            inner.prop_map(|a| format!("(NOT {a})")),
        ]
    })
    .boxed()
}

fn statement() -> BoxedStrategy<String> {
    prop_oneof![
        (int_expr(), text_expr(), num_expr(), bool_expr(), int_expr(), 0usize..4, 0usize..3).prop_map(
            |(i, t, n, w, o, l, off)| format!(
                "SELECT {i}, {t}, {n} FROM t WHERE {w} ORDER BY {o} DESC NULLS FIRST, id LIMIT {l} OFFSET {off}"
            )
        ),
        (int_expr(), bool_expr()).prop_map(|(e, w)| format!("UPDATE t SET i4 = {e} WHERE {w}")),
        (int_expr(), bool_expr()).prop_map(|(e, w)| format!("UPDATE t SET i2 = {e}, u = {e} WHERE {w} RETURNING id, i2")),
        (10i32..20, int_expr(), text_expr()).prop_map(|(id, e, t)| format!("INSERT INTO t (id, i4, s) VALUES ({id}, {e}, {t}) RETURNING id")),
        bool_expr().prop_map(|w| format!("DELETE FROM t WHERE {w} RETURNING id")),
        text_expr().prop_map(|t| format!("SELECT id FROM t WHERE s = {t}")),
    ]
    .boxed()
}

fn dump(db: &mut Db, s: &mut ExecSession) -> Vec<Vec<String>> {
    common::text(
        &s.execute(
            db,
            "SELECT id, i2, i4, i8, n, f, s, b, d, ts, u FROM t ORDER BY id",
        )
        .unwrap(),
    )
}

fn outcome(r: &Result<ExecResult, lattice_exec::ExecError>) -> String {
    match r {
        Ok(r) => format!("ok {} {:?}", r.tag, common::text(r)),
        Err(e) => format!("err {}", e.sqlstate()),
    }
}

fn run_all(statements: &[String]) -> Vec<String> {
    let f = fixture();
    let (mut db, mut s) = (f.db, ExecSession::allow_all());
    for sql in SETUP {
        s.execute(&mut db, sql).unwrap();
    }
    let mut log = Vec::new();
    for sql in statements {
        let before = dump(&mut db, &mut s);
        let r = s.execute(&mut db, sql);
        match &r {
            Err(e) => {
                assert_eq!(e.sqlstate().len(), 5, "{sql}: {e}");
                assert_eq!(
                    dump(&mut db, &mut s),
                    before,
                    "{sql} failed ({e}) but changed the table"
                );
            }
            Ok(res) => {
                for row in &res.rows {
                    assert_eq!(row.len(), res.columns.len(), "{sql}");
                }
                if res.tag.starts_with("SELECT") {
                    assert_eq!(res.tag, format!("SELECT {}", res.rows.len()), "{sql}");
                }
            }
        }
        log.push(outcome(&r));
    }
    log
}

proptest! {
    #[test]
    fn bound_statements_never_panic_and_failures_change_nothing(stmts in prop::collection::vec(statement(), 1..8)) {
        let first = run_all(&stmts);
        // Same statements, fresh database: same outcomes, byte for byte.
        let second = run_all(&stmts);
        prop_assert_eq!(first, second);
    }
}

#[test]
fn the_generator_reaches_the_executor_and_its_error_paths() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = statement();
    let f = fixture();
    let (mut db, mut s) = (f.db, ExecSession::allow_all());
    for sql in SETUP {
        s.execute(&mut db, sql).unwrap();
    }
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for _ in 0..400 {
        let sql = strat.new_tree(&mut runner).unwrap().current();
        let key = match s.execute(&mut db, &sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.sqlstate().to_string(),
        };
        *counts.entry(key).or_default() += 1;
    }
    let get = |k: &str| counts.get(k).copied().unwrap_or(0);
    assert!(get("ok") >= 100, "too few statements ran: {counts:?}");
    for code in ["22003", "22012"] {
        assert!(get(code) >= 1, "no {code} reached: {counts:?}");
    }
    let bind_failures: usize = counts
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "42804" | "42703" | "42883" | "0A000" | "42601"))
        .map(|(_, v)| v)
        .sum();
    assert!(
        bind_failures * 2 < 400,
        "most statements fail to bind: {counts:?}"
    );
}

#[test]
fn garbage_text_is_refused_not_executed() {
    let f = fixture();
    let (mut db, mut s) = (f.db, ExecSession::allow_all());
    for sql in SETUP {
        s.execute(&mut db, sql).unwrap();
    }
    for sql in [
        "",
        ";",
        "SELECT",
        "SELECT * FROM",
        "SELECT '",
        "UPDATE t SET",
        "SELECT ((((((((((1))))))))))) FROM t",
        "\u{0}",
        "SELECT * FROM t WHERE id = $1 AND id = $2",
    ] {
        match s.execute(&mut db, sql) {
            Err(e) => assert_eq!(e.sqlstate().len(), 5, "{sql:?}"),
            Ok(r) => assert!(
                r.rows.iter().all(|row| row.len() == r.columns.len()),
                "{sql:?}"
            ),
        }
    }
}
