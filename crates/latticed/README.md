# latticed development listener

`latticed` now adapts the sans-I/O PostgreSQL session to TCP. The placeholder handler answers
`SELECT 1`; other SQL returns `0A000`. Simple and extended query framing are handled by the existing
session. The default is explicit development trust on loopback. Configured SCRAM mode requires
TLS and refuses plaintext startup or trust fallback. It does not open a database or establish the
five-driver matrix. CancelRequest routes a cooperative token and cannot preempt blocking I/O.

```sh
cargo run -p latticed -- --auth development-trust --listen 127.0.0.1:5432 --max-connections 32
```

The default address is `127.0.0.1:5432`. Read and write timeouts default to 30 seconds and 5 seconds;
set `--read-timeout-ms` and `--write-timeout-ms` to change them. A read timeout covers a complete
frame, including fragmented or slowly supplied headers and bodies. The existing codec's 1 MiB
message limit is checked before allocating a frame body. The configured count must be 1–1024 and
bounds registered SQL sessions. An excess SQL startup receives `53300` after its packet is classified.
Eight additional transport workers allow CancelRequest and startup packets to be read at that cap;
the total worker bound is the configured count plus eight. At that absolute bound new transports
close without a reply. Flooding the extra slots can still prevent cancellation from being admitted.

Type `shutdown` followed by a newline on stdin to stop cleanly. EOF on stdin also stops the process;
a supervisor must keep that control channel open while serving. This control mechanism does not
install signal handlers. Embedders can call `ServerHandle::shutdown`; dropping the handle also stops
the listener, interrupts active socket reads/writes and joins all connection workers.

Each accepted connection has one host thread. Protocol logic remains in `lattice-api`; socket I/O,
thread lifecycle and admission limits stay in this composition crate. Socket tests independently
encode their expected messages and cover fragmentation, pipelining, connection admission, timeouts,
malformed lengths and shutdown. No phase or driver-conformance gate is established by this adapter.


## Cooperative cancellation

Each admitted SQL connection receives a registry-generated `BackendKeyData` pair. A separate
CancelRequest connection checks both fields, signals the current query and closes without a reply;
unknown and stale keys have the same no-reply behavior. Keys live for the whole connection and are
unregistered on termination, disconnect or worker unwind. The host uses explicit logical time zero
and maximum lifetime in the registry rather than expiring a still-open client's only credential.
Registry PIDs are not reused during this server lifetime.

`ServerHandle::start_with_handler` creates a handler per admitted connection. The host always drives
`Session::on_message_with_cancellation` and allocates a fresh token for Query, Parse and Execute.
Handlers that override `execute_cancellable` or `describe_cancellable` must poll the token between
bounded work units and choose their abort/commit boundary. Returning `57014` gives the normal query
error flow; extended errors discard messages until Sync, and an active transaction remains failed
until rollback. A portal cancelled before execution cannot resume; cached successful results remain readable. Idle cancellation does not poison the next query.

Legacy handler methods remain usable: their default cancellation-aware methods check before entry,
but they cannot preempt a handler that blocks without polling. Successful possibly-committed results
are never relabeled as cancellation after return, and already-computed result transmission is not
interrupted. No real SQL executor, statement rollback implementation or driver conformance is claimed.
Shutdown atomically closes admission and new query-token allocation while signalling all registered
tokens, closes sockets and then joins workers; cooperative handlers
can exit promptly. Registry poison causes normal operations to fail closed, while shutdown and
unregister recover the lock only to cancel/clean up safely.

## Configured SCRAM over TLS

```sh
cargo run -p latticed -- --auth scram-tls --listen 127.0.0.1:5432 \
  --tls-cert /secure/server-chain.pem --tls-key /secure/server-key.pem \
  --scram-verifiers /secure/accounts.txt --scram-decoy-secret /secure/decoy-secret.bin \
  --startup-timeout-ms 10000
```

Provision those files externally. The decoy-secret file is mandatory and must contain exactly 32
cryptographically random binary bytes (not hex/Base64 text or a trailing newline). Generate it once
with an operating-system cryptographic RNG, restrict access like the verifier file, and persist the
same file across reloads/restarts and instances serving the same account set. The listener never
generates a replacement on load. Back up and restore it together with credential configuration;
changing it changes unknown-account salts and can reveal account existence to observers comparing
challenges across rotation. Rotation/compromise requires a separate operational policy and is not
claimed unobservable. The credential file has one `username<TAB>verifier` per line:
`SCRAM-SHA-256$4096:base64-salt$base64-stored-key:base64-server-key`. Only the uniform 4096-iteration,
16-byte-salt profile is accepted; keys are 32 bytes, names at most 63 UTF-8 bytes without controls,
and files at most 256 KiB/1024 accounts. There is no plaintext password flag, embedded account,
password derivation on the network path or credential-file persistence API. Protect file permissions
and prepare passwords during external provisioning; SASLprep is outside this listener.

Clients must use certificate verification, including the intended server name (for example libpq
`sslmode=verify-full` with a trusted CA). Standard SSLRequest upgrades to TLS 1.2/1.3; SCRAM never
runs over plaintext. Trust refuses non-loopback binds. Certificate/key validation happens before bind.
No mutual TLS, direct TLS startup, SCRAM-PLUS, channel binding or certificate rotation is implemented.

