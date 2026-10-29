# RFC 0008: WAL admission for required engine capabilities

Status: draft
Comment period ends: at least seven full days after this RFC's PR opens
Decision ADR: proposed ADR 0022

## Motivation and authority

On 2026-10-29 the owner required enforced old-reader refusal before activation of the managed-index
LCIX proposal in RFC 0005 / PR 209. Operational-only coordinated upgrade was rejected. That decision
selects a requirement, not the candidate encoding below. This RFC accepts no storage/checkpoint
format, reserves no assigned release, and waives neither its own review period nor PR 209's hold.
Implementation, independent format review, compatibility evidence and an explicit acceptance decision
remain required. The separate draft implementation now provides these core/log APIs and independent fixtures;
no engine is switched to LCIX activation. Hosted compatibility/crash evidence and independent code
review remain pending. Implementation presence does not accept this encoding.

Existing v1 readers preserve unknown record types and RowStore skips unknown engine payloads. Thus
an unknown record, LCIX marker, directory sidecar, or the ignored v1 header reserved word cannot stop
a shipped reader. A directory containing compound row changes can silently appear incomplete.

## Proposal

### Candidate header and capability assignment

Candidate values below are proposed for review only, not accepted assignments. Leave all existing
`LATLOG01` v1 headers and records byte-unchanged. Introduce a distinct guarded header, before records:

| Offset | Bytes | Candidate field |
|---:|---:|---|
| 0 | 8 | ASCII `LATCAP02` |
| 8 | 4 | Little-endian u32 header version `2` |
| 12 | 2 | Little-endian u16 header length `32` |
| 14 | 2 | Reserved zero u16 |
| 16 | 8 | Little-endian u64 required engine capability mask |
| 24 | 4 | Reserved zero u32 |
| 28 | 4 | CRC32C of the preceding 28 header bytes |

Candidate mask bit 0 (`1`) means managed compound catalog/index/row replay. No other bits are
assigned by this proposal. A guarded header must require at least one defined capability. Reject
unknown versions, unexpected lengths, nonzero reserved fields, header CRC mismatch, zero mask and
unknown required bits. Record framing and ChangeSet bytes remain v1; the first record begins after
32 bytes for a guarded segment and after 16 for legacy v1. Unknown header lengths are rejected rather
than guessed or treated as record data. Future extensions require a separately reviewed version.

The distinct magic causes an intact old reader to fail `BadMagic` even if a version field is damaged;
the header CRC detects accidental damage for new readers. This is corruption detection, not an
adversarial integrity/authentication mechanism. Old readers may repair a newest file shorter than
16 bytes as torn creation; the durable-before-record rule below ensures no acknowledged guarded
record can exist in such a file. Complete guarded headers must be refused without mutation by the
frozen old reader. Compatibility tests must demonstrate this behavior, not infer it from the magic.

## Admission must reach the engine

The kernel/log API needs explicit caller-supported engine capabilities, defaulting to zero. Existing
ordinary open APIs therefore reject guarded directories. Managed RowStore explicitly declares bit 0
only when its complete LCIX decoder, semantic validation and index/history recovery are present.
Callback-only RowStore must not declare bit 0. CatalogStore may declare it only with compound metadata
replay and preservation of row payloads; it still supplies no row execution surface. A maintenance
reader must deliberately choose a capability-aware raw-preservation mode or an engine-aware mode;
adding generic header support alone must not make an old engine silently admissible.

Recovery must pass segment provenance/capabilities alongside each record to the consumer (a separate
recovery context, not a new persistent record field). An engine seeing LCIX in an unguarded segment
must refuse even if a caller supplied the supported bit. Append needs an explicit required-capability
argument bound to that append, checked before record construction/write. Managed compound writers
always request bit 0. Generic kernel code need not inspect opaque LCIX payload bytes, so the engine
must enforce that request and the new guard path needs a planted omission witness.

Before exposing recovered state or performing repair/truncation, preflight every existing segment
header, validate required support, and reject capability rollback across the chain. This can read
only the bounded header prefix per segment; it must not retain whole segment bodies. The union of
required capabilities is monotonic. A legacy v1 segment after the first guarded segment, or a later
guarded segment dropping an earlier required bit, refuses open. Header preflight is not a substitute
for contiguous segment/LSN checks or record CRC validation. Preserve streaming record recovery.

