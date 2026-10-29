# MCP server v0

First slice of the Model Context Protocol server (WP52, issue #254, requirement P3-13): a sans-I/O
message codec, three scoped tools and a grant layer, in `crates/lattice-sdk/src/mcp.rs`. There is no
transport yet, so nothing listens on a socket and no phase exit criterion is claimed.

## Protocol revision

The module targets MCP specification revision **2026-07-28**
(<https://modelcontextprotocol.io/specification/2026-07-28>, read on 2026-10-29). That revision has no
handshake: every request must carry `_meta` with `io.modelcontextprotocol/protocolVersion` and
`io.modelcontextprotocol/clientCapabilities`. The server is conformant to that: a request without
them is refused. For clients that still open with the handshake of **2025-11-25**, the server is
dual-era, as the revision allows: a connection that opens with a successful `initialize` is served
with legacy semantics. The server keeps one piece of per-connection state, the mode (`Stateless` at
start, `Legacy` after `initialize`). Only `initialize` sets it; a missing `_meta` never does. Use one
`Server` per connection.

## Supported subset

| Method | Behaviour |
|---|---|
| `initialize` | Legacy. Requires `protocolVersion`, `capabilities`, `clientInfo`; replies with version `2025-11-25`, the `tools` capability and `serverInfo`, and switches the connection to legacy mode. |
| `notifications/initialized` and any other notification | Accepted, never answered. |
| `ping` | Empty `complete` result. |
| `server/discover` | `supportedVersions` (`2026-07-28`; plus `2025-11-25` on a legacy connection), `capabilities.tools`, `serverInfo` in `_meta`. |
| `tools/list` | The tools the caller holds a grant for, in fixed order, each with its `inputSchema`. No pagination. |
| `tools/call` | Named tool and arguments; result is one text content block with `isError`. |
| anything else | `-32601` method not found. |

Every result carries `resultType: "complete"`.

### Per-request `_meta`

| Connection | `_meta` missing | `_meta` not an object | version or capabilities member missing or mistyped | version not accepted |
|---|---|---|---|---|
| Stateless (default) | `-32602` | `-32602` | `-32602` | `-32022` |
| Legacy (after `initialize`) | accepted | `-32602` | capabilities mistyped: `-32602`; absent: accepted | `-32022` |

This applies to `ping`, `server/discover`, `tools/list` and `tools/call`. A stateless connection
accepts only `2026-07-28` (`data.supported` is `["2026-07-28"]`); a legacy connection also accepts
`2025-11-25` (`data.supported` lists both). `data.requested` echoes the requested version only when
it is a short plain string. Unknown methods are `-32601` regardless of `_meta`.

### Message rules

- Strict JSON-RPC 2.0: `jsonrpc` must be `"2.0"`; the only members are `jsonrpc`, `id`, `method`,
  `params`; `id` is a string or an integer, never null; `params` is an object.
- A JSON array (a batch) is refused with `-32600`: the revision defines no batching.
- A response sent to the server is refused (`-32600`); the server issues no requests of its own.
- Bounds: 256 KiB per message, nesting depth 12, 64 members per array or object, 4096 values per
  message, 65536-character `value`, 1 MiB tool output. Duplicate object keys, lone surrogates, raw control
  characters, leading zeros and a byte order mark are parse errors (`-32700`).
- The JSON reader is local to `mcp.rs`. It is separate from the plugin-contract grammar in
  `contract.rs`, which has no negative numbers, no fractions and a depth of six.

## Tools

All inputs are objects with `additionalProperties: false`.

| Tool | Required | Optional |
|---|---|---|
| `lattice_read` | `scope`, `table` | `key` (omit for a page), `limit` 1..1000 |
| `lattice_write` | `scope`, `table`, `key`, `value` (up to 64 KiB) | `expected_version` |
| `lattice_subscribe` | `scope`, `table` | `after` (resume cursor) |

`scope` is a path of `/`-separated segments of `[A-Za-z0-9_-]` (no `.`, no empty segment, at most 128
characters); `table` is `[A-Za-z0-9_]` up to 64 characters. `minLength` and `maxLength` count
characters, as JSON Schema does (the 256 KiB message bound limits the bytes). `key` may contain no
control characters; `value` may contain none except tab, line feed and carriage return. Integer
arguments follow JSON Schema 2020-12: `10`, `10.0` and `1e1` are all the integer 10; `1.5` is a
wrong type. (JSON-RPC ids stay integer or string only.)

Handlers are the `ToolBackend` trait: `call(&mut self, &Caller, &ToolCall)`. The backend receives only
validated, typed calls, and still owns row-level policy (`BackendError::PolicyDenied`).

A backend's output is bounded by its type: `ToolOutput::new` refuses text over 1 MiB, and
`ToolOutput::or_notice` swaps it for a fixed notice. The server never fails a call after the backend
returns success, so a write that was applied is never reported as an error that a client would retry.
A write backend must build or bound its output before it commits, or return the notice.
`lattice_subscribe` returns what the backend returns for the cursor; streaming notifications are not
implemented.

## Scoping

Every call is made by a `Caller { id, grants }`. A `Grant` pairs a tool with a scope subtree (`a/b`
covers `a/b` and `a/b/c`, not `a/bc`; `*` covers every scope). Order of checks in `tools/call`:

1. tool exists, else `-32602 unknown tool`;
2. caller holds any grant for the tool, else `40301 tool not granted`;
3. arguments valid against the schema, else `-32602` (`data.field` names the schema property at fault);
4. a grant covers the target scope, else `40302 scope denied`;
5. only then the backend is called.

Refusal messages are fixed strings. They never contain caller-supplied names or values or anything
from the store. A backend `PolicyDenied` is also `40302`, `Unavailable` is `50001`, and a version
`Conflict` is a tool execution error (`isError: true`), which the specification reserves for failures
the model can correct. Application codes sit outside `-32768..-32000`, as the specification advises.

## Not implemented

- Transports (stdio framing, Streamable HTTP) and any socket or process handling.
- Authentication and authorization framework: the transport supplies the `Caller`; nothing here
  verifies identity or issues grants.
- Resources, prompts, completion, logging, sampling, elicitation and roots.
- Tasks, multi round-trip requests (`input_required`), progress, cancellation, caching hints and
  pagination cursors.
- `subscriptions/listen` and `notifications/tools/list_changed`.
- Output schemas, structured content and tool annotations.
- Rate limiting.
- A real backend: the row store and its policy are not connected yet.

## Evidence

Unit and property tests live beside the code (`cargo test -p lattice-sdk mcp`): byte vectors per
message, a schema table, a grant table, a `_meta` table, a dual-era connection test, a mock backend that
records calls to prove it is never reached on denial or invalid input (and that an oversize write
output is applied once and reported as success), arbitrary-bytes and valid-request properties, and an
injection test.
The fuzz target is `fuzz/fuzz_targets/mcp_message.rs` with seeds in `fuzz/seeds/mcp_message/`; it is
not yet in a scheduled fuzz matrix.
