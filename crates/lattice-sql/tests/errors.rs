//! Refusals: what is outside the subset is named, syntax errors point at the right byte, limits
//! hold, and nothing panics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sql::{
    ErrorKind, MAX_DEPTH, MAX_IDENT_LEN, MAX_INPUT_LEN, MAX_TOKENS, ParseError, parse_statement,
    parse_statements,
};

fn err(sql: &str) -> ParseError {
    match parse_statement(sql) {
        Ok(s) => panic!("`{sql}` should be refused but parsed as {s:?}"),
        Err(e) => e,
    }
}

/// `sql` is refused as unsupported, naming `feature`, at the first occurrence of `at`.
fn unsupported(sql: &str, feature: &str, at: &str) {
    let e = err(sql);
    assert_eq!(e.kind(), ErrorKind::Unsupported, "`{sql}`: {e}");
    assert_eq!(e.message(), feature, "`{sql}`");
    assert_eq!(e.sqlstate(), "0A000");
    assert_eq!(e.lattice_code(), Some("LSQL-0002"));
    assert_eq!(e.span().start, sql.find(at).unwrap(), "`{sql}` position");
}

/// `sql` is a syntax error at the first occurrence of `at`.
fn syntax(sql: &str, at: usize) -> ParseError {
    let e = err(sql);
    assert_eq!(e.kind(), ErrorKind::Syntax, "`{sql}`: {e}");
    assert_eq!(e.sqlstate(), "42601");
    assert_eq!(e.lattice_code(), Some("LSQL-0001"));
    assert_eq!(e.span().start, at, "`{sql}` position: {e}");
    e
}

#[test]
fn unsupported_queries_are_named() {
    unsupported(
        "SELECT * FROM a RIGHT JOIN b ON 1 = 1",
        "RIGHT JOIN",
        "RIGHT",
    );
    unsupported("SELECT * FROM a FULL JOIN b ON 1 = 1", "FULL JOIN", "FULL");
    unsupported("SELECT * FROM a NATURAL JOIN b", "NATURAL JOIN", "NATURAL");
    unsupported(
        "SELECT * FROM a JOIN b USING (x)",
        "JOIN ... USING",
        "USING",
    );
    unsupported("SELECT * FROM a, LATERAL f(a.x)", "LATERAL", "LATERAL");
    unsupported(
        "SELECT * FROM (SELECT 1) s",
        "subqueries and parenthesized joins in FROM",
        "(SELECT",
    );
    unsupported(
        "WITH x AS (SELECT 1) SELECT * FROM x",
        "WITH (common table expressions)",
        "WITH",
    );
    unsupported(
        "SELECT sum(a) OVER (PARTITION BY b) FROM t",
        "window functions",
        "OVER",
    );
    unsupported(
        "SELECT 1 UNION SELECT 2",
        "UNION, INTERSECT and EXCEPT",
        "UNION",
    );
    unsupported(
        "SELECT 1 EXCEPT SELECT 2",
        "UNION, INTERSECT and EXCEPT",
        "EXCEPT",
    );
    unsupported(
        "SELECT a FROM t FOR UPDATE",
        "row locking (FOR UPDATE)",
        "FOR",
    );
    unsupported("SELECT a INTO u FROM t", "SELECT INTO", "INTO");
    unsupported("SELECT DISTINCT ON (a) a FROM t", "DISTINCT ON", "ON (a)");
    unsupported("SELECT a FROM t LIMIT ALL", "LIMIT ALL", "ALL");
    unsupported("VALUES (1)", "VALUES statement", "VALUES");
    unsupported("(SELECT 1)", "parenthesized query", "(SELECT");
}

