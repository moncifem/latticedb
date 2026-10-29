# ADR 0024: Sync protocol framing and messages

Status: proposed

## Context

P3-01/P3-02 need a sans-I/O sync message model with bounded canonical framing. PR #264 implements a
candidate codec but no server, transport or session state machine. [RFC 0010](../rfc/0010-sync-protocol-framing.md)
opens format review with a comment period ending 2026-11-05. On 2026-10-29 the owner explicitly
authorized PR #264: "Merge experimental codec; keep format proposed", waiving only its seven-day PR
publication hold after final green CI. The format comment period remains open; this is not encoding
acceptance, a freeze, runtime activation or a waiver for any other proposal.

## Decision

Propose the existing [candidate format](../../formats/sync-protocol-v1.md): an eight-type message set
with a one-byte type and four-byte big-endian payload length, at most 1 MiB. Hello/Welcome negotiate
the highest common version (currently 1), with explicit malformed/unsupported errors. Subscribe, Events,
Ack, Error and Ping/Pong carry bounded fields; event bodies and cursors are opaque. Unknown tags,
codes, invalid text, out-of-range values and unconsumed payload bytes are refused.

The pure codec must neither perform I/O nor suppress deterministic lint rules. Fixture tests embed
committed bytes at compile time. A future session, not this codec, must enforce handshake ordering,
selected-version dispatch, direction and close-on-refusal. No live sync or storage behavior is enabled.
This decision remains proposed. Independent review of the experimental codec and permission to publish
it do not replace a future owner decision accepting the encoding.

## Consequences

The decoder can process a stream incrementally and rejects excessive declared lengths before waiting
for the body. Per-frame bounds do not bound host queues, connection lifetime or aggregate allocations;
those admission rules remain required for activation. No authentication, integrity or scope authorization
is supplied by framing. At-least-once delivery and reconnect correctness require a subscription layer.

Handwritten vectors/property tests and fuzz seeds exist; scheduled matrix wiring is not execution
proof. Frozen independent fixtures, interoperable handshake and durable resume tests remain outstanding.
A future change-feed integration must reconcile its larger allowed values with this frame limit before
claiming complete delivery. No silent event loss or unreviewed fragmentation is implied. After acceptance,
format changes need explicit version negotiation/refusal and a consumer migration policy; this proposal
changes no existing WAL, checkpoint, row or PostgreSQL bytes and freezes no version yet.
