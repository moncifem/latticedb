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

Index definitions are supplied by the caller on each open; they are not persisted in the catalog yet.
There is no SQL server, index planner integration, vacuum or checkpoint-based recovery start yet.
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
it. The existing `snapshot_as_of_hlc` intentionally keeps its floor-clamping behavior. Reopening
restores history from the WAL; reapplying vacuum restores the retention boundary. This is a storage
API: SQL timestamp parsing, microsecond conversion and SQLSTATE mapping remain executor work.

## Proposed persisted typed schemas

`TypedStore` compiles canonical LSL into persisted immutable writer layouts over the existing
RowStore/Kernel writer. It keeps schema/version identity, historical layout decoding, raw-table id
reservations, typed validation at raw/compound commit and replay, policy-bound transactions and
strict HLC/retention checks. Managed indexes keep their physical-byte key semantics; this does not
add SQL column-index extraction or the complete SQL type system.

RFC 0001 / ADR 0015 remain draft/proposed. Proposed typed capability 4 protects LCAT v2 and typed
writes; proposed LCIX v2 nests typed schema operations under mask 5. Existing LCIX v1 and legacy
bytes remain unchanged. Callback readers advertise zero and catalog-only readers do not advertise
typed semantics. A guarded-history upgrade has no automatic downgrade; use disposable review data.
