"""The HTTP server: loopback only, GET and HEAD only, one page and a small JSON API.

Everything the board holds is untrusted text. The server never turns it into markup (it only
emits JSON and three fixed static files), and every response carries the same strict headers.
"""

from __future__ import annotations

import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qsl, urlsplit

from . import model
from .source import ConfigError, SourceError, scrub

BIND_HOST = "127.0.0.1"
STATIC_DIR = Path(__file__).resolve().parent / "static"
STATIC_FILES = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/static/app.js": ("app.js", "text/javascript; charset=utf-8"),
    "/static/style.css": ("style.css", "text/css; charset=utf-8"),
}
MAX_BODY = 4 * 1024 * 1024  # a JSON answer larger than this is an error, not a download
MAX_PATH = 2048
REQUEST_TIMEOUT_S = 10  # a connection that sends nothing for this long is dropped
MAX_CONNECTIONS = 32  # beyond this many open connections a new one is answered 503
FEED_DEFAULT_LIMIT = 100
FEED_MAX_LIMIT = 500
MAX_CURSOR = 2**53

CSP = ("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; "
       "img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")
SECURITY_HEADERS = [
    ("Content-Security-Policy", CSP),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
]

# All three are used with fullmatch: "$" would also accept a trailing newline.
_ID_RE = re.compile(r"[A-Za-z0-9_-]{1,64}")
_NAME_RE = re.compile(r"[A-Za-z0-9._-]{1,64}")
_INT_RE = re.compile(r"[0-9]{1,16}")


class BadRequest(Exception):
    pass


def check_bind(host: str) -> str:
    """Only the IPv4 loopback address is accepted; this is not configurable."""
    if host != BIND_HOST:
        raise ConfigError("the console binds to %s only; refusing %r" % (BIND_HOST, host))
    return host


def parse_query(raw: str, allowed: dict) -> dict:
    """Strictly parse a query string. `allowed` maps a name to a validator(value) -> value.
    Unknown names, repeated names and invalid values are a BadRequest."""
    try:
        pairs = parse_qsl(raw, keep_blank_values=True, strict_parsing=bool(raw), max_num_fields=16)
    except ValueError:
        raise BadRequest("malformed query string") from None
    out: dict = {}
    for k, v in pairs:
        if k not in allowed:
            raise BadRequest("unknown parameter %r" % k[:32])
        if k in out:
            raise BadRequest("parameter %r given twice" % k)
        out[k] = allowed[k](k, v)
    return out


def v_int(lo: int, hi: int):
    def check(name, v):
        if not _INT_RE.fullmatch(v):
            raise BadRequest("%s must be a whole number" % name)
        n = int(v)
        if not lo <= n <= hi:
            raise BadRequest("%s must be between %d and %d" % (name, lo, hi))
        return n
    return check


def v_regex(rx, what):
    def check(name, v):
        if not rx.fullmatch(v):
            raise BadRequest("%s must be %s" % (name, what))
        return v
    return check


def v_kind(name, v):
    if v not in model.EVENT_KINDS:
        raise BadRequest("kind must be one of %s" % ", ".join(model.EVENT_KINDS))
    return v


EPOCH = v_int(0, 4_102_444_800)  # up to the year 2100
FEED_PARAMS = {
    "after": v_int(0, MAX_CURSOR), "before": v_int(1, MAX_CURSOR),
    "limit": v_int(1, FEED_MAX_LIMIT),
    "agent": v_regex(_NAME_RE, "letters, digits, '.', '_' or '-' (up to 64)"),
    "kind": v_kind,
    "task": v_regex(_ID_RE, "letters, digits, '_' or '-' (up to 64)"),
}
TASK_PARAMS = {"id": v_regex(_ID_RE, "letters, digits, '_' or '-' (up to 64)")}
_TOOL_RE = re.compile(r"[A-Za-z0-9_.-]{1,64}")
AGENT_LIMIT_DEFAULT = 200


def v_outcome(name, v):
    if v not in ("ok", "error", "denied"):
        raise BadRequest("outcome must be ok, error or denied")
    return v


