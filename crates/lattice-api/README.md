# lattice-api

Layer L6.

**Responsibility.** Protocol front doors: PG wire, S3, sync, OIDC endpoints, MCP, gRPC.

**Must not.** Contain business rules.

Allowed workspace dependencies: lattice-abi, lattice-seams, lattice-core, lattice-engines, lattice-identity, lattice-service, lattice-packs, lattice-lsl.


The proposed host integration adds `auth::Accounts` for bounded pre-provisioned verifier lookup and
`Session::with_scram` for explicit initial/final SASL phases. The account found by
`Accounts::exchange` becomes the connection's `Principal` once the proof verifies (trust mode uses the
fixed `Principal::local_development`), and every `QueryHandler` call receives it in a `QueryContext`. Hosts use `sasl_phase()` to select the
password decoder, select accounts only from startup, and provide TLS and handshake admission/time
bounds. A valid known-account proof precedes all successful startup metadata and query execution;
failed/unknown exchanges close without trust fallback. Uniform public credential profiles and keyed
synthetic unknown accounts use a mandatory provisioned `DecoySecret` (exactly 32 bytes), redacted in
Debug and zeroized on drop. Reuse the same secret across account reloads/restarts so synthetic salts
remain stable independently of fresh nonce RNG. Rotation changes unknown-account salts and can reveal
account existence across that event; no unobservable-rotation or measured timing-safety claim is made.
See [RFC 0007](../../docs/rfc/0007-pgwire-scram-tls-integration.md), still draft pending review.
