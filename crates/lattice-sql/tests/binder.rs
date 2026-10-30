//! Binder tests: hand-written plan goldens, an error-code table, the sqllogictest scripts as bind
//! inputs, parameter typing, and a property test that binding never panics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::collections::BTreeMap;

use lattice_sql::ast::{self, Statement};
use lattice_sql::binder::{BindError, BindErrorKind, bind, bind_statement};
use lattice_sql::parse_statement;
use lattice_sql::plan::{
    CatalogView, ColumnInfo, ExprNode, IndexInfo, InsertSource, LogicalPlan, SqlType, TableInfo,
    TypedExpr,
};
use proptest::prelude::*;

/// Version the test catalog reports.
const CATALOG_VERSION: u64 = 41;

struct Catalog(BTreeMap<String, TableInfo>);

fn col(name: &str, ty: SqlType, not_null: bool, has_default: bool) -> ColumnInfo {
    ColumnInfo {
        name: name.to_string(),
        ty,
        not_null,
        has_default,
    }
}

fn table(name: &str, columns: Vec<ColumnInfo>, indexes: Vec<IndexInfo>) -> (String, TableInfo) {
    (
        name.to_string(),
        TableInfo {
            // Distinct, nonzero ids derived from the name's length and first byte.
            id: 1000 + name.len() as u64 * 10 + u64::from(name.as_bytes()[0] % 10),
            name: name.to_string(),
            columns,
            indexes,
        },
    )
}

/// The tables the sqllogictest scripts create, written out by hand from their `CREATE TABLE`s.
fn catalog() -> Catalog {
    use SqlType::{Date, Integer, Jsonb, Text, Timestamptz};
    let pk = |t: &str| IndexInfo {
        name: format!("{t}_pkey"),
        columns: vec!["id".to_string()],
        unique: true,
    };
    Catalog(BTreeMap::from([
        table(
            "notes",
            vec![
                col("id", Integer, true, false),
                col("title", Text, false, false),
                col("score", Integer, false, false),
            ],
            vec![pk("notes")],
        ),
        table(
            "sales",
            vec![
                col("id", Integer, true, false),
                col("region", Text, true, false),
                col("amount", Integer, false, false),
                col("rep", Text, false, false),
            ],
            vec![pk("sales")],
        ),
        table(
            "teams",
            vec![
                col("id", Integer, true, false),
                col("name", Text, true, false),
            ],
            vec![pk("teams")],
        ),
        table(
            "members",
            vec![
                col("id", Integer, true, false),
                col("team_id", Integer, false, false),
                col("name", Text, true, false),
            ],
            vec![pk("members")],
        ),
        table(
            "items",
            vec![
                col("id", Integer, true, false),
                col("label", Text, true, false),
                col("qty", Integer, false, true),
                col("sku", Text, false, false),
            ],
            vec![pk("items")],
        ),
        table(
            "acct",
            vec![
                col("id", Integer, true, false),
                col("bal", Integer, true, false),
            ],
            vec![pk("acct")],
        ),
        table(
            "events",
            vec![
                col("id", SqlType::BigInt, true, false),
                col("ts", Timestamptz, false, false),
                col("due", Date, false, false),
                col("body", Jsonb, false, false),
                col("flag", SqlType::Boolean, false, false),
                col("ratio", SqlType::DoublePrecision, false, false),
                col("small", SqlType::SmallInt, false, false),
            ],
            vec![pk("events")],
        ),
    ]))
}

impl CatalogView for Catalog {
    fn table(&self, name: &str) -> Option<TableInfo> {
        self.0.get(name).cloned()
    }

    fn version(&self) -> u64 {
        CATALOG_VERSION
    }
}

fn plan_of(sql: &str) -> LogicalPlan {
    let stmt = parse_statement(sql).unwrap_or_else(|e| panic!("`{sql}` should parse: {e}"));
    bind(&stmt, &catalog()).unwrap_or_else(|e| panic!("`{sql}` should bind: {e}"))
}

fn bind_err(sql: &str) -> BindError {
    let stmt = parse_statement(sql).unwrap_or_else(|e| panic!("`{sql}` should parse: {e}"));
    match bind(&stmt, &catalog()) {
        Ok(p) => panic!("`{sql}` should not bind, got\n{p}"),
        Err(e) => e,
    }
}

fn output_types(plan: &LogicalPlan) -> Vec<SqlType> {
    plan.output()
        .expect("plan has output")
        .iter()
        .map(|c| c.expr.ty)
        .collect()
}

