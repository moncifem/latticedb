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
One ignored test records a divergence: the kernel answers `Duplicate` where the plugin contract says `fenced`
for a late retry of an already stored write.
