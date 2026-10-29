# Owner decision: managed-index acceptance and specific review-period waiver

On 2026-10-29 the project owner explicitly accepted RFC 0005 / ADR 0019 and waived this managed-index
format's seven-full-day review wait in response to its scoped acceptance/waiver question:

> Accept managed-index format and merge #209 after green CI

The question disclosed the reviewed actual-engine proof of 15 finite histories / 30 frozen-reader
checks, independent review, and the required pre-upgrade backup/restore-based downgrade constraints.
[The evidence and limits](managed-index-40916386232.md) distinguish this engine proof from the broader
core admission matrix and from Log-only performance/RSS measurements. Row-decoder admission/allocation
budgets and synchronous build performance remain unresolved; no complete memory bound is accepted.

This accepts the reviewed LCIX v1 format and PR 209 implementation: canonical nested table/row blocks,
versioned physical-byte index definitions, atomic row/catalog/index publication, synchronous builds,
retained historical definitions/entries, explicit legacy callback mode and required guarded WAL
admission. Accepted RFC 0008 / ADR 0022 was separately merged in PR 222; its guard's waiver did not
supply this managed-format waiver.

Merge of [the existing PR 209](https://github.com/moncifem/latticedb/pull/209) remains conditional on
exact final-head green CI and hosted proof. This record does not claim that merge has happened.
Acceptance does not create an in-place downgrade converter, accept a checkpoint format, assign a
release, waive unrelated waits, sign any phase gate or establish production readiness.