A single deadline covers TCP accept, startup negotiation, TLS and proof; default 10 seconds.
The existing transport/SQL bounds remain, with at most 64 TLS starts per rolling second per listener.
Unknown accounts get keyed per-name synthetic credentials and the same terminal `28P01` proof failure;
no measured timing-resistance or comprehensive account-enumeration defense is claimed. BackendKeyData,
successful startup and queries are gated on proof. Tests generate certificates/keys in memory and
independently calculate client proofs. The accepted design is [RFC 0007](../../docs/rfc/0007-pgwire-scram-tls-integration.md)
with [ADR 0021](../../docs/adr/0021-pgwire-scram-tls-integration.md). Independent security review and
fix rereview are complete; the owner accepted the integration and waived its comment period on
2026-10-29. This decision does not establish production readiness, driver conformance or a phase gate.

## Timeouts, close and cancel limits

- `--read-timeout-ms` bounds one frame once its first byte arrives. `--idle-timeout-ms` (default: the
  read timeout) separately bounds the wait for the next frame of an authenticated session, so a pool
  can keep idle connections longer than it tolerates a stalled frame. SASL frames stay under the
  startup budget.
- `--write-timeout-ms` bounds each socket write; `--response-timeout-ms` (default 15 s) is an absolute
  budget for one whole response, so a peer that drains a few bytes per interval cannot hold a worker.
  A response that exceeds it ends the connection.
- Every connection ends through one close path. A TLS connection gets a best-effort `close_notify`
  there: after an orderly end, after a FATAL error reply (failed authentication, protocol error),
  after read, idle, response, write and startup-budget timeouts, and when a handler panics (the
  close runs from a drop guard). It only writes, never reads, ignores the expired startup budget,
  and is capped at 2 s absolute, so a slow peer cannot hold the worker. It is skipped only when
  the peer is gone or does not accept the bytes within the cap, and a plaintext connection has
  nothing to send.
- Each socket write is capped at 16 KiB so the response deadline is rechecked while a peer drains.
- `--tls-key` and `--scram-decoy-secret` files must be regular files with no group or other
  permission bits (mode 0600 or 0400, unix only). All configured security files (including the certificate chain and
  verifiers) are stat-checked to be regular files before opening, and the key and decoy handles are
  checked again before reading. The stat and the open are two steps: a local attacker who can replace
  the path in between can still block startup with a FIFO, and symlinks are followed. A dependency-free
  atomic open is not available in std.
  Limits: only the mode bits are checked, not the owner; the verifiers file is not checked; and
  secret mounts at 0440 or 0444 (common for orchestrator-mounted secrets) are refused until
  their mode is tightened.
- CancelRequests are limited to 16 per second per client address (IPv6 /64) and 256 per second
  globally; an excess request is dropped without a reply, like an unknown key. This is a resource
  bound, not a DoS guarantee. The budget is charged before the key is looked up, so wrong keys
  consume it: about 16 addresses (or a few /64s) can use up the global budget and starve real
  cancels, and all clients behind one NAT or proxy share one 16 per second budget.

Not done here: channel binding / `SCRAM-SHA-256-PLUS`, the decoy-secret length-only check and the
`auth.rs` comment fix live in `lattice-api` and are tracked in issue 347.

## SQL from a data directory

```sh
# loopback development trust: the row policy of the development principal is always explicit
cargo run -p latticed -- --data-dir ./data --development-policy allow-all

# SCRAM over TLS: one 'account<TAB>allow-all|deny-all' per line; an unlisted account is denied
cargo run -p latticed -- --auth scram-tls --data-dir /var/lib/lattice --account-policies /secure/policies.txt \
  --tls-cert ... --tls-key ... --scram-verifiers ... --scram-decoy-secret ...
```

Without `--data-dir` the `SELECT 1` placeholder is unchanged. With it, `latticed` opens the write-ahead
log in the directory (created with mode 0700 when missing; one server at a time, a second one is
refused), replays it, then serves the single-table SQL executor (`lattice-exec`) to every admitted
connection. Startup fails before binding if the log cannot be recovered or the policy options are
inconsistent: `--development-policy` is required in trust mode and rejected in SCRAM mode, and
`--account-policies` is required in SCRAM mode and rejected in trust mode. There is no allow-all
default for anyone. `deny-all` also refuses `CREATE TABLE` and `DROP TABLE` (`42501`). Only these two
grants exist until the Phase 4 policy language lands.

**One database thread.** The executor is single-threaded, so one thread owns the database
(`lattice_api::exec_host`) and each connection forwards its statements to it. Statements therefore run
one at a time: a slow statement delays the others. `--statement-timeout-max-ms` (default 30000) is
both the default statement timeout and the ceiling a client's `SET statement_timeout` cannot lift;
`--statement-memory-bytes` (default 64 MiB) bounds the rows one statement holds. A CancelRequest
reaches a running statement between rows, and a statement cancelled while queued does not start.
`--max-connections` and the timeouts above apply as before. A connection that drops mid-transaction
commits nothing.

**What survives a restart or a crash.** Every committed statement is on disk when the client sees the
command tag. Recovery replays the log, so committed rows and table names come back. **Column
definitions do not** (#379): after a restart the recovered tables exist only by name, `SELECT` on one
answers `42P01`, and `CREATE TABLE` of the same name answers `42P07`. The server prints
`N recovered table(s) have no column definitions` on stderr at startup. Until the schema is persisted
(an RFC and an ADR, then a guarded log record), a data directory is useful for tables created in the
current run only, and a new table name is needed after each restart. The stored row layout is not a
frozen format.

Not wired yet: the five-driver matrix (libpq, psycopg, pgx, JDBC, node-postgres), binary result
formats, and a persisted schema. Text parameters of the extended protocol are tested. The tests in `tests/sql_binary.rs` use a plain client that frames
messages itself, over loopback trust and over verified TLS with SCRAM.
