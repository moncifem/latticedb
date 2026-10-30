# ADR 0030: A shared protocol crate for the sync wire types

Status: proposed

Requirement: issue #356, live collaboration package (#518).

## Context

`layers.toml` forbids `lattice-api` from depending on `lattice-sdk` and `lattice-testkit` from depending
on `lattice-sdk`. The sync frame types and codec lived in `lattice-api` (`sync.rs`, `websocket.rs`), so
the SDK client kept a one-to-one mirror of `sync::Message`, `Event` and `ErrorCode` and the subscription
host was tested against a mirror of the client rules. A mirror is a second definition that can drift
from the first without any check noticing. The live-collaboration slice adds a gateway (#524) and a
Rust client driver (#528) that must agree on every byte, so one definition is needed.

## Decision

Add `lattice-proto`, layer 1 with no workspace dependencies, and move into it the pieces both sides
need, unchanged:

- `sync`: frame types, limits, error codes, version negotiation, the strict bounded frame codec and
  the Kani proofs.
- `websocket`: the RFC 6455 subset (handshake, client and server frame codec, the sans-I/O
  connection). It depends on `sync` for its size limits and on `base64` and `sha1` for the accept key,
  which are the only external dependencies of the crate.

Layer 1 is the lowest layer both `lattice-api` (6) and `lattice-sdk` (6) may reach and, with
`lattice-testkit` (8), leaves room for any later crate. The crate is deterministic (no clock, no I/O,
no `HashMap`, no unsafe code).

`lattice-api` re-exports both modules (`lattice_api::sync`, `lattice_api::websocket`), so existing
paths, the fuzz targets and the Rust-owned conformance vectors (`conformance/vectors.json`, rendered by
`crates/lattice-api/src/conformance.rs`, which also covers the change feed) are unchanged.
`lattice-sdk` replaces its mirror with re-exports (`client::Frame` is `sync::Message`,
`client::WireError` is `sync::ErrorCode`, `client::Event` is `sync::Event`). The TypeScript and Python
clients are not touched.

## Consequences

- `lattice-sdk` now pulls `base64` and `sha1` transitively, because the WebSocket module lives in the same crate.
- One definition of the frames: the SDK client and the server codec cannot disagree, and a test in each
  crate decodes the same hand-written bytes to the same value.
- No behaviour change and no wire change. The bytes, limits and error codes are as before.
- `lattice-api` and `lattice-sdk` snapshots change (re-exports); `lattice-proto` gets its own snapshot.
- The change-feed codec stays in `lattice-core`, where the kernel owns it. Moving it is a separate
  decision if a client ever needs to decode feed events without the kernel.
- The mutation, fuzz and Kani coverage of the moved code now belongs to `lattice-proto` (the `kani`
  workflow package list is updated).
