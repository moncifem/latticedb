# lattice-sql

Layer L2.

**Responsibility.** Lexer, parser and AST for the SQL subset declared in `docs/sql-subset.md`.
The parser is strict and bounded: anything outside the subset is refused with an error that names
the unsupported feature, and no input makes it panic.

**Must not.** Do I/O, plan or execute queries, or depend on the kernel.

Allowed workspace dependencies: none.

## Contract

- `parse_statement` parses exactly one statement; `parse_statements` parses a `;`-separated script.
- Syntax errors map to SQLSTATE `42601` (`LSQL-0001`), constructs that are valid PostgreSQL but not
  in the subset map to `0A000` (`LSQL-0002`). Inputs over the size or nesting limits are refused
  with `TooLarge` (SQLSTATE `54001`).
- Every statement and expression prints back (`Display`) to text that parses to an equal AST.
  Spans are carried for error messages and are ignored by `==`.
- Limits: 1 MiB of input, 200,000 tokens, nesting depth 64, identifiers up to 63 bytes.