## Writer upgrade protocol

Keep one exclusive writer and the current synchronous admission contract. On the first append that
requires bit 0:

1. Finish the earlier segment's durable prefix; do not rewrite acknowledged v1 records or headers.
2. If the active tail contains records, create the next-LSN segment with the guarded header. If the
   active legacy tail has no records, replace only that empty tail's header under the exclusive
   writer: truncate it, write the complete guarded header, sync the file, then sync the directory.
   This includes an entirely empty initialized directory and avoids two segment names with the same
   first LSN. Failure may leave torn empty-tail creation but cannot erase an acknowledged record.
3. Sync the guarded header file and its directory entry before appending any guarded record. Mark
   the in-memory append context guarded only after both succeed. Any ambiguous failure poisons the
   writer until reopen; never append through an uncertain guard installation.
4. Append and sync the normal record. Acknowledgement follows existing kernel commit/apply rules.
   Every subsequent rollover preserves the cumulative mask, even for ordinary LROW writes.

A crash after a durable guard but before the first LCIX record may conservatively prohibit downgrade.
A crash during a short empty-header rewrite may let an old reader restore an empty v1 tail, but no
LCIX record or acknowledged data was present there. A new append still must establish the durable
guard before writing LCIX. Guard durability is separate from LCIX adoption: unacknowledged complete
records may recover, but only from an intact guarded segment. Missing segments or damaged complete
headers must refuse; they cannot be treated as permission to reset capability history.

Backup verification and restore-based rollback remain required. There is no in-place downgrade or
converter. This proposal does not compose, accept or select any dormant checkpoint engine format.
A future checkpoint reader must preserve admission and cannot erase the required-capability history.

## Independent compatibility fixtures and executable tests

Implementation must add independently framed global corpus fixtures with version/draft metadata:
legacy v1 unchanged, guarded bit-0 header, guarded header plus complete LCIX ChangeSet, unknown version,
unknown capability, dropped capability, wrong header length, reserved fields, bad CRC, and all truncated
header prefixes. Expected bytes/CRCs come from a separate implementation, not the tested encoder.
Keep valid and rejection examples distinct and pin exact error classification and unchanged bytes.

Build the actual old RowStore source pinned to immutable pre-LCIX revision
`743b0fb52a495a3f875490268d604ed543be9e64`, using its original dependency lock and toolchain, in a separate
compatibility job. A tiny harness may call its public open/read API but must not replace its header
parser or recovery code. Record source, harness and resulting executable hashes. Also test the last
released old binary where its CLI actually opens row storage; the current development TCP adapter
opens no database, so launching it is not an old-reader compatibility test.

The frozen harness must open legacy fixtures normally, reject the complete upgraded directory and
leave a before/after manifest of all filenames/bytes unchanged. The new reader must recover exact
literal rows, definitions and history, reject unsupported capability configurations, and refuse a
CRC-valid LCIX record placed in an unguarded legacy segment. A deliberately omitted guard request
must be detected by an independent capability/adoption oracle. Run both callback and managed opens.

## Alternatives

Operational-only coordinated upgrade was rejected by the owner. An unknown record type, LCIX marker,
sidecar or v1 reserved bit is ignored by old paths and cannot enforce refusal. Updating only future
readers leaves shipped readers unsafe. Rewriting acknowledged legacy segment headers would add a
crash-sensitive migration across existing history; this proposal instead guards only an empty tail
or a new segment before new-format data. A new file extension would be skipped by old segment
discovery and could hide data, so it is not an admission gate.

## Crash matrix and acceptance

Sweep every mutating seam operation in completed controls for nonempty-tail rotation and empty-tail
promotion separately. Include truncate, partial header append, header file sync, directory sync,
record append, record fsync, and later guarded rollover. Inject no-space and sync failures as well
as power loss; exercise torn headers at every prefix, missing segments and record corruption. For
each outcome record whether the guard and LCIX survived, all acknowledged rows/metadata, the old
reader's refusal result, byte manifest before/after the refusal attempt, and a second new recovery.
No outcome may adopt LCIX from unguarded storage or lose an acknowledged prefix. Controls must prove
each failure site was reached; a crash sweep with zero observed upgrade operations is not evidence.

