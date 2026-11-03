"""Shared fixtures of the write-mode tests: a fake `board act`, a board with things waiting, a running server."""

from __future__ import annotations

import http.client
import json
import socket
import threading

import helpers  # noqa: F401  (puts the console package on sys.path)
from helpers import FakeSource, event, lease, task

from console import actions
from console.server import ConsoleServer
from console.service import BoardService

TOKEN = "t" * 8 + "0123456789abcdefghijklmnopqrstuv"
ACTOR = "human.alice"
REQ = "req-0123456789"


def board():
    """One task of each kind the inbox lists, one that it does not, with a body to read."""
    tasks = [
        task("tapprove", "awaiting_approval", created=1000, seq=1, by="maintainer", title="Raise the retry delay",
             body="Proposed: wait 30 s, not 5 s."),
        task("tdead", "dead", created=1100, seq=2, title="Export nightly", body="exports the nightly file", updated=2000),
        task("tclaimed", "claimed", owner="bob", created=1200, seq=3, title="Review the codec"),
        task("topen", "open", created=1300, seq=4, title="Plain open task"),
        task("tmine", "awaiting_approval", created=1400, seq=5, by="alice", title="Made by alice", body="b"),
    ]
    events = [
        event(1, "tapprove", "maintainer", "add", 1000, "Raise the retry delay"),
        event(2, "tdead", "maintainer", "add", 1100, "Export nightly"),
        event(3, "tdead", "bob", "fail", 1900, "disk full"),
        event(4, "tclaimed", "bob", "claim", 1250),
        event(5, "topen", "maintainer", "add", 1300, "Plain open task"),
        event(6, "tmine", "alice", "add", 1400, "Made by alice"),
    ]
    leases = [lease("tdead", attempts=3, last_error="disk full"), lease("tclaimed", until=100000 + 600, hb=99990)]
    return FakeSource(tasks, events, [], now=100000, leases=leases)


class FakeRunner:
    """Stands in for `board act`: records the argument lists, answers with what the test set."""

    def __init__(self, answer=None):
        self.calls = []
        self.answer = answer or (lambda args: (0, "done 901\n"))

    def __call__(self, args):
        self.calls.append(list(args))
        return self.answer(args)


class Write:
    """A real server in write mode on a loopback port."""

    def __init__(self, source=None, runner=None, actor=ACTOR, token=TOKEN, clock=None):
        self.source = source or board()
        self.service = BoardService(self.source, ttl=0.0)
        self.runner = runner or FakeRunner()
        kw = {"clock": clock} if clock else {}
        self.actions = actions.Actions(self.service, actor, token, self.runner, **kw)
        self.server = ConsoleServer(self.service, 0, actions=self.actions)
        self.port = self.server.port
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        self.thread.start()

    def stop(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)

    @property
    def origin(self):
        return "http://127.0.0.1:%d" % self.port

    def good_headers(self, token=TOKEN):
        return {"Origin": self.origin, "Content-Type": "application/json", actions.TOKEN_HEADER: token}

    def request(self, method, path, headers=None, host=None, body=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        conn.putheader("Host", host if host is not None else "127.0.0.1:%d" % self.port)
        for k, v in (headers or {}).items():
            conn.putheader(k, v)
        if body is not None:
            conn.putheader("Content-Length", str(len(body)))
        conn.endheaders(body)
        resp = conn.getresponse()
        data = resp.read()
        conn.close()
        return resp.status, resp, data

    def post(self, path, obj, headers=None, host=None, raw=None):
        body = raw if raw is not None else json.dumps(obj).encode("utf-8")
        h = self.good_headers()
        h.update(headers or {})
        for k in [k for k, v in h.items() if v is None]:
            del h[k]
        status, resp, data = self.request("POST", path, headers=h, host=host, body=body)
        try:
            parsed = json.loads(data.decode("ascii"))
        except ValueError:
            parsed = None
        return status, parsed, resp

    def get_json(self, path, **kw):
        status, resp, data = self.request("GET", path, **kw)
        return status, json.loads(data.decode("ascii")), resp

    def act(self, **fields):
        body = {"action": "cancel", "task": "topen", "request_id": REQ}
        body.update(fields)
        return self.post("/api/act", body)

    def raw(self, payload: bytes, host=None):
        """Send bytes as they are and return everything the server writes back."""
        s = socket.create_connection(("127.0.0.1", self.port), timeout=10)
        s.sendall(payload)
        chunks = []
        while True:
            try:
                c = s.recv(65536)
            except socket.timeout:
                break
            if not c:
                break
            chunks.append(c)
        s.close()
        return b"".join(chunks)