#[test]
fn plans_print_as_the_reviewed_goldens() {
    let cases: &[(&str, &str)] = &[
        (
            "SELECT id, title, score FROM notes WHERE score >= 20",
            "Project [id, title, score]\n  Filter (score >= 20)\n    Scan notes\n",
        ),
        ("SELECT * FROM acct", "Project [id, bal]\n  Scan acct\n"),
        (
            "SELECT title FROM notes ORDER BY id DESC LIMIT 1",
            "Limit 1\n  Project [title]\n    Sort [id DESC NULLS FIRST]\n      Scan notes\n",
        ),
        (
            "SELECT id, region FROM sales ORDER BY amount DESC NULLS LAST, id LIMIT 2",
            "Limit 2\n  Project [id, region]\n    Sort [amount DESC NULLS LAST, id ASC NULLS LAST]\n      Scan sales\n",
        ),
        (
            "SELECT id FROM sales ORDER BY id LIMIT 2 OFFSET 3",
            "Limit 2 OFFSET 3\n  Project [id]\n    Sort [id ASC NULLS LAST]\n      Scan sales\n",
        ),
        (
            "SELECT id FROM sales WHERE amount BETWEEN 10 AND 20 ORDER BY id",
            "Project [id]\n  Sort [id ASC NULLS LAST]\n    Filter (amount BETWEEN 10 AND 20)\n      Scan sales\n",
        ),
        (
            "SELECT id FROM sales WHERE region IN ('east', 'south') AND amount IS NOT NULL ORDER BY id",
            "Project [id]\n  Sort [id ASC NULLS LAST]\n    Filter ((region IN ('east', 'south')) AND (amount IS NOT NULL))\n      Scan sales\n",
        ),
        (
            "SELECT id FROM sales WHERE rep LIKE 'a%' OR NOT (amount > 5)",
            "Project [id]\n  Filter ((rep LIKE 'a%') OR (NOT (amount > 5)))\n    Scan sales\n",
        ),
        (
            "SELECT 1 + 2 * 3 FROM notes",
            "Project [7 AS ?column?]\n  Scan notes\n",
        ),
        (
            "SELECT n.id AS k, upper(title) FROM notes n WHERE n.id = 1",
            "Project [id AS k, upper(title) AS upper]\n  Filter (id = 1)\n    Scan notes\n",
        ),
        (
            "SELECT id AS k FROM notes ORDER BY k",
            "Project [id AS k]\n  Sort [id ASC NULLS LAST]\n    Scan notes\n",
        ),
        (
            "SELECT title, id FROM notes ORDER BY 2",
            "Project [title, id]\n  Sort [id ASC NULLS LAST]\n    Scan notes\n",
        ),
        (
            "SELECT id FROM notes AS OF CHANGESET 'c1'",
            "Project [id]\n  Scan notes AS OF CHANGESET 'c1'\n",
        ),
        (
            "SELECT id FROM notes AS OF TIMESTAMP '2026-01-01T00:00:00Z'",
            "Project [id]\n  Scan notes AS OF TIMESTAMP '2026-01-01T00:00:00Z'\n",
        ),
        (
            "INSERT INTO items (id, label, sku) VALUES (1, 'bolt', 'B1') RETURNING qty",
            "Insert items (#0, #1, #3) RETURNING [qty]\n  Values (1, 'bolt', 'B1')\n",
        ),
        (
            "INSERT INTO items VALUES (2, 'nut', 5, 'N1'), (3, 'it''s', NULL, NULL)",
            "Insert items (#0, #1, #2, #3)\n  Values (2, 'nut', 5, 'N1')\n  Values (3, 'it''s', NULL, NULL)\n",
        ),
        (
            "INSERT INTO items SELECT id + 10, label, qty, NULL FROM items",
            "Insert items (#0, #1, #2, #3)\n  Project [(id + 10) AS ?column?, label, qty, NULL AS ?column?]\n    Scan items\n",
        ),
        (
            "UPDATE items SET qty = qty + 1 WHERE id < 10 RETURNING id, label",
            "Update items SET #2 = (qty + 1) RETURNING [id, label]\n  Filter (id < 10)\n    Scan items\n",
        ),
        (
            "DELETE FROM items WHERE sku IS NULL RETURNING id",
            "Delete items RETURNING [id]\n  Filter (sku IS NULL)\n    Scan items\n",
        ),
        ("DELETE FROM acct", "Delete acct\n  Scan acct\n"),
        (
            "SELECT CAST(score AS double precision) * 2, coalesce(title, 'none') FROM notes",
            "Project [(CAST(score AS double precision) * 2) AS ?column?, coalesce(title, 'none') AS coalesce]\n  Scan notes\n",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(plan_of(sql).to_string(), *want, "plan of `{sql}`");
    }
}

#[test]
fn bind_errors_carry_the_postgres_sqlstate_and_the_lattice_code() {
    // (sql, SQLSTATE, Lattice code)
    let cases: &[(&str, &str, Option<&str>)] = &[
        // undefined table, column, qualifier
        ("SELECT * FROM missing_table", "42P01", Some("LSQL-0003")),
        ("UPDATE nowhere SET a = 1", "42P01", Some("LSQL-0003")),
        ("DELETE FROM nowhere", "42P01", Some("LSQL-0003")),
        ("INSERT INTO nowhere VALUES (1)", "42P01", Some("LSQL-0003")),
        ("SELECT x.id FROM notes", "42P01", Some("LSQL-0003")),
        ("SELECT notes.id FROM notes n", "42P01", Some("LSQL-0003")),
        ("SELECT z.* FROM notes", "42P01", Some("LSQL-0003")),
        ("SELECT missing FROM items", "42703", Some("LSQL-0003")),
        (
            "SELECT id FROM notes WHERE nope = 1",
            "42703",
            Some("LSQL-0003"),
        ),
        ("UPDATE notes SET nope = 1", "42703", Some("LSQL-0003")),
        (
            "INSERT INTO notes (nope) VALUES (1)",
            "42703",
            Some("LSQL-0003"),
        ),
        (
            "SELECT id FROM notes ORDER BY nope",
            "42703",
            Some("LSQL-0003"),
        ),
        ("SELECT id FROM s.notes", "3F000", None),
        // datatype mismatch
        ("SELECT id FROM notes WHERE title = 1", "42804", None),
        ("SELECT id FROM notes WHERE id", "42804", None),
        ("SELECT id FROM notes WHERE NOT title", "42804", None),
        ("SELECT id FROM notes WHERE title AND true", "42804", None),
        ("SELECT title + 1 FROM notes", "42804", None),
        ("SELECT title || 1 FROM notes", "42804", None),
        ("SELECT id FROM notes WHERE id LIKE 'a'", "42804", None),
        (
            "SELECT id FROM notes WHERE id IN (1, 'x', title)",
            "42804",
            None,
        ),
        ("SELECT id FROM events WHERE flag = ts", "42804", None),
        ("SELECT -title FROM notes", "42804", None),
        ("SELECT lower(1) FROM notes", "42804", None),
        ("SELECT abs(title) FROM notes", "42804", None),
        ("UPDATE notes SET title = 5", "42804", None),
        ("INSERT INTO notes VALUES (1, 2, 3)", "42804", None),
        (
            "INSERT INTO notes SELECT title, title, score FROM notes",
            "42804",
            None,
        ),
        ("SELECT id FROM notes LIMIT 'x'", "22P02", None),
        ("SELECT id FROM notes LIMIT true", "42804", None),
        ("SELECT id FROM events AS OF TIMESTAMP 5", "42804", None),
        // numeric range and arithmetic
        ("SELECT 2147483647 + 1 FROM notes", "22003", None),
        ("SELECT 9223372036854775807 + 1 FROM notes", "22003", None),
        ("SELECT 3000000000 * 4000000000 FROM notes", "22003", None),
        (
            "INSERT INTO notes VALUES (99999999999, 'x', 1)",
            "22003",
            None,
        ),
        (
            "INSERT INTO events (id, small) VALUES (1, 70000)",
            "22003",
            None,
        ),
        ("UPDATE notes SET score = 3000000000", "22003", None),
        ("UPDATE notes SET score = '3000000000'", "22003", None),
        ("SELECT 1 / 0 FROM notes", "22012", None),
        ("SELECT 5 % 0 FROM notes", "22012", None),
        // text that is not a value of the type
        ("UPDATE notes SET score = 'x'", "22P02", None),
        ("SELECT id FROM notes WHERE score = 'x'", "22P02", None),
        ("SELECT id FROM events WHERE flag = 'maybe'", "22P02", None),
        ("SELECT id FROM events WHERE ratio = '1.2.3'", "22P02", None),
        // constraints decided at bind time
        (
            "INSERT INTO items (id, qty) VALUES (4, 1)",
            "23502",
            Some("LSQL-0004"),
        ),
        (
            "INSERT INTO sales (id) VALUES (1)",
            "23502",
            Some("LSQL-0004"),
        ),
        (
            "INSERT INTO sales VALUES (1, NULL, 1, 'a')",
            "23502",
            Some("LSQL-0004"),
        ),
        ("UPDATE acct SET bal = NULL", "23502", Some("LSQL-0004")),
        // shape of INSERT, UPDATE, LIMIT
        (
            "INSERT INTO notes VALUES (1, 'a', 2, 3)",
            "42601",
            Some("LSQL-0001"),
        ),
        (
            "INSERT INTO notes (id, title) VALUES (1)",
            "42601",
            Some("LSQL-0001"),
        ),
        (
            "INSERT INTO notes SELECT id, title, score, id FROM notes",
            "42601",
            Some("LSQL-0001"),
        ),
        (
            "INSERT INTO notes (id) SELECT id, title FROM notes",
            "42601",
            Some("LSQL-0001"),
        ),
        ("INSERT INTO notes (id, id) VALUES (1, 2)", "42701", None),
        (
            "UPDATE notes SET score = 1, score = 2",
            "42601",
            Some("LSQL-0001"),
        ),
        ("SELECT id FROM notes LIMIT -1", "2201W", None),
        ("SELECT id FROM notes LIMIT 1 OFFSET -2", "2201X", None),
        ("SELECT id FROM notes ORDER BY 9", "42P10", None),
        (
            "SELECT id AS x, score AS x FROM notes ORDER BY x",
            "42702",
            None,
        ),
        // type modifiers are not carried by CAST, so they are refused
        (
            "SELECT CAST(score AS numeric(5, 2)) FROM notes",
            "0A000",
            Some("LSQL-0002"),
        ),
        (
            "SELECT title::varchar(3) FROM notes",
            "0A000",
            Some("LSQL-0002"),
        ),
        (
            "SELECT CAST(score AS numeric(5)) FROM notes",
            "0A000",
            Some("LSQL-0002"),
        ),
        // a typed parameter reused with another type
        (
            "SELECT id FROM notes WHERE score = $1 AND title = $1",
            "42P08",
            None,
        ),
        (
            "SELECT id FROM notes WHERE score = $1 AND title = $2 AND $1 = $2",
            "42P08",
            None,
        ),
        (
            "SELECT id FROM notes WHERE score = $1 AND title = $2 AND coalesce($1, $2) IS NULL",
            "42P08",
            None,
        ),
        (
            "SELECT id FROM notes WHERE title = $1 AND $1",
            "42P08",
            None,
        ),
        ("SELECT id FROM notes ORDER BY 0", "42P10", None),
        // casts and functions
        ("SELECT CAST(id AS uuid) FROM notes", "42846", None),
        ("SELECT lower() FROM notes", "42883", None),
        ("SELECT now(1) FROM notes", "42883", None),
        (
            "SELECT date_trunc('fortnight', ts) FROM events",
            "22023",
            None,
        ),
        ("SELECT extract(century FROM ts) FROM events", "22023", None),
        // outside what the binder handles yet
        ("SELECT count(*) FROM notes", "0A000", Some("LSQL-0002")),
        ("SELECT sum(score) FROM notes", "0A000", Some("LSQL-0002")),
        (
            "SELECT region FROM sales GROUP BY region",
            "0A000",
            Some("LSQL-0002"),
        ),
        ("SELECT DISTINCT rep FROM sales", "0A000", Some("LSQL-0002")),
        (
            "SELECT t.name FROM teams t INNER JOIN members m ON m.team_id = t.id",
            "0A000",
            Some("LSQL-0002"),
        ),
        ("SELECT * FROM teams, members", "0A000", Some("LSQL-0002")),
        (
            "SELECT id FROM notes WHERE id IN (SELECT id FROM acct)",
            "0A000",
            Some("LSQL-0002"),
        ),
        ("SELECT (SELECT 1) FROM notes", "0A000", Some("LSQL-0002")),
        ("SELECT a FROM notes, tags", "0A000", Some("LSQL-0002")),
        ("CREATE TABLE t (a INT)", "0A000", Some("LSQL-0002")),
        ("BEGIN", "0A000", Some("LSQL-0002")),
        ("SELECT a.b.c FROM notes", "0A000", Some("LSQL-0002")),
    ];
    for (sql, state, code) in cases {
        let e = bind_err(sql);
        assert_eq!(e.sqlstate(), *state, "`{sql}`: {e}");
        assert_eq!(e.lattice_code(), *code, "`{sql}`: {e}");
    }
}

#[test]
fn parameter_errors_have_their_own_sqlstates() {
    let stmt = parse_statement("SELECT $2 FROM notes").unwrap();
    let e = bind_statement(&stmt, &catalog()).unwrap_err();
    assert_eq!(e.kind(), BindErrorKind::IndeterminateParam);
    assert_eq!(e.sqlstate(), "42P18");
    let stmt = parse_statement("SELECT id FROM notes WHERE score = $1 AND title = $1").unwrap();
    let e = bind_statement(&stmt, &catalog()).unwrap_err();
    assert_eq!(e.kind(), BindErrorKind::InconsistentParamTypes);
    assert_eq!(e.sqlstate(), "42P08");
    // The parser refuses `$0`; a tree built by hand reaches the binder with one.
    let stmt = Statement::Delete(ast::Delete {
        table: ast::ObjectName(vec![ast::Ident::new("notes")]),
        alias: None,
        selection: Some(ast::Expr::new(ast::ExprKind::Param(0))),
        returning: Vec::new(),
    });
    let e = bind_statement(&stmt, &catalog()).unwrap_err();
    assert_eq!(e.kind(), BindErrorKind::UndefinedParameter);
    assert_eq!(e.sqlstate(), "42P02");
}

#[test]
fn parameter_types_come_from_the_context() {
    let cases: &[(&str, &[SqlType])] = &[
        (
            "SELECT id FROM notes WHERE score > $1 AND title = $2",
            &[SqlType::Integer, SqlType::Text],
        ),
        (
            "INSERT INTO notes VALUES ($1, $2, $3)",
            &[SqlType::Integer, SqlType::Text, SqlType::Integer],
        ),
        (
            "SELECT id FROM notes LIMIT $1 OFFSET $2",
            &[SqlType::BigInt, SqlType::BigInt],
        ),
        (
            "UPDATE events SET ts = $2 WHERE id = $1",
            &[SqlType::BigInt, SqlType::Timestamptz],
        ),
        (
            "SELECT id FROM notes WHERE id BETWEEN $1 AND $2",
            &[SqlType::Integer, SqlType::Integer],
        ),
        (
            "SELECT id FROM notes WHERE id IN ($1, 2, $2)",
            &[SqlType::Integer, SqlType::Integer],
        ),
        (
            "SELECT substring(title, $1, $2) FROM notes",
            &[SqlType::Integer, SqlType::Integer],
        ),
        ("SELECT id FROM notes WHERE title LIKE $1", &[SqlType::Text]),
        ("SELECT id FROM notes WHERE $1", &[SqlType::Boolean]),
        ("SELECT $1::bigint FROM notes", &[SqlType::BigInt]),
        ("SELECT id FROM notes AS OF CHANGESET $1", &[SqlType::Text]),
        (
            "SELECT id FROM events AS OF TIMESTAMP $1",
            &[SqlType::Timestamptz],
        ),
        (
            "SELECT id FROM notes WHERE score = $1 OR id = $1",
            &[SqlType::Integer],
        ),
        (
            "SELECT id FROM events WHERE body -> $1 = body",
            &[SqlType::Text],
        ),
    ];
    for (sql, want) in cases {
        let stmt = parse_statement(sql).unwrap();
        let bound = bind_statement(&stmt, &catalog()).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
        assert_eq!(&bound.param_types, want, "`{sql}`");
    }
}

#[test]
fn null_and_untyped_operands_take_the_type_of_their_context() {
    // NULL = x and NULL AND TRUE are boolean; a NULL operand keeps the other side's type.
    assert_eq!(
        output_types(&plan_of(
            "SELECT NULL + 1, NULL = score, NULL AND true, NULL FROM notes"
        )),
        [
            SqlType::Integer,
            SqlType::Boolean,
            SqlType::Boolean,
            SqlType::Text
        ]
    );
    assert_eq!(
        plan_of("SELECT id FROM notes WHERE NULL").to_string(),
        "Project [id]\n  Filter NULL\n    Scan notes\n"
    );
    assert_eq!(
        plan_of("SELECT id FROM notes WHERE score = NULL OR title IS NOT NULL").to_string(),
        "Project [id]\n  Filter ((score = NULL) OR (title IS NOT NULL))\n    Scan notes\n"
    );
    // A string literal becomes the type of the column it meets.
    let p = plan_of("SELECT id FROM events WHERE ts > '2026-01-01' AND due = '2026-01-01'");
    assert_eq!(
        p.to_string(),
        "Project [id]\n  Filter ((ts > '2026-01-01') AND (due = '2026-01-01'))\n    Scan events\n"
    );
    assert_eq!(
        output_types(&plan_of(
            "SELECT coalesce(NULL, NULL), nullif(score, 0), coalesce(NULL, score) FROM notes"
        )),
        [SqlType::Text, SqlType::Integer, SqlType::Integer]
    );
    // Inserting NULL into a nullable column is accepted and typed as the column.
    let p = plan_of("INSERT INTO notes VALUES (1, NULL, NULL)");
    let LogicalPlan::Insert {
        source: InsertSource::Values(rows),
        ..
    } = p
    else {
        panic!("not an insert");
    };
    assert_eq!(rows[0][1].ty, SqlType::Text);
    assert_eq!(rows[0][2].ty, SqlType::Integer);
}

#[test]
fn arithmetic_promotes_to_the_wider_type_and_comparisons_are_boolean() {
    let p = plan_of(
        "SELECT small + 1, small + id, id + 1.5, id * ratio, id + CAST(1 AS bigint), \
         id / 2, id % 3, -id, id >= small, 1 + 2 FROM events",
    );
    assert_eq!(
        output_types(&p),
        [
            SqlType::Integer,
            SqlType::BigInt,
            SqlType::Numeric,
            SqlType::DoublePrecision,
            SqlType::BigInt,
            SqlType::BigInt,
            SqlType::BigInt,
            SqlType::BigInt,
            SqlType::Boolean,
            SqlType::Integer,
        ]
    );
    // date and timestamptz compare; text compares with text; jsonb operators.
    assert_eq!(
        output_types(&plan_of(
            "SELECT due = ts, body -> 'k', body ->> 'k', body @> body, CAST(id AS text) || 'x' FROM events"
        )),
        [
            SqlType::Boolean,
            SqlType::Jsonb,
            SqlType::Text,
            SqlType::Boolean,
            SqlType::Text
        ]
    );
}

#[test]
fn constant_folding_respects_the_integer_type() {
    // integer + integer stays integer, so 2147483647 + 0 is fine and 2147483647 + 1 is not;
    // an operand of type bigint widens the result.
    assert_eq!(
        plan_of("SELECT 2147483647 + 0, 2147483648 + 1, -5 + 2, 7 / 2, -7 / 2, -7 % 3 FROM notes")
            .to_string(),
        "Project [2147483647 AS ?column?, 2147483649 AS ?column?, -3 AS ?column?, 3 AS ?column?, -3 AS ?column?, -1 AS ?column?]\n  Scan notes\n"
    );
    assert_eq!(
        output_types(&plan_of("SELECT 2147483648 + 1, 1 + 1 FROM notes")),
        [SqlType::BigInt, SqlType::Integer]
    );
}

#[test]
fn columns_resolve_to_catalog_positions() {
    let p = plan_of("SELECT score, id FROM notes");
    let cols = p.output().unwrap();
    let ExprNode::Column { index: a, .. } = &cols[0].expr.kind else {
        panic!("not a column");
    };
    let ExprNode::Column { index: b, .. } = &cols[1].expr.kind else {
        panic!("not a column");
    };
    assert_eq!((*a, *b), (2, 0));
}

#[test]
fn table_alias_replaces_the_table_name_as_qualifier() {
    plan_of("SELECT n.id FROM notes n");
    plan_of("SELECT notes.id FROM notes");
}

// ----- the sqllogictest scripts -----

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
];

