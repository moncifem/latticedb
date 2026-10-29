# Reachable C2 audit for the pinned experiment

Scope: the actual adapter in `src/fusion.rs`, DataFusion 55.2.0, sqlparser 0.63.0, and the resolved
features in the first hosted run's `datafusion-metadata.json`. Published crate archives were checked
against the checksums in this standalone Cargo.lock. This is a source/call-path assessment of this
integration, not a claim about every possible DataFusion integration or the full SQL target.

## Current assessment

All three candidates produced matching raw observable traces and executed the five positive cases in
[the first hosted run](https://github.com/moncifem/latticedb/actions/runs/40799602453). DataFusion did
so under the single-thread future driver without an outer Tokio runtime. Those useful observations
do not establish the fixed C2 requirement: “no thread pool, no wall clock, no hashing with random seeds.”

**The current DataFusion adapter fails the no-host-clock and no-random-seed portions of C2.** The
following calls are reached by the valid measured plans, rather than merely occurring in an unused
function, test, example or transitive source file. No differing SQL-result trace is asserted.

### Host wall-clock read during logical planning

1. The adapter calls `SessionContext::sql` for its actual relational statement (also while preparing
   the cached INSERT).
2. In published `datafusion-55.2.0/src/execution/context/mod.rs`, `sql` delegates to `sql_with_options`
   (lines 611–650). That method calls `self.state().create_logical_plan(...)`.
3. `SessionContext::state` (lines 2054–2057) clones the state and calls `mark_start_execution`.
4. `SessionState::mark_start_execution` in `src/execution/session_state.rs` (lines 886–888) calls
   `self.execution_props.mark_start_execution(config)`.
5. `datafusion-expr-55.2.0/src/execution_props.rs:99` sets the execution start time with `Utc::now()`.

This consumes the real host UTC clock even though the fixture never calls SQL `now()`. Supplying a
SimClock to the row engine does not replace this independent planner clock. Setting a prior execution
timestamp alone does not avoid the current adapter's subsequent `state()` reset.

### Host monotonic clocks during optimization and execution

`DataFrame::create_physical_plan` calls `SessionState::create_physical_plan`, which calls `optimize`
(`datafusion-55.2.0/src/execution/session_state.rs:777–784`). Its normal non-EXPLAIN path calls the
analyzer and optimizer (lines 758–764):

- `datafusion-optimizer-55.2.0/src/analyzer/mod.rs:131` reads `Instant::now()` unconditionally.
- `datafusion-optimizer-55.2.0/src/optimizer.rs:607` does the same.
- `datafusion-common-55.2.0/src/instant.rs:29` aliases that Instant to `std::time::Instant` on Linux.

The measured group plan includes `AggregateExec: mode=Single, gby=[owner...]`. Its grouped aggregate
stream also uses real Instant reads while grouping and accumulating
(`datafusion-physical-plan-55.2.0/src/aggregates/grouped_hash_stream.rs:876,895`). Timing metrics use
`datafusion-physical-expr-common-55.2.0/src/metrics/value.rs:217` and `elapsed_compute.rs:72` as well.
The UTC call above already violates the literal wall-clock condition; these monotonic calls are
additional host-clock dependencies outside Lattice's Clock seam. The repository's clippy.toml also
classifies `std::time::Instant::now` as a Clock-seam violation.

### Randomized hashing in the optimizer, despite a fixed-state hash utility

1. Every valid normal physical-plan construction reaches `Optimizer::optimize` as described above.
2. `datafusion-optimizer-55.2.0/src/optimizer.rs:611` constructs `HashSet::with_capacity(16)` for prior
   plan signatures. Its import is `datafusion_common::HashSet`.
3. `datafusion-common-55.2.0/src/lib.rs:118–119` aliases HashMap/HashSet to hashbrown collections with
   `hashbrown::DefaultHashBuilder` (imported at line 91).
4. The retained resolved metadata enables hashbrown 0.17.1 `default-hasher` and foldhash 0.2.0 `std`.
   `hashbrown-0.17.1/src/set.rs:184` calls HashMap::with_capacity; `src/map.rs:302` constructs
   `DefaultHashBuilder::default()`.
5. `hashbrown-0.17.1/src/hasher.rs:1–17` imports `foldhash::fast::RandomState` and derives the default
   hash builder containing that state.
6. `foldhash-0.2.0/src/fast.rs:132–139` initializes it using `gen_per_hasher_seed()` and
   `GlobalSeed::new()`. `src/seed.rs:22–65` uses stack-address randomization and per-thread mutable
   state when std is enabled, outside the simulator's Rng seam.

A potentially misleading name was checked explicitly: `datafusion_common::hash_utils::RandomState`
**is** an alias for foldhash FixedState (`src/hash_utils.rs:41`). That utility does not change the
optimizer HashSet's separate DefaultHashBuilder. An audit based only on that alias would miss this
reachable randomized constructor.

## Custom and sqlparser paths

The hand parser, direct AST lowerer and custom executor use ordered BTreeMap/BTreeSet/Vec state. The
simulator entry point supplies actual SimIo, SimClock and SeededRng to the RowStore; the host
Instant/StdIo/OsRng service path is separate. sqlparser 0.63.0's parsing implementation imports ordered
BTreeMap/BTreeSet, and its normal `src` parsing/AST paths were searched for host clocks, thread spawns
and randomized hash constructors. Hits such as the SQL keyword/type name `Instant` and
`ForSystemTimeAsOf` are grammar identifiers, not clock operations. Its default features are disabled;
the experiment enables only std. The raw reports retain repeated actual execution traces.

This supports compatibility of the **bounded implemented custom/parser paths** with C2. It is not
full-subset coverage or a blanket proof about every transitive function, optional feature, future
operator, logger configuration or host-mode entry point. Broad text-search hints include tests,
examples and unused modules; they are evidence to review, not automatic failures.

The outer no-runtime execution worked for the DataFusion corpus. This audit does not classify unused
test-only thread spawns or documentation examples as reachable pool use. A complete pool audit is not
needed to establish the two concrete C2 failures above, and no full pool-safety pass is claimed.

## What would need new evidence

Target-partition count one and a current-thread executor do not seam these planner clocks or replace
the optimizer's default hash builder. A modified adapter, patched upstream components or alternative
planning path could change the assessment, but would require its own actual call-path review,
simulator traces and engineering-cost evidence. No rewrite cost or effort-days are inferred here.
ADR 0013 remains proposed; the first run's separate C5 build failure also remains recorded.

## Source integrity and primary references

The downloaded archives matched these Cargo.lock checksums:

| Published source | SHA-256 |
|---|---|
| datafusion 55.2.0 | `4cd5317db77ba6bca25c761a8091b9d294a9f4188e4f9dcd0243e6a57de862ac` |
| datafusion-expr 55.2.0 | `86560fd7df4dda924f5474b588c56e7ae8e96dcf158c7327c9cfe9daff448ec5` |
| datafusion-optimizer 55.2.0 | `acfb8cea6e05e2e75bae183340cf7d53cccb3db0449e2f1fbc07654d980fd19f` |
| datafusion-common 55.2.0 | `d26626e12b224b83351239465dcd00f72d47b02fb5e5b65a643e2dc8ed37f687` |
| hashbrown 0.17.1 | `ed5909b6e89a2db4456e54cd5f673791d7eca6732202bbf2a9cc504fe2f9b84a` |
| foldhash 0.2.0 | `77ce24cb58228fbb8aa041425bb1050850ac19177686ea6e0f41a70416f56fdb` |

Primary versioned sources: [DataFusion context](https://docs.rs/crate/datafusion/55.2.0/source/src/execution/context/mod.rs),
[session state](https://docs.rs/crate/datafusion/55.2.0/source/src/execution/session_state.rs),
[execution properties](https://docs.rs/crate/datafusion-expr/55.2.0/source/src/execution_props.rs),
[optimizer](https://docs.rs/crate/datafusion-optimizer/55.2.0/source/src/optimizer.rs),
[DataFusion collection aliases](https://docs.rs/crate/datafusion-common/55.2.0/source/src/lib.rs),
[hashbrown hash builder](https://docs.rs/crate/hashbrown/0.17.1/source/src/hasher.rs), and
[foldhash seed generation](https://docs.rs/crate/foldhash/0.2.0/source/src/seed.rs).
