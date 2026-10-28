---- MODULE Fence ----
\* Turn fencing. A session's writes carry the turn that issued them; beginning a newer turn fences the
\* older one. Safety: once a turn has begun, no write from an earlier turn of that session is accepted,
\* so the accepted writes in the log never go back to an older turn.
\*
\* LIMITS: one session; writes are accepted or rejected atomically at the log.
EXTENDS Naturals, Sequences
CONSTANTS MaxTurn, MaxLog
VARIABLES cur, log
vars == <<cur, log>>

Init == cur = 0 /\ log = <<>>

Begin == cur < MaxTurn /\ cur' = cur + 1 /\ UNCHANGED log

\* A writer holding turn t submits. It is accepted only if t is still the current turn.
Write(t) == /\ Len(log) < MaxLog
            /\ IF t = cur THEN log' = Append(log, t) ELSE UNCHANGED log
            /\ UNCHANGED cur

Next == Begin \/ (\E t \in 0..MaxTurn : Write(t))
Spec == Init /\ [][Next]_vars

NeverGoesBack == \A i, j \in 1..Len(log) : i < j => log[i] <= log[j]
NothingFromTheFuture == \A i \in 1..Len(log) : log[i] <= cur

\* The fencing rule itself: whatever is accepted carries the turn that is current at that moment.
AcceptOnlyCurrent == [][ Len(log') > Len(log) => log'[Len(log')] = cur ]_vars
====
