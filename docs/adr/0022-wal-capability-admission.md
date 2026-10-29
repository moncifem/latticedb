# ADR 0022: Required engine capabilities at WAL admission

Status: accepted

## Context

Old RowStore readers skip unknown engine payloads. The proposed LCIX compound encoding can therefore
hide committed rows from them. On 2026-10-29 the owner required enforced old-reader refusal and
rejected operational-only upgrade. Existing WAL v1 reserved bits and unknown records cannot enforce
that requirement in already shipped readers.

## Decision

Accept RFC 0008's guarded segment header, version and required-engine capability admission, preserving
legacy v1 bytes. Preflight the whole header chain before repair/publication, require explicit support
from the actual engine, retain cumulative requirements through rollover and durably install the guard
before a protected append. Promote only an empty legacy tail in place; acknowledged legacy history
remains unchanged. The 32-byte `LATCAP02` version-2 header and bit-0 assignment are accepted.

The owner explicitly accepted RFC 0008 / ADR 0022 and waived this specific seven-full-day wait on
2026-10-29 after independent review, bounded compatibility proof and measured-cost disclosure.
[The narrow decision record](../evidence/wal-admission-owner-decision-2026-10-29.md) records the
response and merge condition: exact final-head green CI. This does not accept or waive PR 209's
separate managed LCIX format/hold; engine activation remains separate.

## Consequences

This adds header preflight and explicit engine capability plumbing without changing record framing.
Unsupported, damaged or downgraded headers refuse. A durable empty guard can prevent downgrade even
before an LCIX commit; rollback requires a verified pre-upgrade backup. No converter, release,
checkpoint format decision or phase gate sign-off is supplied.

`Io::read_prefix` is required without a whole-file fallback, and shipped adapters/wrappers are
migrated. This is an explicit source API break for unpublished seam implementations, not a WAL v1
byte change. Experimental async adapters use their bounded synchronous metadata path. Recovery passes
actual segment context; generic opens advertise zero support. No LCIX engine is activated by this PR.

[Hosted run 40908208450](https://github.com/moncifem/latticedb/actions/runs/40908208450) measured actual
merge source `fb11b8f657643e250adb2da0d78f4f14ded555cd`. Its frozen reader checked 4,648 bounded
histories / 9,296 raw-recovered snapshots. The [evidence record](../evidence/wal-admission-40908208450.md)
preserves finite compatibility scope, 90 timed samples/18 warmups, 32-byte prefix controls and process
RSS limits. At 256 tiny legacy segments median admission/open rose 1.937 ms / 69.1%; the larger case
rose 2.190 ms / 4.75%. Filesystem calls and synchronization are included. Buffer maxima and process
RSS do not establish a whole-recovery allocation bound. Acceptance records these disclosed limits;
it does not convert historical measurements into results for later rebases.