struct Record {
    file: &'static str,
    expect: String,
    sql: String,
}

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

/// What binding each script record must give, derived by hand from the statement and the
/// subset: `Ok` for single-table queries and data changes, `0A000` for joins, aggregates,
/// `DISTINCT`, DDL and transaction control, and the declared error where the script expects a
/// bind-time one. Constraint violations (`23505`) are runtime and bind fine.
fn expected_bind(sql: &str, script_expects: &str) -> &'static str {
    let upper = sql.to_ascii_uppercase();
    let first = upper.split_whitespace().next().unwrap_or("");
    if matches!(
        first,
        "CREATE" | "BEGIN" | "COMMIT" | "ROLLBACK" | "START" | "SET" | "SHOW"
    ) {
        return "0A000";
    }
    let unsupported = [
        "COUNT(",
        "SUM(",
        "MIN(",
        "MAX(",
        "AVG(",
        "GROUP BY",
        "DISTINCT",
        " JOIN ",
        "CROSS JOIN",
        "OVER (",
    ];
    if unsupported.iter().any(|u| upper.contains(u)) {
        return "0A000";
    }
    match script_expects {
        "42P01" => "42P01",
        "42703" => "42703",
        "23502" => "23502",
        _ => "ok",
    }
}

#[test]
fn script_records_bind_as_the_subset_says() {
    let cat = catalog();
    let (mut bound, mut refused, mut skipped) = (0, 0, 0);
    for r in records() {
        // Records the parser refuses (0A000, 42601) never reach the binder.
        let Ok(stmt) = parse_statement(&r.sql) else {
            assert!(
                r.expect == "0A000" || r.expect == "42601",
                "{}: `{}` should parse",
                r.file,
                r.sql
            );
            skipped += 1;
            continue;
        };
        let want = expected_bind(&r.sql, &r.expect);
        match (bind(&stmt, &cat), want) {
            (Ok(_), "ok") => bound += 1,
            (Err(e), state) if state != "ok" => {
                assert_eq!(e.sqlstate(), state, "{}: `{}`: {e}", r.file, r.sql);
                refused += 1;
            }
            (got, state) => panic!("{}: `{}` expected {state}, got {got:?}", r.file, r.sql),
        }
    }
    assert!(bound >= 25, "only {bound} records bound");
    assert!(refused >= 20, "only {refused} records refused");
    assert_eq!(skipped, 7, "parser-refused records");
}

