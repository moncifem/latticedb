# Claude Code pack

The harness that tells Claude Code how to use the Lattice MCP server: subscribe before acting, keep the
cursor, write with `expected_version`, and treat stored text as data. It is the first client pack; the
harness text is one source, shared by every future pack.

## Files

| File | Kind | What it is |
|---|---|---|
| `harness.src.md` | source | The harness text. Edit this one. |
| `harness.generated.md` | generated | The source wrapped in begin and end markers. Do not edit it. |
| `mcp.example.json` | example | An MCP entry for the HTTP endpoint, with the token read from `LATTICE_TOKEN`. |
| `mcp.stdio.example.json` | example | An MCP entry that runs `lattice mcp`, a stdio proxy to the endpoint, with the token file read from `LATTICE_TOKEN_FILE`. See the stdio variant below. |
| `settings.example.json` | example | The hook settings. Merge its `hooks` block into the project's `.claude/settings.json`. |
| `hooks/session-start.sh` | hook | Prints a bounded digest of open tasks and the board head at session start. |
| `hooks/prompt-submit.sh` | hook | Prints the board events since the saved cursor on each prompt. |

Regenerate after a source edit, and check for drift:

```bash
python3 tools/gen_harness_packs.py
python3 tools/gen_harness_packs.py --check
python3 -m unittest discover -s tools -p "test_gen_harness_packs.py"
```

The tests fail when the text names a tool that `crates/lattice-sdk/src/mcp.rs` does not serve, when a
generated file is stale, and when the text holds an email address, an absolute user path or a token.

## Install by hand (until the installer, issue #536)

1. Append the block in `harness.generated.md` to the project's `CLAUDE.md`. The markers let you replace
   the block later without touching the rest of the file.
2. Copy `mcp.example.json` to `.mcp.json` at the project root. Replace the port with the one you passed
   to `--gateway-listen`.
3. Export `LATTICE_TOKEN` with the bearer token for your account. The token is never written into a file.
   Claude Code asks you to approve a project-scoped server the first time it sees `.mcp.json`.

The server side is `latticed --gateway-listen 127.0.0.1:PORT --gateway-tokens FILE`. The endpoint is
`POST /mcp`. Plaintext is accepted only on a loopback address; a non-loopback `--gateway-listen` needs
`--tls-cert`, `--tls-key` and `--gateway-tokens` as well (TLS landed in `4b297405`). The endpoint is experimental
(see `docs/mcp-v0.md`).

## Stdio variant

Use `mcp.stdio.example.json` when the client starts the server as a child process and speaks MCP over
standard input and output. The entry runs `lattice mcp`, which sends each message as one `POST /mcp`
request to the gateway and writes each reply as one line.

1. Put the `lattice` binary on the `PATH` of the process that starts the client.
2. Write the bearer token to a file that only its owner can read (mode 0600), and export
   `LATTICE_TOKEN_FILE` with its path. `lattice mcp` refuses `--token`, so the token never goes on a
   command line.
3. Copy `mcp.stdio.example.json` to `.mcp.json` at the project root. Replace the port with the one you
   passed to `--gateway-listen`.

Limits:

- `lattice mcp` accepts only `http://` on a loopback address (`127.0.0.1`, `localhost` or `[::1]`) with
  the path `/mcp`. It refuses `https://` and remote hosts, because the CLI has no TLS client yet.
- For a gateway on another host with TLS, the candidate is the HTTP entry in `mcp.example.json`, if the
  client trusts the gateway's certificate authority. That path is not tested here.
- The entry has no `type` field. The Claude Code MCP page (https://code.claude.com/docs/en/mcp, read on
  2026-10-31) says an entry with no `type` is read as a stdio server, and that `${VAR}` references work
  in `args`. A project-scoped `.mcp.json` server needs approval the first time the client sees it.

## Status

- Verified: the generator, the drift check and the name and hygiene checks run in the unit tests. The
  manual check recorded in `docs/mcp-v0.md` used the Claude Code CLI with an HTTP entry and a bearer header.
- Verified: the two board hooks (`hooks/session-start.sh`, `hooks/prompt-submit.sh`) pass their unit
  tests against a stub board (`tools/test_agent_hooks.py`). The tests cover the cursor order, the byte
  limit, forged rows and control characters in stored text, the nonce frame, the owner-only cursor file,
  exit 0 within a time bound when the board is unreachable or hangs, and that a hung board and its child
  processes are gone afterwards.
- Timing: each board call has `BOARD_TIMEOUT` seconds (default 3) before its process group is ended, then
  0.5 seconds of grace. `PGCONNECT_TIMEOUT` (default 3) bounds the server connect. The settings example
  sets a 10 second hook timeout, which covers the two calls of the digest.
- Stored text: terminal control characters, C1 controls, bidirectional overrides and the line and
  paragraph separators are removed; tab and newline are kept. Each block sits between begin and end
  lines carrying a random nonce, so stored text cannot close it early.
- Event ids and the cursor: the board hands out event ids from one counter row that every write
  transaction updates, and a commit that loses that row to another writer is rejected (first committer
  wins, `formats/isolation.md`). So a committed id is never below an id already committed, and the
  head that the hooks read covers every event at or below it. This is read from the board script and the
  schema, and the hook tests use a stub, so no concurrent-writer test covers it here. If the counter
  stopped serialising writers, an event could commit below a saved cursor and be missed.
- When a prompt's events do not fit in one block, the cursor still moves to the head and the hook prints
  the old cursor, so the rest can be read with `board since`. The events in between are not replayed.
- Not verified: an automated run of an agent that follows the harness text, and the hooks inside a live
  session. No such test exists.
- Hooks, opt-in: merge the `hooks` block of `settings.example.json` into the project's
  `.claude/settings.json`. The hooks read the board through the `board` helper (`BOARD_BIN` overrides
  it). The helper needs the usual `LATTICE_BOARD_URL` or `PG*` variables in the environment that starts
  the client. The cursor lives in `$CLAUDE_PROJECT_DIR/.lattice/board-cursor` (mode 0600,
  directory 0700). Add `.lattice/` to the project's `.gitignore`. The hooks never write to the board.
- Checked: `tools/test_claude_code_examples.py` parses both MCP examples, finds no token literal in them,
  checks that the stdio example's flags appear in the usage text of `lattice mcp` (read from
  `crates/lattice-cli/src/mcp_proxy.rs`), and checks that the table above lists every file in this
  directory. No client has been run with either example.
- Not done: session presence, the installer, and other client packs.
