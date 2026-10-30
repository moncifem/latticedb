---- MODULE TreeMove ----
\* Move-operation tree CRDT (Kleppmann, Mulligan, Gomes, Beresford), the algorithm in
\* lattice-core/src/crdt_tree.rs. Two replicas. Every change is a move of a node under a parent,
\* stamped with a unique timestamp that is also its position in one total order. A delete is a move
\* under the tombstone root Trash. A move whose parent is unknown, or that would make the node its
\* own ancestor, or that would put the node or anything below it deeper than MaxDepth (the depth cap
\* of lattice-core, G-08), is recorded but does nothing, identically on every replica.
\*
\* Delivery: an operation is broadcast when created and each replica integrates it whenever it likes,
\* so delivery is delayed, reordered and (the Deliver action also fires for operations already
\* integrated) duplicated. Nothing is lost; there is no fairness, so this checks safety only.
\*
\* An operation that arrives out of order is integrated like the Rust code does it: undo every logged
\* operation ordered after it (newest first, restoring the parent it replaced), perform it, then redo
\* the undone ones against the new state. Each replica keeps a materialised tree (`tree`) and a log of
\* entries (operation, parent it replaced, whether it took effect). The tree is never recomputed from
\* the operation set while the model runs, so the properties below are about the integrate rule:
\*
\*   Acyclic       no node is its own ancestor, at every replica, in every state
\*   DepthBounded  no node is deeper than MaxDepth (Root and Trash are depth 0)
\*   Rooted        every node reaches Root or Trash (no dangling parent, so deleted subtrees stay
\*                 whole under Trash and a move into one lands in the trash too)
\*   Convergent    replicas that logged the same operations hold the same tree
\*   MatchesSerial a replica's tree equals a fresh replay of its own log in timestamp order, so the
\*                 tree depends on the operation set and not on delivery order
\*
\* CheckCycles, Reorder and CheckDepth switch the three protections off. tla/negative/*.cfg do that,
\* and tla/check-negative.sh requires TLC to report the matching violation: without the cycle check
\* Acyclic fails, without undo/redo Convergent fails, without the depth rule DepthBounded fails.
\* The Rust code keeps the height of each subtree instead of walking it; the model checks every node's
\* depth after the move, which is the same condition.
\*
\* LIMITS: sibling positions are not modelled (they do not affect parentage); timestamps are plain
\* integers, not hybrid clocks; no crash, no snapshot, no log compaction; two replicas, three nodes
\* and four operations in the checked configuration.
EXTENDS Naturals, FiniteSets, TLC
CONSTANTS Nodes, Root, Trash, None, MaxTs, MaxDepth, CheckCycles, CheckDepth, Reorder
VARIABLES tree, log, sent, used
vars == <<tree, log, sent, used>>

Replicas == {1, 2}
Targets == Nodes \cup {Root, Trash}
Stamps == 1..MaxTs
K == Cardinality(Nodes)

Move == [ts : Stamps, node : Nodes, parent : Targets]
Entry == [ts : Stamps, node : Nodes, parent : Targets, old : Targets \cup {None}, ok : BOOLEAN]
Empty == [n \in Nodes |-> None]

NodeSymmetry == Permutations(Nodes)

\* Does walking up from `from` reach `target` (an ancestor-or-self test)? Fuel K bounds the walk, so a
\* cycle that already exists is reported as reaching, never as an infinite loop.
RECURSIVE Reaches(_, _, _, _)
Reaches(tr, from, target, k) ==
  IF from = target THEN TRUE
  ELSE IF k = 0 \/ from \notin Nodes THEN FALSE
  ELSE IF tr[from] = None THEN FALSE
  ELSE Reaches(tr, tr[from], target, k - 1)

RECURSIVE ReachesTop(_, _, _)
ReachesTop(tr, from, k) ==
  IF from \in {Root, Trash} THEN TRUE
  ELSE IF k = 0 \/ from \notin Nodes THEN FALSE
  ELSE IF tr[from] = None THEN FALSE
  ELSE ReachesTop(tr, tr[from], k - 1)

\* Number of steps from `x` up to Root or Trash, bounded by fuel k.
RECURSIVE Depth(_, _, _)
Depth(tr, x, k) ==
  IF x \in {Root, Trash} THEN 0
  ELSE IF k = 0 \/ tr[x] = None THEN 0
  ELSE 1 + Depth(tr, tr[x], k - 1)

\* Would every node stay within MaxDepth after the move?
Fits(tr, m) ==
  LET t2 == [tr EXCEPT ![m.node] = m.parent]
  IN \A x \in Nodes : t2[x] = None \/ Depth(t2, x, K) <= MaxDepth

\* Decide one move against `tr` and perform it if valid. Returns the new tree and the log entry.
Perform(tr, m) ==
  LET known == m.parent \in {Root, Trash} \/ tr[m.parent] # None
      cycle == CheckCycles /\ Reaches(tr, m.parent, m.node, K)
      ok    == known /\ ~cycle /\ (~CheckDepth \/ Fits(tr, m))
  IN [tree  |-> IF ok THEN [tr EXCEPT ![m.node] = m.parent] ELSE tr,
      entry |-> [ts |-> m.ts, node |-> m.node, parent |-> m.parent, old |-> tr[m.node], ok |-> ok]]

Has(L, t) == \E e \in L : e.ts = t
At(L, t)  == CHOOSE e \in L : e.ts = t

\* Undo logged operations with timestamp in (lo, t], newest first.
RECURSIVE UndoDown(_, _, _, _)
UndoDown(tr, L, t, lo) ==
  IF t <= lo THEN tr
  ELSE IF Has(L, t)
       THEN UndoDown(IF At(L, t).ok THEN [tr EXCEPT ![At(L, t).node] = At(L, t).old] ELSE tr, L, t - 1, lo)
       ELSE UndoDown(tr, L, t - 1, lo)

\* Redo logged operations with timestamp in [t, MaxTs], oldest first, recomputing each entry.
RECURSIVE RedoUp(_, _)
RedoUp(st, t) ==
  IF t > MaxTs THEN st
  ELSE IF Has(st.log, t)
       THEN LET e == At(st.log, t)
                p == Perform(st.tree, [ts |-> e.ts, node |-> e.node, parent |-> e.parent])
            IN RedoUp([tree |-> p.tree, log |-> (st.log \ {e}) \cup {p.entry}], t + 1)
       ELSE RedoUp(st, t + 1)

\* Integrate move m into a replica's (tree, log). Duplicates are filtered out before this is called.
Integrate(tr, L, m) ==
  LET base == IF Reorder THEN UndoDown(tr, L, MaxTs, m.ts) ELSE tr
      p    == Perform(base, m)
      L1   == L \cup {p.entry}
  IN IF Reorder THEN RedoUp([tree |-> p.tree, log |-> L1], m.ts + 1)
                ELSE [tree |-> p.tree, log |-> L1]

\* Oracle: replay a log from the empty tree in timestamp order, ignoring undo and redo.
RECURSIVE Replay(_, _, _)
Replay(tr, L, t) ==
  IF t > MaxTs THEN tr
  ELSE IF Has(L, t)
       THEN Replay(Perform(tr, [ts |-> t, node |-> At(L, t).node, parent |-> At(L, t).parent]).tree, L, t + 1)
       ELSE Replay(tr, L, t + 1)

Init == /\ tree = [r \in Replicas |-> Empty]
        /\ log  = [r \in Replicas |-> {}]
        /\ sent = {}
        /\ used = {}

Take(r, m) ==
  LET res == Integrate(tree[r], log[r], m)
  IN /\ tree' = [tree EXCEPT ![r] = res.tree]
     /\ log'  = [log EXCEPT ![r] = res.log]

\* A replica makes a move (a delete when parent = Trash) and applies it locally at once. The timestamp
\* is above everything the replica has seen, but other replicas may already hold later ones.
Create(r) ==
  \E t \in Stamps, n \in Nodes, p \in Targets :
    /\ t \notin used
    /\ \A e \in log[r] : e.ts < t
    /\ LET m == [ts |-> t, node |-> n, parent |-> p]
       IN /\ sent' = sent \cup {m}
          /\ used' = used \cup {t}
          /\ Take(r, m)

\* Any broadcast operation reaches a replica at any time, in any order, any number of times. One that
\* is already in the log is ignored by timestamp.
Deliver(r) ==
  \E m \in sent :
    /\ UNCHANGED <<sent, used>>
    /\ IF Has(log[r], m.ts) THEN UNCHANGED <<tree, log>> ELSE Take(r, m)

Next == \E r \in Replicas : Create(r) \/ Deliver(r)
Spec == Init /\ [][Next]_vars

TypeOK == /\ tree \in [Replicas -> [Nodes -> Targets \cup {None}]]
          /\ \A r \in Replicas : log[r] \subseteq Entry
          /\ sent \subseteq Move

Acyclic == \A r \in Replicas, n \in Nodes :
             tree[r][n] = None \/ ~Reaches(tree[r], tree[r][n], n, K)

DepthBounded == \A r \in Replicas, n \in Nodes :
                  tree[r][n] = None \/ Depth(tree[r], n, K) <= MaxDepth

Rooted == \A r \in Replicas, n \in Nodes :
            tree[r][n] = None \/ ReachesTop(tree[r], tree[r][n], K)

Stamped(r) == {e.ts : e \in log[r]}

Convergent == Stamped(1) = Stamped(2) => tree[1] = tree[2]

MatchesSerial == \A r \in Replicas : tree[r] = Replay(Empty, log[r], 1)
====
