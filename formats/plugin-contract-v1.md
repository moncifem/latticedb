# Format: Agent plugin contract v1

Status: draft. Requirement P3-10; the codec is `lattice_sdk::contract`; the reusable conformance suite is `crates/lattice-sdk/tests/support/`.

An agent harness (a coding agent, a chat client) talks to Lattice through a small local plugin. The
plugin keeps one Lattice session per harness session, records everything the agent does, and streams
back what teammates add. This document is the whole wire contract between a harness-side client and
a plugin: one line of JSON per message, over any ordered byte stream (a pipe, a socket, a WebSocket
text channel). It carries no schema knowledge and no credentials.

## Layout

### Framing

* A message is exactly one JSON object on one line, encoded as UTF-8, terminated by `\n` (0x0A).
* A line is at most **1,048,576 bytes**, not counting the newline. A longer line is discarded up to
  its newline and answered with one `error` (`too_large`); the stream stays usable.
* No `\r`, no byte order mark, no empty lines. A trailing `\r` is part of the line and is a syntax
  error. An empty line is an error (`bad_request`).
* Spaces and tabs are allowed between JSON tokens. Writers emit none (the canonical form below).
* A stream that ends in the middle of a line (no final newline) discards that partial line.

### JSON subset

The decoder accepts JSON objects, arrays, strings, `true`, `false`, `null` and **non-negative
integers** only.

* Integers are decimal digits with no sign, fraction, exponent or leading zero (`0` is the only
  number that starts with `0`) and at most **9,007,199,254,740,991** (2^53 - 1). Every other number
  form is refused, so no client rounds a position.
* Strings use the standard escapes. A raw control character (below 0x20) inside a string is refused.
  `\uXXXX` escapes must be well formed; a lone surrogate is refused. Invalid UTF-8 anywhere in the
  line is refused.
* An object with a repeated key is refused, whatever the values. An object has at most 16 members,
  an array at most 64 elements, nesting is at most 6 levels.
* `null` appears in exactly one place: the `id` of an `error` that could not be attributed.

### Canonical form

