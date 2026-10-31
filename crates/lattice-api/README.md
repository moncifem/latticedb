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

A prepared statement's parameter count is the larger of the types `Parse` declared and the highest `$n`
its text refers to (a gap counts). `Describe` reports the declared types first and OID 0 (unspecified)
for the rest, because the session infers no types, and `Bind` must supply exactly that many values (`08P01`
otherwise, also for a statement with no placeholders). The scan skips string literals (including `E'...'`), quoted identifiers, comments, dollar
quotes and a `$` that continues an identifier, ignores numbers above 32767, and is one bounded pass.

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
that asks the executor's actor thread for `RowStore::read_feed_as` as the principal's grant
(`PolicyTable::grant_for`, deny by default). The scope is `table/<name>` and needs the read bit on that table; a denied principal, a table it holds no read grant on, an unknown table and any
other scope are all `SourceError::UnknownScope`, decided before the cursor is read. The wire cursor is the 12-byte feed
cursor, `Event::data` the `change-feed-v1` encoding. A cursor below the vacuum floor is `TooOld`. A legacy oversize
key is skipped by continuing from its cursor, so a reader is never wedged. One read returns at most `max_events`
events and is a series of steps, each one actor request: a step examines at most a scan budget of versions (default
4,096, `FeedSource::with_scan_budget`) found through an in-memory commit index and builds events only for rows of the
requested table that the policy allows. A step costs the budget and the page, never the table size or the history (step time is
unmeasured); a caught-up reader examines nothing; a commit queued during a read runs between its steps. The commit index
is kept in memory for the row table only and is rebuilt by replay when the store opens, an O(history) step. Known issue,
accepted for the alpha: a subscriber of a quiet table with more than about 4.2 million other-table or hidden versions
after its last event gets `Unavailable` on every poll and does not recover by polling (table-scoped index: issue #661).
Not done: a per-principal cap on feed work and a wall-clock bound (issue #543). The experimental WebSocket gateway in `latticed` is the transport that drives this host.

`notify::CommitNotifier` (`ExecHost::commit_notifier`) tells waiters "something committed up to LSN `x`" so a
subscriber polls the feed source when woken instead of on a timer. The actor publishes the newest applied LSN (row or
catalog commit) after every statement, once the statement has finished, and before it answers the client. A waiter
calls `wait_after(seen, timeout)` and gets the newest LSN when it is past `seen` (at once if it already is), or
`TimedOut` (poll anyway: the timeout is the fallback), `Full` (more than `HostLimits::commit_waiters` threads are
asleep; poll on a timer) or `Closed` (shutdown, or the database thread ended, however it ended). The value and the
compare share one lock, so a commit that lands before a waiter sleeps is never missed. Memory is one `u64`, a flag
and a count, whatever the number of waiters; there is no per-waiter queue, and a commit takes one lock and one
`notify_all`. The wake carries the LSN and nothing else, no row or scope, and it is not checked against any
principal: it reveals that something committed (threat model G-07, issue #363), so hand it only to authenticated
connections. The wait counts its timeout in waits that ran out, not with a clock, so a waiter whose `seen` is past
the database can outlast its timeout while commits keep arriving.

Row commits made through `ExecHandler` carry an author id so change-feed events say who wrote. The id comes from the
connection's `Principal` only: the fixed id 1 for local development, or the id `AuthorRegistry` assigns to the
authenticated account name (injective, never reissued; `ExecHost::start_dir` keeps it in `authors.reg` beside the log
and refuses to start on a damaged, colliding, missing or older-than-the-log file; the file is created empty and narrowed to 0600 inside the 0700 data directory, and a symlink is refused). An account that is in the credential store but not in the policy file still gets an id on its first statement. The SQL text, `application_name` and the startup `user` never
reach it.

`websocket` is a sans-I/O RFC 6455 server-side codec for the future sync gateway: the HTTP upgrade request (strict, bounded, origin hook, `lattice.sync.v1` subprotocol), the accept key, frame decode and encode, message reassembly with size caps and UTF-8 checks, ping/pong and close handling, and timers on a caller-supplied logical clock. It does no I/O and is not connected to a listener, TLS or authentication yet ([ADR 0027](../../docs/adr/0027-sync-transport-websocket-first.md), proposed).

`bearer` is the credential layer for the HTTP and WebSocket front doors ([ADR 0031](../../docs/adr/0031-bearer-token-authentication.md)):
tokens of the fixed form `lat_<16 hex id>_<43 base64url characters>` (256-bit secret), issued from random
words by `bearer::issue`; a strict, bounded credential file (`CredentialStore::parse` and `render`) that holds only
a salted SHA-256 verifier per token, with at most two active tokens per account and a bounded rotation overlap;
`verify_bearer(header, store, now)`, which returns the same `Principal::account` a SCRAM login of that account
gets and gives unknown and wrong tokens one error; and `FailureLimiter`, a per-address count of failed
attempts. The verifier logs nothing and no token, salt or verifier appears in `Debug`, `Display` or errors.
Nothing calls it yet: the HTTP endpoint, reload, revocation of live connections and the credential tool are
#524, #577 and #582. No end-to-end timing claim is made.

