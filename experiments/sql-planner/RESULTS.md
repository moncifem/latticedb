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


### Fresh-main integration attempts

Run `40897564722` at `763310b` was cancelled deliberately after a static audit found an obsolete
index-read API call; it is not measurement evidence. The corrected run
[40897846609](https://github.com/moncifem/latticedb/actions/runs/40897846609), at
`c637fb37c191910e5e9f4bf7f50de14965e4678a`, finished with build failures because the current
policy-bound `RowTx` no longer exposes `into_writes`. Its artifact `12908347886` was verified against
ZIP SHA256 `dc30b6656bc353739aa6f37224b202a496d412d332fdfc4fe1b2941e6714b5fa`.
It contains no new query timings. Custom/parser dependency-policy checks passed; DataFusion retained
the zstd-related policy failures.

The bridge now derives the expected WAL envelope from its separate submitted-row overlay and checks
read-only agreement, without exposing RowTx's private policy-bound buffer. A remote shared-code
compile preflight uses a separate target before candidate builds, preserving clean target isolation
and C5 measurement scope. The next result must establish compilation, conformance and measurement;
these source corrections alone establish none of them.


The submitted-row overlay is an independent buffer from RowTx, but it mirrors rows selected by the
SQL executor. Its envelope comparison checks bridge publication fidelity, not independent correctness
of those SQL choices. SQL correctness remains checked by the complete PostgreSQL comparisons and
literal conformance expectations. The process-kill verifier independently reconstructs each complete
WAL envelope from the submitted UPDATE sequence and fixed seed rows (`id % 7`), checks observed
receipts, all final rows/indexes/log scrub, and a second reopen. These independent oracles remain in
place; an executor choosing the wrong row/value or omitting writes must not move the expected SQL
answer along with the actual one.

## First optimized scalar-codec comparison

[Run 40900147776](https://github.com/moncifem/latticedb/actions/runs/40900147776), source
`62667ab3d1f5265913ce89371cde2ce37ec9d9f1`, completed all candidate builds, release conformance
tests, repeated simulator traces, nine negative fixtures, PostgreSQL comparisons and three
process-kill checks per candidate. Source status and experiment input hashes stayed unchanged.
Artifact `12909940513` was verified against ZIP SHA256
`6772a2f458935710fd934e5242b6413cc94b81711944d86384a96de52d3ca01f`.

| Candidate | Point ratio | 10,000-row range ratio | Join ratio | Group ratio |
|---|---:|---:|---:|---:|
| custom | 0.4342 | 2.1553 | 4.5819 | 4.1396 |
| sqlparser + custom | 0.4864 | 2.2629 | 5.2550 | 4.3556 |
| DataFusion | 2.4488 | 2.1041 | 3.2154 | 3.7704 |

Point and range measurements meet the fixed 3x threshold; **joins and groups still fail it for all
three candidates**. Thus C4 does not pass. Custom/parser dependency policy passed. DataFusion's zstd
policy failures were the only recorded workflow failures; its strict C2 source-audit failure and
historical C5 clean-build threshold failure also remain. This functional run supplies no new clean
build evidence and does not accept the ADR.

Remaining measured engine medians were 19.2–20.1 ms for custom/parser joins and 12.7–13.1 ms for
grouping. The next bounded change removes the second primary-key map rebuilt during every scan,
merging actual pending rows directly into the already ordered snapshot stream. Generic table-local
conjuncts are evaluated before joining; the complete ON/WHERE predicates still run afterward.
Independent review also identified default ascending NULL ordering and incomplete aggregate binding.
The continuation adds explicit NULLS LAST comparison, PostgreSQL/literal regressions using nullable
boolean expressions, and explicit rejection fixtures for ungrouped aggregate references. These
changes await fresh conformance and measurements; the recorded timings above are not results for
the subsequent source.

## Ordered scan merge and predicate pushdown comparison

[Run 40903066476](https://github.com/moncifem/latticedb/actions/runs/40903066476), source
`45969c88dbfa1a0bb3ddfa09b1cd087cd157e5b1`, completed release conformance, thirteen negative
fixtures, repeated simulator traces, PostgreSQL semantic checks and all three process-kill/reopen
checks for each candidate. Artifact `12909674931` was verified against ZIP SHA256
`392d801cd602bc0020b6e0dcd0736e821ed71df29537eb0c332bc7d87368a288`.

| Candidate | Point ratio | 10,000-row range ratio | Join ratio | Group ratio |
|---|---:|---:|---:|---:|
| custom | 0.5147 | 1.7984 | 2.9703 | 2.6819 |
| sqlparser + custom | 0.5197 | 2.0114 | 2.9703 | 2.7551 |
| DataFusion | 2.3839 | 1.6692 | 2.5695 | 2.6379 |

**All twelve ratios meet the unchanged 3x PostgreSQL part of C4 in this run.** The custom/parser
join margin is small; this is one recorded runner result, not a robust performance headroom claim.
The separate Phase 1 SQL regression comparison remains unestablished. Custom/parser policy checks
passed; DataFusion's zstd findings remain the sole recorded workflow failure. Its strict C2 and
historical clean-build C5 failures remain independent disqualifying evidence for this integration.
No functional-retry compiler timing replaces the first clean measurement.

Subsequent static review identified two additional unsupported shapes that must be refused before
execution: a self join without supported aliases, and mixed-type column joins whose equality index
could otherwise return no rows without reporting the type mismatch. Explicit binding guards and two
negative fixtures address those cases. The nullable-boolean PostgreSQL case now returns both NULL
and non-NULL client cells, rather than only testing their placement before a limit. These final
semantic checks need their own fresh confirmation; no ratios above are attributed to an unmeasured
source. C1 effort/budget, the Phase 1 baseline, C7's qualified assessment and independent final review
remain unresolved. ADR 0013 stays proposed.

## Binding-guard confirmation before the batched type audit

[Run 40904476774](https://github.com/moncifem/latticedb/actions/runs/40904476774), source
`a83c974bf9799c563ca8c747fab284b8ef3f7785`, completed all candidate conformance, fifteen negative
fixtures, repeated simulator traces, full PostgreSQL comparisons and process-kill/reopen cases.
The nullable-boolean PostgreSQL case returned eight rows, checking both NULL and non-NULL client
values against the independent reference. Artifact `12910752906` was verified against ZIP SHA256
`7d232808722ab9bd62b4ed55fed476cbef37989bc142335c04cefc14fd6996c0`.

| Candidate | Point ratio | 10,000-row range ratio | Join ratio | Group ratio |
|---|---:|---:|---:|---:|
| custom | 0.3984 | 2.0053 | 2.8142 | 2.3818 |
| sqlparser + custom | 0.5532 | 1.7617 | 2.9435 | 2.6401 |
| DataFusion | 2.3865 | 1.6115 | 2.3081 | 2.4585 |

All twelve PostgreSQL ratios meet 3x again; the parser join margin remains small. The sole recorded
workflow failure was DataFusion's zstd dependency policy. Historical failures, its strict C2 finding,
and the missing Phase 1 SQL baseline remain. These measurements precede the subsequent batched
actual-parameter/type validation and are not attributed to that later source.

The next iteration uses the explicit custom/parser-only `semantic_confirmation` mode. It validates
integer/text/boolean/NULL parameter transport, type errors even on empty inputs, strict predicates,
prepared rebind guards and absence of writes on validation failure. This mode does not run C4
benchmarks, supplies no C5 criterion result, does not build DataFusion and cannot accept the ADR.
The complete bounded audit and remaining scope limits are in SEMANTICS_AUDIT.md. Final full
confirmation follows code freeze and independent review, with default full measurement unchanged.

## Batched bounded-type semantic confirmation

[Run 40907444118](https://github.com/moncifem/latticedb/actions/runs/40907444118) succeeded at
`be31f26ac23a31871d9d2dfae48f1cbfce05b6a4`. Its `semantic_confirmation` scope selected exactly
custom/parser. Artifact `12912696422` was verified against ZIP SHA256
`c169e7cda72b022a2f320ff53c8342affce1a89a89faeee338d056470b32361e`.
Both candidate builds/conformance suites, nineteen negative fixtures, repeated simulator traces,
PostgreSQL boolean/NULL/text parameter cases, nullable ordering and all six process-kill/reopen cases
passed. Source status and experiment input hashes stayed unchanged.

The report explicitly sets C4/C5 and clean-build evidence false; both criterion assessments say
not measured. No DataFusion candidate was built or executed, and no four-query performance ratio
was measured. Thus this succeeds as bounded semantic evidence only. The newly shared type-validation
and cached prepared-insert guard still need final DataFusion compilation/confirmation after review.

Rebase onto fresh main `2cdd293` changed no experiment or experiment-workflow bytes; the sole conflict
was an append in docs/releasing.md, resolved by retaining both sections. No new prototype scope is
planned. Final independent review and one frozen-source full confirmation remain before landing;
ADR acceptance, missing C1 effort/budget, the Phase 1 SQL baseline and C7 limitations are separate
open decisions, not grounds to relabel research measurements as accepted criteria.

The fresh-main rebase confirmation [40908694603](https://github.com/moncifem/latticedb/actions/runs/40908694603)
succeeded at `e3d8def3580fcb1f14907a678f4c9ad5a32960f2` with the same explicit custom/parser-only
semantic scope and no C4/C5 claims. Artifact `12913260904` was verified against ZIP SHA256
`26d5e720fbf84ea2ae1fee6196db446af2aa08c8b7dc18f6888c8f84c6d5c9fb`; source status and hashes
remained unchanged.

Final independent review found two remaining binding families: bare ORDER/GROUP ordinals and
projection aliases were being misbound or discarded, and qualified DML RETURNING leaked a table
qualifier across DataFusion's derived projection. The bounded prototype now uniformly refuses
those forms with 0A000 / LSQL-0002, rather than expanding its grammar or returning different rows.
Literal/no-write regressions and explicit PostgreSQL reference checks pin the unsupported forms,
including PostgreSQL's ordinal and alias semantics. Those intentional prototype refusals are
reported separately from matching supported SQL results. The fixed four performance queries
remain unchanged. One batched semantic confirmation and a frozen-source clean full comparison
follow these review fixes; no result for that source is inferred here.

The final review batch passed [semantic confirmation 40910710191](https://github.com/moncifem/latticedb/actions/runs/40910710191)
at `ac1efb80162509640f1ecb42d7e474a7dc34626f`. Both custom/parser candidates passed conformance,
twenty-seven negative fixtures, simulator repeat checks, all process-kill cases and seven explicit
unsupported-semantics PostgreSQL reference checks, including the 42803 aggregate-ordinal case and
qualified RETURNING rollback/no-write checks. Artifact `12913138745` was verified against ZIP SHA256
`316cf70a22ed1c88a3e7771ad7e4e93a21b7f0a86a307f3dc5c0db781fe0b0ad`. Source status and hashes
remained unchanged; this semantic-only result supplies no C4/C5 evidence.

The final rebase targets explicit main `0142daf495b0907e2e640547ec7217b15dfe7870`; the documentation
append conflict retained both sections, and all experiment/experiment-workflow bytes were verified
unchanged from that successful semantic source. The next clean full run will establish current
all-candidate compilation, semantics, PostgreSQL timing and clean build measurements. Research
publication remains separate from accepting ADR 0013 or claiming full-subset/phase readiness.


## Final frozen-source clean comparison

[Run 40912191052](https://github.com/moncifem/latticedb/actions/runs/40912191052), attempt 1, measured
`aeae4492debdffdb9363ff8c31bcd865adbffd58` in `clean_build` mode. Artifact `12916705355` was
verified against ZIP SHA256 `863030b2c251edcc5efbfec684fced95a4addda303128460894c33a4261ba429`.
The run/attempt/SHA context matches the report. Experiment input hashes and tracked source stayed
unchanged; the sole untracked path before/after was the run's `planner-evidence/` output directory.
No compiler cache was restored, the bridge and all candidates used separate empty compiler targets,
and the shared compile preflight used its own target. Performance binaries excluded fault hooks;
PostgreSQL fsync, synchronous_commit and full_page_writes all remained on.

All three candidates built and passed release conformance, twenty-seven negative fixtures, repeated
simulator traces, supported PostgreSQL comparisons, seven explicitly unsupported-semantics reference
checks and three process-kill/reopen cases each. This includes the latest DataFusion expression-type
and prepared-rebind guards. There is no remaining known compilation or bounded-contract failure.
The sole workflow failure was DataFusion's dependency-policy check, retained without exclusions.

The bridge-only clean build took **12.452 seconds**. Current clean measurements are:

| Candidate | Clean build seconds | Added vs bridge seconds | Added under 60s | Dependency policy |
|---|---:|---:|---|---|
| custom | 23.730 | 11.278 | yes | passed |
| sqlparser + custom | 69.358 | 56.906 | yes | passed |
| DataFusion | 1140.226 | 1127.774 | **no** | **failed** |

Client median ratios recomputed from all nine raw samples per query are:

| Candidate | Point ratio | 10,000-row range ratio | Join ratio | Group ratio |
|---|---:|---:|---:|---:|
| custom | 0.4399 | 1.8182 | **3.2509** | 2.6230 |
| sqlparser + custom | 0.5500 | 1.8729 | 2.7910 | 2.3901 |
| DataFusion | 2.3001 | 1.8418 | **3.0655** | **3.1132** |

The parser candidate meets all four PostgreSQL ratios and the added-build threshold in this clean
run, with limited build-time margin. Custom fails the join ratio; DataFusion fails join/group ratios,
the build threshold and dependency policy, and its independent strict C2 clock/random-seed finding
remains. Earlier passing ratios are retained above but do not override these latest failures. No
rerun was used to select a more favorable outcome. Client protocols and storage designs still differ
as described in README.md, and no comparable Phase 1 SQL baseline exists.

The final landing rebase includes main `61739f39ee8b736cc43bfcea93ee2e6567c36d99`. Experiment source
and workflow bytes were verified unchanged from this measured source before this evidence-only
update; the rebase added the independent core feed module and tests, without changing the called
storage/I/O APIs. These are measurements at the named source, not invented timings for later commits.

This completes the bounded research artifact, not ADR acceptance. C1's absent budget and unrecorded
historical effort-days, full-subset coverage, C4's Phase 1 baseline portion and the qualified C7
assessment remain open. Production dependencies, the declared SQL subset, phase gates and pivot
selection are unchanged. No further prototype features or heavy runs are planned for this PR.
