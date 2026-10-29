# ADR 0013 planner experiment

This is an isolated, unfinished experiment for P2-10 / issue #119. ADR 0013 remains **proposed**.
Its seven criteria are fixed in `../../docs/adr/0013-sql-planner-and-executor.md`; this directory does
not replace them or select a winner. There are no measured DataFusion, PostgreSQL, clean-CI-build or
effort-day results yet. A workflow or source implementation is not successful execution evidence.

The nested `[workspace]` and lockfile keep all experiment dependencies out of the production graph
and root lockfile. Do not add these dependencies to `lattice-engines`. DataFusion is compiled only by
the manual `sql-planner-spike` workflow, never on the development laptop or ordinary PR gate.

## The three implementations

| Candidate | Parsing | Planning/execution | Storage |
|---|---|---|---|
| custom | Hand-written lexer and precedence parser | Explicit relational plans and iterator loops | Actual RowStore snapshots / RowTx / WAL |
| parser | sqlparser PostgreSqlDialect AST, lowered directly | Same custom planner/executor | Same actual bridge |
| datafusion | sqlparser AST and the same explicit subset binder | Actual DataFusion logical/physical plans | Snapshot TableProvider, then Lattice mutation sink |

The custom candidate never invokes sqlparser. The parser candidate never reparses AST display text
with the hand parser. DataFusion never falls back to custom relational execution. Its DML integration
lowers UPDATE assignments/predicates and INSERT parameter expressions into relational projections,
executes those projections with DataFusion, and stages the resulting complete rows in one RowTx.
RETURNING is another projection over the computed new rows. This is an explicit Lattice integration
extension, not a claim that stock DataFusion's native UPDATE RETURNING already satisfies the contract.
The prepared INSERT caches a real DataFusion logical plan and binds new values without reparsing.

Table providers contain immutable Arrow batches captured from a real engine snapshot, including a
transaction's own buffered changes. MemTable is only the read-only batch container behind the custom
snapshot provider; it never owns committed data or publishes writes. All candidates commit through
RowStore, then decode the WAL and compare the exact recorded envelope to the submitted transaction.
AS OF resolves actual recorded change-set ids / HLC milliseconds to snapshot LSNs. No executor writes
around the log. The fixture schema is byte-encoded rows over the existing raw row API; it does not
claim typed catalog, LSL DDL or SQL schema implementation.

## Executable scope

Each candidate attempts five representative constructs on the same issue-tracker schema:

1. INNER JOIN with an equality join condition, filter and ordered output.
2. GROUP BY with COUNT(*) and HAVING.
3. UPDATE with arithmetic assignment, predicate and RETURNING.
4. AS OF CHANGESET SELECT (and an internal fixture-millisecond timestamp form).
5. Prepared parameterised INSERT, rebind and RETURNING.

BEGIN / COMMIT / ROLLBACK retain one immutable snapshot and one write buffer. SAVEPOINT is rejected
with 0A000 / LSQL-0002. This resolves the ambiguous target transaction paragraph in favor of P2-09
and `formats/isolation.md`; no production SQL contract is edited here. Tests cover own-write reads,
rollback without a WAL append, first-committer-wins 40001, original snapshots and exact commit batches.
The prototype does not yet implement PostgreSQL's aborted-transaction session state after every SQL
error; that is a visible integration gap, not evidence that the full transaction contract is done.

This is **not the complete Phase 2 subset**. It implements a small grammar with bigint, text, NULL,
parameters, comparisons, integer +/-, AND/OR, COUNT and ascending ordered results. Unsupported AST
fields, functions and statement tails are rejected rather than ignored. LEFT JOIN, general aliases,
other aggregates, decimal/JSON/date types, DDL, DELETE, general prepared statements, PG wire and the
rest of `docs/sql-subset.md` remain unimplemented. This does not narrow the production target.
The numeric timestamp form is experiment-only; target timestamp string parsing is not established.

`fixtures/negative.slt` contains minimal statement/error records. The runner compares exact SQLSTATE
and LSQL pairs; it does not claim the full sqllogictest suite exists. Each error, panic, missing metric,
unsupported candidate or timed-out execution is retained. No unsupported representative is recorded
as a successful candidate merely because its parser accepted the text.

## Running lightweight checks

```sh
CARGO_BUILD_JOBS=2 nice -n 15 cargo test --manifest-path experiments/sql-planner/Cargo.toml \
  --features parser --test conformance
cargo run --manifest-path experiments/sql-planner/Cargo.toml --features parser -- \
  --candidate parser --mode sim --seed 42 --output /tmp/parser-simulation.json
```

Simulation runs actual planning/execution over SimIo, SimClock and SeededRng. The report retains raw
plan/provider/storage/result events, not only sorted results. Repeat runs must agree exactly.
DataFusion's simulator path uses a single-thread future driver without a Tokio runtime. A requirement
for Tokio / a thread pool, a panic, a timeout or unequal trace is a recorded failure, not silently
replaced with a host-runtime pass. Matching traces alone cannot prove the absence of randomized hash
seeds, wall clocks or unreachable thread-spawn paths; the C2 source audit remains separately open.
DataFusion host measurements use an explicitly separate current-thread Tokio runtime.