// ----- properties -----

fn walk(e: &TypedExpr, f: &mut impl FnMut(&TypedExpr)) {
    f(e);
    match &e.kind {
        ExprNode::Column { .. } | ExprNode::Const(_) | ExprNode::Param(_) => {}
        ExprNode::Unary { expr, .. } | ExprNode::IsNull { expr, .. } | ExprNode::Cast { expr } => {
            walk(expr, f);
        }
        ExprNode::Binary { left, right, .. } => {
            walk(left, f);
            walk(right, f);
        }
        ExprNode::InList { expr, list, .. } => {
            walk(expr, f);
            list.iter().for_each(|x| walk(x, f));
        }
        ExprNode::Between {
            expr, low, high, ..
        } => {
            walk(expr, f);
            walk(low, f);
            walk(high, f);
        }
        ExprNode::Like { expr, pattern, .. } => {
            walk(expr, f);
            walk(pattern, f);
        }
        ExprNode::Function { args, .. } => args.iter().for_each(|x| walk(x, f)),
    }
}

fn plan_exprs(p: &LogicalPlan, f: &mut impl FnMut(&TypedExpr)) {
    match p {
        LogicalPlan::Scan { .. } | LogicalPlan::OneRow => {}
        LogicalPlan::Filter { input, predicate } => {
            walk(predicate, f);
            plan_exprs(input, f);
        }
        LogicalPlan::Project { input, columns } => {
            columns.iter().for_each(|c| walk(&c.expr, f));
            plan_exprs(input, f);
        }
        LogicalPlan::Sort { input, keys } => {
            keys.iter().for_each(|k| walk(&k.expr, f));
            plan_exprs(input, f);
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            limit.iter().chain(offset).for_each(|x| walk(x, f));
            plan_exprs(input, f);
        }
        LogicalPlan::Insert {
            source, returning, ..
        } => {
            match source {
                InsertSource::Values(rows) => rows.iter().flatten().for_each(|x| walk(x, f)),
                InsertSource::Query(q) => plan_exprs(q, f),
            }
            returning.iter().for_each(|c| walk(&c.expr, f));
        }
        LogicalPlan::Update {
            input,
            assignments,
            returning,
            ..
        } => {
            assignments.iter().for_each(|(_, x)| walk(x, f));
            returning.iter().for_each(|c| walk(&c.expr, f));
            plan_exprs(input, f);
        }
        LogicalPlan::Delete {
            input, returning, ..
        } => {
            returning.iter().for_each(|c| walk(&c.expr, f));
            plan_exprs(input, f);
        }
    }
}