#[test]
fn unsupported_expressions_are_named() {
    unsupported(
        "SELECT a FROM t WHERE EXISTS (SELECT 1)",
        "EXISTS subqueries",
        "EXISTS",
    );
    unsupported("SELECT a = ANY (SELECT 1)", "ANY, SOME and ALL", "ANY");
    unsupported(
        "SELECT a FROM t WHERE a = ALL (1)",
        "ANY, SOME and ALL",
        "ALL",
    );
    unsupported("SELECT CASE WHEN a THEN 1 END", "CASE expressions", "CASE");
    unsupported("SELECT ARRAY[1]", "arrays", "[");
    unsupported("SELECT ARRAY(SELECT 1)", "arrays", "ARRAY");
    unsupported("SELECT a[1]", "arrays", "[");
    unsupported("SELECT foo(1)", "function `foo`", "foo");
    unsupported("SELECT s.foo(1)", "schema-qualified function calls", "(1)");
    unsupported("SELECT a.b.c.d", "names qualified by a database", ".d");
    unsupported("SELECT a ILIKE 'x'", "ILIKE", "ILIKE");
    unsupported("SELECT a NOT ILIKE 'x'", "ILIKE", "NOT");
    unsupported("SELECT a SIMILAR TO 'x'", "SIMILAR TO", "SIMILAR");
    unsupported("SELECT a LIKE 'x' ESCAPE '!'", "LIKE ... ESCAPE", "ESCAPE");
    unsupported("SELECT a IS TRUE", "IS TRUE", "TRUE");
    unsupported("SELECT a IS DISTINCT FROM b", "IS DISTINCT", "DISTINCT");
    unsupported("SELECT a ~ 'x'", "operator `~`", "~");
    unsupported("SELECT # 1", "operator `#`", "#");
    unsupported("SELECT 1 ? 2", "operator `?`", "?");
    unsupported("SELECT a ->> 'x' #> 'y'", "operator `#>`", "#>");
    unsupported(
        "SELECT E'x'",
        "escape, bit-string and national string literals",
        "E'",
    );
    unsupported("SELECT $$x$$", "dollar-quoted strings", "$$");
    unsupported("SELECT INTERVAL '1 day'", "interval", "INTERVAL");
    unsupported("SELECT a AT TIME ZONE 'utc'", "AT TIME ZONE", "AT");
    unsupported("SELECT a COLLATE \"C\"", "COLLATE", "COLLATE");
    unsupported("SELECT (1, 2)", "row values", ",");
    unsupported(
        "SELECT sum(DISTINCT a)",
        "DISTINCT in aggregates other than count",
        "DISTINCT",
    );
    unsupported(
        "SELECT count(a ORDER BY a)",
        "ORDER BY inside an aggregate",
        "ORDER",
    );
    unsupported(
        "SELECT count(*) FILTER (WHERE a)",
        "FILTER clauses",
        "FILTER",
    );
}

#[test]
fn unsupported_statements_are_named() {
    unsupported(
        "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT ... ON CONFLICT",
        "ON CONFLICT",
    );
    unsupported("INSERT INTO t DEFAULT VALUES", "DEFAULT VALUES", "DEFAULT");
    unsupported("UPDATE t SET a = 1 FROM u", "UPDATE ... FROM", "FROM u");
    unsupported("DELETE FROM t USING u", "DELETE ... USING", "USING");
    unsupported("COPY t FROM stdin", "COPY statement", "COPY");
    unsupported("SAVEPOINT s1", "SAVEPOINT", "SAVEPOINT");
    unsupported("RELEASE SAVEPOINT s1", "SAVEPOINT", "RELEASE");
    unsupported("ROLLBACK TO SAVEPOINT s1", "ROLLBACK TO SAVEPOINT", "TO");
    unsupported("TRUNCATE t", "TRUNCATE statement", "TRUNCATE");
    unsupported("EXPLAIN SELECT 1", "EXPLAIN statement", "EXPLAIN");
    unsupported("VACUUM", "VACUUM statement", "VACUUM");
    unsupported("GRANT ALL ON t TO u", "GRANT statement", "GRANT");
    unsupported("PREPARE p AS SELECT 1", "PREPARE statement", "PREPARE");
    unsupported("CREATE VIEW v AS SELECT 1", "CREATE VIEW", "CREATE VIEW");
    unsupported("CREATE SEQUENCE s", "CREATE SEQUENCE", "CREATE SEQUENCE");
    unsupported("CREATE EXTENSION x", "CREATE EXTENSION", "CREATE EXTENSION");
    unsupported("CREATE FUNCTION f()", "CREATE FUNCTION", "CREATE FUNCTION");
    unsupported("CREATE TRIGGER x", "CREATE TRIGGER", "CREATE TRIGGER");
    unsupported("CREATE TABLE t AS SELECT 1", "CREATE TABLE ... AS", "AS");
    unsupported("DROP VIEW v", "DROP VIEW", "DROP");
    unsupported("ALTER VIEW v", "ALTER VIEW", "ALTER");
    unsupported(
        "ALTER TABLE t ALTER COLUMN a TYPE int",
        "ALTER COLUMN",
        "ALTER COLUMN",
    );
    unsupported(
        "ALTER TABLE t ADD CONSTRAINT c UNIQUE (a)",
        "ADD CONSTRAINT",
        "CONSTRAINT",
    );
    unsupported("SHOW ALL", "SHOW ALL", "ALL");
    unsupported("SET SESSION x = 1", "SET SESSION and SET LOCAL", "SESSION");
    unsupported(
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "isolation level READ COMMITTED",
        "READ COMMITTED",
    );
    unsupported(
        "BEGIN ISOLATION LEVEL READ UNCOMMITTED",
        "isolation level READ UNCOMMITTED",
        "READ UNCOMMITTED",
    );
    unsupported(
        "SET TRANSACTION READ ONLY",
        "SET TRANSACTION modes other than isolation",
        "READ ONLY",
    );
    unsupported("BEGIN READ ONLY", "transaction access modes", "READ ONLY");
    unsupported("COMMIT AND CHAIN", "AND CHAIN", "AND");
}

