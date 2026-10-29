# latticed development listener

`latticed` now adapts the sans-I/O PostgreSQL session to TCP. The placeholder handler answers
`SELECT 1`; other SQL returns `0A000`. Simple and extended query framing are handled by the existing
session. This listener uses trust authentication and plaintext; it does not open a database or
implement TLS, real authentication or the driver matrix. CancelRequest is routed to a cooperative
query token; it does not forcibly interrupt blocking handlers or I/O.

```sh
cargo run -p latticed -- --listen 127.0.0.1:5432 --max-connections 32
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
