# latticed development listener

`latticed` now adapts the sans-I/O PostgreSQL session to TCP. The placeholder handler answers
`SELECT 1`; other SQL returns `0A000`. Simple and extended query framing are handled by the existing
session. This listener uses trust authentication and plaintext; it does not open a database or
implement TLS, real authentication, cancellation or the driver matrix.

```sh
cargo run -p latticed -- --listen 127.0.0.1:5432 --max-connections 32
```

The default address is `127.0.0.1:5432`. Read and write timeouts default to 30 seconds and 5 seconds;
set `--read-timeout-ms` and `--write-timeout-ms` to change them. A read timeout covers a complete
frame, including fragmented or slowly supplied headers and bodies. The existing codec's 1 MiB
message limit is checked before allocating a frame body. The connection count must be 1–1024;
connections above the configured count receive `53300` and close.

Type `shutdown` followed by a newline on stdin to stop cleanly. EOF on stdin also stops the process;
a supervisor must keep that control channel open while serving. This control mechanism does not
install signal handlers. Embedders can call `ServerHandle::shutdown`; dropping the handle also stops
the listener, interrupts active socket reads/writes and joins all connection workers.

Each accepted connection has one host thread. Protocol logic remains in `lattice-api`; socket I/O,
thread lifecycle and admission limits stay in this composition crate. Socket tests independently
encode their expected messages and cover fragmentation, pipelining, connection admission, timeouts,
malformed lengths and shutdown. No phase or driver-conformance gate is established by this adapter.