#[test]
fn unsupported_definitions_are_named() {
    unsupported(
        "CREATE TABLE t (a int REFERENCES u)",
        "foreign keys",
        "REFERENCES",
    );
    unsupported(
        "CREATE TABLE t (a int, FOREIGN KEY (a) REFERENCES u)",
        "foreign keys",
        "FOREIGN",
    );
    unsupported(
        "CREATE TABLE t (a int CHECK (a > 0))",
        "CHECK constraints",
        "CHECK",
    );
    unsupported(
        "CREATE TABLE t (a int, CONSTRAINT c UNIQUE (a))",
        "named constraints",
        "CONSTRAINT",
    );
    unsupported(
        "CREATE TABLE t (a int DEFAULT now())",
        "DEFAULT that is not a constant",
        "now()",
    );
    unsupported(
        "CREATE TABLE t (a int DEFAULT 1 + 2)",
        "DEFAULT that is not a constant",
        "1 + 2",
    );
    unsupported(
        "CREATE TABLE t (a int GENERATED ALWAYS)",
        "generated columns",
        "GENERATED",
    );
    unsupported(
        "CREATE INDEX i ON t USING gin (a)",
        "index methods other than the default B-tree",
        "USING",
    );
    unsupported(
        "CREATE TABLE t (a interval)",
        "data type `interval`",
        "interval",
    );
    unsupported("CREATE TABLE t (a xml)", "data type `xml`", "xml");
    unsupported("CREATE TABLE t (a serial)", "data type `serial`", "serial");
    unsupported("CREATE TABLE t (a int[])", "arrays", "[");
    unsupported("CREATE TABLE t (a int ARRAY)", "arrays", "ARRAY");
    unsupported("CREATE TABLE t (a real)", "data type `real`", "real");
    unsupported(
        "CREATE TABLE t (a timestamp)",
        "timestamp without time zone",
        "timestamp",
    );
    unsupported(
        "CREATE TABLE t (a numeric(39, 2))",
        "numeric precision above 38",
        "numeric",
    );
    unsupported("SELECT x::inet", "data type `inet`", "inet");
}

