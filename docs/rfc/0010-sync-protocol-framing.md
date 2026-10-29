# RFC 0010: Sync protocol framing and messages

Status: draft
Comment period ends: 2026-11-05
Decision ADR: none yet (proposed ADR 0024)

## Motivation

P3-01/P3-02 need a bounded, independently testable framing layer for live synchronization. This
proposal reviews the candidate codec in PR #264 and its [format table](../../formats/sync-protocol-v1.md).
It does not implement transport, authentication, subscriptions or a handshake state machine. No format
acceptance or owner waiver is assumed. The seven-day comment period starts on 2026-10-29; ADR 0024 is
proposed. The separate change-feed candidate is under RFC 0009 / ADR 0023 in PR #267.

## Proposal

A frame is type `u8`, payload length `u32` big endian, and that many payload bytes. The absolute payload
limit is 1048576 bytes, excluding the five-byte header. `decode_with_limit` additionally clamps to the
caller's lower limit. The decoder returns the first frame and its consumed length, allowing subsequent
frames in the same buffer. Trailing bytes inside the declared payload are refused. A partial header or
payload returns incomplete; an unknown type is rejected once its first byte is present, and an excessive
length is rejected once the header is present. A complete payload with truncated fields is an error.

All integers are big endian. Text is a `u16` byte length plus valid UTF-8 with no Unicode control
characters; a cursor is a `u16` length plus opaque bytes; event data is a `u32` length plus opaque bytes.
These are proposed layouts, not accepted encodings. Message types and fields are:

| Type | Message | Payload |
|---|---|---|
| 1 | Hello | min/max version `u16`, agent text |
| 2 | Welcome | chosen version `u16`, max payload `u32`, server text |
| 3 | Subscribe | subscription `u32`, scope text, cursor |
| 4 | Events | subscription `u32`, count `u16`, repeated cursor and data |
| 5 | Ack | subscription `u32`, nonempty cursor |
| 6 | Error | code `u16`, subscription `u32` (zero means connection), message text |
| 7 | Ping | nonce `u64` |
| 8 | Pong | nonce `u64` |

Agent/server text is 1..64 bytes, scope 1..512, error message 0..256. Cursors are at most 64 bytes,
nonempty in Events/Ack and optionally empty in Subscribe. Subscription IDs in Subscribe/Events/Ack
are nonzero; Error may name zero. Events count is 1..256. Per-event data must fit the total payload
limit; no interpretation or causal/cursor ordering check occurs here. Welcome max payload is
1024..1048576. Every decoded field length is checked before copying; event vector allocation is bounded
by 256. Encoding must reject an oversize payload before copying the offending data and leave the
caller's output unchanged on error. A host must also bound its receive buffers, queued frames, session
counts and deadlines; a codec limit alone does not provide connection-level admission.

Hello must offer `1 <= min <= max`. The pure negotiation function selects the highest common version
with this implementation's current range 1..1. Malformed ranges produce `malformed`; nonoverlap produces
`unsupported_version`. Welcome must name version 1. Error codes are exactly: 1 unsupported_version,
2 malformed, 3 unknown_subscription, 4 invalid_scope, 5 invalid_cursor, 6 too_many_subscriptions,
7 internal. Unknown message types/codes, bad UTF-8/control text and extra payload bytes are refused.

A future session must enforce Hello first, successful Welcome before Subscribe, legal message directions,
selected-version dispatch and closing after failed negotiation. The codec accepts message shapes without
tracking this state. Frames have no embedded version, so parsing unnegotiated/later-version bytes as v1
would be unsafe without that enclosing admission policy. No SQL listener accepts these messages today.

### Independent evidence and remaining verification

The format contains 19 worked examples and the tests contain handwritten vectors independent of the
encoder, refusal cases, negotiation tables, seed checks and property tests for canonical round trips,
mutated frames and incomplete prefixes. The `sync_frame` target has 18 committed seeds. Wiring it into
nightly, weekly and manual 24-hour matrices schedules future coverage; wiring is not proof of execution.
No frozen golden corpus, actual handshake interoperability, reconnect/subscription simulation, durable
restart, runtime memory admission or authentication evidence is established by these codec tests.
Before freezing, independently compare golden fixtures against the layout and obtain execution
results. Before runtime activation, review the session and transport contracts separately.

## Alternatives

JSON would ease inspection but still needs framing, canonicalization and explicit resource bounds.
Reusing PostgreSQL messages would conflate two distinct session contracts. Per-frame version tags
would simplify later dispatch at additional overhead; this candidate instead relies on mandatory
connection negotiation, which is not implemented by the codec. Deferring encoding and exposing only
message types would avoid an early binary compatibility promise.

## Compatibility and migration

No existing WAL, row, checkpoint or PostgreSQL encoding changes. This is a candidate new sync wire
format, not an activated listener. Keep bytes experimental until acceptance and reviewed frozen golden
fixtures; inventory any experimental consumers before revising bytes and agree an explicit migration.
After acceptance, never silently reinterpret a version's layout: negotiate another version or refuse
and close. No downgrade fallback after failed negotiation is specified.

Event bodies and cursors remain opaque; using RFC 0009's change-feed encoding needs explicit integration.
A maximum change-feed upsert exceeds this protocol's 1 MiB frame limit. Therefore this proposal does
not claim every feed event is transportable: before integration, decide an independently reviewed size,
refusal or fragmentation policy without silently dropping events. Scope authorization, stream identity,
retention errors, filtered cursor semantics, ack retention and atomic change-set delivery also require
separate contracts. No migration/admission behavior for those missing layers is invented here.
