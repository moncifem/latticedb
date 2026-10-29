# Historical measurement evidence

ADR 0013 remains proposed. These are observations at the recorded source revision, not a planner
selection, phase exit certificate or substitute for missing measurements.

## First clean run (failed harness, retained)

[Run 40799602453](https://github.com/moncifem/latticedb/actions/runs/40799602453) executed source commit
`754a338d61b25d9d22fe44c980e02e83039e5170`, tree `0ab5828d445ffb35b3afbe66a715865bcd617cca`.
The later commit-subject correction `6d9a06c` has the same tree. The `sql-planner-spike` artifact is
retained with raw logs, report, timings, source metadata and simulator traces. The run failed; it was
not restarted because observation timed out.

The bridge-only clean build took 9.79 seconds. Separate empty compiler targets produced:

| Candidate | Clean build seconds | Added vs bridge seconds | C5 added build under 60 seconds |
|---|---:|---:|---|
| custom | 18.61 | 8.82 | yes for this run |
| sqlparser + custom | 57.77 | 47.97 | yes for this run |
| DataFusion | 891.18 | 881.38 | **no** |

DataFusion's large measured build-threshold failure remains evidence even if subsequent functional
retries restore a compiler cache. Cached build times never replace this table or establish C5.

All three candidates executed all five positive simulator cases, and their repeated observable raw
traces matched. DataFusion did so using the no-Tokio-runtime future driver on this corpus. That does
not prove C2: hidden clock reads, randomized hash initialization or reachable pool paths still require
source/call-path review. The subsequent [reachable audit](C2_AUDIT.md) identified concrete clock and
randomized-hash constructors in this pinned DataFusion integration, so its strict C2 conditions fail
even though the observable traces matched. Matching ordered output alone is not proof of deterministic
execution.

All three candidates passed all three real process-kill cases: each had 32 submitted commands, then
2, 3 or 4 observed/recovered complete UPDATE statements, with the next 2,000-row statement confirmed
staged and blocked before commit. Every observed receipt, full WAL envelope, row/index/log check and
second reopen passed. These are process-only pre-commit statement boundaries, not hardware power loss
or interruption inside the WAL durability protocol.

Custom/parser matched all nine negative error fixtures. DataFusion missed the NULL constraint fixture:
its adapter treated Arrow's `DataType::Null` as an unimplemented conversion instead of passing a NULL
value to row validation. The strict expected 23502 / LSQL-0004 fixture remains unchanged; the adapter
is corrected and has an additional feature-gated NullArray regression for the next remote run.

PostgreSQL differential/performance measurements are **missing** from this run. The seed query used a
literal `%` modulo operator in a psycopg parameterized query, causing a placeholder error before the
queries ran. It is corrected to SQL `mod()`, and cheap PostgreSQL seed preflight now precedes builds.
No PostgreSQL latency ratio is inferred from this failed setup.

Dependency-policy review also did **not execute** successfully: `--config` was incorrectly placed
behind `cargo deny check`. Its failure was retained, and is not evidence of a specific license or
advisory violation. Correct global option ordering is verified cheaply before builds; actual policy
findings will still fail the measurement without blanket exclusions.

The initial tree-line crate count was inflated by ANSI duplicate markers. Counts recomputed from the
retained resolved metadata (including the experiment root) are 37 normal / 40 normal+build packages
for custom, 39 / 42 for sqlparser+custom, and 240 / 250 for DataFusion. The corrected reporter uses
package IDs rather than displayed tree lines.

C1's missing budget and effort-days remain unresolved, C7 remains qualified, and the prototype is not
full-subset coverage. No ADR acceptance follows from these results.

## Explicit functional retries

A manual `functional_retry=true` run may restore a separate dependency cache and share its target
between candidate feature builds. Such reports use `measurement_mode=functional_retry`,
`clean_build_evidence=false`, `functional_build` durations and no clean-build baseline/delta/threshold
fields. PostgreSQL results, semantic checks and failure logs remain useful; those compiler timings
are diagnostics only. The first retry can still be cold if no cache exists. Fresh clean experiments
remain the default and never restore this target cache. Performance binaries still exclude fault
hooks; barriers are built only after query measurements.
