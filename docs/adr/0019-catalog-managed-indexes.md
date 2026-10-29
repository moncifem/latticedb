# ADR 0019: Managed index definitions in atomic catalog and row changes

Status: accepted

## Context

ADR 0014 derives entries from rows but leaves callback definitions with the caller. A reopened default
store cannot reconstruct an arbitrary function pointer. Definition history must remain aligned with
row history and must be visible atomically with a transaction's data changes.

## Decision

Accept RFC 0005: persist stable physical-byte extraction expressions and versioned
index lifecycle metadata in an independent LCIX v1 compound ChangeSet. Keep LCAT v1/LROW v1 unchanged
and do not reuse the separately reserved typed-catalog version. Default recovery folds the compound
metadata and rows together and rebuilds index entries; explicit legacy callbacks never override it.

Synchronous creation builds current entries and ordinary writes maintain them at the commit LSN.
Drop tombstones preserve historical lookup semantics. This is not an online build or a checkpoint.
The scrubber derives expected managed definitions from the catalog rather than the index registry.

## Consequences

The owner explicitly accepted RFC 0005 / ADR 0019 and waived this format's seven-full-day wait on
2026-10-29 after independent review and bounded actual-engine/frozen-reader proof disclosure.
[The narrow decision record](../evidence/managed-index-owner-decision-2026-10-29.md) conditions merge
on exact final-head green CI. Older readers can silently skip unguarded LCIX; accepted RFC 0008 /
ADR 0022, merged in PR 222, supplies the required refusal mechanism. No mixed-version or in-place
downgrade support is claimed; a verified pre-upgrade backup is required for restore-based rollback.

Synchronous builds can block and require temporary entry memory. Used ids and catalog/index history
grow; the nested row decoder also retains allocation amplification beyond its encoded byte length.
No new row-count limit, peak-allocation bound or phase exit is established. A row admission-budget
policy and its measurement remain unresolved. These disclosed limits remain after acceptance;
no checkpoint format, release or phase gate is accepted by this decision.
Online backfill and schema migration remain RFC 0004 work. Future checkpoint composition must preserve the complete definition timeline under
an independently reviewed engine tag.


PR 209 is rebased onto main after accepted PR 222's WAL guard merged. Managed row/catalog readers
explicitly declare support and check actual record context; callback readers remain at zero support.
Compound writers bind the managed requirement to their append after actor-policy checks. Independent
literal guarded/unguarded fixtures, a planted guard-omission control and actual engine first-upgrade
cuts cover the integration. Hosted run 40916386232 checked 15 engine histories / 30 frozen-reader
probes and independent review found no new blocking defect; the
[evidence record](../evidence/managed-index-40916386232.md) preserves exact-source and finite-scope
limits. Every new final head requires fresh checks. Unguarded directories from an earlier draft
are refused rather than adopted; accepted WAL fixtures alone do not activate an old engine.