Writers emit fields in the fixed order given in the message tables, no whitespace, and escape only
`"`, `\`, and control characters (`\n \r \t \b \f` by name, the rest as `\u00xx`, lower-case hex).
Everything else, including non-ASCII, is written as raw UTF-8. Readers accept any key order and
spacing. `encode(decode(line))` equals `line` for any line that is already canonical.

### Common fields

Every message has `v` (integer, always `1`) and `t` (string, the message type). Nothing else is
common. A message with a field its type does not define is refused (`bad_request`), so is a missing
required field, a field of the wrong JSON type, and an unknown `t`.

### Value types

| Type | Rule |
|---|---|
| `token` | 1 to 64 bytes of `A-Z a-z 0-9 . _ : -` (record ids `rid` allow 128 bytes). Used for `id`, `session`, `parent`, `rid` |
| `source` | 1 to 64 bytes of `a-z 0-9 . _ -` |
| `external_id` | 1 to 256 bytes of UTF-8, no control characters |
| `body` | UTF-8 text, 0 to 262,144 bytes (counted after unescaping) |
| `message` | UTF-8 text, 0 to 512 bytes |
| `turn`, `pos` | integer, 1 to 2^53 - 1 |
| `cursor`, `head` | integer, 0 to 2^53 - 1 |
| `kind` | `"message"`, `"tool_call"`, `"tool_result"`, `"note"` |
| `status` | `"recorded"`, `"duplicate"`, `"fenced"` |
| `state` | `"running"`, `"idle"`, `"superseded"` |
| `code` | `"bad_request"`, `"unsupported_version"`, `"unknown_session"`, `"unknown_turn"`, `"unknown_watch"`, `"conflict"`, `"too_large"`, `"internal"` |

An `append` carries 1 to 64 records; an `append.receipt` has 1 to 64 receipts. A request whose
records are valid individually but whose line exceeds the line limit is refused as a whole.

### Messages

Client to plugin (requests):

| `t` | Fields in canonical order | Reply |
|---|---|---|
| `session.create` | `id`, `source`, `external_id`, `parent` (optional token) | `session.created` |
| `turn.take` | `id`, `session` | `turn.state` |
| `turn.end` | `id`, `session`, `turn` | `turn.state` |
| `append` | `id`, `session`, `turn`, `records` (array of `{rid, kind, body}`) | `append.receipt` |
| `watch` | `id`, `session`, `cursor` | `ready`, items and turn events, `live`, then incremental messages |
| `watch.stop` | `id` | `watch.stopped` |

Plugin to client:

| `t` | Fields in canonical order |
|---|---|
| `session.created` | `id`, `session`, `created` (boolean), `parent` (present only for a child session) |
| `turn.state` | `id`, `session`, `turn`, `state` |
| `append.receipt` | `id`, `session`, `turn`, `receipts` (array of `{rid, status, pos}`; `pos` absent exactly when `status` is `fenced`) |
| `ready` | `id`, `session`, `head` |
| `item` | `id`, `session`, `pos`, `turn`, `rid`, `kind`, `body` |
| `turn.event` | `id`, `session`, `pos`, `turn`, `state` |
| `live` | `id`, `session` |
| `watch.stopped` | `id` |
| `error` | `id` (token or `null`), `code`, `message` |

For a reply, `id` echoes the request. For `ready`, `item`, `turn.event` and `live`, `id` is the id of
the `watch` request they belong to.

## Semantics

### Order and replies

One stream carries one conversation. The plugin answers requests in the order received, each with
its reply (or one `error`). Messages a request causes for open watches (`item`, `turn.event`)
follow that request's reply. Pipelining is allowed. An `error` never closes the stream.

### Session identity and idempotent create

A session is identified by the pair (`source`, `external_id`): the source system's name for itself
(`claude-code`, `codex`) and its own id for the session. `session.create` for a pair that exists
returns the same plugin session id with `created: false`; for a new pair it creates the session and
returns `created: true`. Calling it again, after a crash or a reconnect, is always safe. The plugin
session id is opaque and stable.

### Child sessions

`parent` names an existing plugin session id and is fixed when the child is created. An unknown
parent is `unknown_session`. Creating an existing pair with a different parent (or with a parent when
it has none, or none when it has one) is `conflict`. Because a parent must exist before its child, a
cycle cannot be built. A session's positions, items and turns are its own; watching the parent does
not stream the child.

### Positions

Each session has a counter. Every stored record and every turn event takes the next position, starting
at 1, with no gaps and no reuse. A position is assigned by the plugin and never changes.

### Turns and fencing

A session has a current turn number, `0` before the first `turn.take`. `turn.take` increments it,
makes the new turn `running` and answers with it. If the previous turn was still `running`, it first
becomes `superseded` (a `turn.event` at its own position, then the `running` event of the new turn).
`turn.end` for the current turn records `idle` and answers `idle`; for an older turn it answers
`superseded` and changes nothing; for a number never issued it is `unknown_turn`.

An `append` names the turn the records were produced under. The turn is compared by number:

* equal to the current turn: the records are stored;
* lower than the current turn: **fenced**. Every receipt is `fenced`, nothing is stored, no position
  is consumed and no watcher is notified, even if a record id is one already stored;
* higher than the current turn: `unknown_turn`, nothing stored.

The fence check comes before the duplicate check. After a takeover, late output from the old turn can
never write, however late it arrives or after however many reconnects.

### Append and receipts

The idempotency key of a record is (`session`, `rid`), independent of turn and content. For each
record, in order:

* new key: stored at the next position, receipt `recorded` with that `pos`;
* known key: nothing stored, receipt `duplicate` with the `pos` of the first write. The first write
  wins; a different `kind` or `body` under a known `rid` is still a `duplicate`;
* fenced turn: receipt `fenced`, no `pos`.

A repeated `rid` inside one append makes the second a `duplicate` of the first. An append is checked
whole before anything is stored: a malformed record, an unknown session, an unknown turn, makes
the whole request an `error` with nothing stored. Receipts have the same length and order as the
records. A client resends an append it did not see a receipt for, unchanged, and trusts the receipts.

### Watch

`watch` opens a stream on one session from `cursor`, the last position the client has already seen
(`0` for the start). The plugin answers, in this order:

1. `ready` with `head`, the highest position at the moment the watch opened;
2. every item and turn event with `cursor < pos <= head`, in position order;
3. `live`, meaning replay is complete;
4. afterwards every new item and turn event once, in position order, as it happens (incremental).

A client that applies messages in the order they arrive sees each position after `cursor` exactly
once and none missing; reconnecting with the last `pos` it saw resumes without gap or repeat. A
`cursor` above `head` is `bad_request`. Several watches may be open on a session; a `watch` whose `id`
is already an open watch is `conflict`. `watch.stop` closes a watch, after which no message with that
`id` is sent; an unknown id is `unknown_watch`. An unknown session is `unknown_session`.

Items written by the watching client itself are delivered to its watches like anyone else's; a client
that wants to ignore its own echoes matches `rid`.

### Errors

`error` carries a stable `code` for programs and a `message` for people; the message text is not part
of the contract. A line that cannot be decoded is answered with `id: null` (the plugin did not read
a trustworthy id), and the code is `unsupported_version` for a version other than 1, `too_large` for
an over-long line, `bad_request` for everything else. Errors from a well-formed request carry that
request's `id`.

## Versioning and compatibility

* `v` is an integer. Version 1 is this document. A line with any other integer `v` is refused with
  `unsupported_version` and no other part of it is interpreted. A non-integer or missing `v` is
  `bad_request`.
* There is no negotiation. A plugin that implements several versions answers each request in the
  version of that request; one that implements only version 1 answers `unsupported_version`.
* Version 1 is strict in both directions: unknown fields, unknown `t` and unknown enum values are
  refused. Therefore any change visible on the wire, even an added optional field or enum value, is a
  new version. Adding a message type or field requires a version bump, an RFC (`docs/rfc`) and an ADR.
* Within a version nothing changes: not the field order of the canonical form, not a limit.
* The worked examples below are frozen with the version: a later version adds examples and never edits
  these.

## Checksums

None. Integrity is the transport's job; the contract only bounds sizes and positions.

## Golden files

The worked examples in this document are the golden files of the format. Every `>` line is a client
line and every `<` line the canonical reply; the test `crates/lattice-sdk/tests/plugin_contract.rs`
decodes each line, checks that it re-encodes byte for byte, and replays each transcript against the
reference plugin from a fresh start. Each transcript starts with a new plugin; session ids are `s1`,
`s2` in creation order, and positions are as described above.

## Conformance

`crates/lattice-sdk/tests/support/suite.rs` defines the trait `PluginUnderTest` (send one line, get the
lines produced) and a function per rule:

| Check | Rule |
|---|---|
| `create_is_idempotent` | the same source and external id returns the same session, `created` true then false |
| `append_receipts_and_duplicates` | new records are `recorded` at increasing positions; a resend is `duplicate` at the original position and adds no item |
| `superseded_turn_is_fenced` | after a takeover an append under the old turn is `fenced`, has no position, and writes nothing, also for a record id already stored |
| `watch_ready_items_live` | a watch sends `ready`, then every item after the cursor in order, then `live`, then new items incrementally; a resume from a cursor repeats nothing |
| `child_sessions` | a child echoes its parent; an unknown parent is `unknown_session`; a different parent is `conflict` |
| `rejects_bad_lines` | a wrong version, an unknown field and a malformed line each get an `error` and leave the plugin usable |

The suite in `tests/support/reference.rs` carries a reference plugin that passes every check, and the
tests include one deliberately broken plugin per rule to prove that each check can fail.

## Worked examples

Lines starting with `>` go from the client to the plugin; lines starting with `<` come back.

### 1. Create a session, twice

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"session.create","id":"r2","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r2","session":"s1","created":false}
```

