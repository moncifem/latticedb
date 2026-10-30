# lattice-api

Layer L6.

**Responsibility.** Protocol front doors: PG wire, S3, sync, OIDC endpoints, MCP, gRPC.

**Must not.** Contain business rules.

Allowed workspace dependencies: lattice-abi, lattice-seams, lattice-core, lattice-engines, lattice-identity, lattice-service, lattice-packs, lattice-lsl, lattice-proto.

The sync frame types and codec and the WebSocket codec live in `lattice-proto` so the SDK client uses the same
code; `lattice_api::sync` and `lattice_api::websocket` re-export them.


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
See [RFC 0007](../../docs/rfc/0007-pgwire-scram-tls-integration.md) (accepted, with ADR 0021).

`Session` keeps what a suspended portal still has to send exactly once and reads it with a cursor, so an
`Execute` with a row limit costs the rows it sends, not the rows left. Open portals are charged to a
per-session budget (`MAX_PORTAL_BYTES`, 64 MiB; `Session::with_portal_budget` changes it): the statement
text and bound parameters of every portal plus the rows a suspended portal has not sent. The budget is a
soft cap and never fails a statement after it ran. A `Bind` is refused with `53200` when the bytes held
by the other portals plus the new portal's own text and parameters would pass the budget (a refused
rebind of the unnamed portal leaves the old one in place). The first `Execute` of a portal is refused
with `53200` before anything runs only when the session already holds more than the budget. A
statement that did run always keeps its result (bounded by the statement memory budget), so retained
bytes stay under the budget plus one result. The result is fully materialized before it is counted, so
the budget bounds what a session retains, not its peak. `Close` returns the bytes. A `53200` raised
inside a block, like any session error there, calls `QueryHandler::abort_transaction` so the handler's
block fails with the session's. At most `MAX_PORTALS` (128) portals exist per session
(`54000` beyond). `COMMIT` and `ROLLBACK` destroy every portal, as PostgreSQL does. Not done: the
implicit transaction of the extended protocol does not end at `Sync`, so portals opened outside a block
survive it; statements are still bounded by count only, and the frame buffer is not yet sized by
received bytes (the rest of threat model G-06, #362).

The sync `subscribe::SubscriptionHost` is built with the connection's authenticated `Principal`
(`SubscriptionHost::new`; `local_development` is the loopback stand-in) and passes it to every
`EventSource::read_after`, so the source applies scope and row policy for that identity. The `Hello`
`agent` text is a label, never an identity. A transport that authenticates and supplies the principal
does not exist yet (threat model G-03).

`feed_source::FeedSource` is the kernel-backed `EventSource`: `ExecHost::feed_source(max_events)` returns a reader
that asks the executor's actor thread for `RowStore::events_since_as` as the principal's grant
(`PolicyTable::grant_for`, deny by default). The scope is `table/<name>` and needs the read bit on that table; a
denied principal, a table it holds no read grant on, an unknown table and any
other scope are all `SourceError::UnknownScope`, decided before the cursor is read. The wire cursor is the 12-byte feed
cursor, `Event::data` the `change-feed-v1` encoding. A cursor below the vacuum floor is `TooOld`. A legacy oversize
key is skipped by reading again from the last cursor, so a reader is never wedged. One read returns at most
`max_events` events, which bounds what is returned and not the cost: each read visits every key of every table plus the
versions after the cursor, builds events for hidden tables before the policy filter (memory O(history after the
cursor)), is never interrupted once running, and a reader that is not caught up scans twice. Only principals with a read
grant on the table reach the scan today; the engine-side limit is issue #543. No wake-on-commit and no transport exist yet.

`websocket` is a sans-I/O RFC 6455 server-side codec for the future sync gateway: the HTTP upgrade request (strict, bounded, origin hook, `lattice.sync.v1` subprotocol), the accept key, frame decode and encode, message reassembly with size caps and UTF-8 checks, ping/pong and close handling, and timers on a caller-supplied logical clock. It does no I/O and is not connected to a listener, TLS or authentication yet ([ADR 0027](../../docs/adr/0027-sync-transport-websocket-first.md), proposed).
