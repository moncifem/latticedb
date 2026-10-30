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
  transaction of the session to the actor's row policy; there is no default or allow-all constructor,
  so a host that wants no restriction says so with `ActorPolicy::new(Rc::new(AllowAll))`. `execute` returns an `ExecResult`: typed `Datum` rows, the columns and a
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
- **DDL follows the policy too.** `CREATE TABLE` and `DROP TABLE` commit through a transaction bound to
  the actor's policy, and `ActorPolicy` forwards `authorize_ddl` to it: an actor without that grant gets
  `42501` before anything about the catalog (which names exist) is revealed. Row access never implies
  DDL.
- **`describe` reveals names.** Describing a statement (the extended protocol's `Parse`) binds it against
  the catalog without running it, so a column or table name is answered to any actor that may connect,
  whatever its row policy. Names are not rows; hiding them is a policy question for Phase 4.
- **Known side channel (interim rule, revisit with the Phase 4 policy).** A duplicate primary key or
  `UNIQUE` value is `23505` even when the row that holds it is hidden from the actor, as in PostgreSQL
  row-level security, so an actor can learn that a key exists. Nothing else is revealed: the message is
  only `duplicate key value violates unique constraint "<name>"`, with no value and no row. A test pins this.
- **Derived tables.** Physical table ids with bit 31 set are entry tables derived from `UNIQUE`
  constraints (`0x8000_0000 | table_id << 4 | k`). A `PolicyEvaluator` must decide for them as it decides
  for the owning table; `lattice_exec::owning_table(id)` (and `is_marker_table`) maps a physical id to
  its owner. A policy that compares the raw id denies every write to a table that has a `UNIQUE`
  constraint (`42501`); a test shows both.
- **`RETURNING`** is evaluated over the row just written or deleted and does not consult the read policy
  again. A policy whose write rule is wider than its read rule can show a row through `RETURNING` that
  the actor cannot `SELECT`.
- **Limits.** `ExecLimits` sets a statement timeout (`SET statement_timeout`, on the database clock) and a
  memory budget. Cancellation, the deadline and the budget are checked for every row; a limit error
  is `57014` (cancelled, timed out) or `53200` (memory) and an aborted statement commits nothing.
  The host sets a ceiling with
  `ExecLimits::max_timeout_ms`: a client's `SET statement_timeout` is clamped to it, and `0`, `DEFAULT`
  or a larger value fall back to it. Memory is reserved after a row is decoded and before it is kept, so a statement can exceed its
  budget by one row. `scan_at` materialises the table's borrowed row references before any poll or
  charge, so that step is neither cancellable nor counted; a sort polls once per key evaluation and its
  comparison pass is not interruptible. Write amplification (staged copies of rows and entries) is not
  charged. A snapshot that was vacuumed away is `72000` with its own `ErrorKind::SnapshotTooOld`.

## Storage layout (experimental)

The engine catalog persists table ids and names only. **Column definitions are kept in memory** (`Schema`);
after a restart the host re-registers them with `Database::attach_table`, and until then the table is
unknown to SQL. `attach_table` checks only the id and the name against the engine catalog, so a
different column list or constraint set is accepted after a restart and can silently stop enforcing a
constraint (#379). Within one process it additionally refuses a definition that differs from the first
one registered for that id (`42P17`); `detach_table` and a later `attach_table` exercise it. This guard is
not durable. Rows are encoded with the engine's typed row codec (`numeric` and `jsonb` as text,
`double precision` as its bit pattern, `uuid` as 16 bytes, `date` and `timestamptz` as integers).
Primary keys are order-preserving byte strings; a table without a key gets a process-local row id.
`UNIQUE` constraints keep their entries in separate physical tables with ids
`0x8000_0000 | table_id << 4 | k`, so a row policy sees them as rows too. This layout is not a
frozen format; it needs an RFC and an ADR before any persistence claim.

## Not run

Joins, `GROUP BY`, aggregates, `DISTINCT` and subqueries (refused by the binder with `0A000`);
`ALTER TABLE`, `CREATE INDEX`, `DROP INDEX` (`0A000`); the `jsonb` operators `->`, `->>`, `@>` (bound,
`0A000` at run time); `numeric(p)` and `numeric(p, s)` columns (`0A000`: the plan carries no type
modifiers, so the declaration could not be enforced); `round` with negative digits; `DDL` inside a transaction block (`25001`); keys on
`numeric` or `jsonb` columns; more than 15 `UNIQUE` constraints on a table. Every scan reads the whole
table: no index is used. See `docs/gates/phase-2.md` for the sqllogictest records that fail for these
reasons.

## Known differences from PostgreSQL

`numeric` division gives 20 fractional digits; `double precision` prints with Rust's shortest form (with
an exponent outside 1e-4..1e15); `jsonb` is validated and kept as written, so equality is textual;
timestamps accept `YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][Z|+hh[:mm]]` with years 1 to 9999; `now()` is the
database clock at statement start; `AS OF TIMESTAMP` resolves to the newest commit at or before that
millisecond.
