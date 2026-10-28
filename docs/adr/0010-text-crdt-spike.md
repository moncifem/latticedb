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
