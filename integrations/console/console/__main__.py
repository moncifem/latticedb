"""Command line: `python3 -m console serve [--port N] [--url URL] [--demo]`."""

from __future__ import annotations

import argparse
import os
import sys

from . import actions as actions_mod
from .demo import DemoSource
from .server import BIND_HOST, ConsoleServer
from .service import BoardService
from .source import ConfigError, PsqlSource, connection_url

DEFAULT_PORT = 8765


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="lattice-console",
        description="Local console for the Lattice agent board (alpha). Read-only unless --allow-writes is given.",
    )
    sub = p.add_subparsers(dest="command", required=True)
    s = sub.add_parser("serve", help="serve the console on 127.0.0.1")
    s.add_argument("--port", type=int, default=DEFAULT_PORT,
                   help="port on 127.0.0.1 (default %d; 0 picks a free one)" % DEFAULT_PORT)
    s.add_argument("--url", default=None,
                   help="postgres URL without a password (default: LATTICE_BOARD_URL or PG* variables)")
    s.add_argument("--demo", action="store_true",
                   help="serve generated DEMO data; no database is contacted")
    s.add_argument("--stale-minutes", type=int, default=30,
                   help="a claimed task with no event for this long is stale (default 30)")
    s.add_argument("--quiet-minutes", type=int, default=10,
                   help="an agent holding a task with a live lease and no tool activity for this long is "
                        "flagged (default 10)")
    s.add_argument("--open-hours", type=int, default=24,
                   help="an open task older than this needs attention (default 24)")
    s.add_argument("--allow-writes", action="store_true",
                   help="let a person approve, reject, cancel, requeue, re-prioritise and note from the page, "
                        "through `board act`; needs the per-run write token this prints once")
    s.add_argument("--actor", default=None,
                   help="the person the decisions are recorded for, as human.NAME (default: the OS user)")
    s.add_argument("--board", default=None,
                   help="the board helper to run (default: integrations/board/board next to this package)")
    return p


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    if not 0 <= args.port <= 65535:
        print("console: --port must be between 0 and 65535", file=sys.stderr)
        return 2
    try:
        if args.demo:
            source = DemoSource()
        else:
            source = PsqlSource(connection_url(args.url))
        service = BoardService(source, args.stale_minutes, args.open_hours, quiet_minutes=args.quiet_minutes)
        acts = None
        token = ""
        if args.allow_writes:
            actor = actions_mod.actor_identity(args.actor)
            token = actions_mod.new_token()
            if args.demo:
                runner = source.act_runner(actor)  # simulated against the in-memory demo board
            else:
                board = args.board or actions_mod.default_board_path()
                if not os.access(board, os.X_OK):
                    raise ConfigError("the board helper %r is not an executable file; give --board" % board)
                runner = actions_mod.HelperRunner(board, actor, connection_url(args.url))
            acts = actions_mod.Actions(service, actor, token, runner)
        elif args.actor or args.board:
            raise ConfigError("--actor and --board only mean something with --allow-writes")
        server = ConsoleServer(service, args.port, BIND_HOST, actions=acts)
    except (ConfigError, ValueError) as e:
        print("console: %s" % e, file=sys.stderr)
        return 2
    except OSError as e:
        print("console: cannot listen on port %d (%s)" % (args.port, e.strerror or "error"),
              file=sys.stderr)
        return 1
    # Never print the connection URL: say only where the data comes from.
    where = "DEMO DATA (no database)" if args.demo else "the board database"
    mode = ("writes ON as %s (simulated: DEMO DATA)" % acts.actor if args.demo else "writes ON as %s" % acts.actor) \
        if acts else "read-only"
    print("console: serving %s from %s, %s" % (server.url, where, mode), flush=True)
    if acts:
        # The one place the token is ever shown. Not in the URL, not in a log, not on any page: paste it into the
        # page when it asks. A new one is made on every start.
        print("console: write token (shown once, paste it into the page): %s" % token, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
