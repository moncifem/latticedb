# ADR 0027: Sync transport: WebSocket first, QUIC only if data justifies it

Status: proposed
2026-10-30

## Context

Requirements P3-01 and P3-02 need a transport that carries the [sync protocol](../../formats/sync-protocol-v1.md)
between a server and SDK clients in browsers, agents and services. The protocol assumes an ordered,
reliable byte stream and brings its own framing ([ADR 0024](0024-sync-protocol-framing.md)), so the
transport only has to carry opaque frames, enforce the existing authentication and TLS rules and bound
connections and messages. Candidates are WebSocket (RFC 6455) over TLS and QUIC (RFC 9000).

WebSocket is available in every browser and has client libraries in every SDK language Lattice
targets, passes the HTTP proxies and load balancers teams already run, and is a small protocol: a text
upgrade followed by length-prefixed frames. QUIC would remove head-of-line blocking between streams and
survive client address changes, but browsers cannot open raw QUIC connections (only WebTransport), the
Rust and SDK ecosystems are younger, UDP is blocked on many networks and a second transport doubles the
surface to test and secure.

## Decision

Build the sync gateway on WebSocket first. Each sync frame travels as one binary WebSocket message, so
the sync length prefix gives the message length and a message never carries a partial
sync frame. The codec can refuse text messages (close code 1003) so the gateway can enforce binary-only
messages on the subprotocol `lattice.sync.v1`. The gateway requires
the same protections as the PostgreSQL listener: TLS and authentication on every non-loopback bind.

The WebSocket layer is a sans-I/O codec (`lattice_api::websocket`): upgrade request parsing, the accept
key, frame decode and encode, message reassembly, close handling and timers on a clock the caller
supplies. It does no I/O, reads no real time and draws no randomness, so it can be fuzzed and simulated.

Adopt QUIC (or WebTransport) only when measurements, not preference, show a WebSocket limit. The decision
rule: open a new ADR for QUIC when, on the sync benchmark of the same workload and after tuning the
WebSocket path (per-message compression off, write batching, socket buffer sizes), at least one of these
holds in recorded runs:

1. Head-of-line blocking: with 1% packet loss and 50 ms round trip, the p99 delivery delay of an
   unrelated subscription rises by more than 2x versus the no-loss run because another subscription's
   large batch is stalled on the same TCP connection.
2. Reconnect cost: clients that change network (mobile, laptops) spend a median of more than 1 s
   re-establishing TCP, TLS and the upgrade before the cursor resume, in a share of sessions that
   product owners judge material.
3. Fan-out: the per-connection memory or CPU of the gateway at 10,000 idle subscriptions is more than
   2x what a QUIC prototype shows on the same machine.

If none holds, WebSocket stays. The thresholds are starting points to be revised in the QUIC ADR with the
data, not commitments. A QUIC transport, if ever added, carries the same sync frames, so the protocol
and the SDK surface do not change.

## Consequences

The first slice is only the codec; there is no listener, no TLS or authentication wiring and no
integration test over a real socket yet, so issue #242 stays open. Per-frame and per-message bounds are
in the codec; connection counts, aggregate memory and queue bounds remain host policy. WebSocket runs on
TCP, so a lost packet stalls every subscription on that connection; clients that need isolation open
more than one connection. The permessage-deflate extension is not negotiated, which avoids compression
side channels on authenticated data and keeps frame sizes predictable.