fn value() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("id".to_string()),
        Just("title".to_string()),
        Just("score".to_string()),
        Just("nope".to_string()),
        Just("n.id".to_string()),
        Just("NULL".to_string()),
        Just("TRUE".to_string()),
        Just("'text'".to_string()),
        Just("''".to_string()),
        Just("$1".to_string()),
        Just("$2".to_string()),
        Just("0".to_string()),
        Just("1".to_string()),
        Just("2147483647".to_string()),
        Just("9223372036854775808".to_string()),
        Just("1.5".to_string()),
        Just("1e3".to_string()),
    ]
}

fn expr() -> impl Strategy<Value = String> {
    value().prop_recursive(4, 40, 3, |inner| {
        prop_oneof![
            (
                inner.clone(),
                prop_oneof![
                    Just("+"),
                    Just("-"),
                    Just("*"),
                    Just("/"),
                    Just("%"),
                    Just("||"),
                    Just("="),
                    Just("<>"),
                    Just("<"),
                    Just(">="),
                    Just("AND"),
                    Just("OR"),
                    Just("->"),
                    Just("@>")
                ],
                inner.clone()
            )
                .prop_map(|(a, op, b)| format!("({a} {op} {b})")),
            inner.clone().prop_map(|a| format!("(NOT {a})")),
            inner.clone().prop_map(|a| format!("(-{a})")),
            inner.clone().prop_map(|a| format!("({a} IS NULL)")),
            (inner.clone(), inner.clone(), inner.clone())
                .prop_map(|(a, b, c)| format!("({a} BETWEEN {b} AND {c})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} IN ({b}, 1))")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} LIKE {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("coalesce({a}, {b})")),
            inner.clone().prop_map(|a| format!("lower({a})")),
            inner.clone().prop_map(|a| format!("abs({a})")),
            inner.clone().prop_map(|a| format!("CAST({a} AS bigint)")),
            inner.clone().prop_map(|a| format!("{a}::text")),
            inner.clone().prop_map(|a| format!("length({a})")),
            (inner.clone(), inner).prop_map(|(a, b)| format!("substring({a}, {b})")),
        ]
    })
}

