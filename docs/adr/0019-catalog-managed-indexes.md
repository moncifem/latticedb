# ADR 0019: Managed index definitions in atomic catalog and row changes

Status: proposed

## Context

ADR 0014 derives entries from rows but leaves callback definitions with the caller. A reopened default
store cannot reconstruct an arbitrary function pointer. Definition history must remain aligned with
row history and must be visible atomically with a transaction's data changes.

## Decision

Proposed by RFC 0005, not accepted: persist stable physical-byte extraction expressions and versioned
index lifecycle metadata in an independent LCIX v1 compound ChangeSet. Keep LCAT v1/LROW v1 unchanged
and do not reuse the separately reserved typed-catalog version. Default recovery folds the compound
metadata and rows together and rebuilds index entries; explicit legacy callbacks never override it.

Synchronous creation builds current entries and ordinary writes maintain them at the commit LSN.
Drop tombstones preserve historical lookup semantics. This is not an online build or a checkpoint.
The scrubber derives expected managed definitions from the catalog rather than the index registry.

## Consequences

This remains proposed until RFC review, independent format review and compatibility policy are
complete. Older RowStore readers can silently skip LCIX row mutations. The owner selected enforced
old-reader refusal on 2026-10-29 and rejected operational-only upgrade. LCIX activation requires a separately reviewed, implemented and accepted WAL
version/capability guard. No mixed-version or in-place downgrade support is claimed. A pre-upgrade backup is required for rollback.

Synchronous builds can block and require temporary entry memory. Used ids and catalog/index history
grow; the nested row decoder also retains allocation amplification beyond its encoded byte length.
No new row-count limit, peak-allocation bound or phase exit is established. A row admission-budget
policy and its measurement remain unresolved. The seven-full-day RFC hold was not waived;
Owner selection of the guard approach does not accept this format or the future WAL encoding.
Online backfill and schema migration remain RFC 0004 work. Future checkpoint composition must preserve the complete definition timeline under
an independently reviewed engine tag.


The PR 209 implementation is stacked on PR 222's draft WAL guard. Managed row/catalog readers
explicitly declare support and check actual record context; callback readers remain at zero support.
Compound writers bind the managed requirement to their append after actor-policy checks. Independent
literal guarded/unguarded fixtures, a planted guard-omission control and actual engine first-upgrade
cuts cover the integration. Hosted frozen-old-reader execution is pending exact-head completion.
Neither implementation nor measured evidence accepts either format or waives the review hold;
unguarded directories created by an earlier draft are refused rather than adopted.