Acceptance additionally needs independent review of header dispatch, capability propagation, every
repair path, raw tools and checkpoint interactions; exact-head required checks; unchanged v1 corpus;
measured header-preflight recovery overhead and allocation bounds; and a recorded owner format
acceptance after the review period. Merely merging a design document or selecting the guard approach
cannot activate LCIX. PR 209 stays draft until this guard is implemented and accepted and its own
unwaived hold and review requirements are satisfied.

## Draft implementation scope and seam contract

The draft adds `Io::read_prefix(path, max)` as a required, deliberately breaking seam API for these
unpublished crates. It must bound physical reads and returned-buffer allocation by `max`, include
visible pending/truncated bytes just as `read` does, and report missing paths even at zero length.
There is no whole-file fallback. StdIo and SimIo implement bounded reads; transparent adapters
delegate. The experimental Tokio/io_uring adapters delegate this small metadata read to their
synchronous StdIo metadata path. Existing simulated content faults remain in the shared backing
file model; a prefix read draws no new RNG or mutating trace event, matching full-read behavior.

Core open methods default to zero support. Explicit streaming opens pass `RecoveryContext` alongside
records; explicit append/commit APIs bind required bits to that append. Explicit raw scrub/read
methods retain admission requirements without interpreting LCIX. Caller-controlled supported bits
are a host contract, not proof that arbitrary callers actually understand an engine format. The
RowStore integration is deliberately deferred to PR 209 after independent guard acceptance.

Independent header/full-ChangeSet fixtures, normal codec/context/denial tests and a planted omitted-guard
witness are implemented. The bounded all-operation upgrade sweep is ignored locally and selected
explicitly by the hosted compatibility workflow. That workflow also builds the frozen old source
with its original lockfile, retains executable/source/harness hashes and exact before/after byte
manifests. No completed hosted evidence, large fault campaign or measured recovery overhead is
claimed until the corresponding exact-head jobs and measurements finish.

## Independent review corrections and expanded hosted matrix

Header preflight now requires exactly `min(32, file length)` bytes and stable length across prefix
collection. It retains those exact bytes and the length. Recovery and contextual raw reads must
match that admitted view before parsing or any repair; an initially incomplete header must remain
the identical incomplete full read. A short or changing seam view therefore refuses instead of
truncating newly visible complete data. Guarded versions are rejected as soon as all four version
bytes exist (including 12–15-byte headers), before the repairable-incomplete classification.
Changing-view regressions pin equal-length header swaps, dishonest short prefixes, complete full
views following short admission, and unknown-version short headers without callbacks or mutation.

The global corpus additionally contains independent wrong-length, reserved-field, bad-CRC and
capability-drop examples plus every strict guarded-header prefix. The hosted matrix now uses real
LCIX/LROW ChangeSet bytes in empty-tail promotion, nonempty legacy rotation, and later guarded
rollover. For four seeds in each of clean and torn-write/lost-directory profiles it observes each
control operation, cuts at every operation, fails each append with ENOSPC, fails every file/directory
sync, and injects every append prefix including a full append followed by an error. The torn-write
profile disables garbage injection; header corruption has separate refusal fixtures. This is a
bounded scripted matrix, not soak or arbitrary corruption adoption evidence. Each targeted fault
must be reached. Acknowledged prefixes, exact adopted envelopes, capability contexts and repeat
recovery are checked; raw and recovered directories are exported for every history.

The frozen old reader runs on each exported directory and records complete before/after filename
and byte-hash manifests. Refused guarded directories and already-recovered legacy directories must
remain byte-unchanged. Raw unguarded torn creations may undergo the old reader's documented repair;
that allowed mutation is recorded explicitly and cannot contain an acknowledged LCIX record.
The original hosted attempt failed while compiling the old harness because the historical RowError
did not implement Error; explicit error conversion corrects that harness failure. Expanded hosted
proof remains pending until its exact-head run completes successfully.