### 2. Take the first turn and append

The turn event takes position 1, so the records land at 2 and 3.

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"Fix the flaky test"},{"rid":"m2","kind":"tool_call","body":"{\"cmd\":\"cargo test\"}"}]}
< {"v":1,"t":"append.receipt","id":"r3","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"recorded","pos":2},{"rid":"m2","status":"recorded","pos":3}]}
```

### 3. Resend an append: duplicate receipts

The first record is known, the second is new. Nothing is stored twice.

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"hello"}]}
< {"v":1,"t":"append.receipt","id":"r3","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"recorded","pos":2}]}
> {"v":1,"t":"append","id":"r4","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"hello"},{"rid":"m2","kind":"note","body":"retry"}]}
< {"v":1,"t":"append.receipt","id":"r4","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"duplicate","pos":2},{"rid":"m2","status":"recorded","pos":3}]}
```

### 4. A takeover fences the old turn

A teammate (or a second harness) takes the turn. The late append under turn 1 is fenced and writes
nothing: the later replay shows only `m1` and `m2`, and `m2` is accepted under turn 2 at position 5
because the fenced attempt consumed nothing.

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"start"}]}
< {"v":1,"t":"append.receipt","id":"r3","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"recorded","pos":2}]}
> {"v":1,"t":"turn.take","id":"r4","session":"s1"}
< {"v":1,"t":"turn.state","id":"r4","session":"s1","turn":2,"state":"running"}
> {"v":1,"t":"append","id":"r5","session":"s1","turn":1,"records":[{"rid":"m2","kind":"message","body":"late output"}]}
< {"v":1,"t":"append.receipt","id":"r5","session":"s1","turn":1,"receipts":[{"rid":"m2","status":"fenced"}]}
> {"v":1,"t":"append","id":"r6","session":"s1","turn":2,"records":[{"rid":"m2","kind":"message","body":"new owner"}]}
< {"v":1,"t":"append.receipt","id":"r6","session":"s1","turn":2,"receipts":[{"rid":"m2","status":"recorded","pos":5}]}
> {"v":1,"t":"watch","id":"w1","session":"s1","cursor":0}
< {"v":1,"t":"ready","id":"w1","session":"s1","head":5}
< {"v":1,"t":"turn.event","id":"w1","session":"s1","pos":1,"turn":1,"state":"running"}
< {"v":1,"t":"item","id":"w1","session":"s1","pos":2,"turn":1,"rid":"m1","kind":"message","body":"start"}
< {"v":1,"t":"turn.event","id":"w1","session":"s1","pos":3,"turn":1,"state":"superseded"}
< {"v":1,"t":"turn.event","id":"w1","session":"s1","pos":4,"turn":2,"state":"running"}
< {"v":1,"t":"item","id":"w1","session":"s1","pos":5,"turn":2,"rid":"m2","kind":"message","body":"new owner"}
< {"v":1,"t":"live","id":"w1","session":"s1"}
```

### 5. Watch from the start

`ready`, the replay in position order, then `live`.

```text
> {"v":1,"t":"session.create","id":"r1","source":"codex","external_id":"thread/7"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"a","kind":"message","body":"one"},{"rid":"b","kind":"tool_result","body":"two\nlines"}]}
< {"v":1,"t":"append.receipt","id":"r3","session":"s1","turn":1,"receipts":[{"rid":"a","status":"recorded","pos":2},{"rid":"b","status":"recorded","pos":3}]}
> {"v":1,"t":"watch","id":"w1","session":"s1","cursor":0}
< {"v":1,"t":"ready","id":"w1","session":"s1","head":3}
< {"v":1,"t":"turn.event","id":"w1","session":"s1","pos":1,"turn":1,"state":"running"}
< {"v":1,"t":"item","id":"w1","session":"s1","pos":2,"turn":1,"rid":"a","kind":"message","body":"one"}
< {"v":1,"t":"item","id":"w1","session":"s1","pos":3,"turn":1,"rid":"b","kind":"tool_result","body":"two\nlines"}
< {"v":1,"t":"live","id":"w1","session":"s1"}
```

### 6. Incremental delivery after `live`

The receipt comes first, then the item for the open watch.

```text
> {"v":1,"t":"session.create","id":"r1","source":"codex","external_id":"thread/7"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"watch","id":"w1","session":"s1","cursor":0}
< {"v":1,"t":"ready","id":"w1","session":"s1","head":1}
< {"v":1,"t":"turn.event","id":"w1","session":"s1","pos":1,"turn":1,"state":"running"}
< {"v":1,"t":"live","id":"w1","session":"s1"}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"m1","kind":"note","body":"café ☕"}]}
< {"v":1,"t":"append.receipt","id":"r3","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"recorded","pos":2}]}
< {"v":1,"t":"item","id":"w1","session":"s1","pos":2,"turn":1,"rid":"m1","kind":"note","body":"café ☕"}
```

### 7. Resume from a cursor

The client saw up to position 2 before the stream dropped. It reconnects with `cursor` 2 and gets
position 3 only.

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"a"},{"rid":"m2","kind":"message","body":"b"}]}
< {"v":1,"t":"append.receipt","id":"r3","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"recorded","pos":2},{"rid":"m2","status":"recorded","pos":3}]}
> {"v":1,"t":"watch","id":"w2","session":"s1","cursor":2}
< {"v":1,"t":"ready","id":"w2","session":"s1","head":3}
< {"v":1,"t":"item","id":"w2","session":"s1","pos":3,"turn":1,"rid":"m2","kind":"message","body":"b"}
< {"v":1,"t":"live","id":"w2","session":"s1"}
```

### 8. A child session

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"main"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"session.create","id":"r2","source":"claude-code","external_id":"sub-1","parent":"s1"}
< {"v":1,"t":"session.created","id":"r2","session":"s2","created":true,"parent":"s1"}
> {"v":1,"t":"session.create","id":"r3","source":"claude-code","external_id":"sub-1","parent":"s1"}
< {"v":1,"t":"session.created","id":"r3","session":"s2","created":false,"parent":"s1"}
```

### 9. Refused lines

A line the plugin cannot read as a message is answered with `id: null`. The stream stays open.

```text
> {"v":2,"t":"turn.take","id":"r1","session":"s1"}
< {"v":1,"t":"error","id":null,"code":"unsupported_version","message":"unsupported version 2"}
> {"v":1,"t":"turn.take","id":"r1","session":"s1","extra":1}
< {"v":1,"t":"error","id":null,"code":"bad_request","message":"unknown field \"extra\""}
> {"v":1,"v":1,"t":"turn.take","id":"r1","session":"s1"}
< {"v":1,"t":"error","id":null,"code":"bad_request","message":"duplicate key \"v\""}
> {"v":1,"t":"watch","id":"w1","session":"s1","cursor":1.5}
< {"v":1,"t":"error","id":null,"code":"bad_request","message":"number must be an integer in 0..=2^53-1"}
> ]
< {"v":1,"t":"error","id":null,"code":"bad_request","message":"invalid JSON: unexpected byte"}
```

### 10. Errors of well-formed requests

These carry the request id.

```text
> {"v":1,"t":"turn.take","id":"r1","session":"s9"}
< {"v":1,"t":"error","id":"r1","code":"unknown_session","message":"unknown session"}
> {"v":1,"t":"session.create","id":"r2","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r2","session":"s1","created":true}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"x"}]}
< {"v":1,"t":"error","id":"r3","code":"unknown_turn","message":"turn was never issued"}
> {"v":1,"t":"watch","id":"w1","session":"s1","cursor":4}
< {"v":1,"t":"error","id":"w1","code":"bad_request","message":"cursor is beyond head"}
> {"v":1,"t":"watch.stop","id":"w1"}
< {"v":1,"t":"error","id":"w1","code":"unknown_watch","message":"no such watch"}
```

### 11. Stop a watch

After `watch.stopped` nothing more arrives for that watch; the append still gets its receipt.

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"watch","id":"w1","session":"s1","cursor":0}
< {"v":1,"t":"ready","id":"w1","session":"s1","head":1}
< {"v":1,"t":"turn.event","id":"w1","session":"s1","pos":1,"turn":1,"state":"running"}
< {"v":1,"t":"live","id":"w1","session":"s1"}
> {"v":1,"t":"watch.stop","id":"w1"}
< {"v":1,"t":"watch.stopped","id":"w1"}
> {"v":1,"t":"append","id":"r3","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"quiet"}]}
< {"v":1,"t":"append.receipt","id":"r3","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"recorded","pos":2}]}
```

