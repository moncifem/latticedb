# Format: Sync protocol v1

The reference codec now lives in `crates/lattice-proto` (`lattice_api::sync` re-exports it).

Status: experimental candidate, unaccepted. Proposed in
[RFC 0010](../docs/rfc/0010-sync-protocol-framing.md) and
[ADR 0024](../docs/adr/0024-sync-protocol-framing.md). The owner authorized PR #264 to publish the
experimental codec after final green CI, waiving only its seven-day PR publication hold. The format
remains unaccepted/proposed and its comment period still ends 2026-11-05; no freeze or runtime
activation is authorized by that publication decision. Requirements P3-01 and P3-02; the codec is `lattice_api::sync`; the fuzz target is `fuzz/fuzz_targets/sync_frame.rs`.

A client (an SDK, a plugin) keeps a connection to a server and receives the changes in the scopes it
subscribes to. This document is the framing and the message set of that connection over any ordered,
reliable byte stream. It carries no transport, no authentication and no query language: the scope text
and the event bodies belong to other documents, and a cursor is opaque bytes.

## Framing

A frame is `type (u8) | length (u32) | payload`. All integers are big-endian. `length` is the payload
length in bytes, not counting the five header bytes.

* The payload is at most **1,048,576 bytes** (`MAX_PAYLOAD`). A reader judges the type as soon as the first byte is present and the length as
  soon as the five header bytes are present: an unknown type or a larger length is an error without
  waiting for the payload. A server may announce a lower limit in `Welcome.max_payload`; a reader
  applies it with `decode_with_limit`.
* Fewer bytes than a whole frame is "incomplete", not an error; the reader waits for more.
* The payload must be consumed exactly. Bytes left after the last field are an error.
* Every value has one encoding. There are no optional fields, padding, flags or varints, so
  `encode(decode(bytes)) == bytes` for every frame the decoder accepts.

Fields: `u16`/`u32`/`u64` are fixed width. **text** is `length (u16) | UTF-8 bytes`; it must be valid
UTF-8 and contain no control character (Unicode category Cc). **cursor** is `length (u16) | bytes`,
at most 64 bytes, never interpreted. **bytes** is `length (u32) | bytes`.

## Field limits

| Field | Limit |
|---|---|
| payload | 1,048,576 bytes |
| cursor | 64 bytes |
| scope | 1 to 512 bytes of text |
| agent, server | 1 to 64 bytes of text |
| error message | 0 to 256 bytes of text |
| events per `Events` frame | 1 to 256 |
| subscription id | 1 to 4,294,967,295 (0 is reserved) |
| `Welcome.max_payload` | 1,024 to 1,048,576 |
| version | 1 to 65,535 (0 is reserved) |

## Messages

| Type | Name | Direction | Payload |
|---|---|---|---|
| 0x01 | Hello | client to server | `min_version u16, max_version u16, agent text` |
| 0x02 | Welcome | server to client | `version u16, max_payload u32, server text` |
| 0x03 | Subscribe | client to server | `sub u32, scope text, cursor` (empty cursor: from the beginning) |
| 0x04 | Events | server to client | `sub u32, count u16, count x (cursor, data bytes)` |
| 0x05 | Ack | client to server | `sub u32, cursor` (at least 1 byte) |
| 0x06 | Error | either | `code u16, sub u32, message text` (`sub` 0: whole connection) |
| 0x07 | Ping | either | `nonce u64` |
| 0x08 | Pong | either | `nonce u64`, the nonce of the Ping answered |

Any other type byte is refused. `Hello` requires `1 <= min_version <= max_version`. `Welcome.version`
must be a version the decoder knows (today 1): a `Welcome` naming another version is refused rather than
guessed at. In `Events`, each event cursor is the position after that event, 1 to 64 bytes; subscribing
with it resumes after that event. The event `data` is opaque here; its length is limited only by the
payload limit.

### Error codes

| Code | Name | Meaning |
|---|---|---|
| 1 | unsupported_version | Version ranges do not overlap. The sender closes the connection. |
| 2 | malformed | A frame did not decode, or is not allowed in the current state. |
| 3 | unknown_subscription | A frame names a subscription that does not exist. |
| 4 | invalid_scope | The server does not serve this scope. |
| 5 | invalid_cursor | The server cannot resume from this cursor. |
| 6 | too_many_subscriptions | The connection is at the server's subscription limit. |
| 7 | internal | The server failed for its own reasons. |

