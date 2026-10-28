---- MODULE SetConvergence ----
\* Add-wins set over unique tags, two replicas. Operations (add a fresh tag, remove a tag the origin
\* has seen) are broadcast and each replica applies them in whatever order they arrive, one at a
\* time, into its own materialised member set. Safety (strong eventual consistency): replicas that
\* have applied the same operations hold the same members, whatever the delivery order.
\*
\* The materialised state is deliberately separate from the set of applied operations, so the property
\* is about the apply rule, not about a function of the operations being a function. A remove that
\* arrives before its add leaves a tombstone; without it the replicas diverge (checked by hand: delete
\* the tombstone test in Apply and TLC reports the violation).
\*
\* LIMITS: two replicas, three tags, no operation is lost, only delayed. No fairness is modelled, so
\* this checks safety and not that replicas eventually receive everything.
EXTENDS Naturals, FiniteSets
CONSTANTS Tags
VARIABLES ops, applied, members, tombs, used
vars == <<ops, applied, members, tombs, used>>
Replicas == {1, 2}
Kinds == {"add", "rem"}
Op(k, t) == <<k, t>>

Init == /\ ops = {} /\ used = {}
        /\ applied = [r \in Replicas |-> {}]
        /\ members = [r \in Replicas |-> {}]
        /\ tombs = [r \in Replicas |-> {}]

\* Apply one operation at a replica, once.
Apply(r, o) == /\ o \in ops /\ o \notin applied[r]
               /\ applied' = [applied EXCEPT ![r] = @ \cup {o}]
               /\ IF o[1] = "add"
                     THEN /\ members' = [members EXCEPT ![r] = IF o[2] \in tombs[r] THEN @ ELSE @ \cup {o[2]}]
                          /\ UNCHANGED tombs
                     ELSE /\ members' = [members EXCEPT ![r] = @ \ {o[2]}]
                          /\ tombs' = [tombs EXCEPT ![r] = @ \cup {o[2]}]
               /\ UNCHANGED <<ops, used>>

\* A replica creates an operation and applies it locally at once.
Create(r, o) == /\ o \notin ops
                /\ \/ o[1] = "add" /\ o[2] \notin used
                   \/ o[1] = "rem" /\ Op("add", o[2]) \in applied[r]
                /\ ops' = ops \cup {o}
                /\ used' = IF o[1] = "add" THEN used \cup {o[2]} ELSE used
                /\ UNCHANGED <<applied, members, tombs>>

Next == \E r \in Replicas : \/ \E k \in Kinds, t \in Tags : Create(r, Op(k, t))
                            \/ \E o \in ops : Apply(r, o)
Spec == Init /\ [][Next]_vars

Convergent == applied[1] = applied[2] => members[1] = members[2]
====
