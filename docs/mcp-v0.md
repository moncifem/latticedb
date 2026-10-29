# MCP server v0

First slice of the Model Context Protocol server (WP52, issue #254, requirement P3-13): a sans-I/O
message codec, three scoped tools and a grant layer, in `crates/lattice-sdk/src/mcp.rs`. There is no
transport yet, so nothing listens on a socket and no phase exit criterion is claimed.

## Protocol revision

The module targets MCP specification revision **2026-07-28**
(<https://modelcontextprotocol.io/specification/2026-07-28>, read on 2026-10-29). That revision has no
handshake: every request carries `_meta` with `io.modelcontextprotocol/protocolVersion`. For clients
that still open with the handshake of **2025-11-25**, the server also answers `initialize` and
swallows `notifications/initialized` (the revision allows dual-era servers).

## Supported subset

| Method | Behaviour |
|---|---|
| `initialize` | Legacy. Requires `protocolVersion`, `capabilities`, `clientInfo`; replies with version `2025-11-25`, the `tools` capability and `serverInfo`. |
| `notifications/initialized` and any other notification | Accepted, never answered. |
| `ping` | Empty `complete` result. |
| `server/discover` | `supportedVersions`, `capabilities.tools`, `serverInfo` in `_meta`. |
| `tools/list` | The tools the caller holds a grant for, in fixed order, each with its `inputSchema`. No pagination. |
| `tools/call` | Named tool and arguments; result is one text content block with `isError`. |
| anything else | `-32601` method not found. |

Every result carries `resultType: "complete"`. A request whose `_meta` names another protocol version
gets `-32022` with `data.supported`. Requests without `_meta` are served as legacy-era requests; the
module keeps no session, so it does not enforce that `initialize` came first (a transport may).

### Message rules

- Strict JSON-RPC 2.0: `jsonrpc` must be `"2.0"`; the only members are `jsonrpc`, `id`, `method`,
  `params`; `id` is a string or an integer, never null; `params` is an object.
- A JSON array (a batch) is refused with `-32600`: the revision defines no batching.
- A response sent to the server is refused (`-32600`); the server issues no requests of its own.
- Bounds: 256 KiB per message, nesting depth 12, 64 members per array or object, 4096 values per
  message, 64 KiB `value`, 1 MiB tool output. Duplicate object keys, lone surrogates, raw control
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
bytes); `table` is `[A-Za-z0-9_]` up to 64 bytes. Integers must be JSON integers.

Handlers are the `ToolBackend` trait: `call(&mut self, &Caller, &ToolCall)`. The backend receives only
validated, typed calls, and still owns row-level policy (`BackendError::PolicyDenied`).
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
message, a schema table, a grant table, a mock backend that records calls to prove it is never reached
on denial or invalid input, arbitrary-bytes and valid-request properties, and an injection test.
The fuzz target is `fuzz/fuzz_targets/mcp_message.rs` with seeds in `fuzz/seeds/mcp_message/`; it is
not yet in a scheduled fuzz matrix.
