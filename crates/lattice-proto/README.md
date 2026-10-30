# lattice-proto

Layer L1.

**Responsibility.** The shared wire definitions of the sync protocol v1 (`formats/sync-protocol-v1.md`):
the frame types (`sync::Message`, `Event`, `ErrorCode`), the limits, version negotiation and the
strict bounded frame codec (`sync`), and the WebSocket handshake, frame codec and sans-I/O connection
state machine (`websocket`). `lattice-api` (the server) and `lattice-sdk` (the client) both depend on it,
so they decode the same bytes with the same code. `lattice-api` re-exports both modules under their
old paths (`lattice_api::sync`, `lattice_api::websocket`).

**Must not.** Do I/O, read a clock, hold session state, or depend on another workspace crate.

Allowed workspace dependencies: none.

## Verification

Property tests and edge tests live here with the code. The Rust-owned cross-language vectors
(`conformance/vectors.json`) are checked and rewritten by `lattice-api` (they also cover the change
feed); the Kani proofs in `src/sync/proofs.rs` run in the `kani` workflow.
