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

## Retained functional retry (recovered during continuation)

[Run 40803782042](https://github.com/moncifem/latticedb/actions/runs/40803782042) executed source
`5e750af6195f5ea0c523a8eff4d25e1e92c7d5ad` in `functional_retry` mode. The retained artifact
`12867943294` was downloaded and verified against GitHub's ZIP SHA256
`c8752482ce02c336a6cab1a7b8a00a89367790ee8e5be56704ab61fc3659f6dc`.
The run failed; its successfully completed stages are retained evidence, not an overall pass.

All candidates completed the PostgreSQL representative comparisons, nine negative fixtures, repeated
observable simulator traces and three process-kill/reopen checks. The four measured client median
ratios to PostgreSQL were:

| Candidate | Point | 10,000-row range | Join | Group |
|---|---:|---:|---:|---:|
| custom | 72.318 | 10.840 | 430.499 | 9.781 |
| sqlparser + custom | 64.865 | 10.783 | 452.181 | 10.678 |
| DataFusion | 33.886 | 6.870 | 4.857 | 4.294 |

**All twelve ratios fail the unchanged C4 threshold of 3.** These include the distinct client codecs
and transport costs described in README.md; no equivalent pure-executor timing is claimed. There is
still no comparable Phase 1 SQL baseline. Functional-retry compiler durations cannot establish C5.

The only recorded run-failure stages were dependency-policy checks. The experiment manifest lacked
its Apache-2.0 license metadata, failing all three candidate license checks. DataFusion additionally
reported Tokio RUSTSEC-2026-0386 and zstd/zstd-safe RUSTSEC-2026-0391, RUSTSEC-2026-0398 and
RUSTSEC-2026-0403. Its pinned datafusion-common enables arrow-ipc's zstd feature unconditionally;
Arrow 59's zstd 0.13 constraint cannot select the advisory's patched zstd >=0.14.3 / zstd-safe >=8.2.0.
No advisory is ignored. The continuation adds the missing own-package license and updates the
experiment's compatible Tokio pin to 1.53.3; a fresh policy run is required. The existing DataFusion
C2 source-audit failure is independent of these dependency findings.

### Prototype changes awaiting fresh measurements

The original custom point query scanned and decoded every row. Joins built a new string-keyed
expression environment for every pair in a Cartesian product; nonaggregate output allocated a
singleton group per row. The continuation adds conservative primary-key bounds in the shared real
RowStore bridge, borrowed indexed-row evaluation, deterministic equality-indexed joins with a general
predicate fallback, and count/representative aggregate state. DataFusion receives the same proven
single-table bounds but continues to plan and evaluate its complete relational query itself.
No decoded-row cache, query-result cache, fixture-specific answer, benchmark/oracle relaxation or
production dependency change is introduced. Original failed ratios above remain visible.

`criteria.json` recomputes median ratios from raw samples and records all C1–C7 evidence limits. It
never accepts ADR 0013. New remote measurements and independent review remain necessary; C1's absent
budget and unrecorded historical effort-days remain unresolved, and C7 remains qualified.

The subsequent transport revision uses `json-scalars-v1` uniformly for all three persistent
prototype clients. It removes internal enum wrappers from each result cell, retaining signed bigint,
text, boolean and NULL values. JSON decoding stays in client-round-trip timing; independent shape,
integer-bound and exact type/value comparisons run afterward, as do PostgreSQL oracle assertions.
The report records the codec version. PostgreSQL still uses its distinct libpq/TCP transport.
Historical enum-codec ratios above are retained; neither codec change nor source implementation is a
new measured result.
