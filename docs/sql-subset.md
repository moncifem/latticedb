# SQL subset

Status: draft (Phase 2, requirement P2-09). This is the **target** SQL subset, declared before
implementation. A bounded parser and AST, PostgreSQL wire session state machine and development
listener exist; the listener serves the single-table subset from `lattice-exec` when started with
`--data-dir` (and answers a `SELECT 1` placeholder otherwise); joins, aggregates, `DISTINCT`, subqueries,
indexes and full Lattice conformance remain pending. The constructs below describe the intended
contract, not commands available today.

Once implemented, each accepted construct must be tested; everything outside the subset must fail
with a stable error, never with a wrong answer. Adding to the subset requires a documented change
with tests in the same pull request. The planned conformance suites are `sqllogictest` and differential
tests against PostgreSQL.

The intended front door is PostgreSQL wire v3 (P2-11). Transaction behaviour is specified in
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
| `WHERE` | Comparison, `AND`/`OR`/`NOT`, `IN (list)`, `BETWEEN`, `LIKE`, `IS [NOT] NULL`. The executor reads only a key range or one entry when `AND` terms compare the primary key or a `UNIQUE` constraint with constants; results are the same as a full scan (see the `lattice-exec` README, Access paths) |
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

- `sqllogictest` files cover the tables above and assert the `0A000` rejections. The scripts live in
  `crates/lattice-testkit/tests/slt/` until the SQL crate exists; they are read by the testkit's
  runner (`lattice_testkit::slt`), every statement of them is parsed by the SQL crate's tests, and
  every night `tools/slt_reference.py` checks their expected values against PostgreSQL 17 (except
  the `0A000` records, the records marked `# differs-from-postgres` and the single-session
  transaction scripts). The executor in `crates/lattice-exec` runs them; the records it cannot pass yet
  are listed with reasons in `crates/lattice-exec/tests/slt_expected_failures.txt`. Coverage, in 21 scripts and 669 records:

  | Area | Scripts |
  |---|---|
  | Basics, `INSERT`/`UPDATE`/`DELETE`, constraints | `basics`, `changes`, `constraints`, `returning` |
  | NULL and three-valued logic, `COALESCE`, `NULLIF` | `nulls_logic`, `nulls_functions` |
  | Integer overflow (`22003`), division by zero (`22012`), casts (`22P02`) | `arithmetic_errors` |
  | Text order, `LIKE`, string functions, `varchar(n)` | `strings` |
  | `ORDER BY` with `NULLS FIRST/LAST` and ties, `LIMIT`/`OFFSET` edges | `order_nulls`, `limit_offset`, `aggregates` |
  | Aggregates, `GROUP BY`, `HAVING` on empty and all-NULL input | `empty_aggregates`, `aggregates` |
  | Joins on tiny tables, `ON` against `WHERE` | `joins`, `joins_small` |
  | Uncorrelated subqueries, `NOT IN` with NULL | `subqueries` |
  | `CREATE`/`ALTER`/`DROP`, indexes | `ddl_statements` |
  | Types: boolean, numeric, date, uuid, jsonb | `types_and_casts` |
  | Transactions in one session | `transactions`, `transactions_visibility` |
  | Prepared statements (SQL-level forms refused) | `prepared_forms` |
  | Every refusal class, plus syntax errors (`42601`) | `unsupported` |

  125 records expect `0A000` and 18 expect `42601`; the parser tests check that each is refused with
  exactly that class. A record that Lattice answers differently from PostgreSQL on purpose carries
  the comment `# differs-from-postgres` (byte-order string comparison, where PostgreSQL follows the
  database collation) or expects `0A000` (PostgreSQL accepts the syntax); the reference check skips
  both. Not covered by scripts: write-write conflicts (`40001`, two sessions), correlated subqueries
  (refused when the query is bound, not when it is parsed) and `AS OF` reads (no PostgreSQL
  counterpart).
- A generator produces random queries from this grammar and runs them against Lattice and PostgreSQL;
  any difference in results is a failure (P2-13, E2-3).
- This file is **frozen** for the phase at the Phase 2 gate (E2-8). Until then each change updates the
  tests in the same pull request.

## Open questions

- Correlated subqueries and `EXISTS` are the most likely first additions; they depend on the planner
  choice (see the planner ADR, P2-10).
- Collations other than binary are a Phase 7 concern (search) and are out of scope here.

## Known differences from PostgreSQL in the binder

The binder (`crates/lattice-sql/src/binder.rs`) is the first stage of the planner chosen in
[ADR 0025](adr/0025-sql-planner-in-house.md). The differential harness is the authority on
semantics: a disagreement with PostgreSQL is a bug unless it is listed here as `differs-from-postgres`
with its reason.

| Behaviour | PostgreSQL | Lattice binder | Reason |
|---|---|---|---|
| Operand type mismatch (`int = text`, `text + int`) | `42883` (no such operator) | `42804` | one datatype-mismatch code for every operand check |
| `||` | accepts any type next to text | only `text \|\| text` and `bytea \|\| bytea` | smaller subset; others are `42804` |
| Assigning `integer` (or any non-text type) to a `text` column | assignment cast through text I/O is allowed | `42804` | stricter on purpose; use `CAST(x AS text)` |
| `NOT NULL` | raised when a row is written, so never for a statement that touches no rows | raised at bind time for a `NULL` literal or an omitted column with no default (`23502`) | decided without reading data; a statement that would fail on its first row fails earlier |
| Repeated assignment target in `UPDATE` | `42601` | `42601` | same |
| Repeated column in an `INSERT` column list | `42701` | `42701` | same |
| `ORDER BY name` where two outputs share the name | `42702` unless they are the same expression | `42702` unless they are the same expression | same |
| Constant integer arithmetic | evaluated while planning, so overflow and division by zero fail the statement | folded while binding: `2147483647 + 1` is `22003`, `1 / 0` is `22012`, even inside a branch that a function such as `coalesce(1, 1 / 0)` would never evaluate | cheap and deterministic; PostgreSQL also folds constants, but the cases a short-circuit skips are not matched and are not tested |
| `CAST` to `numeric(p, s)` or `varchar(n)` | applies the modifier | refused with `0A000` | the plan does not carry type modifiers yet |
| Parameter typing | a parameter takes the type of its first use; a conflicting later use is `42P08` | same, and a parameter that already has a type is an operand of that type in later comparisons, arithmetic and `coalesce` | |
| Joins, `GROUP BY`, aggregates, `DISTINCT`, subqueries, DDL, session statements | supported | `0A000` (`LSQL-0002`) until the plan has operators for them | staged delivery |

### Contract with the host catalog

The binder reads tables only through `CatalogView`. The host must: return case-folded names;
return one consistent catalog for all lookups of one bind; keep column order equal to stored order
(plans refer to columns by position); give each table a stable `id`; and change `version()` whenever
any table, column, type or index changes. A bound plan carries the table ids and the catalog
version. An executor must refuse to run a plan whose version differs from the catalog it runs against
and must re-bind instead. Prepared statements keep their SQL text, not their plan, across schema
changes for the same reason.

### Scope freeze

No window functions, correlated subqueries or recursive CTEs until a later phase decides them in an
ADR (ADR 0025, condition 1).