#[test]
fn syntax_errors_point_at_the_problem() {
    let sql = "SELECT id FROM FROM items";
    let e = syntax(sql, sql.rfind("FROM").unwrap());
    assert!(e.message().contains("keyword `from`"), "{e}");

    syntax("SELECT", 6);
    syntax("SELECT 1 +", 10);
    syntax("SELECT (1", 9);
    syntax("SELECT 'abc", 7);
    syntax("SELECT \"abc", 7);
    syntax("SELECT 1abc", 7);
    syntax("SELECT /* open", 7);
    syntax("SELECT \"\"", 7);
    syntax("SELECT a b c", 11);
    syntax("SELECT * FROM t LIMIT", 21);
    syntax("FROB 1", 0);
    syntax("SELECT $0", 7);
    syntax("SELECT $65536", 7);
    syntax("INSERT INTO t VALUES (1), (1, 2)", 26);
    syntax("INSERT INTO t VALUES ()", 22);
    syntax("INSERT INTO t", 13);
    syntax("CREATE TABLE t ()", 16);
    syntax("CREATE TABLE t (a)", 17);
    syntax("CREATE TABLE t (a int NOT)", 25);
    syntax("CREATE TABLE t (a int DEFAULT 1 DEFAULT 2)", 32);
    syntax("CREATE TABLE t (a numeric(0))", 18);
    syntax("CREATE TABLE t (a numeric(3, 4))", 18);
    syntax("CREATE TABLE t (a varchar(0))", 26);
    syntax("SELECT count(*) FROM t WHERE count(DISTINCT a, b) = 1", 29);
    syntax("SELECT sum(*)", 7);
    syntax("SELECT a FROM t JOIN u", 22);
    syntax("SELECT a FROM t CROSS JOIN u ON a = b", 29);
    syntax("SELECT a BETWEEN 1", 18);
    syntax("SELECT a IN 1", 12);
    syntax("SELECT a IS 1", 12);
    syntax("SELECT a. FROM t", 10);
    syntax("UPDATE t SET", 12);
    syntax("DELETE t", 7);
    syntax("DELETE FROM", 11);
    syntax("DROP", 4);
    syntax("SET x", 5);
    syntax("SET x = ", 8);
    syntax("BEGIN ISOLATION LEVEL", 21);
    syntax("SELECT 1 SELECT 2", 9);
    syntax("; ;", 3);
    syntax("", 0);
    syntax("   -- nothing\n", 14);
    syntax("SELECT 1; SELECT 2", 10);
    syntax("SELECT * FROM t AS OF CHANGESET", 31);
    syntax("SELECT * FROM t AS OF CHANGESET x", 32);
}

#[test]
fn script_errors_report_the_failing_statement() {
    let sql = "SELECT 1; SELECT FROM; SELECT 3";
    let e = parse_statements(sql).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Syntax);
    assert_eq!(e.span().start, sql.find("FROM").unwrap());
    let e = parse_statements("SELECT 1 SELECT 2").unwrap_err();
    assert_eq!(e.span().start, 9);
}

#[test]
fn line_and_column_are_one_based() {
    let sql = "SELECT\n  1 +";
    let e = err(sql);
    assert_eq!(e.span().start, sql.len());
    assert_eq!(e.line_col(sql), (2, 6));
    assert_eq!(err("FROB 1").line_col("FROB 1"), (1, 1));
    let sql = "SELECT 'é', FROM t";
    let e = err(sql);
    assert_eq!(e.line_col(sql), (1, 13));
}

#[test]
fn error_display_names_the_code_and_position() {
    let e = err("SELECT * FROM a RIGHT JOIN b ON 1 = 1");
    assert_eq!(
        e.to_string(),
        "LSQL-0002 unsupported feature: RIGHT JOIN (at byte 16)"
    );
    let e = err("SELECT");
    assert_eq!(
        e.to_string(),
        "LSQL-0001 syntax error: expected an expression, found end of input (at byte 6)"
    );
}

#[test]
fn input_size_is_bounded() {
    let big = format!("SELECT 1{}", " ".repeat(MAX_INPUT_LEN));
    let e = parse_statement(&big).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::TooLarge);
    assert_eq!(e.sqlstate(), "54001");
    assert_eq!(e.lattice_code(), None);
    // Exactly at the limit is fine.
    let ok = format!("SELECT 1{}", " ".repeat(MAX_INPUT_LEN - 8));
    assert!(parse_statement(&ok).is_ok());
}

#[test]
fn token_count_is_bounded() {
    let many = format!("SELECT {}1", "1,".repeat(MAX_TOKENS / 2));
    assert_eq!(err(&many).kind(), ErrorKind::TooLarge);
    // A large bulk insert inside the limits still parses.
    let rows: Vec<String> = (0..10_000).map(|i| format!("({i}, 'x', NULL)")).collect();
    let sql = format!("INSERT INTO t VALUES {}", rows.join(", "));
    assert!(parse_statement(&sql).is_ok());
}

