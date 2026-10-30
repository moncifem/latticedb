# tla

TLA+ models checked by TLC. Each header states what the model covers and what it leaves out.

| Model | Property |
|---|---|
| `Commit` | an acknowledged change set is durable and survives crashes |
| `Hlc` | timestamps strictly increase even when the physical clock goes backwards, and the counter stays bounded |
| `Fence` | no write from an older turn is accepted once a newer turn has begun |
| `SetConvergence` | replicas that applied the same operations, in any order, hold the same members (needs the tombstone rule) |
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

Run one with `tla/run-tlc.sh tla/<Model>.tla`, or all (models, then the negative tests) with `tla/check-all.sh`. Both need `java` and
`TLA2TOOLS` pointing at `tla2tools.jar`. Nightly CI runs all of them.
