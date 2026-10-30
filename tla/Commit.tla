---- MODULE Commit ----
\* Commit protocol with power loss. A change set is acknowledged only after an fsync that reached the
\* disk, and a crash keeps exactly what is on the disk. Safety: every acknowledged change set is on the
\* disk, so it is still there after any number of crashes, and recovery only hands out change sets it
\* has made durable.
\*
\* The state separates three places so that the properties are about observable storage, not about the
\* guard of the action that sets them:
\*   buffered  the writer's own buffer (lost by a crash)
\*   cache     what the operating system shows to a reader: appended, not necessarily on the disk
\*   disk      what survives a power cut
\* `synced` is the writer's belief that an fsync finished. The three switches are the protocol's
\* protections turned off, one per planted bug of the kernel (`SkipFsync`, `AckBeforeSync`,
\* `SkipRecoverySync` in lattice-core); tla/negative/Commit*.cfg set them and TLC must report the
\* invariant that catches each.
\*
\* LIMITS (see docs/gates/phase-1.md): this is an abstraction over SETS of ids. It does not model record
\* order, duplicate occurrences, partially written records, segment creation and directory durability,
\* ambiguous failures, batch ownership of acknowledgements, or recovery re-syncing the way the code
\* does. The previous version of this model was checked with TLC over three ids; this rewritten
\* model has not been run through TLC yet. Replay is enabled at any time, not only after a Crash, which
\* over-approximates recovery. It is evidence for the one-line protocol only, not
\* for the code paths the simulator exercises, and there is no trace validation between the model and
\* the code.
EXTENDS Naturals, FiniteSets, Sequences
CONSTANTS Ids, SkipFsync, AckBeforeSync, SkipRecoverySync
VARIABLES buffered, cache, disk, synced, acked, memory
vars == <<buffered, cache, disk, synced, acked, memory>>

Init == /\ buffered = {} /\ cache = {} /\ disk = {}
        /\ synced = {} /\ acked = {} /\ memory = {}

Put(i) == /\ i \in Ids \ (buffered \cup cache)
          /\ buffered' = buffered \cup {i}
          /\ UNCHANGED <<cache, disk, synced, acked, memory>>

\* The append: the buffer reaches the operating system, not yet the disk.
Write == /\ buffered /= {}
          /\ cache' = cache \cup buffered /\ buffered' = {}
          /\ UNCHANGED <<disk, synced, acked, memory>>

\* The fsync. With SkipFsync it reports success without writing anything to the disk.
Fsync == /\ cache /= {}
         /\ synced' = synced \cup cache
         /\ disk' = IF SkipFsync THEN disk ELSE disk \cup cache
         /\ UNCHANGED <<buffered, cache, acked, memory>>

\* Acknowledgement needs a finished fsync. With AckBeforeSync it only needs the change set to be queued.
Ack(i) == /\ i \in (IF AckBeforeSync THEN buffered \cup cache ELSE synced) \ acked
          /\ acked' = acked \cup {i}
          /\ UNCHANGED <<buffered, cache, disk, synced, memory>>

\* Recovery adopts a change set it can see and makes it durable before handing it out. With
\* SkipRecoverySync it adopts without syncing (visible bytes are not necessarily on the disk).
Replay(i) == /\ i \in cache
             /\ memory' = memory \cup {i}
             /\ disk' = IF SkipRecoverySync THEN disk ELSE disk \cup {i}
             /\ UNCHANGED <<buffered, cache, synced, acked>>

\* A power cut loses the buffer, the cache, memory and the writer's belief; the disk survives.
Crash == /\ buffered' = {} /\ memory' = {} /\ synced' = {}
         /\ cache' = disk
         /\ UNCHANGED <<disk, acked>>

Next == Write \/ Fsync \/ Crash \/ (\E i \in Ids : Put(i) \/ Ack(i) \/ Replay(i))
Spec == Init /\ [][Next]_vars

\* Everything acknowledged is on the disk ...
AckedAreDurable == acked \subseteq disk
\* ... so it is still visible after a crash, which replaces the cache by the disk.
\* (Implied by AckedAreDurable and disk \subseteq cache, so redundant in the positive config; it is
\* non-vacuous in CommitSkipFsyncThenCrash.)
AckedSurviveCrash == acked \subseteq cache
\* Recovery never hands out what a power cut could still take back.
\* (Restates the effect of Replay; it can only fail through the SkipRecoverySync switch.)
MemoryIsDurable == memory \subseteq disk
====
