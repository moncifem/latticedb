//! The sqllogictest scripts under `lattice-testkit/tests/slt/` as parse inputs. Every statement
//! and query in them must parse (and print back to the same tree); the records that expect
//! `0A000` or `42601` must be refused with exactly that class of error.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sql::{ErrorKind, parse_statement};

const SCRIPTS: &[(&str, &str)] = &[
    (
        "basics.test",
        include_str!("../../lattice-testkit/tests/slt/basics.test"),
    ),
    (
        "joins.test",
        include_str!("../../lattice-testkit/tests/slt/joins.test"),
    ),
    (
        "aggregates.test",
        include_str!("../../lattice-testkit/tests/slt/aggregates.test"),
    ),
    (
        "changes.test",
        include_str!("../../lattice-testkit/tests/slt/changes.test"),
    ),
    (
        "transactions.test",
        include_str!("../../lattice-testkit/tests/slt/transactions.test"),
    ),
    (
        "nulls_logic.test",
        include_str!("../../lattice-testkit/tests/slt/nulls_logic.test"),
    ),
    (
        "nulls_functions.test",
        include_str!("../../lattice-testkit/tests/slt/nulls_functions.test"),
    ),
    (
        "arithmetic_errors.test",
        include_str!("../../lattice-testkit/tests/slt/arithmetic_errors.test"),
    ),
    (
        "strings.test",
        include_str!("../../lattice-testkit/tests/slt/strings.test"),
    ),
    (
        "order_nulls.test",
        include_str!("../../lattice-testkit/tests/slt/order_nulls.test"),
    ),
    (
        "limit_offset.test",
        include_str!("../../lattice-testkit/tests/slt/limit_offset.test"),
    ),
    (
        "empty_aggregates.test",
        include_str!("../../lattice-testkit/tests/slt/empty_aggregates.test"),
    ),
    (
        "joins_small.test",
        include_str!("../../lattice-testkit/tests/slt/joins_small.test"),
    ),
    (
        "returning.test",
        include_str!("../../lattice-testkit/tests/slt/returning.test"),
    ),
    (
        "transactions_visibility.test",
        include_str!("../../lattice-testkit/tests/slt/transactions_visibility.test"),
    ),
    (
        "constraints.test",
        include_str!("../../lattice-testkit/tests/slt/constraints.test"),
    ),
    (
        "ddl_statements.test",
        include_str!("../../lattice-testkit/tests/slt/ddl_statements.test"),
    ),
    (
        "prepared_forms.test",
        include_str!("../../lattice-testkit/tests/slt/prepared_forms.test"),
    ),
    (
        "subqueries.test",
        include_str!("../../lattice-testkit/tests/slt/subqueries.test"),
    ),
    (
        "unsupported.test",
        include_str!("../../lattice-testkit/tests/slt/unsupported.test"),
    ),
    (
        "types_and_casts.test",
        include_str!("../../lattice-testkit/tests/slt/types_and_casts.test"),
    ),
];

struct Record {
    file: &'static str,
    /// `ok`, `query`, or the SQLSTATE of `statement error <code>`.
    expect: String,
    sql: String,
}

/// Splits a script into records: a header line, the SQL up to `----` or a blank line.
fn records() -> Vec<Record> {
    let mut out = Vec::new();
    for (file, text) in SCRIPTS {
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let expect = match words.as_slice() {
                ["statement", "ok"] => "ok".to_string(),
                ["statement", "error", code] => (*code).to_string(),
                ["query", ..] => "query".to_string(),
                _ => continue,
            };
            let mut sql = Vec::new();
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() || next.trim() == "----" {
                    break;
                }
                sql.push(lines.next().unwrap());
            }
            out.push(Record {
                file,
                expect,
                sql: sql.join("\n"),
            });
        }
    }
    out
}

#[test]
fn every_script_record_is_read() {
    // 21 scripts: counted from the `statement`/`query` header lines of the files.
    let n = records().len();
    let headers: usize = SCRIPTS
        .iter()
        .map(|(_, t)| {
            t.lines()
                .filter(|l| l.starts_with("statement ") || l.starts_with("query "))
                .count()
        })
        .sum();
    assert_eq!(n, headers);
    assert!(n >= 600, "expected the declared scripts, found {n} records");
}

#[test]
fn accepted_records_parse_and_print_back_to_the_same_tree() {
    for r in records() {
        if r.expect == "0A000" || r.expect == "42601" {
            continue;
        }
        let stmt = match parse_statement(&r.sql) {
            Ok(s) => s,
            Err(e) => panic!("{}: `{}` should parse: {e}", r.file, r.sql),
        };
        let printed = stmt.to_string();
        assert_eq!(
            parse_statement(&printed).as_ref(),
            Ok(&stmt),
            "{}: `{}` printed as `{printed}`",
            r.file,
            r.sql
        );
    }
}

#[test]
fn refused_records_fail_with_the_declared_class() {
    let mut unsupported = 0;
    let mut syntax = 0;
    for r in records() {
        let want = match r.expect.as_str() {
            "0A000" => ErrorKind::Unsupported,
            "42601" => ErrorKind::Syntax,
            _ => continue,
        };
        let e = parse_statement(&r.sql).unwrap_err();
        assert_eq!(e.kind(), want, "{}: `{}`: {e}", r.file, r.sql);
        assert_eq!(e.sqlstate(), r.expect);
        match want {
            ErrorKind::Unsupported => unsupported += 1,
            _ => syntax += 1,
        }
    }
    assert_eq!(unsupported, 125);
    assert_eq!(syntax, 18);
}

#[test]
fn the_declared_rejections_name_their_feature() {
    let named = [
        (
            "SELECT * FROM teams t RIGHT JOIN members m ON m.team_id = t.id",
            "RIGHT JOIN",
        ),
        ("SELECT * FROM teams NATURAL JOIN members", "NATURAL JOIN"),
        (
            "SELECT region, sum(amount) OVER (PARTITION BY region) FROM sales",
            "window functions",
        ),
        (
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "WITH (common table expressions)",
        ),
        (
            "INSERT INTO items VALUES (1, 'x', 0, 'Y') ON CONFLICT DO NOTHING",
            "INSERT ... ON CONFLICT",
        ),
        ("SAVEPOINT s1", "SAVEPOINT"),
    ];
    for (sql, feature) in named {
        assert!(
            records().iter().any(|r| r.sql == sql),
            "`{sql}` is not in the scripts"
        );
        assert_eq!(
            parse_statement(sql).unwrap_err().message(),
            feature,
            "{sql}"
        );
    }
}
