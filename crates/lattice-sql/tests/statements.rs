//! One test per statement form. Each expectation is the canonical text written out by hand
//! (keywords upper case, compound expressions parenthesized), not produced by running the printer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sql::ast::{
    AlterAction, AsOf, BinaryOp, DataType, ExprKind, InsertSource, IsolationLevel, JoinKind,
    SelectItem, SetValue, Statement, TableConstraint, TrimSide,
};
use lattice_sql::{parse_statement, parse_statements};

fn parse(sql: &str) -> Statement {
    match parse_statement(sql) {
        Ok(s) => s,
        Err(e) => panic!("`{sql}` should parse: {e}"),
    }
}

/// Parses `sql`, checks the canonical text, and checks the canonical text parses to the same tree.
fn check(sql: &str, canonical: &str) -> Statement {
    let stmt = parse(sql);
    assert_eq!(stmt.to_string(), canonical, "canonical text of `{sql}`");
    assert_eq!(
        parse(canonical),
        stmt,
        "canonical text of `{sql}` re-parses to the same tree"
    );
    stmt
}

fn query(stmt: &Statement) -> &lattice_sql::ast::Query {
    match stmt {
        Statement::Query(q) => q,
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn select_list_forms() {
    let s = check(
        "select distinct a as x, t.*, *, b c from t",
        "SELECT DISTINCT a AS x, t.*, *, b AS c FROM t",
    );
    let q = query(&s);
    assert!(q.distinct);
    assert_eq!(q.projection.len(), 4);
    assert!(
        matches!(q.projection.get(1), Some(SelectItem::QualifiedWildcard(t)) if t.value == "t")
    );
    assert!(matches!(q.projection.get(2), Some(SelectItem::Wildcard)));
    check("SELECT 1", "SELECT 1");
    check("SELECT FooBar FROM T", "SELECT foobar FROM t");
    check(
        r#"SELECT "Order", "a""b", "select" FROM "T""#,
        r#"SELECT "Order", "a""b", "select" FROM "T""#,
    );
}

#[test]
fn where_clause_forms() {
    check(
        "select id, title from notes where score >= 20",
        "SELECT id, title FROM notes WHERE (score >= 20)",
    );
    check(
        "SELECT a FROM t WHERE a != b",
        "SELECT a FROM t WHERE (a <> b)",
    );
    check(
        "SELECT a FROM t WHERE a IS NOT NULL",
        "SELECT a FROM t WHERE (a IS NOT NULL)",
    );
    check(
        "SELECT a FROM t WHERE a IN (1, 2, 3)",
        "SELECT a FROM t WHERE (a IN (1, 2, 3))",
    );
    check(
        "SELECT a FROM t WHERE a NOT IN (1)",
        "SELECT a FROM t WHERE (a NOT IN (1))",
    );
    check(
        "SELECT a FROM t WHERE a BETWEEN 10 AND 20",
        "SELECT a FROM t WHERE (a BETWEEN 10 AND 20)",
    );
    check(
        "SELECT a FROM t WHERE a NOT BETWEEN 1 AND 2",
        "SELECT a FROM t WHERE (a NOT BETWEEN 1 AND 2)",
    );
    check(
        "SELECT a FROM t WHERE r LIKE 'a%'",
        "SELECT a FROM t WHERE (r LIKE 'a%')",
    );
    check(
        "SELECT a FROM t WHERE r NOT LIKE 'a%'",
        "SELECT a FROM t WHERE (r NOT LIKE 'a%')",
    );
    check("SELECT 'it''s'", "SELECT 'it''s'");
    check(
        "SELECT TRUE, FALSE, NULL, $1, 1.5e3, .5",
        "SELECT TRUE, FALSE, NULL, $1, 1.5e3, .5",
    );
}

#[test]
fn operator_precedence() {
    check("SELECT 1 + 2 * 3 - 4", "SELECT ((1 + (2 * 3)) - 4)");
    check("SELECT a OR b AND NOT c", "SELECT (a OR (b AND (NOT c)))");
    check("SELECT NOT a = b", "SELECT (NOT (a = b))");
    check("SELECT a || b || c", "SELECT ((a || b) || c)");
    check(
        "SELECT x BETWEEN 1 AND 2 AND y",
        "SELECT ((x BETWEEN 1 AND 2) AND y)",
    );
    check("SELECT x = 1 IS NULL", "SELECT ((x = 1) IS NULL)");
    check("SELECT 2 * -3", "SELECT (2 * (-3))");
    check("SELECT -a::int", "SELECT (-CAST(a AS integer))");
    check("SELECT a % 2 = 0", "SELECT ((a % 2) = 0)");
    check("SELECT doc -> 'a' ->> 'b'", "SELECT ((doc -> 'a') ->> 'b')");
    check("SELECT doc @> other", "SELECT (doc @> other)");
    check("SELECT (1 + 2) * 3", "SELECT ((1 + 2) * 3)");
    check("SELECT (a < b) = c", "SELECT ((a < b) = c)");
    check("SELECT a=-1", "SELECT (a = (-1))");
    check("SELECT a - -1", "SELECT (a - (-1))");
}

#[test]
fn operators_bind_as_declared() {
    let s = parse("SELECT a + b * c");
    let q = query(&s);
    let Some(SelectItem::Expr { expr, .. }) = q.projection.first() else {
        panic!("projection");
    };
    let ExprKind::Binary { op, right, .. } = &expr.kind else {
        panic!("binary");
    };
    assert_eq!(*op, BinaryOp::Add);
    assert!(matches!(
        &right.kind,
        ExprKind::Binary {
            op: BinaryOp::Mul,
            ..
        }
    ));
}

#[test]
fn joins() {
    let s = check(
        "SELECT t.name, m.name FROM teams t INNER JOIN members m ON m.team_id = t.id",
        "SELECT t.name, m.name FROM teams AS t INNER JOIN members AS m ON (m.team_id = t.id)",
    );
    let q = query(&s);
    let from = q.from.first().unwrap();
    assert_eq!(from.joins.first().unwrap().kind, JoinKind::Inner);
    let s = check(
        "SELECT * FROM a JOIN b ON a.x = b.x LEFT OUTER JOIN c ON TRUE CROSS JOIN d, e",
        "SELECT * FROM a INNER JOIN b ON (a.x = b.x) LEFT JOIN c ON TRUE CROSS JOIN d, e",
    );
    let q = query(&s);
    assert_eq!(q.from.len(), 2);
    let kinds: Vec<_> = q
        .from
        .first()
        .unwrap()
        .joins
        .iter()
        .map(|j| j.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![JoinKind::Inner, JoinKind::Left, JoinKind::Cross]
    );
    assert!(q.from.first().unwrap().joins.last().unwrap().on.is_none());
    check("SELECT * FROM s.t", "SELECT * FROM s.t");
}

#[test]
fn as_of_forms() {
    let s = check(
        "SELECT * FROM t AS OF CHANGESET 12 x",
        "SELECT * FROM t AS OF CHANGESET 12 AS x",
    );
    let q = query(&s);
    let t = &q.from.first().unwrap().relation;
    assert!(matches!(t.as_of, Some(AsOf::Changeset(_))));
    assert_eq!(t.alias.as_ref().unwrap().value, "x");
    let s = check(
        "SELECT * FROM t AS OF TIMESTAMP '2026-01-01T00:00:00Z' AS x",
        "SELECT * FROM t AS OF TIMESTAMP '2026-01-01T00:00:00Z' AS x",
    );
    assert!(matches!(
        query(&s).from.first().unwrap().relation.as_of,
        Some(AsOf::Timestamp(_))
    ));
    check(
        "SELECT * FROM t x AS OF CHANGESET $1",
        "SELECT * FROM t AS OF CHANGESET $1 AS x",
    );
    check(
        "SELECT * FROM a AS OF CHANGESET 1 JOIN b AS OF CHANGESET 2 ON TRUE",
        "SELECT * FROM a AS OF CHANGESET 1 INNER JOIN b AS OF CHANGESET 2 ON TRUE",
    );
    // `of` stays usable as an alias when no CHANGESET or TIMESTAMP follows.
    check("SELECT * FROM t AS of", "SELECT * FROM t AS of");
}

#[test]
fn grouping_ordering_and_paging() {
    let s = check(
        "SELECT region, sum(amount) FROM sales GROUP BY region HAVING sum(amount) > 10 \
         ORDER BY 2 DESC NULLS LAST, region ASC, x NULLS FIRST LIMIT 5 OFFSET 2",
        "SELECT region, sum(amount) FROM sales GROUP BY region HAVING (sum(amount) > 10) \
         ORDER BY 2 DESC NULLS LAST, region, x NULLS FIRST LIMIT 5 OFFSET 2",
    );
    let q = query(&s);
    assert_eq!(q.order_by.len(), 3);
    assert!(q.order_by.first().unwrap().desc);
    assert_eq!(q.order_by.first().unwrap().nulls_first, Some(false));
    assert_eq!(q.order_by.get(2).unwrap().nulls_first, Some(true));
    check(
        "SELECT a FROM t OFFSET 3 LIMIT 1",
        "SELECT a FROM t LIMIT 1 OFFSET 3",
    );
    check(
        "SELECT count(*), count(DISTINCT rep), min(a), max(a), avg(a) FROM sales",
        "SELECT count(*), count(DISTINCT rep), min(a), max(a), avg(a) FROM sales",
    );
}

#[test]
fn subqueries() {
    check(
        "SELECT a FROM t WHERE a IN (SELECT id FROM u)",
        "SELECT a FROM t WHERE (a IN (SELECT id FROM u))",
    );
    check(
        "SELECT a FROM t WHERE a NOT IN (SELECT id FROM u)",
        "SELECT a FROM t WHERE (a NOT IN (SELECT id FROM u))",
    );
    check(
        "SELECT (SELECT max(x) FROM t) + 1",
        "SELECT ((SELECT max(x) FROM t) + 1)",
    );
}

#[test]
fn functions_and_casts() {
    check(
        "SELECT coalesce(a, 'x'), nullif(a, 0)",
        "SELECT coalesce(a, 'x'), nullif(a, 0)",
    );
    check(
        "SELECT lower(a), upper(a), length(a), abs(a), round(a, 2)",
        "SELECT lower(a), upper(a), length(a), abs(a), round(a, 2)",
    );
    check(
        "SELECT date_trunc('day', now())",
        "SELECT date_trunc('day', now())",
    );
    check(
        "SELECT substring(s FROM 2 FOR 3), substring(s, 2)",
        "SELECT substring(s, 2, 3), substring(s, 2)",
    );
    check("SELECT substring(s FROM 2)", "SELECT substring(s, 2)");
    check("SELECT position('a' IN s)", "SELECT position('a' IN s)");
    check("SELECT trim(s)", "SELECT trim(BOTH FROM s)");
    let s = check(
        "SELECT trim(LEADING 'x' FROM s)",
        "SELECT trim(LEADING 'x' FROM s)",
    );
    let q = query(&s);
    let Some(SelectItem::Expr { expr, .. }) = q.projection.first() else {
        panic!("projection");
    };
    assert!(matches!(
        &expr.kind,
        ExprKind::Trim {
            side: TrimSide::Leading,
            chars: Some(_),
            ..
        }
    ));
    check(
        "SELECT extract(year FROM ts)",
        "SELECT extract(year FROM ts)",
    );
    check(
        "SELECT CAST(x AS bigint), x::numeric(10,2), x::double precision",
        "SELECT CAST(x AS bigint), CAST(x AS numeric(10, 2)), CAST(x AS double precision)",
    );
    check(
        "SELECT DATE '2020-01-01'",
        "SELECT CAST('2020-01-01' AS date)",
    );
}

#[test]
fn insert_forms() {
    let s = check(
        "INSERT INTO items (id, label) VALUES (1,'a'), (2,'b') RETURNING qty",
        "INSERT INTO items (id, label) VALUES (1, 'a'), (2, 'b') RETURNING qty",
    );
    let Statement::Insert(ins) = &s else {
        panic!("insert")
    };
    assert_eq!(ins.columns.len(), 2);
    assert!(matches!(&ins.source, InsertSource::Values(rows) if rows.len() == 2));
    assert_eq!(ins.returning.len(), 1);
    let s = check(
        "INSERT INTO items SELECT id + 10, label FROM items",
        "INSERT INTO items SELECT (id + 10), label FROM items",
    );
    assert!(matches!(&s, Statement::Insert(i) if matches!(i.source, InsertSource::Query(_))));
    check(
        "INSERT INTO notes VALUES (1, 'alpha', 10)",
        "INSERT INTO notes VALUES (1, 'alpha', 10)",
    );
    check(
        "INSERT INTO t (a) VALUES ($1)",
        "INSERT INTO t (a) VALUES ($1)",
    );
}

#[test]
fn update_and_delete_forms() {
    check(
        "UPDATE items SET qty = qty + 1, label = 'x' WHERE id < 10 RETURNING id, label",
        "UPDATE items SET qty = (qty + 1), label = 'x' WHERE (id < 10) RETURNING id, label",
    );
    check(
        "UPDATE items i SET qty = 0",
        "UPDATE items AS i SET qty = 0",
    );
    check("DELETE FROM acct", "DELETE FROM acct");
    check(
        "DELETE FROM items WHERE sku IS NULL RETURNING id",
        "DELETE FROM items WHERE (sku IS NULL) RETURNING id",
    );
    check(
        "DELETE FROM items AS i WHERE i.id = 1",
        "DELETE FROM items AS i WHERE (i.id = 1)",
    );
}

#[test]
fn create_table_forms() {
    let s = check(
        "CREATE TABLE items (id INT PRIMARY KEY, label TEXT NOT NULL, qty INT DEFAULT 0, sku TEXT UNIQUE)",
        "CREATE TABLE items (id integer PRIMARY KEY, label text NOT NULL, qty integer DEFAULT 0, sku text UNIQUE)",
    );
    let Statement::CreateTable(t) = &s else {
        panic!("create table")
    };
    assert!(t.columns.first().unwrap().primary_key);
    assert!(t.columns.get(1).unwrap().not_null);
    assert!(t.columns.get(3).unwrap().unique);
    assert!(t.columns.get(2).unwrap().default.is_some());
    check(
        "CREATE TABLE t (a double precision, b numeric(12,3), c varchar(20), d timestamp with time zone, \
         e uuid, f jsonb, g bytea, h date, i boolean, j smallint, k bigint DEFAULT -1, l character varying, \
         m numeric, n numeric(5), o int8, p timestamptz, q decimal(4, 2), r bool, s varchar)",
        "CREATE TABLE t (a double precision, b numeric(12, 3), c varchar(20), d timestamptz, \
         e uuid, f jsonb, g bytea, h date, i boolean, j smallint, k bigint DEFAULT (-1), l varchar, \
         m numeric, n numeric(5), o bigint, p timestamptz, q numeric(4, 2), r boolean, s varchar)",
    );
    let s = check(
        "CREATE TABLE IF NOT EXISTS t (a int, b int, PRIMARY KEY (a, b), UNIQUE (b))",
        "CREATE TABLE IF NOT EXISTS t (a integer, b integer, PRIMARY KEY (a, b), UNIQUE (b))",
    );
    let Statement::CreateTable(t) = &s else {
        panic!("create table")
    };
    assert!(t.if_not_exists);
    assert_eq!(t.constraints.len(), 2);
    assert!(matches!(t.constraints.first(), Some(TableConstraint::PrimaryKey(c)) if c.len() == 2));
    assert_eq!(t.columns.first().unwrap().data_type, DataType::Integer);
    check(
        "CREATE TABLE t (a text DEFAULT 'x', b int DEFAULT NULL NOT NULL, c int NOT NULL PRIMARY KEY)",
        "CREATE TABLE t (a text DEFAULT 'x', b integer NOT NULL DEFAULT NULL, c integer PRIMARY KEY NOT NULL)",
    );
}

#[test]
fn alter_drop_and_index_forms() {
    let s = check(
        "ALTER TABLE t ADD COLUMN c int DEFAULT 5",
        "ALTER TABLE t ADD COLUMN c integer DEFAULT 5",
    );
    assert!(
        matches!(&s, Statement::AlterTable(a) if matches!(a.action, AlterAction::AddColumn(_)))
    );
    check(
        "ALTER TABLE t ADD c text",
        "ALTER TABLE t ADD COLUMN c text",
    );
    check("ALTER TABLE t DROP COLUMN c", "ALTER TABLE t DROP COLUMN c");
    check("ALTER TABLE t DROP c", "ALTER TABLE t DROP COLUMN c");
    check(
        "ALTER TABLE t RENAME COLUMN a TO b",
        "ALTER TABLE t RENAME COLUMN a TO b",
    );
    check(
        "ALTER TABLE t RENAME a TO b",
        "ALTER TABLE t RENAME COLUMN a TO b",
    );
    check("ALTER TABLE t RENAME TO u", "ALTER TABLE t RENAME TO u");
    check(
        "CREATE UNIQUE INDEX IF NOT EXISTS ix ON t (a, b)",
        "CREATE UNIQUE INDEX IF NOT EXISTS ix ON t (a, b)",
    );
    check("CREATE INDEX ON t (a)", "CREATE INDEX ON t (a)");
    check("DROP INDEX IF EXISTS ix", "DROP INDEX IF EXISTS ix");
    check("DROP INDEX ix", "DROP INDEX ix");
    check("DROP TABLE t", "DROP TABLE t");
    check("DROP TABLE IF EXISTS s.t", "DROP TABLE IF EXISTS s.t");
}

#[test]
fn transaction_and_session_forms() {
    check("BEGIN", "BEGIN");
    check("begin work", "BEGIN");
    let s = check(
        "BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
    );
    assert_eq!(
        s,
        Statement::Begin {
            isolation: Some(IsolationLevel::Serializable)
        }
    );
    check("START TRANSACTION", "BEGIN");
    check(
        "START TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "BEGIN ISOLATION LEVEL REPEATABLE READ",
    );
    check("COMMIT", "COMMIT");
    check("COMMIT WORK", "COMMIT");
    check("ROLLBACK", "ROLLBACK");
    check("ROLLBACK TRANSACTION", "ROLLBACK");
    check(
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
    let s = check(
        "SET statement_timeout TO 5000",
        "SET statement_timeout = 5000",
    );
    assert!(matches!(&s, Statement::Set { value: Some(SetValue::Number(n)), .. } if n == "5000"));
    check("SET application_name = 'x'", "SET application_name = 'x'");
    check("SET extra_float_digits = on", "SET extra_float_digits = on");
    check("SET x = DEFAULT", "SET x = DEFAULT");
    check("SET x = -1", "SET x = -1");
    check("SHOW transaction_isolation", "SHOW transaction_isolation");
}

#[test]
fn scripts_split_on_semicolons() {
    let stmts = parse_statements("SELECT 1; ;; SELECT 2;").unwrap();
    assert_eq!(stmts.len(), 2);
    assert!(parse_statements("").unwrap().is_empty());
    assert!(parse_statements("-- only a comment\n").unwrap().is_empty());
    assert_eq!(parse("SELECT 1;"), parse("SELECT 1"));
    assert_eq!(
        parse("/* lead */ SELECT /* mid */ 1 -- tail"),
        parse("SELECT 1")
    );
}

#[test]
fn spans_locate_names_and_expressions() {
    let sql = "SELECT alpha, beta + 1 FROM gamma";
    let s = parse(sql);
    let q = query(&s);
    let Some(SelectItem::Expr { expr, .. }) = q.projection.first() else {
        panic!("projection");
    };
    assert_eq!(expr.span.slice(sql), Some("alpha"));
    let Some(SelectItem::Expr { expr, .. }) = q.projection.get(1) else {
        panic!("projection");
    };
    assert_eq!(expr.span.slice(sql), Some("beta + 1"));
    let name = &q.from.first().unwrap().relation.name.0;
    assert_eq!(name.first().unwrap().span.slice(sql), Some("gamma"));
}