A code outside 1 to 7 is refused by the decoder.

## Handshake and version negotiation

The client sends `Hello` first, offering the range `min_version..=max_version`. The server speaks a
range too (today 1 to 1) and picks **the highest version in both ranges** (negotiate down). If the ranges
do not overlap, it sends `Error(unsupported_version, sub 0)` and closes (refuse). A range with
`min_version` 0 or above `max_version` is `malformed`. On success the server sends `Welcome` with the
chosen version; the client must not send `Subscribe` before it. Later versions that change a frame
layout need a new version number. Frames carry no version: a future session must enforce successful
negotiation and dispatch only the selected layout. This pure codec does not enforce handshake state,
direction, version admission or close-on-error.

Negotiation table for a server that speaks only version 1 (`lattice_api::sync::negotiate`):

| Offer | Result |
|---|---|
| 1..=1 | version 1 |
| 1..=2 | version 1 (negotiated down) |
| 1..=65535 | version 1 |
| 2..=2, 2..=9, 65535..=65535 | `unsupported_version` |
| 0..=1, 0..=0, 0..=65535, 2..=1, 1..=0 | `malformed` |

## Subscriptions

`Subscribe` starts subscription `sub` for `scope`, resuming after `cursor` (empty: from the beginning).
The server answers with `Events` for that `sub`, or an `Error` carrying that `sub`
(`invalid_scope`, `invalid_cursor`, `too_many_subscriptions`). `Ack` tells the server the client has
processed everything up to and including the cursor. Delivery is at least once: after a reconnect the
client subscribes with the last cursor it processed. Which events a cursor covers, and ordering, are
defined by the change feed, not here. `Ping` may be sent at any time and must be answered with a `Pong`
carrying the same nonce.

## Worked examples

All bytes in hex. These are the vectors in `crates/lattice-proto/src/sync.rs`.

1. Hello, versions 1 to 1, agent `sdk-rs` (payload 12 bytes):
   `01 0000000c | 0001 0001 0006 73646b2d7273`.
2. Hello, versions 1 to 3, agent `a`:
   `01 00000007 | 0001 0003 0001 61`.
3. Welcome, version 1, max payload 1,048,576, server `lattice`:
   `02 0000000f | 0001 00100000 0007 6c6174746963 65`.
4. Subscribe, sub 1, scope `ws/main`, no cursor:
   `03 0000000f | 00000001 0007 77732f6d61696e 0000`.
5. Subscribe, sub 2, same scope, cursor `0102030405060708`:
   `03 00000017 | 00000002 0007 77732f6d61696e 0008 0102030405060708`.
6. Events, sub 1, two events (cursor `0001` data `61`; cursor `0002` data empty):
   `04 00000017 | 00000001 0002 | 0002 0001 00000001 61 | 0002 0002 00000000`.
7. Ack, sub 1, cursor `0002`:
   `05 00000008 | 00000001 0002 0002`.
8. Error, unsupported_version, connection level, message `version`:
   `06 0000000f | 0001 00000000 0007 76657273696f6e`.
9. Error, unknown_subscription, sub 9, empty message:
   `06 00000008 | 0003 00000009 0000`.
10. Ping, nonce 1: `07 00000008 | 0000000000000001`.
11. Pong, nonce `0x0102030405060708`: `08 00000008 | 0102030405060708`.

Refused inputs:

12. Hello followed by one extra payload byte, length 13: `01 0000000d | 0001 0001 0006 73646b2d7273 00`, trailing bytes.
13. Unknown type: `09 00000000`.
14. Length one above the limit: `07 00100001`, bad length, refused on the header alone.
15. Hello with `min_version` 2 above `max_version` 1: `01 00000007 | 0002 0001 0001 61`, invalid version range.
16. Welcome naming version 2: `02 00000009 | 0002 00100000 0001 78`, invalid version.
17. Ack with an empty cursor: `05 00000006 | 00000001 0000`, invalid cursor.
18. Error with code 8: `06 00000008 | 0008 00000000 0000`, invalid error code.
19. Ping cut after seven bytes of its 13: `07 00000008 00 00`, incomplete; the reader waits.

## Not covered

Authentication, scope grammar, the event body and cursor encoding, flow control beyond `Ack`,
unsubscribe, compression and TLS. A later version adds them.
