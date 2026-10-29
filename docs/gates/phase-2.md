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
These tests do not establish the Phase 2 soak-volume criteria. Additional bounded fault-profile
evidence is recorded below.

## Bounded row fault evidence (P2-05, P2-06, P2-07)

`cargo test -p lattice-engines --test row_faults -- --nocapture` runs 48 seeded histories of
72 workload steps under each existing disk profile. Its independent oracle matches recovered WAL
transactions to submitted write envelopes and acknowledged LSNs, permits either outcome for in-doubt
commits, refuses durable rejected writes, and compares current/historical rows and index lookups with
that submitted history. Raw commit metadata, unique ids and increasing timestamps are checked too.

| Profile | Runs | Terminal-verified histories | Rot-censored histories | Cut-short workloads |
|---|---:|---:|---:|---:|
| clean | 48 | 48 | 0 | 0 |
| crash-heavy | 48 | 48 | 0 | 0 |
| disk-faulty | 48 | 48 | 0 | 0 |
| hostile | 48 | 48 | 0 | 0 |
| everything | 48 | 42 | 6 | 0 |
| Total | 240 | 234 | 6 | 0 |

A history stops at its first destructive bit-rot event and is counted as incomplete, never as a pass.
Across the whole workload, including censored histories, the run observed 8,131 acknowledgements and
2,742 power cuts inside operation sequences. These counts describe exercised work; they must not be
read as 8,131 independently verified acknowledgements. Disk-faulty observed 40 injected append errors,
267 fsync errors and 924 ENOSPC outcomes; everything observed 37, 156 and 1,115 respectively. A partial
append counts only when a nonempty strict prefix actually becomes visible, rather than trusting the
profile name.

Separate tests replay identical seeds exactly, detect all four existing storage planted bugs, and sweep
every observed mutating commit/rotation and recovery boundary across 20 workloads. The sweep always
replays again after an interrupted recovery. The focused suite passed five tests, with one ignored
10,000-seed-per-profile volume test; that ignored budget has not been run for this evidence.

This is bounded correctness and fault-coverage evidence. It does not satisfy the million-seed,
seven-night soak requirements, the ten-storage-bug target, or any Phase 2 exit criterion. Typed/catalog
histories and broader soak evidence remain separate work.

## Still missing

Catalog-backed index definitions; completed typed-table integration; checkpoint-based recovery with
memory/time measurements; vacuum; SQL parser, planner and executor; PostgreSQL wire and driver
conformance; branches and SQL time travel; online schema changes; and complete Phase 2 simulation,
benchmark and planted-bug evidence. The Phase 1 nightly/fuzz history and final gate review also remain
open. Track partial acceptance on [the roadmap](https://github.com/users/moncifem/projects/1).
