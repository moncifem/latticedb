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
(`exec_handler`); `latticed --data-dir` serves it through `lattice_api::exec_host`.

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
- **Hiding the catalog.** `ActorPolicy::hiding_catalog` makes binding and `describe` see no tables for that
  actor, so every table reference is `42P01` with the message used for a missing table, and `Parse`/`Describe`
  discloses no names or columns. `latticed` uses it for `deny-all` principals. Without it, `describe` and
  binding errors (`42703` versus `42P01`) reveal names to any actor that may connect.
- **DDL follows the policy too.** `CREATE TABLE` and `DROP TABLE` commit through a transaction bound to
  the actor's policy, and `ActorPolicy` forwards `authorize_ddl` to it: an actor without that grant gets
  `42501` before anything about the catalog (which names exist) is revealed. Row access never implies
  DDL.
- **`describe` reveals names** unless the actor's policy hides the catalog (see above). Describing a
  statement (the extended protocol's `Parse`) binds it against the catalog without running it, so for an
  actor that does not hide the catalog a column or table name is answered whatever its row policy.
  Table-level visibility for finer policies is a Phase 4 question.
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
- **Aborting a block from the host.** `ExecSession::abort_transaction` fails the open block as an error in
  it would (`25P02` until `ROLLBACK`; `COMMIT` answers `ROLLBACK`). `latticed` calls it when it told the
  client a statement failed that the executor did not see fail (a timeout waiting in the queue, a
  failed `Parse`/`Describe`, an error the wire session raised itself). `discard_transaction` ends the block without committing, for a `BEGIN` that
  ran after its client was told it failed.
- **Limits.** `ExecLimits` sets a statement timeout (`SET statement_timeout`, on the database clock) and a
  memory budget. Cancellation, the deadline and the budget are checked for every row; a limit error
  is `57014` (cancelled, timed out) or `53200` (memory) and an aborted statement commits nothing.
  The host sets a ceiling with
  `ExecLimits::max_timeout_ms`: a client's `SET statement_timeout` is clamped to it, and `0`, `DEFAULT`
  or a larger value fall back to it. Memory is reserved after a row is decoded and before it is kept, so a statement can exceed its
  budget by one row. `scan_at` materialises the table's borrowed row references before any poll or
  charge, so that step is neither cancellable nor counted; a sort polls once per key evaluation and its
  comparison pass is not interruptible. Staged writes are not charged to the statement budget; they are
  capped per transaction instead: `ExecLimits::transaction_bytes` (256 MiB by default) counts the key and
  value of every staged entry twice (the engine transaction and the scan overlay) plus 96 bytes each,
  excludes the record encoded at `COMMIT` and the engine's version copies, keeps counting across
  the statements of a `BEGIN` block (a rewrite of the same key replaces its earlier count), and the write
  that crosses it fails with `53200` and fails the block. One autocommit statement is a transaction, so
  it is capped the same way. `SET timezone`, `client_encoding` and `datestyle` accept only the supported
  value (UTC, UTF8, `ISO, MDY`) and refuse the rest with `22023`. `setting_value_supported` is the rule;
  the wire startup packet applies it to `client_encoding` only and ignores a requested `TimeZone` or
  `DateStyle`, reporting the effective one. `LIKE` polls the deadline and cancellation every 1,024 characters it steps through (its
  worst case is still O(text x pattern), as in PostgreSQL, but a timeout or cancel stops it), and
  admits its token and character copies against the budget first. `||` on text or bytea,
  `lower`/`upper` (admitted at three times the input) and a bytea-to-text cast (two hex characters per
  byte) check the result size before building it: a value over 16 MiB is `54000`, and one that does
  not fit the remaining statement budget is `53200`. The check is transient (the bytes are not
  kept), so an intermediate value does not count against later rows. A text or bytea left operand of
  `||` of 4 KiB or more is reserved while its right subtree runs and released before the
  concatenation, so the two are bounded together; smaller operands are not held (at most 4 KiB per
  nesting level goes unreserved). A snapshot that was vacuumed away is `72000` with its own `ErrorKind::SnapshotTooOld`.

## Access paths

A scan under a `WHERE` reads only the rows its predicate can match when the primary key or a `UNIQUE`
constraint allows it. The rules are fixed and use no statistics (`src/access.rs`):

1. Every column of a `UNIQUE` constraint (and not the whole primary key) is compared with `=` to a
   constant: **one entry lookup** in the constraint's entry table (`AccessPath::UniqueLookup(k)`), then the
   row it names is read under the actor's policy.
2. Every primary key column is compared with `=`: a **point read** (`PrimaryKeyPoint`).
3. The leading primary key columns are compared with `=` and the next one by `<`, `<=`, `>`, `>=` or
   `BETWEEN` (or only that first column is bounded): a **key range** (`PrimaryKeyRange`).
4. Anything else (`OR`, `NOT`, `<>`, `IN`, `LIKE`, a non-key column) reads the whole table (`FullScan`).

Only the top-level `AND` terms of the form `column <op> constant` are used (the constant can be a
literal, a `$n` parameter or a constant expression such as a cast), and only if its value has exactly
the column's type: an `integer` key with an integer, `text` with text, and so on. `NULL`, `double
precision`, `numeric` and `jsonb` constants or columns never narrow a scan, so `id = 1.5` is a full scan.

**Results do not change.** The narrowed set is a superset of the matching rows, the whole predicate is
still evaluated on every row read, and rows come out in key order, so an index scan equals the full scan,
including the transaction's own uncommitted writes (merged exactly as before), `AS OF` snapshots (the same
snapshot check runs first, so a vacuumed-away snapshot is `72000` either way) and the row policy
(`RowStore::scan_at` authorizes each row; the entry-table read finds only a key, and the row is then read
under the actor's policy). The choice is made only when **every** term of the predicate is certain to
succeed (comparisons, logic, `IS NULL`, `IN`, `BETWEEN`, constants that already evaluated); a term that can
fail, such as `10 / v > 1` or a cast of a column, keeps the full scan, so an error a full scan raises on a
row the narrowed scan would skip is still raised. Limits are unchanged: cancellation and the deadline are
polled before the read and for every candidate row, and memory is charged for every row kept; a narrower
read simply keeps and polls fewer rows. `UPDATE` and `DELETE` find their rows the same way.

`Database::last_access_paths()` reports, for the most recent statement, how each scan read its table (a
test and review aid; there is no `EXPLAIN` yet).

**Not done.** Managed secondary indexes (`CREATE INDEX` is `0A000`, so the executor creates none), `IN`
lists as several point reads, `ORDER BY` on a key without a sort, early stop for `LIMIT`, reverse scans, and
streaming (the range is still materialised as row references before the first poll, like a full scan).

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

## Constant queries

`SELECT` without `FROM` runs over a one-row source that reads no table, so it takes no row policy and
needs only the session itself; `WHERE`, `LIMIT` and `OFFSET` apply to that one row.

## Not run

Joins, `GROUP BY`, aggregates, `DISTINCT` and subqueries (refused by the binder with `0A000`);
`ALTER TABLE`, `CREATE INDEX`, `DROP INDEX` (`0A000`); the `jsonb` operators `->`, `->>`, `@>` (bound,
`0A000` at run time); `numeric(p)` and `numeric(p, s)` columns (`0A000`: the plan carries no type
modifiers, so the declaration could not be enforced); `round` with negative digits; `DDL` inside a transaction block (`25001`); keys on
`numeric` or `jsonb` columns; more than 15 `UNIQUE` constraints on a table. Every scan reads the whole
table unless the rules in **Access paths** above apply. See `docs/gates/phase-2.md` for the sqllogictest records that fail for these
reasons.

## Known differences from PostgreSQL

`numeric` division chooses its scale as PostgreSQL does (about 16 significant digits) but keeps at most 38 fractional digits, and `numeric` values hold at most 38 digits in all; `double precision` prints with Rust's shortest form (with
an exponent outside 1e-4..1e15); `jsonb` is validated and kept as written, so equality is textual;
timestamps accept `YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][Z|+hh[:mm]]` with years 1 to 9999; `now()` is the
database clock at statement start; `AS OF TIMESTAMP` resolves to the newest commit at or before that
millisecond.
