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