#[test]
fn identifier_length_is_bounded() {
    let long = "a".repeat(MAX_IDENT_LEN + 1);
    assert_eq!(err(&format!("SELECT {long}")).kind(), ErrorKind::TooLarge);
    assert_eq!(
        err(&format!("SELECT \"{long}\"")).kind(),
        ErrorKind::TooLarge
    );
    let fits = "a".repeat(MAX_IDENT_LEN);
    assert!(parse_statement(&format!("SELECT {fits}")).is_ok());
}

#[test]
fn nesting_is_bounded_without_exhausting_the_stack() {
    let n = MAX_DEPTH * 50;
    for sql in [
        format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n)),
        format!("SELECT {}1", "- ".repeat(n)),
        format!("SELECT {}a", "NOT ".repeat(n)),
        format!("SELECT {}1{}", "(SELECT ".repeat(n), ")".repeat(n)),
        format!("SELECT {}1", "1 + ".repeat(n)),
        format!("SELECT a {}", "+ 1 ".repeat(n)),
        format!("SELECT a {}", "AND b ".repeat(n)),
        format!("SELECT a {}", "|| 'x' ".repeat(n)),
        format!("SELECT {}1{}", "coalesce(".repeat(n), ")".repeat(n)),
        format!("SELECT a {}", "IS NULL ".repeat(n)),
        format!("SELECT 1 {}", "::int".repeat(n)),
        format!("SELECT x {}", "IN (1) ".repeat(n)),
    ] {
        let e = parse_statement(&sql).unwrap_err();
        assert_eq!(
            e.kind(),
            ErrorKind::TooLarge,
            "{}: {e}",
            &sql[..40.min(sql.len())]
        );
    }
}

#[test]
fn moderate_nesting_is_accepted() {
    let n = MAX_DEPTH / 2;
    assert!(parse_statement(&format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n))).is_ok());
    assert!(parse_statement(&format!("SELECT {}1", "1 + ".repeat(n))).is_ok());
    assert!(
        parse_statement(&format!(
            "SELECT {}1{}",
            "(SELECT ".repeat(10),
            ")".repeat(10)
        ))
        .is_ok()
    );
}

#[test]
fn every_prefix_and_deletion_of_valid_statements_is_handled() {
    let samples = [
        "SELECT DISTINCT t.a, count(*) AS n FROM teams t LEFT JOIN members m ON m.team_id = t.id \
         WHERE t.x BETWEEN 1 AND 5 AND m.name LIKE 'a%' OR t.y IN (1, 2) GROUP BY t.a HAVING sum(m.v) > 1 \
         ORDER BY n DESC NULLS LAST LIMIT 5 OFFSET 2",
        "INSERT INTO items (id, label) VALUES (1, 'a'), (2, 'it''s') RETURNING id",
        "UPDATE items AS i SET qty = qty + 1, label = CAST(x AS text) WHERE id < $1 RETURNING *",
        "CREATE TABLE IF NOT EXISTS t (id int PRIMARY KEY, n numeric(10, 2) DEFAULT -1, PRIMARY KEY (id), UNIQUE (n))",
        "SELECT * FROM t AS OF TIMESTAMP '2026-01-01' WHERE a IN (SELECT b FROM u) AND trim(BOTH 'x' FROM s) = extract(year FROM d)::text",
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; BEGIN; COMMIT; SELECT \"a\"\"b\" -- c\n/* d */",
    ];
    for sample in samples {
        assert!(parse_statements(sample).is_ok(), "{sample}");
        for (i, _) in sample.char_indices() {
            let _ = parse_statements(&sample[..i]);
            let _ = parse_statements(&sample[i..]);
            let mut deleted = String::from(&sample[..i]);
            deleted.push_str(sample[i..].chars().next().map_or("", |_| ""));
            let rest: String = sample[i..].chars().skip(1).collect();
            deleted.push_str(&rest);
            let _ = parse_statements(&deleted);
        }
    }
}
