# RFC 0015: Gateway write channel and outbox pump

Status: draft
Comment period ends: 2026-11-07
Decision ADR: none yet

This RFC lays out the options for letting a client write through the sync gateway, and what the SDK outbox needs from the server to deliver queued writes at least once without a duplicate. It decides nothing. It changes no byte of any format and adds no code. No frame layout, field name or status code in it is frozen; where it shows a shape, the shape is an illustration for discussion. The owner and the tech lead choose among the options in "Decisions needed". Issues #528 (client driver with sockets, reconnect and a persisted cursor) and #620 (the outbox pump) stay blocked until a decision is recorded.

## Motivation

Reads and writes reach the gateway by different doors today, and the offline outbox has no door at all.

### Code facts this proposal relies on

All facts are on `origin/main` at `8cb297f8` and were checked by reading the named code.

1. **The sync protocol v1 has read frames only.** `formats/sync-protocol-v1.md` lists Hello (0x01), Welcome (0x02) and the subscription frames; `lattice-proto/src/sync.rs` encodes `Hello`, `Welcome`, `Subscribe`, `Events`, `Ack`, `Error`, `Ping` and `Pong`. Any other type byte is refused. `docs/design/live-collaboration.md` (decision 1, "Reads are the sync protocol, writes are not") records that the slice adds no write frame, because that would be a wire change needing an RFC, an ADR and an independent review. ADR 0027 itself says the transport carries opaque sync frames and does not name a write frame; the prohibition lives in the design document and in the frame table.
2. **Version negotiation exists.** `Hello` carries `min_version` and `max_version`; the server picks the highest version in both ranges and answers `unsupported_version` when the ranges do not overlap. The server speaks 1 to 1 today. The format document says later versions that change a frame layout need a new version number, and that frames carry no version of their own.
3. **The WebSocket subprotocol is `lattice.sync.v1`.** `websocket::parse_upgrade_with` is called with the required subprotocol in `crates/latticed/src/ws.rs` (`require_subprotocol: true`). ADR 0027 states the same name.
4. **Agents write through `POST /mcp`.** `crates/latticed/src/ws/mcp_http.rs` serves it on the gateway port, bearer token verified, one `McpBackend` per request bound to the verified principal. `lattice_write` takes `scope`, `table`, `key`, `value` and an optional `expected_version` (`crates/lattice-sdk/src/mcp.rs`, `docs/mcp-v0.md`). The request body is bounded by `MAX_BODY` (1 MiB) in `mcp_http.rs`, one MCP message by 256 KiB, and `value` by 65,536 characters.
5. **`lattice_write` has no client-supplied idempotency token.** `McpBackend::write` (`crates/latticed/src/mcp_backend.rs`) issues an `UPDATE ... RETURNING version` or an `INSERT`, and the row store takes a fresh id for every commit (`let id = self.kernel.new_id()` in `crates/lattice-engines/src/store.rs`). Without `expected_version`, resending the same call writes again and bumps `version` again. With `expected_version = n`, a resend after a lost reply finds the row at `n + 1` and gets the tool error `version conflict` (SQLSTATE `40001` and `23505` map to `Conflict`), which looks the same as another writer winning. `crates/latticed/src/mcp_backend_tests.rs` pins that a stale `expected_version` gives `version conflict` (`version_rules_hold`), but no test covers the lost-reply resend itself, so the claim that it looks like another writer winning is inferred from the SQL the backend builds.
6. **The kernel dedupes and fences, but the row path supplies neither a client id nor a turn.** `Kernel::check_admission` (`crates/lattice-core/src/kernel.rs`) checks the turn fence first, then the seen set, then in-flight ids, and returns `Receipt::Duplicate` for a stored id. `Receipt` has two variants, `Recorded { lsn, hlc }` and `Duplicate`; a fenced turn is the error `TxnError::Fenced`, not a receipt. The row store builds its change sets with `change_set(id, payload, author)`, which sets the author and no turn, and `store.rs` has no use of a turn at all. So SQL and MCP commits are never fenced and never deduplicated by a caller-chosen id.
7. **The outbox speaks the plugin contract, not MCP.** `crates/lattice-sdk/src/outbox.rs` is a pure state machine. A write gets the record id `"<client id>-<counter>"` at `enqueue`; an item leaves the queue only on a `recorded`, `duplicate` or `fenced` receipt for its id; one batch is in flight at a time; `fenced` drops every queued write of that turn. The messages are those of `crates/lattice-sdk/src/contract.rs` and `formats/plugin-contract-v1.md`: `append` with up to 64 records and `append.receipt` with the statuses `recorded`, `duplicate` and `fenced`; a line up to 1,048,576 bytes, a body up to 262,144 bytes, a record id up to 128 bytes. Those types are used by the SDK and its tests only. No code in `crates/latticed` or `crates/lattice-api` answers `append`, and `docs/design/live-collaboration.md` lists the server on the kernel (WP48, issue #250) as missing.
8. **The reconnect supervisor decides on reads only.** `crates/lattice-sdk/src/reconnect.rs` classifies connection outcomes for the subscribe path (retry, stop on `Unauthorized`, stop on an incompatible version). It has no input for a write receipt and it never retries a write.
9. **Principal binding is per connection.** The gateway verifies a bearer token at the upgrade (or at `POST /mcp`) and binds the principal; `McpBackend::caller_for` derives the tool grants from the account's table grants, and every statement runs through the executor as that principal with row policy. Arguments cannot name an identity (`docs/mcp-v0.md`, "Identity").
10. **Backpressure for gateway writes is not designed.** `crates/lattice-api/src/backpressure.rs` holds pure queues for outgoing events and is not wired (`docs/design/live-collaboration.md`, Backpressure row). Per-principal connection and output budgets are issue #529. `crates/latticed/src/sql.rs` sizes the executor `queue_depth` from `max_connections` plus the gateway's `max_connections`.

### The problem

A client that is offline for a while queues writes in the outbox. When it reconnects it holds two things: a cursor for the read side and a queue with stable record ids for the write side. The read side has a door (`Subscribe`). The write side has none that understands record ids and receipts. `lattice_write` accepts the write but cannot say "I already have this one", and it carries no turn, so the fence order in the kernel (fence before duplicate) cannot apply to it. A pump built on `POST /mcp` today would either risk double writes (no `expected_version`) or turn every lost reply into a conflict (with it).

## Proposal

This section states the requirements any option must meet, then the options. It recommends nothing as accepted.

### 1. Requirements (what the outbox pump needs)

- R1. **Stable client token.** A write carries the outbox record id, unchanged on every resend. The server treats a second arrival of the same token from the same principal as a no-op and says so.
- R2. **Three receipts.** The server answers each write with one of: recorded (with the commit position), duplicate (nothing changed), or fenced (the turn was superseded; never retry). This is the vocabulary of `contract::Status`, and the outbox already removes an item on any of the three.
- R3. **Fence before duplicate.** A superseded turn is answered fenced even when the token is already stored, as in `Kernel::check_admission`, so a stale writer cannot learn that its old write succeeded.
- R4. **Principal binding.** The principal comes from the credential that opened the connection, never from the message. A token is scoped to the principal: the same token string from two principals is two different writes.
- R5. **Same permission path as a SQL or MCP write.** Grants, row policy and the author id of the commit are the executor's decision, not a second code path.
- R6. **Bounded everywhere.** Message size, records per batch, writes in flight per connection and per principal, and the time a write may wait for the executor are all bounded, and the server refuses before buffering.
- R7. **Order.** Within one connection and one turn, writes commit in the order sent. No promise across connections.
- R8. **Read-your-write.** The receipt carries a commit position that lets the client know (how it maps to the 12-byte feed cursor is Q10) when its own write has appeared on the subscription, so it can drop its local optimistic copy exactly once.
- R9. **No silent loss on overload.** When the server cannot take a write, the answer is an explicit refusal that the outbox treats as "keep and retry later", not a close that the client cannot tell from a lost reply.
- R10. **No cross-principal fencing.** Turn numbers, sessions, fences and any token-to-id mapping are scoped to the principal or assigned by the server. The kernel fence is monotonic per session (`Kernel::check_fence`), so if a client could choose a session or a high turn that another principal's writers also use, it could fence them out: a cross-principal denial of service.

### 2. Dedupe and turns: what is missing on the row path

The kernel already provides what R1 to R3 need for a change set that has a client id and a turn. The row store does not offer them to a caller today (fact 6). Every option below therefore needs the same server-side piece, which this RFC does not design:

- a way for the executor path to commit a row change set under a caller-supplied id and turn, instead of `new_id()` and no turn;
- a rule for how a client token (up to 128 bytes in the plugin contract) maps to the kernel's change-set id (a fixed-size id) per principal, with no collision between principals and no way for one principal to probe another's tokens;
- how long the seen set remembers a token (the kernel's seen set is rebuilt from the log; whether it is bounded or truncated at a checkpoint has to be read in `kernel.rs` before this is decided, and is not measured here);
- how a turn is opened (`Kernel::begin_turn`) on behalf of a remote client, and what bounds the number of sessions and turns a principal may hold.

Changing how a commit is identified may touch the WAL and checkpoint formats and would then be a format change that needs its own RFC, ADR and independent review (RFC 0008 for the capability bit if old readers must refuse). Whether it does is an open question (Q4).

### 3. Option A: reuse `POST /mcp` with `lattice_write`

Add an optional idempotency member to `lattice_write` (for example a record token and a turn), keep the transport and the endpoint, and have the backend commit under that id.

- For: no new transport and no sync version bump; the grant and scope checks already run; the endpoint is bounded and tested; agents already use it.
- Against: HTTP request and response is one write per round trip, so a queue of 1,000 offline writes costs 1,000 round trips unless the tool takes a batch, which enlarges the schema and makes the 256 KiB message bound the batch bound. Receipts and the cursor for R8 would have to be added to the tool result (`docs/mcp-v0.md` already lists `next_cursor` on write results as missing). The tool schema is a surface shown to models; client plumbing members make its description longer and give a model a way to pass a token it does not understand. Changing the input schema is visible to every MCP client.
- Format impact: tool schema only, unless the commit identity change in section 2 touches the log.

### 4. Option B: a write frame in sync protocol v2

Negotiate version 2 in `Hello` and `Welcome`, and define write frames that exist only in v2: for example one client-to-server frame carrying a batch of (token, scope, key, value, optional expected version) records, and one server-to-client frame carrying one receipt per record. Whether the subprotocol name gains a version (`lattice.sync.v2`) or stays and relies on `Hello` is Q2.

- For: one socket for reads and writes, ordered by the connection; batching is natural; the receipt and the event for the same row travel on one ordered stream, which makes R7 and R8 simple. Backpressure can reuse the flow-control rules of subscriptions.
- Against: it is the only option that changes the wire protocol. It needs the full RFC 0010 / ADR 0024 style process, new golden vectors, the TypeScript and Python codecs (`conformance/vectors.json`) and an independent review. A v1 server keeps refusing the frame, and negotiation means it never sees one, but the client must not send it before `Welcome` names version 2 (frames carry no version). A write path on the sync socket is a second way to reach the executor, so every property of `POST /mcp` (principal binding, scope check, grants, message bound) has to be implemented again or shared by construction. The v1 document says the pure codec "does not enforce handshake state, direction, version admission or close-on-error", so a v2 session layer has to.
- Format impact: wire change (new version, new frames), which this RFC does not specify.

### 5. Option C: SQL over the PostgreSQL wire

The outbox pump speaks pgwire to the SQL listener, with the record token stored as an ordinary column and a unique constraint doing the dedupe.

- For: nothing new on the wire; SQL writes already pass the whole permission path; a driver exists in every language.
- Against: the dedupe is schema design, not a kernel guarantee. Every table that accepts outbox writes needs a token column and a unique index. A deduplicated record gives `23505`, which `McpBackend` already treats as a conflict, so "duplicate" and "another writer won" are confusable again unless the pump reads the row first. There is no turn and so no fence (fact 6). pgwire is a different listener, port and credential flow (SCRAM over TLS) from the gateway's bearer token, so a client holds two connections and two credentials.
- Format impact: none, but it does not meet R2 (fenced) or R3.

### 6. Option D: a separate append endpoint speaking the plugin contract

Serve the existing plugin contract (`append` and `append.receipt`) at a new path next to `/sync` and `/mcp`.

- For: the outbox already speaks it, its receipts already have the three statuses, and the conformance suite in `crates/lattice-sdk/tests/support/` already tests it, so the client needs no change.
- Against: it is a third door into the executor and a third message grammar on one port (the contract has its own JSON subset, distinct from the MCP reader, `docs/mcp-v0.md`). The contract is a session protocol (`session`, `turn`, `parent`); mapping a remote principal onto a contract session is the unfinished WP48 / issue #250 work. Its format document describes a local plugin that "carries no credentials", so exposing it beyond the machine is itself a decision.
- Format impact: none on existing formats; adds a served surface.

### 7. Outbox pump semantics (common to A, B and D)

The pump is a driver in front of the pure `Outbox` state machine, as the reconnect driver is in front of `Supervisor`. Whatever the transport:

1. After the connection is up and the cursor resumed, `poll` the outbox and send the batch it returns, with record ids as tokens.
2. On a receipt, feed it to the outbox. `recorded` and `duplicate` remove the item; `fenced` removes the turn's items and reports them to the application (`Outcome::fenced`).
3. On connection loss, call `connection_lost`; the unacked batch is resent after reconnect with the same ids. The outbox's rule that nothing newer than the last persisted snapshot is sent is unchanged.
4. A refusal for overload or temporary failure is neither a receipt nor a loss: the batch stays in flight and is retried on the outbox's own backoff (`base_delay << attempt`, capped at `max_delay`). A permanent refusal (unauthorized, permission denied, rejected value) must reach the application; the outbox has no input for it today, so the pump needs a defined rule (Q6). The reconnect supervisor must also learn that a write-side `Unauthorized` is fatal, as the read side already is.

Read-your-write (R8) uses the subscription the client already holds. The receipt gives the commit position; the client treats its optimistic copy as superseded when an event at or after that cursor for the same key arrives.

### 8. Grants, principal binding and abuse

- The principal is the connection's, as today. A token is meaningful only inside that principal (R4).
- A write to a table without the write bit is refused as `lattice_write` is (`permission_denied`), and the refusal must not reveal whether a table exists to a principal that cannot see it (the backend answers `not_found` for both).
- A principal with a valid credential can fill the seen set with distinct tokens. A bound on remembered tokens per principal, and what happens at the bound (refuse the write, or forget the oldest and risk a late duplicate), is a security and correctness trade-off for the owner (Q5). Forgetting is only safe if the outbox can never resend a token older than the window, which it cannot guarantee after a long offline period.
- A fenced answer tells the stale writer that another turn took over. That is its purpose; the answer must not carry the current turn's contents.
- Value bounds: the MCP numbers (64 KiB value, 256 KiB message) and the plugin contract's (256 KiB body, 64 records, 1 MiB line) differ, and this RFC does not pick (Q7).

### 9. Backpressure and failure modes

| Situation | Needed behaviour | Today |
|---|---|---|
| Executor queue full | Explicit retryable refusal, nothing committed | Statements wait on a bounded queue (`sql.rs` `queue_depth`); behaviour at the bound for gateway writes is neither specified nor measured here |
| Client faster than the executor | Bound writes in flight per connection (the outbox sends one batch at a time) and per principal | Per-principal budgets are issue #529 (a pull request, #861, is in flight and is not assessed here) |
| Reply lost after commit | Resend is a no-op and the answer is `duplicate` | Not true for `lattice_write` (fact 5) |
| Crash between commit and reply | Same as above, across a server restart | Needs the token to be in the log so recovery rebuilds the seen set (fact 6) |
| Server restart | The pump reconnects, resumes the cursor, resends | Reconnect exists for reads only (fact 8) |
| Slow reader on the same socket (Option B) | A write must not be starved by, or block, event delivery | Not designed |
| Poison write that always fails | Must not block the queue behind it forever | The outbox refuses unsendable bodies at `enqueue`; a server-side permanent refusal has no rule (Q6) |
| Clock | No part of dedupe or fencing depends on a wall clock | True of the kernel path: the order is the log's |

Failure injection for any accepted option should reuse the seeded kill schedule in `lattice-e2e` (`kill_points`) and the planted-bug registry (a bug that skips the dedupe, one that checks duplicate before fence, as `Bug::NoDedupe` and `Bug::DuplicateBeforeFence` do in the kernel). That is a test plan, not a claim that it exists for the gateway.

### 10. What is not decided here

The byte layout of any frame, the HTTP shape of any new endpoint, the token-to-id mapping, the token retention window, error codes, and whether the commit identity change alters a stored format.

## Alternatives

Besides the four options above:

- **No write channel; the client does not use the outbox over the gateway.** Agents write through `lattice_write` and accept double writes on retry, or always send `expected_version`. Cheapest, and a legitimate answer for a first alpha if offline writing is not a goal. It leaves #528 and #620 blocked and the outbox without a server.
- **Client-side dedupe through a read.** Before resending, the client reads the key and compares. Needs no server change, but it is wrong whenever another writer changed the row in between, and it costs a round trip per write.
- **An idempotency header on `POST /mcp`** (an `Idempotency-Key` header). A smaller form of Option A that leaves the tool schema untouched, so the token stays outside the model's view; HTTP intermediaries and logs would see it. It still needs the commit identity change from section 2.
- **A second WebSocket subprotocol for writes only** (for example `lattice.write.v1`) beside `lattice.sync.v1`. Keeps v1 untouched and avoids negotiating inside `Hello`, at the cost of a second socket per client and a second codec. ADR 0027 chose one transport and one `Hello` negotiation, which argues against it.

## Compatibility and migration

- Nothing changes today. v1 clients and servers are unaffected by this RFC.
- Option A changes the tool input schema additively (optional members); clients that omit them keep today's behaviour, including its lack of idempotency.
- Option B needs `Hello` negotiation: a v2 client talking to a v1 server negotiates down to 1 and must then not offer writes; a v1 client talking to a v2 server negotiates 1 and sees no change. The v1 document already tabulates this (a client range `1..=2` gets version 1 from a v1-only server).
- Any option that changes how a commit is identified or fenced is a candidate for a WAL capability bit so that an old reader refuses a log it would misread (RFC 0008, ADR 0022). Whether it applies depends on Q4.
- No phase-gate or release claim follows from this RFC. A format-changing outcome needs an independent review before merge.

## Decisions needed (owner and tech lead)

1. Is a write path for the outbox in scope for the alpha, or is "agents write through `lattice_write`, no offline queue over the gateway" the stated limit? This decides whether any of A to D proceeds.
2. Which option: A, B, C, D or an alternative. If B, is the subprotocol name versioned (Q2).
3. Acceptance: this is a wire or tool-surface change, so the seven-day comment period, an independent review of the final text and an ADR apply. This RFC claims none of them.

## Open questions

1. Is a caller-chosen token acceptable as the kernel's change-set id after hashing with the principal, or does the log need a separate token field? (Collision resistance, probe resistance, and the size of the id.)
2. If B: `lattice.sync.v2` as a new subprotocol, or the same name with `Hello` negotiating 2? What does a v1-only intermediary or client do in each case?
3. What does a durable acknowledgement cost for a batch? `Receipt::Recorded` means newly committed and durable, but the latency of a batch of 64 through the executor has not been measured; a benchmark on the real executor path is needed before any claim.
4. Does committing under a caller-supplied id and turn through the row store require a change to the WAL record, the change-set encoding or the checkpoint, or only to the in-memory path? This needs a full read of `store.rs`, `changeset.rs` and `formats/changeset-v1.md` by the author of the follow-up; this RFC did not complete it.
5. Token retention: bound per principal, per server, or by log position; what happens at the bound?
6. Permanent refusals (unauthorized, permission denied, table shape): does the outbox keep the item, drop it with a report, or park it? The current outbox has no such input.
7. Value and batch bounds for a write: the MCP numbers or the plugin contract's?
8. Ordering across a reconnect: if batch N+1 arrives on a new connection while batch N is still in flight on the old one, can the server commit them out of order? The outbox sends one batch at a time, so the client does not cause it, but how the server treats the dead connection's in-flight batch is not designed.
9. Does a gateway write appear on the same principal's subscription with its own author? Events carry the commit's author id; the live-collaboration design lists author and turn on events as open work.
10. The receipt of the plugin contract carries a `u64` position (`Receipt.pos`), while the feed cursor is 12 bytes (commit LSN, then the index in the change set). How a receipt maps to a cursor (R8) is not verified and needs a decision.
11. Scoping of sessions, turns and tokens to the principal (R10): server-assigned sessions, or a principal prefix in the kernel session id? Either changes what the outbox's fenced-turn rule means for a client that picks its own turn numbers.
12. A resend after the principal's write grant was revoked: `permission_denied` (the grant is checked first) or `duplicate` (the token is found first)? The first reveals nothing about stored tokens, the second tells a revoked principal that its write landed. Not decided.

## Test sketch

Not implemented. If an option is accepted: a model test that resends every batch at every I/O boundary and asserts one commit per token; a fence-before-duplicate case on the new path (a stale turn with a stored token is fenced); a cross-principal case (same token, two principals, two commits); a bounded-queue case; the kernel kill schedule from `lattice-e2e` around the commit and the reply; planted bugs for the skipped dedupe and the swapped order; and a differential test that a gateway write and the same write over SQL leave the same rows and the same feed events.

## Review

Pending. This draft has had no independent review.
