# Lattice Python client

Status: experimental. Second client slice of WP51 ([#253](https://github.com/moncifem/latticedb/issues/253)),
after the [TypeScript client](../typescript). The formats it implements are unaccepted candidates
([sync protocol v1](../../formats/sync-protocol-v1.md), [change feed v1](../../formats/change-feed-v1.md));
expect breaking changes. The package is not published and has no `pyproject.toml`.

It is a library for the wire codecs and the client state machine. It needs Python 3.10 or later, uses only
the standard library, has no network code and no generated parts: the code is written by hand and kept
identical to the Rust implementation by shared test vectors. It is typed (`py.typed`; `mypy --strict`
passes on the package and its tests).

## What is in it

| Module | Contents |
|---|---|
| `lattice_client/sync.py` | Encode and decode of every sync frame (`Hello`, `Welcome`, `Subscribe`, `Events`, `Ack`, `Error`, `Ping`, `Pong`), `negotiate`, the field limits. A port of `lattice_api::sync`. |
| `lattice_client/feed.py` | The 12-byte cursor, its order, and encode and decode of a change event. A port of `lattice_core::feed`. |
| `lattice_client/client.py` | `SyncClient`, a sans-I/O state machine: handshake, resume from the last acknowledged cursor, duplicate dropping, ack batching, ping and timeouts. A port of `lattice_sdk::client`. |
| `lattice_client/_bytes.py` | The range checks and the bounded reader and writer the codecs share (private). |

Everything public is re-exported from `lattice_client`.

### Decoding is strict

The rules are the Rust ones, in the same order, so the same bytes fail with the same error:

- the frame type is judged from the first byte and the length from the first five, before the payload is
  read; a length above the limit (1 MiB, or the one `Welcome` announced) is refused;
- every field is checked against its bounds; text must be valid UTF-8 without control characters (Unicode
  category Cc; a byte order mark is kept as data); reserved values (subscription 0, version 0, error codes
  outside 1 to 7) are refused;
- bytes left after the last field are an error, and so is any non-canonical form, so
  `encode(decode(data).message) == data` for every frame that decodes;
- fewer bytes than a whole frame return `None` ("wait for more"), not an error.

Errors are `SyncError`, `FeedDecodeError`, `FeedError` and `ClientError`. Each has a `kind` that matches the
Rust variant (`bad_length`, `trailing`, `invalid_field` and so on) and a `detail` where Rust carries a value
(the length, the type byte, the field name), otherwise `None`.

### Numbers

Python integers have no width, so every field is range checked: the encoders refuse a value outside its
field (`u16`, `u32`, `u64`, the `u128` author) instead of wrapping, and `bool`, `float` and negative numbers
are never accepted as a number. Clock values given to the client (`now`) must be in `u64` range too; this is
the one error kind with no Rust twin, `ClientError("invalid_time")`, because Rust types the argument.
Cursors and event bodies are `bytes`; `bytearray` and `memoryview` are accepted as input, and decoded values
are immutable copies that never alias the input buffer.

## Use

```python
from lattice_client import SyncClient, default_config, decode, encode, Send, Deliver

client = SyncClient(default_config("my-agent"))
now = 0                              # logical ticks, you own the clock

for action in client.connect(now):
    if isinstance(action, Send):
        transport.write(encode(action.frame))

# On bytes from the transport (keep a buffer, call decode until it returns None):
decoded = decode(buffer)
if decoded is not None:
    buffer = buffer[decoded.length:]
    for action in client.handle(decoded.message, now):
        ...                          # Send -> write it; Deliver -> hand to the application, once and in order
                                     # Ready -> handshake done; Close -> close the transport

sub, actions = client.subscribe("ws/main", b"")   # empty cursor: from the start
client.ack(sub, cursor)              # after you processed an event
client.tick(now)                     # now and then: pings and timeouts
client.disconnected()                # transport dropped; client.reconnect(now) resumes
```

Actions are the dataclasses `Send`, `Ready`, `Deliver`, `SubscriptionFailed` and `Close`.

After a reconnect the client resubscribes with the last cursor you acknowledged. The server may replay events
you already saw; the client drops every event whose cursor is not after the last one it delivered (a longer
cursor is later, equal lengths compare byte by byte, as in the Rust client).

## Conformance with the Rust implementation

`conformance/vectors.json` at the repository root is written by the Rust side
(`crates/lattice-api/src/conformance.rs`) and is shared with the TypeScript client; this client does not
regenerate it. It holds valid frames with their decoded form, invalid frames with the expected error kind,
incomplete inputs, messages the encoder must refuse, the version negotiation table, and the same for cursors
and change events. `tests/test_conformance.py` requires this client to decode every vector to the same value,
to encode it back to byte-identical output, and to fail on every invalid vector with the same kind and
detail.

To change a format deliberately, update the Rust codec, run `tools/update-conformance-vectors.sh`, review the
diff of the vector file, and fix both clients until their tests pass again.

`tests/test_property.py` adds randomised tests with a seeded generator (case `i` uses seed `i + 1`, and a
failure names its seed): encode then decode is the identity and canonical, mutated and random bytes never
crash the decoder and anything it accepts is canonical, cursor bytes order like cursors, and the client
delivers each event exactly once against a model server that honours cursors. `tests/test_client.py` holds
the scenarios of the Rust client tests. `LATTICE_PY_CASES` sets the number of random cases (CI uses 20,000).

## Build and test

Python 3.10 or later, nothing to install. From this directory:

```bash
python3 -m unittest discover
```

CI runs the same command on Python 3.10 and 3.13 in `.github/workflows/clients-python.yml`, and checks that
importing the package loads only standard library modules.

## What it does not do

- No transport: no WebSocket, TCP or TLS code, no framing buffer, no timers. You move bytes and call `tick`.
  There is no live server to connect to yet.
- No authentication, no scope grammar, no query language, no flow control beyond `Ack`.
- It does not interpret event bodies as anything but bytes; `decode_event` reads the change feed event layout
  if your bodies use it.
- No packaging: no `pyproject.toml`, no wheel, nothing on PyPI. Run it from this directory or add it to
  `PYTHONPATH`.
- No Go client, no generator and no schema-derived types. WP51 asks for generated clients; this slice is the
  hand-written Python codec those would be checked against.
- Not the agent plugin contract (`formats/plugin-contract-v1.md`, a line-delimited JSON format); that codec
  exists only in Rust so far.
- No cross-version compatibility promise: the formats are unaccepted candidates.
