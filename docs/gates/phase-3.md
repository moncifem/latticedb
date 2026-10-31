# Phase 3 status (gate NOT passed)

Phase 3 (live sync, CRDTs, SDKs and the agent plugin contract) has started on branches. Neither the
Phase 1 gate nor the Phase 2 gate is signed. The project owner confirmed in chat on 2026-10-29 that
Phase 3 development may begin now and that Phase 2 continues in parallel, under the same conditions
as the [Phase 1 review waiver](phase-1-review.md):

- Failures in Phase 1 or Phase 2 evidence take priority over Phase 3 work.
- No earlier gate check is weakened to make Phase 3 land.
- No Phase 3 release tag and no Phase 3 exit-criteria claim before the earlier gates are signed.
- Phase 3 work stops if a gap in an earlier phase invalidates it.
- Phase 3 starts with parts that do not depend on the unfinished SQL and storage work: pure CRDT types,
  the plugin contract and its parser. Anything that needs the server or storage waits for Phase 2.

Requirements and exit criteria are in [the Phase 3 specification](../spec/03-sync-and-sdk.md).

## Implemented slices

Merged. One server piece is connected: `latticed --gateway-listen` serves the sync protocol over WebSocket and `POST /mcp` (experimental: plaintext only on loopback; a non-loopback address needs `--tls-cert`, `--tls-key` and `--gateway-tokens`, the TLS part landed in `4b297405` for issue #578; the sync protocol is read only and rows only). The rest of this list is not connected to a listener (the evidence paragraphs below give the measurements and
what was not run):

- Typed change-feed event model with a stable cursor, and the experimental change-feed and sync frame
  codecs (encodings not frozen).
