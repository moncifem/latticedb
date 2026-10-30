# Lattice TypeScript client

Status: experimental. First slice of WP51 ([#253](https://github.com/moncifem/latticedb/issues/253)).
The formats it implements are unaccepted candidates ([sync protocol v1](../../formats/sync-protocol-v1.md),
[change feed v1](../../formats/change-feed-v1.md)); expect breaking changes. The package is private and
is not published.

It is a library for the wire codecs and the client state machine. It has no runtime dependencies, no
network code and no generated parts: the code is written by hand and kept identical to the Rust
implementation by shared test vectors.

## What is in it

| Module | Contents |
|---|---|
| `src/sync.ts` | Encode and decode of every sync frame (`Hello`, `Welcome`, `Subscribe`, `Events`, `Ack`, `Error`, `Ping`, `Pong`), `negotiate`, the field limits. A port of `lattice_api::sync`. |
| `src/feed.ts` | The 12-byte cursor, its order, and encode and decode of a change event. A port of `lattice_core::feed`. |
| `src/client.ts` | `SyncClient`, a sans-I/O state machine: handshake, resume from the last acknowledged cursor, duplicate dropping, ack batching, ping and timeouts. A port of `lattice_sdk::client`. |
| `src/bytes.ts` | Hex helpers and the bounded reader and writer the codecs share. |

All of it is exported from `src/index.ts`.

### Decoding is strict

The rules are the Rust ones, in the same order, so the same bytes fail with the same error:

- the frame type is judged from the first byte and the length from the first five, before the payload
  is read; a length above the limit (1 MiB, or the one `Welcome` announced) is refused;
- every field is checked against its bounds; text must be valid UTF-8 without control characters
  (a byte order mark is kept as data); reserved values (subscription 0, version 0, error codes
  outside 1 to 7) are refused;
- bytes left after the last field are an error, and so is any non-canonical form, so
  `encode(decode(bytes))` equals the input for every frame that decodes;
- fewer bytes than a whole frame return `null` ("wait for more"), not an error.

Errors are `SyncError`, `FeedDecodeError`, `FeedError` and `ClientError`. Each has a `kind` that matches
the Rust variant (`bad_length`, `trailing`, `invalid_field` and so on) and, where Rust carries a value, a
`detail` (the length, the type byte, the field name).

### Numbers

Every `u64` is a `bigint` (LSNs, nonces, HLC wall time, turn ids, client ticks) and the `u128` author is a
`bigint` too. `u16` and `u32` fields are `number`s. The encoders refuse a value outside its field instead
of wrapping. Cursors and event bodies are `Uint8Array`; decoded values are copies and never alias the
input buffer.

## Use

```ts
import { SyncClient, decode, defaultConfig, encode } from "@lattice/client";

const client = new SyncClient(defaultConfig("my-agent"));
let now = 0n;                       // logical ticks, you own the clock

for (const action of client.connect(now)) {
  // { type: "send", frame } -> write encode(frame) to your transport
}

// On bytes from the transport (keep a buffer, call decode until it returns null):
const decoded = decode(buffer);
if (decoded !== null) {
  const actions = client.handle(decoded.message, now);
  // "deliver" -> hand the event to your application, once and in order
  // "send"    -> write it; "close" -> close the transport; "ready" -> handshake done
}

const { sub, actions } = client.subscribe("ws/main", new Uint8Array(0)); // empty cursor: from the start
client.ack(sub, cursor);            // after you processed an event
client.tick(now);                   // now and then: pings and timeouts
client.disconnected();              // transport dropped; client.reconnect(now) resumes
```

After a reconnect the client resubscribes with the last cursor you acknowledged. The server may replay
events you already saw; the client drops every event whose cursor is not after the last one it delivered
(longer cursors are later, equal lengths compare byte by byte, as in the Rust client).

## Conformance with the Rust implementation

`conformance/vectors.json` at the repository root is written by the Rust side
(`crates/lattice-api/src/conformance.rs`). It holds valid frames with their decoded form, invalid frames
with the expected error kind, incomplete inputs, messages the encoder must refuse, the version
negotiation table, and the same for cursors and change events. The bytes of each vector are written by
hand from the format documents; the Rust test checks them against the Rust codecs and fails if the
checked-in file differs from what it renders. `test/conformance.test.ts` requires this client to decode
every vector to the same value, to encode it back to byte-identical output, and to fail on every invalid
vector with the same kind and detail.

To change a format deliberately, update the Rust codec, run `tools/update-conformance-vectors.sh`, review
the diff of the vector file, and fix this client until its tests pass again.

`test/property.test.ts` adds randomised tests with a seeded generator (case `i` uses seed `i + 1`, and a
failure names its seed): encode then decode is the identity and canonical, mutated and random bytes never
crash the decoder and anything it accepts is canonical, cursor bytes order like cursors, and the client
delivers each event exactly once against a model server that honours cursors. `LATTICE_TS_CASES` sets the
number of cases (CI uses 20,000).

## Build and test

Node 22 or later. The only dependencies are development tools, pinned exactly: `typescript` and
`@types/node`.

```bash
cd clients/typescript
npm ci --ignore-scripts
npx tsc -p tsconfig.json               # strict; writes dist/
node --test "dist/test/*.test.js"
```

CI runs the same steps in `.github/workflows/clients-typescript.yml`.

## What it does not do

- No transport: no WebSocket, TCP or TLS code, no framing buffer, no timers. You move bytes and call
  `tick`. There is no live server to connect to yet.
- No authentication, no scope grammar, no query language, no flow control beyond `Ack`.
- It does not interpret event bodies as anything but bytes; `decodeEvent` reads the change feed event
  layout if your bodies use it.
- No Go client, no generator and no schema-derived types. WP51 asks for generated clients; this slice is
  the hand-written TypeScript codec those would be checked against. A hand-written Python client with the same
  vectors is in [`clients/python`](../python).
- Not the agent plugin contract (`formats/plugin-contract-v1.md`, a line-delimited JSON format); that
  codec exists only in Rust so far.
- No cross-version compatibility promise: the formats are unaccepted candidates.
