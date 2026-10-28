# Isolation (draft, Phase 2)

Status: draft. It describes what `lattice_engines::mvcc` does today; it becomes a frozen format
document at the Phase 2 gate.

## Model

Every row version is stamped with the LSN and id of the change set that wrote it. A **snapshot** is
the last applied LSN. A read at a snapshot returns, for each row, the newest version at or before that
LSN, or nothing if that version is a deletion.

A change set is applied in one step at one LSN, so a snapshot sees all of its writes or none of them.
Snapshots are plain values: reading never blocks a writer, and a writer never changes what an existing
snapshot sees.

## Transactions

A transaction reads from the snapshot taken when it began, plus its own buffered writes. On commit it
is applied as one change set. If another change set has written any row the transaction wrote, after
the transaction's snapshot, the commit fails with a conflict and changes nothing (first committer wins).
Writes that touch different rows never conflict.

## What this prevents

| Anomaly | Result |
|---|---|
| Dirty read | Cannot happen: uncommitted writes are not applied |
| Non-repeatable read | Cannot happen: a snapshot never changes |
| Phantom read | Cannot happen: range scans use the same snapshot |
| Lost update | Cannot happen: first committer wins on the same row |
| Partial change set visible | Cannot happen (invariant I3) |

## What this allows

**Write skew.** Two transactions each read both rows of an invariant and each write a different row;
both commit and the invariant can break. This is the documented anomaly of snapshot isolation. A test
(`write_skew_is_allowed_as_documented`) pins it so a change in behaviour is noticed.

## Not decided yet

Serializable isolation, read-only snapshot pinning limits, and the interaction with vacuum (which row
versions may be dropped, work package WP33) are not specified here.
