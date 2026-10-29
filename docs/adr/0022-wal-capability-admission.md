# ADR 0022: Required engine capabilities at WAL admission

Status: proposed

## Context

Old RowStore readers skip unknown engine payloads. The proposed LCIX compound encoding can therefore
hide committed rows from them. On 2026-10-29 the owner required enforced old-reader refusal and
rejected operational-only upgrade. Existing WAL v1 reserved bits and unknown records cannot enforce
that requirement in already shipped readers.

## Decision

Proposed in RFC 0008, not accepted: introduce a distinct guarded segment header with a version,
required-engine capability mask and header CRC, preserving legacy v1 bytes. Candidate header bytes
and bit assignments are draft only. Require explicit supported capabilities from the actual engine,
preflight the whole header chain before repair/publication, preserve cumulative required bits through
rollover, and durably install the guard before any append needing it. Promote only an empty legacy
tail in place; acknowledged legacy history remains unchanged.

Managed LCIX activation depends on implementation and independent acceptance of this admission path.
A frozen old-reader harness and independently framed fixtures must prove intact upgraded directories
are rejected without mutation. An exhaustive bounded upgrade/crash matrix must show no acknowledged
or adopted LCIX record can exist without its guard.

## Consequences

This proposed design adds header preflight and explicit engine capability plumbing without changing
record framing. Unsupported, damaged or downgraded headers refuse. A durable empty guard can prevent
downgrade even before any LCIX commit; rollback requires a verified pre-upgrade backup. No converter,
release assignment, checkpoint format decision or gate sign-off is supplied. Both this RFC's review
period and PR 209's seven-full-day hold remain unwaived. Owner selection of enforced refusal does
not accept this encoding or authorize LCIX activation.

The draft implementation requires bounded `Io::read_prefix` without a whole-file fallback and
migrates shipped adapters/wrappers. This is an explicit source API break for unpublished seam
implementations, not a WAL v1 byte change. Experimental async adapters use their bounded synchronous
metadata path. Codec/context tests and independent global vectors are present; frozen-reader and
upgrade-cut execution passed at `a132a36bbe9a1d0472f1bb0bc989c8e8bc7c7ff7` in
[run 40906145857](https://github.com/moncifem/latticedb/actions/runs/40906145857). A separate hosted
measurement compares the actual frozen and proposed legacy Log paths and the proposed guarded path,
with source hashes, warm-cache timings, process peak RSS and bounded prefix/read counters. Its raw
results must be reviewed before acceptance; buffer maxima and process RSS are not complete recovery
allocation bounds. LCIX engine activation is still disabled pending acceptance and the unwaived
review holds.


Completed [hosted run 40908208450](https://github.com/moncifem/latticedb/actions/runs/40908208450)
measured actual merge source `fb11b8f657643e250adb2da0d78f4f14ded555cd`, not a later rebase.
The [evidence record](../evidence/wal-admission-40908208450.md) preserves finite compatibility scope,
90 warm-cache samples, 32-byte prefix controls and whole-process RSS limits. At 256 tiny legacy
segments median admission/open cost rose 1.937 ms / 69.1%; the larger case rose 2.190 ms / 4.75%.
Filesystem calls and synchronization are included. Independent review and this measured cost inform
owner acceptance; neither supplies acceptance or a review-period waiver. Status remains proposed.
