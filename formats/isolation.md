# Format: transaction isolation v1

Status: draft (Phase 2, requirement P2-04). It becomes a frozen format document at the Phase 2 gate.

This is the behavioural contract for reads and writes against the row engine. It is a specification of
observable behaviour, not a byte layout; the "layout" is the visibility rule and the conflict rule.
Implementations and checkers are written against this text.

## Model

- All committed change sets form one total order: their log sequence numbers (LSNs). The HLC is not used
  for ordering; it only maps wall-clock times to LSNs for `AS OF TIMESTAMP`.
- A **row version** is the change set that wrote it. A delete is a tombstone version.
- A **snapshot** is a position `(branch, S)`: the transaction sees exactly the change sets on that branch
  with commit LSN `<= S`, and nothing else. Snapshots are immutable and cost no locks.
- A transaction buffers its writes and publishes them as **one change set** at commit. That change set
  is visible in full or not at all (invariant I3), in the log and in every engine (I2).

## Visibility

1. A read inside a transaction sees its own earlier writes, then the snapshot.
2. A transaction started in autocommit mode (a single statement) takes its snapshot at the statement's
   start, at the durable LSN of its branch. A multi-statement transaction takes one snapshot at `BEGIN`
   and keeps it.
3. A reader never blocks a writer and a writer never blocks a reader. There are no read locks.
4. `AS OF CHANGESET c` uses `S = commit LSN of c`. `AS OF TIMESTAMP t` uses the largest `S` whose
   change set has `hlc <= t`. Both are read-only snapshots.
5. After a crash, snapshot positions are gone; a new transaction starts at the recovered durable LSN.

## Conflict rule (first committer wins)

When a transaction commits, the engine checks, at the log's linearization point and atomically with the
append, every row the transaction wrote:

- if the newest committed version of that row (on the same branch) has commit LSN `> S`, the commit is
  **rejected**: nothing is written, the error class is retryable, the SQLSTATE is `40001`
  (`serialization_failure`), and the message names the row key and the winning change set id;
- an insert of a primary key that has a visible or later-committed live version is a conflict of the same
  kind (`23505` if the key is visible in the snapshot, `40001` if it appeared after it);
- the check order at the log is: duplicate change set id (returns the original receipt), turn fence,
  then row conflicts. A change set rejected for a fence or a conflict leaves no trace in the log.

Rows are the unit of conflict. Two transactions that write different rows of the same table, or
different columns of the same row, are in conflict only if they write the same row (column-level merge
is a sync concern, not an isolation one).

## Anomalies

| Anomaly | Allowed? | Note |
|---|---|---|
| Dirty read | No | Only committed change sets are visible |
| Non-repeatable read | No | One snapshot per transaction |
| Phantom (in a read) | No | The snapshot is of the whole database |
| Lost update | No | First committer wins on the same row |
| Write skew | **Yes** | Two transactions read overlapping rows and write disjoint ones |
| Read-only transaction anomaly | **Yes** | A read-only transaction can see a state no serial order produces |
| Stale read | Yes | A snapshot is not refreshed during its transaction |

This is snapshot isolation, not serializability. Callers that need to prevent write skew write a row
both transactions touch (a guard row), which turns the skew into a write-write conflict.

## Limits and errors

- A snapshot older than the retention window (vacuum may have removed versions below the oldest
  snapshot) fails with `snapshot too old` rather than returning wrong data. History in the log is kept;
  only row versions are vacuumed.
- A transaction that was open when the process crashed is gone; its writes were never a change set.
- Retry policy: `40001` is safe to retry as a new transaction; the dedupe id of a retried change set is
  new, so the retry cannot be mistaken for a duplicate.

## Implementation status

`lattice_engines::mvcc` implements today: snapshots as the last applied LSN, newest-version-at-or-before
reads, deletions as tombstones, first-committer-wins per row, and the write-skew behaviour pinned by the
test `write_skew_is_allowed_as_documented`. Not implemented yet: SQLSTATE mapping, branches,
the check order with fences and dedupe at the log, and the history checks below. Vacuum
(`RowStore::vacuum`) keeps the newest version at the horizon and everything newer, and a transaction below
the vacuum floor fails to commit with `snapshot too old`.

## Versioning and compatibility

Changing the visibility or conflict rule changes the behaviour of existing applications and needs an
RFC and an ADR. Adding an error detail or a stricter check that only turns a wrong result into an error
does not.

## Verification obligations

- Model-based test of every operation against a map oracle, with crash and recovery in the sequence.
- Transaction-history checks (Elle-style) on simulated histories: must report no G0, G1a, G1b, G1c or
  lost update, and must be able to find write skew in a history that has it.
- Invariant I3 checker: for every read, the set of change sets seen is a prefix-closed set of whole
  change sets by LSN.
- Planted bugs: conflict check skipped, conflict check against the wrong LSN, snapshot taken after a
  concurrent commit, partial publish of a change set.

## Open questions

- Serializable snapshot isolation (read-set tracking) is not specified; revisit if a product need
  cannot be met with guard rows.
- Cross-branch transactions are not supported; a transaction is on one branch.
