# Changelog

All notable changes are recorded here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0 the on-disk format and APIs can change
between minor versions; format changes are called out explicitly.

## [Unreleased]

## [0.1.0-alpha.1] - 2026-10-30

Pre-release for testing. Test data only: do not store anything you cannot lose. Formats and APIs are
not frozen, so a data directory may not open in a later alpha. This release makes no phase gate claim;
the Phase 1 gate is still open.

Known gaps, with their issues:

- Permissions are table-level grants only, and the filtered change feed reveals the number of hidden writes (#363).
- A change-feed read is not yet bounded in cost (#543).
- A numeric column default loses its trailing zeros: `DEFAULT -2.50` reads back as `-2.5` (#706).
- `COMMIT` or `ROLLBACK` outside a transaction block, and `BEGIN` inside one, give no PostgreSQL warning (#674). An unknown type name answers `0A000` instead of `42704` (#675).
- The sync gateway has no TLS yet, so it serves loopback only until TLS exists on it.
- The MCP transport is not shipped.
- Bearer tokens are reloaded from their file within 5 seconds of a change, or on SIGHUP. SCRAM verifiers and account policies are read only at start, so changing them needs a restart.
- The Python driver smoke test still uses psycopg; moving it to a permissively licensed driver is tracked in #710.

Joins, aggregates, `GROUP BY`, `DISTINCT` and subqueries are not supported in this alpha.

### Added

- `RowStore::events_since` and `events_since_as` (WP36, #238): the change feed emitted from committed row
  changes in commit order, resumable from a cursor, filtered by the actor's read policy, identical after
  reopen, and refused with `CursorTooOld` at or below the vacuum floor. Read-only accessors
  `RowStore::history_from` and `Table::history_from`. No exit criterion is claimed.
- `lattice-api::backpressure` (WP38, #240): bounded per-subscriber outbound queues (event count and
  byte budget) that mark a slow subscriber overflowed with a resume cursor instead of blocking the
  producer, and an O(N) fan-out hub sharing one payload buffer. Pure module, not wired into a
  server yet; no exit criterion is claimed.
- `lattice-testkit` fencing race tests (WP46, #248): takeover during append and during watch
  reconnect on the kernel under five fault profiles, judged by a script oracle, with wrapper mutants
  and planted kernel bugs proving each assertion can fail. Test only; no exit criterion is claimed.

- `lattice_core::crdt_text`: operation based sequence CRDT for text (stable element ids, tombstones, parked out-of-order operations, canonical bounded encoding) with tombstone compaction that keeps declared revert points. ADR 0010 gains Phase 3 data; the Phase 1 decision is provisionally kept and E3-6 is not claimed.
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
- `lattice-sdk::mcp` (WP52, #254): sans-I/O MCP server v0 targeting protocol revision 2026-07-28 (legacy
  `initialize` also answered on a connection that opens with it; `_meta` required otherwise): bounded JSON-RPC codec, `lattice_read`, `lattice_write` and
  `lattice_subscribe` over an abstract `ToolBackend`, schema validation and a caller grant and scope layer
  with stable error codes; fuzz target `mcp_message` (not yet in a scheduled matrix). No transport,
  resources, prompts or authentication; see `docs/mcp-v0.md`.

- Kani proof harnesses (`cfg(kani)`, in `proofs.rs` files) for the change feed cursor, the sync frame decoder, the plugin contract line splitter and the tree CRDT cycle check, a weekly `kani` workflow, and `docs/verification.md` with the bound of each proof. The harnesses have not been executed yet.

- Public API snapshots for the ten library crates (`api-snapshots/`, `tools/check_public_api.sh`, a `public-api` job in `pr.yml`): a public API change now shows as a snapshot diff in review. See `docs/api-snapshots.md`.

- CI: every fuzz target is in the nightly, weekly and 24-hour matrices, checked by `tools/check_fuzz_matrices.py` together with a `cargo check` of the fuzz crate in the pull request gate. A weekly dispatch can add `engines` and `edge` mutation groups (engines, sql, api, testkit, sys, latticed) and a nightly dispatch can run the `lattice-sql` properties at 1,000,000 cases; neither is scheduled until a run has measured it. `cargo-fuzz`, `cargo-deny` and the nightly toolchain date are pinned.

- Dependency policy (#303 item 10): `docs/dependency-policy.md` sets the checklist for adding a
  dependency (licence, maintainers, `unsafe`, build scripts, footprint) and the rules and audit-note
  template for the security-critical set (`rustls`, `ring`, `subtle`, `sha2`, `pbkdf2`, `hmac`). The root
  `Cargo.toml`, `Cargo.lock` and `deny.toml` are now code-owner paths and a `dependencies` risk area for the
  `risk-gate` check. No audits are recorded yet.

- OpenSSF Scorecard workflow (#303 item 1): weekly and on pushes to `main`, SHA-pinned action, least-privilege
  permissions, results published to the Scorecard API and uploaded to the code scanning tab. It is not a
  pull request or required check. The README badge is added after the first successful run; see
  `docs/policies/openssf-scorecard.md`.

- zizmor workflow linter (#303 item 2): a `zizmor` workflow audits `.github/workflows` on every workflow
  change and weekly, with the version and hashes pinned. The first run's 54 findings are triaged in
  `docs/policies/workflow-security.md`: `persist-credentials: false` on every checkout, template
  expansions moved to `env:`, no Rust cache in tagged release builds, action version comments made exact.

- Documentation site and wiki mirror (#311): `tools/docs_summary.py` stages `docs/` (plus the README,
  CONTRIBUTING and `formats/`) with a generated `SUMMARY.md` and fails on a broken relative link; the
  `docs-site` workflow builds it with a pinned mdBook on every pull request that touches the docs and deploys
  to GitHub Pages from `main`; the `wiki-sync` workflow regenerates a small read-only wiki from the same
  sources. New `docs/glossary.md`. The site and the wiki stay dormant until the owner enables Pages and
  creates the first wiki page; see `docs/site-setup.md`.

- Changelog fragments (#315): each pull request adds one file under `changelog.d/` instead of editing
  `CHANGELOG.md`, so parallel pull requests no longer conflict there. `tools/changelog.py` validates the
  fragments in a new `changelog` check (advisory, in `pr-governance`) and compiles them into the release
  section. Entries already in `[Unreleased]` stay and are carried into the next release by the same step.

- `lattice-bench` example `phase3_bench` (WP53, #255, first slice): fixed-seed benchmarks for tree CRDT apply, set and register merge, change-feed and sync frame codecs and fan-out to 1, 100 and 10,000 subscriber queues, with a local baseline and a smoke test. No numbers recorded; E3-4 stays open.

- SQL logical plan IR and binder (WP27, #119, P2-10 foundation): `lattice-sql` binds single-table `SELECT`, `INSERT`, `UPDATE` and `DELETE` against an abstract `CatalogView` into a typed plan that carries table ids and the catalog version. `$n` placeholders get types and bind errors carry PostgreSQL SQLSTATEs (`42P01`, `42703`, `42804`, `22003`, `23502`). Joins, aggregates and DDL are refused with `0A000`, and nothing executes yet. ADR 0025 (accepted by the tech lead under the owner's delegation) builds the planner in house with a scope freeze, the differential harness as semantics authority and a numeric reopen trigger; ADR 0013 stays proposed.

- `lattice_core::workflow` (WP47, #249, P3-09): a pure, server-arbitrated workflow state machine. Transitions are accepted, rejected with a stable reason code, or recorded in a deterministic `Conflict` entity (sorted proposals, resolution options, canonical bounded encoding) that `resolve` closes; requests are idempotent by `(actor, id)`, versioned against ABA, resolution is permissioned and history is bounded; draft encoding in `formats/workflow-v1.md`. Pure library; no log, sync or SQL wiring yet, and no exit criterion is claimed.

- Offline outbox for the SDK (`lattice_sdk::outbox`, WP49, #251): queues writes with stable idempotent record ids, bounded by count and bytes, retries with logical-clock backoff and caller jitter, clears `recorded` and `duplicate` receipts, drops fenced turns to the application, and stores itself in a versioned, canonical, checksummed format. Crash and restart property tests and the `outbox_decode` fuzz target are added; the target is registered in the nightly, weekly and 24-hour fuzz matrices, and no exit criterion is claimed.

- `lattice-testkit` sqllogictest corpus grows from 5 to 21 scripts (WP29, #121): NULL logic, integer overflow and division errors, string order, `ORDER BY` NULL placement and ties, `LIMIT`/`OFFSET` edges, empty aggregates, joins, `RETURNING`, constraints, DDL, types, subqueries, one-session transactions, prepared-statement refusals and a script of every `0A000` and `42601` refusal. `tools/slt_reference.py` skips records marked `# differs-from-postgres` and compares single NULL results correctly. Test only; no Phase 2 exit criterion is claimed.

- `docs/engineering-retrospectives.md`: a process retrospective of the multi-agent workflow (changelog
  conflicts, the `reviewed` label queue, vacuous CI evidence, formats merged as drafts), each with its fix
  and a link. Follow-ups are in #330.

- First threat model and security test plan (`docs/threat-model.md`, `docs/security-test-plan.md`): assets,
  actors, a trust-boundary diagram, STRIDE threats per boundary with the mitigations that exist on `main`
  (file and line links) and the tests that cover them, a prioritised gap register (P1 to P3), explicit
  non-goals and assumptions, and a check of `SECURITY.md` against what the repository supports. Twelve new
  issues (#357 to #368) track the P1 and P2 gaps that had no issue. First draft; no second maintainer has
  reviewed it and no phase exit criterion is claimed.

- `clients/typescript` (WP51, #253, first slice): an experimental TypeScript client with no runtime dependencies. It ports
  the sync frame codec, the change feed cursor and event codec and the client state machine, and checks them against
  `conformance/vectors.json`, which a new Rust test in `lattice-api` renders and requires to be current
  (`tools/update-conformance-vectors.sh` rewrites it). A `clients-typescript` workflow builds and tests the package.
  No transport, no Go or Python client and no generator yet; the formats stay unaccepted candidates.

- Column-level CRDT merge (WP42, #244, part 1; RFC 0011 draft): `lattice-engines::merge` merges remote row changes per column with a
  last-writer-wins register, an observed-remove set or whole-row last-writer-wins for plain columns, and a row delete that
  hides writes stamped at or before it. It is an in-memory function with no byte encoding, not yet wired to the row store or sync.

- CRDT peer limits (threat-model gap G-08, issue 364): `PeerContext` and the `Doc::apply_from`, `Doc::apply_batch_from`, `Tree::apply_from`, `Tree::apply_batch_from`, `LwwRegister::set_from`, `OrSet::add_from` and `OrSet::merge_from` entry points refuse a forged node id, an id past the clock drift bound, a batch over 1024 operations and a set past 2^20 tags before changing state. They accept only a peer's own operations, so a relay forwarding other replicas' operations must use the trusting `apply` path, and quotas are charged per direct peer. The text document caps inserts per quota key (`Limits::max_inserts_per_key`) and the tree caps nodes, depth (a deeper move is a no-op everywhere, checked in O(depth) from cached subtree heights, `Tree::with_max_depth` lowers it) and each origin's share of its op log (always for `Tree::apply_from`, only after `with_origin_quota` for `Tree::apply`). A refusal by a cap is local to the replica: the operation is lost unless it is sent again, so replicas can stay different until resynced. `tla/TreeMove.tla` models the depth rule. Eight planted bugs cover the limits. No sync layer calls the new entry points yet and the compaction frontier is still trusted. Breaking for source: `Limits` gained a public field and `TextError`, `TreeError` gained variants, so struct literals and exhaustive matches outside the crate need updating.

- Authenticated principal (#357, ADR 0026, proposed): a connection now has a `Principal`, set once when
  authentication completes and never changed. SCRAM names it from the credential store's own account key
  (not the client's `user` string); loopback trust mode uses the fixed, labeled `local-development`
  principal (`Session::local_trust`, which replaces `Session::new`). `Accounts::exchange` returns
  `Option<Principal>` instead of a bool, `Session::with_scram` takes it, and the session refuses a
  repeated startup `user`, and fails a `user` that is not the exchange's account at the end of the
  exchange like an unknown account (`28P01` either way; the listener already refuses a repeated `user`
  with `28P01`). Every `QueryHandler` method now takes a
  `&QueryContext` carrying the principal, so this is a breaking change for handler implementors. The row
  policy is not yet selected from the principal (the executor change does that), and a `CancelRequest`
  still authenticates by pid and secret alone.

- `clients/python` (WP51, #253, second slice): an experimental Python client (Python 3.10 or later, standard library only, typed). It ports
  the sync frame codec, the change feed cursor and event codec and the client state machine, and replays every vector in
  `conformance/vectors.json` (shared with the TypeScript client, not regenerated) byte for byte with the same error kinds as Rust.
  A `clients-python` workflow runs the tests on Python 3.10 and 3.13. No transport, no packaging, no Go client and no generator yet;
  the formats stay unaccepted candidates.

- A sans-I/O WebSocket (RFC 6455) server-side codec for the sync gateway, `lattice_api::websocket`: strict bounded upgrade request parsing with an origin hook and the `lattice.sync.v1` subprotocol, the accept key, frame decode and encode with masking, length, reserved-bit, size and UTF-8 rules, message reassembly, the close handshake and ping and close timers on a caller-supplied clock. Adds the `sha1` dependency and the `websocket_frame` fuzz target. No listener, TLS or authentication wiring yet, so the sync gateway itself (issue #242) is still open; ADR 0027 (proposed) records WebSocket first and the data rule for adopting QUIC.

- Merge queue and auto-rebase tools: `tools/merge_queue.py` merges a batch of ready pull requests one at a time and parks the ones that fail, and `tools/rebase_pr.py` rebases a pull request branch in a private worktree, resolving only prose list conflicts. `tools/merge-pr.sh` stays for single, human-driven merges.

- `phase3_bench` now also measures tree compaction (late ops before and after it), the text CRDT (apply and compact), fan-out through the real backpressure `Hub` and a reconnect storm against `SubscriptionHost`, records the CPU model, and the manual `bench-baseline` workflow runs it. Metadata schema 2 changes the fan-out and register-merge cases, so older local baselines must be re-recorded. No numbers are recorded; E3-4 stays open.

- CI now runs the Python unit tests under `tools/` on pull requests that touch `tools/` or the workflows and on pushes to `main`, Dependabot waits seven days before proposing a new release for every ecosystem, and the changelog check explains how to remove a leading `- ` from a fragment (`check --fix-bullets` does it in the working tree).

- `lattice-exec` (#119, #121, #401): a deterministic, bounded executor for the single-table SQL subset. Bound
  `SELECT`, `INSERT`, `UPDATE` and `DELETE` (with `RETURNING` and `AS OF`) run against snapshot-isolated
  row-store transactions with `BEGIN`, `COMMIT`, `ROLLBACK`, PostgreSQL command tags, `23502`, `23505`,
  `22003`, `22012` and `40001` errors, stale-plan refusal, the actor's row policy and per-statement
  deadline, memory and cancellation limits. `CREATE TABLE` and `DROP TABLE` register tables; column
  definitions are in memory only. A `QueryHandler` adapter is in `lattice-api` and is not wired into
  `latticed`. Of the 669 records in the 21 sqllogictest
  scripts, 433 pass; the 236 failures are listed with reasons (aggregates, joins, `GROUP BY`, `DISTINCT`,
  subqueries, `SELECT` without `FROM` and DDL the binder or executor refuses, and `numeric(p, s)` modifiers).
  No exit criterion is claimed.

- `latticed --data-dir DIR` serves the single-table SQL executor over the existing pgwire/TLS/SCRAM listener (#345, #412): it opens and replays a write-ahead log in DIR (created mode 0700; an existing one must be 0700), runs the database on one thread behind a new `lattice_api::exec_host`, applies an explicit row policy per principal (`--development-policy` in trust mode, `--account-policies` in SCRAM mode, unlisted accounts denied, no allow-all default) and a statement timeout ceiling (`--statement-timeout-max-ms`, also the longest a connection waits for the database thread, plus 5 s). A `deny-all` principal also gets an empty catalog (`ActorPolicy::hiding_catalog` in `lattice-exec`): existing tables answer `42P01` like missing ones, including through `Parse`/`Describe`. A dead database thread answers `57P01` and ends the server with an error. Committed rows and table names are recovered after a restart or crash; column definitions are not yet (#379), so a recovered table cannot be queried. Without `--data-dir` the `SELECT 1` placeholder is unchanged. Not done: the driver matrix, binary result formats and a persisted schema.

- Durable SQL table definitions (issue 379): RFC 0012 and ADR 0028 are accepted by the owner (Option A). Each table's columns, types, constraints and defaults will be recorded in the log, in the same change set as the table, behind a new required WAL capability so older readers refuse the directory; existing directories need an explicit format-upgrade setting. Nothing is implemented yet, no bytes change, and merging the code that writes the format waits for the seven-day comment period (ends 2026-11-06) or an owner waiver.

- `lattice-exec` (#381): a scan under `WHERE` now reads only a key range, a single row or a `UNIQUE` constraint entry when the top-level `AND` terms compare the primary key or a `UNIQUE` column with constants of the column's exact type; `UPDATE` and `DELETE` use the same paths. The whole predicate is still evaluated and the path is chosen only when no term can fail, so rows, order and errors equal a full scan, under `AS OF`, open transactions and row policy. `Database::last_access_paths` reports the choice, two planted bugs (`Bug::EXEC_INDEX`) cover the bound and the policy, and a differential property test compares narrowed and full scans. The executor still creates no managed secondary index (`CREATE INDEX` is `0A000`), so `IN` lists, `ORDER BY` from key order and `LIMIT` early stop are open.

- `SELECT` without `FROM` (`SELECT 1 + 2`, with `WHERE`, `LIMIT` and `OFFSET`) runs over a one-row source in the plan IR and the executor, which lets 53 more sqllogictest records run (486 of 669 pass). `date` and `timestamptz` text that is shaped like a value but names a field that does not exist (`2023-02-29`, `25:00:00`, `24:30:00`) now returns `22008` as PostgreSQL does instead of `22007`. The records in `slt_expected_failures.txt` that depend on an earlier refused statement now name the SQLSTATE they get, and the test asserts it.

- The deterministic simulator can now lose power in shapes that are not a prefix of the unsynced bytes: pages persisted out of order, zero-filled tails and a zero-filled new segment header (`sim_runner --profile non-prefix`, now part of the nightly simulation). Tests pin what recovery does for each shape and the log format document states it. Recovery is unchanged: such a power loss can leave a log that refuses to open until an operator follows the documented procedure, without losing acknowledged data. The nightly result of the new profile is not yet recorded.

- Row commits now record who wrote them. The change set's existing author field carries an id for the authenticated principal (an injective, never-reissued number per account, kept in `authors.reg` beside the log; 1 for local development; 0 for unknown or older history), and change-feed events from the row store repeat it. The id never comes from SQL text, `application_name` or the startup user. No log or feed bytes change. Startup refuses an `authors.reg` that is missing or older than the log. Not done: turn is still not recorded, and `authors.reg` is an experimental local file with no operator command for rename or delete.

- Commit notifier: `ExecHost::commit_notifier` returns a `CommitNotifier` that the database thread updates with the newest committed LSN after every statement, so subscribers can wait for a commit (`wait_after`, with a timeout as the fallback and a bounded number of sleepers) instead of polling on a timer. It carries only an LSN, wakes every waiter with `Closed` on shutdown or when the database thread ends, and is checked by two planted bugs (#523).

- Sync gateway over WebSocket (#524, part 1): `latticed --data-dir DIR --gateway-listen 127.0.0.1:PORT` serves the sync protocol on `/sync` (subprotocol `lattice.sync.v1`, one sync frame per binary message). With `--gateway-tokens FILE` an upgrade must carry exactly one `Authorization: Bearer` token from the credential file and the connection is bound to that account; without it only the loopback development principal is served, and only with `--auth development-trust`. A token in the URL, a second `Authorization` header, a wrong origin or subprotocol, an oversize head and a slow upgrade are refused, failed attempts are rate limited per address, connections and upgrades are capped, reads wake on the commit notifier and go through the policy-checked feed source, and a shutdown or a credential replacement closes live connections (1001, 1008). A non-loopback gateway address is refused at start until TLS exists on it (#578). Five planted bugs (`Bug::GATEWAY`) are caught by unit witnesses.

- Design note for live agent collaboration, the first product slice (`docs/design/live-collaboration.md`): the end-to-end flow of two agents sharing a live feed, what exists on `main` and what is missing for each component, the packages in dependency order (tracking issue 518), the risks and a demo script. It changes no code and accepts no format; nothing in it is implemented yet.

- The sync subscription host has a real data source: `lattice-api::feed_source::FeedSource` (from `ExecHost::feed_source`) serves `table/<name>` from the row store's change feed through the executor thread, as the connecting principal's policy with deny by default. A denied table is indistinguishable from an unserved one, a cursor below the vacuum floor is `TooOld`, and a legacy oversize key is skipped without losing or repeating events. Not connected to a transport yet.

- Quickstart smoke test: a `quickstart-smoke` workflow builds `latticed`, runs the SQL from the README "Try it" section through `psql`, stops the server with `shutdown`, restarts it on the same data directory and checks the documented restart behaviour (#379), so the README and the binary cannot drift apart unnoticed.

- WAL required capability bit 3 (`SQL_DEFINITIONS`, value 8) is defined in the guarded segment header and known to `lattice-core`, with independent header fixtures for masks 8 and 9 and a planted bug for a dropped bit. Nothing writes the bit yet, and every earlier reader refuses a directory that carries it; RFC 0012 is still in its comment period.

- The wiki mirror now has pages for getting started, the architecture layers, the SQL subset, agents and live collaboration, the security model and verification, generated from `docs/wiki/` and the reviewed documents. The wiki repository itself is still created by the owner (see `docs/site-setup.md`).

- Durable SQL table definitions in the engine (RFC 0012, ADR 0028, issue 572): `LCIX` version 3 carries a definition block in the same change set as the `Create`, `RowStore::commit_with_definitions` installs WAL capability bit 3 with it so older builds refuse the directory before any callback or repair, recovery validates the block (table live, one definition per table, structure) and `Catalog` and `CatalogStore` expose the definitions, kept for `AS OF` reads after a drop. The first definition in a directory that already holds records needs the explicit `RowStore::set_allow_format_upgrade(true)`; there is no downgrade. Public API additions: `DdlRef::definitions`, `CompoundChange::definitions`, `Catalog::check_compound` and `apply_compound`, `RowError::FormatUpgradeRequired`, `Kernel::required_capabilities`; adding fields to `DdlRef` and `CompoundChange` is a breaking change for code that builds them with struct literals. Independent LCIX version 3 vectors in `formats/corpus/catalog-index-v3-*`, seven planted bugs (group `Bug::SQL_DEFINITIONS`), a power-cut sweep over table creation and the fuzz target `sqldef_decode` now also decodes whole envelopes. The executor still keeps definitions in memory, and the commit-time liveness check for row writes, the catalog caps and vacuum pruning of definitions are not done. Breaking for exhaustive matches: new variants `RowError::FormatUpgradeRequired`, `CatalogError::AlreadyDefined`, `CatalogError::DefinitionNotInCreate` and `CatalogError::BadDefinition`. A definition must come with the `Create` of its table in the same change set; definition-only adoption of an existing table is not implemented and is refused. `TableGrantPolicy::authorize_ddl` judges each definition by the created name or the existing id.

- Tables and their columns now survive a restart (RFC 0012, ADR 0028, issue 573). `CREATE TABLE` writes the table definition (column names, types with `varchar` length, `NOT NULL`, primary key, `UNIQUE` constraints in declaration order, constant defaults, constraint names) in the same change set as the table, and `Database::open` rebuilds the executor's schema from the definitions recovered from the log, so typed columns, constraints and the `UNIQUE` marker tables come from the durable record. `Database::attach_table`, `detach_table`, the in-process fingerprint, `ExecHost::unattached_tables` and the `recovered table(s) have no column definitions` message of `latticed` are removed. A data directory written before definitions existed refuses its first `CREATE TABLE` with `55000` until `latticed --allow-format-upgrade on` (`ExecHost::start_dir_with`, `Database::set_allow_format_upgrade`); older builds cannot open the directory afterwards and there is no downgrade, so take a backup first. A new directory needs nothing. Limits: a column name with an ASCII capital (a quoted identifier) is refused with `0A000` because the record format forbids it, a numeric default is kept without trailing zeros (`1.50` reads `1.5`), `DEFAULT NULL` is the same as no default, a table created by an earlier build keeps its name but has no definition and stays unaddressable, and `ALTER TABLE` is not supported. Planted bug `ExecDefinitionNotPersisted` (group `Bug::EXEC_DEFINITIONS`), a power-cut sweep over `CREATE TABLE`, a reopen step in the executor model test, and the psql restart case moved from known failures to a passing script. Public API: `TableDef` gains `primary_key_name` and `unique_names`, which breaks struct literals.

- psql script suite (#575, `tests/drivers/psql`): checked-in psql scripts with hand-written expected output run in CI (`driver-psql`) against the release-built `latticed --data-dir` over loopback trust and over SCRAM-SHA-256 with TLS and `sslmode=verify-full`, covering DDL, `INSERT`/`UPDATE`/`DELETE`/`SELECT`, transactions with the aborted state, SQLSTATE errors and the extended protocol through `\bind`. `tests/drivers/psql/run.sh <latticed>` runs it locally. The job fails when no script matched or the server did not start, and a self-test shows the runner can fail. Three differences from PostgreSQL are kept as known failures with issues, each pinned to its exact current output (#573 restart, #674 transaction warnings, #675 unknown type). One client only: this is not the driver matrix (#413).

- Smoke scripts for node-postgres and pgx in text mode, run against the real `latticed` by the `driver-smoke` CI job (`tests/drivers/smoke.sh`). Each script runs the same steps, including parameters, NULL, transactions, an error with its SQLSTATE and a clean close, and is checked against hand-written expected output; the driver versions are pinned by lockfiles. One case is pinned as a known failure, not hidden: pgx's default query mode, which fails because the server describes no parameter types (#703). Binary formats, JDBC, a Python driver and the driver matrix remain open (#413, #608, #710).

- `latticed --gateway-tokens FILE` now re-reads the bearer-token file within 5 seconds of a change and on SIGHUP (issue 577, ADR 0031), so `lattice account token revoke` and `lattice account remove` close the live gateway connections of the revoked tokens without a restart, and a rotated token's overlap ends when its expiry passes. A file that does not load keeps the tokens in use, and after 10 minutes of failed loads no token works. Not reloaded: SCRAM verifiers, the decoy secret and account policies (restart after those changes). Not done: the Windows ACL check (the file is refused there) and the security event log (a reload or failed load is one stderr line).

- MCP `lattice_subscribe` (issue #579): a long poll over the change feed, as the authenticated principal. `after` is the 12-byte feed cursor as 24 lowercase hex digits (absent means the origin), `wait_ms` waits at most 30 000 ms for the first readable event, and the reply holds up to 64 events decoded to their columns, with `next_cursor` and `truncated`. A commit the principal cannot read never moves the cursor, and a cursor ahead of the newest commit reads as caught up. Shutdown ends a waiting call with the backend unavailable. Replies carry no count of unseen changes, and issue #363 is not closed (the timing and quiet-table signals are documented). No transport serves the tools yet (issue #527).

- `lattice account init|add|list|remove|token create|token revoke|token list|token prune` provisions the credential files `latticed` reads: SCRAM-SHA-256 verifiers, the decoy secret, the per-table policy file and the bearer-token file, so no one computes a verifier by hand. Passwords come from stdin (never a terminal) or are generated and shown once, never from arguments; tokens are shown once and stored only as salted hashes; files are written atomically with mode 0600, never overwritten by `init`, and changed under an exclusive directory lock. A running `latticed` sees changes only after a restart (no reload yet). Not done: TLS certificate generation, Windows (refused until the ACL check), and a `latticed` flag for the token file (the gateway).

- Added scripts/install.sh, a POSIX sh installer for the release archives (macOS arm64, Linux x86_64, Linux arm64) that checks SHA256SUMS and, when gh is present, the build provenance attestation, with offline tests and a CI workflow.

- The container image runs as the non-root user `lattice` (uid 10001) with its data in a volume, and its entrypoint runs `latticed --stdin-control off`, so a detached container keeps running and `docker stop` gives a clean stop. With no arguments it prints the flags a server needs and exits 2. The steps to serve SCRAM over TLS from the image, with credentials made by `lattice account` inside it, are in [Run in a container](docs/alpha.md#run-in-a-container). A new `image-smoke` CI job tests the amd64 image; the arm64 image is not run in CI yet.

- Alpha documentation (issues 589 and 590): `docs/alpha.md` with the quickstart scope, running `latticed` in loopback and SCRAM modes, connecting with psql, the status of agent clients (PostgreSQL wire only; no gateway or MCP transport on `main`), experimental parts and known issues; a README install section for the alpha pre-release that applies once the tag exists; a rewritten `SECURITY.md` with the private reporting link, supported versions and disclosed gaps; and an alpha disposition of the threat-model gap rows. Documentation only; no phase gate or exit criterion is claimed.

- SQL table definition codec (`lattice-engines::sqldef`, RFC 0012 work package 2): types, a canonical encoder, a strict bounded decoder and a structural validator for definition version 1, the definition-operation block, independent golden files from `formats/generate_sql_definition_golden.py`, the format document `formats/sql-definition-v1.md`, the fuzz target `sqldef_decode` and the planted bug `SqlDefColumnsSorted`. Nothing calls it yet: LCIX version 3, the WAL capability, the catalog rules and the SQL executor are later work packages, and the format stays proposed until the RFC's comment period ends on 2026-11-06.

- `latticed` can now run as a service (issue 584): SIGTERM and SIGINT stop it with the same clean shutdown as the `shutdown` stdin line (exit status 0), `--stdin-control off` stops it from reading stdin so `docker run -d`, systemd and launchd no longer stop it at end-of-file (the default stays `on`), and `--stop-timeout-ms` (default 30000) or a second signal abandon a stop that does not finish (exit status 2). The crate README has the flag table and systemd and launchd examples, which were not run under those supervisors. Stop-signal handling is one `signal(2)` call in `lattice-sys`; no dependency was added. Not done: Windows, and a signal during recovery still kills the process with the default action.

- Interim per-table grants for accounts: a line in the account policy file can be `account<TAB>tables: notes=rw, logs=r, *=r` (read, write and ddl bits per table, `*` for the rest, most specific entry wins, an unlisted account is denied, a malformed file refuses startup with its line number). A table an account holds no bit on reads as a table that does not exist in SQL and as an unserved scope in the change feed. Table-level only: no row predicates, roles or presence bit; replaced by the Phase 4 policy language.

- Bearer-token authentication, part 1 (issue 521, ADR 0031, approved by the owner): a token format `lat_<id>_<secret>` with a 256-bit server-generated secret, a strict credential file that stores only a salted SHA-256 verifier, a pure verifier that maps a token to the same account principal as SCRAM, a per-address failure limiter, and a checked file loader and atomic writer in `latticed`. Nothing calls them yet: the HTTP and WebSocket endpoint, reload, closing connections on revocation and the credential tool are separate issues.

- MCP tool backend over the SQL executor (issue #526, part 1): `latticed::mcp_backend` serves `lattice_read` and `lattice_write` as the authenticated principal under the table grants, with `expected_version` as a conditional write, row text in a fixed untrusted envelope and the output bounded under 1 MiB; `lattice-api` gains `PrincipalSql`, and `layers.toml` allows `latticed` to depend on `lattice-sdk`. Nothing serves it yet (no transport), `lattice_subscribe` and the 12-byte cursor are issue #579, and tables must have `key`, `value` and `version` columns.

### Changed

- **Breaking (core API):** `Kernel::open` and `Log::open` no longer return every record. Recovery now holds
  the kernel's derived state and one segment at a time. Use `read_from` to read records, or
  `open_streaming` to fold state during recovery, as the row store and the ledger do.
- Pull request gate: the whole gate runs on Ubuntu, ARM runs the portable tests without the two slowest
  simulation tests, and macOS runs after a merge instead of before it (its runners queue for minutes).
  Superseded runs of one pull request are cancelled.
- Optimize test builds of workspace crates so the deterministic simulator runs faster in every
  platform gate, retaining debug assertions, overflow checks, test cases and seed budgets.

- latticed hardening from the network review: a separate authenticated-session idle timeout, an absolute per-response write deadline, best-effort TLS `close_notify` on every connection exit, a mode check on the TLS key and decoy-secret files, and a rate limit on CancelRequests (a resource bound that wrong keys can consume). Channel binding and the `lattice-api` auth items of issue 347 are not done.

- CI: the nightly, weekly and 24-hour fuzz matrices now include `crdt_text_decode`, `crdt_tree_decode` and `conflict_decode` with checked-in seeds, and `tools/check_fuzz_matrices.py` also fails when a fuzz target has no seed files. The SQL reference check against PostgreSQL 17 is defined once in `sql-reference.yml` and called from the nightly workflow as well as from pull requests. No scheduled run of the new targets or of the reusable call has been observed yet.

- The `kani` workflow now has a `verified` job that runs only the four harnesses that passed on CI and an opt-in `extended` job for the six that did not finish, keeps each proof log as a 30 day artifact, and `docs/verification.md` records 4 of 10 harnesses verified and 6 not proven.

- New crate `lattice-proto` (layer 1) holds the sync frame types, codec and WebSocket codec that `lattice-api` and `lattice-sdk` both need; the SDK client no longer mirrors the frame types, and `lattice_api::sync` and `lattice_api::websocket` keep working as re-exports. No wire or behaviour change, ADR 0030 proposed.

- Cancel registry tests: capacity and emptiness, keys that differ from a live key only in the top bit of the pid or secret, and the error text. Two equivalent mutants are excluded with their reasons; this changes no behaviour.

- Wire codec tests: exact length limits in both directions, SASL body and mechanism limits, the initial-response length rules and the error text. No behaviour changes.

- Credential file tests: exact size, account-count and name-length limits, and a complete SCRAM login computed independently, which proves both stored keys decode exactly. No behaviour changes.

- Invariant checker tests: each checker reports under its own id, flags a planted violation and passes a clean log, and recovered records are decoded and counted once. No behaviour changes.

- latticed binary tests: option parsing, refusals, security-file size bounds and the stdin shutdown channel are now checked through the real program. No behaviour changes.

- Listener tests: TLS certificate, key and chain bounds, redacted debug text, and the startup budget applying only until authentication. No behaviour changes.

- Sync subscription host: `SubscriptionHost::new` takes the authenticated `Principal` and `EventSource::read_after` receives it on every read, so a source can refuse a scope per principal (threat model G-03, sync part). Adds the planted bug `SubscribePrincipalDropped`.

- The simulator command line now lives in a tested library function; the sim-runner program prints and exits exactly as before. No behaviour changes.

- Risk gate coverage: the `risk-gate` check now treats whole crates as risky paths (`lattice-api`, `latticed`, `lattice-exec`, `lattice-engines`, `lattice-sql`, `lattice-lsl`, `lattice-sdk`, `lattice-abi`, `lattice-seams`, `lattice-sys`, `lattice-identity`; `lattice-api/README.md` and `backpressure.rs` are exempt), plus fuzz targets, TLA+ models, lint and toolchain configuration, more of the merge and governance tools, and the security and review docs, so a new file or a directory module cannot escape it. `.github/CODEOWNERS` lists the same paths. `tools/merge_queue.py` now skips a pull request without the `reviewed` label unless it is a Dependabot dependency update (exact login, bot flag, `dependabot/` branch, title shape); `--allow-unreviewed` turns that off. A label that survives a later push is a known gap, tracked in #418. This does not review anything by itself.

- Simulator report tests: a profile whose runs are mostly unverified fails and exits with 1; exactly half does not. No behaviour changes.

- Mutation testing no longer reports the loop-bound and interrupted-retry variants of the real file reader's prefix read as survivors: with the bound loosened the extra pass reads into an empty slice and ends the loop, and the interrupted retry cannot be provoked on a regular file. Nothing else about mutation coverage changes.

- Convergence tests: recorded runs, network models and a sweep that must reproduce exactly. No behaviour changes.

- Scenario runner tests: recorded runs across all fault profiles that must reproduce exactly. No behaviour changes.

- Rewrote the README around what Lattice is, what works today, what is missing, phase progress, a quickstart and the verification evidence.

- Documentation audit fixes: the README, roadmap, crate READMEs, Phase 2 and Phase 3 gate pages and the threat model now describe single-table SQL from a data directory (columns not recovered after a restart, #379), the merged MCP module and SDK outbox, and the current state of gaps G-01 to G-03, G-08 and G-10. Threat-model rows not listed in its section 10 were not re-checked and its line anchors are permalinks to the first commit it read.

- Merge and risk gates: `tools/merge-pr.sh` now refuses a pull request without the `reviewed` label (a Dependabot dependency update is exempt) and requires the `risk-gate` check, the same rule as the merge queue through one shared module. The `risk-gate` classifier now also covers what defines or weakens the required gate: `xtask` `main.rs`, `conventions.rs` and `lex.rs`, the typos and secret-scan configs, the mutation scope, the API snapshots, the `Dockerfile`, `lattice-testkit`, `lattice-sim` and the checker, mutation, scorecard and SQL reference tools. `.github/CODEOWNERS` lists the same paths.

- Scheduled fuzz runs now pass an explicit `-timeout`, `-rss_limit_mb` and a per-target `-max_len` taken from `fuzz/settings.toml`, the nightly job keeps and merges a corpus between nights, and the pull request gate fails when a target has no limits or a `fuzz run` omits them. The values are unmeasured choices and no scheduled run with them has completed yet. The 60-minute limit on the nightly fuzz job is a guess; its cold build and the corpus merge have not been timed.

- Dependabot now also tracks the `Dockerfile` base image (`docker`), the four Cargo lockfiles outside the root workspace (`fuzz`, `tools/cancel-loom`, `experiments/sql-planner` and its `bridge-baseline`) and the hash-pinned `tools/requirements-sql-planner.txt`. Every entry runs weekly with a seven day cooldown, and `docs/dependency-policy.md` lists the coverage.

- Property tests: three proptest blocks (the MVCC vacuum model, the unknown-index lookups and the SQL parser depth round trip) now honour PROPTEST_CASES instead of a fixed count, and docs/verification.md proposes which crates join the nightly volume run. No workflow change yet.

- Mutation exclusions for the deliberately broken convergence replicas, with the reason. No behaviour changes.

- The 100,000-seed convergence and fencing volume tests now run in the nightly workflow, and the pull request gate fails when an ignored test is neither run by a workflow nor listed with a reason in `tools/ignored-tests.toml`. No run of the new nightly jobs has been observed, so their run time and result are unrecorded.

- Scenario runner tests: summed sweeps over 100 seeds per fault profile that must reproduce exactly. No behaviour changes.

- Listener configuration tests: each connection and timeout bound is checked on its own, as is the loopback rule for development trust. No behaviour changes.

- Phase 1 gate document: the 2026-10-30 scheduled night is recorded as not counted, with its cause (the Miri job limit).

- Session and SCRAM tests: the startup and SASL phases, Flush, a nested BEGIN after a failure, portal rebinding, and the SCRAM cost bound being enforced before key derivation. No behaviour changes.

- Subscription host tests: configuration bounds, version before and after hello, and the exact scope and cursor length limits. No behaviour changes.

- WebSocket tests: size defaults, error text, control-frame limits, frame header length forms and a one-byte close payload. No behaviour changes.

- Backpressure tests: queue accessors, memory accounting, repeated acknowledgement and the hub counters. No behaviour changes.

- Sync frame error text is pinned by a test; the Kani proof harnesses are excluded from mutation testing because tests never compile them. No behaviour changes.

- Subscription pump tests: exact frame fit, splitting a batch across frames, and each way an event source can break its contract. No behaviour changes.

- The `Commit`, `Fence`, `Hlc` and `SetConvergence` TLA+ models now have negative configs (12 in `tla/negative/`, run by `tla/check-negative.sh`) that remove one protection each and must violate a named invariant, mirroring the kernel's planted bugs. Their properties are stated over observable state (the disk, the log, the history of issued timestamps) instead of restating the action guards, `Fence` now covers a crash and the rebuild of the turn from the log, and the `SetConvergence` header matches its config. TLC has not run the rewritten models yet; the first nightly run is the check.

- MCP parser tests: literals, numbers, escapes, surrogate pairs and every size and nesting limit at its edge. No behaviour changes.

- Outbox tests: configuration and queue limits at their exact edges, the stored form checked field by field, line-size edges and non-canonical receipts. No behaviour changes.

- Sync client tests: configuration bounds, error text, closing, the welcome version check, scope and cursor limits, delivery and acknowledgement edges. No behaviour changes.

- Executor numeric tests: results checked against PostgreSQL 17 for arithmetic, rounding, comparison, casts and magnitude edges. No behaviour changes.

- Linux release and nightly archives are now built on `ubuntu-22.04` and `ubuntu-22.04-arm` (glibc 2.35) instead of the 24.04 runners, so they load on Debian 12 and Ubuntu 22.04 (issue 588). The new `archive_runs_on_debian_12_and_ubuntu_22_04` job runs `lattice --version` and `latticed --version` from each unpacked Linux archive in `debian:12` and `ubuntu:22.04` containers before anything is published, and the measured minimum glibc is printed in the build summary and added to the release notes. The container image is unchanged. Not verified yet: a hosted run of the new job.

- Planted bug registry: `Bug::GROUPS` lists every group of planted bugs, and the tests derive the distinct-discriminant check, the mask bound and the every-variant-in-exactly-one-group check from it, so adding a planted bug no longer edits a hand-kept total (#639).

- RFC 0012 amendment: the SQL definition format now caps a varchar limit at 10,485,760 characters, rejects a unique constraint whose column set equals the primary key or another unique, and states that identifiers have no byte A-Z with other UTF-8 kept as written. Tech-lead proposals flagged to the owner during the comment period (ends 2026-11-06); documents only.

- Executor tests: casts, JSON validity and timestamp edges checked against PostgreSQL 17. No behaviour changes.

- Pinned that a stopped latticed listener refuses new connections.

- Change feed reads are bounded (issues 581 and 543): `RowStore::read_feed_as` returns a page of at most `max_events` events after examining at most `max_scanned` versions, with a resume cursor and a `more` flag, and finds the cursor through an in-memory commit index instead of walking every row, so a caught-up reader examines nothing and a read costs the new versions, not the table. Events are built only for rows of the requested table that the reader's policy allows. `FeedSource` reads in steps that each give the executor back (`FeedSource::with_scan_budget`, default 4096 versions). `RowStore::history_from` and `Table::history_from` are removed from the public API (breaking, alpha); `events_since_as` is now the unbounded form of the same walk. No bytes change. Not done: a per-principal cap on feed work, a wall-clock bound and a per-table index (issue 543 stays open).

- Pinned string function, LIKE and text comparison behaviour against PostgreSQL.

- Pinned ordering, NULL logic and LIMIT behaviour against PostgreSQL.

- Pinned insert, update, delete and DDL behaviour against PostgreSQL.

- Pinned transaction control behaviour against PostgreSQL.

- Pinned boolean, float, integer and bytea expression behaviour against PostgreSQL.

- Pinned RETURNING behaviour for insert, update and delete against PostgreSQL.

- Pinned result column type ids and sizes and the kind of each executor error.

- Pinned column default and public-qualified table name behaviour against PostgreSQL.

- Pinned the key encoding, its ordering and the marker table id layout with unit tests.

- Pinned date_trunc, extract and date cast behaviour against PostgreSQL.

- Pinned that an index scan is not chosen when any wrapped term of the predicate can fail.

- Pinned the expression depth past which a constant no longer narrows a scan.

- Pinned that a bound expression mentioning any column does not narrow a scan.

- Pinned how comparisons with the constant on the left, BETWEEN and long conjunctions choose an access path.

- Pinned arithmetic and LIKE behaviour against PostgreSQL.

- The SQL subset page lists six more known differences from PostgreSQL.

- A numeric column default is stored in canonical form, so `DEFAULT -2.50` reads `-2.5`; PostgreSQL prints `-2.50`. The value is the same. The durable table definition (RFC 0012) stores numeric defaults without trailing zeros, so the written scale is not kept. Recorded as a known difference in #706.

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
- `lattice-core`: the kernel checks the turn fence before the duplicate check, so a superseded turn
  retrying an already stored write is answered fenced, as the plugin contract says (#304). Nothing was
  written before either; only the receipt changes. WAL format and recovery are unchanged. Planted bug
  `Bug::DuplicateBeforeFence` restores the old order.

- Store review fixes (#230): a failure after a durable commit marks the store stale; commit authorizes before the conflict check so a denied actor gets `PermissionDenied` and cannot learn a hidden row was written (a permitted blind insert can still see `Conflict`); `snapshot_as_of_hlc` returns `SnapshotTooOld` below the vacuum floor, with `snapshot_as_of_hlc_clamped` as the explicit opt-in; new `scan_at_limited` and `lookup_at_limited` poll cancellation and the deadline per row and charge the memory budget (plain `scan_at`/`lookup_at` stay unbounded); `Deadline::after` saturates to "never expires" on overflow.

- `RowStore::lookup_at` returns `RowError::NoSuchIndex { index, snapshot }` for an index id that does not exist at the snapshot (before creation, after a drop, or mistyped) instead of an empty result. The row-write and catalog decoders report `BadMagic` for a wrong magic instead of `Truncated`, and managed index definition history is now bounded by the vacuum floor.

- Installing a WAL guard on an empty tail can no longer lose required capability bits it had already made durable if it crashes mid-rewrite (issue 307): the new header is staged in a `<20-digit first lsn>.guard` file in the WAL directory (not a segment; readers ignore it) before the tail is rewritten, and recovery repairs a torn or invalid newest header from it. Format change awaiting owner acceptance of the RFC 0008 amendment; a crash during an upgrade followed by a restart on an older build refuses with `UnsupportedCapabilities`. Breaking engines API: `RowStore::commit_with_catalog` with table or index changes now requires `PolicyEvaluator::authorize_ddl` (denied by default, checked first); `AllowAll`, and so `begin()`, grants it, and `commit_catalog`, `CatalogStore::commit` and `commit_managed` are host-only paths with no actor. Recovery no longer copies the whole catalog per record, though live tables are still cloned per change (issue 234).

- latticed: a connection refused because every worker is busy now receives a best-effort FATAL 53300 ("sorry, too many clients already") instead of a silent close, and a CancelRequest arriving at that point is dropped as before (the peer sees the error, not EOF). `ServerHandle::open_connections()` exposes the tracked worker count.

- `latticed --data-dir` (#345, #416, follow-up to #412): a connection that times out waiting for the database thread now also fails its open transaction block on the database side (`ExecSession::abort_transaction`), so a later `COMMIT` answers `ROLLBACK` and nothing of the block is committed. Before, a statement skipped or finished after its `57014` could still be committed with the rest of the block. A `Parse`/`Describe` that fails inside a block now fails it on the database side too, and a `BEGIN` that runs after its timeout is discarded so both sides agree the client is outside a block. A `57014` after the ceiling means the outcome is unknown for autocommit statements, `COMMIT` and DDL, which are not cancelled once dequeued; the README no longer says a statement ends at the ceiling at the latest, and no longer presents the 0700 directory check as a guarantee. `HostLimits` gains a public field `wait_slack_ms`, so code that builds it with a struct literal must add it (use `..HostLimits::default()`).

- The scheduled nightly Miri job now has a 300-minute limit: the 2026-10-30 run was cancelled at the old 120-minute limit while the engine library tests were still running under the interpreter. The job and its name are unchanged. Whether it finishes inside the new limit is not yet known.

- Nightly Miri job: the hostile-peer CRDT tests now use small limits under Miri (depth, tree width, set tag cap) and skip the two that fill a 2^18 or 2^20 default, so the job finishes again. Native runs and public defaults are unchanged. The Miri duration is confirmed from CI, not locally.

- Recovery now refuses a log whose newest segment ends in a complete, checksum-valid record with a skipped or repeated LSN instead of truncating it as a torn tail; scrub reports it as mid-log damage. Genuine torn tails are still repaired. A log that earlier versions silently shortened this way now fails to open.

- Timing tests: the exec host timeout tests and the latticed shutdown and budget tests no longer race the wall clock on a loaded runner. Set-up statements run without a short ceiling, and elapsed-time bounds sit far below the timeout they distinguish from. No behaviour changes.

- SDK outbox (#448): a batch is now bounded by its encoded `append` line, not only by record count. `poll` stops before the line would pass 1 MiB (counted with the longest request id, session id and turn, so five 256 KiB bodies go out as batches of 3 and 2), `enqueue` refuses with the new `OutboxError::BodyUnsendable` a body whose escaped line cannot fit alone (a 256 KiB body of control characters escapes to about 1.5 MiB), and `from_bytes` refuses a stored queue holding one while the new `from_bytes_lossless` restores the rest, keeps the counter (no record id is reused) and returns the unsendable writes to the application like fenced ones. Before, the server refused the whole line, sent no receipts and the identical batch was resent forever, blocking every write behind it. The wire contract is unchanged, so the conformance vectors are too. New public items: `contract::APPEND_ENVELOPE_MAX_BYTES`, `contract::append_record_len` and `Outbox::from_bytes_lossless`.

- Server: a suspended portal is no longer copied on every `Execute`, so reading a large result one row at a time costs the rows sent. Open portals are charged to a 64 MiB per-connection byte budget, at most 128 exist per connection, and `COMMIT` and `ROLLBACK` destroy them. The budget is soft: a `Bind` that would pass the budget, or a portal's first `Execute` while the connection already holds more than the budget, answers `53200` before anything runs (and fails the open block), while a statement that ran always keeps its result, so retained bytes stay under the budget plus one statement result. Results are fully built before they are counted. Not done: prepared statements and frame buffers are still bounded by count and declared length only, and portals survive the `Sync` of an implicit transaction (threat model G-06, #362).

- SQL expressions are now bounded: `LIKE` polls the statement timeout and cancellation while it matches, and `||` on text or bytea, `lower`/`upper` and a bytea-to-text cast refuse a result over 16 MiB (`54000`) or over the remaining statement memory budget (`53200`) before building it, with a large left operand of `||` reserved while the right one is evaluated, so a concatenation tree can no longer allocate without limit. `LIKE` is still O(text x pattern) in the worst case, as in PostgreSQL.

- `RowStore::lookup_at_limited` now returns `RowError::NoSuchIndex` for an index that does not exist at the snapshot, like `lookup_at`, instead of an empty result. The row store refuses to commit a put whose key is over 4096 bytes (`RowError::KeyTooLong`, SQLSTATE `54000` in SQL), the change feed's key bound, so one oversize key can no longer make the feed fail for every reader. A log from before the rule that already holds such a key affects only readers allowed to read that row: they receive the events before it, then `FeedReadError::KeyTooLarge` naming the cursor to read again from to skip it. In SQL a key over the bound fails at its statement. A value over the feed's bound still stops the feed for everyone (issue 488). Deletes are not bounded. The bytes of the row-write and feed formats do not change.

- Hostile peers can no longer make one tree operation replay the whole log (#450, the admission gap behind #308): `Tree::apply_from` refuses an operation ordered before more than `DEFAULT_PEER_REPLAY_LIMIT` (4096, `Tree::with_peer_replay_limit`) live ops with `TreeError::ReplayTooDeep`, counted before anything is undone, and `Tree::apply_batch_from` charges the ops it undoes to one budget of `MAX_BATCH_REPLAY_OPS` (65,536) per batch (`TreeError::BatchReplayBudget`, resendable). The refusal depends on the replica's own log, so it is replica-local: an honest op behind more than the limit is refused there for good unless the caller applies it through the trusted `Tree::apply` or resyncs; this is documented in the module and the threat model. `OrSet` keeps a running tag count (#451), so `add_from` no longer costs O(distinct values) per add and filling a set is linear instead of quadratic. New planted bug `Bug::TreeReplayBudgetSkipped` (`Bug::CRDT_PEER` now has nine).

- `CREATE TABLE` no longer fails permanently after 64 create and drop cycles. The engine catalog gains `Catalog::next_table_id`, one more than the largest table id ever created, and the executor uses it, so ids are still never reused. When every id the executor serves is used, `CREATE TABLE` fails with SQLSTATE `53400`. The row id counter of tables without a primary key is not changed.

- SQL transactions and session settings: the writes a `BEGIN` block (or one autocommit statement) stages are capped as a whole by the new `ExecLimits::transaction_bytes` (256 MiB by default, `latticed --transaction-bytes`), and the write that crosses it fails with `53200` and fails the block. `SET timezone`, `client_encoding` and `datestyle` now accept only UTC, UTF8 and `ISO, MDY` and answer `22023` otherwise instead of being silently ignored, and `SHOW` answers them. On the PostgreSQL wire a startup `TimeZone` or `DateStyle` is ignored and the effective value reported (with `server_encoding`, `integer_datetimes` and `standard_conforming_strings`), while a startup `client_encoding` other than UTF8 is refused before authentication. `SET TIME ZONE` to a non-UTC zone after connecting still fails, which breaks applications such as Django with a non-UTC `TIME_ZONE`.

- Server: a stuck database thread can no longer block connection threads or shutdown without limit. A timed-out statement queues nothing but itself (its abort and a connection close are marks in shared state, applied when the thread takes the connection's next request, including the failing of a block after an error the wire session raised itself), a connection has at most one live request queued, every send and wait shares one budget of the statement ceiling plus slack (counted in waits that ran out) and ends in `57014`, `53300` or, for a dead thread, `57P01`, and shutdown releases waiting connections and gives up on the database thread after `shutdown_grace_ms` (10 s) with an error. Queue memory is bounded by `queue_depth`, not by the connection count: stale entries of timed-out requests stay queued until the thread takes them, and a full queue answers `53300`. Not done: an idle client that got `57014` keeps its block open until its next request or close, and a thread wedged for good is reported, not stopped.

- `lattice-api` (#419, #490): when the wire session fails an open transaction block because of an error it raised itself, one that never reached the executor (a `CancelRequest` seen before dispatch, a `Bind` with binary formats, an unknown portal or statement, a duplicate or excess `Parse`, a busy `describe`), it now tells its handler through a new defaulted `QueryHandler::abort_transaction`. `ExecHandler` and the `latticed` proxy fail the executor's block, so a later `COMMIT` answers `ROLLBACK` and commits nothing, as PostgreSQL does; before, it committed the staged writes. Handlers that keep no transaction state need no change.

- Numeric division now prints the same number of fractional digits as PostgreSQL (about 16 significant digits, at least the scale of either operand) instead of always 20; for example 7.5 / 2.5 is 3.0000000000000000. Values are still limited to 38 digits.

- jsonb casts now refuse an unpaired surrogate escape (22P02) and a NUL escape (22P05), as PostgreSQL does; both were accepted before.

- Timestamptz text now follows PostgreSQL for zone offsets and long fractions: out-of-range offsets give 22009, colon-less offsets of three or more digits read as hours and minutes the way PostgreSQL reads them (`+00530` is 05:30, `+05030` is refused), and fractions past six digits round half to even at the microsecond.

- Bytea input now follows PostgreSQL (escape form, hex error code 22023) and integers and numerics accept digit underscores such as 1_000.

- A select item may use any word as its label after AS, such as SELECT 1 AS from, as PostgreSQL allows.

- A value longer than a varchar column keeps its first characters when the excess is only spaces, as PostgreSQL does; other overflow still fails with 22001.

- CREATE TABLE accepts a NULL column constraint and refuses a column named twice in a primary key or UNIQUE list, as PostgreSQL does.

- DROP TABLE and DROP INDEX with several names now say they are unsupported instead of reporting a syntax error.

- A date input no longer ignores text after the date: stray text and out-of-range times are refused, and BC years are refused instead of being read as AD.

- ORDER BY with a bare string, decimal, boolean or NULL constant is now a syntax error, as in PostgreSQL, instead of sorting by nothing.

- A numeric or floating point LIMIT or OFFSET count is rounded to a whole number instead of being refused.

- DEFAULT as a value in INSERT or UPDATE is now reported as unsupported instead of a syntax error.

- uuid text input accepts hyphens after any group of four digits and no longer ignores surrounding white space, as PostgreSQL does.

## [0.1.0-alpha.0]

Not published. This was the version under development; the first published build is
`0.1.0-alpha.1`, which makes no phase gate claim.

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
[0.1.0-alpha.1]: https://github.com/moncifem/latticedb/releases/tag/v0.1.0-alpha.1
[0.1.0-alpha.0]: https://github.com/moncifem/latticedb/commits/main
