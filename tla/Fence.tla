---- MODULE Fence ----
\* Turn fencing across a crash. A session's writes carry the turn that issued them; beginning a newer
\* turn fences the older one. The writer keeps the current turn in memory (`cur`) and the log holds
\* the durable truth: a begin record for every turn and a write record for every accepted write. A
\* crash loses `cur`; recovery rebuilds it from the begin records in the log.
\*
\* Safety, stated over the log and not over `cur` (so it is not the guard of Write written again): the
\* turn of every accepted write equals the highest turn begun earlier in the log. Hence no write from
\* a fenced (older) turn is accepted, no write carries a turn that has not begun, and the turns of the
\* accepted writes never go back. It can only hold if the in-memory fence always equals what the log
\* says, which is the part recovery must get right (invariant I12 of the kernel).
\*
\* The two switches are protections turned off, one per planted bug of the kernel (`FenceIgnored`,
\* `FenceNotRebuilt` in lattice-core); tla/negative/Fence*.cfg set them and TLC must report the
\* invariant that catches each.
\*
\* LIMITS: one session; a begin and a write are appended and acted on atomically, so the model does not
\* cover a crash between the begin record and the update of `cur`; the log is never torn or truncated.
EXTENDS Naturals, Sequences, FiniteSets
CONSTANTS MaxTurn, MaxLog, FenceIgnored, FenceNotRebuilt
VARIABLES cur, up, log
vars == <<cur, up, log>>

Max(S) == CHOOSE m \in S : \A x \in S : m >= x

\* The highest turn begun by a record before position j of the log (0 before the first begin).
BegunBefore(j) ==
  LET idx == {x \in 1..(j - 1) : log[x].kind = "begin"}
  IN IF idx = {} THEN 0 ELSE Max({log[x].turn : x \in idx})

Init == cur = 0 /\ up = TRUE /\ log = <<>>

Begin == /\ up /\ cur < MaxTurn /\ Len(log) < MaxLog
         /\ log' = Append(log, [kind |-> "begin", turn |-> cur + 1])
         /\ cur' = cur + 1 /\ UNCHANGED up

\* A writer holding turn t submits. It is accepted only if t is still the current turn, unless the
\* fence is ignored.
Write(t) == /\ up /\ Len(log) < MaxLog
            /\ IF FenceIgnored \/ t = cur
                  THEN log' = Append(log, [kind |-> "write", turn |-> t])
                  ELSE UNCHANGED log
            /\ UNCHANGED <<cur, up>>

\* The process dies: the in-memory turn is gone.
Crash == up /\ up' = FALSE /\ cur' = 0 /\ UNCHANGED log

\* Recovery rebuilds the turn from the begin records, unless the rebuild is skipped.
Recover == /\ ~up /\ up' = TRUE
           /\ cur' = IF FenceNotRebuilt THEN 0 ELSE BegunBefore(Len(log) + 1)
           /\ UNCHANGED log

Next == Begin \/ Crash \/ Recover \/ (\E t \in 0..MaxTurn : Write(t))
Spec == Init /\ [][Next]_vars

\* The accepted writes never go back to an older turn.
NeverGoesBack == \A i, j \in 1..Len(log) :
                   (i < j /\ log[i].kind = "write" /\ log[j].kind = "write") => log[i].turn <= log[j].turn
\* A write never carries a turn that has not begun.
NothingFromTheFuture == \A j \in 1..Len(log) : log[j].kind = "write" => log[j].turn <= BegunBefore(j)
\* The fencing rule, read off the log: an accepted write carries the turn that was current when it was
\* accepted, so nothing from a fenced turn is ever in the log.
AcceptOnlyCurrent == \A j \in 1..Len(log) : log[j].kind = "write" => log[j].turn = BegunBefore(j)
====
