# lattice-exec

Layer L5.

**Responsibility.** A deterministic, bounded executor for the single-table SQL subset. It takes the
statements that `lattice-sql` binds (`SELECT` with `WHERE`, `ORDER BY`, `LIMIT`, `OFFSET`, `AS OF`;
`INSERT`, `UPDATE`, `DELETE` with `RETURNING`) and runs the plan operators `Scan`, `Filter`, `Project`,
`Sort`, `Limit`, `Insert`, `Update` and `Delete` against `lattice-engines` row-store transactions. An
`ExecSession` also handles `CREATE TABLE`, `DROP TABLE`, `BEGIN`, `COMMIT`, `ROLLBACK`, `SET` and `SHOW`.

**Must not.** Open sockets, read real time, use threads, or bypass the row store's transactions. The
database clock is the `Clock` seam; there is no ambient state.

Allowed workspace dependencies: `lattice-abi`, `lattice-core`, `lattice-engines`, `lattice-lsl`,
`lattice-seams`, `lattice-sql`. The PostgreSQL `QueryHandler` adapter lives in `lattice-api`
(`exec_handler`); it is not wired into the listener.

## Contract

- `Database::open` opens the log and the row store. `ExecSession::new(ActorPolicy)` binds every
  transaction of the session to the actor's row policy; `ExecSession::allow_all()` is the explicit
  allow-all choice. `execute` returns an `ExecResult`: typed `Datum` rows, the columns and a
  PostgreSQL command tag (`SELECT n`, `INSERT 0 n`, `UPDATE n`, `DELETE n`, `BEGIN`, `COMMIT`, `ROLLBACK`).
  `ExecSession::status` is `Idle`, `InTransaction` or `Failed`.
- **Catalog and stale plans.** The statement is bound against `Database::catalog_view()`, whose
  `version()` is the engine catalog's last change LSN plus a counter of registered or dropped tables.
  `ExecSession::execute_bound` refuses a plan bound at another version with `ErrorKind::StalePlan`
  (`0A000`, "stale plan ... bind the statement again"); `execute` always binds fresh.
- **Transactions.** Outside a block a statement is its own transaction. `BEGIN` takes the snapshot, all
  statements in the block read it plus their own staged writes, and `COMMIT` commits them as one
  change set. A first-committer-wins conflict is `40001` (`ErrorKind::Conflict`) at `COMMIT`. An error in a
  block fails it (`25P02`) until `ROLLBACK`; `COMMIT` of a failed block rolls back. Isolation is snapshot
  isolation; `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` is accepted and `SHOW transaction_isolation`
  reports `repeatable read`.
- **Constraints.** `NOT NULL` is `23502`; a duplicate primary key or `UNIQUE` value is `23505` when the
  statement runs; two transactions that insert the same key or the same unique value collide at
  commit with `40001`. Integer overflow is `22003`, division by zero `22012`, invalid text `22P02`,
  `varchar(n)` overflow `22001`. `NULL` follows three-valued logic.
- **Row policy.** Scans only return rows the actor may read, and the engine checks every final
  old-to-new transition at commit (`42501`). The executor's own constraint probes ignore the read policy,
  as PostgreSQL does for row security, so a duplicate key reveals that a hidden row exists.
- **Limits.** `ExecLimits` sets a statement timeout (`SET statement_timeout`, on the database clock) and a
  memory budget. Cancellation, the deadline and the budget are checked for every row; a limit error
  is `57014` (cancelled, timed out) or `53200` (memory) and an aborted statement commits nothing.
  Memory is reserved after a row is decoded and before it is kept, so a statement can exceed its
  budget by one row; the engine's `scan_at` materialises a vector of borrowed row references first,
  which the budget does not see.

## Storage layout (experimental)

The engine catalog persists table ids and names only. **Column definitions are kept in memory** (`Schema`);
after a restart the host re-registers them with `Database::attach_table`, and until then the table is
unknown to SQL. Rows are encoded with the engine's typed row codec (`numeric` and `jsonb` as text,
`double precision` as its bit pattern, `uuid` as 16 bytes, `date` and `timestamptz` as integers).
Primary keys are order-preserving byte strings; a table without a key gets a process-local row id.
`UNIQUE` constraints keep their entries in separate physical tables with ids
`0x8000_0000 | table_id << 4 | k`, so a row policy sees them as rows too. This layout is not a
frozen format; it needs an RFC and an ADR before any persistence claim.

## Not run

Joins, `GROUP BY`, aggregates, `DISTINCT` and subqueries (refused by the binder with `0A000`);
`ALTER TABLE`, `CREATE INDEX`, `DROP INDEX` (`0A000`); the `jsonb` operators `->`, `->>`, `@>` (bound,
`0A000` at run time); `round` with negative digits; `DDL` inside a transaction block (`25001`); keys on
`numeric` or `jsonb` columns; more than 15 `UNIQUE` constraints on a table. Every scan reads the whole
table: no index is used. See `docs/gates/phase-2.md` for the sqllogictest records that fail for these
reasons.

## Known differences from PostgreSQL

`numeric` division gives 20 fractional digits; `double precision` prints with Rust's shortest form (with
an exponent outside 1e-4..1e15); `jsonb` is validated and kept as written, so equality is textual;
timestamps accept `YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][Z|+hh[:mm]]` with years 1 to 9999; `now()` is the
database clock at statement start; `AS OF TIMESTAMP` resolves to the newest commit at or before that
millisecond.
