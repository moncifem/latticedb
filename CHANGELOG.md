# Changelog

All notable changes are recorded here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0 the on-disk format and APIs can change
between minor versions; format changes are called out explicitly.

## [Unreleased]

### Added

- `RowStore::events_since` and `events_since_as` (WP36, #238): the change feed emitted from committed row
  changes in commit order, resumable from a cursor, filtered by the actor's read policy, identical after
  reopen, and refused with `CursorTooOld` at or below the vacuum floor. Read-only accessors
  `RowStore::history_from` and `Table::history_from`. No exit criterion is claimed.

- `lattice-testkit` fencing race tests (WP46, #248): takeover during append and during watch
  reconnect on the kernel under five fault profiles, judged by a script oracle, with wrapper mutants
  and planted kernel bugs proving each assertion can fail. Test only; no exit criterion is claimed.

- `RowStore::try_snapshot_as_of_hlc`: strict full-HLC predecessor resolution returns
  `SnapshotTooOld` for expired history while the existing helper retains its floor clamp.
  This is a storage API; SQL timestamp execution and SQLSTATE mapping remain pending.

- `docs/agent-workflow.md`: contributor and agent workflow guide with an onboarding checklist.
- A manual same-host foundation benchmark diagnostic with nine alternating sample pairs and raw
  evidence retention; it does not change benchmark thresholds or decide SQL planner criteria.
- `tools/merge-pr.sh`: serial PR merger that treats API errors and unknown values as retries, updates
  BEHIND branches, refuses on failed checks and merges only when CLEAN (tests in
  `tools/test_merge_pr.sh`, guide in `docs/merging.md`). `tools/branch-protection.json` now requires
  strict (up-to-date) status checks and `enforce_admins` as proposed configuration only; the helper
  reads live protection and refuses updates/merges until those prerequisites are actually applied.
- Pull request governance: code owners for risky paths, a `risk-gate` check that requires the
  `reviewed` label when formats, the kernel, auth, layers, workflows, branch protection or RFC/ADR
  files change, and a `claims` job that labels and comments when open pull requests touch the same
  files. Policy in `docs/review-policy.md`. Both checks are advisory until the owner requires them.
- Proposed `main-guard` automation, defaulting to dry-run until separate owner activation. It verifies
  exact workflow/run/main and green-predecessor evidence before proposing a draft full-SHA revert PR;
  unknown, multi-commit, infrastructure and protected-format cases are conservative. Real Git/fake
  GitHub fixture tests cover refs, races and identity; no live GitHub rehearsal or activation is claimed.
  It never pushes to main or changes settings. Procedure and limits are in `docs/main-guard.md`.

- Managed physical-byte index definitions and create/drop/recreate history persisted in atomic LCIX
  catalog/row changes. Default managed opens rebuild without callbacks; explicit callback mode remains
  legacy-only. Compound writes require the accepted WAL capability guard and actual replay context;
  actor-bound row checks precede ID/RNG/WAL. The owner accepted RFC 0005 / ADR 0019 and waived its wait
  on 2026-10-29, conditional on final-head green CI. Independent review and 15 finite engine histories /
  30 frozen-reader probes passed; synchronous build cost, retained-state allocation bounds and broader
  soak remain unmeasured. No in-place downgrade, online build or checkpoint format is introduced.

- Explicit WAL engine-capability admission: preflight the complete segment-header chain,
  require durable guards before protected appends, preserve legacy WAL bytes and expose per-record
  recovery context. `Io::read_prefix` is a required bounded seam API. Hosted frozen-reader/cut checks
  and warmed-cache open/RSS measurements passed at the recorded source. The owner accepted
  RFC 0008 / ADR 0022 and waived this guard's wait on 2026-10-29, conditional on final-head green CI.
  Managed-index RFC 0005 / ADR 0019 was accepted separately; its merge still requires its own final CI.

- A typed change feed model (`lattice_core::feed`): one `ChangeEvent` per entity change of a committed
  change set, and a `Cursor` of `(lsn, index)` whose order is commit order and does not depend on
  restarts. Strict bounded decoding that accepts only canonical bytes, specified with worked byte examples
  in `formats/change-feed-v1.md`, and a `feed_decode` fuzz target. Not yet connected to the log, the
  engines or any subscription, and not yet in the fuzz workflow matrices.
- A TLA+ model of the tree CRDT move rule (`tla/TreeMove.tla`): acyclicity, reachability of the root or
  trash, and convergence under delayed, reordered and duplicated delivery, checked on a materialised
  tree per replica. Negative tests remove the cycle check and the undo/redo of later operations and
  must make TLC fail (`tla/check-negative.sh`, run by `tla/check-all.sh`).
- A sans-I/O sync protocol codec (`lattice_api::sync`, `formats/sync-protocol-v1.md`): hello and version
  negotiation (highest common version, or `unsupported_version`), subscribe with scope and opaque cursor,
  events, ack, error and ping, in strict bounded binary frames with one encoding per message. Round-trip,
  byte-vector and no-panic property tests, and a `sync_frame` fuzz target with seeds registered in the
  nightly, weekly and manual 24-hour matrices. No server, session state or change feed yet.

- A tree CRDT with move semantics (`lattice_core::crdt_tree`): stable node ids, moves ordered by the
  hybrid logical clock with undo and redo of later operations, cycle-forming moves dropped identically
  on every replica, delete as a move into a tombstone root, idempotent delivery, and a strict 41-byte
  operation encoding. Convergence and acyclicity are property-tested (I5); the planted defect
  `TreeCycleCheckSkipped` is caught. Not yet wired to the row engine or sync, and the operation log is
  not compacted.

- Configured PostgreSQL SCRAM-SHA-256 authentication over mandatory TLS, with bounded account/verifier
  loading, a persistent provisioned unknown-user secret and one absolute startup/authentication deadline.
  Trust mode is restricted to loopback; authenticated mode never falls back to trust or plaintext.
  The query handler remains the development `SELECT 1` placeholder, with no row-policy identity binding.

- Loom models exercise cancellation/query replacement, unregister and stale credentials against the
  actual registry with instrumented token atomics. API pull requests and nightly run the models;
  socket, host admission and engine synchronization need separate evidence.

- Seed corpora for every fuzz target under `fuzz/seeds`, read by the nightly, weekly and 24-hour runs. The
  manual 24-hour campaign ran six targets for 28 hours 20 minutes each with no crash.
- A failed scheduled nightly opens an issue labelled `nightly-failure` with the failed jobs and the seed,
  scenario and invariant lines from their logs.
- A sqllogictest reader, runner, two-engine comparison and seeded query generator in the test-support crate,
  with scripts for the declared SQL subset. A nightly job checks the scripts' expected values against
  PostgreSQL 17; no Lattice engine runs them yet.
- A PostgreSQL wire protocol v3 message codec: strict, bounded decoding of the startup phase and simple
  queries, and encoding of the server messages that answer them. It does no I/O, has no session state and
  never panics on any input; a fuzz target runs in the nightly and weekly fuzz matrices.
- Version vacuum for the row store (`RowStore::vacuum`, P2-19): drops row and index versions no snapshot at
  or after a horizon can see, keeps the newest version at the horizon, refuses transactions older than the
  floor with `snapshot too old`, and leaves the log (and so recovery) untouched. A planted bug and a seeded
  workload check it against a model.
- `AS OF` on the row store: a snapshot at a change set id or at a timestamp, rebuilt on recovery.
- A sharded weekly mutation campaign over the core, LSL, evalkit, simulator, seams, CLI and bench crates
  (70 shards of about 24 mutants). Its first full run found 230 survivors; the real ones were closed with
  targeted tests, and measurement programs and the opt-in io_uring experiment are excluded with their reasons
  in `.cargo/mutants.toml`.
- `lattice ledger check` reports a trend over the last seven simulation runs and warns when the unverified
  share is rising.
- Validated mutation and eval scorecard observations, with explicit tested/planned coverage, mutation
  denominator exclusions, exact eval gate fields and retained weekly producer provenance. Legacy eval
  reports without complete thresholds remain unverified.

- Unsigned scorecards with ten observed-or-missing sections, bounded ledger JSON interchange,
  retained nightly producer provenance and completed-fuzz summaries. Benchmark ledger entries now
  include the actual comparison verdict; missing measures remain explicit.

- Ten distinct storage defect witnesses with independent semantic oracles, clean controls and guard
  cleanup checks; the existing four-defect randomized workload remains unchanged.

- Real-process NS1 row recovery checks with observed commit receipts, complete-batch history oracles,
  index/log scrubbers and a second recovery after eight writer terminations.

- Phase review evidence packages from `cargo xtask gate`, with declared criteria, source provenance,
  invariant traceability and optional ledger diagnostics, without inferring or signing a gate pass.

- A row/index fault-history oracle covering all five disk profiles, actual injected-fault quotas,
  deterministic replay and exhaustive rotation/recovery cut points. Rot-censored histories are
  reported separately from verified histories; this does not claim the Phase 2 soak gate.

- Typed-row layouts and a canonical codec with an overflow area for fields unknown to the encoding
  layout. Decoding requires the original layout; catalog integration and schema migration remain open.

- Secondary index primitives derived from row writes, rebuilt during recovery and readable at old
  snapshots, with consistency checks and a planted stale-index defect to verify the checker.

- A bounded PostgreSQL development TCP listener for `latticed`, driving simple/extended sessions
  with a `SELECT 1` placeholder, frame deadlines and explicit shutdown. It uses plaintext/trust auth
  and does not open a database.
- Bounded cancel-key registration and per-query tokens, plus row-policy hooks, seam-clock deadlines
  and live-memory reservations. Deadline/memory enforcement and query abort/rollback remain executor work.
- **Breaking (engine API):** snapshot reads require an explicit policy evaluator, and row transactions
  bind their evaluator at creation. Point reads, scans, index lookups and transaction-local reads hide
  denied rows; a denied final write transition rejects the whole batch before WAL writes or RNG use.
  `begin()` retains the explicit Phase 2 allow-all path. Evaluators come from a trusted host, not an
  authenticated SQL session; recovery and consistency checks retain the complete authoritative state.
- **Breaking (engine API):** snapshot reads now return errors below the vacuum floor, including
  transaction-local reads and index lookups. Vacuum prunes derived commit metadata; timestamp
  resolution clamps to the floor and threshold maintenance respects retention/oldest-open inputs.
  WAL history and kernel deduplication state are retained.
- Agent plugin contract v1 (`formats/plugin-contract-v1.md`, draft): line-delimited JSON for session
  identity by source and external id, idempotent create, append receipts (`recorded`, `duplicate`,
  `fenced`), resumable watch streams and turn events, with 13 worked transcripts. `lattice-sdk` gains a
  strict, bounded codec for it, a conformance suite with a reference plugin and one broken plugin per rule,
  and a `plugin_jsonl` fuzz target (not yet in the scheduled fuzz matrices). No plugin talks to a server yet.
- `lattice_sdk::client`: sans-I/O sync client state machine (handshake and version negotiation,
  subscribe, batched acks, resume without gaps or duplicates, logical-clock ping timeouts).
- `lattice-testkit::convergence`: N-replica (2 to 5) convergence harness for the register, set and tree CRDTs
  over a simulated network driven by the five fault profile names, with a checker for identical digests, no
  lost acknowledged write and an acyclic tree, a seed sweep, an ignored 100,000-seed volume test and planted
  defects in harness replicas. Not yet in the nightly shards.
- `lattice_api::subscribe` (WP37, #239): sans-I/O server subscription host over an abstract
  `EventSource` (Hello/Welcome, scoped subscribe with replay then live events in order, acks, ping
  timers, `invalid_scope` and `invalid_cursor` errors). Simulation with random disconnects, restarts
  and duplicate frames shows no gap or duplicate against an independent oracle. Not wired to a
  socket or the change feed yet.
- `lattice-core` tree CRDT: `Tree::compact(stable)` drops applied ops at or below a causal-stability
  frontier (contract stated in the module docs), `Tree::with_op_limit` caps the live op count and a full log
  returns `TreeError::OpLimitReached` instead of dropping anything (default cap `DEFAULT_OP_LIMIT`). The 41-byte op
  encoding is unchanged. A reserve of the cap is kept for `Tree::apply_local`; `Outcome::BelowFrontier` and
  `below_frontier_arrivals()` flag arrivals at or below the frontier; `compact` returns a `Result` and refuses a
  frontier beyond every known id. Not yet wired to sync; computing the frontier is the caller's job (issue 308).

### Changed

- **Breaking (core API):** `Kernel::open` and `Log::open` no longer return every record. Recovery now holds
  the kernel's derived state and one segment at a time. Use `read_from` to read records, or
  `open_streaming` to fold state during recovery, as the row store and the ledger do.
- Pull request gate: the whole gate runs on Ubuntu, ARM runs the portable tests without the two slowest
  simulation tests, and macOS runs after a merge instead of before it (its runners queue for minutes).
  Superseded runs of one pull request are cancelled.
- Optimize test builds of workspace crates so the deterministic simulator runs faster in every
  platform gate, retaining debug assertions, overflow checks, test cases and seed budgets.

### Fixed

- Serial merger resolves unambiguous same-workflow check retries instead of treating superseded
  cancelled/failed runs as current; ambiguous history and the latest pending/failure still block.

- Keep CLI test process launches separate from in-process ledger writes, preventing inherited WAL
  locks from making parallel macOS tests fail intermittently.
- Restore the catalog test lint gate and retain a separate CI run for each push to `main`, while
  superseded pull request runs can still be cancelled.
- `lattice-sql`: the print-then-parse round trip holds for long NOT and operator chains (call depth is limited separately from tree depth, which rises to 256, so 64+ term AND/OR/`||`/`+` chains parse); ISNULL, NOTNULL and the `current_*`, `session_user` and `localtime*` words are refused as unsupported; `$1abc` and chained comparisons are errors; the earliest error in the source is reported; DROP .. CASCADE, CREATE INDEX CONCURRENTLY, BETWEEN SYMMETRIC and OFFSET .. ROWS are named as unsupported.
- latticed: a failed accept or per-connection setup error drops that socket and the listener keeps
  accepting; cancel keys are registered only after authentication; cancel PIDs wrap and skip active
  ones instead of exhausting; TLS handshake starts are also limited per client address; startup
  packets are capped at 10000 bytes. Behaviour change: a client that connects and sends no
  startup packet for 5 s is now dropped (it was the read timeout, 30 s). Persistent accept errors end
  the listener after about ten seconds and the binary then exits instead of hanging on stdin. The read timeout is documented
  as the idle timeout and `--help` states that stdin EOF stops the server.

- Evidence checks fail closed: the SQL reference check fails on an empty script set, needs a `select 1`
  preflight and tells a connection failure from a query error; nightly failure reporting now covers
  the scorecard job and cancelled or lost-runner jobs; the SQL reference PostgreSQL image and the planner
  experiment's `psycopg` install are pinned (digest, hashes) and fuzz corpus cache keys are scoped to
  the branch. Same-repository and review-state checks are documented as owner procedure for the merger.

## [0.1.0-alpha.0]

Not published. This is the version under development; the first published build will be
`0.1.0-alpha.1`, cut when the Phase 1 gate is met.

### Added

- Deterministic simulation with storage and network fault injection, and simulator checks for the invariants that apply at the log level (I1, I2, I4, I6, I9, I12). The rest (I3, I5, I7, I8, I10, I11, I13, I14) have tests or belong to later phases; see the traceability matrix.
- Write-ahead log (format v1) with group commit, segment rotation, torn-tail recovery and a bounded
  commit pipeline with backpressure.
- Kernel with typestate transactions, change set dedupe, turn fencing, hybrid logical clock
  timestamps, checkpoints verified on recovery, and a reader cursor over the durable log.
- Read-only log scrubber and `lattice doctor`.
- Results ledger stored in Lattice's own log, `lattice ledger show` and `cargo xtask scorecard`.
- Schema language (LSL): parser, checker, `lattice lsl check|fmt|compat`, reversible migration lenses.
- CRDT building blocks with convergence tests over a faulty network.
- 21 planted bugs the simulator must detect; mutation testing of the core crates.
- TLA+ models for commit, hybrid logical clock, turn fencing and set convergence.
- Fuzz targets for the decoders, the schema parser and log recovery.
- Release pipeline: nightly pre-release, tagged releases, archives for Linux x86_64 and arm64 and
  macOS arm64, checksums, SBOM, build provenance and a container image on GHCR.

### Fixed

- Transactions from another kernel could be promoted when kernels shared an RNG seed.
- A checkpoint taken with an unapplied change set made recovery reject the log.
- `read_from` returned a short history instead of an error when the log was damaged, or when its leading segment was missing.
- A kernel with the same seed and first id could still take another kernel's transaction; in-flight transactions are now matched by id, position and content checksum.
- `lattice ledger check` read a fractional count as its integer prefix and could overflow on a huge one.
- Nightly could publish a different commit than the one it tested; one resolved commit is now used throughout, and the container image waits for the release.

[Unreleased]: https://github.com/moncifem/latticedb/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.0]: https://github.com/moncifem/latticedb/commits/main
