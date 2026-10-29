# lattice-sql

Layer L2.

**Responsibility.** Lexer, parser and AST for the SQL subset declared in `docs/sql-subset.md`, and a
binder that turns single-table statements into a typed logical plan (`plan`, `binder`).
The parser is strict and bounded: anything outside the subset is refused with an error that names
the unsupported feature, and no input makes it panic.

**Must not.** Do I/O, execute queries, or depend on the kernel or any storage engine. The binder sees
the database only through the `plan::CatalogView` trait.

Allowed workspace dependencies: none.

## Contract

- `parse_statement` parses exactly one statement; `parse_statements` parses a `;`-separated script.
- Syntax errors map to SQLSTATE `42601` (`LSQL-0001`), constructs that are valid PostgreSQL but not
  in the subset map to `0A000` (`LSQL-0002`). Inputs over the size or nesting limits are refused
  with `TooLarge` (SQLSTATE `54001`).
- Every statement and expression prints back (`Display`) to text that parses to an equal AST.
  Spans are carried for error messages and are ignored by `==`.
- Limits: 1 MiB of input, 200,000 tokens, nesting depth 64, identifiers up to 63 bytes.
- `binder::bind(&Statement, &dyn CatalogView)` returns a `plan::LogicalPlan` (`Scan`, `Filter`,
  `Project`, `Sort`, `Limit`, `Insert`, `Update`, `Delete`); `bind_statement` also returns the types
  of the `$n` placeholders. Names resolve to column positions, expressions are typed, integer
  constants fold (`2147483647 + 1` is `22003`, `1 / 0` is `22012`).
- Bind errors map to `42P01`, `42703`, `42804`, `22003`, `23502` and others, plus `LSQL-0003` and
  `LSQL-0004` where the subset document defines them. Joins, `GROUP BY`, aggregates, `DISTINCT`,
  subqueries, DDL and session statements are refused with `0A000` until the IR has operators for them.
  Operand type mismatches report `42804` where PostgreSQL reports `42883` for operators.
- `LogicalPlan` prints as an indented tree for review. The text is not a stable interface.
