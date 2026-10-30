# lattice-engines

Layer L4.

**Responsibility.** Storage engines: log, row, blob, graph, vector, text, temporal.

**Must not.** Bypass lattice-core transactions.

Allowed workspace dependencies: lattice-abi, lattice-seams, lattice-core, lattice-lsl.


## Row store today

`RowStore` persists changes through the kernel and replays them into versioned rows. Transactions
use snapshot isolation with first-committer-wins conflicts; write skew is allowed as specified in
[`formats/isolation.md`](../../formats/isolation.md).

`open_with_indexes` accepts deterministic index extractors with distinct ids. Index entries are
updated internally at the same LSN as row writes, and recovery rebuilds their history from the row
change sets. `lookup_at` returns exact-key matches at a snapshot; `check_indexes_at` recomputes entries
from the rows and compares both keys and values. A checker can inspect old snapshots too.

With an empty index list `open_with_indexes` selects managed mode: index definitions are persisted in
the catalog through the log ([RFC 0005](../../docs/rfc/0005-catalog-managed-indexes.md), accepted) and
rebuilt on open. A nonempty list selects legacy callback mode, where the caller supplies the
definitions on every open. `RowStore::vacuum` drops versions below a horizon in memory. The SQL
executor is the separate `lattice-exec` crate. There is no index planner integration and no
checkpoint-based recovery start yet.
See [the Phase 2 status](../../docs/gates/phase-2.md).

## Testing the checks

`cargo test -p lattice-engines --test storage_bugs` checks ten distinct planted storage defects
against specific public-operation witnesses and clean controls. The seeded `row_sim` and `row_faults`
workloads retain their original four-defect contract. These are complementary bounded checks; see the
Phase 2 gate record for the remaining soak, north-star and branch evidence.

`cargo test -p lattice-bench --test row_process_kill -- --nocapture` complements simulation with
eight real child-process terminations and two clean controls, verifying acknowledged log/row batches
and index/history recovery. OS process termination preserves the page cache; this does not test power
loss or establish soak-volume evidence.

## Strict historical HLC resolution

`RowStore::try_snapshot_as_of_hlc` returns the newest retained row change-set snapshot at or
before the full `(wall_ms, counter, node)` timestamp, or `RowError::SnapshotTooOld` when that
predecessor has expired. An empty snapshot before the first commit is valid until vacuum expires
it. `snapshot_as_of_hlc` is the same strict lookup; `snapshot_as_of_hlc_clamped` is the explicit
opt-in that moves an expired timestamp to the vacuum floor. Reopening
restores history from the WAL; reapplying vacuum restores the retention boundary. This is a storage
API: SQL timestamp parsing, microsecond conversion and SQLSTATE mapping remain executor work.

## Column merge (not wired)

`merge` (WP42, [RFC 0011](../../docs/rfc/0011-column-crdt-merge.md), draft) is a pure in-memory
function that merges a remote row change into a local row, column by column: a last-writer-wins
register or an observed-remove set per column, whole-row last-writer-wins for plain columns, and a
row delete that hides writes stamped at or before it. It has no byte encoding and is not called by the
row store, the catalog or sync yet; the stored form of CRDT state is an open question in the RFC.
Run its tests with `cargo test -p lattice-engines --lib merge`.
