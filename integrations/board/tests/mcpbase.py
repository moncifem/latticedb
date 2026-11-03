"""Shared pieces of the board-mcp tests: a fixture with a fake helper, state edits, and a JSON Schema
checker written for these tests (it does not share code with the server's own argument validator, so a
mistake in one is not hidden by the same mistake in the other)."""

import json
import os
import re
import shutil
import stat
import sys
import tempfile
import unittest

from mcpclient import Client

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD_DIR = os.path.dirname(HERE)
FAKE = os.path.join(HERE, "fakehelper.py")
SERVER = os.path.join(BOARD_DIR, "board-mcp")
SECRET = "hunter2-very-secret"
FENCE_BASE = 880000  # the fake helper's fences are FENCE_BASE + n


# ---- JSON Schema (the subset the schemas use) ---------------------------------------------------------


def schema_problems(schema, value, path="$"):
    """Every way VALUE fails SCHEMA, as a list of strings (empty when it conforms)."""
    out = []
    if "const" in schema and value != schema["const"]:
        out.append("%s: not the constant %r" % (path, schema["const"]))
    if "enum" in schema and value not in schema["enum"]:
        out.append("%s: %r not in enum" % (path, value))
    t = schema.get("type")
    kinds = t if isinstance(t, list) else ([t] if t else [])
    if kinds:
        def is_kind(k):
            return {"object": isinstance(value, dict), "array": isinstance(value, list), "string": isinstance(value, str),
                    "boolean": isinstance(value, bool), "null": value is None,
                    "integer": isinstance(value, int) and not isinstance(value, bool),
                    "number": isinstance(value, (int, float)) and not isinstance(value, bool)}[k]
        if not any(is_kind(k) for k in kinds):
            return out + ["%s: expected %s, got %s" % (path, kinds, type(value).__name__)]
    if isinstance(value, dict):
        props = schema.get("properties", {})
        for k in schema.get("required", []):
            if k not in value:
                out.append("%s: missing %s" % (path, k))
        if schema.get("additionalProperties") is False:
            for k in value:
                if k not in props:
                    out.append("%s: extra property %s" % (path, k))
        for k, v in value.items():
            if k in props:
                out += schema_problems(props[k], v, "%s.%s" % (path, k))
    if isinstance(value, list):
        if len(value) > schema.get("maxItems", 1 << 30):
            out.append("%s: too many items" % path)
        if len(value) < schema.get("minItems", 0):
            out.append("%s: too few items" % path)
        for i, v in enumerate(value):
            if "items" in schema:
                out += schema_problems(schema["items"], v, "%s[%d]" % (path, i))
    if isinstance(value, str):
        if len(value) > schema.get("maxLength", 1 << 30):
            out.append("%s: longer than %d" % (path, schema["maxLength"]))
        if len(value) < schema.get("minLength", 0):
            out.append("%s: shorter than %d" % (path, schema["minLength"]))
        if "pattern" in schema and not re.search(schema["pattern"], value):
            out.append("%s: does not match the pattern" % path)
    if isinstance(value, int) and not isinstance(value, bool):
        if value < schema.get("minimum", -(1 << 70)):
            out.append("%s: below the minimum" % path)
        if value > schema.get("maximum", 1 << 70):
            out.append("%s: above the maximum" % path)
    if "anyOf" in schema and not any(not schema_problems(s, value, path) for s in schema["anyOf"]):
        out.append("%s: matches none of anyOf" % path)
    return out


def schema_self_check(schema, path="$"):
    """Problems with the SCHEMA itself: every object closed, every string and array bounded, every
    integer bounded where it is an input, required names that exist."""
    out = []
    t = schema.get("type")
    if t == "object":
        if "properties" in schema:
            if schema.get("additionalProperties") is not False:
                out.append("%s: additionalProperties is not false" % path)
            for r in schema.get("required", []):
                if r not in schema["properties"]:
                    out.append("%s: required %s is not a property" % (path, r))
            for k, v in schema["properties"].items():
                out += schema_self_check(v, "%s.%s" % (path, k))
    elif t == "string":
        if "maxLength" not in schema:
            out.append("%s: string without maxLength" % path)
    elif t == "array":
        if "maxItems" not in schema:
            out.append("%s: array without maxItems" % path)
        if "items" in schema:
            out += schema_self_check(schema["items"], path + "[]")
    for key in ("anyOf",):
        for i, s in enumerate(schema.get(key, [])):
            out += schema_self_check(s, "%s.%s[%d]" % (path, key, i))
    return out


# ---- fixture ------------------------------------------------------------------------------------------


def make_helper(tmp):
    """An executable `board` in TMP that runs the fake helper."""
    path = os.path.join(tmp, "board")
    with open(path, "w") as f:
        f.write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, FAKE))
    os.chmod(path, 0o755)
    return path


class Fixture(unittest.TestCase):
    """A temporary directory, a fake board in it, and servers started against it."""

    author = "agent-a"

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="board-mcp-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.fakedir = os.path.join(self.tmp, "fake")
        os.mkdir(self.fakedir)
        self.state_dir = os.path.join(self.tmp, "state")
        os.mkdir(self.state_dir, 0o700)
        self.helper = make_helper(self.tmp)
        self.clients = []

    def env(self, **over):
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.tmp, "BOARD_AUTHOR": self.author,
               "BOARD_MCP_BOARD": self.helper, "BOARD_MCP_PASS_ENV": "FAKEHELPER_DIR", "BOARD_STATE_DIR": self.state_dir, "FAKEHELPER_DIR": self.fakedir,
               "PGPASSWORD": SECRET, "PGHOST": "db.internal.example", "ANTHROPIC_API_KEY": "sk-should-not-reach-helper"}
        env.update(over)
        return {k: v for k, v in env.items() if v is not None}

    def start(self, legacy=False, **over):
        c = Client(env=self.env(**over), legacy=legacy)
        self.clients.append(c)
        self.addCleanup(self.stop, c)
        return c

    def stop(self, c):
        try:
            c.close(wait=10)
        except Exception:
            pass

    def conf(self, **kw):
        with open(os.path.join(self.fakedir, "conf.json"), "w") as f:
            json.dump(kw, f)

    def board_state(self):
        with open(os.path.join(self.fakedir, "state.json")) as f:
            return json.load(f)

    def write_board_state(self, state):
        with open(os.path.join(self.fakedir, "state.json"), "w") as f:
            json.dump(state, f)

    def calls(self):
        try:
            with open(os.path.join(self.fakedir, "calls.jsonl")) as f:
                return [json.loads(x) for x in f if x.strip()]
        except OSError:
            return []

    def add_task(self, title="a task", **kw):
        """A task on the fake board, made through the fake helper as someone else."""
        import subprocess
        by = kw.pop("by", "maker")
        argv = [self.helper, "add"]
        for k, v in kw.items():
            argv += ["--" + k.replace("_", "-"), str(v)]
        argv += ["--", title]
        env = self.env(BOARD_AUTHOR=by)
        p = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=30)
        assert p.returncode == 0, p.stderr
        return p.stdout.strip()

    def reap_and_reclaim(self, task_id, new_owner="agent-b"):
        """What the board does when a lease runs out and someone else takes the task."""
        st = self.board_state()
        t = st["tasks"][task_id]
        st["fence"] += 1
        t.update(state="claimed", owner=new_owner, claim_seq=st["fence"])
        self.write_board_state(st)

    @staticmethod
    def text(result):
        return result["content"][0]["text"]

    @staticmethod
    def mode(path):
        return stat.S_IMODE(os.stat(path).st_mode)
