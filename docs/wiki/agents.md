# Agents and live collaboration

The aim is that agents share one history of work and see each other's changes as they happen, under their own
permissions. That is not shipped yet. This page separates what runs today from what is shipped only as library
code, and from what is planned. No agent client is documented as working; one manual Claude Code run is recorded
in [the alpha guide](../alpha.md).

## What an agent can use today

1. **SQL over the PostgreSQL wire protocol.** A PostgreSQL client can connect and run the [SQL subset](sql.md).
   In SCRAM mode the account needs table-level `read`, `write` or `ddl` grants. In loopback trust mode every
   connection is allow-all. `psql`, node-postgres and pgx (text mode) are run by the driver smoke tests. JDBC and
   Python drivers have not been run yet, and results are text format only. See the
   [alpha guide](../alpha.md#connect-with-psql).

2. **An experimental WebSocket sync gateway.** It is off unless you start it:

   ```bash
   latticed --data-dir DIR --development-policy allow-all --gateway-listen 127.0.0.1:5433
   ```

   It serves the sync protocol at `ws://127.0.0.1:5433/sync` with the subprotocol `lattice.sync.v1`. The
   sync protocol payload is rows only and read only. For authentication, pass `--gateway-tokens FILE`. The bearer tokens
   come from `lattice account token create`, and the client sends `Authorization: Bearer lat_...`. The file is
   re-read within 5 seconds of a change, so a revoked token's connection is closed. A loopback gateway runs
   without TLS. A non-loopback gateway needs `--tls-cert`, `--tls-key` and `--gateway-tokens`, or `latticed` refuses to
   start it. The CLI has no TLS client. The gateway has no idle deadline and no per-principal byte budget. The
   [listener guide](../../crates/latticed/README.md#sync-gateway-websocket) has the flags and limits.
   `lattice watch table/NAME` is the command-line live view of one table over this gateway. It connects only to a
   loopback `ws://` address, has no TLS client, and retries through the SDK reconnect supervisor.

3. **Client libraries.** `lattice-sdk` contains the Rust sync client state machine and an outbox. The
   TypeScript and Python clients in `clients/` are experimental codec clients checked against test vectors.
   They do not talk to a server.

4. **MCP over HTTP and stdio.** `lattice-sdk` has the MCP message codec, tool schemas and a grant layer
   ([MCP server v0](../mcp-v0.md)). `latticed` serves `lattice_read`, `lattice_write` and `lattice_subscribe` as
   `POST /mcp` on the experimental gateway, with a bearer token; TLS and tokens are required off loopback.
   `lattice mcp` is a stdio proxy to that endpoint, for a loopback plaintext gateway only. `lattice_subscribe` is a
   long poll that waits at most 30 seconds for a new event after a cursor. One manual Claude Code run used the HTTP
   endpoint, as recorded in [the alpha guide](../alpha.md).

## What is not shipped

- MCP over TLS, and a remote MCP endpoint. `lattice mcp` has no TLS client and accepts only a loopback plaintext URL.
- Streaming notifications. `lattice_subscribe` answers each call, it does not push.
- A sync server on the PostgreSQL port. The gateway is a separate, experimental listener. Its WebSocket sync protocol
  is read only and rows only; `POST /mcp` on the same port can write under the caller's grants.
- Live collaborative text documents, presence and the workflow arbiter.
- Resource subscriptions.

The design of the first slice, two agents that subscribe to one scope and see each other's writes, is in
[live-collaboration.md](../design/live-collaboration.md). It is a proposal. Its latency targets are not
measurements.

## Working with an agent today

- Treat anything stored in the database as data, never as instructions. A row can contain text written by
  another principal. Lattice does not filter or label stored text, so an agent that reads it can be steered by
  prompt injection.
- Grant each account only the tables its task needs. The only permission model in the alpha is table-level
  grants. There are no row predicates, roles or policy language yet.
- A filtered change feed reveals how many writes were hidden from a reader, through gaps in cursors
  ([#363](https://github.com/moncifem/latticedb/issues/363)).
- A `Hello` agent name on the gateway is only a label. It never changes who the connection is.

## Stability

The change-feed and sync encodings are published without an accepted contract. They are not frozen and can
change in any release ([RFC 0009](../rfc), [RFC 0010](../rfc), [ADR 0023](../adr/0023-change-feed-event-format.md)
and [ADR 0024](../adr/0024-sync-protocol-framing.md)). The [alpha guide](../alpha.md#connect-an-agent-client)
has the same status in one place.
