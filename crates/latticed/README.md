# latticed development listener

`latticed` adapts the sans-I/O PostgreSQL session to TCP. Without `--data-dir` the placeholder
handler answers `SELECT 1` and other SQL returns `0A000`; with `--data-dir` it serves the
single-table SQL executor over a durable log (see below). Simple and extended query framing are handled by the existing
session. The default is explicit development trust on loopback. Configured SCRAM mode requires
TLS and refuses plaintext startup or trust fallback. The five-driver matrix is not established (#413). `tests/drivers/psql` runs psql against the real binary, and `tests/drivers/smoke.sh` runs node-postgres, pgx and pg8000 in text mode over loopback trust (the `driver-smoke` workflow; see the Phase 2 gate page). CancelRequest routes a cooperative token and cannot preempt blocking I/O.

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

Containers: the image's entrypoint runs `latticed --stdin-control off` with your arguments, and with
no arguments it prints the flags it needs and exits 2; the commands are in
[Run in a container](../../docs/alpha.md#run-in-a-container). Any other way of running `latticed` in a
container needs `--stdin-control off` passed yourself. Give `docker stop` more time than
`--stop-timeout-ms` (`docker stop -t 40`, or lower the flag below Docker's default of 10 seconds).
A process running as PID 1 only receives signals it has a handler for. `latticed` installs its SIGTERM
and SIGINT handlers after startup, so a stop sent during recovery is ignored and `docker stop` falls
back to SIGKILL. Trust mode refuses a non-loopback bind, so a container that publishes the port needs
`--auth scram-tls`.

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

### Provisioning with `lattice account`

`lattice account` (crate `lattice-cli`) writes the files below so that nobody has to compute a SCRAM
verifier by hand. It never takes a password or token as an argument: a password is read from stdin
(`--password-stdin`, one line of 12 to 256 printable ASCII characters) or generated, and is printed once.
Every file is created or replaced atomically with mode 0600 inside a directory that only the owner may
write; `init` refuses to overwrite an existing file. Keep the directory outside the data directory. Run one
command that changes the directory takes an exclusive lock on `DIR/.lock` and holds it across its read-modify-write, so concurrent runs queue and a token created during a revocation cannot undo it. It does not make TLS keys: use a
pinned `openssl` command for the certificate.

```sh
lattice account init /secure/lattice          # decoy secret, empty policy and token files; prints the flags
printf '%s\n' "$PASSWORD" | lattice account add /secure/lattice alice --read work_items,docs --write work_items --password-stdin
lattice account add /secure/lattice bob       # generated password, printed once; no grants (denied everything)
lattice account list /secure/lattice
lattice account token create /secure/lattice alice [--expires-days N] [--rotate [--overlap-hours N]]
lattice account token revoke /secure/lattice TOKEN_ID
lattice account remove /secure/lattice alice  # login, grants and bearer tokens
```

The flags it prints for `latticed`:

```sh
latticed --auth scram-tls --scram-verifiers /secure/lattice/scram-verifiers \
  --scram-decoy-secret /secure/lattice/scram-decoy-secret \
  --account-policies /secure/lattice/account-policies --tls-cert CERT.pem --tls-key KEY.pem
```

`--read`, `--write` and `--ddl` take comma-separated table names (`*` is every table) and become the
`tables:` line of `account-policies` (`alice<TAB>tables: docs=r, work_items=rw`); none means `deny-all`.
`scram-verifiers` appears with the first `account add` because `latticed` refuses an empty verifier file,
and is removed with the last account. `bearer-tokens` holds only salted hashes (ADR 0031); `latticed --gateway-tokens` reads it. A second active token needs `--rotate`, which ends the older one after
`--overlap-hours` (default 24, at most 168). `remove` revokes the account's tokens but the revoked lines
stay in the file so their ids are never reissued; `token prune [--older-than-days N]` (default 30) deletes revoked and expired entries older than N days, and `token create` prunes entries older than 30 days by itself when the file holds its maximum of 1024. A token past its expiry counts as expired, not active, and no longer blocks a new token. Windows is refused until the ACL check (#577) exists.

**What a running `latticed` sees.** A running `latticed` with `--gateway-tokens` re-reads the bearer-token file within 5 seconds of a change, so `token revoke` and `account remove` close the revoked tokens' live gateway connections without a restart (#577). The SCRAM verifiers, the decoy secret and the `account-policies` file are read when `latticed` starts only: after a changed password, an account add or an account remove, restart it. SCRAM verifiers use 4096 PBKDF2 iterations (ADR 0021), so a stolen `scram-verifiers` file is cheap to attack offline: keep it owner-only like a key and prefer the generated passwords. `--password-stdin` refuses a terminal because the typed password would echo; pipe it in. The tool wipes its own buffers for passwords, tokens and verifier text on a best-effort basis; what the terminal or the pipe does with the printed output is outside its control.

```sh
cargo run -p latticed -- --auth scram-tls --listen 127.0.0.1:5432 \
  --tls-cert /secure/server-chain.pem --tls-key /secure/server-key.pem \
  --scram-verifiers /secure/accounts.txt --scram-decoy-secret /secure/decoy-secret.bin \
  --startup-timeout-ms 10000
```

Provision those files with `lattice account` (below), or externally. The decoy-secret file is mandatory and must contain exactly 32
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
- Startup secrets are read through one private-file rule: `--tls-key`, `--scram-verifiers`,
  `--scram-decoy-secret` and `--account-policies`. Each must be a regular file with no group or other
  permission bits (mode 0600 or 0400; 0440 and 0444 are refused), in a directory that group and others
  cannot write to. The path is opened without following a symlink, so a symlink is refused (this includes
  a Kubernetes `..data` secret mount), and a FIFO is refused without blocking startup. The mode is checked
  on the open file, so the bytes read are the bytes checked. The certificate chain is public: it is only
  checked to be a regular file, and symlinks are followed for it.
  Limits: the owner is not checked. Secret mounts at 0440 or 0444 (common for orchestrator-mounted secrets)
  must be tightened first. On Windows these files are refused entirely, so `scram-tls` and TLS startup do
  not run there.
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
command tag. Recovery replays the log, so committed rows, tables and their column definitions (types,
`NOT NULL`, primary key, `UNIQUE` constraints, defaults) come back: `CREATE TABLE` writes the definition in
the same change set as the table (RFC 0012, issue 573), and a restarted server answers `SELECT` and
enforces the constraints exactly as before. The test that kills the process with SIGKILL shows the committed row's bytes are in the log files (they reached the operating system); that the log is `fsync`ed before the command tag is returned is the kernel's behaviour (its sync calls), not something that test proves. Process kill is not power loss.

**Upgrading an existing data directory.** A directory written by a build that did not store definitions
refuses its first `CREATE TABLE` with `55000` until the server is started with `--allow-format-upgrade on`.
The first definition adds a required capability to the log, so builds without definition support refuse the
directory afterwards and there is no downgrade: take a verified backup first. A new, empty directory needs
nothing. Tables that earlier builds created keep their names but have no stored definition, so SQL cannot
address them (`SELECT` answers `42P01`, `CREATE TABLE` of the name `42P07`); adopting them is not done.
A stored definition the executor cannot serve refuses startup. Column names with ASCII capitals (quoted
identifiers) are refused with `0A000` because the record format forbids them. The stored row layout is
not a frozen format.

Not wired yet: the five-driver matrix (libpq, psycopg, pgx, JDBC, node-postgres), binary result
formats. Text parameters of the extended protocol are tested. The tests in `tests/sql_binary.rs` use a plain client that frames
messages itself, over loopback trust and over verified TLS with SCRAM.

`load_credential_store` and `write_credential_store` read and replace the bearer-token credential file
([ADR 0031](../../docs/adr/0031-bearer-token-authentication.md)). The loader refuses a file that group or others
can access (anything but mode 0600 or 0400), a directory they can write to, a file over 256 KiB and any
malformed content, and on platforms without Unix mode bits it refuses every file until an ACL check exists
(#577). The writer creates a 0600 temporary file next to the target, syncs it, renames it over the target
and syncs the directory. `--gateway-tokens` loads the file at start and then watches it (#577): a changed file is
read at the next check, at most 1 second after the change (the library setting `credential_poll` is 1 ms to 5 s), and
a file that does not load keeps the tokens in use. The first failed load of a run of failures is reported on stderr, with the path and the
reason, never a token; a file that has not loaded for 10 minutes refuses every token until one loads.

## Sync gateway (WebSocket)

Experimental, pre-alpha. `latticed --data-dir DIR --development-policy allow-all --gateway-listen 127.0.0.1:5433`
serves the [sync protocol](../../formats/sync-protocol-v1.md) (rows only, read only) at `ws://ADDR/sync` with the
subprotocol `lattice.sync.v1`; each sync frame is one binary message, text is closed with 1003. The banner of the
server names the gateway address.

| Flag | Default | Meaning |
|---|---|---|
| `--gateway-listen IP:PORT` | off | Enables the gateway. Needs `--data-dir`. Plaintext only on a loopback address. A non-loopback address needs `--tls-cert`, `--tls-key` and `--gateway-tokens`, or latticed refuses to start (#578). |
| `--tls-cert FILE`, `--tls-key FILE` | none | With `--gateway-listen`, serve the gateway port over TLS. These are the same files and key rules (owner-only mode 0600 or 0400, a directory that group and others cannot write to, no symlink, bounded reads; see [Timeouts, close and cancel limits](#timeouts-close-and-cancel-limits)) as the PostgreSQL listener in `scram-tls`, and one certificate serves both. With `--auth development-trust` they are accepted only together with `--gateway-listen` and apply to the gateway alone; the PostgreSQL listener stays loopback trust in plaintext. In `scram-tls` the gateway always uses them. The handshake runs under `--gateway-handshake-timeout-ms` and holds a handshake place, so a stalled handshake is cut and frees its place. Plaintext clients get no HTTP reply on that port. |
| `--gateway-tokens FILE` | none | Require `Authorization: Bearer lat_...` from this credential file (mode 0600), re-read within 5 seconds of a change and on SIGHUP (SIGHUP is caught only when this flag is set). A file that does not load keeps the tokens in use. SCRAM verifiers and policies are not re-read. The connection is the token's account and its table grants (`--account-policies`, which this flag also allows with `--auth development-trust`). Without it the gateway serves the loopback development principal and only with `--auth development-trust`. |
| `--gateway-allowed-host NAME[:PORT]` | none | Adds a `Host` that `POST /mcp` answers to, besides this address and `localhost` on the bound port (which stay allowed). Repeatable, at most 16, needs `--gateway-listen`. Exact and case-insensitive: an ASCII DNS name or an IP literal (IPv6 in brackets), no wildcards, no IP ranges, no userinfo or path. Without a port it means the bound port. List only names you control that resolve to this gateway; with a name beyond `localhost`, use TLS and tokens, since a plaintext loopback bind is protected only by the `Origin` refusal against DNS rebinding. A malformed, repeated or default-equal value stops the start (#774). |
| `--gateway-max-connections N` | 32 | Connections at once, including upgrades in progress (at most 8 of those). The next one gets `503`. |
| `--gateway-handshake-timeout-ms N` | 5000 | One absolute budget from accept to the `101` response. |

Host and Origin. `/sync` checks that `Host` is present and well formed, but not its value, and refuses any browser
`Origin` with `403` (the default policy); `--gateway-allowed-host` does not change that. `POST /mcp` answers to the bound
address and to `localhost` on the bound port, and to each `--gateway-allowed-host` name (#774); any other `Host` gets
`403`, and any `Origin` gets `403`. A `Host` must match exactly, including its port. The `Host` check is the
DNS-rebinding defence, and a name is allowed only when it is listed.

The token goes in the `Authorization` header only: a request target with a query string gets `400`, two
`Authorization` headers get `401`, and a missing, malformed, unknown, revoked or expired token all get the same
`401`; an address with 10 failures in a minute gets `429`. A `Hello` `agent` name is a label and never changes who
the connection is. Reads wake on the executor's commit notifier and go through the same policy-checked feed as
the rest of the server; a fallback read every second means a lost wake costs latency only. The connection thread
looks at the wake between socket reads of up to 20 ms, so a wake adds up to that. A client that stops reading is
disconnected at the write budget; resume by subscribing again with the last acknowledged cursor. Not done here:
an idle deadline (#578), byte budgets per principal (#529) and the security event log (#530). Token reload,
revocation of live connections and expiry are done (#577): a revoked or expired token's connection is closed with
1008 after the change is read, and a token that expires is closed at its expiry (within the 5 second revalidation).
Rotation is the credential file's rule: two active tokens per account, the older ending at most seven days after the
newer was created. Not done: the Windows ACL check (the file is refused on Windows).

Notes on the reload (#577): SIGHUP through the real `latticed` process is not verified end to end; the evidence is the
`lattice-sys` counter test and the unit test with a bumped request count. The SIGHUP handler is installed after the
server and the gateway have started (`main.rs`, just before the startup banner), so a SIGHUP in that gap ends the process
with the default action. With `--gateway-tokens` set, the daemon no longer dies on terminal hangup, because SIGHUP is caught
after start. A header-only credential file parses as an empty store and revokes every token (fail-closed; the writer's
atomic rename makes a torn file unlikely, but truncating the file by hand revokes everything). The reload reads the file
on the gateway's accept thread (bounded, `O_NONBLOCK`), so a hung network filesystem holding the file would stall accepts;
keep the credential directory on local storage.


## MCP tool backend

`latticed::mcp_backend::McpBackend` is the transport-independent backend of the MCP tools `lattice_read`,
`lattice_write` (issue #526, part 1) and `lattice_subscribe` (issue #579: a long poll over the change feed, with the
12-byte cursor and a bounded `wait_ms`). It is created from an authenticated `Principal`, the account's row grant and
the host's feed and commit notifier, and runs every statement as that principal through the SQL executor, so the
table grants, the author id of a commit and the statement budgets are the ones a PostgreSQL connection of the account
gets. A table the principal may not see answers like a table that does not exist. Row text is returned under a fixed
`untrusted` member.

It is served over HTTP as `POST /mcp` on the sync gateway port (`--gateway-listen` with `--gateway-tokens`; issue
#527, part 1, with the Host, Origin and content-type defences of issue #580). Each request is verified against the
bearer token and served as that token's principal, from `ws/mcp_http.rs`. `latticed` has no stdio transport: the
`lattice mcp` command in `lattice-cli` is a stdio proxy to this endpoint, for loopback `http://` URLs only (see
[crates/lattice-cli/README.md](../lattice-cli/README.md)). See [docs/mcp-v0.md](../../docs/mcp-v0.md#http-transport).
