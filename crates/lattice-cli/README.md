# lattice-cli

Layer L7.

**Responsibility.** Admin and scripting command line tool.

**Must not.** Contain server logic.

Allowed workspace dependencies: lattice-abi, lattice-sdk, lattice-core, lattice-seams, lattice-lsl,
lattice-api, latticed.

`lattice account init|add|list|remove|token create|token revoke|token list` provisions the SCRAM
verifier file, the decoy secret, the per-table policy file and the bearer-token file that `latticed`
reads, with owner-only files and secrets from stdin or generated and shown once. It reuses the file
rules of `latticed::files` and the verifier and token code of `lattice-api`. See
[the listener guide](../latticed/README.md#provisioning-with-lattice-account) and
`lattice account --help`.

`lattice watch table/NAME --url ws://127.0.0.1:PORT --token-file FILE (--from CURSOR | --from-start | --cursor-file FILE)`
connects to the sync gateway on a loopback address, subscribes to the table and prints one line per
event: `CURSOR AUTHOR TABLE KEY OPERATION`, with `CURSOR` as 24 lowercase hex digits, the form `--from`
takes. Each event is printed, then acknowledged, then its cursor is saved, so a crash re-prints an
event on the next start and never skips one. The URL must be `ws://` on a loopback address with a
port (the client has no TLS yet). The token comes from a file, which must be owner-only like the other
credential files (a regular file, not a symlink, mode 0600 or stricter), or from standard input with
`-`; a token on the command line is refused.

`--cursor-file FILE` keeps the saved cursor in FILE. The directory of FILE must exist and must not be
writable by group or others; FILE must be owner-only (mode 0600) and is replaced atomically (temporary
file, sync, rename). A missing file starts at the beginning of the table; a file that does not hold
exactly one cursor is refused and left as it is. Two watches on one cursor file are refused: the watch
holds `FILE.lock` for its whole run, and a second one exits at once. Without the flag the watch starts
from `--from` or `--from-start` and keeps no cursor on disk.

A dropped connection is retried from the last saved cursor. The retry policy is the SDK's reconnect
supervisor (`lattice_sdk::reconnect`), with full jitter: the wait before each retry is random, between
one second and a ceiling that starts at one second, doubles with each failure and stops at eight seconds.
A connection that delivers no event counts as a failure, and three failures in a row stop the watch with
status 1. An event counts as delivered once it is printed, acknowledged and saved; that resets the count
and the ceiling, so the next wait starts again at one second. A refused token (HTTP 401 or 403), a close
with code 1008 whose reason says the token no longer verifies, and a server refusal that a new connection
would repeat (for example an invalid scope or cursor, or a version the client does not speak) stop the
watch at once. A close with code 1008 that says the connection timed out, or that gives no reason the
watch knows, is retried like a dropped connection. A protocol violation or an internal server error is
retried like a dropped connection. A line that cannot be printed, a cursor
that cannot be saved and an event that does not match its cursor stop the watch at once, without a retry.
SIGTERM and SIGINT stop the watch after the event in hand is saved, with status 0. The retry pause is a
plain sleep of at most eight seconds that a stop signal does not interrupt, so a stop during a pause takes
effect when the pause ends. Not done: TLS on the
client, and an end-to-end test against a live gateway (the e2e harness cannot run this binary, see #583).
`lattice watch --help` lists the flags.

`lattice mcp --url http://127.0.0.1:PORT/mcp --token-file FILE` is a stdio proxy to the gateway's
`POST /mcp` endpoint (issue #775). It reads one JSON-RPC message per line from standard input and writes
each reply as one line on standard output; diagnostics go to standard error. The URL must be `http://` on
a loopback address with a port and the path `/mcp` (the CLI has no TLS client yet). The token is read from
an owner-only file, or from the first line of standard input with `-`, and a token on the command line is
refused. A message over 1 MiB is refused with a JSON-RPC error. `lattice mcp --help` lists the flags.

`lattice log repair [--dir DATA_DIR] [--wal NAME] [--apply]` reports damage in the log NAME inside
DATA_DIR (defaults `.` and `wal`). Without `--apply` it changes nothing: it prints the scan and, for a
torn tail, the exact segment, offset and byte count that `--apply` would remove, and why recovery
discards those bytes. With `--apply` it copies the newest segment to `<segment>.repair-backup` with an
exclusive create, reads the copy back and requires it to match byte for byte, truncates only that torn
tail, rescans, and opens the log with the server's recovery. An existing backup, or a link at its path,
is never overwritten. The command holds the log's `LOCK` for the whole run, so a running writer makes
it refuse. It never follows a symbolic link on the `--wal` path or in the log directory, and it never
prints record payloads. Exit status: 0 for a healthy, repairable or repaired log; 1 for damage it
cannot fix or a refused step; 2 for bad arguments. It refuses interior damage, a checksum-valid record
with the wrong LSN (formats/wal-v1.md, issue 441), a missing segment, and a newest segment whose header
is shorter than 16 bytes; for that last case it says to start the server, which rewrites the header.
Not done: repairing the refusals above. `lattice log repair --help` lists the flags.
