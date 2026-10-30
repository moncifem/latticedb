# tla

TLA+ models checked by TLC. Each header states what the model covers and what it leaves out.

| Model | Property |
|---|---|
| `Commit` | an acknowledged change set is on the disk and survives power cuts, and recovery only hands out what it has made durable |
| `Hlc` | the issued timestamps strictly increase even when the physical clock goes backwards, and the counter stays bounded |
| `Fence` | across a crash, no write from an older turn is accepted once a newer turn has begun: the turns in the log are checked, and the in-memory turn is rebuilt from the log |
| `SetConvergence` | replicas that applied the same operations, in any order, hold the same members (needs the tombstone rule; checked with two tags) |
| `TreeMove` | move-operation tree CRDT with delete as move to trash: no cycles at any replica, every node reaches the root or the trash, and replicas that logged the same operations hold the same tree, under delayed, reordered and duplicated delivery |

`TreeMove` keeps a materialised tree per replica and updates it with the undo, perform, redo rule of
`lattice-core/src/crdt_tree.rs`; `Convergent` compares two replicas and `MatchesSerial` compares each
replica with a fresh replay of its own log. Its checked bounds are 2 replicas, 3 nodes, 4 operations and a depth cap of 2 (`MaxDepth`, the model of
`MAX_TREE_DEPTH`; `DepthBounded` is the invariant).
The model keeps the whole log. Op-log compaction (`Tree::compact` in `crdt_tree.rs`) is not modelled in TLA+;
it is covered by a property test against an uncompacted oracle and rests on the causal-stability contract
stated in that module's docs.

`TreeMoveCycleOnly` (a thin module extending `TreeMove`, config with `CheckDepth = FALSE`) checks the same
invariants except `DepthBounded` with the depth rule off. With the rule on, every move that would form a cycle is
also refused by the depth check, so `TreeMove` alone no longer shows that the cycle rule's decisions converge
under reordering. `check-all.sh` runs every `.cfg`, so it picks this one up.

Negative tests: each config in `negative/` removes one protection from a model, and
`check-negative.sh` requires TLC to fail with the named invariant. `TreeMoveNoCycleCheck` must violate
`Acyclic`, `TreeMoveNoUndoRedo` must violate `Convergent` and `TreeMoveNoDepthCheck` must violate
`DepthBounded`. A negative run that passes, or fails with
a different invariant, fails `check-all.sh`.

The other four models switch a protection off with a boolean constant, one per planted bug of the
kernel, and their properties are stated over observable state (the disk, the log, the history of issued
timestamps, the members), not over the guard of the action that the switch removes:

| Config | Switch | Must violate | Kernel planted bug |
|---|---|---|---|
| `CommitAckBeforeSync` | `AckBeforeSync` | `AckedAreDurable` | `AckBeforeSync` |
| `CommitSkipFsync` | `SkipFsync` | `AckedAreDurable` | `SkipFsync` |
| `CommitSkipFsyncThenCrash` | `SkipFsync` | `AckedSurviveCrash` | `SkipFsync` |
| `CommitSkipRecoverySync` | `SkipRecoverySync` | `MemoryIsDurable` | `SkipRecoverySync` |
| `FenceIgnored` | `FenceIgnored` | `AcceptOnlyCurrent` | `FenceIgnored` |
| `FenceIgnoredGoesBack` | `FenceIgnored` | `NeverGoesBack` | `FenceIgnored` |
| `FenceIgnoredFuture` | `FenceIgnored` | `NothingFromTheFuture` | `FenceIgnored` |
| `FenceNotRebuilt` | `FenceNotRebuilt` | `AcceptOnlyCurrent` | `FenceNotRebuilt` |
| `FenceNotRebuiltGoesBack` | `FenceNotRebuilt` | `NeverGoesBack` | `FenceNotRebuilt` |
| `HlcRegress` | `Regress` | `StrictlyIncreasing` | `HlcRegress` |
| `HlcCounterStuck` | `CounterStuck` | `StrictlyIncreasing` | `HlcCounterStuck` |
| `SetConvergenceNoTombstone` | `Tombstones = FALSE` | `Convergent` | none (a model-only protection) |

`Commit` over-approximates recovery: `Replay` is enabled at any time, not only after a `Crash`.
`AckedSurviveCrash` is implied by `AckedAreDurable` and `disk \subseteq cache`, so it is redundant in the
positive config and only non-vacuous in `CommitSkipFsyncThenCrash`; `MemoryIsDurable` restates the effect of
`Replay` and can only fail through `SkipRecoverySync`.

Each config carries the violating trace in a comment. The traces were worked out by hand and checked
with a throwaway breadth-first re-implementation of each model in Python; TLC has not yet run the
changed models, so the first nightly run is the real check.

Run one with `tla/run-tlc.sh tla/<Model>.tla`, or all (models, then the negative tests) with `tla/check-all.sh`. Both need `java` and
`TLA2TOOLS` pointing at `tla2tools.jar`. Nightly CI runs all of them.
