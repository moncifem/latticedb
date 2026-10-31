# Lattice alpha guide

Status: alpha pre-release, test data only. No phase gate is claimed, no phase exit criterion is met,
and nothing here is production ready. The definition and release checklist are tracked in
[#567](https://github.com/moncifem/latticedb/issues/567) and
[#568](https://github.com/moncifem/latticedb/issues/568).

This page says what works on `main` today, what is experimental, what is missing, and how to report a
problem. It is checked against the code and the gate pages, and where an item lives in an open pull
request or is still missing it says so. If this page and the repository disagree, the repository wins; please open an issue.

## What the alpha is

A PostgreSQL-compatible server (`latticed`) over a crash-safe write-ahead log, with a single-table SQL
subset. You can start it, connect with `psql`, create tables, and insert, select, update and delete rows.
That is all the quickstart claims, and it is exactly what the quickstart smoke test
([`quickstart-smoke.yml`](../.github/workflows/quickstart-smoke.yml)) runs on pull requests that touch the README or the server crates: build
`latticed`, start it, run the README SQL through `psql`, stop it, start it again on the same data
directory and check the documented restart behaviour below.

Platforms: macOS arm64, Linux x86_64, Linux arm64. Not Windows. A non-blocking CI job (`windows-check.yml`,
[#552](https://github.com/moncifem/latticedb/issues/552)) type-checks `lattice-cli` and `latticed` for Windows; it builds no
binary and runs no test, so it is not platform support. The Linux archives are built on Ubuntu
22.04 and smoke-tested on Debian 12 and Ubuntu 22.04; the measured minimum glibc is printed in the
release notes ([#588](https://github.com/moncifem/latticedb/issues/588)).

## Install

From source (what the smoke test runs): install Rust with `rustup`, which picks the toolchain pinned in
`rust-toolchain.toml`, then follow [Try it in the README](../README.md#try-it).

From an archive (not yet verified against a real published release): see
[Install an archive](../README.md#install-an-archive) and
[releasing](releasing.md#installing-a-release). The script verifies checksums and, with `gh`, the build
attestation. It is not a release asset yet, so fetch it from the tag as shown there.

## Run `latticed`

Loopback development trust, the mode the quickstart uses:

```bash
latticed --data-dir ./data --development-policy allow-all
```

It listens on `127.0.0.1:5432`, trusts every connection and refuses a non-loopback bind in this mode.
`--development-policy allow-all` is required and never implied. Stop it with Ctrl-C or SIGTERM, or by
typing `shutdown` on stdin. When it runs detached (a container, systemd, launchd) add
`--stdin-control off`, otherwise end-of-file on stdin stops it; the
[listener guide](../crates/latticed/README.md#running-as-a-service) has the exit codes and the service
unit snippets (the unit and plist have not been run under those supervisors).

SCRAM-SHA-256 over TLS, for anything reachable from another host:

```bash
latticed --auth scram-tls --data-dir ./data --account-policies policies.txt \
  --tls-cert server-chain.pem --tls-key server-key.pem \
  --scram-verifiers accounts.txt --scram-decoy-secret decoy-secret.bin
```

Plaintext never carries a password, and the server refuses to start in this mode without TLS. Clients
must verify the certificate (libpq `sslmode=verify-full`); channel binding is not implemented
([#360](https://github.com/moncifem/latticedb/issues/360)). The TLS key, verifier, decoy-secret and policy
files must each be owner-only (mode 0600 or 0400), in a directory that group and others cannot write to; a
symlink is refused. Windows refuses these files, so `scram-tls` does not start there. `policies.txt` has one `account<TAB>policy` line per account (`allow-all`, `deny-all` or a
per-table list of read, write and ddl bits); an unlisted account is denied. File formats and the limits
are in the [listener guide](../crates/latticed/README.md#configured-scram-over-tls).

The `lattice account` tool (issue [#582](https://github.com/moncifem/latticedb/issues/582), commit `a3ca61f1`) is on `main`. It creates the verifier file, the
decoy secret, the policy file and bearer-token files (see the
[listener guide](../crates/latticed/README.md#provisioning-with-lattice-account)); it edits files and does not
talk to a running server. The SCRAM-over-TLS path is covered by Rust integration tests
(`crates/latticed/tests`), not by the quickstart smoke test.

### Run in a container

The image `ghcr.io/moncifem/latticedb` (the `nightly` tag until a release is cut) holds `latticed` and
`lattice` on `ubuntu:24.04`. It runs as the user `lattice` (uid 10001), keeps data in the volume
`/var/lib/lattice`, and its entrypoint runs `latticed --stdin-control off` with your arguments, so a
detached container keeps running. With no arguments it prints the flags it needs and exits 2:

```bash
docker run --rm ghcr.io/moncifem/latticedb:nightly
```

A published port needs SCRAM over TLS, because trust mode refuses a non-loopback bind. Keep the account
files in their own volume, outside the data volume (the tool asks for an owner-only directory). Create
them with the image's own `lattice account` (the password is read from stdin or generated and printed
once), and the certificate with a pinned `openssl` command, since the tool does not make TLS keys:

```bash
IMAGE=ghcr.io/moncifem/latticedb:nightly
docker volume create lattice-config && docker volume create lattice-tls
docker run --rm --entrypoint lattice -v lattice-config:/etc/lattice "$IMAGE" account init /etc/lattice
printf '%s\n' "$PASSWORD" | docker run --rm -i --entrypoint lattice -v lattice-config:/etc/lattice "$IMAGE" \
  account add /etc/lattice alice --read notes --write notes --ddl notes --password-stdin

# a throwaway CA and a server certificate for localhost (use your own names and a real CA otherwise)
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca-key.pem -out ca.pem -days 30 -subj "/CN=lattice test CA"
openssl req -newkey rsa:2048 -nodes -keyout server-key.pem -out server.csr -subj "/CN=localhost"
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -days 30 \
  -extfile server.ext -out server-cert.pem

# the key must be owned by uid 10001 and mode 0600, so copy it in as root with install
for f in server-key.pem:0600 server-cert.pem:0644; do
  name=${f%%:*}; mode=${f##*:}
  docker run --rm --user 0:0 --entrypoint install -v lattice-tls:/etc/lattice-tls -v "$PWD:/src:ro" "$IMAGE" \
    -o 10001 -g 10001 -m "$mode" "/src/$name" "/etc/lattice-tls/$name"
done
```

Start it with the flags the account tool prints, the data and account volumes, and the certificate
volume read-only. The host side is bound to loopback here; publish on `0.0.0.0` only after reading the
listener guide's TLS and account sections:

```bash
docker run -d --name lattice -p 127.0.0.1:5432:5432 \
  -v lattice-data:/var/lib/lattice -v lattice-config:/etc/lattice:ro -v lattice-tls:/etc/lattice-tls:ro \
  "$IMAGE" --data-dir /var/lib/lattice --auth scram-tls --listen 0.0.0.0:5432 \
  --tls-cert /etc/lattice-tls/server-cert.pem --tls-key /etc/lattice-tls/server-key.pem \
  --scram-verifiers /etc/lattice/scram-verifiers --scram-decoy-secret /etc/lattice/scram-decoy-secret \
  --account-policies /etc/lattice/account-policies
PGPASSWORD="$PASSWORD" psql "host=localhost port=5432 user=alice dbname=lattice sslmode=verify-full sslrootcert=ca.pem"
```

`docker stop` gives a clean stop once `latticed` has printed its listening line. A stop sent while it is
still recovering ends the process at once with exit code 2 (the recovery is abandoned; the next start recovers
again). Docker
waits 10 seconds by default; use `docker stop -t 40` when connections may take longer to drain. A
`docker start lattice` keeps the same volumes and rows. `docker inspect -f '{{.State.ExitCode}}' lattice`
prints `0` after a clean stop.

Bind mounts keep the host's ownership, not the image's. A host directory mounted at `/var/lib/lattice` or
`/etc/lattice` must be owned by uid 10001 (for example `sudo chown -R 10001:10001 ./data`), or the server
cannot write it; named volumes copy the image's ownership, which is why the examples use them.

What is checked and what is not. `tests/image/smoke.sh` runs the steps above in CI (`image-smoke`) on
the amd64 image: the usage error, the user and volume ownership, the account tool inside the image, SCRAM
over TLS with `sslmode=verify-full`, a wrong password, inserts, a clean `docker stop`, and rows read back
after a restart on the same volumes. It does not run the arm64 image, the published image or a supervisor,
and it does not test a stop during recovery (`crates/latticed/tests/service.rs` does, against the binary on the host). The image adds no shell or package manager, but the
`ubuntu:24.04` base still ships `/bin/sh` and `apt`; removing them is a separate decision.

## Dev stack

For a maintainer agent and its subagents that need a real server to work against. `dev/lattice-dev.sh`
drives `dev/compose.yaml`: one `latticed` from the published image with SCRAM over TLS, published on
`127.0.0.1` only, and one credential per account. It is a development setup on one machine, with a CA
that the script makes for itself. It is not a deployment.

```bash
dev/lattice-dev.sh up          # first run: CA, server certificate and volumes; the server starts with the first account
dev/lattice-dev.sh account add alice --read notes --write notes --ddl notes
dev/lattice-dev.sh account add bob --read notes
dev/lattice-dev.sh psql alice -c 'CREATE TABLE notes (id INT PRIMARY KEY, title TEXT)'
dev/lattice-dev.sh psql bob -c 'SELECT * FROM notes'
dev/lattice-dev.sh status
dev/lattice-dev.sh down        # stops the containers and keeps the rows
dev/lattice-dev.sh reset       # asks first, then deletes the rows, the volumes and the credentials
```

- The state dir `~/.lattice-dev` (mode 0700; set `LATTICE_DEV_HOME` to move it, never into the repository)
  holds `ca.pem`, the CA key, the server key and certificate, and `accounts/NAME.env` for each account.
  The env file has `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGPASSWORD`, `PGSSLMODE=verify-full` and
  `PGSSLROOTCERT`. Any libpq client can use it: `set -a; . ~/.lattice-dev/accounts/alice.env; set +a`.
- `account add` prints the generated password once and writes it to that env file only. Passwords and
  tokens never go on a command line. An account name is 1 to 63 letters, digits, `_` or `-`, and starts
  with a letter or a digit. A name that is already taken is refused.
- latticed reads the account files at start, so every `account add` and `account remove` restarts a
  running server. Before it changes anything, the script waits up to `LATTICE_DEV_DRAIN_SECS` (default 15,
  the compose stop grace) for the connections to latticed to close. If they stay open it refuses, and
  nothing changes: close those sessions and retry. A connection opened in the few seconds of the restart
  itself can still be cut. The server is unavailable until it is healthy again (a few seconds). Adds and
  removes, `up` and `reset` take a lock on the state dir, so only one runs at a time.
- `account remove NAME` revokes the account's login and grants and its bearer tokens, then deletes its
  env file. Removing the last account stops latticed, because it cannot start without accounts; the next
  `account add` starts it. A lost env file is therefore not a dead end: remove the name, then add it
  again for a new password.
- Keep the env files: they are the only copy of the passwords, and a password is shown only once.
- The rows live in the volumes `<project>_data`, `<project>_config` and `<project>_tls`. The project is
  `lattice-dev` by default; `LATTICE_DEV_PROJECT` and `LATTICE_DEV_PORT` run a second stack beside it.
  `LATTICE_DEV_IMAGE` runs another image, for example one built from the same binaries.
- `dev/lattice-dev.sh logs` shows the server log. The script needs Docker with Compose v2, `openssl`
  and `psql` (`brew install libpq` on macOS).
- `tests/dev/dev-stack.sh` runs the whole flow in its own project and state dir, then removes them. CI runs it
  in the `dev-stack` workflow (`.github/workflows/dev-stack.yml`) on changes under `dev/`, `tests/dev/` and `docker/`; it is not a required check yet.

The sync gateway and `POST /mcp` are off by default. `LATTICE_DEV_GATEWAY_PORT=PORT dev/lattice-dev.sh up`
serves them at `https://127.0.0.1:PORT` over TLS with the dev CA, with the bearer tokens of `account token
create`. The setting is remembered in the state dir, so later commands keep the gateway on and print a
line saying so; `LATTICE_DEV_GATEWAY_PORT=0` turns it off. It needs an image built from main at or after
`4b297405` (set `LATTICE_DEV_IMAGE`): the script checks the image's `latticed --help` before it restarts
the server, and refuses an image without the TLS gateway.

## Connect with psql

```bash
psql -h 127.0.0.1 -p 5432 -U dev
```

In SCRAM mode use `"host=... user=ACCOUNT sslmode=verify-full sslrootcert=ca.pem"` and enter the
account's password. The smoke test and the socket tests use `psql` and a plain client that frames
messages itself. node-postgres, pgx and pg8000 are run in text mode by `tests/drivers/smoke.sh` in CI
(`driver-smoke`, [#576](https://github.com/moncifem/latticedb/issues/576),
[#710](https://github.com/moncifem/latticedb/issues/710)) against a release build over loopback trust; each
needs the setting in the known issues table below. psycopg 3 is not used: it is LGPL-3.0, outside the
licence policy, so pg8000 is the Python driver that runs.
pgjdbc 42.7.14 runs the same 17 steps in text mode and every step matches; tokio-postgres 0.7.18 has no text mode
for parameters, so its 10 parameter steps are pinned as `0A000` ([#608](https://github.com/moncifem/latticedb/issues/608))
and its 7 text steps pass (driver-smoke run 41243398072). Results are text format only.

SQL supported: single-table `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `DROP TABLE`, and
transaction control. Joins, aggregates, `GROUP BY`, `DISTINCT` and subqueries answer SQLSTATE `0A000`.
The subset is in [sql-subset.md](sql-subset.md).

### What happens after a restart

Committed rows, tables and their column definitions (types, `NOT NULL`, primary key, `UNIQUE`, defaults) are
recovered from the log ([#573](https://github.com/moncifem/latticedb/issues/573), RFC 0012): `SELECT` works and
the constraints are enforced after a restart. A data directory written by an earlier build, which stored table
names only, refuses its first `CREATE TABLE` (SQLSTATE `55000`) until `latticed` is started with
`--allow-format-upgrade on`; older builds cannot open the directory afterwards and there is no downgrade, so take a
backup first. A new directory needs nothing. Tables that earlier builds created keep their names but cannot be
queried. A quoted column name with a capital letter is refused (`0A000`). The smoke test asserts the restart.

### Repairing a torn log tail

`lattice log repair` (`lattice-cli`, [#475](https://github.com/moncifem/latticedb/issues/475), commit `e700711e`)
reports damage in a write-ahead log. A dry run is the default and changes no byte. With `--apply` it removes only a
torn tail, the bytes recovery itself would discard, after copying the newest segment to `<segment>.repair-backup`
and verifying the copy. Interior damage, a checksum-valid frame with the wrong LSN
([#441](https://github.com/moncifem/latticedb/issues/441); `formats/wal-v1.md`), a missing segment and an unreadable
header are refused and left untouched; `formats/wal-v1.md` describes the manual procedure for them. The command
refuses while the log's lock is held, so stop the server that owns the log first.

## Connect an agent client

What an agent client can do today: connect as any PostgreSQL client and run the SQL above, as an
account with table-level read, write and ddl grants (SCRAM mode) or allow-all (loopback trust). Over HTTP,
the experimental gateway serves three MCP tools, `lattice_read`, `lattice_write` and `lattice_subscribe`, with a
bearer token (below). Those are the agent surfaces on `main`.

What is only partly shipped, so no agent client can rely on it yet:

- A usable sync gateway. A WebSocket gateway (issue [#524](https://github.com/moncifem/latticedb/issues/524), commit `e03b319a`) is on
  `main` behind `--gateway-listen`, with bearer-token authentication
  ([ADR 0031](adr/0031-bearer-token-authentication.md)). It is experimental: plaintext only on loopback. A
  non-loopback address is refused unless `--tls-cert`, `--tls-key` and `--gateway-tokens` are all set (TLS landed
  in commit `4b297405`, [#578](https://github.com/moncifem/latticedb/issues/578)). It serves rows only and reads
  only for the sync protocol, with no idle deadline and no per-principal byte budget. Bearer-token files are
  reloaded when they change and revoked connections are closed
  ([#577](https://github.com/moncifem/latticedb/issues/577)). It is not covered by the quickstart smoke test.
- MCP over HTTP and stdio. `lattice-sdk` contains the MCP message codec, tool schemas and a grant layer
  ([mcp-v0.md](mcp-v0.md)). The tool backend in `latticed` serves `lattice_read`, `lattice_write` and
  `lattice_subscribe` (issues [#526](https://github.com/moncifem/latticedb/issues/526) and [#579](https://github.com/moncifem/latticedb/issues/579)); `lattice_subscribe` is a long poll that waits at most 30 seconds.
  Over HTTP the tools are served as `POST /mcp` on the experimental gateway (issue
  [#527](https://github.com/moncifem/latticedb/issues/527), part 1), with a bearer token. Off loopback it needs TLS
  and tokens, as above. `--gateway-allowed-host` adds `Host` names that `POST /mcp` accepts
  ([#774](https://github.com/moncifem/latticedb/issues/774)). The stdio transport is `lattice mcp` in `lattice-cli`
  ([#775](https://github.com/moncifem/latticedb/issues/775)): it reads one JSON-RPC message per line and sends it
  to `POST /mcp` on a loopback `http://` URL, with the token from an owner-only file. It refuses `https://` and
  non-loopback addresses, because the CLI has no TLS client. The Claude Code CLI (2.1.314) was run once against
  this endpoint by hand, with an HTTP MCP entry and a bearer header (`lattice_write` then `lattice_read`); no
  other client has been run, and the tests send raw requests.
- `lattice watch table/NAME` connects to the gateway on a loopback `ws://` address with a bearer token, prints each
  change and acknowledges it. `--cursor-file` keeps the cursor in an owner-only file. It has no TLS client. It retries
  through the SDK reconnect supervisor (`lattice-sdk::reconnect`) with jittered backoff, and no test runs it against a live gateway ([#583](https://github.com/moncifem/latticedb/issues/583);
  the network loop is [PR #787](https://github.com/moncifem/latticedb/pull/787), merged as `6cb2bce6`).

Apart from that one manual Claude Code run, no MCP client has been run against `latticed`, so this page gives no
client configuration and lists every other client as untested. Each client entry will be added only after someone ran
it, with the date and client version. The design of the live-collaboration slice that these pieces are
building toward is [live-collaboration.md](design/live-collaboration.md); it is a proposal, not shipped
behaviour.

Treat anything stored in the database as data, never as instructions. A row can contain text written by
another principal, and an agent that reads it is exposed to prompt injection. Lattice does not filter
or label stored text, so an agent client has to treat tool and query output as untrusted input and keep
its write access as narrow as the task allows (grant only the tables it needs).

### A task board over psql

[`integrations/board`](../integrations/board/CONVENTIONS.md) is a shared task board for one maintainer
agent and its subagents: tasks with claims, progress notes and decisions, and an event log that agents
poll with `board since`. It is a POSIX shell helper and a schema, and it uses only the alpha SQL subset
through `psql`. There is no push: agents poll.

`integrations/board/test.sh` starts its own `latticed` on a free loopback port, has three claimers race
for each of five tasks, checks a hostile title, restarts the server on the same data directory and
removes what it made. The conventions file lists the subset limits the board works around. The board has no CI
job yet; run the script by hand with `LATTICE_BIN_DIR` set to the binaries under test.

The Claude Code session hooks in `integrations/claude-code/hooks` print a bounded digest of the open tasks at
session start, and the board events after a saved cursor on each prompt. `tools/test_agent_hooks.py` runs in the
`tools-tests` workflow. No interactive client session with these hooks is recorded here.

## Experimental and unstable

- The change-feed and sync encodings (RFC 0009 and 0010, ADR 0023 and 0024) are published without an
  accepted contract. They are not frozen and may change in any release.
- The MCP message shapes and tool schemas follow a draft design ([mcp-v0.md](mcp-v0.md)).
- Data directories: the stored row layout and log formats are not frozen. A later alpha may not open
  this alpha's data directory. Do not keep data you need.
- The TypeScript and Python clients in `clients/` are experimental codec clients checked against test
  vectors; they do not talk to a server.
- Durability evidence is simulation, process-kill tests and fuzz and proof jobs listed in the
  [gate pages](gates). It is not real power loss. Linux real-disk numbers are pending
  ([ADR 0011](adr/0011-engine-runtime.md)).

## Not in the alpha

Joins, aggregates, `GROUP BY`, `DISTINCT`, subqueries; binary result formats and JDBC; collaborative
text documents, presence and the workflow arbiter; snapshot-then-tail bootstrap for a feed (a cursor
below the vacuum floor is refused); a policy language, roles or row predicates (Phase 4; the alpha has
table-level grants); checkpoints, branches and online schema change; SCRAM channel binding; resource
subscriptions and MCP notifications; a web UI, schema packs, search, blobs and S3, clusters and the
optional models plugin; Windows.

## Known issues

| Issue | What you will see |
|---|---|
| [#574](https://github.com/moncifem/latticedb/issues/574) | Restart is tested in-process, with a power-cut sweep and psql scripts; the `kill -9` seed sweep with the real binary and the simulator workload are not done yet. Process kill is not power loss. |
| [#413](https://github.com/moncifem/latticedb/issues/413) | The driver smoke job covers five drivers in text mode (pgjdbc passes all steps; tokio-postgres parameter steps are pinned against #608), but binary formats are not supported and there is no full driver matrix. Only text results and parameters work. |
| [#608](https://github.com/moncifem/latticedb/issues/608) | Binary parameter and result formats are refused with `0A000`. A driver that sends binary parameters by default fails until it is set to text; psycopg 3's default `%s` placeholder does this (observed). |
| [#703](https://github.com/moncifem/latticedb/issues/703) | A statement is described with the parameter count its text implies (the highest `$n`), with type 0 (unspecified) for every placeholder the client did not declare; the server infers no parameter types. pgx's default query mode still fails, now with `0A000` because binary formats are refused (#608). Use `default_query_exec_mode=exec`. |
| [#382](https://github.com/moncifem/latticedb/issues/382) | `CREATE TABLE` and `DROP TABLE` inside a transaction block are refused with `25001`. A driver that opens a block before DDL fails until autocommit is on; psycopg 3's default connection does this (observed). |
| [#363](https://github.com/moncifem/latticedb/issues/363) | A filtered change feed reveals the count of hidden writes through gaps in cursors and never says a row left a reader's scope. Disclosed, not fixed. |
| [#543](https://github.com/moncifem/latticedb/issues/543) | A feed read still costs more than it should on a large or busy table; a quiet-table reader can get `Unavailable` on every poll after many other-table versions ([#661](https://github.com/moncifem/latticedb/issues/661)). |
| [#674](https://github.com/moncifem/latticedb/issues/674) | `COMMIT` or `ROLLBACK` outside a block, and `BEGIN` inside one, send no PostgreSQL warning (`25P01`, `25001`). |
| [#675](https://github.com/moncifem/latticedb/issues/675) | An unknown type name answers `0A000` instead of `42704`. |
| [#360](https://github.com/moncifem/latticedb/issues/360) | No SCRAM channel binding; a client that skips certificate verification can be relayed. |
| [#361](https://github.com/moncifem/latticedb/issues/361) | No security event log and no failed-authentication backoff for pgwire. |
| Gateway idle deadline and TLS | The gateway port can use TLS (`--tls-cert`, `--tls-key`), and a non-loopback bind needs TLS and `--gateway-tokens`. The idle deadline is not built ([#578](https://github.com/moncifem/latticedb/issues/578)). Do not expose a plaintext gateway. The pgwire listener can use TLS (SCRAM mode). |
| MCP transport | HTTP `POST /mcp` on the experimental gateway, with a bearer token; TLS and tokens are required off loopback. The stdio proxy `lattice mcp` reaches only a loopback, plaintext gateway. |
| Containers | The image's entrypoint passes `--stdin-control off` and prints usage with no arguments. A published port needs SCRAM over TLS. `tests/image/smoke.sh` (CI job `image-smoke`) tests the amd64 image only; the `nightly` image predates this entrypoint until the next nightly build. See [Run in a container](#run-in-a-container). |
| Platforms | No Windows (no signal handling or installer). Linux needs glibc as stated in the release notes. |

This table is edited by hand; the open issues and the [gate pages](gates) are the authority.

## Reporting a problem

Bugs and questions go to [issues](https://github.com/moncifem/latticedb/issues) and
[Discussions](https://github.com/moncifem/latticedb/discussions). Vulnerabilities go to the private
channel in [SECURITY.md](../SECURITY.md), never to a public issue.
