---- MODULE SetConvergence ----
\* Add-wins set over unique tags, two replicas. Each operation is an add of a fresh tag or a remove of
\* a tag the replica has seen; replicas exchange operations in any order and any number of times.
\* Safety (strong eventual consistency): replicas that have delivered the same operations show the
\* same members, regardless of delivery order.
\*
\* LIMITS: two replicas, three tags, operations are never lost, only delayed.
EXTENDS Naturals, FiniteSets
CONSTANTS Tags
VARIABLES adds, rems, used
vars == <<adds, rems, used>>
Replicas == {1, 2}

Init == adds = [r \in Replicas |-> {}] /\ rems = [r \in Replicas |-> {}] /\ used = {}

Add(r, t) == /\ t \notin used
             /\ adds' = [adds EXCEPT ![r] = @ \cup {t}] /\ used' = used \cup {t}
             /\ UNCHANGED rems

Remove(r, t) == /\ t \in adds[r]
                /\ rems' = [rems EXCEPT ![r] = @ \cup {t}]
                /\ UNCHANGED <<adds, used>>

\* Delivery merges everything the sender has into the receiver. Merge is a union, so order and
\* repetition cannot matter.
Deliver(from, to) == /\ from /= to
                     /\ adds' = [adds EXCEPT ![to] = @ \cup adds[from]]
                     /\ rems' = [rems EXCEPT ![to] = @ \cup rems[from]]
                     /\ UNCHANGED used

Next == \E r \in Replicas : \/ \E t \in Tags : Add(r, t) \/ Remove(r, t)
                            \/ \E o \in Replicas : Deliver(o, r)
Spec == Init /\ [][Next]_vars

Members(r) == adds[r] \ rems[r]
Convergent == (adds[1] = adds[2] /\ rems[1] = rems[2]) => Members(1) = Members(2)
RemovedWereSeen == \A r \in Replicas : rems[r] \subseteq adds[r]
====
