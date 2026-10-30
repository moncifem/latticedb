# latticed development listener

`latticed` adapts the sans-I/O PostgreSQL session to TCP. Without `--data-dir` the placeholder
handler answers `SELECT 1` and other SQL returns `0A000`; with `--data-dir` it serves the
single-table SQL executor over a durable log (see below). Simple and extended query framing are handled by the existing
session. The default is explicit development trust on loopback. Configured SCRAM mode requires
TLS and refuses plaintext startup or trust fallback. The five-driver matrix is not established (#413). CancelRequest routes a cooperative token and cannot preempt blocking I/O.

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

Stop the process with SIGTERM or SIGINT (Ctrl-C), or, by default, by typing `shutdown` and a newline
on stdin; see [Running as a service](#running-as-a-service). Embedders can call
`ServerHandle::shutdown`; dropping the handle also stops the listener, interrupts active socket
reads/writes and joins all connection workers.

Each accepted connection has one host thread. Protocol logic remains in `lattice-api`; socket I/O,
thread lifecycle and admission limits stay in this composition crate. Socket tests independently
encode their expected messages and cover fragmentation, pipelining, connection admission, timeouts,
malformed lengths and shutdown. No phase or driver-conformance gate is established by this adapter.

## Running as a service

| Flag | Default | Meaning |
|---|---|---|
| `--stdin-control on\|off` | `on` | `on`: the line `shutdown` on stdin stops the server, and so does end-of-file. `off`: stdin is never read; a closed pipe or `/dev/null` changes nothing. |
| `--stop-timeout-ms N` | `30000` | Upper bound on a graceful stop. When it elapses the stop is abandoned and the process exits with status 2. |

SIGTERM and SIGINT start the same clean stop as `shutdown`, in both stdin modes: the listener closes,
connections are interrupted and joined, the database thread ends and the data-directory lock is
released, so the next start recovers the log without any repair. A second signal while that stop is
running, or the stop timeout, abandons it. The log is crash-safe, so abandoning a stop loses no
acknowledged commit; it is what a SIGKILL would have done, and the next start recovers as after a
crash. Exit status: `0` clean stop, `1` startup or configuration error or a failed stop, `2` forced
stop.

**Which mode.** Default `on` keeps the interactive and script uses working (`echo shutdown |
latticed ...`, a FIFO held open, the README quickstart). Anything that runs the server detached must
pass `--stdin-control off`: `docker run -d` and `systemd` give a process `/dev/null` or a closed
pipe, which `on` reads as end-of-file and stops at once (the server prints a hint when that
happens). `off` is deliberately not the default, so a forgotten flag fails loudly instead of leaving
a server that cannot be stopped from a terminal pipe.

Signal handlers are installed after startup finishes and before the `latticed listening on` line is
printed. A signal during recovery keeps its default action and kills the process (recovery is
crash-safe). Wait for the listening line before signalling for a clean stop. The handler only
increments an atomic counter; it lives in `lattice-sys`, the one crate that may contain `unsafe`,
declares `signal(2)` itself and adds no dependency.

The unit and plist below were written from the systemd and launchd documentation and have not been
run under those supervisors; the real-binary tests cover the signal and stdin behaviour they rely on.

systemd (`/etc/systemd/system/lattice.service`). Set `TimeoutStopSec` above `--stop-timeout-ms`.
Trust mode is for loopback development; use `--auth scram-tls` with the files described below for
anything reachable from other hosts:

```ini
[Unit]
Description=Lattice database
After=network.target

[Service]
Type=simple
DynamicUser=yes
StateDirectory=lattice
ExecStart=/usr/local/bin/latticed --listen 127.0.0.1:5432 --data-dir /var/lib/lattice \
  --development-policy allow-all --stdin-control off --stop-timeout-ms 30000
KillSignal=SIGTERM
TimeoutStopSec=40
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

launchd (`~/Library/LaunchAgents/dev.lattice.latticed.plist`; launchd sends SIGTERM and kills after
`ExitTimeOut` seconds):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.lattice.latticed</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/latticed</string>
    <string>--listen</string><string>127.0.0.1:5432</string>
    <string>--data-dir</string><string>/usr/local/var/lattice</string>
    <string>--development-policy</string><string>allow-all</string>
    <string>--stdin-control</string><string>off</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ExitTimeOut</key><integer>40</integer>
</dict>
</plist>
```

Containers: when the image exists (#587), its entrypoint passes `--stdin-control off`; with any other
way of running `latticed` in a container, pass it yourself. Give `docker stop` more time than
`--stop-timeout-ms` (`docker stop -t 40`, or lower the flag below Docker's default of 10 seconds).
A process running as PID 1 only receives signals it has a handler for, and `latticed` installs one
for SIGTERM and SIGINT. Trust mode refuses a non-loopback bind, so a container that publishes the
port needs `--auth scram-tls`.

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

A suspended portal keeps its remaining rows once and sends them from a cursor; the rows and the
statement text and parameters of every open portal are charged to a 64 MiB per-connection budget
(a soft cap: `53200` before a statement runs once the connection is over it, never after; at most 128 portals, `COMMIT`/`ROLLBACK` destroys them). The budget is separate
from `--statement-memory-bytes`, so a connection can hold up to 64 MiB of retained results after its
statements finished. Portals opened outside a transaction block are not destroyed at `Sync`.

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

# SCRAM over TLS: one 'account<TAB>policy' per line (allow-all, deny-all or tables: ...); an unlisted account is denied
cargo run -p latticed -- --auth scram-tls --data-dir /var/lib/lattice --account-policies /secure/policies.txt \
  --tls-cert ... --tls-key ... --scram-verifiers ... --scram-decoy-secret ...
```

Without `--data-dir` the `SELECT 1` placeholder is unchanged. With it, `latticed` opens the write-ahead
log in the directory (created with mode 0700 when missing; an existing one is refused if group or others have any access, which is a mode check only: the owner is not checked and symlinks are followed, so it is not a guarantee against a local attacker; one server at a time, a second one is
refused), replays it, then serves the single-table SQL executor (`lattice-exec`) to every admitted
connection. Startup fails before binding if the log cannot be recovered or the policy options are
inconsistent: `--development-policy` is required in trust mode and rejected in SCRAM mode, and
`--account-policies` is required in SCRAM mode and rejected in trust mode. There is no allow-all
default for anyone. `deny-all` refuses `CREATE TABLE` and `DROP TABLE` (`42501`) before looking at any name, and it also hides the catalog: for that principal every table reference in `SELECT`, `INSERT`, `UPDATE`, `DELETE` and in `Parse`/`Describe` is `42P01` with the same message as for a table that does not exist, so a denied account cannot learn table or column names (it never sees `42703` or `42501` on DML). The account policy file and the key and decoy files must be regular files with no group or other permission bits, and a data directory that already exists must be mode 0700 (a new one is created 0700). Besides `allow-all` and
`deny-all`, an account line can carry per-table grants (below). Nothing else exists until the Phase 4
policy language lands.

**Per-table grants (interim).** `account<TAB>tables: notes=rw, logs=r, *=r` gives the account the listed bits on
the listed tables: `r` reads (and makes the table visible), `w` writes (`INSERT`, `UPDATE`, `DELETE`), `d` changes
the catalog for that table (`CREATE`, `DROP`, rename). Bits go in any order; `-` means none. `*` stands for every
table, including tables created later. The most specific entry decides and entries are not merged: `notes=r, *=rw`
leaves `notes` read-only and `secret=-` carves a table out of a wider `*`. Names match exactly (case-sensitive, no
prefix or pattern); a grant follows the name, so a dropped and re-created table keeps it. `UPDATE` and `DELETE`
need `r` and `w`, and `RETURNING` needs `r`. `CREATE TABLE x` needs `d` on `x` (or on `*`); `DROP TABLE` needs
`d` on that table (or on `*`, which is also what `IF EXISTS` on a table that does not exist needs). `d` does not
imply `r` or `w`, but a `d` holder can build an index over rows it cannot read. A table the account holds no bit on
is invisible: `42P01` with the missing-table message in `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `Parse` and
`Describe`, and `UnknownScope` for its change feed (`table/<name>` needs `r`, so a write-only or ddl-only table is
not served either, and a bad cursor on a hidden table gets the same answer as on a missing one). A visible table
with a missing bit answers `42501` before any row is touched, whether or not rows exist. The file is strict and
fails startup on: an empty list, an entry that is not `name=bits`, a repeated table or `*`, a name with a space,
control character, `=`, `,` or `*` inside it, bits other than `r`, `w`, `d` (each at most once) or `-`, more than 256
entries per account, a repeated account, or a file over 64 KiB; the error names the line, never the content.
An account that is not in the file is `deny-all`. There are no row predicates, roles, groups or expressions, and
no `presence` bit (presence is not in the alpha, issue #626). A transaction resolves names to tables when it
begins, so a table created by another connection after a block began reads as empty inside that block (its rows are
denied, no error) until the next transaction. DDL decides by name: an account without `d` on a name gets `42501` for
`DROP`, `DROP IF EXISTS` and `CREATE` of it whether or not the table exists, so a carved-out table's existence does not
show. `d` covers a table's name only; the internal entry tables behind `UNIQUE` columns follow the owner for rows
(`r`, `w`) and are not a ddl target. **Known limit of `w` without `r`:** as with an INSERT-only privilege in
PostgreSQL, an `INSERT` of a key that already exists answers `23505` naming the primary key or `UNIQUE` constraint,
so such an account can probe key values of a table it cannot read (pinned by
`a_write_only_account_can_probe_unique_keys_of_a_table_it_cannot_read`). Tables are
named by catalog name, so a table created through a quoted identifier with spaces or punctuation can only be
covered by `*`.

**One database thread, and what bounds a wait.** The executor is single-threaded, so one thread owns the
database (`lattice_api::exec_host`) and each connection forwards its statements to it; a connection has
at most one request in flight. Statements therefore run one at a time. `--statement-timeout-max-ms`
(default 30000) is both the default statement timeout and the ceiling a client's `SET statement_timeout`
cannot lift; `--statement-memory-bytes` (default 64 MiB) bounds the rows one statement holds; `--transaction-bytes` (default 256 MiB) caps the writes one
transaction stages (the write that crosses it is `53200` and fails the block).

A connection waits for its answer at most the ceiling plus 5 seconds, then gets `57014` and its statement
is cancelled: the database thread skips it if it has not started and stops it at the next row if it has.
So the delay a client can see is bounded by that, not by the number of connections, but the database
thread's own backlog is not: a statement queued behind others can time out before it starts, and with
`--max-connections` busy connections the thread can spend up to `max-connections x ceiling` on work
whose clients already left. Reasons the bound is not tighter:

- a client that disconnects mid-statement does not stop it (the connection worker is blocked waiting
  for the answer, so it only notices at the next read); a statement that polls ends at the ceiling, but
  see the uninterruptible steps below;
- the ceiling is checked between rows. Materialising the table's rows before a scan (`scan_at`) and the
  comparison pass of a sort are not interrupted and not counted, so a very large table can exceed the
  ceiling by the time of those steps;
- a CancelRequest reaches a running statement at the same between-row points;
- after the statement is dequeued, `COMMIT` and `CREATE TABLE` / `DROP TABLE` do not poll for
  cancellation at all, and an autocommit statement's commit is not interruptible once the last row was
  produced. So a database thread busy with these runs past the ceiling for as long as they take.

**A `57014` after the ceiling means "outcome unknown".** When a connection gives up waiting, the
statement may still have run or may run later: an autocommit `INSERT`/`UPDATE`/`DELETE`, a `COMMIT`
and DDL can have committed even though the client was told `57014`. Treat it like a lost connection:
read back before retrying. A statement that is skipped because it had not started changed nothing.
Inside a transaction block the timeout fails the block on the database side too, so a later `COMMIT`
answers `ROLLBACK` and nothing of the block is committed. The same holds when `Parse`/`Describe` fails or
times out inside a block. A `BEGIN` that times out leaves no block behind: if it ran late, the database
side discards the block it opened, because the client believes it is outside one. The abort is not
a queue entry: the connection records it in state it shares with the database thread, which applies it
before it runs the connection's next request. An error the
wire session raises itself, which never reaches the executor (a `CancelRequest` that arrives before
dispatch, a `Bind` with binary formats, an unknown portal or statement, a duplicate or excess `Parse`, the
`55P03` of `describe`), also fails the block on the database side: the session tells its handler
(`QueryHandler::abort_transaction`, #419), so a later `COMMIT` answers `ROLLBACK` and nothing of the block
is committed, as in PostgreSQL. The only wire error not covered is `28000` (a changed principal), which a
session cannot produce.

**The request queue is bounded and no send blocks.** A connection has at most one live request in the
queue: a request that timed out stays queued until the database thread takes it, so the connection's
next statement first waits for that one to leave. The wait for that, for room in the queue, and for the
answer share one budget of the ceiling plus 5 seconds, counted in waits that ran out, so it holds unless
the database thread keeps making progress. If the database thread is stuck (a `COMMIT`, DDL
or a scan materialisation that does not poll) a statement gets `57014` after the budget when it was
queued, or `53300` when it never got in, and the block is failed on the database side as for any
other error. Queue memory is bounded by `queue_depth` (`--max-connections` plus the control reserve
plus 16). It is not bounded by the connection count while the thread is stuck: stale entries of timed-out
requests, including those of connections that have since closed, stay queued until the thread takes
them, so when clients time out and reconnect the queue can fill, and a request that finds it full gets
`53300`. A connection thread's wait is bounded unless the database thread keeps taking other
connections' requests (a wait ended by that signal costs no budget). The abort is deferred: an idle client that got
`57014` keeps its block open on the database side until its next request or until it closes (the
block is then abandoned). If the database thread has died, a waiting statement gets `57P01`. Closing a connection is not a request
either: the database thread removes the sessions of closed connections the next time it takes a
request, and one wake-up request at most is queued to make it look sooner. While the thread is stalled,
the list of closed connections grows by one 8-byte id per closed connection that ran SQL, and is emptied
when the thread next takes a request. Measured by unit tests with
a stalled thread (24 clients retrying, a queue of 4 with 20 clients), not by a load test.

Shutdown stops the listener, which cancels every registered statement. Before it joins the connection
threads it tells the waiting ones to give up (`57P01`), so a stuck database thread does not hold them.
Requests still queued are dropped unanswered, which the connections report as `57P01`, and the
database thread leaves at its next request. It waits for at most the statement that is running (itself
bounded as above) and at most `shutdown_grace_ms` (10 s): past that `shutdown` returns an error that
says the SQL thread did not stop and leaves it behind, so a supervisor sees the process end. A connection that
drops mid-transaction commits nothing.

**If the database thread dies.** A panic on that thread makes every later statement answer `57P01`
(connections stay up), `SqlServer::is_finished` turns true, and `latticed` exits non-zero with
`the SQL thread panicked`; the directory lock is released with the thread. The exit code path is tested
at library level (a planted failure); the process-level exit is the supervisor loop in `main.rs`.
The planted failure and the stall hooks used by those tests live behind the `test-hooks` feature of
`lattice-api`, which only `latticed`'s dev-dependencies enable: `cargo tree -p latticed -e normal,features
-i lattice-api` lists no `test-hooks` (it does with `-e dev`), so a release build does not contain them.

**What survives a restart or a crash.** Every committed statement is on disk when the client sees the
command tag. Recovery replays the log, so committed rows and table names come back. **Column
definitions do not** (#379): after a restart the recovered tables exist only by name, `SELECT` on one
answers `42P01`, and `CREATE TABLE` of the same name answers `42P07`. The test that kills the process with SIGKILL shows the committed row's bytes are in the log files (they reached the operating system); that the log is `fsync`ed before the command tag is returned is the kernel's behaviour (its sync calls), not something that test proves. The server prints
`N recovered table(s) have no column definitions` on stderr at startup. Until the schema is persisted
(an RFC and an ADR, then a guarded log record), a data directory is useful for tables created in the
current run only, and a new table name is needed after each restart. The stored row layout is not a
frozen format.

Not wired yet: the five-driver matrix (libpq, psycopg, pgx, JDBC, node-postgres), binary result
formats, and a persisted schema. Text parameters of the extended protocol are tested. The tests in `tests/sql_binary.rs` use a plain client that frames
messages itself, over loopback trust and over verified TLS with SCRAM.
