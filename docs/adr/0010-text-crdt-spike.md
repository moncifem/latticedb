# ADR 0010: Text CRDT: own spike behind a trait, decision revisited in Phase 3

Status: accepted

## Context

Wiki pages and descriptions need concurrent text editing. Phase 1 asks whether to build a sequence
CRDT or adopt one, with the criteria measured, before features depend on the choice.

## Decision

Phase 1 ships a small replicated growable array (`lattice_core::crdt::Rga`) behind the `Crdt` trait, as a
spike for correctness work. It is state based and recomputes the document on demand, so it is not meant
for large documents. The merge laws (commutative, associative, idempotent), convergence in any delivery
order, deletes winning over concurrent edits of the same character, and convergence over a faulty
simulated network are tested.

Criteria and where they stand:

| Criterion | Own spike | Adopting a library |
|---|---|---|
| Merge laws and convergence under faults | Tested here | Must pass the same tests before adoption |
| Memory per character | One map entry with a 16 byte id, a 16 byte parent id, a char and a flag, plus map overhead (tens of bytes per character, tombstones kept) | To be measured |
| Merge cost | Linear in stored characters; the document is rebuilt on read | To be measured |
| History growth | Tombstones are never collected | Libraries add garbage collection |
| Licence and maintenance | Ours | To be checked per candidate |

## Consequences

The trait boundary lets the implementation change without touching callers. Before Phase 3 depends on
text editing the same law, convergence and benchmark tests are run against candidate libraries and the
numbers are added here; if none passes, the spike is hardened (operation based, tombstone garbage
collection, interleaving tests). Nothing in the spec requires a decision earlier than that.

## Phase 3 data (WP44, E3-6)

`lattice_core::crdt_text` implements the hardened option named above: an operation based RGA with stable
element ids (insert op id plus chunk offset), tombstones, newest-sibling-first ordering, parked operations
for out-of-order delivery (indexed by the id they wait for, with per-origin and global caps), a canonical
bounded encoding, size caps, and tombstone compaction that keeps declared revert points. It sits beside the
Phase 1 spike (`crdt::Rga`); the trait boundary has not been changed.

Compaction drops only leaf tombstones: a tombstone that still anchors a remaining element stays, because
removing it changes where later concurrent inserts land. It also needs a causal stability claim from the
caller (a stable id plus an ack per replica); the document refuses to compact while operations are parked
or when a known node has no ack, and cannot check the claim itself. The set of applied insert ids (with a
content hash, to detect id reuse) is never compacted, so memory for it grows with every insert for the life
of the document.

Measured with `cargo test -p lattice-core --lib crdt_text::tests::measure -- --nocapture` on one
workload: random single-character inserts and deletes (about 40% deletes) until 10 000 characters are
visible, then the whole operation log replayed into a fresh replica, in the test profile
(`optimized + debuginfo`), one laptop, one run. Numbers are indicative, not a benchmark.

| Measure | Result |
|---|---|
| Operations in the log | 50 780 (30 390 inserts, 20 390 deletes) |
| Memory per stored element | 48 bytes (`size_of`), 51.8 bytes per element of array capacity; the applied-id map adds a key and a small record per insert (not measured) |
| Replay, in order, into an empty replica | about 150 000 to 165 000 operations per second; an insert is a linear scan plus a vector splice, so cost grows with document size |
| Tombstones after the workload | 20 390 of 30 390 stored elements |
| After compaction with no retained points | 6 507 tombstones left (13 883 removed); the rest anchor later text |
| After compaction with 3 retained points (at 25%, 50% and 75% of the log) | 11 133 tombstones left (9 257 removed) |

What the tests show: with generated three-replica histories shuffled and duplicated, every replica
produces the string an independent after-tree oracle computes (I5); compaction keeps the visible text and
the text at every retained point (checked against the oracle restricted to operations at or before the
point); later operations applied after compaction match an uncompacted replica, and that test fails if the
leaf rule is removed; the decoder never panics on arbitrary bytes and accepts only canonical encodings.
These ran at default proptest case counts locally.

Does the Phase 1 decision stand? Provisionally: the hardened own implementation passes these tests, so
there is no evidence yet to reverse the decision, but this is not a confirmation. No candidate library was
measured here, so the "adopting a library" column above is still empty. Open: merge cost for large
concurrent histories, documents far beyond 10 000 characters, an index to avoid the linear scans, a
protocol that produces the causal stability claim, a way to reclaim tombstones that anchor live text, a
bound on the applied-id set, and integration with the row engine (P3-07). E3-6 is not claimed as met.
