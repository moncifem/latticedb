# Phase 2 status (gate NOT passed)

Phase 2 is in progress under the owner-confirmed waiver in
[the Phase 1 review package](phase-1-review.md). The Phase 1 gate remains open; this page claims no
Phase 2 exit criterion. Requirements and exit criteria are in
[the Phase 2 specification](../spec/02-storage-and-sql.md).

## Implemented slices

- MVCC row versions, immutable snapshots, first-committer-wins write conflicts and documented write skew.
- A persistent row store whose row changes travel through the kernel WAL and replay on recovery.
- A versioned catalog, a row-write codec and their seeded power-cut tests.
- Secondary index primitives maintained internally with row writes and rebuilt from row history.
  Exact-key lookup and consistency checks work at current and historical snapshots. Definitions are
  caller-supplied on each open, not catalog-managed yet (ADR 0014).
- A declared SQL subset. This is a target contract, not an implemented SQL interface.

## Verification for the index slice (P2-07)

The row simulation compares rows and index lookups against a map oracle at remembered snapshots over
120 seeds. It detects all four row planted bugs, including stale index entries. The existing 60-seed
power-cut workload opens the store with indexes, verifies acknowledged rows after terminal recovery,
and checks that every index agrees with the recovered rows. Focused tests cover zero-byte and empty
keys, update/delete history after reopen, wrong stored primary-key values and duplicate index ids.
These tests do not establish the Phase 2 soak-volume or all-fault-profile criteria.

## Still missing

Catalog-backed index definitions; completed typed-table integration; checkpoint-based recovery with
memory/time measurements; vacuum; SQL parser, planner and executor; PostgreSQL wire and driver
conformance; branches and SQL time travel; online schema changes; and complete Phase 2 simulation,
benchmark and planted-bug evidence. The Phase 1 nightly/fuzz history and final gate review also remain
open. Track partial acceptance on [the roadmap](https://github.com/users/moncifem/projects/1).
