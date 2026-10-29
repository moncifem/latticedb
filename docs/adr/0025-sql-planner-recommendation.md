# ADR 0025: Recommendation for the SQL planner

Status: proposed

Requirement P2-10. This ADR does not decide anything yet: it states a recommendation and the
decision the owner has to make. [ADR 0013](0013-sql-planner-and-executor.md) holds the criteria
C1 to C7 and the measured spike; it stays **proposed** and is not superseded until the owner accepts
a choice here.

## Context

The parser and AST are merged (`crates/lattice-sql`). Binding to a typed logical plan for the
single-table subset now exists too (`plan.rs`, `binder.rs`, behind an abstract `CatalogView`). What is
open is everything after that: joins, aggregation, optimisation and execution over the row engine's
snapshots. The choice is between:

- **(a) An in-house planner and executor** for the declared subset in `docs/sql-subset.md`, built on
  the merged parser, binder and plan IR.
- **(b) An existing planner library**, in practice Apache DataFusion with `sqlparser` (the spike also
  measured `sqlparser` as a parser only, with our own planner, which is a variant of (a)).

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

Facts about the candidate libraries from public sources, checked 2026-10-29 and inconsistent across
the pages found, so treat them as a pointer and not as pinned versions:

- `sqlparser` 0.61.0 is the newest release visible on docs.rs, and its manifest lists the Apache
  DataFusion team as publisher ([docs.rs](https://docs.rs/crate/sqlparser/0.61.0/source/Cargo.toml));
  an ecosyste.ms index synced in March 2026 still showed 0.60.0
  ([ecosyste.ms](https://packages.ecosyste.ms/api/v1/registries/crates.io/packages/sqlparser)).
- DataFusion 54.1.0 is listed as released 2026-07-21 in the ASF release catalog
  ([release catalog](https://release-catalog.apache.org/datafusion/54.1.0/index.html)). The spike
  measured an older pinned version; re-measuring on 54.x would be needed before relying on its numbers.
- A third route, `pg_query` wrapping PostgreSQL's own parser, builds vendored C source through
  `build.rs` ([docs.rs build script](https://docs.rs/crate/pg_query/6.1.1/source/build.rs)). It would
  give exact PostgreSQL syntax but adds a C toolchain and a native dependency to a project that
  otherwise builds as pure Rust, and it replaces only the parser we already have. It is not
  recommended.

## Recommendation

Choose **(a): an in-house planner and executor for the declared subset**, built on the merged parser
and the plan IR in this change, with the following conditions:

1. Extend the IR operator by operator (joins, aggregation, `DISTINCT`, subqueries), each with
   PostgreSQL differential tests and the `0A000` rejection for what is not yet there.
2. Keep `lattice-sql` free of storage dependencies; execution lives in a higher layer behind the
   `CatalogView` and scan interfaces so the simulator can run it without threads, clocks or random hashing.
3. Record the join and group ratios again once a hash join exists, keeping the failing run in the
   evidence. If ratios stay above 3 after that, or if correlated subqueries and window functions
   (C7) force a rewrite of the IR, reopen this decision.
4. Revisit adopting a library if the effort for the full subset, now uncapped per the owner's note on
   issue #119, turns out far larger than expected. The measured DataFusion failures on determinism,
   dependency policy and build time are the reasons not to start there.

Why not (b) now: it failed the determinism audit, dependency policy and the build-time threshold in
the measured run, and it would force the policy and snapshot rules to be enforced through a columnar
provider interface the engine does not share. Why not stay undecided: the binder and IR are being
built regardless, so the longer the choice is open, the more the IR risks bending toward one library.

## Owner decision needed

The owner has to say which of these holds, in chat, before this ADR can be accepted:

1. Accept (a) as recommended; or
2. Accept (a) but require the missing measurements first (binary size, memory bounds, a re-run with
   hash join); or
3. Choose (b), or ask for a re-measurement of DataFusion 54.x before deciding.

Until then this ADR is advisory. No planner is selected, the Phase 2 gate is not touched, and ADR 0013
keeps its status.

## Consequences

If accepted, the cost is writing and testing every operator ourselves, and keeping the PostgreSQL
differential harness the authority on semantics. The benefit is a deterministic, streaming executor
whose scans cannot bypass the row policy, with a small dependency tree. If rejected in favour of (b),
the IR in `plan.rs` is still useful as the bound form handed to the library, but the binder's type
rules would have to be reconciled with the library's.
