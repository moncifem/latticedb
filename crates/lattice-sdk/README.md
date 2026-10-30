# lattice-sdk

Layer L6.

**Responsibility.** Generated and handwritten client SDKs (Rust first).

**Must not.** Hold schema knowledge not generated from LSL.

Allowed workspace dependencies: lattice-abi, lattice-lsl, lattice-proto.

## Modules

- `contract`: strict, bounded codec for the agent plugin contract v1 (`formats/plugin-contract-v1.md`).
  `decode_line` refuses unknown fields and versions, duplicate keys, floats, lone surrogates, invalid UTF-8
  and lines over 1 MiB; `encode_line` writes the canonical form; `Framer` splits a byte stream into lines
  with bounded memory.

## Conformance suite

`tests/support/` holds the checks any plugin can run: implement `PluginUnderTest` (send one line, return
the lines produced) and call `suite::run_all`. `tests/support/reference.rs` is an in-memory plugin that
passes them, with switchable faults; `tests/plugin_contract.rs` shows that each check fails for the fault
that breaks its rule.

## Sync client state machine

`client::SyncClient` is a sans-I/O client for the sync protocol v1 (`formats/sync-protocol-v1.md`). The
caller owns the socket, the frame codec and the clock: it passes decoded `Frame`s and a logical time to
`handle` and `tick`, and performs the returned `Action`s (`Send`, `Ready`, `Deliver`, `SubscriptionFailed`,
`Close`). Phases: `Disconnected`, `HelloSent`, `Ready`, `Subscribed`, `Closed`.

- `connect` produces `Hello`; `Welcome` is checked against the offered version range (a server that
  negotiates down is fine, a version outside the range or `unsupported_version` closes for good).
- `subscribe(scope, cursor)`, `ack(sub, cursor)` (merged `ack_batch` calls per wire `Ack`) and `flush_acks`.
- `reconnect` resubscribes every live subscription from its last acknowledged cursor. Events whose cursor is
  not after the last delivered one are dropped, so the application sees no gap and no duplicate from a server
  that honours cursors. This assumes cursors order by length then bytes within a subscription.
- `tick` sends `Ping` after `ping_interval` idle ticks and drops the connection after `ping_timeout`.
- Connection errors drop the connection (reconnect allowed); subscription errors remove that subscription.

`lattice-sdk` may not depend on `lattice-api`, so `Frame`, `Event` and `WireError` are the shared types of
`lattice-proto` (`sync::Message`, `Event`, `ErrorCode`), the ones the server codec uses; `lattice_proto::sync::{encode, decode}`
turns them into bytes with no conversion. No unsafe, no panics from network input. Tests cover scripted scenarios, a
property test against a randomised cursor-honouring server, and a deliberately buggy server that makes the
property fail.

## Offline outbox

`outbox::Outbox` is a sans-I/O write queue for offline use (P3-04). `enqueue(turn, kind, body)` returns a stable
record id `<client id>-<counter>` (the counter is stored with the queue; no time, no randomness). The queue is
bounded by item count and body bytes and answers an explicit `Full` error. `poll(now, jitter)` returns the next
`Batch` (one turn, at most `max_batch` records and at most 1 MiB of encoded `append` line, in enqueue order; a body
whose escaped line cannot fit alone is refused at `enqueue` with `BodyUnsendable`); one batch is in flight at a time and is resent
when its deadline passes, with `base_delay << attempt` capped at `max_delay` plus a caller-supplied jitter, all on
a logical clock. `on_receipts` clears `recorded` and `duplicate` writes; `fenced` drops every queued write of that
turn, returns them to the application and refuses the turn from then on. `connection_lost` keeps every unacked
write and resends it at the next poll.

`snapshot` / `from_bytes` store the queue: magic, version 1, client id, next counter, fenced turns, items, CRC-32,
with one canonical encoding, bounded sizes and no panic on any input. A stored write that could never be sent
(escaped line over 1 MiB, possible only from a build before the `BodyUnsendable` check) fails `from_bytes`;
`from_bytes_lossless` restores the rest, keeps the counter so no record id is reused, and returns those writes
like fenced ones. A write is only sent after the application
has stored a `Snapshot` and handed it to `mark_persisted`, so a crash can neither reuse an id for a different write
nor lose one; a crash between persist and ack just resends under the same id and the server answers `duplicate`.
Tests (`tests/outbox.rs`) cover scripted scenarios, a crash/restart property against a server that applies each
id once, and two deliberately broken variants that make it fail. Fuzz target: `outbox_decode`.