fn statement() -> impl Strategy<Value = String> {
    let table = prop_oneof![
        Just("notes n"),
        Just("notes"),
        Just("items"),
        Just("missing")
    ];
    (expr(), expr(), expr(), table, 0..6u8).prop_map(|(a, b, c, t, kind)| match kind {
        0 => format!("SELECT {a}, {b} FROM {t} WHERE {c}"),
        1 => format!("SELECT {a} FROM {t} ORDER BY {b} DESC LIMIT {c}"),
        2 => format!(
            "INSERT INTO {} VALUES ({a}, {b}, {c})",
            t.split(' ').next().unwrap()
        ),
        3 => format!("UPDATE {t} SET score = {a} WHERE {b}"),
        4 => format!("DELETE FROM {t} WHERE {a} RETURNING {b}"),
        _ => format!("INSERT INTO items SELECT {a}, {b}, {c}, NULL FROM {t}"),
    })
}

proptest! {
    /// Binding never panics, every error has a five-character SQLSTATE, and a plan that binds has
    /// no untyped expression and as many parameter types as the highest placeholder.
    #[test]
    fn binding_never_panics_and_plans_are_fully_typed(sql in statement()) {
        if let Ok(stmt) = parse_statement(&sql) {
            match bind_statement(&stmt, &catalog()) {
                Ok(bound) => {
                    let mut unknown = false;
                    plan_exprs(&bound.plan, &mut |e| unknown |= e.ty == SqlType::Unknown);
                    prop_assert!(!unknown, "untyped expression in\n{}", bound.plan);
                    prop_assert!(bound.param_types.iter().all(|t| *t != SqlType::Unknown));
                    // Printing a plan never panics either.
                    let _ = bound.plan.to_string();
                }
                Err(e) => prop_assert_eq!(e.sqlstate().len(), 5),
            }
        }
    }

    /// Text that is only loosely SQL-shaped reaches the binder when it parses; it must not panic.
    #[test]
    fn loose_text_never_panics_the_binder(sql in "[a-z0-9 ,()*=<>+'$.-]{0,60}") {
        if let Ok(stmt) = parse_statement(&sql) {
            let _ = bind(&stmt, &catalog());
        }
    }
}

