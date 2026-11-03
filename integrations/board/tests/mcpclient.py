"""A scripted MCP client for board-mcp: starts the server as a subprocess and talks newline-delimited
JSON-RPC to it. Used by the unit tests (against a fake helper) and by the dev-stack driver (against the
real board)."""

import json
import os
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SERVER = os.environ.get("BOARD_MCP_UNDER_TEST") or os.path.join(os.path.dirname(HERE), "board-mcp")
MODERN = "2026-07-28"
LEGACY = "2025-11-25"
META = {"io.modelcontextprotocol/protocolVersion": MODERN, "io.modelcontextprotocol/clientCapabilities": {}}


class RpcFailure(Exception):
    def __init__(self, error):
        Exception.__init__(self, json.dumps(error))
        self.error = error


class Client:
    def __init__(self, env=None, extra_env=None, legacy=False, start=True):
        self.env = dict(env if env is not None else os.environ)
        self.env.update(extra_env or {})
        self.legacy = legacy
        self.next_id = 1
        self.proc = None
        self.lines = []  # every raw line the server wrote to stdout
        self.err = []  # every line it wrote to stderr
        self.cond = threading.Condition()
        if start:
            self.start()

    def start(self):
        self.proc = subprocess.Popen([sys.executable, SERVER], env=self.env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        threading.Thread(target=self._read_out, daemon=True).start()
        threading.Thread(target=self._read_err, daemon=True).start()
        if self.legacy:
            self.request("initialize", {"protocolVersion": LEGACY, "capabilities": {},
                                        "clientInfo": {"name": "t", "version": "1"}})
            self.notify("notifications/initialized")

    def _read_out(self):
        for raw in self.proc.stdout:
            with self.cond:
                self.lines.append(raw.decode("utf-8", "replace").rstrip("\n"))
                self.cond.notify_all()

    def _read_err(self):
        for raw in self.proc.stderr:
            self.err.append(raw.decode("utf-8", "replace").rstrip("\n"))

    def send_raw(self, data):
        if isinstance(data, str):
            data = data.encode("utf-8")
        self.proc.stdin.write(data)
        self.proc.stdin.flush()

    def send(self, obj):
        self.send_raw(json.dumps(obj) + "\n")

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self.send(msg)

    def send_request(self, method, params=None, rid=None, meta=True):
        rid = self.next_id if rid is None else rid
        if isinstance(rid, int):
            self.next_id = max(self.next_id, rid) + 1
        p = dict(params or {})
        if meta and not self.legacy and method != "initialize":
            p["_meta"] = dict(META)
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": p})
        return rid

    def wait_for(self, rid, timeout=20.0):
        end = time.monotonic() + timeout
        with self.cond:
            while True:
                for line in self.lines:
                    try:
                        m = json.loads(line)
                    except ValueError:
                        continue
                    if isinstance(m, dict) and m.get("id") == rid and ("result" in m or "error" in m):
                        return m
                left = end - time.monotonic()
                if left <= 0:
                    raise TimeoutError("no response to %r; got %r" % (rid, self.lines[-3:]))
                self.cond.wait(left)

    def request(self, method, params=None, **kw):
        rid = self.send_request(method, params, **kw)
        return self.wait_for(rid)

    def call(self, name, arguments=None, **kw):
        """tools/call; returns the result object (raises RpcFailure when the answer is a JSON-RPC error)."""
        m = self.request("tools/call", {"name": name, "arguments": arguments or {}}, **kw)
        if "error" in m:
            raise RpcFailure(m["error"])
        return m["result"]

    def close(self, wait=5.0):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=wait)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        for stream in (self.proc.stdout, self.proc.stderr):
            try:
                stream.close()
            except OSError:
                pass
        return self.proc.returncode
