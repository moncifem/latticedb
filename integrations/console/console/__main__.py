"""Command line: `python3 -m console serve [--port N] [--url URL] [--demo]`."""

from __future__ import annotations

import argparse
import sys

from .demo import DemoSource
from .server import BIND_HOST, ConsoleServer
from .service import BoardService
from .source import ConfigError, PsqlSource, connection_url

DEFAULT_PORT = 8765


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="lattice-console",
        description="Read-only local console for the Lattice agent board (alpha).",
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
    s.add_argument("--open-hours", type=int, default=24,
                   help="an open task older than this needs attention (default 24)")
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
        service = BoardService(source, args.stale_minutes, args.open_hours)
        server = ConsoleServer(service, args.port, BIND_HOST)
    except (ConfigError, ValueError) as e:
        print("console: %s" % e, file=sys.stderr)
        return 2
    except OSError as e:
        print("console: cannot listen on port %d (%s)" % (args.port, e.strerror or "error"),
              file=sys.stderr)
        return 1
    # Never print the connection URL: say only where the data comes from.
    where = "DEMO DATA (no database)" if args.demo else "the board database (read-only)"
    print("console: serving %s from %s" % (server.url, where), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
