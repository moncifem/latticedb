# lattice-sdk

Layer L6.

**Responsibility.** Generated and handwritten client SDKs (Rust first).

**Must not.** Hold schema knowledge not generated from LSL.

Allowed workspace dependencies: lattice-abi, lattice-lsl.

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

`lattice-sdk` may not depend on `lattice-api`, so `Frame` mirrors `lattice_api::sync::Message`; the adapter
that owns the codec converts. No unsafe, no panics from network input. Tests cover scripted scenarios, a
property test against a randomised cursor-honouring server, and a deliberately buggy server that makes the
property fail.
