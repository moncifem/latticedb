# ADR 0014: Secondary indexes are derived from committed rows

Status: accepted

## Context

P2-07 needs indexes to agree with rows at every snapshot and after crash recovery. Logging index
writes separately would let callers forget them or give rows and index entries different outcomes.
The current row store already preserves row history in one ordered change set log.

## Decision

An index definition identifies a table and a deterministic function from row values to optional index
keys. The row store computes old-entry deletions and new-entry insertions internally for every commit
and publishes their versions at the row commit LSN. The same computation runs while replaying row
change sets; no additional index payload is logged.

Exact-key lookups use an escaped, terminated binary key prefix followed by the primary key. Empty
keys, zero bytes, shared prefixes and multiple rows with the same extracted value remain distinct.
The consistency checker recomputes expected entries from rows at a snapshot and compares both entry
keys and stored primary-key values. The row simulator compares lookups with an independent map oracle.

## Consequences

Every write path through the row store maintains configured indexes. Recovery reconstructs historical
index versions, so old snapshots remain queryable. Rebuilding all index versions has a cost; recovery
benchmarks, checkpoints and vacuum remain Phase 2 work.

Definitions are runtime inputs for now and must be supplied again on reopen. Distinct ids are checked
before the log is opened; the caller must keep extractors deterministic. Catalog-managed definitions,
online index changes, SQL planning and cross-engine soak evidence are not provided by this slice.