#[test]
fn a_parameter_with_a_type_is_an_operand_of_that_type() {
    let cases: &[(&str, &[SqlType])] = &[
        // $1 and $2 are integers from the first two comparisons, so $1 + $2 is integer arithmetic.
        (
            "SELECT id FROM notes WHERE id = $1 AND score = $2 AND $1 + $2 > 0",
            &[SqlType::Integer, SqlType::Integer],
        ),
        (
            "SELECT id FROM notes WHERE id = $1 AND score = $2 AND coalesce($1, $2) > 0",
            &[SqlType::Integer, SqlType::Integer],
        ),
        (
            "SELECT id FROM notes WHERE title = $1 AND $1 IS NOT NULL",
            &[SqlType::Text],
        ),
        (
            "SELECT id FROM notes WHERE title = $1 AND title <> $1 AND $1 = title",
            &[SqlType::Text],
        ),
        // A typed parameter next to a wider column keeps its own type.
        (
            "SELECT id FROM events WHERE small = $1 AND id = $1",
            &[SqlType::SmallInt],
        ),
    ];
    for (sql, want) in cases {
        let stmt = parse_statement(sql).unwrap();
        let bound = bind_statement(&stmt, &catalog()).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
        assert_eq!(&bound.param_types, want, "`{sql}`");
    }
    // The reused parameter prints with its type resolved: the sum is integer arithmetic.
    assert_eq!(
        plan_of("SELECT id FROM notes WHERE id = $1 AND score = $2 AND $1 + $2 > 0").to_string(),
        "Project [id]\n  Filter (((id = $1) AND (score = $2)) AND (($1 + $2) > 0))\n    Scan notes\n"
    );
}

