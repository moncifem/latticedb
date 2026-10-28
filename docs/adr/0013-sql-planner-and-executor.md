# ADR 0013: SQL planner and executor

Status: proposed

Requirement P2-10. This ADR is written before the code: the criteria are fixed first and the decision is
drawn from a measured spike, so the outcome cannot be fitted to the criteria afterwards.

## Context

Phase 2 needs a parser, planner and executor for the subset in `docs/sql-subset.md`,
reading MVCC row versions at a snapshot and writing one change set per transaction. The kernel is
synchronous and runs in a simulator with no executor behind it (ADR 0008); the engine runs on a writer
thread with readers on snapshots (ADR 0011). The options:

1. **Custom.** A hand-written parser, a small logical and physical plan set, and an iterator-style
   executor over the row engine's snapshot cursors.
2. **`sqlparser` for parsing, custom planner and executor.** Reuse the widely used parser crate and
   build the rest ourselves.
3. **`sqlparser` plus Apache DataFusion** for planning and execution, with the row engine exposed as a
   table provider.
4. **A Postgres-derived SQL layer** (the pivot in the direction document). Out of scope here; this ADR
   only tests whether the pivot trigger fires.

## Decision criteria

Fixed before measuring. Each has a threshold and a way to measure it.

| # | Criterion | Threshold | How measured |
|---|---|---|---|
| C1 | Subset coverage cost | Every construct in the subset implementable in under the budget below | Implement five representative constructs from each section of the subset (a join with filter, a group-by with having, an update with returning, an `AS OF` select, a prepared parameterised insert) and record effort in days |
| C2 | Determinism and simulation | The simulator can run the real planner and executor with no thread pool, no wall clock, no hashing with random seeds | Run each option inside the Phase 1 simulator on a fixed query set; any nondeterministic trace fails it |
| C3 | Snapshot and conflict semantics | Reads use the snapshot position of the transaction; writes produce one change set; no executor path bypasses the log | Review of the table provider or scan interface against `formats/isolation.md`, plus a test that kills the process mid-statement |
| C4 | Query performance | Point read, 10,000-row range scan, two-table join and group-by on the issue-tracker dataset within 3 times PostgreSQL on the same machine, and no regression over the Phase 1 baseline | The Phase 2 benchmarks (P2 benchmark list), reported not gated against PostgreSQL |
| C5 | Dependency weight | Added build time under 60 s on a clean CI runner; transitive crates reviewed by `cargo deny`; no async runtime pulled into the engine layers | `cargo build --timings`, `cargo deny`, layer check |
| C6 | Error fidelity | Rejection of unsupported syntax with the stable `LSQL-xxxx` codes and PostgreSQL SQLSTATEs | Negative `sqllogictest` files |
| C7 | Evolvability | Adding correlated subqueries or window functions later does not require a rewrite | Written assessment per option |

## Candidate assessment before the spike

This is a hypothesis to be tested, not the decision.

- **Custom:** best on C2, C3 and C5 by construction; worst on C1 and C7 (every operator is ours to
  write); C4 depends on how much optimisation we build.
- **`sqlparser` + custom:** removes the parser from C1 at the price of one dependency that is pure Rust
  and synchronous. Likely the best balance if C1 for the planner and executor is acceptable.
- **`sqlparser` + DataFusion:** best on C1, C4 (vectorised execution) and C7; at risk on C2 (it expects a
  multi-threaded async runtime), C3 (the table provider contract is columnar and snapshot-agnostic) and
  C5 (large dependency tree, async runtime). Its fit to a synchronous deterministic kernel is the open
  question.

## Spike plan

One week, on a branch, producing numbers for the table above and no production code:

1. Parse the subset's grammar with `sqlparser` and list the constructs it cannot represent.
2. Build the five representative constructs three ways (custom, `sqlparser` + custom, DataFusion).
3. Run each in the simulator and as a benchmark on the issue-tracker dataset; record C1 to C5.
4. Fill in this ADR's decision from the recorded numbers and change its status to accepted. If
   DataFusion fails C2 or C3, say so with the failing trace.

## Decision

Pending the spike. Until it completes, work that does not depend on the choice proceeds: the catalog,
the row engine and the isolation rules do not call the planner.

## Consequences

Deciding late costs a week and keeps the engine free of a dependency we might regret; deciding early
risks building the table interface around one planner's assumptions. The pivot trigger (native coverage
cannot reach the declared subset at acceptable cost) is evaluated from C1 and C6 at the end of the
spike.
