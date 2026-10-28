---- MODULE Commit ----
\* Commit protocol with crashes: a change set is acknowledged only after it is durable, and a crash
\* keeps exactly the durable log. Safety: every acknowledged change set survives any number of crashes.
\*
\* LIMITS (see docs/gates/phase-1.md): this is an abstraction over SETS of ids. It does not model record
\* order, duplicate occurrences, partially written records, segment creation and directory durability,
\* ambiguous failures, batch ownership of acknowledgements, or recovery re-syncing. It has been
\* checked with TLC over three ids. It is evidence for the one-line protocol only, not for the code paths the
\* simulator exercises.
EXTENDS Naturals, FiniteSets, Sequences
CONSTANTS Ids
VARIABLES buffered, durable, acked, memory
vars == <<buffered, durable, acked, memory>>

Init == buffered = {} /\ durable = {} /\ acked = {} /\ memory = {}

Put(i) == i \in Ids \ (buffered \cup durable)
             /\ buffered' = buffered \cup {i} /\ UNCHANGED <<durable, acked, memory>>

Sync == buffered /= {}
        /\ durable' = durable \cup buffered /\ buffered' = {}
        /\ UNCHANGED <<acked, memory>>

Ack(i) == i \in durable \ acked /\ acked' = acked \cup {i} /\ UNCHANGED <<buffered, durable, memory>>

Replay(i) == i \in durable /\ memory' = memory \cup {i} /\ UNCHANGED <<buffered, durable, acked>>

\* A crash loses everything volatile: the buffer and memory. The durable log survives.
Crash == buffered' = {} /\ memory' = {} /\ UNCHANGED <<durable, acked>>

Next == Sync \/ Crash \/ (\E i \in Ids : Put(i) \/ Ack(i) \/ Replay(i))
Spec == Init /\ [][Next]_vars

AckedAreDurable == acked \subseteq durable
MemoryIsDurable == memory \subseteq durable
====
