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

Pending resolution of the unmeasured criteria and recorded candidate failures. The bounded measured
research is recorded below; this ADR does not select a planner. Work that does not depend on the
choice proceeds: the catalog, the row engine and isolation rules do not call the planner.

## Consequences

Deciding late costs a week and keeps the engine free of a dependency we might regret; deciding early
risks building the table interface around one planner's assumptions. The pivot trigger (native coverage
cannot reach the declared subset at acceptable cost) is evaluated from C1 and C6 at the end of the
spike.


## Recorded evidence; decision still pending

The criteria above remain unchanged. The isolated comparison and source audit live under
`experiments/sql-planner/`; [RESULTS.md](../../experiments/sql-planner/RESULTS.md) records exact run
revisions and artifact provenance. The first clean run and its explicitly cached functional retry
establish these bounded observations, without selecting a planner:

| Criterion | Current evidence and remaining gap |
|---|---|
| C1 | Five representative constructs executed across all three options. The stated budget is absent and historical effort-days were not recorded; full-subset cost is unresolved. |
| C2 | Custom/parser paths have matching observable simulator traces and bounded source review. The pinned DataFusion integration reaches host clocks and randomized optimizer hashing, failing the strict condition; ordered output does not repair this. |
| C3 | All candidates passed three process-kill/reopen cases, receipt/WAL-envelope checks and row/index/log scrub on the recorded corpus. These cover staged precommit statements, not a kill inside the durability protocol or hardware power loss. |
| C4 | The final clean run meets all four PostgreSQL ratios for sqlparser + custom; custom fails join, and DataFusion fails join/group. Earlier all-ratio passing runs remain historical observations. No comparable Phase 1 SQL baseline exists, so C4 as a whole is unresolved. |
| C5 | Final clean added build costs are 11.278s custom, 56.906s parser and 1127.774s DataFusion. Custom/parser pass dependency policy; DataFusion retains zstd-related failures and exceeds 60s. Earlier measurements and all raw provenance remain recorded; cached timings do not replace clean evidence. |
| C6 | All three matched twenty-seven negative prototype fixtures and passed the final bounded semantic checks in the clean run. This is bounded error-fidelity coverage, not the complete declared subset. |
| C7 | Written assessments identify required correlated-scope binding and window operators for custom/parser, and snapshot/write/determinism integration work for DataFusion. The assessment remains qualified; no prototype demonstrates the no-rewrite threshold. |

C1, the Phase 1 baseline part of C4 and full-subset feasibility remain open. In particular, no
historical effort-days are inferred from commit timestamps or CI durations. New measurements must
retain candidate failures, full result/type checks, the fixed PostgreSQL workload and the original
thresholds. ADR status remains **proposed**; neither a green workflow nor these bounded tests accepts
it or evaluates the pivot trigger conclusively.
