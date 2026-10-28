# SQL subset

Status: draft (Phase 2, requirement P2-09). The subset is **declared**: what is listed here is
supported and tested; everything else fails with a stable error, never with a wrong answer. Adding to
the subset is a documented change with tests in the same pull request. The Phase 2 conformance suite
(`sqllogictest`, differential tests against PostgreSQL on the constructs below) is generated from this
file.

Reachable by PostgreSQL wire v3 clients (P2-11). Behaviour of transactions is specified in
[`formats/isolation.md`](../formats/isolation.md).

## Principles

1. **PostgreSQL behaviour where we accept the syntax.** If a construct is in the subset it returns what
   PostgreSQL returns (the differential test enforces this). Where we cannot match, we reject the
   construct instead.
2. **Reject, do not approximate.** Unsupported syntax fails with SQLSTATE `0A000` (`feature_not_supported`)
   and a stable Lattice code in the message (`LSQL-xxxx`), documented below.
3. **Tables come from LSL.** DDL is derived from the schema language (`lattice lsl`), so `CREATE TABLE`
   is a thin form of a schema change, not a separate source of truth.
4. **Every statement is a snapshot read or one change set.** A write statement or transaction commits as
   one change set (invariants I2, I3).

## Statements

### Data definition

| Statement | Support |
|---|---|
| `CREATE TABLE` | Columns of the types below, `PRIMARY KEY`, `NOT NULL`, `DEFAULT` (constant), `UNIQUE`; maps to an LSL facet type plus its typed table |
| `ALTER TABLE ... ADD COLUMN` | Online, nullable or with a constant default; the compatibility checker (`lsl compat`) must accept it |
| `ALTER TABLE ... DROP COLUMN`, `RENAME` | Through a reversible lens only; see online schema change (P2-16) |
| `CREATE INDEX`, `DROP INDEX` | B-tree on one or more columns; maintained transactionally (P2-07) |
| `DROP TABLE` | Yes; history stays in the log |
| Views, triggers, sequences beyond `serial`/identity, extensions, `CREATE FUNCTION` | Not supported |

### Queries

| Construct | Support |
|---|---|
| `SELECT` list, `*`, aliases, expressions over supported types | Yes |
| `FROM` with one or more tables, `JOIN` (`INNER`, `LEFT`, `CROSS`) with `ON` | Yes. `RIGHT`, `FULL`, `NATURAL`, `LATERAL` no |
| `WHERE` | Comparison, `AND`/`OR`/`NOT`, `IN (list)`, `BETWEEN`, `LIKE`, `IS [NOT] NULL` |
| `GROUP BY`, `HAVING` | Yes, with `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`; `COUNT(DISTINCT x)` |
| `ORDER BY`, `LIMIT`, `OFFSET` | Yes, with `NULLS FIRST/LAST` |
| `DISTINCT` | Yes |
| Subqueries | Scalar and `IN` subqueries, uncorrelated. Correlated, `EXISTS`, `ANY/ALL` not yet |
| `WITH` (CTE), window functions, `UNION`/`INTERSECT`/`EXCEPT`, `RECURSIVE` | Not in Phase 2 |
| `AS OF CHANGESET <id>` / `AS OF TIMESTAMP '<ts>'` after a table reference | Yes (P2-14). Read-only snapshot |

### Data changes

| Statement | Support |
|---|---|
| `INSERT ... VALUES`, `INSERT ... SELECT` | Yes. `RETURNING` of the inserted columns |
| `UPDATE ... SET ... WHERE` | Yes, `RETURNING` |
| `DELETE ... WHERE` | Yes, `RETURNING` |
| `INSERT ... ON CONFLICT` | Not in Phase 2 |
| `COPY` | Not in Phase 2 |

### Transactions and sessions

`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT` is not supported. Isolation is snapshot isolation only;
`SET TRANSACTION ISOLATION LEVEL` accepts `REPEATABLE READ` and `SERIALIZABLE` as requests for snapshot
isolation and reports the effective level (a documented difference: `SERIALIZABLE` is not granted).
Conflicting commits fail with `40001` and are safe to retry. Prepared statements and the extended query
protocol are supported (P2-11). `SET statement_timeout` and query cancellation are supported (P2-18).

## Types

| Type | Notes |
|---|---|
| `boolean`, `smallint`, `integer`, `bigint`, `double precision` | Yes |
| `numeric(p, s)` | Yes, up to 38 digits; arithmetic is exact |
| `text`, `varchar(n)` | UTF-8; collation is binary (byte order) in Phase 2 |
| `bytea` | Yes |
| `timestamptz`, `date` | Microsecond precision, UTC storage |
| `uuid` | Yes; entity ids render as the 26-character id or `uuid`, per column |
| `jsonb` | Stored, compared and indexed as a whole; operators `->`, `->>`, `@>` only |
| Arrays, ranges, `interval`, geometric, `inet`, `money`, `xml` | Not supported |

`NULL` follows the three-valued logic of PostgreSQL.

## Functions and operators

Arithmetic and comparison operators, `||` for text, `coalesce`, `nullif`, `lower`, `upper`, `length`,
`substring`, `position`, `trim`, `abs`, `round`, `now()`, `date_trunc` (`second` to `year`),
`extract`, `cast` / `::`. Anything else is `0A000`.

## Errors

| Situation | SQLSTATE | Lattice code |
|---|---|---|
| Syntax outside the grammar | `42601` | `LSQL-0001` |
| Valid PostgreSQL, not in this subset | `0A000` | `LSQL-0002` |
| Unknown table, column or type | `42P01`, `42703`, `42704` | `LSQL-0003` |
| Constraint violation | `23502`, `23505` | `LSQL-0004` |
| Write conflict | `40001` | `LSQL-0005` |
| Snapshot older than retention | `72000` | `LSQL-0006` |
| Statement cancelled or timed out | `57014` | `LSQL-0007` |

Codes never change meaning once released; new situations get new codes.

## Conformance

- `sqllogictest` files under `crates/lattice-sql/tests/slt/` cover every row of the tables above, one
  directory per section, plus a negative file per section that asserts the `0A000` rejections.
- A generator produces random queries from this grammar and runs them against Lattice and PostgreSQL;
  any difference in results is a failure (P2-13, E2-3).
- This file is **frozen** for the phase at the Phase 2 gate (E2-8). Until then each change updates the
  tests in the same pull request.

## Open questions

- Correlated subqueries and `EXISTS` are the most likely first additions; they depend on the planner
  choice (see the planner ADR, P2-10).
- Collations other than binary are a Phase 7 concern (search) and are out of scope here.
