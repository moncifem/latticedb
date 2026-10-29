# RFC 0008: WAL admission for required engine capabilities

Status: accepted
Comment period ends: waived by the project owner on 2026-10-29
Decision ADR: ADR 0022

## Motivation and authority

On 2026-10-29 the owner required enforced old-reader refusal before activation of the managed-index
LCIX proposal in RFC 0005 / PR 209. Operational-only coordinated upgrade was rejected. After
implementation, independent review, completed compatibility proof and disclosure of the warmed-cache
costs and memory-evidence limits, the owner explicitly accepted RFC 0008 / ADR 0022 and waived this
specific seven-full-day wait on 2026-10-29. Merge of PR 222 remains conditional on exact-head green CI.
The [narrow decision record](../evidence/wal-admission-owner-decision-2026-10-29.md) preserves the
owner's response and scope. It does not waive PR 209's hold, activate LCIX, accept a checkpoint
format, assign a release or sign a phase gate.

Existing v1 readers preserve unknown record types and RowStore skips unknown engine payloads. Thus
an unknown record, LCIX marker, directory sidecar, or the ignored v1 header reserved word cannot stop
a shipped reader. A directory containing compound row changes can silently appear incomplete.

## Proposal

### Header and capability assignment

The owner accepted the header values and capability assignment below. Leave all existing
`LATLOG01` v1 headers and records byte-unchanged. Introduce a distinct guarded header, before records:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | ASCII `LATCAP02` |
| 8 | 4 | Little-endian u32 header version `2` |
| 12 | 2 | Little-endian u16 header length `32` |
| 14 | 2 | Reserved zero u16 |
| 16 | 8 | Little-endian u64 required engine capability mask |
| 24 | 4 | Reserved zero u32 |
| 28 | 4 | CRC32C of the preceding 28 header bytes |

Mask bit 0 (`1`) means managed compound catalog/index/row replay. No other bits are
assigned by this RFC. A guarded header must require at least one defined capability. Reject
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
converter. This RFC does not compose, accept or select any dormant checkpoint engine format.
A future checkpoint reader must preserve admission and cannot erase the required-capability history.

## Independent compatibility fixtures and executable tests

Implementation must add independently framed global corpus fixtures with version/acceptance metadata:
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

Acceptance required independent review of header dispatch, capability propagation, every repair
path, raw tools and checkpoint interactions; exact-head checks; unchanged v1 corpus; measured
header-preflight overhead, bounded prefix-allocation controls and disclosed recovery-memory limits;
and a recorded owner format decision. Independent review and completed evidence are recorded below.
The owner accepted this scope and explicitly waived this RFC's review wait; final-head CI remains
required before merge. Merely merging a design document or selecting the guard approach
cannot activate LCIX. PR 209 stays draft until this guard is implemented and accepted and its own
unwaived hold and review requirements are satisfied.

## Implementation scope and seam contract

The implementation adds `Io::read_prefix(path, max)` as a required, deliberately breaking seam API for these
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
manifests. The exact-head hosted proof at `a132a36bbe9a1d0472f1bb0bc989c8e8bc7c7ff7`
passed in [run 40906145857](https://github.com/moncifem/latticedb/actions/runs/40906145857).
This bounded proof does not imply a large fault campaign; owner acceptance is a separate recorded
decision. The subsequent hosted measurement also passed; its exact tested merge source, observations
and limits are recorded below.

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
did not implement Error; explicit error conversion corrects that harness failure. The subsequent expanded hosted proof completed successfully at the source recorded below.

The independent generator emits 42 admission vectors; together with the unchanged pre-existing WAL
vector, the global corpus has 43 files. Matrix ChangeSet ids and HLC wall fields are independently
framed in their specified byte orders, increasing together across each real kernel history. A
normal test pins the literal fields and every recovered matrix history also passes the kernel
scrubber. Frozen-reader evidence checks tracked worktree/index cleanliness before build and after
execution setup, permits only the reviewed untracked harness, verifies original lock/toolchain
bytes against the immutable revision, and records their hashes plus verbose rustc/Cargo versions.


## Bounded hosted preflight measurement

The separate `recovery-measurement` job compiles the same reviewed host template against the actual
frozen `743b0fb52a495a3f875490268d604ed543be9e64` Log and the new Log, both in release mode
with their original lockfiles. Source preparation supplies only the actual API call and, in the new
harness, the required bounded-prefix seam method. It does not copy a decoder or modify tracked frozen
source. It compares both readers on identical independently framed legacy files; a third case uses
the new reader on guarded files with byte-identical record bodies. There is one valid ChangeSet per
segment, with strictly increasing literal IDs, clocks and LSNs. Cases vary 1/32/256 segments and
64/65,536-byte row values. This isolates streaming Log recovery, not full Kernel or RowStore rebuild.

One warmup and five samples per mode retain raw process output, internal monotonic open time and
GNU time whole-process peak RSS. Segment files are explicitly read into the OS cache before every
sample; old/new execution order alternates. These are warm-cache observations, not cold-disk
latencies. No regression threshold is invented. The artifact retains exact commands, build settings,
source/binary/lockfile/toolchain/dataset hashes, CPU/memory context and every sample. It checks no
clean file changes during recovery and independently checks record/body/segment/full-read counts.

The CountIo wrapper delegates to the shipped StdIo and records prefix calls, returned bytes and
request/return/Vec-capacity maxima. All new-reader samples must show one 32-byte prefix per segment,
all maxima at most 32, and one full-file read per segment; old-reader prefix counts must be zero.
This measures the shipped bounded buffer path without unsafe allocator instrumentation. The seam
contract and existing adapter tests cover exact `min(limit, length)` reads; these observations do not
prove a bound for arbitrary third-party adapters. Preflight retains one small prefix per segment,
plus names, parsed headers and container metadata; full recovery also holds actual segment/record
buffers. Peak RSS includes the whole process. Neither the prefix maximum nor RSS is a complete
allocation bound or a checkpoint-memory result. Owner acceptance explicitly followed disclosure of
these measured results and limits, independent review and a waiver of this RFC's specific wait.


## Completed hosted observations

[Run 40908208450](https://github.com/moncifem/latticedb/actions/runs/40908208450) passed for PR head
`f9f0307b42766a01261670bc51a23b1dd88b5581` at actual merge source
`fb11b8f657643e250adb2da0d78f4f14ded555cd`. The frozen reader checked 4,648 histories / 9,296
raw-recovered snapshots. Six measurement cases retained 90 timed samples and 18 warmups. For 256
tiny legacy segments median open increased 1.937 ms / 69.1%; at 256 larger segments the increase was
2.190 ms / 4.75%. This is warmed-cache instrumented admission/open with filesystem calls and
synchronization, not pure CPU decoding or an isolated prefix-syscall measurement. Every new-reader
prefix request/return/capacity was at most 32 bytes; process peak RSS does not bound total allocation.
See the [full observations and limits](../evidence/wal-admission-40908208450.md) and
[compact provenance/sample record](../evidence/wal-admission-40908208450.json).

Independent review resolved the identified changing-view/short-prefix/version-dispatch blockers
and validated the completed measurement design. The owner explicitly accepted RFC 0008 / ADR 0022
and waived this specific wait on 2026-10-29, conditional on final-head green CI before merge. See the
[narrow decision record](../evidence/wal-admission-owner-decision-2026-10-29.md). Rebase validation
does not change the historical measured source, waive PR 209's independent hold, or activate LCIX.
