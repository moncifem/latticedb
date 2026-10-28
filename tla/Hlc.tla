---- MODULE Hlc ----
\* Hybrid logical clock on one node. The physical clock may jump backwards. Safety: every timestamp the
\* node issues is strictly greater than the one before it, whatever the physical clock does.
\*
\* LIMITS: one node, no receive/merge of remote timestamps, counter bounded by MaxC.
EXTENDS Naturals
CONSTANTS MaxP, MaxC
VARIABLES pt, l, c, issued
vars == <<pt, l, c, issued>>

Gt(l1, c1, l2, c2) == l1 > l2 \/ (l1 = l2 /\ c1 > c2)

Init == pt = 0 /\ l = 0 /\ c = 0 /\ issued = 0

\* The physical clock moves anywhere in range, including backwards.
Wall == pt' \in 0..MaxP /\ UNCHANGED <<l, c, issued>>

Tick == /\ issued < 6
        /\ IF pt > l
              THEN l' = pt /\ c' = 0
              ELSE /\ c < MaxC /\ l' = l /\ c' = c + 1
        /\ issued' = issued + 1 /\ UNCHANGED pt

Next == Wall \/ Tick
Spec == Init /\ [][Next]_vars

\* Every tick strictly advances the clock pair.
StrictlyIncreasing == [][ (issued' = issued + 1) => Gt(l', c', l, c) ]_vars
====
