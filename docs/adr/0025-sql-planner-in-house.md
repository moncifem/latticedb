# ADR 0025: SQL planner: build in house

Status: accepted

Accepted 2026-10-29. Decided by the tech lead under the owner's delegation of 2026-10-29. The conditions below are part of
the decision. Requirement P2-10.

[ADR 0013](0013-sql-planner-and-executor.md) holds the criteria C1 to C7 and the measured spike. It
keeps its own status (**proposed**): its C1 (effort-days) and the Phase 1 SQL baseline part of C4 are
still unresolved, and condition 4 below is where they get recorded. This ADR selects the planner
direction; it does not close those criteria.

## Context

The parser and AST are merged (`crates/lattice-sql`). Binding to a typed logical plan for the
single-table subset now exists too (`plan.rs`, `binder.rs`, behind an abstract `CatalogView`). What is
open is everything after that: joins, aggregation, optimisation and execution over the row engine's
snapshots. The choice is between:

- **(a) An in-house planner and executor** for the declared subset in `docs/sql-subset.md`, built on
  the merged parser, binder and plan IR.
- **(b) An existing planner library**, in practice Apache DataFusion with `sqlparser` (the spike also
  measured `sqlparser` as a parser only, with our own planner, which is a variant of (a)). The spike
  pins `datafusion =55.2.0` and `sqlparser =0.63.0` (`experiments/sql-planner/Cargo.toml`), the
  latest stable releases when it was written.

## Decision criteria

The seven criteria below are the ones requested for this decision. They extend ADR 0013's C1 to C7
with the two the spike did not measure (binary size, memory bounds) and one that was only reviewed
(row policy in scans).

| Criterion | What would favour a choice | Evidence available |
|---|---|---|
| Correctness surface | Smaller code we can fully test against the subset, with PostgreSQL differential tests | (a) owns every operator and its NULL, overflow and ordering rules; the binder tests already pin error codes. (b) brings a large tested engine whose semantics follow its own dialect, not PostgreSQL's (the spike's semantics audit lists differences). Full-subset conformance was measured for neither |
| Dependency and supply chain | Few crates, passing `cargo deny`, no async runtime in engine layers | Spike, final clean run: `sqlparser` + custom passed policy; DataFusion failed it (zstd-related findings kept in the report). Resolved package counts from the first run: 37 / 39 / 240 (custom / parser / DataFusion) |
| Determinism for the simulator (C2) | No clock, randomised hash seed or thread pool reachable | The audit in `experiments/sql-planner/C2_AUDIT.md` found clock reads and randomised hash construction reachable in the pinned DataFusion integration; custom and parser paths matched traces and passed review |
| Memory bounds | Operators stream or have a stated cap, so a statement cannot exhaust the process | **Not measured.** (a) lets us make every operator streaming or capped by construction (Phase 2 memory theme); whether (b)'s memory accounting can enforce Lattice limits was not tested |
| Binary size | Small `latticed` | **Not measured.** The spike recorded build time, not artifact size. DataFusion's 240-package tree makes a large increase likely, but that is an inference and not a number |
| Row policy enforcement in scans | Every scan goes through the policy-bound `RowTx`, with no executor path around it | (a) can make the scan the only way to read a table. (b) needs a table provider contract that is columnar and snapshot-agnostic (ADR 0013, C3); the process-kill and row/index/log checks passed for all candidates on a bounded corpus, but policy binding to an authenticated identity was not part of the spike |
| Compile time (C5) | Added clean build under 60 s | Final clean run, added over the bridge: custom 11.278 s, `sqlparser` + custom 56.906 s (little margin), DataFusion 1127.774 s (fails) |

## What the spike numbers do and do not show

Numbers are available for build time, dependency policy, the four PostgreSQL latency ratios, determinism
traces and a bounded corpus of twenty-seven error fixtures
([RESULTS.md](../../experiments/sql-planner/RESULTS.md), run
[40912191052](https://github.com/moncifem/latticedb/actions/runs/40912191052)). In that run the median
ratios to PostgreSQL (point / range / join / group) were custom 0.44 / 1.82 / **3.25** / 2.62,
`sqlparser` + custom 0.55 / 1.87 / 2.79 / 2.39, DataFusion 2.30 / 1.84 / **3.07** / **3.11**
(threshold 3). These are prototype results on fixed schemas, not the full subset.

They do **not** show: historical effort-days (C1 has no budget and none were recorded), a comparison
against a Phase 1 SQL baseline (none exists), the cost of correlated subqueries or window functions
(C7 is a written assessment only), binary size or memory behaviour. The custom join miss is a
prototype executor without a join optimiser; it says what the prototype did, not what a planner we
build with hash joins would do, and it must not be read as proof either way.

The library versions measured are the ones the spike pins: `datafusion =55.2.0` and
`sqlparser =0.63.0`. Public pages found while writing this ADR (docs.rs, the ASF release catalog,
an ecosyste.ms index) showed older or inconsistent version numbers, so no version claim here relies
on them; the pins in the experiment are the only versions this ADR speaks about.

A third route, `pg_query` wrapping PostgreSQL's own parser, builds vendored C source through
`build.rs` ([docs.rs build script](https://docs.rs/crate/pg_query/6.1.1/source/build.rs)). It would
give exact PostgreSQL syntax but adds a C toolchain and a native dependency to a project that
otherwise builds as pure Rust, and it replaces only the parser we already have. It was not chosen.

## Decision

Build **(a): an in-house planner and executor for the declared subset**, on the merged parser and the
plan IR in `crates/lattice-sql`. Conditions:

1. **Scope freeze.** No window functions, correlated subqueries or recursive CTEs until a later phase
   decides them in an ADR. Other additions follow `docs/sql-subset.md`: documented change plus tests.
2. **Semantics authority.** The PostgreSQL differential sqllogictest harness (WP29) decides what is
   correct. Any disagreement is a bug unless the construct is marked `# differs-from-postgres` in the
   scripts, with the reason in `docs/sql-subset.md`.
3. **Reopen trigger.** Reopen this decision if, after the Phase 2 gate review, the in-house planner and
   executor has more than 3 unresolved P1 or P2 semantic bugs from differential runs, or if
   join-heavy fixtures stay above the 3x PostgreSQL ratio threshold after the executor exists.
4. **Measure before the gate.** Binary size, memory bounds and effort-days are measured before the
   Phase 2 gate review and recorded in `docs/gates/phase-2.md`. Until then those three criteria are
   open, not passed.
5. Keep `lattice-sql` free of storage dependencies; execution lives in a higher layer behind the
   `CatalogView` and scan interfaces so the simulator can run it without threads, clocks or random
   hashing. A bound plan is valid only for the catalog version it was bound against.

Why not (b) now: in the measured run it failed the determinism audit, dependency policy and the
build-time threshold, and it would force the policy and snapshot rules to be enforced through a
columnar provider interface the engine does not share. If a library is ever reconsidered, it must be
re-measured at the versions then current, starting from the pins above (`datafusion =55.2.0`,
`sqlparser =0.63.0`), against the same thresholds and with the failing runs kept.

## Consequences

The cost is writing and testing every operator ourselves, and keeping the PostgreSQL
differential harness the authority on semantics. The benefit is a deterministic, streaming executor
whose scans cannot bypass the row policy, with a small dependency tree. If the decision is reopened in favour
of a library, the IR in `plan.rs` is still useful as the bound form handed to it, but the binder's
type rules would have to be reconciled with the library's.
