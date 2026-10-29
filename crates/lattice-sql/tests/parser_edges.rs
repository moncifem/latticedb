//! Parser edges found by mutation testing: the exact nesting limits, the numeric and varchar
//! bounds, refusals that sit behind a guard, and the span of a postfix cast.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sql::ast::{SelectItem, Statement};
use lattice_sql::{ErrorKind, MAX_DEPTH, parse_statement};

fn refused(sql: &str, kind: ErrorKind, message: &str) {
    let e = parse_statement(sql).unwrap_err();
    assert_eq!(e.kind(), kind, "`{sql}`: {e}");
    assert_eq!(e.message(), message, "`{sql}`");
}

fn unsupported(sql: &str, message: &str) {
    refused(sql, ErrorKind::Unsupported, message);
}

#[test]
fn the_nesting_limit_is_exact_for_every_shape() {
    // Each shape is accepted one level below its limit and refused at it. Subqueries count twice
    // (the query and the expression that holds it), so their limit is half.
    let flat: Vec<(&str, Box<dyn Fn(usize) -> String>)> = vec![
        (
            "parentheses",
            Box::new(|n| format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n))),
        ),
        (
            "unary minus",
            Box::new(|n| format!("SELECT {}1", "- ".repeat(n))),
        ),
        ("not", Box::new(|n| format!("SELECT {}a", "NOT ".repeat(n)))),
        (
            "right-nested sum",
            Box::new(|n| format!("SELECT {}1", "1 + ".repeat(n))),
        ),
        (
            "left-nested sum",
            Box::new(|n| format!("SELECT a {}", "+ 1 ".repeat(n))),
        ),
        (
            "function calls",
            Box::new(|n| format!("SELECT {}1{}", "coalesce(".repeat(n), ")".repeat(n))),
        ),
        (
            "casts",
            Box::new(|n| format!("SELECT 1 {}", "::int".repeat(n))),
        ),
        (
            "in lists",
            Box::new(|n| format!("SELECT x {}", "IN (1) ".repeat(n))),
        ),
    ];
    for (name, make) in &flat {
        assert!(
            parse_statement(&make(MAX_DEPTH - 2)).is_ok(),
            "{name} at {}",
            MAX_DEPTH - 2
        );
        let e = parse_statement(&make(MAX_DEPTH - 1)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge, "{name}: {e}");
    }
    let subqueries: Vec<(&str, Box<dyn Fn(usize) -> String>)> = vec![
        (
            "scalar subqueries",
            Box::new(|n| format!("SELECT {}1{}", "(SELECT ".repeat(n), ")".repeat(n))),
        ),
        (
            "IN subqueries",
            Box::new(|n| {
                format!(
                    "SELECT a FROM t WHERE {}1{}",
                    "a IN (SELECT ".repeat(n),
                    ")".repeat(n)
                )
            }),
        ),
    ];
    for (name, make) in &subqueries {
        let half = MAX_DEPTH / 2;
        assert!(
            parse_statement(&make(half - 1)).is_ok(),
            "{name} at {}",
            half - 1
        );
        let e = parse_statement(&make(half)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge, "{name}: {e}");
    }
}

#[test]
fn numeric_scale_may_equal_but_not_exceed_precision() {
    assert!(parse_statement("SELECT CAST(a AS numeric(5,5)) FROM t").is_ok());
    assert!(parse_statement("SELECT CAST(a AS numeric(38,0)) FROM t").is_ok());
    refused(
        "SELECT CAST(a AS numeric(5,6)) FROM t",
        ErrorKind::Syntax,
        "numeric scale must not exceed its precision",
    );
    unsupported(
        "SELECT CAST(a AS numeric(39)) FROM t",
        "numeric precision above 38",
    );
}

#[test]
fn varchar_length_is_between_one_and_ten_million_and_a_bit() {
    for ok in ["1", "10485760"] {
        assert!(parse_statement(&format!("SELECT CAST(a AS varchar({ok})) FROM t")).is_ok());
    }
    for bad in ["0", "10485761"] {
        refused(
            &format!("SELECT CAST(a AS varchar({bad})) FROM t"),
            ErrorKind::Syntax,
            "varchar length must be between 1 and 10485760",
        );
    }
    assert!(parse_statement("SELECT CAST(a AS character varying(10)) FROM t").is_ok());
    unsupported(
        "SELECT CAST(a AS character) FROM t",
        "data type `character`",
    );
}

#[test]
fn constraints_and_clauses_outside_the_subset_are_named() {
    unsupported(
        "CREATE TABLE t (a int CONSTRAINT c PRIMARY KEY)",
        "named constraints",
    );
    unsupported("CREATE TABLE t (a text COLLATE \"C\")", "COLLATE");
    unsupported("CREATE TABLE t (a int, CHECK (a > 0))", "CHECK constraints");
    unsupported(
        "SELECT a FROM t WINDOW w AS (PARTITION BY a)",
        "window definitions",
    );
    unsupported("SELECT a FROM t FETCH FIRST 1 ROWS ONLY", "FETCH FIRST");
    for sql in [
        "BEGIN READ ONLY",
        "BEGIN DEFERRABLE",
        "BEGIN NOT DEFERRABLE",
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
    ] {
        unsupported(sql, "transaction access modes");
    }
    refused(
        "CREATE UNIQUE TABLE t (a int)",
        ErrorKind::Syntax,
        "expected INDEX, found `table`",
    );
    refused(
        "CREATE UNIQUE VIEW v AS SELECT 1",
        ErrorKind::Syntax,
        "expected INDEX, found `view`",
    );
    unsupported("CREATE VIEW v AS SELECT 1", "CREATE VIEW");
}

#[test]
fn words_that_look_like_calls_are_only_calls_before_a_parenthesis() {
    for word in ["position", "trim", "extract", "interval", "date"] {
        assert!(
            parse_statement(&format!("SELECT {word} FROM t")).is_ok(),
            "`{word}` without a parenthesis is a column"
        );
    }
    for word in ["cast", "where"] {
        refused(
            &format!("SELECT {word} FROM t"),
            ErrorKind::Syntax,
            &format!("expected an expression, found `{word}`"),
        );
    }
    unsupported("SELECT lateral FROM t", "LATERAL");
    unsupported("SELECT a SIMILAR TO 'x' FROM t", "SIMILAR TO");
    unsupported("SELECT a NOT SIMILAR TO 'x' FROM t", "SIMILAR TO");
    unsupported("SELECT a AT TIME ZONE 'UTC' FROM t", "AT TIME ZONE");
    assert_eq!(
        parse_statement("SELECT a AT x FROM t").unwrap_err().kind(),
        ErrorKind::Syntax
    );
    refused(
        "SET x = -y",
        ErrorKind::Syntax,
        "expected a value, found `-`",
    );
    assert!(parse_statement("SET x = -1").is_ok());
}

#[test]
fn a_postfix_cast_spans_its_operand_and_its_type() {
    let sql = "SELECT  a::int  FROM t";
    let Statement::Query(q) = parse_statement(sql).unwrap() else {
        panic!("query");
    };
    let Some(SelectItem::Expr { expr, .. }) = q.projection.first() else {
        panic!("projection");
    };
    assert_eq!(expr.span.slice(sql), Some("a::int"));
}
