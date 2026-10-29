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

None yet. The kernel already has last-writer-wins registers and an observed-remove set
(`lattice-core::crdt`), which Phase 3 extends.

Verification evidence, not an exit criterion: `tla/TreeMove.tla` (WP43, issue #245) models the tree CRDT
move rule with 2 replicas, 3 nodes and 4 operations. One local run with a single worker explored
6,635,881 states (751,243 distinct) in 45 to 48 s (two runs) with no violation of `Acyclic`, `Rooted`, `Convergent` or
`MatchesSerial`. With the cycle check removed TLC reports `Acyclic` violated, and with undo and redo of
later-ordered operations removed it reports `Convergent` violated (`tla/check-negative.sh`). Not run
on CI yet; simulation, row integration and op-log growth measurement of WP43 are still open.

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
`lattice-sdk`. A 100,000 seed per profile volume test exists as an ignored test and has not been run.
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
run locally. Not wired into the nightly shards, ledger or scorecard yet.
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
on CI yet. The Go and Python clients, a generator and the agent plugin contract in TypeScript are not done, and no client
talks to a server, because none exists.