### 12. Ending turns

An ended turn is `idle`. Later, ending it again reports `superseded` once a newer turn exists, and
appends under it are fenced.

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"abc-123"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"turn.take","id":"r2","session":"s1"}
< {"v":1,"t":"turn.state","id":"r2","session":"s1","turn":1,"state":"running"}
> {"v":1,"t":"turn.end","id":"r3","session":"s1","turn":1}
< {"v":1,"t":"turn.state","id":"r3","session":"s1","turn":1,"state":"idle"}
> {"v":1,"t":"turn.take","id":"r4","session":"s1"}
< {"v":1,"t":"turn.state","id":"r4","session":"s1","turn":2,"state":"running"}
> {"v":1,"t":"turn.end","id":"r5","session":"s1","turn":1}
< {"v":1,"t":"turn.state","id":"r5","session":"s1","turn":1,"state":"superseded"}
> {"v":1,"t":"append","id":"r6","session":"s1","turn":1,"records":[{"rid":"m1","kind":"message","body":"too late"}]}
< {"v":1,"t":"append.receipt","id":"r6","session":"s1","turn":1,"receipts":[{"rid":"m1","status":"fenced"}]}
```

### 13. A child with a conflicting parent

```text
> {"v":1,"t":"session.create","id":"r1","source":"claude-code","external_id":"main"}
< {"v":1,"t":"session.created","id":"r1","session":"s1","created":true}
> {"v":1,"t":"session.create","id":"r2","source":"claude-code","external_id":"sub-1","parent":"s1"}
< {"v":1,"t":"session.created","id":"r2","session":"s2","created":true,"parent":"s1"}
> {"v":1,"t":"session.create","id":"r3","source":"claude-code","external_id":"sub-1"}
< {"v":1,"t":"error","id":"r3","code":"conflict","message":"session exists with a different parent"}
> {"v":1,"t":"session.create","id":"r4","source":"claude-code","external_id":"orphan","parent":"s7"}
< {"v":1,"t":"error","id":"r4","code":"unknown_session","message":"unknown parent session"}
```
