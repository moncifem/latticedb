# RFC 0011: Column-level CRDT merge for remote row changes

Status: draft
Comment period ends: 2026-11-05
Decision ADR: none yet (no ADR is needed for this slice: it adds no bytes)

## Motivation

WP42 (issue #244, P3-05) asks for the last-writer-wins register and the observed-remove set of
`lattice-core::crdt` to become row column types, with remote changes from the change feed applied
deterministically so that N replicas converge under reorder, duplication and delay (I5). The first
slice decides the merge semantics and implements them as a pure in-memory function. It does not
decide how CRDT state is stored or shipped.

## Proposal

New module `lattice-engines::merge`. No byte format, wire message, catalog record, WAL record or
checkpoint changes. Nothing in the row store, catalog or sync code calls it yet.

**Merge kind.** `MergeKind` is `None`, `Lww` or `OrSet`, one per column, held beside a
`typed::TableLayout` (not inside `Column`, so the existing layout and its codec are untouched).
`kinds_for_layout(layout, overrides)` builds the list: every column defaults to `None`; `OrSet`
needs a list column; `Lww` may sit on any column; an override naming a missing column is an error.

**Row state.** `MergeRow` is one replica's state of one row: a cell per column, the stamp of the
plain columns, and the latest delete HLC. A remote row change from the feed is itself a sparse
`MergeRow` (columns it did not touch are empty). Cell values are opaque bytes, compared bytewise,
so every tie has a total order. Row state is a join semilattice, so `merge_rows(kinds, local,
remote)` is the join: commutative, associative and idempotent.

The requested `metadata` argument is not a separate parameter: the HLC and node id of a change are
the stamps stored inside its `MergeRow`, which keeps the function pure and avoids a second source of
truth for the same ordering.

**Per kind.**

- `Lww`: the entry with the greatest `(Hlc, value bytes)` wins. The HLC order is
  `(wall_ms, counter, node)`, so distinct nodes never tie; equal HLC with different bytes (forged
  input) is decided by the bytes, as in `LwwRegister`.
- `OrSet`: adds carry a unique HLC tag; a remove carries the tags it cancels. Merge is the union of
  adds and of removed tags. An element is present while one of its tags is not removed. A remove
  cancels only the tags it saw, so a concurrent add wins (add-wins), as the `lattice-core` type
  documents.
- `None`: not a CRDT. All plain columns of a row move together: the row with the greater
  `(plain stamp, plain values)` supplies all of them. This is the whole-row last-writer-wins that a
  non-CRDT table has, and it keeps plain columns from mixing values of different writes.

**Deletes.** A row delete carries only an HLC (RFC 0009's delete event has no observed-set), so an
observed-remove at row level is not possible without new bytes. The rule: a delete at `d` hides
every write stamped `<= d` (delete wins ties); a write stamped `> d` is visible again. The state
keeps all cells and only the maximum `d`; visibility is computed when reading. Consequences:
a write that was concurrent with the delete but carries a smaller HLC is hidden (delete wins),
a concurrent write with a greater HLC survives (resurrects). Set removes inside a column stay
add-wins. `docs/ARCHITECTURE.md` names "CRDT merge" for text, sets and registers and gives no row
delete rule, so the row-level rule above is this RFC's proposal and an open question (below).

**Bounds.** At most 256 columns, 65 536 bytes per value or element, 1 024 elements and 4 096 add
tags and 4 096 removed tags per set column. Inputs and the merged result are checked; on a breach
`merge_rows` returns an error and the caller keeps the local row. The laws hold for any change set
whose union fits the bounds; a set whose union does not fit is rejected, and which of its changes
lands first would then matter. The wiring slice must quarantine such a change, not skip it silently.
No function panics or indexes; all maps are ordered maps, so results are deterministic.

**Tests.** Property tests of commutativity, associativity and idempotence; replicas applying
shuffled, duplicated orders of the same changes must reach identical rows (I5) and match an oracle
computed from the change set by definition; a single-replica cross-check against the `lattice-core`
register and set; and deliberately wrong merge variants (arrival-order register, dropped remote
removes, toggling removes, averaged delete stamp, lost delete) that the same checks must catch.

### Open questions

1. Is hiding a lower-HLC concurrent write behind a delete the intended row-delete rule, or should
   a delete be an observed-remove (needs the feed to carry observed stamps: a format change)?
2. Where CRDT state lives: delta tags in the existing cell bytes cannot be told from value bytes, so
   persisting register stamps and set tags needs a new cell encoding and a recovery rule. Not
   decided here; stop point for the stored form is a separate RFC and ADR.
3. Tombstone and tag reclamation (compaction) needs causal stability, as ADR 0010 notes for text.

## Alternatives

Per-column LWW for plain columns would let one row hold values from different writers; rejected
because today's tables resolve a row as a unit. Add-wins for deletes needs observed stamps in the
feed. Reusing `lattice_core::crdt::OrSet` directly was rejected because its state is private, so the
bounds above cannot be checked without changing `lattice-core`; the module is cross-checked against
it in tests instead.

## Compatibility and migration

None: no stored or transmitted byte changes. Existing tables and their rows behave as before until a
later change wires the merge in and chooses the stored form (open question 2).