- Shared protocol crate `lattice-proto` (layer 1, issue #356): the sync frame types and codec and the
  sans-I/O WebSocket codec moved out of `lattice-api` (still re-exported as `lattice-api::sync` and
  `lattice-api::websocket`), and the SDK client uses the same frame types instead of a mirror (ADR 0030,
  proposed). A code move with no wire change; the conformance vectors are unchanged.
- Sans-I/O WebSocket codec (`lattice-proto::websocket`) and the principal-bound `SubscriptionHost`
  (`lattice-api::subscribe`); the gateway supplies the principal from the bearer token (#524).
- Bearer-token credentials (`lattice-api::bearer`, `latticed` file loader, [ADR 0031](../adr/0031-bearer-token-authentication.md)):
  format, verifier and generation (ADR 0031); the gateway uses them through `--gateway-tokens`.
- CRDT peer limits and the tree CRDT with moves (`lattice-core`), with the `tla/TreeMove.tla` model.
- Agent plugin contract with a conformance suite, and the MCP server and SDK outbox in `lattice-sdk`.
- Reconnect backoff with caller-supplied jitter (`lattice-sdk::backoff`, `0d7572bf`) and the saved-cursor codec and
  in-memory store (`lattice-sdk::cursor_store`, `befd1196`). `lattice watch` retries through that backoff and the supervisor below (`crates/lattice-cli/src/watch.rs`, `MAX_FAILED_CONNECTIONS` 3, `RETRY_BASE_MS` 1 s doubling to `RETRY_CAP_MS` 8 s, and never less than 1 s between attempts).
- SDK reconnect supervisor, a pure decision state machine over `Backoff` (`lattice-sdk::reconnect`, `f32c3ae2`, #620). `lattice watch` reports each connection outcome to it (`8192053`).
- End-to-end harness `crates/lattice-e2e` (`8a7bde20`, #540): `Fixture` runs the sync gateway and the SQL listener of
  `latticed` in process on loopback sockets, with `Pg` and `Ws` clients, and `Mcp` (`crates/lattice-e2e/src/mcp.rs`, `Fixture::mcp`) posts to `POST /mcp`. `tests/mcp.rs` (`475b3a01`, #534) runs two principals through the gateway (write, read, grants, conflict, scope, 401) and a `lattice_subscribe` cursor chain over a seeded history with no gap and no repeat. No `lattice watch` run against a real gateway is in the harness yet, and it injects no kills.
- A client state machine that resumes without gaps, and experimental TypeScript and Python codec
  clients (`clients/typescript`, `clients/python`).

The kernel already had last-writer-wins registers and an observed-remove set (`lattice-core::crdt`),
which Phase 3 extends.

Verification evidence, not an exit criterion: hostile-peer limits for the CRDTs (issue #364, threat-model gap
G-08). `lattice-core::crdt_peer` defines `PeerContext` and the limits `MAX_BATCH_OPS` (1024) and `MAX_SET_TAGS`
(2^20); `crdt_text::Limits::max_inserts_per_key` (2^19), `crdt_tree::DEFAULT_NODE_LIMIT` (2^20),
`MAX_TREE_DEPTH` (256) and `DEFAULT_ORIGIN_OP_LIMIT` (2^18) bound what one peer can make a replica hold, an old
id cannot make `Tree::apply_from` replay more than `DEFAULT_PEER_REPLAY_LIMIT` (4096) live ops (and
`Tree::apply_batch_from` no more than `MAX_BATCH_REPLAY_OPS`, 65,536, per batch; replica-local refusal, issue #450), and the
`*_from` entry points refuse a forged node id, an id past the drift bound and an oversized batch before any state
changes. `crdt_peer_tests.rs` holds a scenario per limit, three property tests with generated hostile peers
(refused input changes nothing, a flood cannot starve an honest peer, a hostile tree peer stays inside its share)
and nine planted bugs (`Bug::CRDT_PEER`) that the scenarios catch. Not done: nothing in the repository calls these
entry points yet (no sync layer, and `Tree::apply_from` must not be wired in before issue 487 closes: the replay cap is a stopgap) and the compaction frontier is still trusted. The depth rule is in
`tla/TreeMove.tla` (`MaxDepth = 2`, invariant `DepthBounded`) with `tla/negative/TreeMoveNoDepthCheck.cfg`,
which must violate it. `tla/TreeMoveCycleOnly.cfg` checks the cycle rule alone (depth rule off). These models are
written but not yet recorded as passing: the evidence is the pinned-TLC CI step, to be recorded after its first run.

Verification evidence, not an exit criterion: `tla/TreeMove.tla` (WP43, issue #245) models the tree CRDT
move rule with 2 replicas, 3 nodes and 4 operations. Before the depth rule was added, one local run with a
single worker explored 6,635,881 states (751,243 distinct) in 45 to 48 s (two runs) with no violation of
`Acyclic`, `Rooted`, `Convergent` or `MatchesSerial`; with the cycle check removed TLC reported `Acyclic` violated,
and with undo and redo of later-ordered operations removed it reported `Convergent` violated. The model has
changed since (depth rule, `DepthBounded`, the `TreeMoveCycleOnly` config, `TreeMoveNoDepthCheck`), and no run
of the changed models is recorded here. Not run on CI yet; simulation, row integration and op-log growth
measurement of WP43 are still open.

Network hardening of the `latticed` listener (issue #347, no exit criterion): separate idle timeout, absolute
per-response write deadline, best-effort TLS `close_notify` on every connection exit, a private-file check on the TLS key, SCRAM verifier, decoy-secret and account-policy files (owner-only mode 0600 or 0400, with 0440 and 0444 refused; a directory that group or others cannot write to; symlinks and FIFOs refused; not on Windows, which refuses these files) and a CancelRequest rate limit
(a resource bound: wrong keys consume the budget, so a few dozen addresses can starve real cancels, and
clients behind one NAT share 16 per second), each with a test in `crates/latticed` (run locally: `cargo test -p latticed`).
Open from that issue: SCRAM channel binding (`SCRAM-SHA-256-PLUS`), the decoy-secret length-only check
and a comment fix, all in `lattice-api`.

Service mode of `latticed` (issue #584, part of the Alpha 0.1 definition #567, no exit criterion): SIGTERM and SIGINT
start the same clean stop as the `shutdown` line (listener, then database thread, then the data-directory lock),
`--stdin-control off` stops the process from reading stdin so `docker run -d`, systemd and launchd do not stop it at
end-of-file (the default stays `on`), and `--stop-timeout-ms` or a second signal abandon a stop that does not finish
(exit status 2; 0 is a clean stop, 1 a failure). The real-binary tests in `crates/latticed/tests/service.rs` cover
SIGTERM and SIGINT with a client still connected, a restart of the same data directory, end-of-file with control on and
off, and option validation (run locally: `cargo test -p latticed`). The forced-stop paths are tested only with an
injected stuck shutdown (`latticed::stop`), because the real server has no worker that fails to return. Not verified:
the systemd and launchd snippets in the crate README under those supervisors and Windows (no signal handling there).
A SIGTERM during recovery (issue #714) now ends the process at once with status 2: the handlers are installed before the
data directory is opened and a watcher thread exits the process, because recovery cannot be cancelled. The test
`a_stop_signal_during_recovery_ends_the_process_with_status_2_and_the_directory_reopens` stalls recovery with a named pipe
in the log directory; it runs in CI, not locally, and the container case (PID 1 under `docker stop`) is not tested. The handler is one `signal(2)` declaration and call in
`lattice-sys`; no dependency was added.

Linux archive glibc baseline (issue #588, part of the Alpha 0.1 definition #567, no exit criterion): `package.yml`
builds the Linux x86_64 and arm64 archives on `ubuntu-22.04` and `ubuntu-22.04-arm` (both listed in the
runner-images README on 2026-10-30) instead of the 24.04 runners, records the highest `GLIBC_x.y` symbol version
the binaries import (job summary and release notes), and gates publishing on the job
`archive_runs_on_debian_12_and_ubuntu_22_04`, which runs `lattice --version` and `latticed --version` from the
unpacked archive in digest-pinned `debian:12` and `ubuntu:22.04` containers on both architectures. Not verified:
no hosted run has happened yet, so the measured minimum glibc is unknown and neither the build nor the smoke job
is known to pass. The image still copies the same binaries into `ubuntu:24.04`. Musl was not tried.

Install script (issue #585, part of the Alpha 0.1 definition #567, no exit criterion): `scripts/install.sh` installs
`lattice` and `latticed` for the three release targets after a SHA256SUMS check and, when `gh` is present, a
`gh attestation verify`. `scripts/test_install.sh` runs 16 offline tests against a fake release directory (flipped-byte
checksum refusal, unknown-platform refusal, missing checksum entry, attestation pass and fail through a `gh` shim,
newest pre-release lookup, no profile edits); the `install-script` workflow runs it with shellcheck. Not verified:
the script against a real published release or its real attestation (`gh attestation verify` was only exercised through a
shim, and whether it needs a signed-in `gh` for a public repository is untested), the rehearsal job (first run needs a
`release` rehearsal run id), and the script as a release asset (the release workflow does not attach it yet).

Container image (issue #587, part of #568 and the Alpha 0.1 definition #567, no exit criterion): with no arguments the
entrypoint prints the flags a server needs and exits 2; otherwise it runs `latticed --stdin-control off` with the arguments.
The image runs as uid 10001 with `/var/lib/lattice` and `/etc/lattice` owned by that user (the group is created with gid
10001; `useradd --system` alone gave gid 999). The `image-smoke` job builds the binaries on ubuntu-24.04 (glibc 2.39), while
the release archives are built on ubuntu-22.04 (glibc 2.35), so a pass does not prove that the release binaries run in the
image; it tests the image built from the same source on the same base. `tests/image/smoke.sh` ran locally on an arm64 image built with `docker build`
from a debug Linux build of `lattice` and `latticed` in `rust:1.99.1-bookworm`, on a Docker Desktop VM: 12 of 12 checks
passed (usage error, version passthrough, uid, fresh volume owner and mode, `lattice account` init and add inside the image,
TLS key installed for uid 10001, SCRAM over TLS listening on 0.0.0.0:5432, wrong password refused, `psql sslmode=verify-full`
insert and select, clean `docker stop` with exit 0, restart on the same volumes with the rows read back and a duplicate key
still refused, a second clean stop). Not verified here: the amd64 release-profile image (the `image-smoke` job is its first
run), arm64 in CI, the published or nightly image, the systemd and launchd snippets, a stop during recovery as PID 1 under Docker (the
host test in `crates/latticed/tests/service.rs` covers the binary, not the image), and `docker stop` while a client is connected. The
`ubuntu:24.04` base still contains `/bin/sh` and `apt`; they were not removed.

Alpha documentation (issues #589 and #590, part of the Alpha 0.1 definition #567, no exit criterion): `docs/alpha.md`
(what works, how to run `latticed` in trust and SCRAM modes, psql, the agent-client status, experimental parts and a
hand-edited known-issues table), a README install section marked "once the alpha tag exists" (at the time; since removed), a rewritten `SECURITY.md`
with supported versions and the disclosed gaps, and an alpha disposition for every threat-model gap row. Facts checked
when this was written: the gateway (PR #680), the MCP tool backend (PR #687) and the `lattice account` tool (PR #683) were closed unmerged, and their commits (e03b319a, 8932717f, a3ca61f1) landed on `main` directly. Update 2026-10-30: `docs/alpha.md` still says no agent client can connect except as a PostgreSQL client, which held then because no MCP transport existed yet. Superseded 2026-10-31: `POST /mcp` on the experimental gateway and the `lattice mcp` stdio proxy exist on `main`, and the gateway binds each connection to its verified bearer-token principal.
The quickstart smoke test covers only the loopback build, psql and restart steps. Not done: a named MCP client run on a
clean machine (none is recorded; the stdio proxy `lattice mcp` from `f14aa0f4` now exists for a client to use), the independent security review of the gateway diff, a link check run on a hosted
runner, and verification of the install one-liner against a published tag.

## Exit criteria

| # | Criterion | State |
|---|---|---|
| E3-1 | I5 and I12 across 1,000,000 seeds per night for seven nights | Not started |
| E3-2 | Contract suite green for the reference plugin and all four SDKs | Not started |
| E3-3 | Reconnect with cursor yields zero gaps and duplicates | Not started. Related evidence, not E3-3: `crates/lattice-e2e/tests/reconnect_kill.rs` (4 seeds of a seeded kill schedule, each cut at least once inside a frame, resume from the last processed cursor, 110 rows, no gap or duplicate; one table, no SDK supervisor). Not run locally; passed in CI run 41207082634 (PR 852, head b4192fc2, rerun after an unrelated `server_config` port failure; the final head differs from it only in this sentence), verified with gh |
| E3-4 | Fan-out and merge benchmarks recorded | Not started |
| E3-5 | NS7 passes end to end through the SDK | Not started |
| E3-6 | Text CRDT ADR from Phase 1 confirmed or reversed with data | Not started |

Verification evidence for WP46 (issue #248), not an exit criterion: `crates/lattice-testkit/tests/fencing_races.rs`
runs two scripted race scenarios (takeover during append, takeover during watch reconnect) on the kernel and
simulated disk for 200 seeds under each of the five fault profiles (clean, crash-heavy, disk-faulty, hostile,
everything), 2,000 runs in all. One local run: 0 assertion failures, 12 runs censored by bit rot (all in
`everything`), 19,656 superseded writes submitted, 18,809 answered `Fenced`. The test uses the kernel and not
the plugin contract reference plugin, because `layers.toml` does not let `lattice-testkit` depend on
`lattice-sdk`. A 100,000 seed per profile volume test is an ignored test; `nightly.yml` runs it as the job `volume-fencing`. The job has not been observed yet, so no result and no run time is recorded here, and it uses the same fixed seeds every night.
The kernel checks the fence before the duplicate check (issue #304), so a late retry of an already stored write
is answered `Fenced`, as the plugin contract says; the test `late_retry_of_a_stored_write_is_fenced_per_the_contract`
runs by default and the planted bug `DuplicateBeforeFence` is caught by the fencing race sweep.
Verification evidence, not an exit criterion: `lattice-testkit::convergence` (WP45, issue #247) runs 2 to 5
replicas of the register, set and tree CRDTs over a simulated network (delay, reorder, duplication, drops with
retransmit) chosen from the five fault profile names (clean, crash-heavy, disk-faulty, hostile, everything).
One local run of `cargo test -p lattice-testkit --test convergence_replicas`: 200 seeds x 5 profiles x 3 families
(3,000 simulations, 146,822 acknowledged writes) produced 0 violations of identical digests, no lost
acknowledged write and an acyclic tree. Seven planted defects in harness wrapper replicas (overwriting merge,
lost local write, lost remote operation, missing duplicate detection, skipped cycle check, for the three
families) were each detected within the same 200 seeds. The 100,000-seed volume test is `#[ignore]` and was not
run locally; `nightly.yml` runs it as the job `volume-convergence` with the same fixed seeds every night. That job has not been observed yet, and it is not wired into the ledger or scorecard.
Verification evidence, not an exit criterion: principal binding of the sync subscription host (threat model G-03,
issue #359, sync part only). `SubscriptionHost::new` now takes the authenticated `Principal` and
`EventSource::read_after` receives it on every read, so a source can refuse a scope per principal; the free-text
`Hello.agent` is not an identity. Five unit tests in `lattice-api::subscribe` (`cargo test -p lattice-api --lib subscribe`,
28 tests, passing locally) cover the principal seen by the source, a `Hello` that names another account, a denied scope
answered like an unserved one (the host maps only `SourceError::UnknownScope` to `invalid_scope`, so a source must return `UnknownScope` for a denied scope; `TooOld`, `BadCursor` or `Unavailable` would leak existence, and that is a contract on the source until a kernel-backed one exists), live polls, and the planted bug `SubscribePrincipalDropped`. This is an API change
(`SubscriptionHost::new` gained a parameter, `SubscriptionHost::local_development` is the loopback stand-in,
`EventSource::read_after` gained one). The gateway binds each connection to its verified bearer-token principal, and `POST /mcp`
serves the MCP caller that way. The plugin sessions are not bound, so no end-to-end authorization claim is made for them. The kernel-backed source is the next paragraph.
Verification evidence, not an exit criterion: kernel-backed `EventSource` (issue #519, part of #518, sync part of
threat model G-03). `lattice-api::feed_source::FeedSource` (made by `ExecHost::feed_source`) serves `table/<name>` from
`RowStore::read_feed_as` through the SQL executor's actor thread, as the principal's policy: the same
`PolicyTable` mapping the SQL handler uses (`PolicyTable::grant_for`), deny by default, no allow-all fallback. A
denied principal, an unknown table, an unserved scope string and a table hidden by `hiding_catalog` are all
`SourceError::UnknownScope`, decided before the cursor is looked at. `CursorTooOld` is `TooOld`; `MissingCommitTime` and
feed encoding errors are `Unavailable`; the 12-byte feed cursor is the wire cursor (empty is the origin; another length
or a cursor past the newest commit is `BadCursor`). The prefix-then-`KeyTooLarge` rule of `feed_emit` is handled inside the
source: it continues from the blocked event's cursor, which skips exactly that event, so a legacy
oversize key never wedges a reader and no event is lost or repeated. 17 unit tests (`cargo test -p lattice-api --lib
feed_source`, run locally once, default proptest settings): a property test over generated two-table histories
(inserts, updates, deletes, several rows per commit) checks that paging from the origin and from every event cursor, at
page sizes 1 to 4, equals a model built from the commit LSNs and key ranks and not from the feed; the denied answer is
the same `Output` as for an unknown table through a `SubscriptionHost`; a live tail, multi-row change sets with
`(lsn, index)` cursors, the vacuum floor, the oversize-key skip at every page size, and a bounded read racing a writer
on the same actor (issues #581 and #543: the read is made of steps, each one actor request, and `a_long_read_yields_the_executor_between_steps_so_a_commit_lands_in_the_middle_of_it` shows a commit queued during a read being served between its steps). Planted bug `Bug::FeedSourcePolicyDropped` (policy replaced by allow-all) is caught by
`a_dropped_policy_is_caught_by_the_witness`, with a clean control; two hand mutants (no skip of the blocked event, and a
reader that stops after one answer or ignores the table) each failed the oversize or model test. Not done, listed below.

Verification evidence, not an exit criterion: bounded feed read (issues #581 and #543). The engine read `RowStore::read_feed_as` finds the cursor in an in-memory commit index kept only by the row table (commit LSN to the keys it wrote, in key order; secondary-index tables do not keep one). The index costs one clone of each key written by a retained change set plus a `Vec` header per change set, so it grows with retained history, and it is rebuilt by replaying every change set when the store opens, an O(history) step; vacuum prunes it and releases the memory. A step examines at most a scan budget of versions (default 4096, `FeedSource::with_scan_budget`) and builds events only for rows of the requested table that the policy allows, at most the events still wanted. It returns the page with a resume cursor and a `more` flag. A caught-up read examines no version; a read that is not caught up costs one binary search plus one lookup per version examined; memory is the returned page, not the history. A long run of history the reader cannot see is read in steps that are separate actor requests, so a commit queued meanwhile runs between steps (`a_long_read_yields_the_executor_between_steps_so_a_commit_lands_in_the_middle_of_it`). The time of a step has not been measured.

The proof is counted, not timed: a counter of versions handed out by the version table (a test hook) shows a caught-up read visiting 0 and a read after two new commits visiting exactly their versions for tables of 20 and 200 commits. `bounded_pages_equal_the_full_scan_oracle` (property test, run locally at the default case count; the PR run uses more) compares the pages for every generated history, cursor, limit pair, table filter, policy and vacuum horizon with the previous full-scan implementation, kept under `cfg(test)` as the oracle, and without limits with a model built from the commits. A vacuum between two steps of one read makes the next step `CursorTooOld` (`a_vacuum_between_the_steps_of_one_read_makes_the_next_step_too_old`; the source returns the events already collected, and the next poll reports `TooOld`). Planted bugs `FeedLimitIgnored`, `FeedScanBudgetIgnored` and `FeedIndexCommitSkipped` are each caught. `the_log_record_limit_keeps_every_value_over_the_feed_bound_out_today` pins that the log's record limit keeps any value over the feed's bound out (issue #488 is not reachable), and fails if that limit grows. `RowStore::history_from` and `Table::history_from` (full scans) are removed from the public API; `events_since_as` is the unbounded form of the same walk.

Accepted limits of the alpha, each disclosed: (1) quiet-table wedge: the index is not per table and a client cursor only advances on events, so a subscriber of a quiet table whose last event is at cursor `C` examines every version of every other table, and every row its policy hides, after `C`; beyond 1024 steps of the scan budget (about 4.2 million versions) every poll answers `Unavailable` and never recovers by polling (returning the resume cursor would reveal where hidden commits are, issue #363; an empty page would claim the reader is caught up). The old code completed in O(total rows). Mitigation: reconnect through a bootstrap path (issue #525, not built) or the table-scoped index of issue #661. The threshold and the latency of a poll reveal the volume of hidden activity (G-07). (2) No per-principal cap on feed work (issue #543). (3) No wall-clock bound: the bounds count versions. (4) `IndexInconsistent` and `MissingCommitTime` are `Unavailable` on every later poll; the store needs a reopen.

Table-level read check (issue #522, interim): the
precondition recorded here earlier is met for per-table grants. `ActorPolicy::can_read_table` is the one check the feed
(`Database::visible_table_id`) and the binder (`ExecCatalog`, `sees_table`) share, so a table the account holds no bit
on is `UnknownScope` for the feed and `42P01` for SQL, and a table it may see but not read is not served by the feed
either (write-only and ddl-only tables are `UnknownScope`). The two cannot drift: a denied table never reads as an empty
feed. The precondition still binds the Phase 4 policy language and any row-level policy. The
`BadCursor` check against the newest applied LSN lets a principal who can read one table binary-search the global commit
position, and gaps between cursor LSNs show activity on tables it cannot read (same class as issue #363, unchanged); a
denied request still occupies an actor queue slot. The filtered-feed gap question (issue #363) is unchanged; column definitions are recovered across a restart since
issue 573; the gateway binds the principal from its verified bearer token, and nothing here is claimed live end to end.
Verification evidence, not an exit criterion: interim per-table grants (issue #522, part of #567, threat-model G-01).
An account in the policy file can be `allow-all`, `deny-all` or `tables: name=bits, ...` with bits `r`, `w`, `d` per
table and `*` for the rest; the most specific entry wins, nothing is merged, names match exactly, an unlisted account
is `deny-all` (semantics and limits: `crates/latticed/README.md`). `lattice-engines::grants` holds the grant set, its
parser and the `TableGrantPolicy` evaluator; `ActorPolicy::with_table_grants` / `bound` (`lattice-exec`) resolve names
to table ids per transaction, so every `begin_with_policy` transaction and catalog change of an authenticated principal
carries the grants (the `begin()`/`commit_catalog` `AllowAll` paths are not reachable from the executor or the feed).
Tests (`cargo test -p lattice-engines --lib grants`, `cargo test -p lattice-api --lib table_grants`, run locally once with
default proptest settings): a hand-written account x table x operation matrix with literal expectations, at run time and
at `Describe` (`table_grants_gate_reads_writes_and_feed_alike`), the feed for the same matrix, hidden tables equal to missing
ones for SQL messages and for feed cursors, DDL by name and by `*`, `RETURNING`, blocks, `UNIQUE` entry tables, a table
created after the grants, the file parser's bounds and line numbers, and two property tests against bit models
(32 SQL cases; the resolver). Planted bugs `GrantReadOnlyTreatedAsWrite` and `GrantTableMatchByPrefix` are caught by
the SQL matrix, and the prefix bug by the feed matrix. Not done: no row predicates, roles, groups or policy language; no
`presence` bit (issue #626); "alike for SQL, feed and MCP" is shown for SQL and the feed here and for MCP read and write in the MCP backend paragraph below; grants live in a file that the operator edits and restarts for, not in the database; a transaction block
reads a table created after it began as empty; `w` without `r` can probe primary-key and `UNIQUE` values of the table through `23505` (PostgreSQL INSERT-only behaviour, pinned by a test); the BadCursor/LSN-gap side channel of the feed (#363) is unchanged.

Verification evidence, not an exit criterion: commit notifier (issue #523, part of #518 and #567). `ExecHost::commit_notifier`
returns `lattice-api::notify::CommitNotifier`; the actor publishes the newest applied LSN (the larger of the row store's
and the catalog's) after every statement and once at start, so a restarted host publishes its recovered head. A waiter
calls `wait_after(seen, timeout)`: the published LSN and the comparison share one mutex, so a commit before the waiter
sleeps returns at once and one after it sleeps wakes it; the answer is the newest LSN, `TimedOut`, `Full` (cap
`HostLimits::commit_waiters`, default 1024, counts sleepers only) or `Closed` (shutdown or any end of the database
thread, via the actor's exit guard). State is one `u64`, a flag and a count; no per-waiter queue, and publishing is one
lock and one `notify_all` that never waits for a waiter. The wake carries only an LSN. Tests (`cargo test -p
lattice-api --lib notify` and `exec_host::tests`, run locally once, 285 tests in the crate passing): a model test of
`wait_after` against (published, seen, closed), a 24-waiter 4-publisher stress test where any missed wake shows as a
timeout, per-waiter thresholds, the sleeper cap, spurious wakes, close, and host tests for wake-after-durable
(a statement held between dequeue and run announces nothing; the woken LSN is the newest event the feed source shows),
the timeout fallback, a restart, eight stalled sleepers while 20 commits run, shutdown and database-thread death.
Planted bugs: `Bug::NotifyWaitIgnoresPublished` (the first look at the published LSN is skipped, caught by the
already-published and stress paths) and `Bug::NotifyBeforeDurable` (the host publishes the next LSN before the
statement runs, caught by the held-statement test; the host test opens the database on the actor thread with the bug
enabled, because the switch is per thread). Update 2026-10-30: the gateway consumes it (issue #524) and the MCP long poll is on `main` (#579, commit 81953029); no loom model of the notifier (the check is a deterministic model plus a std stress test); the wake
reveals commit activity to any holder regardless of grants (threat model G-07, issue #363) and a waiter that is
woken for a commit it cannot read just gets an empty poll; `wait_after` with a `seen` past the database can outlast
its timeout while commits keep arriving, because no clock is read.
Verification evidence, not an exit criterion: MCP tool backend over the executor (issue #526 part 1, part of #518; threat
model B6 and G-03). `latticed::mcp_backend::McpBackend` implements `ToolBackend` for `lattice_read` and `lattice_write`
through `lattice-api::principal_sql::PrincipalSql` (new: SQL for one fixed principal on the executor actor), so rows
are read and written as the authenticated principal under the executor's policy, with that account's author id. The
`Caller` is derived from the principal and a different one is refused; tool grants map to table grants without
widening them (`caller_for`), the MCP scope maps to `table/<name>` by `sync_scope`, writes carry `expected_version` as
one conditional statement, row text is returned under a fixed `untrusted` envelope and output is cut under 1 MiB at a
row boundary. `layers.toml` gained the edge `latticed -> lattice-sdk` (the only change). Tests (`cargo test -p latticed
--lib mcp_backend`, 18 passing locally; `cargo test -p lattice-sdk --lib mcp`, 24; `cargo test -p lattice-core --lib
bugs`, 2; `cargo xtask check-layers`; clippy `-D warnings` on latticed, lattice-api, lattice-sdk and lattice-core)
include `mcp_write_is_visible_to_a_sql_reader_and_the_feed_with_the_right_author` (author compared with the same
account's SQL write and another account's), `mcp_caller_cannot_name_another_principal`,
`write_conflict_returns_the_expected_version_mismatch_as_a_tool_error_not_a_lost_update` and a six-thread race with
one winner, `tool_output_is_bounded_before_the_backend_commits`, `row_text_is_data_in_a_fixed_envelope` (exact bytes),
`a_denied_table_looks_identical_to_a_missing_one` (same response bytes for SQL-hidden and absent tables),
`mcp_grant_scope_maps_to_table_grants_and_never_widens_them` and
`scope_mapping_is_total_injective_and_refuses_what_it_cannot_express`. Planted bugs `McpCallerFromArgument`,
`McpWriteIgnoresExpectedVersion`, `McpDeniedTableDistinguishable` and `McpRowTextOutsideEnvelope` are each caught by
their witness. Not done at #526 (`lattice_subscribe` and the 12-byte cursor were added under #579, see the paragraph below): a cursor and an unseen-change count
in tool results; author names in tool output (the envelope is built for them, no tool returns one yet); any transport
or token handling (#527, #580); a version column maintained for SQL writers; per-principal limits (#529). Tables must
have the key, value and version columns; others get `table_shape`. Not run locally: the workspace gate, mutation
testing, Miri.

Verification evidence, not an exit criterion: MCP `lattice_subscribe` (issue #579, part 2 of #526). The backend
`latticed::mcp_backend` serves a long poll over `FeedSource::read_records` as the bound principal, waiting on the
`CommitNotifier` for at most `wait_ms` (capped at 30 000 ms, default 0), with the 12-byte feed cursor as 24 lowercase
hex digits. A reply holds at most 64 events (the codec's array bound) and stays within the 1 MiB output bound, each
upsert decoded to its columns on the executor thread (`feed_source::decode_columns`, requested by a flag, so the sync
path is unchanged), with `next_cursor` and `truncated`. Shutdown ends a waiting call with `50001`. Tests:
`cargo test -p latticed --lib mcp_backend` (26 passing locally), `cargo test -p lattice-sdk --lib mcp` (25),
`cargo test -p lattice-core --lib bugs` (2) and `cargo test -p lattice-api --lib feed_source` (17). The subscribe tests
check the reply against the change feed read directly (same cursors, authors and order), a seeded interleaving of
writes and calls that yields every key once in write order, the wait bound and a wake by a commit, shutdown cancelling
a waiting call, two principals (hidden commits do not move `next_cursor`, a restricted table answers as a missing one),
a delete, the event and byte bounds with resumption across replies, a cursor ahead of the head answered as caught up
(identically for two principals, with hidden commits), and a malformed cursor refused. Planted bug
`McpSubscribeInclusiveCursor` is caught by its witness. Issue #363 is not closed: a table-restricted reader can still
observe the quiet-table wedge (`50001` after a large run of hidden versions, issues #525 and #661), the latency of a
waiting call (each wake re-reads the feed), `cursor_too_old` (the store-wide vacuum floor) and a corrupt event's
`50001`, which wedges that reader. Not done: a per-principal long-poll cap (issue #529; a waiting call holds its worker
for up to 30 s, and when the notifier is full the call polls about 50 times a second), any MCP transport (#527), and the
unseen-change count (not returned: it would reveal hidden commits, issue #363).

Verification evidence, not an exit criterion: MCP over HTTP (issue #527, part 1, with the Host, Origin and content-type
defences of issue #580). `POST /mcp` on the gateway port (`latticed::ws::mcp_http`, routed from the request line before
the WebSocket parser runs) verifies the bearer token the way the WebSocket upgrade does, serves one JSON-RPC message per
request as the verified principal through `McpBackend`, and closes. Tests: `cargo test -p latticed --lib mcp_http` (9
passing locally: the Host, Origin, media-type, length and head rules, the route decision, and the witnesses),
`cargo test -p latticed --test mcp_http` (18 passing locally, over real sockets: initialize, the notification, list and
call as two principals with no `_meta` and no session, which is how standard HTTP MCP clients call it; a denied call
answered like a missing table; a present `_meta` with a bad version refused; the same 401 bytes for missing and wrong credentials; a
token in the URL refused and never echoed; the Host and Origin rules; a cross-origin POST that cannot call a tool; method,
media type and declared length (413 before any body byte is read); a body that stops short cut at the deadline; rate
limiting; revocation within the reload bound; a gateway without a credential store serving no MCP request; a principal
named in the body ignored), `cargo test -p latticed --test ws_sync` (19 passing, one ignored as before, so the WebSocket
path is unchanged) and `cargo test -p lattice-core --lib bugs` (2). Planted bugs `McpHostCheckSkipped`,
`McpOriginCheckSkipped`, `McpBodyCheckedAfterRead` and `McpPrincipalFromBody` are caught by their witnesses. The switch is
thread-local, so the socket tests cannot turn them on. Update 2026-10-31: stdio is the `lattice mcp` proxy (`lattice-cli`, `f14aa0f4`, loopback plaintext only), TLS on the
gateway port landed in `4b297405` (#578) and `--gateway-allowed-host` in `fc055d71` (#774). Not done: Streamable HTTP
streams, the gateway idle deadline (#578), and a run with a real MCP client (the per-principal long-poll cap landed in #529 part 1, see its paragraph). The workspace gate, mutation testing,
Miri and the long tests are left to CI.

Verification evidence, not an exit criterion: `lattice-engines::feed_emit` (WP36, issue #238) emits the change feed
from the row store's retained committed versions. One local run of `cargo test -p lattice-engines --lib feed_emit`
(6 tests, default settings): the feed from every cursor of 12 seeded 14-commit histories equals an independently
computed model (filtered by a value-based read policy in a second test); 8 kill-and-reopen histories re-emitted
identical events and resumed with no gap and no duplicate; three planted emitter defects (skipped event, reordered
entity events, inclusive resume) were each rejected by the oracle in more than 100 cases. A cursor at or below
the vacuum floor returns `CursorTooOld`. Row events carried author 0 when that evidence was recorded; the next
paragraph supersedes that for commits made through the executor. The full `cargo xtask ci` gate and the rest of the
engines suite were not run locally.
Verification evidence, not an exit criterion: row commits record who wrote (issue #520, part of #518). No byte format
changes: `formats/changeset-v1.md` already has the 16 byte `author` field and the change feed repeats it
(`formats/change-feed-v1.md`), so recording an author is a use of existing fields (`RowTx::with_author`, read back by
`feed_emit` from the log, rebuilt on recovery, forgotten below the vacuum floor with the rest of the commit metadata).
Turn is still not recorded for row commits (no session turn exists for SQL). The author id is injective per account:
`lattice_engines::authors::AuthorRegistry` assigns ids from a counter at first use, from 256 up (0 is unknown or legacy,
1 is the local-development principal, 2..=255 are reserved), and syncs `authors.reg` beside the log before an id is
handed out. A renamed account keeps its id only through `rename`; a retired account's id is never reissued and a later
account with the same name is a new account; a registry file with a repeated live name, a repeated or decreasing id or
a malformed line refuses startup. The id comes from the authenticated principal in `ExecHandler` (account name from
the credential check, or the fixed development id) and from nothing a client sends. One local run of
`cargo test -p lattice-engines --lib authors`, `-p lattice-engines --lib feed_emit`, `-p lattice-exec --test sessions author`
and `-p lattice-api --lib exec_handler exec_host` passed: events name the right author for alternating principals before
and after a kill and reopen, the same account keeps its id across a host restart, a proptest over generated names
including near-collisions finds no shared id, legacy author 0 still reads, and SQL text, `application_name` and the
startup `user` do not change the author. Two planted defects are caught by those tests: `AuthorFromClient` (author read
from the statement) and `AuthorIdTruncatedHash` (ids collide). Startup also checks the registry against the account ids in the retained log and refuses when the file is missing or
older (`registry_missing_or_older_than_log_refuses_startup`, planted bug `AuthorRegistryLogCheckSkipped`; ids that vacuum
already dropped cannot be checked). The file is created, then narrowed to 0600 inside the 0700 data directory, and a symlink is refused. Not done: the `authors.reg` file is a new local sidecar,
experimental and unfrozen ([ADR 0029](../adr/0029-author-id-registry.md), proposed, independent review pending; it is not
in the WAL, the change set or the feed); retirement is manual and library-only: nothing calls `rename` or `retire`,
removing an account from the credential or policy file does not retire it, and adding the same name back is the same
account with the same id; names are byte exact (case sensitive; confusable names are different accounts) and an account in the credential
store but not in the policy file still gets an id on its first statement; commits that bypass the executor
(`RowStore::commit_catalog`, the standalone catalog store) and older history carry author 0; the MCP write path (#526) and the arbiter must pass the
principal's id the same way when they land; the filtered-feed leak (#363) is untouched. The full `cargo xtask ci` gate was
not run locally.
Verification evidence, not an exit criterion: `lattice-api::backpressure` (WP38, issue #240) is a pure module
with bounded per-subscriber queues (event count and byte budget), an in-flight window, overflow with a resume
cursor (the last acknowledged event) and an O(N) fan-out hub that copies each payload once and shares it. One
local run of `cargo test -p lattice-api --lib backpressure -- --nocapture` (14 tests, 0 failures) with limits of
64 events, 2,048 bytes and 32 in flight, one stalled subscriber and 300 published events: at 10,000 subscribers
there were 3,000,000 offers, 300 payload copies (one per event), 9,999 reference-count increments per publish,
0 blocking waits (every publish returned a result), a peak of 63 events in any queue and a peak of 963,485
bytes for all queues (about 96 bytes per subscriber, declared bound 5,216 bytes each). A queue struct is
112 bytes and an event entry is 24 bytes. The stalled subscriber was overflowed at the event derived from the
payload sizes, with resume cursor 0, at 1, 100 and 10,000 subscribers, and all others received every event in
order. A planted unbounded queue is caught by the bound check at 1 and 100 subscribers. A property test of random
publish, drain and acknowledge sequences kept queue length and bytes within limits, order and a monotonic cursor.
Offers per second are not measured (the crate bans `Instant`); the counts above are the evidence. Not wired into a
server, not part of the nightly shards.
Verification evidence, not an exit criterion: `lattice-engines::merge` (WP42, issue #244, RFC 0011 draft) is a pure
in-memory function `merge_rows` that merges a remote row change into a local row with a per-column `MergeKind`
(last-writer-wins register, observed-remove set, or plain). It is not wired to the row store, the catalog or sync,
has no byte encoding, and CRDT state is not persisted, so recovery of CRDT state, delta replication through change
sets and the simulator-level N-replica check of WP42 are still open. One local run of
`cargo test -p lattice-engines --lib merge` (14 tests, default proptest settings): commutativity, associativity,
idempotence and convergence of replicas applying shuffled, duplicated orders of the same changes held against an
oracle computed from the change set; nine deliberately wrong merge variants were each caught by the convergence check (five also by the law check). The full `cargo xtask ci` gate was not run locally.
P3-05 fact: I5 for column merge holds only while the union of changes fits the bounds (256 columns, 1 024 set
elements, 4 096 tags); beyond them `merge_rows` refuses and a replica can diverge, so I5 is open there until
compaction exists. Row delete uses the interim rule in RFC 0011. Row-store wiring, persisted CRDT state and
simulator-level N-replica checks are not done.

Benchmark programs for E3-4 (WP53, issue #255, first slice), list only, no measurements recorded:
`cargo run --release -p lattice-bench --example phase3_bench` covers tree CRDT apply (in order, late),
sequence CRDT insert, set and register merge, change-feed cursor and event codec, sync frame codec and
fan-out to 1, 100 and 10,000 subscriber queues with slow subscribers. Tree and text compaction, the
backpressure module, reconnect storms and the CI baseline wiring are not covered yet. E3-4 stays open.
Verification evidence for WP51 (issue #253), first slice, not an exit criterion: `clients/typescript` is a hand-written
TypeScript port of the sync frame codec, the change feed cursor and event codec and the client state machine, with no
runtime dependencies. `conformance/vectors.json` is rendered by a Rust test in `lattice-api` (it fails if the checked-in
file differs) from hand-assembled bytes: 37 valid frames, 60 invalid frames, 7 incomplete inputs, 27 encoder refusals and
12 negotiation cases for the sync protocol, and 7 cursors, 4 invalid cursors, 8 events, 23 invalid events and 5 encoder
refusals for the change feed. One local run (Node 24, the CI job pins Node 22) of the TypeScript tests: 67 tests passed,
including every vector decoded and re-encoded byte for byte with the same error kind as Rust, and seeded randomised tests
(encode/decode identity, canonical form of mutated input, client exactly-once against a model server). Checking the
tests against deliberate defects in the compiled client (a byte order mark dropped from text, a loosened text length
bound, a skipped LSN 0 check) made the conformance tests fail each time. The workflow `clients-typescript.yml` has not run
on CI yet. The Go client, a generator and the agent plugin contract in TypeScript are not done, and no client
talks to a server, because none exists.
`cargo run --release -p lattice-bench --example phase3_bench` covers tree CRDT apply (in order, late, and
late before and after `Tree::compact`), `Tree::compact` itself, the text CRDT (replay of a fixed-seed
random edit log and `Doc::compact` with and without retained revert points; 10 000 visible characters at
the default scale), sequence CRDT insert, set merge, register merge (real updates, plus the no-change
path), change-feed cursor and event codec, sync frame codec, fan-out through the real
`lattice_api::backpressure::Hub` to 1, 100 and 10,000 subscribers with every tenth stalled, and a
reconnect storm of clients resuming from cursors against `SubscriptionHost`. The benchmark metadata
records the CPU model (best effort). The manual `bench-baseline` workflow also runs it to produce a CI
baseline artifact; that workflow has not been run for it, no comparison runs on a schedule and no
numbers are recorded here. Not covered: real sockets or TLS, the reconnect storm
with stalled clients, persisted state, multi-threaded writers. E3-4 stays open.
Verification evidence for WP51 (issue #253), second slice, not an exit criterion: `clients/python` is a hand-written Python
port (standard library only, Python 3.10 or later, typed) of the same three parts, checked against the same
`conformance/vectors.json`, which it does not regenerate. Local run: 80 tests passed on Python 3.10, 3.11, 3.12, 3.13 and 3.14
(20,000 randomised cases per property on 3.14), including every vector decoded and re-encoded byte for byte with the same
error kind and detail as Rust, the Rust client scenarios and `mypy --strict` clean. Sixteen deliberate defects were patched in one at a time (eleven
in the codecs, five in the client; for example a loosened text length bound, a stripped byte order mark, a skipped LSN 0 check,
cursor order without length first, resume from the delivered instead of the acknowledged cursor, ping and ack-batch off by one) and each
made the tests fail; ten of the eleven codec defects were caught by the conformance vectors alone (a `bool` accepted as a number, which no vector contains, by the unit tests). The workflow `clients-python.yml` has not run on CI yet.
The Go client, a generator and the agent plugin contract in Python are not done, and no client talks to a server, because none exists.

Verification evidence, not an exit criterion: the text CRDT operation, tree move and workflow conflict decoders
have fuzz targets `crdt_text_decode`, `crdt_tree_decode` and `conflict_decode` (34 checked-in seeds), registered
in the nightly, weekly and manual 24-hour matrices; `tools/check_fuzz_matrices.py` now also requires seeds for
every target. A one-off local read of each seed through its decoder gave the intended accept or refusal; no
scheduled or manual fuzz run of the three targets has been observed, so no fuzz duration is claimed.


Verification evidence for WP40 (issue #242), not an exit criterion: `lattice-proto::websocket` (re-exported as `lattice-api::websocket`) is a sans-I/O RFC 6455 server-side codec (upgrade request parsing, accept key, frame decode and encode, message reassembly, close handshake, ping and close timers on a caller-supplied logical clock) in the first slice of the sync gateway. There is no listener, no TLS or authentication wiring and no real-socket test, so reconnect with cursor and the TLS and SCRAM requirement on non-loopback binds are not exercised. One local run of `cargo test -p lattice-api --lib websocket` (60 tests, default proptest settings, 0 failures) covers the RFC 6455 section 5.7 byte examples and the accept-key vector, hand-written edge cases (unmasked client frame, reserved bits and opcodes, non-minimal lengths, oversize frame refused from the header, fragmented messages with interleaved ping, invalid UTF-8, close payloads of one byte and invalid codes, timers) and properties (encode then decode round trips, fragmented text reassembly, arbitrary bytes never panic). One local 60 second run of the `websocket_frame` fuzz target from its 25 seeds: 2,086,004 executions, no crash; it is added to the nightly, weekly and 24-hour matrices, and those runs have not completed. Mutation testing, Miri and the full `cargo xtask ci` gate were not run. ADR 0027 (proposed) records WebSocket first and the data rule for QUIC. Adds the `sha1` crate (RFC 6455 accept key only). Update 2026-10-31: the listener, TLS and the bearer-token wiring that this paragraph calls missing are on `main` now (issues #524 and #578, paragraphs below).

Verification evidence for issue #521 part 1 (bearer-token authentication, ADR 0031), not an exit criterion: `lattice-api::bearer` adds the token format `lat_<id>_<secret>`, the strict credential file parser and renderer, token generation from caller-supplied random words, the pure verifier `verify_bearer` and a per-address failure limiter, and `latticed` adds `load_credential_store` and `write_credential_store` (mode, directory and size checks; atomic replace). Update 2026-10-30: this paragraph describes the state when its evidence was taken. The gateway (#524), the reload and revocation (#577) and the credential tool (#582) have since landed; each has its own paragraph below. Adds planted bug `TokenAcceptsPrefix` (group `Bug::BEARER`, registered in `Bug::GROUPS`) and the fuzz target `bearer_verify` (registered in the nightly, weekly and 24-hour matrices, seeded with 23 files; no scheduled run has completed). Not run locally: the full `cargo xtask ci` gate, mutation testing, Miri, a timing measurement of the verifier, the fuzz target itself (it type-checks; a seed replay test runs in the gate). Expectations in the unit tests are hand-written values computed outside the crate (Python `hashlib` and `base64`). No new crate in the tree: `latticed` now depends directly on `libc` (already locked at 0.2.191) for `O_NONBLOCK` and `O_NOFOLLOW`.

Verification evidence, not an exit criterion: WebSocket gateway part 1 (issue #524, part of #518 and #567; part 2 is #578). `latticed::ws` is a separate bounded listener (`--gateway-listen`, loopback only: a non-loopback address is refused at start because there is no TLS on it yet) with its own connection and upgrade counters; it shares nothing with the PostgreSQL listener's workers, cancel registry or TLS limiter, and the same-port protocol sniff of the issue was not built. One thread per connection reads the socket in slices (default 20 ms), compares the commit-notifier token (`CommitWake`, the newest LSN) with the last one it used and, when it moved or the fallback interval (1 s) passed, calls `SubscriptionHost::poll` against the executor's feed source, so policy is applied where it always was. The delivery delay of a wake is therefore up to one slice, not zero: a thread cannot block on a socket and a condition variable at once. Real-socket tests in `crates/latticed/tests/ws_sync.rs` (16 named here, one local run, 0 failures; the revoked slow-reader test described below is not part of that run and was not run locally; the other `latticed` tests were not re-run in full): the allowed account receives the rows and a live commit, an account with a token and no grant gets the same `invalid_scope` bytes as for a table that does not exist, a `Hello` that names another account changes nothing, missing, malformed and wrong tokens all get the same 401 and the fourth attempt of an address gets 429 even with the right token, a duplicate `Authorization` header, a query string and an unknown path are refused, wrong origin (403), subprotocol (400) and version (426), an oversize head (431), a text message (close 1003), two frames in one message (`malformed`, close 1002), a trickled head cut by the handshake timeout, the connection cap (503), a commit delivered with the fallback set to 60 s, reconnect with the acknowledged cursor after a dropped socket (rows after it, none repeated, compared with a reference reader; this is the evidence shape of E3-3, not E3-3), credential replacement closing a live connection (1008) and shutdown closing with 1001. Planted bugs `Bug::GATEWAY` (`GatewayDuplicateAuthorizationAccepted`, `GatewayQueryStringAccepted`, `GatewayAuthFailureNotCounted`, `GatewayConnectionCapIgnored`, `GatewayRevalidationSkipped`) are each caught by a unit witness in `ws.rs` that runs on the test thread (the switch is per thread, so the socket tests cannot flip it). Not done: TLS on the gateway and plaintext refusal beyond the loopback rule (#578), idle deadline (#578), byte budget and per-principal limits (#529; per-connection output memory is bounded by `max_subscriptions * max_frames_per_pump` frames of up to 1 MiB), security event log and latency histogram (#530), credential reload, rotation overlap and a revocation signal (done in #577, see its paragraph), `Welcome.max_payload` is 4096 so a client frame above that is refused. Known gaps for the #578 hardening notes: (L1) one client can hold all 8 pre-authentication upgrade slots until its handshake deadline, there is no per-address cap on them; (L2) a successful token verification clears the failure counter of the whole address, so a client that interleaves valid logins with guesses is never limited, and behind a shared address (a proxy once TLS exists) one account's success resets everyone's count; a connection thread that panics ends only itself and is not reported. A write to a reader that stopped is bounded by one absolute 15 s budget per step (poll or request) and the stop flag and the credential re-check are looked at between frames, with `a_revoked_slow_reader_is_cut_between_frames_not_at_the_end_of_the_step`, which counts events instead of timing them: one step of 1,000 frames of about 30 KB is cut by a credential replacement after 40 events, and the test requires that at most 601 events arrive (40, plus 16 MiB of socket buffers at 30 KB a frame, plus 2; the 16 MiB is an assumed bound on the two socket buffers, not measured here), so a connection that still sends the whole step fails (not run locally). The test is in the default test set and has no `#[ignore]`; no planted bug, because the switch is per thread and the connection runs on its own. An accepted socket is set blocking explicitly (it can inherit the listener's non-blocking mode). Mutation testing, Miri, fuzzing of the listener and the full `cargo xtask ci` gate were not run locally. Update 2026-10-31: TLS on the gateway port is on `main` (`4b297405`, issue #578). A non-loopback `--gateway-listen` needs `--tls-cert`, `--tls-key` and `--gateway-tokens`, and `latticed::ws::check_gateway_exposure` (`crates/latticed/src/ws.rs`, line 337) refuses the start otherwise. The gateway's own not-done list still holds: the idle deadline (#578), the byte budget (#529 part 2) and the security event log. Update 2026-10-31: the per-principal connection and long-poll limits are on the branch of #529 part 1, see the paragraph at the end of this section.

Verification evidence for issue #577 (live token reload, revocation and expiry, ADR 0031), not an exit criterion: `latticed::tokens::CredentialWatch` re-reads the bearer-token file at most once per `credential_poll` (default 1 s, at most 5 s) when its signature changes, when SIGHUP asked (only with `--gateway-tokens`), and on every check while the last load failed. A file that does not load keeps the tokens in use; after ten minutes of failed loads every token is refused. The gateway replaces its store and re-checks each live connection's token at once; a revoked or expired token closes its connection with 1008 (expiry is caught at the 5 second revalidation). Evidence: unit tests `a_changed_file_is_loaded_at_the_first_check_after_the_poll_interval`, `a_torn_file_or_a_broader_mode_keeps_the_store_and_is_retried_without_repeating_the_report`, `a_file_broken_past_the_refusal_window_refuses_every_token_once_then_loads_again`, `a_reload_request_rereads_an_unchanged_file_once` and `rotation_allows_two_tokens_for_a_bounded_overlap_then_one` (`crates/latticed/src/tokens.rs`); real-socket tests `revoked_token_closes_live_gateway_connections`, `an_expiring_token_closes_its_live_connection_at_the_expiry`, `a_broken_credential_file_keeps_the_tokens_in_use` and `a_missing_credential_file_stops_the_start` (`crates/latticed/tests/ws_sync.rs`, 50 ms poll); planted bug `RevokedTokenStillAccepted` caught by `a_planted_revoked_acceptance_is_caught_by_the_witness` (`lattice-api::bearer`) and `a_planted_revoked_acceptance_is_caught_by_the_revalidation_witness` (`latticed::ws`); the SIGHUP counter by `a_reload_request_is_counted_apart_from_stop_signals` (`lattice-sys`). A mutation check with the reload tick disabled failed the two file-driven socket tests. Not done: the Windows ACL check (the file is refused there), the security event log (a reload or refusal is one stderr line), reload of SCRAM verifiers, the decoy secret and account policies, and the gateway idle deadline (#578; TLS on the gateway port is on `main` since `4b297405`). Not run here: the nightly, fuzz and mutation jobs (CI). SIGHUP through the real `latticed` process is NOT verified end to end: the evidence is the `lattice-sys` counter test and the unit test with a bumped request count, and no test sent a signal to the binary. The SIGHUP handler is installed after the server and the gateway have started (`main.rs`, just before the startup banner is printed), so a SIGHUP in that gap still ends the process with the default action; with `--gateway-tokens` set, the daemon no longer dies on terminal hangup, because SIGHUP is caught after start. A header-only credential file (a valid `lattice-bearer-tokens v1` line and nothing else) parses as an empty store and revokes every token: that is fail-closed and intended, and the writer's atomic rename makes a torn file unlikely, but an operator who truncates the file by hand revokes everything. The reload reads the file on the gateway's accept thread, with `O_NONBLOCK` and a size bound; a hung network filesystem holding the credential file would therefore stall accepts until the read returns, so keep the file on local storage.

Verification evidence for issue #582 (credential tool), not an exit criterion: `lattice account init|add|list|remove|token create|token revoke|token list` (crate `lattice-cli`, module `account`) writes the SCRAM verifier file, the 32-byte decoy secret, the `tables:` policy file and the bearer-token file; `latticed::files` holds the shared owner-only file rules (atomic replace, create-only, bounded private read) that `load_credential_store` and `write_credential_store` now use too, and `lattice-api` gains `Verifier::to_line`, `CredentialStore::{get, revoke, revoke_account, set_expiry}`, `TokenId::parse` and a public `valid_account_name`. Tests: the verifier line equals the RFC 7677 credential for user/pencil and a second vector computed with Python `hashlib`; `account_add_writes_a_verifier_the_server_accepts` (unit and `crates/lattice-cli/tests/account_scram.rs`, where files written by the real binary are loaded the way `latticed` loads them and a SCRAM-SHA-256 exchange over certificate-verified TLS succeeds with a client proof computed from the password alone, wrong password and unknown account fail); `generated_token_has_at_least_256_bits_and_is_never_stored_in_clear`; `files_are_created_0600_and_refuse_overwrite`; `account_names_are_validated_like_the_credential_parser` (the oracle is `Accounts::parse`). Planted bugs `AccountRemoveKeepsTokens`, `AccountFileOverwritten`, `WeakPasswordAccepted` and `AccountNoLock` (group `Bug::ACCOUNT_TOOL`) are each caught by a test that enables them. After an independent security review: table names are validated before a policy line is composed (a newline or tab cannot start another account's line), every mutating command holds an exclusive `DIR/.lock` across its read-modify-write (a revocation racing a token creation is never undone), `init` removes what it created when it fails, expired tokens count as expired and `token prune` plus an automatic prune keep the 1024-entry file usable, a clock reading of 0 is refused, and `--password-stdin` refuses a terminal. The `lattice-cli` to `latticed` dependency edge stays (#686). Update 2026-10-30: `latticed --gateway-tokens` reads the token file since #577. Not done: no Windows support, no TLS certificate generation, the fuzz target is the shared `bearer_verify` (no second target), the `latticed` binary itself was not started with the produced files (the test uses the same `Accounts::parse` and `ServerHandle` path in process), and the full gate, workspace clippy, mutation testing and Miri were left to CI.

Verification evidence for issue #529 part 1 (per-principal connection and long-poll limits), not an exit criterion: `latticed::ws` limits the connections one principal holds (`--gateway-max-connections-per-principal`, default 8), keeps places of `--gateway-max-connections` for principals that hold none (`--gateway-reserved-connections`, default 4: a principal that already holds a connection is refused while more than 28 of the default 32 are open) and caps `lattice_subscribe` calls with a `wait_ms` above zero per principal (4) and on the whole gateway (16). The principal is known only after the token verified, so the check runs there, before the `101` and after the global and handshake counters; `POST /mcp` takes the same connection share and holds it, with the long-poll place, until the request ends (response written and linger over), as long as it holds its live-connection place, so a client that never reads cannot keep the global place while its own count is free. A handshake that has not authenticated yet counts in the open connections but not in any principal's share, so it can lower the effective reserved boundary for a moment. `max_long_polls_per_principal` at or above `max_long_polls` leaves the global cap as the only limit, and `reserved_connections` at or above `max_connections` gives every principal one connection and no second. A refusal is a `503` with `Retry-After: 1` and is counted (`GatewayHandle::limit_refusals`: global, per principal, reserved share, long polls). Every place is held by a guard that releases it when the connection thread or the call ends, a panic included. Tests: unit tests in `ws.rs` (`one_principal_stops_at_its_cap_and_others_are_not_affected`, `the_reserved_places_are_only_for_principals_that_hold_none`, `long_poll_places_are_capped_per_principal_and_globally_and_come_back`, `a_connection_that_panics_gives_its_place_back`, `nothing_is_kept_for_a_principal_that_holds_nothing`, `the_limits_are_validated_before_binding`) and in `mcp_http.rs` (`only_a_subscribe_that_may_wait_is_a_long_poll`); real-socket tests in `crates/latticed/tests/ws_limits.rs` (`one_principal_cannot_exhaust_connections_for_others`, `the_reserved_share_is_kept_for_a_principal_that_holds_no_connection`, `long_poll_workers_are_capped_per_principal_and_globally`, `an_mcp_request_counts_against_the_connections_of_its_principal`), which wait for a state (open connections, long polls waiting, both read from the gateway) in a bounded loop and assert on the state and the refusal counters, never on a duration. The cap is witnessed by the existing planted bug `GatewayConnectionCapIgnored`, which now also disables the per-principal and long-poll caps (`a_planted_ignored_principal_cap_is_caught_by_the_witness`); no new `Bug` variant was added, because `bugs.rs` belongs to another work package. Not done: the output byte budget and slow-reader disconnect with the last acked cursor (#529 part 2, #617), events per second and a per-principal executor budget (`one_principal_cannot_monopolise_the_executor`), the property `queued_bytes_are_bounded_by_config_times_connections`, and `backpressure.rs` is still not wired to the gateway. The reserved boundary reads the open-connection count outside the lock, so concurrent upgrades can overshoot it by the number of upgrades in flight, never the global cap. A principal that opens many short requests is not rate limited, only its concurrent holdings are. Not run locally (owner rule): the new tests and clippy ran on CI only.