#[test]
fn a_known_parameter_never_matches_an_operand_of_another_type_silently() {
    // integer parameter against a text parameter: both typed before they meet.
    let stmt =
        parse_statement("SELECT id FROM notes WHERE id = $1 AND title = $2 AND $1 = $2").unwrap();
    let e = bind_statement(&stmt, &catalog()).unwrap_err();
    assert_eq!(e.kind(), BindErrorKind::InconsistentParamTypes);
}

#[test]
fn plans_carry_table_ids_and_the_catalog_version() {
    let cat = catalog();
    let notes_id = cat.table("notes").unwrap().id;
    let items_id = cat.table("items").unwrap().id;
    assert_ne!(notes_id, items_id);
    let bound = |sql: &str| bind_statement(&parse_statement(sql).unwrap(), &cat).unwrap();

    let b = bound("SELECT id FROM notes WHERE score > 1");
    assert_eq!(b.catalog_version, CATALOG_VERSION);
    let LogicalPlan::Project { input, .. } = &b.plan else {
        panic!("not a project");
    };
    let LogicalPlan::Filter { input, .. } = &**input else {
        panic!("not a filter");
    };
    assert!(matches!(&**input, LogicalPlan::Scan { table_id, .. } if *table_id == notes_id));

    let b = bound("INSERT INTO items VALUES (1, 'a', 1, 'x')");
    assert!(matches!(&b.plan, LogicalPlan::Insert { table_id, .. } if *table_id == items_id));
    let b = bound("UPDATE items SET qty = 1");
    let LogicalPlan::Update {
        table_id, input, ..
    } = &b.plan
    else {
        panic!("not an update");
    };
    assert_eq!(*table_id, items_id);
    assert!(matches!(&**input, LogicalPlan::Scan { table_id, .. } if *table_id == items_id));
    let b = bound("DELETE FROM items");
    assert!(matches!(&b.plan, LogicalPlan::Delete { table_id, .. } if *table_id == items_id));
}

#[test]
fn order_by_prefers_an_output_name_and_accepts_identical_duplicates() {
    // The same expression under one name twice is not ambiguous.
    plan_of("SELECT id AS x, id AS x FROM notes ORDER BY x");
    plan_of("SELECT id, id FROM notes ORDER BY id");
}

#[test]
fn select_without_from_binds_over_a_one_row_source() {
    let text = plan_of("SELECT 1 + 2 AS three WHERE true LIMIT 1").to_string();
    assert!(text.contains("OneRow"), "{text}");
    assert!(!text.contains("Scan"), "{text}");
    assert_eq!(bind_err("SELECT *").sqlstate(), "42601");
    assert_eq!(bind_err("SELECT nope").sqlstate(), "42703");
}
