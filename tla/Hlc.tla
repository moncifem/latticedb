---- MODULE Hlc ----
\* Hybrid logical clock on one node. The physical clock may jump backwards. Safety: every timestamp the
\* node issues is strictly greater than every one it issued before, whatever the physical clock does.
\*
\* The property is stated over the history of issued timestamps (`hist`), not over the step that issues
\* the next one, so it can fail: the two switches turn off one rule each (`Regress` follows the physical
\* clock even when it moved back, `CounterStuck` does not advance the counter within one millisecond),
\* which mirror the kernel's planted bugs `HlcRegress` and `HlcCounterStuck`. tla/negative/Hlc*.cfg set
\* them and TLC must report StrictlyIncreasing violated.
\*
\* LIMITS: one node, no receive/merge of remote timestamps, counter bounded by MaxC, at most MaxIssued
\* timestamps.
EXTENDS Naturals, Sequences
CONSTANTS MaxP, MaxC, MaxIssued, Regress, CounterStuck
VARIABLES pt, l, c, hist
vars == <<pt, l, c, hist>>

Gt(l1, c1, l2, c2) == l1 > l2 \/ (l1 = l2 /\ c1 > c2)

Init == pt = 0 /\ l = 0 /\ c = 0 /\ hist = <<>>

\* The physical clock moves anywhere in range, including backwards.
Wall == pt' \in 0..MaxP /\ UNCHANGED <<l, c, hist>>

\* Issue a timestamp. At MaxC with a stalled physical clock the tick is refused, so no timestamp is
\* issued rather than a repeated one.
Tick == /\ Len(hist) < MaxIssued
        /\ IF Regress
              THEN /\ l' = pt
                   /\ IF pt = l THEN c < MaxC /\ c' = c + 1 ELSE c' = 0
              ELSE IF pt > l
                      THEN l' = pt /\ c' = 0
                      ELSE IF CounterStuck
                              THEN l' = l /\ c' = c
                              ELSE c < MaxC /\ l' = l /\ c' = c + 1
        /\ hist' = Append(hist, <<l', c'>>)
        /\ UNCHANGED pt

Next == Wall \/ Tick
Spec == Init /\ [][Next]_vars

CounterBounded == c <= MaxC

\* Every timestamp issued is strictly greater than all the earlier ones.
StrictlyIncreasing == \A i, j \in 1..Len(hist) :
                        i < j => Gt(hist[j][1], hist[j][2], hist[i][1], hist[i][2])
====