## Manual same-runner measurement

Before merge, dispatch the already registered `io-probe` workflow at this experiment branch with
`sql_planner=true`; its existing portable/runtime jobs keep their normal behavior when the flag is
false, and the planner flag calls only this isolated reusable experiment.

The manual workflow uses Ubuntu 24.04 and Rust 1.99.1. It builds all candidates in **separate empty
target directories**, records registry prefetch separately, retains `cargo build --timings`, normal
crate trees, and `cargo deny` output. A bridge-only clean build is measured separately; candidate
minus bridge wall time is a noisy estimate of added build cost, alongside the full clean durations.
There is no incremental or shared compilation target cache. Transitive source downloads are not
called compiler work. Cargo deny failures remain failures; this spike adds no policy exceptions.

PostgreSQL is pinned to `postgres:18.1-bookworm` and immutable manifest digest
`sha256:cc9f4143a8d2fa8cf3749d0cb4d26ecf2d53a77a2ac807e9ebd67ae22426221a`; the workflow also captures
the resolved image and actual server configuration. The client is psycopg[binary] 3.3.6. Both run on
the same runner as the candidates. Both databases use durable commit settings: PostgreSQL fsync,
synchronous_commit and full_page_writes stay on, while Lattice uses StdIo WAL fsync per commit.
These are different storage designs: PG maintains heap/index pages and checkpoints; this prototype
retains an in-memory row map with a WAL. This is recorded, not presented as identical durability cost.

The fixed data has 20,000 tickets and 100 users. Point read, a 10,000-row range, two-table join and
group-by use identical data, ordered output and full result comparisons. There are two warmups and
nine measured samples with alternating acquisition order. Medians/p95 and all samples are retained.
The Python coordinator times **full persistent-client round trips**, including transport and result
decoding: PostgreSQL uses psycopg/libpq over localhost TCP; Lattice uses a persistent child with JSON
pipes. Their codecs and connection boundaries differ. Lattice separately reports in-process
parse/plan/execute time; it is not compared to PG server-only time as if they were equivalent.
The 3x ratio is reported against C4, not used to change the production gate. There is no comparable
Phase 1 SQL baseline; “no regression over Phase 1” stays unproven.

Three process-kill schedules per built candidate complement the simulator. A test-only bridge hook
reports that all 2,000 statement row mutations are staged, then blocks before publication; the parent
confirms that interior-statement boundary before killing the actual OS child. The `fault-hooks` feature is built separately only after measurements; its build is excluded from
clean-build evidence. Performance workers assert that the hook feature is absent from their binary,
and all production code is unchanged. A writer executes real
multi-row UPDATE statements against StdIo. The parent records submitted commands and only complete
post-commit replies, confirms the child is alive, kills it and drains its pipe. The verifier matches
every recovered WAL envelope to independently generated full-table mutations, checks every observed
LSN receipt, accepts an unobserved submitted tail, checks rows/indexes/log scrub and reopens again.
This establishes interruption inside the statement before commit; it cannot prove an interruption
inside the WAL commit protocol, physical
power-loss safety or soak coverage. No writer lock exists in the parent at child launch.

## Pinned APIs and evidence limits

Published source APIs were checked through official documentation / Context7 and the crates.io source
archives: sqlparser 0.63.0, DataFusion 55.2.0 (including datafusion-catalog/session), and their
checksummed standalone lockfile. DataFusion itself uses sqlparser 0.62.0, so the separate 0.63 parser
candidate adds a recorded duplicate; it is not hidden when measuring the dependency graph. Tokio
1.52.3, futures 0.3.32 and async-trait 0.1.89 are exact compatible experiment pins, not assertions that
these are the newest releases. Official APIs consulted:

- https://docs.rs/sqlparser/0.63.0/sqlparser/
- https://docs.rs/datafusion/55.2.0/datafusion/
- https://github.com/apache/datafusion/blob/main/docs/source/library-user-guide/custom-table-providers.md
- https://www.psycopg.org/psycopg3/docs/api/cursors.html
- https://www.postgresql.org/docs/18/runtime-config-wal.html

C1 references an effort budget “below” but never defines one. The one-week spike timebox is not an
invented per-construct/full-subset implementation budget. No retrospective effort-days or full-subset
coverage-cost estimates are produced. C1 remains unresolved even if all five examples execute.

C7 hypotheses after implementation: custom plans make operator/storage boundaries explicit, but
correlated subqueries require scoped binding and correlated execution/decorrelation not demonstrated
here; windows require new logical and physical operators. Reusing sqlparser reduces grammar work but
does not solve those execution changes. DataFusion supplies more of those operators, but integration
with snapshot ownership, session writes and deterministic execution still needs proof. None of this
establishes that a rewrite will or will not be necessary. C7 stays a qualified written assessment.

Do not accept ADR 0013, change production dependencies, freeze the SQL subset, infer the pivot
trigger, or declare Phase 2 readiness from parsing alone, this source tree, a green compile or a small
local simulator run. Review the actual manual CI artifacts against C1-C7 first.
