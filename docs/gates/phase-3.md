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

Merged, none connected to a live sync server (the evidence paragraphs below give the measurements and
what was not run):

- Typed change-feed event model with a stable cursor, and the experimental change-feed and sync frame
  codecs (encodings not frozen).
- Shared protocol crate `lattice-proto` (layer 1, issue #356): the sync frame types and codec and the
  sans-I/O WebSocket codec moved out of `lattice-api` (still re-exported as `lattice-api::sync` and
  `lattice-api::websocket`), and the SDK client uses the same frame types instead of a mirror (ADR 0030,
  proposed). A code move with no wire change; the conformance vectors are unchanged.
- Sans-I/O WebSocket codec (`lattice-proto::websocket`) and the principal-bound `SubscriptionHost`
  (`lattice-api::subscribe`); no transport supplies a principal yet.
- CRDT peer limits and the tree CRDT with moves (`lattice-core`), with the `tla/TreeMove.tla` model.
- Agent plugin contract with a conformance suite, and the MCP server and SDK outbox in `lattice-sdk`.
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
per-response write deadline, best-effort TLS `close_notify` on every connection exit, a mode check on the key
and decoy-secret files (mode bits only; 0440 and 0444 mounts are refused) and a CancelRequest rate limit
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
the systemd and launchd snippets in the crate README under those supervisors, Windows (no signal handling there), and a
SIGTERM during recovery (it keeps the default action). The handler is one `signal(2)` declaration and call in
`lattice-sys`; no dependency was added.

## Exit criteria

| # | Criterion | State |
|---|---|---|
| E3-1 | I5 and I12 across 1,000,000 seeds per night for seven nights | Not started |
| E3-2 | Contract suite green for the reference plugin and all four SDKs | Not started |
| E3-3 | Reconnect with cursor yields zero gaps and duplicates | Not started |
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
`EventSource::read_after` gained one). No transport supplies a principal yet, the plugin sessions and the MCP caller
are not bound, so no end-to-end authorization claim is made. The kernel-backed source is the next paragraph.
Verification evidence, not an exit criterion: kernel-backed `EventSource` (issue #519, part of #518, sync part of
threat model G-03). `lattice-api::feed_source::FeedSource` (made by `ExecHost::feed_source`) serves `table/<name>` from
`RowStore::events_since_as` through the SQL executor's actor thread, as the principal's policy: the same
`PolicyTable` mapping the SQL handler uses (`PolicyTable::grant_for`), deny by default, no allow-all fallback. A
denied principal, an unknown table, an unserved scope string and a table hidden by `hiding_catalog` are all
`SourceError::UnknownScope`, decided before the cursor is looked at. `CursorTooOld` is `TooOld`; `MissingCommitTime` and
feed encoding errors are `Unavailable`; the 12-byte feed cursor is the wire cursor (empty is the origin; another length
or a cursor past the newest commit is `BadCursor`). The prefix-then-`KeyTooLarge` rule of `feed_emit` is handled inside the
source: it reads again from the last cursor it saw (whatever table) and skips exactly the blocked event, so a legacy
oversize key never wedges a reader and no event is lost or repeated. 14 unit tests (`cargo test -p lattice-api --lib
feed_source`, run locally once, default proptest settings): a property test over generated two-table histories
(inserts, updates, deletes, several rows per commit) checks that paging from the origin and from every event cursor, at
page sizes 1 to 4, equals a model built from the commit LSNs and key ranks and not from the feed; the denied answer is
the same `Output` as for an unknown table through a `SubscriptionHost`; a live tail, multi-row change sets with
`(lsn, index)` cursors, the vacuum floor, the oversize-key skip at every page size, and a bounded read racing a writer
on the same actor. Planted bug `Bug::FeedSourcePolicyDropped` (policy replaced by allow-all) is caught by
`a_dropped_policy_is_caught_by_the_witness`, with a clean control; two hand mutants (no skip of the blocked event, and a
reader that stops after one answer or ignores the table) each failed the oversize or model test. Not done: Cost of a read: `MvccTable::history_from` visits every key of every table (a `partition_point` per key, then collect and sort), so each read is O(total rows) even when the reader is caught up, plus O(versions after the cursor); `events_since_as` then builds events and clones values for every write after the cursor, including tables the reader may not see, before the policy filter, so memory per read is O(history after the cursor), not O(`max_events`). `max_events` bounds only what is returned and encoded. A running scan is never interrupted (cancellation is checked only before it starts), and a reader that is not caught up scans twice (a second read from the last cursor confirms the end). Only principals with an allow-all grant reach the scan today. The engine-side limit, a per-principal cap and an LSN index are issue #543. Open follow-up: issue #543. Table-level read check (issue #522, interim): the
precondition recorded here earlier is met for per-table grants. `ActorPolicy::can_read_table` is the one check the feed
(`Database::visible_table_id`) and the binder (`ExecCatalog`, `sees_table`) share, so a table the account holds no bit
on is `UnknownScope` for the feed and `42P01` for SQL, and a table it may see but not read is not served by the feed
either (write-only and ddl-only tables are `UnknownScope`). The two cannot drift: a denied table never reads as an empty
feed. The precondition still binds the Phase 4 policy language and any row-level policy. The
`BadCursor` check against the newest applied LSN lets a principal who can read one table binary-search the global commit
position, and gaps between cursor LSNs show activity on tables it cannot read (same class as issue #363, unchanged); a
denied request still occupies an actor queue slot. The filtered-feed gap question (issue #363) is unchanged; column definitions are not recovered across a restart
(#379); no transport supplies the principal and no wake-on-commit exists, so nothing here is live end to end.
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
`presence` bit (issue #626); the MCP backend (#526) has not been built so "alike for SQL, feed and MCP" is shown for SQL and
the feed only; grants live in a file that the operator edits and restarts for, not in the database; a transaction block
reads a table created after it began as empty; `w` without `r` can probe primary-key and `UNIQUE` values of the table through `23505` (PostgreSQL INSERT-only behaviour, pinned by a test); the BadCursor/LSN-gap side channel of the feed (#363) is unchanged.
Verification evidence, not an exit criterion: `lattice-engines::feed_emit` (WP36, issue #238) emits the change feed
from the row store's retained committed versions. One local run of `cargo test -p lattice-engines --lib feed_emit`
(6 tests, default settings): the feed from every cursor of 12 seeded 14-commit histories equals an independently
computed model (filtered by a value-based read policy in a second test); 8 kill-and-reopen histories re-emitted
identical events and resumed with no gap and no duplicate; three planted emitter defects (skipped event, reordered
entity events, inclusive resume) were each rejected by the oracle in more than 100 cases. A cursor at or below
the vacuum floor returns `CursorTooOld`. Author and turn are not recorded by the row store, so events carry author 0
and no turn. The full `cargo xtask ci` gate and the rest of the engines suite were not run locally.
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


Verification evidence for WP40 (issue #242), not an exit criterion: `lattice-proto::websocket` (re-exported as `lattice-api::websocket`) is a sans-I/O RFC 6455 server-side codec (upgrade request parsing, accept key, frame decode and encode, message reassembly, close handshake, ping and close timers on a caller-supplied logical clock) in the first slice of the sync gateway. There is no listener, no TLS or authentication wiring and no real-socket test, so reconnect with cursor and the TLS and SCRAM requirement on non-loopback binds are not exercised. One local run of `cargo test -p lattice-api --lib websocket` (60 tests, default proptest settings, 0 failures) covers the RFC 6455 section 5.7 byte examples and the accept-key vector, hand-written edge cases (unmasked client frame, reserved bits and opcodes, non-minimal lengths, oversize frame refused from the header, fragmented messages with interleaved ping, invalid UTF-8, close payloads of one byte and invalid codes, timers) and properties (encode then decode round trips, fragmented text reassembly, arbitrary bytes never panic). One local 60 second run of the `websocket_frame` fuzz target from its 25 seeds: 2,086,004 executions, no crash; it is added to the nightly, weekly and 24-hour matrices, and those runs have not completed. Mutation testing, Miri and the full `cargo xtask ci` gate were not run. ADR 0027 (proposed) records WebSocket first and the data rule for QUIC. Adds the `sha1` crate (RFC 6455 accept key only).