AGENT_PARAMS = {
    "name": v_regex(_NAME_RE, "letters, digits, '.', '_' or '-' (up to 64)"),
    "tool": v_regex(_TOOL_RE, "letters, digits, '.', '_' or '-' (up to 64)"),
    "outcome": v_outcome,
    "task": v_regex(_ID_RE, "letters, digits, '_' or '-' (up to 64)"),
    "after": v_int(0, MAX_CURSOR),
    "limit": v_int(1, AGENT_LIMIT_DEFAULT),
}
_EXP_RE = re.compile(r"[A-Za-z0-9_-]{1,48}")


def v_rule(name, v):
    if v not in ("majority", "any"):
        raise BadRequest("rule must be majority or any")
    return v


EXPERIMENT_PARAMS = {"name": v_regex(_EXP_RE, "letters, digits, '_' or '-' (up to 48)"), "rule": v_rule}
SCORE_PARAMS = {"from": EPOCH, "to": EPOCH, "from2": EPOCH, "to2": EPOCH,
                "agent": v_regex(_NAME_RE, "letters, digits, '.', '_' or '-' (up to 64)")}


def make_handler(service, allowed_hosts_ref):
    """Build the request handler class for one service. `allowed_hosts_ref` is a callable that
    returns the accepted Host header values (known only after the socket is bound)."""

    class Handler(BaseHTTPRequestHandler):
        server_version = "lattice-console"
        sys_version = ""
        protocol_version = "HTTP/1.1"
        timeout = REQUEST_TIMEOUT_S

        # -- plumbing

        def log_message(self, fmt, *args):  # quiet: no request lines, so nothing to leak
            return

        def _send(self, status: int, body: bytes, ctype: str, extra=()):
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            for k, v in SECURITY_HEADERS:
                self.send_header(k, v)
            for k, v in extra:
                self.send_header(k, v)
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
            self.close_connection = True

        def _json(self, status: int, obj) -> None:
            body = json.dumps(obj, ensure_ascii=True, separators=(",", ":")).encode("ascii")
            if len(body) > MAX_BODY:
                body = b'{"error":"the answer is too large; narrow the filter"}'
                status = 500
            self._send(status, body, "application/json; charset=utf-8")

        def _error(self, status: int, message: str, extra=()):
            body = json.dumps({"error": message}, ensure_ascii=True).encode("ascii")
            self._send(status, body, "application/json; charset=utf-8", extra)

        def send_error(self, code, message=None, explain=None):
            """Every error the base class would write itself (bad request line, 414, 431, an
            unknown method) goes through the same path, so it has the same headers and no HTML."""
            if self.request_version == "HTTP/0.9":
                self.request_version = "HTTP/1.0"  # 0.9 replies carry no headers at all
            short = {400: "bad request", 414: "request target too long", 431: "request headers too large",
                     501: "method not implemented; use GET or HEAD", 505: "HTTP version not supported",
                     408: "request timed out"}.get(code, "error")
            extra = [("Allow", "GET, HEAD")] if code == 501 else []
            self._error(code, short, extra)

        def _not_allowed(self):
            self._error(405, "method not allowed; use GET or HEAD", [("Allow", "GET, HEAD")])

        do_POST = do_PUT = do_DELETE = do_PATCH = do_OPTIONS = do_TRACE = do_CONNECT = _not_allowed

        # -- routing

        def do_HEAD(self):
            self._route()

        def do_GET(self):
            self._route()

        def _route(self):
            host = (self.headers.get("Host") or "").strip().lower()
            if host not in allowed_hosts_ref():
                self._error(421, "misdirected request: unexpected Host header")
                return
            if len(self.path) > MAX_PATH:
                self._error(414, "request target too long")
                return
            try:
                parts = urlsplit(self.path)
            except ValueError:
                self._error(400, "malformed request target")
                return
            if parts.scheme or parts.netloc or "\\" in self.path:
                self._error(400, "request target must be a path")
                return
            path = parts.path
            try:
                if path in STATIC_FILES:
                    self._static(path)
                elif path == "/api/now":
                    parse_query(parts.query, {})
                    self._json(200, service.now_payload())
                elif path == "/api/queue":
                    parse_query(parts.query, {})
                    self._json(200, service.queue_payload())
                elif path == "/api/task":
                    q = parse_query(parts.query, TASK_PARAMS)
                    if "id" not in q:
                        raise BadRequest("id is required")
                    payload = service.task_payload(q["id"])
                    if payload is None:
                        self._error(404, "no such task in the loaded data")
                    else:
                        self._json(200, payload)
                elif path == "/api/agent":
                    q = parse_query(parts.query, AGENT_PARAMS)
                    if "name" not in q:
                        raise BadRequest("name is required")
                    payload = service.agent_payload(q["name"], q.get("tool"), q.get("outcome"), q.get("task"),
                                                    q.get("limit", AGENT_LIMIT_DEFAULT), q.get("after"))
                    if payload is None:
                        self._error(404, "no such agent in the loaded data")
                    else:
                        self._json(200, payload)
                elif path == "/api/experiments":
                    parse_query(parts.query, {})
                    self._json(200, service.bench_list_payload())
                elif path == "/api/experiment":
                    q = parse_query(parts.query, EXPERIMENT_PARAMS)
                    if "name" not in q:
                        raise BadRequest("name is required")
                    payload = service.bench_payload(q["name"], q.get("rule", "majority"))
                    if payload is None:
                        self._error(404, "no such experiment")
                    else:
                        self._json(200, payload)
                elif path == "/api/feed":
                    q = parse_query(parts.query, FEED_PARAMS)
                    self._json(200, service.feed_payload(
                        q.get("after"), q.get("before"), q.get("limit", FEED_DEFAULT_LIMIT),
                        q.get("agent"), q.get("kind"), q.get("task")))
                elif path == "/api/scorecard":
                    q = parse_query(parts.query, SCORE_PARAMS)
                    win_a = (q.get("from"), q.get("to"))
                    win_b = (q.get("from2"), q.get("to2")) if ("from2" in q or "to2" in q) else None
                    for w in (win_a, win_b):
                        if w and w[0] is not None and w[1] is not None and w[0] >= w[1]:
                            raise BadRequest("a window must start before it ends")
                    self._json(200, service.scorecard_payload(win_a, win_b, q.get("agent")))
                else:
                    self._error(404, "not found")
            except BadRequest as e:
                self._error(400, str(e))
            except SourceError as e:
                self._error(502, scrub(str(e))[:400])
            except Exception:  # never echo internals to the browser
                self._error(500, "internal error")

        def _static(self, path: str):
            name, ctype = STATIC_FILES[path]
            try:
                data = (STATIC_DIR / name).read_bytes()
            except OSError:
                self._error(500, "static file missing")
                return
            self._send(200, data, ctype)

    return Handler


_BUSY_BODY = b'{"error":"too many connections"}'
_BUSY = (b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n"
         + b"Content-Length: %d\r\nRetry-After: 2\r\nConnection: close\r\n" % len(_BUSY_BODY)
         + b"".join(("%s: %s\r\n" % kv).encode("ascii") for kv in SECURITY_HEADERS)
         + b"\r\n" + _BUSY_BODY)


class ConsoleServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, service, port: int, host: str = BIND_HOST,
                 max_connections: int = MAX_CONNECTIONS):
        check_bind(host)
        self._slots = threading.BoundedSemaphore(max_connections)
        self.service = service
        self._hosts: set = set()
        super().__init__((host, port), make_handler(service, lambda: self._hosts))
        self.port = self.server_address[1]
        self._hosts = {"127.0.0.1:%d" % self.port, "localhost:%d" % self.port}

    def process_request(self, request, client_address):
        if not self._slots.acquire(blocking=False):
            try:
                request.settimeout(1)
                request.sendall(_BUSY)
            except OSError:
                pass
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self._slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._slots.release()

    @property
    def url(self) -> str:
        return "http://127.0.0.1:%d/" % self.port
