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

**Deletes (interim rule, accepted by the tech lead on 2026-10-29 under the owner's delegation).**
A delete at HLC `d` hides every write stamped at or before `d`. A write stamped after `d` shows
again, and a delete wins ties. A concurrent write with a lower HLC is hidden; one with a higher HLC
revives the row. Hidden cells are kept (the state keeps all cells and only the maximum `d`;
visibility is computed on read), so nothing is lost physically. A row delete carries only an HLC
(RFC 0009's delete event has no observed set), so an observed row remove is not possible without new
bytes. Set removes inside a column stay add-wins. The wiring slice must:

1. stamp an insert on all columns at one HLC, and treat a non-insert write to a deleted row as a
   `Conflict` entity, not a revival;
2. reject remote HLCs beyond a drift bound before merging (a forged `delete(u64::MAX)` would hide
   every later write forever; this module does not know the clock).

Upgrading to observed-remove (add-wins for writes concurrent with a delete) needs the feed to carry
observed parents or a version vector: a separate RFC. The current rule stays a valid join under it.

**Wiring requirements for kinds.**

- The default kind `None` is whole-row last-writer-wins and a write must carry all plain columns,
  so concurrent edits of different plain columns lose one of them. The wiring slice must carry a
  changed-column set, or default synced columns to `Lww`.
- Workflow-state columns must not be `Lww`: they are server-arbitrated and produce `Conflict`
  entities (P3-09).
- The `MergeKind` list is versioned with the schema. Rows merged under different lists fail with a
  `Shape` error rather than merging.
- An add tag is the HLC of the add change, unique per node by I4. A tag reused for two adds is a
  caller bug (removing one element then cancels the other); a test records this.

**Bounds.** At most 256 columns, 65 536 bytes per value or element, 1 024 elements and 4 096 add
tags and 4 096 removed tags per set column. Inputs and the merged result are checked; on a breach
`merge_rows` returns an error and nothing is merged. I5 holds only while the union of all changes
fits the bounds. Beyond them the replica is in a degraded, alarmed state: it keeps the local row,
reports the refused change, and may diverge from replicas that applied the changes in another order.
The earlier idea of quarantining such a change is dropped, because skipping it cannot restore
convergence. Compaction of tombstones and tags (open question 3) is therefore a prerequisite for
wiring, not a follow-up. No function panics or indexes; all maps are ordered maps, so results are
deterministic. Set mutators clone the cell to stay atomic, which costs O(set size), bounded by the
tag limit.

**Tests.** Property tests of commutativity, associativity and idempotence; replicas applying
shuffled, duplicated orders of the same changes must reach identical rows (I5) and match an oracle
computed from the change set by definition; a single-replica cross-check against the `lattice-core`
register and set; and nine deliberately wrong variants (arrival-order register, dropped remote removes, toggling
removes, averaged delete stamp, lost delete, plain columns mixed across writers, register without
the byte tie-break, plain stamp as minimum, delete that shows ties) that the convergence check must
catch; the first five also fail the law check.

### Open questions

1. Row delete rule: decided as the interim rule above (tech lead, 2026-10-29). Observed-remove at
   row level stays a possible later upgrade.
2. Where CRDT state lives: delta tags in the existing cell bytes cannot be told from value bytes, so
   persisting register stamps and set tags needs a new cell encoding and a recovery rule. Not
   decided here; stop point for the stored form is a separate RFC and ADR.
3. Tombstone and tag reclamation (compaction) needs causal stability, as ADR 0010 notes for text.
   It is a prerequisite for wiring, because I5 holds only within the bounds.

## Alternatives

Per-column LWW for plain columns would let one row hold values from different writers; rejected
because today's tables resolve a row as a unit. Add-wins for deletes needs observed stamps in the
feed. Reusing `lattice_core::crdt::OrSet` directly was rejected because its state is private, so the
bounds above cannot be checked without changing `lattice-core`; the module is cross-checked against
it in tests instead.

## Compatibility and migration

None: no stored or transmitted byte changes. Existing tables and their rows behave as before until a
later change wires the merge in and chooses the stored form (open question 2).
